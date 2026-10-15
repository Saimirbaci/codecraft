//! The debugger for agents: a compact stop summary (reason, thread, top frames with file:line
//! and source excerpts, locals, return value, exception) and asynchronous answers without
//! blocking anything. A command that has to wait for the adapter (wait for a stop, evaluate,
//! set a value, complete) returns `{pending: true, token}`; `debug.poll {token}` (or the same
//! command with `token`) returns the answer once the pump has received it, `{pending: true}`
//! until then, and `{timedOut: true}` after the deadline. Tokens are dropped once answered.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{ConsoleLine, DebugConsoleKind, DebugSession, DebugStatus, VariableInfo, console_text};
use crate::{CommandError, Session};

/// Tokens kept at once (the oldest go first).
const MAX_OPS: usize = 64;
/// Longest wait an agent may ask for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_VALUE_CHARS: usize = 240;
const EXCERPT_RADIUS: usize = 3;

#[derive(Clone, Debug)]
enum OpKind {
    /// Until the program stops (and its stack and locals are read) or the session ends.
    Stop,
    /// An adapter answer delivered by `deliver`.
    Answer,
}

#[derive(Clone, Debug)]
struct Op {
    kind: OpKind,
    deadline: Instant,
    result: Option<Result<Value, String>>,
}

/// Outstanding agent requests.
#[derive(Debug)]
pub(crate) struct AgentState {
    next: u64,
    ops: BTreeMap<u64, Op>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self { next: 1, ops: BTreeMap::new() }
    }
}

/// A timeout from `{timeoutMs}` (default 30 s, at most 10 min).
pub(crate) fn timeout_param(p: &Value) -> Result<Duration, CommandError> {
    match p.get("timeoutMs") {
        None | Some(Value::Null) => Ok(DEFAULT_TIMEOUT),
        Some(v) => v
            .as_u64()
            .map(|ms| Duration::from_millis(ms).min(MAX_TIMEOUT))
            .ok_or_else(|| CommandError::BadParams("`timeoutMs` must be a non-negative integer".into())),
    }
}

fn new_op(s: &mut Session, kind: OpKind, timeout: Duration) -> u64 {
    let agent = &mut s.debug.agent;
    while agent.ops.len() >= MAX_OPS {
        let Some(oldest) = agent.ops.keys().next().copied() else { break };
        agent.ops.remove(&oldest);
    }
    let token = agent.next;
    agent.next = agent.next.saturating_add(1);
    let deadline = Instant::now().checked_add(timeout).unwrap_or_else(Instant::now);
    agent.ops.insert(token, Op { kind, deadline, result: None });
    token
}

/// A token for an adapter answer (`deliver` fills it).
pub(crate) fn answer_token(s: &mut Session, timeout: Duration) -> u64 {
    new_op(s, OpKind::Answer, timeout)
}

/// The answer for `token` arrived.
pub(crate) fn deliver(s: &mut Session, token: u64, result: Result<Value, String>) {
    if let Some(op) = s.debug.agent.ops.get_mut(&token) {
        op.result = Some(result);
    }
}

/// `debug.waitForStop {timeoutMs}`: the stop summary now if the program is paused (with its
/// stack read) or the session is over, else a token.
pub(crate) fn wait_for_stop(s: &mut Session, timeout: Duration) -> Value {
    if stop_ready(s) {
        return done(stop_summary(s, 10, 40));
    }
    let token = new_op(s, OpKind::Stop, timeout);
    json!({ "pending": true, "token": token, "state": state_name(s) })
}

/// Paused with everything read, or over (or never started). Before-launch tasks still running
/// (the session starts after them) are waited for.
fn stop_ready(s: &Session) -> bool {
    if s.launch.pending.is_some() {
        return false;
    }
    let Some(session) = &s.debug.session else { return true };
    match session.status() {
        DebugStatus::Exited { .. } | DebugStatus::Failed { .. } => true,
        DebugStatus::Stopped { .. } => !s.debug.settling(),
        _ => !s.debug.busy() && s.debug.rt.is_none(),
    }
}

fn done(mut v: Value) -> Value {
    if let Some(map) = v.as_object_mut() {
        map.insert("pending".into(), false.into());
    }
    v
}

/// `debug.poll {token}`: the answer, `{pending: true}` or `{timedOut: true}`.
pub(crate) fn poll(s: &mut Session, token: u64) -> Result<Value, CommandError> {
    let op = s
        .debug
        .agent
        .ops
        .get(&token)
        .cloned()
        .ok_or_else(|| CommandError::BadParams(format!("unknown or already answered token {token}")))?;
    let answer = match op.kind {
        OpKind::Stop if stop_ready(s) => Some(done(stop_summary(s, 10, 40))),
        OpKind::Stop => None,
        OpKind::Answer => match op.result {
            Some(Ok(v)) => Some(done(v)),
            Some(Err(e)) => Some(json!({ "pending": false, "error": e })),
            None if s.debug.rt.is_none() => {
                Some(json!({ "pending": false, "error": "the debug session ended before the debugger answered" }))
            }
            None => None,
        },
    };
    if let Some(answer) = answer {
        s.debug.agent.ops.remove(&token);
        return Ok(answer);
    }
    if Instant::now() >= op.deadline {
        s.debug.agent.ops.remove(&token);
        let mut v = json!({ "pending": false, "timedOut": true, "state": state_name(s) });
        if matches!(op.kind, OpKind::Stop)
            && let Some(map) = v.as_object_mut()
        {
            map.insert("summary".into(), stop_summary(s, 3, 0));
        }
        return Ok(v);
    }
    Ok(json!({ "pending": true, "token": token, "state": state_name(s) }))
}

fn state_name(s: &Session) -> &'static str {
    match s.debug.session.as_ref().map(DebugSession::status) {
        None => "idle",
        Some(DebugStatus::Building) => "building",
        Some(DebugStatus::Starting) => "starting",
        Some(DebugStatus::Running) => "running",
        Some(DebugStatus::Stopped { .. }) => "stopped",
        Some(DebugStatus::Exited { .. }) => "exited",
        Some(DebugStatus::Failed { .. }) => "failed",
    }
}

fn short(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_VALUE_CHARS).collect();
    if text.chars().count() > MAX_VALUE_CHARS {
        out.push('…');
    }
    out
}

/// A variable as agents see it.
pub(crate) fn variable_json(v: &VariableInfo) -> Value {
    let mut out = json!({ "name": v.name, "value": short(&v.value) });
    if let Some(map) = out.as_object_mut() {
        if let Some(t) = &v.type_name {
            map.insert("type".into(), t.clone().into());
        }
        if v.reference > 0 {
            map.insert("reference".into(), v.reference.into());
        }
    }
    out
}

fn line_of(s: &Session, path: &std::path::Path, line: usize) -> Option<String> {
    super::line_text(s, path, line).map(|t| short(&t))
}

/// A few source lines around `line` (zero-based) with 1-based numbers.
fn excerpt(s: &Session, path: &std::path::Path, line: usize) -> Vec<Value> {
    let text = match s.ws.doc_by_path(path).and_then(|d| s.ws.doc(d)) {
        Some(d) => d.doc.text(),
        None => match codecraft_fs::read_text_file(path) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        },
    };
    let from = line.saturating_sub(EXCERPT_RADIUS);
    text.lines()
        .enumerate()
        .skip(from)
        .take(EXCERPT_RADIUS * 2 + 1)
        .map(|(i, t)| {
            let mut v = json!({ "line": i + 1, "text": short(t.trim_end()) });
            if i == line
                && let Some(map) = v.as_object_mut()
            {
                map.insert("current".into(), true.into());
            }
            v
        })
        .collect()
}

/// The debugger's state in a form an agent can act on: why it stopped, where (top frames with
/// 1-based lines and the source), the selected frame's locals, a returned value, an exception,
/// the last program output and the last error.
pub(crate) fn stop_summary(s: &Session, max_frames: usize, max_locals: usize) -> Value {
    let debug = &s.debug;
    let mut out = serde_json::Map::new();
    out.insert("state".into(), state_name(s).into());
    let Some(session) = &debug.session else {
        out.insert("breakpoints".into(), debug.breakpoints.len().into());
        return Value::Object(out);
    };
    out.insert("session".into(), session.name.clone().into());
    if !session.adapter.is_empty() {
        out.insert("adapter".into(), session.adapter.clone().into());
    }
    if let Some(label) = debug.rt.as_ref().and_then(super::children::focused_label) {
        out.insert("process".into(), label.into());
    }
    if session.processes.len() > 1 {
        out.insert("processes".into(), json!(session.processes));
    }
    match session.status() {
        DebugStatus::Stopped { reason, description, thread } => {
            out.insert("reason".into(), reason.into());
            if let Some(d) = description.filter(|d| !d.is_empty()) {
                out.insert("description".into(), d.into());
            }
            let thread = thread.or(session.thread);
            if let Some(id) = thread {
                let name = session.threads.iter().find(|t| t.id == id).map(|t| t.name.clone()).unwrap_or_default();
                out.insert("thread".into(), json!({ "id": id, "name": name }));
            }
            if session.threads.len() > 1 {
                out.insert("threadCount".into(), session.threads.len().into());
            }
            out.insert("settling".into(), debug.settling().into());
        }
        DebugStatus::Exited { code } => {
            out.insert("exitCode".into(), json!(code));
        }
        DebugStatus::Failed { message } => {
            out.insert("error".into(), message.into());
        }
        _ => {}
    }
    if !session.hit_breakpoints.is_empty() {
        let hit: Vec<Value> = session
            .hit_breakpoints
            .iter()
            .filter_map(|id| debug.breakpoints.iter().find(|b| b.id == *id))
            .map(|b| json!({ "id": b.id, "path": b.path, "line": b.line + 1 }))
            .collect();
        if !hit.is_empty() {
            out.insert("breakpoints".into(), hit.into());
        }
    }
    if let Some(e) = &session.exception {
        out.insert("exception".into(), json!(e));
    } else if matches!(session.status(), DebugStatus::Stopped { reason, .. } if reason == "panic")
        && let Some(message) = panic_message(debug)
    {
        out.insert("exception".into(), json!({ "id": "panic", "typeName": "panic", "message": message }));
    }
    if let Some(v) = &session.return_value {
        out.insert("returnValue".into(), variable_json(v));
    }
    if !session.frames.is_empty() {
        let frames: Vec<Value> = session
            .frames
            .iter()
            .enumerate()
            .take(max_frames)
            .map(|(i, f)| {
                let mut v = json!({ "index": i, "function": f.name });
                if let Some(map) = v.as_object_mut() {
                    if let Some(p) = &f.path {
                        map.insert("path".into(), json!(p));
                        map.insert("line".into(), (f.line + 1).into());
                        if let Some(code) = line_of(s, p, f.line).filter(|c| !c.is_empty()) {
                            map.insert("code".into(), code.into());
                        }
                    }
                    if f.subtle {
                        map.insert("library".into(), true.into());
                    }
                    if i == session.frame {
                        map.insert("selected".into(), true.into());
                    }
                }
                v
            })
            .collect();
        out.insert("frames".into(), frames.into());
        if session.frames.len() > max_frames {
            out.insert("moreFrames".into(), (session.frames.len() - max_frames).into());
        }
    }
    if let Some((path, line)) = &session.execution {
        out.insert("source".into(), json!({ "path": path, "line": line + 1, "lines": excerpt(s, path, *line) }));
    }
    if max_locals > 0 {
        let mut locals = Vec::new();
        for scope in &session.scopes {
            let Some(vars) = session.variables.get(&scope.reference) else { continue };
            for v in vars {
                if locals.len() >= max_locals {
                    break;
                }
                locals.push(variable_json(v));
            }
            // The first scope is the locals; arguments and others only when they are separate.
            if !matches!(scope.presentation_hint.as_deref(), Some("arguments")) {
                break;
            }
        }
        if !locals.is_empty() {
            out.insert("locals".into(), locals.into());
        }
    }
    let output: Vec<&str> = debug
        .console
        .iter()
        .rev()
        .filter(|l: &&ConsoleLine| matches!(l.kind, DebugConsoleKind::Stdout | DebugConsoleKind::Stderr))
        .take(8)
        .map(console_text)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !output.is_empty() {
        out.insert("output".into(), json!(output));
    }
    if let Some(e) = &debug.last_error {
        out.insert("lastError".into(), e.clone().into());
    }
    Value::Object(out)
}

/// The last Rust panic message in the program's output (`thread 'main' panicked at src/main.rs:3:5:`
/// and the message line after it).
fn panic_message(debug: &super::DebugState) -> Option<String> {
    let lines: Vec<&str> = debug.console.iter().rev().take(200).map(console_text).collect::<Vec<_>>().into_iter().rev().collect();
    let at = lines.iter().rposition(|l| l.contains("panicked at "))?;
    let location = lines.get(at)?.split("panicked at ").nth(1)?.trim_end_matches(':').trim();
    match lines.get(at + 1).map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with("note:")) {
        Some(message) => Some(short(&format!("{message} ({location})"))),
        None => Some(short(&format!("panicked at {location}"))),
    }
}

/// The breakpoints as agents see them (1-based lines).
pub(crate) fn breakpoint_json(b: &super::Breakpoint) -> Value {
    let mut v = json!({ "id": b.id, "path": b.path, "line": b.line + 1, "enabled": b.enabled });
    if let Some(map) = v.as_object_mut() {
        for (key, value) in [("condition", &b.condition), ("hitCondition", &b.hit_condition), ("logMessage", &b.log_message)] {
            if let Some(value) = value {
                map.insert(key.into(), value.clone().into());
            }
        }
        if b.temporary {
            map.insert("temporary".into(), true.into());
        }
        if let Some(verified) = b.verified {
            map.insert("verified".into(), verified.into());
        }
        if let Some(m) = &b.message {
            map.insert("message".into(), m.clone().into());
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_answer_once_and_time_out() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        // No session: a stop wait answers at once.
        let v = wait_for_stop(&mut s, DEFAULT_TIMEOUT);
        assert_eq!(v["pending"], json!(false));
        assert_eq!(v["state"], json!("idle"));
        let token = answer_token(&mut s, Duration::from_secs(60));
        deliver(&mut s, token, Ok(json!({ "value": "6" })));
        let v = poll(&mut s, token).unwrap();
        assert_eq!((v["pending"].clone(), v["value"].clone()), (json!(false), json!("6")));
        assert!(poll(&mut s, token).is_err(), "answered tokens are gone");
        let token = answer_token(&mut s, Duration::ZERO);
        let v = poll(&mut s, token).unwrap();
        assert_eq!(v["pending"], json!(false));
        assert!(v.get("error").is_some() || v["timedOut"] == json!(true));
        assert!(poll(&mut s, 999_999).is_err());
        for _ in 0..(MAX_OPS * 2) {
            answer_token(&mut s, DEFAULT_TIMEOUT);
        }
        assert!(s.debug.agent.ops.len() <= MAX_OPS);
        assert!(timeout_param(&json!({ "timeoutMs": -1 })).is_err());
        assert_eq!(timeout_param(&json!({ "timeoutMs": u64::MAX })).unwrap(), MAX_TIMEOUT);
    }

    #[test]
    fn values_are_shortened() {
        let v = VariableInfo {
            name: "s".into(),
            value: "x".repeat(1000),
            type_name: Some("String".into()),
            reference: 3,
            evaluate_name: None,
            memory_reference: None,
        };
        let j = variable_json(&v);
        assert!(j["value"].as_str().unwrap().chars().count() <= MAX_VALUE_CHARS + 1);
        assert_eq!(j["reference"], json!(3));
    }
}
