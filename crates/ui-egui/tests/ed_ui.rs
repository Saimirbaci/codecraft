//! M12 track ED windows driven headlessly: Replace Structurally (dialog → preview → Replace All),
//! macros (recording indicator, name prompt), Run Inspection by Name, Locate Duplicates, Data
//! Flow, Window › Layouts, Tasks & Contexts, the analysis level in the status bar and the
//! Presentation Assistant overlay.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_ui_egui::App;

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

const BODY: &str = "fn total(items: &[Item]) -> u64 {\n    let mut sum = 0;\n    for item in items {\n        if item.enabled {\n            sum += item.price * item.count;\n        }\n    }\n    sum\n}\n";

fn project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), format!("fn a() {{\n    let x = foo().unwrap();\n    use_it(x);\n}}\n\nfn use_it(v: u8) {{\n    let w = v;\n}}\n\n{BODY}")).unwrap();
    std::fs::write(dir.join("src/b.rs"), format!("fn b() {{ bar.unwrap(); }}\n\n{}", BODY.replace("total", "amount"))).unwrap();
}

#[test]
fn ed_windows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    project(&root);
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.isolate_home(&root.join(".home"));

    // Replace Structurally: the dialog, then the preview, then Replace All.
    h.state_mut().session.execute("edit.find.replaceStructurally", &json!({})).unwrap();
    {
        let d = h.state_mut().session.ssr.dialog.as_mut().unwrap();
        d.search = "$e$.unwrap()".into();
        d.replace = "$e$?".into();
        d.language = "rust".into();
    }
    h.run_steps(2);
    assert!(h.query_by_label_contains("$e$").is_some(), "the variables grid lists $e$");
    h.get_by_label("Find and Preview").click();
    h.run_steps(2);
    settle(&mut h, "replacements", |a| a.session.ssr.results.as_ref().is_some_and(|r| r.done));
    h.run_steps(2);
    assert!(h.query_by_label_contains("Replace All (2)").is_some());
    h.get_by_label("Replace All (2)").click();
    h.run_steps(2);
    let a = std::fs::read_to_string(root.join("src/a.rs")).unwrap();
    assert!(a.contains("let x = foo()?;"), "{a}");
    assert!(std::fs::read_to_string(root.join("src/b.rs")).unwrap().contains("bar?;"));

    // Macros: the status bar shows the recording; stopping asks for a name.
    let s = &mut h.state_mut().session;
    s.execute("file.open", &json!({ "path": root.join("src/b.rs") })).unwrap();
    s.execute("edit.macros.startMacroRecording", &json!({})).unwrap();
    s.execute("editor.type", &json!({ "text": "// " })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Recording macro").is_some());
    h.get_by_label_contains("Recording macro").click();
    h.run_steps(2);
    assert!(h.query_by_label_contains("1 step(s) recorded").is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().session.macros.name_prompt.is_none());
    assert!(h.state().session.macros.last.is_some());

    // Configure Current File Analysis: the popup and the status bar.
    h.state_mut().session.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({})).unwrap();
    h.run_steps(2);
    h.get_by_label("Essential").click();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Highlighting: Essential").is_some());

    // Run Inspection by Name lists lints.
    h.state_mut().session.execute("code.analyzeCode.runInspectionByName", &json!({})).unwrap();
    h.state_mut().session.analysis.by_name.as_mut().unwrap().query = "needless_return".into();
    h.run_steps(2);
    assert!(h.query_by_label_contains("clippy::needless_return").is_some());
    h.state_mut().session.execute("analysis.dismiss", &json!({})).unwrap();

    // Locate Duplicates: the renamed copy of `total` is found.
    h.state_mut().session.execute("code.analyzeCode.locateDuplicates", &json!({ "minTokens": 20, "sync": true })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("1 duplicated fragment(s) in 2 file(s)").is_some());
    assert!(h.query_by_label_contains("2 occurrences").is_some());
    h.state_mut().session.execute("duplicates.close", &json!({})).unwrap();

    // Data Flow from Here on `x`.
    let a = std::fs::read_to_string(root.join("src/a.rs")).unwrap();
    let offset = a[..a.find("x = foo").unwrap()].chars().count();
    h.state_mut()
        .session
        .execute("code.analyzeCode.dataFlowFromHere", &json!({ "path": root.join("src/a.rs"), "offset": offset, "sync": true }))
        .unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Data Flow from Here").is_some());
    assert!(h.query_by_label_contains("use_it(…x…) argument #1").is_some());
    assert!(h.query_by_label_contains("parameter 'v' of use_it").is_some());
    h.state_mut().session.execute("dataFlow.close", &json!({})).unwrap();

    // Window › Layouts: save the current layout under a name.
    h.state_mut().session.execute("window.layouts.saveCurrentLayoutAsNew", &json!({})).unwrap();
    h.state_mut().session.layouts.dialog.as_mut().unwrap().name = "Review".into();
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert_eq!(h.state_mut().session.layouts.names(), vec!["Review".to_string()]);

    // Tasks: open a task, see it in the status bar with its timer.
    h.state_mut().session.execute("tools.tasksContexts.openTask", &json!({ "summary": "Speed up parsing" })).unwrap();
    h.state_mut().session.execute("tools.tasksContexts.timeTracking.startTimer", &json!({})).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Speed up parsing ⏱").is_some());
    h.state_mut().session.execute("tools.tasksContexts.switchTask", &json!({})).unwrap();
    h.run_steps(2);
    h.get_by_label("Default task").click();
    h.run_steps(2);
    assert_eq!(h.state().session.tasks.file.active, "default");

    // An inline completion proposal renders as ghost text and Tab-equivalent inserts it.
    let s = &mut h.state_mut().session;
    s.execute("file.open", &json!({ "path": root.join("src/a.rs") })).unwrap();
    let eid = s.ws.active_editor().unwrap();
    let doc = s.ws.active().unwrap().1;
    let revision = doc.doc.revision();
    s.ws.set_selections(eid, codecraft_engine::text::SelectionSet::caret(0));
    s.inline.proposal = Some(codecraft_engine::inline_completion::Proposal { editor: eid, offset: 0, revision, text: "// ghost\n".into() });
    h.run_steps(2);
    assert_eq!(
        codecraft_engine::inline_completion::ghost_text(&h.state().session, eid),
        Some((0, "// ghost …".to_string()))
    );
    h.state_mut().session.execute("code.codeCompletion.insertInlineProposal", &json!({})).unwrap();
    h.run_steps(1);
    assert!(h.state().session.ws.active().unwrap().1.doc.text().starts_with("// ghost\n"));

    // Presentation Assistant: the invoked action is announced.
    h.state_mut().session.execute("view.appearance.presentationAssistant", &json!({})).unwrap();
    h.state_mut().session.execute("edit.selectAll", &json!({})).ok();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Select All").is_some());
}
