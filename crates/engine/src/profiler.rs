//! Profiling runs and snapshots shown in the Profiler tool window (M11 track P).
//!
//! Run › Profile runs the selected run configuration under a sampling profiler in a Run tab:
//! `samply record --save-only` (macOS, Linux), `perf record -g` on Linux when samply is missing,
//! `py-spy record --format speedscope` for Python. Attach Profiler to Process… records a running
//! process the same way. When the recording ends, the snapshot is parsed on a worker thread
//! ([`codecraft_profile`]) and opens in the Profiler tool window; Open Profiler Snapshot opens
//! any saved file. Stop sends SIGINT to the recording's process group (like Ctrl+C in a
//! terminal, so the profiler still writes its file) and kills it if it hangs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

use codecraft_dap::processes::{ProcessInfo, ProcessKind};
use codecraft_profile::{Analysis, Edge, Profile};
use codecraft_workspace::{ConsoleKind, RunTabId, ToolWindowId};

pub use codecraft_profile as profile;

use crate::debug::AttachPicker;
use crate::runs::{ProcessCommand, RunConfig};
use crate::{CommandError, Session};

/// Recent snapshots remembered.
pub const MAX_RECENT: usize = 10;
/// How long a stopped recording may take to write its snapshot before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(15);
/// Largest `perf script` output read.
const MAX_PERF_SCRIPT_BYTES: u64 = 1 << 30;

/// A sampling profiler CodeCraft can drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfilerTool {
    Samply,
    Perf,
    PySpy,
}

impl ProfilerTool {
    pub const ALL: [ProfilerTool; 3] = [Self::Samply, Self::Perf, Self::PySpy];

    pub fn program(self) -> &'static str {
        match self {
            Self::Samply => "samply",
            Self::Perf => "perf",
            Self::PySpy => "py-spy",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.program() == name.trim())
    }

    /// How to install it.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Samply => "Install samply with `cargo install --locked samply` (macOS and Linux).",
            Self::Perf => "Install perf with your distribution's package (linux-tools / linux-perf).",
            Self::PySpy => "Install py-spy with `pip install py-spy` (or `uv tool install py-spy`).",
        }
    }

    /// The command line that installs it, when there is a portable one.
    pub fn install_command(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::Samply => Some(("cargo", &["install", "--locked", "samply"])),
            Self::PySpy => Some(("python3", &["-m", "pip", "install", "--user", "py-spy"])),
            Self::Perf => None,
        }
    }

    /// The extension of the file it writes.
    fn extension(self) -> &'static str {
        match self {
            Self::Samply => "json",
            Self::Perf => "perf.data",
            Self::PySpy => "speedscope.json",
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfilerTab {
    #[default]
    FlameGraph,
    CallTree,
    MethodList,
}

impl ProfilerTab {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "flameGraph" | "flame" => Some(Self::FlameGraph),
            "callTree" | "tree" => Some(Self::CallTree),
            "methodList" | "methods" => Some(Self::MethodList),
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
}

impl Snapshot {
    fn new(path: PathBuf, profile: Arc<Profile>, analysis: Analysis) -> Self {
        let mut s = Self {
            path,
            profile,
            analysis: Arc::new(analysis),
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
        };
        s.reset_view();
        s
    }

    /// After (re)analysis: keep the search, reset positions that refer to old node ids.
    fn reset_view(&mut self) {
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

    /// Title for the tool window: the file name.
    pub fn title(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

/// A finished background load or analysis.
struct Loaded {
    path: PathBuf,
    profile: Arc<Profile>,
    analysis: Analysis,
    /// A thread-selection change of the open snapshot (keeps its view state where it can).
    reanalysis: bool,
}

#[derive(Debug, Default)]
pub struct ProfilerState {
    /// Explicit directory (tests and embedders); otherwise `$CODECRAFT_HOME/profiler`.
    pub dir_override: Option<PathBuf>,
    /// Directories searched for samply/perf/py-spy instead of `PATH` (tests).
    pub tool_dirs: Option<Vec<PathBuf>>,
    /// Use `perf` when samply is missing (Linux only by default).
    pub perf_allowed: Option<bool>,
    pub snapshot: Option<Snapshot>,
    /// A snapshot being parsed (or reanalysed).
    pub loading: Option<PathBuf>,
    load_rx: Option<Receiver<Result<Loaded, String>>>,
    pub error: Option<String>,
    /// How to install a missing profiler (shown with an Install button).
    pub missing: Option<ProfilerTool>,
    recent: Option<Vec<PathBuf>>,
    pub recordings: Vec<Recording>,
    /// Attach Profiler to Process… list.
    pub attach: Option<AttachPicker>,
    attach_rx: Option<Receiver<Result<Vec<ProcessInfo>, String>>>,
    pub tab: ProfilerTab,
    /// Open Profiler Snapshot without a path: the UI shows a file dialog.
    pub pick_snapshot: bool,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded").field("path", &self.path).finish()
    }
}

impl ProfilerState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        self.loading.is_some() || !self.recordings.is_empty() || self.attach_rx.is_some()
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

    /// Where a profiler is installed.
    pub fn find_tool(&self, tool: ProfilerTool) -> Option<PathBuf> {
        let exe = format!("{}{}", tool.program(), std::env::consts::EXE_SUFFIX);
        match &self.tool_dirs {
            Some(dirs) => dirs.iter().map(|d| d.join(&exe)).find(|p| p.is_file()),
            None => codecraft_lsp::which(tool.program()),
        }
    }

    fn perf_allowed(&self) -> bool {
        self.perf_allowed.unwrap_or(cfg!(target_os = "linux"))
    }

    /// The profiler for a program: py-spy for Python, else samply (perf on Linux without it).
    pub fn choose_tool(&self, python: bool) -> Result<(ProfilerTool, PathBuf), ProfilerTool> {
        if python {
            return self.find_tool(ProfilerTool::PySpy).map(|p| (ProfilerTool::PySpy, p)).ok_or(ProfilerTool::PySpy);
        }
        if let Some(p) = self.find_tool(ProfilerTool::Samply) {
            return Ok((ProfilerTool::Samply, p));
        }
        if self.perf_allowed()
            && let Some(p) = self.find_tool(ProfilerTool::Perf)
        {
            return Ok((ProfilerTool::Perf, p));
        }
        Err(ProfilerTool::Samply)
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
}

// ---------------------------------------------------------------- loading

/// Open a snapshot file: parse and analyse it on a worker thread.
pub(crate) fn open_snapshot(s: &mut Session, path: &Path) -> Result<(), CommandError> {
    let path = if path.is_relative() {
        s.project_root().map(|r| r.join(path)).unwrap_or(path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    if !path.is_file() {
        return Err(CommandError::Failed(format!("no such snapshot file: {}", path.display())));
    }
    spawn_load(s, path, None);
    s.ws.layout.show(ToolWindowId::Profiler);
    Ok(())
}

/// Parse `path` (after converting `perf.data` with `perf script` when `perf` is given).
fn spawn_load(s: &mut Session, path: PathBuf, perf: Option<PathBuf>) {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let job_path = path.clone();
    let spawned = std::thread::Builder::new().name("profiler-load".into()).spawn(move || {
        let result = (|| {
            let file = match perf {
                Some(perf) => perf_script(&perf, &job_path)?,
                None => job_path.clone(),
            };
            let profile = codecraft_profile::load(&file).map_err(|e| e.to_string())?;
            let analysis = Analysis::new(&profile, None);
            Ok(Loaded { path: file, profile: Arc::new(profile), analysis, reanalysis: false })
        })();
        let _ = tx.send(result);
    });
    match spawned {
        Ok(_) => {
            s.profiler.loading = Some(path);
            s.profiler.load_rx = Some(rx);
            s.profiler.error = None;
        }
        Err(e) => s.profiler.error = Some(format!("cannot start a worker thread: {e}")),
    }
}

/// `perf script -i <data>` into `<data>.txt` (so the converted snapshot can be reopened
/// without perf). Runs on the load worker.
fn perf_script(perf: &Path, data: &Path) -> Result<PathBuf, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(perf)
        .arg("script")
        .arg("-i")
        .arg(data)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run perf script: {e}"))?;
    let mut text = Vec::new();
    if let Some(out) = child.stdout.take() {
        out.take(MAX_PERF_SCRIPT_BYTES).read_to_end(&mut text).map_err(|e| format!("perf script: {e}"))?;
    }
    let _ = child.kill();
    let _ = child.wait();
    if text.is_empty() {
        return Err("perf script printed nothing (was anything recorded?)".into());
    }
    let out = PathBuf::from(format!("{}.txt", data.display()));
    std::fs::write(&out, &text).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    Ok(out)
}

/// Show another thread (`None` = all threads merged): reanalysed on a worker thread.
pub(crate) fn select_thread(s: &mut Session, thread: Option<usize>) -> Result<(), CommandError> {
    let snap =
        s.profiler.snapshot.as_ref().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))?;
    if let Some(t) = thread
        && t >= snap.profile.threads.len()
    {
        return Err(CommandError::BadParams(format!("no thread {t}")));
    }
    if snap.analysis.thread == thread {
        return Ok(());
    }
    let (profile, path) = (Arc::clone(&snap.profile), snap.path.clone());
    let (tx, rx) = crossbeam_channel::bounded(1);
    let job_path = path.clone();
    std::thread::Builder::new()
        .name("profiler-analyse".into())
        .spawn(move || {
            let analysis = Analysis::new(&profile, thread);
            let _ = tx.send(Ok(Loaded { path: job_path, profile, analysis, reanalysis: true }));
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    s.profiler.loading = Some(path);
    s.profiler.load_rx = Some(rx);
    Ok(())
}

// ---------------------------------------------------------------- recording

/// The command a run configuration runs (local only), as a plain process.
fn config_command(config: &RunConfig) -> Result<ProcessCommand, CommandError> {
    if !config.target.is_local() {
        return Err(CommandError::Failed(
            "profiling runs only on this machine (the configuration has a remote target)".into(),
        ));
    }
    Ok(match &config.command {
        Some(c) => c.clone(),
        None => {
            let inv = config.invocation.clone();
            let mut c = ProcessCommand::new(codecraft_cargo::find_cargo().display().to_string(), &[], inv.cwd.clone());
            c.args = codecraft_cargo::CargoInvocation { json_messages: false, ..inv.clone() }.args();
            c.env = inv.env;
            c
        }
    })
}

/// A program name resolved to a path (so the profiler finds the same program the IDE would).
fn resolve_program(program: &str) -> String {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return program.to_string();
    }
    codecraft_lsp::which(program).map_or_else(|| program.to_string(), |p| p.display().to_string())
}

fn is_python(cmd: &ProcessCommand) -> bool {
    codecraft_dap::processes::guess_kind(&cmd.program) == ProcessKind::Python
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

fn missing(s: &mut Session, tool: ProfilerTool) -> CommandError {
    s.profiler.missing = Some(tool);
    s.ws.layout.show(ToolWindowId::Profiler);
    let msg = format!("{} is not installed. {}", tool.program(), tool.install_hint());
    s.status = msg.clone();
    CommandError::Failed(msg)
}

/// Run › Profile: record a run configuration.
pub(crate) fn profile_config(s: &mut Session, config: &RunConfig) -> Result<RunTabId, CommandError> {
    let cmd = config_command(config)?;
    let (tool, tool_path) = match s.profiler.choose_tool(is_python(&cmd)) {
        Ok(t) => t,
        Err(tool) => return Err(missing(s, tool)),
    };
    s.profiler.missing = None;
    let output = snapshot_path(s, &config.name, tool)?;
    let out = output.display().to_string();
    let mut args: Vec<String> = match tool {
        ProfilerTool::Samply => vec!["record".into(), "--save-only".into(), "-o".into(), out],
        ProfilerTool::Perf => vec!["record".into(), "-g".into(), "-o".into(), out],
        ProfilerTool::PySpy => vec!["record".into(), "--format".into(), "speedscope".into(), "-o".into(), out],
    };
    args.push("--".into());
    args.push(resolve_program(&cmd.program));
    args.extend(cmd.args.iter().cloned());
    let wrapped = ProcessCommand {
        program: tool_path.display().to_string(),
        args,
        cwd: cmd.cwd.clone(),
        env: cmd.env.clone(),
        build: false,
    };
    let title = format!("Profile {}", config.name);
    let tab = crate::runs::start_process(s, &wrapped, &title)?;
    s.profiler.recordings.push(Recording {
        tab,
        tool,
        title,
        output,
        attached: None,
        started: Instant::now(),
        stop_requested: None,
    });
    Ok(tab)
}

/// Attach Profiler to Process: record a running process until Stop.
pub(crate) fn attach(s: &mut Session, pid: u32, name: &str, python: bool) -> Result<RunTabId, CommandError> {
    if pid <= 1 || pid == std::process::id() {
        return Err(CommandError::BadParams(format!("cannot profile process {pid}")));
    }
    let (tool, tool_path) = match s.profiler.choose_tool(python) {
        Ok(t) => t,
        Err(tool) => return Err(missing(s, tool)),
    };
    s.profiler.missing = None;
    let label = if name.is_empty() { format!("pid {pid}") } else { format!("{name} ({pid})") };
    let output = snapshot_path(s, &format!("attach-{}", if name.is_empty() { "process" } else { name }), tool)?;
    let out = output.display().to_string();
    let pid_s = pid.to_string();
    let args: Vec<String> = match tool {
        ProfilerTool::Samply => vec!["record".into(), "--save-only".into(), "-o".into(), out, "-p".into(), pid_s],
        ProfilerTool::Perf => vec!["record".into(), "-g".into(), "-o".into(), out, "-p".into(), pid_s],
        ProfilerTool::PySpy => {
            vec!["record".into(), "--format".into(), "speedscope".into(), "-o".into(), out, "--pid".into(), pid_s]
        }
    };
    let cwd = s.project_root().unwrap_or_else(std::env::temp_dir);
    let cmd = ProcessCommand { program: tool_path.display().to_string(), args, cwd, env: Vec::new(), build: false };
    let title = format!("Profile {label}");
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    if let Some(t) = s.ws.run_mut(tab) {
        t.push(
            ConsoleKind::System,
            "Recording… press Stop in the Profiler tool window to finish and open the snapshot.",
        );
    }
    s.profiler.recordings.push(Recording {
        tab,
        tool,
        title,
        output,
        attached: Some(pid),
        started: Instant::now(),
        stop_requested: None,
    });
    s.ws.layout.show(ToolWindowId::Profiler);
    Ok(tab)
}

/// Stop a recording: SIGINT to its process group so the profiler writes its snapshot (it is
/// killed if it hasn't exited after a grace period).
pub(crate) fn stop(s: &mut Session, tab: RunTabId) -> bool {
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

/// Kill recordings (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    let tabs: Vec<RunTabId> = s.profiler.recordings.iter().map(|r| r.tab).collect();
    for t in tabs {
        crate::runs::stop(s, t);
    }
    s.profiler.recordings.clear();
    s.profiler.load_rx = None;
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
    changed |= pump_recordings(s);
    changed |= pump_load(s);
    changed
}

fn pump_recordings(s: &mut Session) -> bool {
    let mut changed = false;
    let mut i = 0;
    while let Some(rec) = s.profiler.recordings.get(i) {
        let (tab, stop_requested) = (rec.tab, rec.stop_requested);
        if s.runs.is_running(tab) {
            if stop_requested.is_some_and(|t| t.elapsed() > STOP_GRACE) {
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

/// A recording's process ended: open its snapshot, or say why there is none.
fn finished(s: &mut Session, rec: Recording) {
    let written = std::fs::metadata(&rec.output).is_ok_and(|m| m.len() > 0);
    if written {
        if let Some(t) = s.ws.run_mut(rec.tab) {
            t.push(ConsoleKind::System, format!("Profiler snapshot: {}", rec.output.display()));
        }
        let perf = (rec.tool == ProfilerTool::Perf).then(|| s.profiler.find_tool(ProfilerTool::Perf)).flatten();
        spawn_load(s, rec.output, perf);
        s.ws.layout.show(ToolWindowId::Profiler);
        s.status = format!("{}: opening the snapshot", rec.title);
        return;
    }
    let mut msg = format!("{}: {} wrote no snapshot (see the Run tab for its output)", rec.title, rec.tool.program());
    if rec.attached.is_some() && cfg!(target_os = "macos") {
        msg.push_str(
            ". On macOS, attaching needs permission: run `samply setup` once (it code-signs samply), or profile from Run › Profile instead",
        );
    } else if rec.attached.is_some() && rec.tool == ProfilerTool::PySpy {
        msg.push_str(". py-spy may need root to attach (try `sudo py-spy record --pid …` in a terminal)");
    }
    if let Some(t) = s.ws.run_mut(rec.tab) {
        t.push(ConsoleKind::System, msg.clone());
    }
    s.profiler.error = Some(msg.clone());
    s.status = msg;
}

fn pump_load(s: &mut Session) -> bool {
    let Some(rx) = &s.profiler.load_rx else { return false };
    let result = match rx.try_recv() {
        Ok(r) => r,
        Err(crossbeam_channel::TryRecvError::Empty) => return false,
        Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the snapshot loader stopped".into()),
    };
    s.profiler.load_rx = None;
    let path = s.profiler.loading.take();
    match result {
        Ok(loaded) if loaded.reanalysis => {
            if let Some(snap) = &mut s.profiler.snapshot
                && Arc::ptr_eq(&snap.profile, &loaded.profile)
            {
                snap.analysis = Arc::new(loaded.analysis);
                snap.reset_view();
            }
        }
        Ok(loaded) => {
            s.profiler.remember(&loaded.path);
            let snap = Snapshot::new(loaded.path, loaded.profile, loaded.analysis);
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
        Err(e) => {
            let msg = match path {
                Some(p) => format!("{}: {e}", p.display()),
                None => e,
            };
            s.profiler.error = Some(msg.clone());
            s.status = msg;
        }
    }
    true
}

// ---------------------------------------------------------------- views for commands and the UI

/// Open the source of a frame (file:line), relative paths against the project root.
pub(crate) fn jump_to_frame(s: &mut Session, frame: u32) -> Result<(), CommandError> {
    let snap =
        s.profiler.snapshot.as_ref().ok_or_else(|| CommandError::Failed("no profiler snapshot is open".into()))?;
    let f = snap.profile.frame(frame).ok_or_else(|| CommandError::BadParams(format!("no frame {frame}")))?;
    let Some(file) = f.file.clone() else {
        return Err(CommandError::Failed(format!("no source location for {}", f.name)));
    };
    let line = f.line.unwrap_or(1).saturating_sub(1) as usize;
    let mut path = PathBuf::from(&file);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> Snapshot {
        let p = codecraft_profile::collapsed::parse(text).unwrap();
        let a = Analysis::new(&p, None);
        Snapshot::new(PathBuf::from("/x.txt"), Arc::new(p), a)
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
    }
}
