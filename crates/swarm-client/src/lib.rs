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
use tokio::sync::{broadcast, mpsc, oneshot};

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
            .args(["-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", host, &remote])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("run ssh")?;
        let wr = child.stdin.take().context("ssh stdin")?;
        let rd = child.stdout.take().context("ssh stdout")?;
        let client = Client::handshake(rd, wr, client_name).await?;
        // Keep ssh alive as long as the client; it ends when the pipes close.
        tokio::spawn(async move {
            let _ = child.wait().await;
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
        });

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if write_frame(&mut wr, &msg).await.is_err() {
                    break;
                }
            }
        });
        let reader_shared = shared.clone();
        tokio::spawn(async move {
            let s = reader_shared;
            loop {
                let msg = match read_frame::<_, ServerMsg>(&mut rd).await {
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
