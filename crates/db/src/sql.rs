//! Identifier quoting and the queries the IDE generates (open table, sort).

use crate::DbKind;

/// Quote an identifier for the given database (`"a""b"`, `` `a``b` ``).
pub fn quote_ident(kind: DbKind, name: &str) -> String {
    match kind {
        DbKind::Mysql | DbKind::Mariadb => format!("`{}`", name.replace('`', "``")),
        DbKind::Sqlite | DbKind::Postgres => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// `schema.table`, quoted. An empty schema gives just the table.
pub fn qualified(kind: DbKind, schema: &str, table: &str) -> String {
    if schema.is_empty() {
        quote_ident(kind, table)
    } else {
        format!("{}.{}", quote_ident(kind, schema), quote_ident(kind, table))
    }
}

/// `SELECT * FROM schema.table [ORDER BY col [DESC]] [LIMIT n]`.
pub fn select_table(
    kind: DbKind,
    schema: &str,
    table: &str,
    order: Option<(&str, bool)>,
    limit: Option<usize>,
) -> String {
    let mut sql = format!("SELECT * FROM {}", qualified(kind, schema, table));
    if let Some((col, desc)) = order {
        sql.push_str(&format!(" ORDER BY {}{}", quote_ident(kind, col), if desc { " DESC" } else { "" }));
    }
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    sql
}

/// Re-sort an arbitrary query by the 1-based column position.
pub fn order_query(query: &str, column: usize, desc: bool) -> String {
    let inner = query.trim().trim_end_matches(';').trim_end();
    format!("SELECT * FROM ({inner}\n) AS q ORDER BY {}{}", column.max(1), if desc { " DESC" } else { "" })
}

/// `SELECT count(*) FROM schema.table`.
pub fn count_rows(kind: DbKind, schema: &str, table: &str) -> String {
    format!("SELECT count(*) FROM {}", qualified(kind, schema, table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident(DbKind::Postgres, "we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident(DbKind::Mysql, "a`b"), "`a``b`");
        assert_eq!(qualified(DbKind::Sqlite, "main", "t"), "\"main\".\"t\"");
        assert_eq!(qualified(DbKind::Sqlite, "", "t"), "\"t\"");
    }

    #[test]
    fn generated_queries() {
        assert_eq!(
            select_table(DbKind::Postgres, "public", "users", Some(("name", true)), Some(500)),
            "SELECT * FROM \"public\".\"users\" ORDER BY \"name\" DESC LIMIT 500"
        );
        assert_eq!(select_table(DbKind::Mysql, "db", "t", None, None), "SELECT * FROM `db`.`t`");
        assert_eq!(order_query("select a from t;", 2, false), "SELECT * FROM (select a from t\n) AS q ORDER BY 2");
    }
}
