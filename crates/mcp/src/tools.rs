//! The tools Claude Code expects from an IDE (names and argument schemas as documented in
//! `PROTOCOL-NOTES.md`). The host implements them; the server only advertises and routes.

use serde_json::{Value, json};

use crate::ToolDef;

pub const OPEN_FILE: &str = "openFile";
pub const OPEN_DIFF: &str = "openDiff";
pub const GET_CURRENT_SELECTION: &str = "getCurrentSelection";
pub const GET_LATEST_SELECTION: &str = "getLatestSelection";
pub const GET_OPEN_EDITORS: &str = "getOpenEditors";
pub const GET_WORKSPACE_FOLDERS: &str = "getWorkspaceFolders";
pub const GET_DIAGNOSTICS: &str = "getDiagnostics";
pub const CHECK_DOCUMENT_DIRTY: &str = "checkDocumentDirty";
pub const SAVE_DOCUMENT: &str = "saveDocument";
pub const CLOSE_TAB: &str = "close_tab";
pub const CLOSE_ALL_DIFF_TABS: &str = "closeAllDiffTabs";

/// Notification: the user's selection changed (debounced by the IDE).
pub const SELECTION_CHANGED: &str = "selection_changed";
/// Notification: the user sent a file or line range to Claude Code's prompt.
pub const AT_MENTIONED: &str = "at_mentioned";

/// `openDiff` result markers.
pub const FILE_SAVED: &str = "FILE_SAVED";
pub const DIFF_REJECTED: &str = "DIFF_REJECTED";

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
            "$schema": "http://json-schema.org/draft-07/schema#",
        }),
    }
}

/// Every tool CodeCraft offers Claude Code.
pub fn claude_code_tools() -> Vec<ToolDef> {
    let file_path = json!({ "type": "string", "description": "Absolute path of the file" });
    vec![
        tool(
            OPEN_FILE,
            "Open a file in the editor and optionally select a range of text",
            json!({
                "filePath": { "type": "string", "description": "Path to the file to open" },
                "preview": { "type": "boolean", "description": "Whether to open the file in preview mode", "default": false },
                "startText": { "type": "string", "description": "Text pattern to find the start of the selection range. Selects from the beginning of this match." },
                "endText": { "type": "string", "description": "Text pattern to find the end of the selection range. Selects up to the end of this match. If not provided, only the startText match will be selected." },
                "startLine": { "type": "integer", "description": "Optional: line number to start the selection (1-based)" },
                "endLine": { "type": "integer", "description": "Optional: line number to end the selection (1-based)" },
                "selectToEndOfLine": { "type": "boolean", "description": "If true, selection will extend to the end of the line containing the endText match.", "default": false },
                "makeFrontmost": { "type": "boolean", "description": "Whether to make the file the active editor tab. If false, the file is opened in the background without changing focus.", "default": true },
            }),
            &["filePath"],
        ),
        tool(
            OPEN_DIFF,
            "Open a diff view comparing the file with new contents and wait for the user to accept (FILE_SAVED) or reject (DIFF_REJECTED) it",
            json!({
                "old_file_path": { "type": "string", "description": "Path to the file being changed" },
                "new_file_path": { "type": "string", "description": "Path the new contents are written to (usually the same file)" },
                "new_file_contents": { "type": "string", "description": "Proposed contents of the file" },
                "tab_name": { "type": "string", "description": "Name of the diff tab" },
            }),
            &["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
        ),
        tool(GET_CURRENT_SELECTION, "Get the current text selection in the active editor", json!({}), &[]),
        tool(
            GET_LATEST_SELECTION,
            "Get the most recent text selection (even if not in the active editor)",
            json!({}),
            &[],
        ),
        tool(GET_OPEN_EDITORS, "Get the list of currently open editor tabs", json!({}), &[]),
        tool(GET_WORKSPACE_FOLDERS, "Get all workspace folders currently open in the IDE", json!({}), &[]),
        tool(
            GET_DIAGNOSTICS,
            "Get language diagnostics (errors, warnings) from the IDE",
            json!({
                "uri": { "type": "string", "description": "Optional file URI to get diagnostics for. If not provided, gets diagnostics for all files." },
            }),
            &[],
        ),
        tool(
            CHECK_DOCUMENT_DIRTY,
            "Check if a document has unsaved changes",
            json!({ "filePath": file_path.clone() }),
            &["filePath"],
        ),
        tool(SAVE_DOCUMENT, "Save a document with unsaved changes", json!({ "filePath": file_path }), &["filePath"]),
        tool(
            CLOSE_TAB,
            "Close a tab by name",
            json!({ "tab_name": { "type": "string", "description": "Name of the tab to close" } }),
            &["tab_name"],
        ),
        tool(CLOSE_ALL_DIFF_TABS, "Close all diff tabs in the editor", json!({}), &[]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_unique_names_and_object_schemas() {
        let tools = claude_code_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), tools.len());
        for t in &tools {
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            for req in t.input_schema["required"].as_array().unwrap() {
                let req = req.as_str().unwrap();
                assert!(t.input_schema["properties"].get(req).is_some(), "{}: {req}", t.name);
            }
        }
    }
}
