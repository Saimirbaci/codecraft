//! ClickHouse over HTTP against a scripted fake server on 127.0.0.1 (streaming pages,
//! exceptions, cancel with KILL QUERY, sessions, settings), and a real server only when
//! `CODECRAFT_TEST_CLICKHOUSE_HOST`/`_PORT` (`_USER`, `_PASSWORD`) say where one runs; then only
//! a throwaway `codecraft_test_<pid>` database is created and dropped.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use codecraft_db::{Cell, CollectSink, Connection, DataSourceConfig, DbError, DbKind, TableKind, connect, query_all};

/// One request as the fake server saw it.
#[derive(Clone, Debug, Default)]
struct Req {
    params: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: String,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00"), 16).unwrap_or(0));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &TcpStream) -> Option<Req> {
    let mut r = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    let mut req = Req::default();
    if let Some((_, q)) = target.split_once('?') {
        for kv in q.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            req.params.insert(decode(k), decode(v));
        }
    }
    loop {
        line.clear();
        r.read_line(&mut line).ok()?;
        let l = line.trim();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            req.headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = req.headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    req.body = String::from_utf8_lossy(&body).into_owned();
    Some(req)
}

/// What the fake answers: status, extra headers, body chunks (with a pause before each).
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    chunks: Vec<(Duration, String)>,
}

fn ok(body: impl Into<String>) -> Answer {
    Answer { status: 200, headers: Vec::new(), chunks: vec![(Duration::ZERO, body.into())] }
}

fn fake<F>(answer: F) -> (u16, mpsc::Receiver<Req>)
where
    F: Fn(&Req) -> Answer + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (tx, answer) = (tx.clone(), answer.clone());
            std::thread::spawn(move || {
                let Some(req) = read_request(&stream) else { return };
                let _ = tx.send(req.clone());
                let a = answer(&req);
                let mut head = format!("HTTP/1.1 {} X\r\nConnection: close\r\nContent-Type: text/plain\r\n", a.status);
                for (k, v) in &a.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                for (pause, chunk) in a.chunks {
                    std::thread::sleep(pause);
                    if stream.write_all(chunk.as_bytes()).is_err() || stream.flush().is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, rx)
}

fn cfg(port: u16) -> DataSourceConfig {
    let mut c = DataSourceConfig {
        id: "ch".into(),
        name: "ch".into(),
        kind: DbKind::ClickHouse,
        host: "127.0.0.1".into(),
        port: Some(port),
        user: "analyst".into(),
        database: "web".into(),
        ..Default::default()
    };
    c.options.insert("connect_timeout".into(), "5".into());
    c.options.insert("max_execution_time".into(), "60".into());
    c
}

fn rows_body(names: &[&str], types: &[&str], rows: &[String]) -> String {
    let mut s = format!("{}\n{}\n", serde_json::to_string(names).unwrap(), serde_json::to_string(types).unwrap());
    for r in rows {
        s.push_str(r);
        s.push('\n');
    }
    s
}

fn standard(req: &Req) -> Answer {
    let sql = req.body.as_str();
    if sql == "SELECT 1" {
        return ok(rows_body(&["1"], &["UInt8"], &["[1]".into()]));
    }
    if sql == "SELECT version()" {
        return ok(rows_body(&["version()"], &["String"], &["[\"24.3.1.1\"]".into()]));
    }
    if sql.starts_with("SELECT currentDatabase()") {
        return ok(rows_body(&["db"], &["String"], &["[\"web\"]".into()]));
    }
    if sql.contains("FROM system.databases") {
        return ok(rows_body(&["name"], &["String"], &["[\"default\"]".into(), "[\"web\"]".into()]));
    }
    if sql.contains("FROM system.tables WHERE database = 'web' AND NOT is_temporary") {
        return ok(rows_body(
            &["name", "engine"],
            &["String", "String"],
            &["[\"hits\",\"MergeTree\"]".into(), "[\"daily\",\"MaterializedView\"]".into(), "[\"v\",\"View\"]".into()],
        ));
    }
    if sql.contains("FROM system.columns") {
        return ok(rows_body(
            &["name", "type", "default_kind", "default_expression", "is_in_primary_key"],
            &["String", "String", "String", "String", "UInt8"],
            &[
                "[\"id\",\"UInt64\",\"\",\"\",1]".into(),
                "[\"url\",\"Nullable(String)\",\"\",\"\",0]".into(),
                "[\"at\",\"DateTime\",\"DEFAULT\",\"now()\",0]".into(),
            ],
        ));
    }
    if sql.contains("primary_key, sorting_key") {
        return ok(rows_body(&["primary_key", "sorting_key"], &["String", "String"], &["[\"id\",\"id, at\"]".into()]));
    }
    if sql.contains("data_skipping_indices") {
        return ok(rows_body(
            &["name", "expr", "type"],
            &["String", "String", "String"],
            &["[\"u\",\"url\",\"bloom_filter\"]".into()],
        ));
    }
    if sql.starts_with("SELECT count()") {
        return ok(rows_body(&["count()"], &["UInt64"], &["[\"1234\"]".into()]));
    }
    if sql.starts_with("SELECT number") {
        let rows: Vec<String> = (0..1200).map(|i| format!("[\"{i}\",\"n{i}\",null,1.5]")).collect();
        return ok(rows_body(
            &["number", "s", "maybe", "f"],
            &["UInt64", "String", "Nullable(Int32)", "Float64"],
            &rows,
        ));
    }
    if sql.starts_with("SELECT {p1:") {
        // Echo the parameters as the server received them.
        let p = |k: &str| serde_json::to_string(req.params.get(k).map(String::as_str).unwrap_or("<missing>")).unwrap();
        return ok(rows_body(&["a", "b"], &["String", "String"], &[format!("[{},{}]", p("param_p1"), p("param_p2"))]));
    }
    if sql.starts_with("SELECT bad") {
        return Answer {
            status: 404,
            headers: vec![("X-ClickHouse-Exception-Code".into(), "47".into())],
            chunks: vec![(
                Duration::ZERO,
                "Code: 47. DB::Exception: Missing columns: 'bad'. (UNKNOWN_IDENTIFIER) (version 24.3.1.1)\n".into(),
            )],
        };
    }
    if sql.starts_with("SELECT midstream") {
        return ok(format!(
            "{}Code: 241. DB::Exception: Memory limit (total) exceeded. (MEMORY_LIMIT_EXCEEDED) (version 24.3.1.1)\n",
            rows_body(&["x"], &["UInt8"], &["[1]".into()])
        ));
    }
    if sql.starts_with("SELECT garbage") {
        return ok("not json at all\n");
    }
    if sql.starts_with("SELECT sleep") {
        return Answer {
            status: 200,
            headers: Vec::new(),
            chunks: vec![
                (Duration::ZERO, rows_body(&["x"], &["UInt8"], &[])),
                (
                    Duration::from_secs(3),
                    "Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)\n".into(),
                ),
            ],
        };
    }
    if sql.starts_with("KILL QUERY") || sql.starts_with("CREATE") || sql.starts_with("USE") || sql.starts_with("INSERT")
    {
        return ok("");
    }
    Answer { status: 500, headers: Vec::new(), chunks: vec![(Duration::ZERO, format!("unexpected: {sql}"))] }
}

#[test]
fn fake_server_queries_pages_and_introspection() {
    let (port, rx) = fake(standard);
    let mut conn = connect(&cfg(port), Some("s3cret")).unwrap();
    let first: Vec<Req> = rx.try_iter().collect();
    let r = first.first().unwrap();
    assert_eq!(r.headers.get("x-clickhouse-user").map(String::as_str), Some("analyst"));
    assert_eq!(r.headers.get("x-clickhouse-key").map(String::as_str), Some("s3cret"));
    assert_eq!(r.params.get("database").map(String::as_str), Some("web"));
    assert_eq!(r.params.get("max_execution_time").map(String::as_str), Some("60"));
    assert_eq!(r.params.get("default_format").map(String::as_str), Some("JSONCompactEachRowWithNamesAndTypes"));
    let session = r.params.get("session_id").cloned().unwrap();
    assert_eq!(conn.server_version().unwrap(), "ClickHouse 24.3.1.1");

    // Pages stream to the sink.
    let mut sink = CollectSink::default();
    let sum = conn.execute("SELECT number, toString(number) FROM numbers(1200)", 500, &mut sink).unwrap();
    assert_eq!(sink.pages, 3);
    assert_eq!(sum.rows_fetched, 1200);
    assert_eq!(sink.columns[0].type_name, "UInt64");
    assert_eq!(sink.rows[7], vec![Cell::UInt(7), Cell::Text("n7".into()), Cell::Null, Cell::Float(1.5)]);
    let req: Vec<Req> = rx.try_iter().collect();
    assert!(req.iter().all(|r| r.params.get("session_id") == Some(&session)), "one session per connection");
    assert!(req.iter().all(|r| r.params.contains_key("query_id")));

    // Statements without results.
    let (cols, rows, sum) = query_all(conn.as_mut(), "INSERT INTO t VALUES (1)").unwrap();
    assert!(cols.is_empty() && rows.is_empty() && sum.complete);
    query_all(conn.as_mut(), "USE `analytics`").unwrap();
    query_all(conn.as_mut(), "SELECT 1").unwrap();
    let last = rx.try_iter().last().unwrap();
    assert_eq!(last.params.get("database").map(String::as_str), Some("analytics"));

    // Errors.
    match query_all(conn.as_mut(), "SELECT bad") {
        Err(DbError::Query(e)) => {
            assert_eq!(e, "Code: 47. DB::Exception: Missing columns: 'bad'. (UNKNOWN_IDENTIFIER)")
        }
        other => panic!("{other:?}"),
    }
    match query_all(conn.as_mut(), "SELECT midstream") {
        Err(DbError::Query(e)) => assert!(e.contains("MEMORY_LIMIT_EXCEEDED"), "{e}"),
        other => panic!("{other:?}"),
    }
    // Output in a FORMAT of the statement's own choosing shows as lines.
    let (cols, rows, _) = query_all(conn.as_mut(), "SELECT garbage").unwrap();
    assert_eq!(cols[0].name, "output");
    assert_eq!(rows, vec![vec![Cell::Text("not json at all".into())]]);
    assert!(!conn.is_broken());

    // Introspection.
    query_all(conn.as_mut(), "USE web").unwrap();
    let info = conn.database_info().unwrap();
    assert_eq!(info.schemas, ["default", "web"]);
    assert_eq!(info.default_schema.as_deref(), Some("web"));
    let tables = conn.tables("web").unwrap();
    assert_eq!(
        tables.iter().map(|t| t.kind).collect::<Vec<_>>(),
        [TableKind::Table, TableKind::MaterializedView, TableKind::View]
    );
    let d = conn.table_details("web", "hits").unwrap();
    assert_eq!(d.columns.len(), 3);
    assert!(d.columns[0].primary_key && !d.columns[0].nullable);
    assert!(d.columns[1].nullable);
    assert_eq!(d.columns[2].default.as_deref(), Some("now()"));
    assert_eq!(d.indexes[0].name, "PRIMARY KEY");
    assert_eq!(d.indexes[1].columns, ["id", "at"]);
    assert_eq!(d.indexes[2].name, "u (bloom_filter)");
    assert_eq!(conn.row_count("web", "hits").unwrap(), 1234);
    assert_eq!(conn.transaction_status(), codecraft_db::tx::TxStatus::Idle);
    assert!(codecraft_db::tx::begin(conn.as_mut(), None).is_err(), "no transactions");
}

#[test]
fn parameters_travel_as_http_query_parameters() {
    use codecraft_db::bind::{ParamValue, ParamValues, clickhouse_param_text, prepare};
    let (port, rx) = fake(standard);
    let mut conn = connect(&cfg(port), None).unwrap();
    let _: Vec<Req> = rx.try_iter().collect();
    let mut v = ParamValues::new();
    let hostile = "it's \\ a\ttab\nline \0 🦀 ' OR 1=1 --";
    v.insert(":s".into(), ParamValue::Value(Cell::Text(hostile.into())));
    v.insert("n".into(), ParamValue::Value(Cell::Int(-5)));
    let b = prepare(DbKind::ClickHouse, "SELECT :s, {n:Int32}", &v).unwrap();
    assert_eq!(b.sql, "SELECT {p1:String}, {p2:Int32}");
    let mut sink = CollectSink::default();
    conn.execute_bound(&b.sql, &b.params, 500, &mut sink).unwrap();
    let req = rx.try_iter().last().unwrap();
    assert_eq!(req.body, "SELECT {p1:String}, {p2:Int32}", "values never enter the SQL");
    let expected = clickhouse_param_text(&Cell::Text(hostile.into()));
    assert_eq!(expected, "it's \\\\ a\\ttab\\nline \\0 🦀 ' OR 1=1 --");
    assert_eq!(req.params.get("param_p1"), Some(&expected));
    assert_eq!(req.params.get("param_p2").map(String::as_str), Some("-5"));
    assert_eq!(sink.rows[0][1], Cell::Text("-5".into()));
}

#[test]
fn cancel_kills_the_query_by_id() {
    let (port, rx) = fake(standard);
    let mut conn = connect(&cfg(port), None).unwrap();
    let canceller = conn.canceller();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        canceller.cancel().unwrap();
    });
    let started = Instant::now();
    let r = query_all(conn.as_mut(), "SELECT sleep(3)");
    t.join().unwrap();
    assert_eq!(r.err(), Some(DbError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(30));
    let reqs: Vec<Req> = rx.try_iter().collect();
    let query = reqs.iter().find(|r| r.body.starts_with("SELECT sleep")).unwrap();
    let id = query.params.get("query_id").unwrap();
    let kill = reqs.iter().find(|r| r.body.starts_with("KILL QUERY")).expect("KILL QUERY was sent");
    assert!(kill.body.contains(id.as_str()), "{}", kill.body);
    assert!(!kill.params.contains_key("session_id"), "the kill must not wait for the busy session");
}

#[test]
fn connection_errors_and_read_only() {
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    assert!(matches!(connect(&cfg(free), None), Err(DbError::Connect(_))));
    let (port, _rx) = fake(|_| Answer {
        status: 516,
        headers: vec![("X-ClickHouse-Exception-Code".into(), "516".into())],
        chunks: vec![(Duration::ZERO, "Code: 516. DB::Exception: analyst: Authentication failed".into())],
    });
    match connect(&cfg(port), Some("bad")) {
        Err(DbError::Connect(e)) => assert!(e.contains("Authentication failed"), "{e}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("connected"),
    }
    let (port, rx) = fake(standard);
    let mut c = cfg(port);
    c.read_only = true;
    c.options.insert("setting.max_threads".into(), "2".into());
    c.options.insert("setting.bad name".into(), "x".into());
    let _conn = connect(&c, None).unwrap();
    let r = rx.try_iter().next().unwrap();
    assert_eq!(r.params.get("readonly").map(String::as_str), Some("2"));
    assert_eq!(r.params.get("max_threads").map(String::as_str), Some("2"));
    assert!(!r.params.contains_key("bad name"));
    c.options.insert("max_execution_time".into(), "soon".into());
    assert!(matches!(connect(&c, None), Err(DbError::Config(_))));
}

// ---------------------------------------------------------------- a real server

#[test]
fn real_clickhouse_when_configured() {
    let Ok(host) = std::env::var("CODECRAFT_TEST_CLICKHOUSE_HOST") else {
        eprintln!("skipping: CODECRAFT_TEST_CLICKHOUSE_HOST is not set");
        return;
    };
    let port: u16 = std::env::var("CODECRAFT_TEST_CLICKHOUSE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8123);
    let mut c = cfg(port);
    c.host = host;
    c.user = std::env::var("CODECRAFT_TEST_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into());
    c.database = String::new();
    let pw = std::env::var("CODECRAFT_TEST_CLICKHOUSE_PASSWORD").ok();
    let mut conn = match connect(&c, pw.as_deref()) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("skipping: no ClickHouse ({e})");
            return;
        }
    };
    let db = format!("codecraft_test_{}", std::process::id());
    struct Drop_<'a>(&'a mut dyn Connection, String);
    impl Drop for Drop_<'_> {
        fn drop(&mut self) {
            let _ = query_all(self.0, &format!("DROP DATABASE IF EXISTS {}", self.1));
        }
    }
    let g = Drop_(conn.as_mut(), db.clone());
    let conn = &mut *g.0;
    assert!(conn.server_version().unwrap().starts_with("ClickHouse "));
    for sql in [
        format!("CREATE DATABASE {db}"),
        format!("CREATE TABLE {db}.t (id UInt64, s Nullable(String)) ENGINE = MergeTree ORDER BY id"),
        format!("INSERT INTO {db}.t SELECT number, toString(number) FROM numbers(1500)"),
    ] {
        query_all(conn, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let mut sink = CollectSink::default();
    let sum = conn.execute(&format!("SELECT * FROM {db}.t ORDER BY id"), 500, &mut sink).unwrap();
    assert_eq!(sum.rows_fetched, 1500);
    assert_eq!(sink.rows[3], vec![Cell::UInt(3), Cell::Text("3".into())]);
    assert!(conn.tables(&db).unwrap().iter().any(|t| t.name == "t"));
    assert_eq!(conn.row_count(&db, "t").unwrap(), 1500);
    let plan = codecraft_db::explain::explain(conn, &format!("SELECT * FROM {db}.t WHERE id = 1"), false).unwrap();
    assert!(!plan.roots.is_empty());
}
