//! Track DB UI driven headlessly: the console's transaction indicator, the status-bar widget,
//! the pending-transaction question on quit (Rollback ends the transaction, then quits) and
//! the Data Tools window.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::db::MemoryStore;
use codecraft_ui_egui::App;

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
fn quit_with_an_open_transaction_asks_and_rollback_quits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    {
        let cfg = codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(root.join("lib.db")),
            ..Default::default()
        };
        let mut c = codecraft_engine::db::connect(&cfg, None).unwrap();
        codecraft_engine::db::query_all(c.as_mut(), "create table t (id integer primary key, v text)").unwrap();
    }
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    let s = &mut h.state_mut().session;
    s.isolate_home(&root.join(".home"));
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db" })).unwrap();
    s.execute("db.newConsole", &json!({ "dataSource": "lib" })).unwrap();
    s.execute("db.tx.setMode", &json!({ "mode": "manual" })).unwrap();
    s.execute("editor.type", &json!({ "text": "insert into t(v) values ('x')" })).unwrap();
    s.execute("db.execute", &json!({})).unwrap();
    settle(&mut h, "transaction", |a| !a.session.db.running() && !a.session.db.open_transactions(None).is_empty());
    h.run_steps(3);
    // The console shows the open transaction; the status bar too.
    assert!(h.query_by_label_contains("pending transaction · 1 stmt").is_some());
    assert!(h.query_by_label_contains("● Tx 1").is_some());

    // Quit asks first.
    h.state_mut().session.effects.clear();
    h.state_mut().session.execute("app.quitRustRover", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Uncommitted Transactions").is_some());
    assert!(h.query_by_label_contains("Commit or roll back before: quit").is_some());
    h.get_by_label("Rollback").click();
    settle(&mut h, "rolled back", |a| {
        a.session.db.open_transactions(None).is_empty() && a.session.db.txs.guard.is_none()
    });
    let rows = {
        let cfg = codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(root.join("lib.db")),
            ..Default::default()
        };
        let mut c = codecraft_engine::db::connect(&cfg, None).unwrap();
        codecraft_engine::db::query_all(c.as_mut(), "select count(*) from t").unwrap().1
    };
    assert_eq!(rows, vec![vec![codecraft_engine::db::Cell::Int(0)]]);

    // The Data Tools window opens with the tool's form.
    h.state_mut()
        .session
        .execute("db.dataTools.open", &json!({ "tool": "export", "dataSource": "lib", "table": "t" }))
        .unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Data Tools").is_some());
    assert!(h.query_by_label_contains("Output file").is_some());
    h.state_mut().session.shutdown();
}
