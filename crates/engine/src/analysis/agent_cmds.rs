//! Code analysis for agents: start a run and get a job id (`analysis.run`), read its problems as
//! compact JSON with filters and pages (`analysis.results`), fix (`analysis.fix`), suppress
//! (`analysis.suppress`), export (`analysis.export`), baselines (`analysis.baseline.*`), the
//! analyzers available (`analysis.tools`) and every problem the IDE knows (`analysis.projectProblems`).
//!
//! Problems are reported with project-relative paths, 1-based lines and columns (columns count
//! characters), the analyzer id, the rule id, a severity (`error`, `warning`, `weak_warning`,
//! `info`), the message, the quick-fix (title and whether it is safe) and the source line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use codecraft_workspace::{ProblemSource, ToolWindowId};

use super::export::{self, Format};
use super::glob::PathGlob;
use super::jobs::JobStatus;
use super::profiles::Level;
use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, spec};
use crate::inspections::{self, Finding, InspectionResults, JobKind, ScopeKind, StartOptions, Tool, fixes};
use crate::params::{bool_or, opt_path, opt_str, opt_usize};
use crate::{CommandError, Session};

const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 500;
/// Inline export content above this goes to a file instead.
const MAX_INLINE: usize = 4 << 20;
const MAX_EXCERPT: usize = 200;

// ---------------------------------------------------------------- params

/// Analyzer ids or languages (`rust`, `python`, `cpp`, `sql`) → tools.
pub fn analyzers_param(p: &Value) -> Result<Option<Vec<Tool>>, CommandError> {
    let list: Vec<String> = match p.get("analyzers").or_else(|| p.get("tools")) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| CommandError::BadParams(format!("unknown analyzer {v}"))))
            .collect::<Result<_, _>>()?,
        Some(v) => return Err(CommandError::BadParams(format!("`analyzers` must be a list of analyzer ids, not {v}"))),
    };
    let mut out: Vec<Tool> = Vec::new();
    for name in list {
        let lang: &[Tool] = match name.to_ascii_lowercase().as_str() {
            "rust" => &[Tool::Clippy, Tool::RustAnalyzer],
            "python" | "py" => &[Tool::Ruff, Tool::Mypy, Tool::Pyright],
            "cpp" | "c++" | "c" | "c/c++" | "cxx" => &[Tool::Clangd, Tool::ClangAnalyzer, Tool::ClangTidy, Tool::Cppcheck],
            "javascript" | "typescript" | "js" | "ts" => &[Tool::Eslint],
            _ => &[],
        };
        if lang.is_empty() {
            let t = Tool::parse(&name).ok_or_else(|| {
                CommandError::BadParams(format!(
                    "unknown analyzer '{name}' (ids: {}; or rust, python, cpp, sql)",
                    Tool::ALL.iter().map(|t| t.id()).collect::<Vec<_>>().join(", ")
                ))
            })?;
            if !out.contains(&t) {
                out.push(t);
            }
        } else {
            for t in lang {
                if !out.contains(t) {
                    out.push(*t);
                }
            }
        }
    }
    if out.is_empty() {
        return Err(CommandError::BadParams("`analyzers` is empty".into()));
    }
    Ok(Some(out))
}

fn format_param(p: &Value, default: Format) -> Result<Format, CommandError> {
    match opt_str(p, "format") {
        None => Ok(default),
        Some(f) => Format::parse(f).ok_or_else(|| CommandError::BadParams(format!("unknown format '{f}' (json, sarif, html)"))),
    }
}

fn job_param(s: &Session, p: &Value) -> Result<u64, CommandError> {
    match p.get("job") {
        None | Some(Value::Null) => s
            .inspections
            .running
            .as_ref()
            .map(|r| r.job)
            .or_else(|| s.inspections.results.as_ref().map(|r| r.job))
            .or_else(|| s.analysis.jobs.last_id())
            .ok_or_else(|| CommandError::Failed("no analysis has run yet (start one with analysis.run)".into())),
        Some(v) => v.as_u64().ok_or_else(|| CommandError::BadParams(format!("`job` must be a job id, not {v}"))),
    }
}

/// The results of a job: the live ones when it is the current run, else the kept copy.
fn job_results(s: &Session, job: u64) -> Result<&InspectionResults, CommandError> {
    if let Some(r) = s.inspections.results.as_ref().filter(|r| r.job == job) {
        return Ok(r);
    }
    match s.analysis.jobs.get(job) {
        Some(rec) => match (&rec.status, &rec.results) {
            (JobStatus::Done, Some(r)) => Ok(r),
            (JobStatus::Running, _) => Err(CommandError::Failed(format!("job {job} is still running"))),
            (JobStatus::Cancelled, _) => Err(CommandError::Failed(format!("job {job} was cancelled"))),
            _ => Err(CommandError::Failed(format!("job {job} has no results"))),
        },
        None => Err(CommandError::Failed(format!("no analysis job {job} (only the last few are kept)"))),
    }
}

/// Fixing and suppressing work on the current results only (positions move with each fix).
fn live_results(s: &Session, p: &Value) -> Result<u64, CommandError> {
    let live = s.inspections.results.as_ref().ok_or_else(|| CommandError::Failed("no analysis results".into()))?;
    match p.get("job").and_then(Value::as_u64) {
        Some(j) if j != live.job => {
            Err(CommandError::Failed(format!("job {j} is not the current results (job {}); run analysis.run again", live.job)))
        }
        _ => Ok(live.job),
    }
}

fn ids_param(p: &Value) -> Result<Vec<usize>, CommandError> {
    let mut ids = Vec::new();
    if let Some(id) = opt_usize(p, "id")? {
        ids.push(id);
    }
    if let Some(a) = p.get("ids") {
        let a = a.as_array().ok_or_else(|| CommandError::BadParams("`ids` must be a list of problem ids".into()))?;
        for v in a.iter().take(10_000) {
            let id = v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| CommandError::BadParams(format!("bad problem id {v}")))?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------- the compact problem JSON

pub(crate) fn rel_path(root: Option<&Path>, p: &Path) -> String {
    match root.and_then(|r| p.strip_prefix(r).ok()) {
        Some(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => p.to_string_lossy().into_owned(),
    }
}

fn excerpt(texts: &mut HashMap<PathBuf, Option<String>>, s: &Session, f: &Finding) -> Option<String> {
    let text = texts.entry(f.path.clone()).or_insert_with(|| s.current_text(&f.path));
    let line = text.as_deref()?.split('\n').nth(f.start.line)?;
    let t = line.trim_end();
    Some(if t.chars().count() > MAX_EXCERPT { format!("{}…", t.chars().take(MAX_EXCERPT).collect::<String>()) } else { t.to_string() })
}

/// Lines `line - before ..= line + after` of a file as the editor has it (open buffer or disk):
/// (zero-based line, text), for previews.
pub fn source_lines(s: &Session, path: &Path, line: usize, before: usize, after: usize) -> Vec<(usize, String)> {
    let Some(text) = s.current_text(path) else { return Vec::new() };
    let first = line.saturating_sub(before);
    text.split('\n')
        .enumerate()
        .skip(first)
        .take(line.saturating_sub(first).saturating_add(after).saturating_add(1))
        .map(|(i, l)| (i, l.trim_end().chars().take(240).collect()))
        .collect()
}

fn problem_json(
    s: &Session,
    root: Option<&Path>,
    r: &InspectionResults,
    f: &Finding,
    texts: &mut HashMap<PathBuf, Option<String>>,
    with_excerpt: bool,
) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), json!(f.id));
    o.insert("path".into(), json!(rel_path(root, &f.path)));
    o.insert("line".into(), json!(f.start.line + 1));
    o.insert("column".into(), json!(f.start.column + 1));
    if f.end > f.start {
        o.insert("endLine".into(), json!(f.end.line + 1));
        o.insert("endColumn".into(), json!(f.end.column + 1));
    }
    o.insert("severity".into(), json!(Level::of(f.severity).id()));
    o.insert("analyzer".into(), json!(f.tool.id()));
    o.insert("rule".into(), json!(f.rule()));
    o.insert("message".into(), json!(f.message.lines().next().unwrap_or("")));
    let fix = f.safe_fix().or_else(|| f.fixes.iter().find(|x| x.applicability.is_offered()));
    o.insert(
        "fix".into(),
        fix.map_or(Value::Null, |x| json!({ "title": x.title, "safe": x.applicability.is_safe() })),
    );
    o.insert("suppress".into(), json!(f.code.is_some() && !matches!(f.tool, Tool::ZigFmt | Tool::CargoAudit | Tool::CargoDeny)));
    if r.baseline.is_some() {
        o.insert("new".into(), json!(!f.baseline));
    }
    if with_excerpt && let Some(e) = excerpt(texts, s, f) {
        o.insert("excerpt".into(), json!(e));
    }
    Value::Object(o)
}

/// A filter over findings.
struct Filter {
    severities: Option<Vec<Level>>,
    analyzers: Option<Vec<Tool>>,
    rule: Option<String>,
    path: Option<PathGlob>,
    only_new: bool,
    fixable: bool,
    query: Option<String>,
}

impl Filter {
    fn parse(p: &Value) -> Result<Self, CommandError> {
        let f = p.get("filter").cloned().unwrap_or(Value::Null);
        let f = if f.is_object() { f } else { json!({}) };
        let strings = |v: Option<&Value>| -> Result<Option<Vec<String>>, CommandError> {
            match v {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())),
                Some(Value::Array(a)) => Ok(Some(a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())),
                Some(v) => Err(CommandError::BadParams(format!("bad filter value {v}"))),
            }
        };
        let severities = match strings(f.get("severity"))? {
            None => None,
            Some(list) => Some(
                list.iter()
                    .map(|x| Level::parse(x).ok_or_else(|| CommandError::BadParams(format!("unknown severity '{x}'"))))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        let analyzers = match f.get("analyzer").or_else(|| f.get("analyzers")) {
            None | Some(Value::Null) => None,
            Some(v) => analyzers_param(&json!({ "analyzers": v }))?,
        };
        let path = match f.get("path").and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()) {
            None => None,
            Some(g) => {
                // A plain directory or file prefix matches everything under it.
                let g = if g.contains(['*', '?', ',', '!']) { g.to_string() } else { format!("{},{}/**", g.trim_end_matches('/'), g.trim_end_matches('/')) };
                Some(PathGlob::parse(&g).map_err(CommandError::BadParams)?)
            }
        };
        Ok(Self {
            severities,
            analyzers,
            rule: f.get("rule").and_then(Value::as_str).map(str::to_string),
            path,
            only_new: f.get("onlyNew").and_then(Value::as_bool).unwrap_or(false),
            fixable: f.get("fixable").and_then(Value::as_bool).unwrap_or(false),
            query: f.get("query").and_then(Value::as_str).map(str::to_lowercase).filter(|q| !q.is_empty()),
        })
    }

    fn matches(&self, root: Option<&Path>, f: &Finding) -> bool {
        self.severities.as_ref().is_none_or(|l| l.contains(&Level::of(f.severity)))
            && self.analyzers.as_ref().is_none_or(|a| a.contains(&f.tool) || (f.tool == Tool::CargoCheck && a.contains(&Tool::Clippy)))
            && self.rule.as_deref().is_none_or(|r| f.rule() == r || f.inspection == r)
            && self.path.as_ref().is_none_or(|g| g.matches(Path::new(&rel_path(root, &f.path))))
            && (!self.only_new || !f.baseline)
            && (!self.fixable || f.fixes.iter().any(|x| x.applicability.is_offered()))
            && self.query.as_deref().is_none_or(|q| {
                f.message.to_lowercase().contains(q) || f.rule().to_lowercase().contains(q) || f.inspection.to_lowercase().contains(q)
            })
    }
}

fn counts(r: &InspectionResults) -> Value {
    let mut m = Map::new();
    for l in Level::ALL {
        m.insert(l.id().into(), json!(r.open().filter(|f| Level::of(f.severity) == l).count()));
    }
    Value::Object(m)
}

fn analyzers_json(r: &InspectionResults) -> Value {
    json!(r
        .tools
        .iter()
        .map(|t| json!({ "id": t.tool.id(), "ok": t.ok, "outcome": t.outcome, "command": t.command_line }))
        .collect::<Vec<_>>())
}

// ---------------------------------------------------------------- commands

fn run_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let scope = match opt_str(p, "scope") {
        None => ScopeKind::Project,
        Some(v) => ScopeKind::parse(v)
            .ok_or_else(|| CommandError::BadParams(format!("unknown scope '{v}' (file, directory, project, uncommitted, glob)")))?,
    };
    let tools = analyzers_param(p)?;
    let format = format_param(p, Format::Json)?;
    let pattern = opt_str(p, "pattern").or_else(|| opt_str(p, "glob")).map(str::to_string);
    let opts = StartOptions {
        tools: tools.clone(),
        format: true,
        pattern,
        profile: opt_str(p, "profile").map(str::to_string),
        quiet: !bool_or(p, "show", false),
        ..StartOptions::default()
    };
    let job = inspections::start_opts(s, JobKind::Inspect, scope, opt_path(p, "path"), opts)?;
    super::jobs::set_format(s, job, format.id());
    let running = s.inspections.running.as_ref();
    Ok(json!({
        "job": job,
        "status": "running",
        "scope": running.map(|r| r.scope.clone()),
        "profile": s.analysis.profiles.active,
        "analyzers": tools.map(|t| t.iter().map(|x| x.id()).collect::<Vec<_>>()),
        "next": "poll analysis.results {job} until status is done",
    }))
}

fn results_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let job = job_param(s, p)?;
    if let Some(r) = s.inspections.running.as_ref().filter(|r| r.job == job) {
        return Ok(json!({ "job": job, "status": "running", "progress": r.progress, "scope": r.scope }));
    }
    if s.analysis.jobs.get(job).is_some_and(|j| j.status == JobStatus::Cancelled) {
        return Ok(json!({ "job": job, "status": "cancelled" }));
    }
    let default_format = s.analysis.jobs.get(job).and_then(|j| Format::parse(&j.format)).unwrap_or(Format::Json);
    let format = format_param(p, default_format)?;
    let root = s.project_root();
    let r = job_results(s, job)?;
    match format {
        Format::Sarif => return Ok(json!({ "job": job, "status": "done", "format": "sarif", "sarif": export::to_sarif(r, root.as_deref()) })),
        Format::Html => return Ok(json!({ "job": job, "status": "done", "format": "html", "html": export::to_html(r, root.as_deref()) })),
        Format::Json => {}
    }
    let filter = Filter::parse(p)?;
    let page_size = opt_usize(p, "pageSize")?.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let page = opt_usize(p, "page")?.unwrap_or(1).max(1);
    let with_excerpt = bool_or(p, "excerpt", true);
    let mut list: Vec<&Finding> = r.open().filter(|f| filter.matches(root.as_deref(), f)).collect();
    list.sort_by(|a, b| (a.severity, &a.path, a.start).cmp(&(b.severity, &b.path, b.start)));
    let total = list.len();
    let pages = total.div_ceil(page_size).max(1);
    let mut texts = HashMap::new();
    let problems: Vec<Value> = list
        .iter()
        .skip(page.saturating_sub(1).saturating_mul(page_size))
        .take(page_size)
        .map(|f| problem_json(s, root.as_deref(), r, f, &mut texts, with_excerpt))
        .collect();
    Ok(json!({
        "job": job,
        "status": "done",
        "scope": r.scope_label,
        "profile": r.profile,
        "baseline": r.baseline.as_deref().map(|b| rel_path(root.as_deref(), b)),
        "analyzers": analyzers_json(r),
        "counts": counts(r),
        "total": total,
        "page": page,
        "pages": pages,
        "pageSize": page_size,
        "truncated": r.truncated,
        "problems": problems,
    }))
}

fn fix_cmd(s: &mut Session, p: &Value) -> CmdResult {
    live_results(s, p)?;
    let ids = ids_param(p)?;
    let rule = opt_str(p, "rule").map(str::to_string);
    let maybe = bool_or(p, "maybeIncorrect", false);
    let preview = bool_or(p, "preview", false);
    let results = s.inspections.results.as_ref().ok_or_else(|| CommandError::Failed("no analysis results".into()))?;
    let (title, list) = match (&rule, ids.as_slice()) {
        (Some(rule), []) => {
            let list: Vec<_> = results
                .open()
                .filter(|f| f.rule() == rule || &f.inspection == rule)
                .filter_map(|f| {
                    let fix = f.safe_fix().or_else(|| maybe.then(|| f.fixes.iter().find(|x| x.applicability.is_offered())).flatten())?;
                    Some((f.id, fix.clone()))
                })
                .collect();
            (format!("Fix all '{rule}' problems"), list)
        }
        (None, []) => return Err(CommandError::BadParams("pass `id`, `ids` or `rule`".into())),
        (_, ids) => {
            let index = opt_usize(p, "fix")?.unwrap_or(0);
            let mut list = Vec::new();
            for id in ids {
                let f = results.finding(*id).ok_or_else(|| CommandError::BadParams(format!("no problem {id}")))?;
                if results.resolved.contains(id) {
                    continue;
                }
                let fix = if ids.len() == 1 {
                    f.fixes.get(index).filter(|x| x.applicability.is_offered() || index > 0)
                } else {
                    f.safe_fix().or_else(|| maybe.then(|| f.fixes.iter().find(|x| x.applicability.is_offered())).flatten())
                };
                if let Some(fix) = fix {
                    list.push((f.id, fix.clone()));
                }
            }
            let title = match list.as_slice() {
                [(_, one)] => one.title.clone(),
                _ => "Apply quick-fixes".to_string(),
            };
            (title, list)
        }
    };
    if list.is_empty() {
        return Err(CommandError::Failed("no applicable fix (safe fixes only unless `maybeIncorrect: true`)".into()));
    }
    let mut v = fixes::apply(s, &title, list, preview)?;
    v["remaining"] = json!(s.inspections.results.as_ref().map_or(0, |r| r.open().count()));
    Ok(v)
}

fn suppress_cmd(s: &mut Session, p: &Value) -> CmdResult {
    live_results(s, p)?;
    let ids = ids_param(p)?;
    if ids.is_empty() {
        return Err(CommandError::BadParams("pass `id` or `ids`".into()));
    }
    let results = s.inspections.results.clone().ok_or_else(|| CommandError::Failed("no analysis results".into()))?;
    let mut list = Vec::new();
    let mut errors = Vec::new();
    for id in ids {
        let Some(f) = results.finding(id) else {
            errors.push(json!({ "id": id, "error": "no such problem" }));
            continue;
        };
        let Some(text) = s.current_text(&f.path) else {
            errors.push(json!({ "id": id, "error": "the file cannot be read" }));
            continue;
        };
        match fixes::suppress_change(f, &text) {
            Ok(change) => {
                let title = inspections::suppress_label(f);
                list.push((f.id, inspections::Fix { title, applicability: inspections::Applicability::MachineApplicable, changes: vec![change] }));
            }
            Err(e) => errors.push(json!({ "id": id, "error": e })),
        }
    }
    if list.is_empty() {
        return Err(CommandError::Failed(format!("nothing to suppress: {}", Value::Array(errors))));
    }
    let title = if list.len() == 1 { list.first().map(|(_, f)| f.title.clone()).unwrap_or_default() } else { "Suppress problems".into() };
    let mut v = fixes::apply(s, &title, list, bool_or(p, "preview", false))?;
    v["errors"] = json!(errors);
    Ok(v)
}

fn export_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let job = job_param(s, p)?;
    let path = opt_path(p, "path").map(|x| crate::cpp::resolve_path(s, &x));
    let default = path.as_deref().and_then(Format::from_path).unwrap_or(Format::Json);
    let format = format_param(p, default)?;
    let root = s.project_root();
    let r = job_results(s, job)?;
    let problems = r.open().count();
    let text = match format {
        Format::Json => serde_json::to_string_pretty(&export::to_json(r)),
        Format::Sarif => serde_json::to_string_pretty(&export::to_sarif(r, root.as_deref())),
        Format::Html => Ok(export::to_html(r, root.as_deref())),
    }
    .map_err(|e| CommandError::Failed(e.to_string()))?;
    match path {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", dir.display())))?;
            }
            codecraft_fs::write_text_file_atomic(&path, &text)
                .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
            s.status = format!("Exported {problems} problem(s) to {}", path.display());
            Ok(json!({ "job": job, "format": format.id(), "path": path, "problems": problems, "bytes": text.len() }))
        }
        None if text.len() > MAX_INLINE => Err(CommandError::Failed(format!("the export is {} bytes; pass `path` to write it to a file", text.len()))),
        None => {
            let content = match format {
                Format::Html => Value::String(text),
                _ => serde_json::from_str(&text).unwrap_or(Value::String(text)),
            };
            Ok(json!({ "job": job, "format": format.id(), "problems": problems, "content": content }))
        }
    }
}

fn baseline_save(s: &mut Session, p: &Value) -> CmdResult {
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let path = opt_path(p, "path").map(|x| crate::cpp::resolve_path(s, &x)).unwrap_or_else(|| super::baseline::default_path(&root));
    let r = s.inspections.results.as_ref().ok_or_else(|| CommandError::Failed("no analysis results to save as a baseline".into()))?;
    let b = super::baseline::save(&path, &root, r).map_err(CommandError::Failed)?;
    let total = b.total;
    s.analysis.baseline.current = Some(std::sync::Arc::new(b));
    super::baseline::remark(s);
    s.status = format!("Baseline saved: {total} problem(s)");
    Ok(json!({ "path": path, "problems": total }))
}

fn baseline_load(s: &mut Session, p: &Value) -> CmdResult {
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let path = opt_path(p, "path").map(|x| crate::cpp::resolve_path(s, &x)).unwrap_or_else(|| super::baseline::default_path(&root));
    let b = super::baseline::load(&path).map_err(CommandError::Failed)?;
    let total = b.total;
    s.analysis.baseline.current = Some(std::sync::Arc::new(b));
    super::baseline::remark(s);
    let new = s.inspections.results.as_ref().map(|r| r.open().filter(|f| !f.baseline).count());
    Ok(json!({ "path": path, "problems": total, "new": new }))
}

fn baseline_clear(s: &mut Session, p: &Value) -> CmdResult {
    let delete = bool_or(p, "delete", false);
    let old = s.analysis.baseline.current.take();
    if delete && let Some(b) = &old {
        std::fs::remove_file(&b.path).map_err(|e| CommandError::Failed(format!("cannot delete {}: {e}", b.path.display())))?;
    }
    s.inspections.only_new = false;
    super::baseline::remark(s);
    Ok(json!({ "cleared": old.is_some(), "deleted": delete && old.is_some() }))
}

fn tools_cmd(s: &mut Session, _: &Value) -> CmdResult {
    let root = s.project_root().unwrap_or_default();
    let env = super::analyzers::env(s, &root);
    let list: Vec<Value> = Tool::ALL
        .iter()
        .map(|t| {
            let program: Option<PathBuf> = s.inspections.programs.get(t).cloned().or_else(|| match t {
                Tool::Clippy | Tool::CargoCheck => Some(codecraft_cargo::find_cargo()),
                Tool::Ruff => codecraft_lsp::which("ruff"),
                Tool::Eslint => codecraft_lsp::which("eslint"),
                Tool::ClangTidy => codecraft_lsp::which("clang-tidy"),
                Tool::ZigFmt => codecraft_lsp::which("zig"),
                Tool::Mypy => crate::python_lint::find(&root, *t).or_else(|| env.managed_tool("mypy")),
                Tool::Pyright => crate::python_lint::find(&root, *t).or_else(|| env.managed_tool("basedpyright")),
                Tool::CargoAudit | Tool::CargoDeny => crate::cargo_audit::find(*t),
                Tool::RustAnalyzer => codecraft_lsp::find_rust_analyzer(),
                Tool::ClangAnalyzer => env.clang.clone(),
                Tool::Clangd => env.clangd.clone(),
                Tool::Cppcheck => codecraft_lsp::which("cppcheck"),
                Tool::Sql => None,
            });
            let available = program.is_some() || *t == Tool::Sql;
            let install = match t {
                Tool::Pyright => Some("analysis.installTool {tool: \"basedpyright\", confirm: true}"),
                Tool::Mypy => Some("analysis.installTool {tool: \"mypy\", confirm: true}"),
                _ => None,
            };
            json!({
                "id": t.id(),
                "label": t.label(),
                "language": t.language(),
                "available": available,
                "program": program,
                "install": if available { None } else { install },
            })
        })
        .collect();
    Ok(json!({ "analyzers": list, "installing": s.analysis.install.running.as_ref().map(|j| j.tool.clone()), "lastInstall": s.analysis.install.last }))
}

/// One problem of the Problems tool window's Project tab.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProjectProblem {
    #[serde(skip)]
    pub abs: PathBuf,
    /// Project-relative.
    pub path: String,
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
    pub severity: Level,
    /// `language_server`, `build`, `inspection` (CodeCraft's live ones) or `analysis`.
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyzer: Option<&'static str>,
    pub rule: Option<String>,
    pub message: String,
    /// The analysis problem id (for analysis.fix / analysis.suppress).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<usize>,
}

/// Every problem the IDE knows: live language-server and build problems, plus the last
/// analysis (the Problems tool window's Project tab), errors first.
pub fn project_problems(s: &Session, path: Option<&Path>) -> Vec<ProjectProblem> {
    let root = s.project_root();
    let mut out: Vec<ProjectProblem> = Vec::new();
    let all = s.ws.all_problems();
    for (file, list) in &all {
        if path.is_some_and(|p| p != *file) {
            continue;
        }
        for p in list {
            let source = match p.source {
                ProblemSource::RustAnalyzer => "language_server",
                ProblemSource::Cargo => "build",
                ProblemSource::Inspection => "inspection",
                // Analyzer findings are added below with their ids (M15 EA).
                ProblemSource::Analysis => continue,
            };
            out.push(ProjectProblem {
                abs: file.to_path_buf(),
                path: rel_path(root.as_deref(), file),
                line: p.start.line + 1,
                column: p.start.column + 1,
                severity: Level::of(p.severity),
                source,
                analyzer: None,
                rule: p.code.clone(),
                message: p.message.lines().next().unwrap_or("").to_string(),
                id: None,
            });
        }
    }
    if let Some(r) = &s.inspections.results {
        for f in r.open().filter(|f| path.is_none_or(|p| p == f.path) && !crate::analysis::editor::superseded(s, f)) {
            out.push(ProjectProblem {
                abs: f.path.clone(),
                path: rel_path(root.as_deref(), &f.path),
                line: f.start.line + 1,
                column: f.start.column + 1,
                severity: Level::of(f.severity),
                source: "analysis",
                analyzer: Some(f.tool.id()),
                rule: Some(f.rule().to_string()),
                message: f.message.lines().next().unwrap_or("").to_string(),
                id: Some(f.id),
            });
        }
    }
    // Findings of the analysis on save (M15 EA).
    for e in crate::analysis::editor::saved_findings(s, None).filter(|e| path.is_none_or(|p| p == e.finding.path)) {
        let f = &e.finding;
        out.push(ProjectProblem {
            abs: f.path.clone(),
            path: rel_path(root.as_deref(), &f.path),
            line: f.start.line + 1,
            column: f.start.column + 1,
            severity: Level::of(f.severity),
            source: "analysis",
            analyzer: Some(f.tool.id()),
            rule: Some(f.rule().to_string()),
            message: f.message.lines().next().unwrap_or("").to_string(),
            id: None,
        });
    }
    out.sort_by(|a, b| (a.severity, &a.path, a.line, a.column).cmp(&(b.severity, &b.path, b.line, b.column)));
    out
}

fn problems_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let path = match opt_str(p, "scope") {
        Some("file") => Some(
            opt_path(p, "path")
                .map(|x| crate::cpp::resolve_path(s, &x))
                .or_else(|| s.ws.active().and_then(|(_, d)| d.path.clone()))
                .ok_or_else(|| CommandError::BadParams("the file scope needs `path` (or an open editor)".into()))?,
        ),
        None | Some("project") => opt_path(p, "path").map(|x| crate::cpp::resolve_path(s, &x)),
        Some(other) => return Err(CommandError::BadParams(format!("unknown scope '{other}' (file, project)"))),
    };
    let severities: Option<Vec<Level>> = opt_str(p, "severity").map(|v| v.split(',').filter_map(Level::parse).collect());
    let mut list = project_problems(s, path.as_deref());
    if let Some(sev) = &severities {
        list.retain(|v| sev.contains(&v.severity));
    }
    let page_size = opt_usize(p, "pageSize")?.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let page = opt_usize(p, "page")?.unwrap_or(1).max(1);
    let total = list.len();
    let problems: Vec<ProjectProblem> = list.into_iter().skip((page - 1).saturating_mul(page_size)).take(page_size).collect();
    Ok(json!({ "total": total, "page": page, "pages": total.div_ceil(page_size).max(1), "problems": problems }))
}

fn has_results(s: &Session) -> Result<(), String> {
    s.inspections.results.as_ref().map(|_| ()).ok_or_else(|| "no inspection results".into())
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        spec(
            "analysis.run",
            "Analyze Code",
            None,
            "{scope?: \"project\"|\"file\"|\"directory\"|\"uncommitted\"|\"glob\" (default project), path?: string, pattern?: string (glob scope: \"src/**/*.rs, !**/gen/**\"), profile?: string, analyzers?: [\"clippy\"|\"rust_analyzer\"|\"ruff\"|\"mypy\"|\"pyright\"|\"clangd\"|\"clang_analyzer\"|\"clang_tidy\"|\"cppcheck\"|\"sql\"|… or \"rust\"|\"python\"|\"cpp\"], format?: \"json\"|\"sarif\"|\"html\", show?: bool} → {job} — start an analysis on a worker; poll analysis.results {job}",
            g,
            has_project,
            run_cmd,
        ),
        spec(
            "analysis.results",
            "Analysis Results",
            None,
            "{job?: int (default: the latest), filter?: {severity?: [\"error\"|\"warning\"|\"weak_warning\"|\"info\"], analyzer?: [id|language], rule?: string, path?: glob or directory, onlyNew?: bool, fixable?: bool, query?: string}, page?: int (1-based), pageSize?: int (≤500, default 50), format?: \"json\"|\"sarif\"|\"html\", excerpt?: bool} → {status: running|done|cancelled, counts, total, pages, problems: [{id, path, line, column, endLine?, endColumn?, severity, analyzer, rule, message, fix: {title, safe}|null, suppress, new?, excerpt}]} (errors first)",
            g,
            always,
            results_cmd,
        ),
        spec(
            "analysis.fix",
            "Apply Analysis Fixes",
            None,
            "{id?: int, ids?: [int], rule?: string (every problem of the rule), fix?: int (which fix of a single id), maybeIncorrect?: bool, preview?: bool, job?: int} — apply quick-fixes of the current results (safe ones only unless `maybeIncorrect`); files changed since the analysis are refused",
            g,
            has_results,
            fix_cmd,
        ),
        spec(
            "analysis.suppress",
            "Suppress Problems",
            None,
            "{id?: int, ids?: [int], preview?: bool, job?: int} — suppress in the source: #[allow(lint)] on the Rust item, # noqa: CODE, # type: ignore[code], // NOLINT(check), // cppcheck-suppress id, -- noinspection Id",
            g,
            has_results,
            suppress_cmd,
        ),
        spec(
            "analysis.export",
            "Export Analysis Results",
            None,
            "{job?: int, format?: \"json\"|\"sarif\"|\"html\" (default: from the path's extension, else json), path?: string} — write the results to a file, or return them inline (up to 4 MB) without `path`",
            g,
            always,
            export_cmd,
        ),
        spec(
            "analysis.baseline.save",
            "Save as Baseline",
            None,
            "{path?: string (default .codecraft/inspection-baseline.json)} — the current open problems become the baseline; later runs mark them `new: false`",
            g,
            has_results,
            baseline_save,
        ),
        spec(
            "analysis.baseline.load",
            "Load Baseline",
            None,
            "{path?: string} — compare results with a saved baseline",
            g,
            has_project,
            baseline_load,
        ),
        spec(
            "analysis.baseline.clear",
            "Clear Baseline",
            None,
            "{delete?: bool} — stop comparing with the baseline (`delete` removes its file)",
            g,
            always,
            baseline_clear,
        ),
        spec(
            "analysis.tools",
            "Analyzers",
            None,
            "→ {analyzers: [{id, label, language, available, program, install?}]} — which analyzers can run here",
            g,
            always,
            tools_cmd,
        ),
        spec(
            "analysis.projectProblems",
            "Project Problems",
            None,
            "{scope?: \"project\"|\"file\", path?: string, severity?: \"error,warning\", page?: int, pageSize?: int} → live language-server and build problems plus the last analysis, errors first (the Problems tool window's Project tab)",
            g,
            always,
            problems_cmd,
        ),
        spec(
            "analysis.cancel",
            "Stop Analysis",
            None,
            "{job?: int} — stop the running analysis",
            g,
            |s| if s.inspections.running.is_some() { Ok(()) } else { Err("no analysis is running".into()) },
            |s, p| {
                let job = s.inspections.running.as_ref().map(|r| r.job);
                if let (Some(want), Some(j)) = (p.get("job").and_then(Value::as_u64), job)
                    && want != j
                {
                    return Err(CommandError::Failed(format!("job {want} is not running (job {j} is)")));
                }
                inspections::cancel(s);
                Ok(json!({ "cancelled": job }))
            },
        ),
        spec(
            "inspections.groupBy",
            "Group Problems By",
            None,
            "{by: \"rule\"|\"severity\"|\"analyzer\"|\"file\"} — how the Inspection Results tree groups problems",
            g,
            always,
            |s, p| {
                let by = opt_str(p, "by").unwrap_or("");
                s.inspections.group_by = inspections::GroupBy::parse(by)
                    .ok_or_else(|| CommandError::BadParams(format!("unknown grouping '{by}' (rule, severity, analyzer, file)")))?;
                Ok(json!({ "by": s.inspections.group_by.id() }))
            },
        ),
        spec(
            "inspections.onlyNew",
            "Show Only New Problems",
            None,
            "{show?: bool} — hide problems that are in the baseline (toggles without `show`)",
            g,
            always,
            |s, p| {
                s.inspections.only_new = bool_or(p, "show", !s.inspections.only_new);
                s.ws.layout.show(ToolWindowId::InspectionResults);
                Ok(json!({ "onlyNew": s.inspections.only_new }))
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn analyzer_names_and_languages() {
        assert_eq!(analyzers_param(&json!({})).unwrap(), None);
        assert_eq!(analyzers_param(&json!({ "analyzers": ["rust"] })).unwrap(), Some(vec![Tool::Clippy, Tool::RustAnalyzer]));
        assert_eq!(analyzers_param(&json!({ "analyzers": "ruff, sql" })).unwrap(), Some(vec![Tool::Ruff, Tool::Sql]));
        assert_eq!(analyzers_param(&json!({ "analyzers": ["cpp"] })).unwrap().unwrap().len(), 4);
        assert!(analyzers_param(&json!({ "analyzers": ["galaxy"] })).is_err());
        assert!(analyzers_param(&json!({ "analyzers": [] })).is_err());
        assert!(analyzers_param(&json!({ "analyzers": 3 })).is_err());
    }

    #[test]
    fn filters() {
        let mut f = Finding::new(Tool::Ruff, "F401 unused-import", codecraft_workspace::Severity::Warning, "/p/src/a.py".into(), Default::default());
        f.code = Some("F401".into());
        f.message = "`os` imported but unused".into();
        let root = Some(Path::new("/p"));
        let m = |filter: Value, f: &Finding| Filter::parse(&json!({ "filter": filter })).unwrap().matches(root, f);
        assert!(m(json!({}), &f));
        assert!(m(json!({ "severity": ["warning", "error"] }), &f));
        assert!(!m(json!({ "severity": "error" }), &f));
        assert!(m(json!({ "analyzer": "python" }), &f));
        assert!(!m(json!({ "analyzer": ["clippy"] }), &f));
        assert!(m(json!({ "rule": "F401" }), &f));
        assert!(m(json!({ "rule": "F401 unused-import" }), &f));
        assert!(m(json!({ "path": "src" }), &f));
        assert!(m(json!({ "path": "src/*.py" }), &f));
        assert!(!m(json!({ "path": "tests" }), &f));
        assert!(m(json!({ "query": "IMPORTED" }), &f));
        assert!(!m(json!({ "fixable": true }), &f));
        assert!(m(json!({ "onlyNew": true }), &f));
        f.baseline = true;
        assert!(!m(json!({ "onlyNew": true }), &f));
        assert!(Filter::parse(&json!({ "filter": { "severity": "loud" } })).is_err());
        assert!(Filter::parse(&json!({ "filter": "junk" })).is_ok());
    }
}
