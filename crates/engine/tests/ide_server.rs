//! The IDE server end to end: a test client plays Claude Code against a real Session (lock
//! file discovery, auth, MCP, tools, openDiff approvals). Never runs the real `claude` CLI and
//! never touches the user's `~/.claude/ide` (the lock directory is a temp dir).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codecraft_engine::mcp::tungstenite;
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use codecraft_engine::approvals::{ApprovalKind, Decision};
use codecraft_engine::mcp::{AUTH_HEADER, lockfile};
use codecraft_engine::text::Position;
use codecraft_engine::workspace::{Problem, ProblemSource, Severity};
use codecraft_engine::{Session, normalize_path};

type Ws = WebSocket<TcpStream>;

struct Claude {
    ws: Ws,
    next_id: u64,
    /// Notifications received while waiting for responses.
    notifications: Vec<Value>,
    /// Responses that arrived while waiting for another one.
    responses: Vec<Value>,
}

fn connect(port: u16, token: &str) -> Result<Ws, tungstenite::Error> {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!("ws://127.0.0.1:{port}").into_client_request().unwrap();
    req.headers_mut().insert(AUTH_HEADER, token.parse().unwrap());
    match tungstenite::client(req, stream) {
        Ok((ws, _)) => {
            // Short reads from now on so the test can pump the session while waiting.
            ws.get_ref().set_read_timeout(Some(Duration::from_millis(5))).unwrap();
            Ok(ws)
        }
        Err(tungstenite::HandshakeError::Failure(e)) => Err(e),
        Err(tungstenite::HandshakeError::Interrupted(_)) => panic!("interrupted handshake"),
    }
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

impl Claude {
    fn new(port: u16, token: &str) -> Self {
        Self { ws: connect(port, token).unwrap(), next_id: 0, notifications: Vec::new(), responses: Vec::new() }
    }

    fn send_request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        self.ws.send(Message::text(msg.to_string())).unwrap();
        self.next_id
    }

    fn notify(&mut self, method: &str) {
        self.ws.send(Message::text(json!({ "jsonrpc": "2.0", "method": method }).to_string())).unwrap();
    }

    /// Read one message if any arrives within a few ms.
    fn poll(&mut self) -> Option<Value> {
        match self.ws.read() {
            Ok(Message::Text(t)) => Some(serde_json::from_str(t.as_str()).unwrap()),
            Ok(_) => None,
            Err(e) if is_timeout(&e) => None,
            Err(e) => panic!("connection error: {e}"),
        }
    }

    /// Pump the session until the response to `id` arrives.
    fn wait_response(&mut self, s: &mut Session, id: u64) -> Value {
        let start = Instant::now();
        loop {
            if let Some(i) = self.responses.iter().position(|m| m.get("id") == Some(&json!(id))) {
                return self.responses.remove(i);
            }
            s.pump();
            if let Some(msg) = self.poll() {
                if msg.get("method").is_some() {
                    self.notifications.push(msg);
                } else {
                    self.responses.push(msg);
                }
            }
            assert!(start.elapsed() < Duration::from_secs(20), "no response to request {id}");
        }
    }

    fn call(&mut self, s: &mut Session, tool: &str, args: Value) -> Value {
        let id = self.send_request("tools/call", json!({ "name": tool, "arguments": args }));
        let r = self.wait_response(s, id);
        assert!(r.get("error").is_none(), "{tool}: {r}");
        r["result"].clone()
    }

    /// A tool call whose single text item is JSON.
    fn call_json(&mut self, s: &mut Session, tool: &str, args: Value) -> Value {
        let r = self.call(s, tool, args);
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn handshake(&mut self, s: &mut Session) -> Value {
        let id = self.send_request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "claude-code", "version": "test" } }),
        );
        let init = self.wait_response(s, id);
        self.notify("notifications/initialized");
        init
    }
}

fn texts(result: &Value) -> Vec<String> {
    result["content"].as_array().unwrap().iter().map(|c| c["text"].as_str().unwrap().to_string()).collect()
}

fn pump_until(s: &mut Session, what: &str, mut f: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    while !f(s) {
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
        assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
    }
}

fn lock_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "lock")).collect();
    v.sort();
    v
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    lock_dir: PathBuf,
    main_rs: PathBuf,
    s: Session,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = normalize_path(&tmp.path().canonicalize().unwrap().join("proj"));
    std::fs::create_dir_all(root.join("src")).unwrap();
    let main_rs = root.join("src/main.rs");
    std::fs::write(&main_rs, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    let lock_dir = tmp.path().join("claude-ide");
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.ide.set_lock_dir(Some(lock_dir.clone()));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    pump_until(&mut s, "project load", |s| !s.jobs_running());
    pump_until(&mut s, "server start", |s| s.ide.is_running());
    Fixture { _tmp: tmp, root, lock_dir, main_rs, s }
}

/// The lock file Claude Code would read, and a connected, initialized client.
fn connect_claude(f: &mut Fixture) -> Claude {
    let locks = lock_files(&f.lock_dir);
    assert_eq!(locks.len(), 1, "{locks:?}");
    let info = lockfile::read(&locks[0]).unwrap();
    let port: u16 = locks[0].file_stem().unwrap().to_str().unwrap().parse().unwrap();
    assert_eq!(Some(port), f.s.ide.port());
    assert_eq!(info.ide_name, "CodeCraft");
    assert_eq!(info.transport, "ws");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.workspace_folders, vec![f.root.display().to_string()]);
    let mut claude = Claude::new(port, &info.auth_token);
    let init = claude.handshake(&mut f.s);
    assert_eq!(init["result"]["serverInfo"]["name"], "CodeCraft");
    pump_until(&mut f.s, "client registered", |s| s.ide.client_count() == 1);
    claude
}

/// Answer the single pending approval (an openDiff card) after checking it.
fn decide_pending(s: &mut Session, decision: Decision, expect_path: &Path, expect_proposed: &str) {
    pump_until(s, "approval card", |s| !s.approvals.is_empty());
    let req = s.approvals.pending().next().unwrap().clone();
    assert_eq!(req.source, "Claude Code");
    match &req.kind {
        ApprovalKind::FileEdit { path, proposed, .. } => {
            assert_eq!(path, expect_path);
            assert_eq!(proposed, expect_proposed);
        }
        k => panic!("unexpected approval {k:?}"),
    }
    assert!(s.approvals.decide(req.id, decision));
}

#[test]
fn claude_code_session_end_to_end() {
    let mut f = fixture();
    let env = f.s.ide.env_for_child();
    assert!(env.contains(&("CLAUDE_CODE_SSE_PORT".to_string(), f.s.ide.port().unwrap().to_string())));
    assert!(env.contains(&("ENABLE_IDE_INTEGRATION".to_string(), "true".to_string())));
    assert!(env.contains(&("FORCE_CODE_TERMINAL".to_string(), "true".to_string())));
    assert!(env.iter().any(|(k, v)| k == "NO_PROXY" && v.contains("127.0.0.1")));

    // A wrong token is refused at the WebSocket upgrade.
    match connect(f.s.ide.port().unwrap(), "not-the-token") {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 401),
        other => panic!("expected 401, got {:?}", other.map(|_| ())),
    }

    let mut claude = connect_claude(&mut f);
    let s = &mut f.s;

    let id = claude.send_request("tools/list", json!({}));
    let list = claude.wait_response(s, id);
    let names: Vec<String> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for want in [
        "openFile",
        "openDiff",
        "getCurrentSelection",
        "getLatestSelection",
        "getOpenEditors",
        "getWorkspaceFolders",
        "getDiagnostics",
        "checkDocumentDirty",
        "saveDocument",
        "close_tab",
        "closeAllDiffTabs",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} missing");
    }

    // No editor yet.
    let sel = claude.call_json(s, "getCurrentSelection", json!({}));
    assert_eq!(sel["success"], false);

    // openFile with a text range selects it.
    let r = claude.call(s, "openFile", json!({ "filePath": f.main_rs, "startText": "println", "endText": ";" }));
    assert!(texts(&r)[0].starts_with("Opened file:"));
    let sel = claude.call_json(s, "getCurrentSelection", json!({}));
    assert_eq!(sel["success"], true);
    assert_eq!(sel["text"], "println!(\"hi\");");
    assert_eq!(sel["filePath"], f.main_rs.display().to_string());
    assert!(sel["fileUrl"].as_str().unwrap().starts_with("file://"));
    assert_eq!(sel["selection"]["start"], json!({ "line": 1, "character": 4 }));
    assert_eq!(sel["selection"]["isEmpty"], false);

    // openFile with a line range (1-based, inclusive).
    claude.call(s, "openFile", json!({ "filePath": f.main_rs, "startLine": 1, "endLine": 1 }));
    let sel = claude.call_json(s, "getLatestSelection", json!({}));
    assert_eq!(sel["text"], "fn main() {");

    let editors = claude.call_json(s, "getOpenEditors", json!({}));
    let tabs = editors["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["label"], "main.rs");
    assert_eq!(tabs[0]["isActive"], true);
    assert_eq!(tabs[0]["isDirty"], false);
    assert_eq!(tabs[0]["languageId"], "rust");

    let folders = claude.call_json(s, "getWorkspaceFolders", json!({}));
    assert_eq!(folders["folders"][0]["path"], f.root.display().to_string());

    // Moving the caret pushes a debounced selection_changed.
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 2 })).unwrap();
    let start = Instant::now();
    let note = loop {
        s.pump();
        if let Some(m) = claude.poll()
            && m["method"] == "selection_changed"
            && m["params"]["text"] == "fn"
        {
            break m;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no selection_changed");
    };
    assert_eq!(note["params"]["selection"]["end"], json!({ "line": 0, "character": 2 }));
    assert!(note.get("id").is_none());

    // Diagnostics in LSP shape.
    s.ws.set_lsp_problems(
        f.main_rs.clone(),
        vec![Problem {
            path: f.main_rs.clone(),
            start: Position::new(1, 4),
            end: Position::new(1, 11),
            severity: Severity::Warning,
            message: "unused thing".into(),
            code: Some("W1".into()),
            source: ProblemSource::RustAnalyzer,
        }],
    );
    let all = claude.call_json(s, "getDiagnostics", json!({}));
    assert_eq!(all.as_array().unwrap().len(), 1);
    let uri = all[0]["uri"].as_str().unwrap().to_string();
    let one = claude.call_json(s, "getDiagnostics", json!({ "uri": uri }));
    let d = &one[0]["diagnostics"][0];
    assert_eq!(d["message"], "unused thing");
    assert_eq!(d["severity"], "Warning");
    assert_eq!(d["range"]["start"], json!({ "line": 1, "character": 4 }));
    assert_eq!(d["code"], "W1");
    assert_eq!(one[0]["uri"], uri, "the asked-for URI is echoed");
    // A file without problems still gets an entry with the URI exactly as asked.
    let other = format!("{uri}.nothing.rs");
    let none = claude.call_json(s, "getDiagnostics", json!({ "uri": other }));
    assert_eq!(none, json!([{ "uri": other, "diagnostics": [] }]));

    // openDiff → Accept: FILE_SAVED with the content; the open document and the file change,
    // as one undoable step.
    let proposed = "fn main() {\n    println!(\"hello from claude\");\n}\n";
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": f.main_rs, "new_file_path": f.main_rs,
            "new_file_contents": proposed, "tab_name": "✻ [Claude Code] main.rs ⧉" } }),
    );
    decide_pending(s, Decision::Allow, &f.main_rs, proposed);
    let r = claude.wait_response(s, id);
    assert_eq!(texts(&r["result"]), vec!["FILE_SAVED".to_string(), proposed.to_string()]);
    assert_eq!(std::fs::read_to_string(&f.main_rs).unwrap(), proposed);
    let dirty = claude.call_json(s, "checkDocumentDirty", json!({ "filePath": f.main_rs }));
    assert_eq!(
        dirty,
        json!({ "success": true, "filePath": f.main_rs.display().to_string(), "isDirty": false, "isUntitled": false })
    );
    let (_, od) = s.ws.active().unwrap();
    assert_eq!(od.doc.text(), proposed);
    assert!(od.doc.can_undo());

    // openDiff → Reject: DIFF_REJECTED with the tab name; nothing changes.
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": f.main_rs, "new_file_path": f.main_rs,
            "new_file_contents": "garbage", "tab_name": "tab-2" } }),
    );
    decide_pending(s, Decision::Deny, &f.main_rs, "garbage");
    let r = claude.wait_response(s, id);
    assert_eq!(texts(&r["result"]), vec!["DIFF_REJECTED".to_string(), "tab-2".to_string()]);
    assert_eq!(std::fs::read_to_string(&f.main_rs).unwrap(), proposed);

    // A new file that is not open is created on disk when accepted.
    let new_file = f.root.join("src/lib.rs");
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": new_file, "new_file_path": new_file,
            "new_file_contents": "pub fn f() {}\n", "tab_name": "tab-3" } }),
    );
    decide_pending(s, Decision::Allow, &new_file, "pub fn f() {}\n");
    let r = claude.wait_response(s, id);
    assert_eq!(texts(&r["result"])[0], "FILE_SAVED");
    assert_eq!(std::fs::read_to_string(&new_file).unwrap(), "pub fn f() {}\n");

    // Claude closing the diff tab (the user answered in the terminal) removes the card.
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": f.main_rs, "new_file_path": f.main_rs,
            "new_file_contents": "x", "tab_name": "tab-4" } }),
    );
    pump_until(s, "card", |s| !s.approvals.is_empty());
    let closed = claude.call(s, "close_tab", json!({ "tab_name": "tab-4" }));
    assert_eq!(texts(&closed), vec!["TAB_CLOSED".to_string()]);
    let r = claude.wait_response(s, id);
    assert_eq!(texts(&r["result"])[0], "DIFF_REJECTED");
    assert!(s.approvals.is_empty());

    // saveDocument writes an edited document.
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
    s.execute("editor.type", &json!({ "text": "// x\n" })).unwrap();
    let dirty = claude.call_json(s, "checkDocumentDirty", json!({ "filePath": f.main_rs }));
    assert_eq!(dirty["isDirty"], true);
    let saved = claude.call_json(s, "saveDocument", json!({ "filePath": f.main_rs }));
    assert_eq!(saved["success"], true);
    assert!(std::fs::read_to_string(&f.main_rs).unwrap().starts_with("// x\n"));
    let missing = claude.call_json(s, "checkDocumentDirty", json!({ "filePath": f.root.join("nope.rs") }));
    assert_eq!(missing["success"], false);

    // Status command.
    let st = s.execute("ide.status", &Value::Null).unwrap();
    assert_eq!(st["running"], true);
    assert_eq!(st["clients"].as_array().unwrap().len(), 1);
    assert_eq!(st["lockFile"], json!(lock_files(&f.lock_dir)[0]));

    // A diff pending when Claude disconnects is withdrawn.
    claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": f.main_rs, "new_file_path": f.main_rs,
            "new_file_contents": "y", "tab_name": "tab-5" } }),
    );
    pump_until(s, "card", |s| !s.approvals.is_empty());
    drop(claude);
    pump_until(s, "disconnect", |s| s.ide.client_count() == 0 && s.ide.pending_diffs() == 0);
    assert!(s.approvals.is_empty());

    // Closing the project stops the server and removes the lock file.
    s.execute("file.closeProject", &Value::Null).unwrap();
    pump_until(s, "server stop", |s| !s.ide.is_running());
    assert!(lock_files(&f.lock_dir).is_empty());
    assert!(s.ide.env_for_child().is_empty());
    s.shutdown();
}

#[test]
fn enable_disable_restart_and_stale_cleanup() {
    let mut f = fixture();
    let first = lock_files(&f.lock_dir);
    assert_eq!(first.len(), 1);

    // A lock file left by a CodeCraft that died is removed on (re)start.
    let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) { &["/C", "exit"][..] } else { &[][..] })
        .spawn()
        .unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let stale = lockfile::LockInfo {
        pid: dead,
        workspace_folders: vec!["/gone".into()],
        ide_name: "CodeCraft".into(),
        transport: "ws".into(),
        running_in_windows: false,
        auth_token: "old".into(),
    };
    let stale_path = lockfile::write(&f.lock_dir, 1, &stale).unwrap();
    // Another IDE's lock file is never touched.
    let vscode = lockfile::LockInfo { ide_name: "Visual Studio Code".into(), ..stale };
    let vscode_path = lockfile::write(&f.lock_dir, 2, &vscode).unwrap();

    let st = f.s.execute("ide.restart", &Value::Null).unwrap();
    assert_eq!(st["running"], true);
    assert!(!stale_path.exists());
    assert!(vscode_path.exists());
    assert!(!first[0].exists() || st["lockFile"] == json!(first[0]));

    let st = f.s.execute("ide.disable", &Value::Null).unwrap();
    assert_eq!(st["running"], false);
    assert_eq!(lock_files(&f.lock_dir), vec![vscode_path.clone()]);
    f.s.pump();
    assert!(!f.s.ide.is_running(), "disabled: the pump must not restart it");

    let st = f.s.execute("ide.enable", &Value::Null).unwrap();
    assert_eq!(st["running"], true);
    assert_eq!(lock_files(&f.lock_dir).len(), 2);

    // The approval card path still works after a restart.
    let mut claude = connect_claude_after_restart(&mut f);
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": f.main_rs, "new_file_path": f.main_rs,
            "new_file_contents": "fn main() {}\n", "tab_name": "t" } }),
    );
    decide_pending(&mut f.s, Decision::Deny, &f.main_rs, "fn main() {}\n");
    let r = claude.wait_response(&mut f.s, id);
    assert_eq!(texts(&r["result"])[0], "DIFF_REJECTED");

    // Shutdown removes our lock file.
    f.s.shutdown();
    assert_eq!(lock_files(&f.lock_dir), vec![vscode_path]);
}

fn connect_claude_after_restart(f: &mut Fixture) -> Claude {
    let path = f.s.ide.lock_path().unwrap();
    let info = lockfile::read(&path).unwrap();
    let mut claude = Claude::new(f.s.ide.port().unwrap(), &info.auth_token);
    claude.handshake(&mut f.s);
    claude
}

#[test]
fn headless_sessions_never_start_the_server_without_a_lock_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
    pump_until(&mut s, "project load", |s| !s.jobs_running());
    for _ in 0..5 {
        s.pump();
    }
    assert!(!s.ide.is_running());
    let st = s.execute("ide.restart", &Value::Null).unwrap();
    assert_eq!(st["running"], false);
    assert!(s.enabled("ide.sendSelection").is_err());
    s.shutdown();
}

/// Claude Code reports symlink-resolved paths; they must still reach the open document.
#[cfg(unix)]
#[test]
fn symlinked_project_paths_match_open_documents() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().canonicalize().unwrap().join("real");
    std::fs::create_dir_all(real.join("proj")).unwrap();
    std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
    let root = normalize_path(&tmp.path().join("link/proj"));
    let file = root.join("a.txt");
    std::fs::write(&file, "old\n").unwrap();
    let mut f = {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.ide.set_lock_dir(Some(tmp.path().join("locks")));
        s.execute("file.open", &json!({ "path": root })).unwrap();
        pump_until(&mut s, "project load", |s| !s.jobs_running());
        pump_until(&mut s, "server start", |s| s.ide.is_running());
        s.execute("file.open", &json!({ "path": file })).unwrap();
        s.execute("editor.type", &json!({ "text": "unsaved " })).unwrap();
        Fixture { _tmp: tmp, root: root.clone(), lock_dir: Default::default(), main_rs: file.clone(), s }
    };
    f.lock_dir = f.s.ide.lock_path().unwrap().parent().unwrap().to_path_buf();
    let info = lockfile::read(&f.s.ide.lock_path().unwrap()).unwrap();
    let real_root = real.join("proj");
    assert_eq!(info.workspace_folders, vec![root.display().to_string(), real_root.display().to_string()]);

    let mut claude = Claude::new(f.s.ide.port().unwrap(), &info.auth_token);
    claude.handshake(&mut f.s);
    let real_file = real_root.join("a.txt");
    let dirty = claude.call_json(&mut f.s, "checkDocumentDirty", json!({ "filePath": real_file }));
    assert_eq!(dirty["isDirty"], true);
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": real_file, "new_file_path": real_file,
            "new_file_contents": "new\n", "tab_name": "t" } }),
    );
    // The diff's original is the open document (with the unsaved edit), not the disk file.
    pump_until(&mut f.s, "card", |s| !s.approvals.is_empty());
    let req = f.s.approvals.pending().next().unwrap().clone();
    let ApprovalKind::FileEdit { path, original, .. } = &req.kind else { panic!() };
    assert_eq!(path, &file);
    assert_eq!(original, "unsaved old\n");
    f.s.approvals.decide(req.id, Decision::AllowAlways);
    let r = claude.wait_response(&mut f.s, id);
    assert_eq!(texts(&r["result"])[0], "FILE_SAVED");
    let (_, od) = f.s.ws.active().unwrap();
    assert_eq!(od.doc.text(), "new\n");
    assert!(!od.doc.is_modified());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");

    // "Allow always" for Claude Code: the next diff applies without a card.
    let id = claude.send_request(
        "tools/call",
        json!({ "name": "openDiff", "arguments": {
            "old_file_path": real_file, "new_file_path": real_file,
            "new_file_contents": "newer\n", "tab_name": "t2" } }),
    );
    let r = claude.wait_response(&mut f.s, id);
    assert_eq!(texts(&r["result"])[0], "FILE_SAVED");
    assert!(f.s.approvals.is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "newer\n");
    f.s.shutdown();
}
