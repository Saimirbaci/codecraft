//! DuckDB through the `duckdb` command-line shell, file-based like SQLite.
//!
//! Why the CLI and not the `duckdb` crate: the crate compiles DuckDB's C++ sources (several
//! minutes and hundreds of MB of build output per target dir), which this workspace cannot
//! afford; the CLI is one binary the user installs (`brew install duckdb`). Each statement runs in
//! a short-lived `duckdb [-readonly] <file>` process fed on stdin with `.mode jsonlines`;
//! query columns and types come from `DESCRIBE <query>` in the same process. Consequences, stated
//! in the UI: every statement commits on its own (no transactions across statements) and session
//! state (`SET`, temporary tables, `ATTACH`) does not last from one statement to the next.
//! Cancel kills the process. A file locked by another DuckDB process is retried for a few seconds.
//!
//! Options: `duckdb_path` (the CLI; else `duckdb` on PATH or the usual install locations).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::Value;

use crate::{
    Canceller, Cell, Column, ColumnInfo, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult,
    ExecSummary, ForeignKeyInfo, IndexInfo, Pager, ResultSink, TableDetails, TableInfo, TableKind,
};

/// Separates the outputs of the statements of one script.
const MARKER: &str = "__CODECRAFT_NEXT_RESULT__";
/// Longest output line accepted (one row).
const MAX_LINE: usize = 64 * 1024 * 1024;
/// stderr kept.
const MAX_STDERR: u64 = 64 * 1024;
/// How long a locked database file is retried.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// A JSON object with its keys in output order (serde_json's map sorts them).
#[derive(Debug, Default, PartialEq)]
pub struct OrderedRow(pub Vec<(String, Value)>);

impl<'de> Deserialize<'de> for OrderedRow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OrderedRow;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<OrderedRow, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    out.push((k, v));
                }
                Ok(OrderedRow(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl OrderedRow {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn text(&self, key: &str) -> String {
        match self.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }
}

/// One output line: a row, the marker, or something else (ignored).
fn parse_line(line: &str) -> Option<Result<OrderedRow, String>> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    if !t.starts_with('{') {
        return Some(Err(t.to_string()));
    }
    Some(serde_json::from_str::<OrderedRow>(t).map_err(|e| format!("unexpected DuckDB output ({e})")))
}

/// A JSON value of a column of DuckDB type `ty` as a cell.
pub fn cell(v: &Value, ty: &str) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => {
            let t = ty.to_ascii_uppercase();
            if t.starts_with("DECIMAL") || t == "HUGEINT" || t == "UHUGEINT" {
                Cell::Other(n.to_string())
            } else {
                match (n.as_i64(), n.as_u64(), n.as_f64()) {
                    (Some(i), _, _) => Cell::Int(i),
                    (_, Some(u), _) => Cell::UInt(u),
                    (_, _, Some(f)) => Cell::Float(f),
                    _ => Cell::Other(n.to_string()),
                }
            }
        }
        Value::String(s) => {
            let t = ty.to_ascii_uppercase();
            if t.starts_with("DATE")
                || t.starts_with("TIME")
                || t.starts_with("INTERVAL")
                || t.starts_with("DECIMAL")
                || t == "UUID"
                || t.ends_with("INT")
                || t == "BLOB"
            {
                Cell::Other(s.clone())
            } else {
                Cell::Text(s.clone())
            }
        }
        other => Cell::Other(other.to_string()),
    }
}

/// Statements whose result columns `DESCRIBE` can tell in advance.
fn describable(sql: &str) -> bool {
    matches!(
        crate::split::first_keyword(sql).as_deref(),
        Some("SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE" | "PIVOT" | "UNPIVOT")
    )
}

/// The CLI to run: the `duckdb_path` option, else `duckdb` on PATH or a usual location.
pub fn find_cli(cfg: &DataSourceConfig) -> DbResult<PathBuf> {
    if let Some(p) = cfg.options.get("duckdb_path").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(DbError::Config(format!("the DuckDB CLI `{}` does not exist", p.display())))
        };
    }
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].iter().map(PathBuf::from));
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".duckdb/cli/latest"));
        dirs.push(Path::new(&home).join(".local/bin"));
    }
    let exe = if cfg!(windows) { "duckdb.exe" } else { "duckdb" };
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file()).ok_or_else(|| {
        DbError::Connect(
            "the DuckDB CLI was not found: install it (`brew install duckdb`, or duckdb.org) or set the \
             `duckdb_path` option"
                .into(),
        )
    })
}

struct DuckCancel {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl Canceller for DuckCancel {
    fn cancel(&self) -> DbResult<()> {
        let mut g = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(c) = g.as_mut() {
            self.cancelled.store(true, Ordering::SeqCst);
            let _ = c.kill();
        }
        Ok(())
    }
}

pub(crate) struct DuckConn {
    program: PathBuf,
    path: PathBuf,
    read_only: bool,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

/// What one result of a script holds.
#[derive(Default)]
struct ScriptOut {
    /// Rows per statement, in order.
    results: Vec<Vec<OrderedRow>>,
}

impl DuckConn {
    pub(crate) fn open(cfg: &DataSourceConfig) -> DbResult<Self> {
        let path =
            cfg.path.clone().ok_or_else(|| DbError::Config("a DuckDB data source needs a database file".into()))?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.is_dir()
        {
            return Err(DbError::Connect(format!("the folder {} does not exist", dir.display())));
        }
        if cfg.read_only && !path.is_file() {
            return Err(DbError::Connect(format!(
                "{} does not exist (read-only data sources cannot create it)",
                path.display()
            )));
        }
        let program = find_cli(cfg)?;
        let mut conn = DuckConn {
            program,
            path,
            read_only: cfg.read_only,
            child: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        // Opening proves the file is a DuckDB database.
        conn.script(&["SELECT 1 AS ok"], |_, _| true).map_err(|e| DbError::Connect(e.to_string()))?;
        Ok(conn)
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        if self.read_only {
            c.arg("-readonly");
        }
        c.arg(&self.path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        c
    }

    /// Run statements in one process; `row(i, row)` gets each row of statement `i` and returns
    /// false to stop (the process is killed).
    fn script(&mut self, statements: &[&str], mut row: impl FnMut(usize, OrderedRow) -> bool) -> DbResult<()> {
        let started = Instant::now();
        loop {
            match self.script_once(statements, &mut row) {
                Err(DbError::Query(e)) if is_lock_error(&e) && started.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(Duration::from_millis(150));
                }
                other => return other,
            }
        }
    }

    fn script_once(&mut self, statements: &[&str], row: &mut impl FnMut(usize, OrderedRow) -> bool) -> DbResult<()> {
        let mut text = String::from(".bail on\n.mode jsonlines\n");
        for (i, s) in statements.iter().enumerate() {
            if i > 0 {
                text.push_str(&format!(".print {MARKER}\n"));
            }
            text.push_str(s.trim().trim_end_matches(';'));
            // On its own line, so a trailing `-- comment` cannot swallow it.
            text.push_str("\n;\n");
        }
        let mut child = self
            .command()
            .spawn()
            .map_err(|e| DbError::Connect(format!("cannot run {}: {e}", self.program.display())))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DbError::Connect("cannot talk to the DuckDB CLI".into()));
        };
        self.cancelled.store(false, Ordering::SeqCst);
        *self.child.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
        let err_reader = std::thread::Builder::new()
            .name("duckdb-stderr".into())
            .spawn(move || {
                let mut s = String::new();
                let mut r = stderr;
                let _ = r.by_ref().take(MAX_STDERR).read_to_string(&mut s);
                let _ = std::io::copy(&mut r, &mut std::io::sink());
                s
            })
            .map_err(|e| DbError::Connect(format!("cannot start a thread: {e}")))?;
        // The script is small; a CLI that does not read it fails below.
        let _ = stdin.write_all(text.as_bytes());
        drop(stdin);
        let mut reader = BufReader::with_capacity(256 * 1024, stdout);
        let mut index = 0usize;
        let mut stopped = false;
        let mut failure: Option<String> = None;
        // Lines that are not rows: notices, or an error a CLI printed to stdout.
        let mut stray: Vec<String> = Vec::new();
        loop {
            let mut line = Vec::new();
            let n = match reader.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
                Ok(n) => n,
                Err(e) => {
                    failure = Some(format!("reading DuckDB output failed: {e}"));
                    break;
                }
            };
            if n == 0 {
                break;
            }
            if line.len() > MAX_LINE {
                failure = Some("a result row is larger than 64 MB".into());
                break;
            }
            let line = String::from_utf8_lossy(&line);
            if line.trim() == MARKER {
                index += 1;
                continue;
            }
            match parse_line(&line) {
                None => {}
                Some(Ok(r)) => {
                    if !row(index, r) {
                        stopped = true;
                        break;
                    }
                }
                // Text that is not a row (a notice, `.print` output of the user's statement).
                Some(Err(text)) if stray.len() < 20 => stray.push(text.chars().take(500).collect()),
                Some(Err(_)) => {}
            }
        }
        let child = self.child.lock().unwrap_or_else(PoisonError::into_inner).take();
        let status = child.map(|mut c| {
            if stopped || failure.is_some() {
                let _ = c.kill();
            }
            c.wait()
        });
        drop(reader);
        let stderr = err_reader.join().unwrap_or_default();
        if self.cancelled.swap(false, Ordering::SeqCst) {
            return Err(DbError::Cancelled);
        }
        if stopped {
            return Ok(());
        }
        if let Some(f) = failure {
            return Err(DbError::Query(f));
        }
        let ok = matches!(&status, Some(Ok(s)) if s.success());
        let err_text = stderr.trim();
        if !ok || err_text.contains("Error") {
            let msg = if err_text.is_empty() && !stray.is_empty() {
                stray.join("\n")
            } else if err_text.is_empty() {
                match status {
                    Some(Ok(s)) => format!("DuckDB exited with {s}"),
                    Some(Err(e)) => format!("DuckDB failed: {e}"),
                    None => "DuckDB failed".into(),
                }
            } else {
                err_text.chars().take(4000).collect()
            };
            return Err(DbError::Query(msg));
        }
        Ok(())
    }

    fn rows(&mut self, statements: &[&str]) -> DbResult<ScriptOut> {
        let mut out = ScriptOut { results: statements.iter().map(|_| Vec::new()).collect() };
        self.script(statements, |i, r| {
            if let Some(v) = out.results.get_mut(i) {
                v.push(r);
            }
            true
        })?;
        Ok(out)
    }

    fn one(&mut self, sql: &str) -> DbResult<Vec<OrderedRow>> {
        Ok(self.rows(&[sql])?.results.into_iter().next().unwrap_or_default())
    }
}

fn is_lock_error(e: &str) -> bool {
    e.contains("Could not set lock") || e.contains("Conflicting lock")
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `[a, "b c"]` (a DuckDB list printed as text or JSON) → names.
fn list_items(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => {
            a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect()
        }
        Some(Value::String(s)) => s
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// `FOREIGN KEY (a, b) REFERENCES other(x, y)` → (columns, table, ref columns).
pub fn parse_fk(text: &str) -> Option<(Vec<String>, String, Vec<String>)> {
    let upper = text.to_ascii_uppercase();
    let fk = upper.find("FOREIGN KEY")?;
    let refs = upper.find("REFERENCES")?;
    let cols_part = text.get(fk + "FOREIGN KEY".len()..refs)?;
    let rest = text.get(refs + "REFERENCES".len()..)?.trim();
    let split = |s: &str| -> Vec<String> {
        s.trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .map(|c| c.trim().trim_matches('"').to_string())
            .filter(|c| !c.is_empty())
            .collect()
    };
    let (table, ref_cols) = match rest.find('(') {
        Some(i) => (rest.get(..i)?.trim(), rest.get(i..)?.split(')').next().unwrap_or_default()),
        None => (rest, ""),
    };
    Some((split(cols_part), table.trim_matches('"').to_string(), split(ref_cols)))
}

impl Connection for DuckConn {
    fn kind(&self) -> DbKind {
        DbKind::DuckDb
    }

    fn server_version(&mut self) -> DbResult<String> {
        let out = Command::new(&self.program)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| DbError::Query(format!("cannot run {}: {e}", self.program.display())))?;
        let v = String::from_utf8_lossy(&out.stdout);
        let v = v.split_whitespace().next().unwrap_or("");
        Ok(if v.is_empty() { "DuckDB".into() } else { format!("DuckDB {v}") })
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(DuckCancel { child: self.child.clone(), cancelled: self.cancelled.clone() })
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        let started = Instant::now();
        let describe = describable(sql);
        let describe_sql = format!("DESCRIBE {}", sql.trim().trim_end_matches(';'));
        let statements: Vec<&str> = if describe { vec![describe_sql.as_str(), sql] } else { vec![sql] };
        let result_index = statements.len() - 1;
        let mut columns: Vec<Column> = Vec::new();
        let mut announced = false;
        let mut pager: Option<Pager<'_>> = None;
        let mut sink_slot: Option<&mut dyn ResultSink> = Some(sink);
        let r = self.script(&statements, |i, row| {
            if i < result_index {
                columns.push(Column { name: row.text("column_name"), type_name: row.text("column_type") });
                return true;
            }
            if !announced {
                if columns.is_empty() {
                    columns = row.0.iter().map(|(k, _)| Column { name: k.clone(), type_name: String::new() }).collect();
                }
                announced = true;
                if let Some(s) = sink_slot.take() {
                    s.columns(&columns);
                    pager = Some(Pager::new(s, page_size, started));
                }
            }
            let cells = columns
                .iter()
                .enumerate()
                .map(|(k, c)| row.0.get(k).map(|(_, v)| cell(v, &c.type_name)).unwrap_or(Cell::Null))
                .collect();
            if let Some(p) = pager.as_mut()
                && !p.push(cells)
            {
                return false;
            }
            true
        });
        r?;
        let pager = match pager {
            Some(p) => p,
            None => {
                let s = sink_slot.take().ok_or_else(|| DbError::Query("result sink lost".into()))?;
                s.columns(&columns);
                Pager::new(s, page_size, started)
            }
        };
        let affected = (!describe && columns.is_empty()).then_some(0);
        Ok(pager.finish(columns, affected))
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let rows = self.one(
            "SELECT schema_name FROM information_schema.schemata WHERE catalog_name = current_database() \
             AND schema_name NOT IN ('information_schema', 'pg_catalog') ORDER BY schema_name",
        )?;
        let schemas = rows.iter().map(|r| r.text("schema_name")).collect();
        Ok(DatabaseInfo { databases: Vec::new(), current: None, schemas, default_schema: Some("main".into()) })
    }

    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>> {
        let rows = self.one(&format!(
            "SELECT table_name, table_type FROM information_schema.tables WHERE table_catalog = current_database() \
             AND table_schema = {} ORDER BY table_name",
            lit(schema)
        ))?;
        Ok(rows
            .iter()
            .map(|r| TableInfo {
                name: r.text("table_name"),
                kind: if r.text("table_type").contains("VIEW") { TableKind::View } else { TableKind::Table },
            })
            .collect())
    }

    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails> {
        let (s, t) = (lit(schema), lit(table));
        let cols = format!(
            "SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns \
             WHERE table_catalog = current_database() AND table_schema = {s} AND table_name = {t} ORDER BY ordinal_position"
        );
        let cons = format!(
            "SELECT constraint_type, constraint_column_names, constraint_text FROM duckdb_constraints() \
             WHERE database_name = current_database() AND schema_name = {s} AND table_name = {t} \
             AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY')"
        );
        let idx = format!(
            "SELECT index_name, is_unique, is_primary, expressions FROM duckdb_indexes() \
             WHERE database_name = current_database() AND schema_name = {s} AND table_name = {t} ORDER BY index_name"
        );
        let out = self.rows(&[&cols, &cons, &idx])?;
        let mut results = out.results.into_iter();
        let (crows, krows, irows) = (
            results.next().unwrap_or_default(),
            results.next().unwrap_or_default(),
            results.next().unwrap_or_default(),
        );
        let pk: Vec<String> = krows
            .iter()
            .filter(|r| r.text("constraint_type") == "PRIMARY KEY")
            .flat_map(|r| list_items(r.get("constraint_column_names")))
            .collect();
        let columns = crows
            .iter()
            .map(|r| {
                let name = r.text("column_name");
                ColumnInfo {
                    primary_key: pk.contains(&name),
                    name,
                    data_type: r.text("data_type"),
                    nullable: r.text("is_nullable") == "YES",
                    default: Some(r.text("column_default")).filter(|d| !d.is_empty()),
                }
            })
            .collect();
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in &krows {
            let kind = r.text("constraint_type");
            if kind == "PRIMARY KEY" || kind == "UNIQUE" {
                let cols = list_items(r.get("constraint_column_names"));
                indexes.push(IndexInfo {
                    name: format!("{table}_{}", if kind == "UNIQUE" { "unique" } else { "pkey" }),
                    columns: cols,
                    unique: true,
                    primary: kind == "PRIMARY KEY",
                });
            }
        }
        for r in &irows {
            indexes.push(IndexInfo {
                name: r.text("index_name"),
                columns: list_items(r.get("expressions")),
                unique: r.get("is_unique") == Some(&Value::Bool(true)),
                primary: r.get("is_primary") == Some(&Value::Bool(true)),
            });
        }
        let foreign_keys = krows
            .iter()
            .filter(|r| r.text("constraint_type") == "FOREIGN KEY")
            .filter_map(|r| parse_fk(&r.text("constraint_text")))
            .enumerate()
            .map(|(i, (columns, ref_table, ref_columns))| ForeignKeyInfo {
                name: format!("{table}_fk_{i}"),
                columns,
                ref_schema: None,
                ref_table,
                ref_columns,
            })
            .collect();
        Ok(TableDetails { columns, indexes, foreign_keys })
    }

    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64> {
        let rows =
            self.one(&format!("SELECT count(*) AS n FROM {}", crate::sql::qualified(DbKind::DuckDb, schema, table)))?;
        Ok(rows.first().and_then(|r| r.get("n")).and_then(Value::as_u64).unwrap_or(0))
    }

    /// Every statement commits on its own.
    fn transaction_status(&mut self) -> crate::tx::TxStatus {
        crate::tx::TxStatus::Idle
    }

    fn ping(&mut self) -> DbResult<()> {
        if self.path.is_file() || !self.read_only {
            Ok(())
        } else {
            Err(DbError::Connect(format!("{} is gone", self.path.display())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_key_order_and_types() {
        let r: OrderedRow = serde_json::from_str(r#"{"z":1,"a":"x","m":null,"l":[1,2]}"#).unwrap();
        assert_eq!(r.0.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["z", "a", "m", "l"]);
        assert_eq!(cell(&r.0[0].1, "INTEGER"), Cell::Int(1));
        assert_eq!(cell(&r.0[1].1, "VARCHAR"), Cell::Text("x".into()));
        assert_eq!(cell(&r.0[2].1, "VARCHAR"), Cell::Null);
        assert_eq!(cell(&r.0[3].1, "INTEGER[]"), Cell::Other("[1,2]".into()));
        assert_eq!(cell(&Value::String("2024-01-01".into()), "DATE"), Cell::Other("2024-01-01".into()));
        assert_eq!(cell(&serde_json::json!(1.5), "DECIMAL(4,2)"), Cell::Other("1.5".into()));
        for bad in ["{", "{\"a\":}", "[1]", "x", "{\"a\":1}{", "\u{0}"] {
            let _ = parse_line(bad);
        }
        assert!(parse_line("   ").is_none());
        assert!(matches!(parse_line("Error: x"), Some(Err(_))));
    }

    #[test]
    fn foreign_keys_and_lists_parse() {
        assert_eq!(
            parse_fk("FOREIGN KEY (author_id, x) REFERENCES authors(id, y)"),
            Some((vec!["author_id".into(), "x".into()], "authors".into(), vec!["id".into(), "y".into()]))
        );
        assert_eq!(parse_fk("CHECK (a > 0)"), None);
        assert_eq!(parse_fk("FOREIGN KEY (a) REFERENCES"), Some((vec!["a".into()], String::new(), vec![])));
        assert_eq!(list_items(Some(&Value::String("[a, \"b c\"]".into()))), ["a", "b c"]);
        assert_eq!(list_items(Some(&serde_json::json!(["id"]))), ["id"]);
        assert!(describable("  with x as (select 1) select * from x"));
        assert!(!describable("INSERT INTO t VALUES (1)"));
    }
}
