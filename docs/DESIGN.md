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
1. Client (GUI, `swarmctl`, or `swarmd bridge`) tries `$RUNTIME_DIR/swarmd.sock`.
2. If absent: runs `swarmd start`, which spawns `swarmd run` with `setsid`, stdin/stdout → `/dev/null`, stderr → `swarmd.log`, and waits until the socket answers. This happens in both modes, as a fallback in `at-login` mode in case the service is not running.
3. `at-login` is installed/removed by `swarmd service install|uninstall|status` (`swarmctl daemon install-service` / `uninstall-service` call it; the GUI will too). It writes `~/Library/LaunchAgents/dev.swarm.swarmd.plist` (macOS) or `~/.config/systemd/user/dev.swarm.swarmd.service` (Linux), both running `swarmd run --if-not-running`. Both are user-level and need no admin rights. launchd restarts the daemon only after a crash (`KeepAlive.SuccessfulExit = false`), not after `swarmd stop`; systemd uses `Restart=on-failure` and `KillMode=process` so a daemon restart never kills shims. `SWARM_RUNTIME_DIR` / `SWARM_STATE_DIR` set at install time are written into the service. Once the app ships as a bundle, macOS registration moves to `SMAppService` (§9b); the CLI path stays for headless and remote Macs.
4. **Linux remote hosts:** sessions only survive the user's ssh logout if systemd does not kill the user's processes at logout. `loginctl enable-linger` keeps them alive. On most systems a user can enable it for themselves; if the host forbids it, the GUI shows "Sessions on this host will end when you log out" instead of failing silently. In `with-app` mode the daemon is started via `setsid` from the ssh session, which survives logout unless the host sets `KillUserProcesses=yes`; the same check and warning apply.
5. Single-instance guaranteed by an `flock` on `$RUNTIME_DIR/swarmd.lock`. `swarmd run --if-not-running` exits 0 when the lock is held.
6. `swarmd run` and every shim close all inherited file descriptors above stderr as their first action, so a long-lived process never keeps its starter's pipes or terminal open.

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

As built in `crates/swarm-proto` (protocol version 1).

- Transport: a byte stream (unix socket locally; `ssh host swarmd bridge` stdio remotely). Framing: big-endian `u32` length, then a `postcard`-encoded message (max 16 MiB). The first frames are `Hello` / `Welcome`; a protocol mismatch closes the connection.
- One client connection multiplexes requests (with request ids), an event subscription and any number of attachments (`stream` id each), so a remote host needs only one ssh channel.

### Client ⇄ swarmd
```
ClientMsg::Hello { proto, client_name, client_id }      → ServerMsg::Welcome { proto, daemon_version, hostname, os }
Request::Ping                                           → Pong
Request::List                                           → Sessions(Vec<SessionInfo>)
Request::Create(CreateOpts)                             → Session(SessionInfo)      (empty command = login shell)
Request::Kill { id, signal }                            → Ok
Request::Remove { id }                                  → Ok   (SIGHUP, then SIGKILL after 3 s; returns once the session is gone)
Request::SetMeta { id, MetaUpdate { name, ui } }        → Session(SessionInfo)      (ui keys with "" values are removed)
Request::Attach { id, mode, size, readonly }            → Attached { stream, info, replay_from, live_from }
Request::Subscribe                                      → Ok, then ServerMsg::Event(Created | Updated | Removed)
Request::Shutdown                                       → Ok   (daemon exits; sessions keep running)
```
`AttachMode` is `Full` (replay from the oldest buffered boundary), `ResumeFrom(offset)` (falls back to `Full` if no longer buffered) or `Live`.

### Per attachment (relayed by swarmd)
```
→ ClientMsg::Input { stream, data }    keystrokes / paste, raw
→ ClientMsg::Resize { stream, size }   see §7
→ ClientMsg::Detach { stream }
← ServerMsg::Output { stream, offset, data, replay }   raw PTY bytes; replay = true for buffered history
← ServerMsg::Exited { stream, code, signal }
← ServerMsg::Lagged { stream, resume_at }              client fell behind; re-attach with ResumeFrom(resume_at)
← ServerMsg::StreamClosed { stream, reason }
```
A `Snapshot` message for full-screen apps (§6) is not implemented yet (M4).

### swarmd ⇄ shim
Each request opens its own connection to `$RUNTIME_DIR/shims/<id>.sock`: `Info`, `Watch` (the shim pushes `Changed(SessionInfo)` on every significant change, and activity-only changes at most once a second), `Attach` (then `ShimInput` one way and output messages the other), `Kill`, `Remove`, `SetMeta`. The shim owns all session metadata, including name and UI hints, so `swarmd` keeps no state of its own: on start it scans `shims/` and re-adopts every shim that answers, deleting stale sockets. `swarmd` relays attachment frames between the client connection and the shim connection without interpreting the bytes.

## 6. Output, scrollback and reattach — the core problem

Warp's blocks, prompt detection and input editor depend on **escape sequences emitted by its shell integration** (prompt/command start/end markers, metadata in DCS/OSC). A tmux-style design that re-renders the screen would strip those, so Warp would degrade to a dumb grid. Therefore:

**The daemon is byte-transparent.** The client receives exactly the bytes the PTY produced, and Warp's own parser runs on the client as it does today.

Each shim keeps:
1. **Raw ring buffer** of PTY output (default 16 MB, configurable per session) addressed by a global `u64` byte offset.
2. **Boundary index**: offsets of safe replay start points, found by a lightweight `vte` scan (no full emulation). Built so far: offset 0, OSC 133;A prompt-start markers (pointing at the ESC that begins them) and the byte after every alt-screen exit. Warp's own block markers are added in M2. The same scan tracks the title (OSC 0/2), cwd (OSC 7) and alt-screen state.
3. **Server-side terminal model** (planned, M4) (upstream `alacritty_terminal`, the same code Warp's grid model was derived from; we use upstream rather than Warp's AGPL-licensed copy, see §14) used for two things only: knowing whether we're in alt-screen, and producing a screen snapshot for full-screen apps.

Reattach algorithm (`Attach { mode: Full }`):
- **Normal (primary screen) case:** pick the oldest boundary still inside the ring buffer → send `Output` from there. Warp's parser replays the bytes and reconstructs the blocks exactly as if it had been attached the whole time. Older history beyond the ring is lost (shown as "earlier output truncated").
- **Alt-screen case (vim, htop, less):** replay the primary-screen history up to the alt-screen enter boundary, then send a `Snapshot` of the current alt screen (cells + attributes + cursor + modes), then live bytes. *Built so far:* the full replay (which still contains the alt-screen enter while it's buffered) plus a repaint nudge: on a `Full`, non-read-only attach to a session in alt-screen, the shim shrinks the PTY by one row for 50 ms and restores it, so the app redraws. Verified with vim and htop. The snapshot covers the case where the alt-screen enter has scrolled out of the buffer.
- **Network blip (`ResumeFrom(offset)`):** if `offset` is still in the ring, send only the delta — no visual reset at all. This makes remote reconnects seamless.

**Replay must not be answered.** Replayed output can contain terminal queries (device attributes, cursor position, colors). The attaching terminal answers them, and those answers must not reach the live program. `swarmctl` drops terminal-response sequences (CSI reports ending in `c`, `n`, `y`, `t`, `R`, private `u`; OSC and DCS replies) from its input for 250 ms after the last replayed chunk, and passes everything else, so typing right after attaching works. The GUI does the same at the model level by marking replay frames (§8a).

**Exit.** When the child exits, the shim waits up to 500 ms for the PTY reader to drain, so `Exited` always follows the final output, then keeps the buffer for 10 minutes (or until `Remove`) before it exits and removes its socket.

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

**Shell hooks (checked 2026-10-30, upstream `89de3f5`).** Warp's lifecycle hooks are DCS sequences `ESC P $ d <hex-encoded JSON> ESC \` (OSC 9278 on Windows), defined in `crates/warp_terminal/src/model/ansi/dcs_hooks.rs`: `InitShell`, `Bootstrapped`, `Precmd`, `Preexec`, `CommandFinished`, `InputBuffer`, `Clear`, `InitSubshell`, `SSH`, `ExitShell` and a few more. Consequences for persistent sessions:
- **Session id.** The app generates a random `u64` session id per shell and substitutes it into the init script (`@@WARP_SESSION_ID@@`). Hooks that change state are rejected unless that id is registered with the model (`TerminalModel::register_session_id`). `daemon_tty` therefore stores the id in the session's UI metadata (`ui["warp.session_id"]`) when it creates the session, and registers it with a fresh model before replay.
- **`Bootstrapped` is a one-time snapshot** of the shell: histfile, PATH, home, aliases, abbreviations, function and builtin names, env var names, shell version and options, plugins, vi mode, OS/distro. It's emitted once, right after bootstrap, so in a long session it scrolls out of the 16 MB ring and a later replay would lose it. **The shim keeps the raw bytes of the latest `InitShell` and `Bootstrapped` hooks outside the ring** and, on a `Full` attach whose replay starts after them, sends them first as replay frames. (Replay frames never cause the init script to be written, §13 risk 3.)
- **Block boundaries.** `Precmd` marks where Warp starts a new block, so the shim's scanner treats a `Precmd` hook as a replay boundary, the way it already treats OSC 133;A. The hex JSON always starts with `{"hook": "Precmd"`, so the scanner can match that hex prefix without decoding the payload.
- **Backend selection.** `app/src/pane_group/mod.rs` chooses `RemoteTtyTerminalManager` or `LocalTtyTerminalManager` in a compile-time `cfg_if!`. `daemon_tty` goes in at that point as a runtime choice.

**Building the fork (checked 2026-10-30 on an M-series Mac, macOS 27, Xcode with Rust 1.92 from `rust-toolchain.toml`).** The fork lives in `app/` as its own git repo (`upstream` = warpdotdev/warp, cloned at `89de3f5` with `--filter=blob:none`), ignored by the swarm-term repo. It's pinned by commit and becomes a submodule, or a GitHub fork, once it carries changes.
- Prerequisites actually needed: full Xcode as the active developer dir, Xcode's Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`, ~840 MB), `protoc`, `pkg-config`, LLVM, `jq`, and `cargo-bundle` 0.11.0. Warp's `script/bootstrap` also installs Docker, gcloud, PowerShell, Sentry tooling and runs `sudo xcode-select`; none of that is needed for local builds.
- `cd app/app && cargo build --bin warp-oss --features gui` (the OSS channel binary): **3 min 32 s clean**, **~20 s incremental** after touching either the app crate or `warp_terminal`. The target dir is 12 GB; the debug binary is 847 MB.
- `WARP_SKIP_COMMON_SKILLS_INSTALL=1 ./script/run --dont-open` bundles and ad-hoc signs `target/debug/bundle/osx/WarpOss.app` in 36 s. It skips the Warp-internal `warp-channel-config` on its own.
- **The unchanged build runs.** It starts its `terminal-server` helper and opens a "Welcome to Warp" onboarding screen. No shell starts until onboarding is finished. Onboarding offers "skip login", which creates no account (`AuthManager`, "The user chose to skip login entirely"). For swarm-term, skipping login becomes the default and Warp's cloud sign-in becomes opt-in (§8 item 7).

**Not reusable for us.** Warp's `remote_server` crate is a protocol for code, diff and repo features on SSH hosts. It doesn't hold PTYs.

### Swarm is its own app, built on Warp's engine (decided 2026-10-30)

We use Warp's engine (terminal model, blocks, input editor, renderer, shell integration) but the product is **Swarm**, not Warp: its own name and icon, and the look from the mockups (§9b). In the fork that means:
- no Warp welcome, onboarding or login screen (done: `app/src/swarm.rs`, `SKIP_WARP_ONBOARDING`); Warp's cloud sign-in, Drive and agent features are off or opt-in;
- Swarm's first-run sheet (§3a) instead of Warp's onboarding;
- the host-grouped tab bar and the sidebar layout, session-state marks, the session picker, the connect-to-host sheet, the menus and the menu bar extra exactly as mocked, replacing Warp's equivalents;
- Swarm's Settings window with the General / Sessions / Daemon / Hosts panes;
- bundle name, bundle id, icon and window titles say Swarm, never Warp.

Fork-specific code lives in `app/src/swarm*` and `crates/warp_terminal/src/local_tty/daemon.rs` where possible, so the diff against upstream stays small and easy to find.

### How sessions reach the daemon (built 2026-10-30)

Instead of a separate `daemon_tty` manager, the daemon plugs in one level lower, at `PtySpawner`. A separate manager would have to re-implement the ~1,500 lines of `local_tty` (shell detection, init script, recovery, banners). Building at the spawner means everything above it runs unchanged:
- `PtySpawner::spawn_pty` first asks `local_tty::daemon::DaemonSpawner`. That builds exactly the command upstream would spawn (`build_host_shell_command`), creates a `swarmd` session with that argv/env/cwd, attaches, and returns **one end of a socket pair as the "leader fd"**. A bridge task on a small tokio runtime moves bytes between the other end and the attachment. If the daemon can't be reached, Warp spawns the shell itself as before. `SWARM_DAEMON=0` turns the daemon off.
- `PtyHandle` gained `resize()`. `Pty::on_resize` uses it instead of `TIOCSWINSZ`, which fails on a socket.
- The daemon reports the child's exit; the bridge stores the status for `process_termination_status` and sends `SIGCHLD` to the app, which is how Warp notices exits.
- `PtyHandle::kill` only **detaches**, so closing a tab or quitting leaves the session running (§8, item 6).
- Session metadata written at creation: `created_by=warp`, `warp.session_id`, `warp.shell`, `auto_attach=true`.
- `scripts/bundle-app.sh` builds the app bundle with `swarmd`, `swarm-shim` and `swarmctl` in `Contents/MacOS`.

**Reopening sessions on launch (built 2026-10-30):**
- With daemon sessions on, Warp's own window/tab restore is skipped (it would start duplicate shells). At launch the app lists daemon sessions with `created_by=warp`, `auto_attach=true` that are still running, and opens a window with `NewWorkspaceSource::Reattach { tabs }`: one tab per session, ordered by `ui["tab_order"]`, then creation time.
- Each such tab is an ordinary new terminal tab whose `NewTerminalOptions.env_vars` carries `SWARM_REATTACH_SESSION_ID` and `SWARM_REATTACH_WARP_SESSION_ID`; those already travel to the spawner, so no signatures changed. `on_shell_determined` registers the saved Warp session id instead of a new one and skips the init script; `DaemonSpawner` attaches (`Full`) instead of creating and strips both keys so they never reach a shell; shell recovery gets the env without them.
- `PtyController::initialize_shell` returns early for re-attached session ids, so the replayed `InitShell` hook updates the model without re-running shell setup. The bridge drops terminal answers to replayed queries.
- The shim keeps the latest `InitShell` and `Bootstrapped` hooks outside the ring and sends them first on a `Full` replay that starts after them, and treats `Precmd` as a replay boundary (e2e test `replay_keeps_warp_session_hooks_and_starts_at_precmd`).
- Closing a tab detaches and sets `auto_attach=false` (the session waits in the picker). Quitting the app sets a flag first (`PtySpawner::prepare_for_app_termination`), so tabs detached during quit stay `auto_attach=true`.

Verified in the bundled app: launch → run a command → quit → the session keeps running with `auto_attach=true` → relaunch → the same session comes back as the tab with its earlier blocks rebuilt, no second session, no repeated shell setup, and new commands render as new blocks.

**Known gaps:** after a reattach the prompt's folder shows `/Users/<me>` instead of `~` (the session's home-dir info isn't fully restored), a stray `%` appears in the first replayed block, and replayed block durations are wrong. Tab order isn't recorded yet (`tab_order`), only creation order. The Swarm UI (picker, host groups, first-run sheet, branding) isn't built.

### Swarm identity and UI build plan (2026-10-30)

**Done:** the fork has its own `swarm` binary (`app/src/bin/swarm.rs`, Warp's OSS channel underneath) bundled as **Swarm.app**: bundle id `dev.swarm.Swarm`, URL scheme `swarm`, log `~/Library/Logs/swarm.log`, its own data dirs, and the Swarm icon (drawn by `scripts/make-icon.swift`). The app menu and "Set Swarm as Default Terminal" use `swarm::APP_NAME`. `scripts/bundle-app.sh` builds Swarm.app with `swarmd`, `swarm-shim` and `swarmctl` inside. `warp-oss` stays untouched.

**Next: replace Warp's interface with the mocked one, piece by piece.** Each row names the Warp view it replaces:

| # | Swarm piece (mockup) | Warp code it replaces or extends |
|---|---|---|
| 1 ✅ (Connect… comes with M3) | Title bar: sidebar toggle, window title, "Connect…", "N detached" | workspace header in `workspace/view.rs` (search box "Search sessions, agents, files…", AI/inbox/settings icons) |
| 2 | Sidebar layout: host groups with counts, state marks, folder/command, last activity, "attached elsewhere" dot, filter, `+`, footer buttons | vertical tabs panel, `workspace/vertical_tabs.rs` |
| 3 | Tabs layout: host-grouped tab bar with state marks | horizontal tab bar (`workspace/tab_group.rs` and header) |
| 4 | Session picker popover (host switcher, filter, multi-select, open/terminate) | new view, backed by `swarm-client` |
| 5 | Thin status bar (host · shell · cwd · size · who else is attached) | new element below the terminal pane |
| 6 | File / Session / View / Window menus and shortcuts (§9b) | `app/src/app_menus.rs` |
| 7 | Settings: General, Sessions, Daemon, Hosts; Warp's AI, billing, agents and Drive pages hidden | `app/src/settings_view/` |
| 8 | First-run sheet (§3a) | replaces Warp's onboarding in `root_view.rs` (currently skipped) |
| 9 | Menu bar extra | new `NSStatusItem` in `crates/warpui/src/platform/mac` |
| 10 | Warp's AI, Drive and cloud surfaces off by default; debug-build "Lines: … Size: …" block footer off in Swarm | feature flags / `swarm.rs` |

Order: 1 → 2 → 10 → 4 → 6 → 3 → 5 → 7 → 8 → 9.

**Title bar (done 2026-10-30):** with `swarm::SWARM_CHROME`, the vertical-tabs title bar shows only the sidebar toggle, the active tab's title centered, and "N detached" while sessions are running with no client attached. The count comes from the `SwarmSessions` model (`app/src/swarm/sessions.rs`), which follows the daemon through `daemon::watch_sessions` (subscribe, coalesced re-list, reconnect), so it updates live. Clicking it opens a menu of detached sessions. Picking one opens a tab re-attached to it and sets `auto_attach=true` again. Every running session can be opened: `ReattachTab::for_session` re-attaches Swarm's own sessions as before. An idle zsh, bash or fish started elsewhere (e.g. `swarmctl new`) gets Warp's normal setup script typed into it under a new Warp session id, which is then stored on the session (`warp.session_id`, `created_by=warp`), so it shows blocks from then on and re-attaches the normal way. Verified: `WARP_BOOTSTRAPPED=1` and the matching `WARP_SESSION_ID` inside the shell, and the tab shows its folder and git branch. Anything else (a running program) opens without setup and is marked "(no blocks)" in the menu. Launch uses the same rule: every running session with `auto_attach=true` reopens, whatever started it.

**Window close vs. tab close (2026-10-30).** Only an explicit tab close (`Workspace::close_tabs`) marks its sessions `auto_attach=false`, matched by the Warp session id of each closed terminal (`daemon::forget_sessions`). Closing a window or quitting just detaches. Opening a window (launch, Dock click, New Window) goes through `root_view:open_new`, which first re-attaches running sessions that are marked to reopen and attached nowhere; only if there are none does it start a fresh shell. Warp's undo-close hold (closed tabs/windows kept alive 60 s) is skipped with daemon sessions, since it kept closed terminals attached. Verified by driving the app with real clicks and keystrokes:
- type a command → close the window with the red button → click the Dock icon → the same session comes back with its output;
- Cmd+T, type, Cmd+W that tab → its session detaches with `auto_attach=false`; Cmd+Q quits with no dialog; relaunch → only the first session reopens, and the title bar shows "1 detached".

**Swarm, not Warp, in the UI (2026-10-30).** About 125 user-visible strings that mean "this app" now say Swarm: banners ("Did you know that Swarm can directly display Markdown files?" / "View in Swarm"), notifications, window titles, menus, settings and dialogs. Text about Warp's own services (Warp AI/Agent/Drive, plans, billing, teams, referrals, Warp's servers and plugins, privacy policy) keeps the Warp name and will be hidden with those features. Telemetry, logs and internal identifiers are unchanged to keep the upstream diff small.

**Naming sessions (2026-10-30).** Renaming a tab (tab menu, double-click, or the palette's "Rename the Current Tab") names its daemon session (`PaneGroup::set_title` → `daemon::name_sessions`), so the name shows in `swarmctl ls` and the detached menu, and a reopened or reattached tab is titled with it. Verified in the app: rename → `swarmctl ls` shows "nightly-backup" → quit → relaunch → the tab is titled "nightly-backup". Not yet: `swarmctl rename` doesn't retitle an open tab live.

**Session groups (2026-10-30).** Named groups of sessions (like "Demo" / "Work") are Warp's tab groups (`FeatureFlag::GroupedTabs`, on by default): "New group with tab", "Move to group", "New tab in group", rename, ungroup, collapse, pin and colors all come from Warp. Swarm makes a group part of its sessions:
- Every time the app saves its state, `swarm::sync_session_groups` stores each terminal's group on its daemon session as `group=<name>` (or `#<group id>` for an unnamed group). Only changes are sent, and leaving a group removes the key.
- Reopening or reattaching a tab (`Workspace::add_swarm_tab`) puts it back in the group of that name, creating the group if needed. A tab that had no group is taken back out of the group Warp would otherwise have it inherit from the active tab.
- Groups are per session, so they work the same for remote sessions, and `swarmctl info`/`meta` show and set them.
- Fixed while testing: after "New group with tab" the name editor lost focus to the new tab's pane, so typed text went to the shell. The rename now starts once the pane has focus.
Verified in the app: create "builds" with two tabs, quit → relaunch → "builds" comes back with the same two tabs, a loose tab stays loose, an unnamed group stays its own group, and "Ungroup tabs" removes `group` from the sessions.

Known issues (investigated 2026-10-30):
- **Folder chip shows `/Users/<me>` instead of `~` after reattach.** The replayed `Precmd` hooks are processed before the session finishes bootstrapping (Sessions handles `Bootstrapped` asynchronously), so the prompt is first computed without a session. The stored and replayed Warp session ids match, and re-running `refresh_warp_prompt` on `SessionBootstrapped` didn't help, so the input's folder chip is cached elsewhere (`context_chips/directory_fetcher.rs` path). Cosmetic; paths are correct.
- **Stray zsh `%` at the end of replayed blocks: fixed.** Before each prompt zsh prints a standout `%`, one line of spaces and `\r \r`. At the original width that erases itself; replayed into a narrower screen the spaces wrap and leave the `%`. The bridge now drops these marks from replayed output only (`swarm_client::EolMarkFilter`). It handles marks split across chunks (tested at every split point), flushes at `live_from`, and leaves live output alone. Verified: reattached tabs show no `%` after their blocks. Recording the PTY size in the replay stream would still help full-screen programs replayed at another width.
- **Command history in a reattached tab:** history is loaded from the histfile on bootstrap. The empty History panel seen once was opened by a stray Up-arrow during relaunch and showed "No results" before loading finished; not reproduced.

**No "processes are running" warnings (2026-10-30).** Warp asks before quitting or closing a tab/window when commands are running, because for Warp that kills them. In Swarm they keep running in the daemon, so `quit_warning` no longer counts running commands when daemon sessions are on. It still warns about unsaved files and live shared sessions, which closing does lose. Title: "Quit Swarm?". Verified: with a counter loop open in a tab, quitting takes ~1 s with no dialog. Across 15 s with the app closed the loop produced 128 more lines, and the reattach replayed `n=0..554` with no gaps. "Connect…" isn't shown until remote hosts work (M3), rather than as a dead button. The title bar and sidebar are the first thing users see, and the picker is what makes detached sessions reachable.

### Warp's own services out of the app (2026-10-30)

- AI is already off: Warp disables it for logged-out users, and Swarm skips sign-in. Warp Drive commands don't appear either.
- **Settings** lists only Swarm's own pages: Appearance, Features, Keyboard shortcuts, Warpify, Privacy, Editor and Code Review, About (`swarm::shows_settings_section`). Account/sign-up/plans, Agents, Billing, Cloud platform, Teams, Referrals, Shared blocks and Warp Drive are gone, and any request for a hidden page (Cmd+, defaults to Account in Warp) opens Appearance.
- The palette no longer offers Warp's Oz cloud-agent CLI install; "Warp completions" is "Swarm completions".
- The debug-build "Lines: … Size: …" block footer is off in Swarm.
- "Share session" (live sharing through Warp's servers) is gone from the tab, pane and terminal menus (`swarm::DISABLED_WARP_FEATURES`, plus a direct check, because the flag was still being turned on after startup).

Next for Settings: the Sessions page from the mockup (§9b).

### Settings: Daemon and Hosts (built 2026-10-30)

Two Swarm pages lead the Settings sidebar (`settings_view/swarm_pages.rs`):
- **Daemon** shows this Mac's `swarmd`: version, hostname, OS/CPU, and live session counts from the `SwarmSessions` model. It has:
  - "Start the daemon at login", which runs `swarmd service install|uninstall` and reads `swarmd service status`;
  - "Restart daemon": `Shutdown`, then reconnect, which starts it again;
  - "Open logs" (the log dir in Finder) and "Refresh".
  Status is re-read whenever the page is opened.
- **Hosts** lists the remembered hosts (`$STATE_DIR/hosts`). Each host is checked over ssh when the page is opened (not at launch): reachable or ssh's error, swarmd installed, version, session counts. Each host has:
  - an at-login switch (`swarmd service …` over ssh);
  - "Install/Reinstall swarmd", offered only when the app has binaries for that host's OS/CPU. It copies to a staging dir and renames into place, so a running daemon's files can be replaced, then restarts the daemon;
  - "Remove", which stops reopening its sessions and closes the connection; the sessions keep running there.

Verified in the app:
- Restart daemon → new `swarmd` pid, all 4 sessions kept, every tab "reconnected … after 1 attempt(s)".
- On the VM, the at-login switch moved `orb` between at-login and with-app, and its session survived both. The Linux unit uses `KillMode=process`, and shims call `setsid`, so stopping the service never takes sessions with it.
- Remove dropped `orb` from the hosts file and left its session running.

Testing note: macOS permission prompts (UserNotificationCenter) float above other windows and take keyboard focus. While one was up, synthetic clicks under it and all keystrokes went to the prompt, which looked like dead buttons and missing focus in Swarm.

### Menus (built 2026-10-30)

Swarm's menu bar (`app_menus.rs`, under `SWARM_CHROME`):
- **App:** About Swarm, Settings (⌘,), Keyboard Shortcuts, Appearance, Services, Hide Swarm, Set Swarm as Default Terminal, Quit Swarm. Warp's account, referral, resource center, changelog and privacy items are gone, and warpui's standard items no longer say "Warp".
- **File:** New Window, New Session (⌘T), New Session on ▸ (This Mac, then each remembered host), New File, Launch Configurations, Connect to Host…, Detached Sessions…, Open Repository, Close Tab (⌘W; the session keeps running), Close Window. "New Agent Tab" and "Reopen closed session" are gone.
- **View:** Show or Hide Sessions Sidebar, then Warp's palettes, panels and zoom.
- **Session** (Warp's Tab menu): Rename Session…, splits, moving and switching tabs, closing.
- **Window** and **Help**. Help links to the GitHub repo, design notes and issues. The AI and Drive menus are not shown.

Swarm items act on the active window through a `root_view:workspace_action` global action. Verified with real clicks:
- File → Connect to Host… opens the hosts list.
- File → New Session on ▸ orb opens a new session on the VM.
- View → Show or Hide Sessions Sidebar switches between sidebar and tabs.

As in Warp, menu shortcuts like ⌘, need a focused view in the window; they do nothing (the item is disabled) when nothing in the window has focus.

### Sidebar or tabs (built 2026-10-30)

The sidebar button (⌘B) switches between the two layouts from §9b. With the sidebar shown, sessions are listed in it (Warp's vertical tabs panel). With it hidden, sessions become tabs across the title bar (Warp's horizontal tab bar), and the button stays at the left of the title bar to bring the sidebar back. Implemented by having the toggle switch Warp's `use_vertical_tabs` setting, so every part of Warp's layout follows, and the choice is remembered. The Swarm title bar controls (Connect…, N detached) appear in both layouts. Verified in the app: sidebar → tabs → sidebar.

### Remote hosts in the app (built 2026-10-30)

- **Connections.** `local_tty::daemon::Daemons` holds one client per host on a shared runtime: `None` is the local daemon, `Some(alias)` is `ssh <alias> swarmd bridge`, reconnecting on demand. The spawner, the session watchers, renames and tab-close updates all go through it.
- **Remote tabs** reuse the reattach path: `ReattachTab { host, … }` adds `SWARM_REATTACH_HOST`. "New session on <host>" creates a login shell there (`daemon::create_session`), then opens it like an idle shell started elsewhere: Warp's setup script is typed in, and its Warp session id is stored on the session. Warp classifies the session as `WarpifiedRemote` by hostname, so completions and prompt chips run on the server (the in-band executor), not on the Mac. A remote tab reports pid 1, since the remote shell's pid means nothing locally.
- **UI.** "Connect…" in the title bar lists hosts from `~/.ssh/config` and its `Include`s (quoted paths with spaces handled). Picking one runs `check_host` (`ssh -o BatchMode=yes`), and then:
  - lists that host's sessions plus "New session on <host>";
  - or offers to install `swarmd` (copying the app's own binaries when the host has this Mac's OS/CPU);
  - or explains: ssh's own error, or that there's no bundled binary for that OS.
- **Dropped connections.** When a remote stream closes, the bridge reconnects with backoff (up to ~40 tries) and resumes with `ResumeFrom(last offset)`, unless the tab was closed or the session is gone.

Verified against the Linux VM (`orb`):
- Connect → `orb` → New session opened a bash tab on the VM with Warp's prompt (user chip `naotar@swarm-test`), and a command ran there (`remote-Linux-swarm-test-42`).
- Killing the app's ssh connection: "connection to daemon lost" → "reconnected … after 1 attempt(s)" about 6 s later.

**Reopening remote tabs at launch.** A successful Connect remembers the host alias in `$STATE_DIR/hosts`. The first window of a run then asks each remembered host, in the background, for running sessions marked to reopen that nothing is attached to (`daemon::reopenable_sessions`, 20 s timeout) and adds them as tabs, keeping focus on the current tab; unreachable hosts are skipped and logged. This is simpler than the earlier "Reconnect to <host>" prompt idea, and closing a remote tab still means "don't reopen". Verified with the VM: open a remote tab → quit → relaunch → the remote session comes back as a tab next to the local one.

**Local or remote is decided by the daemon, not the hostname (2026-10-30).** Warp guesses a remote session when the shell's hostname differs from the Mac's, which misfires when macOS changes its own hostname (e.g. `Mac.localdomain` vs `MacBook-Pro-5.local`): local tabs were treated as remote. With daemon sessions the spawner knows which daemon a session lives on and records it (`daemon::set_session_locality`), and `determine_session_type` asks that first. Remote tabs are titled by host alias until named.

**Linux binaries in the app (2026-10-30).** `scripts/build-linux.sh` builds static (musl) `swarmd`, `swarm-shim` and `swarmctl` for Linux aarch64 and x86_64, both linked with rust-lld, so they run on any distribution. On a Mac it builds in the OrbStack Linux machine, which sees this checkout at the same path. `scripts/bundle-app.sh` puts them in `Swarm.app/Contents/Resources/swarmd/<os>-<arch>/`. Installing on a host (Connect… or Settings → Hosts) asks the host for `uname -sm`, maps it to swarmd's `<os>-<arch>` name (`linux-aarch64`, `macos-aarch64`, …), and copies the matching set: the app's own binaries for this Mac's platform, else the bundled Linux ones. Verified: Hosts → Reinstall swarmd on `orb` replaced its 44 MB debug glibc binaries with the ~4 MB static ones and restarted the daemon there, and both remote sessions reconnected after one attempt. The x86_64 build also runs (under emulation in the VM).

**Not done yet:** no "Reconnecting…" indicator on the tab; host labels and colors on tabs; remote session labels use the shell's window title, which bash sets to the last command.

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
| `at-login` service | systemd `--user` unit, `KillMode=process` | Local Mac with the app installed: `SMAppService.agent` registering a LaunchAgent plist bundled in the app (§9b). Remote Mac without the app: plist in `~/Library/LaunchAgents`, `ProcessType=Interactive` |
| Survives ssh logout | needs linger unless `KillUserProcesses=no` (§3 step 4) | yes, `setsid` processes started over ssh survive logout |
| Remote prerequisite | sshd running | "Remote Login" enabled in System Settings → General → Sharing |

macOS notes:
- **Socket path must be the same for GUI and ssh logins.** `$TMPDIR` can differ or be missing in an ssh session, so `swarmd` always resolves the per-user dir with `confstr`, never from env. Keep the full socket path under macOS's 104-byte `sun_path` limit; fall back to `/tmp/swarm-$UID/` if it would exceed it.
- **`at-login` on a remote Mac:** a LaunchAgent in the GUI domain only loads when the user logs in at the screen. On a headless Mac (ssh only), the installer bootstraps it into the `user/<uid>` domain instead (`launchctl bootstrap user/$UID …`), and the GUI shows that it will start on the first ssh connection after reboot unless someone logs in. `with-app` is the recommended default for remote Macs.
- **Gatekeeper:** a binary copied over ssh/scp gets no quarantine flag, so it runs, but arm64 requires a valid signature; release builds are Developer ID signed (ad-hoc signing is enough for dev builds).
- **Sleep:** a sleeping remote Mac drops connections; the client's reconnect + `ResumeFrom` (§6) covers it. Optional setting for that host: run `caffeinate -s` while sessions are attached.
- **Full Disk Access / TCC:** shells started by `swarmd` inherit the daemon's privacy permissions, not the GUI's. Commands touching protected folders (Desktop, Documents, removable volumes) may prompt or fail; document granting access to `swarmd` if needed.

## 9b. The macOS app and its user interface

The Mac app is the primary product, so its quality bar is "feels like a first-class Mac app", not "a Linux terminal that runs on a Mac". This section sets the rules for that.

### What Warp gives us and what we add
Warp's UI framework (`warpui`, MIT) on macOS renders with **Metal** and already uses native **menus** (`NSMenu`), **alerts**, **notifications**, clipboard and an **accessibility** layer. Tabs, the terminal and the input editor are custom-drawn, which is fine for a terminal. What it doesn't have, and we add in `warpui`'s mac platform layer (MIT, so these can go upstream):

| Missing | Why we need it |
|---|---|
| Menu bar extra (`NSStatusItem`) | Sessions keep running when the app is closed. The menu bar icon shows that and opens sessions without opening the whole app. |
| Login item via `SMAppService` | `at-login` daemon mode is registered the way macOS expects, and it appears correctly in System Settings → Login Items. |
| Sparkle 2 updates | Warp's updater talks to Warp's servers; we need our own feed. |
| Vibrancy (`NSVisualEffectView`) for the sidebar/popovers (optional) | Matches the look of current macOS apps; behind a setting. |

### Principles
1. **Keyboard-first, mouse-complete.** Everything reachable from the keyboard; everything also discoverable through the menu bar (macOS users look there for features).
2. **Follow macOS conventions over Warp's** where they conflict: standard shortcuts, standard Settings window (Cmd+,), standard window restoration, light/dark following the system.
3. **Make the "sessions keep running" model visible.** This is the product's main difference from every other Mac terminal. Users must always be able to see what is running, where, and whether they are attached.
4. **Never lose work by surprise.** Nothing kills a process without an explicit, clearly-labeled action. Cmd+Q and closing a tab never prompt "processes are running", because nothing will be killed.
5. **Fast and calm.** No spinners for local operations; remote waits show progress inline in the tab, never as blocking modals.

### Main window
```
┌──────────────────────────────────────────────────────────────────────────┐
│ ● ● ●   [● api  ~/src/api] [● logs  tail -f] [◐ build ⟳] | ▣ devbox: [● db] [○ web]   +  ⌄ 3 detached │
├──────────────────────────────────────────────────────────────────────────┤
│                         Warp blocks + input editor                        │
│                                                                           │
├──────────────────────────────────────────────────────────────────────────┤
│ local · zsh · ~/src/api · 120×40                    also attached: mini-2 │
└──────────────────────────────────────────────────────────────────────────┘
```
- **Tabs are grouped by host.** Local tabs first, then one group per remote host with its name and color (the color also tints the tab's top edge). Groups can be collapsed.
- **`+`** creates a session on the host of the current tab. Holding Option, or the `⌄` next to it, picks a host.
- **"N detached"** opens the session picker for detached sessions (§8, item 6).
- **Thin status bar** (optional, on by default): host, shell, cwd, size, and who else is attached.
- Tabs can be dragged into their own window and back; every window shows the same kind of tab bar.
- **Two layouts for open sessions**, switched with the sidebar button in the title bar, View → Sessions as Sidebar (Ctrl+Cmd+S), or a setting for the default:
  - **Tabs** (default): the host-grouped tab bar above.
  - **Sidebar** (like Warp's vertical tabs): the tab bar is hidden and a left panel lists every open session, grouped by host with collapsible headers and counts. Each row shows the state mark, name, folder or running command, last activity, and the "attached elsewhere" dot. The panel has a filter field, a `+` for a new session, and "N detached" / "Connect…" buttons at the bottom. Clicking a row shows that session's terminal on the right. The panel can be resized and hidden; the choice is remembered per window.

### Session states (the visual language)
Each state uses a **shape and a color**, never color alone, so it works for color-blind users and in grayscale.

| State | Tab indicator | Meaning |
|---|---|---|
| Running, idle | `●` filled dot | Shell waiting at a prompt |
| Running a command | `◐` + spinner on long commands | Foreground process other than the shell |
| New output since last viewed | dot + bold title | Something happened in a background tab |
| Bell / needs attention | `!` badge | Bell or a command failed in the background |
| Exited | `○` hollow, dimmed title | Process ended; tab shows exit code with [Restart] [Close] |
| Reconnecting | dimmed tab + `⟳` | Remote connection lost, retrying |
| Attached elsewhere | small second dot | Another client is attached too |

### Session picker (local and remote)
A popover from "N detached", from the host group's menu, or with **Cmd+O**. It's the same component for local and remote sessions.
```
┌ Open sessions on devbox ──────────────── ⌕ filter ┐
│ ☐ ● db          ~/srv/db      psql       2m ago   │
│ ☐ ○ web         ~/srv/web     exited 0   1h ago   │
│ ☐ ● migrate     ~/srv         cargo run  now  ◉◉  │
│───────────────────────────────────────────────────│
│ + New session on devbox     Open all   [Open (2)] │
└───────────────────────────────────────────────────┘
```
- Type to filter; arrow keys and Space to select; Return opens; Cmd+Return opens all matches; Cmd+Delete terminates (with confirm).
- Rows show name, cwd, foreground command, last activity, state and other attached clients.
- Right-click on a row: Rename, Terminate, Copy attach command (`swarmctl attach …`).

### Connect to host
**Cmd+Shift+K** opens a sheet with hosts from `~/.ssh/config` and saved hosts, plus a field for `user@host`. The connect flow (§9) runs **inline in the sheet** as a short checklist: "SSH access ✓ → swarmd 0.4.1 ✓ → 5 sessions" with a clear message and fix if a step fails (for example, "Permission denied (publickey). Add your key with `ssh-add`, then retry"). The sheet then turns into the session picker.

### Menu bar (application menus)
| Menu | Items |
|---|---|
| **File** | New Session (Cmd+T), New Session on Host ▸, Open Session… (Cmd+O), Connect to Host… (Cmd+Shift+K), Detach Tab (Cmd+W), Terminate Session… (Cmd+Shift+W), Close Window |
| **Session** | Rename… , Tab Color ▸, Restart (when exited), Attach Read-Only, Copy Attach Command, Show Session Info (Cmd+I) |
| **View** | Sessions as Sidebar (Ctrl+Cmd+S), Show Status Bar, Collapse Host Groups, Warp's existing view items |
| **Window** | Standard items, plus a list of tabs grouped by host; Move Tab to New Window |

Cmd+N follows the "window ⇒ tab" decision (§8, item 4): it creates a new session tab in the front window. Every Warp default shortcut is audited against this table and macOS conventions before M2 ships.

### Menu bar extra
An optional menu bar icon (on by default) shows:
- daemon status per host (local, and each connected remote);
- sessions per host, with their state dot; clicking one opens the app on that tab;
- "New Session", "Open Swarm", "Settings…".

It's the reminder that sessions are still running when the app's windows are closed. The app keeps running in the background while the menu bar extra is enabled (like Mail or Slack), and quits fully when it's disabled. Sessions keep running in both cases.

### Notifications and Dock
- Notifications (opt-in per type): long command finished in a background tab; session exited with non-zero code; remote host disconnected and couldn't reconnect.
- Dock badge: number of tabs needing attention (bell, failure). Dock menu: list of sessions with attention, "New Session".

### Settings window (Cmd+,)
Standard toolbar-style Settings window with panes:
- **General** — on launch (restore tabs), menu bar extra, notifications;
- **Sessions** — default shell, scrollback size, exited-session retention, size policy;
- **Daemon** — start mode (`with-app` / `at-login`), status, restart daemon, open logs;
- **Hosts** — saved hosts, color, start mode, remove; "Reinstall swarmd";
- **Appearance / Keys / Privacy** — Warp's existing panes, with cloud features off by default.

### First run
One sheet (§3a): daemon start mode, optional "Connect to a server", and a single line explaining the core idea: *"Closing a tab or quitting Swarm doesn't stop your sessions. Use Session → Terminate to end one."*

### Accessibility
- VoiceOver labels for every new control (tabs announce name, host and state; picker rows announce all columns). Verify with VoiceOver on every new view before merging.
- Full keyboard navigation of the picker, connect sheet and settings.
- Respect Reduce Motion (no spinners or animations; static `◐` instead), Increase Contrast, and system accent color.

### Performance budgets (measured on an M1 MacBook Air)
| Action | Target |
|---|---|
| Launch to first interactive tab (daemon already running) | < 500 ms |
| Reattach a tab with a full 16 MB buffer | last screen visible < 150 ms, full history < 1 s |
| New local session | < 100 ms to prompt (excluding shell rc time) |
| Keystroke-to-screen latency | same as upstream Warp (no added hop cost beyond the local socket, < 1 ms) |

To meet the reattach budget, the client **renders the tail first**: the shim sends the bytes from the last boundary immediately, then backfills earlier blocks, which the client prepends to the block list.

### Distribution
- Universal app (arm64 + x86_64), **macOS 13 Ventura or later** (needed for `SMAppService`).
- Hardened runtime, Developer ID signed and notarized; DMG and Homebrew cask.
- `swarmd`, `swarm-shim` and `swarmctl` ship inside the bundle (`Contents/MacOS/`). A Settings button installs a `swarmctl` symlink into `/usr/local/bin` or `~/.local/bin`.
- Updates with Sparkle 2 (EdDSA-signed feed on GitHub Releases). Updating the app never restarts running shims; `swarmd` is restarted and re-adopts them (§3).

### Design process
1. Clickable mockups of the main window, picker, connect sheet, first run and settings **before M2 coding**, reviewed against the principles above.
2. A UI checklist per pull request: light and dark mode, VoiceOver, keyboard-only use, Reduce Motion, screenshots attached.
3. Dogfood from M2 on: the team uses swarm-term as its daily terminal, and UI issues get a label and their own triage.

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

Repo layout (cargo workspace, as built):
```
crates/
  swarm-proto/    protocol types, framing, shared runtime/state/log paths
  swarm-shim/     library: per-session PTY holder, ring buffer, boundary scan, replay
  swarmd/         the daemon binary `swarmd` and the shim binary `swarm-shim`
                  (a thin main over the swarm-shim library), plus e2e tests
  swarmctl/       CLI
  swarm-client/   async client library used by both swarmctl and the GUI
scripts/          interactive-check.exp: drives swarmctl with vim/htop in a real pty
app/              the Warp fork (M2; git subtree or separate repo pinned by commit)
```

## 12. Roadmap

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 – spike** ✅ | shim + `swarmctl attach` with raw passthrough (dtach-equivalent) | Run vim/htop, detach, reattach from another terminal |
| **M1 – daemon** ✅ | `swarmd` registry, protocol v1, multi-client, read-only attach, latest-active sizing, ring buffer, boundary replay, `ResumeFrom`, daemon restart re-adopts shims, `at-login` service (launchd / systemd) | Kill `swarmd`, sessions survive and re-appear |
| **D – UI design** ✅ (in parallel with M0–M1) | Clickable mockups of main window, session states, picker, connect sheet, menus, menu bar extra, settings, first run (§9b) | Mockups reviewed and agreed before M2 starts |
| **M2 – GUI local** | Warp fork with `daemon_tty` `TerminalManager`, runtime manager selection, replay guard, idempotent bootstrap, startup restore, window⇒tab, close/detach semantics, first-run sheet, start-mode setting (`with-app` / `at-login` via `SMAppService`), host-grouped tab bar, session-state indicators, local session picker, menus and shortcuts, Settings window, menu bar extra | Quit app with 5 tabs, reopen, identical blocks restored; §9b performance budgets met; UI checklist passes |
| **M3 – remote** | `swarmd bridge`, ssh transport, provisioning for Linux and macOS hosts, per-host start mode, session picker dropdown, reconnect | Laptop sleep/wake against remote host resumes without visual reset |
| **M4 – polish** | alt-screen snapshots, tab drag-out, `smallest` size policy, notifications and Dock badge, Sparkle updates, signed/notarized DMG and Homebrew cask, optional "restore after reboot" (recreate sessions with the same name/cwd/tab order, without the processes) | — |

M0–M1 are independent of Warp and de-risk the hard part; M2 is where most of the unknowns are.

**Status (2026-10-30):** M0, M1 and D are done on branch `feat/daemon-core`. M0–M1 are covered by end-to-end tests against real daemons, shims and shells (`crates/swarmd/tests/e2e.rs`) and by `scripts/interactive-check.exp` (vim and htop in a real pty, across a daemon restart). Verified on macOS (arm64) and Linux (Ubuntu, arm64, via OrbStack), including the `ssh host swarmd bridge` path. Next: M2, starting with an unchanged build of the Warp fork and the shell-integration spike (§13, risk 2).

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
