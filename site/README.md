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

**Site (Cloudflare Workers static assets).** `scripts/deploy.sh` builds and uploads `dist/` to the Worker `swarm-term` (`wrangler.jsonc`: static files only, no server code; unknown paths get `404.html`). It's live at `swarm-term.artur-borycki.workers.dev`; `scripts/deploy.sh preview` uploads a version with its own preview URL and leaves the live site alone. To serve it at `swarm-term.com`, add `"routes": [{ "pattern": "swarm-term.com", "custom_domain": true }, { "pattern": "www.swarm-term.com", "custom_domain": true }]` to `wrangler.jsonc` and deploy; the domain is already on Cloudflare, so the DNS records are created for you.

**Downloads (Cloudflare R2).** The DMG is too big for static assets (25 MiB per file), so it lives in the R2 bucket `swarm-downloads`, served at `downloads.swarm-term.com`. R2 doesn't charge for downloads.

```bash
npx wrangler r2 bucket create swarm-downloads
npx wrangler r2 bucket domain add swarm-downloads --domain downloads.swarm-term.com --zone-id <swarm-term.com zone id>
scripts/upload-release.sh          # uploads dist/Swarm-<version>.dmg from the repo root
```

**Updates.** `/releases/latest.json` describes the current release in the same shape as GitHub's API, for the app's "Check for Updates…".

**Going public later.** Set `REPO_PUBLIC = true` in `src/site.ts` to turn the grayed-out GitHub links back on.

## Updating

- Version, download URLs and whether to link GitHub: `src/site.ts`.
- Changelog entries: `src/pages/changelog.astro`.
- Hero screenshot: `src/assets/hero.png` (a 2560×1600 window capture; Astro makes the WebP sizes).
