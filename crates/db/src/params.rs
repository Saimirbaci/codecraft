//! Query parameters: `?` (and SQLite `?NNN`), `$1`, `:name` and `${name}` placeholders outside
//! strings, comments, quoted identifiers and dollar-quoted bodies. `::type` casts, `:=`, slices
//! like `a[1:n]` and PostgreSQL's `?|`/`?&` operators are not parameters.
//!
//! Values are substituted as typed (SQL text such as `42` or `'Ann'`), so a parameter can stand
//! for any expression. Positional `?` parameters are named `?1`, `?2`, … in order of appearance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::lex::{Tok, tokens};
use crate::split::{Dialect, is_ident_char, is_ident_start};

/// One placeholder occurrence: its name and `[start, end)` in chars.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// Every placeholder in `sql`, in order.
pub fn find_params(sql: &str, dialect: Dialect) -> Vec<Param> {
    let chars: Vec<char> = sql.chars().collect();
    let at = |k: usize| chars.get(k).copied();
    let toks = tokens(&chars, dialect);
    let mut out = Vec::new();
    let mut positional = 0usize;
    let mut skip_until = 0usize;
    for (ti, tok) in toks.iter().enumerate() {
        let Tok::Punct(i) = *tok else { continue };
        if i < skip_until {
            continue;
        }
        let Some(c) = at(i) else { continue };
        let next = at(i + 1);
        let prev = i.checked_sub(1).and_then(at);
        match c {
            '?' => {
                if dialect == Dialect::Postgres && next.is_some_and(|n| matches!(n, '|' | '&' | '-' | '#')) {
                    skip_until = i + 2;
                    continue;
                }
                let mut end = i + 1;
                while at(end).is_some_and(|d| d.is_ascii_digit()) {
                    end += 1;
                }
                let name = if end > i + 1 {
                    chars.get(i..end).unwrap_or(&[]).iter().collect()
                } else {
                    positional += 1;
                    format!("?{positional}")
                };
                out.push(Param { name, start: i, end });
                skip_until = end;
            }
            '$' if next == Some('{') => {
                let mut end = i + 2;
                while at(end).is_some_and(|d| d != '}' && d != '\n' && !d.is_whitespace()) && end < i + 130 {
                    end += 1;
                }
                if at(end) == Some('}') && end > i + 2 {
                    let name: String = chars.get(i..=end).unwrap_or(&[]).iter().collect();
                    out.push(Param { name, start: i, end: end + 1 });
                    skip_until = end + 1;
                }
            }
            '$' if next.is_some_and(|n| n.is_ascii_digit()) && !prev.is_some_and(is_ident_char) => {
                let mut end = i + 1;
                while at(end).is_some_and(|d| d.is_ascii_digit()) {
                    end += 1;
                }
                let name: String = chars.get(i..end).unwrap_or(&[]).iter().collect();
                out.push(Param { name, start: i, end });
                skip_until = end;
            }
            ':' => {
                if next == Some(':') {
                    // A `::type` cast (and whatever colons follow).
                    let mut end = i;
                    while at(end) == Some(':') {
                        end += 1;
                    }
                    skip_until = end;
                    continue;
                }
                if prev == Some(':') || next == Some('=') {
                    continue;
                }
                if prev.is_some_and(|p| is_ident_char(p) || matches!(p, ']' | ')' | '\'' | '"')) {
                    continue;
                }
                if let Some(Tok::Word(a, b)) = toks.get(ti + 1)
                    && *a == i + 1
                    && next.is_some_and(is_ident_start)
                {
                    let name: String = chars.get(i..*b).unwrap_or(&[]).iter().collect();
                    out.push(Param { name, start: i, end: *b });
                    skip_until = *b;
                }
            }
            _ => {}
        }
    }
    out
}

/// The distinct parameter names of `sql`, in order of first appearance.
pub fn param_names(sql: &str, dialect: Dialect) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for p in find_params(sql, dialect) {
        if !names.contains(&p.name) {
            names.push(p.name);
        }
    }
    names
}

/// Replace every placeholder with its value (SQL text, inserted as is). Fails with the name of
/// the first parameter that has no value.
pub fn substitute(sql: &str, dialect: Dialect, values: &BTreeMap<String, String>) -> Result<String, String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut last = 0usize;
    for p in find_params(sql, dialect) {
        let value = values.get(&p.name).ok_or_else(|| p.name.clone())?;
        out.extend(chars.get(last..p.start).unwrap_or(&[]).iter());
        out.push_str(value);
        last = p.end;
    }
    out.extend(chars.get(last.min(chars.len())..).unwrap_or(&[]).iter());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str, d: Dialect) -> Vec<String> {
        find_params(sql, d).into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn finds_every_style() {
        assert_eq!(
            names("select * from t where a = ? and b = ? and c = $1 and d = :name and e = ${env}", Dialect::Generic),
            ["?1", "?2", "$1", ":name", "${env}"]
        );
        assert_eq!(names("select ?3, ?", Dialect::Sqlite), ["?3", "?1"]);
        assert_eq!(names("insert into t values (:a,:b)", Dialect::Mysql), [":a", ":b"]);
    }

    #[test]
    fn ignores_casts_strings_comments_and_operators() {
        assert!(names("select a::int, b::text[] from t", Dialect::Postgres).is_empty());
        assert!(names("select ':x', '?', \"$1\" -- :y ?\n /* ${z} */", Dialect::Postgres).is_empty());
        assert!(names("select $$ :a ? $1 $$, $tag$ :b $tag$", Dialect::Postgres).is_empty());
        assert!(
            names("select doc ?| array['a'], doc ?& array['b'], arr[1:n], arr[lo:hi]", Dialect::Postgres).is_empty()
        );
        assert!(names("set @x := 1; select a:=b", Dialect::Mysql).is_empty());
        assert!(names("select a$1, x::int", Dialect::Postgres).is_empty());
        assert!(names("select '10:30' || x", Dialect::Generic).is_empty());
        // `${` without a closing brace is not a parameter; neither is `${}`.
        assert!(names("select ${ x, ${}", Dialect::Generic).is_empty());
        assert_eq!(names("where x = :p::int", Dialect::Postgres), [":p"]);
    }

    #[test]
    fn substitutes_values_as_typed() {
        let mut v = BTreeMap::new();
        v.insert("?1".to_string(), "'Ann'".to_string());
        v.insert(":id".to_string(), "42".to_string());
        assert_eq!(
            substitute("select * from t where name = ? and id = :id or id = :id", Dialect::Generic, &v).unwrap(),
            "select * from t where name = 'Ann' and id = 42 or id = 42"
        );
        assert_eq!(substitute("select ü, :x", Dialect::Generic, &v).unwrap_err(), ":x");
        assert_eq!(param_names("select :a, :b, :a", Dialect::Generic), [":a", ":b"]);
        assert_eq!(substitute("select 1", Dialect::Generic, &BTreeMap::new()).unwrap(), "select 1");
    }
}
