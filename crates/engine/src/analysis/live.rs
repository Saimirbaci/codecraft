//! Analysis while typing (M16 track LT): the analyzers re-run on the unsaved buffer shortly
//! after the last edit, like PyCharm's on-the-fly inspections.
//!
//! - **Python**: Ruff on the buffer through stdin (`ruff check --stdin-filename <path> -`, the
//!   inspection profile's rules). The language server's findings (basedpyright/pyright) are live
//!   through LSP already.
//! - **C/C++**: clang-tidy on a temporary copy of the buffer, compiled with the file's real
//!   command from `compile_commands.json` (the file argument rewritten to the copy, the file's own
//!   directory added to the include path), only when clang-tidy is installed. clangd's
//!   diagnostics are live through LSP already.
//! - **SQL**: the SQL inspections of `.sql` files and database consoles run in `sql_lang.rs`; the
//!   settings here switch them and set their delay.
//! - **Rust**: rust-analyzer's diagnostics are live; Clippy stays on save. Nothing runs per
//!   keystroke here.
//!
//! Each pump compares the open documents' revisions with the last ones seen (a map lookup per
//! open document: nothing is copied or re-lexed per keystroke). An edit (re)arms the file's timer
//! (Settings › Editor › Inspections › delay, 500 ms by default) and cancels a run in flight for
//! an older revision (its process is killed). When the timer fires, a cheap snapshot of the
//! document (the rope is shared) goes to a worker thread, which turns it into text, runs the
//! analyzers and sends the findings back tagged with the revision they were computed for. A
//! result for an older revision than the document's is mapped forward through the change log
//! (findings whose text was edited are dropped) and never replaces findings computed for a newer
//! revision (`editor::take_live`). Files are analyzed when they open, too.
//!
//! Power Save mode pauses everything (runs in flight are cancelled; edits made meanwhile are
//! analyzed when it ends).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_text::Document;
use codecraft_workspace::DocId;

use super::cpp::{self, CompileCommand};
use crate::Session;
use crate::inspections::{self, CANCELLED, Finding, JobSpec, ProcOutput, Tool, ToolRun};

/// Runs in flight at most (other due files wait their turn).
pub const MAX_CONCURRENT: usize = 2;
/// Larger buffers are analyzed on save only.
pub const MAX_LIVE_CHARS: usize = 4_000_000;
/// A run taking longer than this is stopped (a hung analyzer must not block the file forever).
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// Analyzer output kept (stdout).
const MAX_OUTPUT_BYTES: usize = 32 << 20;
/// Stderr lines kept.
const MAX_STDERR_LINES: usize = 200;
/// Finished runs remembered (agents read the newest per file).
const MAX_HISTORY: usize = 32;
/// Files tracked at most (open documents; a mass open is not analyzed all at once).
const MAX_FILES: usize = 256;

/// Languages analyzed while typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveLang {
    Python,
    Cpp,
    Sql,
}

impl LiveLang {
    pub fn id(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Cpp => "cpp",
            Self::Sql => "sql",
        }
    }

    /// The analyzers this module runs for the language (SQL runs in `sql_lang.rs`).
    pub fn tools(self) -> &'static [Tool] {
        match self {
            Self::Python => &[Tool::Ruff],
            Self::Cpp => &[Tool::ClangTidy],
            Self::Sql => &[],
        }
    }
}

/// The live language of a file, by extension.
pub fn lang_of(path: &Path) -> Option<LiveLang> {
    let ext = path.extension().and_then(|e| e.to_str())?.to_ascii_lowercase();
    if inspections::PY_EXTS.contains(&ext.as_str()) {
        Some(LiveLang::Python)
    } else if inspections::C_EXTS.contains(&ext.as_str()) {
        Some(LiveLang::Cpp)
    } else if super::sql::EXTS.contains(&ext.as_str()) {
        Some(LiveLang::Sql)
    } else {
        None
    }
}

/// Whether the setting turns analysis while typing on for a language (ignoring Power Save).
pub fn setting(settings: &codecraft_workspace::Settings, lang: LiveLang) -> bool {
    match lang {
        LiveLang::Python => settings.analysis_while_typing_python,
        LiveLang::Cpp => settings.analysis_while_typing_cpp,
        LiveLang::Sql => settings.analysis_while_typing_sql,
    }
}

/// Whether a language is analyzed while typing now (the setting, and not in Power Save mode).
pub fn enabled(s: &Session, lang: LiveLang) -> bool {
    !s.power_save() && setting(&s.ws.settings, lang)
}

/// How long after the last edit the analyzers start.
pub fn delay(s: &Session) -> Duration {
    Duration::from_millis(s.ws.settings.analysis_while_typing_delay_ms.min(10_000))
}

/// Whether the analysis while typing covers `tool` for the open document at `path` (the
/// analysis on save then leaves it out: the buffer is already analyzed).
pub(crate) fn covers(s: &Session, path: &Path, tool: Tool) -> bool {
    let Some(lang) = lang_of(path) else { return false };
    lang.tools().contains(&tool)
        && enabled(s, lang)
        && s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)).is_some_and(|d| d.doc.len_chars() <= MAX_LIVE_CHARS)
}

/// The outcome of a run, for agents (`analysis.fileFindings` › `live.last`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummary {
    pub run: u64,
    pub path: PathBuf,
    /// The document revision the run analyzed.
    pub revision: u64,
    /// `ruff: 2 problems`, `clang-tidy: failed: …`, `ruff: cancelled`.
    pub tools: Vec<String>,
    pub problems: usize,
    pub millis: u64,
    pub cancelled: bool,
    /// The document had moved on when the results came: they were mapped forward through the
    /// edits (findings whose text changed were dropped).
    pub mapped: bool,
}

/// What a worker sends back.
struct LiveOutput {
    findings: Vec<Finding>,
    tools: Vec<ToolRun>,
    cancelled: bool,
}

struct LiveRun {
    id: u64,
    doc: DocId,
    revision: u64,
    snapshot: Document,
    tools: BTreeSet<Tool>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<LiveOutput>,
    started: Instant,
}

#[derive(Clone, Debug)]
struct LiveFile {
    doc: DocId,
    lang: LiveLang,
    /// The last revision seen.
    seen: u64,
    /// When the next run starts (`None`: nothing to do).
    due: Option<Instant>,
    /// The revision of the newest results taken in.
    computed: Option<u64>,
    /// The analyzers of the last run started (empty: none installed or enabled).
    analyzers: Vec<Tool>,
}

/// The open documents analyzed while typing, and the runs in flight.
#[derive(Default)]
pub struct LiveState {
    files: BTreeMap<PathBuf, LiveFile>,
    runs: BTreeMap<PathBuf, LiveRun>,
    next_run: u64,
    /// Runs started in this session (tests and the performance check count them).
    pub started: u64,
    /// The last finished (or cancelled) runs, newest last.
    pub history: Vec<LiveSummary>,
}

impl std::fmt::Debug for LiveState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveState")
            .field("files", &self.files)
            .field("running", &self.runs.iter().map(|(p, r)| (p, r.revision)).collect::<Vec<_>>())
            .field("started", &self.started)
            .field("history", &self.history)
            .finish()
    }
}

impl LiveState {
    /// A run is due or in flight (the UI keeps pumping).
    pub fn busy(&self) -> bool {
        !self.runs.is_empty() || self.files.values().any(|f| f.due.is_some())
    }

    /// A run for `path` is due (the delay after the last edit has not passed yet).
    pub fn pending(&self, path: &Path) -> bool {
        self.files.get(path).is_some_and(|f| f.due.is_some())
    }

    /// A run for `path` is in flight.
    pub fn running(&self, path: &Path) -> bool {
        self.runs.contains_key(path)
    }

    /// A worker result is queued for `path`, awaiting the next pump (nonblocking).
    pub fn result_ready(&self, path: &Path) -> bool {
        self.runs.get(path).is_some_and(|run| !run.rx.is_empty())
    }

    /// The revision of the newest results for `path`.
    pub fn computed_for(&self, path: &Path) -> Option<u64> {
        self.files.get(path).and_then(|f| f.computed)
    }

    /// The analyzers the last run of `path` used.
    pub fn analyzers(&self, path: &Path) -> Vec<Tool> {
        self.files.get(path).map(|f| f.analyzers.clone()).unwrap_or_default()
    }

    /// The newest finished run of `path`.
    pub fn last_for(&self, path: &Path) -> Option<&LiveSummary> {
        self.history.iter().rev().find(|r| r.path == path)
    }

    fn record(&mut self, summary: LiveSummary) {
        self.history.push(summary);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    /// Cancel the run of `path` (its process is killed); the file stays due.
    fn cancel(&mut self, path: &Path) {
        if let Some(run) = self.runs.remove(path) {
            run.cancel.store(true, Ordering::Relaxed);
            self.record(LiveSummary {
                run: run.id,
                path: path.to_path_buf(),
                revision: run.revision,
                tools: run.tools.iter().map(|t| format!("{}: cancelled", t.id())).collect(),
                problems: 0,
                millis: millis(run.started),
                cancelled: true,
                mapped: false,
            });
        }
    }

    fn cancel_all(&mut self) {
        let paths: Vec<PathBuf> = self.runs.keys().cloned().collect();
        for p in paths {
            self.cancel(&p);
            // Analyzed again when the pause ends.
            if let Some(f) = self.files.get_mut(&p) {
                f.due.get_or_insert_with(Instant::now);
            }
        }
    }
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Analyze `path` now (`analysis.analyzeFile`): the run in flight is cancelled and a new one
/// starts with the next pump. Returns the analyzers the language has live.
pub(crate) fn run_now(s: &mut Session, path: &Path) -> Vec<Tool> {
    let Some(lang) = lang_of(path).filter(|l| *l != LiveLang::Sql && enabled(s, *l)) else { return Vec::new() };
    let Some(id) = s.ws.doc_by_path(path) else { return Vec::new() };
    let Some(rev) = s.ws.doc(id).map(|d| d.doc.revision()) else { return Vec::new() };
    let st = &mut s.analysis.editor.live;
    st.cancel(path);
    let f = st.files.entry(path.to_path_buf()).or_insert(LiveFile {
        doc: id,
        lang,
        seen: rev,
        due: None,
        computed: None,
        analyzers: Vec::new(),
    });
    f.doc = id;
    f.seen = rev;
    f.due = Some(Instant::now());
    lang.tools().to_vec()
}

/// Take finished runs in, follow edits, start due runs. Returns whether findings changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let changed = finish(s);
    if s.power_save() {
        s.analysis.editor.live.cancel_all();
        return changed;
    }
    scan(s);
    start_due(s);
    changed
}

/// Compare the open documents' revisions with the last ones seen: an edit arms the timer and
/// cancels a stale run; a newly opened document is due at once (analysis on open).
fn scan(s: &mut Session) {
    let now = Instant::now();
    let delay = delay(s);
    let root = s.project_root();
    let settings = &s.ws.settings;
    let ws = &s.ws;
    let st = &mut s.analysis.editor.live;
    for d in ws.docs() {
        let Some(path) = d.path.as_ref() else { continue };
        let rev = d.doc.revision();
        // The common case (nothing new): one map lookup.
        if let Some(f) = st.files.get(path)
            && f.doc == d.id
            && f.seen == rev
        {
            continue;
        }
        let Some(lang) = lang_of(path) else { continue };
        if lang == LiveLang::Sql || !setting(settings, lang) {
            continue;
        }
        let in_project = root
            .as_deref()
            .is_some_and(|r| path.strip_prefix(r).is_ok_and(|rel| !inspections::in_build_output(rel)));
        if !in_project || d.doc.len_chars() > MAX_LIVE_CHARS {
            continue;
        }
        match st.files.get_mut(path) {
            Some(f) if f.doc == d.id => {
                f.seen = rev;
                f.due = Some(now + delay);
            }
            _ => {
                if st.files.len() >= MAX_FILES && !st.files.contains_key(path) {
                    continue;
                }
                st.files.insert(
                    path.clone(),
                    LiveFile { doc: d.id, lang, seen: rev, due: Some(now), computed: None, analyzers: Vec::new() },
                );
            }
        }
        // A newer edit makes the run in flight stale.
        if st.runs.get(path).is_some_and(|r| r.revision != rev || r.doc != d.id) {
            st.cancel(path);
        }
    }
    // Closed documents and languages turned off.
    let gone: Vec<PathBuf> = st
        .files
        .iter()
        .filter(|(p, f)| {
            !ws.doc(f.doc).is_some_and(|d| d.path.as_ref() == Some(*p)) || !setting(settings, f.lang)
        })
        .map(|(p, _)| p.clone())
        .collect();
    for p in gone {
        st.cancel(&p);
        st.files.remove(&p);
    }
}

fn start_due(s: &mut Session) {
    let now = Instant::now();
    let st = &s.analysis.editor.live;
    let free = MAX_CONCURRENT.saturating_sub(st.runs.len());
    if free == 0 {
        return;
    }
    let due: Vec<PathBuf> = st
        .files
        .iter()
        .filter(|(p, f)| f.due.is_some_and(|d| d <= now) && !st.runs.contains_key(*p))
        .map(|(p, _)| p.clone())
        .take(free)
        .collect();
    for path in due {
        start(s, &path);
    }
}

fn start(s: &mut Session, path: &Path) {
    let Some(f) = s.analysis.editor.live.files.get_mut(path) else { return };
    f.due = None;
    let (doc_id, lang) = (f.doc, f.lang);
    let Some((revision, snapshot)) = s.ws.doc(doc_id).map(|d| (d.doc.revision(), d.doc.snapshot())) else { return };
    let profile = match super::profiles::resolve(s, None) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("analysis while typing: {e}");
            return;
        }
    };
    let wanted: Vec<Tool> = lang.tools().iter().copied().filter(|t| profile.analyzer_enabled(*t)).collect();
    let spec = if wanted.is_empty() {
        None
    } else {
        match inspections::spec_for_files(s, vec![path.to_path_buf()], wanted.clone()) {
            Ok(spec) => Some(spec),
            Err(e) => {
                log::warn!("analysis while typing: {e}");
                None
            }
        }
    };
    let programs: Vec<(Tool, PathBuf)> = spec
        .as_ref()
        .map(|spec| {
            wanted
                .iter()
                .filter(|t| super::on_save::available(spec, **t))
                .filter_map(|t| inspections::find_program(spec, *t).map(|p| (*t, p)))
                .collect()
        })
        .unwrap_or_default();
    let st = &mut s.analysis.editor.live;
    if let Some(f) = st.files.get_mut(path) {
        f.analyzers = programs.iter().map(|(t, _)| *t).collect();
    }
    let Some(spec) = spec.filter(|_| !programs.is_empty()) else { return };
    st.next_run = st.next_run.saturating_add(1);
    let id = st.next_run;
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let job = Job { spec, path: path.to_path_buf(), snapshot: snapshot.clone(), programs, run: id };
    let spawned = std::thread::Builder::new().name("codecraft-analysis-live".into()).spawn(move || {
        let out = job.run(&flag);
        let _ = tx.send(out);
    });
    if let Err(e) = spawned {
        log::warn!("analysis while typing: cannot start a worker: {e}");
        return;
    }
    st.started = st.started.saturating_add(1);
    let tools = st.files.get(path).map(|f| f.analyzers.iter().copied().collect()).unwrap_or_default();
    st.runs.insert(
        path.to_path_buf(),
        LiveRun { id, doc: doc_id, revision, snapshot, tools, cancel, rx, started: Instant::now() },
    );
}

/// Take finished runs in. Returns whether findings changed.
fn finish(s: &mut Session) -> bool {
    let mut changed = false;
    let paths: Vec<PathBuf> = s.analysis.editor.live.runs.keys().cloned().collect();
    for path in paths {
        let Some(run) = s.analysis.editor.live.runs.get(&path) else { continue };
        let out = match run.rx.try_recv() {
            Ok(out) => out,
            Err(TryRecvError::Empty) if run.started.elapsed() < RUN_TIMEOUT => continue,
            Err(TryRecvError::Empty) => {
                // Hung: stop it; the file is analyzed again with its next edit.
                s.analysis.editor.live.cancel(&path);
                continue;
            }
            Err(TryRecvError::Disconnected) => LiveOutput { findings: Vec::new(), tools: Vec::new(), cancelled: true },
        };
        let Some(run) = s.analysis.editor.live.runs.remove(&path) else { continue };
        let mut summary = LiveSummary {
            run: run.id,
            path: path.clone(),
            revision: run.revision,
            tools: out.tools.iter().map(|t| format!("{}: {}", t.tool.id(), t.outcome)).collect(),
            problems: 0,
            millis: millis(run.started),
            cancelled: out.cancelled,
            mapped: false,
        };
        let current = s.ws.doc(run.doc).filter(|d| d.path.as_ref() == Some(&path)).map(|d| d.doc.revision());
        if !out.cancelled && current.is_some() {
            let ok: BTreeSet<Tool> = out.tools.iter().filter(|t| t.ok).map(|t| t.tool).collect();
            let ran: BTreeSet<Tool> = run.tools.iter().copied().filter(|t| ok.contains(t)).collect();
            summary.mapped = current != Some(run.revision);
            let at = (run.revision, run.snapshot);
            let n = out.findings.len();
            if super::editor::take_live(s, &path, &ran, out.findings, &at, run.id) {
                summary.problems = n;
                changed = true;
                if let Some(f) = s.analysis.editor.live.files.get_mut(&path) {
                    f.computed = Some(f.computed.map_or(run.revision, |c| c.max(run.revision)));
                }
            }
        }
        s.analysis.editor.live.record(summary);
    }
    changed
}

/// Stop every run (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    let st = &mut s.analysis.editor.live;
    st.cancel_all();
    for f in st.files.values_mut() {
        f.due = None;
    }
}

/// The live state of one file for agents (`analysis.fileFindings` › `live`).
pub(crate) fn status_json(s: &Session, path: &Path) -> Value {
    let lang = lang_of(path);
    let st = &s.analysis.editor.live;
    let doc = s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id));
    if lang == Some(LiveLang::Sql) {
        // The SQL inspections run in `sql_lang.rs`.
        let computed = doc.and_then(|d| s.sql_lang.checked_revision(d.id));
        let pending = doc.is_some_and(|d| computed != Some(d.doc.revision()) && enabled(s, LiveLang::Sql));
        return json!({
            "language": "sql",
            "enabled": enabled(s, LiveLang::Sql),
            "analyzers": ["sql"],
            "computedFor": computed,
            "pending": pending,
            "running": false,
            "delayMs": s.ws.settings.analysis_while_typing_delay_ms.min(10_000),
        });
    }
    json!({
        "language": lang.map(LiveLang::id),
        "enabled": lang.is_some_and(|l| enabled(s, l)),
        "powerSave": s.power_save(),
        "analyzers": st.analyzers(path).iter().map(|t| t.id()).collect::<Vec<_>>(),
        "computedFor": st.computed_for(path),
        "pending": st.pending(path),
        "running": st.running(path),
        "delayMs": s.ws.settings.analysis_while_typing_delay_ms.min(10_000),
        "last": st.last_for(path),
    })
}

// ---------------------------------------------------------------- the worker

/// Everything a run needs (built on the UI thread).
struct Job {
    spec: JobSpec,
    path: PathBuf,
    snapshot: Document,
    programs: Vec<(Tool, PathBuf)>,
    run: u64,
}

impl Job {
    fn run(self, cancel: &AtomicBool) -> LiveOutput {
        // The text is made here, off the UI thread.
        let text = self.snapshot.text();
        let mut findings = Vec::new();
        let mut tools = Vec::new();
        for (tool, program) in &self.programs {
            if cancel.load(Ordering::Relaxed) {
                return LiveOutput { findings: Vec::new(), tools, cancelled: true };
            }
            let res = match tool {
                Tool::Ruff => ruff_buffer(&self.spec, program, &self.path, &text, cancel),
                Tool::ClangTidy => tidy_buffer(&self.spec, program, &self.path, &text, self.run, cancel),
                _ => continue,
            };
            match res {
                Ok((run, mut found)) => {
                    tools.push(run);
                    findings.append(&mut found);
                }
                Err(e) if e == CANCELLED => return LiveOutput { findings: Vec::new(), tools, cancelled: true },
                Err(e) => tools.push(ToolRun { tool: *tool, command_line: String::new(), outcome: format!("failed: {e}"), ok: false }),
            }
        }
        // The profile's switches and severities, one report per problem.
        let findings = super::analyzers::postprocess(&self.spec, findings);
        let findings: Vec<Finding> = findings.into_iter().filter(|f| f.path == self.path).collect();
        LiveOutput { findings, tools, cancelled: cancel.load(Ordering::Relaxed) }
    }
}

fn command_line(program: &Path, args: &[String]) -> String {
    format!(
        "{} {}",
        program.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
        args.iter().map(|a| codecraft_cargo::shell_quote(a)).collect::<Vec<_>>().join(" ")
    )
}

/// `ruff check --stdin-filename <path> -` on the buffer.
fn ruff_buffer(spec: &JobSpec, program: &Path, path: &Path, text: &str, cancel: &AtomicBool) -> Result<(ToolRun, Vec<Finding>), String> {
    let tail = ["--stdin-filename".to_string(), path.display().to_string(), "-".to_string()];
    let args = inspections::linters::args(Tool::Ruff, spec, &tail, inspections::PY_EXTS);
    let out = run_stdin(program, &args, &spec.root, text, cancel)?;
    let (found, failure) = inspections::linters::parse(Tool::Ruff, spec, program, &out, cancel);
    let n = found.len();
    let (outcome, ok) = match failure {
        Some(f) => (format!("failed: {f}"), false),
        None => (inspections::count_label(n), true),
    };
    Ok((ToolRun { tool: Tool::Ruff, command_line: command_line(program, &args), outcome, ok }, found))
}

/// A temporary directory for one clang-tidy run (removed when dropped).
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || crate::normalize_path(a) == crate::normalize_path(b)
}

/// The file's compile command with the source argument pointing at `copy` and `dir` (the
/// file's own directory, for its `#include "…"`s) first on the include path.
pub(crate) fn rewrite_command(cmd: &CompileCommand, copy: &Path, dir: &Path) -> CompileCommand {
    let file_name = cmd.file.file_name().map(|n| n.to_string_lossy().into_owned());
    let mut arguments: Vec<String> = Vec::with_capacity(cmd.arguments.len() + 2);
    let mut replaced = false;
    for (i, a) in cmd.arguments.iter().enumerate() {
        if i == 0 {
            arguments.push(a.clone());
            arguments.push(format!("-I{}", dir.display()));
            continue;
        }
        let p = Path::new(a);
        let resolved = if p.is_absolute() { p.to_path_buf() } else { cmd.directory.join(p) };
        let is_file = !a.starts_with('-') && (same_path(&resolved, &cmd.file) || file_name.as_deref() == Some(a.as_str()));
        if is_file {
            arguments.push(copy.display().to_string());
            replaced = true;
        } else {
            arguments.push(a.clone());
        }
    }
    if !replaced {
        arguments.push(copy.display().to_string());
    }
    CompileCommand { directory: cmd.directory.clone(), file: copy.to_path_buf(), arguments }
}

/// The `.clang-tidy` that applies to `path`: the nearest one in its directory or above.
fn tidy_config(path: &Path) -> Option<PathBuf> {
    path.parent()?.ancestors().take(64).map(|d| d.join(".clang-tidy")).find(|p| p.is_file())
}

/// clang-tidy on a temporary copy of the buffer, compiled like the real file.
fn tidy_buffer(
    spec: &JobSpec,
    program: &Path,
    path: &Path,
    text: &str,
    run: u64,
    cancel: &AtomicBool,
) -> Result<(ToolRun, Vec<Finding>), String> {
    let base = std::env::temp_dir().join(format!("codecraft-live-{}-{run}", std::process::id()));
    std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    let tmp = TempDir(base.canonicalize().unwrap_or(base));
    let name = path.file_name().ok_or("the file has no name")?;
    let copy = tmp.0.join(name);
    std::fs::write(&copy, text).map_err(|e| format!("{}: {e}", copy.display()))?;
    let dir = path.parent().unwrap_or(&spec.root).to_path_buf();
    let fixes = tmp.0.join("fixes.yaml");
    let mut args = Vec::new();
    // clang-tidy looks for `.clang-tidy` next to the file it analyzes: the copy's directory has
    // none, so the real file's is named.
    if let Some(config) = tidy_config(path) {
        args.push(format!("--config-file={}", config.display()));
    }
    let config = tidy_config(path);
    match spec.profile.clang_tidy_checks() {
        Some(checks) => args.push(format!("--checks={checks}")),
        None if config.is_none() => args.push(format!("--checks={}", inspections::linters::DEFAULT_TIDY_CHECKS)),
        None => {}
    }
    args.push(format!("--export-fixes={}", fixes.display()));
    let entry = super::analyzers::compile_db_dir(spec)
        .and_then(|d| cpp::load_compile_db(&d).ok())
        .and_then(|db| db.into_iter().find(|c| same_path(&c.file, path)));
    match entry {
        Some(cmd) => {
            let cmd = rewrite_command(&cmd, &copy, &dir);
            let db = json!([{ "directory": cmd.directory, "file": cmd.file, "arguments": cmd.arguments }]);
            std::fs::write(tmp.0.join("compile_commands.json"), db.to_string()).map_err(|e| e.to_string())?;
            args.push(copy.display().to_string());
            args.extend(["-p".to_string(), tmp.0.display().to_string()]);
        }
        None => {
            // No compile command: the defaults, with the file's directory and the usual include
            // directories (like Inspect Code without a compilation database).
            args.push(copy.display().to_string());
            args.push("--".into());
            args.push(format!("-I{}", dir.display()));
            for inc in [spec.root.clone(), spec.root.join("include"), spec.root.join("src")] {
                if inc.is_dir() && inc != dir {
                    args.push(format!("-I{}", inc.display()));
                }
            }
            if path.extension().and_then(|e| e.to_str()).is_some_and(|e| ["cc", "cpp", "cxx", "c++", "mm"].contains(&e)) {
                args.push("-std=c++17".into());
            }
        }
    }
    let out = super::analyzers::run_capture(program, &args, &spec.root, &[], cancel)?;
    let mut all = out.stdout.join("\n");
    all.push('\n');
    all.push_str(&out.stderr.join("\n"));
    let buffer = text.to_string();
    let read = |p: &Path| if same_path(p, &copy) { Some(buffer.clone()) } else { codecraft_fs::read_text_file(p).ok() };
    let mut found = inspections::linters::parse_clang_tidy(&all, &spec.root, &read);
    if let Ok(meta) = std::fs::metadata(&fixes)
        && meta.len() <= 32 << 20
        && let Ok(yaml) = std::fs::read_to_string(&fixes)
    {
        cpp::attach_tidy_fixes(&mut found, &yaml, &read);
    }
    // The buffer's findings, on the real file.
    found.retain(|f| same_path(&f.path, &copy));
    for f in &mut found {
        f.path = path.to_path_buf();
        for fix in &mut f.fixes {
            for c in &mut fix.changes {
                if same_path(&c.path, &copy) {
                    c.path = path.to_path_buf();
                }
            }
        }
    }
    let failed = found.is_empty() && !out.success && out.code != Some(1);
    let (outcome, ok) =
        if failed { (format!("failed: {}", out.summary()), false) } else { (inspections::count_label(found.len()), true) };
    Ok((ToolRun { tool: Tool::ClangTidy, command_line: command_line(program, &args), outcome, ok }, found))
}

/// Run a program with `input` on stdin to completion; killed when `cancel` is set or it runs
/// past [`RUN_TIMEOUT`].
pub(crate) fn run_stdin(program: &Path, args: &[String], cwd: &Path, input: &str, cancel: &AtomicBool) -> Result<ProcOutput, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if cwd.is_dir() {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", program.display()))?;
    // Write and read on their own threads: the program may fill a pipe before reading stdin.
    let writer = child.stdin.take().map(|mut stdin| {
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        })
    });
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                        kept.extend_from_slice(buf.get(..n.min(room)).unwrap_or(&[]));
                    }
                }
            }
            kept
        })
    });
    let err_reader = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 16 * 1024];
            loop {
                match err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = (1usize << 20).saturating_sub(kept.len());
                        kept.extend_from_slice(buf.get(..n.min(room)).unwrap_or(&[]));
                    }
                }
            }
            kept
        })
    });
    let started = Instant::now();
    let status = loop {
        if cancel.load(Ordering::Relaxed) || started.elapsed() > RUN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancel.load(Ordering::Relaxed) { CANCELLED.into() } else { "timed out".into() });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                return Err(e.to_string());
            }
        }
    };
    if let Some(w) = writer {
        let _ = w.join();
    }
    let stdout = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    let stderr = err_reader.and_then(|r| r.join().ok()).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    let mut err_lines: Vec<String> = stderr.lines().map(str::to_string).collect();
    if err_lines.len() > MAX_STDERR_LINES {
        err_lines.drain(..err_lines.len() - MAX_STDERR_LINES);
    }
    Ok(ProcOutput {
        stdout: stdout.lines().map(str::to_string).collect(),
        stderr: err_lines,
        success: status.success(),
        code: status.code(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn languages_by_extension() {
        assert_eq!(lang_of(Path::new("/p/a.py")), Some(LiveLang::Python));
        assert_eq!(lang_of(Path::new("/p/a.PYI")), Some(LiveLang::Python));
        assert_eq!(lang_of(Path::new("/p/a.cpp")), Some(LiveLang::Cpp));
        assert_eq!(lang_of(Path::new("/p/a.c")), Some(LiveLang::Cpp));
        assert_eq!(lang_of(Path::new("/p/q.sql")), Some(LiveLang::Sql));
        assert_eq!(lang_of(Path::new("/p/a.rs")), None);
        assert_eq!(lang_of(Path::new("/p/Makefile")), None);
        assert_eq!(LiveLang::Python.tools(), &[Tool::Ruff]);
        assert_eq!(LiveLang::Cpp.tools(), &[Tool::ClangTidy]);
        assert!(LiveLang::Sql.tools().is_empty());
    }

    #[test]
    fn compile_commands_point_at_the_copy() {
        let cmd = CompileCommand {
            directory: PathBuf::from("/w/build"),
            file: PathBuf::from("/w/src/main.cpp"),
            arguments: ["c++", "-std=c++20", "-I/w/include", "-c", "../src/main.cpp", "-o", "main.o"].map(String::from).to_vec(),
        };
        let r = rewrite_command(&cmd, Path::new("/tmp/x/main.cpp"), Path::new("/w/src"));
        assert_eq!(r.file, PathBuf::from("/tmp/x/main.cpp"));
        assert_eq!(r.directory, PathBuf::from("/w/build"));
        assert_eq!(
            r.arguments,
            ["c++", "-I/w/src", "-std=c++20", "-I/w/include", "-c", "/tmp/x/main.cpp", "-o", "main.o"].map(String::from).to_vec()
        );
        // A command naming the file differently still compiles the copy.
        let odd = CompileCommand { arguments: vec!["cc".into(), "-c".into()], ..cmd };
        let r = rewrite_command(&odd, Path::new("/tmp/x/main.cpp"), Path::new("/w/src"));
        assert_eq!(r.arguments.last().map(String::as_str), Some("/tmp/x/main.cpp"));
        // Hostile: an empty command.
        let empty = CompileCommand { directory: PathBuf::new(), file: PathBuf::new(), arguments: Vec::new() };
        assert_eq!(rewrite_command(&empty, Path::new("/c.cpp"), Path::new("/")).arguments, vec!["/c.cpp".to_string()]);
    }

    fn ruff_finding(path: &Path, line: usize) -> Finding {
        let mut f = Finding::new(Tool::Ruff, "F401 unused-import", codecraft_workspace::Severity::Warning, path.to_path_buf(), codecraft_text::Position::new(line, 7));
        f.end = codecraft_text::Position::new(line, 9);
        f.code = Some("F401".into());
        f.message = "`os` imported but unused".into();
        f
    }

    #[test]
    fn an_older_result_never_replaces_newer_findings() {
        let mut s = Session::new();
        let path = PathBuf::from("/p/app.py");
        let id = s.ws.add_document(Some(path.clone()), "import os\nimport sys\n");
        let snap = |s: &Session| s.ws.doc(id).map(|d| (d.doc.revision(), d.doc.snapshot())).unwrap();
        let old = snap(&s);
        // Edit: a line inserted at the top (revision 1).
        let sel = codecraft_text::SelectionSet::caret(0);
        let edit = codecraft_text::Edit { changes: codecraft_text::ChangeSet::single(0, 0, "# c\n"), selections: sel.clone() };
        s.ws.doc_mut(id).unwrap().doc.apply(&edit, &sel, codecraft_text::EditGroup::Standalone).unwrap();
        let new = snap(&s);
        assert!(new.0 > old.0);
        let ruff: BTreeSet<Tool> = [Tool::Ruff].into();
        // The newer run's result (`sys` on line 2 of the new text) is taken in…
        assert!(super::super::editor::take_live(&mut s, &path, &ruff, vec![ruff_finding(&path, 2)], &new, 2));
        // …and the older run's, arriving later, is not.
        assert!(!super::super::editor::take_live(&mut s, &path, &ruff, vec![ruff_finding(&path, 0)], &old, 1));
        let lines: Vec<usize> = s.analysis.editor.of(&path).iter().map(|e| e.finding.start.line).collect();
        assert_eq!(lines, vec![2]);
        assert!(matches!(s.analysis.editor.of(&path)[0].origin, super::super::editor::Origin::Live { run: 2, revision } if revision == new.0));
        // Nor an on-save result for the older revision.
        super::super::editor::take_saved(&mut s, &path, &ruff, vec![ruff_finding(&path, 0)], Some(&old), 9);
        assert_eq!(s.analysis.editor.of(&path).len(), 1);
        assert_eq!(s.analysis.editor.of(&path)[0].finding.start.line, 2);
        // A result for an older revision with nothing newer is mapped forward (line 0 → 1).
        let other = PathBuf::from("/p/other.py");
        let id2 = s.ws.add_document(Some(other.clone()), "import os\n");
        let old2 = s.ws.doc(id2).map(|d| (d.doc.revision(), d.doc.snapshot())).unwrap();
        s.ws.doc_mut(id2).unwrap().doc.apply(&edit, &sel, codecraft_text::EditGroup::Standalone).unwrap();
        assert!(super::super::editor::take_live(&mut s, &other, &ruff, vec![ruff_finding(&other, 0)], &old2, 3));
        assert_eq!(s.analysis.editor.of(&other)[0].finding.start.line, 1);
        // Positions the edit touched are dropped, not guessed.
        let mut touched = ruff_finding(&other, 0);
        touched.start = codecraft_text::Position::new(0, 0);
        touched.end = codecraft_text::Position::new(0, 2);
        let s2 = s.ws.doc(id2).map(|d| (d.doc.revision(), d.doc.snapshot())).unwrap();
        let edit2 = codecraft_text::Edit { changes: codecraft_text::ChangeSet::single(1, 1, "Z"), selections: sel.clone() };
        s.ws.doc_mut(id2).unwrap().doc.apply(&edit2, &sel, codecraft_text::EditGroup::Standalone).unwrap();
        assert!(super::super::editor::take_live(&mut s, &other, &ruff, vec![touched], &s2, 4));
        assert!(s.analysis.editor.of(&other).is_empty(), "the edit was inside the finding");
    }

    #[test]
    fn closed_and_hostile_states_never_panic() {
        let mut s = Session::new();
        // No project: nothing is tracked; status of an unknown file.
        let path = PathBuf::from("/nowhere/x.py");
        s.ws.add_document(Some(path.clone()), "import os\n");
        assert!(!pump(&mut s));
        assert!(!s.analysis.editor.live.busy());
        let v = status_json(&s, &path);
        assert_eq!(v["language"], "python");
        assert_eq!(v["computedFor"], Value::Null);
        assert_eq!(status_json(&s, Path::new("/x/y.sql"))["language"], "sql");
        assert_eq!(status_json(&s, Path::new(""))["language"], Value::Null);
        assert!(run_now(&mut s, Path::new("/not/open.py")).is_empty());
        assert_eq!(run_now(&mut s, &path), vec![Tool::Ruff]);
        assert!(s.analysis.editor.live.pending(&path));
        shutdown(&mut s);
        assert!(!s.analysis.editor.live.busy());
        // Power Save cancels and pauses.
        s.ws.settings.power_save_mode = true;
        assert!(!covers(&s, &path, Tool::Ruff));
        assert!(!pump(&mut s));
    }

    #[cfg(unix)]
    #[test]
    fn stdin_processes_run_and_cancel() {
        let cancel = AtomicBool::new(false);
        let out = run_stdin(Path::new("/bin/cat"), &[], Path::new("/"), "hello\nworld\n", &cancel).unwrap();
        assert_eq!(out.stdout, vec!["hello", "world"]);
        assert!(out.success);
        // A big input does not deadlock (the program writes while we write).
        let big = "x".repeat(4 << 20);
        let out = run_stdin(Path::new("/bin/cat"), &[], Path::new("/"), &big, &cancel).unwrap();
        assert_eq!(out.stdout.concat().len(), big.len());
        // Cancelled: killed at once.
        let cancel = AtomicBool::new(true);
        let t = Instant::now();
        let err = run_stdin(Path::new("/bin/sleep"), &["5".into()], Path::new("/"), "", &cancel).err().unwrap();
        assert_eq!(err, CANCELLED);
        assert!(t.elapsed() < Duration::from_secs(3));
        // A missing program is an error.
        assert!(run_stdin(Path::new("/no/such/program"), &[], Path::new("/"), "", &AtomicBool::new(false)).is_err());
    }
}
