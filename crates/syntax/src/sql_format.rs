//! SQL formatter: one clause per line (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, `ORDER BY`,
//! `HAVING`, `LIMIT`, joins, `UNION`, `VALUES`, `SET`, `RETURNING`, …), `AND`/`OR` of
//! `WHERE`/`HAVING` on their own lines, subqueries indented in their parentheses, `CREATE TABLE`
//! column lists one per line, keywords in upper or lower case, comments kept.
//!
//! The layout depends only on the tokens (plus where comments sat and blank lines between
//! statements), so formatting is idempotent. Dollar-quoted function bodies are kept verbatim.
//! Works on any text and never panics.

use serde::{Deserialize, Serialize};

use crate::HighlightKind as K;
use crate::common::Doc;
use crate::lex::{self, Tt};
use crate::sql;

/// How keywords are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
    /// As typed.
    Preserve,
}

impl KeywordCase {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "upper" | "uppercase" | "upper case" => Some(Self::Upper),
            "lower" | "lowercase" | "lower case" => Some(Self::Lower),
            "preserve" | "as is" | "asis" | "keep" => Some(Self::Preserve),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SqlFormatOptions {
    pub keyword_case: KeywordCase,
    /// Spaces per indentation level.
    pub indent: usize,
}

impl Default for SqlFormatOptions {
    fn default() -> Self {
        Self { keyword_case: KeywordCase::Upper, indent: 4 }
    }
}

/// Multi-char operators, longest first.
const OPERATORS: &[&str] = &[
    "->>", "#>>", "!~*", "<=>", "->", "#>", "@>", "<@", "<>", "<=", ">=", "!=", "||", "::", "&&", "<<", ">>", "==",
    "~~", "!~", "~*", "?|", "?&", "|/", "@@",
];

fn is_op_char(c: char) -> bool {
    matches!(c, '+' | '-' | '*' | '/' | '<' | '>' | '=' | '!' | '|' | '&' | '%' | '^' | '~' | '?' | '#' | '@' | ':')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ut {
    Word,
    /// Bracket or punctuation (`(`, `)`, `[`, `]`, `,`, `;`, `.`, `{`, `}`).
    Sym(char),
    Op,
    Lit,
    Other,
    LineComment,
    BlockComment,
    /// A dollar-quoted body, copied as is.
    Verbatim,
}

#[derive(Clone, Debug)]
struct Unit {
    ut: Ut,
    kind: K,
    text: String,
    /// Lower-cased ASCII word (words only).
    word: String,
    /// A newline separates it from the previous unit in the source.
    nl_before: bool,
    /// A blank line separates it from the previous unit in the source.
    blank_before: bool,
    /// Touches the previous unit in the source.
    adjacent: bool,
}

impl Unit {
    fn is_comment(&self) -> bool {
        matches!(self.ut, Ut::LineComment | Ut::BlockComment)
    }

    fn is_sym(&self, c: char) -> bool {
        self.ut == Ut::Sym(c)
    }
}

fn units(text: &str) -> Vec<Unit> {
    let doc = Doc::new(text);
    let ascii = lex::ascii(&doc, true);
    let (toks, bodies) = sql::tokens(&doc);
    let mut out: Vec<Unit> = Vec::with_capacity(toks.len());
    let mut prev_end = 0usize;
    let mut i = 0usize;
    let push = |out: &mut Vec<Unit>, prev_end: &mut usize, start: usize, end: usize, ut: Ut, kind: K, text: String| {
        let gap = doc.slice(*prev_end, start);
        let newlines = gap.iter().filter(|c| **c == '\n').count();
        let word = if ut == Ut::Word {
            String::from_utf8_lossy(ascii.get(start..end).unwrap_or(&[])).into_owned()
        } else {
            String::new()
        };
        out.push(Unit {
            ut,
            kind,
            text,
            word,
            nl_before: newlines > 0,
            blank_before: newlines > 1,
            adjacent: gap.is_empty() && !out.is_empty(),
        });
        *prev_end = end;
    };
    while let Some(t) = toks.get(i) {
        // A function body: from its opening tag to the end of its closing tag.
        if let Some(&(_, close)) = bodies.iter().find(|(open, _)| *open == t.start) {
            let close_end = toks.iter().skip(i).find(|x| x.start == close).map_or(doc.len(), |x| x.end);
            push(&mut out, &mut prev_end, t.start, close_end, Ut::Verbatim, K::String, doc.string(t.start, close_end));
            while toks.get(i).is_some_and(|x| x.start < close_end) {
                i += 1;
            }
            continue;
        }
        let tt = t.tt;
        match tt {
            Tt::Sym(c) if is_op_char(c) => {
                // A run of touching operator chars, split into known operators.
                let mut run: Vec<char> = vec![c];
                let mut end = t.end;
                let mut j = i + 1;
                while let Some(n) = toks.get(j) {
                    match n.tt {
                        Tt::Sym(d) if is_op_char(d) && n.start == end => {
                            run.push(d);
                            end = n.end;
                            j += 1;
                        }
                        _ => break,
                    }
                }
                let mut k = 0usize;
                let mut at = t.start;
                while k < run.len() {
                    let rest: String = run.get(k..).unwrap_or(&[]).iter().collect();
                    let len = OPERATORS.iter().find(|op| rest.starts_with(*op)).map_or(1, |op| op.chars().count());
                    let piece: String = run.get(k..k + len).unwrap_or(&[]).iter().collect();
                    push(&mut out, &mut prev_end, at, at + len, Ut::Op, K::Operator, piece);
                    at += len;
                    k += len;
                }
                i = j;
                continue;
            }
            _ => {}
        }
        let ut = match tt {
            Tt::Word => Ut::Word,
            Tt::Sym(c) => Ut::Sym(c),
            Tt::Lit => Ut::Lit,
            Tt::LineComment => Ut::LineComment,
            Tt::BlockComment => Ut::BlockComment,
            Tt::Other => Ut::Other,
        };
        let mut s = doc.string(t.start, t.end);
        if ut == Ut::LineComment {
            s = s.trim_end().to_string();
        }
        push(&mut out, &mut prev_end, t.start, t.end, ut, t.kind, s);
        i += 1;
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameKind {
    Top,
    Subquery,
    /// `CREATE TABLE t (…)`: one element per line.
    List,
    Inline,
}

#[derive(Clone, Debug)]
struct Frame {
    kind: FrameKind,
    /// Indentation level of the frame's clauses (or list elements).
    base: usize,
    /// Level of the line the closing parenthesis goes on.
    close: usize,
    /// The clause being written (`where`, `having`, …).
    clause: String,
    /// Inside `BETWEEN … AND`.
    between: bool,
}

impl Frame {
    fn new(kind: FrameKind, base: usize, close: usize) -> Self {
        Self { kind, base, close, clause: String::new(), between: false }
    }

    fn breaks(&self) -> bool {
        matches!(self.kind, FrameKind::Top | FrameKind::Subquery)
    }
}

struct Out {
    s: String,
    indent: usize,
    /// Indentation level of the current line.
    level: usize,
    at_line_start: bool,
}

impl Out {
    fn newline(&mut self, level: usize, blank: bool) {
        if self.s.is_empty() {
            self.level = level;
            return;
        }
        let trimmed = self.s.trim_end_matches([' ', '\t']).len();
        self.s.truncate(trimmed);
        if !self.at_line_start || !self.s.ends_with('\n') {
            self.s.push('\n');
        }
        if blank && !self.s.ends_with("\n\n") {
            self.s.push('\n');
        }
        self.s.extend(std::iter::repeat_n(' ', level.saturating_mul(self.indent).min(1_000)));
        self.level = level;
        self.at_line_start = true;
    }

    fn put(&mut self, text: &str, space: bool) {
        if space && !self.at_line_start && !self.s.is_empty() {
            self.s.push(' ');
        }
        self.s.push_str(text);
        self.at_line_start = false;
    }
}

/// Format `text` (one or more statements).
pub fn format_sql(text: &str, opts: &SqlFormatOptions) -> String {
    let units = units(text);
    let sig: Vec<usize> = (0..units.len()).filter(|&i| units.get(i).is_some_and(|u| !u.is_comment())).collect();
    // Position of each unit among the significant ones (comments: the next significant one).
    let mut f = Formatter {
        units: &units,
        sig: &sig,
        opts,
        out: Out { s: String::new(), indent: opts.indent, level: 0, at_line_start: true },
    };
    f.run();
    let mut s = f.out.s.trim_end().to_string();
    if text.trim_end_matches([' ', '\t']).ends_with('\n') && !s.is_empty() {
        s.push('\n');
    }
    s
}

struct Formatter<'a> {
    units: &'a [Unit],
    sig: &'a [usize],
    opts: &'a SqlFormatOptions,
    out: Out,
}

impl<'a> Formatter<'a> {
    fn unit(&self, i: usize) -> Option<&'a Unit> {
        self.units.get(i)
    }

    /// The significant unit `k` positions away from significant position `s`.
    fn sig_unit(&self, s: usize, k: isize) -> Option<&'a Unit> {
        let p = s.checked_add_signed(k)?;
        let units: &'a [Unit] = self.units;
        self.sig.get(p).and_then(|&i| units.get(i))
    }

    fn sig_word(&self, s: usize, k: isize) -> &'a str {
        self.sig_unit(s, k).map_or("", |u| u.word.as_str())
    }

    fn text_of(&self, u: &Unit) -> String {
        let keyword = u.ut == Ut::Word && matches!(u.kind, K::Keyword | K::Type | K::Boolean);
        match self.opts.keyword_case {
            KeywordCase::Upper if keyword => u.text.to_uppercase(),
            KeywordCase::Lower if keyword => u.text.to_lowercase(),
            _ => u.text.clone(),
        }
    }

    fn run(&mut self) {
        let mut frames: Vec<Frame> = vec![Frame::new(FrameKind::Top, 0, 0)];
        // Significant position of the next significant unit.
        let mut s = 0usize;
        let mut stmt_start = true;
        let mut stmt_verb = String::new();
        let mut stmt_table = false;
        let mut stmt_parens = 0usize;
        // A newline is owed before the next unit (after a line comment or a list comma):
        // `Some(level)` for a fixed level, else the continuation level.
        let mut force: Option<usize> = None;
        let mut pending_nl = false;
        let mut pending_blank = false;
        let mut after_semicolon = false;
        for i in 0..self.units.len() {
            let Some(u) = self.unit(i) else { break };
            let top = frames.last().cloned().unwrap_or_else(|| Frame::new(FrameKind::Top, 0, 0));
            if u.is_comment() {
                if after_semicolon && u.blank_before {
                    pending_blank = true;
                }
                let own_line = u.nl_before || self.out.s.is_empty();
                if own_line {
                    let next_word = self.sig_word(s, 0);
                    let level =
                        if stmt_start || is_clause_start(next_word) { top.base } else { force.unwrap_or(top.base + 1) };
                    self.out.newline(level, pending_blank);
                    pending_blank = false;
                    self.out.put(&u.text, false);
                } else {
                    self.out.put(&u.text, true);
                }
                if u.ut == Ut::LineComment || u.text.contains('\n') {
                    pending_nl = true;
                }
                continue;
            }
            if after_semicolon && u.blank_before {
                pending_blank = true;
            }
            after_semicolon = false;
            let prev = self.sig_unit(s, -1).filter(|_| !stmt_start);
            let prev_word = if stmt_start { "" } else { self.sig_word(s, -1) };
            let prev2_word = if stmt_start { "" } else { self.sig_word(s, -2) };
            let next_word = self.sig_word(s, 1);
            let w = u.word.as_str();
            // Where this unit goes: a new line at a level, or the current line.
            let mut want: Option<usize> = None;
            if stmt_start {
                want = Some(0);
            } else if top.breaks() && u.ut == Ut::Word && u.kind == K::Keyword && !prev.is_some_and(|p| p.is_sym('.')) {
                if clause_break(w, prev_word, prev2_word, next_word, prev, &stmt_verb) {
                    want = Some(top.base);
                } else if matches!(w, "and" | "or")
                    && matches!(top.clause.as_str(), "where" | "having")
                    && !(w == "and" && top.between)
                {
                    want = Some(top.base + 1);
                }
            }
            if u.is_sym(')') && matches!(top.kind, FrameKind::Subquery | FrameKind::List) && frames.len() > 1 {
                want = Some(top.close);
            }
            if want.is_none()
                && let Some(level) = force
            {
                want = Some(level);
            }
            if want.is_none() && pending_nl {
                want = Some(top.base + 1);
            }
            match want {
                Some(level) => {
                    let text = self.text_of(u);
                    self.out.newline(level, pending_blank && stmt_start);
                    self.out.put(&text, false);
                }
                None => {
                    let space = self.space(prev, u, s);
                    let text = self.text_of(u);
                    self.out.put(&text, space);
                }
            }
            force = None;
            pending_nl = false;
            pending_blank = false;
            if stmt_start {
                stmt_verb = w.to_string();
                stmt_table = false;
                stmt_parens = 0;
            }
            stmt_start = false;
            // An unterminated quoted name stops at the line end: keep that line end, or the
            // next format would read further.
            if u.kind == K::Invalid && self.unit(i + 1).is_some_and(|n| n.nl_before) {
                pending_nl = true;
            }
            // Clause and frame bookkeeping.
            if let Some(f) = frames.last_mut() {
                match w {
                    "select" | "from" | "where" | "having" | "set" | "values" | "join" | "on" | "limit"
                    | "returning" | "group" | "order" | "union" | "intersect" | "except" => {
                        if u.ut == Ut::Word && f.breaks() {
                            f.clause = w.to_string();
                            f.between = false;
                        }
                    }
                    "between" => f.between = true,
                    "and" if f.between => f.between = false,
                    _ => {}
                }
            }
            if w == "table" && frames.len() == 1 {
                stmt_table = true;
            }
            match u.ut {
                Ut::Sym('(') => {
                    let line = self.out.level;
                    let subquery = matches!(next_word, "select" | "with");
                    let list = !subquery
                        && frames.len() == 1
                        && stmt_verb == "create"
                        && stmt_table
                        && stmt_parens == 0
                        && !self.sig_unit(s, 1).is_some_and(|n| n.is_sym(')'));
                    if frames.len() == 1 {
                        stmt_parens += 1;
                    }
                    let frame = if subquery {
                        Frame::new(FrameKind::Subquery, line + 1, line)
                    } else if list {
                        force = Some(line + 1);
                        Frame::new(FrameKind::List, line + 1, line)
                    } else {
                        Frame::new(FrameKind::Inline, top.base, line)
                    };
                    frames.push(frame);
                }
                Ut::Sym(')') => {
                    if frames.len() > 1 {
                        frames.pop();
                    }
                }
                Ut::Sym(',') if top.kind == FrameKind::List => force = Some(top.base),
                Ut::Sym(';') => {
                    frames.truncate(1);
                    if let Some(f) = frames.last_mut() {
                        *f = Frame::new(FrameKind::Top, 0, 0);
                    }
                    stmt_start = true;
                    after_semicolon = true;
                }
                _ => {}
            }
            s += 1;
        }
    }

    /// Whether a space goes between `prev` and `u` on the same line. A space the source had is
    /// only dropped when the two tokens still lex the same without it.
    fn space(&self, prev: Option<&Unit>, u: &Unit, s: usize) -> bool {
        let Some(p) = prev else { return true };
        let wanted = self.wants_space(p, u, s);
        wanted || !u.adjacent && !joins_safely(&p.text, &u.text)
    }

    fn wants_space(&self, p: &Unit, u: &Unit, s: usize) -> bool {
        if matches!(u.ut, Ut::Sym(',' | ';' | ')' | ']' | '.' | '[')) {
            return false;
        }
        if matches!(p.ut, Ut::Sym('(' | '[' | '.')) {
            return false;
        }
        if u.ut == Ut::Op && u.text == "::" || p.ut == Ut::Op && p.text == "::" {
            return false;
        }
        if u.is_sym('(') {
            return match p.ut {
                Ut::Word => match p.kind {
                    K::Function | K::Type | K::Field => false,
                    K::Keyword | K::Boolean => !u.adjacent,
                    _ => true,
                },
                Ut::Op => !self.unary(s.saturating_sub(1)),
                Ut::Other => !u.adjacent,
                _ => true,
            };
        }
        if p.ut == Ut::Op && self.unary(s.saturating_sub(1)) {
            return false;
        }
        true
    }

    /// The significant unit at `s` is a unary `-`/`+`.
    fn unary(&self, s: usize) -> bool {
        let Some(u) = self.sig_unit(s, 0) else { return false };
        if !(u.ut == Ut::Op && matches!(u.text.as_str(), "-" | "+")) {
            return false;
        }
        match s.checked_sub(1).and_then(|p| self.sig.get(p)).and_then(|&i| self.units.get(i)) {
            None => true,
            Some(p) => {
                matches!(p.ut, Ut::Op | Ut::Sym('(' | ',' | '[' | ';')) || p.ut == Ut::Word && p.kind == K::Keyword
            }
        }
    }
}

/// Whether `a` and `b` written without a space between still lex as the same tokens.
fn joins_safely(a: &str, b: &str) -> bool {
    let (last, first) = (a.chars().last(), b.chars().next());
    // Touching operator chars merge; `..` changes how the next word is read.
    if last.is_some_and(is_op_char) && first.is_some_and(is_op_char) || last == Some('.') && first == Some('.') {
        return false;
    }
    let count = |t: &str| sql::tokens(&Doc::new(t)).0.len();
    let joined = format!("{a}{b}");
    let split = a.chars().count();
    let (toks, _) = sql::tokens(&Doc::new(&joined));
    toks.iter().any(|t| t.start == split) && toks.len() == count(a) + count(b)
}

/// Words that start a clause line (for placing comments that precede them).
fn is_clause_start(w: &str) -> bool {
    matches!(
        w,
        "select"
            | "from"
            | "where"
            | "group"
            | "order"
            | "having"
            | "limit"
            | "union"
            | "intersect"
            | "except"
            | "values"
            | "set"
            | "returning"
            | "join"
            | "left"
            | "right"
            | "inner"
            | "full"
            | "cross"
            | "natural"
            | "window"
            | "insert"
            | "update"
            | "delete"
    )
}

/// Whether the word `w` starts a new clause line.
fn clause_break(w: &str, prev: &str, prev2: &str, next: &str, prev_unit: Option<&Unit>, verb: &str) -> bool {
    match w {
        "select" => true,
        "from" => prev != "delete" && !(prev == "distinct" && prev2 == "is"),
        "where" | "having" | "returning" | "window" | "union" | "intersect" | "except" | "limit" => true,
        "values" => prev != "default",
        "group" | "order" => next == "by",
        "join" => !matches!(prev, "left" | "right" | "full" | "inner" | "cross" | "natural" | "outer"),
        "left" | "right" | "full" | "inner" | "cross" | "natural" => {
            matches!(next, "join" | "outer" | "left" | "right" | "full" | "inner") && prev != "natural"
        }
        "on" => next == "conflict",
        "set" => verb == "update" || prev == "update",
        "insert" | "update" | "delete" | "merge" => prev_unit.is_some_and(|p| p.is_sym(')')),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sql: &str) -> String {
        format_sql(sql, &SqlFormatOptions::default())
    }

    #[track_caller]
    fn golden(input: &str, expected: &str) {
        let out = fmt(input);
        assert_eq!(out, expected, "\n--- got ---\n{out}\n--- want ---\n{expected}");
        assert_eq!(fmt(&out), out, "not idempotent");
    }

    #[test]
    fn select_clauses_per_line() {
        golden(
            "select u.id, u.name, count(*) as n from users u left join orders o on o.user_id=u.id where u.active=true and o.total>=10 group by u.id, u.name having count(*)>1 order by n desc limit 10",
            "SELECT u.id, u.name, count(*) AS n\nFROM users u\nLEFT JOIN orders o ON o.user_id = u.id\nWHERE u.active = TRUE\n    AND o.total >= 10\nGROUP BY u.id, u.name\nHAVING count(*) > 1\nORDER BY n DESC\nLIMIT 10",
        );
    }

    #[test]
    fn subqueries_are_indented() {
        golden(
            "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 5 AND paid) AND x BETWEEN 1 AND 2",
            "SELECT *\nFROM users\nWHERE id IN (\n    SELECT user_id\n    FROM orders\n    WHERE total > 5\n        AND paid\n)\n    AND x BETWEEN 1 AND 2",
        );
        golden(
            "with recent as (select * from orders where at > now() - interval '1 day') select count(*) from recent",
            "WITH recent AS (\n    SELECT *\n    FROM orders\n    WHERE at > now() - INTERVAL '1 day'\n)\nSELECT count(*)\nFROM recent",
        );
    }

    #[test]
    fn dml_statements() {
        golden(
            "insert into users(id,name) values ($1,$2),(3,'x') on conflict (id) do update set name=excluded.name returning id;\nupdate users set name = ?, active = false where id = ?;\ndelete from users where id = -1",
            "INSERT INTO users (id, name)\nVALUES ($1, $2), (3, 'x')\nON CONFLICT (id) DO UPDATE\nSET name = excluded.name\nRETURNING id;\nUPDATE users\nSET name = ?, active = FALSE\nWHERE id = ?;\nDELETE FROM users\nWHERE id = -1",
        );
    }

    #[test]
    fn create_table_lists_and_lowercase() {
        let opts = SqlFormatOptions { keyword_case: KeywordCase::Lower, indent: 2 };
        let out = format_sql(
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name VARCHAR(80) NOT NULL, CHECK (length(name) > 0))",
            &opts,
        );
        assert_eq!(
            out,
            "create table if not exists users (\n  id integer primary key,\n  name varchar(80) not null,\n  check (length(name) > 0)\n)"
        );
        assert_eq!(format_sql(&out, &opts), out);
        // Preserve keeps the case as typed.
        let opts = SqlFormatOptions { keyword_case: KeywordCase::Preserve, indent: 4 };
        assert_eq!(format_sql("Select a From t", &opts), "Select a\nFrom t");
    }

    #[test]
    fn comments_are_kept() {
        golden(
            "-- header\nSELECT a, -- first\n  b /* inline */ FROM t\n-- before where\nWHERE x = 1; -- trailing\n\n\n-- next\nSELECT 2;\n",
            "-- header\nSELECT a, -- first\n    b /* inline */\nFROM t\n-- before where\nWHERE x = 1; -- trailing\n\n-- next\nSELECT 2;\n",
        );
    }

    #[test]
    fn operators_and_casts() {
        golden(
            "select a::text, b->>'k', c||d, e<>f, -g, h*-1, t.*, arr[1], x not in (1, 2) from t",
            "SELECT a::TEXT, b ->> 'k', c || d, e <> f, -g, h * -1, t.*, arr[1], x NOT IN (1, 2)\nFROM t",
        );
    }

    #[test]
    fn function_bodies_are_verbatim() {
        let sql = "create function f() returns int as $$\nbegin\n  return 1;\nend;\n$$ language plpgsql;\nselect 1";
        golden(sql, "CREATE FUNCTION f() RETURNS INT AS $$\nbegin\n  return 1;\nend;\n$$ LANGUAGE plpgsql;\nSELECT 1");
    }

    #[test]
    fn joins_and_unions() {
        golden(
            "select 1 from a natural left join b cross join c full outer join d using (id) union all select 2 from e is distinct from f",
            "SELECT 1\nFROM a\nNATURAL LEFT JOIN b\nCROSS JOIN c\nFULL OUTER JOIN d USING (id)\nUNION ALL\nSELECT 2\nFROM e IS DISTINCT FROM f",
        );
        golden("SELECT extract(epoch FROM at) FROM t", "SELECT extract(epoch FROM at)\nFROM t");
        golden(
            "select row_number() over (partition by a order by b) from t",
            "SELECT row_number() OVER (PARTITION BY a ORDER BY b)\nFROM t",
        );
    }

    #[test]
    fn hostile_input_never_panics_and_is_idempotent() {
        for sql in [
            "",
            "   ",
            "(",
            ")))",
            "((((select",
            "select (",
            "select )))) from",
            "'unterminated",
            "/* unterminated",
            "-- only a comment",
            "$$ never closed",
            "create function f() as $$ select 1",
            "create table t (",
            "create table t ()",
            ";;;",
            "select\u{0}é from ü",
            "select ----- from",
            "-",
            "+-+-+-",
            "select a from t where a between",
        ] {
            let out = fmt(sql);
            assert_eq!(fmt(&out), out, "not idempotent for {sql:?}");
        }
        let huge = "select a, b from t where (x = 1 and y in (select 1)) or z = 'q';\n".repeat(5_000);
        let out = fmt(&huge);
        assert!(out.lines().count() > 5_000);
        let deep = format!("select {}1{}", "(".repeat(10_000), ")".repeat(10_000));
        let _ = fmt(&deep);
    }
}
