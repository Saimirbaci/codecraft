//! The schema tools through engine commands (SQLite, always): Create/Modify Table with the SQL
//! preview, Drop/Rename/Truncate with confirmation and the read-only/production guards, the
//! rename-column follow-up in `.sql` files, SQL Generator/Copy DDL, the ER diagram and Compare
//! Schemas with an applied migration.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::database::schema_tools::ToolState;
use codecraft_engine::db::catalog::table_def;
use codecraft_engine::db::{Cell, Connection, DataSourceConfig, DbKind, MemoryStore, connect, query_all};
use codecraft_engine::{Effect, Session};

fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}; status: {}; output: {:?}", s.status, s.db.output);
}

fn idle(s: &mut Session, what: &str) {
    pump_until(s, what, |s| !s.db.schema.busy());
}

fn open_db(path: &Path) -> Box<dyn Connection> {
    connect(&DataSourceConfig { kind: DbKind::Sqlite, path: Some(path.to_path_buf()), ..Default::default() }, None)
        .unwrap()
}

fn make_db(path: &Path) {
    let mut c = open_db(path);
    for sql in [
        "create table authors (id integer primary key, name text not null)",
        "create table books (id integer primary key, author_id integer references authors(id) on delete cascade, \
         title text, note text)",
        "insert into authors(name) values ('Ann'), ('Bob')",
        "insert into books(author_id, title, note) values (1, 'Dune', null), (2, 'Emma', 'x')",
    ] {
        query_all(c.as_mut(), sql).unwrap();
    }
}

fn session(root: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&root.join(".home"));
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    pump_until(&mut s, "project", |s| s.ws.project.is_some() && !s.jobs_running());
    s
}

fn add(s: &mut Session, name: &str, path: &str, extra: Value) -> String {
    let mut p = json!({ "kind": "sqlite", "name": name, "path": path });
    if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    s.execute("db.addDataSource", &p).unwrap()["dataSource"].as_str().unwrap().to_string()
}

fn columns(path: &Path, table: &str) -> Vec<String> {
    let mut c = open_db(path);
    table_def(c.as_mut(), "main", table).map(|t| t.columns.into_iter().map(|c| c.name).collect()).unwrap_or_default()
}

fn count(path: &Path, sql: &str) -> i64 {
    let mut c = open_db(path);
    match query_all(c.as_mut(), sql).unwrap().1.first().and_then(|r| r.first()) {
        Some(Cell::Int(n)) => *n,
        other => panic!("{other:?}"),
    }
}

#[test]
fn create_and_modify_table_through_the_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add(&mut s, "Library", "lib.db", json!({}));

    // The dialog opens with a starter definition and a live preview.
    let r = s.execute("db.createTable", &json!({ "dataSource": id })).unwrap();
    assert!(r["sql"].as_str().unwrap().contains("CREATE TABLE \"main\".\"new_table\""), "{r}");
    let def = json!({
        "name": "loans",
        "columns": [
            { "name": "id", "dataType": "INTEGER", "nullable": false, "autoIncrement": true },
            { "name": "book_id", "dataType": "INTEGER", "nullable": false },
            { "name": "due", "dataType": "TEXT", "default": "date('now', '+14 days')" },
        ],
        "primaryKey": ["id"],
        "indexes": [{ "columns": ["due"] }],
        "foreignKeys": [{ "columns": ["book_id"], "refTable": "books", "refColumns": ["id"], "onDelete": "cascade" }],
        "checks": [{ "name": "due_set", "expression": "due <> ''" }],
    });
    let r = s.execute("db.tableEditor.update", &json!({ "definition": def })).unwrap();
    let sql = r["sql"].as_str().unwrap();
    assert!(sql.contains("\"id\" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL"), "{sql}");
    assert!(sql.contains("DEFAULT (date('now', '+14 days'))"), "{sql}");
    assert!(sql.contains("REFERENCES \"books\" (\"id\") ON DELETE CASCADE"), "{sql}");
    assert!(sql.contains("CREATE INDEX \"main\".\"loans_due_index\" ON \"loans\" (\"due\")"), "{sql}");
    // An invalid definition shows why in the preview and does not execute.
    let bad = json!({ "name": "x", "columns": [{ "name": "a", "dataType": "int; drop table books" }] });
    s.execute("db.tableEditor.update", &json!({ "definition": bad })).unwrap();
    let g = s.execute("db.tableEditor.get", &Value::Null).unwrap();
    assert!(g["error"].as_str().unwrap().contains("must not contain `;`"), "{g}");
    assert!(s.execute("db.tableEditor.execute", &Value::Null).is_err());
    s.execute("db.tableEditor.update", &json!({ "definition": def })).unwrap();
    s.execute("db.tableEditor.execute", &Value::Null).unwrap();
    idle(&mut s, "create table");
    assert!(s.db.schema.table_editor.is_none(), "the dialog closes: {}", s.status);
    assert_eq!(columns(&db, "loans"), ["id", "book_id", "due"]);
    assert_eq!(count(&db, "select count(*) from sqlite_master where name = 'loans_due_index'"), 1);

    // Headless create in one call.
    let r = s
        .execute(
            "db.createTable",
            &json!({ "dataSource": id, "execute": true, "definition": {
                "name": "tags", "columns": [{ "name": "name", "dataType": "TEXT" }] } }),
        )
        .unwrap();
    assert_eq!(r["statements"], 1);
    idle(&mut s, "headless create");
    assert_eq!(columns(&db, "tags"), ["name"]);

    // Modify Table reads the table, previews the rebuild, keeps the data.
    s.execute("db.modifyTable", &json!({ "dataSource": id, "schema": "main", "table": "books" })).unwrap();
    idle(&mut s, "read books");
    let e = s.db.schema.table_editor.as_ref().unwrap();
    assert_eq!(e.state, ToolState::Ready);
    assert_eq!(e.original.as_ref().unwrap().columns.len(), 4);
    assert_eq!(s.execute("db.tableEditor.get", &Value::Null).unwrap()["sql"], "-- No changes");
    {
        let e = s.db.schema.table_editor.as_mut().unwrap();
        e.table.columns[2].name = "book title".into();
        e.table.columns[3].data_type = "VARCHAR(40)".into();
        e.table.columns.push(codecraft_engine::db::ddl::ColumnDef {
            name: "isbn".into(),
            data_type: "TEXT".into(),
            ..Default::default()
        });
    }
    let sql = s.db.schema.table_editor.as_ref().unwrap().preview();
    assert!(sql.contains("RENAME COLUMN \"title\" TO \"book title\""), "{sql}");
    assert!(sql.contains("PRAGMA foreign_keys = OFF"), "{sql}");
    s.execute("db.tableEditor.execute", &Value::Null).unwrap();
    idle(&mut s, "modify books");
    assert!(s.db.schema.table_editor.is_none(), "{}", s.status);
    assert_eq!(columns(&db, "books"), ["id", "author_id", "book title", "note", "isbn"]);
    assert_eq!(count(&db, "select count(*) from books where \"book title\" = 'Dune'"), 1);

    // A failing change is rolled back and stays open with the exact error.
    let def = json!({ "name": "books", "columns": [
        { "name": "id", "dataType": "INTEGER", "original": "id" },
        { "name": "note", "dataType": "TEXT", "nullable": false, "original": "note" } ], "primaryKey": ["id"] });
    s.execute(
        "db.modifyTable",
        &json!({ "dataSource": id, "schema": "main", "table": "books", "definition": def, "execute": true }),
    )
    .unwrap();
    idle(&mut s, "failing modify");
    match &s.db.schema.table_editor.as_ref().unwrap().state {
        ToolState::Failed(e) => assert!(e.contains("NOT NULL") && e.contains("rolled back"), "{e}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(columns(&db, "books"), ["id", "author_id", "book title", "note", "isbn"]);
    s.execute("db.tableEditor.close", &Value::Null).unwrap();

    // Bad params fail cleanly.
    assert!(s.execute("db.tableEditor.execute", &Value::Null).is_err());
    assert!(s.execute("db.createTable", &json!({ "dataSource": id, "definition": "nope" })).is_err());
    assert!(s.execute("db.modifyTable", &json!({ "dataSource": id })).is_err());
    assert!(s.execute("db.createTable", &json!({ "dataSource": "missing" })).is_err());
    s.shutdown();
}

#[test]
fn drop_rename_truncate_with_confirmation_and_guards() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    std::fs::copy(&db, root.join("ro.db")).unwrap();
    std::fs::copy(&db, root.join("prod.db")).unwrap();
    std::fs::write(
        root.join("report.sql"),
        "select title, note from books;\nselect title from other_table;\n-- title\nselect 'title' from books b where b.title is not null;\n",
    )
    .unwrap();
    let mut s = session(&root);
    let id = add(&mut s, "Library", "lib.db", json!({}));
    let ro = add(&mut s, "Archive", "ro.db", json!({ "readOnly": true }));
    let prod = add(&mut s, "Prod", "prod.db", json!({ "confirmWrites": true }));

    // Drop asks first and shows the SQL; Cancel leaves the table alone.
    let r = s.execute("db.dropObject", &json!({ "dataSource": id, "kind": "table", "name": "authors" })).unwrap();
    assert_eq!(r["sql"], "BEGIN;\nDROP TABLE \"main\".\"authors\";\nCOMMIT;\n");
    assert!(s.db.schema.prompt.is_some());
    s.execute("db.schemaOp.cancel", &Value::Null).unwrap();
    assert_eq!(columns(&db, "authors"), ["id", "name"]);

    // Rename a column: the SQL files that use it are offered for update in a preview.
    s.execute(
        "db.renameObject",
        &json!({ "dataSource": id, "kind": "column", "table": "books", "name": "title", "newName": "headline" }),
    )
    .unwrap();
    let sql = s.execute("db.schemaOp.get", &Value::Null).unwrap()["sql"].as_str().unwrap().to_string();
    assert!(sql.contains("ALTER TABLE \"main\".\"books\" RENAME COLUMN \"title\" TO \"headline\""), "{sql}");
    s.execute("db.schemaOp.confirm", &Value::Null).unwrap();
    pump_until(&mut s, "rename + preview", |s| !s.db.schema.busy() && s.refactor.preview.is_some());
    assert_eq!(columns(&db, "books"), ["id", "author_id", "headline", "note"]);
    s.execute("refactor.preview.apply", &Value::Null).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("report.sql")).unwrap(),
        "select headline, note from books;\nselect title from other_table;\n-- title\nselect 'title' from books b where b.headline is not null;\n"
    );

    // Truncate and drop with `confirm` run at once; the tree is refreshed.
    s.execute("db.truncateTable", &json!({ "dataSource": id, "table": "books", "confirm": true })).unwrap();
    idle(&mut s, "truncate");
    assert_eq!(count(&db, "select count(*) from books"), 0);
    s.execute(
        "db.dropObject",
        &json!({ "dataSource": id, "kind": "column", "table": "books", "name": "note", "confirm": true }),
    )
    .unwrap();
    idle(&mut s, "drop column");
    assert_eq!(columns(&db, "books"), ["id", "author_id", "headline"]);
    // A failing drop reports the error in the prompt.
    s.execute("db.dropObject", &json!({ "dataSource": id, "kind": "view", "name": "nope", "confirm": true })).unwrap();
    idle(&mut s, "failing drop");
    match &s.db.schema.prompt.as_ref().unwrap().state {
        ToolState::Failed(e) => assert!(e.contains("no such view"), "{e}"),
        other => panic!("{other:?}"),
    }
    s.execute("db.schemaOp.cancel", &Value::Null).unwrap();

    // Read-only data sources refuse; production ones ask before executing table changes.
    assert!(s.execute("db.dropObject", &json!({ "dataSource": ro, "kind": "table", "name": "books" })).is_err());
    assert!(s.execute("db.createTable", &json!({ "dataSource": ro })).is_err());
    s.execute("db.createTable", &json!({ "dataSource": prod, "definition": { "name": "t", "columns": [{ "name": "a", "dataType": "INTEGER" }] } }))
        .unwrap();
    assert_eq!(s.execute("db.tableEditor.execute", &Value::Null).unwrap()["confirm"], true);
    assert_eq!(s.db.schema.table_editor.as_ref().unwrap().state, ToolState::Confirm);
    s.execute("db.tableEditor.execute", &json!({ "confirm": true })).unwrap();
    idle(&mut s, "production create");
    assert_eq!(columns(&root.join("prod.db"), "t"), ["a"]);

    // Hostile params fail cleanly.
    for p in [
        json!({ "dataSource": id, "kind": "spaceship", "name": "x" }),
        json!({ "dataSource": id, "kind": "column", "name": "x" }),
        json!({ "dataSource": id, "kind": "table" }),
        json!({ "dataSource": id, "kind": "schema", "name": "main" }),
    ] {
        assert!(s.execute("db.dropObject", &p).is_err(), "{p}");
    }
    assert!(
        s.execute(
            "db.renameObject",
            &json!({ "dataSource": id, "kind": "index", "table": "t", "name": "i", "newName": "j" })
        )
        .is_err()
    );
    assert!(s.execute("db.schemaOp.confirm", &Value::Null).is_err());
    s.shutdown();
}

#[test]
fn sql_generator_diagram_and_compare() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    open_db(&root.join("empty.db"));
    let mut s = session(&root);
    let id = add(&mut s, "Library", "lib.db", json!({}));
    let empty = add(&mut s, "Empty", "empty.db", json!({}));

    // SQL Generator and Copy DDL.
    s.execute("db.generateDdl", &json!({ "dataSource": id, "kind": "table", "name": "books" })).unwrap();
    idle(&mut s, "ddl");
    let g = s.db.schema.generated.as_ref().unwrap();
    assert!(g.visible && g.sql.starts_with("CREATE TABLE books"), "{}", g.sql);
    s.execute(
        "db.copyDdl",
        &json!({ "dataSource": id, "objects": [
        { "kind": "table", "name": "authors" }, { "kind": "schema", "name": "main" }] }),
    )
    .unwrap();
    idle(&mut s, "copy ddl");
    assert!(s.effects.iter().any(|e| matches!(e, Effect::CopyToClipboard(t) if t.contains("CREATE TABLE authors"))));
    let r = s.execute("db.generateDdl.openInConsole", &Value::Null).unwrap();
    assert!(Path::new(r["path"].as_str().unwrap()).exists());
    s.execute("db.copyDdl", &json!({ "dataSource": id, "kind": "table", "name": "missing" })).unwrap();
    idle(&mut s, "failing ddl");
    assert!(matches!(s.db.schema.generated.as_ref().unwrap().state, ToolState::Failed(_)));

    // ER diagram: nodes, edges, selection, moves, export.
    s.execute("db.diagram.show", &json!({ "dataSource": id })).unwrap();
    idle(&mut s, "diagram");
    let d = s.db.schema.diagram.as_ref().unwrap();
    assert_eq!(d.state, ToolState::Ready);
    assert_eq!(d.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["authors", "books"]);
    assert_eq!(d.edges.len(), 1);
    let books = &d.nodes[1];
    assert!(books.columns.iter().any(|c| c.name == "author_id" && c.foreign_key));
    assert!(d.nodes[0].x < books.x, "referenced tables are left");
    let r = s.execute("db.diagram.select", &json!({ "table": "books" })).unwrap();
    assert_eq!(r["related"], json!(["authors"]));
    s.execute("db.diagram.moveTable", &json!({ "table": "books", "x": 500.0, "y": 40.0 })).unwrap();
    assert_eq!(s.db.schema.diagram.as_ref().unwrap().nodes[1].x, 500.0);
    assert!(s.execute("db.diagram.moveTable", &json!({ "table": "books", "x": "far" })).is_err());
    assert!(s.execute("db.diagram.moveTable", &json!({ "table": "nope", "x": 1, "y": 1 })).is_err());
    s.execute("db.diagram.setView", &json!({ "zoom": 99, "pan": [10, 20] })).unwrap();
    assert_eq!(s.db.schema.diagram.as_ref().unwrap().zoom, 4.0);
    s.execute("db.diagram.fit", &json!({ "width": 800, "height": 600 })).unwrap();
    s.execute("db.diagram.export", &json!({ "path": "schema.svg" })).unwrap();
    assert!(std::fs::read_to_string(root.join("schema.svg")).unwrap().contains("author_id"));
    let r = s.execute("db.diagram.export", &json!({ "path": root.join("schema.png") })).unwrap();
    assert_eq!(r["pending"], true);
    assert!(s.db.schema.png_request.is_some());
    s.execute("db.diagram.show", &json!({ "dataSource": id, "tables": ["books"] })).unwrap();
    idle(&mut s, "partial diagram");
    assert_eq!(s.db.schema.diagram.as_ref().unwrap().nodes.len(), 1);

    // Compare with an empty database, apply the migration, compare again: nothing differs.
    let r = s.execute(
        "db.compareSchemas",
        &json!({ "source": { "dataSource": id, "schema": "main" }, "target": { "dataSource": empty } }),
    );
    r.unwrap();
    idle(&mut s, "compare");
    let c = s.db.schema.compare.as_ref().unwrap();
    assert_eq!(c.diff.counts(), (2, 0, 0), "{:?}", c.diff);
    assert!(c.sql.contains("CREATE TABLE \"main\".\"authors\""), "{}", c.sql);
    let r = s.execute("db.compare.openScript", &Value::Null).unwrap();
    assert!(std::fs::read_to_string(r["path"].as_str().unwrap()).unwrap().contains("CREATE TABLE"));
    s.execute("db.compare.apply", &Value::Null).unwrap();
    idle(&mut s, "apply migration");
    assert!(
        matches!(s.db.schema.compare.as_ref().unwrap().state, ToolState::Done(_)),
        "{:?}",
        s.db.schema.compare.as_ref().unwrap().state
    );
    assert_eq!(columns(&root.join("empty.db"), "books"), ["id", "author_id", "title", "note"]);
    s.execute("db.compare.refresh", &Value::Null).unwrap();
    idle(&mut s, "compare again");
    assert!(s.db.schema.compare.as_ref().unwrap().diff.is_empty());
    assert!(s.execute("db.compare.apply", &Value::Null).is_err(), "nothing to apply");
    assert!(
        s.execute("db.compareSchemas", &json!({ "source": { "dataSource": id }, "target": { "dataSource": id } }))
            .is_err()
    );
    assert!(s.execute("db.compareSchemas", &json!({ "source": { "dataSource": id } })).is_err());
    s.shutdown();
}
