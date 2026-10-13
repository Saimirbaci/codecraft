//! The database UI driven headlessly: Cmd+Enter executes in an SQL console, results show in
//! Services, Ctrl+Space completes table names, and neither shortcut disturbs other editors.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
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
    h.state_mut().session.db.history.set_dir(root.join(".codecraft/test-history"));
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

#[test]
fn table_grid_shows_pending_changes_and_submits_from_the_preview() {
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
    s.execute("db.openTable", &json!({ "dataSource": "lib", "schema": "main", "table": "books" })).unwrap();
    settle(&mut h, "editable table", |a| a.session.db.active().is_some_and(|r| a.session.db.editable(r.id).is_ok()));
    let rid = h.state().session.db.active_result.unwrap();

    // Edited, new and deleted rows render; the preview lists the SQL and submits it.
    let s = &mut h.state_mut().session;
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 2, "value": "edited" })).unwrap();
    let new = s.execute("db.addRow", &json!({ "result": rid })).unwrap()["row"].as_u64().unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": new, "column": 1, "value": "Odyssey" })).unwrap();
    s.execute("db.deleteRows", &json!({ "result": rid, "rows": [1] })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("3 pending rows").is_some());
    h.state_mut().session.execute("db.previewChanges", &json!({ "result": rid })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("UPDATE \"main\".\"books\" SET \"note\" = 'edited'").is_some());
    h.get_by_label("Submit").click();
    settle(&mut h, "submitted", |a| {
        a.session.db.result(rid).is_some_and(|r| r.edits.is_empty() && !r.edits.submitting && !r.is_running())
            && a.session.db.edit_preview.is_none()
    });
    settle(&mut h, "reloaded", |a| a.session.db.result(rid).is_some_and(|r| r.rows.len() == 2 && !r.is_running()));
    let r = h.state().session.db.result(rid).unwrap();
    assert_eq!(r.rows[1][1], codecraft_engine::db::Cell::Text("Odyssey".into()));
    h.run_steps(2);
    h.state_mut().session.shutdown();
}

fn app(root: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.db.set_secret_store(Arc::new(MemoryStore::default()));
    h.state_mut().session.db.history.set_dir(root.join(".codecraft/test-history"));
    h
}

#[test]
fn parameters_plan_history_and_transactions_in_the_console() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    let mut h = app(&root);
    let s = &mut h.state_mut().session;
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db", "color": "green" }))
        .unwrap();
    s.execute("db.newConsole", &json!({ "dataSource": "lib" })).unwrap();
    s.execute("editor.type", &json!({ "text": "select title from books where id = :id" })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Explain").is_some(), "the console toolbar has Explain");

    // Cmd+Enter opens the parameter dialog; typing a value and Execute runs it.
    h.key_press_modifiers(CMD, Key::Enter);
    h.run_steps(3);
    assert!(h.state().session.db.param_prompt.is_some());
    assert!(h.query_by_label_contains("Query Parameters").is_some());
    h.state_mut().session.db.param_prompt.as_mut().unwrap().params[0].value = "2".into();
    h.run_steps(1);
    h.get_by_label("Execute").click();
    settle(&mut h, "result", |a| a.session.db.active().is_some_and(|r| !r.is_running() && r.rows.len() == 1));
    assert!(h.state().session.db.param_prompt.is_none());
    h.run_steps(3);
    assert!(h.query_by_label_contains("WHERE").is_some(), "the filter row shows");

    // Explain Plan renders a tree.
    h.state_mut().session.execute("db.explainPlan", &json!({ "params": { ":id": "1" } })).unwrap();
    settle(&mut h, "plan", |a| a.session.db.active().is_some_and(|r| r.plan.is_some()));
    h.run_steps(3);
    assert!(h.query_by_label_contains("Operation").is_some());
    assert!(h.query_by_label_contains("Explain Plan").is_some());

    // The history popup lists the query; clicking it inserts it into the console.
    h.state_mut().session.execute("db.showHistory", &Value::Null).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Query History").is_some());
    h.get_by_label("select title from books where id = :id").click();
    h.run_steps(3);
    assert!(h.state().session.db.history_popup.is_none());
    let text = h.state().session.ws.active().unwrap().1.doc.text();
    assert!(text.ends_with(";\nselect title from books where id = :id"), "{text}");

    // Manual commit: the toolbar shows the pending transaction; Commit ends it.
    let s = &mut h.state_mut().session;
    s.execute("db.setTransactionMode", &json!({ "manual": true })).unwrap();
    s.execute("db.execute", &json!({ "params": { ":id": "1" } })).unwrap();
    settle(&mut h, "transaction", |a| !a.session.db.busy() && !a.session.db.open_transactions(None).is_empty());
    h.run_steps(3);
    assert!(h.query_by_label_contains("pending transaction").is_some());
    h.get_by_label("✔").click();
    settle(&mut h, "committed", |a| a.session.db.open_transactions(None).is_empty() && !a.session.db.busy());
    h.run_steps(2);
    h.state_mut().session.shutdown();
}

#[test]
fn grid_extras_value_viewer_transpose_and_import_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    std::fs::write(root.join("more.csv"), "title,note\nIliad,old\n").unwrap();
    let mut h = app(&root);
    let s = &mut h.state_mut().session;
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db", "confirmWrites": true }))
        .unwrap();
    s.execute(
        "db.execute",
        &json!({ "dataSource": "lib", "sql": "select id, title, '{\"k\": 1}' as doc from books order by id" }),
    )
    .unwrap();
    settle(&mut h, "result", |a| a.session.db.active().is_some_and(|r| !r.is_running() && r.rows.len() == 2));
    let rid = h.state().session.db.active_result.unwrap();

    // The value viewer pretty-prints JSON of the selected cell.
    h.state_mut().ui.db.value_viewer = true;
    h.run_steps(2);
    h.state_mut().ui.db.grids.get_mut(&rid).unwrap().select_cell(0, 2);
    h.run_steps(3);
    assert!(h.query_by_label_contains("JSON").is_some(), "value viewer kind");

    // Transposed (records as columns, read-only) renders, with the value viewer still mapped.
    h.state_mut().session.execute("db.transposeResult", &json!({ "result": rid })).unwrap();
    h.run_steps(2);
    h.state_mut().ui.db.grids.get_mut(&rid).unwrap().select_cell(2, 1);
    h.run_steps(3);
    assert!(h.state().session.db.result(rid).unwrap().transposed);
    assert!(h.query_by_label_contains("JSON").is_some(), "transposed (2, 1) is the doc of record 1");

    // The production guard asks before an UPDATE.
    h.state_mut()
        .session
        .execute("db.execute", &json!({ "dataSource": "lib", "sql": "update books set note = 'n' where id = 1" }))
        .unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("production data source").is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().session.db.confirm.is_none());

    // Import dialog with the mapping preview.
    h.state_mut()
        .session
        .execute(
            "db.importData",
            &json!({ "dataSource": "lib", "schema": "main", "table": "books", "path": "more.csv" }),
        )
        .unwrap();
    settle(&mut h, "import read", |a| a.session.db.import.as_ref().is_some_and(|d| !d.is_busy()));
    h.run_steps(3);
    assert!(h.query_by_label_contains("Import Data into books").is_some());
    h.get_by_label("Import").click();
    settle(&mut h, "imported", |a| {
        a.session.db.import.as_ref().is_some_and(|d| d.state == codecraft_engine::database::ImportState::Done(1))
    });
    h.run_steps(2);
    h.state_mut().session.shutdown();
}
