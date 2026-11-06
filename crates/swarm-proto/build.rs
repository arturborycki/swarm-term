//! Sets `SWARM_BUILD_ID`: a hash of the swarm crates' sources, so a daemon and
//! an app built from the same sources agree on it, on any machine or CPU, and
//! an app can tell when a running daemon is from another build.

use std::path::{Path, PathBuf};

const CRATES: [&str; 5] = ["swarm-proto", "swarm-client", "swarm-shim", "swarmd", "swarmctl"];

fn main() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let root = crates.parent().unwrap();
    let mut files: Vec<PathBuf> = vec![root.join("Cargo.lock")];
    for name in CRATES {
        let dir = crates.join(name);
        println!("cargo:rerun-if-changed={}", dir.join("src").display());
        println!("cargo:rerun-if-changed={}", dir.join("Cargo.toml").display());
        files.push(dir.join("Cargo.toml"));
        collect(&dir.join("src"), &mut files);
    }
    println!("cargo:rerun-if-changed={}", files[0].display());
    files.sort();
    // FNV-1a: stable across Rust versions, unlike std's hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in &files {
        // Relative, so the ID doesn't depend on where the checkout is.
        let rel = file.strip_prefix(root).unwrap_or(file).to_string_lossy().replace('\\', "/");
        for byte in rel.bytes().chain([0]).chain(std::fs::read(file).unwrap_or_default()).chain([0]) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=SWARM_BUILD_ID={hash:016x}");
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}
