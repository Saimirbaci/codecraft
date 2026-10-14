//! Database connections, schema trees, SQL consoles and result sets. OWNED BY the database track.
//!
//! Data sources are saved per project in `.codecraft/datasources.json` (no secrets; passwords
//! live in the OS keychain). Each connection runs on its own worker thread: one per data source
//! for introspection (`meta:`), one per SQL console, one for table views. Workers report
//! through one channel drained by [`pump`]. Result sets keep their cursor open on the worker so
//! more rows can be fetched in pages of 500. Table results with a primary key can be edited in
//! the grid ([`edit`]): pending changes are submitted as one transaction on the result's worker.
//!
//! Consoles keep a query [`history`] (with saved queries) per data source, prompt for query
//! parameters and confirm writes on production data sources ([`exec`]), explain plans, run in
//! manual-commit mode (Commit/Rollback) when the data source asks for it, and [`import`] loads
//! CSV/JSON files into tables.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};

use codecraft_db::{
    Canceller, Cell, Column, DataSourceConfig, DatabaseInfo, DbError, DbKind, ExecSummary, KeychainStore, Page,
    ResultSink, SecretStore, TableDetails, TableInfo,
};
use codecraft_workspace::NotificationLevel;

use crate::Session;

pub mod data_tools;
mod edit;
pub mod exec;
pub mod history;
pub mod import;
pub mod tx;

pub use codecraft_db::explain::{Plan, PlanNode};
pub use edit::{CellView, EditPreview, GridEdits, KeyState, RowState};
pub use exec::{ConfirmPrompt, ExecAction, ParamField, ParamPrompt};
pub use history::{HistoryEntry, HistoryPopup, HistoryStore, SavedQuery};
pub use import::{ImportDialog, ImportState};

/// Data sources, relative to the project root.
pub const DATASOURCES_FILE: &str = ".codecraft/datasources.json";
/// SQL consoles: `<root>/.codecraft/consoles/<data source id>/console.sql`.
pub const CONSOLES_DIR: &str = ".codecraft/consoles";
/// Output log lines kept.
const MAX_OUTPUT: usize = 2000;
/// Rows kept per result set (more can be fetched up to this).
pub const MAX_RESULT_ROWS: usize = 200_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum ConnStatus {
    #[default]
    Disconnected,
    Connecting,
    /// The server version.
    Connected(String),
    Failed(String),
}

/// The cached schema tree of a data source (loaded lazily level by level).
#[derive(Clone, Debug, Default, Serialize)]
pub struct SchemaTree {
    pub info: Option<DatabaseInfo>,
    pub loading: bool,
    pub error: Option<String>,
    pub schemas: BTreeMap<String, SchemaNode>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SchemaNode {
    pub tables: Option<Vec<TableNode>>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TableNode {
    pub info: TableInfo,
    pub details: Option<TableDetails>,
    pub loading: bool,
    pub row_count: Option<u64>,
    pub counting: bool,
    pub error: Option<String>,
}

impl TableNode {
    fn new(info: TableInfo) -> Self {
        Self { info, details: None, loading: false, row_count: None, counting: false, error: None }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DataSource {
    pub config: DataSourceConfig,
    pub status: ConnStatus,
    pub tree: SchemaTree,
}

impl DataSource {
    pub fn id(&self) -> &str {
        &self.config.id
    }

    pub fn table(&self, schema: &str, table: &str) -> Option<&TableNode> {
        self.tree.schemas.get(schema)?.tables.as_ref()?.iter().find(|t| t.info.name == table)
    }

    fn table_mut(&mut self, schema: &str, table: &str) -> Option<&mut TableNode> {
        self.tree.schemas.get_mut(schema)?.tables.as_mut()?.iter_mut().find(|t| t.info.name == table)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ResultStatus {
    Running,
    Fetching,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    pub schema: String,
    pub table: String,
}

/// The grid's filter row: a WHERE condition and an ORDER BY clause typed by the user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResultFilter {
    #[serde(rename = "where")]
    pub where_: String,
    pub order_by: String,
}

impl ResultFilter {
    pub fn is_empty(&self) -> bool {
        self.where_.trim().is_empty() && self.order_by.trim().is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    /// Column index (0-based).
    pub column: usize,
    pub desc: bool,
}

/// One result tab in the Services tool window.
#[derive(Clone, Debug, Serialize)]
pub struct ResultSet {
    pub id: u64,
    pub title: String,
    /// Data source id.
    pub source: String,
    /// What produced it (a console path, or `table:<source>:<schema>.<table>`); a new run from
    /// the same owner replaces its tabs.
    pub owner: String,
    pub sql: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub status: ResultStatus,
    pub error: Option<String>,
    /// The cursor is open: `db.fetchMore` loads the next page.
    pub has_more: bool,
    /// More rows existed but the cursor was closed (another query ran on the connection).
    pub truncated: bool,
    /// Time to the first page (then the full statement time once it completes).
    pub elapsed_ms: Option<u64>,
    /// Set for table views: sorting re-queries with ORDER BY on the column name.
    pub table: Option<TableRef>,
    pub sort: Option<SortKey>,
    /// Pending in-grid changes (table results with a primary key).
    pub edits: GridEdits,
    /// An Explain Plan tab shows this tree instead of rows.
    pub plan: Option<Plan>,
    /// The filter row applied to this result.
    pub filter: ResultFilter,
    /// Rows shown as columns (record view).
    pub transposed: bool,
    /// The query without the ORDER BY added by sorting.
    #[serde(skip)]
    pub(crate) base_sql: String,
    #[serde(skip)]
    pub(crate) run: u64,
    #[serde(skip)]
    stmt: usize,
    #[serde(skip)]
    pub(crate) worker: String,
}

impl ResultSet {
    /// The status line under the grid.
    pub fn status_line(&self) -> String {
        let n = self.rows.len();
        let rows = if n == 1 { "1 row".to_string() } else { format!("{n} rows") };
        let time = self.elapsed_ms.map(|t| format!(" in {t} ms")).unwrap_or_default();
        if let Some(p) = &self.plan {
            let mut s = format!("{} plan steps", p.roots.iter().map(PlanNode::count).sum::<usize>());
            if let Some(ms) = p.execution_ms {
                s.push_str(&format!("; executed in {} ms", codecraft_db::value::format_number(ms)));
            }
            if let Some(note) = &p.note {
                s.push_str(&format!(" ({note})"));
            }
            return s;
        }
        match self.status {
            ResultStatus::Running => "Executing…".into(),
            ResultStatus::Fetching => format!("{rows} fetched, fetching more…"),
            ResultStatus::Failed => self.error.clone().unwrap_or_else(|| "Failed".into()),
            ResultStatus::Cancelled => format!("{rows} fetched; cancelled{time}"),
            ResultStatus::Done if self.has_more => format!("{rows} fetched{time}; more available"),
            ResultStatus::Done if self.truncated => format!("{rows} fetched{time}; result closed, rerun to fetch more"),
            ResultStatus::Done => format!("{rows} retrieved{time}"),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, ResultStatus::Running | ResultStatus::Fetching)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum OutputLevel {
    Info,
    Sql,
    Error,
}

/// A line of the Services › Output log.
#[derive(Clone, Debug, Serialize)]
pub struct OutputLine {
    pub level: OutputLevel,
    pub source: String,
    pub text: String,
}

/// The add/edit data source dialog. Text fields are edited by the UI in place.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DataSourceForm {
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    /// Never serialized; empty while editing keeps the saved password.
    #[serde(skip_serializing)]
    pub password: String,
    pub save_password: bool,
    pub path: String,
    /// `key=value` pairs separated by `;` (e.g. `sslmode=require; connect_timeout=5`).
    pub options: String,
    pub read_only: bool,
    /// A colour name (empty: none).
    pub color: String,
    /// Production guard: confirm statements that modify data.
    pub confirm_writes: bool,
    /// Manual-commit transactions in consoles.
    pub manual_commit: bool,
    /// SSH server to tunnel through (empty: connect directly). Older configurations; the form
    /// edits the `ssh_*` fields.
    pub ssh_tunnel: String,
    /// SSH tunnel host: a host name or IP, `user@host:port`, or a server of `~/.ssh/config`
    /// (empty: connect directly).
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_auth: codecraft_db::SshAuth,
    /// Private key file for key-pair authentication.
    pub ssh_key_file: String,
    /// Never serialized; empty while editing keeps the saved password or passphrase.
    #[serde(skip_serializing)]
    pub ssh_password: String,
    pub save_ssh_password: bool,
}

impl DataSourceForm {
    pub fn for_kind(kind: DbKind) -> Self {
        let mut f = Self { kind, save_password: true, save_ssh_password: true, ..Self::default() };
        f.set_kind(kind);
        f
    }

    /// Switch the kind, filling the port and host defaults the user hasn't changed.
    pub fn set_kind(&mut self, kind: DbKind) {
        let old_default = self.kind.default_port().map(|p| p.to_string()).unwrap_or_default();
        if self.port.is_empty() || self.port == old_default {
            self.port = kind.default_port().map(|p| p.to_string()).unwrap_or_default();
        }
        if !kind.is_file_based() && self.host.is_empty() {
            self.host = "localhost".into();
        }
        if kind == DbKind::Postgres && self.database.is_empty() {
            self.database = "postgres".into();
        }
        self.kind = kind;
    }

    pub fn from_config(cfg: &DataSourceConfig) -> Self {
        Self {
            name: cfg.name.clone(),
            kind: cfg.kind,
            host: cfg.host.clone(),
            port: cfg.port.map(|p| p.to_string()).unwrap_or_default(),
            database: cfg.database.clone(),
            user: cfg.user.clone(),
            password: String::new(),
            save_password: cfg.password_saved || !cfg.kind.is_file_based(),
            path: cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            options: cfg.options.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "),
            read_only: cfg.read_only,
            color: cfg.color.clone().unwrap_or_default(),
            confirm_writes: cfg.confirm_writes,
            manual_commit: cfg.manual_commit,
            ssh_tunnel: cfg.ssh_tunnel.clone().unwrap_or_default(),
            ssh_host: cfg.ssh.as_ref().map(|t| t.host.clone()).or_else(|| cfg.ssh_tunnel.clone()).unwrap_or_default(),
            ssh_port: cfg.ssh.as_ref().and_then(|t| t.port).map(|p| p.to_string()).unwrap_or_default(),
            ssh_user: cfg.ssh.as_ref().map(|t| t.user.clone()).unwrap_or_default(),
            ssh_auth: cfg.ssh.as_ref().map(|t| t.auth).unwrap_or_default(),
            ssh_key_file: cfg.ssh.as_ref().map(|t| t.identity_file.clone()).unwrap_or_default(),
            ssh_password: String::new(),
            save_ssh_password: cfg.ssh.as_ref().is_none_or(|t| t.secret_saved),
        }
    }

    /// The command params for `db.addDataSource` / `db.editDataSource` / `db.testConnection`.
    pub fn to_params(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("password".into(), serde_json::Value::String(self.password.clone()));
            let host = self.ssh_host.trim();
            let ssh = if host.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!({
                    "host": host,
                    "port": self.ssh_port.trim(),
                    "user": self.ssh_user.trim(),
                    "auth": self.ssh_auth.id(),
                    "identityFile": self.ssh_key_file.trim(),
                })
            };
            o.insert("ssh".into(), ssh);
            // The `ssh` object replaces the older server-name field.
            o.insert("sshTunnel".into(), serde_json::Value::String(String::new()));
            o.insert("sshPassword".into(), serde_json::Value::String(self.ssh_password.clone()));
            o.insert("saveSshPassword".into(), serde_json::Value::Bool(self.save_ssh_password));
            o.insert("options".into(), serde_json::to_value(parse_options(&self.options)).unwrap_or_default());
        }
        v
    }
}

pub(crate) fn parse_options(s: &str) -> BTreeMap<String, String> {
    s.split([';', '\n'])
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub enum TestState {
    #[default]
    Idle,
    Running,
    Ok(String),
    Failed(String),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DataSourceDialog {
    /// The data source being edited; `None` adds a new one.
    pub editing: Option<String>,
    pub form: DataSourceForm,
    pub test: TestState,
    #[serde(skip)]
    test_nonce: u64,
}

impl DataSourceDialog {
    pub fn new(editing: Option<String>, form: DataSourceForm) -> Self {
        Self { editing, form, test: TestState::Idle, test_nonce: 0 }
    }
}

// ---------------------------------------------------------------- workers

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MetaReq {
    Info,
    Tables(String),
    Details(String, String),
    Count(String, String),
}

enum MetaResult {
    Info(DatabaseInfo),
    Tables(Vec<TableInfo>),
    Details(TableDetails),
    Count(u64),
}

enum Req {
    /// Run statements; `manual` opens a transaction first when none is open.
    Exec {
        run: u64,
        statements: Vec<String>,
        manual: bool,
    },
    /// Explain one statement (`analyze` runs it).
    Explain {
        run: u64,
        sql: String,
        analyze: bool,
        manual: bool,
    },
    /// End the connection's transaction.
    Tx {
        commit: bool,
    },
    More {
        run: u64,
    },
    StopFetch,
    Meta(MetaReq),
    /// Apply in-grid edits in one transaction.
    Apply {
        result: u64,
        schema: String,
        table: String,
        changes: Vec<codecraft_db::edit::RowChange>,
    },
    /// Connection settings (isolation level, query timeout).
    Link(tx::LinkReq),
    Close,
}

enum Event {
    Connected {
        worker: String,
        version: String,
        canceller: Arc<dyn Canceller>,
    },
    ConnectFailed {
        worker: String,
        error: String,
    },
    Columns {
        run: u64,
        stmt: usize,
        columns: Vec<Column>,
    },
    Page {
        run: u64,
        stmt: usize,
        rows: Vec<Vec<Cell>>,
        last: bool,
        waiting: bool,
    },
    StmtDone {
        run: u64,
        stmt: usize,
        summary: ExecSummary,
    },
    StmtFailed {
        run: u64,
        stmt: usize,
        error: DbError,
    },
    RunDone {
        run: u64,
    },
    Meta {
        worker: String,
        req: MetaReq,
        result: Result<MetaResult, String>,
    },
    Tested {
        nonce: u64,
        result: Result<String, String>,
    },
    Applied {
        worker: String,
        result: u64,
        outcome: Result<codecraft_db::edit::ApplyReport, codecraft_db::edit::ApplyFailure>,
    },
    SecretFailed(String),
    Plan {
        run: u64,
        result: Result<Plan, String>,
    },
    /// Whether the worker's connection has an open transaction.
    TxState {
        worker: String,
        open: bool,
    },
    TxDone {
        worker: String,
        commit: bool,
        result: Result<(), String>,
    },
    Import(import::ImportEvent),
    /// Transactions and connection state (`tx`).
    Tx(tx::TxEvent),
    /// Data tools jobs (`data_tools`).
    Data(data_tools::DataEvent),
}

struct Worker {
    tx: Sender<Req>,
    source: String,
    canceller: Option<Arc<dyn Canceller>>,
    connected: bool,
    /// A transaction is open on this connection.
    in_tx: bool,
}

/// What to run: see [`DbState::start_run`].
#[derive(Clone, Debug, Default)]
pub(crate) struct RunSpec {
    /// The connection: `console:<path>`, `tables:<source>`, `query:<source>`.
    pub worker: String,
    pub source: String,
    /// Tabs of the same owner are replaced by the new run's tabs.
    pub owner: String,
    pub title: String,
    pub statements: Vec<String>,
    pub table: Option<TableRef>,
    pub sort: Option<SortKey>,
    /// Re-sort this tab in place.
    pub replace: Option<u64>,
    /// The unsorted query, when `statements` is a re-sorted form of it.
    pub base_sql: Option<String>,
    /// The filter row the statements apply.
    pub filter: ResultFilter,
    /// Open a transaction before the first statement (manual-commit mode).
    pub manual: bool,
    /// Explain the (single) statement instead of running it; `true` analyzes.
    pub explain: Option<bool>,
}

struct RunInfo {
    worker: String,
    source: String,
    owner: String,
    title: String,
    statements: Vec<String>,
    /// Re-sorting reuses this result tab.
    replace: Option<u64>,
    table: Option<TableRef>,
    sort: Option<SortKey>,
    base_sql: Option<String>,
    filter: ResultFilter,
    started: Instant,
    cancel_requested: bool,
    /// The last statement's cursor is open, waiting for `More` (not running).
    waiting: bool,
}

/// Everything the database track keeps in the [`Session`].
pub struct DbState {
    pub sources: Vec<DataSource>,
    pub results: Vec<ResultSet>,
    /// The result tab shown in Services; `None` shows the Output log.
    pub active_result: Option<u64>,
    pub output: Vec<OutputLine>,
    pub dialog: Option<DataSourceDialog>,
    /// Expanded tree nodes (see [`node_key`]).
    pub expanded: BTreeSet<String>,
    pub selected: Option<String>,
    /// SQL files bound to a data source (consoles are bound by their location).
    pub bindings: BTreeMap<PathBuf, String>,
    /// Primary keys of tables opened for editing (by [`node_key`]).
    pub table_keys: HashMap<String, KeyState>,
    /// The SQL preview of a result's pending changes (Submit asks to confirm it).
    pub edit_preview: Option<EditPreview>,
    /// Query history and saved queries per data source.
    pub history: HistoryStore,
    /// The query history popup.
    pub history_popup: Option<HistoryPopup>,
    /// Values asked for before running statements with parameters.
    pub param_prompt: Option<ParamPrompt>,
    /// Data-modifying statements waiting for confirmation (production guard).
    pub confirm: Option<ConfirmPrompt>,
    /// The Import Data dialog.
    pub import: Option<ImportDialog>,
    /// Parameter values remembered per console (worker key → name → value).
    pub param_values: HashMap<String, BTreeMap<String, String>>,
    root: Option<PathBuf>,
    loaded: bool,
    secrets: Arc<dyn SecretStore>,
    /// Passwords entered this session (used before the keychain).
    passwords: HashMap<String, String>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    workers: HashMap<String, Worker>,
    runs: HashMap<u64, RunInfo>,
    next_id: u64,
    /// SSH tunnels of data sources with `ssh_tunnel` (track S, `ssh_tunnel.rs`).
    pub(crate) tunnels: crate::ssh_tunnel::Tunnels,
    /// Open transactions, console transaction settings and the pending-transaction guard.
    pub txs: tx::TxBook,
    /// Compare Data, Dump/Restore, Export to File and Full-text Search jobs.
    pub data: data_tools::DataTools,
}

impl Default for DbState {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            sources: Vec::new(),
            results: Vec::new(),
            active_result: None,
            output: Vec::new(),
            dialog: None,
            expanded: BTreeSet::new(),
            selected: None,
            bindings: BTreeMap::new(),
            table_keys: HashMap::new(),
            edit_preview: None,
            history: HistoryStore::default(),
            history_popup: None,
            param_prompt: None,
            confirm: None,
            import: None,
            param_values: HashMap::new(),
            root: None,
            loaded: false,
            secrets: Arc::new(KeychainStore::default()),
            passwords: HashMap::new(),
            tx,
            rx,
            workers: HashMap::new(),
            runs: HashMap::new(),
            next_id: 0,
            tunnels: Default::default(),
            txs: tx::TxBook::default(),
            data: data_tools::DataTools::default(),
        }
    }
}

/// The connection key of an SQL console document (`console:<path>`).
pub fn console_worker(path: Option<&Path>, doc: u64) -> String {
    match path {
        Some(p) => format!("console:{}", p.display()),
        None => format!("console:untitled-{doc}"),
    }
}

/// Tree node key: `source`, `source␟schema`, `source␟schema␟table`.
pub fn node_key(source: &str, schema: Option<&str>, table: Option<&str>) -> String {
    let mut k = source.to_string();
    for part in [schema, table].into_iter().flatten() {
        k.push('\u{1f}');
        k.push_str(part);
    }
    k
}

impl DbState {
    /// Replace the password store (tests use [`codecraft_db::MemoryStore`]).
    pub fn set_secret_store(&mut self, store: Arc<dyn SecretStore>) {
        self.secrets = store;
    }

    pub fn source(&self, id: &str) -> Option<&DataSource> {
        self.sources.iter().find(|s| s.config.id == id)
    }

    pub(crate) fn source_mut(&mut self, id: &str) -> Option<&mut DataSource> {
        self.sources.iter_mut().find(|s| s.config.id == id)
    }

    pub fn result(&self, id: u64) -> Option<&ResultSet> {
        self.results.iter().find(|r| r.id == id)
    }

    pub fn active(&self) -> Option<&ResultSet> {
        self.active_result.and_then(|id| self.result(id))
    }

    /// Requests in flight (connecting, queries, introspection): the UI keeps repainting.
    pub fn busy(&self) -> bool {
        self.sources.iter().any(|s| {
            s.status == ConnStatus::Connecting
                || s.tree.loading
                || s.tree
                    .schemas
                    .values()
                    .any(|n| n.loading || n.tables.as_ref().is_some_and(|t| t.iter().any(|t| t.loading || t.counting)))
        }) || self.results.iter().any(|r| r.is_running() || r.edits.submitting)
            || self.runs.values().any(|r| !r.waiting)
            || self.dialog.as_ref().is_some_and(|d| d.test == TestState::Running)
            || self.import.as_ref().is_some_and(ImportDialog::is_busy)
            || self.data.busy()
            || self.txs.guard.is_some()
    }

    /// Whether a query run is executing on a worker (for Cancel).
    pub fn running(&self) -> bool {
        self.runs.values().any(|r| !r.cancel_requested && !r.waiting)
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Whether the connection `worker` has an open transaction.
    pub fn transaction_open(&self, worker: &str) -> bool {
        self.workers.get(worker).is_some_and(|w| w.in_tx)
    }

    /// Connections with an open transaction (of one data source, or all).
    pub fn open_transactions(&self, source: Option<&str>) -> Vec<String> {
        let mut v: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, w)| w.in_tx && source.is_none_or(|s| w.source == s))
            .map(|(k, _)| k.clone())
            .collect();
        v.sort();
        v
    }

    /// End the transaction on `worker` (Commit or Rollback); the outcome arrives as an event.
    pub(crate) fn end_transaction(&mut self, worker: &str, commit: bool) -> Result<(), String> {
        let source = self.workers.get(worker).map(|w| w.source.clone()).ok_or("the connection is closed")?;
        self.close_cursors(worker, &source);
        self.log(OutputLevel::Sql, &source, if commit { "COMMIT" } else { "ROLLBACK" });
        self.send(worker, Req::Tx { commit })
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub(crate) fn log(&mut self, level: OutputLevel, source: &str, text: impl Into<String>) {
        self.output.push(OutputLine { level, source: source.to_string(), text: text.into() });
        if self.output.len() > MAX_OUTPUT {
            let extra = self.output.len() - MAX_OUTPUT;
            self.output.drain(..extra);
        }
    }

    /// The data source a document runs against: its console directory, its binding, or the only
    /// data source.
    pub fn source_for_path(&self, path: Option<&Path>) -> Option<String> {
        if let Some(p) = path {
            if let Some(root) = &self.root
                && let Ok(rel) = p.strip_prefix(root.join(CONSOLES_DIR))
                && let Some(first) = rel.iter().next()
            {
                let id = first.to_string_lossy();
                if self.source(&id).is_some() {
                    return Some(id.into_owned());
                }
            }
            if let Some(id) = self.bindings.get(p).filter(|id| self.source(id).is_some()) {
                return Some(id.clone());
            }
        }
        match self.sources.as_slice() {
            [only] => Some(only.config.id.clone()),
            _ => None,
        }
    }

    /// The config with a relative SQLite path resolved against the project.
    fn resolved(&self, cfg: &DataSourceConfig) -> DataSourceConfig {
        let mut c = cfg.clone();
        if let (Some(p), Some(root)) = (&c.path, &self.root)
            && p.is_relative()
        {
            c.path = Some(root.join(p));
        }
        if let Some(root) = &self.root {
            for k in ["sslrootcert", "sslcert", "sslkey"] {
                if let Some(v) = c.options.get_mut(k)
                    && !v.trim().is_empty()
                    && Path::new(v.trim()).is_relative()
                {
                    *v = root.join(v.trim()).display().to_string();
                }
            }
        }
        c
    }

    fn worker_password(&self, id: &str) -> Option<String> {
        self.passwords.get(id).cloned()
    }

    /// Start (or reuse) the worker `key` connected to `source`.
    fn ensure_worker(&mut self, key: &str, source: &str) -> Result<(), String> {
        if self.workers.contains_key(key) {
            return Ok(());
        }
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        let cfg = self.resolved(&ds.config);
        let tunnel = self.plan_tunnel(&cfg)?;
        let opts = self.link_opts(key, &cfg);
        let reroute = tx::reroute(self, &cfg);
        let password = self.worker_password(source);
        let secrets = self.secrets.clone();
        let events = self.tx.clone();
        let (tx, rx) = unbounded();
        let name = key.to_string();
        std::thread::Builder::new()
            .name(format!("db-{}", key.chars().take(40).collect::<String>()))
            .spawn(move || match crate::ssh_tunnel::route(tunnel, cfg) {
                Ok(cfg) => worker_main(name, cfg, password, secrets, rx, events, (opts, reroute)),
                Err(error) => {
                    let _ = events.send(Event::ConnectFailed { worker: name, error });
                }
            })
            .map_err(|e| format!("cannot start a database thread: {e}"))?;
        self.workers.insert(
            key.to_string(),
            Worker { tx, source: source.to_string(), canceller: None, connected: false, in_tx: false },
        );
        if key.starts_with("meta:")
            && let Some(ds) = self.source_mut(source)
            && !matches!(ds.status, ConnStatus::Connected(_))
        {
            ds.status = ConnStatus::Connecting;
        }
        Ok(())
    }

    fn send(&mut self, key: &str, req: Req) -> Result<(), String> {
        let w = self.workers.get(key).ok_or_else(|| format!("no connection `{key}`"))?;
        if w.tx.send(req).is_err() {
            self.workers.remove(key);
            return Err("the connection was closed".into());
        }
        Ok(())
    }

    /// Ask the data source's introspection connection for something (connecting if needed).
    pub(crate) fn meta(&mut self, source: &str, req: MetaReq) -> Result<(), String> {
        let key = format!("meta:{source}");
        self.ensure_worker(&key, source)?;
        let Some(ds) = self.source_mut(source) else { return Err(format!("no data source `{source}`")) };
        match &req {
            MetaReq::Info => {
                ds.tree.loading = true;
                ds.tree.error = None;
            }
            MetaReq::Tables(schema) => {
                let node = ds.tree.schemas.entry(schema.clone()).or_default();
                node.loading = true;
                node.error = None;
            }
            MetaReq::Details(schema, table) => {
                if let Some(t) = ds.table_mut(schema, table) {
                    t.loading = true;
                    t.error = None;
                }
            }
            MetaReq::Count(schema, table) => {
                if let Some(t) = ds.table_mut(schema, table) {
                    t.counting = true;
                }
            }
        }
        self.send(&key, Req::Meta(req))
    }

    /// A new request on `worker` closes any cursor left open there. With SQLite an open read
    /// cursor on another connection would also block writes, so close those too.
    fn close_cursors(&mut self, worker: &str, source: &str) {
        let sqlite = self.source(source).is_some_and(|d| d.config.kind == DbKind::Sqlite);
        let mut close: Vec<String> = Vec::new();
        for r in self.results.iter_mut().filter(|r| r.has_more && (r.worker == worker || sqlite && r.source == source))
        {
            r.has_more = false;
            r.truncated = true;
            if r.worker != worker && !close.contains(&r.worker) {
                close.push(r.worker.clone());
            }
        }
        for w in close {
            let _ = self.send(&w, Req::StopFetch);
        }
    }

    /// Run statements on a worker; returns the run id.
    pub(crate) fn start_run(&mut self, spec: RunSpec) -> Result<u64, String> {
        let RunSpec {
            worker,
            source,
            owner,
            title,
            statements,
            table,
            sort,
            replace,
            base_sql,
            filter,
            manual,
            explain,
        } = spec;
        if statements.is_empty() {
            return Err("nothing to execute".into());
        }
        self.ensure_worker(&worker, &source)?;
        self.close_cursors(&worker, &source);
        // A new run replaces the owner's earlier tabs (re-sorting replaces just its own tab).
        if replace.is_none() {
            let stale: Vec<u64> =
                self.results.iter().filter(|r| r.owner == owner && !r.is_running()).map(|r| r.id).collect();
            self.results.retain(|r| !stale.contains(&r.id));
            if self.active_result.is_some_and(|a| stale.contains(&a)) {
                self.active_result = None;
            }
        }
        let run = self.next();
        for sql in &statements {
            self.log(OutputLevel::Sql, &source, sql.clone());
        }
        self.runs.insert(
            run,
            RunInfo {
                worker: worker.clone(),
                source,
                owner,
                title,
                statements: statements.clone(),
                replace,
                table,
                sort,
                base_sql,
                filter,
                started: Instant::now(),
                cancel_requested: false,
                waiting: false,
            },
        );
        let req = match explain {
            Some(analyze) => {
                Req::Explain { run, sql: statements.into_iter().next().unwrap_or_default(), analyze, manual }
            }
            None => Req::Exec { run, statements, manual },
        };
        if let Err(e) = self.send(&worker, req) {
            self.runs.remove(&run);
            return Err(e);
        }
        Ok(run)
    }

    /// Fetch the next page of a result whose cursor is open.
    pub(crate) fn fetch_more(&mut self, result: u64) -> Result<(), String> {
        let r = self.result(result).ok_or_else(|| format!("no result {result}"))?;
        if !r.has_more {
            return Err("all rows are already fetched".into());
        }
        if r.rows.len() >= MAX_RESULT_ROWS {
            return Err(format!("result sets keep at most {MAX_RESULT_ROWS} rows; export or refine the query"));
        }
        let (worker, run) = (r.worker.clone(), r.run);
        self.send(&worker, Req::More { run })?;
        if let Some(info) = self.runs.get_mut(&run) {
            info.waiting = false;
        }
        if let Some(r) = self.results.iter_mut().find(|r| r.id == result) {
            r.has_more = false;
            r.status = ResultStatus::Fetching;
        }
        Ok(())
    }

    /// Cancel what runs on the worker of `result` (or every running query): interrupt a running
    /// statement, or close an open cursor.
    pub(crate) fn cancel(&mut self, result: Option<u64>) -> usize {
        let workers: Vec<String> = match result.and_then(|id| self.result(id)) {
            Some(r) => vec![r.worker.clone()],
            None => self.runs.values().map(|r| r.worker.clone()).collect(),
        };
        let mut n = 0;
        for w in workers {
            let running: Vec<u64> = self.runs.iter().filter(|(_, r)| r.worker == w).map(|(id, _)| *id).collect();
            for id in &running {
                if let Some(r) = self.runs.get_mut(id) {
                    r.cancel_requested = true;
                }
            }
            let fetching = self.results.iter().any(|r| r.worker == w && r.status == ResultStatus::Fetching);
            let idle_cursor = self.results.iter().any(|r| r.worker == w && r.has_more);
            if idle_cursor && !fetching {
                let _ = self.send(&w, Req::StopFetch);
                for r in self.results.iter_mut().filter(|r| r.worker == w && r.has_more) {
                    r.has_more = false;
                    r.truncated = true;
                }
                n += 1;
            } else if !running.is_empty()
                && let Some(c) = self.workers.get(&w).and_then(|w| w.canceller.clone())
            {
                let events = self.tx.clone();
                // Cancelling may connect to the server: never on the UI thread.
                let _ = std::thread::Builder::new().name("db-cancel".into()).spawn(move || {
                    if let Err(e) = c.cancel() {
                        let _ = events.send(Event::SecretFailed(e.to_string()));
                    }
                });
                n += 1;
            }
        }
        n
    }

    /// Close every connection of a data source (or all of them).
    pub(crate) fn disconnect(&mut self, source: Option<&str>) {
        self.tunnels.close(source);
        let keys: Vec<String> =
            self.workers.iter().filter(|(_, w)| source.is_none_or(|s| w.source == s)).map(|(k, _)| k.clone()).collect();
        for k in keys {
            if let Some(w) = self.workers.remove(&k) {
                if w.in_tx {
                    let msg = format!("{k}: the open transaction was rolled back (disconnected)");
                    self.log(OutputLevel::Info, &w.source, msg);
                }
                if self.runs.values().any(|r| r.worker == k)
                    && let Some(c) = w.canceller
                {
                    let _ = std::thread::Builder::new().name("db-cancel".into()).spawn(move || {
                        let _ = c.cancel();
                    });
                }
                let _ = w.tx.send(Req::Close);
            }
            self.runs.retain(|_, r| r.worker != k);
            self.txs.conns.remove(&k);
            for r in self.results.iter_mut().filter(|r| r.worker == k) {
                if r.has_more {
                    r.has_more = false;
                    r.truncated = true;
                }
                if r.is_running() {
                    r.status = ResultStatus::Cancelled;
                }
            }
        }
        let prefix = source.map(|s| node_key(s, Some(""), None));
        self.table_keys.retain(|k, _| prefix.as_ref().is_some_and(|p| !k.starts_with(p.as_str())));
        for r in self.results.iter_mut().filter(|r| source.is_none_or(|s| r.source == s)) {
            r.edits.submitting = false;
        }
        for ds in self.sources.iter_mut().filter(|d| source.is_none_or(|s| d.config.id == s)) {
            ds.status = ConnStatus::Disconnected;
            ds.tree = SchemaTree::default();
        }
    }

    /// Test a configuration on a worker thread; the result lands in the dialog and the status bar.
    pub(crate) fn test(&mut self, cfg: DataSourceConfig, password: Option<String>) -> u64 {
        let nonce = self.next();
        let cfg = self.resolved(&cfg);
        let tunnel = self.plan_tunnel(&cfg).map(|t| t.map(|t| t.keyed(format!("test:{nonce}"))));
        let tunnels = self.tunnels.clone();
        let password = password.or_else(|| self.worker_password(&cfg.id));
        let secrets = self.secrets.clone();
        let events = self.tx.clone();
        let spawned = std::thread::Builder::new().name("db-test".into()).spawn(move || {
            let cfg = match tunnel.and_then(|t| crate::ssh_tunnel::route(t, cfg)) {
                Ok(c) => c,
                Err(e) => {
                    let _ = events.send(Event::Tested { nonce, result: Err(e) });
                    return;
                }
            };
            let pw = match password {
                Some(p) => Some(p),
                None if cfg.password_saved => secrets.get(&cfg.id).ok().flatten(),
                None => None,
            };
            let result = codecraft_db::test_connection(&cfg, pw.as_deref()).map_err(|e| e.to_string());
            tunnels.close(Some(&format!("test:{nonce}")));
            let _ = events.send(Event::Tested { nonce, result });
        });
        if let Err(e) = spawned {
            let _ = self.tx.send(Event::Tested { nonce, result: Err(e.to_string()) });
        }
        if let Some(d) = &mut self.dialog {
            d.test = TestState::Running;
            d.test_nonce = nonce;
        }
        nonce
    }

    /// The SSH tunnel of a data source, with its password or passphrase from this session or
    /// the keychain.
    pub(crate) fn plan_tunnel(&self, cfg: &DataSourceConfig) -> Result<Option<crate::ssh_tunnel::TunnelPlan>, String> {
        let account = cfg.ssh_secret_account();
        let saved = cfg.ssh.as_ref().is_some_and(|t| t.secret_saved);
        self.tunnels.plan_with(cfg, self.passwords.get(&account).cloned(), saved.then(|| self.secrets.clone()))
    }

    /// Remember a password for the session, and save it in the keychain (in the background).
    pub(crate) fn store_password(&mut self, id: &str, password: &str, save: bool) {
        self.passwords.insert(id.to_string(), password.to_string());
        let secrets = self.secrets.clone();
        let events = self.tx.clone();
        let (id, password) = (id.to_string(), password.to_string());
        let _ = std::thread::Builder::new().name("db-secret".into()).spawn(move || {
            let r = if save { secrets.set(&id, &password) } else { secrets.delete(&id) };
            if let Err(e) = r {
                let _ = events.send(Event::SecretFailed(e.to_string()));
            }
        });
    }

    /// Remember a secret for this session only (Test Connection with a typed SSH password).
    pub(crate) fn remember_secret(&mut self, account: &str, secret: &str) {
        self.passwords.insert(account.to_string(), secret.to_string());
    }

    /// Forget a data source's password (session and keychain).
    pub(crate) fn forget_password(&mut self, id: &str, in_keychain: bool) {
        self.passwords.remove(id);
        if in_keychain {
            let secrets = self.secrets.clone();
            let id = id.to_string();
            let _ = std::thread::Builder::new().name("db-secret".into()).spawn(move || {
                let _ = secrets.delete(&id);
            });
        }
    }

    // ------------------------------------------------------------ persistence

    pub(crate) fn save(&mut self) -> Result<(), String> {
        let Some(root) = self.root.clone() else { return Ok(()) };
        let stored = Stored {
            version: 1,
            data_sources: self.sources.iter().map(|s| s.config.clone()).collect(),
            bindings: self
                .bindings
                .iter()
                .map(|(p, id)| (p.strip_prefix(&root).unwrap_or(p).display().to_string(), id.clone()))
                .collect(),
        };
        let json = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
        let path = root.join(DATASOURCES_FILE);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        codecraft_fs::write_text_file_atomic(&path, &format!("{json}\n")).map_err(|e| e.to_string())
    }

    fn load(&mut self, root: Option<PathBuf>) {
        self.disconnect(None);
        self.sources.clear();
        self.results.clear();
        self.active_result = None;
        self.bindings.clear();
        self.expanded.clear();
        self.selected = None;
        self.dialog = None;
        self.history_popup = None;
        self.param_prompt = None;
        self.confirm = None;
        self.import = None;
        self.param_values.clear();
        self.history.set_project(root.clone());
        self.passwords.clear();
        self.root = root.clone();
        self.loaded = true;
        let Some(root) = root else { return };
        let path = root.join(DATASOURCES_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        match serde_json::from_str::<Stored>(&text) {
            Ok(stored) => {
                let mut seen = BTreeSet::new();
                for cfg in stored.data_sources {
                    if cfg.id.is_empty() || !seen.insert(cfg.id.clone()) {
                        continue;
                    }
                    self.sources.push(DataSource {
                        config: cfg,
                        status: ConnStatus::Disconnected,
                        tree: SchemaTree::default(),
                    });
                }
                for (p, id) in stored.bindings {
                    self.bindings.insert(root.join(p), id);
                }
            }
            Err(e) => {
                // Keep the unreadable file for the user instead of overwriting it later.
                let backup = path.with_extension("json.bad");
                let _ = std::fs::rename(&path, &backup);
                self.log(
                    OutputLevel::Error,
                    "",
                    format!("{} is not valid ({e}); moved it to {}", path.display(), backup.display()),
                );
            }
        }
    }

    /// A fresh id for a data source name (`local-pg`, `local-pg-2`, …).
    pub(crate) fn new_source_id(&self, name: &str) -> String {
        let mut slug: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        slug.truncate(40);
        if slug.is_empty() {
            slug = "datasource".into();
        }
        let mut id = slug.clone();
        let mut n = 2;
        while self.source(&id).is_some() {
            id = format!("{slug}-{n}");
            n += 1;
        }
        id
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    data_sources: Vec<DataSourceConfig>,
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

/// Load the project's data sources when the project changed.
pub(crate) fn sync_project(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if s.db.loaded && s.db.root == root {
        return false;
    }
    s.db.load(root);
    true
}

// ---------------------------------------------------------------- worker thread

struct WorkerSink<'a> {
    run: u64,
    stmt: usize,
    /// Keep the cursor open after a page and wait for `More` (the last statement of a run).
    wait: bool,
    rx: &'a Receiver<Req>,
    events: &'a Sender<Event>,
    /// A request that arrived while waiting; handled after this statement stops.
    deferred: &'a mut Option<Req>,
}

impl ResultSink for WorkerSink<'_> {
    fn columns(&mut self, columns: &[Column]) {
        let _ = self.events.send(Event::Columns { run: self.run, stmt: self.stmt, columns: columns.to_vec() });
    }

    fn page(&mut self, page: Page) -> bool {
        let Page { rows, last } = page;
        let waiting = !last && self.wait;
        let _ = self.events.send(Event::Page { run: self.run, stmt: self.stmt, rows, last, waiting });
        if last {
            return true;
        }
        if !waiting {
            return false;
        }
        loop {
            match self.rx.recv() {
                Ok(Req::More { run }) if run == self.run => return true,
                Ok(Req::More { .. }) => {}
                Ok(Req::StopFetch) | Err(_) => return false,
                Ok(other) => {
                    *self.deferred = Some(other);
                    return false;
                }
            }
        }
    }
}

fn worker_main(
    key: String,
    cfg: DataSourceConfig,
    password: Option<String>,
    secrets: Arc<dyn SecretStore>,
    rx: Receiver<Req>,
    events: Sender<Event>,
    (opts, reroute): (tx::LinkOpts, Option<tx::Reroute>),
) {
    let password = match password {
        Some(p) => Some(p),
        None if cfg.password_saved => match secrets.get(&cfg.id) {
            Ok(p) => p,
            Err(e) => {
                let _ = events.send(Event::ConnectFailed { worker: key, error: e.to_string() });
                return;
            }
        },
        None => None,
    };
    let mut link = tx::Link::new(key.clone(), cfg, password, reroute, opts, events.clone());
    let version = match link.open() {
        Ok(v) => v,
        Err(error) => {
            let _ = events.send(Event::ConnectFailed { worker: key, error });
            return;
        }
    };
    if let Some(canceller) = link.canceller() {
        let _ = events.send(Event::Connected { worker: key.clone(), version, canceller });
    }
    let dialect = link.kind().dialect();
    let mut deferred: Option<Req> = None;
    loop {
        let req = match deferred.take() {
            Some(r) => r,
            None => match link.recv(&rx) {
                Some(r) => r,
                None => break,
            },
        };
        match req {
            Req::Close => break,
            Req::More { .. } | Req::StopFetch => {}
            Req::Link(r) => link.handle(r),
            Req::Meta(m) => {
                let result = link.read(|conn| match &m {
                    MetaReq::Info => conn.database_info().map(MetaResult::Info),
                    MetaReq::Tables(schema) => conn.tables(schema).map(MetaResult::Tables),
                    MetaReq::Details(schema, table) => conn.table_details(schema, table).map(MetaResult::Details),
                    MetaReq::Count(schema, table) => conn.row_count(schema, table).map(MetaResult::Count),
                });
                let _ = events.send(Event::Meta { worker: key.clone(), req: m, result });
            }
            Req::Apply { result, schema, table, changes } => {
                let in_tx = link.in_tx;
                let outcome = link.ready().map_err(|e| codecraft_db::edit::ApplyFailure {
                    index: None,
                    statement: None,
                    error: e,
                    rolled_back: true,
                });
                let outcome = outcome.and_then(|()| match link.conn() {
                    Ok(conn) => codecraft_db::edit::apply_changes(conn, &schema, &table, &changes, in_tx),
                    Err(e) => Err(codecraft_db::edit::ApplyFailure {
                        index: None,
                        statement: None,
                        error: e,
                        rolled_back: true,
                    }),
                });
                if let Ok(r) = &outcome
                    && r.in_transaction
                {
                    link.statements += changes.len();
                }
                link.check_broken();
                link.refresh();
                let _ = events.send(Event::Applied { worker: key.clone(), result, outcome });
            }
            Req::Tx { commit } => {
                let result = link.end(commit);
                let _ = events.send(Event::TxDone { worker: key.clone(), commit, result });
                link.refresh();
            }
            Req::Explain { run, sql, analyze, manual } => {
                let result = link.ready().and_then(|()| link.begin_if_manual(manual)).and_then(|()| {
                    let conn = link.conn()?;
                    codecraft_db::explain::explain(conn, &sql, analyze).map_err(|e| e.to_string())
                });
                link.check_broken();
                link.refresh();
                let _ = events.send(Event::Plan { run, result });
                let _ = events.send(Event::RunDone { run });
            }
            Req::Exec { run, statements, manual } => {
                if let Err(e) = link.ready().and_then(|()| link.begin_if_manual(manual)) {
                    let _ = events.send(Event::StmtFailed { run, stmt: 0, error: DbError::Query(e) });
                    link.refresh();
                    let _ = events.send(Event::RunDone { run });
                    continue;
                }
                let n = statements.len();
                for (i, sql) in statements.iter().enumerate() {
                    let mut sink = WorkerSink {
                        run,
                        stmt: i,
                        wait: i + 1 == n,
                        rx: &rx,
                        events: &events,
                        deferred: &mut deferred,
                    };
                    let read = !codecraft_db::is_modifying(sql, dialect)
                        && codecraft_db::transaction_effect(sql, dialect) == codecraft_db::TxEffect::None;
                    let retry = read && !link.in_tx;
                    let r = link.execute(sql, codecraft_db::PAGE_SIZE, &mut sink, retry);
                    link.track(sql, r.is_ok());
                    let stop = r.is_err();
                    let _ = events.send(match r {
                        Ok(summary) => Event::StmtDone { run, stmt: i, summary },
                        Err(error) => Event::StmtFailed { run, stmt: i, error },
                    });
                    if stop || deferred.is_some() {
                        break;
                    }
                }
                link.refresh();
                let _ = events.send(Event::RunDone { run });
            }
        }
    }
}

// ---------------------------------------------------------------- pump

/// Drain background events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = sync_project(s);
    for _ in 0..512 {
        let Ok(ev) = s.db.rx.try_recv() else { break };
        changed = true;
        handle(s, ev);
    }
    s.db.history.flush();
    changed
}

fn handle(s: &mut Session, ev: Event) {
    let db = &mut s.db;
    match ev {
        Event::Connected { worker, version, canceller } => {
            let Some(w) = db.workers.get_mut(&worker) else { return };
            w.canceller = Some(canceller);
            w.connected = true;
            let source = w.source.clone();
            if let Some(ds) = db.source_mut(&source) {
                ds.status = ConnStatus::Connected(version.clone());
            }
            if worker.starts_with("meta:") {
                db.log(OutputLevel::Info, &source, format!("Connected to {version}"));
                let _ = db.meta(&source, MetaReq::Info);
            }
        }
        Event::ConnectFailed { worker, error } => {
            let Some(w) = db.workers.remove(&worker) else { return };
            let source = w.source;
            let connected = db.source(&source).is_some_and(|d| matches!(d.status, ConnStatus::Connected(_)));
            if (worker.starts_with("meta:") || !connected)
                && let Some(ds) = db.source_mut(&source)
            {
                ds.status = ConnStatus::Failed(error.clone());
                if worker.starts_with("meta:") {
                    ds.tree = SchemaTree { error: Some(error.clone()), ..SchemaTree::default() };
                }
            }
            if worker.starts_with("meta:") {
                let prefix = node_key(&source, Some(""), None);
                for (_, k) in db.table_keys.iter_mut().filter(|(k, _)| k.starts_with(&prefix)) {
                    if *k == KeyState::Loading {
                        *k = KeyState::Failed(error.clone());
                    }
                }
            }
            for r in db.results.iter_mut().filter(|r| r.worker == worker && r.edits.submitting) {
                r.edits.submitting = false;
                r.edits.error = Some(error.clone());
            }
            let failed_runs: Vec<u64> = db.runs.iter().filter(|(_, r)| r.worker == worker).map(|(id, _)| *id).collect();
            for id in failed_runs {
                db.runs.remove(&id);
            }
            db.log(OutputLevel::Error, &source, format!("Connection failed: {error}"));
            db.active_result = None;
            let name = db.source(&source).map(|d| d.config.name.clone()).unwrap_or(source);
            s.notify(NotificationLevel::Error, &format!("Cannot connect to {name}"), error);
        }
        Event::Columns { run, stmt, columns } => {
            if columns.is_empty() {
                return;
            }
            let Some(info) = db.runs.get(&run) else { return };
            let sql = info.statements.get(stmt).cloned().unwrap_or_default();
            let multi = info.statements.len() > 1;
            let title = if multi { format!("{} [{}]", info.title, stmt + 1) } else { info.title.clone() };
            let (source, owner, worker, table, sort, replace, filter) = (
                info.source.clone(),
                info.owner.clone(),
                info.worker.clone(),
                info.table.clone(),
                info.sort,
                info.replace.filter(|_| stmt == 0),
                info.filter.clone(),
            );
            let base_sql = info.base_sql.clone().unwrap_or_else(|| sql.clone());
            let id = match replace.filter(|r| db.result(*r).is_some()) {
                Some(r) => r,
                None => db.next(),
            };
            let transposed = db.result(id).is_some_and(|r| r.transposed);
            let rs = ResultSet {
                id,
                title,
                source,
                owner,
                sql,
                columns,
                rows: Vec::new(),
                status: ResultStatus::Running,
                error: None,
                has_more: false,
                truncated: false,
                elapsed_ms: None,
                table,
                sort,
                edits: GridEdits::default(),
                plan: None,
                filter,
                transposed,
                base_sql,
                run,
                stmt,
                worker,
            };
            match db.results.iter_mut().find(|r| r.id == id) {
                Some(existing) => *existing = rs,
                None => db.results.push(rs),
            }
            db.active_result = Some(id);
            crate::services::show(s, crate::services::ServicesSection::Database);
        }
        Event::Page { run, stmt, rows, last, waiting } => {
            if let Some(info) = db.runs.get_mut(&run) {
                info.waiting = waiting;
            }
            let started = db.runs.get(&run).map(|r| r.started);
            let Some(r) = db.results.iter_mut().find(|r| r.run == run && r.stmt == stmt) else { return };
            let room = MAX_RESULT_ROWS.saturating_sub(r.rows.len());
            let overflow = rows.len() > room;
            r.rows.extend(rows.into_iter().take(room));
            if r.elapsed_ms.is_none()
                && let Some(t) = started
            {
                r.elapsed_ms = Some(u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
            }
            if !last {
                r.has_more = waiting && !overflow;
                r.truncated = !waiting || overflow;
                if r.status != ResultStatus::Cancelled {
                    r.status = ResultStatus::Done;
                }
            }
        }
        Event::StmtDone { run, stmt, summary } => {
            let source = db.runs.get(&run).map(|r| r.source.clone()).unwrap_or_default();
            let line = match summary.rows_affected {
                Some(n) => format!("{n} row{} affected in {} ms", if n == 1 { "" } else { "s" }, summary.elapsed_ms),
                None if summary.columns.is_empty() => format!("completed in {} ms", summary.elapsed_ms),
                None => format!(
                    "{} row{} retrieved in {} ms",
                    summary.rows_fetched,
                    if summary.rows_fetched == 1 { "" } else { "s" },
                    summary.elapsed_ms
                ),
            };
            if let Some(r) = db.results.iter_mut().find(|r| r.run == run && r.stmt == stmt) {
                r.status = ResultStatus::Done;
                r.has_more = false;
                if !summary.complete {
                    r.truncated = true;
                }
                if summary.complete {
                    r.elapsed_ms = Some(summary.elapsed_ms);
                }
            }
            db.log(OutputLevel::Info, &source, line.clone());
            s.status = line;
        }
        Event::StmtFailed { run, stmt, error } => {
            let (source, cancel_requested) =
                db.runs.get(&run).map(|r| (r.source.clone(), r.cancel_requested)).unwrap_or_default();
            let cancelled = error == DbError::Cancelled || cancel_requested && matches!(error, DbError::Query(_));
            let msg = if cancelled { "Query cancelled".to_string() } else { error.to_string() };
            let mut has_tab = false;
            if let Some(r) = db.results.iter_mut().find(|r| r.run == run && r.stmt == stmt) {
                has_tab = true;
                r.has_more = false;
                if cancelled {
                    r.status = ResultStatus::Cancelled;
                } else {
                    r.status = ResultStatus::Failed;
                    r.error = Some(msg.clone());
                }
            }
            db.log(if cancelled { OutputLevel::Info } else { OutputLevel::Error }, &source, msg.clone());
            if !has_tab {
                db.active_result = None;
                crate::services::show(s, crate::services::ServicesSection::Database);
            }
            s.status = msg;
        }
        Event::RunDone { run } => {
            db.runs.remove(&run);
        }
        Event::Meta { worker, req, result } => {
            let Some(source) = worker.strip_prefix("meta:").map(str::to_string) else { return };
            on_meta(db, &source, req, result);
        }
        Event::Tested { nonce, result } => {
            let msg = match &result {
                Ok(v) => format!("Connection successful: {v}"),
                Err(e) => format!("Connection failed: {e}"),
            };
            if let Some(d) = &mut db.dialog
                && d.test_nonce == nonce
            {
                d.test = match result {
                    Ok(v) => TestState::Ok(v),
                    Err(e) => TestState::Failed(e),
                };
            }
            s.status = msg;
        }
        Event::Applied { worker, result, outcome } => on_applied(s, &worker, result, outcome),
        Event::Plan { run, result } => on_plan(s, run, result),
        Event::TxState { worker, open } => {
            if let Some(w) = db.workers.get_mut(&worker) {
                w.in_tx = open;
            }
        }
        Event::TxDone { worker, commit, result } => {
            let result_copy = result.clone();
            let source = db.workers.get(&worker).map(|w| w.source.clone()).unwrap_or_default();
            let msg = match result {
                Ok(()) if commit => "Transaction committed".to_string(),
                Ok(()) => "Transaction rolled back".to_string(),
                Err(e) => format!("{} failed: {e}", if commit { "Commit" } else { "Rollback" }),
            };
            let level = if msg.contains("failed") { OutputLevel::Error } else { OutputLevel::Info };
            db.log(level, &source, msg.clone());
            s.status = msg;
            tx::on_tx_done(s, &worker, &result_copy);
        }
        Event::Import(ev) => import::on_event(s, ev),
        Event::Tx(ev) => tx::on_event(s, ev),
        Event::Data(ev) => data_tools::on_event(s, ev),
        Event::SecretFailed(e) => {
            db.log(OutputLevel::Error, "", e.clone());
            s.status = e;
        }
    }
}

fn on_meta(db: &mut DbState, source: &str, req: MetaReq, result: Result<MetaResult, String>) {
    if let MetaReq::Details(schema, table) = &req {
        let k = node_key(source, Some(schema), Some(table));
        if db.table_keys.contains_key(&k) {
            let state = match &result {
                Ok(MetaResult::Details(d)) => {
                    KeyState::Ready(d.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect())
                }
                Ok(_) => KeyState::Failed("unexpected reply".into()),
                Err(e) => KeyState::Failed(e.clone()),
            };
            db.table_keys.insert(k, state);
        }
    }
    let mut follow_up: Vec<MetaReq> = Vec::new();
    let mut expand: Option<String> = None;
    let Some(ds) = db.source_mut(source) else { return };
    let err = result.as_ref().err().cloned();
    match (req, result) {
        (MetaReq::Info, Ok(MetaResult::Info(info))) => {
            ds.tree.loading = false;
            let old = std::mem::take(&mut ds.tree.schemas);
            for name in &info.schemas {
                let node = old.get(name).cloned().unwrap_or_default();
                ds.tree.schemas.insert(name.clone(), node);
            }
            // Load the default schema right away: it is what consoles complete against.
            let default = info
                .default_schema
                .clone()
                .filter(|d| info.schemas.contains(d))
                .or_else(|| (info.schemas.len() == 1).then(|| info.schemas.first().cloned()).flatten());
            if let Some(d) = default {
                if ds.tree.schemas.get(&d).is_none_or(|n| n.tables.is_none()) {
                    follow_up.push(MetaReq::Tables(d.clone()));
                }
                expand = Some(node_key(source, Some(&d), None));
            }
            ds.tree.info = Some(info);
        }
        (MetaReq::Info, _) => {
            ds.tree.loading = false;
            ds.tree.error = err.clone();
        }
        (MetaReq::Tables(schema), Ok(MetaResult::Tables(tables))) => {
            let node = ds.tree.schemas.entry(schema).or_default();
            node.loading = false;
            node.error = None;
            node.tables = Some(tables.into_iter().map(TableNode::new).collect());
        }
        (MetaReq::Tables(schema), _) => {
            let node = ds.tree.schemas.entry(schema).or_default();
            node.loading = false;
            node.error = err.clone();
        }
        (MetaReq::Details(schema, table), r) => {
            if let Some(t) = ds.table_mut(&schema, &table) {
                t.loading = false;
                match r {
                    Ok(MetaResult::Details(d)) => t.details = Some(d),
                    _ => t.error = err.clone(),
                }
            }
        }
        (MetaReq::Count(schema, table), r) => {
            if let Some(t) = ds.table_mut(&schema, &table) {
                t.counting = false;
                match r {
                    Ok(MetaResult::Count(n)) => t.row_count = Some(n),
                    _ => t.error = err.clone(),
                }
            }
        }
    }
    if let Some(e) = err {
        db.log(OutputLevel::Error, source, e);
    }
    if let Some(k) = expand
        && db.expanded.contains(&node_key(source, None, None))
    {
        db.expanded.insert(k);
    }
    for r in follow_up {
        let _ = db.meta(source, r);
    }
}

/// An Explain Plan finished: show the plan in its own result tab (one per console).
fn on_plan(s: &mut Session, run: u64, result: Result<Plan, String>) {
    let db = &mut s.db;
    let Some(info) = db.runs.get(&run) else { return };
    let (source, worker, title) = (info.source.clone(), info.worker.clone(), info.title.clone());
    let sql = info.statements.first().cloned().unwrap_or_default();
    let elapsed = u64::try_from(info.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(plan) => {
            let owner = format!("plan:{worker}");
            let stale: Vec<u64> = db.results.iter().filter(|r| r.owner == owner).map(|r| r.id).collect();
            db.results.retain(|r| !stale.contains(&r.id));
            let id = db.next();
            let msg =
                format!("Plan of {} steps in {elapsed} ms", plan.roots.iter().map(PlanNode::count).sum::<usize>());
            db.results.push(ResultSet {
                id,
                title: format!("Plan: {title}"),
                source: source.clone(),
                owner,
                sql: sql.clone(),
                columns: Vec::new(),
                rows: Vec::new(),
                status: ResultStatus::Done,
                error: None,
                has_more: false,
                truncated: false,
                elapsed_ms: Some(elapsed),
                table: None,
                sort: None,
                edits: GridEdits::default(),
                plan: Some(plan),
                filter: ResultFilter::default(),
                transposed: false,
                base_sql: sql,
                run,
                stmt: 0,
                worker,
            });
            db.active_result = Some(id);
            db.log(OutputLevel::Info, &source, msg.clone());
            s.status = msg;
            crate::services::show(s, crate::services::ServicesSection::Database);
        }
        Err(e) => {
            db.log(OutputLevel::Error, &source, e.clone());
            db.active_result = None;
            s.status = e;
            crate::services::show(s, crate::services::ServicesSection::Database);
        }
    }
}

/// A submit finished: reload the table on success, keep the changes and say exactly what failed
/// otherwise (the failing row is marked in the grid).
fn on_applied(
    s: &mut Session,
    worker: &str,
    result: u64,
    outcome: Result<codecraft_db::edit::ApplyReport, codecraft_db::edit::ApplyFailure>,
) {
    let db = &mut s.db;
    let Some(r) = db.results.iter_mut().find(|r| r.id == result) else { return };
    r.edits.submitting = false;
    match outcome {
        Ok(report) => {
            let n = report.affected;
            r.edits = GridEdits::default();
            let r = r.clone();
            let mut msg = format!("Changes submitted: {n} row{} affected", if n == 1 { "" } else { "s" });
            if report.in_transaction {
                msg.push_str(" (in the open transaction: Commit to save them)");
            }
            db.log(OutputLevel::Info, &r.source, msg.clone());
            s.status = msg;
            let reload = RunSpec {
                worker: worker.to_string(),
                source: r.source.clone(),
                owner: r.owner.clone(),
                title: r.title.clone(),
                statements: vec![r.sql.clone()],
                table: r.table.clone(),
                sort: r.sort,
                replace: Some(r.id),
                base_sql: Some(r.base_sql.clone()),
                filter: r.filter.clone(),
                ..RunSpec::default()
            };
            if let Err(e) = s.db.start_run(reload) {
                s.db.log(OutputLevel::Error, &r.source, format!("Cannot reload the table: {e}"));
            }
        }
        Err(f) => {
            let fetched = r.rows.len();
            r.edits.failed_row = f.index.and_then(|i| r.edits.change_rows(fetched).get(i).copied());
            let mut e = f.to_string();
            if let Some(row) = r.edits.failed_row {
                e = format!("Row {}: {e}", row + 1);
            }
            r.edits.error = Some(e.clone());
            let source = r.source.clone();
            db.log(OutputLevel::Error, &source, e.clone());
            s.notify(NotificationLevel::Error, "Cannot submit changes", e);
        }
    }
}

/// Stop background work (window closing): cancel running queries and close every connection.
pub(crate) fn shutdown(s: &mut Session) {
    s.db.disconnect(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_and_ids_are_unique() {
        let o = parse_options("sslmode=require; connect_timeout = 5;;bad; =x");
        assert_eq!(o.len(), 2);
        assert_eq!(o.get("connect_timeout").map(String::as_str), Some("5"));
        let mut db = DbState::default();
        assert_eq!(db.new_source_id("Local PG!"), "local-pg");
        db.sources.push(DataSource {
            config: DataSourceConfig { id: "local-pg".into(), ..Default::default() },
            status: ConnStatus::Disconnected,
            tree: SchemaTree::default(),
        });
        assert_eq!(db.new_source_id("local pg"), "local-pg-2");
        assert_eq!(db.new_source_id("ü"), "datasource");
    }

    #[test]
    fn form_defaults_follow_the_kind() {
        let mut f = DataSourceForm::for_kind(DbKind::Postgres);
        assert_eq!((f.port.as_str(), f.host.as_str(), f.database.as_str()), ("5432", "localhost", "postgres"));
        f.set_kind(DbKind::Mysql);
        assert_eq!(f.port, "3306");
        f.port = "3307".into();
        f.set_kind(DbKind::Postgres);
        assert_eq!(f.port, "3307");
        f.password = "pw".into();
        let p = f.to_params();
        assert_eq!(p["password"], "pw");
        assert!(!serde_json::to_string(&f).unwrap().contains("\"pw\""));
    }

    #[test]
    fn status_lines() {
        let mut r = ResultSet {
            id: 1,
            title: "t".into(),
            source: "s".into(),
            owner: "o".into(),
            sql: String::new(),
            columns: Vec::new(),
            rows: vec![Vec::new(); 500],
            status: ResultStatus::Done,
            error: None,
            has_more: true,
            truncated: false,
            elapsed_ms: Some(12),
            table: None,
            sort: None,
            edits: GridEdits::default(),
            plan: None,
            filter: ResultFilter::default(),
            transposed: false,
            base_sql: String::new(),
            run: 0,
            stmt: 0,
            worker: String::new(),
        };
        assert_eq!(r.status_line(), "500 rows fetched in 12 ms; more available");
        r.has_more = false;
        assert_eq!(r.status_line(), "500 rows retrieved in 12 ms");
    }
}
