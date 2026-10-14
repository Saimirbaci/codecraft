//! The Redis key browser (track DD): keys found with SCAN (pattern and type filter, never KEYS)
//! in pages, grouped into `:` namespaces, a value view per type with paging, and edits (values,
//! TTL, rename, delete, new keys). Each browsed data source has one connection on its own thread
//! (through the data source's SSH tunnel); the worker answers requests through a channel that
//! [`pump`] drains. Results carry a generation so stale answers (an older scan, another key) are
//! dropped.
//!
//! Edits go through `db.redis.*` commands, which apply the read-only and production guards
//! (`database_cmds/redis.rs`); [`WriteOp::commands`] says exactly what is sent.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::Serialize;

use codecraft_db::redis::{KeyInfo, KeyValue, RedisClient, Reply, ValuePage, display_key, quote_arg};
use codecraft_db::{DataSourceConfig, DbError, SecretStore};

use super::DbState;
use crate::Session;

/// Keys a browser keeps (more must be found with a narrower pattern).
pub const MAX_KEYS: usize = 100_000;
/// Keys asked for per page.
pub const PAGE_KEYS: usize = 500;
/// Elements of a value per page.
pub const PAGE_VALUES: usize = 500;
/// SCAN calls per page (each returns about COUNT keys examined).
const MAX_SCAN_STEPS: usize = 200;
/// The browser connection's reply deadline.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// One key in the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRow {
    #[serde(skip)]
    pub key: Vec<u8>,
    /// The key as shown (`redis-cli` quoting when not printable UTF-8).
    pub name: String,
    pub kind: String,
    /// Milliseconds to live (-1: no expiry).
    pub ttl_ms: i64,
}

/// A row of a value table (hash field, list item, set member …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ValueRow {
    pub cells: Vec<String>,
    /// The raw values behind the cells (for edits).
    #[serde(skip)]
    pub raw: Vec<Vec<u8>>,
}

/// The selected key's value.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueView {
    #[serde(skip)]
    pub key: Vec<u8>,
    pub name: String,
    pub kind: String,
    pub ttl_ms: i64,
    pub length: Option<u64>,
    pub memory: Option<u64>,
    /// String and JSON values as text (lossy for binary strings).
    pub text: Option<String>,
    /// A string value that is not UTF-8 (shown as hex, edited as text only by replacing it).
    pub binary: bool,
    /// Table values: column names and rows.
    pub columns: Vec<String>,
    pub rows: Vec<ValueRow>,
    /// Where the next page starts (`None`: all loaded).
    pub next: Option<String>,
    /// A long string was cut.
    pub truncated: bool,
    /// A type the browser cannot show.
    pub unsupported: Option<String>,
}

impl ValueView {
    fn new(key: Vec<u8>, info: &KeyInfo) -> Self {
        Self {
            name: display_key(&key),
            key,
            kind: info.kind.clone(),
            ttl_ms: info.ttl_ms,
            length: info.length,
            memory: info.memory,
            ..Self::default()
        }
    }

    /// Add a page of the value.
    fn add(&mut self, page: ValuePage, offset: usize) {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        self.next = page.next;
        self.truncated = page.truncated;
        let row = |raw: Vec<Vec<u8>>| ValueRow { cells: raw.iter().map(|r| text(r)).collect(), raw };
        match page.value {
            KeyValue::String(b) => {
                self.binary = std::str::from_utf8(&b).is_err();
                self.text = Some(if self.binary { hex(&b) } else { text(&b) });
            }
            KeyValue::Json(j) => self.text = Some(j),
            KeyValue::Hash(pairs) => {
                self.columns = vec!["field".into(), "value".into()];
                self.rows.extend(pairs.into_iter().map(|(f, v)| row(vec![f, v])));
            }
            KeyValue::List(items) => {
                self.columns = vec!["index".into(), "value".into()];
                for (i, v) in items.into_iter().enumerate() {
                    let index = (offset + i).to_string();
                    self.rows.push(ValueRow { cells: vec![index.clone(), text(&v)], raw: vec![index.into_bytes(), v] });
                }
            }
            KeyValue::Set(members) => {
                self.columns = vec!["member".into()];
                self.rows.extend(members.into_iter().map(|m| row(vec![m])));
            }
            KeyValue::ZSet(pairs) => {
                self.columns = vec!["member".into(), "score".into()];
                self.rows.extend(pairs.into_iter().map(|(m, s)| row(vec![m, s.into_bytes()])));
            }
            KeyValue::Stream(entries) => {
                if self.columns.is_empty() {
                    self.columns = vec!["id".into()];
                }
                for e in entries {
                    for (f, _) in &e.fields {
                        let name = text(f);
                        if !self.columns.contains(&name) && self.columns.len() < 200 {
                            self.columns.push(name);
                        }
                    }
                    let mut raw = vec![e.id.clone().into_bytes()];
                    for c in self.columns.iter().skip(1) {
                        raw.push(
                            e.fields.iter().find(|(f, _)| text(f) == *c).map(|(_, v)| v.clone()).unwrap_or_default(),
                        );
                    }
                    self.rows.push(row(raw));
                }
                // Rows read before a new field appeared get an empty cell for it.
                let n = self.columns.len();
                for r in &mut self.rows {
                    r.cells.resize(n, String::new());
                    r.raw.resize(n, Vec::new());
                }
            }
            KeyValue::Unsupported(why) => self.unsupported = Some(why),
        }
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::from("0x");
    for byte in b.iter().take(64 * 1024) {
        s.push_str(&format!("{byte:02X}"));
    }
    s
}

/// The key browser of one data source.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisBrowser {
    pub source: String,
    pub db: u32,
    pub databases: u32,
    /// Keys per database from `INFO keyspace`.
    pub keyspace: BTreeMap<u32, u64>,
    pub version: Option<String>,
    pub pattern: String,
    /// Server-side type filter (`string`, `hash`, …); `None` for every type.
    pub type_filter: Option<String>,
    pub keys: Vec<KeyRow>,
    /// The next SCAN cursor; `None` when the scan reached the end.
    pub cursor: Option<String>,
    pub scanning: bool,
    pub error: Option<String>,
    /// Namespace prefixes shown expanded (`user:`, `user:42:`).
    pub expanded: BTreeSet<String>,
    #[serde(skip)]
    pub selected: Option<Vec<u8>>,
    pub value: Option<ValueView>,
    pub loading_value: bool,
    pub value_error: Option<String>,
    /// The last edit's outcome.
    pub message: Option<String>,
    pub writing: bool,
    /// Bumped whenever the key list or the expanded namespaces change (views cache the tree).
    pub revision: u64,
    #[serde(skip)]
    seq: u64,
    #[serde(skip)]
    value_seq: u64,
}

impl RedisBrowser {
    pub fn selected_name(&self) -> Option<String> {
        self.selected.as_deref().map(display_key)
    }

    pub fn key_row(&self, key: &[u8]) -> Option<&KeyRow> {
        self.keys.iter().find(|k| k.key == key)
    }

    /// The tree rows of the key list (namespaces by `sep`), honouring [`Self::expanded`].
    pub fn tree(&self, sep: char) -> Vec<TreeRow> {
        namespace_rows(&self.keys, &self.expanded, sep)
    }
}

/// A row of the namespace tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeRow {
    pub depth: usize,
    pub label: String,
    /// A namespace: its prefix (with the trailing separator) and how many keys it holds.
    pub prefix: Option<String>,
    pub count: usize,
    pub expanded: bool,
    /// A key: its index in [`RedisBrowser::keys`].
    pub key: Option<usize>,
}

/// Group keys into namespaces: `a:b:c` sits in `a:` › `a:b:`. Namespaces first, then keys, each
/// sorted; only expanded namespaces show their content.
pub fn namespace_rows(keys: &[KeyRow], expanded: &BTreeSet<String>, sep: char) -> Vec<TreeRow> {
    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        keys: Vec<usize>,
        count: usize,
    }
    let mut root = Node::default();
    for (i, k) in keys.iter().enumerate() {
        let parts: Vec<&str> = k.name.split(sep).collect();
        let mut node = &mut root;
        node.count += 1;
        // At most 16 levels; deeper parts stay in the key's label.
        for part in parts.iter().take(parts.len().saturating_sub(1)).take(16) {
            node = node.children.entry((*part).to_string()).or_default();
            node.count += 1;
        }
        node.keys.push(i);
    }
    let mut out = Vec::new();
    fn walk(
        node: &Node,
        prefix: &str,
        depth: usize,
        keys: &[KeyRow],
        expanded: &BTreeSet<String>,
        sep: char,
        out: &mut Vec<TreeRow>,
    ) {
        for (name, child) in &node.children {
            let p = format!("{prefix}{name}{sep}");
            let open = expanded.contains(&p);
            out.push(TreeRow {
                depth,
                label: name.clone(),
                prefix: Some(p.clone()),
                count: child.count,
                expanded: open,
                key: None,
            });
            if open {
                walk(child, &p, depth + 1, keys, expanded, sep, out);
            }
        }
        let mut leaves: Vec<usize> = node.keys.clone();
        leaves.sort_by(|a, b| keys.get(*a).map(|k| &k.name).cmp(&keys.get(*b).map(|k| &k.name)));
        for i in leaves {
            let Some(k) = keys.get(i) else { continue };
            let label = k.name.strip_prefix(prefix).unwrap_or(&k.name).to_string();
            out.push(TreeRow { depth, label, prefix: None, count: 0, expanded: false, key: Some(i) });
        }
    }
    walk(&root, "", 0, keys, expanded, sep, &mut out);
    out
}

// ---------------------------------------------------------------- edits

/// A change to a key. [`WriteOp::commands`] gives the Redis commands it sends.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteOp {
    SetString {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    HashSet {
        key: Vec<u8>,
        field: Vec<u8>,
        value: Vec<u8>,
    },
    HashDelete {
        key: Vec<u8>,
        field: Vec<u8>,
    },
    ListSet {
        key: Vec<u8>,
        index: i64,
        value: Vec<u8>,
    },
    ListPush {
        key: Vec<u8>,
        value: Vec<u8>,
        head: bool,
    },
    ListRemove {
        key: Vec<u8>,
        index: i64,
    },
    SetAdd {
        key: Vec<u8>,
        member: Vec<u8>,
    },
    SetRemove {
        key: Vec<u8>,
        member: Vec<u8>,
    },
    ZSetAdd {
        key: Vec<u8>,
        member: Vec<u8>,
        score: String,
    },
    ZSetRemove {
        key: Vec<u8>,
        member: Vec<u8>,
    },
    StreamAdd {
        key: Vec<u8>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    },
    StreamDelete {
        key: Vec<u8>,
        id: String,
    },
    JsonSet {
        key: Vec<u8>,
        value: String,
    },
    /// `None` removes the expiry.
    Expire {
        key: Vec<u8>,
        ttl_ms: Option<i64>,
    },
    Delete {
        keys: Vec<Vec<u8>>,
    },
    Rename {
        key: Vec<u8>,
        to: Vec<u8>,
    },
    /// A new key of a type with a first value (`field`/`member`/`score` as the type needs).
    Create {
        key: Vec<u8>,
        kind: String,
        field: Vec<u8>,
        value: Vec<u8>,
        score: String,
    },
}

/// A value that marks a list item for removal by index (LSET then LREM).
const LIST_TOMBSTONE: &[u8] = b"__codecraft_removed_item__";

fn args(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|p| p.to_vec()).collect()
}

fn check_score(score: &str) -> Result<(), String> {
    let s = score.trim();
    if matches!(s, "+inf" | "-inf" | "inf") || s.parse::<f64>().is_ok_and(f64::is_finite) {
        Ok(())
    } else {
        Err(format!("`{score}` is not a score (a number, +inf or -inf)"))
    }
}

impl WriteOp {
    /// The key the change is about (the first one for Delete).
    pub fn key(&self) -> &[u8] {
        match self {
            WriteOp::SetString { key, .. }
            | WriteOp::HashSet { key, .. }
            | WriteOp::HashDelete { key, .. }
            | WriteOp::ListSet { key, .. }
            | WriteOp::ListPush { key, .. }
            | WriteOp::ListRemove { key, .. }
            | WriteOp::SetAdd { key, .. }
            | WriteOp::SetRemove { key, .. }
            | WriteOp::ZSetAdd { key, .. }
            | WriteOp::ZSetRemove { key, .. }
            | WriteOp::StreamAdd { key, .. }
            | WriteOp::StreamDelete { key, .. }
            | WriteOp::JsonSet { key, .. }
            | WriteOp::Expire { key, .. }
            | WriteOp::Rename { key, .. }
            | WriteOp::Create { key, .. } => key,
            WriteOp::Delete { keys } => keys.first().map(Vec::as_slice).unwrap_or_default(),
        }
    }

    /// The commands, in order (sent as one MULTI/EXEC when there are several).
    pub fn commands(&self) -> Result<Vec<Vec<Vec<u8>>>, String> {
        let n = |i: i64| i.to_string().into_bytes();
        Ok(match self {
            WriteOp::SetString { key, value } => vec![args(&[b"SET", key, value, b"KEEPTTL"])],
            WriteOp::HashSet { key, field, value } => vec![args(&[b"HSET", key, field, value])],
            WriteOp::HashDelete { key, field } => vec![args(&[b"HDEL", key, field])],
            WriteOp::ListSet { key, index, value } => vec![args(&[b"LSET", key, &n(*index), value])],
            WriteOp::ListPush { key, value, head } => {
                vec![args(&[if *head { b"LPUSH".as_slice() } else { b"RPUSH" }, key, value])]
            }
            WriteOp::ListRemove { key, index } => {
                vec![args(&[b"LSET", key, &n(*index), LIST_TOMBSTONE]), args(&[b"LREM", key, b"1", LIST_TOMBSTONE])]
            }
            WriteOp::SetAdd { key, member } => vec![args(&[b"SADD", key, member])],
            WriteOp::SetRemove { key, member } => vec![args(&[b"SREM", key, member])],
            WriteOp::ZSetAdd { key, member, score } => {
                check_score(score)?;
                vec![args(&[b"ZADD", key, score.trim().as_bytes(), member])]
            }
            WriteOp::ZSetRemove { key, member } => vec![args(&[b"ZREM", key, member])],
            WriteOp::StreamAdd { key, fields } => {
                if fields.is_empty() {
                    return Err("a stream entry needs at least one field".into());
                }
                let mut c = args(&[b"XADD", key, b"*"]);
                for (f, v) in fields {
                    c.push(f.clone());
                    c.push(v.clone());
                }
                vec![c]
            }
            WriteOp::StreamDelete { key, id } => vec![args(&[b"XDEL", key, id.as_bytes()])],
            WriteOp::JsonSet { key, value } => {
                serde_json::from_str::<serde_json::Value>(value).map_err(|e| format!("not JSON: {e}"))?;
                vec![args(&[b"JSON.SET", key, b"$", value.as_bytes()])]
            }
            WriteOp::Expire { key, ttl_ms: Some(ms) } if *ms > 0 => vec![args(&[b"PEXPIRE", key, &n(*ms)])],
            WriteOp::Expire { ttl_ms: Some(_), .. } => return Err("the TTL must be positive".into()),
            WriteOp::Expire { key, ttl_ms: None } => vec![args(&[b"PERSIST", key])],
            WriteOp::Delete { keys } => {
                if keys.is_empty() {
                    return Err("no keys to delete".into());
                }
                let mut c = vec![b"UNLINK".to_vec()];
                c.extend(keys.iter().cloned());
                vec![c]
            }
            WriteOp::Rename { key, to } => {
                if to.is_empty() {
                    return Err("the new name is empty".into());
                }
                vec![args(&[b"RENAMENX", key, to])]
            }
            WriteOp::Create { key, kind, field, value, score } => {
                if key.is_empty() {
                    return Err("the key name is empty".into());
                }
                match kind.as_str() {
                    "string" => vec![args(&[b"SET", key, value, b"NX"])],
                    "hash" => vec![args(&[b"HSETNX", key, field, value])],
                    "list" => vec![args(&[b"RPUSH", key, value])],
                    "set" => vec![args(&[b"SADD", key, value])],
                    "zset" => {
                        check_score(score)?;
                        vec![args(&[b"ZADD", key, b"NX", score.trim().as_bytes(), value])]
                    }
                    "stream" => vec![args(&[b"XADD", key, b"*", field, value])],
                    "json" | "ReJSON-RL" => {
                        let v = String::from_utf8_lossy(value);
                        serde_json::from_str::<serde_json::Value>(&v).map_err(|e| format!("not JSON: {e}"))?;
                        vec![args(&[b"JSON.SET", key, b"$", value, b"NX"])]
                    }
                    other => return Err(format!("cannot create keys of type `{other}`")),
                }
            }
        })
    }

    /// The commands as `redis-cli` text (the confirmation shows it).
    pub fn preview(&self) -> String {
        match self.commands() {
            Ok(cmds) => cmds
                .iter()
                .map(|c| c.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(e) => format!("(invalid: {e})"),
        }
    }

    /// A short description for messages.
    pub fn label(&self) -> String {
        let k = display_key(self.key());
        match self {
            WriteOp::Delete { keys } if keys.len() > 1 => format!("Deleted {} keys", keys.len()),
            WriteOp::Delete { .. } => format!("Deleted {k}"),
            WriteOp::Rename { to, .. } => format!("Renamed {k} to {}", display_key(to)),
            WriteOp::Expire { ttl_ms: None, .. } => format!("{k} no longer expires"),
            WriteOp::Expire { ttl_ms: Some(ms), .. } => format!("{k} expires in {}", ttl_text(*ms)),
            WriteOp::Create { .. } => format!("Created {k}"),
            _ => format!("Saved {k}"),
        }
    }
}

/// `1h 2m 3s` from milliseconds; `no expiry` for -1.
pub fn ttl_text(ms: i64) -> String {
    if ms < 0 {
        return if ms == -2 { "gone".into() } else { "no expiry".into() };
    }
    let s = ms / 1000;
    let (d, h, m, sec) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60, s % 60);
    let mut out = Vec::new();
    if d > 0 {
        out.push(format!("{d}d"));
    }
    if h > 0 {
        out.push(format!("{h}h"));
    }
    if m > 0 {
        out.push(format!("{m}m"));
    }
    if sec > 0 || out.is_empty() {
        out.push(if s == 0 { format!("{ms}ms") } else { format!("{sec}s") });
    }
    out.join(" ")
}

// ---------------------------------------------------------------- command params

/// Bytes from `name` (text) or `nameHex` (hex, for binary keys and values).
pub fn bytes_param(p: &serde_json::Value, name: &str) -> Result<Option<Vec<u8>>, String> {
    if let Some(h) = p.get(format!("{name}Hex")).and_then(serde_json::Value::as_str) {
        let h = h.trim().trim_start_matches("0x");
        if h.len() % 2 != 0 || !h.is_ascii() {
            return Err(format!("`{name}Hex` is not hex"));
        }
        let bytes: Result<Vec<u8>, String> = (0..h.len())
            .step_by(2)
            .map(|i| {
                h.get(i..i + 2)
                    .and_then(|b| u8::from_str_radix(b, 16).ok())
                    .ok_or_else(|| format!("`{name}Hex` is not hex"))
            })
            .collect();
        return bytes.map(Some);
    }
    match p.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone().into_bytes())),
        Some(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => Ok(Some(v.to_string().into_bytes())),
        Some(_) => Err(format!("`{name}` must be a string")),
    }
}

fn req_bytes(p: &serde_json::Value, name: &str) -> Result<Vec<u8>, String> {
    bytes_param(p, name)?.ok_or_else(|| format!("missing `{name}`"))
}

/// The change a `db.redis.*` edit command asks for.
pub fn op_from_params(id: &str, p: &serde_json::Value) -> Result<WriteOp, String> {
    let key = || req_bytes(p, "key");
    let int = |name: &str| -> Result<i64, String> {
        match p.get(name) {
            Some(serde_json::Value::Number(n)) => n.as_i64().ok_or_else(|| format!("`{name}` must be an integer")),
            Some(serde_json::Value::String(s)) => s.trim().parse().map_err(|_| format!("`{name}` must be an integer")),
            _ => Err(format!("missing `{name}`")),
        }
    };
    let score = || -> Result<String, String> {
        match p.get("score") {
            Some(serde_json::Value::Number(n)) => Ok(n.to_string()),
            Some(serde_json::Value::String(s)) => Ok(s.clone()),
            _ => Err("missing `score`".into()),
        }
    };
    Ok(match id {
        "db.redis.setString" => WriteOp::SetString { key: key()?, value: req_bytes(p, "value")? },
        "db.redis.hashSet" => {
            WriteOp::HashSet { key: key()?, field: req_bytes(p, "field")?, value: req_bytes(p, "value")? }
        }
        "db.redis.hashDelete" => WriteOp::HashDelete { key: key()?, field: req_bytes(p, "field")? },
        "db.redis.listSet" => WriteOp::ListSet { key: key()?, index: int("index")?, value: req_bytes(p, "value")? },
        "db.redis.listPush" => WriteOp::ListPush {
            key: key()?,
            value: req_bytes(p, "value")?,
            head: p.get("head").and_then(serde_json::Value::as_bool).unwrap_or(false),
        },
        "db.redis.listRemove" => WriteOp::ListRemove { key: key()?, index: int("index")? },
        "db.redis.setAdd" => WriteOp::SetAdd { key: key()?, member: req_bytes(p, "member")? },
        "db.redis.setRemove" => WriteOp::SetRemove { key: key()?, member: req_bytes(p, "member")? },
        "db.redis.zsetAdd" => WriteOp::ZSetAdd { key: key()?, member: req_bytes(p, "member")?, score: score()? },
        "db.redis.zsetRemove" => WriteOp::ZSetRemove { key: key()?, member: req_bytes(p, "member")? },
        "db.redis.streamAdd" => {
            let fields = match p.get("fields") {
                Some(serde_json::Value::Object(m)) => m
                    .iter()
                    .map(|(k, v)| {
                        let v = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (k.clone().into_bytes(), v.into_bytes())
                    })
                    .collect(),
                _ => return Err("missing `fields` ({field: value, …})".into()),
            };
            WriteOp::StreamAdd { key: key()?, fields }
        }
        "db.redis.streamDelete" => WriteOp::StreamDelete {
            key: key()?,
            id: p.get("id").and_then(serde_json::Value::as_str).ok_or("missing `id`")?.to_string(),
        },
        "db.redis.jsonSet" => WriteOp::JsonSet {
            key: key()?,
            value: match p.get("value") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => return Err("missing `value`".into()),
            },
        },
        "db.redis.setTtl" => {
            let ttl_ms = match p.get("seconds") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Number(n)) => {
                    let secs = n.as_f64().filter(|f| f.is_finite()).ok_or("`seconds` must be a number")?;
                    // Rounded to whole milliseconds; at most ~292 million years.
                    Some((secs * 1000.0).round().clamp(-1.0, 9.2e18) as i64)
                }
                Some(serde_json::Value::String(s)) if s.trim().is_empty() => None,
                Some(serde_json::Value::String(s)) => {
                    let secs: f64 = s.trim().parse().map_err(|_| "`seconds` must be a number")?;
                    if !secs.is_finite() {
                        return Err("`seconds` must be a number".into());
                    }
                    Some((secs * 1000.0).round().clamp(-1.0, 9.2e18) as i64)
                }
                Some(_) => return Err("`seconds` must be a number or null".into()),
            };
            WriteOp::Expire { key: key()?, ttl_ms }
        }
        "db.redis.deleteKeys" => {
            let mut keys = Vec::new();
            if let Some(k) = bytes_param(p, "key")? {
                keys.push(k);
            }
            if let Some(serde_json::Value::Array(list)) = p.get("keys") {
                for k in list {
                    keys.push(k.as_str().ok_or("`keys` must be strings")?.as_bytes().to_vec());
                }
            }
            WriteOp::Delete { keys }
        }
        "db.redis.renameKey" => WriteOp::Rename { key: key()?, to: req_bytes(p, "newName")? },
        "db.redis.newKey" => WriteOp::Create {
            key: key()?,
            kind: p.get("type").and_then(serde_json::Value::as_str).unwrap_or("string").to_string(),
            field: bytes_param(p, "field")?.unwrap_or_default(),
            value: bytes_param(p, "value")?.unwrap_or_default(),
            score: score().unwrap_or_else(|_| "0".into()),
        },
        other => return Err(format!("`{other}` is not a Redis edit")),
    })
}

/// The edit commands (each takes `dataSource?`, `key`/`keyHex`, its own fields, `confirm?`).
pub const EDIT_COMMANDS: &[&str] = &[
    "db.redis.setString",
    "db.redis.hashSet",
    "db.redis.hashDelete",
    "db.redis.listSet",
    "db.redis.listPush",
    "db.redis.listRemove",
    "db.redis.setAdd",
    "db.redis.setRemove",
    "db.redis.zsetAdd",
    "db.redis.zsetRemove",
    "db.redis.streamAdd",
    "db.redis.streamDelete",
    "db.redis.jsonSet",
    "db.redis.setTtl",
    "db.redis.deleteKeys",
    "db.redis.renameKey",
    "db.redis.newKey",
];

// ---------------------------------------------------------------- the worker

enum Req {
    Scan { seq: u64, db: u32, pattern: String, kind: Option<String>, cursor: String, want: usize },
    Keyspace,
    Load { seq: u64, db: u32, key: Vec<u8>, kind: Option<String>, offset: String },
    Write { db: u32, op: WriteOp },
}

pub(crate) enum Ev {
    Connected { version: String, databases: u32 },
    ConnectFailed(String),
    Scanned { seq: u64, keys: Vec<(Vec<u8>, String, i64)>, cursor: String },
    ScanFailed { seq: u64, error: String },
    Keyspace(Vec<(u32, u64)>),
    Loaded { seq: u64, key: Vec<u8>, info: Option<KeyInfo>, page: ValuePage, offset: usize },
    LoadFailed { seq: u64, error: String },
    Wrote { op: WriteOp, result: Result<String, String> },
}

/// Browsers by data source, and their workers.
pub struct RedisBrowsers {
    pub browsers: BTreeMap<String, RedisBrowser>,
    /// The data source whose browser window is shown.
    pub active: Option<String>,
    /// Events tagged with the data source and the worker that sent them.
    tx: Sender<(String, u64, Ev)>,
    rx: Receiver<(String, u64, Ev)>,
    workers: HashMap<String, (u64, Sender<Req>)>,
    next_worker: u64,
}

impl Default for RedisBrowsers {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self { browsers: BTreeMap::new(), active: None, tx, rx, workers: HashMap::new(), next_worker: 0 }
    }
}

impl std::fmt::Debug for RedisBrowsers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBrowsers").field("browsers", &self.browsers.len()).field("active", &self.active).finish()
    }
}

impl RedisBrowsers {
    pub fn get(&self, source: &str) -> Option<&RedisBrowser> {
        self.browsers.get(source)
    }

    /// The browser shown in the window.
    pub fn shown(&self) -> Option<&RedisBrowser> {
        self.browsers.get(self.active.as_deref()?)
    }

    /// Whether any browser waits for its worker.
    pub fn busy(&self) -> bool {
        self.browsers.values().any(|b| b.scanning || b.loading_value || b.writing)
    }
}

fn tunnel_key(source: &str) -> String {
    format!("redis:{source}")
}

/// What the worker thread needs to connect.
struct Spec {
    cfg: DataSourceConfig,
    tunnel: Option<crate::ssh_tunnel::TunnelPlan>,
    password: Option<String>,
    secrets: Arc<dyn SecretStore>,
}

fn connect(spec: Spec) -> Result<RedisClient, String> {
    let cfg = crate::ssh_tunnel::route(spec.tunnel, spec.cfg)?;
    let pw = match spec.password {
        Some(p) => Some(p),
        None if cfg.password_saved => spec.secrets.get(&cfg.id).map_err(|e| e.to_string())?,
        None => None,
    };
    RedisClient::connect(&cfg, pw.as_deref(), Some(READ_TIMEOUT)).map_err(|e| e.to_string())
}

fn worker(source: String, wid: u64, spec: Spec, rx: Receiver<Req>, tx: Sender<(String, u64, Ev)>) {
    let send = |ev: Ev| {
        let _ = tx.send((source.clone(), wid, ev));
    };
    let mut client = match connect(spec) {
        Ok(c) => c,
        Err(e) => {
            send(Ev::ConnectFailed(e));
            return;
        }
    };
    let version = client.server_version().unwrap_or_else(|_| "Redis".into());
    let databases = client.database_count();
    send(Ev::Connected { version, databases });
    if let Ok(ks) = client.keyspace() {
        send(Ev::Keyspace(ks));
    }
    for req in rx.iter() {
        if client.is_broken() {
            send(Ev::ConnectFailed("the connection to Redis was lost; reopen the key browser".into()));
            return;
        }
        let select = |c: &mut RedisClient, db: u32| -> Result<(), String> {
            if c.db() != db { c.select(db).map_err(|e| e.to_string()) } else { Ok(()) }
        };
        match req {
            Req::Keyspace => {
                if let Ok(ks) = client.keyspace() {
                    send(Ev::Keyspace(ks));
                }
            }
            Req::Scan { seq, db, pattern, kind, cursor, want } => {
                let r = select(&mut client, db)
                    .and_then(|()| scan_page(&mut client, &pattern, kind.as_deref(), cursor, want));
                send(match r {
                    Ok((keys, cursor)) => Ev::Scanned { seq, keys, cursor },
                    Err(error) => Ev::ScanFailed { seq, error },
                });
            }
            Req::Load { seq, db, key, kind, offset } => {
                let r = select(&mut client, db).and_then(|()| {
                    let info = match &kind {
                        Some(_) => None,
                        None => Some(client.key_info(&key).map_err(|e| e.to_string())?),
                    };
                    let kind = kind.or_else(|| info.as_ref().map(|i| i.kind.clone())).unwrap_or_default();
                    let page = client.load_value(&key, &kind, &offset, PAGE_VALUES).map_err(|e| e.to_string())?;
                    Ok((info, page))
                });
                let at = offset.parse::<usize>().unwrap_or(0);
                send(match r {
                    Ok((info, page)) => Ev::Loaded { seq, key, info, page, offset: at },
                    Err(error) => Ev::LoadFailed { seq, error },
                });
            }
            Req::Write { db, op } => {
                let result = select(&mut client, db).and_then(|()| write(&mut client, &op));
                send(Ev::Wrote { op, result });
                if let Ok(ks) = client.keyspace() {
                    send(Ev::Keyspace(ks));
                }
            }
        }
    }
}

/// A key found by SCAN: (key, type, TTL in ms).
type Found = (Vec<u8>, String, i64);

/// SCAN until `want` keys are found or the cursor wraps; then their types and TTLs.
fn scan_page(
    c: &mut RedisClient,
    pattern: &str,
    kind: Option<&str>,
    mut cursor: String,
    want: usize,
) -> Result<(Vec<Found>, String), String> {
    let mut found: Vec<Vec<u8>> = Vec::new();
    let mut server_filter = kind.is_some();
    for _ in 0..MAX_SCAN_STEPS {
        let r = c.scan(&cursor, pattern, if server_filter { kind } else { None }, 1000);
        let (next, mut keys) = match r {
            Ok(x) => x,
            // Before Redis 6 SCAN has no TYPE: filter here instead.
            Err(DbError::Query(e)) if server_filter && e.to_ascii_lowercase().contains("syntax") => {
                server_filter = false;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        found.append(&mut keys);
        cursor = next;
        if cursor == "0" || found.len() >= want {
            break;
        }
    }
    found.sort();
    found.dedup();
    let tt = c.types_and_ttls(&found).map_err(|e| e.to_string())?;
    let mut out: Vec<Found> =
        found.into_iter().zip(tt).map(|(k, (t, ttl))| (k, t, ttl)).filter(|(_, t, _)| t != "none").collect();
    if let Some(k) = kind.filter(|_| !server_filter) {
        out.retain(|(_, t, _)| t == k);
    }
    Ok((out, cursor))
}

fn reply_ok(r: Reply) -> Result<Reply, String> {
    match r {
        Reply::Error(e) => Err(e),
        r => Ok(r),
    }
}

/// Apply a change; the message says what happened.
fn write(c: &mut RedisClient, op: &WriteOp) -> Result<String, String> {
    let cmds = op.commands()?;
    match op {
        WriteOp::Rename { to, .. } => {
            let r = reply_ok(c.call(&cmds.first().cloned().unwrap_or_default()).map_err(|e| e.to_string())?)?;
            if r.as_int() == Some(0) {
                return Err(format!("a key named {} already exists", display_key(to)));
            }
        }
        WriteOp::Create { key, .. } => {
            let exists = c.call(&[b"EXISTS".as_slice(), key]).map_err(|e| e.to_string())?;
            if exists.as_int().unwrap_or(0) > 0 {
                return Err(format!("the key {} already exists", display_key(key)));
            }
            for cmd in &cmds {
                reply_ok(c.call(cmd).map_err(|e| e.to_string())?)?;
            }
        }
        WriteOp::SetString { key, value } => {
            let first = cmds.first().cloned().unwrap_or_default();
            match reply_ok(c.call(&first).map_err(|e| e.to_string())?) {
                Ok(_) => {}
                // Before Redis 6 there is no KEEPTTL: keep the TTL by hand.
                Err(e) if e.to_ascii_lowercase().contains("syntax") => {
                    let ttl = c.call(&[b"PTTL".as_slice(), key]).map_err(|e| e.to_string())?.as_int().unwrap_or(-1);
                    reply_ok(c.call(&[b"SET".as_slice(), key, value]).map_err(|e| e.to_string())?)?;
                    if ttl > 0 {
                        reply_ok(
                            c.call(&[b"PEXPIRE".to_vec(), key.clone(), ttl.to_string().into_bytes()])
                                .map_err(|e| e.to_string())?,
                        )?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        WriteOp::Delete { keys } => {
            match reply_ok(c.call(&cmds.first().cloned().unwrap_or_default()).map_err(|e| e.to_string())?) {
                Ok(_) => {}
                Err(e) if e.to_ascii_lowercase().contains("unknown command") => {
                    let mut del = vec![b"DEL".to_vec()];
                    del.extend(keys.iter().cloned());
                    reply_ok(c.call(&del).map_err(|e| e.to_string())?)?;
                }
                Err(e) => return Err(e),
            }
        }
        _ if cmds.len() > 1 => {
            // All or nothing.
            let mut batch = vec![vec![b"MULTI".to_vec()]];
            batch.extend(cmds.iter().cloned());
            batch.push(vec![b"EXEC".to_vec()]);
            let replies = c.pipeline(&batch).map_err(|e| e.to_string())?;
            for r in replies.iter().take(replies.len().saturating_sub(1)) {
                if let Reply::Error(e) = r {
                    let _ = c.call(&[b"DISCARD".as_slice()]);
                    return Err(e.clone());
                }
            }
            match replies.last() {
                Some(Reply::Array(items)) => {
                    if let Some(Reply::Error(e)) = items.iter().find(|r| matches!(r, Reply::Error(_))) {
                        return Err(e.clone());
                    }
                }
                Some(Reply::Error(e)) => return Err(e.clone()),
                Some(Reply::Nil) => return Err("the change was aborted".into()),
                _ => {}
            }
        }
        _ => {
            for cmd in &cmds {
                reply_ok(c.call(cmd).map_err(|e| e.to_string())?)?;
            }
        }
    }
    Ok(op.label())
}

// ---------------------------------------------------------------- engine side

impl DbState {
    fn redis_spec(&self, source: &str) -> Result<Spec, String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        if ds.config.kind != codecraft_db::DbKind::Redis {
            return Err(format!("{} is not a Redis data source", ds.config.name));
        }
        let cfg = self.resolved(&ds.config);
        let tunnel = self.plan_tunnel(&cfg)?.map(|t| t.keyed(tunnel_key(source)));
        Ok(Spec { cfg, tunnel, password: self.worker_password(source), secrets: self.secrets.clone() })
    }

    fn redis_send(&mut self, source: &str, req: Req) -> Result<(), String> {
        if !self.redis.workers.contains_key(source) {
            self.redis.next_worker += 1;
            let wid = self.redis.next_worker;
            let spec = self.redis_spec(source)?;
            let (tx, rx) = unbounded();
            let events = self.redis.tx.clone();
            let name = source.to_string();
            std::thread::Builder::new()
                .name(format!("redis-{}", source.chars().take(40).collect::<String>()))
                .spawn(move || worker(name, wid, spec, rx, events))
                .map_err(|e| format!("cannot start the Redis thread: {e}"))?;
            self.redis.workers.insert(source.to_string(), (wid, tx));
        }
        let sent = self.redis.workers.get(source).is_some_and(|(_, w)| w.send(req).is_ok());
        if !sent {
            self.redis.workers.remove(source);
            return Err("the Redis connection was closed; try again".into());
        }
        Ok(())
    }

    /// Open (or show) the key browser of a Redis data source, on database `db`.
    pub(crate) fn redis_open(&mut self, source: &str, db: Option<u32>) -> Result<(), String> {
        let default_db = self
            .source(source)
            .map(|d| codecraft_db::redis::database_index(&d.config).unwrap_or(0))
            .ok_or_else(|| format!("no data source `{source}`"))?;
        let fresh = !self.redis.browsers.contains_key(source);
        let b = self.redis.browsers.entry(source.to_string()).or_insert_with(|| RedisBrowser {
            source: source.to_string(),
            db: default_db,
            databases: 16,
            pattern: "*".into(),
            ..Default::default()
        });
        let changed_db = db.is_some_and(|d| d != b.db);
        if let Some(d) = db {
            b.db = d;
        }
        self.redis.active = Some(source.to_string());
        if fresh || changed_db || b.keys.is_empty() && !b.scanning {
            self.redis_scan(source, None, None, true)?;
        } else {
            self.redis_send(source, Req::Keyspace)?;
        }
        Ok(())
    }

    /// Start (or continue, with `restart` false) the scan.
    pub(crate) fn redis_scan(
        &mut self,
        source: &str,
        pattern: Option<String>,
        kind: Option<Option<String>>,
        restart: bool,
    ) -> Result<(), String> {
        let b = self.redis.browsers.get_mut(source).ok_or("the key browser is not open")?;
        if let Some(p) = pattern {
            b.pattern = if p.trim().is_empty() { "*".into() } else { p };
        }
        if let Some(k) = kind {
            b.type_filter = k.filter(|k| !k.is_empty() && k != "all");
        }
        if restart {
            b.revision += 1;
            b.keys.clear();
            b.cursor = Some("0".into());
            b.selected = None;
            b.value = None;
        }
        let Some(cursor) = b.cursor.clone() else { return Err("every key was found already".into()) };
        if b.keys.len() >= MAX_KEYS {
            return Err(format!("the browser keeps {MAX_KEYS} keys; narrow the pattern"));
        }
        b.seq += 1;
        b.scanning = true;
        b.error = None;
        let req = Req::Scan {
            seq: b.seq,
            db: b.db,
            pattern: b.pattern.clone(),
            kind: b.type_filter.clone(),
            cursor,
            want: PAGE_KEYS,
        };
        self.redis_send(source, req)
    }

    /// Show a key's value (`more`: its next page).
    pub(crate) fn redis_load(&mut self, source: &str, key: Vec<u8>, more: bool) -> Result<(), String> {
        let b = self.redis.browsers.get_mut(source).ok_or("the key browser is not open")?;
        let (kind, offset) = match (&b.value, more) {
            (Some(v), true) if v.key == key => {
                (Some(v.kind.clone()), v.next.clone().ok_or("the whole value is loaded")?)
            }
            _ => (None, String::new()),
        };
        if !more {
            b.selected = Some(key.clone());
            b.value = None;
        }
        b.value_seq += 1;
        b.loading_value = true;
        b.value_error = None;
        let req = Req::Load { seq: b.value_seq, db: b.db, key, kind, offset };
        self.redis_send(source, req)
    }

    /// Apply a change (the caller checked read-only and the production guard).
    pub(crate) fn redis_write(&mut self, source: &str, op: WriteOp) -> Result<(), String> {
        op.commands()?;
        let b = self.redis.browsers.get_mut(source).ok_or("the key browser is not open")?;
        b.writing = true;
        b.message = None;
        let db = b.db;
        self.log(super::OutputLevel::Sql, source, op.preview());
        self.redis_send(source, Req::Write { db, op })
    }

    /// Drop the browsers' connections (the data source was disconnected or edited); the
    /// browser stays and reconnects with the current settings on its next request.
    pub(crate) fn redis_disconnect(&mut self, source: Option<&str>) {
        let all: Vec<String> = match source {
            Some(s) => vec![s.to_string()],
            None => self.redis.browsers.keys().cloned().collect(),
        };
        for s in all {
            self.redis.workers.remove(&s);
            self.tunnels.close(Some(&tunnel_key(&s)));
            if let Some(b) = self.redis.browsers.get_mut(&s) {
                b.version = None;
                b.scanning = false;
                b.loading_value = false;
                b.writing = false;
                // Answers of the old connection are ignored.
                b.seq += 1;
                b.value_seq += 1;
            }
        }
    }

    /// Close a browser (and its connection and tunnel).
    pub(crate) fn redis_close(&mut self, source: Option<&str>) {
        let all: Vec<String> = match source {
            Some(s) => vec![s.to_string()],
            None => self.redis.browsers.keys().cloned().collect(),
        };
        for s in all {
            self.redis.workers.remove(&s);
            self.redis.browsers.remove(&s);
            self.tunnels.close(Some(&tunnel_key(&s)));
            if self.redis.active.as_deref() == Some(s.as_str()) {
                self.redis.active = None;
            }
        }
    }
}

/// Drain the browsers' worker events.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    for _ in 0..256 {
        let Ok((source, wid, ev)) = s.db.redis.rx.try_recv() else { break };
        // A dropped connection's late answers are ignored.
        if s.db.redis.workers.get(&source).is_none_or(|(w, _)| *w != wid) {
            continue;
        }
        changed = true;
        handle(&mut s.db, &source, ev);
    }
    changed
}

fn handle(db: &mut DbState, source: &str, ev: Ev) {
    let mut reload: Option<Vec<u8>> = None;
    {
        let Some(b) = db.redis.browsers.get_mut(source) else { return };
        match ev {
            Ev::Connected { version, databases } => {
                b.version = Some(version);
                b.databases = databases;
            }
            Ev::ConnectFailed(e) => {
                b.error = Some(e);
                b.scanning = false;
                b.loading_value = false;
                b.writing = false;
                db.redis.workers.remove(source);
            }
            Ev::Keyspace(ks) => b.keyspace = ks.into_iter().collect(),
            Ev::Scanned { seq, keys, cursor } if seq == b.seq => {
                b.scanning = false;
                b.revision += 1;
                let have: BTreeSet<Vec<u8>> = b.keys.iter().map(|k| k.key.clone()).collect();
                for (key, kind, ttl_ms) in keys {
                    if b.keys.len() >= MAX_KEYS {
                        break;
                    }
                    if !have.contains(&key) {
                        b.keys.push(KeyRow { name: display_key(&key), key, kind, ttl_ms });
                    }
                }
                b.cursor = (cursor != "0").then_some(cursor);
            }
            Ev::ScanFailed { seq, error } if seq == b.seq => {
                b.scanning = false;
                b.error = Some(error);
            }
            Ev::Scanned { .. } | Ev::ScanFailed { .. } => {}
            Ev::Loaded { seq, key, info, page, offset } if seq == b.value_seq => {
                b.loading_value = false;
                match info {
                    Some(info) => {
                        let mut v = ValueView::new(key.clone(), &info);
                        v.add(page, offset);
                        if let Some(row) = b.keys.iter_mut().find(|k| k.key == key) {
                            row.kind = info.kind.clone();
                            row.ttl_ms = info.ttl_ms;
                        }
                        b.value = Some(v);
                    }
                    None => {
                        if let Some(v) = b.value.as_mut().filter(|v| v.key == key) {
                            v.add(page, offset);
                        }
                    }
                }
            }
            Ev::LoadFailed { seq, error } if seq == b.value_seq => {
                b.loading_value = false;
                b.value_error = Some(error);
            }
            Ev::Loaded { .. } | Ev::LoadFailed { .. } => {}
            Ev::Wrote { op, result } => {
                b.writing = false;
                match result {
                    Ok(msg) => {
                        b.message = Some(msg);
                        b.revision += 1;
                        match &op {
                            WriteOp::Delete { keys } => {
                                b.keys.retain(|k| !keys.contains(&k.key));
                                if b.selected.as_ref().is_some_and(|s| keys.contains(s)) {
                                    b.selected = None;
                                    b.value = None;
                                }
                            }
                            WriteOp::Rename { key, to } => {
                                if let Some(row) = b.keys.iter_mut().find(|k| &k.key == key) {
                                    row.key = to.clone();
                                    row.name = display_key(to);
                                }
                                if b.selected.as_ref() == Some(key) {
                                    reload = Some(to.clone());
                                }
                            }
                            WriteOp::Create { key, kind, .. } => {
                                if b.key_row(key).is_none() {
                                    let kind = if kind == "json" { "ReJSON-RL".to_string() } else { kind.clone() };
                                    b.keys.push(KeyRow { name: display_key(key), key: key.clone(), kind, ttl_ms: -1 });
                                }
                                reload = Some(key.clone());
                            }
                            other => {
                                if b.selected.as_deref() == Some(other.key()) {
                                    reload = Some(other.key().to_vec());
                                }
                            }
                        }
                    }
                    Err(e) => b.message = Some(format!("Failed: {e}")),
                }
            }
        }
    }
    if let Some(key) = reload {
        let _ = db.redis_load(source, key, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> KeyRow {
        KeyRow { key: name.as_bytes().to_vec(), name: name.into(), kind: "string".into(), ttl_ms: -1 }
    }

    #[test]
    fn keys_group_into_namespaces() {
        let keys: Vec<KeyRow> =
            ["user:2:name", "user:1:name", "user:1:mail", "plain", "a:b", "user:3"].map(row).to_vec();
        let collapsed = namespace_rows(&keys, &BTreeSet::new(), ':');
        let labels: Vec<(usize, &str, usize)> =
            collapsed.iter().map(|r| (r.depth, r.label.as_str(), r.count)).collect();
        assert_eq!(labels, [(0, "a", 1), (0, "user", 4), (0, "plain", 0)]);
        let open: BTreeSet<String> = ["user:".to_string(), "user:1:".to_string()].into();
        let rows = namespace_rows(&keys, &open, ':');
        let labels: Vec<(usize, &str)> = rows.iter().map(|r| (r.depth, r.label.as_str())).collect();
        assert_eq!(
            labels,
            [(0, "a"), (0, "user"), (1, "1"), (2, "mail"), (2, "name"), (1, "2"), (1, "3"), (0, "plain")]
        );
        assert_eq!(rows[3].key, Some(2));
        // Hostile names: empty parts, only separators, very deep.
        let deep = ":".repeat(500);
        let odd: Vec<KeyRow> = ["", ":", "::x", deep.as_str()].map(row).to_vec();
        let _ = namespace_rows(&odd, &BTreeSet::new(), ':');
    }

    #[test]
    fn edits_send_exact_commands() {
        let k = b"k".to_vec();
        let text = |op: WriteOp| op.preview();
        assert_eq!(text(WriteOp::SetString { key: k.clone(), value: b"a b".to_vec() }), "SET k \"a b\" KEEPTTL");
        assert_eq!(
            text(WriteOp::ListRemove { key: k.clone(), index: 3 }),
            "LSET k 3 __codecraft_removed_item__\nLREM k 1 __codecraft_removed_item__"
        );
        assert_eq!(text(WriteOp::Expire { key: k.clone(), ttl_ms: Some(60_000) }), "PEXPIRE k 60000");
        assert_eq!(text(WriteOp::Expire { key: k.clone(), ttl_ms: None }), "PERSIST k");
        assert!(WriteOp::Expire { key: k.clone(), ttl_ms: Some(0) }.commands().is_err());
        assert!(WriteOp::ZSetAdd { key: k.clone(), member: b"m".to_vec(), score: "x".into() }.commands().is_err());
        assert_eq!(
            text(WriteOp::ZSetAdd { key: k.clone(), member: b"m".to_vec(), score: " 2.5 ".into() }),
            "ZADD k 2.5 m"
        );
        assert!(WriteOp::JsonSet { key: k.clone(), value: "{".into() }.commands().is_err());
        assert!(WriteOp::Delete { keys: vec![] }.commands().is_err());
        assert_eq!(text(WriteOp::Delete { keys: vec![k.clone(), vec![0xff]] }), "UNLINK k \"\\xff\"");
        assert_eq!(text(WriteOp::Rename { key: k.clone(), to: b"k2".to_vec() }), "RENAMENX k k2");
        assert!(
            WriteOp::Create { key: vec![], kind: "string".into(), field: vec![], value: vec![], score: String::new() }
                .commands()
                .is_err()
        );
        assert!(
            WriteOp::Create {
                key: k.clone(),
                kind: "bitmap".into(),
                field: vec![],
                value: vec![],
                score: String::new()
            }
            .commands()
            .is_err()
        );
        assert_eq!(
            text(WriteOp::Create {
                key: k.clone(),
                kind: "hash".into(),
                field: b"f".to_vec(),
                value: b"v".to_vec(),
                score: String::new()
            }),
            "HSETNX k f v"
        );
        assert_eq!(ttl_text(-1), "no expiry");
        assert_eq!(ttl_text(3_723_000), "1h 2m 3s");
        assert_eq!(ttl_text(500), "500ms");
        assert_eq!(ttl_text(90_000_000), "1d 1h");
    }

    #[test]
    fn value_pages_become_tables() {
        let info = KeyInfo { kind: "list".into(), ttl_ms: -1, length: Some(3), memory: None };
        let mut v = ValueView::new(b"l".to_vec(), &info);
        v.add(
            ValuePage {
                value: KeyValue::List(vec![b"a".to_vec(), b"b".to_vec()]),
                next: Some("2".into()),
                truncated: false,
            },
            0,
        );
        v.add(ValuePage { value: KeyValue::List(vec![b"c".to_vec()]), next: None, truncated: false }, 2);
        assert_eq!(v.rows.iter().map(|r| r.cells.clone()).collect::<Vec<_>>(), [["0", "a"], ["1", "b"], ["2", "c"]]);
        assert_eq!(v.next, None);
        let mut s = ValueView::new(b"x".to_vec(), &KeyInfo { kind: "stream".into(), ..Default::default() });
        use codecraft_db::redis::StreamEntry;
        s.add(
            ValuePage {
                value: KeyValue::Stream(vec![
                    StreamEntry { id: "1-0".into(), fields: vec![(b"a".to_vec(), b"1".to_vec())] },
                    StreamEntry { id: "2-0".into(), fields: vec![(b"b".to_vec(), b"2".to_vec())] },
                ]),
                next: None,
                truncated: false,
            },
            0,
        );
        assert_eq!(s.columns, ["id", "a", "b"]);
        assert_eq!(s.rows[0].cells, ["1-0", "1", ""]);
        assert_eq!(s.rows[1].cells, ["2-0", "", "2"]);
        let mut b = ValueView::new(b"s".to_vec(), &KeyInfo { kind: "string".into(), ..Default::default() });
        b.add(ValuePage { value: KeyValue::String(vec![0xff, 0x00]), next: None, truncated: false }, 0);
        assert!(b.binary);
        assert_eq!(b.text.as_deref(), Some("0xFF00"));
    }
}
