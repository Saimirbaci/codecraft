//! Track F's windows driven headlessly: the Database Changes tool window (pending grid edits with
//! Revert), Show Hidden Tabs (click a tab to select it), the Processes window and the Compare
//! With diff.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::db::MemoryStore;
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

fn project(dir: &Path) {
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(dir.join("lib.db")),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in ["create table books (id integer primary key, title text)", "insert into books(title) values ('Dune')"] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();
}

#[test]
fn database_changes_hidden_tabs_processes_and_compare() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));

    // Database Changes lists the pending UPDATE; Revert drops it.
    h.state_mut().session.db.set_secret_store(Arc::new(MemoryStore::default()));
    let s = &mut h.state_mut().session;
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db" })).unwrap();
    s.execute("db.openTable", &json!({ "dataSource": "lib", "schema": "main", "table": "books" })).unwrap();
    settle(&mut h, "editable table", |a| a.session.db.active().is_some_and(|r| a.session.db.editable(r.id).is_ok()));
    let rid = h.state().session.db.active_result.unwrap();
    let s = &mut h.state_mut().session;
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": "Emma" })).unwrap();
    s.execute("view.toolWindows.databaseChanges", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::DatabaseChanges));
    assert!(h.query_by_label_contains("1 table(s) with pending changes").is_some());
    assert!(h.query_by_label_contains("SET \"title\" = ?1").is_some());
    assert!(h.query_by_label_contains("?1 = 'Emma'").is_some());
    h.get_by_label("Revert").click();
    h.run_steps(3);
    assert!(h.state().session.db.result(rid).is_some_and(|r| r.edits.is_empty()));
    assert!(h.query_by_label_contains("No pending changes").is_some());

    // Show Hidden Tabs lists the group's tabs; a click selects one.
    let s = &mut h.state_mut().session;
    s.execute("file.open", &json!({ "path": root.join("a.rs") })).unwrap();
    s.execute("projectView.openPreview", &json!({ "path": root.join("b.rs") })).unwrap();
    s.execute("window.editorTabs.showHiddenTabs", &json!({})).unwrap();
    h.run_steps(3);
    h.get_by_label("a.rs — a.rs").click();
    h.run_steps(3);
    let s = &h.state().session;
    assert!(!s.misc.hidden_tabs);
    assert_eq!(s.ws.active().map(|(_, d)| d.title()), Some("a.rs".to_string()));

    // The Processes window, then Escape closes it.
    h.state_mut().session.execute("run.stopBackgroundProcesses", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Nothing is running.").is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().session.misc.processes.is_none());

    // Compare With shows the diff of two files.
    h.state_mut().session.execute("view.compareWith", &json!({ "path": root.join("b.rs") })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("a.rs vs b.rs").is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().session.misc.compare.is_none());
    h.state_mut().session.shutdown();
}
