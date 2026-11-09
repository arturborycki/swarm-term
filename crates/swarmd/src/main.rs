//! `swarmd`: the per-user session daemon.

mod memory;
mod registry;
mod restore;
mod server;
mod service;
mod spill;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use swarm_proto::{paths, read_frame, write_frame, ClientMsg, Request, ServerMsg, PROTOCOL_VERSION};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

#[derive(Parser)]
#[command(name = "swarmd", version = swarm_proto::VERSION, about = "swarm-term session daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground.
    Run {
        /// Exit quietly if another daemon already runs (used by login services).
        #[arg(long)]
        if_not_running: bool,
    },
    /// Start the daemon in the background if it isn't running.
    Start,
    /// Stop the daemon. Sessions keep running and are re-adopted on next start.
    Stop,
    /// Exit 0 if the daemon is running.
    Status,
    /// Start the daemon if needed and connect stdin/stdout to it (used over ssh).
    Bridge,
    /// Start the daemon at login (`at-login` mode) or only when needed (`with-app`).
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Recreate sessions (same name, folder, tab order) after a reboot.
    Restore {
        #[command(subcommand)]
        cmd: RestoreCmd,
    },
    /// Keep output beyond each session's 16 MB in-memory buffer on disk,
    /// encrypted with a key only the session holds (`swarmctl dump` shows it).
    Spill {
        #[command(subcommand)]
        cmd: SpillCmd,
    },
}

#[derive(Subcommand)]
enum SpillCmd {
    /// Turn it on, for running sessions too.
    On {
        /// Most kept on disk per session, in MB (default 256).
        #[arg(long)]
        limit_mb: Option<u64>,
    },
    /// Turn it off and delete what was spilled.
    Off,
    /// Show whether it's on.
    Status,
}

#[derive(Subcommand)]
enum RestoreCmd {
    /// Recreate sessions whose processes are gone when the daemon starts.
    On,
    /// Don't.
    Off,
    /// Show whether it's on.
    Status,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start the daemon at login: launchd agent (macOS) or systemd user unit (Linux).
    Install {
        /// On Linux, don't turn on lingering (sessions may end at logout).
        #[arg(long)]
        no_linger: bool,
    },
    /// Go back to starting the daemon only when needed.
    Uninstall,
    /// Show the current start mode.
    Status,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run { if_not_running } => run(if_not_running),
        Cmd::Start => block_on(async { start().await.map(|_| ()) }),
        Cmd::Stop => block_on(stop()),
        Cmd::Status => block_on(status()),
        Cmd::Bridge => block_on(bridge()),
        Cmd::Service { cmd } => match cmd {
            ServiceCmd::Install { no_linger } => service::install(!no_linger),
            ServiceCmd::Uninstall => service::uninstall(),
            ServiceCmd::Status => service::status(),
        },
        Cmd::Spill { cmd } => match cmd {
            SpillCmd::On { limit_mb } => {
                let limit = limit_mb.map(|mb| mb.max(1) * 1024 * 1024).unwrap_or(spill::DEFAULT_LIMIT);
                spill::set(Some(limit)).and_then(|_| block_on(apply_spill()))
            }
            SpillCmd::Off => spill::set(None).and_then(|_| block_on(apply_spill())),
            SpillCmd::Status => {
                println!("{}", spill::status_line());
                Ok(())
            }
        },
        Cmd::Restore { cmd } => match cmd {
            RestoreCmd::On => restore::set_enabled(true),
            RestoreCmd::Off => restore::set_enabled(false),
            RestoreCmd::Status => {
                println!("restore after reboot: {}", if restore::is_enabled() { "on" } else { "off" });
                Ok(())
            }
        },
    };
    if let Err(e) = result {
        eprintln!("swarmd: {e:#}");
        std::process::exit(1);
    }
}

fn block_on<F: std::future::Future<Output = Result<()>>>(f: F) -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(f)
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("SWARM_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

fn shim_exe() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("SWARM_SHIM_PATH") {
        return Ok(PathBuf::from(p));
    }
    // Through a link (Homebrew's bin, a PATH symlink) the shim is next to the real file.
    let exe = std::env::current_exe()?.canonicalize()?;
    let sibling = exe.with_file_name("swarm-shim");
    if sibling.exists() {
        return Ok(sibling);
    }
    bail!("swarm-shim not found next to {}", exe.display())
}

fn run(if_not_running: bool) -> Result<()> {
    paths::close_inherited_fds();
    init_logging();
    let runtime = paths::runtime_dir();
    paths::ensure_private_dir(&runtime)?;
    let _lock = match lock(&paths::daemon_lock(&runtime)) {
        Ok(l) => l,
        Err(_) if if_not_running => {
            tracing::info!("another swarmd is already running");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let shim = shim_exe()?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let sock = paths::daemon_socket(&runtime);
        let _ = std::fs::remove_file(&sock);
        let listener = bind(&sock)?;
        let bound = inode(&sock);

        let registry = registry::Registry::new(runtime.clone(), shim);
        registry.adopt_existing().await?;
        tokio::spawn(registry.clone().watch_shims_dir());
        // Sessions started by an older build move to this build's shim,
        // one at a time, without restarting.
        let upgrading = registry.clone();
        tokio::spawn(async move {
            upgrading.upgrade_shims(false).await;
            // The spill setting may have changed while the daemon was down.
            upgrading.apply_spill().await;
            let live: Vec<String> = upgrading.list().into_iter().map(|s| s.id).collect();
            spill::remove_orphans(&live);
        });
        restore::start(&registry).await;
        tokio::spawn(memory::run(registry.clone()));
        let shutdown = Arc::new(Notify::new());
        let server = Arc::new(server::Server { registry, shutdown: shutdown.clone() });
        tracing::info!(socket = %sock.display(), "swarmd {} listening", swarm_proto::VERSION);
        let keeper = tokio::spawn(keep_socket(server.clone(), runtime.clone(), bound));

        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let replaced = tokio::select! {
            _ = server.clone().serve(listener) => false,
            _ = shutdown.notified() => { tracing::info!("shutdown requested"); false }
            _ = term.recv() => { tracing::info!("SIGTERM"); false }
            _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT"); false }
            r = keeper => r.unwrap_or(false),
        };
        // Only remove the socket if it is still ours: after systemd removed
        // the runtime folder, the path may belong to a newer daemon.
        if !replaced {
            let current = inode(&sock);
            if current.is_some() && current == our_inode(bound) {
                let _ = std::fs::remove_file(&sock);
            }
        }
        Ok(())
    })
}

/// How often the daemon checks that its socket is still there.
const SOCKET_CHECK_EVERY: Duration = Duration::from_secs(3);

fn bind(sock: &Path) -> Result<UnixListener> {
    let listener = UnixListener::bind(sock).with_context(|| format!("bind {}", sock.display()))?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The inode at `path`, if something is there.
fn inode(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| m.ino())
}

/// The inode of the socket this daemon bound last: `first`, unless the
/// keeper has bound it again since.
fn our_inode(first: Option<u64>) -> Option<u64> {
    BOUND_INODE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).or(first)
}

/// Set by `keep_socket` when it binds the socket again.
static BOUND_INODE: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

/// Puts the daemon's socket back if it disappears. On Linux without
/// lingering, systemd deletes `/run/user/<uid>` (socket and lock included)
/// when the user's last login ends, while the daemon and sessions keep
/// running. Without this, the next connection would start a second daemon
/// that can't see them. If another daemon got there first (the path is
/// taken, or its lock is held), this one exits, and the newer daemon adopts
/// the sessions once their shims have put their own sockets back.
///
/// Returns `true` when this daemon should exit because it was replaced.
async fn keep_socket(server: Arc<server::Server>, runtime: PathBuf, mut bound: Option<u64>) -> bool {
    let sock = paths::daemon_socket(&runtime);
    // Lock files taken again after the old ones were deleted.
    let mut locks = Vec::new();
    loop {
        tokio::time::sleep(SOCKET_CHECK_EVERY).await;
        match inode(&sock) {
            now if now == bound => continue,
            Some(_) => {
                tracing::warn!("{} now belongs to another swarmd; leaving the sessions to it", sock.display());
                return true;
            }
            None => {}
        }
        tracing::warn!("{} was removed (logged out without lingering?); putting it back", sock.display());
        let relisten = async {
            paths::ensure_private_dir(&runtime)?;
            paths::ensure_private_dir(&paths::shims_dir(&runtime))?;
            let lock_file = lock(&paths::daemon_lock(&runtime))?;
            if inode(&sock).is_some() {
                bail!("another swarmd took its place");
            }
            let listener = bind(&sock)?;
            Ok::<_, anyhow::Error>((lock_file, listener))
        };
        match relisten.await {
            Ok((lock_file, listener)) => {
                locks.push(lock_file);
                bound = inode(&sock);
                *BOUND_INODE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = bound;
                tokio::spawn(server.clone().serve(listener));
                tracing::info!(socket = %sock.display(), "listening again");
            }
            Err(e) => {
                tracing::warn!("couldn't put the socket back ({e:#}); leaving the sessions to the other swarmd");
                return true;
            }
        }
    }
}

/// Holds an exclusive `flock` for the life of the process.
fn lock(path: &Path) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("another swarmd is already running");
    }
    Ok(f)
}

async fn connect() -> Result<UnixStream> {
    let sock = paths::daemon_socket(&paths::runtime_dir());
    let mut s = UnixStream::connect(&sock).await?;
    let hello = ClientMsg::Hello {
        proto: PROTOCOL_VERSION,
        client_name: "swarmd".into(),
        client_id: format!("swarmd-{}", std::process::id()),
    };
    write_frame(&mut s, &hello).await?;
    match read_frame::<_, ServerMsg>(&mut s).await? {
        Some(ServerMsg::Welcome { proto, .. }) if proto == PROTOCOL_VERSION => Ok(s),
        Some(ServerMsg::Welcome { proto, daemon_version, .. }) => {
            bail!("running daemon {daemon_version} speaks protocol {proto}, expected {PROTOCOL_VERSION}")
        }
        _ => bail!("unexpected greeting"),
    }
}

/// Starts the daemon in the background; returns whether it had to be started.
async fn start() -> Result<bool> {
    if connect().await.is_ok() {
        return Ok(false);
    }
    let log_dir = paths::log_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("swarmd.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("run").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(log));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    for _ in 0..500 {
        if connect().await.is_ok() {
            // The daemon is reparented when we exit; reap it if we live longer.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(true);
        }
        if let Some(status) = child.try_wait()? {
            bail!("swarmd exited during startup ({status}); see {}", log_dir.join("swarmd.log").display());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    bail!("swarmd did not start within 5s")
}

/// Tells a running daemon to apply the spill setting to its sessions.
async fn apply_spill() -> Result<()> {
    let Ok(mut s) = connect().await else { return Ok(()) };
    write_frame(&mut s, &ClientMsg::Request { id: 1, req: Request::ApplySpill }).await?;
    match read_frame::<_, ServerMsg>(&mut s).await? {
        Some(ServerMsg::Response { result: Err(e), .. }) => bail!(e),
        _ => Ok(()),
    }
}

async fn stop() -> Result<()> {
    let Ok(mut s) = connect().await else {
        println!("swarmd is not running");
        return Ok(());
    };
    write_frame(&mut s, &ClientMsg::Request { id: 1, req: Request::Shutdown }).await?;
    let _ = read_frame::<_, ServerMsg>(&mut s).await;
    println!("swarmd stopped; sessions keep running");
    Ok(())
}

async fn status() -> Result<()> {
    if connect().await.is_ok() {
        println!("swarmd is running ({})", paths::daemon_socket(&paths::runtime_dir()).display());
        Ok(())
    } else {
        bail!("swarmd is not running")
    }
}

async fn bridge() -> Result<()> {
    start().await?;
    let sock = paths::daemon_socket(&paths::runtime_dir());
    let stream = UnixStream::connect(&sock).await?;
    let (mut rd, mut wr) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut wr) => { r?; }
        r = tokio::io::copy(&mut rd, &mut stdout) => { r?; }
    }
    Ok(())
}
