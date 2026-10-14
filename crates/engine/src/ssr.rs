//! Structural search and replace (Edit › Find › Search Structurally… / Replace Structurally…):
//! a template language with `$var$` placeholders matched over the token stream of a file.
//!
//! Files are lexed with the same lexers the editor highlights with (`codecraft_syntax`): string,
//! char and number literals and lifetimes are single tokens, comments are ignored, whitespace is
//! insignificant and brackets must balance. A variable matches a run of whole tokens and bracket
//! groups: by default one "item" that does not cross a top-level `,` or `;`, a statement keyword
//! or an assignment (so `$e$.unwrap()` matches `foo.bar().unwrap()` but not `let x = y.unwrap()`
//! as a whole). Constraints per variable: a regex the matched text must (or must not) match, and
//! a count range of comma/semicolon-separated items (`0..∞` lets `println!($args$)` match any
//! argument list). A variable used twice must match the same tokens both times.
//!
//! Matching backtracks (longest candidate first) under a step budget, so hostile templates and
//! inputs end in a truncated result, never a hang. Everything here is pure; the commands and the
//! worker thread live in `ssr_cmds.rs`.

use std::collections::BTreeMap;

use regex::Regex;
use serde::{Deserialize, Serialize};

use codecraft_syntax::{HighlightKind, Language};

/// Longest template accepted (chars).
pub const MAX_TEMPLATE_CHARS: usize = 20_000;
/// Matching steps per start position before giving up on it.
const STEPS_PER_START: usize = 50_000;
/// Matching steps per file before the file's search is truncated.
pub const STEPS_PER_FILE: usize = 5_000_000;
/// Candidate extents considered per variable binding.
const MAX_CANDIDATES: usize = 4096;
/// Matches kept per file.
pub const MAX_MATCHES_PER_FILE: usize = 10_000;
/// Placeholder identifiers stand for variables while the template is lexed.
const VAR_PREFIX: &str = "__ssrv";
/// Matches of different lengths reported per start position (search only).
const MAX_SAME_START: usize = 8;
/// Count meaning "unbounded".
pub const UNBOUNDED: usize = usize::MAX;

// ---------------------------------------------------------------- tokens

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokKind {
    Ident,
    Keyword,
    Literal,
    Punct,
    Open,
    Close,
}

/// A token: its kind, text and char range in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub text: String,
    pub start: usize,
    pub end: usize,
    /// No whitespace between this token and the previous one.
    pub joined: bool,
}

impl Token {
    fn is_word(&self) -> bool {
        matches!(self.kind, TokKind::Ident | TokKind::Keyword)
    }

    fn punct(&self, s: &str) -> bool {
        self.kind == TokKind::Punct && self.text == s
    }
}

fn same(a: &Token, b: &Token) -> bool {
    (a.kind == b.kind || a.is_word() && b.is_word()) && a.text == b.text
}

const CLASS_NONE: u8 = 0;
const CLASS_LITERAL: u8 = 1;
const CLASS_COMMENT: u8 = 2;
const CLASS_KEYWORD: u8 = 3;

/// Lex `text` into tokens (comments dropped). Never panics; linear in the text size.
pub fn tokenize(language: Language, text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut class = vec![CLASS_NONE; chars.len()];
    let hl = codecraft_syntax::highlight(language, text);
    let mut line_start = 0usize;
    let mut line = 0usize;
    let mut i = 0usize;
    // Walk lines by `\n` exactly like the highlighter does.
    loop {
        let line_end = chars.get(line_start..).and_then(|rest| rest.iter().position(|c| *c == '\n')).map(|p| line_start + p);
        let end = line_end.unwrap_or(chars.len());
        for span in hl.line(line) {
            let c = match span.kind {
                HighlightKind::String | HighlightKind::Char | HighlightKind::Number | HighlightKind::Lifetime => {
                    CLASS_LITERAL
                }
                HighlightKind::Comment | HighlightKind::DocComment => CLASS_COMMENT,
                HighlightKind::Keyword | HighlightKind::SelfKeyword | HighlightKind::Boolean => CLASS_KEYWORD,
                _ => continue,
            };
            let a = line_start.saturating_add(span.start).min(end);
            let b = line_start.saturating_add(span.end).min(end);
            if let Some(slot) = class.get_mut(a..b) {
                slot.fill(c);
            }
        }
        match line_end {
            Some(e) => {
                line_start = e + 1;
                line += 1;
            }
            None => break,
        }
        i += 1;
        if i > chars.len() + 1 {
            break;
        }
    }
    // A literal or comment that continues on the next line is one token.
    for k in 1..chars.len().saturating_sub(1) {
        if chars.get(k) == Some(&'\n') {
            let (prev, next) = (class.get(k - 1).copied(), class.get(k + 1).copied());
            if prev == next
                && matches!(prev, Some(CLASS_LITERAL | CLASS_COMMENT))
                && let Some(c) = class.get_mut(k)
            {
                *c = prev.unwrap_or(CLASS_NONE);
            }
        }
    }
    let dollar_ident = matches!(language, Language::JavaScript | Language::TypeScript);
    let is_ident_start = |c: char| c.is_alphabetic() || c == '_' || dollar_ident && c == '$';
    let is_ident_char = |c: char| c.is_alphanumeric() || c == '_' || dollar_ident && c == '$';
    let mut out = Vec::new();
    let mut joined = false;
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        let cls = class.get(i).copied().unwrap_or(CLASS_NONE);
        if cls == CLASS_COMMENT {
            while class.get(i) == Some(&CLASS_COMMENT) {
                i += 1;
            }
            joined = false;
            continue;
        }
        if cls == CLASS_LITERAL {
            let start = i;
            while class.get(i) == Some(&CLASS_LITERAL) {
                i += 1;
            }
            out.push(tok(TokKind::Literal, &chars, start, i, joined));
            joined = true;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            joined = false;
            continue;
        }
        let start = i;
        let kind = if is_ident_start(c) {
            while chars.get(i).is_some_and(|c| is_ident_char(*c)) {
                i += 1;
            }
            if cls == CLASS_KEYWORD { TokKind::Keyword } else { TokKind::Ident }
        } else if c.is_ascii_digit() {
            while chars.get(i).is_some_and(|c| {
                c.is_alphanumeric() || *c == '_' || *c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)
            }) {
                i += 1;
            }
            TokKind::Literal
        } else {
            i += 1;
            match c {
                '(' | '[' | '{' => TokKind::Open,
                ')' | ']' | '}' => TokKind::Close,
                _ => TokKind::Punct,
            }
        };
        out.push(tok(kind, &chars, start, i, joined));
        joined = true;
    }
    out
}

fn tok(kind: TokKind, chars: &[char], start: usize, end: usize, joined: bool) -> Token {
    let text: String = chars.get(start..end).map(|s| s.iter().collect()).unwrap_or_default();
    Token { kind, text, start, end, joined }
}

fn closer(open: &str) -> &'static str {
    match open {
        "(" => ")",
        "[" => "]",
        _ => "}",
    }
}

/// For each token, the index of its partner bracket (`None` for non-brackets and unmatched ones).
pub fn partners(toks: &[Token]) -> Vec<Option<usize>> {
    let mut out = vec![None; toks.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Open => stack.push(i),
            TokKind::Close => {
                // Pop until the matching opener (unmatched openers stay unmatched).
                if let Some(pos) = stack.iter().rposition(|&o| toks.get(o).is_some_and(|ot| closer(&ot.text) == t.text)) {
                    if let Some(&o) = stack.get(pos) {
                        if let Some(slot) = out.get_mut(o) {
                            *slot = Some(i);
                        }
                        if let Some(slot) = out.get_mut(i) {
                            *slot = Some(o);
                        }
                    }
                    stack.truncate(pos);
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- templates

/// A per-variable constraint (as saved with a template).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Constraint {
    pub name: String,
    /// The variable's text must match this regex entirely (empty: no text constraint).
    pub regex: String,
    /// The text must NOT match the regex.
    pub invert: bool,
    /// Minimum number of items (default 1).
    pub min: Option<usize>,
    /// Maximum number of items (default 1; `null` with `unbounded` for ∞).
    pub max: Option<usize>,
    /// No upper bound on the item count.
    pub unbounded: bool,
}

#[derive(Clone, Debug)]
pub struct VarSpec {
    pub name: String,
    pub min: usize,
    pub max: usize,
    regex: Option<Regex>,
    invert: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Elem {
    Tok(Token),
    Var(usize),
}

/// A compiled search template.
#[derive(Clone, Debug)]
pub struct Pattern {
    elems: Vec<Elem>,
    pub vars: Vec<VarSpec>,
    pub language: Language,
}

fn var_regex() -> Option<&'static Regex> {
    static RE: std::sync::OnceLock<Option<Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$([A-Za-z_][A-Za-z0-9_]*)\$").ok()).as_ref()
}

/// The variable names of a template, in order of first appearance.
pub fn variables(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(re) = var_regex() else { return out };
    for c in re.captures_iter(template) {
        if let Some(n) = c.get(1).map(|m| m.as_str().to_string())
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

/// Compile a search template for a language.
pub fn compile(language: Language, template: &str, constraints: &[Constraint]) -> Result<Pattern, String> {
    if template.chars().count() > MAX_TEMPLATE_CHARS {
        return Err(format!("the template is too long (over {MAX_TEMPLATE_CHARS} characters)"));
    }
    let names = variables(template);
    let mut vars = Vec::new();
    for name in &names {
        let c = constraints.iter().find(|c| &c.name == name);
        let min = c.and_then(|c| c.min).unwrap_or(1);
        let max = match c {
            Some(c) if c.unbounded => UNBOUNDED,
            Some(c) => c.max.unwrap_or(min.max(1)),
            None => 1,
        };
        if min > max {
            return Err(format!("${name}$: the minimum count {min} is greater than the maximum {max}"));
        }
        let regex = match c.map(|c| c.regex.trim()).filter(|r| !r.is_empty()) {
            Some(r) => Some(
                regex::RegexBuilder::new(&format!("^(?:{r})$"))
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| format!("${name}$: invalid text constraint: {e}"))?,
            ),
            None => None,
        };
        vars.push(VarSpec { name: name.clone(), min, max, regex, invert: c.is_some_and(|c| c.invert) });
    }
    let Some(re) = var_regex() else { return Err("internal error: the variable pattern is invalid".into()) };
    let lexed = re
        .replace_all(template, |caps: &regex::Captures<'_>| {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let i = names.iter().position(|n| n == name).unwrap_or(0);
            format!(" {VAR_PREFIX}{i}__ ")
        })
        .into_owned();
    let mut elems = Vec::new();
    for t in tokenize(language, &lexed) {
        if t.is_word()
            && let Some(rest) = t.text.strip_prefix(VAR_PREFIX)
            && let Some(num) = rest.strip_suffix("__")
            && let Ok(i) = num.parse::<usize>()
            && i < vars.len()
        {
            elems.push(Elem::Var(i));
            continue;
        }
        if t.text.contains(VAR_PREFIX) {
            return Err("variables inside string literals or comments are not supported".into());
        }
        elems.push(Elem::Tok(t));
    }
    if elems.is_empty() {
        return Err("the search template is empty".into());
    }
    // Brackets must balance in the template.
    let toks: Vec<Token> = elems
        .iter()
        .map(|e| match e {
            Elem::Tok(t) => t.clone(),
            Elem::Var(_) => Token { kind: TokKind::Ident, text: String::new(), start: 0, end: 0, joined: false },
        })
        .collect();
    let p = partners(&toks);
    if toks.iter().zip(&p).any(|(t, p)| matches!(t.kind, TokKind::Open | TokKind::Close) && p.is_none()) {
        return Err("the brackets in the template do not balance".into());
    }
    for v in &vars {
        if !elems.iter().any(|e| matches!(e, Elem::Var(i) if vars.get(*i).is_some_and(|x| x.name == v.name))) {
            return Err(format!("${}$ is not a standalone token in the template", v.name));
        }
    }
    Ok(Pattern { elems, vars, language })
}

/// Check a replacement template against the search template's variables.
pub fn check_replacement(search: &str, replace: &str) -> Result<(), String> {
    let known = variables(search);
    for v in variables(replace) {
        if !known.contains(&v) {
            return Err(format!("the replacement uses ${v}$, which is not in the search template"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- matching

/// One structural match: char range and each variable's char range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    /// Variable name → char range of what it matched (empty range for a zero-count match).
    pub bindings: BTreeMap<String, (usize, usize)>,
}

/// Matches in one text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileMatches {
    pub matches: Vec<Match>,
    /// The step budget or the match cap was hit.
    pub truncated: bool,
}

/// Keywords a variable may contain (expression keywords across the lexed languages).
const EXPR_KEYWORDS: &[&str] = &[
    "as", "true", "false", "self", "Self", "super", "crate", "await", "move", "mut", "ref", "dyn", "None", "Some",
    "Ok", "Err", "null", "nil", "this", "new", "typeof", "instanceof", "in", "not", "and", "or", "is", "True",
    "False", "undefined", "void", "unsafe", "async", "lambda", "nullptr", "sizeof", "try", "comptime", "orelse",
    "catch", "NULL",
];

struct Matcher<'a> {
    pat: &'a Pattern,
    toks: &'a [Token],
    partner: &'a [Option<usize>],
    chars: &'a [char],
    steps: usize,
    exhausted: bool,
    /// A match must end at or before this token index.
    limit: usize,
}

impl Matcher<'_> {
    fn text(&self, a: usize, b: usize) -> String {
        if a >= b {
            return String::new();
        }
        let (Some(s), Some(e)) = (self.toks.get(a), self.toks.get(b - 1)) else { return String::new() };
        self.chars.get(s.start..e.end).map(|c| c.iter().collect()).unwrap_or_default()
    }

    fn is_assignment(&self, j: usize) -> bool {
        let Some(t) = self.toks.get(j) else { return false };
        if !t.punct("=") {
            return false;
        }
        let next_joined = self.toks.get(j + 1).filter(|n| n.joined).map(|n| n.text.as_str());
        let prev_joined = if t.joined { j.checked_sub(1).and_then(|p| self.toks.get(p)).map(|p| p.text.as_str()) } else { None };
        !matches!(next_joined, Some("=" | ">")) && !matches!(prev_joined, Some("=" | "!" | "<" | ">"))
    }

    /// Possible ends (exclusive token index) of variable `v` starting at token `ci`, shortest first.
    fn extents(&self, v: &VarSpec, ci: usize, receiver: bool) -> Vec<usize> {
        let mut out = Vec::new();
        if v.min == 0 {
            out.push(ci);
        }
        // A variable never starts with a postfix part of an expression (`.b()`, `?`, the `(…)`
        // of a call or the `[…]` of an index).
        if let Some(first) = self.toks.get(ci) {
            let postfix = matches!(first.text.as_str(), "." | "?" | ":") && first.kind == TokKind::Punct
                || first.kind == TokKind::Open
                    && first.text != "{"
                    && first.joined
                    && ci.checked_sub(1).and_then(|p| self.toks.get(p)).is_some_and(|p| {
                        matches!(p.kind, TokKind::Ident | TokKind::Close | TokKind::Literal)
                            || p.kind == TokKind::Keyword && EXPR_KEYWORDS.contains(&p.text.as_str())
                    });
            if postfix {
                return out;
            }
        }
        let mut j = ci;
        let mut closed_items = 0usize;
        let mut open_item = false;
        while let Some(t) = self.toks.get(j) {
            match t.kind {
                TokKind::Close => break,
                TokKind::Open => {
                    if receiver && j > ci && t.text == "{" {
                        break;
                    }
                    let Some(p) = self.partner.get(j).copied().flatten() else { break };
                    if p <= j {
                        break;
                    }
                    j = p + 1;
                    open_item = true;
                }
                TokKind::Punct if t.text == "," || t.text == ";" => {
                    if v.max <= 1 || !open_item {
                        break;
                    }
                    closed_items = closed_items.saturating_add(1);
                    open_item = false;
                    j += 1;
                    // A list may end with its separator (`a(); b();`, `[1, 2,]`).
                    if (v.min..=v.max).contains(&closed_items) {
                        out.push(j);
                    }
                    continue;
                }
                kind => {
                    if v.max <= 1 {
                        if kind == TokKind::Keyword && !EXPR_KEYWORDS.contains(&t.text.as_str()) {
                            break;
                        }
                        if self.is_assignment(j) {
                            break;
                        }
                        if receiver
                            && kind == TokKind::Punct
                            && !(matches!(t.text.as_str(), "." | ":" | "?" | "'") || t.text == "!" && j > ci)
                        {
                            break;
                        }
                    }
                    j += 1;
                    open_item = true;
                }
            }
            let count = closed_items.saturating_add(usize::from(open_item));
            if count > v.max {
                break;
            }
            if count >= v.min {
                out.push(j);
            }
            if out.len() >= MAX_CANDIDATES {
                break;
            }
        }
        out
    }

    fn accept(&self, v: &VarSpec, a: usize, b: usize) -> bool {
        match &v.regex {
            None => true,
            Some(re) => re.is_match(&self.text(a, b)) != v.invert,
        }
    }

    fn step(&mut self) -> bool {
        self.steps += 1;
        if self.steps > STEPS_PER_START {
            self.exhausted = true;
        }
        !self.exhausted
    }

    fn go(&mut self, pi: usize, ci: usize, binds: &mut Vec<Option<(usize, usize)>>) -> Option<usize> {
        if !self.step() {
            return None;
        }
        let Some(elem) = self.pat.elems.get(pi) else { return (ci <= self.limit).then_some(ci) };
        match elem {
            Elem::Tok(pt) => {
                let t = self.toks.get(ci)?;
                if !same(pt, t) {
                    return None;
                }
                self.go(pi + 1, ci + 1, binds)
            }
            Elem::Var(v) => {
                let v = *v;
                if let Some((a, b)) = binds.get(v).copied().flatten() {
                    let len = b.saturating_sub(a);
                    let mine = self.toks.get(a..b)?;
                    let here = self.toks.get(ci..ci.checked_add(len)?)?;
                    if !mine.iter().zip(here).all(|(x, y)| same(x, y)) {
                        return None;
                    }
                    return self.go(pi + 1, ci + len, binds);
                }
                let spec = self.pat.vars.get(v)?;
                let receiver = matches!(
                    self.pat.elems.get(pi + 1),
                    Some(Elem::Tok(t)) if t.punct(".") || t.punct("?") || t.kind == TokKind::Open && t.text != "{"
                );
                let ends = self.extents(spec, ci, receiver);
                for end in ends.into_iter().rev() {
                    if self.exhausted {
                        return None;
                    }
                    let spec = self.pat.vars.get(v)?;
                    if !self.accept(spec, ci, end) {
                        continue;
                    }
                    if let Some(slot) = binds.get_mut(v) {
                        *slot = Some((ci, end));
                    }
                    if let Some(r) = self.go(pi + 1, end, binds) {
                        return Some(r);
                    }
                    if let Some(slot) = binds.get_mut(v) {
                        *slot = None;
                    }
                }
                None
            }
        }
    }
}

/// Find the template's matches in `text`. `nested`: also report matches inside other matches
/// (search); otherwise matches never overlap (replace).
pub fn find(pat: &Pattern, text: &str, nested: bool) -> FileMatches {
    let toks = tokenize(pat.language, text);
    let chars: Vec<char> = text.chars().collect();
    let partner = partners(&toks);
    let mut out = FileMatches::default();
    let mut total_steps = 0usize;
    let starts_with_word = matches!(
        pat.elems.first(),
        Some(Elem::Var(_)) | Some(Elem::Tok(Token { kind: TokKind::Ident | TokKind::Keyword, .. }))
    );
    let mut ci = 0usize;
    let mut last_end = 0usize;
    'starts: while ci < toks.len() {
        // A match never starts in the middle of a path (`a.b`, `a::b`).
        if starts_with_word
            && ci > 0
            && toks.get(ci - 1).is_some_and(|p| p.punct(".") || p.punct(":") && toks.get(ci).is_some_and(|t| t.joined))
        {
            ci += 1;
            continue;
        }
        // Search also reports shorter matches at the same start (`a.unwrap()` inside
        // `a.unwrap().unwrap()`): the end limit shrinks after each one.
        let mut limit = usize::MAX;
        let mut next = ci + 1;
        for _ in 0..MAX_SAME_START {
            let mut m =
                Matcher { pat, toks: &toks, partner: &partner, chars: &chars, steps: 0, exhausted: false, limit };
            let mut binds = vec![None; pat.vars.len()];
            let found = m.go(0, ci, &mut binds);
            total_steps = total_steps.saturating_add(m.steps);
            if m.exhausted {
                out.truncated = true;
            }
            let Some(end) = found.filter(|e| *e > ci) else { break };
            let (Some(first), Some(last)) = (toks.get(ci), toks.get(end - 1)) else { break 'starts };
            let mut bindings = BTreeMap::new();
            for (spec, b) in pat.vars.iter().zip(&binds) {
                let range = match b {
                    Some((a, e)) if e > a => {
                        let s = toks.get(*a).map(|t| t.start).unwrap_or(0);
                        let x = toks.get(e - 1).map(|t| t.end).unwrap_or(s);
                        (s, x)
                    }
                    Some((a, _)) => {
                        let at = toks.get(*a).map(|t| t.start).unwrap_or(last.end);
                        (at, at)
                    }
                    None => (first.start, first.start),
                };
                bindings.insert(spec.name.clone(), range);
            }
            if nested || first.start >= last_end {
                out.matches.push(Match { start: first.start, end: last.end, bindings });
                last_end = last.end;
            }
            if out.matches.len() >= MAX_MATCHES_PER_FILE {
                out.truncated = true;
                break 'starts;
            }
            if !nested {
                next = end;
                break;
            }
            limit = end - 1;
            if limit <= ci {
                break;
            }
        }
        ci = next;
        if total_steps > STEPS_PER_FILE {
            out.truncated = true;
            break;
        }
    }
    out
}

// ---------------------------------------------------------------- replacing

/// The text replacing a match: the replacement template with each `$var$` substituted by what
/// the variable matched; continuation lines get the indentation of the match's first line.
pub fn replacement(template: &str, m: &Match, text_chars: &[char]) -> String {
    let slice = |a: usize, b: usize| -> String { text_chars.get(a..b).map(|c| c.iter().collect()).unwrap_or_default() };
    let Some(re) = var_regex() else { return template.to_string() };
    let filled = re
        .replace_all(template, |caps: &regex::Captures<'_>| {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            match m.bindings.get(name) {
                Some((a, b)) => slice(*a, *b),
                None => caps.get(0).map(|x| x.as_str().to_string()).unwrap_or_default(),
            }
        })
        .into_owned();
    if !filled.contains('\n') {
        return filled;
    }
    let line_start = text_chars
        .get(..m.start.min(text_chars.len()))
        .and_then(|before| before.iter().rposition(|c| *c == '\n'))
        .map(|p| p + 1)
        .unwrap_or(0);
    let indent: String = text_chars
        .get(line_start..)
        .map(|rest| rest.iter().take_while(|c| **c == ' ' || **c == '\t').collect())
        .unwrap_or_default();
    // Variable values already carry their own indentation; only template lines get the prefix.
    let mut out = String::with_capacity(filled.len());
    for (i, line) in filled.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(&indent);
            }
        }
        out.push_str(line);
    }
    out
}

// ---------------------------------------------------------------- saved templates

/// A saved (or built-in) structural search template.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTemplate {
    pub name: String,
    /// Language id (`rust`, `python`, …; see [`parse_language`]).
    pub language: String,
    pub search: String,
    pub replace: Option<String>,
    pub constraints: Vec<Constraint>,
}

fn unbounded(name: &str, min: usize) -> Constraint {
    Constraint { name: name.into(), min: Some(min), unbounded: true, ..Constraint::default() }
}

/// Templates that ship with CodeCraft (written for CodeCraft; not copied from any IDE).
pub fn builtin_templates() -> Vec<SavedTemplate> {
    let t = |name: &str, lang: &str, search: &str, replace: Option<&str>, c: Vec<Constraint>| SavedTemplate {
        name: name.into(),
        language: lang.into(),
        search: search.into(),
        replace: replace.map(str::to_string),
        constraints: c,
    };
    vec![
        t("unwrap() calls", "rust", "$expr$.unwrap()", None, vec![]),
        t("expect() calls", "rust", "$expr$.expect($message$)", None, vec![]),
        t("unwrap() → expect()", "rust", "$expr$.unwrap()", Some("$expr$.expect(\"TODO\")"), vec![]),
        t("clone() calls", "rust", "$expr$.clone()", None, vec![]),
        t("println! calls", "rust", "println!($args$)", None, vec![unbounded("args", 0)]),
        t("dbg! calls", "rust", "dbg!($args$)", None, vec![unbounded("args", 0)]),
        t("unsafe blocks", "rust", "unsafe { $body$ }", None, vec![unbounded("body", 0)]),
        t("if let Some", "rust", "if let Some($pat$) = $expr$ { $body$ }", None, vec![unbounded("body", 0)]),
        t("functions with a name", "rust", "fn $name$($params$)", None, vec![unbounded("params", 0)]),
        t("console.log calls", "typescript", "console.log($args$)", None, vec![unbounded("args", 0)]),
        t("print calls", "python", "print($args$)", None, vec![unbounded("args", 0)]),
        t("malloc calls", "c", "malloc($size$)", None, vec![]),
    ]
}

/// The languages Search Structurally offers: (id, display name).
pub const LANGUAGES: &[(&str, Language)] = &[
    ("rust", Language::Rust),
    ("toml", Language::Toml),
    ("json", Language::Json),
    ("yaml", Language::Yaml),
    ("shell", Language::Shell),
    ("c", Language::C),
    ("cpp", Language::Cpp),
    ("python", Language::Python),
    ("zig", Language::Zig),
    ("typescript", Language::TypeScript),
    ("javascript", Language::JavaScript),
    ("sql", Language::Sql),
    ("dockerfile", Language::Dockerfile),
    ("markdown", Language::Markdown),
    ("text", Language::PlainText),
];

/// A language by id, display name, LSP id or file extension.
pub fn parse_language(s: &str) -> Option<Language> {
    let k = s.trim().to_ascii_lowercase();
    if k.is_empty() {
        return None;
    }
    LANGUAGES
        .iter()
        .find(|(id, l)| *id == k || l.name().to_ascii_lowercase() == k || l.lsp_id() == k)
        .map(|(_, l)| *l)
        .or_else(|| {
            let l = Language::from_path(std::path::Path::new(&format!("x.{k}")));
            (l != Language::PlainText).then_some(l)
        })
}

/// The id [`parse_language`] accepts for a language.
pub fn language_id(l: Language) -> &'static str {
    LANGUAGES.iter().find(|(_, x)| *x == l).map(|(id, _)| *id).unwrap_or("text")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn texts(pat: &str, code: &str) -> Vec<String> {
        texts_with(pat, code, &[], Language::Rust)
    }

    fn texts_with(pat: &str, code: &str, c: &[Constraint], lang: Language) -> Vec<String> {
        let p = compile(lang, pat, c).unwrap();
        let chars: Vec<char> = code.chars().collect();
        find(&p, code, true).matches.iter().map(|m| chars[m.start..m.end].iter().collect()).collect()
    }

    #[test]
    fn tokens_skip_comments_and_keep_literals_whole() {
        let toks = tokenize(Language::Rust, "let s = \"a b\"; // c\n/* x */ x += 1.5; 'a: loop {}");
        let t: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(t, ["let", "s", "=", "\"a b\"", ";", "x", "+", "=", "1.5", ";", "'a", ":", "loop", "{", "}"]);
        assert_eq!(toks[0].kind, TokKind::Keyword);
        assert_eq!(toks[3].kind, TokKind::Literal);
        // A multi-line string is one token.
        let toks = tokenize(Language::Rust, "f(\"a\nb\")");
        assert_eq!(toks.len(), 4, "{toks:?}");
    }

    #[test]
    fn literal_template_ignores_whitespace_and_comments() {
        assert_eq!(texts("foo ( 1 )", "x = foo(/*c*/1);\nfoo(\n1\n)"), ["foo(/*c*/1)", "foo(\n1\n)"]);
        assert!(texts("foo(1)", "foo(2)").is_empty());
    }

    #[test]
    fn variables_match_expressions_not_statements() {
        assert_eq!(texts("$e$.unwrap()", "let x = a.b().unwrap();"), ["a.b().unwrap()"]);
        assert_eq!(texts("$e$.unwrap()", "x = y.unwrap()"), ["y.unwrap()"]);
        // Nested matches are reported for search.
        assert_eq!(texts("$e$.unwrap()", "a.unwrap().unwrap()"), ["a.unwrap().unwrap()", "a.unwrap()"]);
        // A method call is not a function call.
        assert!(texts("foo($a$)", "x.foo(1)").is_empty());
        assert_eq!(texts("foo($a$)", "foo(1 + g(2, 3))"), ["foo(1 + g(2, 3))"]);
        // The default count is one item: no top-level commas.
        assert!(texts("foo($a$)", "foo(1, 2)").is_empty());
    }

    #[test]
    fn counts_and_repeated_variables() {
        let args = [unbounded("args", 0)];
        assert_eq!(
            texts_with("println!($args$)", "println!(); println!(\"{}\", a, b);", &args, Language::Rust),
            ["println!()", "println!(\"{}\", a, b)"]
        );
        let two = [Constraint { name: "a".into(), min: Some(2), max: Some(2), ..Default::default() }];
        assert_eq!(texts_with("f($a$)", "f(1) f(1, 2) f(1, 2, 3)", &two, Language::Rust), ["f(1, 2)"]);
        assert_eq!(texts("$a$ == $a$", "x == x; x == y; f(1) == f(1)"), ["x == x", "f(1) == f(1)"]);
    }

    #[test]
    fn regex_constraints() {
        let c = [Constraint { name: "name".into(), regex: "test_.*".into(), ..Default::default() }];
        assert_eq!(texts_with("fn $name$()", "fn test_a() {} fn b() {}", &c, Language::Rust), ["fn test_a()"]);
        let inv = [Constraint { name: "name".into(), regex: "test_.*".into(), invert: true, ..Default::default() }];
        assert_eq!(texts_with("fn $name$()", "fn test_a() {} fn b() {}", &inv, Language::Rust), ["fn b()"]);
        let bad = [Constraint { name: "name".into(), regex: "((".into(), ..Default::default() }];
        assert!(compile(Language::Rust, "fn $name$()", &bad).is_err());
        let minmax = [Constraint { name: "x".into(), min: Some(3), max: Some(1), ..Default::default() }];
        assert!(compile(Language::Rust, "$x$", &minmax).is_err());
    }

    #[test]
    fn bad_templates_are_errors() {
        assert!(compile(Language::Rust, "", &[]).is_err());
        assert!(compile(Language::Rust, "  // only a comment", &[]).is_err());
        assert!(compile(Language::Rust, "foo(", &[]).is_err());
        assert!(compile(Language::Rust, "\"$x$\"", &[]).is_err());
        assert!(compile(Language::Rust, &"x".repeat(MAX_TEMPLATE_CHARS + 1), &[]).is_err());
        assert!(check_replacement("$a$", "$b$").is_err());
        assert!(check_replacement("$a$ + $b$", "$b$ + $a$").is_ok());
    }

    #[test]
    fn other_languages() {
        let c = [unbounded("args", 0)];
        assert_eq!(texts_with("print($args$)", "print('a', b)\nx.print(1)\n", &c, Language::Python), ["print('a', b)"]);
        assert_eq!(
            texts_with("console.log($args$)", "console.log(`x ${y}`); log(1)", &c, Language::TypeScript),
            ["console.log(`x ${y}`)"]
        );
        assert_eq!(texts_with("malloc($n$)", "p = malloc(sizeof(int) * 4);", &[], Language::C), ["malloc(sizeof(int) * 4)"]);
    }

    #[test]
    fn replacement_substitutes_and_indents() {
        let p = compile(Language::Rust, "if $c$ { $body$ }", &[unbounded("body", 0)]).unwrap();
        let code = "fn f() {\n    if x { a(); b(); }\n}\n";
        let r = find(&p, code, false);
        assert_eq!(r.matches.len(), 1);
        let chars: Vec<char> = code.chars().collect();
        let out = replacement("if !($c$) {\n    return;\n}\n$body$", &r.matches[0], &chars);
        assert_eq!(out, "if !(x) {\n        return;\n    }\n    a(); b();");
        // Non-overlapping for replace.
        let p = compile(Language::Rust, "$e$.unwrap()", &[]).unwrap();
        assert_eq!(find(&p, "a.unwrap().unwrap()", false).matches.len(), 1);
    }

    #[test]
    fn hostile_inputs_terminate() {
        let p = compile(Language::Rust, "$a$ $b$ $c$ $d$ $e$ ;", &[]).unwrap();
        let code = "x ".repeat(3000);
        let r = find(&p, &code, true);
        assert!(r.matches.is_empty());
        let p = compile(Language::Rust, "$a$", &[unbounded("a", 0)]).unwrap();
        let r = find(&p, &"(".repeat(5000), true);
        assert!(r.matches.is_empty());
        let r = find(&p, &")]}".repeat(500), true);
        assert!(r.matches.is_empty());
        for code in ["", "\u{0}", "😀(😀)", "\"unterminated", "/* open", "'", "$x$", "\r\n\r\n"] {
            let _ = find(&p, code, true);
            let _ = tokenize(Language::Rust, code);
            for (_, l) in LANGUAGES {
                let _ = tokenize(*l, code);
            }
        }
    }

    #[test]
    fn builtins_compile_and_languages_parse() {
        for t in builtin_templates() {
            let lang = parse_language(&t.language).unwrap();
            compile(lang, &t.search, &t.constraints).unwrap_or_else(|e| panic!("{}: {e}", t.name));
            if let Some(r) = &t.replace {
                check_replacement(&t.search, r).unwrap();
            }
        }
        assert_eq!(parse_language("Rust"), Some(Language::Rust));
        assert_eq!(parse_language("py"), Some(Language::Python));
        assert_eq!(parse_language("nope"), None);
        assert_eq!(language_id(Language::Python), "python");
    }
}
