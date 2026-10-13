//! JUnit XML test reports (pytest `--junitxml`, CTest `--output-junit`, most JavaScript runners):
//! a small, tolerant reader for the `testcase` subset, and the merge into a run's test tree.

use std::path::{Path, PathBuf};

use super::parse::{MAX_TESTS, finalize_output, push_output, seconds_ms, test_index};
use super::{SuiteKind, TestStatus, TestSuite};

/// One `<testcase>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JunitCase {
    pub classname: String,
    pub name: String,
    pub time_ms: Option<u64>,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub status: Option<TestStatus>,
    pub message: Option<String>,
    pub output: String,
}

/// The test cases of a JUnit XML document (malformed input yields what could be read).
pub(crate) fn parse(xml: &str) -> Vec<JunitCase> {
    let mut cases = Vec::new();
    let mut case: Option<JunitCase> = None;
    // The open child element of the test case and its text.
    let mut child: Option<(String, String)> = None;
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        let text = rest.get(..lt).unwrap_or("");
        if let Some((_, buf)) = child.as_mut() {
            push_text(buf, &decode(text));
        }
        rest = rest.get(lt..).unwrap_or("");
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let (data, after) = r.split_once("]]>").unwrap_or((r, ""));
            if let Some((_, buf)) = child.as_mut() {
                push_text(buf, data);
            }
            rest = after;
            continue;
        }
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |(_, a)| a);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = rest.get(1..gt).unwrap_or("");
        rest = rest.get(gt + 1..).unwrap_or("");
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            match name.trim() {
                "testcase" => {
                    if let Some(c) = case.take()
                        && cases.len() < MAX_TESTS
                    {
                        cases.push(c);
                    }
                }
                n if child.as_ref().is_some_and(|(open, _)| open == n) => {
                    if let (Some((open, text)), Some(c)) = (child.take(), case.as_mut()) {
                        close_child(c, &open, &text);
                    }
                }
                _ => {}
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attrs) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let attr = |key: &str| attribute(attrs, key);
        match name {
            "testcase" => {
                let c = JunitCase {
                    classname: attr("classname").unwrap_or_default(),
                    name: attr("name").unwrap_or_default(),
                    time_ms: attr("time").and_then(|t| seconds_ms(&t)),
                    file: attr("file"),
                    line: attr("line").and_then(|l| l.parse().ok()),
                    status: Some(TestStatus::Passed),
                    ..JunitCase::default()
                };
                if self_closing {
                    if cases.len() < MAX_TESTS {
                        cases.push(c);
                    }
                } else {
                    case = Some(c);
                }
            }
            "failure" | "error" | "skipped" | "system-out" | "system-err" => {
                let Some(c) = case.as_mut() else { continue };
                match name {
                    "failure" | "error" => {
                        c.status = Some(TestStatus::Failed);
                        c.message = attr("message").or_else(|| c.message.take());
                    }
                    "skipped" => {
                        c.status = Some(TestStatus::Ignored);
                        c.message = attr("message");
                    }
                    _ => {}
                }
                if !self_closing {
                    child = Some((name.to_string(), String::new()));
                }
            }
            _ => {}
        }
    }
    cases
}

fn push_text(buf: &mut String, text: &str) {
    if buf.len() < super::parse::MAX_TEST_OUTPUT {
        buf.push_str(text);
    }
}

fn close_child(c: &mut JunitCase, name: &str, text: &str) {
    match name {
        "failure" | "error" => {
            for line in text.lines() {
                push_output(&mut c.output, line);
            }
        }
        "system-out" | "system-err" => {
            // pytest frames captured output with `---- Captured Out ----` banners.
            let body: Vec<&str> =
                text.lines().filter(|l| !(l.starts_with("-----") && l.contains("Captured"))).collect();
            let body = body.join("\n");
            let body = body.trim_matches('\n');
            if !body.trim().is_empty() {
                if !c.output.is_empty() {
                    c.output.push('\n');
                }
                for line in body.lines() {
                    push_output(&mut c.output, line);
                }
            }
        }
        _ => {}
    }
}

/// `key="value"` (or single-quoted) in an attribute list, entity-decoded.
fn attribute(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        let eq = rest.find('=')?;
        let name = rest.get(..eq)?.trim();
        let after = rest.get(eq + 1..)?.trim_start();
        let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let body = after.get(1..)?;
        let end = body.find(quote)?;
        if name == key {
            return Some(decode(body.get(..end)?));
        }
        rest = body.get(end + 1..)?;
    }
}

/// XML entities: the five named ones and numeric character references.
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or(""));
        rest = rest.get(i..).unwrap_or("");
        let Some(semi) = rest.find(';').filter(|n| *n <= 10) else {
            out.push('&');
            rest = rest.get(1..).unwrap_or("");
            continue;
        };
        let entity = rest.get(1..semi).unwrap_or("");
        let ch = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e => e
                .strip_prefix("#x")
                .or_else(|| e.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = rest.get(semi + 1..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = rest.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

/// pytest's `classname` (`tests.test_math.TestGroup`) → the test file relative to `base` and the
/// class path, by finding the longest dotted prefix that is a `.py` file.
fn pytest_file(case: &JunitCase, base: &Path) -> Option<(String, Vec<String>)> {
    let parts: Vec<&str> = case.classname.split('.').filter(|p| !p.is_empty()).collect();
    if let Some(file) = case.file.as_deref() {
        let module = file.strip_suffix(".py").unwrap_or(file).replace(['/', '\\'], ".");
        let classes = case
            .classname
            .strip_prefix(&module)
            .map(|r| r.split('.').filter(|p| !p.is_empty()).map(String::from).collect())
            .unwrap_or_default();
        return Some((file.to_string(), classes));
    }
    for k in (1..=parts.len()).rev() {
        let candidate = format!("{}.py", parts.get(..k)?.join("/"));
        if base.join(&candidate).is_file() {
            let classes = parts.get(k..).unwrap_or(&[]).iter().map(|p| (*p).to_string()).collect();
            return Some((candidate, classes));
        }
    }
    None
}

/// Merge JUnit results into `suites` (streamed from the console): status, duration, message and
/// output from the report win. Files are resolved against `base` (the run's working directory).
pub(crate) fn merge(suites: &mut Vec<TestSuite>, cases: Vec<JunitCase>, base: &Path) {
    for case in cases {
        let (suite_key, name) = match pytest_file(&case, base) {
            Some((file, mut classes)) => {
                classes.push(case.name.clone());
                (SuiteKey::File(file), classes.join("::"))
            }
            None if case.classname.is_empty() || case.classname == case.name => {
                (SuiteKey::Named("Tests".into()), case.name.clone())
            }
            None => (SuiteKey::Named(case.classname.clone()), case.name.clone()),
        };
        let si = match suites.iter().position(|s| suite_key.matches(s)) {
            Some(si) => si,
            None => {
                suites.push(suite_key.suite());
                suites.len() - 1
            }
        };
        let Some(ti) = test_index(suites, si, &name, true) else { continue };
        let Some(t) = suites.get_mut(si).and_then(|s| s.tests.get_mut(ti)) else { continue };
        if let Some(status) = case.status {
            t.status = status;
        }
        if case.time_ms.is_some() {
            t.duration_ms = case.time_ms;
        }
        if !case.output.is_empty() {
            t.output = case.output;
        }
        if case.message.is_some() {
            t.message = case.message;
        }
        if let (Some(file), Some(line)) = (case.file.as_ref(), case.line) {
            t.location = Some(super::TestLocation { path: base.join(file), line, column: 0 });
        }
        finalize_output(t);
    }
    for s in suites.iter_mut() {
        s.finished = true;
    }
}

enum SuiteKey {
    File(String),
    Named(String),
}

impl SuiteKey {
    fn matches(&self, s: &TestSuite) -> bool {
        match self {
            Self::File(f) => s.source.as_deref() == Some(Path::new(f)),
            Self::Named(n) => s.source.is_none() && s.name == *n,
        }
    }

    fn suite(&self) -> TestSuite {
        match self {
            Self::File(f) => TestSuite::new(f.clone(), SuiteKind::File, None, Some(PathBuf::from(f))),
            Self::Named(n) => TestSuite::new(n.clone(), SuiteKind::Other, None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Framework;

    const PYTEST: &str = include_str!("fixtures/junit_pytest.xml");

    #[test]
    fn reads_pytest_report() {
        let cases = parse(PYTEST);
        assert_eq!(cases.len(), 5);
        let add = &cases[0];
        assert_eq!((add.classname.as_str(), add.name.as_str()), ("tests.test_math", "test_add"));
        assert_eq!(add.status, Some(TestStatus::Passed));
        assert_eq!(add.output, "hello from add\n");
        let fail = &cases[1];
        assert_eq!(fail.status, Some(TestStatus::Failed));
        assert_eq!(fail.message.as_deref(), Some("AssertionError: bad math\nassert (1 + 1) == 3"));
        assert!(fail.output.starts_with("def test_fail():\n>       assert 1 + 1 == 3"), "{:?}", fail.output);
        assert_eq!(cases[2].status, Some(TestStatus::Ignored));
        assert_eq!(cases[2].message.as_deref(), Some("later"));
        assert_eq!(cases[4].classname, "tests.test_math.TestGroup");
        assert_eq!(cases[4].name, "test_param[2]");
    }

    #[test]
    fn merges_into_streamed_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/test_math.py"), "").unwrap();
        // The tree from `-v` lines first, then the report.
        let mut p = super::super::parse::Parser::default();
        let mut suites = Vec::new();
        for line in include_str!("fixtures/pytest_v.txt").lines() {
            p.feed(Framework::Pytest, &mut suites, line);
        }
        p.finish(&mut suites);
        merge(&mut suites, parse(PYTEST), dir.path());
        assert_eq!(suites.len(), 1, "{:?}", suites.iter().map(|s| &s.name).collect::<Vec<_>>());
        assert_eq!(suites[0].tests.len(), 5);
        let add = suites[0].tests.iter().find(|t| t.name == "test_add").unwrap();
        assert_eq!(add.output, "hello from add\n");
        let param = suites[0].tests.iter().find(|t| t.name == "TestGroup::test_param[2]").unwrap();
        assert_eq!(param.status, TestStatus::Failed);
        assert_eq!(param.failure.as_ref().map(|l| l.line), Some(16));

        // Without the files on disk, tests group by class name.
        let mut suites = Vec::new();
        merge(&mut suites, parse(PYTEST), Path::new("/nonexistent"));
        let names: Vec<&str> = suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["tests.test_math", "tests.test_math.TestGroup"]);
    }

    #[test]
    fn tolerant_reader() {
        assert_eq!(decode("a &lt;b&gt; &amp; &#10;&#x41; &bogus; &"), "a <b> & \nA &bogus; &");
        let xml = r#"<testsuite><!-- c --><testcase name='t1' classname="c" time="1.5"/>
            <testcase name="t2"><failure><![CDATA[x < y]]></failure></testcase>
            <testcase name="t3"><skipped/></testcase><testcase name="unterminated"#;
        let cases = parse(xml);
        assert_eq!(cases.len(), 3);
        assert_eq!(cases[0].time_ms, Some(1500));
        assert_eq!(cases[1].output, "x < y\n");
        assert_eq!(cases[2].status, Some(TestStatus::Ignored));
        for junk in [
            "",
            "<",
            "<<<>>>",
            "<testcase",
            "<testcase name=\"x",
            "</testcase>",
            "<failure>x</failure>",
            "&#xFFFFFFFF;",
        ] {
            let _ = parse(junk);
            let _ = decode(junk);
        }
        let mut suites = Vec::new();
        merge(&mut suites, cases, Path::new("/"));
        assert_eq!(suites.len(), 2);
    }
}
