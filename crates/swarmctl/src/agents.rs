//! Coding agents report their state to Swarm.
//!
//! `swarmctl agents install` adds hooks to Claude Code (`~/.claude/settings.json`) and a notify
//! program to Codex (`~/.codex/config.toml`) that run `swarmctl agent-hook`. That command turns
//! the agent's event into a Program Status Protocol report (OSC 7501) written to the session's
//! terminal, so the session shows the agent working, waiting for you or done. Outside a Swarm
//! session it does nothing, so the agents work as before in other terminals.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

/// Marks the hooks this command manages, so installing twice changes nothing and uninstalling
/// removes only them.
const MARK: &str = "agent-hook";

/// Claude Code events and what each one means for the session.
const CLAUDE_EVENTS: &[(&str, bool)] = &[
    // (event, needs a tool matcher)
    ("UserPromptSubmit", false),
    ("PostToolUse", true),
    ("Notification", false),
    ("Stop", false),
    ("SessionEnd", false),
];

/// A report for the session's terminal.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub state: &'static str,
    pub kind: Option<&'static str>,
    pub app: &'static str,
    pub msg: Option<String>,
}

impl Report {
    /// The OSC 7501 sequence.
    pub fn sequence(&self) -> String {
        let mut body = format!("state={}", self.state);
        if let Some(kind) = self.kind {
            body.push_str(&format!(":kind={kind}"));
        }
        body.push_str(&format!(":app={}", self.app));
        if let Some(msg) = self.msg.as_deref().map(clean).filter(|m| !m.is_empty()) {
            body.push_str(&format!(":msg={}", base64(msg.as_bytes())));
        }
        format!("\x1b]7501;{body}\x1b\\")
    }
}

/// What a Claude Code hook event (its JSON on stdin) means, if anything.
pub fn claude_report(event: &Value) -> Option<Report> {
    let app = "claude-code";
    let message = event.get("message").and_then(Value::as_str).map(str::to_string);
    match event.get("hook_event_name").and_then(Value::as_str)? {
        "UserPromptSubmit" | "PostToolUse" => Some(Report { state: "working", kind: None, app, msg: None }),
        "Notification" => {
            let text = message.unwrap_or_default();
            let lower = text.to_lowercase();
            // Sent a while after Claude finished; the session already shows that.
            if lower.contains("waiting for your input") {
                return None;
            }
            let kind = if lower.contains("permission") { "permission" } else { "question" };
            Some(Report { state: "blocked", kind: Some(kind), app, msg: Some(text) })
        }
        "Stop" => Some(Report { state: "done", kind: None, app, msg: None }),
        "SessionEnd" => Some(Report { state: "clear", kind: None, app, msg: None }),
        _ => None,
    }
}

/// What a Codex notification (its JSON, the last argument) means, if anything.
pub fn codex_report(event: &Value) -> Option<Report> {
    match event.get("type").and_then(Value::as_str)? {
        "agent-turn-complete" => Some(Report {
            state: "done",
            kind: None,
            app: "codex",
            msg: event.get("last-assistant-message").and_then(Value::as_str).map(first_line),
        }),
        _ => None,
    }
}

/// `swarmctl agent-hook <agent> [json]`: reports the event to the session's terminal. Never
/// fails, so a problem here can't get in the agent's way.
pub fn run_hook(agent: &str, arg: Option<&str>) {
    if std::env::var_os("SWARM_SESSION_ID").is_none() {
        return;
    }
    let report = match agent {
        "claude" => {
            let mut input = String::new();
            if std::io::stdin().take(1 << 20).read_to_string(&mut input).is_err() {
                return;
            }
            serde_json::from_str(&input).ok().as_ref().and_then(claude_report)
        }
        "codex" => arg.and_then(|a| serde_json::from_str(a).ok()).as_ref().and_then(codex_report),
        _ => None,
    };
    let Some(report) = report else { return };
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(report.sequence().as_bytes());
    }
}

/// Where each agent keeps its configuration.
pub struct Paths {
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
}

impl Paths {
    pub fn from_home() -> Result<Paths> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME isn't set")?);
        Ok(Paths { claude_dir: home.join(".claude"), codex_dir: home.join(".codex") })
    }
}

/// What `install`, `uninstall` or `status` found or did, one line per agent.
pub type Outcome = Vec<(&'static str, String)>;

/// The command an agent runs: this swarmctl, by its full path.
fn hook_command(exe: &Path, agent: &str) -> Vec<String> {
    vec![exe.display().to_string(), MARK.to_string(), agent.to_string()]
}

pub fn install(paths: &Paths, exe: &Path) -> Result<Outcome> {
    Ok(vec![("Claude Code", install_claude(&paths.claude_dir, exe)?), ("Codex", install_codex(&paths.codex_dir, exe)?)])
}

pub fn uninstall(paths: &Paths) -> Result<Outcome> {
    Ok(vec![("Claude Code", uninstall_claude(&paths.claude_dir)?), ("Codex", uninstall_codex(&paths.codex_dir)?)])
}

pub fn status(paths: &Paths) -> Result<Outcome> {
    let claude = if !paths.claude_dir.is_dir() {
        "not installed".to_string()
    } else if claude_has_hooks(&read_json(&paths.claude_dir.join("settings.json"))?) {
        "reports to Swarm".to_string()
    } else {
        "doesn't report to Swarm yet".to_string()
    };
    let codex = if !paths.codex_dir.is_dir() {
        "not installed".to_string()
    } else {
        match codex_notify(&read_text(&paths.codex_dir.join("config.toml"))?) {
            Some(line) if line.contains(MARK) => "reports to Swarm".to_string(),
            Some(_) => "has another notify program, so it doesn't report to Swarm".to_string(),
            None => "doesn't report to Swarm yet".to_string(),
        }
    };
    Ok(vec![("Claude Code", claude), ("Codex", codex)])
}

fn install_claude(dir: &Path, exe: &Path) -> Result<String> {
    if !dir.is_dir() {
        return Ok("not installed, skipped".to_string());
    }
    let path = dir.join("settings.json");
    let mut settings = read_json(&path)?;
    if claude_has_hooks(&settings) {
        remove_claude_hooks(&mut settings);
    }
    let command = shell_join(&hook_command(exe, "claude"));
    let root = settings.as_object_mut().context("~/.claude/settings.json isn't a JSON object")?;
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().context("\"hooks\" in ~/.claude/settings.json isn't an object")?;
    for (event, matcher) in CLAUDE_EVENTS {
        let list = hooks.entry(*event).or_insert_with(|| Value::Array(Vec::new()));
        let list = list.as_array_mut().with_context(|| format!("hooks.{event} isn't a list"))?;
        let mut entry = json!({ "hooks": [{ "type": "command", "command": command }] });
        if *matcher {
            entry["matcher"] = json!("*");
        }
        list.push(entry);
    }
    write_with_backup(&path, &(serde_json::to_string_pretty(&settings)? + "\n"))?;
    Ok(format!("hooks added to {}", path.display()))
}

fn uninstall_claude(dir: &Path) -> Result<String> {
    let path = dir.join("settings.json");
    if !path.exists() {
        return Ok("nothing to remove".to_string());
    }
    let mut settings = read_json(&path)?;
    if !claude_has_hooks(&settings) {
        return Ok("nothing to remove".to_string());
    }
    remove_claude_hooks(&mut settings);
    write_with_backup(&path, &(serde_json::to_string_pretty(&settings)? + "\n"))?;
    Ok(format!("hooks removed from {}", path.display()))
}

fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(MARK) && c.contains("swarmctl"))))
}

fn claude_has_hooks(settings: &Value) -> bool {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| hooks.values().filter_map(Value::as_array).flatten().any(is_ours))
}

/// Removes this command's hooks, and any event list or `hooks` object that leaves empty.
fn remove_claude_hooks(settings: &mut Value) {
    let Some(root) = settings.as_object_mut() else { return };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else { return };
    for list in hooks.values_mut().filter_map(Value::as_array_mut) {
        list.retain(|entry| !is_ours(entry));
    }
    hooks.retain(|_, list| list.as_array().is_none_or(|l| !l.is_empty()));
    if hooks.is_empty() {
        root.remove("hooks");
    }
}

fn install_codex(dir: &Path, exe: &Path) -> Result<String> {
    if !dir.is_dir() {
        return Ok("not installed, skipped".to_string());
    }
    let path = dir.join("config.toml");
    let text = read_text(&path)?;
    let line = format!(
        "notify = [{}]",
        hook_command(exe, "codex").iter().map(|a| toml_string(a)).collect::<Vec<_>>().join(", ")
    );
    let new = match codex_notify(&text) {
        Some(existing) if existing.contains(MARK) => text.replacen(&existing, &line, 1),
        Some(_) => {
            return Ok(format!(
                "{} already runs another notify program; not changed. Codex runs only one.",
                path.display()
            ));
        }
        // Top-level keys must come before the first [table].
        None => format!("{line}\n{text}"),
    };
    write_with_backup(&path, &new)?;
    Ok(format!("notify program set in {}", path.display()))
}

fn uninstall_codex(dir: &Path) -> Result<String> {
    let path = dir.join("config.toml");
    let text = read_text(&path)?;
    match codex_notify(&text) {
        Some(line) if line.contains(MARK) => {
            let new = text.replacen(&format!("{line}\n"), "", 1).replacen(&line, "", 1);
            write_with_backup(&path, &new)?;
            Ok(format!("notify program removed from {}", path.display()))
        }
        _ => Ok("nothing to remove".to_string()),
    }
}

/// The top-level `notify = …` line of a Codex config, if it has one on a single line.
fn codex_notify(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            return None;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        if key == "notify" && trimmed.contains('=') {
            return Some(line.to_string());
        }
    }
    None
}

fn read_json(path: &Path) -> Result<Value> {
    let text = read_text(path)?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&text).with_context(|| format!("{} isn't valid JSON; not changed", path.display()))
}

fn read_text(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Writes `text` to `path` through a temporary file, after copying the old file to
/// `<path>.swarm-backup`.
fn write_with_backup(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // A settings file that is a symlink (into a dotfiles repo, say) stays one:
    // the file it points to is the one rewritten.
    let real = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };
    // The rewritten file keeps the mode it had; a new one is private.
    let mode = match std::fs::metadata(&real) {
        Ok(m) => {
            let backup = PathBuf::from(format!("{}.swarm-backup", path.display()));
            let _ = std::fs::remove_file(&backup);
            std::fs::copy(&real, &backup)
                .with_context(|| format!("back up {} to {}", path.display(), backup.display()))?;
            m.permissions().mode() & 0o7777
        }
        Err(_) => 0o600,
    };
    let tmp = PathBuf::from(format!("{}.swarm-tmp", real.display()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    // The umask may have taken bits off; set the mode the file had.
    f.set_permissions(std::fs::Permissions::from_mode(mode)).with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(text.as_bytes()).and_then(|_| f.sync_all()).with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, &real).with_context(|| format!("replace {}", real.display()))?;
    Ok(())
}

/// Quotes arguments for the shell Claude Code runs hook commands with.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Text without control characters, which reports refuse, and at most 300 characters.
fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(300).collect::<String>().trim().to_string()
}

fn first_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "agents_tests.rs"]
mod tests;
