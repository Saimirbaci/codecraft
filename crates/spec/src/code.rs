//! The code a reference is attached to: where the item ends, what it is called, and a
//! fingerprint of it that ignores comments and whitespace.
//!
//! This is lexical, not a parser: it knows comments, strings and brackets well enough to find the
//! end of the function, type or statement that follows a `// spec: REQ-n` line (or an attribute),
//! and nothing more. When it cannot tell (an unterminated item, an overlong line) it says so and
//! the reference simply goes untracked: a wrong "code changed" is worse than none.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::hash::fingerprint;
use crate::markdown::line_no;
use crate::model::CodeLink;
use crate::{MAX_ITEM_LINES, MAX_LINE_BYTES, MAX_SYMBOL_WORK};

/// Most characters of an item's first line kept as its name.
const MAX_SIGNATURE_CHARS: usize = 160;
/// Lines between a reference and its item (blank lines, comments, attributes) that are skipped.
const MAX_SKIP_LINES: usize = 40;

/// How a language writes comments, strings and blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    /// `//` and nested `/* */` comments, raw strings, lifetimes; blocks in braces.
    Rust,
    /// C, Java, Go, JavaScript, TypeScript and the like: `//` and `/* */`, strings in `"`, `'`
    /// and backticks; blocks in braces.
    CLike,
    /// Python: `#` comments, triple-quoted strings; blocks by indentation.
    Indent,
}

impl Syntax {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "py" | "pyi" | "pyw" => Self::Indent,
            _ => Self::CLike,
        }
    }

    pub fn of_path(path: &Path) -> Self {
        Self::from_extension(path.extension().and_then(|e| e.to_str()).unwrap_or(""))
    }
}

/// The item a reference is attached to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    /// What the item is called: its first line up to the body (`pub fn sign_in(user: &str) -> bool`).
    pub signature: String,
    /// First and last line of the item (1-based).
    pub start_line: u32,
    pub end_line: u32,
    /// Of the item's code without comments, whitespace-insensitive (16 hex digits).
    pub fingerprint: String,
}

/// Where an item ends and its code (comments dropped).
pub(crate) struct Extent {
    /// Index of the last line of the item.
    pub end: usize,
    pub code: String,
}

/// Find the item a reference on line `link_idx` (0-based) is attached to, and about how many
/// lines it took to look.
fn symbol_with_cost(lines: &[&str], link_idx: usize, syntax: Syntax) -> (Option<Symbol>, usize) {
    let Some(start) = item_start(lines, link_idx, syntax) else { return (None, MAX_SKIP_LINES) };
    match extent(lines, start, syntax) {
        Some(extent) => {
            let cost = extent.end.saturating_sub(start).saturating_add(1);
            let symbol = Symbol {
                signature: signature(&extent.code, syntax),
                start_line: line_no(start),
                end_line: line_no(extent.end),
                fingerprint: fingerprint(&[&compact(&extent.code)]),
            };
            (Some(symbol), cost)
        }
        None => (None, MAX_ITEM_LINES.min(lines.len().saturating_sub(start))),
    }
}

/// Fill in `symbol` for every link of the file whose text is `text` (`path` picks the language).
pub fn attach_symbols(text: &str, path: &Path, links: &mut [CodeLink]) {
    if links.is_empty() {
        return;
    }
    let syntax = Syntax::of_path(path);
    let lines: Vec<&str> = text.lines().collect();
    // Several references on one line share the item; the work per file is capped.
    let mut by_line: HashMap<u32, Option<Symbol>> = HashMap::new();
    let mut work = 0usize;
    for link in links {
        link.symbol = match by_line.get(&link.line) {
            Some(known) => known.clone(),
            None if work > MAX_SYMBOL_WORK => None,
            None => {
                let idx = usize::try_from(link.line.saturating_sub(1)).unwrap_or(usize::MAX);
                let (found, cost) = symbol_with_cost(&lines, idx, syntax);
                work = work.saturating_add(cost);
                by_line.insert(link.line, found.clone());
                found
            }
        };
    }
}

// ---------------------------------------------------------------- where the item starts

/// The first line of the item a reference is for: the line itself when code precedes the
/// comment, else the first line after it that is not blank, a comment, an attribute or a
/// decorator.
pub(crate) fn item_start(lines: &[&str], link_idx: usize, syntax: Syntax) -> Option<usize> {
    let line = lines.get(link_idx)?;
    if is_trailing(line) {
        return Some(link_idx);
    }
    let mut i = link_idx.saturating_add(1);
    let limit = i.saturating_add(MAX_SKIP_LINES);
    while i < limit {
        let t = lines.get(i)?.trim();
        if t.is_empty() || t.starts_with("//") || (syntax == Syntax::Indent && t.starts_with('#')) {
            i += 1;
        } else if t.starts_with("/*") || t.starts_with('*') {
            // A block comment: skip to the line that closes it.
            while i < limit && !lines.get(i)?.contains("*/") {
                i += 1;
            }
            i += 1;
        } else if t.starts_with("#[") || t.starts_with("#![") || t.starts_with('@') {
            i = after_attribute(lines, i);
        } else {
            return Some(i);
        }
    }
    None
}

/// Code before the comment marker: `fn a() {} // spec: REQ-1`.
fn is_trailing(line: &str) -> bool {
    let t = line.trim_start();
    !(t.is_empty() || ["//", "/*", "*", "#", "--", "<!--", ";", "@"].iter().any(|m| t.starts_with(m)))
}

/// The line after the attribute or decorator that starts on line `i` (it may span lines).
fn after_attribute(lines: &[&str], i: usize) -> usize {
    let mut balance: i32 = 0;
    let mut j = i;
    while let Some(line) = lines.get(j) {
        for c in line.chars() {
            match c {
                '(' | '[' => balance = balance.saturating_add(1),
                ')' | ']' => balance = balance.saturating_sub(1),
                _ => {}
            }
        }
        j += 1;
        if balance <= 0 || j.saturating_sub(i) > MAX_SKIP_LINES {
            break;
        }
    }
    j
}

// ---------------------------------------------------------------- where the item ends

pub(crate) fn extent(lines: &[&str], start: usize, syntax: Syntax) -> Option<Extent> {
    match syntax {
        Syntax::Indent => extent_indent(lines, start),
        Syntax::Rust | Syntax::CLike => extent_braces(lines, start, syntax),
    }
}

/// Where the lexer is between lines.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Code,
    /// Inside a `/* */` comment, nested this deep.
    Block(usize),
    /// Inside a string: closed by `quote`, or (raw) by a quote and this many `#`.
    Str {
        quote: char,
        raw: Option<usize>,
    },
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whitespace inside a string is part of its value: kept apart from layout (private-use stand-ins
/// that [`compact`] does not touch).
fn mask(c: char) -> char {
    match c {
        ' ' => '\u{E000}',
        '\t' => '\u{E001}',
        '\n' | '\r' => '\u{E002}',
        c => c,
    }
}

/// The code with its layout removed: whitespace is kept (as one space) only between two word
/// characters, where it separates tokens. `f( a , b )` and `f(a, b)` compact alike; `ab` and
/// `a b` do not.
pub fn compact(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut gap = false;
    for c in code.chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && is_ident(c) && out.chars().next_back().is_some_and(is_ident) {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out
}

/// `r"`, `r#"`, `br##"`: the number of `#`s when the quote at `i` opens a Rust raw string.
fn raw_hashes(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    let mut hashes = 0;
    while j > 0 && chars.get(j - 1) == Some(&'#') {
        j -= 1;
        hashes += 1;
    }
    if j == 0 || chars.get(j - 1) != Some(&'r') {
        return None;
    }
    let mut before = j.checked_sub(2).and_then(|k| chars.get(k)).copied();
    if before == Some('b') {
        before = j.checked_sub(3).and_then(|k| chars.get(k)).copied();
    }
    (!before.is_some_and(is_ident)).then_some(hashes)
}

/// A character outside comments: code, or inside a string or character literal (quotes included).
#[derive(Clone, Copy)]
pub(crate) enum Tok {
    Code(char),
    Str(char),
}

/// Lex one line of a brace language, carrying `mode` (open comments and strings) across lines.
/// `f` sees every character outside comments (a block comment shows as one space) and returns
/// `false` to stop; the result says whether it did.
pub(crate) fn lex_braces(line: &[char], syntax: Syntax, mode: &mut Mode, f: &mut dyn FnMut(Tok) -> bool) -> bool {
    let at = |k: usize| line.get(k).copied();
    let mut i = 0;
    while let Some(c) = at(i) {
        match *mode {
            Mode::Block(depth) => {
                if c == '*' && at(i + 1) == Some('/') {
                    *mode = if depth <= 1 { Mode::Code } else { Mode::Block(depth - 1) };
                    i += 2;
                } else if syntax == Syntax::Rust && c == '/' && at(i + 1) == Some('*') {
                    *mode = Mode::Block(depth + 1);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Str { quote, raw } => {
                if !f(Tok::Str(c)) {
                    return true;
                }
                match raw {
                    Some(hashes) => {
                        if c == '"' && (1..=hashes).all(|k| at(i + k) == Some('#')) {
                            for _ in 0..hashes {
                                if !f(Tok::Str('#')) {
                                    return true;
                                }
                            }
                            i += 1 + hashes;
                            *mode = Mode::Code;
                        } else {
                            i += 1;
                        }
                    }
                    None if c == '\\' => {
                        if let Some(next) = at(i + 1)
                            && !f(Tok::Str(next))
                        {
                            return true;
                        }
                        i += 2;
                    }
                    None => {
                        if c == quote {
                            *mode = Mode::Code;
                        }
                        i += 1;
                    }
                }
            }
            Mode::Code => {
                if c == '/' && at(i + 1) == Some('/') {
                    return false;
                }
                if c == '/' && at(i + 1) == Some('*') {
                    *mode = Mode::Block(1);
                    i += 2;
                    if !f(Tok::Code(' ')) {
                        return true;
                    }
                    continue;
                }
                i += 1;
                if !f(Tok::Code(c)) {
                    return true;
                }
                match c {
                    '"' => *mode = Mode::Str { quote: '"', raw: raw_hashes(line, i - 1) },
                    '`' if syntax == Syntax::CLike => *mode = Mode::Str { quote: '`', raw: None },
                    '\'' if syntax == Syntax::CLike => *mode = Mode::Str { quote: '\'', raw: None },
                    '\'' => {
                        // Rust: a character literal, or a lifetime (which has no closing quote).
                        let literal = match at(i) {
                            Some('\\') => (i + 2..i + 12).find(|k| at(*k) == Some('\'')),
                            Some(_) if at(i + 1) == Some('\'') => Some(i + 1),
                            _ => None,
                        };
                        if let Some(close) = literal {
                            for ch in line.get(i..=close).unwrap_or_default() {
                                if !f(Tok::Str(*ch)) {
                                    return true;
                                }
                            }
                            i = close + 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// A quote left open at the end of a line is a stray one (a regex, an apostrophe) unless the
/// language lets the string go on (Rust strings, JS template literals, a trailing backslash).
fn close_stray_quote(mode: &mut Mode, line: &str, syntax: Syntax) {
    if let Mode::Str { quote, raw: None } = *mode
        && syntax == Syntax::CLike
        && quote != '`'
        && !line.ends_with('\\')
    {
        *mode = Mode::Code;
    }
}

/// Brace-delimited languages: the item ends at the `}` that closes its first `{`, or at a `;`
/// reached before any brace. Brackets and parentheses are tracked so a `;` in `[0; 3]` and a
/// closure's `}` inside a call do not end it.
fn extent_braces(lines: &[&str], start: usize, syntax: Syntax) -> Option<Extent> {
    let mut code = String::new();
    let mut mode = Mode::Code;
    let (mut braces, mut parens, mut opened) = (0usize, 0usize, false);
    for (offset, line) in lines.get(start..)?.iter().take(MAX_ITEM_LINES).enumerate() {
        if line.len() > MAX_LINE_BYTES {
            return None;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut done = false;
        lex_braces(&chars, syntax, &mut mode, &mut |tok| {
            match tok {
                Tok::Str(c) => code.push(mask(c)),
                Tok::Code(c) => {
                    code.push(c);
                    match c {
                        '{' => {
                            braces += 1;
                            opened = true;
                        }
                        '}' => {
                            braces = braces.saturating_sub(1);
                            done = braces == 0 && opened && parens == 0;
                        }
                        '(' | '[' => parens += 1,
                        ')' | ']' => parens = parens.saturating_sub(1),
                        ';' => done = braces == 0 && parens == 0,
                        _ => {}
                    }
                }
            }
            !done
        });
        if done {
            return Some(Extent { end: start + offset, code });
        }
        close_stray_quote(&mut mode, line, syntax);
        code.push(if matches!(mode, Mode::Str { .. }) { mask('\n') } else { '\n' });
    }
    None
}

/// Each line with comments removed and the inside of strings blanked, so a line that begins with
/// `pub fn` really is code. Lines over the length cap come back empty.
pub(crate) fn masked_lines(lines: &[&str], syntax: Syntax) -> Vec<String> {
    let mut mode = Mode::Code;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.len() > MAX_LINE_BYTES {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut masked = String::with_capacity(line.len());
        lex_braces(&chars, syntax, &mut mode, &mut |tok| {
            match tok {
                Tok::Code(c) => masked.push(c),
                Tok::Str(_) => masked.push(' '),
            }
            true
        });
        close_stray_quote(&mut mode, line, syntax);
        out.push(masked);
    }
    out
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// Python-like: the item is its first (logical) line and every following line indented deeper,
/// blank lines and lines inside brackets or triple-quoted strings included.
fn extent_indent(lines: &[&str], start: usize) -> Option<Extent> {
    let base = indent_of(lines.get(start)?);
    let mut code = String::new();
    let mut end = start;
    let mut parens = 0usize;
    // Inside a triple-quoted string: the three quote characters that close it.
    let mut triple: Option<[char; 3]> = None;
    for (offset, line) in lines.get(start..)?.iter().take(MAX_ITEM_LINES).enumerate() {
        if line.len() > MAX_LINE_BYTES {
            return None;
        }
        if offset > 0 && parens == 0 && triple.is_none() {
            if line.trim().is_empty() {
                code.push('\n');
                continue;
            }
            if indent_of(line) <= base {
                break;
            }
        }
        end = start + offset;
        let chars: Vec<char> = line.chars().collect();
        let at = |k: usize| chars.get(k).copied();
        let mut i = 0;
        while let Some(c) = at(i) {
            if let Some(q) = triple {
                code.push(mask(c));
                if c == q[0] && at(i + 1) == Some(q[1]) && at(i + 2) == Some(q[2]) {
                    code.push(q[1]);
                    code.push(q[2]);
                    i += 3;
                    triple = None;
                } else {
                    i += if c == '\\' { 2 } else { 1 };
                }
                continue;
            }
            match c {
                '#' => break,
                '"' | '\'' if at(i + 1) == Some(c) && at(i + 2) == Some(c) => {
                    code.extend([c, c, c]);
                    i += 3;
                    triple = Some([c, c, c]);
                }
                '"' | '\'' => {
                    code.push(c);
                    i += 1;
                    while let Some(d) = at(i) {
                        code.push(mask(d));
                        i += 1;
                        if d == '\\' {
                            if let Some(e) = at(i) {
                                code.push(mask(e));
                            }
                            i += 1;
                        } else if d == c {
                            break;
                        }
                    }
                }
                '(' | '[' | '{' => {
                    parens += 1;
                    code.push(c);
                    i += 1;
                }
                ')' | ']' | '}' => {
                    parens = parens.saturating_sub(1);
                    code.push(c);
                    i += 1;
                }
                _ => {
                    code.push(c);
                    i += 1;
                }
            }
        }
        code.push(if triple.is_some() { mask('\n') } else { '\n' });
    }
    Some(Extent { end, code })
}

/// An item's name: its code up to the body, whitespace collapsed and shortened.
fn signature(code: &str, syntax: Syntax) -> String {
    let head = match syntax {
        Syntax::Indent => code.lines().next().unwrap_or(""),
        Syntax::Rust | Syntax::CLike => code.split(['{', ';']).next().unwrap_or(""),
    };
    let words: Vec<&str> = head.split_whitespace().collect();
    let joined = words.join(" ");
    joined
        .chars()
        .take(MAX_SIGNATURE_CHARS)
        .map(|c| match c {
            '\u{E000}' => ' ',
            '\u{E001}' => '\t',
            '\u{E002}' => '\n',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LinkKind;

    fn sym(text: &str, line: u32, path: &str) -> Option<Symbol> {
        let mut links =
            vec![CodeLink { feature: None, id: "REQ-1".into(), line, kind: LinkKind::Comment, symbol: None }];
        attach_symbols(text, Path::new(path), &mut links);
        links.remove(0).symbol
    }

    #[test]
    fn a_comment_above_a_function_attaches_to_it() {
        let text = "use x;\n// spec: REQ-1\n/// Docs.\n#[inline]\npub fn sign_in(user: &str) -> bool {\n    // not part of the code\n    check(user) // nor this\n}\n\nfn other() {}\n";
        let s = sym(text, 2, "a.rs").unwrap();
        assert_eq!(s.signature, "pub fn sign_in(user: &str) -> bool");
        assert_eq!((s.start_line, s.end_line), (5, 8));
        // Comments and layout do not change the fingerprint; code does.
        let relaid = "// spec: REQ-1\npub fn sign_in(user: &str) -> bool { /* x */ check(user) }\n";
        assert_eq!(sym(relaid, 1, "a.rs").unwrap().fingerprint, s.fingerprint);
        let changed = relaid.replace("check(user)", "check_all(user)");
        assert_ne!(sym(&changed, 1, "a.rs").unwrap().fingerprint, s.fingerprint);
        // Formatting is not a change; a word is, and so is the text of a string.
        let spaced = "// spec: REQ-1\npub fn sign_in( user : &str ) -> bool\n{\n    check( user )\n}\n";
        assert_eq!(sym(spaced, 1, "a.rs").unwrap().fingerprint, s.fingerprint);
        let quoted = |text: &str| format!("// spec: REQ-1\nfn f() {{ say(\"{text}\"); }}\n");
        let hello = sym(&quoted("hello  world"), 1, "a.rs").unwrap().fingerprint;
        assert_ne!(hello, sym(&quoted("hello world"), 1, "a.rs").unwrap().fingerprint);
        assert_ne!(hello, sym(&quoted("helloworld"), 1, "a.rs").unwrap().fingerprint);
        assert_ne!(
            sym("// spec: REQ-1\nfn ab() {}\n", 1, "a.rs").unwrap().fingerprint,
            sym("// spec: REQ-1\nfn a b() {}\n", 1, "a.rs").unwrap().fingerprint
        );
        let renamed = relaid.replace("sign_in", "log_in");
        assert_ne!(sym(&renamed, 1, "a.rs").unwrap().fingerprint, s.fingerprint);
    }

    #[test]
    fn an_attribute_link_skips_the_other_attributes_even_across_lines() {
        let text = "#[spec(\"REQ-1\")]\n#[cfg(\n    feature = \"x\"\n)]\n#[test]\nfn t() {\n    assert!(true);\n}\n";
        let s = sym(text, 1, "a.rs").unwrap();
        assert_eq!(s.signature, "fn t()");
        assert_eq!((s.start_line, s.end_line), (6, 8));
    }

    #[test]
    fn a_signature_with_a_string_shows_the_string_as_written() {
        let s = sym("# spec: REQ-1\ndef f(a=\"x y\"):\n    pass\n", 1, "a.py").unwrap();
        assert_eq!(s.signature, "def f(a=\"x y\"):");
    }

    #[test]
    fn compacting_keeps_only_the_whitespace_that_separates_words() {
        assert_eq!(compact("  pub  fn f ( a : u8 )\n -> u8 {\n x \n}"), "pub fn f(a:u8)->u8{x}");
        assert_eq!(compact("a\u{E000}\u{E000}b"), "a\u{E000}\u{E000}b");
        assert_eq!(compact(""), "");
    }

    #[test]
    fn a_trailing_comment_belongs_to_its_own_line() {
        let s = sym("fn a() { b(); } // spec: REQ-1\nfn c() {}\n", 1, "a.rs").unwrap();
        assert_eq!((s.start_line, s.end_line), (1, 1));
        assert_eq!(s.signature, "fn a()");
    }

    #[test]
    fn braces_in_strings_chars_comments_and_closures_do_not_end_the_item() {
        let text = "// spec: REQ-1\nfn f() {\n    let a = \"}\";\n    let b = '}';\n    let c = '\\'';\n    let d = r#\"}\"#;\n    // }\n    /* } /* } */ } */\n    let e = |x| { x };\n    run(|| { 1 });\n    let l: &'static str = \"x\";\n}\nfn after() {}\n";
        let s = sym(text, 1, "a.rs").unwrap();
        assert_eq!((s.start_line, s.end_line), (2, 12), "{s:?}");
    }

    #[test]
    fn items_without_a_body_end_at_the_semicolon() {
        assert_eq!(sym("// spec: REQ-1\nconst A: [u8; 3] = [1, 2, 3];\nfn x() {}\n", 1, "a.rs").unwrap().end_line, 2);
        assert_eq!(sym("// spec: REQ-1\nstruct Unit;\n", 1, "a.rs").unwrap().signature, "struct Unit");
        assert_eq!(sym("// spec: REQ-1\ntype A = Vec<u8>;\n", 1, "a.rs").unwrap().end_line, 2);
    }

    #[test]
    fn other_brace_languages_use_quotes_and_backticks() {
        let text = "// spec: REQ-1\nexport function f(a) {\n  const s = '{';\n  const t = `}${a}`;\n  return \"}\"; // }\n}\nfunction g() {}\n";
        let s = sym(text, 1, "a.ts").unwrap();
        assert_eq!((s.start_line, s.end_line), (2, 6), "{s:?}");
        // A stray apostrophe in a regex-like line does not swallow the rest of the file.
        let stray = "// spec: REQ-1\nfunction f() {\n  x = /'/;\n}\nfunction g() {}\n";
        assert_eq!(sym(stray, 1, "a.js").unwrap().end_line, 4);
    }

    #[test]
    fn python_blocks_follow_indentation() {
        let text = "# spec: REQ-1\n@decorator(\n    1,\n)\ndef f(a,\n      b):\n    \"\"\"Doc with\n# a hash and a dedented line\n\"\"\"\n    x = [\n  1,\n    ]  # comment\n\n    return x\n\ndef after():\n    pass\n";
        let s = sym(text, 1, "a.py").unwrap();
        assert_eq!((s.start_line, s.end_line), (5, 14), "{s:?}");
        assert_eq!(s.signature, "def f(a,");
        let one = "x = 1  # spec: REQ-1\ny = 2\n";
        assert_eq!(sym(one, 1, "a.py").unwrap().end_line, 1);
    }

    #[test]
    fn what_cannot_be_delimited_is_untracked() {
        assert!(sym("// spec: REQ-1\nfn f() {\n", 1, "a.rs").is_none(), "unterminated");
        assert!(sym("// spec: REQ-1\n", 1, "a.rs").is_none(), "nothing follows");
        assert!(sym("// spec: REQ-1\n\n\n", 1, "a.rs").is_none());
        let long = format!("// spec: REQ-1\nfn f() {{ {} }}\n", "x".repeat(MAX_LINE_BYTES + 10));
        assert!(sym(&long, 1, "a.rs").is_none(), "minified lines are not scanned");
        let comments = format!("// spec: REQ-1\n{}fn f() {{}}\n", "// c\n".repeat(MAX_SKIP_LINES + 5));
        assert!(sym(&comments, 1, "a.rs").is_none(), "too far from the item");
        assert!(sym("x", 0, "a.rs").is_none());
        assert!(sym("x", 99, "a.rs").is_none());
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for text in [
            "// spec: REQ-1\nfn \u{0}{ \"\n",
            "// spec: REQ-1\nr####\"\n\n",
            "// spec: REQ-1\n'\\",
            "// spec: REQ-1\n'\\u{",
            "// spec: REQ-1\n/* /* /*",
            "// spec: REQ-1\n}}}}))))]]]]",
            "// spec: REQ-1\n\u{1F600}'\u{1F600}'{\u{1F600}",
            "// spec: REQ-1\n\"\"\"\n",
            "// spec: REQ-1\n(((((",
        ] {
            for path in ["a.rs", "a.py", "a.c"] {
                let _ = sym(text, 1, path);
            }
        }
        let deep = format!("// spec: REQ-1\nfn f() {{\n{}}}\n", "{\n".repeat(5000));
        let _ = sym(&deep, 1, "a.rs");
    }

    #[test]
    fn several_references_on_a_line_share_one_item_and_the_work_is_bounded() {
        let mut links: Vec<CodeLink> = (0..3)
            .map(|n| CodeLink { feature: None, id: format!("REQ-{n}"), line: 1, kind: LinkKind::Comment, symbol: None })
            .collect();
        attach_symbols("// spec: REQ-0, REQ-1, REQ-2\nfn f() {}\n", Path::new("a.rs"), &mut links);
        assert!(links.iter().all(|l| l.symbol == links[0].symbol && l.symbol.is_some()));
        // References whose items cannot be delimited stop being tried once the budget is spent.
        let attempt = |repeats: usize| {
            let text = format!("{}// spec: REQ-1\nfn ok() {{}}\n", "// spec: REQ-1\nx = {\n".repeat(repeats));
            let mut all: Vec<CodeLink> = (0..=repeats)
                .map(|n| CodeLink {
                    feature: None,
                    id: "REQ-1".into(),
                    line: u32::try_from(n * 2 + 1).unwrap(),
                    kind: LinkKind::Comment,
                    symbol: None,
                })
                .collect();
            attach_symbols(&text, Path::new("a.rs"), &mut all);
            all.pop().and_then(|l| l.symbol)
        };
        assert!(attempt(10).is_some());
        assert!(attempt(1500).is_none());
    }
}
