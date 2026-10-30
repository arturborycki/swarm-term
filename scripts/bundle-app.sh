#!/usr/bin/env bash
# Builds the Swarm app bundle: the Warp fork in app/ plus swarmd, swarm-shim
# and swarmctl inside Contents/MacOS, then re-signs it (ad-hoc unless an
# "Apple Development" identity is available).
#
# Usage: scripts/bundle-app.sh            (debug)
# Prints the path of the .app on success.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cargo build -q -p swarmd -p swarmctl
(cd app && WARP_SKIP_COMMON_SKILLS_INSTALL=1 ./script/run --dont-open >/dev/null)
APP="$ROOT/app/target/debug/bundle/osx/WarpOss.app"
for bin in swarmd swarm-shim swarmctl; do
    cp "$ROOT/target/debug/$bin" "$APP/Contents/MacOS/$bin"
done
SIGNING_CERT="$(security find-identity -p codesigning -v | grep "Apple Development" | awk '{print $2}' | head -1 || true)"
codesign --force --deep --options runtime --sign "${SIGNING_CERT:--}" "$APP" \
    --entitlements app/script/Debug-Entitlements.plist >/dev/null 2>&1
echo "$APP"
