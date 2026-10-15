//! M15 track AI with the built binary: `mcp --print-config` for every client, `mcp --ide`
//! finding a running IDE (and failing cleanly without one), and `notifications/cancelled` over
//! stdio. Every run gets its own CODECRAFT_HOME and HOME; no real agent CLI is run.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CLI: &str = env!("CARGO_BIN_EXE_codecraft-cli");

fn tmpdir() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

fn project(root: &Path) -> PathBuf {
    let dir = root.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\n").unwrap();
    dir
}

fn cli(root: &Path) -> Command {
    let mut c = Command::new(CLI);
    c.env("CODECRAFT_HOME", root.join("home"))
        .env("HOME", root.join("home"))
        .env("RUST_LOG", "error")
        .env_remove("CODECRAFT_IDE_PORT")
        .env_remove("CODECRAFT_JOB")
        .env_remove("CODECRAFT_JOB_KEY")
        .env_remove("CLAUDE_CONFIG_DIR");
    c
}

#[test]
fn print_config_for_every_client() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let exe = PathBuf::from(CLI).canonicalize().unwrap().display().to_string();
    for client in ["claude", "codex", "gemini", "cursor", "grok", "json"] {
        let out = cli(&root).args(["mcp", "--print-config", client, dir.to_str().unwrap()]).output().unwrap();
        assert!(out.status.success(), "{client}: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(text.contains(&exe), "{client}: the absolute binary path: {text}");
        assert!(text.contains(dir.to_str().unwrap()), "{client}: the project: {text}");
        assert!(text.contains("run,debug,profile"), "{client}: the default --allow: {text}");
        match client {
            "claude" => assert!(text.starts_with("claude mcp add codecraft -- "), "{text}"),
            "codex" => {
                assert!(text.contains("[mcp_servers.codecraft]") && text.contains("codex mcp add codecraft"), "{text}")
            }
            "grok" => assert!(text.starts_with("grok mcp add --scope project codecraft -- "), "{text}"),
            _ => {
                let body: String = text.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n");
                let v: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{client}: {e}: {text}"));
                assert_eq!(v["mcpServers"]["codecraft"]["command"], json!(exe));
                assert_eq!(v["mcpServers"]["codecraft"]["args"][0], "mcp");
            }
        }
        let ide = cli(&root).args(["mcp", "--print-config", client, "--ide"]).output().unwrap();
        let text = String::from_utf8_lossy(&ide.stdout);
        assert!(ide.status.success() && text.contains("codecraft-ide") && text.contains("--ide"), "{client}: {text}");
    }
    // The current directory is the default project; --allow is passed through.
    let out =
        cli(&root).current_dir(&dir).args(["mcp", "--print-config", "claude", "--allow", "run"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.trim_end().ends_with(&format!("mcp {} --allow run", dir.display())), "{text}");
    let bad = cli(&root).args(["mcp", "--print-config", "vim"]).output().unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("claude, codex, gemini, cursor, grok, json"));
    let bad = cli(&root).args(["mcp", "--print-config", "claude", "/definitely/not/here"]).output().unwrap();
    assert!(!bad.status.success());
    let bad = cli(&root)
        .args(["mcp", "--print-config", "claude", dir.to_str().unwrap(), "--allow", "root"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown capability"));
    // Nothing was written to any client's configuration.
    assert!(!root.join("home/.claude").exists() && !root.join("home/.gemini").exists());
    assert!(!root.join("home/.codex").exists() && !root.join("home/.grok").exists());
}

#[test]
fn ide_bridge_without_an_ide_fails_cleanly() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let out = cli(&root)
        .current_dir(&dir)
        .args(["mcp", "--ide", "--lock-dir", root.join("locks").to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no running CodeCraft"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Lines from a child's stdout as JSON, on a thread.
fn reader(child: &mut Child) -> crossbeam_channel::Receiver<Value> {
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
    rx
}

fn wait_id(rx: &crossbeam_channel::Receiver<Value>, id: u64, seen: &mut Vec<Value>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(50)) {
            seen.push(v.clone());
            if v["id"] == id {
                return v;
            }
        }
    }
    panic!("no answer to {id}; got {seen:?}");
}

fn text_json(v: &Value) -> Value {
    serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[cfg(unix)]
#[test]
fn stdio_cancellation_with_the_built_binary() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let mut child = cli(&root)
        .args(["mcp", dir.to_str().unwrap(), "--no-lsp", "--allow", "run,write"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let rx = reader(&mut child);
    let mut send = |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    let mut seen = Vec::new();
    send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "t" } } }));
    wait_id(&rx, 1, &mut seen);
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let call = |id: u64, name: &str, args: Value| json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } });
    send(call(
        2,
        "codecraft_execute",
        json!({ "id": "runConfig.create", "params": { "name": "nap", "kind": "command", "command": "/bin/sleep 30" } }),
    ));
    let created = wait_id(&rx, 2, &mut seen);
    assert!(created["result"].get("isError").is_none(), "{created}");
    send(call(3, "run_start", json!({ "config": "nap" })));
    let tab = text_json(&wait_id(&rx, 3, &mut seen))["tab"].as_u64().unwrap();
    send(call(4, "run_output", json!({ "tab": tab, "waitMs": 60_000 })));
    std::thread::sleep(Duration::from_millis(400));
    send(
        json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 4, "reason": "user" } }),
    );
    send(call(5, "run_stop", json!({ "tab": tab })));
    let stopped = text_json(&wait_id(&rx, 5, &mut seen));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    // The run's end would have answered 4; it was cancelled, so nothing comes.
    let until = Instant::now() + Duration::from_millis(600);
    while Instant::now() < until {
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(20)) {
            seen.push(v);
        }
    }
    assert!(!seen.iter().any(|v| v["id"] == 4), "the cancelled call was answered: {seen:?}");
    drop(send);
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
}

/// `codecraft-cli mcp --ide` started in a project's directory reaches the CodeCraft window that
/// has it open; with a job's environment it is that job.
#[cfg(unix)]
#[test]
fn ide_bridge_finds_the_running_ide() {
    use codecraft_engine::Session;
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let locks = root.join("locks");
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&root.join("home"));
    s.ide.set_lock_dir(Some(locks.clone()));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    let start = Instant::now();
    while s.ide.port().is_none() && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    let port = s.ide.port().unwrap();
    let mut child = cli(&root)
        .current_dir(&dir)
        .args(["mcp", "--ide", "--lock-dir", locks.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let rx = reader(&mut child);
    let mut seen = Vec::new();
    let pump_wait = |s: &mut Session, id: u64, seen: &mut Vec<Value>| -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            s.pump();
            if let Ok(v) = rx.recv_timeout(Duration::from_millis(5)) {
                seen.push(v.clone());
                if v["id"] == id {
                    return v;
                }
            }
        }
        panic!("no answer to {id}: {seen:?}");
    };
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "grok" } } })
    )
    .unwrap();
    stdin.flush().unwrap();
    pump_wait(&mut s, 1, &mut seen);
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "file_read", "arguments": { "path": "notes.txt" } } })
    )
    .unwrap();
    stdin.flush().unwrap();
    let r = pump_wait(&mut s, 2, &mut seen);
    assert!(text_json(&r)["content"].as_str().unwrap().contains("alpha"), "{r}");
    assert_eq!(s.ide.client_count(), 1);
    let client = &s.ide.clients()[0];
    assert_eq!(client.info["name"], "grok");
    assert!(client.info.get("codecraftJob").is_none(), "no job environment: an ordinary client");
    drop(stdin);
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(10) {
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(child.wait().unwrap().success());
    let _ = port;
    s.shutdown();
}
