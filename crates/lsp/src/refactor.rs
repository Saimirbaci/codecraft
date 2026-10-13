//! Requests behind refactorings and hierarchies: declaration, call and type hierarchies, code
//! actions narrowed by kind, code action resolve, file rename hooks and rust-analyzer's parent
//! module. Hierarchy items and code actions are passed through as raw JSON so the server's `data`
//! field round-trips untouched.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{LspClient, RequestId, Result, document, goto, path_to_uri, position};

impl LspClient {
    /// `textDocument/declaration` (rust-analyzer: an impl item → the trait item it implements).
    pub fn declaration(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request_raw("textDocument/declaration", serde_json::to_value(goto(path, line, character)?)?)
    }

    /// `textDocument/prepareCallHierarchy`: the call hierarchy items at a position.
    pub fn prepare_call_hierarchy(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request_raw("textDocument/prepareCallHierarchy", serde_json::to_value(position(path, line, character)?)?)
    }

    /// `callHierarchy/incomingCalls` (callers) for an item from `prepare_call_hierarchy`.
    pub fn call_hierarchy_incoming(&self, item: Value) -> Result<RequestId> {
        self.request_raw("callHierarchy/incomingCalls", json!({ "item": item }))
    }

    /// `callHierarchy/outgoingCalls` (callees) for an item from `prepare_call_hierarchy`.
    pub fn call_hierarchy_outgoing(&self, item: Value) -> Result<RequestId> {
        self.request_raw("callHierarchy/outgoingCalls", json!({ "item": item }))
    }

    /// `textDocument/prepareTypeHierarchy` (LSP 3.17; servers without it answer "method not found").
    pub fn prepare_type_hierarchy(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request_raw("textDocument/prepareTypeHierarchy", serde_json::to_value(position(path, line, character)?)?)
    }

    pub fn type_hierarchy_supertypes(&self, item: Value) -> Result<RequestId> {
        self.request_raw("typeHierarchy/supertypes", json!({ "item": item }))
    }

    pub fn type_hierarchy_subtypes(&self, item: Value) -> Result<RequestId> {
        self.request_raw("typeHierarchy/subtypes", json!({ "item": item }))
    }

    /// `textDocument/codeAction` restricted to action kinds (`only`, e.g. `["refactor.extract"]`;
    /// empty = every kind).
    pub fn code_action_only(
        &self,
        path: &Path,
        range: lsp_types::Range,
        diagnostics: Vec<lsp_types::Diagnostic>,
        only: &[&str],
    ) -> Result<RequestId> {
        self.request_raw("textDocument/codeAction", code_action_params(path, range, diagnostics, only)?)
    }

    /// `codeAction/resolve`: fill in the `edit` of a code action returned without one.
    pub fn resolve_code_action(&self, action: Value) -> Result<RequestId> {
        self.request_raw("codeAction/resolve", action)
    }

    /// `workspace/willRenameFiles`: the edits the server wants before files are renamed/moved
    /// (rust-analyzer updates `mod` declarations and paths for renamed `.rs` files).
    pub fn will_rename_files(&self, renames: &[(PathBuf, PathBuf)]) -> Result<RequestId> {
        self.request_raw("workspace/willRenameFiles", rename_files_params(renames)?)
    }

    /// `workspace/didRenameFiles` after the renames were applied.
    pub fn did_rename_files(&self, renames: &[(PathBuf, PathBuf)]) -> Result<()> {
        self.notify_raw("workspace/didRenameFiles", rename_files_params(renames)?)
    }

    /// rust-analyzer's `experimental/parentModule`: where the module containing a position is
    /// declared (`mod foo;`).
    pub fn parent_module(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request_raw("experimental/parentModule", serde_json::to_value(position(path, line, character)?)?)
    }
}

fn code_action_params(
    path: &Path,
    range: lsp_types::Range,
    diagnostics: Vec<lsp_types::Diagnostic>,
    only: &[&str],
) -> Result<Value> {
    let params = lsp_types::CodeActionParams {
        text_document: document(path)?,
        range,
        context: lsp_types::CodeActionContext {
            diagnostics,
            only: if only.is_empty() {
                None
            } else {
                Some(only.iter().map(|k| lsp_types::CodeActionKind::from((*k).to_owned())).collect())
            },
            trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    Ok(serde_json::to_value(params)?)
}

fn rename_files_params(renames: &[(PathBuf, PathBuf)]) -> Result<Value> {
    let mut files = Vec::with_capacity(renames.len());
    for (old, new) in renames {
        files.push(lsp_types::FileRename {
            old_uri: path_to_uri(old)?.as_str().to_owned(),
            new_uri: path_to_uri(new)?.as_str().to_owned(),
        });
    }
    Ok(serde_json::to_value(lsp_types::RenameFilesParams { files })?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[cfg(unix)]
    #[test]
    fn code_action_params_carry_only_kinds() {
        let range = lsp_types::Range::default();
        let v = code_action_params(Path::new("/w/a.rs"), range, Vec::new(), &["refactor.extract"]).unwrap();
        assert_eq!(v["context"]["only"], json!(["refactor.extract"]));
        assert_eq!(v["context"]["triggerKind"], 1);
        assert_eq!(v["textDocument"]["uri"], "file:///w/a.rs");
        let all = code_action_params(Path::new("/w/a.rs"), range, Vec::new(), &[]).unwrap();
        assert!(all["context"].get("only").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rename_files_params_are_uris() {
        let v = rename_files_params(&[(PathBuf::from("/w/src/a b.rs"), PathBuf::from("/w/src/c.rs"))]).unwrap();
        assert_eq!(v, json!({ "files": [{ "oldUri": "file:///w/src/a%20b.rs", "newUri": "file:///w/src/c.rs" }] }));
        let typed: lsp_types::RenameFilesParams = serde_json::from_value(v).unwrap();
        assert_eq!(typed.files.len(), 1);
    }

    #[test]
    fn relative_paths_are_rejected_not_panicking() {
        assert!(rename_files_params(&[(PathBuf::from("rel.rs"), PathBuf::from("b.rs"))]).is_err());
    }
}
