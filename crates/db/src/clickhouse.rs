//! ClickHouse over its HTTP interface (port 8123, or 8443 with `ssl=true`), through `ureq`.
//!
//! Every statement is a POST whose body is the SQL; results come back as
//! `JSONCompactEachRowWithNamesAndTypes` (one JSON array per line: names, types, then rows) and
//! are read line by line, so pages reach the grid while the query still streams. Each connection
//! has an HTTP `session_id` (so `SET` and temporary tables last) and every query a `query_id`:
//! Cancel sends `KILL QUERY WHERE query_id = …` on a separate request. `USE db` is remembered
//! and sent as the `database` parameter. ClickHouse has no transactions.
//!
//! Options: `ssl`/`https` (`true`), `tls_insecure`, `max_execution_time` (seconds) and any
//! `setting.<name>` (sent as a query setting).

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde_json::Value;

use crate::{
    Canceller, Cell, Column, ColumnInfo, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult,
    ExecSummary, IndexInfo, Pager, ResultSink, TableDetails, TableInfo, TableKind,
};

/// Longest result line accepted (one row).
const MAX_LINE: usize = 64 * 1024 * 1024;
/// Error bodies kept.
const MAX_ERROR: u64 = 64 * 1024;
/// QUERY_WAS_CANCELLED.
const CANCELLED_CODE: &str = "394";

static QUERY_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_id(prefix: &str) -> String {
    let n = QUERY_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("codecraft-{prefix}-{}-{n}-{nanos:08x}", std::process::id())
}

/// What every request needs.
#[derive(Clone)]
struct Endpoint {
    agent: ureq::Agent,
    url: String,
    user: String,
    password: String,
}

impl Endpoint {
    fn post(&self, params: &[(&str, &str)]) -> ureq::Request {
        let mut r = self.agent.post(&self.url);
        if !self.user.is_empty() {
            r = r.set("X-ClickHouse-User", &self.user);
        }
        if !self.password.is_empty() {
            r = r.set("X-ClickHouse-Key", &self.password);
        }
        for (k, v) in params {
            r = r.query(k, v);
        }
        r
    }

    /// Run a statement that returns nothing of interest (no session).
    fn command(&self, sql: &str) -> DbResult<()> {
        match self.post(&[]).send_string(sql) {
            Ok(r) => {
                let _ = r.into_reader().take(MAX_ERROR).read_to_end(&mut Vec::new());
                Ok(())
            }
            Err(e) => Err(http_error(e, false)),
        }
    }
}

/// A ureq error as the user should see it.
fn http_error(e: ureq::Error, cancelled: bool) -> DbError {
    match e {
        ureq::Error::Status(status, resp) => {
            let code = resp.header("X-ClickHouse-Exception-Code").unwrap_or("").to_string();
            let mut body = String::new();
            let _ = resp.into_reader().take(MAX_ERROR).read_to_string(&mut body);
            if cancelled || code == CANCELLED_CODE {
                return DbError::Cancelled;
            }
            let body = body.trim();
            if status == 401 || status == 403 || code == "516" {
                return DbError::Connect(format!("authentication failed: {}", first_line(body)));
            }
            DbError::Query(if body.is_empty() { format!("HTTP {status}") } else { clean_exception(body) })
        }
        ureq::Error::Transport(t) => {
            if cancelled {
                DbError::Cancelled
            } else {
                DbError::Connect(format!("cannot reach ClickHouse: {t}"))
            }
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// `Code: 62. DB::Exception: Syntax error: … (SYNTAX_ERROR) (version 24.3.1.1)` without the
/// version noise.
pub fn clean_exception(body: &str) -> String {
    let s = body.trim();
    let s = match s.rfind(" (version ") {
        Some(i) if s.ends_with(')') => s.get(..i).unwrap_or(s),
        _ => s,
    };
    s.chars().take(4000).collect()
}

/// A string literal for ClickHouse (`'…'` with backslash escapes).
pub fn literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Cancel: KILL QUERY on a separate request.
struct ChCancel {
    endpoint: Endpoint,
    current: Mutex<Option<String>>,
    cancelled: AtomicBool,
}

impl ChCancel {
    fn kill(&self, id: &str) -> DbResult<()> {
        self.endpoint.command(&format!("KILL QUERY WHERE query_id = {} ASYNC", literal(id)))
    }
}

impl Canceller for ChCancel {
    fn cancel(&self) -> DbResult<()> {
        let id = self.current.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(id) = id {
            self.cancelled.store(true, Ordering::SeqCst);
            self.kill(&id).map_err(|e| DbError::Query(format!("cancel failed: {e}")))?;
        }
        Ok(())
    }
}

pub(crate) struct ChConn {
    endpoint: Endpoint,
    session: String,
    database: String,
    settings: Vec<(String, String)>,
    cancel: Arc<ChCancel>,
}

/// One line of the result: a JSON array.
fn read_row<R: BufRead>(r: &mut R) -> DbResult<Option<Vec<Value>>> {
    match read_text_line(r)? {
        None => Ok(None),
        Some(text) => parse_row(&text).map(Some),
    }
}

/// One line of the body, trimmed (`None` at the end).
fn read_text_line<R: BufRead>(r: &mut R) -> DbResult<Option<String>> {
    let mut line = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|e| DbError::Query(format!("reading the result failed: {e}")))?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE {
        return Err(DbError::Query("a result row is larger than 64 MB".into()));
    }
    Ok(Some(String::from_utf8_lossy(&line).trim().to_string()))
}

/// A line as a JSON row (empty for a blank line); text that is not JSON is an exception or an
/// error.
fn parse_row(text: &str) -> DbResult<Vec<Value>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Vec<Value>>(text) {
        Ok(v) => Ok(v),
        // An error after the HTTP status was sent arrives as text in the body.
        Err(_) if text.contains("Exception") || text.starts_with("Code:") => Err(DbError::Query(clean_exception(text))),
        Err(e) => Err(DbError::Query(format!(
            "unexpected ClickHouse output ({e}): {}",
            text.chars().take(200).collect::<String>()
        ))),
    }
}

fn is_int_type(t: &str) -> Option<bool> {
    let base = strip_wrappers(t);
    if base.starts_with("UInt") {
        Some(true)
    } else if base.starts_with("Int") {
        Some(false)
    } else {
        None
    }
}

/// `Nullable(LowCardinality(String))` → `String`.
fn strip_wrappers(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner =
            ["Nullable(", "LowCardinality("].iter().find_map(|p| t.strip_prefix(p).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

/// A JSON value of a column of type `ty` as a cell. 64-bit integers arrive quoted.
pub fn cell(v: &Value, ty: &str) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Cell::Int(i),
            (_, Some(u), _) => Cell::UInt(u),
            (_, _, Some(f)) => Cell::Float(f),
            _ => Cell::Other(n.to_string()),
        },
        Value::String(s) => match is_int_type(ty) {
            Some(_) if strip_wrappers(ty).ends_with("128") || strip_wrappers(ty).ends_with("256") => {
                Cell::Other(s.clone())
            }
            Some(true) => s.parse::<u64>().map(Cell::UInt).unwrap_or_else(|_| Cell::Other(s.clone())),
            Some(false) => s.parse::<i64>().map(Cell::Int).unwrap_or_else(|_| Cell::Other(s.clone())),
            None if strip_wrappers(ty).starts_with("Float") => match s.as_str() {
                "nan" | "inf" | "-inf" | "+inf" => Cell::Other(s.clone()),
                _ => s.parse::<f64>().map(Cell::Float).unwrap_or_else(|_| Cell::Other(s.clone())),
            },
            None if strip_wrappers(ty).starts_with("Decimal")
                || strip_wrappers(ty).starts_with("Date")
                || strip_wrappers(ty) == "UUID"
                || strip_wrappers(ty).starts_with("IPv")
                || strip_wrappers(ty).starts_with("Enum") =>
            {
                Cell::Other(s.clone())
            }
            None => Cell::Text(s.clone()),
        },
        other => Cell::Other(other.to_string()),
    }
}

/// Whether a statement is `USE db`; returns the database.
fn use_database(sql: &str) -> Option<String> {
    let t = sql.trim().trim_end_matches(';').trim();
    let (first, rest) = t.split_once(char::is_whitespace)?;
    if !first.eq_ignore_ascii_case("USE") {
        return None;
    }
    let name = rest.trim();
    let unq = name
        .strip_prefix('`')
        .and_then(|n| n.strip_suffix('`'))
        .or_else(|| name.strip_prefix('"').and_then(|n| n.strip_suffix('"')))
        .unwrap_or(name);
    (!unq.is_empty()).then(|| unq.to_string())
}

impl ChConn {
    pub(crate) fn open(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Self> {
        let on = |k: &str| {
            cfg.options
                .get(k)
                .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        };
        let tls = on("ssl") || on("https") || on("secure");
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(cfg.connect_timeout())
            .user_agent("CodeCraft")
            .max_idle_connections_per_host(4);
        if tls {
            let mut b = ureq::native_tls::TlsConnector::builder();
            if on("tls_insecure") {
                b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
            }
            let files = cfg.tls_files(None)?;
            if let Some(root) = &files.root_cert {
                let pem = std::fs::read(root).map_err(|e| DbError::Config(format!("{}: {e}", root.display())))?;
                let cert = ureq::native_tls::Certificate::from_pem(&pem)
                    .map_err(|e| DbError::Config(format!("{}: {e}", root.display())))?;
                b.add_root_certificate(cert);
            }
            let connector = b.build().map_err(|e| DbError::Connect(format!("TLS: {e}")))?;
            builder = builder.tls_connector(Arc::new(connector));
        }
        let port = cfg.port.unwrap_or(if tls { 8443 } else { 8123 });
        // Through an SSH tunnel the URL keeps the server's name (TLS checks it) and resolves to
        // the tunnel's local end.
        let tunnel_name = cfg
            .options
            .get("tls_server_name")
            .map(|s| s.trim().to_string())
            .filter(|n| tls && !n.is_empty() && n != cfg.host.trim());
        if tunnel_name.is_some() {
            let target = format!("{}:{port}", cfg.host.trim());
            builder = builder.resolver(move |_: &str| {
                use std::net::ToSocketAddrs;
                target.to_socket_addrs().map(|a| a.collect::<Vec<_>>())
            });
        }
        let host = tunnel_name.unwrap_or_else(|| cfg.host.trim().to_string());
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
        let scheme = if tls { "https" } else { "http" };
        let endpoint = Endpoint {
            agent: builder.build(),
            url: format!("{scheme}://{host}:{port}/"),
            user: if cfg.user.trim().is_empty() { "default".into() } else { cfg.user.trim().into() },
            password: password.unwrap_or_default().to_string(),
        };
        let mut settings: Vec<(String, String)> = Vec::new();
        if let Some(t) = cfg.options.get("max_execution_time").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let secs: u64 =
                t.parse().map_err(|_| DbError::Config(format!("max_execution_time `{t}` is not a number")))?;
            settings.push(("max_execution_time".into(), secs.to_string()));
        }
        for (k, v) in &cfg.options {
            if let Some(name) = k.strip_prefix("setting.")
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                settings.push((name.to_string(), v.trim().to_string()));
            }
        }
        if cfg.read_only {
            // 2: reads and SET only. The server refuses writes too.
            settings.push(("readonly".into(), "2".into()));
        }
        let cancel = Arc::new(ChCancel {
            endpoint: endpoint.clone(),
            current: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        });
        let mut conn =
            ChConn { endpoint, session: new_id("s"), database: cfg.database.trim().to_string(), settings, cancel };
        match conn.strings("SELECT 1") {
            Ok(_) => {}
            // The user's profile is read-only already: it cannot be set again.
            Err(DbError::Query(e)) if cfg.read_only && e.contains("readonly") => {
                conn.settings.retain(|(k, _)| k != "readonly");
                conn.strings("SELECT 1").map_err(|e| DbError::Connect(e.to_string()))?;
            }
            Err(DbError::Connect(e)) | Err(DbError::Query(e)) => return Err(DbError::Connect(e)),
            Err(e) => return Err(e),
        }
        Ok(conn)
    }

    /// Send a statement; the response streams.
    fn send(&self, sql: &str, query_id: &str) -> DbResult<Box<dyn Read + Send + Sync + 'static>> {
        let mut params: Vec<(&str, &str)> = vec![
            ("session_id", &self.session),
            ("session_timeout", "600"),
            ("query_id", query_id),
            ("default_format", "JSONCompactEachRowWithNamesAndTypes"),
            ("output_format_json_quote_64bit_integers", "1"),
            ("output_format_json_quote_denormals", "1"),
        ];
        if !self.database.is_empty() {
            params.push(("database", &self.database));
        }
        for (k, v) in &self.settings {
            params.push((k, v));
        }
        let cancelled = || self.cancel.cancelled.load(Ordering::SeqCst);
        match self.endpoint.post(&params).send_string(sql) {
            Ok(resp) => Ok(resp.into_reader()),
            Err(e) => Err(http_error(e, cancelled())),
        }
    }

    /// Every row of an introspection query, as text.
    fn strings(&mut self, sql: &str) -> DbResult<Vec<Vec<Option<String>>>> {
        let id = new_id("q");
        let mut r = BufReader::new(self.send(sql, &id)?);
        let mut types: Vec<String> = Vec::new();
        let mut out = Vec::new();
        let mut header = 0;
        while let Some(row) = read_row(&mut r)? {
            if row.is_empty() {
                continue;
            }
            if header < 2 {
                if header == 1 {
                    types = row.iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect();
                }
                header += 1;
                continue;
            }
            out.push(
                row.iter()
                    .enumerate()
                    .map(|(i, v)| match cell(v, types.get(i).map(String::as_str).unwrap_or("")) {
                        Cell::Null => None,
                        c => Some(c.display()),
                    })
                    .collect(),
            );
            if out.len() > 1_000_000 {
                break;
            }
        }
        Ok(out)
    }
}

fn col(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

impl Connection for ChConn {
    fn kind(&self) -> DbKind {
        DbKind::ClickHouse
    }

    fn server_version(&mut self) -> DbResult<String> {
        let rows = self.strings("SELECT version()")?;
        Ok(format!("ClickHouse {}", rows.first().map(|r| col(r, 0)).unwrap_or_default()))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.cancel.clone()
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        let started = Instant::now();
        let id = new_id("q");
        self.cancel.cancelled.store(false, Ordering::SeqCst);
        *self.cancel.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.clone());
        let result = self.run(sql, &id, page_size, sink, started);
        *self.cancel.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let cancelled = self.cancel.cancelled.swap(false, Ordering::SeqCst);
        match result {
            Err(_) if cancelled => Err(DbError::Cancelled),
            Ok(summary) => {
                if let Some(db) = use_database(sql) {
                    self.database = db;
                }
                Ok(summary)
            }
            Err(e) => Err(e),
        }
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let current = self.strings("SELECT currentDatabase()")?.first().map(|r| col(r, 0));
        let schemas = self
            .strings(
                "SELECT name FROM system.databases WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', \
                 'information_schema') ORDER BY name",
            )?
            .iter()
            .map(|r| col(r, 0))
            .collect();
        Ok(DatabaseInfo { databases: Vec::new(), current: current.clone(), schemas, default_schema: current })
    }

    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>> {
        Ok(self
            .strings(&format!(
                "SELECT name, engine FROM system.tables WHERE database = {} AND NOT is_temporary ORDER BY name",
                literal(schema)
            ))?
            .iter()
            .map(|r| TableInfo {
                name: col(r, 0),
                kind: match col(r, 1).as_str() {
                    "View" | "LiveView" | "WindowView" => TableKind::View,
                    "MaterializedView" => TableKind::MaterializedView,
                    "Distributed" | "MySQL" | "PostgreSQL" | "URL" | "S3" | "Kafka" | "Dictionary" => {
                        TableKind::Foreign
                    }
                    _ => TableKind::Table,
                },
            })
            .collect())
    }

    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails> {
        let (s, t) = (literal(schema), literal(table));
        let columns: Vec<ColumnInfo> = self
            .strings(&format!(
                "SELECT name, type, default_kind, default_expression, is_in_primary_key FROM system.columns \
                 WHERE database = {s} AND table = {t} ORDER BY position"
            ))?
            .iter()
            .map(|r| ColumnInfo {
                name: col(r, 0),
                nullable: col(r, 1).starts_with("Nullable("),
                data_type: col(r, 1),
                default: match (col(r, 2), col(r, 3)) {
                    (k, e) if !e.is_empty() && k != "DEFAULT" && !k.is_empty() => Some(format!("{k} {e}")),
                    (_, e) if !e.is_empty() => Some(e),
                    _ => None,
                },
                primary_key: col(r, 4) == "1",
            })
            .collect();
        let mut indexes = Vec::new();
        let key = self.strings(&format!(
            "SELECT primary_key, sorting_key FROM system.tables WHERE database = {s} AND name = {t}"
        ))?;
        if let Some(r) = key.first() {
            let pk = col(r, 0);
            let sk = col(r, 1);
            let split =
                |k: &str| k.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect::<Vec<_>>();
            if !pk.is_empty() {
                indexes.push(IndexInfo {
                    name: "PRIMARY KEY".into(),
                    columns: split(&pk),
                    unique: false,
                    primary: true,
                });
            }
            if !sk.is_empty() && sk != pk {
                indexes.push(IndexInfo { name: "ORDER BY".into(), columns: split(&sk), unique: false, primary: false });
            }
        }
        // Data skipping indexes (older servers lack the table: then there are none to show).
        if let Ok(rows) = self.strings(&format!(
            "SELECT name, expr, type FROM system.data_skipping_indices WHERE database = {s} AND table = {t} ORDER BY name"
        )) {
            for r in rows {
                indexes.push(IndexInfo {
                    name: format!("{} ({})", col(&r, 0), col(&r, 2)),
                    columns: vec![col(&r, 1)],
                    unique: false,
                    primary: false,
                });
            }
        }
        Ok(TableDetails { columns, indexes, foreign_keys: Vec::new() })
    }

    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64> {
        let sql = format!("SELECT count() FROM {}", crate::sql::qualified(DbKind::ClickHouse, schema, table));
        let rows = self.strings(&sql)?;
        Ok(rows.first().map(|r| col(r, 0)).and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    fn transaction_status(&mut self) -> crate::tx::TxStatus {
        crate::tx::TxStatus::Idle
    }
}

impl ChConn {
    fn run(
        &mut self,
        sql: &str,
        id: &str,
        page_size: usize,
        sink: &mut dyn ResultSink,
        started: Instant,
    ) -> DbResult<ExecSummary> {
        let body = self.send(sql, id)?;
        let mut r = BufReader::with_capacity(256 * 1024, body);
        let names = loop {
            match read_text_line(&mut r)? {
                None => {
                    // No result set (DDL, INSERT, SET …).
                    sink.columns(&[]);
                    let pager = Pager::new(sink, page_size, started);
                    return Ok(pager.finish(Vec::new(), Some(0)));
                }
                Some(line) if line.is_empty() => continue,
                Some(line) => match parse_row(&line) {
                    Ok(row) => break row,
                    Err(e) if line.contains("Exception") || line.starts_with("Code:") => return Err(e),
                    // The statement chose its own FORMAT (Pretty, TSV …): show its lines.
                    Err(_) => return text_result(line, &mut r, page_size, sink, started),
                },
            }
        };
        let types = read_row(&mut r)?.unwrap_or_default();
        let columns: Vec<Column> = names
            .iter()
            .enumerate()
            .map(|(i, n)| Column {
                name: n.as_str().map(str::to_string).unwrap_or_else(|| n.to_string()),
                type_name: types.get(i).and_then(Value::as_str).unwrap_or_default().to_string(),
            })
            .collect();
        sink.columns(&columns);
        let mut pager = Pager::new(sink, page_size, started);
        let mut stopped = false;
        while let Some(row) = read_row(&mut r)? {
            if row.is_empty() {
                continue;
            }
            let cells = columns
                .iter()
                .enumerate()
                .map(|(i, c)| row.get(i).map(|v| cell(v, &c.type_name)).unwrap_or(Cell::Null))
                .collect();
            if !pager.push(cells) {
                stopped = true;
                break;
            }
        }
        if stopped {
            // Dropping the response closes the socket; make sure the server stops too.
            let _ = self.cancel.kill(id);
        }
        Ok(pager.finish(columns, None))
    }
}

/// Output in a format of the user's choosing: one `output` column, a row per line.
fn text_result<R: BufRead>(
    first: String,
    r: &mut R,
    page_size: usize,
    sink: &mut dyn ResultSink,
    started: Instant,
) -> DbResult<ExecSummary> {
    let columns = vec![Column { name: "output".into(), type_name: "String".into() }];
    sink.columns(&columns);
    let mut pager = Pager::new(sink, page_size, started);
    let mut line = Some(first);
    while let Some(l) = line {
        if !pager.push(vec![Cell::Text(l)]) {
            break;
        }
        line = read_text_line(r)?;
    }
    Ok(pager.finish(columns, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_parse_and_errors_surface() {
        let body = "[\"n\",\"s\"]\n[\"UInt64\",\"Nullable(String)\"]\n[\"18446744073709551615\",null]\n[\"1\",\"x\"]\n";
        let mut r = BufReader::new(body.as_bytes());
        assert_eq!(read_row(&mut r).unwrap().unwrap().len(), 2);
        read_row(&mut r).unwrap();
        let row = read_row(&mut r).unwrap().unwrap();
        assert_eq!(cell(&row[0], "UInt64"), Cell::UInt(u64::MAX));
        assert_eq!(cell(&row[1], "Nullable(String)"), Cell::Null);
        read_row(&mut r).unwrap();
        assert_eq!(read_row(&mut r).unwrap(), None);
        let mut r = BufReader::new("Code: 241. DB::Exception: Memory limit exceeded (version 24.3.1.1)\n".as_bytes());
        match read_row(&mut r) {
            Err(DbError::Query(e)) => assert_eq!(e, "Code: 241. DB::Exception: Memory limit exceeded"),
            other => panic!("{other:?}"),
        }
        for bad in ["{", "[1,", "\u{0}", "nul", "[[[[[[", "\"str\"", "[1]\n[2", "\u{feff}[1]"] {
            let _ = read_row(&mut BufReader::new(bad.as_bytes()));
        }
    }

    #[test]
    fn values_follow_types() {
        let s = |x: &str| Value::String(x.into());
        assert_eq!(cell(&s("-5"), "Int64"), Cell::Int(-5));
        assert_eq!(cell(&s("12"), "LowCardinality(Nullable(UInt32))"), Cell::UInt(12));
        assert_eq!(
            cell(&s("170141183460469231731687303715884105727"), "Int128"),
            Cell::Other("170141183460469231731687303715884105727".into())
        );
        assert_eq!(cell(&s("1.25"), "Float64"), Cell::Float(1.25));
        assert_eq!(cell(&s("nan"), "Float64"), Cell::Other("nan".into()));
        assert_eq!(cell(&s("2024-01-01"), "Date"), Cell::Other("2024-01-01".into()));
        assert_eq!(cell(&s("abc"), "String"), Cell::Text("abc".into()));
        assert_eq!(cell(&serde_json::json!(3), "UInt8"), Cell::Int(3));
        assert_eq!(cell(&serde_json::json!([1, 2]), "Array(UInt8)"), Cell::Other("[1,2]".into()));
        assert_eq!(cell(&serde_json::json!(true), "Bool"), Cell::Bool(true));
        assert_eq!(cell(&s("x"), "Int32"), Cell::Other("x".into()));
    }

    #[test]
    fn literals_and_use() {
        assert_eq!(literal("it's \\ ok"), "'it\\'s \\\\ ok'");
        assert_eq!(use_database("use `my db`;"), Some("my db".into()));
        assert_eq!(use_database("USE analytics"), Some("analytics".into()));
        assert_eq!(use_database("SELECT 1"), None);
        assert_eq!(
            clean_exception("Code: 60. DB::Exception: Table x doesn't exist. (UNKNOWN_TABLE) (version 23.8)"),
            "Code: 60. DB::Exception: Table x doesn't exist. (UNKNOWN_TABLE)"
        );
    }
}
