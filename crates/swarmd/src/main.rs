//! `swarmd`: the per-user session daemon.

mod memory;
mod registry;
mod restore;
mod server;
mod service;

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
    let exe = std::env::current_exe()?;
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
        let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

        let registry = registry::Registry::new(runtime.clone(), shim);
        registry.adopt_existing().await?;
        // Sessions started by an older build move to this build's shim,
        // one at a time, without restarting.
        let upgrading = registry.clone();
        tokio::spawn(async move {
            upgrading.upgrade_shims(false).await;
        });
        restore::start(&registry).await;
        tokio::spawn(memory::run(registry.clone()));
        let shutdown = Arc::new(Notify::new());
        let server = Arc::new(server::Server { registry, shutdown: shutdown.clone() });
        tracing::info!(socket = %sock.display(), "swarmd {} listening", swarm_proto::VERSION);

        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = server.serve(listener) => {}
            _ = shutdown.notified() => tracing::info!("shutdown requested"),
            _ = term.recv() => tracing::info!("SIGTERM"),
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
        }
        let _ = std::fs::remove_file(&sock);
        Ok(())
    })
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
