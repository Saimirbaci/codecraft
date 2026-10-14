//! SQL Server: connection failures against local fakes (nothing listening, a server that is not
//! TDS, one that never answers), and a real server only when `CODECRAFT_TEST_MSSQL_HOST`
//! (`_PORT`, `_USER`, `_PASSWORD`, `_DB`) says where one runs; then only a throwaway
//! `codecraft_test_<pid>` schema is created and dropped.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use codecraft_db::{Cell, CollectSink, Connection, DataSourceConfig, DbError, DbKind, TableKind, connect, query_all};

fn cfg(host: &str, port: u16) -> DataSourceConfig {
    let mut c = DataSourceConfig {
        id: "ms".into(),
        name: "ms".into(),
        kind: DbKind::SqlServer,
        host: host.into(),
        port: Some(port),
        user: "sa".into(),
        ..Default::default()
    };
    c.options.insert("connect_timeout".into(), "3".into());
    c
}

#[test]
fn connection_failures_are_errors() {
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    assert!(matches!(connect(&cfg("127.0.0.1", free), Some("x")), Err(DbError::Connect(_))));

    // Not a TDS server.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { return };
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let started = Instant::now();
    assert!(connect(&cfg("127.0.0.1", port), Some("x")).is_err());
    assert!(started.elapsed() < Duration::from_secs(30));

    // A server that accepts and never answers: the handshake times out.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    let started = Instant::now();
    assert!(connect(&cfg("127.0.0.1", port), Some("x")).is_err());
    assert!(started.elapsed() < Duration::from_secs(30));

    // Options are checked before connecting.
    let mut c = cfg("127.0.0.1", free);
    c.options.insert("encrypt".into(), "maybe".into());
    assert!(matches!(connect(&c, None), Err(DbError::Config(_))));
}

#[test]
fn real_sql_server_when_configured() {
    let Ok(host) = std::env::var("CODECRAFT_TEST_MSSQL_HOST") else {
        eprintln!("skipping: CODECRAFT_TEST_MSSQL_HOST is not set");
        return;
    };
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut c = cfg(&host, env("CODECRAFT_TEST_MSSQL_PORT", "1433").parse().unwrap_or(1433));
    c.user = env("CODECRAFT_TEST_MSSQL_USER", "sa");
    c.database = env("CODECRAFT_TEST_MSSQL_DB", "master");
    c.options.insert("trust_server_certificate".into(), "true".into());
    let pw = std::env::var("CODECRAFT_TEST_MSSQL_PASSWORD").ok();
    let mut conn = match connect(&c, pw.as_deref()) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("skipping: no SQL Server ({e})");
            return;
        }
    };
    let schema = format!("codecraft_test_{}", std::process::id());
    struct Guard<'a>(&'a mut dyn Connection, String);
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            let s = &self.1;
            for sql in [
                format!("IF OBJECT_ID('{s}.v') IS NOT NULL DROP VIEW [{s}].[v]"),
                format!("IF OBJECT_ID('{s}.child') IS NOT NULL DROP TABLE [{s}].[child]"),
                format!("IF OBJECT_ID('{s}.parent') IS NOT NULL DROP TABLE [{s}].[parent]"),
                format!("IF SCHEMA_ID('{s}') IS NOT NULL DROP SCHEMA [{s}]"),
            ] {
                let _ = query_all(self.0, &sql);
            }
        }
    }
    let g = Guard(conn.as_mut(), schema.clone());
    let conn = &mut *g.0;
    let s = &schema;
    assert!(conn.server_version().unwrap().contains("SQL"));
    for sql in [
        format!("CREATE SCHEMA [{s}]"),
        format!(
            "CREATE TABLE [{s}].[parent] (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(50) NOT NULL DEFAULT N'x')"
        ),
        format!(
            "CREATE TABLE [{s}].[child] (id bigint PRIMARY KEY, parent_id int REFERENCES [{s}].[parent](id), \
             amount decimal(10,3), at datetime2, d date, uid uniqueidentifier, flag bit, raw varbinary(10))"
        ),
        format!("CREATE INDEX child_parent ON [{s}].[child](parent_id)"),
        format!("CREATE VIEW [{s}].[v] AS SELECT id FROM [{s}].[parent]"),
    ] {
        query_all(conn, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let (_, _, sum) = query_all(
        conn,
        &format!("INSERT INTO [{s}].[parent](name) SELECT TOP (1200) N'p' FROM sys.all_objects a CROSS JOIN sys.all_objects b"),
    )
    .unwrap();
    assert_eq!(sum.rows_affected, Some(1200));
    query_all(
        conn,
        &format!(
            "INSERT INTO [{s}].[child] VALUES (1, 1, 12345.678, '2024-02-29T13:14:15.5', '2024-02-29', \
             'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', 1, 0x00FF)"
        ),
    )
    .unwrap();
    let (cols, rows, _) = query_all(conn, &format!("SELECT * FROM [{s}].[child]")).unwrap();
    assert_eq!(cols[1].type_name, "int");
    let r = &rows[0];
    assert_eq!(r[2], Cell::Other("12345.678".into()));
    assert_eq!(r[3], Cell::Other("2024-02-29 13:14:15.5000000".into()));
    assert_eq!(r[4], Cell::Other("2024-02-29".into()));
    assert_eq!(r[6], Cell::Bool(true));
    assert_eq!(r[7], Cell::Bytes(vec![0, 255]));
    // Pages and stopping early.
    let mut sink = CollectSink::default();
    let sum = conn.execute(&format!("SELECT * FROM [{s}].[parent] ORDER BY id"), 500, &mut sink).unwrap();
    assert_eq!(sum.rows_fetched, 1200);
    // GO batches split in the console's splitter; a batch with two result sets shows the first.
    let (_, rows, _) = query_all(conn, "SELECT 1 AS a; SELECT 2 AS b").unwrap();
    assert_eq!(rows, vec![vec![Cell::Int(1)]]);
    // Introspection.
    let info = conn.database_info().unwrap();
    assert!(info.schemas.contains(&schema));
    let t = conn.tables(&schema).unwrap();
    assert!(t.iter().any(|t| t.name == "v" && t.kind == TableKind::View));
    let d = conn.table_details(&schema, "child").unwrap();
    assert!(d.columns[0].primary_key);
    assert_eq!(d.columns[2].data_type, "decimal(10,3)");
    assert_eq!(d.foreign_keys[0].ref_table, "parent");
    assert!(d.indexes.iter().any(|i| i.name == "child_parent"));
    assert_eq!(conn.row_count(&schema, "parent").unwrap(), 1200);
    // Transactions: the server's @@TRANCOUNT is the truth.
    codecraft_db::tx::begin(conn, None).unwrap();
    assert_eq!(conn.transaction_status(), codecraft_db::tx::TxStatus::Active);
    query_all(conn, &format!("DELETE FROM [{s}].[child]")).unwrap();
    codecraft_db::tx::end(conn, false).unwrap();
    assert_eq!(conn.transaction_status(), codecraft_db::tx::TxStatus::Idle);
    assert_eq!(query_all(conn, &format!("SELECT COUNT(*) FROM [{s}].[child]")).unwrap().1, vec![vec![Cell::Int(1)]]);
    // Explain.
    let plan =
        codecraft_db::explain::explain(conn, &format!("SELECT * FROM [{s}].[parent] WHERE id = 1"), false).unwrap();
    assert!(!plan.roots.is_empty());
    query_all(conn, "SELECT 1").unwrap();
    // Cancel with the attention signal keeps the connection.
    let canceller = conn.canceller();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        canceller.cancel().unwrap();
    });
    let r = query_all(conn, "WAITFOR DELAY '00:00:20'");
    t.join().unwrap();
    assert_eq!(r.err(), Some(DbError::Cancelled));
    assert!(!conn.is_broken());
    query_all(conn, "SELECT 1").unwrap();
    match query_all(conn, "SELECT * FROM no_such_table") {
        Err(DbError::Query(e)) => assert!(e.contains("208"), "{e}"),
        other => panic!("{other:?}"),
    }
}
