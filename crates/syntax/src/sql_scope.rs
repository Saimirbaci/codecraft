//! Names in SQL text: the tables (with aliases and CTEs) and columns each statement refers to,
//! checked against a schema ([`SqlSchema`]) for unresolved names, and the completion context at
//! a caret (keywords, tables, alias-aware columns).
//!
//! This is lexical analysis over the SQL tokens, deliberately lenient: anything it cannot be
//! sure about (derived tables, table functions, CTE columns, unknown qualifiers, schemas that
//! are not loaded) produces no warning. Offsets are char offsets in the SQL text.

use serde::Serialize;

use crate::HighlightKind as K;
use crate::common::Doc;
use crate::lex::{self, Tok, Tt};
use crate::sql;

/// A table referenced by a statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
    /// Char range of the table name.
    pub start: usize,
    pub end: usize,
}

/// A column reference (`name` or `qualifier.name`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ColumnRef {
    pub qualifier: Option<String>,
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// The names of one statement.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Statement {
    /// Char range of the statement (without the `;`).
    pub start: usize,
    pub end: usize,
    /// The first word, lower-cased (`select`, `insert`, `with`, `create`, …).
    pub verb: String,
    pub tables: Vec<TableRef>,
    pub columns: Vec<ColumnRef>,
    /// Names defined by `WITH name AS (…)`.
    pub ctes: Vec<String>,
    /// Column aliases (`AS name`, `expr name`).
    pub aliases: Vec<String>,
    /// Aliases of subqueries and table functions in FROM (their columns are unknown).
    pub derived: Vec<String>,
    /// A FROM item whose columns are unknown (subquery, table function): unqualified column
    /// names cannot be checked.
    pub opaque: bool,
}

impl Statement {
    /// Whether this statement reads or writes rows (the statements names are checked in).
    pub fn is_dml(&self) -> bool {
        matches!(self.verb.as_str(), "select" | "insert" | "update" | "delete" | "with" | "replace")
    }
}

/// A column of a table in the schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlColumn {
    pub name: String,
    pub data_type: String,
}

/// What the checks and completion know about the database.
pub trait SqlSchema {
    /// Tables (and views) of `schema` (the default schema for `None`); `None` while unknown.
    fn tables(&self, schema: Option<&str>) -> Option<Vec<String>>;
    /// Columns of `table` (a name from [`SqlSchema::tables`]); `None` while unknown.
    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<SqlColumn>>;
    /// Schema names.
    fn schemas(&self) -> Vec<String>;
}

/// An unresolved name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlIssue {
    pub start: usize,
    pub end: usize,
    pub message: String,
}

/// Columns every row has without being declared.
const PSEUDO_COLUMNS: &[&str] = &["rowid", "oid", "_rowid_", "ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// Words that are not columns although the highlighter does not know them as keywords
/// (`EXTRACT(epoch FROM …)`, `COLLATE nocase`, `TRIM(both …)`, trigger rows, …).
const NOT_COLUMNS: &[&str] = &[
    "epoch",
    "nocase",
    "rtrim",
    "both",
    "leading",
    "trailing",
    "unknown",
    "localtime",
    "localtimestamp",
    "session_user",
    "current_role",
    "current_schema",
    "current_catalog",
    "excluded",
    "new",
    "old",
    "utc",
    "day",
    "month",
    "hour",
    "minute",
    "second",
    "week",
    "quarter",
    "dow",
    "doy",
    "isodow",
    "century",
    "decade",
    "millennium",
    "microseconds",
    "milliseconds",
    "timezone",
];

/// Statement and clause keywords offered by completion.
pub const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "RIGHT JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "ON",
    "USING",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "AS",
    "DISTINCT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "UNION",
    "UNION ALL",
    "WITH",
    "ASC",
    "DESC",
    "TRUE",
    "FALSE",
    "DEFAULT",
    "ON CONFLICT",
    "DO NOTHING",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "PRIMARY KEY",
    "REFERENCES",
];

/// Words that end a FROM item (never a table name or an alias).
fn is_clause_word(w: &[u8]) -> bool {
    matches!(
        w,
        b"select"
            | b"from"
            | b"where"
            | b"join"
            | b"inner"
            | b"left"
            | b"right"
            | b"full"
            | b"cross"
            | b"natural"
            | b"outer"
            | b"on"
            | b"using"
            | b"set"
            | b"values"
            | b"group"
            | b"order"
            | b"having"
            | b"limit"
            | b"offset"
            | b"union"
            | b"intersect"
            | b"except"
            | b"as"
            | b"lateral"
            | b"only"
            | b"default"
            | b"window"
            | b"returning"
            | b"into"
            | b"with"
            | b"when"
            | b"then"
            | b"else"
            | b"end"
            | b"not"
            | b"and"
            | b"or"
            | b"in"
            | b"exists"
            | b"if"
            | b"conflict"
            | b"do"
            | b"fetch"
            | b"for"
    )
}

/// The significant (non-comment) tokens of an SQL text.
struct Toks {
    doc: Doc,
    ascii: Vec<u8>,
    toks: Vec<Tok>,
}

impl Toks {
    fn new(text: &str) -> Self {
        let doc = Doc::new(text);
        let ascii = lex::ascii(&doc, true);
        let (all, _) = sql::tokens(&doc);
        let toks = all.into_iter().filter(|t| !t.is_comment()).collect();
        Self { doc, ascii, toks }
    }

    fn len(&self) -> usize {
        self.toks.len()
    }

    fn tok(&self, p: usize) -> Option<&Tok> {
        self.toks.get(p)
    }

    fn sym(&self, p: usize) -> Option<char> {
        match self.tok(p)?.tt {
            Tt::Sym(c) => Some(c),
            _ => None,
        }
    }

    fn is_sym(&self, p: usize, c: char) -> bool {
        self.sym(p) == Some(c)
    }

    /// Lower-cased word at `p` (empty for other tokens).
    fn word(&self, p: usize) -> &[u8] {
        match self.tok(p) {
            Some(t) if t.tt == Tt::Word => self.ascii.get(t.start..t.end).unwrap_or(&[]),
            _ => &[],
        }
    }

    fn is_word(&self, p: usize, w: &str) -> bool {
        self.word(p) == w.as_bytes()
    }

    fn kind(&self, p: usize) -> Option<K> {
        self.tok(p).map(|t| t.kind)
    }

    fn text(&self, p: usize) -> String {
        self.tok(p).map(|t| self.doc.string(t.start, t.end)).unwrap_or_default()
    }

    fn range(&self, p: usize) -> (usize, usize) {
        self.tok(p).map_or((0, 0), |t| (t.start, t.end))
    }

    /// The name at `p`: an identifier (keywords too with `keywords`) or a quoted identifier,
    /// unquoted.
    fn name(&self, p: usize, keywords: bool) -> Option<String> {
        let t = self.tok(p)?;
        match t.tt {
            Tt::Word => {
                let ok = match t.kind {
                    K::Ident | K::Field | K::Function => true,
                    K::Keyword | K::Type => keywords && !is_clause_word(self.word(p)),
                    _ => false,
                };
                ok.then(|| self.text(p))
            }
            Tt::Other if t.kind == K::Ident => {
                let s = self.text(p);
                let q = s.chars().next().filter(|c| matches!(c, '"' | '`'))?;
                let inner = s.strip_prefix(q)?.strip_suffix(q)?;
                let doubled: String = [q, q].iter().collect();
                Some(inner.replace(&doubled, &q.to_string()))
            }
            _ => None,
        }
    }

    /// For each `(`, the position of its `)`.
    fn group_ends(&self) -> Vec<Option<usize>> {
        let mut ends = vec![None; self.len()];
        let mut stack = Vec::new();
        for p in 0..self.len() {
            match self.sym(p) {
                Some('(') => stack.push(p),
                Some(')') => {
                    if let Some(open) = stack.pop()
                        && let Some(slot) = ends.get_mut(open)
                    {
                        *slot = Some(p);
                    }
                }
                _ => {}
            }
        }
        ends
    }

    /// Statement ranges of significant positions, split at `;`.
    fn statements(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut start = 0;
        for p in 0..self.len() {
            if self.is_sym(p, ';') {
                if p > start {
                    out.push((start, p));
                }
                start = p + 1;
            }
        }
        if self.len() > start {
            out.push((start, self.len()));
        }
        out
    }
}

/// The statements of `sql` with their names.
pub fn analyze(sql: &str) -> Vec<Statement> {
    let t = Toks::new(sql);
    let ends = t.group_ends();
    t.statements().into_iter().map(|(a, b)| Analyzer::new(&t, &ends, a, b).run()).collect()
}

struct Analyzer<'a> {
    t: &'a Toks,
    ends: &'a [Option<usize>],
    a: usize,
    b: usize,
    /// Positions that are table names or aliases (not columns).
    consumed: Vec<bool>,
    st: Statement,
}

impl<'a> Analyzer<'a> {
    fn new(t: &'a Toks, ends: &'a [Option<usize>], a: usize, b: usize) -> Self {
        let (start, _) = t.range(a);
        let (_, end) = t.range(b.saturating_sub(1).max(a));
        let st =
            Statement { start, end, verb: String::from_utf8_lossy(t.word(a)).into_owned(), ..Statement::default() };
        Self { t, ends, a, b, consumed: vec![false; b.saturating_sub(a)], st }
    }

    fn consume(&mut self, p: usize) {
        if let Some(c) = p.checked_sub(self.a).and_then(|i| self.consumed.get_mut(i)) {
            *c = true;
        }
    }

    fn is_consumed(&self, p: usize) -> bool {
        p.checked_sub(self.a).and_then(|i| self.consumed.get(i)).copied().unwrap_or(true)
    }

    fn run(mut self) -> Statement {
        let t = self.t;
        // Innermost open paren: is it a function call?
        let mut groups: Vec<bool> = Vec::new();
        let mut p = self.a;
        while p < self.b {
            match t.sym(p) {
                Some('(') => {
                    let call = p
                        .checked_sub(1)
                        .is_some_and(|q| t.kind(q) == Some(K::Function) && t.tok(q).is_some_and(|x| x.tt == Tt::Word));
                    groups.push(call);
                }
                Some(')') => {
                    groups.pop();
                }
                _ => {}
            }
            let in_call = groups.last().copied().unwrap_or(false);
            let prev = p.checked_sub(1).filter(|q| *q >= self.a);
            let prev_word = prev.map_or(&[][..], |q| t.word(q));
            // `name AS (` defines a CTE.
            if t.is_word(p + 1, "as") && t.is_sym(p + 2, '(') && !self.is_consumed(p) {
                if let Some(n) = t.name(p, false) {
                    self.st.ctes.push(n);
                    self.consume(p);
                }
            } else if t.is_sym(p + 1, '(')
                && let Some(close) = self.ends.get(p + 1).copied().flatten()
                && t.is_word(close + 1, "as")
                && t.is_sym(close + 2, '(')
                && t.kind(p) != Some(K::Function)
                && !self.is_consumed(p)
                && let Some(n) = t.name(p, false)
            {
                self.st.ctes.push(n);
                self.consume(p);
            }
            // Item lists are recorded here; the scan still walks through them (subqueries in
            // FROM have names of their own).
            match t.word(p) {
                b"from" if !in_call && prev_word != b"distinct" => {
                    self.source_items(p + 1, true);
                }
                b"join" if !in_call => {
                    self.source_items(p + 1, false);
                }
                b"into"
                    if matches!(
                        prev_word,
                        b"insert" | b"replace" | b"ignore" | b"abort" | b"fail" | b"rollback" | b"merge"
                    ) =>
                {
                    self.target(p + 1);
                }
                b"update" if !t.is_word(p + 1, "set") && (prev.is_none() || prev.is_some_and(|q| t.is_sym(q, ')'))) => {
                    let q = if t.is_word(p + 1, "or") { p + 3 } else { p + 1 };
                    self.source_items(q, false);
                }
                _ => {}
            }
            p += 1;
        }
        self.columns();
        self.st
    }

    /// FROM items starting at `p` (more after commas with `list`); returns the position after.
    fn source_items(&mut self, mut p: usize, list: bool) -> usize {
        let t = self.t;
        loop {
            while t.is_word(p, "only") || t.is_word(p, "lateral") {
                p += 1;
            }
            if p >= self.b {
                return p;
            }
            let q = if t.is_sym(p, '(') {
                // A subquery (its own names are found by the main scan).
                self.st.opaque = true;
                let Some(close) = self.ends.get(p).copied().flatten() else { return p + 1 };
                let (alias, after) = self.alias(close + 1, true);
                if let Some((a, _)) = alias {
                    self.st.derived.push(a);
                }
                after
            } else {
                self.item(p)
            };
            if list && t.is_sym(q, ',') {
                p = q + 1;
                continue;
            }
            return q;
        }
    }

    /// A named FROM item at `p` (table or table function): the position after it.
    fn item(&mut self, p: usize) -> usize {
        let t = self.t;
        let Some((parts, last, mut q)) = self.qualified(p) else { return p };
        {
            if t.is_sym(q, '(') {
                // A table function.
                self.st.opaque = true;
                let close = self.ends.get(q).copied().flatten().unwrap_or(q);
                let (alias, after) = self.alias(close + 1, true);
                if let Some((a, _)) = alias {
                    self.st.derived.push(a);
                }
                q = after;
            } else {
                for k in p..q {
                    self.consume(k);
                }
                let (alias, after) = self.alias(q, true);
                let (start, end) = t.range(last);
                let n = parts.len();
                self.st.tables.push(TableRef {
                    schema: n.checked_sub(2).and_then(|i| parts.get(i)).cloned(),
                    name: parts.last().cloned().unwrap_or_default(),
                    alias: alias.map(|(a, _)| a),
                    start,
                    end,
                });
                q = after;
            }
        }
        q
    }

    /// `INSERT INTO name [AS alias]`: the target table; its column list is scanned as columns.
    fn target(&mut self, p: usize) -> usize {
        let t = self.t;
        let Some((parts, last, q)) = self.qualified(p) else { return p };
        for k in p..q {
            self.consume(k);
        }
        let (alias, after) = if t.is_word(q, "as") { self.alias(q, false) } else { (None, q) };
        let (start, end) = t.range(last);
        let n = parts.len();
        self.st.tables.push(TableRef {
            schema: n.checked_sub(2).and_then(|i| parts.get(i)).cloned(),
            name: parts.last().cloned().unwrap_or_default(),
            alias: alias.map(|(a, _)| a),
            start,
            end,
        });
        after
    }

    /// `name(.name)*` at `p`: the parts, the position of the last part and the position after.
    fn qualified(&self, p: usize) -> Option<(Vec<String>, usize, usize)> {
        let t = self.t;
        let first = t.name(p, true)?;
        let mut parts = vec![first];
        let mut last = p;
        while t.is_sym(last + 1, '.') && parts.len() < 4 {
            let Some(n) = t.name(last + 2, true) else { break };
            parts.push(n);
            last += 2;
        }
        Some((parts, last, last + 1))
    }

    /// `[AS] alias` at `p` (`bare`: without AS too): the alias and the position after.
    fn alias(&mut self, p: usize, bare: bool) -> (Option<(String, usize)>, usize) {
        let t = self.t;
        let (q, explicit) = if t.is_word(p, "as") { (p + 1, true) } else { (p, false) };
        if !explicit && !bare {
            return (None, p);
        }
        if q >= self.b {
            return (None, p);
        }
        match t.name(q, false) {
            Some(a) if t.kind(q) != Some(K::Function) || !t.is_sym(q + 1, '(') => {
                self.consume(q);
                (Some((a, q)), q + 1)
            }
            _ => (None, p),
        }
    }

    /// Column references among the tokens not consumed as table names or aliases.
    fn columns(&mut self) {
        let t = self.t;
        // Whether each position is directly inside a function call's parentheses.
        let mut in_call = vec![false; self.b.saturating_sub(self.a)];
        let mut groups: Vec<bool> = Vec::new();
        for (i, slot) in in_call.iter_mut().enumerate() {
            let p = self.a + i;
            *slot = groups.last().copied().unwrap_or(false);
            match t.sym(p) {
                Some('(') => {
                    groups.push(p.checked_sub(1).is_some_and(|q| q >= self.a && t.kind(q) == Some(K::Function)))
                }
                Some(')') => {
                    groups.pop();
                }
                _ => {}
            }
        }
        let mut p = self.a;
        while p < self.b {
            if self.is_consumed(p) {
                p += 1;
                continue;
            }
            let prev = p.checked_sub(1).filter(|q| *q >= self.a);
            // `qualifier.name` (`a.b.c`: schema.table.column).
            if t.is_sym(p + 1, '.') && t.name(p, true).is_some() && prev.is_none_or(|q| !t.is_sym(q, '.')) {
                let mut q = p;
                while t.is_sym(q + 1, '.') && t.name(q + 2, true).is_some() {
                    q += 2;
                }
                let star = t.is_sym(q + 1, '.') && t.is_sym(q + 2, '*');
                let call = t.is_sym(q + 1, '(');
                if q > p && !star && !call && !self.after_cast(p) {
                    let qualifier = t.name(q - 2, true);
                    let (start, end) = t.range(q);
                    if let Some(name) = t.name(q, true) {
                        self.st.columns.push(ColumnRef { qualifier, name, start, end });
                    }
                }
                p = q + 1;
                continue;
            }
            let Some(name) = t.name(p, false) else {
                p += 1;
                continue;
            };
            let prev_word = prev.map_or(&[][..], |q| t.word(q));
            let alias = prev_word == b"as"
                || prev.is_some_and(|q| {
                    let k = t.tok(q);
                    k.is_some_and(|k| matches!(k.tt, Tt::Lit) || k.kind == K::Number)
                        || t.is_sym(q, ')')
                        || t.word(q) == b"end"
                        || t.name(q, false).is_some() && !t.is_sym(q, '.')
                });
            if alias {
                self.st.aliases.push(name);
                p += 1;
                continue;
            }
            let skip = t.kind(p) == Some(K::Function)
                || t.is_sym(p + 1, '(')
                || prev.is_some_and(|q| t.is_sym(q, '.'))
                || matches!(prev_word, b"collate" | b"over" | b"window" | b"constraint" | b"interval")
                || self.after_cast(p)
                || t.is_sym(p + 1, '=') && t.is_sym(p + 2, '>')
                || t.is_word(p + 1, "from") && in_call.get(p - self.a).copied().unwrap_or(false)
                || NOT_COLUMNS.iter().any(|w| name.eq_ignore_ascii_case(w));
            if !skip {
                let (start, end) = t.range(p);
                self.st.columns.push(ColumnRef { qualifier: None, name, start, end });
            }
            p += 1;
        }
    }

    /// `::` right before `p` (a type name).
    fn after_cast(&self, p: usize) -> bool {
        p.checked_sub(2).is_some_and(|q| q >= self.a && self.t.is_sym(q, ':') && self.t.is_sym(q + 1, ':'))
    }
}

// ---------------------------------------------------------------------------------------------
// Checks

enum Resolved {
    /// (schema it was found in, its name there).
    Found(Option<String>, String),
    Missing,
    Unknown,
}

fn resolve_table(schema: &dyn SqlSchema, qualifier: Option<&str>, name: &str) -> Resolved {
    let find = |sc: Option<&str>| -> Option<Option<String>> {
        let tables = schema.tables(sc)?;
        Some(tables.into_iter().find(|t| t.eq_ignore_ascii_case(name)))
    };
    match qualifier {
        Some(q) => {
            let Some(sc) = schema.schemas().into_iter().find(|s| s.eq_ignore_ascii_case(q)) else {
                return Resolved::Unknown;
            };
            match find(Some(&sc)) {
                Some(Some(t)) => Resolved::Found(Some(sc), t),
                Some(None) => Resolved::Missing,
                None => Resolved::Unknown,
            }
        }
        None => match find(None) {
            Some(Some(t)) => Resolved::Found(None, t),
            Some(None) => {
                // Not in the default schema: another loaded schema may still have it (schemas
                // that are not loaded do not make an unqualified name unknown).
                for sc in schema.schemas() {
                    if let Some(Some(t)) = find(Some(&sc)) {
                        return Resolved::Found(Some(sc), t);
                    }
                }
                Resolved::Missing
            }
            None => Resolved::Unknown,
        },
    }
}

fn has_column(cols: &[SqlColumn], name: &str) -> bool {
    cols.iter().any(|c| c.name.eq_ignore_ascii_case(name))
        || PSEUDO_COLUMNS.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// Unresolved tables and columns of the reading and writing statements of `sql`. Nothing is
/// reported where the schema is not known.
pub fn check(sql: &str, schema: &dyn SqlSchema) -> Vec<SqlIssue> {
    let mut out = Vec::new();
    for st in analyze(sql) {
        if !st.is_dml() {
            continue;
        }
        let is_cte = |n: &str| st.ctes.iter().any(|c| c.eq_ignore_ascii_case(n));
        // Each FROM item: its columns when known.
        let mut sources: Vec<(&TableRef, Option<Vec<SqlColumn>>)> = Vec::new();
        let mut all_known = !st.opaque;
        for tr in &st.tables {
            if tr.schema.is_none() && is_cte(&tr.name) {
                all_known = false;
                continue;
            }
            match resolve_table(schema, tr.schema.as_deref(), &tr.name) {
                Resolved::Found(sc, name) => {
                    let cols = schema.columns(sc.as_deref(), &name);
                    all_known &= cols.is_some();
                    sources.push((tr, cols));
                }
                Resolved::Missing => {
                    all_known = false;
                    out.push(SqlIssue {
                        start: tr.start,
                        end: tr.end,
                        message: format!("Unable to resolve table '{}'", tr.name),
                    });
                }
                Resolved::Unknown => all_known = false,
            }
        }
        for c in &st.columns {
            match &c.qualifier {
                Some(q) => {
                    let by_alias =
                        sources.iter().find(|(tr, _)| tr.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q)));
                    let source = by_alias.or_else(|| sources.iter().find(|(tr, _)| tr.name.eq_ignore_ascii_case(q)));
                    if let Some((_, Some(cols))) = source
                        && !has_column(cols, &c.name)
                    {
                        out.push(column_issue(c));
                    }
                }
                None => {
                    if !all_known || sources.is_empty() {
                        continue;
                    }
                    let known =
                        sources.iter().any(|(_, cols)| cols.as_ref().is_some_and(|cols| has_column(cols, &c.name)))
                            || st.aliases.iter().any(|a| a.eq_ignore_ascii_case(&c.name))
                            || st.tables.iter().any(|t| {
                                t.name.eq_ignore_ascii_case(&c.name)
                                    || t.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&c.name))
                            });
                    if !known {
                        out.push(column_issue(c));
                    }
                }
            }
        }
    }
    out.sort_by_key(|i| (i.start, i.end));
    out.dedup();
    out
}

fn column_issue(c: &ColumnRef) -> SqlIssue {
    SqlIssue { start: c.start, end: c.end, message: format!("Unable to resolve column '{}'", c.name) }
}

// ---------------------------------------------------------------------------------------------
// Completion

/// What completion offers at a caret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SqlItemKind {
    Column,
    Table,
    Schema,
    Keyword,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlItem {
    pub label: String,
    pub kind: SqlItemKind,
    /// Table of a column (`users.text`), schema of a table.
    pub detail: Option<String>,
    /// Lower ranks sort first.
    pub rank: u8,
}

/// Completion at a caret: the items and where the replaced word starts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SqlCompletions {
    /// Char offset where the word being completed starts.
    pub start: usize,
    /// The word typed so far.
    pub prefix: String,
    /// `alias.` before the word.
    pub qualifier: Option<String>,
    pub items: Vec<SqlItem>,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Keywords, tables and columns at `caret` in `sql`. Columns follow the statement's FROM
/// items; after `alias.` only that table's columns come; after FROM/JOIN/INTO/UPDATE tables
/// come first.
pub fn complete(sql: &str, caret: usize, schema: Option<&dyn SqlSchema>) -> SqlCompletions {
    let chars: Vec<char> = sql.chars().collect();
    let caret = caret.min(chars.len());
    let mut start = caret;
    while start > 0 && chars.get(start - 1).is_some_and(|c| is_word_char(*c)) {
        start -= 1;
    }
    let prefix: String = chars.get(start..caret).unwrap_or(&[]).iter().collect();
    let qualifier = (start > 0 && chars.get(start - 1) == Some(&'.')).then(|| {
        let end = start - 1;
        match end.checked_sub(1).and_then(|i| chars.get(i)) {
            Some(q @ ('"' | '`')) => {
                let close = end - 1;
                let mut s = close;
                while s > 0 && chars.get(s - 1) != Some(q) {
                    s -= 1;
                }
                chars.get(s..close).unwrap_or(&[]).iter().collect::<String>()
            }
            _ => {
                let mut s = end;
                while s > 0 && chars.get(s - 1).is_some_and(|c| is_word_char(*c)) {
                    s -= 1;
                }
                chars.get(s..end).unwrap_or(&[]).iter().collect::<String>()
            }
        }
    });
    let mut out = SqlCompletions { start, prefix: prefix.clone(), qualifier: qualifier.clone(), items: Vec::new() };
    let statements = analyze(sql);
    let st = statements.iter().find(|s| s.start <= caret && caret <= s.end.max(s.start)).or_else(|| {
        // Past the last statement's end (typing at the end of the text).
        statements.iter().rev().find(|s| s.start <= caret)
    });
    let expects_table = expects_table(&chars, start);
    let add_columns = |out: &mut Vec<SqlItem>, schema: &dyn SqlSchema, tr: &TableRef, rank: u8| {
        if let Resolved::Found(sc, name) = resolve_table(schema, tr.schema.as_deref(), &tr.name)
            && let Some(cols) = schema.columns(sc.as_deref(), &name)
        {
            for c in cols {
                let detail = Some(format!("{name}.{}", c.data_type));
                out.push(SqlItem { label: c.name, kind: SqlItemKind::Column, detail, rank });
            }
        }
    };
    let add_tables = |out: &mut Vec<SqlItem>, schema: &dyn SqlSchema, sc: Option<&str>, rank: u8| {
        for t in schema.tables(sc).unwrap_or_default() {
            out.push(SqlItem { label: t, kind: SqlItemKind::Table, detail: sc.map(str::to_string), rank });
        }
    };
    if let Some(q) = &qualifier {
        let Some(schema) = schema else { return out };
        let tables: &[TableRef] = st.map(|s| s.tables.as_slice()).unwrap_or(&[]);
        let by_alias = tables.iter().find(|t| t.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q)));
        let as_schema = schema.schemas().into_iter().find(|s| s.eq_ignore_ascii_case(q));
        match (by_alias, as_schema) {
            (Some(tr), _) => add_columns(&mut out.items, schema, tr, 0),
            (None, Some(sc)) => add_tables(&mut out.items, schema, Some(&sc), 0),
            (None, None) => {
                // A table used by its name (in FROM or not).
                let tr = tables.iter().find(|t| t.name.eq_ignore_ascii_case(q)).cloned().unwrap_or(TableRef {
                    schema: None,
                    name: q.clone(),
                    alias: None,
                    start: 0,
                    end: 0,
                });
                add_columns(&mut out.items, schema, &tr, 0);
            }
        }
        return out;
    }
    if let Some(schema) = schema {
        if expects_table {
            add_tables(&mut out.items, schema, None, 0);
            for sc in schema.schemas() {
                out.items.push(SqlItem {
                    label: sc,
                    kind: SqlItemKind::Schema,
                    detail: Some("schema".into()),
                    rank: 1,
                });
            }
        } else {
            if let Some(st) = st {
                for tr in &st.tables {
                    add_columns(&mut out.items, schema, tr, 0);
                }
            }
            add_tables(&mut out.items, schema, None, 1);
            for sc in schema.schemas() {
                out.items.push(SqlItem {
                    label: sc,
                    kind: SqlItemKind::Schema,
                    detail: Some("schema".into()),
                    rank: 2,
                });
            }
        }
    }
    let lower = prefix.chars().next().is_some_and(char::is_lowercase);
    for kw in KEYWORDS {
        let label = if lower { kw.to_lowercase() } else { kw.to_string() };
        out.items.push(SqlItem { label, kind: SqlItemKind::Keyword, detail: None, rank: 3 });
    }
    // Columns of joined tables may repeat.
    let mut seen = std::collections::HashSet::new();
    out.items.retain(|i| seen.insert((i.label.clone(), i.detail.clone())));
    out
}

/// Whether a table name is expected at `start`: right after FROM/JOIN/INTO/UPDATE/TABLE, or
/// after a comma in a FROM list.
fn expects_table(chars: &[char], start: usize) -> bool {
    let before: String = chars.get(..start).unwrap_or(&[]).iter().collect();
    let t = Toks::new(&before);
    let Some(last) = t.len().checked_sub(1) else { return false };
    if matches!(t.word(last), b"from" | b"join" | b"into" | b"update" | b"table") {
        return true;
    }
    if !t.is_sym(last, ',') {
        return false;
    }
    // A comma in a FROM list: FROM comes before any other clause word at this depth.
    let mut depth = 0usize;
    for p in (0..last).rev() {
        match t.sym(p) {
            Some(')') => depth += 1,
            Some('(') => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ if depth == 0 => match t.word(p) {
                b"from" => return true,
                w if is_clause_word(w) => return false,
                _ => {}
            },
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// `table → columns`; `None` = not loaded.
    type Tables = BTreeMap<String, Option<Vec<&'static str>>>;

    /// `schema → tables`; `None` = not loaded.
    #[derive(Default)]
    struct Fake {
        default: String,
        schemas: BTreeMap<String, Option<Tables>>,
    }

    impl Fake {
        fn library() -> Self {
            let mut tables = BTreeMap::new();
            tables.insert("users".to_string(), Some(vec!["id", "name", "email"]));
            tables.insert("orders".to_string(), Some(vec!["id", "user_id", "total"]));
            tables.insert("Weird Table".to_string(), Some(vec!["Odd Col"]));
            tables.insert("lazy".to_string(), None);
            let mut schemas = BTreeMap::new();
            schemas.insert("main".to_string(), Some(tables));
            schemas.insert("other".to_string(), None);
            Self { default: "main".into(), schemas }
        }
    }

    impl SqlSchema for Fake {
        fn tables(&self, schema: Option<&str>) -> Option<Vec<String>> {
            let sc = schema.unwrap_or(&self.default);
            self.schemas.get(sc)?.as_ref().map(|t| t.keys().cloned().collect())
        }
        fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<SqlColumn>> {
            let sc = schema.unwrap_or(&self.default);
            let cols = self.schemas.get(sc)?.as_ref()?.get(table)?.as_ref()?;
            Some(cols.iter().map(|c| SqlColumn { name: c.to_string(), data_type: "text".into() }).collect())
        }
        fn schemas(&self) -> Vec<String> {
            self.schemas.keys().cloned().collect()
        }
    }

    fn issues(sql: &str) -> Vec<String> {
        let chars: Vec<char> = sql.chars().collect();
        check(sql, &Fake::library())
            .into_iter()
            .map(|i| format!("{} @{}", i.message, chars[i.start..i.end].iter().collect::<String>()))
            .collect()
    }

    #[test]
    fn tables_aliases_and_columns() {
        let st = &analyze(
            "SELECT u.id, o.total AS t FROM main.users u JOIN orders AS o ON o.user_id = u.id WHERE name = $1",
        )[0];
        let tables: Vec<(Option<&str>, &str, Option<&str>)> =
            st.tables.iter().map(|t| (t.schema.as_deref(), t.name.as_str(), t.alias.as_deref())).collect();
        assert_eq!(tables, [(Some("main"), "users", Some("u")), (None, "orders", Some("o"))]);
        let cols: Vec<(Option<&str>, &str)> =
            st.columns.iter().map(|c| (c.qualifier.as_deref(), c.name.as_str())).collect();
        assert_eq!(
            cols,
            [(Some("u"), "id"), (Some("o"), "total"), (Some("o"), "user_id"), (Some("u"), "id"), (None, "name")]
        );
        assert_eq!(st.aliases, ["t"]);
        assert!(st.is_dml() && !st.opaque);
    }

    #[test]
    fn ctes_subqueries_and_functions() {
        let st = &analyze(
            "WITH recent AS (SELECT * FROM orders) SELECT * FROM recent r, (SELECT 1) x, generate_series(1, 3) g",
        )[0];
        assert_eq!(st.ctes, ["recent"]);
        assert_eq!(st.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["orders", "recent"]);
        assert!(st.opaque);
        assert_eq!(st.derived, ["x", "g"]);
        // FROM inside a function call is not a table position.
        let st = &analyze("SELECT extract(epoch FROM created) FROM users")[0];
        assert_eq!(st.tables.len(), 1);
        let st = &analyze("SELECT a IS DISTINCT FROM b FROM users")[0];
        assert_eq!(st.tables.len(), 1);
    }

    #[test]
    fn insert_update_delete_targets() {
        let st = &analyze("INSERT INTO users (id, name) VALUES ($1, $2) RETURNING id")[0];
        assert_eq!(st.tables[0].name, "users");
        assert_eq!(st.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "id"]);
        let st = &analyze("UPDATE users SET name = 'x' WHERE id = 1")[0];
        assert_eq!(st.tables[0].name, "users");
        let st = &analyze("DELETE FROM orders o WHERE o.total > 5")[0];
        assert_eq!(st.tables[0].alias.as_deref(), Some("o"));
        let st = &analyze("INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a")[0];
        assert_eq!(st.tables.len(), 1);
    }

    #[test]
    fn unresolved_tables_and_columns() {
        assert!(issues("SELECT u.id, u.name FROM users u WHERE u.email = ?").is_empty());
        assert_eq!(issues("SELECT * FROM userz"), ["Unable to resolve table 'userz' @userz"]);
        assert_eq!(issues("SELECT u.nam FROM users u"), ["Unable to resolve column 'nam' @nam"]);
        assert_eq!(issues("select nme from users"), ["Unable to resolve column 'nme' @nme"]);
        assert_eq!(issues("UPDATE users SET nme = 1"), ["Unable to resolve column 'nme' @nme"]);
        assert_eq!(issues("INSERT INTO users (id, nme) VALUES (1, 2)"), ["Unable to resolve column 'nme' @nme"]);
        // Case-insensitive, quoted, aliases, pseudo columns, ORDER BY aliases, CTEs.
        assert!(issues("SELECT ID, \"name\" FROM USERS").is_empty());
        assert!(issues("SELECT \"Odd Col\" FROM \"Weird Table\"").is_empty());
        assert!(issues("SELECT count(*) AS n FROM orders GROUP BY user_id ORDER BY n DESC").is_empty());
        assert!(issues("SELECT rowid, total t FROM orders ORDER BY t").is_empty());
        assert!(issues("WITH x AS (SELECT id FROM users) SELECT anything FROM x").is_empty());
        assert!(issues("SELECT o.total::text, CAST(o.id AS integer) FROM orders o").is_empty());
        // Unknown: subqueries, table functions, lazy tables, unknown schemas and qualifiers.
        assert!(issues("SELECT whatever FROM (SELECT 1) s").is_empty());
        assert!(issues("SELECT whatever FROM lazy").is_empty());
        assert!(issues("SELECT * FROM other.anything").is_empty());
        assert!(issues("SELECT * FROM nope.anything").is_empty());
        assert!(issues("SELECT excluded.zzz, new.zzz FROM users").is_empty());
        // A table found in another loaded schema is fine; DDL is not checked.
        assert!(issues("CREATE TABLE nope (zzz int)").is_empty());
        assert!(issues("DROP TABLE nope").is_empty());
        // Several statements: each its own scope.
        assert_eq!(
            issues("SELECT id FROM users; SELECT total FROM users"),
            ["Unable to resolve column 'total' @total"]
        );
    }

    #[test]
    fn no_warnings_without_a_schema() {
        struct Empty;
        impl SqlSchema for Empty {
            fn tables(&self, _: Option<&str>) -> Option<Vec<String>> {
                None
            }
            fn columns(&self, _: Option<&str>, _: &str) -> Option<Vec<SqlColumn>> {
                None
            }
            fn schemas(&self) -> Vec<String> {
                Vec::new()
            }
        }
        assert!(check("SELECT nope FROM nothing n WHERE n.x = 1", &Empty).is_empty());
    }

    fn labels(sql: &str, caret_marker: &str) -> (SqlCompletions, Vec<String>) {
        let caret = sql.find(caret_marker).map(|b| sql.get(..b).unwrap().chars().count()).unwrap();
        let text = sql.replacen(caret_marker, "", 1);
        let fake = Fake::library();
        let c = complete(&text, caret, Some(&fake));
        let labels = c.items.iter().map(|i| i.label.clone()).collect();
        (c, labels)
    }

    #[test]
    fn alias_aware_completion() {
        let (c, l) = labels("SELECT u.| FROM users u", "|");
        assert_eq!(c.qualifier.as_deref(), Some("u"));
        assert_eq!(l, ["id", "name", "email"]);
        let (_, l) = labels("SELECT o.to| FROM users u JOIN orders o ON o.user_id = u.id", "|");
        assert_eq!(l, ["id", "user_id", "total"]);
        // Table name as qualifier, schema as qualifier.
        let (_, l) = labels("SELECT orders.| FROM orders", "|");
        assert_eq!(l, ["id", "user_id", "total"]);
        let (_, l) = labels("SELECT * FROM main.|", "|");
        assert!(l.contains(&"users".to_string()) && l.contains(&"orders".to_string()));
    }

    #[test]
    fn tables_after_from_and_columns_elsewhere() {
        let (c, l) = labels("SELECT * FROM us|", "|");
        assert_eq!((c.start, c.prefix.as_str()), (14, "us"));
        assert_eq!(l.first().map(String::as_str), Some("Weird Table"));
        assert!(c.items.iter().take(4).all(|i| i.kind == SqlItemKind::Table));
        let (_, l) = labels("SELECT * FROM users, |", "|");
        assert!(l.contains(&"orders".to_string()));
        let (c, _) = labels("SELECT n| FROM users", "|");
        assert_eq!(
            c.items[0],
            SqlItem { label: "id".into(), kind: SqlItemKind::Column, detail: Some("users.text".into()), rank: 0 }
        );
        // Keywords follow the typed case.
        assert!(c.items.iter().any(|i| i.label == "select"));
        let (c, _) = labels("SEL|", "|");
        assert!(c.items.iter().any(|i| i.label == "SELECT"));
        // Without a schema: keywords only.
        let c = complete("SELECT * FROM u", 15, None);
        assert!(c.items.iter().all(|i| i.kind == SqlItemKind::Keyword));
    }

    #[test]
    fn hostile_input_never_panics() {
        let fake = Fake::library();
        for sql in [
            "",
            "(",
            ")",
            "((((",
            "))))",
            ".",
            "..",
            "a.",
            ".a",
            "FROM",
            "FROM (",
            "FROM a,",
            "JOIN",
            "INSERT INTO",
            "UPDATE",
            "WITH",
            "WITH a AS",
            "WITH a AS (",
            "a AS (",
            "\"",
            "\"a",
            "select a.b.c.d.e.f from x",
            "select * from a as",
            "select :: from x",
            "é.ü FROM ü",
            "SELECT $$ x",
            ";;;;",
        ] {
            let _ = analyze(sql);
            let _ = check(sql, &fake);
            for caret in 0..=sql.chars().count() + 1 {
                let _ = complete(sql, caret, Some(&fake));
            }
        }
        let big = "SELECT a, b FROM t WHERE (x = 1) AND ".repeat(2000);
        let _ = check(&big, &fake);
    }
}
