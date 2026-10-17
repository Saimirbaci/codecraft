//! Track DB commands: transactions (mode, isolation, Commit/Rollback, pending transactions and
//! the pending-transaction guard), query timeout and keep-alive, and the data tools (Compare
//! Data, Dump, Restore, Export to File, Full-text Search) with their jobs. OWNED BY the
//! database track.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_db::export::ExportFormat;
use codecraft_db::tx::Isolation;
use codecraft_syntax::Language;

use super::{bad, console_worker, failed, opt_u64, save, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::data_tools::{DataDialog, JobResult, SideSpec, Tool, hit_filter};
use crate::database::tx::{Choice, LinkReq};
use crate::database::{ConnStatus, sync_project};
use crate::params::{bool_or, opt_str, opt_usize};
use crate::{CommandError, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "db.tx.commit",
            "Commit Transaction",
            None,
            "{connection?, dataSource?} — the active console's transaction, a connection's, or every open one of a \
             data source; a failed transaction is rolled back and reported, never committed",
            Context::Global,
            always,
            |s, p| end(s, p, true),
        ),
        spec(
            "db.tx.rollback",
            "Rollback Transaction",
            None,
            "{connection?, dataSource?} — like db.tx.commit",
            Context::Global,
            always,
            |s, p| end(s, p, false),
        ),
        spec(
            "db.tx.setMode",
            "Transaction Mode",
            None,
            "{mode: auto|manual, connection?, dataSource?} — the active console's (or `connection`'s) mode; with only \
             `dataSource` the data source's default (saved). Switching to auto leaves an open transaction open until \
             Commit or Rollback",
            Context::Global,
            always,
            set_mode,
        ),
        spec(
            "db.tx.setIsolation",
            "Transaction Isolation",
            None,
            "{level: default|read-uncommitted|read-committed|repeatable-read|serializable, connection?, dataSource?} \
             — the console's (or the data source's, saved) isolation level; applies from the next transaction",
            Context::Global,
            always,
            set_isolation,
        ),
        spec(
            "db.tx.pending",
            "Pending Transactions",
            None,
            "{dataSource?} — open transactions (connection, statements, duration, failed), lost connections and the \
             pending-transaction question if one is asked",
            Context::Global,
            always,
            pending,
        ),
        spec(
            "db.tx.resolve",
            "Resolve Pending Transactions",
            None,
            "{choice: commit|rollback|cancel} — answer the pending-transaction question (disconnect, close console, \
             close project, quit); the waiting action runs once every transaction ended. Guarded commands also \
             accept {transactions: commit|rollback} up front",
            Context::Global,
            always,
            resolve,
        ),
        spec(
            "db.setQueryTimeout",
            "Query Timeout",
            None,
            "{seconds: number (0: none), dataSource?} — statements running longer are cancelled (saved; applies to \
             open connections)",
            Context::Global,
            always,
            set_timeout,
        ),
        spec(
            "db.setKeepAlive",
            "Connection Keep-Alive",
            None,
            "{seconds: number (0: off), dataSource?} — ping idle connections this often (default 60; through SSH \
             tunnels too); applies to new connections",
            Context::Global,
            always,
            set_keepalive,
        ),
        spec(
            "db.compareData",
            "Compare Data",
            None,
            "{left: {dataSource, schema?, table? | sql?}, right: {…}, keys?: [column]} — rows only left, only right \
             and different (by key; default: the left table's primary key); both sides stream in key order",
            Context::Global,
            always,
            compare_data,
        ),
        spec(
            "db.compareData.syncScript",
            "Compare Data: Sync Script",
            None,
            "{job?, path?, copy?: bool, run?: bool} — statements that make the right table equal to the left: returned, \
             written to `path`, copied, or run on the right data source (through the production guard) with `run`",
            Context::Global,
            always,
            sync_script,
        ),
        spec(
            "db.dump",
            "Dump Data Source",
            None,
            "{dataSource?, path, schemas?: [..], tables?: [..], dataOnly?, schemaOnly?, format?: plain|custom, tool?} \
             — pg_dump / mysqldump (PATH, `tool`, or the option <tool>_path; password through the environment), SQLite \
             natively; written to <path>.partial and renamed when complete",
            Context::Global,
            always,
            dump,
        ),
        spec(
            "db.restore",
            "Restore from Dump",
            None,
            "{dataSource?, path, tool?, confirm?: bool} — pg_restore (custom archives) or psql, mysql, SQLite natively; \
             one transaction, stops at the first error; production data sources need `confirm`",
            Context::Global,
            always,
            restore,
        ),
        spec(
            "db.exportTable",
            "Export Data to File",
            None,
            "{dataSource?, schema?, table? | sql?, path, format: csv|tsv|json|sql|markdown} — the whole table or query, \
             streamed page by page (not limited to the grid)",
            Context::Global,
            always,
            export_table,
        ),
        spec(
            "db.fullTextSearch",
            "Full-text Search",
            None,
            "{dataSource?, schema?, text, tables?: [..], limit?: rows per table (50), caseSensitive?} — text columns of \
             every table of the schema, one bounded query per table; results grouped by table",
            Context::Global,
            always,
            full_text_search,
        ),
        spec(
            "db.fullTextSearch.open",
            "Open Search Hit",
            None,
            "{job?, table: index, row: index} — open the table filtered to the row",
            Context::Global,
            always,
            open_hit,
        ),
        spec(
            "db.job.cancel",
            "Cancel Data Job",
            None,
            "{job?} — the active (or given) running job",
            Context::Global,
            always,
            cancel_job,
        ),
        spec("db.job.list", "List Data Jobs", None, "", Context::Global, always, |s, _| {
            let jobs: Vec<Value> = s
                .db
                .data
                .jobs
                .iter()
                .map(|j| json!({ "id": j.id, "kind": j.kind, "title": j.title, "state": j.state, "progress": j.progress_text() }))
                .collect();
            Ok(json!({ "jobs": jobs }))
        }),
        spec(
            "db.job.get",
            "Get Data Job",
            None,
            "{job?} — state, progress, log and result",
            Context::Global,
            always,
            |s, p| {
                let id = job_id(s, p)?;
                let j = s.db.data.job(id).ok_or_else(|| bad(format!("no job {id}")))?;
                Ok(serde_json::to_value(j).unwrap_or(Value::Null))
            },
        ),
        spec("db.job.select", "Show Data Job", None, "{job}", Context::Global, always, |s, p| {
            let id = job_id(s, p)?;
            if s.db.data.job(id).is_none() {
                return Err(bad(format!("no job {id}")));
            }
            s.db.data.active = Some(id);
            s.db.data.open = true;
            ok()
        }),
        spec("db.job.close", "Close Data Job", None, "{job?}", Context::Global, always, |s, p| {
            let id = job_id(s, p)?;
            if s.db.data.job(id).is_some_and(|j| j.is_running()) {
                return Err(CommandError::Disabled("the job is running: cancel it first".into()));
            }
            s.db.data.jobs.retain(|j| j.id != id);
            if s.db.data.active == Some(id) {
                s.db.data.active = s.db.data.jobs.last().map(|j| j.id);
            }
            ok()
        }),
        spec(
            "db.dataTools.open",
            "Data Tools",
            None,
            "{tool?: compare|dump|restore|export|search, dataSource?, schema?, table?} — the Data Tools window with \
             the tool's form",
            Context::Global,
            always,
            open_tools,
        ),
        spec(
            "db.dataTools.run",
            "Run Data Tool",
            None,
            "run what the Data Tools form holds",
            Context::Global,
            always,
            run_tool,
        ),
        spec("db.dataTools.close", "Close Data Tools", None, "", Context::Global, always, |s, _| {
            s.db.data.open = false;
            s.db.data.dialog = None;
            ok()
        }),
    ]
}

// ---------------------------------------------------------------- transactions

/// The active SQL console's connection and data source.
fn active_console(s: &Session) -> Option<(String, String)> {
    let (_, d) = s.ws.active()?;
    if d.language != Language::Sql {
        return None;
    }
    let source = s.db.source_for_path(d.path.as_deref())?;
    Some((console_worker(d.path.as_deref(), d.id.0), source))
}

/// The connection a command acts on (`connection`, else the active console) and its source.
fn connection(s: &Session, p: &Value) -> Option<(String, String)> {
    if let Some(w) = opt_str(p, "connection") {
        let source = s
            .db
            .txs
            .conns
            .get(w)
            .map(|c| c.source.clone())
            .filter(|x| !x.is_empty())
            .or_else(|| s.db.pending_transactions(None).into_iter().find(|t| t.connection == w).map(|t| t.data_source))
            .or_else(|| w.strip_prefix("console:").and_then(|_| active_console(s).map(|(_, src)| src)))
            .unwrap_or_default();
        return Some((w.to_string(), source));
    }
    if opt_str(p, "dataSource").is_some() {
        return None;
    }
    active_console(s)
}

fn end(s: &mut Session, p: &Value, commit: bool) -> CmdResult {
    sync_project(s);
    let workers: Vec<String> = if let Some(w) = opt_str(p, "connection") {
        vec![w.to_string()]
    } else if opt_str(p, "dataSource").is_some() {
        let id = target_source(s, p)?;
        s.db.open_transactions(Some(&id))
    } else {
        match active_console(s).filter(|(w, _)| s.db.transaction_open(w)) {
            Some((w, _)) => vec![w],
            None => {
                let open = s.db.open_transactions(None);
                if open.len() > 1 {
                    return Err(bad(format!("several transactions are open; pass `connection` ({})", open.join(", "))));
                }
                open
            }
        }
    };
    let workers: Vec<String> = workers.into_iter().filter(|w| s.db.transaction_open(w)).collect();
    if workers.is_empty() {
        return Err(CommandError::Disabled("no transaction is open".into()));
    }
    for w in &workers {
        s.db.end_transaction(w, commit).map_err(failed)?;
    }
    s.status = if commit { "Committing…".into() } else { "Rolling back…".into() };
    Ok(json!({ "connections": workers, "commit": commit }))
}

fn set_mode(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let manual = match opt_str(p, "mode").map(str::to_ascii_lowercase).as_deref() {
        Some("manual") => true,
        Some("auto") => false,
        Some(other) => return Err(bad(format!("`mode` is auto or manual, not `{other}`"))),
        None => match p.get("manual").and_then(Value::as_bool) {
            Some(m) => m,
            None => return Err(bad("missing `mode` (auto|manual)")),
        },
    };
    let (scope, source) = match connection(s, p) {
        Some((w, source)) => {
            s.db.txs.consoles.entry(w.clone()).or_default().manual = Some(manual);
            (w, source)
        }
        None => {
            let id = target_source(s, p)?;
            if let Some(ds) = s.db.source_mut(&id) {
                ds.config.manual_commit = manual;
            }
            save(s)?;
            (id.clone(), id)
        }
    };
    let open = s.db.open_transactions(Some(&source));
    s.status = match (manual, open.is_empty()) {
        (true, _) => "Manual commit: statements run in a transaction until Commit or Rollback".into(),
        (false, true) => "Auto-commit".into(),
        (false, false) => "Auto-commit (an open transaction stays open until Commit or Rollback)".into(),
    };
    Ok(json!({ "manual": manual, "scope": scope, "openTransactions": open }))
}

fn set_isolation(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let raw = opt_str(p, "level").ok_or_else(|| bad("missing `level`"))?;
    let level = match raw.trim().to_ascii_lowercase().as_str() {
        "" | "default" | "none" => None,
        other => Some(Isolation::parse(other).ok_or_else(|| bad(format!("unknown isolation level `{raw}`")))?),
    };
    let target = connection(s, p);
    let source = match &target {
        Some((_, src)) if !src.is_empty() => src.clone(),
        _ => target_source(s, p)?,
    };
    let kind = s.db.source(&source).map(|d| d.config.kind).unwrap_or_default();
    if let Some(l) = level
        && !Isolation::supported(kind).contains(&l)
    {
        return Err(failed(format!("{} does not support {}", kind.label(), l.label())));
    }
    let workers: Vec<String> = match &target {
        Some((w, _)) => {
            s.db.txs.consoles.entry(w.clone()).or_default().isolation = level;
            vec![w.clone()]
        }
        None => {
            if let Some(ds) = s.db.source_mut(&source) {
                match level {
                    Some(l) => {
                        ds.config.options.insert("isolation".into(), l.id().into());
                    }
                    None => {
                        ds.config.options.remove("isolation");
                    }
                }
            }
            save(s)?;
            // Every connection of the source without its own choice follows.
            let all: Vec<String> = s.db.connections_of(&source);
            all.into_iter().filter(|w| s.db.txs.console(w).isolation.is_none()).collect()
        }
    };
    for w in &workers {
        let effective = s.db.isolation(w, &source);
        s.db.send_link(w, LinkReq::SetIsolation(effective)).map_err(failed)?;
    }
    let label = level.map(Isolation::label).unwrap_or("Default");
    s.status = format!("Isolation level: {label} (from the next transaction)");
    Ok(json!({ "level": level, "connections": workers }))
}

fn pending(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = match opt_str(p, "dataSource") {
        Some(_) => Some(target_source(s, p)?),
        None => None,
    };
    let open = s.db.pending_transactions(source.as_deref());
    let lost: Vec<Value> =
        s.db.txs
            .conns
            .iter()
            .filter(|(_, c)| c.lost.is_some() && source.as_ref().is_none_or(|x| &c.source == x))
            .map(|(w, c)| json!({ "connection": w, "dataSource": c.source, "message": c.lost }))
            .collect();
    Ok(json!({ "transactions": open, "lostConnections": lost, "guard": s.db.txs.guard }))
}

fn resolve(s: &mut Session, p: &Value) -> CmdResult {
    let raw = opt_str(p, "choice").ok_or_else(|| bad("missing `choice` (commit|rollback|cancel)"))?;
    let choice =
        Choice::parse(raw).ok_or_else(|| bad(format!("`choice` is commit, rollback or cancel, not `{raw}`")))?;
    crate::database::tx::resolve(s, choice)
}

fn source_param(s: &mut Session, p: &Value) -> Result<String, CommandError> {
    sync_project(s);
    match opt_str(p, "dataSource") {
        Some(_) => target_source(s, p),
        None => match active_console(s) {
            Some((_, src)) => Ok(src),
            None => target_source(s, p),
        },
    }
}

fn seconds(p: &Value) -> Result<f64, CommandError> {
    let v = p.get("seconds").and_then(Value::as_f64).ok_or_else(|| bad("missing `seconds`"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(bad("`seconds` must be 0 or more"));
    }
    Ok(v)
}

fn set_option(s: &mut Session, source: &str, key: &str, value: Option<String>) -> Result<(), CommandError> {
    if let Some(ds) = s.db.source_mut(source) {
        match value {
            Some(v) => {
                ds.config.options.insert(key.into(), v);
            }
            None => {
                ds.config.options.remove(key);
            }
        }
    }
    save(s)
}

fn set_timeout(s: &mut Session, p: &Value) -> CmdResult {
    let source = source_param(s, p)?;
    let secs = seconds(p)?;
    set_option(s, &source, "query_timeout", (secs > 0.0).then(|| secs.to_string()))?;
    let t = (secs > 0.0).then(|| std::time::Duration::from_secs_f64(secs.min(604_800.0)));
    for w in s.db.connections_of(&source) {
        let _ = s.db.send_link(&w, LinkReq::SetTimeout(t));
    }
    s.status = if secs > 0.0 { format!("Query timeout: {secs} s") } else { "No query timeout".into() };
    Ok(json!({ "seconds": secs }))
}

fn set_keepalive(s: &mut Session, p: &Value) -> CmdResult {
    let source = source_param(s, p)?;
    let secs = seconds(p)?;
    set_option(s, &source, "keepalive", Some(if secs > 0.0 { secs.to_string() } else { "0".into() }))?;
    s.status = if secs > 0.0 {
        format!("Keep-alive every {secs} s (new connections)")
    } else {
        "Keep-alive off (new connections)".into()
    };
    Ok(json!({ "seconds": secs }))
}

// ---------------------------------------------------------------- data tools

fn abs_path(s: &Session, p: PathBuf) -> PathBuf {
    if p.is_relative()
        && let Some(root) = s.db.project_root()
    {
        return root.join(p);
    }
    p
}

fn side(s: &Session, v: Option<&Value>, name: &str) -> Result<SideSpec, CommandError> {
    let v = v.ok_or_else(|| bad(format!("missing `{name}`")))?;
    let mut spec: SideSpec = serde_json::from_value(v.clone()).map_err(|e| bad(format!("`{name}`: {e}")))?;
    if spec.data_source.is_empty() {
        return Err(bad(format!("`{name}.dataSource` is required")));
    }
    if s.db.source(&spec.data_source).is_none() {
        return Err(bad(format!("no data source `{}`", spec.data_source)));
    }
    if spec.schema.is_empty() && spec.sql.trim().is_empty() {
        spec.schema = s.db.default_schema(&spec.data_source).unwrap_or_default();
    }
    Ok(spec)
}

fn strings(p: &Value, key: &str) -> Vec<String> {
    match p.get(key) {
        Some(Value::Array(a)) => {
            a.iter().filter_map(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
        }
        Some(Value::String(s)) => s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn compare_data(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let left = side(s, p.get("left"), "left")?;
    let right = side(s, p.get("right"), "right")?;
    let keys = strings(p, "keys");
    let job = s.db.start_compare(left, right, keys).map_err(failed)?;
    s.status = "Comparing data…".into();
    Ok(json!({ "job": job }))
}

fn job_id(s: &Session, p: &Value) -> Result<u64, CommandError> {
    match opt_u64(p, "job")? {
        Some(id) => Ok(id),
        None => s.db.data.active.ok_or_else(|| bad("missing `job`")),
    }
}

fn sync_script(s: &mut Session, p: &Value) -> CmdResult {
    let id = job_id(s, p)?;
    let j = s.db.data.job(id).ok_or_else(|| bad(format!("no job {id}")))?;
    let Some(JobResult::Compare(c)) = &j.result else {
        return Err(CommandError::Disabled("the job is not a finished comparison".into()));
    };
    if !c.right.is_table() {
        return Err(CommandError::Disabled("the right side is a query: a sync script needs a table".into()));
    }
    let statements = codecraft_db::compare::sync_script(c.right_kind, &c.right.schema, &c.right.table, &c.result)
        .map_err(|e| failed(e.to_string()))?;
    // What running it executes: the same statements with bound values.
    let bound = codecraft_db::compare::sync_bound(c.right_kind, &c.right.schema, &c.right.table, &c.result)
        .map_err(|e| failed(e.to_string()))?;
    let source = c.right.data_source.clone();
    let script = statements.iter().map(|st| format!("{st};\n")).collect::<String>();
    if let Some(path) = crate::params::opt_path(p, "path") {
        let path = abs_path(s, path);
        std::fs::write(&path, &script).map_err(|e| failed(format!("{}: {e}", path.display())))?;
        s.status = format!("Sync script ({} statements) written to {}", statements.len(), path.display());
    }
    if bool_or(p, "copy", false) {
        s.effects.push(crate::Effect::CopyToClipboard(script.clone()));
        s.status = format!("Sync script copied ({} statements)", statements.len());
    }
    if bool_or(p, "run", false) {
        if statements.is_empty() {
            return Ok(json!({ "statements": 0, "identical": true }));
        }
        // Running binds the values (the script written or copied above has literals); the
        // read-only and production guards apply as for any statement.
        crate::database::sync_project(s);
        let (sqls, params): (Vec<String>, Vec<Vec<codecraft_db::Cell>>) =
            bound.into_iter().map(|b| (b.sql, b.params)).unzip();
        let mut run = crate::database::exec::PendingRun::new(
            crate::database::exec::ExecAction::Execute,
            source.clone(),
            format!("query:{source}"),
            "Sync".into(),
            sqls,
        );
        run.bound = params;
        let out = crate::database::exec::submit(s, run, None, bool_or(p, "confirm", false)).map_err(failed)?;
        if out.get("run").is_some() {
            s.status = format!("Running the sync script ({} statements)…", statements.len());
        }
        return Ok(out);
    }
    Ok(json!({ "statements": statements, "script": script, "dataSource": source }))
}

fn dump(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let path = abs_path(s, crate::params::req_path(p, "path")?);
    let opts = codecraft_db::dump::DumpOptions {
        schemas: strings(p, "schemas"),
        tables: strings(p, "tables"),
        data_only: bool_or(p, "dataOnly", false),
        schema_only: bool_or(p, "schemaOnly", false),
        custom_format: opt_str(p, "format").is_some_and(|f| f.eq_ignore_ascii_case("custom")),
    };
    let tool = opt_str(p, "tool").map(str::to_string);
    let job = s.db.start_dump(&source, path, opts, tool).map_err(failed)?;
    s.status = "Dumping…".into();
    Ok(json!({ "job": job }))
}

fn restore(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let path = abs_path(s, crate::params::req_path(p, "path")?);
    let production = s.db.source(&source).is_some_and(|d| d.config.confirm_writes);
    if production && !bool_or(p, "confirm", false) {
        let name = s.db.source(&source).map(|d| d.config.name.clone()).unwrap_or_default();
        return Err(CommandError::Disabled(format!(
            "{name} is a production data source: restoring modifies it; pass `confirm: true`"
        )));
    }
    let tool = opt_str(p, "tool").map(str::to_string);
    let job = s.db.start_restore(&source, path, tool).map_err(failed)?;
    s.status = "Restoring…".into();
    Ok(json!({ "job": job }))
}

fn export_table(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let path = abs_path(s, crate::params::req_path(p, "path")?);
    let format = match opt_str(p, "format") {
        Some(f) => ExportFormat::parse(f).ok_or_else(|| bad(format!("unknown format `{f}`")))?,
        None => path.extension().and_then(|e| ExportFormat::parse(&e.to_string_lossy())).unwrap_or(ExportFormat::Csv),
    };
    let mut side = SideSpec {
        data_source: source.clone(),
        schema: opt_str(p, "schema").unwrap_or_default().to_string(),
        table: opt_str(p, "table").unwrap_or_default().to_string(),
        sql: opt_str(p, "sql").unwrap_or_default().to_string(),
    };
    if side.schema.is_empty() && side.sql.is_empty() {
        side.schema = s.db.default_schema(&source).unwrap_or_default();
    }
    let job = s.db.start_export(&source, side, path, format).map_err(failed)?;
    s.status = "Exporting…".into();
    Ok(json!({ "job": job }))
}

fn full_text_search(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let text = opt_str(p, "text").ok_or_else(|| bad("missing `text`"))?.to_string();
    let schema = opt_str(p, "schema").map(str::to_string).or_else(|| s.db.default_schema(&source)).unwrap_or_default();
    let limit = opt_usize(p, "limit")?.unwrap_or(crate::database::data_tools::DEFAULT_SEARCH_LIMIT);
    let job =
        s.db.start_search(&source, schema, strings(p, "tables"), text, bool_or(p, "caseSensitive", false), limit)
            .map_err(failed)?;
    s.status = "Searching…".into();
    Ok(json!({ "job": job }))
}

fn open_hit(s: &mut Session, p: &Value) -> CmdResult {
    let id = job_id(s, p)?;
    let ti = crate::params::req_usize(p, "table")?;
    let ri = opt_usize(p, "row")?;
    let j = s.db.data.job(id).ok_or_else(|| bad(format!("no job {id}")))?;
    let Some(JobResult::Search { tables, .. }) = &j.result else {
        return Err(CommandError::Disabled("the job is not a finished search".into()));
    };
    let t = tables.get(ti).ok_or_else(|| bad(format!("no table {ti}")))?;
    let kind = s.db.source(&j.source).map(|d| d.config.kind).unwrap_or_default();
    let filter = match ri {
        Some(r) => {
            let row = t.rows.get(r).ok_or_else(|| bad(format!("no row {r}")))?;
            hit_filter(kind, t, row).unwrap_or_default()
        }
        None => String::new(),
    };
    let params = json!({ "dataSource": j.source, "schema": t.schema, "table": t.table, "where": filter });
    s.execute("db.openTable", &params)
}

fn cancel_job(s: &mut Session, p: &Value) -> CmdResult {
    let id = match opt_u64(p, "job")? {
        Some(id) => id,
        None => {
            s.db.data
                .jobs
                .iter()
                .rev()
                .find(|j| j.is_running())
                .map(|j| j.id)
                .ok_or_else(|| CommandError::Disabled("no job is running".into()))?
        }
    };
    s.db.cancel_job(id).map_err(failed)?;
    s.status = "Cancelling…".into();
    Ok(json!({ "job": id }))
}

fn open_tools(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let tool = match opt_str(p, "tool") {
        Some(t) => Tool::parse(t).ok_or_else(|| bad(format!("unknown tool `{t}`")))?,
        None => Tool::default(),
    };
    let source = target_source(s, p).unwrap_or_default();
    let schema = opt_str(p, "schema").map(str::to_string).or_else(|| s.db.default_schema(&source)).unwrap_or_default();
    let table = opt_str(p, "table").unwrap_or_default().to_string();
    let left =
        SideSpec { data_source: source.clone(), schema: schema.clone(), table: table.clone(), sql: String::new() };
    let ext = match tool {
        Tool::Dump => match s.db.source(&source).map(|d| d.config.kind) {
            Some(codecraft_db::DbKind::Postgres) => "dump.sql",
            _ => "dump.sql",
        },
        Tool::Export => "csv",
        _ => "",
    };
    let base = if table.is_empty() { source.clone() } else { table.clone() };
    let path = match (s.db.project_root(), ext.is_empty()) {
        (Some(root), false) => root.join(format!("{base}.{ext}")).display().to_string(),
        _ => String::new(),
    };
    s.db.data.dialog = Some(DataDialog {
        tool,
        left: left.clone(),
        right: left,
        path,
        format: "csv".into(),
        limit: crate::database::data_tools::DEFAULT_SEARCH_LIMIT.to_string(),
        ..DataDialog::default()
    });
    s.db.data.open = true;
    if !source.is_empty()
        && s.db.source(&source).is_some_and(|d| matches!(d.status, ConnStatus::Disconnected | ConnStatus::Failed(_)))
    {
        let _ = s.execute("db.connect", &json!({ "dataSource": source }));
    }
    ok()
}

fn run_tool(s: &mut Session, _: &Value) -> CmdResult {
    let d = s.db.data.dialog.clone().ok_or_else(|| CommandError::Disabled("the Data Tools form is closed".into()))?;
    let ds = d.left.data_source.clone();
    let r = match d.tool {
        Tool::Compare => s.execute(
            "db.compareData",
            &json!({ "left": d.left, "right": d.right, "keys": d.keys }),
        ),
        Tool::Dump => s.execute(
            "db.dump",
            &json!({
                "dataSource": ds, "path": d.path, "tables": d.dump.tables, "schemas": d.dump.schemas,
                "dataOnly": d.dump.data_only, "schemaOnly": d.dump.schema_only,
                "format": if d.dump.custom_format { "custom" } else { "plain" },
            }),
        ),
        Tool::Restore => s.execute("db.restore", &json!({ "dataSource": ds, "path": d.path, "confirm": true })),
        Tool::Export => s.execute(
            "db.exportTable",
            &json!({ "dataSource": ds, "schema": d.left.schema, "table": d.left.table, "sql": d.left.sql,
                     "path": d.path, "format": d.format }),
        ),
        Tool::Search => s.execute(
            "db.fullTextSearch",
            &json!({ "dataSource": ds, "schema": d.left.schema, "text": d.text, "caseSensitive": d.case_sensitive,
                     "limit": d.limit.trim().parse::<usize>().unwrap_or(crate::database::data_tools::DEFAULT_SEARCH_LIMIT) }),
        ),
    }?;
    Ok(r)
}
