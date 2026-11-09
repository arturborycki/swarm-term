//! Client connections: requests, event subscriptions and attachment relays.

use std::collections::HashMap;
use std::sync::Arc;

use swarm_proto::{
    read_frame, write_frame, ClientDesc, ClientMsg, Request, Response, ServerMsg, SessionEvent, ShimInput, ShimMsg, StreamId,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, Notify};

use crate::registry::Registry;

const OUTBOUND_QUEUE: usize = 1024;

pub struct Server {
    pub registry: Arc<Registry>,
    pub shutdown: Arc<Notify>,
}

impl Server {
    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == swarm_proto::paths::uid() => {}
                other => {
                    tracing::warn!("rejecting connection from {other:?}");
                    continue;
                }
            }
            let this = self.clone();
            tokio::spawn(async move {
                let (rd, wr) = stream.into_split();
                this.handle(rd, wr).await;
            });
        }
    }

    pub async fn handle<R, W>(self: Arc<Self>, mut rd: R, mut wr: W)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        // A connection that never says hello mustn't hold a task forever.
        let hello = tokio::time::timeout(std::time::Duration::from_secs(30), read_frame::<_, ClientMsg>(&mut rd)).await;
        let (client_id, client_name) = match hello.unwrap_or(Ok(None)) {
            Ok(Some(ClientMsg::Hello { proto, client_name, client_id })) => {
                let welcome = ServerMsg::Welcome {
                    proto: PROTOCOL_VERSION,
                    daemon_version: swarm_proto::VERSION.to_string(),
                    hostname: hostname(),
                    os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                };
                if write_frame(&mut wr, &welcome).await.is_err() || proto != PROTOCOL_VERSION {
                    return;
                }
                (client_id, client_name)
            }
            _ => return,
        };
        tracing::debug!(client_id, client_name, "client connected");

        let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(OUTBOUND_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if write_frame(&mut wr, &msg).await.is_err() {
                    break;
                }
            }
        });

        let mut streams: HashMap<StreamId, mpsc::Sender<ShimInput>> = HashMap::new();
        let mut next_stream: StreamId = 1;
        let mut tasks = Vec::new();

        while let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut rd).await {
            // A connection can live for weeks (the app lists sessions every
            // few seconds): drop handles of tasks that are done.
            tasks.retain(|t: &tokio::task::JoinHandle<()>| !t.is_finished());
            match msg {
                ClientMsg::Hello { .. } => {}
                ClientMsg::Input { stream, data } => {
                    if let Some(tx) = streams.get(&stream) {
                        let _ = tx.send(ShimInput::Input(data)).await;
                    }
                }
                ClientMsg::Resize { stream, size } => {
                    if let Some(tx) = streams.get(&stream) {
                        let _ = tx.send(ShimInput::Resize(size)).await;
                    }
                }
                ClientMsg::Detach { stream } => {
                    if let Some(tx) = streams.remove(&stream) {
                        let _ = tx.send(ShimInput::Detach).await;
                    }
                }
                ClientMsg::Request { id, req: Request::Attach { id: sid, mode, size, readonly } } => {
                    let stream = next_stream;
                    next_stream += 1;
                    let (in_tx, in_rx) = mpsc::channel::<ShimInput>(256);
                    streams.insert(stream, in_tx);
                    let desc = ClientDesc { client_id: client_id.clone(), name: client_name.clone(), readonly };
                    let registry = self.registry.clone();
                    let out = out_tx.clone();
                    tasks.push(tokio::spawn(async move {
                        relay(registry, out, id, stream, sid, desc, mode, size, in_rx).await;
                    }));
                }
                ClientMsg::Request { id, req: Request::Subscribe } => {
                    let mut events = self.registry.subscribe();
                    let registry = self.registry.clone();
                    let out = out_tx.clone();
                    let _ = out.send(ServerMsg::Response { id, result: Ok(Response::Ok) }).await;
                    tasks.push(tokio::spawn(async move {
                        loop {
                            match events.recv().await {
                                Ok(ev) => {
                                    if out.send(ServerMsg::Event(ev)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                    // Events were missed: send every session's
                                    // current state, so the client catches up
                                    // (and re-lists, which also covers removals).
                                    tracing::warn!("event subscriber lagged by {n}; resending session states");
                                    for info in registry.list() {
                                        if out.send(ServerMsg::Event(SessionEvent::Updated(info))).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }));
                }
                ClientMsg::Request { id, req } => {
                    let this = self.clone();
                    let out = out_tx.clone();
                    tasks.push(tokio::spawn(async move {
                        let result = this.request(req).await.map_err(|e| format!("{e:#}"));
                        let _ = out.send(ServerMsg::Response { id, result }).await;
                    }));
                }
            }
        }

        // Client gone: detach everything it had attached.
        for (_, tx) in streams.drain() {
            let _ = tx.try_send(ShimInput::Detach);
        }
        for t in tasks {
            t.abort();
        }
        drop(out_tx);
        let _ = writer.await;
        tracing::debug!(client_id, "client disconnected");
    }

    async fn request(&self, req: Request) -> anyhow::Result<Response> {
        let r = &self.registry;
        Ok(match req {
            Request::Ping => Response::Pong,
            Request::List => Response::Sessions(r.list()),
            Request::Create(opts) => Response::Session(r.create(opts).await?),
            Request::Kill { id, signal } => {
                r.kill(&id, signal).await?;
                Response::Ok
            }
            Request::Remove { id } => {
                r.remove(&id).await?;
                Response::Ok
            }
            Request::SetMeta { id, update } => Response::Session(r.set_meta(&id, update).await?),
            Request::Shutdown => {
                self.shutdown.notify_one();
                Response::Ok
            }
            Request::UpgradeShims { force } => Response::ShimsUpgraded(r.upgrade_shims(force).await),
            Request::ApplySpill => {
                r.apply_spill().await;
                Response::Ok
            }
            Request::Attach { .. } | Request::Subscribe => unreachable!("handled by the connection loop"),
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn relay(
    registry: Arc<Registry>,
    out: mpsc::Sender<ServerMsg>,
    request_id: u64,
    stream: StreamId,
    session_id: String,
    desc: ClientDesc,
    mode: swarm_proto::AttachMode,
    size: Option<swarm_proto::Size>,
    mut input: mpsc::Receiver<ShimInput>,
) {
    let att = match registry.attach(&session_id, desc.clone(), mode, size).await {
        Ok(a) => a,
        Err(e) => {
            let _ = out.send(ServerMsg::Response { id: request_id, result: Err(format!("{e:#}")) }).await;
            return;
        }
    };
    let response = Response::Attached { stream, info: att.info, replay_from: att.replay_from, live_from: att.live_from };
    if out.send(ServerMsg::Response { id: request_id, result: Ok(response) }).await.is_err() {
        return;
    }

    // Where the client's stream has got to, so it can carry on from there
    // if the shim connection ends because the shim was updated in place.
    let mut next = att.replay_from;
    let mut size = size;
    let mut exited = false;
    let mut conn = att.stream;
    let reason = loop {
        let (mut shim_rd, mut shim_wr) = conn.into_split();
        // Frames are read in a task so a partial read is never cancelled.
        let (frames_tx, mut frames) = mpsc::channel::<Result<ShimMsg, String>>(64);
        let reader = tokio::spawn(async move {
            loop {
                let (frame, last) = match read_frame::<_, ShimMsg>(&mut shim_rd).await {
                    Ok(Some(msg)) => (Ok(msg), false),
                    Ok(None) => (Err("detached".to_string()), true),
                    Err(e) => (Err(format!("session connection lost: {e}")), true),
                };
                if frames_tx.send(frame).await.is_err() || last {
                    break;
                }
            }
        });
        // `Some(reason)`: the shim connection ended; `None`: the client is
        // done (detached, gone, or told to resume itself after lagging).
        let ended: Option<String> = loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(Ok(ShimMsg::Output { offset, data, replay })) => {
                        next = offset + data.len() as u64;
                        // Blocking here when the client is slow lets the shim's
                        // queue fill, which turns into `Lagged` instead of
                        // unbounded memory.
                        if out.send(ServerMsg::Output { stream, offset, data, replay }).await.is_err() {
                            break None;
                        }
                    }
                    Some(Ok(ShimMsg::Exited { code, signal })) => {
                        exited = true;
                        let _ = out.send(ServerMsg::Exited { stream, code, signal }).await;
                    }
                    Some(Ok(ShimMsg::Lagged { resume_at })) => {
                        let _ = out.send(ServerMsg::Lagged { stream, resume_at }).await;
                        break None;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(reason)) => break Some(reason),
                    None => break Some("detached".to_string()),
                },
                msg = input.recv() => match msg {
                    Some(msg) => {
                        if let ShimInput::Resize(s) = &msg {
                            size = Some(*s);
                        }
                        let detach = matches!(msg, ShimInput::Detach);
                        // A failed write shows up as the connection ending.
                        let _ = write_frame(&mut shim_wr, &msg).await;
                        if detach {
                            break None;
                        }
                    }
                    None => break None,
                },
            }
        };
        reader.abort();
        let Some(reason) = ended else { break None };
        // A shim updated in place keeps its socket: carry on from `next` on
        // its new program, unnoticed by the client.
        if !exited {
            let resumed = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                registry.attach(&session_id, desc.clone(), swarm_proto::AttachMode::ResumeFrom(next), size),
            )
            .await;
            if let Ok(Ok(att)) = resumed {
                tracing::debug!(session_id, next, "resumed the stream after the shim was updated");
                conn = att.stream;
                continue;
            }
        }
        break Some(reason);
    };
    if let Some(reason) = reason {
        let _ = out.send(ServerMsg::StreamClosed { stream, reason }).await;
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
