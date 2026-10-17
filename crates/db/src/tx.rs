//! Transactions: isolation levels per dialect, the statements that begin a transaction, the
//! transaction status of a connection (open, failed), savepoints, and a statement-timeout
//! watchdog that cancels a statement running longer than allowed.
//!
//! Robustness rules the engine builds on:
//! - the status comes from the server when the driver can ask ([`Connection::transaction_status`]:
//!   SQLite's autocommit flag, PostgreSQL's transaction timestamps, which also reveal a failed
//!   transaction), and from the statements run otherwise (MySQL);
//! - a failed PostgreSQL transaction is never "committed": COMMIT there silently rolls back, so
//!   [`end`] rolls back and says so;
//! - an operation that must be all-or-nothing inside an already open transaction uses a
//!   savepoint instead of BEGIN/COMMIT (MySQL's START TRANSACTION would commit the open one).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use serde::{Deserialize, Serialize};

use crate::{Canceller, Column, Connection, DbError, DbKind, DbResult, Page, ResultSink};

/// Transaction isolation levels (DataGrip's Tx selector).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Isolation {
    pub const ALL: [Isolation; 4] =
        [Isolation::ReadUncommitted, Isolation::ReadCommitted, Isolation::RepeatableRead, Isolation::Serializable];

    pub fn id(self) -> &'static str {
        match self {
            Isolation::ReadUncommitted => "read-uncommitted",
            Isolation::ReadCommitted => "read-committed",
            Isolation::RepeatableRead => "repeatable-read",
            Isolation::Serializable => "serializable",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Isolation::ReadUncommitted => "Read Uncommitted",
            Isolation::ReadCommitted => "Read Committed",
            Isolation::RepeatableRead => "Repeatable Read",
            Isolation::Serializable => "Serializable",
        }
    }

    /// The SQL words (`READ COMMITTED`).
    pub fn sql(self) -> &'static str {
        match self {
            Isolation::ReadUncommitted => "READ UNCOMMITTED",
            Isolation::ReadCommitted => "READ COMMITTED",
            Isolation::RepeatableRead => "REPEATABLE READ",
            Isolation::Serializable => "SERIALIZABLE",
        }
    }

    /// `read-committed`, `Read Committed`, `READ_COMMITTED`, `rc` …
    pub fn parse(s: &str) -> Option<Isolation> {
        let k: String = s.trim().to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphabetic()).collect();
        match k.as_str() {
            "readuncommitted" | "ru" => Some(Isolation::ReadUncommitted),
            "readcommitted" | "rc" => Some(Isolation::ReadCommitted),
            "repeatableread" | "rr" => Some(Isolation::RepeatableRead),
            "serializable" | "s" => Some(Isolation::Serializable),
            _ => None,
        }
    }

    /// The levels a database offers. SQLite transactions are always serializable; PostgreSQL
    /// accepts Read Uncommitted (it behaves as Read Committed).
    pub fn supported(kind: DbKind) -> &'static [Isolation] {
        match kind {
            DbKind::Sqlite => &[Isolation::Serializable],
            DbKind::Postgres | DbKind::Mysql | DbKind::Mariadb | DbKind::SqlServer => &Isolation::ALL,
            // No transactions (see `DbKind::supports_transactions`).
            DbKind::ClickHouse | DbKind::Redis | DbKind::DuckDb => &[],
        }
    }

    /// The server's default level.
    pub fn default_for(kind: DbKind) -> Isolation {
        match kind {
            DbKind::Sqlite | DbKind::DuckDb => Isolation::Serializable,
            DbKind::Postgres | DbKind::SqlServer | DbKind::ClickHouse | DbKind::Redis => Isolation::ReadCommitted,
            DbKind::Mysql | DbKind::Mariadb => Isolation::RepeatableRead,
        }
    }
}

/// The statement that makes `level` the session's isolation (for every later transaction,
/// auto-commit or not). `None` for SQLite (always serializable).
pub fn session_isolation_sql(kind: DbKind, level: Isolation) -> DbResult<Option<String>> {
    if !Isolation::supported(kind).contains(&level) {
        return Err(DbError::Unsupported(format!("{} does not support {}", kind.label(), level.label())));
    }
    Ok(match kind {
        DbKind::Sqlite | DbKind::ClickHouse | DbKind::Redis | DbKind::DuckDb => None,
        DbKind::Postgres => Some(format!("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL {}", level.sql())),
        DbKind::Mysql | DbKind::Mariadb => Some(format!("SET SESSION TRANSACTION ISOLATION LEVEL {}", level.sql())),
        // Session-wide in T-SQL.
        DbKind::SqlServer => Some(format!("SET TRANSACTION ISOLATION LEVEL {}", level.sql())),
    })
}

/// The statements that open a transaction, at `level` when given.
pub fn begin_statements(kind: DbKind, level: Option<Isolation>) -> DbResult<Vec<String>> {
    if !kind.supports_transactions() {
        return Err(DbError::Unsupported(no_transactions(kind)));
    }
    if let Some(l) = level
        && !Isolation::supported(kind).contains(&l)
    {
        return Err(DbError::Unsupported(format!("{} does not support {}", kind.label(), l.label())));
    }
    Ok(match (kind, level) {
        (DbKind::Postgres, Some(l)) => vec![format!("BEGIN ISOLATION LEVEL {}", l.sql())],
        (DbKind::Mysql | DbKind::Mariadb, Some(l)) => {
            // Without SESSION it applies to the next transaction only.
            vec![format!("SET TRANSACTION ISOLATION LEVEL {}", l.sql()), "START TRANSACTION".into()]
        }
        (DbKind::Mysql | DbKind::Mariadb, None) => vec!["START TRANSACTION".into()],
        (DbKind::Sqlite | DbKind::Postgres, _) => vec!["BEGIN".into()],
        (DbKind::SqlServer, Some(l)) => {
            vec![format!("SET TRANSACTION ISOLATION LEVEL {}", l.sql()), "BEGIN TRANSACTION".into()]
        }
        (DbKind::SqlServer, None) => vec!["BEGIN TRANSACTION".into()],
        (DbKind::ClickHouse | DbKind::Redis | DbKind::DuckDb, _) => {
            return Err(DbError::Unsupported(no_transactions(kind)));
        }
    })
}

/// Why a data source cannot open a transaction.
pub fn no_transactions(kind: DbKind) -> String {
    match kind {
        DbKind::DuckDb => "DuckDB data sources run each statement in its own process: no transactions \
                           (every statement commits)"
            .into(),
        _ => format!("{} has no transactions: every statement commits on its own", kind.label()),
    }
}

/// What is known about a connection's transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TxStatus {
    /// No transaction is open (auto-commit).
    Idle,
    /// A transaction is open.
    Active,
    /// A transaction is open but a statement failed in it: only ROLLBACK (or ROLLBACK TO
    /// SAVEPOINT) works (PostgreSQL).
    Failed,
    /// The driver cannot tell; track the statements instead.
    #[default]
    Unknown,
}

impl TxStatus {
    pub fn is_open(self) -> bool {
        matches!(self, TxStatus::Active | TxStatus::Failed)
    }
}

/// A sink that ignores rows.
pub struct Discard;

impl ResultSink for Discard {
    fn columns(&mut self, _: &[Column]) {}
    fn page(&mut self, _: Page) -> bool {
        true
    }
}

/// Run a statement that returns no rows of interest.
pub fn run(conn: &mut dyn Connection, sql: &str) -> DbResult<()> {
    conn.execute(sql, 1, &mut Discard).map(|_| ())
}

/// Open a transaction (at `level` when given).
pub fn begin(conn: &mut dyn Connection, level: Option<Isolation>) -> DbResult<()> {
    for sql in begin_statements(conn.kind(), level)? {
        run(conn, &sql)?;
    }
    Ok(())
}

/// How [`end`] ended a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TxEnd {
    Committed,
    RolledBack,
}

/// Commit or roll back. A transaction that already failed is rolled back even when asked to
/// commit, and that is an error: nothing was committed.
pub fn end(conn: &mut dyn Connection, commit: bool) -> DbResult<TxEnd> {
    if commit && conn.transaction_status() == TxStatus::Failed {
        let rb = run(conn, "ROLLBACK");
        let tail = match rb {
            Ok(()) => "it was rolled back".to_string(),
            Err(e) => format!("rolling it back failed too: {e}"),
        };
        return Err(DbError::Query(format!(
            "the transaction had failed (a statement in it raised an error), so nothing was committed; {tail}"
        )));
    }
    if commit {
        if let Err(e) = run(conn, "COMMIT") {
            if conn.is_broken() {
                return Err(DbError::Query(format!(
                    "the connection was lost during COMMIT: the outcome is unknown (the server rolls back unless \
                     it received the COMMIT); check the data before retrying: {e}"
                )));
            }
            // A failed COMMIT ends the transaction on PostgreSQL; make sure nothing stays open.
            if conn.transaction_status().is_open() {
                let _ = run(conn, "ROLLBACK");
            }
            return Err(DbError::Query(format!("COMMIT failed, the transaction was rolled back: {e}")));
        }
        Ok(TxEnd::Committed)
    } else {
        match run(conn, "ROLLBACK") {
            Ok(()) => Ok(TxEnd::RolledBack),
            // Without a connection the server has rolled back already.
            Err(_) if conn.is_broken() => Ok(TxEnd::RolledBack),
            Err(e) => Err(e),
        }
    }
}

/// Run a savepoint statement (see [`crate::savepoint::statement`]).
fn savepoint_op(conn: &mut dyn Connection, op: crate::savepoint::SavepointOp, name: &str) -> DbResult<()> {
    match crate::savepoint::statement(conn.kind(), op, name)? {
        Some(sql) => run(conn, &sql),
        None => Ok(()),
    }
}

pub fn savepoint(conn: &mut dyn Connection, name: &str) -> DbResult<()> {
    savepoint_op(conn, crate::savepoint::SavepointOp::Set, name)
}

pub fn rollback_to(conn: &mut dyn Connection, name: &str) -> DbResult<()> {
    savepoint_op(conn, crate::savepoint::SavepointOp::RollbackTo, name)
}

/// SQL Server has no RELEASE: a savepoint lasts until the transaction ends.
pub fn release(conn: &mut dyn Connection, name: &str) -> DbResult<()> {
    savepoint_op(conn, crate::savepoint::SavepointOp::Release, name)
}

// ---------------------------------------------------------------- statement timeout

enum Watch {
    Arm { ticket: u64, deadline: Instant },
    Disarm,
}

/// Cancels the running statement when it runs past its deadline. One per connection; the
/// timer thread ends when the watchdog is dropped.
pub struct Watchdog {
    tx: Sender<Watch>,
    ticket: AtomicU64,
    fired: Arc<AtomicU64>,
    timeout: Option<Duration>,
}

impl Watchdog {
    /// `timeout` `None` or zero: no timeout (the watchdog does nothing).
    pub fn new(canceller: Arc<dyn Canceller>, timeout: Option<Duration>) -> Self {
        let (tx, rx) = unbounded();
        let fired = Arc::new(AtomicU64::new(0));
        let timeout = timeout.filter(|t| !t.is_zero());
        if timeout.is_some() {
            let f = fired.clone();
            let _ = std::thread::Builder::new().name("db-timeout".into()).spawn(move || watch(rx, canceller, f));
        }
        Self { tx, ticket: AtomicU64::new(0), fired, timeout }
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Start timing a statement; returns its ticket for [`Watchdog::fired`].
    pub fn arm(&self) -> u64 {
        let ticket = self.ticket.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        if let Some(t) = self.timeout {
            let _ = self.tx.send(Watch::Arm { ticket, deadline: Instant::now() + t });
        }
        ticket
    }

    /// The statement produced its first rows (or finished): stop timing.
    pub fn disarm(&self) {
        if self.timeout.is_some() {
            let _ = self.tx.send(Watch::Disarm);
        }
    }

    /// Whether the watchdog cancelled the statement of `ticket`.
    pub fn fired(&self, ticket: u64) -> bool {
        ticket != 0 && self.fired.load(Ordering::SeqCst) == ticket
    }

    /// The error to report for a statement the watchdog cancelled.
    pub fn timeout_error(&self) -> DbError {
        let secs = self.timeout.map(|t| t.as_secs_f64()).unwrap_or(0.0);
        DbError::Query(format!("the statement timed out after {secs} s and was cancelled (query timeout)"))
    }
}

fn watch(rx: Receiver<Watch>, canceller: Arc<dyn Canceller>, fired: Arc<AtomicU64>) {
    let mut armed: Option<(u64, Instant)> = None;
    loop {
        let msg = match armed {
            Some((ticket, deadline)) => {
                let now = Instant::now();
                if now >= deadline {
                    fired.store(ticket, Ordering::SeqCst);
                    let _ = canceller.cancel();
                    armed = None;
                    continue;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => return,
            },
        };
        armed = match msg {
            Watch::Arm { ticket, deadline } => Some((ticket, deadline)),
            Watch::Disarm => None,
        };
    }
}

/// A sink that stops the watchdog at the first page (the timeout covers the time to the first
/// rows, like JDBC's query timeout; fetching later pages is not timed).
pub struct TimedSink<'a> {
    pub inner: &'a mut dyn ResultSink,
    pub watchdog: &'a Watchdog,
    disarmed: bool,
}

impl<'a> TimedSink<'a> {
    pub fn new(inner: &'a mut dyn ResultSink, watchdog: &'a Watchdog) -> Self {
        Self { inner, watchdog, disarmed: false }
    }
}

impl ResultSink for TimedSink<'_> {
    fn columns(&mut self, columns: &[Column]) {
        self.inner.columns(columns);
    }

    fn page(&mut self, page: Page) -> bool {
        if !self.disarmed {
            self.watchdog.disarm();
            self.disarmed = true;
        }
        self.inner.page(page)
    }

    fn next_result(&mut self) -> bool {
        self.inner.next_result()
    }
}

/// Run a statement under the watchdog: a statement it cancelled reports a timeout.
pub fn execute_timed(
    conn: &mut dyn Connection,
    watchdog: &Watchdog,
    sql: &str,
    page_size: usize,
    sink: &mut dyn ResultSink,
) -> DbResult<crate::ExecSummary> {
    execute_timed_bound(conn, watchdog, sql, &[], page_size, sink)
}

/// [`execute_timed`] with bind parameters ([`Connection::execute_bound`]).
pub fn execute_timed_bound(
    conn: &mut dyn Connection,
    watchdog: &Watchdog,
    sql: &str,
    params: &[crate::Cell],
    page_size: usize,
    sink: &mut dyn ResultSink,
) -> DbResult<crate::ExecSummary> {
    let ticket = watchdog.arm();
    let r = {
        let mut timed = TimedSink::new(sink, watchdog);
        conn.execute_bound(sql, params, page_size, &mut timed)
    };
    watchdog.disarm();
    match r {
        Err(DbError::Cancelled) if watchdog.fired(ticket) => Err(watchdog.timeout_error()),
        Err(DbError::Query(_)) if watchdog.fired(ticket) => Err(watchdog.timeout_error()),
        other => other,
    }
}

/// Seconds from a data source option (`query_timeout`, `keepalive`); `None` when unset or 0.
pub fn option_secs(options: &std::collections::BTreeMap<String, String>, key: &str) -> Option<Duration> {
    let v = options.get(key)?.trim().parse::<f64>().ok()?;
    (v.is_finite() && v > 0.0).then(|| Duration::from_secs_f64(v.min(86_400.0 * 7.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_names_and_sql() {
        for l in Isolation::ALL {
            assert_eq!(Isolation::parse(l.id()), Some(l));
            assert_eq!(Isolation::parse(l.label()), Some(l));
            assert_eq!(Isolation::parse(l.sql()), Some(l));
        }
        assert_eq!(Isolation::parse("READ_COMMITTED"), Some(Isolation::ReadCommitted));
        assert_eq!(Isolation::parse("snapshot"), None);
        assert_eq!(
            begin_statements(DbKind::Postgres, Some(Isolation::Serializable)).unwrap(),
            ["BEGIN ISOLATION LEVEL SERIALIZABLE"]
        );
        assert_eq!(
            begin_statements(DbKind::Mysql, Some(Isolation::ReadCommitted)).unwrap(),
            ["SET TRANSACTION ISOLATION LEVEL READ COMMITTED", "START TRANSACTION"]
        );
        assert_eq!(begin_statements(DbKind::Sqlite, None).unwrap(), ["BEGIN"]);
        assert!(begin_statements(DbKind::Sqlite, Some(Isolation::ReadCommitted)).is_err());
        assert_eq!(session_isolation_sql(DbKind::Sqlite, Isolation::Serializable).unwrap(), None);
        assert_eq!(
            session_isolation_sql(DbKind::Mariadb, Isolation::RepeatableRead).unwrap().unwrap(),
            "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ"
        );
        assert!(session_isolation_sql(DbKind::Sqlite, Isolation::ReadUncommitted).is_err());
        assert_eq!(Isolation::default_for(DbKind::Postgres), Isolation::ReadCommitted);
    }

    #[test]
    fn options_parse_seconds() {
        let mut o = std::collections::BTreeMap::new();
        assert_eq!(option_secs(&o, "query_timeout"), None);
        o.insert("query_timeout".into(), " 2.5 ".into());
        o.insert("keepalive".into(), "0".into());
        o.insert("bad".into(), "x".into());
        assert_eq!(option_secs(&o, "query_timeout"), Some(Duration::from_millis(2500)));
        assert_eq!(option_secs(&o, "keepalive"), None);
        assert_eq!(option_secs(&o, "bad"), None);
    }

    struct CountCancel(Arc<AtomicU64>);
    impl Canceller for CountCancel {
        fn cancel(&self) -> DbResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn watchdog_fires_only_past_the_deadline() {
        let n = Arc::new(AtomicU64::new(0));
        let w = Watchdog::new(Arc::new(CountCancel(n.clone())), Some(Duration::from_millis(50)));
        let t = w.arm();
        w.disarm();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(!w.fired(t));
        let t = w.arm();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(w.fired(t));
        let off = Watchdog::new(Arc::new(CountCancel(n.clone())), None);
        assert!(!off.fired(off.arm()));
    }

    fn sqlite() -> (tempfile::TempDir, Box<dyn Connection>) {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            crate::DataSourceConfig { kind: DbKind::Sqlite, path: Some(dir.path().join("t.db")), ..Default::default() };
        let conn = crate::connect(&cfg, None).unwrap();
        (dir, conn)
    }

    #[test]
    fn sqlite_status_savepoints_and_end() {
        let (_d, mut c) = sqlite();
        run(c.as_mut(), "create table t (a int)").unwrap();
        assert_eq!(c.transaction_status(), TxStatus::Idle);
        begin(c.as_mut(), None).unwrap();
        assert_eq!(c.transaction_status(), TxStatus::Active);
        run(c.as_mut(), "insert into t values (1)").unwrap();
        savepoint(c.as_mut(), "sp 1").unwrap();
        run(c.as_mut(), "insert into t values (2)").unwrap();
        rollback_to(c.as_mut(), "sp 1").unwrap();
        release(c.as_mut(), "sp 1").unwrap();
        assert_eq!(end(c.as_mut(), true).unwrap(), TxEnd::Committed);
        assert_eq!(c.transaction_status(), TxStatus::Idle);
        let (_, rows, _) = crate::query_all(c.as_mut(), "select a from t").unwrap();
        assert_eq!(rows, vec![vec![crate::Cell::Int(1)]]);
        begin(c.as_mut(), Some(Isolation::Serializable)).unwrap();
        run(c.as_mut(), "insert into t values (3)").unwrap();
        assert_eq!(end(c.as_mut(), false).unwrap(), TxEnd::RolledBack);
        assert_eq!(crate::query_all(c.as_mut(), "select count(*) from t").unwrap().1[0][0], crate::Cell::Int(1));
        assert!(savepoint(c.as_mut(), "").is_err());
    }

    #[test]
    fn sqlite_timeout_cancels_a_long_statement() {
        let (_d, mut c) = sqlite();
        let w = Watchdog::new(c.canceller(), Some(Duration::from_millis(100)));
        let started = Instant::now();
        let e = execute_timed(
            c.as_mut(),
            &w,
            "with recursive n(i) as (select 1 union all select i + 1 from n) select count(*) from n",
            10,
            &mut Discard,
        )
        .unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
        // The connection works afterwards, and a quick statement is not affected.
        assert!(execute_timed(c.as_mut(), &w, "select 1", 10, &mut Discard).is_ok());
    }
}
