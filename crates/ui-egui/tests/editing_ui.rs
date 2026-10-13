//! Editor depth (track E) through the real shell: Tab expands live templates, soft wrap moves
//! the caret by visual row, the navigation bar and template chooser take the keyboard.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use serde_json::json;

use codecraft_ui_egui::App;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };

fn project(dir: &Path, main_text: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, main_text).unwrap();
    main
}

fn harness(open: PathBuf, templates: PathBuf) -> Harness<'static, App> {
    Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(open));
        app.session.editing.templates.set_path(templates);
        app
    })
}

fn text(h: &Harness<'_, App>) -> String {
    h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

fn caret(h: &Harness<'_, App>) -> usize {
    h.state().session.ws.active().map(|(e, _)| e.selections.primary().head).unwrap_or(0)
}

fn settle(h: &mut Harness<'_, App>, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out");
}

#[test]
fn tab_expands_a_live_template_and_typing_fills_mirrors() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {\n    \n}\n");
    let mut h = harness(main, dir.path().join("templates.json"));
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 16 })).unwrap();
    h.event(Event::Text("pv".into()));
    h.run_steps(1);
    h.key_press(Key::Tab);
    h.run_steps(1);
    h.event(Event::Text("n".into()));
    h.run_steps(1);
    assert_eq!(text(&h), "fn main() {\n    println!(\"n = {:?}\", n);\n}\n");
    h.key_press(Key::Tab);
    h.run_steps(1);
    assert!(h.state().session.editing.session.is_none());
    // The variable frames and breadcrumbs paint without trouble.
    h.run_steps(2);
}

#[test]
fn soft_wrap_wraps_and_moves_by_visual_row() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x ".repeat(200);
    let main = project(dir.path(), &format!("// {long}\nfn main() {{}}\n"));
    let mut h = harness(main, dir.path().join("templates.json"));
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.execute("view.toggleSoftWrap", &json!({})).unwrap();
    h.run_steps(3);
    let eid = h.state().session.ws.active_editor().unwrap();
    let width = *h.state().session.editing.wrap_widths.get(&eid).expect("the view reports its wrap width");
    assert!(width > 10 && width < 400, "{width}");
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
    h.run_steps(1);
    h.key_press(Key::ArrowDown);
    h.run_steps(1);
    // Still on the first (long) line, one visual row down.
    let c = caret(&h);
    assert!(c > 0 && c < 405, "caret {c}");
    assert_eq!(h.state().session.ws.active().unwrap().1.doc.char_to_line(c), 0);
}

#[test]
fn navigation_bar_and_template_chooser_take_the_keyboard() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {\n    let a = 1;\n}\n");
    let mut h = harness(main, dir.path().join("templates.json"));
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    h.key_press_modifiers(CMD, Key::ArrowUp);
    h.run_steps(2);
    assert!(h.state().session.editing.navbar.is_some());
    h.key_press(Key::ArrowLeft);
    h.run_steps(1);
    let bar = h.state().session.editing.navbar.clone().unwrap();
    assert_eq!(bar.segments.get(bar.selected).map(|s| s.label.as_str()), Some("src"));
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.state().session.editing.navbar.is_none());

    h.key_press_modifiers(CMD, Key::J);
    h.run_steps(2);
    assert!(h.state().session.editing.chooser.is_some());
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.state().session.editing.chooser.is_none());
    assert_eq!(text(&h), "fn main() {\n    let a = 1;\n}\n");
}
