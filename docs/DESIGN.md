# swarm-term — Design

Status: draft v0.1 (2026-10-30)

## 1. Goal

A terminal app where **every tab is a long-lived session owned by a background daemon**, not by the UI.

- Closing the app (or a crash) does not kill anything. Sessions keep running like `nohup`/`tmux`.
- Reopening the app reattaches every running session of the local daemon as tabs.
- The app can also connect to a **remote daemon**; there the user picks which sessions to open from a dropdown instead of auto-attaching everything.
- The UI is based on Warp (blocks, input editor, GPU rendering), with "new window" replaced by "new tab = new session".

Supported daemon hosts (v1), local and remote: **Linux** (x86_64, aarch64) and **macOS** (arm64, x86_64). Platform-specific details are in §9a.

Non-goals (v1): session sharing between different users, Windows host support for the daemon, a tmux-style split/pane model inside the daemon (splits, if any, stay a UI concept).

## 2. Components

```
┌──────────────────────── macOS / Linux desktop ────────────────────────┐
│                                                                        │
│  swarm (GUI, Warp fork)                                                │
│   ├─ TabManager ── one tab ⇄ one SessionHandle                         │
│   ├─ TerminalManager trait (exists in Warp)                            │
│   │    ├─ daemon_tty  (new; unix socket / ssh stdio)                   │
│   │    └─ local_tty   (kept only for tests / fallback)                 │
│   └─ HostPicker + SessionPicker (remote dropdown)                      │
│            │                                   │                       │
│            │ unix socket                       │ ssh host swarmd bridge│
│            ▼                                   ▼                       │
│  swarmd (local supervisor)               ┌──────────── remote host ──┐ │
│   ├─ swarm-shim #1 ── PTY ── zsh         │ swarmd                    │ │
│   ├─ swarm-shim #2 ── PTY ── zsh         │  ├─ swarm-shim ── PTY ─ … │ │
│   └─ swarm-shim #3 ── PTY ── ssh/…       │  └─ swarm-shim ── PTY ─ … │ │
│                                           └──────────────────────────┘ │
│  swarmctl (CLI: list / attach / kill / rename, dtach-like raw attach)  │
└────────────────────────────────────────────────────────────────────────┘
```

| Binary | Role |
|---|---|
| `swarmd` | Per-user supervisor. Owns the session registry, accepts client connections, spawns/reaps shims. Never touches PTY bytes on the hot path beyond forwarding. |
| `swarm-shim` | One process per session (containerd-shim / abduco model). Holds the PTY master, the child shell, the scrollback ring buffer and the server-side terminal model. |
| `swarm` | The GUI (Warp fork). Pure client: it never owns a PTY in production mode. |
| `swarmctl` | CLI for scripting, debugging and headless use (`swarmctl attach <id>` works over plain ssh with no GUI). |

### Why a shim per session

If the PTY masters lived inside `swarmd`, a daemon crash or upgrade would kill every session (this is tmux's failure mode). With shims:

- `swarmd` can be restarted/upgraded; on startup it rediscovers shims through `$RUNTIME_DIR/swarm/shims/<session-id>.sock` and re-adopts them.
- A bug triggered by one session's output (parser panic) only takes down that session.
- Cost: one small extra process per session (~2–5 MB RSS). Acceptable.

## 3. Process lifecycle

### Daemon start mode (user setting, per host)

| Mode | Behaviour |
|---|---|
| **`with-app`** (default) | The daemon is started on demand: the first time the GUI (locally) or `swarmd bridge` (remotely) finds no running daemon. It then keeps running after the app quits. After a reboot/logout it is gone until the app is opened again. |
| **`at-login`** | A launchd agent (macOS) or systemd `--user` unit (Linux) starts the daemon at login and restarts it if it dies, so sessions can be created by `swarmctl` before the GUI ever opens. |

In both modes the daemon is never a child of the app, and quitting the app never stops it. The mode only decides **who starts it**. Neither mode keeps sessions alive across a reboot (the processes are gone); see §12 M4 for restoring a session's layout and cwd after a reboot.

The local mode is chosen on first run (§3a) and changeable in Settings → Daemon. Each remote host has its own mode, set in its host settings (§9).

### Daemon startup (mechanics)
1. Client (GUI, `swarmctl`, or `swarmd bridge`) tries `$RUNTIME_DIR/swarm/swarmd.sock`.
2. If absent: spawns `swarmd --daemonize` (double-fork + `setsid`, stdio → log file, own process group). This happens in both modes, as a fallback in `at-login` mode in case the service is not running.
3. `at-login` is installed/removed by `swarmctl service install|uninstall`, which the GUI calls when the setting changes. It writes `~/Library/LaunchAgents/dev.swarm.swarmd.plist` (macOS) or `~/.config/systemd/user/swarmd.service` (Linux). Both are user-level and need no admin rights. On Linux the unit uses `KillMode=process` so a daemon restart never kills shims.
4. **Linux remote hosts:** sessions only survive the user's ssh logout if systemd does not kill the user's processes at logout. `loginctl enable-linger` keeps them alive. On most systems a user can enable it for themselves; if the host forbids it, the GUI shows "Sessions on this host will end when you log out" instead of failing silently. In `with-app` mode the daemon is started via `setsid` from the ssh session, which survives logout unless the host sets `KillUserProcesses=yes`; the same check and warning apply.
5. Single-instance guaranteed by an `flock` on `$STATE_DIR/swarmd.lock`.

### 3a. First run

The first time the app opens, a short onboarding sheet (one screen, skippable):

1. **Local sessions:** "Start the session daemon: ( ) when Swarm opens (recommended) ( ) when I log in". The local daemon is started right away either way, and the first local tab opens.
2. **Remote sessions (optional):** "Connect to a server" — host list read from `~/.ssh/config`, plus a field for `user@host[:port]`. Picking a host runs the remote connect flow (§9): access check, `swarmd` install if needed, then the session picker. Can be skipped and done later from the toolbar.
3. Nothing else is asked; all choices are editable in Settings.

The app works fully without a local daemon session ever being used: a user can work only with remote sessions. The local daemon is still started (it is cheap), but no local tab has to stay open.

### Session creation
`swarmd` spawns `swarm-shim --id <ulid> --shell … --cwd … --env …`. The shim calls `forkpty`, execs the shell as a login shell, opens its control socket, and reports `Ready` back to `swarmd`. Shim is in its own session (`setsid`) so terminal signals from the app never reach it.

### Session end
Child exits → shim records exit status, keeps the buffer for `exited_retention` (default 10 min, or until a client acknowledges the exit), then exits. The tab shows "Process exited (code N) — [Restart] [Close]".

### Daemon upgrade
`swarmd` is restarted in place; shims are untouched. Shim upgrade requires session restart (or later: PTY fd handoff via `SCM_RIGHTS` to a new shim — stretch goal).

## 4. Session model

```rust
struct SessionInfo {
    id: SessionId,              // ULID, stable across daemon restarts
    name: Option<String>,       // user-set; else derived from title/cwd
    title: String,              // last OSC 0/2
    cwd: Option<PathBuf>,       // from OSC 7 / shell integration
    shell: ShellKind,           // zsh | bash | fish | pwsh | other
    foreground_cmd: Option<String>, // tcgetpgrp → /proc or sysctl
    state: Running | Exited { code, at },
    created_at, last_output_at, last_input_at,
    size: (cols, rows),
    attached_clients: Vec<ClientInfo>,
    ui: UiHints,                // tab order, color, pinned, "auto_attach" flag
    integration: IntegrationState, // was Warp bootstrap injected? which version?
}
```

`UiHints` is opaque-ish metadata stored by the daemon so that tab order, colors and names survive app restarts and are the same from any client.

## 5. Wire protocol

- Transport: a byte stream (unix socket locally; ssh stdio remotely). Framing: `u32 length | u8 kind | payload`, payload encoded with `postcard` (serde). A version handshake is the first frame.
- One connection multiplexes control and N attached sessions (`stream_id` per attachment), so a remote host needs only one ssh channel.

### Control (client ⇄ swarmd)
```
Hello { proto_version, client_name, client_id }   → Welcome { daemon_version, host_info }
ListSessions                                       → Sessions(Vec<SessionInfo>)
Subscribe                                          → stream of SessionEvent (created/exited/renamed/title/cwd/attached)
CreateSession { shell?, cwd?, env, size, name?, ui } → Created(SessionInfo)
KillSession { id, signal }
RenameSession / SetUiHints { id, … }
Attach { id, mode: Full | ResumeFrom(offset), size, readonly } → Attached { stream_id, … }
Detach { stream_id }
```

### Per attachment (client ⇄ shim, relayed by swarmd)
```
→ Input(bytes)                 keystrokes / paste, raw
→ Resize { cols, rows, px }    see §7
← Snapshot { … }               only when the session is in alt-screen (see §6)
← Output { offset, bytes }     raw PTY bytes, monotonically increasing offset
← Exited { code }
← Lagged { resume_at }         client fell too far behind; it must re-attach
```

`swarmd` relays attachment traffic by splicing the client stream to the shim socket; it does not parse it.

## 6. Output, scrollback and reattach — the core problem

Warp's blocks, prompt detection and input editor depend on **escape sequences emitted by its shell integration** (prompt/command start/end markers, metadata in DCS/OSC). A tmux-style design that re-renders the screen would strip those, so Warp would degrade to a dumb grid. Therefore:

**The daemon is byte-transparent.** The client receives exactly the bytes the PTY produced, and Warp's own parser runs on the client as it does today.

Each shim keeps:
1. **Raw ring buffer** of PTY output (default 16 MB, configurable per session) addressed by a global `u64` byte offset.
2. **Boundary index**: offsets of safe replay start points — every shell-integration "prompt start / block start" marker plus every alt-screen exit. Detected with a lightweight `vte` scanner (no full emulation needed for this).
3. **Server-side terminal model** (upstream `alacritty_terminal`, the same code Warp's grid model was derived from; we use upstream rather than Warp's AGPL-licensed copy, see §14) used for two things only: knowing whether we're in alt-screen, and producing a screen snapshot for full-screen apps.

Reattach algorithm (`Attach { mode: Full }`):
- **Normal (primary screen) case:** pick the oldest boundary still inside the ring buffer → send `Output` from there. Warp's parser replays the bytes and reconstructs the blocks exactly as if it had been attached the whole time. Older history beyond the ring is lost (shown as "earlier output truncated").
- **Alt-screen case (vim, htop, less):** replay the primary-screen history up to the alt-screen enter boundary, then send a `Snapshot` of the current alt screen (cells + attributes + cursor + modes), then live bytes. The app can also send a resize-nudge (SIGWINCH via size ±1 and back) to force the app to repaint, as a fallback.
- **Network blip (`ResumeFrom(offset)`):** if `offset` is still in the ring, send only the delta — no visual reset at all. This makes remote reconnects seamless.

Backpressure: the shim's PTY reader never blocks on clients. Each client has a bounded queue; if it overflows the client gets `Lagged` and re-attaches with `ResumeFrom`. A slow/remote client can never stall the shell.

## 7. Multiple clients and window size

A session can be attached from several clients (laptop GUI + `swarmctl` over ssh + another machine).

- Size policy (configurable): **`latest-active` (default)** — the PTY takes the size of the client that most recently sent input or focused the tab; other clients render it letterboxed/clipped. Alternative `smallest` (tmux default).
- `readonly` attach for watching a session without being able to type or resize.
- Tabs show an "also attached on <client>" indicator.

## 8. Warp integration (the GUI)

### 8a. Findings from Warp's code (checked 2026-10-30, commit `b17a2ed`)

**License.** The UI framework crates `warpui` and `warpui_core` are MIT. Everything else, including the terminal model, blocks, input editor and the app, is **AGPL v3**. Warp's FAQ says forks are welcome as long as they stay open.

**Size.** About 1.8M lines of Rust and 690 MB of history, with many cloud and AI features (Drive, agents, auth, telemetry) wired through the app.

**How Warp runs shells today** (`crates/warp_terminal/src/local_tty/`):
- `PtySpawner` asks a helper subprocess, `TerminalServer` (`local_tty/server/`), to `forkpty` the shell. The helper sends the PTY master fd back over a unix socket with `SCM_RIGHTS`.
- The app's `EventLoop` (adapted from alacritty) reads that fd directly and feeds the terminal model.
- On quit, `PtySpawner::prepare_for_app_termination` drops the `TerminalServer`, which kills every shell. This is exactly the behaviour we replace.

**The seam already exists.** `app/src/terminal/terminal_manager.rs` defines a `TerminalManager` trait with two implementations:
- `local_tty::TerminalManager`, the normal local PTY (~1,500 lines);
- `remote_tty::TerminalManager`, an experimental "network-backed PTY" (~450 lines) that sends input to and reads output from a websocket (built for Warp's SSH proxy), sends resizes as control messages, and writes the zsh init script itself.

`remote_tty` is selected at **compile time** (`feature = "remote_tty"`, which excludes `local_tty`) in `app/src/pane_group/mod.rs`.

**How bootstrap works.** The shell emits an `InitShell` DCS hook. The app answers by writing the init script into the PTY (`enqueue_init_script` in `local_tty/terminal_manager.rs`). On replay, the old `InitShell` hook would be seen again and would **re-inject the script** into a live shell. Replayed bytes must therefore never trigger writes back to the PTY (see §13 risk 3).

**Not reusable for us.** Warp's `remote_server` crate is a protocol for code, diff and repo features on SSH hosts. It doesn't hold PTYs.

### What to change in the Warp fork
1. **Add `daemon_tty::TerminalManager`** as a third implementation of the `TerminalManager` trait, modeled on `remote_tty`. Instead of a websocket it uses `swarm-client` (our protocol over a unix socket or ssh stdio), and it maps `Message::Input` / `Resize` onto `Input` / `Resize` frames. Everything above it (grid, blocks, input editor, rendering) stays unchanged.
   - Select the manager **at runtime** in `pane_group` rather than with the `remote_tty` / `local_tty` cfg. In production every tab uses `daemon_tty`. `local_tty` stays only for tests and as an emergency fallback when no daemon can start.
   - With all shells living in shims, `TerminalServer` / `PtySpawner` is no longer used for normal tabs.
2. **Shell-integration bootstrap must be idempotent.** Bootstrap happens once, when the session is created. The shim records it in `SessionInfo.integration` (shell type, Warp session id, script version).
   - On reattach, `daemon_tty` marks replay frames, and the model's `InitShell` handling is suppressed for them, so the init script is never re-sent.
   - Warp's per-session state that bootstrap normally sets up (session id, env) comes from `SessionInfo` instead.
   - If the app was updated and the integration version changed, offer "re-bootstrap" rather than doing it silently.
3. **Warp's own tab restore must not duplicate blocks.** Warp saves app state (`app/src/persistence`, `session_management`) to restore windows and blocks after a restart. For `daemon_tty` tabs, the daemon's replay is the source of truth. Turn off restoring block contents for those tabs and keep only window and tab layout, or drop that too (to verify in M2).
4. **Window ⇒ tab.** `New Window` (Cmd+N) becomes "new tab with new session" in the existing window; keep at most one main window per host group (optional: allow dragging a tab out into a second window, still bound to the same session).
5. **Startup restore.** On launch: connect to local daemon → `ListSessions` → open a tab for every running local session with `auto_attach = true` (i.e. tabs that were open when the app quit; sessions the user detached stay in the picker), ordered by `UiHints.tab_order`. Sessions created outside the GUI (e.g. `swarmctl new`) start with `auto_attach = false`. If no tab would open, create a new session.
6. **Tab close semantics (decided): closing a tab detaches.** Cmd+W / the tab's × only detaches the tab: the session keeps running and the tab's `UiHints.auto_attach` is set to `false`. Ending a session is always an explicit action:
   - "Terminate session" (Cmd+Shift+W / tab context menu / picker), with a confirm if a foreground process other than the shell is running;
   - typing `exit` in the shell (the session ends normally);
   - closing the tab of a session that has already **exited** removes it for good (nothing left to keep).
   Quitting the app also detaches everything but leaves `auto_attach = true`, so those tabs come back on the next launch.
   Because detached sessions pile up, the tab bar shows a "N detached" button that opens the local session picker (§9), and the picker supports "Terminate" and bulk "Terminate all exited/idle > X days".
7. **Strip/disable** Warp's cloud-only features (login gating, telemetry, Drive sync, hosted agents) or make them opt-in. Do this with feature flags and settings rather than deleting code, to keep the diff against upstream small.

### Tab metadata shown
Host badge (local / host name + color), session name, cwd, running command, exited state, "attached elsewhere" dot.

## 9. Remote daemons

### Transport: SSH, not a listening port
`ssh <host> swarmd bridge` — the remote `swarmd bridge` subcommand connects to the remote user's local socket (starting the daemon if needed) and pipes the protocol over stdio. Benefits: no open port, reuses the user's ssh keys, agent, `~/.ssh/config`, jump hosts, ControlMaster. (A direct TLS/QUIC listener with token auth can come later for environments without ssh.)

The GUI uses a native ssh client (`russh`) or shells out to the system `ssh` binary. Recommend **system `ssh`** first: it honors the user's full config (ProxyJump, agent, hardware keys) for free.

### Access and provisioning
Access is whatever the user's ssh already allows: if `ssh <host>` works with their keys/agent, the app can use the host. No root/sudo is ever needed — everything is installed and run as that user.

Connect flow:
1. **Access check:** `ssh -o BatchMode=yes <host> true`. If it fails (no key, host key not trusted), the app shows ssh's error and how to fix it, rather than prompting for passwords.
2. **Platform + version check:** one round trip runs `uname -sm; ~/.local/share/swarm/bin/swarmd --version` to detect OS/arch and the installed version. If missing or incompatible, offer to upload the matching binary (§9a) to `~/.local/share/swarm/bin/` — always with explicit user confirmation.
3. **Start mode** for that host (asked once, stored in host settings): `with-app` (default — `swarmd bridge` starts the daemon on connect) or `at-login` (installs the user-level systemd/launchd unit on the host and checks linger, see §3 step 4).
4. Open the session picker.

Host settings are stored locally in `$CONFIG_DIR/swarm/hosts.toml`: ssh alias, display name, color, start mode, and which session tabs were open last time.

### Session picker UX (remote)
- Toolbar "Connect…" → host list (from `~/.ssh/config` + saved hosts).
- After connecting, a **dropdown/popover** lists that host's sessions: name, cwd, foreground command, last activity, state, "attached elsewhere". Multi-select, search, "Open selected", "Open all", "+ New session on <host>".
- Remote sessions are **not** auto-attached on app start; instead the app remembers which remote tabs were open and offers "Reconnect to <host> (3 tabs)".
- The same picker is used for the local daemon (via the "N detached" button) to re-open detached sessions, with terminate actions per row and in bulk.
- Connection loss: tab dims with "Reconnecting…", auto-retry with backoff, then `ResumeFrom(last_offset)`.

## 9a. Platform specifics (Linux and macOS hosts)

| Concern | Linux | macOS |
|---|---|---|
| Binaries shipped | static musl, x86_64 + aarch64 | universal (arm64 + x86_64), signed with Developer ID |
| Runtime dir (socket) | `$XDG_RUNTIME_DIR/swarm/`, fallback `/tmp/swarm-$UID/` | `confstr(_CS_DARWIN_USER_TEMP_DIR)/swarm/` |
| State / logs | `~/.local/state/swarm/` | `~/Library/Application Support/swarm/`, logs in `~/Library/Logs/swarm/` |
| Peer credential check | `SO_PEERCRED` | `getpeereid` / `LOCAL_PEERCRED` |
| Foreground command for tab label | `tcgetpgrp` + `/proc/<pid>/cmdline` | `tcgetpgrp` + `proc_pidpath` / `sysctl(KERN_PROCARGS2)` |
| cwd fallback (no OSC 7) | `/proc/<pid>/cwd` | `proc_pidinfo(PROC_PIDVNODEPATHINFO)` |
| `at-login` service | systemd `--user` unit, `KillMode=process` | LaunchAgent in `~/Library/LaunchAgents`, `ProcessType=Interactive` |
| Survives ssh logout | needs linger unless `KillUserProcesses=no` (§3 step 4) | yes, `setsid` processes started over ssh survive logout |
| Remote prerequisite | sshd running | "Remote Login" enabled in System Settings → General → Sharing |

macOS notes:
- **Socket path must be the same for GUI and ssh logins.** `$TMPDIR` can differ or be missing in an ssh session, so `swarmd` always resolves the per-user dir with `confstr`, never from env. Keep the full socket path under macOS's 104-byte `sun_path` limit; fall back to `/tmp/swarm-$UID/` if it would exceed it.
- **`at-login` on a remote Mac:** a LaunchAgent in the GUI domain only loads when the user logs in at the screen. On a headless Mac (ssh only), the installer bootstraps it into the `user/<uid>` domain instead (`launchctl bootstrap user/$UID …`), and the GUI shows that it will start on the first ssh connection after reboot unless someone logs in. `with-app` is the recommended default for remote Macs.
- **Gatekeeper:** a binary copied over ssh/scp gets no quarantine flag, so it runs, but arm64 requires a valid signature; release builds are Developer ID signed (ad-hoc signing is enough for dev builds).
- **Sleep:** a sleeping remote Mac drops connections; the client's reconnect + `ResumeFrom` (§6) covers it. Optional setting for that host: run `caffeinate -s` while sessions are attached.
- **Full Disk Access / TCC:** shells started by `swarmd` inherit the daemon's privacy permissions, not the GUI's. Commands touching protected folders (Desktop, Documents, removable volumes) may prompt or fail; document granting access to `swarmd` if needed.

## 10. Security

- Local socket in `$XDG_RUNTIME_DIR/swarm/` (macOS: `$TMPDIR/swarm-$UID/`), dir `0700`, socket `0600`; verify peer uid via `SO_PEERCRED` / `getpeereid` and reject mismatches.
- No network listener by default. Remote access is only through ssh as the same user.
- Ring buffers live in memory only; no scrollback on disk by default (secrets). Optional encrypted spill-to-disk later.
- Env passed to new sessions is filtered (don't forward GUI-specific vars like `SSH_AUTH_SOCK` of the laptop to a remote host unless asked).

## 11. Tech choices (Rust)

| Concern | Choice |
|---|---|
| async runtime | `tokio` |
| PTY | `rustix`/`nix` `forkpty` directly in the shim (or `portable-pty`) |
| VT parsing (boundaries) | `vte` |
| terminal model (snapshots) | `alacritty_terminal` |
| serialization | `serde` + `postcard`, explicit protocol version |
| IDs | `ulid` |
| paths | `directories` |
| logging | `tracing` + rolling file appender |
| ssh (later, optional) | `russh` |

Repo layout (cargo workspace):
```
crates/
  swarm-proto/    protocol types, framing, version negotiation
  swarm-shim/     per-session PTY holder, ring buffer, boundary index, snapshot
  swarmd/         supervisor, registry, client server, ssh bridge subcommand
  swarmctl/       CLI
  swarm-client/   async client library used by both swarmctl and the GUI
app/              the Warp fork (git subtree or separate repo pinned by commit)
```

## 12. Roadmap

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 – spike** | shim + `swarmctl attach` with raw passthrough (dtach-equivalent) | Run vim/htop, detach, reattach from another terminal |
| **M1 – daemon** | `swarmd` registry, protocol v1, multi-client, ring buffer, boundary replay, `ResumeFrom`, daemon restart re-adopts shims | Kill `swarmd`, sessions survive and re-appear |
| **M2 – GUI local** | Warp fork with `daemon_tty` `TerminalManager`, runtime manager selection, replay guard, idempotent bootstrap, startup restore, window⇒tab, close/detach semantics, first-run sheet, start-mode setting (`with-app` / `at-login` service install) | Quit app with 5 tabs, reopen, identical blocks restored |
| **M3 – remote** | `swarmd bridge`, ssh transport, provisioning for Linux and macOS hosts, per-host start mode, session picker dropdown, reconnect | Laptop sleep/wake against remote host resumes without visual reset |
| **M4 – polish** | alt-screen snapshots, tab drag-out, size policies, readonly attach, settings, optional "restore after reboot" (recreate sessions with the same name/cwd/tab order, without the processes) | — |

M0–M1 are independent of Warp and de-risk the hard part; M2 is where most of the unknowns are.

## 13. Risks

1. **Codebase size and upstream churn.** Warp is about 1.8M lines and moves fast, so rebasing the fork will be ongoing work. Keep the diff concentrated in `daemon_tty`, the runtime selection in `pane_group`, the replay guard and feature flags. Rebase on a fixed schedule rather than tracking `main` continuously.
2. **Warp's shell-integration assumptions beyond bootstrap.** Some per-session state (history, completions, session id) is fetched or set through the PTY when a shell starts (§8a). Each piece must either be stored in the shim or re-derivable after reattach. This is the first task of M2.
3. **Replay must never write to the PTY.** This covers terminal queries (DA, cursor position reports) and Warp's `InitShell` hook alike. For replayed bytes the client must neither answer queries nor run hooks; it does so only for live bytes. Replay frames are marked, and `daemon_tty` gates everything the model would send back to the PTY.
4. Ring buffer memory with many sessions: cap global memory, evict from idle sessions first.

## 14. Open questions (need decisions)

None at the moment.

Decided:
- **Full Warp fork for the GUI, released under AGPL v3** (§8a). Building our own client on only the MIT crates (`warpui`) would mean rewriting the terminal model, blocks and input editor, which are the reason for choosing Warp. The fork is integrated through a new `daemon_tty` `TerminalManager`.
- **Daemon-side crates (`swarm-proto`, `swarm-shim`, `swarmd`, `swarmctl`, `swarm-client`) are separate programs talking over a socket protocol.** They don't depend on or copy Warp code, so they can use a permissive license (MIT/Apache-2.0). For the shim's terminal model use upstream `alacritty_terminal` / `vte` (Apache/MIT), not Warp's AGPL-licensed fork. `swarm-client` must stay permissive as well, since the AGPL app links it, which is allowed.
- Remote (and local) daemon hosts in v1: Linux and macOS (§9a).
- Daemon start mode is a per-host setting: `with-app` (default) or `at-login`; chosen on first run for the local machine and on first connect for each remote host (§3, §3a, §9).
- Tab close = **detach**; terminating is explicit (§8, item 6).
- Follows from that: on launch, only sessions whose tabs were open at quit are re-attached; detached ones wait in the picker (§8, item 5).
