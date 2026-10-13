//! M10 track G in the real shell, headless: Zoom IDE (popup and UI zoom), Presentation Mode
//! (bars and tool windows hidden, larger editor font), status bar widget toggles, Compact Mode,
//! the top navigation bar, and the in-window menus after the catalog took over EXTRA items
//! (no label twice in a submenu, checkmarks on toggles).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::Effect;
use codecraft_ui_egui::App;

fn project(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, "fn main() {\n    let total = 1;\n    println!(\"{}\", total);\n}\n").unwrap();
    main
}

fn harness(open: PathBuf) -> Harness<'static, App> {
    Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(open)))
}

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn run(h: &mut Harness<'_, App>, id: &str, p: Value) -> Value {
    let v = h.state_mut().session.execute(id, &p).unwrap();
    h.run_steps(3);
    v
}

fn ready(dir: &Path) -> Harness<'static, App> {
    let main = project(dir);
    let mut h = harness(main);
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(3);
    h
}

#[test]
fn zoom_ide_popup_scales_the_ui() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready(dir.path());
    run(&mut h, "view.appearance.zoomIde", Value::Null);
    assert!(h.query_by_label("100%").is_some(), "the popup shows the current zoom");
    h.get_by_label(" + ").click();
    h.run_steps(3);
    assert_eq!(h.state().session.ws.settings.ide_zoom, 1.1);
    assert!(h.query_by_label("110%").is_some());
    assert!((h.ctx.zoom_factor() - 1.1).abs() < 1e-3, "zoom factor {}", h.ctx.zoom_factor());
    h.get_by_label("Reset").click();
    h.run_steps(3);
    assert!((h.ctx.zoom_factor() - 1.0).abs() < 1e-3);
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(!h.state().session.appearance.zoom_popup);
    assert!(h.query_by_label("Zoom IDE").is_none());
}

#[test]
fn presentation_mode_hides_bars_and_tool_windows() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready(dir.path());
    // The status bar shows the caret; the Project tool window is open.
    assert!(h.query_by_label("1:1").is_some());
    assert!(h.query_by_label("Project").is_some());
    run(&mut h, "view.appearance.enterPresentationMode", Value::Null);
    let s = &h.state().session;
    assert!(s.appearance.presentation && s.ws.layout.all_hidden);
    assert!(h.query_by_label("1:1").is_none(), "status bar hidden");
    assert_eq!(codecraft_engine::appearance::chrome(s).editor_font_size, s.ws.settings.presentation_font_size);
    // The full-screen request went to the window (effects are drained by the frame).
    assert!(!s.effects.contains(&Effect::SetFullScreen(true)));
    assert!(s.appearance.full_screen);
    run(&mut h, "view.appearance.enterPresentationMode", Value::Null);
    assert!(!h.state().session.ws.layout.all_hidden);
    assert!(h.query_by_label("1:1").is_some());
}

#[test]
fn status_bar_widgets_toggle_their_labels() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready(dir.path());
    assert!(h.query_by_label("1:1").is_some());
    assert!(h.query_by_label("4 spaces").is_some());
    run(&mut h, "view.appearance.statusBarWidgets.lineColumnNumber", Value::Null);
    assert!(h.query_by_label("1:1").is_none());
    run(&mut h, "view.appearance.statusBarWidgets.indentation", json!({ "on": false }));
    assert!(h.query_by_label("4 spaces").is_none());
    run(&mut h, "view.appearance.statusBarWidgets.lineColumnNumber", Value::Null);
    assert!(h.query_by_label("1:1").is_some());
    // Column selection mode shows its widget.
    run(&mut h, "edit.columnSelectionMode", json!({ "on": true }));
    assert!(h.query_by_label("Column").is_some());
    run(&mut h, "view.appearance.statusBarWidgets.editorSelectionMode", Value::Null);
    assert!(h.query_by_label("Column").is_none());
    // The whole status bar.
    run(&mut h, "view.appearance.statusBar", Value::Null);
    assert!(h.query_by_label("1:1").is_none());
    run(&mut h, "view.appearance.statusBar", Value::Null);
    // Memory Indicator: sampled on a worker thread (skipped where `ps` is missing).
    run(&mut h, "view.appearance.statusBarWidgets.memoryIndicator", Value::Null);
    let start = Instant::now();
    while h.state().session.appearance.memory_mb.is_none() && start.elapsed() < Duration::from_secs(10) {
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(mb) = h.state().session.appearance.memory_mb {
        h.run_steps(2);
        assert!(h.query_by_label(&format!("{mb} MB")).is_some());
    }
}

#[test]
fn navigation_bar_moves_between_status_bar_and_top() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready(dir.path());
    run(&mut h, "editor.setSelection", json!({ "anchor": 20 }));
    h.state_mut().session.status.clear();
    h.run_steps(2);
    // In the status bar: project › src › main.rs › main (the tab and the Project view show the
    // file name too).
    let with_path = h.query_all_by_label("main.rs").count();
    run(&mut h, "view.appearance.navigationBar.dontShow", Value::Null);
    h.state_mut().session.status.clear();
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("main.rs").count(), with_path - 1);
    run(&mut h, "view.appearance.navigationBar.top", Value::Null);
    h.state_mut().session.status.clear();
    h.run_steps(2);
    assert!(h.query_by_label("src").is_some(), "the top bar shows the path");
    // Compact Mode and the toolbar are plain switches the frame follows.
    run(&mut h, "view.appearance.compactMode", Value::Null);
    run(&mut h, "view.appearance.toolbar", Value::Null);
    run(&mut h, "view.appearance.toolWindowBars", Value::Null);
    let set = &h.state().session.ws.settings;
    assert!(set.compact_mode && !set.show_toolbar && !set.show_tool_window_bars);
}

#[test]
fn in_window_menus_show_catalog_items_once_with_checkmarks() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready(dir.path());
    h.get_by_label("Edit").click();
    h.run_steps(2);
    h.get_by_label_contains("Bookmarks").hover();
    h.run_steps(3);
    assert_eq!(h.query_all_by_label_contains("Next Line Bookmark").count(), 1);
    assert_eq!(h.query_all_by_label_contains("Show Line Bookmarks").count(), 1);
    assert_eq!(h.query_all_by_label_contains("Toggle Bookmark").count(), 2, "with and without mnemonic");
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    h.get_by_label("View").click();
    h.run_steps(2);
    h.get_by_label_contains("Appearance").hover();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Zoom IDE (Current: 100%)").is_some());
    assert_eq!(h.query_all_by_label_contains("Enter Presentation Mode").count(), 1);
    assert!(h.query_by_label_contains("Compact Mode").is_some());
}

#[test]
fn find_usages_settings_dialog_saves_the_options() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = ready(dir.path());
    run(&mut h, "edit.findUsages.findUsagesSettings", Value::Null);
    assert!(h.query_by_label("Find Usages Settings").is_some());
    h.get_by_label("Current file").click();
    h.run_steps(2);
    h.get_by_label("Include usages in test code").click();
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(3);
    let s = &h.state().session;
    assert!(s.usages.settings_dialog.is_none());
    assert_eq!(s.ws.settings.find_usages.scope, codecraft_workspace::UsageScope::CurrentFile);
    assert!(!s.ws.settings.find_usages.include_tests);
    assert!(h.query_by_label("Find Usages Settings").is_none());
    // Highlight Usages paints, Escape (in the editor) removes.
    let offset = h.state().session.ws.active().unwrap().1.doc.text().find("total").unwrap();
    run(&mut h, "editor.setSelection", json!({ "anchor": offset }));
    run(&mut h, "edit.findUsages.highlightUsagesInFile", Value::Null);
    assert!(h.state().session.usages.highlights.as_ref().is_some_and(|u| u.ranges.len() == 2));
    run(&mut h, "editor.escape", Value::Null);
    assert!(h.state().session.usages.highlights.is_none());
}
