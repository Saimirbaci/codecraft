//! Track DX commands: savepoints, the project-window close guard, Run from Caret / Run
//! Selection as a script, the Data Tools pickers, Languages & Frameworks › SQL for files
//! (dialect per file or folder, default data source, the file's data source chooser) and
//! SQLite attached databases.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_db::DbKind;
use codecraft_db::savepoint::{self, SavepointOp};
use codecraft_syntax::Language;
use codecraft_workspace::ToolWindowId;

use super::{bad, console_worker, ensure_connected, failed, param_values, relative_to_project, save, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::script::{self, ScriptMode, ScriptStart};
use crate::database::sql_settings;
use crate::database::tx::{CLOSE_WINDOW, LinkReq};
use crate::database::{CONSOLES_DIR, ConnStatus, MetaReq, sync_project};
use crate::params::{bool_or, opt_path, opt_str, req_str};
use crate::popup::ActionItem;
use crate::{CommandError, Popup, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------ savepoints
        spec(
            "db.tx.savepoint",
            "Set Savepoint",
            None,
            "{name?: string (default sp<N>), connection?} — a savepoint in the active console's open transaction \
             (manual-commit mode opens one); SAVEPOINT, or SAVE TRANSACTION on SQL Server",
            Context::Global,
            always,
            |s, p| savepoint_cmd(s, p, SavepointOp::Set),
        ),
        spec(
            "db.tx.rollbackToSavepoint",
            "Rollback to Savepoint",
            None,
            "{name?: string (default the latest), connection?} — undo what ran after the savepoint; the transaction \
             stays open (also recovers a failed PostgreSQL transaction)",
            Context::Global,
            always,
            |s, p| savepoint_cmd(s, p, SavepointOp::RollbackTo),
        ),
        spec(
            "db.tx.releaseSavepoint",
            "Release Savepoint",
            None,
            "{name?: string (default the latest), connection?} — forget the savepoint (and later ones), keeping their \
             changes in the transaction; SQL Server has no RELEASE (the savepoint stays until the transaction ends)",
            Context::Global,
            always,
            |s, p| savepoint_cmd(s, p, SavepointOp::Release),
        ),
        spec(
            CLOSE_WINDOW,
            "Close Project Window",
            None,
            "closing a project window: with open transactions the pending-transaction question asks first; once they \
             ended the window may close (the UI closes it)",
            Context::Global,
            always,
            |s, _| {
                s.db.txs.allow_close_window();
                ok()
            },
        ),
        // ------------------------------------------------ scripts
        spec(
            "db.runFromCaret",
            "Run from Caret",
            None,
            "{path?, offset?: char offset (default the caret), mode?: stop|continue|ask, confirm?, params?, dataSource?} \
             — run the statements from the one at the caret to the end of the file as a script, with its execution log",
            Context::Global,
            always,
            |s, p| run_part(s, p, Part::FromCaret),
        ),
        spec(
            "db.runSelectionAsScript",
            "Run Selection as Script",
            None,
            "{path?, mode?, confirm?, params?, dataSource?} — run the selected statements as a script (error mode, \
             execution log, one result tab per query)",
            Context::Global,
            always,
            |s, p| run_part(s, p, Part::Selection),
        ),
        // ------------------------------------------------ data tools pickers
        spec(
            "db.dataTools.objects",
            "Data Tools: Schemas and Tables",
            None,
            "{dataSource, schema?} — the schemas of a data source and the tables of `schema` for the Data Tools \
             pickers (missing levels load in the background: `loading`)",
            Context::Global,
            always,
            objects,
        ),
        // ------------------------------------------------ SQL for files
        spec(
            "sql.fileSettings",
            "SQL Dialects and Data Sources for Files",
            None,
            "{dialects?: {path: dialect|null}, dataSources?: {path: dataSource|null}} — Languages & Frameworks › SQL: \
             the dialect of files and folders (generic, sqlite, postgres, mysql, mariadb, sqlserver, clickhouse, duckdb) \
             and the default data source of files (path \"\" is the whole project); returns the settings and the \
             active file's dialect and data source. Saved in .codecraft/sql-files.json",
            Context::Global,
            always,
            file_settings,
        ),
        spec(
            "db.setFileDialect",
            "SQL Dialect…",
            None,
            "{path?: file or folder (default the active file), dialect?: id|null} — the SQL dialect statements are \
             split and parameters found with; without `dialect` a chooser opens",
            Context::Global,
            always,
            set_file_dialect,
        ),
        spec(
            "db.chooseFileDataSource",
            "Choose Data Source for File…",
            None,
            "{path?: file (default the active one), dataSource?: id|null} — the data source an .sql file runs \
             against (null removes the file's own choice); without `dataSource` a chooser opens",
            Context::Global,
            always,
            choose_file_source,
        ),
        // ------------------------------------------------ SQLite attached databases
        spec(
            "db.sqlite.attach",
            "Attach SQLite Database",
            None,
            "{dataSource?, alias, path} — attach another SQLite file as schema `alias` to every connection of the \
             data source (saved as the option attach.<alias>); reconnects",
            Context::Global,
            always,
            |s, p| attach(s, p, true),
        ),
        spec(
            "db.sqlite.detach",
            "Detach SQLite Database",
            None,
            "{dataSource?, alias} — stop attaching the database; reconnects",
            Context::Global,
            always,
            |s, p| attach(s, p, false),
        ),
    ]
}

// ---------------------------------------------------------------- savepoints

/// The active SQL console's connection and data source.
fn active_console(s: &Session) -> Option<(String, String)> {
    let (_, d) = s.ws.active()?;
    if d.language != Language::Sql {
        return None;
    }
    let source = s.db.source_for_path(d.path.as_deref())?;
    Some((console_worker(d.path.as_deref(), d.id.0), source))
}

fn savepoint_cmd(s: &mut Session, p: &Value, op: SavepointOp) -> CmdResult {
    sync_project(s);
    let (worker, source) = match opt_str(p, "connection") {
        Some(w) => {
            let src = s.db.txs.conn(w).map(|c| c.source.clone()).filter(|x| !x.is_empty());
            let src = src
                .or_else(|| {
                    s.db.pending_transactions(None).into_iter().find(|t| t.connection == w).map(|t| t.data_source)
                })
                .or_else(|| active_console(s).filter(|(aw, _)| aw == w).map(|(_, src)| src))
                .ok_or_else(|| bad(format!("no connection `{w}`")))?;
            (w.to_string(), src)
        }
        None => {
            active_console(s).ok_or_else(|| CommandError::Disabled("open the SQL console of the transaction".into()))?
        }
    };
    let kind = s.db.source(&source).map(|d| d.config.kind).ok_or_else(|| bad(format!("no data source `{source}`")))?;
    if !savepoint::supported(kind) {
        return Err(CommandError::Disabled(format!("{} has no savepoints (no transactions)", kind.label())));
    }
    let live = s.db.connections_of(&source).contains(&worker);
    let manual = s.db.manual_commit(&worker, &source);
    let open = s.db.transaction_open(&worker);
    if !live || !open && !(manual && op == SavepointOp::Set) {
        return Err(CommandError::Disabled(
            "no transaction is open on this console: switch to manual commit or run BEGIN first".into(),
        ));
    }
    let known: Vec<String> = s.db.txs.conn(&worker).map(|c| c.savepoints.clone()).unwrap_or_default();
    let name = match opt_str(p, "name").map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None if op == SavepointOp::Set => {
            let mut stack = savepoint::SavepointStack::new();
            for k in &known {
                stack.apply(SavepointOp::Set, k, false);
            }
            savepoint::next_name(&stack)
        }
        None => {
            known.last().cloned().ok_or_else(|| CommandError::Disabled("the transaction has no savepoints".into()))?
        }
    };
    savepoint::check_name(kind, &name).map_err(|e| bad(e.to_string()))?;
    if op != SavepointOp::Set && !known.iter().any(|k| k.eq_ignore_ascii_case(&name)) {
        return Err(bad(format!("no savepoint `{name}` in the open transaction")));
    }
    s.db.send_link(&worker, LinkReq::Savepoint { op, name: name.clone(), manual }).map_err(failed)?;
    s.status = format!("{} {name}…", op.label());
    Ok(json!({ "connection": worker, "name": name, "op": op }))
}

// ---------------------------------------------------------------- scripts

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    FromCaret,
    Selection,
}

/// Start a script and show its log (shared with the production guard's confirmation).
pub(super) fn start_script(s: &mut Session, start: ScriptStart) -> CmdResult {
    let source = start.source.clone();
    let mode = start.mode;
    match script::start(s, start).map_err(failed)? {
        Some(id) => {
            let _ = ensure_connected(s, &source);
            s.ws.layout.show(ToolWindowId::Services);
            crate::services::show(s, crate::services::ServicesSection::Database);
            let n = s.db.scripts.get(id).map_or(0, |r| r.statements.len());
            Ok(json!({ "script": id, "statements": n, "mode": mode }))
        }
        None => Ok(json!({ "confirm": true, "writes": s.db.scripts.confirm.as_ref().map(|c| c.writes.clone()) })),
    }
}

fn run_part(s: &mut Session, p: &Value, part: Part) -> CmdResult {
    sync_project(s);
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("open an SQL file".into()))?;
    if d.language != Language::Sql {
        return Err(CommandError::Disabled("the active editor is not an SQL file".into()));
    }
    let path = match opt_path(p, "path") {
        Some(given) => crate::normalize_path(&given),
        None => d.path.clone().ok_or_else(|| bad("save the file first"))?,
    };
    if d.path.as_deref() != Some(path.as_path()) {
        return Err(bad("Run from Caret and Run Selection work on the active editor's file"));
    }
    let text = d.doc.text();
    let sel = e.selections.primary();
    let source = match opt_str(p, "dataSource") {
        Some(_) => target_source(s, p)?,
        None => {
            s.db.source_for_path(Some(&path))
                .ok_or_else(|| failed("choose a data source for this file first (db.chooseFileDataSource)"))?
        }
    };
    let dialect = s.db.dialect_for(Some(&path), Some(&source));
    let (body, first_line, scope) = match part {
        Part::FromCaret => {
            let caret = match crate::params::opt_usize(p, "offset")? {
                Some(o) => o,
                None => sel.head,
            };
            let (body, line) = script::from_caret(&text, dialect, caret.min(d.doc.len_chars()))
                .ok_or_else(|| failed("no statement at or after the caret"))?;
            (body, line, format!("from line {}", line + 1))
        }
        Part::Selection => {
            if sel.is_empty() {
                return Err(CommandError::Disabled("select the statements to run".into()));
            }
            let r = sel.range();
            let end = r.end.min(d.doc.len_chars());
            (d.doc.slice(r.start..end), script::line_of(&text, r.start), "selection".to_string())
        }
    };
    let mode = match opt_str(p, "mode") {
        Some(m) => ScriptMode::parse(m).ok_or_else(|| bad(format!("unknown mode `{m}` (stop, continue, ask)")))?,
        None => s.db.nav.settings.script_mode,
    };
    if s.db.scripts.runs.iter().any(|r| r.path == path && r.state == script::ScriptPhase::Running) {
        return Err(failed("the file is already running"));
    }
    let start = ScriptStart {
        worker: console_worker(Some(&path), 0),
        path,
        text: body,
        first_line,
        scope: Some(scope),
        source,
        mode,
        confirmed: bool_or(p, "confirm", false),
        values: param_values(p)?,
    };
    start_script(s, start)
}

// ---------------------------------------------------------------- data tools pickers

fn objects(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let ds = s.db.source(&source).ok_or_else(|| bad(format!("no data source `{source}`")))?;
    let connected = matches!(ds.status, ConnStatus::Connected(_));
    let schemas: Vec<String> = match &ds.tree.info {
        Some(_) => ds.tree.schemas.keys().cloned().collect(),
        None => Vec::new(),
    };
    let schema = opt_str(p, "schema").map(str::to_string);
    let node = schema.as_ref().and_then(|sc| ds.tree.schemas.get(sc));
    let tables: Option<Vec<Value>> = node
        .and_then(|n| n.tables.as_ref())
        .map(|ts| ts.iter().map(|t| json!({ "name": t.info.name, "kind": t.info.kind })).collect());
    let mut loading = false;
    if ds.tree.info.is_none() {
        loading = true;
        if !ds.tree.loading {
            ensure_connected(s, &source)?;
        }
    } else if let Some(sc) = &schema
        && tables.is_none()
    {
        loading = true;
        let busy = node.is_some_and(|n| n.loading);
        if !busy && (connected || node.is_some()) {
            s.db.meta(&source, MetaReq::Tables(sc.clone())).map_err(failed)?;
        }
    }
    let default = s.db.default_schema(&source);
    Ok(json!({ "schemas": schemas, "default": default, "tables": tables, "loading": loading }))
}

// ---------------------------------------------------------------- SQL for files

fn active_sql_path(s: &Session, p: &Value) -> Result<PathBuf, CommandError> {
    match opt_path(p, "path") {
        Some(path) => Ok(crate::normalize_path(&path)),
        None => {
            s.ws.active()
                .filter(|(_, d)| d.language == Language::Sql)
                .and_then(|(_, d)| d.path.clone())
                .ok_or_else(|| bad("no SQL file: pass `path` or open one"))
        }
    }
}

fn key_of(s: &Session, raw: &str) -> Result<String, CommandError> {
    let t = raw.trim();
    if t.is_empty() || t == "." {
        return Ok(String::new());
    }
    let path = Path::new(t);
    let path = if path.is_absolute() { crate::normalize_path(path) } else { path.to_path_buf() };
    s.db.sql_files_key(&path).map_err(bad)
}

fn file_settings(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let mut changed = false;
    if let Some(v) = p.get("dialects") {
        let m = v.as_object().ok_or_else(|| bad("`dialects` must be an object {path: dialect|null}"))?;
        for (k, d) in m {
            let key = key_of(s, k)?;
            let dialect = match d {
                Value::Null => None,
                Value::String(x) if x.trim().is_empty() => None,
                Value::String(x) => Some(x.as_str()),
                _ => return Err(bad("a dialect must be a string or null")),
            };
            s.db.sql_files.set_dialect(key, dialect).map_err(bad)?;
            changed = true;
        }
    }
    if let Some(v) = p.get("dataSources") {
        let m = v.as_object().ok_or_else(|| bad("`dataSources` must be an object {path: dataSource|null}"))?;
        for (k, d) in m {
            let key = key_of(s, k)?;
            let id = match d {
                Value::Null => None,
                Value::String(x) if x.trim().is_empty() => None,
                Value::String(x) => {
                    if s.db.source(x).is_none() {
                        return Err(bad(format!("no data source `{x}`")));
                    }
                    Some(x.clone())
                }
                _ => return Err(bad("a data source must be a string or null")),
            };
            s.db.sql_files.set_source(key, id).map_err(bad)?;
            changed = true;
        }
    }
    if changed {
        s.db.save_sql_files().map_err(|e| failed(format!("cannot save the SQL file settings: {e}")))?;
        s.status = "SQL settings saved".into();
    }
    let active = s.ws.active().filter(|(_, d)| d.language == Language::Sql).map(|(_, d)| d.path.clone());
    let active = match active {
        Some(path) => {
            let source = s.db.source_for_path(path.as_deref());
            let (dialect, from) = s.db.dialect_info(path.as_deref(), source.as_deref());
            json!({ "path": path, "dataSource": source, "dialect": dialect, "dialectFrom": from })
        }
        None => Value::Null,
    };
    let ids: Vec<Value> =
        sql_settings::DIALECTS.iter().map(|(id, label)| json!({ "id": id, "label": label })).collect();
    Ok(
        json!({ "dialects": s.db.sql_files.dialects, "dataSources": s.db.sql_files.data_sources, "active": active, "available": ids }),
    )
}

fn editor_id(s: &Session) -> codecraft_workspace::EditorId {
    s.ws.active().map(|(e, _)| e.id).unwrap_or(codecraft_workspace::EditorId(0))
}

fn set_file_dialect(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let path = active_sql_path(s, p)?;
    let key = s.db.sql_files_key(&path).map_err(bad)?;
    let Some(value) = p.get("dialect") else {
        let current = s.db.sql_files.dialects.get(&key).cloned();
        let mark = |on: bool| if on { "✓ " } else { "" };
        let mut items = vec![ActionItem {
            title: format!("{}Data source's dialect", mark(current.is_none())),
            kind: Some("sql".into()),
            payload: json!({ "codecraftCommand": "db.setFileDialect", "codecraftParams": { "path": path, "dialect": null } }),
        }];
        for (id, label) in sql_settings::DIALECTS {
            items.push(ActionItem {
                title: format!("{}{label}", mark(current.as_deref() == Some(*id))),
                kind: Some("sql".into()),
                payload: json!({ "codecraftCommand": "db.setFileDialect", "codecraftParams": { "path": path, "dialect": id } }),
            });
        }
        s.popup = Some(Popup::Actions {
            editor: editor_id(s),
            title: "SQL Dialect".into(),
            items,
            selected: 0,
            pending: false,
        });
        return Ok(json!({ "chooser": true, "dialect": current }));
    };
    let dialect = match value {
        Value::Null => None,
        Value::String(d) if d.trim().is_empty() => None,
        Value::String(d) => Some(d.as_str()),
        _ => return Err(bad("`dialect` must be a string or null")),
    };
    s.db.sql_files.set_dialect(key.clone(), dialect).map_err(bad)?;
    s.db.save_sql_files().map_err(|e| failed(format!("cannot save the SQL file settings: {e}")))?;
    let shown = s.db.sql_files.dialects.get(&key).cloned();
    s.popup = None;
    s.status = match &shown {
        Some(d) => format!("SQL dialect: {}", sql_settings::label(d)),
        None => "SQL dialect: the data source's".into(),
    };
    Ok(json!({ "path": path, "dialect": shown }))
}

fn choose_file_source(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let path = active_sql_path(s, p)?;
    let console = s.db.project_root().is_some_and(|root| path.starts_with(root.join(CONSOLES_DIR)));
    let Some(value) = p.get("dataSource") else {
        if console {
            return Err(failed("consoles belong to the data source of their folder"));
        }
        let current = s.db.source_for_path(Some(&path));
        let bound = s.db.bindings.get(&path).cloned();
        let mark = |on: bool| if on { "✓ " } else { "" };
        let mut items = Vec::new();
        for ds in &s.db.sources {
            let on = current.as_deref() == Some(ds.config.id.as_str());
            items.push(ActionItem {
                title: format!("{}{} ({})", mark(on), ds.config.name, ds.config.kind.label()),
                kind: Some("sql".into()),
                payload: json!({ "codecraftCommand": "db.chooseFileDataSource",
                                 "codecraftParams": { "path": path, "dataSource": ds.config.id } }),
            });
        }
        if bound.is_some() {
            items.push(ActionItem {
                title: "Use the default data source".into(),
                kind: Some("sql".into()),
                payload: json!({ "codecraftCommand": "db.chooseFileDataSource",
                                 "codecraftParams": { "path": path, "dataSource": null } }),
            });
        }
        if items.is_empty() {
            return Err(CommandError::Disabled("no data sources: add one first".into()));
        }
        s.popup = Some(Popup::Actions {
            editor: editor_id(s),
            title: "Data Source for File".into(),
            items,
            selected: 0,
            pending: false,
        });
        return Ok(json!({ "chooser": true, "dataSource": current }));
    };
    s.popup = None;
    match value {
        Value::String(id) if !id.trim().is_empty() => {
            s.execute("db.setDataSource", &json!({ "dataSource": id, "path": path }))?;
            s.status =
                format!("{} runs against {id}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
            Ok(json!({ "path": path, "dataSource": id }))
        }
        Value::Null | Value::String(_) => {
            if console {
                return Err(failed("consoles belong to the data source of their folder"));
            }
            s.db.bindings.remove(&path);
            save(s)?;
            let now = s.db.source_for_path(Some(&path));
            s.status = match &now {
                Some(id) => format!("Default data source: {id}"),
                None => "No data source for this file".into(),
            };
            Ok(json!({ "path": path, "dataSource": now }))
        }
        _ => Err(bad("`dataSource` must be a string or null")),
    }
}

// ---------------------------------------------------------------- SQLite attached databases

fn attach(s: &mut Session, p: &Value, add: bool) -> CmdResult {
    sync_project(s);
    let id = target_source(s, p)?;
    let kind = s.db.source(&id).map(|d| d.config.kind).unwrap_or_default();
    if kind != DbKind::Sqlite {
        return Err(CommandError::Disabled("only SQLite data sources attach databases".into()));
    }
    let alias = req_str(p, "alias")?.trim().to_string();
    codecraft_db::sqlite::check_alias(&alias).map_err(|e| bad(e.to_string()))?;
    // Reconnecting would end a transaction: ask for Commit or Rollback first.
    if !s.db.open_transactions(Some(&id)).is_empty() {
        return Err(CommandError::Disabled(
            "a transaction is open on this data source: commit or roll back first".into(),
        ));
    }
    let key = format!("attach.{alias}");
    let value = if add {
        let path = crate::params::req_path(p, "path")?;
        let abs = if path.is_relative() {
            s.db.project_root().map(|r| r.join(&path)).unwrap_or_else(|| path.clone())
        } else {
            path.clone()
        };
        if !abs.is_file() {
            return Err(bad(format!("no database file {}", abs.display())));
        }
        Some(relative_to_project(s, &path).display().to_string())
    } else {
        None
    };
    let had = s.db.source(&id).is_some_and(|d| d.config.options.contains_key(&key));
    if !add && !had {
        return Err(bad(format!("`{alias}` is not attached")));
    }
    if let Some(ds) = s.db.source_mut(&id) {
        match &value {
            Some(v) => {
                ds.config.options.insert(key, v.clone());
            }
            None => {
                ds.config.options.remove(&key);
            }
        }
    }
    save(s)?;
    // Every connection attaches on open: reconnect (no transaction is open, checked above).
    s.db.disconnect(Some(&id));
    ensure_connected(s, &id)?;
    s.status = if add { format!("Attached {alias}") } else { format!("Detached {alias}") };
    Ok(json!({ "dataSource": id, "alias": alias, "attached": add }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn specs_have_labels() {
        for s in super::specs() {
            assert!(!s.label.is_empty() && !s.params.is_empty(), "{}", s.id);
        }
    }
}
