// The site's one piece of server code: POST /api/report mails an issue report,
// addressed to info@swarm-term.com, with the reporter as Reply-To. Workers may
// only send to verified Email Routing destinations, so it's delivered to
// REPORT_TO, the address info@ forwards to. Nothing is stored. Every other request is the static
// site in dist/.
import { EmailMessage } from 'cloudflare:email';

interface Env {
  ASSETS: Fetcher;
  REPORTS: SendEmail;
  REPORT_LIMIT: RateLimit;
  REPORT_TO: string;
}

const TO = 'info@swarm-term.com';
const FROM = 'reports@swarm-term.com';
const CATEGORIES = ['Bug', 'Crash', 'Install or update', 'Remote hosts', 'Question or idea'];
const MAX_TEXT = 8000;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname !== '/api/report') {
      return env.ASSETS.fetch(request);
    }
    if (request.method !== 'POST') {
      return new Response('Method Not Allowed', { status: 405, headers: { Allow: 'POST' } });
    }
    const wantsJson = (request.headers.get('Accept') ?? '').includes('application/json');
    const reply = (status: number, error?: string) =>
      wantsJson
        ? Response.json(error ? { error } : { ok: true }, { status })
        : Response.redirect(new URL(error ? `/report/?error=${encodeURIComponent(error)}` : '/report/sent/', url).href, 303);

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
    const report = {
      email: field('email'),
      category: field('category'),
      swarm: field('swarm_version').slice(0, 40),
      macos: field('macos_version').slice(0, 40),
      summary: field('summary').slice(0, 200),
      details: field('details'),
    };
    if (!/^[^\s@<>"]+@[^\s@<>"]+\.[^\s@<>"]+$/.test(report.email) || report.email.length > 254) {
      return reply(400, 'Please enter an email address we can reply to.');
    }
    if (!report.summary || !report.details) {
      return reply(400, 'Please say what happened.');
    }
    if (report.details.length > MAX_TEXT) {
      return reply(400, `Please keep the description under ${MAX_TEXT} characters.`);
    }
    if (!CATEGORIES.includes(report.category)) {
      report.category = 'Bug';
    }

    // Counts only reports about to be sent, so fixing a rejected form doesn't use up the limit.
    const ip = request.headers.get('CF-Connecting-IP') ?? 'unknown';
    if (!(await env.REPORT_LIMIT.limit({ key: ip })).success) {
      return reply(429, 'Too many reports from your address. Please try again in a minute.');
    }
    try {
      await env.REPORTS.send(new EmailMessage(FROM, env.REPORT_TO, mime(report, ip)));
    } catch (e) {
      console.error('send failed', e);
      return reply(502, `Your report couldn't be sent. Please email ${TO} instead.`);
    }
    return reply(200);
  },
} satisfies ExportedHandler<Env>;

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
