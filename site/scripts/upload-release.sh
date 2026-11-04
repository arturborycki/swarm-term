#!/usr/bin/env bash
# Uploads a release DMG to the R2 bucket served at downloads.swarm-term.com, so
# the site's Download buttons work while the GitHub repository is private.
#
# One-time setup (needs `npx wrangler login` first):
#   npx wrangler r2 bucket create swarm-downloads
#   then in the Cloudflare dashboard: R2 → swarm-downloads → Settings →
#   Custom Domains → Connect Domain → downloads.swarm-term.com
#
# Usage: site/scripts/upload-release.sh [path/to/Swarm-<version>.dmg]
#        (default: the DMG for the version in site/src/site.ts, from dist/)
# Also uploads that version's Linux archives from dist/ (scripts/make-linux-tarballs.sh).
set -euo pipefail
SITE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(cd "$SITE/.." && pwd)"
BUCKET="${SWARM_DOWNLOADS_BUCKET:-swarm-downloads}"
VERSION="$(sed -n "s/^export const VERSION = '\(.*\)';/\1/p" "$SITE/src/site.ts")"
DMG="${1:-$ROOT/dist/Swarm-$VERSION.dmg}"
[ -f "$DMG" ] || { echo "no DMG at $DMG (scripts/make-dmg.sh builds it)" >&2; exit 1; }
NAME="$(basename "$DMG")"

cd "$SITE"
npx wrangler r2 object put "$BUCKET/$NAME" --file "$DMG" --remote \
    --content-type application/x-apple-diskimage \
    --content-disposition "attachment; filename=\"$NAME\""
echo "https://downloads.swarm-term.com/$NAME"
echo "sha256 $(shasum -a 256 "$DMG" | cut -d' ' -f1)"
for arch in x86_64 aarch64; do
    TARBALL="$ROOT/dist/swarmd-$VERSION-linux-$arch.tar.gz"
    [ -f "$TARBALL" ] || { echo "no $TARBALL (scripts/make-linux-tarballs.sh builds it)" >&2; exit 1; }
    npx wrangler r2 object put "$BUCKET/$(basename "$TARBALL")" --file "$TARBALL" --remote \
        --content-type application/gzip \
        --content-disposition "attachment; filename=\"$(basename "$TARBALL")\""
    echo "https://downloads.swarm-term.com/$(basename "$TARBALL")"
done
