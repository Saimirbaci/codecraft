//! Agent sessions saved per project (M12 track AG).
//!
//! Every agent job (the Agents tool window, the feedback overlay, Generate run configurations)
//! is saved as `$CODECRAFT_HOME/agent-sessions/<project key>/<session>.json` (default
//! `~/.codecraft`; the project key is the directory's name plus a hash of its canonical path).
//! Never inside the project, so nothing ends up in git. A file holds the prompt and follow-ups,
//! the activity feed (capped: [`agents::MAX_ACTIVITY`] entries and [`MAX_FILE_BYTES`]), status,
//! backend, mode, title, cost and duration, timestamps, the backend's own session id, and the
//! review worktree with the decisions made so far (the diffs themselves are collected again from
//! the worktree when the session is reopened).
//!
//! Saving: when a job starts, on every status change and at most every [`SAVE_EVERY`] while it
//! changes, as atomic writes on a writer thread (the JSON is built on the UI thread, writes never
//! block it), and a last time when the window closes, so a crash loses little. Jobs still running
//! at that point are saved as **interrupted**.
//!
//! Loading: when a project opens its sessions are read in the background into
//! [`SessionsState::saved`] (newest first; corrupt or oversized files are skipped with a
//! warning). `agent.sessions.open` turns one back into a job; a follow-up resumes it: Claude Code
//! `--resume <id>` (Codex `exec resume`, Gemini/Grok `--resume`) in the same directory, ACP
//! agents `session/load` when they offer it, otherwise a new session whose first prompt carries a
//! transcript of the old one. A review whose worktree is gone recreates it at the same path.
//!
//! Pruning keeps at most [`MAX_SESSIONS`] sessions and [`MAX_PROJECT_BYTES`] per project, oldest
//! first, never one that is open as a job.
//!
//! Saving is on when the app saves its sessions (`session_store.autosave`), when
//! [`SessionsState::enabled`] is set, or with a directory override ([`Session::isolate_home`]);
//! the headless CLI and tests that do none of these never write.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use codecraft_agent::git::{self, Worktree};
use codecraft_agent::{Backend, truncate};
use codecraft_workspace::{NotificationLevel, ToolWindowId};

use crate::agents::{self, Activity, Job, JobId, JobStatus, ReviewDecision, ToolState, mode_id, parse_mode};
use crate::approvals::AgentMode;
use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::{bool_or, opt_str, opt_usize, req_str};
use crate::{CommandError, Effect, Session};

/// The directory under `$CODECRAFT_HOME`.
pub const DIR_NAME: &str = "agent-sessions";
/// Version written into each file.
pub const FORMAT_VERSION: u32 = 1;
/// Sessions kept per project (oldest pruned first).
pub const MAX_SESSIONS: usize = 200;
/// Bytes of session files kept per project.
pub const MAX_PROJECT_BYTES: u64 = 256 << 20;
/// A session file is cut to this size (oldest feed entries first).
pub const MAX_FILE_BYTES: usize = 2 << 20;
/// Larger files are skipped when reading.
pub const MAX_READ_BYTES: u64 = 16 << 20;
/// Minimum time between saves of a changing job (status changes are saved at once).
pub const SAVE_EVERY: Duration = Duration::from_secs(1);
/// Longest transcript put before a prompt when a session cannot be resumed directly.
pub const MAX_TRANSCRIPT_CHARS: usize = 12_000;
const MAX_FILES_SCANNED: usize = 5_000;
const MAX_WARNINGS: usize = 20;
const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);
const ERROR_NOTE_EVERY: Duration = Duration::from_secs(60);
/// A saved session updated this recently may belong to a job running in another window.
const RECENT_MS: u64 = 60_000;
const INTERRUPTED_NOTE: &str = "Interrupted:";

// ------------------------------------------------------------------------------ the file

/// One review decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDecision {
    pub rel: String,
    pub decision: ReviewDecision,
}

/// A session file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSession {
    #[serde(default)]
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub backend: Backend,
    /// `auto`, `ask` or `review`.
    #[serde(default)]
    pub mode: String,
    pub status: JobStatus,
    #[serde(default)]
    pub project: PathBuf,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub attachments: Vec<PathBuf>,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// The agent's own session/thread id (`--resume`, `session/load`).
    #[serde(default)]
    pub backend_session_id: Option<String>,
    #[serde(default)]
    pub worktree: Option<Worktree>,
    /// Where the worktree is or was (kept after it is removed, to recreate it there).
    #[serde(default)]
    pub worktree_path: Option<PathBuf>,
    #[serde(default)]
    pub changed_files: Vec<PathBuf>,
    #[serde(default)]
    pub review: Vec<SavedDecision>,
    #[serde(default)]
    pub error: Option<String>,
    /// Feed entries left out to fit the size cap.
    #[serde(default)]
    pub dropped: usize,
    #[serde(default, deserialize_with = "lenient_activity")]
    pub activity: Vec<Activity>,
}

/// Feed entries this version does not know are skipped rather than failing the file.
fn lenient_activity<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Activity>, D::Error> {
    let raw = Option::<Vec<Value>>::deserialize(d)?.unwrap_or_default();
    Ok(raw.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect())
}

/// What a listing needs (the feed is skipped while parsing).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    #[serde(default)]
    title: String,
    backend: Backend,
    #[serde(default)]
    mode: String,
    status: JobStatus,
    #[serde(default)]
    created_ms: u64,
    #[serde(default)]
    updated_ms: u64,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    backend_session_id: Option<String>,
    #[serde(default)]
    worktree: Option<Worktree>,
}

/// One saved session in a listing.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub backend: Backend,
    pub mode: AgentMode,
    /// Running jobs read from disk are [`JobStatus::Interrupted`].
    pub status: JobStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub backend_session_id: Option<String>,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    pub worktree_exists: bool,
    pub bytes: u64,
    pub file: PathBuf,
}

impl SessionSummary {
    fn to_json(&self, job: Option<JobId>) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "backend": self.backend.id(),
            "mode": mode_id(self.mode),
            "status": self.status.label(),
            "created": self.created_ms,
            "updated": self.updated_ms,
            "costUsd": self.cost_usd,
            "durationMs": self.duration_ms,
            "backendSessionId": self.backend_session_id,
            "worktree": self.worktree,
            "branch": self.branch,
            "worktreeExists": self.worktree_exists,
            "job": job.map(|j| j.0),
            "file": self.file,
            "bytes": self.bytes,
        })
    }
}

// ------------------------------------------------------------------------------ state

/// Save bookkeeping of one job.
#[derive(Clone, Debug, Default)]
pub struct Persist {
    fingerprint: Option<u64>,
    status: Option<JobStatus>,
    at: Option<Instant>,
}

enum WriteMsg {
    Write(PathBuf, String),
    Remove(PathBuf),
    Flush(Sender<()>),
}

struct Writer {
    tx: Sender<WriteMsg>,
    errors: Receiver<String>,
}

struct Scan {
    project: PathBuf,
    sessions: Vec<SessionSummary>,
    warnings: Vec<String>,
}

/// Saved agent sessions of the open project, and the switches for saving them.
pub struct SessionsState {
    /// Save sessions even when the app does not save its own sessions (CLI, embedders).
    pub enabled: bool,
    /// Instead of `$CODECRAFT_HOME/agent-sessions` (tests and embedders; turns saving on).
    pub dir_override: Option<PathBuf>,
    pub max_sessions: usize,
    pub max_project_bytes: u64,
    /// Sessions of [`Self::project`] on disk, newest first (live jobs may also be listed here).
    pub saved: Vec<SessionSummary>,
    /// The listing is being read.
    pub loading: bool,
    /// Files that were skipped, write errors.
    pub warnings: Vec<String>,
    project: Option<PathBuf>,
    project_key: Option<String>,
    scan: Option<Receiver<Scan>>,
    writer: Option<Writer>,
    prune_due: bool,
    last_error_note: Option<Instant>,
}

impl Default for SessionsState {
    fn default() -> Self {
        Self {
            enabled: false,
            dir_override: None,
            max_sessions: MAX_SESSIONS,
            max_project_bytes: MAX_PROJECT_BYTES,
            saved: Vec::new(),
            loading: false,
            warnings: Vec::new(),
            project: None,
            project_key: None,
            scan: None,
            writer: None,
            prune_due: false,
            last_error_note: None,
        }
    }
}

impl SessionsState {
    /// `$CODECRAFT_HOME/agent-sessions` (default `~/.codecraft/agent-sessions`).
    pub fn dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.dir_override {
            return Some(d.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home).join(DIR_NAME));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".codecraft").join(DIR_NAME))
    }

    /// Where the sessions of `project` are saved.
    pub fn project_dir(&self, project: &Path) -> Option<PathBuf> {
        Some(self.dir()?.join(project_key(project)))
    }

    /// The project whose sessions are listed.
    pub fn project(&self) -> Option<&Path> {
        self.project.as_deref()
    }

    /// Wait until every queued write is on disk (at most `timeout`). False on timeout.
    pub fn flush(&mut self, timeout: Duration) -> bool {
        let Some(w) = &self.writer else { return true };
        let (tx, rx) = bounded(1);
        if w.tx.send(WriteMsg::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    fn send(&mut self, msg: WriteMsg) {
        if self.writer.is_none() {
            self.writer = spawn_writer();
        }
        let failed = match &self.writer {
            Some(w) => w.tx.send(msg).is_err(),
            None => true,
        };
        if failed {
            self.writer = None;
            self.warn("cannot start the agent session writer".into());
        }
    }

    fn warn(&mut self, w: String) {
        log::warn!("agent sessions: {w}");
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(w);
        }
    }

    fn upsert(&mut self, summary: SessionSummary) {
        match self.saved.iter_mut().find(|s| s.id == summary.id) {
            Some(s) => *s = summary,
            None => {
                self.saved.push(summary);
                self.prune_due = true;
            }
        }
        sort(&mut self.saved);
    }

    fn lists(&self, project: &Path) -> bool {
        self.project_key.as_deref().is_some_and(|k| k == project_key(project))
    }
}

fn sort(list: &mut [SessionSummary]) {
    list.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.id.cmp(&a.id)));
}

/// Whether sessions are saved and listed.
pub fn active(s: &Session) -> bool {
    let st = &s.agents.sessions;
    st.enabled || st.dir_override.is_some() || s.session_store.autosave
}

/// The directory name for a project: its name plus a hash of its canonical path.
pub fn project_key(project: &Path) -> String {
    let normal = crate::normalize_path(project);
    let canonical = std::fs::canonicalize(&normal).unwrap_or(normal);
    git::project_hash(&canonical)
}

/// A new session id: creation time, process and job (unique across windows and restarts).
pub(crate) fn new_key(created_ms: u64, job: JobId) -> String {
    format!("{created_ms}-{:x}-{}", std::process::id(), job.0)
}

/// A session id is one safe file-name component.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 96
        && !key.starts_with(['.', '-'])
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// "just now", "5 min ago", "3 h ago", "2 days ago", else the UTC date.
pub fn age_label(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..604_800 => {
            let d = secs / 86_400;
            if d == 1 { "yesterday".into() } else { format!("{d} days ago") }
        }
        _ => utc_date(then_ms),
    }
}

/// `YYYY-MM-DD` (UTC) of a Unix time in milliseconds.
pub fn utc_date(ms: u64) -> String {
    let days = i64::try_from(ms / 86_400_000).unwrap_or(0);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ------------------------------------------------------------------------------ writer

fn spawn_writer() -> Option<Writer> {
    let (tx, rx) = unbounded::<WriteMsg>();
    let (etx, errors) = unbounded::<String>();
    std::thread::Builder::new().name("codecraft-agent-sessions".into()).spawn(move || writer_loop(&rx, &etx)).ok()?;
    Some(Writer { tx, errors })
}

fn writer_loop(rx: &Receiver<WriteMsg>, errors: &Sender<String>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        // Only the newest write of a file in a batch matters.
        let last: HashMap<PathBuf, usize> = batch
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match m {
                WriteMsg::Write(p, _) => Some((p.clone(), i)),
                _ => None,
            })
            .collect();
        for (i, msg) in batch.into_iter().enumerate() {
            match msg {
                WriteMsg::Write(path, text) => {
                    if last.get(&path) == Some(&i)
                        && let Err(e) = write_file(&path, &text)
                    {
                        let _ = errors.send(e);
                    }
                }
                WriteMsg::Remove(path) => match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        let _ = errors.send(format!("cannot delete {}: {e}", path.display()));
                    }
                },
                WriteMsg::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
        }
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    codecraft_fs::write_text_file_atomic(path, text).map_err(|e| format!("cannot save {}: {e}", path.display()))
}

// ------------------------------------------------------------------------------ saving

fn tool_mark(s: ToolState) -> u8 {
    match s {
        ToolState::Running => 0,
        ToolState::Ok => 1,
        ToolState::Failed => 2,
    }
}

fn decision_mark(d: ReviewDecision) -> u8 {
    match d {
        ReviewDecision::Pending => 0,
        ReviewDecision::Accepted => 1,
        ReviewDecision::Rejected => 2,
    }
}

/// A cheap summary of a feed entry (kind and size) for change detection.
fn activity_mark(a: &Activity) -> (u8, usize) {
    match a {
        Activity::Prompt { text } => (0, text.len()),
        Activity::Text { text } => (1, text.len()),
        Activity::Tool { state, output, .. } => (2 + tool_mark(*state), output.len()),
        Activity::FileChanged { .. } => (5, 0),
        Activity::Plan { entries } => {
            (6, entries.len() * 1000 + entries.iter().filter(|e| e.status == "completed").count())
        }
        Activity::Result { .. } => (7, 0),
        Activity::Error { message } => (8, message.len()),
        Activity::Log { line } => (9, line.len()),
        Activity::Info { text } => (10, text.len()),
    }
}

/// Changes whenever anything saved about the job changes.
fn fingerprint(j: &Job) -> u64 {
    let mut h = DefaultHasher::new();
    j.status.label().hash(&mut h);
    j.title.hash(&mut h);
    j.session_id.hash(&mut h);
    j.cost_usd.map(f64::to_bits).hash(&mut h);
    j.duration_ms.hash(&mut h);
    j.error.hash(&mut h);
    j.worktree.as_ref().map(|w| &w.path).hash(&mut h);
    j.changed_files.len().hash(&mut h);
    j.activity.len().hash(&mut h);
    for a in &j.activity {
        activity_mark(a).hash(&mut h);
    }
    for f in &j.review {
        decision_mark(f.decision).hash(&mut h);
    }
    h.finish()
}

fn to_saved(job: &Job, key: &str) -> SavedSession {
    let mut review: Vec<SavedDecision> = job
        .review
        .iter()
        .filter(|f| f.decision != ReviewDecision::Pending)
        .map(|f| SavedDecision { rel: f.rel.clone(), decision: f.decision })
        .collect();
    if job.review.is_empty() {
        // A reopened review whose changes are still being collected.
        review = job.restored_decisions.iter().map(|(rel, d)| SavedDecision { rel: rel.clone(), decision: *d }).collect();
    }
    SavedSession {
        version: FORMAT_VERSION,
        id: key.to_string(),
        title: job.title.clone(),
        backend: job.backend,
        mode: mode_id(job.mode).to_string(),
        status: job.status,
        project: job.project.clone(),
        cwd: Some(job.cwd.clone()),
        prompt: job.prompt.clone(),
        attachments: job.attachments.clone(),
        created_ms: job.created_ms,
        updated_ms: now_ms(),
        cost_usd: job.cost_usd,
        duration_ms: job.duration_ms,
        backend_session_id: job.session_id.clone(),
        worktree: job.worktree.clone(),
        worktree_path: job.worktree_path.clone(),
        changed_files: job.changed_files.iter().take(git::MAX_REVIEW_FILES).cloned().collect(),
        review,
        error: job.error.clone(),
        dropped: 0,
        activity: job.activity.iter().cloned().collect(),
    }
}

/// The file's JSON, cut to [`MAX_FILE_BYTES`]: long outputs first, then the oldest entries.
pub fn encode(saved: &mut SavedSession) -> Result<String, String> {
    let ser = |s: &SavedSession| serde_json::to_string_pretty(s).map_err(|e| format!("cannot serialize: {e}"));
    let text = ser(saved)?;
    if text.len() <= MAX_FILE_BYTES {
        return Ok(text);
    }
    saved.prompt = truncate(&saved.prompt, 50_000);
    for a in &mut saved.activity {
        match a {
            Activity::Tool { output, .. } => *output = truncate(output, 400),
            Activity::Log { line } => *line = truncate(line, 400),
            Activity::Text { text } | Activity::Prompt { text } => *text = truncate(text, 20_000),
            _ => {}
        }
    }
    loop {
        let text = ser(saved)?;
        if text.len() <= MAX_FILE_BYTES || saved.activity.is_empty() {
            return Ok(text);
        }
        let cut = (saved.activity.len() / 4).max(1);
        saved.activity.drain(..cut);
        saved.dropped = saved.dropped.saturating_add(cut);
    }
}

fn summary_of(saved: &SavedSession, file: PathBuf, bytes: u64) -> SessionSummary {
    let status = match saved.status {
        JobStatus::Running | JobStatus::Preparing => JobStatus::Interrupted,
        s => s,
    };
    SessionSummary {
        id: saved.id.clone(),
        title: saved.title.clone(),
        backend: saved.backend,
        mode: parse_mode(&saved.mode).unwrap_or_default(),
        status,
        created_ms: saved.created_ms,
        updated_ms: saved.updated_ms,
        cost_usd: saved.cost_usd,
        duration_ms: saved.duration_ms,
        backend_session_id: saved.backend_session_id.clone(),
        worktree: saved.worktree.as_ref().map(|w| w.path.clone()),
        branch: saved.worktree.as_ref().map(|w| w.branch.clone()),
        worktree_exists: saved.worktree.is_some(),
        bytes,
        file,
    }
}

/// Queue a save of `job`. `interrupted`: a running job is saved as interrupted (closing).
fn save_job(store: &mut SessionsState, job: &mut Job, interrupted: bool) {
    let Some(key) = job.session_key.clone().filter(|k| valid_key(k)) else { return };
    let Some(dir) = store.project_dir(&job.project) else { return };
    let mut saved = to_saved(job, &key);
    if interrupted && job.is_running() {
        saved.status = JobStatus::Interrupted;
    }
    job.persist = Persist { fingerprint: Some(fingerprint(job)), status: Some(job.status), at: Some(Instant::now()) };
    let text = match encode(&mut saved) {
        Ok(t) => t,
        Err(e) => return store.warn(format!("session {key}: {e}")),
    };
    let file = dir.join(format!("{key}.json"));
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    store.send(WriteMsg::Write(file.clone(), text));
    if store.lists(&job.project) {
        let mut summary = summary_of(&saved, file, bytes);
        // Listed while live: keep the live status.
        summary.status = job.status;
        store.upsert(summary);
    }
}

fn due(job: &Job, now: Instant) -> bool {
    if job.session_key.is_none() || job.persist.fingerprint == Some(fingerprint(job)) {
        return false;
    }
    job.persist.status != Some(job.status) || job.persist.at.is_none_or(|t| now.duration_since(t) >= SAVE_EVERY)
}

/// Save a job that is no longer in the job list (discarded) now.
pub(crate) fn save_detached(s: &mut Session, job: &mut Job) {
    if active(s) {
        save_job(&mut s.agents.sessions, job, false);
    }
}

/// Window closing: save every job (running ones as interrupted) and wait for the writes.
pub(crate) fn shutdown(s: &mut Session) {
    if !active(s) {
        return;
    }
    let agents = &mut s.agents;
    for job in agents.jobs.iter_mut() {
        if job.is_running() || job.persist.fingerprint != Some(fingerprint(job)) {
            save_job(&mut agents.sessions, job, true);
        }
    }
    if !agents.sessions.flush(FLUSH_TIMEOUT) {
        log::warn!("agent sessions: the last saves did not finish in time");
    }
}

/// Apply listings, report write errors, follow the open project and save changed jobs.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = drain_scan(s);
    changed |= drain_errors(s);
    if !active(s) {
        return changed;
    }
    let root = s.project_root();
    if root != s.agents.sessions.project {
        start_scan(&mut s.agents.sessions, root);
        changed = true;
    }
    let now = Instant::now();
    let agents = &mut s.agents;
    for job in agents.jobs.iter_mut() {
        if due(job, now) {
            save_job(&mut agents.sessions, job, false);
        }
    }
    if agents.sessions.prune_due && !agents.sessions.loading {
        let live: HashSet<String> = agents.jobs.iter().filter_map(|j| j.session_key.clone()).collect();
        prune(&mut agents.sessions, &live, now_ms());
    }
    changed
}

fn drain_errors(s: &mut Session) -> bool {
    let errors: Vec<String> = match &s.agents.sessions.writer {
        Some(w) => w.errors.try_iter().take(32).collect(),
        None => return false,
    };
    if errors.is_empty() {
        return false;
    }
    for e in &errors {
        s.agents.sessions.warn(e.clone());
    }
    let st = &mut s.agents.sessions;
    if st.last_error_note.is_none_or(|t| t.elapsed() >= ERROR_NOTE_EVERY) {
        st.last_error_note = Some(Instant::now());
        let msg = errors.join("\n");
        s.notify(NotificationLevel::Warning, "Cannot save an agent session", msg);
    }
    true
}

// ------------------------------------------------------------------------------ listing

fn start_scan(store: &mut SessionsState, root: Option<PathBuf>) {
    store.project_key = root.as_deref().map(project_key);
    store.project = root.clone();
    store.saved.clear();
    store.warnings.clear();
    store.scan = None;
    store.loading = false;
    store.prune_due = false;
    let (Some(root), Some(dir)) = (root.clone(), root.as_deref().and_then(|r| store.project_dir(r))) else { return };
    let (tx, rx) = bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-agent-sessions-scan".into()).spawn(move || {
        let (sessions, warnings) = scan_dir(&dir);
        let _ = tx.send(Scan { project: root, sessions, warnings });
    });
    match spawned {
        Ok(_) => {
            store.scan = Some(rx);
            store.loading = true;
        }
        Err(e) => store.warn(format!("cannot list agent sessions: {e}")),
    }
}

/// Re-read the open project's sessions (`agent.sessions.reload`).
pub fn reload(s: &mut Session) {
    let root = s.project_root();
    start_scan(&mut s.agents.sessions, root);
}

fn drain_scan(s: &mut Session) -> bool {
    let store = &mut s.agents.sessions;
    let Some(rx) = &store.scan else { return false };
    let scan = match rx.try_recv() {
        Ok(scan) => scan,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => {
            store.scan = None;
            store.loading = false;
            return true;
        }
    };
    store.scan = None;
    store.loading = false;
    if store.project.as_ref() != Some(&scan.project) {
        return true;
    }
    for w in scan.warnings {
        store.warn(w);
    }
    // Saves made while the listing was read are newer than what it found.
    for summary in scan.sessions {
        if !store.saved.iter().any(|s| s.id == summary.id) {
            store.saved.push(summary);
        }
    }
    sort(&mut store.saved);
    store.prune_due = true;
    true
}

/// Read every session summary in a project's directory (missing directory = none).
fn scan_dir(dir: &Path) -> (Vec<SessionSummary>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return (out, warnings) };
    for entry in rd.filter_map(Result::ok).take(MAX_FILES_SCANNED) {
        let path = entry.path();
        let Some(id) = session_id_of(&path) else { continue };
        match read_summary(&path, &id) {
            Ok(s) => out.push(s),
            Err(e) => warnings.push(format!("skipped {}: {e}", path.display())),
        }
    }
    sort(&mut out);
    (out, warnings)
}

/// `<id>.json` with a valid id (temp files of atomic writes start with a dot).
fn session_id_of(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    valid_key(stem).then(|| stem.to_string())
}

fn read_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(format!("too large ({} MB)", meta.len() >> 20));
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

fn read_summary(path: &Path, id: &str) -> Result<SessionSummary, String> {
    let text = read_text(path)?;
    let h: Header = serde_json::from_str(&text).map_err(|e| format!("not a session file ({e})"))?;
    let status = match h.status {
        JobStatus::Running | JobStatus::Preparing => JobStatus::Interrupted,
        s => s,
    };
    Ok(SessionSummary {
        id: id.to_string(),
        title: h.title,
        backend: h.backend,
        mode: parse_mode(&h.mode).unwrap_or_default(),
        status,
        created_ms: h.created_ms,
        updated_ms: h.updated_ms,
        cost_usd: h.cost_usd,
        duration_ms: h.duration_ms,
        backend_session_id: h.backend_session_id,
        worktree_exists: h.worktree.as_ref().is_some_and(|w| w.path.is_dir()),
        worktree: h.worktree.as_ref().map(|w| w.path.clone()),
        branch: h.worktree.map(|w| w.branch),
        bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
        file: path.to_path_buf(),
    })
}

/// Read a whole session file.
pub fn read_session(path: &Path) -> Result<SavedSession, String> {
    let text = read_text(path)?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not a session file ({e})", path.display()))
}

/// Delete the oldest sessions beyond the limits, never one open as a job or one that may be
/// running in another window. Returns how many were deleted.
fn prune(store: &mut SessionsState, live: &HashSet<String>, now: u64) -> usize {
    store.prune_due = false;
    let mut count = store.saved.len();
    let mut bytes: u64 = store.saved.iter().map(|s| s.bytes).sum();
    if count <= store.max_sessions && bytes <= store.max_project_bytes {
        return 0;
    }
    let mut victims: Vec<String> = Vec::new();
    // `saved` is newest first.
    for s in store.saved.iter().rev() {
        if count <= store.max_sessions && bytes <= store.max_project_bytes {
            break;
        }
        let maybe_running = s.status == JobStatus::Interrupted && now.saturating_sub(s.updated_ms) < RECENT_MS;
        if live.contains(&s.id) || maybe_running {
            continue;
        }
        victims.push(s.id.clone());
        count -= 1;
        bytes = bytes.saturating_sub(s.bytes);
    }
    let files: Vec<PathBuf> =
        store.saved.iter().filter(|s| victims.contains(&s.id)).map(|s| s.file.clone()).collect();
    for f in files {
        store.send(WriteMsg::Remove(f));
    }
    store.saved.retain(|s| !victims.contains(&s.id));
    if !victims.is_empty() {
        log::info!("agent sessions: pruned {} old session(s)", victims.len());
    }
    victims.len()
}

/// Saved sessions of the open project that are not open as jobs (newest first).
pub fn history(s: &Session) -> Vec<&SessionSummary> {
    let live: HashSet<&str> = s.agents.jobs.iter().filter_map(|j| j.session_key.as_deref()).collect();
    s.agents.sessions.saved.iter().filter(|x| !live.contains(x.id.as_str())).collect()
}

fn live_job(s: &Session, key: &str) -> Option<JobId> {
    s.agents.jobs.iter().find(|j| j.session_key.as_deref() == Some(key)).map(|j| j.id)
}

/// The file of a session: in the listed project's directory, else in any project's.
fn find_file(s: &Session, key: &str) -> Option<PathBuf> {
    if !valid_key(key) {
        return None;
    }
    let name = format!("{key}.json");
    if let Some(f) = s.agents.sessions.saved.iter().find(|x| x.id == key).map(|x| x.file.clone()) {
        return Some(f);
    }
    if let Some(job) = s.agents.jobs.iter().find(|j| j.session_key.as_deref() == Some(key))
        && let Some(f) = s.agents.sessions.project_dir(&job.project).map(|d| d.join(&name))
    {
        return Some(f);
    }
    let root = s.agents.sessions.dir()?;
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .take(MAX_FILES_SCANNED)
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

// ------------------------------------------------------------------------------ restoring

/// Reopen a saved session as a job (or return the job that already has it).
pub fn open(s: &mut Session, key: &str) -> Result<JobId, CommandError> {
    if !valid_key(key) {
        return Err(CommandError::BadParams(format!("`{key}` is not a session id")));
    }
    if let Some(id) = live_job(s, key) {
        return Ok(id);
    }
    let file = find_file(s, key).ok_or_else(|| CommandError::Failed(format!("no saved agent session `{key}`")))?;
    let saved = read_session(&file).map_err(CommandError::Failed)?;
    Ok(restore(s, saved, key))
}

fn restore(s: &mut Session, saved: SavedSession, key: &str) -> JobId {
    let mode = parse_mode(&saved.mode).unwrap_or_default();
    let project = if saved.project.as_os_str().is_empty() {
        s.project_root().unwrap_or_default()
    } else {
        saved.project.clone()
    };
    let mut job = agents::new_job(s, saved.backend, mode, saved.title.clone(), saved.prompt.clone(), project.clone());
    job.session_key = Some(key.to_string());
    if saved.created_ms > 0 {
        job.created_ms = saved.created_ms;
    }
    job.attachments = saved.attachments;
    job.cost_usd = saved.cost_usd;
    job.duration_ms = saved.duration_ms;
    job.session_id = saved.backend_session_id;
    job.error = saved.error;
    job.changed_files = saved.changed_files;
    job.worktree_path = saved.worktree_path.or_else(|| saved.worktree.as_ref().map(|w| w.path.clone()));
    let skip = saved.activity.len().saturating_sub(agents::MAX_ACTIVITY);
    job.activity = saved.activity.into_iter().skip(skip).collect::<VecDeque<_>>();
    let dropped = saved.dropped.saturating_add(skip);
    if dropped > 0 {
        job.activity.push_front(Activity::Info { text: format!("{dropped} earlier entries were not saved.") });
    }
    let mut status = saved.status;
    if matches!(status, JobStatus::Running | JobStatus::Preparing) {
        status = JobStatus::Interrupted;
    }
    let noted = matches!(job.activity.back(), Some(Activity::Info { text }) if text.starts_with(INTERRUPTED_NOTE));
    if status == JobStatus::Interrupted && !noted {
        job.activity.push_back(Activity::Info {
            text: format!("{INTERRUPTED_NOTE} CodeCraft closed while the agent was working. Send a follow-up to resume."),
        });
    }
    match saved.worktree {
        Some(wt) if wt.path.is_dir() => {
            job.cwd = saved.cwd.filter(|c| c.starts_with(&wt.path)).unwrap_or_else(|| wt.cwd());
            job.worktree = Some(wt);
        }
        Some(wt) => {
            job.activity.push_back(Activity::Error {
                message: format!(
                    "The review worktree {} no longer exists; its changes can no longer be reviewed.",
                    wt.path.display()
                ),
            });
            if status == JobStatus::Reviewing {
                status = JobStatus::Done;
            }
        }
        None => {}
    }
    job.status = status;
    job.restored_decisions = saved.review.into_iter().map(|d| (d.rel, d.decision)).collect();
    if status == saved.status {
        // Unchanged since it was saved: no need to write it again.
        job.persist = Persist { fingerprint: Some(fingerprint(&job)), status: Some(status), at: Some(Instant::now()) };
    }
    let collect = status == JobStatus::Reviewing && job.worktree.is_some();
    let id = agents::insert_restored(s, job, collect);
    s.ws.layout.show(ToolWindowId::Agent);
    id
}

/// The transcript put before a prompt when a session cannot be resumed directly (empty when
/// there is nothing to tell).
pub fn transcript_preamble(activity: &VecDeque<Activity>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    for a in activity.iter().rev() {
        let line = match a {
            Activity::Prompt { text } => format!("User: {}", truncate(text.trim(), 2_000)),
            Activity::Text { text } => format!("Assistant: {}", truncate(text.trim(), 2_000)),
            Activity::Tool { tool, summary, state, .. } => {
                let how = match state {
                    ToolState::Ok => "done",
                    ToolState::Failed => "failed",
                    ToolState::Running => "interrupted",
                };
                format!("Tool {tool}: {summary} ({how})")
            }
            Activity::FileChanged { path, change } => format!("Changed {} ({change:?})", path.display()),
            Activity::Result { is_error: true, text, .. } => {
                format!("The turn ended with an error{}", text.as_deref().map(|t| format!(": {t}")).unwrap_or_default())
            }
            Activity::Error { message } => format!("Error: {}", truncate(message, 500)),
            _ => continue,
        };
        let n = line.chars().count() + 1;
        if used + n > MAX_TRANSCRIPT_CHARS {
            lines.push("[… earlier messages omitted]".into());
            break;
        }
        used += n;
        lines.push(line);
    }
    if lines.is_empty() {
        return String::new();
    }
    lines.reverse();
    format!(
        "This continues an earlier conversation that could not be resumed directly. Its transcript, \
         oldest first:\n<previous-session>\n{}\n</previous-session>\n\nThe new request:\n",
        lines.join("\n")
    )
}

// ------------------------------------------------------------------------------ managing

fn rename(s: &mut Session, key: &str, title: &str) -> Result<(), CommandError> {
    let title = truncate(title.trim(), 200);
    if title.is_empty() {
        return Err(CommandError::BadParams("the title is empty".into()));
    }
    if let Some(id) = live_job(s, key) {
        if let Some(j) = s.agents.jobs.iter_mut().find(|j| j.id == id) {
            j.title = title;
        }
        return Ok(());
    }
    let file = find_file(s, key).ok_or_else(|| CommandError::Failed(format!("no saved agent session `{key}`")))?;
    let mut saved = read_session(&file).map_err(CommandError::Failed)?;
    saved.title = title.clone();
    let text = encode(&mut saved).map_err(CommandError::Failed)?;
    let st = &mut s.agents.sessions;
    st.send(WriteMsg::Write(file, text));
    if let Some(x) = st.saved.iter_mut().find(|x| x.id == key) {
        x.title = title;
    }
    Ok(())
}

/// Delete a session (and its job, unless running, and its worktree).
fn delete(s: &mut Session, key: &str) -> Result<(), CommandError> {
    if !valid_key(key) {
        return Err(CommandError::BadParams(format!("`{key}` is not a session id")));
    }
    if let Some(id) = live_job(s, key) {
        if s.agents.job(id).is_some_and(Job::is_running) {
            return Err(CommandError::Failed("the agent is still working; stop it first".into()));
        }
        agents::discard(s, id)?;
    }
    let file = find_file(s, key);
    let saved = file.as_deref().and_then(|f| read_session(f).ok());
    if let Some(wt) = saved.and_then(|x| x.worktree).filter(|w| w.path.is_dir()) {
        agents::remove_worktree_later(s, wt);
    }
    let st = &mut s.agents.sessions;
    if let Some(f) = file {
        st.send(WriteMsg::Remove(f));
    }
    st.saved.retain(|x| x.id != key);
    Ok(())
}

/// Delete every session of the open project except running jobs. Returns (deleted, kept).
fn clear(s: &mut Session) -> Result<(usize, usize), CommandError> {
    let mut keys: Vec<String> = s.agents.sessions.saved.iter().map(|x| x.id.clone()).collect();
    if let Some(root) = s.agents.sessions.project.clone() {
        let key = project_key(&root);
        for j in &s.agents.jobs {
            if let Some(k) = &j.session_key
                && !keys.contains(k)
                && project_key(&j.project) == key
            {
                keys.push(k.clone());
            }
        }
    }
    let (mut deleted, mut kept) = (0, 0);
    for k in keys {
        match delete(s, &k) {
            Ok(()) => deleted += 1,
            Err(_) => kept += 1,
        }
    }
    Ok((deleted, kept))
}

/// `file://` URL of a path (percent-encoded).
fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ------------------------------------------------------------------------------ commands

/// The session param: `session` (id) or `job` (an open job's session).
fn key_param(s: &Session, p: &Value) -> Result<String, CommandError> {
    if let Some(k) = opt_str(p, "session") {
        return Ok(k.to_string());
    }
    if let Some(n) = opt_usize(p, "job")? {
        let id = JobId(u64::try_from(n).unwrap_or(u64::MAX));
        return s
            .agents
            .job(id)
            .and_then(|j| j.session_key.clone())
            .ok_or_else(|| CommandError::BadParams(format!("agent job {n} has no saved session")));
    }
    Err(CommandError::BadParams("missing string param `session`".into()))
}

fn list(s: &mut Session, _: &Value) -> CmdResult {
    let st = &s.agents.sessions;
    let key = st.project_key.clone();
    let mut rows: Vec<(u64, String, Value)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for j in &s.agents.jobs {
        let Some(k) = &j.session_key else { continue };
        if key.as_deref() != Some(project_key(&j.project).as_str()) {
            continue;
        }
        seen.insert(k.clone());
        let saved = st.saved.iter().find(|x| &x.id == k);
        rows.push((
            j.created_ms,
            k.clone(),
            json!({
                "id": k,
                "title": j.title,
                "backend": j.backend.id(),
                "mode": mode_id(j.mode),
                "status": j.status.label(),
                "created": j.created_ms,
                "updated": saved.map(|x| x.updated_ms),
                "costUsd": j.cost_usd,
                "durationMs": j.duration_ms,
                "backendSessionId": j.session_id,
                "worktree": j.worktree.as_ref().map(|w| &w.path),
                "branch": j.worktree.as_ref().map(|w| &w.branch),
                "worktreeExists": j.worktree.as_ref().is_some_and(|w| w.path.is_dir()),
                "job": j.id.0,
                "file": saved.map(|x| &x.file),
                "bytes": saved.map(|x| x.bytes),
            }),
        ));
    }
    for x in &st.saved {
        if seen.contains(&x.id) {
            continue;
        }
        let mut v = x.to_json(None);
        if let Some(o) = v.as_object_mut() {
            o.insert("worktreeExists".into(), json!(x.worktree.as_ref().is_some_and(|w| w.is_dir())));
        }
        rows.push((x.created_ms, x.id.clone(), v));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(json!({
        "project": st.project,
        "dir": st.project.as_deref().and_then(|p| st.project_dir(p)),
        "saving": active(s),
        "loading": st.loading,
        "sessions": rows.into_iter().map(|r| r.2).collect::<Vec<_>>(),
        "warnings": st.warnings,
    }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "agent.sessions.list",
            "List Agent Sessions",
            None,
            "→ {project, dir, saving, loading, sessions: [{id, title, backend, mode, status (\"interrupted\" for jobs \
             that were running when CodeCraft quit), created, updated (Unix ms), costUsd, durationMs, \
             backendSessionId, worktree, branch, worktreeExists, job (open job id or null), file, bytes}], \
             warnings} — this project's saved agent sessions, newest first",
            Context::Global,
            always,
            list,
        ),
        spec(
            "agent.sessions.open",
            "Open Agent Session",
            None,
            "{session: id} → {job} — reopen a saved session as a job with its activity feed (a review whose \
             worktree still exists can be reviewed again)",
            Context::Global,
            always,
            |s, p| {
                let key = key_param(s, p)?;
                let id = open(s, &key)?;
                Ok(json!({ "job": id.0 }))
            },
        ),
        spec(
            "agent.sessions.resume",
            "Resume Agent Session",
            None,
            "{session: id, prompt: string} → {job} — continue a saved session: `--resume` for the CLIs, \
             ACP `session/load` when offered, else a new session seeded with the transcript",
            Context::Global,
            always,
            |s, p| {
                let key = key_param(s, p)?;
                let prompt = req_str(p, "prompt")?.to_string();
                if prompt.trim().is_empty() {
                    return Err(CommandError::BadParams("the prompt is empty".into()));
                }
                let id = open(s, &key)?;
                agents::follow_up(s, id, &prompt)?;
                Ok(json!({ "job": id.0 }))
            },
        ),
        spec(
            "agent.sessions.rename",
            "Rename Agent Session",
            None,
            "{session: id, title: string}",
            Context::Global,
            always,
            |s, p| {
                let key = key_param(s, p)?;
                rename(s, &key, req_str(p, "title")?)?;
                Ok(Value::Null)
            },
        ),
        spec(
            "agent.sessions.delete",
            "Delete Agent Session",
            None,
            "{session: id} — delete the saved session, its job (not while running) and its review worktree",
            Context::Global,
            always,
            |s, p| {
                let key = key_param(s, p)?;
                delete(s, &key)?;
                Ok(Value::Null)
            },
        ),
        spec(
            "agent.sessions.clear",
            "Delete All Agent Sessions",
            None,
            "→ {deleted, kept} — delete every saved session of this project except running jobs",
            Context::Global,
            always,
            |s, _| {
                let (deleted, kept) = clear(s)?;
                s.status = format!("Deleted {deleted} agent session(s)");
                Ok(json!({ "deleted": deleted, "kept": kept }))
            },
        ),
        spec(
            "agent.sessions.reveal",
            "Show Agent Session File",
            None,
            "{session: id, inEditor?: bool} → {file, dir} — show the session file in the file manager (or open it \
             in an editor tab)",
            Context::Global,
            always,
            |s, p| {
                let key = key_param(s, p)?;
                let file = find_file(s, &key)
                    .filter(|f| f.is_file())
                    .ok_or_else(|| CommandError::Failed(format!("session `{key}` is not saved yet")))?;
                let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
                if bool_or(p, "inEditor", false) {
                    s.execute("file.open", &json!({ "path": file }))?;
                } else {
                    s.effects.push(Effect::OpenUrl(file_url(&dir)));
                }
                Ok(json!({ "file": file, "dir": dir }))
            },
        ),
        spec(
            "agent.sessions.reload",
            "Reload Agent Sessions",
            None,
            "— read this project's saved sessions again",
            Context::Global,
            always,
            |s, _| {
                reload(s);
                Ok(Value::Null)
            },
        ),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn saved(id: &str, created: u64) -> SavedSession {
        SavedSession {
            version: FORMAT_VERSION,
            id: id.into(),
            title: format!("job {id}"),
            backend: Backend::Claude,
            mode: "auto".into(),
            status: JobStatus::Finished,
            project: "/p".into(),
            cwd: None,
            prompt: "do it".into(),
            attachments: Vec::new(),
            created_ms: created,
            updated_ms: created,
            cost_usd: Some(0.1),
            duration_ms: Some(5),
            backend_session_id: Some("b".into()),
            worktree: None,
            worktree_path: None,
            changed_files: Vec::new(),
            review: Vec::new(),
            error: None,
            dropped: 0,
            activity: vec![Activity::Prompt { text: "do it".into() }, Activity::Text { text: "ok".into() }],
        }
    }

    #[test]
    fn keys_are_safe_file_names() {
        assert!(valid_key(&new_key(1_700_000_000_000, JobId(3))));
        for bad in ["", ".x", "-x", "a/b", "a\\b", "..", "a b", &"x".repeat(97)] {
            assert!(!valid_key(bad), "{bad:?}");
        }
        assert!(session_id_of(Path::new("/d/abc-1.json")).is_some());
        assert!(session_id_of(Path::new("/d/.abc-1.json.codecraft-tmp-1-2")).is_none());
        assert!(session_id_of(Path::new("/d/.abc.json")).is_none());
        assert!(session_id_of(Path::new("/d/abc.txt")).is_none());
    }

    #[test]
    fn project_keys_are_stable_and_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("My Proj");
        std::fs::create_dir_all(&p).unwrap();
        let a = project_key(&p);
        assert_eq!(a, project_key(&p.join("x").join("..")));
        assert!(a.starts_with("MyProj-"), "{a}");
        assert_ne!(a, project_key(tmp.path()));
    }

    #[test]
    fn oversized_sessions_are_cut_to_the_cap() {
        let mut s = saved("big", 1);
        let big = "x".repeat(4000);
        s.activity = (0..3000)
            .map(|i| Activity::Tool {
                id: i.to_string(),
                tool: "Bash".into(),
                summary: "Running".into(),
                state: ToolState::Ok,
                output: big.clone(),
            })
            .collect();
        let text = encode(&mut s).unwrap();
        assert!(text.len() <= MAX_FILE_BYTES, "{}", text.len());
        let back: SavedSession = serde_json::from_str(&text).unwrap();
        assert!(back.dropped > 0 || back.activity.len() == 3000);
        assert_eq!(back.activity.len() + back.dropped, 3000);
        // The newest entries are kept.
        assert!(matches!(back.activity.last(), Some(Activity::Tool { id, .. }) if id == "2999"));
    }

    #[test]
    fn unknown_feed_entries_are_skipped() {
        let mut v = serde_json::to_value(saved("a", 1)).unwrap();
        v["activity"].as_array_mut().unwrap().push(json!({"kind": "hologram", "x": 1}));
        let s: SavedSession = serde_json::from_value(v).unwrap();
        assert_eq!(s.activity.len(), 2);
    }

    #[test]
    fn transcript_is_compact_and_capped() {
        let mut feed: VecDeque<Activity> = VecDeque::new();
        assert_eq!(transcript_preamble(&feed), "");
        feed.push_back(Activity::Prompt { text: "fix the bug".into() });
        feed.push_back(Activity::Log { line: "noise".into() });
        feed.push_back(Activity::Text { text: "Fixed.".into() });
        let t = transcript_preamble(&feed);
        assert!(t.contains("User: fix the bug\nAssistant: Fixed."), "{t}");
        assert!(!t.contains("noise"));
        for _ in 0..100 {
            feed.push_back(Activity::Text { text: "y".repeat(1000) });
        }
        let t = transcript_preamble(&feed);
        assert!(t.chars().count() < MAX_TRANSCRIPT_CHARS + 500);
        assert!(t.contains("earlier messages omitted"));
    }

    #[test]
    fn scanning_skips_bad_files_and_marks_running_as_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let mut running = saved("r-1", 20);
        running.status = JobStatus::Running;
        std::fs::write(d.join("r-1.json"), encode(&mut running).unwrap()).unwrap();
        std::fs::write(d.join("f-1.json"), encode(&mut saved("f-1", 10)).unwrap()).unwrap();
        std::fs::write(d.join("bad.json"), "{ not json").unwrap();
        std::fs::write(d.join("other.json"), "[1, 2]").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        std::fs::create_dir(d.join("dir.json")).unwrap();
        let (list, warnings) = scan_dir(d);
        assert_eq!(list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["r-1", "f-1"]);
        assert_eq!(list[0].status, JobStatus::Interrupted);
        assert_eq!(list[1].status, JobStatus::Finished);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(scan_dir(&d.join("missing")).0.is_empty());
    }

    #[test]
    fn pruning_drops_the_oldest_but_never_live_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = SessionsState { dir_override: Some(tmp.path().into()), max_sessions: 2, ..Default::default() };
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            let f = tmp.path().join(format!("{id}.json"));
            std::fs::write(&f, "{}").unwrap();
            st.saved.push(summary_of(&saved(id, i as u64 + 1), f, 10));
        }
        sort(&mut st.saved);
        let live: HashSet<String> = ["a".to_string()].into();
        assert_eq!(prune(&mut st, &live, 1_000_000), 2);
        assert_eq!(st.saved.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["d", "a"]);
        assert!(st.flush(Duration::from_secs(5)));
        assert!(!tmp.path().join("b.json").exists() && !tmp.path().join("c.json").exists());
        assert!(tmp.path().join("a.json").exists());
        // The byte cap too.
        st.max_sessions = 10;
        st.max_project_bytes = 10;
        assert_eq!(prune(&mut st, &HashSet::new(), 1_000_000), 1);
        assert_eq!(st.saved.len(), 1);
    }

    #[test]
    fn dates_and_ages() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_760_400_000_000), "2025-10-14");
        let now = 1_760_400_000_000;
        assert_eq!(age_label(now, now - 5_000), "just now");
        assert_eq!(age_label(now, now - 5 * 60_000), "5 min ago");
        assert_eq!(age_label(now, now - 3 * 3_600_000), "3 h ago");
        assert_eq!(age_label(now, now - 86_400_000), "yesterday");
        assert_eq!(age_label(now, now - 30 * 86_400_000), "2025-09-14");
        assert_eq!(age_label(0, 5), "just now");
    }

    #[test]
    fn file_urls_are_encoded() {
        assert_eq!(file_url(Path::new("/a b/ü")), "file:///a%20b/%C3%BC");
    }
}
