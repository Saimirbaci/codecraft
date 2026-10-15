//! The Commit tool window's "Analyze code" check: before committing, inspect the files being
//! committed with the active profile; no errors or warnings → the commit goes ahead, otherwise
//! a warning offers Review (the Inspection Results), Commit Anyway or Cancel. Problems already
//! in the baseline do not count. The switch is kept in the project's inspection profiles file.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Value, json};

use codecraft_workspace::{NotificationLevel, Severity, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::inspections::{self, JobKind, ResolvedScope, Scope, ScopeKind, StartOptions};
use crate::params::bool_or;
use crate::{CommandError, Session};

/// A commit waiting for its analysis.
#[derive(Clone, Debug)]
pub struct PendingCommit {
    pub job: u64,
    /// The `vcs.commit` params to run once the analysis is clean (or the user commits anyway).
    pub params: Value,
}

/// The warning shown when the analysis found problems.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommitWarning {
    pub job: u64,
    pub errors: usize,
    pub warnings: usize,
    pub files: usize,
    #[serde(skip)]
    pub params: Value,
}

#[derive(Debug, Default)]
pub struct CommitCheckState {
    pub pending: Option<PendingCommit>,
    pub warning: Option<CommitWarning>,
}

fn with_analyzed(p: &Value) -> Value {
    let mut p = if p.is_object() { p.clone() } else { json!({}) };
    if let Some(o) = p.as_object_mut() {
        o.insert("analyzed".into(), Value::Bool(true));
    }
    p
}

/// Called by `vcs.commit` before it commits: start the analysis and hold the commit when the
/// check is on (`Some(result)` for the command), else `None` to commit now.
pub(crate) fn before_commit(s: &mut Session, p: &Value) -> Result<Option<Value>, CommandError> {
    if bool_or(p, "analyzed", false) || bool_or(p, "skipAnalysis", false) || !super::profiles::commit_check(s) {
        return Ok(None);
    }
    let Some(repo_root) = s.vcs.repo.as_ref().map(|r| r.root.clone()) else { return Ok(None) };
    let rel: Vec<String> = match p.get("files").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        None => s.vcs.included_paths(),
    };
    let files: Vec<PathBuf> = rel
        .iter()
        .map(|f| {
            let p = std::path::Path::new(f);
            crate::normalize_path(&if p.is_absolute() { p.to_path_buf() } else { repo_root.join(p) })
        })
        .filter(|p| p.is_file())
        .take(inspections::MAX_SCOPE_FILES)
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    let only: HashSet<PathBuf> = files.iter().cloned().collect();
    let resolved = ResolvedScope {
        scope: Scope::new(ScopeKind::Uncommitted, None),
        label: format!("Files to commit ({})", files.len()),
        files,
        only: Some(only),
    };
    let opts = StartOptions { resolved: Some(resolved), quiet: true, format: true, ..StartOptions::default() };
    let job = inspections::start_opts(s, JobKind::Inspect, ScopeKind::Uncommitted, None, opts)?;
    s.analysis.commit.pending = Some(PendingCommit { job, params: with_analyzed(p) });
    s.analysis.commit.warning = None;
    s.status = "Analyzing code before the commit…".into();
    Ok(Some(json!({ "analyzing": true, "job": job })))
}

/// An analysis job ended: if it was a commit's, commit or warn.
pub(crate) fn job_ended(s: &mut Session, job: u64, cancelled: bool) {
    if s.analysis.commit.pending.as_ref().is_none_or(|p| p.job != job) {
        return;
    }
    let Some(pending) = s.analysis.commit.pending.take() else { return };
    if cancelled {
        s.status = "Commit cancelled: the code analysis was stopped".into();
        return;
    }
    let Some(results) = s.inspections.results.as_ref().filter(|r| r.job == job) else { return };
    let counted: Vec<_> = results.open().filter(|f| !f.baseline && matches!(f.severity, Severity::Error | Severity::Warning)).collect();
    let errors = counted.iter().filter(|f| f.severity == Severity::Error).count();
    let warnings = counted.len() - errors;
    let files = counted.iter().map(|f| &f.path).collect::<HashSet<_>>().len();
    if errors == 0 && warnings == 0 {
        s.status = "Code analysis: no problems; committing".into();
        if let Err(e) = s.execute("vcs.commit", &pending.params) {
            s.notify(NotificationLevel::Error, "Commit", e.to_string());
        }
        return;
    }
    s.status = format!("Code analysis found {errors} error(s) and {warnings} warning(s) in {files} file(s)");
    s.analysis.commit.warning = Some(CommitWarning { job, errors, warnings, files, params: pending.params });
}

fn state(s: &mut Session, _: &Value) -> CmdResult {
    let enabled = super::profiles::commit_check(s);
    Ok(json!({
        "enabled": enabled,
        "analyzing": s.analysis.commit.pending.as_ref().map(|p| p.job),
        "warning": s.analysis.commit.warning,
    }))
}

fn has_warning(s: &Session) -> Result<(), String> {
    s.analysis.commit.warning.as_ref().map(|_| ()).ok_or_else(|| "no commit is waiting on a code analysis warning".into())
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        spec(
            "vcs.commit.analyzeCode",
            "Analyze Code Before Commit",
            None,
            "{enabled?: bool} — the Commit tool window's \"Analyze code\" check (toggles without `enabled`); kept per project",
            g,
            always,
            |s, p| {
                let on = match p.get("enabled").and_then(Value::as_bool) {
                    Some(v) => v,
                    None => !super::profiles::commit_check(s),
                };
                super::profiles::set_commit_check(s, on).map_err(CommandError::Failed)?;
                Ok(json!({ "enabled": on }))
            },
        ),
        spec(
            "analysis.commitCheck.state",
            "Commit Check State",
            None,
            "→ {enabled, analyzing: job|null, warning: {job, errors, warnings, files}|null}",
            g,
            always,
            state,
        ),
        spec("analysis.commitCheck.review", "Review", None, "Show the problems found before the commit (the commit is not made)", g, has_warning, |s, _| {
            s.analysis.commit.warning = None;
            s.ws.layout.show(ToolWindowId::InspectionResults);
            Ok(json!({ "review": true }))
        }),
        spec("analysis.commitCheck.commitAnyway", "Commit Anyway", None, "Commit despite the problems found", g, has_warning, |s, _| {
            let w = s.analysis.commit.warning.take().ok_or_else(|| CommandError::Failed("nothing to commit".into()))?;
            s.execute("vcs.commit", &w.params)
        }),
        spec("analysis.commitCheck.cancel", "Cancel Commit", None, "Drop the commit waiting on the code analysis", g, always, |s, _| {
            let had = s.analysis.commit.warning.take().is_some() | s.analysis.commit.pending.take().is_some();
            Ok(json!({ "cancelled": had }))
        }),
    ]
}
