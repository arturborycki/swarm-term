import type { APIRoute } from 'astro';
import { APP_REPO, DMG, REPO, VERSION } from '../site';
import { GUIDES } from '../guides';

// llms.txt (https://llmstxt.org): a plain-text summary for AI assistants that answer questions
// about Swarm.
export const GET: APIRoute = () =>
  new Response(
    `# Swarm

> Swarm is a free macOS terminal with persistent sessions. Every tab is a session owned by a background daemon (swarmd), on your Mac or on a Linux or macOS server over SSH, so shells, builds and servers keep running when you close the window, quit the app, lose the network or update Swarm. It is built on Warp's open-source terminal engine and needs no account.

- Current version: ${VERSION}, for macOS 13 Ventura or later, Apple silicon and Intel; signed and notarized by Apple, and it installs its own updates.
- Download: ${DMG}
- Homebrew: brew install --cask arturborycki/swarm/swarm
- Compared with tmux or GNU screen: same goal (sessions that outlive the terminal and the connection), but sessions belong to a daemon and appear as native tabs with scrollback, command blocks and completions, instead of a multiplexer inside a terminal.
- AI coding agents: Claude Code, Codex and similar agents started in a Swarm tab keep running when the tab closes or the app quits; on a server they also keep running while the Mac sleeps.
- Splits and groups: sessions sit in sidebar groups (This Mac, one per host, your own); any session, local or remote, can be shown beside others in split panes.
- Finding sessions: Go to Anything (⌘P) jumps to tabs, detached sessions and hosts, and to sessions by state ("needs you"); the Activity view (⌘.) lists sessions that need you, finished or failed; notifications and a Dock badge flag them. A Scratch Terminal (⌥⌘T) floats over every window.
- Ports: a dev server in a session shows up in Go to Anything as "Open localhost:3000"; on a remote host Swarm forwards the port over ssh with your own config first. Any port can be forwarded by typing it.
- Remote hosts: uses your ssh config and keys, installs swarmd on the host, reconnects after sleep or network drops.
- Linux servers can also be set up by hand: static builds of swarmd for x86_64 and aarch64 are at https://swarm-term.com/docs/#server-install; unpack into ~/.local/share/swarm/bin.
- Command line: swarmctl ls, new, attach, rename, info, dump, kill and rm, locally or with --host <ssh host>. For scripts and agents: swarmctl send, send-key, capture (the screen as text) and wait (exit status), with --json. swarmctl status shows what programs report through the Program Status Protocol (OSC 7501: working, blocked, done, error), bells and notifications; wait --until blocked,done waits for one; swarmctl events streams changes. swarmctl ports lists the TCP ports programs in sessions listen on (a dev server), and the app's Go to Anything (⌘P) opens them, forwarding from remote hosts.
- Scrollback: 16 MB per session in memory, optionally more on disk, encrypted.
- License: the app (a Warp fork) is AGPL-3.0; the daemon crates are MIT or Apache-2.0.
- Source: ${REPO} (daemon, swarmctl, site) and ${APP_REPO} (the app).

## Docs

- [Install and use Swarm](https://swarm-term.com/docs/): install, sessions and tabs, remote hosts, scrollback, swarmctl, the daemon, troubleshooting.
- [Changelog](https://swarm-term.com/changelog/): what changed in each version.
${GUIDES.map((guide) => `- [${guide.title}](https://swarm-term.com${guide.href}): ${guide.summary}`).join('\n')}

## Optional

- [Release news](https://swarm-term.com/news/): an optional email when a new version is out.
- [Report an issue](https://swarm-term.com/report/): bug reports by email to info@swarm-term.com.
- [Privacy](https://swarm-term.com/privacy/): no cookies or tracking; download and update totals without IP addresses.
`,
    { headers: { 'Content-Type': 'text/plain; charset=utf-8' } },
  );
