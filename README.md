# swarm-term

A terminal where every tab is a long-lived session owned by a background
daemon, local or remote. See [docs/DESIGN.md](docs/DESIGN.md).

## Status

**0.1** is the first release: `Swarm.app` for macOS 13 or later, a
universal build for Apple silicon and Intel. Download the DMG from
[Releases](https://github.com/arturborycki/swarm-term/releases).

- **Persistent tabs:** every tab is a session owned by `swarmd`, so it keeps
  running when you close the window, quit the app or restart the daemon.
- **Local or remote:** sessions run on this Mac or on Linux and macOS hosts
  over ssh. The app installs `swarmd` on remote hosts itself.
- **The GUI:** a fork of Warp, with blocks, the input editor and completions,
  in its own design.
- **Daemon updates:** `swarm-shim` is updated in place without ending
  sessions.
- **Scrollback:** 16 MB per session is kept in memory. More can optionally be
  kept on disk, encrypted.

Since 0.1.2 the DMG is signed with a Developer ID and notarized, and the
app installs its own updates (Install and Restart), each checked against
the release signature. 0.1.0 and 0.1.1 aren't notarized: allow them once in
System Settings → Privacy & Security → Open Anyway.

| Crate | What it is |
|---|---|
| `crates/swarm-proto` | Wire protocol, framing, shared paths |
| `crates/swarm-shim` | Per-session process: PTY, child, scrollback, replay |
| `crates/swarmd` | The daemon (`swarmd`) and the shim binary (`swarm-shim`) |
| `crates/swarm-client` | Async client library (local socket or `ssh host swarmd bridge`) |
| `crates/swarmctl` | Command-line client |

## Build and try

```bash
cargo build --release
```

```bash
./target/release/swarmctl new --name work
```

Inside the session, press Ctrl-\ to detach. Then:

```bash
./target/release/swarmctl ls
```

```bash
./target/release/swarmctl attach work
```

Other commands: `swarmctl rename`, `kill`, `rm`, `info`, `meta`, `daemon status|stop`.
The daemon starts on first use; `swarmd stop` stops it without ending any session.

To start the daemon at login instead (launchd agent on macOS, systemd user unit on
Linux, which also turns on lingering so sessions survive logout):

```bash
./target/release/swarmctl daemon install-service
```

`daemon uninstall-service` goes back to starting it on demand; `daemon service-status`
shows the current mode.

### Remote hosts

Add `--host <ssh-host>` to any command to use the daemon on that machine through your
own ssh setup (keys, agent, `~/.ssh/config`). The host needs `swarmd` and `swarm-shim`
in `~/.local/share/swarm/bin/` (or `swarmd` on `PATH`); automatic install comes with
the app (M3). For example:

```bash
./target/release/swarmctl --host devbox new --name build
```

## Tests

```bash
cargo test --workspace
```

The end-to-end tests in `crates/swarmd/tests/e2e.rs` start real daemons, shims
and shells. `scripts/interactive-check.exp` drives `swarmctl` with vim and htop
in a real pty:

```bash
cargo build && rm -rf /tmp/swx && mkdir -p /tmp/swx && expect scripts/interactive-check.exp $PWD/target/debug
```
