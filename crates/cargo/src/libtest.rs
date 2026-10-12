//! libtest's human ("pretty") output: suite start, per-test results and the summary line.

use std::borrow::Cow;

use crate::{TestEvent, TestOutcome};

/// Suffixes libtest appends to a test's name in its result line (`test x - should panic ... ok`).
const TEST_MODE_SUFFIXES: [&str; 3] = [" - should panic", " - compile fail", " - compile"];

pub(crate) fn parse_test_line(line: &str) -> Option<TestEvent> {
    // Plain unless the user passed `--color=always` to the harness; parse either way.
    let plain = strip_ansi(line);
    let line = plain.trim_end();
    if let Some(rest) = line.strip_prefix("test result: ") {
        parse_summary(rest)
    } else if let Some(rest) = line.strip_prefix("running ") {
        parse_suite_start(rest)
    } else if let Some(rest) = line.strip_prefix("test ") {
        parse_result(rest)
    } else {
        None
    }
}

/// `3 tests` / `1 test`
fn parse_suite_start(rest: &str) -> Option<TestEvent> {
    let (count, noun) = rest.split_once(' ')?;
    if !matches!(noun, "test" | "tests") {
        return None;
    }
    Some(TestEvent::SuiteStarted { count: count.parse().ok()? })
}

/// `path::name ... ok`, `… FAILED`, `… ignored`, `… ignored, reason`, `… bench: 12 ns/iter (+/- 1)`
fn parse_result(rest: &str) -> Option<TestEvent> {
    let (name, status) = rest.split_once(" ... ")?;
    // Benchmarks pad names to a common width.
    let name = name.trim_end();
    let name = TEST_MODE_SUFFIXES.iter().find_map(|suffix| name.strip_suffix(suffix)).unwrap_or(name).trim_end();
    if name.is_empty() {
        return None;
    }
    let word = status.trim_start().split(|c: char| c.is_whitespace() || c == ',').next().unwrap_or_default();
    let outcome = match word {
        "ok" | "bench:" => TestOutcome::Ok,
        "FAILED" => TestOutcome::Failed,
        "ignored" => TestOutcome::Ignored,
        _ => return None,
    };
    Some(TestEvent::Result { name: name.to_owned(), outcome })
}

/// `ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s`
fn parse_summary(rest: &str) -> Option<TestEvent> {
    let (status, counts) = rest.split_once(". ")?;
    if !matches!(status, "ok" | "FAILED") {
        return None;
    }
    let (mut passed, mut failed, mut ignored, mut measured, mut filtered_out) = (0, 0, 0, 0, 0);
    let mut found = false;
    for part in counts.split(';') {
        let Some((count, label)) = part.trim().split_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<usize>() else {
            continue;
        };
        let slot = match label.trim() {
            "passed" => &mut passed,
            "failed" => &mut failed,
            "ignored" => &mut ignored,
            "measured" => &mut measured,
            "filtered out" => &mut filtered_out,
            _ => continue,
        };
        *slot = count;
        found = true;
    }
    found.then_some(TestEvent::SuiteFinished { passed, failed, ignored, measured, filtered_out })
}

/// Remove ANSI escape sequences (CSI `ESC [ … final`, OSC `ESC ] … BEL|ESC \`, two-byte `ESC x`).
pub(crate) fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\u{1b}') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        let _ = chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TestOutcome::*;

    fn result(name: &str, outcome: TestOutcome) -> Option<TestEvent> {
        Some(TestEvent::Result { name: name.into(), outcome })
    }

    #[test]
    fn suite_start() {
        assert_eq!(parse_test_line("running 3 tests"), Some(TestEvent::SuiteStarted { count: 3 }));
        assert_eq!(parse_test_line("running 1 test"), Some(TestEvent::SuiteStarted { count: 1 }));
        assert_eq!(parse_test_line("running 0 tests\r"), Some(TestEvent::SuiteStarted { count: 0 }));
        assert_eq!(parse_test_line("running tests"), None);
        assert_eq!(parse_test_line("running 3 benchmarks"), None);
        assert_eq!(parse_test_line("running -1 tests"), None);
        assert_eq!(parse_test_line("running 99999999999999999999999 tests"), None);
        assert_eq!(parse_test_line("  running 3 tests"), None);
    }

    #[test]
    fn results() {
        assert_eq!(parse_test_line("test tests::adds ... ok"), result("tests::adds", Ok));
        assert_eq!(parse_test_line("test tests::fails ... FAILED"), result("tests::fails", Failed));
        assert_eq!(parse_test_line("test tests::slow ... ignored"), result("tests::slow", Ignored));
        assert_eq!(
            parse_test_line("test tests::net ... ignored, requires network access"),
            result("tests::net", Ignored)
        );
        assert_eq!(parse_test_line("test tests::timed ... ok <0.002s>"), result("tests::timed", Ok));
        assert_eq!(
            parse_test_line("test tests::timeout ... FAILED (time limit exceeded)"),
            result("tests::timeout", Failed)
        );
        assert_eq!(parse_test_line("test it_works ... ok\r"), result("it_works", Ok));
    }

    #[test]
    fn benches_and_test_modes() {
        assert_eq!(
            parse_test_line("test bench_sort     ... bench:       1,234 ns/iter (+/- 56)"),
            result("bench_sort", Ok)
        );
        assert_eq!(parse_test_line("test tests::boom - should panic ... ok"), result("tests::boom", Ok));
        assert_eq!(
            parse_test_line("test src/lib.rs - add (line 5) - compile fail ... ok"),
            result("src/lib.rs - add (line 5)", Ok)
        );
        assert_eq!(
            parse_test_line("test src/lib.rs - Thing::new (line 12) ... FAILED"),
            result("src/lib.rs - Thing::new (line 12)", Failed)
        );
        assert_eq!(
            parse_test_line("test src/lib.rs - net (line 3) - compile ... ok"),
            result("src/lib.rs - net (line 3)", Ok)
        );
    }

    #[test]
    fn summaries() {
        assert_eq!(
            parse_test_line(
                "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 2 filtered out; finished in 0.00s"
            ),
            Some(TestEvent::SuiteFinished { passed: 3, failed: 0, ignored: 1, measured: 0, filtered_out: 2 })
        );
        assert_eq!(
            parse_test_line(
                "test result: FAILED. 1 passed; 1 failed; 0 ignored; 4 measured; 0 filtered out; finished in 1.25s"
            ),
            Some(TestEvent::SuiteFinished { passed: 1, failed: 1, ignored: 0, measured: 4, filtered_out: 0 })
        );
        // Older libtest without the timing.
        assert_eq!(
            parse_test_line("test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"),
            Some(TestEvent::SuiteFinished { passed: 2, failed: 0, ignored: 0, measured: 0, filtered_out: 0 })
        );
        assert_eq!(parse_test_line("test result: maybe. 1 passed"), None);
        assert_eq!(parse_test_line("test result: ok. nothing here"), None);
        assert_eq!(parse_test_line("test result: ok."), None);
    }

    #[test]
    fn not_test_lines() {
        for line in [
            "",
            "test",
            "test ",
            "test  ... ok",
            "test foo",
            "test foo ... maybe",
            "test foo ... ",
            "test foo has been running for over 60 seconds",
            "failures:",
            "---- tests::fails stdout ----",
            "   Compiling app v0.1.0",
            "     Running unittests src/lib.rs (target/debug/deps/app-1234)",
            "   Doc-tests app",
            "testing ... ok",
        ] {
            assert_eq!(parse_test_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn coloured_output() {
        assert_eq!(parse_test_line("test a::b ... \u{1b}[32mok\u{1b}[0m"), result("a::b", Ok));
        assert_eq!(parse_test_line("test a::c ... \u{1b}[31mFAILED\u{1b}[0m"), result("a::c", Failed));
        assert_eq!(
            parse_test_line(
                "test result: \u{1b}[31mFAILED\u{1b}[0m. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
            ),
            Some(TestEvent::SuiteFinished { passed: 0, failed: 1, ignored: 0, measured: 0, filtered_out: 0 })
        );
    }

    #[test]
    fn ansi_stripping() {
        assert!(matches!(strip_ansi("plain"), Cow::Borrowed("plain")));
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m text"), "red text");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{1b}\\ end"), "link end");
        assert_eq!(strip_ansi("a\u{1b}=b"), "ab");
        // Truncated sequences don't panic.
        assert_eq!(strip_ansi("x\u{1b}"), "x");
        assert_eq!(strip_ansi("x\u{1b}[31"), "x");
        assert_eq!(strip_ansi("x\u{1b}]unterminated"), "x");
        assert_eq!(strip_ansi("é\u{1b}[0mü"), "éü");
    }
}
