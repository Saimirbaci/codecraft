//! `codecraft-cli mcp` over stdio with the built binary (initialize, tools/list, a read tool, a
//! denied write, an allowed debug call), and the one-shot `debug`, `test` and `analyze`
//! subcommands (each skipped when its tool is missing). Every run gets its own CODECRAFT_HOME.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const CLI: &str = env!("CARGO_BIN_EXE_codecraft-cli");

fn project(tmp: &Path) -> PathBuf {
    let dir = tmp.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\n").unwrap();
    dir.canonicalize().unwrap()
}

fn cli(tmp: &Path) -> Command {
    let mut c = Command::new(CLI);
    c.env("CODECRAFT_HOME", tmp.join("home")).env("RUST_LOG", "error");
    c
}

#[test]
fn mcp_stdio_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut child = cli(tmp.path())
        // Nothing here may touch the real home (no language servers, no IDE lock file).
        .env("HOME", tmp.path().join("home"))
        .args(["mcp", dir.to_str().unwrap(), "--no-lsp", "--allow", "debug"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
            if tx.send(v).is_err() {
                break;
            }
        }
    });
    let requests = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test-client", "version": "1" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "file_read", "arguments": { "path": "notes.txt" } } }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "file_write", "arguments": { "path": "notes.txt", "content": "pwned" } } }),
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "debug_breakpoint_set", "arguments": { "path": "notes.txt", "line": 2 } } }),
        json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "debug_breakpoints", "arguments": {} } }),
    ];
    for r in &requests {
        writeln!(stdin, "{r}").unwrap();
    }
    stdin.flush().unwrap();
    let mut got = std::collections::BTreeMap::new();
    while got.len() < 6 {
        let v = rx.recv_timeout(Duration::from_secs(60)).expect("the server answered every request");
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            got.insert(id, v);
        }
    }
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");

    assert_eq!(got[&1]["result"]["serverInfo"]["name"], "CodeCraft");
    let tools = got[&2]["result"]["tools"].as_array().unwrap();
    assert!(tools.len() >= 30);
    assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    assert!(tools.iter().any(|t| t["name"] == "debug_wait_for_stop"));
    let text = |id: u64| got[&id]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let read: Value = serde_json::from_str(&text(3)).unwrap();
    assert_eq!(read["content"], "1\talpha\n2\tbeta");
    assert_eq!(got[&4]["result"]["isError"], true);
    assert!(text(4).contains("--allow write"), "{}", text(4));
    assert_eq!(std::fs::read_to_string(dir.join("notes.txt")).unwrap(), "alpha\nbeta\n");
    assert!(got[&5]["result"].get("isError").is_none(), "{}", got[&5]);
    let bps: Value = serde_json::from_str(&text(6)).unwrap();
    assert_eq!(bps[0]["line"], 2);
    assert!(!tmp.path().join("home/.claude").exists(), "no IDE lock file headless");
}

#[test]
fn mcp_help_prints_client_snippets() {
    let tmp = tempfile::tempdir().unwrap();
    let out = cli(tmp.path()).args(["mcp", "--help"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("claude mcp add codecraft --"), "{text}");
    assert!(text.contains("[mcp_servers.codecraft]"));
    assert!(text.contains("debug_wait_for_stop"));
    let bad = cli(tmp.path()).args(["mcp", "/definitely/not/here"]).output().unwrap();
    assert!(!bad.status.success());
    let bad = cli(tmp.path()).args(["mcp", "/tmp", "--allow", "root"]).output().unwrap();
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown capability"));
    let bridge = cli(tmp.path()).args(["--mcp-bridge"]).output().unwrap();
    assert_eq!(bridge.status.code(), Some(2));
}

fn has_debugpy() -> bool {
    Command::new("python3").args(["-c", "import debugpy"]).output().is_ok_and(|o| o.status.success())
}

#[test]
fn one_shot_debug_of_a_python_script() {
    if !has_debugpy() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    std::fs::write(
        dir.join("app.py"),
        "def work(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n\nprint(work(4))\n",
    )
    .unwrap();
    let out = cli(tmp.path())
        .args([
            "debug",
            dir.to_str().unwrap(),
            "app.py",
            "--break",
            "app.py:5",
            "--eval",
            "total * 2",
            "--eval",
            "nope_x",
            "--json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["stop"]["state"], "stopped");
    assert_eq!(v["stop"]["frames"][0]["line"], 5);
    assert_eq!(v["evaluations"][0]["value"], "12");
    assert!(v["evaluations"][1]["error"].is_string());
    // Text output too.
    let out = cli(tmp.path()).args(["debug", dir.to_str().unwrap(), "app.py", "--break", "app.py:5"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Stopped (breakpoint)") && text.contains("total"), "{text}");
}

fn cargo_available() -> bool {
    Command::new("cargo").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn one_shot_tests_of_a_cargo_project() {
    if !cargo_available() {
        eprintln!("skipping: cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"axcli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn good() {}\n\n    #[test]\n    fn bad() {\n        panic!(\"boom\");\n    }\n}\n",
    )
    .unwrap();
    let out = cli(tmp.path()).args(["test", dir.to_str().unwrap(), "--json"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "failures exit with 1: {stdout}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["counts"]["passed"], 1);
    assert_eq!(v["counts"]["failed"], 1);
    assert!(v["failures"][0]["message"].as_str().unwrap_or_default().contains("boom"), "{v}");
    let out = cli(tmp.path()).args(["test", dir.to_str().unwrap(), "good"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed, 0 failed"), "{text}");
}

fn ruff_available() -> bool {
    Command::new("ruff").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn one_shot_analysis_as_sarif() {
    if !ruff_available() {
        eprintln!("skipping: ruff is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"x\"\nversion = \"0\"\n").unwrap();
    std::fs::write(dir.join("mod.py"), "import os\n\n\ndef f():\n    return 1\n").unwrap();
    let out = cli(tmp.path())
        .args(["analyze", dir.to_str().unwrap(), "--format", "sarif", "--tools", "ruff"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{e}: {stdout}\n{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(v["version"], "2.1.0");
    let results = v["runs"][0]["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"] == "mod.py"),
        "{v}"
    );
    let out = cli(tmp.path())
        .args(["analyze", dir.to_str().unwrap(), "--scope", "mod.py", "--tools", "ruff"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("mod.py:1:"), "{text}");
    let bad = cli(tmp.path()).args(["analyze", dir.to_str().unwrap(), "--format", "xml"]).output().unwrap();
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown format"));
}
