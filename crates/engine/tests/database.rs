//! The database track through engine commands: data sources, the schema tree, consoles,
//! execution, paging, sorting, export, completion, cancel and persistence (SQLite, always).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::database::{ConnStatus, ResultStatus};
use codecraft_engine::db::{Cell, MemoryStore, SecretStore};
use codecraft_engine::{Popup, Session};

fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}; output: {:?}", s.db.output);
}

fn make_db(path: &Path) {
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(path.to_path_buf()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in [
        "create table authors (id integer primary key, name text not null)",
        "create table books (id integer primary key, author_id integer references authors(id), title text, note text)",
        "insert into authors(name) values ('Ann'), ('Bob')",
        "insert into books(author_id, title, note) with recursive n(i) as (select 1 union all select i + 1 from n where i < 1234) \
         select 1 + i % 2, printf('Book %04d', i), case when i % 2 = 0 then null else 'x' end from n",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
}

fn session(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    // Never write the query history to the real ~/.codecraft.
    s.db.history.set_dir(dir.join(".codecraft/test-history"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "project", |s| s.ws.project.is_some() && !s.jobs_running());
    s
}

fn add_sqlite(s: &mut Session) -> String {
    let r = s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Library", "path": "lib.db" })).unwrap();
    r["dataSource"].as_str().unwrap().to_string()
}

/// Wait for a result tab other than `prev` to finish.
fn wait_result(s: &mut Session, prev: Option<u64>) -> u64 {
    pump_until(s, "result", |s| s.db.active().is_some_and(|r| Some(r.id) != prev && !r.is_running()));
    s.db.active_result.unwrap()
}

#[test]
fn sqlite_through_commands() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    assert!(s.execute("db.addDataSource", &json!({ "kind": "oracle" })).is_err());
    assert!(s.execute("db.addDataSource", &json!({ "kind": "postgres" })).unwrap()["dialog"].as_bool().unwrap());
    s.execute("db.closeDialog", &Value::Null).unwrap();
    let id = add_sqlite(&mut s);
    assert_eq!(id, "library");

    // Saved without secrets, relative path kept.
    let saved = std::fs::read_to_string(root.join(".codecraft/datasources.json")).unwrap();
    assert!(saved.contains("\"lib.db\"") && !saved.contains("password\""), "{saved}");

    // Connect: the tree loads the default schema's tables.
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "tables", |s| {
        s.db.source("library").is_some_and(|d| d.tree.schemas.get("main").is_some_and(|n| n.tables.is_some()))
    });
    let ds = s.db.source("library").unwrap();
    assert!(matches!(&ds.status, ConnStatus::Connected(v) if v.starts_with("SQLite")));
    let tables: Vec<_> = ds.tree.schemas["main"].tables.as_ref().unwrap().iter().map(|t| t.info.name.clone()).collect();
    assert_eq!(tables, ["authors", "books"]);

    s.execute("db.expand", &json!({ "dataSource": id, "schema": "main", "table": "books", "expanded": true })).unwrap();
    s.execute("db.rowCount", &json!({ "dataSource": id, "schema": "main", "table": "books" })).unwrap();
    pump_until(&mut s, "details", |s| {
        s.db.source("library")
            .and_then(|d| d.table("main", "books"))
            .is_some_and(|t| t.details.is_some() && t.row_count.is_some())
    });
    let t = s.db.source("library").unwrap().table("main", "books").unwrap();
    assert_eq!(t.row_count, Some(1234));
    assert_eq!(t.details.as_ref().unwrap().foreign_keys[0].ref_table, "authors");

    // Open table: first page of 500, then fetch more.
    s.execute("db.openTable", &json!({ "dataSource": id, "schema": "main", "table": "books" })).unwrap();
    let rid = wait_result(&mut s, None);
    let r = s.db.result(rid).unwrap();
    assert_eq!((r.rows.len(), r.has_more, r.status), (500, true, ResultStatus::Done));
    assert_eq!(r.columns.len(), 4);
    assert_eq!(r.rows[1][3], Cell::Null);
    s.execute("db.fetchMore", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "page 2", |s| s.db.result(rid).is_some_and(|r| r.rows.len() == 1000 && !r.is_running()));
    s.execute("db.fetchMore", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "page 3", |s| {
        s.db.result(rid).is_some_and(|r| r.rows.len() == 1234 && !r.is_running() && !r.has_more)
    });
    assert!(s.execute("db.fetchMore", &json!({ "result": rid })).is_err());

    // Sort by title descending (re-query in place).
    s.execute("db.sortResult", &json!({ "result": rid, "column": 2, "desc": true })).unwrap();
    pump_until(&mut s, "sorted", |s| {
        s.db.result(rid).is_some_and(|r| r.sort.is_some() && !r.is_running() && !r.rows.is_empty())
    });
    let r = s.db.result(rid).unwrap();
    assert_eq!(r.rows[0][2], Cell::Text("Book 1234".into()));
    assert!(r.sql.contains("ORDER BY \"title\" DESC"), "{}", r.sql);

    // Export what was fetched.
    let csv = root.join("out.csv");
    s.execute("db.exportCsv", &json!({ "result": rid, "path": csv })).unwrap();
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("id,author_id,title,note\n1234,1,Book 1234,\n"), "{}", &text[..80]);

    // A console: execute the statement at the caret, then all statements.
    let r = s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    let console = Path::new(r["path"].as_str().unwrap()).to_path_buf();
    assert!(console.ends_with(".codecraft/consoles/library/console.sql"));
    s.execute("editor.type", &json!({ "text": "select count(*) from books;\nupdate books set note = 'y' where id <= 3;\nselect name from authors order by name" })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    let rid = wait_result(&mut s, Some(rid));
    let rows = &s.db.result(rid).unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[..], [vec![Cell::Text("Ann".into())], vec![Cell::Text("Bob".into())]]);
    assert_eq!(s.db.result(rid).unwrap().title, "console");
    s.execute("db.executeAll", &Value::Null).unwrap();
    pump_until(&mut s, "all", |s| !s.db.busy());
    let titles: Vec<_> =
        s.db.results.iter().filter(|r| r.owner.starts_with("console:")).map(|r| r.title.clone()).collect();
    assert_eq!(titles, ["console [1]", "console [3]"], "{:?}", s.db.output.iter().rev().take(6).collect::<Vec<_>>());
    assert!(s.db.output.iter().any(|l| l.text.starts_with("3 rows affected")), "{:?}", s.db.output);

    // Errors show up in the output.
    s.execute("db.execute", &json!({ "sql": "select * from nope" })).unwrap();
    pump_until(&mut s, "error", |s| !s.db.busy());
    assert!(s.db.output.last().unwrap().text.contains("no such table"));
    assert!(s.db.active_result.is_none());

    // Completion: tables, then columns after `alias.`.
    s.execute("editor.type", &json!({ "text": ";\nselect * from bo" })).unwrap();
    s.execute("db.complete", &Value::Null).unwrap();
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no completion popup: {}", s.status) };
    let labels: Vec<_> = c.visible.iter().map(|i| c.items[*i].label.clone()).collect();
    assert_eq!(labels.first().map(String::as_str), Some("books"), "{labels:?}");
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    s.execute("editor.type", &json!({ "text": " b where b.ti" })).unwrap();
    s.execute("db.complete", &Value::Null).unwrap();
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no column completion: {}", s.status) };
    let labels: Vec<_> = c.visible.iter().map(|i| c.items[*i].label.clone()).collect();
    // Columns of `b` only (fuzzy-matched like every completion), best match first.
    assert_eq!(labels.first().map(String::as_str), Some("title"), "{labels:?}");
    assert!(labels.iter().all(|l| ["id", "author_id", "title", "note"].contains(&l.as_str())), "{labels:?}");
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    let text = s.ws.active().unwrap().1.doc.text();
    assert!(text.ends_with("select * from books b where b.title"), "{text}");

    // Disconnect, then a fresh session loads the data source from disk.
    s.execute("db.disconnect", &json!({ "dataSource": id })).unwrap();
    assert_eq!(s.db.source("library").unwrap().status, ConnStatus::Disconnected);
    s.shutdown();
    let mut s2 = session(&root);
    s2.pump();
    assert_eq!(s2.db.sources.len(), 1);
    assert_eq!(s2.db.source_for_path(Some(&console)).as_deref(), Some("library"));
    s2.execute("db.removeDataSource", &json!({ "dataSource": "library" })).unwrap();
    assert!(s2.db.sources.is_empty());
    s2.shutdown();
}

#[test]
fn cancel_stops_a_running_query() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);
    s.execute(
        "db.execute",
        &json!({ "dataSource": id, "sql": "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c" }),
    )
    .unwrap();
    pump_until(&mut s, "running", |s| {
        s.db.active().is_some_and(|r| r.status == ResultStatus::Running) && s.db.running()
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(s.enabled("db.cancel").is_ok());
    s.execute("db.cancel", &Value::Null).unwrap();
    pump_until(&mut s, "cancelled", |s| s.db.active().is_some_and(|r| r.status == ResultStatus::Cancelled));
    pump_until(&mut s, "idle", |s| !s.db.busy());
    assert!(s.enabled("db.cancel").is_err());
    s.shutdown();
}

#[test]
fn passwords_go_to_the_secret_store_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let store = Arc::new(MemoryStore::default());
    s.db.set_secret_store(store.clone());
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "postgres", "host": "127.0.0.1", "port": 1, "user": "u", "password": "hunter2", "database": "d" }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    let saved = std::fs::read_to_string(root.join(".codecraft/datasources.json")).unwrap();
    assert!(!saved.contains("hunter2"), "{saved}");
    assert!(saved.contains("\"passwordSaved\": true") || saved.contains("\"password_saved\": true"), "{saved}");
    pump_until(&mut s, "secret", |_| store.get(&id).unwrap().is_some());
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("hunter2"));

    // Connecting to a closed port fails cleanly.
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "failure", |s| matches!(s.db.source(&id).map(|d| &d.status), Some(ConnStatus::Failed(_))));
    assert!(!s.db.busy());

    // Test Connection from params, without saving anything.
    s.execute("db.testConnection", &json!({ "kind": "sqlite", "path": "t.db" })).unwrap();
    pump_until(&mut s, "test", |s| s.status.starts_with("Connection successful"));

    // Editing without a password keeps it; removing forgets it.
    s.execute("db.editDataSource", &json!({ "dataSource": id, "name": "Renamed" })).unwrap();
    assert_eq!(s.db.source(&id).unwrap().config.name, "Renamed");
    assert!(s.db.source(&id).unwrap().config.password_saved);
    s.execute("db.removeDataSource", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "secret removed", |_| store.get(&id).unwrap().is_none());
    s.shutdown();
}

#[test]
fn hostile_params_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    for (id, p) in [
        ("db.addDataSource", json!({ "kind": "sqlite", "name": "x" })),
        ("db.addDataSource", json!({ "kind": "postgres", "host": "h", "port": 99999 })),
        ("db.addDataSource", json!({ "kind": 5 })),
        ("db.openTable", json!({ "dataSource": "x", "schema": "s", "table": "t" })),
        ("db.sortResult", json!({ "result": 99, "column": 0 })),
        ("db.exportCsv", json!({ "result": 1 })),
        ("db.newConsole", json!({})),
        ("db.expand", json!({ "dataSource": "nope" })),
        ("db.execute", Value::Null),
    ] {
        assert!(s.execute(id, &p).is_err(), "{id} {p}");
    }
    s.shutdown();
    // A corrupt datasources.json is moved aside, not overwritten.
    std::fs::create_dir_all(root.join(".codecraft")).unwrap();
    std::fs::write(root.join(".codecraft/datasources.json"), "{ not json").unwrap();
    let mut s = session(&root);
    s.pump();
    assert!(s.db.sources.is_empty());
    assert!(root.join(".codecraft/datasources.json.bad").exists());
    assert!(s.db.output.iter().any(|l| l.text.contains("is not valid")));
    s.shutdown();
}

#[test]
fn sqlite_grid_editing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);
    s.execute("db.openTable", &json!({ "dataSource": id, "schema": "main", "table": "authors" })).unwrap();
    let rid = wait_result(&mut s, None);
    pump_until(&mut s, "primary key", |s| s.db.editable(rid).is_ok());
    let changes = s.execute("db.getChanges", &json!({ "result": rid })).unwrap();
    assert_eq!((changes["editable"].as_bool(), changes["pendingRows"].as_u64()), (Some(true), Some(0)));
    assert!(s.execute("db.previewChanges", &json!({ "result": rid })).is_err());
    assert!(s.execute("db.submitChanges", &json!({ "result": rid })).is_err());

    // Edit, add, delete; an edit back to the original value is no edit.
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": "Ann O'Hara" })).unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": 1, "column": 1, "value": "Bobby" })).unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": 1, "column": 1, "value": "Bob" })).unwrap();
    let new = s.execute("db.addRow", &json!({ "result": rid })).unwrap()["row"].as_u64().unwrap();
    assert_eq!(new, 2);
    s.execute("db.editCell", &json!({ "result": rid, "row": 2, "column": 1, "value": "Cy" })).unwrap();
    let extra = s.execute("db.addRow", &json!({ "result": rid })).unwrap()["row"].as_u64().unwrap();
    s.execute("db.deleteRows", &json!({ "result": rid, "rows": [extra] })).unwrap();
    // Bad params fail cleanly.
    assert!(s.execute("db.editCell", &json!({ "result": rid, "row": 99, "column": 1, "value": "x" })).is_err());
    assert!(s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 9, "value": "x" })).is_err());
    assert!(s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": [1] })).is_err());
    assert!(s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1 })).is_err());
    assert!(s.execute("db.deleteRows", &json!({ "result": rid, "rows": "all" })).is_err());
    assert!(s.execute("db.sortResult", &json!({ "result": rid, "column": 1 })).is_err());

    let preview = s.execute("db.previewChanges", &json!({ "result": rid })).unwrap();
    let statements: Vec<&str> = preview["statements"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(
        statements,
        [
            "UPDATE \"main\".\"authors\" SET \"name\" = 'Ann O''Hara' WHERE \"id\" = 1",
            "INSERT INTO \"main\".\"authors\" (\"name\") VALUES ('Cy')",
        ]
    );
    assert!(s.db.edit_preview.is_some());

    // A failing submit (NOT NULL) keeps the changes and reports the error.
    s.execute("db.addRow", &json!({ "result": rid })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    assert!(s.db.edit_preview.is_none());
    pump_until(&mut s, "failed submit", |s| s.db.result(rid).is_some_and(|r| !r.edits.submitting));
    let r = s.db.result(rid).unwrap();
    assert!(r.edits.error.as_deref().is_some_and(|e| e.contains("NOT NULL")), "{:?}", r.edits.error);
    assert_eq!(r.edits.pending_rows(), 3);
    s.execute("db.revertChanges", &json!({ "result": rid, "rows": [3] })).unwrap();

    // Submit: one transaction, then the table reloads with the new data.
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "submitted", |s| {
        s.db.result(rid).is_some_and(|r| !r.edits.submitting && !r.is_running() && r.rows.len() == 3)
    });
    let r = s.db.result(rid).unwrap();
    assert!(r.edits.is_empty());
    assert_eq!(r.rows[0][1], Cell::Text("Ann O'Hara".into()));
    assert_eq!(r.rows[2][1], Cell::Text("Cy".into()));

    // Delete a row; NULL through the grid.
    s.execute("db.editCell", &json!({ "result": rid, "row": 1, "column": 1, "value": null })).unwrap();
    s.execute("db.revertChanges", &json!({ "result": rid })).unwrap();
    s.execute("db.deleteRows", &json!({ "result": rid, "rows": [2] })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "deleted", |s| {
        s.db.result(rid).is_some_and(|r| !r.edits.submitting && !r.is_running() && r.rows.len() == 2)
    });

    // Query results and read-only data sources are not editable.
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "select * from authors" })).unwrap();
    let q = wait_result(&mut s, Some(rid));
    assert!(s.execute("db.editCell", &json!({ "result": q, "row": 0, "column": 1, "value": "x" })).is_err());
    assert!(s.execute("db.getChanges", &json!({ "result": q })).unwrap()["reason"].as_str().is_some());
}

fn rows_of(s: &mut Session, source: &str, sql: &str) -> Vec<Vec<Cell>> {
    let prev = s.db.active_result;
    s.execute("db.execute", &json!({ "dataSource": source, "sql": sql })).unwrap();
    let rid = wait_result(s, prev);
    s.db.result(rid).unwrap().rows.clone()
}

#[test]
fn parameters_history_and_write_guards() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);

    // A console statement with a parameter asks for its value, then runs with it.
    s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    s.execute("editor.type", &json!({ "text": "select name from authors where id = :id and name <> '?'" })).unwrap();
    let r = s.execute("db.execute", &Value::Null).unwrap();
    assert_eq!(r["parameters"], json!([":id"]));
    let prompt = s.db.param_prompt.as_ref().unwrap();
    assert_eq!(prompt.params[0].value, "");
    assert!(s.execute("db.submitParameters", &Value::Null).is_err(), "empty values are refused");
    s.execute("db.submitParameters", &json!({ "values": { ":id": "2" } })).unwrap();
    assert!(s.db.param_prompt.is_none());
    let rid = wait_result(&mut s, None);
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Text("Bob".into())]]);
    // The value is remembered for the console.
    s.execute("db.execute", &Value::Null).unwrap();
    assert_eq!(s.db.param_prompt.as_ref().unwrap().params[0].value, "2");
    s.execute("db.cancelParameters", &Value::Null).unwrap();
    // Agents pass values directly; positional `?` are `?1`, `?2`.
    let rows = {
        let prev = s.db.active_result;
        s.execute("db.execute", &json!({ "dataSource": id, "sql": "select ? + ?", "params": { "?1": "40", "?2": 2 } }))
            .unwrap();
        let rid = wait_result(&mut s, prev);
        s.db.result(rid).unwrap().rows.clone()
    };
    assert_eq!(rows, vec![vec![Cell::Int(42)]]);
    assert!(s.execute("db.execute", &json!({ "dataSource": id, "sql": "select ?", "params": {} })).is_err());

    // History: newest first, persisted under the history dir, re-usable.
    let h = s.execute("db.getHistory", &json!({ "dataSource": id })).unwrap();
    let sqls: Vec<&str> = h["entries"].as_array().unwrap().iter().map(|e| e["sql"].as_str().unwrap()).collect();
    assert_eq!(sqls[0], "select ? + ?");
    assert!(sqls.contains(&"select name from authors where id = :id and name <> '?'"), "{sqls:?}");
    assert!(h["file"].as_str().unwrap().contains("test-history"));
    s.execute("db.saveQuery", &json!({ "dataSource": id, "name": "count books", "sql": "select count(*) from books" }))
        .unwrap();
    s.execute("db.showHistory", &json!({ "dataSource": id })).unwrap();
    assert!(s.db.history_popup.is_some());
    let prev = s.db.active_result;
    s.execute("db.useHistory", &json!({ "dataSource": id, "name": "count books", "run": true })).unwrap();
    assert!(s.db.history_popup.is_none());
    let rid = wait_result(&mut s, prev);
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Int(1234)]]);
    assert!(s.ws.active().unwrap().1.doc.text().ends_with("select count(*) from books"));
    assert!(s.execute("db.useHistory", &json!({ "dataSource": id, "index": 9999 })).is_err());
    assert!(s.execute("db.removeSavedQuery", &json!({ "dataSource": id, "name": "nope" })).is_err());

    // Read-only data sources refuse writes; production ones ask first.
    s.execute("db.editDataSource", &json!({ "dataSource": id, "readOnly": true })).unwrap();
    let e = s.execute("db.execute", &json!({ "dataSource": id, "sql": "delete from books" })).unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
    assert_eq!(rows_of(&mut s, &id, "select count(*) from books"), vec![vec![Cell::Int(1234)]]);
    s.execute(
        "db.editDataSource",
        &json!({ "dataSource": id, "readOnly": false, "confirmWrites": true, "color": "red" }),
    )
    .unwrap();
    let saved = std::fs::read_to_string(root.join(".codecraft/datasources.json")).unwrap();
    assert!(saved.contains("\"color\": \"red\"") && saved.contains("\"confirm_writes\": true"), "{saved}");
    assert!(s.execute("db.editDataSource", &json!({ "dataSource": id, "color": "plaid" })).is_err());
    let r = s
        .execute("db.execute", &json!({ "dataSource": id, "sql": "update authors set name = 'Al' where id = 1" }))
        .unwrap();
    assert_eq!(r["confirm"], json!(true));
    assert!(s.db.confirm.is_some());
    s.execute("db.cancelExecution", &Value::Null).unwrap();
    assert_eq!(rows_of(&mut s, &id, "select name from authors where id = 1"), vec![vec![Cell::Text("Ann".into())]]);
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "update authors set name = 'Al' where id = 1" }))
        .unwrap();
    s.execute("db.confirmExecution", &Value::Null).unwrap();
    pump_until(&mut s, "update", |s| !s.db.busy() && s.db.output.iter().any(|l| l.text.starts_with("1 row affected")));
    assert_eq!(rows_of(&mut s, &id, "select name from authors where id = 1"), vec![vec![Cell::Text("Al".into())]]);
    let listed = s.execute("db.listDataSources", &Value::Null).unwrap();
    assert_eq!(listed["dataSources"][0]["color"], "red");
    assert_eq!(listed["dataSources"][0]["confirmWrites"], true);
    s.shutdown();

    // A new session reads the history back.
    let mut s2 = session(&root);
    s2.pump();
    let h = s2.execute("db.getHistory", &json!({ "dataSource": id, "filter": "COUNT books" })).unwrap();
    assert_eq!(h["entries"].as_array().unwrap().len(), 1, "{h}");
    assert_eq!(h["saved"][0]["name"], "count books");
    s2.execute("db.clearHistory", &json!({ "dataSource": id })).unwrap();
    assert!(
        s2.execute("db.getHistory", &json!({ "dataSource": id })).unwrap()["entries"].as_array().unwrap().is_empty()
    );
    s2.shutdown();
}

#[test]
fn explain_plan_opens_a_plan_tab() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);
    s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    s.execute(
        "editor.type",
        &json!({ "text": "select * from books b join authors a on a.id = b.author_id where b.id > :min" }),
    )
    .unwrap();
    let r = s.execute("db.explainPlan", &Value::Null).unwrap();
    assert_eq!(r["parameters"], json!([":min"]));
    s.execute("db.submitParameters", &json!({ "values": { ":min": "3" } })).unwrap();
    pump_until(&mut s, "plan", |s| s.db.active().is_some_and(|r| r.plan.is_some()));
    let r = s.db.active().unwrap().clone();
    assert!(r.title.starts_with("Plan: "), "{}", r.title);
    let plan = r.plan.as_ref().unwrap();
    assert!(!plan.roots.is_empty());
    assert!(r.status_line().contains("plan steps"), "{}", r.status_line());
    let got = s.execute("db.getResult", &json!({ "result": r.id })).unwrap();
    assert!(got["plan"]["roots"].as_array().is_some_and(|a| !a.is_empty()), "{got}");
    // Copy copies the raw plan; sorting or filtering a plan is refused.
    s.execute("db.copyResult", &json!({ "result": r.id })).unwrap();
    assert!(matches!(s.effects.last(), Some(codecraft_engine::Effect::CopyToClipboard(t)) if t.contains('|')));
    assert!(s.execute("db.sortResult", &json!({ "result": r.id, "column": 0 })).is_err());
    assert!(s.execute("db.filterResult", &json!({ "result": r.id, "where": "1" })).is_err());
    // SQLite cannot analyze: the estimated plan with a note.
    s.execute("db.explainAnalyze", &json!({ "dataSource": id, "sql": "select count(*) from books" })).unwrap();
    pump_until(&mut s, "analyze", |s| {
        s.db.active().is_some_and(|x| x.id != r.id && x.plan.as_ref().is_some_and(|p| p.note.is_some()))
    });
    s.shutdown();
}

#[test]
fn manual_commit_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);
    assert!(s.execute("db.commit", &Value::Null).is_err(), "nothing to commit");
    s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    let r = s.execute("db.setTransactionMode", &json!({ "manual": true })).unwrap();
    assert_eq!(r["manual"], true);
    assert!(s.db.source(&id).unwrap().config.manual_commit);
    s.execute("editor.type", &json!({ "text": "insert into authors(name) values ('Tx')" })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "open transaction", |s| !s.db.busy() && !s.db.open_transactions(None).is_empty());
    let worker = s.db.open_transactions(None)[0].clone();
    assert!(worker.starts_with("console:"), "{worker}");
    // Another connection does not see the uncommitted row.
    assert_eq!(rows_of(&mut s, &id, "select count(*) from authors"), vec![vec![Cell::Int(2)]]);
    s.execute("db.rollback", &Value::Null).unwrap();
    pump_until(&mut s, "rolled back", |s| s.db.open_transactions(None).is_empty() && !s.db.busy());
    assert!(s.db.output.iter().any(|l| l.text == "Transaction rolled back"));
    assert_eq!(rows_of(&mut s, &id, "select count(*) from authors"), vec![vec![Cell::Int(2)]]);
    // Again, then commit.
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "open again", |s| !s.db.busy() && s.db.transaction_open(&worker));
    s.execute("db.commit", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "committed", |s| !s.db.transaction_open(&worker) && !s.db.busy());
    assert_eq!(rows_of(&mut s, &id, "select count(*) from authors"), vec![vec![Cell::Int(3)]]);
    // A typed BEGIN in auto-commit mode is tracked too.
    s.execute("db.setTransactionMode", &json!({ "manual": false })).unwrap();
    s.execute("editor.type", &json!({ "text": ";\nbegin" })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "typed begin", |s| !s.db.busy() && s.db.transaction_open(&worker));
    // Disconnecting with an open transaction asks first (track DB); answer up front.
    assert_eq!(s.execute("db.disconnect", &json!({ "dataSource": id })).unwrap()["ask"], true);
    s.execute("db.tx.resolve", &json!({ "choice": "rollback" })).unwrap();
    pump_until(&mut s, "disconnected", |s| s.db.txs.guard.is_none() && s.db.open_transactions(None).is_empty());
    assert!(s.db.output.iter().any(|l| l.text == "Transaction rolled back"));
    s.shutdown();
}

#[test]
fn grid_filter_aggregate_copy_and_export() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);
    s.execute("db.openTable", &json!({ "dataSource": id, "schema": "main", "table": "books", "where": "id <= 10" }))
        .unwrap();
    let rid = wait_result(&mut s, None);
    assert_eq!(s.db.result(rid).unwrap().rows.len(), 10);
    assert_eq!(s.db.result(rid).unwrap().filter.where_, "id <= 10");

    // The filter row re-queries in place; header sorting keeps the WHERE.
    let r = s
        .execute("db.filterResult", &json!({ "result": rid, "where": "id between 3 and 5", "orderBy": "id desc" }))
        .unwrap();
    assert!(r["sql"].as_str().unwrap().ends_with("WHERE (id between 3 and 5) ORDER BY id desc"), "{r}");
    pump_until(&mut s, "filtered", |s| s.db.result(rid).is_some_and(|r| !r.is_running() && r.rows.len() == 3));
    assert_eq!(s.db.result(rid).unwrap().rows[0][0], Cell::Int(5));
    let r = s.execute("db.sortResult", &json!({ "result": rid, "column": 0, "desc": false })).unwrap();
    let sorted_sql = r["sql"].as_str().unwrap().to_string();
    assert!(sorted_sql.ends_with("WHERE (id between 3 and 5) ORDER BY \"id\""), "{r}");
    pump_until(&mut s, "sorted", |s| {
        s.db.result(rid).is_some_and(|r| !r.is_running() && r.sql == sorted_sql && r.rows.len() == 3)
    });
    assert_eq!(s.db.result(rid).unwrap().rows[0][0], Cell::Int(3));
    assert_eq!(s.db.result(rid).unwrap().filter.order_by, "");
    // A bad filter fails in the result, not with a panic.
    s.execute("db.filterResult", &json!({ "result": rid, "where": "nope(" })).unwrap();
    pump_until(&mut s, "bad filter", |s| !s.db.busy() && s.db.output.iter().any(|l| l.text.contains("syntax error")));
    s.execute("db.filterResult", &json!({ "result": rid, "where": "", "orderBy": "" })).unwrap();
    pump_until(&mut s, "cleared", |s| s.db.result(rid).is_some_and(|r| !r.is_running() && r.rows.len() == 500));

    // Aggregates over a selection.
    let a = s.execute("db.aggregate", &json!({ "result": rid, "columns": [0], "rowRange": [0, 3] })).unwrap();
    assert_eq!(a["summary"], "Count: 4  Sum: 10  Avg: 2.5  Min: 1  Max: 4");
    assert!(s.execute("db.aggregate", &json!({ "result": rid, "rowRange": [5, 2] })).is_err());
    assert!(s.execute("db.aggregate", &json!({ "result": rid, "columns": [99] })).is_err());

    // Copy As and Export.
    s.execute("db.copyResult", &json!({ "result": rid, "format": "markdown", "rows": [0], "columns": [0, 2] }))
        .unwrap();
    match s.effects.last() {
        Some(codecraft_engine::Effect::CopyToClipboard(t)) => {
            assert_eq!(t, "| id | title |\n| --- | --- |\n| 1 | Book 0001 |\n")
        }
        other => panic!("{other:?}"),
    }
    let out = root.join("books.sql");
    s.execute("db.exportResult", &json!({ "result": rid, "path": out, "rows": [1] })).unwrap();
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "INSERT INTO \"main\".\"books\" (\"id\", \"author_id\", \"title\", \"note\") VALUES (2, 1, 'Book 0002', NULL);\n"
    );
    let out = root.join("books.json");
    s.execute("db.exportResult", &json!({ "result": rid, "path": out, "rowRange": [0, 1] })).unwrap();
    let j: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(j[1]["note"], Value::Null);
    assert!(s.execute("db.copyResult", &json!({ "result": rid, "format": "xml" })).is_err());

    // Transpose and the value viewer.
    assert_eq!(s.execute("db.transposeResult", &json!({ "result": rid })).unwrap()["transposed"], true);
    assert!(s.db.result(rid).unwrap().transposed);
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "select '{\"a\": [1]}' as j, x'89504e470d0a1a0a0000000d49484452000000020000000308' as img" }))
        .unwrap();
    let q = wait_result(&mut s, Some(rid));
    let v = s.execute("db.getCellValue", &json!({ "result": q, "row": 0, "column": 0 })).unwrap();
    assert_eq!(v["value"]["kind"], "json");
    assert_eq!(v["value"]["text"], "{\n  \"a\": [\n    1\n  ]\n}");
    let v = s.execute("db.getCellValue", &json!({ "result": q, "row": 0, "column": 1 })).unwrap();
    assert_eq!(v["value"]["kind"], "image");
    assert_eq!((v["value"]["image"]["width"].as_u64(), v["value"]["image"]["height"].as_u64()), (Some(2), Some(3)));
    assert!(s.execute("db.getCellValue", &json!({ "result": q, "row": 5, "column": 0 })).is_err());
    s.shutdown();
}

#[test]
fn import_csv_and_json_into_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s);
    std::fs::write(root.join("people.csv"), "Name,Comment\nCy,\"likes, commas\"\nDee,\n").unwrap();
    s.execute(
        "db.importData",
        &json!({ "dataSource": id, "schema": "main", "table": "authors", "path": "people.csv" }),
    )
    .unwrap();
    pump_until(&mut s, "read", |s| s.db.import.as_ref().is_some_and(|d| !d.is_busy()));
    let d = s.db.import.as_ref().unwrap();
    assert_eq!(d.state, codecraft_engine::database::ImportState::Ready, "{:?}", d.state);
    assert_eq!(d.table_columns, ["id", "name"]);
    assert_eq!(d.mapping, [None, Some(0)]);
    assert_eq!((d.total_rows, d.preview.len()), (2, 2));
    assert!(s.execute("db.setImportMapping", &json!({ "mapping": [null] })).is_err());
    assert!(s.execute("db.setImportMapping", &json!({ "mapping": { "nope": 0 } })).is_err());
    s.execute("db.setImportMapping", &json!({ "mapping": { "name": "Name" } })).unwrap();
    s.execute("db.runImport", &Value::Null).unwrap();
    pump_until(&mut s, "imported", |s| s.db.import.as_ref().is_some_and(|d| !d.is_busy()));
    assert_eq!(s.db.import.as_ref().unwrap().state, codecraft_engine::database::ImportState::Done(2));
    assert_eq!(rows_of(&mut s, &id, "select count(*) from authors"), vec![vec![Cell::Int(4)]]);

    // JSON, imported right away; a failing import (NOT NULL) inserts nothing.
    std::fs::write(root.join("more.json"), r#"[{"name": "Eve"}, {"name": null}]"#).unwrap();
    s.execute(
        "db.importData",
        &json!({ "dataSource": id, "schema": "main", "table": "authors", "path": "more.json", "run": true }),
    )
    .unwrap();
    pump_until(&mut s, "json import", |s| {
        s.db.import.as_ref().is_some_and(|d| matches!(d.state, codecraft_engine::database::ImportState::Failed(_)))
    });
    assert_eq!(rows_of(&mut s, &id, "select count(*) from authors"), vec![vec![Cell::Int(4)]]);
    s.execute("db.closeImport", &Value::Null).unwrap();
    assert!(
        s.execute(
            "db.importData",
            &json!({ "dataSource": id, "schema": "main", "table": "authors", "path": "missing.csv" })
        )
        .is_err()
    );
    // Read-only data sources cannot import.
    s.execute("db.editDataSource", &json!({ "dataSource": id, "readOnly": true })).unwrap();
    assert!(
        s.execute(
            "db.importData",
            &json!({ "dataSource": id, "schema": "main", "table": "authors", "path": "people.csv" })
        )
        .is_err()
    );
    s.shutdown();
}

#[test]
fn ssh_tunnel_is_set_through_the_data_source_params_and_form() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "postgres", "name": "Prod", "host": "db.internal", "sshTunnel": " bastion " }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    assert_eq!(s.db.source(&id).unwrap().config.ssh_tunnel.as_deref(), Some("bastion"));
    let form = codecraft_engine::database::DataSourceForm::from_config(&s.db.source(&id).unwrap().config);
    assert_eq!(form.ssh_tunnel, "bastion");
    // The form edits it as the tunnel host (a server name still resolves to that server).
    assert_eq!(form.ssh_host, "bastion");
    assert_eq!(form.to_params()["ssh"]["host"], json!("bastion"));
    assert_eq!(form.to_params()["sshTunnel"], json!(""));
    s.execute("db.editDataSource", &json!({ "dataSource": id, "sshTunnel": "" })).unwrap();
    assert_eq!(s.db.source(&id).unwrap().config.ssh_tunnel, None);
    assert!(s.execute("db.editDataSource", &json!({ "dataSource": id, "sshTunnel": 7 })).is_err());
}

#[test]
fn ssh_tunnel_with_host_user_and_password_like_datagrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let r = s
        .execute(
            "db.addDataSource",
            &json!({
                "kind": "postgres", "name": "Prod", "host": "10.0.0.5", "port": 5432,
                "ssh": { "host": "203.0.113.7", "port": "22", "user": "ops", "auth": "password" },
                "sshPassword": "hunter2",
            }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    let cfg = s.db.source(&id).unwrap().config.clone();
    let t = cfg.ssh.clone().unwrap();
    assert_eq!((t.host.as_str(), t.port, t.user.as_str()), ("203.0.113.7", Some(22), "ops"));
    assert_eq!(t.auth, codecraft_engine::db::SshAuth::Password);
    assert!(t.secret_saved);
    let saved = std::fs::read_to_string(root.join(".codecraft/datasources.json")).unwrap();
    assert!(saved.contains("203.0.113.7") && !saved.contains("hunter2"), "no secrets in the project: {saved}");

    // The form shows it and keeps the saved password when left empty.
    let form = codecraft_engine::database::DataSourceForm::from_config(&cfg);
    assert_eq!((form.ssh_host.as_str(), form.ssh_port.as_str(), form.ssh_user.as_str()), ("203.0.113.7", "22", "ops"));
    let mut params = form.to_params();
    params["dataSource"] = json!(id);
    s.execute("db.editDataSource", &params).unwrap();
    assert!(s.db.source(&id).unwrap().config.ssh.as_ref().unwrap().secret_saved);

    // `user@host:port` in the host field fills the rest; agent authentication needs no secret.
    s.execute(
        "db.editDataSource",
        &json!({ "dataSource": id, "ssh": { "host": "deploy@198.51.100.2:2200", "auth": "agent" } }),
    )
    .unwrap();
    let t = s.db.source(&id).unwrap().config.ssh.clone().unwrap();
    assert_eq!((t.host.as_str(), t.port, t.user.as_str(), t.secret_saved), ("198.51.100.2", Some(2200), "deploy", false));

    // Bad values are errors.
    for bad in [
        json!({ "host": "-oProxyCommand=x" }),
        json!({ "host": "h", "port": 0 }),
        json!({ "host": "h", "auth": "telepathy" }),
        json!({ "host": "h", "user": "a b" }),
        json!(7),
    ] {
        assert!(s.execute("db.editDataSource", &json!({ "dataSource": id, "ssh": bad })).is_err(), "{bad}");
    }
    s.execute("db.editDataSource", &json!({ "dataSource": id, "ssh": null })).unwrap();
    assert!(s.db.source(&id).unwrap().config.ssh.is_none());
}
