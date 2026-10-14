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
    let (kind, old, def) = match target_in(&a, offset)? {
        SqlTarget::Alias { name, def, .. } => ("alias", name, def),
        SqlTarget::Cte { name, def } => ("cte", name, def),
        _ => return None,
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
