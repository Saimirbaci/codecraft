//! CodeCraft Database drivers, introspection and query execution (SQLite, PostgreSQL, MySQL) (L0, no workspace deps).
//!
//! Everything here is synchronous and meant to run on worker threads: [`connect`] opens a
//! [`Connection`]; its [`Connection::execute`] streams result pages to a sink so the caller can
//! fetch 500 rows at a time and stop early; a [`Canceller`] obtained before running a query can
//! cancel it from another thread. [`split`] has the statement splitter, [`secrets`] the password
//! store (macOS Keychain), [`sql`] identifier quoting and query builders.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod config;
mod my;
mod pg;
pub mod secrets;
pub mod split;
pub mod sql;
mod sqlite;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use config::{DataSourceConfig, DbKind, SslMode};
pub use secrets::{KeychainStore, MemoryStore, SecretStore};
pub use split::{Dialect, Statement, split_statements, statement_at};

/// Rows per page when fetching results.
pub const PAGE_SIZE: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbError {
    #[error("{0}")]
    Connect(String),
    #[error("{0}")]
    Query(String),
    #[error("query cancelled")]
    Cancelled,
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("secret store: {0}")]
    Secret(String),
    #[error("{0}")]
    Io(String),
}

pub type DbResult<T> = Result<T, DbError>;

/// One value of a result row. NULL is distinct from an empty string.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    /// Values with no natural Rust type (decimals, dates, UUIDs, arrays), already formatted.
    Other(String),
}

impl Cell {
    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }

    /// The value as shown in a grid (`NULL` for null, hex for short binary values).
    pub fn display(&self) -> String {
        match self {
            Cell::Null => "NULL".into(),
            Cell::Bool(b) => b.to_string(),
            Cell::Int(i) => i.to_string(),
            Cell::UInt(u) => u.to_string(),
            Cell::Float(f) => f.to_string(),
            Cell::Text(s) | Cell::Other(s) => s.clone(),
            Cell::Bytes(b) => hex_preview(b, 64),
        }
    }

    /// The value for export or copying: like [`Cell::display`] but NULL is the empty string.
    pub fn to_plain(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Bytes(b) => hex_preview(b, usize::MAX),
            other => other.display(),
        }
    }

    /// Numbers are right-aligned in grids.
    pub fn is_numeric(&self) -> bool {
        matches!(self, Cell::Int(_) | Cell::UInt(_) | Cell::Float(_))
    }
}

fn hex_preview(b: &[u8], max: usize) -> String {
    let mut s = String::from("0x");
    for byte in b.iter().take(max) {
        s.push_str(&format!("{byte:02X}"));
    }
    if b.len() > max {
        s.push('…');
    }
    s
}

impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Cell::Null => ser.serialize_none(),
            Cell::Bool(b) => ser.serialize_bool(*b),
            Cell::Int(i) => ser.serialize_i64(*i),
            Cell::UInt(u) => ser.serialize_u64(*u),
            Cell::Float(f) if f.is_finite() => ser.serialize_f64(*f),
            other => ser.serialize_str(&other.to_plain()),
        }
    }
}

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(de)?;
        Ok(match v {
            serde_json::Value::Null => Cell::Null,
            serde_json::Value::Bool(b) => Cell::Bool(b),
            serde_json::Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                (Some(i), _, _) => Cell::Int(i),
                (_, Some(u), _) => Cell::UInt(u),
                (_, _, Some(f)) => Cell::Float(f),
                _ => Cell::Other(n.to_string()),
            },
            serde_json::Value::String(s) => Cell::Text(s),
            other => Cell::Other(other.to_string()),
        })
    }
}

/// A result column.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// The database's type name, when known (`integer`, `varchar(20)`, …).
    pub type_name: String,
}

/// A batch of rows passed to the sink of [`Connection::execute`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub rows: Vec<Vec<Cell>>,
    /// No more rows follow.
    pub last: bool,
}

/// What a statement did.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecSummary {
    /// Result columns (empty for statements that return no rows).
    pub columns: Vec<Column>,
    /// Rows passed to the sink.
    pub rows_fetched: usize,
    /// Rows changed by INSERT/UPDATE/DELETE (`None` for queries).
    pub rows_affected: Option<u64>,
    /// Every row was fetched (false when the sink stopped early).
    pub complete: bool,
    pub elapsed_ms: u64,
}

/// Cancels the query currently running on a connection, from any thread.
pub trait Canceller: Send + Sync {
    fn cancel(&self) -> DbResult<()>;
}

/// The sink receives the columns once, then pages; returning false stops fetching.
pub trait ResultSink {
    fn columns(&mut self, columns: &[Column]);
    fn page(&mut self, page: Page) -> bool;
}

/// Kinds of schema objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    Foreign,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableInfo {
    pub name: String,
    pub kind: TableKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDetails {
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

/// The top of the schema tree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseInfo {
    /// Databases on the server (PostgreSQL); empty where databases are schemas.
    pub databases: Vec<String>,
    /// The database the connection uses (PostgreSQL) or the configured default schema.
    pub current: Option<String>,
    /// Schemas browsable through this connection.
    pub schemas: Vec<String>,
    /// Where unqualified names resolve (`public`, `main`, the MySQL database).
    pub default_schema: Option<String>,
}

/// An open database connection. Not `Sync`: one thread uses it at a time.
pub trait Connection: Send {
    fn kind(&self) -> DbKind;
    /// A short server description (`PostgreSQL 16.4`, `SQLite 3.50.1`).
    fn server_version(&mut self) -> DbResult<String>;
    /// A handle that cancels the running query from another thread.
    fn canceller(&self) -> Arc<dyn Canceller>;
    /// Run one statement, streaming rows to `sink` in pages of `page_size`.
    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary>;
    fn database_info(&mut self) -> DbResult<DatabaseInfo>;
    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>>;
    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails>;
    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64>;
}

/// Open a connection. `password` comes from the secret store (never from config files).
pub fn connect(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Box<dyn Connection>> {
    cfg.validate()?;
    match cfg.kind {
        DbKind::Sqlite => Ok(Box::new(sqlite::SqliteConn::open(cfg)?)),
        DbKind::Postgres => Ok(Box::new(pg::PgConn::open(cfg, password)?)),
        DbKind::Mysql | DbKind::Mariadb => Ok(Box::new(my::MyConn::open(cfg, password)?)),
    }
}

/// Connect, ask for the server version and disconnect (the data source dialog's Test Connection).
pub fn test_connection(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<String> {
    let mut conn = connect(cfg, password)?;
    conn.server_version()
}

/// Run every row of a statement into memory (small queries: tests, introspection helpers).
pub fn query_all(conn: &mut dyn Connection, sql: &str) -> DbResult<(Vec<Column>, Vec<Vec<Cell>>, ExecSummary)> {
    let mut sink = CollectSink::default();
    let summary = conn.execute(sql, PAGE_SIZE, &mut sink)?;
    Ok((sink.columns, sink.rows, summary))
}

/// A sink that keeps everything.
#[derive(Default)]
pub struct CollectSink {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub pages: usize,
}

impl ResultSink for CollectSink {
    fn columns(&mut self, columns: &[Column]) {
        self.columns = columns.to_vec();
    }

    fn page(&mut self, page: Page) -> bool {
        self.pages += 1;
        self.rows.extend(page.rows);
        true
    }
}

/// Buffers rows and hands full pages to a sink, one row ahead so the last page is marked `last`.
pub(crate) struct Pager<'a> {
    sink: &'a mut dyn ResultSink,
    page_size: usize,
    buf: Vec<Vec<Cell>>,
    fetched: usize,
    stopped: bool,
    started: Instant,
}

impl<'a> Pager<'a> {
    pub(crate) fn new(sink: &'a mut dyn ResultSink, page_size: usize, started: Instant) -> Self {
        Self { sink, page_size: page_size.max(1), buf: Vec::new(), fetched: 0, stopped: false, started }
    }

    /// Add a row; returns false when the sink wants no more rows.
    pub(crate) fn push(&mut self, row: Vec<Cell>) -> bool {
        if self.stopped {
            return false;
        }
        if self.buf.len() >= self.page_size {
            let rows = std::mem::take(&mut self.buf);
            self.fetched += rows.len();
            if !self.sink.page(Page { rows, last: false }) {
                self.stopped = true;
                return false;
            }
        }
        self.buf.push(row);
        true
    }

    /// Flush the final page. `complete`: the driver reached the end of the rows.
    pub(crate) fn finish(mut self, columns: Vec<Column>, rows_affected: Option<u64>) -> ExecSummary {
        let complete = !self.stopped;
        if !self.stopped {
            let rows = std::mem::take(&mut self.buf);
            self.fetched += rows.len();
            self.sink.page(Page { rows, last: true });
        }
        ExecSummary {
            columns,
            rows_fetched: self.fetched,
            rows_affected,
            complete,
            elapsed_ms: elapsed_ms(self.started),
        }
    }
}

pub(crate) fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Default connect timeout for network databases.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Write rows as RFC 4180 CSV (NULL becomes an empty field).
pub fn write_csv<W: std::io::Write>(out: &mut W, columns: &[Column], rows: &[Vec<Cell>]) -> DbResult<()> {
    let io = |e: std::io::Error| DbError::Io(e.to_string());
    let header: Vec<String> = columns.iter().map(|c| csv_field(&c.name)).collect();
    writeln!(out, "{}", header.join(",")).map_err(io)?;
    for row in rows {
        let fields: Vec<String> = row.iter().map(|c| csv_field(&c.to_plain())).collect();
        writeln!(out, "{}", fields.join(",")).map_err(io)?;
    }
    Ok(())
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stop(usize, Vec<Page>);
    impl ResultSink for Stop {
        fn columns(&mut self, _: &[Column]) {}
        fn page(&mut self, page: Page) -> bool {
            self.1.push(page);
            self.1.len() < self.0
        }
    }

    #[test]
    fn pager_marks_last_page_and_stops() {
        let mut sink = Stop(10, Vec::new());
        let mut p = Pager::new(&mut sink, 2, Instant::now());
        for i in 0..4 {
            assert!(p.push(vec![Cell::Int(i)]));
        }
        let sum = p.finish(Vec::new(), None);
        assert!(sum.complete);
        assert_eq!(sum.rows_fetched, 4);
        assert_eq!(sink.1.len(), 2);
        assert!(!sink.1[0].last && sink.1[1].last);

        let mut sink = Stop(1, Vec::new());
        let mut p = Pager::new(&mut sink, 2, Instant::now());
        let mut pushed = 0;
        for i in 0..10 {
            if !p.push(vec![Cell::Int(i)]) {
                break;
            }
            pushed += 1;
        }
        assert_eq!(pushed, 2);
        let sum = p.finish(Vec::new(), None);
        assert!(!sum.complete);
        assert_eq!(sum.rows_fetched, 2);
    }

    #[test]
    fn empty_result_sends_one_last_page() {
        let mut sink = Stop(10, Vec::new());
        let p = Pager::new(&mut sink, 500, Instant::now());
        let sum = p.finish(Vec::new(), Some(3));
        assert_eq!(sink.1.len(), 1);
        assert!(sink.1[0].last && sink.1[0].rows.is_empty());
        assert_eq!(sum.rows_affected, Some(3));
    }

    #[test]
    fn cells_display_and_serialize() {
        assert_eq!(Cell::Null.display(), "NULL");
        assert_eq!(Cell::Null.to_plain(), "");
        assert_eq!(Cell::Text(String::new()).display(), "");
        assert_eq!(Cell::Bytes(vec![1, 0xAB]).display(), "0x01AB");
        let json = serde_json::to_string(&vec![Cell::Null, Cell::Int(3), Cell::Text("a".into())]).unwrap();
        assert_eq!(json, r#"[null,3,"a"]"#);
        let back: Vec<Cell> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![Cell::Null, Cell::Int(3), Cell::Text("a".into())]);
    }

    #[test]
    fn csv_quotes_when_needed() {
        let cols = vec![Column { name: "a".into(), type_name: String::new() }, Column::default()];
        let rows =
            vec![vec![Cell::Text("x,y".into()), Cell::Null], vec![Cell::Text("say \"hi\"".into()), Cell::Int(1)]];
        let mut out = Vec::new();
        write_csv(&mut out, &cols, &rows).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a,\n\"x,y\",\n\"say \"\"hi\"\"\",1\n");
    }
}
