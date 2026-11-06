#!/usr/bin/env bash
# Prints download and usage totals from the site's D1 database (worker/index.ts
# counts them), and the release-news list. Needs `npx wrangler login`.
#
# Usage: site/scripts/stats.sh [days]        totals for the last <days> days (default 30)
#        site/scripts/stats.sh subscribers   the release-news list as CSV
set -euo pipefail
SITE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$SITE"
# Prints the query's JSON. Retries once, because the first call after a Cloudflare
# login expires can fail while wrangler refreshes it.
d1() {
    local out attempt
    for attempt in 1 2; do
        out="$(npx wrangler d1 execute swarm-site --remote --json --command "$1" 2>&1)" || true
        if printf '%s' "$out" | node -e 'const r=JSON.parse(require("fs").readFileSync(0,"utf8"));
                 process.exit(Array.isArray(r)&&r[0]&&r[0].results?0:1)' 2>/dev/null; then
            printf '%s' "$out"
            return
        fi
    done
    echo "couldn't read the database (is \`npx wrangler login\` current?):" >&2
    printf '%s\n' "$out" >&2
    return 1
}

query() {
    local out
    out="$(d1 "$1")" || exit 1
    printf '%s' "$out" |
        node -e 'const r=JSON.parse(require("fs").readFileSync(0,"utf8"))[0].results;
                 if(!r.length){console.log("  (none)");process.exit()}
                 const k=Object.keys(r[0]);console.log("  "+k.join("\t"));
                 for(const x of r)console.log("  "+k.map(c=>x[c]).join("\t"))'
}

if [ "${1:-}" = subscribers ]; then
    out="$(d1 "SELECT email, created_at, source FROM subscribers WHERE unsubscribed_at IS NULL ORDER BY created_at")" || exit 1
    printf '%s' "$out" |
        node -e 'const r=JSON.parse(require("fs").readFileSync(0,"utf8"))[0].results;
                 console.log("email,subscribed,source");for(const x of r)console.log([x.email,x.created_at,x.source].join(","))'
    exit
fi

DAYS="${1:-30}"
case "$DAYS" in *[!0-9]* | "") echo "usage: $0 [days | subscribers]" >&2; exit 2 ;; esac
SINCE="date('now', '-$DAYS days')"
echo "Downloads from the site, last $DAYS days"
query "SELECT version, SUM(count) AS downloads FROM stats WHERE kind = 'download' AND day >= $SINCE GROUP BY version ORDER BY version DESC"
echo "Downloads by day"
query "SELECT day, SUM(count) AS downloads FROM stats WHERE kind = 'download' AND day >= $SINCE GROUP BY day ORDER BY day DESC"
echo "Downloads by country"
query "SELECT country, SUM(count) AS downloads FROM stats WHERE kind = 'download' AND day >= $SINCE GROUP BY country ORDER BY downloads DESC LIMIT 15"
echo "Linux daemon downloads (servers set up by hand; the app installs swarmd itself otherwise)"
query "SELECT version, SUM(count) AS downloads FROM stats WHERE kind = 'linux' AND day >= $SINCE GROUP BY version ORDER BY version DESC"
echo "Updates installed from inside Swarm"
query "SELECT version, SUM(count) AS updates FROM stats WHERE kind = 'update' AND day >= $SINCE GROUP BY version ORDER BY version DESC"
echo "Update checks by day (each running Swarm checks at launch and daily: roughly active copies)"
query "SELECT day, SUM(count) AS checks FROM stats WHERE kind = 'check' AND day >= $SINCE GROUP BY day ORDER BY day DESC LIMIT 14"
echo "Release-news subscribers"
query "SELECT COUNT(*) AS subscribers FROM subscribers WHERE unsubscribed_at IS NULL"
