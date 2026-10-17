//! Analysis while typing (M16 track LT) driven through the UI: typing an unused import
//! underlines it without saving (Ruff on the buffer: the real one when installed, else a
//! stand-in that reads stdin), undoing the edit clears the underline, the file on disk is never
//! touched; Settings › Editor › Inspections has the per-language switches and the delay.
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

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}; live: {:?}", h.state().session.status, h.state().session.analysis.editor.live);
}

fn which(name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("/usr/bin/which").arg(name).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())).filter(|p| p.is_file())
}

/// The real ruff, else a stand-in that reports F401 on `os` (line 1) when the buffer on stdin
/// starts with `import os`.
fn ruff(dir: &Path, file: &Path) -> PathBuf {
    if let Some(r) = which("ruff") {
        return r;
    }
    let report = json!([{
        "code": "F401", "filename": file, "message": "`os` imported but unused",
        "location": {"row": 1, "column": 8}, "end_location": {"row": 1, "column": 10},
    }]);
    let p = dir.join("fake-ruff");
    std::fs::write(&p, format!("#!/bin/sh\nif head -n1 | grep -q '^import os'; then cat <<'JSON'\n{report}\nJSON\nelse echo '[]'; fi\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
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

fn idle(a: &App, path: &Path) -> bool {
    let live = &a.session.analysis.editor.live;
    let rev = a.session.ws.active().map(|(_, d)| d.doc.revision());
    !live.pending(path) && !live.running(path) && live.computed_for(path) == rev
}

#[test]
fn typing_underlines_and_undo_clears_without_saving() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let app_py = root.join("app.py");
    let original = "import sys\n\nprint(sys.argv)\n";
    std::fs::write(&app_py, original).unwrap();
    let program = ruff(&base, &app_py);

    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe({
        let root = root.clone();
        let home = base.join("home");
        move |_| {
            let mut app = App::ephemeral(Some(root));
            app.session.isolate_home(&home);
            app.session.ws.settings.analysis_while_typing_delay_ms = 150;
            app
        }
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.inspections.programs.insert(Tool::Ruff, program);
    h.state_mut().session.execute("file.open", &json!({ "path": app_py })).unwrap();
    settle(&mut h, "the analysis on open", |a| idle(a, &app_py));
    assert!(underlines(&h, &app_py).is_empty(), "clean when it opens");

    // Type an unused import at the top: underlined after the delay, without a save.
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
    h.run_steps(1);
    h.event(Event::Text("import os\n".into()));
    h.run_steps(1);
    settle(&mut h, "the underline while typing", |a| idle(a, &app_py) && !a.session.analysis.editor.of(&app_py).is_empty());
    h.run_steps(2);
    assert_eq!(underlines(&h, &app_py), vec![0], "the unused import is underlined");
    assert!(h.query_by_label_contains("⚠ 1").is_some(), "the inspection widget counts it");
    assert!(h.state().session.ws.active().unwrap().1.doc.is_modified(), "never saved");
    assert_eq!(std::fs::read_to_string(&app_py).unwrap(), original);

    // Undo the typing: the underline goes (the edit removed its text; the re-run agrees).
    for _ in 0..4 {
        if h.state().session.ws.active().unwrap().1.doc.text() == original {
            break;
        }
        h.key_press_modifiers(CMD, Key::Z);
        h.run_steps(1);
    }
    assert_eq!(h.state().session.ws.active().unwrap().1.doc.text(), original);
    settle(&mut h, "the re-analysis after undo", |a| idle(a, &app_py));
    h.run_steps(2);
    assert!(underlines(&h, &app_py).is_empty(), "{:?}", underlines(&h, &app_py));
    assert_eq!(std::fs::read_to_string(&app_py).unwrap(), original);
}

#[test]
fn inspections_page_has_the_analyze_while_typing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("app.py"), "print(1)\n").unwrap();
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
    h.state_mut().session.execute("app.settings", &json!({ "page": codecraft_engine::settings::PAGE_INSPECTIONS })).unwrap();
    h.run_steps(3);
    for label in ["Analyze while typing:", "Python files", "C/C++ files", "SQL files and consoles", "Delay:"] {
        assert!(h.query_by_label(label).is_some(), "{label} on the Inspections page");
    }
    assert!(h.state().session.ws.settings.analysis_while_typing_cpp);
    h.get_by_label("C/C++ files").click();
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert!(!h.state().session.ws.settings.analysis_while_typing_cpp);
    assert!(h.state().session.ws.settings.analysis_while_typing_python);
    // Power Save mode says it pauses the analysis.
    h.state_mut().session.ws.settings.power_save_mode = true;
    h.state_mut().session.execute("app.settings", &json!({ "page": codecraft_engine::settings::PAGE_INSPECTIONS })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("analysis while typing is paused").is_some());
}
