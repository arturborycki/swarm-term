#!/usr/bin/env bash
# Builds Swarm.app: the `swarm` binary of the Warp fork in app/, plus swarmd,
# swarm-shim and swarmctl in Contents/MacOS, then signs it (ad-hoc unless an
# "Apple Development" identity is available). Mirrors the steps of
# app/script/macos/run for the OSS channel.
#
# Usage: scripts/bundle-app.sh            (debug)
# Prints the path of the .app on success.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cargo build -q -p swarmd -p swarmctl

APP="$ROOT/app/target/debug/bundle/osx/Swarm.app"
rm -rf "$APP"
(cd app/app && cargo bundle --bin swarm --features gui >/dev/null)
(cd app && ./script/macos/add_framework_rpath "$APP/Contents/MacOS/swarm" >/dev/null)
(cd app && SKIP_SETTINGS_SCHEMA=1 NO_LICENSES=1 ./script/prepare_bundled_resources "$APP/Contents/Resources" oss >/dev/null)
for bin in swarmd swarm-shim swarmctl; do
    cp "$ROOT/target/debug/$bin" "$APP/Contents/MacOS/$bin"
done
SIGNING_CERT="$(security find-identity -p codesigning -v | grep "Apple Development" | awk '{print $2}' | head -1 || true)"
codesign --force --deep --options runtime --sign "${SIGNING_CERT:--}" "$APP" \
    --entitlements app/script/Debug-Entitlements.plist >/dev/null 2>&1
echo "$APP"
