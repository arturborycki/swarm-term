#!/usr/bin/env bash
# Publishes the release for the version in site/src/site.ts: creates the GitHub release
# v<version> with the DMG and the Linux archives (the site's download links redirect there),
# then deploys the site, whose update feed offers the new version to installed copies.
#
# Before running it:
#   1. Bump the version (Cargo.toml, app/app/Cargo.toml, site/src/site.ts, the cask) and add
#      the changelog entry.
#   2. scripts/build-linux.sh, then scripts/bundle-app.sh --universal and scripts/make-dmg.sh
#      with SWARM_SIGN_IDENTITY and SWARM_NOTARY_PROFILE (see its header).
#   3. scripts/make-linux-tarballs.sh and scripts/sign-update.sh, which record the checksums
#      and the update signature in site/src/site.ts.
#   4. Write dist/RELEASE_NOTES-<version>.md. Merge the changes through pull requests in
#      swarm-term and app/ (swarm-app), then tag v<version> on each and push the tags.
#
# Afterwards, scripts/update-homebrew.sh updates packaging/homebrew/.
#
# Usage: scripts/publish-release.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SITE_TS="$ROOT/site/src/site.ts"
REPO=arturborycki/swarm-term
VERSION="$(sed -n "s/^export const VERSION = '\(.*\)';/\1/p" "$SITE_TS")"
TAG="v$VERSION"
DMG="$ROOT/dist/Swarm-$VERSION.dmg"
NOTES="$ROOT/dist/RELEASE_NOTES-$VERSION.md"
fail() { echo "error: $*" >&2; exit 1; }
recorded() { sed -n "s/^export const $1 = '\(.*\)';/\1/p" "$SITE_TS"; }
sha() { shasum -a 256 "$1" | cut -d' ' -f1; }

echo "Checking Swarm $VERSION"
for f in "$DMG" "$NOTES"; do [ -f "$f" ] || fail "missing $f"; done
spctl --assess --type open --context context:primary-signature "$DMG" 2>/dev/null ||
    fail "$DMG isn't notarized (scripts/make-dmg.sh with SWARM_SIGN_IDENTITY and SWARM_NOTARY_PROFILE)"
[ "$(sha "$DMG")" = "$(recorded DMG_SHA256)" ] ||
    fail "site.ts's DMG_SHA256 isn't this DMG's: run scripts/sign-update.sh, commit and push"
ASSETS=("$DMG")
for arch in x86_64 aarch64; do
    tarball="$ROOT/dist/swarmd-$VERSION-linux-$arch.tar.gz"
    [ -f "$tarball" ] || fail "missing $tarball (scripts/make-linux-tarballs.sh)"
    const="LINUX_SHA256_$(echo "$arch" | tr '[:lower:]' '[:upper:]')"
    [ "$(sha "$tarball")" = "$(recorded "$const")" ] ||
        fail "site.ts's $const isn't this archive's: run scripts/make-linux-tarballs.sh, commit and push"
    ASSETS+=("$tarball")
done
git -C "$ROOT" diff --quiet HEAD -- site || fail "site/ has uncommitted changes: commit and push them first"
# The tag must exist on GitHub, be on the merged branch and hold this version.
check_tag() { # <repo dir> <branch> <file with the version> <name>
    git -C "$1" fetch -q origin "refs/heads/$2:refs/remotes/origin/$2" "+refs/tags/$TAG:refs/tags/$TAG" 2>/dev/null ||
        fail "tag $TAG isn't pushed to $4"
    git -C "$1" merge-base --is-ancestor "$TAG^{commit}" "origin/$2" ||
        fail "$4's tag $TAG isn't on its $2 branch: merge the pull request, then move the tag"
    git -C "$1" show "$TAG:$3" | grep -q "^version = \"$VERSION\"$" ||
        fail "$4's tag $TAG isn't at version $VERSION: merge the pull request, then move the tag"
}
check_tag "$ROOT" main Cargo.toml swarm-term
check_tag "$ROOT/app" swarm app/Cargo.toml swarm-app
gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 && fail "release $TAG already exists"

echo "Creating the GitHub release $TAG"
gh release create "$TAG" "${ASSETS[@]}" --repo "$REPO" --verify-tag --latest \
    --title "Swarm $VERSION" --notes-file "$NOTES"

echo "Deploying swarm-term.com"
"$ROOT/site/scripts/deploy.sh"

echo "Checking the download links"
for name in "Swarm-$VERSION.dmg" "swarmd-$VERSION-linux-x86_64.tar.gz" "swarmd-$VERSION-linux-aarch64.tar.gz"; do
    # HEAD isn't counted as a download by the site.
    code="$(curl -sIL -o /dev/null -w '%{http_code}' "https://swarm-term.com/download/$name")"
    [ "$code" = 200 ] || fail "https://swarm-term.com/download/$name answered $code"
    echo "  ok  $name"
done
echo "Published: https://github.com/$REPO/releases/tag/$TAG"
