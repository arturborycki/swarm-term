#!/usr/bin/env bash
# Packs the Linux builds (scripts/build-linux.sh) as dist/swarmd-<version>-linux-<arch>.tar.gz,
# for installing swarmd on a server without the app (docs: "Install on a server yourself"),
# and records their SHA-256 in site/src/site.ts. Each archive holds swarmd, swarm-shim and
# swarmctl at its top level, to unpack into ~/.local/share/swarm/bin.
#
# Usage: scripts/make-linux-tarballs.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SITE_TS="$ROOT/site/src/site.ts"
VERSION="$(sed -n "s/^export const VERSION = '\(.*\)';/\1/p" "$SITE_TS")"
WANT="$(cd "$ROOT" && cargo build -q --release -p swarmd && target/release/swarmd --version | awk '{print $2}')"
case "$WANT" in "$VERSION"+*) ;; *) echo "swarmd is $WANT, site/src/site.ts says $VERSION" >&2; exit 1 ;; esac
mkdir -p "$ROOT/dist"
for arch in x86_64 aarch64; do
    dir="$ROOT/target/linux/linux-$arch"
    grep -aq "$WANT" "$dir/swarmd" || { echo "$dir isn't $WANT (run scripts/build-linux.sh)" >&2; exit 1; }
    out="$ROOT/dist/swarmd-$VERSION-linux-$arch.tar.gz"
    # No macOS metadata (._ files, xattrs) in an archive for Linux.
    COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata --uid 0 --gid 0 -czf "$out" -C "$dir" swarmd swarm-shim swarmctl
    sha="$(shasum -a 256 "$out" | cut -d' ' -f1)"
    const="LINUX_SHA256_$(echo "$arch" | tr '[:lower:]' '[:upper:]')"
    sed -i '' "s|^export const $const = .*|export const $const = '$sha';|" "$SITE_TS"
    grep -q "^export const $const = '$sha';" "$SITE_TS" || { echo "couldn't record $const in $SITE_TS" >&2; exit 1; }
    echo "$out"
    echo "sha256 $sha"
done
