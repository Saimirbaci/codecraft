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
    fn source_breakpoints_omit_unset_fields() {
        let bp = SourceBreakpoint { line: 4, condition: Some("x > 1".into()), ..Default::default() };
        assert_eq!(serde_json::to_value(bp).unwrap(), json!({"line": 4, "condition": "x > 1"}));
    }
}
