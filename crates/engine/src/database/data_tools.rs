//! Data tools (track DB): Compare Data, Dump and Restore, Export to File and Full-text Search.
//! OWNED BY the database track.
//!
//! Each runs as a background job on its own thread(s) and connection(s) (through the data
//! source's SSH tunnel when it has one), reports progress and log lines through the database
//! event channel, and can be cancelled. Results stay in [`DataTools::jobs`] for the UI and for
//! `db.job.get`.

use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use codecraft_db::compare::{self, CompareResult, RowStream};
use codecraft_db::dump::{self, DumpOptions, ExportSink, StreamWriter};
use codecraft_db::export::{ExportFormat, InsertTarget};
use codecraft_db::sql::{qualified, quote_ident};
use codecraft_db::tx::{Watchdog, execute_timed};
use codecraft_db::{Cell, CollectSink, Column, Connection, DataSourceConfig, DbKind, SecretStore, TableKind};
use codecraft_workspace::NotificationLevel;

use super::{DbState, Event, OutputLevel};
use crate::Session;

/// Log lines kept per job.
const MAX_LOG: usize = 500;
/// Rows kept per table in Full-text Search.
pub const DEFAULT_SEARCH_LIMIT: usize = 50;
/// Each table's search query may take this long.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    Compare,
    Dump,
    Restore,
    Export,
    Search,
}

impl JobKind {
    pub fn label(self) -> &'static str {
        match self {
            JobKind::Compare => "Compare Data",
            JobKind::Dump => "Dump",
            JobKind::Restore => "Restore",
            JobKind::Export => "Export to File",
            JobKind::Search => "Full-text Search",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum JobState {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

/// One side of a comparison: a table, or a query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SideSpec {
    pub data_source: String,
    pub schema: String,
    pub table: String,
    /// A query instead of a table.
    pub sql: String,
}

impl SideSpec {
    fn query(&self, kind: DbKind) -> String {
        if self.sql.trim().is_empty() {
            format!("SELECT * FROM {}", qualified(kind, &self.schema, &self.table))
        } else {
            self.sql.trim().trim_end_matches(';').to_string()
        }
    }

    pub fn label(&self) -> String {
        if self.sql.trim().is_empty() {
            if self.schema.is_empty() { self.table.clone() } else { format!("{}.{}", self.schema, self.table) }
        } else {
            let first: String = self.sql.lines().next().unwrap_or_default().chars().take(40).collect();
            format!("query: {first}")
        }
    }

    pub fn is_table(&self) -> bool {
        self.sql.trim().is_empty() && !self.table.is_empty()
    }
}

/// A finished comparison.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareJob {
    pub left: SideSpec,
    pub right: SideSpec,
    pub result: CompareResult,
    /// The right side's database (for the sync script).
    pub right_kind: DbKind,
}

/// Full-text Search hits of one table.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTable {
    pub schema: String,
    pub table: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// Columns searched.
    pub searched: Vec<String>,
    /// Primary key columns (to open a hit).
    pub key_columns: Vec<String>,
    pub error: Option<String>,
    /// More rows matched than the limit.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum JobResult {
    Compare(Box<CompareJob>),
    Search { text: String, tables: Vec<SearchTable> },
    File { path: PathBuf, rows: Option<u64>, bytes: Option<u64> },
    Restored { path: PathBuf, statements: Option<usize> },
}

/// A background data job.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataJob {
    pub id: u64,
    pub kind: JobKind,
    pub title: String,
    pub source: String,
    pub state: JobState,
    /// Progress: `done` of `total` `unit`s (rows, bytes, tables, statements).
    pub done: u64,
    pub total: Option<u64>,
    pub unit: &'static str,
    pub log: Vec<String>,
    pub result: Option<JobResult>,
    pub elapsed_ms: Option<u64>,
    #[serde(skip)]
    cancel: Arc<AtomicBool>,
    #[serde(skip)]
    started: Option<Instant>,
}

impl DataJob {
    pub fn is_running(&self) -> bool {
        self.state == JobState::Running
    }

    /// `12,345 rows`, `3.2 MB of 10 MB`.
    pub fn progress_text(&self) -> String {
        let fmt = |n: u64| {
            if self.unit == "bytes" {
                if n >= 1 << 20 {
                    format!("{:.1} MB", n as f64 / 1048576.0)
                } else {
                    format!("{:.0} KB", n as f64 / 1024.0)
                }
            } else {
                format!("{} {}", codecraft_db::value::format_number(n as f64), self.unit)
            }
        };
        match self.total {
            Some(t) if t > 0 => format!("{} of {}", fmt(self.done), fmt(t)),
            _ => fmt(self.done),
        }
    }

    /// 0..=1 when the total is known.
    pub fn fraction(&self) -> Option<f32> {
        self.total.filter(|t| *t > 0).map(|t| (self.done as f32 / t as f32).clamp(0.0, 1.0))
    }
}

/// Which tool the Data Tools dialog shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tool {
    #[default]
    Compare,
    Dump,
    Restore,
    Export,
    Search,
}

impl Tool {
    pub fn parse(s: &str) -> Option<Tool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "compare" | "comparedata" => Some(Tool::Compare),
            "dump" | "export-dump" => Some(Tool::Dump),
            "restore" => Some(Tool::Restore),
            "export" | "exporttable" => Some(Tool::Export),
            "search" | "fulltextsearch" | "fts" => Some(Tool::Search),
            _ => None,
        }
    }
}

/// The Data Tools dialog's form (the UI edits it in place; `db.dataTools.run` starts it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DataDialog {
    pub tool: Tool,
    pub left: SideSpec,
    pub right: SideSpec,
    /// Comma-separated key columns (empty: the left table's primary key).
    pub keys: String,
    /// Output or input file.
    pub path: String,
    pub format: String,
    pub dump: DumpOptions,
    /// Full-text Search.
    pub text: String,
    pub case_sensitive: bool,
    pub limit: String,
}

/// Data tools state in the [`DbState`].
#[derive(Default)]
pub struct DataTools {
    pub jobs: Vec<DataJob>,
    /// The job shown in the Data Tools window.
    pub active: Option<u64>,
    pub dialog: Option<DataDialog>,
    /// The Data Tools window is open.
    pub open: bool,
}

impl DataTools {
    pub fn job(&self, id: u64) -> Option<&DataJob> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn busy(&self) -> bool {
        self.jobs.iter().any(DataJob::is_running)
    }
}

pub(crate) enum DataEvent {
    Progress { job: u64, done: u64, total: Option<u64> },
    Log { job: u64, line: String },
    Finished { job: u64, result: Result<JobResult, String>, cancelled: bool },
}

/// What a job thread uses to report.
struct Ctx {
    id: u64,
    events: Sender<Event>,
    cancel: Arc<AtomicBool>,
}

impl Ctx {
    fn progress(&self, done: u64, total: Option<u64>) {
        let _ = self.events.send(Event::Data(DataEvent::Progress { job: self.id, done, total }));
    }
    fn log(&self, line: impl Into<String>) {
        let _ = self.events.send(Event::Data(DataEvent::Log { job: self.id, line: line.into() }));
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// How a job thread connects: the routed config, password and tunnel, prepared on the UI thread.
struct ConnSpec {
    cfg: DataSourceConfig,
    tunnel: Option<crate::ssh_tunnel::TunnelPlan>,
    password: Option<String>,
    secrets: Arc<dyn SecretStore>,
}

impl ConnSpec {
    /// Route through the tunnel and read the password (blocking: job thread).
    fn route(self) -> Result<(DataSourceConfig, Option<String>), String> {
        let cfg = crate::ssh_tunnel::route(self.tunnel, self.cfg)?;
        let pw = match self.password {
            Some(p) => Some(p),
            None if cfg.password_saved => self.secrets.get(&cfg.id).map_err(|e| e.to_string())?,
            None => None,
        };
        Ok((cfg, pw))
    }

    fn connect(self) -> Result<Box<dyn Connection>, String> {
        let (cfg, pw) = self.route()?;
        codecraft_db::connect(&cfg, pw.as_deref()).map_err(|e| e.to_string())
    }
}

impl DbState {
    fn conn_spec(&self, source: &str, key: &str) -> Result<ConnSpec, String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        let cfg = self.resolved(&ds.config);
        let tunnel = self.plan_tunnel(&cfg)?.map(|t| t.keyed(key.to_string()));
        Ok(ConnSpec { cfg, tunnel, password: self.worker_password(source), secrets: self.secrets.clone() })
    }

    /// Start a job; `run` executes on its own thread.
    fn spawn_job<F>(
        &mut self,
        kind: JobKind,
        title: String,
        source: &str,
        unit: &'static str,
        run: F,
    ) -> Result<u64, String>
    where
        F: FnOnce(&Ctx) -> Result<JobResult, String> + Send + 'static,
    {
        let id = self.next();
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = Ctx { id, events: self.tx.clone(), cancel: cancel.clone() };
        let tunnels = self.tunnels.clone();
        std::thread::Builder::new()
            .name(format!("db-job-{id}"))
            .spawn(move || {
                let result = run(&ctx);
                tunnels.close(Some(&format!("job:{id}")));
                tunnels.close(Some(&format!("job:{id}:right")));
                let cancelled = ctx.cancelled();
                let _ = ctx.events.send(Event::Data(DataEvent::Finished { job: id, result, cancelled }));
            })
            .map_err(|e| format!("cannot start the job: {e}"))?;
        self.data.jobs.push(DataJob {
            id,
            kind,
            title: title.clone(),
            source: source.to_string(),
            state: JobState::Running,
            done: 0,
            total: None,
            unit,
            log: Vec::new(),
            result: None,
            elapsed_ms: None,
            cancel,
            started: Some(Instant::now()),
        });
        // Keep the 20 most recent finished jobs.
        while self.data.jobs.iter().filter(|j| !j.is_running()).count() > 20 {
            if let Some(i) = self.data.jobs.iter().position(|j| !j.is_running()) {
                self.data.jobs.remove(i);
            } else {
                break;
            }
        }
        self.data.active = Some(id);
        self.data.open = true;
        self.log(OutputLevel::Info, source, format!("{}: {title} started", kind.label()));
        Ok(id)
    }

    /// Cancel a running job.
    pub(crate) fn cancel_job(&mut self, id: u64) -> Result<(), String> {
        let j = self.data.jobs.iter().find(|j| j.id == id).ok_or_else(|| format!("no job {id}"))?;
        if !j.is_running() {
            return Err("the job is not running".into());
        }
        j.cancel.store(true, Ordering::SeqCst);
        Ok(())
    }

    // ------------------------------------------------------------ compare

    pub(crate) fn start_compare(&mut self, left: SideSpec, right: SideSpec, keys: Vec<String>) -> Result<u64, String> {
        for (side, name) in [(&left, "left"), (&right, "right")] {
            if side.sql.trim().is_empty() && side.table.trim().is_empty() {
                return Err(format!("the {name} side needs a table or a query"));
            }
        }
        if keys.is_empty() && !left.is_table() {
            return Err("comparing queries needs `keys` (the columns that match rows)".into());
        }
        let lkind = self.source(&left.data_source).map(|d| d.config.kind).ok_or("no left data source")?;
        let rkind = self.source(&right.data_source).map(|d| d.config.kind).ok_or("no right data source")?;
        let id_hint = self.next_id + 1;
        let lspec = self.conn_spec(&left.data_source, &format!("job:{id_hint}"))?;
        let rspec = self.conn_spec(&right.data_source, &format!("job:{id_hint}:right"))?;
        let title = format!("{} ↔ {}", left.label(), right.label());
        let (l, r) = (left.clone(), right.clone());
        self.spawn_job(JobKind::Compare, title, &left.data_source.clone(), "rows", move |ctx| {
            let mut lc = lspec.connect().map_err(|e| format!("left: {e}"))?;
            let mut rc = rspec.connect().map_err(|e| format!("right: {e}"))?;
            let keys = if keys.is_empty() {
                let d = lc.table_details(&l.schema, &l.table).map_err(|e| format!("left: {e}"))?;
                let k: Vec<String> = d.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
                if k.is_empty() {
                    return Err(format!("{} has no primary key: choose the key columns", l.label()));
                }
                k
            } else {
                keys
            };
            ctx.log(format!("Comparing by {}", keys.join(", ")));
            let key_types = |c: &mut Box<dyn Connection>, kind: DbKind, q: &str| -> Result<Vec<String>, String> {
                let probe = format!("SELECT * FROM ({q}\n) AS q LIMIT 0");
                let (cols, _, _) = codecraft_db::query_all(c.as_mut(), &probe).map_err(|e| e.to_string())?;
                let _ = kind;
                Ok(keys
                    .iter()
                    .map(|k| {
                        cols.iter()
                            .find(|c| c.name.eq_ignore_ascii_case(k))
                            .map(|c| c.type_name.clone())
                            .unwrap_or_default()
                    })
                    .collect())
            };
            let lq = l.query(lkind);
            let rq = r.query(rkind);
            let lt = key_types(&mut lc, lkind, &lq).map_err(|e| format!("left: {e}"))?;
            let rt = key_types(&mut rc, rkind, &rq).map_err(|e| format!("right: {e}"))?;
            let lsql = compare::ordered_query(lkind, &lq, &keys, &lt);
            let rsql = compare::ordered_query(rkind, &rq, &keys, &rt);
            let mut ls = RowStream::spawn(move || Ok(lc), lsql, codecraft_db::PAGE_SIZE).map_err(|e| e.to_string())?;
            let mut rs = RowStream::spawn(move || Ok(rc), rsql, codecraft_db::PAGE_SIZE).map_err(|e| e.to_string())?;
            let result = compare::compare(&mut ls, &mut rs, &keys, compare::DEFAULT_MAX_KEPT, &ctx.cancel, &mut |c| {
                ctx.progress(c.left.max(c.right), None)
            })
            .map_err(|e| e.to_string())?;
            let c = result.counts;
            ctx.log(format!(
                "{} left rows, {} right rows: {} only left, {} only right, {} different, {} equal",
                c.left, c.right, c.only_left, c.only_right, c.different, c.same
            ));
            Ok(JobResult::Compare(Box::new(CompareJob { left: l, right: r, result, right_kind: rkind })))
        })
    }

    // ------------------------------------------------------------ dump / restore

    pub(crate) fn start_dump(
        &mut self,
        source: &str,
        path: PathBuf,
        opts: DumpOptions,
        tool: Option<String>,
    ) -> Result<u64, String> {
        let kind = self.source(source).map(|d| d.config.kind).ok_or_else(|| format!("no data source `{source}`"))?;
        let tool_name = match kind {
            DbKind::Postgres => Some("pg_dump"),
            DbKind::Mysql | DbKind::Mariadb => Some("mysqldump"),
            DbKind::Sqlite => None,
            DbKind::SqlServer | DbKind::ClickHouse | DbKind::Redis | DbKind::DuckDb => {
                return Err(format!("Dump is not available for {} yet", kind.label()));
            }
        };
        let program = match tool_name {
            Some(name) => Some(self.find_tool(source, name, tool.as_deref())?),
            None => None,
        };
        let spec = self.conn_spec(source, &format!("job:{}", self.next_id + 1))?;
        let title =
            format!("{} → {}", self.source(source).map(|d| d.config.name.clone()).unwrap_or_default(), path.display());
        self.spawn_job(JobKind::Dump, title, source, "bytes", move |ctx| {
            let partial = dump::partial_path(&path);
            let r = (|| -> Result<JobResult, String> {
                match program {
                    None => {
                        let mut conn = spec.connect()?;
                        let file =
                            std::fs::File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
                        let mut out = BufWriter::new(file);
                        let rows = dump::sqlite_dump(conn.as_mut(), &mut out, &opts.tables, &ctx.cancel, &mut |n| {
                            ctx.progress(n, None)
                        })
                        .map_err(|e| e.to_string())?;
                        drop(out);
                        Ok(JobResult::File { path: path.clone(), rows: Some(rows), bytes: None })
                    }
                    Some(program) => {
                        let (cfg, pw) = spec.route()?;
                        let cmd = dump::dump_command(&cfg, pw.as_deref(), program, &partial, &opts)
                            .map_err(|e| e.to_string())?;
                        ctx.log(format!("$ {}", cmd.display()));
                        dump::run_tool(&cmd, Some(&partial), &ctx.cancel, &mut |l| ctx.log(l), &mut |b| {
                            ctx.progress(b, None)
                        })
                        .map_err(|e| e.to_string())?;
                        let bytes = std::fs::metadata(&partial).map(|m| m.len()).ok();
                        Ok(JobResult::File { path: path.clone(), rows: None, bytes })
                    }
                }
            })();
            dump::finish_partial(&path, r.is_ok()).map_err(|e| e.to_string())?;
            r
        })
    }

    pub(crate) fn start_restore(&mut self, source: &str, path: PathBuf, tool: Option<String>) -> Result<u64, String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        if ds.config.read_only {
            return Err(format!("{} is read-only", ds.config.name));
        }
        let kind = ds.config.kind;
        if !path.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let program = match dump::restore_tool(kind, &path) {
            Some(name) => Some(self.find_tool(source, name, tool.as_deref())?),
            None => None,
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).ok();
        let spec = self.conn_spec(source, &format!("job:{}", self.next_id + 1))?;
        let title = format!("{} → {}", path.display(), ds.config.name);
        let unit = if program.is_some() { "bytes" } else { "statements" };
        self.spawn_job(JobKind::Restore, title, source, unit, move |ctx| match program {
            None => {
                let mut conn = spec.connect()?;
                let n = dump::run_script(conn.as_mut(), &path, &ctx.cancel, &mut |d, t| {
                    ctx.progress(d as u64, Some(t as u64))
                })
                .map_err(|e| e.to_string())?;
                Ok(JobResult::Restored { path: path.clone(), statements: Some(n) })
            }
            Some(program) => {
                let (cfg, pw) = spec.route()?;
                let cmd = dump::restore_command(&cfg, pw.as_deref(), program, &path).map_err(|e| e.to_string())?;
                ctx.log(format!("$ {}", cmd.display()));
                let feeds = cmd.stdin.is_some();
                dump::run_tool(&cmd, None, &ctx.cancel, &mut |l| ctx.log(l), &mut |b| {
                    if feeds {
                        ctx.progress(b, size)
                    }
                })
                .map_err(|e| e.to_string())?;
                Ok(JobResult::Restored { path: path.clone(), statements: None })
            }
        })
    }

    /// A tool: the `tool` param, the data source option `<name>_path`, else `PATH`.
    fn find_tool(&self, source: &str, name: &str, given: Option<&str>) -> Result<PathBuf, String> {
        let configured = given
            .map(str::to_string)
            .or_else(|| self.source(source).and_then(|d| d.config.options.get(&format!("{name}_path")).cloned()));
        dump::find_tool(name, configured.as_deref(), None).ok_or_else(|| match configured {
            Some(c) => format!("{name} not found at `{c}`"),
            None => format!(
                "{name} is not on PATH: install the client tools or set the data source option {name}_path=/path/to/{name}"
            ),
        })
    }

    // ------------------------------------------------------------ export

    pub(crate) fn start_export(
        &mut self,
        source: &str,
        side: SideSpec,
        path: PathBuf,
        format: ExportFormat,
    ) -> Result<u64, String> {
        let kind = self.source(source).map(|d| d.config.kind).ok_or_else(|| format!("no data source `{source}`"))?;
        if side.sql.trim().is_empty() && side.table.trim().is_empty() {
            return Err("export needs a table or a query".into());
        }
        let spec = self.conn_spec(source, &format!("job:{}", self.next_id + 1))?;
        let sql = side.query(kind);
        let target = InsertTarget {
            kind,
            schema: side.schema.clone(),
            table: if side.table.is_empty() { "my_table".into() } else { side.table.clone() },
        };
        let title = format!("{} → {}", side.label(), path.display());
        self.spawn_job(JobKind::Export, title, source, "rows", move |ctx| {
            let partial = dump::partial_path(&path);
            let r = (|| -> Result<JobResult, String> {
                let mut conn = spec.connect()?;
                let file = std::fs::File::create(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
                let writer = StreamWriter::new(BufWriter::new(file), format, target);
                let mut progress = |n: u64| ctx.progress(n, None);
                let mut sink =
                    ExportSink { writer: Some(writer), error: None, cancel: &ctx.cancel, progress: &mut progress };
                conn.execute(&sql, codecraft_db::PAGE_SIZE, &mut sink).map_err(|e| e.to_string())?;
                if let Some(e) = sink.error {
                    return Err(format!("{}: {e}", path.display()));
                }
                if ctx.cancelled() {
                    return Err("cancelled".into());
                }
                let w = sink.writer.take().ok_or("no writer")?;
                let rows = w.rows();
                w.finish().map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(JobResult::File { path: path.clone(), rows: Some(rows), bytes: None })
            })();
            dump::finish_partial(&path, r.is_ok()).map_err(|e| e.to_string())?;
            r
        })
    }

    // ------------------------------------------------------------ full-text search

    pub(crate) fn start_search(
        &mut self,
        source: &str,
        schema: String,
        tables: Vec<String>,
        text: String,
        case_sensitive: bool,
        limit: usize,
    ) -> Result<u64, String> {
        if text.is_empty() {
            return Err("enter the text to search for".into());
        }
        let kind = self.source(source).map(|d| d.config.kind).ok_or_else(|| format!("no data source `{source}`"))?;
        let spec = self.conn_spec(source, &format!("job:{}", self.next_id + 1))?;
        let title = format!("“{text}” in {}", if schema.is_empty() { "the default schema" } else { &schema });
        let limit = limit.clamp(1, 10_000);
        self.spawn_job(JobKind::Search, title, source, "tables", move |ctx| {
            let mut conn = spec.connect()?;
            let schema = if schema.is_empty() {
                conn.database_info().map_err(|e| e.to_string())?.default_schema.unwrap_or_default()
            } else {
                schema
            };
            let names: Vec<String> = if tables.is_empty() {
                conn.tables(&schema)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|t| matches!(t.kind, TableKind::Table | TableKind::View | TableKind::MaterializedView))
                    .map(|t| t.name)
                    .collect()
            } else {
                tables
            };
            let total = names.len() as u64;
            let watchdog = Watchdog::new(conn.canceller(), Some(SEARCH_TIMEOUT));
            let mut out = Vec::new();
            for (i, table) in names.iter().enumerate() {
                if ctx.cancelled() {
                    return Err("cancelled".into());
                }
                ctx.progress(i as u64, Some(total));
                let mut hit = SearchTable { schema: schema.clone(), table: table.clone(), ..Default::default() };
                match conn.table_details(&schema, table) {
                    Ok(d) => {
                        hit.key_columns = d.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
                        hit.searched = d
                            .columns
                            .iter()
                            .filter(|c| searchable(kind, &c.data_type))
                            .map(|c| c.name.clone())
                            .collect();
                    }
                    Err(e) => hit.error = Some(e.to_string()),
                }
                if hit.error.is_none() && !hit.searched.is_empty() {
                    match search_sql(kind, &schema, table, &hit.searched, &text, case_sensitive, limit + 1) {
                        Ok(sql) => {
                            let mut sink = CollectSink::default();
                            match execute_timed(conn.as_mut(), &watchdog, &sql, limit + 1, &mut sink) {
                                Ok(_) => {
                                    hit.truncated = sink.rows.len() > limit;
                                    sink.rows.truncate(limit);
                                    hit.columns = sink.columns;
                                    hit.rows = sink.rows;
                                }
                                Err(e) => hit.error = Some(e.to_string()),
                            }
                        }
                        Err(e) => hit.error = Some(e),
                    }
                }
                if !hit.rows.is_empty() || hit.error.is_some() {
                    if let Some(e) = &hit.error {
                        ctx.log(format!("{table}: {e}"));
                    }
                    out.push(hit);
                }
            }
            ctx.progress(total, Some(total));
            let n: usize = out.iter().map(|t| t.rows.len()).sum();
            ctx.log(format!("{n} matching rows in {} tables", out.iter().filter(|t| !t.rows.is_empty()).count()));
            Ok(JobResult::Search { text, tables: out })
        })
    }
}

/// Whether a column type holds text worth searching.
pub fn searchable(kind: DbKind, data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    if compare::is_text_type(&t) || t.contains("json") || t.contains("uuid") || t.contains("xml") {
        return true;
    }
    // SQLite: declared types are free text; untyped columns may hold anything.
    kind == DbKind::Sqlite && (t.is_empty() || t.contains("clob"))
}

/// `SELECT * FROM t WHERE c1 LIKE '%x%' ESCAPE '!' OR … LIMIT n` per dialect: PostgreSQL ILIKE
/// on the text form, MySQL LIKE (its collations ignore case; `BINARY` when case matters),
/// SQLite LIKE (ASCII case-insensitive; `instr` when case matters).
pub fn search_sql(
    kind: DbKind,
    schema: &str,
    table: &str,
    columns: &[String],
    text: &str,
    case_sensitive: bool,
    limit: usize,
) -> Result<String, String> {
    if columns.is_empty() {
        return Err("no text columns".into());
    }
    let escaped: String =
        text.chars().flat_map(|c| if matches!(c, '!' | '%' | '_') { vec!['!', c] } else { vec![c] }).collect();
    let pattern = codecraft_db::edit::literal(kind, &Cell::Text(format!("%{escaped}%"))).map_err(|e| e.to_string())?;
    let plain = codecraft_db::edit::literal(kind, &Cell::Text(text.to_string())).map_err(|e| e.to_string())?;
    let conds: Vec<String> = columns
        .iter()
        .map(|c| {
            let col = quote_ident(kind, c);
            match (kind, case_sensitive) {
                (DbKind::Postgres, false) => format!("CAST({col} AS text) ILIKE {pattern} ESCAPE '!'"),
                (DbKind::Postgres, true) => format!("CAST({col} AS text) LIKE {pattern} ESCAPE '!'"),
                (DbKind::Mysql | DbKind::Mariadb, false) => format!("{col} LIKE {pattern} ESCAPE '!'"),
                (DbKind::Mysql | DbKind::Mariadb, true) => {
                    format!("CAST({col} AS BINARY) LIKE BINARY {pattern} ESCAPE '!'")
                }
                (DbKind::Sqlite, false) => format!("{col} LIKE {pattern} ESCAPE '!'"),
                (DbKind::Sqlite, true) => format!("instr(CAST({col} AS TEXT), {plain}) > 0"),
                (DbKind::SqlServer, false) => format!("CAST({col} AS nvarchar(max)) LIKE {pattern} ESCAPE '!'"),
                (DbKind::SqlServer, true) => {
                    format!("CAST({col} AS nvarchar(max)) COLLATE Latin1_General_100_CS_AS LIKE {pattern} ESCAPE '!'")
                }
                (DbKind::DuckDb, false) => format!("CAST({col} AS VARCHAR) ILIKE {pattern} ESCAPE '!'"),
                (DbKind::DuckDb, true) => format!("CAST({col} AS VARCHAR) LIKE {pattern} ESCAPE '!'"),
                (DbKind::ClickHouse, false) => format!("positionCaseInsensitiveUTF8(toString({col}), {plain}) > 0"),
                (DbKind::ClickHouse, true) => format!("position(toString({col}), {plain}) > 0"),
                (DbKind::Redis, _) => String::new(),
            }
        })
        .collect();
    if kind == DbKind::Redis {
        return Err("Redis has no tables to search; use the key browser".into());
    }
    if kind == DbKind::SqlServer {
        return Ok(format!(
            "SELECT TOP ({limit}) * FROM {} WHERE {}",
            qualified(kind, schema, table),
            conds.join(" OR ")
        ));
    }
    Ok(format!("SELECT * FROM {} WHERE {} LIMIT {limit}", qualified(kind, schema, table), conds.join(" OR ")))
}

/// The filter that opens one search hit in the grid: its primary key, else the searched columns.
pub fn hit_filter(kind: DbKind, t: &SearchTable, row: &[Cell]) -> Option<String> {
    let value = |name: &str| t.columns.iter().position(|c| c.name == name).and_then(|i| row.get(i));
    let cols: &[String] = if t.key_columns.is_empty() { &t.searched } else { &t.key_columns };
    let mut parts = Vec::new();
    for c in cols {
        let v = value(c)?;
        if v.is_null() {
            parts.push(format!("{} IS NULL", quote_ident(kind, c)));
        } else {
            parts.push(format!("{} = {}", quote_ident(kind, c), codecraft_db::edit::literal(kind, v).ok()?));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

/// A job event.
pub(crate) fn on_event(s: &mut Session, ev: DataEvent) {
    let db = &mut s.db;
    match ev {
        DataEvent::Progress { job, done, total } => {
            if let Some(j) = db.data.jobs.iter_mut().find(|j| j.id == job) {
                j.done = done;
                if total.is_some() {
                    j.total = total;
                }
            }
        }
        DataEvent::Log { job, line } => {
            if let Some(j) = db.data.jobs.iter_mut().find(|j| j.id == job) {
                j.log.push(line);
                if j.log.len() > MAX_LOG {
                    j.log.remove(0);
                }
            }
        }
        DataEvent::Finished { job, result, cancelled } => {
            let Some(j) = db.data.jobs.iter_mut().find(|j| j.id == job) else { return };
            j.elapsed_ms = j.started.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
            let (kind, title, source) = (j.kind, j.title.clone(), j.source.clone());
            let msg = match result {
                Ok(r) => {
                    j.state = JobState::Done;
                    if let JobResult::File { bytes: Some(b), .. } = &r {
                        j.done = *b;
                    }
                    j.result = Some(r);
                    format!("{}: {title} finished", kind.label())
                }
                Err(_) if cancelled => {
                    j.state = JobState::Cancelled;
                    format!("{}: {title} cancelled", kind.label())
                }
                Err(e) => {
                    j.state = JobState::Failed(e.clone());
                    j.log.push(e.clone());
                    format!("{}: {title} failed: {e}", kind.label())
                }
            };
            let failed = matches!(j.state, JobState::Failed(_));
            db.log(if failed { OutputLevel::Error } else { OutputLevel::Info }, &source, msg.clone());
            if failed {
                s.notify(NotificationLevel::Error, kind.label(), msg);
            } else {
                s.status = msg;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_queries_per_dialect() {
        let cols = vec!["name".to_string(), "note".to_string()];
        let pg = search_sql(DbKind::Postgres, "public", "t", &cols, "50%_off!", false, 51).unwrap();
        assert_eq!(
            pg,
            "SELECT * FROM \"public\".\"t\" WHERE CAST(\"name\" AS text) ILIKE '%50!%!_off!!%' ESCAPE '!' OR \
             CAST(\"note\" AS text) ILIKE '%50!%!_off!!%' ESCAPE '!' LIMIT 51"
        );
        let my = search_sql(DbKind::Mysql, "db", "t", &cols[..1], "a'b", true, 5).unwrap();
        assert!(my.contains("CAST(`name` AS BINARY) LIKE BINARY '%a''b%' ESCAPE '!'"), "{my}");
        let lite = search_sql(DbKind::Sqlite, "main", "t", &cols[..1], "X", true, 5).unwrap();
        assert!(lite.contains("instr(CAST(\"name\" AS TEXT), 'X') > 0"), "{lite}");
        assert!(search_sql(DbKind::Sqlite, "", "t", &[], "x", false, 1).is_err());
        assert!(searchable(DbKind::Postgres, "character varying(20)"));
        assert!(searchable(DbKind::Postgres, "jsonb") && !searchable(DbKind::Postgres, "integer"));
        assert!(searchable(DbKind::Sqlite, "") && !searchable(DbKind::Mysql, ""));
        let t = SearchTable {
            columns: vec![Column { name: "id".into(), type_name: String::new() }],
            key_columns: vec!["id".into()],
            ..Default::default()
        };
        assert_eq!(hit_filter(DbKind::Sqlite, &t, &[Cell::Int(7)]).unwrap(), "\"id\" = 7");
        assert_eq!(Tool::parse("FTS"), Some(Tool::Search));
    }
}
