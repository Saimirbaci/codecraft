//! Commands the agent tools ([`crate::agent_tools`]) are built on that did not exist yet: compact,
//! agent-readable views of the IDE's state (a debugger stop summary, variables, run output, test
//! failures, problems for a scope, profiler hotspots), file reads and edits with the IDE's unsaved
//! text, language-server queries whose raw answers come back as data (not popups), and a search
//! of the command registry. Every one is an ordinary command, so the UI, the CLI and
//! `codecraft_execute` can use them too.
//!
//! Conventions: absolute paths, 1-based lines and columns in params and results.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use codecraft_lsp::lsp_types as lt;
use codecraft_text::{Document, Position};
use codecraft_workspace::{ConsoleKind, RunStatus, RunTabId, Severity};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, spec};
use crate::debug::{DebugStatus, VariableInfo};
use crate::params::{bool_or, opt_str, opt_usize, req_str};
use crate::{CommandError, Session};

/// Longest value string in debugger results.
const MAX_VALUE_CHARS: usize = 300;
/// Most lines `files.read` returns at once.
pub(crate) const MAX_READ_LINES: usize = 400;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "commands.search",
            "Search Commands",
            None,
            "{query?: string, enabledOnly?: bool, limit?: int, offset?: int} → {total, commands: [{id, label, params, \
             enabled, disabledReason?, menu?, capability}]} — the command registry as agents see it",
            Context::Global,
            always,
            search_commands,
        ),
        spec(
            "project.info",
            "Project Info",
            None,
            "→ {root, name, runConfigurations, languageServers, debugger, profiler, testRuns, dataSources}",
            Context::Global,
            always,
            project_info,
        ),
        spec(
            "files.read",
            "Read File Lines",
            None,
            "{path: string, startLine?: int, endLine?: int} — 1-based, at most 400 lines, the editor's unsaved text \
             when the file is open → {path, startLine, endLine, totalLines, unsaved, content}",
            Context::Global,
            always,
            read_file,
        ),
        spec(
            "files.write",
            "Write File",
            None,
            "{path: string, content: string} — replace (or create) a file; an open editor gets one undo step and is \
             saved → {path, created}",
            Context::Global,
            always,
            write_file,
        ),
        spec(
            "files.edit",
            "Replace Text in File",
            None,
            "{path: string, oldText: string, newText: string, replaceAll?: bool} — `oldText` must occur once unless \
             `replaceAll` → {path, replacements}",
            Context::Global,
            always,
            edit_file,
        ),
        spec(
            "analysis.problems",
            "Problems for Scope",
            None,
            "{path?: file or directory, severity?: error|warning|info|hint (minimum, default warning), limit?: int} → \
             {total, problems: [{file, line, column, endLine, severity, message, source, code?}]}",
            Context::Global,
            always,
            problems,
        ),
        spec(
            "debug.agentSummary",
            "Debugger Stop Summary (Agents)",
            None,
            "{frames?: int (default 5), context?: int (excerpt lines around each frame's line, default 1)} → {state: \
             none|building|starting|running|stopped|exited|failed, reason?, thread?, frames?, locals?, …}",
            Context::Global,
            always,
            |s, p| {
                let frames = opt_usize(p, "frames")?.unwrap_or(5).clamp(1, 50);
                let context = opt_usize(p, "context")?.unwrap_or(1).min(10);
                Ok(stop_summary(s, frames, context))
            },
        ),
        spec(
            "debug.variablesOf",
            "Debugger Variables",
            None,
            "{reference?: int, limit?: int} — the loaded children of a variables reference, or (without one) the \
             selected frame's scopes with their variables → {variables|scopes, pending}",
            Context::Global,
            always,
            variables_of,
        ),
        spec(
            "run.configurations",
            "List Effective Run Configurations",
            None,
            "→ [{index, name, kind: cargo|process, commandLine, selected}] (the `config` index of run.run, run.debug, \
             run.profile)",
            Context::Global,
            always,
            |s, _| Ok(run_configurations(s)),
        ),
        spec(
            "run.output",
            "Run Output",
            None,
            "{tab?: int, tail?: int (default 100), offset?: int} — a Run tab's status and console lines (the newest \
             tab by default) → {tab, title, status, exitCode?, totalLines, firstLine, lines}",
            Context::Global,
            always,
            run_output,
        ),
        spec(
            "run.stopTab",
            "Stop Run Tab",
            None,
            "{tab?: int} — stop that Run tab's process (default: the newest running one) → {tab, stopped}",
            Context::Global,
            always,
            stop_tab,
        ),
        spec(
            "test.summary",
            "Test Results Summary",
            None,
            "{tab?: int, includePassed?: bool} → {tab, title, finished, status, counts, durationMs, failures: [{name, \
             suite, message, file, line, output}], tests?}",
            Context::Global,
            always,
            test_summary,
        ),
        spec(
            "test.runFiltered",
            "Run Tests Matching",
            None,
            "{name: string} — `cargo test <name>` in a Cargo project, `pytest -k <name>` in a Python one (an empty name \
             runs everything) → {tab, title}",
            Context::Global,
            has_project,
            run_filtered,
        ),
        spec(
            "profiler.hotspots",
            "Profiler Hotspots",
            None,
            "{limit?: int (default 20), sort?: self|total, thread?: int|name, range?: {startMs, endMs}} → \
             {snapshot, samples, sort, order, hotspots: [{name, selfPercent, totalPercent, self, total, file?, line?, \
             module?}]} — in a fixed order whatever the IDE shows (all threads and the whole run unless `thread` / \
             `range` are given): by self (default) or total weight, then the other, then name, location, module",
            Context::Global,
            always,
            hotspots,
        ),
        spec(
            "lsp.query",
            "Language Server Query",
            None,
            "{kind: definition|references|hover|documentSymbols|workspaceSymbols|prepareCallHierarchy|incomingCalls|\
             outgoingCalls, path?, line?, column?, symbol?, query?, item?} — send a request whose answer is kept as data \
             (no popup) → {token}; read it with lsp.queryResult",
            Context::Global,
            always,
            lsp_query,
        ),
        spec(
            "lsp.queryResult",
            "Language Server Query Result",
            None,
            "{token: int, limit?: int} → {done, result?, error?} (locations, symbols, hover text or call hierarchy, \
             compact)",
            Context::Global,
            always,
            lsp_query_result,
        ),
    ]
}

// ---------------------------------------------------------------- helpers

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::BadParams(msg.into())
}

fn failed(msg: impl Into<String>) -> CommandError {
    CommandError::Failed(msg.into())
}

/// A path param: absolute or relative to the project root (`file://` URIs accepted).
pub(crate) fn path_param(s: &Session, p: &Value, key: &str) -> Result<PathBuf, CommandError> {
    let raw = req_str(p, key)?;
    crate::ide_server::resolve_path(s, raw).ok_or_else(|| bad(format!("`{key}` is not a usable path: {raw}")))
}

/// A 1-based line param as a 0-based line.
pub(crate) fn line_param(p: &Value, key: &str) -> Result<Option<usize>, CommandError> {
    match opt_usize(p, key)? {
        Some(0) => Err(bad(format!("`{key}` is 1-based"))),
        Some(n) => Ok(Some(n - 1)),
        None => Ok(None),
    }
}

/// The text the IDE sees for a file: the open document (with unsaved changes), else the disk.
pub(crate) fn file_text(s: &Session, path: &Path) -> Result<(String, bool), CommandError> {
    if let Some(od) = s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        return Ok((od.doc.text(), od.doc.is_modified()));
    }
    codecraft_fs::read_text_file(path).map(|t| (t, false)).map_err(|e| failed(format!("{}: {e}", path.display())))
}

/// Cut a string to `max` chars with an ellipsis.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Lines `line0 - before ..= line0 + after` of a file, numbered (1-based), the line itself marked
/// with `>`: `"  41 | let a = 1;\n> 42 | f(a);"`. Empty when the file cannot be read.
pub(crate) fn excerpt(s: &Session, path: &Path, line0: usize, before: usize, after: usize) -> String {
    let lines: Vec<String> = match s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        Some(od) => {
            let first = line0.saturating_sub(before);
            let last = line0.saturating_add(after).min(od.doc.len_lines().saturating_sub(1));
            (first..=last).map(|l| od.doc.line_text(l).unwrap_or_default()).collect::<Vec<_>>()
        }
        None => {
            let Ok(text) = codecraft_fs::read_text_file(path) else { return String::new() };
            let first = line0.saturating_sub(before);
            text.lines()
                .skip(first)
                .take(line0.saturating_add(after).saturating_sub(first).saturating_add(1))
                .map(str::to_string)
                .collect()
        }
    };
    let first = line0.saturating_sub(before);
    let width = (first + lines.len()).to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let n = first + i;
            let mark = if n == line0 { '>' } else { ' ' };
            format!("{mark} {:>width$} | {}", n + 1, clip(text.trim_end_matches(['\r', '\n']), 200))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Frame names longer than this lose their parameter list in agent results.
const MAX_FRAME_NAME: usize = 120;

/// A frame's function name for agents: libc++'s inline namespace dropped (`std::__1::` →
/// `std::`), and a C++ signature longer than [`MAX_FRAME_NAME`] shortened to `name(…)` (QA M14:
/// a crash in `lookup(const std::map<std::string, int>&, …)` spelled out 500 characters of
/// template arguments).
pub(crate) fn short_frame_name(name: &str) -> String {
    let name = name.replace("std::__1::", "std::");
    if name.chars().count() <= MAX_FRAME_NAME {
        return name;
    }
    // The parameter list: the first `(` outside template brackets.
    let mut depth = 0i32;
    for (i, c) in name.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            '(' if depth <= 0 && i > 0 => {
                let head = name.get(..i).unwrap_or(&name);
                if head.chars().count() <= MAX_FRAME_NAME {
                    return format!("{head}(…)");
                }
                break;
            }
            _ => {}
        }
    }
    clip(&name, MAX_FRAME_NAME)
}

fn severity_rank(sev: Severity) -> u8 {
    match sev {
        Severity::Error => 3,
        Severity::Warning => 2,
        Severity::Info => 1,
        Severity::Hint => 0,
    }
}

fn severity_id(sev: Severity) -> &'static str {
    match sev {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

// ---------------------------------------------------------------- registry and project

fn search_commands(s: &mut Session, p: &Value) -> CmdResult {
    let reg = crate::registry();
    let query = opt_str(p, "query").unwrap_or_default().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let enabled_only = bool_or(p, "enabledOnly", false);
    let limit = opt_usize(p, "limit")?.unwrap_or(50).clamp(1, 500);
    let offset = opt_usize(p, "offset")?.unwrap_or(0);
    let mut matches: Vec<Value> = Vec::new();
    for c in reg.all() {
        let menu = reg.menu_path(c.id).map(|m| m.join(" › "));
        let hay = format!("{} {} {} {}", c.id, c.label, menu.as_deref().unwrap_or(""), c.params).to_lowercase();
        if !words.iter().all(|w| hay.contains(w)) {
            continue;
        }
        let enabled = (c.enabled)(s);
        if enabled_only && enabled.is_err() {
            continue;
        }
        let mut v = json!({
            "id": c.id,
            "label": c.label,
            "params": c.params,
            "enabled": enabled.is_ok(),
            "capability": crate::agent_tools::command_capability(c.id).map(|c| c.id()).unwrap_or("blocked"),
        });
        if let Some(o) = v.as_object_mut() {
            if let Err(why) = enabled {
                o.insert("disabledReason".into(), json!(why));
            }
            if let Some(m) = menu {
                o.insert("menu".into(), json!(m));
            }
        }
        matches.push(v);
    }
    let total = matches.len();
    let page: Vec<Value> = matches.into_iter().skip(offset).take(limit).collect();
    let mut out = json!({ "total": total, "offset": offset, "commands": page });
    if total > offset.saturating_add(limit)
        && let Some(o) = out.as_object_mut()
    {
        o.insert("truncated".into(), json!(format!("{} more; pass offset {}", total - offset - limit, offset + limit)));
    }
    Ok(out)
}

fn project_info(s: &mut Session, _: &Value) -> CmdResult {
    let languages = s.execute("lsp.status", &Value::Null).unwrap_or(Value::Null);
    let project = s.ws.project.as_ref().map(|p| json!({ "root": p.root, "name": p.name }));
    Ok(json!({
        "project": project,
        "runConfigurations": run_configurations(s),
        "languageServers": languages,
        "debugger": stop_summary(s, 1, 0)["state"],
        "breakpoints": s.debug.breakpoints.len(),
        "profiler": {
            "recording": !s.profiler.recordings.is_empty(),
            "snapshot": s.profiler.snapshot.is_some(),
        },
        "testRuns": s.testing.runs.len(),
        "runTabs": s.ws.runs.iter().map(|r| json!({ "tab": r.id.0, "title": r.title, "status": run_status(r.status) })).collect::<Vec<_>>(),
        "dataSources": s.db.sources.len(),
        "openEditors": s.ws.all_tabs().iter().filter_map(|e| s.ws.editor(*e)).filter_map(|e| s.ws.doc(e.doc)).filter_map(|d| d.path.clone()).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------- files

fn read_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = path_param(s, p, "path")?;
    let (text, unsaved) = file_text(s, &path)?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = line_param(p, "startLine")?.unwrap_or(0);
    let end_req = line_param(p, "endLine")?.unwrap_or(usize::MAX);
    if start >= total.max(1) {
        return Err(bad(format!("startLine {} is past the end ({} lines)", start + 1, total)));
    }
    let end = end_req.min(total.saturating_sub(1)).min(start + MAX_READ_LINES - 1);
    let width = (end + 1).to_string().len();
    let content = lines
        .get(start..=end.max(start).min(total.saturating_sub(1)))
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>width$}\t{l}", start + i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = json!({
        "path": path,
        "startLine": start + 1,
        "endLine": (end + 1).min(total.max(1)),
        "totalLines": total,
        "unsaved": unsaved,
        "content": content,
    });
    if end < end_req.min(total.saturating_sub(1))
        && let Some(o) = out.as_object_mut()
    {
        o.insert("truncated".into(), json!(format!("more lines follow; pass startLine {}", end + 2)));
    }
    Ok(out)
}

fn write_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = path_param(s, p, "path")?;
    let content = req_str(p, "content")?.to_string();
    if path.is_dir() {
        return Err(bad(format!("{} is a directory", path.display())));
    }
    let created = !path.exists();
    crate::ide_server::apply_edit(s, &path, &content).map_err(failed)?;
    s.status = format!("An agent wrote {}", path.display());
    Ok(json!({ "path": path, "created": created }))
}

fn edit_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = path_param(s, p, "path")?;
    let old = req_str(p, "oldText")?;
    let new = req_str(p, "newText")?;
    if old.is_empty() {
        return Err(bad("`oldText` is empty"));
    }
    let (text, _) = file_text(s, &path)?;
    let n = text.matches(old).count();
    if n == 0 {
        return Err(failed(format!("`oldText` does not occur in {}", path.display())));
    }
    let all = bool_or(p, "replaceAll", false);
    if n > 1 && !all {
        return Err(failed(format!(
            "`oldText` occurs {n} times in {}; add context or pass replaceAll",
            path.display()
        )));
    }
    let updated = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    crate::ide_server::apply_edit(s, &path, &updated).map_err(failed)?;
    s.status = format!("An agent edited {}", path.display());
    Ok(json!({ "path": path, "replacements": if all { n } else { 1 } }))
}

// ---------------------------------------------------------------- problems

fn problems(s: &mut Session, p: &Value) -> CmdResult {
    let scope = match opt_str(p, "path") {
        Some(_) => Some(path_param(s, p, "path")?),
        None => None,
    };
    let min = match opt_str(p, "severity").unwrap_or("warning") {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        "hint" => Severity::Hint,
        other => return Err(bad(format!("unknown severity `{other}`"))),
    };
    let limit = opt_usize(p, "limit")?.unwrap_or(200).clamp(1, 2000);
    let mut out: Vec<(u8, Value)> = Vec::new();
    for (path, list) in s.ws.all_problems() {
        if scope.as_ref().is_some_and(|sc| !path.starts_with(sc)) {
            continue;
        }
        for pr in list {
            if severity_rank(pr.severity) < severity_rank(min) {
                continue;
            }
            let source = match pr.source {
                codecraft_workspace::ProblemSource::RustAnalyzer => "language-server",
                codecraft_workspace::ProblemSource::Cargo => "cargo",
                codecraft_workspace::ProblemSource::Inspection => "inspection",
                // Analyzer findings come below, with their ids (M15 EA).
                codecraft_workspace::ProblemSource::Analysis => continue,
            };
            let mut v = json!({
                "file": path,
                "line": pr.start.line + 1,
                "column": pr.start.column + 1,
                "endLine": pr.end.line + 1,
                "severity": severity_id(pr.severity),
                "message": clip(&pr.message, 2000),
                "source": source,
            });
            if let (Some(code), Some(o)) = (&pr.code, v.as_object_mut()) {
                o.insert("code".into(), json!(code));
            }
            out.push((severity_rank(pr.severity), v));
        }
    }
    // The open findings of the last Inspect Code run (QA M14: `problems` right after `inspect`
    // said "no problems are known"), with the id `analysis.fix` / `analysis.suppress` take.
    if let Some(r) = &s.inspections.results {
        let mut findings: Vec<&crate::inspections::Finding> = r
            .open()
            .filter(|f| scope.as_ref().is_none_or(|sc| f.path.starts_with(sc)))
            // Re-analyzed on save since (the newer findings follow; M15 EA).
            .filter(|f| !crate::analysis::editor::superseded(s, f))
            .filter(|f| severity_rank(f.severity) >= severity_rank(min))
            .collect();
        findings.sort_by(|a, b| (&a.path, a.start.line, a.start.column).cmp(&(&b.path, b.start.line, b.start.column)));
        for f in findings {
            let mut v = json!({
                "file": f.path,
                "line": f.start.line + 1,
                "column": f.start.column + 1,
                "endLine": f.end.line + 1,
                "severity": severity_id(f.severity),
                "message": clip(&f.message, 2000),
                "source": "analysis",
                "analyzer": f.tool.id(),
                "id": f.id,
            });
            if let (Some(code), Some(o)) = (&f.code, v.as_object_mut()) {
                o.insert("code".into(), json!(code));
            }
            if !f.fixes.is_empty()
                && let Some(o) = v.as_object_mut()
            {
                o.insert("fixable".into(), json!(true));
            }
            out.push((severity_rank(f.severity), v));
        }
    }
    // Findings of the analysis on save, with the key `analysis.editorFix` takes (M15 EA).
    for e in crate::analysis::editor::saved_findings(s, scope.as_deref()) {
        let f = &e.finding;
        if severity_rank(f.severity) < severity_rank(min) {
            continue;
        }
        let mut v = json!({
            "file": f.path,
            "line": f.start.line + 1,
            "column": f.start.column + 1,
            "endLine": f.end.line + 1,
            "severity": severity_id(f.severity),
            "message": clip(&f.message, 2000),
            "source": "analysis",
            "analyzer": f.tool.id(),
            "key": e.key,
        });
        if let Some(o) = v.as_object_mut() {
            if let Some(code) = &f.code {
                o.insert("code".into(), json!(code));
            }
            if !f.fixes.is_empty() {
                o.insert("fixable".into(), json!(true));
            }
        }
        out.push((severity_rank(f.severity), v));
    }
    // Errors first, then by file and line (the map is already ordered by file).
    out.sort_by_key(|a| std::cmp::Reverse(a.0));
    let total = out.len();
    let errors = out.iter().filter(|(r, _)| *r == 3).count();
    let list: Vec<Value> = out.into_iter().take(limit).map(|(_, v)| v).collect();
    let mut res = json!({ "total": total, "errors": errors, "problems": list });
    if let Some(o) = res.as_object_mut() {
        if total > limit {
            o.insert("truncated".into(), json!(format!("{} more; narrow `path` or raise `limit`", total - limit)));
        }
        if total == 0 {
            o.insert(
                "note".into(),
                json!("no problems are known for this scope now; run the `inspect` tool (or cargo.check) for a full analysis"),
            );
        }
    }
    Ok(res)
}

// ---------------------------------------------------------------- debugger

fn variable_json(v: &VariableInfo) -> Value {
    let mut o = Map::new();
    o.insert("name".into(), json!(v.name));
    o.insert("value".into(), json!(clip(&v.value, MAX_VALUE_CHARS)));
    if let Some(t) = &v.type_name {
        o.insert("type".into(), json!(t));
    }
    if v.reference > 0 {
        o.insert("ref".into(), json!(v.reference));
    }
    Value::Object(o)
}

fn console_tail(s: &Session, n: usize) -> Vec<String> {
    let skip = s.debug.console.len().saturating_sub(n);
    s.debug.console.iter().skip(skip).map(|l| clip(crate::debug::console_text(l), 500)).collect()
}

/// The debugger's state as an agent needs it after a stop: reason, thread, the top frames with
/// source excerpts, and the top frame's locals.
pub(crate) fn stop_summary(s: &Session, frames: usize, context: usize) -> Value {
    let Some(session) = &s.debug.session else {
        return json!({ "state": "none", "message": "no debug session; start one with debug_start" });
    };
    let mut out = Map::new();
    out.insert("session".into(), json!(session.name));
    if !session.adapter.is_empty() {
        out.insert("adapter".into(), json!(session.adapter));
    }
    let state = match session.status() {
        DebugStatus::Building => "building",
        DebugStatus::Starting => "starting",
        DebugStatus::Running => {
            if let Some(pid) = session.debuggee_pid {
                out.insert("pid".into(), json!(pid));
            }
            "running"
        }
        DebugStatus::Exited { code } => {
            out.insert("exitCode".into(), json!(code));
            out.insert("output".into(), json!(console_tail(s, 20)));
            "exited"
        }
        DebugStatus::Failed { message } => {
            out.insert("message".into(), json!(message));
            out.insert("output".into(), json!(console_tail(s, 20)));
            "failed"
        }
        DebugStatus::Stopped { reason, description, thread } => {
            out.insert("reason".into(), json!(reason));
            if let Some(d) = description.filter(|d| !d.is_empty()) {
                out.insert("description".into(), json!(d));
            }
            let tid = thread.or(session.thread);
            if let Some(tid) = tid {
                let name = session.threads.iter().find(|t| t.id == tid).map(|t| t.name.clone());
                out.insert("thread".into(), json!({ "id": tid, "name": name }));
            }
            if session.threads.len() > 1 {
                out.insert("threads".into(), json!(session.threads.len()));
            }
            let list: Vec<Value> = session
                .frames
                .iter()
                .take(frames)
                .enumerate()
                .map(|(i, f)| {
                    let mut v = Map::new();
                    v.insert("index".into(), json!(i));
                    v.insert("name".into(), json!(short_frame_name(&f.name)));
                    if let Some(path) = &f.path {
                        v.insert("file".into(), json!(path));
                        v.insert("line".into(), json!(f.line + 1));
                        v.insert("column".into(), json!(f.column + 1));
                        let ex = excerpt(s, path, f.line, context, context);
                        if !ex.is_empty() {
                            v.insert("excerpt".into(), json!(ex));
                        }
                    }
                    if f.subtle {
                        v.insert("library".into(), json!(true));
                    }
                    Value::Object(v)
                })
                .collect();
            out.insert("frames".into(), json!(list));
            if session.frames.len() > frames {
                out.insert("moreFrames".into(), json!(session.frames.len() - frames));
            }
            if session.frame != 0 {
                out.insert("selectedFrame".into(), json!(session.frame));
            }
            let scopes: Vec<Value> =
                session.scopes.iter().map(|sc| json!({ "name": sc.name, "ref": sc.reference })).collect();
            out.insert("scopes".into(), json!(scopes));
            let locals_scope = session.scopes.iter().find(|sc| !sc.expensive);
            match locals_scope.and_then(|sc| session.variables.get(&sc.reference)) {
                Some(vars) => {
                    let list: Vec<Value> = vars.iter().take(50).map(variable_json).collect();
                    out.insert("locals".into(), json!(list));
                    if vars.len() > 50 {
                        out.insert(
                            "moreLocals".into(),
                            json!(format!(
                                "{} more; debug_variables with ref {}",
                                vars.len() - 50,
                                locals_scope.map_or(0, |s| s.reference)
                            )),
                        );
                    }
                }
                None if s.debug.settling() => {
                    out.insert("pending".into(), json!(true));
                }
                None => {}
            }
            if !s.debug.watches.is_empty() {
                let watches: Vec<Value> = s
                    .debug
                    .watches
                    .iter()
                    .map(|w| json!({ "expression": w.expression, "value": w.value.as_deref().map(|v| clip(v, MAX_VALUE_CHARS)), "error": w.error }))
                    .collect();
                out.insert("watches".into(), json!(watches));
            }
            "stopped"
        }
    };
    out.insert("state".into(), json!(state));
    // What the debugger's own summary knows beyond this one: the exception or panic (type and
    // message), the value a step out returned, the breakpoints hit, the exit code, the last error.
    if let Value::Object(more) = crate::debug::agent::stop_summary(s, 1, 0) {
        for key in ["exception", "returnValue", "breakpoints", "exitCode", "lastError"] {
            if let Some(v) = more.get(key).filter(|v| !v.is_null())
                && !out.contains_key(key)
            {
                out.insert(key.into(), v.clone());
            }
        }
    }
    Value::Object(out)
}

fn variables_of(s: &mut Session, p: &Value) -> CmdResult {
    let limit = opt_usize(p, "limit")?.unwrap_or(100).clamp(1, 1000);
    let Some(session) = &s.debug.session else { return Err(CommandError::Disabled("no debug session".into())) };
    let reference = p.get("reference").and_then(Value::as_i64);
    if let Some(r) = reference {
        return Ok(match session.variables.get(&r) {
            Some(vars) => {
                let mut v =
                    json!({ "ref": r, "variables": vars.iter().take(limit).map(variable_json).collect::<Vec<_>>() });
                if vars.len() > limit
                    && let Some(o) = v.as_object_mut()
                {
                    o.insert("truncated".into(), json!(format!("{} more; raise `limit`", vars.len() - limit)));
                }
                v
            }
            None => json!({ "ref": r, "pending": session.loading.contains(&r), "variables": Value::Null }),
        });
    }
    let scopes: Vec<Value> = session
        .scopes
        .iter()
        .map(|sc| match session.variables.get(&sc.reference) {
            Some(vars) => json!({
                "name": sc.name,
                "ref": sc.reference,
                "variables": vars.iter().take(limit).map(variable_json).collect::<Vec<_>>(),
                "count": vars.len(),
            }),
            None => {
                json!({ "name": sc.name, "ref": sc.reference, "expensive": sc.expensive, "variables": Value::Null })
            }
        })
        .collect();
    Ok(json!({ "frame": session.frame, "scopes": scopes, "pending": s.debug.settling() }))
}

// ---------------------------------------------------------------- runs and tests

fn run_configurations(s: &Session) -> Value {
    let list: Vec<Value> = s
        .run_configs
        .iter()
        .enumerate()
        .map(|(i, c)| {
            json!({
                "index": i,
                "name": c.name,
                "kind": if c.command.is_some() { "process" } else { "cargo" },
                "commandLine": c.command_line(),
                "selected": i == s.selected_run_config,
            })
        })
        .collect();
    json!(list)
}

fn run_status(st: RunStatus) -> &'static str {
    match st {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed(_) => "failed",
        RunStatus::Stopped => "stopped",
    }
}

fn tab_param(s: &Session, p: &Value) -> Result<RunTabId, CommandError> {
    match opt_usize(p, "tab")? {
        Some(t) => {
            let id = RunTabId(t as u64);
            s.ws.run(id).map(|_| id).ok_or_else(|| bad(format!("no Run tab {t}")))
        }
        None => s.ws.runs.last().map(|r| r.id).ok_or_else(|| CommandError::Disabled("nothing has run yet".into())),
    }
}

fn run_output(s: &mut Session, p: &Value) -> CmdResult {
    let tab = tab_param(s, p)?;
    let run = s.ws.run(tab).ok_or_else(|| bad("the Run tab is gone"))?;
    let total = run.lines.len();
    let tail = opt_usize(p, "tail")?.unwrap_or(100).clamp(1, 2000);
    let first = match opt_usize(p, "offset")? {
        Some(o) => o.saturating_sub(run.dropped_lines).min(total),
        None => total.saturating_sub(tail),
    };
    let lines: Vec<String> = run
        .lines
        .iter()
        .skip(first)
        .take(tail)
        .map(|l| {
            let text = clip(&l.text, 2000);
            if l.kind == ConsoleKind::Stderr { format!("[stderr] {text}") } else { text }
        })
        .collect();
    let shown = lines.len();
    let mut out = json!({
        "tab": tab.0,
        "title": run.title,
        "commandLine": run.command_line,
        "status": run_status(run.status),
        "running": s.runs.is_running(tab),
        "totalLines": total + run.dropped_lines,
        "firstLine": first + run.dropped_lines,
        "lines": lines,
    });
    if let Some(o) = out.as_object_mut() {
        if let RunStatus::Failed(code) = run.status {
            o.insert("exitCode".into(), json!(code));
        } else if run.status == RunStatus::Succeeded {
            o.insert("exitCode".into(), json!(0));
        }
        if let Some(d) = run.duration {
            o.insert("durationMs".into(), json!(d.as_millis()));
        }
        if first + shown < total {
            o.insert(
                "truncated".into(),
                json!(format!("more lines follow; pass offset {}", first + shown + run.dropped_lines)),
            );
        }
    }
    Ok(out)
}

fn stop_tab(s: &mut Session, p: &Value) -> CmdResult {
    let tab = match opt_usize(p, "tab")? {
        Some(t) => RunTabId(t as u64),
        None => {
            s.ws.runs
                .iter()
                .rev()
                .find(|r| s.runs.is_running(r.id))
                .map(|r| r.id)
                .ok_or_else(|| CommandError::Disabled("nothing is running".into()))?
        }
    };
    let stopped = crate::runs::stop(s, tab);
    Ok(json!({ "tab": tab.0, "stopped": stopped }))
}

fn test_status(st: crate::testing::TestStatus) -> &'static str {
    match st {
        crate::testing::TestStatus::Running => "running",
        crate::testing::TestStatus::Passed => "passed",
        crate::testing::TestStatus::Failed => "failed",
        crate::testing::TestStatus::Ignored => "ignored",
    }
}

fn test_summary(s: &mut Session, p: &Value) -> CmdResult {
    let tab = opt_usize(p, "tab")?.map(|t| t as u64);
    let tab = crate::testing::target_run(s, tab)?;
    let run = s.testing.run(tab).ok_or_else(|| bad("the test run is gone"))?;
    let include_passed = bool_or(p, "includePassed", false);
    let abs = |path: &Path| if path.is_absolute() { path.to_path_buf() } else { run.cwd.join(path) };
    let mut failures = Vec::new();
    let mut tests = Vec::new();
    for suite in &run.suites {
        for t in &suite.tests {
            if t.status == crate::testing::TestStatus::Failed {
                let mut f = Map::new();
                f.insert("name".into(), json!(t.name));
                f.insert("suite".into(), json!(suite.name));
                if let Some(m) = &t.message {
                    f.insert("message".into(), json!(clip(m, 2000)));
                }
                if let Some(loc) = t.failure.as_ref().or(t.location.as_ref()) {
                    let file = abs(&loc.path);
                    f.insert("file".into(), json!(file));
                    f.insert("line".into(), json!(loc.line + 1));
                    let ex = excerpt(s, &file, loc.line, 1, 1);
                    if !ex.is_empty() {
                        f.insert("excerpt".into(), json!(ex));
                    }
                }
                let out_lines: Vec<&str> = t.output.lines().collect();
                let skip = out_lines.len().saturating_sub(20);
                if !out_lines.is_empty() {
                    f.insert("output".into(), json!(out_lines.get(skip..).unwrap_or_default().join("\n")));
                }
                failures.push(Value::Object(f));
            }
            if include_passed {
                tests.push(json!({ "name": t.name, "suite": suite.name, "status": test_status(t.status), "durationMs": t.duration_ms }));
            }
        }
    }
    let shown = failures.len().min(50);
    let more = failures.len() - shown;
    failures.truncate(50);
    let mut out = json!({
        "tab": tab.0,
        "title": run.title,
        "commandLine": run.command_line,
        "finished": run.finished,
        "status": test_status(run.status()),
        "counts": run.counts(),
        "durationMs": run.duration_ms,
        "failures": failures,
    });
    if let Some(o) = out.as_object_mut() {
        if include_passed {
            o.insert("tests".into(), json!(tests));
        }
        if more > 0 {
            o.insert("truncated".into(), json!(format!("{more} more failures; see test.tree")));
        }
    }
    Ok(out)
}

fn run_filtered(s: &mut Session, p: &Value) -> CmdResult {
    let name = req_str(p, "name")?.trim().to_string();
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let title = if name.is_empty() { "Tests".to_string() } else { format!("Tests matching '{name}'") };
    let cargo_root =
        s.ws.project
            .as_ref()
            .and_then(|p| p.cargo.as_ref().map(|m| m.workspace_root.clone()))
            .or_else(|| root.join("Cargo.toml").is_file().then(|| root.clone()));
    let origin = if let Some(cargo_root) = cargo_root {
        let mut inv = codecraft_cargo::CargoInvocation::new(codecraft_cargo::CargoCommand::Test, cargo_root);
        if !name.is_empty() {
            inv.test_filter = Some(name.clone());
        }
        crate::testing::TestOrigin::Cargo(inv)
    } else if ["pyproject.toml", "setup.py", "setup.cfg", "pytest.ini", "tox.ini", "requirements.txt"]
        .iter()
        .any(|f| root.join(f).is_file())
        || has_python_tests(&root)
    {
        let python = crate::python_env::project_python(&root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let mut args = vec!["-m", "pytest"];
        if !name.is_empty() {
            args.extend(["-k", name.as_str()]);
        }
        crate::testing::TestOrigin::Process(crate::runs::ProcessCommand::new(python, &args, root))
    } else {
        return Err(CommandError::Failed(
            "no Cargo or Python project here; run a test file with run.testsInFile or a configuration with run.run"
                .into(),
        ));
    };
    let tab = crate::testing::start_origin(s, origin, &title)?;
    Ok(json!({ "tab": tab.0, "title": title }))
}

fn has_python_tests(root: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(root) else { return false };
    rd.flatten().take(500).any(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        (name.starts_with("test_") && name.ends_with(".py")) || name == "tests"
    })
}

// ---------------------------------------------------------------- profiler

fn hotspots(s: &mut Session, p: &Value) -> CmdResult {
    let limit = opt_usize(p, "limit")?.unwrap_or(20).clamp(1, 200);
    if s.profiler.snapshot.is_none() {
        let why = if !s.profiler.recordings.is_empty() {
            "the profiler is still recording; stop it with profiler.stop".to_string()
        } else if s.profiler.loading.is_some() {
            "the snapshot is loading".to_string()
        } else if let Some(e) = &s.profiler.error {
            format!("no snapshot: {e}")
        } else {
            "no profiler snapshot is open; profile a configuration first".to_string()
        };
        return Err(CommandError::Disabled(why));
    }
    let sort = crate::profiler_agent_cmds::sort_param(p)?;
    let status = s.execute("profiler.status", &Value::Null)?;
    // The order is fixed (QA M14 #25: it followed the IDE's Method List sort), and so is the
    // selection: all threads and the whole run unless the request names a thread or a range.
    let (profile, analysis) = {
        let snap = s
            .profiler
            .snapshot
            .as_ref()
            .ok_or_else(|| CommandError::Disabled("no profiler snapshot is open".into()))?;
        let sel = crate::profiler_agent_cmds::selection_param(p, &snap.profile)?;
        let analysis = if snap.analysis.selection() == sel {
            std::sync::Arc::clone(&snap.analysis)
        } else if let Some(c) = s.profiler.cached(&snap.path).filter(|_| sel == codecraft_profile::Selection::default())
        {
            c.analysis
        } else {
            std::sync::Arc::new(codecraft_profile::Analysis::select(&snap.profile, &sel))
        };
        (std::sync::Arc::clone(&snap.profile), analysis)
    };
    let ordered = codecraft_profile::hotspots(&profile, &analysis.methods, sort);
    let root = s.project_root();
    // More than `limit`, to find the project's own functions below library code.
    let all: Vec<Value> = ordered
        .iter()
        .take(limit.max(PROJECT_SCAN))
        .map(|m| hotspot_json(&crate::profiler_cmds::method_json(&profile, &analysis.tree, m), root.as_deref()))
        .collect();
    let list: Vec<Value> = all.iter().take(limit).cloned().collect();
    let snap = status.get("snapshot").cloned().unwrap_or(Value::Null);
    let memory = snap.get("unit").and_then(Value::as_str).is_some_and(|u| u == "bytes");
    let note = if memory {
        "selfPercent: share of the bytes allocated in the function itself; totalPercent: including what it calls"
    } else {
        "selfPercent: time in the function itself; totalPercent: including what it calls"
    };
    let restricted = analysis.thread.is_some() || analysis.range.is_some();
    let mut out = json!({
        "snapshot": snap.get("path"),
        "unit": snap.get("unit"),
        "samples": if restricted { json!(analysis.samples) } else { snap.get("samples").cloned().unwrap_or(Value::Null) },
        "total": analysis.tree.total,
        "sort": sort.name(),
        "order": crate::profiler_report::order_text(sort),
        "hotspots": list,
        "note": note,
    });
    if restricted && let Some(o) = out.as_object_mut() {
        o.insert(
            "selection".into(),
            json!({
                "thread": analysis.thread.and_then(|t| profile.threads.get(t)).map(|t| t.name.clone()),
                "range": analysis.range.map(|r| json!({
                    "startMs": crate::profiler_cmds::ns_ms(r.0),
                    "endMs": crate::profiler_cmds::ns_ms(r.1),
                })),
            }),
        );
    }
    // When library code (the standard library, the interpreter's import machinery) fills the top
    // of the list, the project's own functions come separately (QA M14: a tracemalloc report
    // showed only `<frozen importlib._bootstrap>` frames).
    let in_project = |v: &Value| v.get("inProject") == Some(&Value::Bool(true));
    if !list.iter().take(5).all(in_project) {
        let own: Vec<Value> = all.iter().filter(|v| in_project(v)).take(10).cloned().collect();
        if let Some(o) = out.as_object_mut()
            && !own.is_empty()
        {
            o.insert("projectHotspots".into(), json!(own));
        }
    }
    Ok(out)
}

/// Methods read to find the project's own hotspots.
const PROJECT_SCAN: usize = 2000;

/// One method of `profiler.methods` for agents: file and 1-based line apart, `inProject` when
/// the function is in the project's files.
fn hotspot_json(m: &Value, root: Option<&Path>) -> Value {
    let mut o = Map::new();
    for key in ["name", "module", "selfPercent", "totalPercent", "self", "total"] {
        if let Some(v) = m.get(key).filter(|v| !v.is_null()) {
            o.insert(key.into(), v.clone());
        }
    }
    if let Some(loc) = m.get("location").and_then(Value::as_str) {
        let (file, line) = match loc.rsplit_once(':').and_then(|(f, l)| l.parse::<usize>().ok().map(|l| (f, l))) {
            Some((file, line)) => (file, Some(line)),
            None => (loc, None),
        };
        let path = Path::new(file);
        let inside = root.is_some_and(|r| {
            if path.is_absolute() {
                path.starts_with(r) || crate::normalize_path(path).starts_with(r)
            } else {
                !file.starts_with('<') && !file.starts_with('~') && r.join(path).is_file()
            }
        });
        // Paths relative to the project (cProfile's script names) become absolute.
        let shown = match root {
            Some(r) if inside && path.is_relative() => r.join(path).display().to_string(),
            _ => file.to_string(),
        };
        o.insert("file".into(), json!(shown));
        if let Some(line) = line {
            o.insert("line".into(), json!(line));
        }
        if inside {
            o.insert("inProject".into(), json!(true));
        }
    }
    Value::Object(o)
}

// ---------------------------------------------------------------- language servers

/// What an agent LSP query asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LspKind {
    Definition,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl LspKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "definition" => Self::Definition,
            "references" => Self::References,
            "hover" => Self::Hover,
            "documentSymbols" => Self::DocumentSymbols,
            "workspaceSymbols" => Self::WorkspaceSymbols,
            "prepareCallHierarchy" => Self::PrepareCallHierarchy,
            "incomingCalls" => Self::IncomingCalls,
            "outgoingCalls" => Self::OutgoingCalls,
            _ => return None,
        })
    }
}

/// The server for a file (by language), or any ready server for workspace queries.
pub(crate) fn server_for(s: &Session, path: Option<&Path>) -> Result<codecraft_lsp::ServerKind, CommandError> {
    match path {
        Some(path) => {
            let lang = codecraft_syntax::Language::from_path(path);
            crate::lsp::server_kind(lang)
                .ok_or_else(|| CommandError::Failed(format!("no language server handles {}", path.display())))
        }
        None => s
            .servers
            .keys()
            .copied()
            .find(|k| s.ready_client(*k).is_some())
            .ok_or_else(|| CommandError::Disabled("no language server is ready".into())),
    }
}

/// `(line, UTF-16 character)` of a 1-based line and either a 1-based column or a symbol on it.
pub(crate) fn lsp_position(s: &Session, path: &Path, p: &Value) -> Result<(u32, u32), CommandError> {
    let line = line_param(p, "line")?.ok_or_else(|| bad("missing `line`"))?;
    let (text, _) = file_text(s, path)?;
    let doc = Document::from_text(&text);
    if line >= doc.len_lines() {
        return Err(bad(format!(
            "line {} is past the end of {} ({} lines)",
            line + 1,
            path.display(),
            doc.len_lines()
        )));
    }
    let line_text = doc.line_text(line).unwrap_or_default();
    let column = match (line_param(p, "column")?, opt_str(p, "symbol").filter(|x| !x.is_empty())) {
        (Some(c), _) => c,
        (None, Some(sym)) => find_symbol(&line_text, sym)
            .ok_or_else(|| bad(format!("`{sym}` does not occur on line {} of {}", line + 1, path.display())))?,
        (None, None) => line_text.chars().position(|c| !c.is_whitespace()).unwrap_or(0),
    };
    let offset = doc.position_to_offset(Position::new(line, column));
    Ok(doc.offset_to_utf16(offset))
}

/// Char column of the first whole-word occurrence of `sym` in `line` (any occurrence if none is
/// a whole word).
fn find_symbol(line: &str, sym: &str) -> Option<usize> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut fallback = None;
    for (byte, _) in line.match_indices(sym) {
        let col = line.get(..byte)?.chars().count();
        let before = line.get(..byte).and_then(|b| b.chars().next_back());
        let after = line.get(byte + sym.len()..).and_then(|a| a.chars().next());
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            return Some(col);
        }
        fallback.get_or_insert(col);
    }
    fallback
}

fn lsp_query(s: &mut Session, p: &Value) -> CmdResult {
    let kind_id = req_str(p, "kind")?;
    let kind = LspKind::parse(kind_id).ok_or_else(|| bad(format!("unknown kind `{kind_id}`")))?;
    if !s.lsp_enabled {
        return Err(CommandError::Disabled("language servers are off in this session".into()));
    }
    let token = s.agent_tools.next_lsp_token();
    let pending = crate::lsp::Pending::Agent(token);
    match kind {
        LspKind::WorkspaceSymbols => {
            let query = req_str(p, "query")?.to_string();
            let server = server_for(s, None)?;
            s.lsp_send_to(server, |c| c.workspace_symbol(&query), pending)?;
        }
        LspKind::IncomingCalls | LspKind::OutgoingCalls => {
            let item = p
                .get("item")
                .filter(|i| i.is_object())
                .cloned()
                .ok_or_else(|| bad("missing `item` (from prepareCallHierarchy)"))?;
            let path = item
                .get("uri")
                .and_then(Value::as_str)
                .and_then(|u| u.parse::<lt::Uri>().ok())
                .and_then(|u| codecraft_lsp::uri_to_path(&u));
            let server = server_for(s, path.as_deref())?;
            if kind == LspKind::IncomingCalls {
                s.lsp_send_to(server, |c| c.call_hierarchy_incoming(item), pending)?;
            } else {
                s.lsp_send_to(server, |c| c.call_hierarchy_outgoing(item), pending)?;
            }
        }
        _ => {
            let path = path_param(s, p, "path")?;
            let server = server_for(s, Some(&path))?;
            if let Some(id) = s.ws.doc_by_path(&path) {
                s.lsp_sync(id);
            }
            if kind == LspKind::DocumentSymbols {
                s.lsp_send_to(server, |c| c.document_symbol(&path), pending)?;
            } else {
                let (line, ch) = lsp_position(s, &path, p)?;
                s.lsp_send_to(
                    server,
                    |c| match kind {
                        LspKind::Definition => c.definition(&path, line, ch),
                        LspKind::References => c.references(&path, line, ch, true),
                        LspKind::Hover => c.hover(&path, line, ch),
                        _ => c.prepare_call_hierarchy(&path, line, ch),
                    },
                    pending,
                )?;
            }
        }
    }
    s.agent_tools.lsp_started(token, kind);
    Ok(json!({ "token": token }))
}

fn lsp_query_result(s: &mut Session, p: &Value) -> CmdResult {
    let token = opt_usize(p, "token")?.ok_or_else(|| bad("missing `token`"))? as u64;
    let limit = opt_usize(p, "limit")?.unwrap_or(200).clamp(1, 2000);
    let Some((kind, result)) = s.agent_tools.lsp_take(token) else {
        return Ok(json!({ "done": false, "known": s.agent_tools.lsp_pending(token) }));
    };
    match result {
        Ok(value) => Ok(json!({ "done": true, "result": compact_lsp(s, kind, value, limit) })),
        Err(e) => Ok(json!({ "done": true, "error": e })),
    }
}

/// A location (`Location` or `LocationLink`) as `(path, 0-based line, utf-16 character)`.
fn location_of(v: &Value) -> Option<(PathBuf, u32, u32)> {
    let uri = v.get("uri").or_else(|| v.get("targetUri"))?.as_str()?;
    let range = v.get("targetSelectionRange").or_else(|| v.get("range")).or_else(|| v.get("targetRange"))?;
    let start = range.get("start")?;
    let line = u32::try_from(start.get("line")?.as_u64()?).ok()?;
    let ch = u32::try_from(start.get("character")?.as_u64()?).ok()?;
    let path = codecraft_lsp::uri_to_path(&uri.parse::<lt::Uri>().ok()?)?;
    Some((path, line, ch))
}

fn location_json(s: &Session, path: &Path, line: u32, ch: u32, with_excerpt: bool) -> Value {
    let line_text = s.line_of(path, line as usize);
    let column = crate::convert::utf16_col_to_char(&line_text, ch) + 1;
    let mut v = json!({ "file": path, "line": line as usize + 1, "column": column });
    if with_excerpt && let Some(o) = v.as_object_mut() {
        o.insert("excerpt".into(), json!(clip(line_text.trim(), 200)));
    }
    v
}

fn symbol_kind(k: u64) -> &'static str {
    const NAMES: [&str; 26] = [
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enumMember",
        "struct",
        "event",
        "operator",
        "typeParameter",
    ];
    usize::try_from(k).ok().and_then(|k| k.checked_sub(1)).and_then(|i| NAMES.get(i)).copied().unwrap_or("symbol")
}

fn flatten_symbols(list: &[Value], depth: usize, out: &mut Vec<Value>, limit: usize) {
    for sym in list {
        if out.len() >= limit {
            return;
        }
        let name = sym.get("name").and_then(Value::as_str).unwrap_or_default();
        let kind = symbol_kind(sym.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let range = sym.get("selectionRange").or_else(|| sym.get("range")).or_else(|| sym.pointer("/location/range"));
        let line = range.and_then(|r| r.pointer("/start/line")).and_then(Value::as_u64).unwrap_or(0) + 1;
        let end = sym
            .get("range")
            .or_else(|| sym.pointer("/location/range"))
            .and_then(|r| r.pointer("/end/line"))
            .and_then(Value::as_u64)
            .map(|l| l + 1);
        let mut v = json!({ "name": name, "kind": kind, "line": line, "depth": depth });
        if let Some(o) = v.as_object_mut() {
            if let Some(e) = end {
                o.insert("endLine".into(), json!(e));
            }
            if let Some(d) = sym.get("detail").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                o.insert("detail".into(), json!(clip(d, 200)));
            }
            if let Some(c) = sym.get("containerName").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                o.insert("container".into(), json!(c));
            }
        }
        out.push(v);
        if let Some(children) = sym.get("children").and_then(Value::as_array) {
            flatten_symbols(children, depth + 1, out, limit);
        }
    }
}

fn call_item_json(s: &Session, item: &Value, ranges: Option<&Value>) -> Value {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let kind = symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
    let mut v = json!({ "name": name, "kind": kind });
    if let (Some((path, line, ch)), Some(o)) = (location_of(item), v.as_object_mut()) {
        if let Value::Object(loc) = location_json(s, &path, line, ch, false) {
            o.extend(loc);
        }
        if let Some(d) = item.get("detail").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            o.insert("detail".into(), json!(clip(d, 200)));
        }
        if let Some(r) = ranges.and_then(Value::as_array) {
            let lines: Vec<u64> =
                r.iter().filter_map(|x| x.pointer("/start/line").and_then(Value::as_u64)).map(|l| l + 1).collect();
            o.insert("callSites".into(), json!(lines));
        }
    }
    v
}

/// A raw LSP answer as compact agent JSON.
pub(crate) fn compact_lsp(s: &Session, kind: LspKind, value: Value, limit: usize) -> Value {
    match kind {
        LspKind::Definition | LspKind::References => {
            let items: Vec<Value> = match &value {
                Value::Array(a) => a.clone(),
                Value::Null => Vec::new(),
                other => vec![other.clone()],
            };
            let total = items.len();
            let list: Vec<Value> = items
                .iter()
                .filter_map(location_of)
                .take(limit)
                .map(|(p, l, c)| location_json(s, &p, l, c, true))
                .collect();
            let mut v = json!({ "total": total, "locations": list });
            if total > limit
                && let Some(o) = v.as_object_mut()
            {
                o.insert("truncated".into(), json!(format!("{} more; raise `limit`", total - limit)));
            }
            v
        }
        LspKind::Hover => {
            let hover: Option<lt::Hover> = serde_json::from_value(value).ok().flatten();
            match hover.map(|h| crate::convert::hover_text(&h.contents)).filter(|t| !t.trim().is_empty()) {
                Some(text) => json!({ "text": clip(&text, 8000) }),
                None => json!({ "text": Value::Null, "note": "no hover information here" }),
            }
        }
        LspKind::DocumentSymbols | LspKind::WorkspaceSymbols => {
            let list = value.as_array().cloned().unwrap_or_default();
            let mut out = Vec::new();
            if kind == LspKind::DocumentSymbols {
                flatten_symbols(&list, 0, &mut out, limit);
            } else {
                for sym in list.iter().take(limit) {
                    let name = sym.get("name").and_then(Value::as_str).unwrap_or_default();
                    let k = symbol_kind(sym.get("kind").and_then(Value::as_u64).unwrap_or(0));
                    let mut v = json!({ "name": name, "kind": k });
                    if let (Some(o), Some(loc)) = (v.as_object_mut(), sym.get("location")) {
                        match location_of(loc) {
                            Some((p, l, c)) => {
                                if let Value::Object(m) = location_json(s, &p, l, c, false) {
                                    o.extend(m);
                                }
                            }
                            None => {
                                // WorkspaceSymbol with only a URI.
                                if let Some(path) = loc
                                    .get("uri")
                                    .and_then(Value::as_str)
                                    .and_then(|u| u.parse::<lt::Uri>().ok())
                                    .and_then(|u| codecraft_lsp::uri_to_path(&u))
                                {
                                    o.insert("file".into(), json!(path));
                                }
                            }
                        }
                        if let Some(c) = sym.get("containerName").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                            o.insert("container".into(), json!(c));
                        }
                    }
                    out.push(v);
                }
            }
            json!({ "total": list.len(), "symbols": out })
        }
        LspKind::PrepareCallHierarchy => json!({ "items": value.as_array().cloned().unwrap_or_default() }),
        LspKind::IncomingCalls | LspKind::OutgoingCalls => {
            let key = if kind == LspKind::IncomingCalls { "from" } else { "to" };
            let list: Vec<Value> = value
                .as_array()
                .map(|a| {
                    a.iter()
                        .take(limit)
                        .filter_map(|c| c.get(key).map(|item| call_item_json(s, item, c.get("fromRanges"))))
                        .collect()
                })
                .unwrap_or_default();
            json!({ "calls": list })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_are_found_as_whole_words() {
        assert_eq!(find_symbol("let total = totals + total_x;", "total"), Some(4));
        assert_eq!(find_symbol("xtotal", "total"), Some(1));
        assert_eq!(find_symbol("é = é + 1", "é"), Some(0));
        assert_eq!(find_symbol("abc", "zz"), None);
    }

    #[test]
    fn clip_and_symbol_kinds() {
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("ab", 3), "ab");
        assert_eq!(symbol_kind(12), "function");
        assert_eq!(symbol_kind(0), "symbol");
        assert_eq!(symbol_kind(u64::MAX), "symbol");
    }

    #[test]
    fn locations_parse_from_locations_and_links() {
        let loc = json!({ "uri": "file:///tmp/a.rs", "range": { "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 5 } } });
        assert_eq!(location_of(&loc), Some((PathBuf::from("/tmp/a.rs"), 3, 4)));
        let link = json!({ "targetUri": "file:///tmp/b.rs", "targetRange": { "start": { "line": 1, "character": 0 } },
                           "targetSelectionRange": { "start": { "line": 2, "character": 7 } } });
        assert_eq!(location_of(&link), Some((PathBuf::from("/tmp/b.rs"), 2, 7)));
        assert_eq!(location_of(&json!({ "uri": 5 })), None);
        assert_eq!(
            location_of(&json!({ "uri": "file:///a", "range": { "start": { "line": -1, "character": 0 } } })),
            None
        );
    }

    #[test]
    fn files_read_write_edit_through_commands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&root.join("home"));
        s.execute("file.open", &json!({ "path": root })).unwrap();
        let r = s.execute("files.read", &json!({ "path": "a.txt", "startLine": 2 })).unwrap();
        assert_eq!(r["content"], "2\ttwo\n3\tthree");
        assert_eq!(r["totalLines"], 3);
        assert!(s.execute("files.read", &json!({ "path": "a.txt", "startLine": 0 })).is_err());
        assert!(s.execute("files.read", &json!({ "path": "a.txt", "startLine": 99 })).is_err());
        assert!(s.execute("files.read", &json!({ "path": "nope.txt" })).is_err());
        let r = s.execute("files.edit", &json!({ "path": file, "oldText": "two", "newText": "2" })).unwrap();
        assert_eq!(r["replacements"], 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n2\nthree\n");
        assert!(s.execute("files.edit", &json!({ "path": file, "oldText": "zzz", "newText": "" })).is_err());
        std::fs::write(&file, "a a a").unwrap();
        assert!(s.execute("files.edit", &json!({ "path": file, "oldText": "a", "newText": "b" })).is_err());
        s.execute("files.edit", &json!({ "path": file, "oldText": "a", "newText": "b", "replaceAll": true })).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "b b b");
        let r = s.execute("files.write", &json!({ "path": "new/b.txt", "content": "hi" })).unwrap();
        assert_eq!(r["created"], true);
        assert_eq!(std::fs::read_to_string(root.join("new/b.txt")).unwrap(), "hi");
        // Open documents: reads see unsaved text, writes go through the editor.
        s.execute("file.open", &json!({ "path": file })).unwrap();
        s.execute("editor.type", &json!({ "text": "X" })).unwrap();
        let r = s.execute("files.read", &json!({ "path": file })).unwrap();
        assert_eq!(r["unsaved"], true);
        assert!(r["content"].as_str().unwrap().contains('X'));
        let long: String = (1..=1000).map(|i| format!("{i}\n")).collect();
        std::fs::write(root.join("long.txt"), long).unwrap();
        let r = s.execute("files.read", &json!({ "path": "long.txt" })).unwrap();
        assert_eq!(r["endLine"], 400);
        assert!(r["truncated"].as_str().unwrap().contains("401"));
        assert!(s.execute("files.write", &json!({ "path": root, "content": "x" })).is_err());
    }

    #[test]
    fn problems_filter_by_scope_and_severity() {
        use codecraft_workspace::{Problem, ProblemSource};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        let mk = |p: &str, sev| Problem {
            path: root.join(p),
            start: Position::new(0, 3),
            end: Position::new(0, 4),
            severity: sev,
            message: "m".into(),
            code: Some("E1".into()),
            source: ProblemSource::Cargo,
        };
        for pr in [mk("src/a.rs", Severity::Error), mk("b.rs", Severity::Warning), mk("src/a.rs", Severity::Hint)] {
            s.ws.add_cargo_problem(pr);
        }
        let all = s.execute("analysis.problems", &Value::Null).unwrap();
        assert_eq!(all["total"], 2);
        assert_eq!(all["problems"][0]["severity"], "error");
        assert_eq!(all["problems"][0]["line"], 1);
        assert_eq!(all["problems"][0]["column"], 4);
        let src = s.execute("analysis.problems", &json!({ "path": "src", "severity": "hint" })).unwrap();
        assert_eq!(src["total"], 2);
        assert!(s.execute("analysis.problems", &json!({ "severity": "loud" })).is_err());
        let none = s.execute("analysis.problems", &json!({ "path": "nowhere" })).unwrap();
        assert!(none["note"].is_string());
    }

    #[test]
    fn summaries_without_sessions() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        assert_eq!(s.execute("debug.agentSummary", &Value::Null).unwrap()["state"], "none");
        assert!(s.execute("debug.variablesOf", &Value::Null).is_err());
        assert!(s.execute("run.output", &Value::Null).is_err());
        assert!(s.execute("run.stopTab", &Value::Null).is_err());
        assert!(s.execute("test.summary", &Value::Null).is_err());
        assert!(s.execute("profiler.hotspots", &Value::Null).is_err());
        assert!(s.execute("lsp.query", &json!({ "kind": "hover", "path": "/x.rs", "line": 1 })).is_err());
        assert!(s.execute("lsp.query", &json!({ "kind": "nope" })).is_err());
        assert_eq!(s.execute("lsp.queryResult", &json!({ "token": 99 })).unwrap()["done"], false);
        let found = s.execute("commands.search", &json!({ "query": "step over" })).unwrap();
        assert!(found["commands"].as_array().unwrap().iter().any(|c| c["id"] == "run.debuggingActions.stepOver"));
        let page = s.execute("commands.search", &json!({ "limit": 3 })).unwrap();
        assert_eq!(page["commands"].as_array().unwrap().len(), 3);
        assert!(page["truncated"].is_string());
        assert_eq!(s.execute("run.configurations", &Value::Null).unwrap(), json!([]));
        assert!(s.execute("project.info", &Value::Null).unwrap()["project"].is_null());
    }

    #[test]
    fn long_cpp_frame_names_are_shortened() {
        let long = "lookup(std::__1::map<std::__1::basic_string<char, std::__1::char_traits<char>, std::__1::allocator<char>>, int, std::__1::less<std::__1::basic_string<char, std::__1::char_traits<char>, std::__1::allocator<char>>>, std::__1::allocator<std::__1::pair<std::__1::basic_string<char, std::__1::char_traits<char>, std::__1::allocator<char>> const, int>>> const&, std::__1::basic_string<char, std::__1::char_traits<char>, std::__1::allocator<char>> const&)";
        assert_eq!(short_frame_name(long), "lookup(…)");
        assert_eq!(short_frame_name("main"), "main");
        assert_eq!(short_frame_name("std::__1::vector<int>::push_back(int&&)"), "std::vector<int>::push_back(int&&)");
        assert_eq!(short_frame_name("shop::tests::total_of_two::{closure#0}"), "shop::tests::total_of_two::{closure#0}");
        let weird = "x".repeat(500);
        assert!(short_frame_name(&weird).chars().count() <= MAX_FRAME_NAME + 1);
        let template = format!("f<{}>(int)", "a(b)".repeat(100));
        assert!(short_frame_name(&template).chars().count() <= MAX_FRAME_NAME + 3);
    }
}
