//! Profiler commands: Run › Profile, Attach Profiler, Open Profiler Snapshot (M11 track P), and
//! the Profiler tool window's actions (`profiler.*`).

use serde_json::{Value, json};

use codecraft_dap::processes::ProcessKind;
use codecraft_workspace::{RunTabId, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{bool_or, opt_path, opt_str, opt_usize, req_str, req_usize};
use crate::profiler::{self, MethodColumn, ProfilerTab, ProfilerTool, Snapshot};
use crate::{CommandError, Session};

/// Nodes returned by `profiler.callTree` at most.
const MAX_DUMP_NODES: usize = 2000;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "run.profile",
            "Profile 'Run'",
            None,
            "{config?: int|name, kind?: cpu|memory, tool?: string, seconds?: number, interval?: number, target?} — run the \
             selected (or given) run configuration under a profiler in a Run tab (Cargo targets and CMake chains are \
             built first, then only the program is recorded; CPU: xctrace on macOS, samply, perf on Linux, cProfile \
             or py-spy for Python; memory: leaks on macOS, heaptrack on Linux, tracemalloc or memray for Python; \
             `interval`: seconds between tracemalloc memory samples, default 0.1, 0 = one snapshot at the end), then \
             open the snapshot in the Profiler tool window → {tab, job, config, tool}",
            Context::Global,
            can_profile,
            profile_selected,
        ),
        spec(
            "run.profileRun.profile",
            "Profile 'Run'",
            None,
            "{config?: int|name, kind?: cpu|memory, tool?: string, seconds?: number} — same as run.profile",
            Context::Global,
            can_profile,
            profile_selected,
        ),
        spec(
            "profiler.profileMemory",
            "Profile Memory 'Run'",
            None,
            "{config?: int|name, tool?: string} — run.profile with kind memory",
            Context::Global,
            can_profile,
            |s, p| {
                let mut p = if p.is_object() { p.clone() } else { json!({}) };
                if let Some(o) = p.as_object_mut() {
                    o.insert("kind".into(), json!("memory"));
                }
                profile_selected(s, &p)
            },
        ),
        spec(
            "run.attachProfilerToProcess",
            "Attach Profiler to Process…",
            None,
            "{pid?: int, python?: bool, name?: string} — with `pid`, record that process (samply -p / perf -p / \
             py-spy --pid) until profiler.stop; without, open the process list",
            Context::Global,
            always,
            attach_profiler,
        ),
        spec(
            "run.openProfilerSnapshot",
            "Open Profiler Snapshot",
            None,
            "{path?: string} — open a samply/Firefox Profiler JSON (.json, .json.gz), speedscope JSON, collapsed \
             stacks, perf script or DTrace aggregation file; without `path` the UI asks",
            Context::Global,
            always,
            open_snapshot,
        ),
        spec(
            "run.openProfilerSnapshot.open",
            "Open…",
            None,
            "{path?: string} — same as run.openProfilerSnapshot",
            Context::Global,
            always,
            open_snapshot,
        ),
        // ----- process list
        spec(
            "profiler.attach.refresh",
            "Refresh Process List",
            None,
            "{allUsers?: bool} — list the processes again",
            Context::Global,
            always,
            |s, p| {
                profiler::open_attach_picker(s, p.get("allUsers").and_then(Value::as_bool));
                ok()
            },
        ),
        spec(
            "profiler.attach.filter",
            "Filter Process List",
            None,
            "{query: string} — show the processes whose name, command line or pid match",
            Context::Global,
            picker_open,
            |s, p| {
                let query = req_str(p, "query")?.to_owned();
                if let Some(a) = &mut s.profiler.attach {
                    a.query = query;
                    a.selected = 0;
                }
                Ok(attach_list(s))
            },
        ),
        spec(
            "profiler.attach.move",
            "Move in Process List",
            None,
            "{delta: int} — move the selection",
            Context::Global,
            picker_open,
            |s, p| {
                let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
                if let Some(a) = &mut s.profiler.attach {
                    let len = a.visible().len();
                    if len > 0 {
                        let len = i64::try_from(len).unwrap_or(i64::MAX);
                        let cur = i64::try_from(a.selected).unwrap_or(0);
                        a.selected = usize::try_from(cur.saturating_add(delta).clamp(0, len - 1)).unwrap_or(0);
                    }
                }
                ok()
            },
        ),
        spec(
            "profiler.attach.confirm",
            "Profile Selected Process",
            None,
            "{index?: int} — start recording the selected (or given visible) process",
            Context::Global,
            picker_open,
            attach_confirm,
        ),
        spec("profiler.attach.cancel", "Close Process List", None, "", Context::Global, always, |s, _| {
            s.profiler.attach = None;
            ok()
        }),
        // ----- recordings and tools
        spec(
            "profiler.stop",
            "Stop Profiling",
            None,
            "{tab?: int} — stop the newest (or given Run tab's) recording; the profiler saves its snapshot, which \
             then opens (a second stop kills it)",
            Context::Global,
            recording,
            stop,
        ),
        spec(
            "profiler.tools",
            "Profiler Tools",
            None,
            "→ [{tool, kind, path, installed, builtin, pythonOnly, hint}] — which profilers are installed",
            Context::Global,
            always,
            |s, _| Ok(tools(s)),
        ),
        spec(
            "profiler.install",
            "Install Profiler",
            None,
            "{tool: samply|py-spy|memray} — run the install command in a Terminal tab (py-spy and samply into \
             $CODECRAFT_HOME/tools; memray into the project's interpreter with uv)",
            Context::Global,
            always,
            install,
        ),
        // ----- comparison and focus
        spec(
            "profiler.compareWith",
            "Compare with Snapshot…",
            None,
            "{path: string} — compare the open snapshot against another (the baseline): the flame graph colours \
             what got slower (more of the total) and faster, the method list gets a Δ column",
            Context::Global,
            has_snapshot,
            |s, p| {
                let path = opt_path(p, "path").ok_or_else(|| CommandError::BadParams("`path` is required".into()))?;
                profiler::set_baseline(s, &path)?;
                ok()
            },
        ),
        spec("profiler.clearBaseline", "Stop Comparing", None, "", Context::Global, has_snapshot, |s, _| {
            snapshot_mut(s)?.baseline = None;
            ok()
        }),
        spec(
            "profiler.focus",
            "Focus on Method",
            None,
            "{frame?: int, name?: string} — zoom the flame graph and the call tree into the heaviest call of a \
             method (by frame id or name substring); without either, reset",
            Context::Global,
            has_snapshot,
            focus,
        ),
        spec(
            "profiler.status",
            "Profiler Status",
            None,
            "→ {snapshot: {path, format, unit, total, samples, threads, thread, tab, zoom, search, matched}, \
             loading, error, missing, recordings}",
            Context::Global,
            always,
            |s, _| Ok(status(s)),
        ),
        spec(
            "profiler.recent",
            "Recent Profiler Snapshots",
            None,
            "→ [path] newest first",
            Context::Global,
            always,
            |s, _| Ok(json!(s.profiler.recent())),
        ),
        spec("profiler.clearRecent", "Clear Recent Snapshots", None, "", Context::Global, always, |s, _| {
            s.profiler.clear_recent();
            ok()
        }),
        spec("profiler.close", "Close Snapshot", None, "", Context::Global, has_snapshot, |s, _| {
            s.profiler.snapshot = None;
            ok()
        }),
        // ----- view
        spec(
            "profiler.selectTab",
            "Select Profiler View",
            None,
            "{tab: flameGraph|icicle|callTree|methodList|timeline|memory}",
            Context::Global,
            always,
            |s, p| {
                let name = req_str(p, "tab")?;
                s.profiler.tab = ProfilerTab::from_name(name)
                    .ok_or_else(|| CommandError::BadParams(format!("unknown profiler view `{name}`")))?;
                s.ws.layout.show(ToolWindowId::Profiler);
                ok()
            },
        ),
        spec(
            "profiler.selectThread",
            "Select Profiled Thread",
            None,
            "{thread?: int} — show one thread (index into status.snapshot.threads); without, all threads merged",
            Context::Global,
            has_snapshot,
            |s, p| {
                let thread = opt_usize(p, "thread")?;
                profiler::select_thread(s, thread)?;
                ok()
            },
        ),
        spec(
            "profiler.selectRange",
            "Select Time Range",
            None,
            "{startMs?: number, endMs?: number} — show only the samples taken between the two times (ms since the \
             first sample; formats with sample times: xctrace, samply): the flame graph, call tree, method list, hot \
             lines and the editor gutter follow, as does the thread selection; without both, the whole run → \
             {startMs, endMs, durationMs}",
            Context::Global,
            has_snapshot,
            select_range,
        ),
        spec(
            "profiler.atCaret",
            "Profile Context Configuration",
            None,
            "{path?: string, line?: int (0-based), kind?: cpu|memory, tool?: string, seconds?: number, interval?: \
             number} — profile what the gutter ▶ at the line runs (default: the caret): the test there (Rust `cargo \
             test --lib name -- --exact`, pytest `file::test`, GoogleTest/Catch2/doctest/Boost.Test filters), its \
             module or `main`; built first like run.profile → {job, tab, config, tool}",
            Context::Global,
            always,
            profile_at_caret,
        ),
        spec(
            "profiler.openPeakSnapshot",
            "Open Snapshot at Memory Peak",
            None,
            "— open the whole tracemalloc snapshot taken nearest the memory peak (written next to the open snapshot \
             by memory profiles with periodic snapshots)",
            Context::Global,
            has_peak,
            |s, _| {
                let path = s
                    .profiler
                    .snapshot
                    .as_ref()
                    .and_then(Snapshot::peak_snapshot)
                    .ok_or_else(|| CommandError::Failed("this snapshot has no snapshot at the memory peak".into()))?;
                profiler::open_snapshot(s, &path)?;
                Ok(json!({ "path": path }))
            },
        ),
        spec(
            "profiler.zoom",
            "Zoom Flame Graph",
            None,
            "{node?: int} — zoom the flame graph into a call node; without `node`, reset",
            Context::Global,
            has_snapshot,
            |s, p| {
                let node = opt_node(p, "node")?;
                let snap = snapshot_mut(s)?;
                if let Some(n) = node {
                    snap.analysis.tree.node(n).ok_or_else(|| CommandError::BadParams(format!("no call node {n}")))?;
                    snap.selected = Some(n);
                }
                snap.zoom = node;
                ok()
            },
        ),
        spec("profiler.resetZoom", "Reset Zoom", None, "", Context::Global, has_snapshot, |s, _| {
            snapshot_mut(s)?.zoom = None;
            ok()
        }),
        spec(
            "profiler.search",
            "Search in Profile",
            None,
            "{query: string} — highlight frames whose name or module contains the text → {matched, percent}",
            Context::Global,
            has_snapshot,
            |s, p| {
                let query = opt_str(p, "query").unwrap_or("");
                let snap = snapshot_mut(s)?;
                snap.set_search(query);
                Ok(json!({ "matched": snap.matched, "percent": snap.analysis.tree.percent(snap.matched) }))
            },
        ),
        spec(
            "profiler.select",
            "Select Call Node",
            None,
            "{node: int} — select a call tree node (expanding its ancestors)",
            Context::Global,
            has_snapshot,
            |s, p| {
                let node = req_node(p, "node")?;
                let snap = snapshot_mut(s)?;
                let path = snap.analysis.tree.path(node);
                if path.is_empty() {
                    return Err(CommandError::BadParams(format!("no call node {node}")));
                }
                for n in path.iter().take(path.len().saturating_sub(1)) {
                    snap.expanded.insert(*n);
                }
                snap.selected = Some(node);
                ok()
            },
        ),
        spec(
            "profiler.toggleNode",
            "Expand/Collapse Call Node",
            None,
            "{node: int, expanded?: bool}",
            Context::Global,
            has_snapshot,
            |s, p| {
                let node = req_node(p, "node")?;
                let snap = snapshot_mut(s)?;
                snap.analysis.tree.node(node).ok_or_else(|| CommandError::BadParams(format!("no call node {node}")))?;
                let expand = bool_or(p, "expanded", !snap.expanded.contains(&node));
                if expand {
                    snap.expanded.insert(node);
                } else {
                    snap.expanded.remove(&node);
                }
                ok()
            },
        ),
        spec("profiler.expandAll", "Expand All", None, "", Context::Global, has_snapshot, |s, _| {
            let snap = snapshot_mut(s)?;
            let tree = &snap.analysis.tree;
            snap.expanded = (0..tree.nodes.len())
                .filter(|i| tree.nodes.get(*i).is_some_and(|n| !n.children.is_empty()))
                .filter_map(|i| u32::try_from(i).ok())
                .collect();
            ok()
        }),
        spec("profiler.collapseAll", "Collapse All", None, "", Context::Global, has_snapshot, |s, _| {
            snapshot_mut(s)?.expanded.clear();
            ok()
        }),
        spec(
            "profiler.sortMethods",
            "Sort Method List",
            None,
            "{column: name|self|total, descending?: bool}",
            Context::Global,
            has_snapshot,
            |s, p| {
                let name = req_str(p, "column")?;
                let column = MethodColumn::from_name(name)
                    .ok_or_else(|| CommandError::BadParams(format!("unknown column `{name}`")))?;
                let snap = snapshot_mut(s)?;
                let default_desc = column != MethodColumn::Name;
                let descending = match p.get("descending").and_then(Value::as_bool) {
                    Some(d) => d,
                    // Clicking the sorted column again flips the order.
                    None if snap.sort == column => !snap.descending,
                    None => default_desc,
                };
                snap.sort = column;
                snap.descending = descending;
                snap.sort_methods();
                ok()
            },
        ),
        spec(
            "profiler.selectMethod",
            "Select Method",
            None,
            "{frame: int} — select a method → {callers, callees}",
            Context::Global,
            has_snapshot,
            |s, p| {
                let frame = req_node(p, "frame")?;
                let snap = snapshot_mut(s)?;
                snap.profile.frame(frame).ok_or_else(|| CommandError::BadParams(format!("no frame {frame}")))?;
                snap.select_method(frame);
                Ok(method_edges(snap, frame))
            },
        ),
        spec(
            "profiler.jumpToSource",
            "Jump to Source",
            None,
            "{node?: int, frame?: int} — open the frame's file at its line (default: the selected node or method)",
            Context::Global,
            has_snapshot,
            jump_to_source,
        ),
        spec(
            "profiler.openLine",
            "Open Hot Line",
            None,
            "{frame: int, line: int} — open a line of a method's source file (a hot line)",
            Context::Global,
            has_snapshot,
            |s, p| {
                let frame = req_node(p, "frame")?;
                let line = u32::try_from(req_usize(p, "line")?)
                    .map_err(|_| CommandError::BadParams("`line` is out of range".into()))?;
                let snap = snapshot_mut(s)?;
                let f =
                    snap.profile.frame(frame).ok_or_else(|| CommandError::BadParams(format!("no frame {frame}")))?;
                let file =
                    f.file.clone().ok_or_else(|| CommandError::Failed(format!("no source file for {}", f.name)))?;
                profiler::jump_to_location(s, &file, Some(line))?;
                ok()
            },
        ),
        spec(
            "profiler.openLocation",
            "Open Source Location",
            None,
            "{file: string, line?: int (1-based)} — open a source file of the profile at a line (relative paths \
             against the project root), e.g. an allocation site at the memory peak",
            Context::Global,
            has_snapshot,
            |s, p| {
                let file = req_str(p, "file")?.to_string();
                let line = opt_usize(p, "line")?
                    .map(|l| u32::try_from(l).map_err(|_| CommandError::BadParams("`line` is out of range".into())))
                    .transpose()?;
                profiler::jump_to_location(s, &file, line)?;
                ok()
            },
        ),
        spec(
            "profiler.callTree",
            "Profiler Call Tree",
            None,
            "{depth?: int, minPercent?: number} → nodes [{id, depth, name, module, location, total, self, percent}] \
             in tree order",
            Context::Global,
            has_snapshot,
            call_tree_dump,
        ),
        spec(
            "profiler.methods",
            "Profiler Method List",
            None,
            "{limit?: int} → [{frame, name, module, location, self, total, selfPercent, totalPercent}] in display order",
            Context::Global,
            has_snapshot,
            methods_dump,
        ),
    ]
}

// ---------------------------------------------------------------- predicates and params

fn can_profile(s: &Session) -> Result<(), String> {
    if !s.run_configs.is_empty() || is_cargo_project(s) {
        Ok(())
    } else {
        Err("no run configuration to profile".into())
    }
}

fn is_cargo_project(s: &Session) -> bool {
    s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some() || p.root.join("Cargo.toml").is_file())
}

fn has_snapshot(s: &Session) -> Result<(), String> {
    s.profiler.snapshot.as_ref().map(|_| ()).ok_or_else(|| "no profiler snapshot is open".into())
}

fn has_peak(s: &Session) -> Result<(), String> {
    has_snapshot(s)?;
    s.profiler
        .snapshot
        .as_ref()
        .and_then(Snapshot::peak_snapshot)
        .map(|_| ())
        .ok_or_else(|| "the snapshot has no snapshot at the memory peak".into())
}

fn picker_open(s: &Session) -> Result<(), String> {
    s.profiler.attach.as_ref().map(|_| ()).ok_or_else(|| "the process list is not open".into())
}

fn recording(s: &Session) -> Result<(), String> {
    if s.profiler.recording_active() { Ok(()) } else { Err("no profiler is recording".into()) }
}

fn snapshot_mut(s: &mut Session) -> Result<&mut Snapshot, CommandError> {
    s.profiler.snapshot.as_mut().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))
}

fn opt_node(p: &Value, key: &str) -> Result<Option<u32>, CommandError> {
    opt_usize(p, key)?
        .map(|n| u32::try_from(n).map_err(|_| CommandError::BadParams(format!("`{key}` is out of range"))))
        .transpose()
}

fn req_node(p: &Value, key: &str) -> Result<u32, CommandError> {
    let n = req_usize(p, key)?;
    u32::try_from(n).map_err(|_| CommandError::BadParams(format!("`{key}` is out of range")))
}

// ---------------------------------------------------------------- run

fn profile_selected(s: &mut Session, p: &Value) -> CmdResult {
    let p = if p.is_object() { p.clone() } else { json!({}) };
    let params = crate::profiler_agent_cmds::RecordParams::read(&p)?;
    let config = crate::profiler_agent_cmds::config_for(s, &p)?;
    crate::profiler_agent_cmds::record_config(s, &config, &params)
}

/// Profile Context Configuration: what the gutter ▶ at a line runs (a test, a module's tests,
/// `main`), under a profiler.
fn profile_at_caret(s: &mut Session, p: &Value) -> CmdResult {
    let p = if p.is_object() { p.clone() } else { json!({}) };
    let (path, line) = match (opt_path(&p, "path"), opt_usize(&p, "line")?) {
        (Some(path), line) => (profiler::resolve_path(s, &path), line.unwrap_or(0)),
        (None, _) => {
            let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
            let path = d.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved".into()))?;
            (path, d.doc.char_to_line(e.selections.primary().head))
        }
    };
    let params = crate::profiler_agent_cmds::RecordParams::read(&p)?;
    let config = crate::profiler_agent_cmds::context_config_at(s, &path, line)?;
    crate::profiler_agent_cmds::record_config(s, &config, &params)
}

fn select_range(s: &mut Session, p: &Value) -> CmdResult {
    let num = |key: &str| -> Result<Option<f64>, CommandError> {
        match p.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|x| x.is_finite())
                .map(Some)
                .ok_or_else(|| CommandError::BadParams(format!("`{key}` must be a number of milliseconds"))),
        }
    };
    let (start, end) = (num("startMs")?, num("endMs")?);
    let duration = s.profiler.snapshot.as_ref().and_then(|snap| snap.profile.duration_ns).unwrap_or(0);
    let range = match (start, end) {
        (None, None) => None,
        (a, b) => Some((
            a.map_or(0, profiler::ms_to_ns),
            b.map_or(duration, profiler::ms_to_ns),
        )),
    };
    profiler::select_range(s, range)?;
    let snap = snapshot_mut(s)?;
    let r = snap.selection.range;
    Ok(json!({
        "startMs": r.map(|r| ns_ms(r.0)),
        "endMs": r.map(|r| ns_ms(r.1)),
        "durationMs": snap.profile.duration_ns.map(ns_ms),
    }))
}

/// Nanoseconds as milliseconds with one decimal.
pub(crate) fn ns_ms(ns: u64) -> f64 {
    (ns as f64 / 100_000.0).round() / 10.0
}

fn focus(s: &mut Session, p: &Value) -> CmdResult {
    let frame = opt_node(p, "frame")?;
    let name = opt_str(p, "name").map(str::to_lowercase).filter(|n| !n.trim().is_empty());
    let snap = snapshot_mut(s)?;
    let frame = match (frame, name) {
        (Some(f), _) => Some(f),
        (None, Some(n)) => {
            // The matching method with the most total weight.
            let methods = &snap.analysis.methods;
            let best = methods
                .iter()
                .filter(|m| snap.profile.frame(m.frame).is_some_and(|f| f.name.to_lowercase().contains(&n)))
                .max_by_key(|m| m.total)
                .map(|m| m.frame);
            Some(best.ok_or_else(|| CommandError::Failed(format!("no method matches `{n}`")))?)
        }
        (None, None) => None,
    };
    let Some(frame) = frame else {
        snap.zoom = None;
        return ok();
    };
    snap.profile.frame(frame).ok_or_else(|| CommandError::BadParams(format!("no frame {frame}")))?;
    let node =
        snap.heaviest_node(frame).ok_or_else(|| CommandError::Failed("the method has no samples here".into()))?;
    snap.zoom = Some(node);
    snap.selected = Some(node);
    let path = snap.analysis.tree.path(node);
    for n in path {
        snap.expanded.insert(n);
    }
    snap.select_method(frame);
    Ok(json!({ "node": node }))
}

fn attach_profiler(s: &mut Session, p: &Value) -> CmdResult {
    let Some(pid) = opt_usize(p, "pid")? else {
        profiler::open_attach_picker(s, None);
        return ok();
    };
    let pid = u32::try_from(pid).map_err(|_| CommandError::BadParams("`pid` is out of range".into()))?;
    let name = opt_str(p, "name").unwrap_or("").to_string();
    let python = bool_or(p, "python", false);
    let tab = profiler::attach(s, pid, &name, python)?;
    Ok(json!({ "tab": tab.0 }))
}

fn attach_confirm(s: &mut Session, p: &Value) -> CmdResult {
    let index = opt_usize(p, "index")?;
    let Some(picker) = &s.profiler.attach else { return ok() };
    let process = match index {
        Some(i) => picker.visible().get(i).and_then(|i| picker.processes.get(*i)),
        None => picker.selected_process(),
    };
    let Some(process) = process.cloned() else {
        return Err(CommandError::Failed("no process is selected".into()));
    };
    let tab = profiler::attach(s, process.pid, &process.name, process.kind == ProcessKind::Python)?;
    s.profiler.attach = None;
    Ok(json!({ "tab": tab.0 }))
}

fn attach_list(s: &Session) -> Value {
    let Some(a) = &s.profiler.attach else { return Value::Null };
    let items: Vec<_> = a.visible().iter().filter_map(|i| a.processes.get(*i)).take(200).collect();
    json!({ "loading": a.loading, "error": a.error, "selected": a.selected, "processes": items })
}

fn stop(s: &mut Session, p: &Value) -> CmdResult {
    let newest_job = s
        .profiler
        .jobs
        .iter()
        .rev()
        .find(|j| matches!(j.state, profiler::JobState::Building | profiler::JobState::Recording));
    let tab = match opt_usize(p, "tab")? {
        Some(t) => RunTabId(t as u64),
        None => match newest_job.and_then(|j| j.tab).or(s.profiler.recordings.last().map(|r| r.tab)) {
            Some(t) => t,
            None => return Err(CommandError::Disabled("no profiler is recording".into())),
        },
    };
    if !profiler::stop(s, tab) {
        return Err(CommandError::BadParams(format!("Run tab {} is not recording a profile", tab.0)));
    }
    ok()
}

fn tools(s: &Session) -> Value {
    let list: Vec<Value> = ProfilerTool::ALL
        .iter()
        .map(|t| {
            let path = s.profiler.find_tool(*t);
            json!({
                "tool": t, "kind": t.kind(), "path": path, "installed": path.is_some() || t.builtin(),
                "builtin": t.builtin(), "pythonOnly": t.python_only(), "hint": t.install_hint(),
            })
        })
        .collect();
    json!(list)
}

fn install(s: &mut Session, p: &Value) -> CmdResult {
    let name = req_str(p, "tool")?;
    let tool =
        ProfilerTool::from_name(name).ok_or_else(|| CommandError::BadParams(format!("unknown profiler `{name}`")))?;
    let python = profiler::project_python(s);
    let Some((program, args)) = s.profiler.install_command(tool, python.as_deref()) else {
        return Err(CommandError::Failed(tool.install_hint().to_string()));
    };
    let id =
        crate::terminal::open_command(s, &format!("Install {}", tool.program()), &program, &args, s.project_root())?;
    s.profiler.missing = None;
    Ok(json!({ "terminal": id, "command": format!("{program} {}", args.join(" ")) }))
}

fn open_snapshot(s: &mut Session, p: &Value) -> CmdResult {
    match opt_path(p, "path") {
        Some(path) => {
            profiler::open_snapshot(s, &path)?;
            ok()
        }
        None => {
            s.profiler.pick_snapshot = true;
            s.ws.layout.show(ToolWindowId::Profiler);
            ok()
        }
    }
}

// ---------------------------------------------------------------- views

fn status(s: &Session) -> Value {
    let pr = &s.profiler;
    let snapshot = pr.snapshot.as_ref().map(|snap| {
        let p = &snap.profile;
        let threads: Vec<Value> = p
            .threads
            .iter()
            .enumerate()
            .map(|(i, t)| json!({ "index": i, "name": t.name, "total": t.total(), "samples": t.samples }))
            .collect();
        json!({
            "path": snap.path,
            "format": p.format,
            "formatLabel": p.format.label(),
            "unit": p.unit,
            "total": snap.analysis.tree.total,
            "totalText": p.unit.format(snap.analysis.tree.total),
            "samples": p.sample_count(),
            "frames": p.frames.len(),
            "nodes": snap.analysis.tree.nodes.len(),
            "threads": threads,
            "thread": snap.analysis.thread,
            "zoom": snap.zoom,
            "selected": snap.selected,
            "search": snap.search,
            "matched": snap.matched,
            "lines": p.lines.len(),
            "durationMs": p.duration_ns.map(|d| d / 1_000_000),
            "notes": p.notes,
            "baseline": snap.baseline.as_ref().map(|b| json!({ "path": b.path, "total": b.total })),
            "hasSampleTimes": p.has_sample_times(),
            // What the views show now (the analysis) and what was asked for (still analysing
            // when they differ).
            "range": snap.analysis.range.map(|r| json!({ "startMs": ns_ms(r.0), "endMs": ns_ms(r.1) })),
            "selection": json!({
                "thread": snap.selection.thread,
                "range": snap.selection.range.map(|r| json!({ "startMs": ns_ms(r.0), "endMs": ns_ms(r.1) })),
            }),
            "selectedSamples": snap.analysis.samples,
            "memory": p.memory.as_ref().map(|m| json!({
                "peak": m.peak,
                "peakText": p.unit.format(m.peak),
                "peakMs": m.peak_ms,
                "points": m.points.len(),
                "intervalMs": m.interval_ms,
                "peakSnapshot": snap.peak_snapshot(),
            })),
        })
    });
    let jobs: Vec<Value> = pr.jobs.iter().rev().take(5).map(crate::profiler_agent_cmds::job_json).collect();
    let recordings: Vec<Value> = pr
        .recordings
        .iter()
        .map(|r| {
            json!({
                "tab": r.tab.0, "tool": r.tool, "title": r.title, "output": r.output, "attached": r.attached,
                "seconds": r.started.elapsed().as_secs(), "stopping": r.stop_requested.is_some(),
            })
        })
        .collect();
    json!({
        "snapshot": snapshot,
        "tab": pr.tab,
        "loading": pr.loading,
        "error": pr.error,
        "missing": pr.missing.map(|t| json!({ "tool": t, "hint": t.install_hint() })),
        "recordings": recordings,
        "jobs": jobs,
        "baselineLoading": pr.baseline_loading,
    })
}

fn frame_json(snap: &Snapshot, frame: u32) -> Value {
    match snap.profile.frame(frame) {
        Some(f) => json!({ "frame": frame, "name": f.name, "module": f.module, "location": f.location() }),
        None => json!({ "frame": frame }),
    }
}

fn method_edges(snap: &Snapshot, frame: u32) -> Value {
    let tree = &snap.analysis.tree;
    let edge = |e: &codecraft_profile::Edge| {
        let mut v = match e.frame {
            Some(f) => frame_json(snap, f),
            None => json!({ "name": "[root]" }),
        };
        if let Some(o) = v.as_object_mut() {
            o.insert("weight".into(), json!(e.weight));
            o.insert("percent".into(), json!(tree.percent(e.weight)));
        }
        v
    };
    let callers: Vec<Value> = snap.method_edges.0.iter().take(100).map(edge).collect();
    let callees: Vec<Value> = snap.method_edges.1.iter().take(100).map(edge).collect();
    json!({ "method": frame_json(snap, frame), "callers": callers, "callees": callees })
}

fn jump_to_source(s: &mut Session, p: &Value) -> CmdResult {
    let snap =
        s.profiler.snapshot.as_ref().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))?;
    let frame = if let Some(f) = opt_node(p, "frame")? {
        f
    } else if let Some(n) = opt_node(p, "node")?.or(snap.selected) {
        snap.analysis.tree.node(n).ok_or_else(|| CommandError::BadParams(format!("no call node {n}")))?.frame
    } else if let Some(f) = snap.method {
        f
    } else {
        return Err(CommandError::Failed("select a frame first".into()));
    };
    profiler::jump_to_frame(s, frame)?;
    ok()
}

fn call_tree_dump(s: &mut Session, p: &Value) -> CmdResult {
    let snap = snapshot_mut(s)?;
    let max_depth = opt_usize(p, "depth")?.unwrap_or(usize::MAX);
    let min_percent = p.get("minPercent").and_then(Value::as_f64).unwrap_or(0.0);
    let tree = &snap.analysis.tree;
    let mut out = Vec::new();
    let mut work: Vec<u32> = tree.roots.iter().rev().copied().collect();
    while let Some(id) = work.pop() {
        let Some(n) = tree.node(id) else { continue };
        let percent = tree.percent(n.total);
        if percent < min_percent {
            continue;
        }
        let mut v = frame_json(snap, n.frame);
        if let Some(o) = v.as_object_mut() {
            o.insert("id".into(), json!(id));
            o.insert("depth".into(), json!(n.depth));
            o.insert("total".into(), json!(n.total));
            o.insert("self".into(), json!(n.self_weight));
            o.insert("percent".into(), json!(percent));
        }
        out.push(v);
        if out.len() >= MAX_DUMP_NODES {
            break;
        }
        if (n.depth as usize) < max_depth {
            work.extend(n.children.iter().rev().copied());
        }
    }
    Ok(json!(out))
}

fn methods_dump(s: &mut Session, p: &Value) -> CmdResult {
    let snap = snapshot_mut(s)?;
    let limit = opt_usize(p, "limit")?.unwrap_or(200).min(MAX_DUMP_NODES);
    let tree = &snap.analysis.tree;
    let out: Vec<Value> = snap
        .method_order
        .iter()
        .filter_map(|i| snap.analysis.methods.get(*i))
        .take(limit)
        .map(|m| method_json(&snap.profile, tree, m))
        .collect();
    Ok(json!(out))
}

/// One method as `profiler.methods` lists it.
pub(crate) fn method_json(
    profile: &codecraft_profile::Profile,
    tree: &codecraft_profile::CallTree,
    m: &codecraft_profile::MethodStat,
) -> Value {
    let mut v = match profile.frame(m.frame) {
        Some(f) => json!({ "frame": m.frame, "name": f.name, "module": f.module, "location": f.location() }),
        None => json!({ "frame": m.frame }),
    };
    if let Some(o) = v.as_object_mut() {
        o.insert("self".into(), json!(m.self_weight));
        o.insert("total".into(), json!(m.total));
        o.insert("selfPercent".into(), json!(tree.percent(m.self_weight)));
        o.insert("totalPercent".into(), json!(tree.percent(m.total)));
    }
    v
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::runs::ProcessCommand;
    use crate::runs::RunConfig;

    const COLLAPSED: &str = "main;compute;inner 30\nmain;compute 10\nmain;io 5\n";
    const SPEEDSCOPE: &str = r#"{"$schema":"https://www.speedscope.app/file-format-schema.json",
"shared":{"frames":[{"name":"<module>","file":"run.py","line":1},{"name":"work","file":"run.py","line":3}]},
"profiles":[{"type":"sampled","name":"MainThread","unit":"seconds","startValue":0,"endValue":1,
"samples":[[0,1],[0,1],[0]],"weights":[0.01,0.01,0.01]}]}"#;
    const PERF_SCRIPT: &str = "app 7/7 1.0: 1 cycles:\n\t10 hot+0x1 (/bin/app)\n\t20 main+0x2 (/bin/app)\n\n";

    /// A fake profiler: logs its arguments to `<dir>/calls`, and writes `fixture` to the path after
    /// `-o` — at once, or (with `wait_for_int`) when it gets SIGINT.
    fn fake_tool(dir: &Path, name: &str, fixture: &str, wait_for_int: bool) {
        let fixture_path = dir.join(format!("{name}.fixture"));
        std::fs::write(&fixture_path, fixture).unwrap();
        let calls = dir.join("calls");
        // The trap is set before the call is logged, so a test that waits for the log can stop it.
        let (trap, body) = if wait_for_int {
            ("trap 'cat \"$FIXTURE\" > \"$out\"; exit 0' INT\n", "while true; do sleep 0.05; done")
        } else {
            ("", "cat \"$FIXTURE\" > \"$out\"")
        };
        let script = format!(
            "#!/bin/sh\n{trap}FIXTURE='{fixture}'\necho \"{name} $*\" >> '{calls}'\n\
             if [ \"$1\" = script ]; then cat \"$FIXTURE\"; exit 0; fi\n\
             out=''\nprev=''\nfor a in \"$@\"; do if [ \"$a\" = -- ]; then break; fi; if [ \"$prev\" = -o ]; then out=\"$a\"; fi; prev=\"$a\"; done\n{body}\n",
            fixture = fixture_path.display(),
            calls = calls.display(),
        );
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn session(dir: &Path) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        s.profiler.dir_override = Some(dir.join("profiler"));
        s.profiler.tool_dirs = Some(vec![bin]);
        s.profiler.perf_allowed = Some(false);
        s.profiler.xctrace_allowed = Some(false);
        s
    }

    fn pump_until(s: &mut Session, what: impl Fn(&Session) -> bool) {
        let start = Instant::now();
        while !what(s) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out: {:?}", s.profiler.error);
            s.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn loaded(s: &Session) -> bool {
        s.profiler.snapshot.is_some() && s.profiler.loading.is_none() && s.profiler.recordings.is_empty()
    }

    fn calls(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
    }

    #[test]
    fn missing_profiler_gives_an_install_hint() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        s.run_configs = vec![RunConfig::process("app", ProcessCommand::new("/bin/echo", &["hi"], dir.path()))];
        let err = s.execute("run.profile", &Value::Null).unwrap_err().to_string();
        assert!(err.contains("cargo install --locked samply"), "{err}");
        assert_eq!(s.profiler.missing, Some(ProfilerTool::Samply));
        assert!(s.ws.layout.is_visible(ToolWindowId::Profiler));
        s.run_configs = vec![RunConfig::process("py", ProcessCommand::new("python3", &["run.py"], dir.path()))];
        let err = s.execute("run.profileRun.profile", &json!({ "tool": "py-spy" })).unwrap_err().to_string();
        assert!(err.contains("uv tool install py-spy"), "{err}");
        let tools = s.execute("profiler.tools", &Value::Null).unwrap();
        assert_eq!(tools.as_array().unwrap().len(), ProfilerTool::ALL.len());
        assert!(tools.as_array().unwrap().iter().all(|t| t["installed"] == t["builtin"]));
        assert!(s.execute("run.profile", &json!({ "config": 5 })).is_err());
        assert!(s.execute("run.profile", &json!({ "kind": "gpu" })).is_err());
    }

    #[test]
    fn profile_run_records_with_samply_and_opens_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        fake_tool(&dir.path().join("bin"), "samply", COLLAPSED, false);
        s.run_configs = vec![RunConfig::process("my app", ProcessCommand::new("/bin/echo", &["hi"], dir.path()))];
        let r = s.execute("run.profile", &Value::Null).unwrap();
        assert_eq!(r["tool"], "samply");
        pump_until(&mut s, loaded);
        let log = calls(&dir.path().join("bin"));
        assert!(log.contains("samply record --save-only -o "), "{log}");
        assert!(log.contains("-- /bin/echo hi"), "{log}");
        let snap = s.profiler.snapshot.as_ref().unwrap();
        assert_eq!(snap.analysis.tree.total, 45);
        assert!(snap.path.starts_with(dir.path().join("profiler").join("snapshots")));
        assert!(snap.title().starts_with("my-app-"));
        let tab = s.ws.runs.last().unwrap();
        assert_eq!(tab.title, "Profile my app");
        assert!(s.ws.layout.is_visible(ToolWindowId::Profiler));
        // Remembered as recent (and persisted under the profiler dir).
        let recent = s.execute("profiler.recent", &Value::Null).unwrap();
        assert_eq!(recent.as_array().unwrap().len(), 1);
        assert!(dir.path().join("profiler").join("recent.json").is_file());
        s.shutdown();
    }

    #[test]
    fn python_runs_use_py_spy_and_perf_is_the_linux_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bin = dir.path().join("bin");
        fake_tool(&bin, "py-spy", SPEEDSCOPE, false);
        s.run_configs = vec![RunConfig::process("script", ProcessCommand::new("python3", &["run.py"], dir.path()))];
        // py-spy is the default where it doesn't need root (not macOS).
        let p = if cfg!(target_os = "macos") { json!({ "tool": "py-spy" }) } else { Value::Null };
        let r = s.execute("run.profile", &p).unwrap();
        assert_eq!(r["tool"], "pySpy");
        pump_until(&mut s, loaded);
        assert!(calls(&bin).contains("py-spy record --format speedscope -o "));
        let snap = s.profiler.snapshot.as_ref().unwrap();
        assert_eq!(snap.profile.format, codecraft_profile::Format::Speedscope);
        assert_eq!(snap.analysis.tree.total, 30_000_000);

        // No samply: perf where allowed, converted with `perf script`.
        s.profiler.snapshot = None;
        s.profiler.perf_allowed = Some(true);
        fake_tool(&bin, "perf", PERF_SCRIPT, false);
        s.run_configs = vec![RunConfig::process("native", ProcessCommand::new("/bin/echo", &[], dir.path()))];
        let r = s.execute("run.profile", &Value::Null).unwrap();
        assert_eq!(r["tool"], "perf");
        pump_until(&mut s, loaded);
        let log = calls(&bin);
        assert!(log.contains("perf record -g -o "), "{log}");
        assert!(log.contains("perf script -i "), "{log}");
        let snap = s.profiler.snapshot.as_ref().unwrap();
        assert_eq!(snap.profile.format, codecraft_profile::Format::PerfScript);
        assert!(snap.path.to_string_lossy().ends_with(".perf.data.txt"));
        s.shutdown();
    }

    #[test]
    fn attach_records_until_stop() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bin = dir.path().join("bin");
        fake_tool(&bin, "samply", COLLAPSED, true);
        assert!(s.execute("run.attachProfilerToProcess", &json!({ "pid": 1 })).is_err());
        assert!(s.execute("profiler.stop", &Value::Null).is_err());
        let r = s.execute("run.attachProfilerToProcess", &json!({ "pid": 424242, "name": "server" })).unwrap();
        let tab = r["tab"].as_u64().unwrap();
        // Recording until stopped.
        let b = bin.clone();
        pump_until(&mut s, move |_| calls(&b).contains("-p 424242"));
        for _ in 0..20 {
            s.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(s.profiler.recordings.len(), 1);
        let status = s.execute("profiler.status", &Value::Null).unwrap();
        assert_eq!(status["recordings"][0]["attached"], 424242);
        s.execute("profiler.stop", &json!({ "tab": tab })).unwrap();
        pump_until(&mut s, loaded);
        assert_eq!(s.profiler.snapshot.as_ref().unwrap().analysis.tree.total, 45);
        s.shutdown();
    }

    #[test]
    fn a_profiler_that_writes_nothing_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bin = dir.path().join("bin");
        let path = bin.join("samply");
        std::fs::write(&path, "#!/bin/sh\necho 'samply: permission denied' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        s.run_configs = vec![RunConfig::process("app", ProcessCommand::new("/bin/echo", &[], dir.path()))];
        s.execute("run.profile", &Value::Null).unwrap();
        pump_until(&mut s, |s| s.profiler.recordings.is_empty() && s.profiler.error.is_some());
        let err = s.profiler.error.clone().unwrap();
        assert!(err.contains("wrote no snapshot"), "{err}");
        assert!(s.profiler.snapshot.is_none());
        s.shutdown();
    }

    fn project_with_snapshot(dir: &Path) -> (Session, PathBuf) {
        let root = std::fs::canonicalize(dir).unwrap().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.py"), "a = 1\nb = 2\ndef work():\n    pass\n").unwrap();
        let snap = root.join("profile.txt");
        std::fs::write(
            &snap,
            "main (src/lib.py:1);work (src/lib.py:3) 6\nmain (src/lib.py:1);idle 2\nmain (src/lib.py:1) 1\n",
        )
        .unwrap();
        let mut s = session(dir);
        s.execute("file.open", &json!({ "path": root })).unwrap();
        (s, snap)
    }

    #[test]
    fn open_snapshot_and_drive_the_views() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, snap_path) = project_with_snapshot(dir.path());
        // Without a path the UI is asked for one.
        s.execute("run.openProfilerSnapshot", &Value::Null).unwrap();
        assert!(s.profiler.pick_snapshot);
        assert!(s.execute("run.openProfilerSnapshot.open", &json!({ "path": "/no/such/file" })).is_err());
        s.execute("run.openProfilerSnapshot.open", &json!({ "path": "profile.txt" })).unwrap();
        pump_until(&mut s, loaded);
        let status = s.execute("profiler.status", &Value::Null).unwrap();
        assert_eq!(status["snapshot"]["total"], 9);
        assert_eq!(status["snapshot"]["format"], "collapsed");

        let tree = s.execute("profiler.callTree", &json!({ "depth": 1 })).unwrap();
        let names: Vec<&str> = tree.as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["main", "work", "idle"]);
        let work = tree[1]["id"].as_u64().unwrap();
        assert_eq!(tree[1]["percent"].as_f64().unwrap().round(), 67.0);

        s.execute("profiler.zoom", &json!({ "node": work })).unwrap();
        assert_eq!(s.profiler.snapshot.as_ref().unwrap().zoom, Some(work as u32));
        s.execute("profiler.resetZoom", &Value::Null).unwrap();
        assert!(s.execute("profiler.zoom", &json!({ "node": 999 })).is_err());

        let r = s.execute("profiler.search", &json!({ "query": "idl" })).unwrap();
        assert_eq!(r["matched"], 2);

        s.execute("profiler.collapseAll", &Value::Null).unwrap();
        s.execute("profiler.select", &json!({ "node": work })).unwrap();
        assert_eq!(s.profiler.snapshot.as_ref().unwrap().expanded.len(), 1);
        s.execute("profiler.toggleNode", &json!({ "node": work })).unwrap();
        assert_eq!(s.profiler.snapshot.as_ref().unwrap().expanded.len(), 2);
        s.execute("profiler.expandAll", &Value::Null).unwrap();

        let methods = s.execute("profiler.methods", &Value::Null).unwrap();
        assert_eq!(methods[0]["name"], "work");
        s.execute("profiler.sortMethods", &json!({ "column": "total" })).unwrap();
        let methods = s.execute("profiler.methods", &json!({ "limit": 1 })).unwrap();
        assert_eq!(methods.as_array().unwrap().len(), 1);
        assert_eq!(methods[0]["name"], "main");
        let frame = methods[0]["frame"].as_u64().unwrap();
        let edges = s.execute("profiler.selectMethod", &json!({ "frame": frame })).unwrap();
        assert_eq!(edges["callers"][0]["name"], "[root]");
        assert_eq!(edges["callees"].as_array().unwrap().len(), 2);

        // Jump to the selected node's source: src/lib.py line 3.
        s.execute("profiler.jumpToSource", &Value::Null).unwrap();
        let (e, d) = s.ws.active().unwrap();
        assert!(d.path.as_ref().unwrap().ends_with("src/lib.py"));
        let caret = e.selections.primary().head;
        assert_eq!(d.doc.offset_to_position(caret).line, 2);
        let idle = tree[2]["id"].as_u64().unwrap();
        assert!(s.execute("profiler.jumpToSource", &json!({ "node": idle })).is_err());

        s.execute("profiler.selectTab", &json!({ "tab": "methodList" })).unwrap();
        assert_eq!(s.profiler.tab, ProfilerTab::MethodList);
        assert!(s.execute("profiler.selectTab", &json!({ "tab": "nope" })).is_err());
        s.execute("profiler.selectThread", &json!({ "thread": 0 })).unwrap();
        pump_until(&mut s, loaded);
        assert_eq!(s.profiler.snapshot.as_ref().unwrap().analysis.thread, Some(0));
        assert!(s.execute("profiler.selectThread", &json!({ "thread": 3 })).is_err());
        s.execute("profiler.close", &Value::Null).unwrap();
        assert!(s.profiler.snapshot.is_none());
        assert_eq!(s.profiler.recent()[0], std::fs::canonicalize(&snap_path).unwrap_or(snap_path));
        s.execute("profiler.clearRecent", &Value::Null).unwrap();
        assert!(s.profiler.recent().is_empty());
    }

    /// The xctrace fixture with its later samples on a second thread ("Worker").
    fn two_thread_trace() -> String {
        let fixture = include_str!("../../profile/tests/fixtures/xctrace-time-profile.xml");
        let parts: Vec<&str> = fixture.split("<thread ref=\"2\"/>").collect();
        let mut out = String::new();
        for (i, part) in parts.iter().enumerate() {
            out.push_str(part);
            if i + 1 == parts.len() {
                break;
            }
            out.push_str(match i {
                0..=18 => "<thread ref=\"2\"/>",
                19 => "<thread id=\"9000\" fmt=\"Worker  0x99 (hot, pid: 72611)\"><tid id=\"9001\" fmt=\"0x99\">153</tid></thread>",
                _ => "<thread ref=\"9000\"/>",
            });
        }
        out
    }

    fn open_trace(dir: &Path) -> (Session, PathBuf) {
        let (mut s, _) = project_with_snapshot(dir);
        let path = s.project_root().unwrap().join("two.xml");
        std::fs::write(&path, two_thread_trace()).unwrap();
        s.execute("run.openProfilerSnapshot", &json!({ "path": path })).unwrap();
        pump_until(&mut s, loaded);
        (s, path)
    }

    fn reanalysed(s: &Session) -> bool {
        s.profiler.loading.is_none()
            && s.profiler.snapshot.as_ref().is_some_and(|x| x.analysis.selection() == x.selection)
    }

    #[test]
    fn time_ranges_and_threads_filter_every_view() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _) = open_trace(dir.path());
        let status = s.execute("profiler.status", &Value::Null).unwrap();
        assert_eq!(status["snapshot"]["hasSampleTimes"], true);
        assert_eq!(status["snapshot"]["threads"].as_array().unwrap().len(), 2, "{status}");
        let snap = s.profiler.snapshot.as_ref().unwrap();
        let (whole, duration) = (snap.analysis.tree.total, snap.profile.duration_ns.unwrap());
        let hot_c = Path::new("/work/hot/hot.c");
        let whole_lines = snap.lines.for_path(hot_c).unwrap().to_vec();
        // The first half of the run.
        let half = duration as f64 / 2e6;
        let r = s.execute("profiler.selectRange", &json!({ "startMs": 0, "endMs": half })).unwrap();
        assert_eq!(r["startMs"], 0.0);
        pump_until(&mut s, reanalysed);
        let snap = s.profiler.snapshot.as_ref().unwrap();
        let expected = codecraft_profile::Analysis::select(
            &snap.profile,
            &codecraft_profile::Selection { thread: None, range: Some((0, profiler::ms_to_ns(half))) },
        );
        assert_eq!(snap.analysis.tree.total, expected.tree.total);
        assert!(snap.analysis.tree.total < whole && snap.analysis.tree.total > 0);
        // The gutter's numbers are the range's.
        let range_lines = snap.lines.for_path(hot_c).unwrap().to_vec();
        assert_ne!(range_lines, whole_lines);
        assert!(range_lines.iter().map(|h| h.total).max() <= whole_lines.iter().map(|h| h.total).max());
        let status = s.execute("profiler.status", &Value::Null).unwrap();
        assert!(status["snapshot"]["range"]["endMs"].as_f64().unwrap() > 0.0, "{status}");
        // A thread keeps the range: the worker has only later samples, so none in the first half.
        s.execute("profiler.selectThread", &json!({ "thread": 1 })).unwrap();
        pump_until(&mut s, reanalysed);
        let snap = s.profiler.snapshot.as_ref().unwrap();
        assert_eq!(snap.analysis.thread, Some(1));
        assert!(snap.analysis.range.is_some());
        assert_eq!(snap.analysis.tree.total, 0);
        assert!(snap.lines.for_path(hot_c).is_none_or(|l| l.is_empty()));
        // The whole run of the worker: its own lines only (per-thread line statistics).
        s.execute("profiler.selectRange", &Value::Null).unwrap();
        pump_until(&mut s, reanalysed);
        let snap = s.profiler.snapshot.as_ref().unwrap();
        assert!(snap.analysis.range.is_none() && snap.analysis.tree.total > 0);
        let worker_lines: u64 = snap.lines.for_path(hot_c).unwrap().iter().map(|h| h.self_weight).sum();
        let all_lines: u64 = whole_lines.iter().map(|h| h.self_weight).sum();
        assert!(worker_lines > 0 && worker_lines < all_lines, "{worker_lines} of {all_lines}");
        // Bad requests.
        assert!(matches!(s.execute("profiler.selectRange", &json!({ "startMs": "x" })), Err(CommandError::BadParams(_))));
        assert!(s.execute("profiler.selectRange", &json!({ "startMs": f64::MAX, "endMs": -5 })).is_ok());
        pump_until(&mut s, reanalysed);
        s.execute("run.openProfilerSnapshot", &json!({ "path": "profile.txt" })).unwrap();
        pump_until(&mut s, loaded);
        let e = s.execute("profiler.selectRange", &json!({ "startMs": 1, "endMs": 2 })).unwrap_err();
        assert!(e.to_string().contains("no sample times"), "{e}");
        s.shutdown();
    }

    /// QA M14 #25: agents' hotspots followed the Method List's sort in the IDE; now their order
    /// is fixed (and `sort` picks self or total), and the IDE's thread filter doesn't change them.
    #[test]
    fn agent_hotspots_ignore_the_ides_method_sort() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _) = open_trace(dir.path());
        let names = |v: &Value| -> Vec<String> {
            v["hotspots"].as_array().unwrap().iter().map(|h| h["name"].as_str().unwrap().to_string()).collect()
        };
        let before = s.execute("profiler.hotspots", &json!({ "limit": 6 })).unwrap();
        assert_eq!(before["sort"], "self");
        assert!(before["order"].as_str().unwrap().starts_with("selfPercent desc"));
        s.execute("profiler.sortMethods", &json!({ "column": "name", "descending": true })).unwrap();
        assert_eq!(names(&s.execute("profiler.hotspots", &json!({ "limit": 6 })).unwrap()), names(&before));
        s.execute("profiler.sortMethods", &json!({ "column": "total" })).unwrap();
        assert_eq!(names(&s.execute("profiler.hotspots", &json!({ "limit": 6 })).unwrap()), names(&before));
        // The IDE's method list did change.
        let ide = s.execute("profiler.methods", &json!({ "limit": 1 })).unwrap();
        assert_ne!(ide[0]["name"], before["hotspots"][0]["name"]);
        // Sorted by total: non-increasing totals.
        let by_total = s.execute("profiler.hotspots", &json!({ "limit": 6, "sort": "total" })).unwrap();
        let totals: Vec<f64> =
            by_total["hotspots"].as_array().unwrap().iter().map(|h| h["totalPercent"].as_f64().unwrap()).collect();
        assert!(totals.windows(2).all(|w| w[0] >= w[1]), "{totals:?}");
        assert!(s.execute("profiler.hotspots", &json!({ "sort": "name" })).is_err());
        // A thread selected in the IDE: the agent still gets all threads, unless it asks.
        s.execute("profiler.selectThread", &json!({ "thread": 1 })).unwrap();
        pump_until(&mut s, reanalysed);
        let again = s.execute("profiler.hotspots", &json!({ "limit": 6 })).unwrap();
        assert_eq!(names(&again), names(&before));
        assert_eq!(again["total"], before["total"]);
        let worker = s.execute("profiler.hotspots", &json!({ "limit": 6, "thread": "worker" })).unwrap();
        assert!(worker["selection"]["thread"].as_str().unwrap().starts_with("Worker"), "{worker}");
        assert!(worker["total"].as_u64().unwrap() < before["total"].as_u64().unwrap());
        assert!(s.execute("profiler.hotspots", &json!({ "thread": "nope" })).is_err());
        assert!(s.execute("profiler.hotspots", &json!({ "thread": 7 })).is_err());
        // The report: the same fixed order, plus thread and range restrictions.
        let rep = s.execute("profiler.report", &json!({ "top": 6 })).unwrap();
        let rep_names: Vec<&str> = rep["top"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(rep_names, names(&before));
        let rep = s.execute("profiler.report", &json!({ "sort": "total", "thread": 0, "range": { "startMs": 0, "endMs": 10 } })).unwrap();
        assert_eq!(rep["sort"], "total");
        assert_eq!(rep["selection"]["threadIndex"], 0);
        assert_eq!(rep["selection"]["range"]["endMs"], 10.0);
        assert!(rep["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("time range")));
        assert!(s.execute("profiler.report", &json!({ "range": [1, 2] })).is_err());
        assert!(s.execute("profiler.report", &json!({ "range": { "startMs": "a" } })).is_err());
        assert!(s.execute("profiler.report", &json!({ "sort": 5 })).is_ok(), "a non-string sort is ignored");
        let hits = s.execute("profiler.lineHits", &json!({ "path": "/work/hot/hot.c", "thread": 1 })).unwrap();
        assert!(!hits["lines"].as_array().unwrap().is_empty());
        // A snapshot without times can't take a range.
        let e = s
            .execute("profiler.report", &json!({ "snapshot": "profile.txt", "range": { "startMs": 0, "endMs": 1 } }))
            .unwrap();
        if e.get("pending").is_some() {
            pump_until(&mut s, |s| !s.profiler.busy());
        }
        assert!(s.execute("profiler.report", &json!({ "snapshot": "profile.txt", "range": { "startMs": 0 } })).is_err());
        s.shutdown();
    }

    /// Profile Context Configuration (the gutter's Profile): the pytest test at the line, run
    /// under the profiler (a fake py-spy here).
    #[test]
    fn profile_the_test_at_the_caret() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bin = dir.path().join("bin");
        fake_tool(&bin, "py-spy", SPEEDSCOPE, false);
        let root = std::fs::canonicalize(dir.path()).unwrap().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let test = root.join("test_calc.py");
        std::fs::write(&test, "def helper():\n    return 1\n\n\ndef test_slow():\n    assert helper() == 1\n").unwrap();
        std::fs::write(root.join("notes.txt"), "nothing\n").unwrap();
        s.execute("file.open", &json!({ "path": root })).unwrap();
        // 0-based line 5 is in test_slow.
        let r = s.execute("profiler.atCaret", &json!({ "path": test, "line": 5, "tool": "py-spy" })).unwrap();
        assert_eq!(r["config"], "pytest test_slow");
        pump_until(&mut s, |s| !s.profiler.busy());
        let out: Vec<String> = s.ws.runs.iter().flat_map(|r| r.lines.iter().map(|l| l.text.clone())).collect();
        assert!(s.profiler.snapshot.is_some(), "{:?} {out:?} {}", s.profiler.error, calls(&bin));
        let log = calls(&bin);
        assert!(log.contains("-m pytest"), "{log}");
        assert!(log.contains(&format!("{}::test_slow", test.display())), "{log}");
        // The agent form: a 1-based line.
        let r = s.execute("profiler.record", &json!({ "test": { "path": "test_calc.py", "line": 6 }, "tool": "py-spy" })).unwrap();
        assert!(r["command"].as_str().unwrap().contains("test_calc.py::test_slow"), "{r}");
        assert_eq!(r["config"], "pytest test_slow");
        pump_until(&mut s, |s| !s.profiler.busy());
        // From the editor's caret.
        s.execute("file.open", &json!({ "path": test })).unwrap();
        s.execute("profiler.atCaret", &json!({ "tool": "py-spy", "kind": "cpu" })).unwrap();
        pump_until(&mut s, |s| !s.profiler.busy());
        // Nothing to profile, bad lines, bad params.
        let e = s.execute("profiler.atCaret", &json!({ "path": root.join("notes.txt"), "line": 0 })).unwrap_err();
        assert!(e.to_string().contains("nothing to profile"), "{e}");
        assert!(s.execute("profiler.atCaret", &json!({ "path": root.join("none.py") })).is_err());
        assert!(s.execute("profiler.atCaret", &json!({ "path": test, "line": 5, "kind": "gpu" })).is_err());
        assert!(s.execute("profiler.atCaret", &json!({ "path": test, "line": 5, "interval": -1 })).is_err());
        assert!(s.execute("profiler.record", &json!({ "test": { "path": "test_calc.py", "line": 0 } })).is_err());
        assert!(s.execute("profiler.record", &json!({ "test": { "line": 3 } })).is_err());
        assert!(s.execute("profiler.record", &json!({ "test": "x" })).is_err());
        s.shutdown();
    }

    #[test]
    fn bad_snapshots_report_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{\"threads\": [ {").unwrap();
        s.execute("run.openProfilerSnapshot", &json!({ "path": bad })).unwrap();
        pump_until(&mut s, |s| s.profiler.loading.is_none());
        assert!(s.profiler.error.as_deref().unwrap_or("").contains("not a valid profile"));
        assert!(s.profiler.snapshot.is_none());
    }

    /// Every profiler command with hostile params: errors, never panics.
    #[test]
    fn profiler_commands_never_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _) = project_with_snapshot(dir.path());
        let params = [
            Value::Null,
            json!({}),
            json!([1]),
            json!({ "node": u64::MAX, "frame": -1, "thread": u64::MAX, "tab": "x", "pid": u64::MAX, "depth": 0 }),
            json!({ "node": 0, "frame": 0, "thread": 0, "column": "nope", "query": "\u{0}", "limit": u64::MAX }),
            json!({ "path": "", "tool": "rm", "delta": i64::MIN, "index": 7, "minPercent": "x", "config": 1 }),
            json!({ "path": "profile.txt", "column": "self", "descending": "yes", "tab": 99999 }),
        ];
        let ids: Vec<&str> = crate::commands::registry()
            .all()
            .iter()
            .map(|c| c.id)
            .filter(|id| (id.starts_with("profiler.") || id.contains("rofil")) && *id != "profiler.install")
            // These start real profilers (profiler_agent_cmds tests drive them with fakes).
            .filter(|id| !matches!(*id, "profiler.record" | "profiler.profileMemory" | "profiler.atCaret"))
            .collect();
        assert!(ids.len() > 25, "{ids:?}");
        for id in ids {
            for p in &params {
                if let Err(CommandError::Failed(msg)) = s.execute(id, p) {
                    assert!(!msg.starts_with("internal error"), "{id} panicked with {p}: {msg}");
                }
                s.pump();
            }
        }
        pump_until(&mut s, |s| !s.profiler.busy());
        s.shutdown();
    }
}
