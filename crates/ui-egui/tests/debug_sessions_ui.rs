//! M15 track DM driven headlessly: the Debug tool window's session tabs (select, close, Stop
//! All) and the status bar's session count, with sessions that have no adapter and with two
//! real debugpy sessions (skipped when debugpy is missing).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::debug::{DebugSession, DebugStatus};
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {} {:?}", h.state().session.status, h.state().session.debug.sessions());
}

fn harness(root: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.isolate_home(&root.join(".home"));
    settle(&mut h, "project", |a| a.session.ws.project.is_some() && !a.session.jobs_running());
    h
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("demo");
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

#[test]
fn session_tabs_select_and_close_sessions() {
    let (_dir, root) = project();
    let mut h = harness(&root);
    let s = &mut h.state_mut().session;
    let one = s.debug.add_inert_session(DebugSession {
        name: "api-server".into(),
        status: Some(DebugStatus::Exited { code: Some(0) }),
        ..DebugSession::default()
    });
    let two = s.debug.add_inert_session(DebugSession {
        name: "web-client".into(),
        status: Some(DebugStatus::Failed { message: "boom".into() }),
        ..DebugSession::default()
    });
    s.ws.layout.show(ToolWindowId::Debug);
    h.run_steps(3);
    assert_eq!(h.state().session.debug.selected_id(), Some(one));
    // The unselected session's tab selects it.
    h.get_by_label("web-client").click();
    h.run_steps(3);
    assert_eq!(h.state().session.debug.selected_id(), Some(two));
    // Its close button closes it; one session left, no tabs.
    let closes: Vec<_> = h.query_all_by_label("×").collect();
    assert_eq!(closes.len(), 2, "a close button per session tab");
    closes[1].click();
    h.run_steps(3);
    assert_eq!(h.state().session.debug.session_count(), 1);
    assert_eq!(h.state().session.debug.selected_id(), Some(one));
    assert!(h.query_by_label("×").is_none(), "one session: no tabs");
}

fn debugpy() -> bool {
    codecraft_engine::dap::discover::find_debugpy(None).is_ok()
}

#[test]
fn two_live_sessions_show_in_the_status_bar_and_stop_all() {
    if !debugpy() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let (_dir, root) = project();
    std::fs::write(root.join("server.py"), "import time\nx = 1\ntime.sleep(0)  # stop\nprint(x)\n").unwrap();
    std::fs::write(root.join("client.py"), "import time\ny = 2\ntime.sleep(0)  # stop\nprint(y)\n").unwrap();
    let mut h = harness(&root);
    let s = &mut h.state_mut().session;
    for file in ["server.py", "client.py"] {
        s.execute("debug.breakpoint.set", &json!({ "path": root.join(file), "line": 3 })).unwrap();
    }
    s.execute("debug.start", &json!({ "path": root.join("server.py") })).unwrap();
    settle(&mut h, "the server's stop", |a| a.session.debug.is_stopped() && !a.session.debug.settling());
    h.state_mut().session.execute("debug.start", &json!({ "path": root.join("client.py") })).unwrap();
    settle(&mut h, "both stops", |a| {
        let d = &a.session.debug;
        d.sessions().iter().filter(|x| matches!(x.status, DebugStatus::Stopped { .. })).count() == 2 && !d.settling()
    });
    h.run_steps(3);
    assert!(h.query_by_label("2 debug sessions (2 paused)").is_some(), "the status bar counts the sessions");
    // The client is selected (it stopped last) and has a tab (the server's name is also an
    // editor tab's): selecting the server brings its stop back.
    assert_eq!(h.state().session.debug.session.as_ref().unwrap().name, "client.py");
    assert!(h.query_all_by_label("server.py").count() >= 2, "the session tab and the editor tab");
    let server = h.state().session.debug.sessions().iter().find(|x| x.name == "server.py").unwrap().id;
    h.state_mut().session.execute("debug.selectSession", &json!({ "session": server })).unwrap();
    h.run_steps(3);
    let selected = h.state().session.debug.session.clone().unwrap();
    assert_eq!(selected.name, "server.py");
    assert!(selected.execution.as_ref().is_some_and(|(p, _)| p.ends_with("server.py")));
    h.get_by_label("■ Stop All").click();
    settle(&mut h, "both to end", |a| !a.session.debug.busy());
    assert_eq!(h.state().session.debug.live_count(), 0);
    h.state_mut().session.shutdown();
}
