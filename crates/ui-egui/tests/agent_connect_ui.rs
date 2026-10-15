//! Agents › Connect an External Agent (M15 track AI): the window opens from the Agent tool
//! window and from Settings › Tools › Agent Access, lists every client, and copies the entry.
//! Set `CODECRAFT_SNAPSHOT_DIR` to also write a PNG.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_ui_egui::App;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            h.run_steps(2);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn connect_an_external_agent_window() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("notes.txt"), "hello\n").unwrap();
    let open = proj.join("notes.txt");
    let mut h =
        Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    h.state_mut().session.execute("view.toolWindows.agent", &json!(null)).unwrap();
    h.run_steps(2);
    h.get_by_label("⇄").click();
    settle(&mut h, "the window", |a| a.ui.agent.connect.is_some());
    assert!(h.query_by_label("Connect an External Agent").is_some());
    assert!(h.query_by_label("This IDE").is_some());
    let clients = h.state().ui.agent.connect.as_ref().unwrap()["clients"].as_array().unwrap().len();
    assert_eq!(clients, 6);
    h.get_by_label("Codex").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.agent.connect_client, "codex");
    h.get_all_by_label("Copy").next().unwrap().click();
    h.run_steps(2);
    assert_eq!(h.state().session.status, "Copied the configuration");
    if let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") {
        h.render().unwrap().save(PathBuf::from(dir).join("agent-connect.png")).unwrap();
    }
    // Closing it forgets the snippets; the Agent Access page opens it again.
    h.state_mut().ui.agent.connect = None;
    h.state_mut().session.execute("app.settings", &json!({ "page": "Tools › Agent Access" })).unwrap();
    h.run_steps(3);
    h.get_by_label("Connect an External Agent…").click();
    settle(&mut h, "the window from Settings", |a| a.ui.agent.connect.is_some());
    h.state_mut().session.shutdown();
}
