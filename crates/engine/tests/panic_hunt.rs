//! Every command, every session state, hostile params: commands may fail, never panic.
//! (`Session::execute` turns a panic into `Failed("internal error in …")`, which this test rejects.)
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{CommandError, Session, registry};

fn hostile_params(tmp: &Path) -> Vec<Value> {
    let missing = tmp.join("does-not-exist").display().to_string();
    vec![
        Value::Null,
        json!({}),
        json!([1, 2, 3]),
        json!("string"),
        json!({ "offset": u64::MAX, "line": u64::MAX, "column": u64::MAX, "index": u64::MAX, "lines": u64::MAX }),
        json!({ "offset": -1, "line": -5, "delta": i64::MIN, "editor": -1, "tab": -1, "config": -1 }),
        json!({ "anchor": 3, "head": 1_000_000_000_000u64, "clicks": 7 }),
        json!({ "text": "" }),
        json!({ "text": "\u{0}\r\n😀\t\u{200D}🏳️‍🌈é" }),
        json!({ "text": 42, "path": 7, "query": [], "pattern": {} }),
        json!({ "path": missing }),
        json!({ "motion": "sideways", "extend": "yes" }),
        json!({ "key": "no_such_setting", "value": 1 }),
        json!({ "key": "editor_font_size", "value": "huge" }),
        json!({ "key": "tab_size", "value": 0 }),
        json!({ "pattern": "((", "regex": true, "replace": "$9" }),
        json!({ "pattern": "", "replace": "" }),
        json!({ "newName": "" }),
        json!({ "kind": "nope", "package": "x", "name": "y" }),
        json!({ "editor": 999_999, "tab": 999_999, "config": 999_999, "index": 999_999 }),
        json!({ "query": "\u{FFFF}", "limit": 0 }),
    ]
}

/// Commands that start long-running processes or touch paths outside the temp dir are run only
/// with params that make them bail out early.
fn skip(id: &str, params: &Value) -> bool {
    // Never let the fuzzer start a coding agent (paid API usage) or submit feedback for real.
    let launches_agent = matches!(id, "agent.start" | "agent.followUp" | "feedback.submit" | "feedback.runIteration");
    if launches_agent
        && (params.get("backend").is_some() || params.get("confirm").is_some() || params.get("job").is_some())
    {
        return true;
    }
    // Resuming a saved session starts the agent again.
    if id == "agent.sessions.resume" && (params.get("session").is_some() || params.get("job").is_some()) {
        return true;
    }
    // Generate with `agent` starts a coding agent too.
    if id == "runConfig.generate" && params.get("agent").is_some() {
        return true;
    }
    // Manage Accounts runs the real `gh`/`glab auth status` (misc_cmds tests use a fake one).
    if matches!(id, "git.gitHub.manageAccounts" | "git.gitLab.manageAccounts") {
        return true;
    }
    // Docker commands run the real `docker` CLI (docker_cmds tests fuzz them with a fake one).
    if id.starts_with("docker.")
        || id.starts_with("tools.services.docker")
        || matches!(
            id,
            "tools.services.devContainers.showDevContainers"
                | "tools.services.devContainers.manageDevContainerBackends"
        )
    {
        return true;
    }
    // Profiling runs the real samply/perf/py-spy, and Clear Recent edits the real profiler state
    // (profiler_cmds tests fuzz them with fakes and a temp dir).
    if id.starts_with("run.profile")
        || matches!(id, "run.attachProfilerToProcess" | "profiler.install" | "profiler.clearRecent")
    {
        return true;
    }
    let spawns = id.starts_with("cargo.")
        || id.starts_with("build.")
        || matches!(
            id,
            "run.run"
                | "run.rerun"
                | "run.contextConfiguration"
                | "tools.rust.reformatCargoProjectWithRustfmt"
                | "run.runWithCoverage"
                | "test.rerun"
                | "test.rerunFailed"
        );
    spawns && !params.is_null()
}

fn check_all(s: &mut Session, tmp: &Path, state: &str) {
    for spec in registry().all() {
        for params in hostile_params(tmp) {
            if skip(spec.id, &params) {
                continue;
            }
            if let Err(CommandError::Failed(msg)) = s.execute(spec.id, &params)
                && msg.starts_with("internal error")
            {
                panic!("{state}: {} panicked with {params}: {msg}", spec.id);
            }
            s.pump();
        }
    }
}

fn wait_idle(s: &mut Session) {
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn no_command_panics_without_anything_open() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    check_all(&mut s, tmp.path(), "empty");
    s.shutdown();
}

#[test]
fn no_command_panics_with_an_editor_open() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("lib.rs");
    std::fs::write(&file, "fn main() {\n    let x = (1, [2, 3]);\n}\n\n// 😀 ünïcode\r\n\tlet y = \"str\";\n").unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
    wait_idle(&mut s);
    s.execute("file.open", &json!({ "path": file })).unwrap();
    // A multi-caret selection state too.
    s.execute("editor.setSelection", &json!({ "anchor": 3, "head": 9 })).unwrap();
    s.execute("edit.find.selectAllOccurrences", &Value::Null).unwrap();
    check_all(&mut s, tmp.path(), "editor");
    wait_idle(&mut s);
    s.shutdown();
}

#[test]
fn no_command_panics_with_popups_open() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("a.rs");
    std::fs::write(&file, "struct A;\nimpl A { fn f(&self) {} }\n").unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
    wait_idle(&mut s);
    for opener in [
        "navigate.file",
        "navigate.fileStructure",
        "help.findAction",
        "view.recentFiles",
        "navigate.lineColumn",
        "edit.find.find",
    ] {
        // The previous round may have closed every tab.
        s.execute("file.open", &json!({ "path": file })).unwrap();
        s.execute(opener, &Value::Null).unwrap();
        check_all(&mut s, tmp.path(), opener);
    }
    wait_idle(&mut s);
    s.shutdown();
}
