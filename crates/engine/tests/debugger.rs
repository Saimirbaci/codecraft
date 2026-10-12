//! The debugger through engine commands: breakpoints (persistence, following edits) and real
//! sessions with lldb-dap (Rust) and debugpy (Python). Real sessions skip when the tool is missing.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::debug::{DebugConsoleKind, DebugStatus, DebugTab, console_text};
use codecraft_engine::{Session, dap};

const WAIT: Duration = Duration::from_secs(120);

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            let console: Vec<&str> = s.debug.console.iter().map(console_text).collect();
            panic!(
                "timed out waiting for {what}; status {:?}; console:\n{}",
                s.debug.session.as_ref().map(|x| x.status()),
                console.join("\n")
            );
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn open_project(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    s
}

fn status(s: &Session) -> Option<DebugStatus> {
    s.debug.session.as_ref().map(|x| x.status())
}

fn stopped_line(s: &Session) -> Option<usize> {
    let session = s.debug.session.as_ref()?;
    if !session.is_stopped() || s.debug.settling() {
        return None;
    }
    session.selected_frame().map(|f| f.line)
}

fn local(s: &Session, name: &str) -> Option<String> {
    let session = s.debug.session.as_ref()?;
    let scope = session.scopes.first()?;
    session.variables.get(&scope.reference)?.iter().find(|v| v.name == name).map(|v| v.value.clone())
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---------------------------------------------------------------- breakpoints

#[test]
fn breakpoints_toggle_persist_and_follow_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.py");
    std::fs::write(&file, "a = 1\nb = 2\nc = 3\n").unwrap();
    let mut s = open_project(tmp.path());
    s.execute("file.open", &json!({ "path": file })).unwrap();
    let file = codecraft_engine::normalize_path(&file);

    let r = s.execute("run.toggleBreakpoint.lineBreakpoint", &json!({ "path": file, "line": 1 })).unwrap();
    assert_eq!(r["breakpoint"], true);
    assert!(s.debug.breakpoint_at(&file, 1).is_some());
    // Lines that don't exist are refused.
    assert!(s.execute("run.toggleBreakpoint.lineBreakpoint", &json!({ "path": file, "line": 50 })).is_err());
    // Properties.
    s.execute(
        "debug.setBreakpoint",
        &json!({ "path": file, "line": 1, "condition": "b > 1", "hitCondition": "2", "enabled": false }),
    )
    .unwrap();
    let bp = s.debug.breakpoint_at(&file, 1).unwrap().clone();
    assert_eq!((bp.condition.as_deref(), bp.hit_condition.as_deref(), bp.enabled), (Some("b > 1"), Some("2"), false));
    s.execute("run.toggleBreakpoint.toggleBreakpointEnabled", &json!({ "path": file, "line": 1 })).unwrap();
    assert!(s.debug.breakpoint_at(&file, 1).unwrap().enabled);
    s.execute("debug.addWatch", &json!({ "expression": "a + b" })).unwrap();

    // Saved in the project.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".codecraft/breakpoints.json")).unwrap())
            .unwrap();
    assert_eq!(saved["breakpoints"][0]["path"], "main.py");
    assert_eq!(saved["breakpoints"][0]["line"], 2);
    assert_eq!(saved["breakpoints"][0]["condition"], "b > 1");
    assert_eq!(saved["watches"], json!(["a + b"]));

    // Typing a line above moves the breakpoint with its line.
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
    s.execute("editor.type", &json!({ "text": "import os\n" })).unwrap();
    s.pump();
    assert!(s.debug.breakpoint_at(&file, 2).is_some(), "{:?}", s.debug.breakpoints);
    assert!(s.debug.breakpoint_at(&file, 1).is_none());

    // A fresh session (the IDE restarted) loads them back.
    let mut again = open_project(tmp.path());
    again.pump();
    assert_eq!(again.debug.breakpoints.len(), 1);
    assert_eq!(again.debug.breakpoints[0].line, 2);
    assert_eq!(again.debug.watches.len(), 1);

    // Remove by location, then all.
    let r = s.execute("debug.removeBreakpoint", &json!({ "path": file, "line": 2 })).unwrap();
    assert_eq!(r["removed"], true);
    s.execute("run.toggleBreakpoint.temporaryLineBreakpoint", &json!({ "path": file, "line": 0 })).unwrap();
    assert!(s.debug.breakpoint_at(&file, 0).unwrap().temporary);
    assert_eq!(s.execute("debug.removeAllBreakpoints", &Value::Null).unwrap()["removed"], 1);
    assert!(s.debug.breakpoints.is_empty());

    // The Breakpoints tab.
    s.execute("run.viewBreakpoints", &Value::Null).unwrap();
    assert_eq!(s.debug.tab, DebugTab::Breakpoints);
    assert!(s.ws.layout.is_visible(codecraft_workspace::ToolWindowId::Debug));
}

#[test]
fn stepping_commands_are_disabled_without_a_session() {
    let mut s = Session::new();
    s.lsp_enabled = false;
    for id in [
        "run.debuggingActions.stepOver",
        "run.debuggingActions.stepInto",
        "run.debuggingActions.stepOut",
        "run.debuggingActions.resume",
        "run.debuggingActions.pause",
        "run.debuggingActions.runToCursor",
        "run.debuggingActions.evaluateExpression",
        "debug.stop",
        "debug.rerun",
    ] {
        assert!(s.enabled(id).is_err(), "{id} should be disabled");
    }
    let state = s.execute("debug.state", &Value::Null).unwrap();
    assert_eq!(state["active"], false);
    // Bad targets fail cleanly.
    assert!(
        s.execute("debug.launch", &json!({ "target": { "kind": "executable", "name": "x", "program": "/no/such" } }))
            .is_err()
    );
    assert!(s.execute("debug.launch", &json!({ "target": 5 })).is_err());
    assert!(s.execute("debug.executable", &json!({})).is_err());
}

// ---------------------------------------------------------------- real sessions

fn cargo() -> Option<PathBuf> {
    dap::discover::which("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

#[test]
fn rust_session_with_lldb_dap() {
    if dap::discover::find_lldb_dap().is_err() || cargo().is_none() {
        eprintln!("skipping: lldb-dap or cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Debug info records real paths; open the project through its canonical path.
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ccdebugee\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(
        &main,
        "fn main() {\n\
         \x20   let numbers = vec![1, 2, 3];\n\
         \x20   let total: i32 = numbers.iter().sum();\n\
         \x20   let label = String::from(\"sum\");\n\
         \x20   let doubled = total * 2;\n\
         \x20   println!(\"{label} {doubled}\");\n\
         \x20   let tripled = total * 3;\n\
         \x20   println!(\"{tripled}\");\n\
         }\n",
    )
    .unwrap();
    let mut s = open_project(&dir);
    assert!(!s.run_configs.is_empty(), "cargo metadata gave no run configurations");
    s.execute("file.open", &json!({ "path": main })).unwrap();
    s.execute("run.toggleBreakpoint.lineBreakpoint", &json!({ "path": main, "line": 4 })).unwrap();
    s.execute("debug.addWatch", &json!({ "expression": "total" })).unwrap();

    s.execute("run.debug", &Value::Null).unwrap();
    assert!(s.enabled("debug.stop").is_ok());
    pump_until(&mut s, "the breakpoint", |s| stopped_line(s) == Some(4));
    let session = s.debug.session.clone().unwrap();
    assert_eq!(session.adapter, "lldb-dap");
    assert!(session.frames[0].name.contains("main"), "{:?}", session.frames[0]);
    assert_eq!(session.execution.as_ref().map(|(p, l)| (p.as_path(), *l)), Some((main.as_path(), 4)));
    assert_eq!(local(&s, "total").as_deref(), Some("6"));
    assert!(local(&s, "label").unwrap_or_default().contains("sum"), "pretty printers: {:?}", local(&s, "label"));
    assert_eq!(s.debug.watches[0].value.as_deref(), Some("6"));
    assert_eq!(s.debug.breakpoints[0].verified, Some(true));
    // The editor shows the execution line.
    let (editor, doc) = s.ws.active().unwrap();
    assert_eq!(doc.path.as_deref(), Some(main.as_path()));
    assert_eq!(doc.doc.char_to_line(editor.selections.primary().head), 4);
    // Inline values for the lines above.
    assert!(
        session.inline_values.iter().any(|v| v.line == 2 && v.text.contains("total = 6")),
        "{:?}",
        session.inline_values
    );

    // Expand the Vec.
    let numbers = session.variables[&session.scopes[0].reference].iter().find(|v| v.name == "numbers").unwrap().clone();
    assert!(numbers.reference > 0);
    let path =
        codecraft_engine::debug::child_path(&codecraft_engine::debug::scope_path(&session.scopes[0].name), "numbers");
    s.execute("debug.expandVariable", &json!({ "path": path, "reference": numbers.reference })).unwrap();
    pump_until(&mut s, "the Vec's elements", |s| {
        s.debug.session.as_ref().is_some_and(|x| x.variables.get(&numbers.reference).is_some_and(|v| v.len() == 3))
    });

    // Evaluate Expression.
    s.execute("run.debuggingActions.evaluateExpression", &json!({ "expression": "total" })).unwrap();
    pump_until(&mut s, "the evaluation", |s| s.debug.evaluation.as_ref().is_some_and(|e| !e.pending));
    let evaluation = s.debug.evaluation.clone().unwrap();
    assert_eq!(evaluation.result.map(|v| v.value), Ok("6".to_owned()));

    // Step over (F8) twice: line 5, then 6.
    s.execute("run.debuggingActions.stepOver", &Value::Null).unwrap();
    pump_until(&mut s, "the step", |s| stopped_line(s) == Some(5));
    s.execute("run.debuggingActions.stepOver", &Value::Null).unwrap();
    pump_until(&mut s, "the second step", |s| stopped_line(s) == Some(6));
    assert_eq!(local(&s, "doubled").as_deref(), Some("12"));

    // Resume to the end.
    s.execute("run.debuggingActions.resume", &Value::Null).unwrap();
    pump_until(&mut s, "the program to exit", |s| matches!(status(s), Some(DebugStatus::Exited { .. })));
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    let out: Vec<&str> =
        s.debug.console.iter().filter(|l| l.kind == DebugConsoleKind::Stdout).map(console_text).collect();
    assert!(out.iter().any(|l| l.contains("sum 12")), "{out:?}");
    assert!(out.iter().any(|l| l.contains("18")), "{out:?}");
    assert!(s.enabled("debug.rerun").is_ok());
    s.shutdown();
}

#[test]
fn python_session_with_debugpy() {
    if dap::discover::find_debugpy(None).is_err() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = dir.join("app.py");
    std::fs::write(
        &script,
        "def work(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n\nresult = work(4)\nprint('result', result)\n",
    )
    .unwrap();
    let mut s = open_project(&dir);
    s.execute("debug.setBreakpoint", &json!({ "path": script, "line": 4 })).unwrap();
    s.execute("debug.atCaret", &json!({ "path": script, "line": 0 })).unwrap();
    pump_until(&mut s, "the breakpoint", |s| stopped_line(s) == Some(4));
    assert_eq!(s.debug.session.as_ref().unwrap().adapter, "debugpy");
    assert_eq!(local(&s, "total").as_deref(), Some("6"));
    assert_eq!(s.debug.session.as_ref().unwrap().frames.len(), 2, "work + <module>");

    // The console REPL.
    s.execute("debug.consoleEvaluate", &json!({ "expression": "total * 10" })).unwrap();
    pump_until(&mut s, "the console result", |s| s.debug.console.iter().any(|l| l.kind == DebugConsoleKind::Result));
    assert!(s.debug.console.iter().any(|l| l.kind == DebugConsoleKind::Result && console_text(l) == "60"));

    // Select the caller frame.
    s.execute("debug.selectFrame", &json!({ "index": 1 })).unwrap();
    pump_until(&mut s, "the caller's frame", |s| stopped_line(s) == Some(6));

    // Run to cursor: the print line.
    s.execute("run.debuggingActions.runToCursor", &json!({ "path": script, "line": 7 })).unwrap();
    pump_until(&mut s, "run to cursor", |s| stopped_line(s) == Some(7));
    assert_eq!(local(&s, "result").as_deref(), Some("6"));

    s.execute("run.debuggingActions.resume", &Value::Null).unwrap();
    pump_until(&mut s, "the exit", |s| matches!(status(s), Some(DebugStatus::Exited { code: Some(0) })));
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    assert!(s.debug.console.iter().any(|l| console_text(l).contains("result 6")));
    s.shutdown();
}

#[test]
fn stopping_a_session_leaves_no_processes() {
    if dap::discover::find_debugpy(None).is_err() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = dir.join("forever.py");
    std::fs::write(&script, "import time\nwhile True:\n    time.sleep(0.05)\n").unwrap();
    let mut s = open_project(&dir);
    s.execute("debug.atCaret", &json!({ "path": script })).unwrap();
    pump_until(&mut s, "the program to run", |s| {
        s.debug.is_running() && s.debug.session.as_ref().is_some_and(|x| x.debuggee_pid.is_some())
    });
    let pid = s.debug.session.as_ref().unwrap().debuggee_pid.unwrap();
    assert!(alive(pid));

    // Pause shows where it is, then stop.
    s.execute("run.debuggingActions.pause", &Value::Null).unwrap();
    pump_until(&mut s, "the pause", |s| stopped_line(s).is_some());
    s.execute("debug.stop", &Value::Null).unwrap();
    assert!(matches!(status(&s), Some(DebugStatus::Exited { .. })));
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    let start = Instant::now();
    while alive(pid) && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid), "the debuggee survived the stop");
}
