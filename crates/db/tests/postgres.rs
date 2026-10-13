//! PostgreSQL integration, only when a local server accepts connections (e.g. Postgres.app).
//! Uses a throwaway schema that is dropped at the end; never starts or stops the server.
//! `CODECRAFT_TEST_PG_HOST`/`_PORT`/`_USER`/`_DB` override the defaults (localhost:5432, $USER, postgres).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use codecraft_db::{Cell, Connection, DataSourceConfig, DbError, DbKind, TableKind, connect, query_all};

fn config() -> DataSourceConfig {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut cfg = DataSourceConfig {
        id: "pg".into(),
        name: "pg".into(),
        kind: DbKind::Postgres,
        host: env("CODECRAFT_TEST_PG_HOST", "localhost"),
        port: env("CODECRAFT_TEST_PG_PORT", "5432").parse().ok(),
        user: env("CODECRAFT_TEST_PG_USER", &std::env::var("USER").unwrap_or_default()),
        database: env("CODECRAFT_TEST_PG_DB", "postgres"),
        ..Default::default()
    };
    cfg.options.insert("connect_timeout".into(), "3".into());
    cfg
}

fn server() -> Option<Box<dyn Connection>> {
    match connect(&config(), std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            None
        }
    }
}

struct Schema<'a>(&'a mut dyn Connection, String);

impl Drop for Schema<'_> {
    fn drop(&mut self) {
        let _ = query_all(self.0, &format!("DROP SCHEMA IF EXISTS {} CASCADE", self.1));
    }
}

#[test]
fn postgres_end_to_end() {
    let Some(mut conn) = server() else { return };
    let schema = format!("codecraft_test_{}", std::process::id());
    let guard = Schema(conn.as_mut(), schema.clone());
    let c = &mut *guard.0;
    assert!(c.server_version().unwrap().starts_with("PostgreSQL "));
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.parent (id serial PRIMARY KEY, name text NOT NULL DEFAULT 'x')"),
        format!(
            "CREATE TABLE {schema}.child (id bigint PRIMARY KEY, parent_id int REFERENCES {schema}.parent(id), \
             amount numeric(10,3), at timestamp, tags text[], doc jsonb, uid uuid, flag boolean, raw bytea)"
        ),
        format!("CREATE INDEX child_parent ON {schema}.child(parent_id)"),
        format!("CREATE VIEW {schema}.v AS SELECT id FROM {schema}.parent"),
    ] {
        query_all(c, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let (_, _, sum) =
        query_all(c, &format!("INSERT INTO {schema}.parent(name) SELECT 'p' || g FROM generate_series(1, 1200) g"))
            .unwrap();
    assert_eq!(sum.rows_affected, Some(1200));
    query_all(
        c,
        &format!(
            "INSERT INTO {schema}.child VALUES (1, 1, 12345.678, '2024-02-29 13:14:15.5', ARRAY['a', NULL, 'b c'], \
             '{{\"k\": 1}}', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', true, '\\x00ff'), \
             (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)"
        ),
    )
    .unwrap();

    // Introspection.
    let info = c.database_info().unwrap();
    assert!(info.schemas.contains(&schema));
    assert!(info.current.is_some() && !info.databases.is_empty());
    let tables = c.tables(&schema).unwrap();
    let kinds: Vec<_> = tables.iter().map(|t| (t.name.as_str(), t.kind)).collect();
    assert_eq!(kinds, vec![("child", TableKind::Table), ("parent", TableKind::Table), ("v", TableKind::View)]);
    let d = c.table_details(&schema, "child").unwrap();
    assert_eq!(d.columns.len(), 9);
    assert!(d.columns[0].primary_key && !d.columns[0].nullable);
    assert_eq!(d.columns[2].data_type, "numeric(10,3)");
    assert_eq!(d.foreign_keys.len(), 1);
    assert_eq!(d.foreign_keys[0].ref_table, "parent");
    assert_eq!(d.foreign_keys[0].columns, vec!["parent_id".to_string()]);
    assert!(d.indexes.iter().any(|i| i.name == "child_parent" && i.columns == vec!["parent_id".to_string()]));
    assert!(d.indexes.iter().any(|i| i.primary));
    let p = c.table_details(&schema, "parent").unwrap();
    assert_eq!(p.columns[1].default.as_deref(), Some("'x'::text"));
    assert_eq!(c.row_count(&schema, "parent").unwrap(), 1200);

    // Typed values and NULLs.
    let (cols, rows, _) = query_all(c, &format!("SELECT * FROM {schema}.child ORDER BY id")).unwrap();
    assert_eq!(cols[2].type_name, "numeric");
    let r = &rows[0];
    assert_eq!(r[0], Cell::Int(1));
    assert_eq!(r[2], Cell::Other("12345.678".into()));
    assert_eq!(r[3], Cell::Other("2024-02-29 13:14:15.5".into()));
    assert_eq!(r[4], Cell::Other("{a,NULL,\"b c\"}".into()));
    assert_eq!(r[5], Cell::Text("{\"k\": 1}".into()));
    assert_eq!(r[6], Cell::Other("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()));
    assert_eq!(r[7], Cell::Bool(true));
    assert_eq!(r[8], Cell::Bytes(vec![0, 255]));
    assert!(rows[1][1..].iter().all(Cell::is_null));

    // Paging: 1200 rows in pages of 500; stopping early keeps the connection usable.
    let mut sink = codecraft_db::CollectSink::default();
    let sum = c.execute(&format!("SELECT * FROM {schema}.parent"), 500, &mut sink).unwrap();
    assert_eq!((sink.pages, sink.rows.len(), sum.complete), (3, 1200, true));
    struct First(usize);
    impl codecraft_db::ResultSink for First {
        fn columns(&mut self, _: &[codecraft_db::Column]) {}
        fn page(&mut self, p: codecraft_db::Page) -> bool {
            self.0 += p.rows.len();
            false
        }
    }
    let mut first = First(0);
    let sum = c.execute("SELECT g FROM generate_series(1, 200000) g", 500, &mut first).unwrap();
    assert_eq!((first.0, sum.complete), (500, false));
    let (_, rows, _) = query_all(c, "SELECT 41 + 1").unwrap();
    assert_eq!(rows, vec![vec![Cell::Int(42)]]);

    // Errors carry the server message.
    match query_all(c, "SELECT * FROM no_such_table_xyz") {
        Err(DbError::Query(m)) => assert!(m.contains("does not exist"), "{m}"),
        other => panic!("{other:?}"),
    }
    drop(guard);
}

#[test]
fn postgres_cancel() {
    let Some(mut conn) = server() else { return };
    let canceller = conn.canceller();
    let worker = std::thread::spawn(move || {
        let t = Instant::now();
        let r = query_all(conn.as_mut(), "SELECT pg_sleep(30)").map(|_| ());
        (r, t.elapsed())
    });
    std::thread::sleep(Duration::from_millis(300));
    canceller.cancel().unwrap();
    let (r, elapsed) = worker.join().unwrap();
    assert_eq!(r, Err(DbError::Cancelled));
    assert!(elapsed < Duration::from_secs(10));
}

#[test]
fn postgres_bad_credentials_fail_cleanly() {
    if server().is_none() {
        return;
    }
    let mut cfg = config();
    cfg.database = "codecraft_no_such_database".into();
    assert!(matches!(connect(&cfg, None), Err(DbError::Connect(_))));
    let mut cfg = config();
    cfg.port = Some(1);
    assert!(connect(&cfg, None).is_err());
}

#[test]
fn postgres_grid_edits_apply_in_one_transaction() {
    use codecraft_db::edit::{RowChange, apply};
    let Some(mut conn) = server() else { return };
    let schema = format!("codecraft_edit_{}", std::process::id());
    let guard = Schema(conn.as_mut(), schema.clone());
    let c = &mut *guard.0;
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!(
            "CREATE TABLE {schema}.\"T x\" (id int, k text, v text, flag boolean, raw bytea, n numeric DEFAULT 7, \
             PRIMARY KEY (id, k))"
        ),
        format!("INSERT INTO {schema}.\"T x\" VALUES (1, 'a', 'one', true, NULL), (2, 'b', 'two', false, NULL)"),
    ] {
        query_all(c, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let key = |id: i64, k: &str| vec![("id".to_string(), Cell::Int(id)), ("k".to_string(), Cell::Text(k.into()))];
    let changes = vec![
        RowChange::Update {
            key: key(1, "a"),
            set: vec![
                ("v".into(), Cell::Text("it's \\ back".into())),
                ("flag".into(), Cell::Bool(false)),
                ("raw".into(), Cell::Bytes(vec![0, 0xff])),
            ],
        },
        RowChange::Delete { key: key(2, "b") },
        RowChange::Insert { values: vec![("id".into(), Cell::Int(3)), ("k".into(), Cell::Text("c".into()))] },
    ];
    assert_eq!(apply(c, &schema, "T x", &changes).unwrap(), 3);
    let (_, rows, _) =
        query_all(c, &format!("SELECT id, v, flag, raw, n::text FROM {schema}.\"T x\" ORDER BY id")).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Cell::Text("it's \\ back".into()));
    assert_eq!(rows[0][2], Cell::Bool(false));
    assert_eq!(rows[0][3], Cell::Bytes(vec![0, 0xff]));
    assert_eq!(rows[1][4], Cell::Text("7".into()));
    // A stale key rolls the whole batch back.
    let e = apply(c, &schema, "T x", &[RowChange::Delete { key: key(3, "c") }, RowChange::Delete { key: key(2, "b") }])
        .unwrap_err();
    assert!(e.to_string().contains("changed or deleted"), "{e}");
    let (_, rows, _) = query_all(c, &format!("SELECT count(*) FROM {schema}.\"T x\"")).unwrap();
    assert_eq!(rows[0][0], Cell::Int(2));
}

#[test]
fn postgres_sslmode() {
    let Some(mut conn) = server() else { return };
    let (_, rows, _) = query_all(conn.as_mut(), "SHOW ssl").unwrap();
    let ssl_on = rows.first().and_then(|r| r.first()) == Some(&Cell::Text("on".into()));
    let with = |mode: &str| {
        let mut cfg = config();
        cfg.options.insert("sslmode".into(), mode.into());
        connect(&cfg, std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref())
    };
    // prefer falls back to plain text when the server has no TLS; disable never tries.
    assert!(with("prefer").is_ok());
    assert!(with("disable").is_ok());
    match with("require") {
        Ok(_) => assert!(ssl_on, "require connected to a server without TLS"),
        Err(e) => assert!(!ssl_on || cfg!(not(any(target_os = "macos", target_os = "windows"))), "{e}"),
    }
    // A missing root certificate is a configuration error, not a panic.
    let mut cfg = config();
    cfg.options.insert("sslmode".into(), "verify-full".into());
    cfg.options.insert("sslrootcert".into(), "/nonexistent/root.pem".into());
    let e = connect(&cfg, None).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(e.contains("root.pem") || e.contains("not supported"), "{e}");
}

#[test]
fn postgres_explain_and_read_only_sessions() {
    let Some(mut conn) = server() else { return };
    let schema = format!("codecraft_explain_{}", std::process::id());
    let guard = Schema(conn.as_mut(), schema.clone());
    let c = &mut *guard.0;
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.t (id int PRIMARY KEY, v text)"),
        format!("INSERT INTO {schema}.t SELECT g, 'v' || g FROM generate_series(1, 50) g"),
    ] {
        query_all(c, &sql).unwrap();
    }
    let sql = format!("SELECT * FROM {schema}.t WHERE v LIKE 'v1%' ORDER BY v");
    let plan = codecraft_db::explain::explain(c, &sql, false).unwrap();
    assert!(!plan.analyzed);
    let root = &plan.roots[0];
    assert!(root.total_cost.is_some() && root.rows.is_some(), "{root:?}");
    let mut stack = vec![root];
    let mut relations = Vec::new();
    while let Some(n) = stack.pop() {
        relations.extend(n.relation.clone());
        stack.extend(n.children.iter());
    }
    assert!(relations.iter().any(|r| r.contains(".t") || r == "t"), "{relations:?}");
    let analyzed = codecraft_db::explain::explain(c, &sql, true).unwrap();
    assert!(analyzed.analyzed && analyzed.execution_ms.is_some());
    assert!(analyzed.roots[0].actual_rows.is_some());

    // A read-only data source cannot write, even with statements the IDE does not classify.
    let mut cfg = config();
    cfg.read_only = true;
    let mut ro = connect(&cfg, std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()).unwrap();
    let e = query_all(ro.as_mut(), &format!("INSERT INTO {schema}.t VALUES (999, 'x')")).unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
    assert!(query_all(ro.as_mut(), &format!("SELECT count(*) FROM {schema}.t")).is_ok());
}
