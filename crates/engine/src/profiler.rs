//! Profiling runs and snapshots shown in the Profiler tool window (M11 track P, M14 track PF).
//!
//! Run › Profile runs the selected run configuration under a profiler in a Run tab. The
//! configuration is planned first ([`crate::profiler_plan`]): Cargo configurations and the
//! build part of shell chains (CMake Application) are built in a Build tab, then only the
//! program is recorded. CPU: Instruments' Time Profiler through `xctrace` on macOS, samply,
//! `perf record -g` on Linux, py-spy (not on macOS, where it needs root) or cProfile for Python.
//! Memory: `leaks --atExit` + `malloc_history` on macOS, heaptrack on Linux, memray or
//! tracemalloc for Python. Attach Profiler to Process… records a running process.
//!
//! Every recording is a job (`profiler.record` → job id, `profiler.job` to poll). When it ends,
//! the output is converted ([`crate::profiler_convert`]) and parsed on a worker thread
//! ([`codecraft_profile`]) and opens in the Profiler tool window; loaded profiles stay in a
//! small cache for agents' reports and comparisons. Stop sends SIGINT to the recording's
//! process group (like Ctrl+C in a terminal, so the profiler still writes its file) and kills it
//! if it hangs.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

use codecraft_cargo::{CargoMessage, RunEvent, RunHandle};
use codecraft_dap::processes::ProcessInfo;
use codecraft_profile::{Analysis, Edge, Profile, Selection};
use codecraft_workspace::{ConsoleKind, RunStatus, RunTabId, ToolWindowId};

pub use codecraft_profile as profile;

pub use crate::profiler_plan::ProfileKind;

use crate::debug::AttachPicker;
use crate::profiler_convert::{Convert, Converted};
use crate::profiler_plan::{BuildStep, Plan, Target};
use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

/// Recent snapshots remembered.
pub const MAX_RECENT: usize = 10;
/// Profiles kept loaded for reports and comparisons.
pub const MAX_CACHED: usize = 6;
/// Jobs remembered.
pub const MAX_JOBS: usize = 30;
/// How long a stopped recording may take to write its snapshot before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(15);
/// The helper script run by Python interpreters (cProfile conversion, tracemalloc).
const HELPER: &str = include_str!("profiler_helper.py");

/// A profiler CodeCraft can drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfilerTool {
    Samply,
    Perf,
    PySpy,
    /// Instruments' Time Profiler (`xctrace record`, macOS; comes with Xcode).
    Xctrace,
    /// Python's built-in deterministic profiler.
    #[serde(rename = "cProfile")]
    CProfile,
    /// Python's built-in allocation tracer (through CodeCraft's helper).
    Tracemalloc,
    /// Bloomberg's memray (Python memory).
    Memray,
    /// `leaks --atExit` with malloc stack logging (macOS native memory).
    Leaks,
    /// heaptrack (Linux native memory).
    Heaptrack,
}

impl ProfilerTool {
    pub const ALL: [ProfilerTool; 9] = [
        Self::Xctrace,
        Self::Samply,
        Self::Perf,
        Self::PySpy,
        Self::CProfile,
        Self::Tracemalloc,
        Self::Memray,
        Self::Leaks,
        Self::Heaptrack,
    ];

    pub fn program(self) -> &'static str {
        match self {
            Self::Samply => "samply",
            Self::Perf => "perf",
            Self::PySpy => "py-spy",
            Self::Xctrace => "xctrace",
            Self::CProfile => "cProfile",
            Self::Tracemalloc => "tracemalloc",
            Self::Memray => "memray",
            Self::Leaks => "leaks",
            Self::Heaptrack => "heaptrack",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        let n = name.trim().to_ascii_lowercase().replace(['-', '_', ' '], "");
        Self::ALL.into_iter().find(|t| t.program().to_ascii_lowercase().replace('-', "") == n).or(match n.as_str() {
            "instruments" | "timeprofiler" => Some(Self::Xctrace),
            "malloc" | "mallochistory" => Some(Self::Leaks),
            _ => None,
        })
    }

    /// CPU or memory.
    pub fn kind(self) -> ProfileKind {
        match self {
            Self::Tracemalloc | Self::Memray | Self::Leaks | Self::Heaptrack => ProfileKind::Memory,
            _ => ProfileKind::Cpu,
        }
    }

    /// Profiles Python programs only.
    pub fn python_only(self) -> bool {
        matches!(self, Self::PySpy | Self::CProfile | Self::Tracemalloc | Self::Memray)
    }

    /// Part of Python itself (always available with an interpreter).
    pub fn builtin(self) -> bool {
        matches!(self, Self::CProfile | Self::Tracemalloc)
    }

    /// How to install it.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Samply => "Install samply with `cargo install --locked samply` (macOS and Linux).",
            Self::Perf => "Install perf with your distribution's package (linux-tools / linux-perf).",
            Self::PySpy => {
                "Install py-spy with `uv tool install py-spy` (Install puts it in CodeCraft's tools folder); on macOS it needs root."
            }
            Self::Xctrace => {
                "xctrace comes with Xcode (macOS); without it, install samply with `cargo install --locked samply`."
            }
            Self::CProfile | Self::Tracemalloc => "Part of Python: needs a Python interpreter.",
            Self::Memray => "Install memray into the project's environment (`uv pip install memray`), Linux and macOS.",
            Self::Leaks => "leaks and malloc_history come with macOS's developer tools (xcode-select --install).",
            Self::Heaptrack => "Install heaptrack with your distribution's package (Linux).",
        }
    }

    /// Whether there's an install command (see [`ProfilerState::install_command`]).
    pub fn install_command(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::Samply => Some(("cargo", &["install", "--locked", "samply"])),
            Self::PySpy => Some(("uv", &["tool", "install", "py-spy"])),
            Self::Memray => Some(("uv", &["pip", "install", "memray"])),
            _ => None,
        }
    }

    /// The extension of the file it writes.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Samply => "json",
            Self::Perf => "perf.data",
            Self::PySpy => "speedscope.json",
            Self::Xctrace => "trace",
            Self::CProfile => "pstats",
            Self::Tracemalloc => "tracemalloc.json",
            Self::Memray => "memray.bin",
            Self::Leaks => "memgraph",
            Self::Heaptrack => "heaptrack",
        }
    }
}

/// A recording in progress (its process runs in a Run tab).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    pub tab: RunTabId,
    pub tool: ProfilerTool,
    pub title: String,
    /// Where the profiler writes the snapshot.
    pub output: PathBuf,
    /// The process recorded by Attach Profiler to Process.
    pub attached: Option<u32>,
    pub started: Instant,
    /// When Stop was asked for.
    pub stop_requested: Option<Instant>,
    /// The job it belongs to.
    pub job: u64,
    /// Stop by itself then (`seconds`).
    pub deadline: Option<Instant>,
    /// What turns the output into a snapshot.
    pub convert: Convert,
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Building,
    Recording,
    Loading,
    Done,
    Failed,
}

/// A profiling run (or a snapshot load) agents can poll.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: u64,
    pub title: String,
    pub kind: ProfileKind,
    pub tool: Option<ProfilerTool>,
    pub state: JobState,
    pub tab: Option<RunTabId>,
    /// The snapshot (once loaded).
    pub snapshot: Option<PathBuf>,
    pub error: Option<String>,
    pub started: Instant,
    pub finished: Option<Instant>,
    /// A Cargo debug build was profiled.
    pub debug_build: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfilerTab {
    #[default]
    FlameGraph,
    /// The flame graph upside down (roots on top).
    Icicle,
    CallTree,
    MethodList,
    /// Thread activity over time.
    Timeline,
    /// Memory over time and the top allocations at the peak (tracemalloc).
    Memory,
}

impl ProfilerTab {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "flameGraph" | "flame" => Some(Self::FlameGraph),
            "icicle" => Some(Self::Icicle),
            "callTree" | "tree" => Some(Self::CallTree),
            "methodList" | "methods" => Some(Self::MethodList),
            "timeline" | "threads" => Some(Self::Timeline),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MethodColumn {
    Name,
    #[default]
    SelfWeight,
    Total,
}

impl MethodColumn {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "name" => Some(Self::Name),
            "self" => Some(Self::SelfWeight),
            "total" => Some(Self::Total),
            _ => None,
        }
    }
}

/// Weight on one source line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineHit {
    /// 1-based.
    pub line: u32,
    pub self_weight: u64,
    pub total: u64,
}

/// Line statistics by source file (for the editor gutter and `profiler.lineHits`).
#[derive(Clone, Debug, Default)]
pub struct LineIndex {
    files: HashMap<String, Vec<LineHit>>,
}

impl LineIndex {
    pub fn build(p: &Profile) -> Self {
        Self::from_lines(p, &p.lines)
    }

    /// The index of a selection's lines (see [`Analysis::lines`]).
    pub fn for_analysis(p: &Profile, a: &Analysis) -> Self {
        Self::from_lines(p, a.lines(p))
    }

    pub fn from_lines(p: &Profile, lines: &[codecraft_profile::LineStat]) -> Self {
        let mut acc: HashMap<String, HashMap<u32, (u64, u64)>> = HashMap::new();
        for l in lines {
            let Some(file) = p.frame(l.frame).and_then(|f| f.file.as_ref()) else { continue };
            let e = acc.entry(file.clone()).or_default().entry(l.line).or_insert((0, 0));
            e.0 = e.0.saturating_add(l.self_weight);
            e.1 = e.1.saturating_add(l.total);
        }
        let files = acc
            .into_iter()
            .map(|(f, lines)| {
                let mut v: Vec<LineHit> =
                    lines.into_iter().map(|(line, (s, t))| LineHit { line, self_weight: s, total: t }).collect();
                v.sort_by_key(|h| h.line);
                (f, v)
            })
            .collect();
        Self { files }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The lines of a file: the profile's path as given, or a relative/base name the path
    /// ends with.
    pub fn for_path(&self, path: &Path) -> Option<&[LineHit]> {
        if self.files.is_empty() {
            return None;
        }
        let text = path.to_string_lossy();
        if let Some(v) = self.files.get(text.as_ref()) {
            return Some(v);
        }
        let private = text.strip_prefix("/private").unwrap_or(&text);
        self.files
            .iter()
            .filter(|(k, _)| {
                k.as_str() == private
                    || k.strip_prefix("/private") == Some(private)
                    || (!k.starts_with('/') && !k.is_empty() && path.ends_with(k.as_str()))
            })
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| v.as_slice())
    }
}

type FunctionKey = (String, Option<String>, Option<String>);

/// A snapshot compared against (the diff flame graph and the method list's Δ column).
#[derive(Clone, Debug)]
pub struct Baseline {
    pub path: PathBuf,
    pub total: u64,
    weights: Arc<HashMap<u64, u64>>,
    methods: Arc<HashMap<FunctionKey, (u64, u64)>>,
    /// The baseline's weight for each call node of the open snapshot's tree (same path).
    pub node_weights: Vec<u64>,
}

impl Baseline {
    /// The baseline's (self, total) of a function.
    pub fn method(&self, f: &codecraft_profile::Frame) -> (u64, u64) {
        self.methods.get(&(f.name.clone(), f.module.clone(), f.file.clone())).copied().unwrap_or((0, 0))
    }

    pub fn percent(&self, weight: u64) -> f64 {
        if self.total == 0 { 0.0 } else { weight as f64 * 100.0 / self.total as f64 }
    }
}

/// An open snapshot and its view state.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub path: PathBuf,
    pub profile: Arc<Profile>,
    pub analysis: Arc<Analysis>,
    /// Flame graph zoomed into this call node.
    pub zoom: Option<u32>,
    pub search: String,
    /// Per frame: matches the search.
    pub hits: Vec<bool>,
    /// Weight of the samples through a matching frame.
    pub matched: u64,
    /// Expanded call tree nodes.
    pub expanded: BTreeSet<u32>,
    /// Selected call tree node.
    pub selected: Option<u32>,
    pub sort: MethodColumn,
    pub descending: bool,
    /// Indices into `analysis.methods`, in display order.
    pub method_order: Vec<usize>,
    /// Selected method (a frame id).
    pub method: Option<u32>,
    /// Callers and callees of the selected method.
    pub method_edges: (Vec<Edge>, Vec<Edge>),
    /// Per-line weights by file, of the selected thread and time range.
    pub lines: Arc<LineIndex>,
    /// Compared against.
    pub baseline: Option<Baseline>,
    /// The thread and time range asked for (the analysis catches up on a worker thread).
    pub selection: Selection,
}

impl Snapshot {
    fn new(path: PathBuf, profile: Arc<Profile>, analysis: Arc<Analysis>) -> Self {
        let lines = Arc::new(LineIndex::default());
        let selection = analysis.selection();
        let mut s = Self {
            path,
            profile,
            analysis,
            zoom: None,
            search: String::new(),
            hits: Vec::new(),
            matched: 0,
            expanded: BTreeSet::new(),
            selected: None,
            sort: MethodColumn::SelfWeight,
            descending: true,
            method_order: Vec::new(),
            method: None,
            method_edges: (Vec::new(), Vec::new()),
            lines,
            baseline: None,
            selection,
        };
        s.reset_view();
        s
    }

    /// After (re)analysis: keep the search, reset positions that refer to old node ids.
    fn reset_view(&mut self) {
        self.lines = Arc::new(LineIndex::for_analysis(&self.profile, &self.analysis));
        self.zoom = None;
        self.selected = None;
        self.expanded.clear();
        // Open the heaviest path a few levels, like a profiler's default view.
        let tree = &self.analysis.tree;
        let mut cur = tree.roots.first().copied();
        let mut depth = 0;
        while let Some(id) = cur {
            self.expanded.insert(id);
            depth += 1;
            if depth >= 8 {
                break;
            }
            cur = tree.node(id).and_then(|n| n.children.first().copied());
        }
        self.set_search(&self.search.clone());
        if let Some(m) = self.method {
            self.select_method(m);
        }
        self.sort_methods();
        self.map_baseline();
    }

    /// The baseline's weight per call node of the current tree.
    fn map_baseline(&mut self) {
        let Some(b) = &mut self.baseline else { return };
        let hashes = codecraft_profile::path_hashes(&self.profile, &self.analysis.tree);
        b.node_weights = hashes.iter().map(|h| b.weights.get(h).copied().unwrap_or(0)).collect();
    }

    pub fn set_search(&mut self, query: &str) {
        self.search = query.to_string();
        self.hits = codecraft_profile::search(&self.profile, query);
        self.matched = codecraft_profile::matched_weight(&self.analysis.tree, &self.hits);
    }

    /// Select a method (a frame id) and compute its callers and callees.
    pub fn select_method(&mut self, frame: u32) {
        let tree = &self.analysis.tree;
        self.method_edges = (codecraft_profile::callers(tree, frame), codecraft_profile::callees(tree, frame));
        self.method = Some(frame);
    }

    pub fn is_hit(&self, frame: u32) -> bool {
        self.hits.get(frame as usize).copied().unwrap_or(false)
    }

    pub fn sort_methods(&mut self) {
        let methods = &self.analysis.methods;
        let p = &self.profile;
        let mut order: Vec<usize> = (0..methods.len()).collect();
        let name = |i: usize| methods.get(i).and_then(|m| p.frame(m.frame)).map_or("", |f| f.name.as_str());
        let weight = |i: usize| {
            methods.get(i).map_or(0, |m| if self.sort == MethodColumn::Total { m.total } else { m.self_weight })
        };
        let (column, descending) = (self.sort, self.descending);
        order.sort_by(|a, b| {
            let o = match column {
                MethodColumn::Name => name(*a).cmp(name(*b)),
                _ => weight(*a).cmp(&weight(*b)),
            };
            let o = if descending { o.reverse() } else { o };
            o.then_with(|| name(*a).cmp(name(*b))).then(a.cmp(b))
        });
        self.method_order = order;
    }

    /// The heaviest outermost call node of a frame (for "focus on method").
    pub fn heaviest_node(&self, frame: u32) -> Option<u32> {
        let hits: Vec<bool> = (0..self.profile.frames.len()).map(|f| f == frame as usize).collect();
        codecraft_profile::outermost_matches(&self.analysis.tree, &hits).first().copied()
    }

    /// Title for the tool window: the file name.
    pub fn title(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    /// The selection's line statistics of a method (the Method List's hot lines).
    pub fn lines_of(&self, frame: u32) -> &[codecraft_profile::LineStat] {
        self.analysis.lines_of(&self.profile, frame)
    }

    /// The whole snapshot at the memory peak, written next to this one (tracemalloc with
    /// periodic snapshots).
    pub fn peak_snapshot(&self) -> Option<PathBuf> {
        peak_snapshot_path(&self.path, &self.profile)
    }
}

/// The file with the snapshot at the memory peak, next to a snapshot (when it exists).
pub fn peak_snapshot_path(path: &Path, p: &Profile) -> Option<PathBuf> {
    let name = p.memory.as_ref()?.peak_snapshot_file.as_ref()?;
    let file = path.parent()?.join(name);
    (file != path && file.is_file()).then_some(file)
}

/// Milliseconds (as agents and the UI give them) to nanoseconds.
pub fn ms_to_ns(ms: f64) -> u64 {
    if ms.is_nan() || ms <= 0.0 {
        0
    } else if ms >= (u64::MAX / 1_000_000) as f64 {
        u64::MAX
    } else {
        (ms * 1e6).round() as u64
    }
}

/// Why a snapshot is being loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// Open in the tool window.
    Open,
    /// Another thread selection of the open snapshot.
    Reanalyse,
    /// Into the cache only (agents' reports).
    Cache,
    /// Compare the open snapshot against it.
    Baseline,
}

struct BaselineData {
    weights: HashMap<u64, u64>,
    methods: HashMap<FunctionKey, (u64, u64)>,
    total: u64,
}

/// A finished background load or analysis.
struct Loaded {
    path: PathBuf,
    profile: Arc<Profile>,
    analysis: Analysis,
    baseline: Option<BaselineData>,
}

struct LoadJob {
    rx: Receiver<Result<Loaded, String>>,
    path: PathBuf,
    job: Option<u64>,
    purpose: Purpose,
}

/// A loaded profile kept for reports.
#[derive(Clone, Debug)]
pub struct Cached {
    pub path: PathBuf,
    pub profile: Arc<Profile>,
    pub analysis: Arc<Analysis>,
}

/// A build before a recording.
struct PendingBuild {
    job: u64,
    tab: RunTabId,
    plan: Plan,
    tool: ProfilerTool,
    tool_path: Option<PathBuf>,
    seconds: Option<u64>,
    cargo: Option<CargoBuild>,
}

struct CargoBuild {
    handle: RunHandle,
    events: Receiver<RunEvent>,
    executables: Vec<(String, PathBuf)>,
    pick: Option<String>,
}

#[derive(Default)]
pub struct ProfilerState {
    /// Explicit directory (tests and embedders); otherwise `$CODECRAFT_HOME/profiler`.
    pub dir_override: Option<PathBuf>,
    /// Directories searched for the profilers instead of `PATH` (tests).
    pub tool_dirs: Option<Vec<PathBuf>>,
    /// Use `perf` when samply is missing (Linux only by default).
    pub perf_allowed: Option<bool>,
    /// Prefer xctrace for native CPU profiles (macOS only by default).
    pub xctrace_allowed: Option<bool>,
    pub snapshot: Option<Snapshot>,
    /// A snapshot being parsed (or reanalysed) for the tool window.
    pub loading: Option<PathBuf>,
    loads: Vec<LoadJob>,
    pub error: Option<String>,
    /// How to install a missing profiler (shown with an Install button).
    pub missing: Option<ProfilerTool>,
    recent: Option<Vec<PathBuf>>,
    pub recordings: Vec<Recording>,
    builds: Vec<PendingBuild>,
    pub jobs: Vec<Job>,
    next_job: u64,
    cache: Vec<Cached>,
    /// Attach Profiler to Process… list.
    pub attach: Option<AttachPicker>,
    attach_rx: Option<Receiver<Result<Vec<ProcessInfo>, String>>>,
    pub tab: ProfilerTab,
    /// Open Profiler Snapshot without a path: the UI shows a file dialog.
    pub pick_snapshot: bool,
    /// A baseline being loaded for the open snapshot.
    pub baseline_loading: Option<PathBuf>,
}

impl std::fmt::Debug for ProfilerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfilerState")
            .field("loading", &self.loading)
            .field("recordings", &self.recordings.len())
            .field("builds", &self.builds.len())
            .field("jobs", &self.jobs.len())
            .finish()
    }
}

impl ProfilerState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        !self.loads.is_empty() || !self.recordings.is_empty() || !self.builds.is_empty() || self.attach_rx.is_some()
    }

    /// Whether a recording or its build is running.
    pub fn recording_active(&self) -> bool {
        !self.recordings.is_empty() || !self.builds.is_empty()
    }

    /// Whether a Run/Build tab is a profile's build step still running (a Cargo build runs
    /// outside the Run window's processes, so `run.stop` asks here).
    pub fn builds_in(&self, tab: RunTabId) -> bool {
        self.builds.iter().any(|b| b.tab == tab)
    }

    /// The Run/Build tabs of profile builds in progress, oldest first.
    pub fn build_tabs(&self) -> Vec<RunTabId> {
        self.builds.iter().map(|b| b.tab).collect()
    }

    /// How native addresses in samply snapshots are symbolicated: `atos` on macOS,
    /// `addr2line` (or `llvm-addr2line`) elsewhere; tests point `tool_dirs` at fakes.
    pub fn symbolizers(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        let find = |name: &str| match &self.tool_dirs {
            Some(dirs) => dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()),
            None => codecraft_lsp::which(name),
        };
        let atos = if cfg!(target_os = "macos") || self.tool_dirs.is_some() { find("atos") } else { None };
        let addr2line = find("addr2line").or_else(|| find("llvm-addr2line"));
        (atos, addr2line)
    }

    /// `$CODECRAFT_HOME/profiler` (or `~/.codecraft/profiler`).
    pub fn dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.dir_override {
            return Some(d.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home).join("profiler"));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".codecraft").join("profiler"))
    }

    /// `$CODECRAFT_HOME/tools`: where Install puts profilers (next to the profiler directory).
    pub fn tools_dir(&self) -> Option<PathBuf> {
        Some(self.dir()?.parent()?.join("tools"))
    }

    /// Where a profiler is installed (`None` for the Python built-ins).
    pub fn find_tool(&self, tool: ProfilerTool) -> Option<PathBuf> {
        if tool.builtin() {
            return None;
        }
        let exe = format!("{}{}", tool.program(), std::env::consts::EXE_SUFFIX);
        match &self.tool_dirs {
            Some(dirs) => dirs.iter().map(|d| d.join(&exe)).find(|p| p.is_file()),
            None => self
                .tools_dir()
                .map(|d| d.join("bin").join(&exe))
                .filter(|p| p.is_file())
                .or_else(|| codecraft_lsp::which(tool.program())),
        }
    }

    /// A helper program next to a profiler (`heaptrack_print`), else on `PATH`.
    fn find_companion(&self, tool: ProfilerTool, name: &str) -> Option<PathBuf> {
        self.find_tool(tool).and_then(|p| p.parent().map(|d| d.join(name))).filter(|p| p.is_file()).or_else(|| {
            match &self.tool_dirs {
                Some(dirs) => dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()),
                None => codecraft_lsp::which(name),
            }
        })
    }

    fn perf_allowed(&self) -> bool {
        self.perf_allowed.unwrap_or(cfg!(target_os = "linux"))
    }

    fn xctrace_allowed(&self) -> bool {
        self.xctrace_allowed.unwrap_or(cfg!(target_os = "macos"))
    }

    /// memray for a Python interpreter: `Ok(None)` = `python -m memray` (installed next to the
    /// interpreter), `Ok(Some(exe))` = a memray tool, `Err` = not installed.
    fn memray_for(&self, python: Option<&str>) -> Result<Option<PathBuf>, ()> {
        let beside = python
            .map(Path::new)
            .and_then(Path::parent)
            .map(|d| d.join(format!("memray{}", std::env::consts::EXE_SUFFIX)))
            .is_some_and(|p| p.is_file());
        if beside {
            return Ok(None);
        }
        self.find_tool(ProfilerTool::Memray).map(Some).ok_or(())
    }

    /// The profiler for a program: CPU — xctrace (macOS) or samply, perf on Linux, py-spy (not
    /// on macOS) or cProfile for Python; memory — leaks (macOS) or heaptrack (Linux), memray or
    /// tracemalloc for Python. `forced` picks one. `Err` names the missing profiler.
    pub fn choose(
        &self,
        python: Option<&str>,
        kind: ProfileKind,
        forced: Option<ProfilerTool>,
    ) -> Result<(ProfilerTool, Option<PathBuf>), ProfilerTool> {
        if let Some(t) = forced {
            if t.builtin() {
                return Ok((t, None));
            }
            if t == ProfilerTool::Memray {
                return self.memray_for(python).map(|p| (t, p)).map_err(|()| t);
            }
            return self.find_tool(t).map(|p| (t, Some(p))).ok_or(t);
        }
        let found = |t: ProfilerTool| self.find_tool(t).map(|p| (t, Some(p)));
        match (python.is_some(), kind) {
            (true, ProfileKind::Cpu) => {
                if !cfg!(target_os = "macos")
                    && let Some(t) = found(ProfilerTool::PySpy)
                {
                    return Ok(t);
                }
                Ok((ProfilerTool::CProfile, None))
            }
            (true, ProfileKind::Memory) => match self.memray_for(python) {
                Ok(p) => Ok((ProfilerTool::Memray, p)),
                Err(()) => Ok((ProfilerTool::Tracemalloc, None)),
            },
            (false, ProfileKind::Cpu) => {
                if self.xctrace_allowed()
                    && let Some(t) = found(ProfilerTool::Xctrace)
                {
                    return Ok(t);
                }
                if let Some(t) = found(ProfilerTool::Samply) {
                    return Ok(t);
                }
                if self.perf_allowed()
                    && let Some(t) = found(ProfilerTool::Perf)
                {
                    return Ok(t);
                }
                Err(if self.xctrace_allowed() { ProfilerTool::Xctrace } else { ProfilerTool::Samply })
            }
            (false, ProfileKind::Memory) => {
                if cfg!(target_os = "macos")
                    && let Some(t) = found(ProfilerTool::Leaks)
                {
                    return Ok(t);
                }
                found(ProfilerTool::Heaptrack).ok_or(if cfg!(target_os = "macos") {
                    ProfilerTool::Leaks
                } else {
                    ProfilerTool::Heaptrack
                })
            }
        }
    }

    /// The profiler for a CPU profile of a native or Python program (M11 behaviour).
    pub fn choose_tool(&self, python: bool) -> Result<(ProfilerTool, PathBuf), ProfilerTool> {
        let py = python.then_some("python3");
        match self.choose(py, ProfileKind::Cpu, None) {
            Ok((t, Some(p))) => Ok((t, p)),
            Ok((t, None)) => Ok((t, PathBuf::from("python3"))),
            Err(t) => Err(t),
        }
    }

    /// The command that installs a profiler: py-spy with `uv tool install` into
    /// `$CODECRAFT_HOME/tools`, memray with `uv pip install` into the project's interpreter (else
    /// as a uv tool), samply with `cargo install --root $CODECRAFT_HOME/tools`.
    pub fn install_command(&self, tool: ProfilerTool, python: Option<&Path>) -> Option<(String, Vec<String>)> {
        let tools = self.tools_dir()?;
        let uv_tool = |name: &str| {
            let args = vec![
                format!("UV_TOOL_DIR={}", tools.join("uv").display()),
                format!("UV_TOOL_BIN_DIR={}", tools.join("bin").display()),
                "uv".to_string(),
                "tool".into(),
                "install".into(),
                name.to_string(),
            ];
            ("env".to_string(), args)
        };
        match tool {
            ProfilerTool::PySpy => Some(uv_tool("py-spy")),
            ProfilerTool::Memray => Some(match python {
                Some(py) => (
                    "uv".to_string(),
                    vec!["pip".into(), "install".into(), "--python".into(), py.display().to_string(), "memray".into()],
                ),
                None => uv_tool("memray"),
            }),
            ProfilerTool::Samply => Some((
                "cargo".to_string(),
                vec![
                    "install".into(),
                    "--locked".into(),
                    "--root".into(),
                    tools.display().to_string(),
                    "samply".into(),
                ],
            )),
            _ => None,
        }
    }

    /// Recently opened snapshots, newest first.
    pub fn recent(&mut self) -> &[PathBuf] {
        if self.recent.is_none() {
            let list = self
                .dir()
                .and_then(|d| std::fs::read_to_string(d.join("recent.json")).ok())
                .and_then(|t| serde_json::from_str::<Vec<PathBuf>>(&t).ok())
                .unwrap_or_default();
            self.recent = Some(list.into_iter().take(MAX_RECENT).collect());
        }
        self.recent.as_deref().unwrap_or(&[])
    }

    /// The recent list as last loaded (for drawing; call [`ProfilerState::recent`] once first).
    pub fn recent_cached(&self) -> &[PathBuf] {
        self.recent.as_deref().unwrap_or(&[])
    }

    fn remember(&mut self, path: &Path) {
        let mut list = self.recent().to_vec();
        list.retain(|p| p != path);
        list.insert(0, path.to_path_buf());
        list.truncate(MAX_RECENT);
        self.recent = Some(list.clone());
        self.save_recent(&list);
    }

    pub fn clear_recent(&mut self) {
        if self.recent().is_empty() {
            return;
        }
        self.recent = Some(Vec::new());
        if let Some(dir) = self.dir()
            && let Err(e) = std::fs::remove_file(dir.join("recent.json"))
        {
            log::warn!("cannot clear recent profiler snapshots: {e}");
        }
    }

    fn save_recent(&self, list: &[PathBuf]) {
        let Some(dir) = self.dir() else { return };
        let text = serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".into());
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("recent.json"), text)) {
            log::warn!("cannot save recent profiler snapshots: {e}");
        }
    }

    /// The recording behind a Run tab.
    pub fn recording(&self, tab: RunTabId) -> Option<&Recording> {
        self.recordings.iter().find(|r| r.tab == tab)
    }

    pub fn job(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    fn job_mut(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    fn new_job(&mut self, title: String, kind: ProfileKind, tool: Option<ProfilerTool>, state: JobState) -> u64 {
        self.next_job += 1;
        let id = self.next_job;
        self.jobs.push(Job {
            id,
            title,
            kind,
            tool,
            state,
            tab: None,
            snapshot: None,
            error: None,
            started: Instant::now(),
            finished: None,
            debug_build: false,
        });
        // Forget the oldest finished jobs.
        while self.jobs.len() > MAX_JOBS {
            match self.jobs.iter().position(|j| matches!(j.state, JobState::Done | JobState::Failed)) {
                Some(i) => {
                    self.jobs.remove(i);
                }
                None => break,
            }
        }
        id
    }

    fn fail_job(&mut self, id: Option<u64>, error: &str) {
        if let Some(j) = id.and_then(|id| self.job_mut(id)) {
            j.state = JobState::Failed;
            j.error = Some(error.to_string());
            j.finished = Some(Instant::now());
        }
    }

    /// A loaded profile (the open snapshot or the cache).
    pub fn cached(&self, path: &Path) -> Option<Cached> {
        let same = |p: &Path| p == path || crate::normalize_path(p) == crate::normalize_path(path);
        if let Some(snap) = &self.snapshot
            && same(&snap.path)
            && snap.analysis.thread.is_none()
        {
            return Some(Cached {
                path: snap.path.clone(),
                profile: Arc::clone(&snap.profile),
                analysis: Arc::clone(&snap.analysis),
            });
        }
        self.cache.iter().find(|c| same(&c.path)).cloned()
    }

    fn cache_insert(&mut self, path: PathBuf, profile: Arc<Profile>, analysis: Arc<Analysis>) {
        self.cache.retain(|c| c.path != path);
        self.cache.push(Cached { path, profile, analysis });
        while self.cache.len() > MAX_CACHED {
            self.cache.remove(0);
        }
    }

    /// Whether a load of `path` is in flight (and its job).
    pub fn load_in_flight(&self, path: &Path) -> Option<Option<u64>> {
        self.loads.iter().find(|l| l.path == path).map(|l| l.job)
    }
}

// ---------------------------------------------------------------- loading

/// Resolve a path against the project root.
pub(crate) fn resolve_path(s: &Session, path: &Path) -> PathBuf {
    if path.is_relative() {
        s.project_root().map(|r| r.join(path)).unwrap_or(path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

/// Open a snapshot file: parse and analyse it on a worker thread.
pub(crate) fn open_snapshot(s: &mut Session, path: &Path) -> Result<(), CommandError> {
    let path = resolve_path(s, path);
    if !path.exists() {
        return Err(CommandError::Failed(format!("no such snapshot file: {}", path.display())));
    }
    // An Instruments trace bundle is exported first.
    let convert = convert_for_file(s, &path);
    spawn_load(s, path, convert, Purpose::Open, None);
    s.ws.layout.show(ToolWindowId::Profiler);
    Ok(())
}

/// What a file needs before parsing, by its name.
fn convert_for_file(s: &Session, path: &Path) -> Convert {
    let name = path.to_string_lossy();
    if name.ends_with(".trace")
        && let Some(x) = s.profiler.find_tool(ProfilerTool::Xctrace)
    {
        return Convert::Xctrace { xctrace: x };
    }
    if name.ends_with(".memgraph") {
        return Convert::MallocHistory { program: None };
    }
    if name.ends_with(".perf.data")
        && let Some(perf) = s.profiler.find_tool(ProfilerTool::Perf)
    {
        return Convert::PerfScript { perf };
    }
    let python = || {
        project_python(s).map_or_else(|| crate::profiler_plan::resolve_program("python3"), |p| p.display().to_string())
    };
    let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if (name.ends_with(".pstats") || name.ends_with(".prof"))
        && let Ok(helper) = helper_path(s)
    {
        return Convert::PStats { python: python(), helper, cwd };
    }
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if file.ends_with(".memray.bin") || (file.starts_with("memray-") && file.ends_with(".bin")) {
        return Convert::Memray { python: python(), memray: s.profiler.find_tool(ProfilerTool::Memray), cwd };
    }
    // samply / Firefox Profiler JSON: frames with only addresses are symbolicated.
    let ours = [".tracemalloc.json", ".peak.json", ".speedscope.json", ".pstats.json", ".syms.json"];
    if (file.ends_with(".json") || file.ends_with(".json.gz")) && !ours.iter().any(|x| file.ends_with(x)) {
        let (atos, addr2line) = s.profiler.symbolizers();
        return Convert::Samply { atos, addr2line };
    }
    Convert::None
}

/// Load a snapshot into the cache for reports (a job agents can poll).
pub(crate) fn load_for_report(s: &mut Session, path: &Path) -> Result<u64, CommandError> {
    let path = resolve_path(s, path);
    if let Some(Some(job)) = s.profiler.load_in_flight(&path) {
        return Ok(job);
    }
    if !path.exists() {
        return Err(CommandError::Failed(format!("no such snapshot file: {}", path.display())));
    }
    let title = format!(
        "Load {}",
        path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
    );
    let job = s.profiler.new_job(title, ProfileKind::Cpu, None, JobState::Loading);
    let convert = convert_for_file(s, &path);
    spawn_load(s, path, convert, Purpose::Cache, Some(job));
    Ok(job)
}

/// Compare the open snapshot against another one (loaded on a worker thread).
pub(crate) fn set_baseline(s: &mut Session, path: &Path) -> Result<(), CommandError> {
    if s.profiler.snapshot.is_none() {
        return Err(CommandError::Failed("no profiler snapshot is open".into()));
    }
    let path = resolve_path(s, path);
    if !path.exists() {
        return Err(CommandError::Failed(format!("no such snapshot file: {}", path.display())));
    }
    let convert = convert_for_file(s, &path);
    s.profiler.baseline_loading = Some(path.clone());
    spawn_load(s, path, convert, Purpose::Baseline, None);
    Ok(())
}

/// Parse `path` (after converting it) on a worker thread.
fn spawn_load(s: &mut Session, path: PathBuf, convert: Convert, purpose: Purpose, job: Option<u64>) {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let job_path = path.clone();
    let spawned = std::thread::Builder::new().name("profiler-load".into()).spawn(move || {
        let result = (|| {
            let (file, profile) = match crate::profiler_convert::convert(&convert, &job_path)? {
                Converted::File(file) => {
                    let p = codecraft_profile::load(&file).map_err(|e| e.to_string())?;
                    (file, p)
                }
                Converted::Parsed(file, p) => (file, *p),
            };
            let analysis = Analysis::new(&profile, None);
            let baseline = (purpose == Purpose::Baseline).then(|| {
                let mut methods: HashMap<FunctionKey, (u64, u64)> = HashMap::new();
                for m in &analysis.methods {
                    if let Some(f) = profile.frame(m.frame) {
                        let e = methods.entry((f.name.clone(), f.module.clone(), f.file.clone())).or_insert((0, 0));
                        e.0 = e.0.saturating_add(m.self_weight);
                        e.1 = e.1.saturating_add(m.total);
                    }
                }
                BaselineData {
                    weights: codecraft_profile::path_weights(&profile, &analysis.tree),
                    methods,
                    total: analysis.tree.total,
                }
            });
            Ok(Loaded { path: file, profile: Arc::new(profile), analysis, baseline })
        })();
        let _ = tx.send(result);
    });
    match spawned {
        Ok(_) => {
            if purpose == Purpose::Open {
                s.profiler.loading = Some(path.clone());
                s.profiler.error = None;
                // A newer open replaces an older one still loading.
                s.profiler.loads.retain(|l| !matches!(l.purpose, Purpose::Open | Purpose::Reanalyse));
            }
            s.profiler.loads.push(LoadJob { rx, path, job, purpose });
        }
        Err(e) => {
            let msg = format!("cannot start a worker thread: {e}");
            s.profiler.fail_job(job, &msg);
            s.profiler.error = Some(msg);
        }
    }
}

/// Show another thread (`None` = all threads merged), keeping the time range: reanalysed on a
/// worker thread.
pub(crate) fn select_thread(s: &mut Session, thread: Option<usize>) -> Result<(), CommandError> {
    let range = s.profiler.snapshot.as_ref().and_then(|snap| snap.selection.range);
    select(s, Selection { thread, range })
}

/// Show a time range (nanoseconds since the first sample; `None` = the whole run), keeping
/// the thread: every view (flame graph, call tree, method list, hot lines, gutter) follows.
pub(crate) fn select_range(s: &mut Session, range: Option<(u64, u64)>) -> Result<(), CommandError> {
    let snap =
        s.profiler.snapshot.as_ref().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))?;
    if range.is_some() && !snap.profile.has_sample_times() {
        return Err(CommandError::Failed(format!(
            "{} snapshots have no sample times, so there is no time range to select",
            snap.profile.format.label()
        )));
    }
    let thread = snap.selection.thread;
    select(s, Selection { thread, range })
}

/// Show a selection of the open snapshot: reanalysed on a worker thread.
pub(crate) fn select(s: &mut Session, sel: Selection) -> Result<(), CommandError> {
    let snap =
        s.profiler.snapshot.as_mut().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))?;
    if let Some(t) = sel.thread
        && t >= snap.profile.threads.len()
    {
        return Err(CommandError::BadParams(format!("no thread {t}")));
    }
    let duration = snap.profile.duration_ns.unwrap_or(0);
    let sel = Selection {
        thread: sel.thread,
        range: snap.profile.effective_range(&sel).map(|(a, b)| (a.min(duration), b.min(duration))),
    };
    snap.selection = sel;
    if snap.analysis.selection() == sel {
        s.profiler.loads.retain(|l| l.purpose != Purpose::Reanalyse);
        if s.profiler.loads.iter().all(|l| l.purpose != Purpose::Open) {
            s.profiler.loading = None;
        }
        return Ok(());
    }
    let (profile, path) = (Arc::clone(&snap.profile), snap.path.clone());
    let (tx, rx) = crossbeam_channel::bounded(1);
    let job_path = path.clone();
    std::thread::Builder::new()
        .name("profiler-analyse".into())
        .spawn(move || {
            let analysis = Analysis::select(&profile, &sel);
            let _ = tx.send(Ok(Loaded { path: job_path, profile, analysis, baseline: None }));
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    s.profiler.loads.retain(|l| !matches!(l.purpose, Purpose::Open | Purpose::Reanalyse));
    s.profiler.loading = Some(path.clone());
    s.profiler.loads.push(LoadJob { rx, path, job: None, purpose: Purpose::Reanalyse });
    Ok(())
}

// ---------------------------------------------------------------- recording

/// A recording to start.
pub(crate) struct RecordRequest {
    pub plan: Plan,
    pub kind: ProfileKind,
    pub tool: Option<ProfilerTool>,
    pub seconds: Option<u64>,
    /// tracemalloc: seconds between memory samples (`None`: the default, 0: off).
    pub interval: Option<f64>,
}

/// A fresh snapshot path: `<dir>/snapshots/<name>-<time>.<ext>`.
fn snapshot_path(s: &Session, name: &str, tool: ProfilerTool) -> Result<PathBuf, CommandError> {
    let dir = s
        .profiler
        .dir()
        .ok_or_else(|| CommandError::Failed("cannot find the home directory; set CODECRAFT_HOME".into()))?
        .join("snapshots");
    std::fs::create_dir_all(&dir).map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(60)
        .collect();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    Ok(dir.join(format!("{}-{stamp}.{}", safe.trim_matches('-'), tool.extension())))
}

/// The helper script, written next to the snapshots (rewritten when CodeCraft's copy changed).
fn helper_path(s: &Session) -> Result<PathBuf, CommandError> {
    let dir = s.profiler.dir().ok_or_else(|| CommandError::Failed("cannot find the home directory".into()))?;
    let path = dir.join("codecraft_profile_helper.py");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(HELPER) {
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&path, HELPER))
            .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
    }
    Ok(path)
}

fn missing(s: &mut Session, tool: ProfilerTool) -> CommandError {
    s.profiler.missing = Some(tool);
    s.ws.layout.show(ToolWindowId::Profiler);
    let msg = format!("{} is not installed. {}", tool.program(), tool.install_hint());
    s.status = msg.clone();
    CommandError::Failed(msg)
}

/// The project's Python interpreter.
pub(crate) fn project_python(s: &Session) -> Option<PathBuf> {
    s.project_root().and_then(|r| crate::python_env::project_python(&r))
}

/// Start a profiling job: build first when the plan says so, then record.
pub(crate) fn record(s: &mut Session, req: RecordRequest) -> Result<u64, CommandError> {
    let RecordRequest { mut plan, kind, tool, seconds, interval } = req;
    if interval.is_some() {
        plan.sample_interval_ms = interval.map(crate::profiler_plan::seconds_to_ms);
    }
    let python = match &plan.target {
        Target::Python { python, .. } => Some(python.clone()),
        Target::Native { .. } => None,
    };
    if let Some(t) = tool
        && t.python_only()
        && python.is_none()
    {
        return Err(CommandError::BadParams(format!("{} profiles Python programs only", t.program())));
    }
    let kind = tool.map_or(kind, ProfilerTool::kind);
    let (tool, tool_path) = match s.profiler.choose(python.as_deref(), kind, tool) {
        Ok(t) => t,
        Err(t) => return Err(missing(s, t)),
    };
    s.profiler.missing = None;
    let title = match kind {
        ProfileKind::Cpu => format!("Profile {}", plan.name),
        ProfileKind::Memory => format!("Profile Memory {}", plan.name),
    };
    let job = s.profiler.new_job(title.clone(), kind, Some(tool), JobState::Building);
    if let Some(j) = s.profiler.job_mut(job) {
        j.debug_build = plan.debug_build;
    }
    let result = match plan.build.clone() {
        Some(BuildStep::Cargo { invocation, pick }) => {
            let _ = s.save_all();
            match codecraft_cargo::spawn(&invocation) {
                Ok((handle, events)) => {
                    let tab = s.ws.add_run(format!("Build for {title}"), invocation.command_line(), true);
                    s.ws.layout.show(ToolWindowId::Build);
                    s.ws.clear_cargo_problems();
                    s.profiler.builds.push(PendingBuild {
                        job,
                        tab,
                        plan,
                        tool,
                        tool_path,
                        seconds,
                        cargo: Some(CargoBuild { handle, events, executables: Vec::new(), pick }),
                    });
                    Ok(tab)
                }
                Err(e) => Err(CommandError::Failed(format!("cannot start cargo: {e}"))),
            }
        }
        Some(BuildStep::Shell(cmd)) => {
            crate::runs::start_process(s, &cmd, &format!("Build for {title}")).inspect(|&tab| {
                s.profiler.builds.push(PendingBuild { job, tab, plan, tool, tool_path, seconds, cargo: None });
            })
        }
        None => {
            let program = match &plan.target {
                Target::Native { program, .. } => program.clone(),
                Target::Python { .. } => None,
            };
            start_recording(s, job, tool, tool_path, &plan, program.as_deref(), seconds)
        }
    };
    match result {
        Ok(tab) => {
            if let Some(j) = s.profiler.job_mut(job) {
                j.tab = Some(tab);
            }
            Ok(job)
        }
        Err(e) => {
            s.profiler.fail_job(Some(job), &e.to_string());
            Err(e)
        }
    }
}

/// What turns a tool's output into a snapshot.
fn convert_for(
    s: &Session,
    tool: ProfilerTool,
    tool_path: Option<&Path>,
    plan: &Plan,
    program: Option<&Path>,
    helper: &Path,
) -> Convert {
    let python = match &plan.target {
        Target::Python { python, .. } => python.clone(),
        Target::Native { .. } => "python3".into(),
    };
    match tool {
        ProfilerTool::Perf => tool_path.map_or(Convert::None, |p| Convert::PerfScript { perf: p.to_path_buf() }),
        ProfilerTool::Xctrace => tool_path.map_or(Convert::None, |p| Convert::Xctrace { xctrace: p.to_path_buf() }),
        ProfilerTool::Leaks => Convert::MallocHistory { program: program.map(Path::to_path_buf) },
        ProfilerTool::CProfile => Convert::PStats { python, helper: helper.to_path_buf(), cwd: plan.cwd.clone() },
        ProfilerTool::Memray => {
            Convert::Memray { python, memray: tool_path.map(Path::to_path_buf), cwd: plan.cwd.clone() }
        }
        ProfilerTool::Heaptrack => match s.profiler.find_companion(ProfilerTool::Heaptrack, "heaptrack_print") {
            Some(print) => Convert::Heaptrack { print },
            None => Convert::None,
        },
        ProfilerTool::Samply => {
            let (atos, addr2line) = s.profiler.symbolizers();
            Convert::Samply { atos, addr2line }
        }
        ProfilerTool::PySpy | ProfilerTool::Tracemalloc => Convert::None,
    }
}

/// Start the profiler on the planned program in a Run tab.
fn start_recording(
    s: &mut Session,
    job: u64,
    tool: ProfilerTool,
    tool_path: Option<PathBuf>,
    plan: &Plan,
    program: Option<&Path>,
    seconds: Option<u64>,
) -> Result<RunTabId, CommandError> {
    let output = snapshot_path(s, &plan.name, tool)?;
    let helper = if matches!(tool, ProfilerTool::Tracemalloc | ProfilerTool::CProfile) {
        helper_path(s)?
    } else {
        PathBuf::new()
    };
    let cmd = crate::profiler_plan::wrap(tool, tool_path.as_deref(), plan, program, &output, &helper, seconds)
        .map_err(CommandError::Failed)?;
    let convert = convert_for(s, tool, tool_path.as_deref(), plan, program, &helper);
    let title = s.profiler.job(job).map_or_else(|| format!("Profile {}", plan.name), |j| j.title.clone());
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    if let Some(t) = s.ws.run_mut(tab) {
        let interval = crate::profiler_plan::tracemalloc_interval(plan);
        let note = match tool {
            ProfilerTool::Xctrace => "Recording with Instruments' Time Profiler (xctrace)…".to_string(),
            ProfilerTool::Leaks => {
                "Recording allocations that are still live when the program exits (leaks --atExit)…".to_string()
            }
            ProfilerTool::Tracemalloc if interval > 0.0 => format!(
                "Tracing Python allocations (tracemalloc): memory is sampled every {interval} s with a snapshot at the peak, and the live allocations are taken when the program ends…"
            ),
            ProfilerTool::Tracemalloc => {
                "Tracing Python allocations (tracemalloc); the snapshot is taken when the program ends…".to_string()
            }
            ProfilerTool::CProfile => "Profiling with cProfile…".to_string(),
            _ => "Recording…".to_string(),
        };
        t.push(ConsoleKind::System, note);
    }
    // Tools without a time limit of their own are stopped like Stop would.
    let deadline = seconds.filter(|_| tool != ProfilerTool::Xctrace).map(|x| Instant::now() + Duration::from_secs(x));
    s.profiler.recordings.push(Recording {
        tab,
        tool,
        title,
        output,
        attached: None,
        started: Instant::now(),
        stop_requested: None,
        job,
        deadline,
        convert,
    });
    if let Some(j) = s.profiler.job_mut(job) {
        j.state = JobState::Recording;
        j.tab = Some(tab);
        j.tool = Some(tool);
    }
    Ok(tab)
}

/// Attach Profiler to Process: record a running process until Stop (or `seconds`).
pub(crate) fn attach(s: &mut Session, pid: u32, name: &str, python: bool) -> Result<RunTabId, CommandError> {
    let job = attach_job(s, pid, name, python, None, None)?;
    s.profiler.job(job).and_then(|j| j.tab).ok_or_else(|| CommandError::Failed("the recording did not start".into()))
}

/// Attach as a job.
pub(crate) fn attach_job(
    s: &mut Session,
    pid: u32,
    name: &str,
    python: bool,
    tool: Option<ProfilerTool>,
    seconds: Option<u64>,
) -> Result<u64, CommandError> {
    if pid <= 1 || pid == std::process::id() {
        return Err(CommandError::BadParams(format!("cannot profile process {pid}")));
    }
    let py = python.then_some("python3");
    // Only sampling profilers attach: py-spy for Python where it's installed, else native.
    let forced = match tool {
        Some(t) => Some(t),
        None if python && s.profiler.find_tool(ProfilerTool::PySpy).is_some() => Some(ProfilerTool::PySpy),
        None => None,
    };
    let (tool, tool_path) = match s.profiler.choose(if forced.is_some() { py } else { None }, ProfileKind::Cpu, forced)
    {
        Ok(t) => t,
        Err(t) => return Err(missing(s, t)),
    };
    let tool_path = tool_path
        .ok_or_else(|| CommandError::Failed(format!("{} can't attach to a running process", tool.program())))?;
    s.profiler.missing = None;
    let label = if name.is_empty() { format!("pid {pid}") } else { format!("{name} ({pid})") };
    let output = snapshot_path(s, &format!("attach-{}", if name.is_empty() { "process" } else { name }), tool)?;
    let args = crate::profiler_plan::attach_args(tool, pid, &output, seconds).map_err(CommandError::BadParams)?;
    let cwd = s.project_root().unwrap_or_else(std::env::temp_dir);
    let cmd = ProcessCommand { program: tool_path.display().to_string(), args, cwd, env: Vec::new(), build: false };
    let title = format!("Profile {label}");
    let job = s.profiler.new_job(title.clone(), ProfileKind::Cpu, Some(tool), JobState::Recording);
    let tab = match crate::runs::start_process(s, &cmd, &title) {
        Ok(t) => t,
        Err(e) => {
            s.profiler.fail_job(Some(job), &e.to_string());
            return Err(e);
        }
    };
    if let Some(t) = s.ws.run_mut(tab) {
        t.push(
            ConsoleKind::System,
            "Recording… press Stop in the Profiler tool window to finish and open the snapshot.",
        );
    }
    let convert = match tool {
        ProfilerTool::Perf => Convert::PerfScript { perf: tool_path.clone() },
        ProfilerTool::Xctrace => Convert::Xctrace { xctrace: tool_path.clone() },
        _ => Convert::None,
    };
    let deadline = seconds.filter(|_| tool != ProfilerTool::Xctrace).map(|x| Instant::now() + Duration::from_secs(x));
    s.profiler.recordings.push(Recording {
        tab,
        tool,
        title,
        output,
        attached: Some(pid),
        started: Instant::now(),
        stop_requested: None,
        job,
        deadline,
        convert,
    });
    if let Some(j) = s.profiler.job_mut(job) {
        j.tab = Some(tab);
    }
    s.ws.layout.show(ToolWindowId::Profiler);
    Ok(job)
}

/// Stop a recording: SIGINT to its process group so the profiler writes its snapshot (it is
/// killed if it hasn't exited after a grace period). A build before a recording is cancelled.
pub(crate) fn stop(s: &mut Session, tab: RunTabId) -> bool {
    if let Some(i) = s.profiler.builds.iter().position(|b| b.tab == tab) {
        let b = s.profiler.builds.remove(i);
        cancel_build(s, b, "the build was stopped");
        return true;
    }
    let Some(rec) = s.profiler.recordings.iter_mut().find(|r| r.tab == tab) else { return false };
    if rec.stop_requested.is_some() {
        // Asked twice: stop for good.
        crate::runs::stop(s, tab);
        return true;
    }
    rec.stop_requested = Some(Instant::now());
    let pid = s.runs.pid(tab);
    match pid {
        Some(pid) if cfg!(unix) => {
            let spawned = std::thread::Builder::new().name("profiler-stop".into()).spawn(move || {
                if !codecraft_dap::kill_process_group(pid, "INT") {
                    codecraft_dap::kill_process(pid, "INT");
                }
            });
            if spawned.is_err() {
                crate::runs::stop(s, tab);
            }
        }
        _ => {
            crate::runs::stop(s, tab);
        }
    }
    if let Some(t) = s.ws.run_mut(tab) {
        t.push(ConsoleKind::System, "Stopping the profiler and saving the snapshot…");
    }
    true
}

fn cancel_build(s: &mut Session, b: PendingBuild, why: &str) {
    match b.cargo {
        Some(c) => {
            let _ = c.handle.kill();
            if let Some(t) = s.ws.run_mut(b.tab) {
                t.status = RunStatus::Stopped;
            }
        }
        None => {
            crate::runs::stop(s, b.tab);
        }
    }
    s.profiler.fail_job(Some(b.job), why);
}

/// Kill recordings (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    let tabs: Vec<RunTabId> = s.profiler.recordings.iter().map(|r| r.tab).collect();
    for t in tabs {
        crate::runs::stop(s, t);
    }
    let builds = std::mem::take(&mut s.profiler.builds);
    for b in builds {
        cancel_build(s, b, "CodeCraft closed");
    }
    s.profiler.recordings.clear();
    s.profiler.loads.clear();
    s.profiler.attach_rx = None;
}

// ---------------------------------------------------------------- attach picker

pub(crate) fn open_attach_picker(s: &mut Session, all_users: Option<bool>) {
    let previous = s.profiler.attach.take().unwrap_or_default();
    s.profiler.attach = Some(AttachPicker {
        loading: true,
        query: previous.query,
        all_users: all_users.unwrap_or(previous.all_users),
        user: codecraft_dap::processes::current_user(),
        ..AttachPicker::default()
    });
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("profiler-ps".into()).spawn(move || {
        let _ = tx.send(codecraft_dap::processes::list_processes());
    });
    match spawned {
        Ok(_) => s.profiler.attach_rx = Some(rx),
        Err(e) => {
            if let Some(p) = &mut s.profiler.attach {
                p.loading = false;
                p.error = Some(format!("cannot start a worker thread: {e}"));
            }
        }
    }
}

fn pump_attach(s: &mut Session) -> bool {
    let Some(rx) = &s.profiler.attach_rx else { return false };
    let result = match rx.try_recv() {
        Ok(r) => r,
        Err(crossbeam_channel::TryRecvError::Empty) => return false,
        Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the process list failed".into()),
    };
    s.profiler.attach_rx = None;
    let me = std::process::id();
    if let Some(p) = &mut s.profiler.attach {
        p.loading = false;
        match result {
            Ok(mut list) => {
                list.retain(|x| x.pid != me);
                list.sort_by_key(|a| std::cmp::Reverse(a.pid));
                p.processes = list;
                p.selected = 0;
                p.error = None;
            }
            Err(e) => p.error = Some(e),
        }
    }
    true
}

// ---------------------------------------------------------------- pump

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = pump_attach(s);
    changed |= pump_builds(s);
    changed |= pump_recordings(s);
    changed |= pump_load(s);
    changed
}

/// The executable a Cargo build produced.
fn pick_artifact(pick: Option<&str>, executables: &[(String, PathBuf)]) -> Result<PathBuf, String> {
    let named = pick.and_then(|n| {
        let alt = n.replace('-', "_");
        executables.iter().rev().find(|(t, _)| t == n || *t == alt)
    });
    match (named, executables) {
        (Some((_, path)), _) => Ok(path.clone()),
        (None, [(_, only)]) => Ok(only.clone()),
        (None, []) => Err("the build produced no executable".into()),
        (None, many) => {
            let mut names: Vec<&str> = many.iter().map(|(n, _)| n.as_str()).collect();
            names.dedup();
            Err(format!(
                "the build produced {} executables ({}); profile a single target (a configuration with --bin, --test, --bench or --lib)",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

fn pump_builds(s: &mut Session) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < s.profiler.builds.len() {
        let Some(b) = s.profiler.builds.get_mut(i) else { break };
        let tab = b.tab;
        // `Some(success)` once the build is over.
        let mut done: Option<bool> = None;
        match &mut b.cargo {
            Some(c) => {
                let mut lines: Vec<(ConsoleKind, String)> = Vec::new();
                let mut problems = Vec::new();
                for _ in 0..2000 {
                    let Ok(ev) = c.events.try_recv() else { break };
                    changed = true;
                    match ev {
                        RunEvent::Stdout(l) => lines.push((ConsoleKind::Stdout, crate::runs::strip_ansi(&l))),
                        RunEvent::Stderr(l) => lines.push((ConsoleKind::Stderr, crate::runs::strip_ansi(&l))),
                        RunEvent::Error(e) => lines.push((ConsoleKind::System, e)),
                        RunEvent::Message(CargoMessage::CompilerArtifact {
                            target_name,
                            executable: Some(exe),
                            ..
                        }) => {
                            c.executables.push((target_name, exe));
                        }
                        RunEvent::Message(CargoMessage::CompilerMessage { diagnostic, .. }) => {
                            if let Some(r) = diagnostic.rendered.as_deref() {
                                for l in r.lines() {
                                    lines.push((ConsoleKind::Stderr, crate::runs::strip_ansi(l)));
                                }
                            }
                            problems.push(diagnostic);
                        }
                        RunEvent::Exited { success, .. } => done = Some(success),
                        _ => {}
                    }
                }
                let root = b.plan.cwd.clone();
                if let Some(t) = s.ws.run_mut(tab) {
                    for (k, l) in lines {
                        t.push(k, l);
                    }
                    if let Some(ok) = done {
                        t.status = if ok { RunStatus::Succeeded } else { RunStatus::Failed(None) };
                    }
                }
                for d in problems {
                    if let Some(p) = crate::runs::problem_from_diagnostic(&root, &d) {
                        s.ws.add_cargo_problem(p);
                    }
                }
            }
            None => {
                if !s.runs.is_running(tab) {
                    done = Some(s.ws.run(tab).is_some_and(|t| t.status == RunStatus::Succeeded));
                }
            }
        }
        let Some(success) = done else {
            i += 1;
            continue;
        };
        changed = true;
        let b = s.profiler.builds.remove(i);
        if !success {
            let msg = format!("{}: the build failed (see the Build tab)", b.plan.name);
            s.profiler.fail_job(Some(b.job), &msg);
            s.profiler.error = Some(msg.clone());
            s.status = msg;
            continue;
        }
        let program = match (&b.cargo, &b.plan.target) {
            (Some(c), _) => match pick_artifact(c.pick.as_deref(), &c.executables) {
                Ok(p) => Some(p),
                Err(e) => {
                    s.profiler.fail_job(Some(b.job), &e);
                    s.profiler.error = Some(e.clone());
                    s.status = e;
                    continue;
                }
            },
            (None, Target::Native { program, .. }) => program.clone(),
            (None, Target::Python { .. }) => None,
        };
        if let Err(e) = start_recording(s, b.job, b.tool, b.tool_path.clone(), &b.plan, program.as_deref(), b.seconds) {
            s.profiler.fail_job(Some(b.job), &e.to_string());
            s.profiler.error = Some(e.to_string());
            s.status = e.to_string();
        }
    }
    changed
}

fn pump_recordings(s: &mut Session) -> bool {
    let mut changed = false;
    let mut i = 0;
    while let Some(rec) = s.profiler.recordings.get(i) {
        let (tab, stop_requested, deadline) = (rec.tab, rec.stop_requested, rec.deadline);
        if s.runs.is_running(tab) {
            if stop_requested.is_none() && deadline.is_some_and(|d| Instant::now() >= d) {
                stop(s, tab);
                changed = true;
            } else if stop_requested.is_some_and(|t| t.elapsed() > STOP_GRACE) {
                crate::runs::stop(s, tab);
                if let Some(r) = s.profiler.recordings.get_mut(i) {
                    // Don't escalate again.
                    r.stop_requested = Some(Instant::now() + Duration::from_secs(3600));
                }
            }
            i += 1;
            continue;
        }
        let rec = s.profiler.recordings.remove(i);
        changed = true;
        finished(s, rec);
    }
    changed
}

/// The file a tool wrote (heaptrack appends `.gz` or `.zst` to the name it's given).
fn written_output(rec: &Recording) -> Option<PathBuf> {
    let ok = |p: &Path| p.is_dir() || std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
    if ok(&rec.output) {
        return Some(rec.output.clone());
    }
    if rec.tool == ProfilerTool::Heaptrack {
        for ext in [".zst", ".gz"] {
            let p = PathBuf::from(format!("{}{ext}", rec.output.display()));
            if ok(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// A recording's process ended: open its snapshot, or say why there is none.
fn finished(s: &mut Session, rec: Recording) {
    if let Some(output) = written_output(&rec) {
        if let Some(t) = s.ws.run_mut(rec.tab) {
            t.push(ConsoleKind::System, format!("Profiler snapshot: {}", output.display()));
        }
        if let Some(j) = s.profiler.job_mut(rec.job) {
            j.state = JobState::Loading;
        }
        spawn_load(s, output, rec.convert.clone(), Purpose::Open, Some(rec.job));
        s.ws.layout.show(ToolWindowId::Profiler);
        s.status = format!("{}: opening the snapshot", rec.title);
        return;
    }
    let mut msg = format!("{}: {} wrote no snapshot (see the Run tab for its output)", rec.title, rec.tool.program());
    if rec.attached.is_some() && cfg!(target_os = "macos") && rec.tool == ProfilerTool::Samply {
        msg.push_str(
            ". On macOS, attaching needs permission: run `samply setup` once (it code-signs samply), or profile from Run › Profile instead",
        );
    } else if rec.tool == ProfilerTool::PySpy {
        msg.push_str(". py-spy may need root (on macOS always): use cProfile (the default) or run `sudo py-spy record …` in a terminal");
    } else if rec.tool == ProfilerTool::Xctrace {
        msg.push_str(". xctrace needs Xcode (not just the command line tools); without it install samply");
    }
    if let Some(t) = s.ws.run_mut(rec.tab) {
        t.push(ConsoleKind::System, msg.clone());
    }
    s.profiler.fail_job(Some(rec.job), &msg);
    s.profiler.error = Some(msg.clone());
    s.status = msg;
}

fn pump_load(s: &mut Session) -> bool {
    let mut changed = false;
    let mut i = 0;
    while let Some(l) = s.profiler.loads.get(i) {
        let result = match l.rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                i += 1;
                continue;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the snapshot loader stopped".into()),
        };
        let l = s.profiler.loads.remove(i);
        changed = true;
        loaded(s, l, result);
    }
    changed
}

fn loaded(s: &mut Session, l: LoadJob, result: Result<Loaded, String>) {
    match l.purpose {
        Purpose::Open | Purpose::Reanalyse => s.profiler.loading = None,
        Purpose::Baseline => s.profiler.baseline_loading = None,
        Purpose::Cache => {}
    }
    let loaded = match result {
        Ok(x) => x,
        Err(e) => {
            let msg = format!("{}: {e}", l.path.display());
            s.profiler.fail_job(l.job, &msg);
            if l.purpose != Purpose::Cache {
                s.profiler.error = Some(msg.clone());
                s.status = msg;
            }
            return;
        }
    };
    if let Some(j) = l.job.and_then(|id| s.profiler.job_mut(id)) {
        j.state = JobState::Done;
        j.snapshot = Some(loaded.path.clone());
        j.finished = Some(Instant::now());
    }
    match l.purpose {
        Purpose::Reanalyse => {
            if let Some(snap) = &mut s.profiler.snapshot
                && Arc::ptr_eq(&snap.profile, &loaded.profile)
            {
                snap.analysis = Arc::new(loaded.analysis);
                snap.reset_view();
            }
        }
        Purpose::Open => {
            s.profiler.remember(&loaded.path);
            let analysis = Arc::new(loaded.analysis);
            s.profiler.cache_insert(loaded.path.clone(), Arc::clone(&loaded.profile), Arc::clone(&analysis));
            let snap = Snapshot::new(loaded.path, loaded.profile, analysis);
            s.status = format!(
                "Profiler: {} ({}, {} threads, {})",
                snap.title(),
                snap.profile.format.label(),
                snap.profile.threads.len(),
                snap.profile.unit.format(snap.profile.total())
            );
            s.profiler.snapshot = Some(snap);
            s.profiler.error = None;
        }
        Purpose::Cache => {
            s.profiler.cache_insert(loaded.path, loaded.profile, Arc::new(loaded.analysis));
        }
        Purpose::Baseline => {
            let Some(data) = loaded.baseline else { return };
            let analysis = Arc::new(loaded.analysis);
            s.profiler.cache_insert(loaded.path.clone(), Arc::clone(&loaded.profile), analysis);
            if let Some(snap) = &mut s.profiler.snapshot {
                snap.baseline = Some(Baseline {
                    path: loaded.path,
                    total: data.total,
                    weights: Arc::new(data.weights),
                    methods: Arc::new(data.methods),
                    node_weights: Vec::new(),
                });
                snap.map_baseline();
            }
        }
    }
}

// ---------------------------------------------------------------- views for commands and the UI

/// Open a source location (relative paths against the project root, then by file name).
pub(crate) fn jump_to_location(s: &mut Session, file: &str, line: Option<u32>) -> Result<(), CommandError> {
    let line = line.unwrap_or(1).saturating_sub(1) as usize;
    let mut path = PathBuf::from(file);
    if path.is_relative()
        && let Some(root) = s.project_root()
    {
        path = root.join(path);
    }
    if !path.is_file() {
        return Err(CommandError::Failed(format!("source file not found: {}", path.display())));
    }
    s.navigate_to_position(&path, line, 0)
}

/// Open the source of a frame (file:line), relative paths against the project root.
pub(crate) fn jump_to_frame(s: &mut Session, frame: u32) -> Result<(), CommandError> {
    let snap =
        s.profiler.snapshot.as_ref().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))?;
    let f = snap.profile.frame(frame).ok_or_else(|| CommandError::BadParams(format!("no frame {frame}")))?;
    let Some(file) = f.file.clone() else {
        return Err(CommandError::Failed(format!("no source location for {}", f.name)));
    };
    let line = f.line;
    jump_to_location(s, &file, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> Snapshot {
        let p = codecraft_profile::parse(text.as_bytes()).unwrap();
        let a = Analysis::new(&p, None);
        Snapshot::new(PathBuf::from("/x.txt"), Arc::new(p), Arc::new(a))
    }

    #[test]
    fn method_sorting() {
        let mut s = snapshot("main;b 5\nmain;a 3\nmain;c 3\n");
        let names = |s: &Snapshot| -> Vec<String> {
            s.method_order.iter().map(|i| s.profile.frame(s.analysis.methods[*i].frame).unwrap().name.clone()).collect()
        };
        assert_eq!(names(&s), vec!["b", "a", "c", "main"]);
        s.sort = MethodColumn::Name;
        s.descending = false;
        s.sort_methods();
        assert_eq!(names(&s), vec!["a", "b", "c", "main"]);
        s.descending = true;
        s.sort_methods();
        assert_eq!(names(&s), vec!["main", "c", "b", "a"]);
        s.sort = MethodColumn::Total;
        s.descending = true;
        s.sort_methods();
        assert_eq!(names(&s)[0], "main");
        s.descending = false;
        s.sort_methods();
        assert_eq!(names(&s), vec!["a", "c", "b", "main"]);
    }

    #[test]
    fn default_view_expands_the_heaviest_path() {
        let s = snapshot("main;a;b 5\nmain;c 1\n");
        assert_eq!(s.expanded.len(), 3);
        let mut s = s;
        s.set_search("B");
        assert_eq!(s.matched, 5);
        let b = s.profile.frames.iter().position(|f| f.name == "b").unwrap() as u32;
        let node = s.heaviest_node(b).unwrap();
        assert_eq!(s.analysis.tree.node(node).unwrap().total, 5);
    }

    #[test]
    fn line_index_matches_paths() {
        let s = snapshot("main (src/app.py:3);work (src/app.py:9) 4\nmain (src/app.py:3);work (src/app.py:10) 1\n");
        let lines = s.lines.for_path(Path::new("/proj/src/app.py")).unwrap();
        assert_eq!(
            lines.iter().map(|h| (h.line, h.self_weight, h.total)).collect::<Vec<_>>(),
            vec![(3, 0, 5), (9, 4, 4), (10, 1, 1)]
        );
        assert!(s.lines.for_path(Path::new("/proj/src/other.py")).is_none());
        assert!(s.lines.for_path(Path::new("/proj/xsrc/app.py")).is_none());
        let abs = snapshot("f (/tmp/a.c:2) 1\n");
        assert!(abs.lines.for_path(Path::new("/private/tmp/a.c")).is_some());
    }

    #[test]
    fn tool_names_and_choice() {
        assert_eq!(ProfilerTool::from_name("py-spy"), Some(ProfilerTool::PySpy));
        assert_eq!(ProfilerTool::from_name("cprofile"), Some(ProfilerTool::CProfile));
        assert_eq!(ProfilerTool::from_name("Instruments"), Some(ProfilerTool::Xctrace));
        assert_eq!(ProfilerTool::from_name("rm"), None);
        assert_eq!(serde_json::to_value(ProfilerTool::CProfile).unwrap(), "cProfile");
        let dir = tempfile::tempdir().unwrap();
        let mut st = ProfilerState {
            tool_dirs: Some(vec![dir.path().to_path_buf()]),
            dir_override: Some(dir.path().join("home").join("profiler")),
            perf_allowed: Some(false),
            xctrace_allowed: Some(true),
            ..ProfilerState::default()
        };
        assert_eq!(st.choose(Some("python3"), ProfileKind::Cpu, None), Ok((ProfilerTool::CProfile, None)));
        assert_eq!(st.choose(Some("python3"), ProfileKind::Memory, None), Ok((ProfilerTool::Tracemalloc, None)));
        assert_eq!(st.choose(None, ProfileKind::Cpu, None), Err(ProfilerTool::Xctrace));
        std::fs::write(dir.path().join("samply"), "").unwrap();
        assert_eq!(st.choose(None, ProfileKind::Cpu, None).unwrap().0, ProfilerTool::Samply);
        std::fs::write(dir.path().join("xctrace"), "").unwrap();
        assert_eq!(st.choose(None, ProfileKind::Cpu, None).unwrap().0, ProfilerTool::Xctrace);
        st.xctrace_allowed = Some(false);
        assert_eq!(st.choose(None, ProfileKind::Cpu, None).unwrap().0, ProfilerTool::Samply);
        assert_eq!(st.choose(None, ProfileKind::Cpu, Some(ProfilerTool::PySpy)), Err(ProfilerTool::PySpy));
        assert_eq!(
            st.choose(Some("/v/bin/python"), ProfileKind::Memory, Some(ProfilerTool::Memray)),
            Err(ProfilerTool::Memray)
        );
        std::fs::write(dir.path().join("memray"), "").unwrap();
        assert_eq!(
            st.choose(Some("/v/bin/python"), ProfileKind::Memory, None).unwrap(),
            (ProfilerTool::Memray, Some(dir.path().join("memray")))
        );
        // Install commands go into $CODECRAFT_HOME/tools.
        let (prog, args) = st.install_command(ProfilerTool::PySpy, None).unwrap();
        assert_eq!(prog, "env");
        assert!(args[1].ends_with("home/tools/bin"), "{args:?}");
        assert_eq!(&args[2..], ["uv", "tool", "install", "py-spy"]);
        let (prog, args) = st.install_command(ProfilerTool::Memray, Some(Path::new("/v/python"))).unwrap();
        assert_eq!((prog.as_str(), args.join(" ")), ("uv", "pip install --python /v/python memray".to_string()));
        assert!(st.install_command(ProfilerTool::Perf, None).is_none());
        assert!(pick_artifact(Some("a"), &[]).is_err());
        let exes = vec![("a".to_string(), PathBuf::from("/t/a")), ("b_c".to_string(), PathBuf::from("/t/b"))];
        assert_eq!(pick_artifact(Some("b-c"), &exes).unwrap(), PathBuf::from("/t/b"));
        assert!(pick_artifact(None, &exes).unwrap_err().contains("2 executables"));
    }
}
