# swarm-term.com

The product site: Astro, static output, no client framework, served by Cloudflare.

```bash
npm install
npm run dev      # http://localhost:4321
npm run build    # writes dist/
```

## Deploy (Cloudflare)

Everything is uploaded from this checkout with `wrangler`, so it works while the GitHub repository is private. Log in once:

```bash
npx wrangler login
```

**Site (Cloudflare Workers static assets).** `scripts/deploy.sh` builds and uploads `dist/` to the Worker `swarm-term` (`wrangler.jsonc`: static files only, no server code; unknown paths get `404.html`). It serves `swarm-term.com` and `www.swarm-term.com` (custom domains in `wrangler.jsonc`; Cloudflare created the DNS records and certificate). `scripts/deploy.sh preview` uploads a version with its own preview URL and leaves the live site alone.

**Downloads (GitHub Releases).** The download links are `swarm-term.com/download/<file>`: `worker/index.ts` counts them, then redirects to that file on the GitHub release `v<version>`. Releasing is attaching the files to the release; `scripts/publish-release.sh` (repository root) checks them, creates the release and deploys the site:

```bash
scripts/sign-update.sh             # signs dist/Swarm-<version>.dmg, records it in src/site.ts
scripts/make-linux-tarballs.sh     # packs the Linux daemon, records its checksums in src/site.ts
scripts/publish-release.sh         # GitHub release with all three files, then deploys the site
```

Releases up to 0.1.3 were also uploaded to the R2 bucket `swarm-downloads` (`downloads.swarm-term.com`), which stays up for links that point there directly, such as 0.1.1's update feed.

**Updates.** `/releases/latest.json` describes the current release in the same shape as GitHub's API, for the app's "Check for Updates…", plus the DMG's `sha256` and Ed25519 `signature` (`DMG_SHA256`, `DMG_SIGNATURE` in `src/site.ts`, written by `scripts/sign-update.sh`). The app installs an update itself only when that signature verifies with the release key built into it, so sign every release before deploying the site. The private key is `~/.config/swarm-term/update-signing-key.pem` on the release Mac; keep a backup.

**Issue reports.** `/report/` is a form that `worker/index.ts` (the only server code; it runs for `/api/*`) emails to info@swarm-term.com through Email Routing's `send_email` binding, with the reporter as Reply-To. A Worker may only mail verified destinations, so it delivers to the address info@ forwards to, set once with `npx wrangler secret put REPORT_TO`. Nothing is stored. A hidden honeypot field drops bots, and the Workers rate limiter allows 3 sent reports per minute per address. Email Routing forwards info@ to the account's verified address. `npx wrangler types worker/worker-configuration.d.ts` regenerates the binding types after changing `wrangler.jsonc`; `npx wrangler dev` runs the Worker locally and saves sent mail under `.wrangler/tmp/email/` instead of sending it.

**Counts and release news (D1).** The database `swarm-site` (`migrations/`, applied with `npx wrangler d1 migrations apply swarm-site --remote`) holds daily totals and the news list; nothing identifies a visitor. `worker/index.ts` adds 1 for each site download (`/download/Swarm-<version>.dmg`, which then redirects to R2; bots skipped), each update an installed Swarm downloads, and each update check (`/releases/latest.json` read by `Swarm/<version>`, about once per launch and per day, so it tracks copies in use). The optional signup in the footer and on `/news/` stores an address in `subscribers`. `scripts/stats.sh [days]` prints the totals; `scripts/stats.sh subscribers` prints the list as CSV.

**Search.** Every page has a canonical URL, Open Graph and Twitter tags, and JSON-LD (`WebSite`; the home page adds `SoftwareApplication` and an `FAQPage` matching its visible FAQ). `robots.txt` allows everything but `/api/`, with Cloudflare content signals allowing search and AI use; `sitemap.xml` and `llms.txt` (a summary for AI assistants) are generated from `src/pages/`. Confirmation and error pages are `noindex`.

**Going public later.** Set `REPO_PUBLIC = true` in `src/site.ts` to turn the grayed-out GitHub links back on.

## Updating

- Version, download URLs and whether to link GitHub: `src/site.ts`.
- Changelog entries: `src/pages/changelog.astro`.
- Hero screenshot: `src/assets/hero.png` (a 2560×1600 window capture; Astro makes the WebP sizes).
