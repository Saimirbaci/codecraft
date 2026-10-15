//! Track DX UI driven headlessly: the savepoint menu and indicator of a console, the greyed
//! cached tree after a disconnect, the Data Tools pickers and Settings › Languages &
//! Frameworks › SQL.
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
fn savepoints_cached_tree_pickers_and_sql_settings() {
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
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe({
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
    h.run_steps(2);
    assert!(h.query_by_label_contains("Savepoint").is_some(), "the savepoint menu");
    h.state_mut().session.execute("db.tx.savepoint", &json!({ "name": "s1" })).unwrap();
    settle(&mut h, "savepoint", |a| a.session.db.txs.conns.values().any(|c| c.savepoints == ["s1"]));
    h.run_steps(2);
    assert!(h.query_by_label_contains("Savepoints (1)").is_some());
    assert!(h.query_by_label_contains("⛳ s1").is_some(), "the indicator lists the savepoint");
    h.state_mut().session.execute("db.tx.rollback", &json!({})).unwrap();
    settle(&mut h, "rolled back", |a| a.session.db.open_transactions(None).is_empty());

    // The tree stays, greyed, after a disconnect.
    h.state_mut().session.ws.layout.show(codecraft_workspace::ToolWindowId::Database);
    h.state_mut().session.execute("db.connect", &json!({ "dataSource": "lib" })).unwrap();
    settle(&mut h, "tree", |a| a.session.db.source("lib").is_some_and(|d| d.tree.info.is_some()));
    h.state_mut().session.execute("db.disconnect", &json!({ "dataSource": "lib" })).unwrap();
    h.run_steps(3);
    assert!(h.state().session.db.source("lib").unwrap().tree.stale);
    // (The greyed rows themselves are checked by the panel's unit test: rows are painted.)

    // The Data Tools export form picks schema and table from the tree.
    h.state_mut()
        .session
        .execute("db.dataTools.open", &json!({ "tool": "export", "dataSource": "lib", "table": "t" }))
        .unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Table or query").is_some());
    assert!(h.query_by_value("main").is_some(), "the schema picker shows the schema");
    assert!(h.query_by_value("t").is_some(), "the table picker shows the table");
    h.state_mut().session.execute("db.dataTools.close", &json!({})).unwrap();

    // Settings › Languages & Frameworks › SQL.
    h.state_mut().session.execute("sql.fileSettings", &json!({ "dialects": { "reports": "postgres" } })).unwrap();
    h.state_mut().session.execute("app.settings", &json!({ "page": "Languages & Frameworks › SQL" })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("SQL Dialects").is_some());
    assert!(h.query_by_label_contains("Default Data Sources").is_some());
    assert!(h.query_by_label("reports").is_some(), "the folder's entry is listed");
    assert!(h.query_by_label_contains("SQL in Rust Strings").is_some());
    h.state_mut().session.shutdown();
}
