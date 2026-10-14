//! The schema tools' windows driven headlessly: Create Table (preview, + Column, Execute), the
//! Drop confirmation, the ER diagram (drawn, fitted, exported as PNG) and Compare Schemas.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::database::schema_tools::ToolState;
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
        "create table authors (id integer primary key, name text not null)",
        "create table books (id integer primary key, author_id integer references authors(id), title text)",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
}

fn tables(dir: &Path) -> Vec<String> {
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(dir.join("lib.db")),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    conn.tables("main").unwrap().into_iter().map(|t| t.name).collect()
}

#[test]
fn create_table_dialog_diagram_and_compare_windows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    std::fs::copy(root.join("lib.db"), root.join("copy.db")).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    {
        let s = &mut h.state_mut().session;
        s.isolate_home(&root.join(".home"));
        s.db.set_secret_store(Arc::new(MemoryStore::default()));
        s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": "lib.db" })).unwrap();
        s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Copy", "path": "copy.db" })).unwrap();
        s.execute("db.createTable", &json!({ "dataSource": "lib" })).unwrap();
    }
    h.run_steps(3);

    // The dialog shows the definition and the live SQL preview; + Column adds one.
    assert!(h.query_by_label_contains("Create Table in main").is_some());
    assert!(h.query_by_label_contains("CREATE TABLE \"main\".\"new_table\" (").is_some());
    h.get_by_label("+ Column").click();
    h.run_steps(2);
    let e = h.state().session.db.schema.table_editor.as_ref().unwrap();
    assert_eq!(e.table.columns.len(), 2);
    assert!(h.query_by_label_contains("\"column_2\" INTEGER").is_some());
    // (A notification toast may cover the bottom of the dialog: click through accessibility.)
    h.get_by_label("Execute").click_accesskit();
    settle(&mut h, "create", |a| a.session.db.schema.table_editor.is_none() && !a.session.db.schema.busy());
    assert!(tables(&root).contains(&"new_table".to_string()), "{}", h.state().session.status);

    // Drop asks with the SQL; the button runs it.
    h.state_mut()
        .session
        .execute("db.dropObject", &json!({ "dataSource": "lib", "kind": "table", "name": "new_table" }))
        .unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("DROP TABLE \"main\".\"new_table\";").is_some());
    h.get_by_label("Drop").click_accesskit();
    settle(&mut h, "drop", |a| a.session.db.schema.prompt.is_none() && !a.session.db.schema.busy());
    assert!(!tables(&root).contains(&"new_table".to_string()));

    // The ER diagram draws and fits itself, and exports a PNG off the UI thread.
    h.state_mut().session.execute("db.diagram.show", &json!({ "dataSource": "lib" })).unwrap();
    settle(&mut h, "diagram", |a| {
        a.session.db.schema.diagram.as_ref().is_some_and(|d| d.state == ToolState::Ready && !d.fit_pending)
    });
    h.run_steps(2);
    assert!(h.query_by_label_contains("Diagram: Lib: main").is_some());
    assert!(h.query_by_label_contains("2 tables · 1 references").is_some());
    let png = root.join("diagram.png");
    h.state_mut().session.execute("db.diagram.export", &json!({ "path": png })).unwrap();
    settle(&mut h, "png", |a| a.session.status.starts_with("Diagram saved"));
    let bytes = std::fs::read(&png).unwrap();
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));

    // Compare Schemas: the same schema in two files shows no difference.
    h.state_mut()
        .session
        .execute(
            "db.compareSchemas",
            &json!({ "source": { "dataSource": "lib", "schema": "main" }, "target": { "dataSource": "copy" } }),
        )
        .unwrap();
    settle(&mut h, "compare", |a| a.session.db.schema.compare.as_ref().is_some_and(|c| c.state == ToolState::Ready));
    h.run_steps(2);
    assert!(h.query_all_by_label_contains("The schemas are the same").next().is_some());
    h.state_mut().session.execute("db.diagram.close", &Value::Null).unwrap();
    h.state_mut().session.execute("db.compare.close", &Value::Null).unwrap();
    h.run_steps(2);
    h.state_mut().session.shutdown();
}
