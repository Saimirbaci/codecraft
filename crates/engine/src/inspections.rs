//! Code › Inspect Code…, Code Cleanup…, Analyze Stack Trace or Thread Dump… and Rearrange Code.
//!
//! Inspections run external linters on a worker thread: Rust through `cargo clippy` (or `cargo
//! check` when the Rust settings turn Clippy off or it is not installed), plus ruff, eslint,
//! clang-tidy and `zig fmt --check` when they are installed and the scope has files for them. Their
//! reports become [`Finding`]s (char coordinates, zero-based) grouped by inspection in the
//! Inspection Results tool window. Suggestions become [`Fix`]es that are turned into an LSP
//! `WorkspaceEdit` and applied through the refactoring preview; Code Cleanup applies every
//! machine-applicable fix in the scope and then formats (rustfmt, ruff format, zig fmt).

pub mod clippy;
pub mod fixes;
pub mod linters;
pub mod rearrange;
pub mod stacktrace;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use serde::{Deserialize, Serialize};

use codecraft_cargo::RunEvent;
use codecraft_text::Position;
use codecraft_workspace::{NotificationLevel, Severity, ToolWindowId};

use crate::{CommandError, Session};

/// Findings kept per run (a lint storm in generated code cannot flood the tool window).
pub const MAX_FINDINGS: usize = 10_000;
/// Linter stdout kept per process.
const MAX_OUTPUT_BYTES: usize = 64 << 20;
/// Stderr lines kept per process (the tail, for error messages).
const MAX_STDERR_LINES: usize = 200;
/// Files of a scope considered (paths handed to linters, formatted by Code Cleanup).
pub const MAX_SCOPE_FILES: usize = 20_000;
/// Files passed one by one to tools that need a file list (clang-tidy, cleanup formatters).
const MAX_FILE_ARGS: usize = 2000;

pub(crate) const CANCELLED: &str = "cancelled";

// ---------------------------------------------------------------- model

/// The external tool that reported a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Clippy,
    CargoCheck,
    Ruff,
    Eslint,
    ClangTidy,
    ZigFmt,
    /// mypy (Python type checker, M11 track Y).
    Mypy,
    /// pyright or basedpyright (M11 track Y).
    Pyright,
    /// `cargo audit` security advisories (M12 track RS).
    CargoAudit,
    /// `cargo deny` advisories, licenses, bans and sources (M12 track RS).
    CargoDeny,
    /// `rust-analyzer diagnostics`: its native diagnostics for the whole workspace (M14 CA).
    RustAnalyzer,
    /// `clang --analyze` (the Clang Static Analyzer) per translation unit (M14 CA).
    ClangAnalyzer,
    /// clangd's diagnostics (compiler warnings and its built-in clang-tidy) from a background
    /// clangd that opens each file (M14 CA).
    Clangd,
    /// cppcheck, when installed (M14 CA).
    Cppcheck,
    /// CodeCraft's own SQL inspections over `.sql` files (M14 CA).
    Sql,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Self::Clippy => "Clippy",
            Self::CargoCheck => "cargo check",
            Self::Ruff => "Ruff",
            Self::Eslint => "ESLint",
            Self::ClangTidy => "clang-tidy",
            Self::ZigFmt => "zig fmt",
            Self::Mypy => "mypy",
            Self::Pyright => "Pyright",
            Self::CargoAudit => "cargo audit",
            Self::CargoDeny => "cargo deny",
            Self::RustAnalyzer => "rust-analyzer",
            Self::ClangAnalyzer => "Clang Static Analyzer",
            Self::Clangd => "clangd",
            Self::Cppcheck => "cppcheck",
            Self::Sql => "SQL",
        }
    }

    /// Every tool, in the order a run reports them.
    pub const ALL: [Tool; 15] = [
        Self::Clippy,
        Self::CargoCheck,
        Self::RustAnalyzer,
        Self::CargoAudit,
        Self::CargoDeny,
        Self::Ruff,
        Self::Mypy,
        Self::Pyright,
        Self::Eslint,
        Self::Clangd,
        Self::ClangAnalyzer,
        Self::ClangTidy,
        Self::Cppcheck,
        Self::ZigFmt,
        Self::Sql,
    ];

    /// The stable id agents and profiles use (`clippy`, `rust_analyzer`, `clang_analyzer`, …).
    pub fn id(self) -> &'static str {
        match self {
            Self::Clippy => "clippy",
            Self::CargoCheck => "cargo_check",
            Self::Ruff => "ruff",
            Self::Eslint => "eslint",
            Self::ClangTidy => "clang_tidy",
            Self::ZigFmt => "zig_fmt",
            Self::Mypy => "mypy",
            Self::Pyright => "pyright",
            Self::CargoAudit => "cargo_audit",
            Self::CargoDeny => "cargo_deny",
            Self::RustAnalyzer => "rust_analyzer",
            Self::ClangAnalyzer => "clang_analyzer",
            Self::Clangd => "clangd",
            Self::Cppcheck => "cppcheck",
            Self::Sql => "sql",
        }
    }

    /// The language the tool checks: `rust`, `python`, `cpp`, `javascript`, `zig` or `sql`.
    pub fn language(self) -> &'static str {
        match self {
            Self::Clippy | Self::CargoCheck | Self::RustAnalyzer | Self::CargoAudit | Self::CargoDeny => "rust",
            Self::Ruff | Self::Mypy | Self::Pyright => "python",
            Self::Eslint => "javascript",
            Self::ClangTidy | Self::ClangAnalyzer | Self::Clangd | Self::Cppcheck => "cpp",
            Self::ZigFmt => "zig",
            Self::Sql => "sql",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "clippy" | "rust" | "cargo" => Some(Self::Clippy),
            "cargo_check" | "check" => Some(Self::CargoCheck),
            "ruff" | "python" => Some(Self::Ruff),
            "eslint" | "javascript" | "typescript" => Some(Self::Eslint),
            "clang_tidy" | "c" | "cpp" => Some(Self::ClangTidy),
            "zig_fmt" | "zig" => Some(Self::ZigFmt),
            "mypy" => Some(Self::Mypy),
            "pyright" | "basedpyright" => Some(Self::Pyright),
            "cargo_audit" | "audit" => Some(Self::CargoAudit),
            "cargo_deny" | "deny" => Some(Self::CargoDeny),
            "rust_analyzer" | "ra" => Some(Self::RustAnalyzer),
            "clang_analyzer" | "clang_analyze" | "clang_static_analyzer" | "static_analyzer" | "analyze" => {
                Some(Self::ClangAnalyzer)
            }
            "clangd" => Some(Self::Clangd),
            "cppcheck" => Some(Self::Cppcheck),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }
}

/// How safe a suggestion is to apply without looking (rustc's `Applicability`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Applicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

impl Applicability {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "machineapplicable" | "machine-applicable" | "safe" | "automatic" | "always" => Self::MachineApplicable,
            "maybeincorrect" | "maybe-incorrect" | "unsafe" | "suggested" | "sometimes" => Self::MaybeIncorrect,
            "hasplaceholders" | "has-placeholders" => Self::HasPlaceholders,
            _ => Self::Unspecified,
        }
    }

    /// Applied by Code Cleanup and "Fix all" without asking.
    pub fn is_safe(self) -> bool {
        self == Self::MachineApplicable
    }

    /// May be offered as a quick-fix (placeholders and display-only edits are not).
    pub fn is_offered(self) -> bool {
        matches!(self, Self::MachineApplicable | Self::MaybeIncorrect)
    }
}

/// One replacement in a file, in char coordinates (zero-based line and char column).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextChange {
    pub path: PathBuf,
    pub start: Position,
    pub end: Position,
    pub new_text: String,
}

/// A quick-fix: one or more changes applied together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub title: String,
    pub applicability: Applicability,
    pub changes: Vec<TextChange>,
}

/// One problem reported by a linter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Index in [`InspectionResults::findings`].
    pub id: usize,
    pub tool: Tool,
    /// The inspection (tree group): `clippy::needless_return`, `F401 unused-import`, `no-unused-vars`, …
    pub inspection: String,
    /// The lint code used to suppress it (`#[allow(…)]`, `# noqa: …`), when it has one.
    pub code: Option<String>,
    pub severity: Severity,
    pub path: PathBuf,
    pub start: Position,
    pub end: Position,
    pub message: String,
    /// The longer explanation shown in the description panel (plain text).
    pub description: String,
    /// The lint's documentation.
    pub url: Option<String>,
    pub fixes: Vec<Fix>,
    /// Line-independent identity (analyzer, rule, file, the line's text) for baselines (M14 CA).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    /// Already in the baseline: not a new problem (M14 CA).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub baseline: bool,
}

impl Finding {
    /// A finding without fixes or description (parsers fill in the rest).
    pub fn new(tool: Tool, inspection: impl Into<String>, severity: Severity, path: PathBuf, start: Position) -> Self {
        Self {
            id: 0,
            tool,
            inspection: inspection.into(),
            code: None,
            severity,
            path,
            start,
            end: start,
            message: String::new(),
            description: String::new(),
            url: None,
            fixes: Vec::new(),
            fingerprint: String::new(),
            baseline: false,
        }
    }

    /// The fix "Fix all" and Code Cleanup apply (the first machine-applicable one).
    pub fn safe_fix(&self) -> Option<&Fix> {
        self.fixes.iter().find(|f| f.applicability.is_safe())
    }

    /// The rule id profiles, filters and agents use: the lint code when there is one
    /// (`clippy::needless_return`, `F401`), else the inspection (`E0308`, `Compiler errors`).
    pub fn rule(&self) -> &str {
        self.code.as_deref().unwrap_or(&self.inspection)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// One file (`path`, default: the active editor's).
    File,
    /// A directory and everything under it (`path`, default: the active file's directory).
    Directory,
    /// The whole project.
    Project,
    /// Files with uncommitted changes (from the Git status).
    Uncommitted,
    /// Project files whose relative path matches a glob (`pattern`: `src/**/*.rs, !**/gen/**`;
    /// M14 CA).
    Glob,
}

impl ScopeKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "file" | "current_file" | "currentfile" => Some(Self::File),
            "directory" | "dir" | "folder" => Some(Self::Directory),
            "project" | "whole_project" | "all" => Some(Self::Project),
            "uncommitted" | "uncommitted_files" | "changes" | "changed" | "vcs" | "git" => Some(Self::Uncommitted),
            "glob" | "custom" | "pattern" => Some(Self::Glob),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Project => "project",
            Self::Uncommitted => "uncommitted",
            Self::Glob => "glob",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub kind: ScopeKind,
    /// The file or directory (absolute); `None` for project and uncommitted scopes.
    pub path: Option<PathBuf>,
    /// The glob of a [`ScopeKind::Glob`] scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

impl Scope {
    pub fn new(kind: ScopeKind, path: Option<PathBuf>) -> Self {
        Self { kind, path, pattern: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Inspect,
    Cleanup,
}

impl JobKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Inspect => "Inspect Code",
            Self::Cleanup => "Code Cleanup",
        }
    }
}

/// What one tool did during a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRun {
    pub tool: Tool,
    pub command_line: String,
    /// "4 problems", "not installed", "failed: …", …
    pub outcome: String,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InspectionResults {
    pub kind: JobKind,
    pub scope: Scope,
    /// "Project 'app'", "File src/main.rs", …
    pub scope_label: String,
    pub findings: Vec<Finding>,
    pub tools: Vec<ToolRun>,
    /// Stopped at [`MAX_FINDINGS`].
    pub truncated: bool,
    /// Hash of each reported file's text when it was inspected (fixes refuse stale files).
    #[serde(skip)]
    pub hashes: BTreeMap<PathBuf, u64>,
    /// Findings fixed or suppressed since the run (hidden, kept so ids stay stable).
    #[serde(default)]
    pub resolved: BTreeSet<usize>,
    /// The run's job id (agents ask for results by it; 0 for imported results; M14 CA).
    #[serde(default)]
    pub job: u64,
    /// The inspection profile the run used.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    /// The baseline file findings were compared with (those in it have `baseline: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<PathBuf>,
}

impl InspectionResults {
    /// Empty results of a run.
    pub fn new(kind: JobKind, scope: Scope, scope_label: impl Into<String>) -> Self {
        Self {
            kind,
            scope,
            scope_label: scope_label.into(),
            findings: Vec::new(),
            tools: Vec::new(),
            truncated: false,
            hashes: BTreeMap::new(),
            resolved: BTreeSet::new(),
            job: 0,
            profile: String::new(),
            baseline: None,
        }
    }

    /// Findings not fixed or suppressed yet.
    pub fn open(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| !self.resolved.contains(&f.id))
    }

    pub fn finding(&self, id: usize) -> Option<&Finding> {
        self.findings.get(id).filter(|f| f.id == id)
    }

    /// Open findings per severity.
    pub fn counts(&self) -> BTreeMap<Severity, usize> {
        let mut out = BTreeMap::new();
        for f in self.open() {
            *out.entry(f.severity).or_insert(0usize) += 1;
        }
        out
    }
}

/// The Specify Scope dialog of Inspect Code… / Code Cleanup….
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeDialog {
    pub kind: JobKind,
    pub scope: ScopeKind,
    /// The current file (for the File scope) and its directory (for Directory).
    pub file: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    /// Whether the project is a Git repository (the Uncommitted scope needs it).
    pub has_vcs: bool,
    /// The custom scope's glob (`src/**/*.rs`; M14 CA).
    #[serde(default)]
    pub pattern: String,
    /// The inspection profile to run with (M14 CA).
    #[serde(default)]
    pub profile: String,
}

/// The paste box of Analyze Stack Trace or Thread Dump….
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackDialog {
    pub text: String,
}

/// A run in progress.
pub struct Running {
    pub kind: JobKind,
    pub scope: Scope,
    /// The job id ([`InspectionResults::job`] once it finishes).
    pub job: u64,
    /// The last line a tool printed ("Checking app v0.1.0", …).
    pub progress: String,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Msg>,
}

enum Msg {
    Progress(String),
    Done(Box<JobOutput>),
}

/// A whole-file rewrite produced by Code Cleanup (`before` is the text it was computed from).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rewrite {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
}

struct JobOutput {
    results: InspectionResults,
    rewrites: Vec<Rewrite>,
    /// Errors from Code Cleanup's fix and format steps.
    cleanup_errors: Vec<String>,
    cancelled: bool,
}

#[derive(Default)]
pub struct InspectionsState {
    pub results: Option<InspectionResults>,
    pub running: Option<Running>,
    /// The finding selected in the tool window.
    pub selected: Option<usize>,
    /// Severities hidden by the tool window's filter.
    pub hidden: BTreeSet<Severity>,
    pub dialog: Option<ScopeDialog>,
    pub stack_dialog: Option<StackDialog>,
    /// The analyzed stack trace (shown in its own window).
    pub stack_trace: Option<stacktrace::StackTrace>,
    /// The last run's kind and scope (Rerun).
    pub last: Option<(JobKind, Scope, Option<Vec<Tool>>)>,
    /// Tool programs overridden for tests (`ruff` → a fake).
    pub programs: HashMap<Tool, PathBuf>,
    /// Fixes waiting in the refactoring preview.
    pub(crate) pending: Option<fixes::Pending>,
    /// Run Inspection by Name: the running job keeps only this inspection's findings.
    pub only: Option<String>,
    /// Silent Code Cleanup: the running cleanup applies without the preview.
    pub silent: bool,
    /// The last job id handed out (M14 CA).
    pub next_job: u64,
    /// How the results tree is grouped (M14 CA).
    pub group_by: GroupBy,
    /// Show only problems that are not in the baseline (M14 CA).
    pub only_new: bool,
    /// A tool install or a commit check is in progress (set by `analysis::pump`; M14 CA).
    pub(crate) services_busy: bool,
}

impl InspectionsState {
    pub fn busy(&self) -> bool {
        self.running.is_some() || self.services_busy
    }

    /// Whether a finding passes the severity and baseline filters and is still open.
    pub fn visible(&self, f: &Finding) -> bool {
        !self.hidden.contains(&f.severity)
            && !(self.only_new && f.baseline)
            && self.results.as_ref().is_some_and(|r| !r.resolved.contains(&f.id))
    }
}

/// How the Inspection Results tree groups problems (M14 CA).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// Inspection (rule) → file → problems (RustRover's default).
    #[default]
    Rule,
    /// Severity → file → problems.
    Severity,
    /// Analyzer → rule → problems.
    Analyzer,
    /// File → rule → problems.
    File,
}

impl GroupBy {
    pub const ALL: [GroupBy; 4] = [Self::Rule, Self::Severity, Self::Analyzer, Self::File];

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "rule" | "inspection" => Some(Self::Rule),
            "severity" => Some(Self::Severity),
            "analyzer" | "tool" => Some(Self::Analyzer),
            "file" | "directory" => Some(Self::File),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Severity => "severity",
            Self::Analyzer => "analyzer",
            Self::File => "file",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rule => "Inspection",
            Self::Severity => "Severity",
            Self::Analyzer => "Analyzer",
            Self::File => "File",
        }
    }
}

// ---------------------------------------------------------------- scopes

/// Resolve a scope against the session: the files it covers and a label.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedScope {
    pub scope: Scope,
    pub label: String,
    /// Files in the scope (from the project tree; capped).
    pub files: Vec<PathBuf>,
    /// For the uncommitted scope: exactly these files.
    pub only: Option<HashSet<PathBuf>>,
}

impl ResolvedScope {
    pub fn contains(&self, path: &Path) -> bool {
        if let Some(only) = &self.only {
            return only.contains(path);
        }
        match (&self.scope.kind, &self.scope.path) {
            (ScopeKind::File, Some(p)) => path == p,
            (ScopeKind::Directory, Some(p)) => path.starts_with(p),
            _ => true,
        }
    }
}

/// Whether a path (relative to the scope's directory) is inside generated build output: CMake
/// build directories (`cmake-build-debug`, `CMakeFiles` with CMake's own compiler-check sources),
/// `node_modules`, `__pycache__`, virtual environments. CLion and PyCharm exclude these from
/// inspections (QA M14: clang-tidy and clangd analysed CMake's `CMakeCXXCompilerId.cpp`).
pub(crate) fn in_build_output(rel: &Path) -> bool {
    let mut parts = rel.components().filter_map(|c| match c {
        std::path::Component::Normal(n) => n.to_str(),
        _ => None,
    });
    // Only the directories count (a file may be called anything).
    let _ = parts.next_back();
    parts.any(|d| {
        d.starts_with("cmake-build-")
            || matches!(d, "CMakeFiles" | "node_modules" | "__pycache__" | ".venv" | "venv" | ".tox" | ".mypy_cache" | ".git")
    })
}

pub(crate) fn resolve_scope(s: &Session, kind: ScopeKind, path: Option<PathBuf>) -> Result<ResolvedScope, CommandError> {
    resolve_scope_with(s, kind, path, None)
}

/// [`resolve_scope`] with the glob of a custom scope.
pub(crate) fn resolve_scope_with(
    s: &Session,
    kind: ScopeKind,
    path: Option<PathBuf>,
    pattern: Option<&str>,
) -> Result<ResolvedScope, CommandError> {
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let active = s.ws.active().and_then(|(_, d)| d.path.clone());
    let rel = |p: &Path| match p.strip_prefix(&root) {
        Ok(r) if !r.as_os_str().is_empty() => r.display().to_string(),
        _ => p.display().to_string(),
    };
    let tree_files = |under: &Path| -> Vec<PathBuf> {
        match &s.ws.project {
            Some(p) => p
                .tree
                .files()
                .filter(|n| n.path.strip_prefix(under).is_ok_and(|r| !in_build_output(r)))
                .map(|n| n.path.clone())
                .take(MAX_SCOPE_FILES)
                .collect(),
            None => Vec::new(),
        }
    };
    match kind {
        ScopeKind::File => {
            let path = path.map(|p| crate::normalize_path(&p)).or(active).ok_or_else(|| {
                CommandError::BadParams("the file scope needs `path` (or an open editor)".into())
            })?;
            if !path.is_file() {
                return Err(CommandError::Failed(format!("{} is not a file", path.display())));
            }
            let label = format!("File {}", rel(&path));
            Ok(ResolvedScope { scope: Scope::new(kind, Some(path.clone())), label, files: vec![path], only: None })
        }
        ScopeKind::Directory => {
            let dir = path
                .map(|p| crate::normalize_path(&p))
                .or_else(|| active.as_deref().and_then(Path::parent).map(Path::to_path_buf))
                .ok_or_else(|| CommandError::BadParams("the directory scope needs `path` (or an open editor)".into()))?;
            if !dir.is_dir() {
                return Err(CommandError::Failed(format!("{} is not a directory", dir.display())));
            }
            let label = format!("Directory {}", rel(&dir));
            let files = tree_files(&dir);
            Ok(ResolvedScope { scope: Scope::new(kind, Some(dir)), label, files, only: None })
        }
        ScopeKind::Project => {
            let name = s.ws.project.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| rel(&root));
            Ok(ResolvedScope {
                scope: Scope::new(kind, None),
                label: format!("Project '{name}'"),
                files: tree_files(&root),
                only: None,
            })
        }
        ScopeKind::Uncommitted => {
            let repo = s
                .vcs
                .repo
                .as_ref()
                .ok_or_else(|| CommandError::Failed("the project is not under Git: no uncommitted files".into()))?;
            let files: Vec<PathBuf> = s
                .vcs
                .status
                .files
                .iter()
                .filter(|f| !f.ignored)
                .map(|f| crate::normalize_path(&repo.root.join(&f.path)))
                .filter(|p| p.is_file())
                .take(MAX_SCOPE_FILES)
                .collect();
            if files.is_empty() {
                return Err(CommandError::Failed("there are no uncommitted files".into()));
            }
            let only = files.iter().cloned().collect();
            Ok(ResolvedScope {
                scope: Scope::new(kind, None),
                label: format!("Uncommitted files ({})", files.len()),
                files,
                only: Some(only),
            })
        }
        ScopeKind::Glob => {
            let pattern = pattern
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| CommandError::BadParams("the glob scope needs `pattern` (e.g. \"src/**/*.rs\")".into()))?;
            let glob = crate::analysis::glob::PathGlob::parse(pattern).map_err(CommandError::BadParams)?;
            let files: Vec<PathBuf> = tree_files(&root)
                .into_iter()
                .filter(|f| f.strip_prefix(&root).is_ok_and(|r| glob.matches(r)))
                .collect();
            if files.is_empty() {
                return Err(CommandError::Failed(format!("no project file matches '{pattern}'")));
            }
            let only = files.iter().cloned().collect();
            Ok(ResolvedScope {
                scope: Scope { kind, path: None, pattern: Some(pattern.to_string()) },
                label: format!("Files matching '{pattern}' ({})", files.len()),
                files,
                only: Some(only),
            })
        }
    }
}

// ---------------------------------------------------------------- starting a run

/// Everything the worker needs (built on the UI thread).
pub(crate) struct JobSpec {
    pub kind: JobKind,
    pub scope: ResolvedScope,
    pub root: PathBuf,
    /// Where cargo runs and its relative paths resolve (the Cargo workspace root).
    pub cargo_root: Option<PathBuf>,
    pub use_clippy: bool,
    /// Only these tools (all applicable ones when `None`).
    pub tools: Option<Vec<Tool>>,
    /// Rust edition per crate root (for rustfmt in Code Cleanup), longest prefix wins.
    pub editions: Vec<(PathBuf, String)>,
    pub programs: HashMap<Tool, PathBuf>,
    /// Code Cleanup formats the scope's files after fixing.
    pub format: bool,
    /// Run Inspection by Name: a lint to turn on (`-W lint`) and keep alone in the results.
    pub only: Option<String>,
    /// The run's job id (M14 CA).
    pub job: u64,
    /// The inspection profile: which analyzers and rules run, at which severity (M14 CA).
    pub profile: Arc<crate::analysis::profiles::InspectionProfile>,
    /// Programs and directories of the M14 analyzers (clang, cmake, managed tools; M14 CA).
    pub env: crate::analysis::analyzers::AnalyzerEnv,
    /// Findings already in this baseline are marked `baseline` (M14 CA).
    pub baseline: Option<Arc<crate::analysis::baseline::Baseline>>,
}

/// Start Inspect Code or Code Cleanup on a scope (cancelling a run in progress).
pub(crate) fn start(
    s: &mut Session,
    kind: JobKind,
    scope_kind: ScopeKind,
    path: Option<PathBuf>,
    tools: Option<Vec<Tool>>,
    format: bool,
) -> Result<(), CommandError> {
    start_with(s, kind, scope_kind, path, tools, format, None, false)
}

/// [`start`] for Run Inspection by Name (`only`: keep just this inspection) and Silent Code
/// Cleanup (`silent`: apply without the preview).
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_with(
    s: &mut Session,
    kind: JobKind,
    scope_kind: ScopeKind,
    path: Option<PathBuf>,
    tools: Option<Vec<Tool>>,
    format: bool,
    only: Option<String>,
    silent: bool,
) -> Result<(), CommandError> {
    let opts = StartOptions { tools, format, only, silent, ..StartOptions::default() };
    start_opts(s, kind, scope_kind, path, opts).map(|_| ())
}

/// Everything about a run besides its kind and scope (M14 CA).
#[derive(Clone, Debug, Default)]
pub(crate) struct StartOptions {
    pub tools: Option<Vec<Tool>>,
    pub format: bool,
    pub only: Option<String>,
    pub silent: bool,
    /// The glob of a custom scope.
    pub pattern: Option<String>,
    /// An inspection profile by name (default: the project's active one).
    pub profile: Option<String>,
    /// An already resolved scope (the commit check's files).
    pub resolved: Option<ResolvedScope>,
    /// Keep the Inspection Results tool window as it is (agents, the commit check).
    pub quiet: bool,
}

/// Start a run; returns its job id.
pub(crate) fn start_opts(
    s: &mut Session,
    kind: JobKind,
    scope_kind: ScopeKind,
    path: Option<PathBuf>,
    opts: StartOptions,
) -> Result<u64, CommandError> {
    let StartOptions { tools, format, only, silent, pattern, profile, resolved, quiet } = opts;
    let scope = match resolved {
        Some(r) => r,
        None => resolve_scope_with(s, scope_kind, path, pattern.as_deref())?,
    };
    let profile = crate::analysis::profiles::resolve(s, profile.as_deref()).map_err(CommandError::BadParams)?;
    // Linters read the disk: save first, like RustRover does before running external tools.
    s.save_all()?;
    cancel(s);
    s.inspections.only = only.clone();
    s.inspections.silent = silent;
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let cargo = s.ws.project.as_ref().and_then(|p| p.cargo.as_ref());
    let cargo_root = match cargo {
        Some(m) => Some(m.workspace_root.clone()),
        None => root.join("Cargo.toml").is_file().then(|| root.clone()),
    };
    let editions = cargo
        .map(|m| {
            m.workspace_packages()
                .filter_map(|p| p.manifest_path.parent().map(|d| (d.to_path_buf(), p.edition.clone())))
                .collect()
        })
        .unwrap_or_default();
    s.inspections.last = Some((kind, scope.scope.clone(), tools.clone()));
    s.inspections.next_job = s.inspections.next_job.saturating_add(1);
    let job = s.inspections.next_job;
    let env = crate::analysis::analyzers::env(s, &root);
    let baseline = crate::analysis::baseline::current(s);
    let spec = JobSpec {
        kind,
        root,
        cargo_root,
        use_clippy: s.ws.settings.use_clippy,
        tools,
        editions,
        programs: s.inspections.programs.clone(),
        format,
        scope,
        only,
        job,
        profile: Arc::new(profile),
        env,
        baseline,
    };
    let scope_value = spec.scope.scope.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded();
    let flag = Arc::clone(&cancel);
    std::thread::Builder::new()
        .name("codecraft-inspections".into())
        .spawn(move || {
            let out = run_job(&spec, &flag, &tx);
            let _ = tx.send(Msg::Done(Box::new(out)));
        })
        .map_err(|e| CommandError::Failed(format!("cannot start the inspection: {e}")))?;
    s.inspections.running =
        Some(Running { kind, scope: scope_value, job, progress: format!("{}…", kind.title()), cancel, rx });
    s.inspections.selected = None;
    if !quiet {
        s.ws.layout.show(ToolWindowId::InspectionResults);
    }
    s.status = format!("{}…", kind.title());
    crate::analysis::jobs::started(s, job);
    Ok(job)
}

/// Stop the run in progress (its processes are killed).
pub(crate) fn cancel(s: &mut Session) -> bool {
    match s.inspections.running.take() {
        Some(r) => {
            r.cancel.store(true, Ordering::Relaxed);
            crate::analysis::jobs::ended(s, r.job, true);
            true
        }
        None => false,
    }
}

pub(crate) fn shutdown(s: &mut Session) {
    cancel(s);
}

pub(crate) fn pump(s: &mut Session) -> bool {
    // The M14 analysis services: tool installs, the commit check (M14 CA).
    let services = crate::analysis::pump(s);
    services | pump_run(s)
}

fn pump_run(s: &mut Session) -> bool {
    let Some(running) = &mut s.inspections.running else { return false };
    let mut changed = false;
    let mut done = None;
    loop {
        match running.rx.try_recv() {
            Ok(Msg::Progress(p)) => {
                running.progress = p;
                changed = true;
            }
            Ok(Msg::Done(out)) => {
                done = Some(out);
                break;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                let job = running.job;
                s.inspections.running = None;
                crate::analysis::jobs::ended(s, job, true);
                s.notify(NotificationLevel::Error, "Inspect Code", "the inspection stopped unexpectedly");
                return true;
            }
        }
    }
    let Some(out) = done else { return changed };
    let job = running.job;
    s.inspections.running = None;
    if out.cancelled {
        s.status = "Inspection cancelled".into();
        crate::analysis::jobs::ended(s, job, true);
        return true;
    }
    finish(s, *out);
    crate::analysis::jobs::ended(s, job, false);
    true
}

fn finish(s: &mut Session, out: JobOutput) {
    let JobOutput { mut results, rewrites, cleanup_errors, .. } = out;
    if let Some(only) = s.inspections.only.take() {
        keep_only(&mut results, &only);
        results.scope_label = format!("'{only}' in {}", results.scope_label);
    }
    let silent = std::mem::take(&mut s.inspections.silent);
    let kind = results.kind;
    let failed: Vec<String> =
        results.tools.iter().filter(|t| !t.ok).map(|t| format!("{}: {}", t.tool.label(), t.outcome)).collect();
    let total = results.open().count();
    s.status = match kind {
        JobKind::Inspect => format!("{}: {} problem{}", results.scope_label, total, if total == 1 { "" } else { "s" }),
        JobKind::Cleanup => format!("Code Cleanup: {} file(s) to change", rewrites.len()),
    };
    if !failed.is_empty() {
        s.ws.notify(NotificationLevel::Warning, kind.title(), failed.join("\n"));
    }
    if !cleanup_errors.is_empty() {
        s.ws.notify(NotificationLevel::Warning, "Code Cleanup", cleanup_errors.join("\n"));
    }
    s.inspections.selected = results.open().next().map(|f| f.id);
    s.inspections.results = Some(results);
    crate::cargo_audit::after_inspection(s);
    if kind == JobKind::Cleanup {
        if rewrites.is_empty() {
            s.notify(NotificationLevel::Info, "Code Cleanup", "nothing to clean up");
            return;
        }
        if let Err(e) = fixes::open_rewrites(s, "Code Cleanup", &rewrites) {
            s.notify(NotificationLevel::Error, "Code Cleanup", e.to_string());
            return;
        }
        // Silent Code Cleanup applies at once (the pending fixes settle in the next pump).
        if silent && let Some(pv) = s.refactor.preview.take_if(|pv| pv.title == "Code Cleanup") {
            let report = s.apply_preview(&pv);
            s.status = format!("Silent Code Cleanup: {} file(s) changed", report.applied);
            if !report.errors.is_empty() {
                s.notify(NotificationLevel::Error, "Silent Code Cleanup", report.errors.join("\n"));
            }
            s.refactor.last_report = Some(report);
        }
    }
}

/// Keep the findings of one inspection (by name, lint code or the name without its tool
/// prefix), renumbered.
pub(crate) fn keep_only(results: &mut InspectionResults, only: &str) {
    let only = only.trim();
    let bare = |s: &str| s.rsplit("::").next().unwrap_or(s).to_string();
    results.findings.retain(|f| {
        f.inspection == only
            || f.code.as_deref() == Some(only)
            || f.inspection.split_whitespace().next() == Some(only)
            || (!only.contains("::") && bare(&f.inspection) == only)
    });
    for (i, f) in results.findings.iter_mut().enumerate() {
        f.id = i;
    }
    results.resolved.clear();
}

// ---------------------------------------------------------------- the worker

/// Stdout and the stderr tail of a finished process.
#[derive(Default)]
pub(crate) struct ProcOutput {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub success: bool,
    pub code: Option<i32>,
}

impl ProcOutput {
    pub fn stderr_summary(&self) -> String {
        let lines: Vec<&str> =
            self.stderr.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).rev().take(4).collect();
        let mut lines = lines;
        lines.reverse();
        let s = lines.join(" | ");
        if s.is_empty() { format!("exit code {}", self.code.map_or("?".into(), |c| c.to_string())) } else { s }
    }
}

/// Run a program to completion, streaming stderr lines as progress; killed when `cancel` is set.
pub(crate) fn run_process(
    program: &Path,
    args: &[String],
    cwd: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<ProcOutput, String> {
    let (handle, rx) =
        codecraft_cargo::spawn_process(program, args, cwd, &[], false).map_err(|e| format!("{}: {e}", program.display()))?;
    let mut out = ProcOutput::default();
    let mut bytes = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = handle.kill();
            // Give the runner a moment to reap the group; the events are not needed.
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(RunEvent::Exited { .. }) | Err(RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
            }
            return Err(CANCELLED.into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(RunEvent::Stdout(line)) => {
                bytes = bytes.saturating_add(line.len());
                if bytes <= MAX_OUTPUT_BYTES {
                    out.stdout.push(line);
                }
            }
            Ok(RunEvent::Stderr(line)) => {
                let line = crate::runs::strip_ansi(&line);
                progress(line.trim());
                if out.stderr.len() >= MAX_STDERR_LINES {
                    out.stderr.remove(0);
                }
                out.stderr.push(line);
            }
            Ok(RunEvent::Error(e)) => out.stderr.push(e),
            Ok(RunEvent::Exited { code, success, .. }) => {
                out.code = code;
                out.success = success;
                return Ok(out);
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(out),
        }
    }
}

fn has_ext(files: &[PathBuf], exts: &[&str]) -> bool {
    files.iter().any(|f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e)))
}

fn with_ext<'a>(files: &'a [PathBuf], exts: &'a [&str]) -> impl Iterator<Item = &'a PathBuf> + 'a {
    files.iter().filter(move |f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e)))
}

pub(crate) const RUST_EXTS: &[&str] = &["rs"];
pub(crate) const PY_EXTS: &[&str] = &["py", "pyi"];
pub(crate) const JS_EXTS: &[&str] = &["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];
pub(crate) const C_EXTS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "m", "mm"];
pub(crate) const ZIG_EXTS: &[&str] = &["zig"];

/// The program for a tool: an override, else what is installed.
pub(crate) fn find_program(spec: &JobSpec, tool: Tool) -> Option<PathBuf> {
    if let Some(p) = spec.programs.get(&tool) {
        return Some(p.clone());
    }
    match tool {
        Tool::Clippy | Tool::CargoCheck => Some(codecraft_cargo::find_cargo()),
        Tool::Ruff => codecraft_lsp::which("ruff"),
        Tool::Eslint => {
            let local = spec.root.join("node_modules").join(".bin").join("eslint");
            if local.is_file() { Some(local) } else { codecraft_lsp::which("eslint") }
        }
        Tool::ClangTidy => codecraft_lsp::which("clang-tidy").or_else(|| {
            ["/opt/homebrew/opt/llvm/bin/clang-tidy", "/usr/local/opt/llvm/bin/clang-tidy"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
        }),
        Tool::ZigFmt => codecraft_lsp::which("zig"),
        Tool::Mypy => crate::python_lint::find(&spec.root, tool),
        // The project's or PATH's, else the one Install basedpyright put in the managed tools.
        Tool::Pyright => crate::python_lint::find(&spec.root, tool).or_else(|| spec.env.managed_tool("basedpyright")),
        Tool::CargoAudit | Tool::CargoDeny => crate::cargo_audit::find(tool),
        Tool::RustAnalyzer => codecraft_lsp::find_rust_analyzer(),
        Tool::ClangAnalyzer => spec.env.clang.clone(),
        Tool::Clangd => spec.env.clangd.clone(),
        Tool::Cppcheck => codecraft_lsp::which("cppcheck"),
        // Built in: no program.
        Tool::Sql => None,
    }
}

/// Paths to pass to a linter for the scope: the scope's path (or `.`), or the files themselves.
fn scope_args(spec: &JobSpec, exts: &[&str]) -> Vec<String> {
    match (&spec.scope.scope.kind, &spec.scope.scope.path) {
        (ScopeKind::File | ScopeKind::Directory, Some(p)) => vec![p.display().to_string()],
        (ScopeKind::Uncommitted | ScopeKind::Glob, _) => {
            with_ext(&spec.scope.files, exts).take(MAX_FILE_ARGS).map(|p| p.display().to_string()).collect()
        }
        _ => vec![".".into()],
    }
}

/// Whether a tool runs: the ones asked for, else every one the inspection profile turns on.
pub(crate) fn wanted(spec: &JobSpec, tool: Tool) -> bool {
    match &spec.tools {
        None => spec.profile.analyzer_enabled(tool),
        Some(list) => {
            list.contains(&tool) || (tool == Tool::Clippy && list.contains(&Tool::CargoCheck))
        }
    }
}

pub(crate) fn count_label(n: usize) -> String {
    format!("{n} problem{}", if n == 1 { "" } else { "s" })
}

fn run_job(spec: &JobSpec, cancel: &AtomicBool, tx: &Sender<Msg>) -> JobOutput {
    let progress = |line: &str| {
        if !line.is_empty() {
            let _ = tx.send(Msg::Progress(line.chars().take(200).collect()));
        }
    };
    let mut tools = Vec::new();
    let mut findings: Vec<Finding> = Vec::new();
    let files = &spec.scope.files;
    let project_wide = matches!(spec.scope.scope.kind, ScopeKind::Project);
    let mut cancelled = false;

    // Rust: cargo clippy (or check).
    if wanted(spec, Tool::Clippy)
        && let Some(cargo_root) = &spec.cargo_root
        && (has_ext(files, RUST_EXTS) || project_wide)
    {
        match clippy::run(spec, cargo_root, cancel, &progress) {
            Ok((run, mut found)) => {
                tools.push(run);
                findings.append(&mut found);
            }
            Err(e) if e == CANCELLED => cancelled = true,
            Err(e) => tools.push(ToolRun {
                tool: Tool::Clippy,
                command_line: "cargo clippy".into(),
                outcome: format!("failed: {e}"),
                ok: false,
            }),
        }
    }
    // Security advisories: cargo audit / cargo deny when installed (M12 track RS).
    if !cancelled {
        match crate::cargo_audit::run(spec, cancel, &progress, &mut tools, &mut findings) {
            Ok(()) => {}
            Err(e) if e == CANCELLED => cancelled = true,
            Err(e) => log::warn!("advisory tools: {e}"),
        }
    }
    // C/C++: make sure CMake projects have a compile_commands.json for the analyzers (M14 CA).
    if !cancelled {
        crate::analysis::analyzers::prepare_cpp(spec, cancel, &progress);
    }
    let linters: [(Tool, &[&str]); 6] = [
        (Tool::Ruff, PY_EXTS),
        (Tool::Mypy, PY_EXTS),
        (Tool::Pyright, PY_EXTS),
        (Tool::Eslint, JS_EXTS),
        (Tool::ClangTidy, C_EXTS),
        (Tool::ZigFmt, ZIG_EXTS),
    ];
    for (tool, exts) in linters {
        if cancelled || cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if !wanted(spec, tool) || !has_ext(files, exts) {
            continue;
        }
        let Some(program) = find_program(spec, tool) else {
            // Only an explicit request for a missing tool is worth reporting.
            if spec.tools.is_some() {
                tools.push(ToolRun { tool, command_line: String::new(), outcome: "not installed".into(), ok: false });
            }
            continue;
        };
        let args = linters::args(tool, spec, &scope_args(spec, exts), exts);
        progress(&format!("Running {}…", tool.label()));
        let command_line = format!(
            "{} {}",
            program.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            args.iter().map(|a| codecraft_cargo::shell_quote(a)).collect::<Vec<_>>().join(" ")
        );
        match run_process(&program, &args, &spec.root, cancel, &progress) {
            Ok(out) => {
                let (mut found, failure) = linters::parse(tool, spec, &program, &out, cancel);
                let n = found.len();
                findings.append(&mut found);
                let (outcome, ok) = match failure {
                    Some(f) => (format!("failed: {f}"), false),
                    None => (count_label(n), true),
                };
                tools.push(ToolRun { tool, command_line, outcome, ok });
            }
            Err(e) if e == CANCELLED => cancelled = true,
            Err(e) => tools.push(ToolRun { tool, command_line, outcome: format!("failed: {e}"), ok: false }),
        }
    }
    // rust-analyzer, clangd, the Clang Static Analyzer, cppcheck and SQL (M14 CA); Code Cleanup
    // only needs the analyzers with machine-applicable fixes above.
    if !cancelled && spec.kind == JobKind::Inspect {
        cancelled = crate::analysis::analyzers::run(spec, cancel, &progress, &mut tools, &mut findings);
    }
    let mut results = InspectionResults::new(spec.kind, spec.scope.scope.clone(), spec.scope.label.clone());
    results.tools = tools;
    results.job = spec.job;
    results.profile = spec.profile.name.clone();
    if cancelled {
        return JobOutput { results, rewrites: Vec::new(), cleanup_errors: Vec::new(), cancelled: true };
    }
    // The profile's rule switches and severities, NOLINT comments, one report per problem.
    let findings = crate::analysis::analyzers::postprocess(spec, findings);
    let (mut findings, truncated) = finalize(findings, |p| spec.scope.contains(p));
    results.truncated = truncated;
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for f in &findings {
        if !texts.contains_key(&f.path) {
            let text = read_normalized(&f.path);
            if let Some(t) = &text {
                results.hashes.insert(f.path.clone(), text_hash(t));
            }
            texts.insert(f.path.clone(), text);
        }
    }
    crate::analysis::baseline::mark(&spec.root, spec.baseline.as_deref(), &mut findings, &texts);
    results.baseline = spec.baseline.as_ref().map(|b| b.path.clone());
    results.findings = findings;
    let (rewrites, cleanup_errors) = match spec.kind {
        JobKind::Inspect => (Vec::new(), Vec::new()),
        JobKind::Cleanup => fixes::cleanup_rewrites(spec, &results.findings, cancel, &progress),
    };
    let cancelled = cancel.load(Ordering::Relaxed);
    JobOutput { results, rewrites, cleanup_errors, cancelled }
}

/// Keep findings in scope, drop duplicates (`--all-targets` reports a lint once per target),
/// sort by file and position, cap the count and number them.
pub(crate) fn finalize(findings: Vec<Finding>, in_scope: impl Fn(&Path) -> bool) -> (Vec<Finding>, bool) {
    let mut seen = HashSet::new();
    let mut out: Vec<Finding> = findings
        .into_iter()
        .filter(|f| in_scope(&f.path))
        .filter(|f| {
            seen.insert((
                f.tool,
                f.path.clone(),
                f.start.line,
                f.start.column,
                f.end.line,
                f.end.column,
                f.inspection.clone(),
                f.message.clone(),
            ))
        })
        .collect();
    out.sort_by(|a, b| {
        (&a.path, a.start.line, a.start.column, a.severity).cmp(&(&b.path, b.start.line, b.start.column, b.severity))
    });
    let truncated = out.len() > MAX_FINDINGS;
    out.truncate(MAX_FINDINGS);
    for (i, f) in out.iter_mut().enumerate() {
        f.id = i;
    }
    (out, truncated)
}

pub(crate) fn read_normalized(path: &Path) -> Option<String> {
    codecraft_fs::read_text_file(path).ok().map(|t| codecraft_text::Document::from_text(&t).text())
}

pub(crate) fn text_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Resolve a path a tool printed: absolute as is, else under `base`.
pub(crate) fn resolve_path(base: &Path, file: &str) -> PathBuf {
    let file = file.strip_prefix("file://").unwrap_or(file);
    let p = Path::new(file);
    crate::normalize_path(&if p.is_absolute() { p.to_path_buf() } else { base.join(p) })
}

/// The documentation link of a lint.
pub fn docs_url(tool: Tool, code: &str, severity: Severity) -> Option<String> {
    let code = code.trim();
    if code.is_empty() {
        return None;
    }
    match tool {
        Tool::Clippy | Tool::CargoCheck => {
            if let Some(lint) = code.strip_prefix("clippy::") {
                Some(format!("https://rust-lang.github.io/rust-clippy/master/index.html#{lint}"))
            } else if code.len() == 5 && code.starts_with('E') && code.get(1..).is_some_and(|d| d.chars().all(|c| c.is_ascii_digit())) {
                Some(format!("https://doc.rust-lang.org/error_codes/{code}.html"))
            } else if code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                let page = if severity == Severity::Error { "deny-by-default" } else { "warn-by-default" };
                Some(format!("https://doc.rust-lang.org/rustc/lints/listing/{page}.html#{}", code.replace('_', "-")))
            } else {
                None
            }
        }
        Tool::Ruff => Some(format!("https://docs.astral.sh/ruff/rules/#{code}")),
        Tool::Eslint => (!code.contains('/')).then(|| format!("https://eslint.org/docs/latest/rules/{code}")),
        Tool::ClangTidy => {
            if code.starts_with("clang-analyzer-") {
                Some("https://clang.llvm.org/docs/analyzer/checkers.html".into())
            } else if code.starts_with("clang-diagnostic-") {
                None
            } else {
                let (group, rest) = code.split_once('-')?;
                Some(format!("https://clang.llvm.org/extra/clang-tidy/checks/{group}/{rest}.html"))
            }
        }
        Tool::ZigFmt => None,
        Tool::Mypy => Some(format!("https://mypy.readthedocs.io/en/stable/error_code_list.html#code-{code}")),
        Tool::Pyright => Some(format!("https://microsoft.github.io/pyright/#/configuration?id={code}")),
        Tool::CargoAudit | Tool::CargoDeny => {
            code.starts_with("RUSTSEC-").then(|| format!("https://rustsec.org/advisories/{code}.html"))
        }
        Tool::RustAnalyzer => {
            if code.starts_with("clippy::") || code.starts_with('E') || code.contains('_') {
                docs_url(Tool::Clippy, code, severity)
            } else {
                Some(format!("https://rust-analyzer.github.io/book/diagnostics.html#{code}"))
            }
        }
        Tool::ClangAnalyzer => Some("https://clang.llvm.org/docs/analyzer/checkers.html".into()),
        Tool::Clangd => match code.strip_prefix("-W") {
            Some(flag) => Some(format!("https://clang.llvm.org/docs/DiagnosticsReference.html#w{flag}")),
            None if code.contains('-') && !code.starts_with("clang-diagnostic") => docs_url(Tool::ClangTidy, code, severity),
            None => None,
        },
        Tool::Cppcheck | Tool::Sql => None,
    }
}

/// The suppress action's label: "Suppress 'clippy::x' for item", "Suppress with # noqa: F401", …
pub fn suppress_label(f: &Finding) -> String {
    let code = f.code.as_deref().unwrap_or(&f.inspection);
    match f.tool {
        Tool::Clippy | Tool::CargoCheck => format!("Suppress '{code}' for item"),
        Tool::Ruff => format!("Suppress with # noqa: {code}"),
        Tool::Eslint => format!("Suppress '{code}' for line"),
        Tool::ClangTidy => format!("Suppress with NOLINT({code})"),
        Tool::ZigFmt => "Suppress".to_string(),
        Tool::Mypy => format!("Suppress with # type: ignore[{code}]"),
        Tool::Pyright => format!("Suppress with # pyright: ignore[{code}]"),
        Tool::CargoAudit | Tool::CargoDeny => format!("Ignore {code} in the tool's configuration"),
        Tool::RustAnalyzer => format!("Suppress '{code}' for item"),
        Tool::ClangAnalyzer | Tool::Clangd => format!("Suppress with NOLINT({code})"),
        Tool::Cppcheck => format!("Suppress with // cppcheck-suppress {code}"),
        Tool::Sql => format!("Suppress with -- noinspection {code}"),
    }
}

/// The documentation link of a finding.
pub fn finding_url(f: &Finding) -> Option<String> {
    f.url.clone().or_else(|| f.code.as_deref().and_then(|c| docs_url(f.tool, c, f.severity)))
}

// ---------------------------------------------------------------- tool window actions

/// Navigate to a finding.
pub(crate) fn navigate(s: &mut Session, id: usize) -> Result<(), CommandError> {
    let f = s
        .inspections
        .results
        .as_ref()
        .and_then(|r| r.finding(id))
        .cloned()
        .ok_or_else(|| CommandError::BadParams(format!("no finding {id}")))?;
    s.inspections.selected = Some(id);
    s.navigate_to_position(&f.path, f.start.line, f.start.column)
}

/// The findings a "fix all" covers: one inspection's (or every one), open and with a fix of the
/// accepted kind.
pub(crate) fn fixable(results: &InspectionResults, inspection: Option<&str>, maybe_incorrect: bool) -> Vec<(usize, Fix)> {
    results
        .open()
        .filter(|f| inspection.is_none_or(|i| f.inspection == i))
        .filter_map(|f| {
            let fix = f.safe_fix().or_else(|| {
                maybe_incorrect.then(|| f.fixes.iter().find(|x| x.applicability.is_offered())).flatten()
            })?;
            Some((f.id, fix.clone()))
        })
        .collect()
}

/// Whether a finding's file still has the text it had when it was inspected.
pub(crate) fn is_fresh(s: &Session, results: &InspectionResults, path: &Path) -> bool {
    match (results.hashes.get(path), s.current_text(path)) {
        (Some(h), Some(text)) => *h == text_hash(&text),
        _ => false,
    }
}

/// The tool window's tree: (inspection, [(file, [finding ids])]).
pub type ResultTree = Vec<(String, Vec<(PathBuf, Vec<usize>)>)>;

/// The tool window's tree: inspection → file → findings (open and passing the severity
/// filter), the most severe inspections first.
pub fn tree(state: &InspectionsState) -> ResultTree {
    let Some(results) = &state.results else { return Vec::new() };
    let mut groups: BTreeMap<(Severity, String), BTreeMap<PathBuf, Vec<usize>>> = BTreeMap::new();
    let mut worst: HashMap<String, Severity> = HashMap::new();
    for f in results.open().filter(|f| state.visible(f)) {
        let w = worst.entry(f.inspection.clone()).or_insert(f.severity);
        if f.severity < *w {
            *w = f.severity;
        }
    }
    for f in results.open().filter(|f| state.visible(f)) {
        let sev = worst.get(&f.inspection).copied().unwrap_or(f.severity);
        groups.entry((sev, f.inspection.clone())).or_default().entry(f.path.clone()).or_default().push(f.id);
    }
    groups.into_iter().map(|((_, name), files)| (name, files.into_iter().collect())).collect()
}

/// A top-level node of the grouped results tree (M14 CA).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Group {
    pub label: String,
    /// The file of a group by file.
    pub path: Option<PathBuf>,
    /// The most severe problem under it.
    pub worst: Severity,
    pub children: Vec<SubGroup>,
}

/// A second-level node: a file (`path`) or a rule, with its problems.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SubGroup {
    pub label: String,
    pub path: Option<PathBuf>,
    pub ids: Vec<usize>,
}

/// The results tree grouped as [`InspectionsState::group_by`] says, most severe groups first
/// (files: by path), only visible problems.
pub fn grouped(state: &InspectionsState) -> Vec<Group> {
    let Some(results) = &state.results else { return Vec::new() };
    let visible: Vec<&Finding> = results.open().filter(|f| state.visible(f)).collect();
    // (top key, sub key) → ids; top sort key carries the worst severity.
    type Subs = BTreeMap<(String, Option<PathBuf>), Vec<usize>>;
    let mut top: BTreeMap<String, (Severity, Subs)> = BTreeMap::new();
    for f in &visible {
        let (key, sub) = match state.group_by {
            GroupBy::Rule => (f.inspection.clone(), (file_label(&f.path), Some(f.path.clone()))),
            GroupBy::Severity => (f.severity.label().to_string(), (file_label(&f.path), Some(f.path.clone()))),
            GroupBy::Analyzer => (f.tool.label().to_string(), (f.inspection.clone(), None)),
            GroupBy::File => (f.path.display().to_string(), (f.inspection.clone(), None)),
        };
        let e = top.entry(key).or_insert((f.severity, BTreeMap::new()));
        if f.severity < e.0 {
            e.0 = f.severity;
        }
        e.1.entry(sub).or_default().push(f.id);
    }
    let mut out: Vec<Group> = top
        .into_iter()
        .map(|(key, (worst, subs))| {
            let path = (state.group_by == GroupBy::File).then(|| PathBuf::from(&key));
            let label = match state.group_by {
                GroupBy::File => file_label(Path::new(&key)),
                GroupBy::Severity => crate::analysis::profiles::Level::of(worst).label().to_string(),
                _ => key,
            };
            let mut children: Vec<SubGroup> =
                subs.into_iter().map(|((label, path), ids)| SubGroup { label, path, ids }).collect();
            if state.group_by == GroupBy::File {
                children.sort_by_key(|c| c.ids.iter().filter_map(|id| results.finding(*id)).map(|f| f.severity).min());
            }
            Group { label, path, worst, children }
        })
        .collect();
    match state.group_by {
        // Files keep path order (BTreeMap); everything else most severe first, stable by name.
        GroupBy::File => {}
        _ => out.sort_by(|a, b| a.worst.cmp(&b.worst).then_with(|| a.label.cmp(&b.label))),
    }
    out
}

fn file_label(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn finding(path: &str, line: usize, inspection: &str, severity: Severity) -> Finding {
        let mut f = Finding::new(Tool::Clippy, inspection, severity, PathBuf::from(path), Position::new(line, 0));
        f.message = format!("{inspection} at {line}");
        f
    }

    #[test]
    fn finalize_filters_dedups_sorts_and_numbers() {
        let fs = vec![
            finding("/p/b.rs", 3, "clippy::a", Severity::Warning),
            finding("/p/a.rs", 9, "clippy::a", Severity::Warning),
            finding("/p/a.rs", 9, "clippy::a", Severity::Warning),
            finding("/elsewhere/c.rs", 1, "clippy::a", Severity::Warning),
            finding("/p/a.rs", 2, "E0308", Severity::Error),
        ];
        let (out, truncated) = finalize(fs, |p| p.starts_with("/p"));
        assert!(!truncated);
        let got: Vec<(String, usize, usize)> =
            out.iter().map(|f| (f.path.display().to_string(), f.start.line, f.id)).collect();
        assert_eq!(got, [("/p/a.rs".into(), 2, 0), ("/p/a.rs".into(), 9, 1), ("/p/b.rs".into(), 3, 2)]);
    }

    #[test]
    fn finalize_caps() {
        let fs: Vec<Finding> =
            (0..MAX_FINDINGS + 5).map(|i| finding("/p/a.rs", i, "clippy::a", Severity::Warning)).collect();
        let (out, truncated) = finalize(fs, |_| true);
        assert!(truncated);
        assert_eq!(out.len(), MAX_FINDINGS);
        assert_eq!(out.last().unwrap().id, MAX_FINDINGS - 1);
    }

    #[test]
    fn tree_groups_by_inspection_then_file_and_filters() {
        let fs = vec![
            finding("/p/a.rs", 1, "clippy::needless_return", Severity::Warning),
            finding("/p/b.rs", 1, "clippy::needless_return", Severity::Warning),
            finding("/p/a.rs", 5, "clippy::needless_return", Severity::Warning),
            finding("/p/a.rs", 2, "E0308", Severity::Error),
        ];
        let (findings, _) = finalize(fs, |_| true);
        let mut state = InspectionsState {
            results: Some(InspectionResults {
                findings,
                ..InspectionResults::new(JobKind::Inspect, Scope::new(ScopeKind::Project, None), "Project")
            }),
            ..InspectionsState::default()
        };
        // Grouped views (M14 CA): by severity errors first, by analyzer one group, by file per path.
        state.group_by = GroupBy::Severity;
        let g = grouped(&state);
        assert_eq!(g.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), ["Error", "Warning"]);
        assert_eq!(g[1].children.len(), 2, "two files with warnings");
        state.group_by = GroupBy::Analyzer;
        let g = grouped(&state);
        assert_eq!((g.len(), g[0].label.as_str(), g[0].children.len()), (1, "Clippy", 2));
        state.group_by = GroupBy::File;
        let g = grouped(&state);
        assert_eq!(g.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), ["a.rs", "b.rs"]);
        assert_eq!(g[0].children[0].label, "E0308", "the file's most severe rule first");
        assert_eq!(g[0].path.as_deref(), Some(Path::new("/p/a.rs")));
        state.group_by = GroupBy::Rule;
        let g = grouped(&state);
        assert_eq!(g[0].label, "E0308");
        // Only new problems: baseline ones are hidden.
        state.results.as_mut().unwrap().findings[1].baseline = true;
        state.only_new = true;
        assert_eq!(grouped(&state).iter().map(|g| g.children.iter().map(|c| c.ids.len()).sum::<usize>()).sum::<usize>(), 3);
        state.only_new = false;
        let t = tree(&state);
        // Errors first; files sorted; occurrences in order.
        assert_eq!(t[0].0, "E0308");
        assert_eq!(t[1].0, "clippy::needless_return");
        assert_eq!(t[1].1.len(), 2);
        assert_eq!(t[1].1[0].1.len(), 2);
        state.hidden.insert(Severity::Warning);
        assert_eq!(tree(&state).len(), 1);
        state.hidden.clear();
        state.results.as_mut().unwrap().resolved.insert(0);
        assert_eq!(tree(&state).len(), 2);
        assert_eq!(state.results.as_ref().unwrap().counts().get(&Severity::Warning), Some(&2));
    }

    #[test]
    fn docs_urls() {
        assert_eq!(
            docs_url(Tool::Clippy, "clippy::needless_return", Severity::Warning).as_deref(),
            Some("https://rust-lang.github.io/rust-clippy/master/index.html#needless_return")
        );
        assert_eq!(
            docs_url(Tool::Clippy, "E0308", Severity::Error).as_deref(),
            Some("https://doc.rust-lang.org/error_codes/E0308.html")
        );
        assert_eq!(
            docs_url(Tool::CargoCheck, "unused_variables", Severity::Warning).as_deref(),
            Some("https://doc.rust-lang.org/rustc/lints/listing/warn-by-default.html#unused-variables")
        );
        assert_eq!(docs_url(Tool::Eslint, "react/jsx-key", Severity::Warning), None);
        assert_eq!(
            docs_url(Tool::Eslint, "no-unused-vars", Severity::Error).as_deref(),
            Some("https://eslint.org/docs/latest/rules/no-unused-vars")
        );
        assert_eq!(
            docs_url(Tool::ClangTidy, "bugprone-use-after-move", Severity::Warning).as_deref(),
            Some("https://clang.llvm.org/extra/clang-tidy/checks/bugprone/use-after-move.html")
        );
        assert_eq!(docs_url(Tool::Clippy, "", Severity::Warning), None);
        assert_eq!(docs_url(Tool::Clippy, "Ü", Severity::Warning), None);
    }

    #[test]
    fn parse_names() {
        assert_eq!(ScopeKind::parse("Project"), Some(ScopeKind::Project));
        assert_eq!(ScopeKind::parse("uncommitted"), Some(ScopeKind::Uncommitted));
        assert_eq!(ScopeKind::parse("nope"), None);
        assert_eq!(Tool::parse("clang-tidy"), Some(Tool::ClangTidy));
        assert_eq!(Tool::parse("zig fmt"), Some(Tool::ZigFmt));
        assert_eq!(Applicability::parse("MachineApplicable"), Applicability::MachineApplicable);
        assert_eq!(Applicability::parse("unsafe"), Applicability::MaybeIncorrect);
        assert_eq!(Applicability::parse("display-only"), Applicability::Unspecified);
    }

    #[test]
    fn resolve_paths() {
        assert_eq!(resolve_path(Path::new("/w"), "src/main.rs"), PathBuf::from("/w/src/main.rs"));
        assert_eq!(resolve_path(Path::new("/w"), "/abs/x.rs"), PathBuf::from("/abs/x.rs"));
        assert_eq!(resolve_path(Path::new("/w"), "./a/../b.py"), PathBuf::from("/w/b.py"));
    }

    /// QA M14: Inspect Code on a CMake project analysed CMake's own compiler-check sources.
    #[test]
    fn build_output_is_not_in_scope() {
        for p in [
            "cmake-build-debug/CMakeFiles/4.3.0/CompilerIdCXX/CMakeCXXCompilerId.cpp",
            "build/CMakeFiles/x.c",
            "web/node_modules/a/index.js",
            "pkg/__pycache__/m.py",
            ".venv/lib/site.py",
        ] {
            assert!(super::in_build_output(Path::new(p)), "{p}");
        }
        for p in ["main.cpp", "src/build.rs", "cmake-build-notes.md", "src/venv.py", "docs/CMakeFiles.md"] {
            assert!(!super::in_build_output(Path::new(p)), "{p}");
        }
    }
}
