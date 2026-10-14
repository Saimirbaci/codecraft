//! SQL inspections with quick-fixes (pure): UPDATE/DELETE without WHERE, INSERT value counts,
//! `SELECT *` in a view, unused aliases and CTEs, ambiguous and unresolved columns (with a
//! schema). `-- noinspection Id[, Id]` before a statement suppresses ids for that statement
//! (none or `All`: every inspection); `IdForFile` / `AllForFile` anywhere suppresses for the
//! whole text. Offsets are char offsets in the SQL text.

use serde::Serialize;

use codecraft_syntax::sql_scope::{self, SqlSchema, SqlToken, Statement};
use codecraft_workspace::Severity;

pub const RESOLVE: &str = "SqlResolve";
pub const WITHOUT_WHERE: &str = "SqlWithoutWhere";
pub const AMBIGUOUS: &str = "SqlAmbiguousColumn";
pub const INSERT_VALUES: &str = "SqlInsertValues";
pub const STAR_IN_VIEW: &str = "SqlSelectStarInView";
pub const UNUSED_ALIAS: &str = "SqlUnusedAlias";
pub const UNUSED_CTE: &str = "SqlUnusedCte";

/// Every inspection id (for settings and documentation).
pub const ALL: &[&str] = &[RESOLVE, WITHOUT_WHERE, AMBIGUOUS, INSERT_VALUES, STAR_IN_VIEW, UNUSED_ALIAS, UNUSED_CTE];

/// Texts above this many chars are not inspected (only resolved names are cheap enough).
const MAX_LINT_CHARS: usize = 2_000_000;

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlProblem {
    pub start: usize,
    pub end: usize,
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    /// Inspection data for the quick-fixes (the qualifiers of an ambiguous column).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<String>,
}

/// A text change in SQL coordinates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// A quick-fix: edits applied together, then the caret (SQL offset, after the edits).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuickFix {
    pub title: String,
    pub code: &'static str,
    pub edits: Vec<SqlEdit>,
    pub caret: Option<usize>,
}

/// The tokens of one statement (split at `;`, like [`sql_scope::analyze`]) with its names.
struct Seg<'a> {
    toks: &'a [SqlToken],
    st: Option<&'a Statement>,
}

impl Seg<'_> {
    fn start(&self) -> usize {
        self.toks.first().map_or(0, |t| t.start)
    }

    fn end(&self) -> usize {
        self.toks.last().map_or(0, |t| t.end)
    }

    /// Paren depth before each token.
    fn depths(&self) -> Vec<usize> {
        let mut d = 0usize;
        self.toks
            .iter()
            .map(|t| {
                if t.is_sym(')') {
                    d = d.saturating_sub(1);
                }
                let here = d;
                if t.is_sym('(') {
                    d += 1;
                }
                here
            })
            .collect()
    }

    /// Index of the `)` closing the `(` at `open`.
    fn close_of(&self, open: usize) -> Option<usize> {
        let mut d = 0usize;
        for (k, t) in self.toks.iter().enumerate().skip(open) {
            if t.is_sym('(') {
                d += 1;
            } else if t.is_sym(')') {
                d = d.saturating_sub(1);
                if d == 0 {
                    return Some(k);
                }
            }
        }
        None
    }

    /// Items of the parenthesized list at `open` (top-level commas + 1; 0 when empty).
    fn items(&self, open: usize) -> Option<(usize, usize)> {
        let close = self.close_of(open)?;
        if close == open + 1 {
            return Some((0, close));
        }
        let mut d = 0usize;
        let mut n = 1;
        for t in self.toks.get(open + 1..close)? {
            if t.is_sym('(') {
                d += 1;
            } else if t.is_sym(')') {
                d = d.saturating_sub(1);
            } else if d == 0 && t.is_sym(',') {
                n += 1;
            }
        }
        Some((n, close))
    }
}

fn segments<'a>(toks: &'a [SqlToken], stmts: &'a [Statement]) -> Vec<Seg<'a>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (k, t) in toks.iter().enumerate() {
        if t.is_sym(';') {
            if k > start
                && let Some(seg) = toks.get(start..k)
            {
                out.push(seg);
            }
            start = k + 1;
        }
    }
    if let Some(rest) = toks.get(start..).filter(|r| !r.is_empty()) {
        out.push(rest);
    }
    out.into_iter()
        .map(|toks| {
            let first = toks.first().map_or(0, |t| t.start);
            Seg { toks, st: stmts.iter().find(|s| s.start == first) }
        })
        .collect()
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn problem(start: usize, end: usize, code: &'static str, severity: Severity, message: String) -> SqlProblem {
    SqlProblem { start, end, code, severity, message, data: Vec::new() }
}

// ---------------------------------------------------------------- inspections

/// UPDATE or DELETE without a WHERE clause: (problem, where the clause goes).
fn without_where(seg: &Seg) -> Option<SqlProblem> {
    let depths = seg.depths();
    let first = seg.toks.first()?;
    // Past a WITH list: the first top-level data statement word.
    let verb = if first.is_word("with") {
        seg.toks.iter().enumerate().skip(1).find(|(k, t)| {
            depths.get(*k) == Some(&0) && ["select", "insert", "update", "delete", "merge"].iter().any(|w| t.is_word(w))
        })?
    } else {
        (0, first)
    };
    let (vi, vt) = verb;
    let is_update = vt.is_word("update");
    if !(is_update || vt.is_word("delete")) {
        return None;
    }
    let has_where = seg.toks.iter().enumerate().skip(vi).any(|(k, t)| depths.get(k) == Some(&0) && t.is_word("where"));
    if has_where {
        return None;
    }
    let table_end = seg
        .st
        .and_then(|st| st.tables.iter().find(|t| t.start > vt.start))
        .map_or(vt.end, |t| t.alias_span.map_or(t.end, |(_, e)| e));
    let message = if is_update {
        "Unsafe query: 'UPDATE' statement without 'WHERE' updates all rows of the table"
    } else {
        "Unsafe query: 'DELETE' statement without 'WHERE' clears all data in the table"
    };
    Some(problem(vt.start, table_end.max(vt.end), WITHOUT_WHERE, Severity::Warning, message.into()))
}

/// INSERT whose rows have another number of values than the column list.
fn insert_values(seg: &Seg, out: &mut Vec<SqlProblem>) {
    let depths = seg.depths();
    let toks = seg.toks;
    let Some(into) = toks.iter().enumerate().position(|(k, t)| {
        depths.get(k) == Some(&0)
            && t.is_word("into")
            && k.checked_sub(1)
                .and_then(|p| toks.get(p))
                .is_some_and(|p| p.is_word("insert") || p.is_word("replace") || p.is_word("ignore"))
    }) else {
        return;
    };
    // The target: name(.name)* [AS alias] (.
    let mut k = into + 1;
    if toks.get(k).and_then(|t| t.name(true)).is_none() {
        return;
    }
    while toks.get(k + 1).is_some_and(|t| t.is_sym('.')) && toks.get(k + 2).and_then(|t| t.name(true)).is_some() {
        k += 2;
    }
    k += 1;
    if toks.get(k).is_some_and(|t| t.is_word("as")) {
        k += 2;
    }
    if !toks.get(k).is_some_and(|t| t.is_sym('(')) {
        return;
    }
    let Some((columns, close)) = seg.items(k) else { return };
    if columns == 0 {
        return;
    }
    let mut p = close + 1;
    let Some(next) = toks.get(p) else { return };
    if next.is_word("values") || next.is_word("value") {
        p += 1;
        while toks.get(p).is_some_and(|t| t.is_sym('(')) {
            let Some((n, end)) = seg.items(p) else { return };
            if n != columns {
                let (a, b) = (toks.get(p).map_or(0, |t| t.start), toks.get(end).map_or(0, |t| t.end));
                out.push(problem(a, b, INSERT_VALUES, Severity::Warning, count_message(n, columns)));
            }
            p = end + 1;
            if !toks.get(p).is_some_and(|t| t.is_sym(',')) {
                break;
            }
            p += 1;
        }
    } else if next.is_word("select") {
        // INSERT … SELECT: the select list at this depth, up to FROM (or the end).
        let mut n = 1;
        let mut d = 0usize;
        let mut end = p;
        for (j, t) in toks.iter().enumerate().skip(p + 1) {
            if t.is_sym('(') {
                d += 1;
            } else if t.is_sym(')') {
                if d == 0 {
                    break;
                }
                d -= 1;
            } else if d == 0 {
                if t.is_sym('*')
                    && toks.get(j - 1).is_some_and(|q| q.is_sym(',') || q.is_word("select") || q.is_sym('.'))
                {
                    return;
                }
                if ["from", "where", "union", "group", "order", "limit", "on", "returning"].iter().any(|w| t.is_word(w))
                {
                    break;
                }
                if t.is_sym(',') {
                    n += 1;
                }
            }
            end = j;
        }
        if n != columns {
            let (a, b) = (next.start, toks.get(end).map_or(next.end, |t| t.end));
            out.push(problem(a, b, INSERT_VALUES, Severity::Warning, count_message(n, columns)));
        }
    }
}

fn count_message(values: usize, columns: usize) -> String {
    let plural = |n: usize, w: &str| if n == 1 { format!("1 {w}") } else { format!("{n} {w}s") };
    format!("INSERT has {} for {}", plural(values, "value"), plural(columns, "target column"))
}

/// `*` positions in the select lists of a `CREATE VIEW … AS SELECT`.
fn star_positions(seg: &Seg) -> Vec<usize> {
    let toks = seg.toks;
    if !toks.first().is_some_and(|t| t.is_word("create")) {
        return Vec::new();
    }
    let depths = seg.depths();
    let Some(view) = toks.iter().take(8).position(|t| t.is_word("view")) else { return Vec::new() };
    let Some(as_at) =
        toks.iter().enumerate().skip(view).position(|(k, t)| depths.get(k) == Some(&0) && t.is_word("as"))
    else {
        return Vec::new();
    };
    let as_at = as_at + view;
    toks.iter()
        .enumerate()
        .skip(as_at + 1)
        .filter(|(k, t)| {
            t.is_sym('*')
                && k.checked_sub(1)
                    .and_then(|p| toks.get(p))
                    .is_some_and(|p| p.is_word("select") || p.is_word("distinct") || p.is_sym(',') || p.is_sym('.'))
                && toks.get(k + 1).is_none_or(|n| n.is_sym(',') || n.is_word("from") || n.is_sym(')'))
        })
        .map(|(k, _)| k)
        .collect()
}

fn star_in_view(seg: &Seg, out: &mut Vec<SqlProblem>) {
    for k in star_positions(seg) {
        let Some(t) = seg.toks.get(k) else { continue };
        let start = match k.checked_sub(2).and_then(|q| seg.toks.get(q)) {
            Some(q) if seg.toks.get(k - 1).is_some_and(|d| d.is_sym('.')) => q.start,
            _ => t.start,
        };
        out.push(problem(
            start,
            t.end,
            STAR_IN_VIEW,
            Severity::Info,
            "'SELECT *' in a view: the view's columns are fixed when it is created".into(),
        ));
    }
}

/// Tokens of the statement naming `name` (case-insensitively), other than `except`.
fn uses(seg: &Seg, name: &str, except: (usize, usize), skip_after_dot: bool) -> usize {
    seg.toks
        .iter()
        .enumerate()
        .filter(|(k, t)| {
            (t.start, t.end) != except
                && t.name(true).is_some_and(|n| eq(&n, name))
                && !(skip_after_dot && k.checked_sub(1).and_then(|p| seg.toks.get(p)).is_some_and(|p| p.is_sym('.')))
        })
        .count()
}

fn unused(seg: &Seg, out: &mut Vec<SqlProblem>) {
    let Some(st) = seg.st else { return };
    for tr in &st.tables {
        let (Some(alias), Some(span)) = (&tr.alias, tr.alias_span) else { continue };
        if uses(seg, alias, span, true) == 0 {
            out.push(problem(span.0, span.1, UNUSED_ALIAS, Severity::Info, format!("Unused alias '{alias}'")));
        }
    }
    for (name, span) in st.ctes.iter().zip(&st.cte_spans) {
        if uses(seg, name, *span, true) == 0 {
            out.push(problem(
                span.0,
                span.1,
                UNUSED_CTE,
                Severity::Info,
                format!("Common table expression '{name}' is never used"),
            ));
        }
    }
}

fn has_column(cols: &[sql_scope::SqlColumn], name: &str) -> bool {
    cols.iter().any(|c| eq(&c.name, name))
}

/// Unqualified column references found in more than one FROM table (statements without
/// subqueries or CTEs, where the scope is certain).
fn ambiguous(seg: &Seg, schema: &dyn SqlSchema, out: &mut Vec<SqlProblem>) {
    let Some(st) = seg.st else { return };
    if !st.is_dml() || st.opaque || !st.ctes.is_empty() || !st.derived.is_empty() {
        return;
    }
    let toks = seg.toks;
    // One query level only: SELECT, UPDATE or DELETE without subqueries.
    let selects = toks.iter().filter(|t| t.is_word("select")).count();
    let own = usize::from(st.verb == "select");
    if !matches!(st.verb.as_str(), "select" | "update" | "delete")
        || selects != own
        || toks.iter().any(|t| t.is_word("natural"))
    {
        return;
    }
    let mut sources: Vec<(String, Vec<sql_scope::SqlColumn>)> = Vec::new();
    for tr in &st.tables {
        let Some(cols) = table_columns(schema, tr.schema.as_deref(), &tr.name) else { return };
        sources.push((tr.alias.clone().unwrap_or_else(|| tr.name.clone()), cols));
    }
    if sources.len() < 2 {
        return;
    }
    // Columns joined with USING, INSERT target lists and UPDATE SET targets are not ambiguous.
    let mut exempt: Vec<(usize, usize)> = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        let group = (t.is_word("using") || t.is_word("into") && toks.get(k + 2).is_some_and(|n| n.is_sym('(')))
            .then(|| if t.is_word("using") { k + 1 } else { k + 2 });
        if let Some(open) = group.filter(|o| toks.get(*o).is_some_and(|x| x.is_sym('(')))
            && let Some(close) = seg.close_of(open)
            && let (Some(a), Some(b)) = (toks.get(open), toks.get(close))
        {
            exempt.push((a.start, b.end));
        }
    }
    // Columns joined with USING are one column everywhere in the statement.
    let joined: Vec<String> = exempt
        .iter()
        .flat_map(|&(a, b)| toks.iter().filter(move |t| a < t.start && t.end < b).filter_map(|t| t.name(true)))
        .collect();
    let is_set_target = |c: &sql_scope::ColumnRef| {
        let Some(k) = toks.iter().position(|t| t.start == c.start) else { return false };
        toks.get(k + 1).is_some_and(|n| n.is_sym('='))
            && k.checked_sub(1).and_then(|p| toks.get(p)).is_some_and(|p| p.is_word("set") || p.is_sym(','))
            && st.verb == "update"
    };
    for c in &st.columns {
        if c.qualifier.is_some()
            || exempt.iter().any(|&(a, b)| a <= c.start && c.end <= b)
            || st.aliases.iter().any(|a| eq(a, &c.name))
            || joined.iter().any(|j| eq(j, &c.name))
            || is_set_target(c)
        {
            continue;
        }
        let owners: Vec<String> =
            sources.iter().filter(|(_, cols)| has_column(cols, &c.name)).map(|(q, _)| q.clone()).collect();
        if owners.len() >= 2 {
            let mut p = problem(
                c.start,
                c.end,
                AMBIGUOUS,
                Severity::Warning,
                format!("Ambiguous column reference '{}' (in {})", c.name, owners.join(", ")),
            );
            p.data = owners;
            out.push(p);
        }
    }
}

/// Columns of a table: in its schema, else the default, else any schema that has it.
pub(crate) fn table_columns(schema: &dyn SqlSchema, sc: Option<&str>, name: &str) -> Option<Vec<sql_scope::SqlColumn>> {
    let find =
        |s: Option<&str>| -> Option<Option<String>> { Some(schema.tables(s)?.into_iter().find(|t| eq(t, name))) };
    match sc {
        Some(q) => {
            let real = schema.schemas().into_iter().find(|s| eq(s, q))?;
            let t = find(Some(&real))??;
            schema.columns(Some(&real), &t)
        }
        None => {
            if let Some(Some(t)) = find(None) {
                return schema.columns(None, &t);
            }
            for s in schema.schemas() {
                if let Some(Some(t)) = find(Some(&s)) {
                    return schema.columns(Some(&s), &t);
                }
            }
            None
        }
    }
}

// ---------------------------------------------------------------- suppression

/// Ids suppressed for the whole text, and per statement (`*`: all).
fn suppressions(sql: &str, segs: &[Seg]) -> (Vec<String>, Vec<Vec<String>>) {
    let mut file = Vec::new();
    let mut per: Vec<Vec<String>> = vec![Vec::new(); segs.len()];
    for (start, _, text) in sql_scope::comments(sql) {
        let body = text.trim_start_matches(['-', '/', '*', '#']).trim_end_matches(['*', '/']).trim();
        let Some(rest) = body.strip_prefix("noinspection") else { continue };
        let ids: Vec<String> =
            rest.split([',', ' ', '\t']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
        let ids = if ids.is_empty() { vec!["*".to_string()] } else { ids };
        let mut local = Vec::new();
        for id in ids {
            match id.strip_suffix("ForFile") {
                Some(f) => file.push(if f.is_empty() || f == "All" { "*".to_string() } else { f.to_string() }),
                None => local.push(if id == "All" { "*".to_string() } else { id }),
            }
        }
        // The statement this comment stands before (or in).
        if let Some(k) = segs.iter().position(|s| s.end() >= start)
            && let Some(slot) = per.get_mut(k)
        {
            slot.extend(local);
        }
    }
    (file, per)
}

fn suppressed(list: &[String], code: &str) -> bool {
    list.iter().any(|i| i == "*" || i == code)
}

// ---------------------------------------------------------------- entry points

/// Every problem of `sql`: the schema-free inspections always, unresolved and ambiguous names
/// with a `schema`. Suppressed ones are left out.
pub fn check(sql: &str, schema: Option<&dyn SqlSchema>) -> Vec<SqlProblem> {
    let mut out = Vec::new();
    if sql.len() > MAX_LINT_CHARS * 4 || sql.chars().count() > MAX_LINT_CHARS {
        return out;
    }
    let toks = sql_scope::tokens(sql);
    let stmts = sql_scope::analyze(sql);
    let segs = segments(&toks, &stmts);
    for seg in &segs {
        if let Some(p) = without_where(seg) {
            out.push(p);
        }
        insert_values(seg, &mut out);
        star_in_view(seg, &mut out);
        unused(seg, &mut out);
        if let Some(schema) = schema {
            ambiguous(seg, schema, &mut out);
        }
    }
    if let Some(schema) = schema {
        for i in sql_scope::check(sql, schema) {
            out.push(problem(i.start, i.end, RESOLVE, Severity::Warning, i.message));
        }
    }
    let (file, per) = suppressions(sql, &segs);
    out.retain(|p| {
        let seg = segs.iter().position(|s| s.start() <= p.start && p.start <= s.end());
        !suppressed(&file, p.code) && !seg.and_then(|k| per.get(k)).is_some_and(|l| suppressed(l, p.code))
    });
    out.sort_by_key(|p| (p.start, p.end));
    out.dedup();
    out
}

/// The quick-fixes of `p` (found by [`check`] in `sql`), suppression included.
pub fn fixes(sql: &str, p: &SqlProblem, schema: Option<&dyn SqlSchema>) -> Vec<QuickFix> {
    let toks = sql_scope::tokens(sql);
    let stmts = sql_scope::analyze(sql);
    let segs = segments(&toks, &stmts);
    let mut out = Vec::new();
    let Some(seg) = segs.iter().find(|s| s.start() <= p.start && p.start <= s.end()) else { return out };
    let fix = |title: String, edits: Vec<SqlEdit>, caret: Option<usize>| QuickFix { title, code: p.code, edits, caret };
    match p.code {
        WITHOUT_WHERE => {
            let depths = seg.depths();
            let tail = seg.toks.iter().enumerate().find(|(k, t)| {
                t.start > p.end
                    && depths.get(*k) == Some(&0)
                    && ["returning", "order", "limit"].iter().any(|w| t.is_word(w))
            });
            let (at, text, caret) = match tail {
                Some((_, t)) => (t.start, "WHERE  ".to_string(), t.start + 6),
                None => (seg.end(), " WHERE ".to_string(), seg.end() + 7),
            };
            out.push(fix("Add WHERE clause".into(), vec![SqlEdit { start: at, end: at, text }], Some(caret)));
        }
        AMBIGUOUS => {
            for q in &p.data {
                let text = format!("{}.", qualifier_text(q));
                let caret = p.end + text.chars().count();
                out.push(fix(
                    format!("Qualify with '{q}'"),
                    vec![SqlEdit { start: p.start, end: p.start, text }],
                    Some(caret),
                ));
            }
        }
        UNUSED_ALIAS => {
            if let Some(tr) = seg.st.and_then(|st| st.tables.iter().find(|t| t.alias_span == Some((p.start, p.end)))) {
                out.push(fix(
                    "Remove alias".into(),
                    vec![SqlEdit { start: tr.end, end: p.end, text: String::new() }],
                    Some(tr.end),
                ));
            }
        }
        UNUSED_CTE => {
            if let Some(range) = cte_range(seg, (p.start, p.end)) {
                out.push(fix(
                    "Remove common table expression".into(),
                    vec![SqlEdit { start: range.0, end: range.1, text: String::new() }],
                    Some(range.0),
                ));
            }
        }
        STAR_IN_VIEW => {
            if let Some(schema) = schema
                && let Some(text) = expand_star(seg, p, schema)
            {
                let caret = p.start + text.chars().count();
                out.push(fix(
                    "Expand column list".into(),
                    vec![SqlEdit { start: p.start, end: p.end, text }],
                    Some(caret),
                ));
            }
        }
        _ => {}
    }
    // Suppression: for the statement (a comment line above it) and for the whole text.
    let chars: Vec<char> = sql.chars().collect();
    let s0 = seg.start();
    let mut line_start = s0;
    while line_start > 0 && chars.get(line_start - 1).is_some_and(|c| *c != '\n') {
        line_start -= 1;
    }
    let indent: String = chars.get(line_start..s0).unwrap_or(&[]).iter().collect();
    let comment = format!("-- noinspection {}", p.code);
    let edit = if indent.chars().all(char::is_whitespace) {
        SqlEdit { start: line_start, end: line_start, text: format!("{indent}{comment}\n") }
    } else {
        SqlEdit { start: s0, end: s0, text: format!("\n{comment}\n") }
    };
    out.push(fix("Suppress for statement".into(), vec![edit], None));
    out.push(fix(
        "Suppress for file".into(),
        vec![SqlEdit { start: 0, end: 0, text: format!("-- noinspection {}ForFile\n", p.code) }],
        None,
    ));
    out
}

fn qualifier_text(q: &str) -> String {
    let plain = q.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && q.chars().all(|c| c.is_alphanumeric() || c == '_');
    if plain { q.to_string() } else { format!("\"{}\"", q.replace('"', "\"\"")) }
}

/// The text to delete with an unused CTE: `name AS (…)` with its comma, or the whole WITH.
fn cte_range(seg: &Seg, def: (usize, usize)) -> Option<(usize, usize)> {
    let toks = seg.toks;
    let d = toks.iter().position(|t| (t.start, t.end) == def)?;
    let mut k = d + 1;
    if toks.get(k).is_some_and(|t| t.is_sym('(')) {
        k = seg.close_of(k)? + 1;
    }
    if !toks.get(k).is_some_and(|t| t.is_word("as")) {
        return None;
    }
    k += 1;
    while toks.get(k).is_some_and(|t| t.is_word("not") || t.is_word("materialized")) {
        k += 1;
    }
    if !toks.get(k).is_some_and(|t| t.is_sym('(')) {
        return None;
    }
    let close = seg.close_of(k)?;
    let end_tok = toks.get(close)?;
    if toks.get(close + 1).is_some_and(|t| t.is_sym(',')) {
        let next = toks.get(close + 2)?;
        return Some((def.0, next.start));
    }
    let prev = d.checked_sub(1).and_then(|p| toks.get(p))?;
    if prev.is_sym(',') {
        return Some((prev.start, end_tok.end));
    }
    // The only CTE: remove `WITH [RECURSIVE] name AS (…)` up to the statement.
    let with = toks.iter().take(d).position(|t| t.is_word("with"))?;
    let next = toks.get(close + 1)?;
    Some((toks.get(with)?.start, next.start))
}

/// `*` (or `t.*`) replaced by the columns of the FROM tables.
fn expand_star(seg: &Seg, p: &SqlProblem, schema: &dyn SqlSchema) -> Option<String> {
    let st = seg.st?;
    let toks = seg.toks;
    let k = toks.iter().position(|t| t.start == p.start)?;
    let qualified = toks.get(k + 1).is_some_and(|t| t.is_sym('.'));
    let tables: Vec<&sql_scope::TableRef> = if qualified {
        let q = toks.get(k)?.name(true)?;
        vec![st.tables.iter().find(|t| t.alias.as_deref().is_some_and(|a| eq(a, &q)) || eq(&t.name, &q))?]
    } else {
        st.tables.iter().collect()
    };
    if tables.is_empty() {
        return None;
    }
    let mut cols = Vec::new();
    for t in &tables {
        let list = table_columns(schema, t.schema.as_deref(), &t.name)?;
        let q = t.alias.clone().unwrap_or_else(|| t.name.clone());
        for c in list {
            if tables.len() > 1 || qualified {
                cols.push(format!("{}.{}", qualifier_text(&q), qualifier_text(&c.name)));
            } else {
                cols.push(qualifier_text(&c.name));
            }
        }
    }
    (!cols.is_empty()).then(|| cols.join(", "))
}

/// Apply SQL edits to a text (for tests and previews); `None` when they overlap or are out of
/// range.
pub fn apply(sql: &str, edits: &[SqlEdit]) -> Option<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut sorted: Vec<&SqlEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| (e.start, e.end));
    let mut out = String::new();
    let mut at = 0usize;
    for e in sorted {
        if e.start < at || e.end < e.start || e.end > chars.len() {
            return None;
        }
        out.extend(chars.get(at..e.start)?);
        out.push_str(&e.text);
        at = e.end;
    }
    out.extend(chars.get(at..)?);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_syntax::sql_scope::SqlColumn;
    use std::collections::BTreeMap;

    struct Fake(BTreeMap<&'static str, Vec<&'static str>>);

    impl SqlSchema for Fake {
        fn tables(&self, schema: Option<&str>) -> Option<Vec<String>> {
            (schema.is_none() || schema == Some("main")).then(|| self.0.keys().map(|k| k.to_string()).collect())
        }
        fn columns(&self, _: Option<&str>, table: &str) -> Option<Vec<SqlColumn>> {
            self.0
                .get(table)
                .map(|c| c.iter().map(|n| SqlColumn { name: n.to_string(), data_type: "int".into() }).collect())
        }
        fn schemas(&self) -> Vec<String> {
            vec!["main".into()]
        }
    }

    fn fake() -> Fake {
        let mut m = BTreeMap::new();
        m.insert("users", vec!["id", "name"]);
        m.insert("orders", vec!["id", "user_id", "total"]);
        Fake(m)
    }

    fn codes(sql: &str) -> Vec<&'static str> {
        check(sql, Some(&fake())).into_iter().map(|p| p.code).collect()
    }

    fn fixed(sql: &str, code: &str, title: &str) -> String {
        let f = fake();
        let p = check(sql, Some(&f)).into_iter().find(|p| p.code == code).unwrap();
        let fx = fixes(sql, &p, Some(&f)).into_iter().find(|x| x.title == title).unwrap();
        apply(sql, &fx.edits).unwrap()
    }

    #[test]
    fn update_and_delete_without_where() {
        assert_eq!(codes("DELETE FROM users"), [WITHOUT_WHERE]);
        assert_eq!(codes("update users set name = 'x'"), [WITHOUT_WHERE]);
        assert!(codes("DELETE FROM users WHERE id = 1").is_empty());
        assert!(codes("UPDATE users SET name = (SELECT max(total) FROM orders WHERE id = 1) WHERE id = 2").is_empty());
        // A WHERE inside a subquery does not count.
        assert_eq!(codes("UPDATE users SET name = (SELECT 'a' FROM orders WHERE id = 1)"), [WITHOUT_WHERE]);
        assert_eq!(codes("WITH x AS (SELECT 1) DELETE FROM users"), [UNUSED_CTE, WITHOUT_WHERE]);
        assert_eq!(fixed("DELETE FROM users;", WITHOUT_WHERE, "Add WHERE clause"), "DELETE FROM users WHERE ;");
        assert_eq!(
            fixed("DELETE FROM users RETURNING id", WITHOUT_WHERE, "Add WHERE clause"),
            "DELETE FROM users WHERE  RETURNING id"
        );
    }

    #[test]
    fn insert_value_counts() {
        assert_eq!(codes("INSERT INTO users (id, name) VALUES (1, 'a', 3)"), [INSERT_VALUES]);
        assert_eq!(codes("INSERT INTO users (id, name) VALUES (1, 'a'), (2)"), [INSERT_VALUES]);
        assert!(codes("INSERT INTO users (id, name) VALUES (1, coalesce('a', 'b')), (2, 'c')").is_empty());
        assert_eq!(codes("INSERT INTO users (id, name) SELECT id FROM orders"), [INSERT_VALUES]);
        assert!(codes("INSERT INTO users (id, name) SELECT id, total FROM orders").is_empty());
        assert!(codes("INSERT INTO users SELECT * FROM orders").is_empty());
        let p = check("INSERT INTO users (id) VALUES (1, 2)", None);
        assert_eq!(p[0].message, "INSERT has 2 values for 1 target column");
    }

    #[test]
    fn select_star_in_views_and_expansion() {
        assert_eq!(codes("CREATE VIEW v AS SELECT * FROM users"), [STAR_IN_VIEW]);
        assert_eq!(codes("CREATE OR REPLACE VIEW v AS SELECT u.* FROM users u"), [STAR_IN_VIEW]);
        assert!(codes("CREATE VIEW v AS SELECT count(*) FROM users").is_empty());
        assert!(codes("SELECT * FROM users").is_empty());
        assert_eq!(
            fixed("CREATE VIEW v AS SELECT * FROM users", STAR_IN_VIEW, "Expand column list"),
            "CREATE VIEW v AS SELECT id, name FROM users"
        );
        assert_eq!(
            fixed(
                "CREATE VIEW v AS SELECT u.* FROM users u JOIN orders o ON o.user_id = u.id",
                STAR_IN_VIEW,
                "Expand column list"
            ),
            "CREATE VIEW v AS SELECT u.id, u.name FROM users u JOIN orders o ON o.user_id = u.id"
        );
    }

    #[test]
    fn unused_aliases_and_ctes() {
        assert_eq!(codes("SELECT id FROM users u"), [UNUSED_ALIAS]);
        assert!(codes("SELECT u.id FROM users u").is_empty());
        assert_eq!(
            fixed("SELECT id FROM users AS u WHERE id = 1", UNUSED_ALIAS, "Remove alias"),
            "SELECT id FROM users WHERE id = 1"
        );
        assert_eq!(codes("WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM b"), [UNUSED_CTE]);
        assert_eq!(
            fixed(
                "WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM b",
                UNUSED_CTE,
                "Remove common table expression"
            ),
            "WITH b AS (SELECT 2) SELECT * FROM b"
        );
        assert_eq!(
            fixed(
                "WITH b AS (SELECT 2), a AS (SELECT 1) SELECT * FROM b",
                UNUSED_CTE,
                "Remove common table expression"
            ),
            "WITH b AS (SELECT 2) SELECT * FROM b"
        );
        assert_eq!(fixed("WITH a AS (SELECT 1) SELECT 2", UNUSED_CTE, "Remove common table expression"), "SELECT 2");
    }

    #[test]
    fn ambiguous_columns_with_a_schema() {
        assert_eq!(codes("SELECT id FROM users u JOIN orders o ON o.user_id = u.id"), [AMBIGUOUS]);
        assert!(codes("SELECT name, total FROM users u JOIN orders o ON o.user_id = u.id").is_empty());
        assert!(codes("SELECT id FROM users JOIN orders USING (id)").is_empty());
        assert!(check("SELECT id FROM users u JOIN orders o ON true", None).iter().all(|p| p.code != AMBIGUOUS));
        assert_eq!(
            fixed("SELECT id FROM users u JOIN orders o ON o.user_id = u.id", AMBIGUOUS, "Qualify with 'o'"),
            "SELECT o.id FROM users u JOIN orders o ON o.user_id = u.id"
        );
        // Unresolved names come from the schema check.
        assert_eq!(codes("SELECT nope FROM users"), [RESOLVE]);
    }

    #[test]
    fn suppression_comments() {
        assert!(codes("-- noinspection SqlWithoutWhere\nDELETE FROM users").is_empty());
        assert!(codes("-- noinspection\nDELETE FROM users").is_empty());
        assert_eq!(codes("-- noinspection SqlWithoutWhere\nDELETE FROM users; DELETE FROM orders"), [WITHOUT_WHERE]);
        assert!(codes("DELETE FROM users;\n-- noinspection AllForFile\nDELETE FROM orders").is_empty());
        let s = fixed("SELECT 1;\n  DELETE FROM users", WITHOUT_WHERE, "Suppress for statement");
        assert_eq!(s, "SELECT 1;\n  -- noinspection SqlWithoutWhere\n  DELETE FROM users");
        assert!(codes(&s).is_empty());
        let s = fixed("DELETE FROM users", WITHOUT_WHERE, "Suppress for file");
        assert!(codes(&s).is_empty());
    }

    #[test]
    fn hostile_input_never_panics() {
        let f = fake();
        for sql in [
            "",
            ";",
            "(",
            ")",
            "DELETE",
            "UPDATE",
            "INSERT INTO",
            "INSERT INTO t (",
            "INSERT INTO t (a) VALUES (",
            "INSERT INTO t (a) SELECT",
            "CREATE VIEW",
            "CREATE VIEW v AS",
            "CREATE VIEW v AS SELECT *",
            "WITH",
            "WITH a AS",
            "WITH a AS (",
            "WITH a(x AS (",
            "SELECT 'unterminated",
            "SELECT $$ dollar",
            "SELECT \"q",
            "-- noinspection",
            "/* noinspection SqlResolveForFile",
            "é ü ñ; DELETE FROM é",
        ] {
            for p in check(sql, Some(&f)) {
                for fx in fixes(sql, &p, Some(&f)) {
                    let _ = apply(sql, &fx.edits);
                }
            }
        }
        let big = "DELETE FROM users; ".repeat(5000);
        assert_eq!(check(&big, None).len(), 5000);
    }
}
