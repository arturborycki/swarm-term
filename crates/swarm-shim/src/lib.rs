//! `swarm-shim`: holds one session's PTY, child process and scrollback, and
//! serves it on `$RUNTIME/shims/<id>.sock`. It outlives `swarmd` and the GUI.

mod buffer;
pub mod handoff;
mod scan;
mod screen;
mod server;
mod session;
pub mod spill;

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use swarm_proto::{paths, CreateOpts, SessionState, SpillConfig};
use tokio::net::UnixListener;
use tokio::sync::Notify;

pub use session::Session;

pub const DEFAULT_SCROLLBACK: usize = 16 * 1024 * 1024;
pub const DEFAULT_EXIT_RETENTION: Duration = Duration::from_secs(10 * 60);

pub struct ShimConfig {
    pub id: String,
    pub runtime_dir: PathBuf,
    pub opts: CreateOpts,
    pub scrollback: usize,
    /// How long an exited session's output is kept if nobody removes it.
    pub exit_retention: Duration,
    /// Keep output that leaves the in-memory buffer on disk, encrypted.
    pub spill: Option<SpillConfig>,
    /// Set when this program takes over a session from the previous shim
    /// program (an in-place update) instead of starting one.
    pub adopt: Option<handoff::Adopt>,
}

/// The descriptors a shim program was handed by its predecessor (from
/// `--pty-fd`, `--listen-fd` and `--adopt-state`), which must stay open.
pub fn handed_over_fds(args: &[String]) -> Vec<i32> {
    args.windows(2)
        .filter(|w| matches!(w[0].as_str(), "--pty-fd" | "--listen-fd" | "--adopt-state"))
        .filter_map(|w| w[1].parse().ok())
        .collect()
}

impl ShimConfig {
    /// Parses `--id ID --runtime-dir DIR (--opts-stdin | --opts HEX) [--scrollback BYTES] [--retention-secs N]`.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut id = None;
        let mut runtime_dir = None;
        let mut opts = None;
        let mut scrollback = DEFAULT_SCROLLBACK;
        let mut exit_retention = DEFAULT_EXIT_RETENTION;
        let (mut pid, mut generation, mut pty_fd, mut listen_fd, mut state_fd) = (None, 1, None, None, None);
        let (mut spill_dir, mut spill_limit) = (None, None);
        let mut it = args.into_iter();
        while let Some(flag) = it.next() {
            let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--id" => id = Some(value()?),
                "--runtime-dir" => runtime_dir = Some(PathBuf::from(value()?)),
                // From `swarmd`: the options come on stdin, out of sight of `ps`.
                "--opts-stdin" => {
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes).context("read the options")?;
                    opts = Some(postcard::from_bytes::<CreateOpts>(&bytes).context("the options are not CreateOpts")?);
                }
                "--opts" => {
                    let bytes = swarm_proto::hex::decode(&value()?).context("--opts is not hex")?;
                    opts = Some(postcard::from_bytes::<CreateOpts>(&bytes).context("--opts is not CreateOpts")?);
                }
                "--scrollback" => scrollback = value()?.parse().context("--scrollback")?,
                "--retention-secs" => {
                    exit_retention = Duration::from_secs(value()?.parse().context("--retention-secs")?)
                }
                "--spill-dir" => spill_dir = Some(value()?),
                "--spill-limit" => spill_limit = Some(value()?.parse().context("--spill-limit")?),
                "--pid" => pid = Some(value()?.parse().context("--pid")?),
                "--generation" => generation = value()?.parse().context("--generation")?,
                "--pty-fd" => pty_fd = Some(value()?.parse().context("--pty-fd")?),
                "--listen-fd" => listen_fd = Some(value()?.parse().context("--listen-fd")?),
                "--adopt-state" => state_fd = Some(value()?.parse().context("--adopt-state")?),
                other => bail!("unknown argument {other}"),
            }
        }
        let id = id.context("--id is required")?;
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("invalid session id {id:?}");
        }
        let adopt = match (pid, pty_fd, listen_fd, state_fd) {
            (Some(pid), Some(pty_fd), Some(listen_fd), Some(state_fd)) => {
                Some(handoff::Adopt { pid, generation, pty_fd, listen_fd, state_fd })
            }
            (None, None, None, None) => None,
            _ => bail!("--pid, --pty-fd, --listen-fd and --adopt-state go together"),
        };
        let spill = match (spill_dir, spill_limit) {
            (Some(dir), Some(limit)) => Some(SpillConfig { dir, limit }),
            (None, None) => None,
            _ => bail!("--spill-dir and --spill-limit go together"),
        };
        Ok(ShimConfig {
            spill,
            adopt,
            id,
            runtime_dir: runtime_dir.context("--runtime-dir is required")?,
            opts: opts.unwrap_or_default(),
            scrollback: scrollback.max(64 * 1024),
            exit_retention,
        })
    }
}

/// How often the shim checks that its socket is still there.
const SOCKET_CHECK_EVERY: Duration = Duration::from_secs(3);

/// Puts the shim's socket back if it disappears: on Linux without lingering,
/// systemd deletes `/run/user/<uid>` when the user's last login ends, while
/// the session keeps running. `swarmd` finds the socket again by looking at
/// the shims folder every few seconds.
async fn keep_socket(session: Arc<Session>, shutdown: Arc<Notify>, handoff: Arc<handoff::HandoffCtx>) {
    let sock = paths::shim_socket(&handoff.runtime_dir, &handoff.id);
    loop {
        tokio::time::sleep(SOCKET_CHECK_EVERY).await;
        if std::fs::symlink_metadata(&sock).is_ok() {
            continue;
        }
        tracing::warn!(id = %handoff.id, "{} was removed; putting it back", sock.display());
        let bound = paths::ensure_private_dir(&handoff.runtime_dir)
            .and_then(|_| paths::ensure_private_dir(&paths::shims_dir(&handoff.runtime_dir)))
            .and_then(|_| UnixListener::bind(&sock))
            .and_then(|l| std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).map(|_| l));
        match bound {
            Ok(listener) => {
                // The old listener's file is gone; an in-place update hands
                // over this one.
                handoff.listen_fd.store(listener.as_raw_fd(), std::sync::atomic::Ordering::Release);
                tokio::spawn(server::serve(session.clone(), listener, shutdown.clone(), handoff.clone()));
            }
            Err(e) => tracing::warn!(id = %handoff.id, "couldn't put the socket back, will try again: {e}"),
        }
    }
}

/// Runs the shim until the session is removed or its exit retention passes.
pub fn run(cfg: ShimConfig) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(async_run(cfg))
}

async fn async_run(cfg: ShimConfig) -> Result<()> {
    let shims = paths::shims_dir(&cfg.runtime_dir);
    paths::ensure_private_dir(&cfg.runtime_dir)?;
    paths::ensure_private_dir(&shims)?;
    let sock = paths::shim_socket(&cfg.runtime_dir, &cfg.id);
    let (session, listener) = match &cfg.adopt {
        // An in-place update: the socket is already bound, and listening.
        Some(adopt) => {
            let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(adopt.listen_fd) };
            std_listener.set_nonblocking(true)?;
            let listener = UnixListener::from_std(std_listener)?;
            (handoff::adopt(&cfg.id, cfg.scrollback, adopt)?, listener)
        }
        None => {
            if sock.exists() {
                if tokio::net::UnixStream::connect(&sock).await.is_ok() {
                    bail!("a shim for {} is already running", cfg.id);
                }
                std::fs::remove_file(&sock)?;
            }
            let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
            let session = match Session::spawn(&cfg.id, &cfg.opts, cfg.scrollback, cfg.spill.as_ref()) {
                Ok(s) => s,
                Err(e) => {
                    let _ = std::fs::remove_file(&sock);
                    return Err(e);
                }
            };
            tracing::info!(id = %cfg.id, pid = session.pid(), "session started");
            (session, listener)
        }
    };
    let handoff = Arc::new(handoff::HandoffCtx {
        id: cfg.id.clone(),
        runtime_dir: cfg.runtime_dir.clone(),
        scrollback: cfg.scrollback,
        exit_retention: cfg.exit_retention,
        listen_fd: std::sync::atomic::AtomicI32::new(listener.as_raw_fd()),
    });

    let shutdown = Arc::new(Notify::new());
    tokio::spawn(server::serve(session.clone(), listener, shutdown.clone(), handoff.clone()));
    let keeper = tokio::spawn(keep_socket(session.clone(), shutdown.clone(), handoff));

    let fg = session.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(session::ACTIVITY_NOTIFY_INTERVAL);
        loop {
            tick.tick().await;
            fg.refresh();
        }
    });

    let mut rx = session.subscribe();
    let exited = async {
        let _ = rx.wait_for(|i| !matches!(i.state, SessionState::Running)).await;
        tracing::info!(id = %cfg.id, "session exited; keeping output for {:?}", cfg.exit_retention);
        tokio::time::sleep(cfg.exit_retention).await;
    };
    tokio::select! {
        _ = shutdown.notified() => tracing::info!(id = %cfg.id, "session removed"),
        _ = exited => tracing::info!(id = %cfg.id, "exit retention passed"),
    }
    keeper.abort();
    let _ = std::fs::remove_file(&sock);
    // The session is gone: so is what it spilled.
    let _ = session.set_spill(None);
    // Give in-flight replies a moment to reach their sockets.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}
