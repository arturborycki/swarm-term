//! `at-login` start mode: a launchd agent (macOS) or systemd user unit
//! (Linux) that runs `swarmd run --if-not-running` when the user logs in.
//!
//! `SWARM_SERVICE_DIR` and `SWARM_SERVICE_NAME` override where the file goes
//! and what it's called (used to test without touching the real login items).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
#[cfg(target_os = "macos")]
use swarm_proto::paths;

const DEFAULT_NAME: &str = "dev.swarm.swarmd";

fn name() -> String {
    std::env::var("SWARM_SERVICE_NAME").unwrap_or_else(|_| DEFAULT_NAME.to_string())
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

/// Runs a command and fails with its stderr if it doesn't succeed.
fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("run {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `SWARM_*` overrides in effect at install time, so the service's daemon
/// uses the same runtime and state dirs as the installing client.
fn passthrough_env() -> Vec<(String, String)> {
    ["SWARM_RUNTIME_DIR", "SWARM_STATE_DIR", "SWARM_SHIM_PATH", "SWARM_LOG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// macOS

#[cfg(target_os = "macos")]
fn file() -> Result<PathBuf> {
    let dir = match std::env::var_os("SWARM_SERVICE_DIR") {
        Some(d) => PathBuf::from(d),
        None => home()?.join("Library/LaunchAgents"),
    };
    Ok(dir.join(format!("{}.plist", name())))
}

/// The GUI session's domain when someone is logged in at the screen,
/// otherwise the per-user domain (headless Mac reached over ssh).
#[cfg(target_os = "macos")]
fn domain() -> String {
    let uid = paths::uid();
    let gui = format!("gui/{uid}");
    let ok = Command::new("launchctl").args(["print", &gui]).output().map(|o| o.status.success()).unwrap_or(false);
    if ok {
        gui
    } else {
        format!("user/{uid}")
    }
}

#[cfg(target_os = "macos")]
fn unit_text() -> Result<String> {
    let log = paths::log_dir().join("swarmd.log");
    std::fs::create_dir_all(paths::log_dir())?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
    <string>--if-not-running</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict>{env}</dict>
</dict>
</plist>
"#,
        label = xml_escape(&name()),
        exe = xml_escape(&exe()?.display().to_string()),
        log = xml_escape(&log.display().to_string()),
        env = passthrough_env()
            .iter()
            .map(|(k, v)| format!("<key>{}</key><string>{}</string>", xml_escape(k), xml_escape(v)))
            .collect::<String>(),
    ))
}

#[cfg(target_os = "macos")]
fn activate() -> Result<()> {
    let domain = domain();
    // Replace a previous registration, if any.
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{}", name())]).output();
    run(Command::new("launchctl").args(["bootstrap", &domain]).arg(file()?))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn deactivate() -> Result<()> {
    let _ = Command::new("launchctl").args(["bootout", &format!("{}/{}", domain(), name())]).output();
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_active() -> bool {
    Command::new("launchctl")
        .args(["print", &format!("{}/{}", domain(), name())])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn notes() -> Vec<String> {
    let mut v = Vec::new();
    if domain().starts_with("user/") {
        v.push("No one is logged in at the screen: registered for this user only. After a reboot it starts at the next login (or the next ssh connection runs `swarmd bridge`, which starts it).".into());
    }
    v
}

// ---------------------------------------------------------------------------
// macOS, `with-app` mode
//
// When an app quits, launchd ends every process that descends from it, even
// those in their own session (`setsid`) and reparented to launchd: a daemon
// the app started directly, with all its shims and their shells, got SIGTERM
// when the app quit. So `swarmd start` doesn't start the daemon itself but
// has launchd run it as a job of its own, outside any app. Stopping that job
// only ends the daemon: shims are in their own process groups.

/// The launchd job `swarmd start` runs the daemon as: one per runtime dir, so
/// daemons with their own `SWARM_RUNTIME_DIR` (tests) don't replace it.
#[cfg(target_os = "macos")]
pub fn app_job_label(runtime: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    // FNV-1a: stable across builds, so a newer swarmd replaces the same job.
    let hash = runtime.as_os_str().as_bytes().iter().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(*b)).wrapping_mul(0x0100_0193));
    format!("{DEFAULT_NAME}.app.{hash:08x}")
}

#[cfg(target_os = "macos")]
fn app_job_text(label: &str, exe: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    // The environment `swarmd start` was run in, as a child would have had
    // it (sessions inherit it); not what identifies the calling app to
    // launchd and the system.
    let env = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !k.starts_with("XPC_") && !k.starts_with("__CF"))
        .map(|(k, v)| format!("<key>{}</key><string>{}</string>", xml_escape(&k), xml_escape(&v)))
        .collect::<String>();
    let log = paths::log_dir().join("swarmd.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
    <string>--if-not-running</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>AbandonProcessGroup</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict>{env}</dict>
</dict>
</plist>
"#,
        label = xml_escape(label),
        exe = xml_escape(&String::from_utf8_lossy(exe.as_os_str().as_bytes())),
        log = xml_escape(&log.display().to_string()),
    )
}

/// Has launchd start `exe run --if-not-running` as its own job. The caller
/// waits for the daemon's socket.
#[cfg(target_os = "macos")]
pub fn start_app_job(exe: &std::path::Path) -> Result<()> {
    let runtime = paths::runtime_dir();
    paths::ensure_private_dir(&runtime)?;
    let label = app_job_label(&runtime);
    let plist = runtime.join("swarmd.launchd.plist");
    paths::write_private(&plist, app_job_text(&label, exe).as_bytes())?;
    let domain = domain();
    // The job stays registered after its daemon exits; register it again,
    // as the program may have changed (an upgrade).
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
    let mut result = Ok(String::new());
    for _ in 0..10 {
        // Right after a bootout, launchd may still be removing the job.
        result = run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist));
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // launchd has read it; the environment in it is nobody else's business.
    let _ = std::fs::remove_file(&plist);
    result.map(|_| ())
}

// ---------------------------------------------------------------------------
// Linux

#[cfg(not(target_os = "macos"))]
fn file() -> Result<PathBuf> {
    let dir = match std::env::var_os("SWARM_SERVICE_DIR") {
        Some(d) => PathBuf::from(d),
        None => match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) => PathBuf::from(x).join("systemd/user"),
            None => home()?.join(".config/systemd/user"),
        },
    };
    Ok(dir.join(format!("{}.service", name())))
}

#[cfg(not(target_os = "macos"))]
fn unit_text() -> Result<String> {
    Ok(format!(
        "[Unit]\n\
         Description=swarm-term session daemon\n\
         \n\
         [Service]\n\
         {env}\
         ExecStart=\"{exe}\" run --if-not-running\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # Only the daemon: sessions run in their own processes and must survive\n\
         # a stop or restart of the service.\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe()?.display(),
        env = passthrough_env().iter().map(|(k, v)| format!("Environment=\"{k}={v}\"\n")).collect::<String>(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn unit() -> String {
    format!("{}.service", name())
}

#[cfg(not(target_os = "macos"))]
fn activate() -> Result<()> {
    run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    run(Command::new("systemctl").args(["--user", "enable", "--now", &unit()]))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn deactivate() -> Result<()> {
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", &unit()]).output();
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", &unit()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn linger() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("loginctl").args(["show-user", &user, "--property=Linger", "--value"]).output().ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn notes() -> Vec<String> {
    match linger() {
        Some(true) => vec!["Lingering is on: sessions survive logging out.".into()],
        Some(false) => vec![
            "Lingering is off: systemd may end your sessions when you log out. Run `loginctl enable-linger` to keep them.".into(),
        ],
        None => vec![],
    }
}

// ---------------------------------------------------------------------------

pub fn install(enable_linger: bool) -> Result<()> {
    let path = file()?;
    std::fs::create_dir_all(path.parent().context("service path has no parent")?)?;
    std::fs::write(&path, unit_text()?).with_context(|| format!("write {}", path.display()))?;
    activate()?;
    println!("Installed {} ({})", name(), path.display());
    #[cfg(not(target_os = "macos"))]
    if enable_linger && linger() == Some(false) {
        match run(Command::new("loginctl").arg("enable-linger")) {
            Ok(_) => println!("Enabled lingering so sessions survive logout."),
            Err(e) => println!(
                "Could not enable lingering: {e:#}. Where only an administrator may, they can run `sudo loginctl enable-linger {}`.",
                std::env::var("USER").unwrap_or_else(|_| "<your user>".into())
            ),
        }
    }
    #[cfg(target_os = "macos")]
    let _ = enable_linger;
    for n in notes() {
        println!("{n}");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    deactivate()?;
    let path = file()?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("Removed {} ({})", name(), path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("{} was not installed", name()),
        Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
    }
    println!("The daemon keeps running until stopped; it now starts only when needed.");
    Ok(())
}

pub fn status() -> Result<()> {
    let path = file()?;
    let installed = path.exists();
    let mode = if installed && is_active() { "at-login" } else { "with-app" };
    println!("start mode: {mode}");
    if installed {
        println!("service file: {}", path.display());
    }
    for n in notes() {
        println!("{n}");
    }
    Ok(())
}
