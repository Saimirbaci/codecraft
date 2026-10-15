//! Savepoints per dialect and the savepoint stack of an open transaction, plus the server-side
//! statement timeout.
//!
//! - SQL: PostgreSQL, SQLite and MySQL/MariaDB use `SAVEPOINT x`, `ROLLBACK TO SAVEPOINT x` and
//!   `RELEASE SAVEPOINT x`; SQL Server uses `SAVE TRANSACTION x` and `ROLLBACK TRANSACTION x` and
//!   has no RELEASE (a savepoint lasts until the transaction ends). ClickHouse, Redis and DuckDB
//!   have no transactions, so no savepoints.
//! - [`SavepointStack`]: the names in creation order. Rolling back to a savepoint keeps it and
//!   drops the later ones; releasing drops it and the later ones; ending the transaction drops
//!   all. A name used twice shadows the older one (PostgreSQL, SQLite); MySQL replaces it, which
//!   the stack mirrors by moving the name to the top.
//! - [`effect`]: what a statement typed in a console does to the stack, so the indicator follows
//!   savepoints the user wrote by hand.
//! - [`server_timeout_sql`]: the session setting that makes the server itself stop long
//!   statements (PostgreSQL `statement_timeout`, MySQL `max_execution_time` for SELECTs, MariaDB
//!   `max_statement_time`, SQL Server `LOCK_TIMEOUT`), and the statement that restores the
//!   default.

use std::time::Duration;

use serde::Serialize;

use crate::lex::{Tok, tokens};
use crate::sql::quote_ident;
use crate::{DbError, DbKind, DbResult, Dialect};

/// Longest savepoint name (PostgreSQL's identifier limit; MySQL and SQL Server allow 64/32 —
/// SQL Server names longer than 32 characters are refused below).
pub const MAX_NAME: usize = 63;
/// Savepoints kept in a stack (older ones are still on the server; only the list is capped).
pub const MAX_STACK: usize = 1000;

/// Which savepoint operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SavepointOp {
    Set,
    RollbackTo,
    Release,
}

impl SavepointOp {
    pub fn parse(s: &str) -> Option<SavepointOp> {
        let k: String = s.chars().filter(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        match k.as_str() {
            "set" | "savepoint" | "create" => Some(SavepointOp::Set),
            "rollbackto" | "rollback" => Some(SavepointOp::RollbackTo),
            "release" => Some(SavepointOp::Release),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SavepointOp::Set => "Set savepoint",
            SavepointOp::RollbackTo => "Rollback to savepoint",
            SavepointOp::Release => "Release savepoint",
        }
    }
}

/// Whether the database has savepoints.
pub fn supported(kind: DbKind) -> bool {
    kind.supports_transactions()
}

/// Check a savepoint name (1 to 63 characters, no control characters; 32 on SQL Server).
pub fn check_name(kind: DbKind, name: &str) -> DbResult<()> {
    let n = name.chars().count();
    let max = if kind == DbKind::SqlServer { 32 } else { MAX_NAME };
    if n == 0 || n > max {
        return Err(DbError::Config(format!("savepoint names have 1 to {max} characters")));
    }
    if name.chars().any(char::is_control) {
        return Err(DbError::Config("a savepoint name cannot contain control characters".into()));
    }
    Ok(())
}

/// The statement for a savepoint operation. `None` when the database needs none (RELEASE on
/// SQL Server: the savepoint just stops being listed).
pub fn statement(kind: DbKind, op: SavepointOp, name: &str) -> DbResult<Option<String>> {
    if !supported(kind) {
        return Err(DbError::Unsupported(format!("{} has no savepoints (no transactions)", kind.label())));
    }
    check_name(kind, name)?;
    let id = quote_ident(kind, name);
    Ok(match (kind, op) {
        (DbKind::SqlServer, SavepointOp::Set) => Some(format!("SAVE TRANSACTION {id}")),
        (DbKind::SqlServer, SavepointOp::RollbackTo) => Some(format!("ROLLBACK TRANSACTION {id}")),
        (DbKind::SqlServer, SavepointOp::Release) => None,
        (_, SavepointOp::Set) => Some(format!("SAVEPOINT {id}")),
        (_, SavepointOp::RollbackTo) => Some(format!("ROLLBACK TO SAVEPOINT {id}")),
        (_, SavepointOp::Release) => Some(format!("RELEASE SAVEPOINT {id}")),
    })
}

/// A name for a new savepoint that the stack does not use yet (`sp1`, `sp2`, …).
pub fn next_name(stack: &SavepointStack) -> String {
    let mut n = stack.len() + 1;
    loop {
        let name = format!("sp{n}");
        if !stack.contains(&name) {
            return name;
        }
        n += 1;
    }
}

/// The savepoints of the open transaction, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SavepointStack {
    names: Vec<String>,
}

impl SavepointStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The newest savepoint called `name` (compared case-insensitively, as unquoted
    /// identifiers fold on every supported server).
    fn position(&self, name: &str) -> Option<usize> {
        self.names.iter().rposition(|n| n.eq_ignore_ascii_case(name))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.position(name).is_some()
    }

    /// The transaction ended (or the connection dropped): no savepoints.
    pub fn clear(&mut self) {
        self.names.clear();
    }

    /// Apply an operation that succeeded on the server. `replace`: a name used again replaces
    /// the older savepoint (MySQL) rather than shadowing it.
    pub fn apply(&mut self, op: SavepointOp, name: &str, replace: bool) {
        match op {
            SavepointOp::Set => {
                if replace && let Some(i) = self.position(name) {
                    self.names.remove(i);
                }
                self.names.push(name.to_string());
                if self.names.len() > MAX_STACK {
                    let extra = self.names.len() - MAX_STACK;
                    self.names.drain(..extra);
                }
            }
            SavepointOp::RollbackTo => {
                if let Some(i) = self.position(name) {
                    self.names.truncate(i + 1);
                }
            }
            SavepointOp::Release => {
                if let Some(i) = self.position(name) {
                    self.names.truncate(i);
                }
            }
        }
    }

    /// Whether a rollback to or release of `name` can work (it is in the stack). The server
    /// is the judge for names the stack never saw (e.g. set before the list was capped).
    pub fn check(&self, op: SavepointOp, name: &str) -> Result<(), String> {
        if op == SavepointOp::Set || self.contains(name) {
            Ok(())
        } else {
            Err(format!("no savepoint `{name}` in the open transaction"))
        }
    }
}

/// Whether a repeated savepoint name replaces the older one (MySQL/MariaDB) instead of
/// shadowing it.
pub fn replaces(kind: DbKind) -> bool {
    matches!(kind, DbKind::Mysql | DbKind::Mariadb)
}

/// What a statement does to the savepoints, when it is a savepoint statement: `SAVEPOINT x`,
/// `ROLLBACK [WORK|TRANSACTION] TO [SAVEPOINT] x`, `RELEASE [SAVEPOINT] x`, and T-SQL's
/// `SAVE TRAN[SACTION] x` / `ROLLBACK TRAN[SACTION] x`.
pub fn effect(sql: &str, dialect: Dialect) -> Option<(SavepointOp, String)> {
    if dialect == Dialect::Redis {
        return None;
    }
    let chars: Vec<char> = sql.chars().collect();
    let toks = tokens(&chars, dialect);
    let text = |a: usize, b: usize| chars.get(a..b).unwrap_or(&[]).iter().collect::<String>();
    let word = |i: usize| match toks.get(i) {
        Some(Tok::Word(a, b)) => Some(text(*a, *b).to_ascii_uppercase()),
        _ => None,
    };
    let name = |i: usize| -> Option<String> {
        match toks.get(i)? {
            Tok::Word(a, b) => Some(text(*a, *b)),
            Tok::Literal(a, b) => {
                let raw = text(*a, *b);
                let mut it = raw.chars();
                let open = it.next()?;
                let close = match open {
                    '"' => '"',
                    '`' => '`',
                    '[' => ']',
                    _ => return None,
                };
                let inner = raw.strip_prefix(open)?.strip_suffix(close)?;
                let doubled: String = [close, close].iter().collect();
                Some(inner.replace(&doubled, &close.to_string()))
            }
            Tok::Punct(_) => None,
        }
    };
    // Only one statement (a trailing `;` is fine).
    let ends_at = |i: usize| {
        toks.get(i..).is_none_or(|rest| rest.iter().all(|t| matches!(t, Tok::Punct(p) if chars.get(*p) == Some(&';'))))
    };
    let first = word(0)?;
    let tsql = dialect == Dialect::TSql;
    let found = match first.as_str() {
        "SAVEPOINT" if !tsql => Some((SavepointOp::Set, 1)),
        "SAVE" if tsql && matches!(word(1).as_deref(), Some("TRAN" | "TRANSACTION")) => Some((SavepointOp::Set, 2)),
        "RELEASE" if !tsql => {
            let at = if word(1).as_deref() == Some("SAVEPOINT") { 2 } else { 1 };
            Some((SavepointOp::Release, at))
        }
        "ROLLBACK" if tsql => {
            // `ROLLBACK TRAN x` names a savepoint (or the outermost named transaction; the
            // server's @@TRANCOUNT tells which). Plain `ROLLBACK [TRAN]` ends the transaction.
            matches!(word(1).as_deref(), Some("TRAN" | "TRANSACTION"))
                .then_some((SavepointOp::RollbackTo, 2))
                .filter(|(_, at)| name(*at).is_some())
        }
        "ROLLBACK" => {
            let mut at = 1;
            if matches!(word(at).as_deref(), Some("WORK" | "TRANSACTION")) {
                at += 1;
            }
            if word(at).as_deref() != Some("TO") {
                return None;
            }
            at += 1;
            if word(at).as_deref() == Some("SAVEPOINT") {
                at += 1;
            }
            Some((SavepointOp::RollbackTo, at))
        }
        _ => None,
    }?;
    let (op, at) = found;
    let n = name(at)?;
    ends_at(at + 1).then_some((op, n))
}

// ---------------------------------------------------------------- server-side timeout

/// The session statement that makes the server stop statements running longer than
/// `timeout` (`None`: back to the server default). `None` when the database has no such
/// setting (SQLite, ClickHouse, Redis, DuckDB: the client-side timeout covers them).
///
/// - PostgreSQL: `SET statement_timeout = <ms>`; `RESET statement_timeout` restores the role's
///   or database's own default.
/// - MySQL: `SET SESSION max_execution_time = <ms>`; it applies to read-only SELECTs only.
///   `= DEFAULT` restores the global value.
/// - MariaDB: `SET SESSION max_statement_time = <seconds>` (`= DEFAULT` restores).
/// - SQL Server: `SET LOCK_TIMEOUT <ms>` (-1 restores waiting forever); the client-side query
///   timeout cancels the statement itself.
pub fn server_timeout_sql(kind: DbKind, timeout: Option<Duration>) -> Option<String> {
    // Whole milliseconds, at least 1 when set, and within what every server accepts.
    let ms = timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX).clamp(1, 2_147_483_647));
    match kind {
        DbKind::Postgres => Some(match ms {
            Some(m) => format!("SET statement_timeout = {m}"),
            None => "RESET statement_timeout".into(),
        }),
        DbKind::Mysql => Some(match ms {
            Some(m) => format!("SET SESSION max_execution_time = {m}"),
            None => "SET SESSION max_execution_time = DEFAULT".into(),
        }),
        DbKind::Mariadb => Some(match ms {
            Some(m) => format!("SET SESSION max_statement_time = {}", m as f64 / 1000.0),
            None => "SET SESSION max_statement_time = DEFAULT".into(),
        }),
        DbKind::SqlServer => Some(match ms {
            Some(m) => format!("SET LOCK_TIMEOUT {m}"),
            None => "SET LOCK_TIMEOUT -1".into(),
        }),
        DbKind::Sqlite | DbKind::ClickHouse | DbKind::Redis | DbKind::DuckDb => None,
    }
}

/// What the server-side timeout covers, for the UI.
pub fn server_timeout_note(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "statement_timeout (every statement)",
        DbKind::Mysql => "max_execution_time (read-only SELECT statements)",
        DbKind::Mariadb => "max_statement_time (every statement)",
        DbKind::SqlServer => "LOCK_TIMEOUT (lock waits); the client cancels long statements",
        DbKind::Sqlite | DbKind::ClickHouse | DbKind::Redis | DbKind::DuckDb => "client-side only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_per_dialect() {
        assert_eq!(statement(DbKind::Postgres, SavepointOp::Set, "a b").unwrap().unwrap(), "SAVEPOINT \"a b\"");
        assert_eq!(
            statement(DbKind::Mysql, SavepointOp::RollbackTo, "x").unwrap().unwrap(),
            "ROLLBACK TO SAVEPOINT `x`"
        );
        assert_eq!(statement(DbKind::Sqlite, SavepointOp::Release, "x").unwrap().unwrap(), "RELEASE SAVEPOINT \"x\"");
        assert_eq!(statement(DbKind::SqlServer, SavepointOp::Set, "x").unwrap().unwrap(), "SAVE TRANSACTION [x]");
        assert_eq!(
            statement(DbKind::SqlServer, SavepointOp::RollbackTo, "x]y").unwrap().unwrap(),
            "ROLLBACK TRANSACTION [x]]y]"
        );
        assert_eq!(statement(DbKind::SqlServer, SavepointOp::Release, "x").unwrap(), None);
        assert!(statement(DbKind::ClickHouse, SavepointOp::Set, "x").is_err());
        assert!(statement(DbKind::Postgres, SavepointOp::Set, "").is_err());
        assert!(statement(DbKind::Postgres, SavepointOp::Set, &"x".repeat(64)).is_err());
        assert!(statement(DbKind::SqlServer, SavepointOp::Set, &"x".repeat(33)).is_err());
        assert!(statement(DbKind::Postgres, SavepointOp::Set, "a\nb").is_err());
        assert_eq!(SavepointOp::parse("rollback-to"), Some(SavepointOp::RollbackTo));
        assert_eq!(SavepointOp::parse("set"), Some(SavepointOp::Set));
        assert_eq!(SavepointOp::parse("nope"), None);
    }

    #[test]
    fn stack_follows_operations() {
        let mut s = SavepointStack::new();
        for n in ["a", "b", "c"] {
            s.apply(SavepointOp::Set, n, false);
        }
        s.apply(SavepointOp::RollbackTo, "B", false);
        assert_eq!(s.names(), ["a", "b"]);
        s.apply(SavepointOp::Set, "a", false);
        assert_eq!(s.names(), ["a", "b", "a"]);
        s.apply(SavepointOp::Release, "a", false);
        assert_eq!(s.names(), ["a", "b"]);
        s.apply(SavepointOp::Set, "a", true);
        assert_eq!(s.names(), ["b", "a"]);
        assert!(s.check(SavepointOp::Release, "zzz").is_err());
        assert!(s.check(SavepointOp::Set, "zzz").is_ok());
        assert_eq!(next_name(&s), "sp3");
        s.apply(SavepointOp::Release, "missing", false);
        assert_eq!(s.len(), 2);
        s.clear();
        assert!(s.is_empty());
        for i in 0..(MAX_STACK + 5) {
            s.apply(SavepointOp::Set, &format!("s{i}"), false);
        }
        assert_eq!(s.len(), MAX_STACK);
    }

    #[test]
    fn statements_typed_in_a_console() {
        let e = |sql: &str, d: Dialect| effect(sql, d);
        assert_eq!(e("savepoint a;", Dialect::Postgres), Some((SavepointOp::Set, "a".into())));
        assert_eq!(e("SAVEPOINT \"Odd \"\"x\"", Dialect::Postgres), Some((SavepointOp::Set, "Odd \"x".into())));
        assert_eq!(e("rollback to savepoint a", Dialect::Mysql), Some((SavepointOp::RollbackTo, "a".into())));
        assert_eq!(e("ROLLBACK WORK TO a", Dialect::Postgres), Some((SavepointOp::RollbackTo, "a".into())));
        assert_eq!(e("release a", Dialect::Sqlite), Some((SavepointOp::Release, "a".into())));
        assert_eq!(e("RELEASE SAVEPOINT `b`", Dialect::Mysql), Some((SavepointOp::Release, "b".into())));
        assert_eq!(e("SAVE TRAN [p 1]", Dialect::TSql), Some((SavepointOp::Set, "p 1".into())));
        assert_eq!(e("rollback transaction p", Dialect::TSql), Some((SavepointOp::RollbackTo, "p".into())));
        assert_eq!(e("rollback transaction", Dialect::TSql), None);
        assert_eq!(e("ROLLBACK", Dialect::Postgres), None);
        assert_eq!(e("select 'savepoint a'", Dialect::Postgres), None);
        assert_eq!(e("savepoint a; select 1", Dialect::Postgres), None);
        assert_eq!(e("PING", Dialect::Redis), None);
        for junk in ["", ";", "savepoint", "rollback to", "release", "SAVE TRAN", "\"", "rollback to savepoint 'x"] {
            let _ = effect(junk, Dialect::Postgres);
            let _ = effect(junk, Dialect::TSql);
        }
    }

    #[test]
    fn server_timeouts() {
        let t = Some(Duration::from_millis(2500));
        assert_eq!(server_timeout_sql(DbKind::Postgres, t).unwrap(), "SET statement_timeout = 2500");
        assert_eq!(server_timeout_sql(DbKind::Postgres, None).unwrap(), "RESET statement_timeout");
        assert_eq!(server_timeout_sql(DbKind::Mysql, t).unwrap(), "SET SESSION max_execution_time = 2500");
        assert_eq!(server_timeout_sql(DbKind::Mariadb, t).unwrap(), "SET SESSION max_statement_time = 2.5");
        assert_eq!(server_timeout_sql(DbKind::Mariadb, None).unwrap(), "SET SESSION max_statement_time = DEFAULT");
        assert_eq!(server_timeout_sql(DbKind::SqlServer, t).unwrap(), "SET LOCK_TIMEOUT 2500");
        assert_eq!(server_timeout_sql(DbKind::SqlServer, None).unwrap(), "SET LOCK_TIMEOUT -1");
        assert_eq!(server_timeout_sql(DbKind::Sqlite, t), None);
        assert_eq!(
            server_timeout_sql(DbKind::Postgres, Some(Duration::from_secs(u64::MAX / 4))).unwrap(),
            "SET statement_timeout = 2147483647"
        );
        assert_eq!(
            server_timeout_sql(DbKind::Postgres, Some(Duration::from_micros(10))).unwrap(),
            "SET statement_timeout = 1"
        );
        assert!(!server_timeout_note(DbKind::Mysql).is_empty());
    }
}
