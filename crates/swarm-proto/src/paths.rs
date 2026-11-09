//! Where sockets, state and logs live.
//!
//! The runtime directory must resolve to the same path for GUI and ssh logins
//! of the same user, so on macOS it comes from `confstr`, never `$TMPDIR`.
//! `SWARM_RUNTIME_DIR` / `SWARM_STATE_DIR` override both (used by tests).

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Longest runtime dir that still leaves room for `shims/<ulid>.sock` within
/// macOS's 104-byte `sun_path`.
const MAX_RUNTIME_DIR_LEN: usize = 64;

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SWARM_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    let candidate = platform_runtime_base().map(|b| b.join("swarm"));
    match candidate {
        Some(dir) if dir.as_os_str().len() <= MAX_RUNTIME_DIR_LEN => dir,
        _ => PathBuf::from(format!("/tmp/swarm-{}", uid())),
    }
}

#[cfg(target_os = "macos")]
fn platform_runtime_base() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    let n = unsafe {
        libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr() as *mut libc::c_char, buf.len())
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    let s = String::from_utf8(buf).ok()?;
    Some(PathBuf::from(s.trim_end_matches('/')))
}

#[cfg(not(target_os = "macos"))]
fn platform_runtime_base() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_dir())
}

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SWARM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/swarm")
    } else if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg).join("swarm")
    } else {
        home.join(".local/state/swarm")
    }
}

pub fn log_dir() -> PathBuf {
    if std::env::var_os("SWARM_STATE_DIR").is_none() && cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Logs/swarm");
        }
    }
    state_dir().join("logs")
}

pub fn daemon_socket(runtime: &Path) -> PathBuf {
    runtime.join("swarmd.sock")
}

pub fn daemon_lock(runtime: &Path) -> PathBuf {
    runtime.join("swarmd.lock")
}

pub fn shims_dir(runtime: &Path) -> PathBuf {
    runtime.join("shims")
}

pub fn shim_socket(runtime: &Path, id: &str) -> PathBuf {
    shims_dir(runtime).join(format!("{id}.sock"))
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Closes every file descriptor above stderr. Long-lived processes (the
/// daemon, shims) call this first so they never keep a parent's pipes or
/// sockets open, which would hang whoever waits for EOF on them.
pub fn close_inherited_fds() {
    close_inherited_fds_except(&[]);
}

/// Like `close_inherited_fds`, but keeps `keep`: descriptors handed over on
/// purpose (a shim taking over a session from its previous program).
pub fn close_inherited_fds_except(keep: &[i32]) {
    let dir = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
    let fds: Vec<i32> = match std::fs::read_dir(dir) {
        Ok(entries) => {
            entries.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok()).filter(|&fd| fd > 2 && !keep.contains(&fd)).collect()
        }
        Err(_) => (3..1024).filter(|fd| !keep.contains(fd)).collect(),
    };
    // The directory's own fd is in the list but already closed; close() on
    // it just fails with EBADF.
    for fd in fds {
        unsafe {
            libc::close(fd);
        }
    }
}

/// Like `ensure_private_dir`, but only if `dir`'s parent exists: used to put
/// a removed runtime folder back without creating folders above it (on Linux
/// `/run/user/<uid>` is gone, and can't be made by the user, until they log
/// in again). Fails with `NotFound` while the parent is missing.
pub fn ensure_private_dir_in_place(dir: &Path) -> io::Result<()> {
    if !dir.parent().is_some_and(Path::is_dir) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} has no parent folder", dir.display())));
    }
    ensure_private_dir(dir)
}

/// Creates `dir` (and parents) and makes sure it is a `0700` directory owned
/// by the current user.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", dir.display())));
    }
    if meta.uid() != uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by uid {}, not us", dir.display(), meta.uid()),
        ));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_runtime_dir_fits_socket_paths() {
        if std::env::var_os("SWARM_RUNTIME_DIR").is_some() {
            return;
        }
        let dir = runtime_dir();
        let sock = shim_socket(&dir, "01JC0000000000000000000000");
        assert!(sock.as_os_str().len() < 104, "{}", sock.display());
    }
}
