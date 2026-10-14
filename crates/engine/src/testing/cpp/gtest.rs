//! GoogleTest: `--gtest_list_tests`, the console output (parsed live while the run goes) and the
//! `--gtest_output=json:` report.

use std::path::PathBuf;

use serde_json::Value;

use super::super::parse::{MAX_TESTS, push_output, test_index};
use super::super::{SuiteKind, TestLocation, TestStatus, TestSuite};
use super::{ListedTest, case, location, secs_ms};

/// `--gtest_list_tests`:
///
/// ```text
/// MathTest.
///   Adds
/// Inst/Param.  # TypeParam = int
///   Each/0  # GetParam() = 1
/// ```
pub fn parse_list(text: &str) -> Vec<ListedTest> {
    let mut out = Vec::new();
    let mut suite: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let body = line.split("  #").next().unwrap_or(line).trim_end();
        if body.is_empty() {
            continue;
        }
        if !body.starts_with(' ') {
            suite = body.strip_suffix('.').map(str::to_string);
            continue;
        }
        if let Some(s) = &suite {
            let name = body.trim();
            if !name.is_empty() && !name.contains(' ') && out.len() < MAX_TESTS {
                out.push(ListedTest { suite: Some(s.clone()), name: name.to_string(), file: None, line: None });
            }
        }
    }
    out
}

/// Live console state: the test receiving output lines.
#[derive(Clone, Debug, Default)]
pub struct Console {
    current: Option<(usize, usize)>,
}

/// `[ RUN      ] Suite.Name` → `(tag, rest)`.
fn tagged(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (tag, after) = rest.split_once(']')?;
    if tag.len() != 10 {
        return None;
    }
    Some((tag.trim(), after.trim()))
}

/// `Suite.Name (12 ms)` / `Inst/Suite.Name/0, where GetParam() = 1 (0 ms)` → (suite, name, ms).
fn result_name(rest: &str) -> Option<(&str, &str, Option<u64>)> {
    let ms = rest
        .rsplit_once(" (")
        .and_then(|(_, t)| t.strip_suffix(" ms)"))
        .and_then(|n| n.trim().parse::<u64>().ok());
    let full = rest.split([' ', ',']).next()?;
    let (suite, name) = full.split_once('.')?;
    (!suite.is_empty() && !name.is_empty()).then_some((suite, name, ms))
}

/// `path:12: Failure`, `path(12): error: …` → location.
fn failure_location(line: &str) -> Option<TestLocation> {
    let t = line.trim_end();
    for marker in [": Failure", ": error: ", ": Skipped"] {
        let Some((head, _)) = t.split_once(marker) else { continue };
        if let Some((file, ln)) = head.rsplit_once(':')
            && let Ok(n) = ln.parse::<usize>()
            && !file.is_empty()
        {
            return Some(TestLocation { path: PathBuf::from(file), line: n.saturating_sub(1), column: 0 });
        }
        if let Some((file, ln)) = head.strip_suffix(')').and_then(|h| h.rsplit_once('('))
            && let Ok(n) = ln.parse::<usize>()
        {
            return Some(TestLocation { path: PathBuf::from(file), line: n.saturating_sub(1), column: 0 });
        }
    }
    None
}

fn suite_index(suites: &mut Vec<TestSuite>, name: &str) -> usize {
    match suites.iter().position(|s| s.name == name) {
        Some(i) => i,
        None => {
            suites.push(TestSuite::new(name, SuiteKind::Other, None, None));
            suites.len() - 1
        }
    }
}

/// The failure message from a test's output: the lines after its first failure location.
fn message_of(output: &str) -> Option<String> {
    let mut lines = output.lines().skip_while(|l| failure_location(l).is_none());
    lines.next()?;
    let msg: Vec<&str> = lines.take_while(|l| failure_location(l).is_none()).take(20).collect();
    let msg = msg.join("\n").trim().to_string();
    (!msg.is_empty()).then_some(msg)
}

/// Feed one console line.
pub fn feed(state: &mut Console, suites: &mut Vec<TestSuite>, line: &str) {
    let Some((tag, rest)) = tagged(line) else {
        if let Some((si, ti)) = state.current
            && let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti))
        {
            push_output(&mut t.output, line);
            if t.failure.is_none() {
                t.failure = failure_location(line);
            }
        }
        return;
    };
    match tag {
        "RUN" => {
            state.current = None;
            let Some((suite, name, _)) = result_name(rest) else { return };
            let si = suite_index(suites, suite);
            state.current = test_index(suites, si, name, true).map(|ti| (si, ti));
        }
        "OK" | "FAILED" | "SKIPPED" => {
            let Some((suite, name, ms)) = result_name(rest) else { return };
            // The summary repeats failed tests without a time: only update known ones.
            let known = suites.iter().position(|s| s.name == suite);
            let si = match (known, ms) {
                (Some(i), _) => i,
                (None, Some(_)) => suite_index(suites, suite),
                (None, None) => return,
            };
            let Some(ti) = test_index(suites, si, name, ms.is_some()) else { return };
            if let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti)) {
                if ms.is_none() && t.status != TestStatus::Running {
                    return;
                }
                t.status = match tag {
                    "OK" => TestStatus::Passed,
                    "FAILED" => TestStatus::Failed,
                    _ => TestStatus::Ignored,
                };
                if ms.is_some() {
                    t.duration_ms = ms;
                }
                if t.message.is_none() && t.status != TestStatus::Passed {
                    t.message = message_of(&t.output);
                }
            }
            state.current = None;
        }
        "----------" => {
            state.current = None;
            // `2 tests from MathTest (3 ms total)`
            if let Some(body) = rest.strip_suffix(" ms total)")
                && let Some((head, ms)) = body.rsplit_once(" (")
                && let Some((_, suite)) = head.split_once(" from ")
                && let Some(s) = suites.iter_mut().find(|s| s.name == suite.trim())
            {
                s.finished = true;
                s.duration_ms = ms.trim().parse().ok();
            }
        }
        _ => state.current = None,
    }
}

/// The `--gtest_output=json:` report.
pub fn parse_json(text: &str) -> Vec<TestSuite> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let mut out = Vec::new();
    let mut total = 0usize;
    for s in v.get("testsuites").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = s.get("name").and_then(Value::as_str) else { continue };
        let mut suite = TestSuite::new(name, SuiteKind::Other, None, None);
        suite.finished = true;
        suite.duration_ms = s.get("time").and_then(Value::as_str).and_then(secs_ms);
        for t in s.get("testsuite").and_then(Value::as_array).into_iter().flatten() {
            let Some(tname) = t.get("name").and_then(Value::as_str) else { continue };
            if total >= MAX_TESTS {
                break;
            }
            total += 1;
            let failures: Vec<&str> = t
                .get("failures")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|f| f.get("failure").and_then(Value::as_str))
                .collect();
            let status = t.get("status").and_then(Value::as_str).unwrap_or("RUN");
            let result = t.get("result").and_then(Value::as_str).unwrap_or("COMPLETED");
            let status = if !failures.is_empty() {
                TestStatus::Failed
            } else if status == "NOTRUN" || matches!(result, "SKIPPED" | "SUPPRESSED") {
                TestStatus::Ignored
            } else {
                TestStatus::Passed
            };
            let mut c = case(tname, status);
            c.duration_ms = t.get("time").and_then(Value::as_str).and_then(secs_ms);
            c.location = location(t.get("file").and_then(Value::as_str), t.get("line").and_then(Value::as_u64));
            for f in &failures {
                push_output(&mut c.output, f);
            }
            if let Some(first) = failures.first() {
                let (head, rest) = first.split_once('\n').unwrap_or((first, ""));
                if let Some((file, ln)) = head.trim().rsplit_once(':')
                    && let Ok(n) = ln.trim().parse::<usize>()
                {
                    c.failure = Some(TestLocation { path: PathBuf::from(file), line: n.saturating_sub(1), column: 0 });
                    let msg = rest.trim();
                    c.message = (!msg.is_empty()).then(|| msg.to_string());
                } else {
                    c.message = Some(first.trim().to_string());
                }
            }
            if status == TestStatus::Ignored && result == "SUPPRESSED" {
                c.message = Some("Disabled".into());
            }
            suite.tests.push(c);
        }
        out.push(suite);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../fixtures/cpp/gtest_list.txt");
    const CONSOLE: &str = include_str!("../fixtures/cpp/gtest_console.txt");
    const JSON: &str = include_str!("../fixtures/cpp/gtest.json");

    #[test]
    fn list() {
        let l = parse_list(LIST);
        let names: Vec<String> = l.iter().map(|t| format!("{}.{}", t.suite.as_deref().unwrap_or(""), t.name)).collect();
        assert_eq!(
            names,
            vec!["MathTest.Adds", "MathTest.Fails", "MathTest.DISABLED_Slow", "Inst/ParamTest.Even/0", "Inst/ParamTest.Even/1"]
        );
    }

    #[test]
    fn console_builds_the_tree_live() {
        let mut state = Console::default();
        let mut suites = Vec::new();
        for line in CONSOLE.lines() {
            feed(&mut state, &mut suites, line);
        }
        let names: Vec<&str> = suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["MathTest", "Inst/ParamTest", "SkipTest"]);
        let math = &suites[0];
        assert!(math.finished);
        assert_eq!(math.duration_ms, Some(3));
        let adds = &math.tests[0];
        assert_eq!((adds.name.as_str(), adds.status, adds.duration_ms), ("Adds", TestStatus::Passed, Some(0)));
        let fails = &math.tests[1];
        assert_eq!(fails.status, TestStatus::Failed);
        assert_eq!(fails.duration_ms, Some(1));
        let f = fails.failure.as_ref().unwrap();
        assert_eq!((f.path.to_str(), f.line), (Some("/proj/tests/math_test.cpp"), 11));
        assert!(fails.message.as_deref().unwrap().contains("Which is: 3"), "{:?}", fails.message);
        assert!(fails.output.contains("Expected equality"));
        let param = &suites[1].tests;
        assert_eq!(param.iter().map(|t| t.status).collect::<Vec<_>>(), vec![TestStatus::Passed, TestStatus::Failed]);
        assert_eq!(suites[2].tests[0].status, TestStatus::Ignored);
        // The summary's `[  FAILED  ] MathTest.Fails` lines add nothing.
        assert_eq!(suites.iter().map(|s| s.tests.len()).sum::<usize>(), 5);
    }

    #[test]
    fn json_report() {
        let suites = parse_json(JSON);
        assert_eq!(suites.len(), 3);
        let math = &suites[0];
        assert_eq!(math.tests.len(), 3);
        let adds = &math.tests[0];
        assert_eq!(adds.status, TestStatus::Passed);
        assert_eq!(adds.location.as_ref().map(|l| l.line), Some(4));
        let fails = &math.tests[1];
        assert_eq!(fails.status, TestStatus::Failed);
        assert_eq!(fails.duration_ms, Some(1));
        assert_eq!(fails.failure.as_ref().map(|f| (f.path.clone(), f.line)), Some((PathBuf::from("/proj/tests/math_test.cpp"), 11)));
        assert!(fails.message.as_deref().unwrap().starts_with("Expected equality"));
        assert_eq!(math.tests[2].status, TestStatus::Ignored);
        assert_eq!(math.tests[2].message.as_deref(), Some("Disabled"));
        assert_eq!(suites[2].tests[0].status, TestStatus::Ignored);
    }

    #[test]
    fn hostile_input() {
        assert!(parse_json("{").is_empty());
        assert!(parse_json("{\"testsuites\": [{\"name\": 1}, {\"name\": \"x\", \"testsuite\": [{}, {\"name\": \"t\", \"failures\": [{\"failure\": \"no location\"}]}]}]}")[0].tests[0].message.is_some());
        let mut state = Console::default();
        let mut suites = Vec::new();
        for line in ["[ RUN      ]", "[ RUN      ] .", "[       OK ] a.b (x ms)", "[  FAILED  ] 1 test, listed below:", "[]", "[ RUN      ] é.ü", ":1: Failure", "(1): error: x"] {
            feed(&mut state, &mut suites, line);
        }
        assert_eq!(suites.len(), 1);
        let _ = parse_list("\u{0}\n  x\n.\n  \n");
    }
}
