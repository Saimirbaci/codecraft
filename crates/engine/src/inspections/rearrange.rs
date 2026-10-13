//! Code › Rearrange Code for Rust: sort a file's top-level items by kind.
//!
//! The rule: `use` and `extern crate` first, then module declarations (`mod x;`),
//! `macro_rules!`, constants, statics, types (`struct`, `enum`, `union`, `type`), traits, impls,
//! functions (and `extern` blocks), and inline modules (`mod tests { … }`) last. Items keep their
//! relative order within a kind (a stable sort), their attributes and the comments directly above
//! them. The file header (inner attributes, `//!` docs, a leading comment block) stays on top.
//! Items inside `impl` and `mod` bodies are not reordered.
//!
//! Files with item-level macro invocations (`thread_local! { … }`, `bitflags! { … }`) or with two
//! items on one line are refused: moving them could change meaning or formatting.

use std::sync::OnceLock;

use regex::Regex;

use codecraft_syntax::{HighlightKind, Language};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    Use,
    ModDecl,
    MacroRules,
    Const,
    Static,
    Type,
    Trait,
    Impl,
    Fn,
    InlineMod,
}

struct Chunk {
    /// Lines of the item (leading blank lines removed).
    lines: Vec<String>,
    /// Blank lines that preceded it.
    blanks: usize,
    category: Category,
}

fn head_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:default|async|const|unsafe|auto|extern(?:\s+"[^"]*")?)\s+)*(use|extern\s+crate|mod|macro_rules!|const|static|struct|enum|union|type|trait|impl|fn|extern)\b"#,
        )
        .ok()
    })
    .as_ref()
}

/// The category of an item from its first code line.
fn classify(first: &str, has_body: bool) -> Option<Category> {
    let c = head_re()?.captures(first)?;
    let kw = c.get(1)?.as_str();
    let qualifiers = first.get(..c.get(1)?.start()).unwrap_or("");
    Some(match kw {
        "use" => Category::Use,
        k if k.starts_with("extern") && k.contains("crate") => Category::Use,
        "mod" if has_body => Category::InlineMod,
        "mod" => Category::ModDecl,
        "macro_rules!" => Category::MacroRules,
        // `const fn` is caught by `fn`; a bare `const` here is a constant.
        "const" => Category::Const,
        "static" => Category::Static,
        "struct" | "enum" | "union" | "type" => Category::Type,
        "trait" => Category::Trait,
        "impl" => Category::Impl,
        "fn" => Category::Fn,
        "extern" if !qualifiers.contains("fn") => Category::Fn,
        _ => return None,
    })
}

/// Rearrange the top-level items of a Rust file. `Ok(None)` when it is already arranged.
pub fn rearrange(text: &str) -> Result<Option<String>, String> {
    let hl = codecraft_syntax::highlight(Language::Rust, text);
    let lines: Vec<&str> = text.split('\n').collect();
    // Code chars per line: comments, strings, chars and attributes are masked out.
    let is_code = |line: usize, col: usize| {
        !hl.line(line).iter().any(|s| {
            s.start <= col
                && col < s.end
                && matches!(
                    s.kind,
                    HighlightKind::Comment
                        | HighlightKind::DocComment
                        | HighlightKind::String
                        | HighlightKind::Char
                        | HighlightKind::Attribute
                )
        })
    };
    let line_has_code = |line: usize| {
        lines.get(line).is_some_and(|l| l.chars().enumerate().any(|(i, c)| !c.is_whitespace() && is_code(line, i)))
    };

    // Item ends: the line of a `;` or closing `}` at depth 0.
    let mut ends: Vec<(usize, bool)> = Vec::new(); // (line, ended with a brace body)
    let (mut braces, mut parens) = (0i64, 0i64);
    let mut item_started = false;
    let mut first_code: Option<String> = None;
    for (ln, l) in lines.iter().enumerate() {
        for (col, ch) in l.chars().enumerate() {
            if ch.is_whitespace() || !is_code(ln, col) {
                continue;
            }
            if !item_started {
                item_started = true;
                first_code = Some(l.to_string());
            }
            match ch {
                '{' => braces += 1,
                '}' => {
                    braces -= 1;
                    if braces < 0 {
                        return Err(format!("unbalanced `}}` on line {}", ln + 1));
                    }
                    let semicolon_item = first_code.as_deref().and_then(|f| classify(f, false)).is_some_and(|c| {
                        matches!(c, Category::Use | Category::Const | Category::Static)
                            || (c == Category::Type && is_type_alias(first_code.as_deref()))
                    });
                    if braces == 0 && parens == 0 && !semicolon_item {
                        if l.chars().enumerate().skip(col + 1).any(|(i, c)| !c.is_whitespace() && is_code(ln, i) && c != ';') {
                            return Err(format!("line {} has more than one item; not rearranged", ln + 1));
                        }
                        ends.push((ln, true));
                        item_started = false;
                        first_code = None;
                        break;
                    }
                }
                '(' | '[' => parens += 1,
                ')' | ']' => parens -= 1,
                ';' if braces == 0 && parens == 0 => {
                    if l.chars().enumerate().skip(col + 1).any(|(i, c)| !c.is_whitespace() && is_code(ln, i)) {
                        return Err(format!("line {} has more than one item; not rearranged", ln + 1));
                    }
                    ends.push((ln, false));
                    item_started = false;
                    first_code = None;
                    break;
                }
                _ => {}
            }
        }
    }
    if braces != 0 || parens != 0 || item_started {
        return Err("the file does not parse (unbalanced brackets or an unfinished item)".into());
    }
    if ends.len() < 2 {
        return Ok(None);
    }

    // The header: inner attributes and `//!` docs, plus a leading comment block ended by a blank line.
    let mut header_end: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        let inner = t.starts_with("//!") || t.starts_with("#![");
        if line_has_code(i) && !t.starts_with("#![") {
            break;
        }
        if t.starts_with("///") || (t.starts_with("#[") && !t.starts_with("#![")) {
            break;
        }
        if inner || t.is_empty() {
            header_end = Some(i);
        }
    }
    let first_item_line = header_end.map_or(0, |h| h + 1);

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut start = first_item_line;
    for (end, body) in &ends {
        if *end < start {
            return Err("an item overlaps the file header; not rearranged".into());
        }
        let slice: Vec<&str> = lines.get(start..=*end).map(<[&str]>::to_vec).unwrap_or_default();
        let blanks = slice.iter().take_while(|l| l.trim().is_empty()).count();
        let item_lines: Vec<String> = slice.iter().skip(blanks).map(|s| s.to_string()).collect();
        // The first code line (skipping comments and attributes) decides the category.
        let first = (start..=*end)
            .skip(blanks)
            .find(|&i| {
                let t = lines.get(i).map(|l| l.trim()).unwrap_or("");
                line_has_code(i) && !t.starts_with("#[")
            })
            .and_then(|i| lines.get(i))
            .copied()
            .unwrap_or("");
        let category = classify(first, *body).ok_or_else(|| {
            let name = first.trim().chars().take(40).collect::<String>();
            format!("cannot rearrange around `{name}` (macro invocations and unknown items stay put)")
        })?;
        chunks.push(Chunk { lines: item_lines, blanks, category });
        start = end + 1;
    }
    let footer: Vec<&str> = lines.get(start..).map(<[&str]>::to_vec).unwrap_or_default();

    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by_key(|&i| chunks.get(i).map(|c| c.category));
    if order.iter().enumerate().all(|(i, &j)| i == j) {
        return Ok(None);
    }
    let mut out: Vec<String> = lines.get(..first_item_line).unwrap_or(&[]).iter().map(|s| s.to_string()).collect();
    let mut prev: Option<Category> = None;
    for &i in &order {
        let Some(c) = chunks.get(i) else { continue };
        let blanks = match prev {
            None => {
                let header_ends_blank = out.last().is_none_or(|l| l.trim().is_empty());
                if header_ends_blank { 0 } else { 1 }
            }
            Some(p) if p == c.category => c.blanks.min(2),
            Some(_) => c.blanks.clamp(1, 2),
        };
        out.extend(std::iter::repeat_n(String::new(), blanks));
        out.extend(c.lines.iter().cloned());
        prev = Some(c.category);
    }
    out.extend(footer.iter().map(|s| s.to_string()));
    let result = out.join("\n");
    // Sanity: the same non-blank lines, only moved.
    let mut a: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
    let mut b: Vec<&str> = result.split('\n').filter(|l| !l.trim().is_empty()).collect();
    a.sort_unstable();
    b.sort_unstable();
    if a != b {
        return Err("internal error: rearranging would change the text".into());
    }
    Ok((result != text).then_some(result))
}

/// `type X = …;` (ends at `;`), as opposed to types with bodies.
fn is_type_alias(first: Option<&str>) -> bool {
    first.and_then(|f| head_re()?.captures(f)?.get(1).map(|m| m.as_str() == "type")).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn sorts_items_by_kind() {
        let src = "//! Crate docs.\n#![allow(dead_code)]\n\nfn main() {\n    let s = \"}\"; // }\n}\n\nimpl Foo {\n    fn new() -> Self { Foo }\n}\n\n/// A thing.\n#[derive(Debug)]\nstruct Foo;\nconst A: [u8; 2] = [1, 2];\nuse std::fmt;\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n\nstatic S: Foo = Foo {};\ntrait T {}\nuse std::io;\nmod other;\ntype Alias = u32;\n";
        let out = rearrange(src).unwrap().unwrap();
        assert_eq!(
            out,
            "//! Crate docs.\n#![allow(dead_code)]\n\nuse std::fmt;\nuse std::io;\n\nmod other;\n\nconst A: [u8; 2] = [1, 2];\n\nstatic S: Foo = Foo {};\n\n/// A thing.\n#[derive(Debug)]\nstruct Foo;\ntype Alias = u32;\n\ntrait T {}\n\nimpl Foo {\n    fn new() -> Self { Foo }\n}\n\nfn main() {\n    let s = \"}\"; // }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n"
        );
        // Idempotent.
        assert_eq!(rearrange(&out).unwrap(), None);
    }

    #[test]
    fn already_arranged_and_trivial_files() {
        assert_eq!(rearrange("use a;\n\nfn main() {}\n").unwrap(), None);
        assert_eq!(rearrange("").unwrap(), None);
        assert_eq!(rearrange("fn main() {}\n").unwrap(), None);
    }

    #[test]
    fn refuses_what_it_cannot_move_safely() {
        assert!(rearrange("fn a() {}\nthread_local! { static X: u8 = 0; }\nuse b;\n").is_err());
        assert!(rearrange("fn a() {} fn b() {}\nuse c;\n").is_err());
        assert!(rearrange("fn a() {\n").is_err());
        assert!(rearrange("fn a() {}\n}\n").is_err());
        assert!(rearrange("fn a() { /* { */ }\nuse b;\n").unwrap().is_some());
    }

    #[test]
    fn pub_qualifiers_and_const_fn() {
        let src = "pub(crate) const fn f() -> u8 { 1 }\npub const X: u8 = 1;\npub unsafe impl Send for A {}\npub(super) struct A;\n";
        let out = rearrange(src).unwrap().unwrap();
        assert_eq!(out, "pub const X: u8 = 1;\n\npub(super) struct A;\n\npub unsafe impl Send for A {}\n\npub(crate) const fn f() -> u8 { 1 }\n");
    }
}
