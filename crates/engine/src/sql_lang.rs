//! SQL inside Rust strings and SQL editing (M11 track Q).
//!
//! - **Injected SQL**: the string literals of a Rust file that hold SQL
//!   ([`codecraft_syntax::sql_inject`]) are found again whenever the file changes (only that
//!   file is re-scanned, on the UI's next frame), and their SQL tokens are coloured over the
//!   string colour ([`SqlLangState::highlights_for`]).
//! - **Data source**: injected SQL is completed and checked against one data source, chosen per
//!   project (`.codecraft/sql.json`, `sql.chooseDataSource`); by default the first connected one
//!   matching `DATABASE_URL` in the project's `.env`, else the only connected one. `.sql` files
//!   and consoles use their own data source ([`crate::database::DbState::source_for_path`]).
//! - **Completion**: keywords, tables and alias-aware columns from the schema the database code
//!   has already loaded (missing levels are requested in the background); keywords only
//!   without a connected data source.
//! - **Checks**: unresolved tables and columns become editor warnings once the text has been
//!   stable briefly; nothing is reported while the schema is unknown.
//! - **Formatting**: Reformat Code for `.sql` files, Reformat SQL for the fragment at the caret.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_db::{DataSourceConfig, DbKind};
use codecraft_syntax::sql_format::{self, SqlFormatOptions};
use codecraft_syntax::sql_inject::{self, SqlInjection};
use codecraft_syntax::sql_scope::{self, SqlColumn, SqlItemKind, SqlSchema};
use codecraft_syntax::{Highlights, Language};
use codecraft_text::{Document, EditGroup};
use codecraft_workspace::{DocId, Problem, ProblemSource};

use crate::database::{ConnStatus, DataSource, MetaReq};
use crate::popup::{ActionItem, CompletionItem, CompletionPopup};
use crate::{CommandError, Popup, Session};

/// Per-project settings, relative to the project root.
pub const SETTINGS_FILE: &str = ".codecraft/sql.json";
/// The inspection id the warnings are published under.
const INSPECTION: &str = "sql";
/// Wait this long after the last edit before checking names.
const SETTLE: Duration = Duration::from_millis(300);
/// Larger Rust files are not scanned for SQL (chars).
const MAX_SCAN_CHARS: usize = 4_000_000;
/// How often `.env` is read again.
const ENV_RECHECK: Duration = Duration::from_secs(5);

/// SQL dialect of injected SQL (identifier quoting); `None` in the settings follows the data
/// source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDialect {
    Generic,
    Sqlite,
    Postgres,
    Mysql,
}

impl SqlDialect {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "generic" | "sql" | "ansi" => Some(Self::Generic),
            "sqlite" | "sqlite3" => Some(Self::Sqlite),
            "postgres" | "postgresql" | "pg" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::Mysql),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Generic => "Generic SQL",
            Self::Sqlite => "SQLite",
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
        }
    }

    fn from_kind(kind: DbKind) -> Self {
        match kind {
            DbKind::Sqlite => Self::Sqlite,
            DbKind::Postgres | DbKind::DuckDb => Self::Postgres,
            DbKind::Mysql | DbKind::Mariadb | DbKind::ClickHouse => Self::Mysql,
            DbKind::SqlServer | DbKind::Redis => Self::Generic,
        }
    }
}

/// "SQL dialect & data source for injected SQL" and the SQL code style, per project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SqlSettings {
    /// The data source injected SQL is completed and checked against (`None`: automatic).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_source: Option<String>,
    /// `None`: the data source's dialect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialect: Option<SqlDialect>,
    pub format: SqlFormatOptions,
}

#[derive(Debug)]
struct DocSql {
    revision: u64,
    injections: Arc<Vec<SqlInjection>>,
    highlights: Arc<Highlights>,
}

#[derive(Debug, Default)]
pub struct SqlLangState {
    pub settings: SqlSettings,
    /// The project the settings belong to.
    settings_root: Option<PathBuf>,
    /// `DATABASE_URL` from the project's `.env`, and when it was read.
    env: Option<(Instant, Option<String>)>,
    docs: HashMap<DocId, DocSql>,
    /// Per document: (revision, schema fingerprint) the warnings were computed for.
    checked: HashMap<DocId, (u64, u64)>,
    /// When each document's revision was first seen (settling).
    seen: HashMap<DocId, (u64, Instant)>,
    /// Files with published warnings.
    published: HashSet<PathBuf>,
    /// A visible document is waiting to settle.
    waiting: bool,
    /// SQL files checked on a worker thread (no connected data source: the check needs only the
    /// text; M16 LT), by document.
    jobs: HashMap<DocId, SqlJob>,
}

/// A check of a `.sql` document's text on a worker thread.
#[derive(Debug)]
struct SqlJob {
    revision: u64,
    fingerprint: u64,
    /// The text the problems' offsets refer to.
    snapshot: Document,
    rx: crossbeam_channel::Receiver<Vec<crate::database::sql_nav::inspect::SqlProblem>>,
}

impl SqlLangState {
    /// Whether background work is in flight (a document waiting to be checked).
    pub fn busy(&self) -> bool {
        self.waiting
    }

    /// The document revision its SQL problems were last computed for (M16 LT: agents'
    /// `analysis.fileFindings` › `live.computedFor`).
    pub fn checked_revision(&self, doc: DocId) -> Option<u64> {
        self.checked.get(&doc).map(|(r, _)| *r)
    }

    /// The SQL fragments of a Rust document at `revision` (after [`refresh`]).
    pub fn injections_for(&self, doc: DocId, revision: u64) -> Option<&[SqlInjection]> {
        self.docs.get(&doc).filter(|d| d.revision == revision).map(|d| d.injections.as_slice())
    }

    /// SQL highlighting over the string literals of a Rust document at `revision` (after
    /// [`refresh`]); `None` when it has no SQL.
    pub fn highlights_for(&self, doc: DocId, revision: u64) -> Option<&Highlights> {
        self.docs
            .get(&doc)
            .filter(|d| d.revision == revision && !d.injections.is_empty())
            .map(|d| d.highlights.as_ref())
    }

    /// `DATABASE_URL` from the project's `.env`, as last read.
    pub fn database_url(&self) -> Option<&str> {
        self.env.as_ref().and_then(|(_, u)| u.as_deref())
    }
}

/// Bring the SQL fragments of a Rust document up to date (only when it changed).
pub fn refresh(s: &mut Session, doc: DocId) -> Option<Arc<Vec<SqlInjection>>> {
    let Some(od) = s.ws.doc(doc) else {
        s.sql_lang.docs.remove(&doc);
        return None;
    };
    if od.language != Language::Rust || od.doc.len_chars() > MAX_SCAN_CHARS {
        s.sql_lang.docs.remove(&doc);
        return None;
    }
    let revision = od.doc.revision();
    if let Some(d) = s.sql_lang.docs.get(&doc).filter(|d| d.revision == revision) {
        return Some(d.injections.clone());
    }
    let text = od.doc.text();
    let injections = Arc::new(sql_inject::find_injections(&text));
    let highlights = Arc::new(sql_inject::injected_highlights(&text, &injections));
    s.sql_lang.docs.insert(doc, DocSql { revision, injections: injections.clone(), highlights });
    Some(injections)
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    load_settings(s, false);
    refresh_env(s, false);
    s.sql_lang.waiting = false;
    let mut changed = finish_jobs(s);
    let docs = s.visible_decoration_docs();
    for doc in docs {
        refresh(s, doc);
        changed |= diagnose(s, doc, false);
    }
    if s.sql_lang.docs.len() > 32 || s.sql_lang.seen.len() > 64 {
        let open: HashSet<DocId> = s.ws.docs().map(|d| d.id).collect();
        let st = &mut s.sql_lang;
        st.docs.retain(|d, _| open.contains(d));
        st.seen.retain(|d, _| open.contains(d));
        st.checked.retain(|d, _| open.contains(d));
        st.jobs.retain(|d, _| open.contains(d));
    }
    changed
}

/// Publish the checks finished on worker threads; a result for an older revision than the
/// document's is dropped (the newer text is checked once it settles).
fn finish_jobs(s: &mut Session) -> bool {
    let mut changed = false;
    let ids: Vec<DocId> = s.sql_lang.jobs.keys().copied().collect();
    for id in ids {
        let result = match s.sql_lang.jobs.get(&id).map(|j| j.rx.try_recv()) {
            Some(Ok(found)) => Some(found),
            Some(Err(crossbeam_channel::TryRecvError::Empty)) => {
                s.sql_lang.waiting = true;
                continue;
            }
            Some(Err(crossbeam_channel::TryRecvError::Disconnected)) | None => None,
        };
        let Some(job) = s.sql_lang.jobs.remove(&id) else { continue };
        let Some(found) = result else { continue };
        let Some((rev, path)) = s.ws.doc(id).and_then(|d| d.path.clone().map(|p| (d.doc.revision(), p))) else { continue };
        if rev != job.revision {
            continue;
        }
        let d = &job.snapshot;
        let problems = found
            .into_iter()
            .map(|p| Problem {
                path: path.clone(),
                start: d.offset_to_position(p.start.min(d.len_chars())),
                end: d.offset_to_position(p.end.max(p.start).min(d.len_chars())),
                severity: p.severity,
                message: p.message,
                code: Some(p.code.to_string()),
                source: ProblemSource::Inspection,
            })
            .collect();
        s.sql_lang.checked.insert(id, (job.revision, job.fingerprint));
        changed |= publish(s, path, problems);
    }
    changed
}

// ------------------------------------------------------------------------------- settings

fn project_root(s: &Session) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.clone())
}

/// Load `.codecraft/sql.json` when the project changed (or always with `force`).
pub(crate) fn load_settings(s: &mut Session, force: bool) {
    let root = project_root(s);
    if !force && root == s.sql_lang.settings_root {
        return;
    }
    s.sql_lang.settings = root
        .as_deref()
        .map(|r| r.join(SETTINGS_FILE))
        .filter(|p| p.is_file())
        .and_then(|p| match codecraft_fs::read_text_file(&p) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| log::warn!("ignoring {}: {e}", p.display())).ok(),
            Err(e) => {
                log::warn!("cannot read {}: {e}", p.display());
                None
            }
        })
        .unwrap_or_default();
    if s.sql_lang.settings_root != root {
        s.sql_lang.env = None;
    }
    s.sql_lang.settings_root = root;
}

pub(crate) fn save_settings(s: &mut Session) -> Result<(), CommandError> {
    let root = project_root(s).ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let path = root.join(SETTINGS_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    }
    let text = serde_json::to_string_pretty(&s.sql_lang.settings).map_err(|e| CommandError::Failed(e.to_string()))?;
    codecraft_fs::write_text_file_atomic(&path, &format!("{text}\n"))
        .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
    s.sql_lang.settings_root = Some(root);
    // Different data source: check every file again.
    s.sql_lang.checked.clear();
    Ok(())
}

/// Read `DATABASE_URL` from `.env` again when it was read long enough ago (or with `force`).
pub(crate) fn refresh_env(s: &mut Session, force: bool) {
    if !force && s.sql_lang.env.as_ref().is_some_and(|(t, _)| t.elapsed() < ENV_RECHECK) {
        return;
    }
    let url = project_root(s)
        .map(|r| r.join(".env"))
        .filter(|p| p.is_file())
        .and_then(|p| codecraft_fs::read_text_file(&p).ok())
        .and_then(|text| env_value(&text, "DATABASE_URL"));
    s.sql_lang.env = Some((Instant::now(), url));
}

/// `KEY=value` from a dotenv text (`export` prefix, quotes and trailing comments allowed).
pub(crate) fn env_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        let v = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v.get(1..).and_then(|r| r.split_once(q)).map_or(v, |(inner, _)| inner),
            _ => v.split(" #").next().unwrap_or(v).trim(),
        };
        return (!v.is_empty()).then(|| v.to_string());
    }
    None
}

/// Whether a database URL (`postgres://u@host:5432/db`, `sqlite://file.db`, `mysql://…`) names
/// the data source `cfg` (relative SQLite paths resolve against `root`).
pub(crate) fn url_matches(url: &str, cfg: &DataSourceConfig, root: Option<&Path>) -> bool {
    let Some((scheme, rest)) = url.trim().split_once(':') else { return false };
    let scheme = scheme.to_ascii_lowercase();
    let kind_ok = match scheme.as_str() {
        "sqlite" | "sqlite3" => cfg.kind == DbKind::Sqlite,
        "postgres" | "postgresql" => cfg.kind == DbKind::Postgres,
        "mysql" | "mariadb" => matches!(cfg.kind, DbKind::Mysql | DbKind::Mariadb),
        _ => false,
    };
    if !kind_ok {
        return false;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    if cfg.kind == DbKind::Sqlite {
        let file = rest.strip_prefix("//").unwrap_or(rest);
        let Some(path) = &cfg.path else { return false };
        if file.is_empty() || file.starts_with(':') {
            return false;
        }
        let resolve = |p: &Path| match root {
            Some(r) if p.is_relative() => crate::normalize_path(&r.join(p)),
            _ => crate::normalize_path(p),
        };
        return resolve(Path::new(file)) == resolve(path);
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()),
        None => (host_port, None),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let local = |h: &str| matches!(h.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1" | "");
    let host_ok = host.eq_ignore_ascii_case(&cfg.host) || local(host) && local(&cfg.host);
    let port_ok = port.or(cfg.kind.default_port()) == Some(cfg.port_or_default());
    let db_ok = database.is_empty() || database == cfg.database;
    host_ok && port_ok && db_ok
}

fn connected(ds: &DataSource) -> bool {
    matches!(ds.status, ConnStatus::Connected(_))
}

/// The data source injected SQL uses: the project's choice, else the first connected one
/// matching `DATABASE_URL`, else the only connected one.
pub fn injected_source(s: &Session) -> Option<String> {
    if let Some(id) = &s.sql_lang.settings.data_source
        && s.db.source(id).is_some()
    {
        return Some(id.clone());
    }
    let root = project_root(s);
    if let Some(url) = s.sql_lang.database_url()
        && let Some(ds) = s.db.sources.iter().find(|d| connected(d) && url_matches(url, &d.config, root.as_deref()))
    {
        return Some(ds.config.id.clone());
    }
    let mut live = s.db.sources.iter().filter(|d| connected(d));
    match (live.next(), live.next()) {
        (Some(only), None) => Some(only.config.id.clone()),
        _ => None,
    }
}

/// The dialect of injected SQL: the project's choice, the data source's, the URL's scheme.
pub fn injected_dialect(s: &Session) -> SqlDialect {
    if let Some(d) = s.sql_lang.settings.dialect {
        return d;
    }
    if let Some(ds) = injected_source(s).and_then(|id| s.db.source(&id)) {
        return SqlDialect::from_kind(ds.config.kind);
    }
    s.sql_lang
        .database_url()
        .and_then(|u| u.split_once(':'))
        .and_then(|(scheme, _)| SqlDialect::parse(scheme))
        .unwrap_or(SqlDialect::Generic)
}

// ------------------------------------------------------------------------------- schema

/// The schema tree the database code keeps, as the SQL analysis sees it. Levels that are not
/// loaded yet are remembered in `wanted`.
struct DbSchema<'a> {
    ds: &'a DataSource,
    default: Option<String>,
    wanted: RefCell<Vec<MetaReq>>,
}

impl<'a> DbSchema<'a> {
    fn new(ds: &'a DataSource) -> Self {
        let default = ds.tree.info.as_ref().and_then(|i| i.default_schema.clone());
        let me = Self { ds, default, wanted: RefCell::new(Vec::new()) };
        if ds.tree.info.is_none() && !ds.tree.loading && ds.tree.error.is_none() {
            me.want(MetaReq::Info);
        }
        me
    }

    fn want(&self, req: MetaReq) {
        let mut w = self.wanted.borrow_mut();
        if !w.contains(&req) {
            w.push(req);
        }
    }

    fn schema_key(&self, schema: Option<&str>) -> Option<String> {
        match schema {
            Some(name) => self.ds.tree.schemas.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned(),
            None => self.default.clone(),
        }
    }
}

impl SqlSchema for DbSchema<'_> {
    fn tables(&self, schema: Option<&str>) -> Option<Vec<String>> {
        let key = self.schema_key(schema)?;
        match self.ds.tree.schemas.get(&key) {
            Some(node) => match &node.tables {
                Some(tables) => Some(tables.iter().map(|t| t.info.name.clone()).collect()),
                None => {
                    if !node.loading && node.error.is_none() {
                        self.want(MetaReq::Tables(key));
                    }
                    None
                }
            },
            None => {
                if schema.is_none() {
                    self.want(MetaReq::Tables(key));
                }
                None
            }
        }
    }

    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<SqlColumn>> {
        let key = self.schema_key(schema)?;
        let node = self.ds.table(&key, table)?;
        match &node.details {
            Some(d) => Some(
                d.columns.iter().map(|c| SqlColumn { name: c.name.clone(), data_type: c.data_type.clone() }).collect(),
            ),
            None => {
                if !node.loading && node.error.is_none() {
                    self.want(MetaReq::Details(key, table.to_string()));
                }
                None
            }
        }
    }

    fn schemas(&self) -> Vec<String> {
        self.ds.tree.schemas.keys().cloned().collect()
    }
}

/// Changes whenever more of the schema is known.
fn fingerprint(source: Option<&str>, ds: Option<&DataSource>) -> u64 {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    if let Some(ds) = ds {
        connected(ds).hash(&mut h);
        ds.tree.info.as_ref().map(|i| i.default_schema.clone()).hash(&mut h);
        for (name, node) in &ds.tree.schemas {
            name.hash(&mut h);
            if let Some(tables) = &node.tables {
                tables.len().hash(&mut h);
                for t in tables {
                    t.info.name.hash(&mut h);
                    t.details.as_ref().map(|d| d.columns.len()).hash(&mut h);
                }
            }
        }
    }
    h.finish()
}

/// Ask for the schema levels the analysis needed (connected data sources only).
fn request(s: &mut Session, source: &str, wanted: Vec<MetaReq>) {
    if !s.db.source(source).is_some_and(connected) {
        return;
    }
    for req in wanted.into_iter().take(32) {
        let _ = s.db.meta(source, req);
    }
}

// ------------------------------------------------------------------------------- checks

/// The data source a document's SQL is checked against.
fn source_for_doc(s: &Session, language: Language, path: Option<&Path>) -> Option<String> {
    match language {
        Language::Rust => injected_source(s),
        Language::Sql => s.db.source_for_path(path),
        _ => None,
    }
}

/// SQL problems of a document (the inspections of [`crate::database::sql_nav::inspect`]; names
/// are checked when the data source is connected), and the schema levels to load.
fn problems_of(s: &Session, doc: DocId, source: Option<&str>) -> (Vec<Problem>, Vec<MetaReq>) {
    use crate::database::sql_nav::inspect;
    let Some(od) = s.ws.doc(doc) else { return (Vec::new(), Vec::new()) };
    let Some(path) = od.path.clone() else { return (Vec::new(), Vec::new()) };
    let ds = source.and_then(|id| s.db.source(id)).filter(|d| connected(d));
    let mut schema = ds.map(DbSchema::new);
    // An attached file's names resolve in its schema.
    if let (Some(sc), Some(file_schema)) = (schema.as_mut(), s.db.nav.schema_for_path(&path))
        && sc.ds.tree.schemas.contains_key(&file_schema)
    {
        sc.default = Some(file_schema);
    }
    let schema_ref = schema.as_ref().map(|x| x as &dyn SqlSchema);
    let problem = |d: &Document, p: inspect::SqlProblem, (start, end): (usize, usize)| Problem {
        path: path.clone(),
        start: d.offset_to_position(start),
        end: d.offset_to_position(end.max(start)),
        severity: p.severity,
        message: p.message,
        code: Some(p.code.to_string()),
        source: ProblemSource::Inspection,
    };
    let mut out = Vec::new();
    match od.language {
        Language::Rust => {
            if let Some(injections) = s.sql_lang.injections_for(doc, od.doc.revision()) {
                for inj in injections {
                    for issue in inspect::check(&inj.sql, schema_ref) {
                        let range = inj.to_rust_range(issue.start, issue.end);
                        out.push(problem(&od.doc, issue, range));
                    }
                }
            }
        }
        Language::Sql if od.doc.len_chars() <= MAX_SCAN_CHARS => {
            for issue in inspect::check(&od.doc.text(), schema_ref) {
                let range = (issue.start, issue.end);
                out.push(problem(&od.doc, issue, range));
            }
        }
        _ => {}
    }
    (out, schema.map(|x| x.wanted.into_inner()).unwrap_or_default())
}

/// Check a visible document when it settled and its text or the schema changed (always with
/// `force`). Returns whether its warnings were published.
fn diagnose(s: &mut Session, doc: DocId, force: bool) -> bool {
    let Some(od) = s.ws.doc(doc) else { return false };
    let (language, revision, path) = (od.language, od.doc.revision(), od.path.clone());
    let Some(path) = path else { return false };
    if !matches!(language, Language::Rust | Language::Sql) {
        return false;
    }
    // Redis consoles hold Redis commands: no SQL inspections (track DX).
    if language == Language::Sql && s.db.is_redis_path(Some(&path)) {
        s.sql_lang.checked.insert(doc, (revision, 0));
        return publish(s, path, Vec::new());
    }
    // Nothing to check (no SQL, no connected data source) and nothing shown: done, without
    // waiting for the text to settle.
    let has_sql = language == Language::Sql || s.sql_lang.injections_for(doc, revision).is_some_and(|i| !i.is_empty());
    let source = source_for_doc(s, language, Some(&path));
    // SQL is inspected without a data source too (names only with a connected one).
    if !has_sql && !s.sql_lang.published.contains(&path) {
        s.sql_lang.checked.insert(doc, (revision, 0));
        return false;
    }
    // Analyze while typing (Settings › Editor › Inspections, M16 LT): off, or in Power Save
    // mode, an edited SQL file is checked when it opens and after a save only.
    if !force
        && language == Language::Sql
        && !crate::analysis::live::enabled(s, crate::analysis::live::LiveLang::Sql)
        && s.sql_lang.checked.contains_key(&doc)
        && s.ws.doc(doc).is_some_and(|d| d.doc.is_modified())
    {
        return false;
    }
    if !force {
        let now = Instant::now();
        let first = match s.sql_lang.seen.get(&doc) {
            Some((r, t)) if *r == revision => *t,
            _ => {
                s.sql_lang.seen.insert(doc, (revision, now));
                now
            }
        };
        // SQL files and consoles wait the analysis-while-typing delay; SQL in Rust strings the
        // short settle it always had.
        let settle = if language == Language::Sql { crate::analysis::live::delay(s) } else { SETTLE };
        if now.duration_since(first) < settle {
            if s.sql_lang.checked.get(&doc).is_none_or(|(r, _)| *r != revision) {
                s.sql_lang.waiting = true;
            }
            return false;
        }
    }
    let fp = fingerprint(source.as_deref(), source.as_deref().and_then(|id| s.db.source(id)));
    if !force && s.sql_lang.checked.get(&doc) == Some(&(revision, fp)) {
        return false;
    }
    // A `.sql` file without a connected data source needs only its text: checked on a worker
    // thread, never on the UI thread (M16 LT; a 10k-line file took ~60 ms here).
    let schema_less = source.as_deref().and_then(|id| s.db.source(id)).is_none_or(|d| !connected(d));
    if !force && language == Language::Sql && schema_less {
        if s.sql_lang.jobs.get(&doc).is_some_and(|j| j.revision == revision && j.fingerprint == fp) {
            s.sql_lang.waiting = true;
            return false;
        }
        if let Some(snapshot) = s.ws.doc(doc).filter(|d| d.doc.len_chars() <= MAX_SCAN_CHARS).map(|d| d.doc.snapshot()) {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let text = snapshot.clone();
            let spawned = std::thread::Builder::new().name("codecraft-sql-check".into()).spawn(move || {
                let _ = tx.send(crate::database::sql_nav::inspect::check(&text.text(), None));
            });
            if spawned.is_ok() {
                s.sql_lang.jobs.insert(doc, SqlJob { revision, fingerprint: fp, snapshot, rx });
                s.sql_lang.waiting = true;
                return false;
            }
        }
    }
    let (problems, wanted) = problems_of(s, doc, source.as_deref());
    s.sql_lang.checked.insert(doc, (revision, fp));
    if let Some(src) = &source {
        request(s, src, wanted);
    }
    publish(s, path, problems)
}

fn publish(s: &mut Session, path: PathBuf, problems: Vec<Problem>) -> bool {
    if problems.is_empty() {
        if !s.sql_lang.published.remove(&path) {
            return false;
        }
    } else {
        s.sql_lang.published.insert(path.clone());
    }
    s.ws.set_inspection_problems(INSPECTION, path, problems);
    true
}

/// Check the active document now (no settling) and return its warnings.
pub(crate) fn check_active(s: &mut Session) -> Result<Vec<Problem>, CommandError> {
    let (_, did) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    refresh(s, did);
    diagnose(s, did, true);
    let path = s.ws.doc(did).and_then(|d| d.path.clone());
    Ok(path
        .map(|p| s.ws.problems_for(&p).into_iter().filter(|p| p.source == ProblemSource::Inspection).cloned().collect())
        .unwrap_or_default())
}

// ------------------------------------------------------------------------------- completion

/// The SQL fragment around the caret of the active editor.
pub(crate) fn injection_at_caret(s: &Session) -> Option<SqlInjection> {
    let (e, d) = s.ws.active()?;
    if d.language != Language::Rust || e.selections.len() != 1 {
        return None;
    }
    let caret = e.selections.primary().head;
    if let Some(found) = s.sql_lang.injections_for(d.id, d.doc.revision()) {
        return sql_inject::injection_at(found, caret).cloned();
    }
    if d.doc.len_chars() > MAX_SCAN_CHARS {
        return None;
    }
    let found = sql_inject::find_injections(&d.doc.text());
    sql_inject::injection_at(&found, caret).cloned()
}

/// Whether the caret of the active editor is in injected SQL.
pub(crate) fn in_injection(s: &Session) -> bool {
    injection_at_caret(s).is_some()
}

/// `check`, or enabled when the caret is in injected SQL (Basic completion works there
/// without a language server).
pub(crate) fn or_in_injection(s: &Session, check: fn(&Session) -> Result<(), String>) -> Result<(), String> {
    if in_injection(s) { Ok(()) } else { check(s) }
}

fn needs_quotes(dialect: SqlDialect, name: &str) -> bool {
    let first_ok = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
    let rest_ok = name.chars().all(|c| c.is_alphanumeric() || c == '_');
    !first_ok || !rest_ok || dialect == SqlDialect::Postgres && name.chars().any(char::is_uppercase)
}

fn ident_text(dialect: SqlDialect, name: &str) -> String {
    if !needs_quotes(dialect, name) {
        return name.to_string();
    }
    match dialect {
        SqlDialect::Mysql => codecraft_db::sql::quote_ident(DbKind::Mysql, name),
        _ => codecraft_db::sql::quote_ident(DbKind::Postgres, name),
    }
}

/// Completion in the SQL at the caret: keywords, tables and (alias-aware) columns. With
/// `auto`, stays quiet when there is nothing to offer.
pub(crate) fn complete(s: &mut Session, auto: bool) -> Result<Value, CommandError> {
    let Some(inj) = injection_at_caret(s) else {
        return Err(CommandError::Disabled("the caret is not in SQL".into()));
    };
    let Some((e, _)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    let (editor, caret) = (e.id, e.selections.primary().head);
    let sql_caret = inj.to_sql(caret).unwrap_or(0);
    let source = injected_source(s);
    let dialect = injected_dialect(s);
    let (completions, wanted) = {
        let ds = source.as_deref().and_then(|id| s.db.source(id)).filter(|d| connected(d));
        let schema = ds.map(DbSchema::new);
        let c = sql_scope::complete(&inj.sql, sql_caret, schema.as_ref().map(|x| x as &dyn SqlSchema));
        (c, schema.map(|x| x.wanted.into_inner()).unwrap_or_default())
    };
    if let Some(src) = &source {
        request(s, src, wanted);
    }
    let anchor = inj.to_rust(completions.start);
    let items: Vec<CompletionItem> = completions
        .items
        .iter()
        .filter_map(|i| {
            let (kind, text) = match i.kind {
                SqlItemKind::Column => ("field", ident_text(dialect, &i.label)),
                SqlItemKind::Table => ("struct", ident_text(dialect, &i.label)),
                SqlItemKind::Schema => ("mod", ident_text(dialect, &i.label)),
                SqlItemKind::Keyword => ("keyword", i.label.clone()),
            };
            let insert = sql_inject::encode_for_literal(&text, inj.raw, inj.hashes)?;
            Some(CompletionItem {
                label: i.label.clone(),
                detail: i.detail.clone(),
                kind: kind.to_string(),
                filter_text: i.label.clone(),
                sort_text: format!("{}{}", i.rank, i.label.to_lowercase()),
                insert_text: insert,
                replace_start: Some(anchor),
                resolved: true,
                ..CompletionItem::default()
            })
        })
        .collect();
    if items.is_empty() {
        if !auto {
            s.status = "No suggestions".into();
        }
        return Ok(json!({ "items": 0 }));
    }
    let n = items.len();
    s.popup = Some(Popup::Completion(CompletionPopup { editor, anchor, items, auto, ..CompletionPopup::default() }));
    s.refresh_completion_filter();
    Ok(json!({ "items": n, "dataSource": source }))
}

/// After a typed char: in injected SQL, pop completion up at the start of a word or after
/// `alias.`. Returns true when the caret is in SQL (the language server is not asked).
pub(crate) fn after_typed(s: &mut Session, typed: char) -> bool {
    let Some((_, did)) = s.active_ids() else { return false };
    if s.ws.doc(did).is_none_or(|d| d.language != Language::Rust) {
        return false;
    }
    // Scan the edited file once (the editor reuses it for highlighting).
    refresh(s, did);
    if !in_injection(s) {
        return false;
    }
    if !s.ws.settings.completion_auto_popup || s.power_save() {
        return true;
    }
    let Some((e, d)) = s.ws.active() else { return true };
    let caret = e.selections.primary().head;
    let prev = caret.checked_sub(2).and_then(|i| d.doc.char_at(i));
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let word_start = (typed.is_alphabetic() || typed == '_') && !prev.is_some_and(ident);
    let member = typed == '.' && prev.is_some_and(|c| ident(c) || c == '"' || c == '`');
    if word_start || member {
        let _ = complete(s, true);
    }
    true
}

// ------------------------------------------------------------------------------- formatting

/// The formatter options of the project.
pub fn format_options(s: &Session) -> SqlFormatOptions {
    let mut o = s.sql_lang.settings.format;
    o.indent = o.indent.clamp(1, 16);
    o
}

/// Reformat Code in an `.sql` file or console.
pub(crate) fn reformat_document(s: &mut Session) -> Result<bool, CommandError> {
    let opts = format_options(s);
    let Some((_, d)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    if d.language != Language::Sql {
        return Err(CommandError::Disabled("not an SQL file".into()));
    }
    let old = d.doc.text();
    let new = sql_format::format_sql(&old, &opts);
    apply_text(s, &old, &new)
}

/// The new content of a literal holding `sql` once formatted as `formatted`: the literal's
/// leading and trailing whitespace stay; lines after the first are indented like the first
/// line (when the SQL starts on a line of its own) or aligned with the content start column
/// `column`.
pub(crate) fn layout_injected(sql: &str, formatted: &str, column: usize) -> String {
    let body = formatted.trim();
    if body.is_empty() {
        return sql.to_string();
    }
    let lead: String = sql.chars().take_while(|c| c.is_whitespace()).collect();
    let rest: String = sql.chars().skip(lead.chars().count()).collect();
    let trail_len = rest.chars().rev().take_while(|c| c.is_whitespace()).count();
    let trail: String = rest.chars().skip(rest.chars().count().saturating_sub(trail_len)).collect();
    let indent: String = match lead.rfind('\n') {
        Some(i) => lead.get(i + 1..).unwrap_or("").to_string(),
        None => " ".repeat(column.saturating_add(lead.chars().count()).min(200)),
    };
    let mut out = lead;
    for (k, line) in body.lines().enumerate() {
        if k > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(&indent);
            }
        }
        out.push_str(line);
    }
    out.push_str(&trail);
    out
}

/// Reformat SQL: format the fragment at the caret (or at `offset`) in place.
pub(crate) fn reformat_injected(s: &mut Session, offset: Option<usize>) -> Result<bool, CommandError> {
    let opts = format_options(s);
    let (_, did) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let injections = refresh(s, did).ok_or_else(|| CommandError::Disabled("not a Rust file".into()))?;
    let Some((e, d)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    let at = offset.unwrap_or(e.selections.primary().head);
    let inj = sql_inject::injection_at(&injections, at)
        .cloned()
        .ok_or_else(|| CommandError::Disabled("the caret is not in SQL".into()))?;
    let line = d.doc.char_to_line(inj.start);
    let column = inj.start.saturating_sub(d.doc.line_start(line));
    let formatted = sql_format::format_sql(&inj.sql, &opts);
    let content = layout_injected(&inj.sql, &formatted, column);
    let encoded = sql_inject::encode_for_literal(&content, inj.raw, inj.hashes)
        .ok_or_else(|| CommandError::Failed("the formatted SQL cannot be written into this raw string".into()))?;
    let len = d.doc.len_chars();
    let old = d.doc.text();
    let new = format!("{}{}{}", d.doc.slice(0..inj.start.min(len)), encoded, d.doc.slice(inj.end.min(len)..len));
    apply_text(s, &old, &new)
}

/// Replace the active document's text `old` with `new` as a minimal undoable edit; returns
/// whether anything changed.
fn apply_text(s: &mut Session, old: &str, new: &str) -> Result<bool, CommandError> {
    if old == new {
        s.status = "SQL is already formatted".into();
        return Ok(false);
    }
    s.edit_active(EditGroup::Standalone, |doc, sel, _| crate::reformat::diff_edit(doc, new, sel))?;
    s.status = "Reformatted SQL".into();
    Ok(true)
}

// ------------------------------------------------------------------------------- actions

/// Context actions (Alt+Enter) in injected SQL.
pub(crate) fn context_actions(s: &Session) -> Vec<ActionItem> {
    if !in_injection(s) {
        return Vec::new();
    }
    let action = |title: &str, command: &str| ActionItem {
        title: title.to_string(),
        kind: Some("sql".into()),
        payload: json!({ "title": title, "codecraftCommand": command }),
    };
    // Quick-fixes of the SQL problems at the caret first (track DC).
    let mut items = crate::database::sql_nav::context_actions(s);
    items.push(action("Reformat SQL", "sql.reformatInjected"));
    items.push(action("Choose SQL Data Source…", "sql.chooseDataSource"));
    items
}

/// The data source chooser for injected SQL.
pub(crate) fn chooser(s: &mut Session) {
    let editor = s.ws.active().map(|(e, _)| e.id).unwrap_or(codecraft_workspace::EditorId(0));
    let current = s.sql_lang.settings.data_source.clone();
    let mark = |on: bool| if on { "✓ " } else { "" };
    let mut items = vec![ActionItem {
        title: format!("{}Automatic (DATABASE_URL or the only connected data source)", mark(current.is_none())),
        kind: Some("sql".into()),
        payload: json!({ "codecraftCommand": "sql.chooseDataSource", "codecraftParams": { "dataSource": null } }),
    }];
    for ds in &s.db.sources {
        let on = current.as_deref() == Some(ds.config.id.as_str());
        items.push(ActionItem {
            title: format!("{}{} ({})", mark(on), ds.config.name, ds.config.kind.label()),
            kind: Some("sql".into()),
            payload: json!({ "codecraftCommand": "sql.chooseDataSource", "codecraftParams": { "dataSource": ds.config.id } }),
        });
    }
    s.popup = Some(Popup::Actions {
        editor,
        title: "SQL Data Source for Injected SQL".into(),
        items,
        selected: 0,
        pending: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_values() {
        let text = "# db\nexport DATABASE_URL=\"postgres://u:p@localhost/app\" # main\nOTHER=1\n";
        assert_eq!(env_value(text, "DATABASE_URL").as_deref(), Some("postgres://u:p@localhost/app"));
        assert_eq!(env_value("DATABASE_URL=sqlite://x.db # c", "DATABASE_URL").as_deref(), Some("sqlite://x.db"));
        assert_eq!(env_value("DATABASE_URL='a b'", "DATABASE_URL").as_deref(), Some("a b"));
        assert_eq!(env_value("#DATABASE_URL=x\nDATABASE_URL=", "DATABASE_URL"), None);
        assert_eq!(env_value("", "DATABASE_URL"), None);
    }

    #[test]
    fn urls_match_data_sources() {
        let pg = DataSourceConfig {
            kind: DbKind::Postgres,
            host: "localhost".into(),
            database: "app".into(),
            ..Default::default()
        };
        assert!(url_matches("postgres://u:secret@127.0.0.1:5432/app?sslmode=disable", &pg, None));
        assert!(url_matches("postgresql://localhost/app", &pg, None));
        assert!(!url_matches("postgres://localhost:5433/app", &pg, None));
        assert!(!url_matches("postgres://localhost/other", &pg, None));
        assert!(!url_matches("mysql://localhost/app", &pg, None));
        let my =
            DataSourceConfig { kind: DbKind::Mariadb, host: "db".into(), database: "x".into(), ..Default::default() };
        assert!(url_matches("mysql://root@db:3306/x", &my, None));
        let root = Path::new("/p");
        let lite = DataSourceConfig { kind: DbKind::Sqlite, path: Some("data/app.db".into()), ..Default::default() };
        assert!(url_matches("sqlite://data/app.db", &lite, Some(root)));
        assert!(url_matches("sqlite:/p/data/app.db?mode=rwc", &lite, Some(root)));
        assert!(url_matches("sqlite:///p/data/./app.db", &lite, Some(root)));
        assert!(!url_matches("sqlite::memory:", &lite, Some(root)));
        assert!(!url_matches("sqlite://other.db", &lite, Some(root)));
        for junk in ["", ":", "postgres", "postgres://", "postgres://[::1", "sqlite:", "http://x"] {
            let _ = url_matches(junk, &pg, Some(root));
            let _ = url_matches(junk, &lite, Some(root));
        }
    }

    #[test]
    fn injected_layout_keeps_the_literal_shape() {
        // SQL on lines of its own: indentation of the first line, trailing whitespace kept.
        let sql = "\n        select a from t where x\n    ";
        let f = sql_format::format_sql(sql, &SqlFormatOptions::default());
        assert_eq!(layout_injected(sql, &f, 12), "\n        SELECT a\n        FROM t\n        WHERE x\n    ");
        // Inline SQL: continuation lines align with the content start.
        let sql = "select a from t";
        let f = sql_format::format_sql(sql, &SqlFormatOptions::default());
        assert_eq!(layout_injected(sql, &f, 10), "SELECT a\n          FROM t");
        assert_eq!(layout_injected("  ", "", 3), "  ");
    }

    #[test]
    fn identifiers_are_quoted_per_dialect() {
        assert_eq!(ident_text(SqlDialect::Postgres, "users"), "users");
        assert_eq!(ident_text(SqlDialect::Postgres, "Users"), "\"Users\"");
        assert_eq!(ident_text(SqlDialect::Sqlite, "Users"), "Users");
        assert_eq!(ident_text(SqlDialect::Mysql, "a b"), "`a b`");
        assert_eq!(ident_text(SqlDialect::Generic, "a b"), "\"a b\"");
        assert_eq!(SqlDialect::parse("PostgreSQL"), Some(SqlDialect::Postgres));
        assert_eq!(SqlDialect::parse("oracle"), None);
    }
}
