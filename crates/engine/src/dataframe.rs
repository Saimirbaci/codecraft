//! View as DataFrame / Array (M12 track PY): a paged grid over a pandas `DataFrame`/`Series`,
//! a numpy array or a list of rows living in the Python Console or a notebook kernel.
//!
//! The data never comes over whole: each page (rows `start..start+count`, columns
//! `colStart..colStart+colCount`) is asked of the process with a `frame` request answered by
//! [`INSPECT_PY`] (shared by the console and kernel drivers). Sorting and filtering happen in
//! the process (vectorized for pandas/numpy), cached there until the next execution. The
//! expression may be any Python expression (`df`, `df[df.a > 1]`, `arr[0]`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_db::{Cell, Column};
use codecraft_workspace::DocId;

/// Rows per page by default.
pub const DEFAULT_PAGE_SIZE: usize = 100;
/// Most rows asked at once.
pub const MAX_PAGE_SIZE: usize = 2000;
/// Columns asked at once.
pub const COLUMN_WINDOW: usize = 100;
/// Views kept open.
const MAX_VIEWS: usize = 20;

/// Python helpers shared by the console and kernel drivers: variable descriptions
/// (`_cc_var`) and DataFrame pages (`_cc_frame`).
pub const INSPECT_PY: &str = r#"
_cc_cache = {}
def _cc_short(v, n=200):
    try:
        r = repr(v)
    except Exception as e:
        r = "<repr failed: %s>" % type(e).__name__
    return r if len(r) <= n else r[:n - 3] + "..."
def _cc_kind(v):
    t = type(v)
    mod = getattr(t, "__module__", "") or ""
    name = t.__name__
    if mod.startswith("pandas") and name in ("DataFrame", "Series"):
        return name
    if mod.startswith("numpy") and name == "ndarray":
        return "ndarray"
    if isinstance(v, (list, tuple)) and len(v) > 0 and all(isinstance(r, (list, tuple, dict)) for r in v[:100]):
        return "rows"
    return None
def _cc_shape(v, kind):
    try:
        if kind in ("DataFrame", "Series", "ndarray"):
            return "(" + ", ".join(str(int(x)) for x in v.shape) + ("," if len(v.shape) == 1 else "") + ")"
        if kind == "rows":
            return "(%d, %d)" % (len(v), len(_cc_rows_columns(v)))
    except Exception:
        pass
    return None
def _cc_var(k, v):
    d = {"name": k, "type": type(v).__name__, "value": _cc_short(v)}
    try:
        kind = _cc_kind(v)
    except Exception:
        kind = None
    if kind is not None and not (kind == "ndarray" and getattr(v, "ndim", 0) not in (1, 2)):
        d["viewable"] = True
        s = _cc_shape(v, kind)
        if s:
            d["shape"] = s
    return d
def _cc_rows_columns(v):
    cols = []
    seen = set()
    width = 0
    for r in v[:1000]:
        if isinstance(r, dict):
            for k in r.keys():
                if k not in seen and len(cols) < 1000:
                    seen.add(k)
                    cols.append(k)
        else:
            width = max(width, min(len(r), 1000))
    if cols:
        return cols
    return list(range(width))
def _cc_cell(x):
    if x is None or isinstance(x, bool):
        return x
    if hasattr(x, "item") and not isinstance(x, (str, bytes)):
        try:
            x = x.item()
        except Exception:
            pass
    if isinstance(x, bool) or x is None:
        return x
    if isinstance(x, int):
        return x if -2**63 <= x < 2**63 else str(x)
    if isinstance(x, float):
        return x if x == x and x not in (float("inf"), float("-inf")) else str(x)
    s = x if isinstance(x, str) else str(x)
    return s if len(s) <= 1000 else s[:997] + "..."
def _cc_sk(v):
    if isinstance(v, bool):
        return (0, float(v), "")
    if isinstance(v, (int, float)):
        return (0, v, "") if v == v else (1, 0, "nan")
    return (1, 0, str(v))
class _CCTable:
    pass
def _cc_table(v, kind):
    t = _CCTable()
    t.kind = kind
    if kind == "Series":
        v = v.to_frame(name=str(v.name) if v.name is not None else "value")
        kind = "DataFrame"
    if kind == "DataFrame":
        t.df = v
        t.columns = [str(c) for c in v.columns]
        t.dtypes = [str(x) for x in v.dtypes]
        t.n = len(v)
        t.index_name = "" if v.index.name is None else str(v.index.name)
        t.pandas = True
        return t
    t.pandas = False
    if kind == "ndarray":
        if v.ndim not in (1, 2):
            raise ValueError("View as Array shows 1-D and 2-D arrays; slice it first (for example a[0])")
        names = v.dtype.names
        if names:
            t.columns = [str(n) for n in names]
            t.dtypes = [str(v.dtype.fields[n][0]) for n in names]
            t.get = lambda i, j: v[i][names[j]]
        else:
            a2 = v.reshape(-1, 1) if v.ndim == 1 else v
            t.columns = [str(i) for i in range(a2.shape[1])]
            t.dtypes = [str(v.dtype)] * a2.shape[1]
            t.get = lambda i, j: a2[i, j]
            t.a2 = a2
        t.n = int(v.shape[0])
        t.index_name = ""
        return t
    cols = _cc_rows_columns(v)
    def get(i, j):
        r = v[i]
        k = cols[j]
        if isinstance(r, dict):
            return r.get(k)
        if isinstance(k, int) and k < len(r):
            return r[k]
        return None
    t.get = get
    t.columns = [str(c) for c in cols]
    types = []
    for j in range(len(cols)):
        names = set()
        for i in range(min(len(v), 100)):
            x = get(i, j)
            if x is not None:
                names.add(type(x).__name__)
        types.append(names.pop() if len(names) == 1 else "object")
    t.dtypes = types
    t.n = len(v)
    t.index_name = ""
    return t
def _cc_order(t, sort, asc, flt, fcol):
    if t.pandas:
        import numpy as np
        df = t.df
        pos = np.arange(t.n)
        if flt:
            sub = df if fcol is None else df.iloc[:, [fcol]]
            mask = sub.astype(str).apply(lambda c: c.str.contains(flt, case=False, regex=False, na=False)).any(axis=1).to_numpy()
            pos = pos[mask]
        if sort is not None:
            key = df.iloc[pos, sort].reset_index(drop=True)
            try:
                o = key.sort_values(ascending=asc, kind="mergesort", na_position="last").index.to_numpy()
            except TypeError:
                o = key.astype(str).sort_values(ascending=asc, kind="mergesort").index.to_numpy()
            pos = pos[o]
        return pos
    ncols = len(t.columns)
    pos = list(range(t.n))
    if flt:
        if t.n * max(ncols, 1) > 5000000:
            raise ValueError("too large to filter here (more than 5,000,000 cells)")
        q = flt.lower()
        js = range(ncols) if fcol is None else [fcol]
        pos = [i for i in pos if any(q in str(t.get(i, j)).lower() for j in js)]
    if sort is not None:
        if t.n > 5000000:
            raise ValueError("too large to sort here (more than 5,000,000 rows)")
        vals = [(i, t.get(i, sort)) for i in pos]
        nn = [p for p in vals if p[1] is not None]
        nones = [p[0] for p in vals if p[1] is None]
        nn.sort(key=lambda p: _cc_sk(p[1]), reverse=not asc)
        pos = [p[0] for p in nn] + nones
    return pos
def _cc_frame(m, ns):
    req = m.get("req")
    try:
        expr = str(m.get("expr", "")).strip()
        if not expr:
            raise ValueError("no expression")
        v = ns[expr] if expr in ns else eval(expr, ns)
        kind = _cc_kind(v)
        if kind is None:
            raise TypeError("%s is a %s, not a DataFrame, Series, array or list of rows" % (expr, type(v).__name__))
        t = _cc_table(v, kind)
        ncols = len(t.columns)
        sort = m.get("sort")
        sort = int(sort) if isinstance(sort, int) and 0 <= sort < ncols else None
        asc = bool(m.get("asc", True))
        flt = str(m.get("filter") or "")
        fcol = m.get("filterColumn")
        fcol = int(fcol) if isinstance(fcol, int) and 0 <= fcol < ncols else None
        key = (expr, id(v), sort, asc, flt, fcol)
        c = _cc_cache.get(expr)
        if c is not None and c[0] == key:
            pos = c[1]
        else:
            pos = None if sort is None and not flt else _cc_order(t, sort, asc, flt, fcol)
            if len(_cc_cache) > 8:
                _cc_cache.clear()
            _cc_cache[expr] = (key, pos)
        total = t.n if pos is None else len(pos)
        start = max(0, min(int(m.get("start", 0) or 0), total))
        count = max(0, min(int(m.get("count", 100) or 0), 2000))
        c0 = max(0, min(int(m.get("colStart", 0) or 0), ncols))
        c1 = min(ncols, c0 + max(1, min(int(m.get("colCount", 100) or 100), 500)))
        page = list(range(start, min(total, start + count))) if pos is None else list(pos[start:start + count])
        if t.pandas:
            sub = t.df.iloc[page, c0:c1]
            labels = [str(x) for x in sub.index]
            rows = [[_cc_cell(x) for x in r] for r in sub.itertuples(index=False, name=None)]
        else:
            labels = [str(int(i)) for i in page]
            rows = [[_cc_cell(t.get(int(i), j)) for j in range(c0, c1)] for i in page]
        shape = [int(x) for x in v.shape] if hasattr(v, "shape") else [t.n, ncols]
        return {"ev": "frame", "req": req, "ok": True, "expr": expr, "kind": t.kind, "type": type(v).__name__,
                "shape": shape, "total": int(total), "start": start, "colStart": c0, "colsTotal": ncols,
                "columns": t.columns[c0:c1], "dtypes": t.dtypes[c0:c1], "indexName": t.index_name,
                "labels": labels, "rows": rows}
    except BaseException as e:
        return {"ev": "frame", "req": req, "ok": False, "error": "%s: %s" % (type(e).__name__, e)}
"#;

/// Where the viewed value lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum FrameSource {
    /// The Python Console.
    Console,
    /// A notebook's kernel.
    Notebook { doc: u64 },
}

impl FrameSource {
    pub fn notebook(doc: DocId) -> Self {
        Self::Notebook { doc: doc.0 }
    }
}

/// One View as DataFrame/Array window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameView {
    pub id: u64,
    pub source: FrameSource,
    pub expr: String,
    /// `DataFrame`, `Series`, `ndarray`, `rows` (once loaded).
    pub kind: String,
    /// The Python type name.
    pub type_name: String,
    pub shape: Vec<u64>,
    /// Rows after the filter.
    pub total: u64,
    pub start: u64,
    pub page_size: usize,
    pub col_start: usize,
    pub cols_total: usize,
    pub columns: Vec<String>,
    pub dtypes: Vec<String>,
    pub index_name: String,
    /// Row labels (index values) of the page.
    pub labels: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// `(column, ascending)` over all columns.
    pub sort: Option<(usize, bool)>,
    pub filter: String,
    pub filter_column: Option<usize>,
    /// A request is on its way (its id).
    pub loading: Option<u64>,
    pub error: Option<String>,
}

impl FrameView {
    pub fn new(id: u64, source: FrameSource, expr: &str) -> Self {
        Self {
            id,
            source,
            expr: expr.trim().to_string(),
            kind: String::new(),
            type_name: String::new(),
            shape: Vec::new(),
            total: 0,
            start: 0,
            page_size: DEFAULT_PAGE_SIZE,
            col_start: 0,
            cols_total: 0,
            columns: Vec::new(),
            dtypes: Vec::new(),
            index_name: String::new(),
            labels: Vec::new(),
            rows: Vec::new(),
            sort: None,
            filter: String::new(),
            filter_column: None,
            loading: None,
            error: None,
        }
    }

    /// The `frame` request for the current page.
    pub fn request(&self, req: u64) -> Value {
        json!({
            "op": "frame",
            "req": req,
            "expr": self.expr,
            "start": self.start,
            "count": self.page_size.clamp(1, MAX_PAGE_SIZE),
            "colStart": self.col_start,
            "colCount": COLUMN_WINDOW,
            "sort": self.sort.map(|(c, _)| c),
            "asc": self.sort.is_none_or(|(_, a)| a),
            "filter": self.filter,
            "filterColumn": self.filter_column,
        })
    }

    /// Take a `frame` answer. Returns false when it is not for this view's pending request.
    pub fn apply(&mut self, v: &Value) -> bool {
        let req = v.get("req").and_then(Value::as_u64);
        if req.is_none() || req != self.loading {
            return false;
        }
        self.loading = None;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            self.error = Some(v.get("error").and_then(Value::as_str).unwrap_or("cannot read the value").to_string());
            return true;
        }
        let strs = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|x| x.as_str().map(str::to_owned).unwrap_or_else(|| x.to_string())).collect())
                .unwrap_or_default()
        };
        let num = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.error = None;
        self.kind = v.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
        self.type_name = v.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        self.shape = v.get("shape").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        self.total = num("total");
        self.start = num("start");
        self.col_start = usize::try_from(num("colStart")).unwrap_or(0);
        self.cols_total = usize::try_from(num("colsTotal")).unwrap_or(0);
        self.columns = strs("columns");
        self.dtypes = strs("dtypes");
        self.index_name = v.get("indexName").and_then(Value::as_str).unwrap_or("").to_string();
        self.labels = strs("labels");
        self.rows = v
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().take(MAX_PAGE_SIZE).map(|r| r.as_array().cloned().unwrap_or_default()).collect())
            .unwrap_or_default();
        true
    }

    /// The page as grid columns (the index first) and cells.
    pub fn grid(&self) -> (Vec<Column>, Vec<Vec<Cell>>) {
        let mut columns = vec![Column {
            name: if self.index_name.is_empty() { String::new() } else { self.index_name.clone() },
            type_name: "index".into(),
        }];
        columns.extend(self.columns.iter().enumerate().map(|(i, name)| Column {
            name: name.clone(),
            type_name: self.dtypes.get(i).cloned().unwrap_or_default(),
        }));
        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut row = vec![Cell::Other(self.labels.get(i).cloned().unwrap_or_default())];
                row.extend(r.iter().map(cell));
                row
            })
            .collect();
        (columns, rows)
    }

    /// `1–100 of 1,000 rows` for the status line.
    pub fn summary(&self) -> String {
        let end = self.start.saturating_add(self.rows.len() as u64);
        let shape = self.shape.iter().map(u64::to_string).collect::<Vec<_>>().join(" × ");
        let filtered = if self.filter.is_empty() { String::new() } else { format!(" (filtered from {shape})") };
        let cols = if self.cols_total > self.columns.len() {
            format!(
                ", columns {}–{} of {}",
                self.col_start + 1,
                self.col_start + self.columns.len(),
                self.cols_total
            )
        } else {
            String::new()
        };
        if self.total == 0 {
            return format!("0 rows{filtered}");
        }
        format!("{}–{end} of {} rows{filtered}{cols} · {shape}", self.start + 1, self.total)
    }

    pub fn page_count(&self) -> u64 {
        let size = self.page_size.max(1) as u64;
        self.total.div_ceil(size).max(1)
    }

    pub fn page(&self) -> u64 {
        self.start / self.page_size.max(1) as u64
    }
}

fn cell(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Cell::Int(i),
            (_, Some(u), _) => Cell::UInt(u),
            (_, _, Some(f)) => Cell::Float(f),
            _ => Cell::Other(n.to_string()),
        },
        Value::String(s) => Cell::Text(s.clone()),
        other => Cell::Other(other.to_string()),
    }
}

/// The open viewers.
#[derive(Debug, Default)]
pub struct FramesState {
    pub views: Vec<FrameView>,
    /// Index of the shown view.
    pub active: usize,
    next_id: u64,
}

impl FramesState {
    /// A new view (the oldest is dropped beyond the limit); returns its index.
    pub fn open(&mut self, source: FrameSource, expr: &str) -> usize {
        if let Some(i) = self.views.iter().position(|v| v.source == source && v.expr == expr.trim()) {
            self.active = i;
            return i;
        }
        if self.views.len() >= MAX_VIEWS {
            self.views.remove(0);
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.views.push(FrameView::new(self.next_id, source, expr));
        self.active = self.views.len() - 1;
        self.active
    }

    pub fn active(&self) -> Option<&FrameView> {
        self.views.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut FrameView> {
        self.views.get_mut(self.active)
    }

    /// A request id.
    pub fn next_req(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// Give a `frame` answer to the view waiting for it.
    pub fn deliver(&mut self, v: &Value) -> bool {
        self.views.iter_mut().any(|view| view.apply(v))
    }

    pub fn close(&mut self, index: usize) -> bool {
        if index >= self.views.len() {
            return false;
        }
        self.views.remove(index);
        self.active = self.active.min(self.views.len().saturating_sub(1));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_answers() {
        let mut st = FramesState::default();
        let i = st.open(FrameSource::Console, " df ");
        assert_eq!(st.open(FrameSource::Console, "df"), i, "the same value reuses its view");
        let req = st.next_req();
        let view = st.active_mut().unwrap();
        view.sort = Some((2, false));
        view.loading = Some(req);
        let r = view.request(req);
        assert_eq!(r["expr"], "df");
        assert_eq!(r["sort"], 2);
        assert_eq!(r["asc"], false);
        // Answers for other requests are ignored.
        assert!(!st.deliver(&json!({"req": req + 1, "ok": true})));
        let answer = json!({"ev": "frame", "req": req, "ok": true, "kind": "DataFrame", "type": "DataFrame",
            "shape": [1000, 3], "total": 1000, "start": 0, "colStart": 0, "colsTotal": 3,
            "columns": ["a", "b", 7], "dtypes": ["int64", "object", "float64"], "indexName": "",
            "labels": ["0", "1"], "rows": [[1, "x", 1.5], [null, true, {"o": 1}]]});
        assert!(st.deliver(&answer));
        let v = st.active().unwrap();
        assert_eq!(v.columns, vec!["a", "b", "7"]);
        assert_eq!(v.summary(), "1–2 of 1000 rows · 1000 × 3");
        assert_eq!(v.page_count(), 10);
        let (cols, rows) = v.grid();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[1].type_name, "int64");
        assert_eq!(rows[0], vec![Cell::Other("0".into()), Cell::Int(1), Cell::Text("x".into()), Cell::Float(1.5)]);
        assert_eq!(rows[1][1], Cell::Null);
        assert_eq!(rows[1][2], Cell::Bool(true));
        // Errors.
        let req = st.next_req();
        st.active_mut().unwrap().loading = Some(req);
        assert!(st.deliver(&json!({"req": req, "ok": false, "error": "NameError: x"})));
        assert_eq!(st.active().unwrap().error.as_deref(), Some("NameError: x"));
        assert!(!st.deliver(&json!("garbage")));
        assert!(st.close(0));
        assert!(!st.close(0));
        assert!(st.active().is_none());
    }
}
