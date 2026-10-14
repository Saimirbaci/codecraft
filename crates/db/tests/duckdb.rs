//! DuckDB through its CLI: a fake `duckdb` shell script (canned jsonlines output, errors, a
//! locked file, a statement that hangs) checks the protocol; the real CLI runs on a temp file when
//! it is installed (skipped otherwise).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codecraft_db::{Cell, CollectSink, DataSourceConfig, DbError, DbKind, TableKind, connect, query_all};

const FAKE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
if [ "$1" = "--version" ]; then echo "v1.1.3 fakehash"; exit 0; fi
echo "$@" > "$dir/args"
input=$(cat)
printf '%s' "$input" > "$dir/last_input"
case "$input" in
  *"SELECT 1 AS ok"*) echo '{"ok":1}' ;;
  *"DESCRIBE SELECT * FROM items"*)
    echo '{"column_name":"id","column_type":"INTEGER","null":"YES","key":null}'
    echo '{"column_name":"name","column_type":"VARCHAR","null":"YES","key":null}'
    echo '{"column_name":"at","column_type":"DATE","null":"YES","key":null}'
    echo '__CODECRAFT_NEXT_RESULT__'
    i=0
    while [ $i -lt 1100 ]; do echo "{\"id\":$i,\"name\":\"n$i\",\"at\":\"2024-01-01\"}"; i=$((i+1)); done ;;
  *"DESCRIBE SELECT * FROM empty"*)
    echo '{"column_name":"z","column_type":"BIGINT"}'
    echo '__CODECRAFT_NEXT_RESULT__' ;;
  *"INSERT INTO items"*) echo '{"Count":2}' ;;
  *"CREATE TABLE"*) ;;
  *"syntax"*) echo 'Parser Error: syntax error at or near "syntax"' >&2; exit 1 ;;
  *"locked"*)
    n=$(cat "$dir/lock_count" 2>/dev/null || echo 0)
    echo $((n+1)) > "$dir/lock_count"
    if [ "$n" -lt 2 ]; then echo 'IO Error: Could not set lock on file "x.db": Conflicting lock is held' >&2; exit 1; fi
    echo '{"ok":1}' ;;
  *"hang"*) exec sleep 30 ;;
  *"stdouterr"*) echo 'Catalog Error: Table with name nope does not exist!'; exit 1 ;;
  *"information_schema.schemata"*) echo '{"schema_name":"main"}'; echo '{"schema_name":"staging"}' ;;
  *"information_schema.tables"*) echo '{"table_name":"items","table_type":"BASE TABLE"}'; echo '{"table_name":"v","table_type":"VIEW"}' ;;
  *"information_schema.columns"*)
    echo '{"column_name":"id","data_type":"INTEGER","is_nullable":"NO","column_default":null}'
    echo '{"column_name":"owner","data_type":"INTEGER","is_nullable":"YES","column_default":"42"}'
    echo '__CODECRAFT_NEXT_RESULT__'
    echo '{"constraint_type":"PRIMARY KEY","constraint_column_names":["id"],"constraint_text":"PRIMARY KEY(id)"}'
    echo '{"constraint_type":"FOREIGN KEY","constraint_column_names":["owner"],"constraint_text":"FOREIGN KEY (\"owner\") REFERENCES people(id)"}'
    echo '__CODECRAFT_NEXT_RESULT__'
    echo '{"index_name":"items_name","is_unique":false,"is_primary":false,"expressions":"[name]"}' ;;
  *"count(*)"*) echo '{"n":1234}' ;;
  *) echo "unexpected input" >&2; exit 2 ;;
esac
"#;

fn fake_cli(dir: &Path) -> PathBuf {
    let p = dir.join("duckdb");
    std::fs::write(&p, FAKE).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn cfg(cli: &Path, db: &Path) -> DataSourceConfig {
    let mut c = DataSourceConfig {
        id: "duck".into(),
        name: "duck".into(),
        kind: DbKind::DuckDb,
        path: Some(db.to_path_buf()),
        ..Default::default()
    };
    c.options.insert("duckdb_path".into(), cli.display().to_string());
    c
}

#[test]
fn fake_cli_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_cli(dir.path());
    let db = dir.path().join("x.db");
    let mut conn = connect(&cfg(&cli, &db), None).unwrap();
    assert_eq!(conn.kind(), DbKind::DuckDb);
    assert_eq!(conn.server_version().unwrap(), "DuckDB v1.1.3");
    let input = std::fs::read_to_string(dir.path().join("last_input")).unwrap();
    assert!(input.starts_with(".bail on\n.mode jsonlines\n"), "{input}");

    // A query: DESCRIBE gives the columns and types, rows stream in pages.
    let mut sink = CollectSink::default();
    let sum = conn.execute("SELECT * FROM items -- trailing comment", 500, &mut sink).unwrap();
    assert_eq!(sink.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "at"]);
    assert_eq!(sink.columns[2].type_name, "DATE");
    assert_eq!(sum.rows_fetched, 1100);
    assert_eq!(sink.pages, 3);
    assert_eq!(sink.rows[5], vec![Cell::Int(5), Cell::Text("n5".into()), Cell::Other("2024-01-01".into())]);
    let input = std::fs::read_to_string(dir.path().join("last_input")).unwrap();
    assert!(input.contains("-- trailing comment\n;\n"), "the terminator is on its own line: {input}");
    // Stopping early kills the process.
    let mut stop = Stop(1, CollectSink::default());
    let sum = conn.execute("SELECT * FROM items", 100, &mut stop).unwrap();
    assert!(!sum.complete);
    // An empty result still has its columns.
    let (cols, rows, _) = query_all(conn.as_mut(), "SELECT * FROM empty").unwrap();
    assert_eq!((cols.len(), rows.len()), (1, 0));
    let (cols, rows, _) = query_all(conn.as_mut(), "INSERT INTO items VALUES (1), (2)").unwrap();
    assert_eq!(cols[0].name, "Count");
    assert_eq!(rows, vec![vec![Cell::Int(2)]]);
    let (cols, _, sum) = query_all(conn.as_mut(), "CREATE TABLE t (a INT)").unwrap();
    assert!(cols.is_empty());
    assert_eq!(sum.rows_affected, Some(0));
    match query_all(conn.as_mut(), "SELECT syntax") {
        Err(DbError::Query(e)) => assert!(e.contains("Parser Error"), "{e}"),
        other => panic!("{other:?}"),
    }
    // An error printed on stdout still reaches the user.
    match query_all(conn.as_mut(), "SELECT stdouterr") {
        Err(DbError::Query(e)) => assert!(e.contains("Catalog Error"), "{e}"),
        other => panic!("{other:?}"),
    }
    // A locked file is retried.
    query_all(conn.as_mut(), "SELECT 'locked'").unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("lock_count")).unwrap().trim(), "3");

    // Introspection.
    let info = conn.database_info().unwrap();
    assert_eq!(info.schemas, ["main", "staging"]);
    let t = conn.tables("main").unwrap();
    assert_eq!(t.iter().map(|t| t.kind).collect::<Vec<_>>(), [TableKind::Table, TableKind::View]);
    let d = conn.table_details("main", "items").unwrap();
    assert!(d.columns[0].primary_key && !d.columns[0].nullable);
    assert_eq!(d.columns[1].default.as_deref(), Some("42"));
    assert_eq!(d.foreign_keys[0].ref_table, "people");
    assert_eq!(d.foreign_keys[0].columns, ["owner"]);
    assert!(d.indexes.iter().any(|i| i.primary && i.columns == ["id"]));
    assert!(d.indexes.iter().any(|i| i.name == "items_name" && i.columns == ["name"]));
    assert_eq!(conn.row_count("main", "items").unwrap(), 1234);
    assert_eq!(conn.transaction_status(), codecraft_db::tx::TxStatus::Idle);
    assert!(codecraft_db::tx::begin(conn.as_mut(), None).is_err(), "no transactions across processes");
    assert!(codecraft_db::explain::explain(conn.as_mut(), "SELECT 1", false).is_err());

    // Read-only data sources pass -readonly and need the file.
    let mut ro = cfg(&cli, &db);
    ro.read_only = true;
    assert!(matches!(connect(&ro, None), Err(DbError::Connect(_))), "missing file");
    std::fs::write(&db, b"").unwrap();
    let _ro = connect(&ro, None).unwrap();
    assert!(std::fs::read_to_string(dir.path().join("args")).unwrap().starts_with("-readonly "));
}

struct Stop(usize, CollectSink);
impl codecraft_db::ResultSink for Stop {
    fn columns(&mut self, c: &[codecraft_db::Column]) {
        self.1.columns(c);
    }
    fn page(&mut self, p: codecraft_db::Page) -> bool {
        self.1.page(p);
        self.1.pages < self.0
    }
}

#[test]
fn cancel_kills_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_cli(dir.path());
    let mut conn = connect(&cfg(&cli, &dir.path().join("x.db")), None).unwrap();
    let canceller = conn.canceller();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        canceller.cancel().unwrap();
    });
    let started = Instant::now();
    let r = query_all(conn.as_mut(), "SELECT 'hang'");
    t.join().unwrap();
    assert_eq!(r.err(), Some(DbError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(20));
    // The connection still works.
    query_all(conn.as_mut(), "SELECT 1 AS ok").unwrap();
}

#[test]
fn missing_cli_and_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(&dir.path().join("nope"), &dir.path().join("x.db"));
    assert!(matches!(connect(&c, None), Err(DbError::Config(_))));
    c.options.remove("duckdb_path");
    c.path = Some(dir.path().join("missing-dir/x.db"));
    assert!(connect(&c, None).is_err());
    c.path = None;
    assert!(matches!(connect(&c, None), Err(DbError::Config(_))));
}

#[test]
fn real_duckdb_when_installed() {
    let dir = tempfile::tempdir().unwrap();
    let probe = DataSourceConfig { kind: DbKind::DuckDb, ..Default::default() };
    let Ok(cli) = codecraft_db::duckdb::find_cli(&probe) else {
        eprintln!("skipping: the duckdb CLI is not installed");
        return;
    };
    let mut conn = connect(&cfg(&cli, &dir.path().join("real.duckdb")), None).unwrap();
    assert!(conn.server_version().unwrap().starts_with("DuckDB"));
    for sql in [
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name VARCHAR NOT NULL)",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, owner INTEGER REFERENCES people(id), price DECIMAL(10,2), at DATE)",
        "INSERT INTO people SELECT i, 'p' || i FROM range(1, 1201) t(i)",
        "INSERT INTO items VALUES (1, 1, 9.99, DATE '2024-02-29')",
        "CREATE VIEW v AS SELECT * FROM people",
    ] {
        query_all(conn.as_mut(), sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let mut sink = CollectSink::default();
    let sum = conn.execute("SELECT id, name FROM people ORDER BY id", 500, &mut sink).unwrap();
    assert_eq!(sum.rows_fetched, 1200);
    assert_eq!(sink.rows[0], vec![Cell::Int(1), Cell::Text("p1".into())]);
    let (cols, rows, _) = query_all(conn.as_mut(), "SELECT * FROM items WHERE false").unwrap();
    assert_eq!((cols.len(), rows.len()), (4, 0));
    let t = conn.tables("main").unwrap();
    assert!(t.iter().any(|t| t.name == "v" && t.kind == TableKind::View));
    let d = conn.table_details("main", "items").unwrap();
    assert!(d.columns[0].primary_key);
    assert_eq!(d.foreign_keys.first().map(|f| f.ref_table.as_str()), Some("people"));
    assert_eq!(conn.row_count("main", "people").unwrap(), 1200);
    assert!(query_all(conn.as_mut(), "SELEC 1").is_err());
}
