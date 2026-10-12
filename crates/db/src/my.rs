//! MySQL and MariaDB through the `mysql` crate (pure Rust, no TLS yet). Cancel is
//! `KILL QUERY <id>` over a second connection.

use std::sync::Arc;
use std::time::Instant;

use mysql::consts::ColumnType;
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, OptsBuilder, Value};

use crate::sql::count_rows;
use crate::{
    Canceller, Cell, Column, ColumnInfo, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult,
    ExecSummary, ForeignKeyInfo, IndexInfo, Pager, ResultSink, SslMode, TableDetails, TableInfo, TableKind,
};

/// ER_QUERY_INTERRUPTED.
const QUERY_INTERRUPTED: u16 = 1317;
/// The binary character set: values are bytes, not text.
const BINARY_CHARSET: u16 = 63;

pub(crate) struct MyConn {
    conn: Conn,
    opts: Opts,
    kind: DbKind,
    default_db: Option<String>,
}

struct MyCancel {
    opts: Opts,
    id: u32,
}

impl Canceller for MyCancel {
    fn cancel(&self) -> DbResult<()> {
        kill_query(&self.opts, self.id)
    }
}

fn kill_query(opts: &Opts, id: u32) -> DbResult<()> {
    let mut c = Conn::new(opts.clone()).map_err(|e| DbError::Query(format!("cancel failed: {e}")))?;
    c.query_drop(format!("KILL QUERY {id}")).map_err(|e| DbError::Query(format!("cancel failed: {e}")))
}

fn err(e: mysql::Error) -> DbError {
    match e {
        mysql::Error::MySqlError(m) if m.code == QUERY_INTERRUPTED => DbError::Cancelled,
        mysql::Error::MySqlError(m) => DbError::Query(format!("[{}] {} ({})", m.state, m.message, m.code)),
        other => DbError::Query(other.to_string()),
    }
}

fn value_string(v: &Value) -> Option<String> {
    Some(match v {
        Value::NULL => return None,
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Date(y, mo, d, h, mi, s, us) => {
            let base = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
            if *us == 0 { base } else { format!("{base}.{us:06}") }
        }
        Value::Time(neg, d, h, m, s, us) => {
            let hours = u64::from(*d) * 24 + u64::from(*h);
            let base = format!("{}{hours:02}:{m:02}:{s:02}", if *neg { "-" } else { "" });
            if *us == 0 { base } else { format!("{base}.{us:06}") }
        }
    })
}

fn cell(v: Option<&Value>, ty: ColumnType, charset: u16) -> Cell {
    let Some(v) = v else { return Cell::Null };
    match v {
        Value::NULL => Cell::Null,
        Value::Int(i) => Cell::Int(*i),
        Value::UInt(u) => Cell::UInt(*u),
        Value::Float(f) => Cell::Float(f64::from(*f)),
        Value::Double(f) => Cell::Float(*f),
        Value::Bytes(b) => {
            let s = || String::from_utf8_lossy(b).into_owned();
            match ty {
                ColumnType::MYSQL_TYPE_TINY
                | ColumnType::MYSQL_TYPE_SHORT
                | ColumnType::MYSQL_TYPE_INT24
                | ColumnType::MYSQL_TYPE_LONG
                | ColumnType::MYSQL_TYPE_LONGLONG
                | ColumnType::MYSQL_TYPE_YEAR => {
                    let t = s();
                    t.parse::<i64>()
                        .map(Cell::Int)
                        .or_else(|_| t.parse::<u64>().map(Cell::UInt))
                        .unwrap_or(Cell::Text(t))
                }
                ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
                    let t = s();
                    t.parse::<f64>().map(Cell::Float).unwrap_or(Cell::Text(t))
                }
                ColumnType::MYSQL_TYPE_DECIMAL
                | ColumnType::MYSQL_TYPE_NEWDECIMAL
                | ColumnType::MYSQL_TYPE_DATE
                | ColumnType::MYSQL_TYPE_DATETIME
                | ColumnType::MYSQL_TYPE_TIMESTAMP
                | ColumnType::MYSQL_TYPE_TIME => Cell::Other(s()),
                ColumnType::MYSQL_TYPE_BIT => Cell::Bytes(b.clone()),
                _ if charset == BINARY_CHARSET => Cell::Bytes(b.clone()),
                _ => Cell::Text(s()),
            }
        }
        other => value_string(other).map(Cell::Other).unwrap_or(Cell::Null),
    }
}

fn type_name(ty: ColumnType) -> String {
    format!("{ty:?}").trim_start_matches("MYSQL_TYPE_").to_ascii_lowercase()
}

impl MyConn {
    pub(crate) fn open(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Self> {
        if matches!(cfg.ssl_mode()?, SslMode::Require | SslMode::Verify) {
            return Err(DbError::Unsupported(format!("TLS not supported yet for {} connections", cfg.kind.label())));
        }
        let user = if cfg.user.is_empty() { None } else { Some(cfg.user.clone()) };
        let db = if cfg.database.is_empty() { None } else { Some(cfg.database.clone()) };
        let builder = OptsBuilder::new()
            .ip_or_hostname(Some(cfg.host.trim().to_string()))
            .tcp_port(cfg.port_or_default())
            .user(user)
            .pass(password.filter(|p| !p.is_empty()).map(str::to_string))
            .db_name(db.clone())
            .prefer_socket(false)
            .tcp_connect_timeout(Some(cfg.connect_timeout()));
        let opts = Opts::from(builder);
        let conn = Conn::new(opts.clone()).map_err(|e| DbError::Connect(e.to_string()))?;
        Ok(Self { conn, opts, kind: cfg.kind, default_db: db })
    }

    fn strings(&mut self, sql: &str, params: Vec<Value>) -> DbResult<Vec<Vec<Option<String>>>> {
        let rows: Vec<mysql::Row> = if params.is_empty() {
            self.conn.query(sql).map_err(err)?
        } else {
            self.conn.exec(sql, mysql::Params::Positional(params)).map_err(err)?
        };
        Ok(rows.iter().map(|r| (0..r.len()).map(|i| r.as_ref(i).and_then(value_string)).collect()).collect())
    }
}

fn col(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn v(s: &str) -> Value {
    Value::Bytes(s.as_bytes().to_vec())
}

impl Connection for MyConn {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn server_version(&mut self) -> DbResult<String> {
        let rows = self.strings("SELECT VERSION()", Vec::new())?;
        let ver = rows.first().map(|r| col(r, 0)).unwrap_or_default();
        let label = if ver.to_ascii_lowercase().contains("mariadb") { "MariaDB" } else { "MySQL" };
        Ok(format!("{label} {ver}"))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(MyCancel { opts: self.opts.clone(), id: self.conn.connection_id() })
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        let started = Instant::now();
        let id = self.conn.connection_id();
        let mut result = self.conn.query_iter(sql).map_err(err)?;
        let meta: Vec<(ColumnType, u16)> =
            result.columns().as_ref().iter().map(|c| (c.column_type(), c.character_set())).collect();
        let columns: Vec<Column> = result
            .columns()
            .as_ref()
            .iter()
            .map(|c| Column { name: c.name_str().into_owned(), type_name: type_name(c.column_type()) })
            .collect();
        sink.columns(&columns);
        let mut pager = Pager::new(sink, page_size, started);
        let mut abandoned = false;
        for row in result.by_ref() {
            let row = row.map_err(err)?;
            let cells = meta.iter().enumerate().map(|(i, (ty, cs))| cell(row.as_ref(i), *ty, *cs)).collect();
            if !pager.push(cells) {
                abandoned = true;
                break;
            }
        }
        if abandoned {
            // Dropping the result drains it; stop the server first.
            let _ = kill_query(&self.opts, id);
        }
        let affected = columns.is_empty().then(|| result.affected_rows());
        drop(result);
        Ok(pager.finish(columns, affected))
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let current = self.strings("SELECT DATABASE()", Vec::new())?.first().and_then(|r| r.first().cloned().flatten());
        let schemas = self
            .strings(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN \
                 ('information_schema', 'performance_schema', 'mysql', 'sys') ORDER BY SCHEMA_NAME",
                Vec::new(),
            )?
            .iter()
            .map(|r| col(r, 0))
            .collect();
        let default_schema = current.clone().or_else(|| self.default_db.clone());
        Ok(DatabaseInfo { databases: Vec::new(), current, schemas, default_schema })
    }

    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>> {
        Ok(self
            .strings(
                "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                vec![v(schema)],
            )?
            .iter()
            .map(|r| TableInfo {
                name: col(r, 0),
                kind: if col(r, 1).contains("VIEW") { TableKind::View } else { TableKind::Table },
            })
            .collect())
    }

    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails> {
        let columns = self
            .strings(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                vec![v(schema), v(table)],
            )?
            .iter()
            .map(|r| ColumnInfo {
                name: col(r, 0),
                data_type: col(r, 1),
                nullable: col(r, 2) == "YES",
                default: r.get(3).cloned().flatten(),
                primary_key: col(r, 4) == "PRI",
            })
            .collect();
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in self.strings(
            "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            vec![v(schema), v(table)],
        )? {
            let name = col(&r, 0);
            if indexes.last().map(|i| &i.name) != Some(&name) {
                indexes.push(IndexInfo {
                    primary: name == "PRIMARY",
                    unique: col(&r, 1) == "0",
                    name,
                    columns: Vec::new(),
                });
            }
            if let Some(i) = indexes.last_mut() {
                i.columns.push(col(&r, 2));
            }
        }
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in self.strings(
            "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
            vec![v(schema), v(table)],
        )? {
            let name = col(&r, 0);
            if foreign_keys.last().map(|f| &f.name) != Some(&name) {
                foreign_keys.push(ForeignKeyInfo {
                    name,
                    ref_schema: r.get(2).cloned().flatten(),
                    ref_table: col(&r, 3),
                    ..Default::default()
                });
            }
            if let Some(f) = foreign_keys.last_mut() {
                f.columns.push(col(&r, 1));
                f.ref_columns.push(col(&r, 4));
            }
        }
        Ok(TableDetails { columns, indexes, foreign_keys })
    }

    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64> {
        let rows = self.strings(&count_rows(self.kind, schema, table), Vec::new())?;
        Ok(rows.first().map(|r| col(r, 0)).and_then(|s| s.parse().ok()).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_protocol_values_get_types() {
        let b = |s: &str| Value::Bytes(s.as_bytes().to_vec());
        assert_eq!(cell(Some(&b("42")), ColumnType::MYSQL_TYPE_LONG, 33), Cell::Int(42));
        assert_eq!(cell(Some(&b("18446744073709551615")), ColumnType::MYSQL_TYPE_LONGLONG, 33), Cell::UInt(u64::MAX));
        assert_eq!(cell(Some(&b("1.5")), ColumnType::MYSQL_TYPE_DOUBLE, 33), Cell::Float(1.5));
        assert_eq!(cell(Some(&b("1.50")), ColumnType::MYSQL_TYPE_NEWDECIMAL, 33), Cell::Other("1.50".into()));
        assert_eq!(cell(Some(&b("x")), ColumnType::MYSQL_TYPE_VAR_STRING, 33), Cell::Text("x".into()));
        assert_eq!(cell(Some(&b("x")), ColumnType::MYSQL_TYPE_BLOB, BINARY_CHARSET), Cell::Bytes(vec![b'x']));
        assert_eq!(cell(Some(&Value::NULL), ColumnType::MYSQL_TYPE_LONG, 33), Cell::Null);
        assert_eq!(cell(None, ColumnType::MYSQL_TYPE_LONG, 33), Cell::Null);
        assert_eq!(type_name(ColumnType::MYSQL_TYPE_VARCHAR), "varchar");
        assert_eq!(value_string(&Value::Time(true, 1, 2, 3, 4, 0)).unwrap(), "-26:03:04");
    }
}
