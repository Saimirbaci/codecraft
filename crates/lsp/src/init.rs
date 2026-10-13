//! The `initialize` request: who we are, the workspace root and what CodeCraft can handle.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{LspError, Result};

/// The workspace root in the forms the handshake needs.
pub(crate) struct Root {
    pub(crate) path: PathBuf,
    pub(crate) uri: lsp_types::Uri,
    pub(crate) name: String,
}

impl Root {
    pub(crate) fn new(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path).map_err(|e| LspError::BadUri(format!("{}: {e}", path.display())))?;
        let uri = crate::path_to_uri(&path)?;
        let name =
            path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
        Ok(Self { path, uri, name })
    }

    /// The `workspace/workspaceFolders` answer (and the `workspaceFolders` initialize field).
    pub(crate) fn workspace_folders(&self) -> Value {
        json!([{ "uri": self.uri.as_str(), "name": self.name }])
    }
}

pub(crate) fn initialize_params(root: &Root, initialization_options: Option<Value>) -> Value {
    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "CodeCraft", "version": env!("CARGO_PKG_VERSION") },
        "rootPath": root.path.to_string_lossy(),
        "rootUri": root.uri.as_str(),
        "workspaceFolders": root.workspace_folders(),
        "capabilities": client_capabilities(),
        "trace": "off",
    });
    if let Some(options) = initialization_options
        && let Value::Object(fields) = &mut params
    {
        fields.insert("initializationOptions".to_owned(), options);
    }
    params
}

/// What the IDE supports. Positions are UTF-16 (the only encoding we offer), completion items
/// are plain text with lazily resolved details, and file watching is left to the server.
pub(crate) fn client_capabilities() -> Value {
    let markup = json!(["markdown", "plaintext"]);
    let goto = json!({ "dynamicRegistration": false, "linkSupport": true });
    json!({
        "workspace": {
            "applyEdit": true,
            "didChangeConfiguration": { "dynamicRegistration": false },
            "didChangeWatchedFiles": { "dynamicRegistration": false },
            "symbol": { "dynamicRegistration": false },
            "workspaceFolders": true,
            "configuration": true,
            // The server asks us to re-request these once its analysis is ready.
            "inlayHint": { "refreshSupport": true },
            "semanticTokens": { "refreshSupport": true },
            // Move/rename of `.rs` files asks the server for `mod`/path updates first.
            "fileOperations": { "dynamicRegistration": false, "didRename": true, "willRename": true },
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            },
            "completion": {
                "dynamicRegistration": false,
                "completionItem": {
                    "snippetSupport": true,
                    "commitCharactersSupport": false,
                    "documentationFormat": markup,
                    "deprecatedSupport": true,
                    "preselectSupport": true,
                    "tagSupport": { "valueSet": [1] },
                    "insertReplaceSupport": false,
                    "resolveSupport": { "properties": ["documentation", "detail", "additionalTextEdits"] },
                    "labelDetailsSupport": true,
                },
                "completionItemKind": { "valueSet": (1..=25).collect::<Vec<u32>>() },
                "contextSupport": true,
            },
            "hover": { "dynamicRegistration": false, "contentFormat": markup },
            "signatureHelp": {
                "dynamicRegistration": false,
                "signatureInformation": {
                    "documentationFormat": markup,
                    "parameterInformation": { "labelOffsetSupport": true },
                    "activeParameterSupport": true,
                },
                "contextSupport": true,
            },
            "declaration": goto,
            "definition": goto,
            "typeDefinition": goto,
            "implementation": goto,
            "references": { "dynamicRegistration": false },
            "documentHighlight": { "dynamicRegistration": false },
            "documentSymbol": {
                "dynamicRegistration": false,
                "symbolKind": { "valueSet": (1..=26).collect::<Vec<u32>>() },
                "hierarchicalDocumentSymbolSupport": true,
            },
            "codeAction": {
                "dynamicRegistration": false,
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "",
                            "quickfix",
                            "refactor",
                            "refactor.extract",
                            "refactor.inline",
                            "refactor.rewrite",
                            "source",
                            "source.organizeImports",
                        ],
                    },
                },
                "isPreferredSupport": true,
                "dataSupport": true,
                "resolveSupport": { "properties": ["edit"] },
            },
            "formatting": { "dynamicRegistration": false },
            "rename": { "dynamicRegistration": false, "prepareSupport": true },
            "publishDiagnostics": {
                "relatedInformation": true,
                "tagSupport": { "valueSet": [1, 2] },
                "versionSupport": true,
                "codeDescriptionSupport": true,
            },
            "selectionRange": { "dynamicRegistration": false },
            "semanticTokens": {
                "dynamicRegistration": false,
                "requests": { "range": false, "full": true },
                "tokenTypes": [
                    "namespace", "type", "class", "enum", "interface", "struct", "typeParameter", "parameter",
                    "variable", "property", "enumMember", "event", "function", "method", "macro", "keyword",
                    "modifier", "comment", "string", "number", "regexp", "operator", "decorator",
                ],
                "tokenModifiers": [
                    "declaration", "definition", "readonly", "static", "deprecated", "abstract", "async",
                    "modification", "documentation", "defaultLibrary",
                ],
                "formats": ["relative"],
                "overlappingTokenSupport": false,
                "multilineTokenSupport": false,
                "augmentsSyntaxTokens": true,
            },
            "inlayHint": { "dynamicRegistration": false },
            "callHierarchy": { "dynamicRegistration": false },
            "typeHierarchy": { "dynamicRegistration": false },
        },
        "window": { "workDoneProgress": true },
        "general": { "positionEncodings": ["utf-16"] },
        // rust-analyzer: report `experimental/serverStatus` (health and quiescence).
        "experimental": { "serverStatusNotification": true },
    })
}

#[cfg(test)]
mod tests {
    use lsp_types::{ClientCapabilities, InitializeParams, MarkupKind, PositionEncodingKind};

    use super::*;

    #[test]
    fn capabilities_are_exactly_what_lsp_types_understands() {
        // A misspelt or misplaced key would be dropped by the roundtrip.
        let caps = client_capabilities();
        let typed: ClientCapabilities = serde_json::from_value(caps.clone()).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), caps);
    }

    #[test]
    fn capabilities_cover_what_the_ide_uses() {
        let caps: ClientCapabilities = serde_json::from_value(client_capabilities()).unwrap();
        let text = caps.text_document.unwrap();
        assert_eq!(text.hover.unwrap().content_format.unwrap().first(), Some(&MarkupKind::Markdown));
        let item = text.completion.unwrap().completion_item.unwrap();
        assert_eq!(item.snippet_support, Some(true));
        assert_eq!(item.label_details_support, Some(true));
        assert_eq!(item.resolve_support.unwrap().properties, ["documentation", "detail", "additionalTextEdits"]);
        assert!(text.signature_help.is_some());
        for goto in [text.definition, text.type_definition, text.implementation] {
            assert_eq!(goto.unwrap().link_support, Some(true));
        }
        assert!(text.references.is_some());
        assert_eq!(text.document_symbol.unwrap().hierarchical_document_symbol_support, Some(true));
        let action = text.code_action.unwrap();
        assert!(action.code_action_literal_support.is_some());
        assert_eq!(action.resolve_support.unwrap().properties, ["edit"]);
        assert!(text.formatting.is_some());
        assert_eq!(text.rename.unwrap().prepare_support, Some(true));
        assert_eq!(text.publish_diagnostics.unwrap().related_information, Some(true));
        assert!(text.inlay_hint.is_some());
        assert!(text.semantic_tokens.unwrap().requests.full.is_some());
        assert!(text.selection_range.is_some());
        assert!(text.call_hierarchy.is_some());
        assert!(text.type_hierarchy.is_some());
        assert_eq!(caps.window.unwrap().work_done_progress, Some(true));
        let workspace = caps.workspace.unwrap();
        assert_eq!(workspace.configuration, Some(true));
        assert_eq!(workspace.workspace_folders, Some(true));
        assert_eq!(workspace.apply_edit, Some(true));
        assert_eq!(workspace.did_change_watched_files.unwrap().dynamic_registration, Some(false));
        assert_eq!(caps.general.unwrap().position_encodings.unwrap(), [PositionEncodingKind::UTF16]);
        assert_eq!(workspace.file_operations.unwrap().will_rename, Some(true));
    }

    #[cfg(unix)]
    #[test]
    #[allow(deprecated)]
    fn initialize_params_parse_as_lsp_types() {
        let root = Root::new(Path::new("/work/my crate")).unwrap();
        let value = initialize_params(&root, Some(json!({"checkOnSave": false})));
        let params: InitializeParams = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(params.process_id, Some(std::process::id()));
        assert_eq!(params.root_uri.unwrap().as_str(), "file:///work/my%20crate");
        assert_eq!(params.root_path.as_deref(), Some("/work/my crate"));
        let folders = params.workspace_folders.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "my crate");
        assert_eq!(params.client_info.unwrap().name, "CodeCraft");
        assert_eq!(params.initialization_options, Some(json!({"checkOnSave": false})));
        assert!(params.capabilities.text_document.is_some());
    }

    #[test]
    fn no_initialization_options_means_the_field_is_absent() {
        let root = Root::new(&std::env::temp_dir()).unwrap();
        let value = initialize_params(&root, None);
        assert!(value.get("initializationOptions").is_none());
    }

    #[test]
    fn relative_roots_are_made_absolute() {
        let root = Root::new(Path::new("some/relative/dir")).unwrap();
        assert!(root.path.is_absolute());
        assert_eq!(root.name, "dir");
    }
}
