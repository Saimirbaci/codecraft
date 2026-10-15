//! Project windows: File › Open in New Window gives a project its own session and window;
//! settings are shared; Help opens the guide.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::Popup;
use codecraft_ui_egui::App;

/// The engine keeps the number of project windows in a process-wide counter that every App
/// frame updates, so the tests of this binary run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn project(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
}

#[test]
fn open_in_new_window_switch_and_share_settings() {
    let _serial = serial();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    project(a.path(), "alpha");
    project(b.path(), "beta");
    let open = a.path().to_path_buf();
    let mut h =
        Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    assert_eq!(h.state().project_window_count(), 1);
    assert!(h.state_mut().session.execute("window.nextProjectWindow", &Value::Null).is_err(), "needs two windows");

    h.state_mut().session.execute("file.openInNewWindow", &json!({ "path": b.path() })).unwrap();
    h.run_steps(3);
    let app = h.state();
    assert_eq!(app.project_window_count(), 2);
    let second = app.project_window_session(0).unwrap();
    assert_eq!(second.ws.project.as_ref().map(|p| p.root.clone()), Some(b.path().to_path_buf()));
    assert_ne!(
        app.session.ws.project.as_ref().map(|p| p.root.clone()),
        second.ws.project.as_ref().map(|p| p.root.clone()),
        "each window has its own project"
    );
    assert!(!second.lsp_enabled, "inherits the main window's language server switch");

    // Next/Previous are live with two windows and never panic.
    h.state_mut().session.execute("window.nextProjectWindow", &Value::Null).unwrap();
    h.run_steps(2);
    h.state_mut().session.execute("window.previousProjectWindow", &Value::Null).unwrap();
    h.run_steps(2);

    // A settings change in the main window reaches the other one.
    let wrap = !h.state().session.ws.settings.soft_wrap;
    h.state_mut().session.ws.settings.soft_wrap = wrap;
    h.run_steps(2);
    assert_eq!(h.state().project_window_session(0).unwrap().ws.settings.soft_wrap, wrap);
}

#[test]
fn help_shows_the_guide() {
    let _serial = serial();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(|_| App::ephemeral(None));
    h.run_steps(1);
    h.state_mut().session.execute("help.help", &Value::Null).unwrap();
    h.run_steps(2);
    assert!(matches!(&h.state().session.popup, Some(Popup::Message { title, .. }) if title == "CodeCraft Help"));
    assert!(h.query_by_label_contains("Getting around").is_some(), "the guide renders as Markdown");
    h.get_by_label("Close").click_accesskit();
    h.run_steps(3);
    assert!(h.state().session.popup.is_none());
}
