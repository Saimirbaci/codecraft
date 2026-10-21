//! The agent control plane (M14 track AX) end to end: engine tools on temp projects, the access
//! policy (allow, ask with approval cards, deny, headless), the IDE server advertising the tools
//! next to Claude Code's, the stdio bridge an agent job gets, and real debug sessions through the
//! tools (debugpy, lldb-dap; skipped when missing). Never runs a real agent CLI; never touches
//! the user's ~/.codecraft, ~/.claude or Keychain.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_access::{AgentAccess, Capability};
use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};
use codecraft_engine::approvals::{ApprovalKind, Decision};
use codecraft_engine::mcp::ToolResult;

const LONG: Duration = Duration::from_secs(300);

fn session(home: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s.db.set_secret_store(Arc::new(codecraft_engine::db::MemoryStore::default()));
    s
}

fn open(s: &mut Session, dir: &Path) {
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(s, "the project scan", |s| !s.jobs_running());
}

fn pump_until(s: &mut Session, what: &str, mut cond: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        assert!(start.elapsed() < LONG, "timed out waiting for {what}");
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn call(s: &mut Session, name: &str, args: Value) -> ToolResult {
    agent_tools::call_blocking(s, Caller::new("test agent"), name, args, LONG)
}

/// A successful call's JSON.
fn ok(s: &mut Session, name: &str, args: Value) -> Value {
    let r = call(s, name, args.clone());
    assert!(!r.is_error, "{name} {args} failed: {}", result_text(&r));
    result_json(&r)
}

/// A failing call's message.
fn err(s: &mut Session, name: &str, args: Value) -> String {
    let r = call(s, name, args.clone());
    assert!(r.is_error, "{name} {args} should fail, got {}", result_text(&r));
    result_text(&r).to_string()
}

fn project(tmp: &Path) -> PathBuf {
    let dir = tmp.join("proj");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    dir.canonicalize().unwrap()
}

// ---------------------------------------------------------------- read tools and policy

#[test]
fn read_tools_on_a_temp_project() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);

    let info = ok(&mut s, "project_info", json!({}));
    assert_eq!(info["project"]["root"], json!(dir));
    assert_eq!(info["debugger"], "none");

    let read = ok(&mut s, "file_read", json!({ "path": "notes.txt", "startLine": 2, "endLine": 3 }));
    assert_eq!(read["content"], "2\tbeta\n3\tgamma");
    assert_eq!(read["path"], json!(dir.join("notes.txt")));
    // Unsaved editor text is what agents read.
    s.execute("file.open", &json!({ "path": dir.join("notes.txt") })).unwrap();
    s.execute("editor.type", &json!({ "text": "NEW " })).unwrap();
    let read = ok(&mut s, "file_read", json!({ "path": dir.join("notes.txt") }));
    assert_eq!(read["unsaved"], true);
    assert!(read["content"].as_str().unwrap().starts_with("1\tNEW alpha"));

    let problems = ok(&mut s, "problems", json!({ "path": "src" }));
    assert_eq!(problems["total"], 0);

    let found = ok(&mut s, "codecraft_commands", json!({ "query": "evaluate expression" }));
    let cmd =
        found["commands"].as_array().unwrap().iter().find(|c| c["id"] == "run.debuggingActions.evaluateExpression");
    let cmd = cmd.expect("Evaluate Expression is listed");
    assert_eq!(cmd["capability"], "debug");
    assert_eq!(cmd["enabled"], false);
    assert!(cmd["disabledReason"].is_string());

    let state = ok(&mut s, "codecraft_execute", json!({ "id": "debug.state" }));
    assert_eq!(state["result"]["active"], false);
    assert_eq!(ok(&mut s, "run_configurations", json!({})), json!([]));
    assert_eq!(ok(&mut s, "debug_breakpoints", json!({})), json!([]));
    let opened = ok(&mut s, "editor_open", json!({ "path": "notes.txt", "line": 3 }));
    assert_eq!(opened["line"], 3);

    // Errors an agent can act on.
    assert!(err(&mut s, "no_such_tool", json!({})).contains("unknown tool"));
    assert!(err(&mut s, "file_read", json!([1])).contains("JSON object"));
    assert!(err(&mut s, "file_read", json!({ "path": "missing.txt" })).contains("missing.txt"));
    assert!(err(&mut s, "codecraft_execute", json!({ "id": "agentAccess.set", "params": {} })).contains("may not"));
    assert!(err(&mut s, "codecraft_execute", json!({ "id": "agent.start" })).contains("may not"));
    assert!(err(&mut s, "codecraft_execute", json!({ "id": "nope.nope" })).contains("unknown command"));
    // (Debug calls ask in the IDE; this agent is allowed to debug.)
    s.agent_access = AgentAccess::headless(&[Capability::Debug, Capability::Profile]);
    let none = ok(&mut s, "debug_wait_for_stop", json!({ "timeoutMs": 10 }));
    assert_eq!(none["state"], "none");
    assert!(err(&mut s, "debug_evaluate", json!({ "expression": "1" })).contains("not paused"));
    assert!(err(&mut s, "profile_hotspots", json!({ "timeoutMs": 10 })).contains("snapshot"));
    assert!(err(&mut s, "test_results", json!({})).contains("test run"));
    assert!(err(&mut s, "definition", json!({ "path": "notes.txt", "line": 1 })).contains("language servers are off"));
    s.shutdown();
}

/// Submit a call and leave it parked; returns the slot its answer lands in.
fn submit(s: &mut Session, caller: &str, name: &str, args: Value) -> Arc<std::sync::Mutex<Option<ToolResult>>> {
    let slot = Arc::new(std::sync::Mutex::new(None));
    let out = Arc::clone(&slot);
    agent_tools::submit(s, Caller::new(caller), name, args, Box::new(move |r| *out.lock().unwrap() = Some(r)));
    slot
}

fn wait_answer(s: &mut Session, slot: &Arc<std::sync::Mutex<Option<ToolResult>>>) -> ToolResult {
    pump_until(s, "the answer", |_| slot.lock().unwrap().is_some());
    slot.lock().unwrap().take().unwrap()
}

fn only_card(s: &mut Session) -> codecraft_engine::approvals::ApprovalId {
    pump_until(s, "the approval card", |s| s.approvals.len() == 1);
    let req = s.approvals.pending().next().unwrap().clone();
    match &req.kind {
        ApprovalKind::ToolUse { tool, input } => {
            assert_eq!(tool, "file_write");
            assert_eq!(input["path"], "out.txt");
        }
        other => panic!("{other:?}"),
    }
    assert!(req.source.starts_with("Claude Code") && req.source.ends_with("write"), "{}", req.source);
    req.id
}

#[test]
fn policy_allows_asks_denies_and_remembers() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    let write = json!({ "path": "out.txt", "content": "one" });

    // Ask (the default for writes): an approval card; Deny answers with an error.
    let slot = submit(&mut s, "Claude Code", "file_write", write.clone());
    assert_eq!(s.agent_tools.pending(), 1);
    let id = only_card(&mut s);
    assert!(s.approvals.decide(id, Decision::Deny));
    let r = wait_answer(&mut s, &slot);
    assert!(r.is_error && result_text(&r).contains("denied by the user"));
    assert!(!dir.join("out.txt").exists());

    // Allow once: the write happens, and the next call asks again.
    let slot = submit(&mut s, "Claude Code", "file_write", write.clone());
    let id = only_card(&mut s);
    s.approvals.decide(id, Decision::Allow);
    let r = wait_answer(&mut s, &slot);
    assert!(!r.is_error, "{}", result_text(&r));
    assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "one");
    let slot = submit(&mut s, "Claude Code", "file_write", json!({ "path": "out.txt", "content": "two" }));
    let id = only_card(&mut s);
    // Always for this session: later writes from this caller go through without a card.
    s.approvals.decide(id, Decision::AllowAlways);
    assert!(!wait_answer(&mut s, &slot).is_error);
    let r = agent_tools::call_blocking(
        &mut s,
        Caller::new("Claude Code"),
        "file_write",
        json!({ "path": "out.txt", "content": "three" }),
        LONG,
    );
    assert!(!r.is_error);
    assert!(s.approvals.is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "three");
    let policy = s.execute("agentAccess.get", &Value::Null).unwrap();
    assert_eq!(policy["sessionApprovals"][0], json!({ "caller": "Claude Code", "capability": "write" }));
    // Forgetting the session approvals asks again.
    s.execute("agentAccess.clearSession", &Value::Null).unwrap();
    let slot = submit(&mut s, "Claude Code", "file_write", json!({ "path": "out.txt", "content": "three" }));
    let id = only_card(&mut s);
    s.approvals.decide(id, Decision::Allow);
    assert!(!wait_answer(&mut s, &slot).is_error);
    // …and "always" is never shared with another caller.
    let slot = submit(&mut s, "Codex", "file_write", write.clone());
    pump_until(&mut s, "a card for Codex", |s| s.approvals.len() == 1);
    let id = s.approvals.pending().next().unwrap().id;
    s.approvals.decide(id, Decision::Deny);
    assert!(wait_answer(&mut s, &slot).is_error);

    // Nobody answers: denied after the timeout, and the card goes away.
    s.agent_tools.approval_timeout = Duration::from_millis(50);
    let slot = submit(&mut s, "Codex", "file_write", write.clone());
    let r = wait_answer(&mut s, &slot);
    assert!(result_text(&r).contains("nobody answered"), "{}", result_text(&r));
    assert!(s.approvals.is_empty());

    // Off: denied without asking.
    s.execute("agentAccess.set", &json!({ "capability": "write", "level": "off" })).unwrap();
    assert!(err(&mut s, "file_edit", json!({ "path": "out.txt", "oldText": "three", "newText": "x" })).contains("off"));
    // Allow: no card.
    s.execute("agentAccess.set", &json!({ "capability": "write", "level": "allow" })).unwrap();
    let edited = ok(&mut s, "file_edit", json!({ "path": "out.txt", "oldText": "three", "newText": "four" }));
    assert_eq!(edited["replacements"], 1);
    // Reads never ask.
    ok(&mut s, "file_read", json!({ "path": "out.txt" }));
    assert!(tmp.path().join("home/agent-access.json").is_file(), "the policy is saved under the isolated home");

    // Headless: Ask is denied with how to allow it.
    s.agent_access = AgentAccess::headless(&[Capability::Debug]);
    let msg = err(&mut s, "file_write", write.clone());
    assert!(msg.contains("--allow write"), "{msg}");
    ok(&mut s, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 2 }));
    s.shutdown();
}

#[test]
fn breakpoints_through_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.agent_access = AgentAccess::headless(&[Capability::Debug]);
    let bp = ok(
        &mut s,
        "debug_breakpoint_set",
        json!({ "path": "notes.txt", "line": 2, "condition": "x > 1", "hitCount": "3", "temporary": true }),
    );
    assert_eq!(bp["line"], 2);
    assert_eq!(bp["condition"], "x > 1");
    assert_eq!(bp["hitCount"], "3");
    assert_eq!(bp["temporary"], true);
    ok(&mut s, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 3, "logMessage": "at {x}" }));
    let list = ok(&mut s, "debug_breakpoints", json!({}));
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[1]["logMessage"], "at {x}");
    let id = list[0]["id"].as_u64().unwrap();
    ok(&mut s, "debug_breakpoint_remove", json!({ "id": id }));
    ok(&mut s, "debug_breakpoint_remove", json!({ "path": "notes.txt", "line": 3 }));
    assert_eq!(ok(&mut s, "debug_breakpoints", json!({})), json!([]));
    assert!(err(&mut s, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 0 })).contains("1-based"));
    assert!(err(&mut s, "debug_breakpoint_remove", json!({})).contains("path"));
    assert!(err(&mut s, "debug_control", json!({ "action": "dance" })).contains("unknown action"));
    assert!(err(&mut s, "debug_control", json!({ "action": "stepOver" })).contains("not paused"));
    s.shutdown();
}

// ---------------------------------------------------------------- databases

fn make_db(path: &Path) {
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(path.to_path_buf()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in [
        "create table items (id integer primary key, name text not null)",
        "insert into items(name) values ('apple'), ('pear'), ('plum')",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
}

#[test]
fn database_queries_respect_db_write() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    make_db(&dir.join("shop.db"));
    let mut s = session(&tmp.path().join("home"));
    s.db.history.set_dir(tmp.path().join("home/db-history"));
    open(&mut s, &dir);
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Shop", "path": "shop.db" })).unwrap();
    s.agent_access = AgentAccess::headless(&[]);

    let sources = ok(&mut s, "db_sources", json!({}));
    assert_eq!(sources[0]["name"], "Shop");
    let rows = ok(&mut s, "db_query", json!({ "dataSource": "Shop", "sql": "select name from items order by id" }));
    let result = &rows["results"][0];
    assert_eq!(result["rows"].as_array().unwrap().len(), 3);
    assert_eq!(result["rows"][0][0], "apple");
    // Writes need db-write.
    let msg = err(&mut s, "db_query", json!({ "dataSource": "Shop", "sql": "delete from items where id = 1" }));
    assert!(msg.contains("db-write"), "{msg}");
    // Through codecraft_execute too.
    let msg = err(
        &mut s,
        "codecraft_execute",
        json!({ "id": "db.execute", "params": { "dataSource": "Shop", "sql": "drop table items" } }),
    );
    assert!(msg.contains("db-write"), "{msg}");
    s.agent_access = AgentAccess::headless(&[Capability::DbWrite]);
    let done = ok(&mut s, "db_query", json!({ "dataSource": "Shop", "sql": "delete from items where id = 1" }));
    assert!(done["output"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains('1')), "{done}");
    let rows = ok(&mut s, "db_query", json!({ "dataSource": "Shop", "sql": "select count(*) from items" }));
    assert_eq!(rows["results"][0]["rows"][0][0], 2);
    // A data source that confirms writes is never written headless.
    let id = sources[0]["id"].as_str().unwrap().to_string();
    s.execute("db.editDataSource", &json!({ "dataSource": id, "confirmWrites": true })).unwrap();
    let msg = err(&mut s, "db_query", json!({ "dataSource": id, "sql": "delete from items" }));
    assert!(msg.contains("confirm"), "{msg}");
    assert!(s.db.confirm.is_none());
    let rows = ok(&mut s, "db_query", json!({ "dataSource": "Shop", "sql": "select count(*) from items" }));
    assert_eq!(rows["results"][0]["rows"][0][0], 2);
    assert!(err(&mut s, "db_query", json!({ "dataSource": "Nope", "sql": "select 1" })).contains("Shop"));
    s.shutdown();

    // A fresh session whose data sources are not loaded yet still sees the write.
    let mut fresh = session(&tmp.path().join("home2"));
    fresh.db.history.set_dir(tmp.path().join("home2/db-history"));
    open(&mut fresh, &dir);
    fresh.agent_access = AgentAccess::headless(&[]);
    let msg = err(&mut fresh, "db_query", json!({ "dataSource": "Shop", "sql": "delete from items" }));
    assert!(msg.contains("db-write"), "{msg}");
    fresh.shutdown();
}

// ---------------------------------------------------------------- the IDE server and the bridge

mod ws {
    use std::net::TcpStream;
    use std::time::Duration;

    use codecraft_engine::mcp::tungstenite;
    use codecraft_engine::mcp::{AUTH_HEADER, tungstenite::client::IntoClientRequest};
    use serde_json::Value;
    use tungstenite::{Message, WebSocket};

    pub struct Client(pub WebSocket<TcpStream>);

    impl Client {
        pub fn connect(port: u16, token: &str) -> Self {
            let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut req = format!("ws://127.0.0.1:{port}").into_client_request().unwrap();
            req.headers_mut().insert(AUTH_HEADER, token.parse().unwrap());
            let (ws, _) = tungstenite::client(req, stream).unwrap();
            ws.get_ref().set_read_timeout(Some(Duration::from_millis(5))).unwrap();
            Self(ws)
        }

        pub fn send(&mut self, v: &Value) {
            self.0.send(Message::text(v.to_string())).unwrap();
        }

        pub fn poll(&mut self) -> Option<Value> {
            match self.0.read() {
                Ok(Message::Text(t)) => Some(serde_json::from_str(t.as_str()).unwrap()),
                Ok(_) => None,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    None
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
}

fn wait_ws(s: &mut Session, c: &mut ws::Client, id: u64) -> Value {
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(30), "no response {id}");
        s.pump();
        if let Some(m) = c.poll()
            && m["id"] == id
        {
            return m;
        }
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Lines from a reader, on a thread.
fn line_channel<R: std::io::Read + Send + 'static>(r: R) -> crossbeam_channel::Receiver<Value> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    rx
}

fn wait_line(s: &mut Session, rx: &crossbeam_channel::Receiver<Value>, id: u64) -> Value {
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(30), "no bridged response {id}");
        s.pump();
        while let Ok(v) = rx.try_recv() {
            if v["id"] == id {
                return v;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
#[test]
fn ide_server_serves_engine_tools_and_agent_jobs_get_the_bridge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let locks = tmp.path().join("locks");
    let mut s = session(&tmp.path().join("home"));
    s.ide.set_lock_dir(Some(locks.clone()));
    open(&mut s, &dir);
    pump_until(&mut s, "the IDE server", |s| s.ide.port().is_some());
    let port = s.ide.port().unwrap();
    let lock = codecraft_engine::mcp::lockfile::read(&s.ide.lock_path().unwrap()).unwrap();

    // Claude Code over /ide: the IDE tools and the engine tools.
    let mut c = ws::Client::connect(port, &lock.auth_token);
    c.send(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "claude-code" } } }));
    wait_ws(&mut s, &mut c, 1);
    c.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = wait_ws(&mut s, &mut c, 2);
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in ["openDiff", "getDiagnostics", "debug_wait_for_stop", "codecraft_execute", "test_run", "db_query"] {
        assert!(names.contains(&want), "{want} missing");
    }
    // The IDE tools come first and are unchanged.
    let ide: Vec<String> = codecraft_engine::mcp::tools::claude_code_tools().into_iter().map(|t| t.name).collect();
    assert_eq!(names[..ide.len()].to_vec(), ide.iter().map(String::as_str).collect::<Vec<_>>());
    c.send(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "file_read", "arguments": { "path": "notes.txt" } } }));
    let r = wait_ws(&mut s, &mut c, 3);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("alpha"), "{r}");
    // A debug call from Claude Code asks first (card source names the client).
    c.send(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "debug_breakpoint_set", "arguments": { "path": "notes.txt", "line": 1 } } }));
    pump_until(&mut s, "the card", |s| s.approvals.len() == 1);
    let card = s.approvals.pending().next().unwrap().clone();
    assert_eq!(card.source, "claude-code · debug");
    s.approvals.decide(card.id, Decision::Deny);
    let r = wait_ws(&mut s, &mut c, 4);
    assert_eq!(r["result"]["isError"], true);

    // A built-in agent job (fake claude, Auto mode) is given the bridge to this server.
    let args_log = tmp.path().join("args.log");
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" >> '{}'\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"session_id\":\"s1\"}}'\n",
            args_log.display()
        ),
    );
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let job = s.execute("agent.start", &json!({ "backend": "claude", "mode": "auto", "prompt": "debug it" })).unwrap();
    let job = job["job"].as_u64().unwrap();
    pump_until(&mut s, "the fake job", |s| {
        s.agents.job(codecraft_engine::agents::JobId(job)).is_some_and(|j| !j.is_running())
    });
    let logged = std::fs::read_to_string(&args_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    let i = lines.iter().position(|l| *l == "--mcp-config").expect("--mcp-config was passed");
    let cfg: Value = serde_json::from_str(lines[i + 1]).unwrap();
    let server = &cfg["mcpServers"]["codecraft"];
    let bargs: Vec<String> =
        server["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
    assert_eq!(bargs[0], "--mcp-bridge");
    assert!(bargs.windows(2).any(|w| w[0] == "--port" && w[1] == port.to_string()), "{bargs:?}");
    assert!(bargs.windows(2).any(|w| w[0] == "--job" && w[1] == job.to_string()), "{bargs:?}");
    let parsed = codecraft_engine::agent_mcp::parse_bridge_args(&bargs).unwrap();
    assert_eq!(parsed.lock_dir.as_deref(), Some(locks.as_path()));
    // The job's key travels in the server's environment (M15 AI), not on its command line.
    let key = server["env"]["CODECRAFT_JOB_KEY"].as_str().expect("the job key").to_string();
    assert!(!bargs.iter().any(|a| a.contains(&key)));

    // Run that bridge in-process: the job's calls arrive tagged; Auto mode needs no card.
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let mut tag = serde_json::Map::new();
    tag.insert("codecraftJob".into(), json!(job));
    tag.insert("codecraftJobKey".into(), json!(key));
    let cfg = codecraft_engine::mcp::bridge::BridgeConfig { port, token: lock.auth_token.clone(), client_tag: tag };
    let bridge = std::thread::spawn(move || codecraft_engine::mcp::bridge::run(&cfg, BufReader::new(in_r), out_w));
    let rx = line_channel(out_r);
    let mut send = |v: Value| {
        writeln!(in_w, "{v}").unwrap();
        in_w.flush().unwrap();
    };
    send(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "claude-code" } } }),
    );
    wait_line(&mut s, &rx, 1);
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    send(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "debug_breakpoint_set", "arguments": { "path": "notes.txt", "line": 2 } } }),
    );
    let r = wait_line(&mut s, &rx, 2);
    assert!(r["result"].get("isError").is_none(), "{r}");
    assert!(s.approvals.is_empty(), "an Auto job is not asked");
    assert_eq!(s.debug.breakpoints.len(), 1);
    drop(send);
    drop(in_w);
    let start = Instant::now();
    while !bridge.is_finished() && start.elapsed() < Duration::from_secs(10) {
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(bridge.join().unwrap().is_ok());
    s.shutdown();
}

#[test]
fn stdio_server_in_process() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.agent_access = AgentAccess::headless(&[]);
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let rx = line_channel(out_r);
    let client = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut send = |v: Value| {
            writeln!(in_w, "{v}").unwrap();
            in_w.flush().unwrap();
        };
        send(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "t" } } }),
        );
        send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        send(
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "file_read", "arguments": { "path": "notes.txt" } } }),
        );
        send(
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "file_write", "arguments": { "path": "notes.txt", "content": "x" } } }),
        );
        while got.len() < 4 {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(v) => got.push(v),
                Err(_) => break,
            }
        }
        drop(send);
        drop(in_w);
        got
    });
    codecraft_engine::agent_mcp::serve_stdio(&mut s, BufReader::new(in_r), out_w).unwrap();
    let got = client.join().unwrap();
    let by_id = |id: u64| got.iter().find(|v| v["id"] == id).cloned().unwrap_or(Value::Null);
    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "CodeCraft");
    let tools = by_id(2)["result"]["tools"].as_array().cloned().unwrap_or_default();
    assert!(tools.iter().any(|t| t["name"] == "debug_start"));
    assert!(!tools.iter().any(|t| t["name"] == "openDiff"), "the headless server has no IDE tools");
    assert!(by_id(3)["result"]["content"][0]["text"].as_str().unwrap().contains("alpha"));
    assert_eq!(by_id(4)["result"]["isError"], true);
    assert!(by_id(4)["result"]["content"][0]["text"].as_str().unwrap().contains("--allow write"));
    assert_eq!(std::fs::read_to_string(dir.join("notes.txt")).unwrap(), "alpha\nbeta\ngamma\n");
    s.shutdown();
}

// ---------------------------------------------------------------- real debuggers, tests, runs

fn cargo() -> Option<PathBuf> {
    codecraft_engine::dap::discover::which("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

#[test]
fn python_debugging_through_tools() {
    if codecraft_engine::dap::discover::find_debugpy(None).is_err() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let script = dir.join("app.py");
    std::fs::write(
        &script,
        "def work(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n\nresult = work(5)\nprint('result', result)\n",
    )
    .unwrap();
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.agent_access = AgentAccess::headless(&[Capability::Debug]);

    ok(&mut s, "debug_breakpoint_set", json!({ "path": "app.py", "line": 4, "condition": "i == 3" }));
    let started = ok(&mut s, "debug_start", json!({ "file": "app.py" }));
    assert_eq!(started["started"], true);
    let stop = ok(&mut s, "debug_wait_for_stop", json!({ "timeoutMs": 120_000 }));
    assert_eq!(stop["state"], "stopped", "{stop}");
    assert_eq!(stop["reason"], "breakpoint");
    assert_eq!(stop["frames"][0]["name"], "work");
    assert_eq!(stop["frames"][0]["line"], 4);
    assert_eq!(stop["frames"][0]["file"], json!(script));
    assert!(stop["frames"][0]["excerpt"].as_str().unwrap().contains(">"), "{stop}");
    assert_eq!(stop["frames"][1]["line"], 7);
    let local = |stop: &Value, name: &str| {
        stop["locals"].as_array().unwrap().iter().find(|v| v["name"] == name).map(|v| v["value"].clone())
    };
    assert_eq!(local(&stop, "i"), Some(json!("3")), "the condition held: {stop}");
    assert_eq!(local(&stop, "total"), Some(json!("3")));

    let ev = ok(&mut s, "debug_evaluate", json!({ "expression": "total * 10" }));
    assert_eq!(ev["value"], "30");
    assert!(err(&mut s, "debug_evaluate", json!({ "expression": "nope_nope" })).contains("nope_nope"));
    let vars = ok(&mut s, "debug_variables", json!({}));
    assert!(vars["scopes"][0]["variables"].as_array().unwrap().iter().any(|v| v["name"] == "total"));
    let stack = ok(&mut s, "debug_stack", json!({}));
    assert!(stack["frames"].as_array().unwrap().len() >= 2);
    // The caller's frame.
    let caller = ok(&mut s, "debug_variables", json!({ "frame": 1 }));
    assert_eq!(caller["frame"], 1);
    // Back in `work`'s frame for the evaluation.
    let ev = ok(&mut s, "debug_evaluate", json!({ "expression": "n", "frame": 0 }));
    assert_eq!(ev["value"], "5");

    let step = ok(&mut s, "debug_control", json!({ "action": "stepOver", "timeoutMs": 60_000 }));
    assert_eq!(step["state"], "stopped");
    assert!(matches!(step["frames"][0]["line"].as_u64(), Some(3) | Some(4)), "{step}");
    ok(&mut s, "debug_breakpoint_remove", json!({ "all": true }));
    let end = ok(&mut s, "debug_control", json!({ "action": "continue", "timeoutMs": 60_000 }));
    assert_eq!(end["state"], "exited", "{end}");
    assert_eq!(end["exitCode"], 0);
    assert!(end["output"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("result 10")), "{end}");
    s.shutdown();
}

#[test]
fn rust_debugging_tests_and_runs_through_tools() {
    let Some(_) = cargo() else {
        eprintln!("skipping: cargo is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"axdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(2, 2), 4);\n    }\n\n    #[test]\n    fn breaks() {\n        assert_eq!(add(1, 1), 3, \"one and one\");\n    }\n}\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(
        &main,
        "fn main() {\n    let numbers = vec![1, 2, 3];\n    let total: i32 = numbers.iter().sum();\n    let label = String::from(\"sum\");\n    println!(\"{label} {}\", axdemo::add(total, 0));\n}\n",
    )
    .unwrap();
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.agent_access = AgentAccess::headless(&[Capability::Run, Capability::Debug]);

    // Tests: everything, then a filter.
    let all = ok(&mut s, "test_run", json!({}));
    assert_eq!(all["finished"], true, "{all}");
    assert_eq!(all["counts"]["passed"], 1, "{all}");
    assert_eq!(all["counts"]["failed"], 1, "{all}");
    let failure = &all["failures"][0];
    assert!(failure["name"].as_str().unwrap().contains("breaks"), "{failure}");
    assert!(failure["message"].as_str().unwrap().contains("one and one"), "{failure}");
    assert_eq!(failure["file"], json!(dir.join("src/lib.rs")));
    assert_eq!(failure["line"], 16);
    let only = ok(&mut s, "test_run", json!({ "name": "adds" }));
    assert_eq!(only["counts"]["failed"], 0, "{only}");
    assert_eq!(only["counts"]["passed"], 1, "{only}");
    let again = ok(&mut s, "test_results", json!({ "includePassed": true }));
    assert_eq!(again["tests"].as_array().unwrap().len(), 1);

    // Runs: a configuration by name, its output when it ends.
    let configs = ok(&mut s, "run_configurations", json!({}));
    let run_cfg = configs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["commandLine"].as_str().unwrap().contains("run"))
        .cloned()
        .unwrap_or_else(|| panic!("no run configuration: {configs}"));
    let started = ok(&mut s, "run_start", json!({ "config": run_cfg["name"] }));
    let tab = started["tab"].as_u64().unwrap();
    let out = ok(&mut s, "run_output", json!({ "tab": tab, "waitMs": 240_000 }));
    assert_eq!(out["status"], "succeeded", "{out}");
    assert!(out["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("sum 6")), "{out}");
    assert!(err(&mut s, "run_start", json!({ "config": "nope" })).contains("no run configuration"));

    // Debugging with lldb-dap.
    if codecraft_engine::dap::discover::find_lldb_dap().is_err() {
        eprintln!("skipping the debugger part: lldb-dap is missing");
        s.shutdown();
        return;
    }
    ok(&mut s, "debug_breakpoint_set", json!({ "path": main, "line": 4 }));
    ok(&mut s, "debug_start", json!({ "config": run_cfg["name"] }));
    let stop = ok(&mut s, "debug_wait_for_stop", json!({ "timeoutMs": 240_000 }));
    assert_eq!(stop["state"], "stopped", "{stop}");
    assert_eq!(stop["frames"][0]["line"], 4, "{stop}");
    let total = stop["locals"].as_array().unwrap().iter().find(|v| v["name"] == "total").unwrap();
    assert_eq!(total["value"], "6");
    let numbers = stop["locals"].as_array().unwrap().iter().find(|v| v["name"] == "numbers").unwrap().clone();
    let r = numbers["ref"].as_i64().unwrap();
    let elems = ok(&mut s, "debug_variables", json!({ "ref": r }));
    assert_eq!(elems["variables"].as_array().unwrap().len(), 3, "{elems}");
    let ev = ok(&mut s, "debug_evaluate", json!({ "expression": "total" }));
    assert_eq!(ev["value"], "6");
    let end = ok(&mut s, "debug_control", json!({ "action": "continue", "timeoutMs": 60_000 }));
    assert_eq!(end["state"], "exited", "{end}");
    s.shutdown();
}

// ---------------------------------------------------------------- analysis with rust-analyzer

/// Retry a language-server tool while the server indexes (empty answers at first).
fn ok_eventually(s: &mut Session, name: &str, args: Value, good: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        let r = call(s, name, args.clone());
        let v = result_json(&r);
        if !r.is_error && good(&v) {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(180), "{name} {args}: {}", result_text(&r));
        for _ in 0..50 {
            s.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn analysis_through_rust_analyzer() {
    if codecraft_lsp::find_rust_analyzer().is_none() || cargo().is_none() {
        eprintln!("skipping: rust-analyzer or cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"axra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(
        &main,
        "fn main() {\n    let unused = 5;\n    let text = greet(\"x\");\n    println!(\"{text}\");\n}\n\n/// Says hello.\nfn greet(name: &str) -> String {\n    format!(\"{name}!\")\n}\n",
    )
    .unwrap();
    let mut s = Session::new();
    s.isolate_home(&tmp.path().join("home"));
    // Keep the IDE server's lock file out of ~/.claude.
    s.ide.set_lock_dir(Some(tmp.path().join("locks")));
    open(&mut s, &dir);
    s.agent_access = AgentAccess::headless(&[]);

    let def = ok_eventually(&mut s, "definition", json!({ "path": main, "line": 3, "symbol": "greet" }), |v| {
        v["locations"].as_array().is_some_and(|a| !a.is_empty())
    });
    assert_eq!(def["locations"][0]["file"], json!(main));
    assert_eq!(def["locations"][0]["line"], 8);
    assert!(def["locations"][0]["excerpt"].as_str().unwrap().contains("fn greet"));
    let refs = ok_eventually(&mut s, "references", json!({ "path": main, "line": 8, "symbol": "greet" }), |v| {
        v["total"].as_u64().unwrap_or(0) >= 2
    });
    assert!(refs["locations"].as_array().unwrap().iter().any(|l| l["line"] == 3));
    let hover = ok_eventually(&mut s, "hover", json!({ "path": "src/main.rs", "line": 8, "column": 4 }), |v| {
        v["text"].is_string()
    });
    assert!(hover["text"].as_str().unwrap().contains("greet"), "{hover}");
    let symbols =
        ok_eventually(&mut s, "document_symbols", json!({ "path": main }), |v| v["total"].as_u64().unwrap_or(0) >= 2);
    let names: Vec<&str> = symbols["symbols"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"main") && names.contains(&"greet"), "{names:?}");
    let ws = ok_eventually(&mut s, "workspace_symbols", json!({ "query": "greet" }), |v| {
        v["symbols"].as_array().is_some_and(|a| a.iter().any(|x| x["name"] == "greet"))
    });
    assert!(ws["symbols"].as_array().unwrap().iter().any(|x| x["file"] == json!(main)), "{ws}");
    let calls = ok_eventually(&mut s, "call_hierarchy", json!({ "path": main, "line": 8, "symbol": "greet" }), |v| {
        v["calls"].as_array().is_some_and(|a| !a.is_empty())
    });
    assert_eq!(calls["calls"][0]["name"], "main");
    assert_eq!(calls["calls"][0]["callSites"], json!([3]));
    ok_eventually(
        &mut s,
        "call_hierarchy",
        json!({ "path": main, "line": 1, "symbol": "main", "direction": "outgoing" }),
        |v| v["calls"].as_array().is_some_and(|a| a.iter().any(|c| c["name"] == "greet")),
    );
    assert!(err(&mut s, "definition", json!({ "path": main, "line": 3, "symbol": "zzz" })).contains("zzz"));
    assert!(err(&mut s, "definition", json!({ "path": main, "line": 99 })).contains("past the end"));
    // Problems the server reported (the unused variable).
    let problems = ok_eventually(&mut s, "problems", json!({ "path": main, "severity": "hint" }), |v| {
        v["total"].as_u64().unwrap_or(0) >= 1
    });
    assert!(problems["problems"].as_array().unwrap().iter().any(|p| p["message"].as_str().unwrap().contains("unused")));
    s.shutdown();
}

// ---------------------------------------------------------------- profiling (fake samply)

#[cfg(unix)]
#[test]
fn profiling_through_tools_with_a_fake_profiler() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fixture = bin.join("samply.fixture");
    std::fs::write(&fixture, "main;compute;inner 30\nmain;compute 10\nmain;io 5\n").unwrap();
    // A profiler that writes collapsed stacks to the path after `-o`.
    script(
        &bin,
        "samply",
        &format!(
            "out=''\nprev=''\nfor a in \"$@\"; do if [ \"$prev\" = -o ]; then out=\"$a\"; fi; prev=\"$a\"; done\ncat '{}' > \"$out\"\n",
            fixture.display()
        ),
    );
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.profiler.tool_dirs = Some(vec![bin]);
    s.profiler.perf_allowed = Some(false);
    s.execute("runConfig.create", &json!({ "name": "app", "kind": "command", "command": "/bin/echo hi" })).unwrap();
    s.agent_access = AgentAccess::headless(&[Capability::Profile]);

    let started = ok(&mut s, "profile_start", json!({ "config": "app" }));
    assert_eq!(started["tool"], "samply", "{started}");
    let hot = ok(&mut s, "profile_hotspots", json!({ "limit": 5, "timeoutMs": 60_000 }));
    let top = &hot["hotspots"][0];
    assert_eq!(top["name"], "inner", "{hot}");
    assert!((top["selfPercent"].as_f64().unwrap() - 66.6).abs() < 1.0, "{hot}");
    assert!(hot["hotspots"].as_array().unwrap().len() <= 5);
    // Nothing is recording: stop just reports the snapshot.
    let again = ok(&mut s, "profile_stop", json!({ "limit": 1 }));
    assert_eq!(again["hotspots"].as_array().unwrap().len(), 1);
    // M15 PP (QA M14 #25): the order is fixed whatever the IDE's Method List shows, and `sort`
    // picks self (default) or total.
    s.execute("profiler.sortMethods", &json!({ "column": "name", "descending": false })).unwrap();
    let same = ok(&mut s, "profile_hotspots", json!({ "limit": 5 }));
    assert_eq!(same["hotspots"], hot["hotspots"]);
    assert_eq!(same["sort"], "self");
    let by_total = ok(&mut s, "profile_hotspots", json!({ "limit": 5, "sort": "total" }));
    assert_eq!(by_total["hotspots"][0]["name"], "main", "{by_total}");
    assert_eq!(by_total["report"]["sort"], "total", "{by_total}");
    assert!(err(&mut s, "profile_hotspots", json!({ "sort": "name", "timeoutMs": 5_000 })).contains("sort"));
    assert!(err(&mut s, "profile_start", json!({ "config": 7 })).contains("no run configuration 7"));
    // Without the capability it is denied headless.
    s.agent_access = AgentAccess::headless(&[]);
    assert!(err(&mut s, "profile_start", json!({})).contains("--allow profile"));
    s.shutdown();
}

/// `inspect` with a glob scope or a named profile goes through the analysis jobs (real ruff when
/// installed).
#[test]
fn inspect_with_glob_scope_and_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::create_dir_all(root.join("gen")).unwrap();
    std::fs::write(root.join("app/main.py"), "import os\n").unwrap();
    std::fs::write(root.join("gen/out.py"), "import sys\n").unwrap();
    let mut s = session(&root.join(".home"));
    s.agent_access = AgentAccess::headless(&[Capability::Run]);
    open(&mut s, &root);
    assert!(err(&mut s, "inspect", json!({ "scope": "glob" })).contains("pattern"));
    let ruff = std::process::Command::new("ruff").arg("--version").output().is_ok_and(|o| o.status.success());
    if !ruff {
        eprintln!("skipping the ruff part: ruff not found");
        return;
    }
    let v = ok(
        &mut s,
        "inspect",
        json!({ "scope": "glob", "pattern": "**/*.py, !gen/**", "analyzers": ["ruff"], "timeoutMs": 120_000 }),
    );
    let files: Vec<&str> = v["findings"].as_array().unwrap().iter().filter_map(|f| f["file"].as_str()).collect();
    assert!(files.iter().any(|f| f.ends_with("main.py")), "{v}");
    assert!(!files.iter().any(|f| f.ends_with("out.py")), "{v}");
    let v = ok(&mut s, "inspect", json!({ "profile": "Default", "analyzers": ["ruff"], "timeoutMs": 120_000 }));
    assert!(v["findings"].as_array().unwrap().iter().any(|f| f["file"].as_str().is_some_and(|p| p.ends_with("out.py"))), "{v}");
}

/// Agents always ask before acting on an SSH host or Docker container (M16, the user's rule):
/// a once-only card even when the base capability is allowed, again on every call, refused
/// headless; a data source behind an SSH tunnel counts too; local calls are unchanged.
#[test]
fn remote_targets_are_always_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    for cap in ["run", "debug", "write", "db-write"] {
        s.execute("agentAccess.set", &json!({ "capability": cap, "level": "allow" })).unwrap();
    }
    assert!(s.execute("agentAccess.set", &json!({ "capability": "remote", "level": "allow" })).is_err());

    let card = |s: &mut Session| {
        pump_until(s, "the remote approval card", |s| s.approvals.len() == 1);
        s.approvals.pending().next().unwrap().clone()
    };
    // A Docker command: asked although `run` is allowed; "Always" counts as once.
    let docker = json!({ "id": "docker.list" });
    let slot = submit(&mut s, "Claude Code", "codecraft_execute", docker.clone());
    let req = card(&mut s);
    assert!(req.once && req.source.ends_with("remote"), "{req:?}");
    assert!(req.title.contains("docker.list"), "{}", req.title);
    s.approvals.decide(req.id, Decision::AllowAlways);
    let r = wait_answer(&mut s, &slot);
    assert!(!result_text(&r).contains("denied"), "{}", result_text(&r));
    let slot = submit(&mut s, "Claude Code", "codecraft_execute", docker.clone());
    let req = card(&mut s);
    s.approvals.decide(req.id, Decision::Deny);
    assert!(result_text(&wait_answer(&mut s, &slot)).contains("denied by the user"));

    // A run on an SSH target names it on the card.
    let slot = submit(&mut s, "Claude Code", "run_start", json!({ "config": 0, "target": "ssh:web" }));
    let req = card(&mut s);
    assert!(req.title.contains("SSH") && req.title.contains("web"), "{}", req.title);
    s.approvals.decide(req.id, Decision::Deny);
    assert!(wait_answer(&mut s, &slot).is_error);

    // A data source behind an SSH tunnel.
    s.execute(
        "db.addDataSource",
        &json!({ "kind": "postgres", "name": "Prod", "host": "10.0.0.5", "database": "app", "ssh": "deploy@bastion:22" }),
    )
    .unwrap();
    let slot = submit(&mut s, "Claude Code", "db_query", json!({ "dataSource": "Prod", "sql": "select 1" }));
    let req = card(&mut s);
    assert!(req.once && req.title.contains("SSH tunnel"), "{}", req.title);
    s.approvals.decide(req.id, Decision::Deny);
    assert!(wait_answer(&mut s, &slot).is_error);

    // Local calls don't ask.
    let r = agent_tools::call_blocking(&mut s, Caller::new("Claude Code"), "run_configurations", json!({}), LONG);
    assert!(!r.is_error, "{}", result_text(&r));
    assert!(s.approvals.is_empty());

    // Headless: refused, even with `remote` passed to the policy.
    s.agent_access = AgentAccess::headless(&[Capability::Run, Capability::Remote]);
    let r = agent_tools::call_blocking(&mut s, Caller::new("cli"), "codecraft_execute", docker, LONG);
    assert!(r.is_error && result_text(&r).contains("headless"), "{}", result_text(&r));
    s.shutdown();
}

#[test]
fn saving_a_remote_mirror_also_requires_the_remote_capability() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.remote.project = Some(codecraft_engine::remote::RemoteProject {
        server: "fixture-only".into(),
        destination: "fixture-only".into(),
        remote: "~/project".into(),
        connection: codecraft_engine::remote::ConnectionState::Disconnected,
        error: None,
        pending_uploads: Default::default(),
    });
    s.execute("agentAccess.set", &json!({"capability": "write", "level": "allow"})).unwrap();
    s.execute("agentAccess.set", &json!({"capability": "remote", "level": "off"})).unwrap();
    let result = call(&mut s, "codecraft_execute", json!({"id": "file.saveAll"}));
    assert!(result.is_error, "{}", result_text(&result));
    assert!(result_text(&result).contains("remote"), "{}", result_text(&result));
    assert!(!s.remote.busy());
    s.execute("agentAccess.set", &json!({"capability":"run", "level":"allow"})).unwrap();
    for id in ["terminal.new", "terminal.newRemote", "run.run", "run.testsInFile", "run.profile", "run.debug", "cargo.check", "cargo.clippy"] {
        let result = call(&mut s, "codecraft_execute", json!({"id":id}));
        assert!(result.is_error && result_text(&result).contains("remote"), "{id}: {}", result_text(&result));
    }
    assert!(s.terminal.tabs.is_empty());
    assert!(!s.remote.busy());
    s.shutdown();
}

#[test]
fn opening_a_remote_mirror_also_requires_the_remote_capability() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = project(tmp.path());
    let mirror = tmp.path().join("mirror");
    let mut s = session(&tmp.path().join("home"));
    open(&mut s, &dir);
    s.remote.programs.ssh = tmp.path().join("no-real-ssh").display().to_string();
    let mut cfg = codecraft_remote::DeploymentConfig {
        remote_development: Some("fixture".into()), ..Default::default()
    };
    cfg.ensure_server("fixture").unwrap().mappings.push(codecraft_remote::PathMapping {
        local: String::new(), remote: "~/fixture".into(),
    });
    codecraft_engine::deployment::save(&mirror, &cfg).unwrap();
    s.execute("agentAccess.set", &json!({"capability": "write", "level": "allow"})).unwrap();
    s.execute("agentAccess.set", &json!({"capability": "remote", "level": "off"})).unwrap();
    let result = call(&mut s, "codecraft_execute", json!({"id": "file.open", "params": {"path": mirror}}));
    assert!(result.is_error, "{}", result_text(&result));
    assert!(result_text(&result).contains("remote"), "{}", result_text(&result));
    assert_eq!(s.ws.project.as_ref().unwrap().root, dir);
    assert!(!s.remote.busy());
    s.shutdown();
}
