//! Debugger commands for agents and the M14 debugging depth (track DBG): a stop summary and
//! non-blocking waits (`debug.waitForStop`, `debug.poll`), breakpoints with 1-based lines and
//! compact JSON, stepping that hands back a wait token, evaluation with one level of children,
//! Set Value, Jump to Line, Smart Step Into targets, debug console completion, hover
//! evaluation, exception breakpoints, the breakpoint properties popup, Python remote attach and
//! debugging a test from the test tree.
//!
//! Lines: commands whose params say "1-based" are the agent API (`debug.breakpoint.*`,
//! `debug.runToLine`, `debug.start`); editor actions (`debug.jumpToLine`, `debug.editBreakpoint`)
//! take 0-based lines like the other editor commands. Results always give 1-based lines.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::debug::{self, BreakpointProps, DebugTarget, Step, agent};
use crate::params::{bool_or, opt_path, opt_str, opt_usize, req_str, req_usize};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------------------ state and waiting
        spec(
            "debug.stopSummary",
            "Debugger Stop Summary",
            None,
            "{frames?: int (10), locals?: int (40)} → {state: idle|building|starting|running|stopped|exited|failed, \
             reason, description, thread {id, name}, frames [{index, function, path, line (1-based), code, library?}], \
             source {path, line, lines [{line, text, current?}]}, locals [{name, value, type, reference?}], returnValue?, \
             exception?, breakpoints? (hit), output (last lines), exitCode?, lastError?, settling}",
            Context::Global,
            always,
            |s, p| {
                let frames = opt_usize(p, "frames")?.unwrap_or(10).min(200);
                let locals = opt_usize(p, "locals")?.unwrap_or(40).min(2000);
                Ok(agent::stop_summary(s, frames, locals))
            },
        ),
        spec(
            "debug.waitForStop",
            "Wait for the Debugger to Stop",
            None,
            "{timeoutMs?: int (30000, max 600000), session?: id|name} | {token: int} — the stop summary when the \
             program is paused (stack and locals read) or the session is over; otherwise {pending: true, token}: call \
             again with the token (or debug.poll) after the IDE has pumped events → summary | {pending: true} | \
             {timedOut: true}. With several sessions and no `session`: the first session that stops (or the end of \
             all); with `session`: that one or one of its subprocess sessions. The summary's `sessionId` says which \
             stopped and `sessions` lists them all",
            Context::Global,
            always,
            |s, p| match token_param(p)? {
                Some(token) => agent::poll(s, token),
                None if p.get("session").is_some_and(|v| !v.is_null()) => {
                    Ok(agent::wait_for_stop(s, agent::timeout_param(p)?))
                }
                None => Ok(agent::wait_for_any(s, agent::timeout_param(p)?)),
            },
        ),
        spec(
            "debug.poll",
            "Poll a Debugger Request",
            None,
            "{token: int} — the answer to an earlier debug.* command that returned {pending: true, token}",
            Context::Global,
            always,
            |s, p| {
                let token = token_param(p)?.ok_or_else(|| CommandError::BadParams("missing integer param `token`".into()))?;
                agent::poll(s, token)
            },
        ),
        // ------------------------------------------------------------ starting
        spec(
            "debug.start",
            "Start Debugging",
            None,
            "{configuration?: string (a run configuration's name) | target?: {kind, …} (see debug.launch) | path?: \
             string, line?: int (1-based: the test or main at that line), timeoutMs?} — start a debug session \
             → {name, session (its id), status, token} (token: wait for the first stop with debug.waitForStop). \
             Other sessions keep running; debugging a configuration that is being debugged restarts it",
            Context::Global,
            always,
            start,
        ),
        spec(
            "debug.test",
            "Debug Test",
            None,
            "{key?: string (a test.tree row; default: the selected test), tab?: int} — debug one test of a test run \
             (Rust, Python/pytest, C/C++) → {name, status}",
            Context::Global,
            always,
            debug_test,
        ),
        spec(
            "debug.pythonRemote",
            "Attach to Python Debug Server…",
            None,
            "{address: \"host:port\" | host: string, port: int, pathMappings?: [{localRoot, remoteRoot}] | \
             {\"/local\": \"/remote\"}, justMyCode?: bool, name?: string} — attach to a program started with \
             `python -m debugpy --listen host:port` (here or on another machine)",
            Context::Global,
            always,
            python_remote,
        ),
        // ------------------------------------------------------------ breakpoints (1-based)
        spec(
            "debug.breakpoint.set",
            "Set Breakpoint (1-based)",
            None,
            "{path: string, line: int (1-based), condition?: string, hitCondition?: string, logMessage?: string \
             (a logpoint: printed, `{expr}` interpolated, no stop), temporary?: bool (removed once hit), enabled?: \
             bool} — create or update → {id, path, line, enabled, condition?, …, verified?}",
            Context::Global,
            always,
            breakpoint_set,
        ),
        spec(
            "debug.breakpoint.remove",
            "Remove Breakpoint (1-based)",
            None,
            "{id: int} | {path: string, line: int (1-based)} → {removed}",
            Context::Global,
            always,
            breakpoint_remove,
        ),
        spec(
            "debug.breakpoint.list",
            "List Breakpoints",
            None,
            "→ {breakpoints: [{id, path, line (1-based), enabled, condition?, hitCondition?, logMessage?, temporary?, \
             verified?, message?}], exceptions: {rustPanic, cppThrow, cppCatch, pythonRaised, pythonUncaught, \
             pythonUserUncaught}}",
            Context::Global,
            always,
            |s, _| Ok(breakpoint_list(s)),
        ),
        spec(
            "debug.exceptionBreakpoints",
            "Exception Breakpoints",
            None,
            "{rustPanic?: bool, cppThrow?: bool, cppCatch?: bool, pythonRaised?: bool, pythonUncaught?: bool, \
             pythonUserUncaught?: bool} — which exceptions stop the program (saved with the project's breakpoints; a \
             running session gets them at once) → the settings and the adapter's filters",
            Context::Global,
            always,
            debug::exceptions::update,
        ),
        spec(
            "debug.editBreakpoint",
            "Edit Breakpoint…",
            None,
            "{id?: int, path?: string, line?: int (0-based; the caret line by default), create?: bool} — open the \
             breakpoint properties popup (enabled, condition, hit count, log message, temporary)",
            Context::Global,
            always,
            edit_breakpoint,
        ),
        spec("debug.closeBreakpointPopup", "Close Breakpoint Properties", None, "", Context::Global, always, |s, _| {
            s.debug.breakpoint_popup = None;
            ok()
        }),
        // ------------------------------------------------------------ stepping and evaluation
        spec(
            "debug.step",
            "Debugger Step",
            None,
            "{action: over|into|out|continue|pause|smartInto, timeoutMs?} — step, continue or pause → {status, \
             token} (wait for the next stop with debug.waitForStop {token})",
            Context::Global,
            always,
            step,
        ),
        spec(
            "debug.runToLine",
            "Run to Line",
            None,
            "{path: string, line: int (1-based), timeoutMs?} — continue until that line → {token}",
            Context::Global,
            always,
            run_to_line,
        ),
        spec(
            "debug.evaluate",
            "Evaluate (Agent)",
            None,
            "{expression: string, frame?: int (frame index; the selected one by default), context?: watch|repl|hover, \
             timeoutMs?} → {pending: true, token}; debug.poll gives {value, type?, reference?, children?: [one level]} \
             or {error}",
            Context::Global,
            always,
            evaluate,
        ),
        spec(
            "debug.variables",
            "Variable Children",
            None,
            "{reference: int, timeoutMs?} → {pending: true, token}; debug.poll gives {reference, children: [{name, \
             value, type?, reference?}]}",
            Context::Global,
            always,
            |s, p| {
                let reference = p
                    .get("reference")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| CommandError::BadParams("missing integer param `reference`".into()))?;
                let token = agent::answer_token(s, agent::timeout_param(p)?);
                debug::actions::agent_children(s, reference, token)?;
                Ok(pending(token))
            },
        ),
        spec(
            "debug.setValue",
            "Set Value…",
            None,
            "{name: string, value: string, reference?: int (the parent's variables reference; default: the scope \
             that has `name`)} | {expression: string, value: string} — change a variable while paused → {pending, \
             token}; debug.poll gives the new {name, value, type}",
            Context::Global,
            stopped,
            set_value,
        ),
        spec(
            "debug.jumpToLine",
            "Jump to Line",
            None,
            "{path?: string, line?: int (0-based; the caret line by default)} — set the next statement: the paused \
             thread continues from that line without running the code in between (DAP goto, else LLDB `thread jump`)",
            Context::Global,
            stopped,
            jump_to_line,
        ),
        spec(
            "debug.stepIntoTarget",
            "Step Into Target",
            None,
            "{index: int} — step into one of Smart Step Into's targets (debug.state stepTargets)",
            Context::Global,
            stopped,
            |s, p| {
                debug::actions::step_into_target(s, req_usize(p, "index")?, None)?;
                Ok(json!({ "status": "running" }))
            },
        ),
        spec("debug.cancelStepIntoTargets", "Close Step Into Targets", None, "", Context::Global, always, |s, _| {
            s.debug.step_targets = None;
            ok()
        }),
        spec(
            "debug.consoleComplete",
            "Complete in Debug Console",
            None,
            "{text: string, column?: int (chars; the end by default)} — ask the debugger for completions (shown in \
             the console) → {pending: true, token}; debug.poll gives {items: [{label, text, start, end, kind}]}",
            Context::Global,
            has_session,
            |s, p| {
                let text = req_str(p, "text")?.to_owned();
                let column = opt_usize(p, "column")?.unwrap_or_else(|| text.chars().count());
                let token = agent::answer_token(s, agent::timeout_param(p)?);
                debug::actions::console_complete(s, &text, column, Some(token))?;
                Ok(pending(token))
            },
        ),
        spec("debug.closeCompletions", "Close Debug Console Completions", None, "", Context::Global, always, |s, _| {
            s.debug.completions = None;
            ok()
        }),
        spec(
            "debug.hoverEvaluate",
            "Evaluate Under Mouse",
            None,
            "{expression: string} — evaluate the expression under the mouse while paused (debug.state hover)",
            Context::Global,
            stopped,
            |s, p| {
                debug::actions::hover(s, req_str(p, "expression")?)?;
                ok()
            },
        ),
    ]
}

fn stopped(s: &Session) -> Result<(), String> {
    if s.debug.is_stopped() { Ok(()) } else { Err("the program is not paused in the debugger".into()) }
}

fn has_session(s: &Session) -> Result<(), String> {
    if s.debug.is_active() { Ok(()) } else { Err("start debugging first".into()) }
}

fn pending(token: u64) -> Value {
    json!({ "pending": true, "token": token })
}

fn token_param(p: &Value) -> Result<Option<u64>, CommandError> {
    match p.get("token") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| CommandError::BadParams("`token` must be a positive integer".into())),
    }
}

/// A 1-based `line` param as a 0-based line.
fn line_1(p: &Value) -> Result<usize, CommandError> {
    let line = req_usize(p, "line")?;
    line.checked_sub(1).ok_or_else(|| CommandError::BadParams("`line` is 1-based (the first line is 1)".into()))
}

fn path_param(s: &Session, p: &Value) -> Result<PathBuf, CommandError> {
    let path = opt_path(p, "path").ok_or_else(|| CommandError::BadParams("missing path param `path`".into()))?;
    Ok(match (path.is_absolute(), s.project_root()) {
        (false, Some(root)) => crate::normalize_path(&root.join(path)),
        _ => crate::normalize_path(&path),
    })
}

fn status_name(s: &Session) -> Value {
    json!(s.debug.session.as_ref().map(|x| x.status()))
}

// ---------------------------------------------------------------- starting

fn start(s: &mut Session, p: &Value) -> CmdResult {
    let timeout = agent::timeout_param(p)?;
    let started = if let Some(name) = opt_str(p, "configuration") {
        let index = s
            .run_configs
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| CommandError::BadParams(format!("no run configuration named `{name}`")))?;
        crate::debug_cmds::debug_config(s, &json!({ "config": index }))?
    } else if p.get("target").is_some() {
        s.execute("debug.launch", &json!({ "target": p.get("target") }))?
    } else if p.get("path").is_some() {
        let path = path_param(s, p)?;
        let line = match p.get("line") {
            Some(_) => line_1(p)?,
            None => 0,
        };
        crate::debug_cmds::debug_location(s, &path, line)?
    } else {
        crate::debug_cmds::debug_config(s, &json!({}))?
    };
    if started.is_null() {
        return Err(CommandError::Failed("nothing to debug there".into()));
    }
    let token = agent::wait_for_stop(s, timeout);
    let mut out = json!({
        "name": s.debug.session.as_ref().map(|x| x.name.clone()),
        "session": s.debug.selected_id(),
        "status": status_name(s),
    });
    if let Some(map) = out.as_object_mut() {
        if let Some(t) = token.get("token") {
            map.insert("token".into(), t.clone());
        }
        if started.get("beforeLaunch").is_some() || started.get("tasks").is_some() {
            map.insert("beforeLaunch".into(), started);
        }
    }
    Ok(out)
}

fn debug_test(s: &mut Session, p: &Value) -> CmdResult {
    use crate::testing::{Framework, TestOrigin};
    let tab = crate::testing::target_run(s, opt_usize(p, "tab")?.map(|t| t as u64))?;
    let run = s.testing.run(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    let key = match opt_str(p, "key") {
        Some(k) => k.to_owned(),
        None => run.view.selected.clone().ok_or_else(|| CommandError::BadParams("no test is selected".into()))?,
    };
    let (si, ti) = crate::testing::test_key(&key).ok_or_else(|| CommandError::BadParams(format!("`{key}` is not a test")))?;
    let suite = run.suites.get(si).ok_or_else(|| CommandError::BadParams("no such test".into()))?;
    let test = suite.tests.get(ti).ok_or_else(|| CommandError::BadParams("no such test".into()))?;
    let name = test.name.clone();
    let location = test.location.clone();
    let file = suite.source.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| suite.name.clone());
    let (framework, origin, cwd) = (run.framework, run.origin.clone(), run.cwd.clone());
    // From the test's source location, like the gutter's Debug.
    if let Some(loc) = location {
        let v = crate::debug_cmds::debug_location(s, &loc.path, loc.line)?;
        if !v.is_null() {
            return Ok(v);
        }
    }
    let target = match (framework, &origin) {
        (Framework::Cargo, Some(TestOrigin::Cargo(inv))) => {
            let mut inv = inv.clone();
            inv.test_filter = Some(name.clone());
            inv.test_exact = true;
            inv.program_args.retain(|a| a.starts_with('-') && a != "--exact");
            DebugTarget::Cargo { name: name.clone(), invocation: inv }
        }
        (Framework::Pytest, Some(TestOrigin::Process(cmd))) => {
            let python = std::path::Path::new(&cmd.program)
                .file_name()
                .is_some_and(|b| b.to_string_lossy().starts_with("python"))
                .then(|| crate::normalize_path(&cmd.cwd.join(&cmd.program)))
                .filter(|p| p.is_file());
            DebugTarget::Python {
                name: name.clone(),
                program: None,
                module: Some("pytest".into()),
                args: vec![format!("{file}::{name}")],
                cwd: Some(cwd.clone()),
                env: cmd.env.clone(),
                python,
                just_my_code: true,
            }
        }
        _ => {
            return Err(CommandError::Failed(format!(
                "CodeCraft can't tell how to debug `{name}` (no source location); use Debug in the editor's gutter"
            )));
        }
    };
    debug::start(s, target)?;
    Ok(json!({ "name": name, "status": status_name(s) }))
}

/// `{"/local": "/remote"}` or `[{localRoot, remoteRoot}]`.
fn path_mappings(p: &Value) -> Result<Vec<codecraft_dap::launch::PathMapping>, CommandError> {
    let bad = || CommandError::BadParams("`pathMappings` is [{localRoot, remoteRoot}] or {\"/local\": \"/remote\"}".into());
    match p.get("pathMappings") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(local, remote)| {
                Ok(codecraft_dap::launch::PathMapping {
                    local_root: local.clone(),
                    remote_root: remote.as_str().ok_or_else(bad)?.to_owned(),
                })
            })
            .collect(),
        Some(v @ Value::Array(_)) => serde_json::from_value(v.clone()).map_err(|_| bad()),
        Some(_) => Err(bad()),
    }
}

fn python_remote(s: &mut Session, p: &Value) -> CmdResult {
    let (host, port) = match (opt_str(p, "address"), opt_str(p, "host")) {
        (Some(address), _) => {
            let (host, port) = address
                .trim()
                .rsplit_once(':')
                .ok_or_else(|| CommandError::BadParams("`address` is host:port".into()))?;
            (host.to_owned(), port.parse::<u16>().map_err(|_| CommandError::BadParams("bad port".into()))?)
        }
        (None, Some(host)) => {
            let port = p
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|v| u16::try_from(v).ok())
                .ok_or_else(|| CommandError::BadParams("`port` must be a port number".into()))?;
            (host.to_owned(), port)
        }
        (None, None) => {
            // No address: the form in the Debug tool window.
            s.debug.form = Some(debug::DebugForm::PythonRemote);
            s.debug.choosing = true;
            s.ws.layout.show(codecraft_workspace::ToolWindowId::Debug);
            return ok();
        }
    };
    let name = opt_str(p, "name").map_or_else(|| format!("Python {host}:{port}"), str::to_owned);
    let target = DebugTarget::PythonRemote {
        name: name.clone(),
        host,
        port,
        path_mappings: path_mappings(p)?,
        just_my_code: bool_or(p, "justMyCode", true),
    };
    debug::start(s, target)?;
    Ok(json!({ "name": name, "status": status_name(s) }))
}

// ---------------------------------------------------------------- breakpoints

fn breakpoint_set(s: &mut Session, p: &Value) -> CmdResult {
    let path = path_param(s, p)?;
    let line = line_1(p)?;
    // The same checks as the editor's breakpoints (the line must exist).
    let mut params = p.clone();
    if let Some(map) = params.as_object_mut() {
        map.insert("path".into(), json!(path));
        map.insert("line".into(), json!(line));
    }
    let v = s.execute("debug.setBreakpoint", &params)?;
    let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
    let bp = s.debug.breakpoints.iter().find(|b| b.id == id).map(agent::breakpoint_json).unwrap_or(Value::Null);
    Ok(bp)
}

fn breakpoint_remove(s: &mut Session, p: &Value) -> CmdResult {
    if p.get("id").is_some() {
        return s.execute("debug.removeBreakpoint", &json!({ "id": p.get("id") }));
    }
    let path = path_param(s, p)?;
    let line = line_1(p)?;
    s.execute("debug.removeBreakpoint", &json!({ "path": path, "line": line }))
}

fn breakpoint_list(s: &mut Session) -> Value {
    debug::ensure_loaded(s);
    let mut list: Vec<&debug::Breakpoint> = s.debug.breakpoints.iter().collect();
    list.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    let state = debug::exceptions::state(s);
    json!({
        "breakpoints": list.into_iter().map(agent::breakpoint_json).collect::<Vec<_>>(),
        "exceptions": state,
    })
}

fn edit_breakpoint(s: &mut Session, p: &Value) -> CmdResult {
    let id = match p.get("id").and_then(Value::as_u64) {
        Some(id) => {
            if !s.debug.breakpoints.iter().any(|b| b.id == id) {
                return Err(CommandError::BadParams(format!("no breakpoint {id}")));
            }
            id
        }
        None => {
            let (path, line) = crate::debug_cmds::location_of(s, p)?;
            match s.debug.breakpoint_at(&path, line).map(|b| b.id) {
                Some(id) => id,
                None if bool_or(p, "create", false) => {
                    crate::debug_cmds::check_line_of(s, &path, line)?;
                    debug::set_breakpoint(s, &path, line, BreakpointProps::default())
                }
                None => return Err(CommandError::Failed("there is no breakpoint on this line".into())),
            }
        }
    };
    s.debug.breakpoint_popup = Some(id);
    let bp = s.debug.breakpoints.iter().find(|b| b.id == id).map(agent::breakpoint_json).unwrap_or(Value::Null);
    Ok(bp)
}

// ---------------------------------------------------------------- stepping and evaluation

fn step(s: &mut Session, p: &Value) -> CmdResult {
    let timeout = agent::timeout_param(p)?;
    let action = req_str(p, "action")?;
    match action {
        "over" => debug::step(s, Step::Over)?,
        "into" => debug::step(s, Step::Into)?,
        "out" => debug::step(s, Step::Out)?,
        "continue" | "resume" => debug::step(s, Step::Continue)?,
        "pause" => debug::pause(s)?,
        "smartInto" => {
            let how = debug::actions::smart_step_into(s)?;
            if how == "chooser" || s.debug.step_targets.as_ref().is_some_and(|t| t.pending) {
                return Ok(json!({ "status": "choose", "stepTargets": s.debug.step_targets }));
            }
        }
        other => {
            return Err(CommandError::BadParams(format!(
                "unknown action `{other}` (over, into, out, continue, pause or smartInto)"
            )));
        }
    }
    let wait = agent::wait_for_stop(s, timeout);
    Ok(json!({ "status": status_name(s), "token": wait.get("token") }))
}

fn run_to_line(s: &mut Session, p: &Value) -> CmdResult {
    let timeout = agent::timeout_param(p)?;
    let path = path_param(s, p)?;
    let line = line_1(p)?;
    s.execute("run.debuggingActions.runToCursor", &json!({ "path": path, "line": line }))?;
    let wait = agent::wait_for_stop(s, timeout);
    Ok(json!({ "status": status_name(s), "token": wait.get("token") }))
}

fn evaluate(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(token) = token_param(p)? {
        return agent::poll(s, token);
    }
    let expression = req_str(p, "expression")?.trim().to_owned();
    if expression.is_empty() {
        return Err(CommandError::BadParams("the expression is empty".into()));
    }
    let context = match opt_str(p, "context").unwrap_or("watch") {
        c @ ("watch" | "repl" | "hover" | "clipboard") => c.to_owned(),
        other => return Err(CommandError::BadParams(format!("unknown context `{other}`"))),
    };
    let frame = match opt_usize(p, "frame")? {
        Some(i) => Some(
            s.debug
                .session
                .as_ref()
                .and_then(|x| x.frames.get(i))
                .map(|f| f.id)
                .ok_or_else(|| CommandError::BadParams(format!("no frame {i}")))?,
        ),
        None => s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id),
    };
    let timeout = agent::timeout_param(p)?;
    let token = agent::answer_token(s, timeout);
    debug::evaluate_for_agent(s, &expression, frame, &context, token)?;
    Ok(pending(token))
}

fn set_value(s: &mut Session, p: &Value) -> CmdResult {
    let value = req_str(p, "value")?.to_owned();
    let timeout = agent::timeout_param(p)?;
    if let Some(expression) = opt_str(p, "expression").map(str::to_owned) {
        let token = agent::answer_token(s, timeout);
        debug::actions::set_expression(s, &expression, &value, Some(token))?;
        return Ok(pending(token));
    }
    let name = req_str(p, "name")?.to_owned();
    let parent = match p.get("reference").and_then(Value::as_i64) {
        Some(r) => r,
        None => {
            debug::actions::find_variable(s, &name)
                .ok_or_else(|| CommandError::BadParams(format!("no variable `{name}` in the selected frame")))?
                .0
        }
    };
    let token = agent::answer_token(s, timeout);
    debug::actions::set_variable(s, parent, &name, &value, Some(token))?;
    Ok(pending(token))
}

fn jump_to_line(s: &mut Session, p: &Value) -> CmdResult {
    let (path, line) = crate::debug_cmds::location_of(s, p)?;
    crate::debug_cmds::check_line_of(s, &path, line)?;
    let how = debug::actions::jump_to_line(s, &path, line)?;
    Ok(json!({ "path": path, "line": line + 1, "via": how }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_commands_reject_bad_params_without_a_session() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        assert_eq!(s.execute("debug.stopSummary", &Value::Null).unwrap()["state"], json!("idle"));
        assert_eq!(s.execute("debug.waitForStop", &json!({ "timeoutMs": 10 })).unwrap()["pending"], json!(false));
        assert!(s.execute("debug.poll", &json!({ "token": 42 })).is_err());
        assert!(s.execute("debug.poll", &json!({ "token": -1 })).is_err());
        assert!(s.execute("debug.step", &json!({ "action": "over" })).is_err());
        assert!(s.execute("debug.step", &json!({ "action": "sideways" })).is_err());
        assert!(s.execute("debug.evaluate", &json!({ "expression": "" })).is_err());
        assert!(s.execute("debug.evaluate", &json!({ "expression": "1", "context": "nope" })).is_err());
        assert!(s.execute("debug.breakpoint.set", &json!({ "path": "/no/such.rs", "line": 0 })).is_err());
        assert!(s.execute("debug.pythonRemote", &json!({ "address": "nohost" })).is_err());
        s.execute("debug.pythonRemote", &json!({})).unwrap();
        assert_eq!(s.debug.form, Some(debug::DebugForm::PythonRemote), "no address: the form");
        assert!(s.execute("debug.pythonRemote", &json!({ "host": "h", "port": 70000 })).is_err());
        assert!(s.execute("debug.pythonRemote", &json!({ "address": "h:1", "pathMappings": 5 })).is_err());
        assert!(s.execute("debug.editBreakpoint", &json!({ "id": 9 })).is_err());
        assert!(s.execute("debug.test", &json!({})).is_err());
        let list = s.execute("debug.breakpoint.list", &Value::Null).unwrap();
        assert_eq!(list["exceptions"]["pythonUncaught"], json!(true));
        let e = s.execute("debug.exceptionBreakpoints", &json!({ "cppThrow": true, "rustPanic": false })).unwrap();
        assert_eq!((e["cppThrow"].clone(), e["rustPanic"].clone()), (json!(true), json!(false)));
        assert!(s.execute("debug.exceptionBreakpoints", &json!({ "cppThrow": 1 })).is_err());
    }

    #[test]
    fn debug_only_configurations_parse_their_command() {
        use crate::run_config_store::{RunKind, UserRunConfig};
        let project = std::path::Path::new("/p");
        let config = |kind: RunKind, command: &str| UserRunConfig {
            name: "c".into(),
            kind,
            command: command.into(),
            ..Default::default()
        };
        let remote = config(RunKind::RemoteDebug, "10.0.0.2:1234 build/app").debug_target(project).unwrap().unwrap();
        assert_eq!(
            remote,
            DebugTarget::Remote {
                name: "c".into(),
                address: "10.0.0.2:1234".into(),
                program: Some("/p/build/app".into()),
                platform: None,
                args: vec![],
            }
        );
        let platform = config(RunKind::RemoteDebug, "connect://h:9 --platform remote-android app -- -v")
            .debug_target(project)
            .unwrap()
            .unwrap();
        assert!(matches!(platform, DebugTarget::Remote { ref platform, ref args, .. }
            if platform.as_deref() == Some("remote-android") && args == &["-v"]));
        assert!(config(RunKind::RemoteDebug, "nohost").debug_target(project).unwrap().is_err());
        assert!(config(RunKind::RemoteDebug, "h:1 a b").debug_target(project).unwrap().is_err());
        let core = config(RunKind::CoreDump, "/cores/core.1 app").debug_target(project).unwrap().unwrap();
        assert_eq!(
            core,
            DebugTarget::CoreDump { name: "c".into(), core: "/cores/core.1".into(), program: Some("/p/app".into()) }
        );
        assert!(config(RunKind::CoreDump, "a b c").debug_target(project).unwrap().is_err());
        let mut py = config(RunKind::PythonRemote, "127.0.0.1:5678 src=/srv/app");
        py.debugger.just_my_code = Some(false);
        let target = py.debug_target(project).unwrap().unwrap();
        assert!(matches!(target, DebugTarget::PythonRemote { port: 5678, ref path_mappings, just_my_code: false, .. }
            if path_mappings[0].local_root == "/p/src" && path_mappings[0].remote_root == "/srv/app"));
        assert!(config(RunKind::PythonRemote, "h:1 nomapping").debug_target(project).unwrap().is_err());
        assert!(config(RunKind::Cargo, "run").debug_target(project).is_none());
        // Run of a debug-only configuration explains itself instead of running anything.
        let run = config(RunKind::CoreDump, "core").to_run_config(project, project).unwrap();
        assert!(run.command.unwrap().args.join(" ").contains("use Run › Debug"));
        assert!(config(RunKind::CoreDump, "a b c").validate(project).is_err());
        for kind in [RunKind::RemoteDebug, RunKind::CoreDump, RunKind::PythonRemote] {
            assert_eq!(RunKind::parse(kind.id()).unwrap(), kind);
            assert!(kind.debug_only());
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, json!(kind.id()));
        }
    }

    #[test]
    fn python_frameworks_drop_their_reloaders_while_debugging() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
        let manage = std::path::Path::new("/p/manage.py");
        assert_eq!(
            debug::python_debug_args(Some(manage), None, &args(&["runserver", "8000"])),
            args(&["runserver", "8000", "--noreload"])
        );
        assert_eq!(debug::python_debug_args(Some(manage), None, &args(&["migrate"])), args(&["migrate"]));
        assert_eq!(
            debug::python_debug_args(None, Some("uvicorn"), &args(&["main:app", "--reload", "--reload-dir", "src", "--port", "1"])),
            args(&["main:app", "--port", "1"])
        );
        assert_eq!(
            debug::python_debug_args(None, Some("flask"), &args(&["--app", "app", "run", "--debug", "--reload"])),
            args(&["--app", "app", "run", "--debug", "--no-reload", "--no-debugger"])
        );
        assert_eq!(debug::python_debug_args(None, Some("pytest"), &args(&["-x"])), args(&["-x"]));
        // A Django configuration debugs without the autoreloader.
        let mut s = Session::new();
        s.lsp_enabled = false;
        let target = crate::debug_cmds::process_target(
            "dj",
            &crate::runs::ProcessCommand::new("python3", &["manage.py", "runserver"], "/p"),
        )
        .unwrap();
        assert!(matches!(target, DebugTarget::Python { ref args, .. } if args == &["runserver"]));
    }

    #[test]
    fn debugger_options_take_just_my_code_and_subprocesses() {
        let mut o = crate::run_config_store::DebuggerOptions::default();
        o.fill(&json!({ "justMyCode": false, "subProcess": false })).unwrap();
        assert_eq!((o.just_my_code, o.sub_process), (Some(false), Some(false)));
        o.fill(&json!({ "justMyCode": true })).unwrap();
        assert_eq!(o.just_my_code, None, "true is the default");
        assert!(o.fill(&json!({ "subProcess": "no" })).is_err());
        let v = serde_json::to_value(&o).unwrap();
        assert!(v.get("just_my_code").is_none());
        assert_eq!(v["sub_process"], json!(false));
    }

    #[test]
    fn exception_settings_and_popup_persist_with_the_breakpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("m.py");
        std::fs::write(&file, "a = 1\nb = 2\n").unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(home.path());
        s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
        s.execute("debug.exceptionBreakpoints", &json!({ "pythonRaised": true, "pythonUncaught": false })).unwrap();
        let bp = s.execute("debug.breakpoint.set", &json!({ "path": file, "line": 2, "condition": "b" })).unwrap();
        assert_eq!((bp["line"].clone(), bp["condition"].clone()), (json!(2), json!("b")));
        assert!(s.execute("debug.breakpoint.set", &json!({ "path": file, "line": 0 })).is_err(), "1-based");
        assert!(s.execute("debug.breakpoint.set", &json!({ "path": file, "line": 9 })).is_err());
        // The properties popup.
        let popup = s.execute("debug.editBreakpoint", &json!({ "path": file, "line": 1 })).unwrap();
        assert_eq!(popup["id"], bp["id"]);
        assert_eq!(s.debug.breakpoint_popup, bp["id"].as_u64());
        assert!(s.execute("debug.editBreakpoint", &json!({ "path": file, "line": 0 })).is_err());
        let created = s.execute("debug.editBreakpoint", &json!({ "path": file, "line": 0, "create": true })).unwrap();
        assert_eq!(created["line"], json!(1));
        s.execute("debug.closeBreakpointPopup", &Value::Null).unwrap();
        assert!(s.debug.breakpoint_popup.is_none());
        // A new session loads it all back.
        let mut again = Session::new();
        again.lsp_enabled = false;
        again.isolate_home(home.path());
        again.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
        let list = again.execute("debug.breakpoint.list", &Value::Null).unwrap();
        assert_eq!(list["breakpoints"].as_array().unwrap().len(), 2, "{list}");
        assert_eq!(list["exceptions"]["pythonRaised"], json!(true));
        assert_eq!(list["exceptions"]["pythonUncaught"], json!(false));
        let removed = again.execute("debug.breakpoint.remove", &json!({ "path": file, "line": 2 })).unwrap();
        assert_eq!(removed["removed"], json!(true));
    }

    #[test]
    fn path_mappings_accept_both_shapes() {
        let m = path_mappings(&json!({ "pathMappings": { "/a": "/b" } })).unwrap();
        assert_eq!((m[0].local_root.as_str(), m[0].remote_root.as_str()), ("/a", "/b"));
        let m = path_mappings(&json!({ "pathMappings": [{ "localRoot": "/c", "remoteRoot": "/d" }] })).unwrap();
        assert_eq!(m[0].remote_root, "/d");
        assert!(path_mappings(&json!({ "pathMappings": { "/a": 1 } })).is_err());
        assert!(path_mappings(&json!({})).unwrap().is_empty());
    }
}
