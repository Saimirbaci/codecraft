//! The DAP client against a scripted fake adapter over in-process pipes, plus process-group
//! cleanup with a real (non-DAP) child process.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::{Value, json};

use codecraft_dap::{AdapterCommand, AdapterKind, DapClient, DapEvent, SourceBreakpoint, StoppedEvent, types};

const WAIT: Duration = Duration::from_secs(10);

/// The adapter side of a pipe pair: read requests, write responses/events.
struct Fake {
    reader: BufReader<std::io::PipeReader>,
    writer: std::io::PipeWriter,
    seq: i64,
}

impl Fake {
    fn read(&mut self) -> Value {
        let mut len = None;
        loop {
            let mut line = String::new();
            assert!(self.reader.read_line(&mut line).unwrap() > 0, "client closed the stream");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length: ") {
                len = Some(v.parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; len.unwrap()];
        self.reader.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn send(&mut self, mut message: Value) {
        self.seq += 1;
        message["seq"] = self.seq.into();
        let body = serde_json::to_vec(&message).unwrap();
        self.write_raw(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        self.write_raw(&body);
    }

    fn respond(&mut self, request: &Value, body: Value) {
        self.send(json!({
            "type": "response", "request_seq": request["seq"], "command": request["command"],
            "success": true, "body": body,
        }));
    }

    fn event(&mut self, event: &str, body: Value) {
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }
}

fn pair() -> (DapClient, Receiver<DapEvent>, Fake) {
    let (client_reader, adapter_writer) = std::io::pipe().unwrap();
    let (adapter_reader, client_writer) = std::io::pipe().unwrap();
    let (client, events) = DapClient::connect("fake", client_reader, client_writer).unwrap();
    (client, events, Fake { reader: BufReader::new(adapter_reader), writer: adapter_writer, seq: 0 })
}

fn next(events: &Receiver<DapEvent>) -> DapEvent {
    events.recv_timeout(WAIT).expect("no event from the client")
}

#[test]
fn requests_are_correlated_and_events_delivered() {
    let (client, events, mut fake) = pair();
    let init = client.initialize("lldb-dap").unwrap();
    let req = fake.read();
    assert_eq!(req["type"], "request");
    assert_eq!(req["command"], "initialize");
    assert_eq!(req["arguments"]["linesStartAt1"], true);
    assert_eq!(req["arguments"]["adapterID"], "lldb-dap");
    assert_eq!(req["seq"], init.0);

    let threads = client.threads().unwrap();
    let bps = client
        .set_breakpoints(std::path::Path::new("/src/main.rs"), &[SourceBreakpoint { line: 3, ..Default::default() }])
        .unwrap();
    let threads_req = fake.read();
    let bps_req = fake.read();
    assert!(threads_req.get("arguments").is_none(), "null arguments are omitted");
    assert_eq!(bps_req["arguments"]["source"]["path"], "/src/main.rs");
    assert_eq!(bps_req["arguments"]["breakpoints"], json!([{"line": 3}]));

    // Answer out of order, with an event in between.
    fake.respond(&bps_req, json!({"breakpoints": [{"id": 1, "verified": true, "line": 3}]}));
    fake.event("stopped", json!({"reason": "breakpoint", "threadId": 7, "allThreadsStopped": true}));
    fake.respond(&threads_req, json!({"threads": [{"id": 7, "name": "main"}]}));
    fake.respond(&req, json!({"supportsConfigurationDoneRequest": true}));

    let DapEvent::Response(r) = next(&events) else { panic!() };
    assert_eq!((r.id, r.command.as_str()), (bps, "setBreakpoints"));
    let body: types::BreakpointsBody = types::parse(&r.result.unwrap()).unwrap();
    assert!(body.breakpoints[0].verified);
    let DapEvent::Event { event, body } = next(&events) else { panic!() };
    assert_eq!(event, "stopped");
    let stopped: StoppedEvent = types::parse(&body).unwrap();
    assert_eq!((stopped.reason.as_str(), stopped.thread_id), ("breakpoint", Some(7)));
    let DapEvent::Response(r) = next(&events) else { panic!() };
    assert_eq!(r.id, threads);
    let DapEvent::Response(r) = next(&events) else { panic!() };
    assert_eq!(r.id, init);
    let caps: types::Capabilities = types::parse(&r.result.unwrap()).unwrap();
    assert!(caps.supports_configuration_done_request);

    // A response nobody asked for is ignored; the stream ending ends the session.
    fake.send(json!({"type": "response", "request_seq": 999, "command": "x", "success": true}));
    drop(fake);
    assert_eq!(next(&events), DapEvent::Exited { code: None });
    assert!(client.threads().is_err());
}

#[test]
fn failed_requests_carry_the_error_text() {
    let (client, events, mut fake) = pair();
    let id = client.evaluate("nope", Some(3), "watch").unwrap();
    let req = fake.read();
    assert_eq!(req["arguments"], json!({"expression": "nope", "context": "watch", "frameId": 3}));
    fake.send(json!({
        "type": "response", "request_seq": req["seq"], "command": "evaluate", "success": false,
        "message": "evaluateFailed", "body": {"error": {"id": 1, "format": "use of undeclared identifier 'nope'"}},
    }));
    let DapEvent::Response(r) = next(&events) else { panic!() };
    assert_eq!(r.id, id);
    assert_eq!(r.result, Err("use of undeclared identifier 'nope'".to_owned()));
    let id = client.next(1).unwrap();
    let req = fake.read();
    fake.send(json!({"type": "response", "request_seq": req["seq"], "command": "next", "success": false}));
    let DapEvent::Response(r) = next(&events) else { panic!() };
    assert_eq!((r.id, r.result), (id, Err("request failed".to_owned())));
}

#[test]
fn reverse_requests() {
    let (client, events, mut fake) = pair();
    // runInTerminal is refused right away so the adapter falls back to its console.
    fake.send(json!({"type": "request", "command": "runInTerminal", "arguments": {"args": ["/bin/x"]}}));
    let answer = fake.read();
    assert_eq!(answer["type"], "response");
    assert_eq!(answer["command"], "runInTerminal");
    assert_eq!(answer["request_seq"], 1);
    assert_eq!(answer["success"], false);
    // Unknown reverse requests are refused too.
    fake.send(json!({"type": "request", "command": "somethingNew"}));
    assert_eq!(fake.read()["success"], false);
    // startDebugging goes to the caller, who answers.
    fake.send(json!({"type": "request", "command": "startDebugging",
        "arguments": {"request": "launch", "configuration": {"__pendingTargetId": "x"}}}));
    let DapEvent::Request { seq, command, arguments } = next(&events) else { panic!() };
    assert_eq!(command, "startDebugging");
    let args: types::StartDebuggingArguments = types::parse(&arguments).unwrap();
    assert_eq!(args.request, "launch");
    client.respond(seq, &command, Ok(json!({}))).unwrap();
    let answer = fake.read();
    assert_eq!((answer["request_seq"].as_i64(), answer["success"].as_bool()), (Some(seq), Some(true)));
}

#[test]
fn garbage_is_reported_and_the_stream_survives() {
    let (_client, events, mut fake) = pair();
    fake.write_raw(b"lldb says hello\n");
    fake.write_raw(b"Content-Length: 9\r\n\r\n{not json");
    fake.send(json!({"type": "mystery"}));
    fake.write_raw(b"Content-Length: 5\r\n\r\n[1,2]");
    fake.event("output", json!({"category": "stdout", "output": "hi\n"}));
    let mut errors = 0;
    loop {
        match next(&events) {
            DapEvent::Error(_) => errors += 1,
            DapEvent::Event { event, body } => {
                assert_eq!(event, "output");
                let out: types::OutputEvent = types::parse(&body).unwrap();
                assert_eq!(out.output, "hi\n");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(errors >= 3, "{errors}");
}

#[test]
fn terminate_without_a_process_ends_the_session() {
    let (client, events, mut fake) = pair();
    client.terminate(Duration::from_millis(200));
    let req = fake.read();
    assert_eq!(req["command"], "disconnect");
    assert_eq!(req["arguments"]["terminateDebuggee"], true);
    // Even if the adapter never answers or closes, the session ends.
    let start = Instant::now();
    loop {
        if let DapEvent::Exited { .. } = next(&events) {
            break;
        }
    }
    assert!(start.elapsed() < WAIT);
    assert!(client.request("threads", Value::Null).is_err());
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Stopping a spawned adapter kills its whole process group: no leftover processes.
#[cfg(unix)]
#[test]
fn terminate_kills_the_adapter_process_group() {
    let mut cmd = AdapterCommand::stdio("sleeper", AdapterKind::Other, "/bin/sh");
    // A "debuggee" in the background, its pid on stderr, and an adapter that ignores stdin.
    cmd.args = vec!["-c".into(), "sleep 300 & echo $! >&2; sleep 300".into()];
    let (client, events) = DapClient::spawn(&cmd).unwrap();
    let adapter_pid = client.pid().unwrap();
    let start = Instant::now();
    let child_pid = loop {
        if let Some(pid) = client.stderr_tail().first().and_then(|l| l.trim().parse::<u32>().ok()) {
            break pid;
        }
        assert!(start.elapsed() < WAIT, "the script never printed its child pid");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(alive(adapter_pid) && alive(child_pid));
    client.terminate(Duration::from_millis(100));
    loop {
        match next(&events) {
            DapEvent::Exited { .. } => break,
            DapEvent::Error(e) => panic!("a requested stop is not an error: {e}"),
            _ => {}
        }
    }
    let start = Instant::now();
    while alive(child_pid) && start.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(child_pid), "the adapter's child survived");
    assert!(!alive(adapter_pid));
}

#[cfg(unix)]
#[test]
fn dropping_the_client_kills_the_adapter() {
    let mut cmd = AdapterCommand::stdio("sleeper", AdapterKind::Other, "/bin/sh");
    cmd.args = vec!["-c".into(), "sleep 300".into()];
    let (client, events) = DapClient::spawn(&cmd).unwrap();
    let pid = client.pid().unwrap();
    drop(client);
    loop {
        if let DapEvent::Exited { .. } = next(&events) {
            break;
        }
    }
    assert!(!alive(pid));
}

#[cfg(unix)]
#[test]
fn an_adapter_that_dies_is_reported_with_its_stderr() {
    let mut cmd = AdapterCommand::stdio("crasher", AdapterKind::Other, "/bin/sh");
    cmd.args = vec!["-c".into(), "echo 'fatal: no debugger here' >&2; exit 3".into()];
    let (_client, events) = DapClient::spawn(&cmd).unwrap();
    let mut report = None;
    loop {
        match next(&events) {
            DapEvent::Error(e) => report = Some(e),
            DapEvent::Exited { code } => {
                assert_eq!(code, Some(3));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let report = report.unwrap();
    assert!(report.contains("exited unexpectedly") && report.contains("fatal: no debugger here"), "{report}");
}

#[test]
fn spawning_a_missing_program_fails_cleanly() {
    let cmd = AdapterCommand::stdio("missing", AdapterKind::Other, "/definitely/not/an/adapter");
    assert!(DapClient::spawn(&cmd).is_err());
}
