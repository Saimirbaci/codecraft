//! Rust values under lldb-dap read like RustRover's: `&str`, `String` and `Vec` show their
//! contents, not raw pointers (skips when lldb-dap or cargo is missing).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::debug::console_text;
use codecraft_engine::{Session, dap};

const WAIT: Duration = Duration::from_secs(120);

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            let console: Vec<&str> = s.debug.console.iter().map(console_text).collect();
            panic!("timed out waiting for {what}; console:\n{}", console.join("\n"));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn cargo() -> Option<PathBuf> {
    dap::discover::which("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

fn local(s: &Session, name: &str) -> String {
    let session = s.debug.session.as_ref().unwrap();
    let scope = session.scopes.first().unwrap();
    session.variables[&scope.reference].iter().find(|v| v.name == name).map(|v| v.value.clone()).unwrap_or_default()
}

#[test]
fn rust_strings_and_vecs_show_their_contents() {
    if dap::discover::find_lldb_dap().is_err() || cargo().is_none() {
        eprintln!("skipping: lldb-dap or cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ccvalues\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(
        &main,
        "fn main() {\n\
         \x20   let greeting: &str = \"hello\";\n\
         \x20   let owned = String::from(\"world\");\n\
         \x20   let numbers = vec![1, 2, 3];\n\
         \x20   println!(\"{greeting} {owned} {numbers:?}\");\n\
         }\n",
    )
    .unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    s.execute("file.open", &json!({ "path": main })).unwrap();
    s.execute("run.toggleBreakpoint.lineBreakpoint", &json!({ "path": main, "line": 4 })).unwrap();
    s.execute("run.debug", &Value::Null).unwrap();
    pump_until(&mut s, "the breakpoint", |s| {
        s.debug.session.as_ref().is_some_and(|x| x.is_stopped() && !x.scopes.is_empty())
            && !s.debug.settling()
            && s.debug.session.as_ref().is_some_and(|x| x.scopes.first().is_some_and(|sc| x.variables.contains_key(&sc.reference)))
    });
    let greeting = local(&s, "greeting");
    let owned = local(&s, "owned");
    let numbers = local(&s, "numbers");
    eprintln!("greeting={greeting:?} owned={owned:?} numbers={numbers:?}");
    assert_eq!(greeting, "\"hello\"");
    assert_eq!(owned, "\"world\"");
    assert!(!numbers.starts_with("0x"), "{numbers}");
    assert!(numbers.contains('3'), "{numbers}");
    s.execute("debug.stop", &Value::Null).unwrap();
    s.shutdown();
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn attach_to_a_running_process_and_detach() {
    let cc = dap::discover::which("cc");
    if dap::discover::find_lldb_dap().is_err() || cc.is_none() {
        eprintln!("skipping: lldb-dap or cc is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let src = dir.join("spin.c");
    std::fs::write(
        &src,
        "#include <unistd.h>\nint main(void) {\n    volatile int ticks = 0;\n    for (;;) {\n        ticks++;\n        usleep(10000);\n    }\n}\n",
    )
    .unwrap();
    let exe = dir.join("spin");
    let built = std::process::Command::new(cc.unwrap()).args(["-g", "-O0", "-o"]).arg(&exe).arg(&src).status().unwrap();
    assert!(built.success());
    let mut child = std::process::Command::new(&exe).spawn().unwrap();
    let pid = child.id();

    let mut s = Session::new();
    s.lsp_enabled = false;
    // The process list finds it.
    s.execute("run.attachToProcess", &Value::Null).unwrap();
    pump_until(&mut s, "the process list", |s| s.debug.attach.as_ref().is_some_and(|a| !a.loading));
    let list = s.execute("debug.attach.filter", &json!({ "query": pid.to_string() })).unwrap();
    assert!(list["processes"].as_array().unwrap().iter().any(|p| p["pid"] == json!(pid)), "{list}");
    let index = list["processes"].as_array().unwrap().iter().position(|p| p["pid"] == json!(pid)).unwrap();
    s.execute("debug.attach.confirm", &json!({ "index": index })).unwrap();
    assert!(s.debug.attach.is_none());

    let start = Instant::now();
    loop {
        s.pump();
        let status = s.debug.session.as_ref().map(|x| x.status());
        if let Some(codecraft_engine::debug::DebugStatus::Failed { message }) = &status {
            // macOS refuses to attach without developer-tools permission; nothing to test then.
            eprintln!("skipping: attach failed: {message}");
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        if s.debug.is_running() || s.debug.is_stopped() {
            break;
        }
        assert!(start.elapsed() < WAIT, "attach timed out: {status:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    if s.debug.is_running() {
        s.execute("run.debuggingActions.pauseAllPrograms", &Value::Null).unwrap();
    }
    pump_until(&mut s, "a stop with frames", |s| {
        s.debug.is_stopped() && !s.debug.settling() && s.debug.session.as_ref().is_some_and(|x| !x.frames.is_empty())
    });
    assert_eq!(s.debug.session.as_ref().unwrap().debuggee_pid, None, "an attached process is not ours to kill");
    // Stop detaches: the process keeps running.
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "detaching killed the process");
    let _ = child.kill();
    let _ = child.wait();
}
