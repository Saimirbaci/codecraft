//! Redis: a scripted fake server on 127.0.0.1 (hostile replies, cancel, auth), and a real
//! `redis-server` the test starts itself on a free port in a temp dir (skipped when the binary is
//! missing). `CODECRAFT_TEST_REDIS_HOST`/`_PORT` point at an existing server instead; then only
//! keys under a throwaway `codecraft_test_<pid>:` prefix are written and deleted.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use codecraft_db::redis::{KeyValue, RedisClient, Reply};
use codecraft_db::{Cell, DataSourceConfig, DbError, DbKind, connect, query_all};

fn cfg(port: u16) -> DataSourceConfig {
    let mut c = DataSourceConfig {
        id: "r".into(),
        name: "r".into(),
        kind: DbKind::Redis,
        host: "127.0.0.1".into(),
        port: Some(port),
        ..Default::default()
    };
    c.options.insert("connect_timeout".into(), "5".into());
    c
}

/// Read one RESP command (array of bulk strings) from a client.
fn read_command(r: &mut BufReader<TcpStream>) -> Option<Vec<String>> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let n: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::new();
    for _ in 0..n {
        line.clear();
        r.read_line(&mut line).ok()?;
        let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
        let mut buf = vec![0u8; len + 2];
        r.read_exact(&mut buf).ok()?;
        buf.truncate(len);
        args.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Some(args)
}

/// A fake server: `reply(args)` gives the raw bytes to answer (None: never answer).
fn fake<F>(reply: F) -> (u16, mpsc::Receiver<Vec<String>>)
where
    F: Fn(&[String]) -> Option<Vec<u8>> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let reply = std::sync::Arc::new(reply);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (tx, reply) = (tx.clone(), reply.clone());
            std::thread::spawn(move || {
                let mut w = stream.try_clone().unwrap();
                let mut r = BufReader::new(stream);
                while let Some(args) = read_command(&mut r) {
                    let _ = tx.send(args.clone());
                    match reply(&args) {
                        Some(bytes) => {
                            if w.write_all(&bytes).is_err() {
                                break;
                            }
                        }
                        None => std::thread::sleep(Duration::from_secs(60)),
                    }
                }
            });
        }
    });
    (port, rx)
}

fn basic(args: &[String]) -> Option<Vec<u8>> {
    match args.first().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("PING") => Some(b"+PONG\r\n".to_vec()),
        Some("CLIENT") | Some("SELECT") => Some(b"+OK\r\n".to_vec()),
        Some("CONFIG") => Some(b"*2\r\n$9\r\ndatabases\r\n$1\r\n4\r\n".to_vec()),
        Some("INFO") => {
            let info = "# Server\r\nredis_version:7.2.4\r\nmode:x\r\n";
            Some(format!("${}\r\n{info}\r\n", info.len()).into_bytes())
        }
        _ => None,
    }
}

#[test]
fn fake_server_console_and_hostile_replies() {
    let (port, rx) = fake(|args| {
        if let Some(b) = basic(args) {
            return Some(b);
        }
        Some(match args.first().map(String::as_str) {
            Some("GET") if args.get(1).map(String::as_str) == Some("huge") => b"$999999999999\r\n".to_vec(),
            Some("GET") if args.get(1).map(String::as_str) == Some("garbage") => b"?what\r\n".to_vec(),
            Some("GET") => b"$5\r\nhello\r\n".to_vec(),
            Some("HGETALL") => b"*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$-1\r\n".to_vec(),
            Some("BAD") => b"-ERR unknown command 'BAD'\r\n".to_vec(),
            _ => b"+OK\r\n".to_vec(),
        })
    });
    let mut conn = connect(&cfg(port), None).unwrap();
    assert_eq!(conn.kind(), DbKind::Redis);
    assert_eq!(conn.server_version().unwrap(), "Redis 7.2.4");
    let info = conn.database_info().unwrap();
    assert_eq!(info.schemas, ["db0", "db1", "db2", "db3"]);
    let (cols, rows, _) = query_all(conn.as_mut(), "GET \"k with space\"").unwrap();
    assert_eq!(cols[0].name, "value");
    assert_eq!(rows, vec![vec![Cell::Text("hello".into())]]);
    // The server saw the quoted argument as one.
    let seen: Vec<Vec<String>> = rx.try_iter().collect();
    assert!(seen.iter().any(|a| a == &["GET".to_string(), "k with space".to_string()]), "{seen:?}");
    let (cols, rows, _) = query_all(conn.as_mut(), "HGETALL h").unwrap();
    assert_eq!((cols[0].name.as_str(), cols[1].name.as_str()), ("field", "value"));
    assert_eq!(rows[1], vec![Cell::Text("b".into()), Cell::Null]);
    match query_all(conn.as_mut(), "BAD x") {
        Err(DbError::Query(e)) => assert!(e.contains("unknown command"), "{e}"),
        other => panic!("{other:?}"),
    }
    assert!(query_all(conn.as_mut(), "SET k \"unbalanced").is_err());
    assert!(query_all(conn.as_mut(), "SUBSCRIBE ch").is_err());
    assert!(!conn.is_broken());
    // A lying length is refused without allocating it; the connection is then unusable.
    assert!(query_all(conn.as_mut(), "GET huge").is_err());
    assert!(conn.is_broken());
    let mut conn = connect(&cfg(port), None).unwrap();
    assert!(query_all(conn.as_mut(), "GET garbage").is_err());
    assert!(conn.is_broken());
}

#[test]
fn cancel_shuts_a_blocked_command_down() {
    let (port, _rx) = fake(|args| {
        basic(args).or_else(|| (args.first().map(String::as_str) != Some("BLPOP")).then(|| b"+OK\r\n".to_vec()))
    });
    let mut conn = connect(&cfg(port), None).unwrap();
    let canceller = conn.canceller();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel().unwrap();
    });
    let started = Instant::now();
    let r = query_all(conn.as_mut(), "BLPOP queue 0");
    t.join().unwrap();
    assert_eq!(r.err(), Some(DbError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(conn.is_broken());
}

#[test]
fn auth_failures_are_connect_errors() {
    let (port, rx) = fake(|args| match args.first().map(String::as_str) {
        Some("AUTH") => Some(b"-WRONGPASS invalid username-password pair\r\n".to_vec()),
        Some("PING") => Some(b"-NOAUTH Authentication required.\r\n".to_vec()),
        _ => basic(args),
    });
    match connect(&cfg(port), Some("pw")) {
        Err(DbError::Connect(e)) => assert!(e.contains("WRONGPASS"), "{e}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("connected"),
    }
    let mut c = cfg(port);
    c.user = "alice".into();
    let _ = connect(&c, Some("pw"));
    assert!(rx.try_iter().any(|a| a == ["AUTH", "alice", "pw"]));
    match connect(&cfg(port), None) {
        Err(DbError::Connect(e)) => assert!(e.contains("needs a password"), "{e}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("connected"),
    }
    // Nothing listening.
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    assert!(matches!(connect(&cfg(free), None), Err(DbError::Connect(_))));
    // Not a Redis server: the reply to PING is not RESP.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    assert!(connect(&cfg(port), None).is_err());
}

// ---------------------------------------------------------------- a real server

struct Server {
    child: Option<Child>,
    _dir: Option<tempfile::TempDir>,
    port: u16,
    prefix: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        } else if let Ok(mut c) = RedisClient::connect(&cfg_for(self), None, Some(Duration::from_secs(5))) {
            // An existing server: remove only our keys.
            let mut cursor = "0".to_string();
            while let Ok((next, keys)) = c.scan(&cursor, &format!("{}*", self.prefix), None, 500) {
                for k in keys {
                    let _ = c.call(&[b"DEL".to_vec(), k]);
                }
                if next == "0" {
                    break;
                }
                cursor = next;
            }
        }
    }
}

fn cfg_for(s: &Server) -> DataSourceConfig {
    let mut c = cfg(s.port);
    if s.child.is_none() {
        c.host = std::env::var("CODECRAFT_TEST_REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    }
    c
}

fn find_redis_server() -> Option<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].iter().map(std::path::PathBuf::from));
    dirs.into_iter().map(|d| d.join("redis-server")).find(|p| p.is_file())
}

fn server() -> Option<Server> {
    let prefix = format!("codecraft_test_{}:", std::process::id());
    if let Ok(port) = std::env::var("CODECRAFT_TEST_REDIS_PORT") {
        let port: u16 = port.parse().ok()?;
        let s = Server { child: None, _dir: None, port, prefix };
        let pw = std::env::var("CODECRAFT_TEST_REDIS_PASSWORD").ok();
        if let Err(e) = connect(&cfg_for(&s), pw.as_deref()) {
            eprintln!("skipping: no Redis at CODECRAFT_TEST_REDIS_PORT ({e})");
            return None;
        }
        return Some(s);
    }
    let Some(bin) = find_redis_server() else {
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
    let s = Server { child: Some(child), _dir: Some(dir), port, prefix };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if RedisClient::connect(&cfg(port), None, Some(Duration::from_secs(5))).is_ok() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("skipping: redis-server did not start");
    None
}

#[test]
fn real_server_console_browser_and_values() {
    let Some(srv) = server() else { return };
    let p = srv.prefix.clone();
    let pw = std::env::var("CODECRAFT_TEST_REDIS_PASSWORD").ok();
    let mut conn = connect(&cfg_for(&srv), pw.as_deref()).unwrap();
    assert!(conn.server_version().unwrap().contains(' '));
    conn.ping().unwrap();
    for cmd in [
        format!("SET {p}user:1 \"Ada Lovelace\""),
        format!("SET {p}user:2 x EX 1000"),
        format!("HSET {p}h name ada age 36"),
        format!("RPUSH {p}l a b c"),
        format!("SADD {p}s x y"),
        format!("ZADD {p}z 1.5 one 2 two"),
        format!("XADD {p}x 1-1 f v g w"),
    ] {
        query_all(conn.as_mut(), &cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"));
    }
    let (_, rows, _) = query_all(conn.as_mut(), &format!("GET {p}user:1")).unwrap();
    assert_eq!(rows, vec![vec![Cell::Text("Ada Lovelace".into())]]);
    let (cols, rows, _) = query_all(conn.as_mut(), &format!("ZRANGE {p}z 0 -1 WITHSCORES")).unwrap();
    assert_eq!(cols[1].name, "score");
    assert_eq!(rows[0], vec![Cell::Text("one".into()), Cell::Text("1.5".into())]);
    let (cols, rows, _) = query_all(conn.as_mut(), &format!("XRANGE {p}x - +")).unwrap();
    assert_eq!(cols.len(), 3);
    assert_eq!(rows[0][0], Cell::Text("1-1".into()));
    let (cols, _, _) = query_all(conn.as_mut(), "INFO server").unwrap();
    assert_eq!(cols.len(), 3);
    assert!(conn.tables("db0").unwrap().is_empty());

    // The browser API.
    let mut c = RedisClient::connect(&cfg_for(&srv), pw.as_deref(), Some(Duration::from_secs(30))).unwrap();
    let mut keys = Vec::new();
    let mut cursor = "0".to_string();
    loop {
        let (next, mut k) = c.scan(&cursor, &format!("{p}*"), None, 100).unwrap();
        keys.append(&mut k);
        if next == "0" {
            break;
        }
        cursor = next;
    }
    assert_eq!(keys.len(), 7, "{keys:?}");
    let (_, hashes) = {
        let mut found = Vec::new();
        let mut cursor = "0".to_string();
        loop {
            let (next, mut k) = c.scan(&cursor, &format!("{p}*"), Some("hash"), 100).unwrap();
            found.append(&mut k);
            if next == "0" {
                break (next, found);
            }
            cursor = next;
        }
    };
    assert_eq!(hashes, vec![format!("{p}h").into_bytes()]);
    let tt = c.types_and_ttls(&[format!("{p}user:2").into_bytes(), format!("{p}gone").into_bytes()]).unwrap();
    assert_eq!(tt[0].0, "string");
    assert!(tt[0].1 > 0 && tt[0].1 <= 1_000_000);
    assert_eq!(tt[1], ("none".to_string(), -2));
    let info = c.key_info(format!("{p}l").as_bytes()).unwrap();
    assert_eq!((info.kind.as_str(), info.length), ("list", Some(3)));
    let page = c.load_value(format!("{p}l").as_bytes(), "list", "0", 2).unwrap();
    assert_eq!(page.value, KeyValue::List(vec![b"a".to_vec(), b"b".to_vec()]));
    assert_eq!(page.next.as_deref(), Some("2"));
    let page = c.load_value(format!("{p}h").as_bytes(), "hash", "", 100).unwrap();
    match page.value {
        KeyValue::Hash(mut pairs) => {
            pairs.sort();
            assert_eq!(pairs, vec![(b"age".to_vec(), b"36".to_vec()), (b"name".to_vec(), b"ada".to_vec())]);
        }
        other => panic!("{other:?}"),
    }
    let page = c.load_value(format!("{p}z").as_bytes(), "zset", "0", 10).unwrap();
    assert_eq!(page.value, KeyValue::ZSet(vec![(b"one".to_vec(), "1.5".into()), (b"two".to_vec(), "2".into())]));
    let page = c.load_value(format!("{p}x").as_bytes(), "stream", "", 10).unwrap();
    match page.value {
        KeyValue::Stream(e) => {
            assert_eq!(e[0].fields, vec![(b"f".to_vec(), b"v".to_vec()), (b"g".to_vec(), b"w".to_vec())])
        }
        other => panic!("{other:?}"),
    }
    let page = c.load_value(format!("{p}user:1").as_bytes(), "string", "", 10).unwrap();
    assert_eq!(page.value, KeyValue::String(b"Ada Lovelace".to_vec()));
    assert!(c.load_value(format!("{p}gone").as_bytes(), "none", "", 10).is_err());
    assert!(matches!(c.ok(&[b"PING".as_slice()]).unwrap(), Reply::Status(_)));
    // Database selection.
    if srv.child.is_some() {
        c.select(3).unwrap();
        assert_eq!(c.db(), 3);
        c.ok(&[b"SET".as_slice(), b"in3", b"1"]).unwrap();
        let mut c3 = cfg_for(&srv);
        c3.database = "3".into();
        let mut conn3 = connect(&c3, None).unwrap();
        let (_, rows, _) = query_all(conn3.as_mut(), "GET in3").unwrap();
        assert_eq!(rows, vec![vec![Cell::Text("1".into())]]);
        let (_, rows, _) = query_all(conn.as_mut(), "GET in3").unwrap();
        assert_eq!(rows, vec![vec![Cell::Null]]);
        assert_eq!(c.keyspace().unwrap().iter().find(|(d, _)| *d == 3).map(|(_, n)| *n), Some(1));
    }
}
