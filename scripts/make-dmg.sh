#!/usr/bin/env bash
# Packages a release Swarm.app as dist/Swarm-<version>.dmg (the app plus an
# /Applications link) and prints its path and SHA-256 (for the Homebrew cask).
#
# Signing and notarization need your Apple Developer credentials and only run
# when they are given:
#   SWARM_SIGN_IDENTITY   "Developer ID Application: Name (TEAMID)": signs the
#                         app (hardened runtime) and the DMG.
#   SWARM_NOTARY_PROFILE  a `xcrun notarytool store-credentials` keychain
#                         profile: notarizes the DMG and staples the ticket.
# Without them the DMG holds the ad-hoc-signed app, which runs on this Mac but
# that Gatekeeper rejects elsewhere.
#
# Usage: scripts/make-dmg.sh [path/to/Swarm.app]   (default: release bundle)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${1:-$ROOT/app/target/release/bundle/osx/Swarm.app}"
[ -d "$APP" ] || { echo "no app at $APP (run scripts/bundle-app.sh --release)" >&2; exit 1; }
VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
DIST="$ROOT/dist"
DMG="$DIST/Swarm-$VERSION.dmg"
mkdir -p "$DIST"

if [ -n "${SWARM_SIGN_IDENTITY:-}" ]; then
    # Inner binaries first, then the app, with a secure timestamp.
    for bin in "$APP"/Contents/MacOS/*; do
        codesign --force --options runtime --timestamp --sign "$SWARM_SIGN_IDENTITY" "$bin"
    done
    codesign --force --options runtime --timestamp --sign "$SWARM_SIGN_IDENTITY" \
        --entitlements "$ROOT/app/script/Entitlements.plist" "$APP" 2>/dev/null ||
        codesign --force --options runtime --timestamp --sign "$SWARM_SIGN_IDENTITY" "$APP"
    codesign --verify --deep --strict "$APP"
else
    echo "note: SWARM_SIGN_IDENTITY not set; the DMG holds an ad-hoc-signed app" >&2
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -quiet -volname "Swarm $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO "$DMG"

if [ -n "${SWARM_SIGN_IDENTITY:-}" ]; then
    codesign --force --timestamp --sign "$SWARM_SIGN_IDENTITY" "$DMG"
fi
if [ -n "${SWARM_NOTARY_PROFILE:-}" ]; then
    xcrun notarytool submit "$DMG" --keychain-profile "$SWARM_NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
else
    echo "note: SWARM_NOTARY_PROFILE not set; the DMG isn't notarized" >&2
fi

echo "$DMG"
shasum -a 256 "$DMG" | awk '{print "sha256 " $1}'
