//! Debugger actions beyond stepping: Jump to Line (set next statement), Set Value, Smart Step
//! Into (step-into targets), debug console completion, hover evaluation, the value a function
//! returned after Step Out, and what an exception stop threw.
//!
//! Each uses the DAP request when the adapter has it (`gotoTargets`/`goto`, `setVariable`,
//! `stepInTargets`, `completions`, `exceptionInfo`) and, for LLDB adapters without it, the LLDB
//! console (`thread jump`, `thread step-in --step-in-target`, `thread info`).

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_dap::{self as dap, AdapterKind};
use codecraft_syntax::Language;

use super::{DebugConsoleKind, DebugStatus, EvalPurpose, Evaluation, Pending, Step, VariableInfo};
use crate::{CommandError, Session};

/// Completion proposals kept for the console.
const MAX_COMPLETIONS: usize = 200;
const MAX_STEP_TARGETS: usize = 40;

/// A function Smart Step Into can stop in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepTarget {
    /// The adapter's target id (`stepInTargets`); `None` for LLDB's console fallback.
    pub id: Option<i64>,
    pub label: String,
    /// The called function's name (the LLDB fallback steps with it).
    pub function: Option<String>,
}

/// Smart Step Into's chooser.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepTargets {
    /// Still asking the adapter.
    pub pending: bool,
    pub targets: Vec<StepTarget>,
    pub selected: usize,
}

/// One debug console completion: replace chars `start..end` of the input with `text`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEntry {
    pub label: String,
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub kind: Option<String>,
}

/// The debug console's completion list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completions {
    pub text: String,
    /// The caret (a char index into `text`).
    pub column: usize,
    pub pending: bool,
    pub items: Vec<CompletionEntry>,
}

/// What an exception stop threw (`exceptionInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionStop {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub break_mode: String,
}

/// Requests of this module in flight.
#[derive(Clone, Debug)]
pub(crate) enum ActionRequest {
    GotoTargets { thread: i64, line: usize },
    JumpCommand { line: usize },
    SetVariable { parent: i64, name: String, token: Option<u64> },
    SetExpression { expression: String, token: Option<u64> },
    StepInTargets,
    Completions { text: String, column: usize, prefix: usize, token: Option<u64>, retry: bool },
    /// Both belong to stop number `stop` (frame selection doesn't make them stale).
    ReturnValue { stop: usize },
    ExceptionInfo { stop: usize },
    /// An agent's evaluation with children: `value` plus one level of `variables`.
    AgentChildren { token: u64, value: Value },
    /// A step of reading a Rust value LLDB has no summary for.
    RustValue { slot: super::rust_values::Slot, stage: super::rust_values::Stage },
    /// A Python value's summary (a DataFrame's shape, an object's attributes).
    PythonValue { slot: super::python_values::Slot, stage: super::python_values::Stage },
}

impl ActionRequest {
    /// Part of what a stop shows (the UI and agents wait for it).
    pub(crate) fn blocks_settling(&self) -> bool {
        matches!(
            self,
            Self::ReturnValue { .. }
                | Self::ExceptionInfo { .. }
                | Self::GotoTargets { .. }
                | Self::JumpCommand { .. }
                | Self::RustValue { .. }
                | Self::PythonValue { .. }
        )
    }
}

fn paused(s: &Session) -> Result<(), CommandError> {
    if s.debug.is_stopped() { Ok(()) } else { Err(CommandError::Disabled("the program is not paused".into())) }
}

fn not_responding() -> CommandError {
    CommandError::Failed("the debugger is not responding".into())
}

fn send(s: &mut Session, request: ActionRequest, send: impl FnOnce(&dap::DapClient) -> dap::Result<dap::RequestId>) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    let epoch = rt.epoch;
    rt.request(Pending::Action { epoch, request }, send)
}

fn frame_id(s: &Session) -> Option<i64> {
    s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id)
}

// ---------------------------------------------------------------- Jump to Line

/// Jump to Line / Set Next Statement: the selected thread continues from `line` (zero-based) of
/// `path` without running the code in between. Returns how it is done.
pub(crate) fn jump_to_line(s: &mut Session, path: &Path, line: usize) -> Result<&'static str, CommandError> {
    paused(s)?;
    let thread = super::current_thread(s).ok_or_else(|| CommandError::Failed("no thread is selected".into()))?;
    let frame = frame_id(s);
    let rt = s.debug.rt.as_ref().ok_or_else(|| CommandError::Disabled("not debugging".into()))?;
    let adapter_path = rt.to_adapter(path);
    let dap_line = super::line_to_dap(line);
    if rt.capabilities.supports_goto_targets_request {
        if !send(s, ActionRequest::GotoTargets { thread, line }, |c| c.goto_targets(&adapter_path, dap_line)) {
            return Err(not_responding());
        }
        return Ok("goto");
    }
    if matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb) {
        let command = dap::lldb::thread_jump_command(&adapter_path, dap_line);
        if !send(s, ActionRequest::JumpCommand { line }, |c| c.evaluate(&command, frame, "repl")) {
            return Err(not_responding());
        }
        s.debug.push_console(DebugConsoleKind::Input, format!("> {command}"));
        return Ok("thread jump");
    }
    Err(CommandError::Failed(format!("{} cannot jump to a line (it has no goto request)", super::adapter_id(rt.kind))))
}

fn on_goto_targets(s: &mut Session, thread: i64, line: usize, result: Result<Value, String>) {
    let target = result.map(|body| dap::types::parse::<dap::GotoTargetsBody>(&body).unwrap_or_default().targets);
    let target = match target {
        Ok(targets) => targets.into_iter().next(),
        Err(e) => return failed(s, &format!("Jump to Line: {e}"), None),
    };
    let Some(target) = target else {
        return failed(s, &format!("Jump to Line: execution can't jump to line {}", line.saturating_add(1)), None);
    };
    // `goto` stops again (reason `goto`) where it jumped: like a step.
    let previous = s.debug.session.as_ref().and_then(|x| x.status.clone());
    let Some(rt) = s.debug.rt.as_mut() else { return };
    if !rt.request(Pending::Resume, |c| c.goto(thread, target.id)) {
        return failed(s, "the debugger is not responding", None);
    }
    rt.last_stop = previous;
    rt.epoch += 1;
    if let Some(session) = &mut s.debug.session {
        session.status = Some(DebugStatus::Running);
        session.clear_stop();
    }
    s.debug.views.on_resume();
    s.status = format!("Jumped to line {}", line.saturating_add(1));
}

/// Report a failed action (console, status, agent token).
fn failed(s: &mut Session, message: &str, token: Option<u64>) {
    s.debug.error(message.to_owned());
    s.status = message.to_owned();
    s.debug.last_error = Some(message.to_owned());
    if let Some(token) = token {
        super::agent::deliver(s, token, Err(message.to_owned()));
    }
}

// ---------------------------------------------------------------- Set Value

/// Set Value: give the child `name` of `parent` (a variables reference) a new value (an
/// expression in the program's language).
pub(crate) fn set_variable(
    s: &mut Session,
    parent: i64,
    name: &str,
    value: &str,
    token: Option<u64>,
) -> Result<(), CommandError> {
    paused(s)?;
    if value.trim().is_empty() {
        return Err(CommandError::BadParams("the value is empty".into()));
    }
    let rt = s.debug.rt.as_ref().ok_or_else(|| CommandError::Disabled("not debugging".into()))?;
    if !rt.capabilities.supports_set_variable {
        return Err(CommandError::Failed(format!("{} cannot change values", super::adapter_id(rt.kind))));
    }
    let request = ActionRequest::SetVariable { parent, name: name.to_owned(), token };
    if !send(s, request, |c| c.set_variable(parent, name, value)) {
        return Err(not_responding());
    }
    Ok(())
}

/// Set Value of a watch or another expression (`setExpression`).
pub(crate) fn set_expression(s: &mut Session, expression: &str, value: &str, token: Option<u64>) -> Result<(), CommandError> {
    paused(s)?;
    let frame = frame_id(s);
    let rt = s.debug.rt.as_ref().ok_or_else(|| CommandError::Disabled("not debugging".into()))?;
    if !rt.capabilities.supports_set_expression {
        return Err(CommandError::Failed(format!(
            "{} can only change variables (not expressions)",
            super::adapter_id(rt.kind)
        )));
    }
    let request = ActionRequest::SetExpression { expression: expression.to_owned(), token };
    if !send(s, request, |c| c.set_expression(expression, value, frame)) {
        return Err(not_responding());
    }
    Ok(())
}

/// Find a variable by name among the selected frame's loaded scopes: (parent reference, name).
pub(crate) fn find_variable(s: &Session, name: &str) -> Option<(i64, String)> {
    let session = s.debug.session.as_ref()?;
    session.scopes.iter().find_map(|scope| {
        session.variables.get(&scope.reference)?.iter().find(|v| v.name == name).map(|v| (scope.reference, v.name.clone()))
    })
}

fn on_set_value(s: &mut Session, target: SetTarget, token: Option<u64>, result: Result<Value, String>) {
    let body = match result {
        Ok(body) => dap::types::parse::<dap::SetVariableBody>(&body).unwrap_or_default(),
        Err(e) => return failed(s, &format!("Set Value: {e}"), token),
    };
    let rust = s.debug.rt.as_ref().is_some_and(super::Runtime::is_rust);
    let value = if rust { super::rust_value(body.value.clone()) } else { body.value.clone() };
    let mut updated = None;
    if let Some(session) = s.debug.session.as_mut() {
        match &target {
            SetTarget::Variable { parent, name } => {
                let mut stale = None;
                if let Some(v) = session.variables.get_mut(parent).and_then(|vars| vars.iter_mut().find(|v| &v.name == name)) {
                    v.value = value.clone();
                    if let Some(t) = body.type_.clone().filter(|t| !t.is_empty()) {
                        v.type_name = Some(t);
                    }
                    if let Some(r) = body.variables_reference {
                        // Children of the old value are stale.
                        stale = Some(v.reference);
                        v.reference = r;
                    }
                    updated = Some(v.clone());
                }
                if let Some(old) = stale {
                    session.variables.remove(&old);
                }
            }
            SetTarget::Expression(_) => {}
        }
    }
    let label = match &target {
        SetTarget::Variable { name, .. } => name.clone(),
        SetTarget::Expression(e) => e.clone(),
    };
    s.status = format!("{label} = {value}");
    // Other values (watches, inline values, aliases) may have changed: read the frame again.
    super::reload_frame(s);
    if let Some(token) = token {
        let v = updated.map(|v| super::agent::variable_json(&v)).unwrap_or_else(|| json!({ "name": label, "value": value }));
        super::agent::deliver(s, token, Ok(v));
    }
}

enum SetTarget {
    Variable { parent: i64, name: String },
    Expression(String),
}

// ---------------------------------------------------------------- Smart Step Into

/// Smart Step Into: ask which calls on the line can be stepped into. One (or none) steps right
/// away; several open a chooser (`DebugState::step_targets`).
pub(crate) fn smart_step_into(s: &mut Session) -> Result<&'static str, CommandError> {
    paused(s)?;
    let frame = s.debug.session.as_ref().and_then(|x| x.selected_frame()).cloned();
    let rt = s.debug.rt.as_ref().ok_or_else(|| CommandError::Disabled("not debugging".into()))?;
    if rt.capabilities.supports_step_in_targets_request
        && let Some(frame) = &frame
    {
        let id = frame.id;
        if !send(s, ActionRequest::StepInTargets, |c| c.step_in_targets(id)) {
            return Err(not_responding());
        }
        s.debug.step_targets = Some(StepTargets { pending: true, ..StepTargets::default() });
        return Ok("stepInTargets");
    }
    let lldb = matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb);
    let candidates = match (&frame, lldb) {
        (Some(frame), true) => frame
            .path
            .as_ref()
            .and_then(|p| super::line_text(s, p, frame.line).map(|t| (Language::from_path(p), t)))
            .map(|(language, text)| call_candidates(&text, language))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if candidates.len() < 2 {
        super::step(s, Step::Into)?;
        return Ok("stepIn");
    }
    s.debug.step_targets = Some(StepTargets {
        pending: false,
        targets: candidates
            .into_iter()
            .map(|name| StepTarget { id: None, label: format!("{name}()"), function: Some(name) })
            .collect(),
        selected: 0,
    });
    Ok("chooser")
}

fn on_step_in_targets(s: &mut Session, result: Result<Value, String>) {
    let targets: Vec<StepTarget> = match result {
        Ok(body) => dap::types::parse::<dap::StepInTargetsBody>(&body)
            .unwrap_or_default()
            .targets
            .into_iter()
            .take(MAX_STEP_TARGETS)
            .map(|t| StepTarget { id: Some(t.id), label: t.label, function: None })
            .collect(),
        Err(e) => {
            log::debug!("stepInTargets: {e}");
            Vec::new()
        }
    };
    if s.debug.step_targets.is_none() {
        return;
    }
    match targets.as_slice() {
        [] => {
            s.debug.step_targets = None;
            let _ = super::step(s, Step::Into);
        }
        [_] => {
            let _ = step_into_target(s, 0, Some(targets));
        }
        _ => s.debug.step_targets = Some(StepTargets { pending: false, targets, selected: 0 }),
    }
}

/// Step into target `index` of the chooser (or of `targets`).
pub(crate) fn step_into_target(s: &mut Session, index: usize, targets: Option<Vec<StepTarget>>) -> Result<(), CommandError> {
    paused(s)?;
    let targets = match targets {
        Some(t) => t,
        None => s.debug.step_targets.as_ref().map(|t| t.targets.clone()).unwrap_or_default(),
    };
    let target = targets.get(index).cloned().ok_or_else(|| CommandError::BadParams(format!("no step target {index}")))?;
    s.debug.step_targets = None;
    let thread = super::current_thread(s).ok_or_else(|| CommandError::Failed("no thread to step".into()))?;
    let frame = frame_id(s);
    let previous = s.debug.session.as_ref().and_then(|x| x.status.clone());
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    let sent = match (&target.id, &target.function) {
        (Some(id), _) => rt.request(Pending::Resume, |c| c.step_in_target(thread, *id)),
        (None, Some(function)) => {
            let command = dap::lldb::step_in_target_command(function);
            rt.request(Pending::ResumeCommand, |c| c.evaluate(&command, frame, "repl"))
        }
        (None, None) => return Err(CommandError::BadParams("the target has nothing to step into".into())),
    };
    if !sent {
        return Err(not_responding());
    }
    rt.last_stop = previous;
    rt.last_step = Some(Step::Into);
    rt.epoch += 1;
    if let Some(session) = &mut s.debug.session {
        session.status = Some(DebugStatus::Running);
        session.clear_stop();
    }
    s.debug.views.on_resume();
    s.status = format!("Stepping into {}", target.label);
    Ok(())
}

/// The calls on a source line a step into can go to, in order: identifiers directly followed by
/// `(` (methods too), without keywords, macros (`name!(`) and constructors of Rust's
/// `Some`/`Ok`/`Err`.
pub fn call_candidates(line: &str, language: Language) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "if", "while", "for", "match", "return", "switch", "sizeof", "catch", "elif", "and", "or", "not", "in",
        "lambda", "assert", "fn", "def", "loop", "Some", "Ok", "Err", "None", "decltype", "alignof", "typeid",
        "static_assert", "noexcept", "print!", "yield", "await", "del", "with", "except", "unsafe", "move",
    ];
    let code = match language.line_comment() {
        Some(comment) => line.split(comment).next().unwrap_or(line),
        None => line,
    };
    let chars: Vec<char> = code.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut in_string: Option<char> = None;
    while i < chars.len() {
        let c = chars.get(i).copied().unwrap_or(' ');
        if let Some(q) = in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || (c == '\'' && language == Language::Python) {
            in_string = Some(c);
            i += 1;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while chars.get(i).is_some_and(|c| c.is_alphanumeric() || *c == '_') {
                i += 1;
            }
            let word: String = chars.get(start..i).map(|w| w.iter().collect()).unwrap_or_default();
            // Generic arguments between the name and the call: `parse::<i32>(`, `make<T>(`.
            let mut j = i;
            if chars.get(j) == Some(&':') && chars.get(j + 1) == Some(&':') && chars.get(j + 2) == Some(&'<') {
                j += 2;
            }
            if chars.get(j) == Some(&'<') && language != Language::Python {
                let mut depth = 0usize;
                let mut k = j;
                while let Some(&ch) = chars.get(k) {
                    match ch {
                        '<' => depth += 1,
                        '>' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        '(' | ')' | ';' | '{' => break,
                        _ => {}
                    }
                    k += 1;
                }
                if chars.get(k) == Some(&'>') && chars.get(k + 1) == Some(&'(') {
                    j = k + 1;
                }
            }
            let is_call = chars.get(j) == Some(&'(');
            let is_macro = chars.get(i) == Some(&'!');
            if is_call && !is_macro && !KEYWORDS.contains(&word.as_str()) && !out.contains(&word) {
                out.push(word);
            }
            continue;
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------- console completion

/// Ask for completions of the debug console's `text` with the caret at char `column`.
pub(crate) fn console_complete(s: &mut Session, text: &str, column: usize, token: Option<u64>) -> Result<(), CommandError> {
    let frame = frame_id(s);
    let rt = s.debug.rt.as_ref().filter(|rt| rt.client().is_some()).ok_or_else(|| CommandError::Disabled("the debugger is not running".into()))?;
    if !rt.capabilities.supports_completions_request {
        return Err(CommandError::Failed(format!("{} has no console completion", super::adapter_id(rt.kind))));
    }
    let column = column.min(text.chars().count());
    let utf16: usize = text.chars().take(column).map(char::len_utf16).sum();
    let dap_column = i64::try_from(utf16).unwrap_or(i64::MAX - 1).saturating_add(1);
    let request = ActionRequest::Completions { text: text.to_owned(), column, prefix: 0, token, retry: true };
    let owned = text.to_owned();
    if !send(s, request, |c| c.completions(&owned, dap_column, frame)) {
        return Err(not_responding());
    }
    s.debug.completions = Some(Completions { text: text.to_owned(), column, pending: true, items: Vec::new() });
    Ok(())
}

/// The char index of a UTF-16 offset into `text`.
fn char_index(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.chars().enumerate() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    text.chars().count()
}

/// The start of the word before char `column` (identifier characters).
fn word_start(text: &str, column: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut start = column.min(chars.len());
    while start > 0 && chars.get(start - 1).is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$') {
        start -= 1;
    }
    start
}

/// Turn a `completions` body into entries for `text` (as sent, `prefix` chars longer than the
/// user's input when CodeCraft added `p `).
pub(crate) fn completion_entries(text: &str, column: usize, prefix: usize, body: &dap::CompletionsBody) -> Vec<CompletionEntry> {
    let sent: String = if prefix > 0 { format!("{}{text}", "p ".get(..prefix).unwrap_or("")) } else { text.to_owned() };
    let sent_column = column + prefix;
    let mut out: Vec<CompletionEntry> = Vec::new();
    for item in body.targets.iter().take(MAX_COMPLETIONS) {
        let insert = item.text.clone().unwrap_or_else(|| item.label.clone());
        if insert.is_empty() {
            continue;
        }
        let (start, end) = match item.start {
            // `columnsStartAt1`: 1-based UTF-16 offsets.
            Some(start) => {
                let start = char_index(&sent, usize::try_from(start.saturating_sub(1)).unwrap_or(0));
                let length = item.length.and_then(|l| usize::try_from(l).ok()).unwrap_or(0);
                let end = char_index(&sent, sent.chars().take(start).map(char::len_utf16).sum::<usize>() + length);
                (start, end.max(start))
            }
            None => (word_start(&sent, sent_column), sent_column),
        };
        // Back to the user's text (without the added prefix); clamp to it.
        let start = start.saturating_sub(prefix).min(column);
        let end = end.saturating_sub(prefix).clamp(start, text.chars().count());
        let label = item.label.split(" -- ").next().unwrap_or(&item.label).to_owned();
        if out.iter().any(|e| e.text == insert && e.start == start) {
            continue;
        }
        out.push(CompletionEntry { label, text: insert, start, end, kind: item.type_.clone() });
    }
    out
}

fn on_completions(
    s: &mut Session,
    text: String,
    column: usize,
    prefix: usize,
    token: Option<u64>,
    retry: bool,
    result: Result<Value, String>,
) {
    let body = match result {
        Ok(body) => dap::types::parse::<dap::CompletionsBody>(&body).unwrap_or_default(),
        Err(e) => {
            if let Some(c) = &mut s.debug.completions {
                c.pending = false;
            }
            if let Some(token) = token {
                super::agent::deliver(s, token, Err(e));
            }
            return;
        }
    };
    let items = completion_entries(&text, column, prefix, &body);
    // LLDB completes debugger commands; an expression completes after `p `.
    let lldb = s.debug.rt.as_ref().is_some_and(|rt| matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb));
    if items.is_empty() && retry && lldb && !text.trim().is_empty() {
        let frame = frame_id(s);
        let sent = format!("p {text}");
        let utf16: usize = sent.chars().take(column + 2).map(char::len_utf16).sum();
        let dap_column = i64::try_from(utf16).unwrap_or(i64::MAX - 1).saturating_add(1);
        let request = ActionRequest::Completions { text, column, prefix: 2, token, retry: false };
        if send(s, request, |c| c.completions(&sent, dap_column, frame)) {
            return;
        }
        return;
    }
    if let Some(c) = &mut s.debug.completions
        && c.text == text
        && c.column == column
    {
        c.pending = false;
        c.items = items.clone();
    }
    if let Some(token) = token {
        let list: Vec<Value> = items.iter().map(|e| json!({ "label": e.label, "text": e.text, "start": e.start, "end": e.end, "kind": e.kind })).collect();
        super::agent::deliver(s, token, Ok(json!({ "text": text, "column": column, "items": list })));
    }
}

// ---------------------------------------------------------------- hover

/// Evaluate the expression under the mouse in the selected frame (`hover` context). The answer
/// lands in `DebugState::hover`; asking again for the same expression in the same stop is free.
pub(crate) fn hover(s: &mut Session, expression: &str) -> Result<(), CommandError> {
    let expression = expression.trim();
    if expression.is_empty() || expression.len() > 500 {
        return Err(CommandError::BadParams("give a short expression".into()));
    }
    paused(s)?;
    let epoch = s.debug.rt.as_ref().map_or(0, |rt| rt.epoch);
    if let Some(h) = &s.debug.hover
        && h.expression == expression
        && s.debug.hover_epoch == epoch
    {
        return Ok(());
    }
    let frame = frame_id(s);
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    let context = if rt.capabilities.supports_evaluate_for_hovers { "hover" } else { "watch" };
    let purpose = EvalPurpose::Hover(expression.to_owned());
    if !rt.request(Pending::Evaluate { epoch, purpose }, |c| c.evaluate(expression, frame, context)) {
        return Err(not_responding());
    }
    s.debug.hover = Some(Evaluation { expression: expression.to_owned(), result: Err(String::new()), pending: true });
    s.debug.hover_epoch = epoch;
    Ok(())
}

// ---------------------------------------------------------------- after a stop

/// After a stop: the return value of a Step Out (LLDB's `thread info`) and an exception's
/// details (`exceptionInfo`).
pub(crate) fn after_stop(s: &mut Session, reason: &str, thread: Option<i64>, last_step: Option<Step>) {
    let Some(rt) = s.debug.rt.as_ref() else { return };
    let lldb = matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb);
    let exception_info = rt.capabilities.supports_exception_info_request;
    let stop = rt.stops;
    if lldb && last_step == Some(Step::Out) && reason == "step" {
        send(s, ActionRequest::ReturnValue { stop }, |c| c.evaluate(dap::lldb::THREAD_INFO_COMMAND, None, "repl"));
    }
    if reason == "exception"
        && exception_info
        && let Some(thread) = thread
    {
        send(s, ActionRequest::ExceptionInfo { stop }, |c| c.exception_info(thread));
    }
}

/// debugpy lists the value a function returned among the locals (`(return) name`).
pub(crate) fn pick_return_value(s: &mut Session, locals: &[VariableInfo]) {
    let Some(v) = locals.iter().find(|v| v.name.starts_with("(return)")) else { return };
    if let Some(session) = s.debug.session.as_mut() {
        session.return_value = Some(v.clone());
    }
}

fn on_return_value(s: &mut Session, result: Result<Value, String>) {
    let Ok(body) = result else { return };
    let output = dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default().result;
    let Some((type_name, value)) = dap::lldb::parse_return_value(&output) else { return };
    let rust = s.debug.rt.as_ref().is_some_and(super::Runtime::is_rust);
    let function = s.debug.rt.as_ref().and_then(|rt| rt.step_from_function.clone()).unwrap_or_default();
    if let Some(session) = s.debug.session.as_mut() {
        session.return_value = Some(VariableInfo {
            name: if function.is_empty() { "(return)".into() } else { format!("(return) {function}") },
            value: if rust { super::rust_value(value) } else { value },
            type_name,
            reference: 0,
            evaluate_name: None,
            memory_reference: None,
        });
    }
}

fn on_exception_info(s: &mut Session, result: Result<Value, String>) {
    let Ok(body) = result else { return };
    let info: dap::ExceptionInfoBody = dap::types::parse(&body).unwrap_or_default();
    let details = info.details.unwrap_or_default();
    if let Some(session) = s.debug.session.as_mut() {
        session.exception = Some(ExceptionStop {
            id: info.exception_id,
            description: info.description.filter(|d| !d.is_empty()),
            type_name: details.type_name.or(details.full_type_name),
            message: details.message,
            break_mode: info.break_mode,
        });
    }
}

// ---------------------------------------------------------------- answers

/// Still paused at stop number `stop`.
fn same_stop(s: &Session, stop: usize) -> bool {
    s.debug.is_stopped() && s.debug.rt.as_ref().is_some_and(|rt| rt.stops == stop)
}

pub(crate) fn on_response(s: &mut Session, current: bool, request: ActionRequest, result: Result<Value, String>) {
    match request {
        ActionRequest::GotoTargets { thread, line } => {
            if current {
                on_goto_targets(s, thread, line, result);
            }
        }
        ActionRequest::JumpCommand { line } => {
            let error = match &result {
                Ok(body) => dap::types::parse::<dap::EvaluateBody>(body).ok().and_then(|b| dap::lldb::error_of(&b.result)),
                Err(e) => Some(e.clone()),
            };
            match error {
                Some(e) => failed(s, &format!("Jump to Line: {e}"), None),
                None => s.status = format!("Jumped to line {}", line.saturating_add(1)),
            }
            super::refresh_stack(s);
        }
        ActionRequest::SetVariable { parent, name, token } => {
            on_set_value(s, SetTarget::Variable { parent, name }, token, result);
        }
        ActionRequest::SetExpression { expression, token } => {
            on_set_value(s, SetTarget::Expression(expression), token, result);
        }
        ActionRequest::StepInTargets => {
            if current {
                on_step_in_targets(s, result);
            } else {
                s.debug.step_targets = None;
            }
        }
        ActionRequest::Completions { text, column, prefix, token, retry } => {
            on_completions(s, text, column, prefix, token, retry, result);
        }
        ActionRequest::ReturnValue { stop } => {
            if same_stop(s, stop) {
                on_return_value(s, result);
            }
        }
        ActionRequest::ExceptionInfo { stop } => {
            if same_stop(s, stop) {
                on_exception_info(s, result);
            }
        }
        ActionRequest::RustValue { slot, stage } => super::rust_values::on_response(s, current, slot, stage, result),
        ActionRequest::PythonValue { slot, stage } => super::python_values::on_response(s, current, slot, stage, result),
        ActionRequest::AgentChildren { token, mut value } => {
            let rust = s.debug.rt.as_ref().is_some_and(super::Runtime::is_rust);
            let children: Vec<Value> = match result {
                Ok(body) => dap::types::parse::<dap::VariablesBody>(&body)
                    .unwrap_or_default()
                    .variables
                    .into_iter()
                    .take(100)
                    .map(|v| {
                        super::agent::variable_json(&VariableInfo {
                            name: v.name,
                            value: if rust { super::rust_value(v.value) } else { v.value },
                            type_name: v.type_.filter(|t| !t.is_empty()),
                            reference: v.variables_reference,
                            evaluate_name: v.evaluate_name,
                            memory_reference: None,
                        })
                    })
                    .collect(),
                Err(e) => vec![json!({ "error": e })],
            };
            if let Some(map) = value.as_object_mut() {
                map.insert("children".into(), children.into());
            }
            super::agent::deliver(s, token, Ok(value));
        }
    }
}

/// Fetch one level of children of `reference` for an agent (`debug.variables`).
pub(crate) fn agent_children(s: &mut Session, reference: i64, token: u64) -> Result<(), CommandError> {
    paused(s)?;
    if reference <= 0 {
        return Err(CommandError::BadParams("`reference` must be a positive variables reference".into()));
    }
    let request = ActionRequest::AgentChildren { token, value: json!({ "reference": reference }) };
    if !send(s, request, |c| c.variables(reference)) {
        return Err(not_responding());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_candidates_find_calls_on_a_line() {
        assert_eq!(call_candidates("    let t2 = add(twice(3), 4);", Language::Rust), ["add", "twice"]);
        assert_eq!(call_candidates("    let n = s.trim().parse::<i32>().unwrap();", Language::Rust), ["trim", "parse", "unwrap"]);
        assert_eq!(call_candidates("    println!(\"{}\", f(x)); // g(y)", Language::Rust), ["f"]);
        assert_eq!(call_candidates("    if (check(a)) return make<int>(b);", Language::Cpp), ["check", "make"]);
        assert_eq!(call_candidates("    result = work(helper(4)) if ok('f(x)') else None", Language::Python), ["work", "helper", "ok"]);
        assert_eq!(call_candidates("    let v = Some(f(1));", Language::Rust), ["f"]);
        assert!(call_candidates("", Language::Rust).is_empty());
        assert!(call_candidates("\"unterminated (", Language::Rust).is_empty());
        assert_eq!(call_candidates("a<<b(c)", Language::Cpp), ["b"]);
    }

    #[test]
    fn completion_entries_replace_the_right_range() {
        let body = dap::CompletionsBody {
            targets: vec![
                dap::CompletionItem { label: "total".into(), start: Some(1), length: Some(2), ..Default::default() },
                dap::CompletionItem { label: "tally".into(), ..Default::default() },
                dap::CompletionItem { label: "".into(), ..Default::default() },
            ],
        };
        let items = completion_entries("to", 2, 0, &body);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].start, items[0].end, items[0].text.as_str()), (0, 2, "total"));
        assert_eq!((items[1].start, items[1].end), (0, 2));
        // LLDB command labels drop their help text; an added `p ` prefix is taken off again.
        let lldb = dap::CompletionsBody {
            targets: vec![dap::CompletionItem { label: "a -- int &".into(), text: Some("a".into()), ..Default::default() }],
        };
        let items = completion_entries("x + a", 5, 2, &lldb);
        assert_eq!((items[0].label.as_str(), items[0].start, items[0].end), ("a", 4, 5));
        // Hostile offsets are clamped.
        let bad = dap::CompletionsBody {
            targets: vec![dap::CompletionItem { label: "z".into(), start: Some(-40), length: Some(i64::MAX), ..Default::default() }],
        };
        let items = completion_entries("é", 1, 0, &bad);
        assert!(items[0].start <= items[0].end && items[0].end <= 1);
        assert_eq!(word_start("a.bc", 4), 2);
        assert_eq!(char_index("é€x", 2), 2);
    }
}
