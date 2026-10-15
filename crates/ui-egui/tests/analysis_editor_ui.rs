//! Analysis in the editor (M15 track EA) driven through the UI: a save runs Ruff (the real one
//! when installed, else a stand-in), the finding is underlined, Alt+Enter on the underline
//! offers the analyzer's fix and applies it; Settings › Languages & Frameworks › Python has the
//! interpreter picker, the language server and Ruff rows; Settings › Editor › Inspections has
//! the analysis-on-save switch.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::inspections::Tool;
use codecraft_ui_egui::App;
use codecraft_workspace::ProblemSource;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };
const ALT: Modifiers = Modifiers { alt: true, ctrl: false, shift: false, mac_cmd: false, command: false };

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

fn which(name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("/usr/bin/which").arg(name).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())).filter(|p| p.is_file())
}

/// The real ruff, else a stand-in reporting F401 on `os` (line 1) with ruff's fix.
fn ruff(dir: &Path, file: &Path) -> PathBuf {
    if let Some(r) = which("ruff") {
        return r;
    }
    let report = json!([{
        "code": "F401", "filename": file, "message": "`os` imported but unused",
        "location": {"row": 1, "column": 8}, "end_location": {"row": 1, "column": 10},
        "fix": {"applicability": "safe", "message": "Remove unused import: `os`",
                "edits": [{"content": "", "location": {"row": 1, "column": 1}, "end_location": {"row": 2, "column": 1}}]}
    }]);
    let p = dir.join("fake-ruff");
    std::fs::write(&p, format!("#!/bin/sh\ncat <<'JSON'\n{report}\nJSON\nexit 1\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn text(h: &Harness<'_, App>) -> String {
    h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

fn underlines(h: &Harness<'_, App>, path: &Path) -> Vec<usize> {
    h.state()
        .session
        .ws
        .highlighted_problems_for(path)
        .into_iter()
        .filter(|p| p.source == ProblemSource::Analysis)
        .map(|p| p.start.line)
        .collect()
}

#[test]
fn alt_enter_applies_a_ruff_fix_from_an_underline() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let app_py = root.join("app.py");
    std::fs::write(&app_py, "import os\nimport sys\n\nprint(sys.argv)\n").unwrap();
    let program = ruff(&base, &app_py);

    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe({
        let root = root.clone();
        let home = base.join("home");
        move |_| {
            let mut app = App::ephemeral(Some(root));
            app.session.isolate_home(&home);
            app.session.analysis.editor.on_save.debounce = Duration::ZERO;
            app
        }
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.inspections.programs.insert(Tool::Ruff, program);
    h.state_mut().session.execute("file.open", &json!({ "path": app_py })).unwrap();
    h.run_steps(3);

    // Type at the end and save with Cmd+S: Ruff runs in the background.
    let len = h.state().session.ws.active().unwrap().1.doc.len_chars();
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": len })).unwrap();
    h.event(Event::Text("x = 1\n".into()));
    h.run_steps(1);
    h.key_press_modifiers(CMD, Key::S);
    settle(&mut h, "ruff on save", |a| {
        !a.session.analysis.editor.on_save.busy() && !a.session.analysis.editor.of(&a.session.ws.active().unwrap().1.path.clone().unwrap()).is_empty()
    });
    h.run_steps(2);
    assert_eq!(underlines(&h, &app_py), vec![0], "the unused import is underlined");
    // The inspection widget counts it with the editor's other problems.
    assert!(h.query_by_label_contains("⚠ 1").is_some());

    // Caret on `os`, Alt+Enter: the analyzer's fix and suppression.
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 8 })).unwrap();
    h.run_steps(1);
    h.key_press_modifiers(ALT, Key::Enter);
    h.run_steps(3);
    let titles: Vec<String> = match &h.state().session.popup {
        Some(codecraft_engine::Popup::Actions { items, .. }) => items.iter().map(|i| i.title.clone()).collect(),
        other => panic!("Alt+Enter opened no context actions: {other:?}"),
    };
    assert_eq!(titles.first().map(String::as_str), Some("Ruff: Remove unused import: `os`"), "{titles:?}");
    assert!(titles.iter().any(|t| t == "Suppress with # noqa: F401"), "{titles:?}");
    // Enter applies the selected (first) action, like a click on it.
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(text(&h), "import sys\n\nprint(sys.argv)\nx = 1\n", "status: {}", h.state().session.status);
    assert!(underlines(&h, &app_py).is_empty(), "the fixed finding's underline is gone");
}

#[test]
fn python_and_inspections_settings_pages() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.py"), "print(1)\n").unwrap();
    // An interpreter to add by path.
    let python = base.join("py/bin/python3");
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    std::fs::write(&python, "#!/bin/sh\necho Python 3.12.1\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 900.0)).build_eframe({
        let root = root.clone();
        let home = base.join("home");
        move |_| {
            let mut app = App::ephemeral(Some(root));
            app.session.isolate_home(&home);
            app
        }
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.execute("app.settings", &json!({ "page": codecraft_engine::settings::PAGE_PYTHON })).unwrap();
    h.run_steps(3);
    for label in ["Python Interpreter", "Language Server", "Ruff", "Formatting", "Detect", "Create Virtualenv…", "Add Interpreter"] {
        assert!(h.query_by_label(label).is_some(), "{label} on the Python page");
    }
    if h.state().session.python_server_command().is_none() {
        assert!(h.query_by_label("Install basedpyright").is_some(), "no Python server: the install button");
    }
    // Add an interpreter from a path: the project uses it at once.
    h.get_by_label("Interpreter path").click();
    h.run_steps(1);
    h.event(Event::Text(python.display().to_string()));
    h.run_steps(1);
    h.get_by_label("Add Interpreter").click();
    h.run_steps(3);
    assert_eq!(h.state().session.python.interpreter.as_deref(), Some(python.as_path()), "{}", h.state().session.status);

    // Inspections: analysis on save is a setting of the dialog (OK applies it).
    h.state_mut().session.execute("settings.dialog.page", &json!({ "page": codecraft_engine::settings::PAGE_INSPECTIONS })).unwrap();
    h.run_steps(3);
    assert!(h.state().session.ws.settings.analysis_on_save);
    h.get_by_label("Analyze files on save").click();
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert!(!h.state().session.ws.settings.analysis_on_save);
}
