//! C/C++ analyzers: the Clang Static Analyzer (`clang --analyze` per translation unit, flags
//! from `compile_commands.json`), clangd's diagnostics (a background clangd opens each file and
//! reports what the editor would show, quick-fixes from its code actions), cppcheck when
//! installed, and clang-tidy's `--export-fixes` YAML turned into quick-fixes.
//!
//! Coordinates: clang and cppcheck print 1-based lines and byte columns; clangd speaks LSP
//! (UTF-16); clang-tidy's fixes are byte offsets into the file. All become char positions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use codecraft_lsp::lsp_types as lt;
use codecraft_lsp::{LspClient, ServerConfig, ServerEvent};
use codecraft_text::{Document, Position};
use codecraft_workspace::Severity;

use super::analyzers::run_capture;
use crate::inspections::{self, Applicability, Finding, Fix, TextChange, Tool, ToolRun, docs_url, resolve_path};

/// Translation units the static analyzer runs on per inspection.
const MAX_UNITS: usize = 400;
/// Files a background clangd opens per inspection.
const MAX_CLANGD_FILES: usize = 300;
/// Files clangd works on at once.
const CLANGD_BATCH: usize = 4;
/// Code-action requests (quick-fixes) per clangd run.
const MAX_CODE_ACTIONS: usize = 200;
/// Largest compile_commands.json read.
const MAX_DB_BYTES: u64 = 64 << 20;

pub(crate) const TU_EXTS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "m", "mm"];
pub(crate) const HEADER_EXTS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "inl", "ipp"];

fn ext_in(p: &Path, exts: &[&str]) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn is_tu(p: &Path) -> bool {
    ext_in(p, TU_EXTS)
}

pub(crate) fn is_c_family(p: &Path) -> bool {
    ext_in(p, TU_EXTS) || ext_in(p, HEADER_EXTS)
}

// ---------------------------------------------------------------- compile_commands.json

/// One compile command: the directory it runs in, the file and its arguments (compiler first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileCommand {
    pub directory: PathBuf,
    pub file: PathBuf,
    pub arguments: Vec<String>,
}

/// Split a shell command line (single and double quotes, backslash escapes).
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = cmd.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), c) => cur.push(c),
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') => match chars.peek() {
                Some(&n) if n == '"' || n == '\\' || n == '$' || n == '`' => {
                    cur.push(n);
                    chars.next();
                }
                _ => cur.push('\\'),
            },
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                have = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                have = true;
            }
            (None, c) if c.is_whitespace() => {
                if have || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (None, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse a compilation database (entries with `command` or `arguments`).
pub fn parse_compile_db(text: &str) -> Result<Vec<CompileCommand>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("compile_commands.json: {e}"))?;
    let items = v.as_array().ok_or("compile_commands.json is not a list")?;
    let mut out = Vec::new();
    for it in items.iter().take(inspections::MAX_SCOPE_FILES) {
        let Some(dir) = it.get("directory").and_then(Value::as_str) else { continue };
        let Some(file) = it.get("file").and_then(Value::as_str) else { continue };
        let directory = PathBuf::from(dir);
        let file = crate::normalize_path(&if Path::new(file).is_absolute() { PathBuf::from(file) } else { directory.join(file) });
        let arguments = match (it.get("arguments").and_then(Value::as_array), it.get("command").and_then(Value::as_str)) {
            (Some(a), _) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            (None, Some(c)) => split_command(c),
            _ => continue,
        };
        if arguments.is_empty() {
            continue;
        }
        out.push(CompileCommand { directory, file, arguments });
    }
    Ok(out)
}

pub(crate) fn load_compile_db(dir: &Path) -> Result<Vec<CompileCommand>, String> {
    let path = dir.join("compile_commands.json");
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_DB_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_compile_db(&text)
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// `clang --analyze` arguments for a compile command: its flags without the compiler, output
/// and dependency-file options or the source file, then the analyzer's own.
pub fn analyzer_args(cmd: &CompileCommand, extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = cmd.arguments.iter().skip(1).peekable();
    let file_name = cmd.file.file_name().map(|n| n.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "-MF" | "-MT" | "-MQ" | "-Xclang-output" => {
                it.next();
            }
            "-c" | "-MD" | "-MMD" | "-M" | "-MM" | "-MP" | "--" | "-fcolor-diagnostics" => {}
            _ if a.starts_with("-o") && a.len() > 2 && !a.starts_with("-objc") => {}
            _ if a.starts_with("-MF") || a.starts_with("-MT") || a.starts_with("-MQ") => {}
            _ => {
                let p = Path::new(a);
                let same = crate::normalize_path(&if p.is_absolute() { p.to_path_buf() } else { cmd.directory.join(p) }) == cmd.file
                    || (!a.starts_with('-') && file_name.as_deref() == Some(a.as_str()));
                if !same {
                    out.push(a.clone());
                }
            }
        }
    }
    out.extend(
        ["--analyze", "--analyzer-output", "text", "-fno-color-diagnostics", "-o", null_device()].map(String::from),
    );
    out.extend(extra.iter().cloned());
    out.push(cmd.file.display().to_string());
    out
}

/// Analyzer arguments for a file without a compilation database.
fn default_analyzer_args(root: &Path, file: &Path, extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> =
        ["--analyze", "--analyzer-output", "text", "-fno-color-diagnostics", "-o", null_device()].map(String::from).to_vec();
    for inc in [root.to_path_buf(), root.join("include"), root.join("src")] {
        if inc.is_dir() {
            out.push(format!("-I{}", inc.display()));
        }
    }
    if ext_in(file, &["cc", "cpp", "cxx", "c++", "mm"]) {
        out.push("-std=c++17".into());
    }
    out.extend(extra.iter().cloned());
    out.push(file.display().to_string());
    out
}

// ---------------------------------------------------------------- the Clang Static Analyzer

/// Run the static analyzer on the scope's translation units.
pub(crate) fn run_clang_analyzer(
    clang: &Path,
    root: &Path,
    db_dir: Option<&Path>,
    files: &[PathBuf],
    extra: &[String],
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<(ToolRun, Vec<Finding>), String> {
    let db = match db_dir {
        Some(d) => Some(load_compile_db(d)?),
        None => None,
    };
    let units: Vec<&PathBuf> = files.iter().filter(|f| is_tu(f)).collect();
    let mut plan: Vec<(PathBuf, Vec<String>, PathBuf)> = Vec::new();
    let mut not_in_db = 0usize;
    for f in units.iter().take(MAX_UNITS) {
        match &db {
            Some(db) => match db.iter().find(|c| &&c.file == f) {
                Some(c) => plan.push(((*f).clone(), analyzer_args(c, extra), c.directory.clone())),
                None => not_in_db += 1,
            },
            None => plan.push(((*f).clone(), default_analyzer_args(root, f, extra), root.to_path_buf())),
        }
    }
    let read = |p: &Path| codecraft_fs::read_text_file(p).ok();
    let mut findings = Vec::new();
    let mut failed = 0usize;
    let mut last_error = String::new();
    let total = plan.len();
    for (i, (file, args, dir)) in plan.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(inspections::CANCELLED.into());
        }
        progress(&format!("Clang Static Analyzer: {} ({}/{total})", file.strip_prefix(root).unwrap_or(&file).display(), i + 1));
        let cwd = if dir.is_dir() { dir } else { root.to_path_buf() };
        let out = run_capture(clang, &args, &cwd, &[], cancel)?;
        let text = out.stderr.join("\n");
        let found = parse_analyzer(&text, &cwd, &read);
        if !out.success && found.iter().all(|f| f.severity != Severity::Error) {
            failed += 1;
            last_error = out.summary();
        }
        findings.extend(found);
    }
    let mut outcome = format!("{} in {total} translation unit(s)", inspections::count_label(findings.len()));
    if db_dir.is_none() && total > 0 {
        outcome.push_str(" (no compile_commands.json: default flags)");
    }
    if not_in_db > 0 {
        outcome.push_str(&format!(", {not_in_db} file(s) not in compile_commands.json"));
    }
    if failed > 0 {
        outcome.push_str(&format!(", {failed} failed: {last_error}"));
    }
    let command_line = format!("{} --analyze --analyzer-output text …", clang.display());
    Ok((ToolRun { tool: Tool::ClangAnalyzer, command_line, outcome, ok: failed < total.max(1) }, findings))
}

/// The analyzer's text output: `file:line:col: warning: message [checker]` plus notes (the path
/// to the bug), appended to the description.
pub fn parse_analyzer(text: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    crate::inspections::linters::parse_diagnostic_lines(text, base, Tool::ClangAnalyzer, read)
}

// ---------------------------------------------------------------- cppcheck

fn cppcheck_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(.+?):(\d+):(\d+): (error|warning|style|performance|portability|information|debug): (.*?) \[(\w+)\]\s*$").ok()
    })
    .as_ref()
}

pub const CPPCHECK_TEMPLATE: &str = "{file}:{line}:{column}: {severity}: {message} [{id}]";

/// cppcheck's output with [`CPPCHECK_TEMPLATE`]: errors, warnings, and style, performance and
/// portability as weak warnings (information and debug lines are dropped).
pub fn parse_cppcheck(text: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    let Some(re) = cppcheck_re() else { return Vec::new() };
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(c) = re.captures(line.trim_end()) else { continue };
        let (Some(file), Some(ln), Some(col), Some(sev), Some(msg), Some(id)) =
            (c.get(1), c.get(2), c.get(3), c.get(4), c.get(5), c.get(6))
        else {
            continue;
        };
        let (Ok(ln), Ok(col)) = (ln.as_str().parse::<usize>(), col.as_str().parse::<usize>()) else { continue };
        let severity = match sev.as_str() {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            "style" | "performance" | "portability" => Severity::Info,
            _ => continue,
        };
        if ln == 0 || file.as_str() == "nofile" {
            continue;
        }
        let path = resolve_path(base, file.as_str());
        let text = texts.entry(path.clone()).or_insert_with(|| read(&path));
        let column = byte_col_to_char(text.as_deref(), ln - 1, col.saturating_sub(1));
        let mut f = Finding::new(Tool::Cppcheck, id.as_str(), severity, path, Position::new(ln - 1, column));
        f.code = Some(id.as_str().to_string());
        f.message = msg.as_str().to_string();
        f.description = format!("{}\n\ncppcheck: {} ({})", msg.as_str(), id.as_str(), sev.as_str());
        out.push(f);
    }
    out
}

/// Zero-based byte column → char column on a line of `text`.
fn byte_col_to_char(text: Option<&str>, line: usize, col: usize) -> usize {
    let Some(l) = text.and_then(|t| t.split('\n').nth(line)) else { return col };
    let mut b = col.min(l.len());
    while b > 0 && !l.is_char_boundary(b) {
        b -= 1;
    }
    l.get(..b).map_or(col, |p| p.chars().count())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_cppcheck(
    program: &Path,
    root: &Path,
    db_dir: Option<&Path>,
    files: &[PathBuf],
    project_wide: bool,
    enable: &str,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<(ToolRun, Vec<Finding>), String> {
    let mut args: Vec<String> = vec![
        format!("--enable={enable}"),
        "--inline-suppr".into(),
        "--quiet".into(),
        format!("--template={CPPCHECK_TEMPLATE}"),
    ];
    match db_dir {
        Some(d) if project_wide => args.push(format!("--project={}", d.join("compile_commands.json").display())),
        _ => args.extend(files.iter().filter(|f| is_tu(f)).take(2000).map(|p| p.display().to_string())),
    }
    progress("Running cppcheck…");
    let command_line = format!("cppcheck {}", args.iter().take(4).map(|a| codecraft_cargo::shell_quote(a)).collect::<Vec<_>>().join(" "));
    let out = run_capture(program, &args, root, &[], cancel)?;
    let read = |p: &Path| codecraft_fs::read_text_file(p).ok();
    let mut text = out.stderr.join("\n");
    text.push('\n');
    text.push_str(&out.stdout.join("\n"));
    let findings = parse_cppcheck(&text, root, &read);
    let ok = out.success || !findings.is_empty();
    let outcome = if ok { inspections::count_label(findings.len()) } else { format!("failed: {}", out.summary()) };
    Ok((ToolRun { tool: Tool::Cppcheck, command_line, outcome, ok }, findings))
}

// ---------------------------------------------------------------- clang-tidy --export-fixes

/// A YAML scalar on one line: plain, `'single'` (`''` escapes a quote) or `"double"`.
fn yaml_scalar(v: &str) -> String {
    let v = v.trim();
    if let Some(inner) = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return inner.replace("''", "'");
    }
    if let Some(inner) = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        return out;
    }
    v.to_string()
}

#[derive(Default, Debug)]
struct TidyDiag {
    name: String,
    file: String,
    offset: Option<usize>,
    reps: Vec<(String, usize, usize, String)>,
}

/// A replacement being read: file, offset, length, text.
type PendingRep = (String, Option<usize>, Option<usize>, Option<String>);

/// The diagnostics with replacements in clang-tidy's `--export-fixes` file (both the current
/// format with `DiagnosticMessage` and the older one with `Replacements` at the top level).
fn parse_tidy_yaml(yaml: &str) -> Vec<TidyDiag> {
    let mut out: Vec<TidyDiag> = Vec::new();
    let mut rep: Option<PendingRep> = None;
    let flush = |rep: &mut Option<PendingRep>, out: &mut Vec<TidyDiag>| {
        if let Some((file, Some(off), Some(len), text)) = rep.take()
            && let Some(d) = out.last_mut()
            && d.reps.len() < 1000
        {
            d.reps.push((file, off, len, text.unwrap_or_default()));
        }
    };
    let mut in_reps = false;
    for raw in yaml.lines().take(500_000) {
        let line = raw.trim_start();
        let item = line.strip_prefix("- ").map(str::trim_start);
        let body = item.unwrap_or(line);
        let Some((key, value)) = body.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "DiagnosticName" => {
                flush(&mut rep, &mut out);
                in_reps = false;
                out.push(TidyDiag { name: yaml_scalar(value), ..Default::default() });
            }
            "Replacements" => {
                flush(&mut rep, &mut out);
                in_reps = true;
            }
            "FilePath" if in_reps && item.is_some() => {
                flush(&mut rep, &mut out);
                rep = Some((yaml_scalar(value), None, None, None));
            }
            "FilePath" if !in_reps => {
                if let Some(d) = out.last_mut() {
                    d.file = yaml_scalar(value);
                }
            }
            "FileOffset" => {
                if let Some(d) = out.last_mut() {
                    d.offset = value.parse().ok();
                }
            }
            "Offset" => {
                if let Some(r) = rep.as_mut() {
                    r.1 = value.parse().ok();
                }
            }
            "Length" => {
                if let Some(r) = rep.as_mut() {
                    r.2 = value.parse().ok();
                }
            }
            "ReplacementText" => {
                if let Some(r) = rep.as_mut() {
                    r.3 = Some(yaml_scalar(value));
                }
            }
            "Level" | "BuildDirectory" | "Ranges" | "Notes" => {
                flush(&mut rep, &mut out);
                in_reps = false;
            }
            _ => {}
        }
    }
    flush(&mut rep, &mut out);
    out
}

/// A byte offset into raw file text → a char position (`\r\n` counts as one line break).
pub fn byte_offset_to_position(raw: &str, off: usize) -> Option<Position> {
    if off > raw.len() || !raw.is_char_boundary(off) {
        return None;
    }
    let before = raw.get(..off)?;
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before.get(start..)?.trim_end_matches('\r').chars().count();
    Some(Position::new(line, col))
}

/// Attach the fixes of clang-tidy's export to the findings they belong to (same check, file and
/// line).
pub(crate) fn attach_tidy_fixes(findings: &mut [Finding], yaml: &str, read: &dyn Fn(&Path) -> Option<String>) {
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for d in parse_tidy_yaml(yaml) {
        if d.reps.is_empty() {
            continue;
        }
        let mut changes = Vec::new();
        let mut ok = true;
        for (file, off, len, text) in &d.reps {
            let path = crate::normalize_path(Path::new(file));
            let raw = texts.entry(path.clone()).or_insert_with(|| read(&path));
            let (Some(raw), Some(end)) = (raw.as_deref(), off.checked_add(*len)) else {
                ok = false;
                break;
            };
            match (byte_offset_to_position(raw, *off), byte_offset_to_position(raw, end)) {
                (Some(s), Some(e)) => changes.push(TextChange { path, start: s, end: e, new_text: text.replace("\r\n", "\n") }),
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || changes.is_empty() {
            continue;
        }
        let diag_path = if d.file.is_empty() { changes.first().map(|c| c.path.clone()) } else { Some(crate::normalize_path(Path::new(&d.file))) };
        let Some(diag_path) = diag_path else { continue };
        let line = d
            .offset
            .and_then(|o| texts.get(&diag_path).cloned().flatten().and_then(|raw| byte_offset_to_position(&raw, o)))
            .map(|p| p.line);
        if let Some(f) = findings.iter_mut().find(|f| {
            f.path == diag_path && f.code.as_deref() == Some(d.name.as_str()) && line.is_none_or(|l| l == f.start.line)
        }) && f.fixes.is_empty()
        {
            f.fixes.push(Fix { title: format!("Apply fix for '{}'", d.name), applicability: Applicability::MachineApplicable, changes });
        }
    }
}

// ---------------------------------------------------------------- clangd

fn language_id(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("c") => "c",
        Some("m") => "objective-c",
        Some("mm") => "objective-cpp",
        _ => "cpp",
    }
}

fn severity_of(s: Option<lt::DiagnosticSeverity>) -> Severity {
    match s {
        Some(lt::DiagnosticSeverity::ERROR) => Severity::Error,
        Some(lt::DiagnosticSeverity::INFORMATION) => Severity::Info,
        Some(lt::DiagnosticSeverity::HINT) => Severity::Hint,
        _ => Severity::Warning,
    }
}

fn utf16_pos(doc: &Document, p: lt::Position) -> Position {
    doc.offset_to_position(doc.utf16_to_offset(p.line, p.character))
}

/// A clangd diagnostic → a finding (`None` for clangd's own notes without a code).
pub fn clangd_finding(path: &Path, doc: &Document, d: &lt::Diagnostic) -> Finding {
    let code = d.code.as_ref().map(|c| match c {
        lt::NumberOrString::Number(n) => n.to_string(),
        lt::NumberOrString::String(s) => s.clone(),
    });
    let severity = severity_of(d.severity);
    let start = utf16_pos(doc, d.range.start);
    let end = utf16_pos(doc, d.range.end).max(start);
    let inspection = match (&code, d.source.as_deref()) {
        (Some(c), _) => c.clone(),
        (None, _) if severity == Severity::Error => "Compiler errors".to_string(),
        (None, _) => "Compiler warnings".to_string(),
    };
    let mut f = Finding::new(Tool::Clangd, inspection, severity, path.to_path_buf(), start);
    f.end = end;
    f.message = d.message.chars().take(8192).collect();
    let mut desc = f.message.clone();
    for r in d.related_information.iter().flatten().take(20) {
        desc.push_str(&format!("\nnote: {}", r.message));
    }
    if let Some(src) = &d.source {
        desc.push_str(&format!("\n\n({src})"));
    }
    f.description = desc;
    f.url = d
        .code_description
        .as_ref()
        .map(|c| c.href.as_str().to_string())
        .or_else(|| code.as_deref().and_then(|c| docs_url(Tool::Clangd, c, severity)));
    f.code = code;
    f
}

/// Quick-fixes from a `textDocument/codeAction` answer (clangd's fixes for a diagnostic).
pub fn code_action_fixes(result: &Value, read: &dyn Fn(&Path) -> Option<Document>) -> Vec<Fix> {
    let mut out = Vec::new();
    for a in result.as_array().map(Vec::as_slice).unwrap_or(&[]).iter().take(20) {
        let Some(title) = a.get("title").and_then(Value::as_str) else { continue };
        let Some(edit) = a.get("edit") else { continue };
        if a.get("kind").and_then(Value::as_str).is_some_and(|k| !k.starts_with("quickfix")) {
            continue;
        }
        let mut per_file: Vec<(String, &Value)> = Vec::new();
        if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
            for (uri, edits) in changes {
                per_file.push((uri.clone(), edits));
            }
        }
        for dc in edit.get("documentChanges").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            if let (Some(uri), Some(edits)) = (dc.pointer("/textDocument/uri").and_then(Value::as_str), dc.get("edits")) {
                per_file.push((uri.to_string(), edits));
            }
        }
        let mut changes = Vec::new();
        let mut ok = !per_file.is_empty();
        for (uri, edits) in per_file {
            let Some(path) = uri.parse::<lt::Uri>().ok().and_then(|u| codecraft_lsp::uri_to_path(&u)) else {
                ok = false;
                break;
            };
            let Some(doc) = read(&path) else {
                ok = false;
                break;
            };
            for e in edits.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let Ok(te) = serde_json::from_value::<lt::TextEdit>(e.clone()) else {
                    ok = false;
                    break;
                };
                changes.push(TextChange {
                    path: path.clone(),
                    start: utf16_pos(&doc, te.range.start),
                    end: utf16_pos(&doc, te.range.end),
                    new_text: te.new_text,
                });
            }
        }
        if ok && !changes.is_empty() {
            let preferred = a.get("isPreferred").and_then(Value::as_bool).unwrap_or(false);
            let applicability = if preferred { Applicability::MachineApplicable } else { Applicability::MaybeIncorrect };
            out.push(Fix { title: title.to_string(), applicability, changes });
        }
    }
    out
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || std::fs::canonicalize(a).ok().zip(std::fs::canonicalize(b).ok()).is_some_and(|(x, y)| x == y)
}

/// Open the scope's C/C++ files in a background clangd and collect its diagnostics (and the
/// fixes of its code actions).
pub(crate) fn run_clangd(
    program: &Path,
    root: &Path,
    db_dir: Option<&Path>,
    files: &[PathBuf],
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<(ToolRun, Vec<Finding>), String> {
    let mut args: Vec<String> =
        ["--log=error", "--background-index=false", "--clang-tidy", "--pch-storage=memory", "-j=4"].map(String::from).to_vec();
    if let Some(d) = db_dir {
        args.push(format!("--compile-commands-dir={}", d.display()));
    }
    let command_line = format!("{} {}", program.display(), args.join(" "));
    let config = ServerConfig {
        command: program.to_path_buf(),
        args,
        root: root.to_path_buf(),
        initialization_options: None,
        configuration: None,
        env: Vec::new(),
    };
    progress("Starting clangd…");
    let (client, rx) = LspClient::spawn(config).map_err(|e| format!("clangd: {e}"))?;
    let result = drive_clangd(&client, &rx, files, cancel, progress);
    let _ = client.shutdown(Duration::from_secs(2));
    let (findings, timed_out) = result?;
    let mut outcome = inspections::count_label(findings.len());
    let opened = files.iter().filter(|f| is_c_family(f)).count().min(MAX_CLANGD_FILES);
    outcome.push_str(&format!(" in {opened} file(s)"));
    if timed_out > 0 {
        outcome.push_str(&format!(", {timed_out} file(s) timed out"));
    }
    if db_dir.is_none() {
        outcome.push_str(" (no compile_commands.json)");
    }
    Ok((ToolRun { tool: Tool::Clangd, command_line, outcome, ok: true }, findings))
}

fn drive_clangd(
    client: &LspClient,
    rx: &crossbeam_channel::Receiver<ServerEvent>,
    files: &[PathBuf],
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<(Vec<Finding>, usize), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    // The handshake.
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(inspections::CANCELLED.into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ServerEvent::Initialized(_)) => break,
            Ok(ServerEvent::Exited { code }) => {
                return Err(format!("clangd exited ({}): {}", code.map_or("?".into(), |c| c.to_string()), client.stderr_tail().join(" | ")));
            }
            Ok(ServerEvent::Error(e)) => return Err(format!("clangd: {e}")),
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Err("clangd stopped".into()),
        }
        if Instant::now() > deadline {
            return Err("clangd did not start within 60 seconds".into());
        }
    }
    let list: Vec<&PathBuf> = files.iter().filter(|f| is_c_family(f)).take(MAX_CLANGD_FILES).collect();
    let mut findings = Vec::new();
    let mut timed_out = 0usize;
    let mut actions = 0usize;
    let total = list.len();
    for (b, batch) in list.chunks(CLANGD_BATCH).enumerate() {
        let mut waiting: HashMap<PathBuf, Document> = HashMap::new();
        for f in batch {
            let Some(text) = inspections::read_normalized(f) else { continue };
            if client.did_open(f, language_id(f), 1, &text).is_ok() {
                waiting.insert((*f).clone(), Document::from_text(&text));
            }
        }
        progress(&format!("clangd: {}/{total} file(s)", (b * CLANGD_BATCH + batch.len()).min(total)));
        let batch_deadline = Instant::now() + Duration::from_secs(120);
        let mut got: Vec<(PathBuf, Document, Vec<lt::Diagnostic>)> = Vec::new();
        while !waiting.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                return Err(inspections::CANCELLED.into());
            }
            if Instant::now() > batch_deadline {
                timed_out += waiting.len();
                break;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ServerEvent::Diagnostics(p)) => {
                    let Some(path) = codecraft_lsp::uri_to_path(&p.uri) else { continue };
                    let key = waiting.keys().find(|k| same_file(k, &path)).cloned();
                    if let Some(key) = key
                        && let Some(doc) = waiting.remove(&key)
                    {
                        got.push((key, doc, p.diagnostics));
                    }
                }
                Ok(ServerEvent::Exited { .. }) => return Err(format!("clangd exited: {}", client.stderr_tail().join(" | "))),
                Ok(ServerEvent::ServerRequest { id, .. }) => {
                    let _ = client.respond(id, Ok(Value::Null));
                }
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Err("clangd stopped".into()),
            }
        }
        // Quick-fixes: ask for the code actions of each diagnostic while the file is open.
        for (path, doc, diags) in got {
            for d in diags.iter().take(500) {
                let mut f = clangd_finding(&path, &doc, d);
                if actions < MAX_CODE_ACTIONS && !cancel.load(Ordering::Relaxed) {
                    actions += 1;
                    if let Some(result) = request_actions(client, rx, &path, d) {
                        let read = |p: &Path| if same_file(p, &path) { Some(doc.clone()) } else { None };
                        f.fixes = code_action_fixes(&result, &read);
                        for fix in &mut f.fixes {
                            for c in &mut fix.changes {
                                if same_file(&c.path, &path) {
                                    c.path = path.clone();
                                }
                            }
                        }
                    }
                }
                findings.push(f);
            }
            let _ = client.did_close(&path);
        }
        for p in waiting.keys() {
            let _ = client.did_close(p);
        }
    }
    Ok((findings, timed_out))
}

fn request_actions(
    client: &LspClient,
    rx: &crossbeam_channel::Receiver<ServerEvent>,
    path: &Path,
    d: &lt::Diagnostic,
) -> Option<Value> {
    let id = client.code_action(path, d.range, vec![d.clone()]).ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(ServerEvent::Response { id: rid, result, .. }) if rid == id => return result.ok(),
            Ok(ServerEvent::Exited { .. }) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return None,
            _ => {}
        }
    }
    None
}

/// Drop C/C++ findings on lines with `// NOLINT` (all, or listing the rule) or after a line with
/// `// NOLINTNEXTLINE`, like clang-tidy: our NOLINT suppression works for every C/C++ analyzer.
pub(crate) fn honours_nolint(f: &Finding, text: &str) -> bool {
    if !text.contains("NOLINT") {
        return false;
    }
    // Only the problem's line and the one before it.
    let first = f.start.line.saturating_sub(1);
    let window: Vec<&str> = text.split('\n').skip(first).take(f.start.line - first + 1).collect();
    let lines = |l: usize| l.checked_sub(first).and_then(|i| window.get(i)).copied();
    let mentions = |l: &str, marker: &str| -> bool {
        let Some(i) = l.find(marker) else { return false };
        let rest = l.get(i + marker.len()..).unwrap_or("");
        match rest.strip_prefix('(') {
            Some(list) => {
                let list = list.split(')').next().unwrap_or("");
                let rule = f.rule();
                // `core.X` also as clang-tidy's `clang-analyzer-core.X`; `-Wfoo` as `clang-diagnostic-foo`.
                let tidy_name = rule.strip_prefix("-W").map(|w| format!("clang-diagnostic-{w}"));
                list.split(',').map(str::trim).filter(|c| !c.is_empty()).any(|c| {
                    c == "*" || c == rule || c.ends_with(&format!("-{rule}")) || tidy_name.as_deref() == Some(c)
                })
            }
            None => true,
        }
    };
    let here = lines(f.start.line).is_some_and(|l| {
        mentions(l, "NOLINT") && !l.contains("NOLINTNEXTLINE") && !l.contains("NOLINTBEGIN") && !l.contains("NOLINTEND")
    });
    let prev = f.start.line.checked_sub(1).and_then(lines).is_some_and(|l| mentions(l, "NOLINTNEXTLINE"));
    here || prev
}

/// Whether the scope has files for the C/C++ analyzers.
pub(crate) fn has_c_files(files: &[PathBuf]) -> bool {
    files.iter().any(|f| is_c_family(f))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(split_command(r#"cc -DX="a b" -I'my dir' a\ b.c "" -o x.o"#), ["cc", "-DX=a b", "-Imy dir", "a b.c", "", "-o", "x.o"]);
        assert_eq!(split_command("  "), Vec::<String>::new());
        assert_eq!(split_command(r#""unterminated"#), ["unterminated"]);
    }

    #[test]
    fn compile_db_and_analyzer_args() {
        let db = r#"[
            {"directory": "/w/build", "command": "/usr/bin/c++ -I/w/include -DDEBUG -std=c++17 -o CMakeFiles/app.dir/main.cpp.o -c /w/src/main.cpp", "file": "/w/src/main.cpp"},
            {"directory": "/w/build", "arguments": ["clang", "-MD", "-MF", "x.d", "-c", "../src/util.c", "-ofoo.o"], "file": "../src/util.c"},
            {"directory": "/w", "file": "x.c"},
            7
        ]"#;
        let cmds = parse_compile_db(db).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[1].file, PathBuf::from("/w/src/util.c"));
        let a = analyzer_args(&cmds[0], &["-Xclang".into(), "-analyzer-checker=security".into()]);
        assert_eq!(
            a,
            [
                "-I/w/include", "-DDEBUG", "-std=c++17", "--analyze", "--analyzer-output", "text", "-fno-color-diagnostics", "-o",
                null_device(), "-Xclang", "-analyzer-checker=security", "/w/src/main.cpp"
            ]
        );
        let a = analyzer_args(&cmds[1], &[]);
        assert_eq!(a.first().map(String::as_str), Some("--analyze"), "{a:?}");
        assert_eq!(a.last().map(String::as_str), Some("/w/src/util.c"));
        assert!(parse_compile_db("{").is_err());
        assert!(parse_compile_db("{}").is_err());
    }

    #[test]
    fn analyzer_output_with_notes() {
        let text = "/w/main.c:5:22: warning: Dereference of null pointer (loaded from variable 'p') [core.NullDereference]\n    5 |     if (!p) { return *p; }\n      |                      ^\n/w/main.c:12:14: note: Passing null pointer value via 1st parameter 'p'\n/w/main.c:11:9: warning: unused variable 'unused' [-Wunused-variable]\n2 warnings generated.\n";
        let f = parse_analyzer(text, Path::new("/w"), &|_| None);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].tool, Tool::ClangAnalyzer);
        assert_eq!(f[0].rule(), "core.NullDereference");
        assert_eq!(f[0].start, Position::new(4, 21));
        assert!(f[0].description.contains("note: Passing null pointer"));
        assert_eq!(f[1].rule(), "-Wunused-variable");
    }

    #[test]
    fn cppcheck_output() {
        let text = "/w/a.c:4:5: error: Null pointer dereference: p [nullPointer]\n/w/a.c:7:9: style: Variable 'x' is assigned a value that is never used. [unreadVariable]\nnofile:0:0: information: Active checkers: 1/2 [checkersReport]\n/w/b.c:2:1: information: Include file not found [missingInclude]\ngarbage\n";
        let f = parse_cppcheck(text, Path::new("/w"), &|_| None);
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].rule(), f[0].severity, f[0].start), ("nullPointer", Severity::Error, Position::new(3, 4)));
        assert_eq!(f[1].severity, Severity::Info, "style is a weak warning");
    }

    #[test]
    fn tidy_fixes_attach_to_findings() {
        let raw = "int main() {\r\n  int *p = NULL;\r\n  return 0;\r\n}\r\n";
        let off = raw.find("NULL").unwrap();
        let yaml = format!(
            "---\nMainSourceFile:  '/w/main.cpp'\nDiagnostics:\n  - DiagnosticName:  modernize-use-nullptr\n    DiagnosticMessage:\n      Message:         use nullptr\n      FilePath:        '/w/main.cpp'\n      FileOffset:      {off}\n      Replacements:\n        - FilePath:        '/w/main.cpp'\n          Offset:          {off}\n          Length:          4\n          ReplacementText: nullptr\n    Level:           Warning\n    BuildDirectory:  '/w/build'\n  - DiagnosticName:  misc-no-fix\n    DiagnosticMessage:\n      Message:         'it''s fine'\n      FilePath:        '/w/main.cpp'\n      FileOffset:      0\n      Replacements:    []\n...\n"
        );
        let mut f = Finding::new(Tool::ClangTidy, "modernize-use-nullptr", Severity::Warning, "/w/main.cpp".into(), Position::new(1, 11));
        f.code = Some("modernize-use-nullptr".into());
        let mut fs = vec![f];
        attach_tidy_fixes(&mut fs, &yaml, &|p| (p == Path::new("/w/main.cpp")).then(|| raw.to_string()));
        let fix = &fs[0].fixes[0];
        assert_eq!(fix.applicability, Applicability::MachineApplicable);
        assert_eq!((fix.changes[0].start, fix.changes[0].end, fix.changes[0].new_text.as_str()), (Position::new(1, 11), Position::new(1, 15), "nullptr"));
        assert_eq!(yaml_scalar("\"a\\nb\""), "a\nb");
        assert_eq!(yaml_scalar("'it''s'"), "it's");
        assert_eq!(byte_offset_to_position("aé\nb", 3), Some(Position::new(0, 2)));
        assert_eq!(byte_offset_to_position("aé", 2), None, "inside a char");
        assert_eq!(byte_offset_to_position("a", 9), None);
        // Junk is ignored.
        attach_tidy_fixes(&mut fs, "- DiagnosticName: x\n  Replacements:\n   - FilePath: '/nope'\n     Offset: 99999\n     Length: 1\n", &|_| None);
    }

    #[test]
    fn clangd_diagnostics_and_code_actions() {
        let doc = Document::from_text("int x = 😀y;\n");
        let d: lt::Diagnostic = serde_json::from_value(serde_json::json!({
            "range": {"start": {"line": 0, "character": 10}, "end": {"line": 0, "character": 11}},
            "severity": 1, "code": "undeclared_var_use", "source": "clang",
            "message": "use of undeclared identifier 'y'",
        }))
        .unwrap();
        let f = clangd_finding(Path::new("/w/a.cpp"), &doc, &d);
        assert_eq!((f.start, f.severity, f.rule()), (Position::new(0, 9), Severity::Error, "undeclared_var_use"));
        let actions = serde_json::json!([
            {"title": "change 'y' to 'x'", "kind": "quickfix", "isPreferred": true, "edit": {"changes": {"file:///w/a.cpp": [
                {"range": {"start": {"line": 0, "character": 10}, "end": {"line": 0, "character": 11}}, "newText": "x"}]}}},
            {"title": "not a fix", "kind": "refactor", "edit": {"changes": {}}},
            {"title": "no edit", "kind": "quickfix"}
        ]);
        let fixes = code_action_fixes(&actions, &|p| (p == Path::new("/w/a.cpp")).then(|| doc.clone()));
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].applicability, Applicability::MachineApplicable);
        assert_eq!(fixes[0].changes[0].start, Position::new(0, 9));
    }

    #[test]
    fn nolint_comments() {
        let mut f = Finding::new(Tool::ClangAnalyzer, "core.NullDereference", Severity::Warning, "/a.c".into(), Position::new(1, 0));
        f.code = Some("core.NullDereference".into());
        assert!(honours_nolint(&f, "x\n*p = 1; // NOLINT\n"));
        assert!(honours_nolint(&f, "x\n*p = 1; // NOLINT(core.NullDereference)\n"));
        assert!(honours_nolint(&f, "x\n*p = 1; // NOLINT(clang-analyzer-core.NullDereference)\n"));
        assert!(honours_nolint(&f, "x\n*p = 1; // NOLINT(bugprone-a, *)\n"));
        let mut w = f.clone();
        w.code = Some("-Wunused-variable".into());
        assert!(honours_nolint(&w, "x\nint y; // NOLINT(clang-diagnostic-unused-variable)\n"));
        assert!(!honours_nolint(&f, "x\n*p = 1; // NOLINT(bugprone-x)\n"));
        assert!(honours_nolint(&f, "// NOLINTNEXTLINE\n*p = 1;\n"));
        assert!(!honours_nolint(&f, "x\n*p = 1;\n"));
    }
}
