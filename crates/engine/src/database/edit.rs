//! In-grid editing of table results: pending cell changes, new and deleted rows, the primary key
//! that identifies rows, and submitting everything as one transaction on the result's worker.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use codecraft_db::edit::RowChange;
use codecraft_db::query_table::QueryTable;
use codecraft_db::{Cell, Column, DbKind};

use super::{DbState, OutputLevel, Req, ResultSet, node_key};

/// What is known about a table's primary key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum KeyState {
    Loading,
    /// Primary key column names (empty: the table has none).
    Ready(Vec<String>),
    Failed(String),
}

/// How a grid row differs from the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum RowState {
    Unchanged,
    Updated,
    Deleted,
    Inserted,
}

/// What a grid cell shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellView<'a> {
    Value(&'a Cell),
    /// An edited value (not submitted yet).
    Changed(&'a Cell),
    /// A new row's column left to its default.
    Default,
    Missing,
}

/// Pending changes of one result set. Row indices count the fetched rows first, then the new rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GridEdits {
    /// Fetched row → column → new value.
    pub updated: BTreeMap<usize, BTreeMap<usize, Cell>>,
    /// Fetched rows to delete.
    pub deleted: BTreeSet<usize>,
    /// New rows; `None` leaves a column to its default.
    pub inserted: Vec<Vec<Option<Cell>>>,
    /// Submitted, waiting for the database.
    pub submitting: bool,
    /// Why the last submit failed (the changes stay pending).
    pub error: Option<String>,
    /// The grid row whose change failed in the last submit.
    pub failed_row: Option<usize>,
}

/// Same value as far as the user can tell (an edit back to the original is no edit).
fn same(a: &Cell, b: &Cell) -> bool {
    a.is_null() == b.is_null() && a.to_plain() == b.to_plain()
}

impl GridEdits {
    pub fn is_empty(&self) -> bool {
        self.updated.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    /// Rows with a pending change.
    pub fn pending_rows(&self) -> usize {
        self.updated.keys().filter(|r| !self.deleted.contains(r)).count() + self.deleted.len() + self.inserted.len()
    }

    /// Rows the grid shows: the fetched ones and the new ones.
    pub fn row_count(&self, fetched: usize) -> usize {
        fetched.saturating_add(self.inserted.len())
    }

    pub fn row_state(&self, row: usize, fetched: usize) -> RowState {
        if row >= fetched {
            RowState::Inserted
        } else if self.deleted.contains(&row) {
            RowState::Deleted
        } else if self.updated.contains_key(&row) {
            RowState::Updated
        } else {
            RowState::Unchanged
        }
    }

    pub fn cell<'a>(&'a self, rows: &'a [Vec<Cell>], row: usize, col: usize) -> CellView<'a> {
        if let Some(r) = row.checked_sub(rows.len()) {
            return match self.inserted.get(r).and_then(|v| v.get(col)) {
                Some(Some(c)) => CellView::Changed(c),
                Some(None) => CellView::Default,
                None => CellView::Missing,
            };
        }
        if let Some(c) = self.updated.get(&row).and_then(|m| m.get(&col)) {
            return CellView::Changed(c);
        }
        rows.get(row).and_then(|r| r.get(col)).map(CellView::Value).unwrap_or(CellView::Missing)
    }

    /// Set a cell. Setting a fetched cell back to its original value drops the edit.
    pub(crate) fn set(
        &mut self,
        rows: &[Vec<Cell>],
        columns: usize,
        row: usize,
        col: usize,
        value: Cell,
    ) -> Result<(), String> {
        if col >= columns {
            return Err(format!("no column {col}"));
        }
        if let Some(r) = row.checked_sub(rows.len()) {
            let new = self.inserted.get_mut(r).ok_or_else(|| format!("no row {row}"))?;
            if let Some(slot) = new.get_mut(col) {
                *slot = Some(value);
            }
            return Ok(());
        }
        if self.deleted.contains(&row) {
            return Err(format!("row {} is marked for deletion", row + 1));
        }
        let original = rows.get(row).and_then(|r| r.get(col)).ok_or_else(|| format!("no row {row}"))?;
        let cells = self.updated.entry(row).or_default();
        if same(original, &value) {
            cells.remove(&col);
        } else {
            cells.insert(col, value);
        }
        if cells.is_empty() {
            self.updated.remove(&row);
        }
        Ok(())
    }

    /// Append a new row (every column at its default); returns its grid index.
    pub(crate) fn add_row(&mut self, fetched: usize, columns: usize) -> usize {
        self.inserted.push(vec![None; columns]);
        self.row_count(fetched).saturating_sub(1)
    }

    /// Delete rows: new rows disappear, fetched rows are marked. Indices are grid indices.
    pub(crate) fn delete(&mut self, fetched: usize, rows: &[usize]) -> Result<usize, String> {
        let total = self.row_count(fetched);
        if let Some(bad) = rows.iter().find(|r| **r >= total) {
            return Err(format!("no row {bad}"));
        }
        let mut new: Vec<usize> = rows.iter().filter_map(|r| r.checked_sub(fetched)).collect();
        new.sort_unstable();
        new.dedup();
        for i in new.iter().rev() {
            if *i < self.inserted.len() {
                self.inserted.remove(*i);
            }
        }
        for r in rows.iter().filter(|r| **r < fetched) {
            self.deleted.insert(*r);
        }
        Ok(rows.len())
    }

    /// Undo pending changes of some rows (grid indices), or of all rows.
    pub(crate) fn revert(&mut self, fetched: usize, rows: Option<&[usize]>) {
        let Some(rows) = rows else {
            *self = Self::default();
            return;
        };
        let mut new: Vec<usize> = rows.iter().filter_map(|r| r.checked_sub(fetched)).collect();
        new.sort_unstable();
        new.dedup();
        for i in new.iter().rev() {
            if *i < self.inserted.len() {
                self.inserted.remove(*i);
            }
        }
        for r in rows.iter().filter(|r| **r < fetched) {
            self.updated.remove(r);
            self.deleted.remove(r);
        }
        self.error = None;
    }

    /// The grid row of each change [`GridEdits::changes`] returns, in the same order.
    pub fn change_rows(&self, fetched: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self.deleted.iter().copied().collect();
        out.extend(self.updated.keys().filter(|r| !self.deleted.contains(r)).copied());
        out.extend((0..self.inserted.len()).map(|i| fetched.saturating_add(i)));
        out
    }

    /// The changes to submit: deletes, then updates, then inserts. `keys` are the primary key
    /// column indices.
    pub fn changes(&self, columns: &[Column], rows: &[Vec<Cell>], keys: &[usize]) -> Result<Vec<RowChange>, String> {
        let names: Vec<Option<String>> = columns.iter().map(|c| Some(c.name.clone())).collect();
        self.changes_mapped(&names, rows, keys)
    }

    /// [`GridEdits::changes`] where result column `i` shows table column `names[i]` (`None`:
    /// computed by the query, so it cannot change).
    pub fn changes_mapped(
        &self,
        names: &[Option<String>],
        rows: &[Vec<Cell>],
        keys: &[usize],
    ) -> Result<Vec<RowChange>, String> {
        if keys.is_empty() {
            return Err("the table has no primary key".into());
        }
        let name = |c: usize| match names.get(c) {
            Some(Some(n)) => Ok(n.clone()),
            Some(None) => Err(format!("column {} is computed by the query and cannot be changed", c + 1)),
            None => Err(format!("no column {c}")),
        };
        let key_of = |row: usize| -> Result<Vec<(String, Cell)>, String> {
            let r = rows.get(row).ok_or_else(|| format!("no row {row}"))?;
            keys.iter().map(|k| Ok((name(*k)?, r.get(*k).cloned().ok_or_else(|| format!("no column {k}"))?))).collect()
        };
        let mut out = Vec::new();
        for r in &self.deleted {
            out.push(RowChange::Delete { key: key_of(*r)? });
        }
        for (r, cells) in self.updated.iter().filter(|(r, _)| !self.deleted.contains(r)) {
            let set = cells.iter().map(|(c, v)| Ok((name(*c)?, v.clone()))).collect::<Result<Vec<_>, String>>()?;
            out.push(RowChange::Update { key: key_of(*r)?, set });
        }
        for row in &self.inserted {
            let values = row
                .iter()
                .enumerate()
                .filter_map(|(c, v)| v.as_ref().map(|v| (c, v)))
                .map(|(c, v)| Ok((name(c)?, v.clone())))
                .collect::<Result<Vec<_>, String>>()?;
            out.push(RowChange::Insert { values });
        }
        Ok(out)
    }
}

/// The SQL shown before submitting: each statement with its bound values listed after it
/// (`-- parameters: $1 = …`), or with literals and `note` saying why the driver cannot bind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EditPreview {
    pub result: u64,
    pub statements: Vec<String>,
}

/// Where the edits of a console query of one table go (see [`codecraft_db::query_table`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryEdit {
    /// The schema the table resolved to (the query's own, or the console's current one).
    pub schema: String,
    pub table: String,
    #[serde(skip)]
    pub(crate) query: QueryTable,
}

/// Where a result's edits go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditTarget {
    pub kind: DbKind,
    pub schema: String,
    pub table: String,
    /// Key column indices in the result (the primary key, or for a console query a unique key).
    pub keys: Vec<usize>,
    /// The table column each result column shows (`None`: computed by the query, read-only).
    pub columns: Vec<Option<String>>,
}

impl DbState {
    /// Load a table's primary key (once) so its results can be edited.
    pub(crate) fn request_keys(&mut self, source: &str, schema: &str, table: &str) {
        let k = node_key(source, Some(schema), Some(table));
        if matches!(self.table_keys.get(&k), Some(KeyState::Loading | KeyState::Ready(_))) {
            return;
        }
        self.table_keys.insert(k.clone(), KeyState::Loading);
        if let Err(e) = self.meta(source, super::MetaReq::Details(schema.to_string(), table.to_string())) {
            self.table_keys.insert(k, KeyState::Failed(e));
        }
    }

    /// Whether a console query's rows are one table's rows, and which table: parsed from the
    /// query, unqualified names resolved like the console's connection resolves them. Starts
    /// loading the table's keys. `Err` is the reason the result stays read-only.
    pub(crate) fn query_edit(&mut self, source: &str, worker: &str, sql: &str) -> Result<QueryEdit, String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        let kind = ds.config.kind;
        if !kind.supports_transactions() {
            return Err(format!("read-only: {}", codecraft_db::tx::no_transactions(kind)));
        }
        let query = codecraft_db::query_table::parse(kind, sql)?;
        let schema = match &query.schema {
            Some(s) => s.clone(),
            None => {
                if self.nav.manual_schema(worker) {
                    return Err("a statement on this connection changed its schema: qualify the table \
                                (schema.table) to edit the rows"
                        .into());
                }
                // PostgreSQL and MySQL consoles may run in the schema chosen for them; SQLite
                // and SQL Server name tables in the default schema.
                let switched = match kind {
                    DbKind::Postgres | DbKind::Mysql | DbKind::Mariadb => self.nav.worker_schema(worker),
                    _ => None,
                };
                let default = ds.tree.info.as_ref().and_then(|i| i.default_schema.clone());
                match (switched, default, kind) {
                    (Some(s), _, _) => s.to_string(),
                    (None, Some(d), _) => d,
                    (None, None, DbKind::Sqlite) => "main".into(),
                    (None, None, _) => {
                        return Err("the connection's schema is not known yet: qualify the table (schema.table) \
                                    or open the data source first"
                            .into());
                    }
                }
            }
        };
        let table = query.table.clone();
        self.request_keys(source, &schema, &table);
        Ok(QueryEdit { schema, table, query })
    }

    /// Why a result cannot be edited, or where its edits go.
    pub(crate) fn edit_target(&self, r: &ResultSet) -> Result<EditTarget, String> {
        let ds = self.source(&r.source).ok_or_else(|| format!("no data source `{}`", r.source))?;
        if r.table.is_none()
            && let Some(q) = &r.query_table
        {
            if ds.config.read_only {
                return Err("the data source is read-only".into());
            }
            let k = node_key(&r.source, Some(&q.schema), Some(&q.table));
            let details = match self.table_keys.get(&k) {
                Some(KeyState::Ready(_)) => self.key_details.get(&k).ok_or("the table's keys are not loaded")?,
                Some(KeyState::Loading) => return Err("loading the table's keys…".into()),
                Some(KeyState::Failed(e)) => {
                    return Err(format!("cannot read the keys of {}.{}: {e}", q.schema, q.table));
                }
                None => return Err("the table's keys are not loaded".into()),
            };
            if details.columns.is_empty() {
                return Err(format!("no table {}.{} (a view or another schema's table?)", q.schema, q.table));
            }
            let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
            let columns = codecraft_db::query_table::map_columns(&q.query, details, &names)?;
            let keys = codecraft_db::query_table::pick_key(details, &columns)?;
            return Ok(EditTarget { kind: ds.config.kind, schema: q.schema.clone(), table: q.table.clone(), keys, columns });
        }
        let t = r.table.as_ref().ok_or_else(|| {
            r.read_only_reason.clone().unwrap_or_else(|| {
                "only table data, or a console query of one table with a primary or unique key, can be edited".into()
            })
        })?;
        if ds.config.read_only {
            return Err("the data source is read-only".into());
        }
        let names = match self.table_keys.get(&node_key(&r.source, Some(&t.schema), Some(&t.table))) {
            Some(KeyState::Ready(k)) if !k.is_empty() => k,
            Some(KeyState::Ready(_)) => return Err("the table has no primary key".into()),
            Some(KeyState::Loading) => return Err("loading the table's primary key…".into()),
            Some(KeyState::Failed(e)) => return Err(format!("cannot read the table's primary key: {e}")),
            None => return Err("the table's primary key is not loaded".into()),
        };
        let keys = names
            .iter()
            .map(|n| {
                r.columns
                    .iter()
                    .position(|c| &c.name == n)
                    .ok_or_else(|| format!("the result does not include the primary key column `{n}`"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let columns = r.columns.iter().map(|c| Some(c.name.clone())).collect();
        Ok(EditTarget { kind: ds.config.kind, schema: t.schema.clone(), table: t.table.clone(), keys, columns })
    }

    /// Whether column `col` of an editable result can change (`Err`: computed by the query).
    pub fn column_editable(&self, result: u64, col: usize) -> Result<(), String> {
        let r = self.result(result).ok_or_else(|| format!("no result {result}"))?;
        let t = self.edit_target(r)?;
        match t.columns.get(col) {
            Some(Some(_)) => Ok(()),
            Some(None) => Err(format!(
                "`{}` is computed by the query: only plain columns of {} can be edited",
                r.columns.get(col).map(|c| c.name.as_str()).unwrap_or("?"),
                t.table
            )),
            None => Err(format!("no column {col}")),
        }
    }

    /// Whether a result can be edited now (`Err` says why not).
    pub fn editable(&self, result: u64) -> Result<(), String> {
        let r = self.result(result).ok_or_else(|| format!("no result {result}"))?;
        if r.is_running() {
            return Err("the query is still running".into());
        }
        if r.edits.submitting {
            return Err("submitting changes…".into());
        }
        if let Some(kind) = self.source(&r.source).map(|d| d.config.kind).filter(|k| !k.supports_transactions()) {
            return Err(format!(
                "cannot edit data in the grid: {} (use statements in the console)",
                codecraft_db::tx::no_transactions(kind)
            ));
        }
        self.edit_target(r).map(|_| ())
    }

    pub(crate) fn edits_mut(&mut self, result: u64) -> Option<&mut ResultSet> {
        self.results.iter_mut().find(|r| r.id == result)
    }

    /// The SQL a submit would run.
    pub(crate) fn edit_statements(&self, result: u64) -> Result<Vec<String>, String> {
        let r = self.result(result).ok_or_else(|| format!("no result {result}"))?;
        let t = self.edit_target(r)?;
        let changes = r.edits.changes_mapped(&t.columns, &r.rows, &t.keys)?;
        preview_statements(t.kind, &t.schema, &t.table, &changes)
    }

    /// Send a result's pending changes to its worker (one transaction); the outcome arrives as
    /// an `Applied` event.
    pub(crate) fn submit_edits(&mut self, result: u64) -> Result<Vec<String>, String> {
        self.editable(result)?;
        let r = self.result(result).ok_or_else(|| format!("no result {result}"))?;
        let t = self.edit_target(r)?;
        let changes = r.edits.changes_mapped(&t.columns, &r.rows, &t.keys)?;
        if changes.is_empty() {
            return Err("no pending changes".into());
        }
        let statements = preview_statements(t.kind, &t.schema, &t.table, &changes)?;
        let (worker, source) = (r.worker.clone(), r.source.clone());
        self.close_cursors(&worker, &source);
        self.ensure_worker(&worker, &source)?;
        for sql in &statements {
            self.log(OutputLevel::Sql, &source, sql.clone());
        }
        self.send(&worker, Req::Apply { result, schema: t.schema, table: t.table, changes })?;
        if let Some(r) = self.edits_mut(result) {
            r.edits.submitting = true;
            r.edits.error = None;
            r.edits.failed_row = None;
        }
        if self.edit_preview.as_ref().is_some_and(|p| p.result == result) {
            self.edit_preview = None;
        }
        Ok(statements)
    }
}

/// The statements a submit runs, as previewed and logged: placeholders with the bound values
/// listed after each statement; for a driver that cannot bind, literals and a first line saying
/// so.
pub(crate) fn preview_statements(
    kind: DbKind,
    schema: &str,
    table: &str,
    changes: &[RowChange],
) -> Result<Vec<String>, String> {
    let bound = codecraft_db::edit::bound_statements(kind, schema, table, changes).map_err(|e| e.to_string())?;
    let mut out: Vec<String> = bound.iter().map(|b| b.preview(kind)).collect();
    if !codecraft_db::bind::binds_generated(kind)
        && let (Some(why), Some(first)) = (codecraft_db::bind::literal_reason(kind), out.first_mut())
    {
        *first = format!("-- values are inlined as escaped literals: {why}\n{first}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<Column> {
        names.iter().map(|n| Column { name: (*n).into(), type_name: String::new() }).collect()
    }

    #[test]
    fn edits_track_rows_and_build_changes() {
        let columns = cols(&["id", "name"]);
        let rows = vec![vec![Cell::Int(1), Cell::Text("a".into())], vec![Cell::Int(2), Cell::Null]];
        let mut e = GridEdits::default();
        assert!(e.is_empty());
        e.set(&rows, 2, 0, 1, Cell::Text("b".into())).unwrap();
        assert_eq!(e.row_state(0, 2), RowState::Updated);
        assert_eq!(e.cell(&rows, 0, 1), CellView::Changed(&Cell::Text("b".into())));
        assert_eq!(e.cell(&rows, 1, 1), CellView::Value(&Cell::Null));
        // Back to the original: no edit.
        e.set(&rows, 2, 0, 1, Cell::Text("a".into())).unwrap();
        assert!(e.is_empty());
        // NULL vs empty text differ.
        e.set(&rows, 2, 1, 1, Cell::Text(String::new())).unwrap();
        assert_eq!(e.pending_rows(), 1);
        assert!(e.set(&rows, 2, 0, 5, Cell::Null).is_err());
        assert!(e.set(&rows, 2, 9, 0, Cell::Null).is_err());

        let new = e.add_row(2, 2);
        assert_eq!(new, 2);
        assert_eq!(e.cell(&rows, 2, 0), CellView::Default);
        e.set(&rows, 2, 2, 1, Cell::Text("n".into())).unwrap();
        assert_eq!(e.row_state(2, 2), RowState::Inserted);
        e.delete(2, &[0]).unwrap();
        assert!(e.set(&rows, 2, 0, 1, Cell::Null).is_err());
        assert!(e.delete(2, &[7]).is_err());

        let changes = e.changes(&columns, &rows, &[0]).unwrap();
        assert_eq!(
            changes,
            vec![
                RowChange::Delete { key: vec![("id".into(), Cell::Int(1))] },
                RowChange::Update {
                    key: vec![("id".into(), Cell::Int(2))],
                    set: vec![("name".into(), Cell::Text(String::new()))],
                },
                RowChange::Insert { values: vec![("name".into(), Cell::Text("n".into()))] },
            ]
        );
        assert!(e.changes(&columns, &rows, &[]).is_err());

        // Deleting a new row removes it; reverting rows undoes their changes.
        e.add_row(2, 2);
        e.delete(2, &[2]).unwrap();
        assert_eq!(e.inserted.len(), 1);
        assert_eq!(e.cell(&rows, 2, 1), CellView::Default);
        e.revert(2, Some(&[0, 2]));
        assert_eq!(e.row_state(0, 2), RowState::Unchanged);
        assert_eq!(e.pending_rows(), 1);
        e.revert(2, None);
        assert!(e.is_empty());
    }
}
