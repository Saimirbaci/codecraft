//! Result grid extras: the filter row (WHERE / ORDER BY), transposed view, value viewer,
//! aggregates over a selection, and Copy As / Export in several formats.

use serde_json::{Value, json};

use codecraft_db::export::{ExportFormat, InsertTarget, export};
use codecraft_db::{Cell, Column, sql};

use super::{bad, failed, kind_of, result_param};
use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::database::{ResultFilter, ResultSet, RunSpec, SortKey};
use crate::params::{opt_str, req_path, req_usize};
use crate::{CommandError, Effect, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "db.filterResult",
            "Filter Result",
            None,
            "{result?, where?: sql, orderBy?: sql} — the grid's filter row: re-queries the table (or wraps the query) \
             with WHERE (…) ORDER BY …; empty strings clear",
            Context::Global,
            always,
            filter_result,
        ),
        spec(
            "db.transposeResult",
            "Transpose",
            None,
            "{result?, transposed?: bool} — show rows as columns (toggles without `transposed`)",
            Context::Global,
            always,
            transpose,
        ),
        spec(
            "db.getCellValue",
            "Show Value",
            None,
            "{result?, row, column} — the value viewer: kind (text/number/json/binary/image), text (pretty JSON, hex \
             dump), image size",
            Context::Global,
            always,
            cell_value,
        ),
        spec(
            "db.aggregate",
            "Aggregate Selection",
            None,
            "{result?, rows?: [int], columns?: [int], rowRange?: [from, to], columnRange?: [from, to]} — count, sum, \
             avg, min, max of the cells (all by default)",
            Context::Global,
            always,
            aggregate,
        ),
        spec(
            "db.copyResult",
            "Copy As",
            None,
            "{result?, format?: csv|tsv|json|sql|markdown (tsv), table?: name for SQL, rows?, columns?, rowRange?, \
             columnRange?} — copies to the clipboard; a plan copies its raw output",
            Context::Global,
            always,
            copy_result,
        ),
        spec(
            "db.exportResult",
            "Export Data",
            None,
            "{result?, path, format?: csv|tsv|json|sql|markdown (from the extension), table?, rows?, columns?, \
             rowRange?, columnRange?}",
            Context::Global,
            always,
            export_result,
        ),
    ]
}

/// Re-run a result with a filter row and sort (in place).
pub(super) fn requery(s: &mut Session, r: &ResultSet, filter: ResultFilter, sort: Option<SortKey>) -> CmdResult {
    if r.is_running() {
        return Err(CommandError::Disabled("the query is still running".into()));
    }
    if !r.edits.is_empty() || r.edits.submitting {
        return Err(CommandError::Disabled("submit or revert the pending changes first".into()));
    }
    if r.plan.is_some() {
        return Err(CommandError::Disabled("a plan has no rows to filter".into()));
    }
    let kind = kind_of(s, &r.source);
    let typed_order = !filter.order_by.trim().is_empty();
    let sort = if typed_order { None } else { sort };
    let order = match (typed_order, sort, &r.table) {
        (true, _, _) => filter.order_by.clone(),
        (false, Some(k), Some(_)) => {
            let col = r.columns.get(k.column).map(|c| c.name.clone()).unwrap_or_default();
            sql::order_by_column(kind, &col, k.desc)
        }
        (false, Some(k), None) => format!("{}{}", k.column + 1, if k.desc { " DESC" } else { "" }),
        (false, None, _) => String::new(),
    };
    let statement = match &r.table {
        Some(t) => sql::select_table_where(kind, &t.schema, &t.table, &filter.where_, &order),
        None => sql::filter_query(&r.base_sql, &filter.where_, &order),
    };
    let run =
        s.db.start_run(RunSpec {
            worker: r.worker.clone(),
            source: r.source.clone(),
            owner: r.owner.clone(),
            title: r.title.clone(),
            statements: vec![statement.clone()],
            table: r.table.clone(),
            sort,
            replace: Some(r.id),
            base_sql: Some(r.base_sql.clone()),
            filter,
            ..RunSpec::default()
        })
        .map_err(failed)?;
    Ok(json!({ "run": run, "sql": statement }))
}

fn filter_result(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let text = |key: &str| -> Result<Option<String>, CommandError> {
        match p.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(v)) => Ok(Some(v.clone())),
            Some(_) => Err(bad(format!("`{key}` must be a string"))),
        }
    };
    let filter = ResultFilter {
        where_: text("where")?.unwrap_or_else(|| r.filter.where_.clone()),
        order_by: text("orderBy")?.unwrap_or_else(|| r.filter.order_by.clone()),
    };
    requery(s, &r, filter, r.sort)
}

fn transpose(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let on = p.get("transposed").and_then(Value::as_bool).unwrap_or(!r.transposed);
    if let Some(x) = s.db.results.iter_mut().find(|x| x.id == r.id) {
        x.transposed = on;
    }
    Ok(json!({ "transposed": on }))
}

fn cell_value(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let row = req_usize(p, "row")?;
    let column = req_usize(p, "column")?;
    let col = r.columns.get(column).ok_or_else(|| bad(format!("no column {column}")))?;
    let cell = r.rows.get(row).and_then(|x| x.get(column)).ok_or_else(|| bad(format!("no row {row}")))?;
    let view = codecraft_db::value::view(cell);
    Ok(json!({ "column": col.name, "type": col.type_name, "value": view }))
}

/// Indices from `key` (a list) or `range_key` (`[from, to]`, inclusive), else all of `0..len`.
fn indices(p: &Value, key: &str, range_key: &str, len: usize) -> Result<Vec<usize>, CommandError> {
    let as_index = |v: &Value| v.as_u64().and_then(|n| usize::try_from(n).ok());
    if let Some(v) = p.get(key).filter(|v| !v.is_null()) {
        let list = v.as_array().ok_or_else(|| bad(format!("`{key}` must be an array of indices")))?;
        return list
            .iter()
            .map(|i| as_index(i).filter(|i| *i < len).ok_or_else(|| bad(format!("`{key}`: no index {i}"))))
            .collect();
    }
    if let Some(v) = p.get(range_key).filter(|v| !v.is_null()) {
        let pair =
            v.as_array().filter(|a| a.len() == 2).ok_or_else(|| bad(format!("`{range_key}` must be [from, to]")))?;
        let (Some(a), Some(b)) = (pair.first().and_then(as_index), pair.get(1).and_then(as_index)) else {
            return Err(bad(format!("`{range_key}` must be [from, to]")));
        };
        if a > b || b >= len {
            return Err(bad(format!("`{range_key}` [{a}, {b}] is outside 0..{len}")));
        }
        return Ok((a..=b).collect());
    }
    Ok((0..len).collect())
}

/// The selected columns and rows of a result.
fn selection(r: &ResultSet, p: &Value) -> Result<(Vec<Column>, Vec<Vec<Cell>>), CommandError> {
    let cols = indices(p, "columns", "columnRange", r.columns.len())?;
    let rows = indices(p, "rows", "rowRange", r.rows.len())?;
    let columns = cols.iter().filter_map(|c| r.columns.get(*c).cloned()).collect();
    let data = rows
        .iter()
        .filter_map(|i| r.rows.get(*i))
        .map(|row| cols.iter().map(|c| row.get(*c).cloned().unwrap_or(Cell::Null)).collect())
        .collect();
    Ok((columns, data))
}

fn aggregate(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let (_, rows) = selection(&r, p)?;
    let a = codecraft_db::value::aggregate(rows.iter().flatten());
    Ok(json!({ "aggregates": a, "summary": a.summary() }))
}

fn format_param(p: &Value, default: ExportFormat) -> Result<ExportFormat, CommandError> {
    match opt_str(p, "format") {
        None => Ok(default),
        Some(f) => {
            ExportFormat::parse(f).ok_or_else(|| bad(format!("unknown format `{f}` (csv, tsv, json, sql, markdown)")))
        }
    }
}

fn formatted(s: &Session, r: &ResultSet, p: &Value, format: ExportFormat) -> Result<(String, usize), CommandError> {
    let (columns, rows) = selection(r, p)?;
    let kind = kind_of(s, &r.source);
    let target = match (&r.table, opt_str(p, "table")) {
        (_, Some(t)) => InsertTarget { kind, schema: String::new(), table: t.to_string() },
        (Some(t), None) => InsertTarget { kind, schema: t.schema.clone(), table: t.table.clone() },
        (None, None) => InsertTarget { kind, schema: String::new(), table: "my_table".into() },
    };
    let text = export(format, &columns, &rows, Some(&target)).map_err(|e| failed(e.to_string()))?;
    Ok((text, rows.len()))
}

fn copy_result(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    if let Some(plan) = &r.plan {
        s.effects.push(Effect::CopyToClipboard(plan.raw.clone()));
        s.status = "Plan copied".into();
        return Ok(json!({ "chars": plan.raw.chars().count() }));
    }
    let format = format_param(p, ExportFormat::Tsv)?;
    let (text, n) = formatted(s, &r, p, format)?;
    let chars = text.chars().count();
    s.effects.push(Effect::CopyToClipboard(text));
    s.status = format!("Copied {n} row{} as {}", if n == 1 { "" } else { "s" }, format.label());
    Ok(json!({ "rows": n, "chars": chars, "format": format }))
}

fn export_result(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let path = req_path(p, "path")?;
    if r.plan.is_some() {
        return Err(CommandError::Disabled("a plan has no rows; copy it with db.copyResult".into()));
    }
    let by_ext = path.extension().and_then(|e| ExportFormat::parse(&e.to_string_lossy()));
    let format = format_param(p, by_ext.unwrap_or(ExportFormat::Csv))?;
    let (text, n) = formatted(s, &r, p, format)?;
    std::fs::write(&path, text).map_err(|e| failed(format!("{}: {e}", path.display())))?;
    let note = if r.has_more || r.truncated { " (the rows fetched so far)" } else { "" };
    s.status = format!("Exported {n} rows to {}{note}", path.display());
    Ok(json!({ "rows": n, "path": path, "format": format }))
}
