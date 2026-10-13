//! Split editors (track S) through the real shell: two groups render side by side, a click
//! focuses the group under the pointer, typing goes to that group's editor while the other view
//! of the same file follows, the splitter drags, Alt+Tab cycles groups and a tab dragged onto the
//! other group moves there. Set `CODECRAFT_SNAPSHOT_DIR` to also write a PNG for a visual check.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use serde_json::json;

use codecraft_engine::workspace::SplitNode;
use codecraft_ui_egui::App;

fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn press(h: &mut Harness<'_, App>, pos: Pos2) {
    h.event(Event::PointerMoved(pos));
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.run_steps(1);
}

fn release(h: &mut Harness<'_, App>, pos: Pos2) {
    h.event(Event::PointerMoved(pos));
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
}

fn drag(h: &mut Harness<'_, App>, from: Pos2, to: Pos2) {
    press(h, from);
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        h.event(Event::PointerMoved(from + (to - from) * t));
        h.run_steps(1);
    }
    release(h, to);
}

fn ratio(app: &App) -> f32 {
    match app.session.ws.split_layout() {
        SplitNode::Split { ratio, .. } => *ratio,
        SplitNode::Group(_) => 0.0,
    }
}

#[test]
fn split_groups_focus_type_drag_and_move_tabs() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.rs");
    let b = tmp.path().join("b.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    std::fs::write(&b, "fn b() {}\nfn c() {}\n").unwrap();
    let open = a.clone();
    let mut h =
        Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    h.state_mut().session.execute("file.open", &json!({ "path": b })).unwrap();
    h.state_mut().session.execute("window.editorTabs.splitRight", &json!({})).unwrap();
    h.run_steps(3);
    snapshot(&mut h, "split-right");
    let ws = &h.state().session.ws;
    let [left, right] = ws.group_ids()[..] else { panic!("two groups expected") };
    assert_eq!(ws.focused_group(), right);
    let right_view = ws.active_editor().unwrap();
    let left_view = ws.group(left).unwrap().active.unwrap();
    let right_rect = h.state().ui.editor_rect.unwrap();

    // A click in the left group's editor focuses it and puts the caret there.
    let in_left = pos2(right_rect.left() - 200.0, right_rect.top() + 5.0);
    press(&mut h, in_left);
    release(&mut h, in_left);
    assert_eq!(h.state().session.ws.focused_group(), left);
    assert_eq!(h.state().session.ws.active_editor(), Some(left_view));
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
    h.state_mut().session.execute("window.editorTabs.focusGroup", &json!({ "group": right.0 })).unwrap();
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 13 })).unwrap();
    h.state_mut().session.execute("window.editorTabs.focusGroup", &json!({ "group": left.0 })).unwrap();
    h.run_steps(2);
    h.event(Event::Text("xy".into()));
    h.run_steps(2);
    let ws = &h.state().session.ws;
    let text = ws.active().map(|(_, d)| d.doc.text()).unwrap();
    assert_eq!(text, "xyfn b() {}\nfn c() {}\n");
    assert_eq!(ws.editor(left_view).unwrap().selections.primary().head, 2);
    // The right view of the same file kept its place in the text.
    assert_eq!(ws.editor(right_view).unwrap().selections.primary().head, 15);

    // Alt+Tab goes to the next group.
    h.key_press_modifiers(Modifiers::ALT, Key::Tab);
    h.run_steps(2);
    assert_eq!(h.state().session.ws.focused_group(), right);

    // Dragging the splitter changes the share.
    let before = ratio(h.state());
    let divider = pos2(right_rect.left() - 2.5, right_rect.center().y);
    drag(&mut h, divider, divider + egui::vec2(-120.0, 0.0));
    assert!(ratio(h.state()) < before - 0.05, "ratio {} -> {}", before, ratio(h.state()));

    // Dragging the right group's tab onto the left tab bar moves it; the emptied group goes away.
    let right_rect = h.state().ui.editor_rect.unwrap();
    let tab = pos2(right_rect.left() + 30.0, right_rect.top() - 16.0);
    let target = pos2(right_rect.left() - 60.0, right_rect.top() - 16.0);
    drag(&mut h, tab, target);
    let ws = &h.state().session.ws;
    assert!(!ws.is_split(), "{:?}", ws.split_layout());
    // The dropped view showed a file the left group already had: that tab is selected instead.
    assert!(ws.editor(right_view).is_none());
    assert_eq!(ws.active_editor(), Some(left_view));
    assert_eq!(ws.tabs().len(), 2);
    snapshot(&mut h, "split-merged");
}
