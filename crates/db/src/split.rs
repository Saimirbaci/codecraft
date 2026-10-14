//! SQL statement splitting that respects strings, comments, quoted identifiers, PostgreSQL
//! dollar quoting and SQLite trigger bodies. Offsets are char offsets (like the text model).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dialect {
    #[default]
    Generic,
    Sqlite,
    Postgres,
    Mysql,
    /// Microsoft SQL Server: `[ident]`, nested comments, `GO` batch separators, BEGIN…END blocks.
    TSql,
    /// Redis consoles: one command per line (not SQL).
    Redis,
}

/// One statement: `[start, end)` in chars, trimmed of surrounding whitespace, without the `;`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    pub start: usize,
    pub end: usize,
    /// Char offset just past the terminating `;` (equal to `end` without one).
    pub end_with_delimiter: usize,
    pub text: String,
}

/// Split a script into statements. Statements that hold only comments are dropped.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<Statement> {
    if dialect == Dialect::Redis {
        return crate::redis::split_commands(sql);
    }
    let tsql = dialect == Dialect::TSql;
    // T-SQL `CREATE PROCEDURE|FUNCTION|TRIGGER`: only GO ends the body.
    let mut routine = false;
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut seg_start = 0usize;
    let mut has_code = false;
    // SQLite `CREATE TRIGGER … BEGIN … END;`: semicolons inside the body don't end it.
    let mut words: Vec<String> = Vec::new();
    let mut block_depth = 0usize;
    let mut i = 0usize;
    let n = chars.len();
    let at = |k: usize| chars.get(k).copied();
    while i < n {
        let c = chars.get(i).copied().unwrap_or(' ');
        let next = at(i + 1);
        match c {
            '-' if next == Some('-') => {
                i = skip_line(&chars, i);
                continue;
            }
            '#' if dialect == Dialect::Mysql => {
                i = skip_line(&chars, i);
                continue;
            }
            '/' if next == Some('*') => {
                i = skip_block_comment(&chars, i, dialect == Dialect::Postgres || tsql);
                continue;
            }
            '\'' => {
                has_code = true;
                let backslash = dialect == Dialect::Mysql
                    || dialect == Dialect::Postgres
                        && i.checked_sub(1).and_then(at).is_some_and(|p| p == 'E' || p == 'e')
                        && !i.checked_sub(2).and_then(at).is_some_and(is_ident_char);
                i = skip_quoted(&chars, i, '\'', backslash);
                continue;
            }
            '"' => {
                has_code = true;
                i = skip_quoted(&chars, i, '"', dialect == Dialect::Mysql);
                continue;
            }
            '`' if matches!(dialect, Dialect::Mysql | Dialect::Sqlite | Dialect::Generic) => {
                has_code = true;
                i = skip_quoted(&chars, i, '`', false);
                continue;
            }
            '[' if dialect == Dialect::Sqlite || tsql => {
                has_code = true;
                i = skip_quoted(&chars, i, ']', false);
                continue;
            }
            '$' if matches!(dialect, Dialect::Postgres | Dialect::Generic)
                && !i.checked_sub(1).and_then(at).is_some_and(is_ident_char) =>
            {
                has_code = true;
                if let Some(after) = skip_dollar_quoted(&chars, i) {
                    i = after;
                    continue;
                }
                i += 1;
                continue;
            }
            ';' if block_depth == 0 && !routine => {
                if has_code {
                    out.push(make_statement(&chars, seg_start, i, i + 1));
                }
                seg_start = i + 1;
                has_code = false;
                words.clear();
                i += 1;
                continue;
            }
            c if is_ident_start(c) => {
                let had_code = has_code;
                has_code = true;
                let start = i;
                while at(i).is_some_and(is_ident_char) {
                    i += 1;
                }
                let word: String = chars.get(start..i).unwrap_or(&[]).iter().collect::<String>().to_ascii_uppercase();
                if tsql
                    && word == "GO"
                    && let Some((line_end, count)) = go_line(&chars, start, i)
                {
                    if had_code {
                        for _ in 0..count {
                            out.push(make_statement(&chars, seg_start, start, line_end));
                        }
                    }
                    seg_start = line_end;
                    has_code = false;
                    words.clear();
                    block_depth = 0;
                    routine = false;
                    i = line_end;
                    continue;
                }
                if words.len() < 4 {
                    words.push(word.clone());
                }
                if tsql {
                    match word.as_str() {
                        "BEGIN" => {
                            let next = peek_word(&chars, i);
                            if !matches!(
                                next.as_deref(),
                                Some("TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG" | "CONVERSATION")
                            ) {
                                block_depth += 1;
                            }
                        }
                        "CASE" => block_depth += 1,
                        "END" => block_depth = block_depth.saturating_sub(1),
                        _ => {}
                    }
                    routine |= words.len() <= 4 && is_tsql_routine(&words);
                }
                if is_trigger(&words) && matches!(dialect, Dialect::Sqlite | Dialect::Generic) {
                    match word.as_str() {
                        "BEGIN" | "CASE" => block_depth += 1,
                        "END" => block_depth = block_depth.saturating_sub(1),
                        _ => {}
                    }
                }
                continue;
            }
            c if !c.is_whitespace() => has_code = true,
            _ => {}
        }
        i += 1;
    }
    if has_code {
        out.push(make_statement(&chars, seg_start, n, n));
    }
    out
}

/// The statement to run for a caret: the one containing it, else the one ending on the caret's
/// line, else the next one if only whitespace separates it, else the previous one.
pub fn statement_at(sql: &str, dialect: Dialect, caret: usize) -> Option<Statement> {
    let stmts = split_statements(sql, dialect);
    if let Some(s) = stmts.iter().find(|s| s.start <= caret && caret <= s.end_with_delimiter) {
        return Some(s.clone());
    }
    let chars: Vec<char> = sql.chars().collect();
    let between = |a: usize, b: usize| -> Vec<char> { chars.get(a.min(b)..b.min(chars.len())).unwrap_or(&[]).to_vec() };
    let prev = stmts.iter().rev().find(|s| s.end_with_delimiter <= caret);
    if let Some(p) = prev
        && !between(p.end_with_delimiter, caret).contains(&'\n')
    {
        return Some(p.clone());
    }
    let next = stmts.iter().find(|s| s.start >= caret);
    if let Some(nx) = next {
        let gap = between(caret, nx.start);
        let newlines = gap.iter().filter(|c| **c == '\n').count();
        if gap.iter().all(|c| c.is_whitespace()) && newlines <= 1 {
            return Some(nx.clone());
        }
    }
    prev.or(next).cloned()
}

/// The first keyword of a statement, upper-cased (comments and parentheses skipped).
pub fn first_keyword(sql: &str) -> Option<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars.get(i).copied().unwrap_or(' ');
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            i = skip_line(&chars, i);
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i = skip_block_comment(&chars, i, true);
        } else if c.is_whitespace() || c == '(' {
            i += 1;
        } else if is_ident_start(c) {
            let start = i;
            while chars.get(i).is_some_and(|c| is_ident_char(*c)) {
                i += 1;
            }
            return Some(chars.get(start..i).unwrap_or(&[]).iter().collect::<String>().to_ascii_uppercase());
        } else {
            return None;
        }
    }
    None
}

/// Whether a statement may change data or schema (the production guard and read-only data
/// sources check this). Statements known to only read are allowed; anything else counts as a
/// modification, so an unfamiliar command is guarded rather than let through.
pub fn is_modifying(sql: &str, dialect: Dialect) -> bool {
    if dialect == Dialect::Redis {
        return crate::redis::is_write_command(sql);
    }
    let words = crate::lex::words(sql, dialect);
    let Some(first) = words.first() else { return false };
    let has = |set: &[&str]| words.iter().skip(1).any(|w| set.contains(&w.as_str()));
    let second = words.get(1).map(String::as_str);
    match first.as_str() {
        // A T-SQL BEGIN … END block can hold anything.
        "BEGIN" if dialect == Dialect::TSql => !matches!(second, Some("TRAN" | "TRANSACTION" | "DISTRIBUTED")),
        "PRINT" | "WAITFOR" | "RAISERROR" | "THROW" | "SAVE" if dialect == Dialect::TSql => false,
        "SELECT" => has(&["INTO"]) && !has_mysql_variable_into(sql),
        "WITH" => has(&["INSERT", "UPDATE", "DELETE", "MERGE", "INTO"]),
        "EXPLAIN" => {
            // EXPLAIN ANALYZE runs the statement.
            let analyze = words.iter().skip(1).take(6).any(|w| w == "ANALYZE" || w == "ANALYSE");
            analyze && has(&["INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP", "ALTER", "TRUNCATE"])
        }
        "PRAGMA" => sql.contains('='),
        "SHOW" | "DESCRIBE" | "DESC" | "VALUES" | "TABLE" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "END"
        | "SAVEPOINT" | "RELEASE" | "SET" | "USE" | "FETCH" | "CLOSE" | "DECLARE" | "MOVE" | "LISTEN" | "UNLISTEN"
        | "DISCARD" | "RESET" | "HELP" | "ABORT" | "PREPARE" | "DEALLOCATE" => false,
        _ => true,
    }
}

/// `SELECT … INTO @var` (MySQL user variables) reads only.
fn has_mysql_variable_into(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    upper.split("INTO").skip(1).all(|rest| rest.trim_start().starts_with('@'))
}

/// How a statement changes the connection's transaction state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxEffect {
    None,
    Begin,
    End,
}

/// `BEGIN`/`START TRANSACTION` open a transaction; `COMMIT`/`ROLLBACK`/`END` close it (not
/// `ROLLBACK TO SAVEPOINT`). In MySQL, DDL commits implicitly.
pub fn transaction_effect(sql: &str, dialect: Dialect) -> TxEffect {
    if dialect == Dialect::Redis {
        return TxEffect::None;
    }
    let words = crate::lex::words(sql, dialect);
    let second = words.get(1).map(String::as_str);
    if dialect == Dialect::TSql {
        // `BEGIN` alone opens a block; `END` closes one. `ROLLBACK TRAN <savepoint>` is
        // indistinguishable from a named transaction here; the server's @@TRANCOUNT decides.
        return match words.first().map(String::as_str) {
            Some("BEGIN") if matches!(second, Some("TRAN" | "TRANSACTION" | "DISTRIBUTED")) => TxEffect::Begin,
            Some("COMMIT" | "ROLLBACK") => TxEffect::End,
            _ => TxEffect::None,
        };
    }
    match words.first().map(String::as_str) {
        Some("BEGIN") if dialect != Dialect::Postgres || second != Some("ATOMIC") => TxEffect::Begin,
        Some("START") if second == Some("TRANSACTION") => TxEffect::Begin,
        Some("COMMIT" | "END") => TxEffect::End,
        Some("ROLLBACK" | "ABORT") if !words.iter().any(|w| w == "TO") => TxEffect::End,
        Some("CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME") if dialect == Dialect::Mysql => TxEffect::End,
        _ => TxEffect::None,
    }
}

/// `CREATE [OR ALTER] PROC|PROCEDURE|FUNCTION|TRIGGER` and `ALTER …` of those (T-SQL).
fn is_tsql_routine(words: &[String]) -> bool {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let routine = |k: &str| matches!(k, "PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER");
    match w.as_slice() {
        ["CREATE", "OR", "ALTER", k, ..] => routine(k),
        ["CREATE" | "ALTER", k, ..] => routine(k),
        _ => false,
    }
}

/// A `GO [count]` line (T-SQL batch separator): `GO` alone on its line after whitespace,
/// optionally a repeat count and a `--` comment. Returns the offset after the line and the count.
fn go_line(chars: &[char], word_start: usize, word_end: usize) -> Option<(usize, usize)> {
    let mut b = word_start;
    while b > 0 {
        match chars.get(b - 1) {
            Some('\n') => break,
            Some(c) if c.is_whitespace() => b -= 1,
            _ => return None,
        }
    }
    let mut i = word_end;
    let skip_blanks = |i: &mut usize| {
        while chars.get(*i).is_some_and(|c| *c == ' ' || *c == '\t' || *c == '\r') {
            *i += 1;
        }
    };
    skip_blanks(&mut i);
    let digits_start = i;
    while chars.get(i).is_some_and(char::is_ascii_digit) {
        i += 1;
    }
    let count: usize = chars.get(digits_start..i).unwrap_or(&[]).iter().collect::<String>().parse().unwrap_or(1);
    skip_blanks(&mut i);
    if chars.get(i) == Some(&'-') && chars.get(i + 1) == Some(&'-') {
        i = skip_line(chars, i);
    }
    match chars.get(i) {
        None => Some((chars.len(), count.clamp(1, MAX_GO_REPEAT))),
        Some('\n') => Some((i + 1, count.clamp(1, MAX_GO_REPEAT))),
        Some(_) => None,
    }
}

/// `GO n` repeats a batch at most this often.
const MAX_GO_REPEAT: usize = 100;

/// The next word after `i` (comments and whitespace skipped), upper-cased.
fn peek_word(chars: &[char], mut i: usize) -> Option<String> {
    loop {
        match chars.get(i) {
            Some(c) if c.is_whitespace() => i += 1,
            Some('-') if chars.get(i + 1) == Some(&'-') => i = skip_line(chars, i),
            Some('/') if chars.get(i + 1) == Some(&'*') => i = skip_block_comment(chars, i, true),
            Some(c) if is_ident_start(*c) => {
                let start = i;
                while chars.get(i).is_some_and(|c| is_ident_char(*c)) {
                    i += 1;
                }
                return Some(chars.get(start..i)?.iter().collect::<String>().to_ascii_uppercase());
            }
            _ => return None,
        }
    }
}

fn is_trigger(words: &[String]) -> bool {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(w.as_slice(), ["CREATE", "TRIGGER", ..] | ["CREATE", "TEMP" | "TEMPORARY", "TRIGGER", ..])
}

fn make_statement(chars: &[char], from: usize, to: usize, end_with_delimiter: usize) -> Statement {
    let mut start = from;
    let mut end = to.min(chars.len());
    while start < end && chars.get(start).is_some_and(|c| c.is_whitespace()) {
        start += 1;
    }
    while end > start && chars.get(end - 1).is_some_and(|c| c.is_whitespace()) {
        end -= 1;
    }
    let text: String = chars.get(start..end).unwrap_or(&[]).iter().collect();
    Statement { start, end, end_with_delimiter: end_with_delimiter.min(chars.len()), text }
}

pub(crate) fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

pub(crate) fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

pub(crate) fn skip_line(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars.get(i) != Some(&'\n') {
        i += 1;
    }
    i
}

pub(crate) fn skip_block_comment(chars: &[char], start: usize, nested: bool) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < chars.len() {
        let c = chars.get(i).copied();
        let next = chars.get(i + 1).copied();
        if c == Some('/') && next == Some('*') && (depth == 0 || nested) {
            depth += 1;
            i += 2;
        } else if c == Some('*') && next == Some('/') {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    chars.len()
}

/// Skip a quoted run starting at `start` (the opening quote); doubled closing quotes escape.
pub(crate) fn skip_quoted(chars: &[char], start: usize, close: char, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars.get(i).copied().unwrap_or(' ');
        if backslash && c == '\\' {
            i += 2;
            continue;
        }
        if c == close {
            if close != ']' && chars.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    chars.len()
}

/// `$tag$ … $tag$`: returns the offset after the closing tag, or None if `$` doesn't open a tag.
pub(crate) fn skip_dollar_quoted(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if let Some(c) = chars.get(i)
        && !(c.is_alphabetic() || *c == '_' || *c == '$')
    {
        return None;
    }
    while chars.get(i).is_some_and(|c| c.is_alphanumeric() || *c == '_') {
        i += 1;
    }
    if chars.get(i) != Some(&'$') {
        return None;
    }
    let tag: Vec<char> = chars.get(start..=i)?.to_vec();
    let mut j = i + 1;
    while j + tag.len() <= chars.len() {
        if chars.get(j..j + tag.len()) == Some(tag.as_slice()) {
            return Some(j + tag.len());
        }
        j += 1;
    }
    Some(chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str, d: Dialect) -> Vec<String> {
        split_statements(sql, d).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn classifies_statements() {
        let m = |s: &str| is_modifying(s, Dialect::Generic);
        for read in [
            "select * from t",
            "  -- c\n SELECT 1",
            "with x as (select 1) select * from x",
            "explain select 1",
            "explain (analyze, format json) select 1",
            "show tables",
            "pragma table_info(t)",
            "begin",
            "commit",
            "select 'insert into' from t",
            "",
        ] {
            assert!(!m(read), "{read}");
        }
        for write in [
            "insert into t values (1)",
            "UPDATE t SET a = 1",
            "delete from t",
            "with x as (delete from t returning *) select * from x",
            "explain analyze delete from t",
            "create table t (a int)",
            "drop table t",
            "select * into t2 from t",
            "pragma foreign_keys = on",
            "vacuum",
            "frobnicate",
        ] {
            assert!(m(write), "{write}");
        }
        assert!(!is_modifying("select a into @x from t", Dialect::Mysql));
    }

    #[test]
    fn transaction_effects() {
        let e = |s: &str, d| transaction_effect(s, d);
        assert_eq!(e("BEGIN", Dialect::Postgres), TxEffect::Begin);
        assert_eq!(e("begin immediate transaction", Dialect::Sqlite), TxEffect::Begin);
        assert_eq!(e("start transaction read only", Dialect::Mysql), TxEffect::Begin);
        assert_eq!(e("commit", Dialect::Generic), TxEffect::End);
        assert_eq!(e("rollback", Dialect::Generic), TxEffect::End);
        assert_eq!(e("rollback to savepoint a", Dialect::Generic), TxEffect::None);
        assert_eq!(e("create table t (a int)", Dialect::Mysql), TxEffect::End);
        assert_eq!(e("create table t (a int)", Dialect::Postgres), TxEffect::None);
        assert_eq!(e("select 1", Dialect::Generic), TxEffect::None);
    }

    #[test]
    fn splits_on_semicolons_outside_literals() {
        let sql = "select 1; select ';' as x; select \"a;b\" from t -- c;\n; /* x; */ select 2";
        assert_eq!(
            texts(sql, Dialect::Generic),
            vec!["select 1", "select ';' as x", "select \"a;b\" from t -- c;", "/* x; */ select 2"]
        );
    }

    #[test]
    fn comment_only_segments_are_dropped() {
        assert_eq!(texts("-- just a comment\n;  ;\n/* x */", Dialect::Generic), Vec::<String>::new());
        assert!(split_statements("", Dialect::Generic).is_empty());
    }

    #[test]
    fn doubled_quotes_and_backslashes() {
        assert_eq!(texts("select 'it''s;'; select 2", Dialect::Generic), vec!["select 'it''s;'", "select 2"]);
        assert_eq!(texts(r"select 'a\';b'; select 2", Dialect::Mysql), vec![r"select 'a\';b'", "select 2"]);
        // Standard strings: backslash is literal, so the string ends at the second quote.
        assert_eq!(texts(r"select 'a\'; select 2", Dialect::Postgres), vec![r"select 'a\'", "select 2"]);
        assert_eq!(texts(r"select E'a\';b'; select 2", Dialect::Postgres), vec![r"select E'a\';b'", "select 2"]);
    }

    #[test]
    fn postgres_dollar_quoting() {
        let sql = "create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\n\
                   select $tag$ ; $$ ; $tag$; select $1";
        let t = texts(sql, Dialect::Postgres);
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[0].ends_with("language plpgsql"));
        assert_eq!(t[1], "select $tag$ ; $$ ; $tag$");
        assert_eq!(t[2], "select $1");
    }

    #[test]
    fn nested_block_comments_in_postgres() {
        assert_eq!(
            texts("/* a /* b; */ c; */ select 1; select 2", Dialect::Postgres),
            vec!["/* a /* b; */ c; */ select 1", "select 2"]
        );
    }

    #[test]
    fn mysql_backticks_and_hash_comments() {
        assert_eq!(
            texts("select `a;b` from t # x;\n; select 2", Dialect::Mysql),
            vec!["select `a;b` from t # x;", "select 2"]
        );
    }

    #[test]
    fn sqlite_trigger_bodies() {
        let sql = "create trigger tr after insert on t begin update t set x = case when 1 then 2 end; \
                   insert into u values(1); end; select 1";
        let t = texts(sql, Dialect::Sqlite);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("end"));
        assert_eq!(t[1], "select 1");
    }

    #[test]
    fn unterminated_literals_do_not_panic() {
        for sql in ["select 'abc", "select \"x", "/* open", "select $a$ x", "select `x", "x [y", "é😀'", "$", "$$"] {
            for d in [Dialect::Generic, Dialect::Sqlite, Dialect::Postgres, Dialect::Mysql] {
                let _ = split_statements(sql, d);
                for caret in 0..sql.chars().count() + 2 {
                    let _ = statement_at(sql, d, caret);
                }
            }
        }
    }

    #[test]
    fn offsets_are_char_offsets() {
        let sql = "select 'é😀';\nselect 2;";
        let s = split_statements(sql, Dialect::Generic);
        assert_eq!(s[0].start, 0);
        assert_eq!(s[0].end, 11);
        assert_eq!(s[1].start, 13);
        let chars: Vec<char> = sql.chars().collect();
        assert_eq!(chars[s[1].start..s[1].end].iter().collect::<String>(), "select 2");
    }

    #[test]
    fn statement_at_caret() {
        let sql = "select 1;\nselect 2;   \n\nselect 3";
        let at = |c| statement_at(sql, Dialect::Generic, c).map(|s| s.text);
        assert_eq!(at(0).as_deref(), Some("select 1"));
        assert_eq!(at(9).as_deref(), Some("select 1"));
        assert_eq!(at(12).as_deref(), Some("select 2"));
        // After the `;` on the same line: the statement just finished.
        assert_eq!(at(21).as_deref(), Some("select 2"));
        // On the blank line before `select 3`.
        assert_eq!(at(23).as_deref(), Some("select 3"));
        assert_eq!(at(1000).as_deref(), Some("select 3"));
        assert_eq!(statement_at("  ", Dialect::Generic, 1), None);
    }

    #[test]
    fn tsql_go_batches_and_blocks() {
        let d = Dialect::TSql;
        let sql = "SELECT 1\nGO\nCREATE PROCEDURE p AS\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND\ngo\nSELECT [a;b] FROM t; SELECT 'go'\n  GO 3 -- thrice\nPRINT 'x'";
        let t = texts(sql, d);
        assert_eq!(t[0], "SELECT 1");
        assert!(t[1].starts_with("CREATE PROCEDURE p AS") && t[1].ends_with("END"), "{t:?}");
        assert_eq!(t[2], "SELECT [a;b] FROM t");
        assert_eq!(&t[3..6], ["SELECT 'go'", "SELECT 'go'", "SELECT 'go'"]);
        assert_eq!(t[6], "PRINT 'x'");
        assert_eq!(t.len(), 7);
        // GO inside words, strings, comments or with trailing text is not a separator.
        assert_eq!(texts("SELECT go FROM t\nSELECT 1 /* \nGO\n */", d).len(), 1);
        assert_eq!(texts("SELECT 'a\nGO\nb'", d).len(), 1);
        assert_eq!(texts("SELECT 1\nGO x\nSELECT 2", d).len(), 1);
        // IF … BEGIN … END stays one statement; BEGIN TRAN does not open a block.
        let t = texts("IF 1=1 BEGIN UPDATE t SET a=1; DELETE FROM t; END; BEGIN TRAN; UPDATE t SET a = 2; COMMIT", d);
        assert_eq!(
            t,
            ["IF 1=1 BEGIN UPDATE t SET a=1; DELETE FROM t; END", "BEGIN TRAN", "UPDATE t SET a = 2", "COMMIT"]
        );
        assert_eq!(texts("SELECT CASE WHEN a=1 THEN 2 END; SELECT 3", d).len(), 2);
        assert_eq!(texts("GO\nGO\n", d).len(), 0);
        assert_eq!(texts("SELECT 1\nGO 999999999999999999999", d).len(), 1);
        assert_eq!(texts("SELECT 1\nGO 500", d).len(), MAX_GO_REPEAT);
        for hostile in [
            "GO",
            "G",
            "\nGO 1",
            "BEGIN BEGIN BEGIN",
            "END END",
            "[unterminated",
            "/* /* */",
            "CREATE PROC p AS SELECT 1; SELECT 2",
        ] {
            let _ = split_statements(hostile, d);
            let _ = statement_at(hostile, d, 3);
        }
        assert_eq!(texts("CREATE OR ALTER PROC p AS SELECT 1; SELECT 2\nGO\nSELECT 3", d).len(), 2);
    }

    #[test]
    fn tsql_classification() {
        let d = Dialect::TSql;
        assert!(is_modifying("BEGIN UPDATE t SET a = 1 END", d));
        assert!(!is_modifying("BEGIN TRAN", d));
        assert!(!is_modifying("PRINT 'x'", d));
        assert!(is_modifying("EXEC sp_rename 'a', 'b'", d));
        assert!(!is_modifying("SELECT * FROM [dbo].[t]", d));
        assert_eq!(transaction_effect("BEGIN TRANSACTION", d), TxEffect::Begin);
        assert_eq!(transaction_effect("BEGIN", d), TxEffect::None);
        assert_eq!(transaction_effect("END", d), TxEffect::None);
        assert_eq!(transaction_effect("COMMIT TRAN", d), TxEffect::End);
        assert_eq!(transaction_effect("ROLLBACK", d), TxEffect::End);
        assert_eq!(transaction_effect("MULTI", Dialect::Redis), TxEffect::None);
        assert!(is_modifying("SET k v", Dialect::Redis));
        assert!(!is_modifying("GET k", Dialect::Redis));
        assert_eq!(texts("GET a\nSET b 1", Dialect::Redis), ["GET a", "SET b 1"]);
    }

    #[test]
    fn first_keywords() {
        assert_eq!(first_keyword("  -- x\n /* y */ (select 1)").as_deref(), Some("SELECT"));
        assert_eq!(first_keyword("with x as (select 1) select * from x").as_deref(), Some("WITH"));
        assert_eq!(first_keyword("  "), None);
    }
}
