//! Agent Client Protocol client (<https://agentclientprotocol.com>, protocol version 1):
//! JSON-RPC 2.0, one message per line, over the agent's stdio.
//!
//! The client sends `initialize`, `session/new {cwd, mcpServers: []}`, then one `session/prompt`
//! per turn (`session/cancel` to stop a turn). To continue an earlier session ([`start_resumed`])
//! it sends `session/load {sessionId, cwd, mcpServers: []}` instead when the agent advertises
//! `agentCapabilities.loadSession` (the history the agent replays is dropped: the host has it),
//! and otherwise starts a new session whose first prompt carries a transcript of the old one. It turns `session/update` notifications (message
//! chunks, `tool_call`, `tool_call_update`, `plan`) into [`AgentEvent`]s and forwards the
//! agent's requests (`session/request_permission`, `fs/read_text_file`, `fs/write_text_file`) to
//! the host as [`HostRequest`]s; the host answers through the request's [`Responder`] whenever it
//! is ready, from any thread. Terminals are not offered (`terminal: false`).
//!
//! Threads: a writer (outgoing messages from a channel), a reader (stdout: responses,
//! notifications and requests), a driver (the handshake and the prompt loop), plus the process
//! supervisor from [`crate::process`].

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, select, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::{AgentEvent, ChangeKind, MAX_OUTPUT_CHARS, PlanEntry, command_text};
use crate::process::{AgentProcess, read_lines, spawn};
use crate::{AgentError, Result, truncate};

pub const PROTOCOL_VERSION: u64 = 1;
/// How long the agent may take to answer `initialize` / `session/new`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Error code for "authentication required" (ACP).
const AUTH_REQUIRED: i64 = -32000;
const METHOD_NOT_FOUND: i64 = -32601;

/// Control messages for a running session.
#[derive(Debug)]
enum Control {
    Prompt(String),
    Cancel,
    Shutdown,
}

/// What a permission request is about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PermissionKind {
    /// The tool call carries a diff.
    FileEdit { path: PathBuf, old_text: Option<String>, new_text: String },
    /// An `execute` tool call with a command line.
    Command { command: String },
    /// Anything else.
    Tool { name: String, input: Value },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once` or `reject_always`.
    pub kind: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionChoice {
    AllowOnce,
    AllowAlways,
    Deny,
    /// The turn was cancelled.
    Cancelled,
}

/// The option to answer with: the closest kind the agent offers.
pub fn pick_option(options: &[PermissionOption], choice: PermissionChoice) -> Option<&str> {
    let order: &[&str] = match choice {
        PermissionChoice::AllowOnce => &["allow_once", "allow_always"],
        PermissionChoice::AllowAlways => &["allow_always", "allow_once"],
        PermissionChoice::Deny => &["reject_once", "reject_always"],
        PermissionChoice::Cancelled => &[],
    };
    order.iter().find_map(|k| options.iter().find(|o| o.kind == *k)).map(|o| o.id.as_str())
}

/// Answers one request from the agent. Dropping it unanswered answers with an internal error,
/// so the agent never waits forever.
pub struct Responder {
    id: Option<Value>,
    out: Sender<Value>,
}

impl std::fmt::Debug for Responder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responder").field("id", &self.id).finish()
    }
}

impl Responder {
    pub fn result(mut self, result: Value) {
        if let Some(id) = self.id.take() {
            let _ = self.out.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }

    pub fn error(mut self, code: i64, message: &str) {
        if let Some(id) = self.id.take() {
            let _ = self.out.send(json!({
                "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message }
            }));
        }
    }

    /// Answer a `session/request_permission`.
    pub fn permission(self, options: &[PermissionOption], choice: PermissionChoice) {
        let outcome = match pick_option(options, choice) {
            Some(id) => json!({ "outcome": "selected", "optionId": id }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.result(json!({ "outcome": outcome }));
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.out.send(json!({
                "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": "request dropped by the client" }
            }));
        }
    }
}

/// How to continue an earlier session (see [`start_resumed`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resume {
    /// The agent's id of the earlier session, loaded with `session/load` when the agent offers
    /// `loadSession`.
    pub session_id: Option<String>,
    /// Put before the first prompt when the session is not loaded (a transcript of it), so the
    /// agent still has the context.
    pub fallback_preamble: Option<String>,
}

/// A request from the agent the host must answer.
#[derive(Debug)]
pub enum HostRequest {
    Permission {
        title: String,
        kind: PermissionKind,
        options: Vec<PermissionOption>,
        responder: Responder,
    },
    /// `line` is 1-based.
    ReadTextFile {
        path: PathBuf,
        line: Option<u64>,
        limit: Option<u64>,
        responder: Responder,
    },
    WriteTextFile {
        path: PathBuf,
        content: String,
        responder: Responder,
    },
}

/// A running ACP agent.
#[derive(Clone)]
pub struct AcpClient {
    process: AgentProcess,
    ctl: Sender<Control>,
}

impl AcpClient {
    pub fn process(&self) -> &AgentProcess {
        &self.process
    }

    /// Queue a follow-up prompt (sent when the current turn ends). False if the session is gone.
    pub fn prompt(&self, text: &str) -> bool {
        self.process.is_running() && self.ctl.send(Control::Prompt(text.to_string())).is_ok()
    }

    /// Cancel the current turn (`session/cancel`).
    pub fn cancel(&self) {
        let _ = self.ctl.send(Control::Cancel);
    }

    /// End the session and kill the agent.
    pub fn shutdown(&self) {
        let _ = self.ctl.send(Control::Shutdown);
        self.process.kill();
    }
}

type Pending = Arc<Mutex<HashMap<u64, Sender<std::result::Result<Value, RpcError>>>>>;

#[derive(Clone, Debug)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Clone)]
struct Rpc {
    out: Sender<Value>,
    pending: Pending,
    next: Arc<AtomicU64>,
}

impl Rpc {
    fn call(&self, method: &str, params: Value) -> Receiver<std::result::Result<Value, RpcError>> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        let _ = self.out.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self.out.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
}

/// Start an ACP agent in `cwd` and send `prompt` as the first turn.
pub fn start(
    cmd: Command,
    cwd: &Path,
    prompt: String,
    events: Sender<AgentEvent>,
    host: Sender<HostRequest>,
) -> Result<AcpClient> {
    start_resumed(cmd, cwd, prompt, Resume::default(), events, host)
}

/// [`start`], continuing an earlier session: `session/load` when the agent supports it, else a
/// new session whose first prompt starts with `resume.fallback_preamble`.
pub fn start_resumed(
    cmd: Command,
    cwd: &Path,
    prompt: String,
    resume: Resume,
    events: Sender<AgentEvent>,
    host: Sender<HostRequest>,
) -> Result<AcpClient> {
    let sp = spawn(cmd, true, events.clone())?;
    let (out_tx, out_rx) = unbounded::<Value>();
    let pending: Pending = Arc::default();
    let rpc = Rpc { out: out_tx.clone(), pending: Arc::clone(&pending), next: Arc::new(AtomicU64::new(0)) };

    // Writer.
    if let Some(mut stdin) = sp.stdin {
        std::thread::Builder::new()
            .name("codecraft-acp-writer".into())
            .spawn(move || {
                for msg in out_rx {
                    let mut line = msg.to_string();
                    line.push('\n');
                    if stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).is_err() {
                        break;
                    }
                }
            })
            .map_err(AgentError::Io)?;
    }
    // Reader.
    let done = sp.done_tx;
    let reader_events = events.clone();
    let stdout = sp.stdout;
    let replaying = Arc::new(AtomicBool::new(false));
    let reader = Reader {
        events: reader_events,
        host,
        out: out_tx,
        pending,
        tools: HashMap::new(),
        cwd: cwd.into(),
        replaying: Arc::clone(&replaying),
    };
    std::thread::Builder::new()
        .name("codecraft-acp-reader".into())
        .spawn(move || {
            let mut reader = reader;
            if let Some(stdout) = stdout {
                read_lines(stdout, |line| reader.line(&line));
            }
            // Fail every outstanding call: the agent is gone.
            reader.pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
            let _ = done.send(());
        })
        .map_err(AgentError::Io)?;
    // Driver.
    let (ctl_tx, ctl_rx) = unbounded();
    let cwd = cwd.to_path_buf();
    let process = sp.process;
    let driver_proc = process.clone();
    std::thread::Builder::new()
        .name("codecraft-acp-driver".into())
        .spawn(move || {
            if let Err(e) = drive(&rpc, &cwd, prompt, &resume, &replaying, &ctl_rx, &events) {
                let _ = events.send(AgentEvent::Error { message: e });
                let _ =
                    events.send(AgentEvent::Result { is_error: true, cost_usd: None, duration_ms: None, text: None });
                driver_proc.kill();
            }
        })
        .map_err(AgentError::Io)?;
    Ok(AcpClient { process, ctl: ctl_tx })
}

enum Wait {
    Done(std::result::Result<Value, RpcError>),
    Shutdown,
}

/// Wait for a response, handling control messages meanwhile.
fn wait(
    rx: &Receiver<std::result::Result<Value, RpcError>>,
    ctl: &Receiver<Control>,
    queued: &mut std::collections::VecDeque<String>,
    timeout: Option<Duration>,
    on_cancel: &mut dyn FnMut(),
) -> std::result::Result<Wait, String> {
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
        let tick = crossbeam_channel::after(left.unwrap_or(Duration::from_secs(3600)));
        select! {
            recv(rx) -> r => return match r {
                Ok(r) => Ok(Wait::Done(r)),
                Err(_) => Err("the agent exited".into()),
            },
            recv(ctl) -> c => match c {
                Ok(Control::Prompt(p)) => queued.push_back(p),
                Ok(Control::Cancel) => on_cancel(),
                Ok(Control::Shutdown) | Err(_) => return Ok(Wait::Shutdown),
            },
            recv(tick) -> _ => if deadline.is_some() {
                return Err("the agent did not answer in time".into());
            },
        }
    }
}

fn rpc_message(e: &RpcError) -> String {
    format!("{} (code {})", e.message, e.code)
}

/// The outcome of `session/new` or `session/load`.
enum Opened {
    Done(Value),
    Failed(String),
    Shutdown,
}

/// Call `session/new` or `session/load`, signing in once with the agent's first auth method when
/// it asks for authentication.
fn open_session(
    rpc: &Rpc,
    ctl: &Receiver<Control>,
    queued: &mut std::collections::VecDeque<String>,
    method: &str,
    params: &Value,
    auth_methods: &[String],
) -> std::result::Result<Opened, String> {
    let mut stop_handshake = || {};
    for attempt in 0..2 {
        let rx = rpc.call(method, params.clone());
        match wait(&rx, ctl, queued, Some(HANDSHAKE_TIMEOUT), &mut stop_handshake)? {
            Wait::Shutdown => return Ok(Opened::Shutdown),
            Wait::Done(Ok(v)) => return Ok(Opened::Done(v)),
            Wait::Done(Err(e)) if e.code == AUTH_REQUIRED && attempt == 0 && !auth_methods.is_empty() => {
                // Use the agent's stored login (the first method it offers), then retry once.
                let method = auth_methods.first().cloned().unwrap_or_default();
                let rx = rpc.call("authenticate", json!({ "methodId": method }));
                match wait(&rx, ctl, queued, Some(HANDSHAKE_TIMEOUT), &mut stop_handshake)? {
                    Wait::Shutdown => return Ok(Opened::Shutdown),
                    Wait::Done(Ok(_)) => {}
                    Wait::Done(Err(e)) => {
                        return Err(format!(
                            "the agent needs you to sign in (run it once in a terminal): {}",
                            rpc_message(&e)
                        ));
                    }
                }
            }
            Wait::Done(Err(e)) => return Ok(Opened::Failed(rpc_message(&e))),
        }
    }
    Ok(Opened::Failed("the agent still asks for authentication".into()))
}

/// Handshake, then one `session/prompt` per queued prompt until shutdown.
fn drive(
    rpc: &Rpc,
    cwd: &Path,
    first: String,
    resume: &Resume,
    replaying: &AtomicBool,
    ctl: &Receiver<Control>,
    events: &Sender<AgentEvent>,
) -> std::result::Result<(), String> {
    let mut queued = std::collections::VecDeque::from([first]);
    let mut stop_handshake = || {};
    let init = rpc.call(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": false },
            "clientInfo": { "name": "codecraft", "title": "CodeCraft", "version": env!("CARGO_PKG_VERSION") }
        }),
    );
    let init = match wait(&init, ctl, &mut queued, Some(HANDSHAKE_TIMEOUT), &mut stop_handshake)? {
        Wait::Shutdown => return Ok(()),
        Wait::Done(r) => r.map_err(|e| format!("initialize failed: {}", rpc_message(&e)))?,
    };
    let version = init.get("protocolVersion").and_then(Value::as_u64);
    if version != Some(PROTOCOL_VERSION) {
        return Err(format!("the agent speaks ACP version {version:?}; CodeCraft supports {PROTOCOL_VERSION}"));
    }
    let auth_methods: Vec<String> = init
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    let can_load = init.pointer("/agentCapabilities/loadSession").and_then(Value::as_bool).unwrap_or(false);
    let mut session = None;
    if let Some(old) = resume.session_id.as_deref().filter(|id| !id.is_empty()) {
        if can_load {
            let params = json!({ "sessionId": old, "cwd": cwd, "mcpServers": [] });
            // The agent replays the conversation as updates before it answers; the host already
            // has it, so those updates are dropped.
            replaying.store(true, Ordering::SeqCst);
            let opened = open_session(rpc, ctl, &mut queued, "session/load", &params, &auth_methods);
            replaying.store(false, Ordering::SeqCst);
            match opened? {
                Opened::Shutdown => return Ok(()),
                Opened::Done(_) => {
                    session = Some(old.to_string());
                    let _ = events.send(AgentEvent::Log { line: format!("Resumed the agent's session {old}") });
                }
                Opened::Failed(e) => {
                    let _ = events.send(AgentEvent::Log {
                        line: format!("Cannot load the earlier session ({e}); continuing in a new one"),
                    });
                }
            }
        } else {
            let _ = events.send(AgentEvent::Log {
                line: "The agent cannot load earlier sessions; continuing in a new one with a transcript".into(),
            });
        }
    }
    if session.is_none() {
        if let (Some(pre), Some(first)) = (&resume.fallback_preamble, queued.front_mut()) {
            *first = format!("{pre}{first}");
        }
        let params = json!({ "cwd": cwd, "mcpServers": [] });
        match open_session(rpc, ctl, &mut queued, "session/new", &params, &auth_methods)? {
            Opened::Shutdown => return Ok(()),
            Opened::Done(v) => session = v.get("sessionId").and_then(Value::as_str).map(str::to_string),
            Opened::Failed(e) => return Err(format!("session/new failed: {e}")),
        }
    }
    let session = session.ok_or("session/new returned no sessionId")?;
    let _ = events.send(AgentEvent::Session { id: session.clone() });

    loop {
        let prompt = match queued.pop_front() {
            Some(p) => p,
            None => match ctl.recv() {
                Ok(Control::Prompt(p)) => p,
                Ok(Control::Cancel) => continue,
                Ok(Control::Shutdown) | Err(_) => return Ok(()),
            },
        };
        let rx =
            rpc.call("session/prompt", json!({ "sessionId": session, "prompt": [{ "type": "text", "text": prompt }] }));
        let sid = session.clone();
        let mut cancel = || rpc.notify("session/cancel", json!({ "sessionId": sid }));
        match wait(&rx, ctl, &mut queued, None, &mut cancel)? {
            Wait::Shutdown => return Ok(()),
            Wait::Done(Ok(v)) => {
                let reason = v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn").to_string();
                let is_error = matches!(reason.as_str(), "refusal" | "max_tokens" | "max_turn_requests");
                let _ = events.send(AgentEvent::Result {
                    is_error,
                    cost_usd: None,
                    duration_ms: None,
                    text: (reason != "end_turn").then(|| format!("stop reason: {reason}")),
                });
            }
            Wait::Done(Err(e)) => {
                let _ = events.send(AgentEvent::Error { message: rpc_message(&e) });
                let _ =
                    events.send(AgentEvent::Result { is_error: true, cost_usd: None, duration_ms: None, text: None });
            }
        }
    }
}

/// What the reader remembers about a tool call (updates carry only the changed fields).
#[derive(Default)]
struct ToolState {
    title: String,
    kind: String,
    raw_input: Value,
    content: Vec<Value>,
    locations: Vec<PathBuf>,
}

impl ToolState {
    fn merge(&mut self, v: &Value) {
        if let Some(t) = v.get("title").and_then(Value::as_str) {
            self.title = t.to_string();
        }
        if let Some(k) = v.get("kind").and_then(Value::as_str) {
            self.kind = k.to_string();
        }
        if let Some(r) = v.get("rawInput").filter(|r| !r.is_null()) {
            self.raw_input = r.clone();
        }
        if let Some(c) = v.get("content").and_then(Value::as_array) {
            self.content = c.clone();
        }
        if let Some(l) = v.get("locations").and_then(Value::as_array) {
            self.locations =
                l.iter().filter_map(|x| x.get("path").and_then(Value::as_str)).map(PathBuf::from).collect();
        }
    }

    fn diff(&self) -> Option<(PathBuf, Option<String>, String)> {
        self.content.iter().find(|c| c.get("type").and_then(Value::as_str) == Some("diff")).and_then(|d| {
            let path = d.get("path").and_then(Value::as_str)?;
            let new = d.get("newText").and_then(Value::as_str)?;
            let old = d.get("oldText").and_then(Value::as_str).map(str::to_string);
            Some((PathBuf::from(path), old, new.to_string()))
        })
    }

    fn text(&self) -> String {
        let t = self
            .content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("content"))
            .filter_map(|c| c.pointer("/content/text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        truncate(&t, MAX_OUTPUT_CHARS)
    }

    fn permission_kind(&self) -> PermissionKind {
        if let Some((path, old_text, new_text)) = self.diff() {
            return PermissionKind::FileEdit { path, old_text, new_text };
        }
        if self.kind == "execute" {
            let cmd = command_text(self.raw_input.get("command").or_else(|| self.raw_input.get("cmd")))
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| self.title.clone());
            return PermissionKind::Command { command: cmd };
        }
        let name = if self.title.is_empty() { self.kind.clone() } else { self.title.clone() };
        PermissionKind::Tool { name, input: self.raw_input.clone() }
    }
}

struct Reader {
    events: Sender<AgentEvent>,
    host: Sender<HostRequest>,
    out: Sender<Value>,
    pending: Pending,
    tools: HashMap<String, ToolState>,
    cwd: PathBuf,
    /// `session/load` is replaying history the host already has: drop updates.
    replaying: Arc<AtomicBool>,
}

impl Reader {
    /// One line from the agent. Returns false to stop reading (the host went away).
    fn line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return true;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            return self.events.send(AgentEvent::Log { line: truncate(line, 2000) }).is_ok();
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        match (method, id) {
            (Some(m), Some(id)) => self.request(m, id, msg.get("params").unwrap_or(&Value::Null)),
            (Some(m), None) => {
                if m == "session/update"
                    && !self.replaying.load(Ordering::SeqCst)
                    && let Some(update) = msg.pointer("/params/update")
                {
                    for ev in self.update(update) {
                        if self.events.send(ev).is_err() {
                            return false;
                        }
                    }
                }
            }
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return true };
                let tx = self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                if let Some(tx) = tx {
                    let r = match msg.get("error") {
                        Some(e) => Err(RpcError {
                            code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: e.get("message").and_then(Value::as_str).unwrap_or("error").to_string(),
                        }),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(r);
                }
            }
            (None, None) => {}
        }
        true
    }

    fn request(&mut self, method: &str, id: Value, params: &Value) {
        let responder = Responder { id: Some(id), out: self.out.clone() };
        let path = || params.get("path").and_then(Value::as_str).map(PathBuf::from);
        let req = match method {
            "session/request_permission" => {
                let call = params.get("toolCall").unwrap_or(&Value::Null);
                let tool_id = call.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
                let state = self.tools.entry(tool_id).or_default();
                state.merge(call);
                let options = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|o| {
                                Some(PermissionOption {
                                    id: o.get("optionId").and_then(Value::as_str)?.to_string(),
                                    name: o.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                                    kind: o.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let title = if state.title.is_empty() { "Tool call".to_string() } else { state.title.clone() };
                HostRequest::Permission { title, kind: state.permission_kind(), options, responder }
            }
            "fs/read_text_file" => {
                let Some(path) = path() else { return responder.error(-32602, "missing path") };
                HostRequest::ReadTextFile {
                    path,
                    line: params.get("line").and_then(Value::as_u64),
                    limit: params.get("limit").and_then(Value::as_u64),
                    responder,
                }
            }
            "fs/write_text_file" => {
                let (Some(path), Some(content)) = (path(), params.get("content").and_then(Value::as_str)) else {
                    return responder.error(-32602, "missing path or content");
                };
                HostRequest::WriteTextFile { path, content: content.to_string(), responder }
            }
            other => return responder.error(METHOD_NOT_FOUND, &format!("method not supported: {other}")),
        };
        if let Err(e) = self.host.send(req) {
            // No host: deny rather than hang.
            match e.into_inner() {
                HostRequest::Permission { options, responder, .. } => {
                    responder.permission(&options, PermissionChoice::Cancelled)
                }
                HostRequest::ReadTextFile { responder, .. } | HostRequest::WriteTextFile { responder, .. } => {
                    responder.error(-32603, "the IDE is closing")
                }
            }
        }
    }

    fn update(&mut self, u: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
            "agent_message_chunk" => {
                if let Some(text) = u.pointer("/content/text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(AgentEvent::TextDelta { text: text.to_string() });
                }
            }
            "tool_call" => {
                let id = u.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
                let mut st = ToolState::default();
                st.merge(u);
                out.push(AgentEvent::ToolUse {
                    id: id.clone(),
                    tool: if st.kind.is_empty() { "other".into() } else { st.kind.clone() },
                    summary: if st.title.is_empty() { "Tool call".into() } else { truncate(&st.title, 160) },
                    input: st.raw_input.clone(),
                });
                let status = u.get("status").and_then(Value::as_str).unwrap_or("");
                self.tools.insert(id.clone(), st);
                self.finish_tool(&id, status, &mut out);
            }
            "tool_call_update" => {
                let id = u.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
                self.tools.entry(id.clone()).or_default().merge(u);
                let status = u.get("status").and_then(Value::as_str).unwrap_or("");
                self.finish_tool(&id, status, &mut out);
            }
            "plan" => {
                let entries = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|e| PlanEntry {
                                content: e.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
                                status: e.get("status").and_then(Value::as_str).unwrap_or("pending").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(AgentEvent::Plan { entries });
            }
            _ => {}
        }
        out
    }

    fn finish_tool(&mut self, id: &str, status: &str, out: &mut Vec<AgentEvent>) {
        if !matches!(status, "completed" | "failed") {
            return;
        }
        let Some(st) = self.tools.remove(id) else { return };
        let ok = status == "completed";
        out.push(AgentEvent::ToolResult { id: id.to_string(), ok, output: st.text() });
        if !ok || !matches!(st.kind.as_str(), "edit" | "delete" | "move") {
            return;
        }
        let kind = if st.kind == "delete" { ChangeKind::Deleted } else { ChangeKind::Modified };
        let mut paths: Vec<PathBuf> = st.diff().map(|(p, _, _)| p).into_iter().collect();
        paths.extend(st.locations.iter().cloned());
        paths.sort();
        paths.dedup();
        for p in paths {
            let path = if p.is_absolute() { p } else { self.cwd.join(p) };
            out.push(AgentEvent::FileChanged { path, kind });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn opts() -> Vec<PermissionOption> {
        vec![
            PermissionOption { id: "a1".into(), name: "Allow".into(), kind: "allow_once".into() },
            PermissionOption { id: "aa".into(), name: "Always".into(), kind: "allow_always".into() },
            PermissionOption { id: "r1".into(), name: "Reject".into(), kind: "reject_once".into() },
        ]
    }

    #[test]
    fn option_choice() {
        assert_eq!(pick_option(&opts(), PermissionChoice::AllowOnce), Some("a1"));
        assert_eq!(pick_option(&opts(), PermissionChoice::AllowAlways), Some("aa"));
        assert_eq!(pick_option(&opts(), PermissionChoice::Deny), Some("r1"));
        assert_eq!(pick_option(&opts(), PermissionChoice::Cancelled), None);
        assert_eq!(pick_option(&opts()[..1], PermissionChoice::AllowAlways), Some("a1"));
        assert_eq!(pick_option(&opts()[..1], PermissionChoice::Deny), None);
    }

    fn reader() -> (Reader, Receiver<AgentEvent>, Receiver<HostRequest>, Receiver<Value>) {
        let (etx, erx) = unbounded();
        let (htx, hrx) = unbounded();
        let (otx, orx) = unbounded();
        let r = Reader {
            events: etx,
            host: htx,
            out: otx,
            pending: Arc::default(),
            tools: HashMap::new(),
            cwd: "/p".into(),
            replaying: Arc::default(),
        };
        (r, erx, hrx, orx)
    }

    #[test]
    fn updates_become_events() {
        let (mut r, erx, _h, _o) = reader();
        let lines = [
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hi"}}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"Edit a.rs","kind":"edit","status":"pending","locations":[{"path":"/p/a.rs"}]}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"done"}}]}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"plan","entries":[{"content":"Step","priority":"high","status":"in_progress"}]}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"usage_update"}}}"#,
            "garbage",
        ];
        for l in lines {
            assert!(r.line(l));
        }
        let ev: Vec<AgentEvent> = erx.try_iter().collect();
        assert_eq!(ev.first(), Some(&AgentEvent::TextDelta { text: "Hi".into() }));
        assert!(ev.iter().any(
            |e| matches!(e, AgentEvent::ToolUse { summary, tool, .. } if summary == "Edit a.rs" && tool == "edit")
        ));
        assert!(ev.contains(&AgentEvent::ToolResult { id: "c1".into(), ok: true, output: "done".into() }));
        assert!(ev.contains(&AgentEvent::FileChanged { path: "/p/a.rs".into(), kind: ChangeKind::Modified }));
        assert!(ev.iter().any(|e| matches!(e, AgentEvent::Plan { entries } if entries.len() == 1)));
        assert_eq!(ev.last(), Some(&AgentEvent::Log { line: "garbage".into() }));
    }

    #[test]
    fn requests_reach_the_host_and_answers_go_out() {
        let (mut r, _e, hrx, orx) = reader();
        r.line(r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c","title":"Write x","kind":"edit","content":[{"type":"diff","path":"/p/x","oldText":"a","newText":"b"}]},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"},{"optionId":"no","name":"No","kind":"reject_once"}]}}"#);
        let Ok(HostRequest::Permission { title, kind, options, responder }) = hrx.try_recv() else {
            panic!("no permission request")
        };
        assert_eq!(title, "Write x");
        assert_eq!(
            kind,
            PermissionKind::FileEdit { path: "/p/x".into(), old_text: Some("a".into()), new_text: "b".into() }
        );
        responder.permission(&options, PermissionChoice::Deny);
        let answer = orx.try_recv().unwrap();
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["outcome"], json!({ "outcome": "selected", "optionId": "no" }));

        r.line(r#"{"jsonrpc":"2.0","id":8,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"e","title":"Run tests","kind":"execute","rawInput":{"command":["cargo","test"]}},"options":[]}}"#);
        let Ok(HostRequest::Permission { kind, responder, .. }) = hrx.try_recv() else { panic!() };
        assert_eq!(kind, PermissionKind::Command { command: "cargo test".into() });
        drop(responder);
        assert!(orx.try_recv().unwrap().get("error").is_some(), "a dropped responder answers with an error");

        r.line(r#"{"jsonrpc":"2.0","id":"r","method":"fs/read_text_file","params":{"sessionId":"s","path":"/p/a","line":2,"limit":1}}"#);
        let Ok(HostRequest::ReadTextFile { line: Some(2), limit: Some(1), responder, .. }) = hrx.try_recv() else {
            panic!("no read request")
        };
        responder.result(json!({ "content": "x" }));
        assert_eq!(orx.try_recv().unwrap(), json!({ "jsonrpc": "2.0", "id": "r", "result": { "content": "x" } }));
        r.line(r#"{"jsonrpc":"2.0","id":9,"method":"terminal/create","params":{}}"#);
        assert_eq!(orx.try_recv().unwrap()["error"]["code"], METHOD_NOT_FOUND);
        r.line(r#"{"jsonrpc":"2.0","id":10,"method":"fs/write_text_file","params":{"path":5}}"#);
        assert!(orx.try_recv().unwrap().get("error").is_some());
    }
}
