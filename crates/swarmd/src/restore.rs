//! Restore after reboot (opt-in, DESIGN §12 M4).
//!
//! The daemon keeps a snapshot of its running sessions in the state dir. When
//! it starts and a snapshot session has no shim left (the machine rebooted, or
//! the shim was killed), it creates a fresh shell in its place with the same
//! name, folder and UI metadata (tab order, group, auto-attach), without the
//! processes that ran in it. It's on when `$STATE_DIR/restore-after-reboot`
//! exists (`swarmd restore on`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use swarm_proto::{paths, CreateOpts, SessionInfo};

use crate::registry::Registry;

/// UI metadata that belongs to the old process, not the session.
const PROCESS_META: [&str; 3] = ["warp.session_id", "warp.shell", "created_by"];

fn snapshot_path() -> PathBuf {
    paths::state_dir().join("sessions.snapshot")
}

fn marker_path() -> PathBuf {
    paths::state_dir().join("restore-after-reboot")
}

pub fn is_enabled() -> bool {
    marker_path().exists()
}

pub fn set_enabled(on: bool) -> Result<()> {
    let marker = marker_path();
    if on {
        std::fs::create_dir_all(paths::state_dir())?;
        std::fs::write(&marker, b"").with_context(|| format!("write {}", marker.display()))
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

fn save(sessions: &[SessionInfo]) -> Result<()> {
    let running: Vec<&SessionInfo> = sessions.iter().filter(|s| s.is_running()).collect();
    let bytes = postcard::to_stdvec(&running)?;
    let path = snapshot_path();
    std::fs::create_dir_all(paths::state_dir())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn load() -> Vec<SessionInfo> {
    std::fs::read(snapshot_path())
        .ok()
        .and_then(|b| postcard::from_bytes::<Vec<SessionInfo>>(&b).ok())
        .unwrap_or_default()
}

/// Snapshot sessions with no live session of the same id.
pub fn lost(snapshot: Vec<SessionInfo>, live: &[SessionInfo]) -> Vec<SessionInfo> {
    snapshot.into_iter().filter(|s| !live.iter().any(|l| l.id == s.id)).collect()
}

/// How to recreate `s`: its shell (or the login shell), folder, name and
/// session metadata.
pub fn recreate_opts(s: &SessionInfo) -> CreateOpts {
    let program = s.command.first().map(|a| a.rsplit('/').next().unwrap_or(a).trim_start_matches('-').to_string());
    let is_shell = program.is_some_and(|p| matches!(p.as_str(), "zsh" | "bash" | "fish" | "sh"));
    let mut ui = s.ui.clone();
    for key in PROCESS_META {
        ui.remove(key);
    }
    ui.insert("restored_from".into(), s.id.clone());
    CreateOpts {
        command: if is_shell { s.command.clone() } else { Vec::new() },
        cwd: s.cwd.clone(),
        env: Vec::new(),
        size: s.size,
        name: s.name.clone(),
        ui,
    }
}

/// Recreates lost sessions (when enabled), then keeps the snapshot current.
pub async fn start(registry: &Arc<Registry>) {
    if is_enabled() {
        for s in lost(load(), &registry.list()) {
            match registry.create(recreate_opts(&s)).await {
                Ok(new) => tracing::info!(old = %s.id, new = %new.id, "restored session after restart"),
                Err(e) => tracing::warn!(old = %s.id, "couldn't restore session: {e:#}"),
            }
        }
    }
    let mut events = registry.subscribe();
    let registry = registry.clone();
    if let Err(e) = save(&registry.list()) {
        tracing::warn!("couldn't save session snapshot: {e:#}");
    }
    tokio::spawn(async move {
        loop {
            // Coalesce bursts of updates into one write.
            if events.recv().await.is_err() && events.is_empty() {
                events = registry.subscribe();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            while events.try_recv().is_ok() {}
            if let Err(e) = save(&registry.list()) {
                tracing::warn!("couldn't save session snapshot: {e:#}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_proto::{SessionState, Size};

    fn session(id: &str, command: &[&str]) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: Some("build".into()),
            title: String::new(),
            cwd: Some("/tmp/project".into()),
            command: command.iter().map(|s| s.to_string()).collect(),
            pid: 1,
            foreground: None,
            state: SessionState::Running,
            created_ms: 0,
            last_output_ms: 0,
            last_input_ms: 0,
            size: Size { cols: 100, rows: 30 },
            clients: Vec::new(),
            ui: [("warp.session_id", "42"), ("group", "builds"), ("auto_attach", "true")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            buffer_start: 0,
            buffer_end: 0,
            alt_screen: false,
        }
    }

    #[test]
    fn recreates_shells_with_their_session_metadata() {
        let opts = recreate_opts(&session("A", &["/bin/zsh", "-l"]));
        assert_eq!(opts.command, vec!["/bin/zsh", "-l"]);
        assert_eq!(opts.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(opts.name.as_deref(), Some("build"));
        assert_eq!(opts.ui.get("group").map(String::as_str), Some("builds"));
        assert!(!opts.ui.contains_key("warp.session_id"), "the old shell's Warp id doesn't carry over");
        assert_eq!(opts.ui.get("restored_from").map(String::as_str), Some("A"));
    }

    #[test]
    fn other_programs_come_back_as_a_login_shell() {
        assert!(recreate_opts(&session("B", &["/usr/bin/htop"])).command.is_empty());
    }

    #[test]
    fn only_sessions_without_a_shim_are_lost() {
        let lost = lost(vec![session("A", &["zsh"]), session("B", &["zsh"])], &[session("A", &["zsh"])]);
        assert_eq!(lost.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["B"]);
    }
}
