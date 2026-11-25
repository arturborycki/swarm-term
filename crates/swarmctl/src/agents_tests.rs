use std::path::Path;

use serde_json::json;

use super::*;

fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("swarmctl-agents-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn paths(home: &Path) -> Paths {
    Paths { claude_dir: home.join(".claude"), codex_dir: home.join(".codex") }
}

const EXE: &str = "/Applications/Swarm.app/Contents/MacOS/swarmctl";

#[test]
fn claude_events_become_reports() {
    let report = |event: serde_json::Value| claude_report(&event);
    assert_eq!(report(json!({"hook_event_name": "UserPromptSubmit"})).unwrap().state, "working");
    assert_eq!(report(json!({"hook_event_name": "PostToolUse"})).unwrap().state, "working");
    let blocked = report(json!({"hook_event_name": "Notification", "message": "Claude needs your permission to use Bash"})).unwrap();
    assert_eq!((blocked.state, blocked.kind), ("blocked", Some("permission")));
    assert_eq!(blocked.msg.as_deref(), Some("Claude needs your permission to use Bash"));
    assert_eq!(report(json!({"hook_event_name": "Notification", "message": "Claude is waiting for your input"})), None);
    assert_eq!(report(json!({"hook_event_name": "Stop"})).unwrap().state, "done");
    assert_eq!(report(json!({"hook_event_name": "SessionEnd"})).unwrap().state, "clear");
    assert_eq!(report(json!({"hook_event_name": "PreCompact"})), None);
    assert_eq!(report(json!({})), None);
}

#[test]
fn codex_turn_complete_is_done_with_its_first_line() {
    let event = json!({"type": "agent-turn-complete", "last-assistant-message": "\nAll tests pass.\nDetails…"});
    let report = codex_report(&event).unwrap();
    assert_eq!((report.state, report.app, report.msg.as_deref()), ("done", "codex", Some("All tests pass.")));
    assert_eq!(codex_report(&json!({"type": "other"})), None);
}

#[test]
fn sequence_encodes_the_message() {
    let report = Report { state: "blocked", kind: Some("permission"), app: "claude-code", msg: Some("Run ls?\n".into()) };
    assert_eq!(report.sequence(), "\x1b]7501;state=blocked:kind=permission:app=claude-code:msg=UnVuIGxzPw==\x1b\\");
    let done = Report { state: "done", kind: None, app: "codex", msg: None };
    assert_eq!(done.sequence(), "\x1b]7501;state=done:app=codex\x1b\\");
    assert_eq!(base64(b"ab"), "YWI=");
    assert_eq!(base64(b"abc"), "YWJj");
}

#[test]
fn claude_install_keeps_settings_and_order_and_is_idempotent() {
    let home = temp_home("claude");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"say done\"\n          }\n        ]\n      }\n    ]\n  },\n  \"env\": {}\n}\n";
    let file = home.join(".claude/settings.json");
    std::fs::write(&file, original).unwrap();
    let p = paths(&home);

    install(&p, Path::new(EXE)).unwrap();
    install(&p, Path::new(EXE)).unwrap();
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let keys: Vec<&String> = settings.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["model", "hooks", "env"]);
    let stop = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "the user's hook stays and ours is added once");
    assert_eq!(stop[0]["hooks"][0]["command"], "say done");
    assert_eq!(stop[1]["hooks"][0]["command"], format!("{EXE} agent-hook claude"));
    assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "*");
    assert_eq!(std::fs::read_to_string(home.join(".claude/settings.json.swarm-backup")).unwrap().contains("swarmctl"), true);
    assert_eq!(status(&p).unwrap()[0].1, "reports to Swarm");

    uninstall(&p).unwrap();
    let back: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(back, serde_json::from_str::<Value>(original).unwrap());
    assert_eq!(status(&p).unwrap()[0].1, "doesn't report to Swarm yet");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn claude_hooks_are_removed_with_empty_hooks_object() {
    let home = temp_home("claude-empty");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let p = paths(&home);
    install(&p, Path::new(EXE)).unwrap();
    uninstall(&p).unwrap();
    let back: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(back, json!({}));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn codex_notify_is_added_once_and_never_replaces_another() {
    let home = temp_home("codex");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let file = home.join(".codex/config.toml");
    std::fs::write(&file, "model = \"o3\"\n\n[tui]\nnotifications = true\n").unwrap();
    let p = paths(&home);
    install(&p, Path::new(EXE)).unwrap();
    install(&p, Path::new(EXE)).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        text,
        format!("notify = [\"{EXE}\", \"agent-hook\", \"codex\"]\nmodel = \"o3\"\n\n[tui]\nnotifications = true\n")
    );
    uninstall(&p).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "model = \"o3\"\n\n[tui]\nnotifications = true\n");

    std::fs::write(&file, "notify = [\"terminal-notifier\"]\n").unwrap();
    let outcome = install(&p, Path::new(EXE)).unwrap();
    assert!(outcome[1].1.contains("not changed"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "notify = [\"terminal-notifier\"]\n");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn agents_not_installed_are_skipped() {
    let home = temp_home("none");
    let outcome = install(&paths(&home), Path::new(EXE)).unwrap();
    assert_eq!(outcome, vec![("Claude Code", "not installed, skipped".to_string()), ("Codex", "not installed, skipped".to_string())]);
    assert!(!home.join(".claude").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn shell_join_quotes_paths_with_spaces() {
    assert_eq!(shell_join(&["/a b/swarmctl".into(), "agent-hook".into()]), "'/a b/swarmctl' agent-hook");
}
