//! The PTY, its child process and everything attached clients share.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use swarm_proto::{
    now_ms, AttachMode, ClientDesc, CreateOpts, MetaUpdate, SessionInfo, SessionState, ShimMsg, Size,
};
use tokio::sync::{mpsc, watch};

use crate::buffer::Ring;
use crate::scan::Scanner;

/// Output chunks a client may have queued before it is considered lagged.
const CLIENT_QUEUE: usize = 1024;
/// Activity-only info changes are published at most this often.
const ACTIVITY_NOTIFY_INTERVAL: Duration = Duration::from_secs(1);
const NO_LAG: u64 = u64::MAX;

pub struct Session {
    inner: Mutex<Inner>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    info_tx: watch::Sender<SessionInfo>,
    pid: u32,
    reader_done: (Mutex<bool>, Condvar),
    // Kept so the child handle isn't dropped; reaping is done with waitpid.
    _child: Mutex<Box<dyn Child + Send + Sync>>,
}

/// Warp hooks that describe the whole shell session, kept outside the ring so
/// a replay can still start with them after the ring has dropped them.
const STICKY_HOOKS: [&str; 2] = ["InitShell", "Bootstrapped"];

/// Warp hooks whose time is the start or end of a command; a replay marks
/// them with it so the client's blocks keep their real times.
const TIMED_HOOKS: [&str; 3] = ["Precmd", "Preexec", "CommandFinished"];

/// The hooks that open a Warp block (`Precmd`, then `Preexec` with the
/// command), so a replay that starts inside that block's output can reopen it.
struct BlockHeader {
    precmd_at: u64,
    preexec_at: u64,
    /// When the command started (ms since the epoch).
    preexec_ms: u64,
    bytes: Vec<u8>,
}

/// UI metadata key choosing how clients of different sizes share the PTY:
/// `smallest` fits all writable clients; anything else (default) gives the
/// size to whichever client typed last.
pub const META_SIZE_POLICY: &str = "size_policy";

fn smallest_policy(inner: &Inner) -> bool {
    inner.info.ui.get(META_SIZE_POLICY).map(String::as_str) == Some("smallest")
}

/// A replayed block keeps at most this many of its last lines (Warp's
/// default rows per block), plus its first `REPLAY_BLOCK_HEAD` bytes.
const REPLAY_BLOCK_LINES: usize = 50_000;
const REPLAY_BLOCK_HEAD: usize = 64 * 1024;

/// How many recent block headers are kept.
const BLOCK_HEADERS: usize = 64;

struct Inner {
    ring: Ring,
    /// Latest `InitShell` / `Bootstrapped` hook: (offset, complete bytes).
    sticky: Vec<(&'static str, u64, Vec<u8>)>,
    /// Latest `Precmd` hook (offset, bytes), until its `Preexec` arrives.
    pending_precmd: Option<(u64, Vec<u8>)>,
    /// Recent commands' block headers, oldest first.
    headers: std::collections::VecDeque<BlockHeader>,
    /// The terminal screen as the output left it, for full-screen apps.
    screen: vt100::Parser,
    scanner: Scanner,
    info: SessionInfo,
    clients: HashMap<u64, Slot>,
    next_client: u64,
    last_notify: Instant,
}

struct Slot {
    tx: mpsc::Sender<ShimMsg>,
    lag: Arc<AtomicU64>,
    desc: ClientDesc,
    size: Option<Size>,
}

/// What a newly attached client receives.
pub struct Attachment {
    pub client_id: u64,
    pub rx: mpsc::Receiver<ShimMsg>,
    /// Set to the first dropped offset if the client fell behind.
    pub lag: Arc<AtomicU64>,
    pub info: SessionInfo,
    pub replay_from: u64,
    pub live_from: u64,
    /// The replay as (offset, bytes) pieces; long blocks lose their middle.
    pub replay: Vec<(u64, Vec<u8>)>,
    /// Session-describing hooks the replay no longer contains, to send first.
    pub prelude: Vec<(u64, Vec<u8>)>,
    /// For a full-screen app: its current screen, drawn after the replay (the
    /// replay alone may not reproduce it, e.g. if the switch to the alternate
    /// screen was trimmed from the buffer).
    pub screen: Option<Vec<u8>>,
    pub exited: Option<(Option<i32>, Option<i32>)>,
    /// The session is in a full-screen app: nudge its size so it repaints.
    pub wants_repaint: bool,
}

impl Attachment {
    pub fn lagged_at(&self) -> Option<u64> {
        let v = self.lag.load(Ordering::Acquire);
        (v != NO_LAG).then_some(v)
    }
}

impl Session {
    pub fn spawn(id: &str, opts: &CreateOpts, scrollback: usize) -> Result<Arc<Session>> {
        let size = opts.size;
        let pair = native_pty_system()
            .openpty(PtySize { rows: size.rows.max(1), cols: size.cols.max(1), pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let mut cmd = if opts.command.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            let mut c = CommandBuilder::new(&opts.command[0]);
            c.args(&opts.command[1..]);
            c
        };
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        // A folder that doesn't exist here (e.g. sent from another machine)
        // falls back to home instead of failing the session.
        let cwd = opts.cwd.clone().filter(|d| std::path::Path::new(d).is_dir()).unwrap_or(home);
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("SWARM_SESSION_ID", id);
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        let command = if opts.command.is_empty() { vec![cmd.get_shell()] } else { opts.command.clone() };

        let child = pair.slave.spawn_command(cmd).context("spawn command")?;
        // Only the child may hold the slave, so we see EOF when it's gone.
        drop(pair.slave);
        let pid = child.process_id().context("child has no pid")?;

        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let mut writer = pair.master.take_writer().context("pty writer")?;

        let now = now_ms();
        let info = SessionInfo {
            id: id.to_string(),
            name: opts.name.clone(),
            title: String::new(),
            cwd: Some(cwd),
            command,
            pid,
            foreground: None,
            state: SessionState::Running,
            created_ms: now,
            last_output_ms: now,
            last_input_ms: now,
            size,
            clients: Vec::new(),
            ui: opts.ui.clone(),
            buffer_start: 0,
            buffer_end: 0,
            alt_screen: false,
        };
        let (info_tx, _) = watch::channel(info.clone());
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        let session = Arc::new(Session {
            inner: Mutex::new(Inner {
                ring: Ring::new(scrollback),
                sticky: Vec::new(),
                pending_precmd: None,
                headers: Default::default(),
                screen: vt100::Parser::new(info.size.rows, info.size.cols, 0),
                scanner: Scanner::default(),
                info,
                clients: HashMap::new(),
                next_client: 1,
                last_notify: Instant::now(),
            }),
            master: Mutex::new(pair.master),
            input_tx,
            info_tx,
            pid,
            reader_done: (Mutex::new(false), Condvar::new()),
            _child: Mutex::new(child),
        });

        std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
            for data in input_rx {
                if writer.write_all(&data).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })?;

        let s = session.clone();
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => s.on_output(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let (lock, cv) = &s.reader_done;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        })?;

        let s = session.clone();
        std::thread::Builder::new().name("child-wait".into()).spawn(move || {
            let (code, signal) = wait_for(s.pid);
            s.on_exit(code, signal);
        })?;

        Ok(session)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn on_output(&self, data: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        let offset = inner.ring.end();
        inner.ring.push(data);
        inner.screen.process(data);
        let ev = inner.scanner.feed(offset, data);
        for b in ev.boundaries {
            inner.ring.add_boundary(b);
        }
        for hook in ev.hooks {
            if TIMED_HOOKS.contains(&hook.name.as_str()) {
                inner.ring.add_time(hook.start, now_ms());
            }
            if hook.name == "Precmd" {
                // Warp starts a new block here.
                inner.ring.add_boundary(hook.start);
            }
            if hook.name == "Precmd" || hook.name == "Preexec" {
                if let Some(mut bytes) = inner.ring.slice(hook.start, hook.end.min(inner.ring.end())) {
                    if bytes.last() == Some(&0x1b) {
                        bytes.push(b'\\');
                    }
                    if hook.name == "Precmd" {
                        inner.pending_precmd = Some((hook.start, bytes));
                    } else if let Some((precmd_at, mut header)) = inner.pending_precmd.take() {
                        header.extend_from_slice(&bytes);
                        let preexec_ms = now_ms();
                        inner.headers.push_back(BlockHeader { precmd_at, preexec_at: hook.start, preexec_ms, bytes: header });
                        if inner.headers.len() > BLOCK_HEADERS {
                            inner.headers.pop_front();
                        }
                    }
                }
            }
            if let Some(name) = STICKY_HOOKS.iter().find(|n| **n == hook.name) {
                // `end` stops at the terminator's ESC; the `\` may not be here yet.
                if let Some(mut bytes) = inner.ring.slice(hook.start, hook.end.min(inner.ring.end())) {
                    if bytes.last() == Some(&0x1b) {
                        bytes.push(b'\\');
                    }
                    if *name == "InitShell" {
                        // A new shell epoch (e.g. `exec zsh`): its old snapshot is stale.
                        inner.sticky.clear();
                    }
                    inner.sticky.retain(|(n, _, _)| n != name);
                    inner.sticky.push((name, hook.start, bytes));
                }
            }
        }
        let mut significant = false;
        if let Some(t) = ev.title {
            significant |= inner.info.title != t;
            inner.info.title = t;
        }
        if let Some(c) = ev.cwd {
            significant |= inner.info.cwd.as_deref() != Some(c.as_str());
            inner.info.cwd = Some(c);
        }
        if let Some(a) = ev.alt_screen {
            significant |= inner.info.alt_screen != a;
            inner.info.alt_screen = a;
        }
        inner.info.last_output_ms = now_ms();

        let mut dropped = Vec::new();
        for (id, slot) in &inner.clients {
            let msg = ShimMsg::Output { offset, data: data.to_vec(), replay: false };
            if slot.tx.try_send(msg).is_err() {
                // Queue full or client gone: it resumes from this offset.
                slot.lag.store(offset, Ordering::Release);
                dropped.push(*id);
            }
        }
        for id in &dropped {
            inner.clients.remove(id);
        }
        significant |= !dropped.is_empty();
        self.notify(&mut inner, significant);
    }

    fn on_exit(&self, code: Option<i32>, signal: Option<i32>) {
        // Let the reader drain what the child wrote last, so `Exited` follows
        // the final output.
        let (lock, cv) = &self.reader_done;
        let done = lock.lock().unwrap();
        let _ = cv.wait_timeout_while(done, Duration::from_millis(500), |d| !*d);

        let mut inner = self.inner.lock().unwrap();
        inner.info.state = SessionState::Exited { code, signal, at_ms: now_ms() };
        for slot in inner.clients.values() {
            let _ = slot.tx.try_send(ShimMsg::Exited { code, signal });
        }
        self.notify(&mut inner, true);
    }

    pub fn attach(&self, desc: ClientDesc, mode: AttachMode, size: Option<Size>) -> Attachment {
        let mut inner = self.inner.lock().unwrap();
        let replay_from = match mode {
            AttachMode::Full => inner.ring.replay_start(),
            AttachMode::ResumeFrom(o) if inner.ring.contains(o) => o,
            AttachMode::ResumeFrom(_) => inner.ring.replay_start(),
            AttachMode::Live => inner.ring.end(),
        };
        let full = matches!(mode, AttachMode::Full) || (matches!(mode, AttachMode::ResumeFrom(o) if !inner.ring.contains(o)));
        // The ring may have dropped the start of a command whose output it
        // still holds; replaying from the first boundary would skip that
        // output. Start at the first whole line instead, after the hooks that
        // reopen the command's block.
        let mut reopened_block = None;
        let mut replay_from = replay_from;
        if full && replay_from > inner.ring.start() {
            let start = inner.ring.start();
            let cut = inner.headers.iter().rposition(|h| h.preexec_at < start);
            let still_running_at_start =
                cut.is_some_and(|i| inner.headers.get(i + 1).is_none_or(|next| next.precmd_at >= start));
            if let Some(i) = cut.filter(|_| still_running_at_start) {
                let header = &inner.headers[i];
                replay_from = inner.ring.next_line_start(start, replay_from);
                let mut bytes = crate::buffer::output_time_mark(header.preexec_ms);
                bytes.extend_from_slice(&header.bytes);
                reopened_block = Some((header.preexec_at, bytes));
            }
        }
        let live_from = inner.ring.end();
        let replay = inner.ring.replay_pieces(replay_from, REPLAY_BLOCK_HEAD, REPLAY_BLOCK_LINES);
        let replay = inner.ring.with_times(replay);
        let prelude = if full {
            let mut prelude: Vec<(u64, Vec<u8>)> =
                inner.sticky.iter().filter(|(_, off, _)| *off < replay_from).map(|(_, off, b)| (*off, b.clone())).collect();
            prelude.extend(reopened_block);
            prelude
        } else {
            Vec::new()
        };

        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        let lag = Arc::new(AtomicU64::new(NO_LAG));
        let client_id = inner.next_client;
        inner.next_client += 1;
        let readonly = desc.readonly;
        inner.clients.insert(client_id, Slot { tx, lag: lag.clone(), desc, size });
        if smallest_policy(&inner) {
            self.apply_smallest(&mut inner);
        } else if let (Some(size), false) = (size, readonly) {
            self.apply_size(&mut inner, size);
        }
        let exited = match inner.info.state {
            SessionState::Exited { code, signal, .. } => Some((code, signal)),
            SessionState::Running => None,
        };
        let wants_repaint = inner.info.alt_screen && exited.is_none() && !readonly && mode == AttachMode::Full;
        let screen = (full && exited.is_none() && inner.info.alt_screen && inner.screen.screen().alternate_screen()).then(|| {
            let mut bytes = b"\x1b[?1049h".to_vec();
            bytes.extend(inner.screen.screen().state_formatted());
            bytes
        });
        self.notify(&mut inner, true);
        let info = self.snapshot(&inner);
        Attachment { client_id, rx, lag, info, replay_from, live_from, replay, prelude, screen, exited, wants_repaint }
    }

    pub fn detach(&self, client_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.clients.remove(&client_id).is_some() {
            if smallest_policy(&inner) {
                self.apply_smallest(&mut inner);
            }
            self.notify(&mut inner, true);
        }
    }

    pub fn input(&self, client_id: u64, data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.clients.get(&client_id) else { return };
        if slot.desc.readonly || !inner.info.is_running() {
            return;
        }
        // Latest-active size policy: the client that types owns the size.
        if let Some(size) = slot.size.filter(|_| !smallest_policy(&inner)) {
            if self.apply_size(&mut inner, size) {
                self.notify(&mut inner, true);
            }
        }
        inner.info.last_input_ms = now_ms();
        drop(inner);
        let _ = self.input_tx.send(data);
    }

    pub fn client_resize(&self, client_id: u64, size: Size) {
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.clients.get_mut(&client_id) else { return };
        slot.size = Some(size);
        let readonly = slot.desc.readonly;
        let changed = if smallest_policy(&inner) {
            self.apply_smallest(&mut inner)
        } else {
            !readonly && self.apply_size(&mut inner, size)
        };
        if changed {
            self.notify(&mut inner, true);
        }
    }

    /// Makes a full-screen app repaint by briefly changing the size.
    pub async fn nudge_repaint(&self) {
        let size = self.inner.lock().unwrap().info.size;
        if size.rows < 2 {
            return;
        }
        self.raw_resize(Size { cols: size.cols, rows: size.rows - 1 });
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.raw_resize(size);
    }

    /// `smallest` policy: the PTY fits every writable client, so nobody sees
    /// wrapped or clipped output. Returns whether the size changed.
    fn apply_smallest(&self, inner: &mut Inner) -> bool {
        let sizes = inner.clients.values().filter(|c| !c.desc.readonly).filter_map(|c| c.size);
        let smallest = sizes.reduce(|a, b| Size { cols: a.cols.min(b.cols), rows: a.rows.min(b.rows) });
        match smallest {
            Some(size) => self.apply_size(inner, size),
            None => false,
        }
    }

    /// Returns whether the size changed.
    fn apply_size(&self, inner: &mut Inner, size: Size) -> bool {
        if size.cols == 0 || size.rows == 0 || inner.info.size == size {
            return false;
        }
        inner.info.size = size;
        inner.screen.set_size(size.rows, size.cols);
        self.raw_resize(size);
        true
    }

    fn raw_resize(&self, size: Size) {
        let master = self.master.lock().unwrap();
        let _ = master.resize(PtySize { rows: size.rows, cols: size.cols, pixel_width: 0, pixel_height: 0 });
    }

    pub fn kill(&self, signal: i32) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        let rc = unsafe { libc::kill(self.pid as libc::pid_t, signal) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("kill");
        }
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().info.is_running()
    }

    pub fn set_meta(&self, update: MetaUpdate) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(name) = update.name {
            inner.info.name = name.filter(|n| !n.is_empty());
        }
        for (k, v) in update.ui {
            if v.is_empty() {
                inner.info.ui.remove(&k);
            } else {
                inner.info.ui.insert(k, v);
            }
        }
        if smallest_policy(&inner) {
            self.apply_smallest(&mut inner);
        }
        self.notify(&mut inner, true);
    }

    pub fn info(&self) -> SessionInfo {
        let inner = self.inner.lock().unwrap();
        self.snapshot(&inner)
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionInfo> {
        self.info_tx.subscribe()
    }

    /// Republishes info if the foreground process changed (it produces no output event).
    pub fn refresh_foreground(&self) {
        let mut inner = self.inner.lock().unwrap();
        let fg = self.foreground();
        if fg != inner.info.foreground {
            inner.info.foreground = fg;
            self.notify(&mut inner, true);
        }
    }

    fn foreground(&self) -> Option<String> {
        let leader = self.master.lock().unwrap().process_group_leader()?;
        if leader <= 0 || leader as u32 == self.pid {
            return None;
        }
        process_name(leader)
    }

    fn snapshot(&self, inner: &Inner) -> SessionInfo {
        let mut info = inner.info.clone();
        info.buffer_start = inner.ring.start();
        info.buffer_end = inner.ring.end();
        info.clients = inner.clients.values().map(|s| s.desc.clone()).collect();
        info.clients.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        info
    }

    fn notify(&self, inner: &mut Inner, significant: bool) {
        if !significant && inner.last_notify.elapsed() < ACTIVITY_NOTIFY_INTERVAL {
            return;
        }
        inner.last_notify = Instant::now();
        let snap = self.snapshot(inner);
        self.info_tx.send_replace(snap);
    }
}

fn wait_for(pid: u32) -> (Option<i32>, Option<i32>) {
    let mut status: libc::c_int = 0;
    loop {
        let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        if rc == pid as libc::pid_t {
            break;
        }
        if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return (None, None);
    }
    if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    }
}

#[cfg(target_os = "macos")]
fn process_name(pid: libc::pid_t) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn process_name(pid: libc::pid_t) -> Option<String> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(s.trim_end().to_string())
}
