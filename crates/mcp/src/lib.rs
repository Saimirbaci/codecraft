//! CodeCraft IDE server for external agents: Claude Code IDE integration over WebSocket MCP (L0, no workspace deps).
//!
//! Claude Code finds IDEs through lock files (`~/.claude/ide/<port>.lock`), connects to
//! `ws://127.0.0.1:<port>` with the lock file's token in the `x-claude-code-ide-authorization`
//! header and speaks MCP (JSON-RPC 2.0): `initialize`, `tools/list`, `tools/call`. The IDE
//! pushes `selection_changed` and `at_mentioned` notifications. See `PROTOCOL-NOTES.md`.
//!
//! [`IdeServer::start`] binds a random loopback port, writes the lock file and runs one thread
//! per connection. The server core answers the protocol methods itself; every `tools/call`
//! reaches the host as a [`ServerEvent::ToolCall`] carrying a [`Reply`] handle, which the host
//! may answer at any time later (`openDiff` waits for the user).
//!
//! Besides Claude Code's IDE tools ([`tools`]) the server can advertise CodeCraft's engine tools
//! ([`engine_tools`]: debugging, analysis, profiling, tests, runs, databases). [`StdioServer`]
//! serves the same protocol over stdin/stdout (the headless `codecraft-cli mcp`), and
//! [`bridge`] connects a stdio client to a running IDE server.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod bridge;
pub mod engine_tools;
pub mod lockfile;
mod rpc;
mod server;
mod stdio;
pub mod tools;

use std::path::PathBuf;

use crossbeam_channel::Sender;
use serde_json::{Value, json};

pub use server::IdeServer;
pub use stdio::{STDIO_CLIENT, StdioServer};
/// The WebSocket implementation, re-exported for clients and tests that talk to the server.
pub use tungstenite;

/// Header carrying the lock file's `authToken` on the WebSocket upgrade request.
pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
/// Env var telling a `claude` launched from the IDE which port to connect to.
pub const ENV_PORT: &str = "CLAUDE_CODE_SSE_PORT";
/// Env var asking a `claude` launched from the IDE to connect automatically.
pub const ENV_ENABLE: &str = "ENABLE_IDE_INTEGRATION";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    #[error("{0}")]
    Io(String),
    #[error("lock file: {0}")]
    Lock(String),
}

pub type Result<T> = std::result::Result<T, McpError>;

/// A connection, numbered from 1 in accept order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ClientId(pub u64);

/// A tool the server advertises in `tools/list`.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Shown in Claude Code's `/ide` list (`ideName` in the lock file) and as `serverInfo.name`.
    pub ide_name: String,
    pub server_version: String,
    /// Folders Claude Code matches its working directory against.
    pub workspace_folders: Vec<PathBuf>,
    /// Where to write `<port>.lock`; `None` writes no lock file (embedding, tests).
    /// [`lockfile::default_lock_dir`] is the directory Claude Code reads.
    pub lock_dir: Option<PathBuf>,
    pub tools: Vec<ToolDef>,
    /// A fixed token (tests); `None` generates a random one.
    pub auth_token: Option<String>,
}

impl ServerConfig {
    /// A config advertising Claude Code's IDE tools ([`tools::claude_code_tools`]).
    pub fn claude_code(ide_name: &str, workspace_folders: Vec<PathBuf>, lock_dir: Option<PathBuf>) -> Self {
        Self {
            ide_name: ide_name.to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            workspace_folders,
            lock_dir,
            tools: tools::claude_code_tools(),
            auth_token: None,
        }
    }
}

/// A connected client as the host sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientSummary {
    pub id: ClientId,
    /// The client sent `notifications/initialized`.
    pub initialized: bool,
    /// `clientInfo` from `initialize` (`{name, version}`), or null.
    pub info: Value,
}

/// What the server reports to its host.
#[derive(Debug)]
pub enum ServerEvent {
    Connected {
        client: ClientId,
    },
    Initialized {
        client: ClientId,
        client_info: Value,
    },
    ToolCall(ToolCall),
    /// The client cancelled an earlier request (its JSON-RPC id).
    Cancelled {
        client: ClientId,
        request: Value,
    },
    Disconnected {
        client: ClientId,
    },
}

/// A `tools/call` for the host to answer through `reply`.
#[derive(Debug)]
pub struct ToolCall {
    pub client: ClientId,
    /// The JSON-RPC request id (matches [`ServerEvent::Cancelled::request`]).
    pub request: Value,
    pub name: String,
    /// Always an object or whatever non-null value the client sent.
    pub arguments: Value,
    pub reply: Reply,
}

/// The tool calls of one connection still waiting for their answer, and those the client
/// cancelled (`notifications/cancelled`): an answer to a cancelled request is not sent (MCP: the
/// receiver of a cancellation should not respond).
#[derive(Debug, Default)]
pub(crate) struct Inflight {
    pending: std::collections::HashSet<String>,
    cancelled: std::collections::HashSet<String>,
}

/// Shared by a connection's reader and the [`Reply`] handles of its calls.
pub(crate) type InflightRef = std::sync::Arc<std::sync::Mutex<Inflight>>;

/// A request id as a set key (`7` and `"7"` stay different).
fn id_key(id: &Value) -> String {
    id.to_string()
}

impl Inflight {
    /// A tool call was handed to the host.
    pub(crate) fn started(this: &InflightRef, id: &Value) {
        server::lock(this).pending.insert(id_key(id));
    }

    /// The client cancelled `id`; true if that call is still waiting (its answer will be dropped).
    pub(crate) fn cancel(this: &InflightRef, id: &Value) -> bool {
        let mut st = server::lock(this);
        let key = id_key(id);
        if st.pending.contains(&key) {
            st.cancelled.insert(key);
            true
        } else {
            false
        }
    }

    /// The answer to `id` is going out: whether it may be sent (false when it was cancelled).
    fn finish(this: &InflightRef, id: &Value) -> bool {
        let mut st = server::lock(this);
        let key = id_key(id);
        st.pending.remove(&key);
        !st.cancelled.remove(&key)
    }
}

/// Answers one tool call. Dropping it unanswered sends an error, so a client never waits on a
/// request the host forgot. Nothing is sent for a request the client cancelled.
#[derive(Debug)]
pub struct Reply {
    id: Value,
    out: Option<Sender<server::Outgoing>>,
    inflight: Option<InflightRef>,
}

impl Reply {
    #[cfg(test)]
    fn new(id: Value, out: Sender<server::Outgoing>) -> Self {
        Self { id, out: Some(out), inflight: None }
    }

    /// A reply that honours the connection's cancellations.
    pub(crate) fn tracked(id: Value, out: Sender<server::Outgoing>, inflight: &InflightRef) -> Self {
        Inflight::started(inflight, &id);
        Self { id, out: Some(out), inflight: Some(inflight.clone()) }
    }

    /// The JSON-RPC id being answered.
    pub fn request_id(&self) -> &Value {
        &self.id
    }

    /// Send the tool's result. Returns false if the client has gone.
    pub fn send(mut self, result: ToolResult) -> bool {
        let msg = rpc::response(self.id.clone(), result.to_value());
        self.send_raw(msg)
    }

    /// Answer with a JSON-RPC error instead of a result (protocol-level failures).
    pub fn error(mut self, message: impl Into<String>) -> bool {
        let msg = rpc::error_response(self.id.clone(), rpc::INTERNAL_ERROR, message);
        self.send_raw(msg)
    }

    fn send_raw(&mut self, msg: Value) -> bool {
        let Some(out) = self.out.take() else { return false };
        if let Some(inflight) = &self.inflight
            && !Inflight::finish(inflight, &self.id)
        {
            return false;
        }
        out.send(server::Outgoing::Text(msg.to_string())).is_ok()
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if self.out.is_some() {
            let msg = rpc::error_response(self.id.clone(), rpc::INTERNAL_ERROR, "the IDE dropped the request");
            self.send_raw(msg);
        }
    }
}

/// An MCP `CallToolResult`: text content items, optionally flagged as a tool error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub texts: Vec<String>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { texts: vec![text.into()], is_error: false }
    }

    pub fn texts<I: IntoIterator<Item = String>>(texts: I) -> Self {
        Self { texts: texts.into_iter().collect(), is_error: false }
    }

    /// One text item holding `value` serialized as JSON (how the IDE tools return data).
    pub fn json(value: &Value) -> Self {
        Self::text(value.to_string())
    }

    /// A tool-level failure the model can read and react to.
    pub fn error(message: impl Into<String>) -> Self {
        Self { texts: vec![message.into()], is_error: true }
    }

    pub fn to_value(&self) -> Value {
        let content: Vec<Value> = self.texts.iter().map(|t| json!({ "type": "text", "text": t })).collect();
        if self.is_error { json!({ "content": content, "isError": true }) } else { json!({ "content": content }) }
    }
}

/// A random version-4 UUID string from the OS random source.
pub fn new_token() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| McpError::Io(format!("no OS randomness: {e}")))?;
    if let Some(x) = b.get_mut(6) {
        *x = (*x & 0x0f) | 0x40;
    }
    if let Some(x) = b.get_mut(8) {
        *x = (*x & 0x3f) | 0x80;
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let part = |r: std::ops::Range<usize>| hex.get(r).unwrap_or_default().to_string();
    Ok(format!("{}-{}-{}-{}-{}", part(0..8), part(8..12), part(12..16), part(16..20), part(20..32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_uuid_v4_and_unique() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'));
    }

    #[test]
    fn tool_result_shape() {
        assert_eq!(ToolResult::text("hi").to_value(), json!({ "content": [{ "type": "text", "text": "hi" }] }));
        assert_eq!(ToolResult::error("no").to_value()["isError"], true);
        let v = ToolResult::texts(["FILE_SAVED".to_string(), "x".to_string()]).to_value();
        assert_eq!(v["content"][1]["text"], "x");
    }

    #[test]
    fn dropped_reply_answers_with_an_error() {
        let (tx, rx) = crossbeam_channel::unbounded();
        drop(Reply::new(json!(7), tx.clone()));
        let server::Outgoing::Text(t) = rx.try_recv().unwrap();
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["id"], 7);
        assert!(v.get("error").is_some());
        assert!(Reply::new(json!(8), tx).send(ToolResult::text("ok")));
        let server::Outgoing::Text(t) = rx.try_recv().unwrap();
        assert!(t.contains("\"ok\""));
        assert!(rx.try_recv().is_err(), "a sent reply must not also send an error on drop");
    }

    #[test]
    fn cancelled_requests_get_no_answer() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let inflight = InflightRef::default();
        let a = Reply::tracked(json!(1), tx.clone(), &inflight);
        let b = Reply::tracked(json!("1"), tx.clone(), &inflight);
        assert!(Inflight::cancel(&inflight, &json!(1)));
        assert!(!Inflight::cancel(&inflight, &json!(99)), "nothing waits for 99");
        assert!(!a.send(ToolResult::text("late")), "the answer to a cancelled call is dropped");
        assert!(rx.try_recv().is_err());
        // A different id (a string "1") is still answered, and dropping it answers with an error.
        drop(b);
        let server::Outgoing::Text(t) = rx.try_recv().unwrap();
        assert!(t.contains("dropped"), "{t}");
        // A cancel after the answer went out does not affect a later request with the same id.
        let c = Reply::tracked(json!(2), tx.clone(), &inflight);
        assert!(c.send(ToolResult::text("ok")));
        assert!(!Inflight::cancel(&inflight, &json!(2)));
        let d = Reply::tracked(json!(2), tx, &inflight);
        assert!(d.send(ToolResult::text("again")));
        assert_eq!(rx.try_iter().count(), 2);
        // A cancelled call whose reply is dropped sends nothing either.
        let (tx, rx) = crossbeam_channel::unbounded();
        let e = Reply::tracked(json!(3), tx, &inflight);
        Inflight::cancel(&inflight, &json!(3));
        drop(e);
        assert!(rx.try_recv().is_err());
    }
}
