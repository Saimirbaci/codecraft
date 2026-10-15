//! The Agent tool window's History, M13 track SR: search and status/agent filters narrow the
//! list ("History · 1 of 4"), a session running in another window is marked and not opened,
//! and Clean Up asks first (a confirmation listing what goes) before removing old and
//! interrupted sessions. Saved sessions are written straight into a temp home; no agent runs.
//! Set `CODECRAFT_SNAPSHOT_DIR` to also write PNGs.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::agent_sessions;
use codecraft_ui_egui::App;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            h.run_steps(2);
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    panic!("timed out waiting for {what}");
}

fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn write_session(dir: &Path, id: &str, title: &str, backend: &str, status: &str, updated: u64) {
    std::fs::create_dir_all(dir).unwrap();
    let v = json!({
        "version": 1, "id": id, "title": title, "backend": backend, "mode": "auto", "status": status,
        "prompt": format!("Please {title}"), "createdMs": updated, "updatedMs": updated,
        "activity": [{ "kind": "prompt", "text": format!("Please {title}") }],
    });
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
}

#[test]
fn history_filters_marks_running_elsewhere_and_cleans_up_after_asking() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (home, proj) = (root.join("home"), root.join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let dir = home.join("agent-sessions").join(agent_sessions::project_key(&proj));
    let now = agent_sessions::now_ms();
    let old = now - 90 * 86_400_000;
    write_session(&dir, "old-1", "Fix the parser", "claude", "finished", old);
    write_session(&dir, "old-2", "Write the docs", "codex", "failed", old + 1);
    write_session(&dir, "new-1", "Parser benchmarks", "codex", "finished", now - 1000);
    // Running in another window: a fresh lock next to it.
    write_session(&dir, "live-1", "Long migration", "claude", "running", now - 500);
    let lock = agent_sessions::lock_file(&dir.join("live-1.json"));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(lock, json!({ "heartbeatMs": now }).to_string()).unwrap();

    let open = proj.clone();
    let mut h =
        Harness::builder().with_size(egui::vec2(1440.0, 900.0)).build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    {
        let s = &mut h.state_mut().session;
        s.isolate_home(&home);
        s.execute("agent.sessions.reload", &json!(null)).unwrap();
        s.execute("view.toolWindows.agent", &json!(null)).unwrap();
    }
    settle(&mut h, "the history", |a| agent_sessions::history(&a.session).len() == 4);
    assert!(h.query_by_label_contains("History · 4").is_some());
    assert!(h.query_by_label_contains("in another window").is_some(), "the running job is marked");
    snapshot(&mut h, "agent-history-filters");

    // Search narrows the list.
    h.state_mut().ui.agent.history_query = "parser".into();
    h.run_steps(3);
    assert!(h.query_by_label_contains("History · 2 of 4").is_some());
    assert!(h.query_by_label("Write the docs").is_none());
    // …and the status and agent filters too.
    h.state_mut().ui.agent.history_backend = "codex".into();
    h.run_steps(3);
    assert!(h.query_by_label_contains("History · 1 of 4").is_some());
    assert!(h.query_by_label("Parser benchmarks").is_some());
    h.state_mut().ui.agent.history_query = "nothing like this".into();
    h.run_steps(3);
    assert!(h.query_by_label("No session matches.").is_some());
    h.state_mut().ui.agent.history_query.clear();
    h.state_mut().ui.agent.history_backend.clear();
    h.state_mut().ui.agent.history_status = "running".into();
    h.run_steps(3);
    assert!(h.query_by_label_contains("History · 1 of 4").is_some());
    // Clicking the job running elsewhere does not open it here.
    h.get_by_label("Long migration").click();
    h.run_steps(3);
    assert!(h.state().session.agents.jobs.is_empty());
    assert!(h.state().session.status.contains("another CodeCraft window"));
    h.state_mut().ui.agent.history_status.clear();
    h.run_steps(2);

    // Clean Up asks first; Cancel removes nothing.
    h.get_by_label("Clean Up…").click();
    h.run_steps(3);
    assert!(h.query_by_label("Clean Up Agent Sessions").is_some());
    assert!(h.query_by_label_contains("2 session(s) and 0 review worktree(s) will be removed").is_some());
    snapshot(&mut h, "agent-history-cleanup");
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.state().ui.agent.cleanup.is_none());
    assert!(dir.join("old-1.json").is_file());
    // Delete removes the two old ones; the recent and the running one stay.
    h.get_by_label("Clean Up…").click();
    h.run_steps(3);
    h.get_by_label("Delete").click();
    h.run_steps(3);
    assert!(h.state_mut().session.agents.sessions.flush(Duration::from_secs(5)));
    assert!(!dir.join("old-1.json").exists() && !dir.join("old-2.json").exists());
    assert!(dir.join("new-1.json").is_file() && dir.join("live-1.json").is_file());
    assert!(h.query_by_label_contains("History · 2").is_some());
    h.state_mut().session.shutdown();
}
