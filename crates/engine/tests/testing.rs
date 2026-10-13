//! The test runner through engine commands on real projects: a temp Cargo crate (test tree,
//! locations, rerun failed, history, export/import, gutter status, coverage or its install hint)
//! and a pytest project. Each part skips cleanly when its tool (cargo, pytest) is missing.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::testing::{TestRun, TestStatus};
use codecraft_workspace::{NotificationLevel, RunTabId};

const WAIT: Duration = Duration::from_secs(300);

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            let console: Vec<String> = s.ws.runs.iter().flat_map(|r| r.lines.iter().map(|l| l.text.clone())).collect();
            panic!("timed out waiting for {what}; console:\n{}", console.join("\n"));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn available(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn open_project(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    s
}

fn run_of(s: &Session, tab: RunTabId) -> &TestRun {
    s.testing.run(tab).unwrap_or_else(|| panic!("tab {tab:?} is not a test run"))
}

/// Wait until the run in `tab` finished and its locations were resolved.
fn wait_run(s: &mut Session, tab: RunTabId) {
    pump_until(s, "the test run", |s| s.testing.run(tab).is_some_and(|r| r.finished) && !s.testing.busy());
}

fn tab_of(v: &Value) -> RunTabId {
    RunTabId(v["tab"].as_u64().unwrap())
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(1, 2), 3);\n    }\n\n    #[test]\n    fn fails() {\n        println!(\"some output\");\n        assert_eq!(add(1, 2), 4);\n    }\n\n    #[test]\n    #[ignore]\n    fn slow() {}\n}\n";

#[test]
fn cargo_test_tree_rerun_history_and_coverage() {
    if !available("cargo", &["--version"]) {
        eprintln!("skipping: cargo not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // Canonical (macOS temp dirs are behind the /var → /private/var symlink; Cargo reports
    // canonical paths and run-at-caret matches files against them).
    let root = &dir.path().canonicalize().unwrap();
    write(root, "Cargo.toml", "[package]\nname = \"tdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n");
    write(root, "src/lib.rs", LIB);
    let mut s = open_project(root);

    // Run the tests: the tree is built from the console, locations resolved afterwards.
    let tab = tab_of(&s.execute("cargo.test", &Value::Null).unwrap());
    wait_run(&mut s, tab);
    let run = run_of(&s, tab);
    let counts = run.counts();
    assert_eq!((counts.passed, counts.failed, counts.ignored), (1, 1, 1), "{:#?}", run.suites);
    let fails = run.suites.iter().flat_map(|s| &s.tests).find(|t| t.name == "tests::fails").unwrap();
    assert_eq!(fails.status, TestStatus::Failed);
    assert!(fails.output.contains("some output"), "{:?}", fails.output);
    let loc = fails.location.as_ref().expect("located");
    assert!(loc.path.ends_with("src/lib.rs"));
    assert_eq!(loc.line, 14);
    let failure = fails.failure.as_ref().expect("failure point");
    assert!(failure.path.is_absolute() && failure.path.ends_with("src/lib.rs"));
    assert_eq!(failure.line, 16);
    // The gutter shows the last status at the test function.
    let lib = root.join("src/lib.rs");
    let gutter = s.testing.gutter_tests(&lib).cloned().unwrap_or_default();
    assert_eq!(gutter.get(&14), Some(&TestStatus::Failed));
    assert_eq!(gutter.get(&9), Some(&TestStatus::Passed));

    // The tree as an agent sees it; select, navigate and step through failures.
    let tree = s.execute("test.tree", &json!({ "tab": tab.0 })).unwrap();
    assert_eq!(tree["counts"]["failed"], 1);
    let rows = tree["rows"].as_array().unwrap();
    let key = rows.iter().find(|r| r["label"] == "fails").unwrap()["key"].as_str().unwrap().to_string();
    let next = s.execute("test.nextFailed", &json!({ "tab": tab.0 })).unwrap();
    assert_eq!(next.as_str(), Some(key.as_str()));
    s.execute("test.navigate", &json!({ "tab": tab.0 })).unwrap();
    let (e, d) = s.ws.active().unwrap();
    assert!(d.path.as_deref().unwrap().ends_with("src/lib.rs"));
    assert_eq!(d.doc.char_to_line(e.selections.primary().head), 14);
    s.execute("test.toggleShowPassed", &json!({ "value": false })).unwrap();
    let tree = s.execute("test.tree", &json!({ "tab": tab.0 })).unwrap();
    assert!(!tree["rows"].as_array().unwrap().iter().any(|r| r["label"] == "adds"));
    s.execute("test.toggleShowPassed", &Value::Null).unwrap();

    // Rerun Failed Tests runs exactly the failed test.
    let rerun = tab_of(&s.execute("test.rerunFailed", &json!({ "tab": tab.0 })).unwrap());
    assert!(
        s.ws.run(rerun).unwrap().command_line.contains("--exact tests::fails"),
        "{}",
        s.ws.run(rerun).unwrap().command_line
    );
    wait_run(&mut s, rerun);
    let c = run_of(&s, rerun).counts();
    assert_eq!((c.passed, c.failed, c.ignored), (0, 1, 0));

    // Rerun (Cmd+R on the tab) repeats the exact invocation.
    s.ws.active_run = Some(rerun);
    let again = tab_of(&s.execute("run.rerun", &Value::Null).unwrap());
    assert_eq!(s.ws.run(again).unwrap().command_line, s.ws.run(rerun).unwrap().command_line);
    wait_run(&mut s, again);

    // History: three runs recorded in the project; open the first one.
    let history = s.execute("run.testHistory", &Value::Null).unwrap();
    let list = history.as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert!(root.join(".codecraft/test-history.json").is_file());
    let oldest = list.last().unwrap()["index"].as_u64().unwrap();
    let opened = tab_of(&s.execute("test.openHistory", &json!({ "index": oldest })).unwrap());
    let c = run_of(&s, opened).counts();
    assert_eq!((c.passed, c.failed, c.ignored), (1, 1, 1));
    assert!(run_of(&s, opened).imported);
    assert!(s.execute("test.openHistory", &json!({ "index": 99 })).is_err());

    // Export and import.
    let export = root.join("results.json");
    s.execute("test.export", &json!({ "tab": tab.0, "path": export })).unwrap();
    let imported = tab_of(&s.execute("run.importTestsFromFile", &json!({ "path": export })).unwrap());
    assert_eq!(run_of(&s, imported).counts(), run_of(&s, tab).counts());
    assert_eq!(s.execute("run.importTestsFromFile", &Value::Null).unwrap()["pick"], true);
    assert!(s.testing.import_requested);

    // Coverage: with cargo-llvm-cov a report; without it an install hint, no crash.
    let has_llvm_cov = available("cargo", &["llvm-cov", "--version"]);
    let result = s.execute("run.runWithCoverage", &json!({ "path": lib, "line": 10 }));
    if has_llvm_cov {
        let cov_tab = tab_of(&result.unwrap());
        let _ = cov_tab;
        pump_until(&mut s, "the coverage report", |s| {
            s.testing.coverage.is_some() || s.ws.notifications.iter().any(|n| n.title == "Coverage")
        });
        let report = s.testing.coverage.as_ref().expect("a coverage report");
        assert!(report.file(&lib).is_some_and(|f| f.lines_found() > 0));
        let reports = s.execute("run.manageCoverageReports", &Value::Null).unwrap();
        assert_eq!(reports.as_array().unwrap().len(), 1);
    } else {
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cargo install cargo-llvm-cov"), "{err}");
        let n = s.ws.notifications.last().unwrap();
        assert_eq!(n.level, NotificationLevel::Warning);
        assert!(n.message.contains("llvm-tools-preview"));
    }
    s.shutdown();
}

#[test]
fn coverage_reports_and_gutter_marks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "fn a() {}\nfn b() {}\n");
    let lcov = root.join("cov.lcov");
    std::fs::write(&lcov, "SF:src/a.rs\nDA:1,3\nDA:2,0\nend_of_record\n").unwrap();
    let mut s = open_project(root);
    let summary = s.execute("coverage.importLcov", &json!({ "path": lcov })).unwrap();
    assert_eq!((summary["linesHit"].as_u64(), summary["linesFound"].as_u64()), (Some(1), Some(2)));
    assert_eq!(summary["rows"][0]["name"], "src");
    let a = root.join("src/a.rs");
    assert_eq!(s.testing.editor_coverage(&a).and_then(|c| c.line(0)), Some(true));
    assert_eq!(s.testing.editor_coverage(&a).and_then(|c| c.line(1)), Some(false));
    assert_eq!(s.execute("coverage.toggleEditorMarks", &Value::Null).unwrap(), false);
    assert!(s.testing.editor_coverage(&a).is_none());
    s.execute("coverage.toggleEditorMarks", &json!({ "value": true })).unwrap();

    let list = s.execute("run.manageCoverageReports", &Value::Null).unwrap();
    assert!(s.testing.reports_open);
    let id = list[0]["id"].as_str().unwrap().to_string();
    s.execute("coverage.closeReport", &Value::Null).unwrap();
    assert!(s.testing.coverage.is_none());
    s.execute("coverage.openReport", &json!({ "id": id })).unwrap();
    assert_eq!(s.testing.coverage.as_ref().map(|c| c.id.clone()), Some(id.clone()));
    s.execute("coverage.deleteReport", &json!({ "id": id })).unwrap();
    assert!(s.testing.coverage.is_none());
    assert!(s.execute("coverage.openReport", &json!({ "id": id })).is_err());
    assert!(s.execute("coverage.openReport", &json!({ "id": "../../x" })).is_err());
    // Not a test or coverage-capable configuration.
    assert!(s.execute("run.runWithCoverage", &json!({ "path": a, "line": 0 })).is_err());
    s.shutdown();
}

#[test]
fn pytest_tree_from_junit() {
    let python = ["python3", "python"].into_iter().find(|p| available(p, &["-m", "pytest", "--version"]));
    let Some(_python) = python else {
        eprintln!("skipping: pytest not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "pyproject.toml", "[tool.pytest.ini_options]\naddopts = \"-p no:cacheprovider\"\n");
    write(
        root,
        "tests/test_math.py",
        "def test_add():\n    print(\"hello from add\")\n    assert 1 + 1 == 2\n\n\ndef test_fail():\n    assert 1 + 1 == 3\n\n\nclass TestGroup:\n    def test_x(self):\n        pass\n",
    );
    let mut s = open_project(root);
    let index = s.run_configs.iter().position(|c| c.name == "pytest").expect("a pytest configuration");
    let tab = tab_of(&s.execute("run.run", &json!({ "config": index })).unwrap());
    wait_run(&mut s, tab);
    let run = run_of(&s, tab);
    let c = run.counts();
    assert_eq!((c.passed, c.failed), (2, 1), "{:#?}", run.suites);
    assert_eq!(run.suites.len(), 1);
    let tests = &run.suites[0].tests;
    let add = tests.iter().find(|t| t.name == "test_add").unwrap();
    assert_eq!(add.output, "hello from add\n");
    assert!(add.duration_ms.is_some());
    assert_eq!(add.location.as_ref().map(|l| l.line), Some(0));
    let x = tests.iter().find(|t| t.name == "TestGroup::test_x").unwrap();
    assert_eq!(x.location.as_ref().map(|l| l.line), Some(10));
    let fail = tests.iter().find(|t| t.name == "test_fail").unwrap();
    assert_eq!(fail.failure.as_ref().map(|l| l.line), Some(6));

    let rerun = tab_of(&s.execute("test.rerunFailed", &json!({ "tab": tab.0 })).unwrap());
    assert!(s.ws.run(rerun).unwrap().command_line.contains("tests/test_math.py::test_fail"));
    wait_run(&mut s, rerun);
    let c = run_of(&s, rerun).counts();
    assert_eq!((c.passed, c.failed), (0, 1));
    s.shutdown();
}
