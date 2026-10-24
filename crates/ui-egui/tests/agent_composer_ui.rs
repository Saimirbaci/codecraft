//! Attachment shortcuts and the persistent, resizable Agent composer.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use codecraft_ui_egui::App;
use egui::{Event, Key};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;

#[test]
fn at_attaches_files_and_folders_and_history_is_always_visible() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("README.md"), "demo").unwrap();
    let root = dir.path().to_path_buf();
    let mut h =
        Harness::builder().with_size(egui::vec2(1440.0, 900.0)).build_eframe(move |_| App::ephemeral(Some(root)));
    for _ in 0..200 {
        h.run_steps(1);
        if !h.state().session.jobs_running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.state_mut().session.execute("view.toolWindows.agent", &json!(null)).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("History · 0").is_some());
    let prompt_id = egui::Id::new("agent-prompt");
    h.ctx.memory_mut(|m| m.request_focus(prompt_id));
    h.event(Event::Text("Check @README".into()));
    h.run_steps(3);
    assert!(h.query_by_label("File README.md").is_some());
    if let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") {
        h.render().unwrap().save(std::path::PathBuf::from(dir).join("agent-mentions.png")).unwrap();
    }
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().ui.agent.context, vec![dir.path().join("README.md")]);
    assert_eq!(h.state().ui.agent.prompt, "Check ");
    h.event(Event::Text("@src".into()));
    h.run_steps(3);
    h.get_by_label("Folder src").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.agent.context.len(), 2);
    assert_eq!(h.state().ui.agent.prompt, "Check ");
    let height = h.state().session.ws.layout.pane_size("agent.composer", 0.0);
    assert!(height >= 58.0);
    let rect = h.ctx.read_response(prompt_id).unwrap().rect;
    let from = rect.right_bottom() - egui::vec2(4.0, 4.0);
    let to = from + egui::vec2(0.0, 80.0);
    h.event(Event::PointerMoved(from));
    h.run_steps(2);
    h.event(Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    h.run_steps(2);
    h.event(Event::PointerMoved(to));
    h.run_steps(2);
    h.event(Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    h.run_steps(3);
    assert!(h.state().session.ws.layout.pane_size("agent.composer", 0.0) > height + 40.0);
    h.ctx.memory_mut(|m| m.request_focus(prompt_id));
    h.event(Event::Text("@missing".into()));
    h.run_steps(3);
    assert!(h.query_by_label("No matching files or folders").is_some());
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.query_by_label("No matching files or folders").is_none());
    let resized_height = h.state().session.ws.layout.pane_size("agent.composer", 0.0);
    h.state_mut().ui.agent.prompt = "A long instruction\n".repeat(100);
    h.run_steps(4);
    assert!(
        (h.state().session.ws.layout.pane_size("agent.composer", 0.0) - resized_height).abs() < 2.0,
        "Long prompts scroll inside the resized composer"
    );
    h.state_mut().ui.agent.prompt = "Check the attached files".into();
    h.run_steps(2);
    if let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") {
        h.render().unwrap().save(std::path::PathBuf::from(dir).join("agent-composer.png")).unwrap();
    }
}
