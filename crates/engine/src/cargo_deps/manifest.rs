//! Cargo.toml dependencies: a tolerant TOML scanner (tables, keys, strings, inline tables,
//! arrays) that keeps char offsets, the dependency declarations it finds
//! (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, `[workspace.dependencies]`,
//! `[target.'cfg(…)'.dependencies]`, `[dependencies.name]` tables, inline tables and dotted keys),
//! and where completion applies at a caret (crate name or version). Pure; malformed input yields
//! fewer results, never a panic.

use std::ops::Range;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Largest manifest analysed (bigger texts yield nothing).
const MAX_TEXT_CHARS: usize = 4 << 20;
/// Nesting depth of inline tables/arrays followed.
const MAX_DEPTH: usize = 32;

/// A scanned value with its char range.
#[derive(Clone, Debug, PartialEq)]
pub struct Val {
    pub span: Range<usize>,
    pub kind: ValKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValKind {
    /// A string: its (unescaped) value and the range of its contents (inside the quotes).
    Str {
        value: String,
        inner: Range<usize>,
    },
    Table(Vec<Entry>),
    Array(Vec<Val>),
    /// Numbers, booleans, dates (raw text).
    Scalar(String),
}

/// `key.path = value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: Vec<String>,
    pub key_span: Range<usize>,
    pub value: Val,
}

/// A `[table]` (or `[[array]]`) and its entries; the first section is the root table.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub header: Vec<String>,
    pub span: Range<usize>,
    pub array: bool,
    pub entries: Vec<Entry>,
}

struct Scanner<'a> {
    c: &'a [char],
    i: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn at(&self, off: usize) -> Option<char> {
        self.c.get(self.i + off).copied()
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.i += 1;
        }
    }

    fn skip_to_eol(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.i += 1;
        }
    }

    /// Spaces, newlines and comments.
    fn skip_blank(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\n') => self.i += 1,
                Some('#') => self.skip_to_eol(),
                _ => break,
            }
        }
    }

    fn is_bare(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '-'
    }

    /// `a.b."c"` → segments and range; `None` when there is no key here.
    fn key(&mut self) -> Option<(Vec<String>, Range<usize>)> {
        let start = self.i;
        let mut parts = Vec::new();
        loop {
            self.skip_spaces();
            match self.peek() {
                Some('"') | Some('\'') => {
                    let v = self.string(0)?;
                    match v.kind {
                        ValKind::Str { value, .. } => parts.push(value),
                        _ => return None,
                    }
                }
                Some(c) if Self::is_bare(c) => {
                    let s = self.i;
                    while self.peek().is_some_and(Self::is_bare) {
                        self.i += 1;
                    }
                    parts.push(self.c.get(s..self.i).unwrap_or_default().iter().collect());
                }
                _ => return None,
            }
            let end = self.i;
            self.skip_spaces();
            if self.peek() == Some('.') {
                self.i += 1;
                continue;
            }
            return Some((parts, start..end));
        }
    }

    fn string(&mut self, _depth: usize) -> Option<Val> {
        let start = self.i;
        let quote = self.peek()?;
        let literal = quote == '\'';
        let multi = self.at(1) == Some(quote) && self.at(2) == Some(quote);
        self.i += if multi { 3 } else { 1 };
        let inner_start = self.i;
        let mut value = String::new();
        while let Some(c) = self.peek() {
            if multi {
                if c == quote && self.at(1) == Some(quote) && self.at(2) == Some(quote) {
                    let inner = inner_start..self.i;
                    self.i += 3;
                    return Some(Val { span: start..self.i, kind: ValKind::Str { value, inner } });
                }
            } else if c == quote {
                let inner = inner_start..self.i;
                self.i += 1;
                return Some(Val { span: start..self.i, kind: ValKind::Str { value, inner } });
            } else if c == '\n' {
                // Unterminated: the string ends with the line (keeps the rest of the file sane).
                break;
            }
            if c == '\\' && !literal {
                let next = self.at(1);
                match next {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some('r') => value.push('\r'),
                    Some('"') => value.push('"'),
                    Some('\\') => value.push('\\'),
                    Some(other) => {
                        value.push('\\');
                        value.push(other);
                    }
                    None => {}
                }
                self.i += if next.is_some() { 2 } else { 1 };
                continue;
            }
            value.push(c);
            self.i += 1;
        }
        Some(Val { span: start..self.i, kind: ValKind::Str { value, inner: inner_start..self.i } })
    }

    fn value(&mut self, depth: usize) -> Option<Val> {
        if depth > MAX_DEPTH {
            return None;
        }
        let start = self.i;
        match self.peek()? {
            '"' | '\'' => self.string(depth),
            '{' => {
                self.i += 1;
                let mut entries = Vec::new();
                loop {
                    self.skip_blank();
                    match self.peek() {
                        None => break,
                        Some('}') => {
                            self.i += 1;
                            break;
                        }
                        Some(',') => {
                            self.i += 1;
                            continue;
                        }
                        _ => {}
                    }
                    let Some((key, key_span)) = self.key() else { break };
                    self.skip_spaces();
                    if self.peek() != Some('=') {
                        break;
                    }
                    self.i += 1;
                    self.skip_spaces();
                    let Some(value) = self.value(depth + 1) else { break };
                    entries.push(Entry { key, key_span, value });
                }
                Some(Val { span: start..self.i, kind: ValKind::Table(entries) })
            }
            '[' => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_blank();
                    match self.peek() {
                        None => break,
                        Some(']') => {
                            self.i += 1;
                            break;
                        }
                        Some(',') => {
                            self.i += 1;
                            continue;
                        }
                        _ => {}
                    }
                    let before = self.i;
                    match self.value(depth + 1) {
                        Some(v) => items.push(v),
                        None => break,
                    }
                    if self.i == before {
                        break;
                    }
                }
                Some(Val { span: start..self.i, kind: ValKind::Array(items) })
            }
            _ => {
                while let Some(c) = self.peek() {
                    if matches!(c, ',' | ']' | '}' | '\n' | '#') {
                        break;
                    }
                    self.i += 1;
                }
                let raw: String = self.c.get(start..self.i).unwrap_or_default().iter().collect();
                if self.i == start {
                    return None;
                }
                Some(Val { span: start..self.i, kind: ValKind::Scalar(raw.trim().to_string()) })
            }
        }
    }
}

/// Scan a TOML text into sections (root first).
pub fn scan(text: &str) -> Vec<Section> {
    let chars: Vec<char> = text.chars().take(MAX_TEXT_CHARS + 1).collect();
    if chars.len() > MAX_TEXT_CHARS {
        return Vec::new();
    }
    let mut p = Scanner { c: &chars, i: 0 };
    let mut sections = vec![Section { header: Vec::new(), span: 0..0, array: false, entries: Vec::new() }];
    loop {
        p.skip_blank();
        let Some(c) = p.peek() else { break };
        let line_start = p.i;
        if c == '[' {
            let array = p.at(1) == Some('[');
            p.i += if array { 2 } else { 1 };
            if let Some((header, _)) = p.key() {
                p.skip_spaces();
                if p.peek() == Some(']') {
                    p.i += if array && p.at(1) == Some(']') { 2 } else { 1 };
                    sections.push(Section { header, span: line_start..p.i, array, entries: Vec::new() });
                }
            }
            p.skip_to_eol();
            continue;
        }
        let parsed = p.key().and_then(|(key, key_span)| {
            p.skip_spaces();
            if p.peek() != Some('=') {
                return None;
            }
            p.i += 1;
            p.skip_spaces();
            p.value(0).map(|value| Entry { key, key_span, value })
        });
        if let Some(e) = parsed
            && let Some(sec) = sections.last_mut()
        {
            sec.entries.push(e);
        }
        if p.i == line_start {
            p.i += 1;
        }
        p.skip_to_eol();
    }
    sections
}

/// Which dependency table a declaration is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepTable {
    Normal,
    Dev,
    Build,
    Workspace,
}

impl DepTable {
    fn from_key(k: &str) -> Option<Self> {
        match k {
            "dependencies" => Some(Self::Normal),
            "dev-dependencies" | "dev_dependencies" => Some(Self::Dev),
            "build-dependencies" | "build_dependencies" => Some(Self::Build),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "dependencies",
            Self::Dev => "dev-dependencies",
            Self::Build => "build-dependencies",
            Self::Workspace => "workspace.dependencies",
        }
    }
}

/// Where a dependency comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepSource {
    CratesIo,
    Path,
    Git,
    /// Another registry (`registry = "…"`).
    Registry(String),
    /// `workspace = true`: the version lives in the workspace manifest.
    Workspace,
}

/// The version string of a dependency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionField {
    pub req: String,
    /// Char range of the string's contents (inside the quotes).
    pub range: Range<usize>,
}

/// One dependency declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// The key (what the code calls it).
    pub name: String,
    /// The crate on the registry (`package = "…"`, else the key).
    pub package: String,
    pub table: DepTable,
    /// `cfg(…)` / triple of a `[target.….dependencies]` table.
    pub target: Option<String>,
    /// Char range of the key (the table header for `[dependencies.name]`).
    pub key_range: Range<usize>,
    pub version: Option<VersionField>,
    pub source: DepSource,
}

impl Dependency {
    /// Whether its version can be looked up on crates.io.
    pub fn on_crates_io(&self) -> bool {
        self.source == DepSource::CratesIo && self.version.is_some()
    }
}

/// How a section header relates to dependencies.
enum SectionRole {
    /// A table whose keys are dependencies.
    List(DepTable, Option<String>),
    /// `[dependencies.name]`: the section's keys are the dependency's fields.
    Single(DepTable, Option<String>, String),
}

fn role(header: &[String]) -> Option<SectionRole> {
    match header {
        [t] => DepTable::from_key(t).map(|k| SectionRole::List(k, None)),
        [w, d] if w == "workspace" && d == "dependencies" => Some(SectionRole::List(DepTable::Workspace, None)),
        [t, name] => DepTable::from_key(t).map(|k| SectionRole::Single(k, None, name.clone())),
        [w, d, name] if w == "workspace" && d == "dependencies" => {
            Some(SectionRole::Single(DepTable::Workspace, None, name.clone()))
        }
        [tg, cfg, t] if tg == "target" => DepTable::from_key(t).map(|k| SectionRole::List(k, Some(cfg.clone()))),
        [tg, cfg, t, name] if tg == "target" => {
            DepTable::from_key(t).map(|k| SectionRole::Single(k, Some(cfg.clone()), name.clone()))
        }
        _ => None,
    }
}

/// A dependency from its fields (`(path below the dependency, value)`).
fn from_fields(
    name: &str,
    table: DepTable,
    target: Option<String>,
    key_range: Range<usize>,
    fields: &[(&[String], &Val)],
) -> Dependency {
    let mut dep = Dependency {
        name: name.to_string(),
        package: name.to_string(),
        table,
        target,
        key_range,
        version: None,
        source: DepSource::CratesIo,
    };
    let str_of = |v: &Val| match &v.kind {
        ValKind::Str { value, inner } => Some((value.clone(), inner.clone())),
        _ => None,
    };
    for (path, value) in fields {
        match (*path, &value.kind) {
            ([], ValKind::Str { .. }) => {
                if let Some((req, range)) = str_of(value) {
                    dep.version = Some(VersionField { req, range });
                }
            }
            ([], ValKind::Table(entries)) => {
                let sub: Vec<(&[String], &Val)> = entries.iter().map(|e| (e.key.as_slice(), &e.value)).collect();
                let inner = from_fields(name, table, dep.target.clone(), dep.key_range.clone(), &sub);
                dep.version = inner.version.or(dep.version.take());
                dep.package = inner.package;
                if inner.source != DepSource::CratesIo {
                    dep.source = inner.source;
                }
            }
            ([k], _) if k == "version" => {
                if let Some((req, range)) = str_of(value) {
                    dep.version = Some(VersionField { req, range });
                }
            }
            ([k], _) if k == "package" => {
                if let Some((p, _)) = str_of(value) {
                    dep.package = p;
                }
            }
            ([k], _) if k == "path" => {
                if dep.source == DepSource::CratesIo {
                    dep.source = DepSource::Path;
                }
            }
            ([k], _) if k == "git" => dep.source = DepSource::Git,
            ([k], _) if k == "registry" => {
                if let Some((r, _)) = str_of(value) {
                    dep.source = DepSource::Registry(r);
                }
            }
            ([k], ValKind::Scalar(b)) if k == "workspace" && b == "true" => dep.source = DepSource::Workspace,
            _ => {}
        }
    }
    // A path dependency with a version is published from crates.io: its version still counts.
    if dep.source == DepSource::Path && dep.version.is_some() {
        dep.source = DepSource::CratesIo;
    }
    dep
}

/// Every dependency declared in a manifest, in file order.
pub fn dependencies(text: &str) -> Vec<Dependency> {
    let sections = scan(text);
    let mut out: Vec<Dependency> = Vec::new();
    for sec in &sections {
        match role(&sec.header) {
            Some(SectionRole::List(table, target)) => {
                // Group entries by their first key segment (dotted keys: `serde.version = …`).
                let mut order: Vec<&str> = Vec::new();
                for e in &sec.entries {
                    if let Some(first) = e.key.first()
                        && !order.contains(&first.as_str())
                    {
                        order.push(first);
                    }
                }
                for name in order {
                    let entries: Vec<&Entry> =
                        sec.entries.iter().filter(|e| e.key.first().map(String::as_str) == Some(name)).collect();
                    let Some(first) = entries.first() else { continue };
                    let fields: Vec<(&[String], &Val)> =
                        entries.iter().map(|e| (e.key.get(1..).unwrap_or_default(), &e.value)).collect();
                    out.push(from_fields(name, table, target.clone(), first.key_span.clone(), &fields));
                }
            }
            Some(SectionRole::Single(table, target, name)) => {
                let fields: Vec<(&[String], &Val)> = sec.entries.iter().map(|e| (e.key.as_slice(), &e.value)).collect();
                out.push(from_fields(&name, table, target, sec.span.clone(), &fields));
            }
            None => {}
        }
    }
    out
}

/// What completion offers at a caret in Cargo.toml.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionSite {
    /// Typing a dependency key: `prefix` starts at `start`. `key_only`: the line already has
    /// `= …` after the caret (insert just the name).
    CrateName { start: usize, prefix: String, key_only: bool },
    /// Inside the version string of `package`: `typed` is the text from `start` (the string's
    /// first char) to the caret.
    Version { package: String, start: usize, typed: String },
}

fn re(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

/// The header of the section a line belongs to: the closest `[…]` line above it.
fn header_above(lines: &[&str], line: usize) -> Option<Vec<String>> {
    let header = re(r#"^\s*\[\[?\s*([A-Za-z0-9_\-."' ()=,]+?)\s*\]\]?\s*(#.*)?$"#)?;
    for l in lines.get(..line)?.iter().rev() {
        if let Some(c) = header.captures(l) {
            let inner = c.get(1)?.as_str();
            let mut p = Scanner { c: &inner.chars().collect::<Vec<char>>(), i: 0 };
            return p.key().map(|(k, _)| k);
        }
    }
    Some(Vec::new())
}

/// Where completion applies at `caret` (char offset).
pub fn completion_site(text: &str, caret: usize) -> Option<CompletionSite> {
    if text.len() > MAX_TEXT_CHARS {
        return None;
    }
    let before_all: String = text.chars().take(caret).collect();
    if before_all.chars().count() != caret {
        return None;
    }
    let line_idx = before_all.matches('\n').count();
    let before = before_all.rsplit('\n').next().unwrap_or("");
    let after: String = text.chars().skip(caret).take_while(|c| *c != '\n').collect();
    let lines: Vec<&str> = text.split('\n').collect();
    let header = header_above(&lines, line_idx)?;
    let typed_start = |typed: &str| caret.saturating_sub(typed.chars().count());
    match role(&header)? {
        SectionRole::List(..) => {
            if let Some(c) = re(r"^\s*([A-Za-z0-9_\-]*)$")?.captures(before) {
                let prefix = c.get(1)?.as_str().to_string();
                let key_only = after.contains('=');
                return Some(CompletionSite::CrateName { start: typed_start(&prefix), prefix, key_only });
            }
            if let Some(c) = re(r#"^\s*"?([A-Za-z0-9_\-]+)"?\s*=\s*"([^"]*)$"#)?.captures(before) {
                let typed = c.get(2)?.as_str().to_string();
                return Some(CompletionSite::Version {
                    package: c.get(1)?.as_str().to_string(),
                    start: typed_start(&typed),
                    typed,
                });
            }
            if let Some(c) = re(r#"^\s*"?([A-Za-z0-9_\-]+)"?\s*\.\s*version\s*=\s*"([^"]*)$"#)?.captures(before) {
                let typed = c.get(2)?.as_str().to_string();
                return Some(CompletionSite::Version {
                    package: c.get(1)?.as_str().to_string(),
                    start: typed_start(&typed),
                    typed,
                });
            }
            if let Some(c) = re(r#"^\s*"?([A-Za-z0-9_\-]+)"?\s*=\s*\{.*\bversion\s*=\s*"([^"]*)$"#)?.captures(before) {
                let typed = c.get(2)?.as_str().to_string();
                let whole = format!("{before}{after}");
                let package = re(r#"\bpackage\s*=\s*"([A-Za-z0-9_\-]+)""#)?
                    .captures(&whole)
                    .and_then(|p| p.get(1))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default());
                return Some(CompletionSite::Version { package, start: typed_start(&typed), typed });
            }
            None
        }
        SectionRole::Single(table, target, name) => {
            let c = re(r#"^\s*version\s*=\s*"([^"]*)$"#)?.captures(before)?;
            let typed = c.get(1)?.as_str().to_string();
            // `package = "…"` in the same table renames the crate.
            let package = dependencies(text)
                .into_iter()
                .find(|d| d.name == name && d.table == table && d.target == target)
                .map(|d| d.package)
                .unwrap_or(name);
            Some(CompletionSite::Version { package, start: typed_start(&typed), typed })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"[package]
name = "demo"
version = "0.1.0"   # not a dependency
edition = "2021"

[dependencies]
serde = "1.0"
serde_json = { version = "1", features = ["std"] }
rand = { version = '0.7', default-features = false }
local = { path = "../local" }
published = { path = "../p", version = "0.3" }
git-dep = { git = "https://example.com/x.git" }
"quoted-name" = "2"
toml.version = "0.5"
toml.features = [
    "a", # comment ]
    "b",
]
renamed = { package = "real-crate", version = "~1.2.3" }
ws = { workspace = true }
other = { version = "1", registry = "corp" }

[dependencies.tokio]
version = "1.30"
features = ["full"]

[dev-dependencies]
tempfile = "3"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[build-dependencies.cc]
package = "cc-real"
version = "1.0.0"

[workspace.dependencies]
anyhow = "1"

[[bin]]
name = "x"
"#;

    fn dep<'a>(deps: &'a [Dependency], name: &str) -> &'a Dependency {
        deps.iter().find(|d| d.name == name).unwrap_or_else(|| panic!("no {name} in {deps:#?}"))
    }

    fn slice(text: &str, r: &Range<usize>) -> String {
        text.chars().skip(r.start).take(r.end - r.start).collect()
    }

    #[test]
    fn every_declaration_form() {
        let deps = dependencies(MANIFEST);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "serde",
                "serde_json",
                "rand",
                "local",
                "published",
                "git-dep",
                "quoted-name",
                "toml",
                "renamed",
                "ws",
                "other",
                "tokio",
                "tempfile",
                "libc",
                "cc",
                "anyhow"
            ]
        );
        let serde = dep(&deps, "serde");
        assert_eq!(serde.version.as_ref().unwrap().req, "1.0");
        assert_eq!(slice(MANIFEST, &serde.version.as_ref().unwrap().range), "1.0");
        assert_eq!(slice(MANIFEST, &serde.key_range), "serde");
        assert_eq!(dep(&deps, "serde_json").version.as_ref().unwrap().req, "1");
        assert_eq!(slice(MANIFEST, &dep(&deps, "rand").version.as_ref().unwrap().range), "0.7");
        assert_eq!(dep(&deps, "local").source, DepSource::Path);
        assert!(!dep(&deps, "local").on_crates_io());
        assert!(dep(&deps, "published").on_crates_io());
        assert_eq!(dep(&deps, "git-dep").source, DepSource::Git);
        assert_eq!(dep(&deps, "quoted-name").version.as_ref().unwrap().req, "2");
        assert_eq!(dep(&deps, "toml").version.as_ref().unwrap().req, "0.5");
        let renamed = dep(&deps, "renamed");
        assert_eq!(renamed.package, "real-crate");
        assert_eq!(renamed.version.as_ref().unwrap().req, "~1.2.3");
        assert_eq!(dep(&deps, "ws").source, DepSource::Workspace);
        assert_eq!(dep(&deps, "other").source, DepSource::Registry("corp".into()));
        let tokio = dep(&deps, "tokio");
        assert_eq!(tokio.version.as_ref().unwrap().req, "1.30");
        assert_eq!(slice(MANIFEST, &tokio.key_range), "[dependencies.tokio]");
        assert_eq!(dep(&deps, "tempfile").table, DepTable::Dev);
        assert_eq!(dep(&deps, "libc").target.as_deref(), Some("cfg(unix)"));
        let cc = dep(&deps, "cc");
        assert_eq!((cc.table, cc.package.as_str()), (DepTable::Build, "cc-real"));
        assert_eq!(dep(&deps, "anyhow").table, DepTable::Workspace);
    }

    #[test]
    fn hostile_input_never_panics() {
        for text in [
            "",
            "[",
            "[[",
            "[dependencies",
            "[dependencies]\nserde = \"1",
            "[dependencies]\nserde = {",
            "[dependencies]\nserde = { version = ",
            "[dependencies]\nx = [[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[",
            "[dependencies]\n\"\"\"\n",
            "[dependencies]\na = '''\nmulti\n'''\nb = \"1\"",
            "\u{feff}[dependencies]\né = \"1\"\n😀 = \"2\"",
            "[dependencies]\nx = \"\\",
            "=\n=\n[dependencies]\n= \"1\"",
        ] {
            let _ = dependencies(text);
            for caret in 0..=text.chars().count() + 2 {
                let _ = completion_site(text, caret);
            }
        }
        let deps = dependencies("[dependencies]\na = '''\nmulti\n'''\nb = \"1\"");
        assert_eq!(deps.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    fn site(text_with_caret: &str) -> Option<CompletionSite> {
        let byte = text_with_caret.find('|').unwrap();
        let caret = text_with_caret[..byte].chars().count();
        let text = text_with_caret.replacen('|', "", 1);
        completion_site(&text, caret)
    }

    #[test]
    fn completion_sites() {
        assert_eq!(
            site("[package]\nname = \"x\"\n\n[dependencies]\nser|"),
            Some(CompletionSite::CrateName { start: 37, prefix: "ser".into(), key_only: false })
        );
        assert_eq!(
            site("[dependencies]\n|"),
            Some(CompletionSite::CrateName { start: 15, prefix: String::new(), key_only: false })
        );
        assert!(matches!(site("[dependencies]\nser| = \"1\""), Some(CompletionSite::CrateName { key_only: true, .. })));
        assert_eq!(
            site("[dependencies]\nserde = \"1.0.|\""),
            Some(CompletionSite::Version { package: "serde".into(), start: 24, typed: "1.0.".into() })
        );
        assert_eq!(
            site("[dev-dependencies]\nfoo = { features = [\"x\"], version = \"^0|\" }"),
            Some(CompletionSite::Version { package: "foo".into(), start: 56, typed: "^0".into() })
        );
        assert_eq!(
            site("[dependencies]\nalias = { package = \"real\", version = \"|\" }"),
            Some(CompletionSite::Version { package: "real".into(), start: 54, typed: String::new() })
        );
        assert_eq!(
            site("[dependencies]\ntoml.version = \"0|\""),
            Some(CompletionSite::Version { package: "toml".into(), start: 31, typed: "0".into() })
        );
        assert_eq!(
            site("[dependencies.tokio]\npackage = \"tokio-real\"\nversion = \"1|\""),
            Some(CompletionSite::Version { package: "tokio-real".into(), start: 55, typed: "1".into() })
        );
        assert!(matches!(site("[target.'cfg(unix)'.dependencies]\nli|"), Some(CompletionSite::CrateName { .. })));
        assert!(matches!(site("[workspace.dependencies]\nany|"), Some(CompletionSite::CrateName { .. })));
        // Not dependencies.
        assert_eq!(site("[package]\nna|"), None);
        assert_eq!(site("[package]\nversion = \"0.|\""), None);
        assert_eq!(site("[features]\ndefault = [\"|\"]"), None);
        assert_eq!(site("[dependencies]\nserde = { features = [\"|\"] }"), None);
        assert_eq!(site("|"), None);
    }
}
