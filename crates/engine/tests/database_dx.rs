//! Track DX through engine commands: savepoints, the server-side statement timeout, the
//! pending-transaction guard on opening another project and closing a project window, the
//! metadata cache across disconnects, Run from Caret / Run Selection, the Data Tools pickers,
//! SQL dialects and data sources for files, SQLite attached databases, DDL tab cleanup, Redis
//! consoles without SQL analysis and column alias rename. SQLite always; PostgreSQL in a
//! throwaway schema when a local server accepts connections. Nothing touches the real home.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::database::tx::GuardState;
use codecraft_engine::db::{Cell, DataSourceConfig, DbKind, MemoryStore};
use codecraft_engine::{Popup, Session};

fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(40) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}; output: {:?}", s.db.output);
}

fn session(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&dir.join(".home"));
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "project", |s| s.ws.project.is_some() && !s.jobs_running());
    s
}

fn sqlite_cfg(path: &Path) -> DataSourceConfig {
    DataSourceConfig { kind: DbKind::Sqlite, path: Some(path.to_path_buf()), ..Default::default() }
}

fn make_db(path: &Path) {
    let mut c = codecraft_engine::db::connect(&sqlite_cfg(path), None).unwrap();
    for sql in [
        "create table authors (id integer primary key, name text not null, born int)",
        "insert into authors(name, born) values ('Ann', 1950), ('Bob', 1960)",
        "create table t (a int)",
    ] {
        codecraft_engine::db::query_all(c.as_mut(), sql).unwrap();
    }
}

fn committed(path: &Path, sql: &str) -> Vec<Vec<Cell>> {
    let mut c = codecraft_engine::db::connect(&sqlite_cfg(path), None).unwrap();
    codecraft_engine::db::query_all(c.as_mut(), sql).unwrap().1
}

fn add_sqlite(s: &mut Session, name: &str, file: &str) -> String {
    let r = s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": name, "path": file })).unwrap();
    r["dataSource"].as_str().unwrap().to_string()
}

fn run(s: &mut Session, source: &str, sql: &str) {
    s.execute("db.execute", &json!({ "dataSource": source, "sql": sql })).unwrap();
    pump_until(s, "run", |s| !s.db.running());
}

fn savepoints(s: &Session, worker: &str) -> Vec<String> {
    s.db.txs.conn(worker).map(|c| c.savepoints.clone()).unwrap_or_default()
}

fn connect(s: &mut Session, id: &str) {
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(s, "connected", |s| {
        s.db.source(id).is_some_and(|d| {
            d.tree.info.is_some() && !d.tree.stale && d.tree.schemas.values().any(|n| n.tables.is_some())
        })
    });
}

#[test]
fn sqlite_savepoints_set_roll_back_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    let w = format!("query:{id}");
    // Without a transaction there is nothing to save.
    run(&mut s, &id, "select 1");
    assert!(s.execute("db.tx.savepoint", &json!({ "connection": w })).is_err());

    run(&mut s, &id, "begin; insert into authors(name) values ('A1')");
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&w));
    s.execute("db.tx.savepoint", &json!({ "connection": w, "name": "s1" })).unwrap();
    pump_until(&mut s, "s1", |s| savepoints(s, &w) == ["s1"]);
    run(&mut s, &id, "insert into authors(name) values ('A2')");
    // The latest savepoint by default.
    s.execute("db.tx.rollbackToSavepoint", &json!({ "connection": w })).unwrap();
    pump_until(&mut s, "rolled back to s1", |s| s.db.output.iter().any(|l| l.text.contains("ROLLBACK TO SAVEPOINT")));
    assert_eq!(savepoints(&s, &w), ["s1"], "rolling back keeps the savepoint");
    assert!(s.db.transaction_open(&w), "the transaction stays open");
    // A savepoint typed in the console is tracked too; the default name is the next free one.
    run(&mut s, &id, "savepoint typed");
    pump_until(&mut s, "typed", |s| savepoints(s, &w) == ["s1", "typed"]);
    s.execute("db.tx.savepoint", &json!({ "connection": w })).unwrap();
    pump_until(&mut s, "sp3", |s| savepoints(s, &w) == ["s1", "typed", "sp3"]);
    assert!(s.execute("db.tx.releaseSavepoint", &json!({ "connection": w, "name": "nope" })).is_err());
    assert!(s.execute("db.tx.savepoint", &json!({ "connection": w, "name": "" })).is_ok(), "empty: a generated name");
    pump_until(&mut s, "sp4", |s| savepoints(s, &w).len() == 4);
    s.execute("db.tx.releaseSavepoint", &json!({ "connection": w, "name": "s1" })).unwrap();
    pump_until(&mut s, "released", |s| savepoints(s, &w).is_empty());
    let pending = s.execute("db.tx.pending", &json!({})).unwrap();
    assert_eq!(pending["transactions"][0]["connection"], w.as_str());
    s.execute("db.tx.commit", &json!({ "connection": w })).unwrap();
    pump_until(&mut s, "committed", |s| !s.db.transaction_open(&w));
    assert!(savepoints(&s, &w).is_empty());
    let names = committed(&db, "select name from authors where name in ('A1', 'A2') order by name");
    assert_eq!(names, vec![vec![Cell::Text("A1".into())]], "A2 was rolled back, A1 committed");
    s.shutdown();
}

#[test]
fn opening_another_project_and_closing_a_window_ask_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let other = root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    let w = format!("query:{id}");
    run(&mut s, &id, "begin; insert into t values (7)");
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&w));

    // Opening a file of the project is not guarded; another project is.
    let r = s.execute("file.open", &json!({ "path": other })).unwrap();
    assert_eq!(r["action"], "Open Project", "{r}");
    assert_eq!(s.ws.project.as_ref().unwrap().root, root, "still the same project");
    s.execute("db.tx.resolve", &json!({ "choice": "cancel" })).unwrap();
    assert!(s.db.transaction_open(&w));

    // Closing a project window asks; once committed the window may close.
    assert!(codecraft_engine::database::tx::close_window_blocked(&mut s));
    assert_eq!(s.db.txs.guard.as_ref().unwrap().action, "Close Window");
    assert_eq!(s.db.txs.guard.as_ref().unwrap().state, GuardState::Asking);
    assert!(!s.db.txs.take_close_window());
    s.execute("db.tx.resolve", &json!({ "choice": "commit" })).unwrap();
    pump_until(&mut s, "resolved", |s| s.db.txs.guard.is_none());
    assert!(s.db.txs.take_close_window(), "the window may close");
    assert!(!s.db.txs.take_close_window(), "reading resets it");
    assert_eq!(committed(&db, "select count(*) from t"), vec![vec![Cell::Int(1)]]);
    assert!(!codecraft_engine::database::tx::close_window_blocked(&mut s), "nothing open: closes at once");

    // Open another project after rolling back.
    run(&mut s, &id, "begin; insert into t values (8)");
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&w));
    s.execute("file.open", &json!({ "path": other, "transactions": "rollback" })).unwrap();
    pump_until(&mut s, "switched", |s| s.ws.project.as_ref().is_some_and(|p| p.root == other));
    assert_eq!(committed(&db, "select count(*) from t"), vec![vec![Cell::Int(1)]]);
    s.shutdown();
}

#[test]
fn metadata_is_cached_across_disconnects_and_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    connect(&mut s, &id);
    s.execute("db.expand", &json!({ "dataSource": id, "schema": "main", "table": "authors", "expanded": true }))
        .unwrap();
    pump_until(&mut s, "details", |s| s.db.source(&id).unwrap().table("main", "authors").unwrap().details.is_some());
    s.execute("db.disconnect", &json!({ "dataSource": id })).unwrap();
    let ds = s.db.source(&id).unwrap();
    assert!(ds.tree.stale, "kept, stale");
    assert!(ds.table("main", "authors").unwrap().details.is_some());
    let file = root.join(".codecraft/db-cache").join(format!("{id}.json"));
    pump_until(&mut s, "cache file", |_| file.is_file());
    assert!(root.join(".codecraft/db-cache/.gitignore").is_file());
    s.shutdown();

    // A new session shows the cached tree without connecting.
    let mut s = session(&root);
    pump_until(&mut s, "cache loaded", |s| s.db.source(&id).is_some_and(|d| d.tree.stale && d.tree.info.is_some()));
    assert!(s.db.connections_of(&id).is_empty(), "not connected");
    // Expanding the cached tree does not connect.
    s.execute("db.expand", &json!({ "dataSource": id, "expanded": true })).unwrap();
    s.execute("db.expand", &json!({ "dataSource": id, "schema": "main", "expanded": true })).unwrap();
    assert!(s.db.connections_of(&id).is_empty());
    // Completion and Database Object work offline.
    let q = root.join("q.sql");
    std::fs::write(&q, "select * from auth").unwrap();
    s.execute("file.open", &json!({ "path": q })).unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": 18, "head": 18 })).unwrap();
    s.execute("db.complete", &json!({})).unwrap();
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no completion: {}", s.status) };
    assert!(c.items.iter().any(|i| i.label == "authors"), "{:?}", c.items.iter().map(|i| &i.label).collect::<Vec<_>>());
    s.popup = None;
    let objs = s.execute("db.gotoObjectResults", &json!({ "query": "authors" })).unwrap();
    assert!(objs["objects"].as_array().unwrap().iter().any(|o| o["name"] == "authors"), "{objs}");
    assert!(s.db.connections_of(&id).is_empty(), "still offline");
    // Connecting refreshes it.
    connect(&mut s, &id);
    pump_until(&mut s, "details refreshed", |s| {
        s.db.source(&id).unwrap().table("main", "authors").is_some_and(|t| t.details.is_some())
    });
    // Another file: the cache is forgotten.
    make_db(&root.join("other.db"));
    s.execute("db.editDataSource", &json!({ "dataSource": id, "path": "other.db" })).unwrap();
    pump_until(&mut s, "forgotten", |_| !file.is_file());
    s.shutdown();
}

#[test]
fn run_from_caret_and_selection_as_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    let f = root.join("load.sql");
    std::fs::write(&f, "insert into t values (1);\ninsert into t values (2);\n\ninsert into t values (3);\n").unwrap();
    s.execute("file.open", &json!({ "path": f })).unwrap();
    s.execute("db.chooseFileDataSource", &json!({ "dataSource": id })).unwrap();
    // The caret in the second statement: statements 2 and 3 run.
    s.execute("editor.setSelection", &json!({ "anchor": 30, "head": 30 })).unwrap();
    let r = s.execute("db.runFromCaret", &json!({})).unwrap();
    assert_eq!(r["statements"], 2, "{r}");
    pump_until(&mut s, "script", |s| s.db.scripts.runs.last().is_some_and(|r| !s.db.scripts.busy() && r.done() == 2));
    let log = s.execute("db.scriptLog", &json!({})).unwrap();
    let lines: Vec<u64> = log["statements"].as_array().unwrap().iter().map(|st| st["line"].as_u64().unwrap()).collect();
    assert_eq!(lines, [1, 3], "lines are the file's: {log}");
    assert_eq!(log["scope"], "from line 2");
    assert!(log["title"].as_str().unwrap().contains("from line 2"));
    assert_eq!(committed(&db, "select sum(a) from t"), vec![vec![Cell::Int(5)]]);
    // The selection (the first statement) as a script.
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 25 })).unwrap();
    let r = s.execute("db.runSelectionAsScript", &json!({})).unwrap();
    assert_eq!(r["statements"], 1, "{r}");
    pump_until(&mut s, "selection", |s| !s.db.scripts.busy() && s.db.scripts.runs.len() == 2);
    assert_eq!(committed(&db, "select sum(a) from t"), vec![vec![Cell::Int(6)]]);
    // Nothing selected, or nothing after the caret.
    s.execute("editor.setSelection", &json!({ "anchor": 10, "head": 10 })).unwrap();
    assert!(s.execute("db.runSelectionAsScript", &json!({})).is_err());
    let end = std::fs::read_to_string(&f).unwrap().chars().count();
    s.execute("editor.setSelection", &json!({ "anchor": end, "head": end })).unwrap();
    assert!(s.execute("db.runFromCaret", &json!({})).is_err());
    s.shutdown();
}

#[test]
fn production_confirmation_runs_the_same_part() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "sqlite", "name": "Prod", "path": "lib.db", "confirmWrites": true }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    let f = root.join("p.sql");
    std::fs::write(&f, "insert into t values (100);\ninsert into t values (200);\n").unwrap();
    s.execute("file.open", &json!({ "path": f })).unwrap();
    s.execute("db.chooseFileDataSource", &json!({ "dataSource": id })).unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": 30, "head": 30 })).unwrap();
    let r = s.execute("db.runFromCaret", &json!({})).unwrap();
    assert_eq!(r["confirm"], true, "{r}");
    s.execute("db.scriptConfirm", &json!({ "confirm": true })).unwrap();
    pump_until(&mut s, "confirmed", |s| !s.db.scripts.busy() && !s.db.scripts.runs.is_empty());
    assert_eq!(committed(&db, "select sum(a) from t"), vec![vec![Cell::Int(200)]], "only the part from the caret");
    s.shutdown();
}

#[test]
fn data_tools_pickers_list_schemas_and_tables() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    let r = s.execute("db.dataTools.objects", &json!({ "dataSource": id, "schema": "main" })).unwrap();
    assert_eq!(r["loading"], true, "connects first");
    let mut last = Value::Null;
    pump_until(&mut s, "tables", |s| {
        let mut s2 = s.db.source(&id).is_some_and(|d| d.tree.info.is_some());
        if s2 {
            s2 = s.db.source(&id).unwrap().tree.schemas.get("main").is_some_and(|n| n.tables.is_some());
        }
        s2
    });
    for _ in 0..3 {
        last = s.execute("db.dataTools.objects", &json!({ "dataSource": id, "schema": "main" })).unwrap();
    }
    assert_eq!(last["schemas"], json!(["main"]));
    let names: Vec<&str> = last["tables"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["authors", "t"]);
    assert_eq!(last["loading"], false);
    s.shutdown();
}

#[test]
fn sql_dialects_and_data_sources_for_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("a.db"));
    make_db(&root.join("b.db"));
    std::fs::create_dir_all(root.join("reports")).unwrap();
    let mut s = session(&root);
    let a = add_sqlite(&mut s, "A", "a.db");
    let b = add_sqlite(&mut s, "B", "b.db");
    let rep = root.join("reports/sum.sql");
    std::fs::write(&rep, "select 1").unwrap();
    assert_eq!(s.db.source_for_path(Some(&rep)), None, "two data sources: none by default");
    let r = s
        .execute(
            "sql.fileSettings",
            &json!({ "dialects": { "reports": "postgres", "": "mysql" }, "dataSources": { "reports": b, "": a } }),
        )
        .unwrap();
    assert_eq!(r["dialects"]["reports"], "postgres");
    assert!(root.join(".codecraft/sql-files.json").is_file());
    assert_eq!(s.db.source_for_path(Some(&rep)).as_deref(), Some(b.as_str()));
    assert_eq!(s.db.source_for_path(Some(&root.join("x.sql"))).as_deref(), Some(a.as_str()));
    assert_eq!(s.db.dialect_for(Some(&rep), Some(&b)), codecraft_engine::db::Dialect::Postgres);
    assert_eq!(s.db.dialect_for(Some(&root.join("x.sql")), Some(&a)), codecraft_engine::db::Dialect::Mysql);
    assert!(s.execute("sql.fileSettings", &json!({ "dialects": { "x": "oracle" } })).is_err());
    assert!(s.execute("sql.fileSettings", &json!({ "dataSources": { "x": "nope" } })).is_err());
    assert!(s.execute("sql.fileSettings", &json!({ "dialects": { "/elsewhere/q.sql": "mysql" } })).is_err());

    // The file's own data source: chooser, choice, back to the folder default.
    s.execute("file.open", &json!({ "path": rep })).unwrap();
    let r = s.execute("db.chooseFileDataSource", &json!({})).unwrap();
    assert_eq!(r["chooser"], true);
    assert!(matches!(&s.popup, Some(Popup::Actions { items, .. }) if items.len() == 2));
    s.execute("db.chooseFileDataSource", &json!({ "dataSource": a })).unwrap();
    assert_eq!(s.db.source_for_path(Some(&rep)).as_deref(), Some(a.as_str()));
    s.execute("db.chooseFileDataSource", &json!({ "dataSource": null })).unwrap();
    assert_eq!(s.db.source_for_path(Some(&rep)).as_deref(), Some(b.as_str()));
    // The file's dialect: chooser, then a value; the active file's settings are reported.
    let r = s.execute("db.setFileDialect", &json!({})).unwrap();
    assert_eq!(r["chooser"], true);
    s.execute("db.setFileDialect", &json!({ "dialect": "sqlite" })).unwrap();
    let r = s.execute("sql.fileSettings", &json!({})).unwrap();
    assert_eq!(r["active"]["dialect"], "sqlite");
    assert_eq!(r["active"]["dialectFrom"], "file");
    s.execute("db.setFileDialect", &json!({ "dialect": null })).unwrap();
    let r = s.execute("sql.fileSettings", &json!({})).unwrap();
    assert_eq!(r["active"]["dialectFrom"], "folder");
    // Saved and loaded with the project; removing a data source drops its defaults.
    s.shutdown();
    let mut s = session(&root);
    assert_eq!(s.db.source_for_path(Some(&rep)).as_deref(), Some(b.as_str()));
    s.execute("db.removeDataSource", &json!({ "dataSource": b })).unwrap();
    assert!(!s.db.sql_files.data_sources.values().any(|v| *v == b));
    s.shutdown();
}

#[test]
fn sqlite_attached_databases_appear_as_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    {
        let mut c = codecraft_engine::db::connect(&sqlite_cfg(&root.join("archive.db")), None).unwrap();
        codecraft_engine::db::query_all(c.as_mut(), "create table old (x int)").unwrap();
    }
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    connect(&mut s, &id);
    assert!(
        s.execute("db.sqlite.attach", &json!({ "dataSource": id, "alias": "main", "path": "archive.db" })).is_err()
    );
    assert!(s.execute("db.sqlite.attach", &json!({ "dataSource": id, "alias": "x", "path": "missing.db" })).is_err());
    s.execute("db.sqlite.attach", &json!({ "dataSource": id, "alias": "arch", "path": "archive.db" })).unwrap();
    assert_eq!(s.db.source(&id).unwrap().config.options["attach.arch"], "archive.db", "stored relative");
    pump_until(&mut s, "attached", |s| {
        s.db.source(&id)
            .is_some_and(|d| !d.tree.stale && d.tree.info.as_ref().is_some_and(|i| i.schemas.contains(&"arch".into())))
    });
    // Consoles see it too.
    run(&mut s, &id, "select count(*) from arch.old");
    assert!(s.db.active().is_some_and(|r| r.error.is_none()), "{:?}", s.db.output);
    // Attaching reconnects, so an open transaction must end first.
    run(&mut s, &id, "begin; insert into t values (1)");
    pump_until(&mut s, "tx", |s| !s.db.open_transactions(None).is_empty());
    assert!(s.execute("db.sqlite.detach", &json!({ "dataSource": id, "alias": "arch" })).is_err());
    s.execute("db.tx.rollback", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "ended", |s| s.db.open_transactions(None).is_empty());
    s.execute("db.sqlite.detach", &json!({ "dataSource": id, "alias": "arch" })).unwrap();
    pump_until(&mut s, "detached", |s| {
        s.db.source(&id)
            .is_some_and(|d| !d.tree.stale && d.tree.info.as_ref().is_some_and(|i| !i.schemas.contains(&"arch".into())))
    });
    s.shutdown();
}

fn ddl_open(s: &Session) -> bool {
    s.ws.active().and_then(|(_, d)| d.path.clone()).is_some_and(|p| p.to_string_lossy().contains("codecraft-ddl"))
}

#[test]
fn ddl_tabs_are_deleted_when_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    connect(&mut s, &id);
    s.execute("sql.openDdl", &json!({ "dataSource": id, "schema": "main", "name": "authors" })).unwrap();
    pump_until(&mut s, "ddl tab", ddl_open);
    let path = s.ws.active().and_then(|(_, d)| d.path.clone()).unwrap();
    assert!(path.is_file() && path.extension().is_some_and(|e| e == "sql"));
    s.pump();
    assert!(path.is_file(), "kept while open");
    s.execute("window.editorTabs.closeTab", &json!({})).unwrap();
    pump_until(&mut s, "deleted", |_| !path.exists());
    // Open again, then end the session: its DDL files go.
    s.execute("sql.openDdl", &json!({ "dataSource": id, "schema": "main", "name": "t" })).unwrap();
    pump_until(&mut s, "ddl tab", ddl_open);
    let path = s.ws.active().and_then(|(_, d)| d.path.clone()).unwrap();
    assert!(path.is_file());
    s.shutdown();
    assert!(!path.exists());
}

#[test]
fn redis_consoles_get_no_sql_analysis() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    // A port nothing listens on (never the user's 6379): connecting just fails.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let r = s
        .execute("db.addDataSource", &json!({ "kind": "redis", "name": "Cache", "host": "127.0.0.1", "port": port }))
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    s.execute("editor.type", &json!({ "text": "SELECT * FROM nowhere WHERE x = 1\nHG" })).unwrap();
    let problems = s.execute("sql.problems", &json!({})).unwrap();
    assert_eq!(problems["problems"], json!([]), "no SQL inspections in a Redis console");
    let r = s.execute("db.complete", &json!({})).unwrap();
    assert_eq!(r["redis"], true, "{r}");
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no completion") };
    assert!(c.items.iter().any(|i| i.label == "HGETALL"));
    assert!(c.items.iter().all(|i| i.label.starts_with("HG")));
    s.popup = None;
    assert!(s.execute("sql.gotoDeclaration", &json!({})).is_err(), "no SQL navigation");
    s.shutdown();
}

#[test]
fn rename_of_a_column_alias_in_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let f = root.join("r.sql");
    std::fs::write(&f, "SELECT born * 2 AS twice FROM authors ORDER BY twice").unwrap();
    s.execute("file.open", &json!({ "path": f })).unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": 20, "head": 20 })).unwrap();
    let r = s.execute("sql.renameAlias", &json!({ "newName": "double" })).unwrap();
    assert_eq!(r["occurrences"], 2, "{r}");
    let text = s.ws.active().unwrap().1.doc.text();
    assert_eq!(text, "SELECT born * 2 AS double FROM authors ORDER BY double");
    s.shutdown();
}

// ---------------------------------------------------------------- PostgreSQL

fn pg_params(name: &str) -> Value {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    json!({
        "kind": "postgres", "name": name,
        "host": env("CODECRAFT_TEST_PG_HOST", "localhost"),
        "port": env("CODECRAFT_TEST_PG_PORT", "5432"),
        "user": env("CODECRAFT_TEST_PG_USER", &std::env::var("USER").unwrap_or_default()),
        "database": env("CODECRAFT_TEST_PG_DB", "postgres"),
        "password": std::env::var("CODECRAFT_TEST_PG_PASSWORD").unwrap_or_default(),
        "savePassword": false,
        "options": { "connect_timeout": "3" },
    })
}

fn pg_direct() -> Option<Box<dyn codecraft_engine::db::Connection>> {
    let p = pg_params("x");
    let cfg = DataSourceConfig {
        kind: DbKind::Postgres,
        host: p["host"].as_str().unwrap().into(),
        port: p["port"].as_str().unwrap().parse().ok(),
        user: p["user"].as_str().unwrap().into(),
        database: p["database"].as_str().unwrap().into(),
        options: [("connect_timeout".to_string(), "3".to_string())].into_iter().collect(),
        ..Default::default()
    };
    match codecraft_engine::db::connect(&cfg, std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            None
        }
    }
}

fn first_cell(s: &Session) -> String {
    s.db.active().and_then(|r| r.rows.first()).and_then(|r| r.first()).map(Cell::display).unwrap_or_default()
}

#[test]
fn postgres_savepoint_recovers_a_failed_transaction_and_the_server_timeout_applies() {
    let Some(mut admin) = pg_direct() else { return };
    let schema = format!("codecraft_dx_{}", std::process::id());
    let q =
        |c: &mut Box<dyn codecraft_engine::db::Connection>, sql: &str| codecraft_engine::db::query_all(c.as_mut(), sql);
    q(&mut admin, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).unwrap();
    q(&mut admin, &format!("CREATE SCHEMA {schema}")).unwrap();
    q(&mut admin, &format!("CREATE TABLE {schema}.t (id int primary key)")).unwrap();
    let default_timeout = q(&mut admin, "SHOW statement_timeout").unwrap().1[0][0].display();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let id = s.execute("db.addDataSource", &pg_params("Local PG")).unwrap()["dataSource"].as_str().unwrap().to_string();
    let w = format!("query:{id}");

    run(&mut s, &id, &format!("begin; insert into {schema}.t values (1)"));
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&w));
    s.execute("db.tx.savepoint", &json!({ "connection": w, "name": "Before Dup" })).unwrap();
    pump_until(&mut s, "savepoint", |s| savepoints(s, &w) == ["Before Dup"]);
    // A failing statement fails the transaction; rolling back to the savepoint recovers it.
    s.execute("db.execute", &json!({ "dataSource": id, "sql": format!("insert into {schema}.t values (1)") })).unwrap();
    pump_until(&mut s, "failed", |s| s.db.txs.conn(&w).is_some_and(|c| c.failed));
    s.execute("db.tx.rollbackToSavepoint", &json!({ "connection": w })).unwrap();
    pump_until(&mut s, "recovered", |s| s.db.txs.conn(&w).is_some_and(|c| c.open && !c.failed));
    run(&mut s, &id, &format!("insert into {schema}.t values (2)"));
    s.execute("db.tx.releaseSavepoint", &json!({ "connection": w, "name": "Before Dup" })).unwrap();
    pump_until(&mut s, "released", |s| savepoints(s, &w).is_empty());
    s.execute("db.tx.commit", &json!({ "connection": w })).unwrap();
    pump_until(&mut s, "committed", |s| !s.db.transaction_open(&w));
    assert_eq!(q(&mut admin, &format!("select count(*) from {schema}.t")).unwrap().1[0][0], Cell::Int(2));

    // The query timeout also sets the server's statement_timeout, and clearing restores it.
    s.execute("db.setQueryTimeout", &json!({ "dataSource": id, "seconds": 1.5 })).unwrap();
    run(&mut s, &id, "show statement_timeout");
    pump_until(&mut s, "show", |s| first_cell(s) == "1500ms");
    // It survives a rolled-back transaction (PostgreSQL undoes SET there).
    run(&mut s, &id, "begin");
    run(&mut s, &id, "rollback");
    run(&mut s, &id, "show statement_timeout");
    pump_until(&mut s, "still set", |s| first_cell(s) == "1500ms");
    let t = Instant::now();
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "select pg_sleep(5)" })).unwrap();
    pump_until(&mut s, "stopped", |s| !s.db.running());
    assert!(t.elapsed() < Duration::from_secs(4), "stopped by a timeout");
    s.execute("db.setQueryTimeout", &json!({ "dataSource": id, "seconds": 0 })).unwrap();
    run(&mut s, &id, "show statement_timeout");
    let wanted = default_timeout.clone();
    pump_until(&mut s, "restored", |s| first_cell(s) == wanted);
    s.shutdown();
    q(&mut admin, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).unwrap();
}
