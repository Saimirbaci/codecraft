//! CodeCraft Language Server Protocol client (L0, no workspace deps).
//!
//! Spawns a server (rust-analyzer by default) over stdio, frames JSON-RPC with
//! `Content-Length` headers, correlates requests and responses, and delivers everything the
//! server says as [`ServerEvent`]s on a channel the engine polls each frame. Nothing blocks the
//! UI thread: requests return a [`RequestId`] immediately and the response arrives as an event.
//!
//! The client performs the `initialize` handshake itself and queues outgoing messages until the
//! server has answered it. It answers the common server→client requests on its own
//! (`workspace/configuration`, `window/workDoneProgress/create`, `client/registerCapability`,
//! `client/unregisterCapability`, `workspace/workspaceFolders`); anything else arrives as
//! [`ServerEvent::ServerRequest`] and must be answered with [`LspClient::respond`].
//!
//! Threads, no async runtime: a writer fed by a channel, a reader that parses and dispatches, and
//! for spawned servers a stderr drain (logged, last lines kept for error reports) and a reaper.
//! [`ServerEvent::Exited`] is sent exactly once and is always the last event. Positions are LSP
//! coordinates (zero-based line, UTF-16 column).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod client;
mod discover;
mod init;
mod transport;
mod uri;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use lsp_types;

use crossbeam_channel::Receiver;
use lsp_types::{notification as notif, request as req};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to start language server {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("language server I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("language server protocol error: {0}")]
    Protocol(String),
    #[error("language server is not running")]
    NotRunning,
    #[error("invalid path or URI: {0}")]
    BadUri(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, LspError>;

#[derive(Clone, Debug, PartialEq)]
pub struct ServerConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    /// Workspace root (sent as `rootUri` and the single workspace folder).
    pub root: PathBuf,
    pub initialization_options: Option<Value>,
    /// Returned for every `workspace/configuration` item (rust-analyzer reads its settings here).
    pub configuration: Option<Value>,
    pub env: Vec<(String, String)>,
}

impl ServerConfig {
    pub fn rust_analyzer(command: PathBuf, root: PathBuf) -> Self {
        Self { command, args: Vec::new(), root, initialization_options: None, configuration: None, env: Vec::new() }
    }
}

/// Client-generated request id (we only ever send integer ids).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub i64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub token: String,
    /// The title from the token's `begin` (repeated on its reports and end).
    pub title: Option<String>,
    pub message: Option<String>,
    pub percentage: Option<u32>,
    pub done: bool,
}

#[derive(Clone, Debug)]
pub enum ServerEvent {
    /// The `initialize` response arrived and `initialized` was sent.
    Initialized(Box<lsp_types::ServerCapabilities>),
    Response {
        id: RequestId,
        /// The request method, for routing (`textDocument/hover`, …).
        method: String,
        result: std::result::Result<Value, ResponseError>,
    },
    Diagnostics(lsp_types::PublishDiagnosticsParams),
    /// `$/progress` with a work-done payload.
    Progress(ProgressUpdate),
    ShowMessage {
        level: lsp_types::MessageType,
        message: String,
    },
    Log {
        level: lsp_types::MessageType,
        message: String,
    },
    /// Any other notification (e.g. `experimental/serverStatus`).
    Notification {
        method: String,
        params: Value,
    },
    /// A server→client request the client did not answer itself; reply with [`LspClient::respond`].
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// The server process ended (or the stream closed).
    Exited {
        code: Option<i32>,
    },
    Error(String),
}

/// A running language-server connection. Cheap to share by reference; all methods take `&self`.
///
/// Dropping it closes the server's stdin and kills a spawned server that is still running; call
/// [`LspClient::shutdown`] first for a graceful exit.
pub struct LspClient {
    inner: Arc<client::Inner>,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.inner.close();
    }
}

impl LspClient {
    /// Spawn the server process and start the handshake.
    pub fn spawn(config: ServerConfig) -> Result<(Self, Receiver<ServerEvent>)> {
        let (inner, events) = client::spawn(config)?;
        Ok((Self { inner }, events))
    }

    /// Connect over arbitrary streams (tests use in-memory pipes) and start the handshake.
    pub fn connect(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        root: PathBuf,
        initialization_options: Option<Value>,
        configuration: Option<Value>,
    ) -> Result<(Self, Receiver<ServerEvent>)> {
        let (inner, events) = client::connect(reader, writer, root, initialization_options, configuration)?;
        Ok((Self { inner }, events))
    }

    pub fn is_initialized(&self) -> bool {
        self.inner.is_initialized()
    }

    pub fn capabilities(&self) -> Option<lsp_types::ServerCapabilities> {
        self.inner.capabilities()
    }

    /// The last lines (up to 50) the spawned server wrote to stderr; empty for [`LspClient::connect`].
    pub fn stderr_tail(&self) -> Vec<String> {
        self.inner.stderr_tail()
    }

    pub fn request_raw(&self, method: &str, params: Value) -> Result<RequestId> {
        self.inner.request(method, params)
    }

    pub fn request<R: lsp_types::request::Request>(&self, params: R::Params) -> Result<RequestId> {
        self.request_raw(R::METHOD, serde_json::to_value(params)?)
    }

    pub fn notify_raw(&self, method: &str, params: Value) -> Result<()> {
        self.inner.notify(method, params)
    }

    pub fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) -> Result<()> {
        self.notify_raw(N::METHOD, serde_json::to_value(params)?)
    }

    /// Answer a [`ServerEvent::ServerRequest`].
    pub fn respond(&self, id: Value, result: std::result::Result<Value, ResponseError>) -> Result<()> {
        self.inner.respond(id, result)
    }

    /// `$/cancelRequest`. The server still answers the request (usually with an error).
    pub fn cancel(&self, id: RequestId) -> Result<()> {
        self.notify_raw("$/cancelRequest", serde_json::json!({ "id": id.0 }))
    }

    pub fn did_open(&self, path: &Path, language_id: &str, version: i32, text: &str) -> Result<()> {
        let text_document =
            lsp_types::TextDocumentItem::new(path_to_uri(path)?, language_id.to_owned(), version, text.to_owned());
        self.notify::<notif::DidOpenTextDocument>(lsp_types::DidOpenTextDocumentParams { text_document })
    }

    /// Full-document sync.
    pub fn did_change_full(&self, path: &Path, version: i32, text: &str) -> Result<()> {
        self.notify::<notif::DidChangeTextDocument>(lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier::new(path_to_uri(path)?, version),
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_owned(),
            }],
        })
    }

    pub fn did_save(&self, path: &Path) -> Result<()> {
        self.notify::<notif::DidSaveTextDocument>(lsp_types::DidSaveTextDocumentParams {
            text_document: document(path)?,
            text: None,
        })
    }

    pub fn did_close(&self, path: &Path) -> Result<()> {
        self.notify::<notif::DidCloseTextDocument>(lsp_types::DidCloseTextDocumentParams {
            text_document: document(path)?,
        })
    }

    /// Positions are LSP coordinates: zero-based line and UTF-16 column.
    pub fn hover(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::HoverRequest>(lsp_types::HoverParams {
            text_document_position_params: position(path, line, character)?,
            work_done_progress_params: Default::default(),
        })
    }

    pub fn completion(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::Completion>(lsp_types::CompletionParams {
            text_document_position: position(path, line, character)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: Some(lsp_types::CompletionContext {
                trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
        })
    }

    pub fn signature_help(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::SignatureHelpRequest>(lsp_types::SignatureHelpParams {
            context: None,
            text_document_position_params: position(path, line, character)?,
            work_done_progress_params: Default::default(),
        })
    }

    pub fn definition(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::GotoDefinition>(goto(path, line, character)?)
    }

    pub fn type_definition(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::GotoTypeDefinition>(goto(path, line, character)?)
    }

    pub fn implementation(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::GotoImplementation>(goto(path, line, character)?)
    }

    pub fn references(&self, path: &Path, line: u32, character: u32, include_declaration: bool) -> Result<RequestId> {
        self.request::<req::References>(lsp_types::ReferenceParams {
            text_document_position: position(path, line, character)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext { include_declaration },
        })
    }

    pub fn document_highlight(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::DocumentHighlightRequest>(lsp_types::DocumentHighlightParams {
            text_document_position_params: position(path, line, character)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    }

    pub fn document_symbol(&self, path: &Path) -> Result<RequestId> {
        self.request::<req::DocumentSymbolRequest>(lsp_types::DocumentSymbolParams {
            text_document: document(path)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    }

    pub fn workspace_symbol(&self, query: &str) -> Result<RequestId> {
        self.request::<req::WorkspaceSymbolRequest>(lsp_types::WorkspaceSymbolParams {
            partial_result_params: Default::default(),
            work_done_progress_params: Default::default(),
            query: query.to_owned(),
        })
    }

    pub fn formatting(&self, path: &Path, tab_size: u32, insert_spaces: bool) -> Result<RequestId> {
        self.request::<req::Formatting>(lsp_types::DocumentFormattingParams {
            text_document: document(path)?,
            options: lsp_types::FormattingOptions { tab_size, insert_spaces, ..Default::default() },
            work_done_progress_params: Default::default(),
        })
    }

    pub fn prepare_rename(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request::<req::PrepareRenameRequest>(position(path, line, character)?)
    }

    pub fn rename(&self, path: &Path, line: u32, character: u32, new_name: &str) -> Result<RequestId> {
        self.request::<req::Rename>(lsp_types::RenameParams {
            text_document_position: position(path, line, character)?,
            new_name: new_name.to_owned(),
            work_done_progress_params: Default::default(),
        })
    }

    pub fn code_action(
        &self,
        path: &Path,
        range: lsp_types::Range,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Result<RequestId> {
        self.request::<req::CodeActionRequest>(lsp_types::CodeActionParams {
            text_document: document(path)?,
            range,
            context: lsp_types::CodeActionContext {
                diagnostics,
                only: None,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    }

    pub fn inlay_hints(&self, path: &Path, range: lsp_types::Range) -> Result<RequestId> {
        self.request::<req::InlayHintRequest>(lsp_types::InlayHintParams {
            work_done_progress_params: Default::default(),
            text_document: document(path)?,
            range,
        })
    }

    pub fn semantic_tokens_full(&self, path: &Path) -> Result<RequestId> {
        self.request::<req::SemanticTokensFullRequest>(lsp_types::SemanticTokensParams {
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            text_document: document(path)?,
        })
    }

    pub fn selection_range(&self, path: &Path, positions: Vec<lsp_types::Position>) -> Result<RequestId> {
        self.request::<req::SelectionRangeRequest>(lsp_types::SelectionRangeParams {
            text_document: document(path)?,
            positions,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    }

    /// `shutdown` + `exit`, wait up to `timeout`, then kill.
    ///
    /// Blocks for at most about `timeout` (plus a short grace period after a kill). Idempotent:
    /// later calls return at once. Afterwards requests fail with [`LspError::NotRunning`].
    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.inner.shutdown(timeout)
    }
}

fn document(path: &Path) -> Result<lsp_types::TextDocumentIdentifier> {
    Ok(lsp_types::TextDocumentIdentifier { uri: path_to_uri(path)? })
}

fn position(path: &Path, line: u32, character: u32) -> Result<lsp_types::TextDocumentPositionParams> {
    Ok(lsp_types::TextDocumentPositionParams {
        text_document: document(path)?,
        position: lsp_types::Position { line, character },
    })
}

fn goto(path: &Path, line: u32, character: u32) -> Result<lsp_types::GotoDefinitionParams> {
    Ok(lsp_types::GotoDefinitionParams {
        text_document_position_params: position(path, line, character)?,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    })
}

/// Locate rust-analyzer: `$RUST_ANALYZER`, then `PATH`, then `~/.cargo/bin`, then
/// `rustup which rust-analyzer`. Only returns a binary that answers `--version` successfully
/// (the rustup proxy without the component installed fails that check).
pub fn find_rust_analyzer() -> Option<PathBuf> {
    discover::find_rust_analyzer()
}

/// `file://` URI for an absolute path (percent-encoded).
pub fn path_to_uri(path: &Path) -> Result<lsp_types::Uri> {
    uri::path_to_uri(path)
}

/// The local path of a `file://` URI (percent-decoded).
pub fn uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    uri::uri_to_path(uri)
}

/// Decode a response payload into a typed result.
pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    Ok(serde_json::from_value(value)?)
}
