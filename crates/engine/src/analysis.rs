//! Code › Analyze Code: Run Inspection by Name…, Silent Code Cleanup, Configure Current File
//! Analysis…, View Offline Inspection Results… (and the export they read). Locate Duplicates and
//! Data Flow live in `duplicates.rs` and `data_flow.rs`.
//!
//! Inspections are the external linters of Inspect Code (`inspections.rs`); running one by name
//! runs its tool on the scope (turning a Rust lint on with `-W` when it is allow-by-default) and
//! keeps only that inspection's findings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{AnalysisLevel, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, ok, spec};
use crate::inspections::{self, InspectionResults, JobKind, ScopeKind, Tool};
use crate::params::{bool_or, opt_path, opt_str, req_str};
use crate::{CommandError, Effect, Session};

/// An inspection Run Inspection by Name offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionInfo {
    pub name: String,
    pub tool: Tool,
    pub description: String,
}

/// Well-known lints of the linters CodeCraft runs (names from rustc, Clippy, Ruff and ESLint; the
/// descriptions are CodeCraft's own).
const KNOWN: &[(&str, Tool, &str)] = &[
    ("unused_variables", Tool::Clippy, "A variable is never read"),
    ("unused_imports", Tool::Clippy, "An import is never used"),
    ("unused_mut", Tool::Clippy, "A binding is declared mutable but never changed"),
    ("dead_code", Tool::Clippy, "An item is never used"),
    ("unreachable_code", Tool::Clippy, "Code after a diverging expression"),
    ("unused_must_use", Tool::Clippy, "A #[must_use] value is ignored"),
    ("non_snake_case", Tool::Clippy, "A name is not in snake_case"),
    ("deprecated", Tool::Clippy, "Use of a deprecated item"),
    ("clippy::needless_return", Tool::Clippy, "An explicit `return` at the end of a function"),
    ("clippy::redundant_clone", Tool::Clippy, "A clone of a value that is not used afterwards"),
    ("clippy::clone_on_copy", Tool::Clippy, "`.clone()` on a Copy type"),
    ("clippy::needless_borrow", Tool::Clippy, "A reference that the compiler takes anyway"),
    ("clippy::len_zero", Tool::Clippy, "`len() == 0` instead of `is_empty()`"),
    ("clippy::collapsible_if", Tool::Clippy, "Nested `if`s that can be one condition"),
    ("clippy::single_match", Tool::Clippy, "A `match` with one interesting arm"),
    ("clippy::manual_map", Tool::Clippy, "A `match` that is `Option::map`"),
    ("clippy::redundant_closure", Tool::Clippy, "A closure that only calls a function"),
    ("clippy::needless_range_loop", Tool::Clippy, "An index loop that could iterate"),
    ("clippy::question_mark", Tool::Clippy, "An early return that is the `?` operator"),
    ("clippy::let_and_return", Tool::Clippy, "A `let` immediately returned"),
    ("clippy::useless_conversion", Tool::Clippy, "A conversion to the same type"),
    ("clippy::bool_comparison", Tool::Clippy, "Comparison with `true` or `false`"),
    ("clippy::new_without_default", Tool::Clippy, "`new()` without a Default impl"),
    ("clippy::too_many_arguments", Tool::Clippy, "A function with many parameters"),
    ("clippy::type_complexity", Tool::Clippy, "A very complex type"),
    ("clippy::large_enum_variant", Tool::Clippy, "An enum with one much larger variant"),
    ("clippy::unwrap_used", Tool::Clippy, "`.unwrap()` on Option or Result (allow-by-default)"),
    ("clippy::expect_used", Tool::Clippy, "`.expect()` on Option or Result (allow-by-default)"),
    ("clippy::todo", Tool::Clippy, "`todo!()` left in the code (allow-by-default)"),
    ("clippy::dbg_macro", Tool::Clippy, "`dbg!` left in the code (allow-by-default)"),
    ("clippy::print_stdout", Tool::Clippy, "Printing to stdout (allow-by-default)"),
    ("clippy::missing_errors_doc", Tool::Clippy, "A public fallible function without an Errors section"),
    ("F401", Tool::Ruff, "Python: an import is never used"),
    ("F841", Tool::Ruff, "Python: a local variable is never used"),
    ("E711", Tool::Ruff, "Python: comparison to None with =="),
    ("E722", Tool::Ruff, "Python: bare `except`"),
    ("no-unused-vars", Tool::Eslint, "JavaScript/TypeScript: a variable is never used"),
    ("no-undef", Tool::Eslint, "JavaScript/TypeScript: an undefined name"),
    ("eqeqeq", Tool::Eslint, "JavaScript/TypeScript: `==` instead of `===`"),
];

/// Every inspection offered: the tools themselves, the well-known lints, and the inspections
/// of the last results.
pub fn catalogue(s: &Session) -> Vec<InspectionInfo> {
    let mut out: Vec<InspectionInfo> = [
        (Tool::Clippy, "All Clippy and compiler lints"),
        (Tool::CargoCheck, "Compiler errors and warnings (cargo check)"),
        (Tool::Ruff, "Every Ruff rule enabled for the project"),
        (Tool::Mypy, "Python type checking"),
        (Tool::Pyright, "Python type checking"),
        (Tool::Eslint, "Every ESLint rule enabled for the project"),
        (Tool::ClangTidy, "Every clang-tidy check enabled for the project"),
        (Tool::ZigFmt, "Zig formatting"),
    ]
    .iter()
    .map(|(t, d)| InspectionInfo { name: t.label().to_string(), tool: *t, description: (*d).to_string() })
    .collect();
    for (name, tool, d) in KNOWN {
        out.push(InspectionInfo { name: (*name).to_string(), tool: *tool, description: (*d).to_string() });
    }
    if let Some(r) = &s.inspections.results {
        for f in &r.findings {
            let name = f.code.clone().unwrap_or_else(|| f.inspection.clone());
            if !out.iter().any(|i| i.name == name) && out.len() < 2000 {
                out.push(InspectionInfo { name, tool: f.tool, description: "From the last inspection".into() });
            }
        }
    }
    out
}

/// Inspections matching a query (case-insensitive substring of the name or description).
pub fn filter(list: &[InspectionInfo], query: &str) -> Vec<InspectionInfo> {
    let q = query.trim().to_lowercase();
    list.iter()
        .filter(|i| q.is_empty() || i.name.to_lowercase().contains(&q) || i.description.to_lowercase().contains(&q))
        .cloned()
        .collect()
}

/// Run Inspection by Name's popup.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByNameDialog {
    pub query: String,
    pub selected: usize,
    /// `file` or `project`.
    pub scope: String,
}

#[derive(Debug, Default)]
pub struct AnalysisState {
    pub by_name: Option<ByNameDialog>,
    /// The Configure Current File Analysis popup is open for this file.
    pub level_popup: Option<PathBuf>,
}

/// The tool and filter for an inspection name.
fn resolve(name: &str) -> (Option<Vec<Tool>>, Option<String>) {
    let name = name.trim();
    if let Some(t) = [Tool::Clippy, Tool::CargoCheck, Tool::Ruff, Tool::Mypy, Tool::Pyright, Tool::Eslint, Tool::ClangTidy, Tool::ZigFmt]
        .into_iter()
        .find(|t| t.label().eq_ignore_ascii_case(name))
    {
        return (Some(vec![t]), None);
    }
    if let Some((_, tool, _)) = KNOWN.iter().find(|(n, ..)| *n == name) {
        return (Some(vec![*tool]), Some(name.to_string()));
    }
    if inspections::clippy::is_lint_name(name) {
        return (Some(vec![Tool::Clippy]), Some(name.to_string()));
    }
    (None, Some(name.to_string()))
}

fn scope_of(s: &Session, p: &Value) -> Result<ScopeKind, CommandError> {
    match opt_str(p, "scope") {
        Some(v) => ScopeKind::parse(v).ok_or_else(|| CommandError::BadParams(format!("unknown scope `{v}`"))),
        None => Ok(if s.ws.active().and_then(|(_, d)| d.path.as_ref()).is_some() { ScopeKind::File } else { ScopeKind::Project }),
    }
}

fn run_by_name(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = opt_str(p, "name").map(str::trim).filter(|n| !n.is_empty()) else {
        let scope = if s.ws.active().and_then(|(_, d)| d.path.as_ref()).is_some() { "file" } else { "project" };
        s.analysis.by_name = Some(ByNameDialog { scope: scope.into(), ..ByNameDialog::default() });
        return Ok(json!({ "dialog": true }));
    };
    let name = name.to_string();
    let scope = scope_of(s, p)?;
    let (tools, only) = resolve(&name);
    inspections::start_with(s, JobKind::Inspect, scope, opt_path(p, "path"), tools.clone(), true, only.clone(), false)?;
    s.analysis.by_name = None;
    s.status = format!("Running inspection '{name}'…");
    Ok(json!({ "started": true, "tools": tools, "only": only, "scope": scope.label() }))
}

fn silent_cleanup(s: &mut Session, p: &Value) -> CmdResult {
    let scope = scope_of(s, p)?;
    inspections::start_with(s, JobKind::Cleanup, scope, opt_path(p, "path"), None, bool_or(p, "format", false), None, true)?;
    // Silent: the results tool window is not brought up.
    s.ws.layout.hide(ToolWindowId::InspectionResults);
    s.status = "Silent Code Cleanup…".into();
    Ok(json!({ "started": true, "scope": scope.label() }))
}

fn configure_level(s: &mut Session, p: &Value) -> CmdResult {
    let path = match opt_path(p, "path") {
        Some(p) => crate::normalize_path(&p),
        None => s
            .ws
            .active()
            .and_then(|(_, d)| d.path.clone())
            .ok_or_else(|| CommandError::Failed("the editor has no file".into()))?,
    };
    match opt_str(p, "level") {
        None => {
            s.analysis.level_popup = Some(path.clone());
            Ok(json!({ "popup": true, "level": s.ws.analysis_level(&path) }))
        }
        Some(l) => {
            let level = AnalysisLevel::parse(l)
                .ok_or_else(|| CommandError::BadParams(format!("unknown level '{l}' (none, syntax, essential, all)")))?;
            if level == AnalysisLevel::All {
                s.ws.analysis_levels.remove(&path);
            } else {
                if s.ws.analysis_levels.len() >= 10_000 && !s.ws.analysis_levels.contains_key(&path) {
                    return Err(CommandError::Failed("too many files with a custom analysis level".into()));
                }
                s.ws.analysis_levels.insert(path.clone(), level);
            }
            s.analysis.level_popup = None;
            s.status = format!("Highlighting level: {}", level.label());
            Ok(json!({ "level": level }))
        }
    }
}

fn export_results(s: &mut Session, p: &Value) -> CmdResult {
    let results = s.inspections.results.clone().ok_or_else(|| CommandError::Failed("no inspection results to export".into()))?;
    let Some(path) = opt_path(p, "path") else {
        s.effects.push(Effect::PickFile {
            title: "Export Inspection Results".into(),
            command: "inspections.export".into(),
            param: "path".into(),
            save: true,
            extensions: vec!["json".into()],
            default_name: Some("inspection-results.json".into()),
        });
        return Ok(json!({ "picker": true }));
    };
    let text = serde_json::to_string_pretty(&OfflineFile { format: OFFLINE_FORMAT.into(), results })
        .map_err(|e| CommandError::Failed(e.to_string()))?;
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
    s.status = format!("Exported inspection results to {}", path.display());
    Ok(json!({ "path": path }))
}

const OFFLINE_FORMAT: &str = "codecraft-inspection-results/1";
/// Largest offline results file read.
const MAX_OFFLINE_BYTES: u64 = 64 << 20;

#[derive(Serialize, Deserialize)]
struct OfflineFile {
    format: String,
    results: InspectionResults,
}

fn view_offline(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        s.effects.push(Effect::PickFile {
            title: "View Offline Inspection Results".into(),
            command: "code.analyzeCode.viewOfflineInspectionResults".into(),
            param: "path".into(),
            save: false,
            extensions: vec!["json".into()],
            default_name: None,
        });
        return Ok(json!({ "picker": true }));
    };
    let meta = std::fs::metadata(&path).map_err(|e| CommandError::Failed(format!("cannot read {}: {e}", path.display())))?;
    if meta.len() > MAX_OFFLINE_BYTES {
        return Err(CommandError::Failed(format!("{} is too large", path.display())));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| CommandError::Failed(format!("cannot read {}: {e}", path.display())))?;
    let file: OfflineFile = serde_json::from_str(&text)
        .or_else(|_| serde_json::from_str::<InspectionResults>(&text).map(|results| OfflineFile { format: OFFLINE_FORMAT.into(), results }))
        .map_err(|e| CommandError::Failed(format!("{} is not an inspection results file: {e}", path.display())))?;
    let mut results = file.results;
    // Ids must be indexes (a hand-edited file may not keep them).
    results.findings.truncate(inspections::MAX_FINDINGS);
    for (i, f) in results.findings.iter_mut().enumerate() {
        f.id = i;
    }
    results.resolved.retain(|id| *id < results.findings.len());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    results.scope_label = format!("Offline ({name}): {}", results.scope_label);
    let n = results.open().count();
    inspections::cancel(s);
    s.inspections.selected = results.open().next().map(|f| f.id);
    s.inspections.results = Some(results);
    s.ws.layout.show(ToolWindowId::InspectionResults);
    s.status = format!("Offline inspection results: {n} problem(s)");
    Ok(json!({ "problems": n }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.analyzeCode.runInspectionByName",
            "Run Inspection by Name…",
            None,
            "{name?: string, scope?: \"file\"|\"directory\"|\"project\"|\"uncommitted\", path?: string} — run one inspection (a tool like \"Clippy\"/\"Ruff\", or a lint like \"clippy::needless_return\", \"F401\") on a scope; without `name` opens the chooser",
            Context::Global,
            has_project,
            run_by_name,
        ),
        spec(
            "code.analyzeCode.silentCodeCleanup",
            "Silent Code Cleanup",
            None,
            "{scope?: string (default: the current file), path?: string, format?: bool} — apply every safe (machine-applicable) fix in the scope without dialogs or preview",
            Context::Global,
            has_project,
            silent_cleanup,
        ),
        spec(
            "code.analyzeCode.configureCurrentFileAnalysis",
            "Configure Current File Analysis…",
            None,
            "{level?: \"none\"|\"syntax\"|\"essential\"|\"all\", path?: string} — the file's highlighting level (shown in the status bar); without `level` opens the popup",
            Context::Global,
            has_editor,
            configure_level,
        ),
        spec(
            "code.analyzeCode.viewOfflineInspectionResults",
            "View Offline Inspection Results…",
            None,
            "{path?: string} — open inspection results exported to JSON (Inspection Results › Export); without `path` asks for the file",
            Context::Global,
            always,
            view_offline,
        ),
        spec(
            "inspections.export",
            "Export Inspection Results",
            None,
            "{path?: string} — write the Inspection Results to a JSON file (View Offline Inspection Results reads it)",
            Context::Global,
            |s| s.inspections.results.as_ref().map(|_| ()).ok_or_else(|| "no inspection results".into()),
            export_results,
        ),
        spec(
            "analysis.inspections",
            "Inspections",
            None,
            "{query?: string} → the inspections Run Inspection by Name offers [{name, tool, description}]",
            Context::Global,
            always,
            |s, p| Ok(json!(filter(&catalogue(s), opt_str(p, "query").unwrap_or_default()))),
        ),
        spec("analysis.dismiss", "Close", None, "Close the Analyze Code popups", Context::Global, always, |s, _| {
            s.analysis.by_name = None;
            s.analysis.level_popup = None;
            ok()
        }),
        spec(
            "analysis.byName.run",
            "Run Selected Inspection",
            None,
            "{name: string} — run an inspection from the open chooser with its scope",
            Context::Global,
            |s| s.analysis.by_name.as_ref().map(|_| ()).ok_or_else(|| "the chooser is not open".into()),
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                let scope = s.analysis.by_name.as_ref().map(|d| d.scope.clone()).unwrap_or_else(|| "project".into());
                run_by_name(s, &json!({ "name": name, "scope": scope }))
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use codecraft_workspace::{Problem, ProblemSource, Severity};

    #[test]
    fn names_resolve_to_tools_and_filters() {
        assert_eq!(resolve("Clippy"), (Some(vec![Tool::Clippy]), None));
        assert_eq!(resolve("ruff"), (Some(vec![Tool::Ruff]), None));
        assert_eq!(resolve("clippy::needless_return"), (Some(vec![Tool::Clippy]), Some("clippy::needless_return".into())));
        assert_eq!(resolve("F401"), (Some(vec![Tool::Ruff]), Some("F401".into())));
        assert_eq!(resolve("clippy::made_up"), (Some(vec![Tool::Clippy]), Some("clippy::made_up".into())));
        assert_eq!(resolve("Weird Name"), (None, Some("Weird Name".into())));
        let s = Session::new();
        let all = catalogue(&s);
        assert!(filter(&all, "needless").iter().all(|i| i.name.contains("needless") || i.description.contains("needless")));
        assert!(filter(&all, "").len() > 30);
    }

    #[test]
    fn keep_only_renumbers() {
        use crate::inspections::{Finding, Scope};
        let mk = |insp: &str| Finding::new(Tool::Clippy, insp, Severity::Warning, "/a.rs".into(), Default::default());
        let mut r = InspectionResults {
            kind: JobKind::Inspect,
            scope: Scope { kind: ScopeKind::Project, path: None },
            scope_label: String::new(),
            findings: vec![mk("clippy::a"), mk("clippy::needless_return"), mk("unused_variables"), mk("clippy::needless_return")],
            tools: Vec::new(),
            truncated: false,
            hashes: Default::default(),
            resolved: Default::default(),
        };
        inspections::keep_only(&mut r, "needless_return");
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings.iter().map(|f| f.id).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn analysis_levels_filter_highlighting_and_offline_results_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": file })).unwrap();
        let path = s.ws.active().unwrap().1.path.clone().unwrap();
        let p = |sev, code: Option<&str>| Problem {
            path: path.clone(),
            start: Default::default(),
            end: Default::default(),
            severity: sev,
            message: "m".into(),
            code: code.map(str::to_string),
            source: ProblemSource::RustAnalyzer,
        };
        s.ws.set_lsp_problems(
            path.clone(),
            vec![p(Severity::Error, None), p(Severity::Error, Some("E0425")), p(Severity::Warning, None), p(Severity::Hint, None)],
        );
        assert_eq!(s.ws.highlighted_problems_for(&path).len(), 4);
        let v = s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({})).unwrap();
        assert_eq!(v["popup"], true);
        s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "essential" })).unwrap();
        assert_eq!(s.ws.highlighted_problems_for(&path).len(), 3);
        s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "syntax" })).unwrap();
        assert_eq!(s.ws.highlighted_problems_for(&path).len(), 1);
        s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "none" })).unwrap();
        assert!(s.ws.highlighted_problems_for(&path).is_empty());
        assert_eq!(s.ws.problems_for(&path).len(), 4, "the Problems view still has them");
        s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "all" })).unwrap();
        assert!(s.ws.analysis_levels.is_empty());
        assert!(s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "loud" })).is_err());

        // Offline results: export, then view.
        use crate::inspections::{Finding, Scope};
        let mut f = Finding::new(Tool::Clippy, "clippy::x", Severity::Warning, path.clone(), Default::default());
        f.id = 7;
        s.inspections.results = Some(InspectionResults {
            kind: JobKind::Inspect,
            scope: Scope { kind: ScopeKind::Project, path: None },
            scope_label: "Project 'p'".into(),
            findings: vec![f],
            tools: Vec::new(),
            truncated: false,
            hashes: Default::default(),
            resolved: Default::default(),
        });
        let out = tmp.path().join("r.json");
        assert_eq!(s.execute("inspections.export", &json!({})).unwrap()["picker"], true);
        assert!(matches!(s.effects.last(), Some(Effect::PickFile { save: true, .. })));
        s.execute("inspections.export", &json!({ "path": out })).unwrap();
        s.inspections.results = None;
        let v = s.execute("code.analyzeCode.viewOfflineInspectionResults", &json!({ "path": out })).unwrap();
        assert_eq!(v["problems"], 1);
        let r = s.inspections.results.as_ref().unwrap();
        assert_eq!(r.findings[0].id, 0);
        assert!(r.scope_label.starts_with("Offline (r.json)"));
        std::fs::write(&out, "{ nope").unwrap();
        assert!(s.execute("code.analyzeCode.viewOfflineInspectionResults", &json!({ "path": out })).is_err());
    }
}
