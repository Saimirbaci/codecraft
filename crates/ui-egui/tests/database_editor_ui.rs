//! Track DC's UI driven headlessly: Navigate › Database Object (⌥⇧⌘O, type, Enter selects the
//! table in the Database tool window) and Run 'file.sql' from an attached file's toolbar with
//! the execution log in Services.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::database::{ScriptPhase, StmtStatus, node_key};
use codecraft_engine::db::MemoryStore;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

const ALT_SHIFT_CMD: Modifiers = Modifiers { alt: true, ctrl: false, shift: true, mac_cmd: true, command: true };

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
    for sql in [
        "create table authors (id integer primary key, name text)",
        "create table books (id integer primary key, title text)",
        "insert into books(title) values ('Dune'), ('Emma')",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    std::fs::write(
        dir.join("script.sql"),
        "select title from books;\ninsert into nope values (1);\nselect name from authors;\n",
    )
    .unwrap();
}

#[test]
fn goto_database_object_popup_and_run_attached_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    {
        let s = &mut h.state_mut().session;
        s.isolate_home(&root.join(".home"));
        s.db.set_secret_store(Arc::new(MemoryStore::default()));
        s.db.history.set_dir(root.join(".codecraft/test-history"));
        s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db" })).unwrap();
        s.execute("db.connect", &json!({ "dataSource": "lib" })).unwrap();
    }
    settle(&mut h, "schema", |a| {
        a.session.db.source("lib").and_then(|d| d.tree.schemas.get("main")).is_some_and(|n| n.tables.is_some())
    });

    // ⌥⇧⌘O opens the popup; typing filters; Enter selects the table in the tree.
    h.key_press_modifiers(ALT_SHIFT_CMD, Key::O);
    h.run_steps(2);
    assert!(h.state().session.db.nav.goto.is_some(), "{}", h.state().session.status);
    h.event(egui::Event::Text("auth".into()));
    h.run_steps(2);
    assert_eq!(h.state().session.db.nav.goto.as_ref().unwrap().query, "auth");
    h.key_press(Key::Enter);
    h.run_steps(3);
    let s = &h.state().session;
    assert!(s.db.nav.goto.is_none());
    assert_eq!(s.db.selected.as_deref(), Some(node_key("lib", Some("main"), Some("authors")).as_str()));
    assert!(s.ws.layout.is_visible(ToolWindowId::Database));

    // The attached file's toolbar runs the whole file; the log shows each statement.
    h.state_mut().session.execute("file.open", &json!({ "path": root.join("script.sql") })).unwrap();
    h.state_mut().session.execute("db.setScriptMode", &json!({ "mode": "continue" })).unwrap();
    h.run_steps(3);
    h.get_by_label_contains("Run 'script.sql'").click();
    settle(&mut h, "script", |a| {
        a.session.db.scripts.runs.last().is_some_and(|r| r.state == ScriptPhase::Done) && !a.session.db.busy()
    });
    let run = h.state().session.db.scripts.runs.last().unwrap().clone();
    let states: Vec<bool> = run.statements.iter().map(|s| matches!(s.status, StmtStatus::Done { .. })).collect();
    assert_eq!(states, [true, false, true]);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Services));
    // Show the execution log tab.
    h.state_mut().session.execute("db.showScriptLog", &json!({ "script": run.id })).unwrap();
    h.run_steps(3);
    let _ = h.get_by_label_contains("no such table");
    h.state_mut().session.shutdown();
}
