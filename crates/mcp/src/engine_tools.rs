//! The CodeCraft engine tools: what agents use to debug, analyze, profile, test, run and query
//! through the IDE (or the headless `codecraft-cli mcp` server). This module only describes them
//! (names, descriptions, argument schemas); the engine executes them and decides, per call,
//! whether the agent may (the access policy).
//!
//! Conventions every tool follows: paths are absolute (relative ones resolve against the
//! project root), lines and columns are 1-based, results are compact JSON with stable ids, and
//! long outputs are cut with a `truncated` note that says how to page.

use serde_json::{Value, json};

use crate::ToolDef;

pub const COMMANDS: &str = "codecraft_commands";
pub const EXECUTE: &str = "codecraft_execute";
pub const PROJECT_INFO: &str = "project_info";
pub const FILE_READ: &str = "file_read";
pub const FILE_WRITE: &str = "file_write";
pub const FILE_EDIT: &str = "file_edit";
pub const EDITOR_OPEN: &str = "editor_open";
pub const PROBLEMS: &str = "problems";
pub const INSPECT: &str = "inspect";
pub const DEFINITION: &str = "definition";
pub const REFERENCES: &str = "references";
pub const HOVER: &str = "hover";
pub const DOCUMENT_SYMBOLS: &str = "document_symbols";
pub const WORKSPACE_SYMBOLS: &str = "workspace_symbols";
pub const CALL_HIERARCHY: &str = "call_hierarchy";
pub const RUN_CONFIGURATIONS: &str = "run_configurations";
pub const DEBUG_START: &str = "debug_start";
pub const DEBUG_BREAKPOINT_SET: &str = "debug_breakpoint_set";
pub const DEBUG_BREAKPOINT_REMOVE: &str = "debug_breakpoint_remove";
pub const DEBUG_BREAKPOINTS: &str = "debug_breakpoints";
pub const DEBUG_CONTROL: &str = "debug_control";
pub const DEBUG_WAIT_FOR_STOP: &str = "debug_wait_for_stop";
pub const DEBUG_STACK: &str = "debug_stack";
pub const DEBUG_VARIABLES: &str = "debug_variables";
pub const DEBUG_EVALUATE: &str = "debug_evaluate";
pub const PROFILE_START: &str = "profile_start";
pub const PROFILE_STOP: &str = "profile_stop";
pub const PROFILE_HOTSPOTS: &str = "profile_hotspots";
pub const TEST_RUN: &str = "test_run";
pub const TEST_RESULTS: &str = "test_results";
pub const RUN_START: &str = "run_start";
pub const RUN_OUTPUT: &str = "run_output";
pub const RUN_STOP: &str = "run_stop";
pub const DB_SOURCES: &str = "db_sources";
pub const DB_QUERY: &str = "db_query";

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

fn path(what: &str) -> Value {
    json!({ "type": "string", "description": format!("{what} (absolute, or relative to the project root)") })
}

fn line(what: &str) -> Value {
    json!({ "type": "integer", "minimum": 1, "description": format!("{what} (1-based)") })
}

fn timeout(default_ms: u64) -> Value {
    json!({ "type": "integer", "minimum": 0, "maximum": 600_000,
            "description": format!("How long to wait, in milliseconds (default {default_ms})") })
}

/// A position in a file: `line` plus either `column` or `symbol` (the identifier on that line).
fn position_props() -> Value {
    json!({
        "path": path("File"),
        "line": line("Line"),
        "column": { "type": "integer", "minimum": 1, "description": "Column (1-based, in characters). Give this or `symbol`" },
        "symbol": { "type": "string", "description": "An identifier on the line; its first occurrence is used when `column` is missing" },
    })
}

/// The order of profile hotspots.
fn hotspot_sort() -> Value {
    json!({ "type": "string", "enum": ["self", "total"], "default": "self",
            "description": "self: by time (or bytes) in the function itself, then total; total: including callees, then self; ties by name and location" })
}

/// The debug session a debug tool acts on (M15 track DM).
fn session_ref() -> Value {
    json!({ "description": "The debug session: its id (`sessionId` of debug_start or a stop summary) or name; \
                            default the selected one (debug_wait_for_stop: any session)",
            "oneOf": [{ "type": "integer", "minimum": 0 }, { "type": "string" }] })
}

fn config_ref() -> Value {
    json!({ "description": "A run configuration: its name, or its index from run_configurations",
            "oneOf": [{ "type": "string" }, { "type": "integer", "minimum": 0 }] })
}

/// Every engine tool, in a stable order.
pub fn engine_tools() -> Vec<ToolDef> {
    vec![
        // ----------------------------------------------------------- generic
        tool(
            COMMANDS,
            "Search CodeCraft's command registry (every IDE action is a command). Returns [{id, label, params, enabled, \
             disabledReason?, menu?, capability}]; call codecraft_execute with an id",
            json!({
                "query": { "type": "string", "description": "Words matched against the id, label and menu path (empty = all)" },
                "enabledOnly": { "type": "boolean", "description": "Only commands that can run now", "default": false },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 },
                "offset": { "type": "integer", "minimum": 0, "default": 0 },
            }),
            &[],
        ),
        tool(
            EXECUTE,
            "Run any CodeCraft command by id with JSON params (see codecraft_commands for ids and params docs). The \
             access policy applies per command (read, run, debug, profile, write, db-write)",
            json!({
                "id": { "type": "string", "description": "Command id, e.g. `cargo.check` or `debug.state`" },
                "params": { "description": "The command's params (object), if any" },
            }),
            &["id"],
        ),
        tool(
            PROJECT_INFO,
            "The open project: root, name, languages, run configurations, language servers, debugger and profiler state",
            json!({}),
            &[],
        ),
        // ----------------------------------------------------------- files and editor
        tool(
            FILE_READ,
            "Read lines of a file as the IDE sees it (unsaved editor changes included). Lines are numbered; at most \
             400 per call (page with startLine)",
            json!({
                "path": path("File"),
                "startLine": line("First line"),
                "endLine": line("Last line, inclusive"),
            }),
            &["path"],
        ),
        tool(
            FILE_WRITE,
            "Replace a file's whole content (creates it if missing). Open editors update with one undo step. Needs \
             the `write` capability",
            json!({ "path": path("File"), "content": { "type": "string", "description": "The new content" } }),
            &["path", "content"],
        ),
        tool(
            FILE_EDIT,
            "Replace text in a file (unsaved editor text included): `oldText` must occur exactly once unless \
             `replaceAll`. Needs the `write` capability",
            json!({
                "path": path("File"),
                "oldText": { "type": "string" },
                "newText": { "type": "string" },
                "replaceAll": { "type": "boolean", "default": false },
            }),
            &["path", "oldText", "newText"],
        ),
        tool(
            EDITOR_OPEN,
            "Open a file in the IDE's editor at a line (shows the user what you are looking at)",
            json!({ "path": path("File"), "line": line("Line"), "column": line("Column") }),
            &["path"],
        ),
        // ----------------------------------------------------------- analysis
        tool(
            PROBLEMS,
            "Problems the IDE knows now (language servers, cargo, inspections) for a file, a directory or the whole \
             project: [{file, line, column, severity, message, source, code?}]",
            json!({
                "path": path("A file or directory (default: the project)"),
                "severity": { "type": "string", "enum": ["error", "warning", "info", "hint"], "description": "Minimum severity (default: warning)" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 200 },
            }),
            &[],
        ),
        tool(
            INSPECT,
            "Run Inspect Code (Clippy/cargo check, ruff, mypy/pyright, clang-tidy, … whichever apply and are \
             installed) on a scope and wait for the findings: [{id, file, line, column, severity, inspection, message, \
             fixes}]",
            json!({
                "scope": { "type": "string", "enum": ["file", "directory", "project", "uncommitted", "glob"], "default": "project" },
                "path": path("The file or directory for those scopes"),
                "pattern": { "type": "string", "description": "For scope glob: comma-separated globs, `!` excludes (\"src/**/*.rs, !**/gen/**\")" },
                "profile": { "type": "string", "description": "An inspection profile (Default, Strict or a user profile; see codecraft_execute analysis.profiles)" },
                "analyzers": { "type": "array", "items": { "type": "string" }, "description": "With profile/glob: analyzer ids (clippy, rust_analyzer, ruff, mypy, pyright, clangd, clang_analyzer, clang_tidy, cppcheck, sql) or languages (rust, python, cpp)" },
                "tools": { "type": "array", "items": { "type": "string" }, "description": "Limit to these analyzers (clippy, check, ruff, mypy, pyright, eslint, clang-tidy, zig, cargo-audit, cargo-deny)" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 200 },
                "timeoutMs": timeout(300_000),
            }),
            &[],
        ),
        tool(
            DEFINITION,
            "Go to definition through the language server: [{file, line, column, excerpt}]",
            json!({ "path": position_props()["path"], "line": position_props()["line"], "column": position_props()["column"], "symbol": position_props()["symbol"], "timeoutMs": timeout(30_000) }),
            &["path", "line"],
        ),
        tool(
            REFERENCES,
            "Find usages through the language server: [{file, line, column, excerpt}] (declaration included)",
            json!({ "path": position_props()["path"], "line": position_props()["line"], "column": position_props()["column"], "symbol": position_props()["symbol"],
                    "limit": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 200 }, "timeoutMs": timeout(30_000) }),
            &["path", "line"],
        ),
        tool(
            HOVER,
            "Type and documentation of the symbol at a position (the language server's hover)",
            json!({ "path": position_props()["path"], "line": position_props()["line"], "column": position_props()["column"], "symbol": position_props()["symbol"], "timeoutMs": timeout(30_000) }),
            &["path", "line"],
        ),
        tool(
            DOCUMENT_SYMBOLS,
            "The symbols of a file (functions, types, fields…) as a flat list with depth: [{name, kind, line, endLine, depth, detail?}]",
            json!({ "path": path("File"), "timeoutMs": timeout(30_000) }),
            &["path"],
        ),
        tool(
            WORKSPACE_SYMBOLS,
            "Search symbols across the project by name: [{name, kind, file, line, container?}]",
            json!({ "query": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }, "timeoutMs": timeout(30_000) }),
            &["query"],
        ),
        tool(
            CALL_HIERARCHY,
            "Callers (incoming) or callees (outgoing) of the function at a position: [{name, kind, file, line, callSites: [line]}]",
            json!({ "path": position_props()["path"], "line": position_props()["line"], "column": position_props()["column"], "symbol": position_props()["symbol"],
                    "direction": { "type": "string", "enum": ["incoming", "outgoing"], "default": "incoming" }, "timeoutMs": timeout(30_000) }),
            &["path", "line"],
        ),
        // ----------------------------------------------------------- run configurations and debugging
        tool(
            RUN_CONFIGURATIONS,
            "The project's run configurations: [{index, name, kind, commandLine}] (use the name or index with run_start, \
             debug_start, profile_start)",
            json!({}),
            &[],
        ),
        tool(
            DEBUG_START,
            "Start debugging (lldb-dap/CodeLLDB for Rust, C and C++; debugpy for Python). Give one of: `config`; `file` \
             (a Python/Zig file, or the Rust test or main at `file`+`line`); `program` (a native executable); `pid` \
             (attach). Breakpoints set before stay. Other debug sessions keep running (debugging a configuration that \
             is being debugged restarts it); the result's `sessionId` is the new session's, for the `session` argument \
             of the other debug tools. Then call debug_wait_for_stop",
            json!({
                "config": config_ref(),
                "file": path("A file to debug, or the file of the test/main to debug"),
                "line": line("With `file`: the line of the test or main function"),
                "program": path("A native executable"),
                "args": { "type": "array", "items": { "type": "string" }, "description": "Program arguments (with `program`)" },
                "pid": { "type": "integer", "minimum": 1, "description": "Attach to this process" },
                "kind": { "type": "string", "enum": ["native", "rust", "python"], "description": "With `pid`: the debugger to attach with" },
            }),
            &[],
        ),
        tool(
            DEBUG_BREAKPOINT_SET,
            "Create or update a line breakpoint: condition, hit count, log message (a logpoint does not stop), \
             temporary. Returns {id, file, line, verified}",
            json!({
                "path": path("File"),
                "line": line("Line"),
                "condition": { "type": "string", "description": "Stop only when this expression is true" },
                "hitCount": { "type": "string", "description": "Stop on this hit count (adapter syntax, e.g. `3` or `>= 3`)" },
                "logMessage": { "type": "string", "description": "Log this (with {expr} interpolation) instead of stopping" },
                "temporary": { "type": "boolean", "description": "Remove after the first hit" },
                "enabled": { "type": "boolean", "default": true },
            }),
            &["path", "line"],
        ),
        tool(
            DEBUG_BREAKPOINT_REMOVE,
            "Remove a breakpoint by id, by file and line, or all of them",
            json!({
                "id": { "type": "integer", "minimum": 0 },
                "path": path("File"),
                "line": line("Line"),
                "all": { "type": "boolean" },
            }),
            &[],
        ),
        tool(
            DEBUG_BREAKPOINTS,
            "Every breakpoint: [{id, file, line, enabled, condition?, hitCount?, logMessage?, temporary, verified?}]",
            json!({}),
            &[],
        ),
        tool(
            DEBUG_CONTROL,
            "Continue, step or stop the debugged program. With `wait` (default true for continue/step*/runToLine), \
             waits for the next stop and returns its summary (like debug_wait_for_stop)",
            json!({
                "action": { "type": "string", "enum": ["continue", "stepOver", "stepInto", "stepOut", "pause", "stop", "runToLine"] },
                "path": path("With runToLine: the file"),
                "line": line("With runToLine: the line"),
                "wait": { "type": "boolean" },
                "timeoutMs": timeout(30_000),
                "frames": { "type": "integer", "minimum": 1, "maximum": 50, "default": 5 },
                "session": session_ref(),
            }),
            &["action"],
        ),
        tool(
            DEBUG_WAIT_FOR_STOP,
            "Wait until the debugged program stops (breakpoint, step, exception, pause) or exits. Returns a stop \
             summary: {state, reason, thread, frames: [{index, name, file, line, excerpt}], locals: [{name, value, type, \
             ref}], sessionId}; `ref` > 0 can be expanded with debug_variables. On timeout: {state: running, timedOut: \
             true}. With several debug sessions the summary lists them all (`sessions`: [{id, name, state, reason?}]) \
             and `sessionId` says which one stopped; without `session` it waits for the first one that stops",
            json!({
                "timeoutMs": timeout(30_000),
                "frames": { "type": "integer", "minimum": 1, "maximum": 50, "default": 5, "description": "Frames to include" },
                "session": session_ref(),
            }),
            &[],
        ),
        tool(
            DEBUG_STACK,
            "The stack of the stopped thread (or another thread): threads and frames with file:line",
            json!({
                "threadId": { "type": "integer" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 30 },
                "timeoutMs": timeout(10_000),
                "session": session_ref(),
            }),
            &[],
        ),
        tool(
            DEBUG_VARIABLES,
            "Variables while stopped: the scopes of a frame (default the top frame) with their variables, or the \
             children of a structured value (`ref` from a previous result)",
            json!({
                "ref": { "type": "integer", "minimum": 1, "description": "Expand this variable reference" },
                "frame": { "type": "integer", "minimum": 0, "description": "Frame index (0 = top); selects that frame" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 },
                "timeoutMs": timeout(10_000),
                "session": session_ref(),
            }),
            &[],
        ),
        tool(
            DEBUG_EVALUATE,
            "Evaluate an expression in the selected (or given) frame while stopped: {value, type, ref}",
            json!({
                "expression": { "type": "string" },
                "frame": { "type": "integer", "minimum": 0, "description": "Frame index (0 = top)" },
                "timeoutMs": timeout(10_000),
                "session": session_ref(),
            }),
            &["expression"],
        ),
        // ----------------------------------------------------------- profiling
        tool(
            PROFILE_START,
            "Profile a run configuration, a command line or the test at a source line (`test`): CPU (xctrace/samply/perf for native code, py-spy/cProfile for \
             Python) or memory (`kind: memory`: leaks/heaptrack for native code, tracemalloc/memray for Python). Cargo and \
             CMake targets are built first. Stop it with profile_stop, or let the program finish and call profile_hotspots",
            json!({
                "config": config_ref(),
                "command": { "description": "Instead of `config`: a command line to profile, e.g. `python3 main.py`, \
                    `cargo run --release`, `pytest -k slow`, `./build/app --fast` (string or array of arguments)" },
                "kind": { "type": "string", "enum": ["cpu", "memory"], "default": "cpu" },
                "tool": { "type": "string", "description": "A specific profiler: xctrace, samply, perf, py-spy, cProfile, tracemalloc, memray, leaks, heaptrack" },
                "seconds": { "type": "number", "minimum": 0, "description": "Stop recording after this many seconds" },
                "test": {
                    "type": "object",
                    "description": "Instead of `config`/`command`: profile the test at a source line, like the gutter's run icon \
                        (Rust `cargo test --lib name -- --exact`, pytest `file::test`, GoogleTest/Catch2/doctest/Boost.Test filters)",
                    "properties": { "path": path("The test's file"), "line": line("A line of the test") },
                    "required": ["path", "line"],
                },
                "interval": { "type": "number", "minimum": 0, "description": "Memory profiles with tracemalloc: seconds between memory samples (default 0.1; 0 = only the snapshot at the end). The report then has the memory over time and the top allocations at the peak" },
            }),
            &[],
        ),
        tool(
            PROFILE_STOP,
            "Stop the recording and wait for the snapshot; returns the hotspots (like profile_hotspots)",
            json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20 },
                "sort": hotspot_sort(),
                "timeoutMs": timeout(120_000),
            }),
            &[],
        ),
        tool(
            PROFILE_HOTSPOTS,
            "The open profiler snapshot's hottest functions: [{name, selfPercent, totalPercent, self, total, file, line}], \
             plus `report` (hottest lines with their code, top functions by total time, notes on missing symbols \
             or debug builds, memory over time for tracemalloc). The order is fixed (see `sort`), whatever the IDE's \
             Method List shows. Waits for a recording that is still running or loading",
            json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20 },
                "sort": hotspot_sort(),
                "timeoutMs": timeout(120_000),
            }),
            &[],
        ),
        // ----------------------------------------------------------- tests
        tool(
            TEST_RUN,
            "Run tests and wait for the results: everything (default), a file's tests (`path`), the test at \
             `path`+`line`, or tests whose name contains `name` (cargo test / pytest -k). Returns counts and failures \
             with messages and locations",
            json!({
                "path": path("A test file"),
                "line": line("With `path`: the test at this line"),
                "name": { "type": "string", "description": "A test name filter" },
                "timeoutMs": timeout(600_000),
            }),
            &[],
        ),
        tool(
            TEST_RESULTS,
            "Results of the latest (or a given) test run: counts, failures (message, file:line, output tail) and, with \
             `includePassed`, every test",
            json!({
                "tab": { "type": "integer", "minimum": 0, "description": "Run tab id (from test_run)" },
                "includePassed": { "type": "boolean", "default": false },
            }),
            &[],
        ),
        // ----------------------------------------------------------- run
        tool(
            RUN_START,
            "Run a configuration (its before-launch tasks first). Returns {tab}; read the output with run_output",
            json!({ "config": config_ref() }),
            &["config"],
        ),
        tool(
            RUN_OUTPUT,
            "Output of a Run tab (default: the newest): status, exit code and the last `tail` lines (or from `offset`)",
            json!({
                "tab": { "type": "integer", "minimum": 0 },
                "tail": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 100 },
                "offset": { "type": "integer", "minimum": 0, "description": "First line to return (instead of the tail)" },
                "waitMs": { "type": "integer", "minimum": 0, "maximum": 600_000, "description": "Wait up to this long for the process to finish first" },
            }),
            &[],
        ),
        tool(
            RUN_STOP,
            "Stop a running Run tab (default: the newest running one)",
            json!({ "tab": { "type": "integer", "minimum": 0 } }),
            &[],
        ),
        // ----------------------------------------------------------- database
        tool(
            DB_SOURCES,
            "The project's data sources: [{id, name, kind, readOnly, confirmWrites, status, schemas}]",
            json!({}),
            &[],
        ),
        tool(
            DB_QUERY,
            "Run SQL on a data source and wait for the rows. Statements that modify data need the `db-write` \
             capability; data sources that confirm writes (production) ask the user in the IDE. Put values in \
             `params` (bound by the driver, never spliced into the SQL) with ?, $1, :name or ${name} placeholders",
            json!({
                "dataSource": { "type": "string", "description": "Data source id or name (db_sources)" },
                "sql": { "type": "string" },
                "params": {
                    "description": "Values for the SQL's placeholders: an object by name (`{\"id\": 42}` for :id) or \
                                    an array in order (`[\"Ann\", 3]` for ? ? or $1 $2). Strings are text exactly as \
                                    given, numbers/booleans/null are themselves, {\"hex\": \"00ff\"} is bytes",
                    "type": ["object", "array"],
                },
                "limit": { "type": "integer", "minimum": 1, "maximum": 5000, "default": 200 },
                "timeoutMs": timeout(60_000),
            }),
            &["dataSource", "sql"],
        ),
    ]
}

/// Whether `name` is one of [`engine_tools`].
pub fn is_engine_tool(name: &str) -> bool {
    engine_tools().iter().any(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_tools_are_unique_valid_and_distinct_from_the_ide_tools() {
        let tools = engine_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), tools.len(), "duplicate engine tool names");
        let ide = crate::tools::claude_code_tools();
        for t in &tools {
            assert!(!ide.iter().any(|i| i.name == t.name), "{} clashes with an IDE tool", t.name);
            assert!(
                t.name.chars().all(|c| c.is_ascii_lowercase() || c == '_') && t.name.len() <= 64,
                "{}: MCP tool names should be short snake_case",
                t.name
            );
            assert!(t.description.len() > 20, "{} needs a description", t.name);
            let schema = &t.input_schema;
            assert_eq!(schema["type"], "object", "{}", t.name);
            let props = schema["properties"].as_object().unwrap_or_else(|| panic!("{}: properties", t.name));
            for req in schema["required"].as_array().unwrap() {
                let req = req.as_str().unwrap();
                assert!(props.contains_key(req), "{}: required `{req}` is not a property", t.name);
            }
            for (key, prop) in props {
                assert!(prop.is_object(), "{}.{key} is not a schema", t.name);
                assert!(!prop.as_object().unwrap().is_empty() || key == "params", "{}.{key} is empty", t.name);
            }
            // Round-trips as JSON (what tools/list sends).
            let text = serde_json::to_string(schema).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), *schema);
        }
        assert!(is_engine_tool(DEBUG_WAIT_FOR_STOP));
        assert!(!is_engine_tool("openDiff"));
    }
}
