//! CodeCraft lexical syntax services (L0, no workspace deps).
//!
//! Language detection, fast lexical highlighting (Rust via `rustc_lexer`, plus TOML, Markdown,
//! JSON, YAML and shell), bracket matching, folding ranges and a lexical outline used before the
//! language server is ready. Semantic highlighting from the LSP layers on top in the UI.
//!
//! Every function accepts arbitrary text and never panics. Offsets are **char** offsets;
//! highlight spans are char columns within a line. Lines are split on `\n` exactly, so a text
//! with `k` newlines has `k + 1` lines (a `\r` before the `\n` is part of the line).
//!
//! Signatures are contract-stable: add, don't rename.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::string_slice
)]

mod common;
mod json;
mod markdown;
mod rust;
mod shell;
mod toml;
mod yaml;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::common::{Doc, Span};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Toml,
    Markdown,
    Json,
    Yaml,
    Shell,
    C,
    Cpp,
    Python,
    Zig,
    TypeScript,
    JavaScript,
    Sql,
    #[default]
    PlainText,
}

impl Language {
    /// By file name/extension: `.rs`, `Cargo.toml`/`.toml`, `.md`, `.json`, `.yml`/`.yaml`, `.sh`.
    pub fn from_path(path: &Path) -> Self {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Self::Rust,
            "toml" => Self::Toml,
            "md" | "markdown" => Self::Markdown,
            "json" => Self::Json,
            "yml" | "yaml" => Self::Yaml,
            "sh" | "bash" | "zsh" => Self::Shell,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "tpp" | "inl" | "ixx" | "cppm" => {
                Self::Cpp
            }
            "py" | "pyi" | "pyw" => Self::Python,
            "zig" | "zon" => Self::Zig,
            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "sql" | "ddl" | "dml" | "psql" | "pgsql" => Self::Sql,
            _ => match path.file_name().and_then(|n| n.to_str()) {
                Some("Cargo.lock") => Self::Toml,
                Some("BUILD" | "WORKSPACE" | "SConstruct" | "SConscript") => Self::Python,
                _ => Self::PlainText,
            },
        }
    }

    /// Display name for the status bar ("Rust", "TOML", …).
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Toml => "TOML",
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Shell => "Shell Script",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Python => "Python",
            Self::Zig => "Zig",
            Self::TypeScript => "TypeScript",
            Self::JavaScript => "JavaScript",
            Self::Sql => "SQL",
            Self::PlainText => "Plain text",
        }
    }

    /// LSP `languageId`.
    pub fn lsp_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Toml => "toml",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Shell => "shellscript",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Python => "python",
            Self::Zig => "zig",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Sql => "sql",
            Self::PlainText => "plaintext",
        }
    }

    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Rust | Self::Json | Self::C | Self::Cpp | Self::Zig | Self::TypeScript | Self::JavaScript => {
                Some("//")
            }
            Self::Toml | Self::Yaml | Self::Shell | Self::Python => Some("#"),
            Self::Sql => Some("--"),
            Self::Markdown | Self::PlainText => None,
        }
    }

    pub fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Rust | Self::C | Self::Cpp | Self::TypeScript | Self::JavaScript | Self::Sql => Some(("/*", "*/")),
            Self::Markdown => Some(("<!--", "-->")),
            _ => None,
        }
    }
}

/// Highlight classes. The UI maps each to a theme colour (RustRover's Dark scheme by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HighlightKind {
    #[default]
    Plain,
    Keyword,
    Ident,
    /// Function or method name at a call (`foo(`) or declaration (`fn foo`).
    Function,
    /// Macro invocation (`println!`) or `macro_rules! name`.
    Macro,
    /// Type-like name: capitalized identifier, primitive type, or after `struct`/`enum`/`trait`/`type`.
    Type,
    Lifetime,
    /// `#[...]` / `#![...]` attribute, including its contents.
    Attribute,
    String,
    Char,
    Number,
    Boolean,
    Comment,
    /// `///`, `//!`, `/** */`, `/*! */`.
    DocComment,
    Operator,
    Punctuation,
    Bracket,
    /// SCREAMING_CASE identifier.
    Constant,
    /// `self` / `Self` / `crate` / `super`.
    SelfKeyword,
    /// Identifier after `.` that is not a call.
    Field,
    /// TOML/YAML/JSON key.
    Key,
    /// TOML `[table]` header, Markdown heading.
    Heading,
    /// Markdown emphasis.
    Emphasis,
    /// Markdown inline code / code fence.
    Code,
    /// Markdown link.
    Link,
    /// Unterminated literal or unknown token.
    Invalid,
}

/// A span within one line, in char columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
}

/// Per-line highlight spans for a whole text. Lines are split on `\n`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Highlights {
    lines: Vec<Vec<LineSpan>>,
}

impl Highlights {
    pub fn from_lines(lines: Vec<Vec<LineSpan>>) -> Self {
        Self { lines }
    }

    /// Spans of `line`, sorted and non-overlapping; empty for out-of-range lines.
    pub fn line(&self, line: usize) -> &[LineSpan] {
        self.lines.get(line).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn len_lines(&self) -> usize {
        self.lines.len()
    }
}

/// Highlight the whole text. Must be fast (lexing a 10k-line file well under 10 ms in release).
///
/// Returns one entry per line (`\n` count + 1). Spans are sorted, non-overlapping, never
/// `Plain`, and touching spans of the same kind are merged; multi-line tokens (block comments,
/// raw strings, fenced code) are split per line.
pub fn highlight(language: Language, text: &str) -> Highlights {
    match language {
        Language::Rust => rust::highlight(text),
        Language::PlainText => {
            let lines = text.bytes().filter(|&b| b == b'\n').count() + 1;
            Highlights::from_lines(vec![Vec::new(); lines])
        }
        _ => {
            let doc = Doc::new(text);
            common::to_highlights(&doc.index, &lexical_spans(language, &doc))
        }
    }
}

/// Document-level spans for the hand-written lexers.
fn lexical_spans(language: Language, doc: &Doc) -> Vec<Span> {
    match language {
        Language::Toml => toml::analyze(doc).spans,
        Language::Markdown => markdown::analyze(doc).spans,
        Language::Json => json::spans(doc),
        Language::Yaml => yaml::spans(doc),
        Language::Shell => shell::spans(doc),
        // TODO(syntax agent): C, C++, Python, Zig, TypeScript, JavaScript, SQL lexers.
        Language::C
        | Language::Cpp
        | Language::Python
        | Language::Zig
        | Language::TypeScript
        | Language::JavaScript
        | Language::Sql
        | Language::Rust
        | Language::PlainText => Vec::new(),
    }
}

/// If `offset` is on or just after a bracket (`()[]{}`, and `<>` is NOT matched), the char offset
/// of its partner, skipping brackets inside strings and comments.
///
/// The bracket at `offset` is tried first, then the one before it. Matching respects nesting
/// across all three bracket kinds: `None` when the brackets in between are unbalanced.
pub fn matching_bracket(language: Language, text: &str, offset: usize) -> Option<usize> {
    let brackets = match language {
        Language::Rust => rust::brackets(text),
        _ => {
            let doc = Doc::new(text);
            common::code_brackets(&doc.chars, &lexical_spans(language, &doc))
        }
    };
    common::match_bracket(&brackets, offset)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FoldKind {
    /// A `{ … }` / `[ … ]` / `( … )` block spanning lines.
    Block,
    /// A run of consecutive line comments, or a multi-line block comment.
    Comment,
    /// A run of consecutive `use` declarations.
    Imports,
}

/// A foldable region: lines `start_line..=end_line`; folding hides lines after `start_line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
    pub kind: FoldKind,
}

/// Fold regions, sorted by start line; only regions spanning at least two lines.
///
/// Ranges with the same start line are ordered outermost first; exact duplicates are dropped.
/// Rust: bracket blocks, comment runs and block comments, `use` runs. JSON: bracket blocks.
/// TOML: tables, multi-line arrays, comment runs. Markdown: sections by heading, fenced code,
/// HTML comments. YAML: indentation blocks, comment runs. Shell: brace/paren blocks,
/// `if`/`case`/`do` blocks, comment runs.
pub fn folding_ranges(language: Language, text: &str) -> Vec<FoldRange> {
    match language {
        Language::Rust => rust::folding_ranges(text),
        Language::Toml => toml::folding_ranges(&Doc::new(text)),
        Language::Markdown => markdown::folding_ranges(&Doc::new(text)),
        Language::Json => json::folding_ranges(&Doc::new(text)),
        Language::Yaml => yaml::folding_ranges(&Doc::new(text)),
        Language::Shell => shell::folding_ranges(&Doc::new(text)),
        Language::C
        | Language::Cpp
        | Language::Python
        | Language::Zig
        | Language::TypeScript
        | Language::JavaScript
        | Language::Sql
        | Language::PlainText => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Static,
    TypeAlias,
    Macro,
    /// TOML table / Markdown heading.
    Section,
}

/// One entry of the lexical file structure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItem {
    pub name: String,
    pub kind: SymbolKind,
    /// Zero-based line of the item's name.
    pub line: usize,
    /// Char column of the name within the line.
    pub column: usize,
    /// Nesting depth (items inside `impl`/`mod`/`trait` bodies are deeper).
    pub depth: usize,
}

/// Lexical outline for File Structure (Cmd+F12) when no language server is available.
/// Rust: fn/struct/enum/trait/impl (named `impl Trait for Type`)/mod/const/static/type/macro_rules.
/// TOML: tables. Markdown: headings.
///
/// Items come in document order. Rust items inside fn bodies are not listed; `impl` items are
/// positioned at the `impl` keyword. TOML tables and Markdown headings are `Section`s; a
/// heading's depth is its level minus one.
pub fn outline(language: Language, text: &str) -> Vec<OutlineItem> {
    match language {
        Language::Rust => rust::outline(text),
        Language::Toml => toml::outline(&Doc::new(text)),
        Language::Markdown => markdown::outline(&Doc::new(text)),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod test_util;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection() {
        let cases = [
            ("src/main.rs", Language::Rust),
            ("Cargo.toml", Language::Toml),
            ("Cargo.lock", Language::Toml),
            ("README.md", Language::Markdown),
            ("notes.MARKDOWN", Language::Markdown),
            ("a.json", Language::Json),
            ("ci.yml", Language::Yaml),
            ("ci.yaml", Language::Yaml),
            ("run.sh", Language::Shell),
            ("x.zsh", Language::Shell),
            ("LICENSE", Language::PlainText),
            ("", Language::PlainText),
        ];
        for (path, language) in cases {
            assert_eq!(Language::from_path(Path::new(path)), language, "{path}");
        }
        assert_eq!(Language::Shell.name(), "Shell Script");
        assert_eq!(Language::Shell.lsp_id(), "shellscript");
        assert_eq!(Language::Json.line_comment(), Some("//"));
        assert_eq!(Language::Markdown.block_comment(), Some(("<!--", "-->")));
    }

    #[test]
    fn plain_text_has_lines_but_no_spans() {
        let h = highlight(
            Language::PlainText,
            "fn main() {}
// x
",
        );
        assert_eq!(h.len_lines(), 3);
        assert!((0..4).all(|l| h.line(l).is_empty()));
        assert!(folding_ranges(Language::PlainText, "{\n}").is_empty());
        assert!(outline(Language::PlainText, "# x").is_empty());
        // Brackets still match in plain text.
        assert_eq!(matching_bracket(Language::PlainText, "a (b) c", 2), Some(4));
    }

    #[test]
    fn highlights_accessors() {
        let span = LineSpan { start: 0, end: 2, kind: HighlightKind::Keyword };
        let h = Highlights::from_lines(vec![vec![span], vec![]]);
        assert_eq!(h.len_lines(), 2);
        assert_eq!(h.line(0), &[span]);
        assert!(h.line(1).is_empty());
        assert!(h.line(usize::MAX).is_empty());
        assert_eq!(Highlights::default().len_lines(), 0);
    }

    #[test]
    fn every_language_handles_empty_text() {
        for language in [
            Language::Rust,
            Language::Toml,
            Language::Markdown,
            Language::Json,
            Language::Yaml,
            Language::Shell,
            Language::PlainText,
        ] {
            assert_eq!(highlight(language, "").len_lines(), 1);
            assert_eq!(matching_bracket(language, "", 0), None);
            assert!(folding_ranges(language, "").is_empty());
            assert!(outline(language, "").is_empty());
        }
    }
}
