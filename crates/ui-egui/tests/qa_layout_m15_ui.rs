//! M15 QA (track QB) regressions found in the second gallery (`qa_gallery_m15_ui.rs`): glyphs
//! drawn as empty boxes (the Database, Hierarchy and Rust REPL stripe icons, Agents' ⇄ button,
//! Reset Frame's ⤺, the ✕ close buttons, ↶/↵/◆/⊘), the Python Console's Variables panel taking
//! the whole width for a long value, the Memory view's rows not lining up with its header (and
//! absolute paths), and Connect an External Agent doing nothing from Find Action.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/glyphs.rs"]
mod glyphs;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::python_console::ConsoleVar;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            h.run_steps(2);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

fn project(base: &Path) -> PathBuf {
    let root = base.join("p");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.txt"), "hello\n").unwrap();
    std::fs::write(root.join("profile.txt"), "main;work 3\nmain;idle 1\n").unwrap();
    root.canonicalize().unwrap()
}

fn harness(root: &Path, home: &Path, size: egui::Vec2) -> Harness<'static, App> {
    let (root, home) = (root.to_path_buf(), home.to_path_buf());
    let mut h = Harness::builder().with_size(size).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(root));
        app.session.isolate_home(&home);
        app
    });
    settle(&mut h, "the project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h
}

fn only(h: &mut Harness<'_, App>, tw: ToolWindowId) {
    for other in ToolWindowId::ALL {
        h.state_mut().session.ws.layout.hide(other);
    }
    h.state_mut().session.ws.layout.show(tw);
    h.run_steps(4);
}

/// The platform fonts the app installs on macOS have every glyph the UI uses (elsewhere the
/// fonts differ, so the check only reports).
fn assert_no_tofu(h: &Harness<'_, App>, what: &str) {
    let missing = glyphs::missing_glyphs(h);
    if cfg!(target_os = "macos") {
        assert!(missing.is_empty(), "{what}: drawn as empty boxes: {missing:?}");
    } else if !missing.is_empty() {
        eprintln!("{what}: drawn as empty boxes: {missing:?}");
    }
}

#[test]
fn stripe_toolbar_and_close_glyphs_are_in_the_fonts() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = project(&base);
    let mut h = harness(&root, &base.join("home"), egui::vec2(1280.0, 800.0));
    // The stripes (Database 🛢, Hierarchy ⊶, Rust REPL 🦀 were boxes) and the Agent window (⇄).
    only(&mut h, ToolWindowId::Agent);
    assert_no_tofu(&h, "the stripes and the Agent tool window");
    assert!(h.query_by_label("⇄").is_some(), "Connect an External Agent's button");
    // The Debug toolbar's Reset Frame (⤺).
    only(&mut h, ToolWindowId::Debug);
    assert_no_tofu(&h, "the Debug tool window");
    // The Profiler's close button (✕ → ✖).
    h.state_mut().session.execute("run.openProfilerSnapshot", &json!({ "path": root.join("profile.txt") })).unwrap();
    settle(&mut h, "the snapshot", |a| a.session.profiler.snapshot.is_some() && a.session.profiler.loading.is_none());
    only(&mut h, ToolWindowId::Profiler);
    assert_no_tofu(&h, "the Profiler");
    assert!(h.query_by_label("✖").is_some(), "the close button");
    // Search Everywhere's file markers (◆).
    h.state_mut().session.execute("navigate.searchEverywhere", &json!({ "query": "main" })).unwrap();
    h.run_steps(4);
    assert_no_tofu(&h, "Search Everywhere");
    h.state_mut().session.shutdown();
}

#[test]
fn python_console_variables_leave_room_for_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut h = harness(&project(&base), &base.join("home"), egui::vec2(900.0, 700.0));
    h.state_mut().session.python.console.vars = vec![
        ConsoleVar {
            name: "items".into(),
            type_name: "list".into(),
            value: format!(
                "[{}]",
                (0..60).map(|i| format!("{{'price': {i}, 'qty': 2}}")).collect::<Vec<_>>().join(", ")
            ),
            viewable: true,
            shape: Some("(60, 2)".into()),
        },
        ConsoleVar { name: "n".into(), type_name: "int".into(), value: "3".into(), viewable: false, shape: None },
    ];
    only(&mut h, ToolWindowId::PythonConsole);
    // The Variables panel used to grow to its longest value and squeeze the transcript to a
    // few words per line.
    let vars = h.get_by_label("Variables").rect();
    assert!(vars.left() >= 900.0 * 0.45, "the Variables panel starts at {}", vars.left());
    let hint = h.get_by_label("Press ▶ or type code below to start the Python Console.").rect();
    assert!(hint.height() < 40.0, "the transcript is squeezed: {hint:?}");
}

#[test]
fn memory_view_rows_have_columns_and_relative_locations() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = project(&base);
    let grow = root.join("grow.py");
    std::fs::write(&grow, "def build(n):\n    return [bytes(1000) for _ in range(n)]\n").unwrap();
    let g = grow.display().to_string();
    let doc = json!({
        "codecraft": "tracemalloc", "current": 2000, "peak": 31_000_000, "when": "when the program ended",
        "traces": [{ "size": 2000, "count": 2, "frames": [{ "name": "build", "file": g, "line": 2 }] }],
        "interval": 0.02, "timeline": [[20.0, 1000, 1500], [400.0, 30_000_000, 31_000_000], [600.0, 2000, 30_000_000]],
        "peakSnapshot": { "time": 400.0, "current": 30_000_000, "file": null,
            "top": [{ "name": "build", "file": g, "line": 2, "size": 29_000_000, "count": 29000, "callers": [] }] }
    });
    let main = root.join("run.tracemalloc.json");
    std::fs::write(&main, serde_json::to_vec(&doc).unwrap()).unwrap();
    let mut h = harness(&root, &base.join("home"), egui::vec2(900.0, 700.0));
    h.state_mut().session.execute("run.openProfilerSnapshot", &json!({ "path": main })).unwrap();
    settle(&mut h, "the snapshot", |a| a.session.profiler.snapshot.is_some() && a.session.profiler.loading.is_none());
    h.state_mut().session.execute("profiler.selectTab", &json!({ "tab": "memory" })).unwrap();
    only(&mut h, ToolWindowId::Profiler);
    let row = h.get_by_label_contains("build grow.py:2").rect();
    let size = h.get_by_label("Size").rect();
    let count = h.get_by_label("Count").rect();
    assert!(size.left() > row.left() + 300.0 && count.left() > size.left(), "{row:?} {size:?} {count:?}");
    assert!(h.query_by_label_contains(&base.display().to_string()).is_none(), "no absolute paths in the rows");
    // A click opens the line.
    h.get_by_label_contains("build grow.py:2").click();
    h.run_steps(3);
    assert!(h.state().session.ws.active().is_some_and(|(_, d)| d.path.as_deref() == Some(grow.as_path())));
    h.state_mut().session.shutdown();
}

#[test]
fn connect_an_external_agent_opens_from_find_action() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut h = harness(&project(&base), &base.join("home"), egui::vec2(1280.0, 800.0));
    // What Find Action / a menu does: run the command (its result was dropped before).
    h.state_mut().session.execute("agent.connectExternal", &json!(null)).unwrap();
    h.run_steps(3);
    assert!(h.state().ui.agent.connect.is_some());
    assert!(h.query_by_label("Connect an External Agent").is_some());
    h.state_mut().session.shutdown();
}
