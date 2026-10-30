//! The shim's socket: one request per connection from `swarmd`.

use std::sync::Arc;
use std::time::Duration;

use swarm_proto::{read_frame, write_frame, SessionState, ShimInput, ShimMsg, ShimRequest};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};

use crate::session::Session;

/// Largest output frame sent to a client.
const REPLAY_CHUNK: usize = 64 * 1024;

pub async fn serve(session: Arc<Session>, listener: UnixListener, shutdown: Arc<Notify>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
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
        let session = session.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(session, stream, shutdown).await {
                tracing::debug!("connection ended: {e}");
            }
        });
    }
}

async fn handle(session: Arc<Session>, stream: UnixStream, shutdown: Arc<Notify>) -> std::io::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let Some(req) = read_frame::<_, ShimRequest>(&mut rd).await? else { return Ok(()) };
    match req {
        ShimRequest::Info => write_frame(&mut wr, &ShimMsg::Info(session.info())).await,
        ShimRequest::SetMeta(update) => {
            session.set_meta(update);
            write_frame(&mut wr, &ShimMsg::Ok).await
        }
        ShimRequest::Kill { signal } => {
            let reply = match session.kill(signal) {
                Ok(()) => ShimMsg::Ok,
                Err(e) => ShimMsg::Err(format!("{e:#}")),
            };
            write_frame(&mut wr, &reply).await
        }
        ShimRequest::Remove => {
            remove(&session).await;
            write_frame(&mut wr, &ShimMsg::Ok).await?;
            shutdown.notify_one();
            Ok(())
        }
        ShimRequest::Watch => {
            let mut rx = session.subscribe();
            let current = rx.borrow_and_update().clone();
            write_frame(&mut wr, &ShimMsg::Changed(current)).await?;
            let mut probe = [0u8; 1];
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                        let info = rx.borrow_and_update().clone();
                        write_frame(&mut wr, &ShimMsg::Changed(info)).await?;
                    }
                    // The watcher never sends anything; a read completes when it goes away.
                    _ = tokio::io::AsyncReadExt::read(&mut rd, &mut probe) => return Ok(()),
                }
            }
        }
        ShimRequest::Attach { client, mode, size } => {
            let mut att = session.attach(client, mode, size);
            let client_id = att.client_id;
            let result = async {
                write_frame(
                    &mut wr,
                    &ShimMsg::Attached { info: att.info.clone(), replay_from: att.replay_from, live_from: att.live_from },
                )
                .await?;
                for (offset, bytes) in std::mem::take(&mut att.prelude) {
                    write_frame(&mut wr, &ShimMsg::Output { offset, data: bytes, replay: true }).await?;
                }
                send_replay(&mut wr, att.replay_from, &att.replay).await?;
                att.replay = Vec::new();
                if let Some((code, signal)) = att.exited {
                    write_frame(&mut wr, &ShimMsg::Exited { code, signal }).await?;
                }
                if att.wants_repaint {
                    let s = session.clone();
                    tokio::spawn(async move { s.nudge_repaint().await });
                }

                // Frames from the daemon arrive through a task so that a partial
                // read is never cancelled by `select!`.
                let (in_tx, mut in_rx) = mpsc::channel::<ShimInput>(64);
                let reader = tokio::spawn(async move {
                    while let Ok(Some(msg)) = read_frame::<_, ShimInput>(&mut rd).await {
                        if in_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                });
                loop {
                    tokio::select! {
                        out = att.rx.recv() => match out {
                            Some(msg) => write_frame(&mut wr, &msg).await?,
                            None => {
                                if let Some(resume_at) = att.lagged_at() {
                                    write_frame(&mut wr, &ShimMsg::Lagged { resume_at }).await?;
                                }
                                break;
                            }
                        },
                        input = in_rx.recv() => match input {
                            Some(ShimInput::Input(data)) => session.input(client_id, data),
                            Some(ShimInput::Resize(size)) => session.client_resize(client_id, size),
                            Some(ShimInput::Detach) | None => break,
                        },
                    }
                }
                reader.abort();
                Ok::<_, std::io::Error>(())
            }
            .await;
            session.detach(client_id);
            result
        }
    }
}

async fn send_replay(wr: &mut OwnedWriteHalf, from: u64, bytes: &[u8]) -> std::io::Result<()> {
    let mut offset = from;
    for chunk in bytes.chunks(REPLAY_CHUNK) {
        write_frame(wr, &ShimMsg::Output { offset, data: chunk.to_vec(), replay: true }).await?;
        offset += chunk.len() as u64;
    }
    Ok(())
}

/// Ends the session's process: SIGHUP like a closed terminal, then SIGKILL.
async fn remove(session: &Session) {
    let mut rx = session.subscribe();
    for (signal, wait) in [(libc::SIGHUP, 3), (libc::SIGKILL, 2)] {
        if !session.is_running() {
            return;
        }
        let _ = session.kill(signal);
        let exited = tokio::time::timeout(
            Duration::from_secs(wait),
            rx.wait_for(|i| !matches!(i.state, SessionState::Running)),
        )
        .await;
        if matches!(exited, Ok(Ok(_))) {
            return;
        }
    }
}
