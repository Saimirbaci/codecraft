//! SQL language commands: injected SQL, formatting (M11 track Q).

use serde_json::{Value, json};

use codecraft_syntax::sql_format::{self, KeywordCase};
use codecraft_syntax::sql_inject;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, spec};
use crate::params::{opt_path, opt_str, opt_usize, req_str};
use crate::sql_lang::{self, SqlDialect};
use crate::{CommandError, Session};

fn in_sql(s: &Session) -> Result<(), String> {
    if sql_lang::in_injection(s) { Ok(()) } else { Err("the caret is not in SQL inside a string".into()) }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "sql.injections",
            "List SQL in String Literals",
            None,
            "{path?: string} — the string literals of a Rust file (default: the active editor's) that hold SQL: content range (chars, and line/column), API, raw or not, the SQL text",
            Context::Global,
            always,
            injections,
        ),
        spec(
            "sql.reformatInjected",
            "Reformat SQL",
            None,
            "{offset?: int} — formats the SQL string literal at the caret (or char offset) in place, keeping the literal's indentation",
            Context::Editor,
            in_sql,
            |s, p| {
                let changed = sql_lang::reformat_injected(s, opt_usize(p, "offset")?)?;
                Ok(json!({ "changed": changed }))
            },
        ),
        spec(
            "sql.complete",
            "SQL Completion in String",
            None,
            "completion in the SQL string at the caret: keywords, tables and alias-aware columns of the data source for injected SQL",
            Context::Editor,
            in_sql,
            |s, _| sql_lang::complete(s, false),
        ),
        spec(
            "sql.chooseDataSource",
            "SQL Data Source for Injected SQL…",
            None,
            "{dataSource?: string|null} — the data source SQL in Rust strings is completed and checked against (null: automatic, the first connected one matching DATABASE_URL in .env, else the only connected one); without the param a chooser opens. Saved in .codecraft/sql.json",
            Context::Global,
            has_project,
            choose_data_source,
        ),
        spec(
            "sql.settings",
            "SQL Settings",
            None,
            "{dialect?: \"generic\"|\"sqlite\"|\"postgres\"|\"mysql\"|null, keywordCase?: \"upper\"|\"lower\"|\"preserve\", indent?: int} — changes and returns the project's SQL settings, the resolved data source and dialect for injected SQL",
            Context::Global,
            always,
            settings,
        ),
        spec(
            "sql.formatText",
            "Format SQL Text",
            None,
            "{text: string, keywordCase?: \"upper\"|\"lower\"|\"preserve\", indent?: int} — returns {text} formatted with the project's SQL code style (or the given options)",
            Context::Global,
            always,
            format_text,
        ),
        spec(
            "sql.problems",
            "Check SQL Names",
            None,
            "unresolved tables and columns in the active editor's SQL (injected SQL in Rust, or an .sql file) against its data source, checked now; empty while the schema is unknown",
            Context::Global,
            has_editor,
            |s, _| {
                let problems = sql_lang::check_active(s)?;
                Ok(json!({ "problems": problems }))
            },
        ),
    ]
}

fn injections(s: &mut Session, p: &Value) -> CmdResult {
    let path = opt_path(p, "path");
    let (text, did) = match &path {
        Some(path) => {
            let path = crate::normalize_path(path);
            match s.ws.doc_by_path(&path) {
                Some(id) => (None, Some(id)),
                None => {
                    (Some(codecraft_fs::read_text_file(&path).map_err(|e| CommandError::Failed(e.to_string()))?), None)
                }
            }
        }
        None => {
            let (_, did) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
            (None, Some(did))
        }
    };
    let (found, doc) = match (text, did) {
        (Some(text), _) => (sql_inject::find_injections(&text), codecraft_text::Document::from_text(&text)),
        (None, Some(did)) => {
            let found = sql_lang::refresh(s, did).map(|v| v.as_ref().clone()).unwrap_or_default();
            let doc = s.ws.doc(did).map(|d| d.doc.snapshot()).unwrap_or_default();
            (found, doc)
        }
        (None, None) => (Vec::new(), codecraft_text::Document::default()),
    };
    let list: Vec<Value> = found
        .iter()
        .map(|i| {
            let (a, b) = (doc.offset_to_position(i.start), doc.offset_to_position(i.end));
            json!({
                "start": i.start,
                "end": i.end,
                "line": a.line,
                "column": a.column,
                "endLine": b.line,
                "endColumn": b.column,
                "api": i.api,
                "raw": i.raw,
                "sql": i.sql,
            })
        })
        .collect();
    Ok(json!({ "injections": list }))
}

fn choose_data_source(s: &mut Session, p: &Value) -> CmdResult {
    sql_lang::load_settings(s, false);
    let Some(value) = p.get("dataSource") else {
        sql_lang::chooser(s);
        return Ok(json!({ "dataSource": s.sql_lang.settings.data_source, "chooser": true }));
    };
    let id = match value {
        Value::Null => None,
        Value::String(id) if id.trim().is_empty() => None,
        Value::String(id) => {
            if s.db.source(id).is_none() {
                return Err(CommandError::BadParams(format!("no data source `{id}`")));
            }
            Some(id.clone())
        }
        _ => return Err(CommandError::BadParams("`dataSource` must be a string or null".into())),
    };
    s.sql_lang.settings.data_source = id;
    sql_lang::save_settings(s)?;
    let resolved = sql_lang::injected_source(s);
    s.status = match &resolved {
        Some(id) => format!("Injected SQL uses data source {id}"),
        None => "Injected SQL: no connected data source".into(),
    };
    Ok(json!({ "dataSource": s.sql_lang.settings.data_source, "resolved": resolved }))
}

fn keyword_case(p: &Value) -> Result<Option<KeywordCase>, CommandError> {
    match opt_str(p, "keywordCase") {
        None => Ok(None),
        Some(v) => KeywordCase::parse(v)
            .map(Some)
            .ok_or_else(|| CommandError::BadParams(format!("unknown keywordCase `{v}` (upper, lower, preserve)"))),
    }
}

fn indent(p: &Value) -> Result<Option<usize>, CommandError> {
    match opt_usize(p, "indent")? {
        Some(n) if (1..=16).contains(&n) => Ok(Some(n)),
        Some(n) => Err(CommandError::BadParams(format!("indent {n} is out of range (1–16)"))),
        None => Ok(None),
    }
}

fn settings(s: &mut Session, p: &Value) -> CmdResult {
    sql_lang::load_settings(s, false);
    sql_lang::refresh_env(s, true);
    let mut changed = false;
    if let Some(v) = p.get("dialect") {
        s.sql_lang.settings.dialect = match v {
            Value::Null => None,
            Value::String(d) => {
                Some(SqlDialect::parse(d).ok_or_else(|| CommandError::BadParams(format!("unknown dialect `{d}`")))?)
            }
            _ => return Err(CommandError::BadParams("`dialect` must be a string or null".into())),
        };
        changed = true;
    }
    if let Some(k) = keyword_case(p)? {
        s.sql_lang.settings.format.keyword_case = k;
        changed = true;
    }
    if let Some(n) = indent(p)? {
        s.sql_lang.settings.format.indent = n;
        changed = true;
    }
    if changed {
        sql_lang::save_settings(s)?;
    }
    Ok(json!({
        "settings": s.sql_lang.settings,
        "dataSource": sql_lang::injected_source(s),
        "dialect": sql_lang::injected_dialect(s),
        "databaseUrl": s.sql_lang.database_url().is_some(),
    }))
}

fn format_text(s: &mut Session, p: &Value) -> CmdResult {
    let text = req_str(p, "text")?;
    let mut opts = sql_lang::format_options(s);
    if let Some(k) = keyword_case(p)? {
        opts.keyword_case = k;
    }
    if let Some(n) = indent(p)? {
        opts.indent = n;
    }
    Ok(json!({ "text": sql_format::format_sql(text, &opts) }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn specs_have_labels_and_sql_ids() {
        for s in super::specs() {
            assert!(!s.label.is_empty());
            assert!(s.id.starts_with("sql."));
        }
    }
}
