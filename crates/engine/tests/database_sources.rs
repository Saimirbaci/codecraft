//! Track DD through engine commands: Redis (a `redis-server` the test starts on a free port in a
//! temp dir, skipped when not installed) with consoles, the key browser, edits and the guards;
//! ClickHouse against a scripted HTTP server on 127.0.0.1 (consoles, the tree, no transactions);
//! DuckDB through a fake CLI script; the data source dialog's defaults for every new kind.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::{ConnStatus, DataSourceForm};
use codecraft_engine::db::{Cell, DbKind, MemoryStore};

fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
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
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    s.db.history.set_dir(dir.join(".codecraft/test-history"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "project", |s| s.ws.project.is_some() && !s.jobs_running());
    s
}

/// Rows of the last result after running `sql` on a data source.
fn rows_of(s: &mut Session, id: &str, sql: &str) -> Vec<Vec<Cell>> {
    let prev = s.db.active_result;
    s.execute("db.execute", &json!({ "dataSource": id, "sql": sql })).unwrap();
    pump_until(s, sql, |s| {
        !s.db.busy() && s.db.active_result != prev
            || !s.db.busy() && s.db.output.last().is_some_and(|l| l.text.contains("rror"))
    });
    s.db.active().map(|r| r.rows.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------- Redis

struct RedisServer {
    child: Child,
    _dir: tempfile::TempDir,
    port: u16,
}

impl Drop for RedisServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn redis_server() -> Option<RedisServer> {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].iter().map(std::path::PathBuf::from));
    let Some(bin) = dirs.into_iter().map(|d| d.join("redis-server")).find(|p| p.is_file()) else {
        eprintln!("skipping: redis-server is not installed");
        return None;
    };
    let dir = tempfile::tempdir().ok()?;
    let port = TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok()?.port();
    let child = Command::new(bin)
        .args(["--port", &port.to_string(), "--bind", "127.0.0.1", "--save", "", "--appendonly", "no"])
        .arg("--dir")
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let srv = RedisServer { child, _dir: dir, port };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(mut c) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            if c.write_all(b"PING\r\n").is_ok() {
                let mut buf = [0u8; 7];
                if c.read_exact(&mut buf).is_ok() && &buf == b"+PONG\r\n" {
                    return Some(srv);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("skipping: redis-server did not start");
    None
}

#[test]
fn redis_consoles_key_browser_and_guards() {
    let Some(srv) = redis_server() else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "redis", "name": "Cache", "host": "127.0.0.1", "port": srv.port.to_string(), "database": "0" }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "connected", |s| {
        s.db.source(&id).is_some_and(|d| matches!(d.status, ConnStatus::Connected(_)) && d.tree.info.is_some())
    });
    let info = s.db.source(&id).unwrap().tree.info.clone().unwrap();
    assert_eq!(info.schemas.len(), 16);
    assert_eq!(info.default_schema.as_deref(), Some("db0"));

    // A console runs Redis commands, one per line, quoting respected.
    s.execute("db.execute", &json!({ "dataSource": id, "sql": "SET user:1:name \"Ada Lovelace\"\nHSET user:1 a 1 b 2\nRPUSH queue x y z\nSET plain v" }))
        .unwrap();
    pump_until(&mut s, "writes", |s| !s.db.busy());
    assert_eq!(rows_of(&mut s, &id, "GET user:1:name"), vec![vec![Cell::Text("Ada Lovelace".into())]]);
    let r = s.db.active().unwrap();
    assert_eq!(r.columns[0].name, "value");
    let rows = rows_of(&mut s, &id, "HGETALL user:1");
    assert_eq!(s.db.active().unwrap().columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["field", "value"]);
    assert_eq!(rows.len(), 2);
    // Manual commit is refused: no transactions.
    assert!(
        s.execute("db.tx.setMode", &json!({ "dataSource": id, "mode": "manual" })).is_err()
            || !s.db.manual_commit("console:x", &id)
    );

    // The key browser.
    s.execute("db.redis.open", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "scan", |s| s.db.redis.get(&id).is_some_and(|b| !b.scanning && b.version.is_some()));
    let b = s.db.redis.get(&id).unwrap();
    assert_eq!(b.keys.len(), 4, "{:?}", b.error);
    assert!(b.cursor.is_none());
    let st = s.execute("db.redis.state", &json!({ "dataSource": id, "tree": true })).unwrap();
    let tree: Vec<String> =
        st["tree"].as_array().unwrap().iter().map(|r| r["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(tree, ["user", "plain", "queue"]);
    s.execute("db.redis.toggleNamespace", &json!({ "prefix": "user:" })).unwrap();
    assert_eq!(s.db.redis.get(&id).unwrap().tree(':').len(), 5);
    // Pattern and type filters (SCAN MATCH/TYPE).
    s.execute("db.redis.scan", &json!({ "pattern": "user:*", "type": "hash" })).unwrap();
    pump_until(&mut s, "filtered scan", |s| s.db.redis.get(&id).is_some_and(|b| !b.scanning));
    let names: Vec<String> = s.db.redis.get(&id).unwrap().keys.iter().map(|k| k.name.clone()).collect();
    assert_eq!(names, ["user:1"]);
    // A value with its type, TTL and fields.
    s.execute("db.redis.openKey", &json!({ "key": "user:1" })).unwrap();
    pump_until(&mut s, "value", |s| s.db.redis.get(&id).is_some_and(|b| !b.loading_value && b.value.is_some()));
    let v = s.db.redis.get(&id).unwrap().value.clone().unwrap();
    assert_eq!((v.kind.as_str(), v.ttl_ms, v.length), ("hash", -1, Some(2)));
    assert_eq!(v.columns, ["field", "value"]);
    // Edits.
    s.execute("db.redis.hashSet", &json!({ "key": "user:1", "field": "c", "value": "3" })).unwrap();
    pump_until(&mut s, "hset", |s| {
        s.db.redis
            .get(&id)
            .is_some_and(|b| !b.writing && !b.loading_value && b.value.as_ref().is_some_and(|v| v.rows.len() == 3))
    });
    s.execute("db.redis.setTtl", &json!({ "key": "user:1", "seconds": 600 })).unwrap();
    pump_until(&mut s, "ttl", |s| {
        s.db.redis
            .get(&id)
            .is_some_and(|b| !b.writing && !b.loading_value && b.value.as_ref().is_some_and(|v| v.ttl_ms > 0))
    });
    assert!(s.execute("db.redis.setTtl", &json!({ "key": "user:1", "seconds": "soon" })).is_err());
    s.execute("db.redis.scan", &json!({ "pattern": "*", "type": "all" })).unwrap();
    pump_until(&mut s, "rescan", |s| s.db.redis.get(&id).is_some_and(|b| !b.scanning));
    s.execute("db.redis.renameKey", &json!({ "key": "plain", "newName": "user:1:name" })).unwrap();
    pump_until(&mut s, "rename refused", |s| s.db.redis.get(&id).is_some_and(|b| !b.writing));
    assert!(s.db.redis.get(&id).unwrap().message.as_deref().unwrap_or("").contains("already exists"));
    s.execute("db.redis.renameKey", &json!({ "key": "plain", "newName": "plain2" })).unwrap();
    pump_until(&mut s, "rename", |s| s.db.redis.get(&id).is_some_and(|b| !b.writing));
    assert!(s.db.redis.get(&id).unwrap().keys.iter().any(|k| k.name == "plain2"));
    s.execute("db.redis.newKey", &json!({ "key": "z", "type": "zset", "value": "m", "score": 2.5 })).unwrap();
    pump_until(&mut s, "new key", |s| {
        s.db.redis
            .get(&id)
            .is_some_and(|b| !b.writing && !b.loading_value && b.value.as_ref().is_some_and(|v| v.kind == "zset"))
    });
    s.execute("db.redis.deleteKeys", &json!({ "keys": ["plain2", "queue"] })).unwrap();
    pump_until(&mut s, "delete", |s| s.db.redis.get(&id).is_some_and(|b| !b.writing));
    assert_eq!(rows_of(&mut s, &id, "EXISTS plain2 queue"), vec![vec![Cell::Int(0)]]);
    assert_eq!(rows_of(&mut s, &id, "ZSCORE z m"), vec![vec![Cell::Text("2.5".into())]]);

    // Database selection.
    s.execute("db.redis.selectDb", &json!({ "db": 2 })).unwrap();
    pump_until(&mut s, "db2", |s| s.db.redis.get(&id).is_some_and(|b| b.db == 2 && !b.scanning));
    assert!(s.db.redis.get(&id).unwrap().keys.is_empty());

    // Production: edits and console writes need confirmation; read-only refuses them.
    s.execute("db.editDataSource", &json!({ "dataSource": id, "confirmWrites": true })).unwrap();
    s.execute("db.redis.open", &json!({ "dataSource": id, "db": 0 })).unwrap();
    pump_until(&mut s, "reopen", |s| s.db.redis.get(&id).is_some_and(|b| !b.scanning && b.version.is_some()));
    let e = s.execute("db.redis.setString", &json!({ "key": "user:1:name", "value": "x" })).unwrap_err();
    assert!(e.to_string().contains("SET user:1:name x KEEPTTL"), "{e}");
    let r = s.execute("db.execute", &json!({ "dataSource": id, "sql": "DEL user:1:name" })).unwrap();
    assert_eq!(r["confirm"], json!(true));
    s.execute("db.cancelExecution", &Value::Null).unwrap();
    let r = s.execute("db.execute", &json!({ "dataSource": id, "sql": "GET user:1:name" })).unwrap();
    assert!(r.get("confirm").is_none(), "reads run at once");
    pump_until(&mut s, "read", |s| !s.db.busy());
    s.execute("db.redis.setString", &json!({ "key": "user:1:name", "value": "Grace", "confirm": true })).unwrap();
    pump_until(&mut s, "confirmed write", |s| s.db.redis.get(&id).is_some_and(|b| !b.writing));
    assert_eq!(rows_of(&mut s, &id, "GET user:1:name"), vec![vec![Cell::Text("Grace".into())]]);
    s.execute("db.editDataSource", &json!({ "dataSource": id, "readOnly": true })).unwrap();
    let e = s.execute("db.redis.deleteKeys", &json!({ "key": "user:1:name", "confirm": true })).unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
    let e = s.execute("db.execute", &json!({ "dataSource": id, "sql": "FLUSHALL" })).unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");

    // Disconnect drops the browser's connection; Close closes it.
    s.execute("db.disconnect", &json!({ "dataSource": id })).unwrap();
    assert!(s.db.redis.get(&id).is_some_and(|b| b.version.is_none()));
    s.execute("db.redis.close", &json!({ "dataSource": id })).unwrap();
    assert!(s.db.redis.get(&id).is_none());
    s.shutdown();
}

// ---------------------------------------------------------------- ClickHouse

fn read_request(stream: &TcpStream) -> Option<(String, String)> {
    let mut r = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    let mut len = 0usize;
    loop {
        line.clear();
        r.read_line(&mut line).ok()?;
        let l = line.trim();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((target, String::from_utf8_lossy(&body).into_owned()))
}

fn clickhouse_fake() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let Some((_, sql)) = read_request(&stream) else { return };
                let rows = |names: &str, types: &str, rows: &[&str]| {
                    let mut s = format!("{names}\n{types}\n");
                    for r in rows {
                        s.push_str(r);
                        s.push('\n');
                    }
                    s
                };
                let body = if sql == "SELECT 1" {
                    rows("[\"1\"]", "[\"UInt8\"]", &["[1]"])
                } else if sql == "SELECT version()" {
                    rows("[\"v\"]", "[\"String\"]", &["[\"24.8.1\"]"])
                } else if sql.starts_with("SELECT currentDatabase()") {
                    rows("[\"d\"]", "[\"String\"]", &["[\"default\"]"])
                } else if sql.contains("system.databases") {
                    rows("[\"name\"]", "[\"String\"]", &["[\"default\"]", "[\"logs\"]"])
                } else if sql.contains("FROM system.tables WHERE database = 'default' AND NOT is_temporary") {
                    rows("[\"name\",\"engine\"]", "[\"String\",\"String\"]", &["[\"events\",\"MergeTree\"]"])
                } else if sql.starts_with("SELECT * FROM events") || sql.starts_with("SELECT * FROM `default`.`events`")
                {
                    rows("[\"id\",\"name\"]", "[\"UInt64\",\"String\"]", &["[\"1\",\"a\"]", "[\"2\",\"b\"]"])
                } else if sql.contains("system.columns") {
                    rows(
                        "[\"name\",\"type\",\"default_kind\",\"default_expression\",\"is_in_primary_key\"]",
                        "[\"String\",\"String\",\"String\",\"String\",\"UInt8\"]",
                        &["[\"id\",\"UInt64\",\"\",\"\",1]", "[\"name\",\"String\",\"\",\"\",0]"],
                    )
                } else if sql.contains("primary_key, sorting_key") {
                    rows("[\"p\",\"s\"]", "[\"String\",\"String\"]", &["[\"id\",\"id\"]"])
                } else if sql.contains("data_skipping_indices") {
                    rows("[\"n\",\"e\",\"t\"]", "[\"String\",\"String\",\"String\"]", &[])
                } else if sql.starts_with("INSERT") || sql.starts_with("KILL") {
                    String::new()
                } else {
                    let _ = stream.write_all(
                        format!("HTTP/1.1 400 X\r\nConnection: close\r\n\r\nCode: 62. DB::Exception: unexpected {sql}")
                            .as_bytes(),
                    );
                    return;
                };
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}").as_bytes());
            });
        }
    });
    port
}

#[test]
fn clickhouse_through_commands() {
    let port = clickhouse_fake();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "clickhouse", "name": "Events", "host": "127.0.0.1", "port": port.to_string(), "database": "default", "user": "default" }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    s.execute("db.testConnection", &json!({ "dataSource": id })).unwrap();
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "tree", |s| {
        s.db.source(&id).is_some_and(|d| d.tree.schemas.get("default").is_some_and(|n| n.tables.is_some()))
    });
    match &s.db.source(&id).unwrap().status {
        ConnStatus::Connected(v) => assert_eq!(v, "ClickHouse 24.8.1"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        s.db.source(&id).unwrap().tree.schemas.get("default").unwrap().tables.as_ref().unwrap()[0].info.name,
        "events"
    );
    let rows = rows_of(&mut s, &id, "SELECT * FROM events");
    assert_eq!(rows, vec![vec![Cell::UInt(1), Cell::Text("a".into())], vec![Cell::UInt(2), Cell::Text("b".into())]]);
    // No transactions: manual mode is refused, grid edits are not offered.
    let e = s.execute("db.tx.setMode", &json!({ "dataSource": id, "mode": "manual" }));
    assert!(e.is_err() || !s.db.manual_commit("x", &id), "{e:?}");
    assert!(!s.db.manual_commit("x", &id));
    s.execute("db.openTable", &json!({ "dataSource": id, "schema": "default", "table": "events" })).unwrap();
    pump_until(&mut s, "table", |s| !s.db.busy() && s.db.active().is_some_and(|r| !r.is_running()));
    let rid = s.db.active_result.unwrap();
    let e = s.db.editable(rid).unwrap_err();
    assert!(e.contains("no transactions"), "{e}");
    s.shutdown();
}

// ---------------------------------------------------------------- DuckDB

#[cfg(unix)]
#[test]
fn duckdb_through_commands() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let cli = root.join("fake-duckdb");
    std::fs::write(
        &cli,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo v1.2.0 x; exit 0; fi\ninput=$(cat)\ncase \"$input\" in\n\
         *\"SELECT 1 AS ok\"*) echo '{\"ok\":1}' ;;\n\
         *\"information_schema.schemata\"*) echo '{\"schema_name\":\"main\"}' ;;\n\
         *\"information_schema.tables\"*) echo '{\"table_name\":\"t\",\"table_type\":\"BASE TABLE\"}' ;;\n\
         *\"DESCRIBE SELECT 42\"*) echo '{\"column_name\":\"answer\",\"column_type\":\"INTEGER\"}'; echo __CODECRAFT_NEXT_RESULT__; echo '{\"answer\":42}' ;;\n\
         *) echo 'Error: unexpected' >&2; exit 1 ;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut s = session(&root);
    let r = s
        .execute(
            "db.addDataSource",
            &json!({ "kind": "duckdb", "name": "Lake", "path": "lake.duckdb", "options": { "duckdb_path": cli.display().to_string() } }),
        )
        .unwrap();
    let id = r["dataSource"].as_str().unwrap().to_string();
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "tree", |s| {
        s.db.source(&id).is_some_and(|d| d.tree.schemas.get("main").is_some_and(|n| n.tables.is_some()))
    });
    assert_eq!(rows_of(&mut s, &id, "SELECT 42 AS answer"), vec![vec![Cell::Int(42)]]);
    assert_eq!(s.db.active().unwrap().columns[0].type_name, "INTEGER");
    assert!(!s.db.manual_commit("x", &id));
    // SSH tunnels make no sense for a file.
    assert!(s.execute("db.editDataSource", &json!({ "dataSource": id, "sshTunnel": "bastion" })).is_ok());
    s.shutdown();
}

// ---------------------------------------------------------------- the dialog

#[test]
fn dialog_defaults_for_new_kinds() {
    for (kind, port, db, user) in [
        (DbKind::SqlServer, "1433", "master", "sa"),
        (DbKind::ClickHouse, "8123", "default", "default"),
        (DbKind::Redis, "6379", "0", ""),
        (DbKind::DuckDb, "", "", ""),
    ] {
        let f = DataSourceForm::for_kind(kind);
        assert_eq!((f.port.as_str(), f.database.as_str(), f.user.as_str()), (port, db, user), "{kind:?}");
        assert_eq!(f.host.is_empty(), kind.is_file_based());
        assert!(!kind.notes().is_empty());
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(&root);
    for kind in ["sqlserver", "mssql", "clickhouse", "redis", "duckdb"] {
        assert_eq!(s.execute("db.addDataSource", &json!({ "kind": kind })).unwrap()["dialog"], true, "{kind}");
        s.execute("db.closeDialog", &Value::Null).unwrap();
    }
    // A DuckDB data source needs its file; a Redis database must be a number.
    assert!(s.execute("db.addDataSource", &json!({ "kind": "duckdb", "name": "x" })).is_err());
    let r = s.execute(
        "db.addDataSource",
        &json!({ "kind": "redis", "name": "r", "host": "127.0.0.1", "port": "1", "database": "zero" }),
    );
    if let Ok(r) = r {
        let id = r["dataSource"].as_str().unwrap().to_string();
        s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
        pump_until(&mut s, "failure", |s| s.db.source(&id).is_some_and(|d| matches!(d.status, ConnStatus::Failed(_))));
    }
    s.shutdown();
}
