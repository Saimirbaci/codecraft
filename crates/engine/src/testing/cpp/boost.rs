//! Boost.Test: `--list_content` (printed on stderr) and the XML log (`--log_format=XML`).

use crate::cpp::xml::{self, Element};

use super::super::parse::{MAX_TESTS, push_output};
use super::super::{SuiteKind, TestStatus, TestSuite};
use super::{ListedTest, case, location};

/// The suite name for test cases that are not in a suite (Boost's master test suite).
pub const MASTER: &str = "Master Test Suite";

/// `--list_content`: four spaces per level, `*` marks enabled units.
///
/// ```text
/// math*
///     adds*
///     deep*
///         nested*
/// free_case*
/// ```
pub fn parse_list(text: &str) -> Vec<ListedTest> {
    let rows: Vec<(usize, String)> = text
        .lines()
        .filter_map(|l| {
            let l = l.trim_end_matches('\r');
            let body = l.trim();
            if body.is_empty() || body.contains(' ') && !body.ends_with('*') {
                return None;
            }
            let indent = (l.len() - l.trim_start().len()) / 4;
            Some((indent, body.trim_end_matches('*').to_string()))
        })
        .collect();
    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    for (i, (level, name)) in rows.iter().enumerate() {
        path.truncate(*level);
        let is_suite = rows.get(i + 1).is_some_and(|(next, _)| next > level);
        if is_suite {
            path.push(name.clone());
            continue;
        }
        if out.len() >= MAX_TESTS {
            break;
        }
        let (suite, rest) = match path.split_first() {
            Some((top, below)) => {
                let mut name_path = below.to_vec();
                name_path.push(name.clone());
                (Some(top.clone()), name_path.join("/"))
            }
            None => (None, name.clone()),
        };
        out.push(ListedTest { suite, name: rest, file: None, line: None });
    }
    out
}

/// The XML log: `TestLog > TestSuite (master) > TestSuite* > TestCase`.
pub fn parse_xml(text: &str) -> Vec<TestSuite> {
    let doc = xml::parse(text);
    let Some(log) = doc.find("TestLog") else { return Vec::new() };
    let mut suites: Vec<TestSuite> = Vec::new();
    let mut total = 0usize;
    for master in log.children_named("TestSuite") {
        walk(master, &[], &mut suites, &mut total);
    }
    // Test cases logged without a suite element.
    let mut loose = Vec::new();
    for tc in log.children_named("TestCase") {
        loose.push(test_case(tc, tc.attr("name").unwrap_or("")));
    }
    if !loose.is_empty() {
        let mut s = TestSuite::new(MASTER, SuiteKind::Other, None, None);
        s.finished = true;
        s.tests = loose;
        suites.push(s);
    }
    suites
}

/// `path` = suite names below the master suite.
fn walk(e: &Element, path: &[String], suites: &mut Vec<TestSuite>, total: &mut usize) {
    for c in &e.children {
        match c.name.as_str() {
            "TestSuite" => {
                let mut p = path.to_vec();
                p.push(c.attr("name").unwrap_or("").to_string());
                walk(c, &p, suites, total);
            }
            "TestCase" => {
                if *total >= MAX_TESTS {
                    return;
                }
                *total += 1;
                let (suite, name) = match path.split_first() {
                    Some((top, below)) => {
                        let mut n = below.to_vec();
                        n.push(c.attr("name").unwrap_or("").to_string());
                        (top.clone(), n.join("/"))
                    }
                    None => (MASTER.to_string(), c.attr("name").unwrap_or("").to_string()),
                };
                let t = test_case(c, &name);
                match suites.iter_mut().find(|s| s.name == suite) {
                    Some(s) => s.tests.push(t),
                    None => {
                        let mut s = TestSuite::new(suite, SuiteKind::Other, None, None);
                        s.finished = true;
                        s.tests.push(t);
                        suites.push(s);
                    }
                }
            }
            _ => {}
        }
    }
}

fn test_case(c: &Element, name: &str) -> crate::testing::TestCase {
    let mut failures: Vec<(String, Option<(String, u64)>)> = Vec::new();
    for x in &c.children {
        let what = match x.name.as_str() {
            "Error" => "error",
            "FatalError" => "fatal error",
            "Exception" => "exception",
            _ => continue,
        };
        let mut text = format!("{what}: {}", x.text.trim());
        if let Some(cp) = x.child("LastCheckpoint") {
            text.push_str(&format!(
                "\nlast checkpoint: {}:{}",
                cp.attr("file").unwrap_or("?"),
                cp.attr("line").unwrap_or("?")
            ));
        }
        failures.push((text, x.attr("file").map(|f| (f.to_string(), x.attr_u64("line").unwrap_or(1)))));
    }
    let skipped = c.attr("skipped") == Some("yes");
    let status = if skipped {
        TestStatus::Ignored
    } else if failures.is_empty() {
        TestStatus::Passed
    } else {
        TestStatus::Failed
    };
    let mut t = case(name, status);
    t.location = location(c.attr("file"), c.attr_u64("line"));
    // Microseconds.
    t.duration_ms = c.child_text("TestingTime").and_then(|v| v.parse::<u64>().ok()).map(|us| us / 1000);
    for (text, _) in &failures {
        push_output(&mut t.output, text);
    }
    for m in c.children.iter().filter(|m| matches!(m.name.as_str(), "Message" | "Warning")) {
        push_output(&mut t.output, &format!("{}: {}", m.name.to_ascii_lowercase(), m.text.trim()));
    }
    if let Some((text, loc)) = failures.first() {
        t.message = Some(text.clone());
        t.failure = loc.as_ref().and_then(|(f, l)| location(Some(f), Some(*l)));
    }
    if skipped {
        t.message = c.attr("reason").map(str::to_string);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../fixtures/cpp/boost_list.txt");
    const XML: &str = include_str!("../fixtures/cpp/boost.xml");

    #[test]
    fn listing() {
        let l = parse_list(LIST);
        let got: Vec<(Option<&str>, &str)> = l.iter().map(|t| (t.suite.as_deref(), t.name.as_str())).collect();
        assert_eq!(
            got,
            vec![(Some("math"), "adds"), (Some("math"), "fails"), (Some("math"), "deep/nested"), (None, "free_case")]
        );
    }

    #[test]
    fn xml_log() {
        let suites = parse_xml(XML);
        let names: Vec<&str> = suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["math", MASTER]);
        let math = &suites[0].tests;
        let got: Vec<(&str, TestStatus)> = math.iter().map(|t| (t.name.as_str(), t.status)).collect();
        assert_eq!(
            got,
            vec![
                ("adds", TestStatus::Passed),
                ("fails", TestStatus::Failed),
                ("deep/nested", TestStatus::Failed),
                ("disabled", TestStatus::Ignored)
            ]
        );
        let fails = &math[1];
        assert_eq!(fails.failure.as_ref().map(|f| f.line), Some(13));
        assert_eq!(fails.location.as_ref().map(|f| f.line), Some(11));
        assert_eq!(fails.duration_ms, Some(2));
        assert!(fails.message.as_deref().unwrap().contains("[3 != 4]"));
        assert!(math[2].message.as_deref().unwrap().contains("last checkpoint"));
        assert_eq!(math[3].message.as_deref(), Some("disabled"));
        assert_eq!(suites[1].tests[0].name, "free_case");
    }

    #[test]
    fn hostile_input() {
        let cut = parse_xml("<TestLog><TestSuite name=\"m\"><TestCase name=\"t\"><Error>x");
        assert_eq!(cut[0].tests[0].status, TestStatus::Failed);
        let _ = parse_xml(&XML[..XML.len() / 2]);
        assert!(parse_xml("nope").is_empty());
        let _ = parse_list("        deep without parent*\n*\n    \n");
    }
}
