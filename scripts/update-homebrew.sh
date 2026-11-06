#!/usr/bin/env bash
# Sets the version and checksums in packaging/homebrew/ for the release in
# site/src/site.ts. Run it after scripts/publish-release.sh, because the formula's
# checksum is of the source archive GitHub makes from the pushed tag. Commit the result
# through a pull request.
#
# Once the cask and formula are in Homebrew's own repositories, use
# `brew bump-cask-pr swarm` and `brew bump-formula-pr swarmd` instead.
#
# Usage: scripts/update-homebrew.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SITE_TS="$ROOT/site/src/site.ts"
VERSION="$(sed -n "s/^export const VERSION = '\(.*\)';/\1/p" "$SITE_TS")"
DMG_SHA="$(sed -n "s/^export const DMG_SHA256 = '\(.*\)';/\1/p" "$SITE_TS")"
ARCHIVE="https://github.com/arturborycki/swarm-term/archive/refs/tags/v$VERSION.tar.gz"
ARCHIVE_SHA="$(curl -fsSL "$ARCHIVE" | shasum -a 256 | cut -d' ' -f1)"

sed -i '' -e "s/^  version \".*\"$/  version \"$VERSION\"/" \
    -e "s/^  sha256 \".*\"$/  sha256 \"$DMG_SHA\"/" "$ROOT/packaging/homebrew/swarm.rb"
sed -i '' -e "s|^  url \".*\"$|  url \"$ARCHIVE\"|" \
    -e "s/^  sha256 \".*\"$/  sha256 \"$ARCHIVE_SHA\"/" "$ROOT/packaging/homebrew/swarmd.rb"
echo "cask:    $VERSION  $DMG_SHA"
echo "formula: $VERSION  $ARCHIVE_SHA"
