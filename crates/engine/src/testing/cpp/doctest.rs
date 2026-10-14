//! doctest: `--list-test-cases` (console or `--reporters=xml`) and the XML reporter
//! (`--reporters=xml`).

use std::path::PathBuf;

use crate::cpp::xml::{self, Element};

use super::super::parse::{MAX_TESTS, push_output};
use super::super::{SuiteKind, TestStatus, TestSuite};
use super::{ListedTest, case, location, secs_ms};

/// Escape a name for a doctest filter (commas separate filters).
pub fn escape(name: &str) -> String {
    name.replace('\\', "\\\\").replace(',', "\\,")
}

/// `--list-test-cases`: the XML listing, or the console one between the `listing all test case
/// names` banner and the closing `====` line.
pub fn parse_list(text: &str) -> Vec<ListedTest> {
    if text.contains("<doctest") {
        let doc = xml::parse(text);
        let mut out = Vec::new();
        let mut cases = Vec::new();
        collect(&doc, None, &mut cases);
        for (suite, tc) in cases.into_iter().take(MAX_TESTS) {
            let Some(name) = tc.attr("name") else { continue };
            out.push(ListedTest {
                suite: suite.or_else(|| tc.attr("testsuite").filter(|s| !s.is_empty()).map(str::to_string)),
                name: name.to_string(),
                file: tc.attr("filename").filter(|f| !f.is_empty()).map(PathBuf::from),
                line: tc.attr_u64("line").and_then(|l| usize::try_from(l).ok()).map(|l| l.saturating_sub(1)),
            });
        }
        return out;
    }
    let mut out = Vec::new();
    let mut state = 0u8; // 0: before the banner, 1: banner seen, 2: listing
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let rule = !line.is_empty() && line.chars().all(|c| c == '=');
        match state {
            0 if line.contains("listing all test case names") => state = 1,
            1 if rule => state = 2,
            2 if rule => break,
            2 if !line.trim().is_empty() && out.len() < MAX_TESTS => {
                out.push(ListedTest { suite: None, name: line.to_string(), file: None, line: None })
            }
            _ => {}
        }
    }
    out
}

fn collect<'a>(e: &'a Element, suite: Option<String>, out: &mut Vec<(Option<String>, &'a Element)>) {
    for c in &e.children {
        match c.name.as_str() {
            "TestCase" => out.push((suite.clone(), c)),
            "TestSuite" => {
                let s = c.attr("name").filter(|n| !n.is_empty()).map(str::to_string);
                collect(c, s.or_else(|| suite.clone()), out);
            }
            _ => collect(c, suite.clone(), out),
        }
    }
}

fn failures_in(e: &Element, subcases: &mut Vec<String>, out: &mut Vec<(String, Option<(String, u64)>)>) {
    for c in &e.children {
        let loc = c.attr("filename").map(|f| (f.to_string(), c.attr_u64("line").unwrap_or(1)));
        let text = match c.name.as_str() {
            "SubCase" => {
                subcases.push(c.attr("name").unwrap_or("").to_string());
                failures_in(c, subcases, out);
                subcases.pop();
                continue;
            }
            "Expression" if c.attr("success") == Some("false") => {
                let kind = c.attr("type").unwrap_or("CHECK");
                let original = c.child_text("Original").unwrap_or("");
                let mut text = format!("{kind}( {original} ) is NOT correct!");
                if let Some(expanded) = c.child_text("Expanded").filter(|x| !x.is_empty()) {
                    text.push_str(&format!("\n  values: {kind}( {expanded} )"));
                }
                if let Some(ex) = c.child_text("Exception").filter(|x| !x.is_empty()) {
                    text.push_str(&format!("\n  threw exception: {ex}"));
                }
                text
            }
            "Exception" => format!("TEST CASE FAILED: threw exception: {}", c.text.trim()),
            "Message" if matches!(c.attr("type"), Some("FAIL" | "FAIL_CHECK" | "ERROR")) => {
                let text = c.child_text("Text").unwrap_or(c.text.trim());
                format!("{}: {text}", c.attr("type").unwrap_or("FAIL"))
            }
            _ => continue,
        };
        let text = if subcases.is_empty() { text } else { format!("{}\n{text}", subcases.join(" / ")) };
        out.push((text, loc));
    }
}

/// The XML reporter's document.
pub fn parse_xml(text: &str, binary: &str) -> Vec<TestSuite> {
    let doc = xml::parse(text);
    let mut cases = Vec::new();
    collect(&doc, None, &mut cases);
    let mut suites: Vec<TestSuite> = Vec::new();
    for (suite_name, tc) in cases.into_iter().take(MAX_TESTS) {
        let Some(tname) = tc.attr("name") else { continue };
        let results = tc.child("OverallResultsAsserts");
        let mut failures = Vec::new();
        failures_in(tc, &mut Vec::new(), &mut failures);
        let failed = results.and_then(|r| r.attr("test_case_success")) == Some("false")
            || results.and_then(|r| r.attr_u64("failures")).is_some_and(|n| n > 0)
            || !failures.is_empty();
        let status = if tc.attr("skipped") == Some("true") {
            TestStatus::Ignored
        } else if failed {
            TestStatus::Failed
        } else {
            TestStatus::Passed
        };
        let mut c = case(tname, status);
        c.location = location(tc.attr("filename"), tc.attr_u64("line"));
        c.duration_ms = results.and_then(|r| r.attr("duration")).and_then(secs_ms);
        for (text, _) in &failures {
            push_output(&mut c.output, text);
        }
        if let Some((text, loc)) = failures.first().filter(|_| status == TestStatus::Failed) {
            c.message = Some(text.clone());
            c.failure = loc.as_ref().and_then(|(f, l)| location(Some(f), Some(*l)));
        }
        let sname = suite_name.unwrap_or_else(|| binary.to_string());
        let si = match suites.iter().position(|s| s.name == sname) {
            Some(i) => i,
            None => {
                let mut s = TestSuite::new(sname, SuiteKind::Other, None, None);
                s.finished = true;
                suites.push(s);
                suites.len() - 1
            }
        };
        if let Some(s) = suites.get_mut(si) {
            s.tests.push(c);
        }
    }
    suites
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../fixtures/cpp/doctest_list.txt");
    const XML: &str = include_str!("../fixtures/cpp/doctest.xml");

    #[test]
    fn listings() {
        let l = parse_list(LIST);
        assert_eq!(l.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["adds", "fails", "in suite"]);
        let x = parse_list(XML);
        assert_eq!(x.len(), 4);
        assert_eq!(x[2].suite.as_deref(), Some("math"));
        assert_eq!(x[0].line, Some(4));
    }

    #[test]
    fn report() {
        let suites = parse_xml(XML, "doctests");
        let names: Vec<&str> = suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["doctests", "math"]);
        let d = &suites[0].tests;
        assert_eq!(d.iter().map(|t| t.status).collect::<Vec<_>>(), vec![TestStatus::Passed, TestStatus::Failed]);
        let fails = &d[1];
        assert_eq!(fails.failure.as_ref().map(|f| f.line), Some(10));
        assert!(fails.message.as_deref().unwrap().contains("CHECK( add(1, 2) == 4 ) is NOT correct!"));
        assert!(fails.message.as_deref().unwrap().contains("values: CHECK( 3 == 4 )"));
        assert_eq!(fails.duration_ms, Some(1));
        let m = &suites[1].tests;
        assert_eq!(m.iter().map(|t| t.status).collect::<Vec<_>>(), vec![TestStatus::Failed, TestStatus::Ignored]);
        assert!(m[0].message.as_deref().unwrap().starts_with("one\n"));
        assert!(m[0].output.contains("threw exception"));
    }

    #[test]
    fn hostile_input() {
        assert!(parse_xml("<doctest><TestSuite><TestCase/></TestSuite>", "b").is_empty());
        let _ = parse_xml(&XML[..XML.len() / 3], "b");
        assert!(parse_list("==\n").is_empty());
        assert_eq!(escape("a,b\\"), "a\\,b\\\\");
    }
}
