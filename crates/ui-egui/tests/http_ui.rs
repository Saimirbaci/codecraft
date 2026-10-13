//! HTTP Client (M11 track H) driven headlessly: a request in a `.http` file runs against a tiny
//! server on 127.0.0.1 and its response shows in Services › HTTP Requests; the Convert cURL
//! dialog opens and converts.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::services::ServicesSection;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

/// Answers one GET with a JSON body.
fn server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
        }
        let body = r#"{"greeting":"hello from the test server"}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    });
    (port, handle)
}

#[test]
fn runs_a_request_and_shows_the_response_in_services() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (port, server) = server();
    let file = root.join("api.http");
    std::fs::write(&file, format!("### hello\nGET http://127.0.0.1:{port}/hello\nAccept: application/json\n")).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.is_some());
    h.state_mut().session.open_file(&file).unwrap();
    h.run_steps(2);
    h.state_mut().session.execute("http.run", &json!({ "line": 1 })).unwrap();
    settle(&mut h, "the response", |a| !a.session.http.busy());
    server.join().unwrap();
    let s = &h.state().session;
    assert_eq!(s.services.section, ServicesSection::Http);
    assert!(s.ws.layout.is_visible(ToolWindowId::Services));
    h.run_steps(3);
    assert!(h.query_by_label_contains("HTTP/1.1 200 OK").is_some());
    assert!(h.query_by_label_contains("hello from the test server").is_some());

    // Convert cURL: the dialog opens; converting inserts the request into the active file.
    h.state_mut().session.execute("tools.httpClient.convertCURLToHttpRequest", &serde_json::Value::Null).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Paste a cURL command").is_some());
    h.state_mut()
        .session
        .execute("tools.httpClient.convertCURLToHttpRequest", &json!({ "curl": "curl -X DELETE http://127.0.0.1:1/x" }))
        .unwrap();
    h.run_steps(2);
    assert!(h.state().session.http.curl_dialog.is_none());
    let text = h.state().session.ws.active().unwrap().1.doc.text();
    assert!(text.contains("DELETE http://127.0.0.1:1/x"), "{text}");
}
