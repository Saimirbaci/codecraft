//! The Inspection Results tool window driven headlessly: a fake linter's findings show as a tree,
//! clicking a problem shows its description and quick-fix, the fix rewrites the file, the severity
//! filter hides rows; the scope dialog and the stack trace windows open and link to the project.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::inspections::Tool;
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

#[test]
fn inspection_results_tree_fix_filter_and_stack_trace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let app_py = root.join("app.py");
    std::fs::write(&app_py, "import os\nimport sys\n\nprint(sys.argv)\nx = 1\n").unwrap();
    // A stand-in for `ruff check --output-format=json`: one fixable and one unfixable problem.
    let report = json!([
        {
            "code": "F401", "filename": app_py, "message": "`os` imported but unused",
            "location": {"row": 1, "column": 8}, "end_location": {"row": 1, "column": 10},
            "fix": {"applicability": "safe", "message": "Remove unused import: `os`",
                    "edits": [{"content": "", "location": {"row": 1, "column": 1}, "end_location": {"row": 2, "column": 1}}]},
            "url": "https://docs.astral.sh/ruff/rules/unused-import"
        },
        {
            "code": "E999", "filename": app_py, "message": "SyntaxError: imaginary",
            "location": {"row": 5, "column": 1}, "end_location": {"row": 5, "column": 2}, "fix": null
        }
    ]);
    let fake = root.join("fake-ruff.sh");
    std::fs::write(&fake, format!("#!/bin/sh\ncat <<'JSON'\n{report}\nJSON\n")).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1100.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.inspections.programs.insert(Tool::Ruff, fake.clone());
    h.state_mut().session.ws.layout.remember_size(ToolWindowId::InspectionResults, 520.0);

    // The scope dialog from the menu command.
    h.state_mut().session.execute("code.inspectCode", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Specify Inspection Scope").is_some());
    assert!(h.query_all_by_label_contains("Whole project").count() > 0);
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.state().session.inspections.dialog.is_none());

    h.state_mut().session.execute("code.inspectCode", &json!({ "scope": "project", "tools": ["ruff"] })).unwrap();
    settle(&mut h, "inspection", |a| !a.session.inspections.busy());
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::InspectionResults));
    assert!(h.query_all_by_label_contains("F401 unused-import").count() > 0, "the inspection group");
    assert!(h.query_all_by_label_contains("app.py").count() > 0, "the file node");
    let row = "`os` imported but unused  (1:8)";
    assert!(h.query_by_label(row).is_some(), "the occurrence row");

    // Click a problem: its description and quick-fix appear.
    h.get_by_label(row).click();
    h.run_steps(3);
    assert_eq!(h.state().session.inspections.selected, Some(0));
    assert!(h.query_all_by_label_contains("Open documentation").count() > 0);
    if let Ok(dir) = std::env::var("CODECRAFT_SNAPSHOT_DIR") {
        h.render().unwrap().save(format!("{dir}/inspections.png")).unwrap();
    }
    h.get_by_label("💡 Remove unused import: `os`").click();
    h.run_steps(3);
    assert_eq!(
        std::fs::read_to_string(&app_py).unwrap(),
        "import sys\n\nprint(sys.argv)\nx = 1\n",
        "status: {}",
        h.state().session.status
    );
    assert!(h.query_all_by_label_contains("F401 unused-import").count() == 0, "fixed problems leave the tree");
    let syntax = "SyntaxError: imaginary  (4:1)";
    assert!(h.query_by_label(syntax).is_some(), "the other problem moved up a line with the edit");

    // The severity filter hides errors.
    h.get_by_label("✖ 1").click();
    h.run_steps(3);
    assert!(h.query_by_label(syntax).is_none());
    assert!(h.query_by_label("All problems are filtered out.").is_some());

    // Analyze Stack Trace: the dialog is prefilled from the clipboard; the result links to the file.
    h.state_mut().session.clipboard =
        Some(("Traceback (most recent call last):\n  File \"app.py\", line 3, in <module>\nNameError: x".into(), false));
    h.state_mut().session.execute("code.analyzeStackTraceOrThreadDump", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Analyze Stack Trace").is_some());
    h.get_by_label("Analyze").click();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("Python traceback").count() > 0);
    h.get_by_label("  File \"app.py\", line 3, in <module>").click();
    h.run_steps(3);
    let (e, d) = h.state().session.ws.active().unwrap();
    assert_eq!(d.path.as_deref(), Some(app_py.as_path()));
    assert_eq!(d.doc.char_to_line(e.selections.primary().head), 2);
}
