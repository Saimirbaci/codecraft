//! C++ test frameworks in the test runner (track CL): GoogleTest, Catch2, doctest and Boost.Test.
//!
//! A C++ test run is a plain process (the test executable, optionally behind a `cmake --build`
//! step: `sh -c '<build> && exec "$@"' codecraft-test <exe> <args…>`). The framework comes from
//! the `CODECRAFT_TEST_FRAMEWORK` environment variable our commands set (`gtest`, `catch2`,
//! `doctest`, `boost`), else from the arguments (`--gtest_filter=…`, `--run_test=…`,
//! `--test-case=…`). Before it starts, the run gets the framework's structured report in a temp
//! file (`--gtest_output=json:`, Catch2 `--reporter xml --out`, doctest `--reporters=xml --out=`,
//! Boost `--log_format=XML --log_sink=`), which replaces the tree when the process ends.
//! GoogleTest's console output also builds the tree live.
//!
//! The parsers are pure and never panic on hostile input; the fixtures are samples written for
//! these tests from each framework's documented output formats.

pub(crate) mod boost;
pub(crate) mod catch2;
pub(crate) mod doctest;
pub(crate) mod gtest;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Framework, TestCase, TestLocation, TestStatus, TestSuite};
use crate::runs::ProcessCommand;

/// The environment variable naming a run's C++ test framework.
pub const FRAMEWORK_ENV: &str = "CODECRAFT_TEST_FRAMEWORK";
/// `argv[0]` of the `sh -c '<build> && exec "$@"'` wrapper.
pub const WRAPPER_ARG0: &str = "codecraft-test";
/// Largest report read.
const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;
/// Source files scanned when locating C++ tests.
const MAX_SCAN_FILES: usize = 3_000;
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;

impl Framework {
    /// One of the C++ test frameworks.
    pub fn is_cpp(self) -> bool {
        matches!(self, Self::GoogleTest | Self::Catch2 | Self::Doctest | Self::BoostTest)
    }

    /// `gtest`, `catch2`, `doctest`, `boost` (the `CODECRAFT_TEST_FRAMEWORK` values).
    pub fn cpp_key(self) -> Option<&'static str> {
        match self {
            Self::GoogleTest => Some("gtest"),
            Self::Catch2 => Some("catch2"),
            Self::Doctest => Some("doctest"),
            Self::BoostTest => Some("boost"),
            _ => None,
        }
    }

    /// Parse a C++ framework name (`gtest`, `googletest`, `catch2`, `catch`, `doctest`, `boost`).
    pub fn parse_cpp(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gtest" | "googletest" | "google test" | "google_test" => Some(Self::GoogleTest),
            "catch2" | "catch" => Some(Self::Catch2),
            "doctest" => Some(Self::Doctest),
            "boost" | "boost.test" | "boosttest" | "boost_test" => Some(Self::BoostTest),
            _ => None,
        }
    }

    /// The display name (`Google Test`, `Catch2`, `doctest`, `Boost.Test`).
    pub fn cpp_label(self) -> &'static str {
        match self {
            Self::GoogleTest => "Google Test",
            Self::Catch2 => "Catch2",
            Self::Doctest => "doctest",
            Self::BoostTest => "Boost.Test",
            Self::Cargo => "Cargo",
            Self::Pytest => "pytest",
            Self::CTest => "CTest",
            Self::Zig => "Zig",
            Self::Jest => "Jest",
            Self::Vitest => "Vitest",
        }
    }
}

/// A test listed by the executable (`--gtest_list_tests`, `--list-tests`, …).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedTest {
    /// GoogleTest suite, doctest test suite, Boost.Test suite path; `None` for Catch2 and
    /// suite-less tests.
    pub suite: Option<String>,
    pub name: String,
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// 0-based.
    #[serde(default)]
    pub line: Option<usize>,
}

// ---------------------------------------------------------------- commands

/// Where the test executable's own arguments start in `cmd.args` (after the `sh -c` wrapper).
pub fn test_args_start(cmd: &ProcessCommand) -> usize {
    if is_wrapper(cmd) { 4 } else { 0 }
}

fn is_wrapper(cmd: &ProcessCommand) -> bool {
    super::basename(&cmd.program) == "sh"
        && cmd.args.first().is_some_and(|a| a == "-c")
        && cmd.args.get(2).is_some_and(|a| a == WRAPPER_ARG0)
        && cmd.args.len() >= 4
}

/// The C++ framework of a test process, if it is one.
pub(crate) fn framework_of(cmd: &ProcessCommand) -> Option<Framework> {
    if let Some((_, v)) = cmd.env.iter().rev().find(|(k, _)| k == FRAMEWORK_ENV) {
        return Framework::parse_cpp(v);
    }
    let program = super::basename(&cmd.program);
    if matches!(program, "sh" | "bash" | "zsh") && !is_wrapper(cmd) {
        return None;
    }
    let args = cmd.args.get(test_args_start(cmd)..).unwrap_or(&[]);
    if args.iter().any(|a| a.starts_with("--gtest_")) {
        Some(Framework::GoogleTest)
    } else if args.iter().any(|a| a.starts_with("--run_test=") || a.starts_with("--log_format=")) {
        Some(Framework::BoostTest)
    } else if args.iter().any(|a| {
        ["--test-case=", "-tc=", "--test-suite=", "-ts=", "--dt-test-case=", "--reporters="]
            .iter()
            .any(|p| a.starts_with(p))
    }) {
        Some(Framework::Doctest)
    } else {
        None
    }
}

/// The arguments that make `fw` write its structured report to `path`.
pub fn report_args(fw: Framework, path: &Path) -> Vec<String> {
    let p = path.display().to_string();
    match fw {
        Framework::GoogleTest => vec![format!("--gtest_output=json:{p}")],
        Framework::Catch2 => {
            vec!["--reporter".into(), "xml".into(), "--out".into(), p, "--durations".into(), "yes".into()]
        }
        Framework::Doctest => vec!["--reporters=xml".into(), format!("--out={p}"), "--duration=true".into()],
        Framework::BoostTest => vec![
            "--log_format=XML".into(),
            format!("--log_sink={p}"),
            "--log_level=test_suite".into(),
            "--report_level=short".into(),
        ],
        _ => Vec::new(),
    }
}

/// A report the user asked for already (we read it instead of adding ours).
fn existing_report(fw: Framework, args: &[String], cwd: &Path) -> Option<Option<PathBuf>> {
    let at = |p: &str| {
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    match fw {
        Framework::GoogleTest => {
            let v = args.iter().find_map(|a| a.strip_prefix("--gtest_output="))?;
            Some(v.strip_prefix("json:").map(at))
        }
        Framework::Catch2 => {
            let i = args.iter().position(|a| a == "-r" || a == "--reporter" || a.starts_with("--reporter="))?;
            let xml = args.get(i).is_some_and(|a| a.ends_with("=xml"))
                || args.get(i + 1).is_some_and(|a| a.starts_with("xml"));
            let out = args
                .iter()
                .position(|a| a == "-o" || a == "--out")
                .and_then(|o| args.get(o + 1))
                .map(String::as_str)
                .or_else(|| args.get(i + 1).and_then(|a| a.strip_prefix("xml::out=")));
            Some(if xml { out.map(at) } else { None })
        }
        Framework::Doctest => {
            let r = args.iter().find_map(|a| a.strip_prefix("--reporters=").or_else(|| a.strip_prefix("-r=")))?;
            let out = args.iter().find_map(|a| a.strip_prefix("--out=").or_else(|| a.strip_prefix("-o=")));
            Some(if r.contains("xml") { out.map(at) } else { None })
        }
        Framework::BoostTest => {
            let f = args.iter().find_map(|a| a.strip_prefix("--log_format="))?;
            let sink = args.iter().find_map(|a| a.strip_prefix("--log_sink="));
            Some(if f.eq_ignore_ascii_case("xml") { sink.filter(|s| !matches!(*s, "stdout" | "stderr")).map(at) } else { None })
        }
        _ => None,
    }
}

/// Before a C++ test process starts: add the framework's report in a temp file. Returns the
/// command to run and the report path.
pub(crate) fn instrument(cmd: &ProcessCommand, fw: Framework) -> (ProcessCommand, Option<PathBuf>) {
    let start = test_args_start(cmd);
    let args = cmd.args.get(start..).unwrap_or(&[]);
    if let Some(existing) = existing_report(fw, args, &cmd.cwd) {
        return (cmd.clone(), existing);
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ext = if fw == Framework::GoogleTest { "json" } else { "xml" };
    let path = std::env::temp_dir().join(format!(
        "codecraft-{}-{}-{}-{seq}.{ext}",
        fw.cpp_key().unwrap_or("cpp"),
        std::process::id(),
        super::now_millis()
    ));
    let _ = std::fs::remove_file(&path);
    let mut out = cmd.clone();
    out.args.extend(report_args(fw, &path));
    (out, Some(path))
}

/// The arguments that run one test (or one suite, or everything when both are `None`).
pub fn filter_args(fw: Framework, suite: Option<&str>, test: Option<&str>) -> Vec<String> {
    match fw {
        Framework::GoogleTest => match (suite, test) {
            (Some(s), Some(t)) => vec![format!("--gtest_filter={s}.{t}")],
            (Some(s), None) => vec![format!("--gtest_filter={s}.*")],
            (None, Some(t)) => vec![format!("--gtest_filter=*.{t}")],
            (None, None) => Vec::new(),
        },
        Framework::Catch2 => match test {
            Some(t) => vec![catch2::escape(t)],
            None => suite.map(|s| vec![s.to_string()]).unwrap_or_default(),
        },
        Framework::Doctest => {
            let mut v = Vec::new();
            if let Some(s) = suite.filter(|s| !s.is_empty()) {
                v.push(format!("--test-suite={}", doctest::escape(s)));
            }
            if let Some(t) = test {
                v.push(format!("--test-case={}", doctest::escape(t)));
            }
            v
        }
        Framework::BoostTest => match (suite, test) {
            (Some(s), Some(t)) => vec![format!("--run_test={s}/{t}")],
            (Some(s), None) => vec![format!("--run_test={s}")],
            (None, Some(t)) => vec![format!("--run_test={t}")],
            (None, None) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Options of Catch2 that take a value (their value is not a test spec).
const CATCH2_VALUE_OPTIONS: &[&str] = &[
    "-r", "--reporter", "-o", "--out", "-d", "--durations", "-D", "--min-duration", "-c", "--section", "-x",
    "--abortx", "-w", "--warn", "-v", "--verbosity", "--order", "--rng-seed", "--use-colour", "--colour-mode",
    "--input-file", "-f", "--shard-count", "--shard-index", "--benchmark-samples", "--benchmark-resamples",
    "--benchmark-confidence-interval", "--benchmark-warmup-time", "--wait-for-keypress", "-n", "--name",
];

/// The test executable's arguments without the framework's test filter.
pub fn without_filter(fw: Framework, args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let drop = match fw {
            Framework::GoogleTest => a.starts_with("--gtest_filter="),
            Framework::Doctest => ["--test-case=", "-tc=", "--test-suite=", "-ts=", "--dt-test-case=", "--dt-test-suite="]
                .iter()
                .any(|p| a.starts_with(p)),
            Framework::BoostTest => {
                if a == "-t" {
                    it.next();
                    true
                } else {
                    a.starts_with("--run_test=")
                }
            }
            Framework::Catch2 => {
                if CATCH2_VALUE_OPTIONS.contains(&a.as_str()) {
                    out.push(a.clone());
                    if let Some(v) = it.next() {
                        out.push(v.clone());
                    }
                    continue;
                }
                !a.starts_with('-')
            }
            _ => false,
        };
        if !drop {
            out.push(a.clone());
        }
    }
    out
}

/// How to run only the failed tests of a C++ run.
pub(crate) fn rerun_failed(
    fw: Framework,
    cmd: &ProcessCommand,
    failed: &[(&TestSuite, &TestCase)],
) -> Result<ProcessCommand, String> {
    let start = test_args_start(cmd);
    let mut out = cmd.clone();
    let head: Vec<String> = cmd.args.get(..start).unwrap_or(&[]).to_vec();
    let mut args = without_filter(fw, cmd.args.get(start..).unwrap_or(&[]));
    match fw {
        Framework::GoogleTest => {
            let names: Vec<String> = dedup(failed.iter().map(|(s, t)| format!("{}.{}", s.name, t.name)));
            args.push(format!("--gtest_filter={}", names.join(":")));
        }
        Framework::Catch2 => {
            let names: Vec<String> = dedup(failed.iter().map(|(_, t)| catch2::escape(&t.name)));
            args.push(names.join(","));
        }
        Framework::Doctest => {
            let names: Vec<String> = dedup(failed.iter().map(|(_, t)| doctest::escape(&t.name)));
            args.push(format!("--test-case={}", names.join(",")));
        }
        Framework::BoostTest => {
            // `suite/a,b:other/c`; tests of the master suite are named by their own path.
            let mut groups: Vec<(String, Vec<String>)> = Vec::new();
            for (s, t) in failed {
                let (prefix, leaf) = match t.name.rsplit_once('/') {
                    Some((p, l)) => (format!("{}/{p}", s.name), l.to_string()),
                    None => (s.name.clone(), t.name.clone()),
                };
                let prefix = if s.name == boost::MASTER { prefix.trim_start_matches(boost::MASTER).trim_start_matches('/').to_string() } else { prefix };
                match groups.iter_mut().find(|(p, _)| *p == prefix) {
                    Some((_, v)) if !v.contains(&leaf) => v.push(leaf),
                    Some(_) => {}
                    None => groups.push((prefix, vec![leaf])),
                }
            }
            let spec: Vec<String> = groups
                .into_iter()
                .map(|(p, v)| if p.is_empty() { v.join(",") } else { format!("{p}/{}", v.join(",")) })
                .collect();
            args.push(format!("--run_test={}", spec.join(":")));
        }
        _ => return Err("not a C++ test run".into()),
    }
    out.args = head.into_iter().chain(args).collect();
    Ok(out)
}

fn dedup(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in it {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// The arguments that list the tests of an executable.
pub fn list_args(fw: Framework) -> Vec<&'static str> {
    match fw {
        Framework::GoogleTest => vec!["--gtest_list_tests"],
        Framework::Catch2 => vec!["--list-tests", "--reporter", "xml"],
        Framework::Doctest => vec!["--list-test-cases", "--reporters=xml"],
        Framework::BoostTest => vec!["--list_content"],
        _ => Vec::new(),
    }
}

/// Parse a listing (Boost.Test prints it on stderr, so both streams are passed).
pub fn parse_list(fw: Framework, stdout: &str, stderr: &str) -> Vec<ListedTest> {
    match fw {
        Framework::GoogleTest => gtest::parse_list(stdout),
        Framework::Catch2 => catch2::parse_list(stdout),
        Framework::Doctest => doctest::parse_list(stdout),
        Framework::BoostTest => {
            let from_err = boost::parse_list(stderr);
            if from_err.is_empty() { boost::parse_list(stdout) } else { from_err }
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------- reports

/// Parse a structured report of `fw` into suites; relative paths resolve against `cwd`.
pub fn parse_report(fw: Framework, text: &str, cwd: &Path, binary: &str) -> Vec<TestSuite> {
    let mut suites = match fw {
        Framework::GoogleTest => gtest::parse_json(text),
        Framework::Catch2 => catch2::parse_xml(text, binary),
        Framework::Doctest => doctest::parse_xml(text, binary),
        Framework::BoostTest => boost::parse_xml(text),
        _ => Vec::new(),
    };
    for t in suites.iter_mut().flat_map(|s| s.tests.iter_mut()) {
        for loc in [t.location.as_mut(), t.failure.as_mut()].into_iter().flatten() {
            if loc.path.is_relative() {
                loc.path = crate::normalize_path(&cwd.join(&loc.path));
            }
        }
    }
    suites
}

/// When a C++ run ended: read its report and replace the live tree with it (keeping console
/// output captured per test when the report has none). The report file is removed.
pub(crate) fn merge_report(run: &mut super::TestRun, path: &Path) {
    let fw = run.framework;
    let text = std::fs::metadata(path)
        .ok()
        .filter(|m| m.len() <= MAX_REPORT_BYTES)
        .and_then(|_| std::fs::read(path).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let _ = std::fs::remove_file(path);
    let Some(text) = text else { return };
    let binary = match &run.origin {
        Some(super::TestOrigin::Process(cmd)) => {
            let start = test_args_start(cmd);
            let exe = if start > 0 { cmd.args.get(3).cloned().unwrap_or_default() } else { cmd.program.clone() };
            Path::new(&exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(exe)
        }
        _ => "tests".into(),
    };
    let mut fresh = parse_report(fw, &text, &run.cwd, &binary);
    if fresh.iter().all(|s| s.tests.is_empty()) {
        return;
    }
    for suite in &mut fresh {
        suite.finished = true;
        for t in &mut suite.tests {
            let old = run
                .suites
                .iter()
                .filter(|s| s.name == suite.name)
                .flat_map(|s| s.tests.iter())
                .find(|o| o.name == t.name);
            if let Some(old) = old {
                if t.output.is_empty() {
                    t.output = old.output.clone();
                }
                if t.failure.is_none() {
                    t.failure = old.failure.clone();
                }
                if t.duration_ms.is_none() {
                    t.duration_ms = old.duration_ms;
                }
            }
        }
    }
    // Tests the console saw that the report lacks (a crash cut the report short) stay.
    for old in &run.suites {
        for t in &old.tests {
            let present = fresh.iter().any(|s| s.name == old.name && s.tests.iter().any(|n| n.name == t.name));
            if !present {
                match fresh.iter_mut().find(|s| s.name == old.name) {
                    Some(s) => s.tests.push(t.clone()),
                    None => {
                        let mut s = TestSuite::new(old.name.clone(), old.kind, None, None);
                        s.finished = true;
                        s.tests.push(t.clone());
                        fresh.push(s);
                    }
                }
            }
        }
    }
    run.suites = fresh;
}

/// A location from a report: `file` and a 1-based `line`.
pub(crate) fn location(file: Option<&str>, line: Option<u64>) -> Option<TestLocation> {
    let file = file.map(str::trim).filter(|f| !f.is_empty() && !f.contains('\0'))?;
    let line = usize::try_from(line.unwrap_or(1)).unwrap_or(1).saturating_sub(1);
    Some(TestLocation { path: PathBuf::from(file), line, column: 0 })
}

/// A new test case with a final status.
pub(crate) fn case(name: &str, status: TestStatus) -> TestCase {
    let mut t = TestCase::new(name);
    t.status = status;
    t
}

/// Seconds (`0.25`, `0.25s`) → milliseconds.
pub(crate) fn secs_ms(s: &str) -> Option<u64> {
    let v: f64 = s.trim().trim_end_matches('s').trim().parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v * 1000.0).round().min(u64::MAX as f64) as u64)
}

// ---------------------------------------------------------------- sources

/// One test macro in a source file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestMacro {
    /// 0-based.
    pub line: usize,
    pub framework: Framework,
    /// A suite (Boost `BOOST_AUTO_TEST_SUITE`, doctest `TEST_SUITE`) rather than a test.
    pub is_suite: bool,
    /// GoogleTest suite, Boost suite path, doctest suite of the test.
    pub suite: Option<String>,
    /// The test's name as the framework reports it (`Adds`, `Scenario: x`), or the suite's.
    pub name: String,
    /// The filter that runs it: `(suite, test)` for [`filter_args`] (GoogleTest's parameterized and
    /// typed tests use wildcards).
    pub filter: (Option<String>, Option<String>),
}

/// The framework a C/C++ source uses, from its includes (else its macros).
pub fn framework_in_source(text: &str) -> Option<Framework> {
    let mut by_macro = None;
    for line in text.lines().take(20_000) {
        let t = line.trim_start();
        if let Some(inc) = t.strip_prefix('#').map(str::trim_start).and_then(|r| r.strip_prefix("include")) {
            let inc = inc.trim();
            if inc.contains("gtest/") || inc.contains("gmock/") {
                return Some(Framework::GoogleTest);
            }
            if inc.contains("catch2/") || inc.contains("catch.hpp") || inc.contains("catch_amalgamated") {
                return Some(Framework::Catch2);
            }
            if inc.contains("doctest") {
                return Some(Framework::Doctest);
            }
            if inc.contains("boost/test/") {
                return Some(Framework::BoostTest);
            }
        }
        if by_macro.is_none() {
            by_macro = if ["TEST(", "TEST_F(", "TEST_P(", "TYPED_TEST("].iter().any(|m| t.starts_with(m)) {
                Some(Framework::GoogleTest)
            } else if t.starts_with("BOOST_AUTO_TEST_CASE(") || t.starts_with("BOOST_FIXTURE_TEST_CASE(") {
                Some(Framework::BoostTest)
            } else if t.starts_with("TEST_CASE_FIXTURE(") || t.starts_with("TEST_SUITE(") {
                Some(Framework::Doctest)
            } else if t.starts_with("TEST_CASE(") || t.starts_with("TEST_CASE_METHOD(") || t.starts_with("SCENARIO(") {
                Some(Framework::Catch2)
            } else {
                None
            };
        }
    }
    by_macro
}

/// `MACRO(args…)` at the start of a (trimmed) line → (macro, top-level arguments).
fn macro_call(line: &str) -> Option<(&str, Vec<String>)> {
    let t = line.trim_start();
    let open = t.find('(')?;
    let name = t.get(..open)?.trim_end();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
        return None;
    }
    let body = t.get(open + 1..)?;
    let mut args = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut quote, mut escape) = (0usize, false, false);
    for c in body.chars() {
        if quote {
            cur.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                quote = false;
            }
            continue;
        }
        match c {
            '"' => {
                quote = true;
                cur.push(c);
            }
            '(' | '<' | '[' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ')' if depth == 0 => {
                args.push(cur.trim().to_string());
                return Some((name, args));
            }
            ')' | '>' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => args.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    // The call continues on the next line: what we have is enough for the name.
    args.push(cur.trim().to_string());
    Some((name, args))
}

/// A C string literal argument (`"a \"b\""`) → its text.
fn string_arg(arg: &str) -> Option<String> {
    let body = arg.trim().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    Some(out)
}

fn ident_arg(arg: Option<&String>) -> Option<String> {
    let a = arg?.trim();
    (!a.is_empty() && a.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| a.to_string())
}

/// The test macros of a C/C++ source (tests and suites), with the framework they belong to.
pub fn test_macros(text: &str) -> Vec<TestMacro> {
    let Some(fw) = framework_in_source(text) else { return Vec::new() };
    let mut out = Vec::new();
    // Boost suites nest; doctest `TEST_SUITE("x") {` applies to the braces that follow.
    let mut boost_suites: Vec<String> = Vec::new();
    let mut doctest_suite: Option<(String, i64)> = None;
    let mut depth: i64 = 0;
    for (i, line) in text.lines().enumerate().take(200_000) {
        let t = line.trim_start();
        if fw == Framework::Doctest
            && let Some((_, d)) = &doctest_suite
            && depth < *d
        {
            doctest_suite = None;
        }
        if let Some((name, args)) = macro_call(t) {
            let m = |is_suite: bool, suite: Option<String>, name: String, filter: (Option<String>, Option<String>)| TestMacro {
                line: i,
                framework: fw,
                is_suite,
                suite,
                name,
                filter,
            };
            match (fw, name) {
                (Framework::GoogleTest, "TEST" | "TEST_F" | "TEST_P" | "TYPED_TEST" | "TYPED_TEST_P") => {
                    if let (Some(s), Some(n)) = (ident_arg(args.first()), ident_arg(args.get(1))) {
                        let filter = match name {
                            "TEST_P" => (Some(format!("*/{s}")), Some(format!("{n}/*"))),
                            "TYPED_TEST" => (Some(format!("{s}/*")), Some(n.clone())),
                            "TYPED_TEST_P" => (Some(format!("*/{s}/*")), Some(n.clone())),
                            _ => (Some(s.clone()), Some(n.clone())),
                        };
                        out.push(m(false, Some(s), n, filter));
                    }
                }
                (Framework::Catch2, "TEST_CASE" | "SCENARIO" | "TEMPLATE_TEST_CASE" | "TEMPLATE_PRODUCT_TEST_CASE")
                | (Framework::Catch2, "TEST_CASE_METHOD" | "TEMPLATE_TEST_CASE_METHOD") => {
                    let arg = if name.ends_with("_METHOD") { args.get(1) } else { args.first() };
                    if let Some(n) = arg.and_then(|a| string_arg(a)) {
                        let (shown, filter) = match name {
                            "SCENARIO" => (format!("Scenario: {n}"), format!("Scenario: {n}")),
                            n2 if n2.starts_with("TEMPLATE") => (n.clone(), format!("{n} - *")),
                            _ => (n.clone(), n.clone()),
                        };
                        out.push(m(false, None, shown, (None, Some(filter))));
                    }
                }
                (Framework::Doctest, "TEST_SUITE" | "TEST_SUITE_BEGIN") => {
                    if let Some(n) = args.first().and_then(|a| string_arg(a)) {
                        if name == "TEST_SUITE" {
                            doctest_suite = Some((n.clone(), depth + 1));
                        } else {
                            doctest_suite = Some((n.clone(), i64::MIN));
                        }
                        out.push(m(true, Some(n.clone()), n.clone(), (Some(n), None)));
                    }
                }
                (Framework::Doctest, "TEST_SUITE_END") => doctest_suite = None,
                (Framework::Doctest, "TEST_CASE" | "SCENARIO" | "TEST_CASE_FIXTURE" | "TEST_CASE_TEMPLATE") => {
                    let arg = if name == "TEST_CASE_FIXTURE" { args.get(1) } else { args.first() };
                    if let Some(n) = arg.and_then(|a| string_arg(a)) {
                        let suite = doctest_suite.as_ref().map(|(s, _)| s.clone());
                        let (shown, filter) = match name {
                            "SCENARIO" => (format!("  Scenario: {n}"), format!("*Scenario: {n}")),
                            "TEST_CASE_TEMPLATE" => (n.clone(), format!("{n}<*")),
                            _ => (n.clone(), n.clone()),
                        };
                        out.push(m(false, suite.clone(), shown, (suite, Some(filter))));
                    }
                }
                (Framework::BoostTest, "BOOST_AUTO_TEST_SUITE" | "BOOST_FIXTURE_TEST_SUITE") => {
                    if let Some(n) = ident_arg(args.first()) {
                        boost_suites.push(n);
                        let path = boost_suites.join("/");
                        out.push(m(true, Some(path.clone()), path.clone(), (Some(path), None)));
                    }
                }
                (Framework::BoostTest, "BOOST_AUTO_TEST_SUITE_END") => {
                    boost_suites.pop();
                }
                (
                    Framework::BoostTest,
                    "BOOST_AUTO_TEST_CASE" | "BOOST_FIXTURE_TEST_CASE" | "BOOST_DATA_TEST_CASE"
                    | "BOOST_AUTO_TEST_CASE_TEMPLATE" | "BOOST_PARAM_TEST_CASE",
                ) => {
                    if let Some(n) = ident_arg(args.first()) {
                        let suite = (!boost_suites.is_empty()).then(|| boost_suites.join("/"));
                        out.push(m(false, suite.clone(), n.clone(), (suite, Some(n))));
                    }
                }
                _ => {}
            }
        }
        if fw == Framework::Doctest {
            for c in strip_strings(line).chars() {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
            }
        }
        if out.len() >= 10_000 {
            break;
        }
    }
    out
}

/// A line without its string and char literals and `//` comment (for brace counting).
fn strip_strings(line: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '/' && chars.peek() == Some(&'/') {
                    break;
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Lines with a gutter ▶ for C++ tests (test and suite macros).
pub fn gutter_lines(text: &str) -> Vec<usize> {
    test_macros(text).into_iter().map(|m| m.line).collect()
}

/// The test (or suite) to run for the caret on `line`: the macro on that line, else the nearest
/// test macro above it.
pub fn macro_at(text: &str, line: usize) -> Option<TestMacro> {
    let macros = test_macros(text);
    macros
        .iter()
        .find(|m| m.line == line)
        .or_else(|| macros.iter().filter(|m| m.line <= line && !m.is_suite).max_by_key(|m| m.line))
        .cloned()
}

/// The 0-based line of a test's macro in `text`, for a result named `suite`/`name`.
pub fn find_definition(text: &str, fw: Framework, suite: &str, name: &str) -> Option<usize> {
    let macros = test_macros(text);
    let base = |n: &str| n.split('/').next().unwrap_or(n).to_string();
    macros
        .iter()
        .filter(|m| !m.is_suite && m.framework == fw)
        .find(|m| match fw {
            Framework::GoogleTest => {
                // `Inst/Suite` + `Name/0`, `Suite/0` + `Name`.
                let s = m.suite.as_deref().unwrap_or("");
                let suite_ok = suite == s || suite.ends_with(&format!("/{s}")) || base(suite) == s;
                suite_ok && (m.name == name || base(name) == m.name)
            }
            Framework::BoostTest => {
                let full = match &m.suite {
                    Some(s) => format!("{s}/{}", m.name),
                    None => m.name.clone(),
                };
                let wanted = if suite == boost::MASTER { name.to_string() } else { format!("{suite}/{name}") };
                full == wanted || m.name == name
            }
            _ => m.name == name || m.name.trim() == name.trim() || name.starts_with(&format!("{} - ", m.name)),
        })
        .map(|m| m.line)
}

/// C/C++ sources under `root` (build directories and dot directories skipped), bounded.
pub fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        visited += 1;
        if visited > 20_000 || out.len() >= MAX_SCAN_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                let skip = name.starts_with('.')
                    || name.starts_with("cmake-build-")
                    || matches!(name.as_str(), "build" | "target" | "node_modules" | "out" | "_deps" | "vcpkg_installed");
                if !skip {
                    stack.push(e.path());
                }
            } else if ft.is_file() {
                let ext = Path::new(&name).extension().and_then(|x| x.to_str()).unwrap_or("");
                if matches!(ext, "cpp" | "cc" | "cxx" | "c++" | "C" | "c" | "hpp" | "h" | "hh" | "ipp") {
                    out.push(e.path());
                }
            }
        }
    }
    out
}

/// Find where a C++ test is defined by scanning the project's sources (worker thread).
pub(crate) fn locate(
    fw: Framework,
    sources: &[PathBuf],
    suite: &str,
    name: &str,
    read: &mut dyn FnMut(&Path) -> Option<String>,
) -> Option<TestLocation> {
    for f in sources {
        if !std::fs::metadata(f).is_ok_and(|m| m.len() <= MAX_SCAN_BYTES) {
            continue;
        }
        let Some(text) = read(f) else { continue };
        if !text.contains("TEST") {
            continue;
        }
        if let Some(line) = find_definition(&text, fw, suite, name) {
            return Some(TestLocation { path: f.clone(), line, column: 0 });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(program: &str, args: &[&str]) -> ProcessCommand {
        ProcessCommand::new(program, args, "/w")
    }

    #[test]
    fn frameworks_from_env_and_arguments() {
        let mut c = cmd("/w/build/tests", &[]);
        assert_eq!(framework_of(&c), None);
        c.env.push((FRAMEWORK_ENV.into(), "catch2".into()));
        assert_eq!(framework_of(&c), Some(Framework::Catch2));
        assert_eq!(framework_of(&cmd("/b/t", &["--gtest_filter=A.b"])), Some(Framework::GoogleTest));
        assert_eq!(framework_of(&cmd("/b/t", &["--run_test=s/c"])), Some(Framework::BoostTest));
        assert_eq!(framework_of(&cmd("/b/t", &["--test-case=x"])), Some(Framework::Doctest));
        // A plain shell script is not a C++ test run; the build wrapper is.
        assert_eq!(framework_of(&cmd("sh", &["-c", "x --gtest_filter=A.b"])), None);
        let w = cmd("sh", &["-c", "cmake --build b && exec \"$@\"", WRAPPER_ARG0, "/b/t", "--gtest_filter=A.b"]);
        assert_eq!(test_args_start(&w), 4);
        assert_eq!(framework_of(&w), Some(Framework::GoogleTest));
        for k in ["gtest", "catch2", "doctest", "boost"] {
            assert_eq!(Framework::parse_cpp(k).and_then(Framework::cpp_key), Some(k));
        }
    }

    #[test]
    fn instrumenting_adds_a_report_unless_one_is_asked_for() {
        let mut c = cmd("/b/t", &["--gtest_filter=A.*"]);
        c.env.push((FRAMEWORK_ENV.into(), "gtest".into()));
        let (out, report) = instrument(&c, Framework::GoogleTest);
        let report = report.unwrap();
        assert!(out.args.last().unwrap().starts_with("--gtest_output=json:"));
        assert!(out.args.last().unwrap().ends_with(&report.display().to_string()));
        let mine = cmd("/b/t", &["--gtest_output=json:out.json"]);
        let (out, report) = instrument(&mine, Framework::GoogleTest);
        assert_eq!(out, mine);
        assert_eq!(report, Some(PathBuf::from("/w/out.json")));
        let (out, report) = instrument(&cmd("/b/t", &[]), Framework::BoostTest);
        assert!(out.args.iter().any(|a| a == "--log_format=XML"));
        assert!(report.unwrap().extension().is_some_and(|e| e == "xml"));
        let (out, _) = instrument(&cmd("/b/t", &["-r", "xml", "-o", "r.xml"]), Framework::Catch2);
        assert_eq!(out.args.len(), 4);
        let (_, r) = instrument(&cmd("/b/t", &["--reporters=xml", "--out=d.xml"]), Framework::Doctest);
        assert_eq!(r, Some(PathBuf::from("/w/d.xml")));
    }

    #[test]
    fn filters_and_rerun_failed() {
        assert_eq!(filter_args(Framework::GoogleTest, Some("Math"), Some("Adds")), vec!["--gtest_filter=Math.Adds"]);
        assert_eq!(filter_args(Framework::GoogleTest, Some("Math"), None), vec!["--gtest_filter=Math.*"]);
        assert_eq!(filter_args(Framework::Catch2, None, Some("a, [b]")), vec!["a\\, \\[b\\]"]);
        assert_eq!(filter_args(Framework::Doctest, Some("s"), Some("t")), vec!["--test-suite=s", "--test-case=t"]);
        assert_eq!(filter_args(Framework::BoostTest, Some("math"), Some("adds")), vec!["--run_test=math/adds"]);
        assert!(filter_args(Framework::Cargo, None, None).is_empty());

        let mut suite = TestSuite::new("Math", crate::testing::SuiteKind::Other, None, None);
        suite.tests = vec![case("Fails", TestStatus::Failed), case("Other", TestStatus::Failed)];
        let failed: Vec<(&TestSuite, &TestCase)> = suite.tests.iter().map(|t| (&suite, t)).collect();
        let w = cmd("sh", &["-c", "b && exec \"$@\"", WRAPPER_ARG0, "/b/t", "--gtest_filter=Math.*", "--gtest_repeat=1"]);
        let r = rerun_failed(Framework::GoogleTest, &w, &failed).unwrap();
        assert_eq!(&r.args[4..], ["--gtest_repeat=1", "--gtest_filter=Math.Fails:Math.Other"]);
        let r = rerun_failed(Framework::Catch2, &cmd("/b/t", &["old", "-r", "console", "--success"]), &failed).unwrap();
        assert_eq!(r.args, ["-r", "console", "--success", "Fails,Other"]);
        let r = rerun_failed(Framework::Doctest, &cmd("/b/t", &["--test-case=x"]), &failed).unwrap();
        assert_eq!(r.args, ["--test-case=Fails,Other"]);
        let mut nested = TestSuite::new("math", crate::testing::SuiteKind::Other, None, None);
        nested.tests = vec![case("deep/a", TestStatus::Failed), case("b", TestStatus::Failed)];
        let mut master = TestSuite::new(boost::MASTER, crate::testing::SuiteKind::Other, None, None);
        master.tests = vec![case("free", TestStatus::Failed)];
        let failed: Vec<(&TestSuite, &TestCase)> =
            nested.tests.iter().map(|t| (&nested, t)).chain(master.tests.iter().map(|t| (&master, t))).collect();
        let r = rerun_failed(Framework::BoostTest, &cmd("/b/t", &["-t", "old", "--run_test=x"]), &failed).unwrap();
        assert_eq!(r.args, ["--run_test=math/deep/a:math/b:free"]);
        assert!(rerun_failed(Framework::Zig, &cmd("/b/t", &[]), &failed).is_err());
    }

    const GTEST_SRC: &str = "#include <gtest/gtest.h>\n\nTEST(MathTest, Adds) {\n  EXPECT_EQ(1, 1);\n}\n\
        TEST_F(Fixture, Works) {}\nTEST_P(Param, Each) {}\nTYPED_TEST(Typed, Sizes) {}\n";
    const CATCH_SRC: &str = "#include <catch2/catch_test_macros.hpp>\nTEST_CASE(\"adds \\\"quoted\\\"\", \"[math]\") {\n}\n\
        SCENARIO(\"vectors\") {}\nTEST_CASE_METHOD(Fix, \"method case\") {}\nTEMPLATE_TEST_CASE(\"tpl\", \"\", int) {}\n";
    const DOCTEST_SRC: &str = "#include \"doctest/doctest.h\"\nTEST_CASE(\"free\") {}\nTEST_SUITE(\"math\") {\n\
        TEST_CASE(\"in suite\") { CHECK(\"}\" != nullptr); }\n}\nTEST_CASE(\"after\") {}\n";
    const BOOST_SRC: &str = "#define BOOST_TEST_MODULE m\n#include <boost/test/unit_test.hpp>\nBOOST_AUTO_TEST_CASE(free_case) {}\n\
        BOOST_AUTO_TEST_SUITE(math)\nBOOST_AUTO_TEST_SUITE(deep)\nBOOST_AUTO_TEST_CASE(adds) {}\nBOOST_AUTO_TEST_SUITE_END()\n\
        BOOST_FIXTURE_TEST_CASE(fixed, F) {}\nBOOST_AUTO_TEST_SUITE_END()\n";

    #[test]
    fn macros_in_sources() {
        assert_eq!(framework_in_source(GTEST_SRC), Some(Framework::GoogleTest));
        assert_eq!(framework_in_source("TEST_CASE(\"x\") {}"), Some(Framework::Catch2));
        assert_eq!(framework_in_source("int main() {}"), None);
        let g = test_macros(GTEST_SRC);
        assert_eq!(g.iter().map(|m| m.line).collect::<Vec<_>>(), vec![2, 5, 6, 7]);
        assert_eq!(g[0].filter, (Some("MathTest".into()), Some("Adds".into())));
        assert_eq!(g[2].filter, (Some("*/Param".into()), Some("Each/*".into())));
        assert_eq!(g[3].filter, (Some("Typed/*".into()), Some("Sizes".into())));
        assert_eq!(gutter_lines(GTEST_SRC), vec![2, 5, 6, 7]);
        assert_eq!(macro_at(GTEST_SRC, 3).unwrap().name, "Adds");
        assert_eq!(macro_at(GTEST_SRC, 0), None);

        let c = test_macros(CATCH_SRC);
        let names: Vec<&str> = c.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["adds \"quoted\"", "Scenario: vectors", "method case", "tpl"]);
        assert_eq!(c[3].filter.1.as_deref(), Some("tpl - *"));

        let d = test_macros(DOCTEST_SRC);
        let got: Vec<(bool, Option<&str>, &str)> = d.iter().map(|m| (m.is_suite, m.suite.as_deref(), m.name.as_str())).collect();
        assert_eq!(
            got,
            vec![(false, None, "free"), (true, Some("math"), "math"), (false, Some("math"), "in suite"), (false, None, "after")]
        );

        let b = test_macros(BOOST_SRC);
        let got: Vec<(bool, Option<&str>, &str)> = b.iter().map(|m| (m.is_suite, m.suite.as_deref(), m.name.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (false, None, "free_case"),
                (true, Some("math"), "math"),
                (true, Some("math/deep"), "math/deep"),
                (false, Some("math/deep"), "adds"),
                (false, Some("math"), "fixed"),
            ]
        );
        assert_eq!(find_definition(BOOST_SRC, Framework::BoostTest, "math", "deep/adds"), Some(5));
        assert_eq!(find_definition(BOOST_SRC, Framework::BoostTest, boost::MASTER, "free_case"), Some(2));
        assert_eq!(find_definition(GTEST_SRC, Framework::GoogleTest, "Inst/Param", "Each/0"), Some(6));
        assert_eq!(find_definition(CATCH_SRC, Framework::Catch2, "tests", "tpl - int"), Some(5));
        assert_eq!(find_definition(DOCTEST_SRC, Framework::Doctest, "math", "in suite"), Some(3));
    }

    #[test]
    fn hostile_sources_never_panic() {
        for text in ["TEST(", "TEST(\"", "#include <gtest/gtest.h>\nTEST(,)\nTEST(a b, c)\nTEST_F(A,", "\u{0}TEST_CASE(\"\\", "}}}}{{"] {
            let _ = test_macros(text);
            let _ = gutter_lines(text);
            let _ = macro_at(text, usize::MAX);
        }
        let big = "#include <gtest/gtest.h>\n".to_string() + &"TEST(A, B) {}\n".repeat(20_000);
        assert_eq!(test_macros(&big).len(), 10_000);
        assert_eq!(secs_ms("0.0015s"), Some(2));
        assert_eq!(secs_ms("NaN"), None);
        assert_eq!(secs_ms("-1"), None);
    }
}
