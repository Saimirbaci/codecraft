//! Compare Data (DataGrip's Compare Content): two tables or queries, possibly on different data
//! sources, matched by key columns. Both sides are read ordered by the key and merged as they
//! stream ([`RowStream`]: one connection and thread per side, a bounded channel between them),
//! so big tables are never loaded whole; only the differences are kept (up to a cap). The
//! result yields a sync script that makes the right side (a table) equal to the left.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crossbeam_channel::{Receiver, bounded};
use serde::Serialize;

use crate::edit::RowChange;
use crate::sql::quote_ident;
use crate::{Cell, Column, Connection, DbError, DbKind, DbResult, Page, ResultSink};

/// Differences kept per category (counts go on).
pub const DEFAULT_MAX_KEPT: usize = 5_000;

/// The query that reads one side ordered by its key, in an order [`compare_keys`] agrees with:
/// text keys sort by code point (PostgreSQL `COLLATE "C"`, MySQL binary), NULLs first.
/// `key_types` are the key columns' type names (empty when unknown).
pub fn ordered_query(kind: DbKind, query: &str, keys: &[String], key_types: &[String]) -> String {
    let inner = query.trim().trim_end_matches(';').trim_end();
    let order: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let col = format!("q.{}", quote_ident(kind, k));
            let ty = key_types.get(i).map(|t| t.to_ascii_lowercase()).unwrap_or_default();
            let texty = is_text_type(&ty);
            match kind {
                DbKind::Postgres if texty => format!("{col} COLLATE \"C\" NULLS FIRST"),
                DbKind::Postgres => format!("{col} NULLS FIRST"),
                DbKind::Mysql | DbKind::Mariadb if texty => format!("CAST({col} AS BINARY)"),
                _ => col,
            }
        })
        .collect();
    format!("SELECT * FROM ({inner}\n) AS q ORDER BY {}", order.join(", "))
}

/// Whether a type name is a character type.
pub fn is_text_type(ty: &str) -> bool {
    let t = ty.to_ascii_lowercase();
    ["char", "text", "clob", "string", "name", "citext", "enum", "set"].iter().any(|w| t.contains(w))
        || t == "bpchar"
        || t == "var_string"
}

fn as_number(c: &Cell) -> Option<f64> {
    match c {
        Cell::Int(i) => Some(*i as f64),
        Cell::UInt(u) => Some(*u as f64),
        Cell::Float(f) => Some(*f),
        Cell::Other(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        Cell::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Integers compare exactly (no float rounding of big keys).
fn as_int(c: &Cell) -> Option<i128> {
    match c {
        Cell::Int(i) => Some(i128::from(*i)),
        Cell::UInt(u) => Some(i128::from(*u)),
        Cell::Other(s) => s.trim().parse::<i128>().ok(),
        _ => None,
    }
}

/// Order of two key values: NULL first, numbers numerically, everything else by text.
pub fn compare_cell(a: &Cell, b: &Cell) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    if let (Some(x), Some(y)) = (as_int(a), as_int(b)) {
        return x.cmp(&y);
    }
    let text_like = |c: &Cell| matches!(c, Cell::Text(_));
    if !text_like(a)
        && !text_like(b)
        && let (Some(x), Some(y)) = (as_number(a), as_number(b))
    {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    match (a, b) {
        (Cell::Bytes(x), Cell::Bytes(y)) => x.cmp(y),
        _ => a.to_plain().cmp(&b.to_plain()),
    }
}

/// Order of two keys.
pub fn compare_keys(a: &[Cell], b: &[Cell]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = compare_cell(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Whether two values are equal as data (`1` = `1.0`, decimals by value, NULL only equals NULL).
pub fn same_value(a: &Cell, b: &Cell) -> bool {
    if a.is_null() || b.is_null() {
        return a.is_null() && b.is_null();
    }
    if let (Some(x), Some(y)) = (as_int(a), as_int(b)) {
        return x == y;
    }
    if !matches!(a, Cell::Text(_))
        && !matches!(b, Cell::Text(_))
        && let (Some(x), Some(y)) = (as_number(a), as_number(b))
    {
        return x == y;
    }
    match (a, b) {
        (Cell::Bool(x), Cell::Bool(y)) => x == y,
        (Cell::Bytes(x), Cell::Bytes(y)) => x == y,
        _ => a.to_plain() == b.to_plain(),
    }
}

/// A row on both sides whose values differ.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RowDiff {
    pub key: Vec<Cell>,
    /// Values of the compared columns, left and right.
    pub left: Vec<Cell>,
    pub right: Vec<Cell>,
    /// Indices (into the compared columns) that differ.
    pub changed: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareCounts {
    pub left: u64,
    pub right: u64,
    pub only_left: u64,
    pub only_right: u64,
    pub different: u64,
    pub same: u64,
}

impl CompareCounts {
    pub fn identical(&self) -> bool {
        self.only_left == 0 && self.only_right == 0 && self.different == 0
    }
}

/// The outcome of a comparison.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    /// The columns compared (present on both sides), keys first.
    pub columns: Vec<String>,
    pub key_columns: Vec<String>,
    /// Columns only one side has (not compared).
    pub left_only_columns: Vec<String>,
    pub right_only_columns: Vec<String>,
    pub only_left: Vec<Vec<Cell>>,
    pub only_right: Vec<Vec<Cell>>,
    pub different: Vec<RowDiff>,
    pub counts: CompareCounts,
    /// More differences exist than were kept.
    pub truncated: bool,
}

/// Rows of one side, in key order.
pub trait RowSource {
    fn columns(&mut self) -> DbResult<Vec<Column>>;
    /// The next row; `None` at the end.
    fn next_row(&mut self) -> DbResult<Option<Vec<Cell>>>;
}

/// Rows in memory (tests, small inputs).
pub struct VecSource {
    pub columns: Vec<Column>,
    pub rows: std::vec::IntoIter<Vec<Cell>>,
}

impl RowSource for VecSource {
    fn columns(&mut self) -> DbResult<Vec<Column>> {
        Ok(self.columns.clone())
    }
    fn next_row(&mut self) -> DbResult<Option<Vec<Cell>>> {
        Ok(self.rows.next())
    }
}

enum Msg {
    Columns(Vec<Column>),
    Rows(Vec<Vec<Cell>>),
    Done(DbResult<()>),
}

struct ChannelSink {
    tx: crossbeam_channel::Sender<Msg>,
    stop: Arc<AtomicBool>,
}

impl ResultSink for ChannelSink {
    fn columns(&mut self, columns: &[Column]) {
        let _ = self.tx.send(Msg::Columns(columns.to_vec()));
    }
    fn page(&mut self, page: Page) -> bool {
        if self.stop.load(AtomicOrdering::SeqCst) {
            return false;
        }
        self.tx.send(Msg::Rows(page.rows)).is_ok()
    }
}

/// A query streaming on its own thread and connection, with backpressure (two pages ahead).
/// Dropping it stops the query.
pub struct RowStream {
    rx: Receiver<Msg>,
    columns: Option<Vec<Column>>,
    buf: std::collections::VecDeque<Vec<Cell>>,
    done: bool,
    stop: Arc<AtomicBool>,
}

impl RowStream {
    /// `connect` runs on the stream's thread.
    pub fn spawn<F>(connect: F, sql: String, page_size: usize) -> DbResult<RowStream>
    where
        F: FnOnce() -> DbResult<Box<dyn Connection>> + Send + 'static,
    {
        let (tx, rx) = bounded(2);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        std::thread::Builder::new()
            .name("db-compare".into())
            .spawn(move || {
                let r = connect().and_then(|mut conn| {
                    let mut sink = ChannelSink { tx: tx.clone(), stop: s2 };
                    conn.execute(&sql, page_size, &mut sink).map(|_| ())
                });
                let _ = tx.send(Msg::Done(r));
            })
            .map_err(|e| DbError::Io(format!("cannot start a compare thread: {e}")))?;
        Ok(RowStream { rx, columns: None, buf: Default::default(), done: false, stop })
    }

    fn pull(&mut self) -> DbResult<()> {
        match self.rx.recv() {
            Ok(Msg::Columns(c)) => self.columns = Some(c),
            Ok(Msg::Rows(r)) => self.buf.extend(r),
            Ok(Msg::Done(r)) => {
                self.done = true;
                r?;
            }
            Err(_) => self.done = true,
        }
        Ok(())
    }
}

impl Drop for RowStream {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::SeqCst);
    }
}

impl RowSource for RowStream {
    fn columns(&mut self) -> DbResult<Vec<Column>> {
        while self.columns.is_none() && !self.done {
            self.pull()?;
        }
        Ok(self.columns.clone().unwrap_or_default())
    }

    fn next_row(&mut self) -> DbResult<Option<Vec<Cell>>> {
        loop {
            if let Some(r) = self.buf.pop_front() {
                return Ok(Some(r));
            }
            if self.done {
                return Ok(None);
            }
            self.pull()?;
        }
    }
}

/// Positions of `names` among `columns` (exact name, else case-insensitive).
fn positions(columns: &[Column], names: &[String], side: &str) -> DbResult<Vec<usize>> {
    names
        .iter()
        .map(|n| {
            columns
                .iter()
                .position(|c| &c.name == n)
                .or_else(|| columns.iter().position(|c| c.name.eq_ignore_ascii_case(n)))
                .ok_or_else(|| DbError::Config(format!("the {side} side has no column `{n}`")))
        })
        .collect()
}

struct Side<'a> {
    src: &'a mut dyn RowSource,
    key_pos: Vec<usize>,
    col_pos: Vec<usize>,
    current: Option<(Vec<Cell>, Vec<Cell>)>,
    last_key: Option<Vec<Cell>>,
    name: &'static str,
    count: u64,
}

impl Side<'_> {
    fn advance(&mut self) -> DbResult<()> {
        self.current = match self.src.next_row()? {
            Some(row) => {
                let pick = |pos: &[usize]| pos.iter().map(|i| row.get(*i).cloned().unwrap_or(Cell::Null)).collect();
                let key: Vec<Cell> = pick(&self.key_pos);
                if let Some(prev) = &self.last_key {
                    match compare_keys(prev, &key) {
                        Ordering::Greater => {
                            return Err(DbError::Query(format!(
                                "the {} side is not in key order (the database sorts the key differently, e.g. a \
                                 collation): compare by a numeric key or add an ORDER BY the IDE can follow",
                                self.name
                            )));
                        }
                        Ordering::Equal => {
                            return Err(DbError::Query(format!(
                                "the {} side has two rows with the key ({}): choose key columns that identify rows",
                                self.name,
                                key.iter().map(Cell::display).collect::<Vec<_>>().join(", ")
                            )));
                        }
                        Ordering::Less => {}
                    }
                }
                self.last_key = Some(key.clone());
                self.count += 1;
                Some((key, pick(&self.col_pos)))
            }
            None => None,
        };
        Ok(())
    }
}

/// Merge two key-ordered sides. `keys` must exist on both; the other columns are matched by
/// name. `progress` sees the counts every 1000 rows; `cancel` stops early with
/// [`DbError::Cancelled`].
pub fn compare(
    left: &mut dyn RowSource,
    right: &mut dyn RowSource,
    keys: &[String],
    max_kept: usize,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&CompareCounts),
) -> DbResult<CompareResult> {
    if keys.is_empty() {
        return Err(DbError::Config("choose the key columns that match rows".into()));
    }
    let lc = left.columns()?;
    let rc = right.columns()?;
    let lk = positions(&lc, keys, "left")?;
    let rk = positions(&rc, keys, "right")?;
    let mut columns: Vec<String> = keys.to_vec();
    for c in &lc {
        let on_right = rc.iter().any(|r| r.name == c.name || r.name.eq_ignore_ascii_case(&c.name));
        if on_right && !columns.iter().any(|k| k.eq_ignore_ascii_case(&c.name)) {
            columns.push(c.name.clone());
        }
    }
    let left_only_columns = lc
        .iter()
        .filter(|c| !rc.iter().any(|r| r.name.eq_ignore_ascii_case(&c.name)))
        .map(|c| c.name.clone())
        .collect();
    let right_only_columns = rc
        .iter()
        .filter(|c| !lc.iter().any(|l| l.name.eq_ignore_ascii_case(&c.name)))
        .map(|c| c.name.clone())
        .collect();
    let lp = positions(&lc, &columns, "left")?;
    let rp = positions(&rc, &columns, "right")?;
    let mut out = CompareResult {
        columns: columns.clone(),
        key_columns: keys.to_vec(),
        left_only_columns,
        right_only_columns,
        ..Default::default()
    };
    let mut l = Side { src: left, key_pos: lk, col_pos: lp, current: None, last_key: None, name: "left", count: 0 };
    let mut r = Side { src: right, key_pos: rk, col_pos: rp, current: None, last_key: None, name: "right", count: 0 };
    l.advance()?;
    r.advance()?;
    let mut steps = 0u64;
    loop {
        steps += 1;
        if steps.is_multiple_of(1000) {
            if cancel.load(AtomicOrdering::SeqCst) {
                return Err(DbError::Cancelled);
            }
            out.counts.left = l.count;
            out.counts.right = r.count;
            progress(&out.counts);
        }
        let order = match (&l.current, &r.current) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((a, _)), Some((b, _))) => compare_keys(a, b),
        };
        match order {
            Ordering::Less => {
                if let Some((_, vals)) = l.current.take() {
                    out.counts.only_left += 1;
                    if out.only_left.len() < max_kept {
                        out.only_left.push(vals);
                    } else {
                        out.truncated = true;
                    }
                }
                l.advance()?;
            }
            Ordering::Greater => {
                if let Some((_, vals)) = r.current.take() {
                    out.counts.only_right += 1;
                    if out.only_right.len() < max_kept {
                        out.only_right.push(vals);
                    } else {
                        out.truncated = true;
                    }
                }
                r.advance()?;
            }
            Ordering::Equal => {
                if let (Some((key, a)), Some((_, b))) = (l.current.take(), r.current.take()) {
                    let changed: Vec<usize> = a
                        .iter()
                        .enumerate()
                        .filter(|(i, x)| !same_value(x, b.get(*i).unwrap_or(&Cell::Null)))
                        .map(|(i, _)| i)
                        .collect();
                    if changed.is_empty() {
                        out.counts.same += 1;
                    } else {
                        out.counts.different += 1;
                        if out.different.len() < max_kept {
                            out.different.push(RowDiff { key, left: a, right: b, changed });
                        } else {
                            out.truncated = true;
                        }
                    }
                }
                l.advance()?;
                r.advance()?;
            }
        }
    }
    out.counts.left = l.count;
    out.counts.right = r.count;
    progress(&out.counts);
    Ok(out)
}

/// Statements that make the right table equal to the left: INSERT the rows only on the left,
/// DELETE the rows only on the right, UPDATE the differing columns. Refuses a truncated result
/// (some differences were not kept).
pub fn sync_script(kind: DbKind, schema: &str, table: &str, result: &CompareResult) -> DbResult<Vec<String>> {
    if result.truncated {
        return Err(DbError::Unsupported(
            "too many differences were found to keep them all; narrow the comparison (a WHERE) and compare again"
                .into(),
        ));
    }
    let nk = result.key_columns.len();
    let named = |vals: &[Cell], idx: &mut dyn Iterator<Item = usize>| -> Vec<(String, Cell)> {
        idx.filter_map(|i| Some((result.columns.get(i)?.clone(), vals.get(i)?.clone()))).collect()
    };
    let mut changes = Vec::new();
    for row in &result.only_right {
        changes.push(RowChange::Delete { key: named(row, &mut (0..nk)) });
    }
    for d in &result.different {
        changes.push(RowChange::Update {
            key: named(&d.right, &mut (0..nk)),
            set: named(&d.left, &mut d.changed.iter().copied()),
        });
    }
    for row in &result.only_left {
        changes.push(RowChange::Insert { values: named(row, &mut (0..row.len())) });
    }
    crate::edit::statements(kind, schema, table, &changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(n: &[&str]) -> Vec<Column> {
        n.iter().map(|n| Column { name: (*n).into(), type_name: String::new() }).collect()
    }

    fn src(c: &[&str], rows: Vec<Vec<Cell>>) -> VecSource {
        VecSource { columns: cols(c), rows: rows.into_iter() }
    }

    fn t(s: &str) -> Cell {
        Cell::Text(s.into())
    }

    #[test]
    fn values_and_keys_compare_across_types() {
        assert!(same_value(&Cell::Int(1), &Cell::Float(1.0)));
        assert!(same_value(&Cell::Other("1.50".into()), &Cell::Other("1.5".into())));
        assert!(!same_value(&Cell::Null, &t("")));
        assert!(same_value(&Cell::Null, &Cell::Null));
        assert!(!same_value(&t("1"), &t("1.0")));
        assert_eq!(compare_cell(&Cell::Int(9), &Cell::UInt(10)), Ordering::Less);
        assert_eq!(compare_cell(&Cell::Null, &Cell::Int(0)), Ordering::Less);
        assert_eq!(compare_cell(&t("B"), &t("a")), Ordering::Less);
        assert_eq!(compare_cell(&Cell::Int(i64::MAX), &Cell::UInt(u64::MAX)), Ordering::Less);
        assert!(is_text_type("varchar(20)") && is_text_type("TEXT") && !is_text_type("int4"));
        assert_eq!(
            ordered_query(
                DbKind::Postgres,
                "select * from t;",
                &["id".into(), "n".into()],
                &["int4".into(), "text".into()]
            ),
            "SELECT * FROM (select * from t\n) AS q ORDER BY q.\"id\" NULLS FIRST, q.\"n\" COLLATE \"C\" NULLS FIRST"
        );
        assert!(
            ordered_query(DbKind::Mysql, "t", &["n".into()], &["varchar".into()]).ends_with("CAST(q.`n` AS BINARY)")
        );
    }

    #[test]
    fn merge_finds_every_kind_of_difference() {
        let mut l = src(
            &["id", "name", "extra"],
            vec![
                vec![Cell::Int(1), t("a"), Cell::Null],
                vec![Cell::Int(2), t("b"), Cell::Null],
                vec![Cell::Int(4), t("d"), Cell::Null],
            ],
        );
        let mut r = src(
            &["ID", "name"],
            vec![vec![Cell::Int(2), t("B")], vec![Cell::Int(3), t("c")], vec![Cell::Int(4), t("d")]],
        );
        let cancel = AtomicBool::new(false);
        let res = compare(&mut l, &mut r, &["id".into()], 100, &cancel, &mut |_| {}).unwrap();
        assert_eq!(res.columns, ["id", "name"]);
        assert_eq!(res.left_only_columns, ["extra"]);
        assert_eq!(res.counts, CompareCounts { left: 3, right: 3, only_left: 1, only_right: 1, different: 1, same: 1 });
        assert_eq!(res.only_left, vec![vec![Cell::Int(1), t("a")]]);
        assert_eq!(res.different[0].changed, vec![1]);
        let script = sync_script(DbKind::Sqlite, "", "t", &res).unwrap();
        assert_eq!(
            script,
            [
                "DELETE FROM \"t\" WHERE \"id\" = 3",
                "UPDATE \"t\" SET \"name\" = 'b' WHERE \"id\" = 2",
                "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, 'a')"
            ]
        );
        // Identical sides.
        let mut a = src(&["id"], vec![vec![Cell::Int(1)]]);
        let mut b = src(&["id"], vec![vec![Cell::Int(1)]]);
        assert!(compare(&mut a, &mut b, &["id".into()], 10, &cancel, &mut |_| {}).unwrap().counts.identical());
    }

    #[test]
    fn caps_disorder_duplicates_and_missing_keys() {
        let cancel = AtomicBool::new(false);
        let rows = |n: i64| (0..n).map(|i| vec![Cell::Int(i)]).collect::<Vec<_>>();
        let mut a = src(&["id"], rows(10));
        let mut b = src(&["id"], Vec::new());
        let res = compare(&mut a, &mut b, &["id".into()], 3, &cancel, &mut |_| {}).unwrap();
        assert_eq!((res.only_left.len(), res.counts.only_left, res.truncated), (3, 10, true));
        assert!(sync_script(DbKind::Sqlite, "", "t", &res).is_err());
        let mut a = src(&["id"], vec![vec![Cell::Int(2)], vec![Cell::Int(1)]]);
        let mut b = src(&["id"], Vec::new());
        let e = compare(&mut a, &mut b, &["id".into()], 3, &cancel, &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("not in key order"), "{e}");
        let mut a = src(&["id"], vec![vec![Cell::Int(1)], vec![Cell::Int(1)]]);
        let mut b = src(&["id"], Vec::new());
        let e = compare(&mut a, &mut b, &["id".into()], 3, &cancel, &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("two rows"), "{e}");
        let mut a = src(&["id"], Vec::new());
        let mut b = src(&["x"], Vec::new());
        assert!(compare(&mut a, &mut b, &["id".into()], 3, &cancel, &mut |_| {}).is_err());
        assert!(compare(&mut a, &mut b, &[], 3, &cancel, &mut |_| {}).is_err());
    }

    #[test]
    fn streams_two_sqlite_databases() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, sql: &[&str]| {
            let cfg = crate::DataSourceConfig {
                kind: DbKind::Sqlite,
                path: Some(dir.path().join(name)),
                ..Default::default()
            };
            let mut c = crate::connect(&cfg, None).unwrap();
            for s in sql {
                crate::query_all(c.as_mut(), s).unwrap();
            }
            cfg
        };
        let numbers = "with recursive n(i) as (select 1 union all select i + 1 from n where i < 3000)";
        let a = mk(
            "a.db",
            &[
                "create table t (id integer primary key, v text)",
                &format!("{numbers} insert into t select i, 'v' || i from n"),
            ],
        );
        let b = mk(
            "b.db",
            &[
                "create table t (id integer primary key, v text)",
                &format!("{numbers} insert into t select i, 'v' || i from n where i % 1000 <> 0"),
                "update t set v = 'changed' where id = 1500",
                "insert into t values (5000, 'new')",
            ],
        );
        let sql = ordered_query(DbKind::Sqlite, "select * from t", &["id".into()], &["integer".into()]);
        let (ca, cb) = (a.clone(), b.clone());
        let mut l = RowStream::spawn(move || crate::connect(&ca, None), sql.clone(), 100).unwrap();
        let mut r = RowStream::spawn(move || crate::connect(&cb, None), sql, 100).unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let res = compare(&mut l, &mut r, &["id".into()], 100, &cancel, &mut |_| calls += 1).unwrap();
        assert_eq!(res.counts.left, 3000);
        assert_eq!((res.counts.only_left, res.counts.only_right, res.counts.different), (3, 1, 1));
        assert!(calls >= 3);
        // Apply the sync script to the right side: now identical.
        let mut c = crate::connect(&b, None).unwrap();
        for s in sync_script(DbKind::Sqlite, "", "t", &res).unwrap() {
            crate::query_all(c.as_mut(), &s).unwrap();
        }
        let (ca, cb) = (a.clone(), b.clone());
        let sql = ordered_query(DbKind::Sqlite, "select * from t", &["id".into()], &[]);
        let mut l = RowStream::spawn(move || crate::connect(&ca, None), sql.clone(), 100).unwrap();
        let mut r = RowStream::spawn(move || crate::connect(&cb, None), sql, 100).unwrap();
        assert!(compare(&mut l, &mut r, &["id".into()], 100, &cancel, &mut |_| {}).unwrap().counts.identical());
        // A failing side reports its error.
        let ca = a.clone();
        let mut bad = RowStream::spawn(move || crate::connect(&ca, None), "select * from missing".into(), 10).unwrap();
        let mut empty = src(&["id"], Vec::new());
        assert!(compare(&mut bad, &mut empty, &["id".into()], 10, &cancel, &mut |_| {}).is_err());
    }
}
