//! Find Usages of a table or column across the project's `.sql` files and the SQL in Rust
//! string literals. Matching is lexical (aliases and qualifiers resolved per statement); the
//! scan runs on a worker thread.

use std::collections::HashMap;
use std::path::PathBuf;

use crossbeam_channel::{Receiver, unbounded};
use serde::Serialize;

use codecraft_fs::FileMatch;
use codecraft_syntax::sql_inject;
use codecraft_syntax::sql_scope::{self, TableRef};
use codecraft_text::Document;

/// Files larger than this are skipped (bytes).
const MAX_FILE_BYTES: u64 = 4 << 20;
/// Files scanned at most.
pub const MAX_FILES: usize = 20_000;
/// Matches reported at most.
const MAX_MATCHES: usize = 10_000;

/// What to look for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsageTarget {
    pub schema: Option<String>,
    pub table: String,
    pub column: Option<String>,
}

impl UsageTarget {
    pub fn title(&self) -> String {
        match &self.column {
            Some(c) => format!("Usages of column '{}.{c}'", self.table),
            None => format!("Usages of table '{}'", self.table),
        }
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn matches_table(t: &TableRef, target: &UsageTarget, ctes: &[String]) -> bool {
    eq(&t.name, &target.table)
        && match (&t.schema, &target.schema) {
            (Some(a), Some(b)) => eq(a, b),
            (None, _) => !ctes.iter().any(|c| eq(c, &t.name)),
            _ => true,
        }
}

/// Char ranges in `sql` where `target` is used.
pub fn find_in_sql(sql: &str, target: &UsageTarget) -> Vec<(usize, usize)> {
    let toks = sql_scope::tokens(sql);
    let mut out = Vec::new();
    for st in sql_scope::analyze(sql) {
        let hits: Vec<&TableRef> = st.tables.iter().filter(|t| matches_table(t, target, &st.ctes)).collect();
        if hits.is_empty() {
            continue;
        }
        match &target.column {
            None => {
                out.extend(hits.iter().map(|t| (t.start, t.end)));
                // `table.column` where the table is used by its name.
                for (k, t) in toks.iter().enumerate() {
                    if t.start < st.start || t.end > st.end {
                        continue;
                    }
                    let qualifier = toks.get(k + 1).is_some_and(|n| n.is_sym('.'))
                        && !k.checked_sub(1).and_then(|p| toks.get(p)).is_some_and(|p| p.is_sym('.'));
                    let unaliased = hits.iter().any(|h| h.alias.is_none() && (h.start, h.end) != (t.start, t.end));
                    if qualifier && unaliased && t.name(true).is_some_and(|n| eq(&n, &target.table)) {
                        out.push((t.start, t.end));
                    }
                }
            }
            Some(col) => {
                for c in st.columns.iter().filter(|c| eq(&c.name, col)) {
                    let ok = match &c.qualifier {
                        Some(q) => hits.iter().any(|h| h.alias.as_deref().is_some_and(|a| eq(a, q)) || eq(&h.name, q)),
                        // Unqualified: one of the statement's tables is the target.
                        None => true,
                    };
                    if ok {
                        out.push((c.start, c.end));
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Usages in a file's text (`.sql` as a whole, `.rs` in its SQL string literals).
pub fn find_in_file(path: &std::path::Path, text: &str, target: &UsageTarget) -> Vec<FileMatch> {
    let rust = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("rs"));
    let ranges: Vec<(usize, usize)> = if rust {
        if !text.contains(&target.table) && !text.to_ascii_lowercase().contains(&target.table.to_ascii_lowercase()) {
            return Vec::new();
        }
        sql_inject::find_injections(text)
            .iter()
            .flat_map(|inj| find_in_sql(&inj.sql, target).into_iter().map(|(a, b)| inj.to_rust_range(a, b)))
            .collect()
    } else {
        find_in_sql(text, target)
    };
    if ranges.is_empty() {
        return Vec::new();
    }
    let doc = Document::from_text(text);
    ranges
        .into_iter()
        .map(|(a, b)| {
            let start = doc.offset_to_position(a);
            let end = doc.offset_to_position(b);
            let line_text: String = doc.line_text(start.line).unwrap_or_default().chars().take(500).collect();
            let end_column = if end.line == start.line { end.column } else { line_text.chars().count() };
            FileMatch { path: path.to_path_buf(), line: start.line, column: start.column, end_column, line_text }
        })
        .collect()
}

/// Whether a path is searched (`.sql` and `.rs`).
pub fn searched(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql") || e.eq_ignore_ascii_case("rs"))
}

/// Search `files` (open documents' text from `open`) on a worker thread.
pub(crate) fn spawn(
    files: Vec<PathBuf>,
    open: HashMap<PathBuf, String>,
    target: UsageTarget,
) -> Result<Receiver<Vec<FileMatch>>, String> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("sql-usages".into())
        .spawn(move || {
            let mut out = Vec::new();
            for path in files.into_iter().take(MAX_FILES) {
                let text = match open.get(&path) {
                    Some(t) => t.clone(),
                    None => {
                        if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                            continue;
                        }
                        match codecraft_fs::read_text_file(&path) {
                            Ok(t) => t,
                            Err(_) => continue,
                        }
                    }
                };
                out.extend(find_in_file(&path, &text, &target));
                if out.len() >= MAX_MATCHES {
                    out.truncate(MAX_MATCHES);
                    break;
                }
            }
            let _ = tx.send(out);
        })
        .map_err(|e| format!("cannot start the search: {e}"))?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(sql: &str, table: &str, column: Option<&str>) -> Vec<String> {
        let t = UsageTarget { schema: None, table: table.into(), column: column.map(str::to_string) };
        let chars: Vec<char> = sql.chars().collect();
        find_in_sql(sql, &t).into_iter().map(|(a, b)| chars[a..b].iter().collect()).collect()
    }

    #[test]
    fn tables_and_columns_through_aliases() {
        let sql = "SELECT u.name, users.id FROM users JOIN orders o ON o.uid = users.id; INSERT INTO users (name) VALUES ('x'); SELECT name FROM orders";
        assert_eq!(found(sql, "users", None), ["users", "users", "users", "users"]);
        assert_eq!(found(sql, "USERS", Some("name")), ["name"]);
        assert_eq!(found(sql, "orders", Some("uid")), ["uid"]);
        assert_eq!(found("SELECT o.total FROM orders o", "orders", Some("total")), ["total"]);
        // A CTE with the table's name is not the table.
        assert!(found("WITH users AS (SELECT 1) SELECT * FROM users", "users", None).is_empty());
        let t = UsageTarget { schema: Some("app".into()), table: "users".into(), column: None };
        assert_eq!(find_in_sql("SELECT * FROM other.users, app.users, users", &t).len(), 2);
    }

    #[test]
    fn rust_strings_are_searched() {
        let text = "fn f() {\n    sqlx::query(\"SELECT id FROM users WHERE id = $1\");\n}\n";
        let t = UsageTarget { schema: None, table: "users".into(), column: Some("id".into()) };
        let m = find_in_file(std::path::Path::new("/p/src/a.rs"), text, &t);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].line, m[0].column, m[0].end_column), (1, 24, 26));
        assert!(searched(std::path::Path::new("x.SQL")) && !searched(std::path::Path::new("x.py")));
        for junk in ["", "\"", "SELECT", "é"] {
            let _ = find_in_file(std::path::Path::new("a.sql"), junk, &t);
            let _ = find_in_file(std::path::Path::new("a.rs"), junk, &t);
        }
    }
}
