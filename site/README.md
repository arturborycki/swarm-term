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

**Downloads (Cloudflare R2).** The DMG is too big for static assets (25 MiB per file), so it lives in the R2 bucket `swarm-downloads`, served at `downloads.swarm-term.com`. R2 doesn't charge for downloads.

```bash
npx wrangler r2 bucket create swarm-downloads
npx wrangler r2 bucket domain add swarm-downloads --domain downloads.swarm-term.com --zone-id <swarm-term.com zone id>
scripts/sign-update.sh             # signs dist/Swarm-<version>.dmg, records it in src/site.ts
scripts/upload-release.sh          # uploads dist/Swarm-<version>.dmg from the repo root
```

**Updates.** `/releases/latest.json` describes the current release in the same shape as GitHub's API, for the app's "Check for Updates…", plus the DMG's `sha256` and Ed25519 `signature` (`DMG_SHA256`, `DMG_SIGNATURE` in `src/site.ts`, written by `scripts/sign-update.sh`). The app installs an update itself only when that signature verifies with the release key built into it, so sign every release before deploying the site. The private key is `~/.config/swarm-term/update-signing-key.pem` on the release Mac; keep a backup.

**Issue reports.** `/report/` is a form that `worker/index.ts` (the only server code; it runs for `/api/*`) emails to info@swarm-term.com through Email Routing's `send_email` binding, with the reporter as Reply-To. A Worker may only mail verified destinations, so it delivers to the address info@ forwards to, set once with `npx wrangler secret put REPORT_TO`. Nothing is stored. A hidden honeypot field drops bots, and the Workers rate limiter allows 3 sent reports per minute per address. Email Routing forwards info@ to the account's verified address. `npx wrangler types worker/worker-configuration.d.ts` regenerates the binding types after changing `wrangler.jsonc`; `npx wrangler dev` runs the Worker locally and saves sent mail under `.wrangler/tmp/email/` instead of sending it.

**Going public later.** Set `REPO_PUBLIC = true` in `src/site.ts` to turn the grayed-out GitHub links back on.

## Updating

- Version, download URLs and whether to link GitHub: `src/site.ts`.
- Changelog entries: `src/pages/changelog.astro`.
- Hero screenshot: `src/assets/hero.png` (a 2560×1600 window capture; Astro makes the WebP sizes).
