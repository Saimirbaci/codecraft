//! A fake language server speaking LSP over `std::io::pipe()`, and event helpers. Every wait has
//! a timeout so a broken client fails the test instead of hanging it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use codecraft_lsp::{LspClient, ServerEvent};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use serde_json::{Value, json};

pub const WAIT: Duration = Duration::from_secs(10);
pub const QUIET: Duration = Duration::from_millis(250);

pub fn root() -> PathBuf {
    std::env::temp_dir().join("codecraft lsp tests").join("my crate")
}

pub fn file() -> PathBuf {
    root().join("src").join("main.rs")
}

pub fn uri_of(path: &std::path::Path) -> String {
    codecraft_lsp::path_to_uri(path).unwrap().as_str().to_owned()
}

/// Test-side framing, deliberately independent of the crate's reader.
fn read_frame(reader: &mut impl BufRead) -> Option<Value> {
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
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            len = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

pub fn frame(value: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend(body);
    out
}

pub struct FakeServer {
    /// Everything the client wrote, parsed. Disconnects when the client closes its output.
    pub incoming: Receiver<Value>,
    out: Option<PipeWriter>,
}

impl FakeServer {
    pub fn recv(&self) -> Value {
        self.incoming.recv_timeout(WAIT).expect("a message from the client")
    }

    pub fn expect(&self, method: &str) -> Value {
        let message = self.recv();
        assert_eq!(message["method"], method, "unexpected message {message}");
        message
    }

    pub fn assert_quiet(&self) {
        match self.incoming.recv_timeout(QUIET) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("expected no message, got {other:?}"),
        }
    }

    pub fn send(&mut self, value: Value) {
        self.send_raw(&frame(&value));
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let out = self.out.as_mut().expect("server output is open");
        out.write_all(bytes).unwrap();
        out.flush().unwrap();
    }

    pub fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }));
    }

    pub fn reply_error(&mut self, request: &Value, code: i64, message: &str) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": code, "message": message } }));
    }

    /// Close the server→client stream (the client sees EOF).
    pub fn close(&mut self) {
        self.out = None;
    }

    /// Answer `initialize` and consume `initialized`; returns the initialize request.
    pub fn handshake(&mut self) -> Value {
        let init = self.expect("initialize");
        self.reply(&init, json!({ "capabilities": { "hoverProvider": true } }));
        self.expect("initialized");
        init
    }
}

pub fn start() -> (LspClient, Receiver<ServerEvent>, FakeServer) {
    start_with(None, None)
}

pub fn start_with(
    options: Option<Value>,
    configuration: Option<Value>,
) -> (LspClient, Receiver<ServerEvent>, FakeServer) {
    let (client_in, server_out) = std::io::pipe().unwrap();
    let (server_in, client_out) = std::io::pipe().unwrap();
    let (client, events) =
        LspClient::connect(Box::new(client_in), Box::new(client_out), root(), options, configuration).unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    thread::spawn(move || {
        let mut reader = BufReader::new(server_in);
        while let Some(message) = read_frame(&mut reader) {
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    (client, events, FakeServer { incoming: rx, out: Some(server_out) })
}

/// Start and complete the handshake, consuming the `Initialized` event.
pub fn start_initialized() -> (LspClient, Receiver<ServerEvent>, FakeServer) {
    let (client, events, mut server) = start();
    server.handshake();
    match next_event(&events) {
        ServerEvent::Initialized(_) => {}
        other => panic!("expected Initialized, got {other:?}"),
    }
    (client, events, server)
}

pub fn next_event(events: &Receiver<ServerEvent>) -> ServerEvent {
    events.recv_timeout(WAIT).expect("an event from the client")
}

pub fn assert_no_event(events: &Receiver<ServerEvent>) {
    match events.recv_timeout(QUIET) {
        Err(_) => {}
        Ok(event) => panic!("expected no event, got {event:?}"),
    }
}

pub fn expect_error(events: &Receiver<ServerEvent>, needle: &str) -> String {
    match next_event(events) {
        ServerEvent::Error(message) => {
            assert!(message.contains(needle), "error {message:?} does not mention {needle:?}");
            message
        }
        other => panic!("expected an error mentioning {needle:?}, got {other:?}"),
    }
}

pub fn expect_exited(events: &Receiver<ServerEvent>) -> Option<i32> {
    match next_event(events) {
        ServerEvent::Exited { code } => code,
        other => panic!("expected Exited, got {other:?}"),
    }
}
