//! Running requests against a tiny HTTP server on 127.0.0.1 (never the internet).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread::JoinHandle;

use codecraft_http::runner::{RunSpec, run};
use codecraft_http::{ExecOptions, Variables, execute, history, parse, resolve};

/// What the server saw of one request.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    Some(Seen { method, path, headers, body })
}

/// Serve `n` connections; each response is chosen by the request path.
fn server(n: usize) -> (String, JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..n {
            let Ok((mut stream, _)) = listener.accept() else { break };
            let Some(req) = read_request(&mut stream) else { continue };
            let (status, ctype, body): (&str, &str, Vec<u8>) = match req.path.as_str() {
                "/json" => ("200 OK", "application/json", br#"{"ok":true,"items":[1,2]}"#.to_vec()),
                "/missing" => ("404 Not Found", "text/plain", b"nope".to_vec()),
                "/redirect" => ("302 Found", "text/plain", Vec::new()),
                "/big" => ("200 OK", "text/plain", vec![b'x'; 100_000]),
                _ => {
                    let echo = serde_json::json!({
                        "method": req.method,
                        "path": req.path,
                        "body": String::from_utf8_lossy(&req.body),
                    });
                    ("200 OK", "application/json", echo.to_string().into_bytes())
                }
            };
            let extra = if req.path == "/redirect" { "Location: /json\r\n" } else { "" };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nX-Test: a\r\nX-Test: b\r\n{extra}Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            seen.push(req);
        }
        seen
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

fn opts() -> ExecOptions {
    ExecOptions {
        connect_timeout: std::time::Duration::from_secs(5),
        read_timeout: std::time::Duration::from_secs(5),
        ..ExecOptions::default()
    }
}

#[test]
fn sends_method_headers_and_body_and_reads_the_response() {
    let (host, server) = server(3);
    let text = format!(
        "@host = {host}\n### create\nPOST http://{{{{host}}}}/echo\nContent-Type: application/json\nX-Id: {{{{id}}}}\n\n{{\"a\": \"{{{{id}}}}\"}}\n\n###\nGET http://{{{{host}}}}/missing\n\n###\nGET http://{{{{host}}}}/json\n"
    );
    let f = parse(&text);
    let mut env = std::collections::BTreeMap::new();
    env.insert("id".to_string(), "42".to_string());
    let vars = Variables::new(&f.variables, env);
    let dir = tempfile::tempdir().unwrap();

    let r = resolve(&f.requests[0], &vars, dir.path()).unwrap();
    let resp = execute(&r, &opts()).unwrap();
    assert_eq!(resp.status, 200);
    let echo: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["path"], "/echo");
    assert_eq!(echo["body"], "{\"a\": \"42\"}");
    assert!(resp.elapsed_ms < 5000);

    let resp = execute(&resolve(&f.requests[1], &vars, dir.path()).unwrap(), &opts()).unwrap();
    assert_eq!((resp.status, resp.status_text.as_str()), (404, "Not Found"));
    assert_eq!(resp.body, b"nope");
    assert_eq!(resp.status_line(), "HTTP/1.1 404 Not Found");

    let resp = execute(&resolve(&f.requests[2], &vars, dir.path()).unwrap(), &opts()).unwrap();
    assert_eq!(resp.content_type(), Some("application/json"));
    let tests: Vec<&str> =
        resp.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("x-test")).map(|(_, v)| v.as_str()).collect();
    assert_eq!(tests, vec!["a", "b"]);
    assert_eq!(
        codecraft_http::format::display_body(resp.content_type(), &resp.body, resp.body_len),
        "{\n  \"ok\": true,\n  \"items\": [\n    1,\n    2\n  ]\n}"
    );

    let seen = server.join().unwrap();
    assert_eq!(seen[0].headers.iter().find(|(k, _)| k == "x-id").map(|(_, v)| v.as_str()), Some("42"));
    assert!(seen[0].headers.iter().any(|(k, v)| k == "user-agent" && v == "CodeCraft HTTP Client"));
}

#[test]
fn redirects_are_followed_unless_disabled() {
    let (host, server) = server(3);
    let f = parse(&format!("GET http://{host}/redirect\n###\n# @no-redirect\nGET http://{host}/redirect\n"));
    let v = Variables::default();
    let resp = execute(&resolve(&f.requests[0], &v, Path::new(".")).unwrap(), &opts()).unwrap();
    assert_eq!(resp.status, 200);
    let resp = execute(&resolve(&f.requests[1], &v, Path::new(".")).unwrap(), &opts()).unwrap();
    assert_eq!(resp.status, 302);
    assert_eq!(server.join().unwrap().len(), 3);
}

#[test]
fn large_bodies_are_kept_partly_and_written_fully() {
    let (host, server) = server(2);
    let dir = tempfile::tempdir().unwrap();
    let f = parse(&format!("GET http://{host}/big\n"));
    let r = resolve(&f.requests[0], &Variables::default(), dir.path()).unwrap();
    let file = dir.path().join("big.txt");
    let o = ExecOptions { max_keep: 1000, body_file: Some(file.clone()), ..opts() };
    let resp = execute(&r, &o).unwrap();
    assert_eq!(resp.body.len(), 1000);
    assert_eq!(resp.body_len, 100_000);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 100_000);
    let o = ExecOptions { max_total: 10, ..opts() };
    let resp = execute(&r, &o).unwrap();
    assert!(resp.cut);
    assert_eq!(resp.body_len, 10);
    server.join().unwrap();
}

#[test]
fn runner_records_history_and_writes_output() {
    let (host, server) = server(2);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("http-client.env.json"), format!("{{\"dev\": {{\"host\": \"{host}\"}}}}")).unwrap();
    let f = parse("GET http://{{host}}/json\n>>! ./out.json\n###\nGET http://{{host}}/echo\n");
    let hist = history::dir(root.path());
    let spec = |i: usize| RunSpec {
        request: f.requests[i].clone(),
        file_variables: f.variables.clone(),
        env: Some("dev".into()),
        base_dir: root.path().to_path_buf(),
        root: Some(root.path().to_path_buf()),
        history_dir: Some(hist.clone()),
        options: opts(),
    };
    let r = run(&spec(0));
    assert_eq!(r.error, None);
    let resp = r.response.unwrap();
    assert_eq!(resp.status, 200);
    let body_file = resp.body_file.unwrap();
    assert!(body_file.to_string_lossy().ends_with(".200.json"));
    assert_eq!(std::fs::read(root.path().join("out.json")).unwrap(), br#"{"ok":true,"items":[1,2]}"#);
    let r = run(&spec(1));
    assert!(r.response.is_some());
    let entries = history::load(&hist);
    assert_eq!(entries.len(), 2);
    assert!(entries[0].url.ends_with("/echo"));
    assert!(std::fs::read_to_string(hist.join(history::LOG)).unwrap().contains("<> "));
    server.join().unwrap();

    // Unknown host variable: the URL stays unresolved and sending fails (no network access).
    let mut s = spec(0);
    s.env = None;
    s.request.output = None;
    let r = run(&s);
    assert!(r.error.is_some());
    assert_eq!(r.resolved.unwrap().unresolved, vec!["host".to_string()]);
    assert!(history::load(&hist)[0].error.is_some());
}

#[test]
fn connection_errors_are_errors_not_panics() {
    // A port nobody listens on.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let f = parse(&format!("GET http://127.0.0.1:{port}/\n###\nGET http://\n###\nGET http://bad host/\n"));
    for r in &f.requests {
        if let Ok(req) = resolve(r, &Variables::default(), Path::new(".")) {
            assert!(execute(&req, &opts()).is_err());
        }
    }
}
