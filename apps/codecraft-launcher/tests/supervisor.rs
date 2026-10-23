#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic)]
use codecraft_selfupdate::{Status, Store};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
fn script(dir: &Path, body: &str) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("codecraft");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
const HEALTHY: &str = "sleep 1; printf 'healthy\\n' > \"$CODECRAFT_HEALTH_FILE\"; sleep 1; exit 0";
struct Fixture {
    _dir: tempfile::TempDir,
    s: Store,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let factory = dir.path().join("factory space");
        script(&factory, HEALTHY);
        let s = Store::new(dir.path().join("home space"), factory);
        Self { _dir: dir, s }
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_codecraft-launcher"));
        c.env("CODECRAFT_HOME", &self.s.base)
            .env("CODECRAFT_FACTORY_DIR", &self.s.factory)
            .env("CODECRAFT_BOOT_TIMEOUT", "3")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
    fn stage(&self, body: &str) -> String {
        let p = self._dir.path().join("source");
        script(&p, body);
        let id = self.s.stage(&p, "0.3.0", "abcdef123456", "fake").unwrap();
        self.s.promote(&id).unwrap();
        id
    }
    fn run(&self) -> Output {
        bounded(self.cmd().spawn().unwrap())
    }
}
fn bounded(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("launcher exceeded test deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn promote_health_noise_and_clean_exit() {
    let f = Fixture::new();
    let id = f.stage(&format!("printf '\\377 noise\\n'; {HEALTHY}"));
    assert!(f.run().status.success());
    assert_eq!(f.s.meta(&id).unwrap().status, Status::Good);
    assert_eq!(f.s.state().last_good.as_deref(), Some(id.as_str()));
    assert!(!f.s.state().pending_health);
    assert!(f.s.log_tail().contains("noise"));
}
#[test]
fn crash_rolls_back_restores_notice_and_second_failure_blacklists() {
    let f = Fixture::new();
    let good = f.stage(HEALTHY);
    assert!(f.run().status.success());
    let bad = f.stage("printf corrupt > \"$CODECRAFT_HOME/settings.json\"; exit 1");
    fs::write(f.s.base.join("settings.json"), "old settings").unwrap();
    let out = f.run();
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(f.s.state().last_good.as_deref(), Some(good.as_str()));
    assert_eq!(fs::read_to_string(f.s.base.join("settings.json")).unwrap(), "old settings");
    assert!(f.s.take_notice().unwrap().unwrap().contains("before health"));
    let out = bounded(f.cmd().args(["--version-id", &bad]).spawn().unwrap());
    assert!(out.status.success());
    assert_eq!(f.s.meta(&bad).unwrap().status, Status::Bad);
    assert_ne!(codecraft_selfupdate::choose(&f.s.boot_state(), codecraft_selfupdate::now()).id, bad);
}
#[test]
fn timeout_and_late_crash_fall_back() {
    for body in [
        "exec sleep 60",
        "mkfifo \"$CODECRAFT_HEALTH_FILE\"; exec sleep 60",
        "sleep 1; printf 'healthy\\n' > \"$CODECRAFT_HEALTH_FILE\"; sleep 1; exit 1",
    ] {
        let f = Fixture::new();
        let id = f.stage(body);
        let out = f.run();
        assert!(out.status.success(), "{:?}", out);
        assert!(f.s.meta(&id).unwrap().reason.is_some());
        assert!(f.s.take_notice().unwrap().is_some());
    }
}
#[test]
fn corrupt_pointers_and_metadata_factory_escape() {
    let f = Fixture::new();
    let id = f.stage("exit 1");
    fs::write(f.s.version_dir(&id).unwrap().join("meta.json"), "{").unwrap();
    for bytes in ["", "{", "garbage"] {
        fs::write(f.s.base.join("state.json"), bytes).unwrap();
        assert!(f.run().status.success());
    }
    f.s.save_state(&codecraft_selfupdate::State {
        pending_health: true,
        boot: Some(codecraft_selfupdate::Boot { id: "../../invalid".into(), started_at: 0, attempts: 99 }),
        ..Default::default()
    })
    .unwrap();
    assert!(bounded(f.cmd().arg("--factory").spawn().unwrap()).status.success());
    let output = f.cmd().arg("--status").output().unwrap();
    assert!(output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["choice"]["id"], "factory");
}
#[test]
fn concurrent_launchers_and_factory_only_install() {
    let f = Fixture::new();
    let first = f.cmd().spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !f.s.base.join("launcher.lock").exists() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(50));
    let second = f.run();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("busy"));
    assert!(bounded(first).status.success());
    assert!(f.run().status.success());
}

#[test]
fn non_utf8_home_and_factory_paths() {
    use std::os::unix::ffi::OsStringExt;
    let mut f = Fixture::new();
    f.s.base = f._dir.path().join(std::ffi::OsString::from_vec(b"home space-\xff".to_vec()));
    f.s.factory = f._dir.path().join(std::ffi::OsString::from_vec(b"factory space-\xff".to_vec()));
    if let Err(e) = fs::create_dir_all(&f.s.factory) {
        // EILSEQ: APFS (92 on macOS) and some Linux filesystems (84) refuse non-UTF-8 names.
        if e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(84 | 92)) {
            eprintln!("non-UTF8 filesystem fixture unavailable on this filesystem or sandbox: {e}");
            return;
        }
        panic!("non-UTF8 fixture: {e}");
    }
    script(&f.s.factory, HEALTHY);
    assert!(f.run().status.success());
    assert!(f.cmd().arg("--status").output().unwrap().status.success());
}

#[test]
fn forced_factory_does_not_read_or_rewrite_pointer_files() {
    let f = Fixture::new();
    fs::create_dir_all(&f.s.base).unwrap();
    let state = f.s.base.join("state.json");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("sentinel"), "untouched").unwrap();
    assert!(bounded(f.cmd().arg("--factory").spawn().unwrap()).status.success());
    assert_eq!(fs::read_to_string(state.join("sentinel")).unwrap(), "untouched");
}

#[test]
fn wait_pid_rejects_invalid_and_waits_for_a_real_local_child() {
    let f = Fixture::new();
    for pid in ["0", "4294967295"] {
        let invalid = bounded(f.cmd().args(["--wait-pid", pid]).spawn().unwrap());
        assert!(!invalid.status.success());
    }
    let mut old = Command::new("sleep").arg("1").spawn().unwrap();
    let pid = old.id();
    let reap = std::thread::spawn(move || old.wait());
    let start = Instant::now();
    let result = bounded(f.cmd().args(["--factory", "--wait-pid", &pid.to_string()]).spawn().unwrap());
    let _ = reap.join();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(start.elapsed() >= Duration::from_secs(2));
}
