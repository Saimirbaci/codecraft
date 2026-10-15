//! Settings › Editor › Code Style per language: the project's `rustfmt.toml` / `.rustfmt.toml`
//! options (Rust), its `.clang-format` basics (C/C++), and the editor's tab and indent per
//! language ([`Settings::indent_by_language`]).
//!
//! Formatter files are edited line by line: a known option's `key = value` (TOML) or
//! `Key: value` (YAML) line is replaced, added or removed, and everything else in the file
//! (comments, other options, other sections) is kept. While the Settings dialog is open the
//! changes wait in its draft and are written on Apply / OK.
//!
//! The preview runs the formatter (rustfmt, clang-format) on a sample with the draft options on
//! a worker thread, in a temporary directory that holds only the draft config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_text::Document;
use codecraft_workspace::{LanguageIndent, Settings};

/// The languages with a Code Style page: `(id, label)`.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("rust", "Rust"),
    ("cpp", "C/C++"),
    ("python", "Python"),
    ("toml", "TOML"),
    ("json", "JSON"),
    ("yaml", "YAML"),
    ("markdown", "Markdown"),
    ("shellscript", "Shell Script"),
    ("zig", "Zig"),
    ("typescript", "TypeScript"),
    ("javascript", "JavaScript"),
    ("sql", "SQL"),
    ("django-html", "Django Template"),
    ("http", "HTTP Client"),
    ("dockerfile", "Dockerfile"),
];

/// The formatter file a language's options are written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StyleFileKind {
    /// `rustfmt.toml` (or an existing `.rustfmt.toml`).
    Rustfmt,
    /// `.clang-format` (or an existing `_clang-format`).
    ClangFormat,
}

impl StyleFileKind {
    /// The kind for a Code Style language id (`c` and `cpp` share `.clang-format`).
    pub fn for_language(id: &str) -> Option<Self> {
        match id {
            "rust" => Some(Self::Rustfmt),
            "c" | "cpp" => Some(Self::ClangFormat),
            _ => None,
        }
    }

    pub fn options(self) -> &'static [StyleOption] {
        match self {
            Self::Rustfmt => RUSTFMT_OPTIONS,
            Self::ClangFormat => CLANG_FORMAT_OPTIONS,
        }
    }

    /// Candidate file names in the project root, preferred first.
    fn names(self) -> [&'static str; 2] {
        match self {
            Self::Rustfmt => ["rustfmt.toml", ".rustfmt.toml"],
            Self::ClangFormat => [".clang-format", "_clang-format"],
        }
    }

    /// The file in `root` the options go to: an existing one, else the preferred name.
    pub fn path(self, root: &Path) -> PathBuf {
        let names = self.names();
        names.iter().map(|n| root.join(n)).find(|p| p.is_file()).unwrap_or_else(|| root.join(names[0]))
    }
}

/// The kind of value an option takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OptionKind {
    Bool,
    Int { min: i64, max: i64 },
    Choice(&'static [&'static str]),
}

/// One formatter option shown on a Code Style page.
#[derive(Clone, Copy, Debug)]
pub struct StyleOption {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: OptionKind,
    /// The formatter's default (shown when the file does not set it).
    pub default: &'static str,
    /// rustfmt: needs a nightly rustfmt (stable rustfmt warns and ignores it).
    pub unstable: bool,
    pub description: &'static str,
}

const fn opt(
    key: &'static str,
    label: &'static str,
    kind: OptionKind,
    default: &'static str,
    unstable: bool,
    description: &'static str,
) -> StyleOption {
    StyleOption { key, label, kind, default, unstable, description }
}

/// rustfmt options (names and defaults from rustfmt's public configuration documentation).
pub const RUSTFMT_OPTIONS: &[StyleOption] = &[
    opt("max_width", "Hard wrap at", OptionKind::Int { min: 10, max: 1000 }, "100", false, "Maximum width of each line"),
    opt("hard_tabs", "Use tab character", OptionKind::Bool, "false", false, "Indent with tabs"),
    opt("tab_spaces", "Indent", OptionKind::Int { min: 1, max: 16 }, "4", false, "Spaces per indent level"),
    opt(
        "edition",
        "Edition",
        OptionKind::Choice(&["2015", "2018", "2021", "2024"]),
        "2015",
        false,
        "Rust edition the code is parsed with",
    ),
    opt(
        "newline_style",
        "Line separator",
        OptionKind::Choice(&["Auto", "Native", "Unix", "Windows"]),
        "Auto",
        false,
        "Unix (\\n), Windows (\\r\\n), Native or Auto (the file's first one)",
    ),
    opt(
        "use_small_heuristics",
        "Width heuristics",
        OptionKind::Choice(&["Default", "Off", "Max"]),
        "Default",
        false,
        "How the widths of calls, chains, structs… scale with max_width",
    ),
    opt("reorder_imports", "Reorder imports", OptionKind::Bool, "true", false, "Sort `use` items alphabetically"),
    opt("reorder_modules", "Reorder modules", OptionKind::Bool, "true", false, "Sort `mod` items alphabetically"),
    opt(
        "use_field_init_shorthand",
        "Field init shorthand",
        OptionKind::Bool,
        "false",
        false,
        "Write `S { x }` instead of `S { x: x }`",
    ),
    opt("use_try_shorthand", "Use `?`", OptionKind::Bool, "false", false, "Replace `try!(…)` with `…?`"),
    opt("merge_derives", "Merge derives", OptionKind::Bool, "true", false, "Merge consecutive `#[derive]`s"),
    opt(
        "imports_granularity",
        "Imports granularity",
        OptionKind::Choice(&["Preserve", "Crate", "Module", "Item", "One"]),
        "Preserve",
        true,
        "Merge or split `use` items",
    ),
    opt(
        "group_imports",
        "Group imports",
        OptionKind::Choice(&["Preserve", "StdExternalCrate", "One"]),
        "Preserve",
        true,
        "Group `use` items: std, external crates, then the crate",
    ),
    opt("wrap_comments", "Wrap comments", OptionKind::Bool, "false", true, "Wrap comments at max_width"),
    opt(
        "format_code_in_doc_comments",
        "Format code in doc comments",
        OptionKind::Bool,
        "false",
        true,
        "Format Rust code blocks in doc comments",
    ),
    opt("normalize_comments", "Normalize comments", OptionKind::Bool, "false", true, "Turn /* */ comments into //"),
];

/// clang-format basics (names from clang-format's public style options documentation).
pub const CLANG_FORMAT_OPTIONS: &[StyleOption] = &[
    opt(
        "BasedOnStyle",
        "Based on style",
        OptionKind::Choice(&["LLVM", "Google", "Chromium", "Mozilla", "WebKit", "Microsoft", "GNU"]),
        "LLVM",
        false,
        "The predefined style the other options change",
    ),
    opt("IndentWidth", "Indent", OptionKind::Int { min: 1, max: 16 }, "2", false, "Columns per indent level"),
    opt("TabWidth", "Tab size", OptionKind::Int { min: 1, max: 16 }, "8", false, "Columns a tab takes"),
    opt("ColumnLimit", "Hard wrap at", OptionKind::Int { min: 0, max: 1000 }, "80", false, "0 = no limit"),
    opt(
        "UseTab",
        "Use tab character",
        OptionKind::Choice(&["Never", "ForIndentation", "ForContinuationAndIndentation", "AlignWithSpaces", "Always"]),
        "Never",
        false,
        "Where tabs are used",
    ),
    opt(
        "BreakBeforeBraces",
        "Braces",
        OptionKind::Choice(&["Attach", "Linux", "Mozilla", "Stroustrup", "Allman", "Whitesmiths", "GNU", "WebKit"]),
        "Attach",
        false,
        "Where opening braces go",
    ),
];

/// The option `key` of a file kind.
pub fn find_option(kind: StyleFileKind, key: &str) -> Option<&'static StyleOption> {
    kind.options().iter().find(|o| o.key == key)
}

/// Validate a value for an option: booleans, integers in range, one of the choices (any case).
pub fn check_value(o: &StyleOption, v: &Value) -> Result<Value, String> {
    match o.kind {
        OptionKind::Bool => v.as_bool().map(Value::Bool).ok_or_else(|| format!("{}: expected true or false", o.key)),
        OptionKind::Int { min, max } => {
            let n = v
                .as_i64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .ok_or_else(|| format!("{}: expected an integer", o.key))?;
            if !(min..=max).contains(&n) {
                return Err(format!("{}: {n} is out of range {min}–{max}", o.key));
            }
            Ok(json!(n))
        }
        OptionKind::Choice(choices) => {
            let s = match v {
                Value::String(s) => s.trim().to_string(),
                Value::Number(n) => n.to_string(),
                _ => return Err(format!("{}: expected one of {}", o.key, choices.join(", "))),
            };
            choices
                .iter()
                .find(|c| c.eq_ignore_ascii_case(&s))
                .map(|c| json!(c))
                .ok_or_else(|| format!("{}: `{s}` is not one of {}", o.key, choices.join(", ")))
        }
    }
}

/// The option values a formatter file sets (known options only; malformed values are skipped).
pub type StyleValues = BTreeMap<String, Value>;

/// Most bytes of a formatter file read.
const MAX_FILE: u64 = 256 * 1024;

/// Read a formatter file's values (`None` when it does not exist; an unreadable or oversized
/// file is an error).
pub fn read_file(kind: StyleFileKind, path: &Path) -> Result<Option<StyleValues>, String> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        Ok(m) if m.len() > MAX_FILE => return Err(format!("{} is too large", path.display())),
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(Some(parse(kind, &text)))
}

/// The known options set in a file's text.
pub fn parse(kind: StyleFileKind, text: &str) -> StyleValues {
    let mut out = StyleValues::new();
    for line in top_level_lines(kind, text) {
        let Some((key, raw)) = split_line(kind, line) else { continue };
        let Some(o) = find_option(kind, key) else { continue };
        let v = match o.kind {
            OptionKind::Bool => match raw {
                "true" => json!(true),
                "false" => json!(false),
                _ => continue,
            },
            OptionKind::Int { .. } => match raw.parse::<i64>() {
                Ok(n) => json!(n),
                Err(_) => continue,
            },
            OptionKind::Choice(_) => json!(raw.trim_matches(|c| c == '"' || c == '\'')),
        };
        if let Ok(v) = check_value(o, &v) {
            // A repeated key: the first line counts (as `rewrite` keeps the first).
            out.entry(key.to_string()).or_insert(v);
        }
    }
    out
}

/// The lines of the part of the file options are read from and written to: TOML lines before
/// the first `[table]`; YAML lines of the first document.
fn top_level_lines(kind: StyleFileKind, text: &str) -> impl Iterator<Item = &str> {
    let mut done = false;
    let mut started = false;
    text.lines().filter(move |l| {
        if done {
            return false;
        }
        let t = l.trim();
        match kind {
            StyleFileKind::Rustfmt if t.starts_with('[') => {
                done = true;
                false
            }
            StyleFileKind::ClangFormat if t == "---" || t == "..." => {
                if started {
                    done = true;
                }
                started = true;
                false
            }
            _ => {
                started |= !t.is_empty() && !t.starts_with('#');
                true
            }
        }
    })
}

/// `(key, raw value)` of an option line (comments after the value dropped), if it is one.
fn split_line(kind: StyleFileKind, line: &str) -> Option<(&str, &str)> {
    if line.starts_with([' ', '\t']) {
        return None; // nested (YAML) or continuation
    }
    let sep = if kind == StyleFileKind::Rustfmt { '=' } else { ':' };
    let (key, rest) = line.split_once(sep)?;
    let key = key.trim();
    if key.is_empty() || key.starts_with('#') || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let raw = strip_comment(rest).trim();
    Some((key, raw))
}

/// A value without a trailing `# comment` (outside quotes).
fn strip_comment(s: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') => return s.get(..i).unwrap_or(s),
            _ => {}
        }
    }
    s
}

fn render(kind: StyleFileKind, key: &str, v: &Value) -> String {
    let value = match (kind, v) {
        (StyleFileKind::Rustfmt, Value::String(s)) => format!("\"{s}\""),
        (_, Value::String(s)) => s.clone(),
        (_, other) => other.to_string(),
    };
    match kind {
        StyleFileKind::Rustfmt => format!("{key} = {value}"),
        StyleFileKind::ClangFormat => format!("{key}: {value}"),
    }
}

/// `text` with the known options set to `values`: changed lines replaced in place, options no
/// longer set removed, new ones added at the end of the top-level part. Other lines are kept.
pub fn rewrite(kind: StyleFileKind, text: &str, values: &StyleValues) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut written: Vec<&str> = Vec::new();
    // Index where new options go: after the last top-level line.
    let mut insert_at: usize = 0;
    let top: Vec<&str> = top_level_lines(kind, text).collect();
    let mut top_iter = top.iter().peekable();
    for line in text.lines() {
        let is_top = top_iter.peek().is_some_and(|t| std::ptr::eq(**t, line));
        if is_top {
            top_iter.next();
        }
        let known = is_top.then(|| split_line(kind, line)).flatten().and_then(|(k, _)| find_option(kind, k));
        match known {
            Some(o) => match values.get(o.key) {
                Some(v) if !written.contains(&o.key) => {
                    lines.push(render(kind, o.key, v));
                    written.push(o.key);
                    insert_at = lines.len();
                }
                // Unset (or a duplicate line): drop it.
                _ => {}
            },
            None => {
                lines.push(line.to_string());
                if is_top && !line.trim().is_empty() {
                    insert_at = lines.len();
                }
            }
        }
    }
    let new: Vec<String> = kind
        .options()
        .iter()
        .filter(|o| !written.contains(&o.key))
        .filter_map(|o| values.get(o.key).map(|v| render(kind, o.key, v)))
        .collect();
    if kind == StyleFileKind::ClangFormat && lines.first().is_some_and(|l| l.trim() == "---") && insert_at == 0 {
        insert_at = 1;
    }
    let at = insert_at.min(lines.len());
    lines.splice(at..at, new);
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Write `values` to the file of `kind` in `root` (created when missing; nothing is written
/// when no value changes and the file does not exist).
pub fn write_values(kind: StyleFileKind, root: &Path, values: &StyleValues) -> Result<PathBuf, String> {
    let path = kind.path(root);
    let current = std::fs::read_to_string(&path).ok();
    if current.is_none() && values.is_empty() {
        return Ok(path);
    }
    let text = rewrite(kind, current.as_deref().unwrap_or(""), values);
    if current.as_deref() == Some(text.as_str()) {
        return Ok(path);
    }
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

// ---------------------------------------------------------------- indent per language

/// A language's tab and indent: its Code Style entry, else the global tab size.
pub fn indent(settings: &Settings, language: Language) -> LanguageIndent {
    indent_for_id(settings, language.lsp_id())
}

/// [`indent`] by language id (`c` falls back to `cpp`'s entry and vice versa).
pub fn indent_for_id(settings: &Settings, id: &str) -> LanguageIndent {
    let sibling = match id {
        "c" => Some("cpp"),
        "cpp" => Some("c"),
        _ => None,
    };
    let global = settings.tab_size.clamp(1, 16);
    settings
        .indent_by_language
        .get(id)
        .or_else(|| sibling.and_then(|s| settings.indent_by_language.get(s)))
        .map(|i| LanguageIndent {
            tab_size: i.tab_size.clamp(1, 16),
            indent_size: i.indent_size.clamp(1, 16),
            use_tab: i.use_tab,
        })
        .unwrap_or(LanguageIndent { tab_size: global, indent_size: global, use_tab: false })
}

/// One indent level for `doc`: the file's own style when it has indented lines (tabs or
/// spaces, by majority), else the language's setting.
pub fn indent_unit(doc: &Document, opts: LanguageIndent) -> String {
    let (mut tabs, mut spaces) = (0usize, 0usize);
    for i in 0..doc.len_lines().min(2000) {
        match doc.char_at(doc.line_start(i)) {
            Some('\t') => tabs += 1,
            Some(' ') => spaces += 1,
            _ => {}
        }
    }
    let use_tab = if tabs + spaces == 0 { opts.use_tab } else { tabs > spaces };
    if use_tab { "\t".to_string() } else { " ".repeat(opts.indent_size.clamp(1, 16)) }
}

/// Validate an indent entry.
pub fn check_indent(i: &LanguageIndent) -> Result<LanguageIndent, String> {
    for (name, n) in [("tab_size", i.tab_size), ("indent_size", i.indent_size)] {
        if !(1..=16).contains(&n) {
            return Err(format!("{name}: {n} is out of range 1–16"));
        }
    }
    Ok(*i)
}

/// Validate the whole per-language map (known Code Style language ids, `c` included).
pub fn check_indents(map: &BTreeMap<String, LanguageIndent>) -> Result<BTreeMap<String, LanguageIndent>, String> {
    let mut out = BTreeMap::new();
    for (id, i) in map {
        let id = id.trim().to_ascii_lowercase();
        if id != "c" && !LANGUAGES.iter().any(|(l, _)| *l == id) {
            return Err(format!("unknown language `{id}`"));
        }
        out.insert(id, check_indent(i)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- preview

/// The sample code a page previews.
pub fn sample(kind: StyleFileKind) -> &'static str {
    match kind {
        StyleFileKind::Rustfmt => RUST_SAMPLE,
        StyleFileKind::ClangFormat => CPP_SAMPLE,
    }
}

const RUST_SAMPLE: &str = "use std::collections::{HashMap, BTreeMap};\nuse std::fmt;\nuse crate::model::Point;\n\n#[derive(Debug)]\n#[derive(Clone)]\npub struct Config { name: String, width: usize, verbose: bool }\n\nimpl Config {\n    pub fn new(name: String, width: usize) -> Self { Config { name: name, width: width, verbose: false } }\n\n    pub fn describe(&self, items: &[u32]) -> String {\n        let total: u32 = items.iter().filter(|x| **x > 1).map(|x| x * 2).sum();\n        format!(\"{} is {} wide and adds up to {} ({} items)\", self.name, self.width, total, items.len())\n    }\n}\n";

const CPP_SAMPLE: &str = "#include <vector>\n#include <string>\n\nnamespace demo {\nclass Shape {\npublic:\n  Shape(std::string name, int sides) : name_(name), sides_(sides) {}\n  int sides() const { return sides_; }\n  std::string describe(const std::vector<int>& lengths) const {\n    int total = 0;\n    for (int l : lengths) { if (l > 0) { total += l; } }\n    return name_ + \" with \" + std::to_string(sides_) + \" sides and perimeter \" + std::to_string(total);\n  }\nprivate:\n  std::string name_;\n  int sides_;\n};\n}\n";

/// The Code Style preview: the sample formatted with the draft options.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub kind: Option<StyleFileKind>,
    pub running: bool,
    pub text: Option<String>,
    pub error: Option<String>,
}

/// The preview's worker.
#[derive(Debug, Default)]
pub struct PreviewJob {
    pub view: Preview,
    rx: Option<Receiver<Result<String, String>>>,
}

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Format the sample with `values` on a worker thread (replacing a running preview).
pub fn start_preview(job: &mut PreviewJob, kind: StyleFileKind, values: StyleValues) {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-style-preview".into()).spawn(move || {
        let _ = tx.send(run_preview(kind, &values));
    });
    job.view = Preview { kind: Some(kind), running: spawned.is_ok(), text: None, error: None };
    match spawned {
        Ok(_) => job.rx = Some(rx),
        Err(e) => job.view.error = Some(format!("cannot start the preview: {e}")),
    }
}

fn run_preview(kind: StyleFileKind, values: &StyleValues) -> Result<String, String> {
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("codecraft-style-preview-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let result = (|| {
        let config = kind.path(&dir);
        std::fs::write(&config, rewrite(kind, "", values)).map_err(|e| e.to_string())?;
        match kind {
            StyleFileKind::Rustfmt => {
                let edition = values.get("edition").and_then(Value::as_str).unwrap_or("2021");
                codecraft_cargo::rustfmt(RUST_SAMPLE, edition, &dir).map_err(|e| e.to_string())
            }
            StyleFileKind::ClangFormat => clang_format(&dir, CPP_SAMPLE),
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// `clang-format` on `source` with the style file in `dir`.
fn clang_format(dir: &Path, source: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("clang-format")
        .arg("--assume-filename=preview.cpp")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("clang-format not found ({e})"))?;
    let writer = child.stdin.take().map(|mut stdin| {
        let input = source.to_string();
        std::thread::spawn(move || stdin.write_all(input.as_bytes()))
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().chars().take(2000).collect());
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// Drain the preview worker. Returns true if the preview changed.
pub fn pump_preview(job: &mut PreviewJob) -> bool {
    let Some(rx) = &job.rx else { return false };
    match rx.try_recv() {
        Ok(r) => {
            match r {
                Ok(t) => job.view.text = Some(t),
                Err(e) => job.view.error = Some(e),
            }
            job.view.running = false;
            job.rx = None;
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            job.view.running = false;
            job.view.error = Some("the formatter stopped".into());
            job.rx = None;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustfmt_values_parse_and_rewrite_keeping_other_lines() {
        let text = "# project style\nmax_width = 120 # wide\nhard_tabs = false\nunknown_option = \"x\"\nmax_width = 90\nedition = \"2021\"\n\n[unstable]\ntab_spaces = 8\n";
        let v = parse(StyleFileKind::Rustfmt, text);
        assert_eq!(v.get("max_width"), Some(&json!(120)), "the first line wins");
        assert_eq!(v.get("edition"), Some(&json!("2021")));
        assert!(!v.contains_key("tab_spaces"), "only the top-level table");
        let mut next = v.clone();
        next.insert("max_width".into(), json!(80));
        next.remove("hard_tabs");
        next.insert("imports_granularity".into(), json!("Crate"));
        let out = rewrite(StyleFileKind::Rustfmt, text, &next);
        assert_eq!(
            out,
            "# project style\nmax_width = 80\nunknown_option = \"x\"\nedition = \"2021\"\nimports_granularity = \"Crate\"\n\n[unstable]\ntab_spaces = 8\n"
        );
        assert_eq!(parse(StyleFileKind::Rustfmt, &out).get("max_width"), Some(&json!(80)));
        // Malformed values are ignored, not fatal.
        let bad = parse(StyleFileKind::Rustfmt, "max_width = lots\nhard_tabs = 3\nedition = \"1999\"\n=\n");
        assert!(bad.is_empty());
        assert_eq!(rewrite(StyleFileKind::Rustfmt, "", &StyleValues::new()), "");
    }

    #[test]
    fn clang_format_values_parse_and_rewrite() {
        let text = "---\n# mine\nBasedOnStyle: Google\nIndentWidth: 4\nIncludeCategories:\n  - Regex: x\n    Priority: 2\n---\nLanguage: JavaScript\nIndentWidth: 8\n";
        let v = parse(StyleFileKind::ClangFormat, text);
        assert_eq!(v.get("BasedOnStyle"), Some(&json!("Google")));
        assert_eq!(v.get("IndentWidth"), Some(&json!(4)), "the first document only");
        let mut next = v.clone();
        next.insert("ColumnLimit".into(), json!(100));
        next.insert("UseTab".into(), json!("Never"));
        let out = rewrite(StyleFileKind::ClangFormat, text, &next);
        assert!(out.starts_with("---\n# mine\nBasedOnStyle: Google\nIndentWidth: 4\nIncludeCategories:\n  - Regex: x\n    Priority: 2\nColumnLimit: 100\nUseTab: Never\n---\n"), "{out}");
        assert!(out.ends_with("Language: JavaScript\nIndentWidth: 8\n"));
        let fresh = rewrite(StyleFileKind::ClangFormat, "", &next);
        assert_eq!(fresh, "BasedOnStyle: Google\nIndentWidth: 4\nColumnLimit: 100\nUseTab: Never\n");
    }

    #[test]
    fn values_are_validated() {
        let o = find_option(StyleFileKind::Rustfmt, "max_width").unwrap();
        assert_eq!(check_value(o, &json!("90")).unwrap(), json!(90));
        assert!(check_value(o, &json!(5)).is_err());
        assert!(check_value(o, &json!(true)).is_err());
        let e = find_option(StyleFileKind::Rustfmt, "edition").unwrap();
        assert_eq!(check_value(e, &json!(2021)).unwrap(), json!("2021"));
        assert!(check_value(e, &json!("2030")).is_err());
        let b = find_option(StyleFileKind::ClangFormat, "BreakBeforeBraces").unwrap();
        assert_eq!(check_value(b, &json!("allman")).unwrap(), json!("Allman"));
        assert!(find_option(StyleFileKind::ClangFormat, "max_width").is_none());
    }

    #[test]
    fn files_are_found_written_and_left_alone_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(StyleFileKind::Rustfmt.path(root), root.join("rustfmt.toml"));
        std::fs::write(root.join(".rustfmt.toml"), "max_width = 90\n").unwrap();
        assert_eq!(StyleFileKind::Rustfmt.path(root), root.join(".rustfmt.toml"));
        let mut v = read_file(StyleFileKind::Rustfmt, &root.join(".rustfmt.toml")).unwrap().unwrap();
        v.insert("tab_spaces".into(), json!(2));
        let p = write_values(StyleFileKind::Rustfmt, root, &v).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "max_width = 90\ntab_spaces = 2\n");
        // Nothing to write for an absent file.
        write_values(StyleFileKind::ClangFormat, root, &StyleValues::new()).unwrap();
        assert!(!root.join(".clang-format").exists());
        assert!(read_file(StyleFileKind::ClangFormat, &root.join(".clang-format")).unwrap().is_none());
        // Hostile content never panics.
        let junk = "\u{0}=\u{feff}\n[[[\nmax_width = 99999999999999999999\n\"a\" = \"\n#=#\n";
        let _ = parse(StyleFileKind::Rustfmt, junk);
        let _ = rewrite(StyleFileKind::ClangFormat, junk, &v);
    }

    #[test]
    fn indent_per_language_and_detection() {
        let mut s = Settings { tab_size: 2, ..Default::default() };
        assert_eq!(indent(&s, Language::Rust), LanguageIndent { tab_size: 2, indent_size: 2, use_tab: false });
        s.indent_by_language.insert("cpp".into(), LanguageIndent { tab_size: 8, indent_size: 3, use_tab: true });
        assert_eq!(indent(&s, Language::C).indent_size, 3, "C uses the C/C++ entry");
        let opts = indent(&s, Language::Cpp);
        assert_eq!(indent_unit(&Document::from_text("int x;\n"), opts), "\t", "no indented lines: the setting");
        assert_eq!(indent_unit(&Document::from_text("{\n  x;\n  y;\n}\n"), opts), "   ", "spaces: the indent size");
        assert_eq!(indent_unit(&Document::from_text("{\n\tx;\n}\n"), indent(&s, Language::Rust)), "\t");
        assert!(check_indents(&BTreeMap::from([("cobol".into(), LanguageIndent::default())])).is_err());
        assert!(check_indent(&LanguageIndent { tab_size: 0, ..Default::default() }).is_err());
    }

    #[test]
    fn preview_formats_with_rustfmt_when_installed() {
        let mut job = PreviewJob::default();
        let values = StyleValues::from([("tab_spaces".to_string(), json!(2)), ("max_width".to_string(), json!(60))]);
        start_preview(&mut job, StyleFileKind::Rustfmt, values);
        let start = std::time::Instant::now();
        while job.view.running && start.elapsed() < std::time::Duration::from_secs(30) {
            pump_preview(&mut job);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if job.view.error.as_deref().is_some_and(|e| e.starts_with("failed to run")) {
            return; // rustfmt is not installed
        }
        let text = job.view.text.clone().unwrap_or_else(|| panic!("{:?}", job.view.error));
        assert!(text.contains("\n  pub fn new"), "two-space indent: {text}");
        assert!(text.lines().all(|l| l.chars().count() <= 60 || l.contains('"')), "{text}");
    }
}
