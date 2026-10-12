//! Drive the real app shell headlessly: key events go through the keymap to engine commands.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;

use codecraft_engine::Popup;
use codecraft_ui_egui::App;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };
const SHIFT_CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: true, mac_cmd: true, command: true };

fn project(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    main
}

fn harness(open: PathBuf) -> Harness<'static, App> {
    Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(open)))
}

fn text(h: &Harness<'_, App>) -> String {
    h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
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
fn typing_and_editing_shortcuts() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path());
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    assert!(text(&h).starts_with("fn main"));

    h.event(Event::Text("// hello\n".into()));
    h.run_steps(1);
    assert!(text(&h).starts_with("// hello\nfn main"), "{}", text(&h));

    h.key_press_modifiers(CMD, Key::Z);
    h.run_steps(1);
    assert!(text(&h).starts_with("fn main"), "undo: {}", text(&h));

    h.key_press_modifiers(CMD, Key::D);
    h.run_steps(1);
    assert!(text(&h).starts_with("fn main() {\nfn main() {\n"), "duplicate: {}", text(&h));

    // The caret moved to the duplicate (RustRover behaviour), so that line gets commented.
    h.key_press_modifiers(CMD, Key::Slash);
    h.run_steps(1);
    assert!(text(&h).starts_with("fn main() {\n// fn main() {\n"), "comment: {}", text(&h));

    // Cmd+S saves to disk.
    h.key_press_modifiers(CMD, Key::S);
    h.run_steps(1);
    let on_disk = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(on_disk.starts_with("fn main() {\n// fn main() {\n"), "saved: {on_disk}");
}

#[test]
fn navigation_popups_open_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path());
    std::fs::write(dir.path().join("src/util.rs"), "pub fn util() {}\n").unwrap();
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| p.tree.len() > 3));
    h.run_steps(2);

    h.key_press_modifiers(SHIFT_CMD, Key::O);
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::GotoFile { .. })));
    // Typing goes to the popup's search field, not the editor.
    let before = text(&h);
    h.event(Event::Text("util".into()));
    h.run_steps(2);
    assert_eq!(text(&h), before);
    assert!(matches!(&h.state().session.popup, Some(Popup::GotoFile { query, .. }) if query == "util"));
    h.key_press(Key::Enter);
    h.run_steps(2);
    let active = h.state().session.ws.active().and_then(|(_, d)| d.path.clone()).unwrap();
    assert!(active.ends_with("util.rs"), "{}", active.display());

    h.key_press_modifiers(CMD, Key::L);
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::GotoLine { .. })));
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.state().session.popup.is_none());

    // Cmd+F opens the find bar; typing searches.
    h.key_press_modifiers(CMD, Key::F);
    h.run_steps(2);
    assert!(h.state().session.find_bar.is_some());
}

#[test]
fn tool_window_shortcuts_toggle() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path());
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    let visible = |h: &Harness<'_, App>, tw| h.state().session.ws.layout.is_visible(tw);
    use codecraft_workspace::ToolWindowId::*;
    assert!(visible(&h, Project));
    h.key_press_modifiers(CMD, Key::Num1);
    h.run_steps(1);
    assert!(!visible(&h, Project));
    h.key_press_modifiers(CMD, Key::Num6);
    h.run_steps(1);
    assert!(visible(&h, Problems));
    h.key_press_modifiers(CMD, Key::Num4);
    h.run_steps(1);
    assert!(visible(&h, Run) && !visible(&h, Problems));
}
