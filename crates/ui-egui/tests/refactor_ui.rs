//! The refactoring UI driven headlessly: the preview window (exclude an entry, Apply) and the
//! file refactoring dialogs (Rename File via the dialog, Safe Delete confirmation).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::refactor::RefactorDialog;
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

fn harness(root: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| p.tree.len() > 1));
    h
}

/// `file://` URI of a temp path (no characters that need escaping).
fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[test]
fn preview_window_excludes_and_applies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(root.join("b.txt"), "beta\n").unwrap();
    let mut h = harness(&root);
    let edit = |p: &Path, text: &str| {
        json!({ "textDocument": { "uri": uri(p), "version": null },
                "edits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": text }] })
    };
    let params = json!({ "title": "Prefix", "edit": { "documentChanges": [edit(&root.join("a.txt"), "1 "), edit(&root.join("b.txt"), "2 ")] } });
    h.state_mut().session.execute("refactor.preview.open", &params).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Refactoring Preview: Prefix").is_some());
    // Uncheck the second entry, then Apply (1).
    h.get_by_label("Include b.txt (1 change)").click();
    h.run_steps(2);
    let inc: Vec<bool> =
        h.state().session.refactor.preview.as_ref().unwrap().entries.iter().map(|e| e.include).collect();
    assert_eq!(inc, [true, false], "status: {}", h.state().session.status);
    h.get_by_role_and_label(Role::Button, "Apply (1)").click();
    h.run_steps(2);
    assert!(h.state().session.refactor.preview.is_none());
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "1 alpha\n");
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "beta\n");
}

#[test]
fn file_dialogs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let a = root.join("a.txt");
    std::fs::write(&a, "alpha\n").unwrap();
    let mut h = harness(&root);

    // Rename File: the dialog proposes the current name; Refactor renames.
    h.state_mut().session.execute("refactor.renameFile", &json!({ "path": a })).unwrap();
    h.run_steps(3);
    if let Some(RefactorDialog::RenameFile { name, .. }) = &mut h.state_mut().session.refactor.dialog {
        *name = "renamed.txt".into();
    }
    h.run_steps(2);
    // (The menu bar has a "Refactor" menu too: the dialog's button comes last.)
    h.query_all_by_role_and_label(Role::Button, "Refactor").last().unwrap().click();
    h.run_steps(2);
    let renamed = root.join("renamed.txt");
    assert!(renamed.is_file() && !a.exists(), "{}", h.state().session.status);
    assert!(h.state().session.refactor.dialog.is_none());

    // Safe Delete: Delete is enabled once the usage search finished.
    h.state_mut().session.execute("refactor.safeDelete", &json!({ "path": renamed })).unwrap();
    settle(&mut h, "usages", |a| {
        matches!(&a.session.refactor.dialog, Some(RefactorDialog::SafeDelete { usages: Some(_), .. }))
    });
    h.run_steps(2);
    assert!(h.query_by_label("No usages found.").is_some());
    h.get_by_role_and_label(Role::Button, "Delete").click();
    h.run_steps(2);
    assert!(!renamed.exists());
}
