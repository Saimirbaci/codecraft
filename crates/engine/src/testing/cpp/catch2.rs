//! Catch2: `--list-tests` (console or `--reporter xml`) and the XML reporter (`--reporter xml`;
//! v3's `Catch2TestRun` and v2's `Catch`/`Group` documents).

use std::path::PathBuf;

use crate::cpp::xml::{self, Element};

use super::super::parse::{MAX_TESTS, push_output};
use super::super::{SuiteKind, TestStatus, TestSuite};
use super::{ListedTest, case, location, secs_ms};

/// Escape a test name for a Catch2 test spec (`,` `[` `]` `\` are special).
pub fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '\\' | ',' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `--list-tests`: the XML listing (`<MatchingTests>`), or the console one:
///
/// ```text
/// All available test cases:
///   Adds numbers
///       [math]
///   A long name that
///     wraps
/// 2 test cases
/// ```
pub fn parse_list(text: &str) -> Vec<ListedTest> {
    if text.contains("<MatchingTests") {
        let doc = xml::parse(text);
        let Some(m) = doc.find("MatchingTests") else { return Vec::new() };
        return m
            .children_named("TestCase")
            .take(MAX_TESTS)
            .filter_map(|t| {
                let name = t.child_text("Name")?.to_string();
                let info = t.child("SourceInfo");
                let file = info.and_then(|i| i.child_text("File")).filter(|f| !f.is_empty()).map(PathBuf::from);
                let line = info
                    .and_then(|i| i.child_text("Line"))
                    .and_then(|l| l.parse::<usize>().ok())
                    .map(|l| l.saturating_sub(1));
                Some(ListedTest { suite: None, name, file, line })
            })
            .collect();
    }
    let mut out: Vec<ListedTest> = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.ends_with("test cases:") || line.ends_with("test cases matching") {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        if body.is_empty() {
            continue;
        }
        if indent == 0 {
            in_list = false;
            continue;
        }
        if body.starts_with('[') && indent >= 6 {
            continue;
        }
        if indent == 2 {
            if out.len() < MAX_TESTS {
                out.push(ListedTest { suite: None, name: body.to_string(), file: None, line: None });
            }
        } else if let Some(last) = out.last_mut() {
            last.name.push(' ');
            last.name.push_str(body);
        }
    }
    out
}

/// What a failed assertion (or message) says, and where.
fn failures_in(e: &Element, sections: &mut Vec<String>, out: &mut Vec<(String, Option<(String, u64)>)>) {
    for c in &e.children {
        let loc = c.attr("filename").map(|f| (f.to_string(), c.attr_u64("line").unwrap_or(1)));
        match c.name.as_str() {
            "Section" => {
                sections.push(c.attr("name").unwrap_or("").to_string());
                failures_in(c, sections, out);
                sections.pop();
            }
            "Expression" if c.attr("success") == Some("false") => {
                let kind = c.attr("type").unwrap_or("CHECK");
                let original = c.child_text("Original").unwrap_or("");
                let expanded = c.child_text("Expanded").unwrap_or("");
                let mut text = format!("{kind}( {original} )");
                if !expanded.is_empty() && expanded != original {
                    text.push_str(&format!("\nwith expansion:\n  {expanded}"));
                }
                for info in c.children_named("Info") {
                    text.push_str(&format!("\nwith message:\n  {}", info.text.trim()));
                }
                if let Some(ex) = c.child("Exception") {
                    text.push_str(&format!("\ndue to unexpected exception:\n  {}", ex.text.trim()));
                }
                if !sections.is_empty() {
                    text = format!("{}\n{text}", sections.join(" / "));
                }
                out.push((text, loc));
            }
            "Exception" | "FatalErrorCondition" | "Failure" => {
                let what = match c.name.as_str() {
                    "Exception" => "unexpected exception",
                    "FatalErrorCondition" => "fatal error condition",
                    _ => "explicit failure",
                };
                out.push((format!("{what}: {}", c.text.trim()), loc));
            }
            "Expression" => failures_in(c, sections, out),
            _ => {}
        }
    }
}

/// The XML reporter's document.
pub fn parse_xml(text: &str, binary: &str) -> Vec<TestSuite> {
    let doc = xml::parse(text);
    let root = doc.find("Catch2TestRun").or_else(|| doc.find("Catch"));
    let name = root.and_then(|r| r.attr("name")).filter(|n| !n.is_empty()).unwrap_or(binary);
    let mut suite = TestSuite::new(name, SuiteKind::Other, None, None);
    suite.finished = true;
    let mut cases = Vec::new();
    collect(&doc, &mut cases);
    for tc in cases.into_iter().take(MAX_TESTS) {
        let Some(tname) = tc.attr("name") else { continue };
        let result = tc.child("OverallResult");
        let success = result.and_then(|r| r.attr("success")) != Some("false");
        let skipped = result.and_then(|r| r.attr_u64("skips")).is_some_and(|n| n > 0) || tc.find("Skip").is_some();
        let mut failures = Vec::new();
        failures_in(tc, &mut Vec::new(), &mut failures);
        let status = if !success {
            TestStatus::Failed
        } else if skipped {
            TestStatus::Ignored
        } else {
            TestStatus::Passed
        };
        let mut c = case(tname, status);
        c.location = location(tc.attr("filename"), tc.attr_u64("line"));
        c.duration_ms = result.and_then(|r| r.attr("durationInSeconds")).and_then(secs_ms);
        for (text, _) in &failures {
            push_output(&mut c.output, text);
        }
        if let Some(r) = result {
            for stream in ["StdOut", "StdErr"] {
                if let Some(t) = r.child_text(stream).filter(|t| !t.is_empty()) {
                    push_output(&mut c.output, t);
                }
            }
        }
        if let Some((text, loc)) = failures.first().filter(|_| status == TestStatus::Failed) {
            c.message = Some(text.clone());
            c.failure = loc.as_ref().and_then(|(f, l)| location(Some(f), Some(*l)));
        }
        if status == TestStatus::Ignored {
            c.message = tc.find("Skip").map(|s| s.text.trim().to_string()).filter(|m| !m.is_empty());
        }
        suite.tests.push(c);
    }
    if suite.tests.is_empty() { Vec::new() } else { vec![suite] }
}

fn collect<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    for c in &e.children {
        if c.name == "TestCase" {
            out.push(c);
        } else {
            collect(c, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../fixtures/cpp/catch2_list.txt");
    const LIST_XML: &str = include_str!("../fixtures/cpp/catch2_list.xml");
    const XML: &str = include_str!("../fixtures/cpp/catch2.xml");
    const XML_V2: &str = include_str!("../fixtures/cpp/catch2_v2.xml");

    #[test]
    fn listings() {
        let l = parse_list(LIST);
        let names: Vec<&str> = l.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Adds numbers", "Fails", "A very long test case name that wraps onto the next line"]);
        let x = parse_list(LIST_XML);
        assert_eq!(x.len(), 2);
        assert_eq!(x[1].name, "Fails");
        assert_eq!(x[1].line, Some(9));
        assert_eq!(x[1].file.as_deref(), Some(std::path::Path::new("/proj/tests/catch_test.cpp")));
    }

    #[test]
    fn v3_report() {
        let suites = parse_xml(XML, "bin");
        assert_eq!(suites.len(), 1);
        let s = &suites[0];
        assert_eq!(s.name, "catch_tests");
        let names: Vec<(&str, TestStatus)> = s.tests.iter().map(|t| (t.name.as_str(), t.status)).collect();
        assert_eq!(
            names,
            vec![
                ("Adds numbers", TestStatus::Passed),
                ("Fails", TestStatus::Failed),
                ("Sections", TestStatus::Failed),
                ("Throws", TestStatus::Failed),
                ("Not yet", TestStatus::Ignored),
            ]
        );
        let fails = &s.tests[1];
        assert_eq!(fails.location.as_ref().map(|l| l.line), Some(9));
        assert_eq!(fails.failure.as_ref().map(|l| l.line), Some(11));
        assert!(fails.message.as_deref().unwrap().contains("REQUIRE( add(1, 2) == 4 )\nwith expansion:\n  3 == 4"));
        assert_eq!(fails.duration_ms, Some(2));
        assert!(fails.output.contains("printed by the test"));
        assert!(s.tests[2].message.as_deref().unwrap().starts_with("second"));
        assert!(s.tests[3].message.as_deref().unwrap().contains("boom"));
        assert_eq!(s.tests[4].message.as_deref(), Some("not ready"));
    }

    #[test]
    fn v2_report_and_hostile_input() {
        let suites = parse_xml(XML_V2, "bin");
        assert_eq!(suites[0].name, "tests");
        assert_eq!(suites[0].tests.iter().map(|t| t.status).collect::<Vec<_>>(), vec![TestStatus::Passed, TestStatus::Failed]);
        // A crashed binary leaves the report cut short.
        let cut = &XML[..XML.len() / 2];
        let _ = parse_xml(cut, "bin");
        assert!(parse_xml("garbage", "bin").is_empty());
        assert!(parse_xml("<TestCase/>", "bin").is_empty());
        let _ = parse_list("All available test cases:\n      [only tags]\n    continuation first\n");
        assert_eq!(escape("a\\b"), "a\\\\b");
    }
}
