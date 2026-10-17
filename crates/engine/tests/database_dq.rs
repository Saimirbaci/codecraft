//! Track DQ through engine commands: bind parameters (console prompt, `db.execute` params,
//! agents' `db_query` params, Compare Data sync runs) and editing console query results of one
//! table (pending changes, savepoint submit inside an open transaction, read-only reasons).
//! SQLite always; PostgreSQL in a throwaway schema when a local server accepts connections.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::db::{Cell, DataSourceConfig, DbKind, MemoryStore};

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
        "insert into authors(name, born) values ('Ann', 1950), ('Bob', 1960), ('Cy', 1970)",
        "create table tags (code text not null unique, label text)",
        "insert into tags values ('a', 'Alpha'), ('b', 'Beta')",
        "create table loose (a int, b text)",
        "insert into loose values (1, 'x')",
    ] {
        codecraft_engine::db::query_all(c.as_mut(), sql).unwrap();
    }
}

fn committed(path: &Path, sql: &str) -> Vec<Vec<Cell>> {
    let mut c = codecraft_engine::db::connect(&sqlite_cfg(path), None).unwrap();
    codecraft_engine::db::query_all(c.as_mut(), sql).unwrap().1
}

fn add_sqlite(s: &mut Session, file: &str) -> String {
    let r = s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Lib", "path": file })).unwrap();
    r["dataSource"].as_str().unwrap().to_string()
}

/// Run `sql` (with `params`) on the query connection; the new result's id.
fn query(s: &mut Session, source: &str, sql: &str, params: Value) -> u64 {
    let prev = s.db.active_result;
    let mut p = json!({ "dataSource": source, "sql": sql });
    if !params.is_null() {
        p["params"] = params;
    }
    s.execute("db.execute", &p).unwrap();
    pump_until(s, "result", |s| s.db.active().is_some_and(|r| Some(r.id) != prev && !r.is_running()));
    s.db.active_result.unwrap()
}

fn reason(s: &mut Session, rid: u64) -> String {
    s.execute("db.getChanges", &json!({ "result": rid })).unwrap()["reason"].as_str().unwrap_or("").to_string()
}

#[test]
fn console_query_results_of_one_table_are_editable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "lib.db");

    let rid = query(&mut s, &id, "select id, name, upper(name) as shout from authors where id <= 2 order by id", Value::Null);
    pump_until(&mut s, "keys", |s| s.db.editable(rid).is_ok());
    let r = s.db.result(rid).unwrap();
    assert_eq!(r.query_table.as_ref().map(|q| (q.schema.as_str(), q.table.as_str())), Some(("main", "authors")));
    let ch = s.execute("db.getChanges", &json!({ "result": rid })).unwrap();
    assert_eq!(ch["editableColumns"], json!([true, true, false]), "{ch}");
    assert_eq!(ch["target"]["table"], "authors");

    // A computed column cannot change; a plain one can, with hostile text.
    let e = s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 2, "value": "x" })).unwrap_err();
    assert!(e.to_string().contains("computed"), "{e}");
    let hostile = "O'Brien \\ '); DROP TABLE authors; -- 🦀";
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": hostile })).unwrap();
    let row = s.execute("db.addRow", &json!({ "result": rid })).unwrap()["row"].as_u64().unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": row, "column": 1, "value": "Dee" })).unwrap();
    let preview = s.execute("db.previewChanges", &json!({ "result": rid })).unwrap();
    let st: Vec<&str> = preview["statements"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(st[0].starts_with("UPDATE \"main\".\"authors\" SET \"name\" = ?1 WHERE \"id\" = ?2\n-- parameters:"), "{st:?}");
    assert!(st[1].starts_with("INSERT INTO \"main\".\"authors\" (\"name\") VALUES (?1)"), "{st:?}");
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "submitted and reloaded", |s| {
        s.db.result(rid).is_some_and(|r| !r.edits.submitting && !r.is_running() && r.edits.is_empty() && r.rows.len() == 2)
    });
    assert_eq!(committed(&db, "select name from authors where id = 1"), vec![vec![Cell::Text(hostile.into())]]);
    assert_eq!(committed(&db, "select count(*) from authors"), vec![vec![Cell::Int(4)]]);
    // The reloaded result is the same query, still editable.
    pump_until(&mut s, "editable again", |s| s.db.editable(rid).is_ok());

    // A unique key identifies rows when there is no primary key.
    let tags = query(&mut s, &id, "select code, label from tags", Value::Null);
    pump_until(&mut s, "tag keys", |s| s.db.editable(tags).is_ok());
    s.execute("db.editCell", &json!({ "result": tags, "row": 1, "column": 1, "value": "Bêta" })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": tags })).unwrap();
    pump_until(&mut s, "tags", |s| {
        s.db.result(tags).is_some_and(|r| !r.edits.submitting && r.edits.is_empty() && !r.is_running()) && !s.db.running()
    });
    assert_eq!(committed(&db, "select label from tags where code = 'b'"), vec![vec![Cell::Text("Bêta".into())]]);

    // Read-only, with the reason.
    for (sql, why) in [
        ("select a.name, b.code from authors a join tags b on 1 = 1", "JOIN"),
        ("select count(*) from authors", "aggregates"),
        ("select distinct name from authors", "DISTINCT"),
        ("select name from authors", "key columns (id)"),
        ("select label from tags", "key columns (code)"),
        ("select * from loose", "no primary or unique key"),
        ("select 1 + 1", "no table"),
    ] {
        let rid = query(&mut s, &id, sql, Value::Null);
        pump_until(&mut s, "a reason", |s| s.db.editable(rid).err().is_some_and(|e| !e.starts_with("loading")));
        let r = reason(&mut s, rid);
        assert!(r.contains(why), "{sql}: {r}");
        assert!(s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 0, "value": "1" })).is_err());
    }
}

#[test]
fn console_edits_inside_an_open_transaction_use_a_savepoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "lib.db");
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "begin" })).unwrap();
    let worker = format!("query:{id}");
    pump_until(&mut s, "tx", |s| s.db.transaction_open(&worker));
    let rid = query(&mut s, &id, "select * from authors", Value::Null);
    pump_until(&mut s, "keys", |s| s.db.editable(rid).is_ok());
    // A failing change (NOT NULL) undoes just the submit; the transaction stays open.
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": null })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "failed", |s| s.db.result(rid).is_some_and(|r| !r.edits.submitting && r.edits.error.is_some()));
    let e = s.db.result(rid).unwrap().edits.error.clone().unwrap();
    assert!(e.contains("NOT NULL") && e.contains("rolled back"), "{e}");
    assert!(s.db.transaction_open(&worker));
    s.execute("db.revertChanges", &json!({ "result": rid })).unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": 2, "column": 1, "value": "Cyd" })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "submitted", |s| s.db.result(rid).is_some_and(|r| !r.edits.submitting && r.edits.is_empty()));
    assert!(s.status.contains("Commit") || s.db.output.iter().any(|l| l.text.contains("Commit to save")), "{}", s.status);
    assert_eq!(committed(&db, "select name from authors where id = 3"), vec![vec![Cell::Text("Cy".into())]]);
    s.execute("db.commit", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "committed", |s| !s.db.transaction_open(&worker));
    assert_eq!(committed(&db, "select name from authors where id = 3"), vec![vec![Cell::Text("Cyd".into())]]);
}

#[test]
fn parameters_are_bound_not_spliced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "lib.db");

    // Object and array params; a value that looks like SQL is just a value.
    let attack = "x' OR '1'='1";
    let rid = query(&mut s, &id, "select id from authors where name = :n", json!({ "n": attack }));
    assert!(s.db.result(rid).unwrap().rows.is_empty());
    let rid = query(&mut s, &id, "select id from authors where name = ? or id = ?", json!([attack, 3]));
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Int(3)]]);
    assert_eq!(s.db.result(rid).unwrap().params, vec![Cell::Text(attack.into()), Cell::Int(3)]);
    // The log shows the statement with placeholders and the values after it.
    assert!(s.db.output.iter().any(|l| l.text.contains("where name = ?1 or id = ?2\n-- parameters: ?1 = 'x'' OR")));
    // Sorting and filtering re-run the query with the same values.
    s.execute("db.filterResult", &json!({ "result": rid, "where": "id > 0" })).unwrap();
    pump_until(&mut s, "filtered", |s| s.db.result(rid).is_some_and(|r| !r.is_running() && r.error.is_none() && r.rows.len() == 1));
    // Wrong counts and expressions from agents are refused cleanly.
    assert!(s.execute("db.execute", &json!({ "dataSource": id, "sql": "select ?, ?", "params": [1] })).is_err());
    assert!(s.execute("db.execute", &json!({ "dataSource": id, "sql": "select ?", "params": [1, 2] })).is_err());
    let rid = query(&mut s, &id, "select :t", json!({ ":t": { "sql": "1 + 1" } }));
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Int(2)]]);
    let rid = query(&mut s, &id, "select hex(:b)", json!({ "b": { "hex": "00ff27" } }));
    assert_eq!(s.db.result(rid).unwrap().rows, vec![vec![Cell::Text("00FF27".into())]]);

    // The prompt: Value reads numbers and NULL, Text keeps the text, SQL inserts it.
    s.execute("db.newConsole", &json!({ "dataSource": id })).unwrap();
    s.execute("editor.type", &json!({ "text": "select :a, typeof(:a), :b, :c" })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    assert!(s.db.param_prompt.is_some());
    let prev = s.db.active_result;
    s.execute(
        "db.submitParameters",
        &json!({ "values": { ":a": "007", ":b": "NULL", ":c": "2 * 21" }, "kinds": { ":b": "text", ":c": "sql" } }),
    )
    .unwrap();
    pump_until(&mut s, "prompt run", |s| s.db.active().is_some_and(|r| Some(r.id) != prev && !r.is_running()));
    let r = s.db.active().unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Cell::Text("007".into()), Cell::Text("text".into()), Cell::Text("NULL".into()), Cell::Int(42)]]
    );
    // The kinds are remembered with the values.
    s.execute("db.execute", &Value::Null).unwrap();
    let p = s.db.param_prompt.clone().unwrap();
    let kinds: Vec<_> = p.params.iter().map(|f| (f.value.as_str(), f.kind)).collect();
    use codecraft_engine::database::ParamKind;
    assert_eq!(kinds, vec![("007", ParamKind::Auto), ("NULL", ParamKind::Text), ("2 * 21", ParamKind::Sql)]);
    s.execute("db.cancelParameters", &Value::Null).unwrap();
}

#[test]
fn agents_bind_params_and_keep_the_read_only_classification() {
    use codecraft_engine::agent_access::{AgentAccess, Capability};
    use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("lib.db");
    make_db(&db);
    let mut s = session(&root);
    let id = add_sqlite(&mut s, "lib.db");
    // Reads only: no db-write.
    s.agent_access = AgentAccess::headless(&[]);
    let call = |s: &mut Session, args: Value| {
        agent_tools::call_blocking(s, Caller::new("dq"), "db_query", args, Duration::from_secs(60))
    };
    let r = call(&mut s, json!({ "dataSource": id, "sql": "select name from authors where id = :id", "params": { "id": 2 } }));
    assert!(!r.is_error, "{}", result_text(&r));
    assert_eq!(result_json(&r)["results"][0]["rows"], json!([["Bob"]]));
    // A value that is SQL stays a value: still a read, nothing dropped.
    let r = call(
        &mut s,
        json!({ "dataSource": id, "sql": "select count(*) from authors where name = ?", "params": ["x'; drop table authors; --"] }),
    );
    assert!(!r.is_error, "{}", result_text(&r));
    assert_eq!(result_json(&r)["results"][0]["rows"], json!([[0]]));
    assert_eq!(committed(&db, "select count(*) from authors"), vec![vec![Cell::Int(3)]]);
    // Writes still need db-write, params or not.
    let r = call(&mut s, json!({ "dataSource": id, "sql": "delete from authors where id = ?", "params": [1] }));
    assert!(r.is_error, "{}", result_text(&r));
    assert_eq!(committed(&db, "select count(*) from authors"), vec![vec![Cell::Int(3)]]);
    // Agents cannot pass SQL expressions as values.
    let r = call(&mut s, json!({ "dataSource": id, "sql": "select :x", "params": { "x": { "sql": "1" } } }));
    assert!(r.is_error && result_text(&r).contains("expressions"), "{}", result_text(&r));
    // Through codecraft_execute an expression is judged as part of the SQL.
    let r = agent_tools::call_blocking(
        &mut s,
        Caller::new("dq"),
        "codecraft_execute",
        json!({ "id": "db.execute", "params": { "dataSource": id, "sql": "select :x",
            "params": { "x": { "sql": "1; delete from authors" } } } }),
        Duration::from_secs(60),
    );
    assert!(r.is_error, "an expression that writes needs db-write: {}", result_text(&r));
    assert_eq!(committed(&db, "select count(*) from authors"), vec![vec![Cell::Int(3)]]);
    // Missing values say what to pass.
    let r = call(&mut s, json!({ "dataSource": id, "sql": "select :x" }));
    assert!(r.is_error && result_text(&r).contains("params"), "{}", result_text(&r));
    // With db-write, bound writes run.
    s.agent_access = AgentAccess::headless(&[Capability::DbWrite]);
    let r = call(&mut s, json!({ "dataSource": id, "sql": "update authors set name = ? where id = ?", "params": ["Ann 🦀 '\\", 1] }));
    assert!(!r.is_error, "{}", result_text(&r));
    assert_eq!(committed(&db, "select name from authors where id = 1"), vec![vec![Cell::Text("Ann 🦀 '\\".into())]]);
    s.shutdown();
}

#[test]
fn compare_data_sync_runs_with_bound_values() {
    use codecraft_engine::database::data_tools::{JobResult, JobState};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for (f, name) in [("left.db", "it's \\ 🦀"), ("right.db", "old")] {
        let mut c = codecraft_engine::db::connect(&sqlite_cfg(&root.join(f)), None).unwrap();
        codecraft_engine::db::query_all(c.as_mut(), "create table t (id integer primary key, name text)").unwrap();
        codecraft_engine::db::query_all(c.as_mut(), &format!("insert into t values (1, '{}')", name.replace('\'', "''")))
            .unwrap();
    }
    let mut s = session(&root);
    let l = s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "L", "path": "left.db" })).unwrap();
    let r = s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "R", "path": "right.db" })).unwrap();
    let (l, r) = (l["dataSource"].as_str().unwrap().to_string(), r["dataSource"].as_str().unwrap().to_string());
    let job = s
        .execute(
            "db.compareData",
            &json!({ "left": { "dataSource": l, "table": "t", "schema": "main" },
                     "right": { "dataSource": r, "table": "t", "schema": "main" } }),
        )
        .unwrap()["job"]
        .as_u64()
        .unwrap();
    pump_until(&mut s, "compare", |s| s.db.data.job(job).is_some_and(|j| j.state != JobState::Running));
    assert!(matches!(s.db.data.job(job).and_then(|j| j.result.as_ref()), Some(JobResult::Compare(_))));
    let script = s.execute("db.compareData.syncScript", &json!({ "job": job })).unwrap();
    // The script (for files and the clipboard) has literals; running it binds them.
    assert!(script["script"].as_str().unwrap().contains("'it''s \\ 🦀'"), "{script}");
    s.execute("db.compareData.syncScript", &json!({ "job": job, "run": true })).unwrap();
    pump_until(&mut s, "sync", |s| !s.db.running());
    assert!(s.db.output.iter().any(|l| l.text.contains("SET \"name\" = ?1") && l.text.contains("-- parameters")));
    assert_eq!(committed(&root.join("right.db"), "select name from t"), vec![vec![Cell::Text("it's \\ 🦀".into())]]);
}

// ---------------------------------------------------------------- PostgreSQL

fn pg_params() -> Value {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    json!({
        "kind": "postgres",
        "name": "pg",
        "host": env("CODECRAFT_TEST_PG_HOST", "localhost"),
        "port": env("CODECRAFT_TEST_PG_PORT", "5432").parse::<u16>().unwrap_or(5432),
        "user": env("CODECRAFT_TEST_PG_USER", &std::env::var("USER").unwrap_or_default()),
        "database": env("CODECRAFT_TEST_PG_DB", "postgres"),
        "options": "connect_timeout=3",
    })
}

#[test]
fn postgres_console_query_edit_and_params() {
    let cfg = DataSourceConfig {
        kind: DbKind::Postgres,
        host: pg_params()["host"].as_str().unwrap().into(),
        port: pg_params()["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
        user: pg_params()["user"].as_str().unwrap().into(),
        database: pg_params()["database"].as_str().unwrap().into(),
        options: [("connect_timeout".to_string(), "3".to_string())].into_iter().collect(),
        ..Default::default()
    };
    let mut admin = match codecraft_engine::db::connect(&cfg, std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            return;
        }
    };
    let schema = format!("codecraft_dq_{}", std::process::id());
    struct Drop_(Box<dyn codecraft_engine::db::Connection>, String);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = codecraft_engine::db::query_all(self.0.as_mut(), &format!("DROP SCHEMA IF EXISTS {} CASCADE", self.1));
        }
    }
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.items (id int PRIMARY KEY, name text NOT NULL, price numeric(12,2), at timestamptz)"),
        format!("INSERT INTO {schema}.items VALUES (1, 'a', 1.50, '2024-01-01 00:00+00'), (2, 'b', 2.00, NULL)"),
    ] {
        codecraft_engine::db::query_all(admin.as_mut(), &sql).unwrap();
    }
    let mut guard = Drop_(admin, schema.clone());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let id = s.execute("db.addDataSource", &pg_params()).unwrap()["dataSource"].as_str().unwrap().to_string();

    // Typed parameters: numeric and timestamptz from text, the server infers the types.
    let rid = query(
        &mut s,
        &id,
        &format!("select id, name, price from {schema}.items where price >= $1 and (at < $2 or at is null) order by id"),
        json!(["1.75", "2030-01-01T00:00:00+02:00"]),
    );
    assert_eq!(s.db.result(rid).unwrap().rows.len(), 1, "{:?}", s.db.result(rid).unwrap().error);
    pump_until(&mut s, "keys", |s| s.db.editable(rid).is_ok());
    let hostile = "it's \\ $1 🦀";
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 1, "value": hostile })).unwrap();
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 2, "value": "9999999999.99" })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "submitted", |s| s.db.result(rid).is_some_and(|r| !r.edits.submitting && !r.is_running() && r.edits.is_empty()));
    assert!(s.db.result(rid).unwrap().edits.error.is_none());
    let back = codecraft_engine::db::query_all(
        guard.0.as_mut(),
        &format!("select name, price::text from {schema}.items where id = 2"),
    )
    .unwrap()
    .1;
    assert_eq!(back, vec![vec![Cell::Text(hostile.into()), Cell::Text("9999999999.99".into())]]);
    // An out-of-range value fails with the server's error and nothing changes.
    s.execute("db.editCell", &json!({ "result": rid, "row": 0, "column": 2, "value": "99999999999999" })).unwrap();
    s.execute("db.submitChanges", &json!({ "result": rid })).unwrap();
    pump_until(&mut s, "failed", |s| s.db.result(rid).is_some_and(|r| !r.edits.submitting && r.edits.error.is_some()));
    let e = s.db.result(rid).unwrap().edits.error.clone().unwrap();
    assert!(e.contains("overflow") && e.contains("rolled back"), "{e}");
    // Unqualified names resolve in the connection's schema; after a typed `SET search_path`
    // the console's results are read-only until qualified.
    s.execute("db.execute", &json!({ "dataSource": id, "sql": format!("set search_path to {schema}") })).unwrap();
    pump_until(&mut s, "set", |s| !s.db.running());
    let rid = query(&mut s, &id, "select * from items", Value::Null);
    let why = reason(&mut s, rid);
    assert!(why.contains("qualify the table"), "{why}");
    s.shutdown();
}
