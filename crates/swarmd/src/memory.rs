//! Caps the scrollback all sessions hold together. Each shim keeps up to its
//! own limit (16 MB by default), so many sessions can add up; when the total
//! passes the cap, the idlest sessions keep only their last `TRIM_KEEP`
//! bytes: exited ones first, then detached, then the least recently active.

use std::sync::Arc;
use std::time::Duration;

use swarm_proto::SessionInfo;

use crate::registry::Registry;

/// Default cap on all sessions' scrollback together.
pub const DEFAULT_TOTAL: u64 = 512 << 20;
/// What a trimmed session keeps.
pub const TRIM_KEEP: u64 = 1 << 20;
const CHECK_EVERY: Duration = Duration::from_secs(10);

/// The cap: `SWARM_SCROLLBACK_TOTAL` (bytes) or `DEFAULT_TOTAL`.
pub fn total_cap() -> u64 {
    std::env::var("SWARM_SCROLLBACK_TOTAL").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TOTAL)
}

fn held(s: &SessionInfo) -> u64 {
    s.buffer_end.saturating_sub(s.buffer_start)
}

/// The sessions to trim so the total gets back under 90% of `cap`, idlest
/// first; none while the total is within `cap`.
pub fn to_trim(sessions: &[SessionInfo], cap: u64) -> Vec<String> {
    let mut total: u64 = sessions.iter().map(held).sum();
    if total <= cap {
        return Vec::new();
    }
    let target = cap / 10 * 9;
    let mut candidates: Vec<&SessionInfo> = sessions.iter().filter(|s| held(s) > TRIM_KEEP).collect();
    candidates.sort_by_key(|s| (s.is_running(), !s.clients.is_empty(), s.last_output_ms.max(s.last_input_ms)));
    let mut ids = Vec::new();
    for s in candidates {
        if total <= target {
            break;
        }
        total -= held(s) - TRIM_KEEP;
        ids.push(s.id.clone());
    }
    ids
}

/// Checks the total every few seconds and trims sessions while it's over the cap.
pub async fn run(registry: Arc<Registry>) {
    let cap = total_cap();
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        for id in to_trim(&registry.list(), cap) {
            match registry.trim(&id, TRIM_KEEP).await {
                Ok(()) => tracing::info!(id, "trimmed scrollback: over the {} MB total", cap >> 20),
                Err(e) => tracing::warn!(id, "couldn't trim scrollback: {e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_proto::{ClientDesc, SessionState};

    fn session(id: &str, mb: u64, active_ms: u64, attached: bool, running: bool) -> SessionInfo {
        let mut s: SessionInfo = blank(id);
        s.buffer_end = mb << 20;
        s.last_output_ms = active_ms;
        if attached {
            s.clients.push(ClientDesc { client_id: "c".into(), name: "app".into(), readonly: false });
        }
        if !running {
            s.state = SessionState::Exited { code: Some(0), signal: None, at_ms: 0 };
        }
        s
    }

    fn blank(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: None,
            title: String::new(),
            cwd: None,
            command: Vec::new(),
            pid: 1,
            foreground: None,
            state: SessionState::Running,
            created_ms: 0,
            last_output_ms: 0,
            last_input_ms: 0,
            size: Default::default(),
            clients: Vec::new(),
            ui: Default::default(),
            buffer_start: 0,
            buffer_end: 0,
            alt_screen: false,
        }
    }

    #[test]
    fn nothing_trimmed_under_the_cap() {
        let s = [session("a", 10, 1, false, true), session("b", 10, 2, false, true)];
        assert!(to_trim(&s, 20 << 20).is_empty());
    }

    #[test]
    fn idlest_trimmed_first_until_under_the_cap() {
        let s = [
            session("busy", 16, 900, true, true),
            session("detached-old", 16, 100, false, true),
            session("attached-old", 16, 50, true, true),
            session("exited", 16, 800, false, false),
            session("detached-new", 16, 700, false, true),
        ];
        // 80 MB held; a 60 MB cap trims down to at most 54 MB.
        assert_eq!(to_trim(&s, 60 << 20), vec!["exited", "detached-old"]);
        // A tight cap goes on to attached sessions, least active first.
        assert_eq!(to_trim(&s, 10 << 20), vec!["exited", "detached-old", "detached-new", "attached-old", "busy"]);
    }
}
