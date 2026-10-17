//! Bind parameters round-trip hostile values: quotes, backslashes, NUL, unicode, emoji, very
//! long text, every byte value, NULL vs empty text, numbers at their type's limits, dates and
//! times with zones. Against SQLite always, and a local PostgreSQL server in a throwaway schema
//! (skipped when none accepts connections).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use codecraft_db::bind::{self, Bound, ParamValue, ParamValues};
use codecraft_db::edit::{RowChange, apply_changes, bound_statement};
use codecraft_db::import::{ImportData, insert_bound, run_import_bound};
use codecraft_db::{Cell, Connection, DataSourceConfig, DbKind, connect, query_all, query_all_bound};

fn hostile_texts() -> Vec<String> {
    vec![
        "it's".into(),
        "''; DROP TABLE t; --".into(),
        "back\\slash \\' \\\\ end\\".into(),
        "E'\\x41' $$ $1 ? :name ${x} @P1 {p1:String}".into(),
        "línea\nnueva\r\n\ttab".into(),
        "ünïcödé — 日本語 — עברית".into(),
        "emoji 🦀👩‍💻🏳️‍🌈".into(),
        String::new(),
        " ".into(),
        "x".repeat(1 << 20),
    ]
}

fn all_bytes() -> Vec<u8> {
    (0..=255u8).chain([0, 0, 255]).collect()
}

// ---------------------------------------------------------------- SQLite

fn sqlite() -> (tempfile::TempDir, Box<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = DataSourceConfig { kind: DbKind::Sqlite, path: Some(dir.path().join("b.db")), ..Default::default() };
    let mut c = connect(&cfg, None).unwrap();
    query_all(c.as_mut(), "create table t (id integer primary key, s text, b blob, i integer, f real)").unwrap();
    (dir, c)
}

#[test]
fn sqlite_grid_edits_round_trip_hostile_values() {
    let (_dir, mut c) = sqlite();
    let mut texts = hostile_texts();
    texts.push("nul \0 inside".into());
    let mut changes = Vec::new();
    for (i, s) in texts.iter().enumerate() {
        changes.push(RowChange::Insert {
            values: vec![("id".into(), Cell::Int(i as i64 + 1)), ("s".into(), Cell::Text(s.clone()))],
        });
    }
    let n = texts.len() as i64;
    changes.push(RowChange::Insert {
        values: vec![
            ("id".into(), Cell::Int(n + 1)),
            ("s".into(), Cell::Null),
            ("b".into(), Cell::Bytes(all_bytes())),
            ("i".into(), Cell::Int(i64::MIN)),
            ("f".into(), Cell::Float(f64::MAX)),
        ],
    });
    changes.push(RowChange::Insert {
        values: vec![
            ("id".into(), Cell::Int(n + 2)),
            ("s".into(), Cell::Text(String::new())),
            ("i".into(), Cell::Int(i64::MAX)),
            ("f".into(), Cell::Float(f64::MIN_POSITIVE)),
            ("b".into(), Cell::Bytes(Vec::new())),
        ],
    });
    // The statements carry placeholders, never the values.
    let st = bound_statement(DbKind::Sqlite, "main", "t", changes.first().unwrap()).unwrap();
    assert_eq!(st.sql, "INSERT INTO \"main\".\"t\" (\"id\", \"s\") VALUES (?1, ?2)");
    apply_changes(c.as_mut(), "main", "t", &changes, false).unwrap();

    let (_, rows, _) = query_all(c.as_mut(), "select id, s, b, i, f from t order by id").unwrap();
    for (i, s) in texts.iter().enumerate() {
        assert_eq!(rows[i][1], Cell::Text(s.clone()), "row {i}");
    }
    let last = &rows[texts.len()];
    assert_eq!(last[1], Cell::Null);
    assert_eq!(last[2], Cell::Bytes(all_bytes()));
    assert_eq!(last[3], Cell::Int(i64::MIN));
    assert_eq!(last[4], Cell::Float(f64::MAX));
    let empty = &rows[texts.len() + 1];
    assert_eq!(empty[1], Cell::Text(String::new()), "empty text is not NULL");
    assert_eq!(empty[3], Cell::Int(i64::MAX));
    assert_eq!(empty[4], Cell::Float(f64::MIN_POSITIVE));
    assert_eq!(empty[2], Cell::Bytes(Vec::new()));

    // Updates and deletes keyed by hostile values.
    query_all(c.as_mut(), "create table k (code text primary key, v integer)").unwrap();
    let key = "a'b\\c\0d 🦀".to_string();
    let ins = RowChange::Insert { values: vec![("code".into(), Cell::Text(key.clone())), ("v".into(), Cell::Int(1))] };
    apply_changes(c.as_mut(), "", "k", &[ins], false).unwrap();
    let k = vec![("code".into(), Cell::Text(key.clone()))];
    let up = RowChange::Update { key: k.clone(), set: vec![("v".into(), Cell::Int(2))] };
    assert_eq!(apply_changes(c.as_mut(), "", "k", &[up], false).unwrap().affected, 1);
    assert_eq!(query_all(c.as_mut(), "select v from k").unwrap().1, vec![vec![Cell::Int(2)]]);
    assert_eq!(apply_changes(c.as_mut(), "", "k", &[RowChange::Delete { key: k }], false).unwrap().affected, 1);
    assert!(query_all(c.as_mut(), "select v from k").unwrap().1.is_empty());
}

#[test]
fn sqlite_console_parameters_bind() {
    let (_dir, mut c) = sqlite();
    query_all(c.as_mut(), "insert into t (id, s) values (1, 'x'), (2, '007')").unwrap();
    let mut v = ParamValues::new();
    v.insert(":s".into(), ParamValue::Value(Cell::Text("x' OR '1'='1".into())));
    let b = bind::prepare(DbKind::Sqlite, "select id from t where s = :s", &v).unwrap();
    assert_eq!(b.sql, "select id from t where s = ?1");
    assert!(query_all_bound(c.as_mut(), &b.sql, &b.params).unwrap().1.is_empty(), "the value is data, not SQL");
    // Text that looks like a number stays text.
    v.insert(":s".into(), bind::parse_param("007", bind::ParamKind::Auto));
    let b = bind::prepare(DbKind::Sqlite, "select id from t where s = :s", &v).unwrap();
    assert_eq!(query_all_bound(c.as_mut(), &b.sql, &b.params).unwrap().1, vec![vec![Cell::Int(2)]]);
    // A wrong number of values is an error, not a panic.
    let e = query_all_bound(c.as_mut(), "select ?1, ?2", &[Cell::Int(1)]).unwrap_err();
    assert!(e.to_string().contains("2 parameters"), "{e}");
    assert!(query_all_bound(c.as_mut(), "select 1", &[Cell::Int(1)]).is_err());
}

#[test]
fn sqlite_import_binds_and_splits_batches() {
    let (_dir, mut c) = sqlite();
    let rows: Vec<Vec<Cell>> = (0..300)
        .map(|i| vec![Cell::Int(i), Cell::Text(format!("v'{i}\\\0")), if i % 2 == 0 { Cell::Null } else { Cell::Text(String::new()) }])
        .collect();
    let data = ImportData { columns: vec!["a".into(), "b".into(), "c".into()], rows };
    let cols = vec!["id".to_string(), "s".to_string(), "b".to_string()];
    let stmts = insert_bound(DbKind::Sqlite, "", "t", &cols, &[Some(0), Some(1), Some(2)], &data, 100).unwrap();
    assert_eq!(stmts.len(), 3);
    assert!(stmts.iter().all(|s| !s.sql.contains("v'")));
    let n = run_import_bound(c.as_mut(), &stmts, &mut |_| {}).unwrap();
    assert_eq!(n, 300);
    let (_, back, _) = query_all(c.as_mut(), "select s, b from t where id in (7, 8) order by id").unwrap();
    assert_eq!(back[0], vec![Cell::Text("v'7\\\0".into()), Cell::Text(String::new())]);
    assert_eq!(back[1], vec![Cell::Text("v'8\\\0".into()), Cell::Null]);
    // SQL Server's 2100-parameter limit splits the batches.
    let wide = insert_bound(DbKind::SqlServer, "dbo", "t", &cols, &[Some(0), Some(1), Some(2)], &data, 1000).unwrap();
    assert!(wide.iter().all(|s| s.params.len() <= 2000), "{}", wide.len());
    assert_eq!(wide.iter().map(|s| s.params.len()).sum::<usize>(), 300 + 300 + 150);
}

// ---------------------------------------------------------------- PostgreSQL

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

struct Schema(Box<dyn Connection>, String);

impl Drop for Schema {
    fn drop(&mut self) {
        let _ = query_all(self.0.as_mut(), "ROLLBACK");
        let _ = query_all(self.0.as_mut(), &format!("DROP SCHEMA IF EXISTS {} CASCADE", self.1));
    }
}

fn pg_schema(tag: &str) -> Option<Schema> {
    let mut c = match connect(&pg_config(), std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            return None;
        }
    };
    let name = format!("codecraft_bind_{tag}_{}", std::process::id());
    for sql in [
        format!("DROP SCHEMA IF EXISTS {name} CASCADE"),
        format!("CREATE SCHEMA {name}"),
        "SET TIME ZONE 'UTC'".to_string(),
        format!(
            "CREATE TABLE {name}.t (id int PRIMARY KEY, s text, b bytea, i2 smallint, i8 bigint, f8 float8, \
             n numeric, j json, jb jsonb, u uuid, ts timestamp, tz timestamptz, d date, ok boolean)"
        ),
    ] {
        query_all(c.as_mut(), &sql).unwrap();
    }
    Some(Schema(c, name))
}

#[test]
fn postgres_grid_edits_round_trip_hostile_values() {
    let Some(mut sc) = pg_schema("edit") else { return };
    let schema = sc.1.clone();
    let c = sc.0.as_mut();
    let texts = hostile_texts();
    let mut changes = Vec::new();
    for (i, s) in texts.iter().enumerate() {
        changes.push(RowChange::Insert {
            values: vec![("id".into(), Cell::Int(i as i64 + 1)), ("s".into(), Cell::Text(s.clone()))],
        });
    }
    let n = texts.len() as i64;
    let typed = |id: i64, vals: &[(&str, Cell)]| {
        let mut v = vec![("id".to_string(), Cell::Int(id))];
        v.extend(vals.iter().map(|(k, c)| (k.to_string(), c.clone())));
        RowChange::Insert { values: v }
    };
    changes.push(typed(
        n + 1,
        &[
            ("s", Cell::Null),
            ("b", Cell::Bytes(all_bytes())),
            ("i2", Cell::Int(i64::from(i16::MIN))),
            ("i8", Cell::Int(i64::MIN)),
            ("f8", Cell::Float(f64::MAX)),
            ("n", Cell::Other("-12345678901234567890.123456789012345678".into())),
            ("j", Cell::Text("{\"a\": \"it's \\\\ \\u0000-free\", \"b\": [1, 2.5e300]}".into())),
            ("jb", Cell::Text("{\"k\": \"🦀\"}".into())),
            ("u", Cell::Other("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into())),
            ("ts", Cell::Other("2024-02-29 23:59:59.999999".into())),
            ("tz", Cell::Other("2024-03-10 01:59:59.5-08:00".into())),
            ("d", Cell::Other("0001-01-01".into())),
            ("ok", Cell::Bool(true)),
        ],
    ));
    changes.push(typed(
        n + 2,
        &[
            ("s", Cell::Text(String::new())),
            ("b", Cell::Bytes(Vec::new())),
            ("i2", Cell::Int(i64::from(i16::MAX))),
            ("i8", Cell::Int(i64::MAX)),
            ("f8", Cell::Float(f64::NAN)),
            ("n", Cell::Int(0)),
            ("tz", Cell::Other("2024-11-03 01:30:00+05:45".into())),
            ("ok", Cell::Bool(false)),
        ],
    ));
    changes.push(typed(n + 3, &[("f8", Cell::Float(f64::NEG_INFINITY)), ("n", Cell::Text("NaN".into()))]));
    apply_changes(c, &schema, "t", &changes, false).unwrap();

    let (_, rows, _) = query_all(c, &format!("SELECT id, s FROM {schema}.t WHERE id <= {n} ORDER BY id")).unwrap();
    for (i, s) in texts.iter().enumerate() {
        assert_eq!(rows[i][1], Cell::Text(s.clone()), "row {i}");
    }
    let q = format!(
        "SELECT s IS NULL, b, i2, i8, f8::text, n::text, j::text, jb::text, u::text, ts::text, tz::text, d::text, ok \
         FROM {schema}.t WHERE id > {n} ORDER BY id"
    );
    let (_, rows, _) = query_all(c, &q).unwrap();
    let r = &rows[0];
    assert_eq!(r[0], Cell::Bool(true), "NULL stays NULL");
    assert_eq!(r[1], Cell::Bytes(all_bytes()));
    assert_eq!(r[2], Cell::Int(i64::from(i16::MIN)));
    assert_eq!(r[3], Cell::Int(i64::MIN));
    assert_eq!(r[4].to_plain().parse::<f64>().unwrap(), f64::MAX);
    assert_eq!(r[5].to_plain(), "-12345678901234567890.123456789012345678");
    assert_eq!(r[6].to_plain(), "{\"a\": \"it's \\\\ \\u0000-free\", \"b\": [1, 2.5e300]}");
    assert_eq!(r[7].to_plain(), "{\"k\": \"🦀\"}");
    assert_eq!(r[8].to_plain(), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(r[9].to_plain(), "2024-02-29 23:59:59.999999");
    assert_eq!(r[10].to_plain(), "2024-03-10 09:59:59.5+00");
    assert_eq!(r[11].to_plain(), "0001-01-01");
    assert_eq!(r[12], Cell::Bool(true));
    let r = &rows[1];
    assert_eq!(r[0], Cell::Bool(false), "empty text is not NULL");
    assert_eq!(r[1], Cell::Bytes(Vec::new()));
    assert_eq!((r[2].clone(), r[3].clone()), (Cell::Int(i64::from(i16::MAX)), Cell::Int(i64::MAX)));
    assert_eq!(r[4].to_plain(), "NaN");
    assert_eq!(r[10].to_plain(), "2024-11-02 19:45:00+00");
    assert_eq!(r[12], Cell::Bool(false));
    assert_eq!(rows[2][4].to_plain(), "-Infinity");
    assert_eq!(rows[2][5].to_plain(), "NaN");

    // Out-of-range values and NUL in text fail with the server's (or a clear) error, and
    // nothing is saved.
    let before = query_all(c, &format!("SELECT count(*) FROM {schema}.t")).unwrap().1;
    let f = apply_changes(c, &schema, "t", &[typed(900, &[("i2", Cell::Int(i64::from(i16::MAX) + 1))])], false)
        .unwrap_err();
    assert!(f.error.contains("out of range"), "{f}");
    assert!(f.statement.as_deref().unwrap_or("").contains("-- parameters: $1 = 900, $2 = 32768"), "{f}");
    let f = apply_changes(c, &schema, "t", &[typed(901, &[("s", Cell::Text("a\0b".into()))])], false).unwrap_err();
    assert!(f.error.contains("NUL"), "{f}");
    assert!(f.rolled_back);
    assert_eq!(query_all(c, &format!("SELECT count(*) FROM {schema}.t")).unwrap().1, before);

    // Updates and deletes find rows by hostile keys.
    query_all(c, &format!("CREATE TABLE {schema}.k (code text PRIMARY KEY, v int)")).unwrap();
    let key = "a'b\\c $1 🦀".to_string();
    let ins = RowChange::Insert { values: vec![("code".into(), Cell::Text(key.clone())), ("v".into(), Cell::Int(1))] };
    apply_changes(c, &schema, "k", &[ins], false).unwrap();
    let k = vec![("code".into(), Cell::Text(key.clone()))];
    let up = RowChange::Update { key: k.clone(), set: vec![("v".into(), Cell::Text("2".into()))] };
    assert_eq!(apply_changes(c, &schema, "k", &[up], false).unwrap().affected, 1);
    assert_eq!(query_all(c, &format!("SELECT v FROM {schema}.k")).unwrap().1, vec![vec![Cell::Int(2)]]);
    assert_eq!(apply_changes(c, &schema, "k", &[RowChange::Delete { key: k }], false).unwrap().affected, 1);
}

#[test]
fn postgres_console_parameters_and_import_bind() {
    let Some(mut sc) = pg_schema("params") else { return };
    let schema = sc.1.clone();
    let c = sc.0.as_mut();
    query_all(c, &format!("INSERT INTO {schema}.t (id, s) VALUES (1, 'x'), (2, '007')")).unwrap();
    let mut v = ParamValues::new();
    v.insert("s".into(), ParamValue::Value(Cell::Text("x' OR '1'='1".into())));
    v.insert("id".into(), ParamValue::Value(Cell::Int(1)));
    let b = bind::prepare(DbKind::Postgres, &format!("SELECT id FROM {schema}.t WHERE s = :s OR id = :id"), &v).unwrap();
    assert!(b.sql.ends_with("WHERE s = $1 OR id = $2"), "{}", b.sql);
    assert_eq!(query_all_bound(c, &b.sql, &b.params).unwrap().1, vec![vec![Cell::Int(1)]]);
    // Untyped placeholders are text; the server infers numeric types from the context.
    let (_, rows, _) = query_all_bound(c, "SELECT $1::text, $2 + 1, $3::numeric * 2", &[
        Cell::Text("🦀".into()),
        Cell::Int(41),
        Cell::Other("0.1".into()),
    ])
    .unwrap();
    assert_eq!(rows[0][0], Cell::Text("🦀".into()));
    assert_eq!(rows[0][1].to_plain(), "42");
    assert_eq!(rows[0][2].to_plain(), "0.2");
    let e = query_all_bound(c, "SELECT $1, $2", &[Cell::Int(1)]).unwrap_err();
    assert!(e.to_string().contains("2 parameters"), "{e}");

    // Import with bound multi-row INSERTs.
    let rows: Vec<Vec<Cell>> =
        (10..260).map(|i| vec![Cell::Int(i), Cell::Text(format!("it's {i} \\ 🦀")), Cell::Bytes(vec![0, i as u8])]).collect();
    let data = ImportData { columns: vec!["id".into(), "s".into(), "b".into()], rows };
    let cols = vec!["id".to_string(), "s".to_string(), "b".to_string()];
    let stmts: Vec<Bound> = insert_bound(DbKind::Postgres, &schema, "t", &cols, &[Some(0), Some(1), Some(2)], &data, 100).unwrap();
    assert_eq!(stmts.len(), 3);
    assert_eq!(run_import_bound(c, &stmts, &mut |_| {}).unwrap(), 250);
    let (_, back, _) = query_all(c, &format!("SELECT s, b FROM {schema}.t WHERE id = 99")).unwrap();
    assert_eq!(back[0], vec![Cell::Text("it's 99 \\ 🦀".into()), Cell::Bytes(vec![0, 99])]);
}
