//! The SQL editor's view of the database (track DC): Go to Declaration from SQL to the table,
//! column or routine (selected in the Database tool window, DDL in a read-only tab), Quick
//! Documentation, Find Usages across the project's SQL, SQL inspections with quick-fixes,
//! in-statement rename of aliases and CTEs, Go to Database Object, the explorer's filter and
//! schemas to introspect, pinned result tabs and the schema of attached files.
//!
//! What the schema tree does not load (routines, comments, view and routine definitions) is
//! asked from the data source's introspection connection with catalog queries
//! ([`ddl::routines_query`], [`ddl::info_query`]); answers arrive through [`on_query`].
//! Navigation that needs metadata which is not loaded yet waits in [`NavState::pending`] and
//! finishes in [`pump`] once it arrives.

pub mod ddl;
pub mod inspect;
pub mod resolve;
pub mod usages;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_db::{Cell, DbKind, TableKind};
use codecraft_fs::FileMatch;
use codecraft_syntax::Language;
use codecraft_syntax::sql_inject::{self, SqlInjection};
use codecraft_syntax::sql_scope::{SqlColumn, SqlSchema};
use codecraft_text::{Change, ChangeSet, EditGroup, SelectionSet};
use codecraft_workspace::{EditorId, ToolWindowId};

use super::script::ScriptMode;
use super::{ConnStatus, DataSource, DbState, MetaReq, node_key};
use crate::popup::ActionItem;
use crate::{CommandError, Popup, Session};
use ddl::{InfoKind, ObjectInfo, Routine};
use inspect::{QuickFix, SqlEdit};
use resolve::SqlTarget;
use usages::UsageTarget;

/// Per-project settings of the SQL editor, relative to the project root.
pub const SETTINGS_FILE: &str = ".codecraft/sql-editor.json";
/// SQL texts larger than this are not analyzed (chars).
const MAX_SQL_CHARS: usize = 4_000_000;
/// Navigation waits this long for metadata.
const PENDING_TIMEOUT: Duration = Duration::from_secs(15);
/// Objects offered by Go to Database Object at most.
pub const GOTO_LIMIT: usize = 200;

/// Settings saved in [`SETTINGS_FILE`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EditorSettings {
    /// Schemas to introspect per data source id (missing: all).
    pub introspect: BTreeMap<String, Vec<String>>,
    /// The schema of attached SQL files and consoles, by path relative to the project.
    pub file_schemas: BTreeMap<String, String>,
    /// What Run 'file.sql' does when a statement fails.
    pub script_mode: ScriptMode,
}

/// Kinds of objects Go to Database Object finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DbObjectKind {
    Table,
    View,
    Routine,
    Schema,
}

impl DbObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Routine => "routine",
            Self::Schema => "schema",
        }
    }
}

/// A schema object of a connected data source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbObject {
    pub source: String,
    pub source_name: String,
    /// Empty for schemas.
    pub schema: String,
    pub name: String,
    pub kind: DbObjectKind,
}

impl DbObject {
    /// `schema.name` (the schema's name for schemas).
    pub fn qualified(&self) -> String {
        if self.schema.is_empty() { self.name.clone() } else { format!("{}.{}", self.schema, self.name) }
    }
}

/// Navigate › Database Object… (state; the UI edits `query` in place).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GotoObjectPopup {
    pub query: String,
    pub selected: usize,
}

/// Routines of one schema.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RoutineList {
    pub loading: bool,
    pub error: Option<String>,
    pub items: Option<Vec<Routine>>,
}

/// The "Schemas to introspect" chooser of a data source (the UI edits `chosen` in place).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SchemasDialog {
    pub source: String,
    pub all: Vec<String>,
    pub chosen: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavAction {
    Declaration,
    QuickDoc,
}

/// Navigation waiting for metadata.
#[derive(Clone, Debug)]
pub(crate) struct PendingNav {
    action: NavAction,
    source: String,
    target: SqlTarget,
    default_schema: Option<String>,
    editor: EditorId,
    offset: usize,
    since: Instant,
}

/// Everything track DC keeps in [`DbState`].
#[derive(Default)]
pub struct NavState {
    pub settings: EditorSettings,
    root: Option<PathBuf>,
    /// The Database tool window's filter (matches table, view, routine and schema names).
    pub explorer_filter: String,
    /// Result tabs that a new run does not replace.
    pub pinned: BTreeSet<u64>,
    pub goto: Option<GotoObjectPopup>,
    /// Routines per (data source, schema).
    pub routines: HashMap<(String, String), RoutineList>,
    /// Comments and definitions per (data source, schema, name).
    pub info: HashMap<(String, String, String), ObjectInfo>,
    pub schemas_dialog: Option<SchemasDialog>,
    pub(crate) pending: Option<PendingNav>,
    /// The schema each console connection was switched to.
    worker_schema: HashMap<String, String>,
    /// DDL tabs: file → (data source, schema).
    ddl_files: HashMap<PathBuf, (String, String)>,
    /// A Find Usages search in flight: results and the title.
    usages: Option<(Receiver<Vec<FileMatch>>, String)>,
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl NavState {
    /// Load the project's settings (on project change).
    pub(crate) fn load(&mut self, root: Option<&Path>) {
        *self = Self { ddl_files: std::mem::take(&mut self.ddl_files), ..Self::default() };
        self.root = root.map(Path::to_path_buf);
        let Some(path) = root.map(|r| r.join(SETTINGS_FILE)).filter(|p| p.is_file()) else { return };
        match codecraft_fs::read_text_file(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<EditorSettings>(&t).map_err(|e| e.to_string()))
        {
            Ok(s) => self.settings = s,
            Err(e) => log::warn!("ignoring {}: {e}", path.display()),
        }
    }

    pub(crate) fn save(&self) -> Result<(), String> {
        let Some(root) = &self.root else { return Err("open a project first".into()) };
        let path = root.join(SETTINGS_FILE);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(&self.settings).map_err(|e| e.to_string())?;
        codecraft_fs::write_text_file_atomic(&path, &format!("{json}\n")).map_err(|e| e.to_string())
    }

    fn rel(&self, path: &Path) -> String {
        match &self.root {
            Some(r) => path.strip_prefix(r).unwrap_or(path).display().to_string(),
            None => path.display().to_string(),
        }
    }

    /// The schema chosen for an attached file or console.
    pub fn schema_for_path(&self, path: &Path) -> Option<String> {
        self.settings.file_schemas.get(&self.rel(path)).cloned()
    }

    pub(crate) fn set_schema_for_path(&mut self, path: &Path, schema: Option<String>) {
        let rel = self.rel(path);
        match schema {
            Some(s) => self.settings.file_schemas.insert(rel, s),
            None => self.settings.file_schemas.remove(&rel),
        };
    }

    /// The data source of a DDL tab.
    pub fn ddl_source(&self, path: &Path) -> Option<&str> {
        self.ddl_files.get(path).map(|(s, _)| s.as_str())
    }

    /// A connection was (re)opened: it starts in the default schema.
    pub(crate) fn forget_worker(&mut self, worker: &str) {
        self.worker_schema.remove(worker);
    }

    /// Cached metadata of a data source (all with `None`) is stale.
    pub(crate) fn forget_source(&mut self, source: Option<&str>) {
        let keep = |s: &String| source.is_some_and(|x| x != s);
        self.routines.retain(|(s, _), _| keep(s));
        self.info.retain(|(s, _, _), _| keep(s));
        // The source's console connections closed too (others switch again: harmless).
        self.worker_schema.clear();
    }

    /// The statement switching `worker` (a console or attached file) to its chosen schema,
    /// once per connection.
    pub(crate) fn schema_switch(&mut self, worker: &str, kind: DbKind) -> Option<String> {
        let path = worker.strip_prefix("console:")?;
        let schema = self.schema_for_path(Path::new(path))?;
        if self.worker_schema.get(worker) == Some(&schema) {
            return None;
        }
        let sql = match kind {
            DbKind::Postgres => format!("SET search_path TO {}", codecraft_db::sql::quote_ident(kind, &schema)),
            DbKind::Mysql | DbKind::Mariadb | DbKind::ClickHouse => {
                format!("USE {}", codecraft_db::sql::quote_ident(kind, &schema))
            }
            // Redis "schemas" are the numbered databases.
            DbKind::Redis => format!("SELECT {}", schema.strip_prefix("db")?.parse::<u32>().ok()?),
            // T-SQL has no session schema; DuckDB runs each statement in its own process.
            DbKind::Sqlite | DbKind::SqlServer | DbKind::DuckDb => return None,
        };
        self.worker_schema.insert(worker.to_string(), schema);
        Some(sql)
    }

    /// The schemas of `all` to list and load for a data source.
    pub(crate) fn introspected(&self, source: &str, all: &[String]) -> Vec<String> {
        match self.settings.introspect.get(source).filter(|l| !l.is_empty()) {
            Some(list) => all.iter().filter(|s| list.iter().any(|w| eq(w, s))).cloned().collect(),
            None => all.to_vec(),
        }
    }

    /// Whether a name passes the explorer filter (case-insensitive substring, or fuzzy).
    pub fn filter_matches(&self, name: &str) -> bool {
        let f = self.explorer_filter.trim();
        f.is_empty() || name.to_lowercase().contains(&f.to_lowercase()) || codecraft_fs::fuzzy_match(f, name).is_some()
    }

    /// Routines of a schema, when loaded.
    pub fn routines_of(&self, source: &str, schema: &str) -> Option<&RoutineList> {
        self.routines.get(&(source.to_string(), schema.to_string()))
    }
}

/// The title of a query's result tab: the first table it reads.
pub fn result_title(sql: &str) -> Option<String> {
    if sql.len() > 200_000 {
        return None;
    }
    let st = codecraft_syntax::sql_scope::analyze(sql).into_iter().next()?;
    st.tables
        .iter()
        .find(|t| !(t.schema.is_none() && st.ctes.iter().any(|c| eq(c, &t.name))))
        .map(|t| t.name.clone())
        .filter(|n| !n.is_empty())
}

// ---------------------------------------------------------------- catalog queries

fn kind_of(db: &DbState, source: &str) -> DbKind {
    db.source(source).map(|d| d.config.kind).unwrap_or_default()
}

fn connected(ds: &DataSource) -> bool {
    matches!(ds.status, ConnStatus::Connected(_))
}

const ROUTINES: &str = "routines";
const INFO: &str = "info";

fn query_key(parts: &[&str]) -> String {
    parts.join("\u{1f}")
}

/// Load the routines of a schema (once; again with `force`).
pub(crate) fn request_routines(db: &mut DbState, source: &str, schema: &str, force: bool) {
    let key = (source.to_string(), schema.to_string());
    if !force && db.nav.routines.get(&key).is_some_and(|r| r.loading || r.items.is_some()) {
        return;
    }
    match ddl::routines_query(kind_of(db, source), schema) {
        None => {
            db.nav.routines.insert(key, RoutineList { items: Some(Vec::new()), ..RoutineList::default() });
        }
        Some(sql) => {
            let entry = db.nav.routines.entry(key).or_default();
            entry.loading = true;
            entry.error = None;
            if let Err(e) = db.meta(source, MetaReq::Query(query_key(&[ROUTINES, schema]), sql))
                && let Some(r) = db.nav.routines.get_mut(&(source.to_string(), schema.to_string()))
            {
                r.loading = false;
                r.error = Some(e);
            }
        }
    }
}

/// Load the comments and definition of an object (once; again with `force`).
pub(crate) fn request_info(db: &mut DbState, source: &str, schema: &str, name: &str, what: InfoKind, force: bool) {
    let key = (source.to_string(), schema.to_string(), name.to_string());
    if !force && db.nav.info.contains_key(&key) {
        return;
    }
    match ddl::info_query(kind_of(db, source), schema, name, what) {
        None => {
            db.nav.info.insert(key, ObjectInfo::default());
        }
        Some(sql) => {
            db.nav.info.insert(key.clone(), ObjectInfo { loading: true, ..ObjectInfo::default() });
            if let Err(e) = db.meta(source, MetaReq::Query(query_key(&[INFO, schema, name]), sql)) {
                db.nav.info.insert(key, ObjectInfo { error: Some(e), ..ObjectInfo::default() });
            }
        }
    }
}

/// A catalog query answered.
pub(crate) fn on_query(db: &mut DbState, source: &str, key: &str, rows: Result<Vec<Vec<Cell>>, String>) {
    let parts: Vec<&str> = key.split('\u{1f}').collect();
    match parts.as_slice() {
        [ROUTINES, schema] => {
            let entry = db.nav.routines.entry((source.to_string(), schema.to_string())).or_default();
            entry.loading = false;
            match rows {
                Ok(rows) => {
                    entry.items = Some(ddl::parse_routines(&rows));
                    entry.error = None;
                }
                Err(e) => {
                    entry.items = Some(Vec::new());
                    entry.error = Some(e);
                }
            }
        }
        [INFO, schema, name] => {
            let info = match rows {
                Ok(rows) => ddl::parse_info(&rows),
                // Old servers lack some catalog functions: the DDL is generated instead.
                Err(e) => ObjectInfo { error: Some(e), ..ObjectInfo::default() },
            };
            db.nav.info.insert((source.to_string(), schema.to_string(), name.to_string()), info);
        }
        _ => {}
    }
}

/// Load what Go to Database Object searches: the tables and routines of every introspected
/// schema of the connected data sources.
pub(crate) fn load_all(db: &mut DbState) {
    let mut reqs: Vec<(String, String, bool)> = Vec::new();
    for ds in db.sources.iter().filter(|d| connected(d)) {
        for (schema, node) in ds.tree.schemas.iter().take(64) {
            reqs.push((
                ds.config.id.clone(),
                schema.clone(),
                node.tables.is_none() && !node.loading && node.error.is_none(),
            ));
        }
    }
    for (source, schema, tables) in reqs {
        if tables {
            let _ = db.meta(&source, MetaReq::Tables(schema.clone()));
        }
        request_routines(db, &source, &schema, false);
    }
}

// ---------------------------------------------------------------- Go to Database Object

/// Every object of the connected data sources that is loaded.
pub fn objects(db: &DbState) -> Vec<DbObject> {
    let mut out = Vec::new();
    for ds in db.sources.iter().filter(|d| connected(d)) {
        let source = ds.config.id.clone();
        let source_name = ds.config.name.clone();
        for (schema, node) in &ds.tree.schemas {
            out.push(DbObject {
                source: source.clone(),
                source_name: source_name.clone(),
                schema: String::new(),
                name: schema.clone(),
                kind: DbObjectKind::Schema,
            });
            for t in node.tables.iter().flatten() {
                let kind = if t.info.kind == TableKind::Table || t.info.kind == TableKind::Foreign {
                    DbObjectKind::Table
                } else {
                    DbObjectKind::View
                };
                out.push(DbObject {
                    source: source.clone(),
                    source_name: source_name.clone(),
                    schema: schema.clone(),
                    name: t.info.name.clone(),
                    kind,
                });
            }
            if let Some(items) = db.nav.routines_of(&source, schema).and_then(|r| r.items.as_ref()) {
                for r in items {
                    out.push(DbObject {
                        source: source.clone(),
                        source_name: source_name.clone(),
                        schema: schema.clone(),
                        name: r.name.clone(),
                        kind: DbObjectKind::Routine,
                    });
                }
            }
        }
    }
    out
}

/// The objects matching `query` (fuzzy over the name, or `schema.name` when the query has a
/// dot), best first: (index into `objects`, matched char indices of the name).
pub fn rank_objects(objects: &[DbObject], query: &str, limit: usize) -> Vec<(usize, Vec<usize>)> {
    let q = query.trim();
    if q.is_empty() {
        let mut idx: Vec<usize> = (0..objects.len()).collect();
        idx.sort_by(|&a, &b| {
            let (x, y) = (&objects[a], &objects[b]);
            (x.kind, x.name.to_lowercase()).cmp(&(y.kind, y.name.to_lowercase()))
        });
        return idx.into_iter().take(limit).map(|i| (i, Vec::new())).collect();
    }
    let dotted = q.contains('.');
    let keys: Vec<String> = objects.iter().map(|o| if dotted { o.qualified() } else { o.name.clone() }).collect();
    let mut ranked = codecraft_fs::rank(q, keys.iter().map(String::as_str), objects.len());
    // Equal scores: exact names, then tables before views, routines and schemas.
    ranked.sort_by(|(a, ma), (b, mb)| {
        let (x, y) = (objects.get(*a), objects.get(*b));
        let exact = |o: Option<&DbObject>| !o.is_some_and(|o| eq(&o.name, q));
        mb.score
            .cmp(&ma.score)
            .then_with(|| exact(x).cmp(&exact(y)))
            .then_with(|| x.map(|o| o.kind).cmp(&y.map(|o| o.kind)))
            .then_with(|| a.cmp(b))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(i, m)| {
            // Highlight indices within the name.
            let shift = if dotted {
                objects.get(i).map_or(0, |o| o.qualified().chars().count() - o.name.chars().count())
            } else {
                0
            };
            (i, m.indices.into_iter().filter_map(|k| k.checked_sub(shift)).collect())
        })
        .collect()
}

// ---------------------------------------------------------------- schema access

/// The loaded schema tree of a data source as the SQL analysis sees it (no loading).
pub(crate) struct NavSchema<'a> {
    ds: &'a DataSource,
    default: Option<String>,
}

impl<'a> NavSchema<'a> {
    pub(crate) fn new(ds: &'a DataSource, default: Option<String>) -> Self {
        let default = default
            .and_then(|d| ds.tree.schemas.keys().find(|k| eq(k, &d)).cloned())
            .or_else(|| ds.tree.info.as_ref().and_then(|i| i.default_schema.clone()));
        Self { ds, default }
    }

    fn key(&self, schema: Option<&str>) -> Option<String> {
        match schema {
            Some(s) => self.ds.tree.schemas.keys().find(|k| eq(k, s)).cloned(),
            None => self.default.clone(),
        }
    }
}

impl SqlSchema for NavSchema<'_> {
    fn tables(&self, schema: Option<&str>) -> Option<Vec<String>> {
        let key = self.key(schema)?;
        Some(self.ds.tree.schemas.get(&key)?.tables.as_ref()?.iter().map(|t| t.info.name.clone()).collect())
    }

    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<SqlColumn>> {
        let key = self.key(schema)?;
        let d = self.ds.table(&key, table)?.details.as_ref()?;
        Some(d.columns.iter().map(|c| SqlColumn { name: c.name.clone(), data_type: c.data_type.clone() }).collect())
    }

    fn schemas(&self) -> Vec<String> {
        self.ds.tree.schemas.keys().cloned().collect()
    }
}

/// What a name resolved to in a data source.
enum Found {
    Table {
        schema: String,
        name: String,
        kind: TableKind,
    },
    Routine {
        schema: String,
        routine: Routine,
    },
    Missing(String),
    /// Metadata is loading (requests were sent).
    Wait,
}

/// Find a table or view: in `schema`, else the default schema, else any loaded schema.
fn find_table(db: &mut DbState, source: &str, default: Option<&str>, schema: Option<&str>, name: &str) -> Found {
    let Some(ds) = db.source(source) else { return Found::Missing(format!("no data source `{source}`")) };
    match ds.status {
        ConnStatus::Failed(ref e) => return Found::Missing(format!("cannot connect: {e}")),
        ConnStatus::Disconnected => {
            let _ = db.meta(source, MetaReq::Info);
            return Found::Wait;
        }
        ConnStatus::Connecting => return Found::Wait,
        ConnStatus::Connected(_) => {}
    }
    let Some(info) = &ds.tree.info else {
        if !ds.tree.loading {
            let _ = db.meta(source, MetaReq::Info);
        }
        return Found::Wait;
    };
    let default = default
        .and_then(|d| ds.tree.schemas.keys().find(|k| eq(k, d)).cloned())
        .or_else(|| info.default_schema.clone())
        .or_else(|| ds.tree.schemas.keys().next().cloned());
    let order: Vec<String> = match schema {
        Some(s) => match ds.tree.schemas.keys().find(|k| eq(k, s)) {
            Some(k) => vec![k.clone()],
            None => return Found::Missing(format!("no schema `{s}`")),
        },
        None => default
            .iter()
            .cloned()
            .chain(ds.tree.schemas.keys().filter(|k| Some(*k) != default.as_ref()).cloned())
            .collect(),
    };
    let mut wait = false;
    let mut load = Vec::new();
    for (k, sc) in order.iter().enumerate() {
        let Some(node) = ds.tree.schemas.get(sc) else { continue };
        match &node.tables {
            Some(tables) => {
                if let Some(t) = tables.iter().find(|t| eq(&t.info.name, name)) {
                    return Found::Table { schema: sc.clone(), name: t.info.name.clone(), kind: t.info.kind };
                }
            }
            // Only the asked or default schema is loaded on demand.
            None if k == 0 => {
                wait = true;
                if !node.loading {
                    load.push(sc.clone());
                }
            }
            None => {}
        }
    }
    for sc in load {
        let _ = db.meta(source, MetaReq::Tables(sc));
    }
    if wait { Found::Wait } else { Found::Missing(format!("table `{name}` not found")) }
}

fn find_routine(db: &mut DbState, source: &str, default: Option<&str>, schema: Option<&str>, name: &str) -> Found {
    let Some(ds) = db.source(source) else { return Found::Missing(format!("no data source `{source}`")) };
    if !connected(ds) {
        return find_table(db, source, default, schema, name);
    }
    let default = default.map(str::to_string).or_else(|| ds.tree.info.as_ref().and_then(|i| i.default_schema.clone()));
    let schemas: Vec<String> = match schema {
        Some(s) => ds.tree.schemas.keys().filter(|k| eq(k, s)).cloned().collect(),
        None => default
            .iter()
            .cloned()
            .chain(ds.tree.schemas.keys().filter(|k| Some(*k) != default.as_ref()).cloned())
            .collect(),
    };
    let mut wait = false;
    for sc in schemas.iter().take(32) {
        match db.nav.routines_of(source, sc) {
            Some(RoutineList { items: Some(items), .. }) => {
                if let Some(r) = items.iter().find(|r| eq(&r.name, name)) {
                    return Found::Routine { schema: sc.clone(), routine: r.clone() };
                }
            }
            Some(RoutineList { loading: true, .. }) => wait = true,
            _ => {
                request_routines(db, source, sc, false);
                wait = true;
            }
        }
    }
    if wait { Found::Wait } else { Found::Missing(format!("routine `{name}` not found")) }
}

// ---------------------------------------------------------------- SQL at the caret

/// The SQL around a caret: an SQL document or the SQL string literal of a Rust file.
#[derive(Clone, Debug)]
pub(crate) struct SqlCtx {
    pub sql: String,
    /// Char offset in `sql`.
    pub offset: usize,
    pub inj: Option<SqlInjection>,
    pub source: Option<String>,
    pub schema: Option<String>,
    pub editor: EditorId,
    pub path: Option<PathBuf>,
}

impl SqlCtx {
    /// A document offset for an SQL offset.
    pub fn to_doc(&self, at: usize) -> usize {
        match &self.inj {
            Some(inj) => inj.to_rust(at),
            None => at,
        }
    }
}

/// The SQL at `offset` (default: the caret) of the active editor.
pub(crate) fn context(s: &Session, offset: Option<usize>) -> Option<SqlCtx> {
    let (e, d) = s.ws.active()?;
    let at = offset.unwrap_or(e.selections.primary().head).min(d.doc.len_chars());
    match d.language {
        Language::Sql if d.doc.len_chars() <= MAX_SQL_CHARS => Some(SqlCtx {
            sql: d.doc.text(),
            offset: at,
            inj: None,
            source: s.db.source_for_path(d.path.as_deref()),
            schema: d.path.as_deref().and_then(|p| s.db.nav.schema_for_path(p)),
            editor: e.id,
            path: d.path.clone(),
        }),
        Language::Rust => {
            let inj = match offset {
                None => crate::sql_lang::injection_at_caret(s)?,
                Some(_) if d.doc.len_chars() <= MAX_SQL_CHARS => {
                    sql_inject::injection_at(&sql_inject::find_injections(&d.doc.text()), at).cloned()?
                }
                Some(_) => return None,
            };
            let sql_at = inj.to_sql(at).unwrap_or(0);
            Some(SqlCtx {
                sql: inj.sql.clone(),
                offset: sql_at,
                source: crate::sql_lang::injected_source(s),
                schema: None,
                editor: e.id,
                path: d.path.clone(),
                inj: Some(inj),
            })
        }
        _ => None,
    }
}

/// Whether the active editor is an SQL file or its caret is in SQL inside a Rust string.
pub(crate) fn in_sql(s: &Session) -> bool {
    match s.ws.active() {
        Some((_, d)) if d.language == Language::Sql => true,
        Some((_, d)) if d.language == Language::Rust => crate::sql_lang::in_injection(s),
        _ => false,
    }
}

fn no_source() -> CommandError {
    CommandError::Failed(
        "choose a data source for this SQL (the editor toolbar, db.attachFile or sql.chooseDataSource)".into(),
    )
}

// ---------------------------------------------------------------- editing

/// Apply edits in SQL coordinates to the editor of `ctx`; `caret` is an offset in the edited
/// SQL.
pub(crate) fn apply_edits(
    s: &mut Session,
    ctx: &SqlCtx,
    edits: &[SqlEdit],
    caret: Option<usize>,
) -> Result<(), CommandError> {
    if s.active_ids().map(|(e, _)| e) != Some(ctx.editor) {
        return Err(CommandError::Failed("the editor changed".into()));
    }
    let mut changes = Vec::new();
    for e in edits {
        let (start, end, text) = match &ctx.inj {
            Some(inj) => {
                let (a, b) = inj.to_rust_range(e.start, e.end);
                let text = sql_inject::encode_for_literal(&e.text, inj.raw, inj.hashes)
                    .ok_or_else(|| CommandError::Failed("the change cannot be written into this raw string".into()))?;
                (a, b, text)
            }
            None => (e.start, e.end, e.text.clone()),
        };
        changes.push(Change::new(start, end, text));
    }
    changes.sort_by_key(|c| (c.start, c.end));
    let set = ChangeSet::new(changes).map_err(|e| CommandError::Failed(format!("cannot apply the change: {e}")))?;
    let new_sql = inspect::apply(&ctx.sql, edits);
    s.edit_active(EditGroup::Standalone, |doc, sel, _| {
        if set.check_bounds(doc.len_chars()).is_err() {
            return codecraft_text::Edit::none(sel.clone());
        }
        codecraft_text::Edit { changes: set.clone(), selections: sel.map(&set) }
    })?;
    if let (Some(c), Some(new_sql)) = (caret, new_sql) {
        let at = match &ctx.inj {
            Some(inj) => {
                let prefix: String = new_sql.chars().take(c).collect();
                inj.start
                    + sql_inject::encode_for_literal(&prefix, inj.raw, inj.hashes).map_or(0, |p| p.chars().count())
            }
            None => c,
        };
        let len = s.ws.active().map(|(_, d)| d.doc.len_chars()).unwrap_or(0);
        s.ws.set_selections(ctx.editor, SelectionSet::caret(at.min(len)));
    }
    Ok(())
}

// ---------------------------------------------------------------- quick-fixes

/// A quick-fix offered at a caret.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixItem {
    pub title: String,
    pub code: String,
    /// Text edits (SQL coordinates), or a command to run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<QuickFix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<(String, Value)>,
}

/// The problems of the SQL at the caret and their fixes.
pub(crate) fn fixes_at(s: &Session, ctx: &SqlCtx) -> Vec<FixItem> {
    let ds = ctx.source.as_deref().and_then(|id| s.db.source(id)).filter(|d| connected(d));
    let schema = ds.map(|d| NavSchema::new(d, ctx.schema.clone()));
    let schema_ref = schema.as_ref().map(|x| x as &dyn SqlSchema);
    let mut out = Vec::new();
    for p in inspect::check(&ctx.sql, schema_ref).into_iter().filter(|p| p.start <= ctx.offset && ctx.offset <= p.end) {
        if p.code == inspect::RESOLVE
            && let Some(src) = &ctx.source
        {
            out.push(FixItem {
                title: "Refresh database schema".into(),
                code: p.code.to_string(),
                fix: None,
                command: Some(("db.refresh".into(), json!({ "dataSource": src }))),
            });
        }
        for f in inspect::fixes(&ctx.sql, &p, schema_ref) {
            out.push(FixItem { title: f.title.clone(), code: p.code.to_string(), fix: Some(f), command: None });
        }
    }
    out
}

/// Context actions (Alt+Enter) for SQL: the quick-fixes at the caret.
pub(crate) fn context_actions(s: &Session) -> Vec<ActionItem> {
    let Some(ctx) = context(s, None) else { return Vec::new() };
    let mut items: Vec<ActionItem> = fixes_at(s, &ctx)
        .into_iter()
        .enumerate()
        .map(|(i, f)| ActionItem {
            title: f.title.clone(),
            kind: Some("quickfix".into()),
            payload: json!({
                "title": f.title,
                "codecraftCommand": "sql.applyFix",
                "codecraftParams": { "index": i, "offset": ctx.to_doc(ctx.offset) },
            }),
        })
        .collect();
    if resolve::rename_plan(&ctx.sql, ctx.offset).is_some() {
        items.push(ActionItem {
            title: "Rename…".into(),
            kind: Some("refactor".into()),
            payload: json!({ "title": "Rename…", "codecraftCommand": "refactor.rename" }),
        });
    }
    items
}

// ---------------------------------------------------------------- navigation

/// Go to Declaration / Quick Documentation of the name at the caret.
pub(crate) fn navigate(s: &mut Session, offset: Option<usize>, action: NavAction) -> Result<Value, CommandError> {
    let ctx = context(s, offset).ok_or_else(|| CommandError::Disabled("the caret is not in SQL".into()))?;
    let target = resolve::target_at(&ctx.sql, ctx.offset)
        .ok_or_else(|| CommandError::Failed("no database object at the caret".into()))?;
    // Names defined in the statement itself.
    if let SqlTarget::Cte { name, def } = &target {
        let doc_at = ctx.to_doc(def.0);
        return match action {
            NavAction::Declaration => {
                jump_in_editor(s, &ctx, doc_at)?;
                Ok(json!({ "target": target, "offset": doc_at }))
            }
            NavAction::QuickDoc => {
                let text = cte_doc(&ctx.sql, name, *def);
                show_doc(s, ctx.editor, ctx.to_doc(ctx.offset), text.clone());
                Ok(json!({ "target": target, "text": text }))
            }
        };
    }
    if let (SqlTarget::Alias { def, .. }, NavAction::Declaration) = (&target, action)
        && !(def.0 <= ctx.offset && ctx.offset <= def.1)
    {
        // From a use of an alias to its definition.
        let doc_at = ctx.to_doc(def.0);
        jump_in_editor(s, &ctx, doc_at)?;
        return Ok(json!({ "target": target, "offset": doc_at }));
    }
    let source = ctx.source.clone().ok_or_else(no_source)?;
    let pending = PendingNav {
        action,
        source,
        target: target.clone(),
        default_schema: ctx.schema.clone(),
        editor: ctx.editor,
        offset: ctx.to_doc(ctx.offset),
        since: Instant::now(),
    };
    match attempt(s, &pending)? {
        Some(v) => Ok(v),
        None => {
            s.db.nav.pending = Some(pending);
            s.status = "Loading database metadata…".into();
            Ok(json!({ "target": target, "pending": true }))
        }
    }
}

/// Go to (or document) a named object of a data source, waiting for its metadata if needed.
pub(crate) fn navigate_object(
    s: &mut Session,
    source: &str,
    target: SqlTarget,
    action: NavAction,
) -> Result<Value, CommandError> {
    let (editor, offset) = s.ws.active().map(|(e, _)| (e.id, e.selections.primary().head)).unwrap_or((EditorId(0), 0));
    let pending = PendingNav {
        action,
        source: source.to_string(),
        target: target.clone(),
        default_schema: None,
        editor,
        offset,
        since: Instant::now(),
    };
    match attempt(s, &pending)? {
        Some(v) => Ok(v),
        None => {
            s.db.nav.pending = Some(pending);
            s.status = "Loading database metadata…".into();
            Ok(json!({ "target": target, "pending": true }))
        }
    }
}

/// Close one connection (a console's), so the next run reconnects in the default schema.
pub(crate) fn close_connection(db: &mut DbState, worker: &str) -> Result<(), String> {
    if db.transaction_open(worker) {
        return Err("commit or roll back the open transaction first".into());
    }
    if db.runs.values().any(|r| r.worker == worker) {
        return Err("a query is running on this connection".into());
    }
    if let Some(w) = db.workers.remove(worker) {
        let _ = w.tx.send(super::Req::Close);
    }
    for r in db.results.iter_mut().filter(|r| r.worker == worker && r.has_more) {
        r.has_more = false;
        r.truncated = true;
    }
    db.nav.forget_worker(worker);
    Ok(())
}

fn jump_in_editor(s: &mut Session, ctx: &SqlCtx, doc_at: usize) -> Result<(), CommandError> {
    match &ctx.path {
        Some(p) => s.navigate_to(p, doc_at),
        None => {
            s.ws.set_selections(ctx.editor, SelectionSet::caret(doc_at));
            Ok(())
        }
    }
}

fn show_doc(s: &mut Session, editor: EditorId, offset: usize, text: String) {
    if s.active_ids().map(|(e, _)| e) == Some(editor) {
        s.popup = Some(Popup::Hover { editor, offset, text });
    }
}

fn cte_doc(sql: &str, name: &str, def: (usize, usize)) -> String {
    let rest: String = sql.chars().skip(def.0).take(4000).collect();
    // Up to the closing parenthesis of the CTE's body.
    let mut depth = 0i32;
    let mut end = rest.chars().count();
    let mut opened = false;
    for (k, c) in rest.chars().enumerate() {
        match c {
            '(' => {
                depth += 1;
                opened = true;
            }
            ')' => {
                depth -= 1;
                if opened && depth == 0 {
                    end = k + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let body: String = rest.chars().take(end).collect();
    format!("**{name}** · common table expression\n\n```sql\n{body}\n```")
}

/// Try to finish a navigation: `Some` when done, `None` while metadata loads.
fn attempt(s: &mut Session, p: &PendingNav) -> Result<Option<Value>, CommandError> {
    let src = p.source.clone();
    let default = p.default_schema.clone();
    let fail = |m: String| CommandError::Failed(m);
    match &p.target {
        SqlTarget::Table { schema, name } | SqlTarget::Alias { schema, table: name, .. } => {
            match find_table(&mut s.db, &src, default.as_deref(), schema.as_deref(), name) {
                Found::Table { schema, name, kind } => finish_table(s, p, &schema, &name, kind, None),
                Found::Wait => Ok(None),
                Found::Missing(m) => Err(fail(m)),
                _ => Err(fail(format!("`{name}` is not a table"))),
            }
        }
        SqlTarget::Column { name: column, tables } => {
            if tables.is_empty() {
                return Err(fail(format!("column `{column}` is not from a table")));
            }
            let mut found = Vec::new();
            for (schema, table) in tables {
                match find_table(&mut s.db, &src, default.as_deref(), schema.as_deref(), table) {
                    Found::Table { schema, name, kind } => found.push((schema, name, kind)),
                    Found::Wait => return Ok(None),
                    _ => {}
                }
            }
            // The table that has the column (loading the candidates' columns first).
            let mut waiting = false;
            let mut owner = None;
            for (schema, name, kind) in &found {
                match s.db.source(&src).and_then(|d| d.table(schema, name)).map(|t| (t.details.clone(), t.loading)) {
                    Some((Some(d), _)) => {
                        if owner.is_none() && d.columns.iter().any(|c| eq(&c.name, column)) {
                            owner = Some((schema.clone(), name.clone(), *kind));
                        }
                    }
                    Some((None, loading)) => {
                        waiting = true;
                        if !loading {
                            let _ = s.db.meta(&src, MetaReq::Details(schema.clone(), name.clone()));
                        }
                    }
                    None => {}
                }
            }
            match owner {
                Some((schema, name, kind)) => finish_table(s, p, &schema, &name, kind, Some(column)),
                None if waiting => Ok(None),
                None => Err(fail(format!("column `{column}` not found"))),
            }
        }
        SqlTarget::Routine { schema, name } => {
            match find_routine(&mut s.db, &src, default.as_deref(), schema.as_deref(), name) {
                Found::Routine { schema, routine } => finish_routine(s, p, &schema, &routine),
                Found::Wait => Ok(None),
                Found::Missing(m) => Err(fail(m)),
                _ => Err(fail(format!("`{name}` is not a routine"))),
            }
        }
        SqlTarget::Schema { name } => {
            let Some(ds) = s.db.source(&src) else { return Err(fail(format!("no data source `{src}`"))) };
            let Some(real) = ds.tree.schemas.keys().find(|k| eq(k, name)).cloned() else {
                if ds.tree.info.is_none() {
                    let _ = s.db.meta(&src, MetaReq::Info);
                    return Ok(None);
                }
                return Err(fail(format!("no schema `{name}`")));
            };
            let n = ds.tree.schemas.get(&real).and_then(|n| n.tables.as_ref()).map(Vec::len);
            match p.action {
                NavAction::Declaration => {
                    select_in_tree(s, &src, Some(&real), None);
                    Ok(Some(json!({ "dataSource": src, "schema": real })))
                }
                NavAction::QuickDoc => {
                    let text = format!(
                        "**{real}** · schema · {}{}",
                        ds.config.name,
                        n.map(|n| format!(" · {n} tables and views")).unwrap_or_default()
                    );
                    show_doc(s, p.editor, p.offset, text.clone());
                    Ok(Some(json!({ "dataSource": src, "schema": real, "text": text })))
                }
            }
        }
        SqlTarget::Cte { .. } => Ok(Some(Value::Null)),
    }
}

fn info_kind(kind: TableKind) -> InfoKind {
    match kind {
        TableKind::View | TableKind::MaterializedView => InfoKind::View,
        _ => InfoKind::Table,
    }
}

/// The table is known: wait for its columns and catalog info, then act.
fn finish_table(
    s: &mut Session,
    p: &PendingNav,
    schema: &str,
    name: &str,
    kind: TableKind,
    column: Option<&str>,
) -> Result<Option<Value>, CommandError> {
    let src = p.source.clone();
    if !ensure_table_meta(&mut s.db, &src, schema, name, kind) && p.since.elapsed() < PENDING_TIMEOUT {
        return Ok(None);
    }
    match p.action {
        NavAction::Declaration => {
            select_in_tree(s, &src, Some(schema), Some((name, column.is_some())));
            let path = open_ddl(s, &src, schema, name, column)?;
            Ok(Some(json!({ "dataSource": src, "schema": schema, "name": name, "column": column, "ddl": path })))
        }
        NavAction::QuickDoc => {
            let text = object_doc(s, &src, schema, name, kind, column);
            show_doc(s, p.editor, p.offset, text.clone());
            Ok(Some(json!({ "dataSource": src, "schema": schema, "name": name, "column": column, "text": text })))
        }
    }
}

/// Request what a table's DDL needs; true when everything is there.
fn ensure_table_meta(db: &mut DbState, source: &str, schema: &str, name: &str, kind: TableKind) -> bool {
    let details = db
        .source(source)
        .and_then(|d| d.table(schema, name))
        .map(|t| (t.details.is_some(), t.loading, t.error.is_some()));
    let mut ready = true;
    match details {
        Some((false, false, false)) => {
            let _ = db.meta(source, MetaReq::Details(schema.to_string(), name.to_string()));
            ready = false;
        }
        Some((false, true, _)) => ready = false,
        _ => {}
    }
    request_info(db, source, schema, name, info_kind(kind), false);
    ready && !db.nav.info.get(&(source.to_string(), schema.to_string(), name.to_string())).is_some_and(|i| i.loading)
}

fn finish_routine(s: &mut Session, p: &PendingNav, schema: &str, r: &Routine) -> Result<Option<Value>, CommandError> {
    let src = p.source.clone();
    request_info(&mut s.db, &src, schema, &r.name, InfoKind::Routine, false);
    let key = (src.clone(), schema.to_string(), r.name.clone());
    if s.db.nav.info.get(&key).is_some_and(|i| i.loading) && p.since.elapsed() < PENDING_TIMEOUT {
        return Ok(None);
    }
    let kind = kind_of(&s.db, &src);
    match p.action {
        NavAction::Declaration => {
            select_in_tree(s, &src, Some(schema), None);
            let text = ddl::routine_ddl(kind, schema, &r.name, s.db.nav.info.get(&key));
            let path = write_ddl(s, &src, schema, &r.name, &text)?;
            open_read_only(s, &path, None)?;
            Ok(Some(json!({ "dataSource": src, "schema": schema, "routine": r.name, "ddl": path })))
        }
        NavAction::QuickDoc => {
            let text = ddl::routine_doc(kind, schema, r, s.db.nav.info.get(&key));
            show_doc(s, p.editor, p.offset, text.clone());
            Ok(Some(json!({ "dataSource": src, "schema": schema, "routine": r.name, "text": text })))
        }
    }
}

/// Quick Documentation text of a table, view or column.
pub(crate) fn object_doc(
    s: &Session,
    source: &str,
    schema: &str,
    name: &str,
    kind: TableKind,
    column: Option<&str>,
) -> String {
    let Some(ds) = s.db.source(source) else { return String::new() };
    let node = ds.table(schema, name);
    let details = node.and_then(|t| t.details.as_ref());
    let info = s.db.nav.info.get(&(source.to_string(), schema.to_string(), name.to_string()));
    if let Some(col) = column
        && let Some(c) = details.and_then(|d| d.columns.iter().find(|c| eq(&c.name, col)))
    {
        return ddl::column_doc(name, schema, c, info.and_then(|i| i.column_comments.get(&c.name)));
    }
    ddl::table_doc(&ds.config.name, ds.config.kind, schema, name, kind, details, info, node.and_then(|t| t.row_count))
}

/// Select a node in the Database tool window (expanding its parents) and show the window.
pub(crate) fn select_in_tree(s: &mut Session, source: &str, schema: Option<&str>, table: Option<(&str, bool)>) {
    let db = &mut s.db;
    db.expanded.insert(node_key(source, None, None));
    let mut key = node_key(source, None, None);
    if let Some(sc) = schema {
        key = node_key(source, Some(sc), None);
        db.expanded.insert(key.clone());
        if let Some((t, expand)) = table {
            key = node_key(source, Some(sc), Some(t));
            if expand {
                db.expanded.insert(key.clone());
            }
        }
    }
    // A filter that hides the node would hide the selection.
    if !db.nav.explorer_filter.is_empty() && table.is_some_and(|(t, _)| !db.nav.filter_matches(t)) {
        db.nav.explorer_filter.clear();
    }
    db.selected = Some(key);
    s.ws.layout.show(ToolWindowId::Database);
}

// ---------------------------------------------------------------- DDL tabs

fn ddl_dir() -> PathBuf {
    std::env::temp_dir().join("codecraft-ddl").join(std::process::id().to_string())
}

fn slug(s: &str) -> String {
    let mut out: String =
        s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).take(80).collect();
    if out.is_empty() || out.chars().all(|c| c == '.') {
        out = "_".into();
    }
    out
}

fn write_ddl(s: &mut Session, source: &str, schema: &str, name: &str, text: &str) -> Result<PathBuf, CommandError> {
    let dir = ddl_dir().join(slug(source)).join(slug(schema));
    std::fs::create_dir_all(&dir).map_err(|e| CommandError::Failed(format!("{}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.sql", slug(name)));
    codecraft_fs::write_text_file_atomic(&path, text).map_err(|e| CommandError::Failed(e.to_string()))?;
    s.db.nav.ddl_files.insert(path.clone(), (source.to_string(), schema.to_string()));
    Ok(path)
}

/// Open the DDL of a table or view in a read-only tab (the caret on `column`'s line).
pub(crate) fn open_ddl(
    s: &mut Session,
    source: &str,
    schema: &str,
    name: &str,
    column: Option<&str>,
) -> Result<PathBuf, CommandError> {
    let ds = s.db.source(source).ok_or_else(|| CommandError::BadParams(format!("no data source `{source}`")))?;
    let node = ds.table(schema, name);
    let kind = node.map(|t| t.info.kind).unwrap_or(TableKind::Table);
    let info = s.db.nav.info.get(&(source.to_string(), schema.to_string(), name.to_string()));
    let text = ddl::table_ddl(ds.config.kind, schema, name, kind, node.and_then(|t| t.details.as_ref()), info);
    let line = column.and_then(|c| ddl::column_line_of(&text, c));
    let path = write_ddl(s, source, schema, name, &text)?;
    open_read_only(s, &path, line)?;
    s.status = format!("DDL of {schema}.{name}");
    Ok(path)
}

/// Open (or refresh) a generated file in a read-only tab.
fn open_read_only(s: &mut Session, path: &Path, line: Option<usize>) -> Result<(), CommandError> {
    if let Some(id) = s.ws.doc_by_path(path)
        && let Ok(text) = codecraft_fs::read_text_file(path)
    {
        let editor = s.ws.editor_for_doc(id);
        let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        if let Some(od) = s.ws.doc_mut(id)
            && od.doc.text() != text
            && od.doc.replace_all(&text, &sels).is_ok()
        {
            od.doc.mark_saved();
        }
    }
    let editor = s.open_file(path)?;
    let doc = s.ws.editor(editor).map(|e| e.doc);
    if let Some(od) = doc.and_then(|d| s.ws.doc_mut(d)) {
        od.read_only = true;
    }
    let offset = doc
        .and_then(|d| s.ws.doc(d))
        .map(|d| line.map_or(0, |l| d.doc.line_start(l.min(d.doc.len_lines().saturating_sub(1)))))
        .unwrap_or(0);
    s.ws.set_selections(editor, SelectionSet::caret(offset));
    if let Some(e) = s.ws.editor_mut(editor) {
        e.reveal_caret = true;
    }
    Ok(())
}

// ---------------------------------------------------------------- Find Usages

/// What Find Usages looks for.
pub(crate) enum UsageQuery {
    /// A table or column, across the project.
    Project(UsageTarget),
    /// A CTE: its uses in the statement (name and SQL ranges).
    Statement(String, Vec<(usize, usize)>),
}

/// The table or column at the caret, for Find Usages.
pub(crate) fn usage_target(s: &Session, ctx: &SqlCtx) -> Option<UsageQuery> {
    let target = resolve::target_at(&ctx.sql, ctx.offset)?;
    Some(match target {
        SqlTarget::Table { schema, name } | SqlTarget::Alias { schema, table: name, .. } => {
            UsageQuery::Project(UsageTarget { schema, table: name, column: None })
        }
        SqlTarget::Column { name, tables } => {
            // Prefer the table whose loaded columns include it.
            let ds = ctx.source.as_deref().and_then(|id| s.db.source(id));
            let owner = tables
                .iter()
                .find(|(sc, t)| {
                    ds.is_some_and(|d| {
                        let schema = NavSchema::new(d, ctx.schema.clone());
                        inspect::table_columns(&schema, sc.as_deref(), t)
                            .is_some_and(|c| c.iter().any(|c| eq(&c.name, &name)))
                    })
                })
                .or(tables.first())
                .cloned()?;
            UsageQuery::Project(UsageTarget { schema: owner.0, table: owner.1, column: Some(name) })
        }
        SqlTarget::Cte { name, .. } => {
            let plan = resolve::rename_plan(&ctx.sql, ctx.offset)?;
            UsageQuery::Statement(name, plan.spans)
        }
        _ => return None,
    })
}

/// Start Find Usages of `target` over the project's SQL and Rust files.
pub(crate) fn start_usages(s: &mut Session, target: UsageTarget) -> Result<Value, CommandError> {
    let project =
        s.ws.project.as_ref().ok_or_else(|| CommandError::Disabled("open a project to find usages".into()))?;
    let mut files: Vec<PathBuf> =
        project.tree.files().map(|n| n.path.clone()).filter(|p| usages::searched(p)).take(usages::MAX_FILES).collect();
    let mut open = HashMap::new();
    for d in s.ws.docs() {
        if let Some(p) = d.path.as_ref().filter(|p| usages::searched(p)) {
            open.insert(p.clone(), d.doc.text());
            if !files.contains(p) && p.starts_with(&project.root) {
                files.push(p.clone());
            }
        }
    }
    let n = files.len();
    let title = target.title();
    let rx = usages::spawn(files, open, target.clone()).map_err(CommandError::Failed)?;
    s.db.nav.usages = Some((rx, title.clone()));
    s.status = format!("{title}: searching {n} files…");
    Ok(json!({ "searching": n, "title": title, "target": target }))
}

/// Find Usages results (CTE usages are listed from the statement directly).
pub(crate) fn show_spans(s: &mut Session, ctx: &SqlCtx, title: String, spans: &[(usize, usize)]) {
    let Some((_, d)) = s.ws.active() else { return };
    let Some(path) = d.path.clone() else { return };
    let matches: Vec<FileMatch> = spans
        .iter()
        .map(|&(a, b)| {
            let (a, b) = (ctx.to_doc(a), ctx.to_doc(b));
            let start = d.doc.offset_to_position(a);
            let end = d.doc.offset_to_position(b);
            let line_text: String = d.doc.line_text(start.line).unwrap_or_default().chars().take(500).collect();
            FileMatch {
                path: path.clone(),
                line: start.line,
                column: start.column,
                end_column: end.column.max(start.column),
                line_text,
            }
        })
        .collect();
    crate::usages::show_find_results(s, title, matches);
}

/// Whether a Find Usages search is running.
pub fn usages_running(db: &DbState) -> bool {
    db.nav.usages.is_some()
}

// ---------------------------------------------------------------- pump

/// Finish pending navigation and searches. Returns whether anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    if let Some((rx, title)) = s.db.nav.usages.take() {
        match rx.try_recv() {
            Ok(matches) => {
                crate::usages::show_find_results(s, title, matches);
                changed = true;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => s.db.nav.usages = Some((rx, title)),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                s.status = "Find Usages stopped".into();
                changed = true;
            }
        }
    }
    if let Some(p) = s.db.nav.pending.take() {
        match attempt(s, &p) {
            Ok(Some(_)) => changed = true,
            Ok(None) if p.since.elapsed() < PENDING_TIMEOUT => s.db.nav.pending = Some(p),
            Ok(None) => {
                s.status = "Database metadata did not arrive in time".into();
                changed = true;
            }
            Err(e) => {
                s.status = e.to_string();
                changed = true;
            }
        }
    }
    changed
}

/// Whether navigation is waiting for metadata (the UI keeps repainting).
pub fn busy(db: &DbState) -> bool {
    db.nav.pending.is_some() || db.nav.usages.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(schema: &str, name: &str, kind: DbObjectKind) -> DbObject {
        DbObject { source: "s".into(), source_name: "S".into(), schema: schema.into(), name: name.into(), kind }
    }

    #[test]
    fn object_ranking() {
        let objects = vec![
            obj("public", "user_roles", DbObjectKind::Table),
            obj("public", "users", DbObjectKind::Table),
            obj("audit", "users", DbObjectKind::View),
            obj("", "public", DbObjectKind::Schema),
            obj("public", "update_user", DbObjectKind::Routine),
        ];
        let names = |q: &str| -> Vec<String> {
            rank_objects(&objects, q, 10).into_iter().map(|(i, _)| objects[i].qualified()).collect()
        };
        let r = names("users");
        assert_eq!(r[..2], ["public.users".to_string(), "audit.users".to_string()]);
        assert_eq!(names("audit.us"), ["audit.users"]);
        assert!(names("upus").contains(&"public.update_user".to_string()));
        assert_eq!(names("zzz"), Vec::<String>::new());
        assert_eq!(names("").len(), 5);
        let hl = rank_objects(&objects, "audit.us", 1);
        assert_eq!(hl[0].1, vec![0, 1]);
        for q in ["", ".", "..", "é", "\u{1f}", "a.b.c.d"] {
            let _ = rank_objects(&objects, q, 3);
        }
    }

    #[test]
    fn result_titles_and_schema_switches() {
        assert_eq!(result_title("select count(*) from books").as_deref(), Some("books"));
        assert_eq!(result_title("WITH x AS (SELECT 1) SELECT * FROM x, authors a").as_deref(), Some("authors"));
        assert_eq!(result_title("select 1"), None);
        assert_eq!(result_title(""), None);
        let mut n = NavState::default();
        n.load(None);
        n.set_schema_for_path(Path::new("/p/a.sql"), Some("sales".into()));
        assert_eq!(
            n.schema_switch("console:/p/a.sql", DbKind::Postgres).as_deref(),
            Some("SET search_path TO \"sales\"")
        );
        assert_eq!(n.schema_switch("console:/p/a.sql", DbKind::Postgres), None);
        n.forget_worker("console:/p/a.sql");
        assert_eq!(n.schema_switch("console:/p/a.sql", DbKind::Mysql).as_deref(), Some("USE `sales`"));
        assert_eq!(n.schema_switch("console:/p/b.sql", DbKind::Postgres), None);
        assert_eq!(n.schema_switch("query:x", DbKind::Postgres), None);
        n.settings.introspect.insert("s".into(), vec!["Public".into()]);
        assert_eq!(n.introspected("s", &["public".into(), "other".into()]), ["public"]);
        assert_eq!(n.introspected("t", &["a".into()]), ["a"]);
        n.explorer_filter = "usr".into();
        assert!(n.filter_matches("users") && !n.filter_matches("orders"));
    }
}
