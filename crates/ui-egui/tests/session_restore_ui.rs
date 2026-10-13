//! Session restore (M10 track A) through the real shell: an app with a split project and a
//! second project window saves its state; a new app built from that state reopens both
//! projects with their tabs, splits and focus, and paints them. Nothing touches the real config
//! directory (`App::restoring` never writes the app state file; sessions go into the temp
//! projects).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use egui_kittest::Harness;
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::workspace::SplitNode;
use codecraft_ui_egui::{App, PersistedState};

fn project(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    root
}

fn names(s: &Session) -> Vec<Vec<String>> {
    s.ws.group_ids()
        .iter()
        .map(|g| {
            s.ws.group(*g)
                .unwrap()
                .tabs
                .iter()
                .map(|t| {
                    let d = s.ws.doc(s.ws.editor(*t).unwrap().doc).unwrap();
                    d.path.as_ref().unwrap().file_name().unwrap().to_string_lossy().into_owned()
                })
                .collect()
        })
        .collect()
}

fn harness(persisted: PersistedState, cfg: PathBuf) -> Harness<'static, App> {
    Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_eframe(move |_| App::restoring(persisted.clone(), cfg.clone()))
}

#[test]
fn projects_windows_tabs_and_splits_come_back() {
    let tmp = tempfile::tempdir().unwrap();
    let alpha = project(tmp.path(), "alpha");
    let beta = project(tmp.path(), "beta");
    let cfg = tmp.path().join("cfg");

    // beta was last left with lib.rs open (saved by an earlier session).
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": beta })).unwrap();
    s.execute("file.open", &json!({ "path": beta.join("src/lib.rs") })).unwrap();
    s.execute("session.save", &Value::Null).unwrap();
    s.shutdown();

    let mut h = harness(PersistedState::default(), cfg.clone());
    h.run_steps(2);
    assert!(h.state().session.ws.project.is_none(), "nothing to reopen yet");
    {
        let s = &mut h.state_mut().session;
        s.execute("file.open", &json!({ "path": alpha })).unwrap();
        s.execute("file.open", &json!({ "path": alpha.join("src/main.rs") })).unwrap();
        s.execute("file.open", &json!({ "path": alpha.join("src/lib.rs") })).unwrap();
        s.execute("window.editorTabs.splitAndMoveDown", &Value::Null).unwrap();
        s.execute("file.openInNewWindow", &json!({ "path": beta })).unwrap();
    }
    h.run_steps(3);
    assert_eq!(h.state().project_window_count(), 2);
    assert_eq!(names(h.state().project_window_session(0).unwrap()), vec![vec!["lib.rs"]], "beta restored on open");
    let state = h.state_mut().capture_state();
    assert_eq!(state.last_project.as_deref(), Some(alpha.as_path()));
    assert_eq!(state.open_windows, vec![beta.clone()]);
    assert!(alpha.join(".codecraft/workspace.json").is_file());
    h.state_mut().session.shutdown();
    drop(h);

    // A new app from that state: both projects, their tabs, the split and the focus.
    let mut h = harness(state.clone(), cfg.clone());
    h.run_steps(3);
    let app = h.state();
    let main = &app.session;
    assert_eq!(main.ws.project.as_ref().map(|p| p.root.clone()), Some(alpha.clone()));
    assert_eq!(names(main), vec![vec!["main.rs"], vec!["lib.rs"]]);
    assert!(matches!(main.ws.split_layout(), SplitNode::Split { .. }));
    let (_, d) = main.ws.active().unwrap();
    assert!(d.path.as_ref().unwrap().ends_with("src/lib.rs"), "the focused group is focused again");
    assert_eq!(app.project_window_count(), 2);
    let second = app.project_window_session(0).unwrap();
    assert_eq!(second.ws.project.as_ref().map(|p| p.root.clone()), Some(beta.clone()));
    assert_eq!(names(second), vec![vec!["lib.rs"]]);
    h.state_mut().session.shutdown();
    drop(h);

    // With "Reopen projects on startup" off, nothing comes back.
    let mut off = state;
    let mut settings = off.settings.clone().unwrap_or_default();
    settings.reopen_last_project = false;
    off.settings = Some(settings);
    let mut h = harness(off, cfg);
    h.run_steps(2);
    assert!(h.state().session.ws.project.is_none());
    assert_eq!(h.state().project_window_count(), 1);
    h.state_mut().session.shutdown();
}
