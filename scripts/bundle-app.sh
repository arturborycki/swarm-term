#!/usr/bin/env bash
# Builds Swarm.app: the `swarm` binary of the Warp fork in app/, plus swarmd,
# swarm-shim and swarmctl in Contents/MacOS, then signs it (ad-hoc unless an
# "Apple Development" identity is available). Mirrors the steps of
# app/script/macos/run for the OSS channel.
#
# Linux builds from scripts/build-linux.sh (target/linux/<platform>/) go to
# Contents/Resources/swarmd/<platform>/, for installing swarmd on Linux hosts.
#
# Usage: scripts/bundle-app.sh              (debug)
#        scripts/bundle-app.sh --release    (optimized)
#        scripts/bundle-app.sh --universal  (optimized, arm64 + x86_64; needs
#                                           `rustup target add x86_64-apple-darwin
#                                           aarch64-apple-darwin` in both repos)
# Prints the path of the .app on success.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PROFILE=debug
RELEASE_FLAG=""
UNIVERSAL=""
case "${1:-}" in
    --release) PROFILE=release; RELEASE_FLAG="--release" ;;
    --universal) PROFILE=release; RELEASE_FLAG="--release"; UNIVERSAL=1 ;;
    "") ;;
    *) echo "usage: $0 [--release | --universal]" >&2; exit 2 ;;
esac
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
if [ -n "$UNIVERSAL" ]; then
    # Build the other CPU's binaries and merge each pair into one file.
    case "$(uname -m)" in
        arm64) OTHER=x86_64-apple-darwin ;;
        *) OTHER=aarch64-apple-darwin ;;
    esac
    cargo build -q --release --target "$OTHER" -p swarmd -p swarm-shim -p swarmctl
    (cd app/app && cargo build -q --release --target "$OTHER" --bin swarm --features gui)
    for bin in swarmd swarm-shim swarmctl; do
        lipo -create "$APP/Contents/MacOS/$bin" "$ROOT/target/$OTHER/release/$bin" -output "$APP/Contents/MacOS/$bin.universal"
        mv "$APP/Contents/MacOS/$bin.universal" "$APP/Contents/MacOS/$bin"
    done
    # The bundled `swarm` has its framework rpath added; the other one needs it too.
    OTHER_SWARM="$ROOT/app/target/$OTHER/release/swarm"
    (cd app && ./script/macos/add_framework_rpath "$OTHER_SWARM" >/dev/null)
    lipo -create "$APP/Contents/MacOS/swarm" "$OTHER_SWARM" -output "$APP/Contents/MacOS/swarm.universal"
    mv "$APP/Contents/MacOS/swarm.universal" "$APP/Contents/MacOS/swarm"
fi
if [ -d "$ROOT/target/linux" ]; then
    # The app installs and updates swarmd on Linux hosts from these, and won't
    # update a host from a build that isn't its own: a release with stale ones
    # leaves every host on the old daemon.
    WANT="$("$ROOT/target/$PROFILE/swarmd" --version | awk '{print $2}')"
    for dir in "$ROOT"/target/linux/*/; do
        if ! grep -aq "$WANT" "$dir/swarmd"; then
            msg="$dir is from another build than $WANT (run scripts/build-linux.sh)"
            [ "$PROFILE" = release ] && { echo "error: $msg" >&2; exit 1; }
            echo "note: $msg" >&2
        fi
    done
    mkdir -p "$APP/Contents/Resources/swarmd"
    cp -R "$ROOT/target/linux/." "$APP/Contents/Resources/swarmd/"
else
    echo "note: no Linux builds (scripts/build-linux.sh); Linux hosts can't get swarmd from this app" >&2
fi
if [ "$PROFILE" = release ]; then
    # Local symbols are only for debugging; they're a third of `swarm`'s size.
    for bin in swarm swarmd swarm-shim swarmctl; do
        strip -x "$APP/Contents/MacOS/$bin" 2>/dev/null
    done
fi
# A stable identity keeps the permissions macOS grants Swarm (notifications,
# badges) across rebuilds; an ad-hoc signature looks like a new app each time.
SIGNING_CERT="$(security find-identity -p codesigning -v | grep "Apple Development" | awk '{print $2}' | head -1 || true)"
sign() {
    codesign --force --deep --options runtime --sign "$1" "$APP" \
        --entitlements app/script/Debug-Entitlements.plist >/dev/null 2>&1
}
if [ -n "$SIGNING_CERT" ] && sign "$SIGNING_CERT"; then
    :
else
    [ -n "$SIGNING_CERT" ] && echo "note: signing with $SIGNING_CERT failed (keychain locked?); signing ad-hoc" >&2
    sign -
fi
echo "$APP"
