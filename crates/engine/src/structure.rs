//! Lexical code structure for editor actions: which chars are code (not strings or comments),
//! bracket pairs, the items enclosing a position (breadcrumbs), and pure text transformations
//! (Move Element Left/Right, Complete Current Statement, Auto-Indent Lines, Unwrap/Remove,
//! Cyclic Expand Word). Everything works on chars (char offsets), never panics and caps work
//! on hostile input.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use codecraft_syntax::{HighlightKind, Language, SymbolKind};

/// Languages whose blocks are `{ … }`.
pub fn is_brace_language(lang: Language) -> bool {
    matches!(
        lang,
        Language::Rust | Language::C | Language::Cpp | Language::TypeScript | Language::JavaScript | Language::Zig
    )
}

/// A document as chars plus a per-char "is code" mask (false inside strings and comments).
pub struct Code {
    pub chars: Vec<char>,
    pub code: Vec<bool>,
    /// Char offset of each line start.
    pub line_starts: Vec<usize>,
}

impl Code {
    pub fn new(language: Language, text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut line_starts = vec![0usize];
        for (i, c) in chars.iter().enumerate() {
            if *c == '\n' {
                line_starts.push(i + 1);
            }
        }
        let mut code = vec![true; chars.len()];
        let hl = codecraft_syntax::highlight(language, text);
        for (line, start) in line_starts.iter().enumerate() {
            for span in hl.line(line) {
                if matches!(
                    span.kind,
                    HighlightKind::String | HighlightKind::Char | HighlightKind::Comment | HighlightKind::DocComment
                ) {
                    let a = start.saturating_add(span.start).min(code.len());
                    let b = start.saturating_add(span.end).min(code.len());
                    for m in code.get_mut(a..b).into_iter().flatten() {
                        *m = false;
                    }
                }
            }
        }
        Self { chars, code, line_starts }
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn char(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    /// The char at `i` if it is code.
    pub fn code_char(&self, i: usize) -> Option<char> {
        if self.code.get(i).copied().unwrap_or(false) { self.char(i) } else { None }
    }

    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|s| *s <= offset).saturating_sub(1)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts.get(line).copied().unwrap_or(self.chars.len())
    }

    /// End of a line (before its `\n`).
    pub fn line_end(&self, line: usize) -> usize {
        match self.line_starts.get(line + 1) {
            Some(next) => next.saturating_sub(1),
            None => self.chars.len(),
        }
    }

    pub fn slice(&self, r: Range<usize>) -> String {
        let end = r.end.min(self.chars.len());
        self.chars.get(r.start.min(end)..end).unwrap_or(&[]).iter().collect()
    }

    pub fn line_text(&self, line: usize) -> String {
        self.slice(self.line_start(line)..self.line_end(line))
    }

    /// Leading whitespace of a line.
    pub fn indent_of(&self, line: usize) -> String {
        self.line_text(line).chars().take_while(|c| *c == ' ' || *c == '\t').collect()
    }

    /// Matched bracket pairs `(open, close)` over code chars, sorted by open offset.
    pub fn pairs(&self) -> Vec<(usize, usize)> {
        let mut stack: Vec<(char, usize)> = Vec::new();
        let mut out = Vec::new();
        for (i, c) in self.chars.iter().enumerate() {
            if !self.code.get(i).copied().unwrap_or(false) {
                continue;
            }
            match c {
                '(' | '[' | '{' => stack.push((*c, i)),
                ')' | ']' | '}' => {
                    let want = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    // Pop to the matching opener (unbalanced openers in between are dropped).
                    if let Some(pos) = stack.iter().rposition(|(o, _)| *o == want) {
                        if let Some((_, open)) = stack.get(pos).copied() {
                            out.push((open, i));
                        }
                        stack.truncate(pos);
                    }
                }
                _ => {}
            }
        }
        out.sort_unstable();
        out
    }
}

/// Pairs that contain `offset` strictly inside (`open < offset <= close`), innermost first.
pub fn enclosing(pairs: &[(usize, usize)], offset: usize) -> Vec<(usize, usize)> {
    let mut v: Vec<(usize, usize)> = pairs.iter().copied().filter(|(o, c)| *o < offset && offset <= *c).collect();
    v.sort_by_key(|(o, c)| c.saturating_sub(*o));
    v
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ------------------------------------------------------------------------------- breadcrumbs

/// An outline item with its extent (char offsets), for breadcrumbs and the navigation bar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub name: String,
    pub kind: SymbolKind,
    pub depth: usize,
    pub line: usize,
    pub column: usize,
    /// Offset of the item's name.
    pub offset: usize,
    /// Last offset inside the item (its closing brace or last line end).
    pub end: usize,
}

impl Extent {
    /// Label for breadcrumbs (`main()` for functions).
    pub fn label(&self) -> String {
        match self.kind {
            SymbolKind::Function => format!("{}()", self.name),
            _ => self.name.clone(),
        }
    }
}

/// How far to look for an item's body after its name.
const BODY_SEARCH: usize = 4000;

/// Outline items with their extents.
pub fn extents(language: Language, text: &str) -> Vec<Extent> {
    let outline = codecraft_syntax::outline(language, text);
    if outline.is_empty() {
        return Vec::new();
    }
    let code = Code::new(language, text);
    let pairs = if is_brace_language(language) { code.pairs() } else { Vec::new() };
    let close_of = |open: usize| pairs.binary_search_by_key(&open, |(o, _)| *o).ok().and_then(|i| pairs.get(i));
    let mut out: Vec<Extent> = Vec::with_capacity(outline.len());
    for (idx, item) in outline.iter().enumerate() {
        let offset = code.line_start(item.line).saturating_add(item.column).min(code.len());
        let end = if is_brace_language(language) {
            // The first `{` or `;` at bracket depth 0 after the name.
            let mut depth = 0usize;
            let mut found = None;
            let mut i = offset;
            while i < code.len() && i < offset + BODY_SEARCH {
                match code.code_char(i) {
                    Some('(' | '[') => depth += 1,
                    Some(')' | ']') => depth = depth.saturating_sub(1),
                    Some('{') if depth == 0 => {
                        found = close_of(i).map(|(_, c)| *c);
                        break;
                    }
                    Some(';') if depth == 0 => {
                        found = Some(i);
                        break;
                    }
                    _ => {}
                }
                i += 1;
            }
            found.unwrap_or_else(|| code.line_end(item.line))
        } else if language == Language::Python {
            let indent = code.indent_of(item.line).chars().count();
            let mut last = item.line;
            let mut l = item.line + 1;
            while l < code.line_starts.len() {
                let t = code.line_text(l);
                if !t.trim().is_empty() {
                    if t.chars().take_while(|c| *c == ' ' || *c == '\t').count() <= indent {
                        break;
                    }
                    last = l;
                }
                l += 1;
            }
            code.line_end(last)
        } else {
            // Sections: until the next item at the same or a shallower depth.
            let next = outline.iter().skip(idx + 1).find(|o| o.depth <= item.depth);
            match next {
                Some(n) => code.line_start(n.line).saturating_sub(1),
                None => code.len(),
            }
        };
        out.push(Extent {
            name: item.name.clone(),
            kind: item.kind,
            depth: item.depth,
            line: item.line,
            column: item.column,
            offset,
            end: end.max(offset),
        });
    }
    out
}

/// The items enclosing `offset`, outermost first.
pub fn crumbs_at(extents: &[Extent], offset: usize) -> Vec<Extent> {
    let mut v: Vec<Extent> = extents
        .iter()
        .filter(|e| {
            let line_start_ok = e.offset <= offset || e.offset.saturating_sub(e.column) <= offset;
            line_start_ok && offset <= e.end
        })
        .cloned()
        .collect();
    v.sort_by_key(|e| (e.offset, std::cmp::Reverse(e.end)));
    v
}

// ------------------------------------------------------------------------------- move element

/// Swap the comma-separated element at `caret` (inside the innermost `( )`, `[ ]` or `{ }`
/// list) with its neighbour. Returns the replacement `(range, text)` covering both elements
/// and the new caret.
pub fn move_element(code: &Code, caret: usize, right: bool) -> Option<(Range<usize>, String, usize)> {
    let pairs = code.pairs();
    for (open, close) in enclosing(&pairs, caret) {
        let elements = split_elements(code, open, close, &pairs);
        if elements.len() < 2 {
            continue;
        }
        let i = elements
            .iter()
            .position(|r| r.start <= caret && caret <= r.end)
            .or_else(|| elements.iter().position(|r| r.start >= caret))
            .unwrap_or(elements.len() - 1);
        let j = if right { i.checked_add(1).filter(|j| *j < elements.len())? } else { i.checked_sub(1)? };
        let (a, b) = (elements.get(i.min(j))?.clone(), elements.get(i.max(j))?.clone());
        let ta = code.slice(a.clone());
        let tb = code.slice(b.clone());
        let between = code.slice(a.end..b.start);
        let text = format!("{tb}{between}{ta}");
        let rel = caret.saturating_sub(elements.get(i)?.start);
        let la = ta.chars().count();
        let lb = tb.chars().count();
        let new_start = if right {
            // The element moved to where the right one was (shifted by the length difference).
            b.start + lb - la
        } else {
            a.start
        };
        let moved_len = if right { la } else { code.slice(elements.get(i)?.clone()).chars().count() };
        return Some((a.start..b.end, text, new_start + rel.min(moved_len)));
    }
    None
}

/// Element ranges (trimmed, non-empty) between `open` and `close`, split at top-level commas.
fn split_elements(code: &Code, open: usize, close: usize, pairs: &[(usize, usize)]) -> Vec<Range<usize>> {
    let opener = code.char(open);
    let mut out = Vec::new();
    let mut start = open + 1;
    let mut i = open + 1;
    let mut saw_semicolon = false;
    while i < close {
        match code.code_char(i) {
            Some('(' | '[' | '{') => {
                // Skip a nested group whole.
                if let Ok(k) = pairs.binary_search_by_key(&i, |(o, _)| *o)
                    && let Some((_, c)) = pairs.get(k)
                    && *c < close
                {
                    i = *c + 1;
                    continue;
                }
            }
            Some(',') => {
                out.push(start..i);
                start = i + 1;
            }
            Some(';') => saw_semicolon = true,
            _ => {}
        }
        i += 1;
    }
    out.push(start..close);
    if opener == Some('{') && saw_semicolon {
        return Vec::new();
    }
    out.into_iter()
        .filter_map(|r| {
            let mut a = r.start;
            let mut b = r.end;
            while a < b && code.char(a).is_some_and(char::is_whitespace) {
                a += 1;
            }
            while b > a && code.char(b - 1).is_some_and(char::is_whitespace) {
                b -= 1;
            }
            (a < b).then_some(a..b)
        })
        .collect()
}

// ------------------------------------------------------------------------------- complete statement

const RUST_BLOCK: &[&str] = &["if", "else", "while", "for", "loop", "match", "fn", "impl", "trait", "enum", "unsafe", "mod"];
const C_BLOCK: &[&str] = &["if", "else", "while", "for", "switch", "do", "try", "catch", "finally", "function", "class"];
const ZIG_BLOCK: &[&str] = &["if", "else", "while", "for", "fn", "test", "switch", "comptime"];
const PY_BLOCK: &[&str] =
    &["if", "elif", "else", "for", "while", "def", "class", "try", "except", "finally", "with", "async"];

/// The first keyword of a line after visibility and qualifiers (`pub`, `pub(crate)`, `async`,
/// `const`, `unsafe fn`, `export`, `static`).
fn head_keyword(line: &str) -> String {
    let mut rest = line.trim_start();
    loop {
        let word: String = rest.chars().take_while(|c| is_ident(*c)).collect();
        let after = rest.get(word.len()..).unwrap_or("");
        let skip = match word.as_str() {
            "pub" => {
                let after = after.trim_start();
                if after.starts_with('(') { after.find(')').map(|i| after.get(i + 1..).unwrap_or("")) } else { Some(after) }
            }
            "async" | "export" | "static" | "extern" | "default" | "inline" => Some(after),
            "const" | "unsafe" if after.trim_start().starts_with("fn") => Some(after),
            _ => None,
        };
        match skip {
            Some(r) => rest = r.trim_start(),
            None => return word,
        }
    }
}

/// Unclosed `(`/`[` on a line (code chars only), as the closers to append in order.
fn missing_closers(code: &Code, line: usize) -> String {
    let mut stack = Vec::new();
    for i in code.line_start(line)..code.line_end(line) {
        match code.code_char(i) {
            Some(c @ ('(' | '[')) => stack.push(c),
            Some(')') if stack.last() == Some(&'(') => {
                stack.pop();
            }
            Some(']') if stack.last() == Some(&'[') => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack.iter().rev().map(|c| if *c == '(' { ')' } else { ']' }).collect()
}

/// Complete Current Statement on the caret's line: the new text for the whole line (it may
/// contain newlines) and the caret position within it.
pub fn complete_statement(language: Language, code: &Code, line: usize, unit: &str) -> (String, usize) {
    let text = code.line_text(line);
    let indent = code.indent_of(line);
    let trimmed = text.trim_end().to_string();
    if trimmed.trim().is_empty() {
        let s = format!("{trimmed}\n{indent}");
        let n = s.chars().count();
        return (s, n);
    }
    let body = format!("{trimmed}{}", missing_closers(code, line));
    let kw = head_keyword(&body);
    let last = body.chars().last().unwrap_or(' ');
    let first = body.trim_start().chars().next().unwrap_or(' ');
    if language == Language::Python {
        let is_block = PY_BLOCK.contains(&kw.as_str());
        let line_text = if is_block && last != ':' { format!("{body}:") } else { body };
        let inner = if is_block { format!("{indent}{unit}") } else { indent.clone() };
        let s = format!("{line_text}\n{inner}");
        let n = s.chars().count();
        return (s, n);
    }
    if !is_brace_language(language) {
        let s = format!("{body}\n{indent}");
        let n = s.chars().count();
        return (s, n);
    }
    let block_words = match language {
        Language::Rust => RUST_BLOCK,
        Language::Zig => ZIG_BLOCK,
        _ => C_BLOCK,
    };
    let is_block = block_words.contains(&kw.as_str())
        || (language == Language::Rust && matches!(kw.as_str(), "struct" | "union") && !body.ends_with(';'));
    let next_is_closer = code.line_text(line + 1).trim_start().starts_with('}');
    if is_block {
        let head = if last == '{' { body } else { format!("{body} {{") };
        let inner = format!("{indent}{unit}");
        let close = if last == '{' && next_is_closer { String::new() } else { format!("\n{indent}}}") };
        let s = format!("{head}\n{inner}");
        let caret = s.chars().count();
        return (format!("{s}{close}"), caret);
    }
    let complete = matches!(last, ';' | '{' | '}' | ',' | ':') || matches!(first, '#' | '/' | '*' | '@');
    if complete && body == trimmed {
        // Nothing to finish: go to a new line.
        let s = format!("{body}\n{indent}");
        let n = s.chars().count();
        return (s, n);
    }
    let s = if complete { body } else { format!("{body};") };
    let n = s.chars().count();
    (s, n)
}

// ------------------------------------------------------------------------------- auto-indent

/// New indentation for lines `lines` of a brace language (bracket depth at each line start,
/// minus leading closers). Returns `(line, new indent)` for lines whose indentation changes.
/// Blank lines and lines starting inside a multi-line string or comment are left alone.
pub fn auto_indent(code: &Code, lines: Range<usize>, unit: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut depth: isize = 0;
    let mut pos = 0usize;
    let last = lines.end.min(code.line_starts.len());
    for line in 0..last {
        let start = code.line_start(line);
        // Advance the bracket depth to this line's start.
        while pos < start {
            match code.code_char(pos) {
                Some('(' | '[' | '{') => depth += 1,
                Some(')' | ']' | '}') => depth -= 1,
                _ => {}
            }
            pos += 1;
        }
        if line < lines.start {
            continue;
        }
        let text = code.line_text(line);
        let trimmed = text.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let ws = text.chars().count() - trimmed.chars().count();
        let first = start + ws;
        let in_literal = !code.code.get(first).copied().unwrap_or(true)
            && !(trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('#'));
        if in_literal {
            continue;
        }
        let mut closers = 0isize;
        for c in trimmed.chars() {
            if matches!(c, ')' | ']' | '}') {
                closers += 1;
            } else {
                break;
            }
        }
        let continuation = isize::from(trimmed.starts_with('.') && !trimmed.starts_with(".."));
        let level = usize::try_from((depth - closers + continuation).max(0)).unwrap_or(0).min(64);
        let indent = unit.repeat(level);
        if text.chars().take(ws).collect::<String>() != indent {
            out.push((line, indent));
        }
    }
    out
}

// ------------------------------------------------------------------------------- unwrap / remove

/// One Unwrap/Remove choice: replace `range` with `text`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnwrapOption {
    pub label: String,
    pub range: Range<usize>,
    pub text: String,
}

const UNWRAP_KEYWORDS: &[&str] = &["if", "else", "while", "for", "loop", "unsafe", "try", "do", "async", "switch"];

/// Re-indent a block body: dedent it and give every line after the first `indent`.
fn reindent_body(body: &str, indent: &str) -> String {
    let trimmed: Vec<&str> = body.split('\n').collect();
    // Drop the blank first line (after `{`) and the trailing whitespace line (before `}`).
    let mut lines: &[&str] = &trimmed;
    if lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines = lines.get(1..).unwrap_or(&[]);
    }
    if lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines = lines.get(..lines.len() - 1).unwrap_or(&[]);
    }
    let joined = lines.join("\n");
    let ded = crate::templates::dedent(&joined);
    ded.split('\n')
        .enumerate()
        .map(|(i, l)| if i == 0 || l.trim().is_empty() { l.trim_end().to_string() } else { format!("{indent}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Start of the statement whose block opens at `open`: just after the previous `;`, `{` or `}`
/// at the same nesting level (skipping nested groups).
fn statement_start(code: &Code, open: usize, pairs: &[(usize, usize)]) -> usize {
    let mut i = open;
    while i > 0 {
        let j = i - 1;
        match code.code_char(j) {
            Some(')' | ']') => {
                // Jump over the group.
                match pairs.iter().find(|(_, c)| *c == j) {
                    Some((o, _)) => i = *o,
                    None => i = j,
                }
                continue;
            }
            Some(';' | '{' | '}') => break,
            _ => {}
        }
        i = j;
    }
    // Skip leading whitespace.
    while i < open && code.char(i).is_some_and(char::is_whitespace) {
        i += 1;
    }
    i
}

/// First word of a header (`else if` → `else`).
fn first_word(s: &str) -> String {
    s.trim_start().chars().take_while(|c| is_ident(*c)).collect()
}

/// After the block closing at `close`, the `else` branches that follow: returns the end of the
/// last branch (inclusive offset of its `}`), or `close` when there is no `else`.
fn chain_end(code: &Code, close: usize, pairs: &[(usize, usize)]) -> usize {
    let mut end = close;
    loop {
        let mut i = end + 1;
        while code.char(i).is_some_and(char::is_whitespace) {
            i += 1;
        }
        if code.slice(i..i + 4) != "else" || code.char(i + 4).is_some_and(is_ident) {
            return end;
        }
        // The branch's `{`.
        let mut k = i + 4;
        while k < code.len() && code.code_char(k) != Some('{') {
            k += 1;
        }
        match pairs.iter().find(|(o, _)| *o == k) {
            Some((_, c)) => end = *c,
            None => return end,
        }
    }
}

/// Unwrap/Remove choices for the code around `caret`, innermost first.
pub fn unwrap_options(language: Language, code: &Code, caret: usize) -> Vec<UnwrapOption> {
    if language == Language::Python {
        return python_unwrap_options(code, caret);
    }
    if !is_brace_language(language) {
        return Vec::new();
    }
    let pairs = code.pairs();
    let mut out = Vec::new();
    for (open, close) in enclosing(&pairs, caret) {
        if out.len() >= 4 {
            break;
        }
        match code.char(open) {
            Some('{') => {
                let start = statement_start(code, open, &pairs);
                let header = code.slice(start..open);
                let word = first_word(&header);
                let indent = code.indent_of(code.line_of(start));
                let body = reindent_body(&code.slice(open + 1..close), &indent);
                let header_trim = header.trim();
                if word == "else" {
                    // An `else` branch: find where its chain starts.
                    let prev_close = start.checked_sub(1).and_then(|p| {
                        let mut p = p;
                        while p > 0 && code.char(p).is_some_and(char::is_whitespace) {
                            p -= 1;
                        }
                        (code.code_char(p) == Some('}')).then_some(p)
                    });
                    if let Some(pc) = prev_close {
                        let chain_open = pairs.iter().find(|(_, c)| *c == pc).map(|(o, _)| *o).unwrap_or(pc);
                        let mut chain_start = statement_start(code, chain_open, &pairs);
                        // Walk back over `else if` links to the first `if`.
                        while first_word(&code.slice(chain_start..chain_open)) == "else" {
                            let Some(p) = chain_start.checked_sub(1) else { break };
                            let mut p = p;
                            while p > 0 && code.char(p).is_some_and(char::is_whitespace) {
                                p -= 1;
                            }
                            let Some((o, _)) = pairs.iter().find(|(_, c)| *c == p).copied() else { break };
                            chain_start = statement_start(code, o, &pairs);
                        }
                        let end = chain_end(code, close, &pairs);
                        out.push(UnwrapOption {
                            label: "Unwrap 'else...'".into(),
                            range: chain_start..end + 1,
                            text: body.clone(),
                        });
                        out.push(UnwrapOption {
                            label: "Remove 'else...'".into(),
                            range: pc + 1..close + 1,
                            text: String::new(),
                        });
                    }
                    continue;
                }
                let label = if header_trim.is_empty() {
                    "Unwrap braces".to_string()
                } else if UNWRAP_KEYWORDS.contains(&word.as_str()) {
                    format!("Unwrap '{word}...'")
                } else {
                    // fn bodies, impls, closures, struct literals, match: not unwrappable.
                    continue;
                };
                let end = if word == "if" { chain_end(code, close, &pairs) } else { close };
                out.push(UnwrapOption { label, range: start..end + 1, text: body });
            }
            Some('(') => {
                let inner = code.slice(open + 1..close);
                let top_comma =
                    (open + 1..close).any(|i| code.code_char(i) == Some(',') && depth_between(&pairs, open, i) == 0);
                if top_comma || inner.trim().is_empty() {
                    continue;
                }
                // `name(…)` / `name!(…)`: unwrap the call; `(…)`: unwrap parentheses.
                let mut s = open;
                if s > 0 && code.code_char(s - 1) == Some('!') {
                    s -= 1;
                }
                while s > 0 && code.code_char(s - 1).is_some_and(|c| is_ident(c) || c == ':') {
                    s -= 1;
                }
                let name = code.slice(s..open);
                let label = if name.is_empty() {
                    "Unwrap parentheses".to_string()
                } else if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    continue;
                } else {
                    format!("Unwrap '{name}(...)'")
                };
                out.push(UnwrapOption { label, range: s..close + 1, text: inner.trim().to_string() });
            }
            _ => {}
        }
    }
    out
}

/// Nesting depth of `at` relative to the group opened at `open` (0 = directly inside).
fn depth_between(pairs: &[(usize, usize)], open: usize, at: usize) -> usize {
    pairs.iter().filter(|(o, c)| *o > open && *o < at && at < *c).count()
}

fn python_unwrap_options(code: &Code, caret: usize) -> Vec<UnwrapOption> {
    let line = code.line_of(caret);
    let width = |l: usize| code.indent_of(l).chars().count();
    let mut out = Vec::new();
    let mut cur_indent = width(line);
    let mut l = line;
    while l > 0 && out.len() < 3 {
        l -= 1;
        let t = code.line_text(l);
        if t.trim().is_empty() || width(l) >= cur_indent {
            continue;
        }
        cur_indent = width(l);
        let word = first_word(&t);
        if !["if", "for", "while", "with", "try", "else", "elif"].contains(&word.as_str())
            || !t.trim_end().ends_with(':')
        {
            if word == "def" || word == "class" {
                break;
            }
            continue;
        }
        // Body: following lines indented deeper than the header.
        let mut last = l;
        let mut k = l + 1;
        while k < code.line_starts.len() {
            let tk = code.line_text(k);
            if !tk.trim().is_empty() && width(k) <= cur_indent {
                break;
            }
            if !tk.trim().is_empty() {
                last = k;
            }
            k += 1;
        }
        // Continuation branches at the header's indentation (elif/else/except/finally).
        let mut chain_last = last;
        let mut k = last + 1;
        while k < code.line_starts.len() {
            let tk = code.line_text(k);
            if tk.trim().is_empty() {
                k += 1;
                continue;
            }
            let w = first_word(&tk);
            if width(k) == cur_indent && ["elif", "else", "except", "finally"].contains(&w.as_str()) {
                let mut m = k + 1;
                while m < code.line_starts.len() {
                    let tm = code.line_text(m);
                    if !tm.trim().is_empty() && width(m) <= cur_indent {
                        break;
                    }
                    if !tm.trim().is_empty() {
                        chain_last = m;
                    }
                    m += 1;
                }
                k = chain_last + 1;
                continue;
            }
            break;
        }
        let body = code.slice(code.line_start(l + 1)..code.line_end(last));
        let header_indent = code.indent_of(l);
        let text = crate::templates::dedent(&body)
            .split('\n')
            .map(|x| if x.trim().is_empty() { String::new() } else { format!("{header_indent}{x}") })
            .collect::<Vec<_>>()
            .join("\n");
        let end = if word == "if" || word == "try" { chain_last } else { last };
        out.push(UnwrapOption {
            label: format!("Unwrap '{word}...'"),
            range: code.line_start(l)..code.line_end(end),
            text,
        });
    }
    out
}

// ------------------------------------------------------------------------------- expand word

/// Words for Cyclic Expand Word: identifiers starting with `prefix` (and longer), nearest
/// first, searching backward from the caret first (or forward first with `forward_first`).
pub fn expand_candidates(chars: &[char], word_start: usize, prefix: &str, forward_first: bool) -> Vec<String> {
    let mut before: Vec<(usize, String)> = Vec::new();
    let mut after: Vec<(usize, String)> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if !chars.get(i).copied().is_some_and(is_ident) {
            i += 1;
            continue;
        }
        let s = i;
        while chars.get(i).copied().is_some_and(is_ident) {
            i += 1;
        }
        if s == word_start {
            continue;
        }
        let w: String = chars.get(s..i).unwrap_or(&[]).iter().collect();
        if w.starts_with(prefix) && w != prefix && !w.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            if s < word_start { before.push((s, w)) } else { after.push((s, w)) }
        }
    }
    before.reverse();
    let (first, second) = if forward_first { (after, before) } else { (before, after) };
    let mut out: Vec<String> = Vec::new();
    for (_, w) in first.into_iter().chain(second) {
        if !out.contains(&w) {
            out.push(w);
            if out.len() >= 200 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust(text: &str) -> Code {
        Code::new(Language::Rust, text)
    }

    #[test]
    fn mask_skips_strings_and_comments() {
        let c = rust("a(\"(\") // )\n}");
        let pairs = c.pairs();
        assert_eq!(pairs, vec![(1, 5)]);
        assert!(!c.code[3]);
        assert_eq!(c.line_of(12), 1);
        assert_eq!(c.line_text(1), "}");
    }

    #[test]
    fn breadcrumbs_follow_nesting() {
        let text = "mod a {\n    impl Foo {\n        fn bar() {\n            x();\n        }\n    }\n}\nfn main() {}\n";
        let ex = extents(Language::Rust, text);
        let caret = text.find("x()").unwrap();
        let crumbs: Vec<String> = crumbs_at(&ex, caret).iter().map(Extent::label).collect();
        assert_eq!(crumbs.len(), 3, "{crumbs:?}");
        assert_eq!(crumbs[0], "a");
        assert_eq!(crumbs[2], "bar()");
        let main = text.find("main").unwrap();
        let crumbs: Vec<String> = crumbs_at(&ex, main + 7).iter().map(Extent::label).collect();
        assert_eq!(crumbs, vec!["main()".to_string()]);
        assert!(crumbs_at(&ex, text.len() + 50).is_empty());
    }

    #[test]
    fn python_and_markdown_extents() {
        let text = "class A:\n    def f(self):\n        pass\n\nx = 1\n";
        let ex = extents(Language::Python, text);
        let crumbs: Vec<String> = crumbs_at(&ex, text.find("pass").unwrap()).iter().map(Extent::label).collect();
        assert_eq!(crumbs, vec!["A".to_string(), "f()".to_string()]);
        assert!(crumbs_at(&ex, text.find("x = 1").unwrap()).is_empty());
        let md = "# A\ntext\n## B\nmore\n# C\n";
        let ex = extents(Language::Markdown, md);
        let crumbs: Vec<String> = crumbs_at(&ex, md.find("more").unwrap()).iter().map(|e| e.name.clone()).collect();
        assert_eq!(crumbs.len(), 2, "{crumbs:?}");
    }

    #[test]
    fn move_element_swaps_arguments() {
        let text = "f(alpha, beta, gamma)";
        let c = rust(text);
        let caret = text.find("beta").unwrap() + 1;
        let (r, t, caret2) = move_element(&c, caret, true).unwrap();
        let mut out = text.to_string();
        out.replace_range(r.clone(), &t);
        assert_eq!(out, "f(alpha, gamma, beta)");
        assert_eq!(caret2, out.find("beta").unwrap() + 1);
        let (r, t, caret3) = move_element(&c, caret, false).unwrap();
        let mut out = text.to_string();
        out.replace_range(r, &t);
        assert_eq!(out, "f(beta, alpha, gamma)");
        assert_eq!(caret3, 3);
        // At the edges nothing moves.
        assert!(move_element(&c, 3, false).is_none());
        // Nested calls count as one element.
        let text = "f(g(a, b), c)";
        let c = rust(text);
        let (r, t, _) = move_element(&c, 11, false).unwrap();
        let mut out = text.to_string();
        out.replace_range(r, &t);
        assert_eq!(out, "f(c, g(a, b))");
        // Blocks with statements are not lists.
        assert!(move_element(&rust("{ a; b, c }"), 7, true).is_none());
    }

    #[test]
    fn complete_statement_cases() {
        let c = rust("    let x = foo(1\n");
        assert_eq!(complete_statement(Language::Rust, &c, 0, "    "), ("    let x = foo(1);".into(), 19));
        let c = rust("    if x > 1\n");
        let (t, caret) = complete_statement(Language::Rust, &c, 0, "    ");
        assert_eq!(t, "    if x > 1 {\n        \n    }");
        assert_eq!(caret, "    if x > 1 {\n        ".chars().count());
        let c = rust("pub fn f(a: u8)\n");
        assert_eq!(complete_statement(Language::Rust, &c, 0, "    ").0, "pub fn f(a: u8) {\n    \n}");
        let c = rust("let y = 2;\n");
        assert_eq!(complete_statement(Language::Rust, &c, 0, "    "), ("let y = 2;\n".into(), 11));
        let c = rust("if x {\n}\n");
        assert_eq!(complete_statement(Language::Rust, &c, 0, "    ").0, "if x {\n    ");
        let c = Code::new(Language::Python, "if x\n");
        assert_eq!(complete_statement(Language::Python, &c, 0, "    ").0, "if x:\n    ");
        let c = Code::new(Language::TypeScript, "console.log(\"a\"\n");
        assert_eq!(complete_statement(Language::TypeScript, &c, 0, "  ").0, "console.log(\"a\");");
        assert_eq!(complete_statement(Language::Rust, &rust(""), 5, "    ").0, "\n");
    }

    #[test]
    fn auto_indent_by_bracket_depth() {
        let text = "fn f() {\nlet a = (\n1,\n);\n        if x {\ny();\n}\n}\n";
        let c = rust(text);
        let changes = auto_indent(&c, 0..9, "    ");
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        for (l, ind) in changes {
            let t = lines[l].trim_start().to_string();
            lines[l] = format!("{ind}{t}");
        }
        assert_eq!(
            lines.join("\n"),
            "fn f() {\n    let a = (\n        1,\n    );\n    if x {\n        y();\n    }\n}"
        );
        // Only the requested lines change.
        assert!(auto_indent(&c, 1..2, "    ").iter().all(|(l, _)| *l == 1));
    }

    #[test]
    fn unwrap_if_and_call() {
        let text = "fn f() {\n    if x {\n        a();\n        b();\n    }\n}\n";
        let c = rust(text);
        let caret = text.find("a()").unwrap();
        let opts = unwrap_options(Language::Rust, &c, caret);
        assert_eq!(opts.first().map(|o| o.label.as_str()), Some("Unwrap 'if...'"));
        let o = &opts[0];
        let s: String = text.chars().take(o.range.start).collect();
        let e: String = text.chars().skip(o.range.end).collect();
        let out = format!("{s}{}{e}", o.text);
        assert_eq!(out, "fn f() {\n    a();\n    b();\n}\n");
        // fn bodies are not offered.
        assert!(opts.iter().all(|o| !o.label.contains("fn")));

        let text = "let v = Some(compute(1));";
        let c = rust(text);
        let opts = unwrap_options(Language::Rust, &c, text.find("compute").unwrap());
        assert_eq!(opts[0].label, "Unwrap 'Some(...)'");
        assert_eq!(opts[0].text, "compute(1)");
    }

    #[test]
    fn unwrap_else_branch() {
        let text = "if a {\n    x();\n} else {\n    y();\n}\n";
        let c = rust(text);
        let opts = unwrap_options(Language::Rust, &c, text.find("y()").unwrap());
        let labels: Vec<&str> = opts.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["Unwrap 'else...'", "Remove 'else...'"]);
        let apply = |o: &UnwrapOption| {
            let s: String = text.chars().take(o.range.start).collect();
            let e: String = text.chars().skip(o.range.end).collect();
            format!("{s}{}{e}", o.text)
        };
        assert_eq!(apply(&opts[0]), "y();\n");
        assert_eq!(apply(&opts[1]), "if a {\n    x();\n}\n");
        // Unwrapping the `if` drops the else branch.
        let opts = unwrap_options(Language::Rust, &c, text.find("x()").unwrap());
        assert_eq!(apply(&opts[0]), "x();\n");
    }

    #[test]
    fn unwrap_python_block() {
        let text = "def f():\n    if x:\n        a()\n        b()\n    c()\n";
        let c = Code::new(Language::Python, text);
        let opts = unwrap_options(Language::Python, &c, text.find("a()").unwrap());
        assert_eq!(opts.len(), 1, "{opts:?}");
        let o = &opts[0];
        let s: String = text.chars().take(o.range.start).collect();
        let e: String = text.chars().skip(o.range.end).collect();
        assert_eq!(format!("{s}{}{e}", o.text), "def f():\n    a()\n    b()\n    c()\n");
    }

    #[test]
    fn expand_word_candidates_nearest_first() {
        let text = "alpha alphabet beta al alpine";
        let chars: Vec<char> = text.chars().collect();
        let ws = text.find("al ").unwrap();
        assert_eq!(expand_candidates(&chars, ws, "al", false), vec!["alphabet", "alpha", "alpine"]);
        assert_eq!(expand_candidates(&chars, ws, "al", true), vec!["alpine", "alphabet", "alpha"]);
        assert!(expand_candidates(&chars, ws, "zz", false).is_empty());
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let c = rust("}}}((([[[\"");
        let _ = c.pairs();
        let _ = move_element(&c, 999, true);
        let _ = complete_statement(Language::Rust, &c, 99, "\t");
        let _ = auto_indent(&c, 0..999, "    ");
        let _ = unwrap_options(Language::Rust, &c, 999);
        let _ = unwrap_options(Language::Python, &Code::new(Language::Python, ":\n\t:"), 3);
        let _ = extents(Language::Rust, "fn");
        assert_eq!(head_keyword("pub(crate) async fn x"), "fn");
        assert_eq!(head_keyword("pub(crate"), "pub");
    }
}
