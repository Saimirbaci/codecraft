//! `update.check` and `update.install` against a local release server: a signed payload is
//! downloaded, verified and staged in the version store; bad releases change nothing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use codecraft_engine::Session;
use codecraft_selfupdate::Store;
use codecraft_update as upd;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn serve() -> (String, Files) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let files: Files = Arc::default();
    let shared = files.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
            let body = shared.lock().unwrap().get(&path).cloned();
            let _ = match body {
                Some(b) => {
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                    stream.write_all(&b)
                }
                None => write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            };
        }
    });
    (base, files)
}

struct Fixture {
    session: Session,
    home: tempfile::TempDir,
    files: Files,
    base: String,
    secret: String,
}

fn publish(f: &Fixture, version: &str, secret: &str) {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join(if cfg!(windows) { "codecraft.exe" } else { "codecraft" }), "#!/bin/sh\n").unwrap();
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("payload.tar.gz");
    upd::pack(src.path(), &archive).unwrap();
    let bytes = std::fs::read(&archive).unwrap();
    let platform = upd::platform();
    let name = upd::payload_name(version, &platform);
    f.files.lock().unwrap().insert(format!("/download/v{version}/{name}"), bytes.clone());
    let manifest = upd::Manifest {
        format: upd::FORMAT,
        version: version.into(),
        commit: "abcdef123456".into(),
        platform: platform.clone(),
        url: format!("{}/download/v{version}/{name}", f.base),
        sha256: upd::sha256_file(&archive).unwrap(),
        size: bytes.len() as u64,
        min_launcher: String::new(),
        notes: String::new(),
    };
    let body = serde_json::to_vec_pretty(&manifest).unwrap();
    let key = upd::manifest_name(&platform);
    f.files.lock().unwrap().insert(format!("/latest/{key}"), body.clone());
    f.files.lock().unwrap().insert(format!("/latest/{key}.sig"), upd::sign(secret, &body).unwrap().into_bytes());
}

fn fixture() -> Fixture {
    let (base, files) = serve();
    let secret = upd::generate_secret().unwrap();
    let public = {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.decode(upd::public_key(&secret).unwrap()).unwrap();
        raw.try_into().unwrap()
    };
    let home = tempfile::tempdir().unwrap();
    let mut session = Session::new();
    session.isolate_home(home.path());
    session.versions.base = Some(home.path().join("codecraft-home"));
    session.app_update.allow_http = true;
    session.app_update.config =
        upd::Config { base_url: format!("{base}/latest"), asset_prefix: format!("{base}/download/"), public_key: public };
    Fixture { session, home, files, base, secret }
}

fn settle(s: &mut Session) {
    let deadline = Instant::now() + Duration::from_secs(20);
    s.pump();
    while s.app_update.busy && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        s.pump();
    }
    assert!(!s.app_update.busy, "update worker did not finish");
}

fn last_notification(s: &Session) -> String {
    s.ws.notifications.last().map(|n| format!("{}: {}", n.title, n.message)).unwrap_or_default()
}

#[test]
fn a_newer_signed_release_is_staged_and_offered_for_restart() {
    let mut f = fixture();
    publish(&f, "0.99.0", &f.secret.clone());
    f.session.execute("update.check", &serde_json::Value::Null).unwrap();
    settle(&mut f.session);
    assert_eq!(f.session.app_update.available.as_ref().map(|m| m.version.as_str()), Some("0.99.0"));
    f.session.execute("update.install", &serde_json::Value::Null).unwrap();
    settle(&mut f.session);
    let id = f.session.selfupdate.ready.clone().expect("ready to restart");
    assert!(id.starts_with("0.99.0-abcdef123456-"), "{id}");
    let store = Store::new(f.home.path().join("codecraft-home"), Default::default());
    let meta = store.meta(&id).unwrap();
    assert_eq!((meta.semver.as_str(), meta.source_branch.as_str()), ("0.99.0", "release"));
    assert!(store.executable(&id).unwrap().is_file());
    assert!(!f.home.path().join("codecraft-home/update-tmp").exists(), "scratch space is removed");
    assert!(last_notification(&f.session).contains("Update ready"), "{}", last_notification(&f.session));
}

#[test]
fn the_current_version_is_up_to_date_and_stages_nothing() {
    let mut f = fixture();
    publish(&f, env!("CARGO_PKG_VERSION"), &f.secret.clone());
    f.session.execute("update.install", &serde_json::Value::Null).unwrap();
    settle(&mut f.session);
    assert!(f.session.selfupdate.ready.is_none());
    assert!(last_notification(&f.session).contains("up to date"));
    assert!(Store::new(f.home.path().join("codecraft-home"), Default::default()).list().is_empty());
}

#[test]
fn a_release_signed_by_someone_else_is_refused() {
    let mut f = fixture();
    publish(&f, "0.99.0", &upd::generate_secret().unwrap());
    f.session.execute("update.install", &serde_json::Value::Null).unwrap();
    settle(&mut f.session);
    assert!(f.session.selfupdate.ready.is_none());
    assert!(last_notification(&f.session).contains("Update failed"), "{}", last_notification(&f.session));
    assert!(Store::new(f.home.path().join("codecraft-home"), Default::default()).list().is_empty());
}

#[test]
fn a_manual_check_reports_a_network_failure_and_a_second_call_while_busy_is_refused() {
    let mut f = fixture();
    f.session.app_update.config.base_url = format!("{}/nothing", f.base);
    f.session.execute("update.check", &serde_json::Value::Null).unwrap();
    assert!(f.session.execute("update.check", &serde_json::Value::Null).is_err(), "one operation at a time");
    settle(&mut f.session);
    assert!(last_notification(&f.session).contains("Update check failed"));
}

#[test]
fn install_needs_a_launcher_managed_app() {
    let mut f = fixture();
    f.session.selfupdate.isolated = false;
    // A test process normally has no launcher; skip if someone runs the suite inside one.
    if std::env::var_os("CODECRAFT_LAUNCHER").is_none() {
        let err = f.session.execute("update.install", &serde_json::Value::Null).err().unwrap();
        assert!(err.to_string().contains("launcher"), "{err}");
    }
}
