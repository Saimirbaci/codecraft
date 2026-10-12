//! The common event model every backend's stream is parsed into, plus short human summaries of
//! tool calls ("Reading src/x.rs", "Running `cargo test`").

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::truncate;

/// Longest tool output kept per event.
pub const MAX_OUTPUT_CHARS: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    /// `pending`, `in_progress` or `completed`.
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    /// The agent's session/thread id (used for follow-ups with `--resume`).
    Session {
        id: String,
    },
    /// A complete block of assistant text (Markdown).
    Text {
        text: String,
    },
    /// A streamed piece of assistant text; consecutive deltas form one message.
    TextDelta {
        text: String,
    },
    ToolUse {
        id: String,
        tool: String,
        summary: String,
        input: Value,
    },
    ToolResult {
        id: String,
        ok: bool,
        output: String,
    },
    FileChanged {
        path: PathBuf,
        kind: ChangeKind,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    /// The end of a turn.
    Result {
        is_error: bool,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
        text: Option<String>,
    },
    Error {
        message: String,
    },
    /// A line that is not part of the protocol (stderr, plain text).
    Log {
        line: String,
    },
    /// The process exited (always the last event of a process).
    Exited {
        code: Option<i32>,
        success: bool,
    },
}

fn s<'a>(input: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| input.get(*k).and_then(Value::as_str)).unwrap_or("")
}

/// Paths are shown relative to the job's directory; the absolute prefix is noise.
pub fn display_path(p: &str, cwd: &Path) -> String {
    let path = Path::new(p);
    match path.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => p.to_string(),
    }
}

/// A shell command on one short line, without a leading `cd … &&` and the `bash -lc` wrapper.
pub fn clean_command(cmd: &str) -> String {
    let mut c = cmd.trim();
    for wrapper in ["/bin/zsh -lc ", "/bin/bash -lc ", "bash -lc ", "zsh -lc ", "sh -c ", "/bin/sh -c "] {
        if let Some(rest) = c.strip_prefix(wrapper) {
            c = rest.trim();
            if c.len() >= 2 && ((c.starts_with('\'') && c.ends_with('\'')) || (c.starts_with('"') && c.ends_with('"')))
            {
                c = c.get(1..c.len() - 1).unwrap_or(c);
            }
            break;
        }
    }
    if let Some(rest) = c.strip_prefix("cd ")
        && let Some(i) = rest.find("&&")
    {
        c = rest.get(i + 2..).unwrap_or(rest).trim();
    }
    let flat: String = c.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, 140)
}

/// Command text from a JSON value: a string, or an argv array.
pub fn command_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

/// A one-line description of a tool call. Knows Claude Code's, Gemini CLI's and common
/// Messages-API tool names; anything else is "Using <tool>".
pub fn describe_tool(tool: &str, input: &Value, cwd: &Path) -> String {
    let path = || display_path(s(input, &["file_path", "path", "absolute_path", "filePath", "notebook_path"]), cwd);
    let lower = tool.to_ascii_lowercase();
    match lower.as_str() {
        "read" | "notebookread" | "read_file" | "view" | "read_many_files" => {
            let p = path();
            if p.is_empty() { "Reading files".into() } else { format!("Reading {p}") }
        }
        "edit"
        | "multiedit"
        | "notebookedit"
        | "replace"
        | "edit_file"
        | "str_replace"
        | "str_replace_editor"
        | "str_replace_based_edit_tool"
        | "search_replace" => format!("Editing {}", path()),
        "write" | "write_file" | "create" | "create_file" => format!("Writing {}", path()),
        "bash" | "run_shell_command" | "shell" | "run_terminal_cmd" | "execute" | "exec" | "terminal" => {
            let cmd = command_text(input.get("command").or_else(|| input.get("cmd"))).unwrap_or_default();
            format!("Running `{}`", clean_command(&cmd))
        }
        "grep" | "search_file_content" | "search" | "grep_search" => {
            format!("Searching for “{}”", truncate(s(input, &["pattern", "query", "regex"]), 80))
        }
        "glob" | "list_directory" | "ls" | "list_dir" | "find_files" => {
            let what = s(input, &["pattern", "path", "dir_path"]);
            if what.is_empty() {
                "Listing files".into()
            } else {
                format!("Listing {}", truncate(&display_path(what, cwd), 80))
            }
        }
        "websearch" | "google_web_search" | "web_search" => {
            format!("Searching the web: {}", truncate(s(input, &["query"]), 100))
        }
        "webfetch" | "web_fetch" => format!("Fetching {}", truncate(s(input, &["url", "prompt"]), 100)),
        "task" | "agent" => format!("Starting a helper agent: {}", truncate(s(input, &["description"]), 100)),
        "todowrite" | "write_todos" | "update_plan" => "Updating the task list".into(),
        _ => format!("Using {tool}"),
    }
}

/// Whether a tool writes the file named in its input (for Auto-mode reloads).
pub fn is_edit_tool(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "edit"
            | "multiedit"
            | "notebookedit"
            | "write"
            | "replace"
            | "write_file"
            | "edit_file"
            | "create"
            | "create_file"
            | "str_replace"
            | "str_replace_editor"
            | "str_replace_based_edit_tool"
            | "search_replace"
    )
}

/// The file an edit tool writes, resolved against `cwd`.
pub fn edited_path(input: &Value, cwd: &Path) -> Option<PathBuf> {
    let p = s(input, &["file_path", "path", "absolute_path", "filePath", "notebook_path"]);
    if p.is_empty() {
        return None;
    }
    let p = PathBuf::from(p);
    Some(if p.is_absolute() { p } else { cwd.join(p) })
}

/// Tool output as text: a string, or the text parts of a content array.
pub fn output_text(v: Option<&Value>) -> String {
    let text = match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str).or_else(|| i.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    truncate(&text, MAX_OUTPUT_CHARS)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summaries() {
        let cwd = Path::new("/p");
        assert_eq!(describe_tool("Read", &json!({"file_path": "/p/src/x.rs"}), cwd), "Reading src/x.rs");
        assert_eq!(describe_tool("Edit", &json!({"file_path": "/p/y.rs"}), cwd), "Editing y.rs");
        assert_eq!(describe_tool("Bash", &json!({"command": "cd /p && cargo test"}), cwd), "Running `cargo test`");
        assert_eq!(describe_tool("run_shell_command", &json!({"command": "ls -la"}), cwd), "Running `ls -la`");
        assert_eq!(describe_tool("Mystery", &json!(null), cwd), "Using Mystery");
        assert_eq!(clean_command("/bin/zsh -lc 'cargo build'"), "cargo build");
        assert_eq!(command_text(Some(&json!(["git", "status"]))).as_deref(), Some("git status"));
    }

    #[test]
    fn hostile_inputs_do_not_panic() {
        let cwd = Path::new("/");
        for v in [json!(null), json!([]), json!({"command": 5}), json!({"file_path": ""})] {
            for t in ["Read", "Bash", "Edit", "", "Grep", "glob"] {
                let _ = describe_tool(t, &v, cwd);
            }
        }
        assert_eq!(clean_command("\"\""), "\"\"");
        assert_eq!(clean_command("sh -c '"), "'");
        assert_eq!(clean_command("cd &&"), "");
    }
}
