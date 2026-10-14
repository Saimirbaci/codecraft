//! Transactions, grid submits, cancel, timeouts and dropped connections against SQLite (always)
//! and a local PostgreSQL server (skipped when none accepts connections; a throwaway schema).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use codecraft_db::edit::{RowChange, apply_changes};
use codecraft_db::tx::{self, TxStatus, Watchdog, execute_timed};
use codecraft_db::{Cell, Connection, DataSourceConfig, DbError, DbKind, connect, query_all};

fn pg_config() -> DataSourceConfig {
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

fn pg() -> Option<Box<dyn Connection>> {
    match connect(&pg_config(), std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            None
        }
    }
}

struct Schema(Box<dyn Connection>, String);

impl Drop for Schema {
    fn drop(&mut self) {
        let _ = query_all(self.0.as_mut(), "ROLLBACK");
        let _ = query_all(self.0.as_mut(), &format!("DROP SCHEMA IF EXISTS {} CASCADE", self.1));
    }
}

fn pg_schema(tag: &str) -> Option<Schema> {
    let mut c = pg()?;
    let name = format!("codecraft_tx_{tag}_{}", std::process::id());
    for sql in [
        format!("DROP SCHEMA IF EXISTS {name} CASCADE"),
        format!("CREATE SCHEMA {name}"),
        format!("CREATE TABLE {name}.t (id int PRIMARY KEY, name text NOT NULL, n int CHECK (n >= 0))"),
        format!("INSERT INTO {name}.t VALUES (1, 'a', 1), (2, 'b', 2), (3, 'c', 3)"),
    ] {
        query_all(c.as_mut(), &sql).unwrap();
    }
    Some(Schema(c, name))
}

fn rows(c: &mut dyn Connection, table: &str) -> Vec<Vec<Cell>> {
    query_all(c, &format!("SELECT id, name, n FROM {table} ORDER BY id")).unwrap().1
}

fn key(id: i64) -> Vec<(String, Cell)> {
    vec![("id".into(), Cell::Int(id))]
}

fn sqlite() -> (tempfile::TempDir, Box<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = DataSourceConfig { kind: DbKind::Sqlite, path: Some(dir.path().join("t.db")), ..Default::default() };
    let mut c = connect(&cfg, None).unwrap();
    query_all(c.as_mut(), "create table t (id integer primary key, name text not null, n int check (n >= 0))").unwrap();
    query_all(c.as_mut(), "insert into t values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3)").unwrap();
    (dir, c)
}

/// A multi-row submit whose third change fails: nothing changes, the failing change is named.
fn failing_submit_changes_nothing(c: &mut dyn Connection, schema: &str) {
    let table = if schema.is_empty() { "t".to_string() } else { format!("{schema}.t") };
    let before = rows(c, &table);
    let changes = vec![
        RowChange::Delete { key: key(1) },
        RowChange::Update { key: key(2), set: vec![("name".into(), Cell::Text("B".into()))] },
        RowChange::Update { key: key(3), set: vec![("n".into(), Cell::Int(-5))] },
        RowChange::Insert { values: vec![("id".into(), Cell::Int(9)), ("name".into(), Cell::Text("z".into()))] },
    ];
    let f = apply_changes(c, schema, "t", &changes, false).unwrap_err();
    assert_eq!(f.index, Some(2), "{f}");
    assert!(f.rolled_back);
    assert!(f.statement.as_deref().unwrap_or("").contains("-5"), "{f}");
    assert!(f.to_string().to_lowercase().contains("check"), "{f}");
    assert_eq!(rows(c, &table), before);
    assert_eq!(c.transaction_status(), TxStatus::Idle);
    // Fixed, the same submit goes through.
    let mut fixed = changes;
    fixed[2] = RowChange::Update { key: key(3), set: vec![("n".into(), Cell::Int(5))] };
    let r = apply_changes(c, schema, "t", &fixed, false).unwrap();
    assert_eq!((r.affected, r.in_transaction), (4, false));
    assert_eq!(rows(c, &table).len(), 3);
}

/// Inside an open transaction a submit uses a savepoint: a failure keeps the earlier work, a
/// success waits for the transaction's commit.
fn submit_inside_a_transaction(c: &mut dyn Connection, schema: &str) {
    let table = if schema.is_empty() { "t".to_string() } else { format!("{schema}.t") };
    tx::begin(c, None).unwrap();
    tx::run(c, &format!("UPDATE {table} SET name = 'kept' WHERE id = 2")).unwrap();
    let bad = vec![RowChange::Delete { key: key(2) }, RowChange::Delete { key: key(99) }];
    let f = apply_changes(c, schema, "t", &bad, true).unwrap_err();
    assert_eq!(f.index, Some(1));
    assert!(c.transaction_status().is_open(), "the outer transaction survives");
    assert_eq!(rows(c, &table)[1][1], Cell::Text("kept".into()));
    let ok = vec![RowChange::Delete { key: key(3) }];
    let r = apply_changes(c, schema, "t", &ok, true).unwrap();
    assert!(r.in_transaction);
    assert_eq!(tx::end(c, false).unwrap(), tx::TxEnd::RolledBack);
    assert_eq!(rows(c, &table).len(), 3, "rolled back");
}

#[test]
fn sqlite_submits_are_all_or_nothing() {
    let (_d, mut c) = sqlite();
    failing_submit_changes_nothing(c.as_mut(), "");
    // Reset rows for the next scenario.
    query_all(c.as_mut(), "delete from t").unwrap();
    query_all(c.as_mut(), "insert into t values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3)").unwrap();
    submit_inside_a_transaction(c.as_mut(), "");
}

#[test]
fn postgres_submits_are_all_or_nothing() {
    let Some(mut s) = pg_schema("submit") else { return };
    let name = s.1.clone();
    failing_submit_changes_nothing(s.0.as_mut(), &name);
    query_all(s.0.as_mut(), &format!("DELETE FROM {name}.t")).unwrap();
    query_all(s.0.as_mut(), &format!("INSERT INTO {name}.t VALUES (1, 'a', 1), (2, 'b', 2), (3, 'c', 3)")).unwrap();
    submit_inside_a_transaction(s.0.as_mut(), &name);
}

#[test]
fn postgres_status_and_failed_transactions() {
    let Some(mut s) = pg_schema("status") else { return };
    let name = s.1.clone();
    let c = s.0.as_mut();
    assert_eq!(c.transaction_status(), TxStatus::Idle);
    // Probing twice outside a transaction stays idle (the probe is its own statement).
    assert_eq!(c.transaction_status(), TxStatus::Idle);
    tx::begin(c, Some(tx::Isolation::RepeatableRead)).unwrap();
    assert_eq!(c.transaction_status(), TxStatus::Active);
    let (_, iso, _) = query_all(c, "SHOW transaction_isolation").unwrap();
    assert_eq!(iso[0][0], Cell::Text("repeatable read".into()));
    tx::run(c, &format!("INSERT INTO {name}.t VALUES (10, 'x', 0)")).unwrap();
    assert!(tx::run(c, "SELECT 1/0").is_err());
    assert_eq!(c.transaction_status(), TxStatus::Failed);
    // COMMIT of a failed transaction is refused loudly and rolls back.
    let e = tx::end(c, true).unwrap_err();
    assert!(e.to_string().contains("nothing was committed"), "{e}");
    assert_eq!(c.transaction_status(), TxStatus::Idle);
    assert_eq!(rows(c, &format!("{name}.t")).len(), 3);
    // A submit into a failed transaction is refused before running anything.
    tx::begin(c, None).unwrap();
    let _ = tx::run(c, "SELECT 1/0");
    let f = apply_changes(c, &name, "t", &[RowChange::Delete { key: key(1) }], true).unwrap_err();
    assert!(f.error.contains("has failed"), "{f}");
    tx::end(c, false).unwrap();
    // Session isolation.
    let sql = tx::session_isolation_sql(DbKind::Postgres, tx::Isolation::Serializable).unwrap().unwrap();
    tx::run(c, &sql).unwrap();
    let (_, iso, _) = query_all(c, "SHOW transaction_isolation").unwrap();
    assert_eq!(iso[0][0], Cell::Text("serializable".into()));
}

#[test]
fn postgres_cancel_and_timeout_stop_a_long_query() {
    let Some(mut c) = pg() else { return };
    // Cancel from another thread.
    let canceller = c.canceller();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel().unwrap();
    });
    let started = Instant::now();
    let e = query_all(c.as_mut(), "SELECT pg_sleep(30)").unwrap_err();
    t.join().unwrap();
    assert_eq!(e, DbError::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    // The query timeout.
    let w = Watchdog::new(c.canceller(), Some(Duration::from_millis(300)));
    let started = Instant::now();
    let e = execute_timed(c.as_mut(), &w, "SELECT pg_sleep(30)", 10, &mut tx::Discard).unwrap_err();
    assert!(e.to_string().contains("timed out"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(c.ping().is_ok());
}

#[test]
fn postgres_killed_connection_inside_a_transaction() {
    let Some(mut s) = pg_schema("kill") else { return };
    let name = s.1.clone();
    let Some(mut victim) = pg() else { return };
    let (_, pid, _) = query_all(victim.as_mut(), "SELECT pg_backend_pid()").unwrap();
    let pid = match &pid[0][0] {
        Cell::Int(p) => *p,
        other => panic!("pid {other:?}"),
    };
    tx::begin(victim.as_mut(), None).unwrap();
    tx::run(victim.as_mut(), &format!("DELETE FROM {name}.t WHERE id = 1")).unwrap();
    query_all(s.0.as_mut(), &format!("SELECT pg_terminate_backend({pid})")).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(tx::run(victim.as_mut(), "SELECT 1").is_err());
    assert!(victim.is_broken());
    assert!(victim.ping().is_err());
    // The server rolled the delete back.
    assert_eq!(rows(s.0.as_mut(), &format!("{name}.t")).len(), 3);
    // Rolling back a lost transaction is not an error (the server did it).
    assert_eq!(tx::end(victim.as_mut(), false).unwrap(), tx::TxEnd::RolledBack);
}
