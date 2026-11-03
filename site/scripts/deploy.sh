#!/usr/bin/env bash
# Builds the site and uploads it to Cloudflare (a Worker serving dist/ as
# static assets, wrangler.jsonc), straight from this checkout: no GitHub access
# needed, so it works while the repository is private. Needs `npx wrangler
# login` once.
#
# Usage: site/scripts/deploy.sh            (live: workers.dev and attached domains)
#        site/scripts/deploy.sh preview    (a preview URL; the live site is unchanged)
set -euo pipefail
SITE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$SITE"
npm run build
case "${1:-production}" in
    production) npx wrangler deploy ;;
    preview) npx wrangler versions upload ;;
    *) echo "usage: $0 [preview]" >&2; exit 2 ;;
esac
