//! Transactions and connection robustness (track DB). OWNED BY the database track.
//!
//! - [`Link`]: a worker's connection. It applies the session isolation level, times statements
//!   out (a watchdog cancels them), pings idle connections (keep-alive, also through SSH
//!   tunnels), notices a dropped connection, and reconnects for the next request. A read that
//!   failed because the connection dropped before any row arrived is retried once on the new
//!   connection; a write, or anything inside an open transaction, is never retried: the user
//!   learns that the server rolled the transaction back.
//! - [`TxBook`]: what the UI shows: per connection whether a transaction is open (or failed),
//!   how many statements it ran and since when; per console the transaction mode (Auto/Manual)
//!   and isolation level; the pending-transaction guard.
//! - The guard ([`intercept`]): disconnecting, removing or editing a data source, closing a
//!   console, closing the project, opening another project in the window, closing a project
//!   window and quitting ask Commit / Rollback / Cancel while a transaction is open, and only go
//!   ahead once every transaction ended.
//! - Savepoints (track DX): Set / Rollback to / Release on the open transaction
//!   ([`LinkReq::Savepoint`]), and savepoints typed in the console, kept as a stack the
//!   transaction indicator shows; ending the transaction or losing the connection clears it.
//! - Server-side statement timeout (track DX): with a query timeout set, the session also gets
//!   the server's own limit ([`codecraft_db::savepoint::server_timeout_sql`]) so a statement
//!   stops even when the client cannot cancel it; clearing the timeout restores the default.
//!   PostgreSQL's `SET` is undone by a rollback, so it is applied again after each transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_db::savepoint::{SavepointOp, SavepointStack, server_timeout_sql};
use codecraft_db::tx::{Isolation, TxStatus, Watchdog, execute_timed, option_secs, session_isolation_sql};
use codecraft_db::{
    Canceller, Connection, DataSourceConfig, DbError, DbKind, ExecSummary, Page, ResultSink, SecretStore,
};

use super::{DbState, Event, OutputLevel, Req};
use crate::{CommandError, Session};

/// Ping an idle connection this often unless the data source says otherwise (`keepalive`).
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(60);
/// Before using a connection idle for this long, check it is still there.
const CHECK_IDLE: Duration = Duration::from_secs(20);

/// Per-connection settings sent to a worker.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LinkOpts {
    pub isolation: Option<Isolation>,
    pub timeout: Option<Duration>,
    pub keepalive: Option<Duration>,
    /// Also set the server's own statement timeout (option `server_timeout`, on by default).
    pub server_timeout: bool,
}

impl LinkOpts {
    /// From the data source's options (`isolation`, `query_timeout`, `keepalive` seconds; 0 turns
    /// keep-alive off).
    pub(crate) fn from_config(cfg: &DataSourceConfig) -> Self {
        let keepalive = match cfg.options.get("keepalive").map(|v| v.trim()) {
            Some("0") | Some("off") => None,
            _ if cfg.kind.is_file_based() => None,
            _ => option_secs(&cfg.options, "keepalive").or(Some(DEFAULT_KEEPALIVE)),
        };
        let server_timeout = !matches!(
            cfg.options.get("server_timeout").map(|v| v.trim().to_ascii_lowercase()).as_deref(),
            Some("0" | "off" | "false" | "no")
        );
        Self {
            isolation: cfg.options.get("isolation").and_then(|v| Isolation::parse(v)),
            timeout: option_secs(&cfg.options, "query_timeout"),
            keepalive,
            server_timeout,
        }
    }
}

/// Requests for a worker's [`Link`].
pub(crate) enum LinkReq {
    /// The session isolation level (`None`: the server default).
    SetIsolation(Option<Isolation>),
    SetTimeout(Option<Duration>),
    /// Set, roll back to or release a savepoint of the open transaction.
    Savepoint { op: SavepointOp, name: String, manual: bool },
}

/// What a worker reports about its connection and transaction.
pub(crate) enum TxEvent {
    State {
        worker: String,
        open: bool,
        failed: bool,
        statements: usize,
        isolation: Option<Isolation>,
        savepoints: Vec<String>,
    },
    /// The connection dropped. `in_tx`: a transaction was open (the server rolled it back).
    Lost {
        worker: String,
        in_tx: bool,
        error: String,
    },
    /// Reconnected after a drop.
    Reconnected {
        worker: String,
    },
    /// Something to log for the connection.
    Note {
        worker: String,
        error: bool,
        text: String,
    },
}

/// Recreates the routed config (a new SSH tunnel) when reconnecting directly fails.
pub(crate) type Reroute = Box<dyn Fn() -> Result<DataSourceConfig, String> + Send>;

/// A worker's connection with transaction tracking, timeouts, keep-alive and reconnect.
pub(crate) struct Link {
    pub key: String,
    cfg: DataSourceConfig,
    password: Option<String>,
    reroute: Option<Reroute>,
    conn: Option<Box<dyn Connection>>,
    watchdog: Option<Watchdog>,
    pub opts: LinkOpts,
    /// A transaction is open.
    pub in_tx: bool,
    /// It failed (only rollback works).
    pub failed: bool,
    /// Statements run in the open transaction.
    pub statements: usize,
    events: Sender<Event>,
    last_used: Instant,
    /// The connection dropped with a transaction open and no request has been told yet.
    lost_tx: bool,
    /// Savepoints of the open transaction.
    pub savepoints: SavepointStack,
    /// The server-side timeout the session has (`None`: unknown, set it before the next
    /// statement).
    server_timeout: Option<Option<Duration>>,
    /// A server-side timeout was set on this session at some point.
    server_timeout_touched: bool,
}

impl Link {
    pub(super) fn new(
        key: String,
        cfg: DataSourceConfig,
        password: Option<String>,
        reroute: Option<Reroute>,
        opts: LinkOpts,
        events: Sender<Event>,
    ) -> Self {
        Self {
            key,
            cfg,
            password,
            reroute,
            conn: None,
            watchdog: None,
            opts,
            in_tx: false,
            failed: false,
            statements: 0,
            events,
            last_used: Instant::now(),
            lost_tx: false,
            savepoints: SavepointStack::new(),
            server_timeout: Some(None),
            server_timeout_touched: false,
        }
    }

    pub(crate) fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    /// Connect (first time or again). Returns the server version.
    pub(crate) fn open(&mut self) -> Result<String, String> {
        self.conn = None;
        self.watchdog = None;
        let mut conn = match codecraft_db::connect(&self.cfg, self.password.as_deref()) {
            Ok(c) => c,
            Err(e) => match &self.reroute {
                // The tunnel may be what died: open a new one.
                Some(reroute) => {
                    let cfg = reroute().map_err(|t| format!("{e}; reopening the SSH tunnel failed: {t}"))?;
                    let c = codecraft_db::connect(&cfg, self.password.as_deref()).map_err(|e| e.to_string())?;
                    self.cfg = cfg;
                    c
                }
                None => return Err(e.to_string()),
            },
        };
        if let Some(level) = self.opts.isolation.filter(|_| self.cfg.kind.supports_transactions())
            && let Some(sql) = session_isolation_sql(self.cfg.kind, level).map_err(|e| e.to_string())?
        {
            codecraft_db::tx::run(conn.as_mut(), &sql).map_err(|e| format!("cannot set the isolation level: {e}"))?;
        }
        let version = conn.server_version().unwrap_or_else(|_| self.cfg.kind.label().to_string());
        self.watchdog = Some(Watchdog::new(conn.canceller(), self.opts.timeout));
        self.conn = Some(conn);
        self.in_tx = false;
        self.failed = false;
        self.statements = 0;
        self.savepoints.clear();
        self.last_used = Instant::now();
        // A new session has the server's default timeout.
        self.server_timeout = Some(None);
        self.server_timeout_touched = false;
        self.sync_server_timeout(false);
        Ok(version)
    }

    /// Give the session the server-side timeout the options ask for, if it does not have it.
    /// `say`: log the change (the user changed the timeout).
    pub(crate) fn sync_server_timeout(&mut self, say: bool) {
        let desired = self.opts.timeout.filter(|_| self.opts.server_timeout);
        if self.server_timeout == Some(desired) || self.failed {
            return;
        }
        let kind = self.cfg.kind;
        let Some(sql) = server_timeout_sql(kind, desired) else {
            self.server_timeout = Some(desired);
            return;
        };
        let Some(conn) = self.conn.as_mut() else { return };
        let r = codecraft_db::tx::run(conn.as_mut(), &sql);
        // Applied or not, do not retry every statement; a failure is said once.
        self.server_timeout = Some(desired);
        self.server_timeout_touched = true;
        match r {
            Ok(()) if say => self.send(TxEvent::Note {
                worker: self.key.clone(),
                error: false,
                text: format!("Server-side timeout: {sql}"),
            }),
            Ok(()) => {}
            Err(e) => self.send(TxEvent::Note {
                worker: self.key.clone(),
                error: true,
                text: format!("cannot set the server-side timeout ({sql}): {e}; the client-side timeout still applies"),
            }),
        }
    }

    /// The transaction is over: no savepoints; PostgreSQL may have undone a `SET` made in it.
    fn tx_closed(&mut self) {
        self.savepoints.clear();
        if self.cfg.kind == DbKind::Postgres && self.server_timeout_touched {
            self.server_timeout = None;
        }
    }

    pub(crate) fn canceller(&self) -> Option<Arc<dyn Canceller>> {
        self.conn.as_ref().map(|c| c.canceller())
    }

    fn send(&self, ev: TxEvent) {
        let _ = self.events.send(Event::Tx(ev));
    }

    /// The connection dropped: say so (and that the server rolled back an open transaction).
    fn lost(&mut self, error: &str) {
        let in_tx = self.in_tx;
        self.lost_tx |= in_tx;
        self.conn = None;
        self.watchdog = None;
        self.in_tx = false;
        self.failed = false;
        self.statements = 0;
        self.savepoints.clear();
        self.send(TxEvent::Lost { worker: self.key.clone(), in_tx, error: error.to_string() });
        self.report_state();
    }

    /// Make sure there is a live connection before a request. A connection found dead while a
    /// transaction was open fails the request: that transaction is gone and the request was
    /// meant for it.
    pub(crate) fn ready(&mut self) -> Result<(), String> {
        if self.conn.is_some() && self.last_used.elapsed() >= CHECK_IDLE {
            self.check();
        }
        if self.conn.is_none() {
            let was_in_tx = std::mem::take(&mut self.lost_tx) || self.in_tx;
            let version = self.open().map_err(|e| format!("cannot reconnect: {e}"))?;
            if let Some(c) = self.canceller() {
                let _ = self.events.send(Event::Connected { worker: self.key.clone(), version, canceller: c });
            }
            self.send(TxEvent::Reconnected { worker: self.key.clone() });
            if was_in_tx {
                return Err("the connection was lost; the server rolled back the transaction (nothing was \
                            retried)"
                    .into());
            }
        }
        Ok(())
    }

    /// Ping; a dead connection is reported as lost.
    pub(crate) fn check(&mut self) {
        let Some(conn) = self.conn.as_mut() else { return };
        let r = conn.ping();
        self.last_used = Instant::now();
        if let Err(e) = r
            && conn.is_broken()
        {
            self.lost(&e.to_string());
        }
    }

    /// After a request that used the connection directly: report it lost if it dropped.
    pub(crate) fn check_broken(&mut self) {
        if let Some(conn) = self.conn.as_mut()
            && conn.is_broken()
        {
            self.lost("the server closed the connection");
        }
    }

    /// The connection, after [`Link::ready`].
    pub(crate) fn conn(&mut self) -> Result<&mut dyn Connection, String> {
        self.last_used = Instant::now();
        match self.conn.as_mut() {
            Some(c) => Ok(c.as_mut()),
            None => Err("not connected".into()),
        }
    }

    /// Wait for the next request, pinging the connection when idle (keep-alive).
    pub(super) fn recv(&mut self, rx: &Receiver<Req>) -> Option<Req> {
        loop {
            let Some(every) = self.opts.keepalive.filter(|_| self.conn.is_some()) else {
                return rx.recv().ok();
            };
            match rx.recv_timeout(every) {
                Ok(r) => return Some(r),
                Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) => self.check(),
            }
        }
    }

    /// Run one statement (under the timeout). `retry_read`: the statement only reads, so if
    /// the connection dropped before any row arrived it is run again on a new connection.
    pub(crate) fn execute(
        &mut self,
        sql: &str,
        page_size: usize,
        sink: &mut dyn ResultSink,
        retry_read: bool,
    ) -> Result<ExecSummary, DbError> {
        for attempt in 0..2 {
            self.ready().map_err(DbError::Connect)?;
            self.sync_server_timeout(false);
            let (Some(conn), Some(watchdog)) = (self.conn.as_mut(), self.watchdog.as_ref()) else {
                return Err(DbError::Connect("not connected".into()));
            };
            let mut counted = CountingSink { inner: &mut *sink, pages: 0 };
            let r = execute_timed(conn.as_mut(), watchdog, sql, page_size, &mut counted);
            let pages = counted.pages;
            self.last_used = Instant::now();
            let Err(e) = r else { return r };
            if !conn.is_broken() {
                return Err(e);
            }
            let in_tx = self.in_tx;
            self.lost(&e.to_string());
            if in_tx {
                // This request carries the news; the next one runs on a new connection.
                self.lost_tx = false;
                return Err(DbError::Query(format!(
                    "the connection was lost ({e}); the server rolled back the transaction — nothing was retried"
                )));
            }
            if !retry_read || pages > 0 || attempt > 0 {
                let what = if retry_read { "the query" } else { "the statement (it may or may not have been applied)" };
                return Err(DbError::Query(format!(
                    "the connection was lost ({e}) while running {what}; it was not retried — run it again"
                )));
            }
            self.send(TxEvent::Note {
                worker: self.key.clone(),
                error: false,
                text: "connection lost; reconnecting and running the query again".into(),
            });
        }
        Err(DbError::Connect("not connected".into()))
    }

    /// A metadata request (reads only): retried once after a dropped connection.
    pub(crate) fn read<T>(
        &mut self,
        f: impl Fn(&mut dyn Connection) -> codecraft_db::DbResult<T>,
    ) -> Result<T, String> {
        for attempt in 0..2 {
            self.ready()?;
            let conn = self.conn.as_mut().ok_or("not connected")?;
            let r = f(conn.as_mut());
            self.last_used = Instant::now();
            match r {
                Ok(v) => return Ok(v),
                Err(e) if conn.is_broken() && attempt == 0 && !self.in_tx => self.lost(&e.to_string()),
                Err(e) => {
                    if conn.is_broken() {
                        self.lost(&e.to_string());
                    }
                    return Err(e.to_string());
                }
            }
        }
        Err("not connected".into())
    }

    /// Note a statement's effect on the transaction (statement tracking for drivers that cannot
    /// tell, e.g. MySQL).
    pub(crate) fn track(&mut self, sql: &str, ok: bool) {
        let dialect = self.cfg.kind.dialect();
        // A savepoint typed in the console (T-SQL `ROLLBACK TRAN x` only when `x` is one).
        if let Some((op, name)) = codecraft_db::savepoint::effect(sql, dialect)
            && (dialect != codecraft_db::Dialect::TSql || op != SavepointOp::RollbackTo || self.savepoints.contains(&name))
        {
            if ok {
                // SQLite: SAVEPOINT outside a transaction starts one.
                if op == SavepointOp::Set && !self.in_tx {
                    self.in_tx = true;
                    self.failed = false;
                    self.statements = 0;
                }
                self.savepoints.apply(op, &name, codecraft_db::savepoint::replaces(self.cfg.kind));
            }
            if self.in_tx {
                self.statements += 1;
            }
            return;
        }
        let effect = codecraft_db::transaction_effect(sql, dialect);
        if ok {
            match effect {
                codecraft_db::TxEffect::Begin => {
                    self.in_tx = true;
                    self.failed = false;
                    self.statements = 0;
                    self.savepoints.clear();
                }
                codecraft_db::TxEffect::End => {
                    self.in_tx = false;
                    self.failed = false;
                    self.statements = 0;
                    self.tx_closed();
                }
                codecraft_db::TxEffect::None if self.in_tx => self.statements += 1,
                codecraft_db::TxEffect::None => {}
            }
        } else if self.in_tx && effect == codecraft_db::TxEffect::None {
            self.statements += 1;
        }
    }

    /// Ask the server (when it can tell) and report the transaction state to the UI.
    pub(crate) fn refresh(&mut self) {
        if let Some(conn) = self.conn.as_mut() {
            match conn.transaction_status() {
                TxStatus::Idle => {
                    if self.in_tx {
                        // No COMMIT/ROLLBACK of ours ended it: the server did (a deadlock, an
                        // error that aborts the transaction).
                        self.statements = 0;
                        self.send(TxEvent::Note {
                            worker: self.key.clone(),
                            error: true,
                            text: "The server rolled back the transaction (e.g. a deadlock); its changes are lost"
                                .into(),
                        });
                        self.tx_closed();
                    }
                    self.in_tx = false;
                    self.failed = false;
                    self.savepoints.clear();
                }
                TxStatus::Active => {
                    self.in_tx = true;
                    self.failed = false;
                }
                TxStatus::Failed => {
                    self.in_tx = true;
                    self.failed = true;
                }
                TxStatus::Unknown => {}
            }
        }
        self.report_state();
    }

    pub(crate) fn report_state(&self) {
        let _ = self.events.send(Event::TxState { worker: self.key.clone(), open: self.in_tx });
        self.send(TxEvent::State {
            worker: self.key.clone(),
            open: self.in_tx,
            failed: self.failed,
            statements: self.statements,
            isolation: self.opts.isolation,
            savepoints: self.savepoints.names().to_vec(),
        });
    }

    /// Open a transaction for manual-commit mode unless one is open.
    pub(crate) fn begin_if_manual(&mut self, manual: bool) -> Result<(), String> {
        if !manual || self.in_tx {
            return Ok(());
        }
        self.ready()?;
        let conn = self.conn()?;
        codecraft_db::tx::begin(conn, None).map_err(|e| format!("cannot start a transaction: {e}"))?;
        self.in_tx = true;
        self.failed = false;
        self.statements = 0;
        self.savepoints.clear();
        Ok(())
    }

    /// Commit or roll back (a failed transaction is never committed: see
    /// [`codecraft_db::tx::end`]).
    pub(crate) fn end(&mut self, commit: bool) -> Result<(), String> {
        let lost_tx = std::mem::take(&mut self.lost_tx);
        let Some(conn) = self.conn.as_mut() else {
            self.in_tx = false;
            return if commit && lost_tx {
                Err("the connection was lost; the server rolled back the transaction, nothing was committed".into())
            } else if commit {
                Err("the connection was lost; the server rolled back the transaction".into())
            } else {
                Ok(())
            };
        };
        let r = codecraft_db::tx::end(conn.as_mut(), commit).map(|_| ()).map_err(|e| e.to_string());
        if conn.is_broken() {
            let e = r.as_ref().err().cloned().unwrap_or_default();
            self.lost(&e);
        }
        self.in_tx = false;
        self.failed = false;
        self.statements = 0;
        self.tx_closed();
        r
    }

    /// Set, roll back to or release a savepoint of the open transaction (manual-commit mode
    /// opens one first).
    pub(crate) fn savepoint(&mut self, op: SavepointOp, name: &str, manual: bool) -> Result<String, String> {
        let kind = self.cfg.kind;
        let sql = codecraft_db::savepoint::statement(kind, op, name).map_err(|e| e.to_string())?;
        if op == SavepointOp::Set {
            self.begin_if_manual(manual)?;
        }
        if !self.in_tx {
            return Err("no transaction is open: switch the console to manual commit or run BEGIN first".into());
        }
        self.savepoints.check(op, name)?;
        if self.failed && op != SavepointOp::RollbackTo {
            return Err("the transaction failed: only Rollback or Rollback to Savepoint work".into());
        }
        self.ready()?;
        if let Some(sql) = &sql {
            let conn = self.conn()?;
            codecraft_db::tx::run(conn, sql).map_err(|e| e.to_string())?;
            self.statements += 1;
        }
        self.savepoints.apply(op, name, codecraft_db::savepoint::replaces(kind));
        Ok(sql.unwrap_or_else(|| format!("{} {name} (SQL Server keeps it until the transaction ends)", op.label())))
    }

    pub(crate) fn handle(&mut self, req: LinkReq) {
        match req {
            LinkReq::SetIsolation(level) => {
                self.opts.isolation = level;
                let level = level.unwrap_or_else(|| Isolation::default_for(self.cfg.kind));
                let text = match (self.conn.as_mut(), session_isolation_sql(self.cfg.kind, level)) {
                    (Some(conn), Ok(Some(sql))) => match codecraft_db::tx::run(conn.as_mut(), &sql) {
                        Ok(()) => Ok(format!("Isolation level: {}", level.label())),
                        Err(e) => Err(format!("cannot set the isolation level: {e}")),
                    },
                    (_, Err(e)) => Err(e.to_string()),
                    _ => Ok(format!("Isolation level: {}", level.label())),
                };
                let (error, text) = match text {
                    Ok(t) => (false, t),
                    Err(t) => (true, t),
                };
                self.send(TxEvent::Note { worker: self.key.clone(), error, text });
                self.report_state();
            }
            LinkReq::SetTimeout(t) => {
                self.opts.timeout = t;
                if let Some(c) = self.canceller() {
                    self.watchdog = Some(Watchdog::new(c, t));
                }
                if self.conn.is_some() {
                    self.sync_server_timeout(true);
                }
            }
            LinkReq::Savepoint { op, name, manual } => {
                let (error, text) = match self.savepoint(op, &name, manual) {
                    Ok(sql) => (false, sql),
                    Err(e) => (true, format!("{} `{name}` failed: {e}", op.label())),
                };
                self.check_broken();
                self.refresh();
                self.send(TxEvent::Note { worker: self.key.clone(), error, text });
            }
        }
    }
}

struct CountingSink<'a> {
    inner: &'a mut dyn ResultSink,
    pages: usize,
}

impl ResultSink for CountingSink<'_> {
    fn columns(&mut self, columns: &[codecraft_db::Column]) {
        self.inner.columns(columns);
    }
    fn page(&mut self, page: Page) -> bool {
        self.pages += 1;
        self.inner.page(page)
    }
}

/// Build the reconnect-through-a-new-tunnel closure for a data source with an SSH tunnel.
pub(crate) fn reroute(db: &DbState, cfg: &DataSourceConfig) -> Option<Reroute> {
    let has_tunnel = cfg.ssh.as_ref().is_some_and(|t| !t.host.trim().is_empty())
        || cfg.ssh_tunnel.as_deref().is_some_and(|t| !t.trim().is_empty());
    if !has_tunnel {
        return None;
    }
    let tunnels = db.tunnels.clone();
    let account = cfg.ssh_secret_account();
    let secret = db.passwords.get(&account).cloned();
    let saved = cfg.ssh.as_ref().is_some_and(|t| t.secret_saved);
    let store: Option<Arc<dyn SecretStore>> = saved.then(|| db.secrets.clone());
    let base = cfg.clone();
    Some(Box::new(move || {
        let plan = tunnels.plan_with(&base, secret.clone(), store.clone())?;
        crate::ssh_tunnel::route(plan, base.clone())
    }))
}

// ---------------------------------------------------------------- engine state

/// One connection's transaction, as the UI shows it.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnTx {
    pub source: String,
    pub open: bool,
    /// A statement failed inside it: only Rollback works.
    pub failed: bool,
    /// Statements run in it.
    pub statements: usize,
    pub isolation: Option<Isolation>,
    /// Savepoints of the open transaction, oldest first.
    pub savepoints: Vec<String>,
    /// The connection dropped (what happened); cleared when it reconnects.
    pub lost: Option<String>,
    #[serde(skip)]
    pub since: Option<Instant>,
}

impl ConnTx {
    /// How long the transaction has been open.
    pub fn duration(&self) -> Option<Duration> {
        self.since.filter(|_| self.open).map(|t| t.elapsed())
    }
}

/// A console's own transaction settings (unset: the data source's).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleTx {
    pub manual: Option<bool>,
    pub isolation: Option<Isolation>,
}

/// One open transaction listed by the guard and `db.tx.pending`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingTx {
    pub connection: String,
    pub data_source: String,
    pub data_source_name: String,
    pub statements: usize,
    pub duration_ms: u64,
    pub failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum GuardState {
    Asking,
    /// Commit (true) or rollback sent; waiting for the connections.
    Ending(bool),
    /// Ending a transaction failed; the action did not happen.
    Failed(String),
}

/// The pending-transaction question: Commit, Rollback or Cancel before `action`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxGuard {
    /// What waits (`Disconnect`, `Close Console`, `Close Project`, `Quit`).
    pub action: String,
    pub transactions: Vec<PendingTx>,
    pub state: GuardState,
    /// The command to run once every transaction ended.
    #[serde(skip)]
    then: Option<(String, Value)>,
    #[serde(skip)]
    waiting: BTreeSet<String>,
    /// Connections to close afterwards (a closed console's).
    #[serde(skip)]
    close: Vec<String>,
}

/// Transaction state for the UI and the guard.
#[derive(Default)]
pub struct TxBook {
    pub conns: BTreeMap<String, ConnTx>,
    pub consoles: HashMap<String, ConsoleTx>,
    pub guard: Option<TxGuard>,
    /// Run the guarded command for real (the guard resolved it).
    bypass: bool,
    /// The guard of a window close resolved: the window may close now.
    close_window: bool,
}

impl TxBook {
    pub fn conn(&self, worker: &str) -> Option<&ConnTx> {
        self.conns.get(worker)
    }

    pub fn console(&self, worker: &str) -> ConsoleTx {
        self.consoles.get(worker).copied().unwrap_or_default()
    }

    /// Whether the project window may close now (its transactions were resolved); reading it
    /// resets it.
    pub fn take_close_window(&mut self) -> bool {
        std::mem::take(&mut self.close_window)
    }

    pub(crate) fn allow_close_window(&mut self) {
        self.close_window = true;
    }
}

impl DbState {
    /// Manual commit for a connection: the console's choice, else the data source's.
    pub fn manual_commit(&self, worker: &str, source: &str) -> bool {
        // ClickHouse, Redis and DuckDB have no transactions: always auto-commit.
        if self.source(source).is_some_and(|d| !d.config.kind.supports_transactions()) {
            return false;
        }
        self.txs.console(worker).manual.unwrap_or_else(|| self.source(source).is_some_and(|d| d.config.manual_commit))
    }

    /// The isolation level a connection uses (`None`: the server default).
    pub fn isolation(&self, worker: &str, source: &str) -> Option<Isolation> {
        self.txs.console(worker).isolation.or_else(|| {
            self.source(source).and_then(|d| d.config.options.get("isolation")).and_then(|v| Isolation::parse(v))
        })
    }

    /// The settings a new worker starts with.
    pub(crate) fn link_opts(&self, worker: &str, cfg: &DataSourceConfig) -> LinkOpts {
        let mut o = LinkOpts::from_config(cfg);
        if let Some(i) = self.txs.console(worker).isolation {
            o.isolation = Some(i);
        }
        o
    }

    /// Change a live connection's settings.
    pub(crate) fn send_link(&mut self, worker: &str, req: LinkReq) -> Result<(), String> {
        if !self.workers.contains_key(worker) {
            return Ok(());
        }
        self.send(worker, Req::Link(req))
    }

    /// Every live connection of a data source.
    pub fn connections_of(&self, source: &str) -> Vec<String> {
        let mut v: Vec<String> =
            self.workers.iter().filter(|(_, w)| w.source == source).map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    /// Where unqualified names of a data source resolve (`public`, `main`, the MySQL database).
    pub fn default_schema(&self, source: &str) -> Option<String> {
        let ds = self.source(source)?;
        if let Some(d) = ds.tree.info.as_ref().and_then(|i| i.default_schema.clone()) {
            return Some(d);
        }
        Some(fallback_schema(&ds.config))
    }

    /// Open transactions (of one data source, or all), with what the guard shows.
    pub fn pending_transactions(&self, source: Option<&str>) -> Vec<PendingTx> {
        self.open_transactions(source)
            .into_iter()
            .map(|w| {
                let src = self.workers.get(&w).map(|x| x.source.clone()).unwrap_or_default();
                let c = self.txs.conns.get(&w).cloned().unwrap_or_default();
                PendingTx {
                    data_source_name: self.source(&src).map(|d| d.config.name.clone()).unwrap_or_else(|| src.clone()),
                    data_source: src,
                    statements: c.statements,
                    duration_ms: c.duration().map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(0),
                    failed: c.failed,
                    connection: w,
                }
            })
            .collect()
    }
}

/// A worker event about transactions or the connection.
/// Where unqualified names of a data source resolve before its schema tree is loaded.
pub(crate) fn fallback_schema(cfg: &DataSourceConfig) -> String {
    let db = cfg.database.trim();
    match cfg.kind {
        DbKind::Sqlite | DbKind::DuckDb => "main".into(),
        DbKind::Postgres => "public".into(),
        DbKind::Mysql | DbKind::Mariadb => cfg.database.clone(),
        DbKind::SqlServer => "dbo".into(),
        DbKind::ClickHouse if db.is_empty() => "default".into(),
        DbKind::ClickHouse => db.to_string(),
        DbKind::Redis => format!("db{}", db.trim_start_matches("db").parse::<u32>().unwrap_or(0)),
    }
}

pub(crate) fn on_event(s: &mut Session, ev: TxEvent) {
    let db = &mut s.db;
    match ev {
        TxEvent::State { worker, open, failed, statements, isolation, savepoints } => {
            let source = db.workers.get(&worker).map(|w| w.source.clone()).unwrap_or_default();
            if let Some(w) = db.workers.get_mut(&worker) {
                w.in_tx = open;
            }
            let c = db.txs.conns.entry(worker).or_default();
            if open && !c.open {
                c.since = Some(Instant::now());
            }
            if !open {
                c.since = None;
            }
            c.source = source;
            c.open = open;
            c.failed = failed;
            c.statements = statements;
            c.isolation = isolation;
            c.savepoints = if open { savepoints } else { Vec::new() };
        }
        TxEvent::Lost { worker, in_tx, error } => {
            let source = db.workers.get(&worker).map(|w| w.source.clone()).unwrap_or_default();
            if let Some(w) = db.workers.get_mut(&worker) {
                w.in_tx = false;
            }
            let msg = if in_tx {
                format!("Connection lost; the server rolled back the open transaction ({error})")
            } else {
                format!("Connection lost ({error}); it reconnects for the next query")
            };
            let c = db.txs.conns.entry(worker.clone()).or_default();
            c.source = source.clone();
            c.open = false;
            c.failed = false;
            c.since = None;
            c.savepoints.clear();
            c.lost = Some(msg.clone());
            db.log(OutputLevel::Error, &source, format!("{worker}: {msg}"));
            if worker.starts_with("meta:")
                && let Some(ds) = db.source_mut(&source)
            {
                ds.status = super::ConnStatus::Failed(msg.clone());
            }
            if in_tx {
                let name = db.source(&source).map(|d| d.config.name.clone()).unwrap_or(source);
                s.notify(
                    codecraft_workspace::NotificationLevel::Error,
                    &format!("{name}: transaction rolled back"),
                    msg.clone(),
                );
            }
            s.status = msg;
            guard_connection_gone(s, &worker);
        }
        TxEvent::Reconnected { worker } => {
            let source = db.workers.get(&worker).map(|w| w.source.clone()).unwrap_or_default();
            if let Some(c) = db.txs.conns.get_mut(&worker) {
                c.lost = None;
            }
            db.log(OutputLevel::Info, &source, format!("{worker}: reconnected"));
        }
        TxEvent::Note { worker, error, text } => {
            let source = db.workers.get(&worker).map(|w| w.source.clone()).unwrap_or_default();
            db.log(if error { OutputLevel::Error } else { OutputLevel::Info }, &source, text.clone());
            s.status = text;
        }
    }
}

// ---------------------------------------------------------------- the guard

/// Commands that end connections, closing them with a transaction open.
const QUIT: [&str; 2] = ["app.quitRustRover", "app.quitAndKeepWindows"];
/// Closing a project window (not a menu item: the window's close button runs it through
/// [`close_window_blocked`]).
pub const CLOSE_WINDOW: &str = "db.tx.closeWindow";

/// Whether `file.open` / `file.new.project` with these params would replace the window's
/// project (open transactions of this project would be closed).
fn opens_other_project(s: &Session, id: &str, p: &Value) -> bool {
    let Some(path) = crate::params::opt_path(p, "path") else { return false };
    let path = crate::normalize_path(&path);
    let current = s.ws.project.as_ref().map(|pr| pr.root.clone());
    match id {
        "file.open" => path.is_dir() && current.as_deref() != Some(path.as_path()),
        "file.new.project" => current.as_deref() != Some(path.as_path()),
        _ => false,
    }
}

fn console_workers_of(s: &Session, editors: &[codecraft_workspace::EditorId]) -> Vec<String> {
    let mut out = Vec::new();
    for id in editors {
        let Some(e) = s.ws.editor(*id) else { continue };
        let Some(d) = s.ws.doc(e.doc) else { continue };
        if d.language != codecraft_syntax::Language::Sql {
            continue;
        }
        // Another tab of the same document keeps the console open.
        let others =
            s.ws.all_tabs()
                .into_iter()
                .filter(|t| !editors.contains(t))
                .any(|t| s.ws.editor(t).is_some_and(|x| x.doc == e.doc));
        if others {
            continue;
        }
        let w = super::console_worker(d.path.as_deref(), d.id.0);
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// The connections a command would close, and the guard's label for it. `None`: not guarded.
fn affected(s: &Session, id: &str, p: &Value) -> Option<(String, Vec<String>, Vec<String>)> {
    let all = || s.db.open_transactions(None);
    let of_source = |src: Option<&str>| s.db.open_transactions(src);
    let source_param = || {
        crate::params::opt_str(p, "dataSource").map(str::to_string).or_else(|| {
            s.db.selected
                .as_ref()
                .and_then(|k| k.split('\u{1f}').next())
                .map(str::to_string)
                .filter(|x| s.db.source(x).is_some())
        })
    };
    let tabs_closing = |ids: Vec<codecraft_workspace::EditorId>| {
        let workers = console_workers_of(s, &ids);
        let open: Vec<String> = workers.iter().filter(|w| s.db.transaction_open(w)).cloned().collect();
        ("Close Console".to_string(), open, workers)
    };
    match id {
        "db.disconnect" => Some(("Disconnect".into(), of_source(source_param().as_deref()), Vec::new())),
        "db.removeDataSource" => {
            let src = source_param()?;
            Some(("Remove Data Source".into(), of_source(Some(&src)), Vec::new()))
        }
        "db.editDataSource" if p.as_object().is_some_and(|o| o.len() > 1) => {
            let src = crate::params::opt_str(p, "dataSource")?;
            Some(("Change Data Source".into(), of_source(Some(src)), Vec::new()))
        }
        "file.closeProject" => Some(("Close Project".into(), all(), Vec::new())),
        "file.open" | "file.new.project" if opens_other_project(s, id, p) => {
            Some(("Open Project".into(), all(), Vec::new()))
        }
        CLOSE_WINDOW => Some(("Close Window".into(), all(), Vec::new())),
        q if QUIT.contains(&q) => Some(("Quit".into(), all(), Vec::new())),
        "window.editorTabs.closeTab" => Some(tabs_closing(s.ws.active_editor().into_iter().collect())),
        "window.editorTabs.close" => {
            let e = crate::params::opt_usize(p, "editor").ok().flatten()?;
            Some(tabs_closing(vec![codecraft_workspace::EditorId(e as u64)]))
        }
        "window.editorTabs.closeAllTabs" => Some(tabs_closing(s.ws.all_tabs())),
        "window.editorTabs.closeOtherTabs" => {
            let active = s.ws.active_editor();
            Some(tabs_closing(s.ws.all_tabs().into_iter().filter(|t| Some(*t) != active).collect()))
        }
        _ => None,
    }
}

/// Called before every command: a command that would close connections with an open
/// transaction asks Commit / Rollback / Cancel first (`transactions: "commit" | "rollback"` in
/// its params answers up front). `None`: run the command normally.
pub(crate) fn intercept(s: &mut Session, id: &str, p: &Value) -> Option<Result<Value, CommandError>> {
    if s.db.txs.bypass {
        return None;
    }
    let (action, open, consoles) = affected(s, id, p)?;
    if open.is_empty() {
        return None;
    }
    let choice = crate::params::opt_str(p, "transactions").map(str::to_ascii_lowercase);
    let mut then_params = p.clone();
    if let Some(o) = then_params.as_object_mut() {
        o.remove("transactions");
    }
    let transactions: Vec<PendingTx> =
        s.db.pending_transactions(None).into_iter().filter(|t| open.contains(&t.connection)).collect();
    s.db.txs.guard = Some(TxGuard {
        action: action.clone(),
        transactions: transactions.clone(),
        state: GuardState::Asking,
        then: Some((id.to_string(), then_params)),
        waiting: BTreeSet::new(),
        close: consoles,
    });
    match choice.as_deref() {
        Some("commit") => Some(resolve(s, Choice::Commit)),
        Some("rollback") => Some(resolve(s, Choice::Rollback)),
        Some(other) => {
            s.db.txs.guard = None;
            Some(Err(CommandError::BadParams(format!("`transactions` is commit or rollback, not `{other}`"))))
        }
        None => {
            let n = transactions.len();
            s.status = format!(
                "{n} transaction{} open: Commit, Rollback or Cancel before {}",
                if n == 1 { " is" } else { "s are" },
                action.to_lowercase()
            );
            Some(Ok(json!({ "pendingTransactions": transactions, "ask": true, "action": action })))
        }
    }
}

/// Closing a project window (not the main one): true while transactions must be resolved
/// first (the guard asks; once they ended, [`TxBook::take_close_window`] says to close it).
pub fn close_window_blocked(s: &mut Session) -> bool {
    if s.db.txs.guard.is_some() {
        return true;
    }
    if s.db.open_transactions(None).is_empty() {
        return false;
    }
    matches!(intercept(s, CLOSE_WINDOW, &json!({})), Some(Ok(_)))
}

/// Closing the main window: true while transactions must be resolved first (the guard asks).
pub fn quit_blocked(s: &mut Session) -> bool {
    if s.db.txs.guard.is_some() {
        return true;
    }
    if s.db.open_transactions(None).is_empty() {
        return false;
    }
    matches!(intercept(s, "app.quitRustRover", &json!({})), Some(Ok(_)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Commit,
    Rollback,
    Cancel,
}

impl Choice {
    pub fn parse(s: &str) -> Option<Choice> {
        match s.trim().to_ascii_lowercase().as_str() {
            "commit" => Some(Choice::Commit),
            "rollback" => Some(Choice::Rollback),
            "cancel" => Some(Choice::Cancel),
            _ => None,
        }
    }
}

/// Answer the guard.
pub(crate) fn resolve(s: &mut Session, choice: Choice) -> Result<Value, CommandError> {
    let Some(guard) = s.db.txs.guard.as_mut() else {
        return Err(CommandError::Disabled("no pending-transaction question".into()));
    };
    if choice == Choice::Cancel {
        let action = guard.action.clone();
        s.db.txs.guard = None;
        s.status = format!("{action} cancelled; the transactions stay open");
        return Ok(json!({ "cancelled": true }));
    }
    let commit = choice == Choice::Commit;
    let conns: Vec<String> = guard.transactions.iter().map(|t| t.connection.clone()).collect();
    guard.state = GuardState::Ending(commit);
    guard.waiting.clear();
    let mut started = Vec::new();
    for w in conns {
        if !s.db.transaction_open(&w) {
            continue;
        }
        match s.db.end_transaction(&w, commit) {
            Ok(()) => {
                started.push(w.clone());
                if let Some(g) = s.db.txs.guard.as_mut() {
                    g.waiting.insert(w);
                }
            }
            // A closed connection's transaction is gone already.
            Err(e) => s.db.log(OutputLevel::Info, "", format!("{w}: {e}; the server rolled the transaction back")),
        }
    }
    s.status = if commit { "Committing…".into() } else { "Rolling back…".into() };
    maybe_finish(s);
    Ok(json!({ "ending": started, "commit": commit }))
}

/// A Commit/Rollback finished on `worker` (called from the TxDone event).
pub(crate) fn on_tx_done(s: &mut Session, worker: &str, result: &Result<(), String>) {
    let Some(g) = s.db.txs.guard.as_mut() else { return };
    if !g.waiting.remove(worker) {
        return;
    }
    if let Err(e) = result
        && matches!(g.state, GuardState::Ending(true))
    {
        g.state = GuardState::Failed(format!("{worker}: {e}"));
    }
    maybe_finish(s);
}

fn guard_connection_gone(s: &mut Session, worker: &str) {
    let Some(g) = s.db.txs.guard.as_mut() else { return };
    if g.waiting.remove(worker) && matches!(g.state, GuardState::Ending(true)) {
        g.state = GuardState::Failed(format!("{worker}: the connection was lost; the server rolled back"));
    }
    maybe_finish(s);
}

/// Every transaction ended: run the waiting command (unless a commit failed).
fn maybe_finish(s: &mut Session) {
    let Some(g) = s.db.txs.guard.as_ref() else { return };
    if !g.waiting.is_empty() || !matches!(g.state, GuardState::Ending(_)) {
        return;
    }
    let Some(g) = s.db.txs.guard.take() else { return };
    for w in &g.close {
        s.db.close_worker(w);
    }
    if let Some((id, params)) = g.then {
        s.db.txs.bypass = true;
        let r = s.execute(&id, &params);
        s.db.txs.bypass = false;
        if let Err(e) = r {
            s.status = format!("{}: {e}", g.action);
        }
    }
}

impl DbState {
    /// Close one connection (a console that was closed).
    pub(crate) fn close_worker(&mut self, worker: &str) {
        if let Some(w) = self.workers.remove(worker) {
            let _ = w.tx.send(Req::Close);
        }
        self.txs.conns.remove(worker);
        self.txs.consoles.remove(worker);
        self.runs.retain(|_, r| r.worker != worker);
        for r in self.results.iter_mut().filter(|r| r.worker == worker) {
            if r.has_more {
                r.has_more = false;
                r.truncated = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_choices() {
        let mut cfg = DataSourceConfig { kind: DbKind::Postgres, ..Default::default() };
        assert_eq!(LinkOpts::from_config(&cfg).keepalive, Some(DEFAULT_KEEPALIVE));
        cfg.options.insert("keepalive".into(), "0".into());
        cfg.options.insert("query_timeout".into(), "30".into());
        cfg.options.insert("isolation".into(), "serializable".into());
        let o = LinkOpts::from_config(&cfg);
        assert_eq!(
            (o.keepalive, o.timeout, o.isolation),
            (None, Some(Duration::from_secs(30)), Some(Isolation::Serializable))
        );
        cfg.kind = DbKind::Sqlite;
        cfg.options.clear();
        assert_eq!(LinkOpts::from_config(&cfg).keepalive, None);
        assert!(o.server_timeout);
        cfg.options.insert("server_timeout".into(), "off".into());
        assert!(!LinkOpts::from_config(&cfg).server_timeout);
        assert_eq!(Choice::parse(" Commit"), Some(Choice::Commit));
        assert_eq!(Choice::parse("later"), None);
    }
}
