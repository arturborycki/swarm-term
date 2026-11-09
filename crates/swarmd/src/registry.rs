//! The daemon's view of the sessions: one watch connection per shim keeps a
//! cache of `SessionInfo` and feeds `SessionEvent`s to subscribers. The shims
//! are the source of truth, so a restarted daemon just re-adopts them.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use swarm_proto::{
    paths, read_frame, write_frame, AttachMode, ClientDesc, CreateOpts, MetaUpdate, SessionEvent, SessionId,
    SessionInfo, ShimMsg, ShimRequest, ShimUpgrade, ShimUpgradeOutcome, Size,
};
use tokio::net::UnixStream;
use tokio::sync::broadcast;

/// How long a shim may take to answer a request.
const SHIM_TIMEOUT: Duration = Duration::from_secs(5);
/// For requests that wait for the session's process (`Remove`) or start a
/// program (`Upgrade`).
const SHIM_SLOW_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a known session's socket to come back.
const SOCKET_RETURN_WAIT: Duration = Duration::from_millis(3500);
/// How often the shims folder is checked for sockets the daemon doesn't know.
const RESCAN_EVERY: Duration = Duration::from_secs(3);

/// Runs `fut`, failing if it takes longer than `limit`.
async fn timed<T>(limit: Duration, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => bail!("the session didn't answer within {} s", limit.as_secs()),
    }
}

/// Whether adopting failed because nothing listens on the socket any more
/// (its shim is gone), rather than because the shim was slow.
fn is_stale(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(io.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
        })
    }) || e.to_string().starts_with("unexpected watch reply")
}

pub struct Registry {
    runtime: PathBuf,
    shim_exe: PathBuf,
    sessions: Mutex<HashMap<SessionId, SessionInfo>>,
    events: broadcast::Sender<SessionEvent>,
}

/// An open attachment to a shim, before the relay starts.
pub struct ShimAttachment {
    pub stream: UnixStream,
    pub info: SessionInfo,
    pub replay_from: u64,
    pub live_from: u64,
}

impl Registry {
    pub fn new(runtime: PathBuf, shim_exe: PathBuf) -> Arc<Registry> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Registry { runtime, shim_exe, sessions: Mutex::new(HashMap::new()), events })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Re-adopts every shim still running from a previous daemon, all at once
    /// and each within `SHIM_TIMEOUT`, so a shim that doesn't answer can't
    /// hold up the others or the daemon's start.
    pub async fn adopt_existing(self: &Arc<Self>) -> Result<()> {
        paths::ensure_private_dir(&paths::shims_dir(&self.runtime))?;
        self.adopt_new().await;
        Ok(())
    }

    /// Adopts shims whose sockets appeared since the last look: a shim that
    /// had to put its socket back (systemd removes `/run/user/<uid>` at the
    /// last logout), or one a previous daemon left. Sockets nobody listens on
    /// are removed; a shim that doesn't answer is tried again next time.
    pub async fn adopt_new(self: &Arc<Self>) {
        let dir = paths::shims_dir(&self.runtime);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut tasks = tokio::task::JoinSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".sock")) else {
                continue;
            };
            if self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(id) {
                continue;
            }
            let (this, id) = (self.clone(), id.to_string());
            tasks.spawn(async move {
                match this.adopt(&id).await {
                    Ok(info) => tracing::info!(id, pid = info.pid, "adopted session"),
                    Err(e) if is_stale(&e) => {
                        tracing::info!(id, "removing stale shim socket: {e:#}");
                        let _ = std::fs::remove_file(&path);
                    }
                    Err(e) => tracing::warn!(id, "couldn't adopt session, will try again: {e:#}"),
                }
            });
        }
        while tasks.join_next().await.is_some() {}
    }

    /// Session ids with a socket in the shims folder, adopted or not.
    pub fn socket_ids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(paths::shims_dir(&self.runtime)) else { return Vec::new() };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".sock").map(str::to_string))
            .collect()
    }

    /// Looks for new shim sockets every few seconds.
    pub async fn watch_shims_dir(self: Arc<Self>) {
        loop {
            tokio::time::sleep(RESCAN_EVERY).await;
            self.adopt_new().await;
        }
    }

    /// Starts watching a shim and returns its current info.
    async fn adopt(self: &Arc<Self>, id: &str) -> Result<SessionInfo> {
        let (stream, info) = timed(SHIM_TIMEOUT, async {
            let mut stream = self.connect(id).await?;
            write_frame(&mut stream, &ShimRequest::Watch).await?;
            match read_frame::<_, ShimMsg>(&mut stream).await? {
                Some(ShimMsg::Changed(info)) => Ok((stream, info)),
                other => bail!("unexpected watch reply {other:?}"),
            }
        })
        .await?;
        let mut stream = stream;
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if sessions.contains_key(id) {
                return Ok(info);
            }
            sessions.insert(id.to_string(), info.clone());
        }
        let _ = self.events.send(SessionEvent::Created(info.clone()));

        let this = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            loop {
                let mut removed = false;
                while let Ok(Some(ShimMsg::Changed(info))) = read_frame::<_, ShimMsg>(&mut stream).await {
                    // A late update must not resurrect a session that was just removed.
                    match this.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&id) {
                        Some(slot) => *slot = info.clone(),
                        None => {
                            removed = true;
                            break;
                        }
                    }
                    let _ = this.events.send(SessionEvent::Updated(info));
                }
                // A shim replacing its program (an in-place update) keeps its
                // socket listening: watch the new program.
                match this.rewatch(&id).await {
                    Some(s) if !removed => stream = s,
                    _ => break,
                }
            }
            // The shim is gone: removed, or its exit retention passed.
            this.forget(&id);
        });
        Ok(info)
    }

    /// Watches a shim again after its watch connection ended, if it's still
    /// there (it was updated in place). `None` if it's gone.
    async fn rewatch(&self, id: &str) -> Option<UnixStream> {
        // One try: a shim that put its socket back keeps the watch connection,
        // so a missing socket here means the shim is gone.
        let mut stream = UnixStream::connect(paths::shim_socket(&self.runtime, id)).await.ok()?;
        write_frame(&mut stream, &ShimRequest::Watch).await.ok()?;
        let reply = tokio::time::timeout(Duration::from_secs(10), read_frame::<_, ShimMsg>(&mut stream)).await;
        let Ok(Ok(Some(ShimMsg::Changed(info)))) = reply else { return None };
        match self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(id) {
            Some(slot) => *slot = info.clone(),
            None => return None,
        }
        let _ = self.events.send(SessionEvent::Updated(info));
        Some(stream)
    }

    /// The shim's build, handoff format and in-place update count.
    async fn shim_version(&self, id: &str) -> Result<(String, u32, u32)> {
        timed(SHIM_TIMEOUT, async {
            let mut stream = self.connect(id).await?;
            write_frame(&mut stream, &ShimRequest::Version).await?;
            match read_frame::<_, ShimMsg>(&mut stream).await? {
                Some(ShimMsg::Version { build_id, handoff, generation }) => Ok((build_id, handoff, generation)),
                other => bail!("unexpected reply {other:?}"),
            }
        })
        .await
    }

    /// Applies the spill-to-disk setting to every running session: on starts
    /// spilling, off stops and deletes what was spilled.
    pub async fn apply_spill(&self) {
        let cfg = crate::spill::config();
        for info in self.list().into_iter().filter(|s| s.is_running()) {
            if let Err(e) = self.simple(&info.id, ShimRequest::SetSpill(cfg.clone())).await {
                tracing::info!(id = %info.id, "spill setting not applied: {e:#}");
            }
        }
    }

    /// Moves every running session to this daemon's `swarm-shim`, one at a
    /// time, without restarting them (`force`: even those already on it).
    pub async fn upgrade_shims(&self, force: bool) -> Vec<ShimUpgrade> {
        let mut results = Vec::new();
        for info in self.list() {
            let outcome = self.upgrade_shim(&info, force).await;
            match &outcome {
                ShimUpgradeOutcome::Upgraded => tracing::info!(id = %info.id, "moved the session to the new shim"),
                ShimUpgradeOutcome::Failed(e) => tracing::warn!(id = %info.id, "couldn't update its shim: {e}"),
                ShimUpgradeOutcome::Skipped(why) => tracing::info!(id = %info.id, "shim left as is: {why}"),
                ShimUpgradeOutcome::UpToDate => {}
            }
            results.push(ShimUpgrade { id: info.id, outcome });
        }
        results
    }

    async fn upgrade_shim(&self, info: &SessionInfo, force: bool) -> ShimUpgradeOutcome {
        if !info.is_running() {
            return ShimUpgradeOutcome::Skipped("the session has exited".into());
        }
        let Ok((build, _, generation)) = self.shim_version(&info.id).await else {
            return ShimUpgradeOutcome::Skipped(
                "its shim predates in-place updates; it moves to the new build when the session is restarted".into(),
            );
        };
        if build == swarm_proto::BUILD_ID && !force {
            return ShimUpgradeOutcome::UpToDate;
        }
        let exe = self.shim_exe.display().to_string();
        if let Err(e) = self.simple(&info.id, ShimRequest::Upgrade { exe }).await {
            return ShimUpgradeOutcome::Failed(format!("{e:#}"));
        }
        // The shim answered and is switching programs; the new one answers
        // with a higher generation once it has the session.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Ok((_, _, now)) = self.shim_version(&info.id).await {
                if now > generation {
                    return ShimUpgradeOutcome::Upgraded;
                }
            }
        }
        ShimUpgradeOutcome::Failed(format!(
            "the shim kept its old program; see {}",
            paths::log_dir().join("shims.log").display()
        ))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut v: Vec<_> = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).values().cloned().collect();
        v.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then_with(|| a.id.cmp(&b.id)));
        v
    }

    pub async fn create(self: &Arc<Self>, opts: CreateOpts) -> Result<SessionInfo> {
        let id = ulid::Ulid::new().to_string();
        // The options (command, folder, environment, names) go through a pipe,
        // not the command line, where other users could read them with `ps`.
        let encoded = postcard::to_stdvec(&opts)?;
        let log_dir = paths::log_dir();
        std::fs::create_dir_all(&log_dir).ok();
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("shims.log"));
        let stderr = match log {
            Ok(f) => Stdio::from(f),
            Err(_) => Stdio::null(),
        };

        let mut cmd = std::process::Command::new(&self.shim_exe);
        cmd.arg("--id")
            .arg(&id)
            .arg("--runtime-dir")
            .arg(&self.runtime)
            .arg("--opts-stdin");
        if let Some(cfg) = crate::spill::config() {
            cmd.arg("--spill-dir").arg(&cfg.dir).arg("--spill-limit").arg(cfg.limit.to_string());
        }
        cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(stderr);
        // Own session: no controlling terminal, untouched by our signals.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().with_context(|| format!("spawn {}", self.shim_exe.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A thread, so a large environment can't block the runtime.
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&encoded);
            });
        }

        let sock = paths::shim_socket(&self.runtime, &id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if UnixStream::connect(&sock).await.is_ok() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                bail!("shim exited during startup ({status}); see {}", log_dir.join("shims.log").display());
            }
            if tokio::time::Instant::now() > deadline {
                let _ = child.kill();
                bail!("shim did not start within 5s");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Reap the shim when it eventually exits (if we're still around).
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        self.adopt(&id).await
    }

    pub async fn kill(&self, id: &str, signal: i32) -> Result<()> {
        self.simple(id, ShimRequest::Kill { signal }).await
    }

    /// Returns once the session is gone, so a following `list` won't show it.
    pub async fn remove(&self, id: &str) -> Result<()> {
        self.simple(id, ShimRequest::Remove).await?;
        self.forget(id);
        Ok(())
    }

    fn refresh(&self, info: &SessionInfo) {
        if let Some(slot) = self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&info.id) {
            *slot = info.clone();
        }
    }

    fn forget(&self, id: &str) {
        if self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id).is_some() {
            let _ = self.events.send(SessionEvent::Removed { id: id.to_string() });
            tracing::info!(id, "session gone");
        }
    }

    pub async fn trim(&self, id: &str, keep: u64) -> Result<()> {
        self.simple(id, ShimRequest::Trim { keep }).await
    }

    pub async fn set_meta(&self, id: &str, update: MetaUpdate) -> Result<SessionInfo> {
        self.simple(id, ShimRequest::SetMeta(update)).await?;
        self.info(id).await
    }

    pub async fn info(&self, id: &str) -> Result<SessionInfo> {
        timed(SHIM_TIMEOUT, async {
            let mut stream = self.connect(id).await?;
            write_frame(&mut stream, &ShimRequest::Info).await?;
            match read_frame::<_, ShimMsg>(&mut stream).await? {
                Some(ShimMsg::Info(info)) => Ok(info),
                other => bail!("unexpected reply {other:?}"),
            }
        })
        .await
    }

    pub async fn attach(
        &self,
        id: &str,
        client: ClientDesc,
        mode: AttachMode,
        size: Option<Size>,
    ) -> Result<ShimAttachment> {
        timed(SHIM_TIMEOUT, async {
            let mut stream = self.connect(id).await?;
            write_frame(&mut stream, &ShimRequest::Attach { client, mode, size }).await?;
            match read_frame::<_, ShimMsg>(&mut stream).await? {
                Some(ShimMsg::Attached { info, replay_from, live_from }) => {
                    // The shim's info now counts this client; don't wait for the
                    // watch connection to deliver it.
                    self.refresh(&info);
                    Ok(ShimAttachment { stream, info, replay_from, live_from })
                }
                other => bail!("unexpected attach reply {other:?}"),
            }
        })
        .await
    }

    async fn simple(&self, id: &str, req: ShimRequest) -> Result<()> {
        // `Remove` waits up to 5 s for the process to end before answering;
        // `Upgrade` first runs the new program, whose first start can be slow
        // (macOS checks a newly installed binary).
        let limit = if matches!(req, ShimRequest::Remove | ShimRequest::Upgrade { .. }) { SHIM_SLOW_TIMEOUT } else { SHIM_TIMEOUT };
        timed(limit, async {
            let mut stream = self.connect(id).await?;
            write_frame(&mut stream, &req).await?;
            match read_frame::<_, ShimMsg>(&mut stream).await? {
                Some(ShimMsg::Ok) => Ok(()),
                Some(ShimMsg::Err(e)) => bail!(e),
                // A shim closes the connection on a request it can't read.
                None => bail!("its shim is from an older build that doesn't support this"),
                other => bail!("unexpected reply {other:?}"),
            }
        })
        .await
    }

    async fn connect(&self, id: &str) -> Result<UnixStream> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("invalid session id {id:?}");
        }
        let sock = paths::shim_socket(&self.runtime, id);
        // A known session's socket may be missing for a moment: its shim
        // puts it back within a few seconds after the runtime folder was
        // removed (logout without lingering).
        let deadline = tokio::time::Instant::now() + SOCKET_RETURN_WAIT;
        loop {
            match UnixStream::connect(&sock).await {
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && tokio::time::Instant::now() < deadline
                        && self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(id) =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                r => return r.with_context(|| format!("no session {id}")),
            }
        }
    }
}
