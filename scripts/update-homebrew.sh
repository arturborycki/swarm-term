#!/usr/bin/env bash
# Sets the version and checksums in packaging/homebrew/ for the release in
# site/src/site.ts, then copies them to the tap, arturborycki/homebrew-swarm, and pushes
# it, so `brew install --cask arturborycki/swarm/swarm` gets the release. Run it after
# scripts/publish-release.sh, because the formula's checksum is of the source archive
# GitHub makes from the pushed tag. Commit the changes here through a pull request.
#
# --no-tap updates only the files here; --tap-only copies them to the tap as they are.
#
# Once the cask and formula are in Homebrew's own repositories, use
# `brew bump-cask-pr swarm` and `brew bump-formula-pr swarmd` instead.
#
# Usage: scripts/update-homebrew.sh [--no-tap | --tap-only]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAP_REPO="https://github.com/arturborycki/homebrew-swarm.git"
MODE="${1:-}"

# Copies the cask and formula to the tap without their leading comment, and pushes.
sync_tap() {
    local version="$1" dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN
    git clone -q --depth 1 "$TAP_REPO" "$dir"
    mkdir -p "$dir/Casks" "$dir/Formula"
    awk 'started || !/^#/ {started=1; print}' "$ROOT/packaging/homebrew/swarm.rb" >"$dir/Casks/swarm.rb"
    awk 'started || !/^#/ {started=1; print}' "$ROOT/packaging/homebrew/swarmd.rb" >"$dir/Formula/swarmd.rb"
    if git -C "$dir" diff --quiet; then
        echo "tap:     already up to date"
        return
    fi
    git -C "$dir" commit -q -am "Swarm $version"
    git -C "$dir" push -q origin HEAD:main
    echo "tap:     pushed Swarm $version"
}

SITE_TS="$ROOT/site/src/site.ts"
VERSION="$(sed -n "s/^export const VERSION = '\(.*\)';/\1/p" "$SITE_TS")"
DMG_SHA="$(sed -n "s/^export const DMG_SHA256 = '\(.*\)';/\1/p" "$SITE_TS")"
if [ "$MODE" = "--tap-only" ]; then
    sync_tap "$VERSION"
    exit 0
fi
ARCHIVE="https://github.com/arturborycki/swarm-term/archive/refs/tags/v$VERSION.tar.gz"
ARCHIVE_SHA="$(curl -fsSL "$ARCHIVE" | shasum -a 256 | cut -d' ' -f1)"

sed -i '' -e "s/^  version \".*\"$/  version \"$VERSION\"/" \
    -e "s/^  sha256 \".*\"$/  sha256 \"$DMG_SHA\"/" "$ROOT/packaging/homebrew/swarm.rb"
sed -i '' -e "s|^  url \".*\"$|  url \"$ARCHIVE\"|" \
    -e "s/^  sha256 \".*\"$/  sha256 \"$ARCHIVE_SHA\"/" "$ROOT/packaging/homebrew/swarmd.rb"
echo "cask:    $VERSION  $DMG_SHA"
echo "formula: $VERSION  $ARCHIVE_SHA"
[ "$MODE" = "--no-tap" ] || sync_tap "$VERSION"
