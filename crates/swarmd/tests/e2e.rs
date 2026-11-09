//! End-to-end tests: real `swarmd` and `swarm-shim` processes running real
//! shells in real PTYs, driven through `swarm-client`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use swarm_client::{Attachment, Client, StreamEvent};
use swarm_proto::{
    paths, write_frame, AttachMode, CreateOpts, MetaUpdate, SessionEvent, SessionState, ShimRequest,
    ShimUpgrade, ShimUpgradeOutcome, Size,
};
use tokio::net::UnixStream;
use tokio::time::timeout;

struct Env {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
    /// Extra environment for the daemon.
    vars: Vec<(&'static str, String)>,
}

impl Env {
    async fn new() -> Env {
        Env::with_vars(Vec::new()).await
    }

    async fn with_vars(vars: Vec<(&'static str, String)>) -> Env {
        // Short path: sockets must fit in sun_path.
        let dir = tempfile::Builder::new().prefix("sw").tempdir_in("/tmp").unwrap();
        let mut env = Env { dir, daemon: None, vars };
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
            .envs(self.vars.iter().map(|(k, v)| (k, v)))
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
        // Any shim left (its socket missing at this moment, e.g. after a test
        // removed the runtime folder) would otherwise outlive the test.
        let _ = Command::new("pkill").args(["-f", &format!("--runtime-dir {}", self.runtime().display())]).status();
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
async fn daemon_started_through_a_symlink_finds_its_shim() {
    let dir = tempfile::Builder::new().prefix("sw").tempdir_in("/tmp").unwrap();
    let link = dir.path().join("swarmd");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_swarmd"), &link).unwrap();
    let swarmd = |arg: &str| {
        Command::new(&link)
            .arg(arg)
            .env("SWARM_RUNTIME_DIR", dir.path().join("run"))
            .env("SWARM_STATE_DIR", dir.path().join("state"))
            .env_remove("SWARM_SHIM_PATH")
            .output()
            .unwrap()
    };
    let started = swarmd("start");
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    assert!(swarmd("stop").status.success());
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
async fn close_drops_the_connection_but_not_the_session() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("closing")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    c.close();
    let ev = tokio::time::timeout(Duration::from_secs(5), a.recv()).await.expect("no event after close");
    assert!(matches!(ev, Some(StreamEvent::Closed { .. })), "attachment must close: {ev:?}");
    assert!(c.ping().await.is_err(), "requests must fail after close");

    let c2 = env.client().await;
    let listed = c2.list().await.unwrap();
    assert!(listed.iter().any(|s| s.id == info.id && s.is_running()), "the session must keep running");
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
async fn shim_update_in_place_keeps_a_running_command_going() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut events = c.subscribe().await.unwrap();
    let mut opts = sh_opts("counter");
    opts.command = vec![
        "/bin/sh".into(),
        "-c".into(),
        "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo n$i; sleep 0.01; done; sleep 30".into(),
    ];
    let info = c.create(opts).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    let mut out = read_until(&mut a, "n20\r\n").await.0;

    let results = c.upgrade_shims(true).await.unwrap();
    assert_eq!(results, vec![ShimUpgrade { id: info.id.clone(), outcome: ShimUpgradeOutcome::Upgraded }]);

    // The same attachment carries on: every line arrives once, in order.
    out.push_str(&read_until(&mut a, "n300\r\n").await.0);
    let numbers: Vec<u32> = out.split("\r\n").filter_map(|l| l.strip_prefix('n')?.parse().ok()).collect();
    assert_eq!(numbers, (1..=300).collect::<Vec<_>>());

    // Same process, still running, and never reported gone.
    let listed = c.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].pid, info.pid);
    assert!(listed[0].is_running());
    while let Ok(ev) = events.try_recv() {
        assert!(!matches!(ev, SessionEvent::Removed { .. }), "the session was reported removed");
    }
}

#[tokio::test]
async fn updated_shim_replays_its_scrollback_and_reports_the_exit_code() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("updated")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"echo before-update\n".to_vec()).await.unwrap();
    read_until(&mut a, "before-update\r\n").await;

    for _ in 0..2 {
        let results = c.upgrade_shims(true).await.unwrap();
        assert_eq!(results[0].outcome, ShimUpgradeOutcome::Upgraded);
    }

    let mut b = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    read_until(&mut b, "before-update").await;
    b.input(b"echo after-update\n".to_vec()).await.unwrap();
    read_until(&mut b, "after-update\r\n").await;
    b.input(b"exit 7\n".to_vec()).await.unwrap();
    let exited = timeout(Duration::from_secs(10), async {
        loop {
            match b.recv().await {
                Some(StreamEvent::Exited { code, signal }) => return (code, signal),
                Some(_) => continue,
                None => panic!("stream ended without Exited"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(exited, (Some(7), None));
}

#[tokio::test]
async fn a_shim_program_that_refuses_the_handoff_leaves_the_session_alone() {
    let mut env = Env::new().await;
    let id = {
        let c = env.client().await;
        let info = c.create(sh_opts("stays")).await.unwrap();
        let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
        a.input(b"echo one\n".to_vec()).await.unwrap();
        read_until(&mut a, "one\r\n").await;
        info.id
    };
    // A daemon whose shim program can't take sessions over.
    let refusing = env.dir.path().join("refusing-shim");
    std::fs::write(&refusing, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&refusing, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    env.kill_daemon();
    env.vars = vec![("SWARM_SHIM_PATH", refusing.display().to_string())];
    env.start_daemon().await;

    let c = env.client().await;
    let results = c.upgrade_shims(true).await.unwrap();
    assert!(matches!(&results[0].outcome, ShimUpgradeOutcome::Failed(_)), "{results:?}");
    let mut a = c.attach(&id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"echo still-here\n".to_vec()).await.unwrap();
    read_until(&mut a, "still-here\r\n").await;
}

/// Runs `swarmd <args>` against the test's daemon.
fn swarmd_cli(env: &Env, args: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_swarmd"))
        .args(args)
        .env("SWARM_RUNTIME_DIR", env.runtime())
        .env("SWARM_STATE_DIR", env.dir.path().join("state"))
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "swarmd {args:?} failed");
}

/// Everything a `History` attachment sends before live output.
async fn read_history(att: &mut Attachment) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = att.replay_from;
    timeout(Duration::from_secs(60), async {
        while at < att.live_from {
            match att.recv().await {
                Some(StreamEvent::Output { offset, data, .. }) => {
                    assert_eq!(offset, at, "history has a gap or overlap");
                    at = offset + data.len() as u64;
                    out.extend(data);
                }
                Some(_) => {}
                None => break,
            }
        }
    })
    .await
    .expect("history did not arrive");
    out
}

/// The numbers printed one per line (the terminal ends lines with `\r\n`,
/// now and then `\r\r\n`).
fn seq_numbers(bytes: &[u8]) -> Vec<u64> {
    String::from_utf8_lossy(bytes).split('\n').filter_map(|l| l.trim_end_matches('\r').parse().ok()).collect()
}

#[tokio::test]
async fn spill_to_disk_keeps_what_leaves_the_buffer_through_a_shim_update() {
    let env = Env::new().await;
    swarmd_cli(&env, &["spill", "on", "--limit-mb", "64"]);
    let c = env.client().await;
    let info = c.create(sh_opts("spilled")).await.unwrap();
    let a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    // About 19 MB: more than the 16 MB kept in memory.
    a.input(b"awk 'BEGIN { for (i = 1; i <= 2500000; i++) print i }'; echo seq-done\n".to_vec()).await.unwrap();
    drop(a);
    let total: u64 = (1..=2_500_000u64).map(|n| n.to_string().len() as u64 + 2).sum();
    timeout(Duration::from_secs(60), async {
        loop {
            let s = c.list().await.unwrap().into_iter().find(|s| s.id == info.id).unwrap();
            if s.buffer_end > total && s.foreground.is_none() {
                assert!(s.buffer_start > 0, "the buffer should have wrapped");
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("seq did not finish");

    let check = |history: &[u8]| {
        let numbers = seq_numbers(history);
        assert_eq!(numbers.len(), 2_500_000, "every line is kept");
        assert!(numbers.iter().zip(1..).all(|(n, i)| *n == i), "lines are in order, once each");
    };
    let mut h = c.attach(&info.id, AttachMode::History, None, true).await.unwrap();
    assert!(h.replay_from < 1024, "history starts at the session's start");
    check(&read_history(&mut h).await);
    drop(h);

    // On disk only ciphertext: no run of seq's output is there in the clear.
    let spill_dir = env.dir.path().join("state/spill").join(&info.id);
    let files: Vec<PathBuf> = std::fs::read_dir(&spill_dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert!(!files.is_empty(), "nothing was spilled");
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        assert!(!bytes.windows(9).any(|w| w == b"\r\n123456\r\n" || w == b"0\r\n100001"), "plaintext in {f:?}");
    }

    // The spill and its key move with the session to an updated shim.
    let results = c.upgrade_shims(true).await.unwrap();
    assert_eq!(results[0].outcome, ShimUpgradeOutcome::Upgraded);
    let mut h = c.attach(&info.id, AttachMode::History, None, true).await.unwrap();
    check(&read_history(&mut h).await);
    drop(h);

    // Turning it off deletes what was spilled.
    swarmd_cli(&env, &["spill", "off"]);
    assert!(!spill_dir.exists(), "spill off should delete the files");
}

#[tokio::test]
async fn removing_a_session_deletes_its_spilled_output() {
    let env = Env::new().await;
    swarmd_cli(&env, &["spill", "on", "--limit-mb", "8"]);
    let c = env.client().await;
    let info = c.create(sh_opts("short-lived")).await.unwrap();
    let a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"head -c 20000000 /dev/zero | tr '\\0' y; echo; echo filled\n".to_vec()).await.unwrap();
    drop(a);
    let spill_dir = env.dir.path().join("state/spill").join(&info.id);
    timeout(Duration::from_secs(60), async {
        while std::fs::read_dir(&spill_dir).map(|d| d.count()).unwrap_or(0) < 2 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("nothing was spilled");
    c.remove(&info.id).await.unwrap();
    timeout(Duration::from_secs(10), async {
        while spill_dir.exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the spill outlived its session");
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
    let a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let init = hex(r#"{"hook": "InitShell", "value": {"session_id": 42}}"#);
    let boot = hex(r#"{"hook": "Bootstrapped", "value": {"session_id": 42}}"#);
    let precmd = hex(r#"{"hook": "Precmd", "value": {"session_id": 42}}"#);
    let p = r"printf '\033P$d%s\033\\'";
    let script = format!(
        "{p} {init}; {p} {boot}; head -c 17000000 /dev/zero | tr '\\0' x; {p} {precmd}; echo; echo flood-$((1+1))\n"
    );
    a.input(script.into_bytes()).await.unwrap();
    // Reading the flood live is slow under load; wait on the session instead.
    timeout(Duration::from_secs(180), async {
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
    let rest = rest.strip_prefix(b"\x1b]1337;SwarmTime=").expect("the Precmd hook comes with its time");
    let rest = &rest[rest.iter().position(|&b| b == 0x07).unwrap() + 1..];
    assert!(rest.starts_with(format!("\x1bP$d{precmd}").as_bytes()), "replay must start at the Precmd hook");
}

#[tokio::test]
async fn replay_reopens_a_command_whose_start_was_trimmed() {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("trimmed")).await.unwrap();
    let a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
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
    timeout(Duration::from_secs(180), async {
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
    let reopened = chunks[1].strip_prefix(b"\x1b]1337;SwarmTime=").expect("the reopened block keeps its start time");
    let reopened = &reopened[reopened.iter().position(|&b| b == 0x07).unwrap() + 1..];
    assert!(reopened.starts_with(&header), "then the trimmed command's Precmd and Preexec; got {:?}", String::from_utf8_lossy(&chunks[1][..chunks[1].len().min(300)]));
    let rest: Vec<u8> = chunks[2..].concat();
    assert!(rest.starts_with(b"line\r\n"), "the kept output starts on a whole line");
    // The block keeps its head and its last 50,000 lines (what a terminal
    // keeps per block), not all 16 MB.
    assert!(total > 250_000 && total < 2_000_000, "the command's tail is replayed, got {total} bytes");
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

#[tokio::test]
async fn full_attach_to_a_full_screen_app_ends_with_its_screen() {
    full_screen_app_attach(false).await;
}

#[tokio::test]
async fn a_full_screen_app_keeps_its_screen_across_a_shim_update() {
    full_screen_app_attach(true).await;
}

async fn full_screen_app_attach(update_shim: bool) {
    let env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("altscreen")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, Some(Size { cols: 80, rows: 24 }), false).await.unwrap();
    a.input(b"printf '\\033[?1049h\\033[2J\\033[5;10HALT-SCREEN-MARK'; sleep 30\n".to_vec()).await.unwrap();
    read_until(&mut a, "ALT-SCREEN-MARK").await;
    // Wait until the shim has seen the switch to the alternate screen.
    timeout(Duration::from_secs(10), async {
        while !c.list().await.unwrap().iter().any(|s| s.id == info.id && s.alt_screen) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("alt screen not reported");
    a.detach().await.unwrap();
    if update_shim {
        let results = c.upgrade_shims(true).await.unwrap();
        assert_eq!(results[0].outcome, ShimUpgradeOutcome::Upgraded);
    }

    let mut b = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    let mut last = Vec::new();
    let _ = timeout(Duration::from_secs(3), async {
        while let Some(ev) = b.recv().await {
            match ev {
                StreamEvent::Output { data, replay: true, offset } => {
                    assert!(offset + data.len() as u64 <= b.live_from, "replay must not run past live_from");
                    last = data;
                }
                StreamEvent::Output { replay: false, .. } => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(last.starts_with(b"\x1b[?1049h"), "last replayed frame is the screen snapshot");
    let text = String::from_utf8_lossy(&last);
    assert!(text.contains("ALT-SCREEN-MARK"), "snapshot shows the app's screen: {text:?}");
}

/// Keystroke round trip through daemon and shim (client → swarmd → shim →
/// pty → `cat` → back). Run with `--ignored --nocapture` to see the numbers.
#[tokio::test]
#[ignore]
async fn bench_keystroke_round_trip() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut opts = sh_opts("latency");
    opts.command = vec!["/bin/cat".into()];
    let info = c.create(opts).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let mut samples = Vec::new();
    for i in 0..300 {
        let ch = [b'a' + (i % 26) as u8];
        let t = std::time::Instant::now();
        a.input(ch.to_vec()).await.unwrap();
        loop {
            match a.recv().await {
                Some(StreamEvent::Output { data, .. }) if data.contains(&ch[0]) => break,
                Some(_) => {}
                None => panic!("stream closed"),
            }
        }
        samples.push(t.elapsed());
    }
    samples.sort();
    let pct = |p: usize| samples[samples.len() * p / 100];
    eprintln!("keystroke round trip: p50 {:?}, p90 {:?}, p99 {:?}", pct(50), pct(90), pct(99));
}

/// New session to first prompt (`/bin/sh`, so no rc files). Run with
/// `--ignored --nocapture` to see the numbers.
#[tokio::test]
#[ignore]
async fn bench_new_session_to_prompt() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut samples = Vec::new();
    for i in 0..20 {
        let t = std::time::Instant::now();
        let info = c.create(sh_opts(&format!("new-{i}"))).await.unwrap();
        let mut a = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
        read_until(&mut a, "$ ").await;
        samples.push(t.elapsed());
        c.remove(&info.id).await.unwrap();
    }
    samples.sort();
    eprintln!("new session to prompt: p50 {:?}, p90 {:?}, max {:?}", samples[10], samples[18], samples[19]);
}

#[tokio::test]
async fn scrollback_over_the_total_cap_is_trimmed_from_idle_sessions() {
    // A 6 MB cap; two sessions printing 4 MB each.
    let env = Env::with_vars(vec![("SWARM_SCROLLBACK_TOTAL", (6u64 << 20).to_string())]).await;
    let c = env.client().await;
    let held = |s: &swarm_proto::SessionInfo| s.buffer_end - s.buffer_start;
    let mut ids = Vec::new();
    for name in ["old", "new"] {
        let info = c.create(sh_opts(name)).await.unwrap();
        let a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
        a.input(b"head -c 4194304 /dev/zero | tr '\\0' x; echo; echo done-$((1+1))\n".to_vec()).await.unwrap();
        timeout(Duration::from_secs(60), async {
            while held(&c.list().await.unwrap().into_iter().find(|s| s.id == info.id).unwrap()) < 4 << 20 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("output did not arrive");
        drop(a);
        ids.push(info.id);
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    // The daemon checks every 10 s; the older idle session is trimmed to 1 MB.
    let trimmed = timeout(Duration::from_secs(30), async {
        loop {
            let list = c.list().await.unwrap();
            let get = |id: &str| list.iter().find(|s| s.id == id).cloned().unwrap();
            let (old, new) = (get(&ids[0]), get(&ids[1]));
            if held(&old) <= 1 << 20 {
                return (old, new);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("the older session was never trimmed");
    assert!(held(&trimmed.1) > 4_000_000, "the newer session keeps its scrollback");
    // A full replay of the trimmed session still works and ends with its prompt.
    let mut a = c.attach(&ids[0], AttachMode::Full, None, false).await.unwrap();
    let (out, replayed) = read_until(&mut a, "done-2").await;
    assert!(replayed && out.len() <= 1 << 20, "replay of {} bytes", out.len());
}

// ---------------------------------------------------------------------------
// Resilience

/// The shim process running session `info` (the shell's parent).
fn shim_pid(shell_pid: u32) -> u32 {
    let out = Command::new("ps").args(["-o", "ppid=", "-p", &shell_pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// Waits up to `secs` for a fresh connection whose `list` has session `id`.
async fn wait_listed(env: &Env, id: &str, secs: u64) -> Client {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(s) = UnixStream::connect(paths::daemon_socket(&env.runtime())).await {
            let (rd, wr) = s.into_split();
            if let Ok(c) = Client::handshake(rd, wr, "e2e").await {
                if let Ok(Ok(listed)) = timeout(Duration::from_secs(2), c.list()).await {
                    if listed.iter().any(|s| s.id == id && s.is_running()) {
                        return c;
                    }
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "session {id} not reachable within {secs} s");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// systemd deletes `/run/user/<uid>` at the last logout when lingering is
/// off, taking every socket with it while the processes keep running.
#[tokio::test]
async fn sessions_stay_reachable_after_the_runtime_dir_is_removed() {
    let mut env = Env::new().await;
    let c = env.client().await;
    let info = c.create(sh_opts("logout")).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"echo before-logout\n".to_vec()).await.unwrap();
    read_until(&mut a, "before-logout\r\n").await;
    drop(a);
    c.close();

    std::fs::remove_dir_all(env.runtime()).unwrap();

    let c = wait_listed(&env, &info.id, 15).await;
    assert!(env.daemon.as_mut().unwrap().try_wait().unwrap().is_none(), "the same daemon keeps running");
    let mut a = c.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    read_until(&mut a, "before-logout").await;
    a.input(b"echo after-login\n".to_vec()).await.unwrap();
    read_until(&mut a, "after-login\r\n").await;
}

/// A client may start a new daemon before the old one notices its socket is
/// gone. One daemon must be left, with the session.
#[tokio::test]
async fn a_new_daemon_takes_over_after_the_runtime_dir_is_removed() {
    let mut env = Env::new().await;
    let info = env.client().await.create(sh_opts("handover")).await.unwrap();
    let mut old = env.daemon.take().unwrap();

    std::fs::remove_dir_all(env.runtime()).unwrap();
    env.start_daemon().await;

    let c = wait_listed(&env, &info.id, 15).await;
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"echo still-here\n".to_vec()).await.unwrap();
    read_until(&mut a, "still-here\r\n").await;

    // Within a few checks, only one of the two daemons is left.
    let mut alive = 2;
    for _ in 0..100 {
        alive = [old.try_wait().unwrap().is_none(), env.daemon.as_mut().unwrap().try_wait().unwrap().is_none()]
            .iter()
            .filter(|a| **a)
            .count();
        if alive == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = old.kill();
    let _ = old.wait();
    assert_eq!(alive, 1, "both daemons are still running");
}

/// A shim that accepts connections but never answers must not hold up the
/// daemon's start or other sessions.
#[tokio::test]
async fn a_stopped_shim_does_not_hold_up_the_daemon() {
    let mut env = Env::new().await;
    let c = env.client().await;
    let stuck = c.create(sh_opts("stuck")).await.unwrap();
    let fine = c.create(sh_opts("fine")).await.unwrap();
    c.close();
    let stuck_shim = shim_pid(stuck.pid);
    unsafe { libc::kill(stuck_shim as i32, libc::SIGSTOP) };

    env.kill_daemon();
    env.start_daemon().await;
    let result = timeout(Duration::from_secs(20), async {
        let c = wait_listed(&env, &fine.id, 15).await;
        let mut a = c.attach(&fine.id, AttachMode::Live, None, false).await.unwrap();
        a.input(b"echo fine-here\n".to_vec()).await.unwrap();
        read_until(&mut a, "fine-here\r\n").await;
        // The stopped one fails in bounded time instead of hanging.
        let started = std::time::Instant::now();
        let r = c.attach(&stuck.id, AttachMode::Live, None, false).await;
        (r.is_err(), started.elapsed())
    })
    .await;
    unsafe { libc::kill(stuck_shim as i32, libc::SIGCONT) };
    let (failed, took) = result.expect("the daemon hung on a stopped shim");
    assert!(failed, "attaching to a stopped shim can't succeed");
    assert!(took < Duration::from_secs(8), "took {took:?}");
    // Once it runs again, the daemon picks it up.
    wait_listed(&env, &stuck.id, 15).await;
}

/// Output that crashes the screen model (`vt100`) must not take the session
/// with it: here a wide character in a one-column terminal.
#[tokio::test]
async fn output_that_crashes_the_screen_model_leaves_the_session_working() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut opts = sh_opts("narrow");
    opts.size = Size { cols: 1, rows: 24 };
    let info = c.create(opts).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"printf '\\346\\274\\242\\n'; echo after-$((6*7))\n".to_vec()).await.unwrap();
    read_until(&mut a, "after-42").await;
    a.resize(Size { cols: 80, rows: 24 }).await.unwrap();
    a.input(b"echo still-$((2+3))\n".to_vec()).await.unwrap();
    read_until(&mut a, "still-5").await;
    // Info and a new full attach still work.
    let c2 = env.client().await;
    assert!(c2.list().await.unwrap().iter().any(|s| s.id == info.id && s.is_running()));
    let mut b = c2.attach(&info.id, AttachMode::Full, None, false).await.unwrap();
    read_until(&mut b, "still-5").await;
}

/// What a session is created with (command, folder, environment) must not
/// show in the shim's command line, which other users can read with `ps`.
#[tokio::test]
async fn create_options_stay_off_the_command_line() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut opts = sh_opts("private");
    opts.env.push(("SWARM_E2E_SECRET".into(), "s3cr3t-value".into()));
    let info = c.create(opts).await.unwrap();
    let mut a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    a.input(b"echo got-$SWARM_E2E_SECRET\n".to_vec()).await.unwrap();
    read_until(&mut a, "got-s3cr3t-value").await;

    let out = Command::new("ps").args(["-ww", "-o", "args=", "-p", &shim_pid(info.pid).to_string()]).output().unwrap();
    let args = String::from_utf8_lossy(&out.stdout);
    assert!(args.contains("swarm-shim"), "{args}");
    assert!(!args.contains("s3cr3t") && !args.contains("--opts "), "options visible to ps: {args}");
}

/// Input for a program that doesn't read it is capped, not held without limit.
#[tokio::test]
async fn input_for_a_program_that_does_not_read_is_capped() {
    let env = Env::new().await;
    let c = env.client().await;
    let mut opts = sh_opts("not-reading");
    // Raw mode, like a busy full-screen app: the terminal stops taking input
    // once its buffer is full, instead of discarding it.
    opts.command = vec!["/bin/sh".into(), "-c".into(), "stty raw -echo; sleep 600".into()];
    let info = c.create(opts).await.unwrap();
    let a = c.attach(&info.id, AttachMode::Live, None, false).await.unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..(64 * 16) {
        a.input(chunk.clone()).await.unwrap();
    }
    // Let the relay deliver what it accepted.
    c.ping().await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let out = Command::new("ps").args(["-o", "rss=", "-p", &shim_pid(info.pid).to_string()]).output().unwrap();
    let rss_kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert!(rss_kb < 48 * 1024, "the shim holds {} MiB after 64 MiB of unread input", rss_kb / 1024);
    assert!(c.list().await.unwrap().iter().any(|s| s.id == info.id && s.is_running()));
}

/// After a logout without lingering, the runtime folder is gone and can't be
/// made again until the next login (here: its parent can't be written). The
/// old daemon waits; at the next login a client starts a new daemon before
/// the shims have put their sockets back. With restore-after-reboot and
/// spill-to-disk on, nothing may be restored twice and no spilled output of
/// a running session may be deleted.
#[tokio::test]
async fn a_logout_without_lingering_neither_duplicates_nor_loses_sessions() {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new().await;
    swarmd_cli(&env, &["restore", "on"]);
    swarmd_cli(&env, &["spill", "on"]);
    let info = env.client().await.create(sh_opts("kept")).await.unwrap();
    let spill = env.dir.path().join("state/spill").join(&info.id);
    assert!(spill.is_dir(), "spill dir missing at start");
    // Let the snapshot record the session.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Logged out: the folder goes, and can't be made again.
    let mut old = env.daemon.take().unwrap();
    std::fs::remove_dir_all(env.runtime()).unwrap();
    std::fs::set_permissions(env.dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(old.try_wait().unwrap().is_none(), "the daemon must wait, not exit, while logged out");

    // Logged in again; a client starts a daemon at once.
    std::fs::set_permissions(env.dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(env.runtime()).unwrap();
    env.start_daemon().await;
    let c = wait_listed(&env, &info.id, 15).await;

    // Past the new daemon's start-up grace, when it cleans up.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let listed = c.list().await.unwrap();
    assert_eq!(listed.len(), 1, "sessions were duplicated: {:?}", listed.iter().map(|s| (&s.id, &s.ui)).collect::<Vec<_>>());
    assert_eq!(listed[0].id, info.id);
    assert!(spill.is_dir(), "the running session's spilled output was deleted");

    let old_alive = old.try_wait().unwrap().is_none();
    let new_alive = env.daemon.as_mut().unwrap().try_wait().unwrap().is_none();
    let _ = old.kill();
    let _ = old.wait();
    assert!(old_alive != new_alive, "exactly one daemon must be left (old {old_alive}, new {new_alive})");
}
