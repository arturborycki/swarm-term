#!/usr/bin/env bash
# Signs a release DMG for in-app updates and records its SHA-256 and signature
# in site/src/site.ts, which the site's update feed (/releases/latest.json)
# serves. The app installs an update only if the Ed25519 signature over
# "swarm-update:<version>:<sha256>" verifies with the public key built into it
# (app/app/src/swarm/updates.rs), so whoever controls the download host can't
# substitute a different app.
#
# The private key stays on the release Mac, outside the repositories:
#   $SWARM_UPDATE_KEY, default ~/.config/swarm-term/update-signing-key.pem
# Losing it means shipping a new public key through a manual download, so keep
# a backup (e.g. in a password manager). To make one:
#   (umask 077; openssl genpkey -algorithm ed25519 -out <key.pem>)
#   openssl pkey -in <key.pem> -pubout -outform DER | tail -c 32 | base64
#                                              (the app's UPDATE_PUBLIC_KEY)
#
# Usage: scripts/sign-update.sh [path/to/Swarm-<version>.dmg]
#        (default: the DMG for the version in site/src/site.ts, from dist/)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SITE_TS="$ROOT/site/src/site.ts"
KEY="${SWARM_UPDATE_KEY:-$HOME/.config/swarm-term/update-signing-key.pem}"
VERSION="$(sed -n "s/^export const VERSION = '\(.*\)';/\1/p" "$SITE_TS")"
DMG="${1:-$ROOT/dist/Swarm-$VERSION.dmg}"
[ -f "$DMG" ] || { echo "no DMG at $DMG (scripts/make-dmg.sh builds it)" >&2; exit 1; }
[ -f "$KEY" ] || { echo "no signing key at $KEY" >&2; exit 1; }
[ "$(basename "$DMG")" = "Swarm-$VERSION.dmg" ] \
    || { echo "$DMG isn't Swarm-$VERSION.dmg (site/src/site.ts says $VERSION)" >&2; exit 1; }

SHA256="$(shasum -a 256 "$DMG" | cut -d' ' -f1)"
MESSAGE="$(mktemp)"
trap 'rm -f "$MESSAGE"' EXIT
printf 'swarm-update:%s:%s' "$VERSION" "$SHA256" > "$MESSAGE"
SIGNATURE="$(openssl pkeyutl -sign -rawin -inkey "$KEY" -in "$MESSAGE" | base64)"

sed -i '' \
    -e "s|^export const DMG_SHA256 = .*|export const DMG_SHA256 = '$SHA256';|" \
    -e "s|^export const DMG_SIGNATURE = .*|export const DMG_SIGNATURE = '$SIGNATURE';|" \
    "$SITE_TS"
grep -q "^export const DMG_SIGNATURE = '$SIGNATURE';" "$SITE_TS" \
    || { echo "couldn't record the signature in $SITE_TS" >&2; exit 1; }
echo "Swarm $VERSION"
echo "sha256    $SHA256"
echo "signature $SIGNATURE"
