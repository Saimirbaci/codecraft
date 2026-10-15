//! M15 track PP in the UI, driven headlessly: dragging across the threads timeline selects a
//! time range that every view follows (and the range chip / Whole Run clear it), the Memory view
//! of a tracemalloc profile with its chart, the allocations at the peak and the snapshot at the
//! peak, and Profile in the gutter run icon's menu.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::profiler::ProfilerTab;
use codecraft_ui_egui::App;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {} {:?}", h.state().session.status, h.state().session.profiler.error);
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

fn harness(root: &Path, home: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.isolate_home(home);
    settle(&mut h, "project", |a| a.session.ws.project.is_some() && !a.session.jobs_running());
    h
}

fn open(h: &mut Harness<'_, App>, path: &Path) {
    h.state_mut().session.execute("run.openProfilerSnapshot", &json!({ "path": path })).unwrap();
    let name = path.file_name().unwrap().to_owned();
    settle(h, "snapshot", |a| {
        a.session.profiler.loading.is_none()
            && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.path.file_name() == Some(name.as_os_str()))
    });
}

#[test]
fn dragging_across_the_timeline_selects_a_time_range() {
    let (dir, root) = project();
    let trace = root.join("trace.xml");
    std::fs::write(&trace, include_str!("../../profile/tests/fixtures/xctrace-time-profile.xml")).unwrap();
    let mut h = harness(&root, &dir.path().join("home"));
    open(&mut h, &trace);
    h.state_mut().session.execute("profiler.selectTab", &json!({ "tab": "timeline" })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("drag across the strips").is_some());
    let row = h.get_by_label_contains("Thread Main Thread").rect();
    // The strip starts after the 200 px thread name.
    let strip_left = row.left() + 204.0;
    let strip_w = row.right() - 6.0 - strip_left;
    let at = |f: f32| egui::pos2(strip_left + f * strip_w, row.center().y);
    h.hover_at(at(0.25));
    h.run_steps(1);
    h.drag_at(at(0.25));
    h.run_steps(1);
    for f in [0.3, 0.45, 0.6, 0.75] {
        h.hover_at(at(f));
        h.run_steps(1);
    }
    h.drop_at(at(0.75));
    h.run_steps(1);
    settle(&mut h, "the range", |a| {
        a.session.profiler.loading.is_none()
            && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.analysis.range.is_some())
    });
    let (range, duration, total, whole) = {
        let s = h.state().session.profiler.snapshot.as_ref().unwrap();
        let whole = codecraft_engine::profiler::profile::Analysis::new(&s.profile, None).tree.total;
        (s.analysis.range.unwrap(), s.profile.duration_ns.unwrap() as f64, s.analysis.tree.total, whole)
    };
    let (a, b) = (range.0 as f64 / duration, range.1 as f64 / duration);
    assert!((a - 0.25).abs() < 0.03 && (b - 0.75).abs() < 0.03, "{a} {b}");
    assert!(total < whole, "{total} of {whole}");
    // The toolbar shows the range; Whole Run clears it.
    assert!(h.query_by_label_contains("⏱").is_some());
    h.get_by_label("Whole Run").click();
    h.run_steps(1);
    settle(&mut h, "the whole run", |a| {
        a.session.profiler.loading.is_none()
            && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.analysis.range.is_none())
    });
    assert!(h.query_by_label_contains("⏱").is_none());
    // A click (no drag) on a thread still selects it.
    h.get_by_label_contains("Thread Main Thread").click();
    h.run_steps(1);
    settle(&mut h, "the thread", |a| {
        a.session.profiler.loading.is_none()
            && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.analysis.thread == Some(0))
    });
    assert!(h.state().session.profiler.snapshot.as_ref().unwrap().analysis.range.is_none());
    h.state_mut().session.shutdown();
}

#[test]
fn the_memory_view_shows_the_peak() {
    let (dir, root) = project();
    std::fs::write(
        root.join("grow.py"),
        "import time\n\n\ndef build(n):\n    return [bytes(1000) for _ in range(n)]\n",
    )
    .unwrap();
    let grow = root.join("grow.py").display().to_string();
    let frames = json!([{ "name": "<module>", "file": grow, "line": 9 }, { "name": "build", "file": grow, "line": 5 }]);
    let peak_doc = json!({
        "codecraft": "tracemalloc", "current": 30_000_000, "peak": 31_000_000, "when": "at the peak (0.4 s after the start)",
        "traces": [{ "size": 30_000_000, "count": 30000, "frames": frames }],
        "interval": 0.02, "timeline": [[20.0, 1000, 1500], [400.0, 30_000_000, 31_000_000], [600.0, 2000, 30_000_000]],
        "peakSnapshot": { "time": 400.0, "current": 30_000_000, "file": null, "top": [] }
    });
    let doc = json!({
        "codecraft": "tracemalloc", "current": 2000, "peak": 31_000_000, "when": "when the program ended",
        "traces": [{ "size": 2000, "count": 2, "frames": frames }],
        "interval": 0.02, "timeline": [[20.0, 1000, 1500], [400.0, 30_000_000, 31_000_000], [600.0, 2000, 30_000_000]],
        "peakSnapshot": { "time": 400.0, "current": 30_000_000, "file": "run.tracemalloc.peak.json",
            "top": [{ "name": "build", "file": grow, "line": 5, "size": 29_000_000, "count": 29000, "callers": ["<module> (grow.py:9)"] }] }
    });
    let main = root.join("run.tracemalloc.json");
    std::fs::write(&main, serde_json::to_vec(&doc).unwrap()).unwrap();
    std::fs::write(root.join("run.tracemalloc.peak.json"), serde_json::to_vec(&peak_doc).unwrap()).unwrap();
    let mut h = harness(&root, &dir.path().join("home"));
    open(&mut h, &main);
    h.run_steps(3);
    h.get_by_label("Memory").click();
    h.run_steps(3);
    assert_eq!(h.state().session.profiler.tab, ProfilerTab::Memory);
    assert!(h.query_by_label_contains("Peak 29.6 MB").is_some());
    assert!(h.query_by_label("Memory over time").is_some());
    assert!(h.query_by_label_contains("Live at the snapshot nearest the peak").is_some());
    // An allocation site opens the editor at its line.
    h.get_by_label_contains("29000").click();
    h.run_steps(3);
    let (e, d) = h.state().session.ws.active().unwrap();
    assert!(d.path.as_ref().unwrap().ends_with("grow.py"));
    assert_eq!(d.doc.offset_to_position(e.selections.primary().head).line, 4);
    // The whole snapshot at the peak.
    h.get_by_label("Open Snapshot at Peak").click();
    h.run_steps(1);
    settle(&mut h, "the peak snapshot", |a| {
        a.session.profiler.loading.is_none()
            && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.path.ends_with("run.tracemalloc.peak.json"))
    });
    assert_eq!(h.state().session.profiler.snapshot.as_ref().unwrap().profile.total(), 30_000_000);
    h.run_steps(3);
    // Snapshots without a memory timeline have no Memory tab.
    std::fs::write(root.join("cpu.txt"), "main;work 3\n").unwrap();
    open(&mut h, &root.join("cpu.txt"));
    h.run_steps(3);
    assert!(h.query_by_label("Memory").is_none());
    h.state_mut().session.shutdown();
}

#[test]
fn the_gutter_run_menu_offers_profile() {
    let (dir, root) = project();
    std::fs::write(root.join("test_main.py"), "def test_one():\n    assert True\n").unwrap();
    // A stand-in interpreter (the project's venv): the profile starts and ends at once.
    std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
    let python = root.join(".venv/bin/python");
    std::fs::write(&python, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut h = harness(&root, &dir.path().join("home"));
    h.state_mut().session.execute("file.open", &json!({ "path": root.join("test_main.py") })).unwrap();
    h.run_steps(3);
    let rect = h.state().ui.editor_rect.unwrap();
    let line_h = h.state().ui.editor_line_h;
    let at = egui::pos2(rect.left() + 9.0, rect.top() + line_h * 0.5);
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    h.event(egui::Event::PointerMoved(at));
    h.run_steps(1);
    h.event(button(true));
    h.run_steps(1);
    h.event(button(false));
    h.run_steps(3);
    assert!(h.query_by_label("⏱  Profile Memory").is_some());
    h.get_by_label("⏱  Profile").click();
    h.run_steps(3);
    let job = h.state().session.profiler.jobs.last().cloned().unwrap();
    assert_eq!(job.title, "Profile pytest test_one");
    let tab = h.state().session.ws.run(job.tab.unwrap()).unwrap().command_line.clone();
    assert!(tab.contains("-m cProfile"), "{tab}");
    assert!(tab.contains("test_main.py::test_one"), "{tab}");
    assert!(h.query_by_label("⏱  Profile Memory").is_none(), "the menu closed");
    settle(&mut h, "the profile to end", |a| !a.session.profiler.busy());
    h.state_mut().session.shutdown();
}
