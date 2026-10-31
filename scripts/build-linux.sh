#!/usr/bin/env bash
# Builds static (musl) swarmd, swarm-shim and swarmctl for Linux aarch64 and
# x86_64 into target/linux/<platform>/, where scripts/bundle-app.sh picks them
# up so the app can install swarmd on Linux hosts.
#
# On Linux it builds here; on macOS it builds in an OrbStack Linux machine
# (SWARM_LINUX_MACHINE, default "swarm-test"), which sees this checkout at the
# same path. Needs rustup in that machine.
#
# Usage: scripts/build-linux.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

build() {
    set -euo pipefail
    cd "$1"
    export PATH="$HOME/.cargo/bin:$PATH"
    rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl >/dev/null 2>&1
    # Pure Rust with Rust's own musl, so rust-lld can link either CPU.
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    for arch in aarch64 x86_64; do
        CARGO_TARGET_DIR="$HOME/swarm-target-linux" \
            cargo build -q --release --target "$arch-unknown-linux-musl" -p swarmd -p swarm-shim -p swarmctl
        out="$1/target/linux/linux-$arch"
        mkdir -p "$out"
        for bin in swarmd swarm-shim swarmctl; do
            cp "$HOME/swarm-target-linux/$arch-unknown-linux-musl/release/$bin" "$out/$bin"
        done
        echo "$out"
    done
}

if [ "$(uname -s)" = Linux ]; then
    build "$ROOT"
else
    orb -m "${SWARM_LINUX_MACHINE:-swarm-test}" bash -c "$(declare -f build); build '$ROOT'"
fi
