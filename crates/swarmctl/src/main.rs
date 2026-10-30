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
#[command(name = "swarmctl", version, about = "Manage swarm-term sessions")]
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
    let client = connect(&cli.host).await?;
    match cmd {
        Cmd::Ls => {
            print_table(&client.list().await?);
            Ok(0)
        }
        Cmd::New { name, cwd, detached, command } => {
            let cwd = cwd.or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()));
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
        },
    }
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
                    data = strip_terminal_responses(&data);
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

/// Removes what a terminal sends in answer to queries (device attributes,
/// cursor position and mode reports, OSC color and DCS replies) and keeps
/// everything the user typed.
fn strip_terminal_responses(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b {
            if let Some(len) = response_len(&data[i..]) {
                i += len;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Length of the terminal response starting at `s[0] == ESC`, if it is one.
fn response_len(s: &[u8]) -> Option<usize> {
    match s.get(1)? {
        b'[' => {
            // CSI [?>=]params [$] final, where the final marks a report:
            // c (DA), R (CPR), n (DSR), y (DECRPM), t (window ops), u (kitty keys).
            let mut j = 2;
            let private = matches!(s.get(j), Some(b'?' | b'>' | b'='));
            if private {
                j += 1;
            }
            while matches!(s.get(j), Some(b'0'..=b'9' | b';' | b':')) {
                j += 1;
            }
            if s.get(j) == Some(&b'$') {
                j += 1;
            }
            let fin = *s.get(j)?;
            let report = matches!(fin, b'c' | b'n' | b'y' | b't' | b'R') || (private && fin == b'u');
            // A bare ESC [ R would be a key, not a report.
            (report && j > 2).then_some(j + 1)
        }
        // OSC and DCS replies, terminated by BEL or ST.
        b']' | b'P' => {
            let mut j = 2;
            while j < s.len() {
                if s[j] == 0x07 {
                    return Some(j + 1);
                }
                if s[j] == 0x1b && s.get(j + 1) == Some(&b'\\') {
                    return Some(j + 2);
                }
                j += 1;
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_terminal_responses as strip;

    #[test]
    fn keeps_typing_and_keys() {
        assert_eq!(strip(b"ihello\x1b"), b"ihello\x1b");
        assert_eq!(strip(b"\x1b[A\x1b[1;5D:wq\r"), b"\x1b[A\x1b[1;5D:wq\r");
    }

    #[test]
    fn drops_responses() {
        assert_eq!(strip(b"\x1b[?62;22cab\x1b[24;80R"), b"ab");
        assert_eq!(strip(b"x\x1b]11;rgb:0000/0000/0000\x07y"), b"xy");
        assert_eq!(strip(b"\x1bP>|xterm(390)\x1b\\z"), b"z");
        assert_eq!(strip(b"\x1b[?2004;1$yq"), b"q");
    }
}
