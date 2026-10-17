//! Whether a console query's rows are one table's rows, so the grid can edit them like Open
//! Table does: `SELECT <columns> FROM [schema.]table [alias] [WHERE …] [ORDER BY …] [LIMIT …]`
//! with no joins, subqueries, aggregates, window functions, DISTINCT, GROUP BY, HAVING or set
//! operations. [`parse`] reads the query; [`map_columns`] matches its result columns to the
//! table's (with the table's metadata), and [`pick_key`] chooses the primary key, else a unique
//! key, whose columns the result includes.
//!
//! Only plain column references (`name`, `t.name`, `"Name" AS n`) are editable; any other
//! select item (`upper(name)`, `price * 2`, `'x'`) is computed and read-only, and can never be
//! a key. Anything the parser does not understand makes the result read-only (it fails closed).

use serde::Serialize;

use crate::lex::{Tok, tokens};
use crate::{DbKind, Dialect, TableDetails};

/// One item of the select list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SelectItem {
    /// `*` or `t.*`: every table column, in order.
    All,
    /// A plain column reference; `output` is the result column's name (the alias, if any).
    Column { name: String, output: String },
    /// A computed value (read-only).
    Expr,
}

/// The table a query reads and what it selects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueryTable {
    /// `None` for an unqualified table (resolved by the connection's current schema).
    pub schema: Option<String>,
    pub table: String,
    pub items: Vec<SelectItem>,
}

/// Words that make a query's rows something other than one table's rows.
const REFUSE: &[&str] = &[
    "JOIN", "UNION", "INTERSECT", "EXCEPT", "MINUS", "GROUP", "HAVING", "DISTINCT", "OVER", "WINDOW", "INTO",
    "LATERAL", "PIVOT", "UNPIVOT", "QUALIFY", "TABLESAMPLE",
];

/// Aggregate (and set-returning) functions: one result row is not one table row.
const AGGREGATES: &[&str] = &[
    "COUNT", "SUM", "AVG", "MIN", "MAX", "ARRAY_AGG", "STRING_AGG", "GROUP_CONCAT", "JSON_AGG", "JSONB_AGG",
    "JSON_GROUP_ARRAY", "JSON_GROUP_OBJECT", "BOOL_AND", "BOOL_OR", "EVERY", "BIT_AND", "BIT_OR", "STDDEV",
    "STDDEV_POP", "STDDEV_SAMP", "VARIANCE", "VAR_POP", "VAR_SAMP", "TOTAL", "LISTAGG", "XMLAGG", "UNNEST",
    "GENERATE_SERIES", "JSON_EACH", "JSONB_EACH", "JSON_ARRAY_ELEMENTS", "COUNT_BIG", "GROUPING", "ANY_VALUE",
    "MEDIAN", "PERCENTILE_CONT", "PERCENTILE_DISC", "MODE", "CORR", "COVAR_POP", "COVAR_SAMP", "REGR_SLOPE",
    "UNIQ", "GROUPARRAY", "ARGMAX", "ARGMIN", "ANY", "SOME",
];

/// Words that may follow the table reference.
const AFTER_TABLE: &[&str] = &["WHERE", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR", "WITH", "OPTION"];

#[derive(Clone, Debug, PartialEq)]
enum T {
    Word(String),
    /// A quoted identifier, unquoted.
    Ident(String),
    /// A string or other literal.
    Lit,
    P(char),
}

fn toks(kind: DbKind, sql: &str) -> Vec<T> {
    let chars: Vec<char> = sql.chars().collect();
    let text = |a: usize, b: usize| chars.get(a..b).unwrap_or(&[]).iter().collect::<String>();
    let dialect = kind.dialect();
    tokens(&chars, dialect)
        .into_iter()
        .map(|t| match t {
            Tok::Word(a, b) => T::Word(text(a, b)),
            Tok::Punct(i) => T::P(chars.get(i).copied().unwrap_or(' ')),
            Tok::Literal(a, b) => {
                let s = text(a, b);
                let mut it = s.chars();
                let open = it.next().unwrap_or(' ');
                let close = match open {
                    '"' if dialect != Dialect::Mysql => '"',
                    '`' => '`',
                    '[' => ']',
                    _ => return T::Lit,
                };
                let inner: String = s.chars().skip(1).collect();
                let Some(inner) = inner.strip_suffix(close) else { return T::Lit };
                let doubled: String = [close, close].iter().collect();
                T::Ident(inner.replace(&doubled, &close.to_string()))
            }
        })
        .collect()
}

fn upper(t: &T) -> Option<String> {
    match t {
        T::Word(w) => Some(w.to_ascii_uppercase()),
        _ => None,
    }
}

fn is_word(t: Option<&T>, w: &str) -> bool {
    t.and_then(upper).is_some_and(|u| u == w)
}

/// An identifier's name as the server stores it: quoted names as written, unquoted ones folded
/// to lower case on PostgreSQL (others keep them; their lookups ignore case).
fn ident(kind: DbKind, t: &T) -> Option<String> {
    match t {
        T::Ident(s) => Some(s.clone()),
        T::Word(w) if kind == DbKind::Postgres => Some(w.to_lowercase()),
        T::Word(w) => Some(w.clone()),
        _ => None,
    }
}

/// Read a console query. `Err` says why its rows are not one table's rows.
pub fn parse(kind: DbKind, sql: &str) -> Result<QueryTable, String> {
    let all = toks(kind, sql.trim().trim_end_matches(';'));
    let mut t: &[T] = &all;
    if !is_word(t.first(), "SELECT") {
        return Err("only a SELECT from one table can be edited".into());
    }
    t = t.get(1..).unwrap_or(&[]);
    if all.contains(&T::P(';')) || all.iter().any(|x| is_word(Some(x), "GO")) {
        return Err("the query is more than one statement".into());
    }
    if let Some(w) = all.iter().filter_map(upper).find(|w| REFUSE.contains(&w.as_str())) {
        return Err(format!("the query uses {w}: its rows are not one table's rows"));
    }
    // No subqueries anywhere.
    if all.windows(2).any(|w| w.first() == Some(&T::P('(')) && is_word(w.get(1), "SELECT")) {
        return Err("the query has a subquery".into());
    }
    if is_word(t.first(), "ALL") {
        t = t.get(1..).unwrap_or(&[]);
    }
    // SQL Server `TOP (n)` / `TOP n`.
    if is_word(t.first(), "TOP") {
        t = t.get(1..).unwrap_or(&[]);
        if t.first() == Some(&T::P('(')) {
            let close = t.iter().position(|x| *x == T::P(')')).ok_or("unbalanced parentheses")?;
            t = t.get(close + 1..).unwrap_or(&[]);
        } else {
            t = t.get(1..).unwrap_or(&[]);
        }
        if is_word(t.first(), "PERCENT") || is_word(t.first(), "WITH") {
            return Err("TOP … PERCENT/WITH TIES results cannot be edited".into());
        }
    }
    // The select list ends at the top-level FROM.
    let mut depth = 0i32;
    let mut from = None;
    for (i, x) in t.iter().enumerate() {
        match x {
            T::P('(') => depth += 1,
            T::P(')') => depth -= 1,
            _ if depth == 0 && is_word(Some(x), "FROM") => {
                from = Some(i);
                break;
            }
            _ => {}
        }
    }
    let from = from.ok_or("the query reads no table")?;
    let list = t.get(..from).unwrap_or(&[]);
    let rest = t.get(from + 1..).unwrap_or(&[]);

    // FROM [db.][schema.]table [[AS] alias]
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0usize;
    loop {
        let name = rest.get(i).and_then(|x| ident(kind, x)).ok_or("the query reads no plain table")?;
        parts.push(name);
        i += 1;
        if rest.get(i) == Some(&T::P('.')) && parts.len() < 3 {
            i += 1;
            continue;
        }
        break;
    }
    if rest.get(i) == Some(&T::P('(')) {
        return Err("the query reads a table function".into());
    }
    let mut alias = None;
    if is_word(rest.get(i), "AS") {
        i += 1;
        alias = Some(rest.get(i).and_then(|x| ident(kind, x)).ok_or("the table alias is missing")?);
        i += 1;
    } else if let Some(x) = rest.get(i)
        && !matches!(x, T::P(_))
        && !upper(x).is_some_and(|u| AFTER_TABLE.contains(&u.as_str()))
    {
        alias = Some(ident(kind, x).ok_or("the query reads no plain table")?);
        i += 1;
    }
    match rest.get(i) {
        None => {}
        Some(T::P(',')) => return Err("the query reads more than one table".into()),
        Some(x) if upper(x).is_some_and(|u| AFTER_TABLE.contains(&u.as_str())) => {}
        Some(_) => return Err("the query has clauses that cannot be edited".into()),
    }
    // (A second table after a comma is caught above: only WHERE, ORDER BY, LIMIT… may follow
    // the table, and the comma lists after them — ORDER BY a, b — are not tables.)
    if parts.len() > 2 {
        return Err("tables of another database (db.schema.table) cannot be edited from a console".into());
    }
    let table = parts.pop().ok_or("the query reads no plain table")?;
    let schema = parts.pop();
    let qualifiers: Vec<String> = [Some(table.clone()), alias.clone()].into_iter().flatten().collect();
    let qualifier_ok = |q: &str| qualifiers.iter().any(|x| x.eq_ignore_ascii_case(q));

    // The select list, split at top-level commas.
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut chunks: Vec<&[T]> = Vec::new();
    for (k, x) in list.iter().enumerate() {
        match x {
            T::P('(') => depth += 1,
            T::P(')') => depth -= 1,
            T::P(',') if depth == 0 => {
                chunks.push(list.get(start..k).unwrap_or(&[]));
                start = k + 1;
            }
            _ => {}
        }
    }
    chunks.push(list.get(start..).unwrap_or(&[]));
    for item in chunks {
        if item.is_empty() {
            return Err("the select list is empty".into());
        }
        for w in item.windows(2) {
            if let Some(u) = w.first().and_then(upper)
                && AGGREGATES.contains(&u.as_str())
                && w.get(1) == Some(&T::P('('))
            {
                return Err(format!("the query aggregates ({})", u.to_lowercase()));
            }
        }
        items.push(select_item(kind, item, &qualifier_ok));
    }
    Ok(QueryTable { schema, table, items })
}

fn select_item(kind: DbKind, item: &[T], qualifier_ok: &dyn Fn(&str) -> bool) -> SelectItem {
    // `*`, `t.*`
    match item {
        [T::P('*')] => return SelectItem::All,
        [q, T::P('.'), T::P('*')] if ident(kind, q).is_some_and(|q| qualifier_ok(&q)) => return SelectItem::All,
        _ => {}
    }
    // Strip `[AS] alias` from the end.
    let (expr, alias) = match item {
        [head @ .., as_, a] if is_word(Some(as_), "AS") => (head, ident(kind, a)),
        [head @ .., a]
            if !head.is_empty()
                && matches!(a, T::Word(_) | T::Ident(_))
                && !matches!(head.last(), Some(T::P('.'))) =>
        {
            (head, ident(kind, a))
        }
        _ => (item, None),
    };
    let column = match expr {
        [c] => ident(kind, c),
        [q, T::P('.'), c] if ident(kind, q).is_some_and(|q| qualifier_ok(&q)) => ident(kind, c),
        _ => None,
    };
    // Keywords are not columns (`NULL`, `TRUE`, `CURRENT_TIMESTAMP`).
    let keyword = matches!(expr, [T::Word(w)] if ["NULL", "TRUE", "FALSE", "CURRENT_DATE", "CURRENT_TIME",
        "CURRENT_TIMESTAMP", "CURRENT_USER", "LOCALTIME", "LOCALTIMESTAMP", "DEFAULT"]
        .contains(&w.to_ascii_uppercase().as_str()));
    match column {
        Some(name) if !keyword => {
            let output = alias.unwrap_or_else(|| name.clone());
            SelectItem::Column { name, output }
        }
        _ => SelectItem::Expr,
    }
}

/// Whether a statement may change where unqualified table names resolve on its connection
/// (`USE db`, `SET search_path`, `SET ROLE`, `RESET ALL`, `DISCARD ALL`, `SET SCHEMA`).
pub fn switches_schema(kind: DbKind, sql: &str) -> bool {
    let words = crate::lex::words(sql, kind.dialect());
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    match w(0) {
        "USE" => true,
        "SET" | "RESET" => {
            let (a, b) = if w(1) == "SESSION" || w(1) == "LOCAL" { (w(2), w(3)) } else { (w(1), w(2)) };
            matches!(a, "SEARCH_PATH" | "SCHEMA" | "ROLE" | "ALL" | "CURRENT_SCHEMA")
                || (a == "SESSION" && b == "AUTHORIZATION")
                || (a == "SESSION" && b == "ROLE")
                || a == "AUTHORIZATION"
        }
        "DISCARD" => true,
        "SELECT" => words.iter().any(|x| x == "SET_CONFIG"),
        "ALTER" => words.iter().any(|x| x == "SEARCH_PATH" || x == "CURRENT_SCHEMA"),
        "EXEC" | "EXECUTE" => kind == DbKind::SqlServer && words.iter().any(|x| x == "AS"),
        _ => false,
    }
}

/// For each result column, the table column it shows (`None`: computed, read-only). `Err` when
/// the result's columns do not line up with the query's select list and the table.
pub fn map_columns(q: &QueryTable, details: &TableDetails, result: &[String]) -> Result<Vec<Option<String>>, String> {
    let find = |n: &str| details.columns.iter().find(|c| c.name == n).or_else(|| {
        let mut it = details.columns.iter().filter(|c| c.name.eq_ignore_ascii_case(n));
        match (it.next(), it.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    });
    let mut out: Vec<(Option<String>, Option<String>)> = Vec::new(); // (table column, expected output)
    for item in &q.items {
        match item {
            SelectItem::All => {
                for c in &details.columns {
                    out.push((Some(c.name.clone()), Some(c.name.clone())));
                }
            }
            SelectItem::Column { name, output } => match find(name) {
                Some(c) => out.push((Some(c.name.clone()), Some(output.clone()))),
                None => out.push((None, None)),
            },
            SelectItem::Expr => out.push((None, None)),
        }
    }
    if out.len() != result.len() {
        return Err(format!(
            "the result has {} columns where the query and the table give {}; it cannot be edited",
            result.len(),
            out.len()
        ));
    }
    for ((_, expected), got) in out.iter().zip(result) {
        if let Some(e) = expected
            && !e.eq_ignore_ascii_case(got)
        {
            return Err(format!("the result column `{got}` does not match the table column `{e}`"));
        }
    }
    Ok(out.into_iter().map(|(c, _)| c).collect())
}

/// The key that identifies rows: the primary key when the result shows all its columns, else
/// the first unique key it shows. Returns the key's result column indices.
pub fn pick_key(details: &TableDetails, mapped: &[Option<String>]) -> Result<Vec<usize>, String> {
    let index_of = |col: &str| mapped.iter().position(|m| m.as_deref() == Some(col));
    let resolve = |cols: &[String]| -> Option<Vec<usize>> { cols.iter().map(|c| index_of(c)).collect() };
    let primary: Vec<String> = details.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    if !primary.is_empty()
        && let Some(k) = resolve(&primary)
    {
        return Ok(k);
    }
    for ix in details.indexes.iter().filter(|i| i.unique && !i.columns.is_empty()) {
        if let Some(k) = resolve(&ix.columns) {
            return Ok(k);
        }
    }
    let unique: Vec<&crate::IndexInfo> = details.indexes.iter().filter(|i| i.unique).collect();
    if primary.is_empty() && unique.is_empty() {
        return Err("the table has no primary or unique key".into());
    }
    let wanted = if primary.is_empty() {
        unique.first().map(|i| i.columns.join(", ")).unwrap_or_default()
    } else {
        primary.join(", ")
    };
    Err(format!("the result does not include the table's key columns ({wanted})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnInfo, IndexInfo};

    fn col(name: &str, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: "int".into(), nullable: !pk, default: None, primary_key: pk }
    }

    fn details() -> TableDetails {
        TableDetails {
            columns: vec![col("id", true), col("name", false), col("email", false)],
            indexes: vec![IndexInfo { name: "u".into(), columns: vec!["email".into()], unique: true, primary: false }],
            foreign_keys: Vec::new(),
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_table_queries_parse() {
        let q = parse(DbKind::Postgres, "select * from Users where id in (1, 2) order by id, name desc limit 10;").unwrap();
        assert_eq!((q.schema.as_deref(), q.table.as_str()), (None, "users"));
        assert_eq!(q.items, vec![SelectItem::All]);
        let q = parse(DbKind::Postgres, "SELECT u.id, u.\"Name\" AS n, upper(email) e FROM app.users u").unwrap();
        assert_eq!((q.schema.as_deref(), q.table.as_str()), (Some("app"), "users"));
        assert_eq!(
            q.items,
            vec![
                SelectItem::Column { name: "id".into(), output: "id".into() },
                SelectItem::Column { name: "Name".into(), output: "n".into() },
                SelectItem::Expr,
            ]
        );
        let q = parse(DbKind::SqlServer, "select top (50) [id], name from [dbo].[t] with (nolock)").unwrap();
        assert_eq!((q.schema.as_deref(), q.table.as_str()), (Some("dbo"), "t"));
        let q = parse(DbKind::Mysql, "select `id`, \"lit\", NULL, id + 1 as x from `db`.`t` limit 5, 10").unwrap();
        assert_eq!(q.items.get(1), Some(&SelectItem::Expr));
        assert_eq!(q.items.get(2), Some(&SelectItem::Expr));
        assert_eq!(q.items.get(3), Some(&SelectItem::Expr));
        let q = parse(DbKind::Sqlite, "select t.* from t").unwrap();
        assert_eq!(q.items, vec![SelectItem::All]);
    }

    #[test]
    fn other_queries_are_refused_with_a_reason() {
        for (sql, why) in [
            ("select * from a join b on a.id = b.id", "JOIN"),
            ("select * from a, b", "more than one table"),
            ("select count(*) from t", "aggregates"),
            ("select distinct name from t", "DISTINCT"),
            ("select name from t group by name", "GROUP"),
            ("select * from t union select * from u", "UNION"),
            ("select * from (select 1) x", "subquery"),
            ("select id, (select max(id) from u) from t", "subquery"),
            ("select * from generate_series(1, 3)", "table function"),
            ("with x as (select 1) select * from x", "SELECT"),
            ("update t set a = 1", "SELECT"),
            ("select 1", "no table"),
            ("select row_number() over () from t", "OVER"),
            ("select * from t natural left outer join u", "JOIN"),
            ("select * from a; select * from b", "more than one statement"),
            ("select * from a\nGO\nselect * from b", "more than one statement"),
            ("select * from otherdb.dbo.t", "another database"),
        ] {
            let e = parse(DbKind::Postgres, sql).unwrap_err();
            assert!(e.contains(why), "{sql}: {e}");
        }
    }

    #[test]
    fn schema_switches_are_recognised() {
        for sql in ["use shop", "SET search_path TO a, public", "set local search_path = x", "set role admin",
            "reset all", "discard all", "select set_config('search_path', 'x', false)", "SET SCHEMA 'x'"]
        {
            assert!(switches_schema(DbKind::Postgres, sql), "{sql}");
        }
        for sql in ["select * from t", "set statement_timeout = 5", "update t set search = 1"] {
            assert!(!switches_schema(DbKind::Postgres, sql), "{sql}");
        }
        assert!(switches_schema(DbKind::SqlServer, "EXECUTE AS USER = 'x'"));
    }

    #[test]
    fn columns_map_and_keys_are_chosen() {
        let d = details();
        let q = parse(DbKind::Postgres, "select * from users").unwrap();
        let m = map_columns(&q, &d, &names(&["id", "name", "email"])).unwrap();
        assert_eq!(m, vec![Some("id".into()), Some("name".into()), Some("email".into())]);
        assert_eq!(pick_key(&d, &m).unwrap(), vec![0]);
        // Aliased and computed columns.
        let q = parse(DbKind::Postgres, "select name as n, email, id * 2 as id from users").unwrap();
        let m = map_columns(&q, &d, &names(&["n", "email", "id"])).unwrap();
        assert_eq!(m, vec![Some("name".into()), Some("email".into()), None]);
        // The primary key is computed: the unique key identifies rows.
        assert_eq!(pick_key(&d, &m).unwrap(), vec![1]);
        let m = map_columns(&parse(DbKind::Postgres, "select name from users").unwrap(), &d, &names(&["name"])).unwrap();
        assert!(pick_key(&d, &m).unwrap_err().contains("key columns (id)"));
        // The result does not line up (a stale definition): refused.
        assert!(map_columns(&q, &d, &names(&["n", "email"])).is_err());
        assert!(map_columns(&parse(DbKind::Postgres, "select * from users").unwrap(), &d, &names(&["a", "b", "c"])).is_err());
        let none = TableDetails { columns: vec![col("a", false)], ..Default::default() };
        assert_eq!(pick_key(&none, &[Some("a".into())]).unwrap_err(), "the table has no primary or unique key");
    }
}
