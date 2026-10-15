//! The Profiler tool window driven headlessly (M14 track PF): the view tabs (flame graph,
//! icicle, call tree, method list, timeline), comparing against a baseline snapshot (the diff
//! legend and the method list's Δ column), a method's hot lines opening the editor at that line,
//! and the per-line numbers the editor gutter shows.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

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
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

#[test]
fn profiler_views_baseline_and_hot_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/app.py"),
        "def main():\n    work()\n\ndef work():\n    total = sum(range(10))\n    return total\n",
    )
    .unwrap();
    // py-spy style collapsed stacks: the line each sample was at.
    std::fs::write(
        root.join("now.txt"),
        "main (src/app.py:2);work (src/app.py:5) 60\nmain (src/app.py:2);work (src/app.py:6) 20\nmain (src/app.py:2);idle 20\n",
    )
    .unwrap();
    std::fs::write(root.join("base.txt"), "main (src/app.py:2);work (src/app.py:5) 20\nmain (src/app.py:2);idle 80\n")
        .unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.isolate_home(&dir.path().join("home"));
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));

    // The baseline first (so it's a recent snapshot), then the current one.
    for name in ["base.txt", "now.txt"] {
        h.state_mut().session.execute("run.openProfilerSnapshot", &json!({ "path": root.join(name) })).unwrap();
        settle(&mut h, name, |a| {
            a.session.profiler.snapshot.as_ref().is_some_and(|s| s.path.ends_with(name))
                && a.session.profiler.loading.is_none()
        });
    }
    h.run_steps(3);
    for tab in ["Flame Graph", "Icicle", "Call Tree", "Method List", "Timeline"] {
        assert!(h.query_by_label(tab).is_some(), "{tab}");
    }

    // Tabs switch views; the timeline says when a format has no sample times.
    h.get_by_label("Icicle").click();
    h.run_steps(3);
    assert_eq!(h.state().session.profiler.tab, ProfilerTab::Icicle);
    h.get_by_label("Timeline").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("has no sample times").is_some());

    // Compare with the baseline: the legend and the Δ column.
    h.state_mut().session.execute("profiler.compareWith", &json!({ "path": root.join("base.txt") })).unwrap();
    settle(&mut h, "baseline", |a| {
        a.session.profiler.snapshot.as_ref().is_some_and(|s| s.baseline.is_some())
            && a.session.profiler.baseline_loading.is_none()
    });
    h.get_by_label("Flame Graph").click();
    h.run_steps(3);
    assert_eq!(h.state().session.profiler.tab, ProfilerTab::FlameGraph);
    assert!(h.query_by_label("larger share than in").is_some());
    h.get_by_label("Method List").click();
    h.run_steps(3);
    assert!(h.query_by_label("Δ self (pts)").is_some());

    // Select `work`: its hot lines; clicking one opens the editor at that line.
    let work = {
        let snap = h.state().session.profiler.snapshot.as_ref().unwrap();
        snap.profile.frames.iter().position(|f| f.name == "work").unwrap()
    };
    h.state_mut().session.execute("profiler.selectMethod", &json!({ "frame": work })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Hot lines").is_some());
    h.get_by_label_contains("line 6").click();
    h.run_steps(3);
    let (e, d) = h.state().session.ws.active().unwrap();
    assert!(d.path.as_ref().unwrap().ends_with("src/app.py"));
    assert_eq!(d.doc.offset_to_position(e.selections.primary().head).line, 5);

    // The editor's gutter numbers come from the same per-line weights.
    let hits = h.state_mut().session.execute("profiler.lineHits", &json!({ "path": root.join("src/app.py") })).unwrap();
    let lines: Vec<(u64, f64)> = hits["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["line"].as_u64().unwrap(), l["totalPercent"].as_f64().unwrap()))
        .collect();
    assert_eq!(lines, vec![(2, 100.0), (5, 60.0), (6, 20.0)]);
    h.run_steps(3);
    h.state_mut().session.shutdown();
}
