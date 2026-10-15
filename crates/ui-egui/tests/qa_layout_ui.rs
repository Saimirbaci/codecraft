//! M14 QA layout regressions found by rendering every tool window at 1280×800 and 900×700
//! (`qa_gallery_ui.rs`): empty bottom tool windows shrinking to 80 px for good, the left stripe's
//! two button groups overlapping in a 700 px window, toolbars whose right end ran out of the
//! panel (Profiler, Debug), and the Agent Access settings squeezing its labels to a word per line.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::epaint::{ClippedShape, Shape};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn project(base: &Path) -> PathBuf {
    let root = base.join("p");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/app.py"), "def main():\n    work()\n\ndef work():\n    return sum(range(10))\n")
        .unwrap();
    std::fs::write(
        root.join("profile.txt"),
        "main (src/app.py:2);work (src/app.py:5) 60\nmain (src/app.py:2);idle 40\n",
    )
    .unwrap();
    root.canonicalize().unwrap()
}

fn harness(root: &Path, home: &Path, size: egui::Vec2) -> Harness<'static, App> {
    let (root, home) = (root.to_path_buf(), home.to_path_buf());
    Harness::builder().with_size(size).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(root));
        app.session.isolate_home(&home);
        app
    })
}

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

fn only(h: &mut Harness<'_, App>, tw: ToolWindowId) {
    for other in ToolWindowId::ALL {
        h.state_mut().session.ws.layout.hide(other);
    }
    h.state_mut().session.ws.layout.show(tw);
    h.run_steps(4);
}

#[test]
fn empty_bottom_tool_windows_keep_their_height() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut h = harness(&project(&base), &base.join("home"), egui::vec2(1280.0, 800.0));
    settle(&mut h, "the project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    // An empty Run ("Nothing has run yet") used to shrink to the 80 px minimum, and every bottom
    // tool window opened that small afterwards.
    only(&mut h, ToolWindowId::Run);
    only(&mut h, ToolWindowId::Find);
    only(&mut h, ToolWindowId::Debug);
    let height = h.state().session.ws.layout.bottom_height;
    assert!(height >= 200.0, "the bottom tool window is {height} px high");
}

/// Text painted in the left stripe (its icons), as rectangles.
fn stripe_icons(h: &Harness<'_, App>) -> Vec<egui::Rect> {
    let mut out = Vec::new();
    for ClippedShape { shape, clip_rect } in &h.output().shapes {
        let mut visit = |s: &Shape| {
            if let Shape::Text(t) = s {
                let r = egui::Rect::from_min_size(t.pos, t.galley.size());
                if r.max.x < 40.0 && r.min.y > 60.0 && clip_rect.intersects(r) && !t.galley.text().trim().is_empty() {
                    out.push(r);
                }
            }
        };
        match shape {
            Shape::Vec(v) => v.iter().for_each(&mut visit),
            s => visit(s),
        }
    }
    out
}

#[test]
fn stripe_buttons_do_not_overlap_in_a_small_window() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut h = harness(&project(&base), &base.join("home"), egui::vec2(900.0, 700.0));
    settle(&mut h, "the project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.run_steps(3);
    let icons = stripe_icons(&h);
    assert!(icons.len() >= 15, "the stripe's icons: {}", icons.len());
    for (i, a) in icons.iter().enumerate() {
        for b in icons.iter().skip(i + 1) {
            assert!(!a.shrink(1.0).intersects(b.shrink(1.0)), "stripe icons overlap: {a:?} and {b:?}");
        }
    }
}

#[test]
fn profiler_toolbar_fits_a_900px_window() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = project(&base);
    let mut h = harness(&root, &base.join("home"), egui::vec2(900.0, 700.0));
    settle(&mut h, "the project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.execute("run.openProfilerSnapshot", &json!({ "path": root.join("profile.txt") })).unwrap();
    settle(&mut h, "the snapshot", |a| a.session.profiler.snapshot.is_some() && a.session.profiler.loading.is_none());
    only(&mut h, ToolWindowId::Profiler);
    let right_stripe = 900.0 - 38.0;
    for label in ["Open…", "✕"] {
        let r = h.get_by_label(label).rect();
        assert!(r.max.x <= right_stripe, "`{label}` ends at {} (the panel ends at {right_stripe})", r.max.x);
    }
    h.state_mut().session.execute("profiler.selectTab", &json!({ "tab": "methodList" })).unwrap();
    h.run_steps(3);
    let total = h.get_by_label("Total").rect();
    assert!(total.max.x <= right_stripe, "the Total column ends at {}", total.max.x);
}

#[test]
fn agent_access_labels_get_room_at_900px() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut h = harness(&project(&base), &base.join("home"), egui::vec2(900.0, 700.0));
    h.run_steps(2);
    h.state_mut().session.execute("app.settings", &json!({ "page": "Tools › Agent Access" })).unwrap();
    h.run_steps(4);
    let label = h.get_by_label("Read project and IDE state").rect();
    assert!(label.width() > 100.0 && label.height() < 30.0, "squeezed to {label:?}");
    let allow = h.get_all_by_label("Allow").next().unwrap().rect();
    assert!(allow.max.x < 900.0 && allow.min.x > label.max.x, "{allow:?} vs {label:?}");
}

fn has_debugpy() -> bool {
    std::process::Command::new("python3").args(["-c", "import debugpy"]).output().is_ok_and(|o| o.status.success())
}

#[test]
fn debug_status_does_not_cover_the_toolbar_at_900px() {
    if !has_debugpy() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = project(&base);
    let mut h = harness(&root, &base.join("home"), egui::vec2(900.0, 700.0));
    settle(&mut h, "the project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    let app = root.join("src/app.py");
    std::fs::write(&app, "def work():\n    total = 1\n    return total\n\nwork()\n").unwrap();
    h.state_mut().session.execute("debug.breakpoint.set", &json!({ "path": app, "line": 3 })).unwrap();
    h.state_mut().session.execute("debug.atCaret", &json!({ "path": app })).unwrap();
    settle(&mut h, "the stop", |a| a.session.debug.is_stopped() && !a.session.debug.settling());
    only(&mut h, ToolWindowId::Debug);
    let status =
        h.get_all_by_label_contains("Paused").map(|n| n.rect()).find(|r| r.max.y < 680.0).expect("the Debug status");
    for tab in ["Threads & Variables", "Registers"] {
        let r = h.get_by_label(tab).rect();
        assert!(!status.intersects(r), "the status {status:?} covers `{tab}` {r:?}");
    }
    assert!(status.max.x <= 900.0 - 38.0, "{status:?}");
    h.state_mut().session.execute("debug.stop", &serde_json::Value::Null).ok();
    h.run_steps(3);
    h.state_mut().session.shutdown();
}
