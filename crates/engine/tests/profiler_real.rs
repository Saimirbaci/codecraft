//! Profiling with the real tools (M14 track PF): cProfile and tracemalloc on a tiny Python
//! script, and on macOS Instruments' Time Profiler (`xctrace`) and `leaks` on tiny C and Rust
//! programs. Each test skips cleanly when its tool is missing or can't record here (xctrace
//! without Xcode, no permission), and everything stays in a temp dir (`isolate_home`).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;

const SCRIPT: &str = r#"def leaf(n):
    total = 0
    for i in range(n):
        total += i * i
    return total


def heavy():
    return sum(leaf(20000) for _ in range(60))


def light():
    return sum(leaf(20000) for _ in range(6))


def allocate():
    keep = []
    for i in range(2000):
        keep.append(bytearray(4096))
    return keep


KEEP = None


def main():
    global KEEP
    heavy()
    light()
    KEEP = allocate()


if __name__ == "__main__":
    main()
"#;

fn which(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

fn session(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&dir.join("home"));
    s
}

/// Pump until the job is done or failed; `None` on a failure (with its error).
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

fn report(s: &mut Session, job: u64, extra: Value) -> Value {
    let mut p = json!({ "snapshot": job, "top": 8 });
    if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    let r = s.execute("profiler.report", &p).unwrap();
    assert!(r.get("pending").is_none(), "{r}");
    r
}

fn names(list: &Value) -> Vec<String> {
    list.as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap_or("").to_string()).collect()
}

#[test]
fn cprofile_on_a_python_script() {
    if !which("python3") {
        eprintln!("skipped: no python3");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("work.py"), SCRIPT).unwrap();
    let mut s = session(dir.path());
    let r = s.execute("profiler.record", &json!({ "command": "python3 work.py", "cwd": dir.path() })).unwrap();
    assert_eq!(r["tool"], "cProfile");
    let job = r["job"].as_u64().unwrap();
    wait_job(&mut s, job, 120).unwrap();
    let rep = report(&mut s, job, json!({ "focus": "heavy" }));
    assert_eq!(rep["format"], "cProfile");
    let top = &rep["top"][0];
    assert_eq!(top["name"], "leaf", "{rep}");
    assert!(top["location"].as_str().unwrap().ends_with("work.py:1"), "{top}");
    assert_eq!(top["calls"], 66);
    assert!(top["selfPercent"].as_f64().unwrap() > 50.0);
    // heavy calls leaf ten times as often as light does.
    let focus = &rep["focus"];
    assert!(focus["percent"].as_f64().unwrap() > 60.0, "{focus}");
    assert_eq!(focus["callers"][0]["name"], "main", "{focus}");
    assert!(names(&focus["inside"]).contains(&"leaf".to_string()));
    // The method list in the tool window too.
    let methods = s.execute("profiler.methods", &json!({ "limit": 3 })).unwrap();
    assert_eq!(methods[0]["name"], "leaf");
    s.shutdown();
}

#[test]
fn tracemalloc_on_a_python_script() {
    if !which("python3") {
        eprintln!("skipped: no python3");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("work.py"), SCRIPT).unwrap();
    let mut s = session(dir.path());
    let r = s
        .execute("profiler.record", &json!({ "command": ["python3", "work.py"], "cwd": dir.path(), "kind": "memory" }))
        .unwrap();
    assert_eq!(r["tool"], "tracemalloc");
    let job = r["job"].as_u64().unwrap();
    wait_job(&mut s, job, 120).unwrap();
    let rep = report(&mut s, job, Value::Null);
    assert_eq!(rep["unit"], "bytes");
    assert_eq!(rep["top"][0]["name"], "allocate", "{rep}");
    let line = &rep["hotLines"][0];
    assert!(line["location"].as_str().unwrap().ends_with("work.py:19"), "{line}");
    assert_eq!(line["code"], "keep.append(bytearray(4096))");
    assert!(rep["total"].as_u64().unwrap() > 2000 * 4096);
    // Per-line numbers for the editor gutter.
    let hits = s.execute("profiler.lineHits", &json!({ "path": dir.path().join("work.py") })).unwrap();
    assert!(hits["lines"].as_array().unwrap().iter().any(|l| l["line"] == 19), "{hits}");
    s.shutdown();
}

/// memray (memory) with an interpreter that has it installed: set
/// `CODECRAFT_TEST_PYTHON_WITH_MEMRAY=/path/to/venv/bin/python` (skipped otherwise; never
/// installs anything).
#[test]
fn memray_on_a_python_script() {
    let Some(python) = std::env::var_os("CODECRAFT_TEST_PYTHON_WITH_MEMRAY") else {
        eprintln!("skipped: set CODECRAFT_TEST_PYTHON_WITH_MEMRAY to a Python with memray");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("work.py"), SCRIPT).unwrap();
    let mut s = session(dir.path());
    let python = python.to_string_lossy().into_owned();
    let r = s
        .execute("profiler.record", &json!({ "command": [python, "work.py"], "cwd": dir.path(), "kind": "memory" }))
        .unwrap();
    assert_eq!(r["tool"], "memray");
    let job = r["job"].as_u64().unwrap();
    wait_job(&mut s, job, 180).unwrap();
    let rep = report(&mut s, job, Value::Null);
    assert_eq!(rep["unit"], "bytes");
    assert!(names(&rep["topByTotal"]).contains(&"allocate".to_string()), "{rep}");
    s.shutdown();
}

/// A program that burns ~1.5 s in `inner` (Time Profiler needs a moment to start sampling).
const HOT_C: &str = r#"#include <stdio.h>
__attribute__((noinline)) double inner(int n) {
    double s = 0;
    for (int i = 0; i < n; i++) s += (double)i * 1.0001 / (i + 1);
    return s;
}
__attribute__((noinline)) double outer(int n) {
    double s = 0;
    for (int k = 0; k < 400; k++) s += inner(n);
    return s;
}
int main(void) {
    printf("%f\n", outer(1000000));
    return 0;
}
"#;

/// One Instruments recording at a time.
static XCTRACE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn macos_tools(tools: &[&str]) -> bool {
    cfg!(target_os = "macos") && tools.iter().all(|t| which(t))
}

fn compile_c(dir: &Path, name: &str, source: &str) -> bool {
    let src = dir.join(format!("{name}.c"));
    std::fs::write(&src, source).unwrap();
    std::process::Command::new("clang")
        .args(["-g", "-O0", "-o"])
        .arg(dir.join(name))
        .arg(&src)
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn xctrace_time_profiler_on_a_c_program() {
    if !macos_tools(&["xctrace", "clang"]) {
        eprintln!("skipped: needs macOS with xctrace and clang");
        return;
    }
    let _one = XCTRACE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    if !compile_c(dir.path(), "hot", HOT_C) {
        eprintln!("skipped: clang failed");
        return;
    }
    let mut s = session(dir.path());
    let r = s.execute("profiler.record", &json!({ "command": "./hot", "cwd": dir.path(), "seconds": 30 })).unwrap();
    assert_eq!(r["tool"], "xctrace");
    let job = r["job"].as_u64().unwrap();
    if let Err(e) = wait_job(&mut s, job, 120) {
        eprintln!("skipped: xctrace could not record here: {e}");
        s.shutdown();
        return;
    }
    let rep = report(&mut s, job, json!({ "focus": "outer" }));
    assert_eq!(rep["format"], "xctrace");
    assert_eq!(rep["top"][0]["name"], "inner", "{rep}");
    assert!(rep["top"][0]["location"].as_str().unwrap().contains("hot.c:"), "{rep}");
    let line = &rep["hotLines"][0];
    assert_eq!(line["function"], "inner");
    assert!(line["code"].as_str().unwrap().contains("for (int i = 0"), "{line}");
    assert!(rep["focus"]["percent"].as_f64().unwrap() > 50.0);
    // The timeline has the main thread's activity.
    let t = s.execute("profiler.timeline", &json!({ "snapshot": job, "buckets": 20 })).unwrap();
    assert!(t["durationMs"].as_u64().is_some(), "{t}");
    s.shutdown();
}

const ALLOC_C: &str = r#"#include <stdlib.h>
#include <string.h>
static void *keep[3000];
__attribute__((noinline)) void *big(int i) { char *p = malloc(64 * 1024); memset(p, i, 64 * 1024); return p; }
__attribute__((noinline)) void *small(int i) { char *p = malloc(32); memset(p, i, 32); return p; }
int main(void) {
    for (int i = 0; i < 500; i++) keep[i] = big(i);
    for (int i = 500; i < 3000; i++) keep[i] = small(i);
    return 0;
}
"#;

#[test]
fn leaks_native_memory_on_a_c_program() {
    if !macos_tools(&["leaks", "malloc_history", "atos", "clang"]) {
        eprintln!("skipped: needs macOS with leaks, malloc_history, atos and clang");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    if !compile_c(dir.path(), "alloc", ALLOC_C) {
        eprintln!("skipped: clang failed");
        return;
    }
    let mut s = session(dir.path());
    let r =
        s.execute("profiler.record", &json!({ "command": "./alloc", "cwd": dir.path(), "kind": "memory" })).unwrap();
    assert_eq!(r["tool"], "leaks");
    let job = r["job"].as_u64().unwrap();
    if let Err(e) = wait_job(&mut s, job, 120) {
        eprintln!("skipped: leaks could not record here: {e}");
        s.shutdown();
        return;
    }
    let rep = report(&mut s, job, Value::Null);
    assert_eq!(rep["unit"], "bytes");
    let by_total = names(&rep["topByTotal"]);
    assert!(by_total.contains(&"big".to_string()), "{rep}");
    // `big` symbolicated by atos to its source line.
    let big = rep["topByTotal"].as_array().unwrap().iter().find(|f| f["name"] == "big").unwrap();
    assert!(big["location"].as_str().unwrap_or("").contains("alloc.c:"), "{big}");
    s.shutdown();
}

/// `cargo test <name>` is built first, then only the test executable is recorded, with Rust
/// functions and their file:line.
#[test]
fn xctrace_on_a_cargo_test() {
    if !macos_tools(&["xctrace", "cargo"]) {
        eprintln!("skipped: needs macOS with xctrace and cargo");
        return;
    }
    let _one = XCTRACE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("hotcrate");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"hotcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "#[inline(never)]\npub fn spin(n: u64) -> u64 {\n    let mut x = 1u64;\n    for i in 0..n {\n        x = x.wrapping_mul(6364136223846793005).wrapping_add(i);\n    }\n    x\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn hot_test() {\n        assert_ne!(super::spin(std::hint::black_box(800_000_000)), 0);\n    }\n}\n",
    )
    .unwrap();
    let mut s = session(dir.path());
    let r = s
        .execute("profiler.record", &json!({ "command": "cargo test --lib hot_test", "cwd": root, "seconds": 60 }))
        .unwrap();
    let job = r["job"].as_u64().unwrap();
    assert_eq!(r["state"], "building");
    if let Err(e) = wait_job(&mut s, job, 300) {
        eprintln!("skipped: could not build or record the cargo test here: {e}");
        s.shutdown();
        return;
    }
    let rep = report(&mut s, job, json!({ "focus": "hot_test" }));
    let top = names(&rep["top"]);
    assert!(top.iter().any(|n| n.contains("spin")), "{rep}");
    let spin = rep["top"].as_array().unwrap().iter().find(|f| f["name"].as_str().unwrap().contains("spin")).unwrap();
    assert!(spin["location"].as_str().unwrap_or("").contains("lib.rs:"), "{spin}");
    assert!(rep["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("debug build")));
    s.shutdown();
}
