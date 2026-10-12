//! Stream parsers: one JSON line from a headless CLI → zero or more [`AgentEvent`]s.
//!
//! - Claude Code `--output-format stream-json --verbose`: `system/init` (session id),
//!   `assistant` / `user` messages with `text`, `tool_use` and `tool_result` blocks, and a final
//!   `result` (cost, duration, `is_error`).
//! - Grok `--output-format streaming-messages-json`: Anthropic Messages wire format. Parsed with
//!   the Claude parser, which also accepts bare `{role, content}` messages.
//! - Codex `exec --json`: `thread.started`, `item.started|updated|completed` (command_execution,
//!   file_change, agent_message, mcp_tool_call, web_search, todo_list, error), `turn.completed`,
//!   `turn.failed`, `error`.
//! - Gemini `--output-format stream-json`: `init`, `message` (assistant deltas), `tool_use`,
//!   `tool_result`, `error`, `result`.
//!
//! Every parser tolerates unknown lines: non-JSON becomes [`AgentEvent::Log`], unknown JSON is
//! ignored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::backend::Backend;
use crate::event::{
    AgentEvent, ChangeKind, PlanEntry, command_text, describe_tool, edited_path, is_edit_tool, output_text,
};
use crate::truncate;

/// Longest non-JSON line kept as a log event.
const MAX_LOG_CHARS: usize = 2000;

pub struct StreamParser {
    backend: Backend,
    cwd: PathBuf,
    /// Tool call id → the file it edits (reported as a change once the call succeeds).
    pending_edits: HashMap<String, PathBuf>,
    session_seen: bool,
}

impl StreamParser {
    pub fn new(backend: Backend, cwd: &Path) -> Self {
        Self { backend, cwd: cwd.to_path_buf(), pending_edits: HashMap::new(), session_seen: false }
    }

    /// Parse one line of output.
    pub fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Log { line: truncate(line, MAX_LOG_CHARS) }];
        };
        if !v.is_object() {
            return vec![AgentEvent::Log { line: truncate(line, MAX_LOG_CHARS) }];
        }
        let mut out = Vec::new();
        match self.backend {
            Backend::Claude | Backend::Grok => self.anthropic(&v, &mut out),
            Backend::Codex => self.codex(&v, &mut out),
            Backend::Gemini => self.gemini(&v, &mut out),
        }
        out
    }

    fn session(&mut self, id: Option<&str>, out: &mut Vec<AgentEvent>) {
        if let Some(id) = id.filter(|s| !s.is_empty())
            && !self.session_seen
        {
            self.session_seen = true;
            out.push(AgentEvent::Session { id: id.to_string() });
        }
    }

    fn tool_use(&mut self, id: &str, tool: &str, input: &Value, out: &mut Vec<AgentEvent>) {
        if is_edit_tool(tool)
            && let Some(p) = edited_path(input, &self.cwd)
        {
            self.pending_edits.insert(id.to_string(), p);
        }
        out.push(AgentEvent::ToolUse {
            id: id.to_string(),
            tool: tool.to_string(),
            summary: describe_tool(tool, input, &self.cwd),
            input: input.clone(),
        });
    }

    fn tool_result(&mut self, id: &str, ok: bool, output: String, out: &mut Vec<AgentEvent>) {
        let edit = self.pending_edits.remove(id);
        out.push(AgentEvent::ToolResult { id: id.to_string(), ok, output });
        if ok && let Some(path) = edit {
            out.push(AgentEvent::FileChanged { path, kind: ChangeKind::Modified });
        }
    }

    // ------------------------------------------------------------------ Claude Code / Grok

    fn anthropic(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let session = v.get("session_id").or_else(|| v.get("sessionId")).and_then(Value::as_str);
        self.session(session, out);
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "assistant" | "user" => {
                if let Some(content) = v.pointer("/message/content") {
                    self.anthropic_content(ty, content, out);
                }
            }
            "message" => {
                let role = v.get("role").and_then(Value::as_str).unwrap_or("assistant");
                if let Some(content) = v.get("content") {
                    self.anthropic_content(role, content, out);
                }
            }
            "result" => {
                let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("success");
                let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(subtype != "success");
                out.push(AgentEvent::Result {
                    is_error,
                    cost_usd: v.get("total_cost_usd").or_else(|| v.get("cost_usd")).and_then(Value::as_f64),
                    duration_ms: v.get("duration_ms").and_then(Value::as_u64),
                    text: v.get("result").and_then(Value::as_str).map(str::to_string),
                });
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .or_else(|| v.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                out.push(AgentEvent::Error { message: msg.to_string() });
            }
            "" => {
                // A bare Messages-API message: `{"role": …, "content": …}`.
                if let (Some(role), Some(content)) = (v.get("role").and_then(Value::as_str), v.get("content")) {
                    self.anthropic_content(role, content, out);
                }
            }
            _ => {}
        }
    }

    fn anthropic_content(&mut self, role: &str, content: &Value, out: &mut Vec<AgentEvent>) {
        let items: Vec<&Value> = match content {
            Value::Array(a) => a.iter().collect(),
            Value::String(text) => {
                if role == "assistant" && !text.trim().is_empty() {
                    out.push(AgentEvent::Text { text: text.clone() });
                }
                return;
            }
            _ => return,
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") if role == "assistant" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if !text.trim().is_empty() {
                        out.push(AgentEvent::Text { text: text.to_string() });
                    }
                }
                Some("tool_use" | "server_tool_use") => {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = item.get("input").cloned().unwrap_or(Value::Null);
                    self.tool_use(id, name, &input, out);
                }
                Some("tool_result") => {
                    let id = item.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    let ok = !item.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    self.tool_result(id, ok, output_text(item.get("content")), out);
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------ Codex

    fn codex(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "thread.started" => self.session(v.get("thread_id").and_then(Value::as_str), out),
            "turn.completed" => {
                out.push(AgentEvent::Result { is_error: false, cost_usd: None, duration_ms: None, text: None })
            }
            "turn.failed" => {
                let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("the turn failed");
                out.push(AgentEvent::Error { message: msg.to_string() });
                out.push(AgentEvent::Result {
                    is_error: true,
                    cost_usd: None,
                    duration_ms: None,
                    text: Some(msg.to_string()),
                });
            }
            "error" => {
                let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                out.push(AgentEvent::Error { message: msg.to_string() });
            }
            "item.started" | "item.updated" | "item.completed" => {
                if let Some(item) = v.get("item") {
                    self.codex_item(ty, item, out);
                }
            }
            _ => {}
        }
    }

    fn codex_item(&mut self, phase: &str, item: &Value, out: &mut Vec<AgentEvent>) {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let started = phase == "item.started";
        let completed = phase == "item.completed";
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        match kind {
            "agent_message" if completed => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.trim().is_empty() {
                    out.push(AgentEvent::Text { text: text.to_string() });
                }
            }
            "command_execution" => {
                if started {
                    let cmd = command_text(item.get("command")).unwrap_or_default();
                    self.tool_use(id, "shell", &serde_json::json!({ "command": cmd }), out);
                } else if completed {
                    let ok = match item.get("exit_code").and_then(Value::as_i64) {
                        Some(code) => code == 0,
                        None => status != "failed",
                    };
                    self.tool_result(id, ok, output_text(item.get("aggregated_output")), out);
                }
            }
            "file_change" => {
                let changes: Vec<(PathBuf, ChangeKind)> = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| {
                                let p = c.get("path").and_then(Value::as_str)?;
                                let kind = match c.get("kind").and_then(Value::as_str) {
                                    Some("add") => ChangeKind::Added,
                                    Some("delete") => ChangeKind::Deleted,
                                    _ => ChangeKind::Modified,
                                };
                                let p = PathBuf::from(p);
                                Some((if p.is_absolute() { p } else { self.cwd.join(p) }, kind))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if started {
                    let names: Vec<String> = changes
                        .iter()
                        .map(|(p, _)| crate::event::display_path(&p.to_string_lossy(), &self.cwd))
                        .collect();
                    out.push(AgentEvent::ToolUse {
                        id: id.to_string(),
                        tool: "apply_patch".into(),
                        summary: format!("Editing {}", truncate(&names.join(", "), 140)),
                        input: item.get("changes").cloned().unwrap_or(Value::Null),
                    });
                } else if completed {
                    let ok = status != "failed";
                    out.push(AgentEvent::ToolResult { id: id.to_string(), ok, output: String::new() });
                    if ok {
                        for (path, kind) in changes {
                            out.push(AgentEvent::FileChanged { path, kind });
                        }
                    }
                }
            }
            "mcp_tool_call" => {
                if started {
                    let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
                    out.push(AgentEvent::ToolUse {
                        id: id.to_string(),
                        tool: format!("{server}.{tool}"),
                        summary: format!("Using {server} › {tool}"),
                        input: item.get("arguments").cloned().unwrap_or(Value::Null),
                    });
                } else if completed {
                    let output = item.get("result").map(|r| output_text(r.get("content").or(Some(r))));
                    let output =
                        output.or_else(|| item.pointer("/error/message").and_then(Value::as_str).map(str::to_string));
                    out.push(AgentEvent::ToolResult {
                        id: id.to_string(),
                        ok: status != "failed",
                        output: output.unwrap_or_default(),
                    });
                }
            }
            "web_search" if started => {
                let q = item.get("query").and_then(Value::as_str).unwrap_or("");
                out.push(AgentEvent::ToolUse {
                    id: id.to_string(),
                    tool: "web_search".into(),
                    summary: format!("Searching the web: {}", truncate(q, 100)),
                    input: Value::Null,
                });
            }
            "web_search" if completed => {
                out.push(AgentEvent::ToolResult { id: id.to_string(), ok: true, output: String::new() })
            }
            "todo_list" => {
                let entries = item
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|e| PlanEntry {
                                content: e.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                                status: if e.get("completed").and_then(Value::as_bool).unwrap_or(false) {
                                    "completed".into()
                                } else {
                                    "pending".into()
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(AgentEvent::Plan { entries });
            }
            "error" => {
                let msg = item.get("message").and_then(Value::as_str).unwrap_or("error");
                out.push(AgentEvent::Error { message: msg.to_string() });
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ Gemini

    fn gemini(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "init" => self.session(v.get("session_id").and_then(Value::as_str), out),
            "message" => {
                if v.get("role").and_then(Value::as_str) != Some("assistant") {
                    return;
                }
                let text = v.get("content").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    return;
                }
                if v.get("delta").and_then(Value::as_bool).unwrap_or(false) {
                    out.push(AgentEvent::TextDelta { text: text.to_string() });
                } else {
                    out.push(AgentEvent::Text { text: text.to_string() });
                }
            }
            "tool_use" => {
                let id = v.get("tool_id").and_then(Value::as_str).unwrap_or("");
                let name = v.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
                let input = v.get("parameters").cloned().unwrap_or(Value::Null);
                self.tool_use(id, name, &input, out);
            }
            "tool_result" => {
                let id = v.get("tool_id").and_then(Value::as_str).unwrap_or("");
                let ok = v.get("status").and_then(Value::as_str) == Some("success");
                let output = if ok {
                    output_text(v.get("output"))
                } else {
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(|m| truncate(m, crate::event::MAX_OUTPUT_CHARS))
                        .unwrap_or_else(|| output_text(v.get("output")))
                };
                self.tool_result(id, ok, output, out);
            }
            "error" => {
                let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                if v.get("severity").and_then(Value::as_str) == Some("warning") {
                    out.push(AgentEvent::Log { line: format!("warning: {msg}") });
                } else {
                    out.push(AgentEvent::Error { message: msg.to_string() });
                }
            }
            "result" => {
                let is_error = v.get("status").and_then(Value::as_str) != Some("success");
                let text = v.pointer("/error/message").and_then(Value::as_str).map(str::to_string);
                out.push(AgentEvent::Result {
                    is_error,
                    cost_usd: None,
                    duration_ms: v.pointer("/stats/duration_ms").and_then(Value::as_u64),
                    text,
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn parse_all(backend: Backend, lines: &str) -> Vec<AgentEvent> {
        let mut p = StreamParser::new(backend, Path::new("/proj"));
        lines.lines().flat_map(|l| p.line(l)).collect()
    }

    #[test]
    fn claude_stream() {
        let lines = r#"{"type":"system","subtype":"hook_started","hook_id":"x","session_id":"abc"}
{"type":"system","subtype":"init","cwd":"/proj","session_id":"abc","tools":["Read"]}
{"type":"assistant","message":{"content":[{"type":"text","text":"I'll fix it."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/proj/src/a.rs"}}]},"session_id":"abc"}
{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"fn a() {}"}]}}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/proj/src/a.rs","old_string":"a","new_string":"b"}}]}}
{"type":"user","message":{"content":[{"tool_use_id":"t2","type":"tool_result","content":[{"type":"text","text":"ok"}]}]}}
{"type":"rate_limit_event","rate_limit_info":{}}
not json at all
{"type":"result","subtype":"success","is_error":false,"duration_ms":45798,"total_cost_usd":0.61,"result":"Done.","session_id":"abc"}"#;
        let ev = parse_all(Backend::Claude, lines);
        assert_eq!(ev.first(), Some(&AgentEvent::Session { id: "abc".into() }));
        assert!(ev.contains(&AgentEvent::Text { text: "I'll fix it.".into() }));
        assert!(ev.iter().any(|e| matches!(e, AgentEvent::ToolUse { summary, .. } if summary == "Reading src/a.rs")));
        assert!(ev.contains(&AgentEvent::ToolResult { id: "t1".into(), ok: true, output: "fn a() {}".into() }));
        assert!(ev.contains(&AgentEvent::FileChanged { path: "/proj/src/a.rs".into(), kind: ChangeKind::Modified }));
        assert!(ev.contains(&AgentEvent::Log { line: "not json at all".into() }));
        assert_eq!(
            ev.last(),
            Some(&AgentEvent::Result {
                is_error: false,
                cost_usd: Some(0.61),
                duration_ms: Some(45798),
                text: Some("Done.".into())
            })
        );
        // Only one session event although every line carries the id.
        assert_eq!(ev.iter().filter(|e| matches!(e, AgentEvent::Session { .. })).count(), 1);
    }

    #[test]
    fn claude_error_result_and_failed_edit() {
        let lines = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Write","input":{"file_path":"x.rs"}}]}}
{"type":"user","message":{"content":[{"tool_use_id":"t","type":"tool_result","content":"denied","is_error":true}]}}
{"type":"result","subtype":"error_max_turns","duration_ms":5}"#;
        let ev = parse_all(Backend::Claude, lines);
        assert!(!ev.iter().any(|e| matches!(e, AgentEvent::FileChanged { .. })));
        assert!(matches!(ev.last(), Some(AgentEvent::Result { is_error: true, .. })));
    }

    #[test]
    fn grok_messages_format() {
        let lines = r#"{"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"g1","name":"edit_file","input":{"path":"src/m.rs"}}],"session_id":"g-s"}
{"type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"g1","content":"done"}]}
{"type":"stream_event","event":{"type":"content_block_delta"}}
{"type":"result","is_error":false,"duration_ms":10}"#;
        let ev = parse_all(Backend::Grok, lines);
        assert_eq!(ev.first(), Some(&AgentEvent::Session { id: "g-s".into() }));
        assert!(ev.contains(&AgentEvent::Text { text: "Looking.".into() }));
        assert!(ev.contains(&AgentEvent::FileChanged { path: "/proj/src/m.rs".into(), kind: ChangeKind::Modified }));
        assert!(matches!(ev.last(), Some(AgentEvent::Result { is_error: false, .. })));
    }

    #[test]
    fn codex_stream() {
        let lines = r#"Reading additional input from stdin...
{"type":"thread.started","thread_id":"th-1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Inspecting."}}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'cargo test'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'cargo test'","aggregated_output":"ok\n","exit_code":0,"status":"completed"}}
{"type":"item.started","item":{"id":"item_2","type":"file_change","changes":[{"path":"/proj/src/types.rs","kind":"update"}],"status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"/proj/src/types.rs","kind":"update"},{"path":"new.rs","kind":"add"}],"status":"completed"}}
{"type":"item.updated","item":{"id":"item_3","type":"todo_list","items":[{"text":"Plan","completed":true},{"text":"Do","completed":false}]}}
{"type":"item.started","item":{"id":"item_4","type":"reasoning","text":"hmm"}}
{"type":"turn.completed","usage":{"input_tokens":1}}"#;
        let ev = parse_all(Backend::Codex, lines);
        assert_eq!(ev.first(), Some(&AgentEvent::Log { line: "Reading additional input from stdin...".into() }));
        assert!(ev.contains(&AgentEvent::Session { id: "th-1".into() }));
        assert!(ev.contains(&AgentEvent::Text { text: "Inspecting.".into() }));
        assert!(
            ev.iter().any(|e| matches!(e, AgentEvent::ToolUse { summary, .. } if summary == "Running `cargo test`"))
        );
        assert!(ev.contains(&AgentEvent::ToolResult { id: "item_1".into(), ok: true, output: "ok\n".into() }));
        assert!(ev.contains(&AgentEvent::FileChanged { path: "/proj/new.rs".into(), kind: ChangeKind::Added }));
        assert!(ev.iter().any(|e| matches!(e, AgentEvent::Plan { entries } if entries.len() == 2)));
        assert!(matches!(ev.last(), Some(AgentEvent::Result { is_error: false, .. })));

        let failed = parse_all(Backend::Codex, r#"{"type":"turn.failed","error":{"message":"quota"}}"#);
        assert!(failed.contains(&AgentEvent::Error { message: "quota".into() }));
    }

    #[test]
    fn gemini_stream() {
        let lines = r#"{"type":"init","timestamp":"t","session_id":"gm-1","model":"gemini-2.5-pro"}
{"type":"message","timestamp":"t","role":"user","content":"fix it"}
{"type":"message","timestamp":"t","role":"assistant","content":"Sure","delta":true}
{"type":"tool_use","timestamp":"t","tool_name":"replace","tool_id":"r1","parameters":{"file_path":"/proj/a.py","old_string":"x","new_string":"y"}}
{"type":"tool_result","timestamp":"t","tool_id":"r1","status":"success","output":"ok"}
{"type":"tool_use","timestamp":"t","tool_name":"run_shell_command","tool_id":"r2","parameters":{"command":"pytest"}}
{"type":"tool_result","timestamp":"t","tool_id":"r2","status":"error","output":"","error":{"type":"X","message":"boom"}}
{"type":"error","timestamp":"t","severity":"warning","message":"slow"}
{"type":"result","timestamp":"t","status":"success","stats":{"total_tokens":5,"duration_ms":1234,"tool_calls":2}}"#;
        let ev = parse_all(Backend::Gemini, lines);
        assert_eq!(ev.first(), Some(&AgentEvent::Session { id: "gm-1".into() }));
        assert!(ev.contains(&AgentEvent::TextDelta { text: "Sure".into() }));
        assert!(ev.contains(&AgentEvent::FileChanged { path: "/proj/a.py".into(), kind: ChangeKind::Modified }));
        assert!(ev.contains(&AgentEvent::ToolResult { id: "r2".into(), ok: false, output: "boom".into() }));
        assert!(ev.iter().any(|e| matches!(e, AgentEvent::ToolUse { summary, .. } if summary == "Running `pytest`")));
        assert_eq!(
            ev.last(),
            Some(&AgentEvent::Result { is_error: false, cost_usd: None, duration_ms: Some(1234), text: None })
        );
    }

    #[test]
    fn hostile_lines_never_panic() {
        let junk = [
            "",
            "{",
            "[]",
            "42",
            "null",
            r#"{"type":5}"#,
            r#"{"type":"assistant","message":{"content":"plain"}}"#,
            r#"{"type":"assistant","message":{"content":[1,null,{"type":"tool_use"}]}}"#,
            r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":5}]}}"#,
            r#"{"type":"item.completed","item":null}"#,
            r#"{"type":"tool_result","tool_id":null}"#,
            r#"{"type":"result","stats":"x"}"#,
            "\u{0}\u{1b}[31m",
        ];
        for b in Backend::ALL {
            let mut p = StreamParser::new(b, Path::new("/"));
            for j in junk {
                let _ = p.line(j);
            }
        }
    }
}
