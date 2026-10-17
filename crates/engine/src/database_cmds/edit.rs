//! In-grid editing commands: edit cells, add/delete rows, revert, preview the SQL and submit.

use serde_json::{Value, json};

use codecraft_db::Cell;

use super::{bad, failed, opt_u64};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::{EditPreview, ResultSet};
use crate::params::req_usize;
use crate::{CommandError, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "db.editCell",
            "Edit Cell",
            None,
            "{result?, row, column, value} — value: string/number/bool, null sets NULL; rows after the fetched ones are new rows. \
             Table data and console queries of one table with a primary or unique key can be edited; a query's computed columns cannot",
            Context::Global,
            always,
            edit_cell,
        ),
        spec("db.addRow", "Add Row", None, "{result?} — returns the new row's index", Context::Global, always, add_row),
        spec(
            "db.deleteRows",
            "Delete Rows",
            None,
            "{result?, rows: [int]} — new rows disappear, fetched rows are deleted on submit",
            Context::Global,
            always,
            delete_rows,
        ),
        spec(
            "db.revertChanges",
            "Revert Changes",
            None,
            "{result?, rows?: [int]} — all pending changes by default",
            Context::Global,
            always,
            revert,
        ),
        spec(
            "db.previewChanges",
            "Preview Pending Changes",
            None,
            "{result?} — the UPDATE/INSERT/DELETE statements Submit runs",
            Context::Global,
            always,
            preview,
        ),
        spec("db.closePreview", "Close Changes Preview", None, "", Context::Global, always, |s, _| {
            s.db.edit_preview = None;
            ok()
        }),
        spec(
            "db.submitChanges",
            "Submit Changes",
            None,
            "{result?} — applies pending changes in one transaction, then reloads the table",
            Context::Global,
            always,
            submit,
        ),
        spec(
            "db.getChanges",
            "Get Pending Changes",
            None,
            "{result?} — pending edits, whether the result is editable, and the last submit error",
            Context::Global,
            always,
            get_changes,
        ),
    ]
}

fn result_id(s: &Session, p: &Value) -> Result<u64, CommandError> {
    let id = match opt_u64(p, "result")? {
        Some(id) => id,
        None => s.db.active_result.ok_or_else(|| bad("missing `result`"))?,
    };
    s.db.result(id).map(|r| r.id).ok_or_else(|| bad(format!("no result {id}")))
}

/// The result, after checking it can be edited.
fn editable(s: &Session, p: &Value) -> Result<u64, CommandError> {
    let id = result_id(s, p)?;
    s.db.editable(id).map_err(CommandError::Disabled)?;
    Ok(id)
}

fn with_result<T>(
    s: &mut Session,
    id: u64,
    f: impl FnOnce(&mut ResultSet) -> Result<T, String>,
) -> Result<T, CommandError> {
    let r = s.db.edits_mut(id).ok_or_else(|| bad(format!("no result {id}")))?;
    let out = f(r).map_err(bad)?;
    if s.db.edit_preview.as_ref().is_some_and(|p| p.result == id) {
        s.db.edit_preview = None;
    }
    Ok(out)
}

fn rows_param(p: &Value) -> Result<Option<Vec<usize>>, CommandError> {
    match p.get("rows") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad("`rows` must be row indices")))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(bad("`rows` must be an array of row indices")),
    }
}

fn edit_cell(s: &mut Session, p: &Value) -> CmdResult {
    let id = editable(s, p)?;
    let row = req_usize(p, "row")?;
    let column = req_usize(p, "column")?;
    s.db.column_editable(id, column).map_err(CommandError::Disabled)?;
    let value = match p.get("value") {
        None => return Err(bad("missing `value` (null sets NULL)")),
        Some(Value::Array(_) | Value::Object(_)) => return Err(bad("`value` must be a string, number, bool or null")),
        Some(v) => serde_json::from_value::<Cell>(v.clone()).map_err(|e| bad(e.to_string()))?,
    };
    with_result(s, id, |r| {
        let n = r.columns.len();
        r.edits.set(&r.rows, n, row, column, value)
    })?;
    ok()
}

fn add_row(s: &mut Session, p: &Value) -> CmdResult {
    let id = editable(s, p)?;
    let row = with_result(s, id, |r| Ok(r.edits.add_row(r.rows.len(), r.columns.len())))?;
    Ok(json!({ "row": row }))
}

fn delete_rows(s: &mut Session, p: &Value) -> CmdResult {
    let id = editable(s, p)?;
    let rows = rows_param(p)?.ok_or_else(|| bad("missing `rows`"))?;
    let n = with_result(s, id, |r| r.edits.delete(r.rows.len(), &rows))?;
    Ok(json!({ "deleted": n }))
}

fn revert(s: &mut Session, p: &Value) -> CmdResult {
    let id = result_id(s, p)?;
    if s.db.result(id).is_some_and(|r| r.edits.submitting) {
        return Err(CommandError::Disabled("submitting changes…".into()));
    }
    let rows = rows_param(p)?;
    with_result(s, id, |r| {
        r.edits.revert(r.rows.len(), rows.as_deref());
        Ok(())
    })?;
    ok()
}

fn preview(s: &mut Session, p: &Value) -> CmdResult {
    let id = editable(s, p)?;
    let statements = s.db.edit_statements(id).map_err(failed)?;
    if statements.is_empty() {
        return Err(failed("no pending changes"));
    }
    s.db.edit_preview = Some(EditPreview { result: id, statements: statements.clone() });
    Ok(json!({ "statements": statements }))
}

fn submit(s: &mut Session, p: &Value) -> CmdResult {
    let id = editable(s, p)?;
    let statements = s.db.submit_edits(id).map_err(failed)?;
    s.status = format!("Submitting {} change{}…", statements.len(), if statements.len() == 1 { "" } else { "s" });
    Ok(json!({ "statements": statements }))
}

fn get_changes(s: &mut Session, p: &Value) -> CmdResult {
    let id = result_id(s, p)?;
    let editable = s.db.editable(id);
    let statements = s.db.edit_statements(id).unwrap_or_default();
    let r = s.db.result(id).ok_or_else(|| bad(format!("no result {id}")))?;
    // Which columns can change (a console query's computed columns cannot), and where to.
    let columns: Vec<bool> = (0..r.columns.len()).map(|c| s.db.column_editable(id, c).is_ok()).collect();
    let target = r
        .table
        .as_ref()
        .map(|t| json!({ "schema": t.schema, "table": t.table }))
        .or_else(|| r.query_table.as_ref().map(|q| json!({ "schema": q.schema, "table": q.table, "fromQuery": true })));
    Ok(json!({
        "result": id,
        "editable": editable.is_ok(),
        "reason": editable.err(),
        "target": target,
        "editableColumns": columns,
        "pendingRows": r.edits.pending_rows(),
        "edits": r.edits,
        "statements": statements,
    }))
}
