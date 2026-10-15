//! The test runner UI driven headlessly: an imported JUnit report shows as a tree in the Run tool
//! window, clicking a test shows its output, the toolbar toggles filter the tree, and the
//! Coverage tool window lists an imported LCOV report.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_ui_egui::App;
use codecraft_workspace::{RunTabId, ToolWindowId};

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

const JUNIT: &str = r#"<?xml version="1.0" encoding="utf-8"?><testsuites><testsuite name="pytest">
<testcase classname="tests.test_math" name="test_add" time="0.004"><system-out>hello from add</system-out></testcase>
<testcase classname="tests.test_math" name="test_fail" time="0.010"><failure message="bad math">def test_fail():
&gt;       assert 1 + 1 == 3
E       AssertionError: bad math

tests/test_math.py:5: AssertionError</failure></testcase>
<testcase classname="tests.test_math" name="test_skip" time="0.000"><skipped message="later"/></testcase>
</testsuite></testsuites>"#;

#[test]
fn test_tree_selection_filters_and_coverage_window() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/test_math.py"),
        "def test_add():\n    assert 1 + 1 == 2\n\n\ndef test_fail():\n    assert 1 + 1 == 3\n\n\ndef test_skip():\n    pass\n",
    )
    .unwrap();
    std::fs::write(root.join("report.xml"), JUNIT).unwrap();
    std::fs::write(root.join("cov.lcov"), "SF:tests/test_math.py\nDA:1,1\nDA:2,1\nDA:5,1\nDA:6,0\nend_of_record\n")
        .unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));

    let r =
        h.state_mut().session.execute("run.importTestsFromFile", &json!({ "path": root.join("report.xml") })).unwrap();
    let tab = RunTabId(r["tab"].as_u64().unwrap());
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Run));
    settle(&mut h, "locations", |a| !a.session.testing.busy());
    h.run_steps(3);

    // The tree: root, the file suite, three tests; the summary in the toolbar.
    assert!(h.query_by_label("Test Results").is_some());
    assert!(h.query_by_label("tests/test_math.py").is_some());
    assert!(h.query_by_label_contains("Tests failed: 1, passed: 1, ignored: 1 of 3 tests").is_some());

    // Click a test: it is selected and its output replaces the console.
    h.get_by_label("test_fail").click();
    h.run_steps(3);
    let run = h.state().session.testing.run(tab).unwrap();
    let key = run.view.selected.clone().unwrap();
    let (si, ti) = codecraft_engine::testing::test_key(&key).unwrap();
    assert_eq!(run.test(si, ti).unwrap().name, "test_fail");
    assert!(h.query_by_label_contains("AssertionError: bad math").is_some());
    // Relative to the project in the Run window (QA M14 #27); agents keep the absolute path.
    assert!(h.query_by_label("at tests/test_math.py:5").is_some(), "the failure point link");
    let summary = h.state_mut().session.execute("test.summary", &json!({ "tab": tab.0 })).unwrap().to_string();
    assert!(summary.contains(&root.join("tests/test_math.py").display().to_string()), "{summary}");

    // Show Passed off hides test_add; Show Ignored off hides test_skip.
    h.get_by_label("Show Passed").click();
    h.run_steps(3);
    assert!(h.query_by_label("test_add").is_none());
    h.get_by_label("Show Ignored").click();
    h.run_steps(3);
    assert!(h.query_by_label("test_skip").is_none());
    assert!(h.query_by_label("test_fail").is_some());

    // Jump to source opens the test function.
    h.state_mut().session.execute("test.navigate", &json!({ "tab": tab.0 })).unwrap();
    h.run_steps(2);
    let (e, d) = h.state().session.ws.active().unwrap();
    assert!(d.path.as_deref().unwrap().ends_with("tests/test_math.py"));
    assert_eq!(d.doc.char_to_line(e.selections.primary().head), 4);

    // Coverage: the tool window lists the report's files.
    h.state_mut().session.execute("coverage.importLcov", &json!({ "path": root.join("cov.lcov") })).unwrap();
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Coverage));
    assert!(h.query_by_label_contains("75.0% lines covered (3/4)").is_some());
    // Clicking the directory row collapses it (the file row disappears; the editor tab stays).
    let files = h.query_all_by_label("test_math.py").count();
    assert!(files >= 2, "the coverage row and the editor tab");
    h.get_by_label("tests").click();
    h.run_steps(3);
    assert_eq!(h.query_all_by_label("test_math.py").count(), files - 1);

    // Test History and Manage Coverage Reports open as windows.
    h.state_mut().session.execute("run.manageCoverageReports", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label_contains("cov.lcov  ·  75.0% lines").is_some(), "the report in the dialog");
}
