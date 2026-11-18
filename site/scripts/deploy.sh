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
# IndexNow (Bing, Yandex, Seznam and others): tells search engines the
# sitemap's pages changed. The key file is public/<key>.txt.
INDEXNOW_KEY=c1e29cfd153e26f607e21f748878fbed
ping_indexnow() {
    local urls
    urls="$(curl -fsS https://swarm-term.com/sitemap.xml | sed -n 's:.*<loc>\(.*\)</loc>.*:"\1":p' | paste -sd, -)"
    [ -n "$urls" ] || { echo "IndexNow: no URLs in the sitemap" >&2; return 0; }
    curl -fsS -o /dev/null -w 'IndexNow: %{http_code}\n' -X POST https://api.indexnow.org/indexnow \
        -H 'Content-Type: application/json; charset=utf-8' \
        -d "{\"host\":\"swarm-term.com\",\"key\":\"$INDEXNOW_KEY\",\"keyLocation\":\"https://swarm-term.com/$INDEXNOW_KEY.txt\",\"urlList\":[$urls]}" ||
        echo "IndexNow ping failed (the deploy itself succeeded)" >&2
}
npm run build
case "${1:-production}" in
    production) npx wrangler deploy && ping_indexnow ;;
    preview) npx wrangler versions upload ;;
    *) echo "usage: $0 [preview]" >&2; exit 2 ;;
esac
