//! Python support (M11 track Y): interpreter selection driving run configurations, the Python
//! Packages operations through fake `pip`/`uv` scripts, virtualenv creation, setup.py, framework
//! run configurations, and the Python Console against the real `python3` (skipped without it).
//! Never installs real packages and never touches the network.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::python_env::Probe;
use codecraft_engine::{CommandError, Session};

fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake interpreter: logs its arguments, answers `pip list` from a state file, `pip install`
/// adds flask, anything else prints a version.
fn fake_python(path: &Path, log: &Path) {
    let state = path.with_file_name("pip-state.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&state, r#"[{"name": "requests", "version": "2.0"}, {"name": "pip", "version": "24.0"}]"#).unwrap();
    let body = format!(
        r#"echo "$@" >> '{log}'
case "$*" in
  *"no-such"*) echo 'ERROR: No matching distribution' >&2; exit 1 ;;
  *"pip list --format json --outdated"*) echo '[{{"name": "requests", "version": "2.0", "latest_version": "2.31.0"}}]' ;;
  *"pip list --format json"*) cat '{state}' ;;
  *"pip install"*) echo 'Collecting flask'; echo '[{{"name": "requests", "version": "2.0"}}, {{"name": "Flask", "version": "3.0.0"}}]' > '{state}' ;;
  *"pip uninstall"*) echo 'Successfully uninstalled' ;;
  *) echo 'Python 3.12.1' ;;
esac"#,
        log = log.display(),
        state = state.display()
    );
    script(path, &body);
}

fn venv(dir: &Path, log: &Path) -> PathBuf {
    let py = dir.join("bin/python");
    fake_python(&py, log);
    std::fs::write(dir.join("pyvenv.cfg"), "home = /usr/bin\nversion = 3.12.1\n").unwrap();
    py
}

fn wait<F: Fn(&Session) -> bool>(s: &mut Session, done: F) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out; status: {}", s.status);
}

fn open(root: &Path, probe_dirs: Vec<PathBuf>) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.python.probe = Some(Probe::isolated(probe_dirs));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait(&mut s, |s| !s.jobs_running() && s.python.root.is_some());
    s
}

fn log_text(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
fn selection_drives_run_configurations_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    let log = tmp.path().join("calls.log");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("manage.py"), "import django\n").unwrap();
    std::fs::write(root.join("requirements.txt"), "django\n").unwrap();
    let project_py = venv(&root.join(".venv"), &log);
    let other_py = venv(&tmp.path().join("other-env"), &log);

    let mut s = open(&root, Vec::new());
    assert_eq!(s.python.interpreter.as_deref(), Some(project_py.as_path()));
    assert!(!s.python.explicit);
    assert_eq!(s.python.interpreter_label(), "Python 3.12 (.venv)");
    // Derived Django configuration with the project's virtualenv.
    let django = s.run_configs.iter().find(|c| c.name == "Django server").expect("django config");
    assert_eq!(django.command.as_ref().unwrap().program, project_py.display().to_string());

    // Detection (isolated: only the project's virtualenv).
    s.execute("python.interpreter.detect", &Value::Null).unwrap();
    wait(&mut s, |s| s.python.detected);
    let list = s.execute("python.interpreter.list", &Value::Null).unwrap();
    assert_eq!(list["interpreters"][0]["kind"], "venv");
    assert_eq!(list["interpreters"][0]["path"], project_py.display().to_string());

    // Select another interpreter: persisted and used by new configurations.
    s.execute("python.interpreter.select", &json!({ "path": other_py })).unwrap();
    assert!(s.python.explicit);
    assert!(root.join(".codecraft/python.json").is_file());
    assert_eq!(codecraft_engine::run_configs::python_for(&root), other_py.display().to_string());
    let django = s.run_configs.iter().find(|c| c.name == "Django server").unwrap();
    assert_eq!(django.command.as_ref().unwrap().program, other_py.display().to_string());
    s.execute("runConfig.create", &json!({ "name": "Serve", "kind": "django", "command": "runserver 8001" })).unwrap();
    let serve = s.run_configs.iter().find(|c| c.name == "Serve").unwrap().command.clone().unwrap();
    assert_eq!(serve.program, other_py.display().to_string());
    assert_eq!(serve.args, ["manage.py", "runserver", "8001"]);
    s.execute("runConfig.create", &json!({ "name": "Api", "kind": "fastapi", "command": "main:app --reload" }))
        .unwrap();
    let api = s.run_configs.iter().find(|c| c.name == "Api").unwrap().command.clone().unwrap();
    assert_eq!(api.args, ["-m", "uvicorn", "main:app", "--reload"]);

    // A new session reads the selection back; null returns to the virtualenv.
    drop(s);
    let mut s = open(&root, Vec::new());
    assert_eq!(s.python.interpreter.as_deref(), Some(other_py.as_path()));
    s.execute("python.interpreter.select", &json!({ "path": null })).unwrap();
    assert_eq!(s.python.interpreter.as_deref(), Some(project_py.as_path()));
    assert!(!root.join(".codecraft/python.json").exists());
    assert!(matches!(
        s.execute("python.interpreter.select", &json!({ "path": "/no/such/python" })),
        Err(CommandError::Failed(_))
    ));
    assert!(matches!(s.execute("python.interpreter.select", &json!({})), Err(CommandError::BadParams(_))));
}

#[test]
fn virtualenvs_are_found_in_the_project_and_chosen_from_any_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    let log = tmp.path().join("calls.log");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.py"), "print(1)\n").unwrap();
    std::fs::write(root.join("requirements.txt"), "requests\n").unwrap();
    // An unconventionally named virtualenv in the project is picked up on its own.
    let project_py = venv(&root.join("linkedin_env"), &log);
    let shared_py = venv(&tmp.path().join("shared/ml-env"), &log);

    let mut s = open(&root, Vec::new());
    assert!(s.python.python_project);
    assert_eq!(s.python.interpreter.as_deref(), Some(project_py.as_path()));
    assert!(!s.python.explicit);
    let run = s.run_configs.iter().find(|c| c.name == "Run main.py").expect("derived main.py config");
    assert_eq!(run.command.as_ref().unwrap().program, project_py.display().to_string());
    s.execute("python.interpreter.detect", &Value::Null).unwrap();
    wait(&mut s, |s| s.python.detected);
    let list = s.execute("python.interpreter.list", &Value::Null).unwrap();
    assert_eq!(list["interpreters"][0]["name"], "linkedin_env");

    // Without a path the browse command asks the shell for a folder, then selects it.
    s.effects.clear();
    s.execute("python.interpreter.browse", &Value::Null).unwrap();
    assert!(matches!(
        s.effects.last(),
        Some(codecraft_engine::Effect::PickFolder { command, param, .. })
            if command == "python.interpreter.select" && param == "path"
    ));
    // A virtualenv folder outside the project selects the interpreter inside it and stays listed.
    s.execute("python.interpreter.browse", &json!({ "path": tmp.path().join("shared/ml-env") })).unwrap();
    assert!(s.python.explicit);
    assert_eq!(s.python.interpreter.as_deref(), Some(shared_py.as_path()));
    assert_eq!(s.python.interpreter_label(), "Python 3.12 (ml-env)");
    let list = s.execute("python.interpreter.list", &Value::Null).unwrap();
    assert!(list["interpreters"].as_array().unwrap().iter().any(|i| i["path"] == shared_py.display().to_string()));
    let run = s.run_configs.iter().find(|c| c.name == "Run main.py").unwrap();
    assert_eq!(run.command.as_ref().unwrap().program, shared_py.display().to_string());
    // A folder that is not a virtualenv is refused.
    assert!(matches!(
        s.execute("python.interpreter.select", &json!({ "path": tmp.path() })),
        Err(CommandError::Failed(_))
    ));

    // One configuration can run in its own virtualenv, whatever the project uses.
    s.execute(
        "runConfig.create",
        &json!({ "name": "In project env", "kind": "python", "command": "main.py",
                 "pythonInterpreter": "linkedin_env" }),
    )
    .unwrap();
    let cmd = s.run_configs.iter().find(|c| c.name == "In project env").unwrap().command.clone().unwrap();
    assert_eq!(cmd.program, project_py.display().to_string());
    assert!(cmd.env.iter().any(|(k, v)| k == "VIRTUAL_ENV" && v.ends_with("linkedin_env")));
    let listed = s.execute("runConfig.list", &Value::Null).unwrap();
    assert_eq!(listed["user"][0]["pythonInterpreter"], "linkedin_env");
    // A missing virtualenv is rejected with the configuration unchanged.
    let bad = s.execute("runConfig.update", &json!({ "index": 0, "pythonInterpreter": "gone" }));
    assert!(matches!(bad, Err(CommandError::BadParams(e)) if e.contains("Python interpreter")));
    s.execute("runConfig.update", &json!({ "index": 0, "pythonInterpreter": null })).unwrap();
    let cmd = s.run_configs.iter().find(|c| c.name == "In project env").unwrap().command.clone().unwrap();
    assert_eq!(cmd.program, shared_py.display().to_string());
}

#[test]
fn packages_list_install_upgrade_uninstall_with_pip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    let log = tmp.path().join("calls.log");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"demo\"\n").unwrap();
    let py = venv(&root.join(".venv"), &log);
    // No uv on the (isolated) probe path: pip is used.
    let mut s = open(&root, Vec::new());

    s.execute("tools.managePythonPackages", &Value::Null).unwrap();
    wait(&mut s, |s| s.python.jobs.is_empty() && s.python.packages.iter().any(|p| p.latest.is_some()));
    let names: Vec<&str> = s.python.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["pip", "requests"]);
    assert_eq!(s.python.packages[1].latest.as_deref(), Some("2.31.0"));
    assert_eq!(s.python.packages_for.as_deref(), Some(py.as_path()));
    assert!(log_text(&log).contains("-m pip list --format json --disable-pip-version-check"));

    // Search filters the installed list.
    s.python.filter = "REQ".into();
    assert_eq!(s.python.visible_packages().len(), 1);
    s.python.filter.clear();

    // Install from the tool window's fields.
    s.python.install_name = "flask".into();
    s.python.install_version = "3.0.0".into();
    s.execute("python.packages.install", &Value::Null).unwrap();
    assert!(s.python.install_name.is_empty());
    // One change at a time.
    assert!(s.execute("python.packages.upgrade", &json!({ "name": "requests" })).is_err());
    wait(&mut s, |s| s.python.jobs.is_empty() && s.python.packages.iter().any(|p| p.name == "Flask"));
    assert!(log_text(&log).contains("-m pip install flask==3.0.0"));
    assert!(s.python.log.iter().any(|l| l.contains("Collecting flask")), "{:?}", s.python.log);

    // Bad names never reach pip.
    for bad in ["--index-url=http://x", "", "a b"] {
        assert!(matches!(
            s.execute("python.packages.install", &json!({ "name": bad })),
            Err(CommandError::BadParams(_))
        ));
    }

    s.execute("python.packages.upgrade", &json!({ "name": "requests" })).unwrap();
    wait(&mut s, |s| s.python.jobs.is_empty());
    assert!(log_text(&log).contains("-m pip install --upgrade requests"));

    // Uninstall asks first; confirmed it runs.
    s.execute("python.packages.uninstall", &json!({ "name": "requests" })).unwrap();
    assert_eq!(s.python.confirm_uninstall.as_deref(), Some("requests"));
    assert!(!log_text(&log).contains("uninstall"));
    s.execute("python.packages.uninstall", &json!({ "name": "requests", "confirm": true })).unwrap();
    assert!(s.python.confirm_uninstall.is_none());
    wait(&mut s, |s| s.python.jobs.is_empty());
    assert!(log_text(&log).contains("-m pip uninstall -y requests"));

    // A failing install is reported, not a panic.
    s.execute("python.packages.install", &json!({ "name": "no-such" })).unwrap();
    wait(&mut s, |s| s.python.jobs.is_empty());
    assert!(s.status.contains("failed"), "{}", s.status);

    let listed = s.execute("python.packages.list", &Value::Null).unwrap();
    assert!(listed["packages"].as_array().unwrap().iter().any(|p| p["name"] == "Flask"));
}

#[test]
fn uv_is_used_for_virtualenvs_and_creates_them() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    let log = tmp.path().join("calls.log");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("requirements.txt"), "").unwrap();
    // A fake uv: logs, lists packages, and `uv venv DIR` makes a virtualenv with a fake python.
    let template = tmp.path().join("template/bin/python");
    fake_python(&template, &log);
    script(
        &bin.join("uv"),
        &format!(
            r#"echo "uv $@" >> '{log}'
case "$1 $2" in
  "venv "*) mkdir -p "$2/bin" && cp '{template}' "$2/bin/python" && echo 'version = 3.13.0' > "$2/pyvenv.cfg" ;;
  "pip list") echo '[{{"name": "uvpkg", "version": "1.0"}}]' ;;
  *) exit 0 ;;
esac"#,
            log = log.display(),
            template = template.display()
        ),
    );
    let mut s = open(&root, vec![bin.clone()]);
    assert_eq!(s.python.interpreter, None);

    // Without `dir` the dialog opens with a free default.
    s.execute("python.interpreter.createVenv", &Value::Null).unwrap();
    assert_eq!(s.python.create_venv.as_ref().unwrap().dir, ".venv");
    s.execute("python.interpreter.createVenv", &json!({ "dir": ".venv" })).unwrap();
    assert!(s.python.create_venv.is_none());
    let venv_py = root.join(".venv/bin/python");
    wait(&mut s, |s| s.python.jobs.is_empty() && s.python.interpreter.is_some());
    assert_eq!(s.python.interpreter.as_deref(), Some(venv_py.as_path()));
    assert!(log_text(&log).contains(&format!("uv venv {}", root.join(".venv").display())));
    // An existing non-empty directory is refused.
    assert!(s.execute("python.interpreter.createVenv", &json!({ "dir": ".venv" })).is_err());

    // Packages of a virtualenv go through `uv pip … --python`.
    s.execute("python.packages.refresh", &json!({ "outdated": false })).unwrap();
    wait(&mut s, |s| s.python.jobs.is_empty() && !s.python.packages.is_empty());
    assert_eq!(s.python.packages[0].name, "uvpkg");
    assert!(log_text(&log).contains(&format!("uv pip list --format json --python {}", venv_py.display())));
    s.execute("python.packages.install", &json!({ "name": "httpx", "version": ">=0.27" })).unwrap();
    wait(&mut s, |s| s.python.jobs.is_empty());
    assert!(log_text(&log).contains(&format!("uv pip install --python {} httpx>=0.27", venv_py.display())));
}

#[test]
fn setup_py_and_doctest_and_missing_modules() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("demo");
    let log = tmp.path().join("calls.log");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("requirements.txt"), "requests>=2\n").unwrap();
    let py = venv(&root.join(".venv"), &log);
    let mut s = open(&root, Vec::new());

    // Run setup.py Task needs a setup.py.
    assert!(s.execute("tools.runSetupPyTask", &json!({ "task": "sdist" })).is_err());
    let r = s.execute("tools.createSetupPy", &json!({ "author": "Ada" })).unwrap();
    let text = std::fs::read_to_string(root.join("setup.py")).unwrap();
    assert!(
        text.contains("name=\"demo\"") && text.contains("author=\"Ada\"") && text.contains("\"requests>=2\""),
        "{text}"
    );
    assert_eq!(r["path"], root.join("setup.py").display().to_string());
    assert!(s.ws.active().is_some_and(|(_, d)| d.path.as_deref() == Some(root.join("setup.py").as_path())));
    assert!(s.execute("tools.createSetupPy", &Value::Null).is_err());

    // Without a task the chooser opens; with one it runs with the project's interpreter.
    s.execute("tools.runSetupPyTask", &Value::Null).unwrap();
    assert!(s.python.setup_task.is_some());
    assert!(s.execute("tools.runSetupPyTask", &json!({ "task": "--help" })).is_err());
    s.execute("tools.runSetupPyTask", &json!({ "task": "sdist", "args": "--formats=gztar" })).unwrap();
    assert!(s.python.setup_task.is_none());
    let tab = s.ws.runs.last().unwrap();
    assert_eq!(tab.title, "setup.py sdist");
    assert!(
        tab.command_line.contains(&py.display().to_string())
            && tab.command_line.contains("setup.py sdist --formats=gztar")
    );
    wait(&mut s, |s| !s.jobs_running());

    // Doctests of a file.
    let m = root.join("m.py");
    std::fs::write(&m, "def f():\n    \"\"\"\n    >>> f()\n    1\n    \"\"\"\n    return 1\n").unwrap();
    s.execute("python.runDoctest", &json!({ "path": m })).unwrap();
    assert!(s.ws.runs.last().unwrap().command_line.ends_with("-m doctest -v m.py"));
    assert!(s.execute("python.runDoctest", &json!({ "path": root.join("setup.cfg") })).is_err());
    wait(&mut s, |s| !s.jobs_running());

    // A run that failed with ModuleNotFoundError offers the package.
    let tab = s.ws.add_run("app", "python app.py", false);
    s.ws.run_mut(tab)
        .unwrap()
        .push(codecraft_engine::workspace::ConsoleKind::Stderr, "ModuleNotFoundError: No module named 'yaml'");
    assert_eq!(codecraft_engine::python::missing_modules_in_runs(&s), ["yaml"]);
    let r = s.execute("python.packages.installMissing", &Value::Null).unwrap();
    assert_eq!(r["installing"], "PyYAML");
    wait(&mut s, |s| s.python.jobs.is_empty());
    assert!(log_text(&log).contains("-m pip install PyYAML"));
}

fn python3() -> Option<()> {
    let out = std::process::Command::new("python3").arg("--version").output().ok()?;
    out.status.success().then_some(())
}

#[test]
fn python_console_round_trip() {
    let Some(_) = python3() else {
        eprintln!("python3 not installed; skipping");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("script.py"), "print('from selection')\nvalue = 7\n").unwrap();
    let mut s = open(&root, Vec::new());

    s.execute("tools.pythonOrDebugConsole", &Value::Null).unwrap();
    assert!(s.python.console.is_alive());
    let idle = |s: &Session| s.python.console.ready && s.python.console.running.is_none();
    wait(&mut s, idle);

    s.execute("python.console.execute", &json!({ "code": "a = 6 * 7\na" })).unwrap();
    wait(&mut s, idle);
    let state = s.execute("python.console.state", &Value::Null).unwrap();
    assert!(state["transcript"].as_str().unwrap().contains(">>> a = 6 * 7\n... a\n42\n"), "{state}");
    assert!(state["vars"].as_array().unwrap().iter().any(|v| v["name"] == "a" && v["value"] == "42"));

    // Typed input: a block continues until its blank line.
    s.python.console.draft = "def f():".into();
    s.execute("python.console.execute", &Value::Null).unwrap();
    wait(&mut s, idle);
    assert_eq!(s.python.console.draft, "def f():\n    ");
    s.python.console.draft.push_str("return 5");
    s.execute("python.console.execute", &Value::Null).unwrap();
    wait(&mut s, idle);
    assert_eq!(s.python.console.draft, "def f():\n    return 5\n    ");
    s.execute("python.console.execute", &Value::Null).unwrap();
    wait(&mut s, idle);
    assert!(s.python.console.draft.is_empty());
    s.python.console.draft = "f() + 1".into();
    s.execute("python.console.execute", &Value::Null).unwrap();
    wait(&mut s, idle);
    assert!(s.python.console.text().contains("\n6\n"), "{}", s.python.console.text());
    assert_eq!(s.python.console.history, ["a = 6 * 7\na", "def f():\n    return 5", "f() + 1"]);
    s.execute("python.console.historyPrevious", &Value::Null).unwrap();
    assert_eq!(s.python.console.draft, "f() + 1");
    s.execute("python.console.historyNext", &Value::Null).unwrap();
    assert_eq!(s.python.console.draft, "");

    // Execute Selection (no selection: the caret line).
    s.execute("file.open", &json!({ "path": root.join("script.py") })).unwrap();
    s.execute("python.console.executeSelection", &Value::Null).unwrap();
    wait(&mut s, idle);
    assert!(s.python.console.text().contains("from selection"));

    // Restart, then shutdown kills the process.
    s.execute("python.console.restart", &Value::Null).unwrap();
    wait(&mut s, idle);
    assert!(s.python.console.vars.is_empty());
    s.shutdown();
    assert!(!s.python.console.is_alive());
}
