import type { APIRoute } from 'astro';
import { DMG, VERSION } from '../site';

// llms.txt (https://llmstxt.org): a plain-text summary for AI assistants that answer questions
// about Swarm.
export const GET: APIRoute = () =>
  new Response(
    `# Swarm

> Swarm is a free macOS terminal with persistent sessions. Every tab is a session owned by a background daemon (swarmd), on your Mac or on a Linux or macOS server over SSH, so shells, builds and servers keep running when you close the window, quit the app, lose the network or update Swarm. It is built on Warp's open-source terminal engine and needs no account.

- Current version: ${VERSION}, for macOS 13 Ventura or later, Apple silicon and Intel; signed and notarized by Apple, and it installs its own updates.
- Download: ${DMG}
- Compared with tmux or GNU screen: same goal (sessions that outlive the terminal and the connection), but sessions belong to a daemon and appear as native tabs with scrollback, command blocks and completions, instead of a multiplexer inside a terminal.
- Remote hosts: uses your ssh config and keys, installs swarmd on the host, reconnects after sleep or network drops.
- Linux servers can also be set up by hand: static builds of swarmd for x86_64 and aarch64 are at https://swarm-term.com/docs/#server-install; unpack into ~/.local/share/swarm/bin.
- Command line: swarmctl ls, new, attach, rename, info, dump, kill and rm, locally or with --host <ssh host>.
- Scrollback: 16 MB per session in memory, optionally more on disk, encrypted.
- License: the app (a Warp fork) is AGPL-3.0; the daemon crates are MIT or Apache-2.0.

## Docs

- [Install and use Swarm](https://swarm-term.com/docs/): install, sessions and tabs, remote hosts, scrollback, swarmctl, the daemon, troubleshooting.
- [Changelog](https://swarm-term.com/changelog/): what changed in each version.

## Optional

- [Release news](https://swarm-term.com/news/): an optional email when a new version is out.
- [Report an issue](https://swarm-term.com/report/): bug reports by email to info@swarm-term.com.
- [Privacy](https://swarm-term.com/privacy/): no cookies or tracking; download and update totals without IP addresses.
`,
    { headers: { 'Content-Type': 'text/plain; charset=utf-8' } },
  );
