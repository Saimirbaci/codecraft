//! Test runner (M6, track T): the test tree of each test run (suites → tests with outcome,
//! duration, captured output and source location), rerun (failed), test history and coverage.
//!
//! Test runs are Run tool window tabs. `runs.rs` reports test processes as they start
//! ([`track_cargo`], [`instrument_process`] + [`track_process`]); [`pump`] parses each tab's new
//! console lines, and when the process ends merges a JUnit report (pytest), resolves test
//! locations on a worker thread, records the run in `<project>/.codecraft/test-history.json` and
//! loads the coverage report of a "Run with Coverage" run.

mod junit;
mod lcov;
mod locate;
mod parse;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::json;

use codecraft_cargo::{CargoCommand, CargoInvocation, TargetSelector};
use codecraft_workspace::{ConsoleKind, NotificationLevel, RunStatus, RunTabId, ToolWindowId};

pub use lcov::{CoverageReport, CoverageRow, FileCoverage, percent as coverage_percent};

use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

/// Runs kept in the history file.
pub const HISTORY_LIMIT: usize = 20;
/// Test runs kept in memory (their tabs may be closed meanwhile).
const MAX_RUNS: usize = 30;
/// Captured output kept per test in the history file.
const HISTORY_OUTPUT: usize = 16 * 1024;
/// Largest coverage report read.
const MAX_LCOV_BYTES: u64 = 256 * 1024 * 1024;

pub const LLVM_COV_HINT: &str = "Install cargo-llvm-cov to collect Rust coverage: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview";
pub const PYTEST_COV_HINT: &str = "Install pytest-cov to collect Python coverage: pip install pytest-cov";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TestStatus {
    Running,
    Passed,
    Failed,
    Ignored,
}

/// Which output format a run speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Framework {
    Cargo,
    Pytest,
    CTest,
    Zig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuiteKind {
    /// `cargo test` unit tests of a crate root (`unittests src/lib.rs`).
    Unit,
    /// An integration test target (`tests/x.rs`).
    Integration,
    Doc,
    Bench,
    /// A test file (pytest, Zig).
    File,
    Other,
}

/// A source position (0-based line and char column).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestLocation {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCase {
    /// Name within the suite (`tests::adds`, `TestGroup::test_x[1]`, `src/lib.rs - add (line 6)`).
    pub name: String,
    pub status: TestStatus,
    pub duration_ms: Option<u64>,
    /// Captured output (libtest's `---- name stdout ----` section, pytest's report, …).
    pub output: String,
    /// Where the test is defined.
    pub location: Option<TestLocation>,
    /// Where it failed (panic location, assertion line).
    pub failure: Option<TestLocation>,
    /// Failure message, or the ignore reason.
    pub message: Option<String>,
}

impl TestCase {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: TestStatus::Running,
            duration_ms: None,
            output: String::new(),
            location: None,
            failure: None,
            message: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSuite {
    pub name: String,
    pub kind: SuiteKind,
    /// The test binary's crate (cargo).
    pub crate_name: Option<String>,
    /// The suite's source as printed (`src/lib.rs`, `tests/test_math.py`).
    pub source: Option<PathBuf>,
    pub tests: Vec<TestCase>,
    /// `running N tests`.
    pub expected: Option<usize>,
    pub finished: bool,
    pub duration_ms: Option<u64>,
}

impl TestSuite {
    pub fn new(name: impl Into<String>, kind: SuiteKind, crate_name: Option<String>, source: Option<PathBuf>) -> Self {
        Self {
            name: name.into(),
            kind,
            crate_name,
            source,
            tests: Vec::new(),
            expected: None,
            finished: false,
            duration_ms: None,
        }
    }
}

/// How to run a test run again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestOrigin {
    Cargo(CargoInvocation),
    Process(ProcessCommand),
}

/// Pass/fail/ignore/running counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub running: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.passed + self.failed + self.ignored + self.running
    }
}

/// Tree view state of one run (engine-owned so agents can drive it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeView {
    /// Collapsed node keys (everything is expanded by default).
    pub collapsed: BTreeSet<String>,
    /// Selected node key (see [`TreeRow::key`]).
    pub selected: Option<String>,
}

/// One test run: a Run tool window tab running tests, or an imported/historic result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestRun {
    pub tab: RunTabId,
    pub title: String,
    pub command_line: String,
    pub framework: Framework,
    pub origin: Option<TestOrigin>,
    /// Base for relative paths (the process working directory).
    pub cwd: PathBuf,
    pub suites: Vec<TestSuite>,
    pub started_unix: u64,
    pub duration_ms: Option<u64>,
    pub finished: bool,
    /// Loaded from history or a file (no process).
    #[serde(default)]
    pub imported: bool,
    #[serde(skip)]
    pub view: TreeView,
    #[serde(skip)]
    junit: Option<PathBuf>,
    #[serde(skip)]
    parser: parse::Parser,
    /// Console lines consumed (absolute index, counting dropped lines).
    #[serde(skip)]
    cursor: usize,
}

impl TestRun {
    fn new(
        tab: RunTabId,
        title: &str,
        command_line: String,
        framework: Framework,
        origin: Option<TestOrigin>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            tab,
            title: title.to_string(),
            command_line,
            framework,
            origin,
            cwd,
            suites: Vec::new(),
            started_unix: now_unix(),
            duration_ms: None,
            finished: false,
            imported: false,
            view: TreeView::default(),
            junit: None,
            parser: parse::Parser::default(),
            cursor: 0,
        }
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for t in self.suites.iter().flat_map(|s| &s.tests) {
            match t.status {
                TestStatus::Passed => c.passed += 1,
                TestStatus::Failed => c.failed += 1,
                TestStatus::Ignored => c.ignored += 1,
                TestStatus::Running => c.running += 1,
            }
        }
        c
    }

    pub fn test(&self, suite: usize, test: usize) -> Option<&TestCase> {
        self.suites.get(suite).and_then(|s| s.tests.get(test))
    }

    /// The overall status: running while the process runs, failed if any test failed.
    pub fn status(&self) -> TestStatus {
        let c = self.counts();
        if !self.finished {
            TestStatus::Running
        } else if c.failed > 0 {
            TestStatus::Failed
        } else if c.passed > 0 || c.total() == 0 {
            TestStatus::Passed
        } else {
            TestStatus::Ignored
        }
    }

    /// Whether names of this run's suite split into module nodes at `::`.
    fn splits(&self, suite: &TestSuite) -> bool {
        matches!(self.framework, Framework::Cargo | Framework::Pytest) && suite.kind != SuiteKind::Doc
    }
}

// ---------------------------------------------------------------- tree rows

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowKind {
    Root,
    Suite,
    Module,
    Test,
}

/// One visible row of the test tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeRow {
    /// `root`, `s<suite>`, `s<suite>:<module path>`, `s<suite>#<test>`.
    pub key: String,
    pub depth: usize,
    pub label: String,
    pub kind: RowKind,
    pub status: TestStatus,
    pub duration_ms: Option<u64>,
    pub has_children: bool,
    pub expanded: bool,
    pub suite: Option<usize>,
    pub test: Option<usize>,
}

/// Filters and order of the tree (the runner toolbar toggles).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeOptions {
    pub show_passed: bool,
    pub show_ignored: bool,
    pub sort_by_duration: bool,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self { show_passed: true, show_ignored: true, sort_by_duration: false }
    }
}

#[derive(Default)]
struct Node {
    children: Vec<(String, Node)>,
    tests: Vec<usize>,
}

fn aggregate(statuses: impl Iterator<Item = TestStatus>) -> Option<TestStatus> {
    let (mut any, mut failed, mut running, mut passed) = (false, false, false, false);
    for s in statuses {
        any = true;
        match s {
            TestStatus::Failed => failed = true,
            TestStatus::Running => running = true,
            TestStatus::Passed => passed = true,
            TestStatus::Ignored => {}
        }
    }
    if !any {
        None
    } else if failed {
        Some(TestStatus::Failed)
    } else if running {
        Some(TestStatus::Running)
    } else if passed {
        Some(TestStatus::Passed)
    } else {
        Some(TestStatus::Ignored)
    }
}

fn sum_durations(tests: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut total: Option<u64> = None;
    for d in tests.flatten() {
        total = Some(total.unwrap_or(0).saturating_add(d));
    }
    total
}

/// Visible rows of `run`'s tree under `opts`, honouring collapsed nodes when `respect_collapse`.
pub fn tree_rows(run: &TestRun, opts: TreeOptions, respect_collapse: bool) -> Vec<TreeRow> {
    let visible = |t: &TestCase| match t.status {
        TestStatus::Passed => opts.show_passed,
        TestStatus::Ignored => opts.show_ignored,
        _ => true,
    };
    let collapsed = |key: &str| respect_collapse && run.view.collapsed.contains(key);
    let all = run.suites.iter().flat_map(|s| &s.tests);
    let root_status =
        if run.finished { aggregate(all.map(|t| t.status)).unwrap_or(TestStatus::Passed) } else { TestStatus::Running };
    let mut rows = vec![TreeRow {
        key: "root".into(),
        depth: 0,
        label: "Test Results".into(),
        kind: RowKind::Root,
        status: root_status,
        duration_ms: run.duration_ms,
        has_children: !run.suites.is_empty(),
        expanded: !collapsed("root"),
        suite: None,
        test: None,
    }];
    if collapsed("root") {
        return rows;
    }
    for (si, suite) in run.suites.iter().enumerate() {
        let shown: Vec<usize> = (0..suite.tests.len()).filter(|i| suite.tests.get(*i).is_some_and(visible)).collect();
        if shown.is_empty() && (suite.finished || !suite.tests.is_empty()) {
            continue;
        }
        // Module tree.
        let mut root = Node::default();
        for &ti in &shown {
            let Some(t) = suite.tests.get(ti) else { continue };
            let mut node = &mut root;
            if run.splits(suite) {
                let mut parts: Vec<&str> = t.name.split("::").collect();
                parts.pop();
                for p in parts {
                    let idx = match node.children.iter().position(|(n, _)| n == p) {
                        Some(i) => i,
                        None => {
                            node.children.push((p.to_string(), Node::default()));
                            node.children.len() - 1
                        }
                    };
                    let Some((_, child)) = node.children.get_mut(idx) else { break };
                    node = child;
                }
            }
            node.tests.push(ti);
        }
        let status = aggregate(shown.iter().filter_map(|i| suite.tests.get(*i)).map(|t| t.status))
            .map(|s| if !suite.finished && !run.finished && s != TestStatus::Failed { TestStatus::Running } else { s })
            .unwrap_or(if suite.finished { TestStatus::Passed } else { TestStatus::Running });
        let key = format!("s{si}");
        let expanded = !collapsed(&key);
        rows.push(TreeRow {
            key: key.clone(),
            depth: 1,
            label: suite.name.clone(),
            kind: RowKind::Suite,
            status,
            duration_ms: suite.duration_ms.or_else(|| sum_durations(suite.tests.iter().map(|t| t.duration_ms))),
            has_children: !shown.is_empty(),
            expanded,
            suite: Some(si),
            test: None,
        });
        if expanded {
            emit_node(run, opts, &collapsed, suite, si, &root, "", 2, &mut rows);
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn emit_node(
    run: &TestRun,
    opts: TreeOptions,
    collapsed: &dyn Fn(&str) -> bool,
    suite: &TestSuite,
    si: usize,
    node: &Node,
    path: &str,
    depth: usize,
    rows: &mut Vec<TreeRow>,
) {
    fn node_tests(n: &Node, out: &mut Vec<usize>) {
        out.extend(&n.tests);
        for (_, c) in &n.children {
            node_tests(c, out);
        }
    }
    let leaf = |t: &TestCase| t.name.rsplit("::").next().unwrap_or(&t.name).to_string();
    let mut children: Vec<(&String, &Node, Vec<usize>)> = node
        .children
        .iter()
        .map(|(n, c)| {
            let mut ts = Vec::new();
            node_tests(c, &mut ts);
            (n, c, ts)
        })
        .collect();
    let dur =
        |ts: &[usize]| sum_durations(ts.iter().filter_map(|i| suite.tests.get(*i)).map(|t| t.duration_ms)).unwrap_or(0);
    if opts.sort_by_duration {
        children.sort_by(|a, b| dur(&b.2).cmp(&dur(&a.2)).then_with(|| a.0.cmp(b.0)));
    } else {
        children.sort_by(|a, b| a.0.cmp(b.0));
    }
    for (name, child, ts) in children {
        let full = if path.is_empty() { name.clone() } else { format!("{path}::{name}") };
        let key = format!("s{si}:{full}");
        let expanded = !collapsed(&key);
        rows.push(TreeRow {
            key,
            depth,
            label: name.clone(),
            kind: RowKind::Module,
            status: aggregate(ts.iter().filter_map(|i| suite.tests.get(*i)).map(|t| t.status))
                .unwrap_or(TestStatus::Passed),
            duration_ms: sum_durations(ts.iter().filter_map(|i| suite.tests.get(*i)).map(|t| t.duration_ms)),
            has_children: true,
            expanded,
            suite: Some(si),
            test: None,
        });
        if expanded {
            emit_node(run, opts, collapsed, suite, si, child, &full, depth + 1, rows);
        }
    }
    let mut tests: Vec<usize> = node.tests.clone();
    if opts.sort_by_duration {
        tests.sort_by_key(|i| std::cmp::Reverse(suite.tests.get(*i).and_then(|t| t.duration_ms).unwrap_or(0)));
    } else if run.splits(suite) || suite.kind == SuiteKind::Doc {
        tests.sort_by(|a, b| {
            let name = |i: &usize| suite.tests.get(*i).map(leaf).unwrap_or_default();
            name(a).cmp(&name(b))
        });
    }
    for ti in tests {
        let Some(t) = suite.tests.get(ti) else { continue };
        rows.push(TreeRow {
            key: format!("s{si}#{ti}"),
            depth,
            label: if run.splits(suite) { leaf(t) } else { t.name.clone() },
            kind: RowKind::Test,
            status: t.status,
            duration_ms: t.duration_ms,
            has_children: false,
            expanded: false,
            suite: Some(si),
            test: Some(ti),
        });
    }
}

/// Parse a test key `s<suite>#<test>`.
pub fn test_key(key: &str) -> Option<(usize, usize)> {
    let (s, t) = key.strip_prefix('s')?.split_once('#')?;
    Some((s.parse().ok()?, t.parse().ok()?))
}

/// Keys of the ancestors of a node (`root`, `s0`, `s0:a`, `s0:a::b`).
fn ancestors(run: &TestRun, key: &str) -> Vec<String> {
    let mut out = vec!["root".to_string()];
    let (si, module) = if let Some((si, ti)) = test_key(key) {
        let module = run.test(si, ti).filter(|_| run.suites.get(si).is_some_and(|s| run.splits(s))).map(|t| {
            let mut parts: Vec<&str> = t.name.split("::").collect();
            parts.pop();
            parts.join("::")
        });
        (Some(si), module)
    } else if let Some(rest) = key.strip_prefix('s') {
        let (si, module) = match rest.split_once(':') {
            Some((si, m)) => (si.parse().ok(), Some(m.to_string())),
            None => (rest.parse().ok(), None),
        };
        (si, module.map(|m| m.rsplit_once("::").map(|(p, _)| p.to_string()).unwrap_or_default()))
    } else {
        (None, None)
    };
    if let Some(si) = si {
        if key != format!("s{si}") {
            out.push(format!("s{si}"));
        }
        if let Some(m) = module.filter(|m| !m.is_empty()) {
            let mut acc = String::new();
            for part in m.split("::") {
                if !acc.is_empty() {
                    acc.push_str("::");
                }
                acc.push_str(part);
                out.push(format!("s{si}:{acc}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- state

/// An in-flight location job.
struct LocateJob {
    tab: RunTabId,
    rx: Receiver<Vec<locate::Located>>,
}

/// A "Run with Coverage" process whose report is read when it ends.
struct CoverageRun {
    tab: RunTabId,
    lcov: PathBuf,
    base: PathBuf,
    name: String,
    command_line: String,
    python: bool,
}

/// A coverage report being read and stored after its run.
struct CoverageLoad {
    rx: Receiver<Result<(CoverageReport, ReportMeta), String>>,
}

/// A stored coverage report (`Manage Coverage Reports`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportMeta {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    pub command_line: String,
    pub lines_hit: usize,
    pub lines_found: usize,
}

/// An entry of the test history list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySummary {
    pub index: usize,
    pub title: String,
    pub command_line: String,
    pub started_unix: u64,
    pub duration_ms: Option<u64>,
    pub counts: Counts,
}

pub struct TestingState {
    pub runs: Vec<TestRun>,
    pub options: TreeOptions,
    /// The Test History dialog is open.
    pub history_open: bool,
    /// The Manage Coverage Reports dialog is open.
    pub reports_open: bool,
    /// `Import Tests from File…` without a path: the shell asks for one.
    pub import_requested: bool,
    /// The coverage report shown in the Coverage tool window.
    pub coverage: Option<CoverageReport>,
    /// Coverage marks in the editor gutter.
    pub coverage_in_editor: bool,
    /// Path → (0-based line → last status) of located tests, newest run last.
    gutter: HashMap<PathBuf, BTreeMap<usize, TestStatus>>,
    locate_jobs: Vec<LocateJob>,
    coverage_runs: Vec<CoverageRun>,
    coverage_loads: Vec<CoverageLoad>,
    /// Navigate to this test once its location is known.
    pending_nav: Option<(RunTabId, usize, usize)>,
}

impl Default for TestingState {
    fn default() -> Self {
        Self {
            runs: Vec::new(),
            options: TreeOptions::default(),
            history_open: false,
            reports_open: false,
            import_requested: false,
            coverage: None,
            coverage_in_editor: true,
            gutter: HashMap::new(),
            locate_jobs: Vec::new(),
            coverage_runs: Vec::new(),
            coverage_loads: Vec::new(),
            pending_nav: None,
        }
    }
}

impl TestingState {
    pub fn run(&self, tab: RunTabId) -> Option<&TestRun> {
        self.runs.iter().find(|r| r.tab == tab)
    }

    pub fn run_mut(&mut self, tab: RunTabId) -> Option<&mut TestRun> {
        self.runs.iter_mut().find(|r| r.tab == tab)
    }

    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        !self.locate_jobs.is_empty() || !self.coverage_loads.is_empty()
    }

    /// Last test statuses by line in `path` (for gutter run icons).
    pub fn gutter_tests(&self, path: &Path) -> Option<&BTreeMap<usize, TestStatus>> {
        if self.gutter.is_empty() {
            return None;
        }
        self.gutter
            .get(path)
            .or_else(|| self.gutter.get(&crate::normalize_path(path)))
            .or_else(|| std::fs::canonicalize(path).ok().and_then(|c| self.gutter.get(&c)))
    }

    /// Coverage of `path` when gutter coverage marks are on.
    pub fn editor_coverage(&self, path: &Path) -> Option<&FileCoverage> {
        self.coverage.as_ref().filter(|_| self.coverage_in_editor).and_then(|c| c.file(path))
    }

    fn rebuild_gutter(&mut self) {
        self.gutter.clear();
        for run in &self.runs {
            for t in run.suites.iter().flat_map(|s| &s.tests) {
                if let Some(loc) = &t.location {
                    // Under the canonical path too: Cargo reports canonical paths, editors may not.
                    let canonical = std::fs::canonicalize(&loc.path).ok();
                    for key in std::iter::once(crate::normalize_path(&loc.path)).chain(canonical) {
                        self.gutter.entry(key).or_default().insert(loc.line, t.status);
                    }
                }
            }
        }
    }

    fn add_run(&mut self, run: TestRun) {
        self.runs.retain(|r| r.tab != run.tab);
        self.runs.push(run);
        while self.runs.len() > MAX_RUNS {
            self.runs.remove(0);
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn now_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

// ---------------------------------------------------------------- hooks from runs.rs

/// A cargo invocation started in `tab`: track it when it runs tests.
pub(crate) fn track_cargo(s: &mut Session, tab: RunTabId, inv: &CargoInvocation, title: &str) {
    if !matches!(inv.command, CargoCommand::Test | CargoCommand::Bench) {
        return;
    }
    let run = TestRun::new(
        tab,
        title,
        inv.command_line(),
        Framework::Cargo,
        Some(TestOrigin::Cargo(inv.clone())),
        inv.cwd.clone(),
    );
    s.testing.add_run(run);
}

fn basename(program: &str) -> &str {
    Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program)
}

/// The test framework a process runs, if any.
pub(crate) fn process_framework(cmd: &ProcessCommand) -> Option<Framework> {
    let program = basename(&cmd.program);
    let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
    if program == "sh" || program == "bash" || program == "zsh" {
        let script = args.last().copied().unwrap_or("");
        return if script.contains("pytest") {
            Some(Framework::Pytest)
        } else if script.contains("ctest") {
            Some(Framework::CTest)
        } else if script.contains("cargo test") || script.contains("cargo llvm-cov") || script.contains("cargo nextest")
        {
            Some(Framework::Cargo)
        } else if script.contains("zig build test") || script.contains("zig test") {
            Some(Framework::Zig)
        } else {
            None
        };
    }
    match program {
        "pytest" | "py.test" => Some(Framework::Pytest),
        "ctest" => Some(Framework::CTest),
        "zig" if args.first() == Some(&"test") || args.get(..2) == Some(&["build", "test"][..]) => Some(Framework::Zig),
        "cargo" => {
            let sub = args.iter().find(|a| !a.starts_with('-') && !a.starts_with('+')).copied();
            match sub {
                Some("test") | Some("bench") => Some(Framework::Cargo),
                Some("llvm-cov") if args.get(1).is_none_or(|a| *a != "run") => Some(Framework::Cargo),
                _ => None,
            }
        }
        _ if args.windows(2).any(|w| w == ["-m", "pytest"]) => Some(Framework::Pytest),
        _ => None,
    }
}

/// Before a process starts: pytest gets `-v` (live results) and a JUnit report in a temp file
/// (durations, output per test). Returns the command to run and the report path.
pub(crate) fn instrument_process(cmd: &ProcessCommand) -> (ProcessCommand, Option<PathBuf>) {
    let program = basename(&cmd.program);
    let direct = !matches!(program, "sh" | "bash" | "zsh");
    if !direct || process_framework(cmd) != Some(Framework::Pytest) {
        return (cmd.clone(), None);
    }
    let mut out = cmd.clone();
    if let Some(existing) = cmd.args.iter().find_map(|a| a.strip_prefix("--junitxml=")) {
        return (out, Some(cmd.cwd.join(existing)));
    }
    let verbosity = cmd.args.iter().any(|a| {
        a == "--verbose" || a == "--quiet" || (a.starts_with("-v") || a.starts_with("-q")) && !a.starts_with("--")
    });
    if !verbosity {
        out.args.push("-v".into());
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codecraft-junit-{}-{}-{seq}.xml", std::process::id(), now_millis()));
    out.args.push("-o".into());
    out.args.push("junit_logging=all".into());
    out.args.push(format!("--junitxml={}", path.display()));
    (out, Some(path))
}

/// A process started in `tab` (`cmd` as configured, before [`instrument_process`]).
pub(crate) fn track_process(s: &mut Session, tab: RunTabId, cmd: &ProcessCommand, junit: Option<PathBuf>, title: &str) {
    let Some(framework) = process_framework(cmd) else { return };
    let command_line = s.ws.run(tab).map(|t| t.command_line.clone()).unwrap_or_else(|| cmd.command_line());
    let mut run =
        TestRun::new(tab, title, command_line, framework, Some(TestOrigin::Process(cmd.clone())), cmd.cwd.clone());
    run.junit = junit;
    s.testing.add_run(run);
}

// ---------------------------------------------------------------- pump

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let mut finished: Vec<RunTabId> = Vec::new();
    let mut gone: Vec<RunTabId> = Vec::new();
    for run in s.testing.runs.iter_mut().filter(|r| !r.imported && !r.finished) {
        let Some(tab) = s.ws.run(run.tab) else {
            gone.push(run.tab);
            continue;
        };
        let first = tab.dropped_lines;
        let end = first + tab.lines.len();
        if run.cursor < end {
            let from = run.cursor.saturating_sub(first);
            for line in tab.lines.get(from..).unwrap_or(&[]) {
                if line.kind != ConsoleKind::System {
                    run.parser.feed(run.framework, &mut run.suites, &line.text);
                }
            }
            run.cursor = end;
            changed = true;
        }
        if !tab.is_running() {
            run.parser.finish(&mut run.suites);
            run.duration_ms = tab.duration.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            run.finished = true;
            finished.push(run.tab);
            changed = true;
        }
    }
    // Runs whose tab was closed while running: stop parsing them.
    for tab in gone {
        if let Some(run) = s.testing.run_mut(tab) {
            run.parser.finish(&mut run.suites);
            run.finished = true;
        }
    }
    for tab in finished {
        on_finished(s, tab);
    }
    changed |= pump_locate(s);
    changed |= pump_coverage(s);
    changed
}

fn on_finished(s: &mut Session, tab: RunTabId) {
    // pytest's JUnit report.
    if let Some(run) = s.testing.run_mut(tab)
        && let Some(path) = run.junit.take()
    {
        let size_ok = std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_LCOV_BYTES);
        if size_ok && let Ok(xml) = std::fs::read_to_string(&path) {
            let cwd = run.cwd.clone();
            junit::merge(&mut run.suites, junit::parse(&xml), &cwd);
        }
        let _ = std::fs::remove_file(&path);
    }
    let Some(run) = s.testing.run(tab) else { return };
    let c = run.counts();
    if c.total() > 0 {
        s.status = format!(
            "{}: {} passed, {} failed{}",
            run.title,
            c.passed,
            c.failed,
            if c.ignored > 0 { format!(", {} ignored", c.ignored) } else { String::new() }
        );
        if let Some(root) = s.project_root() {
            let entry = run.clone();
            if let Err(e) = append_history(&root, entry) {
                log::warn!("test history: {e}");
            }
        }
    }
    // Like RustRover: select the first failed test when the run ends.
    if c.failed > 0 && s.testing.run(tab).is_some_and(|r| r.view.selected.is_none()) {
        let _ = step_failed(s, tab, true);
    }
    start_locate(s, tab);
}

/// Resolve test definitions and failure locations of `tab`'s run on a worker thread.
fn start_locate(s: &mut Session, tab: RunTabId) {
    let Some(run) = s.testing.run(tab) else { return };
    let mut requests = Vec::new();
    for (si, suite) in run.suites.iter().enumerate() {
        for (ti, t) in suite.tests.iter().enumerate() {
            let failure_unresolved = t.failure.as_ref().is_some_and(|f| !f.path.is_absolute() || !f.path.is_file());
            if t.location.is_none() || failure_unresolved {
                requests.push(locate::Request {
                    suite: si,
                    test: ti,
                    name: t.name.clone(),
                    suite_kind: suite.kind,
                    suite_source: suite.source.clone(),
                    crate_name: suite.crate_name.clone(),
                    failure: t.failure.clone(),
                });
            }
        }
    }
    if requests.is_empty() {
        s.testing.rebuild_gutter();
        return;
    }
    let mut bases = vec![run.cwd.clone()];
    let mut targets = Vec::new();
    if let Some(project) = s.ws.project.as_ref() {
        if let Some(meta) = project.cargo.as_ref() {
            bases.push(meta.workspace_root.clone());
            for p in meta.workspace_packages() {
                let package_dir = p.manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
                for t in &p.targets {
                    targets.push(locate::TargetInfo {
                        crate_name: t.name.replace('-', "_"),
                        src_path: t.src_path.clone(),
                        package_dir: package_dir.clone(),
                        is_lib: t.is_lib(),
                    });
                }
            }
        }
        bases.push(project.root.clone());
    }
    let zig_file = match &run.origin {
        Some(TestOrigin::Process(cmd))
            if basename(&cmd.program) == "zig" && cmd.args.first().is_some_and(|a| a == "test") =>
        {
            cmd.args.get(1).map(|f| run.cwd.join(f))
        }
        _ => None,
    };
    let job = locate::Job { framework: run.framework, bases, targets, zig_file, requests };
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-test-locate".into()).spawn(move || {
        let _ = tx.send(locate::run(job));
    });
    match spawned {
        Ok(_) => s.testing.locate_jobs.push(LocateJob { tab, rx }),
        Err(e) => log::warn!("test locate thread: {e}"),
    }
}

fn pump_locate(s: &mut Session) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < s.testing.locate_jobs.len() {
        let Some(job) = s.testing.locate_jobs.get(i) else { break };
        let result = match job.rx.try_recv() {
            Ok(r) => Some(r),
            Err(crossbeam_channel::TryRecvError::Empty) => {
                i += 1;
                continue;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => None,
        };
        let tab = job.tab;
        s.testing.locate_jobs.remove(i);
        changed = true;
        if let Some(results) = result
            && let Some(run) = s.testing.run_mut(tab)
        {
            for (si, ti, name, def, failure) in results {
                let Some(t) = run.suites.get_mut(si).and_then(|s| s.tests.get_mut(ti)).filter(|t| t.name == name)
                else {
                    continue;
                };
                if t.location.is_none() {
                    t.location = def;
                }
                if failure.is_some() || t.failure.as_ref().is_some_and(|f| !f.path.is_absolute()) {
                    t.failure = failure;
                }
            }
        }
        s.testing.rebuild_gutter();
        if let Some((ptab, si, ti)) = s.testing.pending_nav
            && ptab == tab
        {
            s.testing.pending_nav = None;
            if let Err(e) = navigate_to(s, tab, si, ti) {
                s.status = e.to_string();
            }
        }
    }
    changed
}

// ---------------------------------------------------------------- commands' logic

/// The run commands act on: `tab` if given, else the active Run tab if it is a test run, else
/// the most recent test run.
pub(crate) fn target_run(s: &Session, tab: Option<u64>) -> Result<RunTabId, CommandError> {
    if let Some(t) = tab {
        let id = RunTabId(t);
        return s
            .testing
            .run(id)
            .map(|r| r.tab)
            .ok_or_else(|| CommandError::BadParams(format!("tab {t} is not a test run")));
    }
    if let Some(active) = s.ws.active_run
        && s.testing.run(active).is_some()
    {
        return Ok(active);
    }
    s.testing.runs.last().map(|r| r.tab).ok_or_else(|| CommandError::Disabled("no test run".into()))
}

/// The run as JSON: summary, visible rows and the selected node.
pub(crate) fn run_json(s: &Session, tab: RunTabId) -> serde_json::Value {
    let Some(run) = s.testing.run(tab) else { return serde_json::Value::Null };
    let rows = tree_rows(run, s.testing.options, true);
    json!({
        "tab": tab.0,
        "title": run.title,
        "commandLine": run.command_line,
        "framework": run.framework,
        "finished": run.finished,
        "status": run.status(),
        "counts": run.counts(),
        "durationMs": run.duration_ms,
        "options": s.testing.options,
        "selected": run.view.selected,
        "rows": rows,
        "suites": run.suites,
    })
}

/// Select a node, expanding its ancestors.
pub(crate) fn select(s: &mut Session, tab: RunTabId, key: &str) -> Result<(), CommandError> {
    let opts = TreeOptions { show_passed: true, show_ignored: true, sort_by_duration: false };
    let run = s.testing.run_mut(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    if !tree_rows(run, opts, false).iter().any(|r| r.key == key) {
        return Err(CommandError::BadParams(format!("no test tree node `{key}`")));
    }
    for a in ancestors(run, key) {
        run.view.collapsed.remove(&a);
    }
    run.view.selected = Some(key.to_string());
    Ok(())
}

/// Select the next (or previous) failed test after the selection, wrapping around.
pub(crate) fn step_failed(s: &mut Session, tab: RunTabId, forward: bool) -> Result<Option<String>, CommandError> {
    let run = s.testing.run(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    let opts =
        TreeOptions { show_passed: true, show_ignored: true, sort_by_duration: s.testing.options.sort_by_duration };
    let order: Vec<String> = tree_rows(run, opts, false)
        .into_iter()
        .filter(|r| r.kind == RowKind::Test && r.status == TestStatus::Failed)
        .map(|r| r.key)
        .collect();
    if order.is_empty() {
        s.status = "No failed tests".into();
        return Ok(None);
    }
    let all: Vec<String> = tree_rows(run, opts, false).into_iter().map(|r| r.key).collect();
    let cur = run.view.selected.as_ref().and_then(|k| all.iter().position(|x| x == k));
    let pos_of = |k: &String| all.iter().position(|x| x == k).unwrap_or(0);
    let next = match (cur, forward) {
        (Some(c), true) => order.iter().find(|k| pos_of(k) > c).or(order.first()),
        (Some(c), false) => order.iter().rev().find(|k| pos_of(k) < c).or(order.last()),
        (None, true) => order.first(),
        (None, false) => order.last(),
    }
    .cloned();
    if let Some(k) = &next {
        select(s, tab, k)?;
    }
    Ok(next)
}

pub(crate) fn set_all_collapsed(s: &mut Session, tab: RunTabId, collapse: bool) -> Result<(), CommandError> {
    let opts = TreeOptions::default();
    let run = s.testing.run_mut(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    if collapse {
        let keys: Vec<String> = tree_rows(run, opts, false)
            .into_iter()
            .filter(|r| matches!(r.kind, RowKind::Suite | RowKind::Module))
            .map(|r| r.key)
            .collect();
        run.view.collapsed = keys.into_iter().collect();
    } else {
        run.view.collapsed.clear();
    }
    Ok(())
}

/// Text shown for a selected node: a test's output (with its message), or the outputs of the
/// failed tests below a suite/module. `None` for the root (the shell shows the console).
pub fn node_output(run: &TestRun, key: &str) -> Option<String> {
    if key == "root" {
        return None;
    }
    let describe = |t: &TestCase| {
        let mut text = String::new();
        if !t.output.is_empty() {
            text.push_str(&t.output);
        }
        if text.is_empty() {
            text = match (t.status, &t.message) {
                (TestStatus::Ignored, Some(m)) => format!("Test ignored: {m}\n"),
                (TestStatus::Ignored, None) => "Test ignored\n".into(),
                (TestStatus::Passed, _) => "Test passed\n".into(),
                (TestStatus::Running, _) => "Test is running…\n".into(),
                (TestStatus::Failed, Some(m)) => format!("{m}\n"),
                (TestStatus::Failed, None) => "Test failed\n".into(),
            };
        }
        text
    };
    if let Some((si, ti)) = test_key(key) {
        return run.test(si, ti).map(describe);
    }
    let rest = key.strip_prefix('s')?;
    let (si, module) = match rest.split_once(':') {
        Some((si, m)) => (si.parse::<usize>().ok()?, Some(format!("{m}::"))),
        None => (rest.parse::<usize>().ok()?, None),
    };
    let suite = run.suites.get(si)?;
    let mut out = String::new();
    for t in suite.tests.iter().filter(|t| module.as_deref().is_none_or(|m| t.name.starts_with(m))) {
        if t.status == TestStatus::Failed {
            out.push_str(&format!("---- {} ----\n{}\n", t.name, describe(t)));
        }
    }
    if out.is_empty() {
        let c = suite.tests.iter().filter(|t| module.as_deref().is_none_or(|m| t.name.starts_with(m))).count();
        out = format!("{c} test(s), no failures\n");
    }
    Some(out)
}

/// Open the definition (else the failure point) of a test; when the location is not known yet,
/// resolve it in the background and navigate when it arrives.
pub(crate) fn navigate(
    s: &mut Session,
    tab: RunTabId,
    si: usize,
    ti: usize,
) -> Result<serde_json::Value, CommandError> {
    let run = s.testing.run(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    let t = run.test(si, ti).ok_or_else(|| CommandError::BadParams("no such test".into()))?;
    let known = t.location.is_some() || t.failure.as_ref().is_some_and(|f| f.path.is_absolute());
    if known {
        navigate_to(s, tab, si, ti)?;
        return Ok(json!({ "navigated": true }));
    }
    let pending = s.testing.locate_jobs.iter().any(|j| j.tab == tab);
    s.testing.pending_nav = Some((tab, si, ti));
    if !pending {
        start_locate(s, tab);
    }
    s.status = "Locating test…".into();
    Ok(json!({ "pending": true }))
}

fn navigate_to(s: &mut Session, tab: RunTabId, si: usize, ti: usize) -> Result<(), CommandError> {
    let t = s
        .testing
        .run(tab)
        .and_then(|r| r.test(si, ti))
        .ok_or_else(|| CommandError::Failed("the test is gone".into()))?;
    let loc = t
        .location
        .clone()
        .or_else(|| t.failure.clone().filter(|f| f.path.is_absolute()))
        .ok_or_else(|| CommandError::Failed(format!("Cannot find the source of {}", t.name)))?;
    s.execute("navigate.openLocation", &json!({ "path": loc.path, "line": loc.line, "column": loc.column }))?;
    Ok(())
}

/// Start a configuration again (a new Run tab).
pub(crate) fn start_origin(s: &mut Session, origin: TestOrigin, title: &str) -> Result<RunTabId, CommandError> {
    match origin {
        TestOrigin::Cargo(inv) => crate::runs::start(s, inv, title, false),
        TestOrigin::Process(cmd) => crate::runs::start_process(s, &cmd, title),
    }
}

/// `run.rerun` on a test tab: run its exact invocation again (filters included).
pub(crate) fn rerun_tab(s: &mut Session, tab: RunTabId) -> Option<Result<RunTabId, CommandError>> {
    let run = s.testing.run(tab)?;
    let origin = run.origin.clone()?;
    let title = run.title.clone();
    Some(start_origin(s, origin, &title))
}

/// The failed tests of a run, as (suite, test).
fn failed(run: &TestRun) -> Vec<(&TestSuite, &TestCase)> {
    run.suites
        .iter()
        .flat_map(|s| s.tests.iter().filter(|t| t.status == TestStatus::Failed).map(move |t| (s, t)))
        .collect()
}

/// How to run only the failed tests of `run`.
pub fn rerun_failed_origin(run: &TestRun) -> Result<TestOrigin, String> {
    let failed = failed(run);
    if failed.is_empty() {
        return Err("No failed tests".into());
    }
    let mut names: Vec<String> = Vec::new();
    for (_, t) in &failed {
        if !names.contains(&t.name) {
            names.push(t.name.clone());
        }
    }
    let origin = run.origin.as_ref().ok_or("This run cannot be rerun (it was imported)")?;
    match (run.framework, origin) {
        (Framework::Cargo, TestOrigin::Cargo(inv)) => {
            let mut inv = inv.clone();
            let keep: Vec<String> =
                inv.program_args.iter().filter(|a| a.starts_with('-') && *a != "--exact").cloned().collect();
            inv.test_filter = None;
            inv.test_exact = false;
            inv.program_args = keep;
            inv.program_args.push("--exact".into());
            inv.program_args.extend(names);
            // One suite failed: run only its target.
            let suites: BTreeSet<&str> = failed.iter().map(|(s, _)| s.name.as_str()).collect();
            if inv.target.is_none()
                && suites.len() == 1
                && let Some((suite, _)) = failed.first()
            {
                match suite.kind {
                    SuiteKind::Doc => inv.extra_args.push("--doc".into()),
                    SuiteKind::Unit if suite.source.as_deref().is_some_and(|p| p.ends_with("lib.rs")) => {
                        inv.target = Some(TargetSelector::Lib)
                    }
                    SuiteKind::Integration => {
                        if let Some(c) = &suite.crate_name {
                            inv.target = Some(TargetSelector::Test(c.clone()));
                        }
                    }
                    _ => {}
                }
            }
            Ok(TestOrigin::Cargo(inv))
        }
        (Framework::Cargo, TestOrigin::Process(cmd)) => {
            let mut cmd = cmd.clone();
            let (head, tail) = match cmd.args.iter().position(|a| a == "--") {
                Some(i) => (cmd.args.get(..i).unwrap_or(&[]).to_vec(), cmd.args.get(i + 1..).unwrap_or(&[]).to_vec()),
                None => (cmd.args.clone(), Vec::new()),
            };
            let mut args = head;
            args.push("--".into());
            args.extend(tail.into_iter().filter(|a| a.starts_with('-') && a != "--exact"));
            args.push("--exact".into());
            args.extend(names);
            cmd.args = args;
            Ok(TestOrigin::Process(cmd))
        }
        (Framework::Pytest, TestOrigin::Process(cmd)) if !matches!(basename(&cmd.program), "sh" | "bash" | "zsh") => {
            let mut cmd = cmd.clone();
            let mut args = Vec::new();
            let mut it = cmd.args.iter().peekable();
            while let Some(a) = it.next() {
                if a == "-m" {
                    if let Some(m) = it.next() {
                        args.push(a.clone());
                        args.push(m.clone());
                    }
                } else if a == "-k" || (a == "-o" && it.peek().is_some_and(|v| v.starts_with("junit_logging"))) {
                    it.next();
                } else if a.starts_with('-') && !a.starts_with("--junitxml") {
                    args.push(a.clone());
                }
            }
            for (suite, t) in &failed {
                let file = suite.source.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| suite.name.clone());
                args.push(format!("{file}::{}", t.name));
            }
            cmd.args = args;
            Ok(TestOrigin::Process(cmd))
        }
        (Framework::CTest, TestOrigin::Process(cmd)) => {
            let mut cmd = cmd.clone();
            let alternatives: Vec<String> = names.iter().map(|n| regex::escape(n)).collect();
            let filter = format!("^({})$", alternatives.join("|"));
            if basename(&cmd.program) == "ctest" {
                cmd.args.push("-R".into());
                cmd.args.push(filter);
            } else if let Some(script) = cmd.args.last_mut().filter(|s| s.trim_end().contains("ctest")) {
                script.push_str(" -R ");
                script.push_str(&codecraft_cargo::shell_quote(&filter));
            } else {
                return Err("Rerun Failed Tests needs a ctest command".into());
            }
            Ok(TestOrigin::Process(cmd))
        }
        (Framework::Zig, TestOrigin::Process(cmd))
            if basename(&cmd.program) == "zig" && cmd.args.first().is_some_and(|a| a == "test") =>
        {
            let mut cmd = cmd.clone();
            let mut args: Vec<String> = Vec::new();
            let mut skip = false;
            for a in &cmd.args {
                if skip {
                    skip = false;
                    continue;
                }
                if a == "--test-filter" {
                    skip = true;
                    continue;
                }
                args.push(a.clone());
            }
            for n in names {
                args.push("--test-filter".into());
                args.push(n);
            }
            cmd.args = args;
            Ok(TestOrigin::Process(cmd))
        }
        _ => Err("Rerun Failed Tests is not supported for this configuration; use Rerun".into()),
    }
}

// ---------------------------------------------------------------- history

fn history_path(root: &Path) -> PathBuf {
    root.join(".codecraft").join("test-history.json")
}

/// The stored runs, oldest first (an unreadable file reads as empty).
pub fn load_history(root: &Path) -> Vec<TestRun> {
    let path = history_path(root);
    let Ok(text) = codecraft_fs::read_text_file(&path) else { return Vec::new() };
    match serde_json::from_str::<Vec<TestRun>>(&text) {
        Ok(runs) => runs,
        Err(e) => {
            log::warn!("{}: {e}", path.display());
            Vec::new()
        }
    }
}

fn save_history(root: &Path, runs: &[TestRun]) -> Result<(), String> {
    let path = history_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string(runs).map_err(|e| e.to_string())?;
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| e.to_string())
}

/// Append a finished run to the history, keeping the last [`HISTORY_LIMIT`].
pub fn append_history(root: &Path, mut run: TestRun) -> Result<(), String> {
    for t in run.suites.iter_mut().flat_map(|s| s.tests.iter_mut()) {
        if t.output.len() > HISTORY_OUTPUT {
            let mut cut = HISTORY_OUTPUT;
            while !t.output.is_char_boundary(cut) {
                cut -= 1;
            }
            t.output.truncate(cut);
            t.output.push_str("… (truncated)\n");
        }
    }
    run.imported = true;
    let mut runs = load_history(root);
    runs.push(run);
    let excess = runs.len().saturating_sub(HISTORY_LIMIT);
    runs.drain(..excess);
    save_history(root, &runs)
}

pub fn history_summaries(root: &Path) -> Vec<HistorySummary> {
    load_history(root)
        .iter()
        .enumerate()
        .rev()
        .map(|(index, r)| HistorySummary {
            index,
            title: r.title.clone(),
            command_line: r.command_line.clone(),
            started_unix: r.started_unix,
            duration_ms: r.duration_ms,
            counts: r.counts(),
        })
        .collect()
}

pub(crate) fn clear_history(root: &Path) -> Result<(), String> {
    match std::fs::remove_file(history_path(root)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Show a stored or imported run in a new Run tab.
pub(crate) fn show_imported(s: &mut Session, mut run: TestRun, note: &str) -> RunTabId {
    let tab = s.ws.add_run(run.title.clone(), run.command_line.clone(), false);
    let failed = run.counts().failed;
    if let Some(t) = s.ws.run_mut(tab) {
        t.status = if failed > 0 { RunStatus::Failed(None) } else { RunStatus::Succeeded };
        t.duration = run.duration_ms.map(std::time::Duration::from_millis);
        t.push(ConsoleKind::System, note.to_string());
    }
    s.ws.layout.show(ToolWindowId::Run);
    run.tab = tab;
    run.imported = true;
    run.finished = true;
    run.view = TreeView::default();
    for suite in run.suites.iter_mut() {
        suite.finished = true;
    }
    s.testing.add_run(run);
    start_locate(s, tab);
    tab
}

/// Read a test result file: our JSON export (one run or a list, the last one wins) or JUnit XML.
pub fn import_file(path: &Path, base: &Path) -> Result<TestRun, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_LCOV_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Imported".into());
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(run) = serde_json::from_str::<TestRun>(&text) {
            return Ok(run);
        }
        return serde_json::from_str::<Vec<TestRun>>(&text)
            .map_err(|e| format!("{}: not a test result file ({e})", path.display()))?
            .pop()
            .ok_or_else(|| format!("{}: no test runs", path.display()));
    }
    if trimmed.starts_with('<') {
        let cases = junit::parse(&text);
        if cases.is_empty() {
            return Err(format!("{}: no test cases found", path.display()));
        }
        let mut run = TestRun::new(
            RunTabId(0),
            &title,
            format!("Imported {}", path.display()),
            Framework::Pytest,
            None,
            base.to_path_buf(),
        );
        junit::merge(&mut run.suites, cases, base);
        run.finished = true;
        run.started_unix =
            meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        return Ok(run);
    }
    Err(format!("{}: expected a JSON export or a JUnit XML report", path.display()))
}

// ---------------------------------------------------------------- coverage

fn reports_dir(root: &Path) -> PathBuf {
    root.join(".codecraft").join("coverage")
}

pub fn list_reports(root: &Path) -> Vec<ReportMeta> {
    let path = reports_dir(root).join("reports.json");
    codecraft_fs::read_text_file(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_reports(root: &Path, list: &[ReportMeta]) -> Result<(), String> {
    let dir = reports_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    codecraft_fs::write_text_file_atomic(&dir.join("reports.json"), &text).map_err(|e| e.to_string())
}

fn safe_id(id: &str) -> Result<&str, CommandError> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(CommandError::BadParams(format!("bad report id `{id}`")));
    }
    Ok(id)
}

/// Store an LCOV text as a report and return it parsed (runs off the UI thread after a run).
pub fn store_report(
    root: &Path,
    base: &Path,
    lcov_text: &str,
    name: &str,
    command_line: &str,
) -> Result<(CoverageReport, ReportMeta), String> {
    let mut report = CoverageReport::parse_lcov(lcov_text, base);
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!("{}-{}", now_millis(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let dir = reports_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    codecraft_fs::write_text_file_atomic(&dir.join(format!("{id}.lcov")), lcov_text).map_err(|e| e.to_string())?;
    let (lines_hit, lines_found) = report.totals();
    let meta = ReportMeta {
        id: id.clone(),
        name: name.to_string(),
        created_unix: now_unix(),
        command_line: command_line.to_string(),
        lines_hit,
        lines_found,
    };
    let mut list = list_reports(root);
    list.push(meta.clone());
    save_reports(root, &list)?;
    report.id = id;
    report.name = name.to_string();
    Ok((report, meta))
}

/// Load a stored report.
pub(crate) fn open_report(root: &Path, id: &str) -> Result<CoverageReport, CommandError> {
    let id = safe_id(id)?;
    let meta = list_reports(root)
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| CommandError::BadParams(format!("no coverage report `{id}`")))?;
    let path = reports_dir(root).join(format!("{id}.lcov"));
    let size = std::fs::metadata(&path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?.len();
    if size > MAX_LCOV_BYTES {
        return Err(CommandError::Failed(format!("{} is too large", path.display())));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
    let mut report = CoverageReport::parse_lcov(&text, root);
    report.id = meta.id;
    report.name = meta.name;
    Ok(report)
}

pub(crate) fn delete_report(root: &Path, id: &str) -> Result<(), CommandError> {
    let id = safe_id(id)?;
    let mut list = list_reports(root);
    let before = list.len();
    list.retain(|m| m.id != id);
    if list.len() == before {
        return Err(CommandError::BadParams(format!("no coverage report `{id}`")));
    }
    let _ = std::fs::remove_file(reports_dir(root).join(format!("{id}.lcov")));
    save_reports(root, &list).map_err(CommandError::Failed)
}

/// `cargo llvm-cov` for a cargo invocation (`test`, or `run` for binaries), writing LCOV to `lcov`.
pub fn llvm_cov_command(inv: &CargoInvocation, lcov: &Path) -> ProcessCommand {
    let mut args = inv.args();
    let rest: Vec<String> = if args.is_empty() { Vec::new() } else { args.split_off(1) };
    let (head, tail) = match rest.iter().position(|a| a == "--") {
        Some(i) => (rest.get(..i).unwrap_or(&[]).to_vec(), rest.get(i..).unwrap_or(&[]).to_vec()),
        None => (rest, Vec::new()),
    };
    let mut out = vec!["llvm-cov".to_string()];
    if inv.command == CargoCommand::Run {
        out.push("run".into());
    }
    out.extend(head.into_iter().filter(|a| !a.starts_with("--message-format")));
    out.extend(["--lcov".to_string(), "--output-path".to_string(), lcov.display().to_string()]);
    out.extend(tail);
    ProcessCommand { program: "cargo".into(), args: out, cwd: inv.cwd.clone(), env: inv.env.clone(), build: false }
}

/// Start `config` with coverage. Errors (with a notification) when the coverage tool is missing.
pub(crate) fn run_with_coverage(s: &mut Session, config: crate::RunConfig) -> Result<RunTabId, CommandError> {
    if s.project_root().is_none() {
        return Err(CommandError::Disabled("no project is open".into()));
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let lcov =
        std::env::temp_dir().join(format!("codecraft-coverage-{}-{}-{seq}.lcov", std::process::id(), now_millis()));
    let (cmd, python) = match (&config.command, config.cargo_invocation()) {
        (None, Some(inv)) => {
            if !matches!(inv.command, CargoCommand::Test | CargoCommand::Run) {
                return Err(CommandError::Failed(format!(
                    "Coverage needs a test or run configuration, not `{}`",
                    config.name
                )));
            }
            if codecraft_lsp::which("cargo-llvm-cov").is_none() {
                s.ws.notify(NotificationLevel::Warning, "Coverage", LLVM_COV_HINT);
                s.status = "cargo-llvm-cov is not installed".into();
                return Err(CommandError::Failed(LLVM_COV_HINT.into()));
            }
            (llvm_cov_command(inv, &lcov), false)
        }
        (Some(cmd), _)
            if process_framework(cmd) == Some(Framework::Pytest)
                && !matches!(basename(&cmd.program), "sh" | "bash" | "zsh") =>
        {
            let mut cmd = cmd.clone();
            cmd.args.push("--cov".into());
            cmd.args.push(format!("--cov-report=lcov:{}", lcov.display()));
            (cmd, true)
        }
        _ => {
            return Err(CommandError::Failed(format!(
                "Coverage is available for Cargo and pytest configurations, not `{}`",
                config.name
            )));
        }
    };
    let title = format!("{} with Coverage", config.name);
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    let base = cmd.cwd.clone();
    let command_line = s.ws.run(tab).map(|t| t.command_line.clone()).unwrap_or_default();
    s.testing.coverage_runs.push(CoverageRun { tab, lcov, base, name: config.name.clone(), command_line, python });
    Ok(tab)
}

/// Read (and store) the report a coverage run wrote; the temporary file is removed.
fn load_run_report(root: &Path, cr: &CoverageRun) -> Result<(CoverageReport, ReportMeta), String> {
    let size = std::fs::metadata(&cr.lcov).map_err(|e| e.to_string())?.len();
    if size > MAX_LCOV_BYTES {
        return Err(format!("{} is too large", cr.lcov.display()));
    }
    let text = std::fs::read_to_string(&cr.lcov).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&cr.lcov);
    store_report(root, &cr.base, &text, &cr.name, &cr.command_line)
}

fn pump_coverage(s: &mut Session) -> bool {
    let mut changed = false;
    // Runs that ended: read their report on a worker thread.
    let mut i = 0;
    while i < s.testing.coverage_runs.len() {
        let Some(cr) = s.testing.coverage_runs.get(i) else { break };
        let running = s.ws.run(cr.tab).is_some_and(|t| t.is_running());
        if running {
            i += 1;
            continue;
        }
        let cr = s.testing.coverage_runs.remove(i);
        changed = true;
        let console: String =
            s.ws.run(cr.tab)
                .map(|t| t.lines.iter().rev().take(400).map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
        if !cr.lcov.is_file() {
            let hint = if cr.python
                && (console.contains("unrecognized arguments: --cov") || console.contains("no such option: --cov"))
            {
                PYTEST_COV_HINT.to_string()
            } else if console.contains("no such command: `llvm-cov`") || console.contains("no such subcommand") {
                LLVM_COV_HINT.to_string()
            } else {
                format!("{}: no coverage data was written", cr.name)
            };
            s.ws.notify(NotificationLevel::Warning, "Coverage", hint.clone());
            s.status = hint;
            continue;
        }
        let Some(root) = s.project_root() else { continue };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new().name("codecraft-coverage".into()).spawn(move || {
            let _ = tx.send(load_run_report(&root, &cr));
        });
        match spawned {
            Ok(_) => s.testing.coverage_loads.push(CoverageLoad { rx }),
            Err(e) => log::warn!("coverage thread: {e}"),
        }
    }
    // Reports read.
    let mut i = 0;
    while i < s.testing.coverage_loads.len() {
        let Some(load) = s.testing.coverage_loads.get(i) else { break };
        let result = match load.rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                i += 1;
                continue;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the coverage reader stopped".into()),
        };
        s.testing.coverage_loads.remove(i);
        changed = true;
        match result {
            Ok((report, meta)) => {
                let pct = lcov::percent(meta.lines_hit, meta.lines_found);
                let msg = format!("{}: {pct:.1}% lines covered ({}/{})", meta.name, meta.lines_hit, meta.lines_found);
                s.ws.notify(NotificationLevel::Info, "Coverage", msg.clone());
                s.status = msg;
                s.testing.coverage = Some(report);
                s.testing.coverage_in_editor = true;
                s.ws.layout.show(ToolWindowId::Coverage);
            }
            Err(e) => {
                s.ws.notify(NotificationLevel::Error, "Coverage", e.clone());
                s.status = e;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_run(text: &str) -> TestRun {
        let inv = CargoInvocation::new(CargoCommand::Test, "/w");
        let mut run = TestRun::new(
            RunTabId(1),
            "Test",
            inv.command_line(),
            Framework::Cargo,
            Some(TestOrigin::Cargo(inv)),
            "/w".into(),
        );
        for line in text.lines() {
            run.parser.feed(Framework::Cargo, &mut run.suites, line);
        }
        run.parser.finish(&mut run.suites);
        run.finished = true;
        run
    }

    fn labels(rows: &[TreeRow]) -> Vec<String> {
        rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.label)).collect()
    }

    #[test]
    fn tree_groups_modules_and_filters() {
        let run = cargo_run(include_str!("testing/fixtures/cargo_test.txt"));
        let rows = tree_rows(&run, TreeOptions::default(), true);
        assert_eq!(
            labels(&rows),
            [
                "Test Results",
                "  demo: src/lib.rs",
                "    tests",
                "      nested",
                "        deep",
                "      adds",
                "      fails",
                "      slow",
                "    util",
                "      tests",
                "        panics",
                "        twice_works",
                "  tests/integration.rs",
                "    it_fails",
                "    it_works",
                "  Doc-tests demo",
                "    src/lib.rs - add (line 6)",
                "    src/lib.rs - sub (line 13)",
            ]
        );
        assert_eq!(rows[0].status, TestStatus::Failed);
        assert_eq!(rows[8].status, TestStatus::Passed);
        assert_eq!(rows[2].key, "s0:tests");
        assert_eq!(rows[3].key, "s0:tests::nested");
        // Hide passed and ignored: only failures (and their parents) remain.
        let opts = TreeOptions { show_passed: false, show_ignored: false, sort_by_duration: false };
        assert_eq!(
            labels(&tree_rows(&run, opts, true)),
            [
                "Test Results",
                "  demo: src/lib.rs",
                "    tests",
                "      fails",
                "  tests/integration.rs",
                "    it_fails",
                "  Doc-tests demo",
                "    src/lib.rs - sub (line 13)",
            ]
        );
    }

    #[test]
    fn select_expand_and_step_failures() {
        let mut s = Session::new();
        let mut run = cargo_run(include_str!("testing/fixtures/cargo_test.txt"));
        run.view.collapsed.insert("s0".into());
        s.testing.runs.push(run);
        let tab = RunTabId(1);
        assert_eq!(tree_rows(s.testing.run(tab).unwrap(), TreeOptions::default(), true).len(), 8);
        let first = step_failed(&mut s, tab, true).unwrap().unwrap();
        let run = s.testing.run(tab).unwrap();
        assert_eq!(run.test(0, test_key(&first).unwrap().1).unwrap().name, "tests::fails");
        assert!(!run.view.collapsed.contains("s0"), "selecting expands the parents");
        let second = step_failed(&mut s, tab, true).unwrap().unwrap();
        assert_eq!(test_key(&second).map(|k| k.0), Some(1));
        let third = step_failed(&mut s, tab, true).unwrap().unwrap();
        assert_eq!(test_key(&third).map(|k| k.0), Some(2));
        let wrapped = step_failed(&mut s, tab, true).unwrap().unwrap();
        assert_eq!(wrapped, first);
        let back = step_failed(&mut s, tab, false).unwrap().unwrap();
        assert_eq!(back, third);

        set_all_collapsed(&mut s, tab, true).unwrap();
        let rows = tree_rows(s.testing.run(tab).unwrap(), TreeOptions::default(), true);
        assert_eq!(rows.len(), 4, "root + three collapsed suites");
        set_all_collapsed(&mut s, tab, false).unwrap();
        assert!(select(&mut s, tab, "s9#9").is_err());
        select(&mut s, tab, "s0:util::tests").unwrap();

        let run = s.testing.run(tab).unwrap();
        assert!(node_output(run, "root").is_none());
        assert!(node_output(run, &first).unwrap().contains("math is broken"));
        assert!(node_output(run, "s0:tests").unwrap().contains("---- tests::fails ----"));
        assert_eq!(node_output(run, "s0:util").unwrap(), "2 test(s), no failures\n");
        assert_eq!(node_output(run, "s0#0").unwrap(), "Test ignored: slow\n");
    }

    #[test]
    fn sort_by_duration() {
        let mut run = cargo_run("running 3 tests\ntest a ... ok <0.100s>\ntest b ... ok <0.900s>\ntest c ... ok\n");
        run.suites[0].finished = true;
        let opts = TreeOptions { sort_by_duration: true, ..TreeOptions::default() };
        assert_eq!(labels(&tree_rows(&run, opts, true)), ["Test Results", "  Tests", "    b", "    a", "    c"]);
        assert_eq!(tree_rows(&run, opts, true)[1].duration_ms, Some(1000));
    }

    #[test]
    fn rerun_failed_cargo() {
        let run = cargo_run(include_str!("testing/fixtures/cargo_test.txt"));
        let Ok(TestOrigin::Cargo(inv)) = rerun_failed_origin(&run) else { panic!("cargo origin") };
        assert_eq!(
            inv.args(),
            [
                "test",
                "--message-format=json-diagnostic-rendered-ansi",
                "--",
                "--exact",
                "tests::fails",
                "it_fails",
                "src/lib.rs - sub (line 13)"
            ]
        );
        // Only one suite failed: narrow to its target and keep harness flags.
        let mut inv = CargoInvocation::new(CargoCommand::Test, "/w");
        inv.test_filter = Some("tests::".into());
        inv.program_args = vec!["--nocapture".into()];
        let mut run = cargo_run(
            "     Running tests/integration.rs (target/debug/deps/integration-0a1b)\nrunning 1 test\ntest it_fails ... FAILED\n",
        );
        run.origin = Some(TestOrigin::Cargo(inv));
        let Ok(TestOrigin::Cargo(inv)) = rerun_failed_origin(&run) else { panic!("cargo origin") };
        assert_eq!(inv.target, Some(TargetSelector::Test("integration".into())));
        assert_eq!(inv.program_args, ["--nocapture", "--exact", "it_fails"]);
        assert!(inv.test_filter.is_none());
        let ok = cargo_run("running 1 test\ntest a ... ok\n");
        assert_eq!(rerun_failed_origin(&ok).err().as_deref(), Some("No failed tests"));
    }

    #[test]
    fn rerun_failed_processes() {
        let mut run = TestRun::new(RunTabId(2), "pytest", String::new(), Framework::Pytest, None, "/p".into());
        let cmd = ProcessCommand::new("python3", &["-m", "pytest", "-x", "-k", "math", "tests"], "/p");
        run.origin = Some(TestOrigin::Process(cmd));
        for line in include_str!("testing/fixtures/pytest_v.txt").lines() {
            run.parser.feed(Framework::Pytest, &mut run.suites, line);
        }
        let Ok(TestOrigin::Process(cmd)) = rerun_failed_origin(&run) else { panic!("process") };
        assert_eq!(
            cmd.args,
            ["-m", "pytest", "-x", "tests/test_math.py::test_fail", "tests/test_math.py::TestGroup::test_param[2]"]
        );

        let mut run = TestRun::new(RunTabId(3), "CTest", String::new(), Framework::CTest, None, "/p".into());
        run.origin = Some(TestOrigin::Process(ProcessCommand::shell("cmake --build b && ctest --test-dir b", "/p")));
        for line in include_str!("testing/fixtures/ctest.txt").lines() {
            run.parser.feed(Framework::CTest, &mut run.suites, line);
        }
        let Ok(TestOrigin::Process(cmd)) = rerun_failed_origin(&run) else { panic!("process") };
        assert_eq!(cmd.args[1], "cmake --build b && ctest --test-dir b -R '^(test_fail|test_crash)$'");

        let mut run = TestRun::new(RunTabId(4), "zig", String::new(), Framework::Zig, None, "/p".into());
        run.origin =
            Some(TestOrigin::Process(ProcessCommand::new("zig", &["test", "a.zig", "--test-filter", "x"], "/p")));
        for line in include_str!("testing/fixtures/zig_test.txt").lines() {
            run.parser.feed(Framework::Zig, &mut run.suites, line);
        }
        let Ok(TestOrigin::Process(cmd)) = rerun_failed_origin(&run) else { panic!("process") };
        assert_eq!(cmd.args, ["test", "a.zig", "--test-filter", "fails"]);

        let mut run = TestRun::new(RunTabId(5), "cov", String::new(), Framework::Cargo, None, "/w".into());
        let inv = CargoInvocation::new(CargoCommand::Test, "/w");
        run.origin = Some(TestOrigin::Process(llvm_cov_command(&inv, Path::new("/tmp/x.lcov"))));
        for line in ["running 1 test", "test a::b ... FAILED"] {
            run.parser.feed(Framework::Cargo, &mut run.suites, line);
        }
        let Ok(TestOrigin::Process(cmd)) = rerun_failed_origin(&run) else { panic!("process") };
        assert_eq!(cmd.args, ["llvm-cov", "--lcov", "--output-path", "/tmp/x.lcov", "--", "--exact", "a::b"]);
    }

    #[test]
    fn process_frameworks_and_instrumentation() {
        let p = |prog: &str, args: &[&str]| ProcessCommand::new(prog, args, "/p");
        assert_eq!(process_framework(&p("/v/bin/python", &["-m", "pytest"])), Some(Framework::Pytest));
        assert_eq!(process_framework(&p("pytest", &[])), Some(Framework::Pytest));
        assert_eq!(
            process_framework(&p("sh", &["-c", "cmake --build b && ctest --test-dir b"])),
            Some(Framework::CTest)
        );
        assert_eq!(process_framework(&p("zig", &["build", "test"])), Some(Framework::Zig));
        assert_eq!(process_framework(&p("zig", &["test", "a.zig"])), Some(Framework::Zig));
        assert_eq!(process_framework(&p("zig", &["build", "run"])), None);
        assert_eq!(process_framework(&p("cargo", &["+nightly", "test"])), Some(Framework::Cargo));
        assert_eq!(process_framework(&p("cargo", &["llvm-cov", "--lcov"])), Some(Framework::Cargo));
        assert_eq!(process_framework(&p("cargo", &["llvm-cov", "run"])), None);
        assert_eq!(process_framework(&p("python3", &["app.py"])), None);

        let (cmd, junit) = instrument_process(&p("python3", &["-m", "pytest", "tests"]));
        let junit = junit.unwrap();
        assert!(cmd.args.contains(&"-v".to_string()));
        assert!(cmd.args.contains(&format!("--junitxml={}", junit.display())));
        let (cmd, _) = instrument_process(&p("python3", &["-m", "pytest", "-q"]));
        assert!(!cmd.args.contains(&"-v".to_string()));
        let (cmd, junit) = instrument_process(&p("pytest", &["--junitxml=out.xml"]));
        assert_eq!((cmd.args.len(), junit), (1, Some(PathBuf::from("/p/out.xml"))));
        let (cmd, junit) = instrument_process(&p("sh", &["-c", "pytest"]));
        assert_eq!((cmd.args.len(), junit), (2, None));
    }

    #[test]
    fn llvm_cov_commands() {
        let mut inv = CargoInvocation::new(CargoCommand::Test, "/w");
        inv.package = Some("demo".into());
        inv.test_filter = Some("tests::adds".into());
        inv.test_exact = true;
        let cmd = llvm_cov_command(&inv, Path::new("/tmp/c.lcov"));
        assert_eq!(cmd.program, "cargo");
        assert_eq!(
            cmd.args,
            ["llvm-cov", "--package", "demo", "--lcov", "--output-path", "/tmp/c.lcov", "--", "tests::adds", "--exact"]
        );
        let mut run = CargoInvocation::new(CargoCommand::Run, "/w");
        run.target = Some(TargetSelector::Bin("app".into()));
        let cmd = llvm_cov_command(&run, Path::new("/tmp/c.lcov"));
        assert_eq!(cmd.args, ["llvm-cov", "run", "--bin", "app", "--lcov", "--output-path", "/tmp/c.lcov"]);
    }

    #[test]
    fn history_keeps_the_last_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(load_history(root).is_empty());
        for i in 0..(HISTORY_LIMIT + 3) {
            let mut run = cargo_run("running 1 test\ntest a ... FAILED\n");
            run.title = format!("run {i}");
            run.suites[0].tests[0].output = "é".repeat(HISTORY_OUTPUT);
            append_history(root, run).unwrap();
        }
        let runs = load_history(root);
        assert_eq!(runs.len(), HISTORY_LIMIT);
        assert_eq!(runs.first().map(|r| r.title.as_str()), Some("run 3"));
        assert!(runs[0].suites[0].tests[0].output.len() <= HISTORY_OUTPUT + 20);
        let list = history_summaries(root);
        assert_eq!(list[0].title, format!("run {}", HISTORY_LIMIT + 2));
        assert_eq!(list[0].counts.failed, 1);
        // A corrupt file reads as empty and is replaced on the next append.
        std::fs::write(history_path(root), "{not json").unwrap();
        assert!(load_history(root).is_empty());
        clear_history(root).unwrap();
        clear_history(root).unwrap();
    }

    #[test]
    fn import_export_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let run = cargo_run(include_str!("testing/fixtures/cargo_test.txt"));
        let path = dir.path().join("export.json");
        std::fs::write(&path, serde_json::to_string(&run).unwrap()).unwrap();
        let back = import_file(&path, dir.path()).unwrap();
        assert_eq!(back.suites, run.suites);
        let xml = dir.path().join("report.xml");
        std::fs::write(&xml, include_str!("testing/fixtures/junit_pytest.xml")).unwrap();
        let imported = import_file(&xml, dir.path()).unwrap();
        assert_eq!(imported.counts(), Counts { passed: 2, failed: 2, ignored: 1, running: 0 });
        std::fs::write(dir.path().join("bad.txt"), "hello").unwrap();
        assert!(import_file(&dir.path().join("bad.txt"), dir.path()).is_err());
        assert!(import_file(&dir.path().join("missing.xml"), dir.path()).is_err());
        std::fs::write(dir.path().join("empty.json"), "[]").unwrap();
        assert!(import_file(&dir.path().join("empty.json"), dir.path()).is_err());
    }

    #[test]
    fn coverage_reports_are_stored_and_managed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (report, meta) =
            store_report(root, root, "SF:src/a.rs\nDA:1,1\nDA:2,0\nend_of_record\n", "Test", "cargo llvm-cov").unwrap();
        assert_eq!((meta.lines_hit, meta.lines_found), (1, 2));
        assert_eq!(report.id, meta.id);
        assert_eq!(list_reports(root).len(), 1);
        let opened = open_report(root, &meta.id).unwrap();
        assert_eq!(opened.files, report.files);
        assert!(open_report(root, "../etc").is_err());
        assert!(open_report(root, "nope").is_err());
        delete_report(root, &meta.id).unwrap();
        assert!(list_reports(root).is_empty());
        assert!(delete_report(root, &meta.id).is_err());
    }

    #[test]
    fn ancestors_of_nodes() {
        let run = cargo_run(include_str!("testing/fixtures/cargo_test.txt"));
        let deep = run.suites[0].tests.iter().position(|t| t.name == "tests::nested::deep").unwrap();
        assert_eq!(ancestors(&run, &format!("s0#{deep}")), ["root", "s0", "s0:tests", "s0:tests::nested"]);
        assert_eq!(ancestors(&run, "s0:util::tests"), ["root", "s0", "s0:util"]);
        assert_eq!(ancestors(&run, "s0"), ["root"]);
        assert_eq!(ancestors(&run, "bogus"), ["root"]);
    }
}
