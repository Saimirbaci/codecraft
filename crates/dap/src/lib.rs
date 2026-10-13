//! CodeCraft Debug Adapter Protocol client and adapter discovery (lldb-dap, CodeLLDB, debugpy,
//! js-debug) (L0, no workspace deps).
//!
//! [`DapClient`] talks to a debug adapter over stdio (or TCP for adapters that only listen on a
//! port): `Content-Length` framing, request/response correlation by `seq`/`request_seq`, events,
//! and reverse requests (`runInTerminal` is refused so adapters use their own console;
//! `startDebugging` is handed to the caller). Everything arrives as [`DapEvent`]s on a channel the
//! caller drains; no call blocks waiting for the adapter.
//!
//! Adapters run in their own process group; stopping the client kills the group, so a debuggee
//! never outlives its session.
//!
//! The protocol follows the public DAP specification
//! (<https://microsoft.github.io/debug-adapter-protocol/specification>).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod client;
pub mod discover;
pub mod launch;
pub mod processes;
mod transport;
pub mod types;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use serde_json::{Value, json};

pub use discover::Unavailable;
pub use types::*;

#[derive(Debug, thiserror::Error)]
pub enum DapError {
    #[error("failed to start {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the debug adapter is not running")]
    NotRunning,
    #[error("debug adapter protocol error: {0}")]
    Protocol(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DapError>;

/// A request we sent (its `seq`); responses carry it back as `request_seq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub i64);

/// The answer to one of our requests.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub id: RequestId,
    pub command: String,
    /// The body on success (`null` when there is none), the error text on failure.
    pub result: std::result::Result<Value, String>,
}

/// Everything the adapter tells us, in arrival order. `Exited` is always last.
#[derive(Clone, Debug, PartialEq)]
pub enum DapEvent {
    Response(Response),
    /// A DAP event (`stopped`, `output`, `initialized`, …) with its body (`null` when absent).
    Event {
        event: String,
        body: Value,
    },
    /// A reverse request the client did not answer itself (`startDebugging`); reply with
    /// [`DapClient::respond`].
    Request {
        seq: i64,
        command: String,
        arguments: Value,
    },
    /// A connection problem (malformed message, unexpected exit with the stderr tail).
    Error(String),
    /// The connection is over (the adapter exited or closed the stream).
    Exited {
        code: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AdapterKind {
    LldbDap,
    CodeLldb,
    Debugpy,
    JsDebug,
    /// Anything else (tests, user-configured adapters).
    Other,
}

/// How the client talks to the adapter process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterTransport {
    /// DAP over the adapter's stdin/stdout.
    Stdio,
    /// The adapter listens on a TCP port on 127.0.0.1; `port_arg` is appended to the arguments
    /// with `{port}` replaced by a free port.
    Tcp { port_arg: Vec<String> },
}

/// How to start a debug adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCommand {
    /// Display name (`lldb-dap`, `debugpy`, …).
    pub name: String,
    pub kind: AdapterKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub transport: AdapterTransport,
}

impl AdapterCommand {
    pub fn stdio(name: &str, kind: AdapterKind, program: impl Into<PathBuf>) -> Self {
        Self {
            name: name.to_owned(),
            kind,
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            transport: AdapterTransport::Stdio,
        }
    }
}

/// `initialize` arguments CodeCraft sends: 1-based lines and columns, paths, no terminal.
pub fn initialize_arguments(adapter_id: &str) -> Value {
    json!({
        "clientID": "codecraft",
        "clientName": "CodeCraft",
        "adapterID": adapter_id,
        "locale": "en",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "pathFormat": "path",
        "supportsVariableType": true,
        "supportsVariablePaging": false,
        "supportsRunInTerminalRequest": false,
        "supportsMemoryReferences": false,
        "supportsProgressReporting": false,
        "supportsInvalidatedEvent": false,
        "supportsStartDebuggingRequest": true,
        "supportsArgsCanBeInterpretedByShell": false,
    })
}

/// A connection to a debug adapter. Dropping it kills a spawned adapter and its process group.
pub struct DapClient {
    inner: Arc<client::Inner>,
}

impl std::fmt::Debug for DapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DapClient").field("pid", &self.inner.pid()).finish()
    }
}

impl DapClient {
    /// Start an adapter process and connect to it. TCP adapters are waited for (a few seconds at
    /// most), so call this from a worker thread.
    pub fn spawn(adapter: &AdapterCommand) -> Result<(Self, Receiver<DapEvent>)> {
        let (inner, events) = client::spawn(adapter)?;
        Ok((Self { inner }, events))
    }

    /// Talk DAP over an existing stream (tests use pipes; also a TCP connection to a running
    /// adapter).
    pub fn connect(
        name: &str,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> Result<(Self, Receiver<DapEvent>)> {
        let (inner, events) = client::connect(name, Box::new(reader), Box::new(writer))?;
        Ok((Self { inner }, events))
    }

    /// Connect to an adapter already listening on `127.0.0.1:port` (js-debug child sessions).
    pub fn connect_tcp(name: &str, port: u16, timeout: Duration) -> Result<(Self, Receiver<DapEvent>)> {
        let stream =
            std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), timeout)?;
        let _ = stream.set_nodelay(true);
        let writer = stream.try_clone()?;
        Self::connect(name, stream, writer)
    }

    /// The adapter's process id (`None` for connected streams).
    pub fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    /// Whether the connection is still open.
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    /// The adapter's last lines of stderr.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.inner.stderr_tail()
    }

    /// Send a request; the answer arrives as [`DapEvent::Response`] with this id.
    pub fn request(&self, command: &str, arguments: Value) -> Result<RequestId> {
        self.inner.request(command, arguments)
    }

    /// Answer a reverse request ([`DapEvent::Request`]).
    pub fn respond(&self, request_seq: i64, command: &str, result: std::result::Result<Value, String>) -> Result<()> {
        self.inner.respond(request_seq, command, result)
    }

    /// End the session: `disconnect` (terminating the debuggee), then kill the adapter's process
    /// group if it is still there after `grace`. Returns at once; [`DapEvent::Exited`] follows.
    pub fn terminate(&self, grace: Duration) {
        self.inner.terminate(grace);
    }

    /// End an attach session: `disconnect` leaving the debuggee running (detach), then kill the
    /// adapter's process group if it is still there after `grace`.
    pub fn detach(&self, grace: Duration) {
        self.inner.terminate_with(grace, false);
    }

    /// Kill the adapter's process group now.
    pub fn kill(&self) {
        self.inner.kill();
    }

    // ----- typed requests -------------------------------------------------------------------

    pub fn initialize(&self, adapter_id: &str) -> Result<RequestId> {
        self.request("initialize", initialize_arguments(adapter_id))
    }

    pub fn launch(&self, arguments: Value) -> Result<RequestId> {
        self.request("launch", arguments)
    }

    pub fn attach(&self, arguments: Value) -> Result<RequestId> {
        self.request("attach", arguments)
    }

    /// Replace all breakpoints in one source file (an empty list clears them).
    pub fn set_breakpoints(&self, path: &Path, breakpoints: &[SourceBreakpoint]) -> Result<RequestId> {
        self.request(
            "setBreakpoints",
            json!({
                "source": Source::from_path(path),
                "breakpoints": breakpoints,
                "lines": breakpoints.iter().map(|b| b.line).collect::<Vec<_>>(),
                "sourceModified": false,
            }),
        )
    }

    pub fn set_function_breakpoints(&self, breakpoints: &[FunctionBreakpoint]) -> Result<RequestId> {
        self.request("setFunctionBreakpoints", json!({ "breakpoints": breakpoints }))
    }

    pub fn set_exception_breakpoints(&self, filters: &[String]) -> Result<RequestId> {
        self.request("setExceptionBreakpoints", json!({ "filters": filters }))
    }

    pub fn configuration_done(&self) -> Result<RequestId> {
        self.request("configurationDone", json!({}))
    }

    pub fn threads(&self) -> Result<RequestId> {
        self.request("threads", Value::Null)
    }

    pub fn stack_trace(&self, thread_id: i64, start_frame: i64, levels: i64) -> Result<RequestId> {
        self.request("stackTrace", json!({ "threadId": thread_id, "startFrame": start_frame, "levels": levels }))
    }

    pub fn scopes(&self, frame_id: i64) -> Result<RequestId> {
        self.request("scopes", json!({ "frameId": frame_id }))
    }

    pub fn variables(&self, variables_reference: i64) -> Result<RequestId> {
        self.request("variables", json!({ "variablesReference": variables_reference }))
    }

    /// `context` is `watch`, `repl`, `hover` or `clipboard`.
    pub fn evaluate(&self, expression: &str, frame_id: Option<i64>, context: &str) -> Result<RequestId> {
        let mut args = json!({ "expression": expression, "context": context });
        if let (Some(frame), Some(map)) = (frame_id, args.as_object_mut()) {
            map.insert("frameId".into(), frame.into());
        }
        self.request("evaluate", args)
    }

    pub fn continue_(&self, thread_id: i64) -> Result<RequestId> {
        self.request("continue", json!({ "threadId": thread_id }))
    }

    pub fn next(&self, thread_id: i64) -> Result<RequestId> {
        self.request("next", json!({ "threadId": thread_id }))
    }

    pub fn step_in(&self, thread_id: i64) -> Result<RequestId> {
        self.request("stepIn", json!({ "threadId": thread_id }))
    }

    pub fn step_out(&self, thread_id: i64) -> Result<RequestId> {
        self.request("stepOut", json!({ "threadId": thread_id }))
    }

    pub fn pause(&self, thread_id: i64) -> Result<RequestId> {
        self.request("pause", json!({ "threadId": thread_id }))
    }

    /// `disconnect` as a plain request (see [`DapClient::terminate`] for ending a session).
    pub fn disconnect(&self, terminate_debuggee: bool) -> Result<RequestId> {
        self.request("disconnect", json!({ "restart": false, "terminateDebuggee": terminate_debuggee }))
    }

    pub fn terminate_request(&self) -> Result<RequestId> {
        self.request("terminate", json!({ "restart": false }))
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        self.inner.kill();
    }
}

/// Send `signal` (`TERM`, `KILL`) to the process group `pgid` with the `kill` command. False if
/// `kill` could not be run. Refuses group 0 and 1 (ours and init's).
pub fn kill_process_group(pgid: u32, signal: &str) -> bool {
    if pgid <= 1 || !cfg!(unix) {
        return false;
    }
    run_kill(signal, &format!("-{pgid}"))
}

/// Send `signal` to one process (a debuggee that left its adapter's process group).
pub fn kill_process(pid: u32, signal: &str) -> bool {
    if pid <= 1 || pid == std::process::id() || !cfg!(unix) {
        return false;
    }
    run_kill(signal, &pid.to_string())
}

fn run_kill(signal: &str, target: &str) -> bool {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) => {
            if !status.success() {
                log::debug!("kill -{signal} {target}: {status}");
            }
            true
        }
        Err(e) => {
            log::warn!("cannot run `kill`: {e}");
            false
        }
    }
}
