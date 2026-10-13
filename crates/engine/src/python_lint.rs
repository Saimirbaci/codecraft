//! mypy and pyright (or basedpyright) in Code › Inspect Code…, when installed (in the project's
//! virtualenv or on `PATH`). Both check against the project's selected interpreter.
//!
//! Coordinates: mypy prints 1-based line and char columns (`--show-column-numbers
//! --show-error-end`); pyright's JSON has zero-based lines and UTF-16 characters, converted
//! through the file's text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use codecraft_text::{Document, Position};
use codecraft_workspace::Severity;

use crate::inspections::{Finding, Tool, docs_url, resolve_path};

/// Description text kept per finding (mypy notes are appended).
const MAX_DESCRIPTION: usize = 8 * 1024;

/// The program for mypy or pyright: next to the project's interpreter, else on `PATH`
/// (basedpyright counts as pyright).
pub(crate) fn find(root: &Path, tool: Tool) -> Option<PathBuf> {
    match tool {
        Tool::Mypy => crate::python_env::python_tool(root, "mypy"),
        Tool::Pyright => crate::python_env::python_tool(root, "basedpyright")
            .or_else(|| crate::python_env::python_tool(root, "pyright")),
        _ => None,
    }
}

/// Arguments for a run over `paths`, checking against `python` when the project has one.
pub(crate) fn args(tool: Tool, paths: &[String], python: Option<&Path>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    match tool {
        Tool::Mypy => {
            a.extend(
                [
                    "--show-column-numbers",
                    "--show-error-codes",
                    "--show-error-end",
                    "--no-error-summary",
                    "--no-pretty",
                    "--no-color-output",
                    "--hide-error-context",
                    "--show-absolute-path",
                ]
                .map(String::from),
            );
            if let Some(py) = python {
                a.extend(["--python-executable".into(), py.display().to_string()]);
            }
        }
        Tool::Pyright => {
            a.push("--outputjson".into());
            if let Some(py) = python {
                a.extend(["--pythonpath".into(), py.display().to_string()]);
            }
        }
        _ => {}
    }
    a.extend(paths.iter().cloned());
    a
}

fn mypy_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<file>.+?):(?P<line>\d+):(?P<col>\d+)(?::(?P<eline>\d+):(?P<ecol>\d+))?: (?P<sev>error|warning|note): (?P<msg>.*?)(?:\s+\[(?P<code>[A-Za-z0-9_-]+)\])?\s*$",
        )
        .ok()
    })
    .as_ref()
}

/// Findings from mypy's text output.
pub(crate) fn parse_mypy(stdout: &str, base: &Path) -> Vec<Finding> {
    let Some(re) = mypy_regex() else { return Vec::new() };
    let mut out: Vec<Finding> = Vec::new();
    for line in stdout.lines() {
        let Some(c) = re.captures(line) else { continue };
        let num = |k: &str| c.name(k).and_then(|m| m.as_str().parse::<usize>().ok());
        let (Some(file), Some(l), Some(col)) = (c.name("file"), num("line"), num("col")) else { continue };
        let msg = c.name("msg").map(|m| m.as_str()).unwrap_or("").to_string();
        let path = resolve_path(base, file.as_str());
        let start = Position::new(l.saturating_sub(1), col.saturating_sub(1));
        if c.name("sev").is_some_and(|s| s.as_str() == "note") {
            // Notes explain the error before them.
            if let Some(prev) = out.last_mut().filter(|p| p.path == path && p.start.line == start.line)
                && prev.description.len() < MAX_DESCRIPTION
            {
                if !prev.description.is_empty() {
                    prev.description.push('\n');
                }
                prev.description.push_str(&msg);
            }
            continue;
        }
        let code = c.name("code").map(|m| m.as_str().to_string());
        let severity =
            if c.name("sev").is_some_and(|s| s.as_str() == "warning") { Severity::Warning } else { Severity::Error };
        let mut f = Finding::new(Tool::Mypy, code.clone().unwrap_or_else(|| "mypy".into()), severity, path, start);
        f.end = match (num("eline"), num("ecol")) {
            (Some(el), Some(ec)) if el >= l => Position::new(el.saturating_sub(1), ec),
            _ => start,
        };
        if f.end == f.start {
            f.end = Position::new(start.line, start.column.saturating_add(1));
        }
        f.message = msg;
        f.url = code.as_deref().and_then(|c| docs_url(Tool::Mypy, c, severity));
        f.code = code;
        out.push(f);
    }
    out
}

/// Findings from `pyright --outputjson`; `None` when the output is not pyright's report.
pub(crate) fn parse_pyright(stdout: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Option<Vec<Finding>> {
    let start = stdout.find('{')?;
    let v: Value = serde_json::from_str(stdout.get(start..)?).ok()?;
    let diags = v.get("generalDiagnostics")?.as_array()?;
    let mut docs: HashMap<PathBuf, Option<Document>> = HashMap::new();
    let mut out = Vec::new();
    for d in diags {
        let Some(file) = d.get("file").and_then(Value::as_str) else { continue };
        let path = resolve_path(base, file);
        let severity = match d.get("severity").and_then(Value::as_str) {
            Some("error") => Severity::Error,
            Some("warning") => Severity::Warning,
            _ => Severity::Info,
        };
        let pos = |key: &str| -> Option<(u32, u32)> {
            let p = d.get("range")?.get(key)?;
            let line = u32::try_from(p.get("line")?.as_u64()?).ok()?;
            let ch = u32::try_from(p.get("character")?.as_u64()?).ok()?;
            Some((line, ch))
        };
        let doc = docs.entry(path.clone()).or_insert_with(|| read(&path).map(|t| Document::from_text(&t)));
        let convert = |p: Option<(u32, u32)>| -> Position {
            match (p, doc.as_ref()) {
                (Some((l, c)), Some(doc)) => doc.offset_to_position(doc.utf16_to_offset(l, c)),
                (Some((l, c)), None) => Position::new(l as usize, c as usize),
                (None, _) => Position::new(0, 0),
            }
        };
        let start = convert(pos("start"));
        let end = convert(pos("end").or(pos("start")));
        let rule = d.get("rule").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned);
        let mut f =
            Finding::new(Tool::Pyright, rule.clone().unwrap_or_else(|| "pyright".into()), severity, path, start);
        f.end = end;
        f.message = d.get("message").and_then(Value::as_str).unwrap_or("").chars().take(MAX_DESCRIPTION).collect();
        f.url = rule.as_deref().and_then(|r| docs_url(Tool::Pyright, r, severity));
        f.code = rule;
        out.push(f);
    }
    Some(out)
}

/// Findings of a finished run and why it failed (when it did). mypy exits 1 and pyright 1 when
/// they found errors; anything above is a failure (bad configuration, crash).
pub(crate) fn parse(
    tool: Tool,
    base: &Path,
    stdout: &str,
    code: Option<i32>,
    success: bool,
    stderr_summary: &dyn Fn() -> String,
) -> (Vec<Finding>, Option<String>) {
    let read = |p: &Path| codecraft_fs::read_text_file(p).ok();
    let ok_exit = success || code == Some(1);
    match tool {
        Tool::Mypy => {
            let f = parse_mypy(stdout, base);
            if ok_exit || !f.is_empty() { (f, None) } else { (f, Some(stderr_summary())) }
        }
        Tool::Pyright => match parse_pyright(stdout, base, &read) {
            Some(f) if ok_exit || !f.is_empty() => (f, None),
            _ => (Vec::new(), Some(stderr_summary())),
        },
        _ => (Vec::new(), None),
    }
}

/// The comment that suppresses a finding on its line.
pub(crate) fn suppress_comment(tool: Tool, code: &str) -> Option<String> {
    match tool {
        Tool::Mypy => Some(format!("  # type: ignore[{code}]")),
        Tool::Pyright => Some(format!("  # pyright: ignore[{code}]")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mypy_output() {
        let out = "/p/a.py:3:5:3:9: error: Incompatible types in assignment (expression has type \"str\", variable has type \"int\")  [assignment]\n\
                   /p/a.py:3:5: note: See https://mypy.rtfd.io\n\
                   b.py:10:1: error: Cannot find implementation or library stub for module named \"yaml\"  [import-not-found]\n\
                   b.py:12:2: warning: unused 'type: ignore' comment\n\
                   garbage line\n\
                   c.py:0:0: error: x  [misc]";
        let f = parse_mypy(out, Path::new("/p"));
        assert_eq!(f.len(), 4);
        assert_eq!(f[0].path, Path::new("/p/a.py"));
        assert_eq!((f[0].start, f[0].end), (Position::new(2, 4), Position::new(2, 9)));
        assert_eq!(f[0].code.as_deref(), Some("assignment"));
        assert_eq!(f[0].description, "See https://mypy.rtfd.io");
        assert_eq!(f[1].path, Path::new("/p/b.py"));
        assert_eq!(crate::python::unresolved_import(&f[1].message).as_deref(), Some("yaml"));
        assert_eq!((f[2].severity, f[2].code.as_deref(), f[2].inspection.as_str()), (Severity::Warning, None, "mypy"));
        assert_eq!(f[3].start, Position::new(0, 0));
        assert!(f[0].url.is_some());
    }

    #[test]
    fn pyright_output() {
        let json = r#"{"version": "1.1", "generalDiagnostics": [
            {"file": "/p/m.py", "severity": "error", "message": "Import \"numpy\" could not be resolved",
             "range": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 12}}, "rule": "reportMissingImports"},
            {"file": "/p/m.py", "severity": "information", "message": "x",
             "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}}},
            {"severity": "error", "message": "no file"}
        ], "summary": {"errorCount": 1}}"#;
        // Line 0 has a non-BMP character before column 4 (UTF-16 4 = char 3).
        let read = |_: &Path| Some("😀 = x\nimport numpy\n".to_string());
        let f = parse_pyright(json, Path::new("/p"), &read).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].start, f[0].end), (Position::new(1, 7), Position::new(1, 12)));
        assert_eq!(f[0].code.as_deref(), Some("reportMissingImports"));
        assert_eq!(f[1].start, Position::new(0, 3));
        assert_eq!(f[1].severity, Severity::Info);
        assert!(parse_pyright("not json", Path::new("/p"), &read).is_none());
        let summary = || "boom".to_string();
        assert_eq!(
            parse(Tool::Pyright, Path::new("/p"), "", Some(3), false, &summary),
            (Vec::new(), Some("boom".into()))
        );
        assert_eq!(parse(Tool::Mypy, Path::new("/p"), "", Some(0), true, &summary), (Vec::new(), None));
        assert_eq!(parse(Tool::Mypy, Path::new("/p"), "", Some(2), false, &summary).1.as_deref(), Some("boom"));
    }

    #[test]
    fn args_and_suppression() {
        let a = args(Tool::Mypy, &[".".into()], Some(Path::new("/v/bin/python")));
        assert!(a.ends_with(&["--python-executable".into(), "/v/bin/python".into(), ".".into()]));
        let a = args(Tool::Pyright, &["x.py".into()], None);
        assert_eq!(a, ["--outputjson", "x.py"]);
        assert_eq!(suppress_comment(Tool::Mypy, "arg-type").as_deref(), Some("  # type: ignore[arg-type]"));
        assert_eq!(suppress_comment(Tool::Pyright, "reportX").as_deref(), Some("  # pyright: ignore[reportX]"));
        assert_eq!(suppress_comment(Tool::Ruff, "F401"), None);
    }
}
