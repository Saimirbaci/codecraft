//! The Redis key browser window driven headlessly against a `redis-server` the test starts on a
//! free port in a temp dir (skipped when it is not installed): namespaces render, a key's value
//! shows, and on a production data source Delete asks first with the exact command.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::db::MemoryStore;
use codecraft_ui_egui::App;

struct Server(Child, #[allow(dead_code)] tempfile::TempDir, u16);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server() -> Option<Server> {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].iter().map(std::path::PathBuf::from));
    let Some(bin) = dirs.into_iter().map(|d| d.join("redis-server")).find(|p| p.is_file()) else {
        eprintln!("skipping: redis-server is not installed");
        return None;
    };
    let dir = tempfile::tempdir().ok()?;
    let port = TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok()?.port();
    let child = Command::new(bin)
        .args(["--port", &port.to_string(), "--bind", "127.0.0.1", "--save", "", "--appendonly", "no"])
        .arg("--dir")
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let srv = Server(child, dir, port);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(mut c) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 7];
            if c.write_all(b"PING\r\n").is_ok() && c.read_exact(&mut buf).is_ok() && &buf == b"+PONG\r\n" {
                return Some(srv);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

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

#[test]
fn key_browser_window_shows_keys_and_confirms_on_production() {
    let Some(srv) = server() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.db.set_secret_store(Arc::new(MemoryStore::default()));
    h.state_mut().session.db.history.set_dir(root.join(".codecraft/test-history"));
    let s = &mut h.state_mut().session;
    s.execute(
        "db.addDataSource",
        &json!({ "kind": "redis", "name": "Cache", "host": "127.0.0.1", "port": srv.2.to_string(), "confirmWrites": true }),
    )
    .unwrap();
    // Reads run at once on production; seed through a non-production copy of the connection.
    s.execute("db.editDataSource", &json!({ "dataSource": "cache", "confirmWrites": false })).unwrap();
    s.execute(
        "db.execute",
        &json!({ "dataSource": "cache", "sql": "HSET session:abc user ada\nSET session:def x\nSET counter 1" }),
    )
    .unwrap();
    settle(&mut h, "seed", |a| {
        !a.session.db.busy() && a.session.db.output.iter().filter(|l| l.text.contains("SET")).count() >= 3
    });
    let s = &mut h.state_mut().session;
    s.execute("db.editDataSource", &json!({ "dataSource": "cache", "confirmWrites": true })).unwrap();
    s.execute("db.redis.open", &json!({ "dataSource": "cache" })).unwrap();
    settle(&mut h, "keys", |a| a.session.db.redis.get("cache").is_some_and(|b| !b.scanning && b.keys.len() == 3));
    h.run_steps(3);
    assert!(h.query_by_label_contains("3 keys").is_some());
    assert!(h.query_by_label_contains("session").is_some());

    // Open a key: its fields render.
    h.state_mut().session.execute("db.redis.openKey", &json!({ "key": "session:abc" })).unwrap();
    settle(&mut h, "value", |a| a.session.db.redis.get("cache").is_some_and(|b| b.value.is_some()));
    h.run_steps(3);
    assert!(h.query_by_label_contains("ada").is_some());
    assert!(h.query_by_label_contains("TTL no expiry").is_some());

    // Delete on production: the confirmation shows the command; Execute deletes.
    h.get_by_label("Delete").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("UNLINK session:abc").is_some(), "the confirmation shows the command");
    assert!(h.state().session.db.redis.get("cache").unwrap().keys.iter().any(|k| k.name == "session:abc"));
    h.get_by_label("Execute").click();
    settle(&mut h, "deleted", |a| {
        a.session.db.redis.get("cache").is_some_and(|b| !b.writing && !b.keys.iter().any(|k| k.name == "session:abc"))
    });
    h.run_steps(2);
    h.state_mut().session.shutdown();
}
