//! Several debug sessions at once (M15 track DM) against scripted adapters over in-process
//! pipes: the session list, selection, per-session state, breakpoints shared by language, stops
//! bringing a session forward, `session` params, Stop All, closing, and agents' waits.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_dap::{AdapterKind, Capabilities, DapClient};

use crate::Session;

type Requests = Arc<Mutex<Vec<(String, Value)>>>;
type Writer = Arc<Mutex<std::io::PipeWriter>>;

fn write_message(writer: &Writer, message: &Value) {
    let body = serde_json::to_vec(message).unwrap();
    let mut w = writer.lock().unwrap();
    let _ = w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    let _ = w.write_all(&body);
    let _ = w.flush();
}

fn read_message(reader: &mut BufReader<std::io::PipeReader>) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            len = v.parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn event(name: &str, body: Value) -> Value {
    json!({ "seq": 0, "type": "event", "event": name, "body": body })
}

/// One scripted program: `who` is its only local, evaluations answer `<who>:<expression>`, a
/// step stops again, `disconnect` ends it.
struct Fake {
    requests: Requests,
    writer: Writer,
}

impl Fake {
    fn stop(&self, reason: &str) {
        write_message(&self.writer, &event("stopped", json!({ "reason": reason, "threadId": 1 })));
    }

    fn output(&self, text: &str) {
        write_message(&self.writer, &event("output", json!({ "category": "stdout", "output": text })));
    }

    fn requests(&self, command: &str) -> Vec<Value> {
        self.requests.lock().unwrap().iter().filter(|(c, _)| c == command).map(|(_, a)| a.clone()).collect()
    }
}

fn fake(who: &str, source: PathBuf) -> (DapClient, crossbeam_channel::Receiver<codecraft_dap::DapEvent>, Fake) {
    let (client_reader, adapter_writer) = std::io::pipe().unwrap();
    let (adapter_reader, client_writer) = std::io::pipe().unwrap();
    let (client, events) = DapClient::connect(who, client_reader, client_writer).unwrap();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let writer: Writer = Arc::new(Mutex::new(adapter_writer));
    let (r2, w2, who) = (Arc::clone(&requests), Arc::clone(&writer), who.to_owned());
    std::thread::spawn(move || {
        let mut reader = BufReader::new(adapter_reader);
        while let Some(request) = read_message(&mut reader) {
            let command = request["command"].as_str().unwrap_or_default().to_owned();
            let args = request["arguments"].clone();
            r2.lock().unwrap().push((command.clone(), args.clone()));
            let mut queued = Vec::new();
            let body = match command.as_str() {
                "threads" => json!({ "threads": [{ "id": 1, "name": "main" }] }),
                "stackTrace" => json!({ "stackFrames": [{
                    "id": 100, "name": format!("{who}_main"), "line": 2, "column": 1, "source": { "path": source },
                }] }),
                "scopes" => json!({ "scopes": [{ "name": "Locals", "variablesReference": 1 }] }),
                "variables" => json!({ "variables": [{ "name": "who", "value": format!("\"{who}\""), "variablesReference": 0 }] }),
                "evaluate" => json!({ "result": format!("{who}:{}", args["expression"].as_str().unwrap_or_default()) }),
                "setBreakpoints" => {
                    let n = args["breakpoints"].as_array().map_or(0, Vec::len);
                    json!({ "breakpoints": vec![json!({ "verified": true }); n] })
                }
                "next" | "stepIn" | "stepOut" => {
                    queued.push(event("stopped", json!({ "reason": "step", "threadId": 1 })));
                    Value::Null
                }
                "continue" => {
                    queued.push(event("continued", json!({ "threadId": 1 })));
                    Value::Null
                }
                "pause" => {
                    queued.push(event("stopped", json!({ "reason": "pause", "threadId": 1 })));
                    Value::Null
                }
                _ => Value::Null,
            };
            let response = json!({ "seq": 0, "type": "response", "request_seq": request["seq"], "command": command,
                                   "success": true, "body": body });
            write_message(&w2, &response);
            for e in queued {
                write_message(&w2, &e);
            }
            if command == "disconnect" {
                write_message(&w2, &event("terminated", json!({})));
                break;
            }
        }
    });
    (client, events, Fake { requests, writer })
}

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}: {:?}", s.debug.sessions());
        s.pump();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn settled(s: &Session) -> bool {
    s.debug.is_stopped() && !s.debug.settling()
}

struct Two {
    s: Session,
    a: Fake,
    b: Fake,
    a_id: u64,
    b_id: u64,
    py: PathBuf,
    rs: PathBuf,
    _tmp: tempfile::TempDir,
}

/// A native session `a` (not selected) and a Python session `b` (selected), both running.
fn two() -> Two {
    let tmp = tempfile::tempdir().unwrap();
    let rs = tmp.path().join("main.rs");
    let py = tmp.path().join("app.py");
    std::fs::write(&rs, "fn main() {\n    let who = 1;\n}\n").unwrap();
    std::fs::write(&py, "who = 1\nprint(who)\n").unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    let (client, events, a) = fake("a", rs.clone());
    let a_id = super::install_fake_named(&mut s, client, events, AdapterKind::LldbDap, Capabilities::default(), "server");
    let (client, events, b) = fake("b", py.clone());
    let b_id = super::install_fake_named(&mut s, client, events, AdapterKind::Debugpy, Capabilities::default(), "client");
    Two { s, a, b, a_id, b_id, py, rs, _tmp: tmp }
}

#[test]
fn sessions_are_listed_selected_and_kept_apart() {
    let Two { mut s, a, b, a_id, b_id, _tmp, .. } = two();
    let list = s.execute("debug.sessions", &Value::Null).unwrap();
    assert_eq!(list["selected"], json!(b_id));
    let names: Vec<&str> = list["sessions"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["server", "client"]);
    assert_eq!(s.debug.live_count(), 2);

    // Output goes to its own session's console.
    a.output("from a\n");
    b.output("from b\n");
    pump_until(&mut s, "the output", |s| s.debug.console.iter().any(|l| super::console_text(l) == "from b"));
    assert!(!s.debug.console.iter().any(|l| super::console_text(l) == "from a"), "b's console has only b's output");

    // `a` stops while `b` is selected: `a` comes forward, with its own frames and variables.
    a.stop("breakpoint");
    pump_until(&mut s, "a's stop", |s| s.debug.selected_id() == Some(a_id) && settled(s));
    let session = s.debug.session.clone().unwrap();
    assert_eq!(session.frames[0].name, "a_main");
    assert_eq!(session.variables.get(&1).unwrap()[0].value, "\"a\"");
    assert!(s.debug.console.iter().any(|l| super::console_text(l) == "from a"));
    // The editor's execution point follows the selected session.
    assert!(session.execution.as_ref().is_some_and(|(p, _)| p.ends_with("main.rs")));

    // A command for `b` by name runs there and leaves `a` selected.
    let v = s.execute("debug.stopSummary", &json!({ "session": "client" })).unwrap();
    assert_eq!((v["sessionId"].clone(), v["state"].clone()), (json!(b_id), json!("running")));
    assert_eq!(s.debug.selected_id(), Some(a_id));
    let names: Vec<(u64, String)> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["id"].as_u64().unwrap(), x["state"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(names, [(a_id, "stopped".to_owned()), (b_id, "running".to_owned())]);
    // Pause `b` through its id: it stops and comes forward.
    s.execute("debug.pause", &json!({ "session": b_id })).unwrap();
    pump_until(&mut s, "b's pause", |s| s.debug.selected_id() == Some(b_id) && settled(s));
    assert_eq!(s.debug.session.as_ref().unwrap().frames[0].name, "b_main");
    // Selecting `a` again shows its stop as it was.
    s.execute("debug.selectSession", &json!({ "session": a_id })).unwrap();
    assert_eq!(s.debug.session.as_ref().unwrap().frames[0].name, "a_main");
    assert!(s.debug.is_stopped());

    // Bad session params are errors, never panics.
    for bad in [json!(999), json!("nobody"), json!(-1), json!([1]), json!({})] {
        assert!(s.execute("debug.stopSummary", &json!({ "session": bad })).is_err(), "{bad}");
        assert!(s.execute("debug.selectSession", &json!({ "session": bad })).is_err(), "{bad}");
    }
    assert!(s.execute("debug.selectSession", &json!({})).is_err());
    s.shutdown();
}

#[test]
fn breakpoints_are_shared_by_language_and_watches_are_per_session() {
    let Two { mut s, a, b, a_id, b_id, py, rs, _tmp } = two();
    // A Rust breakpoint goes to the native session only, a Python one to debugpy only.
    s.execute("debug.setBreakpoint", &json!({ "path": rs, "line": 1 })).unwrap();
    s.execute("debug.setBreakpoint", &json!({ "path": py, "line": 0 })).unwrap();
    pump_until(&mut s, "the breakpoints", |s| s.debug.breakpoints.iter().all(|b| b.verified == Some(true)));
    let paths = |f: &Fake| -> Vec<String> {
        f.requests("setBreakpoints").iter().map(|a| a["source"]["path"].as_str().unwrap_or_default().to_owned()).collect()
    };
    assert!(paths(&a).iter().all(|p| p.ends_with("main.rs")) && !paths(&a).is_empty(), "{:?}", paths(&a));
    assert!(paths(&b).iter().all(|p| p.ends_with("app.py")) && !paths(&b).is_empty(), "{:?}", paths(&b));

    // A watch has each session's own value.
    a.stop("breakpoint");
    pump_until(&mut s, "a's stop", |s| s.debug.selected_id() == Some(a_id) && settled(s));
    s.execute("debug.addWatch", &json!({ "expression": "total" })).unwrap();
    pump_until(&mut s, "a's watch", |s| s.debug.watches[0].value.as_deref() == Some("a:total"));
    b.stop("breakpoint");
    pump_until(&mut s, "b's stop", |s| s.debug.selected_id() == Some(b_id) && settled(s));
    pump_until(&mut s, "b's watch", |s| s.debug.watches[0].value.as_deref() == Some("b:total"));
    s.execute("debug.selectSession", &json!({ "session": a_id })).unwrap();
    assert_eq!(s.debug.watches[0].value.as_deref(), Some("a:total"));

    // Steps go to the session named, and its wait token answers with its summary.
    let r = s.execute("debug.step", &json!({ "action": "over", "session": b_id })).unwrap();
    assert_eq!(s.debug.selected_id(), Some(a_id), "the selection stays");
    let token = r["token"].as_u64().unwrap();
    let start = Instant::now();
    let v = loop {
        s.pump();
        let v = s.execute("debug.poll", &json!({ "token": token })).unwrap();
        if v["pending"] == json!(false) {
            break v;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{v}");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!((v["sessionId"].clone(), v["reason"].clone()), (json!(b_id), json!("step")), "{v}");
    assert_eq!(b.requests("next").len(), 1);
    assert!(a.requests("next").is_empty());
    s.shutdown();
}

#[test]
fn waiting_for_any_session_and_stopping_them_all() {
    let Two { mut s, a, b, a_id, b_id, _tmp, .. } = two();
    // Both run: waiting for any session waits.
    let v = s.execute("debug.waitForStop", &json!({ "timeoutMs": 10_000 })).unwrap();
    assert_eq!(v["pending"], json!(true));
    let token = v["token"].as_u64().unwrap();
    a.stop("breakpoint");
    let start = Instant::now();
    let v = loop {
        s.pump();
        let v = s.execute("debug.poll", &json!({ "token": token })).unwrap();
        if v["pending"] == json!(false) {
            break v;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{v}");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!((v["sessionId"].clone(), v["state"].clone()), (json!(a_id), json!("stopped")), "{v}");
    assert_eq!(v["sessions"].as_array().map(Vec::len), Some(2));
    // A wait for `b` keeps waiting although `a` is paused.
    let v = s.execute("debug.waitForStop", &json!({ "timeoutMs": 10_000, "session": b_id })).unwrap();
    assert_eq!(v["pending"], json!(true), "{v}");

    // Stop All ends both; the status bar and the tabs see it.
    let r = s.execute("debug.stopAll", &Value::Null).unwrap();
    assert_eq!(r["stopped"], json!(2));
    pump_until(&mut s, "both to end", |s| !s.debug.busy());
    assert_eq!(a.requests("disconnect").len(), 1);
    assert_eq!(b.requests("disconnect").len(), 1);
    assert_eq!(s.debug.live_count(), 0);
    assert_eq!(s.debug.session_count(), 2, "ended sessions keep their tabs");
    // Closing a tab removes it; the other comes forward.
    s.execute("debug.closeSession", &json!({ "session": a_id })).unwrap();
    assert_eq!(s.debug.session_count(), 1);
    assert_eq!(s.debug.selected_id(), Some(b_id));
    s.execute("debug.closeSession", &Value::Null).unwrap();
    assert_eq!(s.debug.session_count(), 0);
    assert!(s.execute("debug.closeSession", &Value::Null).is_err());
    s.shutdown();
}

#[test]
fn stopping_one_session_leaves_the_other_running() {
    let Two { mut s, a, b, a_id, b_id, _tmp, .. } = two();
    // Stop (the selected `b`) only.
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "b to end", |s| s.debug.sessions().iter().any(|x| x.id == b_id && !x.live));
    let start = Instant::now();
    while b.requests("disconnect").is_empty() && start.elapsed() < Duration::from_secs(10) {
        s.pump();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(b.requests("disconnect").len(), 1);
    assert!(a.requests("disconnect").is_empty());
    assert!(s.debug.sessions().iter().any(|x| x.id == a_id && x.live));
    // The processes list (Stop Background Processes) names each live session.
    let list = crate::misc::processes(&s);
    let debug: Vec<&crate::misc::ProcessInfo> = list.iter().filter(|p| p.kind == "debug").collect();
    assert_eq!(debug.len(), 1);
    assert_eq!(debug[0].id, a_id.to_string());
    assert!(crate::misc::stop_process(&mut s, "debug", &a_id.to_string()));
    pump_until(&mut s, "a to end", |s| !s.debug.busy());
    assert_eq!(a.requests("disconnect").len(), 1);
    // A start that fails at once (no such program) leaves the ended sessions as they were.
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    assert!(s.execute("debug.launch", &json!({ "target": { "kind": "executable", "name": "x", "program": missing } })).is_err());
    assert_eq!(s.debug.session_count(), 2);
    s.shutdown();
}

#[test]
fn inert_sessions_can_be_listed_selected_and_closed() {
    let mut d = super::DebugState::default();
    let first = d.add_inert_session(super::DebugSession { name: "one".into(), ..Default::default() });
    let second = d.add_inert_session(super::DebugSession { name: "two".into(), ..Default::default() });
    assert_eq!(d.selected_id(), Some(first));
    assert_eq!(d.session_count(), 2);
    assert_eq!(d.live_count(), 0);
    assert!(d.bring_forward(second));
    assert_eq!(d.session.as_ref().map(|x| x.name.as_str()), Some("two"));
    assert!(!d.bring_forward(77));
    assert_eq!(super::resolve_session(&d, &json!("ONE")).unwrap(), first);
    assert_eq!(super::resolve_session(&d, &json!(second.to_string())).unwrap(), second);
    assert!(super::resolve_session(&d, &json!(1.5)).is_err());
    d.drop_selected();
    assert_eq!(d.selected_id(), Some(first));
    d.drop_selected();
    assert_eq!(d.session_count(), 0);
}
