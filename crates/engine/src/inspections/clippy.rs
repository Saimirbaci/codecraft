//! Rust inspections: `cargo clippy --message-format=json --all-targets` (or `cargo check` when the
//! Rust settings turn Clippy off or it is not installed), compiler messages → findings with
//! rustc's suggestions as fixes.
//!
//! rustc spans are 1-based lines and 1-based *char* columns; findings are zero-based.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Deserialize;
use serde_json::Value;

use codecraft_text::Position;
use codecraft_workspace::Severity;

use super::{Applicability, Finding, Fix, JobSpec, TextChange, Tool, ToolRun, docs_url, resolve_path, run_process};

/// Suggestion children nested deeper than this are ignored (rustc nests one level).
const MAX_DEPTH: usize = 4;
/// Description text kept per finding.
const MAX_DESCRIPTION: usize = 16 * 1024;

/// The arguments after `cargo` for an inspection run.
pub fn args(clippy: bool) -> Vec<String> {
    let sub = if clippy { "clippy" } else { "check" };
    vec![sub.into(), "--message-format=json".into(), "--all-targets".into()]
}

/// A rustc or Clippy lint name (`unused_variables`, `clippy::unwrap_used`) safe to pass to `-W`.
pub fn is_lint_name(s: &str) -> bool {
    let bare = s.strip_prefix("clippy::").unwrap_or(s);
    !bare.is_empty() && bare.len() <= 80 && bare.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether a failed `cargo clippy` means Clippy is not installed (rather than a build error).
pub fn clippy_missing(stderr: &[String]) -> bool {
    stderr.iter().any(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("no such command: `clippy`")
            || l.contains("no such subcommand: `clippy`")
            || (l.contains("clippy") && l.contains("is not installed"))
            || l.contains("cargo-clippy` is not installed")
    })
}

/// Run clippy (falling back to check) in `cargo_root`; findings for every compiler message.
pub(crate) fn run(
    spec: &JobSpec,
    cargo_root: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<(ToolRun, Vec<Finding>), String> {
    let program = super::find_program(spec, Tool::Clippy).unwrap_or_else(codecraft_cargo::find_cargo);
    // Clippy when asked for by name, else when Settings › Rust turns it on or the inspection
    // profile turns on Clippy lint groups — unless the profile turns Clippy off (M14 CA).
    let asked = spec.tools.as_ref().is_some_and(|t| t.contains(&Tool::Clippy));
    let mut clippy = !spec.tools.as_ref().is_some_and(|t| t == &[Tool::CargoCheck])
        && (asked || ((spec.use_clippy || !spec.profile.clippy_groups.is_empty()) && spec.profile.clippy_enabled()));
    let mut note = String::new();
    loop {
        let mut args = args(clippy);
        if clippy {
            // The profile's lint groups and rule switches (M14 CA).
            let mut flags = spec.profile.clippy_args();
            // Run Inspection by Name turns the lint on (allow-by-default Clippy lints included).
            if let Some(lint) = spec.only.as_deref().filter(|l| is_lint_name(l)) {
                flags.extend(["-W".to_string(), lint.to_string()]);
            }
            if !flags.is_empty() {
                args.push("--".to_string());
                args.extend(flags);
            }
        }
        let command_line = format!("cargo {}", args.join(" "));
        progress(&format!("Running {command_line}…"));
        let out = run_process(&program, &args, cargo_root, cancel, progress)?;
        let tool = if clippy { Tool::Clippy } else { Tool::CargoCheck };
        let mut findings = Vec::new();
        let mut messages = 0usize;
        for line in &out.stdout {
            if let Some(f) = parse_line(line, cargo_root, tool) {
                findings.push(f);
            }
            if line.contains("\"reason\":\"compiler-message\"") || line.contains("\"reason\":\"build-finished\"") {
                messages += 1;
            }
        }
        if clippy && !out.success && messages == 0 && clippy_missing(&out.stderr) {
            clippy = false;
            note = " (Clippy is not installed: `rustup component add clippy`; used cargo check)".into();
            continue;
        }
        // A failed build still reports its errors; a failure without any message is the tool's.
        let ok = out.success || messages > 0;
        let outcome = if ok {
            format!("{}{note}", super::count_label(dedup_count(&findings)))
        } else {
            format!("failed: {}", out.stderr_summary())
        };
        return Ok((ToolRun { tool, command_line, outcome, ok }, findings));
    }
}

fn dedup_count(findings: &[Finding]) -> usize {
    let mut seen = std::collections::HashSet::new();
    findings.iter().filter(|f| seen.insert((&f.path, f.start, &f.message))).count()
}

#[derive(Deserialize)]
struct RawLine {
    reason: Option<String>,
    message: Option<RawDiag>,
}

#[derive(Default, Deserialize)]
struct RawDiag {
    message: Option<String>,
    code: Option<Value>,
    level: Option<String>,
    spans: Option<Vec<RawSpan>>,
    children: Option<Vec<RawDiag>>,
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct RawSpan {
    file_name: Option<String>,
    line_start: Option<usize>,
    line_end: Option<usize>,
    column_start: Option<usize>,
    column_end: Option<usize>,
    is_primary: Option<bool>,
    suggested_replacement: Option<String>,
    suggestion_applicability: Option<String>,
}

impl RawSpan {
    /// Zero-based (start, end); `None` for missing or zero (invalid) coordinates.
    fn range(&self) -> Option<(Position, Position)> {
        let (ls, le, cs, ce) = (self.line_start?, self.line_end?, self.column_start?, self.column_end?);
        if ls == 0 || le == 0 || cs == 0 || ce == 0 || (le, ce) < (ls, cs) {
            return None;
        }
        Some((Position::new(ls - 1, cs - 1), Position::new(le - 1, ce - 1)))
    }

    fn file(&self) -> Option<&str> {
        self.file_name.as_deref().filter(|f| !f.is_empty() && !f.starts_with('<'))
    }
}

fn code_string(code: &Value) -> Option<String> {
    match code {
        Value::String(c) => Some(c.clone()),
        Value::Object(o) => o.get("code").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
    .filter(|c| !c.is_empty())
}

fn severity(level: &str) -> Option<Severity> {
    match level {
        "error" | "error: internal compiler error" => Some(Severity::Error),
        "warning" => Some(Severity::Warning),
        "note" | "help" => Some(Severity::Info),
        "failure-note" => None,
        l if l.starts_with("error") => Some(Severity::Error),
        l if l.starts_with("warning") => Some(Severity::Warning),
        _ => Some(Severity::Info),
    }
}

/// A finding from one `compiler-message` line of cargo's JSON output (`None` for anything else,
/// for summaries without a span, and for malformed lines).
pub fn parse_line(line: &str, base: &Path, tool: Tool) -> Option<Finding> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    // serde_json's recursion limit rejects hostile nesting instead of overflowing the stack.
    let raw: RawLine = serde_json::from_str(line).ok()?;
    if raw.reason.as_deref() != Some("compiler-message") {
        return None;
    }
    let d = raw.message?;
    let severity = severity(d.level.as_deref().unwrap_or("warning"))?;
    let spans = d.spans.as_deref().unwrap_or(&[]);
    let primary = spans.iter().find(|s| s.is_primary == Some(true) && s.file().is_some())?;
    let (start, end) = primary.range()?;
    let path = resolve_path(base, primary.file()?);
    let code = d.code.as_ref().and_then(code_string);
    let inspection = match &code {
        Some(c) => c.clone(),
        None if severity == Severity::Error => "Compiler errors".to_string(),
        None => "Compiler warnings".to_string(),
    };
    let mut f = Finding::new(tool, inspection, severity, path, start);
    f.end = end;
    f.message = d.message.clone().unwrap_or_default();
    f.description = description(&d);
    f.url = further_information_url(&d).or_else(|| code.as_deref().and_then(|c| docs_url(tool, c, severity)));
    f.code = code.filter(|c| !is_error_code(c));
    collect_fixes(&d, base, 0, &mut f.fixes);
    Some(f)
}

/// `E0308` (not suppressible with `#[allow]`).
fn is_error_code(code: &str) -> bool {
    code.len() == 5 && code.starts_with('E') && code.get(1..).is_some_and(|d| d.chars().all(|c| c.is_ascii_digit()))
}

/// Clippy adds "for further information visit <url>" as a help child.
fn further_information_url(d: &RawDiag) -> Option<String> {
    d.children.as_deref()?.iter().find_map(|c| {
        let m = c.message.as_deref()?;
        let at = m.find("https://").or_else(|| m.find("http://"))?;
        let url: String = m.get(at..)?.chars().take_while(|ch| !ch.is_whitespace()).collect();
        Some(url.trim_end_matches(['.', ',', ')']).to_string())
    })
}

/// The rendered diagnostic (plain text), else the message and its notes.
fn description(d: &RawDiag) -> String {
    let text = match d.rendered.as_deref().map(crate::runs::strip_ansi) {
        Some(r) if !r.trim().is_empty() => r.trim_end().to_string(),
        _ => {
            let mut s = d.message.clone().unwrap_or_default();
            for c in d.children.as_deref().unwrap_or(&[]) {
                if let Some(m) = &c.message {
                    s.push_str(&format!("\n= {}: {m}", c.level.as_deref().unwrap_or("note")));
                }
            }
            s
        }
    };
    text.chars().take(MAX_DESCRIPTION).collect()
}

/// Every child with suggested replacements is one fix (all its spans apply together).
fn collect_fixes(d: &RawDiag, base: &Path, depth: usize, out: &mut Vec<Fix>) {
    if depth > MAX_DEPTH {
        return;
    }
    let spans = d.spans.as_deref().unwrap_or(&[]);
    let suggested: Vec<&RawSpan> = spans.iter().filter(|s| s.suggested_replacement.is_some()).collect();
    if !suggested.is_empty() {
        let mut changes = Vec::new();
        let mut applicability = Applicability::MachineApplicable;
        let mut valid = true;
        for s in &suggested {
            let (Some(file), Some((start, end))) = (s.file(), s.range()) else {
                valid = false;
                break;
            };
            let a = s.suggestion_applicability.as_deref().map_or(Applicability::Unspecified, Applicability::parse);
            if a > applicability {
                applicability = a;
            }
            changes.push(TextChange {
                path: resolve_path(base, file),
                start,
                end,
                new_text: s.suggested_replacement.clone().unwrap_or_default(),
            });
        }
        if valid {
            let title = fix_title(d.message.as_deref().unwrap_or("Apply suggestion"), &changes);
            if !out.iter().any(|f: &Fix| f.changes == changes) {
                out.push(Fix { title, applicability, changes });
            }
        }
    }
    for c in d.children.as_deref().unwrap_or(&[]) {
        collect_fixes(c, base, depth.saturating_add(1), out);
    }
}

/// "if this is intentional, prefix it with an underscore" → "If this is intentional, prefix it
/// with an underscore: `_x`" (the replacement is named when there is one short one).
fn fix_title(message: &str, changes: &[TextChange]) -> String {
    let mut t: String = message.trim().to_string();
    if let Some(first) = t.get(..1) {
        t = format!("{}{}", first.to_uppercase(), t.get(1..).unwrap_or(""));
    }
    if let [only] = changes
        && !only.new_text.is_empty()
        && only.new_text.len() <= 40
        && !only.new_text.contains('\n')
        && !t.contains(&only.new_text)
    {
        t = format!("{t}: `{}`", only.new_text);
    }
    t
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;

    const FIXTURE: &str = include_str!("fixtures/clippy.jsonl");

    fn parse_all(text: &str) -> Vec<Finding> {
        text.lines().filter_map(|l| parse_line(l, Path::new("/work/app"), Tool::Clippy)).collect()
    }

    #[test]
    fn real_clippy_output() {
        let fs = parse_all(FIXTURE);
        // Two of each (bin and test targets), the artifact and build-finished lines ignored.
        assert_eq!(fs.len(), 6);
        let (fs, _) = super::super::finalize(fs, |_| true);
        assert_eq!(fs.len(), 3);

        let ret = &fs[0];
        assert_eq!(ret.inspection, "clippy::needless_return");
        assert_eq!(ret.code.as_deref(), Some("clippy::needless_return"));
        assert_eq!(ret.severity, Severity::Warning);
        assert_eq!(ret.path, PathBuf::from("/work/app/src/main.rs"));
        assert_eq!((ret.start, ret.end), (Position::new(1, 4), Position::new(1, 16)));
        assert_eq!(ret.message, "unneeded `return` statement");
        assert!(ret.description.contains("help: remove `return`"));
        assert_eq!(
            ret.url.as_deref(),
            Some("https://rust-lang.github.io/rust-clippy/rust-1.99.0/index.html#needless_return")
        );
        // A multi-span suggestion: replace `return a + b` and delete the `;`.
        assert_eq!(ret.fixes.len(), 1);
        let fix = &ret.fixes[0];
        assert_eq!(fix.title, "Remove `return`");
        assert_eq!(fix.applicability, Applicability::MachineApplicable);
        assert_eq!(fix.changes.len(), 2);
        assert_eq!((fix.changes[0].start, fix.changes[0].end), (Position::new(1, 4), Position::new(1, 16)));
        assert_eq!(fix.changes[0].new_text, "a + b");
        assert_eq!((fix.changes[1].start, fix.changes[1].end, fix.changes[1].new_text.as_str()), (
            Position::new(1, 16),
            Position::new(1, 17),
            ""
        ));

        // Char columns on a line with non-ASCII text before the span.
        let unused = &fs[1];
        assert_eq!(unused.inspection, "unused_variables");
        assert_eq!((unused.start, unused.end), (Position::new(5, 31), Position::new(5, 37)));
        assert_eq!(unused.fixes[0].title, "If this is intentional, prefix it with an underscore: `_unused`");
        assert_eq!(
            unused.url.as_deref(),
            Some("https://doc.rust-lang.org/rustc/lints/listing/warn-by-default.html#unused-variables")
        );

        let len = &fs[2];
        assert_eq!(len.inspection, "clippy::len_zero");
        assert_eq!(len.fixes[0].changes[0].new_text, "v.is_empty()");
    }

    #[test]
    fn errors_without_code_and_maybe_incorrect_suggestions() {
        let line = r#"{"reason":"compiler-message","message":{"message":"expected `;`","level":"error","code":null,"spans":[{"file_name":"/abs/src/lib.rs","line_start":3,"line_end":3,"column_start":10,"column_end":10,"is_primary":true}],"children":[{"message":"add `;` here","level":"help","spans":[{"file_name":"/abs/src/lib.rs","line_start":3,"line_end":3,"column_start":10,"column_end":10,"is_primary":true,"suggested_replacement":";","suggestion_applicability":"MaybeIncorrect"}]}],"rendered":null}}"#;
        let f = parse_line(line, Path::new("/work"), Tool::CargoCheck).unwrap();
        assert_eq!(f.inspection, "Compiler errors");
        assert_eq!(f.code, None);
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.path, PathBuf::from("/abs/src/lib.rs"));
        assert_eq!(f.fixes[0].applicability, Applicability::MaybeIncorrect);
        assert_eq!(f.fixes[0].title, "Add `;` here");
        assert!(f.safe_fix().is_none());
        assert!(f.description.contains("= help: add `;` here"));
    }

    #[test]
    fn error_codes_are_not_suppressible() {
        let line = r#"{"reason":"compiler-message","message":{"message":"mismatched types","level":"error","code":{"code":"E0308","explanation":"…"},"spans":[{"file_name":"src/main.rs","line_start":2,"line_end":2,"column_start":14,"column_end":17,"is_primary":true}],"children":[]}}"#;
        let f = parse_line(line, Path::new("/w"), Tool::Clippy).unwrap();
        assert_eq!(f.inspection, "E0308");
        assert_eq!(f.code, None);
        assert_eq!(f.url.as_deref(), Some("https://doc.rust-lang.org/error_codes/E0308.html"));
    }

    #[test]
    fn junk_and_summaries_are_ignored() {
        for line in [
            "",
            "   Checking app v0.1.0",
            "{",
            "[1]",
            r#"{"reason":"build-finished","success":true}"#,
            r#"{"reason":"compiler-message","message":{"message":"2 warnings emitted","level":"warning","spans":[],"children":[]}}"#,
            r#"{"reason":"compiler-message","message":{"message":"x","level":"failure-note","spans":[{"file_name":"a.rs","line_start":1,"line_end":1,"column_start":1,"column_end":2,"is_primary":true}]}}"#,
            r#"{"reason":"compiler-message","message":{"message":"x","level":"warning","spans":[{"file_name":"a.rs","line_start":0,"line_end":0,"column_start":0,"column_end":0,"is_primary":true}]}}"#,
            r#"{"reason":"compiler-message","message":{"message":"x","level":"warning","spans":[{"file_name":"<macros>","line_start":1,"line_end":1,"column_start":1,"column_end":2,"is_primary":true}]}}"#,
            r#"{"reason":"compiler-message","message":"str"}"#,
            r#"{"reason":"compiler-message","message":{"spans":[{"line_start":-1}]}}"#,
        ] {
            assert!(parse_line(line, Path::new("/w"), Tool::Clippy).is_none(), "{line}");
        }
        let deep = format!(r#"{{"reason":"compiler-message","message":{}"#, "[".repeat(100_000));
        assert!(parse_line(&deep, Path::new("/w"), Tool::Clippy).is_none());
    }

    #[test]
    fn suggestions_with_bad_spans_are_dropped_and_depth_is_capped() {
        let mut child = serde_json::json!({"message": "leaf", "level": "help", "spans": [
            {"file_name": "a.rs", "line_start": 1, "line_end": 1, "column_start": 1, "column_end": 2, "suggested_replacement": "x", "suggestion_applicability": "MachineApplicable"}
        ]});
        for _ in 0..10 {
            child = serde_json::json!({"message": "n", "level": "note", "children": [child]});
        }
        let line = serde_json::json!({"reason": "compiler-message", "message": {"message": "m", "level": "warning", "spans": [
            {"file_name": "a.rs", "line_start": 1, "line_end": 1, "column_start": 1, "column_end": 2, "is_primary": true}
        ], "children": [child, {"message": "bad", "level": "help", "spans": [
            {"file_name": "a.rs", "line_start": 2, "line_end": 1, "column_start": 1, "column_end": 2, "suggested_replacement": "y"}
        ]}]}})
        .to_string();
        let f = parse_line(&line, Path::new("/w"), Tool::Clippy).unwrap();
        assert!(f.fixes.is_empty(), "{:?}", f.fixes);
    }

    #[test]
    fn missing_clippy_is_detected() {
        assert!(clippy_missing(&["error: no such command: `clippy`".into()]));
        assert!(clippy_missing(&["error: 'cargo-clippy' is not installed for the toolchain 'stable'".into()]));
        assert!(!clippy_missing(&["error: could not compile `app`".into()]));
        assert_eq!(args(true), ["clippy", "--message-format=json", "--all-targets"]);
        assert_eq!(args(false)[0], "check");
    }
}
