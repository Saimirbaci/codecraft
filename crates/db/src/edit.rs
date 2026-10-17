//! In-grid editing: turning pending row changes into `UPDATE`/`INSERT`/`DELETE` statements and
//! applying them in one transaction.
//!
//! Applied changes bind their values ([`bound_statement`], [`crate::bind`]): PostgreSQL, MySQL,
//! MariaDB, SQLite and SQL Server get placeholders and parameters, never values inside the SQL.
//! [`statement`] writes the same statement with literals (scripts, the clipboard, drivers that
//! cannot bind), escaped per dialect: strings that a dialect would read escapes from
//! (backslashes in MySQL and PostgreSQL, NUL anywhere) are written in a form that is safe
//! whatever the server's settings, such as `E'…'` for PostgreSQL or a hex string converted to
//! text for MySQL and SQLite.

use serde::{Deserialize, Serialize};

use crate::bind::{Binder, Bound};
use crate::sql::{qualified, quote_ident};
use crate::tx::{self, Discard, TxStatus};
use crate::{Cell, Connection, DbError, DbKind, DbResult};

/// One pending change to a table row. Keys identify the row by its primary key columns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum RowChange {
    Update {
        key: Vec<(String, Cell)>,
        set: Vec<(String, Cell)>,
    },
    /// Columns left out take their default.
    Insert {
        values: Vec<(String, Cell)>,
    },
    Delete {
        key: Vec<(String, Cell)>,
    },
}

/// A value as an SQL literal for `kind`.
pub fn literal(kind: DbKind, value: &Cell) -> DbResult<String> {
    Ok(match value {
        Cell::Null => "NULL".into(),
        Cell::Bool(b) => match kind {
            DbKind::Sqlite | DbKind::SqlServer => if *b { "1" } else { "0" }.into(),
            _ => if *b { "TRUE" } else { "FALSE" }.into(),
        },
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if f.is_finite() => {
            let s = f.to_string();
            if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
        }
        Cell::Float(f) => match kind {
            DbKind::Postgres => {
                let v = if f.is_nan() {
                    "NaN"
                } else if *f > 0.0 {
                    "Infinity"
                } else {
                    "-Infinity"
                };
                format!("'{v}'::float8")
            }
            _ => return Err(DbError::Unsupported(format!("{} cannot store {f}", kind.label()))),
        },
        Cell::Text(s) | Cell::Other(s) => text_literal(kind, s)?,
        Cell::Bytes(b) => match kind {
            DbKind::Postgres => format!("decode('{}', 'hex')", hex(b)),
            DbKind::SqlServer => format!("0x{}", hex(b)),
            DbKind::ClickHouse => format!("unhex('{}')", hex(b)),
            DbKind::DuckDb => format!("from_hex('{}')", hex(b)),
            _ => format!("X'{}'", hex(b)),
        },
    })
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02X}"));
    }
    s
}

fn text_literal(kind: DbKind, s: &str) -> DbResult<String> {
    let quoted = || format!("'{}'", s.replace('\'', "''"));
    Ok(match kind {
        DbKind::Postgres => {
            if s.contains('\0') {
                return Err(DbError::Unsupported("PostgreSQL text cannot contain NUL characters".into()));
            }
            if s.contains('\\') {
                // E'' reads escapes whatever `standard_conforming_strings` says.
                format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
            } else {
                quoted()
            }
        }
        DbKind::Mysql | DbKind::Mariadb => {
            if s.contains(['\\', '\0']) {
                // Backslashes are escapes unless NO_BACKSLASH_ESCAPES is set: a hex string reads
                // the same either way.
                format!("CONVERT(X'{}' USING utf8mb4)", hex(s.as_bytes()))
            } else {
                quoted()
            }
        }
        DbKind::Sqlite => {
            if s.contains('\0') {
                format!("CAST(X'{}' AS TEXT)", hex(s.as_bytes()))
            } else {
                quoted()
            }
        }
        // N'' keeps Unicode whatever the column's collation.
        DbKind::SqlServer => format!("N{}", quoted()),
        DbKind::ClickHouse => crate::clickhouse::literal(s),
        DbKind::DuckDb => {
            if s.contains('\0') {
                return Err(DbError::Unsupported("DuckDB text cannot contain NUL characters".into()));
            }
            quoted()
        }
        DbKind::Redis => return Err(DbError::Unsupported("Redis has no SQL literals".into())),
    })
}

fn where_clause(kind: DbKind, key: &[(String, Cell)], b: &mut Binder) -> DbResult<String> {
    if key.is_empty() {
        return Err(DbError::Unsupported("the row has no primary key values".into()));
    }
    let mut parts = Vec::with_capacity(key.len());
    for (col, value) in key {
        if value.is_null() {
            parts.push(format!("{} IS NULL", quote_ident(kind, col)));
        } else {
            parts.push(format!("{} = {}", quote_ident(kind, col), b.value(value)?));
        }
    }
    Ok(parts.join(" AND "))
}

/// The statement for one change, with literal values. `schema` may be empty (unqualified table).
pub fn statement(kind: DbKind, schema: &str, table: &str, change: &RowChange) -> DbResult<String> {
    let mut b = Binder::literals(kind);
    build(kind, schema, table, change, &mut b)
}

/// The statement for one change with its values as bind parameters (literals where the driver
/// cannot bind: [`crate::bind::binds_generated`]).
pub fn bound_statement(kind: DbKind, schema: &str, table: &str, change: &RowChange) -> DbResult<Bound> {
    let mut b = Binder::new(kind);
    let sql = build(kind, schema, table, change, &mut b)?;
    Ok(b.finish(sql))
}

/// Every change's bound statement, in order.
pub fn bound_statements(kind: DbKind, schema: &str, table: &str, changes: &[RowChange]) -> DbResult<Vec<Bound>> {
    changes.iter().map(|c| bound_statement(kind, schema, table, c)).collect()
}

fn build(kind: DbKind, schema: &str, table: &str, change: &RowChange, b: &mut Binder) -> DbResult<String> {
    let target = qualified(kind, schema, table);
    match change {
        RowChange::Update { key, set } => {
            if set.is_empty() {
                return Err(DbError::Unsupported("nothing to update".into()));
            }
            let mut assignments = Vec::with_capacity(set.len());
            for (col, value) in set {
                assignments.push(format!("{} = {}", quote_ident(kind, col), b.value(value)?));
            }
            Ok(format!("UPDATE {target} SET {} WHERE {}", assignments.join(", "), where_clause(kind, key, b)?))
        }
        RowChange::Insert { values } => {
            if values.is_empty() {
                return Ok(match kind {
                    DbKind::Mysql | DbKind::Mariadb => format!("INSERT INTO {target} () VALUES ()"),
                    _ => format!("INSERT INTO {target} DEFAULT VALUES"),
                });
            }
            let cols: Vec<String> = values.iter().map(|(c, _)| quote_ident(kind, c)).collect();
            let mut vals = Vec::with_capacity(values.len());
            for (_, v) in values {
                vals.push(b.value(v)?);
            }
            Ok(format!("INSERT INTO {target} ({}) VALUES ({})", cols.join(", "), vals.join(", ")))
        }
        RowChange::Delete { key } => Ok(format!("DELETE FROM {target} WHERE {}", where_clause(kind, key, b)?)),
    }
}

/// Every change's statement, in order.
pub fn statements(kind: DbKind, schema: &str, table: &str, changes: &[RowChange]) -> DbResult<Vec<String>> {
    changes.iter().map(|c| statement(kind, schema, table, c)).collect()
}

/// The statement that opens a transaction.
pub fn begin_statement(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Mysql | DbKind::Mariadb => "START TRANSACTION",
        DbKind::Sqlite | DbKind::Postgres | DbKind::DuckDb | DbKind::ClickHouse | DbKind::Redis => "BEGIN",
        DbKind::SqlServer => "BEGIN TRANSACTION",
    }
}

/// Why a submit failed. Unless `rolled_back` is false, nothing was saved and the pending changes
/// can be fixed and submitted again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyFailure {
    /// The change that failed (index into the changes), when one did.
    pub index: Option<usize>,
    /// Its statement.
    pub statement: Option<String>,
    /// The server's error (or why nothing ran).
    pub error: String,
    /// Everything was rolled back. False only when the outcome is unknown (the connection was
    /// lost during COMMIT, or rolling back failed).
    pub rolled_back: bool,
}

impl std::fmt::Display for ApplyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(i) = self.index {
            write!(f, "change {} failed: ", i + 1)?;
        }
        f.write_str(&self.error)?;
        if let Some(sql) = &self.statement {
            write!(f, "\n  {sql}")?;
        }
        if self.rolled_back {
            f.write_str("\nNothing was saved: the changes were rolled back and are still pending.")?;
        }
        Ok(())
    }
}

/// A successful submit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    /// Rows changed.
    pub affected: u64,
    /// The changes joined a transaction that was already open: its Commit saves them.
    pub in_transaction: bool,
}

/// The savepoint that makes a submit inside an open transaction all-or-nothing.
const SUBMIT_SAVEPOINT: &str = "codecraft_submit";

fn lost(conn: &mut dyn Connection, e: &DbError) -> String {
    if conn.is_broken() {
        format!("the connection was lost ({e}); the server rolled back the transaction")
    } else {
        e.to_string()
    }
}

/// Apply changes in one transaction: all or nothing. An UPDATE or DELETE that does not hit
/// exactly one row (changed or deleted meanwhile) rolls everything back. Returns the rows
/// affected.
pub fn apply(conn: &mut dyn Connection, schema: &str, table: &str, changes: &[RowChange]) -> DbResult<u64> {
    apply_changes(conn, schema, table, changes, false).map(|r| r.affected).map_err(|f| DbError::Query(f.to_string()))
}

/// [`apply`] with the details: which change failed and whether everything was rolled back.
/// `in_tx`: the caller knows a transaction is open on the connection (used when the driver
/// cannot tell). Inside an open transaction the changes run under a savepoint, so a failure
/// undoes just them and success leaves them for the transaction's Commit; otherwise they run
/// in their own transaction.
pub fn apply_changes(
    conn: &mut dyn Connection,
    schema: &str,
    table: &str,
    changes: &[RowChange],
    in_tx: bool,
) -> Result<ApplyReport, ApplyFailure> {
    let kind = conn.kind();
    let fail = |index: Option<usize>, statement: Option<String>, error: String, rolled_back: bool| ApplyFailure {
        index,
        statement,
        error,
        rolled_back,
    };
    let mut sqls = Vec::with_capacity(changes.len());
    for (i, c) in changes.iter().enumerate() {
        sqls.push(bound_statement(kind, schema, table, c).map_err(|e| fail(Some(i), None, e.to_string(), true))?);
    }
    if sqls.is_empty() {
        return Ok(ApplyReport::default());
    }
    if !kind.supports_transactions() {
        // All-or-nothing needs a transaction.
        return Err(fail(None, None, format!("cannot edit data in the grid: {}", tx::no_transactions(kind)), true));
    }
    let outer = match conn.transaction_status() {
        TxStatus::Failed => {
            return Err(fail(
                None,
                None,
                "the connection's transaction has failed; roll it back before submitting".into(),
                true,
            ));
        }
        TxStatus::Active => true,
        TxStatus::Idle => false,
        TxStatus::Unknown => in_tx,
    };
    let started = if outer { tx::savepoint(conn, SUBMIT_SAVEPOINT) } else { tx::begin(conn, None) };
    if let Err(e) = started {
        let error = format!("cannot start a transaction: {}", lost(conn, &e));
        return Err(fail(None, None, error, true));
    }
    let mut total = 0u64;
    let mut failure: Option<(usize, String, String)> = None;
    for (i, (bound, change)) in sqls.iter().zip(changes).enumerate() {
        let sql = bound.preview(kind);
        match conn.execute_bound(&bound.sql, &bound.params, 1, &mut Discard) {
            Ok(summary) => {
                let n = summary.rows_affected.unwrap_or(0);
                total = total.saturating_add(n);
                // MySQL reports 0 for an UPDATE that sets the values a row already has.
                let mysql_update =
                    matches!(kind, DbKind::Mysql | DbKind::Mariadb) && matches!(change, RowChange::Update { .. });
                let keyed = !matches!(change, RowChange::Insert { .. });
                if keyed && (n > 1 || n == 0 && !mysql_update) {
                    let why = if n == 0 {
                        "the row was changed or deleted by someone else".to_string()
                    } else {
                        format!("{n} rows matched one row's key")
                    };
                    failure = Some((i, sql, why));
                    break;
                }
            }
            Err(e) => {
                let why = lost(conn, &e);
                failure = Some((i, sql, why));
                break;
            }
        }
    }
    if let Some((i, sql, why)) = failure {
        let undo = if conn.is_broken() {
            Ok(())
        } else if outer {
            tx::rollback_to(conn, SUBMIT_SAVEPOINT).and_then(|()| tx::release(conn, SUBMIT_SAVEPOINT))
        } else {
            tx::run(conn, "ROLLBACK")
        };
        return Err(match undo {
            Ok(()) => fail(Some(i), Some(sql), why, true),
            Err(e) => fail(Some(i), Some(sql), format!("{why}; rolling back failed too: {e}"), false),
        });
    }
    if outer {
        return match tx::release(conn, SUBMIT_SAVEPOINT) {
            Ok(()) => Ok(ApplyReport { affected: total, in_transaction: true }),
            Err(e) => {
                let rb = tx::rollback_to(conn, SUBMIT_SAVEPOINT).is_ok();
                Err(fail(None, None, format!("cannot release the savepoint: {}", lost(conn, &e)), rb))
            }
        };
    }
    match tx::end(conn, true) {
        Ok(_) => Ok(ApplyReport { affected: total, in_transaction: false }),
        Err(e) => {
            let unknown = conn.is_broken();
            Err(fail(None, Some("COMMIT".into()), e.to_string(), !unknown))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: i64) -> Vec<(String, Cell)> {
        vec![("id".into(), Cell::Int(id))]
    }

    #[test]
    fn literals_per_dialect() {
        let t = |s: &str| Cell::Text(s.into());
        assert_eq!(literal(DbKind::Postgres, &t("it's")).unwrap(), "'it''s'");
        assert_eq!(literal(DbKind::Postgres, &t("a\\b'")).unwrap(), "E'a\\\\b'''");
        assert!(literal(DbKind::Postgres, &t("a\0")).is_err());
        assert_eq!(literal(DbKind::Mysql, &t("it's")).unwrap(), "'it''s'");
        assert_eq!(literal(DbKind::Mysql, &t("a\\")).unwrap(), "CONVERT(X'615C' USING utf8mb4)");
        assert_eq!(literal(DbKind::Sqlite, &t("a\\'")).unwrap(), "'a\\'''");
        assert_eq!(literal(DbKind::Sqlite, &t("\0")).unwrap(), "CAST(X'00' AS TEXT)");
        assert_eq!(literal(DbKind::Sqlite, &Cell::Bool(true)).unwrap(), "1");
        assert_eq!(literal(DbKind::Postgres, &Cell::Bool(false)).unwrap(), "FALSE");
        assert_eq!(literal(DbKind::Mysql, &Cell::Null).unwrap(), "NULL");
        assert_eq!(literal(DbKind::Sqlite, &Cell::Float(2.0)).unwrap(), "2.0");
        assert_eq!(literal(DbKind::Sqlite, &Cell::Float(0.25)).unwrap(), "0.25");
        assert_eq!(literal(DbKind::Postgres, &Cell::Float(f64::NAN)).unwrap(), "'NaN'::float8");
        assert!(literal(DbKind::Sqlite, &Cell::Float(f64::INFINITY)).is_err());
        assert_eq!(literal(DbKind::Postgres, &Cell::Bytes(vec![0xAB, 1])).unwrap(), "decode('AB01', 'hex')");
        assert_eq!(literal(DbKind::Mysql, &Cell::Bytes(vec![0xAB])).unwrap(), "X'AB'");
        assert_eq!(literal(DbKind::Postgres, &Cell::UInt(u64::MAX)).unwrap(), u64::MAX.to_string());
    }

    #[test]
    fn statements_per_dialect() {
        let up = RowChange::Update { key: key(3), set: vec![("na\"me".into(), Cell::Text("O'Neil".into()))] };
        assert_eq!(
            statement(DbKind::Postgres, "public", "users", &up).unwrap(),
            "UPDATE \"public\".\"users\" SET \"na\"\"me\" = 'O''Neil' WHERE \"id\" = 3"
        );
        assert_eq!(
            statement(DbKind::Mysql, "db", "users", &up).unwrap(),
            "UPDATE `db`.`users` SET `na\"me` = 'O''Neil' WHERE `id` = 3"
        );
        let del = RowChange::Delete { key: vec![("a".into(), Cell::Int(1)), ("b".into(), Cell::Text("x".into()))] };
        assert_eq!(
            statement(DbKind::Sqlite, "", "t", &del).unwrap(),
            "DELETE FROM \"t\" WHERE \"a\" = 1 AND \"b\" = 'x'"
        );
        let ins = RowChange::Insert { values: vec![("a".into(), Cell::Int(1)), ("b".into(), Cell::Null)] };
        assert_eq!(
            statement(DbKind::Sqlite, "main", "t", &ins).unwrap(),
            "INSERT INTO \"main\".\"t\" (\"a\", \"b\") VALUES (1, NULL)"
        );
        let empty = RowChange::Insert { values: Vec::new() };
        assert_eq!(statement(DbKind::Postgres, "", "t", &empty).unwrap(), "INSERT INTO \"t\" DEFAULT VALUES");
        assert_eq!(statement(DbKind::Mariadb, "", "t", &empty).unwrap(), "INSERT INTO `t` () VALUES ()");
        assert!(statement(DbKind::Sqlite, "", "t", &RowChange::Delete { key: Vec::new() }).is_err());
        assert!(statement(DbKind::Sqlite, "", "t", &RowChange::Update { key: key(1), set: Vec::new() }).is_err());
    }

    #[test]
    fn applied_statements_bind_their_values() {
        let up = RowChange::Update {
            key: vec![("id".into(), Cell::Int(3)), ("k".into(), Cell::Null)],
            set: vec![("name".into(), Cell::Text("O'Neil".into())), ("n".into(), Cell::Null)],
        };
        let b = bound_statement(DbKind::Postgres, "public", "users", &up).unwrap();
        assert_eq!(b.sql, "UPDATE \"public\".\"users\" SET \"name\" = $1, \"n\" = NULL WHERE \"id\" = $2 AND \"k\" IS NULL");
        assert_eq!(b.params, vec![Cell::Text("O'Neil".into()), Cell::Int(3)]);
        let b = bound_statement(DbKind::Mysql, "db", "users", &up).unwrap();
        assert_eq!(b.sql, "UPDATE `db`.`users` SET `name` = ?, `n` = NULL WHERE `id` = ? AND `k` IS NULL");
        let b = bound_statement(DbKind::SqlServer, "dbo", "users", &up).unwrap();
        assert_eq!(b.sql, "UPDATE [dbo].[users] SET [name] = @P1, [n] = NULL WHERE [id] = @P2 AND [k] IS NULL");
        let ins = RowChange::Insert { values: vec![("a".into(), Cell::Bytes(vec![0, 1])), ("b".into(), Cell::Int(1))] };
        let b = bound_statement(DbKind::Sqlite, "", "t", &ins).unwrap();
        assert_eq!(b.sql, "INSERT INTO \"t\" (\"a\", \"b\") VALUES (?1, ?2)");
        // Drivers that cannot bind keep escaped literals, and the text form matches.
        let b = bound_statement(DbKind::DuckDb, "", "t", &ins).unwrap();
        assert_eq!(b.sql, "INSERT INTO \"t\" (\"a\", \"b\") VALUES (from_hex('0001'), 1)");
        assert!(b.params.is_empty());
        let pg = bound_statement(DbKind::Postgres, "public", "users", &up).unwrap();
        assert_eq!(pg.inline(DbKind::Postgres).unwrap(), statement(DbKind::Postgres, "public", "users", &up).unwrap());
    }

    fn sqlite() -> (tempfile::TempDir, Box<dyn Connection>) {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            crate::DataSourceConfig { kind: DbKind::Sqlite, path: Some(dir.path().join("t.db")), ..Default::default() };
        let mut conn = crate::connect(&cfg, None).unwrap();
        crate::query_all(conn.as_mut(), "create table t (id integer primary key, name text, n real default 1.5)")
            .unwrap();
        crate::query_all(conn.as_mut(), "insert into t(id, name) values (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        (dir, conn)
    }

    fn rows(conn: &mut dyn Connection) -> Vec<Vec<Cell>> {
        crate::query_all(conn, "select id, name, n from t order by id").unwrap().1
    }

    #[test]
    fn apply_on_sqlite() {
        let (_dir, mut conn) = sqlite();
        let changes = vec![
            RowChange::Delete { key: key(2) },
            RowChange::Update { key: key(1), set: vec![("name".into(), Cell::Text("it's \\ \0 ok".into()))] },
            RowChange::Insert { values: vec![("name".into(), Cell::Text("new".into()))] },
            RowChange::Insert { values: Vec::new() },
        ];
        assert_eq!(apply(conn.as_mut(), "main", "t", &changes).unwrap(), 4);
        let r = rows(conn.as_mut());
        assert_eq!(r.len(), 4);
        assert_eq!(r[0][1], Cell::Text("it's \\ \0 ok".into()));
        assert_eq!(r[1][0], Cell::Int(3));
        assert_eq!((r[2][1].clone(), r[2][2].clone()), (Cell::Text("new".into()), Cell::Float(1.5)));
        assert_eq!(r[3][1], Cell::Null);
    }

    #[test]
    fn apply_is_all_or_nothing() {
        let (_dir, mut conn) = sqlite();
        let before = rows(conn.as_mut());
        // The second change hits no row: the first is rolled back too.
        let changes = vec![RowChange::Delete { key: key(1) }, RowChange::Delete { key: key(99) }];
        let e = apply(conn.as_mut(), "main", "t", &changes).unwrap_err();
        assert!(e.to_string().contains("changed or deleted"), "{e}");
        assert_eq!(rows(conn.as_mut()), before);
        // A failing statement (constraint) rolls back as well.
        let changes =
            vec![RowChange::Delete { key: key(1) }, RowChange::Insert { values: vec![("id".into(), Cell::Int(2))] }];
        let e = apply(conn.as_mut(), "main", "t", &changes).unwrap_err();
        assert!(e.to_string().contains("INSERT INTO"), "{e}");
        assert_eq!(rows(conn.as_mut()), before);
        // The connection is usable afterwards (no transaction left open).
        assert_eq!(apply(conn.as_mut(), "main", "t", &[RowChange::Delete { key: key(3) }]).unwrap(), 1);
        assert_eq!(apply(conn.as_mut(), "main", "t", &[]).unwrap(), 0);
    }
}
