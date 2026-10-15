//! rust-analyzer's own diagnostics for the whole workspace: `rust-analyzer diagnostics <root>`.
//!
//! The command prints one line per diagnostic:
//! `at crate app, file /abs/src/main.rs: Error RustcHardError("E0308") from LineCol { line: 12,
//! col: 17 } to LineCol { line: 12, col: 20 }: expected i32, found &str`. Lines are zero-based,
//! columns are zero-based UTF-8 byte offsets in the line (converted to chars through the file).
//! The format is rust-analyzer's debug output, not a stable interface: lines that do not match
//! continue the previous message, anything else is ignored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use regex::Regex;

use codecraft_text::Position;
use codecraft_workspace::Severity;

use crate::inspections::{self, Finding, Tool, ToolRun, docs_url, resolve_path, run_process};

const MAX_MESSAGE: usize = 4096;

fn line_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^at crate [^,]*, file (?P<file>.+?): (?P<sev>Error|Warning|WeakWarning|Allow) (?P<code>[A-Za-z]+(?:\([^)]*\))?) from LineCol \{ line: (?P<l1>\d+), col: (?P<c1>\d+) \} to LineCol \{ line: (?P<l2>\d+), col: (?P<c2>\d+) \}: ?(?P<msg>.*)$",
        )
        .ok()
    })
    .as_ref()
}

/// `RustcHardError("E0308")` → (kind, first quoted argument).
fn split_code(code: &str) -> (&str, Option<&str>) {
    let Some((kind, rest)) = code.split_once('(') else { return (code, None) };
    let arg = rest.split('"').nth(1).filter(|a| !a.is_empty());
    (kind, arg)
}

/// Zero-based byte column on a line → char column.
fn byte_to_char(text: Option<&str>, line: usize, col: usize) -> usize {
    let Some(l) = text.and_then(|t| t.split('\n').nth(line)) else { return col };
    let mut b = col.min(l.len());
    while b > 0 && !l.is_char_boundary(b) {
        b -= 1;
    }
    l.get(..b).map_or(col, |p| p.chars().count())
}

/// Findings from the command's output. `read` returns a file's text (for columns).
pub fn parse(stdout: &str, base: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    let Some(re) = line_re() else { return Vec::new() };
    let mut out: Vec<Finding> = Vec::new();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut continuing = false;
    for line in stdout.lines() {
        let Some(c) = re.captures(line) else {
            // A multi-line message continues until a blank line or the summary.
            let t = line.trim_end();
            if continuing && !t.is_empty() && !t.starts_with("diagnostic scan complete")
                && let Some(prev) = out.last_mut()
                && prev.message.len() < MAX_MESSAGE
            {
                prev.message.push('\n');
                prev.message.push_str(t.trim_start());
                prev.description = prev.message.clone();
            } else {
                continuing = false;
            }
            continue;
        };
        let num = |k: &str| c.name(k).and_then(|m| m.as_str().parse::<usize>().ok());
        let (Some(file), Some(l1), Some(c1), Some(l2), Some(c2)) = (c.name("file"), num("l1"), num("c1"), num("l2"), num("c2"))
        else {
            continue;
        };
        let severity = match c.name("sev").map(|m| m.as_str()) {
            Some("Error") => Severity::Error,
            Some("Warning") => Severity::Warning,
            Some("WeakWarning") => Severity::Info,
            _ => {
                continuing = false;
                continue;
            }
        };
        let path = resolve_path(base, file.as_str());
        let text = texts.entry(path.clone()).or_insert_with(|| read(&path));
        let start = Position::new(l1, byte_to_char(text.as_deref(), l1, c1));
        let end = Position::new(l2, byte_to_char(text.as_deref(), l2, c2)).max(start);
        let (kind, arg) = split_code(c.name("code").map_or("", |m| m.as_str()));
        // (rule shown and used by profiles, the lint name `#[allow]` takes)
        let (rule, allow) = match (kind, arg) {
            ("RustcLint", Some(lint)) => (lint.to_string(), Some(lint.to_string())),
            ("Clippy", Some(lint)) => (format!("clippy::{lint}"), Some(format!("clippy::{lint}"))),
            (_, Some(code)) => (code.to_string(), None),
            ("SyntaxError", None) => ("syntax-error".to_string(), None),
            (k, None) => (k.to_string(), None),
        };
        let mut f = Finding::new(Tool::RustAnalyzer, rule.clone(), severity, path, start);
        f.end = end;
        f.message = c.name("msg").map_or("", |m| m.as_str()).chars().take(MAX_MESSAGE).collect();
        f.description = f.message.clone();
        f.url = docs_url(Tool::RustAnalyzer, &rule, severity);
        f.code = allow;
        out.push(f);
        continuing = true;
    }
    out
}

/// Run `rust-analyzer diagnostics` over the Cargo workspace.
pub(crate) fn run(
    program: &Path,
    cargo_root: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<(ToolRun, Vec<Finding>), String> {
    let args = vec!["diagnostics".to_string(), cargo_root.display().to_string()];
    let command_line = format!("rust-analyzer {}", args.iter().map(|a| codecraft_cargo::shell_quote(a)).collect::<Vec<_>>().join(" "));
    progress("Running rust-analyzer diagnostics…");
    let out = run_process(program, &args, cargo_root, cancel, progress)?;
    let read = |p: &Path| codecraft_fs::read_text_file(p).ok().map(|t| codecraft_text::Document::from_text(&t).text());
    let stdout = out.stdout.join("\n");
    let findings = parse(&stdout, cargo_root, &read);
    let ok = out.success || !findings.is_empty() || stdout.contains("diagnostic scan complete");
    let outcome = if ok { inspections::count_label(findings.len()) } else { format!("failed: {}", out.stderr_summary()) };
    Ok((ToolRun { tool: Tool::RustAnalyzer, command_line, outcome, ok }, findings))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // Captured from rust-analyzer 1.99 on a small crate (paths shortened).
    const OUT: &str = "at crate r1, file /w/src/main.rs: Error RustcHardError(\"E0583\") from LineCol { line: 0, col: 0 } to LineCol { line: 0, col: 12 }: unresolved module, can't find module file: missing.rs, or missing/mod.rs
at crate r1, file /w/src/main.rs: WeakWarning Clippy(\"needless_return\") from LineCol { line: 3, col: 4 } to LineCol { line: 3, col: 17 }: replace return <expr>; with <expr>
at crate r1, file /w/src/main.rs: Error RustcHardError(\"E0308\") from LineCol { line: 12, col: 18 } to LineCol { line: 12, col: 21 }: expected i32, found &'static str
at crate r1, file src/lib.rs: Warning RustcLint(\"unused_variables\") from LineCol { line: 1, col: 8 } to LineCol { line: 1, col: 9 }: unused variable: `x`
  second line of the message
at crate r1, file src/lib.rs: Allow Ra(\"inactive-code\", WeakWarning) from LineCol { line: 2, col: 0 } to LineCol { line: 2, col: 3 }: code is inactive
at crate r1, file src/lib.rs: Warning Ra(\"unlinked-file\", WeakWarning) from LineCol { line: 0, col: 0 } to LineCol { line: 0, col: 0 }: file not included
at crate r1, file src/lib.rs: Error SyntaxError from LineCol { line: 3, col: 0 } to LineCol { line: 3, col: 1 }: expected item

diagnostic scan complete
garbage
at crate r1, file x.rs: Error Weird( from LineCol { line: 99999999999999999999, col: 0 } to LineCol { line: 0, col: 0 }: overflow
";

    #[test]
    fn diagnostics_output() {
        // Line 12 has `é` (two bytes) before the span: byte 18 → char 17.
        let main = "mod missing;\n\nfn add() -> i32 {\n    return 1;\n}\n\n\n\n\n\n\n\n    let s: é32 = \"x\";\n";
        let read = |p: &Path| (p == Path::new("/w/src/main.rs")).then(|| main.to_string());
        let f = parse(OUT, Path::new("/w"), &read);
        assert_eq!(f.len(), 6);
        assert_eq!((f[0].inspection.as_str(), f[0].severity, f[0].code.as_deref()), ("E0583", Severity::Error, None));
        assert_eq!(f[1].inspection, "clippy::needless_return");
        assert_eq!(f[1].code.as_deref(), Some("clippy::needless_return"), "suppressible with #[allow]");
        assert_eq!(f[1].severity, Severity::Info);
        assert_eq!((f[2].start, f[2].end), (Position::new(12, 17), Position::new(12, 20)));
        assert_eq!(f[3].path, PathBuf::from("/w/src/lib.rs"));
        assert_eq!(f[3].code.as_deref(), Some("unused_variables"));
        assert!(f[3].message.ends_with("\nsecond line of the message"));
        assert_eq!((f[4].inspection.as_str(), f[4].code.as_deref()), ("unlinked-file", None));
        assert_eq!(f[4].url.as_deref(), Some("https://rust-analyzer.github.io/book/diagnostics.html#unlinked-file"));
        assert_eq!(f[5].inspection, "syntax-error");
        assert!(parse("", Path::new("/w"), &read).is_empty());
    }
}
