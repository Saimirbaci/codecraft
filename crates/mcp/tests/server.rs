//! The server over a real loopback WebSocket, with a test client playing Claude Code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use codecraft_mcp::{AUTH_HEADER, IdeServer, ServerConfig, ServerEvent, ToolResult, lockfile};

type Ws = WebSocket<TcpStream>;

fn connect(port: u16, token: Option<&str>) -> Result<Ws, tungstenite::Error> {
    connect_to("127.0.0.1", port, token)
}

fn connect_to(host: &str, port: u16, token: Option<&str>) -> Result<Ws, tungstenite::Error> {
    let stream = TcpStream::connect((host, port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("ws://127.0.0.1:{port}").into_client_request().unwrap();
    if let Some(t) = token {
        req.headers_mut().insert(AUTH_HEADER, t.parse().unwrap());
    }
    req.headers_mut().insert("sec-websocket-protocol", "mcp".parse().unwrap());
    tungstenite::client(req, stream)
        .map(|(ws, resp)| {
            assert_eq!(resp.headers().get("sec-websocket-protocol").unwrap(), "mcp");
            ws
        })
        .map_err(|e| match e {
            tungstenite::HandshakeError::Failure(e) => e,
            tungstenite::HandshakeError::Interrupted(_) => panic!("interrupted handshake"),
        })
}

fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).unwrap();
}

/// Next JSON message (skipping pings).
fn recv(ws: &mut Ws) -> Value {
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Close(_) => panic!("closed"),
            _ => {}
        }
    }
}

fn next_event(rx: &crossbeam_channel::Receiver<ServerEvent>) -> ServerEvent {
    rx.recv_timeout(Duration::from_secs(10)).unwrap()
}

fn start(dir: &std::path::Path) -> (IdeServer, crossbeam_channel::Receiver<ServerEvent>) {
    let cfg = ServerConfig::claude_code("CodeCraft", vec!["/work/project".into()], Some(dir.to_path_buf()));
    IdeServer::start(cfg).unwrap()
}

#[test]
fn lock_file_auth_and_mcp_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, events) = start(dir.path());
    let lock_path = server.lock_path().unwrap().to_path_buf();
    assert_eq!(lock_path, dir.path().join(format!("{}.lock", server.port())));
    let info = lockfile::read(&lock_path).unwrap();
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.ide_name, "CodeCraft");
    assert_eq!(info.transport, "ws");
    assert_eq!(info.workspace_folders, vec!["/work/project".to_string()]);
    assert_eq!(info.auth_token, server.auth_token());

    // Wrong or missing token: the upgrade is refused with 401.
    for bad in [Some("wrong"), None] {
        match connect(server.port(), bad) {
            Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 401),
            other => panic!("expected 401, got {:?}", other.map(|_| ())),
        }
    }

    let mut ws = connect(server.port(), Some(&info.auth_token)).unwrap();
    assert!(matches!(next_event(&events), ServerEvent::Connected { .. }));
    send(
        &mut ws,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"test"}}}),
    );
    let init = recv(&mut ws);
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    send(&mut ws, json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    match next_event(&events) {
        ServerEvent::Initialized { client_info, .. } => assert_eq!(client_info["name"], "claude-code"),
        e => panic!("{e:?}"),
    }
    assert_eq!(server.clients().len(), 1);
    assert!(server.clients()[0].initialized);

    send(&mut ws, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let list = recv(&mut ws);
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in ["openFile", "openDiff", "getCurrentSelection", "getOpenEditors", "getDiagnostics", "close_tab"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }

    // A tool call reaches the host, which answers later.
    send(
        &mut ws,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"getWorkspaceFolders","arguments":{}}}),
    );
    let ServerEvent::ToolCall(call) = next_event(&events) else { panic!() };
    assert_eq!(call.name, "getWorkspaceFolders");
    assert_eq!(call.request, json!(3));
    // Meanwhile the connection keeps serving other requests.
    send(&mut ws, json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    assert_eq!(recv(&mut ws)["id"], 4);
    assert!(call.reply.send(ToolResult::json(&json!({"success": true}))));
    let r = recv(&mut ws);
    assert_eq!(r["id"], 3);
    assert_eq!(r["result"]["content"][0]["type"], "text");
    assert_eq!(
        serde_json::from_str::<Value>(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()["success"],
        true
    );

    // Notifications are broadcast.
    assert_eq!(server.notify("selection_changed", json!({"text": "x"})), 1);
    let n = recv(&mut ws);
    assert_eq!(n["method"], "selection_changed");
    assert!(n.get("id").is_none());

    // Stop: the lock file goes away and the client sees the connection close.
    server.stop();
    assert!(!lock_path.exists());
    let start = Instant::now();
    loop {
        match ws.read() {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => assert!(start.elapsed() < Duration::from_secs(10)),
        }
    }
    assert!(matches!(next_event(&events), ServerEvent::Disconnected { .. }));
}

#[test]
fn drop_removes_lock_file_and_disconnect_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (server, events) = start(dir.path());
    let lock_path = server.lock_path().unwrap().to_path_buf();
    let ws = connect(server.port(), Some(server.auth_token())).unwrap();
    assert!(matches!(next_event(&events), ServerEvent::Connected { .. }));
    drop(ws);
    assert!(matches!(next_event(&events), ServerEvent::Disconnected { .. }));
    assert_eq!(server.client_count(), 0);
    drop(server);
    assert!(!lock_path.exists());
}

#[cfg(unix)]
#[test]
fn start_cleans_up_stale_lock_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let stale = lockfile::LockInfo {
        pid: dead,
        workspace_folders: vec![],
        ide_name: "CodeCraft".into(),
        transport: "ws".into(),
        running_in_windows: false,
        auth_token: "old".into(),
    };
    let stale_path = lockfile::write(dir.path(), 1, &stale).unwrap();
    let (server, _events) = start(dir.path());
    assert!(!stale_path.exists());
    assert!(server.lock_path().unwrap().exists());
}

#[test]
fn ipv6_loopback_is_served_when_available() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        return; // no IPv6 loopback on this machine
    }
    let dir = tempfile::tempdir().unwrap();
    let (server, events) = start(dir.path());
    let mut ws = connect_to("::1", server.port(), Some(server.auth_token())).unwrap();
    assert!(matches!(next_event(&events), ServerEvent::Connected { .. }));
    send(&mut ws, json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
    assert_eq!(recv(&mut ws)["id"], 1);
}
