//! Identifier quoting and the queries the IDE generates (open table, sort).

use crate::DbKind;

/// Quote an identifier for the given database (`"a""b"`, `` `a``b` ``, `[a]]b]`).
pub fn quote_ident(kind: DbKind, name: &str) -> String {
    match kind {
        DbKind::Mysql | DbKind::Mariadb | DbKind::ClickHouse => format!("`{}`", name.replace('`', "``")),
        DbKind::SqlServer => format!("[{}]", name.replace(']', "]]")),
        DbKind::Sqlite | DbKind::Postgres | DbKind::DuckDb | DbKind::Redis => {
            format!("\"{}\"", name.replace('"', "\"\""))
        }
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
    // SQL Server has TOP instead of LIMIT.
    let top = match (kind, limit) {
        (DbKind::SqlServer, Some(n)) => format!("TOP ({n}) "),
        _ => String::new(),
    };
    let mut sql = format!("SELECT {top}* FROM {}", qualified(kind, schema, table));
    if let Some((col, desc)) = order {
        sql.push_str(&format!(" ORDER BY {}{}", quote_ident(kind, col), if desc { " DESC" } else { "" }));
    }
    if let Some(n) = limit.filter(|_| kind != DbKind::SqlServer) {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    sql
}

/// Re-sort an arbitrary query by the 1-based column position.
pub fn order_query(query: &str, column: usize, desc: bool) -> String {
    let inner = query.trim().trim_end_matches(';').trim_end();
    format!("SELECT * FROM ({inner}\n) AS q ORDER BY {}{}", column.max(1), if desc { " DESC" } else { "" })
}

/// Table data with the grid's filter row: `SELECT * FROM t [WHERE (filter)] [ORDER BY order]`.
/// `filter` and `order` are SQL typed by the user (empty: none).
pub fn select_table_where(kind: DbKind, schema: &str, table: &str, filter: &str, order: &str) -> String {
    let mut sql = format!("SELECT * FROM {}", qualified(kind, schema, table));
    push_filter(&mut sql, filter, order);
    sql
}

/// Filter and order an arbitrary query's rows: `SELECT * FROM (query) AS q WHERE … ORDER BY …`.
pub fn filter_query(query: &str, filter: &str, order: &str) -> String {
    let inner = query.trim().trim_end_matches(';').trim_end();
    if filter.trim().is_empty() && order.trim().is_empty() {
        return inner.to_string();
    }
    let mut sql = format!("SELECT * FROM ({inner}\n) AS q");
    push_filter(&mut sql, filter, order);
    sql
}

fn push_filter(sql: &mut String, filter: &str, order: &str) {
    let clean = |s: &str| s.trim().trim_end_matches(';').trim_end().to_string();
    let (filter, order) = (clean(filter), clean(order));
    if !filter.is_empty() {
        sql.push_str(&format!(" WHERE ({filter})"));
    }
    if !order.is_empty() {
        sql.push_str(&format!(" ORDER BY {order}"));
    }
}

/// `"col" [DESC]`, for an ORDER BY clause.
pub fn order_by_column(kind: DbKind, column: &str, desc: bool) -> String {
    format!("{}{}", quote_ident(kind, column), if desc { " DESC" } else { "" })
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
        assert_eq!(qualified(DbKind::SqlServer, "dbo", "a]b"), "[dbo].[a]]b]");
        assert_eq!(quote_ident(DbKind::ClickHouse, "x"), "`x`");
        assert_eq!(select_table(DbKind::SqlServer, "dbo", "t", None, Some(5)), "SELECT TOP (5) * FROM [dbo].[t]");
    }

    #[test]
    fn generated_queries() {
        assert_eq!(
            select_table(DbKind::Postgres, "public", "users", Some(("name", true)), Some(500)),
            "SELECT * FROM \"public\".\"users\" ORDER BY \"name\" DESC LIMIT 500"
        );
        assert_eq!(select_table(DbKind::Mysql, "db", "t", None, None), "SELECT * FROM `db`.`t`");
        assert_eq!(order_query("select a from t;", 2, false), "SELECT * FROM (select a from t\n) AS q ORDER BY 2");
        assert_eq!(
            select_table_where(DbKind::Sqlite, "main", "t", " a > 1 or b = 2; ", "a desc"),
            "SELECT * FROM \"main\".\"t\" WHERE (a > 1 or b = 2) ORDER BY a desc"
        );
        assert_eq!(select_table_where(DbKind::Mysql, "", "t", "", ""), "SELECT * FROM `t`");
        assert_eq!(filter_query("select * from t;", "", ""), "select * from t");
        assert_eq!(
            filter_query("select * from t", "x = 1", ""),
            "SELECT * FROM (select * from t\n) AS q WHERE (x = 1)"
        );
        assert_eq!(order_by_column(DbKind::Postgres, "Name", true), "\"Name\" DESC");
    }
}
