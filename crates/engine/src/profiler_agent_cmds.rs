//! Profiling for agents (M14 track PF): record a configuration, a command line or a running
//! process as a job, poll it, and get compact reports (`profiler.record`, `profiler.job`,
//! `profiler.jobs`, `profiler.load`, `profiler.report`, `profiler.compare`, `profiler.lineHits`,
//! `profiler.timeline`). Reports work on any loaded snapshot; a snapshot that isn't loaded yet
//! is loaded in the background and the command answers `{pending: true, job}`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation};
use codecraft_profile::{HotspotOrder, Selection};

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::{opt_str, opt_usize, req_str};
use crate::profiler::{self, Cached, Job, JobState, ProfileKind, ProfilerTool, RecordRequest};
use crate::profiler_report::{self, ReportOptions};
use crate::runs::RunConfig;
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "profiler.record",
            "Record Profile",
            None,
            "{config?: int|name, command?: string|[string], cwd?: string, test?: {path, line (1-based)}, pid?: int, \
             python?: bool, kind?: cpu|memory, tool?: xctrace|samply|perf|py-spy|cProfile|tracemalloc|memray|leaks|\
             heaptrack, seconds?: number, interval?: number, target?: string|object} — profile a run configuration (default: the selected \
             one), a command line (`cargo test name`, `pytest -k x`, `./build/app --fast`, `python app.py`), the test \
             at a source line (`test`: what the gutter ▶ runs there — `cargo test --lib name -- --exact`, pytest \
             `file::test`, a GoogleTest/Catch2/doctest/Boost.Test filter) or a running process (`pid`). Cargo \
             targets and CMake chains are built first; `seconds` stops the recording early; `interval` = seconds \
             between tracemalloc memory samples (default 0.1; 0 = only the snapshot at the end). → {job, state, \
             tool, tab}; poll profiler.job, then profiler.report {snapshot: job}. On an SSH/Docker target (the \
             configuration's, or `target`): cProfile/tracemalloc/py-spy for Python, samply/perf for native code run \
             there and the snapshot comes back with local paths",
            Context::Global,
            always,
            record,
        ),
        spec(
            "profiler.job",
            "Profiling Job",
            None,
            "{job?: int} — a profiling job (default: the newest) → {job, title, kind, tool, state: \
             building|recording|loading|done|failed, snapshot, error, seconds}",
            Context::Global,
            always,
            |s, p| {
                let id = opt_usize(p, "job")?.map(|j| j as u64);
                let job = match id {
                    Some(id) => s.profiler.job(id),
                    None => s.profiler.jobs.last(),
                };
                job.map(job_json).ok_or_else(|| CommandError::Failed("no such profiling job".into()))
            },
        ),
        spec(
            "profiler.jobs",
            "Profiling Jobs",
            None,
            "→ [job] oldest first (see profiler.job)",
            Context::Global,
            always,
            |s, _| Ok(json!(s.profiler.jobs.iter().map(job_json).collect::<Vec<_>>())),
        ),
        spec(
            "profiler.load",
            "Load Profiler Snapshot",
            None,
            "{path: string} — load a snapshot for reports without opening it → {loaded: true} or {job} to poll",
            Context::Global,
            always,
            |s, p| {
                let path = profiler::resolve_path(s, Path::new(req_str(p, "path")?));
                if s.profiler.cached(&path).is_some() {
                    return Ok(json!({ "loaded": true, "snapshot": path }));
                }
                let job = profiler::load_for_report(s, &path)?;
                Ok(json!({ "job": job, "state": "loading" }))
            },
        ),
        spec(
            "profiler.report",
            "Profiler Report",
            None,
            "{snapshot?: job id|path, top?: int (15), lines?: int (10), focus?: string, sort?: self|total, thread?: \
             int|name, range?: {startMs, endMs}} — the hot spots of a snapshot (default: the open one, else the \
             newest job's) → {snapshot, unit, total, totalText, threads, sort, order, top: [{name, location: \
             file:line, selfPercent, totalPercent, self, total, calls?}], topByTotal, hotLines: [{location, \
             function, selfPercent, totalPercent, code}], focus?: {percent, inside, callers, callees, hotLines}, \
             memory?: {peak, peakMs, timeline, atPeak, peakSnapshot}, selection?: {thread, range}, notes}. `top` is \
             in a fixed order whatever the IDE's Method List shows: by self (default) or total weight, then the \
             other, then name, location. `thread` and `range` (ms since the first sample; xctrace and samply \
             snapshots) restrict everything to those samples. {pending: true, job} while it loads",
            Context::Global,
            always,
            report,
        ),
        spec(
            "profiler.compare",
            "Compare Profiler Snapshots",
            None,
            "{a: job id|path (baseline), b?: job id|path (default: the open snapshot), top?: int} → {a, b, \
             totalChangePercent, slower: [{name, location, aSelfPercent, bSelfPercent, deltaSelfPercent, aSelf, \
             bSelf}], faster, added, removed, notes}; {pending: true, job} while one loads",
            Context::Global,
            always,
            compare,
        ),
        spec(
            "profiler.lineHits",
            "Profiler Line Hits",
            None,
            "{path: string, snapshot?: job id|path, thread?: int|name, range?: {startMs, endMs}} — samples per \
             source line of a file (the editor gutter's numbers), of one thread and a time range when given → \
             {file, unit, lines: [{line, self, total, selfPercent, totalPercent}]}",
            Context::Global,
            always,
            line_hits,
        ),
        spec(
            "profiler.timeline",
            "Profiler Thread Timeline",
            None,
            "{snapshot?: job id|path, buckets?: int (50)} → {durationMs, buckets, threads: [{name, total, \
             activity: [weight per time slice]}]}",
            Context::Global,
            always,
            |s, p| {
                let buckets = opt_usize(p, "buckets")?.unwrap_or(50);
                match resolve(s, p.get("snapshot"))? {
                    Resolved::Ready(c, _) => Ok(profiler_report::timeline(&c.profile, buckets)),
                    Resolved::Pending(v) => Ok(v),
                }
            },
        ),
        spec(
            "profiler.memory",
            "Profiler Memory Timeline",
            None,
            "{snapshot?: job id|path, points?: int (100), top?: int (20)} — memory over time of a tracemalloc \
             profile with periodic snapshots → {peak, peakText, peakMs, intervalMs, timeline: [{ms, current, \
             peak}], atPeak: [{name, location, size, sizeText, count, percentOfPeak, callers}], peakSnapshot: \
             path (profiler.report it for the whole tree at the peak)}",
            Context::Global,
            always,
            |s, p| {
                let points = opt_usize(p, "points")?.unwrap_or(100);
                let top = opt_usize(p, "top")?.unwrap_or(20);
                match resolve(s, p.get("snapshot"))? {
                    Resolved::Pending(v) => Ok(v),
                    Resolved::Ready(c, _) => profiler_report::memory(&c.profile, &c.path, points, top).ok_or_else(|| {
                        CommandError::Failed(
                            "this snapshot has no memory timeline (profile memory with tracemalloc: profiler.record \
                             {kind: memory, interval: 0.1})"
                                .into(),
                        )
                    }),
                }
            },
        ),
    ]
}

/// `thread` (an index, or a name or part of one) and `range` ({startMs, endMs}) of a request.
pub(crate) fn selection_param(p: &Value, profile: &codecraft_profile::Profile) -> Result<Selection, CommandError> {
    let thread = match p.get("thread") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let i = n
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .filter(|i| *i < profile.threads.len())
                .ok_or_else(|| CommandError::BadParams(format!("no thread {n} (there are {})", profile.threads.len())))?;
            Some(i)
        }
        Some(Value::String(name)) => {
            let q = name.to_lowercase();
            let exact = profile.threads.iter().position(|t| t.name.to_lowercase() == q);
            let i = exact.or_else(|| profile.threads.iter().position(|t| t.name.to_lowercase().contains(&q)));
            Some(i.ok_or_else(|| {
                let names: Vec<&str> = profile.threads.iter().take(10).map(|t| t.name.as_str()).collect();
                CommandError::BadParams(format!("no thread matches `{name}` (threads: {})", names.join(", ")))
            })?)
        }
        Some(_) => return Err(CommandError::BadParams("`thread` is an index or a name".into())),
    };
    let range = match p.get("range") {
        None | Some(Value::Null) => None,
        Some(r) if r.is_object() => {
            let num = |k: &str| -> Result<Option<f64>, CommandError> {
                match r.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(v) => v
                        .as_f64()
                        .filter(|x| x.is_finite())
                        .map(Some)
                        .ok_or_else(|| CommandError::BadParams(format!("`range.{k}` must be a number of milliseconds"))),
                }
            };
            if !profile.has_sample_times() {
                return Err(CommandError::BadParams(format!(
                    "{} snapshots have no sample times; `range` needs an xctrace or samply snapshot",
                    profile.format.label()
                )));
            }
            let duration = profile.duration_ns.unwrap_or(0);
            let start = num("startMs")?.map_or(0, profiler::ms_to_ns);
            let end = num("endMs")?.map_or(duration, profiler::ms_to_ns);
            Some((start.min(end), end.max(start)))
        }
        Some(_) => return Err(CommandError::BadParams("`range` is {startMs, endMs}".into())),
    };
    Ok(Selection { thread, range })
}

/// `sort` of hotspot lists (`self` by default).
pub(crate) fn sort_param(p: &Value) -> Result<HotspotOrder, CommandError> {
    match opt_str(p, "sort") {
        None => Ok(HotspotOrder::SelfWeight),
        Some(x) => HotspotOrder::from_name(x)
            .ok_or_else(|| CommandError::BadParams(format!("unknown sort `{x}` (self|total)"))),
    }
}

/// A job as JSON.
pub(crate) fn job_json(j: &Job) -> Value {
    let elapsed = j.finished.unwrap_or_else(std::time::Instant::now).saturating_duration_since(j.started);
    json!({
        "job": j.id,
        "title": j.title,
        "kind": j.kind,
        "tool": j.tool,
        "state": j.state,
        "tab": j.tab.map(|t| t.0),
        "snapshot": j.snapshot,
        "error": j.error,
        "seconds": (elapsed.as_secs_f64() * 10.0).round() / 10.0,
        "debugBuild": j.debug_build,
    })
}

fn kind_param(p: &Value) -> Result<ProfileKind, CommandError> {
    match opt_str(p, "kind") {
        Some(k) => {
            ProfileKind::from_name(k).ok_or_else(|| CommandError::BadParams(format!("unknown kind `{k}` (cpu|memory)")))
        }
        None => Ok(ProfileKind::Cpu),
    }
}

pub(crate) fn tool_param(p: &Value) -> Result<Option<ProfilerTool>, CommandError> {
    opt_str(p, "tool")
        .map(|t| ProfilerTool::from_name(t).ok_or_else(|| CommandError::BadParams(format!("unknown profiler `{t}`"))))
        .transpose()
}

pub(crate) fn seconds_param(p: &Value) -> Result<Option<u64>, CommandError> {
    match p.get("seconds") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let x = v
                .as_f64()
                .filter(|x| x.is_finite() && *x > 0.0)
                .ok_or_else(|| CommandError::BadParams("`seconds` must be a positive number".into()))?;
            Ok(Some(x.ceil().min(86_400.0) as u64))
        }
    }
}

/// The run configuration a request names (index or name), else the selected one, else
/// `cargo run` in a Cargo project.
pub(crate) fn config_for(s: &mut Session, p: &Value) -> Result<RunConfig, CommandError> {
    match p.get("config") {
        Some(Value::String(name)) => {
            let i = s
                .run_configs
                .iter()
                .position(|c| c.name == *name)
                .ok_or_else(|| CommandError::BadParams(format!("no run configuration named `{name}`")))?;
            s.selected_run_config = i;
        }
        Some(Value::Number(_)) => {
            let i = opt_usize(p, "config")?.unwrap_or(0);
            if i >= s.run_configs.len() {
                return Err(CommandError::BadParams(format!("no run configuration {i}")));
            }
            s.selected_run_config = i;
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(CommandError::BadParams("`config` is an index or a name".into())),
    }
    if let Some(name) = s.run_configs.get(s.selected_run_config).map(|c| c.name.clone()) {
        crate::run_config_store::prepare_run(s, &name).map_err(CommandError::Failed)?;
    }
    match s.run_configs.get(s.selected_run_config) {
        Some(c) => Ok(c.clone()),
        None if s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some() || p.root.join("Cargo.toml").is_file()) => {
            let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
            Ok(RunConfig::cargo("cargo run", CargoInvocation::new(CargoCommand::Run, root)))
        }
        None => Err(CommandError::Disabled("no run configuration to profile".into())),
    }
}

/// `interval`: seconds between tracemalloc memory samples (0 turns them off).
pub(crate) fn interval_param(p: &Value) -> Result<Option<f64>, CommandError> {
    match p.get("interval") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x >= 0.0)
            .map(|x| Some(x.min(3600.0)))
            .ok_or_else(|| CommandError::BadParams("`interval` must be a number of seconds (0 = off)".into())),
    }
}

/// What to record with (`kind`, `tool`, `seconds`, `interval`), read before anything starts.
pub(crate) struct RecordParams {
    pub kind: ProfileKind,
    pub tool: Option<ProfilerTool>,
    pub seconds: Option<u64>,
    pub interval: Option<f64>,
}

impl RecordParams {
    pub(crate) fn read(p: &Value) -> Result<Self, CommandError> {
        Ok(Self { kind: kind_param(p)?, tool: tool_param(p)?, seconds: seconds_param(p)?, interval: interval_param(p)? })
    }
}

/// Profile a run configuration → {tab, job, config, tool, state}.
pub(crate) fn record_config(s: &mut Session, config: &RunConfig, params: &RecordParams) -> CmdResult {
    let python = profiler::project_python(s);
    let plan = crate::profiler_plan::plan_config(config, python.as_deref()).map_err(CommandError::Failed)?;
    let job = profiler::record(
        s,
        RecordRequest {
            plan,
            kind: params.kind,
            tool: params.tool,
            seconds: params.seconds,
            interval: params.interval,
        },
    )?;
    let j = s.profiler.job(job);
    Ok(json!({
        "tab": j.and_then(|j| j.tab).map(|t| t.0),
        "job": job,
        "config": config.name,
        "tool": j.and_then(|j| j.tool),
        "state": j.map(|j| j.state),
    }))
}

/// What the gutter ▶ at a line (0-based) of a file runs: the Rust test (`cargo test --lib
/// name -- --exact`), test module or `main` there; a pytest test (`file::test`); a C++ test or
/// suite of GoogleTest, Catch2, doctest or Boost.Test (the CMake target with the framework's
/// filter); a single-file program.
pub(crate) fn context_config_at(s: &Session, path: &Path, line: usize) -> Result<RunConfig, CommandError> {
    if !path.is_file() {
        return Err(CommandError::BadParams(format!("no such file: {}", path.display())));
    }
    if let Some((title, inv)) = crate::run_cmds::context_invocation(s, path, line) {
        return Ok(RunConfig::cargo(title, inv));
    }
    let text = match s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        Some(d) => d.doc.text(),
        None => codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(e.to_string()))?,
    };
    let root = s.project_root().unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());
    crate::run_configs::context_config(&root, path, &text, line).ok_or_else(|| {
        CommandError::Failed(format!(
            "nothing to profile at {}:{} (no test, test module or main function there)",
            path.display(),
            line.saturating_add(1)
        ))
    })
}

fn record(s: &mut Session, p: &Value) -> CmdResult {
    let kind = kind_param(p)?;
    let tool = tool_param(p)?;
    let seconds = seconds_param(p)?;
    let interval = interval_param(p)?;
    // The test (or module, or main) at a source line, like the gutter's ▶ (1-based line).
    if let Some(t) = p.get("test").filter(|t| !t.is_null()) {
        let path = t
            .get("path")
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
            .ok_or_else(|| CommandError::BadParams("`test` is {path, line} (line 1-based)".into()))?;
        let line = t
            .get("line")
            .and_then(Value::as_u64)
            .filter(|l| *l >= 1)
            .ok_or_else(|| CommandError::BadParams("`test.line` is a 1-based line number".into()))?;
        let path = profiler::resolve_path(s, Path::new(path));
        let line = usize::try_from(line - 1).map_err(|_| CommandError::BadParams("`test.line` is out of range".into()))?;
        let config = context_config_at(s, &path, line)?;
        let params = RecordParams { kind, tool, seconds, interval };
        let started = record_config(s, &config, &params)?;
        let job = started.get("job").and_then(Value::as_u64).unwrap_or(0);
        let mut v = s.profiler.job(job).map(job_json).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("config".into(), json!(config.name));
            o.insert("command".into(), json!(config.command_line()));
        }
        return Ok(v);
    }
    if let Some(pid) = opt_usize(p, "pid")? {
        let pid = u32::try_from(pid).map_err(|_| CommandError::BadParams("`pid` is out of range".into()))?;
        let python = p.get("python").and_then(Value::as_bool).unwrap_or(false);
        let job = profiler::attach_job(s, pid, opt_str(p, "name").unwrap_or(""), python, tool, seconds)?;
        return Ok(s.profiler.job(job).map(job_json).unwrap_or(Value::Null));
    }
    let python = profiler::project_python(s);
    let plan = match p.get("command") {
        Some(c) => {
            let words: Vec<String> = match c {
                Value::String(text) => crate::profiler_plan::shell_split(text).ok_or_else(|| {
                    CommandError::BadParams("cannot split `command` into words (quotes, pipes?)".into())
                })?,
                Value::Array(items) => items
                    .iter()
                    .map(|w| w.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| CommandError::BadParams("`command` items must be strings".into()))?,
                _ => return Err(CommandError::BadParams("`command` is a string or an array of strings".into())),
            };
            let cwd = match opt_str(p, "cwd") {
                Some(d) => profiler::resolve_path(s, Path::new(d)),
                None => s.project_root().unwrap_or_else(|| PathBuf::from(".")),
            };
            if !cwd.is_dir() {
                return Err(CommandError::BadParams(format!("no such directory: {}", cwd.display())));
            }
            crate::profiler_plan::plan_command(&words, &cwd, Vec::new(), python.as_deref())
                .map_err(CommandError::BadParams)?
        }
        None => {
            let config = config_for(s, p)?;
            crate::profiler_plan::plan_config(&config, python.as_deref()).map_err(CommandError::Failed)?
        }
    };
    let job = profiler::record(s, RecordRequest { plan, kind, tool, seconds, interval })?;
    Ok(s.profiler.job(job).map(job_json).unwrap_or(Value::Null))
}

/// A snapshot reference resolved.
enum Resolved {
    /// Loaded (and whether a Cargo debug build was profiled).
    Ready(Cached, bool),
    /// Still recording or loading: the answer to give.
    Pending(Value),
}

fn pending(job: u64, state: JobState) -> Value {
    json!({
        "pending": true, "job": job, "state": state,
        "hint": "poll profiler.job {job} until its state is done, then ask again",
    })
}

/// `snapshot`: a job id, a path, or nothing (the open snapshot, else the newest finished job).
fn resolve(s: &mut Session, v: Option<&Value>) -> Result<Resolved, CommandError> {
    let path = match v {
        None | Some(Value::Null) => {
            if let Some(snap) = &s.profiler.snapshot {
                snap.path.clone()
            } else {
                let job = s.profiler.jobs.iter().rev().find(|j| j.snapshot.is_some() || j.state != JobState::Failed);
                match job {
                    Some(j) if j.state != JobState::Done => return Ok(Resolved::Pending(pending(j.id, j.state))),
                    Some(j) => j.snapshot.clone().unwrap_or_default(),
                    None => {
                        return Err(CommandError::Failed(
                            "no profiler snapshot: record one with profiler.record or pass `snapshot`".into(),
                        ));
                    }
                }
            }
        }
        Some(Value::Number(n)) => {
            let id = n.as_u64().ok_or_else(|| CommandError::BadParams("`snapshot` job ids are positive".into()))?;
            let j = s.profiler.job(id).ok_or_else(|| CommandError::BadParams(format!("no profiling job {id}")))?;
            match (j.state, &j.snapshot) {
                (JobState::Failed, _) => {
                    return Err(CommandError::Failed(j.error.clone().unwrap_or_else(|| "the job failed".into())));
                }
                (JobState::Done, Some(p)) => p.clone(),
                (state, _) => return Ok(Resolved::Pending(pending(id, state))),
            }
        }
        Some(Value::String(text)) => profiler::resolve_path(s, Path::new(text)),
        Some(_) => return Err(CommandError::BadParams("`snapshot` is a job id or a path".into())),
    };
    let debug = s.profiler.jobs.iter().any(|j| j.snapshot.as_deref() == Some(path.as_path()) && j.debug_build);
    if let Some(c) = s.profiler.cached(&path) {
        return Ok(Resolved::Ready(c, debug));
    }
    let job = profiler::load_for_report(s, &path)?;
    Ok(Resolved::Pending(pending(job, JobState::Loading)))
}

/// The analysis of a request's selection (`thread`, `range`): the cached one when neither is
/// given.
fn analysis_for(c: &Cached, p: &Value) -> Result<std::sync::Arc<codecraft_profile::Analysis>, CommandError> {
    let sel = selection_param(p, &c.profile)?;
    if sel == Selection::default() {
        return Ok(std::sync::Arc::clone(&c.analysis));
    }
    Ok(std::sync::Arc::new(codecraft_profile::Analysis::select(&c.profile, &sel)))
}

fn report(s: &mut Session, p: &Value) -> CmdResult {
    let top = opt_usize(p, "top")?.unwrap_or(15);
    let lines = opt_usize(p, "lines")?.unwrap_or(10);
    let focus = opt_str(p, "focus").map(str::to_string);
    let sort = sort_param(p)?;
    let root = s.project_root();
    match resolve(s, p.get("snapshot"))? {
        Resolved::Pending(v) => Ok(v),
        Resolved::Ready(c, debug_build) => {
            let analysis = analysis_for(&c, p)?;
            let opts = ReportOptions { top, lines, focus, root, debug_build, sort };
            Ok(profiler_report::report(&c.profile, &analysis, &c.path, &opts))
        }
    }
}

fn compare(s: &mut Session, p: &Value) -> CmdResult {
    let top = opt_usize(p, "top")?.unwrap_or(10);
    if p.get("a").is_none_or(Value::is_null) {
        return Err(CommandError::BadParams("`a` (the baseline: a job id or a path) is required".into()));
    }
    let a = resolve(s, p.get("a"))?;
    let b = resolve(s, p.get("b"))?;
    match (a, b) {
        (Resolved::Pending(v), _) | (_, Resolved::Pending(v)) => Ok(v),
        (Resolved::Ready(a, _), Resolved::Ready(b, _)) => {
            Ok(profiler_report::compare((&a.path, &a.profile, &a.analysis), (&b.path, &b.profile, &b.analysis), top))
        }
    }
}

fn line_hits(s: &mut Session, p: &Value) -> CmdResult {
    let file = profiler::resolve_path(s, Path::new(req_str(p, "path")?));
    match resolve(s, p.get("snapshot"))? {
        Resolved::Pending(v) => Ok(v),
        Resolved::Ready(c, _) => {
            let analysis = analysis_for(&c, p)?;
            let index = profiler::LineIndex::for_analysis(&c.profile, &analysis);
            let hits = index.for_path(&file).unwrap_or(&[]);
            Ok(profiler_report::line_hits(&c.profile, analysis.tree.total, &file, hits))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::runs::ProcessCommand;

    fn session(dir: &Path) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        s.profiler.dir_override = Some(dir.join("home").join("profiler"));
        s.profiler.tool_dirs = Some(vec![bin]);
        s.profiler.perf_allowed = Some(false);
        s.profiler.xctrace_allowed = Some(true);
        s
    }

    fn pump_until(s: &mut Session, secs: u64, what: impl Fn(&Session) -> bool) {
        let start = Instant::now();
        while !what(s) {
            assert!(
                start.elapsed() < Duration::from_secs(secs),
                "timed out: {:?} {:?}",
                s.profiler.error,
                s.profiler.jobs
            );
            s.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn job_done(s: &Session, job: u64) -> bool {
        s.profiler.job(job).is_some_and(|j| matches!(j.state, JobState::Done | JobState::Failed))
    }

    /// A fake xctrace: `record … --output X … --launch -- prog` runs the program and creates the
    /// trace directory; `export --input X … --output Y` copies the fixture to Y.
    fn fake_xctrace(bin: &Path, fixture: &Path) {
        let script = format!(
            "#!/bin/sh\necho \"xctrace $*\" >> '{log}'\n\
             if [ \"$1\" = export ]; then out=''; prev=''; for a in \"$@\"; do if [ \"$prev\" = --output ]; then out=\"$a\"; fi; prev=\"$a\"; done; cp '{fixture}' \"$out\"; exit 0; fi\n\
             out=''; prev=''; run=''; for a in \"$@\"; do if [ \"$prev\" = --output ]; then out=\"$a\"; fi; prev=\"$a\"; done\n\
             mkdir -p \"$out\"; echo data > \"$out/x\"\n",
            log = bin.join("calls").display(),
            fixture = fixture.display(),
        );
        let path = bin.join("xctrace");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fixture(dir: &Path) -> PathBuf {
        let p = dir.join("fixture.xml");
        std::fs::write(&p, include_str!("../../profile/tests/fixtures/xctrace-time-profile.xml")).unwrap();
        p
    }

    #[test]
    fn record_a_command_with_xctrace_then_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bin = dir.path().join("bin");
        fake_xctrace(&bin, &fixture(dir.path()));
        let r = s
            .execute("profiler.record", &json!({ "command": "/bin/echo hi", "cwd": dir.path(), "seconds": 2 }))
            .unwrap();
        assert_eq!(r["tool"], "xctrace");
        let job = r["job"].as_u64().unwrap();
        // Not ready yet: the report says so.
        let early = s.execute("profiler.report", &json!({ "snapshot": job })).unwrap();
        if early.get("pending").is_some() {
            assert_eq!(early["job"], job);
        }
        pump_until(&mut s, 30, |s| job_done(s, job) && s.profiler.loading.is_none());
        let j = s.execute("profiler.job", &json!({ "job": job })).unwrap();
        assert_eq!(j["state"], "done", "{j}");
        let log = std::fs::read_to_string(bin.join("calls")).unwrap();
        assert!(log.contains("--time-limit 2s --launch -- /bin/echo hi"), "{log}");
        assert!(log.contains("xctrace export --input "), "{log}");
        let rep = s.execute("profiler.report", &json!({ "snapshot": job, "top": 3 })).unwrap();
        assert_eq!(rep["format"], "xctrace");
        assert_eq!(rep["top"][0]["name"], "inner", "{rep}");
        assert!(rep["top"][0]["location"].as_str().unwrap().ends_with("hot.c:5"));
        assert_eq!(rep["hotLines"][0]["function"], "inner");
        // Default snapshot = the open one; compare against itself: nothing changed.
        let rep2 = s.execute("profiler.report", &json!({ "focus": "outer" })).unwrap();
        assert!(rep2["focus"]["percent"].as_f64().unwrap() > 0.0);
        let c = s.execute("profiler.compare", &json!({ "a": job })).unwrap();
        assert!(c["slower"].as_array().unwrap().is_empty());
        assert!(s.execute("profiler.compare", &json!({})).is_err());
        let t = s.execute("profiler.timeline", &json!({ "buckets": 10 })).unwrap();
        assert_eq!(t["threads"][0]["activity"].as_array().unwrap().len(), 10);
        let jobs = s.execute("profiler.jobs", &Value::Null).unwrap();
        assert_eq!(jobs.as_array().unwrap().len(), 1);
        s.shutdown();
    }

    #[test]
    fn report_loads_a_path_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "main;parse 50\nmain;render 50\n").unwrap();
        std::fs::write(&b, "main;parse 10\nmain;render 50\n").unwrap();
        let r = s.execute("profiler.compare", &json!({ "a": a, "b": b })).unwrap();
        assert_eq!(r["pending"], true);
        let job = r["job"].as_u64().unwrap();
        pump_until(&mut s, 20, |s| job_done(s, job));
        // b is loaded next.
        let r = s.execute("profiler.compare", &json!({ "a": a, "b": b })).unwrap();
        if r.get("pending").is_some() {
            let job = r["job"].as_u64().unwrap();
            pump_until(&mut s, 20, |s| job_done(s, job));
        }
        let r = s.execute("profiler.compare", &json!({ "a": a, "b": b })).unwrap();
        assert_eq!(r["faster"][0]["name"], "parse", "{r}");
        let l = s.execute("profiler.load", &json!({ "path": a })).unwrap();
        assert_eq!(l["loaded"], true);
        assert!(s.execute("profiler.load", &json!({ "path": dir.path().join("none") })).is_err());
        assert!(s.execute("profiler.report", &json!({ "snapshot": 999 })).is_err());
        assert!(s.execute("profiler.report", &json!({ "snapshot": [1] })).is_err());
        let hits = s.execute("profiler.lineHits", &json!({ "path": "x.py", "snapshot": a })).unwrap();
        assert!(hits["lines"].as_array().unwrap().is_empty());
        assert!(s.execute("profiler.job", &json!({ "job": 12345 })).is_err());
    }

    #[test]
    fn bad_record_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        assert!(s.execute("profiler.record", &Value::Null).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": "a | b" })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": 5 })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": [1] })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": "cargo fmt", "cwd": dir.path() })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": "x", "cwd": "/no/such/dir" })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": "x", "kind": "gpu" })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": "x", "tool": "rm" })).is_err());
        assert!(s.execute("profiler.record", &json!({ "command": "x", "seconds": -1 })).is_err());
        assert!(s.execute("profiler.record", &json!({ "pid": u64::MAX })).is_err());
        assert!(s.execute("profiler.record", &json!({ "pid": 1 })).is_err());
        assert!(s.execute("profiler.record", &json!({ "config": "nope" })).is_err());
        assert!(s.execute("profiler.record", &json!({ "config": 3 })).is_err());
        // cProfile on a native program.
        let e = s
            .execute("profiler.record", &json!({ "command": "/bin/echo", "cwd": dir.path(), "tool": "cProfile" }))
            .unwrap_err();
        assert!(e.to_string().contains("Python"), "{e}");
        // No native profiler installed: the error says how to get one.
        let e = s.execute("profiler.record", &json!({ "command": "/bin/echo", "cwd": dir.path() })).unwrap_err();
        assert!(e.to_string().contains("xctrace"), "{e}");
        assert_eq!(s.profiler.missing, Some(ProfilerTool::Xctrace));
        let e = s
            .execute("profiler.record", &json!({ "command": "/bin/echo", "cwd": dir.path(), "kind": "memory" }))
            .unwrap_err();
        assert!(e.to_string().contains("not installed"), "{e}");
    }

    /// A shell chain (CMake Application style): the build part runs first, then only the program
    /// is recorded.
    #[test]
    fn shell_chains_build_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bin = dir.path().join("bin");
        fake_xctrace(&bin, &fixture(dir.path()));
        let marker = dir.path().join("built");
        let script = format!("touch {} && /bin/echo run", marker.display());
        s.run_configs = vec![RunConfig::process("chain", ProcessCommand::shell(script, dir.path()))];
        let r = s.execute("run.profile", &Value::Null).unwrap();
        let job = r["job"].as_u64().unwrap();
        pump_until(&mut s, 30, |s| job_done(s, job));
        assert!(marker.is_file());
        let log = std::fs::read_to_string(bin.join("calls")).unwrap();
        assert!(log.contains("--launch -- /bin/echo run"), "{log}");
        assert!(!log.contains("touch"), "{log}");
        // A failing build fails the job and records nothing.
        s.run_configs = vec![RunConfig::process("bad", ProcessCommand::shell("false && /bin/echo x", dir.path()))];
        let r = s.execute("run.profile", &Value::Null).unwrap();
        let job = r["job"].as_u64().unwrap();
        pump_until(&mut s, 30, |s| job_done(s, job));
        assert_eq!(s.profiler.job(job).unwrap().state, JobState::Failed);
        s.shutdown();
    }
}
