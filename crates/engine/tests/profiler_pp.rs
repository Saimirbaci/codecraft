//! M15 track PP with the real tools: profiling the test at the caret (a Rust test with
//! Instruments' Time Profiler, a pytest test with cProfile), time ranges and threads on a real
//! recording, tracemalloc's periodic snapshots with the peak, and Stop during a profile's Cargo
//! build. Each test skips cleanly when its tool is missing, and everything stays in a temp dir
//! (`isolate_home`).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;

fn which(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

fn session(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&dir.join("home"));
    s
}

/// Open a project and wait for its scan (and Cargo metadata, for a Cargo project).
fn open_project(s: &mut Session, root: &Path, cargo: bool) {
    s.execute("file.open", &json!({ "path": root })).unwrap();
    let start = Instant::now();
    loop {
        s.pump();
        let ready = s.ws.project.as_ref().is_some_and(|p| !cargo || p.cargo.is_some());
        if ready {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(120), "the project did not load");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_job(s: &mut Session, job: u64, secs: u64) -> Result<(), String> {
    let start = Instant::now();
    loop {
        s.pump();
        let j = s.execute("profiler.job", &json!({ "job": job })).unwrap();
        match j["state"].as_str() {
            Some("done") if s.profiler.loading.is_none() => return Ok(()),
            Some("failed") => return Err(j["error"].as_str().unwrap_or("failed").to_string()),
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(secs) {
            return Err(format!("timed out: {j}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pump_until(s: &mut Session, secs: u64, what: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !what(s) {
        assert!(start.elapsed() < Duration::from_secs(secs), "timed out: {:?}", s.profiler.error);
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn names(list: &Value) -> Vec<String> {
    list.as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap_or("").to_string()).collect()
}

/// One Instruments recording at a time.
static XCTRACE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A library whose test burns ~2 s in `spin` early and in `churn` late.
const HOT_LIB: &str = "#[inline(never)]\npub fn spin(n: u64) -> u64 {\n    let mut x = 1u64;\n    for i in 0..n {\n        x = x.wrapping_mul(6364136223846793005).wrapping_add(i);\n    }\n    x\n}\n\n#[inline(never)]\npub fn churn(n: u64) -> u64 {\n    let mut x = 7u64;\n    for i in 0..n {\n        x ^= x.rotate_left(5).wrapping_add(i);\n    }\n    x\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn hot_test() {\n        let a = super::spin(std::hint::black_box(400_000_000));\n        let b = super::churn(std::hint::black_box(400_000_000));\n        assert_ne!(a ^ b, 1);\n    }\n\n    #[test]\n    fn other_test() {}\n}\n";

/// Profile → the Rust test at the caret: `cargo test --lib tests::hot_test -- --exact` is
/// built, then only that test is recorded with xctrace; a time range then splits `spin`
/// (early) from `churn` (late).
#[test]
fn xctrace_on_the_rust_test_at_the_caret() {
    if !(cfg!(target_os = "macos") && which("xctrace") && which("cargo")) {
        eprintln!("skipped: needs macOS with xctrace and cargo");
        return;
    }
    let _one = XCTRACE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("hotcrate");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"hotcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), HOT_LIB).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut s = session(dir.path());
    open_project(&mut s, &root, true);
    // 0-based line 21 is `fn hot_test()`.
    let lib = root.join("src/lib.rs");
    let r = s.execute("profiler.atCaret", &json!({ "path": lib, "line": 21, "seconds": 60 })).unwrap();
    assert_eq!(r["config"], "Test hot_test", "{r}");
    assert_eq!(r["state"], "building");
    let job = r["job"].as_u64().unwrap();
    if let Err(e) = wait_job(&mut s, job, 400) {
        eprintln!("skipped: could not build or record the test here: {e}");
        s.shutdown();
        return;
    }
    // Only the one test ran: the recording's command is the test executable with the filter.
    let tab = s.profiler.jobs.iter().find(|j| j.id == job).and_then(|j| j.tab).unwrap();
    let cmd = s.ws.run(tab).unwrap().command_line.clone();
    assert!(cmd.contains("tests::hot_test --exact"), "{cmd}");
    assert!(!cmd.contains("cargo"), "{cmd}");
    let rep = s.execute("profiler.report", &json!({ "snapshot": job, "top": 10 })).unwrap();
    let top = names(&rep["top"]);
    assert!(top.iter().any(|n| n.contains("spin")) && top.iter().any(|n| n.contains("churn")), "{rep}");
    assert_eq!(rep["sort"], "self");
    // A time range: the first part of the run is `spin`, the last part `churn`.
    let duration = rep["durationMs"].as_f64().unwrap_or(0.0);
    assert!(duration > 500.0, "{rep}");
    // Total shares (a debug build spends its self time in core's non-inlined helpers).
    let share = |r: &Value, f: &str| -> f64 {
        r["topByTotal"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["name"].as_str().unwrap_or("").contains(f))
            .map(|x| x["totalPercent"].as_f64().unwrap())
            .fold(0.0, f64::max)
    };
    let early = s
        .execute("profiler.report", &json!({ "snapshot": job, "range": { "startMs": 0, "endMs": duration * 0.3 } }))
        .unwrap();
    let late = s
        .execute(
            "profiler.report",
            &json!({ "snapshot": job, "range": { "startMs": duration * 0.75, "endMs": duration } }),
        )
        .unwrap();
    assert!(share(&early, "spin") > share(&early, "churn"), "{early}");
    assert!(share(&late, "churn") > share(&late, "spin"), "{late}");
    assert!(early["selection"]["range"]["endMs"].as_f64().is_some(), "{early}");
    // The same range in the tool window: every view and the gutter follow.
    let path = s.profiler.job(job).unwrap().snapshot.clone();
    s.execute("run.openProfilerSnapshot", &json!({ "path": path })).unwrap();
    pump_until(&mut s, 60, |s| s.profiler.snapshot.is_some() && s.profiler.loading.is_none());
    s.execute("profiler.selectRange", &json!({ "startMs": duration * 0.75, "endMs": duration })).unwrap();
    pump_until(&mut s, 60, |s| s.profiler.loading.is_none());
    let snap = s.profiler.snapshot.as_ref().unwrap();
    assert!(snap.analysis.range.is_some());
    // The method list has only what ran then (a debug build's self time is in core helpers).
    let methods: Vec<&str> =
        snap.analysis.methods.iter().map(|m| snap.profile.frames[m.frame as usize].name.as_str()).collect();
    assert!(methods.iter().any(|n| n.contains("churn")), "{methods:?}");
    assert!(!methods.iter().any(|n| n.contains("spin")), "{methods:?}");
    // lib.rs as the debug info names it (a test run with CARGO_TARGET_DIR set may reuse a
    // binary built from another temp dir).
    let file = snap
        .profile
        .frames
        .iter()
        .find(|f| f.name.ends_with("churn"))
        .and_then(|f| f.file.clone())
        .unwrap_or_else(|| lib.display().to_string());
    assert!(file.ends_with("src/lib.rs"), "{file}");
    let hits = snap.lines.for_path(Path::new(&file)).unwrap_or(&[]);
    assert!(hits.iter().any(|h| (12..=16).contains(&h.line)), "churn's lines in the gutter: {hits:?}");
    assert!(
        !hits.iter().any(|h| (3..=7).contains(&h.line) && h.self_weight > 0),
        "spin's lines are outside the range: {hits:?}"
    );
    s.shutdown();
}

const PYTEST_FILE: &str = "def slow_sum(n):\n    total = 0\n    for i in range(n):\n        total += i * i\n    return total\n\n\ndef test_fast():\n    assert slow_sum(10) == 285\n\n\ndef test_slow():\n    assert slow_sum(3_000_000) > 0\n";

/// Profile → the pytest test at the caret: `python -m cProfile … -m pytest file::test_slow`.
#[test]
fn cprofile_on_the_pytest_test_at_the_caret() {
    let pytest = std::process::Command::new("python3").args(["-m", "pytest", "--version"]).output();
    if !pytest.is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: needs python3 with pytest");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("test_calc.py");
    std::fs::write(&file, PYTEST_FILE).unwrap();
    let mut s = session(dir.path());
    open_project(&mut s, &root, false);
    // Agents give a 1-based line (13 = the body of test_slow).
    let r = s.execute("profiler.record", &json!({ "test": { "path": file, "line": 13 } })).unwrap();
    assert_eq!(r["tool"], "cProfile", "{r}");
    assert!(r["command"].as_str().unwrap().contains("test_calc.py::test_slow"), "{r}");
    let job = r["job"].as_u64().unwrap();
    wait_job(&mut s, job, 180).unwrap();
    let rep = s.execute("profiler.report", &json!({ "snapshot": job, "top": 5 })).unwrap();
    assert_eq!(rep["top"][0]["name"], "slow_sum", "{rep}");
    assert!(rep["top"][0]["location"].as_str().unwrap().ends_with("test_calc.py:1"), "{rep}");
    // The fast test did not run.
    let all = s.execute("profiler.report", &json!({ "snapshot": job, "top": 200 })).unwrap();
    assert!(!names(&all["top"]).contains(&"test_fast".to_string()), "{all}");
    s.shutdown();
}

const GROW: &str = "import time\n\n\ndef build(n):\n    return [bytes(1000) for _ in range(n)]\n\n\ndef main():\n    keep = []\n    for _ in range(4):\n        keep.append(build(1000))\n        time.sleep(0.05)\n    spike = build(15000)\n    time.sleep(0.2)\n    del spike\n    time.sleep(0.15)\n    return keep\n\n\nKEEP = main()\n";

/// tracemalloc with periodic snapshots: the memory over time, the peak (the 15 MB spike that is
/// freed before the end) and the allocation sites at the peak.
#[test]
fn tracemalloc_periodic_snapshots_and_the_peak() {
    if !which("python3") {
        eprintln!("skipped: no python3");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("grow.py"), GROW).unwrap();
    let mut s = session(&root);
    let r = s
        .execute(
            "profiler.record",
            &json!({ "command": "python3 grow.py", "cwd": root, "kind": "memory", "interval": 0.02 }),
        )
        .unwrap();
    assert_eq!(r["tool"], "tracemalloc");
    let job = r["job"].as_u64().unwrap();
    wait_job(&mut s, job, 120).unwrap();
    let mem = s.execute("profiler.memory", &json!({ "snapshot": job, "points": 50, "top": 5 })).unwrap();
    let peak = mem["peak"].as_u64().unwrap();
    assert!(peak > 15_000_000, "{mem}");
    assert!(mem["timeline"].as_array().unwrap().len() >= 5, "{mem}");
    assert_eq!(mem["intervalMs"], 20);
    let first = &mem["atPeak"][0];
    assert_eq!(first["name"], "build", "{mem}");
    assert!(first["location"].as_str().unwrap().ends_with("grow.py:5"), "{first}");
    assert!(first["size"].as_u64().unwrap() > 15_000_000, "{first}");
    // Live at the end is much less than the peak (the spike was freed).
    let rep = s.execute("profiler.report", &json!({ "snapshot": job })).unwrap();
    assert!(rep["total"].as_u64().unwrap() < peak / 2, "{rep}");
    assert!(rep["memory"]["peakSnapshot"].is_string(), "{rep}");
    // The whole snapshot at the peak opens as a profile of its own.
    let path = s.profiler.job(job).unwrap().snapshot.clone();
    s.execute("run.openProfilerSnapshot", &json!({ "path": path })).unwrap();
    pump_until(&mut s, 60, |s| s.profiler.snapshot.is_some() && s.profiler.loading.is_none());
    s.execute("profiler.selectTab", &json!({ "tab": "memory" })).unwrap();
    let r = s.execute("profiler.openPeakSnapshot", &Value::Null).unwrap();
    assert!(r["path"].as_str().unwrap().ends_with(".peak.json"), "{r}");
    pump_until(&mut s, 60, |s| {
        s.profiler.loading.is_none()
            && s.profiler.snapshot.as_ref().is_some_and(|x| x.path.to_string_lossy().ends_with(".peak.json"))
    });
    let at_peak = s.execute("profiler.report", &json!({ "top": 3 })).unwrap();
    assert_eq!(at_peak["top"][0]["name"], "build", "{at_peak}");
    assert!(at_peak["total"].as_u64().unwrap() > 15_000_000, "{at_peak}");
    // interval 0: one snapshot at the end, no timeline.
    let r = s
        .execute(
            "profiler.record",
            &json!({ "command": "python3 grow.py", "cwd": root, "kind": "memory", "interval": 0 }),
        )
        .unwrap();
    let job = r["job"].as_u64().unwrap();
    wait_job(&mut s, job, 120).unwrap();
    assert!(s.execute("profiler.memory", &json!({ "snapshot": job })).is_err());
    s.shutdown();
}

/// Stop in the Run window during a profile's Cargo build stops the build (it runs outside the
/// Run window's processes); the job fails as stopped and nothing is recorded.
#[test]
fn stop_stops_a_profiles_cargo_build() {
    if !which("cargo") {
        eprintln!("skipped: no cargo");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("slowbuild");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"slowbuild\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[workspace]\n",
    )
    .unwrap();
    // The build script takes a minute.
    std::fs::write(root.join("build.rs"), "fn main() { std::thread::sleep(std::time::Duration::from_secs(60)); }\n")
        .unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut s = session(dir.path());
    open_project(&mut s, &root, false);
    // A profiler that is never reached (the build is stopped first).
    s.profiler.tool_dirs = Some(vec![dir.path().join("bin")]);
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    std::fs::write(dir.path().join("bin/samply"), "#!/bin/sh\nexit 1\n").unwrap();
    s.profiler.xctrace_allowed = Some(false);
    let r = s.execute("profiler.record", &json!({ "command": "cargo run", "cwd": root })).unwrap();
    assert_eq!(r["state"], "building");
    let job = r["job"].as_u64().unwrap();
    let tab = r["tab"].as_u64().unwrap();
    // Let cargo get going, then Stop as the Run window does.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1500) {
        s.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(s.execute("profiler.job", &json!({ "job": job })).unwrap()["state"], "building");
    s.ws.active_run = Some(codecraft_workspace::RunTabId(tab));
    s.execute("run.stop", &Value::Null).unwrap();
    assert_ne!(s.status, "Nothing is running");
    pump_until(&mut s, 30, |s| s.profiler.job(job).is_some_and(|j| j.error.is_some()));
    let j = s.execute("profiler.job", &json!({ "job": job })).unwrap();
    assert_eq!(j["state"], "failed", "{j}");
    assert!(j["error"].as_str().unwrap().contains("stopped"), "{j}");
    assert!(s.profiler.recordings.is_empty());
    assert!(!s.ws.run(codecraft_workspace::RunTabId(tab)).unwrap().is_running());
    // Stopped well before the build script's minute.
    assert!(started.elapsed() < Duration::from_secs(30));
    s.shutdown();
}
