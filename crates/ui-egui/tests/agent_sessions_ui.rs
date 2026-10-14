//! The History of saved agent sessions in the Agent tool window: a session saved by one run of
//! the app is listed by the next one, opened with a click and resumed from the Resume field.
//! Fake agent CLI only; every file lives in a temp home. Set `CODECRAFT_SNAPSHOT_DIR` to also
//! write PNGs of each state.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::agent_sessions;
use codecraft_engine::agents::{Activity, JobStatus};
use codecraft_ui_egui::App;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
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

/// The app on `proj`, its per-user files under `home`, the fake agent configured.
fn app<'a>(proj: &Path, home: &Path, fake: &Path) -> Harness<'a, App> {
    let open = proj.to_path_buf();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .with_pixels_per_point(2.0)
        .build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    let s = &mut h.state_mut().session;
    s.isolate_home(home);
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    s.execute("view.toolWindows.agent", &json!(null)).unwrap();
    h.run_steps(2);
    h
}

#[test]
fn history_lists_saved_sessions_and_resumes_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("README.md"), "# demo\n").unwrap();
    let log = tmp.path().join("args.log");
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            r#"printf '%s\n' "$@" >> '{log}'
echo '{{"type":"system","subtype":"init","session_id":"s-hist"}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Polished."}}]}}}}'
echo '{{"type":"result","subtype":"success","is_error":false,"duration_ms":3,"total_cost_usd":0.05,"session_id":"s-hist"}}'
"#,
            log = log.display()
        ),
    );

    // First run of the app: one finished job.
    {
        let mut h = app(&proj, &home, &fake);
        let s = &mut h.state_mut().session;
        s.execute("agent.start", &json!({ "backend": "claude", "mode": "auto", "prompt": "Polish the README" }))
            .unwrap();
        settle(&mut h, "the job", |a| a.session.agents.jobs.first().is_some_and(|j| j.status == JobStatus::Finished));
        h.state_mut().session.shutdown();
    }

    // Second run: the History lists it.
    let mut h = app(&proj, &home, &fake);
    settle(&mut h, "the history", |a| !agent_sessions::history(&a.session).is_empty());
    assert!(h.query_by_label_contains("History · 1").is_some());
    assert!(h.state().session.agents.jobs.is_empty());
    snapshot(&mut h, "agent-history");

    // A click reopens it with its feed.
    h.get_by_label("Polish the README").click();
    settle(&mut h, "the reopened job", |a| !a.session.agents.jobs.is_empty());
    let job = h.state().session.agents.jobs.first().unwrap();
    assert_eq!(job.status, JobStatus::Finished);
    assert!(job.activity.iter().any(|a| matches!(a, Activity::Text { text } if text == "Polished.")));
    assert!(h.query_by_label_contains("History · 0").is_none(), "an open session leaves the History");
    snapshot(&mut h, "agent-history-reopened");

    // Resume it.
    std::fs::write(&log, "").unwrap();
    h.state_mut().ui.agent.follow_up = "And the changelog".into();
    h.run_steps(2);
    h.get_by_label("Resume").click();
    settle(&mut h, "the resumed turn", |a| {
        a.session.agents.jobs.first().is_some_and(|j| {
            j.status == JobStatus::Finished
                && j.activity.iter().filter(|a| matches!(a, Activity::Prompt { .. })).count() == 2
        })
    });
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("--resume\ns-hist"), "{args}");
    snapshot(&mut h, "agent-history-resumed");

    // Rename it from the job header.
    let key = h.state().session.agents.jobs.first().and_then(|j| j.session_key.clone()).unwrap();
    h.state_mut().ui.agent.renaming = Some((key, "README polish".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    settle(&mut h, "the rename", |a| a.session.agents.jobs.first().is_some_and(|j| j.title == "README polish"));
    h.state_mut().session.shutdown();
}
