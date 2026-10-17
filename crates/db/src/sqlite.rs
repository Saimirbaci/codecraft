//! SQLite through rusqlite (bundled SQLite).
//!
//! Attached databases (track DX): every `attach.<alias>` option of the data source names a
//! database file that each connection attaches as schema `<alias>` (`ATTACH DATABASE`), so the
//! tree, completion and consoles all see it; [`attachments`] lists them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{ErrorCode, OpenFlags};

use crate::sql::{count_rows, quote_ident};
use crate::{
    Canceller, Cell, Column, ColumnInfo, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult,
    ExecSummary, ForeignKeyInfo, IndexInfo, Pager, ResultSink, TableDetails, TableInfo, TableKind,
};

pub(crate) struct SqliteConn {
    conn: rusqlite::Connection,
}

struct SqliteCancel(rusqlite::InterruptHandle);

impl Canceller for SqliteCancel {
    fn cancel(&self) -> DbResult<()> {
        self.0.interrupt();
        Ok(())
    }
}

fn err(e: rusqlite::Error) -> DbError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::OperationInterrupted => DbError::Cancelled,
        _ => DbError::Query(e.to_string()),
    }
}

fn cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Int(i),
        ValueRef::Real(f) => Cell::Float(f),
        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
    }
}

/// A bind value: SQLite has integers, reals, text and blobs. Booleans are 0/1; unsigned values
/// above `i64::MAX` bind as their decimal text (an INTEGER column converts what fits).
fn bind_value(c: &Cell) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match c {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Integer(i64::from(*b)),
        Cell::Int(i) => Value::Integer(*i),
        Cell::UInt(u) => match i64::try_from(*u) {
            Ok(i) => Value::Integer(i),
            Err(_) => Value::Text(u.to_string()),
        },
        Cell::Float(f) => Value::Real(*f),
        Cell::Text(s) | Cell::Other(s) => Value::Text(s.clone()),
        Cell::Bytes(b) => Value::Blob(b.clone()),
    }
}

impl SqliteConn {
    pub(crate) fn open(cfg: &DataSourceConfig) -> DbResult<Self> {
        let path = cfg.path.as_ref().ok_or_else(|| DbError::Config("no database file".into()))?;
        let flags = if cfg.read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::default()
        };
        let conn = rusqlite::Connection::open_with_flags(path, flags)
            .map_err(|e| DbError::Connect(format!("{}: {e}", path.display())))?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(|e| DbError::Connect(e.to_string()))?;
        // Opening is lazy; touch the schema so a non-database file fails here, not on first use.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
            .map_err(|e| DbError::Connect(format!("{}: {e}", path.display())))?;
        for (alias, file) in attachments(cfg)? {
            if !file.is_file() {
                return Err(DbError::Connect(format!("attached database `{alias}`: no file {}", file.display())));
            }
            let target = if cfg.read_only { read_only_uri(&file) } else { file.display().to_string() };
            conn.execute(&format!("ATTACH DATABASE ?1 AS {}", quote_ident(DbKind::Sqlite, &alias)), [&target])
                .map_err(|e| DbError::Connect(format!("cannot attach `{alias}` ({}): {e}", file.display())))?;
        }
        Ok(Self { conn })
    }

    fn strings(&self, sql: &str, params: &[&str]) -> DbResult<Vec<Vec<Option<String>>>> {
        let mut stmt = self.conn.prepare(sql).map_err(err)?;
        let n = stmt.column_count();
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).map_err(err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(err)? {
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                v.push(match row.get_ref(i).map(cell) {
                    Ok(Cell::Null) | Err(_) => None,
                    Ok(c) => Some(c.display()),
                });
            }
            out.push(v);
        }
        Ok(out)
    }
}

/// The `attach.<alias>` options: (alias, file), sorted by alias. Relative files are as given
/// (the engine resolves them against the project).
pub fn attachments(cfg: &DataSourceConfig) -> DbResult<Vec<(String, std::path::PathBuf)>> {
    let mut out = Vec::new();
    for (k, v) in &cfg.options {
        let Some(alias) = k.strip_prefix("attach.") else { continue };
        let alias = alias.trim();
        check_alias(alias)?;
        let file = v.trim();
        if file.is_empty() {
            return Err(DbError::Config(format!("attached database `{alias}` needs a file")));
        }
        out.push((alias.to_string(), std::path::PathBuf::from(file)));
    }
    Ok(out)
}

/// A schema name for an attached database: not `main`/`temp`, 1–64 plain characters.
pub fn check_alias(alias: &str) -> DbResult<()> {
    let ok = !alias.is_empty()
        && alias.chars().count() <= 64
        && alias.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !alias.eq_ignore_ascii_case("main")
        && !alias.eq_ignore_ascii_case("temp");
    if ok {
        Ok(())
    } else {
        Err(DbError::Config(format!(
            "`{alias}` cannot name an attached database (letters, digits and _; not main or temp)"
        )))
    }
}

/// `file:<path>?mode=ro` with the URI's special characters escaped.
fn read_only_uri(path: &std::path::Path) -> String {
    let mut uri = String::from("file:");
    for c in path.display().to_string().chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?mode=ro");
    uri
}

fn col(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

impl Connection for SqliteConn {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    fn server_version(&mut self) -> DbResult<String> {
        Ok(format!("SQLite {}", rusqlite::version()))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(SqliteCancel(self.conn.get_interrupt_handle()))
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        self.execute_bound(sql, &[], page_size, sink)
    }

    fn execute_bound(
        &mut self,
        sql: &str,
        params: &[Cell],
        page_size: usize,
        sink: &mut dyn ResultSink,
    ) -> DbResult<ExecSummary> {
        let started = Instant::now();
        let mut stmt = self.conn.prepare(sql).map_err(err)?;
        let expected = stmt.parameter_count();
        if expected != params.len() {
            return Err(DbError::Query(format!(
                "the statement has {expected} parameter{} but {} value{} given",
                if expected == 1 { "" } else { "s" },
                params.len(),
                if params.len() == 1 { " was" } else { "s were" }
            )));
        }
        let values: Vec<rusqlite::types::Value> = params.iter().map(bind_value).collect();
        let columns: Vec<Column> = stmt
            .columns()
            .iter()
            .map(|c| Column { name: c.name().to_string(), type_name: c.decl_type().unwrap_or("").to_string() })
            .collect();
        sink.columns(&columns);
        let n = columns.len();
        let mut pager = Pager::new(sink, page_size, started);
        {
            let mut rows = stmt.query(rusqlite::params_from_iter(values.iter())).map_err(err)?;
            while let Some(row) = rows.next().map_err(err)? {
                let cells = (0..n).map(|i| row.get_ref(i).map(cell).unwrap_or(Cell::Null)).collect();
                if !pager.push(cells) {
                    break;
                }
            }
        }
        let affected = (n == 0).then(|| self.conn.changes());
        Ok(pager.finish(columns, affected))
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let rows = self.strings("SELECT name FROM pragma_database_list ORDER BY seq", &[])?;
        let schemas: Vec<String> = rows.iter().map(|r| col(r, 0)).filter(|s| s != "temp").collect();
        Ok(DatabaseInfo { databases: Vec::new(), current: None, schemas, default_schema: Some("main".into()) })
    }

    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master WHERE type IN ('table', 'view') \
             AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
            quote_ident(DbKind::Sqlite, schema)
        );
        Ok(self
            .strings(&sql, &[])?
            .iter()
            .map(|r| TableInfo {
                name: col(r, 0),
                kind: if col(r, 1) == "view" { TableKind::View } else { TableKind::Table },
            })
            .collect())
    }

    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails> {
        let columns = self
            .strings(
                "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_xinfo(?1, ?2) WHERE hidden <> 1 ORDER BY cid",
                &[table, schema],
            )?
            .iter()
            .map(|r| ColumnInfo {
                name: col(r, 0),
                data_type: col(r, 1),
                nullable: col(r, 2) != "1",
                default: r.get(3).cloned().flatten(),
                primary_key: col(r, 4) != "0" && !col(r, 4).is_empty(),
            })
            .collect();
        let mut indexes = Vec::new();
        for r in self
            .strings("SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY name", &[table, schema])?
        {
            let name = col(&r, 0);
            let cols = self
                .strings("SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno", &[&name, schema])?
                .iter()
                .map(|c| col(c, 0))
                .collect();
            indexes.push(IndexInfo { name, columns: cols, unique: col(&r, 1) == "1", primary: col(&r, 2) == "pk" });
        }
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        let mut last_id = String::new();
        for r in self.strings(
            "SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
            &[table, schema],
        )? {
            let id = col(&r, 0);
            if id != last_id || foreign_keys.is_empty() {
                foreign_keys.push(ForeignKeyInfo {
                    name: format!("{table}_fk_{id}"),
                    ref_table: col(&r, 1),
                    ..Default::default()
                });
                last_id = id;
            }
            if let Some(fk) = foreign_keys.last_mut() {
                fk.columns.push(col(&r, 2));
                fk.ref_columns.push(col(&r, 3));
            }
        }
        Ok(TableDetails { columns, indexes, foreign_keys })
    }

    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64> {
        let n: i64 = self.conn.query_row(&count_rows(DbKind::Sqlite, schema, table), [], |r| r.get(0)).map_err(err)?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    fn in_transaction(&mut self) -> Option<bool> {
        Some(!self.conn.is_autocommit())
    }
}
