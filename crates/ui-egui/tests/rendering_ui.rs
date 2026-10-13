//! Editor rendering (M10 track C) through the real shell: Show Whitespaces paints marks by mode,
//! wrapped rows keep the line's indentation, box carets sit in virtual space, and the IME
//! pre-edit is drawn without touching the document until it is committed.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::epaint::{ClippedShape, Shape};
use egui::{Color32, Event, ImeEvent};
use egui_kittest::Harness;
use serde_json::json;

use codecraft_ui_egui::App;

fn project(dir: &Path, main_text: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, main_text).unwrap();
    main
}

fn harness(open: PathBuf) -> Harness<'static, App> {
    Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_eframe(move |_| App::ephemeral(Some(open)))
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

fn text(h: &Harness<'_, App>) -> String {
    h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

fn visit<'a>(shape: &'a Shape, f: &mut impl FnMut(&'a Shape)) {
    if let Shape::Vec(v) = shape {
        for s in v {
            visit(s, f);
        }
    } else {
        f(shape);
    }
}

fn shapes(h: &Harness<'_, App>, mut f: impl FnMut(&Shape)) {
    for ClippedShape { shape, .. } in &h.output().shapes {
        visit(shape, &mut f);
    }
}

/// Space marks (dots) painted in `color`.
fn dots(h: &Harness<'_, App>, color: Color32) -> usize {
    let mut n = 0;
    shapes(h, |s| {
        if let Shape::Circle(c) = s
            && c.fill == color
        {
            n += 1;
        }
    });
    n
}

/// Tab marks: arrow strokes painted in `color` (three segments per tab).
fn arrow_segments(h: &Harness<'_, App>, color: Color32) -> usize {
    let mut n = 0;
    shapes(h, |s| {
        if let Shape::LineSegment { stroke, .. } = s
            && stroke.color == color
        {
            n += 1;
        }
    });
    n
}

fn painted_text_contains(h: &Harness<'_, App>, needle: &str) -> bool {
    let mut found = false;
    shapes(h, |s| {
        if let Shape::Text(t) = s
            && t.galley.text().contains(needle)
        {
            found = true;
        }
    });
    found
}

fn caret_x(h: &mut Harness<'_, App>, offset: usize) -> f32 {
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": offset })).unwrap();
    h.run_steps(2);
    h.state().ui.caret_screen.expect("the caret is on screen").x
}

#[test]
fn show_whitespace_paints_marks_by_mode() {
    let dir = tempfile::tempdir().unwrap();
    // Line 0: 2 inner spaces. Line 1: 4 leading, 3 inner, 2 trailing. Line 2: a tab, then `}`.
    let main = project(dir.path(), "fn main() {\n    let a = 1;  \n\t}\n");
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    let color = h.state().ui.theme.whitespace;
    assert_eq!(dots(&h, color), 0, "off by default");
    assert_eq!(arrow_segments(&h, color), 0);

    h.state_mut().session.execute("view.toggleWhitespace", &json!({})).unwrap();
    h.run_steps(2);
    assert!(h.state().session.ws.settings.show_whitespace);
    assert_eq!(dots(&h, color), 11, "every space");
    assert_eq!(arrow_segments(&h, color), 3, "one arrow for the tab");

    let set = |h: &mut Harness<'_, App>, mode: &str| {
        h.state_mut().session.execute("settings.set", &json!({ "key": "whitespace_mode", "value": mode })).unwrap();
        h.run_steps(2);
    };
    set(&mut h, "leading_trailing");
    assert_eq!(dots(&h, color), 6, "indentation and line ends");
    assert_eq!(arrow_segments(&h, color), 3, "the tab is indentation");
    set(&mut h, "trailing");
    assert_eq!(dots(&h, color), 2, "line ends only");
    assert_eq!(arrow_segments(&h, color), 0);
    assert!(h.state_mut().session.execute("settings.set", &json!({ "key": "whitespace_mode", "value": "some" })).is_err());

    h.state_mut().session.execute("view.toggleWhitespace", &json!({})).unwrap();
    h.run_steps(2);
    assert_eq!(dots(&h, color), 0, "toggled off again");
}

#[test]
fn wrapped_rows_keep_the_line_indentation() {
    let dir = tempfile::tempdir().unwrap();
    let words = "word ".repeat(120);
    let main = project(dir.path(), &format!("fn main() {{\n        let s = \"{words}\";\n}}\n"));
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.execute("view.toggleSoftWrap", &json!({})).unwrap();
    h.run_steps(3);
    let eid = h.state().session.ws.active_editor().unwrap();
    let width = *h.state().session.editing.wrap_widths.get(&eid).expect("wrapping");
    let (line1, starts) = {
        let d = &h.state().session.ws.active().unwrap().1.doc;
        let text = d.line_text(1).unwrap();
        (d.line_start(1), codecraft_text::wrap::wrap_line(&text, codecraft_text::wrap::WrapConfig::new(width, 4)))
    };
    assert!(starts.len() > 2, "the line wraps: {starts:?}");
    let origin = caret_x(&mut h, 0);
    let first_text = caret_x(&mut h, line1 + 8);
    let second_row = caret_x(&mut h, line1 + starts[1]);
    let third_row = caret_x(&mut h, line1 + starts[2]);
    assert!(first_text - origin > 20.0, "the line is indented: {origin} {first_text}");
    assert!((second_row - first_text).abs() < 1.0, "continuation row starts at the indent: {first_text} {second_row}");
    assert!((third_row - first_text).abs() < 1.0);
    // Clicking at the continuation row's indentation puts the caret at the row start.
    let rect = h.state().ui.editor_rect.unwrap();
    let line_h = h.state().ui.editor_line_h;
    let y = h.state().ui.caret_screen.unwrap().y - line_h * 0.5;
    assert!(rect.contains(egui::pos2(second_row + 2.0, y)));
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
    h.run_steps(1);
    h.event(Event::PointerButton {
        pos: egui::pos2(second_row + 1.0, y),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    h.event(Event::PointerButton {
        pos: egui::pos2(second_row + 1.0, y),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(2);
    let caret = h.state().session.ws.active().unwrap().0.selections.primary().head;
    assert_eq!(caret, line1 + starts[2], "a click at the indentation of row three lands on its first char");
}

#[test]
fn box_carets_render_in_virtual_space() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {\nx\n}\n");
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    let x0 = caret_x(&mut h, 0);
    let x1 = caret_x(&mut h, 1);
    let cw = x1 - x0;
    h.state_mut()
        .session
        .execute("editor.boxSelect", &json!({ "anchor": { "line": 0, "column": 20 }, "head": { "line": 2, "column": 20 } }))
        .unwrap();
    h.run_steps(2);
    let x = h.state().ui.caret_screen.unwrap().x;
    assert!((x - (x0 + 20.0 * cw)).abs() < 1.5, "caret at the virtual column: {x} vs {}", x0 + 20.0 * cw);
    h.event(Event::Text("Z".into()));
    h.run_steps(1);
    assert_eq!(text(&h), format!("fn main() {{{}Z\nx{}Z\n}}{}Z\n", " ".repeat(9), " ".repeat(19), " ".repeat(19)));
}

#[test]
fn ime_preedit_is_drawn_but_not_typed_until_commit() {
    let dir = tempfile::tempdir().unwrap();
    let main = project(dir.path(), "fn main() {\n    \n}\n");
    let mut h = harness(main);
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 16 })).unwrap();
    h.run_steps(1);
    let before = text(&h);
    let caret_before = h.state().ui.caret_screen.unwrap().x;
    h.event(Event::Ime(ImeEvent::Preedit { text: "zhong".into(), active_range_chars: None }));
    h.run_steps(2);
    assert_eq!(text(&h), before, "pre-edit text is not in the document");
    assert!(painted_text_contains(&h, "zhong"), "pre-edit text is painted");
    assert!(h.state().ui.caret_screen.unwrap().x >= caret_before, "the IME area follows the caret");
    h.event(Event::Ime(ImeEvent::Commit("中".into())));
    h.run_steps(2);
    assert_eq!(text(&h), "fn main() {\n    中\n}\n");
    assert!(!painted_text_contains(&h, "zhong"), "the pre-edit is gone after the commit");
}
