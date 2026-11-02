# swarm-term.com

The product site: Astro, static output, no client framework.

```bash
npm install
npm run dev      # http://localhost:4321
npm run build    # writes dist/
```

## Deploy (Cloudflare Pages)

The domain is registered and served by Cloudflare, so Pages needs no DNS changes elsewhere.

1. Cloudflare dashboard → Workers & Pages → Create → Pages → connect `arturborycki/swarm-term`.
2. Build settings: root directory `site`, build command `npm run build`, output directory `dist`.
3. Every push to `main` deploys; other branches get preview URLs.
4. Custom domains → add `swarm-term.com` and `www.swarm-term.com`.

The download links point at GitHub release assets (`src/site.ts`), which only work for visitors once the repo is public.

## Updating

- Version and download URLs: `src/site.ts`.
- Changelog entries: `src/pages/changelog.astro`.
- Hero screenshot: `src/assets/hero.png` (a 2560×1600 window capture; Astro makes the WebP sizes).
