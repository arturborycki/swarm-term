// The site's server code. Everything else is the static site in dist/.
//   GET  /download/Swarm-<version>.dmg  counts a download, then redirects to the GitHub release
//   GET  /download/swarmd-<version>-linux-<arch>.tar.gz  the same, for the Linux daemon
//   GET  /releases/latest.json          counts an installed Swarm's update check
//   POST /api/report                    mails an issue report (nothing stored)
//   POST /api/subscribe                 adds an address to the release-news list
// Counts are daily totals in D1 (migrations/), with no IP addresses.
import { EmailMessage } from 'cloudflare:email';

interface Env {
  ASSETS: Fetcher;
  DB: D1Database;
  REPORTS: SendEmail;
  REPORT_LIMIT: RateLimit;
  REPORT_TO: string;
}

// Release files are assets of the GitHub release tagged v<version> (scripts/publish-release.sh).
const RELEASES = 'https://github.com/arturborycki/swarm-term/releases/download';
const TO = 'info@swarm-term.com';
const FROM = 'reports@swarm-term.com';
const CATEGORIES = ['Bug', 'Crash', 'Install or update', 'Remote hosts', 'Question or idea'];
const MAX_TEXT = 8000;
const DMG = /^\/download\/(Swarm-(\d+\.\d+\.\d+)\.dmg)$/;
const LINUX = /^\/download\/(swarmd-(\d+\.\d+\.\d+)-linux-(?:x86_64|aarch64)\.tar\.gz)$/;
// Installed apps identify themselves as `Swarm/<version>` (app/src/swarm/updates.rs).
const APP_AGENT = /^Swarm\/(\d+\.\d+\.\d+)/;
const BOT_AGENT = /bot|crawl|spider|slurp|preview|monitor|curl|wget|python|headless/i;
// Linux archives are usually fetched with curl or wget, so only crawlers are left out.
const CRAWLER_AGENT = /bot|crawl|spider|slurp|preview|monitor/i;

type Reply = (status: number, error?: string) => Response;

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    const agent = request.headers.get('User-Agent') ?? '';
    const country = (request.cf?.country as string | undefined) ?? 'XX';

    const dmg = url.pathname.match(DMG);
    if (dmg) {
      const app = agent.match(APP_AGENT);
      if (request.method === 'GET' && (app || !BOT_AGENT.test(agent))) {
        ctx.waitUntil(count(env, app ? 'update' : 'download', dmg[2], country));
      }
      return Response.redirect(`${RELEASES}/v${dmg[2]}/${dmg[1]}`, 302);
    }

    const linux = url.pathname.match(LINUX);
    if (linux) {
      if (request.method === 'GET' && !CRAWLER_AGENT.test(agent)) {
        ctx.waitUntil(count(env, 'linux', linux[2], country));
      }
      return Response.redirect(`${RELEASES}/v${linux[2]}/${linux[1]}`, 302);
    }

    if (url.pathname === '/releases/latest.json') {
      const app = agent.match(APP_AGENT);
      if (app) {
        ctx.waitUntil(count(env, 'check', app[1], country));
      }
      return env.ASSETS.fetch(request);
    }

    if (url.pathname === '/api/report' || url.pathname === '/api/subscribe') {
      if (request.method !== 'POST') {
        return new Response('Method Not Allowed', { status: 405, headers: { Allow: 'POST' } });
      }
      const page = url.pathname === '/api/report' ? '/report/' : '/news/';
      const done = url.pathname === '/api/report' ? '/report/sent/' : '/news/subscribed/';
      const wantsJson = (request.headers.get('Accept') ?? '').includes('application/json');
      const reply: Reply = (status, error) =>
        wantsJson
          ? Response.json(error ? { error } : { ok: true }, { status })
          : Response.redirect(new URL(error ? `${page}?error=${encodeURIComponent(error)}` : done, url).href, 303);
      let form: FormData;
      try {
        form = await request.formData();
      } catch {
        return reply(400, "That form didn't come through. Please try again.");
      }
      const field = (name: string) => String(form.get(name) ?? '').trim();
      // Hidden from people; bots fill it in.
      if (field('website')) {
        return reply(200);
      }
      const ip = request.headers.get('CF-Connecting-IP') ?? 'unknown';
      return url.pathname === '/api/report'
        ? report(env, field, ip, reply)
        : subscribe(env, field, ip, reply);
    }

    return env.ASSETS.fetch(request);
  },
} satisfies ExportedHandler<Env>;

async function count(env: Env, kind: string, version: string, country: string): Promise<void> {
  try {
    await env.DB.prepare(
      `INSERT INTO stats (day, kind, version, country, count) VALUES (?, ?, ?, ?, 1)
       ON CONFLICT (day, kind, version, country) DO UPDATE SET count = count + 1`,
    )
      .bind(new Date().toISOString().slice(0, 10), kind, version, country)
      .run();
  } catch (e) {
    console.error('count failed', e);
  }
}

function validEmail(email: string): boolean {
  return /^[^\s@<>"]+@[^\s@<>"]+\.[^\s@<>"]+$/.test(email) && email.length <= 254;
}

/** Counts only requests about to do something, so fixing a rejected form doesn't use up the limit. */
async function limited(env: Env, ip: string): Promise<boolean> {
  return !(await env.REPORT_LIMIT.limit({ key: ip })).success;
}

async function subscribe(env: Env, field: (name: string) => string, ip: string, reply: Reply): Promise<Response> {
  const email = field('email');
  if (!validEmail(email)) {
    return reply(400, 'Please enter a valid email address.');
  }
  if (await limited(env, ip)) {
    return reply(429, 'Too many requests from your address. Please try again in a minute.');
  }
  try {
    await env.DB.prepare(
      `INSERT INTO subscribers (email, created_at, source) VALUES (?, ?, ?)
       ON CONFLICT (email) DO UPDATE SET unsubscribed_at = NULL`,
    )
      .bind(email, new Date().toISOString(), field('source').slice(0, 40) || 'site')
      .run();
  } catch (e) {
    console.error('subscribe failed', e);
    return reply(502, `That didn't work. Please email ${TO} instead.`);
  }
  return reply(200);
}

async function report(env: Env, field: (name: string) => string, ip: string, reply: Reply): Promise<Response> {
  const fields = {
    email: field('email'),
    category: field('category'),
    swarm: field('swarm_version').slice(0, 40),
    macos: field('macos_version').slice(0, 40),
    summary: field('summary').slice(0, 200),
    details: field('details'),
  };
  if (!validEmail(fields.email)) {
    return reply(400, 'Please enter an email address we can reply to.');
  }
  if (!fields.summary || !fields.details) {
    return reply(400, 'Please say what happened.');
  }
  if (fields.details.length > MAX_TEXT) {
    return reply(400, `Please keep the description under ${MAX_TEXT} characters.`);
  }
  if (!CATEGORIES.includes(fields.category)) {
    fields.category = 'Bug';
  }
  if (await limited(env, ip)) {
    return reply(429, 'Too many reports from your address. Please try again in a minute.');
  }
  try {
    // Workers may only send to verified Email Routing destinations, so the report,
    // addressed to info@, is delivered to REPORT_TO, the address info@ forwards to.
    await env.REPORTS.send(new EmailMessage(FROM, env.REPORT_TO, mime(fields, ip)));
  } catch (e) {
    console.error('send failed', e);
    return reply(502, `Your report couldn't be sent. Please email ${TO} instead.`);
  }
  return reply(200);
}

function mime(r: Record<'email' | 'category' | 'swarm' | 'macos' | 'summary' | 'details', string>, ip: string): string {
  const body = [
    `From:     ${r.email}`,
    `Category: ${r.category}`,
    `Swarm:    ${r.swarm || '(not given)'}`,
    `macOS:    ${r.macos || '(not given)'}`,
    `Sent from ${ip} via swarm-term.com/report`,
    '',
    r.details,
    '',
  ].join('\r\n');
  return [
    `From: Swarm issue reports <${FROM}>`,
    `To: ${TO}`,
    `Reply-To: ${r.email}`,
    `Subject: ${encodeWord(`[Swarm ${r.category}] ${r.summary}`)}`,
    `Date: ${new Date().toUTCString()}`,
    `Message-ID: <${crypto.randomUUID()}@swarm-term.com>`,
    'MIME-Version: 1.0',
    'Content-Type: text/plain; charset=utf-8',
    'Content-Transfer-Encoding: base64',
    '',
    base64(body).replace(/.{76}/g, '$&\r\n'),
  ].join('\r\n');
}

/** A header value as an RFC 2047 encoded word, so any text is safe in it. */
function encodeWord(text: string): string {
  return `=?utf-8?B?${base64(text.replace(/[\r\n]+/g, ' '))}?=`;
}

function base64(text: string): string {
  let binary = '';
  for (const byte of new TextEncoder().encode(text)) binary += String.fromCharCode(byte);
  return btoa(binary);
}
