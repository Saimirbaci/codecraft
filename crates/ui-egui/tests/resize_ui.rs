//! Tool window panels and floating windows resize by dragging, and the sizes are remembered.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use egui::{Event, PointerButton, Pos2};
use egui_kittest::Harness;

use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn drag(h: &mut Harness<'_, App>, from: Pos2, to: Pos2) {
    let mods = Default::default();
    h.event(Event::PointerMoved(from));
    h.run_steps(2);
    h.event(Event::PointerButton { pos: from, button: PointerButton::Primary, pressed: true, modifiers: mods });
    h.run_steps(2);
    h.event(Event::PointerMoved(to));
    h.run_steps(2);
    h.event(Event::PointerButton { pos: to, button: PointerButton::Primary, pressed: false, modifiers: mods });
    h.run_steps(3);
}

fn open(tools: (ToolWindowId, ToolWindowId, ToolWindowId)) -> Harness<'static, App> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"d\"\nversion=\"0.1.0\"\n").unwrap();
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let mut h =
        Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(root)));
    h.run_steps(5);
    let l = &mut h.state_mut().session.ws.layout;
    l.left = Some(tools.0);
    l.right = Some(tools.1);
    l.bottom = Some(tools.2);
    h.run_steps(5);
    h
}

#[test]
fn every_tool_window_edge_resizes_and_is_remembered_per_window() {
    let mut h = open((ToolWindowId::Commit, ToolWindowId::Database, ToolWindowId::Terminal));
    let l = h.state().session.ws.layout.clone();
    // The left panel's edge is where the stripe ends plus the panel width (within the grab radius).
    let lx = 34.0 + l.left_width;
    drag(&mut h, Pos2::new(lx, 300.0), Pos2::new(lx + 100.0, 300.0));
    let a = h.state().session.ws.layout.clone();
    assert!(a.left_width > l.left_width + 50.0, "left {} -> {}", l.left_width, a.left_width);
    assert_eq!(a.tool_sizes.get("commit").copied(), Some(a.left_width));

    let rx = 1280.0 - 34.0 - a.right_width;
    drag(&mut h, Pos2::new(rx, 300.0), Pos2::new(rx - 100.0, 300.0));
    let b = h.state().session.ws.layout.clone();
    assert!(b.right_width > a.right_width + 50.0, "right {} -> {}", a.right_width, b.right_width);

    let by = 800.0 - 24.0 - b.bottom_height;
    drag(&mut h, Pos2::new(640.0, by), Pos2::new(640.0, by - 100.0));
    let c = h.state().session.ws.layout.clone();
    assert!(c.bottom_height > b.bottom_height + 50.0, "bottom {} -> {}", b.bottom_height, c.bottom_height);

    // Another tool in the same slot starts from its own size, and switching back restores the first.
    h.state_mut().session.ws.layout.left = Some(ToolWindowId::Project);
    h.run_steps(4);
    assert!((h.state().session.ws.layout.left_width - 280.0).abs() < 1.0);
    h.state_mut().session.ws.layout.left = Some(ToolWindowId::Commit);
    h.run_steps(4);
    assert!((h.state().session.ws.layout.left_width - a.left_width).abs() < 1.0);
}
