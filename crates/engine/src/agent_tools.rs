//! The agent control plane: CodeCraft's engine tools ([`codecraft_mcp::engine_tools`]) executed
//! against the session, for every MCP transport (the IDE's WebSocket server, the headless stdio
//! server of `codecraft-cli mcp`, built-in agent jobs through the bridge).
//!
//! A transport hands each call to [`submit`] with a [`Respond`] callback. The call's capability
//! is checked against the access policy ([`crate::agent_access`]); Ask becomes an approval card
//! and the call waits for the decision. Tools are built on registered commands (`debug.*`,
//! `run.*`, `lsp.query`, …) so the debugging, analysis and profiling internals can change behind
//! them. Tools that wait (for a debugger stop, test results, a language server, a profile) park
//! the call; [`pump`] (from `Session::pump`) finishes it when the condition holds or its timeout
//! passes. Nothing blocks the UI thread.
//!
//! A call can be cancelled by its client (`notifications/cancelled`, [`cancel`]) or abandoned
//! when the client disconnects ([`client_gone`]); either withdraws its approval card. A Review
//! job's calls run in a session on its worktree ([`crate::agent_worktree`]), never on the open
//! project.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use serde_json::{Map, Value, json};

use codecraft_lsp::ServerKind;
use codecraft_mcp::ToolResult;
use codecraft_mcp::engine_tools as t;

use crate::agent_access::{Capability, Verdict};
use crate::agent_tools_cmds::{LspKind, path_param, server_for, stop_summary};
use crate::agents::JobId;
use crate::approvals::{ApprovalId, ApprovalKind, Decision};
use crate::debug::DebugStatus;
use crate::{CommandError, Effect, LspStatus, Session};

/// How the transport answers a call. Called exactly once.
pub type Respond = Box<dyn FnOnce(ToolResult) + Send>;

/// How long an Ask waits for the user.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Results longer than this (in JSON characters) are cut.
const MAX_RESULT_CHARS: usize = 200_000;
/// LSP answers kept for `lsp.queryResult` at most.
const MAX_LSP_RESULTS: usize = 256;

/// Who is calling: shown on approval cards and used for "Always for this session".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub name: String,
    /// The built-in agent job behind the call (through the bridge).
    pub job: Option<JobId>,
    /// A Review job's directory in its worktree: the call runs there (files, runs, tests,
    /// analysis, debugging), never on the open project. An empty path: the worktree is gone.
    pub root: Option<PathBuf>,
}

/// The `clientInfo` field a job's bridge sends with its job id.
pub const CLIENT_INFO_JOB: &str = "codecraftJob";
/// The `clientInfo` field with the job's key ([`job_key`]); a job id without it is not trusted.
pub const CLIENT_INFO_JOB_KEY: &str = "codecraftJobKey";

impl Caller {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), job: None, root: None }
    }

    /// From an MCP client's `clientInfo` (`name`, and `codecraftJob` + `codecraftJobKey` when
    /// the bridge of an agent job connected). A job id that is not one of the session's jobs, or
    /// comes without the job's key, is ignored (any client with the lock file's token could
    /// claim to be an Auto job otherwise).
    pub fn from_client_info(s: &Session, info: &Value, fallback: &str) -> Self {
        let job = info.get(CLIENT_INFO_JOB).and_then(Value::as_u64).map(JobId).filter(|j| s.agents.job(*j).is_some());
        let key = info.get(CLIENT_INFO_JOB_KEY).and_then(Value::as_str).unwrap_or_default();
        if let Some(job) = job.filter(|j| !key.is_empty() && key == job_key(s, *j)).and_then(|j| s.agents.job(j)) {
            let root = (job.mode == crate::approvals::AgentMode::Review).then(|| {
                job.worktree.as_ref().map(|wt| crate::normalize_path(&wt.cwd())).unwrap_or_default()
            });
            return Self { name: format!("{} (job {})", job.backend.label(), job.id.0), job: Some(job.id), root };
        }
        let name = info.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).unwrap_or(fallback);
        Self { name: name.to_string(), job: None, root: None }
    }
}

/// The secret a job's bridge proves it belongs to `job` with (in the bridge's environment, never
/// in the lock file). Stable for the session, different per job.
pub fn job_key(s: &Session, job: JobId) -> String {
    use std::hash::{BuildHasher, Hasher};
    let st = &s.agent_tools.job_secret;
    let part = |salt: u8| {
        let mut h = st.hasher.build_hasher();
        h.write(st.token.as_bytes());
        h.write_u8(salt);
        h.write_u64(job.0);
        h.finish()
    };
    format!("{:016x}{:016x}", part(1), part(2))
}

/// One call on one connection, for `notifications/cancelled` and disconnects.
#[derive(Clone, Debug, PartialEq)]
pub struct CallKey {
    /// The connection (`ide:3`, `stdio`).
    pub client: String,
    /// The JSON-RPC id of the `tools/call`.
    pub request: Value,
}

impl CallKey {
    pub fn new(client: impl Into<String>, request: Value) -> Self {
        Self { client: client.into(), request }
    }
}

/// Per-session randomness behind [`job_key`].
pub(crate) struct JobSecret {
    hasher: std::hash::RandomState,
    token: String,
}

impl Default for JobSecret {
    fn default() -> Self {
        Self { hasher: std::hash::RandomState::new(), token: codecraft_mcp::new_token().unwrap_or_default() }
    }
}

/// The connection name of calls a session forwards to a worktree session.
const FORWARD_CLIENT: &str = "worktree";
/// How long a forwarded call may take at most (the worktree session has its own timeouts).
const FORWARD_TIMEOUT: Duration = Duration::from_secs(3600);

/// Pending calls and language-server answers.
pub struct AgentToolsState {
    pending: Vec<PendingCall>,
    lsp_next: u64,
    lsp_inflight: HashMap<u64, LspKind>,
    lsp_done: BTreeMap<u64, (LspKind, Result<Value, String>)>,
    /// Calls answered so far.
    pub answered: u64,
    /// How long an Ask waits for the user ([`APPROVAL_TIMEOUT`]; tests shorten it).
    pub approval_timeout: Duration,
    /// Calls the client cancelled or abandoned by disconnecting.
    pub cancelled: u64,
    pub(crate) job_secret: JobSecret,
    /// Tells this session's per-job files from other sessions' in the same process.
    pub(crate) instance: u64,
    /// Sessions on Review jobs' worktrees.
    pub(crate) worktrees: Vec<crate::agent_worktree::WorktreeSession>,
    next_forward: u64,
}

impl Default for AgentToolsState {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            lsp_next: 0,
            lsp_inflight: HashMap::new(),
            lsp_done: BTreeMap::new(),
            answered: 0,
            approval_timeout: APPROVAL_TIMEOUT,
            cancelled: 0,
            job_secret: JobSecret::default(),
            instance: {
                use std::hash::{BuildHasher, Hasher};
                let mut h = std::hash::RandomState::new().build_hasher();
                h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
                h.finish()
            },
            worktrees: Vec::new(),
            next_forward: 0,
        }
    }
}

impl AgentToolsState {
    /// Calls still waiting (for the user, a debugger stop, results…).
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Names of the waiting calls (status display, tests).
    pub fn pending_tools(&self) -> Vec<String> {
        self.pending.iter().map(|p| p.name.clone()).collect()
    }

    /// The worktree directories Review jobs' calls run in now (status display, tests).
    pub fn worktree_roots(&self) -> Vec<PathBuf> {
        self.worktrees.iter().map(|w| w.root.clone()).collect()
    }

    pub(crate) fn next_lsp_token(&mut self) -> u64 {
        self.lsp_next = self.lsp_next.saturating_add(1);
        self.lsp_next
    }

    pub(crate) fn lsp_started(&mut self, token: u64, kind: LspKind) {
        self.lsp_inflight.insert(token, kind);
    }

    pub(crate) fn lsp_pending(&self, token: u64) -> bool {
        self.lsp_inflight.contains_key(&token)
    }

    pub(crate) fn lsp_take(&mut self, token: u64) -> Option<(LspKind, Result<Value, String>)> {
        self.lsp_done.remove(&token)
    }
}

/// A language server answered an agent query ([`crate::lsp::Pending::Agent`]).
pub(crate) fn on_lsp_response(s: &mut Session, token: u64, result: Result<Value, codecraft_lsp::ResponseError>) {
    let st = &mut s.agent_tools;
    let Some(kind) = st.lsp_inflight.remove(&token) else { return };
    st.lsp_done.insert(token, (kind, result.map_err(|e| e.message)));
    while st.lsp_done.len() > MAX_LSP_RESULTS {
        let Some(oldest) = st.lsp_done.keys().next().copied() else { break };
        st.lsp_done.remove(&oldest);
    }
}

struct PendingCall {
    caller: Caller,
    key: Option<CallKey>,
    name: String,
    deadline: Instant,
    respond: Respond,
    wait: Wait,
}

/// What a parked call waits for.
enum Wait {
    Approval {
        approval: ApprovalId,
        rx: Receiver<Decision>,
        cap: Capability,
        tool: String,
        args: Value,
    },
    /// The debugger to stop (settled) or the session to end: debug session `session` (or one of
    /// its subprocess sessions); `None`: any session (M15 track DM).
    DebugStop {
        frames: usize,
        session: Option<u64>,
    },
    /// Another wait, for debug session `session` (a tool call with a `session` argument).
    InSession {
        session: u64,
        inner: Box<Wait>,
    },
    /// The stop to settle after selecting a thread or frame, then answer.
    DebugSettle {
        then: AfterSettle,
    },
    DebugVariables {
        reference: i64,
        limit: usize,
    },
    DebugEval {
        expression: String,
    },
    /// A language server to become ready, then send the query.
    LspReady {
        server: ServerKind,
        kind: LspKind,
        args: Value,
        limit: usize,
        then: Option<LspKind>,
    },
    Lsp {
        token: u64,
        limit: usize,
        then: Option<LspKind>,
    },
    Inspection {
        limit: usize,
    },
    Tests {
        tab: u64,
    },
    Profile {
        limit: usize,
        /// The newest profile's snapshot was opened (it was not the one shown).
        reopened: bool,
        /// `self` or `total` (M15 PP: a fixed order, not the Method List's).
        sort: Option<String>,
    },
    RunDone {
        args: Value,
    },
    /// A command that answered `pending: true` to answer again without it (M16 LT:
    /// `analysis.fileFindings {waitMs}` waits for the current revision's results).
    Pending {
        id: String,
        params: Value,
    },
    Db {
        source: String,
        before: Vec<u64>,
        output_before: usize,
        limit: usize,
        /// A SQLite file this call added as a data source.
        added: Option<Value>,
    },
    /// The project to finish loading (scan, Cargo metadata), then run the tool: its run
    /// configurations and targets are not known before.
    ProjectLoad {
        tool: String,
        args: Value,
    },
    /// The answer of a call forwarded to a Review job's worktree session.
    Worktree {
        slot: Arc<Mutex<Option<ToolResult>>>,
        job: JobId,
        forward: u64,
    },
}

enum AfterSettle {
    Stack { limit: usize },
    Variables { limit: usize },
    Evaluate { expression: String },
}

/// The outcome of starting or polling a tool.
enum Step {
    Done(ToolResult),
    Wait(Wait, Duration),
}

fn done_json(v: Value) -> Step {
    Step::Done(json_result(&v))
}

fn done_err(msg: impl Into<String>) -> Step {
    Step::Done(ToolResult::error(msg))
}

/// Compact JSON text; very large results are cut with a note.
pub(crate) fn json_result(v: &Value) -> ToolResult {
    let text = v.to_string();
    if text.len() <= MAX_RESULT_CHARS {
        return ToolResult::text(text);
    }
    let cut: String = text.chars().take(MAX_RESULT_CHARS / 4).collect();
    ToolResult::json(&json!({
        "truncated": format!(
            "the result is {} KB; showing the first part. Narrow the request (limit, path, offset) to see the rest",
            text.len() / 1024
        ),
        "preview": cut,
    }))
}

fn cmd_err(e: CommandError) -> Step {
    done_err(e.to_string())
}

fn timeout_arg(args: &Value, default_ms: u64) -> Duration {
    let ms = args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(default_ms).min(600_000);
    Duration::from_millis(ms)
}

fn usize_arg(args: &Value, key: &str, default: usize, max: usize) -> usize {
    args.get(key).and_then(Value::as_u64).map_or(default, |n| usize::try_from(n).unwrap_or(max)).clamp(1, max)
}

/// `sort` of profile hotspots (`self` | `total`), passed on as given (the command validates it).
fn sort_arg(args: &Value) -> Option<String> {
    args.get("sort").and_then(Value::as_str).map(str::to_string)
}

// ---------------------------------------------------------------- capabilities

/// Read-only commands outside the read-only namespaces.
const READ_COMMANDS: &[&str] = &[
    "agentAccess.get",
    "analysis.fileFindings",
    "analysis.problems",
    "commands.search",
    "coverage.summary",
    "db.connect",
    "db.disconnect",
    "db.expand",
    "db.getHistory",
    "db.getResult",
    "db.listDataSources",
    "db.refresh",
    "db.rowCount",
    "db.testConnection",
    "debug.state",
    "debug.stopSummary",
    "debug.agentSummary",
    "debug.variablesOf",
    "debug.sessions",
    "files.read",
    "inspections.results",
    "lsp.query",
    "lsp.queryResult",
    "lsp.status",
    "profiler.callTree",
    "profiler.compare",
    "profiler.hotspots",
    "profiler.job",
    "profiler.lineHits",
    "profiler.memory",
    "profiler.methods",
    "profiler.report",
    "profiler.recent",
    "profiler.status",
    "profiler.tools",
    "project.info",
    "python.languageServer",
    "run.configurations",
    "run.output",
    "run.testHistory",
    "runConfig.generate.list",
    "runConfig.list",
    "runConfig.templates.list",
    "test.summary",
    "test.tree",
    // QA M15: the playbook's `vcs.status` (and the other repository queries) needed `write`.
    "vcs.changelists",
    "vcs.merge.state",
    "vcs.shelves",
    "vcs.status",
    "vcs.worktrees",
];

/// Commands in run-like namespaces that change files.
/// Adding or editing a data source changes the project's settings, not a database (QA M14: it
/// needed `db-write`).
const WRITE_COMMANDS: &[&str] = &[
    "inspections.fix",
    "inspections.fixAll",
    "inspections.suppress",
    "test.export",
    "test.clearHistory",
    "db.addDataSource",
    "db.editDataSource",
];

/// Commands that act outside this machine or can't be taken back: publishing code (push, GitHub
/// and GitLab sharing, gists, pull requests, fork sync), changing deployed servers, pruning
/// Docker. The user runs these; agents only see them in search results.
const OUTWARD_COMMANDS: &[&str] = &[
    "git.push",
    "vcs.push",
    "vcs.createPullRequest",
    "vcs.pr.merge",
    "vcs.github.share",
    "vcs.github.createGist",
    "git.gitLab.shareProjectOnGitLab",
    "remote.sync.apply",
    "remote.host.delete",
    "remote.host.mkdir",
    "remote.host.rename",
    "docker.prune",
];

/// Prefixes of [`OUTWARD_COMMANDS`]-like groups.
const OUTWARD_PREFIXES: &[&str] = &[
    "git.gitHub.createGist",
    "git.gitHub.createPullRequest",
    "git.gitHub.shareProjectOnGitHub",
    "git.gitHub.syncFork",
    "tools.deployment.upload",
    "tools.deployment.sync",
    "tools.deployment.automaticUpload",
];

/// The capability a command needs through `codecraft_execute`; `None` = agents may not run it
/// (their own permissions, approvals, other agents, the IDE integration itself, and commands
/// that publish or change things outside this machine).
pub fn command_capability(id: &str) -> Option<Capability> {
    let id = crate::registry().canonical(id);
    if READ_COMMANDS.contains(&id) {
        return Some(Capability::Read);
    }
    if id.starts_with("agentAccess.")
        || id.starts_with("agent.")
        || id.starts_with("ide.")
        || id.starts_with("feedback.")
        || OUTWARD_COMMANDS.contains(&id)
        || OUTWARD_PREFIXES.iter().any(|p| id.starts_with(p))
    {
        return None;
    }
    if WRITE_COMMANDS.contains(&id) {
        return Some(Capability::Write);
    }
    let debug = id.starts_with("debug.")
        || id.starts_with("run.debuggingActions.")
        || id.starts_with("run.toggleBreakpoint.")
        || matches!(id, "run.debug" | "run.chooseAndDebug" | "run.attachToProcess" | "run.viewBreakpoints");
    if debug {
        return Some(Capability::Debug);
    }
    if id.starts_with("profiler.")
        || id.starts_with("run.profile")
        || matches!(id, "run.attachProfilerToProcess" | "run.openProfilerSnapshot" | "run.openProfilerSnapshot.open")
    {
        return Some(Capability::Profile);
    }
    if id.starts_with("db.") {
        return Some(Capability::DbWrite);
    }
    let run = ["run.", "cargo.", "build.", "test.", "coverage.", "terminal.", "zig.", "docker.", "inspections."]
        .iter()
        .any(|p| id.starts_with(p))
        || matches!(id, "code.inspectCode" | "analysis.analyzeFile");
    if run {
        return Some(Capability::Run);
    }
    if ["navigate.", "view.", "window.", "help.", "hierarchy."].iter().any(|p| id.starts_with(p)) {
        return Some(Capability::Read);
    }
    Some(Capability::Write)
}

/// File extensions of SQLite databases (an empty file with one of these is a new database).
const SQLITE_EXTENSIONS: &[&str] = &["db", "sqlite", "sqlite3", "db3"];

/// Whether `path` is a SQLite database: the file header, or an empty file named like one.
pub(crate) fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    if meta.len() == 0 {
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
        return SQLITE_EXTENSIONS.contains(&ext.as_str());
    }
    let mut head = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
}

/// A SQLite file an agent named as `dataSource` that no data source points at yet (QA M14:
/// "what's in this table" on a project's `.db` file needed the agent to find `db.addDataSource`).
fn unconfigured_sqlite(s: &Session, wanted: &str) -> Option<std::path::PathBuf> {
    let path = crate::ide_server::resolve_path(s, wanted)?;
    // Only the project's own files; other databases are added by the user.
    let inside = s.project_root().is_some_and(|root| path.starts_with(&root));
    if !inside || !is_sqlite_file(&path) {
        return None;
    }
    let known = source_for_file(s, &path).is_some();
    (!known).then_some(path)
}

/// A data source's file path as an absolute path (data sources keep project files relative).
fn source_path(s: &Session, p: &Path) -> std::path::PathBuf {
    match s.db.project_root() {
        Some(root) if p.is_relative() => crate::normalize_path(&root.join(p)),
        _ => crate::normalize_path(p),
    }
}

/// The data source pointing at a SQLite file (by path).
fn source_for_file(s: &Session, path: &Path) -> Option<String> {
    s.db.sources
        .iter()
        .find(|d| d.config.path.as_ref().is_some_and(|p| source_path(s, p) == path))
        .map(|d| d.config.id.clone())
}

/// SQLite files in the project that no data source points at (at most `max`).
fn sqlite_candidates(s: &Session, max: usize) -> Vec<std::path::PathBuf> {
    let Some(project) = s.ws.project.as_ref() else { return Vec::new() };
    project
        .tree
        .files()
        .filter(|n| {
            n.path.extension().and_then(|e| e.to_str()).is_some_and(|e| SQLITE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        })
        .map(|n| n.path.clone())
        .filter(|p| is_sqlite_file(p) && source_for_file(s, p).is_none())
        .take(max)
        .collect()
}

/// Commands that act on an SSH host or a Docker container whatever their params (M16: asked
/// every time through `remote`).
const REMOTE_PREFIXES: &[&str] = &[
    "remote.",
    "docker.",
    "ssh.",
    "tools.deployment.",
    "tools.startSshSession",
    "tools.services.docker",
    "file.remoteDevelopment",
    "python.remoteInterpreters.detect",
    "python.remoteInterpreters.packages",
    "run.manageTargets.resolve",
    "debug.pythonRemote",
];

/// What a call would act on remotely (an SSH host or Docker container: a run target, a
/// configuration whose target is remote, Docker/deployment/remote-host commands, remote
/// debugging, a data source behind an SSH tunnel), for the approval card; `None` = local.
fn remote_reason(s: &mut Session, name: &str, args: &Value) -> Option<String> {
    // Editing/saving a mirror can enqueue an SSH upload even without an explicit target.
    // Apply the remote gate before the local write capability can trigger that side effect.
    if let Some(project) = &s.remote.project {
        let writes_mirror = matches!(name, t::FILE_WRITE | t::FILE_EDIT)
            || (name == t::EXECUTE
                && args
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| command_capability(id) == Some(Capability::Write)));
        if writes_mirror {
            return Some(format!("remote project {} (checked save uploads)", project.server));
        }
        let target_default = matches!(name, t::RUN_START | t::DEBUG_START | t::PROFILE_START | t::TEST_RUN)
            && args.get("target").is_none_or(Value::is_null);
        let command = (name == t::EXECUTE).then(|| args.get("id").and_then(Value::as_str)).flatten();
        let params = args.get("params").unwrap_or(&Value::Null);
        let command_default = command.is_some_and(|id| crate::target_exec::TARGET_COMMANDS.contains(&id)
            && params.get("target").is_none_or(Value::is_null));
        let terminal = command.is_some_and(|id| id == "terminal.newRemote"
            || (id == "terminal.new" && params.get("command").is_none_or(Value::is_null))
            || (matches!(id, "terminal.sendText" | "terminal.sendKey" | "terminal.paste" | "terminal.close")
                && params.get("tab").and_then(Value::as_u64).and_then(|id| s.terminal.tabs.iter().find(|t| t.id == id))
                    .or_else(|| s.terminal.tabs.get(s.terminal.active)).is_some_and(|t| t.remote)));
        if target_default || command_default || terminal {
            return Some(format!("remote project {} (default SSH execution)", project.server));
        }
    }
    fn label(s: &mut Session, v: &Value) -> Option<String> {
        match crate::target_exec::resolve_ref(s, v) {
            Ok(t) if t.is_local() => None,
            Ok(t) => Some(t.label()),
            // Unknown: it fails later with its own error, but it may name a remote target.
            Err(_) => Some(format!("target {v}")),
        }
    }
    // `defer`: the tool waits for the project to load before it runs and is judged again then
    // (`Wait::ProjectLoad`), so an unknown configuration while loading isn't decided yet.
    fn config(s: &Session, v: Option<&Value>, defer: bool) -> Option<String> {
        let v = v.filter(|v| !v.is_null())?;
        match config_index(s, v) {
            Ok(i) => s
                .run_configs
                .get(i)
                .filter(|c| !c.target.is_local())
                .map(|c| format!("{} ({})", c.target.label(), c.name)),
            // While the project loads its configurations aren't known yet: one of them may be remote.
            Err(_) if s.project_loading() && !defer => {
                Some(format!("configuration {v} (the project is still loading)"))
            }
            Err(_) => None,
        }
    }
    fn tunnel(s: &Session, v: Option<&Value>) -> Option<String> {
        let id = find_source(s, v?.as_str()?)?;
        let d = s.db.sources.iter().find(|d| d.config.id == id)?;
        let ssh = d.config.ssh.is_some() || d.config.ssh_tunnel.as_deref().is_some_and(|t| !t.trim().is_empty());
        ssh.then(|| format!("data source {} (through an SSH tunnel)", d.config.name))
    }
    match name {
        t::DEBUG_START | t::PROFILE_START | t::TEST_RUN | t::RUN_START => match args.get("target").filter(|v| !v.is_null()) {
            Some(v) => label(s, v),
            None if args.get("config").is_some_and(|c| !c.is_null()) => config(s, args.get("config"), true),
            // A file, test or program: the configuration picked for it may be remote, so a
            // project with remote configurations asks unless the call says `target: "local"`.
            None => s
                .run_configs
                .iter()
                .find(|c| !c.target.is_local())
                .map(|c| format!("a remote configuration, maybe ({}: {}); pass target \"local\" to stay here", c.name, c.target.label())),
        },
        t::DB_QUERY => tunnel(s, args.get("dataSource")),
        t::EXECUTE => {
            let id = crate::registry().canonical(args.get("id").and_then(Value::as_str)?);
            let params = args.get("params").cloned().unwrap_or(Value::Null);
            // Reopening a cached mirror starts its SSH connection before any tool runs.
            if id == "file.open" && let Some(path) = params.get("path").and_then(Value::as_str) {
                let path = crate::normalize_path(std::path::Path::new(path));
                let root = if path.is_dir() { Some(path) }
                    else if s.ws.project.is_none() { codecraft_cargo::find_manifest(&path).and_then(|p| p.parent().map(std::path::Path::to_path_buf)) }
                    else { None };
                if let Some(server) = root.and_then(|root| crate::deployment::load(&root).ok()).and_then(|cfg| cfg.remote_development) {
                    return Some(format!("reopen remote project {server}"));
                }
            }
            if REMOTE_PREFIXES.iter().any(|p| id.starts_with(p)) {
                return Some(format!("`{id}`"));
            }
            if id.starts_with("db.")
                && let Some(on) = tunnel(s, params.get("dataSource"))
            {
                return Some(on);
            }
            match params.get("target").filter(|v| !v.is_null()) {
                Some(v) if crate::target_exec::TARGET_COMMANDS.contains(&id) => label(s, v),
                _ if matches!(id, "run.run" | "run.debug" | "run.profile") => config(s, params.get("config"), false),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The data source an agent named (by id or name, or a SQLite file a data source points at).
fn find_source(s: &Session, wanted: &str) -> Option<String> {
    if let Some(id) = find_source_by_name(s, wanted) {
        return Some(id);
    }
    let path = crate::ide_server::resolve_path(s, wanted)?;
    source_for_file(s, &path)
}

fn find_source_by_name(s: &Session, wanted: &str) -> Option<String> {
    let w = wanted.trim();
    s.db.sources
        .iter()
        .find(|d| d.config.id == w)
        .or_else(|| s.db.sources.iter().find(|d| d.config.name.eq_ignore_ascii_case(w)))
        .map(|d| d.config.id.clone())
}

/// Whether `sql` may modify data on `source`. Stricter than the production guard: an agent's
/// statement counts as reading only when it starts with a plain read (`SELECT` without `INTO`,
/// `WITH` without a write, `SHOW`, `DESCRIBE`, `EXPLAIN` without `ANALYZE`, `VALUES`, `TABLE`,
/// `PRAGMA` without `=`) and calls no function known to change state; session changes (`SET`,
/// `BEGIN`, `PREPARE`, …) need db-write too.
fn sql_writes(s: &Session, source: &str, sql: &str) -> bool {
    sql_writes_in(s.db.dialect_for(None, Some(source)), sql)
}

fn sql_writes_in(dialect: codecraft_db::Dialect, sql: &str) -> bool {
    codecraft_db::split_statements(sql, dialect)
        .iter()
        .any(|st| codecraft_db::is_modifying(&st.text, dialect) || !agent_reads_only(&st.text, dialect))
}

/// Functions that change state although they can be called from a `SELECT`.
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    // PostgreSQL
    "NEXTVAL",
    "SETVAL",
    "SET_CONFIG",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_XACT_LOCK",
    "PG_TRY_ADVISORY_LOCK",
    "PG_SWITCH_WAL",
    "PG_CREATE_RESTORE_POINT",
    "PG_PROMOTE",
    "PG_DROP_REPLICATION_SLOT",
    "PG_CREATE_LOGICAL_REPLICATION_SLOT",
    "PG_CREATE_PHYSICAL_REPLICATION_SLOT",
    "PG_FILE_WRITE",
    "PG_FILE_UNLINK",
    "PG_FILE_RENAME",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_UNLINK",
    "LO_CREATE",
    "DBLINK",
    "DBLINK_EXEC",
    "QUERY_TO_XML",
    // MySQL / MariaDB
    "GET_LOCK",
    "RELEASE_LOCK",
    "RELEASE_ALL_LOCKS",
    "LOAD_FILE",
    "SLEEP",
    "BENCHMARK",
    // SQLite
    "LOAD_EXTENSION",
    "WRITEFILE",
    "EDIT",
];

/// The statement's words, upper-cased, with comments and string literals left out.
fn sql_words(sql: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut chars = sql.chars().peekable();
    let flush = |cur: &mut String, words: &mut Vec<String>| {
        if !cur.is_empty() {
            words.push(std::mem::take(cur).to_ascii_uppercase());
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                flush(&mut cur, &mut words);
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                flush(&mut cur, &mut words);
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            '\'' => {
                flush(&mut cur, &mut words);
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                }
            }
            c if c.is_alphanumeric() || c == '_' => cur.push(c),
            _ => flush(&mut cur, &mut words),
        }
    }
    flush(&mut cur, &mut words);
    words
}

/// Whether an agent's statement only reads (see [`sql_writes`]).
fn agent_reads_only(sql: &str, dialect: codecraft_db::Dialect) -> bool {
    if dialect == codecraft_db::Dialect::Redis {
        return !codecraft_db::is_modifying(sql, dialect);
    }
    let words = sql_words(sql);
    let Some(first) = words.first() else { return true };
    let has = |w: &str| words.iter().skip(1).any(|x| x == w);
    let plain_read = match first.as_str() {
        "SELECT" => !has("INTO"),
        "WITH" => !["INSERT", "UPDATE", "DELETE", "MERGE", "INTO"].iter().any(|w| has(w)),
        "EXPLAIN" => !has("ANALYZE") && !has("ANALYSE"),
        "SHOW" | "DESCRIBE" | "DESC" | "VALUES" | "TABLE" => true,
        "PRAGMA" => !sql.contains('='),
        _ => false,
    };
    plain_read && !words.iter().any(|w| SIDE_EFFECT_FUNCTIONS.contains(&w.as_str()))
}

/// `sql` with the `{"sql": …}` expressions of `params` inlined (bound values stay
/// placeholders): what decides whether the call writes.
fn expand_sql_params(s: &Session, source: &str, sql: &str, params: Option<&Value>) -> String {
    let Some(Value::Object(m)) = params else { return sql.to_string() };
    let mut values = codecraft_db::bind::ParamValues::new();
    for (k, v) in m {
        if let Some(e) = v.get("sql").and_then(Value::as_str) {
            values.insert(k.clone(), codecraft_db::bind::ParamValue::Sql(e.to_string()));
        }
    }
    if values.is_empty() {
        return sql.to_string();
    }
    let dialect = s.db.source(source).map(|d| d.config.kind.dialect()).unwrap_or(codecraft_db::Dialect::Postgres);
    // Every expression is judged, wherever it lands (a statement after the expansion too).
    let mut out = codecraft_db::bind::expand_sql_values(sql, dialect, &values);
    for v in values.values() {
        if let codecraft_db::bind::ParamValue::Sql(e) = v {
            out.push_str(";\nSELECT ");
            out.push_str(e);
        }
    }
    out
}

/// The capability one call needs (some depend on the arguments).
fn required_capability(s: &Session, name: &str, args: &Value) -> Result<Capability, String> {
    Ok(match name {
        t::EXECUTE => {
            let id = args.get("id").and_then(Value::as_str).ok_or("`id` is required")?;
            let reg = crate::registry();
            if reg.get(id).is_none() {
                return Err(format!("unknown command `{id}`; search with codecraft_commands"));
            }
            let cap = command_capability(id).ok_or_else(|| format!("agents may not run `{id}`"))?;
            let canonical = reg.canonical(id);
            let params = args.get("params").cloned().unwrap_or(Value::Null);
            match (canonical, params.get("sql").and_then(Value::as_str)) {
                ("db.execute" | "db.executeAll", Some(sql)) => {
                    let source = params
                        .get("dataSource")
                        .and_then(Value::as_str)
                        .and_then(|d| find_source(s, d))
                        .unwrap_or_default();
                    // Bound values never change the statement; SQL expressions passed as
                    // `{"sql": …}` values are part of it, so they are judged with it.
                    let sql = expand_sql_params(s, &source, sql, params.get("params"));
                    if sql_writes(s, &source, &sql) { Capability::DbWrite } else { Capability::Read }
                }
                _ => cap,
            }
        }
        t::DB_QUERY => {
            let sql = args.get("sql").and_then(Value::as_str).unwrap_or_default();
            let wanted = args.get("dataSource").and_then(Value::as_str).unwrap_or_default();
            match find_source(s, wanted) {
                Some(src) if sql_writes(s, &src, sql) => Capability::DbWrite,
                Some(_) => Capability::Read,
                // A SQLite file becomes a data source first: that changes the project's settings.
                None if unconfigured_sqlite(s, wanted).is_some() => {
                    if sql_writes_in(codecraft_db::DbKind::Sqlite.dialect(), sql) {
                        Capability::DbWrite
                    } else {
                        Capability::Write
                    }
                }
                None => Capability::Read,
            }
        }
        _ => static_capability(name).ok_or_else(|| format!("unknown tool `{name}`"))?,
    })
}

/// The capability of a tool whose needs do not depend on its arguments (`None` for unknown
/// tools; `codecraft_execute` and `db_query` are decided per call).
pub fn static_capability(name: &str) -> Option<Capability> {
    Some(match name {
        t::INSPECT | t::TEST_RUN | t::RUN_START | t::RUN_STOP => Capability::Run,
        t::DEBUG_START
        | t::DEBUG_BREAKPOINT_SET
        | t::DEBUG_BREAKPOINT_REMOVE
        | t::DEBUG_CONTROL
        | t::DEBUG_EVALUATE => Capability::Debug,
        t::PROFILE_START | t::PROFILE_STOP => Capability::Profile,
        t::FILE_WRITE | t::FILE_EDIT => Capability::Write,
        _ if t::is_engine_tool(name) => Capability::Read,
        _ => return None,
    })
}

// ---------------------------------------------------------------- submit and pump

/// Handle one tool call: check the policy (asking the user when needed), run the tool, and answer
/// through `respond` now or from a later [`pump`].
pub fn submit(s: &mut Session, caller: Caller, name: &str, args: Value, respond: Respond) {
    submit_call(s, caller, None, name, args, respond);
}

/// [`submit`] for a call the transport can cancel later ([`cancel`], [`client_gone`]).
pub fn submit_call(s: &mut Session, caller: Caller, key: Option<CallKey>, name: &str, args: Value, respond: Respond) {
    let args = match args {
        Value::Object(_) => args,
        Value::Null => json!({}),
        _ => return respond(ToolResult::error("the arguments must be a JSON object")),
    };
    // Whether SQL writes depends on the data source's dialect: load the project's data sources
    // first, so a source that is not loaded yet can't make a write look like a read.
    let db_call = name == t::DB_QUERY
        || (name == t::EXECUTE && args.get("id").and_then(Value::as_str).is_some_and(|i| i.starts_with("db.")));
    if db_call {
        let _ = s.execute("db.listDataSources", &Value::Null);
    }
    let mut cap = match required_capability(s, name, &args) {
        Ok(c) => c,
        Err(e) => return respond(ToolResult::error(e)),
    };
    // A Review job's SQL runs on its worktree's data sources, which this session may not know
    // (a SQLite file of the worktree): judge the SQL on its own, strictly.
    if caller.root.is_some() && cap == Capability::Read {
        let sql = match name {
            t::DB_QUERY => args.get("sql"),
            t::EXECUTE => args.pointer("/params/sql"),
            _ => None,
        };
        if sql.and_then(Value::as_str).is_some_and(|q| sql_writes_in(codecraft_db::Dialect::Postgres, q)) {
            cap = Capability::DbWrite;
        }
    }
    let mut job_mode = caller.job.and_then(|j| s.agents.job(j)).map(|j| j.mode);
    // A Review job's writes land in its worktree, and its user reviews them file by file
    // afterwards: like the agent's own edits there, they need no card (Off still denies).
    if caller.root.is_some() && cap == Capability::Write && job_mode == Some(crate::approvals::AgentMode::Review) {
        job_mode = Some(crate::approvals::AgentMode::Auto);
    }
    // A call that acts on an SSH host or Docker container also needs `remote`, which is asked
    // every time (once-only card, Auto jobs too, refused headless).
    let remote = remote_reason(s, name, &args);
    let verdict = match s.agent_access.verdict(&caller.name, job_mode, cap) {
        Verdict::Deny(why) => Verdict::Deny(why),
        base => match &remote {
            None => base,
            Some(_) => s.agent_access.verdict(&caller.name, job_mode, Capability::Remote),
        },
    };
    match verdict {
        Verdict::Deny(why) => {
            s.agent_tools.answered += 1;
            respond(ToolResult::error(why));
        }
        Verdict::Allow => start(s, caller, key, name, args, respond),
        Verdict::Ask => {
            let kind = ApprovalKind::ToolUse { tool: name.to_string(), input: args.clone() };
            let (cap, (approval, rx)) = match &remote {
                Some(on) => {
                    let source = crate::agent_access::approval_source(&caller.name, Capability::Remote);
                    let title = format!("{} wants to use {name} on {on} ({})", caller.name, cap.label().to_lowercase());
                    (Capability::Remote, s.approvals.sink().ask_once(source, title, kind))
                }
                None => {
                    let source = crate::agent_access::approval_source(&caller.name, cap);
                    let title = format!("{} wants to use {name} ({})", caller.name, cap.label().to_lowercase());
                    (cap, s.approvals.sink().ask(source, title, kind))
                }
            };
            s.status = format!("{} asks to use {name}", caller.name);
            s.agent_tools.pending.push(PendingCall {
                caller,
                key,
                name: name.to_string(),
                deadline: Instant::now() + s.agent_tools.approval_timeout,
                respond,
                wait: Wait::Approval { approval, rx, cap, tool: name.to_string(), args },
            });
        }
    }
}

/// The client cancelled a call (`notifications/cancelled`): stop waiting for it, withdraw its
/// approval card, and answer it with an error (the transport sends nothing for a cancelled
/// request). Work it already started (a test run, a debug session) goes on. True if the call was
/// still waiting.
pub fn cancel(s: &mut Session, client: &str, request: &Value) -> bool {
    let pos = s
        .agent_tools
        .pending
        .iter()
        .position(|p| p.key.as_ref().is_some_and(|k| k.client == client && k.request == *request));
    let Some(pos) = pos else { return false };
    let call = s.agent_tools.pending.remove(pos);
    abandon(s, call, "cancelled by the client");
    true
}

/// A client disconnected: every call of it that still waits is dropped and its approval cards
/// are withdrawn at once. Returns how many calls were dropped.
pub fn client_gone(s: &mut Session, client: &str) -> usize {
    let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut s.agent_tools.pending)
        .into_iter()
        .partition(|p| p.key.as_ref().is_some_and(|k| k.client == client));
    s.agent_tools.pending = keep;
    let n = gone.len();
    for call in gone {
        abandon(s, call, "the client disconnected");
    }
    n
}

fn abandon(s: &mut Session, call: PendingCall, why: &str) {
    match &call.wait {
        Wait::Approval { approval, .. } => {
            s.approvals.withdraw(*approval);
        }
        Wait::Worktree { job, forward, .. } => {
            if let Some(ws) = s.agent_tools.worktrees.iter_mut().find(|w| w.job == *job) {
                cancel(&mut ws.session, FORWARD_CLIENT, &json!(forward));
            }
        }
        _ => {}
    }
    s.agent_tools.cancelled += 1;
    if s.status.contains(&call.name) {
        s.status.clear();
    }
    (call.respond)(ToolResult::error(format!("{}: {why}", call.name)));
}

/// Tools that need the project's run configurations, targets or files (QA M14: right after
/// `codecraft-cli mcp` started, `run_configurations` answered `[]` while Cargo metadata loaded).
const NEED_LOADED_PROJECT: &[&str] = &[
    t::PROJECT_INFO,
    t::RUN_CONFIGURATIONS,
    t::DEBUG_START,
    t::PROFILE_START,
    t::TEST_RUN,
    t::RUN_START,
    t::INSPECT,
    t::DB_SOURCES,
    t::DB_QUERY,
];

/// `inspect {scope: "uncommitted"}` and the repository commands (`codecraft_execute
/// vcs.status`) need the repository's status: right after the start they said "the project is
/// not under Git" / "not a Git repository" (QA M15) while the repository was still being found.
fn repo_pending(s: &Session, name: &str, args: &Value) -> bool {
    let wants_repo = match name {
        t::INSPECT => args.get("scope").and_then(Value::as_str) == Some("uncommitted"),
        t::EXECUTE => {
            args.get("id").and_then(Value::as_str).is_some_and(|id| id.starts_with("vcs.") || id.starts_with("git."))
        }
        _ => false,
    };
    wants_repo && s.ws.project.as_ref().is_some_and(|p| s.vcs.settling_for(&p.root))
}

/// QA M15: the playbook's `input.is_empty()` fails in a Rust session — LLDB evaluates C-like
/// expressions over Rust values (fields, indexing, comparisons), not method calls. Say what
/// works instead of leaving the agent with "no member named 'is_empty' in '&str'".
fn rust_eval_hint(s: &Session, error: &str) -> &'static str {
    let method = error.contains("no member named") || error.contains("is not a function");
    if s.debug.rust_session() && method {
        "\nLLDB can't call Rust methods; use fields, indexing and comparisons: `s.length == 0` (&str/String), \
         `v.len` (Vec), `v[0]`, `opt` (shown as Some(…)/None), `x.field`"
    } else {
        ""
    }
}

/// How long a call waits for the project to load.
const PROJECT_LOAD_TIMEOUT: Duration = Duration::from_secs(120);

fn start(s: &mut Session, caller: Caller, key: Option<CallKey>, name: &str, args: Value, respond: Respond) {
    if caller.root.is_some() {
        let step = forward(s, &caller, name, &args);
        return settle_step(s, caller, key, name.to_string(), step, respond);
    }
    if (s.project_loading() && NEED_LOADED_PROJECT.contains(&name)) || (name == t::INSPECT && repo_pending(s, name, &args)) {
        let wait = Wait::ProjectLoad { tool: name.to_string(), args };
        return settle_step(s, caller, key, name.to_string(), Step::Wait(wait, PROJECT_LOAD_TIMEOUT), respond);
    }
    let step = run_tool(s, &caller, name, &args);
    settle_step(s, caller, key, name.to_string(), step, respond);
}

/// Run the tool here, or in the Review job's worktree session.
fn run_or_forward(s: &mut Session, caller: &Caller, name: &str, args: &Value) -> Step {
    if caller.root.is_some() { forward(s, caller, name, args) } else { run_tool(s, caller, name, args) }
}

/// Hand a Review job's call to the session on its worktree (opened on first use). Paths inside
/// the open project are moved into the worktree; writes outside the worktree are refused.
fn forward(s: &mut Session, caller: &Caller, name: &str, args: &Value) -> Step {
    let (Some(job), Some(root)) = (caller.job, caller.root.clone()) else {
        return done_err("this call has no worktree to run in");
    };
    let args = match s.project_root() {
        Some(project) => crate::agent_worktree::remap_paths(args, &project, &root),
        None => args.clone(),
    };
    if matches!(name, t::FILE_WRITE | t::FILE_EDIT)
        && let Some(raw) = args.get("path").and_then(Value::as_str)
    {
        let p = Path::new(raw.trim());
        let abs = crate::normalize_path(&if p.is_absolute() { p.to_path_buf() } else { root.join(p) });
        if !abs.starts_with(&root) {
            return done_err(format!(
                "a Review job writes only inside its worktree ({}); {} is outside it",
                root.display(),
                abs.display()
            ));
        }
    }
    let existing = s.agent_tools.worktrees.iter().position(|w| w.job == job && w.root == root);
    let index = match existing {
        Some(i) => i,
        None => {
            // The job's worktree moved (recreated for a follow-up): the old session goes.
            let (stale, keep): (Vec<_>, Vec<_>) =
                std::mem::take(&mut s.agent_tools.worktrees).into_iter().partition(|w| w.job == job);
            s.agent_tools.worktrees = keep;
            stale.into_iter().for_each(crate::agent_worktree::close);
            match crate::agent_worktree::open(s, job, &root) {
                Ok(ws) => {
                    s.agent_tools.worktrees.push(ws);
                    s.agent_tools.worktrees.len() - 1
                }
                Err(e) => return done_err(e),
            }
        }
    };
    s.agent_tools.next_forward += 1;
    let n = s.agent_tools.next_forward;
    let slot: Arc<Mutex<Option<ToolResult>>> = Arc::new(Mutex::new(None));
    let out = Arc::clone(&slot);
    let Some(ws) = s.agent_tools.worktrees.get_mut(index) else {
        return done_err("the worktree session is gone");
    };
    submit_call(
        &mut ws.session,
        Caller::new(caller.name.clone()),
        Some(CallKey::new(FORWARD_CLIENT, json!(n))),
        name,
        args,
        Box::new(move |r| {
            *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
        }),
    );
    Step::Wait(Wait::Worktree { slot, job, forward: n }, FORWARD_TIMEOUT)
}

/// Answer a finished step, or park it (polled once right away so instant results don't wait a
/// pump).
fn settle_step(s: &mut Session, caller: Caller, key: Option<CallKey>, name: String, step: Step, respond: Respond) {
    match step {
        Step::Done(r) => {
            s.agent_tools.answered += 1;
            respond(r);
        }
        Step::Wait(mut wait, timeout) => match poll(s, &caller, &mut wait) {
            Some(step) => settle_step(s, caller, key, name, step, respond),
            None => {
                let deadline = Instant::now() + timeout;
                s.agent_tools.pending.push(PendingCall { caller, key, name, deadline, respond, wait });
            }
        },
    }
}

/// Pump the Review jobs' worktree sessions; close those whose job stopped running and that have
/// nothing left to answer.
fn pump_worktrees(s: &mut Session) -> bool {
    if s.agent_tools.worktrees.is_empty() {
        return false;
    }
    let mut list = std::mem::take(&mut s.agent_tools.worktrees);
    let mut changed = false;
    for ws in &mut list {
        changed |= ws.session.pump();
    }
    let (keep, done): (Vec<_>, Vec<_>) = list.into_iter().partition(|ws| {
        let running = s.agents.job(ws.job).is_some_and(|j| j.is_running());
        let waiting =
            s.agent_tools.pending.iter().any(|p| matches!(&p.wait, Wait::Worktree { job, .. } if *job == ws.job));
        running || waiting || ws.session.agent_tools.busy()
    });
    // Sessions opened while these were pumped (none today) are kept too.
    let opened = std::mem::replace(&mut s.agent_tools.worktrees, keep);
    s.agent_tools.worktrees.extend(opened);
    for ws in done {
        changed = true;
        crate::agent_worktree::close(ws);
    }
    changed
}

/// Finish parked calls. True if any was answered.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = pump_worktrees(s);
    if s.agent_tools.pending.is_empty() {
        return changed;
    }
    let calls = std::mem::take(&mut s.agent_tools.pending);
    for mut call in calls {
        match poll(s, &call.caller, &mut call.wait) {
            Some(step) => {
                changed = true;
                settle_step(s, call.caller, call.key, call.name, step, call.respond);
            }
            None if Instant::now() >= call.deadline => {
                changed = true;
                let r = on_timeout(s, &call.name, &call.wait);
                s.agent_tools.answered += 1;
                (call.respond)(r);
            }
            None => s.agent_tools.pending.push(call),
        }
    }
    changed
}

/// Answer every parked call (the session is closing) and close the worktree sessions.
pub(crate) fn shutdown(s: &mut Session) {
    for call in std::mem::take(&mut s.agent_tools.pending) {
        if let Wait::Approval { approval, .. } = &call.wait {
            s.approvals.decide(*approval, Decision::Deny);
        }
        (call.respond)(ToolResult::error("CodeCraft is shutting down"));
    }
    for mut ws in std::mem::take(&mut s.agent_tools.worktrees) {
        ws.session.shutdown();
    }
}

fn on_timeout(s: &mut Session, name: &str, wait: &Wait) -> ToolResult {
    match wait {
        Wait::Approval { approval, .. } => {
            s.approvals.decide(*approval, Decision::Deny);
            ToolResult::error(format!(
                "denied: nobody answered the approval prompt within {}s",
                s.agent_tools.approval_timeout.as_secs()
            ))
        }
        Wait::InSession { session, inner } => crate::debug::with_session(s, *session, |s| on_timeout(s, name, inner)),
        Wait::DebugStop { frames, session } => {
            let mut v = match session {
                Some(id) => crate::debug::with_session(s, *id, |s| stop_summary(s, *frames, 1)),
                None => stop_summary(s, *frames, 1),
            };
            if let Some(o) = v.as_object_mut() {
                o.insert("timedOut".into(), json!(true));
                o.insert(
                    "note".into(),
                    json!("still not stopped; call debug_wait_for_stop again, or debug_control pause"),
                );
            }
            json_result(&v)
        }
        Wait::Tests { tab } => {
            let summary = s.execute("test.summary", &json!({ "tab": tab })).unwrap_or(Value::Null);
            ToolResult::error(format!(
                "{name}: the tests are still running (tab {tab}); call test_results later. So far: {summary}"
            ))
        }
        Wait::RunDone { args } => {
            let mut a = args.clone();
            if let Some(o) = a.as_object_mut() {
                o.remove("waitMs");
            }
            match s.execute("run.output", &a) {
                Ok(v) => json_result(&v),
                Err(e) => ToolResult::error(e.to_string()),
            }
        }
        Wait::Pending { id, params } => match s.execute(id, params) {
            Ok(mut v) => {
                if let Some(o) = v.as_object_mut() {
                    o.insert("timedOut".into(), json!(true));
                }
                json_result(&json!({ "id": id, "result": v }))
            }
            Err(e) => ToolResult::error(e.to_string()),
        },
        Wait::ProjectLoad { .. } => ToolResult::error(format!(
            "{name}: the project is still loading (scanning files, reading Cargo metadata); try again in a moment"
        )),
        _ => ToolResult::error(format!("{name}: timed out; the IDE is still working on it, try again later")),
    }
}

/// `Some` once the wait is over.
fn poll(s: &mut Session, caller: &Caller, wait: &mut Wait) -> Option<Step> {
    match wait {
        Wait::ProjectLoad { tool, args } => {
            if s.project_loading() || repo_pending(s, tool, args) {
                return None;
            }
            let (tool, args) = (tool.clone(), args.clone());
            // Judged before the configurations were known: one of them may run remotely.
            if let Some(on) = remote_reason(s, &tool, &args) {
                let job_mode = caller.job.and_then(|j| s.agents.job(j)).map(|j| j.mode);
                if let Verdict::Deny(why) = s.agent_access.verdict(&caller.name, job_mode, Capability::Remote) {
                    return Some(done_err(why));
                }
                let source = crate::agent_access::approval_source(&caller.name, Capability::Remote);
                let title = format!("{} wants to use {tool} on {on}", caller.name);
                let kind = ApprovalKind::ToolUse { tool: tool.clone(), input: args.clone() };
                let (approval, rx) = s.approvals.sink().ask_once(source, title, kind);
                let wait = Wait::Approval { approval, rx, cap: Capability::Remote, tool, args };
                return Some(Step::Wait(wait, s.agent_tools.approval_timeout));
            }
            Some(run_tool(s, caller, &tool, &args))
        }
        Wait::Approval { rx, cap, tool, args, .. } => match rx.try_recv() {
            Ok(Decision::Allow) => Some(run_or_forward(s, caller, tool, args)),
            Ok(Decision::AllowAlways) => {
                s.agent_access.allow_for_session(&caller.name, *cap);
                Some(run_or_forward(s, caller, tool, args))
            }
            Ok(Decision::Deny) | Err(TryRecvError::Disconnected) => Some(done_err("denied by the user")),
            Err(TryRecvError::Empty) => None,
        },
        Wait::InSession { session, inner } => crate::debug::with_session(s, *session, |s| poll(s, caller, inner)),
        Wait::DebugStop { frames, session } => {
            let ready = crate::debug::agent::ready_session(s, *session)?;
            Some(done_json(match ready {
                Some(id) => crate::debug::with_session(s, id, |s| stop_summary(s, *frames, 1)),
                None => stop_summary(s, *frames, 1),
            }))
        }
        Wait::DebugSettle { then } => {
            if !debug_settled(s) {
                return None;
            }
            Some(match then {
                AfterSettle::Stack { limit } => done_json(stack_json(s, *limit)),
                AfterSettle::Variables { limit } => match s.execute("debug.variablesOf", &json!({ "limit": limit })) {
                    Ok(v) => done_json(v),
                    Err(e) => cmd_err(e),
                },
                AfterSettle::Evaluate { expression } => start_evaluate(s, expression),
            })
        }
        Wait::DebugVariables { reference, limit } => {
            let session = s.debug.session.as_ref()?;
            if !session.is_stopped() {
                return Some(done_err("the program is no longer paused"));
            }
            if !session.variables.contains_key(reference) {
                return None;
            }
            Some(match s.execute("debug.variablesOf", &json!({ "reference": reference, "limit": limit })) {
                Ok(v) => done_json(v),
                Err(e) => cmd_err(e),
            })
        }
        Wait::DebugEval { expression } => {
            if !s.debug.is_stopped() {
                return Some(done_err("the program is no longer paused"));
            }
            let ev = s.debug.evaluation.as_ref()?;
            if ev.pending || ev.expression != *expression {
                return None;
            }
            Some(match &ev.result {
                Ok(v) => {
                    let mut o = Map::new();
                    o.insert("expression".into(), json!(expression));
                    o.insert("value".into(), json!(crate::agent_tools_cmds::clip(&v.value, 4000)));
                    if let Some(ty) = &v.type_name {
                        o.insert("type".into(), json!(ty));
                    }
                    if v.reference > 0 {
                        o.insert("ref".into(), json!(v.reference));
                    }
                    done_json(Value::Object(o))
                }
                Err(e) => done_err(format!("cannot evaluate `{expression}`: {e}{}", rust_eval_hint(s, e))),
            })
        }
        Wait::LspReady { server, kind, args, limit, then } => {
            if s.ready_client(*server).is_some() {
                return Some(send_lsp(s, *kind, args, *limit, *then));
            }
            match s.server_status(*server) {
                LspStatus::Unavailable(m) | LspStatus::Stopped(m) => Some(done_err(m)),
                _ => None,
            }
        }
        Wait::Lsp { token, limit, then } => {
            if s.agent_tools.lsp_pending(*token) {
                return None;
            }
            let res = s.execute("lsp.queryResult", &json!({ "token": token, "limit": limit }));
            let v = match res {
                Ok(v) => v,
                Err(e) => return Some(cmd_err(e)),
            };
            if v["done"] != json!(true) {
                // The server went away without answering.
                return Some(done_err("the language server did not answer"));
            }
            if let Some(e) = v.get("error").and_then(Value::as_str) {
                return Some(done_err(format!("the language server failed: {e}")));
            }
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            match then.take() {
                Some(next) => {
                    let Some(item) = result.pointer("/items/0").cloned() else {
                        return Some(done_json(json!({ "calls": [], "note": "no function at this position" })));
                    };
                    let kind = if next == LspKind::IncomingCalls { "incomingCalls" } else { "outgoingCalls" };
                    match s.execute("lsp.query", &json!({ "kind": kind, "item": item })) {
                        Ok(r) => {
                            let token = r.get("token").and_then(Value::as_u64).unwrap_or(0);
                            *wait = Wait::Lsp { token, limit: *limit, then: None };
                            None
                        }
                        Err(e) => Some(cmd_err(e)),
                    }
                }
                None => Some(done_json(result)),
            }
        }
        Wait::Inspection { limit } => {
            if s.inspections.running.is_some() {
                return None;
            }
            Some(inspection_results(s, *limit))
        }
        Wait::Tests { tab } => {
            let id = codecraft_workspace::RunTabId(*tab);
            let finished = s.testing.run(id).map(|r| r.finished);
            match finished {
                Some(true) => Some(match s.execute("test.summary", &json!({ "tab": tab })) {
                    Ok(v) => done_json(v),
                    Err(e) => cmd_err(e),
                }),
                Some(false) => None,
                // Not a test run: answer with its output once it ends.
                None if !s.runs.is_running(id) => {
                    Some(match s.execute("run.output", &json!({ "tab": tab, "tail": 60 })) {
                        Ok(v) => done_json(json!({ "note": "the command produced no test results", "run": v })),
                        Err(e) => cmd_err(e),
                    })
                }
                None => None,
            }
        }
        Wait::Profile { limit, reopened, sort } => {
            use crate::profiler::JobState;
            // A profile still building its target (Cargo, CMake), recording or loading (QA M14:
            // profile_hotspots right after profile_start of a Cargo target said "no snapshot").
            let busy = !s.profiler.recordings.is_empty()
                || s.profiler.loading.is_some()
                || s.profiler
                    .jobs
                    .iter()
                    .any(|j| matches!(j.state, JobState::Building | JobState::Recording | JobState::Loading));
            if busy {
                return None;
            }
            // The newest profile failed (the build broke, the profiler is missing…): say why
            // instead of answering from an older snapshot.
            if let Some(j) = s.profiler.jobs.iter().max_by_key(|j| j.id).filter(|j| j.state == JobState::Failed) {
                let why = j.error.clone().unwrap_or_else(|| "the profiler stopped without a snapshot".into());
                let mut msg = format!("profiling `{}` failed: {why}", j.title);
                if let Some(tab) = j.tab {
                    msg.push_str(&format!(" (the output: run_output {{\"tab\": {}}})", tab.0));
                }
                return Some(done_err(msg));
            }
            // Profiles that ended out of order leave an older snapshot open: answer for the
            // newest one.
            let newest = s.profiler.jobs.iter().max_by_key(|j| j.id).and_then(|j| j.snapshot.clone());
            let shown = s.profiler.snapshot.as_ref().map(|snap| snap.path.clone());
            if !*reopened
                && let Some(path) = newest
                && shown.as_ref() != Some(&path)
            {
                *reopened = true;
                if s.execute("run.openProfilerSnapshot", &json!({ "path": path })).is_ok() {
                    return None;
                }
            }
            let mut params = json!({ "limit": limit });
            let mut report_params = json!({ "top": limit });
            if let Some(sort) = sort.as_deref() {
                params["sort"] = json!(sort);
                report_params["sort"] = json!(sort);
            }
            Some(match s.execute("profiler.hotspots", &params) {
                Ok(mut v) => {
                    // The full report adds the hottest lines with their code, a total-time
                    // ranking and notes (missing symbols, debug builds); skipped while it loads.
                    if let Ok(report) = s.execute("profiler.report", &report_params)
                        && report.get("pending").is_none()
                        && let Some(o) = v.as_object_mut()
                    {
                        o.insert("report".into(), report);
                    }
                    done_json(v)
                }
                Err(e) => cmd_err(e),
            })
        }
        Wait::Pending { id, params } => match s.execute(id, params) {
            Ok(v) if v.get("pending").and_then(Value::as_bool) == Some(true) => None,
            Ok(v) => Some(done_json(json!({ "id": id, "result": v }))),
            Err(e) => Some(cmd_err(e)),
        },
        Wait::RunDone { args } => {
            let tab = args.get("tab").and_then(Value::as_u64).map(codecraft_workspace::RunTabId);
            let running = match tab {
                Some(t) => s.runs.is_running(t),
                None => s.ws.runs.last().is_some_and(|r| s.runs.is_running(r.id)),
            };
            if running {
                return None;
            }
            let mut a = args.clone();
            if let Some(o) = a.as_object_mut() {
                o.remove("waitMs");
            }
            Some(match s.execute("run.output", &a) {
                Ok(v) => done_json(v),
                Err(e) => cmd_err(e),
            })
        }
        Wait::Db { source, before, output_before, limit, added } => {
            if s.db.confirm.is_some() || s.db.running() {
                return None;
            }
            Some(db_outcome(s, source, before, *output_before, *limit, added.take()))
        }
        Wait::Worktree { slot, .. } => slot.lock().unwrap_or_else(PoisonError::into_inner).take().map(Step::Done),
    }
}

// ---------------------------------------------------------------- tools

fn exec(s: &mut Session, id: &str, params: Value) -> Result<Value, Step> {
    s.execute(id, &params).map_err(cmd_err)
}

fn exec_done(s: &mut Session, id: &str, params: Value) -> Step {
    match s.execute(id, &params) {
        Ok(v) => done_json(v),
        Err(e) => cmd_err(e),
    }
}

/// A run configuration from `config` (index or name).
fn config_index(s: &Session, config: &Value) -> Result<usize, String> {
    let names = || s.run_configs.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join(", ");
    match config {
        Value::Number(n) => {
            let i = n.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or("`config` must be a non-negative index")?;
            if i < s.run_configs.len() {
                Ok(i)
            } else {
                Err(format!("no run configuration {i}; there are {}: {}", s.run_configs.len(), names()))
            }
        }
        Value::String(name) => s
            .run_configs
            .iter()
            .position(|c| c.name == *name)
            .or_else(|| s.run_configs.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
            .ok_or_else(|| {
                if s.run_configs.is_empty() {
                    format!(
                        "no run configuration named `{name}`: the project has none (debug_start takes `file` or \
                         `program`, profile_start `command`)"
                    )
                } else {
                    format!("no run configuration named `{name}`; have: {}", names())
                }
            }),
        _ => Err("`config` must be a configuration name or index".into()),
    }
}

/// `{path, line}` (1-based) as `{path, line}` (absolute, 0-based) for commands.
fn zero_based_location(s: &Session, args: &Value) -> Result<(std::path::PathBuf, Option<usize>), Step> {
    let path = path_param(s, args, "path").map_err(cmd_err)?;
    let line = crate::agent_tools_cmds::line_param(args, "line").map_err(cmd_err)?;
    Ok((path, line))
}

fn run_tool(s: &mut Session, caller: &Caller, name: &str, args: &Value) -> Step {
    let _ = caller;
    // Debug tools act on the debug session their `session` argument names (M15 track DM).
    if name.starts_with("debug_")
        && name != t::DEBUG_START
        && let Some(v) = args.get("session").filter(|v| !v.is_null())
    {
        let id = match crate::debug::resolve_session(&s.debug, v) {
            Ok(id) => id,
            Err(e) => return cmd_err(e),
        };
        let mut inner = args.clone();
        if let Some(o) = inner.as_object_mut() {
            o.remove("session");
        }
        return match crate::debug::with_session(s, id, |s| run_tool(s, caller, name, &inner)) {
            Step::Wait(Wait::DebugStop { frames, .. }, t) => Step::Wait(Wait::DebugStop { frames, session: Some(id) }, t),
            Step::Wait(w, t) => Step::Wait(Wait::InSession { session: id, inner: Box::new(w) }, t),
            done => done,
        };
    }
    match name {
        t::COMMANDS => exec_done(s, "commands.search", args.clone()),
        t::EXECUTE => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let params = args.get("params").cloned().unwrap_or(Value::Null);
            // `waitMs` on a command that reports `pending` (analysis.fileFindings, M16 LT): wait
            // here, polling with the IDE's pumps, instead of blocking the command.
            let wait_ms = params.get("waitMs").and_then(Value::as_u64).filter(|ms| *ms > 0 && id == "analysis.fileFindings");
            match s.execute(&id, &params) {
                Ok(v) if wait_ms.is_some() && v.get("pending").and_then(Value::as_bool) == Some(true) => {
                    let mut params = params.clone();
                    if let Some(o) = params.as_object_mut() {
                        o.remove("waitMs");
                    }
                    let ms = wait_ms.unwrap_or(0).min(600_000);
                    Step::Wait(Wait::Pending { id, params }, Duration::from_millis(ms))
                }
                Ok(v) => done_json(json!({ "id": id, "result": v })),
                // A command that needs the project's settings (run configurations, data
                // sources) while the project is still loading: run it again once it has (M15 AI:
                // `runConfig.create` right after `codecraft-cli mcp` started said "no project is
                // open").
                Err(e)
                    if (s.project_loading()
                        && (matches!(e, CommandError::Disabled(_)) || e.to_string().contains("no project is open")))
                        || (matches!(e, CommandError::Disabled(_)) && repo_pending(s, name, args)) =>
                {
                    let wait = Wait::ProjectLoad { tool: name.to_string(), args: args.clone() };
                    Step::Wait(wait, PROJECT_LOAD_TIMEOUT)
                }
                Err(e) => cmd_err(e),
            }
        }
        t::PROJECT_INFO => exec_done(s, "project.info", Value::Null),
        t::FILE_READ => exec_done(s, "files.read", args.clone()),
        t::FILE_WRITE => exec_done(s, "files.write", args.clone()),
        t::FILE_EDIT => exec_done(s, "files.edit", args.clone()),
        t::EDITOR_OPEN => {
            let (path, line) = match zero_based_location(s, args) {
                Ok(x) => x,
                Err(step) => return step,
            };
            let column = crate::agent_tools_cmds::line_param(args, "column").ok().flatten().unwrap_or(0);
            match s
                .execute("navigate.openLocation", &json!({ "path": path, "line": line.unwrap_or(0), "column": column }))
            {
                Ok(_) => done_json(json!({ "opened": path, "line": line.unwrap_or(0) + 1 })),
                Err(e) => cmd_err(e),
            }
        }
        t::PROBLEMS => exec_done(s, "analysis.problems", args.clone()),
        t::INSPECT => start_inspection(s, args),
        t::DEFINITION => start_lsp(s, LspKind::Definition, args, None),
        t::REFERENCES => start_lsp(s, LspKind::References, args, None),
        t::HOVER => start_lsp(s, LspKind::Hover, args, None),
        t::DOCUMENT_SYMBOLS => start_lsp(s, LspKind::DocumentSymbols, args, None),
        t::WORKSPACE_SYMBOLS => start_lsp(s, LspKind::WorkspaceSymbols, args, None),
        t::CALL_HIERARCHY => {
            let then = if args.get("direction").and_then(Value::as_str) == Some("outgoing") {
                LspKind::OutgoingCalls
            } else {
                LspKind::IncomingCalls
            };
            start_lsp(s, LspKind::PrepareCallHierarchy, args, Some(then))
        }
        t::RUN_CONFIGURATIONS => exec_done(s, "run.configurations", Value::Null),
        t::DEBUG_START => debug_start(s, args),
        t::DEBUG_BREAKPOINT_SET => breakpoint_set(s, args),
        t::DEBUG_BREAKPOINT_REMOVE => breakpoint_remove(s, args),
        t::DEBUG_BREAKPOINTS => done_json(breakpoints_json(s)),
        t::DEBUG_CONTROL => debug_control(s, args),
        t::DEBUG_WAIT_FOR_STOP => {
            let frames = usize_arg(args, "frames", 5, 50);
            // Any session's stop (with one session, that one's).
            Step::Wait(Wait::DebugStop { frames, session: None }, timeout_arg(args, 30_000))
        }
        t::DEBUG_STACK => debug_stack(s, args),
        t::DEBUG_VARIABLES => debug_variables(s, args),
        t::DEBUG_EVALUATE => debug_evaluate(s, args),
        t::PROFILE_START => {
            let mut p = json!({});
            let command = args.get("command").filter(|c| !c.is_null());
            if let Some(c) = args.get("config") {
                match config_index(s, c) {
                    Ok(i) => p = json!({ "config": i }),
                    Err(e) => return done_err(e),
                }
            } else if let Some(c) = command {
                p = json!({ "command": c });
            } else if let Some(t) = args.get("test").filter(|t| !t.is_null()) {
                // The test at a source line (1-based), like the gutter's ▶ (M15 PP).
                p = json!({ "test": t });
            } else if s.run_configs.is_empty() {
                return done_err(
                    "the project has no run configurations; pass `command` (e.g. \"python3 main.py\", \"cargo run --release\", \"./build/app\")",
                );
            }
            if let Some(o) = p.as_object_mut() {
                for key in ["kind", "tool", "seconds", "interval", "target"] {
                    if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
                        o.insert(key.into(), v.clone());
                    }
                }
            }
            let by_record = args.get("config").is_none() && (command.is_some() || p.get("test").is_some());
            let id = if by_record { "profiler.record" } else { "run.profile" };
            match s.execute(id, &p) {
                Ok(mut v) => {
                    if let Some(o) = v.as_object_mut() {
                        o.insert(
                            "next".into(),
                            json!("call profile_stop to stop recording, or wait for the program to end and call profile_hotspots"),
                        );
                    }
                    done_json(v)
                }
                Err(e) => cmd_err(e),
            }
        }
        t::PROFILE_STOP => {
            let limit = usize_arg(args, "limit", 20, 200);
            if !s.profiler.recordings.is_empty()
                && let Err(step) = exec(s, "profiler.stop", Value::Null)
            {
                return step;
            }
            Step::Wait(Wait::Profile { limit, reopened: false, sort: sort_arg(args) }, timeout_arg(args, 120_000))
        }
        t::PROFILE_HOTSPOTS => {
            let limit = usize_arg(args, "limit", 20, 200);
            Step::Wait(Wait::Profile { limit, reopened: false, sort: sort_arg(args) }, timeout_arg(args, 120_000))
        }
        t::TEST_RUN => test_run(s, args),
        t::TEST_RESULTS => exec_done(s, "test.summary", args.clone()),
        t::RUN_START => {
            let Some(c) = args.get("config") else { return done_err("`config` is required") };
            let i = match config_index(s, c) {
                Ok(i) => i,
                Err(e) => return done_err(e),
            };
            match s.execute("run.run", &with_target(args, json!({ "config": i }))) {
                Ok(mut v) => {
                    if let Some(o) = v.as_object_mut() {
                        o.insert("next".into(), json!("read the output with run_output (waitMs waits for the end)"));
                    }
                    done_json(v)
                }
                Err(e) => cmd_err(e),
            }
        }
        t::RUN_OUTPUT => match args.get("waitMs").and_then(Value::as_u64) {
            Some(ms) if ms > 0 => {
                Step::Wait(Wait::RunDone { args: args.clone() }, Duration::from_millis(ms.min(600_000)))
            }
            _ => exec_done(s, "run.output", args.clone()),
        },
        t::RUN_STOP => exec_done(s, "run.stopTab", args.clone()),
        t::DB_SOURCES => match s.execute("db.listDataSources", &Value::Null) {
            Ok(v) => {
                let mut list = match v.get("dataSources") {
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
                // SQLite files no data source points at yet: db_query takes their path.
                for path in sqlite_candidates(s, 20) {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    list.push(json!({
                        "id": null, "name": name, "kind": "sqlite", "path": path, "status": "not added",
                        "hint": "a SQLite file in the project: pass its path as db_query's `dataSource` (adds it as a data source)",
                    }));
                }
                done_json(Value::Array(list))
            }
            Err(e) => cmd_err(e),
        },
        t::DB_QUERY => db_query(s, args),
        other => done_err(format!("unknown tool `{other}`")),
    }
}

// ----- analysis

fn start_inspection(s: &mut Session, args: &Value) -> Step {
    let scope = args.get("scope").and_then(Value::as_str).unwrap_or("project");
    let mut p = json!({ "scope": scope });
    if let Some(o) = p.as_object_mut() {
        if args.get("path").is_some() {
            match path_param(s, args, "path") {
                Ok(path) => {
                    o.insert("path".into(), json!(path));
                }
                Err(e) => return cmd_err(e),
            }
        } else if matches!(scope, "file" | "directory") {
            return done_err(format!("scope `{scope}` needs `path`"));
        }
        if let Some(tools) = args.get("tools") {
            o.insert("tools".into(), tools.clone());
        }
    }
    // A glob scope or a named inspection profile goes through the analysis jobs (same runner).
    let profile = args.get("profile").and_then(Value::as_str);
    if scope == "glob" || profile.is_some() {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str).or((scope != "glob").then_some("")) else {
            return done_err("scope `glob` needs `pattern` (e.g. \"src/**/*.rs, !**/gen/**\")");
        };
        let mut q = json!({ "scope": scope, "show": true });
        if let Some(o) = q.as_object_mut() {
            for key in ["path", "analyzers"] {
                if let Some(x) = p.get(key).or_else(|| args.get(key)) {
                    o.insert(key.into(), x.clone());
                }
            }
            if !pattern.is_empty() {
                o.insert("pattern".into(), json!(pattern));
            }
            if let Some(name) = profile {
                o.insert("profile".into(), json!(name));
            }
        }
        if let Err(step) = exec(s, "analysis.run", q) {
            return step;
        }
    } else if let Err(step) = exec(s, "code.inspectCode", p) {
        return step;
    }
    Step::Wait(Wait::Inspection { limit: usize_arg(args, "limit", 200, 2000) }, timeout_arg(args, 300_000))
}

fn inspection_results(s: &mut Session, limit: usize) -> Step {
    let v = match s.execute("inspections.results", &Value::Null) {
        Ok(v) => v,
        Err(e) => return cmd_err(e),
    };
    if v.get("results").is_some_and(Value::is_null) && v.get("findings").is_none() {
        return done_json(
            json!({ "findings": [], "note": "the inspection produced no results (cancelled, or no analyzer applies)" }),
        );
    }
    let findings = v.get("findings").and_then(Value::as_array).cloned().unwrap_or_default();
    let total = findings.len();
    let list: Vec<Value> = findings
        .iter()
        .take(limit)
        .map(|f| {
            let line = f.pointer("/start/line").and_then(Value::as_u64).map(|l| l + 1);
            let column = f.pointer("/start/column").and_then(Value::as_u64).map(|c| c + 1);
            let mut o = Map::new();
            for (k, src) in [
                ("id", "id"),
                ("severity", "severity"),
                ("inspection", "inspection"),
                ("tool", "tool"),
                ("code", "code"),
            ] {
                if let Some(x) = f.get(src).filter(|x| !x.is_null()) {
                    o.insert(k.into(), x.clone());
                }
            }
            o.insert("file".into(), f.get("path").cloned().unwrap_or(Value::Null));
            o.insert("line".into(), json!(line));
            o.insert("column".into(), json!(column));
            let msg = f.get("message").and_then(Value::as_str).unwrap_or_default();
            o.insert("message".into(), json!(crate::agent_tools_cmds::clip(msg, 1000)));
            let fixes = f.get("fixes").and_then(Value::as_array).map_or(0, Vec::len);
            if fixes > 0 {
                o.insert("fixes".into(), json!(fixes));
            }
            Value::Object(o)
        })
        .collect();
    // The analyzers that ran, in the camelCase of every other result (QA M14 #24: `command_line`),
    // without CodeCraft's temporary report files.
    let tools: Vec<Value> = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|t| {
                    let mut o = Map::new();
                    for key in ["tool", "ok", "outcome"] {
                        if let Some(x) = t.get(key) {
                            o.insert(key.into(), x.clone());
                        }
                    }
                    let cl = t.get("command_line").or_else(|| t.get("commandLine")).and_then(Value::as_str);
                    if let Some(cl) = cl.filter(|c| !c.is_empty()) {
                        o.insert("commandLine".into(), json!(crate::agent_tools_cmds::public_command_line(cl)));
                    }
                    Value::Object(o)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = json!({
        "scope": v.get("scopeLabel").or_else(|| v.get("scope")).cloned().unwrap_or(Value::Null),
        "tools": tools,
        "counts": v.get("counts").cloned().unwrap_or(Value::Null),
        "total": total,
        "findings": list,
    });
    if total > limit
        && let Some(o) = out.as_object_mut()
    {
        o.insert("truncated".into(), json!(format!("{} more; raise `limit` or narrow the scope", total - limit)));
    }
    done_json(out)
}

/// Open `path` in a background tab (keeping the user's editor and focus), so servers that only
/// answer for open documents know it.
fn open_in_background(s: &mut Session, path: &Path) {
    if s.ws.doc_by_path(path).is_some() {
        return;
    }
    let prev = s.ws.active_editor();
    let before = s.effects.len();
    if s.open_file(path).is_err() {
        return;
    }
    if let Some(prev) = prev.filter(|p| s.ws.editor(*p).is_some()) {
        s.ws.set_active_editor(prev);
    }
    let tail = s.effects.split_off(before.min(s.effects.len()));
    s.effects.extend(tail.into_iter().filter(|e| *e != Effect::FocusEditor));
}

fn start_lsp(s: &mut Session, kind: LspKind, args: &Value, then: Option<LspKind>) -> Step {
    if !s.lsp_enabled {
        return done_err("language servers are off in this session");
    }
    let limit = usize_arg(args, "limit", 200, 2000);
    let path = if kind == LspKind::WorkspaceSymbols {
        None
    } else {
        match path_param(s, args, "path") {
            Ok(p) => Some(p),
            Err(e) => return cmd_err(e),
        }
    };
    if let Some(p) = &path {
        if !p.is_file() && s.ws.doc_by_path(p).is_none() {
            return done_err(format!("{} does not exist", p.display()));
        }
        // rust-analyzer reads the disk; other servers want the document open.
        if codecraft_syntax::Language::from_path(p) != codecraft_syntax::Language::Rust {
            open_in_background(s, p);
        }
    }
    let server = match path.as_deref() {
        Some(p) => match server_for(s, Some(p)) {
            Ok(k) => k,
            Err(e) => return cmd_err(e),
        },
        None => match server_for(s, None) {
            Ok(k) => k,
            Err(_) => ServerKind::RustAnalyzer,
        },
    };
    s.ensure_server(server);
    let mut a = args.clone();
    if let (Some(p), Some(o)) = (&path, a.as_object_mut()) {
        o.insert("path".into(), json!(p));
    }
    Step::Wait(Wait::LspReady { server, kind, args: a, limit, then }, timeout_arg(args, 30_000))
}

fn send_lsp(s: &mut Session, kind: LspKind, args: &Value, limit: usize, then: Option<LspKind>) -> Step {
    let kind_id = match kind {
        LspKind::Definition => "definition",
        LspKind::References => "references",
        LspKind::Hover => "hover",
        LspKind::DocumentSymbols => "documentSymbols",
        LspKind::WorkspaceSymbols => "workspaceSymbols",
        LspKind::PrepareCallHierarchy => "prepareCallHierarchy",
        LspKind::IncomingCalls => "incomingCalls",
        LspKind::OutgoingCalls => "outgoingCalls",
    };
    let mut p = args.clone();
    if let Some(o) = p.as_object_mut() {
        o.insert("kind".into(), json!(kind_id));
        o.remove("timeoutMs");
        o.remove("limit");
        o.remove("direction");
    }
    match s.execute("lsp.query", &p) {
        Ok(v) => {
            let token = v.get("token").and_then(Value::as_u64).unwrap_or(0);
            Step::Wait(Wait::Lsp { token, limit, then }, Duration::from_secs(60))
        }
        Err(e) => cmd_err(e),
    }
}

// ----- debugging

/// The debugger needs nothing more to answer: paused with everything loaded, ended, or absent.
fn debug_settled(s: &Session) -> bool {
    match &s.debug.session {
        None => true,
        Some(x) => match x.status() {
            DebugStatus::Stopped { .. } => !s.debug.settling(),
            DebugStatus::Exited { .. } | DebugStatus::Failed { .. } => true,
            DebugStatus::Building | DebugStatus::Starting | DebugStatus::Running => false,
        },
    }
}

/// `p` with the tool's `target` (M16 track RT: run there instead of the configuration's own
/// target).
fn with_target(args: &Value, mut p: Value) -> Value {
    if let (Some(t), Some(o)) = (args.get("target").filter(|t| !t.is_null()), p.as_object_mut()) {
        o.insert("target".into(), t.clone());
    }
    p
}

fn debug_start(s: &mut Session, args: &Value) -> Step {
    let started = if let Some(c) = args.get("config") {
        match config_index(s, c) {
            Ok(i) => s.execute("run.debug", &with_target(args, json!({ "config": i }))),
            Err(e) => return done_err(e),
        }
    } else if args.get("pid").is_some() {
        let mut p = json!({ "pid": args.get("pid") });
        if let (Some(kind), Some(o)) = (args.get("kind"), p.as_object_mut()) {
            o.insert("kind".into(), kind.clone());
        }
        s.execute("run.attachToProcess", &p)
    } else if args.get("program").is_some() {
        let program = match path_param(s, args, "program") {
            Ok(p) => p,
            Err(e) => return cmd_err(e),
        };
        let mut p = json!({ "program": program });
        if let (Some(a), Some(o)) = (args.get("args"), p.as_object_mut()) {
            o.insert("args".into(), a.clone());
        }
        s.execute("debug.executable", &with_target(args, p))
    } else if args.get("file").is_some() {
        let path = match path_param(s, args, "file") {
            Ok(p) => p,
            Err(e) => return cmd_err(e),
        };
        let mut p = json!({ "path": path });
        match crate::agent_tools_cmds::line_param(args, "line") {
            Ok(Some(l)) => {
                if let Some(o) = p.as_object_mut() {
                    o.insert("line".into(), json!(l));
                }
            }
            Ok(None) => {}
            Err(e) => return cmd_err(e),
        }
        s.execute("debug.atCaret", &with_target(args, p))
    } else {
        s.execute("run.debug", &with_target(args, json!({})))
    };
    if let Err(e) = started {
        return cmd_err(e);
    }
    if s.debug.session.is_none() {
        return done_err(if s.status.is_empty() { "nothing to debug here".to_string() } else { s.status.clone() });
    }
    let mut v = stop_summary(s, 1, 0);
    if let Some(o) = v.as_object_mut() {
        o.insert("started".into(), json!(true));
        o.insert("breakpoints".into(), json!(s.debug.breakpoints.len()));
        o.insert("next".into(), json!("call debug_wait_for_stop"));
    }
    done_json(v)
}

fn breakpoint_json(b: &crate::debug::Breakpoint) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), json!(b.id));
    o.insert("file".into(), json!(b.path));
    o.insert("line".into(), json!(b.line + 1));
    o.insert("enabled".into(), json!(b.enabled));
    for (k, v) in [
        ("condition", &b.condition),
        ("hitCount", &b.hit_condition),
        ("logMessage", &b.log_message),
        ("message", &b.message),
    ] {
        if let Some(v) = v {
            o.insert(k.into(), json!(v));
        }
    }
    if b.temporary {
        o.insert("temporary".into(), json!(true));
    }
    if let Some(v) = b.verified {
        o.insert("verified".into(), json!(v));
    }
    Value::Object(o)
}

fn breakpoints_json(s: &Session) -> Value {
    json!(s.debug.breakpoints.iter().map(breakpoint_json).collect::<Vec<_>>())
}

fn breakpoint_set(s: &mut Session, args: &Value) -> Step {
    let (path, line) = match zero_based_location(s, args) {
        Ok(x) => x,
        Err(step) => return step,
    };
    let Some(line) = line else { return done_err("`line` is required") };
    let mut p = json!({ "path": path, "line": line });
    if let Some(o) = p.as_object_mut() {
        for (from, to) in [
            ("condition", "condition"),
            ("hitCount", "hitCondition"),
            ("logMessage", "logMessage"),
            ("temporary", "temporary"),
            ("enabled", "enabled"),
        ] {
            if let Some(v) = args.get(from).filter(|v| !v.is_null()) {
                o.insert(to.into(), v.clone());
            }
        }
    }
    if let Err(step) = exec(s, "debug.setBreakpoint", p) {
        return step;
    }
    match s.debug.breakpoints.iter().find(|b| b.path == path && b.line == line) {
        Some(b) => done_json(breakpoint_json(b)),
        None => done_json(json!({ "file": path, "line": line + 1, "note": "the breakpoint moved or was not kept" })),
    }
}

fn breakpoint_remove(s: &mut Session, args: &Value) -> Step {
    if args.get("all").and_then(Value::as_bool) == Some(true) {
        return exec_done(s, "debug.removeAllBreakpoints", Value::Null);
    }
    if let Some(id) = args.get("id").and_then(Value::as_u64) {
        return exec_done(s, "debug.removeBreakpoint", json!({ "id": id }));
    }
    let (path, line) = match zero_based_location(s, args) {
        Ok(x) => x,
        Err(step) => return step,
    };
    let Some(line) = line else { return done_err("give `id`, `path`+`line`, or `all`") };
    exec_done(s, "debug.removeBreakpoint", json!({ "path": path, "line": line }))
}

fn debug_control(s: &mut Session, args: &Value) -> Step {
    let action = args.get("action").and_then(Value::as_str).unwrap_or_default();
    let (id, params, wait_default) = match action {
        "continue" => ("run.debuggingActions.resumeProgram", Value::Null, true),
        "stepOver" => ("run.debuggingActions.stepOver", Value::Null, true),
        "stepInto" => ("run.debuggingActions.stepInto", Value::Null, true),
        "stepOut" => ("run.debuggingActions.stepOut", Value::Null, true),
        "pause" => ("debug.pause", Value::Null, true),
        "stop" => ("debug.stop", Value::Null, false),
        "runToLine" => {
            let (path, line) = match zero_based_location(s, args) {
                Ok(x) => x,
                Err(step) => return step,
            };
            let Some(line) = line else { return done_err("runToLine needs `line`") };
            ("run.debuggingActions.runToCursor", json!({ "path": path, "line": line }), true)
        }
        other => {
            return done_err(format!(
                "unknown action `{other}` (continue, stepOver, stepInto, stepOut, pause, stop, runToLine)"
            ));
        }
    };
    if let Err(step) = exec(s, id, params) {
        return step;
    }
    let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(wait_default);
    let frames = usize_arg(args, "frames", 5, 50);
    if wait {
        // The session acted on (or one of its subprocess sessions).
        let session = s.debug.selected_id();
        Step::Wait(Wait::DebugStop { frames, session }, timeout_arg(args, 30_000))
    } else {
        done_json(stop_summary(s, frames, 1))
    }
}

fn stack_json(s: &Session, limit: usize) -> Value {
    let Some(session) = &s.debug.session else { return json!({ "state": "none" }) };
    let threads: Vec<Value> = session
        .threads
        .iter()
        .map(|t| json!({ "id": t.id, "name": t.name, "current": Some(t.id) == session.thread }))
        .collect();
    let frames: Vec<Value> = session
        .frames
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, f)| {
            let mut o = Map::new();
            o.insert("index".into(), json!(i));
            o.insert("name".into(), json!(crate::agent_tools_cmds::short_frame_name(&f.name)));
            if let Some(p) = &f.path {
                o.insert("file".into(), json!(p));
                o.insert("line".into(), json!(f.line + 1));
            }
            if f.subtle {
                o.insert("library".into(), json!(true));
            }
            Value::Object(o)
        })
        .collect();
    let mut v =
        json!({ "thread": session.thread, "threads": threads, "frames": frames, "selectedFrame": session.frame });
    if session.frames.len() > limit
        && let Some(o) = v.as_object_mut()
    {
        o.insert("moreFrames".into(), json!(session.frames.len() - limit));
    }
    v
}

fn debug_stack(s: &mut Session, args: &Value) -> Step {
    let limit = usize_arg(args, "limit", 30, 200);
    if !s.debug.is_stopped() {
        return done_json(stop_summary(s, 1, 0));
    }
    let current = s.debug.session.as_ref().and_then(|x| x.thread);
    if let Some(tid) = args.get("threadId").and_then(Value::as_i64)
        && Some(tid) != current
        && let Err(step) = exec(s, "debug.selectThread", json!({ "id": tid }))
    {
        return step;
    }
    Step::Wait(Wait::DebugSettle { then: AfterSettle::Stack { limit } }, timeout_arg(args, 10_000))
}

/// Select frame `index` when it is not the selected one; `Ok(true)` if a selection happened.
fn select_frame(s: &mut Session, args: &Value) -> Result<bool, Step> {
    let Some(index) = args.get("frame").and_then(Value::as_u64) else { return Ok(false) };
    let current = s.debug.session.as_ref().map_or(0, |x| x.frame);
    if usize::try_from(index).ok() == Some(current) {
        return Ok(false);
    }
    exec(s, "debug.selectFrame", json!({ "index": index }))?;
    Ok(true)
}

fn debug_variables(s: &mut Session, args: &Value) -> Step {
    let limit = usize_arg(args, "limit", 100, 1000);
    if !s.debug.is_stopped() {
        return done_err("the program is not paused (debug_wait_for_stop first)");
    }
    if let Some(reference) = args.get("ref").and_then(Value::as_i64) {
        let loaded = s.debug.session.as_ref().is_some_and(|x| x.variables.contains_key(&reference));
        if !loaded
            && let Err(step) =
                exec(s, "debug.expandVariable", json!({ "path": format!("agent/{reference}"), "reference": reference }))
        {
            return step;
        }
        return Step::Wait(Wait::DebugVariables { reference, limit }, timeout_arg(args, 10_000));
    }
    if let Err(step) = select_frame(s, args) {
        return step;
    }
    Step::Wait(Wait::DebugSettle { then: AfterSettle::Variables { limit } }, timeout_arg(args, 10_000))
}

fn start_evaluate(s: &mut Session, expression: &str) -> Step {
    if let Err(step) = exec(s, "run.debuggingActions.evaluateExpression", json!({ "expression": expression })) {
        return step;
    }
    Step::Wait(Wait::DebugEval { expression: expression.trim().to_string() }, Duration::from_secs(10))
}

fn debug_evaluate(s: &mut Session, args: &Value) -> Step {
    let Some(expression) = args.get("expression").and_then(Value::as_str).filter(|e| !e.trim().is_empty()) else {
        return done_err("`expression` is required");
    };
    if !s.debug.is_stopped() {
        return done_err("the program is not paused (debug_wait_for_stop first)");
    }
    match select_frame(s, args) {
        Ok(true) => Step::Wait(
            Wait::DebugSettle { then: AfterSettle::Evaluate { expression: expression.to_string() } },
            timeout_arg(args, 10_000),
        ),
        Ok(false) => match start_evaluate(s, expression) {
            Step::Wait(w, _) => Step::Wait(w, timeout_arg(args, 10_000)),
            done => done,
        },
        Err(step) => step,
    }
}

// ----- tests

fn test_run(s: &mut Session, args: &Value) -> Step {
    let started = if let Some(name) = args.get("name").and_then(Value::as_str) {
        s.execute("test.runFiltered", &with_target(args, json!({ "name": name })))
    } else if args.get("path").is_some() {
        let (path, line) = match zero_based_location(s, args) {
            Ok(x) => x,
            Err(step) => return step,
        };
        match line {
            Some(l) => s.execute("run.contextConfiguration", &with_target(args, json!({ "path": path, "line": l }))),
            None => s.execute("run.testsInFile", &with_target(args, json!({ "path": path }))),
        }
    } else {
        s.execute("test.runFiltered", &with_target(args, json!({ "name": "" })))
    };
    let v = match started {
        Ok(v) => v,
        Err(e) => return cmd_err(e),
    };
    let Some(tab) = v.get("tab").and_then(Value::as_u64) else {
        let why = if s.status.is_empty() { "no test was found there".to_string() } else { s.status.clone() };
        return done_err(why);
    };
    Step::Wait(Wait::Tests { tab }, timeout_arg(args, 600_000))
}

// ----- databases

fn db_query(s: &mut Session, args: &Value) -> Step {
    let wanted = args.get("dataSource").and_then(Value::as_str).unwrap_or_default();
    let Some(sql) = args.get("sql").and_then(Value::as_str).filter(|q| !q.trim().is_empty()) else {
        return done_err("`sql` is required");
    };
    let _ = s.execute("db.listDataSources", &Value::Null);
    let mut added = None;
    let source = match find_source(s, wanted) {
        Some(source) => source,
        None => match unconfigured_sqlite(s, wanted) {
            // Like dropping a .db file on DataGrip's Database tool window: add it, then query.
            Some(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| wanted.to_string());
                let v = match s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": name, "path": path })) {
                    Ok(v) => v,
                    Err(e) => return cmd_err(e),
                };
                let Some(id) = v.get("dataSource").and_then(Value::as_str).map(str::to_string) else {
                    return done_err(format!("could not add {} as a data source", path.display()));
                };
                added = Some(json!({ "id": id, "name": name, "path": path }));
                id
            }
            None => {
                let have: Vec<String> =
                    s.db.sources.iter().map(|d| format!("{} ({})", d.config.name, d.config.id)).collect();
                let files: Vec<String> = sqlite_candidates(s, 5).iter().map(|p| p.display().to_string()).collect();
                let mut msg = if have.is_empty() {
                    format!("no data source `{wanted}`: the project has no data sources")
                } else {
                    format!("no data source `{wanted}`; have: {}", have.join(", "))
                };
                if !files.is_empty() {
                    msg.push_str(&format!(". SQLite files in the project (pass one as `dataSource`): {}", files.join(", ")));
                } else if have.is_empty() {
                    msg.push_str(
                        ". Pass a SQLite file's path as `dataSource`, or add a server with codecraft_execute \
                         db.addDataSource {kind, name, host, port, database, user}",
                    );
                }
                return done_err(msg);
            }
        },
    };
    let limit = usize_arg(args, "limit", 200, 5000);
    // `params` are bound: they travel next to the SQL and never change what it does, so they
    // may only be values (an `{"sql": …}` expression would be SQL the policy did not judge).
    let params = args.get("params").cloned().unwrap_or(Value::Null);
    if let Err(e) = agent_params_ok(&params) {
        return done_err(e);
    }
    let before: Vec<u64> = s.db.results.iter().map(|r| r.id).collect();
    let output_before = s.db.output.len();
    let mut exec = json!({ "dataSource": source, "sql": sql });
    if !params.is_null()
        && let Some(o) = exec.as_object_mut()
    {
        o.insert("params".into(), params);
    }
    let v = match s.execute("db.execute", &exec) {
        Ok(v) => v,
        Err(e) => return cmd_err(e),
    };
    if v.get("prompt").and_then(Value::as_bool) == Some(true) {
        s.db.param_prompt = None;
        let names = v.get("parameters").cloned().unwrap_or(Value::Null);
        return done_err(format!(
            "the SQL has parameters {names}: pass their values in `params` (an object by name, or an array in order)"
        ));
    }
    if v.get("confirm").and_then(Value::as_bool) == Some(true) && !s.agent_access.is_interactive() {
        s.db.confirm = None;
        return done_err(format!(
            "the data source asks to confirm data modifications and this server is headless; nothing was run. Writes: {}",
            v.get("writes").cloned().unwrap_or(Value::Null)
        ));
    }
    if v.is_null() {
        return done_err("nothing to execute");
    }
    Step::Wait(Wait::Db { source, before, output_before, limit, added }, timeout_arg(args, 60_000))
}

/// Agents' `params` are values only: no `{"sql": …}` expressions.
fn agent_params_ok(params: &Value) -> Result<(), String> {
    let items: Vec<&Value> = match params {
        Value::Null => return Ok(()),
        Value::Object(m) => m.values().collect(),
        Value::Array(a) => a.iter().collect(),
        _ => return Err("`params` must be an object {name: value} or an array of values".into()),
    };
    for v in items {
        codecraft_db::bind::param_from_json(v, false)?;
    }
    Ok(())
}

fn db_outcome(s: &mut Session, source: &str, before: &[u64], output_before: usize, limit: usize, added: Option<Value>) -> Step {
    let owner = format!("query:{source}");
    let new_ids: Vec<u64> =
        s.db.results.iter().filter(|r| r.owner == owner && !before.contains(&r.id)).map(|r| r.id).collect();
    let mut results = Vec::new();
    for id in new_ids {
        match s.execute("db.getResult", &json!({ "result": id, "limit": limit })) {
            Ok(v) => {
                let mut o = Map::new();
                for key in ["sql", "columns", "rows", "totalRows", "statusLine", "error", "hasMore"] {
                    if let Some(x) = v.get(key).filter(|x| !x.is_null()) {
                        // Column descriptions in the camelCase of every other result (QA M14
                        // #24: `type_name`); rows are values and stay as the database gave them.
                        let x = if key == "columns" { camel_case_objects(x) } else { x.clone() };
                        o.insert(key.into(), x);
                    }
                }
                // QA M15: `limit: 20` on 30 rows answered 20 rows with `hasMore: false` (it
                // only meant "the server has more than were fetched").
                let shown = o.get("rows").and_then(Value::as_array).map_or(0, Vec::len);
                let total = o.get("totalRows").and_then(Value::as_u64).unwrap_or(0);
                if (shown as u64) < total {
                    o.insert("hasMore".into(), json!(true));
                    o.insert(
                        "note".into(),
                        json!(format!("{shown} of {total} rows; raise `limit` or narrow the query (WHERE, LIMIT)")),
                    );
                }
                results.push(Value::Object(o));
            }
            Err(e) => results.push(json!({ "error": e.to_string() })),
        }
    }
    let output: Vec<String> = s.db.output.iter().skip(output_before).map(|l| l.text.clone()).take(50).collect();
    if results.is_empty() && output.is_empty() {
        return done_err("the query was not run (cancelled by the user, or nothing to execute)");
    }
    // The database's errors (QA M14: `no such table` came back as an empty success).
    let errors: Vec<String> = s
        .db
        .output
        .iter()
        .skip(output_before)
        .filter(|l| l.level == crate::database::OutputLevel::Error)
        .map(|l| l.text.clone())
        .take(10)
        .collect();
    if results.is_empty() && !errors.is_empty() {
        return done_err(format!("the database refused the SQL: {}", errors.join("; ")));
    }
    let mut v = json!({ "dataSource": source, "results": results, "output": output });
    if !errors.is_empty()
        && let Some(o) = v.as_object_mut()
    {
        o.insert("errors".into(), json!(errors));
    }
    if let (Some(a), Some(o)) = (added, v.as_object_mut()) {
        o.insert("added".into(), a);
        o.insert(
            "note".into(),
            json!(format!("the SQLite file is now data source `{source}` (Database tool window); use that id next time")),
        );
    }
    done_json(v)
}

/// `snake_case` → `snakeCase`.
fn camel_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = false;
    for c in key.chars() {
        if c == '_' {
            upper = !out.is_empty();
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// The keys of the objects in `v` (an object or an array of them, one level) in camelCase.
fn camel_case_objects(v: &Value) -> Value {
    let obj = |o: &Map<String, Value>| Value::Object(o.iter().map(|(k, x)| (camel_case(k), x.clone())).collect());
    match v {
        Value::Object(o) => obj(o),
        Value::Array(a) => Value::Array(a.iter().map(|x| x.as_object().map_or_else(|| x.clone(), obj)).collect()),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------- helpers for embedders

/// Run one tool to completion on this thread, pumping the session (CLI one-shots and tests;
/// never on the UI thread). `timeout` bounds the whole call.
pub fn call_blocking(s: &mut Session, caller: Caller, name: &str, args: Value, timeout: Duration) -> ToolResult {
    let slot: Arc<Mutex<Option<ToolResult>>> = Arc::new(Mutex::new(None));
    let out = Arc::clone(&slot);
    submit(
        s,
        caller,
        name,
        args,
        Box::new(move |r| {
            *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
        }),
    );
    let start = Instant::now();
    loop {
        if let Some(r) = slot.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return r;
        }
        if start.elapsed() > timeout {
            return ToolResult::error(format!("{name}: no answer within {}s", timeout.as_secs()));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The text of a tool result (first content item), for embedders.
pub fn result_text(r: &ToolResult) -> &str {
    r.texts.first().map(String::as_str).unwrap_or_default()
}

/// A tool result's JSON (Null when it is not JSON).
pub fn result_json(r: &ToolResult) -> Value {
    serde_json::from_str(result_text(r)).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_db::Dialect;

    #[test]
    fn agent_reads_only_plain_reads() {
        for sql in [
            "select * from t",
            "-- note\nSELECT a FROM t WHERE b = 'drop table x'",
            "with x as (select 1) select * from x",
            "show tables",
            "explain select 1",
            "pragma table_info(t)",
        ] {
            assert!(agent_reads_only(sql, Dialect::Postgres), "{sql}");
        }
        for sql in [
            "select nextval('s')",
            "SELECT pg_terminate_backend(42)",
            "select * into t2 from t",
            "explain analyze select 1",
            "set role admin",
            "begin",
            "prepare p as delete from t",
            "pragma query_only = 0",
            "select dblink_exec('x', 'drop table t')",
            "update t set a = 1",
        ] {
            assert!(!agent_reads_only(sql, Dialect::Postgres), "{sql}");
        }
    }

    #[test]
    fn keys_become_camel_case() {
        assert_eq!(camel_case("type_name"), "typeName");
        assert_eq!(camel_case("a_b_c"), "aBC");
        assert_eq!(camel_case("already"), "already");
        assert_eq!(camel_case("_lead"), "lead");
        assert_eq!(camel_case("trail_"), "trail");
        let v = camel_case_objects(&json!([{ "type_name": "int", "name": "n_items" }, 3]));
        assert_eq!(v, json!([{ "typeName": "int", "name": "n_items" }, 3]));
        assert_eq!(camel_case_objects(&json!({ "is_key": true })), json!({ "isKey": true }));
    }

    #[test]
    fn outward_commands_are_not_for_agents() {
        for id in ["git.push", "vcs.push", "vcs.github.share", "vcs.pr.merge", "docker.prune"] {
            if crate::registry().get(id).is_some() {
                assert_eq!(command_capability(id), None, "{id}");
            }
        }
        assert_eq!(command_capability("git.gitHub.shareProjectOnGitHub"), None);
        assert_eq!(command_capability("tools.deployment.uploadToDefaultServer"), None);
    }

    /// QA M14: adding a data source needed `db-write`, and reading the query history too.
    #[test]
    fn data_source_settings_and_metadata_are_not_database_writes() {
        assert_eq!(command_capability("db.addDataSource"), Some(Capability::Write));
        assert_eq!(command_capability("db.editDataSource"), Some(Capability::Write));
        for id in ["db.getHistory", "db.connect", "db.refresh", "db.rowCount", "db.listDataSources"] {
            assert_eq!(command_capability(id), Some(Capability::Read), "{id}");
        }
        assert_eq!(command_capability("db.commit"), Some(Capability::DbWrite));
        assert_eq!(command_capability("db.removeDataSource"), Some(Capability::DbWrite));
    }

    /// QA M15: commands the agent docs name as reads needed `write` (`vcs.status`,
    /// `analysis.fileFindings`) or `profile`/`debug` (reports and listings); analyzing a file
    /// runs an analyzer.
    #[test]
    fn documented_queries_are_reads() {
        for id in [
            "vcs.status",
            "vcs.changelists",
            "vcs.shelves",
            "analysis.fileFindings",
            "python.languageServer",
            "debug.sessions",
            "profiler.report",
            "profiler.memory",
            "profiler.lineHits",
            "profiler.compare",
            "profiler.job",
        ] {
            assert!(crate::registry().get(id).is_some(), "{id} is a command");
            assert_eq!(command_capability(id), Some(Capability::Read), "{id}");
        }
        assert_eq!(command_capability("analysis.analyzeFile"), Some(Capability::Run));
        assert_eq!(command_capability("analysis.fix"), Some(Capability::Write));
        assert_eq!(command_capability("vcs.commit"), Some(Capability::Write));
        assert_eq!(command_capability("profiler.record"), Some(Capability::Profile));
    }

    #[test]
    fn sqlite_files_are_recognised_by_their_header() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.data");
        std::fs::write(&db, b"SQLite format 3\0rest of the page").unwrap();
        assert!(is_sqlite_file(&db), "the header, whatever the name");
        let empty = dir.path().join("new.sqlite");
        std::fs::write(&empty, b"").unwrap();
        assert!(is_sqlite_file(&empty), "an empty file named like a database");
        let text = dir.path().join("notes.db");
        std::fs::write(&text, b"not a database at all").unwrap();
        assert!(!is_sqlite_file(&text));
        assert!(!is_sqlite_file(dir.path()), "a directory");
        assert!(!is_sqlite_file(&dir.path().join("missing.db")));
    }
}
