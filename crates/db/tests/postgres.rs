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
