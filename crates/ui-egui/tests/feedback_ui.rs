//! The feedback overlay driven headlessly: shortcut, drawing with the pointer, Escape, and the
//! full submit (screenshot → composite → record) with a fake agent.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::feedback::Shape;
use codecraft_engine::{CommandError, Session};
use codecraft_ui_egui::App;

const GOTO_LINE: &str = "navigate.lineColumn";
const TOGGLE: Modifiers = Modifiers { alt: true, ctrl: false, shift: true, mac_cmd: true, command: true };

fn project(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    main
}

fn fake_agent(_: &mut Session, _: &Value) -> Result<Value, CommandError> {
    Ok(json!({ "job": 42 }))
}

fn fake_agent_with_session(_: &mut Session, _: &Value) -> Result<Value, CommandError> {
    Ok(json!({ "job": 43, "session": "sess-ui" }))
}

fn harness(dir: &Path) -> Harness<'static, App> {
    let main = project(dir);
    let records = dir.join("records");
    let source = dir.to_path_buf();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(main));
        // Never write to the user's home or start a real agent.
        app.session.isolate_home(&source.join("home"));
        app.session.feedback.dir = Some(records);
        app.session.feedback.source_dir = Some(source);
        app.session.feedback.launcher = Some(fake_agent);
        app
    });
    settle(&mut h, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(2);
    h
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

fn caret(h: &Harness<'_, App>) -> usize {
    h.state().session.ws.active().map(|(e, _)| e.selections.primary().head).unwrap()
}

fn drag(h: &mut Harness<'_, App>, from: Pos2, to: Pos2) {
    let button = |pos, pressed| Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.event(Event::PointerMoved(from));
    h.run_steps(1);
    h.event(button(from, true));
    h.run_steps(1);
    for i in 1..=6 {
        h.event(Event::PointerMoved(from + (to - from) * (i as f32 / 6.0)));
        h.run_steps(1);
    }
    h.event(button(to, false));
    h.run_steps(2);
}

#[test]
fn draw_a_rectangle_then_escape_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let before = caret(&h);

    h.key_press_modifiers(TOGGLE, Key::F);
    h.run_steps(2);
    assert!(h.state().session.feedback.active, "the shortcut opens the overlay");

    h.key_press(Key::R);
    h.run_steps(1);
    // Drag over the editor: the overlay takes the pointer, the editor must not react.
    drag(&mut h, pos2(400.0, 150.0), pos2(640.0, 330.0));
    let shapes = h.state().session.feedback.draft.shapes.clone();
    assert_eq!(shapes.len(), 1, "{shapes:?}");
    let Shape::Rect { x, y, w, h: hh, .. } = &shapes[0] else { panic!("{shapes:?}") };
    assert!((x - 400.0 / 1280.0).abs() < 0.01 && (y - 150.0 / 800.0).abs() < 0.01, "{shapes:?}");
    assert!((w - 240.0 / 1280.0).abs() < 0.01 && (hh - 180.0 / 800.0).abs() < 0.01, "{shapes:?}");
    assert_eq!(caret(&h), before, "the editor reacted to a click under the overlay");

    // Undo / redo from the keyboard.
    h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
    h.run_steps(1);
    assert!(h.state().session.feedback.draft.shapes.is_empty());
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
    h.run_steps(1);
    assert_eq!(h.state().session.feedback.draft.shapes.len(), 1);

    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(!h.state().session.feedback.active);
    assert!(h.state().session.popup.is_none());
    // The draft survives closing (Cancel discards it).
    assert_eq!(h.state().session.feedback.draft.shapes.len(), 1);
}

#[test]
fn the_toolbar_button_on_the_left_opens_the_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let button = h.get_by_label("✏ Feedback");
    let r = button.rect();
    assert!(r.max.x < 640.0 && r.min.y < 80.0, "on the left of the main toolbar: {r:?}");
    button.click();
    h.run_steps(2);
    assert!(h.state().session.feedback.active, "the toolbar button opens the overlay");
}

#[test]
fn the_right_stripe_button_under_remote_host_opens_the_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    // Hide the main toolbar: the stripe button is still there.
    h.state_mut().session.execute("view.appearance.toolbar", &json!({ "on": false })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label("✏ Feedback").is_none(), "the toolbar is hidden");
    let button = h.get_by_label("✏");
    let remote = h.get_by_label("🖥");
    let (r, host) = (button.rect(), remote.rect());
    assert!(r.min.x > 1200.0, "on the right stripe: {r:?}");
    assert!(
        r.min.y > host.max.y - 1.0 && (r.center().x - host.center().x).abs() < 2.0,
        "under Remote Host: {r:?} {host:?}"
    );
    button.click();
    h.run_steps(2);
    assert!(h.state().session.feedback.active, "the stripe button opens the overlay");
}

#[test]
fn escape_does_not_reach_popups_underneath() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.state_mut().session.execute(GOTO_LINE, &Value::Null).unwrap();
    h.run_steps(2);
    assert!(h.state().session.popup.is_some());
    h.state_mut().session.execute("feedback.toggle", &Value::Null).unwrap();
    h.run_steps(2);
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(!h.state().session.feedback.active);
    assert!(h.state().session.popup.is_some(), "Escape closed the overlay and the popup");
}

#[test]
fn submit_captures_screenshots_and_writes_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.state_mut().session.execute("feedback.toggle", &Value::Null).unwrap();
    h.run_steps(2);
    h.key_press(Key::A);
    h.run_steps(1);
    drag(&mut h, pos2(200.0, 500.0), pos2(500.0, 250.0));
    assert!(matches!(h.state().session.feedback.draft.shapes.as_slice(), [Shape::Arrow { .. }]));
    h.state_mut().session.feedback.draft.text = "Make the gutter wider".into();
    h.run_steps(1);
    h.get_by_label("Send to agent").click();
    settle(&mut h, |a| a.session.feedback.last.is_some() || a.session.status.contains("did not start"));
    let s = &h.state().session;
    let last = s.feedback.last.clone().unwrap_or_else(|| panic!("{}", s.status));
    assert_eq!(last.job, Some(42));
    assert!(!s.feedback.active);
    let clean = image::open(last.dir.join("screenshot.png")).unwrap().to_rgba8();
    let annotated = image::open(last.dir.join("annotated.png")).unwrap().to_rgba8();
    assert_eq!(clean.dimensions(), annotated.dimensions());
    // The arrow's midpoint (350, 375) in points; the clean frame has no overlay there.
    let scale = clean.width() as f32 / 1280.0;
    let (mx, my) = ((350.0 * scale) as u32, (375.0 * scale) as u32);
    let near = |img: &image::RgbaImage| {
        (mx.saturating_sub(3)..mx + 3)
            .flat_map(|x| (my.saturating_sub(3)..my + 3).map(move |y| (x, y)))
            .map(|(x, y)| img.get_pixel(x, y).0)
            .collect::<Vec<_>>()
    };
    assert_ne!(near(&clean), near(&annotated), "the annotation is missing from annotated.png");
    let rec: Value = serde_json::from_str(&std::fs::read_to_string(last.dir.join("feedback.json")).unwrap()).unwrap();
    assert_eq!(rec["shapes"][0]["type"], "arrow");
    assert_eq!(rec["text"], "Make the gutter wider");
    assert!(rec["context"]["activeFile"].as_str().unwrap().ends_with("main.rs"));
    assert_eq!(rec["window"]["width"], 1280.0);
}

#[test]
fn past_feedback_shows_on_reopen_and_continues_in_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.state_mut().session.feedback.launcher = Some(fake_agent_with_session);
    h.state_mut()
        .session
        .execute("feedback.submit", &json!({ "confirm": true, "text": "Add a rename action to the tabs" }))
        .unwrap();
    h.run_steps(2);

    // Reopening the overlay shows the earlier feedback.
    h.key_press_modifiers(TOGGLE, Key::F);
    h.run_steps(2);
    assert!(h.query_by_label("Recent feedback").is_some(), "the recent threads are listed");
    assert!(h.query_by_label("Add a rename action to the tabs").is_some());
    h.get_by_label("Continue").click();
    h.run_steps(3);

    // The thread: its earlier request, the same agent session, and a follow-up to send.
    let fb = &h.state().session.feedback;
    assert!(fb.thread.is_some() && fb.draft.session.as_deref() == Some("sess-ui"));
    assert!(h.query_by_label("Thread: Add a rename action to the tabs").is_some());
    assert!(h.query_by_label("Earlier requests (1)").is_some());
    assert!(h.query_by_label("Send follow-up").is_some());
    assert!(h.query_by_label("Recent feedback").is_none(), "the list makes room for the thread");
    assert!(h.query_by_label("Agent session").is_some());
    // The follow-up goes to the thread's own agent: the choice is locked.
    h.get_by_label("Codex").click();
    h.run_steps(2);
    assert_eq!(h.state().session.feedback.draft.backend, "claude");
    assert_eq!(h.state().session.feedback.draft.session.as_deref(), Some("sess-ui"));

    // New thread leaves it.
    h.get_by_label("New thread").click();
    h.run_steps(2);
    assert!(h.state().session.feedback.thread.is_none());
    assert!(h.query_by_label("Send to agent").is_some());

    // The history lists the thread with Continue too.
    h.get_by_label("History…").click();
    h.run_steps(2);
    assert!(h.query_by_label("1 request").is_some(), "grouped by thread");
}
