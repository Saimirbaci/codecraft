//! The Terminal tool window driven headlessly: typing goes to the shell (not the editor), Ctrl+C
//! reaches the program, Cmd+K clears, Cmd+digit still toggles tool windows.
//! Set `CODECRAFT_SNAPSHOT_DIR` to also write a PNG for a visual check.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use serde_json::json;

use codecraft_engine::workspace::ToolWindowId;
use codecraft_ui_egui::App;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            h.run_steps(1);
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let text = h.state().session.terminal.active_tab().map(|t| t.screen.text()).unwrap_or_default();
    panic!("timed out waiting for {what}; terminal:\n{text}");
}

fn screen_has_line(app: &App, line: &str) -> bool {
    app.session.terminal.active_tab().is_some_and(|t| t.screen.text().lines().any(|l| l.trim_end() == line))
}

fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn type_text(h: &mut Harness<'_, App>, text: &str) {
    h.event(Event::Text(text.into()));
    h.run_steps(1);
}

#[test]
fn typing_in_the_terminal_runs_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("notes.txt"), "keep me\n").unwrap();
    let file = root.join("notes.txt");
    let open = root.clone();
    let mut h =
        Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    h.state_mut().session.execute("file.open", &json!({ "path": file })).unwrap();
    h.run_steps(2);

    // Opening a tab shows the window and focuses the grid.
    h.state_mut().session.execute("terminal.new", &json!({})).unwrap();
    settle(&mut h, "the shell prompt", |a| {
        a.session.terminal.active_tab().is_some_and(|t| t.screen.text().contains('$'))
    });
    settle(&mut h, "terminal focus", |a| a.session.terminal.tabs.len() == 1);
    h.run_steps(2);

    type_text(&mut h, "echo hi");
    h.key_press(Key::Enter);
    settle(&mut h, "`hi` from the shell", |a| screen_has_line(a, "hi"));
    let doc = h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap();
    assert_eq!(doc, "keep me\n", "keys went to the terminal, not the editor");

    // Ctrl+C reaches the program (the shell prints ^C and a fresh prompt).
    type_text(&mut h, "sleep 100");
    h.key_press(Key::Enter);
    h.run_steps(5);
    std::thread::sleep(Duration::from_millis(300));
    h.key_press_modifiers(Modifiers::CTRL, Key::C);
    type_text(&mut h, "echo done-$((2+3))");
    h.key_press(Key::Enter);
    settle(&mut h, "Ctrl+C then a command", |a| screen_has_line(a, "done-5"));
    snapshot(&mut h, "terminal");

    // Tab goes to the shell (completion), not to egui's focus traversal.
    type_text(&mut h, "echo tab");
    h.key_press(Key::Tab);
    h.run_steps(2);
    assert!(h.ctx.memory(|m| m.has_focus(egui::Id::new("codecraft-terminal-grid"))), "Tab kept the focus");
    type_text(&mut h, "-ok");
    h.key_press(Key::Enter);
    settle(&mut h, "the line typed around Tab", |a| screen_has_line(a, "tab-ok"));

    // Cmd+K clears the terminal (not Commit), Cmd+1 still toggles the Project window.
    h.key_press_modifiers(Modifiers::MAC_CMD | Modifiers::COMMAND, Key::K);
    settle(&mut h, "cleared", |a| a.session.terminal.active_tab().is_some_and(|t| !t.screen.text().contains("done-5")));
    let project_visible = h.state().session.ws.layout.is_visible(ToolWindowId::Project);
    h.key_press_modifiers(Modifiers::MAC_CMD | Modifiers::COMMAND, Key::Num1);
    h.run_steps(2);
    assert_ne!(h.state().session.ws.layout.is_visible(ToolWindowId::Project), project_visible);

    // Shutdown leaves nothing running.
    let pid = h.state().session.terminal.active_tab().and_then(|t| t.term.pid()).unwrap();
    h.state_mut().session.shutdown();
    let start = Instant::now();
    loop {
        let out = std::process::Command::new("/bin/ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stat.is_empty() || stat.starts_with('Z') {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "the shell survived shutdown");
        std::thread::sleep(Duration::from_millis(30));
    }
}
