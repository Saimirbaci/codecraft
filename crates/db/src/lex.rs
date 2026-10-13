//! A small SQL lexer shared by parameter detection and statement classification: words and
//! punctuation outside strings, comments, quoted identifiers and dollar-quoted bodies.

use crate::split::{
    Dialect, is_ident_char, is_ident_start, skip_block_comment, skip_dollar_quoted, skip_line, skip_quoted,
};

/// A token, as char offsets into the statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    /// An unquoted word `[start, end)`.
    Word(usize, usize),
    /// One punctuation (or digit) character.
    Punct(usize),
    /// A string, quoted identifier or dollar-quoted body `[start, end)`.
    Literal(usize, usize),
}

/// The tokens of `chars`. `$` that starts `${name}` or `$1` is punctuation, never a dollar quote.
pub(crate) fn tokens(chars: &[char], dialect: Dialect) -> Vec<Tok> {
    let mut out = Vec::new();
    let at = |k: usize| chars.get(k).copied();
    let mut i = 0usize;
    while let Some(c) = at(i) {
        let next = at(i + 1);
        let start = i;
        match c {
            '-' if next == Some('-') => i = skip_line(chars, i),
            '#' if dialect == Dialect::Mysql => i = skip_line(chars, i),
            '/' if next == Some('*') => i = skip_block_comment(chars, i, dialect == Dialect::Postgres),
            '\'' => {
                let backslash = dialect == Dialect::Mysql
                    || dialect == Dialect::Postgres
                        && i.checked_sub(1).and_then(at).is_some_and(|p| p == 'E' || p == 'e')
                        && !i.checked_sub(2).and_then(at).is_some_and(is_ident_char);
                i = skip_quoted(chars, i, '\'', backslash);
                out.push(Tok::Literal(start, i));
            }
            '"' => {
                i = skip_quoted(chars, i, '"', dialect == Dialect::Mysql);
                out.push(Tok::Literal(start, i));
            }
            '`' if matches!(dialect, Dialect::Mysql | Dialect::Sqlite | Dialect::Generic) => {
                i = skip_quoted(chars, i, '`', false);
                out.push(Tok::Literal(start, i));
            }
            '[' if dialect == Dialect::Sqlite => {
                i = skip_quoted(chars, i, ']', false);
                out.push(Tok::Literal(start, i));
            }
            '$' if matches!(dialect, Dialect::Postgres | Dialect::Generic)
                && !i.checked_sub(1).and_then(at).is_some_and(is_ident_char)
                && next != Some('{')
                && !next.is_some_and(|n| n.is_ascii_digit()) =>
            {
                match skip_dollar_quoted(chars, i) {
                    Some(end) => {
                        i = end;
                        out.push(Tok::Literal(start, end));
                    }
                    None => {
                        out.push(Tok::Punct(i));
                        i += 1;
                    }
                }
            }
            c if is_ident_start(c) => {
                while at(i).is_some_and(is_ident_char) {
                    i += 1;
                }
                out.push(Tok::Word(start, i));
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                out.push(Tok::Punct(i));
                i += 1;
            }
        }
        // Every branch advances; this guards against a helper that would not.
        if i <= start {
            i = start + 1;
        }
    }
    out
}

/// The upper-cased words of a statement, in order.
pub(crate) fn words(sql: &str, dialect: Dialect) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    tokens(&chars, dialect)
        .into_iter()
        .filter_map(|t| match t {
            Tok::Word(a, b) => Some(chars.get(a..b).unwrap_or(&[]).iter().collect::<String>().to_ascii_uppercase()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_literals_and_comments() {
        let w = words("select 'from x', \"y\" -- delete\n from t /* drop */ where $$ insert $$ = a", Dialect::Postgres);
        assert_eq!(w, ["SELECT", "FROM", "T", "WHERE", "A"]);
        let chars: Vec<char> = "a $1 ${b}".chars().collect();
        let t = tokens(&chars, Dialect::Postgres);
        assert!(t.contains(&Tok::Punct(2)) && t.contains(&Tok::Punct(5)), "{t:?}");
        // Unterminated literals end the input without looping forever.
        assert_eq!(words("select 'abc", Dialect::Generic), ["SELECT"]);
        assert_eq!(words("x /* never closed", Dialect::Postgres), ["X"]);
    }
}
