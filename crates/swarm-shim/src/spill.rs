//! Spill-to-disk (opt-in): output that leaves the in-memory scrollback is
//! kept on disk instead of being dropped, so `swarmctl dump` can still show
//! it. Terminal output often holds secrets, so it's encrypted
//! (ChaCha20-Poly1305) with a key that exists only in this shim's memory, and
//! a handed-over shim program's. The files die with the session: they're
//! deleted when it's removed, and any left by a crash can't be read by
//! anyone and are cleared by `swarmd`.
//!
//! Output is collected in memory into 1 MiB chunks; a background thread
//! encrypts each full chunk into `<dir>/<start offset>.chunk` and drops the
//! oldest files beyond the size limit.

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use swarm_proto::{paths, SpillConfig};

/// Bytes of output per chunk file.
pub const CHUNK: usize = 1024 * 1024;

pub struct Spill {
    dir: PathBuf,
    key: [u8; 32],
    limit: u64,
    state: Mutex<State>,
    cv: Condvar,
}

#[derive(Default)]
struct State {
    /// Chunk files written, oldest first: (start offset, length).
    written: VecDeque<(u64, u64)>,
    /// Full chunks waiting to be written, oldest first.
    queued: VecDeque<(u64, Vec<u8>)>,
    /// The chunk being filled: its start offset and bytes.
    open_start: u64,
    open: Vec<u8>,
    closed: bool,
}

/// Where a piece of spilled output is: a chunk file, or still in memory.
pub enum Piece {
    File { start: u64, len: u64 },
    Memory { start: u64, bytes: Vec<u8> },
}

/// A spill handed to the next shim program on an in-place update.
#[derive(Serialize, Deserialize)]
pub struct SpillHandoff {
    dir: PathBuf,
    key: [u8; 32],
    limit: u64,
    written: Vec<(u64, u64)>,
    queued: Vec<(u64, Vec<u8>)>,
    open_start: u64,
    open: Vec<u8>,
}

impl Spill {
    /// Starts spilling session `id`'s output under `cfg.dir`, with a new key.
    pub fn create(cfg: &SpillConfig, id: &str) -> Result<Arc<Spill>> {
        let dir = Path::new(&cfg.dir).join(id);
        paths::ensure_private_dir(Path::new(&cfg.dir))?;
        // Files from an earlier spill of this session can't be read without
        // its key: start clean.
        let _ = std::fs::remove_dir_all(&dir);
        paths::ensure_private_dir(&dir)?;
        let spill = Arc::new(Spill {
            dir,
            key: random_key()?,
            limit: cfg.limit.max(CHUNK as u64),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
        });
        start_writer(&spill)?;
        Ok(spill)
    }

    pub fn export(&self) -> SpillHandoff {
        let st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        SpillHandoff {
            dir: self.dir.clone(),
            key: self.key,
            limit: self.limit,
            written: st.written.iter().copied().collect(),
            queued: st.queued.iter().cloned().collect(),
            open_start: st.open_start,
            open: st.open.clone(),
        }
    }

    /// Carries on a spill handed over by the previous shim program.
    pub fn import(h: SpillHandoff) -> Result<Arc<Spill>> {
        let spill = Arc::new(Spill {
            dir: h.dir,
            key: h.key,
            limit: h.limit,
            state: Mutex::new(State {
                written: h.written.into(),
                queued: h.queued.into(),
                open_start: h.open_start,
                open: h.open,
                closed: false,
            }),
            cv: Condvar::new(),
        });
        start_writer(&spill)?;
        Ok(spill)
    }

    /// Keeps `bytes`, which start at absolute offset `start`.
    pub fn append(&self, start: u64, bytes: &[u8]) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if st.closed || bytes.is_empty() {
            return;
        }
        if st.open.is_empty() {
            st.open_start = start;
        } else if st.open_start + st.open.len() as u64 != start {
            // Not contiguous (shouldn't happen): close the chunk so offsets
            // stay right.
            let chunk = (st.open_start, std::mem::take(&mut st.open));
            st.queued.push_back(chunk);
            st.open_start = start;
        }
        st.open.extend_from_slice(bytes);
        while st.open.len() >= CHUNK {
            let rest = st.open.split_off(CHUNK);
            let full = std::mem::replace(&mut st.open, rest);
            let full_start = st.open_start;
            st.open_start += CHUNK as u64;
            st.queued.push_back((full_start, full));
        }
        self.cv.notify_all();
    }

    /// The pieces of output before offset `until`, oldest first.
    pub fn pieces(&self, until: u64) -> Vec<Piece> {
        let st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<Piece> = st.written.iter().map(|&(start, len)| Piece::File { start, len }).collect();
        out.extend(st.queued.iter().map(|(start, bytes)| Piece::Memory { start: *start, bytes: bytes.clone() }));
        if !st.open.is_empty() {
            out.push(Piece::Memory { start: st.open_start, bytes: st.open.clone() });
        }
        out.retain(|p| p.start() < until);
        out
    }

    /// The bytes of `piece` (decrypted), cut off at `until`. `None` if its
    /// file is gone (dropped for the size limit since) or unreadable.
    pub fn read(&self, piece: &Piece, until: u64) -> Option<(u64, Vec<u8>)> {
        let (start, mut bytes) = match piece {
            Piece::Memory { start, bytes } => (*start, bytes.clone()),
            Piece::File { start, len } => {
                let mut sealed = Vec::new();
                std::fs::File::open(self.chunk_path(*start)).ok()?.read_to_end(&mut sealed).ok()?;
                let bytes = self.cipher().decrypt(&nonce(*start), Payload { msg: &sealed, aad: &aad(*start) }).ok()?;
                if bytes.len() as u64 != *len {
                    return None;
                }
                (*start, bytes)
            }
        };
        bytes.truncate(until.saturating_sub(start) as usize);
        Some((start, bytes))
    }

    /// Stops spilling and deletes everything spilled.
    pub fn destroy(&self) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.closed = true;
        st.written.clear();
        st.queued.clear();
        st.open.clear();
        self.cv.notify_all();
        drop(st);
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(Key::from_slice(&self.key))
    }

    fn chunk_path(&self, start: u64) -> PathBuf {
        self.dir.join(format!("{start:020}.chunk"))
    }

    /// Encrypts and writes one chunk (atomically: temp file, then rename).
    fn write_chunk(&self, start: u64, bytes: &[u8]) -> Result<()> {
        let sealed = self
            .cipher()
            .encrypt(&nonce(start), Payload { msg: bytes, aad: &aad(start) })
            .map_err(|_| anyhow::anyhow!("encrypt"))?;
        let path = self.chunk_path(start);
        let tmp = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        std::io::Write::write_all(&mut file, &sealed)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

impl Piece {
    pub fn start(&self) -> u64 {
        match self {
            Piece::File { start, .. } | Piece::Memory { start, .. } => *start,
        }
    }
}

fn start_writer(spill: &Arc<Spill>) -> Result<()> {
    let s = spill.clone();
    std::thread::Builder::new().name("spill-writer".into()).spawn(move || loop {
        let (start, bytes) = {
            let mut st = s.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while st.queued.is_empty() && !st.closed {
                st = s.cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if st.closed {
                return;
            }
            st.queued.front().cloned().unwrap()
        };
        let written = s.write_chunk(start, &bytes);
        let mut st = s.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if st.closed {
            return;
        }
        // The chunk stays queued (and readable from memory) until its file
        // is complete.
        st.queued.pop_front();
        match written {
            Ok(()) => st.written.push_back((start, bytes.len() as u64)),
            Err(e) => tracing::warn!("couldn't spill output to disk: {e:#}"),
        }
        let mut total: u64 = st.written.iter().map(|&(_, len)| len).sum();
        while total > s.limit {
            let Some((old, len)) = st.written.pop_front() else { break };
            let _ = std::fs::remove_file(s.chunk_path(old));
            total -= len;
        }
    })?;
    Ok(())
}

/// Offsets are never reused within a session, so the chunk's start offset
/// makes a unique nonce for its key.
fn nonce(start: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&start.to_le_bytes());
    *Nonce::from_slice(&n)
}

/// Binds a chunk's ciphertext to its place, so files can't be swapped around.
fn aad(start: u64) -> [u8; 8] {
    start.to_le_bytes()
}

fn random_key() -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    if key.iter().all(|&b| b == 0) {
        bail!("no randomness for the spill key");
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, limit: u64) -> SpillConfig {
        SpillConfig { dir: dir.display().to_string(), limit }
    }

    fn read_all(spill: &Spill, until: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for piece in spill.pieces(until) {
            if let Some((_, bytes)) = spill.read(&piece, until) {
                out.extend(bytes);
            }
        }
        out
    }

    fn wait_written(spill: &Spill, n: usize) {
        for _ in 0..500 {
            if spill.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).written.len() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("chunks not written");
    }

    #[test]
    fn spilled_output_is_encrypted_on_disk_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let spill = Spill::create(&cfg(dir.path(), 64 * CHUNK as u64), "s1").unwrap();
        let data: Vec<u8> = (0..(CHUNK * 2 + 1000)).map(|i| b"secret-token "[i % 13]).collect();
        spill.append(0, &data[..300_000]);
        spill.append(300_000, &data[300_000..]);
        wait_written(&spill, 2);
        assert_eq!(read_all(&spill, u64::MAX), data);
        // Partial reads stop at `until`.
        assert_eq!(read_all(&spill, 10), &data[..10]);
        // The files hold no plaintext.
        for entry in std::fs::read_dir(dir.path().join("s1")).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!bytes.windows(12).any(|w| w == b"secret-token"), "plaintext on disk");
        }
        spill.destroy();
        assert!(!dir.path().join("s1").exists());
    }

    #[test]
    fn the_oldest_chunks_go_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let spill = Spill::create(&cfg(dir.path(), 2 * CHUNK as u64), "s2").unwrap();
        let data = vec![7u8; CHUNK * 4];
        spill.append(0, &data);
        for _ in 0..1000 {
            if spill.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).queued.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let pieces = spill.pieces(u64::MAX);
        assert_eq!(pieces.first().map(Piece::start), Some(2 * CHUNK as u64));
        assert_eq!(std::fs::read_dir(dir.path().join("s2")).unwrap().count(), 2);
    }

    #[test]
    fn a_handed_over_spill_still_reads_and_keeps_writing() {
        let dir = tempfile::tempdir().unwrap();
        let spill = Spill::create(&cfg(dir.path(), 64 * CHUNK as u64), "s3").unwrap();
        let data: Vec<u8> = (0..CHUNK * 3).map(|i| (i % 251) as u8).collect();
        spill.append(0, &data[..CHUNK + 5]);
        let handed = postcard::to_stdvec(&spill.export()).unwrap();
        // The old program is gone (its writer thread with it).
        spill.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).closed = true;
        spill.cv.notify_all();
        let next = Spill::import(postcard::from_bytes(&handed).unwrap()).unwrap();
        next.append(CHUNK as u64 + 5, &data[CHUNK + 5..]);
        wait_written(&next, 3);
        assert_eq!(read_all(&next, u64::MAX), data);
    }

    #[test]
    fn a_file_from_another_key_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let a = Spill::create(&cfg(dir.path(), 64 * CHUNK as u64), "s4").unwrap();
        a.append(0, &vec![1u8; CHUNK]);
        wait_written(&a, 1);
        let mut h = a.export();
        h.key = [9u8; 32];
        h.queued.clear();
        let b = Spill::import(h).unwrap();
        assert!(read_all(&b, u64::MAX).is_empty());
    }
}
