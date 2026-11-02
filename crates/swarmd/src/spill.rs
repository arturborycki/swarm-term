//! Spill-to-disk setting (opt-in). When on, each session's shim keeps the
//! output that leaves its in-memory buffer in `$STATE_DIR/spill/<id>/`,
//! encrypted with a key only that shim holds, up to a size limit per session.
//! It's on when `$STATE_DIR/spill-to-disk` exists; the file holds the limit in
//! bytes (`swarmd spill on [--limit-mb N]`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use swarm_proto::{paths, SpillConfig};

/// Default limit per session: 256 MiB on top of the 16 MiB kept in memory.
pub const DEFAULT_LIMIT: u64 = 256 * 1024 * 1024;

fn marker_path() -> PathBuf {
    paths::state_dir().join("spill-to-disk")
}

pub fn spill_dir() -> PathBuf {
    paths::state_dir().join("spill")
}

/// The setting: `None` when off.
pub fn config() -> Option<SpillConfig> {
    let text = std::fs::read_to_string(marker_path()).ok()?;
    let limit = text.trim().parse().unwrap_or(DEFAULT_LIMIT);
    Some(SpillConfig { dir: spill_dir().display().to_string(), limit })
}

/// Turns it on with `limit` bytes per session, or off (`None`).
pub fn set(limit: Option<u64>) -> Result<()> {
    let marker = marker_path();
    match limit {
        Some(limit) => {
            std::fs::create_dir_all(paths::state_dir())?;
            std::fs::write(&marker, limit.to_string()).with_context(|| format!("write {}", marker.display()))
        }
        None => match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

pub fn status_line() -> String {
    match config() {
        Some(cfg) => format!("spill to disk: on, up to {} MB per session", cfg.limit / (1024 * 1024)),
        None => "spill to disk: off".to_string(),
    }
}

/// Deletes spilled output of sessions that are gone (a shim that crashed
/// can't clean up; without its key the files are unreadable anyway).
pub fn remove_orphans(live: &[String]) {
    let Ok(entries) = std::fs::read_dir(spill_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !live.contains(&name) {
            tracing::info!(id = %name, "removing spilled output of a session that's gone");
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
