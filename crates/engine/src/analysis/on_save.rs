//! Analysis on save (M15 track EA): when files are saved, the analyzers of the active inspection
//! profile for their language run on them in the background — Ruff for Python, Clippy for Rust
//! when the user turned Clippy on (Settings › Rust) and rust-analyzer is not running (its own
//! check on save is Clippy then), clang-tidy for C/C++ when installed, plus clangd's diagnostics
//! when the clangd language server is not running (its diagnostics are live otherwise) — and
//! their findings replace the file's old ones in the editor.
//!
//! Saves are debounced (a burst of saves is one run); a save while a run is in flight cancels it
//! (its processes are killed) and the next run covers both. Nothing runs on the UI thread, and
//! Settings › Editor › Inspections › Analyze files on save turns it off.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_lsp::ServerKind;
use codecraft_text::Document;

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::inspections::{self, InspectionResults, ToolRun, Tool};
use crate::{CommandError, Session};

/// Saves closer together than this are analyzed together.
pub const DEBOUNCE: Duration = Duration::from_millis(400);
/// Files analyzed in one run (the rest wait for the next).
const MAX_FILES_PER_RUN: usize = 64;
/// Files waiting at most (older ones are dropped: a mass save is Inspect Code's job).
const MAX_QUEUE: usize = 512;

/// The outcome of a run, for agents (`analysis.fileFindings` › `onSave.last`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub run: u64,
    pub files: Vec<PathBuf>,
    /// `ruff: 2 problems`, `clippy: failed: …`.
    pub tools: Vec<String>,
    pub problems: usize,
    pub millis: u64,
    pub cancelled: bool,
}

struct Run {
    id: u64,
    files: BTreeMap<PathBuf, Option<(u64, Document)>>,
    tools: BTreeSet<Tool>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<(InspectionResults, bool)>,
    started: Instant,
}

/// Saved files waiting for analysis and the run in flight.
pub struct OnSaveState {
    /// Saved files → the document's text when saved (`None`: the file was not open).
    queue: BTreeMap<PathBuf, Option<(u64, Document)>>,
    due: Option<Instant>,
    running: Option<Run>,
    next_run: u64,
    /// How long saves are collected (tests set zero).
    pub debounce: Duration,
    /// The last few finished runs, newest last.
    pub history: Vec<RunSummary>,
}

impl Default for OnSaveState {
    fn default() -> Self {
        Self { queue: BTreeMap::new(), due: None, running: None, next_run: 0, debounce: DEBOUNCE, history: Vec::new() }
    }
}

impl std::fmt::Debug for OnSaveState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnSaveState")
            .field("queue", &self.queue.keys().collect::<Vec<_>>())
            .field("running", &self.running.as_ref().map(|r| r.files.keys().collect::<Vec<_>>()))
            .field("history", &self.history)
            .finish()
    }
}

impl OnSaveState {
    /// A run is in flight or saves are waiting.
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    pub fn pending(&self, path: &Path) -> bool {
        self.queue.contains_key(path)
    }

    fn record(&mut self, summary: RunSummary) {
        self.history.push(summary);
        if self.history.len() > 16 {
            self.history.remove(0);
        }
    }

    pub fn running_for(&self, path: &Path) -> bool {
        self.running.as_ref().is_some_and(|r| r.files.contains_key(path))
    }

    /// The newest finished run that covered `path`.
    pub fn last_for(&self, path: &Path) -> Option<&RunSummary> {
        self.history.iter().rev().find(|r| r.files.iter().any(|f| f == path))
    }

    fn cancel_running(&mut self) {
        if let Some(run) = self.running.take() {
            run.cancel.store(true, Ordering::Relaxed);
            self.record(RunSummary {
                run: run.id,
                files: run.files.keys().cloned().collect(),
                tools: run.tools.iter().map(|t| format!("{}: cancelled", t.id())).collect(),
                problems: 0,
                millis: u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                cancelled: true,
            });
            // Its files are analyzed again with the newer save.
            for (p, at) in run.files {
                self.queue.entry(p).or_insert(at);
            }
        }
    }
}

/// Whether a file's language has analyzers on save.
fn analyzable(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    inspections::PY_EXTS.contains(&ext) || ext == "rs" || inspections::C_EXTS.contains(&ext)
}

/// The analyzers to run on save for a file (before checking that they are installed).
fn wanted_tools(s: &Session, profile: &super::profiles::InspectionProfile, path: &Path) -> Vec<Tool> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if inspections::PY_EXTS.contains(&ext) {
        return if profile.analyzer_enabled(Tool::Ruff) { vec![Tool::Ruff] } else { Vec::new() };
    }
    if ext == "rs" {
        // rust-analyzer's check on save already runs Clippy when it is on.
        let ra = s.servers.contains_key(&ServerKind::RustAnalyzer);
        let cargo = s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some());
        return if s.ws.settings.use_clippy && profile.clippy_enabled() && !ra && cargo { vec![Tool::Clippy] } else { Vec::new() };
    }
    if inspections::C_EXTS.contains(&ext) {
        let mut out = Vec::new();
        if profile.analyzer_enabled(Tool::ClangTidy) {
            out.push(Tool::ClangTidy);
        }
        // clangd's diagnostics are live while the clangd language server runs.
        if profile.analyzer_enabled(Tool::Clangd) && !s.servers.contains_key(&ServerKind::Clangd) {
            out.push(Tool::Clangd);
        }
        return out;
    }
    Vec::new()
}

/// Files were saved (called from `Session::save_all`).
pub(crate) fn after_save(s: &mut Session, saved: &[PathBuf]) {
    if !s.ws.settings.analysis_on_save || s.power_save() {
        return;
    }
    let Some(root) = s.project_root() else { return };
    let mut any = false;
    for path in saved {
        let in_project = path.strip_prefix(&root).is_ok_and(|r| !inspections::in_build_output(r));
        if !in_project || !analyzable(path) {
            continue;
        }
        queue(s, path);
        any = true;
    }
    if any {
        let st = &mut s.analysis.editor.on_save;
        st.due = Some(Instant::now() + st.debounce);
        // A newer save makes the run in flight stale.
        let stale = st.running.as_ref().is_some_and(|r| r.files.keys().any(|p| st.queue.contains_key(p)));
        if stale {
            st.cancel_running();
        }
    }
}

/// A file was opened in an editor: a Python file without findings yet is analyzed right away
/// (Ruff takes milliseconds), so its problems are underlined without an edit and a save, as
/// PyCharm's inspections are when a file opens (QA M15: Cmd+S on an unmodified file saves
/// nothing, so there was no way to see them short of an edit or Inspect Code). Rust and C/C++
/// wait for a save: Clippy builds the workspace, and their language servers report as they go.
pub(crate) fn after_open(s: &mut Session, path: &Path) {
    if !s.ws.settings.analysis_on_save || s.power_save() {
        return;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !inspections::PY_EXTS.contains(&ext) || !s.analysis.editor.of(path).is_empty() {
        return;
    }
    let Some(root) = s.project_root() else { return };
    if !path.strip_prefix(&root).is_ok_and(|r| !inspections::in_build_output(r)) {
        return;
    }
    queue(s, path);
    let st = &mut s.analysis.editor.on_save;
    st.due = Some(Instant::now() + st.debounce);
}

fn queue(s: &mut Session, path: &Path) {
    let at = s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)).map(|d| (d.doc.revision(), d.doc.snapshot()));
    let st = &mut s.analysis.editor.on_save;
    if st.queue.len() >= MAX_QUEUE && !st.queue.contains_key(path) {
        return;
    }
    st.queue.insert(path.to_path_buf(), at);
}

/// Start a run when the debounce has passed; take a finished run in.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = finish(s);
    let st = &s.analysis.editor.on_save;
    if st.running.is_none() && !st.queue.is_empty() && st.due.is_none_or(|d| Instant::now() >= d) {
        changed |= start(s);
    }
    changed
}

fn start(s: &mut Session) -> bool {
    let st = &mut s.analysis.editor.on_save;
    let mut batch = BTreeMap::new();
    while batch.len() < MAX_FILES_PER_RUN {
        let Some((p, at)) = st.queue.pop_first() else { break };
        batch.insert(p, at);
    }
    st.due = (!st.queue.is_empty()).then(Instant::now);
    let profile = match super::profiles::resolve(s, None) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("analysis on save: {e}");
            return false;
        }
    };
    let mut tools: BTreeSet<Tool> = BTreeSet::new();
    let mut files: BTreeMap<PathBuf, Option<(u64, Document)>> = BTreeMap::new();
    for (path, at) in batch {
        let t = wanted_tools(s, &profile, &path);
        if t.is_empty() || !path.is_file() {
            continue;
        }
        tools.extend(t);
        files.insert(path, at);
    }
    if files.is_empty() {
        return false;
    }
    let mut spec = match inspections::spec_for_files(s, files.keys().cloned().collect(), tools.iter().copied().collect()) {
        Ok(spec) => spec,
        Err(e) => {
            log::warn!("analysis on save: {e}");
            return false;
        }
    };
    // Only installed analyzers.
    tools.retain(|t| available(&spec, *t));
    if tools.is_empty() {
        return false;
    }
    spec.tools = Some(tools.iter().copied().collect());
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-analysis-on-save".into()).spawn(move || {
        let out = inspections::run_spec(&spec, &flag);
        let _ = tx.send(out);
    });
    if let Err(e) = spawned {
        log::warn!("analysis on save: cannot start a worker: {e}");
        return false;
    }
    let st = &mut s.analysis.editor.on_save;
    st.next_run = st.next_run.saturating_add(1);
    st.running = Some(Run { id: st.next_run, files, tools, cancel, rx, started: Instant::now() });
    true
}

/// Whether the run would find the analyzer: like Inspect Code, the linters (Ruff, Clippy,
/// clang-tidy) come from overrides or `PATH`; the M14 analyzers (clangd) from overrides only in
/// isolated sessions.
fn available(spec: &inspections::JobSpec, tool: Tool) -> bool {
    if spec.programs.contains_key(&tool) {
        return true;
    }
    let isolated_only = matches!(tool, Tool::Clangd | Tool::ClangAnalyzer | Tool::Cppcheck | Tool::RustAnalyzer);
    !(isolated_only && spec.env.isolated) && inspections::find_program(spec, tool).is_some()
}

fn finish(s: &mut Session) -> bool {
    let Some(run) = &s.analysis.editor.on_save.running else { return false };
    let (results, cancelled) = match run.rx.try_recv() {
        Ok(out) => out,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => (InspectionResults::new(inspections::JobKind::Inspect, inspections::Scope::new(inspections::ScopeKind::File, None), ""), true),
    };
    let Some(run) = s.analysis.editor.on_save.running.take() else { return false };
    let summary_tools: Vec<String> = results.tools.iter().map(|t| format!("{}: {}", t.tool.id(), t.outcome)).collect();
    let mut summary = RunSummary {
        run: run.id,
        files: run.files.keys().cloned().collect(),
        tools: summary_tools,
        problems: 0,
        millis: u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cancelled,
    };
    if !cancelled {
        // Analyzers that ran (a failed one keeps the file's old findings).
        let ok: BTreeSet<Tool> = results.tools.iter().filter(|t| t.ok).map(|t: &ToolRun| t.tool).collect();
        let ran: BTreeSet<Tool> = run.tools.iter().copied().filter(|t| ok.contains(t)).collect();
        for (path, at) in &run.files {
            let found: Vec<inspections::Finding> = results.findings.iter().filter(|f| &f.path == path).cloned().collect();
            summary.problems = summary.problems.saturating_add(found.len());
            super::editor::take_saved(s, path, &ran, found, at.as_ref(), run.id);
        }
    }
    s.analysis.editor.on_save.record(summary);
    true
}

/// Stop the run in flight (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    let st = &mut s.analysis.editor.on_save;
    st.queue.clear();
    st.cancel_running();
    st.queue.clear();
}

// ---------------------------------------------------------------- commands

fn analyze_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = match crate::params::opt_path(p, "path") {
        Some(p) => {
            let p = if p.is_absolute() { p } else { s.project_root().map(|r| r.join(&p)).unwrap_or(p) };
            crate::normalize_path(&p)
        }
        None => s
            .ws
            .active()
            .and_then(|(_, d)| d.path.clone())
            .ok_or_else(|| CommandError::BadParams("`path` is required (no file is open)".into()))?,
    };
    if !path.is_file() {
        return Err(CommandError::Failed(format!("{} is not a file", path.display())));
    }
    let profile = super::profiles::resolve(s, None).map_err(CommandError::Failed)?;
    let tools = wanted_tools(s, &profile, &path);
    if tools.is_empty() {
        return Err(CommandError::Failed(format!(
            "no analyzer runs on save for {} (Python: Ruff; Rust: Clippy when Settings › Rust › External linter is Clippy and rust-analyzer is not running; C/C++: clang-tidy or clangd); use analysis.run for the rest",
            path.display()
        )));
    }
    // Analyzers read the disk.
    if let Some(id) = s.ws.doc_by_path(&path)
        && s.ws.doc(id).is_some_and(|d| d.doc.is_modified())
    {
        s.save_all()?;
    }
    queue(s, &path);
    let st = &mut s.analysis.editor.on_save;
    st.due = Some(Instant::now());
    if st.running.as_ref().is_some_and(|r| r.files.contains_key(&path)) {
        st.cancel_running();
    }
    Ok(json!({
        "queued": path,
        "analyzers": tools.iter().map(|t| t.id()).collect::<Vec<_>>(),
        "next": "read the findings with analysis.fileFindings {path} once onSave.pending and onSave.running are false",
    }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "analysis.analyzeFile",
        "Analyze File Now",
        None,
        "{path?: string (default: the active file)} — run the file's analyzers on save now (Ruff, Clippy, clang-tidy/clangd per the active inspection profile), in the background; the findings show in the editor and in analysis.fileFindings",
        Context::Global,
        always,
        analyze_file,
    )]
}
