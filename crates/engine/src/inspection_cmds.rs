//! Code › Inspect Code…, Code Cleanup…, Analyze Stack Trace or Thread Dump…, Rearrange Code and
//! the Inspection Results tool window's actions (`inspections.*`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_workspace::{NotificationLevel, Severity, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, ok, spec};
use crate::inspections::{
    self, Finding, JobKind, ScopeDialog, ScopeKind, StackDialog, Tool, fixes, rearrange, stacktrace,
};
use crate::params::{bool_or, opt_path, opt_str, opt_usize, req_usize};
use crate::{CommandError, Effect, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.inspectCode",
            "Inspect Code…",
            None,
            "{scope?: \"file\"|\"directory\"|\"project\"|\"uncommitted\"|\"glob\", path?: string, pattern?: string (glob scope), profile?: string, tools?: [\"clippy\"|\"check\"|\"rust_analyzer\"|\"ruff\"|\"mypy\"|\"pyright\"|\"eslint\"|\"clangd\"|\"clang_analyzer\"|\"clang-tidy\"|\"cppcheck\"|\"sql\"|\"zig\"|\"cargo-audit\"|\"cargo-deny\"]} — run the analyzers the inspection profile turns on (or `tools`) on a scope into the Inspection Results tool window → {job}. Without `scope` opens the scope dialog.",
            Context::Global,
            has_project,
            |s, p| run_or_dialog(s, p, JobKind::Inspect),
        ),
        spec(
            "code.codeCleanup",
            "Code Cleanup…",
            None,
            "{scope?, path?, tools?, format?: bool} — apply every machine-applicable fix in the scope, then rustfmt / ruff format / zig fmt (`format`, default true), through the refactoring preview. Without `scope` opens the scope dialog.",
            Context::Global,
            has_project,
            |s, p| run_or_dialog(s, p, JobKind::Cleanup),
        ),
        spec(
            "code.analyzeStackTraceOrThreadDump",
            "Analyze Stack Trace or Thread Dump…",
            None,
            "{text?: string} — analyze a Rust panic/backtrace, Python traceback, Zig/sanitizer or JavaScript stack: lines with links to project files. Without `text` opens the paste dialog (prefilled from the clipboard).",
            Context::Global,
            always,
            analyze_stack,
        ),
        spec(
            "code.rearrangeCode",
            "Rearrange Code",
            None,
            "{path?: string, apply?: bool} — sort the Rust file's top-level items by kind (use, mod, macros, consts, statics, types, traits, impls, fns, inline mods) in the refactoring preview; `apply` applies at once.",
            Context::Global,
            has_editor,
            rearrange_code,
        ),
        // ------------------------------------------------ scope dialog
        spec(
            "inspections.dialog.set",
            "Set Inspection Scope",
            None,
            "{scope?: \"file\"|\"directory\"|\"project\"|\"uncommitted\"|\"glob\", pattern?: string (the custom scope's glob), profile?: string}",
            Context::Global,
            has_dialog,
            |s, p| {
                let kind = scope_param(p)?;
                let pattern = opt_str(p, "pattern").map(str::to_string);
                let profile = opt_str(p, "profile").map(str::to_string);
                if kind.is_none() && pattern.is_none() && profile.is_none() {
                    return Err(CommandError::BadParams("pass `scope`, `pattern` or `profile`".into()));
                }
                if let Some(name) = &profile {
                    crate::analysis::profiles::resolve(s, Some(name)).map_err(CommandError::BadParams)?;
                }
                if let Some(d) = &mut s.inspections.dialog {
                    if let Some(k) = kind {
                        d.scope = k;
                    }
                    if let Some(g) = pattern {
                        d.pattern = g.chars().take(4096).collect();
                        if kind.is_none() {
                            d.scope = ScopeKind::Glob;
                        }
                    }
                    if let Some(name) = profile {
                        d.profile = name;
                    }
                }
                ok()
            },
        ),
        spec("inspections.dialog.ok", "Run Inspection", None, "", Context::Global, has_dialog, |s, _| {
            let d = s.inspections.dialog.clone().ok_or_else(|| CommandError::Failed("no scope dialog".into()))?;
            let path = match d.scope {
                ScopeKind::File => d.file.clone(),
                ScopeKind::Directory => d.directory.clone(),
                _ => None,
            };
            let opts = inspections::StartOptions {
                format: true,
                pattern: (d.scope == ScopeKind::Glob).then(|| d.pattern.clone()),
                profile: Some(d.profile.clone()).filter(|p| !p.is_empty()),
                ..inspections::StartOptions::default()
            };
            // The dialog stays open on a bad scope (an empty glob, no matching file).
            inspections::start_opts(s, d.kind, d.scope, path, opts)?;
            s.inspections.dialog = None;
            ok()
        }),
        spec("inspections.dialog.cancel", "Cancel", None, "", Context::Global, always, |s, _| {
            s.inspections.dialog = None;
            ok()
        }),
        // ------------------------------------------------ results
        spec("inspections.rerun", "Rerun Inspection", None, "", Context::Global, has_last, |s, _| {
            let (kind, scope, tools) =
                s.inspections.last.clone().ok_or_else(|| CommandError::Failed("nothing to rerun".into()))?;
            let profile = s.inspections.results.as_ref().map(|r| r.profile.clone()).filter(|p| !p.is_empty());
            let opts = inspections::StartOptions { tools, format: true, pattern: scope.pattern.clone(), profile, ..Default::default() };
            let job = inspections::start_opts(s, kind, scope.kind, scope.path, opts)?;
            Ok(json!({ "job": job }))
        }),
        spec("inspections.cancel", "Stop Inspection", None, "", Context::Global, is_running, |s, _| {
            inspections::cancel(s);
            s.status = "Inspection cancelled".into();
            ok()
        }),
        spec("inspections.close", "Close Inspection Results", None, "", Context::Global, always, |s, _| {
            inspections::cancel(s);
            s.inspections.results = None;
            s.inspections.selected = None;
            s.ws.layout.hide(ToolWindowId::InspectionResults);
            ok()
        }),
        spec(
            "inspections.results",
            "Inspection Results",
            None,
            "{all?: bool} — the last run as JSON: scope, tools run, counts per severity and the open findings (with fixes); `all` includes fixed/suppressed ones",
            Context::Global,
            always,
            results_json,
        ),
        spec("inspections.select", "Select Problem", None, "{id: int}", Context::Global, has_results, |s, p| {
            let id = req_usize(p, "id")?;
            finding(s, Some(id))?;
            s.inspections.selected = Some(id);
            ok()
        }),
        spec(
            "inspections.navigate",
            "Jump to Source",
            None,
            "{id?: int} — open the problem's file at its position (default: the selected problem)",
            Context::Global,
            has_results,
            |s, p| {
                let f = finding(s, opt_usize(p, "id")?)?;
                inspections::navigate(s, f.id)?;
                ok()
            },
        ),
        spec(
            "inspections.filter",
            "Filter by Severity",
            None,
            "{severity: \"error\"|\"warning\"|\"info\"|\"hint\", show: bool}",
            Context::Global,
            always,
            |s, p| {
                let sev = opt_str(p, "severity")
                    .and_then(parse_severity)
                    .ok_or_else(|| CommandError::BadParams("`severity` must be error, warning, info or hint".into()))?;
                if bool_or(p, "show", s.inspections.hidden.contains(&sev)) {
                    s.inspections.hidden.remove(&sev);
                } else {
                    s.inspections.hidden.insert(sev);
                }
                ok()
            },
        ),
        spec(
            "inspections.fix",
            "Apply Quick-Fix",
            None,
            "{id?: int, fix?: int, preview?: bool} — apply a problem's fix (default: its first) through the refactoring preview (single-file fixes apply at once unless `preview`)",
            Context::Global,
            has_results,
            |s, p| {
                let f = finding(s, opt_usize(p, "id")?)?;
                let index = opt_usize(p, "fix")?.unwrap_or(0);
                let fix = f.fixes.get(index).cloned().ok_or_else(|| {
                    CommandError::Failed(format!("'{}' has no quick-fix{}", f.inspection, if index > 0 { " with that index" } else { "" }))
                })?;
                let title = fix.title.clone();
                fixes::apply(s, &title, vec![(f.id, fix)], bool_or(p, "preview", false))
            },
        ),
        spec(
            "inspections.fixAll",
            "Fix All Problems",
            None,
            "{inspection?: string, maybeIncorrect?: bool, preview?: bool} — apply the machine-applicable fixes of one inspection (\"Fix all 'clippy::needless_return' problems\") or of every one; `maybeIncorrect` adds suggestions marked maybe-incorrect. Opens the refactoring preview unless `preview: false`.",
            Context::Global,
            has_results,
            |s, p| {
                let inspection = opt_str(p, "inspection").map(str::to_string);
                let results = s.inspections.results.as_ref().ok_or_else(|| CommandError::Failed("no inspection results".into()))?;
                let list = inspections::fixable(results, inspection.as_deref(), bool_or(p, "maybeIncorrect", false));
                let title = match &inspection {
                    Some(i) => format!("Fix all '{i}' problems"),
                    None => "Fix all problems".to_string(),
                };
                fixes::apply(s, &title, list, bool_or(p, "preview", true))
            },
        ),
        spec(
            "inspections.suppress",
            "Suppress for Item",
            None,
            "{id?: int, preview?: bool} — suppress the problem: #[allow(lint)] on the enclosing Rust item, `# noqa: CODE` (ruff), `// eslint-disable-next-line rule`, `// NOLINT(check)` (clang-tidy)",
            Context::Global,
            has_results,
            |s, p| {
                let f = finding(s, opt_usize(p, "id")?)?;
                let text = s.current_text(&f.path).ok_or_else(|| CommandError::Failed(format!("{} cannot be read", f.path.display())))?;
                let change = fixes::suppress_change(&f, &text).map_err(CommandError::Failed)?;
                let title = inspections::suppress_label(&f);
                let fix = inspections::Fix { title: title.clone(), applicability: inspections::Applicability::MachineApplicable, changes: vec![change] };
                fixes::apply(s, &title, vec![(f.id, fix)], bool_or(p, "preview", false))
            },
        ),
        spec(
            "inspections.openDocs",
            "Open Inspection Documentation",
            None,
            "{id?: int} — open the lint's documentation in the browser",
            Context::Global,
            has_results,
            |s, p| {
                let f = finding(s, opt_usize(p, "id")?)?;
                let url = inspections::finding_url(&f).ok_or_else(|| CommandError::Failed(format!("no documentation for '{}'", f.inspection)))?;
                s.effects.push(Effect::OpenUrl(url.clone()));
                Ok(json!({ "url": url }))
            },
        ),
        // ------------------------------------------------ stack traces
        spec(
            "inspections.stackTrace.cancel",
            "Cancel",
            None,
            "",
            Context::Global,
            always,
            |s, _| {
                s.inspections.stack_dialog = None;
                ok()
            },
        ),
        spec("inspections.stackTrace.close", "Close Stack Trace", None, "", Context::Global, always, |s, _| {
            s.inspections.stack_trace = None;
            ok()
        }),
        spec(
            "inspections.stackTrace.open",
            "Open Frame",
            None,
            "{line: int} — open the file location on a line of the analyzed stack trace",
            Context::Global,
            |s| s.inspections.stack_trace.as_ref().map(|_| ()).ok_or_else(|| "no stack trace is shown".into()),
            |s, p| {
                let i = req_usize(p, "line")?;
                let link = s
                    .inspections
                    .stack_trace
                    .as_ref()
                    .and_then(|t| t.lines.get(i))
                    .and_then(|l| l.link.clone())
                    .ok_or_else(|| CommandError::BadParams(format!("line {i} has no file location")))?;
                if !link.exists {
                    return Err(CommandError::Failed(format!("{} is not on this machine", link.written)));
                }
                s.navigate_to_position(&link.path, link.line, link.column)?;
                ok()
            },
        ),
    ]
}

fn has_dialog(s: &Session) -> Result<(), String> {
    s.inspections.dialog.as_ref().map(|_| ()).ok_or_else(|| "no inspection scope dialog is open".into())
}

fn has_results(s: &Session) -> Result<(), String> {
    s.inspections.results.as_ref().map(|_| ()).ok_or_else(|| "no inspection results".into())
}

fn has_last(s: &Session) -> Result<(), String> {
    has_project(s)?;
    s.inspections.last.as_ref().map(|_| ()).ok_or_else(|| "nothing to rerun".into())
}

fn is_running(s: &Session) -> Result<(), String> {
    if s.inspections.busy() { Ok(()) } else { Err("no inspection is running".into()) }
}

pub fn parse_severity(s: &str) -> Option<Severity> {
    match s.to_ascii_lowercase().as_str() {
        "error" | "errors" => Some(Severity::Error),
        "warning" | "warnings" => Some(Severity::Warning),
        "info" | "weak warning" | "weak_warning" | "note" => Some(Severity::Info),
        "hint" | "hints" => Some(Severity::Hint),
        _ => None,
    }
}

fn scope_param(p: &Value) -> Result<Option<ScopeKind>, CommandError> {
    match opt_str(p, "scope") {
        None => Ok(None),
        Some(v) => ScopeKind::parse(v)
            .map(Some)
            .ok_or_else(|| CommandError::BadParams(format!("unknown scope `{v}` (file, directory, project, uncommitted, glob)"))),
    }
}

fn tools_param(p: &Value) -> Result<Option<Vec<Tool>>, CommandError> {
    match p.get("tools") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .and_then(Tool::parse)
                    .ok_or_else(|| CommandError::BadParams(format!("unknown tool {v} in `tools`")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(CommandError::BadParams("`tools` must be an array of tool names".into())),
    }
}

fn run_or_dialog(s: &mut Session, p: &Value, kind: JobKind) -> CmdResult {
    let tools = tools_param(p)?;
    match scope_param(p)? {
        Some(scope) => {
            let opts = inspections::StartOptions {
                tools,
                format: bool_or(p, "format", true),
                pattern: opt_str(p, "pattern").map(str::to_string),
                profile: opt_str(p, "profile").map(str::to_string),
                ..Default::default()
            };
            let job = inspections::start_opts(s, kind, scope, opt_path(p, "path"), opts)?;
            Ok(json!({ "started": true, "job": job }))
        }
        None => {
            let file = s.ws.active().and_then(|(_, d)| d.path.clone());
            let directory = file.as_deref().and_then(Path::parent).map(Path::to_path_buf);
            let previous = s.inspections.last.as_ref().map(|(_, sc, _)| sc.kind);
            let pattern = s.inspections.last.as_ref().and_then(|(_, sc, _)| sc.pattern.clone()).unwrap_or_default();
            crate::analysis::profiles::ensure_loaded(s);
            s.inspections.dialog = Some(ScopeDialog {
                kind,
                scope: previous.unwrap_or(ScopeKind::Project),
                file,
                directory,
                has_vcs: s.vcs.repo.is_some(),
                pattern,
                profile: s.analysis.profiles.active.clone(),
            });
            Ok(json!({ "dialog": true }))
        }
    }
}

fn finding(s: &Session, id: Option<usize>) -> Result<Finding, CommandError> {
    let results = s.inspections.results.as_ref().ok_or_else(|| CommandError::Failed("no inspection results".into()))?;
    let id = id.or(s.inspections.selected).ok_or_else(|| CommandError::BadParams("no problem is selected (pass `id`)".into()))?;
    results.finding(id).cloned().ok_or_else(|| CommandError::BadParams(format!("no problem {id}")))
}

fn results_json(s: &mut Session, p: &Value) -> CmdResult {
    let all = bool_or(p, "all", false);
    let running = s.inspections.running.as_ref().map(|r| {
        json!({ "kind": r.kind, "scope": r.scope, "progress": r.progress })
    });
    let Some(r) = &s.inspections.results else {
        return Ok(json!({ "running": running, "results": Value::Null }));
    };
    let findings: Vec<&Finding> = if all { r.findings.iter().collect() } else { r.open().collect() };
    let counts: serde_json::Map<String, Value> =
        r.counts().into_iter().map(|(k, v)| (k.label().to_string(), json!(v))).collect();
    Ok(json!({
        "running": running,
        "kind": r.kind,
        "scope": r.scope,
        "scopeLabel": r.scope_label,
        "tools": r.tools,
        "counts": counts,
        "truncated": r.truncated,
        "resolved": r.resolved,
        "selected": s.inspections.selected,
        "findings": findings,
    }))
}

fn analyze_stack(s: &mut Session, p: &Value) -> CmdResult {
    match opt_str(p, "text") {
        Some(text) => {
            let root = s.project_root();
            let trace = stacktrace::analyze(text, root.as_deref(), &|p: &Path| p.is_file());
            let summary = json!({
                "language": trace.language,
                "lines": trace.lines.len(),
                "links": trace.lines.iter().filter(|l| l.link.is_some()).count(),
                "projectFrames": trace.lines.iter().filter(|l| l.project && l.link.is_some()).count(),
                "firstProjectFrame": trace.first_project_frame,
            });
            s.inspections.stack_dialog = None;
            s.inspections.stack_trace = Some(trace);
            Ok(summary)
        }
        None => {
            let text = s.clipboard.as_ref().map(|(t, _)| t.clone()).unwrap_or_default();
            s.inspections.stack_dialog = Some(StackDialog { text });
            Ok(json!({ "dialog": true }))
        }
    }
}

fn rearrange_code(s: &mut Session, p: &Value) -> CmdResult {
    let path: PathBuf = match opt_path(p, "path") {
        Some(p) => crate::normalize_path(&p),
        None => s
            .ws
            .active()
            .and_then(|(_, d)| d.path.clone())
            .ok_or_else(|| CommandError::Failed("the editor has no file".into()))?,
    };
    if Language::from_path(&path) != Language::Rust {
        return Err(CommandError::Failed("Rearrange Code works on Rust files".into()));
    }
    let text = s.current_text(&path).ok_or_else(|| CommandError::Failed(format!("{} cannot be read", path.display())))?;
    let Some(new_text) = rearrange::rearrange(&text).map_err(CommandError::Failed)? else {
        s.status = "Rearrange Code: the items are already in order".into();
        return Ok(json!({ "changed": false }));
    };
    let change = inspections::linters::whole_file_change(&path, &text, new_text);
    let doc = codecraft_text::Document::from_text(&text);
    let uri = codecraft_lsp::path_to_uri(&path).map_err(|e| CommandError::Failed(e.to_string()))?;
    let range = crate::convert::lsp_range(&doc, 0..doc.len_chars());
    let edit = codecraft_lsp::lsp_types::WorkspaceEdit {
        document_changes: Some(codecraft_lsp::lsp_types::DocumentChanges::Edits(vec![
            codecraft_lsp::lsp_types::TextDocumentEdit {
                text_document: codecraft_lsp::lsp_types::OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: vec![codecraft_lsp::lsp_types::OneOf::Left(codecraft_lsp::lsp_types::TextEdit {
                    range,
                    new_text: change.new_text,
                })],
            },
        ])),
        ..Default::default()
    };
    let preview = s.build_preview("Rearrange Code", &edit)?;
    if bool_or(p, "apply", false) {
        let report = s.apply_preview(&preview);
        s.refactor.last_report = Some(report.clone());
        if !report.errors.is_empty() {
            s.notify(NotificationLevel::Error, "Rearrange Code", report.errors.join("\n"));
        }
        return Ok(json!({ "changed": true, "applied": report.applied }));
    }
    s.refactor.preview = Some(preview);
    Ok(json!({ "changed": true, "preview": true }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn severities_and_labels() {
        assert_eq!(parse_severity("Warning"), Some(Severity::Warning));
        assert_eq!(parse_severity("weak warning"), Some(Severity::Info));
        assert_eq!(parse_severity("x"), None);
        let mut f = Finding::new(Tool::Ruff, "F401", Severity::Warning, "/a.py".into(), Default::default());
        f.code = Some("F401".into());
        assert_eq!(inspections::suppress_label(&f), "Suppress with # noqa: F401");
    }

    #[test]
    fn bad_params_are_errors() {
        assert!(tools_param(&json!({ "tools": ["nope"] })).is_err());
        assert!(tools_param(&json!({ "tools": "clippy" })).is_err());
        assert_eq!(tools_param(&json!({ "tools": ["ruff", "clang-tidy"] })).unwrap(), Some(vec![Tool::Ruff, Tool::ClangTidy]));
        assert!(scope_param(&json!({ "scope": "galaxy" })).is_err());
        assert_eq!(scope_param(&json!({})).unwrap(), None);
    }
}
