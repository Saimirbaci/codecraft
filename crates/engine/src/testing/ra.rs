//! Rust test locations from rust-analyzer (M13 track RC): when a cargo test run ends, the files
//! of its tests (and the open Rust files) are asked for `experimental/runnables`; each test's
//! location becomes the runnable's (the name of the `#[test]` function as rust-analyzer resolved
//! it, through `#[path]`, `mod` re-declarations and macros). Without rust-analyzer, or for tests
//! it doesn't report, the module-path walk (`locate.rs`) stays.

use std::path::{Path, PathBuf};

use codecraft_lsp::{ResponseError, ServerKind, TestRunnable};
use codecraft_workspace::RunTabId;
use serde_json::Value;

use super::{Framework, SuiteKind, TestLocation, TestSuite};
use crate::Session;

/// Files asked per run.
const MAX_FILES: usize = 24;

/// Ask rust-analyzer for the runnables of the run's test files (each file once per run).
pub(crate) fn request(s: &mut Session, tab: RunTabId) {
    let Some(run) = s.testing.run(tab) else { return };
    if run.framework != Framework::Cargo || s.server_status(ServerKind::RustAnalyzer) != crate::LspStatus::Ready {
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for t in run.suites.iter().flat_map(|s| &s.tests) {
        if let Some(l) = &t.location
            && !files.contains(&l.path)
        {
            files.push(l.path.clone());
        }
    }
    for d in s.ws.docs() {
        if let Some(p) = d.path.as_ref().filter(|p| p.extension().is_some_and(|e| e == "rs"))
            && !files.contains(p)
        {
            files.push(p.clone());
        }
    }
    for path in files.into_iter().take(MAX_FILES) {
        if !s.testing.ra_requested.insert((tab, path.clone())) {
            continue;
        }
        let p = path.clone();
        let _ = s.lsp_send_to(
            ServerKind::RustAnalyzer,
            |c| c.runnables(&p),
            crate::lsp::Pending::Rust(crate::rust_tools::RustPending::Runnables { tab, path }),
        );
    }
}

/// The char column of a UTF-16 column on `line` of `text`.
fn char_column(text: &str, line: u32, utf16: u32) -> usize {
    let Some(l) = text.lines().nth(line as usize) else { return utf16 as usize };
    let mut units = 0u32;
    for (i, c) in l.chars().enumerate() {
        if units >= utf16 {
            return i;
        }
        units = units.saturating_add(u32::try_from(c.len_utf16()).unwrap_or(2));
    }
    l.chars().count()
}

fn normalized(name: &str) -> String {
    name.replace('-', "_")
}

/// Whether a runnable can be the test of `suite` (same crate when both are known).
fn same_crate(suite: &TestSuite, r: &TestRunnable) -> bool {
    let Some(crate_name) = suite.crate_name.as_deref() else { return true };
    match suite.kind {
        SuiteKind::Integration => {
            r.path.file_stem().is_some_and(|s| normalized(&s.to_string_lossy()) == normalized(crate_name))
                || r.path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|d| normalized(&d.to_string_lossy()) == normalized(crate_name))
        }
        SuiteKind::Unit => {
            r.package.as_deref().is_none_or(|p| normalized(p) == normalized(crate_name)) || r.package.is_none()
        }
        _ => true,
    }
}

/// Apply `runnables` to the run of `tab`; returns how many tests got a location.
pub fn apply(
    run: &mut super::TestRun,
    runnables: &[TestRunnable],
    text_of: &mut dyn FnMut(&Path) -> Option<String>,
) -> usize {
    let mut updated = 0;
    for r in runnables.iter().filter(|r| r.is_test) {
        let mut column = None;
        for suite in &mut run.suites {
            if !same_crate(suite, r) {
                continue;
            }
            for t in suite.tests.iter_mut().filter(|t| t.name == r.test_path) {
                let col = *column.get_or_insert_with(|| {
                    text_of(&r.path)
                        .map(|text| char_column(&text, r.line, r.utf16_column))
                        .unwrap_or(r.utf16_column as usize)
                });
                t.location =
                    Some(TestLocation { path: crate::normalize_path(&r.path), line: r.line as usize, column: col });
                updated += 1;
            }
        }
    }
    updated
}

pub(crate) fn on_response(s: &mut Session, tab: RunTabId, path: PathBuf, result: Result<Value, ResponseError>) {
    let Ok(value) = result else { return };
    let runnables = codecraft_lsp::decode_test_runnables(value);
    if runnables.is_empty() {
        return;
    }
    let open: Vec<(PathBuf, String)> =
        s.ws.docs()
            .filter_map(|d| d.path.clone().map(|p| (p, d.doc.text())))
            .filter(|(p, _)| runnables.iter().any(|r| r.path == *p) || *p == path)
            .collect();
    let mut text_of = |p: &Path| -> Option<String> {
        open.iter().find(|(o, _)| o == p).map(|(_, t)| t.clone()).or_else(|| codecraft_fs::read_text_file(p).ok())
    };
    let Some(run) = s.testing.run_mut(tab) else { return };
    if apply(run, &runnables, &mut text_of) > 0 {
        s.testing.rebuild_gutter();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::testing::{TestCase, TestRun};

    #[test]
    fn utf16_columns_become_char_columns() {
        assert_eq!(char_column("    fn adds() {}", 0, 7), 7);
        assert_eq!(char_column("/* 😀 */ fn x() {}", 0, 12), 11);
        assert_eq!(char_column("a", 5, 3), 3);
    }

    #[test]
    fn runnables_set_locations_of_matching_tests() {
        let mut run = TestRun::new(RunTabId(1), "t", String::new(), Framework::Cargo, None, "/w".into());
        let mut unit = TestSuite::new("unittests src/lib.rs", SuiteKind::Unit, Some("demo".into()), None);
        unit.tests.push(TestCase::new("tests::adds"));
        unit.tests.push(TestCase::new("tests::other"));
        let mut other = TestSuite::new("unittests src/lib.rs", SuiteKind::Unit, Some("other_crate".into()), None);
        other.tests.push(TestCase::new("tests::adds"));
        run.suites = vec![unit, other];
        let r = TestRunnable {
            test_path: "tests::adds".into(),
            is_test: true,
            package: Some("demo".into()),
            path: "/w/src/lib.rs".into(),
            line: 9,
            utf16_column: 7,
        };
        let n = apply(&mut run, &[r], &mut |_| Some("\n".repeat(9) + "    fn adds() {}"));
        assert_eq!(n, 1);
        let loc = run.suites[0].tests[0].location.clone().unwrap();
        assert_eq!((loc.line, loc.column), (9, 7));
        assert!(run.suites[0].tests[1].location.is_none());
        assert!(run.suites[1].tests[0].location.is_none(), "another crate's test with the same path");
    }
}
