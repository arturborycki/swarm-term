# swarm-term

A terminal where every tab is a long-lived session owned by a background
daemon, local or remote. See [docs/DESIGN.md](docs/DESIGN.md).

## Status

Milestones M0–M1 (daemon core) are working on macOS and Linux: sessions
survive closing the client and restarting the daemon, and can be listed,
attached, detached and removed from the command line, locally or over ssh.
The GUI (Warp fork, M2) isn't started yet.

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
