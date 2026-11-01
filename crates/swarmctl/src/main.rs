//! `swarmctl`: list, create, attach to and manage swarm-term sessions.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use swarm_client::{resolve, Attachment, Client, StreamEvent};
use swarm_proto::{now_ms, AttachMode, CreateOpts, MetaUpdate, SessionInfo, SessionState, Size};
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
    /// Show everything known about a session.
    Info { session: String },
    /// Daemon control.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
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
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("expected key=value, got {s}"))
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let code = match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("swarmctl: {e:#}");
            1
        }
    };
    rt.shutdown_timeout(Duration::from_millis(100));
    std::process::exit(code);
}

async fn connect(host: &Option<String>) -> Result<Client> {
    match host {
        Some(h) => Client::connect_ssh(h, "swarmctl").await,
        None => Client::connect_local("swarmctl").await,
    }
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
    match cmd {
        Cmd::Ls => {
            print_table(&client.list().await?);
            Ok(0)
        }
        Cmd::New { name, cwd, detached, command } => {
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
            if detached {
                println!("{}", info.id);
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
            let s = find(&client, &session).await?;
            print_info(&s);
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

async fn find(client: &Client, query: &str) -> Result<SessionInfo> {
    let sessions = client.list().await?;
    resolve(&sessions, query).cloned()
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

fn print_table(sessions: &[SessionInfo]) {
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
                state_text(s),
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
