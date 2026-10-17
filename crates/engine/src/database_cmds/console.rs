//! Console commands: Explain Plan, the parameter prompt, the production guard's confirmation,
//! manual-commit transactions (Commit/Rollback) and the query history with saved queries.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use codecraft_db::statement_at;
use codecraft_syntax::Language;

use super::{bad, console_worker, failed, run_console, save, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::exec::{ExecAction, submit};
use crate::database::{HistoryPopup, sync_project};
use crate::params::{bool_or, opt_str, opt_usize, req_str};
use crate::{CommandError, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "db.explainPlan",
            "Explain Plan",
            None,
            "{sql?, dataSource?, params?} — the statement at the caret (or `sql`): PostgreSQL EXPLAIN (FORMAT JSON), \
             SQLite EXPLAIN QUERY PLAN, MySQL EXPLAIN FORMAT=JSON, shown as a tree",
            Context::Global,
            always,
            |s, p| run_console(s, p, false, ExecAction::Explain { analyze: false }),
        ),
        spec(
            "db.explainAnalyze",
            "Explain Analyze",
            None,
            "{sql?, dataSource?, params?, confirm?} — runs the statement and shows actual times and rows \
             (PostgreSQL, MySQL, MariaDB)",
            Context::Global,
            always,
            |s, p| run_console(s, p, false, ExecAction::Explain { analyze: true }),
        ),
        spec(
            "db.submitParameters",
            "Run with Parameters",
            None,
            "{values?: {name: text}, kinds?: {name: value|text|sql}} — run what the parameter prompt holds (values \
             default to the prompt's fields). Values are bound: `value` reads NULL, integers, true/false and 'quoted' \
             text, `text` takes the text exactly, `sql` inserts an expression as typed",
            Context::Global,
            always,
            submit_parameters,
        ),
        spec("db.cancelParameters", "Cancel Parameters", None, "", Context::Global, always, |s, _| {
            s.db.param_prompt = None;
            s.status = "Cancelled".into();
            ok()
        }),
        spec(
            "db.confirmExecution",
            "Confirm Data Modification",
            None,
            "run the statements the production guard asked about",
            Context::Global,
            always,
            confirm_execution,
        ),
        spec("db.cancelExecution", "Cancel Data Modification", None, "", Context::Global, always, |s, _| {
            s.db.confirm = None;
            s.status = "Cancelled".into();
            ok()
        }),
        spec(
            "db.commit",
            "Commit",
            None,
            "{dataSource?, connection?} — commit the active console's transaction (or every open one of a data source)",
            Context::Global,
            always,
            |s, p| end_transaction(s, p, true),
        ),
        spec(
            "db.rollback",
            "Rollback",
            None,
            "{dataSource?, connection?} — roll back the active console's transaction",
            Context::Global,
            always,
            |s, p| end_transaction(s, p, false),
        ),
        spec(
            "db.setTransactionMode",
            "Transaction Mode",
            None,
            "{manual: bool, dataSource?} — manual commit: console statements run in a transaction that Commit or \
             Rollback ends; auto: every statement commits",
            Context::Global,
            always,
            set_transaction_mode,
        ),
        spec(
            "db.showHistory",
            "Query History",
            None,
            "{dataSource?} — the console's query history popup (insert or run a previous query)",
            Context::Global,
            always,
            show_history,
        ),
        spec("db.closeHistory", "Close Query History", None, "", Context::Global, always, |s, _| {
            s.db.history_popup = None;
            ok()
        }),
        spec(
            "db.getHistory",
            "Get Query History",
            None,
            "{dataSource?, filter?, limit?} — executed queries (newest first) and saved queries",
            Context::Global,
            always,
            get_history,
        ),
        spec(
            "db.useHistory",
            "Use Query from History",
            None,
            "{dataSource?, sql? | index? (newest first) | name? (a saved query), run?: bool} — insert into the console \
             (opening it if needed), and run it with `run`",
            Context::Global,
            always,
            use_history,
        ),
        spec("db.clearHistory", "Clear Query History", None, "{dataSource?}", Context::Global, always, clear_history),
        spec(
            "db.saveQuery",
            "Save Query",
            None,
            "{name, sql?, dataSource?} — save a query (default: the selection or statement at the caret)",
            Context::Global,
            always,
            save_query,
        ),
        spec(
            "db.removeSavedQuery",
            "Remove Saved Query",
            None,
            "{name, dataSource?}",
            Context::Global,
            always,
            remove_saved_query,
        ),
    ]
}

// ---------------------------------------------------------------- parameters and guard

fn submit_parameters(s: &mut Session, p: &Value) -> CmdResult {
    let mut prompt = s.db.param_prompt.clone().ok_or_else(|| CommandError::Disabled("no parameters to fill".into()))?;
    // `values` replace the fields' text (read with each field's kind); `kinds` change kinds.
    let text_of = |v: &Value| match v {
        Value::String(t) => Some(t.clone()),
        Value::Null => Some("NULL".into()),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        _ => None,
    };
    for f in &mut prompt.params {
        if let Some(v) = p.get("values").and_then(|m| m.get(&f.name)) {
            f.value = text_of(v).ok_or_else(|| bad(format!("the value of {} must be text", f.name)))?;
        }
        if let Some(k) = p.get("kinds").and_then(|m| m.get(&f.name)).and_then(Value::as_str) {
            f.kind = codecraft_db::bind::ParamKind::parse(k)
                .ok_or_else(|| bad(format!("unknown kind `{k}` (value, text or sql)")))?;
        }
    }
    // An empty Text value is the empty string; other kinds need something typed.
    if let Some(missing) = prompt
        .params
        .iter()
        .find(|f| f.kind != codecraft_db::bind::ParamKind::Text && f.value.trim().is_empty())
    {
        return Err(bad(format!("enter a value for {} (NULL for no value)", missing.name)));
    }
    let values: BTreeMap<String, codecraft_db::bind::ParamValue> =
        prompt.params.iter().map(|f| (f.name.clone(), f.to_value())).collect();
    s.db.param_prompt = None;
    let source = prompt.run.source.clone();
    let out = submit(s, prompt.run, Some(&values), false).map_err(failed)?;
    after_submit(s, &source, &out);
    Ok(out)
}

fn confirm_execution(s: &mut Session, _: &Value) -> CmdResult {
    let c = s.db.confirm.take().ok_or_else(|| CommandError::Disabled("nothing waits for confirmation".into()))?;
    let source = c.run.source.clone();
    let out = submit(s, c.run, Some(&c.values), true).map_err(failed)?;
    after_submit(s, &source, &out);
    Ok(out)
}

fn after_submit(s: &mut Session, source: &str, out: &Value) {
    if out.get("run").is_some() {
        s.ws.layout.show(codecraft_workspace::ToolWindowId::Services);
        s.status = format!("Executing on {source}…");
    }
}

// ---------------------------------------------------------------- transactions

/// The connection of the active SQL editor, if it is one.
fn active_console(s: &Session) -> Option<(String, String)> {
    let (_, d) = s.ws.active()?;
    if d.language != Language::Sql {
        return None;
    }
    let source = s.db.source_for_path(d.path.as_deref())?;
    Some((console_worker(d.path.as_deref(), d.id.0), source))
}

fn end_transaction(s: &mut Session, p: &Value, commit: bool) -> CmdResult {
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
    Ok(json!({ "connections": workers }))
}

fn set_transaction_mode(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = match opt_str(p, "dataSource") {
        Some(_) => target_source(s, p)?,
        None => match active_console(s) {
            Some((_, source)) => source,
            None => target_source(s, p)?,
        },
    };
    let manual = match p.get("manual").and_then(Value::as_bool) {
        Some(m) => m,
        None => !s.db.source(&id).is_some_and(|d| d.config.manual_commit),
    };
    if let Some(kind) = s.db.source(&id).map(|d| d.config.kind).filter(|k| manual && !k.supports_transactions()) {
        return Err(CommandError::Disabled(codecraft_db::tx::no_transactions(kind)));
    }
    if let Some(ds) = s.db.source_mut(&id) {
        ds.config.manual_commit = manual;
    }
    save(s)?;
    let open = s.db.open_transactions(Some(&id));
    s.status = if manual {
        "Manual commit: statements run in a transaction until Commit or Rollback".into()
    } else if open.is_empty() {
        "Auto-commit".into()
    } else {
        "Auto-commit (an open transaction remains until Commit or Rollback)".into()
    };
    Ok(json!({ "manual": manual, "openTransactions": open }))
}

// ---------------------------------------------------------------- history

/// The data source for history commands: the param, the active console's, the selected one.
fn history_source(s: &Session, p: &Value) -> Result<String, CommandError> {
    if opt_str(p, "dataSource").is_none()
        && let Some((_, source)) = active_console(s)
    {
        return Ok(source);
    }
    target_source(s, p)
}

fn show_history(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = history_source(s, p)?;
    let n = s.db.history.entries(&source, "").len();
    s.db.history_popup = Some(HistoryPopup { source: source.clone(), filter: String::new() });
    Ok(json!({ "dataSource": source, "entries": n }))
}

fn get_history(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = history_source(s, p)?;
    let limit = opt_usize(p, "limit")?.unwrap_or(100).min(crate::database::history::MAX_HISTORY);
    let entries: Vec<_> =
        s.db.history.entries(&source, opt_str(p, "filter").unwrap_or_default()).into_iter().take(limit).collect();
    let saved = s.db.history.saved(&source);
    Ok(json!({
        "dataSource": source,
        "entries": entries,
        "saved": saved,
        "file": s.db.history.file_path(&source),
        "error": s.db.history.error,
    }))
}

fn use_history(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = history_source(s, p)?;
    let sql = if let Some(sql) = opt_str(p, "sql") {
        sql.to_string()
    } else if let Some(name) = opt_str(p, "name") {
        s.db.history
            .saved(&source)
            .into_iter()
            .find(|q| q.name == name)
            .map(|q| q.sql)
            .ok_or_else(|| bad(format!("no saved query `{name}`")))?
    } else {
        let index = opt_usize(p, "index")?.ok_or_else(|| bad("pass `sql`, `index` or `name`"))?;
        s.db.history
            .entries(&source, "")
            .into_iter()
            .nth(index)
            .map(|e| e.sql)
            .ok_or_else(|| bad(format!("no entry {index}")))?
    };
    if sql.trim().is_empty() {
        return Err(bad("the query is empty"));
    }
    // Into the active console of this data source, else its own console.
    if active_console(s).is_none_or(|(_, src)| src != source) {
        s.execute("db.newConsole", &json!({ "dataSource": source }))?;
    }
    // A selection is replaced; otherwise the query becomes the console's last statement.
    let (at_end, prefix) = match s.ws.active() {
        Some((e, d)) if e.selections.primary().is_empty() => {
            let text = d.doc.text();
            let trimmed = text.trim_end();
            let prefix = if trimmed.is_empty() {
                ""
            } else if trimmed.ends_with(';') {
                "\n"
            } else {
                ";\n"
            };
            (Some(d.doc.len_chars()), prefix)
        }
        _ => (None, ""),
    };
    if let Some(end) = at_end {
        s.execute("editor.setSelection", &json!({ "anchor": end, "head": end }))?;
    }
    s.execute("editor.type", &json!({ "text": format!("{prefix}{sql}") }))?;
    s.db.history_popup = None;
    if bool_or(p, "run", false) {
        return s.execute("db.execute", &json!({ "dataSource": source }));
    }
    ok()
}

fn clear_history(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = history_source(s, p)?;
    let n = s.db.history.clear(&source);
    s.status = format!("Cleared {n} history entr{}", if n == 1 { "y" } else { "ies" });
    Ok(json!({ "cleared": n }))
}

/// The selection or the statement at the caret of the active SQL editor.
fn statement_under_caret(s: &Session) -> Option<String> {
    let (e, d) = s.ws.active()?;
    if d.language != Language::Sql {
        return None;
    }
    let sel = e.selections.primary();
    if !sel.is_empty() {
        let r = sel.range();
        return Some(d.doc.slice(r.start..r.end.min(d.doc.len_chars())));
    }
    let source = s.db.source_for_path(d.path.as_deref());
    let dialect = source.and_then(|id| s.db.source(&id)).map(|ds| ds.config.kind.dialect()).unwrap_or_default();
    statement_at(&d.doc.text(), dialect, sel.head).map(|st| st.text)
}

fn save_query(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let name = req_str(p, "name")?.trim().to_string();
    if name.is_empty() {
        return Err(bad("`name` is empty"));
    }
    let source = history_source(s, p)?;
    let sql = match opt_str(p, "sql") {
        Some(sql) => sql.to_string(),
        None => statement_under_caret(s).ok_or_else(|| bad("no query: pass `sql` or put the caret in one"))?,
    };
    if sql.trim().is_empty() {
        return Err(bad("the query is empty"));
    }
    s.db.history.save_query(&source, &name, &sql);
    s.status = format!("Saved query {name}");
    Ok(json!({ "name": name, "dataSource": source }))
}

fn remove_saved_query(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let name = req_str(p, "name")?;
    let source = history_source(s, p)?;
    if !s.db.history.remove_saved(&source, name) {
        return Err(bad(format!("no saved query `{name}`")));
    }
    ok()
}
