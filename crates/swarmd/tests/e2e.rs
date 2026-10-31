//! End-to-end tests: real `swarmd` and `swarm-shim` processes running real
//! shells in real PTYs, driven through `swarm-client`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use swarm_client::{Attachment, Client, StreamEvent};
use swarm_proto::{
    paths, write_frame, AttachMode, CreateOpts, MetaUpdate, SessionEvent, SessionState, ShimRequest, Size,
};
use tokio::net::UnixStream;
use tokio::time::timeout;

struct Env {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Env {
    async fn new() -> Env {
        // Short path: sockets must fit in sun_path.
        let dir = tempfile::Builder::new().prefix("sw").tempdir_in("/tmp").unwrap();
        let mut env = Env { dir, daemon: None };
        env.start_daemon().await;
        env
    }

    fn runtime(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    async fn start_daemon(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_swarmd"))
            .arg("run")
            .env("SWARM_RUNTIME_DIR", self.runtime())
            .env("SWARM_STATE_DIR", self.dir.path().join("state"))
            .env("SWARM_SHIM_PATH", env!("CARGO_BIN_EXE_swarm-shim"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let sock = paths::daemon_socket(&self.runtime());
        for _ in 0..500 {
            if UnixStream::connect(&sock).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("daemon did not start");
    }

    fn kill_daemon(&mut self) {
        if let Some(mut c) = self.daemon.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    async fn client(&self) -> Client {
        let s = UnixStream::connect(paths::daemon_socket(&self.runtime())).await.unwrap();
        let (rd, wr) = s.into_split();
        Client::handshake(rd, wr, "e2e").await.unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_daemon();
        // Remove every shim this test started so nothing outlives it.
        let shims = paths::shims_dir(&self.runtime());
        let socks: Vec<PathBuf> = std::fs::read_dir(&shims)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        // Drop runs inside the test's runtime; clean up from a fresh thread.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                for sock in socks {
                    remove_shim(&sock).await;
                }
            });
        })
        .join()
        .unwrap();
    }
}

async fn remove_shim(sock: &Path) {
    if let Ok(mut s) = UnixStream::connect(sock).await {
        let _ = write_frame(&mut s, &ShimRequest::Remove).await;
        let mut buf = [0u8; 64];
        let _ = timeout(Duration::from_secs(6), tokio::io::AsyncReadExt::read(&mut s, &mut buf)).await;
    }
}

fn sh_opts(name: &str) -> CreateOpts {
    CreateOpts {
        command: vec!["/bin/sh".into()],
        cwd: Some("/tmp".into()),
        env: vec![("PS1".into(), "$ ".into()), ("ENV".into(), "".into())],
        size: Size { cols: 80, rows: 24 },
        name: Some(name.into()),
        ui: Default::default(),
    }
}

/// Reads output until `needle` appears. Returns (all output, saw any replay chunk).
async fn read_until(att: &mut Attachment, needle: &str) -> (String, bool) {
    let mut out = String::new();
    let mut saw_replay = false;
    let res = timeout(Duration::from_secs(10), async {
        while let Some(ev) = att.recv().await {
            if let StreamEvent::Output { data, replay, .. } = ev {
                saw_replay |= replay;
                out.push_str(&String::from_utf8_lossy(&data));
                if out.contains(needle) {
                    return;
                }
            }
        }
    })
    .await;
    assert!(res.is_ok(), "timed out waiting for {needle:?}; got {out:?}");
    (out, saw_replay)
}

/// Like `read_until`, but returns the offset just past the last byte read.
async fn read_until_offset(att: &mut Attachment, needle: &str) -> u64 {
    let mut out = String::new();
    let mut end = 0;
    timeout(Duration::from_secs(10), async {
        while let Some(ev) = att.recv().await {
            if let StreamEvent::Output { offset, data, .. } = ev {
                end = offset + data.len() as u64;
                out.push_str(&String::from_utf8_lossy(&data));
                if out.contains(needle) {
                    return;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; got {out:?}"));
    end
}

/// Reads output for a while and returns it.
async fn read_for(att: &mut Attachment, dur: Duration) -> String {
    let mut out = String::new();
    let _ = timeout(dur, async {
        while let Some(ev) = att.recv().await {
            if let StreamEvent::Output { data, .. } = ev {
                out.push_str(&String::from_utf8_lossy(&data));
            }
        }
    })
    .await;
    out
}

#[tokio::test]
async fn create_attach_detach_and_replay() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("one")).await.unwrap();
    assert!(info.is_running());

    let mut a = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    a.input(b"echo hello-$((40+2))\n".to_vec()).await.unwrap();
    read_until(&mut a, "hello-42").await;
    a.detach().await.unwrap();

    // Another connection, as after the app restarted.
    let c2 = env.client().await;
    let listed = c2.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, info.id);
    assert_eq!(listed[0].name.as_deref(), Some("one"));

    let mut b = c2.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    assert_eq!(b.replay_from, 0);
    let (out, saw_replay) = read_until(&mut b, "hello-42").await;
    assert!(saw_replay, "history must arrive marked as replay: {out:?}");
    b.input(b"echo after-$((1+1))\n".to_vec()).await.unwrap();
    read_until(&mut b, "after-2").await;
}

#[tokio::test]
async fn resume_from_offset_sends_only_the_delta() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("resume")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    a.input(b"echo first-mark\n".to_vec()).await.unwrap();
    // Wait for the prompt after the output, so nothing of it comes later.
    let resume_at = read_until_offset(&mut a, "first-mark\r\n$ ").await;
    a.input(b"echo second-mark\n".to_vec()).await.unwrap();
    read_until(&mut a, "second-mark\r\n").await;
    a.detach().await.unwrap();

    let mut b = c.attach(&info.id, AttachMode::ResumeFrom(resume_at), None, false).await.unwrap();
    assert_eq!(b.replay_from, resume_at);
    let (out, _) = read_until(&mut b, "second-mark").await;
    assert!(!out.contains("first-mark"), "resume replayed old output: {out:?}");
}

#[tokio::test]
async fn two_clients_see_the_same_output() {
    let env = Env::new().await;
    let c1 = env.client().await;
    let c2 = env.client().await;
    let info = c1.create(sh_opts("shared")).await.unwrap();
    let mut a = c1.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let mut b = c2.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let listed = c1.list().await.unwrap();
    assert_eq!(listed[0].clients.len(), 2, "attach must be reflected in the list right away");
    a.input(b"echo both-$((2*3))\n".to_vec()).await.unwrap();
    read_until(&mut a, "both-6").await;
    read_until(&mut b, "both-6").await;
}

#[tokio::test]
async fn attach_size_resizes_the_pty() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("size")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, Some(Size { cols: 101, rows: 33 }), false).await.unwrap();
    a.input(b"stty size\n".to_vec()).await.unwrap();
    read_until(&mut a, "33 101").await;
    a.resize(Size { cols: 120, rows: 40 }).await.unwrap();
    // The resize reaches the PTY asynchronously; under load a `stty size`
    // sent right after it can still see the old size, so ask until it doesn't.
    let mut seen = String::new();
    for _ in 0..20 {
        a.input(b"stty size\n".to_vec()).await.unwrap();
        seen.push_str(&read_for(&mut a, Duration::from_millis(500)).await);
        if seen.contains("40 120") {
            return;
        }
    }
    panic!("the PTY never reported 40 120; got {seen:?}");
}

#[tokio::test]
async fn sessions_survive_daemon_restart() {
    let mut env = Env::new().await;
    let id = {
        let c = env.client().await;
        let info = c.create(sh_opts("survivor")).await.unwrap();
        let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
        a.input(b"echo before-restart\n".to_vec()).await.unwrap();
        read_until(&mut a, "before-restart\r\n").await;
        info.id
    };
    env.kill_daemon();
    env.start_daemon().await;

    let c = env.client().await;
    let listed = c.list().await.unwrap();
    assert_eq!(listed.len(), 1, "session not re-adopted");
    assert_eq!(listed[0].id, id);
    assert!(listed[0].is_running());
    let mut a = c.attach(&id, AttachMode::Full, None, false).await.unwrap();
    read_until(&mut a, "before-restart").await;
    a.input(b"echo after-restart\n".to_vec()).await.unwrap();
    read_until(&mut a, "after-restart\r\n").await;
}

#[tokio::test]
async fn exit_code_is_reported_and_remove_drops_session() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut events = c.subscribe().await.unwrap();
    let info = c.create(sh_opts("exiting")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"exit 3\n".to_vec()).await.unwrap();
    let exited = timeout(Duration::from_secs(10), async {
        loop {
            match a.recv().await {
                Some(StreamEvent::Exited { code, signal }) => return (code, signal),
                Some(_) => continue,
                None => panic!("stream ended without Exited"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(exited, (Some(3), None));

    // The exited session stays listed until removed.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let listed = c.list().await.unwrap();
    assert!(matches!(listed[0].state, SessionState::Exited { code: Some(3), .. }));

    c.remove(&info.id).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(SessionEvent::Removed { id }) = events.recv().await {
                if id == info.id {
                    return;
                }
            }
        }
    })
    .await
    .expect("no Removed event");
    assert!(c.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn kill_reports_signal() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("killme")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    c.kill(&info.id, libc::SIGKILL).await.unwrap();
    let ev = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(StreamEvent::Exited { code, signal }) = a.recv().await {
                return (code, signal);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ev, (None, Some(libc::SIGKILL)));
}

#[tokio::test]
async fn events_and_metadata() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut events = c.subscribe().await.unwrap();
    let info = c.create(sh_opts("meta")).await.unwrap();
    let mut ui = std::collections::BTreeMap::new();
    ui.insert("tab_order".to_string(), "3".to_string());
    let updated = c.set_meta(&info.id, MetaUpdate { name: Some(Some("renamed".into())), ui }).await.unwrap();
    assert_eq!(updated.name.as_deref(), Some("renamed"));
    assert_eq!(updated.ui.get("tab_order").map(|s| s.as_str()), Some("3"));

    let mut saw_created = false;
    let mut saw_renamed = false;
    timeout(Duration::from_secs(5), async {
        while !(saw_created && saw_renamed) {
            match events.recv().await.unwrap() {
                SessionEvent::Created(s) if s.id == info.id => saw_created = true,
                SessionEvent::Updated(s) if s.id == info.id && s.name.as_deref() == Some("renamed") => {
                    saw_renamed = true
                }
                _ => {}
            }
        }
    })
    .await
    .expect("missing events");
}

#[tokio::test]
async fn readonly_clients_cannot_type() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("ro")).await.unwrap();
    let mut rw = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let ro = c.attach(&info.id, AttachMode::Live, None, true).await.unwrap();
    ro.input(b"echo from-$((7*7))\n".to_vec()).await.unwrap();
    let out = read_for(&mut rw, Duration::from_millis(700)).await;
    assert!(!out.contains("from-49"), "read-only input reached the shell: {out:?}");
    rw.input(b"echo rw-$((7*7))\n".to_vec()).await.unwrap();
    read_until(&mut rw, "rw-49").await;
}

#[tokio::test]
async fn tracks_title_cwd_and_alt_screen() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("track")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"printf '\\033]2;my title\\007\\033]7;file://h/tmp/a%%20b\\007\\033[?1049h'; echo done-mark\n".to_vec())
        .await
        .unwrap();
    read_until(&mut a, "done-mark\r\n").await;
    let s = timeout(Duration::from_secs(3), async {
        loop {
            let s = c.list().await.unwrap().remove(0);
            if s.alt_screen && s.title == "my title" {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("info not updated");
    assert_eq!(s.cwd.as_deref(), Some("/tmp/a b"));
}

#[tokio::test]
async fn missing_cwd_falls_back_to_home() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut opts = sh_opts("nocwd");
    opts.cwd = Some("/definitely/not/here".into());
    let info = c.create(opts).await.unwrap();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(info.cwd.as_deref(), Some(home.as_str()));
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"echo \"cwd=$(pwd)\"\n".to_vec()).await.unwrap();
    read_until(&mut a, &format!("cwd={home}")).await;
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn replay_keeps_warp_session_hooks_and_starts_at_precmd() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("hooks")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let init = hex(r#"{"hook": "InitShell", "value": {"session_id": 42}}"#);
    let boot = hex(r#"{"hook": "Bootstrapped", "value": {"session_id": 42}}"#);
    let precmd = hex(r#"{"hook": "Precmd", "value": {"session_id": 42}}"#);
    let p = r"printf '\033P$d%s\033\\'";
    let script = format!(
        "{p} {init}; {p} {boot}; head -c 17000000 /dev/zero | tr '\\0' x; {p} {precmd}; echo; echo flood-$((1+1))\n"
    );
    a.input(script.into_bytes()).await.unwrap();
    // Reading the flood live is slow under load; wait on the session instead.
    timeout(Duration::from_secs(60), async {
        loop {
            let s = c.list().await.unwrap().into_iter().find(|s| s.id == info.id).unwrap();
            if s.buffer_end > 17_000_000 && s.foreground.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("flood did not finish");
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(a);

    let mut b = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    timeout(Duration::from_secs(10), async {
        while let Some(ev) = b.recv().await {
            if let StreamEvent::Output { data, replay: true, .. } = ev {
                total += data.len();
                chunks.push(data);
                if String::from_utf8_lossy(chunks.last().unwrap()).contains("flood-2\r\n") {
                    return;
                }
            }
        }
    })
    .await
    .expect("replay did not finish");
    let starts = |c: &[u8], hex: &str| c.starts_with(format!("\x1bP$d{hex}\x1b\\").as_bytes());
    assert!(starts(&chunks[0], &init), "first replayed chunk must be the kept InitShell hook");
    assert!(starts(&chunks[1], &boot), "second replayed chunk must be the kept Bootstrapped hook");
    assert!(total < 64 * 1024, "replay should start at the Precmd boundary, got {total} bytes");
    let rest: Vec<u8> = chunks[2..].concat();
    assert!(rest.starts_with(format!("\x1bP$d{precmd}").as_bytes()), "replay must start at the Precmd hook");
}

#[tokio::test]
async fn replay_reopens_a_command_whose_start_was_trimmed() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("trimmed")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let init = hex(r#"{"hook": "InitShell", "value": {"session_id": 42}}"#);
    let precmd = hex(r#"{"hook": "Precmd", "value": {"session_id": 42}}"#);
    let preexec = hex(r#"{"hook": "Preexec", "value": {"command": "yes | head"}}"#);
    let p = r"printf '\033P$d%s\033\\'";
    // A command printing more than the 16 MB buffer, then the next prompt.
    let script = format!(
        "{p} {init}; {p} {precmd}; {p} {preexec}; yes line | head -c 20000000; {p} {precmd}; echo flood-$((1+1))\n"
    );
    a.input(script.into_bytes()).await.unwrap();
    // The live client falls behind during the flood; wait on the session.
    timeout(Duration::from_secs(60), async {
        loop {
            let s = c.list().await.unwrap().into_iter().find(|s| s.id == info.id).unwrap();
            if s.buffer_end > 24_000_000 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("flood did not finish");
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(a);

    let mut b = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    timeout(Duration::from_secs(20), async {
        while let Some(ev) = b.recv().await {
            if let StreamEvent::Output { data, replay: true, .. } = ev {
                total += data.len();
                let done = String::from_utf8_lossy(&data).contains("flood-2\r\n");
                chunks.push(data);
                if done {
                    return;
                }
            }
        }
    })
    .await
    .expect("replay did not finish");
    let hook = |hex: &str| format!("\x1bP$d{hex}\x1b\\").into_bytes();
    let head = |c: &[u8]| String::from_utf8_lossy(&c[..c.len().min(200)]).into_owned();
    assert!(chunks[0].starts_with(&hook(&init)), "first comes the kept InitShell hook; got {:?} ({} chunks, {total} bytes, from {})", head(&chunks[0]), chunks.len(), b.replay_from);
    let header = [hook(&precmd), hook(&preexec)].concat();
    assert!(chunks[1].starts_with(&header), "then the trimmed command's Precmd and Preexec; got {:?}", String::from_utf8_lossy(&chunks[1][..chunks[1].len().min(300)]));
    let rest: Vec<u8> = chunks[2..].concat();
    assert!(rest.starts_with(b"line\r\n"), "the kept output starts on a whole line");
    assert!(total > 15_000_000, "the command's kept output is replayed, got {total} bytes");
}

#[tokio::test]
async fn smallest_size_policy_fits_every_writable_client() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("smallest")).await.unwrap();
    let mut ui = std::collections::BTreeMap::new();
    ui.insert("size_policy".to_string(), "smallest".to_string());
    c.set_meta(&info.id, MetaUpdate { name: None, ui }).await.unwrap();
    let mut big = c.attach(&info.id, AttachMode::Live, Some(Size { cols: 150, rows: 50 }), false).await.unwrap();
    let small = c.attach(&info.id, AttachMode::Live, Some(Size { cols: 90, rows: 30 }), false).await.unwrap();
    // The big client types; the PTY keeps the small client's size anyway.
    let mut seen = String::new();
    for _ in 0..20 {
        big.input(b"stty size\n".to_vec()).await.unwrap();
        seen.push_str(&read_for(&mut big, Duration::from_millis(400)).await);
        if seen.contains("30 90") {
            break;
        }
    }
    assert!(seen.contains("30 90"), "PTY should fit the smaller client; got {seen:?}");
    // When the small client leaves, the PTY grows to the remaining one.
    small.detach().await.unwrap();
    let mut seen = String::new();
    for _ in 0..20 {
        big.input(b"stty size\n".to_vec()).await.unwrap();
        seen.push_str(&read_for(&mut big, Duration::from_millis(400)).await);
        if seen.contains("50 150") {
            return;
        }
    }
    panic!("PTY should grow back to 50x150; got {seen:?}");
}

#[tokio::test]
async fn restore_after_reboot_recreates_lost_sessions() {
    let mut env = Env::new().await;
    std::fs::create_dir_all(env.dir.path().join("state")).unwrap();
    std::fs::write(env.dir.path().join("state").join("restore-after-reboot"), b"").unwrap();
    let c = env.client().await;
    let mut opts = sh_opts("nightly");
    opts.ui.insert("group".into(), "builds".into());
    opts.ui.insert("warp.session_id".into(), "42".into());
    let info = c.create(opts).await.unwrap();
    // Give the snapshot writer time to record it.
    tokio::time::sleep(Duration::from_secs(3)).await;
    drop(c);

    // "Reboot": the daemon and the session's shim are gone.
    env.kill_daemon();
    let _ = Command::new("pkill").args(["-f", &format!("swarm-shim --id {}", info.id)]).status();
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.start_daemon().await;

    let c = env.client().await;
    let sessions = c.list().await.unwrap();
    let restored = sessions
        .iter()
        .find(|s| s.ui.get("restored_from") == Some(&info.id))
        .unwrap_or_else(|| panic!("no session restored from {}; have {sessions:?}", info.id));
    assert_ne!(restored.id, info.id);
    assert_eq!(restored.name.as_deref(), Some("nightly"));
    assert_eq!(restored.cwd.as_deref(), Some("/tmp"));
    assert_eq!(restored.ui.get("group").map(String::as_str), Some("builds"));
    assert!(!restored.ui.contains_key("warp.session_id"));
    assert!(restored.is_running());
}
