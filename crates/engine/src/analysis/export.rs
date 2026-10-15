//! Exporting inspection results: CodeCraft JSON (what View Offline Inspection Results reads),
//! SARIF 2.1.0 (for CI and code-scanning tools) and a self-contained HTML report; importing
//! SARIF back as offline results.
//!
//! SARIF regions are 1-based; columns are Unicode code points (`columnKind:
//! unicodeCodePoints`), which is what findings hold. Paths are relative to `%SRCROOT%` (the
//! project root) when inside it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_text::Position;
use codecraft_workspace::Severity;

use super::profiles::Level;
use crate::inspections::{self, Finding, InspectionResults, JobKind, Scope, ScopeKind, Tool};

pub const JSON_FORMAT: &str = "codecraft-inspection-results/1";
/// Results imported from SARIF are capped like a run.
const MAX_IMPORT: usize = inspections::MAX_FINDINGS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Sarif,
    Html,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" | "codecraft" => Some(Self::Json),
            "sarif" | "sarif2" | "sarif-2.1.0" => Some(Self::Sarif),
            "html" | "htm" => Some(Self::Html),
            _ => None,
        }
    }

    /// From a file name's extension (`.sarif`, `.sarif.json`, `.html`, `.json`).
    pub fn from_path(p: &Path) -> Option<Self> {
        let name = p.file_name()?.to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".sarif") || name.ends_with(".sarif.json") {
            Some(Self::Sarif)
        } else if name.ends_with(".html") || name.ends_with(".htm") {
            Some(Self::Html)
        } else if name.ends_with(".json") {
            Some(Self::Json)
        } else {
            None
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Sarif => "sarif",
            Self::Html => "html",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Sarif => "sarif",
            Self::Html => "html",
        }
    }
}

fn rel(root: Option<&Path>, p: &Path) -> String {
    match root.and_then(|r| p.strip_prefix(r).ok()) {
        Some(r) if !r.as_os_str().is_empty() => r.to_string_lossy().replace('\\', "/"),
        _ => p.to_string_lossy().into_owned(),
    }
}

/// The CodeCraft JSON document (open and resolved findings; ids kept).
pub fn to_json(results: &InspectionResults) -> Value {
    json!({ "format": JSON_FORMAT, "results": results })
}

fn sarif_level(sev: Severity) -> &'static str {
    match sev {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info | Severity::Hint => "note",
    }
}

fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = if s.ends_with('/') { s } else { format!("{s}/") };
    if s.starts_with('/') { format!("file://{s}") } else { format!("file:///{s}") }
}

/// A SARIF 2.1.0 log: one run per analyzer, rules from the findings, `baselineState` when the
/// results were compared with a baseline, fixes as `artifactChanges`.
pub fn to_sarif(results: &InspectionResults, root: Option<&Path>) -> Value {
    let mut by_tool: BTreeMap<Tool, Vec<&Finding>> = BTreeMap::new();
    for f in results.open() {
        by_tool.entry(f.tool).or_default().push(f);
    }
    // Tools that ran without findings still get a run (a clean result is information).
    for t in &results.tools {
        by_tool.entry(t.tool).or_default();
    }
    let runs: Vec<Value> = by_tool
        .into_iter()
        .map(|(tool, findings)| {
            let mut rules: Vec<String> = Vec::new();
            for f in &findings {
                if !rules.iter().any(|r| r == f.rule()) {
                    rules.push(f.rule().to_string());
                }
            }
            let rule_objs: Vec<Value> = rules
                .iter()
                .map(|r| {
                    let sample = findings.iter().find(|f| f.rule() == r);
                    let mut o = json!({ "id": r, "name": sample.map(|f| f.inspection.clone()).unwrap_or_default() });
                    if let Some(url) = sample.and_then(|f| inspections::finding_url(f)) {
                        o["helpUri"] = json!(url);
                    }
                    o
                })
                .collect();
            let res: Vec<Value> = findings
                .iter()
                .map(|f| {
                    let mut region = json!({
                        "startLine": f.start.line + 1,
                        "startColumn": f.start.column + 1,
                    });
                    if f.end > f.start {
                        region["endLine"] = json!(f.end.line + 1);
                        region["endColumn"] = json!(f.end.column + 1);
                    }
                    let mut r = json!({
                        "ruleId": f.rule(),
                        "ruleIndex": rules.iter().position(|x| x == f.rule()).unwrap_or(0),
                        "level": sarif_level(f.severity),
                        "message": { "text": if f.message.is_empty() { &f.inspection } else { &f.message } },
                        "locations": [{
                            "physicalLocation": {
                                "artifactLocation": { "uri": rel(root, &f.path), "uriBaseId": "%SRCROOT%" },
                                "region": region,
                            }
                        }],
                        "properties": { "codecraftSeverity": Level::of(f.severity).id(), "codecraftId": f.id },
                    });
                    if !f.fingerprint.is_empty() {
                        r["partialFingerprints"] = json!({ "codecraft/v1": f.fingerprint });
                    }
                    if results.baseline.is_some() {
                        r["baselineState"] = json!(if f.baseline { "unchanged" } else { "new" });
                    }
                    let fixes: Vec<Value> = f
                        .fixes
                        .iter()
                        .filter(|x| x.applicability.is_offered())
                        .map(|x| {
                            let mut files: BTreeMap<&PathBuf, Vec<Value>> = BTreeMap::new();
                            for c in &x.changes {
                                files.entry(&c.path).or_default().push(json!({
                                    "deletedRegion": {
                                        "startLine": c.start.line + 1, "startColumn": c.start.column + 1,
                                        "endLine": c.end.line + 1, "endColumn": c.end.column + 1,
                                    },
                                    "insertedContent": { "text": c.new_text },
                                }));
                            }
                            json!({
                                "description": { "text": x.title },
                                "artifactChanges": files.into_iter().map(|(p, reps)| json!({
                                    "artifactLocation": { "uri": rel(root, p), "uriBaseId": "%SRCROOT%" },
                                    "replacements": reps,
                                })).collect::<Vec<_>>(),
                            })
                        })
                        .collect();
                    if !fixes.is_empty() {
                        r["fixes"] = json!(fixes);
                    }
                    r
                })
                .collect();
            let ran = results.tools.iter().find(|t| t.tool == tool);
            let mut run = json!({
                "tool": { "driver": {
                    "name": tool.label(),
                    "rules": rule_objs,
                    "properties": { "codecraftAnalyzer": tool.id() },
                } },
                "columnKind": "unicodeCodePoints",
                "results": res,
            });
            if let Some(root) = root {
                run["originalUriBaseIds"] = json!({ "%SRCROOT%": { "uri": file_uri(root) } });
            }
            if let Some(t) = ran {
                run["invocations"] = json!([{
                    "executionSuccessful": t.ok,
                    "commandLine": t.command_line,
                    "properties": { "outcome": t.outcome },
                }]);
            }
            run
        })
        .collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": runs,
    })
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A self-contained HTML report: summary per severity and analyzer, then problems per file.
pub fn to_html(results: &InspectionResults, root: Option<&Path>) -> String {
    let open: Vec<&Finding> = results.open().collect();
    let mut by_file: BTreeMap<String, Vec<&Finding>> = BTreeMap::new();
    for f in &open {
        by_file.entry(rel(root, &f.path)).or_default().push(f);
    }
    let mut counts: BTreeMap<Level, usize> = BTreeMap::new();
    for f in &open {
        *counts.entry(Level::of(f.severity)).or_insert(0) += 1;
    }
    let mut h = String::new();
    let title = format!("Inspection results: {}", results.scope_label);
    let _ = write!(
        h,
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>\
         body{{font:14px/1.45 -apple-system,Segoe UI,sans-serif;margin:24px;color:#1f2328;background:#fff}}\
         h1{{font-size:20px}} h2{{font-size:15px;margin-top:28px}} table{{border-collapse:collapse;width:100%}}\
         td,th{{text-align:left;padding:4px 8px;border-bottom:1px solid #d0d7de;vertical-align:top}}\
         code{{font:12px ui-monospace,Menlo,monospace}} .error{{color:#cf222e}} .warning{{color:#9a6700}}\
         .weak_warning,.info{{color:#57606a}} .new{{font-weight:600}} .muted{{color:#57606a}}\
         @media (prefers-color-scheme: dark){{body{{background:#0d1117;color:#e6edf3}} td,th{{border-color:#30363d}}\
         .error{{color:#ff7b72}} .warning{{color:#d29922}} .muted,.weak_warning,.info{{color:#8b949e}}}}\
         </style></head><body>\n<h1>{}</h1>\n",
        esc(&title),
        esc(&title)
    );
    let summary: Vec<String> = Level::ALL
        .iter()
        .map(|l| format!("<span class=\"{}\">{} {}</span>", l.id(), counts.get(l).copied().unwrap_or(0), esc(l.label())))
        .collect();
    let _ = writeln!(h, "<p>{} problem(s): {}", open.len(), summary.join(" · "));
    if !results.profile.is_empty() {
        let _ = write!(h, " — profile <b>{}</b>", esc(&results.profile));
    }
    if results.baseline.is_some() {
        let new = open.iter().filter(|f| !f.baseline).count();
        let _ = write!(h, " — <span class=\"new\">{new} new</span> since the baseline");
    }
    let _ = writeln!(h, "</p>");
    let _ = writeln!(h, "<table><tr><th>Analyzer</th><th>Outcome</th><th>Command</th></tr>");
    for t in &results.tools {
        let _ = writeln!(
            h,
            "<tr><td>{}</td><td class=\"{}\">{}</td><td><code>{}</code></td></tr>",
            esc(t.tool.label()),
            if t.ok { "muted" } else { "warning" },
            esc(&t.outcome),
            esc(&t.command_line)
        );
    }
    let _ = writeln!(h, "</table>");
    for (file, list) in &by_file {
        let _ = writeln!(h, "<h2><code>{}</code> <span class=\"muted\">{}</span></h2>", esc(file), list.len());
        let _ = writeln!(h, "<table><tr><th>Line</th><th>Severity</th><th>Rule</th><th>Message</th><th>Analyzer</th></tr>");
        for f in list {
            let level = Level::of(f.severity);
            let rule = match inspections::finding_url(f) {
                Some(u) => format!("<a href=\"{}\">{}</a>", esc(&u), esc(f.rule())),
                None => esc(f.rule()),
            };
            let new = if results.baseline.is_some() && !f.baseline { " <span class=\"new\">new</span>" } else { "" };
            let _ = writeln!(
                h,
                "<tr><td>{}:{}</td><td class=\"{}\">{}{new}</td><td><code>{rule}</code></td><td>{}</td><td class=\"muted\">{}</td></tr>",
                f.start.line + 1,
                f.start.column + 1,
                level.id(),
                esc(level.label()),
                esc(&f.message),
                esc(f.tool.label())
            );
        }
        let _ = writeln!(h, "</table>");
    }
    if open.is_empty() {
        let _ = writeln!(h, "<p class=\"muted\">No problems found.</p>");
    }
    h.push_str("</body></html>\n");
    h
}

fn num(v: &Value, k: &str) -> Option<usize> {
    v.get(k).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok())
}

/// Import a SARIF log (ours or another tool's) as offline results. Paths resolve against
/// `root` (`%SRCROOT%` or relative URIs) or the log's own base URIs.
pub fn from_sarif(v: &Value, root: Option<&Path>, label: &str) -> Result<InspectionResults, String> {
    let runs = v.get("runs").and_then(Value::as_array).ok_or("not a SARIF log (no `runs`)")?;
    let mut out = InspectionResults::new(JobKind::Inspect, Scope::new(ScopeKind::Project, None), label);
    for run in runs {
        let driver = run.pointer("/tool/driver");
        let name = driver.and_then(|d| d.get("name")).and_then(Value::as_str).unwrap_or("SARIF");
        let tool = driver
            .and_then(|d| d.pointer("/properties/codecraftAnalyzer"))
            .and_then(Value::as_str)
            .and_then(Tool::parse)
            .or_else(|| Tool::parse(name))
            .unwrap_or(Tool::ClangTidy);
        let base = run
            .pointer("/originalUriBaseIds/%SRCROOT%/uri")
            .and_then(Value::as_str)
            .map(|u| PathBuf::from(u.strip_prefix("file://").unwrap_or(u)));
        for r in run.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            if out.findings.len() >= MAX_IMPORT {
                out.truncated = true;
                break;
            }
            let Some(loc) = r.pointer("/locations/0/physicalLocation") else { continue };
            let Some(uri) = loc.pointer("/artifactLocation/uri").and_then(Value::as_str) else { continue };
            let uri = uri.strip_prefix("file://").unwrap_or(uri);
            let p = Path::new(uri);
            let path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                match root.or(base.as_deref()) {
                    Some(b) => b.join(p),
                    None => p.to_path_buf(),
                }
            };
            let region = loc.get("region").cloned().unwrap_or(Value::Null);
            let sl = num(&region, "startLine").unwrap_or(1).max(1);
            let sc = num(&region, "startColumn").unwrap_or(1).max(1);
            let start = Position::new(sl - 1, sc - 1);
            let el = num(&region, "endLine").unwrap_or(sl).max(1);
            let ec = num(&region, "endColumn").unwrap_or(sc).max(1);
            let end = Position::new(el - 1, ec - 1).max(start);
            let severity = match r.pointer("/properties/codecraftSeverity").and_then(Value::as_str).and_then(Level::parse) {
                Some(l) => l.to_severity(),
                None => match r.get("level").and_then(Value::as_str) {
                    Some("error") => Severity::Error,
                    Some("note") => Severity::Info,
                    Some("none") => Severity::Hint,
                    _ => Severity::Warning,
                },
            };
            let rule = r.get("ruleId").and_then(Value::as_str).unwrap_or("SARIF").chars().take(200).collect::<String>();
            let mut f = Finding::new(tool, rule.clone(), severity, crate::normalize_path(&path), start);
            f.end = end;
            f.code = Some(rule);
            f.message = r.pointer("/message/text").and_then(Value::as_str).unwrap_or("").chars().take(8192).collect();
            f.description = f.message.clone();
            f.fingerprint =
                r.pointer("/partialFingerprints/codecraft~1v1").and_then(Value::as_str).unwrap_or("").chars().take(2048).collect();
            f.baseline = r.get("baselineState").and_then(Value::as_str) == Some("unchanged");
            f.id = out.findings.len();
            out.findings.push(f);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inspections::{Applicability, Fix, TextChange, ToolRun};

    fn results() -> InspectionResults {
        let mut a = Finding::new(Tool::Clippy, "clippy::needless_return", Severity::Warning, "/p/src/main.rs".into(), Position::new(1, 4));
        a.code = Some("clippy::needless_return".into());
        a.end = Position::new(1, 16);
        a.message = "unneeded `return` <statement>".into();
        a.fingerprint = "fp1".into();
        a.fixes.push(Fix {
            title: "Remove `return`".into(),
            applicability: Applicability::MachineApplicable,
            changes: vec![TextChange { path: "/p/src/main.rs".into(), start: Position::new(1, 4), end: Position::new(1, 11), new_text: String::new() }],
        });
        let mut b = Finding::new(Tool::Ruff, "F401 unused-import", Severity::Info, "/p/app.py".into(), Position::new(0, 7));
        b.code = Some("F401".into());
        b.id = 1;
        b.baseline = true;
        let mut r = InspectionResults {
            findings: vec![a, b],
            ..InspectionResults::new(JobKind::Inspect, Scope::new(ScopeKind::Project, None), "Project 'p'")
        };
        r.tools.push(ToolRun { tool: Tool::Clippy, command_line: "cargo clippy".into(), outcome: "1 problem".into(), ok: true });
        r.tools.push(ToolRun { tool: Tool::Sql, command_line: String::new(), outcome: "0 problems".into(), ok: true });
        r.baseline = Some("/p/.codecraft/inspection-baseline.json".into());
        r.profile = "Default".into();
        r
    }

    #[test]
    fn sarif_has_runs_rules_regions_and_fixes() {
        let v = to_sarif(&results(), Some(Path::new("/p")));
        assert_eq!(v["version"], "2.1.0");
        let runs = v["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 3, "clippy, ruff and the clean SQL run");
        let clippy = runs.iter().find(|r| r["tool"]["driver"]["name"] == "Clippy").unwrap();
        assert_eq!(clippy["columnKind"], "unicodeCodePoints");
        assert_eq!(clippy["originalUriBaseIds"]["%SRCROOT%"]["uri"], "file:///p/");
        let r = &clippy["results"][0];
        assert_eq!(r["ruleId"], "clippy::needless_return");
        assert_eq!(r["level"], "warning");
        assert_eq!(r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"], "src/main.rs");
        assert_eq!(r["locations"][0]["physicalLocation"]["region"]["startLine"], 2);
        assert_eq!(r["locations"][0]["physicalLocation"]["region"]["startColumn"], 5);
        assert_eq!(r["baselineState"], "new");
        assert_eq!(r["fixes"][0]["artifactChanges"][0]["replacements"][0]["deletedRegion"]["endColumn"], 12);
        assert!(clippy["tool"]["driver"]["rules"][0]["helpUri"].as_str().unwrap().contains("needless_return"));
        let ruff = runs.iter().find(|r| r["tool"]["driver"]["name"] == "Ruff").unwrap();
        assert_eq!(ruff["results"][0]["level"], "note");
        assert_eq!(ruff["results"][0]["baselineState"], "unchanged");
        // Back again.
        let back = from_sarif(&v, Some(Path::new("/p")), "imported").unwrap();
        assert_eq!(back.findings.len(), 2);
        let a = back.findings.iter().find(|f| f.tool == Tool::Clippy).unwrap();
        assert_eq!((a.start, a.end), (Position::new(1, 4), Position::new(1, 16)));
        assert_eq!(a.path, PathBuf::from("/p/src/main.rs"));
        assert_eq!(a.fingerprint, "fp1");
        let b = back.findings.iter().find(|f| f.tool == Tool::Ruff).unwrap();
        assert_eq!(b.severity, Severity::Info, "weak warning survives the round trip");
        assert!(b.baseline);
        assert!(from_sarif(&json!({ "nope": 1 }), None, "x").is_err());
        // Junk results are skipped, not fatal.
        let junk = json!({ "runs": [{ "results": [1, {"locations": []}, {"locations": [{"physicalLocation": {"artifactLocation": {"uri": "a.c"}, "region": {"startLine": 0, "startColumn": -4}}}], "ruleId": "x"}] }] });
        let r = from_sarif(&junk, Some(Path::new("/w")), "j").unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].start, Position::new(0, 0));
    }

    #[test]
    fn html_is_escaped_and_complete() {
        let h = to_html(&results(), Some(Path::new("/p")));
        assert!(h.starts_with("<!doctype html>"));
        assert!(h.contains("unneeded `return` &lt;statement&gt;"));
        assert!(!h.contains("<statement>"));
        assert!(h.contains("<code>src/main.rs</code>"));
        assert!(h.contains("1 new</span> since the baseline"));
        assert!(h.contains("Weak Warning"));
        assert!(h.ends_with("</body></html>\n"));
    }

    #[test]
    fn formats() {
        assert_eq!(Format::parse("SARIF"), Some(Format::Sarif));
        assert_eq!(Format::parse("xml"), None);
        assert_eq!(Format::from_path(Path::new("r.sarif.json")), Some(Format::Sarif));
        assert_eq!(Format::from_path(Path::new("r.HTML")), Some(Format::Html));
        assert_eq!(Format::from_path(Path::new("r.json")), Some(Format::Json));
        assert_eq!(Format::from_path(Path::new("r.txt")), None);
        assert_eq!(to_json(&results())["format"], JSON_FORMAT);
    }
}
