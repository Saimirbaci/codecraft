//! Test output parsers, fed one console line at a time while the process runs: libtest's human
//! output (`cargo test`), pytest (`-v` result lines, `-rA` summaries, the FAILURES section),
//! CTest and `zig test`. JUnit XML (pytest's `--junitxml`) is in `junit.rs`.

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;

use codecraft_cargo::{TestEvent, TestOutcome};

use super::{Framework, SuiteKind, TestCase, TestLocation, TestStatus, TestSuite};

/// Captured output kept per test (bytes); the rest is dropped with a marker.
pub(crate) const MAX_TEST_OUTPUT: usize = 256 * 1024;
/// Tests per run (a runaway parameterised suite must not exhaust memory).
pub(crate) const MAX_TESTS: usize = 100_000;

/// Incremental parser state for one run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Parser {
    /// cargo: the last `Running …`/`Doc-tests …` header, not yet bound to a suite.
    pending: Option<TestSuite>,
    /// The suite receiving results.
    current: Option<usize>,
    /// Where captured output lines go: (suite, test).
    capture: Option<(usize, usize)>,
    /// pytest: inside `=== FAILURES ===` / `=== ERRORS ===`.
    in_failures: bool,
    /// GoogleTest's console (track CL).
    gtest: super::cpp::gtest::Console,
}

impl Parser {
    pub(crate) fn feed(&mut self, fw: Framework, suites: &mut Vec<TestSuite>, line: &str) {
        let line = line.trim_end_matches(['\r', '\n']);
        match fw {
            Framework::Cargo => self.feed_cargo(suites, line),
            Framework::Pytest => self.feed_pytest(suites, line),
            Framework::CTest => self.feed_ctest(suites, line),
            Framework::Zig => self.feed_zig(suites, line),
            Framework::GoogleTest => super::cpp::gtest::feed(&mut self.gtest, suites, line),
            // Catch2, doctest and Boost.Test results come from their XML report at the end.
            Framework::Catch2 | Framework::Doctest | Framework::BoostTest => {}
        }
    }

    /// The process ended: close captures, finish suites; tests still running were terminated.
    pub(crate) fn finish(&mut self, suites: &mut [TestSuite]) {
        self.end_capture(suites);
        for suite in suites.iter_mut() {
            suite.finished = true;
            for t in suite.tests.iter_mut().filter(|t| t.status == TestStatus::Running) {
                t.status = TestStatus::Failed;
                t.message = Some("Terminated".into());
            }
        }
        self.current = None;
    }

    fn end_capture(&mut self, suites: &mut [TestSuite]) {
        if let Some((si, ti)) = self.capture.take()
            && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
        {
            finalize_output(t);
        }
    }

    fn append(&self, suites: &mut [TestSuite], line: &str) {
        if let Some((si, ti)) = self.capture
            && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
        {
            push_output(&mut t.output, line);
        }
    }

    // ------------------------------------------------------------ cargo / libtest

    fn feed_cargo(&mut self, suites: &mut Vec<TestSuite>, line: &str) {
        if let Some(name) = libtest_section(line) {
            self.end_capture(suites);
            let si = self.current.unwrap_or_else(|| suites.len().saturating_sub(1));
            self.capture = test_index(suites, si, name, false).map(|ti| (si, ti));
            return;
        }
        if self.capture.is_some() {
            if !(line == "failures:" || line == "successes:" || line.starts_with("test result: ")) {
                self.append(suites, line);
                return;
            }
            self.end_capture(suites);
        }
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Running ").filter(|_| t.len() < line.len()) {
            self.pending = Some(cargo_suite(rest));
            self.current = None;
            return;
        }
        if let Some(krate) = t.strip_prefix("Doc-tests ").filter(|_| t.len() < line.len()) {
            let krate = krate.trim().to_string();
            self.pending = Some(TestSuite::new(format!("Doc-tests {krate}"), SuiteKind::Doc, Some(krate), None));
            self.current = None;
            return;
        }
        match codecraft_cargo::parse_test_line(line) {
            Some(TestEvent::SuiteStarted { count }) => {
                let mut suite =
                    self.pending.take().unwrap_or_else(|| TestSuite::new("Tests", SuiteKind::Other, None, None));
                suite.expected = Some(count);
                suites.push(suite);
                self.current = Some(suites.len() - 1);
            }
            Some(TestEvent::Result { name, outcome }) => {
                let si = self.suite_or_default(suites);
                let Some(ti) = test_index(suites, si, &name, true) else { return };
                if let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti)) {
                    t.status = match outcome {
                        TestOutcome::Ok => TestStatus::Passed,
                        TestOutcome::Failed => TestStatus::Failed,
                        TestOutcome::Ignored => TestStatus::Ignored,
                    };
                    if t.duration_ms.is_none() {
                        t.duration_ms = report_time(line);
                    }
                    if outcome == TestOutcome::Ignored
                        && let Some((_, reason)) = line.split_once(" ... ignored, ")
                    {
                        t.message = Some(reason.trim().to_string());
                    }
                }
            }
            Some(TestEvent::SuiteFinished { .. }) => {
                if let Some(s) = self.current.and_then(|si| suites.get_mut(si)) {
                    s.finished = true;
                    s.duration_ms = line.split_once("finished in ").and_then(|(_, d)| seconds_ms(d));
                }
                self.current = None;
            }
            None => {
                // `test a::b has been running for over 60 seconds`
                if let Some(name) =
                    line.strip_prefix("test ").and_then(|r| r.strip_suffix(" has been running for over 60 seconds"))
                {
                    let si = self.suite_or_default(suites);
                    if let Some(ti) = test_index(suites, si, name, true)
                        && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
                    {
                        t.status = TestStatus::Running;
                    }
                }
            }
        }
    }

    fn suite_or_default(&mut self, suites: &mut Vec<TestSuite>) -> usize {
        match self.current {
            Some(si) if si < suites.len() => si,
            _ => {
                let suite =
                    self.pending.take().unwrap_or_else(|| TestSuite::new("Tests", SuiteKind::Other, None, None));
                suites.push(suite);
                let si = suites.len() - 1;
                self.current = Some(si);
                si
            }
        }
    }

    // ------------------------------------------------------------ pytest

    fn feed_pytest(&mut self, suites: &mut Vec<TestSuite>, line: &str) {
        if line.starts_with("====") || line.starts_with("____ ") && line.ends_with(" ____") || is_banner(line, '_') {
            self.end_capture(suites);
            if is_banner(line, '=') || line.starts_with("====") {
                let title = line.trim_matches(|c| c == '=' || c == ' ');
                self.in_failures = matches!(title, "FAILURES" | "ERRORS");
                return;
            }
            if self.in_failures {
                // `____ TestGroup.test_param[2] ____` (or `ERROR at setup of test_x`)
                let head = line.trim_matches(|c| c == '_' || c == ' ');
                let head = head.rsplit(" of ").next().unwrap_or(head);
                self.capture = find_pytest_test(suites, head);
            }
            return;
        }
        if self.capture.is_some() {
            self.append(suites, line);
            return;
        }
        if let Some((nodeid, status, message)) = pytest_result(line) {
            let (file, name) = nodeid.split_once("::").unwrap_or((nodeid, nodeid));
            let si = match suites.iter().position(|s| s.source.as_deref() == Some(std::path::Path::new(file))) {
                Some(si) => si,
                None => {
                    suites.push(TestSuite::new(file, SuiteKind::File, None, Some(PathBuf::from(file))));
                    suites.len() - 1
                }
            };
            if let Some(ti) = test_index(suites, si, name, true)
                && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
            {
                t.status = status;
                if t.message.is_none() {
                    t.message = message;
                }
            }
        }
    }

    // ------------------------------------------------------------ ctest

    fn feed_ctest(&mut self, suites: &mut Vec<TestSuite>, line: &str) {
        static RE: OnceLock<Option<Regex>> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(
                r"^\s*\d+/\d+\s+Test\s+#\d+:\s+(\S+)\s+\.*\s*(?:\*\*\*)?([A-Za-z][A-Za-z ]*?)(?::[^\d]*)?\s+([0-9.]+)\s+sec",
            )
            .ok()
        });
        if let Some(c) = re.as_ref().and_then(|re| re.captures(line)) {
            self.end_capture(suites);
            let name = c.get(1).map_or("", |m| m.as_str());
            let word = c.get(2).map_or("", |m| m.as_str()).trim();
            let status = match word {
                "Passed" => TestStatus::Passed,
                "Skipped" | "Not Run" | "Disabled" => TestStatus::Ignored,
                _ => TestStatus::Failed,
            };
            if suites.is_empty() {
                suites.push(TestSuite::new("CTest", SuiteKind::Other, None, None));
            }
            let si = suites.len() - 1;
            if let Some(ti) = test_index(suites, si, name, true)
                && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
            {
                t.status = status;
                t.duration_ms = c.get(3).and_then(|m| seconds_ms(m.as_str()));
                if status == TestStatus::Failed {
                    t.message = Some(word.to_string());
                    self.capture = Some((si, ti));
                }
            }
            return;
        }
        let t = line.trim_start();
        if t.starts_with("Start ") || line.contains("% tests passed") || t.starts_with("The following tests") {
            self.end_capture(suites);
            if let Some(s) = suites.last_mut().filter(|_| line.contains("% tests passed")) {
                s.finished = true;
            }
            return;
        }
        if self.capture.is_some() {
            self.append(suites, line);
        }
    }

    // ------------------------------------------------------------ zig

    fn feed_zig(&mut self, suites: &mut Vec<TestSuite>, line: &str) {
        static RE: OnceLock<Option<Regex>> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"^\d+/\d+\s+(.+?)\.\.\.(OK|FAIL|SKIP|LEAK)(?:\s*\((.*)\))?\s*$").ok());
        if let Some(c) = re.as_ref().and_then(|re| re.captures(line)) {
            self.end_capture(suites);
            let full = c.get(1).map_or("", |m| m.as_str());
            let (module, name) = match full.split_once(".test.") {
                Some((m, n)) => (m, n),
                None => ("zig", full.strip_prefix("test.").unwrap_or(full)),
            };
            let si = match suites.iter().position(|s| s.name == module) {
                Some(si) => si,
                None => {
                    suites.push(TestSuite::new(module, SuiteKind::File, None, None));
                    suites.len() - 1
                }
            };
            let status = match c.get(2).map_or("", |m| m.as_str()) {
                "OK" => TestStatus::Passed,
                "SKIP" => TestStatus::Ignored,
                _ => TestStatus::Failed,
            };
            if let Some(ti) = test_index(suites, si, name, true)
                && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
            {
                t.status = status;
                t.message = c.get(3).map(|m| m.as_str().to_string());
                if status == TestStatus::Failed {
                    self.capture = Some((si, ti));
                }
            }
            return;
        }
        if line.starts_with("All ") && line.ends_with(" tests passed.")
            || line.contains(" passed; ") && line.ends_with(" failed.")
            || line.starts_with("error: the following test command failed")
        {
            self.end_capture(suites);
            for s in suites.iter_mut() {
                s.finished = true;
            }
            return;
        }
        if self.capture.is_some() {
            self.append(suites, line);
        }
    }
}

// ---------------------------------------------------------------- helpers

/// The index of test `name` in suite `si`; created (status Running) when `create` is set.
pub(crate) fn test_index(suites: &mut [TestSuite], si: usize, name: &str, create: bool) -> Option<usize> {
    let total: usize = suites.iter().map(|s| s.tests.len()).sum();
    let suite = suites.get_mut(si)?;
    if let Some(i) = suite.tests.iter().rposition(|t| t.name == name) {
        return Some(i);
    }
    if !create || total >= MAX_TESTS {
        return None;
    }
    suite.tests.push(TestCase::new(name));
    Some(suite.tests.len() - 1)
}

/// Append a line to captured output, within [`MAX_TEST_OUTPUT`].
pub(crate) fn push_output(out: &mut String, line: &str) {
    const MARK: &str = "… (output truncated)";
    if out.len() >= MAX_TEST_OUTPUT {
        if !out.ends_with(MARK) {
            out.push_str(MARK);
        }
        return;
    }
    out.push_str(line);
    out.push('\n');
}

/// Trim the blank lines libtest/pytest leave around a section and pull out the failure point and
/// message (a panic location, a pytest `file.py:12: Error` line or a Zig stack line).
pub(crate) fn finalize_output(t: &mut TestCase) {
    let trimmed = t.output.trim_matches('\n');
    if trimmed.len() != t.output.len() {
        t.output = format!("{trimmed}\n");
    }
    if t.output == "\n" {
        t.output.clear();
    }
    if t.failure.is_none()
        && let Some((loc, message)) = failure_point(&t.output)
    {
        t.failure = Some(loc);
        if t.message.is_none() {
            t.message = message;
        }
    }
}

/// Where a test failed, and the failure message, from its captured output.
pub(crate) fn failure_point(output: &str) -> Option<(TestLocation, Option<String>)> {
    let lines: Vec<&str> = output.lines().collect();
    // Rust panics: `thread 'x' (123) panicked at src/lib.rs:26:9:` (message on the next lines), or
    // the pre-1.73 `thread 'x' panicked at 'message', src/lib.rs:26:9`.
    for (i, line) in lines.iter().enumerate() {
        let Some((_, rest)) = line.split_once("panicked at ") else { continue };
        if let Some(quoted) = rest.strip_prefix('\'') {
            let (message, loc) = quoted.rsplit_once("', ")?;
            if let Some(loc) = parse_file_line_col(loc) {
                return Some((loc, Some(message.to_string())));
            }
            continue;
        }
        let Some(loc) = parse_file_line_col(rest.trim_end_matches(':')) else { continue };
        let message: Vec<&str> = lines
            .get(i + 1..)
            .unwrap_or(&[])
            .iter()
            .take_while(|l| !l.is_empty() && !l.starts_with("note: ") && !l.starts_with("stack backtrace:"))
            .copied()
            .collect();
        let message = (!message.is_empty()).then(|| message.join("\n"));
        return Some((loc, message));
    }
    // pytest: the last `path.py:12: AssertionError` line, message from the `E   ` lines.
    if let Some((loc, kind)) = lines.iter().rev().find_map(|l| {
        let (file, rest) = l.split_once(".py:")?;
        let (num, kind) = rest.split_once(": ")?;
        let line = num.parse::<usize>().ok()?;
        (!file.contains(' ')).then(|| {
            (TestLocation { path: PathBuf::from(format!("{file}.py")), line: line.saturating_sub(1), column: 0 }, kind)
        })
    }) {
        let e: Vec<&str> = lines.iter().filter_map(|l| l.strip_prefix("E ")).map(str::trim).collect();
        let message = if e.is_empty() { Some(kind.to_string()) } else { Some(e.join("\n")) };
        return Some((loc, message));
    }
    // Zig: `/path/a.zig:3:16: 0x1045b88b7 in test.fails (test)`
    lines.iter().find_map(|l| {
        let (loc, rest) = l.split_once(": 0x")?;
        rest.contains(" in test").then_some(())?;
        parse_file_line_col(loc).map(|loc| (loc, None))
    })
}

/// `path:line:col` (1-based) → a 0-based location.
fn parse_file_line_col(s: &str) -> Option<TestLocation> {
    let (rest, col) = s.trim().rsplit_once(':')?;
    let (path, line) = rest.rsplit_once(':')?;
    let line = line.parse::<usize>().ok()?;
    let column = col.parse::<usize>().ok()?;
    if path.is_empty() {
        return None;
    }
    Some(TestLocation { path: PathBuf::from(path), line: line.saturating_sub(1), column: column.saturating_sub(1) })
}

/// `---- name stdout ----` / `---- name stderr ----`
fn libtest_section(line: &str) -> Option<&str> {
    let inner = line.strip_prefix("---- ")?.strip_suffix(" ----")?;
    inner.strip_suffix(" stdout").or_else(|| inner.strip_suffix(" stderr"))
}

/// A cargo `Running …` header: `unittests src/lib.rs (target/debug/deps/demo-0123abcd)` or
/// `tests/integration.rs (target/debug/deps/integration-4567)`.
fn cargo_suite(rest: &str) -> TestSuite {
    let rest = rest.trim();
    let (source, binary) = match rest.rsplit_once(" (") {
        Some((s, b)) => (s, b.strip_suffix(')').unwrap_or(b)),
        None => (rest, ""),
    };
    let crate_name = std::path::Path::new(binary).file_stem().map(|s| {
        let stem = s.to_string_lossy();
        match stem.rsplit_once('-') {
            Some((name, hash)) if !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()) => name.to_string(),
            _ => stem.into_owned(),
        }
    });
    let (kind, path) = match source.strip_prefix("unittests ") {
        Some(p) => (SuiteKind::Unit, p),
        None if source.starts_with("benches/") => (SuiteKind::Bench, source),
        None => (SuiteKind::Integration, source),
    };
    let name = match (&kind, &crate_name) {
        (SuiteKind::Unit, Some(c)) => format!("{c}: {path}"),
        _ => path.to_string(),
    };
    TestSuite::new(name, kind, crate_name.filter(|c| !c.is_empty()), Some(PathBuf::from(path)))
}

/// libtest `--report-time`: `test x ... ok <0.002s>`.
fn report_time(line: &str) -> Option<u64> {
    let (_, rest) = line.rsplit_once('<')?;
    seconds_ms(rest.strip_suffix('>')?)
}

/// `0.25s` / `1.5` / `0.01 sec` → milliseconds.
pub(crate) fn seconds_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let num: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let secs: f64 = num.parse().ok()?;
    (0.0..1e9).contains(&secs).then(|| (secs * 1000.0).round() as u64)
}

fn is_banner(line: &str, c: char) -> bool {
    line.len() > 6 && line.starts_with(c) && line.ends_with(c) && line.contains(' ')
}

/// A pytest result line: `tests/x.py::test_a PASSED [ 20%]` (`-v`) or `FAILED tests/x.py::t - msg`
/// (`-rA` summary).
fn pytest_result(line: &str) -> Option<(&str, TestStatus, Option<String>)> {
    const WORDS: [(&str, TestStatus); 6] = [
        ("PASSED", TestStatus::Passed),
        ("FAILED", TestStatus::Failed),
        ("ERROR", TestStatus::Failed),
        ("SKIPPED", TestStatus::Ignored),
        ("XFAIL", TestStatus::Ignored),
        ("XPASS", TestStatus::Passed),
    ];
    // Summary form.
    for (word, status) in WORDS {
        if let Some(rest) = line.strip_prefix(word).and_then(|r| r.strip_prefix(' ')) {
            let (nodeid, message) = match rest.split_once(" - ") {
                Some((n, m)) => (n, Some(m.trim().to_string())),
                None => (rest.trim(), None),
            };
            return nodeid.contains("::").then_some((nodeid, status, message));
        }
    }
    // Verbose form.
    let (nodeid, rest) = line.split_once(' ')?;
    if !nodeid.contains("::") || nodeid.starts_with('[') {
        return None;
    }
    let rest = rest.trim_start();
    let word = rest.split_whitespace().next()?;
    let status = WORDS.iter().find(|(w, _)| *w == word).map(|(_, s)| *s)?;
    let message = rest
        .strip_prefix(word)
        .map(str::trim)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.split_once(')'))
        .map(|(m, _)| m.to_string());
    Some((nodeid, status, message))
}

/// A FAILURES section header (`TestGroup.test_param[2]` or `test_fail`) → the test.
fn find_pytest_test(suites: &[TestSuite], head: &str) -> Option<(usize, usize)> {
    for (si, s) in suites.iter().enumerate() {
        if let Some(ti) = s.tests.iter().position(|t| t.name.replace("::", ".") == head) {
            return Some((si, ti));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO: &str = include_str!("fixtures/cargo_test.txt");
    const PYTEST: &str = include_str!("fixtures/pytest_v.txt");
    const CTEST: &str = include_str!("fixtures/ctest.txt");
    const ZIG: &str = include_str!("fixtures/zig_test.txt");

    fn parse(fw: Framework, text: &str) -> Vec<TestSuite> {
        let mut p = Parser::default();
        let mut suites = Vec::new();
        for line in text.lines() {
            p.feed(fw, &mut suites, line);
        }
        p.finish(&mut suites);
        suites
    }

    fn test<'a>(suites: &'a [TestSuite], suite: &str, name: &str) -> &'a TestCase {
        let s = suites.iter().find(|s| s.name == suite).unwrap_or_else(|| panic!("no suite {suite}"));
        s.tests.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no test {name} in {suite}"))
    }

    #[test]
    fn cargo_suites_results_and_output() {
        let suites = parse(Framework::Cargo, CARGO);
        let names: Vec<&str> = suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["demo: src/lib.rs", "tests/integration.rs", "Doc-tests demo"]);
        let lib = &suites[0];
        assert_eq!(lib.kind, SuiteKind::Unit);
        assert_eq!(lib.crate_name.as_deref(), Some("demo"));
        assert_eq!(lib.source.as_deref(), Some(std::path::Path::new("src/lib.rs")));
        assert_eq!(lib.expected, Some(6));
        assert_eq!(lib.tests.len(), 6);
        assert!(lib.finished);
        assert_eq!(lib.duration_ms, Some(0));
        assert_eq!(suites[1].crate_name.as_deref(), Some("integration"));
        assert_eq!(suites[1].kind, SuiteKind::Integration);

        let fails = test(&suites, "demo: src/lib.rs", "tests::fails");
        assert_eq!(fails.status, TestStatus::Failed);
        assert!(fails.output.starts_with("some output\n"), "{:?}", fails.output);
        assert!(fails.output.contains("math is broken"));
        let failure = fails.failure.as_ref().unwrap();
        assert_eq!((failure.path.as_path(), failure.line, failure.column), (std::path::Path::new("src/lib.rs"), 25, 8));
        assert!(fails.message.as_deref().unwrap().starts_with("assertion `left == right` failed: math is broken"));

        let slow = test(&suites, "demo: src/lib.rs", "tests::slow");
        assert_eq!(slow.status, TestStatus::Ignored);
        assert_eq!(slow.message.as_deref(), Some("slow"));
        assert_eq!(test(&suites, "demo: src/lib.rs", "util::tests::panics").status, TestStatus::Passed);
        // Passing tests' output is only printed with `--show-output`.
        assert_eq!(test(&suites, "demo: src/lib.rs", "tests::nested::deep").output, "");

        let it = test(&suites, "tests/integration.rs", "it_fails");
        assert_eq!(it.status, TestStatus::Failed);
        assert_eq!(it.failure.as_ref().unwrap().path, PathBuf::from("tests/integration.rs"));
        assert_eq!(it.message.as_deref(), Some("integration failure"));

        let doc = test(&suites, "Doc-tests demo", "src/lib.rs - sub (line 13)");
        assert_eq!(doc.status, TestStatus::Failed);
        assert!(doc.output.contains("Test executable failed"));
        assert_eq!(test(&suites, "Doc-tests demo", "src/lib.rs - add (line 6)").status, TestStatus::Passed);
    }

    #[test]
    fn cargo_show_output_successes() {
        let text = "running 2 tests\ntest a::ok ... ok\ntest a::bad ... FAILED\n\nsuccesses:\n\n---- a::ok stdout ----\nhello\n\n\nsuccesses:\n    a::ok\n\nfailures:\n\n---- a::bad stderr ----\nboom\n\nfailures:\n    a::bad\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.50s\n";
        let suites = parse(Framework::Cargo, text);
        assert_eq!(test(&suites, "Tests", "a::ok").output, "hello\n");
        assert_eq!(test(&suites, "Tests", "a::bad").output, "boom\n");
        assert_eq!(suites[0].duration_ms, Some(1500));
    }

    #[test]
    fn cargo_streaming_running_and_terminated() {
        let mut p = Parser::default();
        let mut suites = Vec::new();
        for line in ["running 2 tests", "test a ... ok <0.250s>", "test b has been running for over 60 seconds"] {
            p.feed(Framework::Cargo, &mut suites, line);
        }
        assert_eq!(suites.len(), 1);
        assert_eq!(test(&suites, "Tests", "a").duration_ms, Some(250));
        assert_eq!(test(&suites, "Tests", "b").status, TestStatus::Running);
        assert!(!suites[0].finished);
        p.finish(&mut suites);
        let b = test(&suites, "Tests", "b");
        assert_eq!((b.status, b.message.as_deref()), (TestStatus::Failed, Some("Terminated")));
    }

    #[test]
    fn old_panic_format() {
        let (loc, msg) = failure_point("thread 'x' panicked at 'boom, really', src/a.rs:3:5\nnote: x").unwrap();
        assert_eq!((loc.path, loc.line, loc.column), (PathBuf::from("src/a.rs"), 2, 4));
        assert_eq!(msg.as_deref(), Some("boom, really"));
        assert!(failure_point("nothing here").is_none());
        assert!(failure_point("panicked at nowhere").is_none());
    }

    #[test]
    fn pytest_verbose_and_failures() {
        let suites = parse(Framework::Pytest, PYTEST);
        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].source.as_deref(), Some(std::path::Path::new("tests/test_math.py")));
        let s = &suites[0].tests;
        let st: Vec<(&str, TestStatus)> = s.iter().map(|t| (t.name.as_str(), t.status)).collect();
        assert_eq!(
            st,
            [
                ("test_add", TestStatus::Passed),
                ("test_fail", TestStatus::Failed),
                ("test_skip", TestStatus::Ignored),
                ("TestGroup::test_param[1]", TestStatus::Passed),
                ("TestGroup::test_param[2]", TestStatus::Failed),
            ]
        );
        let skip = test(&suites, "tests/test_math.py", "test_skip");
        assert_eq!(skip.message.as_deref(), Some("later"));
        let fail = test(&suites, "tests/test_math.py", "test_fail");
        assert!(fail.output.contains("assert 1 + 1 == 3"), "{:?}", fail.output);
        let loc = fail.failure.as_ref().unwrap();
        assert_eq!((loc.path.as_path(), loc.line), (std::path::Path::new("tests/test_math.py"), 7));
        assert_eq!(fail.message.as_deref(), Some("AssertionError: bad math\nassert (1 + 1) == 3"));
        let param = test(&suites, "tests/test_math.py", "TestGroup::test_param[2]");
        assert_eq!(param.failure.as_ref().map(|l| l.line), Some(16));
    }

    #[test]
    fn pytest_summary_lines() {
        let suites = parse(
            Framework::Pytest,
            "PASSED tests/a.py::test_x\nFAILED tests/a.py::test_y - assert 1 == 2\nSKIPPED [1] tests/a.py:3: why\n",
        );
        assert_eq!(test(&suites, "tests/a.py", "test_x").status, TestStatus::Passed);
        let y = test(&suites, "tests/a.py", "test_y");
        assert_eq!((y.status, y.message.as_deref()), (TestStatus::Failed, Some("assert 1 == 2")));
        assert_eq!(suites[0].tests.len(), 2);
    }

    #[test]
    fn ctest_results() {
        let suites = parse(Framework::CTest, CTEST);
        assert_eq!(suites.len(), 1);
        let st: Vec<(&str, TestStatus, Option<u64>)> =
            suites[0].tests.iter().map(|t| (t.name.as_str(), t.status, t.duration_ms)).collect();
        assert_eq!(
            st,
            [
                ("test_add", TestStatus::Passed, Some(0)),
                ("test_fail", TestStatus::Failed, Some(10)),
                ("test_skip", TestStatus::Ignored, Some(0)),
                ("test_crash", TestStatus::Failed, Some(20)),
            ]
        );
        let fail = test(&suites, "CTest", "test_fail");
        assert_eq!(fail.output, "expected 4, got 3\n");
        assert_eq!(test(&suites, "CTest", "test_crash").message.as_deref(), Some("Exception"));
    }

    #[test]
    fn zig_results() {
        let suites = parse(Framework::Zig, ZIG);
        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].name, "a");
        let st: Vec<(&str, TestStatus)> = suites[0].tests.iter().map(|t| (t.name.as_str(), t.status)).collect();
        assert_eq!(st, [("adds", TestStatus::Passed), ("fails", TestStatus::Failed), ("skipped", TestStatus::Ignored)]);
        let fails = test(&suites, "a", "fails");
        assert_eq!(fails.message.as_deref(), Some("TestUnexpectedResult"));
        // The first stack line is in std; the location is the first `in test.` frame.
        let loc = fails.failure.as_ref().unwrap();
        assert_eq!((loc.path.as_path(), loc.line), (std::path::Path::new("/p/a.zig"), 2));
    }

    #[test]
    fn hostile_lines_do_not_panic() {
        for fw in [Framework::Cargo, Framework::Pytest, Framework::CTest, Framework::Zig] {
            let mut p = Parser::default();
            let mut suites = Vec::new();
            for line in [
                "",
                "---- ----",
                "---- x stdout ----",
                "test result: ok.",
                "     Running ",
                "     Running ( )",
                "   Doc-tests ",
                "test  ... ok <s>",
                "test x ... ok <99999999999999999999s>",
                "=",
                "________",
                "::",
                ":: PASSED",
                "1/1 Test #1: x ...***",
                "1/1 ...OK",
                "é::ü PASSED",
                "thread 'x' panicked at :::",
                "\u{1b}[31m",
            ] {
                p.feed(fw, &mut suites, line);
            }
            p.finish(&mut suites);
        }
        assert_eq!(seconds_ms("NaN"), None);
        assert_eq!(seconds_ms("99999999999999999999"), None);
        assert_eq!(seconds_ms("0.01 sec"), Some(10));
    }

    #[test]
    fn output_is_capped() {
        let mut out = String::new();
        let line = "x".repeat(1000);
        for _ in 0..1000 {
            push_output(&mut out, &line);
        }
        assert!(out.len() < MAX_TEST_OUTPUT + 2000);
        assert!(out.ends_with("(output truncated)"));
    }
}
