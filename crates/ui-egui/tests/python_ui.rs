//! Python (M11 track Y) driven headlessly: the status bar interpreter widget, the Python Packages
//! tool window (listing through a fake pip, uninstall with confirmation) and the Python Console
//! (against the real `python3`, skipped without it).
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::python_env::Probe;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

/// A virtualenv whose `python` answers `pip list` and logs `pip uninstall`.
fn fake_venv(dir: &Path, log: &Path) {
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("pyvenv.cfg"), "version = 3.12.1\n").unwrap();
    let py = dir.join("bin/python");
    let body = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$*\" in\n  *--outdated*) echo '[]' ;;\n  *\"pip list\"*) echo '[{{\"name\": \"requests\", \"version\": \"2.31.0\"}}]' ;;\n  *) echo 'Python 3.12.1' ;;\nesac\n",
        log.display()
    );
    std::fs::write(&py, body).unwrap();
    std::fs::set_permissions(&py, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn packages_window_lists_and_uninstalls_with_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let log = root.join("calls.log");
    std::fs::write(root.join("requirements.txt"), "requests\n").unwrap();
    std::fs::write(root.join("main.py"), "import requests\n").unwrap();
    fake_venv(&root.join(".venv"), &log);

    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.python.probe = Some(Probe::isolated(Vec::new()));
    settle(&mut h, "project", |a| a.session.python.root.is_some() && a.session.ws.project.is_some());
    h.run_steps(2);
    // The status bar shows the project's interpreter.
    assert!(h.query_by_label("Python 3.12 (.venv)").is_some(), "status bar interpreter widget");

    h.state_mut().session.ws.layout.remember_size(ToolWindowId::PythonPackages, 420.0);
    h.state_mut().session.execute("tools.managePythonPackages", &json!({})).unwrap();
    settle(&mut h, "package list", |a| a.session.python.jobs.is_empty() && !a.session.python.packages.is_empty());
    h.run_steps(3);
    assert!(h.query_by_label("requests").is_some(), "the installed package row");
    assert!(h.query_by_label("2.31.0").is_some());

    // Select it, Uninstall asks first, then runs pip.
    h.get_by_label("requests").click();
    h.run_steps(2);
    assert_eq!(h.state().session.python.selected_package.as_deref(), Some("requests"));
    h.get_by_label("Uninstall requests").click();
    h.run_steps(3);
    assert!(h.query_by_label("Uninstall Package").is_some(), "the confirmation dialog");
    assert!(!std::fs::read_to_string(&log).unwrap_or_default().contains("uninstall"));
    h.get_by_label("Uninstall").click();
    settle(&mut h, "uninstall", |a| a.session.python.jobs.is_empty() && a.session.python.confirm_uninstall.is_none());
    assert!(std::fs::read_to_string(&log).unwrap().contains("-m pip uninstall -y requests"));
}

#[test]
fn console_window_shows_results_and_variables() {
    let has_python = std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success());
    if !has_python {
        eprintln!("python3 not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("main.py"), "print('hi')\n").unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.python.root.is_some());
    h.state_mut().session.ws.layout.remember_size(ToolWindowId::PythonConsole, 420.0);
    h.state_mut().session.execute("tools.pythonOrDebugConsole", &json!({})).unwrap();
    settle(&mut h, "console", |a| a.session.python.console.ready);
    h.state_mut().session.execute("python.console.execute", &json!({ "code": "answer = 6 * 7\nanswer" })).unwrap();
    settle(&mut h, "result", |a| {
        a.session.python.console.running.is_none() && !a.session.python.console.vars.is_empty()
    });
    h.run_steps(3);
    assert!(h.query_by_label("answer").is_some(), "the Variables view");
    assert!(h.query_all_by_label_contains("{int} 42").count() > 0);
    assert!(h.query_all_by_label_contains(">>> answer = 6 * 7").count() > 0, "the transcript");
    h.state_mut().session.shutdown();
    assert!(!h.state().session.python.console.is_alive());
}
