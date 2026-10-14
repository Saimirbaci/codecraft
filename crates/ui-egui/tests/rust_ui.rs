//! M12 track RS driven headlessly: the Rust REPL tool window (against a fake evcxr) and the macro
//! expansion popup.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::macro_expand::MacroExpansion;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

#[test]
fn rust_repl_window_and_macro_popup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let fake = root.join("evcxr");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf 'Welcome to evcxr. For help, type :help\\n>> '\nwhile IFS= read -r line; do printf 'got: %s\\n\\302\\221>> ' \"$line\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe({
        let file = root.join("main.rs");
        move |_| App::ephemeral(Some(file))
    });
    h.state_mut().session.isolate_home(&root.join("home"));
    h.state_mut().session.rust.repl.program = Some(fake);
    h.state_mut().session.ws.layout.remember_size(ToolWindowId::RustRepl, 380.0);
    h.state_mut().session.execute("tools.rust.rustRepl", &json!({})).unwrap();
    settle(&mut h, "REPL ready", |a| a.session.rust.repl.ready);
    h.run_steps(2);
    assert!(h.query_by_label("Rust REPL").is_some(), "the tool window header");
    assert!(h.query_by_label("Ready").is_some(), "the REPL status");

    h.state_mut().session.execute("rust.repl.execute", &json!({ "code": "40 + 2" })).unwrap();
    settle(&mut h, "output", |a| a.session.rust.repl.last_status.as_deref() == Some("ok"));
    h.run_steps(2);
    assert!(h.query_by_label("got: 40 + 2").is_some(), "the evaluated output");
    h.get_by_label("Clear").click();
    h.run_steps(2);
    assert!(h.state().session.rust.repl.transcript.is_empty());
    h.state_mut().session.execute("rust.repl.stop", &json!({})).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label("Not running").is_some());
    assert!(!h.state().session.rust.repl.is_alive());

    // The macro expansion popup (the engine fills it from rust-analyzer; set directly here).
    h.state_mut().session.rust.macros.view = Some(MacroExpansion {
        name: "vec!".into(),
        text: "<[_]>::into_vec(Box::new([1, 2]))".into(),
        formatted: true,
        path: root.join("main.rs"),
        line: 0,
        request: 1,
    });
    h.run_steps(3);
    assert!(h.query_by_label("Recursive expansion of vec! macro").is_some(), "the popup title");
    h.get_by_label("Copy").click();
    h.run_steps(2);
    assert_eq!(h.state().session.clipboard.as_ref().map(|c| c.0.as_str()), Some("<[_]>::into_vec(Box::new([1, 2]))"));
    h.get_by_label("Close").click();
    h.run_steps(2);
    assert!(h.state().session.rust.macros.view.is_none());
    h.state_mut().session.shutdown();
}
