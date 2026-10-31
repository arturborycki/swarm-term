#!/usr/bin/env bash
# Builds Swarm.app: the `swarm` binary of the Warp fork in app/, plus swarmd,
# swarm-shim and swarmctl in Contents/MacOS, then signs it (ad-hoc unless an
# "Apple Development" identity is available). Mirrors the steps of
# app/script/macos/run for the OSS channel.
#
# Linux builds from scripts/build-linux.sh (target/linux/<platform>/) go to
# Contents/Resources/swarmd/<platform>/, for installing swarmd on Linux hosts.
#
# Usage: scripts/bundle-app.sh            (debug)
#        scripts/bundle-app.sh --release  (optimized)
# Prints the path of the .app on success.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PROFILE=debug
RELEASE_FLAG=""
if [ "${1:-}" = "--release" ]; then
    PROFILE=release
    RELEASE_FLAG="--release"
fi
# shellcheck disable=SC2086 # empty when debug
cargo build -q $RELEASE_FLAG -p swarmd -p swarm-shim -p swarmctl

APP="$ROOT/app/target/$PROFILE/bundle/osx/Swarm.app"
rm -rf "$APP"
# shellcheck disable=SC2086
(cd app/app && cargo bundle $RELEASE_FLAG --bin swarm --features gui >/dev/null)
(cd app && ./script/macos/add_framework_rpath "$APP/Contents/MacOS/swarm" >/dev/null)
(cd app && SKIP_SETTINGS_SCHEMA=1 NO_LICENSES=1 ./script/prepare_bundled_resources "$APP/Contents/Resources" oss >/dev/null)
for bin in swarmd swarm-shim swarmctl; do
    cp "$ROOT/target/$PROFILE/$bin" "$APP/Contents/MacOS/$bin"
done
if [ -d "$ROOT/target/linux" ]; then
    mkdir -p "$APP/Contents/Resources/swarmd"
    cp -R "$ROOT/target/linux/." "$APP/Contents/Resources/swarmd/"
else
    echo "note: no Linux builds (scripts/build-linux.sh); Linux hosts can't get swarmd from this app" >&2
fi
SIGNING_CERT="$(security find-identity -p codesigning -v | grep "Apple Development" | awk '{print $2}' | head -1 || true)"
codesign --force --deep --options runtime --sign "${SIGNING_CERT:--}" "$APP" \
    --entitlements app/script/Debug-Entitlements.plist >/dev/null 2>&1
echo "$APP"
