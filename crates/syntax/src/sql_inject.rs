//! SQL injected into Rust string literals: which literals hold SQL, the SQL they hold (escapes
//! decoded) and the way back from SQL offsets to the Rust text.
//!
//! A literal is SQL when it is
//! - an argument of a sqlx query macro (`query!`, `query_as!`, `query_scalar!` and their
//!   `_unchecked` forms; the `query_file*!` macros take a path and are skipped): the first
//!   string literal among the first two arguments;
//! - the first argument of a query function or method that starts with an SQL keyword: sqlx
//!   `query`/`query_as`/`query_scalar`/`query_with`/…/`raw_sql`, diesel `sql_query`, rusqlite
//!   `execute`/`prepare`/`prepare_cached`/`query_row`/`execute_batch`, tokio-postgres and
//!   postgres `query`/`query_one`/`query_opt`/`execute`/`batch_execute`/`prepare`/`simple_query`;
//! - the first string literal after a `// language=SQL` comment (case-insensitive, also as a
//!   block comment) in the same statement.
//!
//! Detection works on the `rustc_lexer` tokens of the file (no parsing), so it is cheap enough
//! to run on every edit of the file. Offsets are char offsets.

use ra_ap_rustc_lexer::{LiteralKind, TokenKind};
use serde::Serialize;

use crate::common::LineIndex;
use crate::rust::{self, Tok};
use crate::{HighlightKind, Highlights, Language, LineSpan};

/// One injected SQL fragment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlInjection {
    /// Char range of the literal's content in the Rust text (without quotes, `r` or `#`s).
    pub start: usize,
    pub end: usize,
    /// Char range of the whole literal.
    pub literal_start: usize,
    pub literal_end: usize,
    /// A raw string (`r"…"`, `r#"…"#`); `hashes` is the number of `#`s.
    pub raw: bool,
    pub hashes: usize,
    /// What marks the literal as SQL: `query!`, `sql_query`, `.execute`, `language=SQL`.
    pub api: String,
    /// The SQL text (escapes of normal strings decoded).
    pub sql: String,
    /// For each char of `sql`, its char range in the Rust text.
    #[serde(skip)]
    map: Vec<(usize, usize)>,
}

impl SqlInjection {
    /// The Rust offset of SQL offset `at` (the content end for `at >= sql.len()`).
    pub fn to_rust(&self, at: usize) -> usize {
        self.map.get(at).map_or(self.end, |r| r.0)
    }

    /// The Rust range of the SQL range `start..end`.
    pub fn to_rust_range(&self, start: usize, end: usize) -> (usize, usize) {
        let s = self.to_rust(start);
        let e = match end.checked_sub(1).and_then(|i| self.map.get(i)) {
            Some(r) if end > start => r.1,
            _ => s,
        };
        (s, e.max(s))
    }

    /// The SQL offset at Rust offset `at`, when `at` is inside the content (its end included).
    pub fn to_sql(&self, at: usize) -> Option<usize> {
        if at < self.start || at > self.end {
            return None;
        }
        Some(self.map.partition_point(|r| r.0 < at))
    }

    /// Whether the Rust offset `at` is inside the content (its end included: a caret there
    /// is still typing SQL).
    pub fn contains(&self, at: usize) -> bool {
        self.start <= at && at <= self.end
    }
}

/// The SQL fragments of a Rust text, in order.
pub fn find_injections(text: &str) -> Vec<SqlInjection> {
    let toks = rust::lex(text);
    Finder::new(text, &toks).run()
}

/// The injection containing Rust offset `at`.
pub fn injection_at(injections: &[SqlInjection], at: usize) -> Option<&SqlInjection> {
    injections.iter().find(|i| i.contains(at))
}

/// SQL highlighting of the fragments as per-line spans of the Rust text (empty lines where
/// there is no SQL). Unknown chars keep the string colour (no span).
pub fn injected_highlights(text: &str, injections: &[SqlInjection]) -> Highlights {
    let index = LineIndex::new(text);
    let mut lines: Vec<Vec<LineSpan>> = vec![Vec::new(); index.len_lines()];
    for inj in injections {
        let sql_index = LineIndex::new(&inj.sql);
        let hl = crate::highlight(Language::Sql, &inj.sql);
        for line in 0..hl.len_lines() {
            let base = sql_index.line_start(line);
            for span in hl.line(line) {
                if span.kind == HighlightKind::Invalid {
                    continue;
                }
                // Runs of chars whose Rust ranges touch become one span.
                let mut run: Option<(usize, usize)> = None;
                for k in base + span.start..base + span.end {
                    let Some(&(a, b)) = inj.map.get(k) else { break };
                    run = match run {
                        Some((s, e)) if e == a => Some((s, b)),
                        Some(r) => {
                            push_run(&mut lines, &index, r, span.kind);
                            Some((a, b))
                        }
                        None => Some((a, b)),
                    };
                }
                if let Some(r) = run {
                    push_run(&mut lines, &index, r, span.kind);
                }
            }
        }
    }
    Highlights::from_lines(lines)
}

/// Add the Rust char range `(start, end)` to the line spans, split at line ends.
fn push_run(lines: &mut [Vec<LineSpan>], index: &LineIndex, (start, end): (usize, usize), kind: HighlightKind) {
    let mut at = start;
    while at < end {
        let line = index.line_of(at);
        let line_start = index.line_start(line);
        let line_end = index.line_end(line);
        let stop = end.min(line_end.max(at + 1));
        if let Some(v) = lines.get_mut(line) {
            let (s, e) = (at.saturating_sub(line_start), stop.saturating_sub(line_start));
            match v.last_mut() {
                Some(last) if last.end == s && last.kind == kind => last.end = e,
                _ => v.push(LineSpan { start: s, end: e, kind }),
            }
        }
        at = match index.next_line_start(line) {
            Some(n) if n > at => n.max(stop),
            _ => break,
        };
    }
}

/// `sql` as the content of a Rust string literal: escaped for a normal string, unchanged for a
/// raw one (`None` when a raw string with `hashes` cannot hold it).
pub fn encode_for_literal(sql: &str, raw: bool, hashes: usize) -> Option<String> {
    if raw {
        let closer: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
        return (!sql.contains(&closer)).then(|| sql.to_string());
    }
    let mut out = String::with_capacity(sql.len() + 8);
    for c in sql.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    Some(out)
}

/// Whether `sql` starts (after whitespace, comments and `(`) with a statement keyword.
pub fn looks_like_sql(sql: &str) -> bool {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, r)| r);
        } else {
            break;
        }
    }
    let word: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).take(16).collect();
    matches!(
        word.to_ascii_lowercase().as_str(),
        "select"
            | "insert"
            | "update"
            | "delete"
            | "with"
            | "create"
            | "drop"
            | "alter"
            | "truncate"
            | "replace"
            | "merge"
            | "upsert"
            | "begin"
            | "commit"
            | "rollback"
            | "savepoint"
            | "release"
            | "pragma"
            | "explain"
            | "analyze"
            | "vacuum"
            | "values"
            | "set"
            | "show"
            | "grant"
            | "revoke"
            | "copy"
            | "lock"
            | "call"
            | "attach"
            | "detach"
            | "reindex"
            | "comment"
            | "refresh"
            | "listen"
            | "notify"
            | "unlisten"
            | "declare"
            | "fetch"
            | "close"
            | "table"
            | "use"
            | "describe"
    )
}

/// sqlx query macros whose first string literal argument is SQL.
fn is_query_macro(name: &str) -> bool {
    matches!(
        name,
        "query" | "query_as" | "query_scalar" | "query_unchecked" | "query_as_unchecked" | "query_scalar_unchecked"
    )
}

/// Free functions taking SQL first (sqlx, diesel).
fn is_query_fn(name: &str) -> bool {
    matches!(
        name,
        "query"
            | "query_as"
            | "query_scalar"
            | "query_with"
            | "query_as_with"
            | "query_scalar_with"
            | "raw_sql"
            | "sql_query"
    )
}

/// Methods taking SQL first (rusqlite, postgres, tokio-postgres, sqlx executors).
fn is_query_method(name: &str) -> bool {
    matches!(
        name,
        "execute"
            | "prepare"
            | "prepare_cached"
            | "query_row"
            | "query_row_and_then"
            | "execute_batch"
            | "query"
            | "query_one"
            | "query_opt"
            | "query_raw"
            | "batch_execute"
            | "simple_query"
            | "prepare_typed"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Callee {
    /// A query macro.
    Macro,
    /// A query function or method: the literal must look like SQL.
    Call,
    None,
}

struct Frame {
    callee: Callee,
    /// The api name for the report.
    api: String,
    arg: usize,
    found: bool,
}

struct Finder<'a> {
    text: &'a str,
    toks: &'a [Tok],
    /// Indices of the significant tokens.
    sig: Vec<usize>,
}

impl<'a> Finder<'a> {
    fn new(text: &'a str, toks: &'a [Tok]) -> Self {
        let sig = toks.iter().enumerate().filter(|(_, t)| !rust::is_trivia(t.kind)).map(|(i, _)| i).collect();
        Self { text, toks, sig }
    }

    fn tok(&self, p: usize) -> Option<&'a Tok> {
        self.sig.get(p).and_then(|&i| self.toks.get(i))
    }

    fn text_of(&self, t: &Tok) -> &'a str {
        self.text.get(t.start..t.end).unwrap_or("")
    }

    fn sym(&self, p: usize) -> &'a str {
        self.tok(p).map_or("", |t| self.text_of(t))
    }

    fn ident(&self, p: usize) -> Option<&'a str> {
        self.tok(p).filter(|t| t.kind == TokenKind::Ident).map(|t| self.text_of(t))
    }

    /// The callee of the bracket at significant position `p`.
    fn callee(&self, p: usize) -> (Callee, String) {
        let Some(q) = p.checked_sub(1) else { return (Callee::None, String::new()) };
        // `name!(`
        if self.sym(q) == "!"
            && let Some(name) = q.checked_sub(1).and_then(|r| self.ident(r))
        {
            return if is_query_macro(name) {
                (Callee::Macro, format!("{name}!"))
            } else {
                (Callee::None, String::new())
            };
        }
        if self.tok(p).is_none_or(|t| t.kind != TokenKind::OpenParen) {
            return (Callee::None, String::new());
        }
        // `name::<…>(`: skip the turbofish.
        let mut r = q;
        if self.sym(r) == ">" {
            let mut depth = 0usize;
            loop {
                match self.sym(r) {
                    ">" => depth += 1,
                    "<" => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                    }
                    "" | ";" | "{" | "}" => return (Callee::None, String::new()),
                    _ => {}
                }
                let Some(prev) = r.checked_sub(1) else { return (Callee::None, String::new()) };
                r = prev;
            }
            // `::` before `<`.
            match r.checked_sub(2) {
                Some(s) if self.sym(s + 1) == ":" && self.sym(s) == ":" => match s.checked_sub(1) {
                    Some(n) => r = n,
                    None => return (Callee::None, String::new()),
                },
                _ => return (Callee::None, String::new()),
            }
        }
        let Some(name) = self.ident(r) else { return (Callee::None, String::new()) };
        let before = r.checked_sub(1);
        if before.is_some_and(|b| self.ident(b) == Some("fn")) {
            return (Callee::None, String::new());
        }
        if before.is_some_and(|b| self.sym(b) == ".") {
            return if is_query_method(name) {
                (Callee::Call, format!(".{name}"))
            } else {
                (Callee::None, String::new())
            };
        }
        if is_query_fn(name) { (Callee::Call, name.to_string()) } else { (Callee::None, String::new()) }
    }

    fn run(&self) -> Vec<SqlInjection> {
        let mut out = Vec::new();
        let mut frames: Vec<Frame> = Vec::new();
        // A `language=SQL` comment waiting for its literal.
        let mut marked = false;
        let mut p = 0usize;
        for t in self.toks {
            match t.kind {
                TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => {
                    if is_language_sql_comment(self.text_of(t)) {
                        marked = true;
                    }
                    continue;
                }
                TokenKind::Whitespace | TokenKind::Frontmatter { .. } => continue,
                _ => {}
            }
            match t.kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => {
                    let (callee, api) = self.callee(p);
                    frames.push(Frame { callee, api, arg: 0, found: false });
                }
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    frames.pop();
                }
                TokenKind::Comma => {
                    if let Some(f) = frames.last_mut() {
                        f.arg += 1;
                    }
                }
                TokenKind::Semi => marked = false,
                TokenKind::Literal { kind, suffix_start } => {
                    let starts_arg = p.checked_sub(1).is_some_and(|q| matches!(self.sym(q), "(" | "[" | "{" | ","));
                    let mut api = None;
                    if marked {
                        api = Some("language=SQL".to_string());
                    } else if starts_arg && let Some(f) = frames.last() {
                        api = match f.callee {
                            Callee::Macro if !f.found && f.arg <= 1 => Some(f.api.clone()),
                            Callee::Call if f.arg == 0 => Some(f.api.clone()),
                            _ => None,
                        };
                    }
                    if let Some(api) = api
                        && let Some(inj) = self.literal(t, kind, suffix_start, api)
                    {
                        let call = frames.last().is_some_and(|f| f.callee == Callee::Call);
                        if marked || !call || looks_like_sql(&inj.sql) {
                            if !marked && let Some(f) = frames.last_mut() {
                                f.found = true;
                            }
                            out.push(inj);
                        }
                    }
                    if is_str(kind) {
                        marked = false;
                        if let Some(f) = frames.last_mut()
                            && f.callee == Callee::Macro
                        {
                            f.found = true;
                        }
                    }
                }
                _ => {}
            }
            p += 1;
        }
        out
    }

    /// The injection for a string literal token (`None` for other literals and unterminated
    /// strings).
    fn literal(&self, t: &Tok, kind: LiteralKind, suffix_start: u32, api: String) -> Option<SqlInjection> {
        let (raw, hashes) = match kind {
            LiteralKind::Str { terminated: true } => (false, 0),
            LiteralKind::RawStr { n_hashes: Some(n) } => (true, usize::from(n)),
            _ => return None,
        };
        let body_bytes = t.start.checked_add(suffix_start as usize)?.min(t.end);
        let lit = self.text.get(t.start..body_bytes)?;
        let lit_chars = lit.chars().count();
        let (open, close) = if raw { (2 + hashes, 1 + hashes) } else { (1, 1) };
        if lit_chars < open + close {
            return None;
        }
        let start = t.cstart + open;
        let end = t.cstart + lit_chars - close;
        let content: Vec<char> = lit.chars().skip(open).take(lit_chars - open - close).collect();
        let (sql, map) = if raw { plain(&content, start) } else { unescape(&content, start) };
        Some(SqlInjection {
            start,
            end,
            literal_start: t.cstart,
            literal_end: t.cstart + lit_chars,
            raw,
            hashes,
            api,
            sql,
            map,
        })
    }
}

fn is_str(kind: LiteralKind) -> bool {
    matches!(kind, LiteralKind::Str { .. } | LiteralKind::RawStr { .. })
}

/// `// language=SQL`, `/* language=sql */` (more words may follow).
fn is_language_sql_comment(text: &str) -> bool {
    let body = text.trim_start_matches('/').trim_start_matches(['*', '!']).trim_end_matches('/').trim_end_matches('*');
    let body = body.trim();
    let Some((key, value)) = body.split_once('=') else { return false };
    if !key.trim().eq_ignore_ascii_case("language") {
        return false;
    }
    let value = value.trim_start();
    let word: String = value.chars().take_while(|c| !c.is_whitespace()).collect();
    word.eq_ignore_ascii_case("sql")
        || matches!(word.to_ascii_lowercase().as_str(), "postgresql" | "sqlite" | "mysql" | "pgsql")
}

fn plain(content: &[char], start: usize) -> (String, Vec<(usize, usize)>) {
    let sql: String = content.iter().collect();
    let map = (0..content.len()).map(|k| (start + k, start + k + 1)).collect();
    (sql, map)
}

/// Decode the escapes of a normal string's content starting at Rust offset `start`.
fn unescape(content: &[char], start: usize) -> (String, Vec<(usize, usize)>) {
    let mut sql = String::with_capacity(content.len());
    let mut map = Vec::with_capacity(content.len());
    let at = |k: usize| content.get(k).copied();
    let mut k = 0usize;
    while k < content.len() {
        let c = at(k).unwrap_or('\0');
        if c != '\\' {
            sql.push(c);
            map.push((start + k, start + k + 1));
            k += 1;
            continue;
        }
        let (decoded, len) = match at(k + 1) {
            Some('n') => (Some('\n'), 2),
            Some('t') => (Some('\t'), 2),
            Some('r') => (Some('\r'), 2),
            Some('0') => (Some('\0'), 2),
            Some('\\') => (Some('\\'), 2),
            Some('\'') => (Some('\''), 2),
            Some('"') => (Some('"'), 2),
            Some('x') => {
                let hex: String = content.get(k + 2..k + 4).unwrap_or(&[]).iter().collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(b) if hex.len() == 2 && b.is_ascii() => (Some(char::from(b)), 4),
                    _ => (Some('\\'), 1),
                }
            }
            Some('u') if at(k + 2) == Some('{') => {
                let close = (k + 3..content.len().min(k + 12)).find(|&j| at(j) == Some('}'));
                match close {
                    Some(j) => {
                        let hex: String = content.get(k + 3..j).unwrap_or(&[]).iter().filter(|c| **c != '_').collect();
                        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            Some(ch) => (Some(ch), j + 1 - k),
                            None => (Some('\\'), 1),
                        }
                    }
                    None => (Some('\\'), 1),
                }
            }
            // Line continuation: the newline and the next line's leading whitespace vanish.
            Some('\n') | Some('\r') => {
                let mut j = k + 1;
                while at(j).is_some_and(char::is_whitespace) {
                    j += 1;
                }
                (None, j - k)
            }
            _ => (Some('\\'), 1),
        };
        if let Some(ch) = decoded {
            sql.push(ch);
            map.push((start + k, start + k + len));
        }
        k += len.max(1);
    }
    (sql, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(text: &str) -> Vec<String> {
        find_injections(text).into_iter().map(|i| i.sql).collect()
    }

    fn apis(text: &str) -> Vec<String> {
        find_injections(text).into_iter().map(|i| i.api).collect()
    }

    /// The Rust text of each injection's content range.
    fn ranges(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        find_injections(text).into_iter().map(|i| chars[i.start..i.end].iter().collect()).collect()
    }

    #[test]
    fn sqlx_macros() {
        let text = r#####"
async fn f(pool: &PgPool, id: i64) {
    let a = sqlx::query!("SELECT id FROM users WHERE id = $1", id).fetch_one(pool).await;
    let b = sqlx::query_as!(User, "SELECT * FROM users").fetch_all(pool).await;
    let c = query_scalar!(r#"SELECT count(*) AS "n!" FROM users"#).fetch_one(pool).await;
    let d = sqlx::query_unchecked!("DELETE FROM users");
    let e = sqlx::query_file!("queries/users.sql");
    let g = sqlx::query_file_as!(User, "queries/users.sql");
    let h = sqlx::query_as_unchecked!(User, r##"SELECT "#" FROM t"##);
}
"#####;
        assert_eq!(
            sqls(text),
            [
                "SELECT id FROM users WHERE id = $1",
                "SELECT * FROM users",
                "SELECT count(*) AS \"n!\" FROM users",
                "DELETE FROM users",
                "SELECT \"#\" FROM t",
            ]
        );
        assert_eq!(apis(text), ["query!", "query_as!", "query_scalar!", "query_unchecked!", "query_as_unchecked!"]);
        // The range is the content only.
        assert_eq!(ranges(text)[2], "SELECT count(*) AS \"n!\" FROM users");
        let inj = &find_injections(text)[4];
        assert!(inj.raw && inj.hashes == 2);
    }

    #[test]
    fn functions_and_methods() {
        let text = r#"
fn f() {
    sqlx::query("UPDATE t SET a = 1").execute(&pool);
    sqlx::query_as::<_, User>("SELECT * FROM users").fetch_all(&pool);
    sqlx::query_scalar::<_, i64>("select count(*) from t");
    diesel::sql_query("SELECT * FROM posts").load(conn);
    conn.execute("INSERT INTO t (a) VALUES (?1)", params![1])?;
    conn.prepare("SELECT a FROM t")?;
    conn.prepare_cached("SELECT b FROM t")?;
    conn.query_row("SELECT 1", [], |r| r.get(0))?;
    conn.execute_batch("BEGIN; CREATE TABLE x (a); COMMIT;")?;
    client.query("SELECT * FROM t WHERE id = $1", &[&id]).await?;
    client.query_one("SELECT 1", &[]).await?;
    client.query_opt("SELECT 2", &[]).await?;
    client.batch_execute("DROP TABLE x").await?;
    // Not SQL: arguments that are not literals, other text, other methods.
    q.execute(&pool).await?;
    map.query("name");
    log("SELECT * FROM t");
    other.run("SELECT * FROM t");
    fn query(sql: &str) {}
}
"#;
        assert_eq!(
            sqls(text),
            [
                "UPDATE t SET a = 1",
                "SELECT * FROM users",
                "select count(*) from t",
                "SELECT * FROM posts",
                "INSERT INTO t (a) VALUES (?1)",
                "SELECT a FROM t",
                "SELECT b FROM t",
                "SELECT 1",
                "BEGIN; CREATE TABLE x (a); COMMIT;",
                "SELECT * FROM t WHERE id = $1",
                "SELECT 1",
                "SELECT 2",
                "DROP TABLE x",
            ]
        );
        assert_eq!(apis(text)[0], "query");
        assert_eq!(apis(text)[4], ".execute");
    }

    #[test]
    fn language_comments() {
        let text = r#"
// language=SQL
const Q: &str = "select * from users";
/* language=sql */ let x = r"delete from t";
// language = PostgreSQL
let y = "anything at all";
// language=SQL
let n = 5; let z = "not sql";
// language=JSON
let j = "{}";
"#;
        assert_eq!(sqls(text), ["select * from users", "delete from t", "anything at all"]);
        assert!(apis(text).iter().all(|a| a == "language=SQL"));
    }

    #[test]
    fn escapes_map_back() {
        let text = "conn.execute(\"SELECT \\\"a\\\" FROM t\\nWHERE x = '\\u{e9}' \\\n        AND y\", [])";
        let inj = &find_injections(text)[0];
        assert_eq!(inj.sql, "SELECT \"a\" FROM t\nWHERE x = 'é' AND y");
        let chars: Vec<char> = text.chars().collect();
        // `"a"` maps to `\"a\"`.
        let (s, e) = inj.to_rust_range(7, 10);
        assert_eq!(chars[s..e].iter().collect::<String>(), "\\\"a\\\"");
        // The line continuation is skipped: `AND` maps to itself.
        let and = inj.sql.get(..inj.sql.find("AND").unwrap()).unwrap().chars().count();
        let (s, e) = inj.to_rust_range(and, and + 3);
        assert_eq!(chars[s..e].iter().collect::<String>(), "AND");
        // Back from Rust to SQL.
        assert_eq!(inj.to_sql(s), Some(and));
        assert_eq!(inj.to_sql(inj.end), Some(inj.sql.chars().count()));
        assert_eq!(inj.to_sql(inj.start.wrapping_sub(1)), None);
        assert_eq!(inj.to_rust(usize::MAX), inj.end);
    }

    #[test]
    fn multiline_raw_and_nested_calls() {
        let text = r##"
let rows = sqlx::query!(
    r#"
    SELECT u.id, u.name
    FROM users u
    WHERE u.id = $1
    "#,
    format!("{}", id),
)
.fetch_all(&pool)
.await?;
outer(conn.execute("DELETE FROM t", [inner("SELECT not_this")]));
"##;
        let found = find_injections(text);
        assert_eq!(found.len(), 2);
        assert!(found[0].sql.contains("FROM users u\n"));
        assert_eq!(found[1].sql, "DELETE FROM t");
        assert_eq!(injection_at(&found, found[0].start + 3).map(|i| i.api.as_str()), Some("query!"));
        assert!(injection_at(&found, 0).is_none());
    }

    #[test]
    fn highlights_layer_over_the_literal() {
        let text = "let q = sqlx::query!(\"SELECT a\\n FROM t\");\n";
        let found = find_injections(text);
        let hl = injected_highlights(text, &found);
        let col = text.find("SELECT").unwrap();
        assert_eq!(hl.line(0)[0], LineSpan { start: col, end: col + 6, kind: HighlightKind::Keyword });
        let from = text.find("FROM").unwrap();
        assert!(hl.line(0).iter().any(|s| s.start == from && s.kind == HighlightKind::Keyword));
        assert!(hl.line(1).is_empty());
        // Multi-line raw strings: spans land on their own lines.
        let text = "sqlx::query!(r\"\n  select 1\n  from t\")";
        let hl = injected_highlights(text, &find_injections(text));
        assert_eq!(hl.line(1)[0], LineSpan { start: 2, end: 8, kind: HighlightKind::Keyword });
        assert_eq!(hl.line(2)[0], LineSpan { start: 2, end: 6, kind: HighlightKind::Keyword });
    }

    #[test]
    fn encoding_for_literals() {
        assert_eq!(encode_for_literal("a \"b\" \\ c\n", false, 0).as_deref(), Some("a \\\"b\\\" \\\\ c\n"));
        assert_eq!(encode_for_literal("a \"b\"", true, 1).as_deref(), Some("a \"b\""));
        assert_eq!(encode_for_literal("a \"# b", true, 1), None);
        assert_eq!(encode_for_literal("a \"# b", true, 2).as_deref(), Some("a \"# b"));
    }

    #[test]
    fn hostile_input_never_panics() {
        for text in [
            "",
            "query!(",
            "query!(\"",
            "query!(r#\"never closed",
            "conn.execute(\"\\",
            "conn.execute(\"\\u{\")",
            "conn.execute(\"\\u{ffffffff}\")",
            "conn.execute(\"\\x\")",
            "x.query::<>(\"SELECT\")",
            ">(\"SELECT 1\")",
            "::<a>(\"SELECT 1\")",
            ")))]]]}}}",
            "// language=SQL",
            "// language=SQL\n\"",
            "query!(\"é\\u{1F600}\")",
            "\u{feff}fn a() { query!(\"SELECT 1\") }",
        ] {
            let found = find_injections(text);
            let _ = injected_highlights(text, &found);
            for i in &found {
                let _ = (i.to_rust(0), i.to_rust_range(0, usize::MAX), i.to_sql(i.start));
            }
        }
        assert_eq!(sqls("x.query::<>(\"SELECT 1\")"), ["SELECT 1"]);
        assert_eq!(sqls("query!(\"é\\u{1F600}\")"), ["é😀"]);
    }

    #[test]
    fn looks_like_sql_skips_comments_and_parens() {
        assert!(looks_like_sql("  -- note\n/* x */ (SELECT 1)"));
        assert!(looks_like_sql("with x as (select 1) select * from x"));
        assert!(!looks_like_sql("hello world"));
        assert!(!looks_like_sql(""));
    }
}
