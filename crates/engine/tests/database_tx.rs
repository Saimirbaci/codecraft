//! Track DB through engine commands: transactions (modes, isolation, the pending-transaction
//! guard on disconnect / close console / close project / quit), all-or-nothing grid submits,
//! query timeout and cancel, dropped connections, and the data tools (export, compare, search,
//! dump and restore). SQLite always; PostgreSQL when a local server accepts connections.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::database::ResultStatus;
use codecraft_engine::database::data_tools::{JobResult, JobState};
use codecraft_engine::database::tx::GuardState;
use codecraft_engine::db::{Cell, DataSourceConfig, DbKind, MemoryStore, SecretStore};
use codecraft_engine::{Effect, Session};

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
        "create table authors (id integer primary key, name text not null, born int check (born > 0))",
        "insert into authors(name, born) values ('Ann', 1950), ('Bob', 1960), ('Cy', 1970)",
        "create table books (id integer primary key, author_id int, title text, note text)",
        "insert into books(author_id, title, note) with recursive n(i) as (select 1 union all select i + 1 from n \
         where i < 1500) select 1 + i % 3, printf('Book %04d', i), case when i % 100 = 0 then 'needle here' end from n",
    ] {
        codecraft_engine::db::query_all(c.as_mut(), sql).unwrap();
    }
}

/// Rows through a separate connection (sees only committed data).
fn committed(path: &Path, sql: &str) -> Vec<Vec<Cell>> {
    let mut c = codecraft_engine::db::connect(&sqlite_cfg(path), None).unwrap();
    codecraft_engine::db::query_all(c.as_mut(), sql).unwrap().1
}

fn add_sqlite(s: &mut Session, name: &str, file: &str) -> String {
    let r = s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": name, "path": file })).unwrap();
    r["dataSource"].as_str().unwrap().to_string()
}

fn wait_result(s: &mut Session, prev: Option<u64>) -> u64 {
    pump_until(s, "result", |s| s.db.active().is_some_and(|r| Some(r.id) != prev && !r.is_running()));
    s.db.active_result.unwrap()
}

/// Run SQL on the data source's query connection and wait until it finished.
fn run(s: &mut Session, source: &str, sql: &str) {
    s.execute("db.execute", &json!({ "dataSource": source, "sql": sql })).unwrap();
    pump_until(s, "run", |s| !s.db.running());
}

fn quits(s: &Session) -> usize {
    s.effects.iter().filter(|e| matches!(e, Effect::Quit)).count()
}

#[test]
fn disconnect_with_an_open_transaction_asks_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    run(&mut s, &id, "begin; insert into authors(name, born) values ('Tx', 2000)");
    let worker = format!("query:{id}");
    pump_until(&mut s, "tx open", |s| s.db.transaction_open(&worker));
    let p = s.execute("db.tx.pending", &json!({})).unwrap();
    assert_eq!(p["transactions"][0]["connection"], worker.as_str());
    assert!(p["transactions"][0]["statements"].as_u64().unwrap() >= 1, "{p}");

    // Disconnect asks; nothing is closed yet.
    let r = s.execute("db.disconnect", &json!({ "dataSource": id })).unwrap();
    assert_eq!(r["ask"], true);
    assert_eq!(s.db.txs.guard.as_ref().unwrap().state, GuardState::Asking);
    assert!(s.db.transaction_open(&worker));
    // Cancel keeps everything.
    s.execute("db.tx.resolve", &json!({ "choice": "cancel" })).unwrap();
    assert!(s.db.txs.guard.is_none() && s.db.transaction_open(&worker));
    // Rollback, then the disconnect happens.
    s.execute("db.disconnect", &json!({ "dataSource": id })).unwrap();
    s.execute("db.tx.resolve", &json!({ "choice": "rollback" })).unwrap();
    pump_until(&mut s, "disconnected", |s| s.db.txs.guard.is_none() && s.db.open_transactions(None).is_empty());
    assert!(s.db.connections_of(&id).is_empty(), "disconnected");
    assert_eq!(committed(&db, "select count(*) from authors"), vec![vec![Cell::Int(3)]]);

    // Answering up front (agents): commit, then disconnect.
    run(&mut s, &id, "begin; insert into authors(name, born) values ('Kept', 2001)");
    pump_until(&mut s, "tx open", |s| s.db.transaction_open(&worker));
    s.execute("db.disconnect", &json!({ "dataSource": id, "transactions": "commit" })).unwrap();
    pump_until(&mut s, "committed", |s| s.db.txs.guard.is_none() && s.db.connections_of(&id).is_empty());
    assert_eq!(committed(&db, "select count(*) from authors"), vec![vec![Cell::Int(4)]]);
    assert!(s.execute("db.disconnect", &json!({ "transactions": "maybe" })).is_ok(), "nothing open: no question");
    s.shutdown();
}

#[test]
fn quit_and_close_project_resolve_transactions_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    let worker = format!("query:{id}");
    run(&mut s, &id, "begin; delete from books where id <= 10");
    pump_until(&mut s, "tx open", |s| s.db.transaction_open(&worker));
    s.effects.clear();

    // Quit asks instead of quitting; the window close is blocked too.
    let r = s.execute("app.quitRustRover", &json!({})).unwrap();
    assert_eq!(r["action"], "Quit");
    assert_eq!(quits(&s), 0);
    assert!(codecraft_engine::database::tx::quit_blocked(&mut s));
    s.execute("db.tx.resolve", &json!({ "choice": "commit" })).unwrap();
    pump_until(&mut s, "quit", |s| quits(s) == 1);
    assert!(!codecraft_engine::database::tx::quit_blocked(&mut s));
    assert_eq!(committed(&db, "select count(*) from books"), vec![vec![Cell::Int(1490)]]);

    // Close Project asks as well; rollback, then the project closes.
    run(&mut s, &id, "begin; delete from books");
    pump_until(&mut s, "tx open", |s| s.db.transaction_open(&worker));
    s.execute("file.closeProject", &json!({})).unwrap();
    assert!(s.ws.project.is_some());
    s.execute("db.tx.resolve", &json!({ "choice": "rollback" })).unwrap();
    pump_until(&mut s, "closed", |s| s.ws.project.is_none());
    assert_eq!(committed(&db, "select count(*) from books"), vec![vec![Cell::Int(1490)]]);
    s.shutdown();
}

#[test]
fn closing_a_console_with_a_transaction_asks_and_the_failed_commit_keeps_the_question() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    // Per-console manual mode and isolation.
    let r = s.execute("db.tx.setMode", &json!({ "mode": "manual" })).unwrap();
    assert!(r["scope"].as_str().unwrap().starts_with("console:"), "{r}");
    assert!(!s.db.source(&id).unwrap().config.manual_commit, "the data source default is unchanged");
    assert!(
        s.execute("db.tx.setIsolation", &json!({ "level": "read-committed" })).is_err(),
        "SQLite: serializable only"
    );
    s.execute("db.tx.setIsolation", &json!({ "level": "serializable" })).unwrap();
    s.execute("editor.type", &json!({ "text": "update authors set name = 'Zed' where id = 1" })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "tx", |s| !s.db.running() && !s.db.open_transactions(None).is_empty());
    let worker = s.db.open_transactions(None)[0].clone();
    assert!(worker.starts_with("console:"));
    let c = s.db.txs.conn(&worker).unwrap();
    assert!(c.open && c.statements >= 1 && c.duration().is_some(), "{c:?}");

    // Closing the console tab asks.
    let r = s.execute("window.editorTabs.closeTab", &json!({})).unwrap();
    assert_eq!(r["action"], "Close Console");
    assert!(s.ws.active().is_some(), "still open");
    s.execute("db.tx.resolve", &json!({ "choice": "commit" })).unwrap();
    pump_until(&mut s, "closed", |s| s.ws.active().is_none() && s.db.txs.guard.is_none());
    assert!(!s.db.connections_of(&id).contains(&worker), "the console's connection closed");
    assert_eq!(committed(&db, "select name from authors where id = 1"), vec![vec![Cell::Text("Zed".into())]]);
    s.shutdown();
}

#[test]
fn a_failing_grid_submit_leaves_the_table_unchanged_and_names_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    s.execute("db.openTable", &json!({ "dataSource": id, "schema": "main", "table": "authors" })).unwrap();
    let rid = wait_result(&mut s, None);
    pump_until(&mut s, "key", |s| s.db.editable(rid).is_ok());
    let before = committed(&db, "select * from authors order by id");
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": "Ann B" })).unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": 1, "column": 2, "value": -1 })).unwrap();
    let new = s.execute("db.addRow", &json!({ "result": rid })).unwrap();
    let new = new["row"].as_u64().or_else(|| new.as_u64()).unwrap_or(3);
    s.execute("db.editCell", &json!({ "result": rid, "row": new, "column": 1, "value": "New" })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "submitted", |s| s.db.result(rid).is_some_and(|r| !r.edits.submitting));
    let r = s.db.result(rid).unwrap();
    let err = r.edits.error.clone().unwrap();
    assert_eq!(r.edits.failed_row, Some(1), "{err}");
    assert!(
        err.contains("Row 2") && err.to_lowercase().contains("check") && err.contains("Nothing was saved"),
        "{err}"
    );
    assert_eq!(r.edits.pending_rows(), 3, "the edits stay for a retry");
    assert_eq!(committed(&db, "select * from authors order by id"), before);
    assert!(s.db.open_transactions(None).is_empty(), "nothing left open");
    // Fix the bad value and retry.
    s.execute("db.editCell", &json!({ "result": rid, "row": 1, "column": 2, "value": 1961 })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "saved", |s| {
        s.db.result(rid).is_some_and(|r| !r.edits.submitting && r.edits.is_empty() && !r.is_running())
    });
    assert_eq!(committed(&db, "select count(*) from authors where name in ('Ann B', 'New')"), vec![vec![Cell::Int(2)]]);
    s.shutdown();
}

#[test]
fn query_timeout_cancels_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "Library", "lib.db");
    s.execute("db.setQueryTimeout", &json!({ "dataSource": id, "seconds": 0.3 })).unwrap();
    assert_eq!(s.db.source(&id).unwrap().config.options["query_timeout"], "0.3");
    let t = Instant::now();
    s.execute(
        "db.execute",
        &json!({ "dataSource": id, "sql": "with recursive n(i) as (select 1 union all select i + 1 from n) select count(*) from n" }),
    )
    .unwrap();
    pump_until(&mut s, "timeout", |s| !s.db.running() && s.db.output.iter().any(|l| l.text.contains("timed out")));
    assert!(t.elapsed() < Duration::from_secs(15));
    // Cancel (no timeout).
    s.execute("db.setQueryTimeout", &json!({ "dataSource": id, "seconds": 0 })).unwrap();
    s.execute(
        "db.execute",
        &json!({ "dataSource": id, "sql": "with recursive n(i) as (select 1 union all select i + 1 from n) select count(*) from n" }),
    )
    .unwrap();
    pump_until(&mut s, "running", |s| s.db.running());
    std::thread::sleep(Duration::from_millis(200));
    s.execute("db.cancel", &json!({})).unwrap();
    pump_until(&mut s, "cancelled", |s| !s.db.running() && s.db.output.iter().any(|l| l.text == "Query cancelled"));
    s.shutdown();
}

#[test]
fn data_tools_export_compare_search_dump_restore() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("a.db"));
    make_db(&root.join("b.db"));
    {
        let mut c = codecraft_engine::db::connect(&sqlite_cfg(&root.join("b.db")), None).unwrap();
        for sql in [
            "delete from books where id in (7, 700)",
            "update books set title = 'Other' where id = 42",
            "insert into books values (9999, 1, 'Extra', null)",
        ] {
            codecraft_engine::db::query_all(c.as_mut(), sql).unwrap();
        }
    }
    let mut s = session(&root);
    let a = add_sqlite(&mut s, "A", "a.db");
    let b = add_sqlite(&mut s, "B", "b.db");
    let wait_job = |s: &mut Session, job: u64| {
        pump_until(s, "job", |s| s.db.data.job(job).is_some_and(|j| !j.is_running()));
        let j = s.db.data.job(job).unwrap().clone();
        assert_eq!(j.state, JobState::Done, "{:?}", j.log);
        j
    };

    // Export the whole table (more than a grid page) to CSV and JSON.
    let r = s.execute("db.exportTable", &json!({ "dataSource": a, "table": "books", "path": "books.csv" })).unwrap();
    let j = wait_job(&mut s, r["job"].as_u64().unwrap());
    assert!(matches!(j.result, Some(JobResult::File { rows: Some(1500), .. })));
    let csv = std::fs::read_to_string(root.join("books.csv")).unwrap();
    assert_eq!(csv.lines().count(), 1501);
    assert!(!root.join("books.csv.partial").exists());
    let r = s
        .execute(
            "db.exportTable",
            &json!({ "dataSource": a, "sql": "select id from books where id < 4", "path": "q.json", "format": "json" }),
        )
        .unwrap();
    wait_job(&mut s, r["job"].as_u64().unwrap());
    let v: Value = serde_json::from_str(&std::fs::read_to_string(root.join("q.json")).unwrap()).unwrap();
    assert_eq!(v, json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]));

    // Compare A.books with B.books by primary key; the sync script makes B equal.
    let r = s
        .execute(
            "db.compareData",
            &json!({ "left": { "dataSource": a, "table": "books" }, "right": { "dataSource": b, "table": "books" } }),
        )
        .unwrap();
    let job = r["job"].as_u64().unwrap();
    let j = wait_job(&mut s, job);
    let Some(JobResult::Compare(c)) = &j.result else { panic!("{:?}", j.result) };
    let n = c.result.counts;
    assert_eq!((n.only_left, n.only_right, n.different, n.same), (2, 1, 1, 1497));
    let script = s.execute("db.compareData.syncScript", &json!({ "job": job })).unwrap();
    assert_eq!(script["statements"].as_array().unwrap().len(), 4);
    s.execute("db.compareData.syncScript", &json!({ "job": job, "run": true })).unwrap();
    pump_until(&mut s, "synced", |s| !s.db.running());
    let r = s
        .execute(
            "db.compareData",
            &json!({ "left": { "dataSource": a, "table": "books" }, "right": { "dataSource": b, "sql": "select * from books" }, "keys": ["id"] }),
        )
        .unwrap();
    let j = wait_job(&mut s, r["job"].as_u64().unwrap());
    let Some(JobResult::Compare(c)) = &j.result else { panic!() };
    assert!(c.result.counts.identical(), "{:?}", c.result.counts);

    // Full-text search, then open a hit filtered to its row.
    let r = s.execute("db.fullTextSearch", &json!({ "dataSource": a, "text": "NEEDLE" })).unwrap();
    let job = r["job"].as_u64().unwrap();
    let j = wait_job(&mut s, job);
    let Some(JobResult::Search { tables, .. }) = &j.result else { panic!() };
    assert_eq!(tables.len(), 1);
    assert_eq!((tables[0].table.as_str(), tables[0].rows.len()), ("books", 15));
    let prev = s.db.active_result;
    s.execute("db.fullTextSearch.open", &json!({ "job": job, "table": 0, "row": 0 })).unwrap();
    let rid = wait_result(&mut s, prev);
    let res = s.db.result(rid).unwrap();
    assert_eq!((res.rows.len(), res.status), (1, ResultStatus::Done));
    assert_eq!(res.filter.where_, "\"id\" = 100");
    let r =
        s.execute("db.fullTextSearch", &json!({ "dataSource": a, "text": "NEEDLE", "caseSensitive": true })).unwrap();
    let j = wait_job(&mut s, r["job"].as_u64().unwrap());
    assert!(matches!(&j.result, Some(JobResult::Search { tables, .. }) if tables.is_empty()));

    // SQLite dump and restore (native), into a fresh database.
    let r = s.execute("db.dump", &json!({ "dataSource": a, "path": "a.sql" })).unwrap();
    wait_job(&mut s, r["job"].as_u64().unwrap());
    let c = add_sqlite(&mut s, "C", "c.db");
    let r = s.execute("db.restore", &json!({ "dataSource": c, "path": "a.sql" })).unwrap();
    let j = wait_job(&mut s, r["job"].as_u64().unwrap());
    assert!(matches!(j.result, Some(JobResult::Restored { statements: Some(n), .. }) if n > 1500));
    assert_eq!(committed(&root.join("c.db"), "select count(*) from books"), vec![vec![Cell::Int(1500)]]);
    // Cancel a job and list jobs.
    assert!(s.execute("db.job.cancel", &json!({})).is_err(), "nothing runs");
    assert!(s.execute("db.job.list", &json!({})).unwrap()["jobs"].as_array().unwrap().len() >= 7);
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn dump_and_restore_with_fake_tools_keep_the_password_off_argv() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let tools = root.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let script = |name: &str, body: &str| {
        let p = tools.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let log = root.join("argv.txt");
    let fake_dump = script(
        "pg_dump",
        &format!(
            "echo \"$@\" > '{}'; echo \"env=$PGPASSWORD\" >> '{}'; for a in \"$@\"; do case \"$a\" in --file=*) \
             echo 'CREATE TABLE t();' > \"${{a#--file=}}\";; esac; done; echo 'pg_dump: dumping contents' >&2",
            log.display(),
            log.display()
        ),
    );
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "postgres", "name": "Prod", "host": "db.invalid", "database": "shop", "user": "ann",
                     "password": "s3cret-pw", "savePassword": false, "confirmWrites": true }),
        )
        .unwrap();
    let pg = r["dataSource"].as_str().unwrap().to_string();
    let r = s
        .execute("db.dump", &json!({ "dataSource": pg, "path": "shop.sql", "tool": fake_dump, "tables": ["public.t"] }))
        .unwrap();
    let job = r["job"].as_u64().unwrap();
    pump_until(&mut s, "dump", |s| s.db.data.job(job).is_some_and(|j| !j.is_running()));
    let j = s.db.data.job(job).unwrap();
    assert_eq!(j.state, JobState::Done, "{:?}", j.log);
    assert!(j.log.iter().any(|l| l.contains("dumping contents")), "{:?}", j.log);
    assert!(j.log.iter().all(|l| !l.contains("s3cret-pw")), "{:?}", j.log);
    let argv = std::fs::read_to_string(&log).unwrap();
    let mut lines = argv.lines();
    let args = lines.next().unwrap();
    assert!(!args.contains("s3cret-pw"), "{args}");
    assert!(
        args.contains("--dbname=shop") && args.contains("--table=public.t") && args.contains("--no-password"),
        "{args}"
    );
    assert_eq!(lines.next().unwrap(), "env=s3cret-pw");
    assert_eq!(std::fs::read_to_string(root.join("shop.sql")).unwrap(), "CREATE TABLE t();\n");

    // A failing tool leaves no output file behind.
    let bad = script("pg_dump_bad", "echo 'pg_dump: error: connection refused' >&2; exit 1");
    let r = s.execute("db.dump", &json!({ "dataSource": pg, "path": "bad.sql", "tool": bad })).unwrap();
    let job = r["job"].as_u64().unwrap();
    pump_until(&mut s, "dump fails", |s| s.db.data.job(job).is_some_and(|j| !j.is_running()));
    assert!(matches!(&s.db.data.job(job).unwrap().state, JobState::Failed(e) if e.contains("connection refused")));
    assert!(!root.join("bad.sql").exists() && !root.join("bad.sql.partial").exists());

    // Restore: production data sources need confirmation; psql gets the file and the password.
    let fake_psql =
        script("psql", &format!("echo \"$@\" > '{}'; echo \"env=$PGPASSWORD\" >> '{}'", log.display(), log.display()));
    assert!(s.execute("db.restore", &json!({ "dataSource": pg, "path": "shop.sql", "tool": fake_psql })).is_err());
    let r = s
        .execute("db.restore", &json!({ "dataSource": pg, "path": "shop.sql", "tool": fake_psql, "confirm": true }))
        .unwrap();
    let job = r["job"].as_u64().unwrap();
    pump_until(&mut s, "restore", |s| s.db.data.job(job).is_some_and(|j| !j.is_running()));
    assert_eq!(s.db.data.job(job).unwrap().state, JobState::Done);
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(
        argv.contains("--single-transaction")
            && argv.contains("ON_ERROR_STOP=1")
            && !argv.lines().next().unwrap().contains("s3cret")
    );
    assert!(argv.contains("env=s3cret-pw"));
    // A missing tool says how to configure it.
    let r = s.execute("db.dump", &json!({ "dataSource": pg, "path": "x.sql", "tool": "/nonexistent/pg_dump" }));
    assert!(r.unwrap_err().to_string().contains("not found"));
    let _ = MemoryStore::default().get("x");
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

#[test]
fn postgres_lost_connection_in_a_transaction_reports_the_rollback_and_does_not_retry() {
    let Some(mut admin) = pg_direct() else { return };
    let schema = format!("codecraft_txe_{}", std::process::id());
    let q =
        |c: &mut Box<dyn codecraft_engine::db::Connection>, sql: &str| codecraft_engine::db::query_all(c.as_mut(), sql);
    q(&mut admin, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).unwrap();
    q(&mut admin, &format!("CREATE SCHEMA {schema}")).unwrap();
    q(&mut admin, &format!("CREATE TABLE {schema}.t (id int primary key)")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let id = s.execute("db.addDataSource", &pg_params("Local PG")).unwrap()["dataSource"].as_str().unwrap().to_string();
    let worker = format!("query:{id}");
    let count = |admin: &mut Box<dyn codecraft_engine::db::Connection>| {
        codecraft_engine::db::query_all(admin.as_mut(), &format!("SELECT count(*) FROM {schema}.t")).unwrap().1[0][0]
            .clone()
    };

    run(&mut s, &id, &format!("BEGIN; INSERT INTO {schema}.t VALUES (1)"));
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&worker));
    run(&mut s, &id, "SELECT pg_backend_pid()");
    let pid = s.db.active().unwrap().rows[0][0].clone();
    let Cell::Int(pid) = pid else { panic!("{pid:?}") };
    q(&mut admin, &format!("SELECT pg_terminate_backend({pid})")).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // The next statement fails: the transaction is gone, and it is not retried.
    run(&mut s, &id, &format!("INSERT INTO {schema}.t VALUES (2)"));
    pump_until(&mut s, "lost", |s| s.db.output.iter().any(|l| l.text.contains("rolled back")));
    assert!(!s.db.transaction_open(&worker));
    assert!(s.db.txs.conn(&worker).unwrap().lost.is_some());
    assert_eq!(count(&mut admin), Cell::Int(0), "neither insert survived, nothing retried");
    // The next read reconnects transparently.
    let prev = s.db.active_result;
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "SELECT 41 + 1" })).unwrap();
    let rid = wait_result(&mut s, prev);
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Int(42)]]);

    // A read on a connection that dropped while idle is retried on a new one.
    run(&mut s, &id, "SELECT pg_backend_pid()");
    let Cell::Int(pid) = s.db.active().unwrap().rows[0][0].clone() else { panic!() };
    q(&mut admin, &format!("SELECT pg_terminate_backend({pid})")).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let prev = s.db.active_result;
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "SELECT 7" })).unwrap();
    let rid = wait_result(&mut s, prev);
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Int(7)]], "{:?}", s.db.output);

    // A failed transaction is never committed.
    run(&mut s, &id, &format!("BEGIN; INSERT INTO {schema}.t VALUES (3)"));
    run(&mut s, &id, "SELECT 1/0");
    pump_until(&mut s, "failed", |s| s.db.txs.conn(&worker).is_some_and(|c| c.failed));
    s.execute("db.tx.commit", &json!({ "connection": worker })).unwrap();
    pump_until(&mut s, "commit refused", |s| s.db.output.iter().any(|l| l.text.contains("nothing was committed")));
    pump_until(&mut s, "closed", |s| !s.db.transaction_open(&worker));
    assert_eq!(count(&mut admin), Cell::Int(0));

    // Cancel stops pg_sleep; isolation applies to the session.
    s.execute("db.tx.setIsolation", &json!({ "dataSource": id, "level": "repeatable-read" })).unwrap();
    pump_until(&mut s, "isolation", |s| s.db.output.iter().any(|l| l.text.contains("Repeatable Read")));
    let prev = s.db.active_result;
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "SHOW transaction_isolation" })).unwrap();
    let rid = wait_result(&mut s, prev);
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Text("repeatable read".into())]]);
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "SELECT pg_sleep(30)" })).unwrap();
    pump_until(&mut s, "sleeping", |s| s.db.running());
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    s.execute("db.cancel", &json!({})).unwrap();
    pump_until(&mut s, "cancelled", |s| s.db.output.iter().any(|l| l.text == "Query cancelled"));
    assert!(t.elapsed() < Duration::from_secs(10));
    assert!(s.db.output.iter().any(|l| l.text == "Query cancelled"), "{:?}", s.db.output);
    s.shutdown();
    q(&mut admin, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).unwrap();
}

#[test]
fn postgres_keepalive_notices_a_dropped_connection_while_idle() {
    let Some(mut admin) = pg_direct() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let mut params = pg_params("Idle PG");
    params["options"]["keepalive"] = json!("0.2");
    let id = s.execute("db.addDataSource", &params).unwrap()["dataSource"].as_str().unwrap().to_string();
    let worker = format!("query:{id}");
    run(&mut s, &id, "BEGIN; SELECT pg_backend_pid()");
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&worker));
    let Cell::Int(pid) = s.db.active().unwrap().rows[0][0].clone() else { panic!() };
    codecraft_engine::db::query_all(admin.as_mut(), &format!("SELECT pg_terminate_backend({pid})")).unwrap();
    // No request: the keep-alive ping finds out and says the transaction was rolled back.
    pump_until(&mut s, "lost", |s| s.db.txs.conn(&worker).is_some_and(|c| c.lost.is_some()));
    assert!(!s.db.transaction_open(&worker));
    assert!(s.db.txs.conn(&worker).unwrap().lost.as_deref().unwrap().contains("rolled back"));
    // The next statement (meant for that transaction) is refused once, the one after runs.
    let n = s.db.output.len();
    run(&mut s, &id, "SELECT 1");
    pump_until(&mut s, "refused", |s| s.db.output[n..].iter().any(|l| l.text.contains("nothing was retried")));
    let prev = s.db.active_result;
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "SELECT 2" })).unwrap();
    let rid = wait_result(&mut s, prev);
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Int(2)]]);
    s.shutdown();
}
