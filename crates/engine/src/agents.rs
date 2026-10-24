//! Coding-agent jobs (Claude Code, Codex, Grok, Gemini): launching, the activity feed, Ask-mode
//! approvals over ACP, and review of worktree changes. OWNED BY the agents track.
//!
//! - **Auto**: the CLI runs headless in the project with its permission bypass; files it changes
//!   are reloaded while it runs (throttled) and when it ends (`file.synchronize`).
//! - **Ask**: the agent runs over ACP; every `session/request_permission` becomes an approval
//!   card ([`crate::approvals`]), file reads come from open documents, writes go through the
//!   engine (undoable in open editors).
//! - **Review**: the CLI runs in a git worktree ([`codecraft_agent::git`]); when it ends the
//!   changed files are listed for per-file accept/reject, and the worktree is removed once every
//!   file is decided.
//!
//! Git and the agents run on worker threads; [`pump`] applies their results on the UI thread.
//! Every job is saved per project and can be reopened and resumed after a restart
//! ([`crate::agent_sessions`]).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_agent::acp::{
    self, AcpClient, HostRequest, PermissionChoice, PermissionKind, PermissionOption, Responder,
};
use codecraft_agent::backend::{self as agent_backend, Overrides};
use codecraft_agent::cli::{self, CliRequest};
use codecraft_agent::git::{self, ChangedFile, Worktree};
use codecraft_agent::{AgentEvent, AgentProcess, Backend, BackendInfo, ChangeKind, PlanEntry, truncate};
use codecraft_workspace::{NotificationLevel, ToolWindowId};

use crate::approvals::{AgentMode, ApprovalId, ApprovalKind, Decision};
use crate::{CommandError, Session};

/// Activity entries kept per job (oldest dropped first).
pub const MAX_ACTIVITY: usize = 1000;
/// Events applied per job per pump (the rest wait for the next frame).
const MAX_EVENTS_PER_PUMP: usize = 500;
/// Minimum time between reloads of changed files while an Auto job runs.
const SYNC_INTERVAL: Duration = Duration::from_millis(800);
/// Selections longer than this are cut in the prompt.
const MAX_SELECTION_CHARS: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Review mode: creating the worktree.
    Preparing,
    Running,
    Finished,
    Failed,
    Stopped,
    /// Review mode: changes are listed and wait for decisions (or are being computed).
    Reviewing,
    /// Review finished (or there was nothing to review).
    Done,
    /// A saved job that was still running when CodeCraft quit (or crashed).
    Interrupted,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Reviewing => "reviewing",
            Self::Done => "done",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Preparing | Self::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

/// One line of a job's activity feed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Activity {
    /// The user's prompt (first or follow-up).
    Prompt {
        text: String,
    },
    /// Assistant Markdown.
    Text {
        text: String,
    },
    Tool {
        id: String,
        tool: String,
        summary: String,
        state: ToolState,
        output: String,
    },
    FileChanged {
        path: PathBuf,
        change: ChangeKind,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    Result {
        is_error: bool,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
        text: Option<String>,
    },
    Error {
        message: String,
    },
    Log {
        line: String,
    },
    /// A note from CodeCraft (worktree created, waiting for approval, …).
    Info {
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewDecision {
    Pending,
    Accepted,
    Rejected,
}

/// One changed file of a review job.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewFile {
    /// The file in the project.
    pub path: PathBuf,
    /// Path relative to the repository root (display).
    pub rel: String,
    pub change: ChangeKind,
    /// Text at the start of the job (`None` for added or binary files).
    pub original: Option<String>,
    /// The agent's text (`None` for deleted or binary files).
    pub proposed: Option<String>,
    /// Not text, or too large to diff; accepting copies the file.
    pub binary: bool,
    pub decision: ReviewDecision,
    /// The project file changed since the job started; accepting overwrites those changes.
    pub conflict: bool,
    /// The agent's file in the worktree.
    #[serde(skip)]
    source: PathBuf,
}

enum Runner {
    Idle,
    Cli(AgentProcess),
    Acp(AcpClient),
}

struct PendingPermission {
    approval: ApprovalId,
    rx: Receiver<Decision>,
    options: Vec<PermissionOption>,
    responder: Responder,
}

/// Results of background git work.
enum Work {
    Worktree(Result<Worktree, String>),
    Review(Result<Vec<ChangedFile>, String>),
    Cleaned(Result<(), String>),
}

pub struct Job {
    pub id: JobId,
    pub backend: Backend,
    pub mode: AgentMode,
    pub title: String,
    pub prompt: String,
    pub attachments: Vec<PathBuf>,
    /// The project directory the job belongs to.
    pub project: PathBuf,
    /// Where the agent runs (the project, or its directory inside the worktree).
    pub cwd: PathBuf,
    pub worktree: Option<Worktree>,
    pub status: JobStatus,
    pub activity: VecDeque<Activity>,
    /// Files the agent reported changing (absolute paths as the agent saw them).
    pub changed_files: Vec<PathBuf>,
    pub review: Vec<ReviewFile>,
    /// Total over all turns.
    pub cost_usd: Option<f64>,
    /// Total over all turns.
    pub duration_ms: Option<u64>,
    pub session_id: Option<String>,
    pub error: Option<String>,
    /// When the current turn started.
    pub started: Instant,
    /// The id of the job's saved session (`agent.sessions.*`); stable across restarts.
    pub session_key: Option<String>,
    /// When the job was created (Unix milliseconds).
    pub created_ms: u64,
    /// Save bookkeeping ([`crate::agent_sessions`]).
    pub(crate) persist: crate::agent_sessions::Persist,
    /// The worktree's path, kept after it is removed so a resumed review recreates it at the
    /// same place (the CLIs key their sessions by directory).
    pub(crate) worktree_path: Option<PathBuf>,
    /// Decisions of a reopened review, applied once its changes are collected again.
    pub(crate) restored_decisions: Vec<(String, ReviewDecision)>,
    /// The pending prompt continues the agent's session (`--resume`) once the worktree exists.
    pub(crate) pending_resume: bool,
    /// While a turn resumes the agent's own session: the prompt with this job's transcript, run
    /// in a new session if the resume fails without any output (the agent's store was cleared).
    pub(crate) resume_seed: Option<String>,
    /// The current turn produced output (text, tools, file changes).
    pub(crate) turn_output: bool,
    runner: Runner,
    events_tx: Sender<AgentEvent>,
    events: Receiver<AgentEvent>,
    host_tx: Sender<HostRequest>,
    host: Receiver<HostRequest>,
    /// The composed prompt waiting for the worktree (review mode).
    pending_prompt: Option<String>,
    stop_requested: bool,
    turn_error: bool,
    turn_reported_duration: bool,
    /// The last activity entry is assistant text still being streamed.
    streaming_text: bool,
    sync_due: Option<Instant>,
    last_sync: Option<Instant>,
    permissions: Vec<PendingPermission>,
    /// Computing the review in the background.
    review_pending: bool,
}

impl Job {
    pub fn is_running(&self) -> bool {
        self.status.is_active()
    }

    /// Approvals this job is waiting for.
    pub fn pending_approvals(&self) -> Vec<ApprovalId> {
        self.permissions.iter().map(|p| p.approval).collect()
    }

    /// The ACP session or CLI process is still alive (follow-ups can reuse an ACP session).
    pub fn has_live_session(&self) -> bool {
        match &self.runner {
            Runner::Acp(c) => c.process().is_running(),
            Runner::Cli(p) => p.is_running(),
            Runner::Idle => false,
        }
    }

    pub fn can_follow_up(&self) -> Result<(), String> {
        if self.is_running() || self.review_pending {
            return Err("the agent is still working".into());
        }
        // Without a live session the follow-up resumes it (a review recreates its worktree).
        Ok(())
    }

    pub fn pending_review(&self) -> usize {
        self.review.iter().filter(|f| f.decision == ReviewDecision::Pending).count()
    }

    fn push(&mut self, a: Activity) {
        self.streaming_text = false;
        self.activity.push_back(a);
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_front();
        }
    }

    fn source(&self) -> String {
        format!("{} (job {})", self.backend.label(), self.id.0)
    }

    /// Short JSON for listings.
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id.0,
            "backend": self.backend.id(),
            "mode": mode_id(self.mode),
            "title": self.title,
            "status": self.status.label(),
            "cwd": self.cwd,
            "project": self.project,
            "worktree": self.worktree.as_ref().map(|w| &w.path),
            "branch": self.worktree.as_ref().map(|w| &w.branch),
            "sessionId": self.session_id,
            "session": self.session_key,
            "created": self.created_ms,
            "costUsd": self.cost_usd,
            "durationMs": self.duration_ms,
            "error": self.error,
            "changedFiles": self.changed_files,
            "pendingApprovals": self.permissions.iter().map(|p| p.approval.0).collect::<Vec<_>>(),
            "review": self.review.iter().map(|f| json!({
                "path": f.path, "rel": f.rel, "change": f.change, "decision": f.decision,
                "binary": f.binary, "conflict": f.conflict,
            })).collect::<Vec<_>>(),
        })
    }

    /// Everything, including the activity feed and the review texts.
    pub fn detail(&self) -> Value {
        let mut v = self.summary();
        if let Some(o) = v.as_object_mut() {
            o.insert("prompt".into(), json!(self.prompt));
            o.insert("attachments".into(), json!(self.attachments));
            o.insert("activity".into(), json!(self.activity));
            o.insert("review".into(), json!(self.review));
        }
        v
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Remove `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` from Claude Code's environment so the
    /// subscription login is used.
    pub scrub_api_keys: bool,
    /// Where review worktrees go (default `~/.codecraft/worktrees`).
    pub worktrees_dir: Option<PathBuf>,
    /// Explicit CLI paths and ACP commands.
    pub overrides: Overrides,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self { scrub_api_keys: true, worktrees_dir: None, overrides: Overrides::default() }
    }
}

pub struct AgentsState {
    pub jobs: Vec<Job>,
    pub settings: AgentSettings,
    next_id: u64,
    backends: Option<Vec<BackendInfo>>,
    work_tx: Sender<(JobId, Work)>,
    work_rx: Receiver<(JobId, Work)>,
    /// Worktree removals still running.
    cleanups: usize,
    /// Saved sessions of the open project, and the switches for saving them.
    pub sessions: crate::agent_sessions::SessionsState,
    /// What `agent.connectExternal` returned, for the UI to show in the Connect an External
    /// Agent window (so Find Action and the menus open it too, not only the ⇄ button).
    pub connect_shown: Option<serde_json::Value>,
    /// Snippets queued by `agent.sendToChat` (editor › Send to Chat); the Agent panel appends them
    /// to its prompt draft and clears the queue.
    pub chat_inbox: Vec<String>,
}

impl Default for AgentsState {
    fn default() -> Self {
        let (work_tx, work_rx) = unbounded();
        Self {
            jobs: Vec::new(),
            settings: AgentSettings::default(),
            next_id: 0,
            backends: None,
            work_tx,
            work_rx,
            cleanups: 0,
            sessions: crate::agent_sessions::SessionsState::default(),
            connect_shown: None,
            chat_inbox: Vec::new(),
        }
    }
}

impl AgentsState {
    pub fn job(&self, id: JobId) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    fn job_mut(&mut self, id: JobId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// Detected backends (cached; stat calls only).
    pub fn backends(&mut self) -> &[BackendInfo] {
        let overrides = &self.settings.overrides;
        self.backends.get_or_insert_with(|| agent_backend::detect_all(overrides))
    }

    /// The cached detection, if any (for drawing without `&mut`).
    pub fn cached_backends(&self) -> Option<&[BackendInfo]> {
        self.backends.as_deref()
    }

    pub fn refresh_backends(&mut self) {
        self.backends = None;
    }

    /// Background work in flight (the UI keeps repainting while true).
    pub fn any_active(&self) -> bool {
        self.cleanups > 0 || self.jobs.iter().any(|j| j.is_running() || j.review_pending || !j.permissions.is_empty())
    }

    pub(crate) fn worktrees_dir(&self) -> Option<PathBuf> {
        self.settings.worktrees_dir.clone().or_else(git::default_base_dir)
    }
}

pub fn mode_id(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Auto => "auto",
        AgentMode::Ask => "ask",
        AgentMode::Review => "review",
    }
}

pub fn parse_mode(s: &str) -> Option<AgentMode> {
    match s {
        "auto" => Some(AgentMode::Auto),
        "ask" => Some(AgentMode::Ask),
        "review" => Some(AgentMode::Review),
        _ => None,
    }
}

/// Which modes a backend supports right now, and why not otherwise.
pub fn mode_support(info: &BackendInfo, mode: AgentMode) -> Result<(), String> {
    match mode {
        AgentMode::Auto | AgentMode::Review => {
            if info.installed() {
                Ok(())
            } else {
                Err(format!("{} is not installed. {}", info.backend.label(), info.backend.install_hint()))
            }
        }
        AgentMode::Ask => info.ask_unavailable().map_or(Ok(()), Err),
    }
}

/// What `agent.start` needs.
#[derive(Clone, Debug)]
pub struct StartRequest {
    pub backend: Backend,
    pub mode: AgentMode,
    pub prompt: String,
    pub attachments: Vec<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    /// Mention the active file in the prompt.
    pub include_file: bool,
    /// Include the active selection in the prompt.
    pub include_selection: bool,
}

/// The prompt plus IDE context: project root, active file and selection, attachments. The CLIs
/// read AGENTS.md / CLAUDE.md themselves.
fn compose_prompt(s: &Session, req: &StartRequest, project: &Path, agent_root: &Path) -> String {
    let rel = |p: &Path| -> String {
        p.strip_prefix(project).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
    };
    let mut ctx = vec![format!("- Project root: {}", agent_root.display())];
    if let Some((editor, doc)) = s.ws.active()
        && let Some(path) = doc.path.as_deref()
    {
        if req.include_file {
            ctx.push(format!("- Active file: {}", rel(path)));
        }
        let sel = editor.selections.primary();
        let (from, to) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
        if req.include_selection && from < to {
            let text = doc.doc.slice(from..to);
            let (l1, l2) = (doc.doc.char_to_line(from) + 1, doc.doc.char_to_line(to) + 1);
            let lang = doc.language.lsp_id();
            ctx.push(format!(
                "- Selected lines {l1}-{l2} of {}:\n```{lang}\n{}\n```",
                rel(path),
                truncate(text.trim_end_matches('\n'), MAX_SELECTION_CHARS)
            ));
        }
    }
    if !req.attachments.is_empty() {
        // Folders get a trailing slash so the agent knows to list them.
        let list: Vec<String> =
            req.attachments.iter().map(|p| if p.is_dir() { format!("{}/", rel(p)) } else { rel(p) }).collect();
        ctx.push(format!("- Attached files and folders: {}", list.join(", ")));
    }
    format!("{}\n\n---\nContext from the IDE (CodeCraft):\n{}", req.prompt.trim_end(), ctx.join("\n"))
}

/// The active editor's selection as a fenced Markdown snippet headed by `file:lines`, for the chat.
pub(crate) fn selection_snippet(s: &Session) -> Option<String> {
    let (editor, doc) = s.ws.active()?;
    let sel = editor.selections.primary();
    let (from, to) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
    if from >= to {
        return None;
    }
    let text = doc.doc.slice(from..to);
    let (l1, l2) = (doc.doc.char_to_line(from) + 1, doc.doc.char_to_line(to) + 1);
    let name = doc.path.as_deref().map(|p| match s.project_root() {
        Some(root) => p.strip_prefix(&root).unwrap_or(p).display().to_string(),
        None => p.display().to_string(),
    });
    let head = match name {
        Some(n) if l1 == l2 => format!("{n}:{l1}"),
        Some(n) => format!("{n}:{l1}-{l2}"),
        None => "Selection".to_string(),
    };
    Some(format!(
        "{head}\n```{}\n{}\n```\n",
        doc.language.lsp_id(),
        truncate(text.trim_end_matches('\n'), MAX_SELECTION_CHARS)
    ))
}

fn default_title(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("Agent job").trim();
    truncate(line, 60)
}

/// Start a job. Returns its id.
pub(crate) fn start(s: &mut Session, req: StartRequest) -> Result<JobId, CommandError> {
    if req.prompt.trim().is_empty() {
        return Err(CommandError::BadParams("the prompt is empty".into()));
    }
    s.agents.refresh_backends();
    let info = s
        .agents
        .backends()
        .iter()
        .find(|b| b.backend == req.backend)
        .cloned()
        .ok_or_else(|| CommandError::Failed("unknown backend".into()))?;
    mode_support(&info, req.mode).map_err(CommandError::Failed)?;
    let project = req
        .cwd
        .clone()
        .map(|p| crate::normalize_path(&p))
        .or_else(|| s.project_root())
        .ok_or_else(|| CommandError::Failed("open a project first (or pass `cwd`)".into()))?;
    if !project.is_dir() {
        return Err(CommandError::Failed(format!("{} is not a directory", project.display())));
    }
    if req.mode == AgentMode::Review && !git::is_repo(&project) {
        return Err(CommandError::Failed(
            "Review mode needs a git repository; use Auto or Ask, or run `git init` and commit first".into(),
        ));
    }
    let title = req.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(&req.prompt));
    let mut job = new_job(s, req.backend, req.mode, title, req.prompt.clone(), project.clone());
    let id = job.id;
    job.attachments = req.attachments.clone();
    job.push(Activity::Prompt { text: req.prompt.clone() });
    match req.mode {
        AgentMode::Auto | AgentMode::Ask => {
            let prompt = compose_prompt(s, &req, &project, &project);
            launch(s, &mut job, &info, prompt, false, None)?;
        }
        AgentMode::Review => {
            let base = s
                .agents
                .worktrees_dir()
                .ok_or_else(|| CommandError::Failed("cannot find the home directory for worktrees".into()))?;
            // The agent sees paths relative to its own root, which is the worktree.
            let prompt = compose_prompt(s, &req, &project, Path::new("."));
            job.pending_prompt = Some(prompt);
            job.status = JobStatus::Preparing;
            job.push(Activity::Info { text: "Creating a git worktree…".into() });
            let wt_id = format!("{}-{}", id.0, unix_millis());
            let tx = s.agents.work_tx.clone();
            let proj = project.clone();
            std::thread::Builder::new()
                .name("codecraft-agent-worktree".into())
                .spawn(move || {
                    let r = git::create_worktree(&proj, &base, &wt_id).map_err(|e| e.to_string());
                    let _ = tx.send((id, Work::Worktree(r)));
                })
                .map_err(|e| CommandError::Failed(format!("cannot start a thread: {e}")))?;
        }
    }
    s.agents.jobs.push(job);
    s.ws.layout.show(ToolWindowId::Agent);
    s.status = format!("Agent job {} started", id.0);
    Ok(id)
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// A new idle job with the next id and a fresh session key (not yet in the job list).
pub(crate) fn new_job(
    s: &mut Session,
    backend: Backend,
    mode: AgentMode,
    title: String,
    prompt: String,
    project: PathBuf,
) -> Job {
    s.agents.next_id += 1;
    let id = JobId(s.agents.next_id);
    let (events_tx, events) = unbounded();
    let (host_tx, host) = unbounded();
    let created_ms = u64::try_from(unix_millis()).unwrap_or(u64::MAX);
    Job {
        id,
        backend,
        mode,
        title,
        prompt,
        attachments: Vec::new(),
        cwd: project.clone(),
        project,
        worktree: None,
        status: JobStatus::Running,
        activity: VecDeque::new(),
        changed_files: Vec::new(),
        review: Vec::new(),
        cost_usd: None,
        duration_ms: None,
        session_id: None,
        error: None,
        started: Instant::now(),
        session_key: Some(crate::agent_sessions::new_key(created_ms, id)),
        created_ms,
        persist: crate::agent_sessions::Persist::default(),
        worktree_path: None,
        restored_decisions: Vec::new(),
        pending_resume: false,
        resume_seed: None,
        turn_output: false,
        runner: Runner::Idle,
        events_tx,
        events,
        host_tx,
        host,
        pending_prompt: None,
        stop_requested: false,
        turn_error: false,
        turn_reported_duration: false,
        streaming_text: false,
        sync_due: None,
        last_sync: None,
        permissions: Vec::new(),
        review_pending: false,
    }
}

/// Add a job reopened from its saved session. With `collect_review`, the changes in its
/// worktree are listed again in the background (saved decisions are reapplied).
pub(crate) fn insert_restored(s: &mut Session, mut job: Job, collect_review: bool) -> JobId {
    let id = job.id;
    if collect_review && let Some(wt) = job.worktree.clone() {
        job.status = JobStatus::Reviewing;
        job.review_pending = true;
        let tx = s.agents.work_tx.clone();
        let spawned = std::thread::Builder::new().name("codecraft-agent-review".into()).spawn(move || {
            let r = git::changed_files(&wt).map_err(|e| e.to_string());
            let _ = tx.send((id, Work::Review(r)));
        });
        if let Err(e) = spawned {
            job.review_pending = false;
            job.push(Activity::Error { message: format!("cannot start a thread: {e}") });
        }
    }
    s.agents.jobs.push(job);
    id
}

/// Start the agent process for a turn.
fn launch(
    s: &Session,
    job: &mut Job,
    info: &BackendInfo,
    prompt: String,
    follow_up: bool,
    acp_resume: Option<acp::Resume>,
) -> Result<(), CommandError> {
    let scrub = s.agents.settings.scrub_api_keys;
    job.started = Instant::now();
    job.turn_output = false;
    if !(follow_up && matches!(job.mode, AgentMode::Auto | AgentMode::Review)) {
        job.resume_seed = None;
    }
    job.turn_error = false;
    job.turn_reported_duration = false;
    job.stop_requested = false;
    job.status = JobStatus::Running;
    // CodeCraft's own tools (debugging, analysis, profiling, tests…) through the IDE server
    // (M14 track AX); the access policy and the job's mode decide what the agent may do.
    let mcp_servers: Vec<codecraft_agent::McpServer> =
        crate::agent_mcp::job_mcp_server(s, job.id).into_iter().collect();
    match job.mode {
        AgentMode::Ask => {
            let launch =
                info.acp.clone().ok_or_else(|| CommandError::Failed(info.ask_unavailable().unwrap_or_default()))?;
            let cmd = cli::acp_command(job.backend, &launch.program, &launch.args, &job.cwd, scrub);
            let mut resume = acp_resume.unwrap_or_default();
            resume.mcp_servers = mcp_servers;
            let client = acp::start_resumed(cmd, &job.cwd, prompt, resume, job.events_tx.clone(), job.host_tx.clone())
                .map_err(|e| CommandError::Failed(e.to_string()))?;
            job.runner = Runner::Acp(client);
        }
        AgentMode::Auto | AgentMode::Review => {
            let program = info.path.clone().ok_or_else(|| CommandError::Failed(info.backend.install_hint().into()))?;
            if cli::mcp_support(job.backend) == cli::McpSupport::UserConfig && !follow_up && !mcp_servers.is_empty() {
                job.push(Activity::Info { text: USER_CONFIG_MCP_HINT.into() });
            }
            let req = CliRequest {
                backend: job.backend,
                program,
                prompt,
                cwd: job.cwd.clone(),
                bypass: true,
                resume: if follow_up { job.session_id.clone() } else { None },
                continue_last: follow_up && job.session_id.is_none() && job.backend == Backend::Grok,
                scrub_api_keys: scrub,
                mcp_servers,
                // A bridge the user registered with the CLI (`mcp --ide`) finds this IDE and job.
                env: crate::agent_mcp::job_env(s, job.id),
                // Gemini reads its MCP servers from a settings file: one of its own per job.
                settings_dir: Some(settings_dir(s, job.id)),
            };
            let process = codecraft_agent::process::spawn_cli(&req, job.events_tx.clone())
                .map_err(|e| CommandError::Failed(e.to_string()))?;
            job.runner = Runner::Cli(process);
        }
    }
    Ok(())
}

/// Shown when a headless agent can't be given CodeCraft's tools on its command line (Grok).
const USER_CONFIG_MCP_HINT: &str = "Grok's headless mode takes MCP servers only from its own configuration, so this \
    job has CodeCraft's tools only if you added them once (Agents › Connect an External Agent: `grok mcp add \
    codecraft-ide -- codecraft-cli mcp --ide`). Ask mode always has them.";

/// A private directory for a job's per-run settings files (Gemini's MCP servers), removed when
/// the turn ends. Named per session too (project windows share the process and job numbers).
fn settings_dir(s: &Session, id: JobId) -> PathBuf {
    std::env::temp_dir().join(format!(
        "codecraft-agent-{}-{:x}-job{}",
        std::process::id(),
        s.agent_tools.instance,
        id.0
    ))
}

fn remove_settings_dir(s: &Session, id: JobId) {
    let dir = settings_dir(s, id);
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        log::warn!("cannot remove {}: {e}", dir.display());
    }
}

fn backend_info(s: &mut Session, backend: Backend) -> Option<BackendInfo> {
    s.agents.backends().iter().find(|b| b.backend == backend).cloned()
}

/// Stop the running turn (or the worktree preparation).
pub(crate) fn stop(s: &mut Session, id: JobId) -> Result<(), CommandError> {
    let job = s.agents.job_mut(id).ok_or_else(|| no_job(id))?;
    if !job.is_running() {
        return Ok(());
    }
    job.stop_requested = true;
    let denied: Vec<ApprovalId> = job.permissions.iter().map(|p| p.approval).collect();
    for p in job.permissions.drain(..) {
        p.responder.permission(&p.options, PermissionChoice::Cancelled);
    }
    match &job.runner {
        Runner::Cli(p) => p.kill(),
        Runner::Acp(c) => c.cancel(),
        Runner::Idle => {}
    }
    if job.status == JobStatus::Preparing {
        job.status = JobStatus::Stopped;
        job.pending_prompt = None;
    }
    job.push(Activity::Info { text: "Stopping…".into() });
    for a in denied {
        s.approvals.decide(a, Decision::Deny);
    }
    Ok(())
}

/// Send a follow-up prompt to a finished job.
pub(crate) fn follow_up(s: &mut Session, id: JobId, prompt: &str) -> Result<(), CommandError> {
    if prompt.trim().is_empty() {
        return Err(CommandError::BadParams("the prompt is empty".into()));
    }
    let backend = s.agents.job(id).ok_or_else(|| no_job(id))?.backend;
    let info = backend_info(s, backend).ok_or_else(|| CommandError::Failed("unknown backend".into()))?;
    let allowed = s.agents.job(id).map(Job::can_follow_up).unwrap_or_else(|| Err(no_job(id).to_string()));
    allowed.map_err(CommandError::Failed)?;
    let Some(pos) = s.agents.jobs.iter().position(|j| j.id == id) else { return Err(no_job(id)) };
    let mut job = s.agents.jobs.remove(pos);
    // What the agent saw so far, for agents that cannot reload their session.
    let preamble = crate::agent_sessions::transcript_preamble(&job.activity);
    job.push(Activity::Prompt { text: prompt.to_string() });
    job.review.clear();
    job.error = None;
    let live_acp = matches!(&job.runner, Runner::Acp(c) if c.process().is_running());
    let result = match &job.runner {
        Runner::Acp(c) if live_acp => {
            if c.prompt(prompt) {
                job.started = Instant::now();
                job.turn_error = false;
                job.stop_requested = false;
                job.status = JobStatus::Running;
                Ok(())
            } else {
                Err(CommandError::Failed("the agent session ended; start a new job".into()))
            }
        }
        _ if job.mode == AgentMode::Ask => {
            // A new agent process continuing the session (`session/load`, else the transcript).
            job.push(Activity::Info { text: "Resuming the agent session…".into() });
            let resume = acp::Resume {
                session_id: job.session_id.clone(),
                fallback_preamble: Some(preamble),
                ..Default::default()
            };
            launch(s, &mut job, &info, prompt.to_string(), false, Some(resume))
        }
        _ if job.mode == AgentMode::Review && job.worktree.is_none() => {
            recreate_worktree(s, &mut job, prompt, preamble)
        }
        _ => {
            let resumable = job.session_id.is_some() || job.backend == Backend::Grok;
            if resumable {
                // If the agent cannot find its session any more, the turn is retried with the
                // transcript (see `finish_turn`).
                job.resume_seed = Some(format!("{preamble}{prompt}"));
                launch(s, &mut job, &info, prompt.to_string(), true, None)
            } else {
                // No agent session to resume: start one that carries the transcript.
                launch(s, &mut job, &info, format!("{preamble}{prompt}"), false, None)
            }
        }
    };
    if let Err(e) = &result {
        job.push(Activity::Error { message: e.to_string() });
    }
    s.agents.jobs.insert(pos.min(s.agents.jobs.len()), job);
    result
}

/// A follow-up on a review whose worktree is gone: create it again (at the same path when the
/// job had one, so the agent finds its session there), then resume.
fn recreate_worktree(s: &mut Session, job: &mut Job, prompt: &str, preamble: String) -> Result<(), CommandError> {
    let base = s
        .agents
        .worktrees_dir()
        .ok_or_else(|| CommandError::Failed("cannot find the home directory for worktrees".into()))?;
    if !git::is_repo(&job.project) {
        return Err(CommandError::Failed(format!("{} is no longer a git repository", job.project.display())));
    }
    // The old place, when it is free (not still being removed) and under the same base.
    let old_id = job
        .worktree_path
        .as_deref()
        .filter(|p| !p.exists() && s.agents.cleanups == 0)
        .filter(|p| p.parent().and_then(Path::parent) == Some(base.as_path()))
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|w| !w.is_empty() && !w.starts_with('.'));
    let same_place = old_id.is_some();
    let wt_id = old_id.unwrap_or_else(|| format!("{}-{}", job.id.0, unix_millis()));
    job.pending_resume = same_place && job.session_id.is_some();
    job.resume_seed = job.pending_resume.then(|| format!("{preamble}{prompt}"));
    job.pending_prompt =
        Some(if job.pending_resume { prompt.to_string() } else { format!("{preamble}{prompt}") });
    job.status = JobStatus::Preparing;
    job.stop_requested = false;
    job.turn_error = false;
    job.push(Activity::Info { text: "Creating a git worktree…".into() });
    let (id, proj, tx) = (job.id, job.project.clone(), s.agents.work_tx.clone());
    std::thread::Builder::new()
        .name("codecraft-agent-worktree".into())
        .spawn(move || {
            let r = git::create_worktree(&proj, &base, &wt_id).map_err(|e| e.to_string());
            let _ = tx.send((id, Work::Worktree(r)));
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a thread: {e}")))?;
    Ok(())
}

fn no_job(id: JobId) -> CommandError {
    CommandError::BadParams(format!("no agent job {}", id.0))
}

/// Discard a job: stop it, deny its approvals, remove its worktree and branch, forget it.
pub(crate) fn discard(s: &mut Session, id: JobId) -> Result<(), CommandError> {
    let pos = s.agents.jobs.iter().position(|j| j.id == id).ok_or_else(|| no_job(id))?;
    let mut job = s.agents.jobs.remove(pos);
    let approvals: Vec<ApprovalId> = job.permissions.iter().map(|p| p.approval).collect();
    for p in job.permissions.drain(..) {
        p.responder.permission(&p.options, PermissionChoice::Cancelled);
    }
    for a in approvals {
        s.approvals.decide(a, Decision::Deny);
    }
    match &job.runner {
        Runner::Cli(p) => p.kill(),
        Runner::Acp(c) => c.shutdown(),
        Runner::Idle => {}
    }
    if let Some(wt) = job.worktree.take() {
        cleanup_worktree(s, id, wt);
    }
    if job.is_running() {
        job.status = JobStatus::Stopped;
    }
    // The saved session stays (history); it no longer has a worktree.
    crate::agent_sessions::save_detached(s, &mut job);
    s.status = format!("Agent job {} discarded", id.0);
    Ok(())
}

/// Remove a worktree and its branch in the background (a deleted saved session's).
pub(crate) fn remove_worktree_later(s: &mut Session, wt: Worktree) {
    cleanup_worktree(s, JobId(0), wt);
}

fn cleanup_worktree(s: &mut Session, id: JobId, wt: Worktree) {
    let Some(base) = s.agents.worktrees_dir() else { return };
    let tx = s.agents.work_tx.clone();
    s.agents.cleanups += 1;
    let spawned = std::thread::Builder::new().name("codecraft-agent-cleanup".into()).spawn(move || {
        let r = git::remove_worktree(&wt, &base).map_err(|e| e.to_string());
        let _ = tx.send((id, Work::Cleaned(r)));
    });
    if spawned.is_err() {
        s.agents.cleanups = s.agents.cleanups.saturating_sub(1);
    }
}

/// Accept or reject one file (`path` may be absolute or relative to the repository root), or
/// all pending files when `path` is `None`.
pub(crate) fn review_decide(
    s: &mut Session,
    id: JobId,
    path: Option<&str>,
    accept: bool,
) -> Result<usize, CommandError> {
    let job = s.agents.job(id).ok_or_else(|| no_job(id))?;
    if job.review_pending {
        return Err(CommandError::Failed("the review is still being prepared".into()));
    }
    let targets: Vec<usize> = match path {
        Some(p) => {
            let abs = crate::normalize_path(Path::new(p));
            let idx = job
                .review
                .iter()
                .position(|f| f.rel == p || f.path == abs || f.path == Path::new(p))
                .ok_or_else(|| CommandError::BadParams(format!("{p} is not in the review of job {}", id.0)))?;
            vec![idx]
        }
        None => (0..job.review.len())
            .filter(|i| job.review.get(*i).is_some_and(|f| f.decision == ReviewDecision::Pending))
            .collect(),
    };
    let mut errors = Vec::new();
    let mut wrote_disk = false;
    for idx in &targets {
        let Some(file) = s.agents.job(id).and_then(|j| j.review.get(*idx)).cloned() else { continue };
        if accept {
            match apply_review_file(s, &file) {
                Ok(disk) => wrote_disk |= disk,
                Err(e) => {
                    errors.push(format!("{}: {e}", file.rel));
                    continue;
                }
            }
        }
        if let Some(f) = s.agents.job_mut(id).and_then(|j| j.review.get_mut(*idx)) {
            f.decision = if accept { ReviewDecision::Accepted } else { ReviewDecision::Rejected };
        }
    }
    if wrote_disk {
        s.jobs_rescan();
    }
    finish_review_if_done(s, id);
    if !errors.is_empty() {
        let msg = errors.join("\n");
        s.notify(NotificationLevel::Error, "Cannot apply agent changes", msg.clone());
        return Err(CommandError::Failed(msg));
    }
    Ok(targets.len())
}

/// Write one reviewed file into the project. Returns whether the disk was written (as opposed
/// to an open document).
fn apply_review_file(s: &mut Session, f: &ReviewFile) -> Result<bool, String> {
    match f.change {
        ChangeKind::Deleted => {
            if f.path.exists() {
                std::fs::remove_file(&f.path).map_err(|e| e.to_string())?;
            }
            Ok(true)
        }
        ChangeKind::Added | ChangeKind::Modified => {
            if let Some(text) = &f.proposed
                && !f.binary
            {
                if replace_open_document(s, &f.path, text, false)? {
                    return Ok(false);
                }
                if let Some(dir) = f.path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                codecraft_fs::write_text_file_atomic(&f.path, text).map_err(|e| e.to_string())?;
                return Ok(true);
            }
            if let Some(dir) = f.path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&f.source, &f.path).map_err(|e| e.to_string())?;
            Ok(true)
        }
    }
}

/// Replace an open document's text as one undoable edit. With `save`, also write it to disk.
/// Returns false if the file is not open.
fn replace_open_document(s: &mut Session, path: &Path, text: &str, save: bool) -> Result<bool, String> {
    let norm = crate::normalize_path(path);
    let Some(doc) = s.ws.doc_by_path(&norm) else { return Ok(false) };
    let editor = s.ws.editor_for_doc(doc);
    let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
    let od = s.ws.doc_mut(doc).ok_or("the document closed")?;
    if od.read_only {
        return Err("the file is read-only".into());
    }
    od.doc.replace_all(text, &sels).map_err(|e| e.to_string())?;
    if save {
        codecraft_fs::write_text_file_atomic(&norm, &od.doc.to_disk_text()).map_err(|e| e.to_string())?;
        od.doc.mark_saved();
        od.disk_mtime = std::fs::metadata(&norm).and_then(|m| m.modified()).ok();
    }
    if let Some(e) = editor {
        let len = s.ws.doc(doc).map(|d| d.doc.len_chars()).unwrap_or(0);
        s.ws.set_selections(e, sels.clamp(len));
        s.after_text_change(e);
    }
    if save {
        s.lsp_did_save(&norm);
    }
    Ok(true)
}

fn finish_review_if_done(s: &mut Session, id: JobId) {
    let Some(job) = s.agents.job_mut(id) else { return };
    if job.status != JobStatus::Reviewing || job.review_pending || job.pending_review() > 0 {
        return;
    }
    let accepted = job.review.iter().filter(|f| f.decision == ReviewDecision::Accepted).count();
    let rejected = job.review.len() - accepted;
    job.status = JobStatus::Done;
    job.push(Activity::Info { text: format!("Review finished: {accepted} accepted, {rejected} rejected") });
    if let Some(wt) = job.worktree.take() {
        // The accepted changes now live in the project; the worktree is going away.
        job.cwd = job.project.clone();
        cleanup_worktree(s, id, wt);
    }
}

// ---------------------------------------------------------------------------------- pump

/// Drain background events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    while let Ok((id, work)) = s.agents.work_rx.try_recv() {
        changed = true;
        apply_work(s, id, work);
    }
    let ids: Vec<JobId> = s.agents.jobs.iter().map(|j| j.id).collect();
    for id in ids {
        changed |= pump_job(s, id);
    }
    changed |= crate::agent_sessions::pump(s);
    changed
}

fn apply_work(s: &mut Session, id: JobId, work: Work) {
    match work {
        Work::Cleaned(r) => {
            s.agents.cleanups = s.agents.cleanups.saturating_sub(1);
            if let Err(e) = r {
                log::warn!("agent job {}: worktree cleanup: {e}", id.0);
                s.notify(NotificationLevel::Warning, "Cannot remove an agent worktree", e);
            }
        }
        Work::Worktree(r) => {
            let Some(info) = s.agents.job(id).map(|j| j.backend).and_then(|b| backend_info(s, b)) else {
                // The job was discarded meanwhile.
                if let Ok(wt) = r {
                    cleanup_worktree(s, id, wt);
                }
                return;
            };
            let Some(pos) = s.agents.jobs.iter().position(|j| j.id == id) else { return };
            let mut job = s.agents.jobs.remove(pos);
            match r {
                Ok(wt) => {
                    job.cwd = wt.cwd();
                    job.push(Activity::Info {
                        text: format!("Working in {} on branch {}", wt.path.display(), wt.branch),
                    });
                    job.worktree_path = Some(wt.path.clone());
                    job.worktree = Some(wt);
                    let resume = std::mem::take(&mut job.pending_resume);
                    match job.pending_prompt.take() {
                        Some(prompt) if !job.stop_requested => {
                            if let Err(e) = launch(s, &mut job, &info, prompt, resume, None) {
                                job.status = JobStatus::Failed;
                                job.error = Some(e.to_string());
                                job.push(Activity::Error { message: e.to_string() });
                            }
                        }
                        _ => {
                            job.status = JobStatus::Stopped;
                            job.cwd = job.project.clone();
                            if let Some(wt) = job.worktree.take() {
                                cleanup_worktree(s, id, wt);
                            }
                        }
                    }
                }
                Err(e) => {
                    job.status = JobStatus::Failed;
                    job.error = Some(e.clone());
                    job.push(Activity::Error { message: format!("Cannot create the worktree: {e}") });
                }
            }
            s.agents.jobs.insert(pos.min(s.agents.jobs.len()), job);
        }
        Work::Review(r) => {
            let Some(job) = s.agents.job_mut(id) else { return };
            job.review_pending = false;
            match r {
                Ok(files) => {
                    let Some(wt) = job.worktree.clone() else { return };
                    let project = job.project.clone();
                    job.review = files.into_iter().filter_map(|f| review_file(&wt, &project, f)).collect();
                    for (rel, decision) in std::mem::take(&mut job.restored_decisions) {
                        if let Some(f) = job.review.iter_mut().find(|f| f.rel == rel) {
                            f.decision = decision;
                        }
                    }
                    if job.review.is_empty() {
                        job.push(Activity::Info { text: "The agent changed no files.".into() });
                    } else {
                        job.push(Activity::Info { text: format!("{} file(s) to review", job.review.len()) });
                    }
                    let n = job.review.len();
                    let title = job.title.clone();
                    s.notify(
                        NotificationLevel::Info,
                        "Agent changes ready for review",
                        format!("{title}: {n} file(s)"),
                    );
                    finish_review_if_done(s, id);
                }
                Err(e) => {
                    job.error = Some(e.clone());
                    job.push(Activity::Error { message: format!("Cannot list the changes: {e}") });
                }
            }
        }
    }
}

fn as_text(bytes: Option<Vec<u8>>) -> (Option<String>, bool) {
    match bytes {
        None => (None, false),
        Some(b) if b.contains(&0) => (None, true),
        Some(b) => match String::from_utf8(b) {
            Ok(s) => (Some(s), false),
            Err(_) => (None, true),
        },
    }
}

/// A review entry, or `None` when the project already has the agent's version.
fn review_file(wt: &Worktree, project: &Path, f: ChangedFile) -> Option<ReviewFile> {
    // Spell paths inside the project the way the user opened it (git reports the canonical
    // repository path, e.g. /private/var vs /var on macOS), so open documents match.
    let path = match f.rel.strip_prefix(&wt.prefix) {
        Ok(inner) => project.join(inner),
        Err(_) => wt.repo.join(&f.rel),
    };
    let path = crate::normalize_path(&path);
    let source = wt.path.join(&f.rel);
    let current = std::fs::read(&path).ok();
    match f.kind {
        ChangeKind::Deleted if current.is_none() => return None,
        ChangeKind::Added | ChangeKind::Modified if f.proposed.is_some() && current == f.proposed => return None,
        _ => {}
    }
    let conflict = match f.kind {
        ChangeKind::Added => current.is_some(),
        _ => !f.too_large && current != f.original,
    };
    let (original, bin_a) = as_text(f.original);
    let (proposed, bin_b) = as_text(f.proposed);
    Some(ReviewFile {
        path,
        rel: f.rel.to_string_lossy().into_owned(),
        change: f.kind,
        original,
        proposed,
        binary: bin_a || bin_b || f.too_large,
        decision: ReviewDecision::Pending,
        conflict,
        source,
    })
}

fn pump_job(s: &mut Session, id: JobId) -> bool {
    let mut changed = false;
    let Some(job) = s.agents.job_mut(id) else { return false };
    let mut events = Vec::new();
    while events.len() < MAX_EVENTS_PER_PUMP {
        match job.events.try_recv() {
            Ok(ev) => events.push(ev),
            Err(_) => break,
        }
    }
    let hosts: Vec<HostRequest> = job.host.try_iter().take(64).collect();
    for ev in events {
        changed = true;
        apply_event(s, id, ev);
    }
    for req in hosts {
        changed = true;
        host_request(s, id, req);
    }
    changed |= poll_permissions(s, id);
    // Throttled reload of files an Auto job changed.
    let now = Instant::now();
    let due = s.agents.job(id).is_some_and(|j| {
        j.sync_due.is_some_and(|d| now >= d) && j.last_sync.is_none_or(|l| now.duration_since(l) >= SYNC_INTERVAL)
    });
    if due {
        if let Some(j) = s.agents.job_mut(id) {
            j.sync_due = None;
            j.last_sync = Some(now);
        }
        synchronize(s);
        changed = true;
    }
    changed
}

fn synchronize(s: &mut Session) {
    if let Err(e) = s.execute("file.synchronize", &Value::Null) {
        log::warn!("agent: synchronize: {e}");
    }
}

fn apply_event(s: &mut Session, id: JobId, ev: AgentEvent) {
    let Some(job) = s.agents.job_mut(id) else { return };
    let mut turn_over = false;
    match ev {
        AgentEvent::Session { id: sid } => job.session_id = Some(sid),
        AgentEvent::Text { text } => {
            job.turn_output = true;
            job.push(Activity::Text { text });
        }
        AgentEvent::TextDelta { text } => {
            job.turn_output = true;
            if job.streaming_text
                && let Some(Activity::Text { text: last }) = job.activity.back_mut()
            {
                last.push_str(&text);
            } else {
                job.push(Activity::Text { text });
                job.streaming_text = true;
            }
        }
        AgentEvent::ToolUse { id: tid, tool, summary, .. } => {
            job.turn_output = true;
            job.push(Activity::Tool { id: tid, tool, summary, state: ToolState::Running, output: String::new() });
        }
        AgentEvent::ToolResult { id: tid, ok, output } => {
            job.streaming_text = false;
            let entry = job.activity.iter_mut().rev().find(|a| matches!(a, Activity::Tool { id, .. } if *id == tid));
            if let Some(Activity::Tool { state, output: out, .. }) = entry {
                *state = if ok { ToolState::Ok } else { ToolState::Failed };
                *out = output;
            }
        }
        AgentEvent::FileChanged { path, kind } => {
            job.turn_output = true;
            if !job.changed_files.contains(&path) {
                job.changed_files.push(path.clone());
            }
            job.push(Activity::FileChanged { path, change: kind });
            if job.mode == AgentMode::Auto {
                job.sync_due.get_or_insert_with(Instant::now);
            }
        }
        AgentEvent::Plan { entries } => {
            if let Some(Activity::Plan { entries: last }) = job.activity.back_mut() {
                *last = entries;
            } else {
                job.push(Activity::Plan { entries });
            }
        }
        AgentEvent::Result { is_error, cost_usd, duration_ms, text } => {
            if let Some(c) = cost_usd {
                job.cost_usd = Some(job.cost_usd.unwrap_or(0.0) + c);
            }
            if let Some(d) = duration_ms {
                job.duration_ms = Some(job.duration_ms.unwrap_or(0) + d);
                job.turn_reported_duration = true;
            }
            job.turn_error |= is_error;
            job.push(Activity::Result { is_error, cost_usd, duration_ms, text });
            // An ACP turn ends with its prompt response; a CLI turn when the process exits.
            turn_over = matches!(job.runner, Runner::Acp(_));
        }
        AgentEvent::Error { message } => {
            job.error = Some(message.clone());
            job.push(Activity::Error { message });
        }
        AgentEvent::Log { line } => {
            if !line.trim().is_empty() {
                job.push(Activity::Log { line });
            }
        }
        AgentEvent::Exited { code, success } => {
            let was_running = job.is_running();
            if matches!(job.runner, Runner::Cli(_)) {
                if !success && !job.stop_requested {
                    job.turn_error = true;
                    if job.error.is_none() {
                        job.error = Some(format!(
                            "the agent exited with {}",
                            code.map_or("a signal".to_string(), |c| format!("code {c}"))
                        ));
                    }
                }
                turn_over = was_running;
            } else if matches!(job.runner, Runner::Acp(_)) {
                // The ACP agent process ended: the session is over.
                if was_running {
                    job.turn_error |= !success && !job.stop_requested;
                    turn_over = true;
                }
                job.push(Activity::Info { text: "The agent session ended.".into() });
            }
            job.runner = Runner::Idle;
        }
    }
    if turn_over {
        finish_turn(s, id);
    }
}

fn finish_turn(s: &mut Session, id: JobId) {
    if !s.agents.job(id).is_some_and(Job::is_running) {
        return;
    }
    remove_settings_dir(s, id);
    let Some(job) = s.agents.job_mut(id) else { return };
    let seed = job.resume_seed.take();
    if let Some(seed) = seed
        && job.turn_error
        && !job.stop_requested
        && !job.turn_output
        && retry_with_transcript(s, id, seed)
    {
        return;
    }
    let Some(job) = s.agents.job_mut(id) else { return };
    if !job.turn_reported_duration {
        let d = u64::try_from(job.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        job.duration_ms = Some(job.duration_ms.unwrap_or(0).saturating_add(d));
    }
    job.status = if job.stop_requested {
        JobStatus::Stopped
    } else if job.turn_error {
        JobStatus::Failed
    } else {
        JobStatus::Finished
    };
    let (mode, title, status) = (job.mode, job.title.clone(), job.status);
    let review_from = job.worktree.clone().filter(|_| mode == AgentMode::Review);
    if let Some(wt) = review_from {
        job.status = JobStatus::Reviewing;
        job.review_pending = true;
        job.push(Activity::Info { text: "Collecting the changes for review…".into() });
        let tx = s.agents.work_tx.clone();
        let spawned = std::thread::Builder::new().name("codecraft-agent-review".into()).spawn(move || {
            let r = git::changed_files(&wt).map_err(|e| e.to_string());
            let _ = tx.send((id, Work::Review(r)));
        });
        if let Err(e) = spawned
            && let Some(job) = s.agents.job_mut(id)
        {
            job.review_pending = false;
            job.push(Activity::Error { message: format!("cannot start a thread: {e}") });
        }
    } else {
        synchronize(s);
        if let Some(job) = s.agents.job_mut(id) {
            job.sync_due = None;
        }
    }
    let level = if status == JobStatus::Failed { NotificationLevel::Warning } else { NotificationLevel::Info };
    s.status = format!("Agent job {}: {}", id.0, status.label());
    if mode != AgentMode::Review {
        s.ws.notify(level, format!("Agent {}", status.label()), title);
    }
}

/// A resumed turn failed before the agent said anything (`claude --resume` whose own session
/// store was cleared, …): run the prompt again in a new session that carries the transcript.
/// False if it could not be started (the turn then fails as usual).
fn retry_with_transcript(s: &mut Session, id: JobId, seed: String) -> bool {
    let Some(backend) = s.agents.job(id).map(|j| j.backend) else { return false };
    let Some(info) = backend_info(s, backend) else { return false };
    let Some(pos) = s.agents.jobs.iter().position(|j| j.id == id) else { return false };
    let mut job = s.agents.jobs.remove(pos);
    let old = job.session_id.take();
    let failure = job.error.take();
    job.push(Activity::Info {
        text: format!(
            "The agent could not resume its session{}{}; continuing in a new session with this conversation's transcript.",
            old.as_deref().map(|o| format!(" {o}")).unwrap_or_default(),
            failure.as_deref().map(|e| format!(" ({})", truncate(e, 200))).unwrap_or_default(),
        ),
    });
    let started = launch(s, &mut job, &info, seed, false, None);
    let ok = started.is_ok();
    if !ok {
        job.session_id = old;
        job.error = failure;
    }
    s.agents.jobs.insert(pos.min(s.agents.jobs.len()), job);
    ok
}

// ------------------------------------------------------------------ ACP host requests

fn host_request(s: &mut Session, id: JobId, req: HostRequest) {
    match req {
        HostRequest::Permission { title, kind, options, responder } => {
            let Some(job) = s.agents.job(id) else {
                return responder.permission(&options, PermissionChoice::Cancelled);
            };
            if job.stop_requested {
                return responder.permission(&options, PermissionChoice::Cancelled);
            }
            let source = job.source();
            let cwd = job.cwd.clone();
            let approval_kind = match kind {
                PermissionKind::FileEdit { path, old_text, new_text } => {
                    let original = old_text.unwrap_or_else(|| current_text(s, &path).unwrap_or_default());
                    ApprovalKind::FileEdit { path, original, proposed: new_text }
                }
                PermissionKind::Command { command } => ApprovalKind::Command { command, cwd: Some(cwd) },
                PermissionKind::Tool { name, input } => ApprovalKind::ToolUse { tool: name, input },
            };
            let (approval, rx) = s.approvals.sink().ask(source, title.clone(), approval_kind);
            if let Some(job) = s.agents.job_mut(id) {
                job.push(Activity::Info { text: format!("Waiting for approval: {title}") });
                job.permissions.push(PendingPermission { approval, rx, options, responder });
            }
        }
        HostRequest::ReadTextFile { path, line, limit, responder } => match current_text(s, &path) {
            Ok(text) => responder.result(json!({ "content": slice_lines(&text, line, limit) })),
            Err(e) => responder.error(-32002, &e),
        },
        HostRequest::WriteTextFile { path, content, responder } => {
            let Some(project) = s.agents.job(id).map(|j| j.project.clone()) else {
                return responder.error(-32603, "the job is gone");
            };
            let norm = crate::normalize_path(&path);
            if !norm.starts_with(&project) {
                return responder.error(-32602, &format!("{} is outside the project", norm.display()));
            }
            match write_through_engine(s, &norm, &content) {
                Ok(existed) => {
                    responder.result(Value::Null);
                    if let Some(job) = s.agents.job_mut(id) {
                        if !job.changed_files.contains(&norm) {
                            job.changed_files.push(norm.clone());
                        }
                        let change = if existed { ChangeKind::Modified } else { ChangeKind::Added };
                        job.push(Activity::FileChanged { path: norm, change });
                    }
                }
                Err(e) => responder.error(-32603, &e),
            }
        }
    }
}

/// The text of a file: the open document if any, else the disk.
fn current_text(s: &Session, path: &Path) -> Result<String, String> {
    let norm = crate::normalize_path(path);
    if let Some(d) = s.ws.doc_by_path(&norm).and_then(|d| s.ws.doc(d)) {
        return Ok(d.doc.text());
    }
    codecraft_fs::read_text_file(&norm).map_err(|e| e.to_string())
}

/// ACP's `line` (1-based) and `limit` applied to a text.
fn slice_lines(text: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let skip = usize::try_from(line.unwrap_or(1).saturating_sub(1)).unwrap_or(usize::MAX);
    let take = limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    text.split_inclusive('\n').skip(skip).take(take).collect()
}

/// An agent's write: into the open document (undoable, and saved so tools that read the disk
/// see it), else to disk. Returns whether the file existed.
fn write_through_engine(s: &mut Session, path: &Path, content: &str) -> Result<bool, String> {
    let existed = path.exists();
    if replace_open_document(s, path, content, true)? {
        return Ok(existed);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    codecraft_fs::write_text_file_atomic(path, content).map_err(|e| e.to_string())?;
    if !existed {
        s.jobs_rescan();
    }
    Ok(existed)
}

fn poll_permissions(s: &mut Session, id: JobId) -> bool {
    let Some(job) = s.agents.job_mut(id) else { return false };
    let mut changed = false;
    let mut i = 0;
    while let Some(p) = job.permissions.get(i) {
        let choice = match p.rx.try_recv() {
            Ok(Decision::Allow) => PermissionChoice::AllowOnce,
            Ok(Decision::AllowAlways) => PermissionChoice::AllowAlways,
            Ok(Decision::Deny) => PermissionChoice::Deny,
            Err(TryRecvError::Disconnected) => PermissionChoice::Cancelled,
            Err(TryRecvError::Empty) => {
                i += 1;
                continue;
            }
        };
        let p = job.permissions.remove(i);
        let label = match choice {
            PermissionChoice::AllowOnce | PermissionChoice::AllowAlways => "Approved",
            PermissionChoice::Deny => "Denied",
            PermissionChoice::Cancelled => "Cancelled",
        };
        job.push(Activity::Info { text: label.into() });
        p.responder.permission(&p.options, choice);
        changed = true;
    }
    changed
}

/// Stop everything (window closing): agents are terminated (TERM, then KILL after a grace
/// period), and worktrees without changes waiting for review are removed. Worktrees with
/// pending reviews are kept (with their branch) so the agent's work is not lost.
pub(crate) fn shutdown(s: &mut Session) {
    let processes: Vec<AgentProcess> = s
        .agents
        .jobs
        .iter()
        .filter_map(|j| match &j.runner {
            Runner::Cli(p) => Some(p.clone()),
            Runner::Acp(c) => {
                c.cancel();
                Some(c.process().clone())
            }
            Runner::Idle => None,
        })
        .collect();
    for p in &processes {
        p.kill();
    }
    let deadline = Instant::now() + Duration::from_millis(1500);
    while processes.iter().any(AgentProcess::is_running) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    for p in &processes {
        // A second kill is immediate (SIGKILL to the group).
        p.kill();
    }
    let base = s.agents.worktrees_dir();
    // With saved sessions an interrupted review can be resumed in its worktree later.
    let persisting = crate::agent_sessions::active(s);
    for job in &mut s.agents.jobs {
        for p in job.permissions.drain(..) {
            p.responder.permission(&p.options, PermissionChoice::Cancelled);
        }
        job.runner = Runner::Idle;
        let keep = (job.status == JobStatus::Reviewing && (job.pending_review() > 0 || job.review_pending))
            || (persisting && job.is_running());
        let mut removed = false;
        if let (Some(wt), Some(base)) = (&job.worktree, &base) {
            if keep {
                log::info!(
                    "keeping agent worktree {} (branch {}) with unreviewed changes",
                    wt.path.display(),
                    wt.branch
                );
            } else if let Err(e) = git::remove_worktree(wt, base) {
                log::warn!("cannot remove agent worktree {}: {e}", wt.path.display());
            } else {
                removed = true;
            }
        }
        if removed {
            job.worktree = None;
            job.cwd = job.project.clone();
        }
    }
    for job in &s.agents.jobs {
        remove_settings_dir(s, job.id);
    }
    // Last save: jobs still running are saved as interrupted.
    crate::agent_sessions::shutdown(s);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn slicing_lines() {
        let t = "a\nb\nc\n";
        assert_eq!(slice_lines(t, None, None), t);
        assert_eq!(slice_lines(t, Some(2), None), "b\nc\n");
        assert_eq!(slice_lines(t, Some(2), Some(1)), "b\n");
        assert_eq!(slice_lines(t, Some(0), Some(1)), "a\n");
        assert_eq!(slice_lines(t, Some(u64::MAX), Some(u64::MAX)), "");
    }

    #[test]
    fn titles_and_modes() {
        assert_eq!(default_title("\n  Fix the bug\nmore"), "Fix the bug");
        for m in [AgentMode::Auto, AgentMode::Ask, AgentMode::Review] {
            assert_eq!(parse_mode(mode_id(m)), Some(m));
        }
        assert_eq!(parse_mode("yolo"), None);
    }
}
