//! The database UI driven headlessly: Cmd+Enter executes in an SQL console, results show in
//! Services, Ctrl+Space completes table names, and neither shortcut disturbs other editors.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use serde_json::{Value, json};

use codecraft_engine::Popup;
use codecraft_engine::db::MemoryStore;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };
const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: false };

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
        "create table books (id integer primary key, title text, note text)",
        "insert into books(title, note) values ('Dune', null), ('Emma', 'x')",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
}

#[test]
fn console_executes_with_cmd_enter_and_completes_with_ctrl_space() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.db.set_secret_store(Arc::new(MemoryStore::default()));
    let s = &mut h.state_mut().session;
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db" })).unwrap();
    s.execute("db.newConsole", &json!({ "dataSource": "lib" })).unwrap();
    s.execute("editor.type", &json!({ "text": "select title, note from books order by id" })).unwrap();
    h.run_steps(2);

    // Cmd+Enter runs the statement at the caret; the grid shows in Services.
    h.key_press_modifiers(CMD, Key::Enter);
    settle(&mut h, "result", |a| a.session.db.active().is_some_and(|r| !r.is_running() && r.rows.len() == 2));
    let text = h.state().session.ws.active().unwrap().1.doc.text();
    assert_eq!(text, "select title, note from books order by id", "Cmd+Enter must not insert a newline");
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Services));
    h.run_steps(3);

    // Ctrl+Space in the console completes table names.
    h.state_mut().session.execute("editor.type", &json!({ "text": ";\nselect * from bo" })).unwrap();
    h.run_steps(1);
    h.key_press_modifiers(CTRL, Key::Space);
    h.run_steps(1);
    match &h.state().session.popup {
        Some(Popup::Completion(c)) => assert!(c.visible.iter().any(|i| c.items[*i].label == "books")),
        other => panic!("no completion: {other:?} / {}", h.state().session.status),
    }
    h.key_press(Key::Escape);
    h.run_steps(1);

    // In a Rust file Cmd+Enter does nothing to the text and Ctrl+Space is basic completion.
    h.state_mut().session.execute("file.open", &json!({ "path": root.join("main.rs") })).unwrap();
    h.run_steps(2);
    h.key_press_modifiers(CMD, Key::Enter);
    h.key_press_modifiers(CTRL, Key::Space);
    h.run_steps(2);
    assert_eq!(h.state().session.ws.active().unwrap().1.doc.text(), "fn main() {}\n");
    assert!(!matches!(h.state().session.popup, Some(Popup::Completion(_))));

    // The Database tool window and the dialog render without trouble.
    let s = &mut h.state_mut().session;
    s.execute("view.toolWindows.database", &Value::Null).unwrap();
    s.execute("db.expand", &json!({ "dataSource": "lib", "expanded": true })).unwrap();
    s.execute("db.addDataSource", &Value::Null).unwrap();
    h.run_steps(5);
    assert!(h.state().session.db.dialog.is_some());
    h.state_mut().session.execute("db.closeDialog", &Value::Null).unwrap();
    h.run_steps(2);
    h.state_mut().session.shutdown();
}
