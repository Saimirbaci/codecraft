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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
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
}

impl Caller {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), job: None }
    }

    /// From an MCP client's `clientInfo` (`name`, and `codecraftJob` when the bridge of an agent
    /// job connected). A job id that is not one of the session's jobs is ignored.
    pub fn from_client_info(s: &Session, info: &Value, fallback: &str) -> Self {
        let job = info.get("codecraftJob").and_then(Value::as_u64).map(JobId).filter(|j| s.agents.job(*j).is_some());
        if let Some(job) = job.and_then(|j| s.agents.job(j)) {
            return Self { name: format!("{} (job {})", job.backend.label(), job.id.0), job: Some(job.id) };
        }
        let name = info.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).unwrap_or(fallback);
        Self { name: name.to_string(), job: None }
    }
}

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
    /// The debugger to stop (settled) or the session to end.
    DebugStop {
        frames: usize,
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
    },
    RunDone {
        args: Value,
    },
    Db {
        source: String,
        before: Vec<u64>,
        output_before: usize,
        limit: usize,
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

// ---------------------------------------------------------------- capabilities

/// Read-only commands outside the read-only namespaces.
const READ_COMMANDS: &[&str] = &[
    "agentAccess.get",
    "analysis.problems",
    "commands.search",
    "coverage.summary",
    "db.getResult",
    "db.listDataSources",
    "debug.state",
    "debug.stopSummary",
    "debug.variablesOf",
    "files.read",
    "inspections.results",
    "lsp.query",
    "lsp.queryResult",
    "lsp.status",
    "profiler.callTree",
    "profiler.hotspots",
    "profiler.methods",
    "profiler.recent",
    "profiler.status",
    "profiler.tools",
    "project.info",
    "run.configurations",
    "run.output",
    "run.testHistory",
    "runConfig.generate.list",
    "runConfig.list",
    "runConfig.templates.list",
    "test.summary",
    "test.tree",
];

/// Commands in run-like namespaces that change files.
const WRITE_COMMANDS: &[&str] =
    &["inspections.fix", "inspections.fixAll", "inspections.suppress", "test.export", "test.clearHistory"];

/// The capability a command needs through `codecraft_execute`; `None` = agents may not run it
/// (their own permissions, approvals, other agents, the IDE integration itself).
pub fn command_capability(id: &str) -> Option<Capability> {
    let id = crate::registry().canonical(id);
    if READ_COMMANDS.contains(&id) {
        return Some(Capability::Read);
    }
    if id.starts_with("agentAccess.")
        || id.starts_with("agent.")
        || id.starts_with("ide.")
        || id.starts_with("feedback.")
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
        || id == "code.inspectCode";
    if run {
        return Some(Capability::Run);
    }
    if ["navigate.", "view.", "window.", "help.", "hierarchy."].iter().any(|p| id.starts_with(p)) {
        return Some(Capability::Read);
    }
    Some(Capability::Write)
}

/// The data source an agent named (by id or name).
fn find_source(s: &Session, wanted: &str) -> Option<String> {
    let w = wanted.trim();
    s.db.sources
        .iter()
        .find(|d| d.config.id == w)
        .or_else(|| s.db.sources.iter().find(|d| d.config.name.eq_ignore_ascii_case(w)))
        .map(|d| d.config.id.clone())
}

/// Whether `sql` modifies data on `source`.
fn sql_writes(s: &Session, source: &str, sql: &str) -> bool {
    let dialect = s.db.dialect_for(None, Some(source));
    codecraft_db::split_statements(sql, dialect).iter().any(|st| codecraft_db::is_modifying(&st.text, dialect))
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
                    if sql_writes(s, &source, sql) { Capability::DbWrite } else { Capability::Read }
                }
                _ => cap,
            }
        }
        t::DB_QUERY => {
            let sql = args.get("sql").and_then(Value::as_str).unwrap_or_default();
            let source = args.get("dataSource").and_then(Value::as_str).and_then(|d| find_source(s, d));
            match source {
                Some(src) if sql_writes(s, &src, sql) => Capability::DbWrite,
                _ => Capability::Read,
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
    let cap = match required_capability(s, name, &args) {
        Ok(c) => c,
        Err(e) => return respond(ToolResult::error(e)),
    };
    let job_mode = caller.job.and_then(|j| s.agents.job(j)).map(|j| j.mode);
    match s.agent_access.verdict(&caller.name, job_mode, cap) {
        Verdict::Deny(why) => {
            s.agent_tools.answered += 1;
            respond(ToolResult::error(why));
        }
        Verdict::Allow => start(s, caller, name, args, respond),
        Verdict::Ask => {
            let source = crate::agent_access::approval_source(&caller.name, cap);
            let title = format!("{} wants to use {name} ({})", caller.name, cap.label().to_lowercase());
            let kind = ApprovalKind::ToolUse { tool: name.to_string(), input: args.clone() };
            let (approval, rx) = s.approvals.sink().ask(source, title, kind);
            s.status = format!("{} asks to use {name}", caller.name);
            s.agent_tools.pending.push(PendingCall {
                caller,
                name: name.to_string(),
                deadline: Instant::now() + s.agent_tools.approval_timeout,
                respond,
                wait: Wait::Approval { approval, rx, cap, tool: name.to_string(), args },
            });
        }
    }
}

fn start(s: &mut Session, caller: Caller, name: &str, args: Value, respond: Respond) {
    let step = run_tool(s, &caller, name, &args);
    settle_step(s, caller, name.to_string(), step, respond);
}

/// Answer a finished step, or park it (polled once right away so instant results don't wait a
/// pump).
fn settle_step(s: &mut Session, caller: Caller, name: String, step: Step, respond: Respond) {
    match step {
        Step::Done(r) => {
            s.agent_tools.answered += 1;
            respond(r);
        }
        Step::Wait(mut wait, timeout) => match poll(s, &caller, &mut wait) {
            Some(step) => settle_step(s, caller, name, step, respond),
            None => {
                let deadline = Instant::now() + timeout;
                s.agent_tools.pending.push(PendingCall { caller, name, deadline, respond, wait });
            }
        },
    }
}

/// Finish parked calls. True if any was answered.
pub(crate) fn pump(s: &mut Session) -> bool {
    if s.agent_tools.pending.is_empty() {
        return false;
    }
    let calls = std::mem::take(&mut s.agent_tools.pending);
    let mut changed = false;
    for mut call in calls {
        match poll(s, &call.caller, &mut call.wait) {
            Some(step) => {
                changed = true;
                settle_step(s, call.caller, call.name, step, call.respond);
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

/// Answer every parked call (the session is closing).
pub(crate) fn shutdown(s: &mut Session) {
    for call in std::mem::take(&mut s.agent_tools.pending) {
        if let Wait::Approval { approval, .. } = &call.wait {
            s.approvals.decide(*approval, Decision::Deny);
        }
        (call.respond)(ToolResult::error("CodeCraft is shutting down"));
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
        Wait::DebugStop { frames } => {
            let mut v = stop_summary(s, *frames, 1);
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
        _ => ToolResult::error(format!("{name}: timed out; the IDE is still working on it, try again later")),
    }
}

/// `Some` once the wait is over.
fn poll(s: &mut Session, caller: &Caller, wait: &mut Wait) -> Option<Step> {
    match wait {
        Wait::Approval { rx, cap, tool, args, .. } => match rx.try_recv() {
            Ok(Decision::Allow) => Some(run_tool(s, caller, tool, args)),
            Ok(Decision::AllowAlways) => {
                s.agent_access.allow_for_session(&caller.name, *cap);
                Some(run_tool(s, caller, tool, args))
            }
            Ok(Decision::Deny) | Err(TryRecvError::Disconnected) => Some(done_err("denied by the user")),
            Err(TryRecvError::Empty) => None,
        },
        Wait::DebugStop { frames } => debug_settled(s).then(|| done_json(stop_summary(s, *frames, 1))),
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
                Err(e) => done_err(format!("cannot evaluate `{expression}`: {e}")),
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
        Wait::Profile { limit } => {
            if !s.profiler.recordings.is_empty() || s.profiler.loading.is_some() {
                return None;
            }
            Some(match s.execute("profiler.hotspots", &json!({ "limit": limit })) {
                Ok(v) => done_json(v),
                Err(e) => cmd_err(e),
            })
        }
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
        Wait::Db { source, before, output_before, limit } => {
            if s.db.confirm.is_some() || s.db.running() {
                return None;
            }
            Some(db_outcome(s, source, before, *output_before, *limit))
        }
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
            .ok_or_else(|| format!("no run configuration named `{name}`; have: {}", names())),
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
    match name {
        t::COMMANDS => exec_done(s, "commands.search", args.clone()),
        t::EXECUTE => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let params = args.get("params").cloned().unwrap_or(Value::Null);
            match s.execute(&id, &params) {
                Ok(v) => done_json(json!({ "id": id, "result": v })),
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
            Step::Wait(Wait::DebugStop { frames }, timeout_arg(args, 30_000))
        }
        t::DEBUG_STACK => debug_stack(s, args),
        t::DEBUG_VARIABLES => debug_variables(s, args),
        t::DEBUG_EVALUATE => debug_evaluate(s, args),
        t::PROFILE_START => {
            let mut p = json!({});
            if let Some(c) = args.get("config") {
                match config_index(s, c) {
                    Ok(i) => p = json!({ "config": i }),
                    Err(e) => return done_err(e),
                }
            }
            match s.execute("run.profile", &p) {
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
            Step::Wait(Wait::Profile { limit }, timeout_arg(args, 120_000))
        }
        t::PROFILE_HOTSPOTS => {
            let limit = usize_arg(args, "limit", 20, 200);
            Step::Wait(Wait::Profile { limit }, timeout_arg(args, 120_000))
        }
        t::TEST_RUN => test_run(s, args),
        t::TEST_RESULTS => exec_done(s, "test.summary", args.clone()),
        t::RUN_START => {
            let Some(c) = args.get("config") else { return done_err("`config` is required") };
            let i = match config_index(s, c) {
                Ok(i) => i,
                Err(e) => return done_err(e),
            };
            match s.execute("run.run", &json!({ "config": i })) {
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
            Ok(v) => done_json(v.get("dataSources").cloned().unwrap_or(json!([]))),
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
    if let Err(step) = exec(s, "code.inspectCode", p) {
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
    let mut out = json!({
        "scope": v.get("scopeLabel").or_else(|| v.get("scope")).cloned().unwrap_or(Value::Null),
        "tools": v.get("tools").cloned().unwrap_or(Value::Null),
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

fn debug_start(s: &mut Session, args: &Value) -> Step {
    let started = if let Some(c) = args.get("config") {
        match config_index(s, c) {
            Ok(i) => s.execute("run.debug", &json!({ "config": i })),
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
        s.execute("debug.executable", &p)
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
        s.execute("debug.atCaret", &p)
    } else {
        s.execute("run.debug", &json!({}))
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
        "pause" => ("run.debuggingActions.pauseAllPrograms", Value::Null, true),
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
        Step::Wait(Wait::DebugStop { frames }, timeout_arg(args, 30_000))
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
            o.insert("name".into(), json!(f.name));
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
        s.execute("test.runFiltered", &json!({ "name": name }))
    } else if args.get("path").is_some() {
        let (path, line) = match zero_based_location(s, args) {
            Ok(x) => x,
            Err(step) => return step,
        };
        match line {
            Some(l) => s.execute("run.contextConfiguration", &json!({ "path": path, "line": l })),
            None => s.execute("run.testsInFile", &json!({ "path": path })),
        }
    } else {
        s.execute("test.runFiltered", &json!({ "name": "" }))
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
    let _ = s.execute("db.listDataSources", &Value::Null);
    let Some(source) = find_source(s, wanted) else {
        let have: Vec<String> = s.db.sources.iter().map(|d| format!("{} ({})", d.config.name, d.config.id)).collect();
        return done_err(format!("no data source `{wanted}`; have: {}", have.join(", ")));
    };
    let Some(sql) = args.get("sql").and_then(Value::as_str).filter(|q| !q.trim().is_empty()) else {
        return done_err("`sql` is required");
    };
    let limit = usize_arg(args, "limit", 200, 5000);
    let before: Vec<u64> = s.db.results.iter().map(|r| r.id).collect();
    let output_before = s.db.output.len();
    let v = match s.execute("db.execute", &json!({ "dataSource": source, "sql": sql })) {
        Ok(v) => v,
        Err(e) => return cmd_err(e),
    };
    if v.get("prompt").and_then(Value::as_bool) == Some(true) {
        s.db.param_prompt = None;
        return done_err("the SQL has parameters (?, $1, :name); put the values into the statement");
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
    Step::Wait(Wait::Db { source, before, output_before, limit }, timeout_arg(args, 60_000))
}

fn db_outcome(s: &mut Session, source: &str, before: &[u64], output_before: usize, limit: usize) -> Step {
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
                        o.insert(key.into(), x.clone());
                    }
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
    done_json(json!({ "dataSource": source, "results": results, "output": output }))
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
