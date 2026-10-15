//! What the name at a caret in SQL text refers to (an alias, a CTE, a table, a column, a
//! routine, a schema), and the occurrences an in-statement rename of an alias or CTE changes.
//! Pure functions over the lexical analysis of [`codecraft_syntax::sql_scope`]; offsets are char
//! offsets in the SQL text.

use serde::Serialize;

use codecraft_syntax::sql_scope::{self, SqlToken, SqlTokenKind, Statement};

/// The object a name in SQL stands for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SqlTarget {
    /// A table alias: where it is defined and the table it stands for.
    Alias {
        name: String,
        def: (usize, usize),
        schema: Option<String>,
        table: String,
    },
    /// A common table expression and where it is defined.
    Cte {
        name: String,
        def: (usize, usize),
    },
    Table {
        schema: Option<String>,
        name: String,
    },
    /// A column and the tables it may belong to (the qualifier's, else every FROM table).
    Column {
        name: String,
        tables: Vec<(Option<String>, String)>,
    },
    Routine {
        schema: Option<String>,
        name: String,
    },
    Schema {
        name: String,
    },
}

/// The tokens and statements of an SQL text.
pub struct Analysis {
    pub tokens: Vec<SqlToken>,
    pub statements: Vec<Statement>,
}

impl Analysis {
    pub fn new(sql: &str) -> Self {
        Self { tokens: sql_scope::tokens(sql), statements: sql_scope::analyze(sql) }
    }

    /// The statement containing the char offset `at`.
    pub fn statement_at(&self, at: usize) -> Option<&Statement> {
        self.statements.iter().find(|s| s.start <= at && at <= s.end.max(s.start))
    }

    /// Index of the name-like token at (or just before) `at`.
    pub fn token_at(&self, at: usize) -> Option<usize> {
        let named = |t: &SqlToken| {
            matches!(
                t.kind,
                SqlTokenKind::Name | SqlTokenKind::Function | SqlTokenKind::QuotedName | SqlTokenKind::Keyword
            )
        };
        self.tokens
            .iter()
            .position(|t| t.start <= at && at < t.end && named(t))
            .or_else(|| self.tokens.iter().position(|t| t.end == at && named(t)))
    }

    fn sym(&self, i: Option<usize>, c: char) -> bool {
        i.and_then(|i| self.tokens.get(i)).is_some_and(|t| t.is_sym(c))
    }

    fn name(&self, i: usize) -> Option<String> {
        self.tokens.get(i).and_then(|t| t.name(true))
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// What the name at `offset` in `sql` refers to.
pub fn target_at(sql: &str, offset: usize) -> Option<SqlTarget> {
    let a = Analysis::new(sql);
    target_in(&a, offset)
}

pub(crate) fn target_in(a: &Analysis, offset: usize) -> Option<SqlTarget> {
    let i = a.token_at(offset)?;
    let tok = a.tokens.get(i)?;
    let name = tok.name(true)?;
    let span = (tok.start, tok.end);
    let st = a.statement_at(tok.start)?;
    let is_cte = |n: &str| st.ctes.iter().any(|c| eq(c, n));
    let cte_def = |n: &str| {
        st.ctes
            .iter()
            .position(|c| eq(c, n))
            .and_then(|k| st.cte_spans.get(k).copied())
            .map(|def| SqlTarget::Cte { name: n.to_string(), def })
    };
    // Where an alias or a CTE is defined.
    if let Some(tr) = st.tables.iter().find(|t| t.alias_span == Some(span)) {
        return Some(SqlTarget::Alias { name, def: span, schema: tr.schema.clone(), table: tr.name.clone() });
    }
    if let Some(k) = st.cte_spans.iter().position(|sp| *sp == span) {
        return Some(SqlTarget::Cte { name: st.ctes.get(k).cloned().unwrap_or(name), def: span });
    }
    // A table position (FROM, JOIN, INTO, UPDATE).
    if let Some(tr) = st.tables.iter().find(|t| (t.start, t.end) == span) {
        if tr.schema.is_none() && is_cte(&tr.name) {
            return cte_def(&tr.name);
        }
        return Some(SqlTarget::Table { schema: tr.schema.clone(), name: tr.name.clone() });
    }
    // A column.
    if let Some(c) = st.columns.iter().find(|c| (c.start, c.end) == span) {
        let tables: Vec<(Option<String>, String)> = match &c.qualifier {
            Some(q) => {
                let by_alias = st.tables.iter().find(|t| t.alias.as_deref().is_some_and(|al| eq(al, q)));
                let by_name = st.tables.iter().find(|t| eq(&t.name, q));
                match by_alias.or(by_name) {
                    Some(t) if !(t.schema.is_none() && is_cte(&t.name)) => vec![(t.schema.clone(), t.name.clone())],
                    Some(_) => Vec::new(),
                    None if is_cte(q) => Vec::new(),
                    // `schema.table.column` or a table that is not in FROM.
                    None => {
                        let schema = i
                            .checked_sub(4)
                            .filter(|&k| a.sym(Some(k + 1), '.') && a.sym(Some(k + 3), '.'))
                            .and_then(|k| a.name(k));
                        vec![(schema, q.clone())]
                    }
                }
            }
            None => st
                .tables
                .iter()
                .filter(|t| !(t.schema.is_none() && is_cte(&t.name)))
                .map(|t| (t.schema.clone(), t.name.clone()))
                .collect(),
        };
        return Some(SqlTarget::Column { name: c.name.clone(), tables });
    }
    // A qualifier: `alias.`, `table.`, `cte.`, `schema.`.
    if a.sym(Some(i + 1), '.') {
        if let Some(tr) = st.tables.iter().find(|t| t.alias.as_deref().is_some_and(|al| eq(al, &name))) {
            let def = tr.alias_span.unwrap_or(span);
            return Some(SqlTarget::Alias { name, def, schema: tr.schema.clone(), table: tr.name.clone() });
        }
        if is_cte(&name) {
            return cte_def(&name);
        }
        // The schema part of a qualified table or column (`s.t`, `s.t.c`).
        let three_part = a.sym(Some(i + 3), '.') && a.name(i + 2).is_some() && a.name(i + 4).is_some();
        let schema_of_table = st.tables.iter().any(|t| {
            t.schema.as_deref().is_some_and(|s| eq(s, &name)) && a.tokens.get(i + 2).is_some_and(|n| n.start == t.start)
        });
        if three_part || schema_of_table {
            return Some(SqlTarget::Schema { name });
        }
        let schema = i.checked_sub(2).filter(|&k| a.sym(Some(k + 1), '.')).and_then(|k| a.name(k));
        if let Some(tr) = st.tables.iter().find(|t| eq(&t.name, &name)) {
            return Some(SqlTarget::Table { schema: tr.schema.clone().or(schema), name: tr.name.clone() });
        }
        return Some(SqlTarget::Table { schema, name });
    }
    // A function call.
    if tok.kind == SqlTokenKind::Function || a.sym(Some(i + 1), '(') && tok.kind != SqlTokenKind::Keyword {
        let schema = i.checked_sub(2).filter(|&k| a.sym(Some(k + 1), '.')).and_then(|k| a.name(k));
        return Some(SqlTarget::Routine { schema, name });
    }
    None
}

/// An in-statement rename: the current name and every place it is written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenamePlan {
    pub old: String,
    /// `alias` or `cte`.
    pub kind: &'static str,
    pub spans: Vec<(usize, usize)>,
}

/// The alias or CTE at `offset` and its occurrences in its statement; `None` elsewhere.
pub fn rename_plan(sql: &str, offset: usize) -> Option<RenamePlan> {
    let a = Analysis::new(sql);
    let (kind, old, def) = match target_in(&a, offset) {
        Some(SqlTarget::Alias { name, def, .. }) => ("alias", name, def),
        Some(SqlTarget::Cte { name, def }) => ("cte", name, def),
        // A column alias (`expr AS total`) and its uses (track DX).
        _ => return column_alias_plan(&a, offset),
    };
    let st = a.statement_at(def.0)?;
    let mut spans = vec![def];
    for (k, t) in a.tokens.iter().enumerate() {
        if t.start < st.start || t.end > st.end || (t.start, t.end) == def {
            continue;
        }
        if !t.name(false).is_some_and(|n| eq(&n, &old)) {
            continue;
        }
        let after_dot = k.checked_sub(1).is_some_and(|p| a.sym(Some(p), '.'));
        let before_dot = a.sym(Some(k + 1), '.');
        let used = match kind {
            // `u.col` (an alias stands before a dot).
            "alias" => before_dot && !after_dot,
            // A CTE is used as a table name or a qualifier.
            _ => !after_dot,
        };
        if used {
            spans.push((t.start, t.end));
        }
    }
    spans.sort_unstable();
    spans.dedup();
    Some(RenamePlan { old, kind, spans })
}

/// Words that start a clause of a query (the select list ends at the first one).
const CLAUSES: &[&str] = &[
    "select", "from", "where", "group", "order", "having", "limit", "offset", "union", "intersect", "except",
    "window", "qualify", "fetch", "for", "into", "join", "on", "values", "set", "returning", "with",
];

/// A column alias defined in a select list (`expr AS name`, `expr name`), at `offset` or used
/// there (ORDER BY, GROUP BY, HAVING of the same query; `cte.name` / `sub.name` outside a CTE
/// or derived table), and every place it is written.
fn column_alias_plan(a: &Analysis, offset: usize) -> Option<RenamePlan> {
    let i = a.token_at(offset)?;
    let tok = a.tokens.get(i)?;
    let name = tok.name(true)?;
    let st = a.statement_at(tok.start)?;
    if !st.aliases.iter().any(|al| eq(al, &name)) {
        return None;
    }
    // Token indexes of the statement and their paren depth.
    let range: Vec<usize> =
        (0..a.tokens.len()).filter(|&k| a.tokens.get(k).is_some_and(|t| t.start >= st.start && t.end <= st.end)).collect();
    let first = *range.first()?;
    let last = *range.last()?;
    let mut depth = vec![0usize; a.tokens.len()];
    let mut d = 0usize;
    for &k in &range {
        let t = a.tokens.get(k)?;
        if t.is_sym(')') {
            d = d.saturating_sub(1);
        }
        if let Some(slot) = depth.get_mut(k) {
            *slot = d;
        }
        if t.is_sym('(') {
            d += 1;
        }
    }
    let dep = |k: usize| depth.get(k).copied().unwrap_or(0);
    let is_word = |k: usize, w: &str| a.tokens.get(k).is_some_and(|t| t.is_word(w));
    let clause_of = |k: usize| a.tokens.get(k).and_then(|t| CLAUSES.iter().find(|c| t.is_word(c)).copied());
    // The clause a token is in, at its own depth (`None` past the scope's opening paren).
    let clause_at = |k: usize| -> Option<&'static str> {
        let mut j = k;
        while j > first {
            j -= 1;
            if dep(j) < dep(k) {
                return None;
            }
            if dep(j) == dep(k)
                && let Some(c) = clause_of(j)
            {
                return Some(c);
            }
        }
        None
    };
    let named = |k: usize| a.tokens.get(k).and_then(|t| t.name(true)).is_some_and(|n| eq(&n, &name));
    let sym = |k: usize, c: char| a.sym(Some(k), c);
    let is_def = |k: usize| -> bool {
        if !named(k) || clause_at(k) != Some("select") || k == first {
            return false;
        }
        let prev = k - 1;
        if is_word(prev, "as") {
            return true;
        }
        // `expr name`: after a value, before `,`, FROM or the end of the list.
        let after_value = a.tokens.get(prev).is_some_and(|t| {
            matches!(t.kind, SqlTokenKind::Name | SqlTokenKind::QuotedName | SqlTokenKind::Literal) || t.is_sym(')')
        }) && !sym(prev, '.')
            && clause_of(prev).is_none();
        let next_ends = k == last || sym(k + 1, ',') || sym(k + 1, ')') || is_word(k + 1, "from");
        after_value && next_ends && dep(prev) == dep(k)
    };
    // The scope of a token: the tokens between its enclosing parens (or the statement).
    let scope_of = |k: usize| -> (usize, usize) {
        let mut open = first;
        let mut j = k;
        while j > first {
            j -= 1;
            if dep(j) < dep(k) {
                open = j;
                break;
            }
        }
        let mut close = last;
        for &j in range.iter().filter(|&&j| j > k) {
            if dep(j) < dep(k) {
                close = j;
                break;
            }
        }
        (open, close)
    };
    // The definition: the alias at the caret, else the one of the query the caret is in.
    let def = if is_def(i) {
        i
    } else {
        let (o, c) = scope_of(i);
        range.iter().copied().find(|&k| k >= o && k <= c && dep(k) == dep(i) && is_def(k))?
    };
    let def_tok = a.tokens.get(def)?;
    let old = def_tok.name(true)?;
    let mut spans = vec![(def_tok.start, def_tok.end)];
    let (open, close) = scope_of(def);
    // Uses in ORDER BY / GROUP BY / HAVING of the same query (not in nested subqueries).
    let mut in_ref = false;
    let mut k = open;
    while k <= close {
        if dep(k) == dep(def) {
            if let Some(c) = clause_of(k) {
                in_ref = matches!(c, "order" | "group" | "having");
            }
        } else if dep(k) > dep(def) && sym(k, '(') && (is_word(k + 1, "select") || is_word(k + 1, "with")) {
            // Skip a nested subquery.
            let inner = dep(k) + 1;
            k += 1;
            while k <= close && dep(k) >= inner {
                k += 1;
            }
            continue;
        }
        let plain = !sym(k.wrapping_sub(1), '.') && !sym(k + 1, '.') && !sym(k + 1, '(');
        if in_ref
            && k != def
            && named(k)
            && plain
            && let Some(t) = a.tokens.get(k)
        {
            spans.push((t.start, t.end));
        }
        k += 1;
    }
    // Outside the query: `cte.name` for a CTE body, `sub.name` for a derived table.
    if open != first && sym(open, '(') {
        let mut qualifiers: Vec<String> = Vec::new();
        if open >= 2
            && is_word(open - 1, "as")
            && let Some(cte) = open.checked_sub(2).and_then(|k| a.name(k))
            && st.ctes.iter().any(|c| eq(c, &cte))
        {
            let aliases = st.tables.iter().filter(|t| t.schema.is_none() && eq(&t.name, &cte));
            qualifiers.extend(aliases.filter_map(|t| t.alias.clone()));
            qualifiers.push(cte);
        }
        let after = if is_word(close + 1, "as") { close + 2 } else { close + 1 };
        if sym(close, ')')
            && let Some(alias) = a.tokens.get(after).filter(|t| t.end <= st.end).and_then(|t| t.name(false))
            && st.derived.iter().any(|dname| eq(dname, &alias))
        {
            qualifiers.push(alias);
        }
        for &k in &range {
            if k >= open && k <= close {
                continue;
            }
            let q = k.checked_sub(2).filter(|&p| sym(p + 1, '.')).and_then(|p| a.name(p));
            if named(k)
                && q.is_some_and(|q| qualifiers.iter().any(|x| eq(x, &q)))
                && !sym(k + 1, '.')
                && let Some(t) = a.tokens.get(k)
            {
                spans.push((t.start, t.end));
            }
        }
    }
    spans.sort_unstable();
    spans.dedup();
    Some(RenamePlan { old, kind: "column alias", spans })
}

/// `name` as an SQL identifier: bare when it is a plain lower-case-safe word, else quoted.
pub fn identifier(name: &str, mysql: bool) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.chars().any(|c| c.is_ascii_uppercase());
    if plain {
        name.to_string()
    } else if mysql {
        format!("`{}`", name.replace('`', "``"))
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(sql: &str, marker: &str) -> usize {
        let b = sql.find(marker).unwrap_or(0);
        sql.get(..b).map(|p| p.chars().count()).unwrap_or(0)
    }

    #[test]
    fn targets_of_names() {
        let sql = "WITH r AS (SELECT 1 AS x) SELECT u.name, o.total, r.x, count(*), public.f(1) FROM public.users u JOIN orders o ON o.uid = u.id, r";
        let t = |m: &str, extra: usize| target_at(sql, at(sql, m) + extra);
        assert_eq!(
            t("u.name", 0),
            Some(SqlTarget::Alias {
                name: "u".into(),
                def: (at(sql, "u JOIN"), at(sql, "u JOIN") + 1),
                schema: Some("public".into()),
                table: "users".into()
            })
        );
        assert_eq!(
            t("name,", 1),
            Some(SqlTarget::Column { name: "name".into(), tables: vec![(Some("public".into()), "users".into())] })
        );
        assert_eq!(t("users u", 2), Some(SqlTarget::Table { schema: Some("public".into()), name: "users".into() }));
        assert_eq!(t("public.users", 0), Some(SqlTarget::Schema { name: "public".into() }));
        assert_eq!(t("orders", 0), Some(SqlTarget::Table { schema: None, name: "orders".into() }));
        assert!(matches!(t("r.x", 0), Some(SqlTarget::Cte { ref name, .. }) if name == "r"));
        assert!(matches!(t(", r", 2), Some(SqlTarget::Cte { ref name, def }) if name == "r" && def.0 == 5));
        assert_eq!(t("f(1)", 0), Some(SqlTarget::Routine { schema: Some("public".into()), name: "f".into() }));
        assert!(matches!(t("count", 0), Some(SqlTarget::Routine { .. })));
        // Unqualified columns may belong to any FROM table (not the CTE).
        let sql2 = "SELECT total FROM users, orders";
        assert_eq!(
            target_at(sql2, 8),
            Some(SqlTarget::Column {
                name: "total".into(),
                tables: vec![(None, "users".into()), (None, "orders".into())]
            })
        );
        assert_eq!(target_at("SELECT 1", 3), None);
        assert_eq!(target_at("", 0), None);
    }

    #[test]
    fn rename_alias_and_cte_in_the_statement_only() {
        let sql = "SELECT u.id, u.name FROM users u WHERE u.id > 1; SELECT u.id FROM users u";
        let p = rename_plan(sql, at(sql, "u.name")).unwrap();
        assert_eq!(p.old, "u");
        assert_eq!(p.kind, "alias");
        assert_eq!(p.spans.len(), 4, "{p:?}");
        assert!(p.spans.iter().all(|&(_, e)| e < at(sql, "; SELECT")));
        let sql = "WITH recent AS (SELECT * FROM orders) SELECT recent.id FROM recent JOIN recent r2 ON true";
        let p = rename_plan(sql, at(sql, "recent AS")).unwrap();
        assert_eq!((p.kind, p.spans.len()), ("cte", 4));
        assert!(rename_plan("SELECT id FROM users", 8).is_none());
        assert_eq!(identifier("ok_1", false), "ok_1");
        assert_eq!(identifier("Odd name", false), "\"Odd name\"");
        assert_eq!(identifier("x`y", true), "`x``y`");
    }

    #[test]
    fn rename_column_aliases() {
        let sql = "SELECT price * qty AS total, name n FROM items WHERE total > 0 ORDER BY total DESC, n";
        let p = rename_plan(sql, at(sql, "total,")).unwrap();
        assert_eq!((p.kind, p.old.as_str()), ("column alias", "total"));
        // The definition and ORDER BY; WHERE cannot use the alias, so it is a column there.
        assert_eq!(p.spans, vec![(at(sql, "total,"), at(sql, "total,") + 5), (at(sql, "total DESC"), at(sql, "total DESC") + 5)]);
        // From a use, and a bare alias (`name n`).
        let q = rename_plan(sql, at(sql, "total DESC") + 1).unwrap();
        assert_eq!(q.spans, p.spans);
        let n = rename_plan(sql, at(sql, " n FROM") + 1).unwrap();
        assert_eq!(n.spans.len(), 2, "{n:?}");

        // A CTE's column alias is renamed where the CTE is used with a qualifier.
        let sql = "WITH t AS (SELECT count(*) AS c FROM x GROUP BY y HAVING c > 1) SELECT t.c, u.c FROM t, u";
        let p = rename_plan(sql, at(sql, "c FROM")).unwrap();
        assert_eq!(p.spans.len(), 3, "{p:?}");
        assert!(p.spans.contains(&(at(sql, "c, u.c"), at(sql, "c, u.c") + 1)));
        assert!(!p.spans.contains(&(at(sql, "c FROM t"), at(sql, "c FROM t") + 1)));

        // A nested query's alias of the same name stays.
        let sql = "SELECT a AS v FROM t WHERE id IN (SELECT b AS v FROM s ORDER BY v) ORDER BY v";
        let p = rename_plan(sql, at(sql, "v FROM t")).unwrap();
        assert_eq!(p.spans.len(), 2, "{p:?}");
        assert!(p.spans.iter().all(|&(s, _)| s < at(sql, "(SELECT") || s > at(sql, ") ORDER")));
        // CAST(x AS int) is no alias; plain columns are not renamed here.
        assert!(rename_plan("SELECT CAST(a AS int) FROM t", 17).is_none());
        assert!(rename_plan("SELECT a FROM t ORDER BY a", 7).is_none());
    }

    #[test]
    fn hostile_input_never_panics() {
        for sql in [
            "",
            ".",
            "a.",
            ".a",
            "a..b",
            "(((",
            "WITH",
            "WITH a AS (",
            "SELECT \"",
            "é.ü(",
            "u.",
            "SELECT * FROM t x WHERE x.",
        ] {
            for k in 0..=sql.chars().count() + 2 {
                let _ = target_at(sql, k);
                let _ = rename_plan(sql, k);
            }
        }
    }
}
