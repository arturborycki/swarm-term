#!/usr/bin/env bash
# Builds the site and uploads it to Cloudflare Pages (project "swarm-term"),
# straight from this checkout: no GitHub access needed, so it works while the
# repository is private. Needs `npx wrangler login` once.
#
# Usage: site/scripts/deploy.sh            (production: swarm-term.com)
#        site/scripts/deploy.sh preview    (a preview URL only)
set -euo pipefail
SITE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT="${SWARM_PAGES_PROJECT:-swarm-term}"
cd "$SITE"
npm run build
case "${1:-production}" in
    production) BRANCH=main ;;
    preview) BRANCH=preview ;;
    *) echo "usage: $0 [preview]" >&2; exit 2 ;;
esac
npx wrangler pages deploy dist --project-name "$PROJECT" --branch "$BRANCH" --commit-dirty=true
