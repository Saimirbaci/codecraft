//! Other linters, run when installed and the scope has files for them: ruff (Python), ESLint
//! (JavaScript/TypeScript), clang-tidy (C/C++) and `zig fmt --check` (Zig).
//!
//! Coordinates: ruff reports 1-based char columns; ESLint 1-based UTF-16 columns and fix ranges
//! as UTF-16 offsets into the file; clang-tidy and zig 1-based byte columns. Everything becomes
//! zero-based char positions here (the file text is read to convert).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use regex::Regex;
use serde_json::Value;

use codecraft_text::Position;
use codecraft_workspace::Severity;

use super::{Applicability, Finding, Fix, JobSpec, ProcOutput, TextChange, Tool, docs_url, resolve_path};

/// Description text kept per finding.
const MAX_DESCRIPTION: usize = 8 * 1024;
/// Files `zig fmt` reformats to offer a fix (each runs `zig fmt --stdin`).
const MAX_ZIG_FIXES: usize = 200;
/// Largest input piped through a formatter.
const MAX_FORMAT_BYTES: usize = 8 << 20;

/// Arguments for a linter run over `paths`.
pub(crate) fn args(tool: Tool, spec: &JobSpec, paths: &[String], exts: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = match tool {
        Tool::Ruff => vec!["check".into(), "--output-format=json".into(), "--no-cache".into(), "--exit-zero".into()],
        Tool::Eslint => vec!["--format".into(), "json".into(), "--no-error-on-unmatched-pattern".into()],
        Tool::ClangTidy => Vec::new(),
        Tool::ZigFmt => vec!["fmt".into(), "--check".into()],
        Tool::Clippy | Tool::CargoCheck => Vec::new(),
        Tool::Mypy | Tool::Pyright => {
            return crate::python_lint::args(tool, paths, crate::python_env::project_python(&spec.root).as_deref());
        }
    };
    if tool == Tool::ClangTidy {
        // clang-tidy needs files, not directories.
        let files: Vec<String> = spec
            .scope
            .files
            .iter()
            .filter(|f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e)))
            .take(super::MAX_FILE_ARGS)
            .map(|p| p.display().to_string())
            .collect();
        a.extend(files);
        match compile_db_dir(&spec.root) {
            Some(dir) => a.extend(["-p".into(), dir.display().to_string()]),
            // No compilation database: compile with defaults instead of complaining.
            None => a.push("--".into()),
        }
        return a;
    }
    a.extend(paths.iter().cloned());
    a
}

fn compile_db_dir(root: &Path) -> Option<PathBuf> {
    ["", "build", "cmake-build-debug", "out"]
        .iter()
        .map(|d| if d.is_empty() { root.to_path_buf() } else { root.join(d) })
        .find(|d| d.join("compile_commands.json").is_file())
}

/// Findings from a finished linter, and why the run failed (when it did).
pub(crate) fn parse(
    tool: Tool,
    spec: &JobSpec,
    program: &Path,
    out: &ProcOutput,
    cancel: &AtomicBool,
) -> (Vec<Finding>, Option<String>) {
    let stdout = out.stdout.join("\n");
    let read = |p: &Path| codecraft_fs::read_text_file(p).ok();
    match tool {
        // ruff runs with --exit-zero: any other exit is a failure (bad config, crash).
        Tool::Ruff => match parse_ruff(&stdout, &spec.root) {
            Some(f) if out.success || !f.is_empty() => (f, None),
            _ => (Vec::new(), Some(out.stderr_summary())),
        },
        // ESLint exits 1 when it found problems and 2 on errors (no configuration, …).
        Tool::Eslint => match parse_eslint(&stdout, &spec.root, &read) {
            Some(f) if out.success || out.code == Some(1) || !f.is_empty() => (f, None),
            _ => (Vec::new(), Some(out.stderr_summary())),
        },
        Tool::ClangTidy => {
            let mut text = stdout.clone();
            text.push('\n');
            text.push_str(&out.stderr.join("\n"));
            let f = parse_clang_tidy(&text, &spec.root, &read);
            let failed = f.is_empty() && !out.success && out.code != Some(1);
            (f, failed.then(|| out.stderr_summary()))
        }
        Tool::ZigFmt => {
            let mut f = parse_zig_fmt(&stdout, &out.stderr.join("\n"), &spec.root, &read);
            let mut fixed = 0usize;
            for finding in f.iter_mut().filter(|f| f.severity == Severity::Info) {
                if fixed >= MAX_ZIG_FIXES || cancel.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(text) = super::read_normalized(&finding.path)
                    && let Ok(formatted) = pipe_through(program, &["fmt", "--stdin"], &spec.root, &text)
                    && formatted != text
                {
                    finding.fixes.push(Fix {
                        title: "Reformat file with zig fmt".into(),
                        applicability: Applicability::MachineApplicable,
                        changes: vec![whole_file_change(&finding.path, &text, formatted)],
                    });
                    fixed += 1;
                }
            }
            (f, None)
        }
        Tool::Clippy | Tool::CargoCheck => (Vec::new(), None),
        Tool::Mypy | Tool::Pyright => {
            crate::python_lint::parse(tool, &spec.root, &stdout, out.code, out.success, &|| out.stderr_summary())
        }
    }
}

/// A change replacing all of `text`.
pub fn whole_file_change(path: &Path, text: &str, new_text: String) -> TextChange {
    let lines = text.split('\n').count();
    let last = text.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0);
    TextChange {
        path: path.to_path_buf(),
        start: Position::new(0, 0),
        end: Position::new(lines.saturating_sub(1), last),
        new_text,
    }
}

/// Pipe `input` through a formatter (`zig fmt --stdin`, `ruff format -`) and return its stdout.
pub(crate) fn pipe_through(program: &Path, args: &[&str], cwd: &Path, input: &str) -> Result<String, String> {
    if input.len() > MAX_FORMAT_BYTES {
        return Err("file too large to format".into());
    }
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if cwd.is_dir() {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", program.display()))?;
    // Write on another thread: the formatter may fill stdout before reading all of stdin.
    let writer = child.stdin.take().map(|mut stdin| {
        let input = input.to_string();
        std::thread::spawn(move || stdin.write_all(input.as_bytes()))
    });
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let written = writer.map(|w| w.join().map_err(|_| "formatter stdin writer failed".to_string()));
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().lines().take(3).collect::<Vec<_>>().join(" | ");
        return Err(if err.is_empty() { format!("exited with {}", output.status) } else { err });
    }
    if let Some(Ok(Err(e))) = written {
        return Err(e.to_string());
    }
    String::from_utf8(output.stdout).map_err(|_| "formatter output is not UTF-8".to_string())
}

// ---------------------------------------------------------------- ruff

fn num(v: &Value, key: &str) -> Option<usize> {
    v.get(key).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok())
}

/// `{row, column}` (1-based, char column) → zero-based position.
fn ruff_pos(v: Option<&Value>) -> Option<Position> {
    let v = v?;
    let (row, col) = (num(v, "row")?, num(v, "column")?);
    (row > 0 && col > 0).then(|| Position::new(row - 1, col - 1))
}

/// `ruff check --output-format=json` (a JSON array). `None` when the output is not ruff's JSON.
pub fn parse_ruff(json: &str, base: &Path) -> Option<Vec<Finding>> {
    let json = json.trim();
    if json.is_empty() {
        return Some(Vec::new());
    }
    let items: Vec<Value> = serde_json::from_str(json).ok()?;
    let mut out = Vec::new();
    for it in &items {
        let Some(file) = it.get("filename").and_then(Value::as_str) else { continue };
        let Some(start) = ruff_pos(it.get("location")) else { continue };
        let end = ruff_pos(it.get("end_location")).filter(|e| *e >= start).unwrap_or(start);
        let code = it.get("code").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string);
        let url = it.get("url").and_then(Value::as_str).map(str::to_string);
        let rule = url.as_deref().and_then(|u| u.rsplit('/').next()).filter(|r| !r.is_empty() && !r.contains('#'));
        let inspection = match (&code, rule) {
            (Some(c), Some(r)) => format!("{c} {r}"),
            (Some(c), None) => c.clone(),
            (None, _) => "Syntax errors".to_string(),
        };
        let severity = if code.is_none() || code.as_deref() == Some("E999") { Severity::Error } else { Severity::Warning };
        let path = resolve_path(base, file);
        let mut f = Finding::new(Tool::Ruff, inspection, severity, path.clone(), start);
        f.end = end;
        f.message = it.get("message").and_then(Value::as_str).unwrap_or("").to_string();
        f.description = f.message.clone();
        f.url = url.or_else(|| code.as_deref().and_then(|c| docs_url(Tool::Ruff, c, severity)));
        f.code = code;
        if let Some(fix) = it.get("fix").filter(|v| v.is_object()) {
            let applicability = fix.get("applicability").and_then(Value::as_str).map_or(Applicability::Unspecified, Applicability::parse);
            let title = fix.get("message").and_then(Value::as_str).unwrap_or("Apply fix").to_string();
            let edits = fix.get("edits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            let changes: Option<Vec<TextChange>> = edits
                .iter()
                .map(|e| {
                    let start = ruff_pos(e.get("location"))?;
                    let end = ruff_pos(e.get("end_location"))?;
                    (end >= start).then(|| TextChange {
                        path: path.clone(),
                        start,
                        end,
                        new_text: e.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
                    })
                })
                .collect();
            if let Some(changes) = changes.filter(|c| !c.is_empty()) {
                f.fixes.push(Fix { title, applicability, changes });
            }
        }
        f.description = format!("{}\n\nRule: {}", f.message, f.inspection);
        out.push(f);
    }
    Some(out)
}

// ---------------------------------------------------------------- ESLint

/// UTF-16 offset in raw text → zero-based (line, char column). `\r\n` counts as one line break.
fn utf16_offset_to_position(text: &str, offset: usize) -> Option<Position> {
    let (mut line, mut col, mut units) = (0usize, 0usize, 0usize);
    let mut chars = text.chars().peekable();
    while units < offset {
        let c = chars.next()?;
        units = units.saturating_add(c.len_utf16());
        match c {
            '\n' => {
                line += 1;
                col = 0;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            _ => col += 1,
        }
    }
    (units == offset).then_some(Position::new(line, col))
}

/// 1-based line and UTF-16 column → zero-based char position on that line of `text`.
fn utf16_line_col(text: Option<&str>, line: usize, col16: usize) -> Option<Position> {
    if line == 0 {
        return None;
    }
    let line = line - 1;
    let col16 = col16.saturating_sub(1);
    let column = match text.and_then(|t| t.split('\n').nth(line)) {
        Some(l) => crate::convert::utf16_col_to_char(l.trim_end_matches('\r'), u32::try_from(col16).unwrap_or(u32::MAX)),
        None => col16,
    };
    Some(Position::new(line, column))
}

fn eslint_fix(v: &Value, text: Option<&str>, path: &Path) -> Option<TextChange> {
    let range = v.get("range")?.as_array()?;
    let (a, b) = (range.first()?.as_u64()?, range.get(1)?.as_u64()?);
    let text = text?;
    let start = utf16_offset_to_position(text, usize::try_from(a).ok()?)?;
    let end = utf16_offset_to_position(text, usize::try_from(b).ok()?)?;
    (end >= start).then(|| TextChange {
        path: path.to_path_buf(),
        start,
        end,
        new_text: v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
    })
}

/// `eslint --format json`. `read` returns a file's raw text (for column and fix-range conversion).
pub fn parse_eslint(json: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Option<Vec<Finding>> {
    let json = json.trim();
    if json.is_empty() {
        return Some(Vec::new());
    }
    let files: Vec<Value> = serde_json::from_str(json).ok()?;
    let mut out = Vec::new();
    for file in &files {
        let Some(name) = file.get("filePath").and_then(Value::as_str) else { continue };
        let path = resolve_path(base, name);
        let messages = file.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        if messages.is_empty() {
            continue;
        }
        let text = file.get("source").and_then(Value::as_str).map(str::to_string).or_else(|| read(&path));
        for m in messages {
            let rule = m.get("ruleId").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_string);
            let fatal = m.get("fatal").and_then(Value::as_bool).unwrap_or(false);
            let severity = match (fatal, num(m, "severity")) {
                (true, _) | (_, Some(2)) => Severity::Error,
                (_, Some(1)) => Severity::Warning,
                _ => Severity::Info,
            };
            let Some(start) = utf16_line_col(text.as_deref(), num(m, "line").unwrap_or(1), num(m, "column").unwrap_or(1))
            else {
                continue;
            };
            let end = match (num(m, "endLine"), num(m, "endColumn")) {
                (Some(l), Some(c)) => utf16_line_col(text.as_deref(), l, c).filter(|e| *e >= start).unwrap_or(start),
                _ => start,
            };
            let inspection = rule.clone().unwrap_or_else(|| "Parsing errors".to_string());
            let mut f = Finding::new(Tool::Eslint, inspection, severity, path.clone(), start);
            f.end = end;
            f.message = m.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            f.description = f.message.clone();
            f.url = rule.as_deref().and_then(|r| docs_url(Tool::Eslint, r, severity));
            f.code = rule.clone();
            if let Some(change) = m.get("fix").and_then(|v| eslint_fix(v, text.as_deref(), &path)) {
                let title = format!("Fix '{}'", rule.as_deref().unwrap_or("problem"));
                f.fixes.push(Fix { title, applicability: Applicability::MachineApplicable, changes: vec![change] });
            }
            for s in m.get("suggestions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(change) = s.get("fix").and_then(|v| eslint_fix(v, text.as_deref(), &path)) {
                    let title = s.get("desc").and_then(Value::as_str).unwrap_or("Apply suggestion").to_string();
                    f.fixes.push(Fix { title, applicability: Applicability::MaybeIncorrect, changes: vec![change] });
                }
            }
            out.push(f);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- clang-tidy and zig

fn diag_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.+?):(\d+):(\d+): (fatal error|error|warning|note): (.*?)(?: \[([^\]\s]+)\])?\s*$").ok())
        .as_ref()
}

/// 1-based byte column on a line → zero-based char column.
fn byte_col_to_char(text: Option<&str>, line: usize, col: usize) -> usize {
    let col = col.saturating_sub(1);
    let Some(l) = text.and_then(|t| t.split('\n').nth(line)) else { return col };
    let mut b = col.min(l.len());
    while b > 0 && !l.is_char_boundary(b) {
        b -= 1;
    }
    l.get(..b).map(|p| p.chars().count()).unwrap_or(col)
}

/// `file:line:col: severity: message [check]` lines (clang-tidy, zig). Notes become part of the
/// preceding finding's description; source excerpts and carets are skipped.
fn parse_diagnostic_lines(text: &str, base: &Path, tool: Tool, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    let Some(re) = diag_re() else { return Vec::new() };
    let mut out: Vec<Finding> = Vec::new();
    let mut cache: std::collections::HashMap<PathBuf, Option<String>> = std::collections::HashMap::new();
    for raw in text.lines() {
        let line = crate::runs::strip_ansi(raw);
        let Some(c) = re.captures(&line) else { continue };
        let (Some(file), Some(ln), Some(col), Some(level), Some(msg)) = (c.get(1), c.get(2), c.get(3), c.get(4), c.get(5))
        else {
            continue;
        };
        let (Ok(ln), Ok(col)) = (ln.as_str().parse::<usize>(), col.as_str().parse::<usize>()) else { continue };
        if ln == 0 {
            continue;
        }
        if level.as_str() == "note" {
            if let Some(prev) = out.last_mut()
                && prev.description.len() < MAX_DESCRIPTION
            {
                prev.description.push_str(&format!("\nnote: {}", msg.as_str()));
            }
            continue;
        }
        let path = resolve_path(base, file.as_str());
        let text = cache.entry(path.clone()).or_insert_with(|| read(&path));
        let column = byte_col_to_char(text.as_deref(), ln - 1, col);
        let severity = if level.as_str().contains("error") { Severity::Error } else { Severity::Warning };
        let check = c.get(6).map(|m| m.as_str().to_string());
        let inspection = match (&check, tool) {
            (Some(ch), _) => ch.clone(),
            (None, Tool::ZigFmt) => "Zig parse errors".to_string(),
            (None, _) if severity == Severity::Error => "Compiler errors".to_string(),
            (None, _) => "Compiler warnings".to_string(),
        };
        let mut f = Finding::new(tool, inspection, severity, path, Position::new(ln - 1, column));
        f.message = msg.as_str().to_string();
        f.description = f.message.clone();
        f.url = check.as_deref().and_then(|ch| docs_url(tool, ch, severity));
        f.code = check;
        out.push(f);
    }
    out
}

/// clang-tidy's text output.
pub fn parse_clang_tidy(text: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    parse_diagnostic_lines(text, base, Tool::ClangTidy, read)
}

/// `zig fmt --check`: stdout lists unformatted files; stderr has parse errors.
pub fn parse_zig_fmt(stdout: &str, stderr: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    let mut out = parse_diagnostic_lines(stderr, base, Tool::ZigFmt, read);
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut f = Finding::new(Tool::ZigFmt, "zig fmt", Severity::Info, resolve_path(base, line), Position::new(0, 0));
        f.message = "File is not formatted according to zig fmt".into();
        f.description = "`zig fmt --check` lists this file: its formatting differs from the canonical Zig style.".into();
        out.push(f);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const RUFF: &str = include_str!("fixtures/ruff.json");

    #[test]
    fn ruff_json() {
        let fs = parse_ruff(RUFF, Path::new("/work/py")).unwrap();
        assert_eq!(fs.len(), 5);
        let f401 = &fs[0];
        assert_eq!(f401.inspection, "F401 unused-import");
        assert_eq!(f401.code.as_deref(), Some("F401"));
        assert_eq!(f401.path, PathBuf::from("/work/py/app.py"));
        assert_eq!((f401.start, f401.end), (Position::new(0, 7), Position::new(0, 9)));
        assert_eq!(f401.url.as_deref(), Some("https://docs.astral.sh/ruff/rules/unused-import"));
        let fix = &f401.fixes[0];
        assert_eq!(fix.title, "Remove unused import: `os`");
        assert_eq!(fix.applicability, Applicability::MachineApplicable);
        assert_eq!((fix.changes[0].start, fix.changes[0].end), (Position::new(0, 0), Position::new(1, 0)));
        // `y` after a non-ASCII string: char columns.
        let y = &fs[3];
        assert_eq!(y.message, "Local variable `y` is assigned to but never used");
        assert_eq!(y.start, Position::new(4, 17));
        assert_eq!(y.fixes[0].applicability, Applicability::MaybeIncorrect);
        assert!(fs[2].fixes.is_empty());
        assert_eq!(parse_ruff("", Path::new("/w")), Some(Vec::new()));
        assert_eq!(parse_ruff("error: not json", Path::new("/w")), None);
        let junk = r#"[{"filename":"a.py","location":{"row":0,"column":1}},{"location":{"row":1}},7,{"filename":"b.py","location":{"row":2,"column":3},"code":null,"fix":{"edits":[{"location":{"row":3,"column":1},"end_location":{"row":2,"column":1}}]}}]"#;
        let fs = parse_ruff(junk, Path::new("/w")).unwrap();
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].severity, Severity::Error);
        assert!(fs[0].fixes.is_empty());
    }

    const ESLINT: &str = r#"[{"filePath":"/work/web/src/a.js","messages":[{"ruleId":"no-unused-vars","severity":2,"message":"'x' is assigned a value but never used.","line":1,"column":11,"nodeType":"Identifier","messageId":"unusedVar","endLine":1,"endColumn":12},{"ruleId":"semi","severity":1,"message":"Missing semicolon.","line":2,"column":12,"nodeType":"ExpressionStatement","messageId":"missingSemi","endLine":3,"endColumn":1,"fix":{"range":[30,30],"text":";"}},{"ruleId":"no-useless-escape","severity":2,"message":"Unnecessary escape character: \\..","line":2,"column":9,"suggestions":[{"messageId":"removeEscape","fix":{"range":[27,28],"text":""},"desc":"Remove the `\\`."}]}],"suppressedMessages":[],"errorCount":2,"warningCount":1},{"filePath":"/work/web/src/b.ts","messages":[{"ruleId":null,"fatal":true,"severity":2,"message":"Parsing error: Unexpected token","line":1,"column":5}]},{"filePath":"/work/web/src/clean.js","messages":[]}]"#;

    #[test]
    fn eslint_json_with_utf16_columns_and_fix_ranges() {
        // Line 1 has an astral emoji (two UTF-16 units) before `x`; CRLF line endings.
        let a = "/*😀*/let x = 1;\r\nlet s = '\\.'\r\n";
        let read = |p: &Path| (p == Path::new("/work/web/src/a.js")).then(|| a.to_string());
        let fs = parse_eslint(ESLINT, Path::new("/work/web"), &read).unwrap();
        assert_eq!(fs.len(), 4);
        let unused = &fs[0];
        assert_eq!(unused.inspection, "no-unused-vars");
        assert_eq!(unused.severity, Severity::Error);
        // UTF-16 column 11 → char column 9 (the emoji is one char, two UTF-16 units).
        assert_eq!((unused.start, unused.end), (Position::new(0, 9), Position::new(0, 10)));
        assert_eq!(unused.url.as_deref(), Some("https://eslint.org/docs/latest/rules/no-unused-vars"));
        let semi = &fs[1];
        assert_eq!(semi.severity, Severity::Warning);
        // Offset 30 (UTF-16, counting `\r`) is the end of line 2.
        assert_eq!(semi.fixes[0].changes[0].start, Position::new(1, 12));
        assert_eq!(semi.fixes[0].changes[0].new_text, ";");
        assert_eq!(semi.fixes[0].applicability, Applicability::MachineApplicable);
        let esc = &fs[2];
        assert_eq!(esc.fixes[0].applicability, Applicability::MaybeIncorrect);
        assert_eq!(esc.fixes[0].title, "Remove the `\\`.");
        assert_eq!((esc.fixes[0].changes[0].start, esc.fixes[0].changes[0].end), (Position::new(1, 9), Position::new(1, 10)));
        let fatal = &fs[3];
        assert_eq!(fatal.inspection, "Parsing errors");
        assert_eq!(fatal.severity, Severity::Error);
        assert_eq!(fatal.code, None);
        assert_eq!(parse_eslint("Oops! Something went wrong!", Path::new("/w"), &read), None);
    }

    #[test]
    fn utf16_offsets() {
        let t = "a😀b\r\ncd";
        assert_eq!(utf16_offset_to_position(t, 0), Some(Position::new(0, 0)));
        assert_eq!(utf16_offset_to_position(t, 3), Some(Position::new(0, 2)));
        assert_eq!(utf16_offset_to_position(t, 2), None, "inside a surrogate pair");
        assert_eq!(utf16_offset_to_position(t, 6), Some(Position::new(1, 0)));
        assert_eq!(utf16_offset_to_position(t, 8), Some(Position::new(1, 2)));
        assert_eq!(utf16_offset_to_position(t, 99), None);
    }

    const CLANG_TIDY: &str = "3 warnings generated.\n/work/c/main.c:4:13: warning: Value stored to 'y' during its initialization is never read [clang-analyzer-deadcode.DeadStores]\n    4 |     int y = x * 2;\n      |         ^\n/work/c/main.c:4:13: note: Value stored to 'y' during its initialization is never read\n/work/c/main.c:6:5: warning: 'strcpy' is insecure [clang-analyzer-security.insecureAPI.strcpy]\nsrc/util.cpp:2:10: error: 'missing.h' file not found [clang-diagnostic-error]\nSuppressed 10 warnings (10 in non-user code).\n";

    #[test]
    fn clang_tidy_text() {
        let main = "#include <string.h>\nint main(void) {\n    int x = 1;\n    /*é*/int y = x * 2;\n    char b[4];\n    strcpy(b, \"abc\");\n}\n";
        let read = |p: &Path| (p == Path::new("/work/c/main.c")).then(|| main.to_string());
        let fs = parse_clang_tidy(CLANG_TIDY, Path::new("/work/c"), &read);
        assert_eq!(fs.len(), 3);
        assert_eq!(fs[0].inspection, "clang-analyzer-deadcode.DeadStores");
        // Byte column 13 on a line with `é` (2 bytes) → char column 11.
        assert_eq!(fs[0].start, Position::new(3, 11));
        assert!(fs[0].description.contains("note: Value stored"));
        assert_eq!(fs[0].url.as_deref(), Some("https://clang.llvm.org/docs/analyzer/checkers.html"));
        assert_eq!(fs[2].path, PathBuf::from("/work/c/src/util.cpp"));
        assert_eq!(fs[2].severity, Severity::Error);
        assert_eq!(fs[2].url, None);
    }

    #[test]
    fn zig_fmt_check() {
        let fs = parse_zig_fmt(
            "a.zig\nsrc/b.zig\n",
            "bad.zig:1:11: error: expected expression, found ';'\nconst x = ;\n          ^\n",
            Path::new("/work/z"),
            &|_| None,
        );
        assert_eq!(fs.len(), 3);
        assert_eq!(fs[0].inspection, "Zig parse errors");
        assert_eq!(fs[0].start, Position::new(0, 10));
        assert_eq!(fs[1].inspection, "zig fmt");
        assert_eq!(fs[1].severity, Severity::Info);
        assert_eq!(fs[2].path, PathBuf::from("/work/z/src/b.zig"));
    }

    #[test]
    fn whole_file_changes_cover_the_text() {
        let c = whole_file_change(Path::new("/a"), "ab\ncdé", "x".into());
        assert_eq!((c.start, c.end), (Position::new(0, 0), Position::new(1, 3)));
        let c = whole_file_change(Path::new("/a"), "ab\n", "x".into());
        assert_eq!(c.end, Position::new(1, 0));
    }

    fn spec() -> JobSpec {
        use crate::inspections::{JobKind, ResolvedScope, Scope, ScopeKind};
        JobSpec {
            kind: JobKind::Inspect,
            scope: ResolvedScope {
                scope: Scope { kind: ScopeKind::Project, path: None },
                label: String::new(),
                files: Vec::new(),
                only: None,
            },
            root: "/w".into(),
            cargo_root: None,
            use_clippy: true,
            tools: None,
            editions: Vec::new(),
            programs: std::collections::HashMap::new(),
            format: false,
        }
    }

    #[test]
    fn tool_failures_are_reported() {
        let cancel = AtomicBool::new(false);
        let failed = ProcOutput {
            stdout: Vec::new(),
            stderr: vec!["Oops! Something went wrong!".into(), "ESLint couldn't find a configuration file.".into()],
            success: false,
            code: Some(2),
        };
        let (f, why) = parse(Tool::Eslint, &spec(), Path::new("eslint"), &failed, &cancel);
        assert!(f.is_empty());
        assert!(why.unwrap().contains("configuration file"));
        let (_, why) = parse(Tool::Ruff, &spec(), Path::new("ruff"), &failed, &cancel);
        assert!(why.is_some());
        let found = ProcOutput { stdout: vec!["[]".into()], stderr: Vec::new(), success: false, code: Some(1) };
        assert_eq!(parse(Tool::Eslint, &spec(), Path::new("eslint"), &found, &cancel), (Vec::new(), None));
        let clean = ProcOutput { stdout: Vec::new(), stderr: Vec::new(), success: true, code: Some(0) };
        assert_eq!(parse(Tool::Ruff, &spec(), Path::new("ruff"), &clean, &cancel), (Vec::new(), None));
        let tidy = ProcOutput { stdout: Vec::new(), stderr: vec!["error: unable to handle compilation".into()], success: false, code: Some(3) };
        assert!(parse(Tool::ClangTidy, &spec(), Path::new("clang-tidy"), &tidy, &cancel).1.is_some());
    }

    #[test]
    fn byte_columns() {
        assert_eq!(byte_col_to_char(Some("é=1"), 0, 3), 1);
        assert_eq!(byte_col_to_char(Some("é=1"), 0, 2), 0, "inside a char rounds down");
        assert_eq!(byte_col_to_char(Some("ab"), 0, 99), 2);
        assert_eq!(byte_col_to_char(None, 5, 4), 3);
    }
}
