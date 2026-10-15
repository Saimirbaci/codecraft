//! jest and vitest in the test runner (M13 track RC): which processes run them, their JSON
//! reporter added to the command line (`--json --outputFile` for jest, `--reporter=json
//! --outputFile` for vitest; both write jest's result format), and the report turned into the
//! test tree when the run ends: a suite per test file, a test per `test`/`it` (`describe › test`),
//! with durations, failure messages and locations (jest's `--testLocationInResults`, else the
//! `test("name"` line in the file), and Rerun Failed Tests through `-t`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Framework, SuiteKind, TestCase, TestLocation, TestRun, TestStatus, TestSuite, basename};
use crate::runs::ProcessCommand;

/// Largest report read.
const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;
/// Separator between `describe` titles and the test title in test names.
pub const TITLE_SEPARATOR: &str = " › ";

impl Framework {
    /// jest or vitest.
    pub fn is_js(self) -> bool {
        matches!(self, Self::Jest | Self::Vitest)
    }
}

fn runner(word: &str) -> Option<Framework> {
    match basename(word) {
        "jest" | "jest.js" => Some(Framework::Jest),
        "vitest" | "vitest.mjs" => Some(Framework::Vitest),
        _ => None,
    }
}

/// The runner a `package.json` script calls (`"test": "vitest run"`).
fn script_runner(cwd: &Path, name: &str) -> Option<Framework> {
    let path = cwd.join("package.json");
    let size_ok = std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1024 * 1024);
    if !size_ok {
        return None;
    }
    let pkg: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    let script = pkg.get("scripts")?.get(name)?.as_str()?;
    let words: Vec<&str> = script.split(|c: char| c.is_whitespace() || c == '&' || c == ';').collect();
    if words.iter().any(|w| runner(w) == Some(Framework::Vitest)) {
        Some(Framework::Vitest)
    } else if words.iter().any(|w| runner(w) == Some(Framework::Jest)) {
        Some(Framework::Jest)
    } else {
        None
    }
}

/// How the runner is reached: directly (`npx jest …`), or through a package script whose
/// arguments need `--` first (`npm run test -- …`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    Direct,
    Script { dashes: bool },
}

fn detect(cmd: &ProcessCommand) -> Option<(Framework, Via)> {
    let program = basename(&cmd.program);
    let mut words = cmd.args.iter().map(String::as_str).filter(|w| !w.starts_with('-'));
    if let Some(fw) = runner(program) {
        return Some((fw, Via::Direct));
    }
    match program {
        "npx" | "bunx" | "pnpx" => words.next().and_then(runner).map(|fw| (fw, Via::Direct)),
        "node" => words.next().and_then(runner).map(|fw| (fw, Via::Direct)),
        "npm" | "pnpm" | "yarn" | "bun" => {
            let first = words.next()?;
            match first {
                "exec" | "x" | "dlx" => words.next().and_then(runner).map(|fw| (fw, Via::Direct)),
                "run" | "run-script" => {
                    let script = words.next()?;
                    script_runner(&cmd.cwd, script).map(|fw| (fw, Via::Script { dashes: program == "npm" }))
                }
                "test" | "t" => {
                    script_runner(&cmd.cwd, "test").map(|fw| (fw, Via::Script { dashes: program == "npm" }))
                }
                other => runner(other).map(|fw| (fw, Via::Direct)).or_else(|| {
                    // `yarn test`, `pnpm test`, `bun run`-less scripts.
                    (program != "npm")
                        .then(|| script_runner(&cmd.cwd, other))
                        .flatten()
                        .map(|fw| (fw, Via::Script { dashes: false }))
                }),
            }
        }
        _ => None,
    }
}

/// The JavaScript test runner a process runs, if any.
pub(crate) fn framework_of(cmd: &ProcessCommand) -> Option<Framework> {
    detect(cmd).map(|(fw, _)| fw)
}

/// Append runner arguments, after `--` when a package script passes them on.
fn push_args(out: &mut ProcessCommand, via: Via, extra: Vec<String>) {
    if via == (Via::Script { dashes: true }) && !out.args.iter().any(|a| a == "--") {
        out.args.push("--".into());
    }
    out.args.extend(extra);
}

/// Before the process starts: the runner's JSON report in a temp file. Returns the command and
/// the report path (a report the user asked for already is read instead).
pub(crate) fn instrument(cmd: &ProcessCommand, fw: Framework) -> (ProcessCommand, Option<PathBuf>) {
    let Some((_, via)) = detect(cmd) else { return (cmd.clone(), None) };
    let mut out = cmd.clone();
    if let Some(existing) = cmd.args.iter().find_map(|a| a.strip_prefix("--outputFile=")) {
        let p = PathBuf::from(existing);
        return (out, Some(if p.is_absolute() { p } else { cmd.cwd.join(p) }));
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "codecraft-{}-{}-{}-{seq}.json",
        if fw == Framework::Vitest { "vitest" } else { "jest" },
        std::process::id(),
        super::now_millis()
    ));
    let file = format!("--outputFile={}", path.display());
    let extra = match fw {
        Framework::Vitest => vec!["--reporter=default".into(), "--reporter=json".into(), file],
        _ => vec!["--json".into(), file, "--testLocationInResults".into()],
    };
    push_args(&mut out, via, extra);
    (out, Some(path))
}

fn status(s: &str) -> TestStatus {
    match s {
        "passed" => TestStatus::Passed,
        "failed" => TestStatus::Failed,
        // pending, skipped, todo, disabled.
        _ => TestStatus::Ignored,
    }
}

/// The first stack frame in `file` (`at … (/p/a.test.ts:12:5)`), as a location.
fn failure_location(message: &str, file: &Path) -> Option<TestLocation> {
    let wanted = file.display().to_string();
    for line in message.lines() {
        let Some(at) = line.find(&wanted) else { continue };
        let rest = line.get(at + wanted.len()..)?;
        let mut nums = rest.strip_prefix(':')?.split(|c: char| !c.is_ascii_digit());
        let line_no: usize = nums.next()?.parse().ok()?;
        let col: usize = nums.next().and_then(|c| c.parse().ok()).unwrap_or(1);
        return Some(TestLocation {
            path: file.to_path_buf(),
            line: line_no.saturating_sub(1),
            column: col.saturating_sub(1),
        });
    }
    None
}

/// The line of `test("title"`/`it('title'` in the file text, searching from `from`.
fn title_line(text: &str, title: &str, from: usize) -> Option<usize> {
    let find = |skip: usize| {
        text.lines()
            .enumerate()
            .skip(skip)
            .find(|(_, l)| crate::run_configs::js_test_name(l).is_some_and(|n| n == title))
            .map(|(i, _)| i)
    };
    find(from).or_else(|| find(0))
}

/// Parse a jest/vitest JSON report into suites (paths relative to `cwd` in suite names).
pub fn parse_report(text: &str, cwd: &Path) -> Vec<TestSuite> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let mut out = Vec::new();
    for file in v.get("testResults").and_then(Value::as_array).into_iter().flatten().take(10_000) {
        let path = PathBuf::from(file.get("name").and_then(Value::as_str).unwrap_or(""));
        let path = if path.is_absolute() || path.as_os_str().is_empty() { path } else { cwd.join(path) };
        let shown = path.strip_prefix(cwd).map(Path::to_path_buf).unwrap_or_else(|_| path.clone());
        let mut suite = TestSuite::new(shown.display().to_string(), SuiteKind::File, None, Some(path.clone()));
        let started = file.get("startTime").and_then(Value::as_f64);
        let ended = file.get("endTime").and_then(Value::as_f64);
        if let (Some(a), Some(b)) = (started, ended)
            && b >= a
        {
            suite.duration_ms = Some((b - a) as u64);
        }
        // Read once to find `test("…"` lines when the report has no locations.
        let source = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.len() <= 4 * 1024 * 1024)
            .and_then(|_| std::fs::read_to_string(&path).ok());
        let mut next_line = 0;
        for a in file.get("assertionResults").and_then(Value::as_array).into_iter().flatten().take(50_000) {
            let title = a.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let mut parts: Vec<String> = a
                .get("ancestorTitles")
                .and_then(Value::as_array)
                .map(|t| t.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            parts.push(title.clone());
            let mut t = TestCase::new(parts.join(TITLE_SEPARATOR));
            t.status = status(a.get("status").and_then(Value::as_str).unwrap_or(""));
            t.duration_ms = a.get("duration").and_then(Value::as_f64).filter(|d| *d >= 0.0).map(|d| d.round() as u64);
            let messages: Vec<String> = a
                .get("failureMessages")
                .and_then(Value::as_array)
                .map(|m| m.iter().filter_map(Value::as_str).map(crate::runs::strip_ansi).collect())
                .unwrap_or_default();
            if !messages.is_empty() {
                t.output = messages.join("\n\n");
                t.message = messages
                    .first()
                    .and_then(|m| m.lines().find(|l| !l.trim().is_empty()))
                    .map(|l| l.trim().to_string());
                t.failure = messages.iter().find_map(|m| failure_location(m, &path));
            }
            let reported = a.get("location").and_then(|l| {
                let line = l.get("line").and_then(Value::as_u64)?;
                let column = l.get("column").and_then(Value::as_u64).unwrap_or(1);
                Some((usize::try_from(line).ok()?, usize::try_from(column).unwrap_or(1)))
            });
            t.location = match reported {
                Some((line, column)) => Some(TestLocation {
                    path: path.clone(),
                    line: line.saturating_sub(1),
                    column: column.saturating_sub(1),
                }),
                None => source.as_deref().and_then(|src| title_line(src, &title, next_line)).map(|line| {
                    next_line = line + 1;
                    TestLocation { path: path.clone(), line, column: 0 }
                }),
            };
            suite.tests.push(t);
        }
        // A file that failed to run at all (syntax error, missing module).
        if suite.tests.is_empty() && file.get("status").and_then(Value::as_str) == Some("failed") {
            let message = file
                .get("message")
                .or_else(|| file.get("failureMessage"))
                .and_then(Value::as_str)
                .map(crate::runs::strip_ansi)
                .unwrap_or_default();
            let mut t = TestCase::new("(test file failed to run)");
            t.status = TestStatus::Failed;
            t.message = message.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string());
            t.output = message;
            suite.tests.push(t);
        }
        suite.expected = Some(suite.tests.len());
        suite.finished = true;
        out.push(suite);
    }
    out
}

/// The run ended: its report replaces the tree (jest and vitest have no live tree here).
pub(crate) fn merge_report(run: &mut TestRun, path: &Path) {
    let text = std::fs::metadata(path)
        .ok()
        .filter(|m| m.len() <= MAX_REPORT_BYTES)
        .and_then(|_| std::fs::read(path).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let _ = std::fs::remove_file(path);
    let Some(text) = text else { return };
    let suites = parse_report(&text, &run.cwd);
    if !suites.is_empty() {
        run.suites = suites;
    }
}

/// Rerun Failed Tests: the same command with `-t` matching the failed tests' full names.
pub(crate) fn rerun_failed(cmd: &ProcessCommand, failed: &[(&TestSuite, &TestCase)]) -> Result<ProcessCommand, String> {
    let (_, via) = detect(cmd).ok_or("Rerun Failed Tests needs a jest or vitest command")?;
    let mut out = cmd.clone();
    let mut args = Vec::new();
    let mut it = cmd.args.iter();
    while let Some(a) = it.next() {
        if a == "-t" || a == "--testNamePattern" {
            it.next();
        } else if !a.starts_with("-t=") && !a.starts_with("--testNamePattern=") {
            args.push(a.clone());
        }
    }
    out.args = args;
    let mut names: Vec<String> = Vec::new();
    for (_, t) in failed {
        let full = t.name.replace(TITLE_SEPARATOR, " ");
        let escaped = regex::escape(&full);
        if !names.contains(&escaped) {
            names.push(escaped);
        }
    }
    // The failed files only, so the other files aren't even loaded.
    let files: Vec<String> = failed
        .iter()
        .filter_map(|(s, _)| s.source.as_ref().map(|p| p.display().to_string()))
        .fold(Vec::new(), |mut acc, f| {
            if !acc.contains(&f) {
                acc.push(f);
            }
            acc
        });
    let has_files = out.args.iter().any(|a| {
        let p = Path::new(a);
        p.extension().is_some_and(|e| matches!(e.to_str(), Some("js" | "ts" | "jsx" | "tsx" | "mjs" | "cjs" | "mts")))
    });
    let mut extra = Vec::new();
    if !has_files {
        extra.extend(files);
    }
    extra.push("-t".into());
    extra.push(format!("^(?:{})$", names.join("|")));
    push_args(&mut out, via, extra);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn cmd(program: &str, args: &[&str], cwd: &Path) -> ProcessCommand {
        ProcessCommand::new(program, args, cwd)
    }

    #[test]
    fn detects_runners_and_package_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"scripts":{"test":"vitest run","unit":"jest --ci","dev":"vite"}}"#,
        )
        .unwrap();
        assert_eq!(framework_of(&cmd("npx", &["jest", "a.test.js"], cwd)), Some(Framework::Jest));
        assert_eq!(framework_of(&cmd("pnpm", &["exec", "vitest", "run"], cwd)), Some(Framework::Vitest));
        assert_eq!(framework_of(&cmd("/x/node_modules/.bin/vitest", &[], cwd)), Some(Framework::Vitest));
        assert_eq!(framework_of(&cmd("npm", &["test"], cwd)), Some(Framework::Vitest));
        assert_eq!(framework_of(&cmd("npm", &["run", "unit"], cwd)), Some(Framework::Jest));
        assert_eq!(framework_of(&cmd("yarn", &["unit"], cwd)), Some(Framework::Jest));
        assert_eq!(framework_of(&cmd("npm", &["run", "dev"], cwd)), None);
        assert_eq!(framework_of(&cmd("node", &["server.js"], cwd)), None);
        assert_eq!(framework_of(&cmd("npm", &[], cwd)), None);
    }

    #[test]
    fn instruments_with_a_json_report() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(cwd.join("package.json"), r#"{"scripts":{"test":"jest"}}"#).unwrap();
        let (out, report) = instrument(&cmd("npx", &["jest", "a.test.js"], cwd), Framework::Jest);
        let report = report.unwrap();
        assert!(out.args.contains(&"--json".to_string()));
        assert!(out.args.contains(&format!("--outputFile={}", report.display())));
        let (out, _) = instrument(&cmd("npm", &["test"], cwd), Framework::Jest);
        assert_eq!(out.args.get(1).map(String::as_str), Some("--"), "{:?}", out.args);
        let (out, _) = instrument(&cmd("npx", &["vitest", "run"], cwd), Framework::Vitest);
        assert!(out.args.contains(&"--reporter=json".to_string()));
        let (out, existing) = instrument(&cmd("npx", &["jest", "--json", "--outputFile=r.json"], cwd), Framework::Jest);
        assert_eq!(existing, Some(cwd.join("r.json")));
        assert_eq!(out.args.len(), 3);
    }

    const REPORT: &str = r#"{
      "numFailedTests": 1,
      "testResults": [
        {"name": "%DIR%/math.test.ts", "status": "failed", "startTime": 1000, "endTime": 1250,
         "assertionResults": [
           {"ancestorTitles": ["math"], "title": "adds", "status": "passed", "duration": 3, "failureMessages": [],
            "location": {"line": 3, "column": 5}},
           {"ancestorTitles": ["math"], "title": "subtracts", "status": "failed", "duration": 7.4,
            "failureMessages": ["Error: expect(received).toBe(expected)\n\nExpected: 1\nReceived: 2\n    at Object.<anonymous> (%DIR%/math.test.ts:7:15)"]},
           {"ancestorTitles": [], "title": "later", "status": "todo", "failureMessages": []}
         ]},
        {"name": "%DIR%/broken.test.ts", "status": "failed", "message": "SyntaxError: Unexpected token", "assertionResults": []}
      ]
    }"#;

    #[test]
    fn parses_reports_into_suites() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(
            cwd.join("math.test.ts"),
            "describe('math', () => {\n\n  test('adds', () => {});\n\n  // x\n\n  it(\"subtracts\", () => {\n    expect(2).toBe(1);\n  });\n});\ntest.todo('later');\n",
        )
        .unwrap();
        let text = REPORT.replace("%DIR%", &cwd.display().to_string());
        let suites = parse_report(&text, cwd);
        assert_eq!(suites.len(), 2);
        let math = &suites[0];
        assert_eq!(math.name, "math.test.ts");
        assert_eq!(math.duration_ms, Some(250));
        assert_eq!(math.tests.len(), 3);
        let adds = &math.tests[0];
        assert_eq!((adds.name.as_str(), adds.status, adds.duration_ms), ("math › adds", TestStatus::Passed, Some(3)));
        assert_eq!(adds.location.as_ref().map(|l| (l.line, l.column)), Some((2, 4)));
        let sub = &math.tests[1];
        assert_eq!(sub.status, TestStatus::Failed);
        assert_eq!(sub.message.as_deref(), Some("Error: expect(received).toBe(expected)"));
        assert_eq!(sub.failure.as_ref().map(|f| f.line), Some(6));
        // No location in the report: the `it("subtracts"` line.
        assert_eq!(sub.location.as_ref().map(|l| l.line), Some(6));
        assert_eq!(math.tests[2].status, TestStatus::Ignored);
        assert_eq!(math.tests[2].location.as_ref().map(|l| l.line), Some(10));
        let broken = &suites[1];
        assert_eq!(broken.tests.len(), 1);
        assert_eq!(broken.tests[0].message.as_deref(), Some("SyntaxError: Unexpected token"));
        assert!(parse_report("not json", cwd).is_empty());
        assert!(parse_report(r#"{"testResults": 5}"#, cwd).is_empty());
    }

    #[test]
    fn reruns_failed_tests_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let mut suite = TestSuite::new("a.test.ts", SuiteKind::File, None, Some(cwd.join("a.test.ts")));
        let mut t = TestCase::new("math › sub (x)");
        t.status = TestStatus::Failed;
        suite.tests.push(t);
        let failed: Vec<(&TestSuite, &TestCase)> = suite.tests.iter().map(|t| (&suite, t)).collect();
        let c = rerun_failed(&cmd("npx", &["vitest", "run", "-t", "old"], cwd), &failed).unwrap();
        let tail: Vec<&str> = c.args.iter().map(String::as_str).collect();
        assert_eq!(tail.first().copied(), Some("vitest"));
        assert!(!tail.contains(&"old"));
        assert_eq!(tail.last().copied(), Some(r"^(?:math sub \(x\))$"));
        assert!(tail.contains(&cwd.join("a.test.ts").display().to_string().as_str()));
        assert!(rerun_failed(&cmd("node", &["x.js"], cwd), &failed).is_err());
    }
}
