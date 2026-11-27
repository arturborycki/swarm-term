//! Sharing a session read-only with another local user.
//!
//! `Request::Share` opens a socket of its own for one session, in
//! `/tmp/swarm-share-<uid>` (see `paths::share_dir`), for a set time. Only
//! the named user may connect (checked with the peer's credentials, like the
//! main socket checks this user), and a viewer can only list that one
//! session and attach to it read-only: no typing, no resizing, no other
//! request. When the time is up or the share is stopped, the socket goes
//! and connected viewers are cut off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use swarm_proto::{
    paths, read_frame, write_frame, AttachMode, ClientDesc, ClientMsg, Request, Response, ServerMsg, SessionInfo,
    ShareInfo, ShimInput, StreamId, MAX_SHARE_MINUTES, PROTOCOL_VERSION,
};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::registry::Registry;

/// Attachments one viewer connection may have at once.
const MAX_VIEWER_STREAMS: usize = 4;
/// Viewers one share may have connected at once.
const MAX_VIEWERS: usize = 16;

#[derive(Default)]
pub struct Shares {
    active: Mutex<HashMap<String, Share>>,
}

struct Share {
    info: ShareInfo,
    /// Serves the socket and its viewers; aborting it cuts them all off.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Share {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.info.path);
    }
}

impl Shares {
    /// Removes sockets a previous daemon left behind.
    pub fn new() -> Arc<Self> {
        if let Ok(entries) = std::fs::read_dir(paths::share_dir()) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".sock") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Arc::new(Shares::default())
    }

    pub fn share(self: &Arc<Self>, registry: &Arc<Registry>, id: &str, uid: u32, minutes: u32) -> Result<ShareInfo> {
        let Some(session) = registry.list().into_iter().find(|s| s.id == id) else {
            bail!("no session {id}");
        };
        if !session.is_running() {
            bail!("the session has exited");
        }
        let minutes = minutes.clamp(1, MAX_SHARE_MINUTES);
        let dir = paths::ensure_share_dir().context("make the folder for shared sessions")?;
        let token = random_token()?;
        let path = dir.join(format!("{token}.sock"));
        let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
        // Anyone may connect; only `uid` gets past the check.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        let expires_ms = swarm_proto::now_ms() + u64::from(minutes) * 60_000;
        let info = ShareInfo { session: session.id.clone(), uid, path: path.display().to_string(), expires_ms };
        let task = tokio::spawn(serve(
            self.clone(),
            token.clone(),
            listener,
            registry.clone(),
            session.id.clone(),
            uid,
            Duration::from_secs(u64::from(minutes) * 60),
        ));
        tracing::info!(session = %session.id, uid, minutes, "sharing a session read-only");
        self.active.lock().unwrap().insert(token, Share { info: info.clone(), task });
        Ok(info)
    }

    /// Stops sharing `id`, or every session.
    pub fn unshare(&self, id: Option<&str>) {
        self.active.lock().unwrap().retain(|_, s| id.is_some_and(|id| s.info.session != id));
    }

    pub fn list(&self) -> Vec<ShareInfo> {
        let now = swarm_proto::now_ms();
        let mut active = self.active.lock().unwrap();
        active.retain(|_, s| s.info.expires_ms > now);
        active.values().map(|s| s.info.clone()).collect()
    }

    fn ended(&self, token: &str) {
        self.active.lock().unwrap().remove(token);
    }
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(swarm_proto::hex::encode(&bytes))
}

async fn serve(
    shares: Arc<Shares>,
    token: String,
    listener: UnixListener,
    registry: Arc<Registry>,
    session: String,
    uid: u32,
    lasts: Duration,
) {
    let until = tokio::time::Instant::now() + lasts;
    // Dropped (time up, or the share stopped and this task aborted): every
    // viewer's task is aborted with it.
    let mut viewers = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == uid => {}
                    other => {
                        tracing::warn!(%session, "rejecting a viewer that isn't the user it's shared with: {other:?}");
                        continue;
                    }
                }
                while viewers.try_join_next().is_some() {}
                if viewers.len() >= MAX_VIEWERS {
                    continue;
                }
                let (rd, wr) = stream.into_split();
                viewers.spawn(view(registry.clone(), session.clone(), rd, wr));
            }
            _ = tokio::time::sleep_until(until) => break,
        }
    }
    tracing::info!(%session, "stopped sharing the session (time up)");
    drop(viewers);
    shares.ended(&token);
}

/// What a viewer sees of the session: not its metadata or other clients.
fn for_viewer(mut s: SessionInfo) -> SessionInfo {
    s.ui.clear();
    s.clients.clear();
    s
}

/// One viewer connection: the protocol's handshake, then only `List`,
/// `Ping` and read-only `Attach` to the shared session.
async fn view<R, W>(registry: Arc<Registry>, session: String, mut rd: R, mut wr: W)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let hello = tokio::time::timeout(Duration::from_secs(30), read_frame::<_, ClientMsg>(&mut rd)).await;
    let (client_id, client_name) = match hello.unwrap_or(Ok(None)) {
        Ok(Some(ClientMsg::Hello { proto, client_name, client_id })) => {
            let welcome = ServerMsg::Welcome {
                proto: PROTOCOL_VERSION,
                daemon_version: swarm_proto::VERSION.to_string(),
                hostname: crate::server::hostname(),
                os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            };
            if write_frame(&mut wr, &welcome).await.is_err() || proto != PROTOCOL_VERSION {
                return;
            }
            (client_id, format!("{} (shared view)", swarm_proto::clean_text(&client_name)))
        }
        _ => return,
    };
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(1024);
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });
    let mut streams: HashMap<StreamId, mpsc::Sender<ShimInput>> = HashMap::new();
    let mut next_stream: StreamId = 1;
    while let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut rd).await {
        streams.retain(|_, tx| !tx.is_closed());
        let (id, req) = match msg {
            // Read-only: typing and resizing go nowhere.
            ClientMsg::Hello { .. } | ClientMsg::Input { .. } | ClientMsg::Resize { .. } => continue,
            ClientMsg::Detach { stream } => {
                if let Some(tx) = streams.remove(&stream) {
                    let _ = tx.send(ShimInput::Detach).await;
                }
                continue;
            }
            ClientMsg::Request { id, req } => (id, req),
        };
        let result = match req {
            Request::Ping => Ok(Response::Pong),
            Request::List => {
                Ok(Response::Sessions(registry.list().into_iter().filter(|s| s.id == session).map(for_viewer).collect()))
            }
            Request::Attach { id: sid, mode, .. } if sid == session && streams.len() < MAX_VIEWER_STREAMS => {
                // What any attachment shows, not the history kept on disk.
                let mode = match mode {
                    AttachMode::History => AttachMode::Full,
                    other => other,
                };
                let stream = next_stream;
                next_stream += 1;
                let (in_tx, in_rx) = mpsc::channel::<ShimInput>(16);
                streams.insert(stream, in_tx);
                let desc = ClientDesc { client_id: client_id.clone(), name: client_name.clone(), readonly: true };
                tasks.spawn(crate::server::relay(registry.clone(), out_tx.clone(), id, stream, sid, desc, mode, None, in_rx));
                continue;
            }
            _ => Err("this is a read-only view of one session".to_string()),
        };
        if out_tx.send(ServerMsg::Response { id, result }).await.is_err() {
            break;
        }
    }
    for (_, tx) in streams.drain() {
        let _ = tx.try_send(ShimInput::Detach);
    }
}
