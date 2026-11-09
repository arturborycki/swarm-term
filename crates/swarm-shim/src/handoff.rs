//! In-place update: the shim replaces its own program with a newer
//! `swarm-shim` (`exec`), keeping the session. The process stays the same, so
//! the shell is still its child (exit codes keep working), and the PTY and the
//! listening socket are inherited, so nothing is closed and nobody notices
//! more than a pause. The rest of the shim's state (scrollback, the hooks it
//! keeps for replays, the screen) goes to the new program in a file that is
//! unlinked as soon as it's written, so only the inherited descriptor reaches it.
//! `swarmd` asks for this when it starts and finds shims from another build.

use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use swarm_proto::{paths, SessionInfo, SessionState, Size};

use crate::buffer::{Ring, RingState};
use crate::session::{BlockHeader, Inner, Session};
use crate::spill::{Spill, SpillHandoff};

/// Version of `HandoffState` this program writes. A program takes over a
/// session only from a shim that writes a version it reads (`accepts`).
pub const HANDOFF_VERSION: u32 = 2;

/// Whether this program can read handoff format `version`: this one, and 1
/// (the same state without the spill).
pub fn accepts(version: u32) -> bool {
    matches!(version, 1 | 2)
}

/// How far back output is fed again to rebuild the scanner's parser state.
const RESCAN_LIMIT: usize = 4 * 1024 * 1024;

/// What the previous program hands over, besides the PTY and the socket.
#[derive(Serialize, Deserialize)]
struct HandoffState {
    version: u32,
    info: SessionInfo,
    ring: RingState,
    sticky: Vec<(String, u64, Vec<u8>)>,
    pending_precmd: Option<(u64, Vec<u8>)>,
    headers: Vec<BlockHeader>,
    screen: ScreenState,
    next_client: u64,
    /// Spill-to-disk, with its key, if it's on (format 2).
    spill: Option<SpillHandoff>,
}

/// Format 1: format 2 without `spill`.
#[derive(Deserialize)]
struct HandoffStateV1 {
    version: u32,
    info: SessionInfo,
    ring: RingState,
    sticky: Vec<(String, u64, Vec<u8>)>,
    pending_precmd: Option<(u64, Vec<u8>)>,
    headers: Vec<BlockHeader>,
    screen: ScreenState,
    next_client: u64,
}

/// The terminal screen as the previous program's emulator had it.
#[derive(Serialize, Deserialize)]
struct ScreenState {
    rows: u16,
    cols: u16,
    alternate: bool,
    /// Escape sequences that redraw it (cursor and modes included).
    contents: Vec<u8>,
}

/// What a shim needs to hand its session over: set once the shim is serving.
pub struct HandoffCtx {
    pub id: String,
    pub runtime_dir: PathBuf,
    pub scrollback: usize,
    pub exit_retention: Duration,
    /// The listening socket the next program keeps serving (replaced if the
    /// shim had to bind its socket again).
    pub listen_fd: std::sync::atomic::AtomicI32,
}

/// Descriptors and facts the new program is started with.
pub struct Adopt {
    pub pid: u32,
    /// The count of in-place updates, this one included.
    pub generation: u32,
    pub pty_fd: RawFd,
    pub listen_fd: RawFd,
    pub state_fd: RawFd,
}

/// Whether `exe` can take a session over from this shim: it must accept the
/// handoff format this shim writes.
pub async fn check_exe(exe: &Path) -> Result<()> {
    let status = tokio::process::Command::new(exe)
        .arg("--accepts-handoff")
        .arg(HANDOFF_VERSION.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .with_context(|| format!("run {}", exe.display()))?;
    if !status.success() {
        bail!("{} can't take over a session from this shim (handoff format {HANDOFF_VERSION})", exe.display());
    }
    Ok(())
}

/// Replaces this process's program with `exe`, handing the session over.
/// Returns only if that failed; the session then carries on here.
pub fn hand_off(session: &Arc<Session>, ctx: &HandoffCtx, exe: &Path) -> anyhow::Error {
    // Stop reading output between reads: unread output stays in the PTY for
    // the next program, and nothing read is lost.
    if !session.reader_ctl.pause(Duration::from_secs(2)) {
        session.reader_ctl.resume();
        return anyhow!("the session's output reader didn't stop");
    }
    let mut inner = session.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Input that's been accepted must reach the PTY first.
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.pending_input.load(Ordering::Acquire) > 0 {
        if Instant::now() > deadline {
            drop(inner);
            session.reader_ctl.resume();
            return anyhow!("input for the session is still being written");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let err = match write_state(ctx, &export(&mut inner)) {
        Ok(state) => exec(session, ctx, exe, &state),
        Err(e) => e,
    };
    drop(inner);
    session.reader_ctl.resume();
    err
}

fn export(inner: &mut Inner) -> HandoffState {
    let (rows, cols) = inner.screen.size();
    let alternate = inner.screen.alternate();
    let contents = inner.screen.formatted();
    HandoffState {
        version: HANDOFF_VERSION,
        info: inner.info.clone(),
        ring: inner.ring.export(),
        sticky: inner.sticky.iter().map(|(n, o, b)| (n.to_string(), *o, b.clone())).collect(),
        pending_precmd: inner.pending_precmd.clone(),
        headers: inner.headers.iter().cloned().collect(),
        screen: ScreenState { rows, cols, alternate, contents },
        next_client: inner.next_client,
        spill: inner.spill.as_ref().map(|s| s.export()),
    }
}

/// Writes `state` to a private file, unlinks it and returns its descriptor.
fn write_state(ctx: &HandoffCtx, state: &HandoffState) -> Result<OwnedFd> {
    let bytes = postcard::to_stdvec(state).context("encode the session's state")?;
    let path = paths::shims_dir(&ctx.runtime_dir).join(format!("{}.handoff", ctx.id));
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    let written = file.write_all(&bytes).and_then(|_| file.flush());
    let _ = std::fs::remove_file(&path);
    written.context("write the session's state")?;
    Ok(OwnedFd::from(file))
}

fn exec(session: &Session, ctx: &HandoffCtx, exe: &Path, state: &OwnedFd) -> anyhow::Error {
    let fds = [session.master.as_raw_fd(), ctx.listen_fd.load(Ordering::Acquire), state.as_raw_fd()];
    for fd in fds {
        set_cloexec(fd, false);
    }
    tracing::info!(id = %ctx.id, exe = %exe.display(), "handing the session over to a new shim program");
    let err = std::process::Command::new(exe)
        .arg("--id")
        .arg(&ctx.id)
        .arg("--runtime-dir")
        .arg(&ctx.runtime_dir)
        .arg("--scrollback")
        .arg(ctx.scrollback.to_string())
        .arg("--retention-secs")
        .arg(ctx.exit_retention.as_secs().to_string())
        .arg("--pid")
        .arg(session.pid().to_string())
        .arg("--generation")
        .arg((session.generation + 1).to_string())
        .arg("--pty-fd")
        .arg(fds[0].to_string())
        .arg("--listen-fd")
        .arg(fds[1].to_string())
        .arg("--adopt-state")
        .arg(fds[2].to_string())
        .exec();
    // Still here: the exec failed and this program keeps the session.
    for fd in fds {
        set_cloexec(fd, true);
    }
    anyhow!("start {}: {err}", exe.display())
}

fn set_cloexec(fd: RawFd, on: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
            libc::fcntl(fd, libc::F_SETFD, flags);
        }
    }
}

/// Takes over the session handed over by the previous program. If its state
/// can't be read, the session still carries on (same PTY and child) with
/// empty scrollback, rather than being lost.
pub fn adopt(id: &str, scrollback: usize, adopt: &Adopt) -> Result<Arc<Session>> {
    let master = unsafe { OwnedFd::from_raw_fd(adopt.pty_fd) };
    set_cloexec(master.as_raw_fd(), true);
    let inner = match read_state(adopt.state_fd) {
        Ok(state) => import(state, scrollback),
        Err(e) => {
            tracing::error!(id, "couldn't read the handed-over state ({e:#}); keeping the session without its scrollback");
            fresh_inner(id, adopt.pid, scrollback, pty_size(master.as_raw_fd()))
        }
    };
    let generation = adopt.generation;
    let session = Session::adopt(master, adopt.pid, inner, generation)?;
    tracing::info!(id, pid = adopt.pid, generation, "took over the session from the previous shim program");
    Ok(session)
}

fn read_state(fd: RawFd) -> Result<HandoffState> {
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.rewind()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let (version, _) = postcard::take_from_bytes::<u32>(&bytes).context("decode the format")?;
    match version {
        1 => {
            let v1: HandoffStateV1 = postcard::from_bytes(&bytes).context("decode")?;
            Ok(HandoffState {
                version: v1.version,
                info: v1.info,
                ring: v1.ring,
                sticky: v1.sticky,
                pending_precmd: v1.pending_precmd,
                headers: v1.headers,
                screen: v1.screen,
                next_client: v1.next_client,
                spill: None,
            })
        }
        HANDOFF_VERSION => postcard::from_bytes(&bytes).context("decode"),
        other => bail!("handoff format {other} (this program reads 1 and {HANDOFF_VERSION})"),
    }
}

fn import(state: HandoffState, scrollback: usize) -> Inner {
    let ring = Ring::import(state.ring, scrollback);
    let mut screen = crate::screen::Screen::new(state.screen.rows, state.screen.cols);
    if state.screen.alternate {
        screen.process(b"\x1b[?1049h");
    }
    screen.process(&state.screen.contents);
    let mut inner = Inner::new(ring, screen, state.info);
    // The scanner's parser may have been inside an escape sequence: feed the
    // output since the last boundary again (its events were already handled).
    let from = inner.ring.last_boundary().max(inner.ring.end().saturating_sub(RESCAN_LIMIT as u64));
    let tail = inner.ring.slice_from(from);
    let _ = inner.scanner.feed(from, &tail);
    inner.sticky = state
        .sticky
        .into_iter()
        .filter_map(|(name, off, bytes)| {
            crate::session::sticky_hook_name(&name).map(|n| (n, off, bytes))
        })
        .collect();
    inner.pending_precmd = state.pending_precmd;
    inner.headers = state.headers.into();
    inner.next_client = state.next_client;
    inner.info.clients.clear();
    inner.spill = state.spill.and_then(|h| match Spill::import(h) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("couldn't carry on spilling to disk: {e:#}");
            None
        }
    });
    inner
}

fn fresh_inner(id: &str, pid: u32, scrollback: usize, size: Size) -> Inner {
    let now = swarm_proto::now_ms();
    let info = SessionInfo {
        id: id.to_string(),
        name: None,
        title: String::new(),
        cwd: None,
        command: Vec::new(),
        pid,
        foreground: None,
        state: SessionState::Running,
        created_ms: now,
        last_output_ms: now,
        last_input_ms: now,
        size,
        clients: Vec::new(),
        ui: Default::default(),
        buffer_start: 0,
        buffer_end: 0,
        alt_screen: false,
    };
    Inner::new(Ring::new(scrollback), crate::screen::Screen::new(size.rows, size.cols), info)
}

fn pty_size(fd: RawFd) -> Size {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Size { cols: ws.ws_col, rows: ws.ws_row }
    } else {
        Size::default()
    }
}
