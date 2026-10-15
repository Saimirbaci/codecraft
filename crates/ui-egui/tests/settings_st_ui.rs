//! Settings and keymaps (M13 track ST) through the real shell: two-stroke shortcuts with the
//! pending-chord indicator (VS Code and Emacs keymaps), keymap copies and recording a chord,
//! the editor font from a fixture font directory (a broken font never crashes), Code Style
//! pages writing `rustfmt.toml` on OK, editable live templates and TODO filters.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::Popup;
use codecraft_ui_egui::App;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };
const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: false };
const SHIFT_CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: true, mac_cmd: true, command: true };

fn project(dir: &Path, main_text: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, main_text).unwrap();
    main
}

/// An app on `open` whose per-user files (templates, fonts) live in `home`.
fn harness(open: Option<PathBuf>, home: &Path) -> Harness<'static, App> {
    let home = home.to_path_buf();
    Harness::builder().with_size(egui::vec2(1100.0, 800.0)).build_eframe(move |_| {
        let mut app = App::ephemeral(open);
        app.session.isolate_home(&home);
        app.session.ws.settings.keymap = "macOS".into();
        app
    })
}

/// The project finished loading (it closes popups when it does).
fn loaded(a: &App) -> bool {
    a.session.ws.active().is_some() && a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty())
}

#[track_caller]
fn settle(h: &mut Harness<'_, App>, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out");
}

fn text(h: &Harness<'_, App>) -> String {
    h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

/// Set `CODECRAFT_SNAPSHOT_DIR` to write PNGs of each state for a visual check.
fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

#[test]
fn vscode_chord_comments_the_line_with_a_pending_indicator() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {}\n");
    let mut h = harness(Some(main), dir.path());
    settle(&mut h, loaded);
    h.state_mut().session.execute("keymap.select", &json!({ "name": "VS Code (macOS)" })).unwrap();
    h.run_steps(2);
    assert_eq!(codecraft_ui_egui::keymap::shortcut_label("help.findAction").as_deref(), Some("⇧⌘P"));
    h.key_press_modifiers(CMD, Key::K);
    h.run_steps(2);
    assert!(
        h.query_by_label_contains("Waiting for the second key").is_some(),
        "the status bar shows the pending chord"
    );
    snapshot(&mut h, "chord-pending");
    h.key_press_modifiers(CMD, Key::C);
    h.run_steps(2);
    assert_eq!(text(&h), "// fn main() {}\n", "⌘K, ⌘C comments the line");
    assert!(h.query_by_label_contains("Waiting for the second key").is_none());
    // An unbound second stroke ends the chord without doing anything.
    h.key_press_modifiers(CMD, Key::K);
    h.run_steps(1);
    h.key_press_modifiers(CMD, Key::Q);
    h.run_steps(2);
    assert!(h.state().session.status.contains("is not a shortcut"), "{}", h.state().session.status);
    assert_eq!(text(&h), "// fn main() {}\n");
    // ⇧⌘P is VS Code's command palette (Find Action).
    h.key_press_modifiers(SHIFT_CMD, Key::P);
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::FindAction { .. })));
}

#[test]
fn emacs_ctrl_x_ctrl_s_saves() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {}\n");
    let mut h = harness(Some(main.clone()), dir.path());
    settle(&mut h, loaded);
    h.state_mut().session.execute("keymap.select", &json!({ "name": "Emacs" })).unwrap();
    h.state_mut().session.execute("editor.type", &json!({ "text": "// x\n" })).unwrap();
    h.run_steps(2);
    assert_eq!(codecraft_ui_egui::keymap::shortcut_label("file.saveAll").as_deref(), Some("Ctrl+X, Ctrl+S"));
    h.key_press_modifiers(CTRL, Key::X);
    h.run_steps(1);
    h.key_press_modifiers(CTRL, Key::S);
    h.run_steps(2);
    assert!(std::fs::read_to_string(&main).unwrap().starts_with("// x\n"), "saved");
    assert!(!text(&h).is_empty(), "Ctrl+X did not cut (it is a chord prefix in Emacs)");
}

#[test]
fn keymap_page_duplicates_and_records_a_chord() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(None, dir.path());
    h.run_steps(1);
    h.state_mut().session.execute("app.settings", &json!({ "page": "Keymap" })).unwrap();
    h.run_steps(2);
    h.get_by_label("Duplicate…").click();
    h.run_steps(2);
    let draft = h.state().session.prefs.dialog.as_ref().unwrap().draft.clone();
    assert_eq!(draft.user_keymaps.len(), 1);
    assert_eq!(draft.keymap, "macOS copy");
    assert!(h.query_by_label_contains("Based on macOS").is_some());
    // Record ⌘K, ⌘Y on the first row with "Second stroke".
    h.get_by_label("Second stroke").click();
    h.run_steps(1);
    let first = codecraft_engine::registry().all().first().unwrap().id;
    h.query_all_by_label("+").next().unwrap().click();
    h.run_steps(2);
    h.key_press_modifiers(CMD, Key::K);
    h.run_steps(2);
    h.key_press_modifiers(CMD, Key::Y);
    h.run_steps(2);
    snapshot(&mut h, "keymap-chord-recorded");
    let draft = &h.state().session.prefs.dialog.as_ref().unwrap().draft;
    assert!(codecraft_engine::keymaps::shortcuts(draft, first).contains(&"Cmd+K Cmd+Y".to_string()));
    assert!(draft.keymap_overrides.contains_key("macOS copy"), "the change belongs to the copy");
    h.get_by_label("OK").click();
    h.run_steps(2);
    let s = &h.state().session.ws.settings;
    assert_eq!(s.keymap, "macOS copy");
    assert!(codecraft_engine::keymaps::user_keymap(s, "macOS copy").is_some());
}

/// egui's built-in Hack font (a real TrueType file) for the fixture font directory.
fn hack_bytes() -> Vec<u8> {
    egui::FontDefinitions::default().font_data.get("Hack").map(|d| d.font.to_vec()).unwrap()
}

/// Hack with its `glyf` table moved past the end of the file: listed (its names are fine) but
/// broken.
fn broken_hack() -> Vec<u8> {
    let mut b = hack_bytes();
    let n = u16::from_be_bytes([b[4], b[5]]) as usize;
    for i in 0..n {
        let r = 12 + i * 16;
        if &b[r..r + 4] == b"glyf" {
            b[r + 8..r + 12].copy_from_slice(&(u32::MAX - 16).to_be_bytes());
        }
    }
    b
}

#[test]
fn editor_font_comes_from_the_fixture_dir_and_broken_fonts_never_crash() {
    let dir = tempfile::tempdir().unwrap();
    let fonts = dir.path().join("fonts");
    std::fs::create_dir_all(&fonts).unwrap();
    std::fs::write(fonts.join("Hack-Regular.ttf"), hack_bytes()).unwrap();
    std::fs::write(fonts.join("garbage.ttf"), vec![0x5Au8; 50_000]).unwrap();
    let main = project(dir.path(), "fn main() { let x = 1; }\n");
    let mut h = harness(Some(main), dir.path());
    settle(&mut h, loaded);
    h.state_mut().session.execute("app.settings", &json!({ "page": "Editor › Font" })).unwrap();
    settle(&mut h, |a| a.session.prefs_services.fonts.families.is_some());
    h.run_steps(2);
    snapshot(&mut h, "font-page");
    let fams = h.state_mut().session.execute("fonts.list", &json!({ "monospaceOnly": true })).unwrap();
    assert_eq!(fams["families"], json!([{ "name": "Hack", "monospace": true }]), "only the fixture dir is scanned");
    h.state_mut().session.execute("settings.set", &json!({ "key": "editor_font_family", "value": "Hack" })).unwrap();
    h.run_steps(1);
    h.get_by_label("OK").click();
    settle(&mut h, |a| a.ui.mono_font_name == "Hack");
    h.run_steps(3);
    snapshot(&mut h, "editor-font-hack");

    // A font whose tables are broken is either refused or rendered without crashing.
    std::fs::write(fonts.join("Hack-Regular.ttf"), broken_hack()).unwrap();
    h.state_mut().session.execute("fonts.rescan", &Value::Null).unwrap();
    settle(&mut h, |a| !a.session.prefs_services.fonts.scanning);
    h.state_mut().session.execute("settings.set", &json!({ "key": "editor_font_family", "value": "" })).unwrap();
    settle(&mut h, |a| a.session.prefs_services.fonts.loaded.is_none());
    h.state_mut().session.execute("settings.set", &json!({ "key": "editor_font_family", "value": "Hack" })).unwrap();
    settle(&mut h, |a| a.session.prefs_services.fonts.loaded.is_some() || a.session.prefs_services.fonts.error.is_some());
    h.run_steps(4);
    // Missing family: the default font with a note.
    h.state_mut().session.execute("settings.set", &json!({ "key": "editor_font_family", "value": "No Such Font" })).unwrap();
    settle(&mut h, |a| a.session.prefs_services.fonts.error.as_deref().is_some_and(|e| e.contains("not found")));
    h.run_steps(3);
    assert!(h.state().session.prefs_services.fonts.loaded.is_none());
}

#[test]
fn code_style_rust_page_writes_rustfmt_toml_on_ok() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {}\n");
    std::fs::write(dir.path().join("rustfmt.toml"), "max_width = 120\n").unwrap();
    let mut h = harness(Some(main), dir.path());
    settle(&mut h, loaded);
    h.state_mut().session.execute("app.settings", &json!({ "page": "Editor › Code Style › Rust" })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Hard wrap at").is_some());
    assert!(h.query_by_label_contains("rustfmt.toml").is_some());
    h.state_mut()
        .session
        .execute("codeStyle.set", &json!({ "language": "rust", "key": "imports_granularity", "value": "Crate" }))
        .unwrap();
    h.run_steps(2);
    snapshot(&mut h, "code-style-rust");
    assert!(!std::fs::read_to_string(dir.path().join("rustfmt.toml")).unwrap().contains("imports_granularity"));
    h.get_by_label("OK").click();
    h.run_steps(2);
    let text = std::fs::read_to_string(dir.path().join("rustfmt.toml")).unwrap();
    assert_eq!(text, "max_width = 120\nimports_granularity = \"Crate\"\n");
    // C/C++ and the other pages paint too.
    for page in ["Editor › Code Style › C/C++", "Editor › Code Style › Python", "Editor › Code Style › Other File Types"] {
        h.state_mut().session.execute("app.settings", &json!({ "page": page })).unwrap();
        h.run_steps(2);
    }
    assert!(h.query_by_label("Django Template").is_some());
}

#[test]
fn live_templates_page_lists_and_edits_templates() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(None, dir.path());
    h.run_steps(1);
    h.state_mut()
        .session
        .execute(
            "liveTemplates.save",
            &json!({ "abbreviation": "mytpl", "text": "let $NAME$ = $END$;", "languages": ["rust"], "expressions": { "NAME": "\"value\"" } }),
        )
        .unwrap();
    h.state_mut().session.execute("app.settings", &json!({ "page": "Editor › Live Templates" })).unwrap();
    h.run_steps(2);
    h.get_by_label("mytpl").click();
    h.run_steps(2);
    assert!(h.query_by_label("Variables").is_some(), "the form shows the template's variables");
    assert!(h.query_by_label("NAME").is_some());
    snapshot(&mut h, "live-template-form");
    h.get_by_label("Delete").click();
    h.run_steps(2);
    assert!(h.state_mut().session.editing.templates.user_templates().is_empty());
    h.get_by_label("+ Add").click();
    h.run_steps(2);
    assert!(h.query_by_label("Abbreviation:").is_some());
}

#[test]
fn todo_page_and_tool_window_filter() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "// TODO one\n// FIXME two\nfn main() {}\n");
    let mut h = harness(Some(main), dir.path());
    settle(&mut h, loaded);
    let s = &mut h.state_mut().session;
    s.execute("settings.set", &json!({ "key": "todo_filters", "value": [{ "name": "Fixes", "patterns": [r"\bfixme\b.*"] }] }))
        .unwrap();
    s.execute("app.settings", &json!({ "page": "Editor › TODO" })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label("Add Pattern").is_some() && h.query_by_label("Fixes").is_some());
    snapshot(&mut h, "todo-settings");
    h.get_by_label("Cancel").click();
    h.run_steps(1);
    h.state_mut().session.execute("view.toolWindows.todo", &Value::Null).unwrap();
    settle(&mut h, |a| !a.session.todo.busy() && a.session.todo.count() == 2);
    h.state_mut().session.execute("todo.setFilter", &json!({ "name": "Fixes" })).unwrap();
    h.run_steps(2);
    assert_eq!(h.state().session.todo.filter.as_deref(), Some("Fixes"));
    assert!(h.query_by_label_contains("Found 1 TODO item").is_some(), "the tool window shows the filter's items");
    snapshot(&mut h, "todo-filtered");
}
