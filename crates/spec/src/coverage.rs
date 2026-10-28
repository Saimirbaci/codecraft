//! "Unspecified code": the public items of a file that no requirement covers.
//!
//! It is opt-in per file: a file that carries `// spec: required` (any comment marker) asks for
//! its public API to be traced to requirements. Its public items (Rust `pub fn`, `pub struct`, …;
//! Python top-level `def` and `class` without a leading underscore; TypeScript and JavaScript
//! `export`s; Go capitalised `func` and `type`) are listed unless a reference covers them: one
//! attached to the item (the `// spec:` line above it), one inside it, or one attached to an
//! item that contains it (an `impl` block, a class). Rust code under `#[cfg(test)]` is not API.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::code::{Syntax, extent, item_start, masked_lines};
use crate::links::comment_markers;
use crate::markdown::line_no;
use crate::model::CodeLink;
use crate::{MAX_ITEM_LINES, MAX_LINE_BYTES, MAX_SYMBOL_WORK, MAX_UNSPECIFIED};

/// A public item of a file that requires specs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicItem {
    /// 1-based line of the item (after its attributes).
    pub line: u32,
    pub end_line: u32,
    /// `fn`, `struct`, `class`, `function`, …
    pub kind: String,
    pub name: String,
}

/// The file asks for its public items to be specified: `// spec: required`.
pub fn requires_spec(text: &str) -> bool {
    crate::links::mentions_spec(text)
        && text.lines().filter(|l| l.len() <= MAX_LINE_BYTES).any(|line| {
            comment_markers(line).into_iter().any(|after| {
                let word = after.trim_start().to_ascii_lowercase();
                word.strip_prefix("required")
                    .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
            })
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    /// JavaScript and TypeScript.
    Script,
    Go,
    /// Not recognised: nothing is listed.
    Other,
}

impl Lang {
    fn of(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Self::Script,
            "go" => Self::Go,
            _ => Self::Other,
        }
    }

    fn syntax(self) -> Syntax {
        match self {
            Self::Rust => Syntax::Rust,
            Self::Python => Syntax::Indent,
            Self::Script | Self::Go | Self::Other => Syntax::CLike,
        }
    }
}

/// The public items of the file `text` that none of `links` (its references) covers.
pub fn unspecified_items(text: &str, path: &Path, links: &[CodeLink]) -> Vec<PublicItem> {
    let lang = Lang::of(path);
    if lang == Lang::Other {
        return Vec::new();
    }
    let syntax = lang.syntax();
    let lines: Vec<&str> = text.lines().collect();
    let masked = if lang == Lang::Python { python_masked(&lines) } else { masked_lines(&lines, syntax) };
    let tests = if lang == Lang::Rust { test_ranges(&lines, &masked) } else { Vec::new() };
    // Where each reference is attached, and the items that contain a reference's item.
    let attached: Vec<usize> = links
        .iter()
        .filter_map(|l| item_start(&lines, usize::try_from(l.line.saturating_sub(1)).ok()?, syntax))
        .collect();
    let covering: Vec<(u32, u32)> =
        links.iter().filter_map(|l| l.symbol.as_ref()).map(|s| (s.start_line, s.end_line)).collect();

    let mut out = Vec::new();
    let mut work = 0usize;
    for (i, m) in masked.iter().enumerate() {
        if tests.iter().any(|(from, to)| (*from..=*to).contains(&i)) {
            continue;
        }
        let Some((kind, name)) = public_item(m, lang) else { continue };
        let end = if work > MAX_SYMBOL_WORK {
            i
        } else {
            match extent(&lines, i, syntax) {
                Some(e) => {
                    work = work.saturating_add(e.end.saturating_sub(i).saturating_add(1));
                    e.end
                }
                None => {
                    work = work.saturating_add(MAX_ITEM_LINES.min(lines.len().saturating_sub(i)));
                    i
                }
            }
        };
        let (start_line, end_line) = (line_no(i), line_no(end));
        let covered = attached.contains(&i)
            || links.iter().any(|l| start_line <= l.line && l.line <= end_line)
            || covering.iter().any(|(from, to)| *from <= start_line && start_line <= *to);
        if !covered {
            out.push(PublicItem { line: start_line, end_line, kind, name });
            if out.len() >= MAX_UNSPECIFIED {
                break;
            }
        }
    }
    out
}

/// Line ranges (0-based, inclusive) of Rust items under `#[cfg(test)]` or `#[test]`.
fn test_ranges(lines: &[&str], masked: &[String]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, m) in masked.iter().enumerate() {
        let t = m.trim_start();
        if !(t.starts_with("#[cfg(test)]")
            || t.starts_with("#[test]")
            || (t.starts_with("#[") && t.contains("cfg(test)")))
        {
            continue;
        }
        if let Some(start) = item_start(lines, i, Syntax::Rust)
            && let Some(e) = extent(lines, start, Syntax::Rust)
        {
            out.push((i, e.end));
        }
    }
    out
}

/// Python lines with the inside of triple-quoted strings emptied.
fn python_masked(lines: &[&str]) -> Vec<String> {
    let mut inside: Option<&str> = None;
    lines
        .iter()
        .map(|line| {
            let was_inside = inside.is_some();
            for quote in ["\"\"\"", "'''"] {
                let n = line.matches(quote).count();
                match inside {
                    Some(q) if q == quote && n % 2 == 1 => inside = None,
                    None if n % 2 == 1 => inside = Some(quote),
                    _ => {}
                }
            }
            if was_inside { String::new() } else { (*line).to_string() }
        })
        .collect()
}

/// The identifier at the start of `s` (letters, digits, `_`, `$`).
fn ident(s: &str) -> &str {
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(s.len());
    s.get(..end).unwrap_or_default()
}

/// `(kind, name)` when the (masked) line starts a public item of the language.
fn public_item(line: &str, lang: Lang) -> Option<(String, String)> {
    match lang {
        Lang::Rust => rust_item(line),
        Lang::Python => python_item(line),
        Lang::Script => script_item(line),
        Lang::Go => go_item(line),
        Lang::Other => None,
    }
}

fn rust_item(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix("pub")?;
    // `pub(crate)` is not public API; `pub` must stand alone.
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut i = 0;
    while let Some(t) = tokens.get(i) {
        let next_is_fn = tokens.get(i + 1).is_some_and(|n| n.starts_with("fn"));
        match *t {
            "async" | "unsafe" | "default" | "extern" | "\"" => i += 1,
            "const" if next_is_fn => i += 1,
            "fn" | "struct" | "enum" | "trait" | "union" | "type" | "const" | "static" => {
                // `pub static mut NAME`
                let at = if tokens.get(i + 1) == Some(&"mut") { i + 2 } else { i + 1 };
                let name = ident(tokens.get(at)?);
                return (!name.is_empty()).then(|| ((*t).to_string(), name.to_string()));
            }
            _ => return None,
        }
    }
    None
}

fn python_item(line: &str) -> Option<(String, String)> {
    // Only module-level API: no indentation.
    let (kind, rest) = line
        .strip_prefix("def ")
        .or_else(|| line.strip_prefix("async def "))
        .map(|r| ("def", r))
        .or_else(|| line.strip_prefix("class ").map(|r| ("class", r)))?;
    let name = ident(rest.trim_start());
    (!name.is_empty() && !name.starts_with('_')).then(|| (kind.to_string(), name.to_string()))
}

fn script_item(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix("export")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut i = 0;
    let mut default = false;
    while let Some(t) = tokens.get(i) {
        let next_is_enum = tokens.get(i + 1).is_some_and(|n| *n == "enum");
        match *t {
            "default" => {
                default = true;
                i += 1;
            }
            "declare" | "abstract" | "async" => i += 1,
            "const" if next_is_enum => i += 1,
            "function" | "function*" | "class" | "interface" | "type" | "enum" | "const" | "let" | "var" => {
                let kind = t.trim_end_matches('*');
                let name = tokens.get(i + 1).map_or("", |n| ident(n.trim_start_matches('*')));
                let name = if name.is_empty() && default { "default" } else { name };
                return (!name.is_empty()).then(|| (kind.to_string(), name.to_string()));
            }
            _ => return None,
        }
    }
    None
}

fn go_item(line: &str) -> Option<(String, String)> {
    let (kind, rest) = line
        .strip_prefix("func ")
        .map(|r| ("func", r.trim_start()))
        .or_else(|| line.strip_prefix("type ").map(|r| ("type", r.trim_start())))?;
    // A method: skip the receiver `(r *T)`.
    let rest = if kind == "func" && rest.starts_with('(') {
        rest.split_once(')').map_or("", |(_, after)| after.trim_start())
    } else {
        rest
    };
    let name = ident(rest);
    name.starts_with(char::is_uppercase).then(|| (kind.to_string(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::scan_links;

    fn unspecified(text: &str, path: &str) -> Vec<String> {
        let mut links = scan_links(text);
        crate::code::attach_symbols(text, Path::new(path), &mut links);
        unspecified_items(text, Path::new(path), &links)
            .into_iter()
            .map(|i| format!("{} {}@{}", i.kind, i.name, i.line))
            .collect()
    }

    #[test]
    fn the_marker_is_a_comment_saying_required() {
        assert!(requires_spec("// spec: required\nfn a() {}"));
        assert!(requires_spec("x\n#  SPEC:   Required  \n"));
        assert!(requires_spec("/* spec: required */"));
        assert!(!requires_spec("// spec: requirements follow"));
        assert!(!requires_spec("let spec: required = 1;"), "not in a comment");
        assert!(!requires_spec("// spec: REQ-1"));
        assert!(!requires_spec(""));
        assert!(
            !requires_spec(&format!("// spec: required{}", " ".repeat(MAX_LINE_BYTES))),
            "overlong lines are skipped"
        );
    }

    #[test]
    fn rust_public_items_without_a_reference_are_listed() {
        let text = "// spec: required\n\n// spec: REQ-1\npub fn covered() {}\n\npub fn bare() {}\n\npub(crate) fn internal() {}\nfn private() {}\n\n/// Docs\n#[derive(Debug)]\npub struct Shape { pub x: u8 }\n\npub async fn later() {}\npub const fn konst() {}\npub const LIMIT: u8 = 1;\npub use other::Thing;\npub mod inner;\npub unsafe extern \"C\" fn ffi() {}\npub enum E {}\npub trait T {}\npub type Alias = u8;\npub static mut COUNT: u8 = 0;\n";
        assert_eq!(
            unspecified(text, "a.rs"),
            [
                "fn bare@6",
                "struct Shape@13",
                "fn later@15",
                "fn konst@16",
                "const LIMIT@17",
                "fn ffi@20",
                "enum E@21",
                "trait T@22",
                "type Alias@23",
                "static COUNT@24",
            ]
        );
    }

    #[test]
    fn a_reference_covers_its_item_what_is_inside_and_what_it_contains() {
        let text = "// spec: required\n// spec: REQ-1\nimpl Thing {\n    pub fn method(&self) {}\n    pub fn other(&self) {}\n}\n\npub fn with_marker_inside() {\n    // spec: REQ-2\n    work();\n}\n\n#[spec(\"REQ-3\")]\n#[inline]\npub fn attributed() {}\n\npub fn lonely() {}\n";
        assert_eq!(unspecified(text, "a.rs"), ["fn lonely@17"]);
    }

    #[test]
    fn test_code_and_text_that_only_looks_like_code_are_not_api() {
        let text = "// spec: required\n#[cfg(test)]\nmod tests {\n    pub fn helper() {}\n}\n\n#[test]\npub fn t() {}\n\nconst SAMPLE: &str = \"\npub fn in_a_string() {}\n\";\n// pub fn in_a_comment() {}\n/*\npub fn in_a_block() {}\n*/\npub fn real() {}\n";
        assert_eq!(unspecified(text, "a.rs"), ["fn real@17"]);
    }

    #[test]
    fn python_lists_module_level_public_defs_and_classes() {
        let text = "# spec: required\nimport os\n\n# spec: REQ-1\ndef covered():\n    pass\n\ndef bare():\n    \"\"\"\ndef in_docstring():\n    \"\"\"\n\ndef _private():\n    pass\n\nasync def later():\n    pass\n\nclass Widget:\n    def method(self):\n        pass\n";
        assert_eq!(unspecified(text, "a.py"), ["def bare@8", "def later@16", "class Widget@19"]);
    }

    #[test]
    fn typescript_and_go() {
        let ts = "// spec: required\nexport function a() {}\n// spec: REQ-1\nexport const covered = 1;\nexport default class Panel {}\nexport async function* gen() {}\nexport const enum Mode { A }\nexport interface Props {}\nexport type Id = string;\nexport { a, b };\nexport * from './x';\nfunction notExported() {}\nexport default function () {}\n";
        assert_eq!(
            unspecified(ts, "a.ts"),
            [
                "function a@2",
                "class Panel@5",
                "function gen@6",
                "enum Mode@7",
                "interface Props@8",
                "type Id@9",
                "function default@13"
            ]
        );
        let go = "// spec: required\npackage x\n\n// spec: REQ-1\nfunc Covered() {}\n\nfunc Open() {}\nfunc closed() {}\nfunc (s *Server) Handle() {}\nfunc (s *Server) hidden() {}\ntype Config struct{}\ntype internal struct{}\n";
        assert_eq!(unspecified(go, "a.go"), ["func Open@7", "func Handle@9", "type Config@11"]);
    }

    #[test]
    fn unknown_languages_list_nothing_and_hostile_input_does_not_panic() {
        assert!(unspecified("// spec: required\npub fn a() {}\n", "a.c").is_empty());
        for text in [
            "pub",
            "pub ",
            "pub fn",
            "pub fn \u{0}",
            "export",
            "export default",
            "func (",
            "func (a",
            "def ",
            "class",
            "\"\"\"",
            "#[cfg(test)]",
            "#[test]\n",
            "pub fn a() {\n",
        ] {
            for path in ["a.rs", "a.py", "a.ts", "a.go"] {
                let _ = unspecified(text, path);
            }
        }
        let many = "pub fn f() {}\n".repeat(MAX_UNSPECIFIED + 50);
        assert_eq!(unspecified(&many, "a.rs").len(), MAX_UNSPECIFIED);
    }
}
