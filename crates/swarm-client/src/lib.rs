//! Async client for `swarmd`, used by `swarmctl` and the GUI.
//!
//! One [`Client`] is one connection (a local unix socket, or `ssh host swarmd
//! bridge`). Requests, event subscriptions and any number of attachments are
//! multiplexed over it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use swarm_proto::{
    paths, read_frame, write_frame, AttachMode, ClientMsg, CreateOpts, MetaUpdate, Request, Response, ServerMsg,
    SessionEvent, SessionInfo, Size, StreamId, PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

pub use swarm_proto;

/// What an attachment receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Output { offset: u64, data: Vec<u8>, replay: bool },
    Exited { code: Option<i32>, signal: Option<i32> },
    /// Fell behind; re-attach with `AttachMode::ResumeFrom(resume_at)`.
    Lagged { resume_at: u64 },
    Closed { reason: String },
}

#[derive(Debug, Clone)]
pub struct DaemonInfo {
    pub version: String,
    pub hostname: String,
    pub os: String,
}

type Pending = HashMap<u64, (oneshot::Sender<Result<Response, String>>, Option<mpsc::UnboundedSender<StreamEvent>>)>;

struct Shared {
    out: mpsc::Sender<ClientMsg>,
    pending: Mutex<Pending>,
    streams: Mutex<HashMap<StreamId, mpsc::UnboundedSender<StreamEvent>>>,
    events: broadcast::Sender<SessionEvent>,
    next_id: AtomicU64,
    subscribed: tokio::sync::Mutex<bool>,
    /// Set by [`Client::close`]: the connection is being dropped.
    closed: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    daemon: DaemonInfo,
}

impl Client {
    /// Connects to the local daemon, starting it if it isn't running.
    pub async fn connect_local(client_name: &str) -> Result<Client> {
        let sock = paths::daemon_socket(&paths::runtime_dir());
        if let Ok(s) = UnixStream::connect(&sock).await {
            let (rd, wr) = s.into_split();
            if let Ok(c) = Client::handshake(rd, wr, client_name).await {
                return Ok(c);
            }
        }
        start_local_daemon().await?;
        let s = UnixStream::connect(&sock).await.with_context(|| format!("connect {}", sock.display()))?;
        let (rd, wr) = s.into_split();
        Client::handshake(rd, wr, client_name).await
    }

    /// Connects to `host`'s daemon through `ssh host swarmd bridge`. Uses the
    /// system ssh, so keys, agent and `~/.ssh/config` apply.
    pub async fn connect_ssh(host: &str, client_name: &str) -> Result<Client> {
        let remote = remote_swarmd_command("bridge");
        let mut child = tokio::process::Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=3",
                host,
                &remote,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("run ssh")?;
        let wr = child.stdin.take().context("ssh stdin")?;
        let rd = child.stdout.take().context("ssh stdout")?;
        let client = Client::handshake(rd, wr, client_name).await?;
        // Keep ssh alive as long as the client; it ends when the pipes close,
        // or at once on `close` (a dead link can keep ssh waiting a while).
        let closing = client.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = closing.closed() => {
                    let _ = child.kill().await;
                }
            }
        });
        Ok(client)
    }

    pub async fn handshake<R, W>(mut rd: R, mut wr: W, client_name: &str) -> Result<Client>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let hello = ClientMsg::Hello {
            proto: PROTOCOL_VERSION,
            client_name: client_name.to_string(),
            client_id: ulid::Ulid::new().to_string(),
        };
        write_frame(&mut wr, &hello).await?;
        let daemon = match tokio::time::timeout(Duration::from_secs(10), read_frame::<_, ServerMsg>(&mut rd)).await {
            Ok(Ok(Some(ServerMsg::Welcome { proto, daemon_version, hostname, os }))) => {
                if proto != PROTOCOL_VERSION {
                    bail!("daemon {daemon_version} speaks protocol {proto}, this client speaks {PROTOCOL_VERSION}");
                }
                DaemonInfo { version: daemon_version, hostname, os }
            }
            Ok(Ok(other)) => bail!("unexpected greeting {other:?}"),
            Ok(Err(e)) => return Err(e).context("read greeting"),
            Err(_) => bail!("timed out waiting for the daemon"),
        };

        let (out, mut out_rx) = mpsc::channel::<ClientMsg>(1024);
        let (events, _) = broadcast::channel(1024);
        let shared = Arc::new(Shared {
            out,
            pending: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            events,
            next_id: AtomicU64::new(1),
            subscribed: tokio::sync::Mutex::new(false),
            closed: watch::channel(false).0,
        });

        let mut closed = shared.closed.subscribe();
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    m = out_rx.recv() => m,
                    _ = closed.wait_for(|c| *c) => None,
                };
                let Some(msg) = msg else { break };
                if write_frame(&mut wr, &msg).await.is_err() {
                    break;
                }
            }
        });
        let reader_shared = shared.clone();
        let mut closed = shared.closed.subscribe();
        tokio::spawn(async move {
            let s = reader_shared;
            loop {
                let msg = tokio::select! {
                    m = read_frame::<_, ServerMsg>(&mut rd) => m,
                    _ = closed.wait_for(|c| *c) => break,
                };
                let msg = match msg {
                    Ok(Some(m)) => m,
                    _ => break,
                };
                match msg {
                    ServerMsg::Response { id, result } => {
                        let Some((reply, stream_tx)) = s.pending.lock().unwrap().remove(&id) else { continue };
                        // Register the stream before anyone can see the response,
                        // so no output for it is ever dropped.
                        if let (Ok(Response::Attached { stream, .. }), Some(tx)) = (&result, stream_tx) {
                            s.streams.lock().unwrap().insert(*stream, tx);
                        }
                        let _ = reply.send(result);
                    }
                    ServerMsg::Output { stream, offset, data, replay } => {
                        s.send_stream(stream, StreamEvent::Output { offset, data, replay }, false);
                    }
                    ServerMsg::Exited { stream, code, signal } => {
                        s.send_stream(stream, StreamEvent::Exited { code, signal }, false);
                    }
                    ServerMsg::Lagged { stream, resume_at } => {
                        s.send_stream(stream, StreamEvent::Lagged { resume_at }, true);
                    }
                    ServerMsg::StreamClosed { stream, reason } => {
                        s.send_stream(stream, StreamEvent::Closed { reason }, true);
                    }
                    ServerMsg::Event(ev) => {
                        let _ = s.events.send(ev);
                    }
                    ServerMsg::Welcome { .. } => {}
                }
            }
            // Connection lost: fail everything still waiting.
            s.pending.lock().unwrap().clear();
            for (_, tx) in s.streams.lock().unwrap().drain() {
                let _ = tx.send(StreamEvent::Closed { reason: "connection to daemon lost".into() });
            }
        });

        Ok(Client { shared, daemon })
    }

    pub fn daemon(&self) -> &DaemonInfo {
        &self.daemon
    }

    /// Drops the connection now: pending requests fail and every attachment
    /// gets [`StreamEvent::Closed`], as when the link dies. For a link that is
    /// dead but not yet noticed (after sleep, or a network change).
    pub fn close(&self) {
        self.shared.closed.send_replace(true);
    }

    /// Resolves once [`Client::close`] was called on this connection.
    pub async fn closed(&self) {
        let mut closed = self.shared.closed.subscribe();
        let _ = closed.wait_for(|c| *c).await;
    }

    async fn request_with(
        &self,
        req: Request,
        stream_tx: Option<mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<Response> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, (tx, stream_tx));
        self.shared.out.send(ClientMsg::Request { id, req }).await.map_err(|_| anyhow!("connection closed"))?;
        match rx.await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => bail!("connection to daemon lost"),
        }
    }

    async fn request(&self, req: Request) -> Result<Response> {
        self.request_with(req, None).await
    }

    pub async fn ping(&self) -> Result<()> {
        match self.request(Request::Ping).await? {
            Response::Pong => Ok(()),
            other => bail!("unexpected {other:?}"),
        }
    }

    pub async fn list(&self) -> Result<Vec<SessionInfo>> {
        match self.request(Request::List).await? {
            Response::Sessions(v) => Ok(v),
            other => bail!("unexpected {other:?}"),
        }
    }

    pub async fn create(&self, opts: CreateOpts) -> Result<SessionInfo> {
        match self.request(Request::Create(opts)).await? {
            Response::Session(s) => Ok(s),
            other => bail!("unexpected {other:?}"),
        }
    }

    pub async fn kill(&self, id: &str, signal: i32) -> Result<()> {
        self.request(Request::Kill { id: id.into(), signal }).await.map(|_| ())
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        self.request(Request::Remove { id: id.into() }).await.map(|_| ())
    }

    pub async fn set_meta(&self, id: &str, update: MetaUpdate) -> Result<SessionInfo> {
        match self.request(Request::SetMeta { id: id.into(), update }).await? {
            Response::Session(s) => Ok(s),
            other => bail!("unexpected {other:?}"),
        }
    }

    pub async fn shutdown_daemon(&self) -> Result<()> {
        self.request(Request::Shutdown).await.map(|_| ())
    }

    /// Moves running sessions to the daemon's `swarm-shim` without
    /// restarting them (`force`: even those already on its build).
    pub async fn upgrade_shims(&self, force: bool) -> Result<Vec<swarm_proto::ShimUpgrade>> {
        match self.request(Request::UpgradeShims { force }).await? {
            Response::ShimsUpgraded(results) => Ok(results),
            other => bail!("unexpected {other:?}"),
        }
    }

    /// Session events from now on. The first call subscribes the connection.
    pub async fn subscribe(&self) -> Result<broadcast::Receiver<SessionEvent>> {
        let rx = self.shared.events.subscribe();
        let mut subscribed = self.shared.subscribed.lock().await;
        if !*subscribed {
            self.request(Request::Subscribe).await?;
            *subscribed = true;
        }
        Ok(rx)
    }

    pub async fn attach(&self, id: &str, mode: AttachMode, size: Option<Size>, readonly: bool) -> Result<Attachment> {
        let (tx, rx) = mpsc::unbounded_channel();
        let req = Request::Attach { id: id.into(), mode, size, readonly };
        match self.request_with(req, Some(tx)).await? {
            Response::Attached { stream, info, replay_from, live_from } => {
                Ok(Attachment { client: self.clone(), stream, info, replay_from, live_from, events: rx, detached: false })
            }
            other => bail!("unexpected {other:?}"),
        }
    }
}

impl Shared {
    fn send_stream(&self, stream: StreamId, ev: StreamEvent, last: bool) {
        let mut streams = self.streams.lock().unwrap();
        if let Some(tx) = streams.get(&stream) {
            let _ = tx.send(ev);
        }
        if last {
            streams.remove(&stream);
        }
    }
}

pub struct Attachment {
    client: Client,
    pub stream: StreamId,
    pub info: SessionInfo,
    pub replay_from: u64,
    pub live_from: u64,
    events: mpsc::UnboundedReceiver<StreamEvent>,
    detached: bool,
}

impl Attachment {
    pub async fn recv(&mut self) -> Option<StreamEvent> {
        self.events.recv().await
    }

    pub async fn input(&self, data: impl Into<Vec<u8>>) -> Result<()> {
        self.send(ClientMsg::Input { stream: self.stream, data: data.into() }).await
    }

    pub async fn resize(&self, size: Size) -> Result<()> {
        self.send(ClientMsg::Resize { stream: self.stream, size }).await
    }

    pub async fn detach(mut self) -> Result<()> {
        self.detached = true;
        self.client.shared.streams.lock().unwrap().remove(&self.stream);
        self.send(ClientMsg::Detach { stream: self.stream }).await
    }

    async fn send(&self, msg: ClientMsg) -> Result<()> {
        self.client.shared.out.send(msg).await.map_err(|_| anyhow!("connection closed"))
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if !self.detached {
            self.client.shared.streams.lock().unwrap().remove(&self.stream);
            let _ = self.client.shared.out.try_send(ClientMsg::Detach { stream: self.stream });
        }
    }
}

/// Finds a session by exact id, unique id prefix, or name.
pub fn resolve<'a>(sessions: &'a [SessionInfo], query: &str) -> Result<&'a SessionInfo> {
    if let Some(s) = sessions.iter().find(|s| s.id == query) {
        return Ok(s);
    }
    let by_name: Vec<_> = sessions.iter().filter(|s| s.name.as_deref() == Some(query)).collect();
    if by_name.len() == 1 {
        return Ok(by_name[0]);
    }
    let upper = query.to_ascii_uppercase();
    let by_prefix: Vec<_> = sessions.iter().filter(|s| !query.is_empty() && s.id.starts_with(&upper)).collect();
    match (by_name.len(), by_prefix.len()) {
        (0, 1) => Ok(by_prefix[0]),
        (0, 0) => bail!("no session matches {query:?}"),
        _ => bail!("{query:?} matches more than one session"),
    }
}

/// A remote shell command running `swarmd <args>` from the per-user install
/// location, falling back to `PATH`.
pub fn remote_swarmd_command(args: &str) -> String {
    format!(r#"p="$HOME/.local/share/swarm/bin/swarmd"; [ -x "$p" ] || p=swarmd; exec "$p" {args}"#)
}

/// The local `swarmd` binary: `SWARM_DAEMON_PATH`, next to this executable, or on `PATH`.
pub fn daemon_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("SWARM_DAEMON_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("swarmd");
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from("swarmd")
}

async fn start_local_daemon() -> Result<()> {
    let exe = daemon_exe();
    let status = tokio::process::Command::new(&exe)
        .arg("start")
        .stdin(Stdio::null())
        .status()
        .await
        .with_context(|| format!("run {} start", exe.display()))?;
    if !status.success() {
        bail!("{} start failed ({status})", exe.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Replay hygiene

/// Removes what a terminal sends in answer to queries (device attributes,
/// cursor position and mode reports, OSC color and DCS replies) and keeps
/// everything the user typed.
pub fn strip_terminal_responses(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b {
            if let Some(len) = response_len(&data[i..]) {
                i += len;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Length of the terminal response starting at `s[0] == ESC`, if it is one.
fn response_len(s: &[u8]) -> Option<usize> {
    match s.get(1)? {
        b'[' => {
            // CSI [?>=]params [$] final, where the final marks a report:
            // c (DA), R (CPR), n (DSR), y (DECRPM), t (window ops), u (kitty keys).
            let mut j = 2;
            let private = matches!(s.get(j), Some(b'?' | b'>' | b'='));
            if private {
                j += 1;
            }
            while matches!(s.get(j), Some(b'0'..=b'9' | b';' | b':')) {
                j += 1;
            }
            if s.get(j) == Some(&b'$') {
                j += 1;
            }
            let fin = *s.get(j)?;
            let report = matches!(fin, b'c' | b'n' | b'y' | b't' | b'R') || (private && fin == b'u');
            // A bare ESC [ R would be a key, not a report.
            (report && j > 2).then_some(j + 1)
        }
        // OSC and DCS replies, terminated by BEL or ST.
        b']' | b'P' => {
            let mut j = 2;
            while j < s.len() {
                if s[j] == 0x07 {
                    return Some(j + 1);
                }
                if s[j] == 0x1b && s.get(j + 1) == Some(&b'\\') {
                    return Some(j + 2);
                }
                j += 1;
            }
            None
        }
        _ => None,
    }
}

/// Drops zsh's partial-line mark (`PROMPT_SP`) from replayed output.
///
/// Before each prompt zsh prints a standout `%` (`#` for root), one line's
/// worth of spaces and `\r \r`. At the width it was written for, that erases
/// itself; replayed into a narrower screen the spaces wrap and leave a stray
/// `%` after every command. Chunks may split a mark, so a possible start at the
/// end of a chunk is held back until the next one, or [`Self::finish`].
#[derive(Default)]
pub struct EolMarkFilter {
    carry: Vec<u8>,
}

/// How much of a possible mark is held back at most.
const EOL_MARK_MAX: usize = 4096;
/// Fewer spaces than this after the `%` is not zsh's mark.
const EOL_MARK_MIN_SPACES: usize = 8;

enum Scan {
    Full(usize),
    Partial,
    No,
}

impl EolMarkFilter {
    /// `data` without complete marks; a possible incomplete one at the end is kept.
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(data);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if buf[i] == 0x1b {
                match scan_eol_mark(&buf[i..]) {
                    Scan::Full(n) => {
                        i += n;
                        continue;
                    }
                    Scan::Partial if buf.len() - i <= EOL_MARK_MAX => {
                        self.carry = buf[i..].to_vec();
                        return out;
                    }
                    _ => {}
                }
            }
            out.push(buf[i]);
            i += 1;
        }
        out
    }

    /// Whatever is held back (the replay is over).
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.carry)
    }
}

fn scan_sgr(s: &[u8]) -> Scan {
    if s.first() != Some(&0x1b) {
        return Scan::No;
    }
    match s.get(1) {
        None => return Scan::Partial,
        Some(b'[') => {}
        Some(_) => return Scan::No,
    }
    let mut k = 2;
    while k < s.len() && (s[k].is_ascii_digit() || s[k] == b';') {
        k += 1;
    }
    match s.get(k) {
        None => Scan::Partial,
        Some(b'm') => Scan::Full(k + 1),
        Some(_) => Scan::No,
    }
}

/// SGR sequences, then `%` or `#`, SGR sequences, spaces and `\r \r`.
fn scan_eol_mark(s: &[u8]) -> Scan {
    let mut j = 0;
    let mut sgrs = 0;
    let skip_sgrs = |j: &mut usize, n: &mut usize| loop {
        match scan_sgr(&s[*j..]) {
            Scan::Full(len) => {
                *j += len;
                *n += 1;
            }
            Scan::Partial => return false,
            Scan::No => return true,
        }
    };
    if !skip_sgrs(&mut j, &mut sgrs) {
        return Scan::Partial;
    }
    match s.get(j) {
        None => return Scan::Partial,
        Some(b'%' | b'#') if sgrs > 0 => j += 1,
        Some(_) => return Scan::No,
    }
    if !skip_sgrs(&mut j, &mut sgrs) {
        return Scan::Partial;
    }
    let spaces = s[j..].iter().take_while(|&&b| b == b' ').count();
    j += spaces;
    let rest = &s[j..];
    let tail = b"\r \r";
    if rest.len() < tail.len() {
        return if tail.starts_with(rest) { Scan::Partial } else { Scan::No };
    }
    if spaces >= EOL_MARK_MIN_SPACES && rest.starts_with(tail) { Scan::Full(j + tail.len()) } else { Scan::No }
}

#[cfg(test)]
mod filter_tests {
    use super::strip_terminal_responses as strip;
    use super::EolMarkFilter;

    #[test]
    fn keeps_typing_and_keys() {
        assert_eq!(strip(b"ihello\x1b"), b"ihello\x1b");
        assert_eq!(strip(b"\x1b[A\x1b[1;5D:wq\r"), b"\x1b[A\x1b[1;5D:wq\r");
    }

    #[test]
    fn drops_responses() {
        assert_eq!(strip(b"\x1b[?62;22cab\x1b[24;80R"), b"ab");
        assert_eq!(strip(b"x\x1b]11;rgb:0000/0000/0000\x07y"), b"xy");
        assert_eq!(strip(b"\x1bP>|xterm(390)\x1b\\z"), b"z");
        assert_eq!(strip(b"\x1b[?2004;1$yq"), b"q");
    }

    fn filter_all(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = EolMarkFilter::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(f.push(c));
        }
        out.extend(f.finish());
        out
    }

    const MARK: &[u8] = b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m                                       \r \r";

    #[test]
    fn eol_mark_is_dropped_from_replay() {
        let data = [b"hi".as_slice(), MARK, b"\r\x1b[0mhost% "].concat();
        assert_eq!(filter_all(&[&data]), b"hi\r\x1b[0mhost% ");
    }

    #[test]
    fn eol_mark_split_across_chunks_is_dropped() {
        let data = [b"out\n".as_slice(), MARK, b"prompt"].concat();
        for cut in 1..data.len() {
            assert_eq!(filter_all(&[&data[..cut], &data[cut..]]), b"out\nprompt", "cut at {cut}");
        }
    }

    #[test]
    fn other_output_is_kept() {
        let keep: &[&[u8]] = &[
            b"50%        \r \r",                      // no styling: not zsh's mark
            b"\x1b[7m%\x1b[0m  \r \r",                // too few spaces
            b"\x1b[1mbold\x1b[0m and \x1b[31mred\x1b[0m", // ordinary colors
            b"\x1b[7m%\x1b[0m          \n",          // no \r \r
        ];
        for k in keep {
            assert_eq!(filter_all(&[k]), k.to_vec());
        }
    }
}
