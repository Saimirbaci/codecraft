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
