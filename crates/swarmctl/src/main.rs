//! `swarmctl`: list, create, attach to and manage swarm-term sessions.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use swarm_client::{resolve, Attachment, Client, StreamEvent};
use swarm_proto::{
    now_ms, AttachMode, CreateOpts, MetaUpdate, ProgramStatus, SessionInfo, SessionState, Size, StatusRecord, StatusState,
};
use tokio::sync::mpsc;

/// Ctrl-\ detaches, as in dtach.
const DETACH_KEY: u8 = 0x1c;
/// For this long after replayed output, terminal responses in the input are
/// answers to replayed queries, not to the live program, and are dropped.
const REPLAY_INPUT_GUARD: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(name = "swarmctl", version = swarm_client::swarm_proto::VERSION, about = "Manage swarm-term sessions")]
struct Cli {
    /// Talk to the daemon on this host over ssh instead of the local one.
    #[arg(long, global = true)]
    host: Option<String>,
    /// Print JSON instead of text (ls, info, new, capture, wait).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List sessions.
    Ls,
    /// Create a session and attach to it.
    New {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Don't attach; print the new session's id.
        #[arg(short, long)]
        detached: bool,
        /// Don't attach; wait for the command to exit and exit with its status.
        #[arg(long)]
        wait: bool,
        /// With --wait: give up after this long (like 90s, 10m, 2h) and exit 1.
        #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration, requires = "wait")]
        wait_for: Option<Duration>,
        /// Command to run instead of your login shell.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Attach to a session (by id, id prefix or name). Ctrl-\ detaches.
    Attach {
        session: String,
        /// Watch without typing or resizing.
        #[arg(long)]
        readonly: bool,
        /// Skip the replay of buffered output.
        #[arg(long)]
        live: bool,
    },
    /// Send a signal to a session's process (default HUP).
    Kill {
        session: String,
        #[arg(short, long, default_value = "HUP")]
        signal: String,
    },
    /// End a session if needed and drop it for good.
    Rm { session: String },
    /// Rename a session ("" clears the name).
    Rename { session: String, name: String },
    /// Set UI metadata keys (empty value removes a key).
    Meta {
        session: String,
        #[arg(value_parser = parse_kv)]
        pairs: Vec<(String, String)>,
    },
    /// Show everything known about a session (default: the one this runs in).
    Info { session: Option<String> },
    /// Type text into a session (standard input if no text is given). Enter
    /// is pressed only with --enter or a newline in the text.
    Send {
        #[command(flatten)]
        target: Target,
        /// Press Enter after the text.
        #[arg(short, long)]
        enter: bool,
        text: Option<String>,
    },
    /// Press keys in a session: enter, tab, esc, backspace, up, f5, ctrl+c,
    /// alt+b, shift+tab, ctrl+left, or a single character.
    SendKey {
        #[command(flatten)]
        target: Target,
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Print what a session's screen shows now (the visible rows; `dump`
    /// prints the scrollback).
    Capture {
        #[command(flatten)]
        target: Target,
        /// Keep colors and styles as escape sequences.
        #[arg(long)]
        ansi: bool,
        /// Join lines the terminal wrapped.
        #[arg(long)]
        unwrap: bool,
    },
    /// Wait for a session's program to exit, and exit with its status.
    Wait {
        #[command(flatten)]
        target: Target,
        /// Give up after this long (like 90s, 10m, 2h) and exit 1.
        #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration)]
        wait_for: Option<Duration>,
        /// Instead, wait until the program reports one of these states
        /// (working, blocked, done, error, idle), and exit 0. Exits with the
        /// program's status if it ends first.
        #[arg(long, value_delimiter = ',', value_parser = parse_state)]
        until: Vec<StatusState>,
    },
    /// What programs report about themselves: working, blocked (waiting for
    /// you), done or error, with a message. Every session, or one with -s.
    Status {
        /// Session id, id prefix or name.
        #[arg(short, long)]
        session: Option<String>,
        /// Mark the session seen: drop its done and error reports and its
        /// last notification.
        #[arg(long, requires = "session")]
        clear: bool,
    },
    /// Print changes as they happen: sessions created, exited, renamed or
    /// removed, clients attaching, program status, bells and notifications.
    Events,
    /// Print a session's output: everything still kept, including what was
    /// spilled to disk (`swarmd spill on`), oldest first.
    Dump {
        session: String,
        /// The bytes as written, escape sequences included.
        #[arg(long)]
        raw: bool,
    },
    /// Daemon control.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

/// The session a command acts on: `-s`, or else the session it runs in.
#[derive(clap::Args)]
struct Target {
    /// Session id, id prefix or name. Default: the session this command
    /// runs in ($SWARM_SESSION_ID).
    #[arg(short, long)]
    session: Option<String>,
}

#[derive(Subcommand)]
enum DaemonCmd {
    Status,
    Start,
    /// Stop the daemon; sessions keep running.
    Stop,
    /// Start the daemon at login (launchd agent / systemd user unit).
    InstallService,
    /// Start the daemon only when needed again.
    UninstallService,
    /// Show whether the daemon starts at login.
    ServiceStatus,
    /// Move running sessions to the daemon's shim program without
    /// restarting them (the daemon does this itself when it starts).
    UpgradeShims {
        /// Also redo sessions whose shim already runs the daemon's build.
        #[arg(long)]
        force: bool,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("expected key=value, got {s}"))
}

/// Exit statuses besides 0 (done) and the waited-for program's own:
/// 1 failed, 2 bad command line, 3 no such session (or more than one
/// matches), 4 the daemon couldn't be reached.
const EXIT_NOT_FOUND: i32 = 3;
const EXIT_UNREACHABLE: i32 = 4;

/// An error that ends swarmctl with a particular status.
#[derive(Debug)]
struct Exit(i32, String);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Exit {}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let code = match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("swarmctl: {e:#}");
            e.downcast_ref::<Exit>().map_or(1, |x| x.0)
        }
    };
    rt.shutdown_timeout(Duration::from_millis(100));
    std::process::exit(code);
}

async fn connect(host: &Option<String>) -> Result<Client> {
    let client = match host {
        Some(h) => Client::connect_ssh(h, "swarmctl").await,
        None => Client::connect_local("swarmctl").await,
    };
    client.map_err(|e| Exit(EXIT_UNREACHABLE, format!("{e:#}")).into())
}

/// A daemon from before a request existed drops the connection when it
/// gets one: say so instead of "connection lost".
fn older_daemon(client: &Client, e: anyhow::Error) -> anyhow::Error {
    let daemon = &client.daemon().version;
    if daemon != swarm_client::swarm_proto::VERSION
        && format!("{e:#}").contains("connection to daemon lost")
    {
        return anyhow::anyhow!(
            "swarmd {daemon} on {} is older than this swarmctl ({}) and doesn't support this. \
             Update it: connect to the host from the Swarm app, or run `swarmctl daemon stop` there \
             so the next command starts the installed swarmd (sessions keep running)",
            client.daemon().hostname,
            swarm_client::swarm_proto::VERSION
        );
    }
    e
}

async fn run(cli: Cli) -> Result<i32> {
    let cmd = cli.cmd.unwrap_or(Cmd::Ls);
    let remote = cli.host.is_some();
    if let Cmd::Daemon { cmd: c @ (DaemonCmd::InstallService | DaemonCmd::UninstallService | DaemonCmd::ServiceStatus) } =
        &cmd
    {
        let args = match c {
            DaemonCmd::InstallService => "service install",
            DaemonCmd::UninstallService => "service uninstall",
            _ => "service status",
        };
        return service(&cli.host, args).await;
    }
    let client = connect(&cli.host).await?;
    let json = cli.json;
    match cmd {
        Cmd::Ls => {
            let sessions = client.list().await?;
            if json {
                print_json(&sessions)?;
            } else {
                // A daemon from before program status drops the connection
                // on this request; it's the last one, so that's harmless.
                let statuses: HashMap<String, ProgramStatus> = if client.daemon().version == swarm_client::swarm_proto::VERSION {
                    client.status().await.map(|v| v.into_iter().collect()).unwrap_or_default()
                } else {
                    HashMap::new()
                };
                print_table(&sessions, &statuses);
            }
            Ok(0)
        }
        Cmd::New {
            name,
            cwd,
            detached,
            wait,
            wait_for,
            command,
        } => {
            // A local folder means nothing on another host: start in its home there.
            let cwd = cwd.or_else(|| {
                if remote {
                    return None;
                }
                std::env::current_dir().ok().map(|p| p.display().to_string())
            });
            let mut ui = BTreeMap::new();
            ui.insert("created_by".into(), "swarmctl".into());
            let opts = CreateOpts { command, cwd, env: Vec::new(), size: terminal_size(), name, ui };
            let info = client.create(opts).await?;
            if wait {
                return wait_and_report(&client, &info.id, wait_for, json).await;
            }
            if detached {
                if json {
                    print_json(&info)?;
                } else {
                    println!("{}", info.id);
                }
                return Ok(0);
            }
            attach(&client, &info.id, false, false).await
        }
        Cmd::Attach { session, readonly, live } => {
            let id = find(&client, &session).await?.id;
            attach(&client, &id, readonly, live).await
        }
        Cmd::Kill { session, signal } => {
            let id = find(&client, &session).await?.id;
            client.kill(&id, parse_signal(&signal)?).await?;
            Ok(0)
        }
        Cmd::Rm { session } => {
            let id = find(&client, &session).await?.id;
            client.remove(&id).await?;
            Ok(0)
        }
        Cmd::Rename { session, name } => {
            let id = find(&client, &session).await?.id;
            client.set_meta(&id, MetaUpdate { name: Some(Some(name)), ui: BTreeMap::new() }).await?;
            Ok(0)
        }
        Cmd::Meta { session, pairs } => {
            let id = find(&client, &session).await?.id;
            let info = client.set_meta(&id, MetaUpdate { name: None, ui: pairs.into_iter().collect() }).await?;
            for (k, v) in &info.ui {
                println!("{k}={v}");
            }
            Ok(0)
        }
        Cmd::Info { session } => {
            let s = target(&client, session, remote).await?;
            if json {
                print_json(&s)?;
            } else {
                print_info(&s);
            }
            Ok(0)
        }
        Cmd::Send {
            target: t,
            enter,
            text,
        } => {
            let s = target(&client, t.session, remote).await?;
            let mut data = match text {
                Some(t) => t.into_bytes(),
                None => {
                    let mut buf = Vec::new();
                    std::io::stdin()
                        .read_to_end(&mut buf)
                        .context("read standard input")?;
                    buf
                }
            };
            if enter {
                data.push(b'\r');
            }
            if !data.is_empty() {
                client
                    .write(&s.id, data)
                    .await
                    .map_err(|e| older_daemon(&client, e))?;
            }
            Ok(0)
        }
        Cmd::SendKey { target: t, keys } => {
            let s = target(&client, t.session, remote).await?;
            // Arrow, Home and End keys depend on the program's cursor key mode.
            let app_cursor = if keys.iter().any(|k| keys::mode_dependent(k)) {
                client
                    .screen(&s.id)
                    .await
                    .map(|c| c.application_cursor)
                    .unwrap_or(false)
            } else {
                false
            };
            let mut data = Vec::new();
            for k in &keys {
                data.extend(keys::encode(k, app_cursor).map_err(|e| Exit(2, e))?);
            }
            client
                .write(&s.id, data)
                .await
                .map_err(|e| older_daemon(&client, e))?;
            Ok(0)
        }
        Cmd::Capture {
            target: t,
            ansi,
            unwrap,
        } => {
            let s = target(&client, t.session, remote).await?;
            let screen = client
                .screen(&s.id)
                .await
                .map_err(|e| older_daemon(&client, e))?;
            let lines = screen_lines(&screen, unwrap);
            let mut out = std::io::stdout().lock();
            if json {
                let v = serde_json::json!({
                    "id": s.id,
                    "cols": screen.size.cols,
                    "rows": screen.size.rows,
                    "cursor": { "row": screen.cursor.0, "col": screen.cursor.1 },
                    "alt_screen": screen.alt_screen,
                    "lines": lines,
                });
                writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
            } else if ansi {
                out.write_all(&screen.formatted)?;
                out.write_all(b"\x1b[0m\n")?;
            } else {
                for l in &lines {
                    writeln!(out, "{l}")?;
                }
            }
            out.flush()?;
            Ok(0)
        }
        Cmd::Wait { target: t, wait_for, until } => {
            let s = target(&client, t.session, remote).await?;
            if until.is_empty() {
                wait_and_report(&client, &s.id, wait_for, json).await
            } else {
                wait_for_state(&client, &s.id, &until, wait_for, json).await.map_err(|e| older_daemon(&client, e))
            }
        }
        Cmd::Status { session, clear } => {
            let statuses = client.status().await.map_err(|e| older_daemon(&client, e))?;
            let sessions = client.list().await?;
            if let Some(q) = session {
                let s = resolve(&sessions, &q).map_err(|e| Exit(EXIT_NOT_FOUND, format!("{e:#}")))?;
                if clear {
                    client.clear_status(&s.id).await?;
                    return Ok(0);
                }
                let status = statuses.into_iter().find(|(id, _)| *id == s.id).map(|(_, st)| st).unwrap_or_default();
                if json {
                    print_json(&serde_json::json!({ "id": s.id, "name": s.display_name(), "status": status }))?;
                } else {
                    print_status(s, &status);
                }
            } else if json {
                let all: Vec<_> = statuses
                    .iter()
                    .filter_map(|(id, st)| {
                        let s = sessions.iter().find(|s| s.id == *id)?;
                        Some(serde_json::json!({ "id": id, "name": s.display_name(), "status": st }))
                    })
                    .collect();
                print_json(&all)?;
            } else {
                print_status_table(&sessions, &statuses);
            }
            Ok(0)
        }
        Cmd::Events => events(&client, json).await.map_err(|e| older_daemon(&client, e)),
        Cmd::Dump { session, raw } => {
            let s = find(&client, &session).await?;
            let mut att = client.attach(&s.id, AttachMode::History, None, true).await?;
            let end = att.live_from;
            let mut text = TextFilter::default();
            let mut out = std::io::stdout().lock();
            let mut at = att.replay_from;
            while at < end {
                match att.recv().await {
                    Some(StreamEvent::Output { offset, data, .. }) => {
                        let keep = &data[..data.len().min(end.saturating_sub(offset) as usize)];
                        if raw {
                            out.write_all(keep)?;
                        } else {
                            out.write_all(&text.feed(keep))?;
                        }
                        at = offset + data.len() as u64;
                    }
                    Some(StreamEvent::Exited { .. }) => {}
                    Some(StreamEvent::Lagged { .. } | StreamEvent::Closed { .. }) | None => break,
                }
            }
            out.flush()?;
            drop(out);
            let _ = att.detach().await;
            Ok(0)
        }
        Cmd::Daemon { cmd } => match cmd {
            DaemonCmd::Status | DaemonCmd::Start => {
                client.ping().await?;
                let d = client.daemon();
                println!("swarmd {} running on {} ({})", d.version, d.hostname, d.os);
                Ok(0)
            }
            DaemonCmd::Stop => {
                client.shutdown_daemon().await?;
                println!("swarmd stopped; sessions keep running");
                Ok(0)
            }
            DaemonCmd::UpgradeShims { force } => {
                use swarm_proto::ShimUpgradeOutcome::*;
                let results = client.upgrade_shims(force).await?;
                let mut failed = false;
                for r in &results {
                    let what = match &r.outcome {
                        Upgraded => "updated in place".to_string(),
                        UpToDate => "already up to date".to_string(),
                        Skipped(why) => format!("skipped: {why}"),
                        Failed(why) => {
                            failed = true;
                            format!("failed: {why}")
                        }
                    };
                    println!("{}  {what}", &r.id[..r.id.len().min(10)]);
                }
                if results.is_empty() {
                    println!("no sessions");
                }
                Ok(if failed { 1 } else { 0 })
            }
            DaemonCmd::InstallService | DaemonCmd::UninstallService | DaemonCmd::ServiceStatus => unreachable!(),
        },
    }
}

/// Runs `swarmd service ...` here or on `host` (it manages the login
/// service of the machine it runs on).
async fn service(host: &Option<String>, args: &str) -> Result<i32> {
    let mut cmd = match host {
        Some(h) => {
            let mut c = tokio::process::Command::new("ssh");
            c.args(["-o", "BatchMode=yes", h, &swarm_client::remote_swarmd_command(args)]);
            c
        }
        None => {
            let mut c = tokio::process::Command::new(swarm_client::daemon_exe());
            c.args(args.split(' '));
            c
        }
    };
    let status = cmd.status().await.context("run swarmd service")?;
    Ok(status.code().unwrap_or(1))
}

/// Terminal output as plain text: escape sequences (colors, cursor moves,
/// titles, Warp's shell hooks) and carriage returns removed. Keeps its state
/// across chunks, which can split a sequence.
#[derive(Default)]
struct TextFilter {
    state: FilterState,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum FilterState {
    #[default]
    Text,
    /// After ESC.
    Escape,
    /// In a CSI sequence (`ESC [`), until its final byte.
    Csi,
    /// In a string (OSC, DCS, APC, PM, SOS) until BEL or `ESC \`.
    Str,
    /// ESC seen inside a string.
    StrEscape,
}

impl TextFilter {
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        use FilterState::*;
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.state = match (self.state, b) {
                (Text, 0x1b) => Escape,
                (Text, b'\r') => Text,
                (Text, b) if b >= 0x20 || b == b'\n' || b == b'\t' => {
                    out.push(b);
                    Text
                }
                (Text, _) => Text,
                (Escape, b'[') => Csi,
                (Escape, b']' | b'P' | b'_' | b'^' | b'X') => Str,
                (Escape, _) => Text,
                (Csi, 0x40..=0x7e) => Text,
                (Csi, _) => Csi,
                (Str, 0x07) => Text,
                (Str, 0x1b) => StrEscape,
                (Str, _) => Str,
                (StrEscape, b'\\') => Text,
                (StrEscape, 0x1b) => StrEscape,
                (StrEscape, _) => Str,
            };
        }
        out
    }
}

async fn find(client: &Client, query: &str) -> Result<SessionInfo> {
    let sessions = client.list().await?;
    resolve(&sessions, query)
        .cloned()
        .map_err(|e| Exit(EXIT_NOT_FOUND, format!("{e:#}")).into())
}

/// The named session, or else the session this command runs in.
async fn target(client: &Client, session: Option<String>, remote: bool) -> Result<SessionInfo> {
    if let Some(q) = session {
        return find(client, &q).await;
    }
    match std::env::var("SWARM_SESSION_ID") {
        Ok(id) if !id.is_empty() && !remote => {
            let sessions = client.list().await?;
            sessions.into_iter().find(|s| s.id == id).ok_or_else(|| {
                Exit(
                    EXIT_NOT_FOUND,
                    format!("this session ({id}) isn't on this daemon"),
                )
                .into()
            })
        }
        _ => Err(Exit(
            2,
            "name a session with -s (outside a Swarm session there's no default)".into(),
        )
        .into()),
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// The screen's rows, without the blank ones at the bottom; with `unwrap`,
/// rows the terminal wrapped are joined.
fn screen_lines(screen: &swarm_proto::ScreenCapture, unwrap: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut joining = false;
    for (i, row) in screen.rows.iter().enumerate() {
        match lines.last_mut() {
            Some(last) if joining => last.push_str(row),
            _ => lines.push(row.clone()),
        }
        joining = unwrap && screen.wrapped.get(i).copied().unwrap_or(false);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Like 90, 90s, 10m, 2h.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or((s, ""), |i| s.split_at(i));
    let n: f64 = num
        .parse()
        .map_err(|_| format!("not a duration: {s} (use 90s, 10m or 2h)"))?;
    let secs = match unit {
        "" | "s" => n,
        "ms" => n / 1000.0,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(format!("not a duration: {s} (use 90s, 10m or 2h)")),
    };
    Ok(Duration::from_secs_f64(secs))
}

/// A program's end: the status a shell would report (the exit code, or
/// 128 + the signal), its exit code and its signal.
type ExitInfo = (i32, Option<i32>, Option<i32>);

/// The status a shell would report: the exit code, or 128 + the signal.
fn exit_status(state: &SessionState) -> Option<ExitInfo> {
    match state {
        SessionState::Running => None,
        SessionState::Exited { code, signal, .. } => {
            Some((code.unwrap_or(128 + signal.unwrap_or(0)), *code, *signal))
        }
    }
}

/// Waits for the session's program to exit; returns its status.
async fn wait_exit(client: &Client, id: &str, limit: Option<Duration>) -> Result<ExitInfo> {
    use tokio::sync::broadcast::error::RecvError;
    // Subscribe first, then look: an exit between the two isn't missed.
    let mut events = client.subscribe().await?;
    let current = |sessions: Vec<SessionInfo>| -> Result<Option<ExitInfo>> {
        match sessions.into_iter().find(|s| s.id == id) {
            Some(s) => Ok(exit_status(&s.state)),
            None => bail!("the session was removed"),
        }
    };
    if let Some(done) = current(client.list().await?)? {
        return Ok(done);
    }
    let watch = async {
        loop {
            match events.recv().await {
                Ok(swarm_proto::SessionEvent::Updated(s)) if s.id == id => {
                    if let Some(done) = exit_status(&s.state) {
                        return Ok(done);
                    }
                }
                Ok(swarm_proto::SessionEvent::Removed { id: gone }) if gone == id => {
                    bail!("the session was removed")
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    if let Some(done) = current(client.list().await?)? {
                        return Ok(done);
                    }
                }
                Err(RecvError::Closed) => bail!("connection to daemon lost"),
            }
        }
    };
    match limit {
        Some(d) => tokio::time::timeout(d, watch)
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))?,
        None => watch.await,
    }
}

async fn wait_and_report(
    client: &Client,
    id: &str,
    limit: Option<Duration>,
    json: bool,
) -> Result<i32> {
    let (status, code, signal) = wait_exit(client, id, limit).await?;
    if json {
        print_json(
            &serde_json::json!({ "id": id, "status": status, "code": code, "signal": signal }),
        )?;
    }
    Ok(status)
}

fn parse_state(s: &str) -> Result<StatusState, String> {
    Ok(match s.trim() {
        "idle" => StatusState::Idle,
        "working" => StatusState::Working,
        "done" => StatusState::Done,
        "error" => StatusState::Error,
        "blocked" => StatusState::Blocked,
        other => return Err(format!("unknown state {other:?} (working, blocked, done, error, idle)")),
    })
}

/// Waits until the session's headline status is one of `states`; exits 0,
/// or with the program's status if it ends first.
async fn wait_for_state(
    client: &Client,
    id: &str,
    states: &[StatusState],
    limit: Option<Duration>,
    json: bool,
) -> Result<i32> {
    use tokio::sync::broadcast::error::RecvError;
    let mut changes = client.subscribe_status().await?;
    let mut events = client.subscribe().await?;
    let reached = |st: &ProgramStatus| st.headline().filter(|r| states.contains(&r.state)).cloned();
    let report = |r: &StatusRecord| -> Result<i32> {
        if json {
            print_json(&serde_json::json!({ "id": id, "state": r.state.as_str(), "record": r }))?;
        }
        Ok(0)
    };
    let current = client.status().await?;
    if let Some(r) = current.iter().find(|(sid, _)| sid == id).and_then(|(_, st)| reached(st)) {
        return report(&r);
    }
    if let Some(done) = client.list().await?.iter().find(|s| s.id == id).and_then(|s| exit_status(&s.state)) {
        return Ok(done.0);
    }
    let watch = async {
        loop {
            tokio::select! {
                change = changes.recv() => match change {
                    Ok((sid, st)) if sid == id => {
                        if let Some(r) = reached(&st) {
                            return report(&r);
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {
                        if let Some(r) = client.status().await?.iter().find(|(sid, _)| sid == id).and_then(|(_, st)| reached(st)) {
                            return report(&r);
                        }
                    }
                    Err(RecvError::Closed) => bail!("connection to daemon lost"),
                },
                ev = events.recv() => match ev {
                    Ok(swarm_proto::SessionEvent::Updated(s)) if s.id == id => {
                        if let Some(done) = exit_status(&s.state) {
                            return Ok(done.0);
                        }
                    }
                    Ok(swarm_proto::SessionEvent::Removed { id: gone }) if gone == id => bail!("the session was removed"),
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => bail!("connection to daemon lost"),
                },
            }
        }
    };
    match limit {
        Some(d) => tokio::time::timeout(d, watch).await.map_err(|_| anyhow::anyhow!("timed out"))?,
        None => watch.await,
    }
}

/// `blocked (permission): Apply 3 changes?`, `working 40%: Building`.
fn record_text(r: &StatusRecord) -> String {
    let mut text = r.state.as_str().to_string();
    if let Some(k) = r.kind {
        text.push_str(&format!(" ({})", k.as_str()));
    }
    if let Some(p) = r.progress {
        text.push_str(&format!(" {p}%"));
    }
    if let Some(m) = r.msg.as_deref().or(r.title.as_deref()) {
        text.push_str(": ");
        text.push_str(m);
    }
    text
}

/// `now` or `5m ago`.
fn when(ms: u64) -> String {
    match ago(ms) {
        now if now == "now" => now,
        t => format!("{t} ago"),
    }
}

fn print_status_table(sessions: &[SessionInfo], statuses: &[(String, ProgramStatus)]) {
    if sessions.is_empty() {
        println!("No sessions.");
        return;
    }
    println!("{:<10}  {:<20}  {:<12}  STATUS", "ID", "NAME", "APP");
    for s in sessions {
        let st = statuses.iter().find(|(id, _)| *id == s.id).map(|(_, st)| st);
        let (app, text) = match st.and_then(|st| st.headline().map(|r| (st, r))) {
            Some((st, r)) => (st.app_of(r).unwrap_or("-").to_string(), record_text(r)),
            None => match st.and_then(|st| st.notification.as_ref()) {
                Some(n) => ("-".to_string(), format!("notified: {}", if n.body.is_empty() { &n.title } else { &n.body })),
                None => ("-".to_string(), "-".to_string()),
            },
        };
        let id: String = s.id.chars().take(10).collect();
        let name: String = s.display_name().chars().take(20).collect();
        let app: String = app.chars().take(12).collect();
        println!("{id:<10}  {name:<20}  {app:<12}  {text}");
    }
}

fn print_status(s: &SessionInfo, st: &ProgramStatus) {
    println!("session     {} ({})", s.display_name(), s.id);
    if st.records.is_empty() {
        println!("status      - (nothing reported)");
    }
    for r in &st.records {
        let id = if r.id.is_empty() { "(program)" } else { r.id.as_str() };
        let app = st.app_of(r).map(|a| format!(" [{a}]")).unwrap_or_default();
        println!("status      {id}{app}  {}  ({})", record_text(r), when(r.updated_ms));
    }
    if let Some(n) = &st.notification {
        println!("notified    {}{}  ({})", n.title, if n.title.is_empty() { n.body.clone() } else { format!(": {}", n.body) }, when(n.at_ms));
    }
    if st.bells > 0 {
        println!("bells       {} (last {})", st.bells, when(st.bell_ms));
    }
    if let Some((code, at)) = st.last_exit {
        println!("last exit   {code} ({})", when(at));
    }
}

/// Streams changes, as text lines or one JSON object per line.
async fn events(client: &Client, json: bool) -> Result<i32> {
    use tokio::sync::broadcast::error::RecvError;
    // Asked first: a daemon too old for status fails here, not mid-stream.
    let mut statuses: HashMap<String, ProgramStatus> = client.status().await?.into_iter().collect();
    let mut changes = client.subscribe_status().await?;
    let mut events = client.subscribe().await?;
    let mut infos: HashMap<String, SessionInfo> = client.list().await?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let out = |kind: &str, id: &str, name: &str, text: String, fields: serde_json::Value| -> Result<()> {
        if json {
            let mut v = serde_json::json!({ "type": kind, "id": id, "name": name, "at_ms": now_ms() });
            if let (Some(obj), serde_json::Value::Object(extra)) = (v.as_object_mut(), fields) {
                obj.extend(extra);
            }
            println!("{}", serde_json::to_string(&v)?);
        } else {
            let secs = now_ms() / 1000;
            let local = time_of_day(secs);
            let short: String = id.chars().take(10).collect();
            println!("{local}  {kind:<12}  {short}  {name}{}", if text.is_empty() { String::new() } else { format!("  {text}") });
        }
        std::io::stdout().flush()?;
        Ok(())
    };
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(swarm_proto::SessionEvent::Created(s)) => {
                    out("created", &s.id, s.display_name(), s.command.join(" "), serde_json::json!({ "command": s.command }))?;
                    infos.insert(s.id.clone(), s);
                }
                Ok(swarm_proto::SessionEvent::Updated(s)) => {
                    if let Some(old) = infos.get(&s.id) {
                        session_changes(old, &s, &out)?;
                    }
                    infos.insert(s.id.clone(), s);
                }
                Ok(swarm_proto::SessionEvent::Removed { id }) => {
                    let name = infos.remove(&id).map(|s| s.display_name().to_string()).unwrap_or_default();
                    statuses.remove(&id);
                    out("removed", &id, &name, String::new(), serde_json::json!({}))?;
                }
                Err(RecvError::Lagged(_)) => {
                    infos = client.list().await?.into_iter().map(|s| (s.id.clone(), s)).collect();
                }
                Err(RecvError::Closed) => bail!("connection to daemon lost"),
            },
            change = changes.recv() => match change {
                Ok((id, st)) => {
                    let name = infos.get(&id).map(|s| s.display_name().to_string()).unwrap_or_default();
                    let old = statuses.insert(id.clone(), st.clone()).unwrap_or_default();
                    status_changes(&id, &name, &old, &st, &out)?;
                }
                Err(RecvError::Lagged(_)) => {
                    statuses = client.status().await?.into_iter().collect();
                }
                Err(RecvError::Closed) => bail!("connection to daemon lost"),
            },
        }
    }
}

type Emit<'a> = dyn Fn(&str, &str, &str, String, serde_json::Value) -> Result<()> + 'a;

fn session_changes(old: &SessionInfo, new: &SessionInfo, out: &Emit) -> Result<()> {
    let (id, name) = (new.id.as_str(), new.display_name());
    if let (SessionState::Running, Some((status, code, signal))) = (&old.state, exit_status(&new.state)) {
        let text = match signal {
            Some(sig) if code.is_none() => format!("killed by signal {sig}"),
            _ => format!("exit {status}"),
        };
        out("exited", id, name, text, serde_json::json!({ "status": status, "code": code, "signal": signal }))?;
    }
    if old.name != new.name {
        out("renamed", id, name, String::new(), serde_json::json!({ "old": old.name, "new": new.name }))?;
    }
    if old.title != new.title {
        out("title", id, name, new.title.clone(), serde_json::json!({ "title": new.title }))?;
    }
    if old.cwd != new.cwd {
        let cwd = new.cwd.clone().unwrap_or_default();
        out("cwd", id, name, cwd.clone(), serde_json::json!({ "cwd": cwd }))?;
    }
    if old.foreground != new.foreground {
        let fg = new.foreground.clone().unwrap_or_default();
        out("foreground", id, name, fg.clone(), serde_json::json!({ "foreground": new.foreground }))?;
    }
    for c in new.clients.iter().filter(|c| !old.clients.iter().any(|o| o.client_id == c.client_id)) {
        out("attached", id, name, c.name.clone(), serde_json::json!({ "client": c.name, "readonly": c.readonly }))?;
    }
    for c in old.clients.iter().filter(|c| !new.clients.iter().any(|n| n.client_id == c.client_id)) {
        out("detached", id, name, c.name.clone(), serde_json::json!({ "client": c.name }))?;
    }
    Ok(())
}

fn status_changes(id: &str, name: &str, old: &ProgramStatus, new: &ProgramStatus, out: &Emit) -> Result<()> {
    let key = |st: &ProgramStatus| st.headline().map(|r| (r.id.clone(), r.state, r.kind, r.progress, r.msg.clone()));
    if key(old) != key(new) {
        match new.headline() {
            Some(r) => out(
                "status",
                id,
                name,
                record_text(r),
                serde_json::json!({ "state": r.state.as_str(), "record": r, "app": new.app_of(r) }),
            )?,
            None => out("status", id, name, "-".into(), serde_json::json!({ "state": null }))?,
        }
    }
    if new.bells > old.bells {
        out("bell", id, name, String::new(), serde_json::json!({ "bells": new.bells }))?;
    }
    if let Some(n) = new.notification.as_ref().filter(|n| old.notification.as_ref().map(|o| o.at_ms) != Some(n.at_ms)) {
        let text = if n.title.is_empty() { n.body.clone() } else { format!("{}: {}", n.title, n.body) };
        out("notification", id, name, text, serde_json::json!({ "title": n.title, "body": n.body }))?;
    }
    if new.last_exit.is_some() && new.last_exit != old.last_exit {
        let code = new.last_exit.map(|(c, _)| c);
        out("command", id, name, format!("exit {}", code.unwrap_or_default()), serde_json::json!({ "code": code }))?;
    }
    Ok(())
}

/// `HH:MM:SS` in local time.
fn time_of_day(secs: u64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn terminal_size() -> Size {
    // Some ptys report 0x0 until someone sets a size.
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => Size { cols, rows },
        _ => Size::default(),
    }
}

fn parse_signal(s: &str) -> Result<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return Ok(n);
    }
    let name = s.trim_start_matches("SIG").to_ascii_uppercase();
    Ok(match name.as_str() {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "CONT" => libc::SIGCONT,
        "STOP" => libc::SIGSTOP,
        _ => bail!("unknown signal {s}"),
    })
}

// ---------------------------------------------------------------------------
// Listing

fn ago(ms: u64) -> String {
    let secs = now_ms().saturating_sub(ms) / 1000;
    match secs {
        0..=4 => "now".into(),
        5..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn state_text(s: &SessionInfo) -> String {
    match &s.state {
        SessionState::Running => match (&s.foreground, main_program(s)) {
            (Some(fg), _) => format!("running {fg}"),
            (None, Some(prog)) if !is_shell(prog) => format!("running {prog}"),
            _ => "idle".into(),
        },
        SessionState::Exited { code: Some(c), .. } => format!("exited {c}"),
        SessionState::Exited { signal: Some(sig), .. } => format!("killed ({sig})"),
        SessionState::Exited { .. } => "exited".into(),
    }
}

fn main_program(s: &SessionInfo) -> Option<&str> {
    let argv0 = s.command.first()?;
    Some(argv0.rsplit('/').next().unwrap_or(argv0).trim_start_matches('-'))
}

fn is_shell(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "pwsh" | "elvish" | "xonsh")
}

fn print_table(sessions: &[SessionInfo], statuses: &HashMap<String, ProgramStatus>) {
    if sessions.is_empty() {
        println!("No sessions. Start one with `swarmctl new`.");
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let rows: Vec<[String; 6]> = sessions
        .iter()
        .map(|s| {
            let cwd = s.cwd.clone().unwrap_or_default();
            let cwd = if !home.is_empty() && cwd.starts_with(&home) { format!("~{}", &cwd[home.len()..]) } else { cwd };
            [
                s.id.chars().take(10).collect(),
                s.display_name().to_string(),
                match statuses.get(&s.id).and_then(|st| st.headline()) {
                    Some(r) if r.state != StatusState::Idle => record_text(r),
                    _ => state_text(s),
                },
                cwd,
                s.clients.len().to_string(),
                ago(s.last_output_ms.max(s.last_input_ms)),
            ]
        })
        .collect();
    let header = ["ID", "NAME", "STATE", "CWD", "ATTACHED", "ACTIVE"];
    let mut widths = header.map(|h| h.len());
    for r in &rows {
        for (w, c) in widths.iter_mut().zip(r.iter()) {
            *w = (*w).max(c.chars().count()).min(40);
        }
    }
    let line = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .zip(widths.iter())
            .map(|(c, w)| {
                let c: String = c.chars().take(*w).collect();
                format!("{c:<w$}")
            })
            .collect();
        println!("{}", parts.join("  ").trim_end());
    };
    line(header.to_vec());
    for r in &rows {
        line(r.iter().map(|s| s.as_str()).collect());
    }
}

fn print_info(s: &SessionInfo) {
    println!("id          {}", s.id);
    println!("name        {}", s.name.as_deref().unwrap_or("-"));
    println!("title       {}", s.title);
    println!("command     {}", s.command.join(" "));
    println!("pid         {}", s.pid);
    println!("state       {}", state_text(s));
    println!("cwd         {}", s.cwd.as_deref().unwrap_or("-"));
    println!("size        {}x{}", s.size.cols, s.size.rows);
    println!("alt screen  {}", s.alt_screen);
    println!("buffer      {}..{} ({} bytes)", s.buffer_start, s.buffer_end, s.buffer_end - s.buffer_start);
    println!("created     {} ago", ago(s.created_ms));
    for c in &s.clients {
        println!("client      {} ({}){}", c.name, c.client_id, if c.readonly { " read-only" } else { "" });
    }
    for (k, v) in &s.ui {
        println!("ui          {k}={v}");
    }
}

// ---------------------------------------------------------------------------
// Attaching

struct RawMode;

impl RawMode {
    fn enable() -> Result<RawMode> {
        crossterm::terminal::enable_raw_mode().context("stdin is not a terminal")?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Undo modes a full-screen app may have left on.
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b>\x1b[0m",
        );
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

async fn attach(client: &Client, id: &str, readonly: bool, live: bool) -> Result<i32> {
    let mode = if live { AttachMode::Live } else { AttachMode::Full };
    let mut att = client.attach(id, mode, Some(terminal_size()), readonly).await?;
    let name = att.info.display_name().to_string();

    let raw = RawMode::enable()?;
    let mut stdout = std::io::stdout();
    // Start from a clean screen; a replay that doesn't begin at the very
    // first byte may begin mid-state, so fully reset in that case.
    stdout.write_all(if att.replay_from > 0 { b"\x1bc" } else { b"\x1b[H\x1b[2J" })?;
    stdout.flush()?;

    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut guard_until = Instant::now() + REPLAY_INPUT_GUARD;
    let mut last_offset = att.replay_from;

    let outcome = loop {
        tokio::select! {
            ev = att.recv() => match ev {
                Some(StreamEvent::Output { offset, data, replay }) => {
                    stdout.write_all(&data)?;
                    stdout.flush()?;
                    last_offset = offset + data.len() as u64;
                    if replay {
                        guard_until = Instant::now() + REPLAY_INPUT_GUARD;
                    }
                }
                Some(StreamEvent::Exited { code, signal }) => {
                    break match (code, signal) {
                        (Some(c), _) => format!("session exited with code {c}"),
                        (_, Some(s)) => format!("session killed by signal {s}"),
                        _ => "session exited".into(),
                    };
                }
                Some(StreamEvent::Lagged { resume_at }) => {
                    // Too slow to keep up: pick up exactly where we stopped.
                    let resume = resume_at.min(last_offset);
                    att = reattach(client, id, resume, readonly).await?;
                }
                Some(StreamEvent::Closed { reason }) => break reason,
                None => break "connection closed".into(),
            },
            input = in_rx.recv() => {
                let Some(mut data) = input else { break "stdin closed".into() };
                if Instant::now() < guard_until {
                    data = swarm_client::strip_terminal_responses(&data);
                }
                let detach_at = data.iter().position(|&b| b == DETACH_KEY);
                if let Some(i) = detach_at {
                    data.truncate(i);
                }
                if !data.is_empty() && !readonly {
                    att.input(data).await?;
                }
                if detach_at.is_some() {
                    att.detach().await?;
                    break format!("detached from {name}");
                }
            }
            _ = winch.recv() => {
                att.resize(terminal_size()).await?;
            }
        }
    };
    drop(raw);
    println!("\r\n[{outcome}]");
    Ok(0)
}

async fn reattach(client: &Client, id: &str, from: u64, readonly: bool) -> Result<Attachment> {
    client.attach(id, AttachMode::ResumeFrom(from), Some(terminal_size()), readonly).await
}

#[cfg(test)]
mod tests {
    use super::{parse_duration, screen_lines, TextFilter};
    use std::time::Duration;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_secs(5400));
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn screen_lines_trim_and_unwrap() {
        let screen = swarm_proto::ScreenCapture {
            rows: vec![
                "$ echo".into(),
                "hello".into(),
                " world".into(),
                "".into(),
                "".into(),
            ],
            wrapped: vec![false, true, false, false, false],
            ..Default::default()
        };
        assert_eq!(screen_lines(&screen, false), ["$ echo", "hello", " world"]);
        assert_eq!(screen_lines(&screen, true), ["$ echo", "hello world"]);
    }

    #[test]
    fn text_filter_drops_escapes_split_across_chunks() {
        let mut f = TextFilter::default();
        let mut out = f.feed(b"\x1b[1;32mgreen\x1b[0m\r\nhook:\x1bP$d7b2268");
        out.extend(f.feed(b"6f6f6b\x1b"));
        out.extend(f.feed(b"\\done \x1b]0;title\x07!\n"));
        assert_eq!(String::from_utf8(out).unwrap(), "green\nhook:done !\n");
    }
}

mod keys {
    //! Key names to the bytes an xterm-compatible terminal sends.

    /// Keys whose bytes change with the cursor key mode (DECCKM).
    pub fn mode_dependent(name: &str) -> bool {
        let key = name.rsplit('+').next().unwrap_or(name).to_ascii_lowercase();
        let plain = !name.contains('+');
        plain
            && matches!(
                key.as_str(),
                "up" | "down" | "left" | "right" | "home" | "end"
            )
    }

    pub fn encode(name: &str, app_cursor: bool) -> Result<Vec<u8>, String> {
        let bad = || format!("unknown key {name:?} (like enter, tab, esc, up, f5, ctrl+c, alt+b)");
        let parts: Vec<&str> = name.split('+').collect();
        // `+` itself, and `ctrl++`.
        let (mods, key) = match parts.as_slice() {
            [.., "", ""] => (&parts[..parts.len() - 2], "+"),
            [mods @ .., key] => (mods, *key),
            [] => return Err(bad()),
        };
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "opt" | "option" | "meta" => alt = true,
                "shift" => shift = true,
                _ => return Err(bad()),
            }
        }
        // xterm's modifier parameter: 1 + shift 1 + alt 2 + ctrl 4.
        let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
        let lower = key.to_ascii_lowercase();
        let csi_letter = |c: char| -> Vec<u8> {
            if m > 1 {
                format!("\x1b[1;{m}{c}").into_bytes()
            } else if app_cursor {
                format!("\x1bO{c}").into_bytes()
            } else {
                format!("\x1b[{c}").into_bytes()
            }
        };
        let tilde = |n: u8| -> Vec<u8> {
            if m > 1 {
                format!("\x1b[{n};{m}~").into_bytes()
            } else {
                format!("\x1b[{n}~").into_bytes()
            }
        };
        let special = match lower.as_str() {
            "up" => Some(csi_letter('A')),
            "down" => Some(csi_letter('B')),
            "right" => Some(csi_letter('C')),
            "left" => Some(csi_letter('D')),
            "home" => Some(csi_letter('H')),
            "end" => Some(csi_letter('F')),
            "insert" => Some(tilde(2)),
            "delete" | "del" => Some(tilde(3)),
            "pageup" | "pgup" => Some(tilde(5)),
            "pagedown" | "pgdn" => Some(tilde(6)),
            "f1" | "f2" | "f3" | "f4" => {
                let c = (b'P' + lower.as_bytes()[1] - b'1') as char;
                Some(if m > 1 {
                    format!("\x1b[1;{m}{c}").into_bytes()
                } else {
                    format!("\x1bO{c}").into_bytes()
                })
            }
            "f5" => Some(tilde(15)),
            "f6" => Some(tilde(17)),
            "f7" => Some(tilde(18)),
            "f8" => Some(tilde(19)),
            "f9" => Some(tilde(20)),
            "f10" => Some(tilde(21)),
            "f11" => Some(tilde(23)),
            "f12" => Some(tilde(24)),
            _ => None,
        };
        if let Some(bytes) = special {
            return Ok(bytes);
        }
        let mut bytes = match lower.as_str() {
            "enter" | "return" | "cr" => vec![b'\r'],
            "tab" if shift => b"\x1b[Z".to_vec(),
            "tab" => vec![b'\t'],
            "esc" | "escape" => vec![0x1b],
            "backspace" | "bs" => vec![if ctrl { 0x08 } else { 0x7f }],
            "space" => vec![if ctrl { 0 } else { b' ' }],
            _ => {
                let mut chars = key.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return Err(bad());
                };
                let c = if shift { c.to_ascii_uppercase() } else { c };
                if ctrl {
                    let b = match c.to_ascii_lowercase() {
                        c @ 'a'..='z' => c as u8 - b'a' + 1,
                        '@' | '2' | ' ' => 0,
                        '[' | '3' => 0x1b,
                        '\\' | '4' => 0x1c,
                        ']' | '5' => 0x1d,
                        '^' | '6' => 0x1e,
                        '_' | '7' | '/' | '-' => 0x1f,
                        '?' | '8' => 0x7f,
                        _ => return Err(bad()),
                    };
                    vec![b]
                } else {
                    c.to_string().into_bytes()
                }
            }
        };
        if alt {
            bytes.insert(0, 0x1b);
        }
        Ok(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::encode;

        #[test]
        fn names_become_xterm_bytes() {
            let k = |n: &str| encode(n, false).unwrap();
            assert_eq!(k("enter"), b"\r");
            assert_eq!(k("ctrl+c"), [3]);
            assert_eq!(k("alt+b"), b"\x1bb");
            assert_eq!(k("shift+tab"), b"\x1b[Z");
            assert_eq!(k("up"), b"\x1b[A");
            assert_eq!(encode("up", true).unwrap(), b"\x1bOA");
            assert_eq!(k("ctrl+left"), b"\x1b[1;5D");
            assert_eq!(k("f5"), b"\x1b[15~");
            assert_eq!(k("shift+f5"), b"\x1b[15;2~");
            assert_eq!(k("f1"), b"\x1bOP");
            assert_eq!(k("q"), b"q");
            assert_eq!(k("shift+a"), b"A");
            assert_eq!(k("+"), b"+");
            assert!(encode("hyper+x", false).is_err());
            assert!(encode("nope", false).is_err());
        }
    }
}
