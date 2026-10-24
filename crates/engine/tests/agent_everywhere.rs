//! M15 track AI, "AI agents everywhere": Antigravity and Grok jobs get CodeCraft's tools the way their
//! CLIs document (the job's environment for a bridge the user registered with `agy mcp add` or
//! `grok mcp add`), a job's bridge must prove it is the job (its key), Review jobs' calls
//! run on their worktree, and `notifications/cancelled` / disconnects cancel waiting calls and
//! withdraw their approval cards. Fake agent CLIs only (shell scripts); never the real ones, the
//! user's ~/.gemini/antigravity-cli, ~/.grok, ~/.claude or ~/.codecraft.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_access::{AgentAccess, Capability, Level};
use codecraft_engine::agent_mcp;
use codecraft_engine::agent_tools::{self, CallKey, Caller};
use codecraft_engine::agents::{Activity, JobId, JobStatus};
use codecraft_engine::approvals::Decision;
use codecraft_engine::mcp::ToolResult;

const LONG: Duration = Duration::from_secs(120);

fn session(home: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s.db.set_secret_store(Arc::new(codecraft_engine::db::MemoryStore::default()));
    s
}

fn pump_until(s: &mut Session, what: &str, mut cond: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        assert!(start.elapsed() < LONG, "timed out waiting for {what}");
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open(s: &mut Session, dir: &Path) {
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(s, "the project scan", |s| !s.jobs_running());
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A temp directory with its real path (macOS: /private/var/…), so paths compare equal.
fn tmpdir() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

fn project(root: &Path) -> PathBuf {
    let dir = root.join("proj");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    dir
}

/// A session with the IDE server running (lock files in the temp dir).
fn ide_session(root: &Path, dir: &Path) -> (Session, u16, String) {
    let mut s = session(&root.join("home"));
    s.ide.set_lock_dir(Some(root.join("locks")));
    open(&mut s, dir);
    pump_until(&mut s, "the IDE server", |s| s.ide.port().is_some());
    let port = s.ide.port().unwrap();
    let token = codecraft_engine::mcp::lockfile::read(&s.ide.lock_path().unwrap()).unwrap().auth_token;
    (s, port, token)
}

fn status(s: &Session, id: u64) -> JobStatus {
    s.agents.job(JobId(id)).map(|j| j.status).unwrap()
}

fn start_job(s: &mut Session, backend: &str, mode: &str, fake: &Path) -> u64 {
    s.execute("agent.settings", &json!({ "binaries": { backend: fake } })).unwrap();
    let r = s.execute("agent.start", &json!({ "backend": backend, "mode": mode, "prompt": "look around" })).unwrap();
    r["job"].as_u64().unwrap()
}

/// `KEY=value` lines a fake CLI logged.
fn logged_env(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

// ---------------------------------------------------------------- an in-process bridge

/// The stdio side of a bridge to the IDE server, run on a thread like an agent's MCP server.
struct Bridge {
    input: Option<std::io::PipeWriter>,
    rx: crossbeam_channel::Receiver<Value>,
    thread: Option<std::thread::JoinHandle<codecraft_engine::mcp::Result<()>>>,
}

impl Bridge {
    fn start(port: u16, token: &str, tag: serde_json::Map<String, Value>) -> Self {
        let (in_r, in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let cfg = codecraft_engine::mcp::bridge::BridgeConfig { port, token: token.to_string(), client_tag: tag };
        let thread = std::thread::spawn(move || codecraft_engine::mcp::bridge::run(&cfg, BufReader::new(in_r), out_w));
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for line in BufReader::new(out_r).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Self { input: Some(in_w), rx, thread: Some(thread) }
    }

    fn send(&mut self, v: Value) {
        let w = self.input.as_mut().unwrap();
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    fn init(&mut self, s: &mut Session, name: &str) {
        self.send(
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "clientInfo": { "name": name } } }),
        );
        self.wait(s, 0);
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }

    fn call(&mut self, id: u64, name: &str, args: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } }));
    }

    fn wait(&mut self, s: &mut Session, id: u64) -> Value {
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < LONG, "no bridged response {id}");
            s.pump();
            while let Ok(v) = self.rx.try_recv() {
                if v["id"] == id {
                    return v;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A call's JSON result (it must succeed).
    fn ok(&mut self, s: &mut Session, id: u64, name: &str, args: Value) -> Value {
        self.call(id, name, args.clone());
        let r = self.wait(s, id);
        assert!(r["result"].get("isError").is_none(), "{name} {args}: {r}");
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap_or(Value::Null)
    }

    fn err(&mut self, s: &mut Session, id: u64, name: &str, args: Value) -> String {
        self.call(id, name, args.clone());
        let r = self.wait(s, id);
        assert_eq!(r["result"]["isError"], true, "{name} {args} should fail: {r}");
        r["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn close(mut self, s: &mut Session) {
        self.input = None;
        let start = Instant::now();
        while self.thread.as_ref().is_some_and(|t| !t.is_finished()) && start.elapsed() < Duration::from_secs(10) {
            s.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(t) = self.thread.take() {
            assert!(t.join().unwrap().is_ok());
        }
    }
}

fn tag(job: u64, key: &str) -> serde_json::Map<String, Value> {
    let mut t = serde_json::Map::new();
    t.insert("codecraftJob".into(), json!(job));
    t.insert("codecraftJobKey".into(), json!(key));
    t
}

// ---------------------------------------------------------------- Antigravity and Grok

#[test]
fn antigravity_jobs_carry_the_job_and_take_no_mcp_flag() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let (mut s, port, _) = ide_session(&root, &dir);
    let log = root.join("agy.log");
    let fake = script(
        &root,
        "agy",
        &format!(
            "echo \"CODECRAFT_JOB=$CODECRAFT_JOB\" >> '{log}'\n\
             echo \"CODECRAFT_IDE_PORT=$CODECRAFT_IDE_PORT\" >> '{log}'\n\
             printf '%s\\n' \"$@\" > '{args}'\n\
             echo '{{\"event\":\"init\",\"conversation_id\":\"c-1\"}}'\n\
             echo '{{\"event\":\"result\",\"result\":{{\"status\":\"SUCCESS\",\"duration_seconds\":0.1}}}}'\n",
            log = log.display(),
            args = root.join("agy.args").display(),
        ),
    );
    let job = start_job(&mut s, "antigravity", "auto", &fake);
    pump_until(&mut s, "the fake Antigravity job", |s| !s.agents.job(JobId(job)).unwrap().is_running());
    let env = logged_env(&std::fs::read_to_string(&log).unwrap());
    assert_eq!(env["CODECRAFT_JOB"], job.to_string());
    assert_eq!(env["CODECRAFT_IDE_PORT"], port.to_string());
    // agy has no per-run MCP flag: it reads servers from `agy mcp add`.
    let cli_args = std::fs::read_to_string(root.join("agy.args")).unwrap();
    assert!(cli_args.contains("--output-format") && !cli_args.contains("mcp"), "{cli_args}");
    assert_eq!(s.agents.job(JobId(job)).unwrap().session_id.as_deref(), Some("c-1"));
    s.shutdown();
}

#[test]
fn grok_jobs_carry_the_job_for_a_bridge_the_user_registered() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let (mut s, port, token) = ide_session(&root, &dir);
    let log = root.join("grok.log");
    let fake = script(
        &root,
        "grok",
        &format!(
            "echo \"CODECRAFT_IDE_PORT=$CODECRAFT_IDE_PORT\" >> '{log}'\n\
             echo \"CODECRAFT_IDE_LOCK_DIR=$CODECRAFT_IDE_LOCK_DIR\" >> '{log}'\n\
             echo \"CODECRAFT_JOB=$CODECRAFT_JOB\" >> '{log}'\n\
             echo \"CODECRAFT_JOB_KEY=$CODECRAFT_JOB_KEY\" >> '{log}'\n\
             echo \"ARGS=$*\" >> '{log}'\n",
            log = log.display()
        ),
    );
    let job = start_job(&mut s, "grok", "auto", &fake);
    pump_until(&mut s, "the fake Grok job", |s| !s.agents.job(JobId(job)).unwrap().is_running());
    let env = logged_env(&std::fs::read_to_string(&log).unwrap());
    assert_eq!(env["CODECRAFT_IDE_PORT"], port.to_string());
    assert_eq!(env["CODECRAFT_JOB"], job.to_string());
    assert_eq!(env["CODECRAFT_JOB_KEY"], agent_tools::job_key(&s, JobId(job)));
    assert!(!env["ARGS"].contains("mcp"), "Grok has no MCP flag: {}", env["ARGS"]);
    let hint = s
        .agents
        .job(JobId(job))
        .unwrap()
        .activity
        .iter()
        .any(|a| matches!(a, Activity::Info { text } if text.contains("grok mcp add") && text.contains("--ide")));
    assert!(hint, "the job says how to give Grok the tools");

    // `codecraft-cli mcp --ide`, started by Grok with that environment from the job's worktree or
    // anywhere: it reaches this IDE as the job (an Auto job: no card).
    let target = agent_mcp::resolve_bridge(
        agent_mcp::BridgeArgs { discover: true, ..Default::default() },
        &|k| env.get(k).cloned(),
        Path::new("/somewhere/else"),
    )
    .unwrap();
    assert_eq!(target.args.port, port);
    let mut b = Bridge::start(port, &token, agent_mcp::client_tag(&target));
    b.init(&mut s, "grok");
    let bp = b.ok(&mut s, 1, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 2 }));
    assert_eq!(bp["line"], 2);
    assert!(s.approvals.is_empty(), "an Auto job's call is not asked");
    // The same bridge without the job's environment, from inside the project: the IDE is found by
    // its lock file, and the call is an ordinary client's (asked).
    let plain = agent_mcp::resolve_bridge(
        agent_mcp::BridgeArgs { discover: true, lock_dir: Some(root.join("locks")), ..Default::default() },
        &|_| None,
        &dir.join("src"),
    )
    .unwrap();
    assert_eq!((plain.args.port, plain.args.job), (port, None));
    b.close(&mut s);
    s.shutdown();
}

/// Any client with the lock file's token could claim to be an Auto job; without the job's key the
/// claim is ignored.
#[test]
fn a_job_tag_without_the_job_key_is_not_trusted() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let (mut s, port, token) = ide_session(&root, &dir);
    let fake = script(&root, "claude", "echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n");
    let job = start_job(&mut s, "claude", "auto", &fake);
    pump_until(&mut s, "the fake job", |s| !s.agents.job(JobId(job)).unwrap().is_running());
    for (n, bad) in [
        (1u64, tag(job, "forged")),
        (2, {
            let mut t = serde_json::Map::new();
            t.insert("codecraftJob".into(), json!(job));
            t
        }),
    ] {
        let mut b = Bridge::start(port, &token, bad);
        b.init(&mut s, "intruder");
        b.call(n, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 1 }));
        pump_until(&mut s, "the card", |s| s.approvals.len() == 1);
        let card = s.approvals.pending().next().unwrap().clone();
        assert!(card.source.starts_with("intruder"), "{}", card.source);
        s.approvals.decide(card.id, Decision::Deny);
        assert_eq!(b.wait(&mut s, n)["result"]["isError"], true);
        b.close(&mut s);
    }
    assert!(s.debug.breakpoints.is_empty());
    s.shutdown();
}

// ---------------------------------------------------------------- cancellation

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

/// Pump for `d`, collecting whatever the client receives.
fn drain(s: &mut Session, c: &mut ws::Client, d: Duration) -> Vec<Value> {
    let until = Instant::now() + d;
    let mut got = Vec::new();
    while Instant::now() < until {
        s.pump();
        while let Some(m) = c.poll() {
            got.push(m);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    got
}

fn wait_ws(s: &mut Session, c: &mut ws::Client, id: u64) -> Value {
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < LONG, "no response {id}");
        s.pump();
        if let Some(m) = c.poll()
            && m["id"] == id
        {
            return m;
        }
    }
}

fn call_msg(id: u64, name: &str, args: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } })
}

fn cancel_msg(id: u64) -> Value {
    json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id, "reason": "user" } })
}

#[test]
fn cancelled_calls_and_disconnected_clients_withdraw_their_cards() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let (mut s, port, token) = ide_session(&root, &dir);
    s.execute("runConfig.create", &json!({ "name": "nap", "kind": "command", "command": "/bin/sleep 30" })).unwrap();
    let mut c = ws::Client::connect(port, &token);
    c.send(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "claude-code" } } }));
    wait_ws(&mut s, &mut c, 1);

    // A call waiting for its approval card: cancelling withdraws the card, nothing is answered.
    c.send(&call_msg(10, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 1 })));
    pump_until(&mut s, "the card", |s| s.approvals.len() == 1);
    c.send(&cancel_msg(10));
    pump_until(&mut s, "the card to go", |s| s.approvals.is_empty());
    assert_eq!(s.agent_tools.pending(), 0);
    assert_eq!(s.agent_tools.cancelled, 1);
    let got = drain(&mut s, &mut c, Duration::from_millis(300));
    assert!(!got.iter().any(|m| m["id"] == 10), "a cancelled request gets no answer: {got:?}");
    assert!(s.debug.breakpoints.is_empty());

    // A long wait (a run's end): cancelled the same way; the next call is answered as usual.
    s.agent_access.set_level(Capability::Run, Level::Allow).unwrap();
    c.send(&call_msg(11, "run_start", json!({ "config": "nap" })));
    let started = wait_ws(&mut s, &mut c, 11);
    let started: Value = serde_json::from_str(started["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let tab = started["tab"].as_u64().unwrap();
    c.send(&call_msg(12, "run_output", json!({ "tab": tab, "waitMs": 60_000 })));
    pump_until(&mut s, "the wait", |s| s.agent_tools.pending_tools() == ["run_output"]);
    c.send(&cancel_msg(12));
    pump_until(&mut s, "the cancellation", |s| s.agent_tools.pending() == 0);
    c.send(&call_msg(13, "run_stop", json!({ "tab": tab })));
    let stopped = wait_ws(&mut s, &mut c, 13);
    assert!(stopped["result"].get("isError").is_none(), "{stopped}");
    let got = drain(&mut s, &mut c, Duration::from_millis(300));
    assert!(!got.iter().any(|m| m["id"] == 12), "{got:?}");
    // Cancelling something already answered (or unknown) changes nothing.
    c.send(&cancel_msg(13));
    c.send(&cancel_msg(999));
    drain(&mut s, &mut c, Duration::from_millis(100));
    assert_eq!(s.agent_tools.cancelled, 2);

    // A client that disconnects while its card is shown: the card goes at once.
    let mut other = ws::Client::connect(port, &token);
    other.send(
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "codex" } } }),
    );
    wait_ws(&mut s, &mut other, 1);
    other.send(&call_msg(2, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 2 })));
    pump_until(&mut s, "the second card", |s| s.approvals.len() == 1);
    drop(other);
    let gone = Instant::now();
    pump_until(&mut s, "the disconnected client's card to go", |s| s.approvals.is_empty());
    assert!(gone.elapsed() < Duration::from_secs(10), "withdrawn when the client went, not after the timeout");
    assert_eq!(s.agent_tools.cancelled, 3);
    s.shutdown();
}

#[test]
fn stdio_cancellation_in_process() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let mut s = session(&root.join("home"));
    open(&mut s, &dir);
    s.execute("runConfig.create", &json!({ "name": "nap", "kind": "command", "command": "/bin/sleep 30" })).unwrap();
    s.agent_access = AgentAccess::headless(&[Capability::Run]);
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(out_r).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let client = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut send = |v: Value| {
            writeln!(in_w, "{v}").unwrap();
            in_w.flush().unwrap();
        };
        let wait = |id: u64, got: &mut Vec<Value>| -> Value {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(60) {
                if let Ok(v) = rx.recv_timeout(Duration::from_millis(50)) {
                    got.push(v.clone());
                    if v["id"] == id {
                        return v;
                    }
                }
            }
            Value::Null
        };
        send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "t" } } }));
        wait(1, &mut got);
        send(call_msg(2, "run_start", json!({ "config": 0 })));
        let started = wait(2, &mut got);
        let tab: Value = serde_json::from_str(started["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let tab = tab["tab"].as_u64().unwrap();
        send(call_msg(3, "run_output", json!({ "tab": tab, "waitMs": 60_000 })));
        std::thread::sleep(Duration::from_millis(300));
        send(cancel_msg(3));
        send(call_msg(4, "run_stop", json!({ "tab": tab })));
        let stopped = wait(4, &mut got);
        // Anything late for 3 would arrive by now.
        let until = Instant::now() + Duration::from_millis(400);
        while Instant::now() < until {
            if let Ok(v) = rx.recv_timeout(Duration::from_millis(20)) {
                got.push(v);
            }
        }
        drop(send);
        (got, stopped)
    });
    agent_mcp::serve_stdio(&mut s, BufReader::new(in_r), out_w).unwrap();
    let (got, stopped) = client.join().unwrap();
    assert!(stopped["result"].get("isError").is_none(), "{stopped}");
    assert!(!got.iter().any(|v| v["id"] == 3), "the cancelled wait got no answer: {got:?}");
    assert_eq!(s.agent_tools.cancelled, 1);
    s.shutdown();
}

/// The engine side of cancellation without a transport.
#[test]
fn cancel_and_client_gone_answer_with_an_error_once() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let mut s = session(&root.join("home"));
    open(&mut s, &dir);
    let slot = |s: &mut Session, key: CallKey, line: u64| {
        let out = Arc::new(std::sync::Mutex::new(Vec::<ToolResult>::new()));
        let o = Arc::clone(&out);
        agent_tools::submit_call(
            s,
            Caller::new("Claude Code"),
            Some(key),
            "debug_breakpoint_set",
            json!({ "path": "notes.txt", "line": line }),
            Box::new(move |r| o.lock().unwrap().push(r)),
        );
        out
    };
    let a = slot(&mut s, CallKey::new("ide:1", json!(5)), 1);
    let b = slot(&mut s, CallKey::new("ide:1", json!(6)), 2);
    let c = slot(&mut s, CallKey::new("ide:2", json!(5)), 3);
    pump_until(&mut s, "three cards", |s| s.approvals.len() == 3);
    assert!(agent_tools::cancel(&mut s, "ide:1", &json!(5)));
    assert!(!agent_tools::cancel(&mut s, "ide:1", &json!(5)), "already gone");
    assert!(!agent_tools::cancel(&mut s, "ide:3", &json!(6)), "another client's id");
    assert_eq!(s.approvals.len(), 2);
    assert_eq!(agent_tools::client_gone(&mut s, "ide:1"), 1);
    assert_eq!(s.approvals.len(), 1);
    for (out, why) in [(&a, "cancelled"), (&b, "disconnected")] {
        let got = out.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].is_error && got[0].texts[0].contains(why), "{:?}", got[0]);
    }
    // The other client's call still waits and is answered when the user decides.
    let card = s.approvals.pending().next().unwrap().id;
    s.approvals.decide(card, Decision::Allow);
    pump_until(&mut s, "the answer", |_| !c.lock().unwrap().is_empty());
    assert!(!c.lock().unwrap()[0].is_error);
    assert_eq!(s.agent_tools.cancelled, 2);
    s.shutdown();
}

// ---------------------------------------------------------------- Review jobs on their worktree

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok, "git {args:?}");
}

#[test]
fn review_jobs_tools_act_on_their_worktree() {
    if !Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: git is missing");
        return;
    }
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(dir.join("shop.db")),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in ["create table items (id integer primary key, name text)", "insert into items(name) values ('apple')"] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    drop(conn);
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    let (mut s, port, token) = ide_session(&root, &dir);
    let args_log = root.join("args.log");
    let done = root.join("done");
    // A Review job that keeps running until the test says so (its tools are used meanwhile).
    let fake = script(
        &root,
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" > '{log}.tmp'\nmv '{log}.tmp' '{log}'\n\
             i=0\nwhile [ ! -f '{done}' ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done\n\
             echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}}'\n",
            log = args_log.display(),
            done = done.display()
        ),
    );
    let job = start_job(&mut s, "claude", "review", &fake);
    pump_until(&mut s, "the job in its worktree", |_| args_log.exists());
    let wt: PathBuf = s.agents.job(JobId(job)).unwrap().cwd.components().collect();
    assert_ne!(wt, dir);
    assert!(wt.is_dir());
    let logged = std::fs::read_to_string(&args_log).unwrap();
    let lines: Vec<&str> = logged.lines().collect();
    let i = lines.iter().position(|l| *l == "--mcp-config").unwrap();
    let cfg: Value = serde_json::from_str(lines[i + 1]).unwrap();
    let key = cfg["mcpServers"]["codecraft"]["env"]["CODECRAFT_JOB_KEY"].as_str().unwrap().to_string();

    let mut b = Bridge::start(port, &token, tag(job, &key));
    b.init(&mut s, "claude-code");
    // Reads come from the worktree.
    let read = b.ok(&mut s, 1, "file_read", json!({ "path": "notes.txt" }));
    assert_eq!(read["path"], json!(wt.join("notes.txt")), "{read}");
    let info = b.ok(&mut s, 2, "project_info", json!({}));
    assert_eq!(info["project"]["root"], json!(wt));
    // Writes land in the worktree without a card (they are reviewed afterwards); a path of the
    // open project is moved into the worktree; the project is untouched.
    let w = b.ok(&mut s, 3, "file_write", json!({ "path": "added.txt", "content": "new\n" }));
    assert_eq!(w["path"], json!(wt.join("added.txt")));
    b.ok(&mut s, 4, "file_edit", json!({ "path": dir.join("notes.txt"), "oldText": "beta", "newText": "BETA" }));
    assert!(s.approvals.is_empty());
    assert_eq!(std::fs::read_to_string(wt.join("added.txt")).unwrap(), "new\n");
    assert!(std::fs::read_to_string(wt.join("notes.txt")).unwrap().contains("BETA"));
    assert!(!dir.join("added.txt").exists());
    assert_eq!(std::fs::read_to_string(dir.join("notes.txt")).unwrap(), "alpha\nbeta\ngamma\n");
    let outside = b.err(&mut s, 5, "file_write", json!({ "path": root.join("elsewhere.txt"), "content": "x" }));
    assert!(outside.contains("only inside its worktree"), "{outside}");
    assert!(!root.join("elsewhere.txt").exists());
    // Debugging asks (a Review job is not Auto) and then acts in the worktree's session.
    b.call(6, "debug_breakpoint_set", json!({ "path": "notes.txt", "line": 2 }));
    pump_until(&mut s, "the debug card", |s| s.approvals.len() == 1);
    let card = s.approvals.pending().next().unwrap().id;
    s.approvals.decide(card, Decision::Allow);
    let r = b.wait(&mut s, 6);
    let bp: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(bp["file"], json!(wt.join("notes.txt")), "{bp}");
    assert!(s.debug.breakpoints.is_empty(), "the IDE's own debugger is untouched");
    assert_eq!(s.agent_tools.worktree_roots(), vec![wt.clone()]);
    // SQL on the worktree's copy of a SQLite file: a read just runs; a write asks (db-write)
    // although this session has no data source for that file.
    let rows = b.ok(&mut s, 7, "db_query", json!({ "dataSource": "shop.db", "sql": "select name from items" }));
    assert_eq!(rows["results"][0]["rows"][0][0], "apple", "{rows}");
    assert_eq!(rows["added"]["path"], json!(wt.join("shop.db")), "{rows}");
    b.call(8, "db_query", json!({ "dataSource": "shop.db", "sql": "delete from items" }));
    pump_until(&mut s, "the db-write card", |s| s.approvals.len() == 1);
    let card = s.approvals.pending().next().unwrap().clone();
    assert!(card.source.ends_with("db-write"), "{}", card.source);
    s.approvals.decide(card.id, Decision::Deny);
    assert_eq!(b.wait(&mut s, 8)["result"]["isError"], true);
    assert!(s.db.sources.is_empty(), "the IDE's own data sources are untouched");
    b.close(&mut s);

    // The job ends: its review lists the agent's files (not CodeCraft's own state), and the
    // worktree session closes.
    std::fs::write(&done, "").unwrap();
    pump_until(&mut s, "the review", |s| {
        let j = s.agents.job(JobId(job)).unwrap();
        j.status == JobStatus::Reviewing && !j.review.is_empty()
    });
    let rels: Vec<String> = s.agents.job(JobId(job)).unwrap().review.iter().map(|f| f.rel.clone()).collect();
    assert!(rels.contains(&"added.txt".to_string()) && rels.contains(&"notes.txt".to_string()), "{rels:?}");
    assert!(!rels.iter().any(|r| r.starts_with(".codecraft")), "{rels:?}");
    pump_until(&mut s, "the worktree session to close", |s| s.agent_tools.worktree_roots().is_empty());
    assert_eq!(status(&s, job), JobStatus::Reviewing);
    s.shutdown();
}

#[test]
fn connect_snippets_cover_every_client() {
    let (_t, root) = tmpdir();
    let dir = project(&root);
    let (mut s, port, _) = ide_session(&root, &dir);
    let v = s.execute("agent.connectExternal", &Value::Null).unwrap();
    assert_eq!(v["running"], true);
    assert_eq!(v["port"], port);
    let ids: Vec<&str> = v["clients"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["claude", "codex", "gemini", "cursor", "grok", "json"]);
    for c in v["clients"].as_array().unwrap() {
        let ide = c["ide"].as_str().unwrap();
        assert!(ide.contains("--ide"), "{ide}");
    }
    assert!(v["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("Grok")));
    // Agents may not run it (it is about connecting agents).
    let r = agent_tools::call_blocking(
        &mut s,
        Caller::new("Claude Code"),
        "codecraft_execute",
        json!({ "id": "agent.connectExternal" }),
        LONG,
    );
    assert!(r.is_error);
    s.shutdown();
}
