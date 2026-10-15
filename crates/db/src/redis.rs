//! Redis over RESP with a small client of our own (no dependency): command line tokenizing like
//! `redis-cli`, a reply parser that caps every length it reads from the network, a console
//! [`Connection`] that turns replies into grid rows, and a typed API for the key browser (SCAN
//! pages, key types and TTLs, values per type, edits).
//!
//! Why not the `redis` crate: the console and the browser need a handful of commands, explicit
//! limits on hostile replies, and cancel by shutting the socket down; the crate brings its own
//! connection management, URL parsing and ~10 transitive crates for little gain. TLS (`tls=true`)
//! goes through the platform TLS stack (`native-tls`, already used for PostgreSQL) on macOS and
//! Windows.
//!
//! Redis has no SQL transactions: `MULTI`/`EXEC` queue commands but cannot roll back, so the IDE
//! treats Redis data sources as having no transactions (auto-commit only).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::{
    Canceller, Cell, Column, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult, ExecSummary, Pager,
    ResultSink, TableDetails, TableInfo,
};

/// Longest status/integer line accepted.
const MAX_LINE: usize = 64 * 1024;
/// Largest bulk string accepted (Redis' own limit is 512 MB).
pub const MAX_BULK: usize = 64 * 1024 * 1024;
/// Bytes of bulk data in one reply, all levels together.
pub const MAX_REPLY_BYTES: usize = 256 * 1024 * 1024;
/// Elements in one reply, all levels together.
pub const MAX_REPLY_ELEMENTS: usize = 5_000_000;
/// Nesting depth of arrays/maps.
const MAX_DEPTH: usize = 32;
/// Databases offered when `CONFIG GET databases` is not allowed.
const DEFAULT_DATABASES: u32 = 16;

/// One RESP reply (RESP2, plus the RESP3 types a proxy may send).
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Nil,
    Status(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Array(Vec<Reply>),
    Double(f64),
    Bool(bool),
    /// RESP3 maps (and attributes are dropped).
    Map(Vec<(Reply, Reply)>),
    /// RESP3 big numbers and verbatim strings, as text.
    Text(String),
}

impl Reply {
    /// The reply as text (bulk strings decoded lossily); `None` for nil and aggregates.
    pub fn as_text(&self) -> Option<String> {
        match self {
            Reply::Status(s) | Reply::Text(s) => Some(s.clone()),
            Reply::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Reply::Int(i) => Some(i.to_string()),
            Reply::Double(f) => Some(f.to_string()),
            Reply::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
            Reply::Error(e) => Some(e.clone()),
            Reply::Nil | Reply::Array(_) | Reply::Map(_) => None,
        }
    }

    pub fn as_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Reply::Bulk(b) => Some(b.clone()),
            other => other.as_text().map(String::into_bytes),
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Reply::Int(i) => Some(*i),
            Reply::Bulk(_) | Reply::Status(_) | Reply::Text(_) => self.as_text()?.trim().parse().ok(),
            _ => None,
        }
    }

    /// Elements of an array (or the flattened pairs of a map).
    pub fn items(&self) -> Vec<Reply> {
        match self {
            Reply::Array(v) => v.clone(),
            Reply::Map(m) => m.iter().flat_map(|(k, v)| [k.clone(), v.clone()]).collect(),
            _ => Vec::new(),
        }
    }

    fn is_scalar(&self) -> bool {
        !matches!(self, Reply::Array(_) | Reply::Map(_))
    }

    /// The reply as one grid cell: scalars as values, aggregates as JSON-like text.
    pub fn to_cell(&self) -> Cell {
        match self {
            Reply::Nil => Cell::Null,
            Reply::Status(s) | Reply::Text(s) => Cell::Text(s.clone()),
            Reply::Error(e) => Cell::Text(format!("(error) {e}")),
            Reply::Int(i) => Cell::Int(*i),
            Reply::Double(f) => Cell::Float(*f),
            Reply::Bool(b) => Cell::Bool(*b),
            Reply::Bulk(b) => match std::str::from_utf8(b) {
                Ok(s) => Cell::Text(s.to_string()),
                Err(_) => Cell::Bytes(b.clone()),
            },
            Reply::Array(_) | Reply::Map(_) => Cell::Other(self.to_json().to_string()),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Reply::Nil => Value::Null,
            Reply::Int(i) => Value::from(*i),
            Reply::Double(f) => serde_json::Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
            Reply::Bool(b) => Value::Bool(*b),
            Reply::Array(v) => Value::Array(v.iter().map(Reply::to_json).collect()),
            Reply::Map(m) => {
                let mut o = serde_json::Map::new();
                for (k, v) in m {
                    o.insert(k.as_text().unwrap_or_default(), v.to_json());
                }
                Value::Object(o)
            }
            other => Value::String(other.as_text().unwrap_or_default()),
        }
    }
}

// ---------------------------------------------------------------- RESP parsing

/// Reads replies with the limits above; never panics on hostile input.
struct Budget {
    bytes: usize,
    elements: usize,
}

fn proto(msg: impl Into<String>) -> DbError {
    DbError::Query(format!("Redis protocol error: {}", msg.into()))
}

fn io_err(e: std::io::Error) -> DbError {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            DbError::Query("Redis connection: the server did not answer in time".into())
        }
        _ => DbError::Query(format!("Redis connection: {e}")),
    }
}

/// A CRLF-terminated line without the CRLF.
fn read_line<R: BufRead>(r: &mut R) -> DbResult<Vec<u8>> {
    let mut line = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64 + 2).read_until(b'\n', &mut line).map_err(io_err)?;
    if n == 0 {
        return Err(DbError::Query("Redis closed the connection".into()));
    }
    if line.last() != Some(&b'\n') {
        return Err(proto(if line.len() > MAX_LINE { "line too long" } else { "truncated reply" }));
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

fn parse_len(line: &[u8]) -> DbResult<i64> {
    std::str::from_utf8(line).ok().and_then(|s| s.trim().parse::<i64>().ok()).ok_or_else(|| {
        proto(format!("bad length `{}`", String::from_utf8_lossy(line).chars().take(40).collect::<String>()))
    })
}

fn read_bulk<R: BufRead>(r: &mut R, len: i64, budget: &mut Budget) -> DbResult<Option<Vec<u8>>> {
    if len < 0 {
        return Ok(None);
    }
    let len = usize::try_from(len).map_err(|_| proto("bad bulk length"))?;
    if len > MAX_BULK {
        return Err(proto(format!("a value of {len} bytes is larger than the {MAX_BULK} bytes shown")));
    }
    budget.bytes = budget.bytes.checked_sub(len).ok_or_else(|| proto("the reply is too large to show"))?;
    let mut buf = Vec::with_capacity(len.min(1 << 20));
    r.by_ref().take(len as u64).read_to_end(&mut buf).map_err(io_err)?;
    if buf.len() != len {
        return Err(proto("truncated bulk string"));
    }
    let mut crlf = [0u8; 2];
    r.read_exact(&mut crlf).map_err(io_err)?;
    if &crlf != b"\r\n" {
        return Err(proto("bulk string without CRLF"));
    }
    Ok(Some(buf))
}

fn read_reply_at<R: BufRead>(r: &mut R, depth: usize, budget: &mut Budget) -> DbResult<Reply> {
    if depth > MAX_DEPTH {
        return Err(proto("replies nested too deeply"));
    }
    budget.elements = budget.elements.checked_sub(1).ok_or_else(|| proto("the reply has too many elements"))?;
    let line = read_line(r)?;
    let Some((&kind, rest)) = line.split_first() else { return Err(proto("empty line")) };
    let text = || String::from_utf8_lossy(rest).into_owned();
    Ok(match kind {
        b'+' => Reply::Status(text()),
        b'-' => Reply::Error(text()),
        b':' => Reply::Int(parse_len(rest)?),
        b'$' => match read_bulk(r, parse_len(rest)?, budget)? {
            Some(b) => Reply::Bulk(b),
            None => Reply::Nil,
        },
        b'=' => match read_bulk(r, parse_len(rest)?, budget)? {
            // `txt:` / `mkd:` prefix.
            Some(b) => Reply::Text(String::from_utf8_lossy(b.get(4..).unwrap_or_default()).into_owned()),
            None => Reply::Nil,
        },
        b'!' => match read_bulk(r, parse_len(rest)?, budget)? {
            Some(b) => Reply::Error(String::from_utf8_lossy(&b).into_owned()),
            None => Reply::Nil,
        },
        b'*' | b'~' | b'>' => {
            let n = parse_len(rest)?;
            if n < 0 {
                return Ok(Reply::Nil);
            }
            let n = usize::try_from(n).map_err(|_| proto("bad array length"))?;
            if n > budget.elements {
                return Err(proto("the reply has too many elements"));
            }
            let mut v = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                v.push(read_reply_at(r, depth + 1, budget)?);
            }
            Reply::Array(v)
        }
        b'%' | b'|' => {
            let n = parse_len(rest)?;
            let n = usize::try_from(n).map_err(|_| proto("bad map length"))?;
            if n.saturating_mul(2) > budget.elements {
                return Err(proto("the reply has too many elements"));
            }
            let mut m = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                let k = read_reply_at(r, depth + 1, budget)?;
                let v = read_reply_at(r, depth + 1, budget)?;
                m.push((k, v));
            }
            if kind == b'|' {
                // An attribute precedes the real reply.
                return read_reply_at(r, depth, budget);
            }
            Reply::Map(m)
        }
        b'_' => Reply::Nil,
        b'#' => Reply::Bool(rest == b"t"),
        b',' => {
            let s = text();
            let f = match s.trim() {
                "inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                t => t
                    .parse::<f64>()
                    .map_err(|_| proto(format!("bad double `{}`", t.chars().take(40).collect::<String>())))?,
            };
            Reply::Double(f)
        }
        b'(' => Reply::Text(text()),
        other => {
            return Err(proto(format!("unexpected reply type `{}`", char::from(other).escape_default())));
        }
    })
}

/// Read one reply.
pub fn read_reply<R: BufRead>(r: &mut R) -> DbResult<Reply> {
    let mut budget = Budget { bytes: MAX_REPLY_BYTES, elements: MAX_REPLY_ELEMENTS };
    read_reply_at(r, 0, &mut budget)
}

/// Parse one reply from bytes (tests, fuzzing).
pub fn parse_reply(bytes: &[u8]) -> DbResult<Reply> {
    read_reply(&mut std::io::Cursor::new(bytes))
}

/// A command in RESP (an array of bulk strings).
pub fn encode_command<A: AsRef<[u8]>>(args: &[A]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        let a = a.as_ref();
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

// ---------------------------------------------------------------- command lines

fn hex_val(c: char) -> Option<u8> {
    c.to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// Split a command line like `redis-cli`: whitespace separates arguments; `"…"` understands
/// `\n \r \t \b \a \\ \" \xHH`; `'…'` understands `\'`. A closing quote must end the argument.
pub fn split_args(line: &str) -> Result<Vec<Vec<u8>>, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let at = |k: usize| chars.get(k).copied();
    loop {
        while at(i).is_some_and(char::is_whitespace) {
            i += 1;
        }
        let Some(first) = at(i) else { break };
        let mut arg: Vec<u8> = Vec::new();
        let push = |arg: &mut Vec<u8>, c: char| {
            let mut buf = [0u8; 4];
            arg.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        };
        if first == '"' || first == '\'' {
            let quote = first;
            i += 1;
            loop {
                let Some(c) = at(i) else { return Err("unbalanced quotes".into()) };
                if c == quote {
                    i += 1;
                    break;
                }
                if c == '\\' && quote == '"' {
                    match at(i + 1) {
                        Some('x') if at(i + 2).and_then(hex_val).is_some() && at(i + 3).and_then(hex_val).is_some() => {
                            let hi = at(i + 2).and_then(hex_val).unwrap_or(0);
                            let lo = at(i + 3).and_then(hex_val).unwrap_or(0);
                            arg.push(hi * 16 + lo);
                            i += 4;
                            continue;
                        }
                        Some(e) => {
                            let c = match e {
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                'b' => '\u{8}',
                                'a' => '\u{7}',
                                other => other,
                            };
                            push(&mut arg, c);
                            i += 2;
                            continue;
                        }
                        None => return Err("unbalanced quotes".into()),
                    }
                }
                if c == '\\' && quote == '\'' && at(i + 1) == Some('\'') {
                    arg.push(b'\'');
                    i += 2;
                    continue;
                }
                push(&mut arg, c);
                i += 1;
            }
            if at(i).is_some_and(|c| !c.is_whitespace()) {
                return Err("closing quote must be followed by a space".into());
            }
        } else {
            while let Some(c) = at(i) {
                if c.is_whitespace() {
                    break;
                }
                push(&mut arg, c);
                i += 1;
            }
        }
        out.push(arg);
    }
    Ok(out)
}

/// Quote an argument so [`split_args`] reads it back (for generated command text).
pub fn quote_arg(arg: &[u8]) -> String {
    let plain = !arg.is_empty() && arg.iter().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'));
    if plain {
        return String::from_utf8_lossy(arg).into_owned();
    }
    let mut s = String::from("\"");
    match std::str::from_utf8(arg) {
        Ok(text) => {
            for c in text.chars() {
                match c {
                    '"' => s.push_str("\\\""),
                    '\\' => s.push_str("\\\\"),
                    '\n' => s.push_str("\\n"),
                    '\r' => s.push_str("\\r"),
                    '\t' => s.push_str("\\t"),
                    c if c.is_control() => {
                        let mut buf = [0u8; 4];
                        for b in c.encode_utf8(&mut buf).bytes() {
                            s.push_str(&format!("\\x{b:02x}"));
                        }
                    }
                    c => s.push(c),
                }
            }
        }
        Err(_) => {
            for b in arg {
                match b {
                    b'"' => s.push_str("\\\""),
                    b'\\' => s.push_str("\\\\"),
                    b if b.is_ascii_graphic() || *b == b' ' => s.push(char::from(*b)),
                    b => s.push_str(&format!("\\x{b:02x}")),
                }
            }
        }
    }
    s.push('"');
    s
}

/// A key as shown: the text when it is printable UTF-8, else `redis-cli` quoting.
pub fn display_key(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(s) if !s.chars().any(char::is_control) => s.to_string(),
        _ => quote_arg(key),
    }
}

/// Commands that only read (the production guard and read-only data sources let them run).
const READ_COMMANDS: &[&str] = &[
    "BITCOUNT",
    "BITPOS",
    "CLIENT",
    "COMMAND",
    "CONFIG",
    "DBSIZE",
    "DUMP",
    "ECHO",
    "EXISTS",
    "EXPIRETIME",
    "GEODIST",
    "GEOHASH",
    "GEOPOS",
    "GEORADIUSBYMEMBER_RO",
    "GEORADIUS_RO",
    "GEOSEARCH",
    "GET",
    "GETBIT",
    "GETRANGE",
    "HELLO",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HRANDFIELD",
    "HSCAN",
    "HSTRLEN",
    "HVALS",
    "INFO",
    "JSON.ARRINDEX",
    "JSON.ARRLEN",
    "JSON.DEBUG",
    "JSON.GET",
    "JSON.MGET",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
    "JSON.RESP",
    "JSON.STRLEN",
    "JSON.TYPE",
    "KEYS",
    "LASTSAVE",
    "LCS",
    "LINDEX",
    "LLEN",
    "LOLWUT",
    "LPOS",
    "LRANGE",
    "MEMORY",
    "MGET",
    "OBJECT",
    "PEXPIRETIME",
    "PFCOUNT",
    "PING",
    "PTTL",
    "PUBSUB",
    "RANDOMKEY",
    "READONLY",
    "ROLE",
    "SCAN",
    "SCARD",
    "SDIFF",
    "SELECT",
    "SINTER",
    "SINTERCARD",
    "SISMEMBER",
    "SLOWLOG",
    "SMEMBERS",
    "SMISMEMBER",
    "SORT_RO",
    "SRANDMEMBER",
    "SSCAN",
    "STRLEN",
    "SUBSTR",
    "SUNION",
    "TIME",
    "TOUCH",
    "TTL",
    "TYPE",
    "XINFO",
    "XLEN",
    "XPENDING",
    "XRANGE",
    "XREAD",
    "XREVRANGE",
    "ZCARD",
    "ZCOUNT",
    "ZDIFF",
    "ZINTER",
    "ZINTERCARD",
    "ZLEXCOUNT",
    "ZMSCORE",
    "ZRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYLEX",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREVRANGE",
    "ZREVRANGEBYLEX",
    "ZREVRANGEBYSCORE",
    "ZREVRANK",
    "ZSCAN",
    "ZSCORE",
    "ZUNION",
];

/// Whether a Redis command line may change data (anything not known to only read counts).
/// `CONFIG SET`, `CLIENT KILL`, `MEMORY PURGE` and the like are writes even though their
/// first word is listed as reading.
/// Common write commands (with [`READ_COMMANDS`], what a Redis console completes).
const WRITE_COMMANDS: &[&str] = &[
    "APPEND",
    "COPY",
    "DECR",
    "DECRBY",
    "DEL",
    "EXPIRE",
    "EXPIREAT",
    "FLUSHALL",
    "FLUSHDB",
    "GETDEL",
    "GETEX",
    "GETSET",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HMSET",
    "HSET",
    "HSETNX",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "JSON.SET",
    "JSON.DEL",
    "LINSERT",
    "LMOVE",
    "LPOP",
    "LPUSH",
    "LPUSHX",
    "LREM",
    "LSET",
    "LTRIM",
    "MOVE",
    "MSET",
    "MSETNX",
    "PERSIST",
    "PEXPIRE",
    "PEXPIREAT",
    "PSETEX",
    "PUBLISH",
    "RENAME",
    "RENAMENX",
    "RESTORE",
    "RPOP",
    "RPOPLPUSH",
    "RPUSH",
    "RPUSHX",
    "SADD",
    "SDIFFSTORE",
    "SET",
    "SETBIT",
    "SETEX",
    "SETNX",
    "SETRANGE",
    "SINTERSTORE",
    "SMOVE",
    "SPOP",
    "SREM",
    "SUNIONSTORE",
    "SWAPDB",
    "UNLINK",
    "XACK",
    "XADD",
    "XDEL",
    "XTRIM",
    "ZADD",
    "ZINCRBY",
    "ZPOPMAX",
    "ZPOPMIN",
    "ZREM",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
];

/// Redis command names a console completes, sorted and without duplicates.
pub fn command_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = READ_COMMANDS.iter().chain(WRITE_COMMANDS).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

pub fn is_write_command(line: &str) -> bool {
    let Ok(args) = split_args(line.trim()) else { return true };
    let Some(first) = args.first() else { return false };
    let cmd = String::from_utf8_lossy(first).to_ascii_uppercase();
    let sub = args.get(1).map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).unwrap_or_default();
    match cmd.as_str() {
        "CONFIG" => sub != "GET",
        "CLIENT" => !matches!(sub.as_str(), "LIST" | "INFO" | "GETNAME" | "ID" | "SETNAME" | "NO-EVICT" | "NO-TOUCH"),
        "MEMORY" => !matches!(sub.as_str(), "USAGE" | "STATS" | "DOCTOR" | "MALLOC-STATS" | "HELP"),
        "SLOWLOG" => sub == "RESET",
        "OBJECT" | "XINFO" | "PUBSUB" | "COMMAND" => false,
        "SORT_RO" => false,
        c => !READ_COMMANDS.contains(&c),
    }
}

/// Split a Redis console into commands: one per line (a newline inside quotes continues the
/// command); lines starting with `#` or `//` are comments. Offsets are char offsets.
pub fn split_commands(text: &str) -> Vec<crate::Statement> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    let n = chars.len();
    let flush = |from: usize, to: usize, out: &mut Vec<crate::Statement>| {
        let mut s = from;
        let mut e = to.min(n);
        while s < e && chars.get(s).is_some_and(|c| c.is_whitespace()) {
            s += 1;
        }
        while e > s && chars.get(e - 1).is_some_and(|c| c.is_whitespace()) {
            e -= 1;
        }
        let body: String = chars.get(s..e).unwrap_or(&[]).iter().collect();
        if body.is_empty() || body.starts_with('#') || body.starts_with("//") {
            return;
        }
        out.push(crate::Statement { start: s, end: e, end_with_delimiter: e, text: body });
    };
    while i < n {
        let c = chars.get(i).copied().unwrap_or(' ');
        match quote {
            Some(q) => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\n' {
                    flush(start, i, &mut out);
                    start = i + 1;
                } else if (c == '"' || c == '\'')
                    && i.checked_sub(1).and_then(|p| chars.get(p)).is_none_or(|p| p.is_whitespace())
                {
                    quote = Some(c);
                }
            }
        }
        i += 1;
    }
    flush(start, n, &mut out);
    out
}

// ---------------------------------------------------------------- connection

enum Stream {
    Plain(TcpStream),
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Shuts the socket down from another thread (cancel).
struct RedisCancel {
    socket: Mutex<Option<TcpStream>>,
    cancelled: Arc<AtomicBool>,
}

impl Canceller for RedisCancel {
    fn cancel(&self) -> DbResult<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(s) = self.socket.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        Ok(())
    }
}

/// A synchronous Redis client.
pub struct RedisClient {
    io: BufReader<Stream>,
    cancel: Arc<RedisCancel>,
    db: u32,
    broken: bool,
}

/// The database index of a data source (`database` field, empty is 0).
pub fn database_index(cfg: &DataSourceConfig) -> DbResult<u32> {
    let d = cfg.database.trim().trim_start_matches("db");
    if d.is_empty() {
        return Ok(0);
    }
    d.parse::<u32>()
        .ok()
        .filter(|n| *n < 10_000)
        .ok_or_else(|| DbError::Config(format!("the Redis database must be a number (0–15), not `{}`", cfg.database)))
}

fn option_on(cfg: &DataSourceConfig, key: &str) -> bool {
    cfg.options.get(key).is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

impl RedisClient {
    /// Connect, authenticate and select the database. `read_timeout` bounds every reply (`None`
    /// for consoles, where blocking commands may wait).
    pub fn connect(cfg: &DataSourceConfig, password: Option<&str>, read_timeout: Option<Duration>) -> DbResult<Self> {
        let host = cfg.host.trim();
        let port = cfg.port_or_default();
        let addrs: Vec<_> = (host, port)
            .to_socket_addrs()
            .map_err(|e| DbError::Connect(format!("cannot resolve {host}: {e}")))?
            .collect();
        let timeout = cfg.connect_timeout();
        let mut last = None;
        let mut socket = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, timeout) {
                Ok(s) => {
                    socket = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let socket = socket.ok_or_else(|| {
            DbError::Connect(format!(
                "cannot connect to Redis at {host}:{port}: {}",
                last.map(|e| e.to_string()).unwrap_or_else(|| "no address".into())
            ))
        })?;
        let _ = socket.set_nodelay(true);
        // The handshake always has a deadline.
        socket.set_read_timeout(Some(timeout)).map_err(|e| DbError::Connect(e.to_string()))?;
        socket.set_write_timeout(Some(Duration::from_secs(30))).map_err(|e| DbError::Connect(e.to_string()))?;
        let handle = socket.try_clone().map_err(|e| DbError::Connect(e.to_string()))?;
        let stream = if option_on(cfg, "tls") || option_on(cfg, "ssl") {
            tls_stream(cfg, socket)?
        } else {
            Stream::Plain(socket)
        };
        let cancel =
            Arc::new(RedisCancel { socket: Mutex::new(Some(handle)), cancelled: Arc::new(AtomicBool::new(false)) });
        let mut client = RedisClient { io: BufReader::with_capacity(64 * 1024, stream), cancel, db: 0, broken: false };
        let fail = |e: DbError| DbError::Connect(e.to_string().replace("Redis connection: ", ""));
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            let user = cfg.user.trim();
            let r = if user.is_empty() {
                client.call(&[b"AUTH".as_slice(), p.as_bytes()])
            } else {
                client.call(&[b"AUTH".as_slice(), user.as_bytes(), p.as_bytes()])
            }
            .map_err(fail)?;
            if let Reply::Error(e) = r {
                return Err(DbError::Connect(format!("authentication failed: {e}")));
            }
        }
        // PING proves the server speaks RESP (and that no password is needed).
        match client.call(&[b"PING".as_slice()]).map_err(fail)? {
            Reply::Error(e) if e.starts_with("NOAUTH") => {
                return Err(DbError::Connect("the Redis server needs a password".into()));
            }
            Reply::Error(e) => return Err(DbError::Connect(e)),
            _ => {}
        }
        let db = database_index(cfg)?;
        if db != 0 {
            client.select(db).map_err(|e| DbError::Connect(e.to_string()))?;
        }
        let _ = client.call(&[b"CLIENT".as_slice(), b"SETNAME", b"codecraft"]);
        if let Some(stream) = client.cancel.socket.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            let _ = stream.set_read_timeout(read_timeout);
        }
        Ok(client)
    }

    /// The selected database.
    pub fn db(&self) -> u32 {
        self.db
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// A handle that shuts the connection down (the running command fails as cancelled and the
    /// connection must be reopened).
    pub fn canceller(&self) -> Arc<dyn Canceller> {
        self.cancel.clone()
    }

    fn lost(&mut self, e: DbError) -> DbError {
        self.broken = true;
        if self.cancel.cancelled.swap(false, Ordering::SeqCst) { DbError::Cancelled } else { e }
    }

    /// Send a command and read its reply (an error reply is a value, not an `Err`).
    pub fn call<A: AsRef<[u8]>>(&mut self, args: &[A]) -> DbResult<Reply> {
        if self.broken {
            return Err(DbError::Query("the Redis connection was closed; reconnect".into()));
        }
        let cmd = encode_command(args);
        if let Err(e) = self.io.get_mut().write_all(&cmd).and_then(|()| self.io.get_mut().flush()) {
            return Err(self.lost(io_err(e)));
        }
        match read_reply(&mut self.io) {
            Ok(r) => Ok(r),
            Err(e) => Err(self.lost(e)),
        }
    }

    /// Send several commands at once and read every reply (pipelining).
    pub fn pipeline(&mut self, cmds: &[Vec<Vec<u8>>]) -> DbResult<Vec<Reply>> {
        if self.broken {
            return Err(DbError::Query("the Redis connection was closed; reconnect".into()));
        }
        let mut buf = Vec::new();
        for c in cmds {
            buf.extend_from_slice(&encode_command(c));
        }
        if let Err(e) = self.io.get_mut().write_all(&buf).and_then(|()| self.io.get_mut().flush()) {
            return Err(self.lost(io_err(e)));
        }
        let mut out = Vec::with_capacity(cmds.len());
        for _ in cmds {
            match read_reply(&mut self.io) {
                Ok(r) => out.push(r),
                Err(e) => return Err(self.lost(e)),
            }
        }
        Ok(out)
    }

    /// A command whose error reply is an `Err`.
    pub fn ok<A: AsRef<[u8]>>(&mut self, args: &[A]) -> DbResult<Reply> {
        match self.call(args)? {
            Reply::Error(e) => Err(DbError::Query(e)),
            r => Ok(r),
        }
    }

    pub fn select(&mut self, db: u32) -> DbResult<()> {
        self.ok(&[b"SELECT".to_vec(), db.to_string().into_bytes()])?;
        self.db = db;
        Ok(())
    }

    /// `INFO server` → `Redis 7.2.4` (or Valkey/KeyDB/Dragonfly when they say so).
    pub fn server_version(&mut self) -> DbResult<String> {
        let info = self.ok(&[b"INFO".as_slice(), b"server"])?.as_text().unwrap_or_default();
        Ok(server_label(&info))
    }

    /// How many databases the server has (`CONFIG GET databases`; 16 when that is not allowed).
    pub fn database_count(&mut self) -> u32 {
        match self.call(&[b"CONFIG".as_slice(), b"GET", b"databases"]) {
            Ok(r) => r
                .items()
                .get(1)
                .and_then(Reply::as_int)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=256).contains(n))
                .unwrap_or(DEFAULT_DATABASES),
            Err(_) => DEFAULT_DATABASES,
        }
    }

    /// Keys per database from `INFO keyspace` (`db0:keys=3,expires=0,…`).
    pub fn keyspace(&mut self) -> DbResult<Vec<(u32, u64)>> {
        let info = self.ok(&[b"INFO".as_slice(), b"keyspace"])?.as_text().unwrap_or_default();
        Ok(parse_keyspace(&info))
    }

    /// One `SCAN` step: the next cursor (`"0"` at the end) and the keys. `kind` filters by type
    /// on the server (Redis 6+).
    pub fn scan(
        &mut self,
        cursor: &str,
        pattern: &str,
        kind: Option<&str>,
        count: u32,
    ) -> DbResult<(String, Vec<Vec<u8>>)> {
        let mut args: Vec<Vec<u8>> = vec![b"SCAN".to_vec(), cursor.as_bytes().to_vec()];
        if !pattern.is_empty() && pattern != "*" {
            args.push(b"MATCH".to_vec());
            args.push(pattern.as_bytes().to_vec());
        }
        args.push(b"COUNT".to_vec());
        args.push(count.max(1).to_string().into_bytes());
        if let Some(k) = kind {
            args.push(b"TYPE".to_vec());
            args.push(k.as_bytes().to_vec());
        }
        let r = self.ok(&args)?;
        scan_reply(&r)
    }

    /// Type and TTL (milliseconds; -1 none, -2 gone) of many keys, pipelined.
    pub fn types_and_ttls(&mut self, keys: &[Vec<u8>]) -> DbResult<Vec<(String, i64)>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(200) {
            let mut cmds = Vec::with_capacity(chunk.len() * 2);
            for k in chunk {
                cmds.push(vec![b"TYPE".to_vec(), k.clone()]);
                cmds.push(vec![b"PTTL".to_vec(), k.clone()]);
            }
            let replies = self.pipeline(&cmds)?;
            for pair in replies.chunks(2) {
                let kind = pair.first().and_then(Reply::as_text).unwrap_or_else(|| "none".into());
                let ttl = pair.get(1).and_then(Reply::as_int).unwrap_or(-1);
                out.push((kind, ttl));
            }
        }
        Ok(out)
    }

    /// The type, TTL and size of one key.
    pub fn key_info(&mut self, key: &[u8]) -> DbResult<KeyInfo> {
        let (kind, ttl_ms) = self.types_and_ttls(&[key.to_vec()])?.into_iter().next().unwrap_or(("none".into(), -2));
        let len_cmd: Option<&[u8]> = match kind.as_str() {
            "string" => Some(b"STRLEN"),
            "hash" => Some(b"HLEN"),
            "list" => Some(b"LLEN"),
            "set" => Some(b"SCARD"),
            "zset" => Some(b"ZCARD"),
            "stream" => Some(b"XLEN"),
            _ => None,
        };
        let length = match len_cmd {
            Some(c) => self.call(&[c, key])?.as_int().and_then(|n| u64::try_from(n).ok()),
            None => None,
        };
        let memory = self
            .call(&[b"MEMORY".as_slice(), b"USAGE", key])
            .ok()
            .and_then(|r| r.as_int())
            .and_then(|n| u64::try_from(n).ok());
        Ok(KeyInfo { kind, ttl_ms, length, memory })
    }

    /// A page of a key's value: `offset` is an index (lists, sorted sets) or a SCAN cursor
    /// (hashes, sets) or the last stream id read (streams; empty from the start).
    pub fn load_value(&mut self, key: &[u8], kind: &str, offset: &str, limit: usize) -> DbResult<ValuePage> {
        let limit = limit.clamp(1, 10_000);
        let start = offset.parse::<i64>().unwrap_or(0).max(0);
        let stop = start.saturating_add(i64::try_from(limit).unwrap_or(500)).saturating_sub(1);
        let num = |n: i64| n.to_string().into_bytes();
        Ok(match kind {
            "string" => {
                let max = i64::try_from(MAX_STRING_SHOWN).unwrap_or(i64::MAX).saturating_sub(1);
                let r = self.ok(&[b"GETRANGE".to_vec(), key.to_vec(), b"0".to_vec(), num(max)])?;
                let len = self.ok(&[b"STRLEN".as_slice(), key])?.as_int().unwrap_or(0);
                let bytes = r.as_bytes().unwrap_or_default();
                let truncated = usize::try_from(len).unwrap_or(0) > bytes.len();
                ValuePage { value: KeyValue::String(bytes), next: None, truncated }
            }
            "list" => {
                let r = self.ok(&[b"LRANGE".to_vec(), key.to_vec(), num(start), num(stop)])?;
                let items: Vec<Vec<u8>> = r.items().iter().map(|i| i.as_bytes().unwrap_or_default()).collect();
                let next = (items.len() >= limit).then(|| (start + items.len() as i64).to_string());
                ValuePage { value: KeyValue::List(items), next, truncated: false }
            }
            "zset" => {
                let r = self.ok(&[b"ZRANGE".to_vec(), key.to_vec(), num(start), num(stop), b"WITHSCORES".to_vec()])?;
                let items = r.items();
                let pairs: Vec<(Vec<u8>, String)> = items
                    .chunks(2)
                    .map(|p| {
                        (
                            p.first().and_then(Reply::as_bytes).unwrap_or_default(),
                            p.get(1).and_then(Reply::as_text).unwrap_or_default(),
                        )
                    })
                    .collect();
                let next = (pairs.len() >= limit).then(|| (start + pairs.len() as i64).to_string());
                ValuePage { value: KeyValue::ZSet(pairs), next, truncated: false }
            }
            "hash" | "set" => {
                let cmd: &[u8] = if kind == "hash" { b"HSCAN" } else { b"SSCAN" };
                let cursor = if offset.is_empty() { "0" } else { offset };
                let mut cursor = cursor.to_string();
                let mut items: Vec<Reply> = Vec::new();
                // SCAN may return few elements per call; keep going until a page is full.
                for _ in 0..64 {
                    let r = self.ok(&[cmd, key, cursor.as_bytes(), b"COUNT", limit.to_string().as_bytes()])?;
                    let parts = r.items();
                    cursor = parts.first().and_then(Reply::as_text).unwrap_or_else(|| "0".into());
                    items.extend(parts.get(1).map(Reply::items).unwrap_or_default());
                    let have = if kind == "hash" { items.len() / 2 } else { items.len() };
                    if cursor == "0" || have >= limit {
                        break;
                    }
                }
                let next = (cursor != "0").then_some(cursor);
                if kind == "hash" {
                    let pairs = items
                        .chunks(2)
                        .map(|p| {
                            (
                                p.first().and_then(Reply::as_bytes).unwrap_or_default(),
                                p.get(1).and_then(Reply::as_bytes).unwrap_or_default(),
                            )
                        })
                        .collect();
                    ValuePage { value: KeyValue::Hash(pairs), next, truncated: false }
                } else {
                    let members = items.iter().map(|m| m.as_bytes().unwrap_or_default()).collect();
                    ValuePage { value: KeyValue::Set(members), next, truncated: false }
                }
            }
            "stream" => {
                let from = if offset.is_empty() { "-".to_string() } else { format!("({offset}") };
                let r = self.ok(&[
                    b"XRANGE".to_vec(),
                    key.to_vec(),
                    from.into_bytes(),
                    b"+".to_vec(),
                    b"COUNT".to_vec(),
                    limit.to_string().into_bytes(),
                ])?;
                let entries = stream_entries(&r);
                let next = (entries.len() >= limit).then(|| entries.last().map(|e| e.id.clone())).flatten();
                ValuePage { value: KeyValue::Stream(entries), next, truncated: false }
            }
            "ReJSON-RL" | "json" => {
                let r = self.ok(&[b"JSON.GET".as_slice(), key])?;
                ValuePage { value: KeyValue::Json(r.as_text().unwrap_or_default()), next: None, truncated: false }
            }
            "none" => return Err(DbError::Query(format!("the key {} no longer exists", display_key(key)))),
            other => ValuePage {
                value: KeyValue::Unsupported(format!("values of type `{other}` cannot be shown")),
                next: None,
                truncated: false,
            },
        })
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn tls_stream(cfg: &DataSourceConfig, socket: TcpStream) -> DbResult<Stream> {
    let mut b = native_tls::TlsConnector::builder();
    if option_on(cfg, "tls_insecure") || option_on(cfg, "trust_server_certificate") {
        b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
    }
    let files = cfg.tls_files(None)?;
    if let Some(root) = &files.root_cert {
        let pem = std::fs::read(root).map_err(|e| DbError::Config(format!("{}: {e}", root.display())))?;
        let cert =
            native_tls::Certificate::from_pem(&pem).map_err(|e| DbError::Config(format!("{}: {e}", root.display())))?;
        b.add_root_certificate(cert);
    }
    if let (Some(c), Some(k)) = (&files.client_cert, &files.client_key) {
        let cert = std::fs::read(c).map_err(|e| DbError::Config(format!("{}: {e}", c.display())))?;
        let key = std::fs::read(k).map_err(|e| DbError::Config(format!("{}: {e}", k.display())))?;
        let id = native_tls::Identity::from_pkcs8(&cert, &key).map_err(|e| DbError::Config(e.to_string()))?;
        b.identity(id);
    }
    let connector = b.build().map_err(|e| DbError::Connect(format!("TLS: {e}")))?;
    let host =
        cfg.options.get("tls_server_name").map(|s| s.trim().to_string()).unwrap_or_else(|| cfg.host.trim().to_string());
    let s = connector.connect(&host, socket).map_err(|e| DbError::Connect(format!("TLS handshake failed: {e}")))?;
    Ok(Stream::Tls(Box::new(s)))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn tls_stream(_cfg: &DataSourceConfig, _socket: TcpStream) -> DbResult<Stream> {
    Err(DbError::Unsupported("TLS for Redis is not supported on this platform yet".into()))
}

/// Longest string value loaded into the viewer.
pub const MAX_STRING_SHOWN: usize = 4 * 1024 * 1024;

/// What the key browser shows about a key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyInfo {
    /// `string`, `hash`, `list`, `set`, `zset`, `stream`, `ReJSON-RL`, `none` (gone) …
    pub kind: String,
    /// Milliseconds to live; -1 without expiry, -2 when the key is gone.
    pub ttl_ms: i64,
    /// Length in bytes (strings) or elements.
    pub length: Option<u64>,
    /// `MEMORY USAGE`, when the server allows it.
    pub memory: Option<u64>,
}

/// A stream entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<(Vec<u8>, Vec<u8>)>,
}

/// A loaded value (or a page of it).
#[derive(Clone, Debug, PartialEq)]
pub enum KeyValue {
    String(Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    /// Members with their scores as Redis prints them.
    ZSet(Vec<(Vec<u8>, String)>),
    Stream(Vec<StreamEntry>),
    Json(String),
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValuePage {
    pub value: KeyValue,
    /// Where the next page starts; `None` at the end.
    pub next: Option<String>,
    /// A string longer than [`MAX_STRING_SHOWN`] was cut.
    pub truncated: bool,
}

fn stream_entries(r: &Reply) -> Vec<StreamEntry> {
    r.items()
        .iter()
        .map(|e| {
            let parts = e.items();
            let id = parts.first().and_then(Reply::as_text).unwrap_or_default();
            let fields = parts
                .get(1)
                .map(Reply::items)
                .unwrap_or_default()
                .chunks(2)
                .map(|p| {
                    (
                        p.first().and_then(Reply::as_bytes).unwrap_or_default(),
                        p.get(1).and_then(Reply::as_bytes).unwrap_or_default(),
                    )
                })
                .collect();
            StreamEntry { id, fields }
        })
        .collect()
}

fn scan_reply(r: &Reply) -> DbResult<(String, Vec<Vec<u8>>)> {
    let parts = r.items();
    let cursor = parts.first().and_then(Reply::as_text).ok_or_else(|| proto("SCAN reply without a cursor"))?;
    let keys = parts.get(1).map(Reply::items).unwrap_or_default().iter().filter_map(Reply::as_bytes).collect();
    Ok((cursor, keys))
}

/// `INFO server` text → a short version label.
pub fn server_label(info: &str) -> String {
    let field = |k: &str| {
        info.lines()
            .find_map(|l| l.trim().strip_prefix(k).and_then(|r| r.strip_prefix(':')).map(|v| v.trim().to_string()))
    };
    if let Some(v) = field("valkey_version") {
        return format!("Valkey {v}");
    }
    if let Some(v) = field("dragonfly_version") {
        return format!("Dragonfly {v}");
    }
    let name = match field("server_name").as_deref() {
        Some("valkey") => "Valkey",
        Some("keydb") => "KeyDB",
        _ => "Redis",
    };
    match field("redis_version") {
        Some(v) => format!("{name} {v}"),
        None => name.to_string(),
    }
}

/// `INFO keyspace` text → (database, keys).
pub fn parse_keyspace(info: &str) -> Vec<(u32, u64)> {
    info.lines()
        .filter_map(|l| {
            let (db, rest) = l.trim().split_once(':')?;
            let n = db.strip_prefix("db")?.parse::<u32>().ok()?;
            let keys = rest.split(',').find_map(|kv| kv.strip_prefix("keys="))?.parse::<u64>().ok()?;
            Some((n, keys))
        })
        .collect()
}

// ---------------------------------------------------------------- console connection

/// The `Connection` of Redis consoles: each "statement" is one command line.
pub(crate) struct RedisConn {
    client: RedisClient,
    databases: u32,
}

impl RedisConn {
    pub(crate) fn open(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Self> {
        let mut client = RedisClient::connect(cfg, password, None)?;
        let databases = client.database_count();
        Ok(Self { client, databases })
    }
}

fn col(name: &str) -> Column {
    Column { name: name.into(), type_name: String::new() }
}

/// Turn a reply into grid columns and rows, shaped by the command that produced it.
pub fn reply_grid(args: &[Vec<u8>], reply: &Reply) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let cmd = args.first().map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).unwrap_or_default();
    let has = |w: &str| args.iter().skip(1).any(|a| a.eq_ignore_ascii_case(w.as_bytes()));
    let pairs = |names: [&str; 2], items: &[Reply]| -> (Vec<Column>, Vec<Vec<Cell>>) {
        let rows = items
            .chunks(2)
            .map(|p| {
                vec![
                    p.first().map(Reply::to_cell).unwrap_or(Cell::Null),
                    p.get(1).map(Reply::to_cell).unwrap_or(Cell::Null),
                ]
            })
            .collect();
        (vec![col(names[0]), col(names[1])], rows)
    };
    let items = reply.items();
    let scalar_items = items.iter().all(Reply::is_scalar);
    match (cmd.as_str(), reply) {
        ("INFO", r) if r.is_scalar() => {
            let text = r.as_text().unwrap_or_default();
            let mut section = String::new();
            let mut rows = Vec::new();
            for line in text.lines() {
                let line = line.trim();
                if let Some(s) = line.strip_prefix("# ") {
                    section = s.to_string();
                } else if let Some((k, v)) = line.split_once(':') {
                    rows.push(vec![Cell::Text(section.clone()), Cell::Text(k.into()), Cell::Text(v.into())]);
                }
            }
            (vec![col("section"), col("key"), col("value")], rows)
        }
        ("HGETALL" | "CONFIG", Reply::Array(_) | Reply::Map(_)) if scalar_items => pairs(["field", "value"], &items),
        ("HRANDFIELD", Reply::Array(_)) if has("WITHVALUES") && scalar_items => pairs(["field", "value"], &items),
        (
            "ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE" | "ZRANDMEMBER" | "ZDIFF" | "ZUNION"
            | "ZINTER",
            Reply::Array(_),
        ) if has("WITHSCORES") && scalar_items => pairs(["member", "score"], &items),
        ("ZPOPMIN" | "ZPOPMAX", Reply::Array(_)) if scalar_items => pairs(["member", "score"], &items),
        ("SCAN" | "SSCAN" | "HSCAN" | "ZSCAN", Reply::Array(parts)) if parts.len() == 2 => {
            let cursor = parts.first().and_then(Reply::as_text).unwrap_or_default();
            let found = parts.get(1).map(Reply::items).unwrap_or_default();
            let next = format!("next cursor {cursor}");
            match cmd.as_str() {
                "HSCAN" if !has("NOVALUES") => pairs(["field", &next], &found),
                "ZSCAN" => pairs(["member", &next], &found),
                _ => (
                    vec![col(&format!("{} ({next})", if cmd == "SCAN" { "key" } else { "member" }))],
                    found.iter().map(|k| vec![k.to_cell()]).collect(),
                ),
            }
        }
        ("XRANGE" | "XREVRANGE", Reply::Array(_)) => stream_grid(&stream_entries(reply)),
        (_, Reply::Map(m)) => {
            let rows = m.iter().map(|(k, v)| vec![k.to_cell(), v.to_cell()]).collect();
            (vec![col("field"), col("value")], rows)
        }
        (_, Reply::Array(v)) => (vec![col("value")], v.iter().map(|i| vec![i.to_cell()]).collect()),
        (_, r) => (vec![col("value")], vec![vec![r.to_cell()]]),
    }
}

/// Stream entries as rows: the id, then one column per field name (in order of appearance).
fn stream_grid(entries: &[StreamEntry]) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let mut names: Vec<Vec<u8>> = Vec::new();
    for e in entries {
        for (f, _) in &e.fields {
            if !names.contains(f) && names.len() < 200 {
                names.push(f.clone());
            }
        }
    }
    let mut cols = vec![col("id")];
    cols.extend(names.iter().map(|n| col(&display_key(n))));
    let rows = entries
        .iter()
        .map(|e| {
            let mut row = vec![Cell::Text(e.id.clone())];
            for n in &names {
                row.push(match e.fields.iter().find(|(f, _)| f == n) {
                    Some((_, v)) => Reply::Bulk(v.clone()).to_cell(),
                    None => Cell::Null,
                });
            }
            row
        })
        .collect();
    (cols, rows)
}

impl Connection for RedisConn {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }

    fn server_version(&mut self) -> DbResult<String> {
        self.client.server_version()
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.client.canceller()
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        let started = Instant::now();
        let args = split_args(sql.trim()).map_err(|e| DbError::Query(format!("cannot parse the command: {e}")))?;
        if args.is_empty() {
            return Err(DbError::Query("empty command".into()));
        }
        let cmd = args.first().map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).unwrap_or_default();
        if matches!(cmd.as_str(), "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC") {
            return Err(DbError::Unsupported(format!("{cmd} streams forever; use redis-cli in the terminal")));
        }
        let reply = self.client.call(&args)?;
        if let Reply::Error(e) = &reply {
            return Err(DbError::Query(e.clone()));
        }
        if cmd == "SELECT"
            && let Some(n) = args.get(1).and_then(|a| std::str::from_utf8(a).ok()).and_then(|s| s.parse().ok())
        {
            self.client.db = n;
        }
        let (columns, rows) = reply_grid(&args, &reply);
        sink.columns(&columns);
        let mut pager = Pager::new(sink, page_size, started);
        for row in rows {
            if !pager.push(row) {
                break;
            }
        }
        Ok(pager.finish(columns, None))
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let schemas = (0..self.databases).map(|n| format!("db{n}")).collect();
        Ok(DatabaseInfo {
            databases: Vec::new(),
            current: None,
            schemas,
            default_schema: Some(format!("db{}", self.client.db())),
        })
    }

    /// Redis has keys, not tables: the tree shows the key browser instead.
    fn tables(&mut self, _schema: &str) -> DbResult<Vec<TableInfo>> {
        Ok(Vec::new())
    }

    fn table_details(&mut self, _schema: &str, _table: &str) -> DbResult<TableDetails> {
        Err(DbError::Unsupported("Redis has no tables; use the key browser".into()))
    }

    fn row_count(&mut self, _schema: &str, _table: &str) -> DbResult<u64> {
        Err(DbError::Unsupported("Redis has no tables; use the key browser".into()))
    }

    fn transaction_status(&mut self) -> crate::tx::TxStatus {
        crate::tx::TxStatus::Idle
    }

    fn is_broken(&mut self) -> bool {
        self.client.is_broken()
    }

    fn ping(&mut self) -> DbResult<()> {
        self.client.ok(&[b"PING".as_slice()]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_reply_type() {
        assert_eq!(parse_reply(b"+OK\r\n").unwrap(), Reply::Status("OK".into()));
        assert_eq!(parse_reply(b"-ERR bad\r\n").unwrap(), Reply::Error("ERR bad".into()));
        assert_eq!(parse_reply(b":-42\r\n").unwrap(), Reply::Int(-42));
        assert_eq!(parse_reply(b"$3\r\nabc\r\n").unwrap(), Reply::Bulk(b"abc".to_vec()));
        assert_eq!(parse_reply(b"$-1\r\n").unwrap(), Reply::Nil);
        assert_eq!(parse_reply(b"*-1\r\n").unwrap(), Reply::Nil);
        assert_eq!(parse_reply(b"$0\r\n\r\n").unwrap(), Reply::Bulk(Vec::new()));
        assert_eq!(
            parse_reply(b"*2\r\n$1\r\na\r\n*1\r\n:1\r\n").unwrap(),
            Reply::Array(vec![Reply::Bulk(b"a".to_vec()), Reply::Array(vec![Reply::Int(1)])])
        );
        assert_eq!(
            parse_reply(b"%1\r\n+k\r\n:2\r\n").unwrap(),
            Reply::Map(vec![(Reply::Status("k".into()), Reply::Int(2))])
        );
        assert_eq!(parse_reply(b"_\r\n").unwrap(), Reply::Nil);
        assert_eq!(parse_reply(b"#t\r\n").unwrap(), Reply::Bool(true));
        assert_eq!(parse_reply(b",1.5\r\n").unwrap(), Reply::Double(1.5));
        assert_eq!(parse_reply(b",-inf\r\n").unwrap(), Reply::Double(f64::NEG_INFINITY));
        assert_eq!(parse_reply(b"=7\r\ntxt:abc\r\n").unwrap(), Reply::Text("abc".into()));
        assert_eq!(parse_reply(b"|1\r\n+a\r\n+b\r\n:5\r\n").unwrap(), Reply::Int(5));
    }

    #[test]
    fn hostile_replies_are_errors_not_panics() {
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\r\n".to_vec(),
            b"?\r\n".to_vec(),
            b"$5\r\nab\r\n".to_vec(),
            b"$3\r\nabcXY".to_vec(),
            b"$99999999999999999999\r\n".to_vec(),
            b"$-5000\r\n".to_vec(),
            format!("${}\r\n", MAX_BULK + 1).into_bytes(),
            b"*9223372036854775807\r\n".to_vec(),
            b"*3\r\n:1\r\n".to_vec(),
            b":abc\r\n".to_vec(),
            b",nan-ish\r\n".to_vec(),
            b"%-1\r\n".to_vec(),
            b"*1\r\n".repeat(100),
            vec![b'+'; MAX_LINE + 10],
            b"$\xff\r\n".to_vec(),
            b"=2\r\nab\r\n".to_vec(),
        ];
        for c in cases {
            let _ = parse_reply(&c);
        }
        assert!(parse_reply(&b"*1\r\n".repeat(100)).is_err(), "depth limit");
        assert!(parse_reply(b"*9223372036854775807\r\n").is_err());
        assert!(parse_reply(b"$5\r\nab\r\n").is_err());
        // Pseudo-random garbage.
        let mut seed = 0x2545F4914F6CDD1Du64;
        for _ in 0..2000 {
            let mut buf = Vec::new();
            for _ in 0..(seed % 64) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let alphabet = b"*$:+-%_#,=|!(~>0123456789\r\n-abc";
                buf.push(alphabet[(seed % alphabet.len() as u64) as usize]);
            }
            let _ = parse_reply(&buf);
        }
    }

    #[test]
    fn command_lines_split_like_redis_cli() {
        let a = |s: &str| split_args(s).unwrap().into_iter().map(|v| String::from_utf8(v).unwrap()).collect::<Vec<_>>();
        assert_eq!(a("SET  k   v"), ["SET", "k", "v"]);
        assert_eq!(a(r#"SET k "a b\n\"c\"""#), ["SET", "k", "a b\n\"c\""]);
        assert_eq!(a(r"SET k 'it\'s'"), ["SET", "k", "it's"]);
        assert_eq!(a(r#"SET k "\x41\x42""#), ["SET", "k", "AB"]);
        assert_eq!(a(r#"SET k """#), ["SET", "k", ""]);
        assert_eq!(a("GET é"), ["GET", "é"]);
        assert!(split_args(r#"SET k "abc"#).is_err());
        assert!(split_args(r#"SET k "a"b"#).is_err());
        assert!(split_args(r#"SET k "\"#).is_err());
        assert_eq!(split_args(r#"SET k "\xff""#).unwrap()[2], vec![0xff]);
        for s in ["", "   ", "\"", "'", "\\", "\"\\x\"", "\"\\x4\""] {
            let _ = split_args(s);
        }
        for raw in [
            b"plain".to_vec(),
            b"a b".to_vec(),
            b"q\"uote".to_vec(),
            vec![0, 0xff, b'x'],
            Vec::new(),
            "é\n".as_bytes().to_vec(),
        ] {
            let q = quote_arg(&raw);
            assert_eq!(split_args(&format!("GET {q}")).unwrap()[1], raw, "{q}");
        }
    }

    #[test]
    fn write_commands_are_recognised() {
        for w in [
            "SET k v",
            "del k",
            "FLUSHALL",
            "CONFIG SET maxmemory 1",
            "CLIENT KILL ID 1",
            "EVAL x 0",
            "HSET h f v",
            "FOO",
        ] {
            assert!(is_write_command(w), "{w}");
        }
        for r in [
            "GET k",
            "hgetall h",
            "SCAN 0 MATCH a:*",
            "CONFIG GET databases",
            "INFO",
            "TTL k",
            "TYPE k",
            "",
            "CLIENT LIST",
            "SELECT 2",
        ] {
            assert!(!is_write_command(r), "{r}");
        }
        assert!(is_write_command("SET \"unbalanced"));
    }

    #[test]
    fn consoles_split_into_lines() {
        let s = split_commands("SET a 1\n\n# comment\n  GET a  \nSET b \"x\ny\"\n// c\nDEL a");
        let texts: Vec<&str> = s.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["SET a 1", "GET a", "SET b \"x\ny\"", "DEL a"]);
        assert_eq!(s[1].start, "SET a 1\n\n# comment\n  ".chars().count());
        assert!(split_commands("").is_empty());
        let _ = split_commands("SET \"a\\");
    }

    #[test]
    fn replies_become_grids() {
        let args = |s: &str| split_args(s).unwrap();
        let (c, r) = reply_grid(&args("GET k"), &Reply::Bulk(b"v".to_vec()));
        assert_eq!((c.len(), r), (1, vec![vec![Cell::Text("v".into())]]));
        let h = Reply::Array(vec![Reply::Bulk(b"f".to_vec()), Reply::Bulk(b"1".to_vec())]);
        let (c, r) = reply_grid(&args("HGETALL h"), &h);
        assert_eq!(c[0].name, "field");
        assert_eq!(r, vec![vec![Cell::Text("f".into()), Cell::Text("1".into())]]);
        let (c, _) = reply_grid(&args("ZRANGE z 0 -1 WITHSCORES"), &h);
        assert_eq!(c[1].name, "score");
        let scan = Reply::Array(vec![Reply::Bulk(b"17".to_vec()), Reply::Array(vec![Reply::Bulk(b"a".to_vec())])]);
        let (c, r) = reply_grid(&args("SCAN 0"), &scan);
        assert_eq!(c[0].name, "key (next cursor 17)");
        assert_eq!(r.len(), 1);
        let x = parse_reply(b"*1\r\n*2\r\n$3\r\n1-0\r\n*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n").unwrap();
        let (c, r) = reply_grid(&args("XRANGE s - +"), &x);
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "a", "b"]);
        assert_eq!(r[0][2], Cell::Text("2".into()));
        let (c, r) = reply_grid(&args("INFO"), &Reply::Bulk(b"# Server\r\nredis_version:7.2.4\r\n".to_vec()));
        assert_eq!(c.len(), 3);
        assert_eq!(r[0][2], Cell::Text("7.2.4".into()));
        let (_, r) = reply_grid(&args("LRANGE l 0 -1"), &Reply::Array(vec![Reply::Nil, Reply::Bulk(vec![0xff])]));
        assert_eq!(r, vec![vec![Cell::Null], vec![Cell::Bytes(vec![0xff])]]);
        let (_, r) = reply_grid(&args("X"), &Reply::Array(vec![Reply::Array(vec![Reply::Int(1)])]));
        assert_eq!(r[0][0], Cell::Other("[1]".into()));
    }

    #[test]
    fn info_fields() {
        assert_eq!(server_label("# Server\r\nredis_version:7.2.4\r\n"), "Redis 7.2.4");
        assert_eq!(
            server_label("redis_version:7.2.4\r\nserver_name:valkey\r\nvalkey_version:8.0.1\r\n"),
            "Valkey 8.0.1"
        );
        assert_eq!(
            parse_keyspace("# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=0\r\ndb5:keys=10,expires=0\r\nbad\r\n"),
            vec![(0, 3), (5, 10)]
        );
        assert_eq!(display_key(b"user:1"), "user:1");
        assert_eq!(display_key(b"a\nb"), "\"a\\nb\"");
        assert_eq!(display_key(&[0xff]), "\"\\xff\"");
        let cfg = DataSourceConfig { database: "3".into(), ..Default::default() };
        assert_eq!(database_index(&cfg).unwrap(), 3);
        assert!(database_index(&DataSourceConfig { database: "x".into(), ..Default::default() }).is_err());
    }
}
