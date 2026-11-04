import type { APIRoute } from 'astro';

// Pages for search engines; confirmation and error pages are noindex and left out.
const PAGES = ['/', '/docs/', '/changelog/', '/news/', '/report/', '/privacy/'];

export const GET: APIRoute = ({ site }) => {
  const lastmod = new Date().toISOString().slice(0, 10);
  const urls = PAGES.map(
    (path) => `  <url><loc>${new URL(path, site)}</loc><lastmod>${lastmod}</lastmod></url>`,
  ).join('\n');
  return new Response(
    `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`,
    { headers: { 'Content-Type': 'application/xml' } },
  );
};
