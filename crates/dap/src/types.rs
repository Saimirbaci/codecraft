//! The subset of Debug Adapter Protocol types CodeCraft uses (see the public DAP specification,
//! <https://microsoft.github.io/debug-adapter-protocol/specification>).
//!
//! Deserialization is lenient: every field has a default, unknown fields are ignored, so an
//! adapter that leaves something out (or adds something) still parses. Lines and columns are
//! **1-based**, as CodeCraft asks for in `initialize` (`linesStartAt1`, `columnsStartAt1`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Parse a message body into a typed value; a malformed body becomes an error string.
pub fn parse<T: for<'de> Deserialize<'de>>(body: &Value) -> Result<T, String> {
    // `null` or a missing body is treated like `{}` so all-default types still parse.
    let body = if body.is_null() { &Value::Object(serde_json::Map::new()) } else { body };
    T::deserialize(body).map_err(|e| e.to_string())
}

/// What an adapter supports (`initialize` response body). Only the flags CodeCraft reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Capabilities {
    pub supports_configuration_done_request: bool,
    pub supports_function_breakpoints: bool,
    pub supports_conditional_breakpoints: bool,
    pub supports_hit_conditional_breakpoints: bool,
    pub supports_evaluate_for_hovers: bool,
    pub supports_log_points: bool,
    pub supports_terminate_request: bool,
    pub supports_set_variable: bool,
    pub supports_step_in_targets_request: bool,
    pub supports_goto_targets_request: bool,
    pub supports_stepping_granularity: bool,
    pub supports_single_thread_execution_requests: bool,
    pub support_terminate_debuggee: bool,
    pub supports_value_formatting_options: bool,
    pub supports_read_memory_request: bool,
    pub supports_write_memory_request: bool,
    pub supports_disassemble_request: bool,
    pub supports_data_breakpoints: bool,
    pub supports_instruction_breakpoints: bool,
    /// `restartFrame` (Run › Debugging Actions › Reset Frame).
    pub supports_restart_frame: bool,
    /// `completions` (debug console completion).
    pub supports_completions_request: bool,
    /// `exceptionInfo` (what was thrown, for the stop summary).
    pub supports_exception_info_request: bool,
    /// `setExpression` (Set Value on a watch or an evaluated expression).
    pub supports_set_expression: bool,
    pub exception_breakpoint_filters: Vec<ExceptionBreakpointsFilter>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExceptionBreakpointsFilter {
    pub filter: String,
    pub label: String,
    pub default: bool,
}

/// A source file reference.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Source {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    /// Non-zero when the content must be fetched with a `source` request (no file on disk).
    pub source_reference: Option<i64>,
    /// `normal`, `emphasize` or `deemphasize` (library code).
    pub presentation_hint: Option<String>,
}

impl Source {
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        Self { name, path: Some(path), ..Self::default() }
    }
}

/// A breakpoint to set in a source file (`setBreakpoints` arguments).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SourceBreakpoint {
    pub line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

/// A breakpoint on a function name (`setFunctionBreakpoints` arguments).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FunctionBreakpoint {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

/// A breakpoint as the adapter reports it (in responses and `breakpoint` events).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: Option<i64>,
    pub verified: bool,
    pub message: Option<String>,
    pub source: Option<Source>,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BreakpointsBody {
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thread {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadsBody {
    pub threads: Vec<Thread>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    pub source: Option<Source>,
    pub line: i64,
    pub column: i64,
    pub end_line: Option<i64>,
    pub instruction_pointer_reference: Option<String>,
    pub module_id: Option<Value>,
    /// `normal`, `label` or `subtle`.
    pub presentation_hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StackTraceBody {
    pub stack_frames: Vec<StackFrame>,
    pub total_frames: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Scope {
    pub name: String,
    pub presentation_hint: Option<String>,
    pub variables_reference: i64,
    pub named_variables: Option<i64>,
    pub indexed_variables: Option<i64>,
    pub expensive: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopesBody {
    pub scopes: Vec<Scope>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Variable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    /// Non-zero when the variable has children (fetch them with `variables`).
    pub variables_reference: i64,
    pub named_variables: Option<i64>,
    pub indexed_variables: Option<i64>,
    pub evaluate_name: Option<String>,
    /// The value's memory location (adapters send it when the client supports memory references).
    pub memory_reference: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VariablesBody {
    pub variables: Vec<Variable>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EvaluateBody {
    pub result: String,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub variables_reference: i64,
    pub named_variables: Option<i64>,
    pub indexed_variables: Option<i64>,
    pub memory_reference: Option<String>,
}

/// `readMemory` response body. `data` is base64; bytes after it up to `unreadableBytes` could not
/// be read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReadMemoryBody {
    pub address: String,
    pub unreadable_bytes: Option<i64>,
    pub data: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WriteMemoryBody {
    pub offset: Option<i64>,
    pub bytes_written: Option<i64>,
}

/// One instruction of a `disassemble` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisassembledInstruction {
    pub address: String,
    pub instruction_bytes: Option<String>,
    pub instruction: String,
    pub symbol: Option<String>,
    pub location: Option<Source>,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub end_line: Option<i64>,
    /// `normal` or `invalid` (padding the adapter could not decode).
    pub presentation_hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisassembleBody {
    pub instructions: Vec<DisassembledInstruction>,
}

/// `dataBreakpointInfo` response body: `dataId` is `None` when the value can't be watched
/// (`description` says why).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DataBreakpointInfoBody {
    pub data_id: Option<String>,
    pub description: String,
    /// `read`, `write`, `readWrite`.
    pub access_types: Option<Vec<String>>,
    pub can_persist: Option<bool>,
}

/// A watchpoint (`setDataBreakpoints` arguments).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DataBreakpoint {
    pub data_id: String,
    /// `read`, `write` or `readWrite`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

/// A place execution can jump to (`gotoTargets` response).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GotoTarget {
    pub id: i64,
    pub label: String,
    pub line: i64,
    pub column: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GotoTargetsBody {
    pub targets: Vec<GotoTarget>,
}

/// A function a step into can stop in (`stepInTargets` response).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StepInTarget {
    pub id: i64,
    pub label: String,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub end_column: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepInTargetsBody {
    pub targets: Vec<StepInTarget>,
}

/// One proposal of a `completions` response. `start`/`length` (in UTF-16 units of the text,
/// zero-based) say what it replaces; without them it replaces the word before the column.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompletionItem {
    pub label: String,
    pub text: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub detail: Option<String>,
    pub start: Option<i64>,
    pub length: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletionsBody {
    pub targets: Vec<CompletionItem>,
}

/// `setVariable` / `setExpression` response body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetVariableBody {
    pub value: String,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub variables_reference: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExceptionDetails {
    pub message: Option<String>,
    pub type_name: Option<String>,
    pub full_type_name: Option<String>,
    pub stack_trace: Option<String>,
}

/// `exceptionInfo` response body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExceptionInfoBody {
    pub exception_id: String,
    pub description: Option<String>,
    /// `never`, `always`, `unhandled`, `userUnhandled`.
    pub break_mode: String,
    pub details: Option<ExceptionDetails>,
}

// ---------------------------------------------------------------- events

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StoppedEvent {
    /// `step`, `breakpoint`, `exception`, `pause`, `entry`, `goto`, `function breakpoint`, …
    pub reason: String,
    pub description: Option<String>,
    pub thread_id: Option<i64>,
    pub preserve_focus_hint: bool,
    pub text: Option<String>,
    pub all_threads_stopped: bool,
    pub hit_breakpoint_ids: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContinuedEvent {
    pub thread_id: Option<i64>,
    pub all_threads_continued: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExitedEvent {
    pub exit_code: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputEvent {
    /// `console`, `important`, `stdout`, `stderr`, `telemetry`; missing means `console`.
    pub category: Option<String>,
    pub output: String,
    pub source: Option<Source>,
    pub line: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadEvent {
    /// `started` or `exited`.
    pub reason: String,
    pub thread_id: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BreakpointEvent {
    /// `changed`, `new` or `removed`.
    pub reason: String,
    pub breakpoint: Breakpoint,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProcessEvent {
    pub name: String,
    pub system_process_id: Option<i64>,
    pub is_local_process: Option<bool>,
    pub start_method: Option<String>,
}

/// A `startDebugging` reverse request (js-debug and debugpy use it for child sessions).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartDebuggingArguments {
    pub configuration: Value,
    /// `launch` or `attach`.
    pub request: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn lenient_parsing() {
        let v: Variable =
            parse(&json!({"name": "x", "value": "5", "type": "i32", "variablesReference": 0, "extra": 1})).unwrap();
        assert_eq!(v.name, "x");
        assert_eq!(v.type_.as_deref(), Some("i32"));
        let s: StoppedEvent = parse(&Value::Null).unwrap();
        assert_eq!(s.reason, "");
        let frames: StackTraceBody = parse(
            &json!({"stackFrames": [{"id": 1, "name": "main", "line": 3, "column": 1, "source": {"path": "/a.rs"}}]}),
        )
        .unwrap();
        assert_eq!(frames.stack_frames.first().and_then(|f| f.source.as_ref()?.path.clone()), Some("/a.rs".into()));
        assert!(parse::<ThreadsBody>(&json!({"threads": "nope"})).is_err());
    }

    #[test]
    fn memory_and_disassembly_bodies_parse() {
        let caps: Capabilities =
            parse(&json!({"supportsReadMemoryRequest": true, "supportsDataBreakpoints": true})).unwrap();
        assert!(caps.supports_read_memory_request && caps.supports_data_breakpoints);
        assert!(!caps.supports_disassemble_request);
        let mem: ReadMemoryBody = parse(&json!({"address": "0x10", "data": "KgA=", "unreadableBytes": 2})).unwrap();
        assert_eq!((mem.address.as_str(), mem.unreadable_bytes), ("0x10", Some(2)));
        let dis: DisassembleBody = parse(&json!({"instructions": [
            {"address": "0x100003EF4", "instruction": "ldr w8, [sp]", "instructionBytes": "e8 1b 40 b9", "line": 6,
             "location": {"name": "t.c", "path": "/t.c"}}
        ]}))
        .unwrap();
        assert_eq!(dis.instructions[0].line, Some(6));
        let info: DataBreakpointInfoBody = parse(&json!({"dataId": null, "description": "no"})).unwrap();
        assert!(info.data_id.is_none());
        let bp =
            DataBreakpoint { data_id: "16FDFCCF8/4".into(), access_type: Some("write".into()), ..Default::default() };
        assert_eq!(serde_json::to_value(bp).unwrap(), json!({"dataId": "16FDFCCF8/4", "accessType": "write"}));
    }

    #[test]
    fn stepping_and_completion_bodies_parse() {
        let goto: GotoTargetsBody = parse(&json!({"targets": [{"id": 3, "label": "line 7", "line": 7}]})).unwrap();
        assert_eq!((goto.targets[0].id, goto.targets[0].line), (3, 7));
        let step: StepInTargetsBody = parse(&json!({"targets": [{"id": 1, "label": "work"}, {"id": 2}]})).unwrap();
        assert_eq!(step.targets.len(), 2);
        let completions: CompletionsBody =
            parse(&json!({"targets": [{"label": "total", "type": "variable", "start": 0, "length": 3}]})).unwrap();
        assert_eq!(completions.targets[0].type_.as_deref(), Some("variable"));
        let set: SetVariableBody = parse(&json!({"value": "99", "type": "int"})).unwrap();
        assert_eq!(set.value, "99");
        let info: ExceptionInfoBody = parse(&json!({"exceptionId": "ValueError", "breakMode": "always",
            "details": {"message": "bad", "typeName": "ValueError"}}))
        .unwrap();
        assert_eq!(info.details.and_then(|d| d.message).as_deref(), Some("bad"));
        let caps: Capabilities = parse(&json!({"supportsCompletionsRequest": true})).unwrap();
        assert!(caps.supports_completions_request && !caps.supports_set_expression);
    }

    #[test]
    fn source_breakpoints_omit_unset_fields() {
        let bp = SourceBreakpoint { line: 4, condition: Some("x > 1".into()), ..Default::default() };
        assert_eq!(serde_json::to_value(bp).unwrap(), json!({"line": 4, "condition": "x > 1"}));
    }
}
