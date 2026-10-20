//! Python commands: interpreters, packages, Python Console, setup.py (M11 track Y).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, ok, simple, spec};
use crate::params::{bool_or, opt_path, opt_str, req_str};
use crate::python::{self, CreateVenvDialog, PkgOp};
use crate::python_console::{self, ConsoleKind};
use crate::python_project::{self, SetupInfo};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "tools.pythonOrDebugConsole",
            "Python or Debug Console",
            None,
            "— open the Python Console (starting it with the project's interpreter)",
            Context::Global,
            always,
            open_console,
        ),
        spec(
            "tools.managePythonPackages",
            "Manage Python Packages…",
            None,
            "— open the Python Packages tool window and list the interpreter's packages",
            Context::Global,
            always,
            manage_packages,
        ),
        spec(
            "tools.createSetupPy",
            "Create setup.py",
            None,
            "{name?, version?, description?, author?, authorEmail?, url?, license?} — write setup.py in the project \
             (defaults from pyproject.toml; install_requires from requirements.txt) and open it",
            Context::Global,
            has_project,
            create_setup_py,
        ),
        spec(
            "tools.runSetupPyTask",
            "Run setup.py Task…",
            None,
            "{task?: string, args?: string} — run `python setup.py <task>` (sdist, bdist_wheel, develop, …) in the Run \
             tool window; without `task` opens the task chooser",
            Context::Global,
            has_project,
            run_setup_py_task,
        ),
        spec(
            "python.interpreter.detect",
            "Detect Python Interpreters",
            None,
            "— find virtualenvs, uv, poetry, conda, pyenv and system Pythons in the background",
            Context::Global,
            always,
            |s, _| {
                python::start_detect(s);
                ok()
            },
        ),
        spec(
            "python.interpreter.list",
            "Python Interpreters",
            None,
            "→ {selected, explicit, detecting, interpreters: [{path, version, kind, name}]}",
            Context::Global,
            always,
            list_interpreters,
        ),
        spec(
            "python.interpreter.select",
            "Select Python Interpreter",
            None,
            "{path: string|null} — use this interpreter for the project (run configurations, debugger, tests, \
             language server, packages, console); `path` is a Python executable or a virtualenv folder \
             (also one outside the project); null goes back to the project's virtualenv",
            Context::Global,
            has_project,
            select_interpreter,
        ),
        spec(
            "python.interpreter.browse",
            "Add Existing Virtualenv…",
            None,
            "{path?: string} — use a virtualenv (or interpreter) from any folder, like PyCharm's Add Interpreter › \
             Existing; without `path` asks for the virtualenv folder",
            Context::Global,
            has_project,
            browse_interpreter,
        ),
        spec(
            "python.interpreter.createVenv",
            "Create Virtualenv…",
            None,
            "{dir?: string (default .venv, relative to the project), base?: string, select?: bool (default true)} — \
             `uv venv` when uv is installed, else `python -m venv`; without `dir` opens the dialog",
            Context::Global,
            has_project,
            create_venv,
        ),
        spec(
            "python.packages.refresh",
            "Refresh Python Packages",
            None,
            "{outdated?: bool (default true)} — list installed packages (then check for newer versions)",
            Context::Global,
            always,
            refresh_packages,
        ),
        spec(
            "python.packages.list",
            "Python Packages",
            None,
            "→ {interpreter, packages: [{name, version, latest}], error, busy}",
            Context::Global,
            always,
            list_packages,
        ),
        spec(
            "python.packages.install",
            "Install Python Package",
            None,
            "{name?: string, version?: string} — install (`==1.2`, `>=1` or a bare version); without `name` uses the \
             Python Packages tool window's fields",
            Context::Global,
            always,
            install_package,
        ),
        spec(
            "python.packages.upgrade",
            "Upgrade Python Package",
            None,
            "{name: string} — upgrade to the newest version",
            Context::Global,
            always,
            |s, p| {
                let name = package_name(p)?;
                python::start_package_op(s, PkgOp::Upgrade { name }).map_err(CommandError::Failed)?;
                ok()
            },
        ),
        spec(
            "python.packages.uninstall",
            "Uninstall Python Package",
            None,
            "{name: string, confirm?: bool} — uninstall; without `confirm: true` asks first",
            Context::Global,
            always,
            uninstall_package,
        ),
        spec(
            "python.packages.installMissing",
            "Install Missing Python Module",
            None,
            "{module?: string} — install the package for a module reported by `ModuleNotFoundError` in the latest run",
            Context::Global,
            always,
            install_missing,
        ),
        spec(
            "python.console.start",
            "Start Python Console",
            None,
            "— start the console if it is not running",
            Context::Global,
            always,
            |s, _| {
                ensure_console(s)?;
                python::show(s, ToolWindowId::PythonConsole);
                ok()
            },
        ),
        spec(
            "python.console.execute",
            "Execute in Python Console",
            None,
            "{code?: string, force?: bool} — run code (default: the console's input); incomplete input stays in the \
             input with a new line unless `force`; while the code waits in input() the text answers it",
            Context::Global,
            always,
            console_execute,
        ),
        spec(
            "python.console.executeSelection",
            "Execute Selection in Python Console",
            Some("Alt+Shift+E"),
            "— run the selection (or the caret line) of the editor in the Python Console",
            Context::Editor,
            has_editor,
            execute_selection,
        ),
        simple("python.console.interrupt", "Interrupt Python Console", always, |s, _| {
            s.python.console.interrupt().map_err(CommandError::Failed)?;
            ok()
        }),
        simple("python.console.stop", "Stop Python Console", always, |s, _| {
            s.python.console.stop();
            ok()
        }),
        simple("python.console.restart", "Restart Python Console", always, |s, _| {
            s.python.console.stop();
            start_console(s)?;
            python::show(s, ToolWindowId::PythonConsole);
            ok()
        }),
        simple("python.console.clear", "Clear Python Console", always, |s, _| {
            s.python.console.clear();
            ok()
        }),
        simple("python.console.historyPrevious", "Previous Console Input", always, |s, _| {
            s.python.console.browse_history(true);
            ok()
        }),
        simple("python.console.historyNext", "Next Console Input", always, |s, _| {
            s.python.console.browse_history(false);
            ok()
        }),
        spec(
            "python.console.state",
            "Python Console State",
            None,
            "→ {running, alive, ready, version, awaitingInput, transcript, vars: [{name, type, value}], history}",
            Context::Global,
            always,
            console_state,
        ),
        spec(
            "python.runDoctest",
            "Run Doctests",
            None,
            "{path?: string} — run `python -m doctest -v` on a Python file (default: the active one)",
            Context::Global,
            always,
            run_doctest,
        ),
    ]
}

fn root(s: &Session) -> Result<PathBuf, CommandError> {
    s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

/// The project's interpreter as a program for a run configuration.
fn python_program(s: &Session) -> String {
    s.project_root()
        .map(|r| crate::run_configs::python_for(&r))
        .unwrap_or_else(|| s.python.python_program().display().to_string())
}

// ---------------------------------------------------------------- interpreters

fn list_interpreters(s: &mut Session, _: &Value) -> CmdResult {
    if !s.python.detected {
        python::start_detect(s);
    }
    Ok(json!({
        "selected": s.python.interpreter,
        "label": s.python.interpreter_label(),
        "explicit": s.python.explicit,
        "detecting": !s.python.detected,
        "interpreters": s.python.interpreters,
    }))
}

fn select_interpreter(s: &mut Session, p: &Value) -> CmdResult {
    let path = match p.get("path") {
        None => return Err(CommandError::BadParams("missing `path` (a string, or null for the default)".into())),
        Some(Value::Null) => None,
        Some(v) => {
            Some(PathBuf::from(v.as_str().ok_or_else(|| CommandError::BadParams("`path` must be a string".into()))?))
        }
    };
    let path = path.map(|p| match s.project_root() {
        Some(r) if p.is_relative() => r.join(p),
        _ => p,
    });
    python::select(s, path.as_deref()).map_err(CommandError::Failed)?;
    Ok(json!({ "selected": s.python.interpreter, "label": s.python.interpreter_label() }))
}

fn browse_interpreter(s: &mut Session, p: &Value) -> CmdResult {
    if p.get("path").is_some_and(|v| !v.is_null()) {
        return select_interpreter(s, p);
    }
    s.effects.push(crate::Effect::PickFolder {
        title: "Select Virtualenv Folder".into(),
        command: "python.interpreter.select".into(),
        param: "path".into(),
    });
    ok()
}

fn create_venv(s: &mut Session, p: &Value) -> CmdResult {
    let root = root(s)?;
    let Some(dir) = opt_str(p, "dir").map(str::trim).filter(|d| !d.is_empty()) else {
        let taken = |d: &str| root.join(d).exists();
        let dir = [".venv", "venv"].into_iter().find(|d| !taken(d)).unwrap_or(".venv").to_string();
        s.python.create_venv = Some(CreateVenvDialog { dir, base: opt_path(p, "base") });
        return ok();
    };
    let dir = PathBuf::from(dir);
    let dir = if dir.is_absolute() { dir } else { root.join(dir) };
    let base = opt_path(p, "base");
    python::create_venv(s, &dir, base.as_deref(), bool_or(p, "select", true)).map_err(CommandError::Failed)?;
    s.python.create_venv = None;
    python::show(s, ToolWindowId::PythonPackages);
    ok()
}

// ---------------------------------------------------------------- packages

fn package_name(p: &Value) -> Result<String, CommandError> {
    let name = req_str(p, "name")?.trim();
    python::requirement(name, None).map_err(CommandError::BadParams)
}

fn manage_packages(s: &mut Session, _: &Value) -> CmdResult {
    python::show(s, ToolWindowId::PythonPackages);
    if s.python.packages_for.as_deref() != Some(s.python.python_program().as_path()) {
        python::start_package_op(s, PkgOp::List { outdated: false, then_outdated: true })
            .map_err(CommandError::Failed)?;
    }
    ok()
}

fn refresh_packages(s: &mut Session, p: &Value) -> CmdResult {
    let outdated = bool_or(p, "outdated", true);
    python::start_package_op(s, PkgOp::List { outdated: false, then_outdated: outdated })
        .map_err(CommandError::Failed)?;
    ok()
}

fn list_packages(s: &mut Session, _: &Value) -> CmdResult {
    Ok(json!({
        "interpreter": s.python.packages_for,
        "packages": s.python.packages,
        "error": s.python.packages_error,
        "busy": !s.python.jobs.is_empty(),
    }))
}

fn install_package(s: &mut Session, p: &Value) -> CmdResult {
    let (name, version) = match opt_str(p, "name") {
        Some(n) => (n.to_string(), opt_str(p, "version").map(str::to_owned)),
        None => (s.python.install_name.clone(), Some(s.python.install_version.clone())),
    };
    if name.trim().is_empty() {
        return Err(CommandError::BadParams("type the name of a package to install".into()));
    }
    let spec = python::requirement(&name, version.as_deref()).map_err(CommandError::BadParams)?;
    python::start_package_op(s, PkgOp::Install { spec: spec.clone() }).map_err(CommandError::Failed)?;
    if p.get("name").is_none() {
        s.python.install_name.clear();
        s.python.install_version.clear();
    }
    Ok(json!({ "installing": spec }))
}

fn uninstall_package(s: &mut Session, p: &Value) -> CmdResult {
    let name = package_name(p)?;
    if !bool_or(p, "confirm", false) {
        s.python.confirm_uninstall = Some(name);
        return ok();
    }
    s.python.confirm_uninstall = None;
    python::start_package_op(s, PkgOp::Uninstall { name }).map_err(CommandError::Failed)?;
    ok()
}

fn install_missing(s: &mut Session, p: &Value) -> CmdResult {
    let module = match opt_str(p, "module") {
        Some(m) => m.to_string(),
        None => python::missing_modules_in_runs(s)
            .into_iter()
            .next()
            .ok_or_else(|| CommandError::Failed("no ModuleNotFoundError in the latest runs".into()))?,
    };
    let package = python::package_for_module(&module);
    let spec = python::requirement(&package, None).map_err(CommandError::BadParams)?;
    python::start_package_op(s, PkgOp::Install { spec: spec.clone() }).map_err(CommandError::Failed)?;
    python::show(s, ToolWindowId::PythonPackages);
    Ok(json!({ "installing": spec }))
}

// ---------------------------------------------------------------- console

fn start_console(s: &mut Session) -> Result<(), CommandError> {
    let python = s.python.python_program();
    let cwd = s.project_root();
    let r = s.python.console.start(&python, cwd.as_deref());
    let (kind, line) = match &r {
        Ok(()) => (crate::python_output::LineKind::Command, format!("{} -u -c <console driver>", python.display())),
        Err(e) => (crate::python_output::LineKind::Stderr, e.clone()),
    };
    s.python.process_output.push("Python Console", kind, &line);
    r.map_err(CommandError::Failed)
}

fn ensure_console(s: &mut Session) -> Result<(), CommandError> {
    if s.python.console.is_alive() { Ok(()) } else { start_console(s) }
}

fn open_console(s: &mut Session, _: &Value) -> CmdResult {
    ensure_console(s)?;
    python::show(s, ToolWindowId::PythonConsole);
    ok()
}

/// `>>> first line` / `... more` as shown in the transcript.
fn prompt_lines(code: &str) -> String {
    let mut out = String::new();
    for (i, l) in code.trim_end_matches('\n').split('\n').enumerate() {
        out.push_str(if i == 0 { ">>> " } else { "... " });
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// Run code in the console (the draft when `code` is `None`).
pub(crate) fn run_in_console(s: &mut Session, code: Option<String>, force: bool) -> Result<(), CommandError> {
    let from_draft = code.is_none();
    if s.python.console.awaiting_input.is_some() {
        let text = code.unwrap_or_else(|| std::mem::take(&mut s.python.console.draft));
        return s.python.console.send_input(text.trim_end_matches('\n')).map_err(CommandError::Failed);
    }
    let code = code.unwrap_or_else(|| s.python.console.draft.clone());
    if code.trim().is_empty() {
        if from_draft {
            s.python.console.draft.clear();
        }
        return Ok(());
    }
    ensure_console(s)?;
    let force = force || python_console::ends_block(&code);
    s.python.console.execute(&code, force).map_err(CommandError::Failed)?;
    if from_draft {
        s.python.console.draft.clear();
    }
    s.python.console.history_pos = None;
    s.python.console.pending_input = Some(code.clone());
    s.python.console.push(ConsoleKind::Input, &prompt_lines(&code));
    Ok(())
}

fn console_execute(s: &mut Session, p: &Value) -> CmdResult {
    let code = match p.get("code") {
        None | Some(Value::Null) => None,
        Some(v) => {
            Some(v.as_str().ok_or_else(|| CommandError::BadParams("`code` must be a string".into()))?.to_string())
        }
    };
    run_in_console(s, code, bool_or(p, "force", false))?;
    ok()
}

/// Remove the indentation common to all non-blank lines.
pub(crate) fn dedent(text: &str) -> String {
    let indent = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);
    text.lines().map(|l| l.chars().skip(indent).collect::<String>()).collect::<Vec<_>>().join("\n")
}

fn execute_selection(s: &mut Session, _: &Value) -> CmdResult {
    let code = {
        let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
        let sel = e.selections.primary();
        if sel.end() > sel.start() {
            d.doc.slice(sel.start()..sel.end())
        } else {
            let line = d.doc.char_to_line(sel.head);
            d.doc.line_text(line).unwrap_or_default()
        }
    };
    let code = dedent(&code);
    if code.trim().is_empty() {
        s.status = "Nothing to execute".into();
        return ok();
    }
    python::show(s, ToolWindowId::PythonConsole);
    run_in_console(s, Some(code), true)?;
    ok()
}

fn console_state(s: &mut Session, _: &Value) -> CmdResult {
    let c = &s.python.console;
    Ok(json!({
        "alive": c.is_alive(),
        "ready": c.ready,
        "running": c.running.is_some(),
        "version": c.version,
        "awaitingInput": c.awaiting_input,
        "transcript": c.text(),
        "vars": c.vars,
        "history": c.history,
        "draft": c.draft,
    }))
}

// ---------------------------------------------------------------- setup.py and doctest

fn create_setup_py(s: &mut Session, p: &Value) -> CmdResult {
    let root = root(s)?;
    let path = root.join("setup.py");
    if path.exists() {
        let _ = s.open_file(&path);
        return Err(CommandError::Failed("setup.py already exists".into()));
    }
    let defaults = python_project::default_setup_info(&root);
    let field = |key: &str, default: &str| opt_str(p, key).map(str::to_owned).unwrap_or_else(|| default.to_string());
    let info = SetupInfo {
        name: field("name", &defaults.name),
        version: field("version", &defaults.version),
        description: field("description", &defaults.description),
        author: field("author", ""),
        author_email: field("authorEmail", ""),
        url: field("url", ""),
        license: field("license", ""),
    };
    let reqs = std::fs::read_to_string(root.join("requirements.txt"))
        .map(|t| python_project::requirements(&t))
        .unwrap_or_default();
    std::fs::write(&path, python_project::setup_py(&info, &reqs))
        .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
    s.jobs_rescan();
    s.open_file(&path)?;
    Ok(json!({ "path": path }))
}

fn run_setup_py_task(s: &mut Session, p: &Value) -> CmdResult {
    let root = root(s)?;
    if !root.join("setup.py").is_file() {
        return Err(CommandError::Failed("the project has no setup.py (Tools › Create setup.py)".into()));
    }
    let Some(task) = opt_str(p, "task").map(str::trim).filter(|t| !t.is_empty()) else {
        s.python.setup_task = Some(String::new());
        return ok();
    };
    if !python_project::valid_task(task) {
        return Err(CommandError::BadParams(format!("`{task}` is not a setup.py command")));
    }
    let extra =
        crate::run_config_store::split_words(opt_str(p, "args").unwrap_or("")).map_err(CommandError::BadParams)?;
    let mut cmd = crate::runs::ProcessCommand::new(python_program(s), &["setup.py", task], root);
    cmd.args.extend(extra);
    let config = crate::runs::RunConfig::process(format!("setup.py {task}"), cmd);
    s.python.setup_task = None;
    crate::runs::start_config(s, &config)?;
    ok()
}

fn run_doctest(s: &mut Session, p: &Value) -> CmdResult {
    let path = match opt_path(p, "path") {
        Some(p) => p,
        None => {
            s.ws.active()
                .and_then(|(_, d)| d.path.clone())
                .ok_or_else(|| CommandError::BadParams("missing `path` (or an open Python file)".into()))?
        }
    };
    if path.extension().and_then(|e| e.to_str()) != Some("py") || !path.is_file() {
        return Err(CommandError::BadParams(format!("{} is not a Python file", path.display())));
    }
    let root = s.project_root().unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());
    let config = python_project::doctest_config(&root, &path, &crate::run_configs::python_for(&root));
    crate::runs::start_config(s, &config)?;
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedent_and_prompts() {
        assert_eq!(dedent("    if x:\n        y()\n\n    z()"), "if x:\n    y()\n\nz()");
        assert_eq!(dedent("a\n  b"), "a\n  b");
        assert_eq!(prompt_lines("def f():\n    return 1\n"), ">>> def f():\n...     return 1\n");
    }
}
