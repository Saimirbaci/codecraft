//! SQL editor and explorer commands (track DC): navigation from SQL (Go to Declaration, Quick
//! Documentation, Find Usages), quick-fixes, alias/CTE rename, attached files and Run 'file.sql',
//! Go to Database Object, the explorer's filter, schemas to introspect and node refresh, and
//! pinned result tabs. State lives in [`crate::database::sql_nav`] and
//! [`crate::database::script`].
//!
//! Go to Declaration, Quick Documentation, Find Usages, Rename and Show Context Actions are the
//! IDE's own commands: [`route`] makes them handle SQL (an `.sql` file, or SQL in a Rust
//! string) and leaves everything else to the language server as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};

use codecraft_db::TableKind;
use codecraft_syntax::Language;
use codecraft_workspace::ToolWindowId;

use super::{bad, console_worker, ensure_connected, failed, opt_u64, param_values, save, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, Enabled, Run, always, ok, spec};
use crate::database::script::{self, ScriptMode, ScriptStart};
use crate::database::sql_nav::{self, DbObjectKind, GotoObjectPopup, NavAction, SchemasDialog, resolve};
use crate::database::{CONSOLES_DIR, ConnStatus, MetaReq, sync_project};
use crate::params::{bool_or, opt_path, opt_str, opt_usize, req_str};
use crate::popup::ActionItem;
use crate::{CommandError, Popup, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------ navigation in SQL
        spec(
            "sql.gotoDeclaration",
            "Go to Database Object Declaration",
            None,
            "{offset?: int} — the name at the caret (or char offset) of an SQL file or SQL in a Rust string: an alias or CTE \
             jumps to its definition; a table, view or column is selected in the Database tool window and its DDL opens in a \
             read-only tab; a routine opens its definition. Waits for missing metadata (returns {pending: true})",
            Context::Editor,
            in_sql,
            |s, p| sql_nav::navigate(s, opt_usize(p, "offset")?, NavAction::Declaration),
        ),
        spec(
            "sql.quickDoc",
            "SQL Quick Documentation",
            None,
            "{offset?: int} — documentation of the table, view, column, routine, schema or CTE at the caret: its kind, \
             comment, columns and DDL (shown as a popup, returned as {text})",
            Context::Editor,
            in_sql,
            |s, p| sql_nav::navigate(s, opt_usize(p, "offset")?, NavAction::QuickDoc),
        ),
        spec(
            "sql.findUsages",
            "Find Usages in SQL",
            None,
            "{offset?: int} or {table: string, column?: string, schema?: string} — usages of the table or column at the caret \
             (or given) in the project's .sql files and SQL string literals of Rust files, listed in the Find tool window \
             (searched in the background); a CTE's usages in its statement",
            Context::Global,
            always,
            find_usages,
        ),
        spec(
            "sql.renameAlias",
            "Rename Alias or CTE",
            None,
            "{newName: string, offset?: int} — renames the table alias or common table expression at the caret everywhere \
             in its statement (quoted when the name needs it)",
            Context::Editor,
            in_sql,
            rename_alias,
        ),
        spec(
            "sql.quickFixes",
            "SQL Quick-Fixes",
            None,
            "{offset?: int} — the SQL problems at the caret and their fixes: [{title, code, fix: {edits, caret} | command}]",
            Context::Editor,
            in_sql,
            |s, p| {
                let ctx = sql_nav::context(s, opt_usize(p, "offset")?).ok_or_else(not_sql)?;
                Ok(json!({ "fixes": sql_nav::fixes_at(s, &ctx) }))
            },
        ),
        spec(
            "sql.applyFix",
            "Apply SQL Quick-Fix",
            None,
            "{index?: int, title?: string, offset?: int} — apply a fix of sql.quickFixes at the caret (by index or title; \
             default the first)",
            Context::Editor,
            in_sql,
            apply_fix,
        ),
        spec(
            "sql.openDdl",
            "Open DDL",
            None,
            "{dataSource, schema, name, column?, routine?: bool} — the DDL of a table, view or routine in a read-only tab \
             (selected in the Database tool window too)",
            Context::Global,
            always,
            open_ddl,
        ),
        spec(
            "sql.objectInfo",
            "Database Object Documentation",
            None,
            "{dataSource, schema, name, column?} — {doc, ddl, loading} of a table or view (missing metadata is requested; \
             ask again when `loading`)",
            Context::Global,
            always,
            object_info,
        ),
        // ------------------------------------------------ attached files and scripts
        spec(
            "db.attachFile",
            "Attach SQL File to Data Source",
            None,
            "{path?, dataSource?, schema?: string|null} — the data source (and schema: SET search_path / USE before \
             running) an .sql file runs against; consoles keep their data source but take a schema. Saved in the project",
            Context::Global,
            always,
            attach_file,
        ),
        spec(
            "db.runFile",
            "Run SQL File",
            None,
            "{path?, mode?: stop|continue|ask, confirm?: bool, params?: {name: sqlText}, dataSource?} — runs every statement \
             of the file (default: the active one) on its data source: one result tab per query, an execution log \
             (db.scriptLog). `mode` defaults to the project's script error mode",
            Context::Global,
            always,
            run_file,
        ),
        spec(
            "db.scriptDecision",
            "Continue or Stop Script",
            None,
            "{script?: id, action: continue|continueAll|stop} — answer a script that stopped at a failed statement (mode ask)",
            Context::Global,
            always,
            script_decision,
        ),
        spec(
            "db.scriptConfirm",
            "Confirm Script Data Modification",
            None,
            "{confirm: bool} — run (or not) the script the production guard asked about",
            Context::Global,
            always,
            script_confirm,
        ),
        spec(
            "db.scriptLog",
            "Script Execution Log",
            None,
            "{script?: id} — a script run (default: the latest): statements with line, status, time, rows, errors",
            Context::Global,
            always,
            script_log,
        ),
        spec(
            "db.showScriptLog",
            "Show Script Execution Log",
            None,
            "{script?: id|null} — show a script's execution log in Services (null hides it)",
            Context::Global,
            always,
            show_script_log,
        ),
        spec(
            "db.setScriptMode",
            "Script Error Mode",
            None,
            "{mode: stop|continue|ask} — what Run 'file.sql' does when a statement fails (saved in the project)",
            Context::Global,
            always,
            set_script_mode,
        ),
        // ------------------------------------------------ results
        spec(
            "db.results.pin",
            "Pin Result Tab",
            None,
            "{result?, pinned?: bool} — a pinned tab is kept when the console runs again (the next run opens a new tab); \
             without `pinned` toggles",
            Context::Global,
            always,
            pin_result,
        ),
        // ------------------------------------------------ explorer
        spec(
            "db.gotoObject",
            "Database Object…",
            Some("Alt+Shift+Cmd+O"),
            "{query?: string} — Navigate › Database Object: a popup searching every table, view, routine and schema of the \
             connected data sources (loads what is missing)",
            Context::Global,
            always,
            goto_object,
        ),
        spec(
            "db.gotoObjectResults",
            "Database Object Results",
            None,
            "{query?: string, limit?: int} — ranked objects: [{dataSource, schema, name, kind}]",
            Context::Global,
            always,
            goto_object_results,
        ),
        spec(
            "db.gotoObjectAccept",
            "Open Database Object",
            None,
            "{index?: int, open?: tree|data|ddl} — the popup's selected (or `index`th) object: select it in the Database tool \
             window (`tree`, default), open its data or its DDL",
            Context::Global,
            always,
            goto_object_accept,
        ),
        spec("db.gotoObjectClose", "Close Database Object Popup", None, "", Context::Global, always, |s, _| {
            s.db.nav.goto = None;
            ok()
        }),
        spec(
            "db.explorer.filter",
            "Filter Database Explorer",
            None,
            "{text: string} — show only tables, views, routines and schemas whose name matches (empty: everything)",
            Context::Global,
            always,
            |s, p| {
                s.db.nav.explorer_filter = opt_str(p, "text").unwrap_or_default().chars().take(200).collect();
                ok()
            },
        ),
        spec(
            "db.introspectSchemas",
            "Schemas to Introspect…",
            None,
            "{dataSource, schemas?: [string]|null} — the schemas listed and loaded for a data source (null or []: all); \
             without `schemas` opens the chooser. Saved in the project",
            Context::Global,
            always,
            introspect_schemas,
        ),
        spec(
            "db.refreshNode",
            "Refresh Database Node",
            None,
            "{dataSource, schema?, table?} — reload one level: the schema list, a schema's tables and routines, or a \
             table's columns and comments",
            Context::Global,
            always,
            refresh_node,
        ),
        spec(
            "db.routines",
            "List Routines",
            None,
            "{dataSource, schema} — functions and procedures of a schema (loads them; `loading` until they arrive)",
            Context::Global,
            always,
            routines,
        ),
    ]
}

fn not_sql() -> CommandError {
    CommandError::Disabled("the caret is not in SQL".into())
}

fn in_sql(s: &Session) -> Result<(), String> {
    if sql_nav::in_sql(s) { Ok(()) } else { Err("the caret is not in SQL".into()) }
}

// ---------------------------------------------------------------- routing of IDE commands

type Pair = (Enabled, Run);

static ORIGINAL: OnceLock<HashMap<&'static str, Pair>> = OnceLock::new();

const DECLARATION: &str = "navigate.declarationOrUsages";
const QUICK_DOC: &str = "view.quickDocumentation";
const FIND_USAGES: &str = "edit.findUsages.findUsages";
const RENAME: &str = "refactor.rename";
const CONTEXT_ACTIONS: &str = "code.showContextActions";

/// Make the IDE's navigation commands handle SQL: the registry calls this once with every
/// spec; the commands' own behaviour stays for everything else.
pub(crate) fn route(specs: &mut [CommandSpec]) {
    let mut orig = HashMap::new();
    for spec in specs.iter_mut() {
        let (enabled, run): Pair = match spec.id {
            DECLARATION => (declaration_enabled, declaration_run),
            QUICK_DOC => (quick_doc_enabled, quick_doc_run),
            FIND_USAGES => (find_usages_enabled, find_usages_run),
            RENAME => (rename_enabled, rename_run),
            CONTEXT_ACTIONS => (actions_enabled, actions_run),
            _ => continue,
        };
        orig.insert(spec.id, (spec.enabled, spec.run));
        spec.enabled = enabled;
        spec.run = run;
    }
    let _ = ORIGINAL.set(orig);
}

fn orig_enabled(id: &str, s: &Session) -> Result<(), String> {
    match ORIGINAL.get().and_then(|m| m.get(id)) {
        Some((e, _)) => e(s),
        None => Err("not available".into()),
    }
}

fn orig_run(id: &str, s: &mut Session, p: &Value) -> CmdResult {
    match ORIGINAL.get().and_then(|m| m.get(id)) {
        Some((_, r)) => r(s, p),
        None => Err(CommandError::Unknown(id.to_string())),
    }
}

fn sql_doc(s: &Session) -> bool {
    s.ws.active().is_some_and(|(_, d)| d.language == Language::Sql)
}

/// SQL handles it; in a Rust string, the language server gets what SQL has nothing for.
fn sql_or(id: &str, s: &mut Session, p: &Value, action: NavAction) -> CmdResult {
    match sql_nav::navigate(s, None, action) {
        Ok(v) => Ok(v),
        Err(e) if !sql_doc(s) && orig_enabled(id, s).is_ok() => {
            log::debug!("{id} in SQL: {e}; asking the language server");
            orig_run(id, s, p)
        }
        Err(e) => Err(e),
    }
}

fn declaration_enabled(s: &Session) -> Result<(), String> {
    if sql_nav::in_sql(s) { Ok(()) } else { orig_enabled(DECLARATION, s) }
}

fn declaration_run(s: &mut Session, p: &Value) -> CmdResult {
    if sql_nav::in_sql(s) { sql_or(DECLARATION, s, p, NavAction::Declaration) } else { orig_run(DECLARATION, s, p) }
}

fn quick_doc_enabled(s: &Session) -> Result<(), String> {
    if sql_nav::in_sql(s) { Ok(()) } else { orig_enabled(QUICK_DOC, s) }
}

fn quick_doc_run(s: &mut Session, p: &Value) -> CmdResult {
    if sql_nav::in_sql(s) { sql_or(QUICK_DOC, s, p, NavAction::QuickDoc) } else { orig_run(QUICK_DOC, s, p) }
}

fn find_usages_enabled(s: &Session) -> Result<(), String> {
    if sql_nav::in_sql(s) { Ok(()) } else { orig_enabled(FIND_USAGES, s) }
}

fn find_usages_run(s: &mut Session, p: &Value) -> CmdResult {
    if !sql_nav::in_sql(s) {
        return orig_run(FIND_USAGES, s, p);
    }
    match find_usages(s, &Value::Null) {
        Err(_) if !sql_doc(s) && orig_enabled(FIND_USAGES, s).is_ok() => orig_run(FIND_USAGES, s, p),
        r => r,
    }
}

/// The caret is on an alias or CTE (not during an in-place rename).
fn sql_rename_here(s: &Session) -> bool {
    s.completion.rename.is_none()
        && sql_nav::context(s, None).is_some_and(|c| resolve::rename_plan(&c.sql, c.offset).is_some())
}

fn rename_enabled(s: &Session) -> Result<(), String> {
    // Cheap (asked every frame by the menus): the alias check happens when it runs.
    if sql_nav::in_sql(s) { Ok(()) } else { orig_enabled(RENAME, s) }
}

fn rename_run(s: &mut Session, p: &Value) -> CmdResult {
    if !sql_rename_here(s) {
        if orig_enabled(RENAME, s).is_ok() {
            return orig_run(RENAME, s, p);
        }
        return Err(CommandError::Disabled("Rename in SQL works on table aliases and common table expressions".into()));
    }
    match opt_str(p, "newName") {
        Some(_) => rename_alias(s, p),
        None => {
            // Ask for the name (the Rename popup calls back with `newName`).
            let ctx = sql_nav::context(s, None).ok_or_else(not_sql)?;
            let plan = resolve::rename_plan(&ctx.sql, ctx.offset).ok_or_else(not_sql)?;
            s.popup =
                Some(Popup::Rename { editor: ctx.editor, offset: ctx.to_doc(ctx.offset), name: plan.old.clone() });
            Ok(json!({ "rename": plan.old, "kind": plan.kind, "occurrences": plan.spans.len() }))
        }
    }
}

fn actions_enabled(s: &Session) -> Result<(), String> {
    if sql_doc(s) || sql_nav::in_sql(s) && s.lsp_ready().is_err() { Ok(()) } else { orig_enabled(CONTEXT_ACTIONS, s) }
}

fn actions_run(s: &mut Session, p: &Value) -> CmdResult {
    if !(sql_doc(s) || sql_nav::in_sql(s) && s.lsp_ready().is_err()) {
        return orig_run(CONTEXT_ACTIONS, s, p);
    }
    let mut items: Vec<ActionItem> = sql_nav::context_actions(s);
    if !sql_doc(s) {
        items.extend(crate::sql_lang::context_actions(s));
    }
    let Some(editor) = s.ws.active().map(|(e, _)| e.id) else { return ok() };
    if items.is_empty() {
        s.status = "No context actions available here".into();
        return Ok(json!({ "actions": 0 }));
    }
    let n = items.len();
    s.popup = Some(Popup::Actions { editor, title: "Context Actions".into(), items, selected: 0, pending: false });
    Ok(json!({ "actions": n }))
}

// ---------------------------------------------------------------- navigation commands

fn find_usages(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    if let Some(table) = opt_str(p, "table") {
        let target = sql_nav::usages::UsageTarget {
            schema: opt_str(p, "schema").map(str::to_string),
            table: table.to_string(),
            column: opt_str(p, "column").map(str::to_string),
        };
        return sql_nav::start_usages(s, target);
    }
    let ctx = sql_nav::context(s, opt_usize(p, "offset")?).ok_or_else(not_sql)?;
    match sql_nav::usage_target(s, &ctx) {
        Some(sql_nav::UsageQuery::Project(target)) => sql_nav::start_usages(s, target),
        Some(sql_nav::UsageQuery::Statement(name, spans)) => {
            let n = spans.len();
            sql_nav::show_spans(s, &ctx, format!("Usages of '{name}'"), &spans);
            Ok(json!({ "usages": n }))
        }
        None => Err(failed("no table or column at the caret")),
    }
}

fn rename_alias(s: &mut Session, p: &Value) -> CmdResult {
    let new_name = req_str(p, "newName")?.trim().to_string();
    if new_name.is_empty() || new_name.chars().count() > 128 || new_name.chars().any(char::is_control) {
        return Err(bad("`newName` must be a name of 1–128 characters"));
    }
    let ctx = sql_nav::context(s, opt_usize(p, "offset")?).ok_or_else(not_sql)?;
    let plan = resolve::rename_plan(&ctx.sql, ctx.offset).ok_or_else(|| failed("no alias or CTE at the caret"))?;
    let mysql = ctx
        .source
        .as_deref()
        .and_then(|id| s.db.source(id))
        .is_some_and(|d| matches!(d.config.kind, codecraft_db::DbKind::Mysql | codecraft_db::DbKind::Mariadb));
    // A name typed with quotes is kept as typed.
    let text = if new_name.starts_with(['"', '`']) { new_name.clone() } else { resolve::identifier(&new_name, mysql) };
    let edits: Vec<_> =
        plan.spans.iter().map(|&(start, end)| sql_nav::inspect::SqlEdit { start, end, text: text.clone() }).collect();
    // Keep the caret on the renamed name.
    let first_after = plan.spans.iter().position(|&(a, b)| a <= ctx.offset && ctx.offset <= b).unwrap_or(0);
    let shift: isize =
        plan.spans.iter().take(first_after).map(|&(a, b)| text.chars().count() as isize - (b - a) as isize).sum();
    let caret = plan.spans.get(first_after).map(|&(a, _)| (a as isize + shift).max(0) as usize);
    sql_nav::apply_edits(s, &ctx, &edits, caret)?;
    s.popup = None;
    s.status = format!("Renamed {} '{}' to {text} ({} occurrences)", plan.kind, plan.old, plan.spans.len());
    Ok(json!({ "renamed": plan.old, "to": text, "occurrences": plan.spans.len() }))
}

fn apply_fix(s: &mut Session, p: &Value) -> CmdResult {
    let ctx = sql_nav::context(s, opt_usize(p, "offset")?).ok_or_else(not_sql)?;
    let fixes = sql_nav::fixes_at(s, &ctx);
    let chosen = match (opt_str(p, "title"), opt_usize(p, "index")?) {
        (Some(t), _) => fixes.into_iter().find(|f| f.title == t),
        (None, i) => fixes.into_iter().nth(i.unwrap_or(0)),
    }
    .ok_or_else(|| failed("no such quick-fix here"))?;
    if let Some((id, params)) = &chosen.command {
        return s.execute(id, params);
    }
    let fix = chosen.fix.ok_or_else(|| failed("the quick-fix has no changes"))?;
    sql_nav::apply_edits(s, &ctx, &fix.edits, fix.caret)?;
    s.popup = None;
    s.status = fix.title.clone();
    Ok(json!({ "applied": fix.title }))
}

fn object_params(s: &Session, p: &Value) -> Result<(String, String, String), CommandError> {
    let source = target_source(s, p)?;
    Ok((source, req_str(p, "schema")?.to_string(), req_str(p, "name")?.to_string()))
}

fn open_ddl(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let (source, schema, name) = object_params(s, p)?;
    let target = if bool_or(p, "routine", false) {
        resolve::SqlTarget::Routine { schema: Some(schema), name }
    } else if let Some(c) = opt_str(p, "column") {
        resolve::SqlTarget::Column { name: c.to_string(), tables: vec![(Some(schema), name)] }
    } else {
        resolve::SqlTarget::Table { schema: Some(schema), name }
    };
    sql_nav::navigate_object(s, &source, target, NavAction::Declaration)
}

fn object_info(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let (source, schema, name) = object_params(s, p)?;
    let ds = s.db.source(&source).ok_or_else(|| bad(format!("no data source `{source}`")))?;
    if !matches!(ds.status, ConnStatus::Connected(_)) {
        ensure_connected(s, &source)?;
        return Ok(json!({ "loading": true }));
    }
    let Some(node) = ds.table(&schema, &name) else {
        let loaded = ds.tree.schemas.get(&schema).is_some_and(|n| n.tables.is_some());
        if !loaded {
            s.db.meta(&source, MetaReq::Tables(schema.clone())).map_err(failed)?;
            return Ok(json!({ "loading": true }));
        }
        return Err(bad(format!("no table {schema}.{name}")));
    };
    let kind = node.info.kind;
    let need_details = node.details.is_none() && !node.loading;
    if need_details {
        s.db.meta(&source, MetaReq::Details(schema.clone(), name.clone())).map_err(failed)?;
    }
    let what = if matches!(kind, TableKind::View | TableKind::MaterializedView) {
        sql_nav::ddl::InfoKind::View
    } else {
        sql_nav::ddl::InfoKind::Table
    };
    sql_nav::request_info(&mut s.db, &source, &schema, &name, what, false);
    let loading = s.db.source(&source).and_then(|d| d.table(&schema, &name)).is_some_and(|t| t.details.is_none())
        || s.db.nav.info.get(&(source.clone(), schema.clone(), name.clone())).is_some_and(|i| i.loading);
    let doc = sql_nav::object_doc(s, &source, &schema, &name, kind, opt_str(p, "column"));
    let ds = s.db.source(&source);
    let node = ds.and_then(|d| d.table(&schema, &name));
    let ddl = sql_nav::ddl::table_ddl(
        ds.map(|d| d.config.kind).unwrap_or_default(),
        &schema,
        &name,
        kind,
        node.and_then(|t| t.details.as_ref()),
        s.db.nav.info.get(&(source.clone(), schema.clone(), name.clone())),
    );
    Ok(json!({ "loading": loading, "doc": doc, "ddl": ddl }))
}

// ---------------------------------------------------------------- attached files and scripts

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

fn is_console(s: &Session, path: &Path) -> bool {
    s.db.project_root().is_some_and(|root| path.starts_with(root.join(CONSOLES_DIR)))
}

fn attach_file(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let path = active_sql_path(s, p)?;
    let source = match opt_str(p, "dataSource") {
        Some(id) => {
            s.db.source(id).ok_or_else(|| bad(format!("no data source `{id}`")))?;
            id.to_string()
        }
        None => s.db.source_for_path(Some(&path)).ok_or_else(|| bad("missing `dataSource`"))?,
    };
    let console = is_console(s, &path);
    if console && s.db.source_for_path(Some(&path)).as_deref() != Some(source.as_str()) {
        return Err(failed(
            "consoles belong to the data source of their folder; open a console of the other data source",
        ));
    }
    let worker = console_worker(Some(&path), 0);
    let old_source = s.db.source_for_path(Some(&path));
    let old_schema = s.db.nav.schema_for_path(&path);
    let schema = match p.get("schema") {
        None => old_schema.clone().filter(|_| old_source.as_deref() == Some(source.as_str())),
        Some(Value::Null) => None,
        Some(Value::String(v)) if v.trim().is_empty() => None,
        Some(Value::String(v)) => Some(v.trim().to_string()),
        Some(_) => return Err(bad("`schema` must be a string or null")),
    };
    if old_source.as_deref() != Some(source.as_str()) || old_schema != schema {
        // The file's connection starts over (in the new data source or default schema).
        sql_nav::close_connection(&mut s.db, &worker).map_err(failed)?;
    }
    if !console {
        s.db.bindings.insert(path.clone(), source.clone());
        save(s)?;
    }
    s.db.nav.set_schema_for_path(&path, schema.clone());
    s.db.nav.save().map_err(|e| failed(format!("cannot save the SQL editor settings: {e}")))?;
    ensure_connected(s, &source)?;
    // The file's names resolve in its schema: load it.
    if let Some(sc) = &schema
        && s.db
            .source(&source)
            .is_some_and(|d| d.tree.schemas.get(sc).is_some_and(|n| n.tables.is_none() && !n.loading))
    {
        let _ = s.db.meta(&source, MetaReq::Tables(sc.clone()));
    }
    s.status = match &schema {
        Some(sc) => format!(
            "{} runs against {source} ({sc})",
            path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        ),
        None => format!("{} runs against {source}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()),
    };
    Ok(json!({ "path": path, "dataSource": source, "schema": schema }))
}

fn script_text(s: &Session, path: &Path) -> Result<String, CommandError> {
    if let Some(d) = s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        return Ok(d.doc.text());
    }
    let size = std::fs::metadata(path).map_err(|e| failed(format!("{}: {e}", path.display())))?.len();
    if size > (script::MAX_SCRIPT_CHARS as u64).saturating_mul(4) {
        return Err(failed(format!("{} is too large to run", path.display())));
    }
    codecraft_fs::read_text_file(path).map_err(|e| failed(e.to_string()))
}

fn run_file(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let path = active_sql_path(s, p)?;
    let source = match opt_str(p, "dataSource") {
        Some(_) => target_source(s, p)?,
        None => {
            s.db.source_for_path(Some(&path))
                .ok_or_else(|| failed("attach the file to a data source first (db.attachFile or the editor toolbar)"))?
        }
    };
    let mode = match opt_str(p, "mode") {
        Some(m) => ScriptMode::parse(m).ok_or_else(|| bad(format!("unknown mode `{m}` (stop, continue, ask)")))?,
        None => s.db.nav.settings.script_mode,
    };
    if s.db.scripts.runs.iter().any(|r| r.path == path && r.state == script::ScriptPhase::Running) {
        return Err(failed("the file is already running"));
    }
    let text = script_text(s, &path)?;
    let worker = console_worker(Some(&path), 0);
    let start = ScriptStart {
        path: path.clone(),
        text,
        first_line: 0,
        scope: None,
        source: source.clone(),
        worker,
        mode,
        confirmed: bool_or(p, "confirm", false),
        values: param_values(p)?,
    };
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

fn script_confirm(s: &mut Session, p: &Value) -> CmdResult {
    let Some(c) = s.db.scripts.confirm.take() else { return Err(failed("no script is waiting for confirmation")) };
    if !bool_or(p, "confirm", false) {
        s.status = "Cancelled".into();
        return ok();
    }
    // Run from Caret / Run Selection run the same part again (track DX).
    if let Some((text, first_line, scope, values)) = c.part {
        let start = ScriptStart {
            worker: console_worker(Some(&c.path), 0),
            path: c.path,
            text,
            first_line,
            scope: Some(scope),
            source: c.source,
            mode: c.mode,
            confirmed: true,
            values,
        };
        return super::files::start_script(s, start);
    }
    run_file(s, &json!({ "path": c.path, "dataSource": c.source, "mode": c.mode, "confirm": true }))
}

fn script_id(s: &Session, p: &Value) -> Result<u64, CommandError> {
    match opt_u64(p, "script")? {
        Some(id) => s.db.scripts.get(id).map(|r| r.id).ok_or_else(|| bad(format!("no script {id}"))),
        None => s.db.scripts.runs.last().map(|r| r.id).ok_or_else(|| bad("no script has run")),
    }
}

fn script_decision(s: &mut Session, p: &Value) -> CmdResult {
    let id = match opt_u64(p, "script")? {
        Some(id) => id,
        None => {
            s.db.scripts
                .runs
                .iter()
                .rev()
                .find(|r| r.state == script::ScriptPhase::Asking)
                .map(|r| r.id)
                .ok_or_else(|| failed("no script is waiting for an answer"))?
        }
    };
    let action = req_str(p, "action")?;
    let (go_on, all) = match action {
        "continue" => (true, false),
        "continueAll" | "continue-all" => (true, true),
        "stop" => (false, false),
        other => return Err(bad(format!("unknown action `{other}` (continue, continueAll, stop)"))),
    };
    script::decide(s, id, go_on, all).map_err(failed)?;
    Ok(json!({ "script": id }))
}

fn script_log(s: &mut Session, p: &Value) -> CmdResult {
    let id = script_id(s, p)?;
    let r = s.db.scripts.get(id).ok_or_else(|| bad(format!("no script {id}")))?;
    let mut v = serde_json::to_value(r).map_err(|e| failed(e.to_string()))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("summary".into(), json!(r.summary()));
        o.insert("asking".into(), json!(r.asking_at()));
    }
    Ok(v)
}

fn show_script_log(s: &mut Session, p: &Value) -> CmdResult {
    match p.get("script") {
        Some(Value::Null) => s.db.scripts.shown = None,
        _ => {
            let id = script_id(s, p)?;
            s.db.scripts.shown = Some(id);
            s.db.active_result = None;
            s.ws.layout.show(ToolWindowId::Services);
        }
    }
    ok()
}

fn set_script_mode(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let m = req_str(p, "mode")?;
    let mode = ScriptMode::parse(m).ok_or_else(|| bad(format!("unknown mode `{m}` (stop, continue, ask)")))?;
    s.db.nav.settings.script_mode = mode;
    s.db.nav.save().map_err(failed)?;
    s.status = format!("Run SQL file: {}", mode.label());
    Ok(json!({ "mode": mode }))
}

// ---------------------------------------------------------------- results

fn pin_result(s: &mut Session, p: &Value) -> CmdResult {
    let id = match opt_u64(p, "result")? {
        Some(id) => id,
        None => s.db.active_result.ok_or_else(|| bad("missing `result`"))?,
    };
    if s.db.result(id).is_none() {
        return Err(bad(format!("no result {id}")));
    }
    let pinned = p.get("pinned").and_then(Value::as_bool).unwrap_or(!s.db.nav.pinned.contains(&id));
    if pinned {
        s.db.nav.pinned.insert(id);
    } else {
        s.db.nav.pinned.remove(&id);
    }
    // Forget pins of closed tabs.
    let open: Vec<u64> = s.db.results.iter().map(|r| r.id).collect();
    s.db.nav.pinned.retain(|r| open.contains(r));
    Ok(json!({ "result": id, "pinned": pinned }))
}

// ---------------------------------------------------------------- explorer

fn goto_object(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    sql_nav::load_all(&mut s.db);
    let query = opt_str(p, "query").unwrap_or_default().to_string();
    s.db.nav.goto = Some(GotoObjectPopup { query, selected: 0 });
    let connected = s.db.sources.iter().filter(|d| matches!(d.status, ConnStatus::Connected(_))).count();
    if connected == 0 {
        s.status = "Connect a data source to search its objects".into();
    }
    Ok(json!({ "objects": sql_nav::objects(&s.db).len(), "connected": connected }))
}

fn ranked(s: &Session, query: &str, limit: usize) -> Vec<sql_nav::DbObject> {
    let objects = sql_nav::objects(&s.db);
    sql_nav::rank_objects(&objects, query, limit).into_iter().filter_map(|(i, _)| objects.get(i).cloned()).collect()
}

fn goto_object_results(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    sql_nav::load_all(&mut s.db);
    let query = opt_str(p, "query").map(str::to_string).or_else(|| s.db.nav.goto.as_ref().map(|g| g.query.clone()));
    let limit = opt_usize(p, "limit")?.unwrap_or(50).min(sql_nav::GOTO_LIMIT);
    let items = ranked(s, query.as_deref().unwrap_or_default(), limit);
    Ok(json!({ "objects": items }))
}

fn goto_object_accept(s: &mut Session, p: &Value) -> CmdResult {
    let popup = s.db.nav.goto.clone().unwrap_or_default();
    let index = opt_usize(p, "index")?.unwrap_or(popup.selected);
    let items = ranked(s, &popup.query, sql_nav::GOTO_LIMIT);
    let o = items.get(index).cloned().ok_or_else(|| bad(format!("no object {index}")))?;
    s.db.nav.goto = None;
    let open = opt_str(p, "open").unwrap_or("tree");
    match (o.kind, open) {
        (DbObjectKind::Schema, _) => {
            sql_nav::select_in_tree(s, &o.source, Some(&o.name), None);
            let _ = s.execute("db.expand", &json!({ "dataSource": o.source, "schema": o.name, "expanded": true }));
        }
        (DbObjectKind::Table | DbObjectKind::View, "data") => {
            return s.execute("db.openTable", &json!({ "dataSource": o.source, "schema": o.schema, "table": o.name }));
        }
        (DbObjectKind::Table | DbObjectKind::View, "ddl") => {
            return s.execute("sql.openDdl", &json!({ "dataSource": o.source, "schema": o.schema, "name": o.name }));
        }
        (DbObjectKind::Routine, "ddl" | "data") => {
            return s.execute(
                "sql.openDdl",
                &json!({ "dataSource": o.source, "schema": o.schema, "name": o.name, "routine": true }),
            );
        }
        (DbObjectKind::Routine, _) => {
            sql_nav::select_in_tree(s, &o.source, Some(&o.schema), None);
            s.db.expanded
                .insert(format!("{}\u{1f}#routines", crate::database::node_key(&o.source, Some(&o.schema), None)));
        }
        _ => sql_nav::select_in_tree(s, &o.source, Some(&o.schema), Some((&o.name, false))),
    }
    s.status = format!("{} {}", o.kind.label(), o.qualified());
    Ok(json!({ "object": o }))
}

fn introspect_schemas(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let Some(value) = p.get("schemas") else {
        let ds = s.db.source(&source).ok_or_else(|| bad(format!("no data source `{source}`")))?;
        let Some(info) = &ds.tree.info else {
            ensure_connected(s, &source)?;
            return Err(failed("connecting to list the schemas; try again in a moment"));
        };
        let all = info.schemas.clone();
        let chosen = s.db.nav.introspected(&source, &all).into_iter().collect();
        s.db.nav.schemas_dialog = Some(SchemasDialog { source: source.clone(), all: all.clone(), chosen });
        return Ok(json!({ "dialog": true, "schemas": all }));
    };
    let list: Vec<String> = match value {
        Value::Null => Vec::new(),
        Value::Array(a) => {
            a.iter().filter_map(Value::as_str).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
        }
        _ => return Err(bad("`schemas` must be a list of names or null")),
    };
    if list.is_empty() {
        s.db.nav.settings.introspect.remove(&source);
    } else {
        s.db.nav.settings.introspect.insert(source.clone(), list.clone());
    }
    s.db.nav.save().map_err(failed)?;
    s.db.nav.schemas_dialog = None;
    // Reload the schema list with the new choice.
    if s.db.source(&source).is_some_and(|d| matches!(d.status, ConnStatus::Connected(_))) {
        s.db.meta(&source, MetaReq::Info).map_err(failed)?;
    }
    s.status =
        if list.is_empty() { format!("{source}: all schemas") } else { format!("{source}: {} schema(s)", list.len()) };
    Ok(json!({ "dataSource": source, "schemas": list }))
}

fn refresh_node(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let connected = s.db.source(&source).is_some_and(|d| matches!(d.status, ConnStatus::Connected(_)));
    if !connected {
        ensure_connected(s, &source)?;
        return ok();
    }
    match (opt_str(p, "schema"), opt_str(p, "table")) {
        (None, _) => s.db.meta(&source, MetaReq::Info).map_err(failed)?,
        (Some(sc), None) => {
            s.db.meta(&source, MetaReq::Tables(sc.to_string())).map_err(failed)?;
            sql_nav::request_routines(&mut s.db, &source, sc, true);
        }
        (Some(sc), Some(t)) => {
            let kind =
                s.db.source(&source)
                    .and_then(|d| d.table(sc, t))
                    .map(|n| n.info.kind)
                    .ok_or_else(|| bad(format!("no table {sc}.{t} loaded")))?;
            s.db.meta(&source, MetaReq::Details(sc.to_string(), t.to_string())).map_err(failed)?;
            let what =
                if kind == TableKind::Table { sql_nav::ddl::InfoKind::Table } else { sql_nav::ddl::InfoKind::View };
            sql_nav::request_info(&mut s.db, &source, sc, t, what, true);
        }
    }
    ok()
}

fn routines(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = req_str(p, "schema")?.to_string();
    let connected = s.db.source(&source).is_some_and(|d| matches!(d.status, ConnStatus::Connected(_)));
    if !connected {
        ensure_connected(s, &source)?;
        return Ok(json!({ "loading": true }));
    }
    sql_nav::request_routines(&mut s.db, &source, &schema, false);
    let r = s.db.nav.routines_of(&source, &schema).cloned().unwrap_or_default();
    Ok(json!({ "loading": r.loading, "error": r.error, "routines": r.items }))
}
