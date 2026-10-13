//! Debugger commands: Run › Debug, breakpoints, stepping (Run › Debugging Actions), the Debug
//! tool window's actions, and `debug.*` commands for agents and the CLI.
//!
//! Shortcuts follow RustRover's macOS keymap. The Debugging Actions submenu was not captured in
//! the observed catalog, so its items use `run.debuggingActions.*` ids.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation};
use codecraft_syntax::Language;
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, ok, spec};
use crate::debug::{self, BreakpointProps, DebugTab, DebugTarget, Step};
use crate::params::{bool_or, opt_path, opt_str, opt_usize, req_str, req_usize};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "run.debug",
            "Debug",
            None,
            "{config?: int} — debug the selected (or given) run configuration",
            Context::Global,
            has_project,
            debug_config,
        ),
        spec(
            "run.chooseAndDebug",
            "Debug…",
            None,
            "{config?: int} — choose a run configuration to debug (with `config`, debug it)",
            Context::Global,
            has_project,
            choose_and_debug,
        ),
        spec(
            "debug.atCaret",
            "Debug Context Configuration",
            Some("Ctrl+Shift+D"),
            "{path?: string, line?: int} — debug the Rust test or main, Python file or Zig file at the caret",
            Context::Editor,
            always,
            debug_at_caret,
        ),
        spec(
            "debug.launch",
            "Debug Target",
            None,
            "{target: {kind: cargo|executable|python|zig|zigFile|node, name, …}} — debug a target (see DebugTarget)",
            Context::Global,
            always,
            launch,
        ),
        spec(
            "debug.executable",
            "Debug Executable",
            None,
            "{program: path, args?: [string], cwd?: path, build?: [string]} — debug a native program (C, C++, …), \
             optionally built first by the `build` command",
            Context::Global,
            always,
            debug_executable,
        ),
        spec("debug.stop", "Stop Debugging", None, "", Context::Global, active, |s, _| {
            debug::stop(s);
            ok()
        }),
        spec("debug.rerun", "Rerun Debugging", None, "", Context::Global, has_last_target, rerun),
        // ----- breakpoints
        spec(
            "run.toggleBreakpoint.lineBreakpoint",
            "Toggle Line Breakpoint",
            None,
            "{path?: string, line?: int} — toggle a breakpoint on the caret line (or the given 0-based line)",
            Context::Global,
            has_editor,
            |s, p| toggle(s, p, false),
        ),
        spec(
            "run.toggleBreakpoint.temporaryLineBreakpoint",
            "Toggle Temporary Line Breakpoint",
            None,
            "{path?: string, line?: int} — a breakpoint removed after its first hit",
            Context::Global,
            has_editor,
            |s, p| toggle(s, p, true),
        ),
        spec(
            "run.toggleBreakpoint.toggleBreakpointEnabled",
            "Toggle Breakpoint Enabled",
            None,
            "{path?: string, line?: int}",
            Context::Global,
            has_editor,
            toggle_enabled,
        ),
        spec("run.viewBreakpoints", "View Breakpoints…", None, "", Context::Global, always, |s, _| {
            show_tab(s, DebugTab::Breakpoints);
            ok()
        }),
        spec(
            "debug.setBreakpoint",
            "Set Breakpoint",
            None,
            "{path: string, line: int (0-based), enabled?: bool, condition?: string, logMessage?: string, \
             hitCondition?: string, temporary?: bool} — create or update a breakpoint",
            Context::Global,
            always,
            set_breakpoint,
        ),
        spec(
            "debug.removeBreakpoint",
            "Remove Breakpoint",
            None,
            "{id: int} | {path: string, line: int}",
            Context::Global,
            always,
            remove_breakpoint,
        ),
        spec("debug.removeAllBreakpoints", "Remove All Breakpoints", None, "", Context::Global, always, |s, _| {
            Ok(json!({ "removed": debug::remove_all_breakpoints(s) }))
        }),
        spec(
            "debug.breakOnPanic",
            "Break on Rust Panics",
            None,
            "{enabled?: bool} — stop when a Rust program panics (toggles without `enabled`)",
            Context::Global,
            always,
            break_on_panic,
        ),
        // ----- stepping (Run › Debugging Actions)
        spec("run.debuggingActions.stepOver", "Step Over", Some("F8"), "", Context::Global, stopped, |s, _| {
            step(s, Step::Over)
        }),
        spec("run.debuggingActions.stepInto", "Step Into", Some("F7"), "", Context::Global, stopped, |s, _| {
            step(s, Step::Into)
        }),
        spec(
            "run.debuggingActions.forceStepInto",
            "Force Step Into",
            Some("Alt+Shift+F7"),
            "",
            Context::Global,
            stopped,
            |s, _| step(s, Step::Into),
        ),
        spec("run.debuggingActions.stepOut", "Step Out", Some("Shift+F8"), "", Context::Global, stopped, |s, _| {
            step(s, Step::Out)
        }),
        spec(
            "run.debuggingActions.runToCursor",
            "Run to Cursor",
            Some("Alt+F9"),
            "{path?: string, line?: int} — continue to the caret line (or the given 0-based line)",
            Context::Global,
            stopped,
            run_to_cursor,
        ),
        spec("run.debuggingActions.resumeProgram", "Resume Program", None, "", Context::Global, stopped, |s, _| {
            step(s, Step::Continue)
        }),
        // CodeCraft debugs one program at a time, so the "All Programs" actions act on it.
        spec(
            "run.debuggingActions.pauseAllPrograms",
            "Pause All Programs",
            None,
            "",
            Context::Global,
            running,
            |s, _| {
                debug::pause(s)?;
                ok()
            },
        ),
        spec(
            "run.debuggingActions.resumeAllPrograms",
            "Resume All Programs",
            None,
            "",
            Context::Global,
            stopped,
            |s, _| step(s, Step::Continue),
        ),
        spec(
            "run.debuggingActions.stopAllPrograms",
            "Stop All Programs",
            None,
            "Stops the debug session and every running process",
            Context::Global,
            anything_running,
            |s, _| {
                debug::stop(s);
                crate::runs::stop_all(s);
                ok()
            },
        ),
        // A DAP step is never "forced" past breakpoints and lldb-dap has no step-in targets, so the
        // force/smart variants do the plain step.
        spec("run.debuggingActions.forceStepOver", "Force Step Over", None, "", Context::Global, stopped, |s, _| {
            step(s, Step::Over)
        }),
        spec("run.debuggingActions.smartStepInto", "Smart Step Into", None, "", Context::Global, stopped, |s, _| {
            step(s, Step::Into)
        }),
        spec(
            "run.debuggingActions.forceRunToCursor",
            "Force Run to Cursor",
            None,
            "{path?: string, line?: int} — continue to the caret line (or the given 0-based line)",
            Context::Global,
            stopped,
            run_to_cursor,
        ),
        spec(
            "run.debuggingActions.showExecutionPoint",
            "Show Execution Point",
            None,
            "Opens the file of the selected frame at its line",
            Context::Global,
            stopped,
            show_execution_point,
        ),
        spec(
            "run.debuggingActions.evaluateExpression",
            "Evaluate Expression…",
            Some("Alt+F8"),
            "{expression?: string} — evaluate in the current frame (without one, focus the Evaluate field)",
            Context::Global,
            stopped,
            evaluate_expression,
        ),
        // ----- the Debug tool window
        spec(
            "debug.consoleEvaluate",
            "Evaluate in Debug Console",
            None,
            "{expression: string} — run an expression or debugger command in the debug console",
            Context::Global,
            has_session,
            |s, p| {
                debug::evaluate(s, req_str(p, "expression")?, true)?;
                ok()
            },
        ),
        spec("debug.addWatch", "Add Watch", None, "{expression: string}", Context::Global, always, |s, p| {
            Ok(json!({ "index": debug::add_watch(s, req_str(p, "expression")?)? }))
        }),
        spec("debug.removeWatch", "Remove Watch", None, "{index: int}", Context::Global, always, |s, p| {
            debug::remove_watch(s, req_usize(p, "index")?)?;
            ok()
        }),
        spec("debug.selectThread", "Select Thread", None, "{id: int}", Context::Global, stopped, |s, p| {
            let id = p
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| CommandError::BadParams("missing integer param `id`".into()))?;
            debug::select_thread(s, id)?;
            ok()
        }),
        spec(
            "debug.selectFrame",
            "Select Frame",
            None,
            "{index: int} — make a frame of the selected thread current and show its source",
            Context::Global,
            stopped,
            |s, p| {
                debug::select_frame_cmd(s, req_usize(p, "index")?)?;
                ok()
            },
        ),
        spec(
            "debug.expandVariable",
            "Expand Variable",
            None,
            "{path: string, reference?: int, expanded?: bool} — expand (fetching children) or collapse a node",
            Context::Global,
            always,
            |s, p| {
                let path = req_str(p, "path")?.to_owned();
                let reference = p.get("reference").and_then(Value::as_i64).unwrap_or(0);
                debug::set_expanded(s, &path, reference, bool_or(p, "expanded", true));
                ok()
            },
        ),
        spec(
            "debug.showTab",
            "Show Debug Tab",
            None,
            "{tab: variables|console|breakpoints|memory|disassembly|registers}",
            Context::Global,
            always,
            |s, p| {
                let tab = match req_str(p, "tab")? {
                    "variables" => DebugTab::Variables,
                    "console" => DebugTab::Console,
                    "breakpoints" => DebugTab::Breakpoints,
                    "memory" => DebugTab::Memory,
                    "disassembly" => DebugTab::Disassembly,
                    "registers" => DebugTab::Registers,
                    other => return Err(CommandError::BadParams(format!("unknown tab `{other}`"))),
                };
                crate::debug_views::show_view(s, tab);
                ok()
            },
        ),
        spec("debug.clearConsole", "Clear Debug Console", None, "", Context::Global, always, |s, _| {
            s.debug.console.clear();
            ok()
        }),
        // ----- attach to process
        spec(
            "run.attachToProcess",
            "Attach to Process…",
            None,
            "{pid?: int, kind?: native|rust|python, name?: string} — with `pid`, attach to that process; \
             without, open the process list",
            Context::Global,
            always,
            attach_to_process,
        ),
        spec(
            "debug.attach.refresh",
            "Refresh Process List",
            None,
            "{allUsers?: bool} — list the processes again (all users' with `allUsers`)",
            Context::Global,
            attach_open,
            |s, p| {
                let all = s.debug.attach.as_ref().is_some_and(|a| a.all_users);
                debug::open_attach_picker(s);
                if let Some(a) = &mut s.debug.attach {
                    a.all_users = bool_or(p, "allUsers", all);
                }
                ok()
            },
        ),
        spec(
            "debug.attach.filter",
            "Filter Process List",
            None,
            "{query: string} — show the processes whose name, command line or pid match",
            Context::Global,
            attach_open,
            |s, p| {
                let query = req_str(p, "query")?.to_owned();
                if let Some(a) = &mut s.debug.attach {
                    a.query = query;
                    a.selected = 0;
                }
                Ok(attach_list(s))
            },
        ),
        spec(
            "debug.attach.move",
            "Move in Process List",
            None,
            "{delta: int} — move the selection",
            Context::Global,
            attach_open,
            |s, p| {
                let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
                if let Some(a) = &mut s.debug.attach {
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
            "debug.attach.confirm",
            "Attach to Selected Process",
            None,
            "{index?: int} — attach to the selected (or given visible) process",
            Context::Global,
            attach_open,
            attach_confirm,
        ),
        spec("debug.attach.cancel", "Close Process List", None, "", Context::Global, always, |s, _| {
            s.debug.attach = None;
            ok()
        }),
        spec(
            "debug.state",
            "Debugger State",
            None,
            "→ {active, session, breakpoints, watches, evaluation, console, views}",
            Context::Global,
            always,
            |s, _| Ok(debug::snapshot(s)),
        ),
        // ----- post-mortem and remote debugging
        spec(
            "debug.coreDump",
            "Debug Core Dump…",
            None,
            "{core?: path, program?: path, name?: string} — open a core dump in the debugger (lldb-dap or \
             CodeLLDB); without `core`, show the form in the Debug tool window",
            Context::Global,
            always,
            debug_core_dump,
        ),
        spec(
            "debug.remote",
            "Remote Debug…",
            None,
            "{address?: \"host:port\", program?: path, name?: string} — connect to gdbserver, lldb-server \
             gdbserver or debugserver (`gdb-remote`); `program` is a local copy of the executable for \
             symbols. Without `address`, show the form in the Debug tool window",
            Context::Global,
            always,
            debug_remote,
        ),
    ]
    .into_iter()
    .chain(crate::debug_views::specs())
    .chain(crate::asm::specs())
    .collect()
}

/// Which form the Debug tool window's chooser shows (`debug.coreDump`/`debug.remote` without
/// params).
fn show_form(s: &mut Session, form: debug::DebugForm) -> CmdResult {
    s.debug.form = Some(form);
    s.debug.choosing = true;
    show_tab(s, s.debug.tab);
    ok()
}

fn path_in_project(s: &Session, path: PathBuf) -> PathBuf {
    match (path.is_absolute(), s.project_root()) {
        (false, Some(root)) => crate::normalize_path(&root.join(path)),
        _ => crate::normalize_path(&path),
    }
}

fn debug_core_dump(s: &mut Session, p: &Value) -> CmdResult {
    let Some(core) = opt_path(p, "core") else { return show_form(s, debug::DebugForm::CoreDump) };
    let core = path_in_project(s, core);
    let program = opt_path(p, "program").map(|x| path_in_project(s, x));
    let name = opt_str(p, "name").map(str::to_owned).unwrap_or_else(|| {
        format!("Core {}", core.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    });
    s.debug.form = None;
    debug::start(s, DebugTarget::CoreDump { name: name.clone(), core, program })?;
    started(s, &name)
}

fn debug_remote(s: &mut Session, p: &Value) -> CmdResult {
    let Some(address) = opt_str(p, "address").map(str::to_owned) else {
        return show_form(s, debug::DebugForm::Remote);
    };
    let program = opt_path(p, "program").map(|x| path_in_project(s, x));
    let name = opt_str(p, "name").map_or_else(|| format!("Remote {}", address.trim()), str::to_owned);
    s.debug.form = None;
    debug::start(s, DebugTarget::Remote { name: name.clone(), address, program })?;
    started(s, &name)
}

// ---------------------------------------------------------------- enabled predicates

fn active(s: &Session) -> Result<(), String> {
    if s.debug.is_active() { Ok(()) } else { Err("nothing is being debugged".into()) }
}

fn stopped(s: &Session) -> Result<(), String> {
    if s.debug.is_stopped() { Ok(()) } else { Err("the program is not paused in the debugger".into()) }
}

fn running(s: &Session) -> Result<(), String> {
    if s.debug.is_running() { Ok(()) } else { Err("the program is not running in the debugger".into()) }
}

fn anything_running(s: &Session) -> Result<(), String> {
    if s.debug.is_active() || s.ws.runs.iter().any(|r| r.is_running()) {
        Ok(())
    } else {
        Err("nothing is running".into())
    }
}

fn has_session(s: &Session) -> Result<(), String> {
    if s.debug.is_active() { Ok(()) } else { Err("start debugging first".into()) }
}

fn attach_open(s: &Session) -> Result<(), String> {
    if s.debug.attach.is_some() { Ok(()) } else { Err("the process list is not open".into()) }
}

fn has_last_target(s: &Session) -> Result<(), String> {
    if s.debug.last_target.is_some() { Ok(()) } else { Err("nothing has been debugged yet".into()) }
}

// ---------------------------------------------------------------- helpers

fn show_tab(s: &mut Session, tab: DebugTab) {
    s.debug.tab = tab;
    s.ws.layout.show(ToolWindowId::Debug);
}

fn started(s: &Session, name: &str) -> CmdResult {
    Ok(json!({ "name": name, "status": s.debug.session.as_ref().map(|x| x.status()) }))
}

fn cargo_root(s: &Session) -> Option<PathBuf> {
    let project = s.ws.project.as_ref()?;
    project.cargo.as_ref().map(|m| m.workspace_root.clone())
}

/// The file and zero-based line from params, else the active editor's caret line.
fn location(s: &Session, p: &Value) -> Result<(PathBuf, usize), CommandError> {
    let line = opt_usize(p, "line")?;
    if let Some(path) = opt_path(p, "path") {
        return Ok((crate::normalize_path(&path), line.unwrap_or(0)));
    }
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let path = d.path.clone().ok_or_else(|| CommandError::Failed("save the file first".into()))?;
    let line = line.unwrap_or_else(|| d.doc.char_to_line(e.selections.primary().head));
    Ok((path, line))
}

/// A breakpoint line must exist in the file.
fn check_line(s: &Session, path: &Path, line: usize) -> Result<(), CommandError> {
    let lines = match s.ws.doc_by_path(path).and_then(|d| s.ws.doc(d)) {
        Some(d) => d.doc.len_lines(),
        None => {
            let text = codecraft_fs::read_text_file(path)
                .map_err(|e| CommandError::BadParams(format!("cannot read {}: {e}", path.display())))?;
            text.lines().count().max(1)
        }
    };
    if line >= lines {
        return Err(CommandError::BadParams(format!("{} has no line {}", path.display(), line.saturating_add(1))));
    }
    Ok(())
}

// ---------------------------------------------------------------- starting

fn debug_config(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(i) = opt_usize(p, "config")? {
        if i >= s.run_configs.len() {
            return Err(CommandError::BadParams(format!("no run configuration {i}")));
        }
        s.selected_run_config = i;
    }
    s.popup = None;
    if let Some(name) = s.run_configs.get(s.selected_run_config).map(|c| c.name.clone()) {
        crate::run_config_store::prepare_run(s, &name).map_err(CommandError::Failed)?;
    }
    let target = match s.run_configs.get(s.selected_run_config) {
        Some(c) => match (c.cargo_invocation(), &c.command) {
            (Some(invocation), _) => DebugTarget::Cargo { name: c.name.clone(), invocation: invocation.clone() },
            (None, Some(command)) => process_target(&c.name, command).ok_or_else(|| {
                CommandError::Failed(format!(
                    "{} can't be debugged (it doesn't run a program CodeCraft can debug)",
                    c.name
                ))
            })?,
            (None, None) => return Err(CommandError::Failed(format!("{} has nothing to run", c.name))),
        },
        None => {
            let root = cargo_root(s).ok_or_else(|| CommandError::Failed("no run configuration to debug".into()))?;
            DebugTarget::Cargo { name: "cargo run".into(), invocation: CargoInvocation::new(CargoCommand::Run, root) }
        }
    };
    let name = target.name().to_owned();
    debug::start(s, target)?;
    started(s, &name)
}

fn choose_and_debug(s: &mut Session, p: &Value) -> CmdResult {
    if opt_usize(p, "config")?.is_some() {
        return debug_config(s, p);
    }
    s.debug.choosing = true;
    s.debug.form = None;
    show_tab(s, s.debug.tab);
    ok()
}

fn rerun(s: &mut Session, _: &Value) -> CmdResult {
    let target = s.debug.last_target.clone().ok_or_else(|| CommandError::Disabled("nothing to rerun".into()))?;
    let name = target.name().to_owned();
    debug::start(s, target)?;
    started(s, &name)
}

fn launch(s: &mut Session, p: &Value) -> CmdResult {
    let target = p.get("target").ok_or_else(|| CommandError::BadParams("missing object param `target`".into()))?;
    let target: DebugTarget = serde_json::from_value(target.clone())
        .map_err(|e| CommandError::BadParams(format!("invalid `target`: {e}")))?;
    let name = target.name().to_owned();
    debug::start(s, target)?;
    started(s, &name)
}

fn strings(p: &Value, key: &str) -> Result<Vec<String>, CommandError> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| CommandError::BadParams(format!("`{key}` must be a list of strings"))),
        Some(_) => Err(CommandError::BadParams(format!("`{key}` must be a list of strings"))),
    }
}

fn debug_executable(s: &mut Session, p: &Value) -> CmdResult {
    let program =
        opt_path(p, "program").ok_or_else(|| CommandError::BadParams("missing path param `program`".into()))?;
    let cwd = opt_path(p, "cwd").or_else(|| s.project_root());
    let program = match (program.is_absolute(), &cwd) {
        (false, Some(cwd)) => cwd.join(program),
        _ => program,
    };
    let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "program".into());
    let target = DebugTarget::Executable {
        name: name.clone(),
        program: crate::normalize_path(&program),
        args: strings(p, "args")?,
        cwd,
        env: Vec::new(),
        build: strings(p, "build")?,
    };
    debug::start(s, target)?;
    started(s, &name)
}

fn attach_to_process(s: &mut Session, p: &Value) -> CmdResult {
    let Some(pid) = p.get("pid") else {
        debug::open_attach_picker(s);
        return ok();
    };
    let pid = pid
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| CommandError::BadParams("`pid` must be a process id".into()))?;
    let process = match opt_str(p, "kind") {
        None => debug::ProcessKind::Native,
        Some(k) => serde_json::from_value(json!(k))
            .map_err(|_| CommandError::BadParams(format!("unknown kind `{k}` (native, rust or python)")))?,
    };
    let name = opt_str(p, "name").map_or_else(|| format!("Attach to {pid}"), str::to_owned);
    attach(s, pid, process, name)
}

fn attach(s: &mut Session, pid: u32, process: debug::ProcessKind, name: String) -> CmdResult {
    s.debug.attach = None;
    debug::start(s, DebugTarget::Attach { name: name.clone(), pid, process })?;
    started(s, &name)
}

fn attach_confirm(s: &mut Session, p: &Value) -> CmdResult {
    let index = opt_usize(p, "index")?;
    let Some(picker) = &s.debug.attach else { return ok() };
    let process = match index {
        Some(i) => picker.visible().get(i).and_then(|i| picker.processes.get(*i)),
        None => picker.selected_process(),
    };
    let Some(process) = process.cloned() else {
        return Err(CommandError::Failed("no process is selected".into()));
    };
    attach(s, process.pid, process.kind, format!("{} ({})", process.name, process.pid))
}

/// The visible processes (for agents driving the list).
fn attach_list(s: &Session) -> Value {
    let Some(a) = &s.debug.attach else { return Value::Null };
    let items: Vec<&debug::ProcessInfo> = a.visible().iter().filter_map(|i| a.processes.get(*i)).take(200).collect();
    json!({ "loading": a.loading, "error": a.error, "selected": a.selected, "processes": items })
}

/// What to debug for a file and line: Rust tests and `main` (via the run context), Python files,
/// Zig files (with `zig build` when the project has a build.zig), JavaScript under js-debug.
fn context_target(s: &Session, path: &Path, line: usize) -> Option<DebugTarget> {
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    match Language::from_path(path) {
        Language::Rust => {
            let (name, invocation) = crate::run_cmds::context_invocation(s, path, line)?;
            let name = name.replacen("Run ", "", 1);
            Some(DebugTarget::Cargo { name, invocation })
        }
        Language::Python => {
            let cwd = root.clone().or_else(|| path.parent().map(Path::to_path_buf));
            let python = root.as_deref().and_then(project_python);
            Some(DebugTarget::Python {
                name: file_name,
                program: Some(path.to_path_buf()),
                module: None,
                args: Vec::new(),
                cwd,
                env: Vec::new(),
                python,
                just_my_code: true,
            })
        }
        Language::Zig => {
            let build_root = path
                .ancestors()
                .skip(1)
                .take_while(|dir| root.as_ref().is_none_or(|r| dir.starts_with(r)))
                .find(|dir| dir.join("build.zig").is_file());
            match build_root {
                Some(dir) => Some(DebugTarget::Zig {
                    name: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(file_name),
                    root: dir.to_path_buf(),
                    exe: None,
                    args: Vec::new(),
                }),
                None => Some(DebugTarget::ZigFile { name: file_name, path: path.to_path_buf(), args: Vec::new() }),
            }
        }
        Language::JavaScript => Some(DebugTarget::Node {
            name: file_name,
            program: path.to_path_buf(),
            args: Vec::new(),
            cwd: root.or_else(|| path.parent().map(Path::to_path_buf)),
        }),
        _ => None,
    }
}

/// What to debug for a plain process run configuration: `zig build …`, `python script.py` /
/// `python -m module`, `pytest`, `node file.js`, a program path, or a `sh -c "build && ./program"`
/// chain (the steps before the program become the build). `None` if nothing debuggable runs.
fn process_target(name: &str, cmd: &crate::runs::ProcessCommand) -> Option<DebugTarget> {
    let base = Path::new(&cmd.program).file_name()?.to_string_lossy().into_owned();
    let path_like = |p: &str| p.contains('/');
    let in_cwd = |p: &str| crate::normalize_path(&cmd.cwd.join(p));
    let name = name.to_owned();
    match base.as_str() {
        "zig" if cmd.args.first().is_some_and(|a| a == "build") => {
            let args = cmd.args.iter().skip_while(|a| *a != "--").skip(1).cloned().collect();
            Some(DebugTarget::Zig { name, root: cmd.cwd.clone(), exe: None, args })
        }
        b if b.starts_with("python") || b == "pytest" => {
            let python = path_like(&cmd.program).then(|| in_cwd(&cmd.program)).filter(|_| b != "pytest");
            let (program, module, args) = if b == "pytest" {
                (None, Some("pytest".to_owned()), cmd.args.clone())
            } else {
                match cmd.args.split_first() {
                    Some((flag, rest)) if flag == "-m" => {
                        let (module, args) = rest.split_first()?;
                        (None, Some(module.clone()), args.to_vec())
                    }
                    Some((script, rest)) if !script.starts_with('-') => (Some(in_cwd(script)), None, rest.to_vec()),
                    _ => return None,
                }
            };
            Some(DebugTarget::Python {
                name,
                program,
                module,
                args,
                cwd: Some(cmd.cwd.clone()),
                env: cmd.env.clone(),
                python,
                just_my_code: true,
            })
        }
        "node" => {
            let (program, args) = cmd.args.split_first()?;
            Some(DebugTarget::Node { name, program: in_cwd(program), args: args.to_vec(), cwd: Some(cmd.cwd.clone()) })
        }
        "sh" | "bash" | "zsh" if cmd.args.first().is_some_and(|a| a == "-c") => {
            let script = cmd.args.get(1)?;
            let steps: Vec<&str> = script.split("&&").map(str::trim).collect();
            let (last, before) = steps.split_last()?;
            let mut words = last.split_whitespace();
            let program = words.next().filter(|p| path_like(p))?;
            let build =
                if before.is_empty() { Vec::new() } else { vec![base.clone(), "-c".to_owned(), before.join(" && ")] };
            Some(DebugTarget::Executable {
                name,
                program: in_cwd(program),
                args: words.map(str::to_owned).collect(),
                cwd: Some(cmd.cwd.clone()),
                env: cmd.env.clone(),
                build,
            })
        }
        _ if path_like(&cmd.program) => Some(DebugTarget::Executable {
            name,
            program: in_cwd(&cmd.program),
            args: cmd.args.clone(),
            cwd: Some(cmd.cwd.clone()),
            env: cmd.env.clone(),
            build: Vec::new(),
        }),
        _ => None,
    }
}

/// The project's interpreter: the selected one (Python status bar widget), else its virtualenv.
fn project_python(root: &Path) -> Option<PathBuf> {
    crate::python_env::project_python(root)
}

fn debug_at_caret(s: &mut Session, p: &Value) -> CmdResult {
    let (path, line) = location(s, p)?;
    let Some(target) = context_target(s, &path, line) else {
        s.status = "Nothing to debug here".into();
        return Ok(Value::Null);
    };
    let name = target.name().to_owned();
    debug::start(s, target)?;
    started(s, &name)
}

// ---------------------------------------------------------------- breakpoints

fn toggle(s: &mut Session, p: &Value, temporary: bool) -> CmdResult {
    let (path, line) = location(s, p)?;
    check_line(s, &path, line)?;
    let present = debug::toggle_breakpoint(s, &path, line, temporary);
    Ok(json!({ "path": path, "line": line, "breakpoint": present }))
}

fn toggle_enabled(s: &mut Session, p: &Value) -> CmdResult {
    let (path, line) = location(s, p)?;
    let Some(enabled) = s.debug.breakpoint_at(&path, line).map(|b| b.enabled) else {
        s.status = "No breakpoint on this line".into();
        return Ok(Value::Null);
    };
    debug::set_breakpoint(s, &path, line, BreakpointProps { enabled: Some(!enabled), ..Default::default() });
    Ok(json!({ "enabled": !enabled }))
}

/// `Some(None)` clears a property (`null` or `""`), `None` leaves it.
fn opt_text(p: &Value, key: &str) -> Result<Option<Option<String>>, CommandError> {
    match p.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(text)) => Ok(Some(Some(text.clone()).filter(|t| !t.is_empty()))),
        Some(_) => Err(CommandError::BadParams(format!("`{key}` must be a string"))),
    }
}

fn set_breakpoint(s: &mut Session, p: &Value) -> CmdResult {
    let path = crate::normalize_path(
        &opt_path(p, "path").ok_or_else(|| CommandError::BadParams("missing path param `path`".into()))?,
    );
    let line = req_usize(p, "line")?;
    check_line(s, &path, line)?;
    let props = BreakpointProps {
        enabled: p.get("enabled").and_then(Value::as_bool),
        condition: opt_text(p, "condition")?,
        log_message: opt_text(p, "logMessage")?,
        hit_condition: opt_text(p, "hitCondition")?,
        temporary: p.get("temporary").and_then(Value::as_bool),
    };
    let id = debug::set_breakpoint(s, &path, line, props);
    Ok(json!({ "id": id }))
}

fn remove_breakpoint(s: &mut Session, p: &Value) -> CmdResult {
    let id = match p.get("id").and_then(Value::as_u64) {
        Some(id) => Some(id),
        None => {
            let path = crate::normalize_path(
                &opt_path(p, "path")
                    .ok_or_else(|| CommandError::BadParams("give `id`, or `path` and `line`".into()))?,
            );
            let line = req_usize(p, "line")?;
            s.debug.breakpoint_at(&path, line).map(|b| b.id)
        }
    };
    let removed = id.is_some_and(|id| debug::remove_breakpoint_by_id(s, id));
    Ok(json!({ "removed": removed }))
}

fn break_on_panic(s: &mut Session, p: &Value) -> CmdResult {
    s.debug.break_on_panic = p.get("enabled").and_then(Value::as_bool).unwrap_or(!s.debug.break_on_panic);
    debug::save(s);
    Ok(json!({ "enabled": s.debug.break_on_panic }))
}

// ---------------------------------------------------------------- stepping

fn step(s: &mut Session, kind: Step) -> CmdResult {
    debug::step(s, kind)?;
    ok()
}

fn run_to_cursor(s: &mut Session, p: &Value) -> CmdResult {
    let (path, line) = location(s, p)?;
    check_line(s, &path, line)?;
    debug::run_to_cursor(s, path, line)?;
    ok()
}

fn show_execution_point(s: &mut Session, _: &Value) -> CmdResult {
    let Some((path, line)) = s.debug.session.as_ref().and_then(|x| x.execution.clone()) else {
        s.status = "No execution point".into();
        return ok();
    };
    s.navigate_to_position(&path, line, 0)?;
    Ok(json!({ "path": path, "line": line }))
}

fn evaluate_expression(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(expression) = opt_str(p, "expression") {
        debug::evaluate(s, expression, false)?;
        return ok();
    }
    // Prefill with the selection, or the word at the caret.
    let prefill =
        s.ws.active()
            .map(|(e, d)| {
                let sel = e.selections.primary();
                if sel.is_empty() {
                    d.doc.slice(codecraft_text::movement::word_range_at(&d.doc, sel.head))
                } else {
                    d.doc.slice(sel.start()..sel.end())
                }
            })
            .filter(|t| !t.contains('\n'))
            .unwrap_or_default();
    s.debug.focus_evaluate = Some(prefill.trim().to_owned());
    show_tab(s, DebugTab::Variables);
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debugging_actions_have_rustrover_shortcuts() {
        let reg = crate::registry();
        for (id, key) in [
            ("run.debuggingActions.stepOver", "F8"),
            ("run.debuggingActions.stepInto", "F7"),
            ("run.debuggingActions.forceStepInto", "Alt+Shift+F7"),
            ("run.debuggingActions.stepOut", "Shift+F8"),
            ("run.debuggingActions.runToCursor", "Alt+F9"),
            ("run.debuggingActions.resumeProgram", "Alt+Cmd+R"),
            ("run.debuggingActions.evaluateExpression", "Alt+F8"),
            ("run.debug", "Ctrl+D"),
            ("run.chooseAndDebug", "Ctrl+Alt+D"),
            ("run.toggleBreakpoint.lineBreakpoint", "Cmd+F8"),
            ("run.toggleBreakpoint.temporaryLineBreakpoint", "Alt+Shift+Cmd+F8"),
            ("run.viewBreakpoints", "Shift+Cmd+F8"),
        ] {
            assert_eq!(reg.shortcuts(id).first().copied(), Some(key), "{id}");
        }
        // Catalog ids light up the observed menu items.
        for id in ["run.debug", "run.chooseAndDebug", "run.toggleBreakpoint.lineBreakpoint", "run.viewBreakpoints"] {
            assert!(reg.catalog_item(id).is_some(), "{id}");
        }
    }

    #[test]
    fn process_configurations_map_to_debug_targets() {
        use crate::runs::ProcessCommand;
        let zig = process_target("zig", &ProcessCommand::new("zig", &["build", "run", "--", "x"], "/p")).unwrap();
        assert!(
            matches!(zig, DebugTarget::Zig { ref root, ref args, .. } if root == Path::new("/p") && args == &["x"])
        );
        let py = process_target("py", &ProcessCommand::new("python3", &["app.py", "-v"], "/p")).unwrap();
        assert!(matches!(py, DebugTarget::Python { ref program, ref args, python: None, .. }
                if program.as_deref() == Some(Path::new("/p/app.py")) && args == &["-v"]));
        let module = process_target("m", &ProcessCommand::new(".venv/bin/python", &["-m", "pkg", "a"], "/p")).unwrap();
        assert!(matches!(module, DebugTarget::Python { ref module, ref python, .. }
                if module.as_deref() == Some("pkg") && python.as_deref() == Some(Path::new("/p/.venv/bin/python"))));
        let pytest = process_target("t", &ProcessCommand::new("pytest", &["-k", "x"], "/p")).unwrap();
        assert!(matches!(pytest, DebugTarget::Python { ref module, .. } if module.as_deref() == Some("pytest")));
        let chain = process_target(
            "cmake",
            &ProcessCommand::shell("cmake -S . -B build && cmake --build build && ./build/app --fast", "/p"),
        )
        .unwrap();
        assert!(matches!(chain, DebugTarget::Executable { ref program, ref args, ref build, .. }
                if program == Path::new("/p/build/app") && args == &["--fast"]
                    && build == &["sh", "-c", "cmake -S . -B build && cmake --build build"]));
        let exe = process_target("exe", &ProcessCommand::new("./zig-out/bin/app", &[], "/p")).unwrap();
        assert!(matches!(exe, DebugTarget::Executable { ref program, ref build, .. }
            if program == Path::new("/p/zig-out/bin/app") && build.is_empty()));
        assert!(process_target("npm", &ProcessCommand::new("npm", &["start"], "/p")).is_none());
        assert!(process_target("sh", &ProcessCommand::shell("make", "/p")).is_none());
        assert!(process_target("py", &ProcessCommand::new("python3", &[], "/p")).is_none());
    }

    #[test]
    fn project_python_prefers_a_virtualenv() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(project_python(tmp.path()).is_none());
        std::fs::create_dir_all(tmp.path().join(".venv/bin")).unwrap();
        std::fs::write(tmp.path().join(".venv/bin/python"), "").unwrap();
        assert_eq!(project_python(tmp.path()), Some(tmp.path().join(".venv/bin/python")));
    }
}
