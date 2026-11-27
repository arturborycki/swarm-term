#!/usr/bin/env bash
# Prints the git tree of the sources a build uses, so scripts/publish-release.sh can
# check that the release files were built from what the tag holds.
#
# Usage: scripts/source-trees.sh mac|linux          (the working trees, as they are now)
#        scripts/source-trees.sh mac|linux <tag>    (what <tag> holds in each repo)
# "mac" covers the daemon crates and app/; "linux" the daemon crates only.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KIND="${1:?usage: $0 mac|linux [tag]}"
REV="${2:-}"

# The tree of <dir>'s files as they are, uncommitted changes included, written with
# a scratch index so the real one is left alone.
worktree() { # <repo dir>
    local index
    index="$(mktemp)"
    rm -f "$index"
    cp "$(git -C "$1" rev-parse --absolute-git-dir)/index" "$index" 2>/dev/null || true
    GIT_INDEX_FILE="$index" git -C "$1" add -A . >/dev/null
    GIT_INDEX_FILE="$index" git -C "$1" write-tree
    rm -f "$index"
}

root() { # <repo dir>
    if [ -n "$REV" ]; then git -C "$1" rev-parse "$REV^{tree}"; else worktree "$1"; fi
}

term="$(root "$ROOT")"
for path in Cargo.toml Cargo.lock crates; do
    echo "swarm-term:$path $(git -C "$ROOT" rev-parse "$term:$path")"
done
if [ "$KIND" = mac ]; then
    echo "swarm-app $(root "$ROOT/app")"
fi
