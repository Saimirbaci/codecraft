//! QA M14 #24: every engine tool's result uses camelCase keys (no `command_line` next to
//! `commandLine`) and shows no CodeCraft temp files (pytest's JUnit XML). The tools run on a temp
//! project: Python with debugpy and pytest, ruff, a fake profiler, SQLite, a run configuration;
//! parts whose tool is missing are skipped (and listed). Never touches the user's home.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_access::{AgentAccess, Capability};
use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};

const LONG: Duration = Duration::from_secs(300);

/// Maps whose keys are data (names the user or a tool chose), not field names.
const DATA_MAPS: &[&str] = &["counts", "env", "languageServers"];

/// Every object key under `v` that is not camelCase (has `_`), with its JSON path.
fn snake_keys(v: &Value, path: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                let here = format!("{path}.{k}");
                if k.contains('_') {
                    out.push(here.clone());
                }
                if !DATA_MAPS.contains(&k.as_str()) {
                    snake_keys(x, &here, out);
                }
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                snake_keys(x, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

struct Walk {
    s: Session,
    checked: Vec<String>,
    skipped: Vec<String>,
    problems: Vec<String>,
}

impl Walk {
    /// Call a tool; walk its JSON result. Returns it (Null when it failed).
    fn call(&mut self, name: &str, args: Value) -> Value {
        let r = agent_tools::call_blocking(&mut self.s, Caller::new("walker"), name, args.clone(), LONG);
        if r.is_error {
            self.skipped.push(format!("{name}: {}", result_text(&r)));
            return Value::Null;
        }
        let v = result_json(&r);
        let mut bad = Vec::new();
        snake_keys(&v, name, &mut bad);
        self.problems.extend(bad);
        let text = result_text(&r);
        let tmp = std::env::temp_dir().display().to_string();
        let tmp = tmp.trim_end_matches('/');
        if text.contains(&format!("{tmp}/codecraft-junit")) || text.contains("junit_logging") {
            self.problems.push(format!("{name}: an internal JUnit report path in {text}"));
        }
        if !self.checked.iter().any(|c| c == name) {
            self.checked.push(name.to_string());
        }
        v
    }
}

fn have_python_module(m: &str) -> bool {
    Command::new("python3").args(["-c", &format!("import {m}")]).output().is_ok_and(|o| o.status.success())
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
    #[cfg(not(unix))]
    {
        let _ = body;
        dir.join(name)
    }
}

#[test]
fn every_tool_result_is_camel_case() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let dir = root.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("app.py"),
        "import os\n\n\ndef work(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n\n\n\
         if __name__ == \"__main__\":\n    print(work(5))\n",
    )
    .unwrap();
    std::fs::write(dir.join("test_app.py"), "from app import work\n\n\ndef test_work():\n    assert work(3) == 4\n")
        .unwrap();
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(dir.join("shop.db")),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in
        ["create table items (id integer primary key, item_name text)", "insert into items(item_name) values ('apple')"]
    {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    drop(conn);
    // A profiler that writes collapsed stacks to the path after `-o` (as in agent_tools.rs).
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fixture = bin.join("samply.fixture");
    std::fs::write(&fixture, "main;work;inner 30\nmain;work 10\n").unwrap();
    script(
        &bin,
        "samply",
        &format!(
            "out=''\nprev=''\nfor a in \"$@\"; do if [ \"$prev\" = -o ]; then out=\"$a\"; fi; prev=\"$a\"; done\ncat '{}' > \"$out\"\n",
            fixture.display()
        ),
    );

    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&root.join("home"));
    s.db.set_secret_store(Arc::new(codecraft_engine::db::MemoryStore::default()));
    s.db.history.set_dir(root.join("home/db-history"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < LONG {
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    s.profiler.tool_dirs = Some(vec![bin]);
    s.profiler.perf_allowed = Some(false);
    s.execute("runConfig.create", &json!({ "name": "echo", "kind": "command", "command": "/bin/echo hi" })).unwrap();
    s.agent_access = AgentAccess::headless(&Capability::ALL);
    let mut w = Walk { s, checked: Vec::new(), skipped: Vec::new(), problems: Vec::new() };

    // Generic, files, analysis.
    w.call("codecraft_commands", json!({ "query": "step over", "limit": 3 }));
    w.call("project_info", json!({}));
    w.call("file_read", json!({ "path": "app.py", "startLine": 1, "endLine": 3 }));
    w.call("file_write", json!({ "path": "out.txt", "content": "x\n" }));
    w.call("file_edit", json!({ "path": "out.txt", "oldText": "x", "newText": "y" }));
    w.call("editor_open", json!({ "path": "app.py", "line": 2 }));
    w.call("inspect", json!({ "scope": "file", "path": "app.py", "tools": ["ruff"], "timeoutMs": 120_000 }));
    w.call("problems", json!({}));
    for (tool, args) in [
        ("definition", json!({ "path": "app.py", "line": 12, "symbol": "work" })),
        ("references", json!({ "path": "app.py", "line": 4, "symbol": "work" })),
        ("hover", json!({ "path": "app.py", "line": 4, "column": 5 })),
        ("document_symbols", json!({ "path": "app.py" })),
        ("workspace_symbols", json!({ "query": "work" })),
        ("call_hierarchy", json!({ "path": "app.py", "line": 4, "symbol": "work" })),
    ] {
        // Language servers are off in this session (their results are compacted by
        // `compact_lsp`, checked by agent_tools.rs with rust-analyzer when it is installed).
        w.call(tool, args);
    }

    // Runs and run configurations.
    w.call("run_configurations", json!({}));
    let run = w.call("run_start", json!({ "config": "echo" }));
    if let Some(tab) = run["tab"].as_u64() {
        w.call("run_output", json!({ "tab": tab, "waitMs": 60_000 }));
        w.call("run_stop", json!({ "tab": tab }));
    }

    // Debugging (debugpy).
    w.call("debug_breakpoint_set", json!({ "path": "app.py", "line": 7, "condition": "i == 2" }));
    w.call("debug_breakpoints", json!({}));
    w.call("debug_wait_for_stop", json!({ "timeoutMs": 10 }));
    if codecraft_engine::dap::discover::find_debugpy(None).is_ok() {
        w.call("debug_start", json!({ "file": "app.py" }));
        let stop = w.call("debug_wait_for_stop", json!({ "timeoutMs": 120_000 }));
        if stop["state"] == "stopped" {
            w.call("debug_stack", json!({}));
            let vars = w.call("debug_variables", json!({}));
            let structured = vars.pointer("/scopes/0/ref").and_then(Value::as_i64);
            if let Some(r) = structured {
                w.call("debug_variables", json!({ "ref": r }));
            }
            w.call("debug_evaluate", json!({ "expression": "total" }));
            w.call("debug_control", json!({ "action": "stepOver", "timeoutMs": 60_000 }));
            w.call("debug_breakpoint_remove", json!({ "all": true }));
            w.call("debug_control", json!({ "action": "continue", "timeoutMs": 60_000 }));
        }
    } else {
        w.skipped.push("debug_start: debugpy is missing".into());
    }
    w.call("debug_breakpoint_remove", json!({ "path": "app.py", "line": 7 }));

    // Profiling (a fake samply).
    w.call("profile_start", json!({ "config": "echo" }));
    w.call("profile_hotspots", json!({ "limit": 5, "timeoutMs": 60_000 }));
    w.call("profile_stop", json!({ "limit": 2 }));

    // Tests (pytest): the failing test's summary, and no JUnit temp file in `commandLine`.
    if have_python_module("pytest") {
        let run = w.call("test_run", json!({ "path": "test_app.py", "timeoutMs": 240_000 }));
        assert_eq!(run["status"], "failed", "{run}");
        let cl = run["commandLine"].as_str().unwrap_or_default();
        assert!(cl.contains("pytest") && !cl.contains("junitxml"), "{cl}");
        let results = w.call("test_results", json!({ "includePassed": true }));
        if let Some(tab) = results["tab"].as_u64() {
            w.call("run_output", json!({ "tab": tab, "tail": 5 }));
        }
    } else {
        w.skipped.push("test_run: pytest is missing".into());
    }

    // Databases (a column named in snake_case stays a value, not a key).
    w.call("db_sources", json!({}));
    w.call("db_query", json!({ "dataSource": "shop.db", "sql": "select item_name from items" }));
    w.call("db_query", json!({ "dataSource": "shop.db", "sql": "select count(*) as n_items from items" }));
    // A couple of commands agents run through codecraft_execute in the playbook.
    // (codecraft_execute passes a command's own JSON through; older commands such as
    // `debug.state` keep their snake_case fields, so the agent commands are checked here.)
    w.call("codecraft_execute", json!({ "id": "debug.agentSummary" }));
    w.call("codecraft_execute", json!({ "id": "project.info" }));
    w.call("codecraft_execute", json!({ "id": "analysis.problems", "params": {} }));

    eprintln!("checked: {}", w.checked.join(", "));
    eprintln!("not checked here:\n  {}", w.skipped.join("\n  "));
    w.s.shutdown();
    assert!(w.problems.is_empty(), "snake_case keys or internal paths in tool results:\n{}", w.problems.join("\n"));
    // The tools that need nothing installed always answered.
    for must in [
        "codecraft_commands",
        "project_info",
        "file_read",
        "file_write",
        "file_edit",
        "editor_open",
        "inspect",
        "problems",
        "run_configurations",
        "run_start",
        "run_output",
        "debug_breakpoint_set",
        "debug_breakpoints",
        "debug_wait_for_stop",
        "profile_start",
        "profile_hotspots",
        "db_sources",
        "db_query",
        "codecraft_execute",
    ] {
        assert!(w.checked.iter().any(|c| c == must), "{must} did not answer: {:?}", w.skipped);
    }
}

#[test]
fn the_walker_finds_snake_case() {
    let mut out = Vec::new();
    snake_keys(
        &json!({ "ok": 1, "a_b": { "c_d": 2 }, "list": [{ "e_f": 3 }], "counts": { "weak_warning": 1 } }),
        "t",
        &mut out,
    );
    assert_eq!(out, ["t.a_b", "t.a_b.c_d", "t.list[0].e_f"]);
}
