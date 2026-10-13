//! Track X's windows driven headlessly: F3 and the mnemonic chooser (a key press picks the
//! mnemonic), the Bookmarks and TODO tool windows, the Local History window with its diff, and
//! the scratch language chooser.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use egui::{Event, Key};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

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
fn bookmarks_todo_history_and_scratches() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.rs"), "fn a() {}\n// TODO: tidy up\nfn b() {}\n").unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.scratch.dir_override = Some(root.join("scratches-home"));
    h.state_mut().session.execute("file.open", &json!({ "path": root.join("a.rs") })).unwrap();
    h.run_steps(3);

    // F3 on the caret line, then the Bookmarks tool window lists it.
    h.state_mut().session.execute("edit.bookmarks.toggle", &json!({})).unwrap();
    h.state_mut().session.execute("view.toolWindows.bookmarks", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("1: fn a() {}").is_some());

    // Alt+F3 on line 3 opens the chooser; typing "5" picks the mnemonic.
    h.state_mut().session.execute("edit.bookmarks.toggleWithMnemonic", &json!({ "line": 2 })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Add Mnemonic Bookmark").is_some());
    h.event(Event::Text("5".into()));
    h.run_steps(3);
    let s = &h.state().session;
    assert!(s.bookmarks.mnemonic_dialog.is_none());
    assert_eq!(s.bookmarks.lines_in(&root.join("a.rs")), vec![(0, None), (2, Some('5'))]);
    assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap().lines().count(), 3, "the key did not reach the editor");

    // Cmd+F3 popup, Escape closes it.
    h.state_mut().session.execute("edit.bookmarks.show", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("[5] a.rs:3").is_some());
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(!h.state().session.bookmarks.popup);

    // The TODO tool window scans the project.
    h.state_mut().session.execute("view.toolWindows.todo", &json!({})).unwrap();
    settle(&mut h, "todo scan", |a| !a.session.todo.busy() && a.session.todo.count() == 1);
    h.run_steps(3);
    assert!(h.query_by_label_contains("TODO: tidy up").is_some());
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Todo));

    // Edit, save, and the Local History window shows the diff.
    h.state_mut().session.execute("editor.type", &json!({ "text": "// new\n" })).unwrap();
    h.state_mut().session.save_all().unwrap();
    h.state_mut().session.execute("file.localHistory.showHistory", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("Saved").next().is_some());
    h.state_mut().session.execute("localHistory.select", &json!({ "index": 1 })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Revert").is_some());
    h.get_by_label("Revert").click();
    h.run_steps(3);
    assert_eq!(h.state().session.ws.active().unwrap().1.doc.text(), "fn a() {}\n// TODO: tidy up\nfn b() {}\n");
    h.state_mut().session.execute("localHistory.close", &json!({})).unwrap();

    // Put Label dialog and the scratch chooser.
    h.state_mut().session.execute("file.localHistory.putLabel", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Put Label").is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().session.local_history.label_dialog.is_none());

    h.state_mut().session.execute("file.new.scratchFile", &json!({})).unwrap();
    h.run_steps(3);
    h.event(Event::Text("pyth".into()));
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(3);
    let s = &h.state().session;
    assert!(s.scratch.chooser.is_none());
    let path = s.ws.active().unwrap().1.path.clone().unwrap();
    assert_eq!(path, root.join("scratches-home/scratch.py"));
}
