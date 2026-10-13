//! Settings and keymaps (track K) through the real shell: the Settings dialog with Apply/OK,
//! recording a shortcut in the keymap editor, the Windows/Linux keymap dispatching, Quick Switch
//! Scheme changing the theme, and Edit Configurations saving a user run configuration.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::Popup;
use codecraft_ui_egui::App;
use codecraft_ui_egui::theme::Theme;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };
const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: false };
const CTRL_SHIFT: Modifiers = Modifiers { alt: false, ctrl: true, shift: true, mac_cmd: false, command: false };
const SHIFT_CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: true, mac_cmd: true, command: true };

fn harness(open: Option<PathBuf>) -> Harness<'static, App> {
    Harness::builder().with_size(egui::vec2(1100.0, 800.0)).build_eframe(move |_| {
        let mut app = App::ephemeral(open);
        // Never depend on the machine's default keymap.
        app.session.ws.settings.keymap = "macOS".into();
        app
    })
}

/// Set `CODECRAFT_SNAPSHOT_DIR` to write PNGs of each state for a visual check.
fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn project(dir: &Path) -> PathBuf {
    let root = dir.join("demo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    root
}

#[test]
fn settings_dialog_applies_on_ok_and_discards_on_cancel() {
    let mut h = harness(None);
    h.run_steps(2);
    h.key_press_modifiers(CMD, Key::Comma);
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::Settings)));
    assert!(h.query_by_label("Apply").is_some() && h.query_by_label("OK").is_some());
    // Pages in the tree.
    h.get_by_label("Code Style").click();
    h.run_steps(2);
    h.state_mut().session.execute("settings.set", &json!({ "key": "tab_size", "value": 2 })).unwrap();
    h.run_steps(1);
    assert_eq!(h.state().session.ws.settings.tab_size, 4, "only the draft changed");
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().session.popup.is_none());
    assert_eq!(h.state().session.ws.settings.tab_size, 4);

    h.key_press_modifiers(CMD, Key::Comma);
    h.run_steps(2);
    h.state_mut().session.execute("settings.set", &json!({ "key": "theme", "value": "Light" })).unwrap();
    h.run_steps(1);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert_eq!(h.state().session.ws.settings.theme, "Light");
    assert_eq!(h.state().ui.theme.panel_bg, Theme::light().panel_bg, "the theme follows the setting");
    assert_eq!(h.state().ui.theme.editor_bg, Theme::light().editor_bg);
    snapshot(&mut h, "light-theme");
}

#[test]
fn every_settings_page_paints() {
    let mut h = harness(None);
    h.run_steps(1);
    let pages = codecraft_engine::settings::PAGES;
    for page in pages {
        h.state_mut().session.execute("app.settings", &json!({ "page": page })).unwrap();
        h.run_steps(2);
        assert!(matches!(h.state().session.popup, Some(Popup::Settings)), "{page}");
        snapshot(&mut h, &format!("settings-{}", page.replace([' ', '›', '&'], "")));
    }
    // A user colour scheme shows its token editor.
    h.state_mut().session.execute("colorScheme.duplicate", &Value::Null).unwrap();
    h.state_mut().session.execute("settings.dialog.page", &json!({ "page": "Editor › Color Scheme" })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label("keyword").is_some());
    snapshot(&mut h, "settings-user-scheme");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.state().session.popup.is_none(), "Escape cancels the dialog");
    assert!(h.state().session.ws.settings.user_schemes.is_empty(), "…and drops the draft");
}

#[test]
fn keymap_editor_records_a_shortcut() {
    let mut h = harness(None);
    h.run_steps(1);
    h.state_mut().session.execute("app.settings", &json!({ "page": "Keymap" })).unwrap();
    h.run_steps(2);
    // The first row's "+" starts recording; the next key press is the shortcut.
    let first = codecraft_engine::registry().all().first().unwrap().id;
    h.query_all_by_label("+").next().unwrap().click();
    h.run_steps(2);
    h.key_press_modifiers(SHIFT_CMD, Key::Y);
    h.run_steps(2);
    snapshot(&mut h, "keymap-recorded");
    let draft = &h.state().session.prefs.dialog.as_ref().unwrap().draft;
    assert_eq!(codecraft_engine::keymaps::shortcuts(draft, first), vec!["Shift+Cmd+Y"]);
    h.get_by_label("OK").click();
    h.run_steps(2);
    let s = &h.state().session.ws.settings;
    assert!(codecraft_engine::keymaps::is_overridden(s, first));
    let sc = codecraft_ui_egui::keymap::Shortcut::parse("Shift+Cmd+Y").unwrap();
    assert!(h.state().ui.bindings.iter().any(|b| b.shortcut == sc && b.id == first), "dispatch follows the override");
    assert_eq!(codecraft_ui_egui::keymap::shortcut_label(first).as_deref(), Some("⇧⌘Y"));
}

#[test]
fn windows_keymap_dispatches_ctrl_shortcuts() {
    let mut h = harness(None);
    h.run_steps(1);
    h.state_mut().session.execute("keymap.select", &json!({ "name": "Windows/Linux" })).unwrap();
    h.run_steps(2);
    h.key_press_modifiers(CTRL_SHIFT, Key::A);
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::FindAction { .. })));
    h.key_press(Key::Escape);
    h.run_steps(2);
    // Cmd shortcuts are not bound any more; Ctrl+Alt+S is Settings.
    h.key_press_modifiers(CMD, Key::Comma);
    h.run_steps(2);
    assert!(h.state().session.popup.is_none());
    h.key_press_modifiers(Modifiers { alt: true, ..CTRL }, Key::S);
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::Settings)));
    assert_eq!(codecraft_ui_egui::keymap::shortcut_label("help.findAction").as_deref(), Some("Ctrl+Shift+A"));
}

#[test]
fn quick_switch_scheme_switches_the_theme() {
    let mut h = harness(None);
    h.run_steps(1);
    h.key_press_modifiers(CTRL, Key::Backtick);
    h.run_steps(2);
    assert!(h.state().session.prefs.quick_switch.is_some());
    assert!(h.query_by_label_contains("Editor Color Scheme").is_some());
    snapshot(&mut h, "quick-switch");
    // 3. Theme, then High Contrast (third row).
    h.event(Event::Text("3".into()));
    h.run_steps(2);
    h.key_press(Key::ArrowDown);
    h.key_press(Key::ArrowDown);
    h.key_press(Key::Enter);
    h.run_steps(2);
    assert!(h.state().session.prefs.quick_switch.is_none());
    assert_eq!(h.state().session.ws.settings.theme, "High Contrast");
    assert_eq!(h.state().ui.theme.editor_bg, Theme::high_contrast().editor_bg);
    snapshot(&mut h, "high-contrast");
}

#[test]
fn edit_configurations_creates_and_saves() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path());
    let mut h = harness(Some(root.clone()));
    let start = std::time::Instant::now();
    while h.state().session.run_store.root().is_none() && start.elapsed() < std::time::Duration::from_secs(60) {
        h.run_steps(1);
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    h.state_mut().session.execute("run.editConfigurations", &Value::Null).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Run/Debug Configurations").is_some());
    h.get_by_label("+ Add").click();
    h.run_steps(3);
    for kind in ["Cargo", "Command", "Python", "C/C++", "Zig", "Npm"] {
        assert!(h.query_by_label(kind).is_some(), "+ Add offers {kind}");
    }
    h.get_by_label("Python").click();
    h.run_steps(3);
    assert_eq!(h.state().session.run_store.dialog.as_ref().unwrap().draft.len(), 1);
    assert!(h.query_by_label_contains("Environment variables").is_some());
    assert!(h.query_by_label_contains("Environment file").is_some());
    snapshot(&mut h, "edit-configurations");
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert!(h.state().session.run_store.dialog.is_none());
    let saved = std::fs::read_to_string(root.join(".codecraft/run-configurations.json")).unwrap();
    assert!(saved.contains("\"python\"") && saved.contains("main.py"), "{saved}");
    assert_eq!(h.state().session.run_configs.first().map(|c| c.name.as_str()), Some("Python"));
}
