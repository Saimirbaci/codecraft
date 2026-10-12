//! The connection: shared state, the writer/reader/stderr/reaper threads and message dispatch.
//!
//! Every thread holds an `Arc<Inner>`; [`crate::LspClient`] is the handle whose `Drop` stops the
//! writer and kills a spawned server. Threads are never joined, so no call blocks longer than the
//! timeout it was given. Events go through one gate so `Exited` is sent exactly once and last.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde_json::{Map, Value, json};

use crate::init::{self, Root};
use crate::transport::{self, FrameReader, Incoming};
use crate::{LspError, ProgressUpdate, RequestId, ResponseError, Result, ServerConfig, ServerEvent};

/// Messages held back until the server has answered `initialize`.
const MAX_QUEUED: usize = 4096;
const STDERR_TAIL_LINES: usize = 50;
const MAX_STDERR_LINE_BYTES: u64 = 16 * 1024;
const MAX_STDERR_LINE_CHARS: usize = 1000;
/// After this many malformed messages in a row the server is considered broken.
const MAX_CONSECUTIVE_MALFORMED: u32 = 32;
const MAX_CONFIGURATION_ITEMS: usize = 1024;
const MAX_PROGRESS_TOKENS: usize = 1024;
const REAP_POLL: Duration = Duration::from_millis(50);
/// How long a server may live on after closing its stdout before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long the reaper lets the reader and stderr drain after the process exited.
const DRAIN_GRACE: Duration = Duration::from_secs(1);
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Stands in for "no deadline" (a `Duration::MAX` timeout).
const FOREVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// LSP `ServerNotInitialized`.
const SERVER_NOT_INITIALIZED: i64 = -32002;
/// JSON-RPC `InvalidParams`.
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC `InternalError`, for error responses without a code.
const INTERNAL_ERROR: i64 = -32603;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A point in time `timeout` from now; a timeout too large to represent never expires.
pub(crate) struct Deadline(Option<Instant>);

impl Deadline {
    pub(crate) fn after(timeout: Duration) -> Self {
        Self(Instant::now().checked_add(timeout))
    }

    pub(crate) fn remaining(&self) -> Duration {
        match self.0 {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => FOREVER,
        }
    }

    pub(crate) fn expired(&self) -> bool {
        self.0.is_some_and(|at| Instant::now() >= at)
    }
}

/// A one-way flag threads can wait on.
#[derive(Default)]
struct Latch {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Latch {
    fn set(&self) {
        *lock(&self.set) = true;
        self.changed.notify_all();
    }

    fn is_set(&self) -> bool {
        *lock(&self.set)
    }

    /// Waits until set or `timeout` passes; true when set.
    fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Deadline::after(timeout);
        let mut set = lock(&self.set);
        while !*set {
            let left = deadline.remaining();
            if left.is_zero() {
                return false;
            }
            set = match self.changed.wait_timeout(set, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }
}

enum WriterMsg {
    Send(Value),
    Stop,
}

struct EventSink {
    tx: Sender<ServerEvent>,
    exited: bool,
}

#[derive(Default)]
struct State {
    next_id: i64,
    initialize_id: Option<i64>,
    initialized: bool,
    init_error: Option<String>,
    capabilities: Option<lsp_types::ServerCapabilities>,
    /// Requests and notifications issued before the `initialize` response, in order.
    queue: Vec<Value>,
    /// In-flight request id → method.
    pending: HashMap<i64, String>,
    shutdown_waiter: Option<(i64, Sender<()>)>,
    /// Work-done progress token → the title its `begin` announced.
    progress_titles: HashMap<String, String>,
}

impl State {
    fn allocate_id(&mut self) -> Result<i64> {
        let id = self.next_id.max(1);
        self.next_id = id.checked_add(1).ok_or_else(|| LspError::Protocol("request ids exhausted".to_owned()))?;
        Ok(id)
    }
}

pub(crate) struct Inner {
    state: Mutex<State>,
    events: Mutex<EventSink>,
    writer: Sender<WriterMsg>,
    child: Option<Mutex<Child>>,
    stderr_tail: Mutex<VecDeque<String>>,
    shutdown_requested: AtomicBool,
    reader_done: Latch,
    process_exited: Latch,
    stderr_done: Latch,
    workspace_folders: Value,
    configuration: Option<Value>,
}

struct Streams {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<Child>,
    stderr: Option<ChildStderr>,
}

pub(crate) fn connect(
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    root: PathBuf,
    initialization_options: Option<Value>,
    configuration: Option<Value>,
) -> Result<(Arc<Inner>, Receiver<ServerEvent>)> {
    let root = Root::new(&root)?;
    start(Streams { reader, writer, child: None, stderr: None }, &root, initialization_options, configuration)
}

pub(crate) fn spawn(config: ServerConfig) -> Result<(Arc<Inner>, Receiver<ServerEvent>)> {
    let root = Root::new(&config.root)?;
    let command_name = config.command.display().to_string();
    let mut command = Command::new(&config.command);
    command.args(&config.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (key, value) in &config.env {
        command.env(key, value);
    }
    if root.path.is_dir() {
        command.current_dir(&root.path);
    }
    crate::discover::hide_console(&mut command);
    // Own process group, so the server's helpers (proc-macro servers, cargo check, …) can be
    // killed together with it and never outlive the IDE.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|source| LspError::Spawn { command: command_name.clone(), source })?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(LspError::Spawn {
            command: command_name,
            source: io::Error::other("the server's stdio pipes are unavailable"),
        });
    };
    log::info!("started language server {command_name} (pid {})", child.id());
    let streams =
        Streams { reader: Box::new(stdout), writer: Box::new(stdin), child: Some(child), stderr: Some(stderr) };
    start(streams, &root, config.initialization_options, config.configuration)
}

fn start(
    streams: Streams,
    root: &Root,
    initialization_options: Option<Value>,
    configuration: Option<Value>,
) -> Result<(Arc<Inner>, Receiver<ServerEvent>)> {
    let Streams { reader, writer, child, stderr } = streams;
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (writer_tx, writer_rx) = crossbeam_channel::unbounded();
    let inner = Arc::new(Inner {
        state: Mutex::new(State::default()),
        events: Mutex::new(EventSink { tx: event_tx, exited: false }),
        writer: writer_tx,
        child: child.map(Mutex::new),
        stderr_tail: Mutex::new(VecDeque::new()),
        shutdown_requested: AtomicBool::new(false),
        reader_done: Latch::default(),
        process_exited: Latch::default(),
        stderr_done: Latch::default(),
        workspace_folders: root.workspace_folders(),
        configuration,
    });
    {
        // `initialize` goes first and straight to the writer; everything else waits for its answer.
        let mut state = lock(&inner.state);
        let id = state.allocate_id()?;
        state.initialize_id = Some(id);
        let params = init::initialize_params(root, initialization_options);
        let _ = inner.writer.send(WriterMsg::Send(request_message(id, "initialize", params)));
    }
    if let Err(e) = inner.start_threads(writer_rx, writer, reader, stderr) {
        inner.close();
        if let Some(child) = &inner.child {
            // The reaper may not be running; collect the killed process ourselves.
            let _ = lock(child).wait();
        }
        return Err(LspError::Io(e));
    }
    Ok((inner, event_rx))
}

impl Inner {
    fn start_threads(
        self: &Arc<Self>,
        writer_rx: Receiver<WriterMsg>,
        writer: Box<dyn Write + Send>,
        reader: Box<dyn Read + Send>,
        stderr: Option<ChildStderr>,
    ) -> io::Result<()> {
        let me = Arc::clone(self);
        thread::Builder::new().name("lsp-writer".to_owned()).spawn(move || me.writer_loop(&writer_rx, writer))?;
        let me = Arc::clone(self);
        thread::Builder::new().name("lsp-reader".to_owned()).spawn(move || me.reader_loop(reader))?;
        match stderr {
            Some(stderr) => {
                let me = Arc::clone(self);
                thread::Builder::new().name("lsp-stderr".to_owned()).spawn(move || me.stderr_loop(stderr))?;
            }
            None => self.stderr_done.set(),
        }
        if self.child.is_some() {
            let me = Arc::clone(self);
            thread::Builder::new().name("lsp-reaper".to_owned()).spawn(move || me.reaper_loop())?;
        }
        Ok(())
    }

    // ----- public API backing -------------------------------------------------------------

    pub(crate) fn is_initialized(&self) -> bool {
        lock(&self.state).initialized
    }

    pub(crate) fn capabilities(&self) -> Option<lsp_types::ServerCapabilities> {
        lock(&self.state).capabilities.clone()
    }

    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        lock(&self.stderr_tail).iter().cloned().collect()
    }

    pub(crate) fn request(&self, method: &str, params: Value) -> Result<RequestId> {
        self.ensure_open()?;
        let mut state = lock(&self.state);
        if let Some(error) = &state.init_error {
            return Err(LspError::Protocol(format!("the language server failed to initialize: {error}")));
        }
        let id = state.allocate_id()?;
        self.send_or_queue(&mut state, request_message(id, method, params))?;
        state.pending.insert(id, method.to_owned());
        Ok(RequestId(id))
    }

    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.ensure_open()?;
        let mut state = lock(&self.state);
        if let Some(error) = &state.init_error {
            return Err(LspError::Protocol(format!("the language server failed to initialize: {error}")));
        }
        self.send_or_queue(&mut state, notification_message(method, params))
    }

    /// Answers a server→client request; never queued (the server is waiting for it).
    pub(crate) fn respond(&self, id: Value, result: std::result::Result<Value, ResponseError>) -> Result<()> {
        if self.is_exited() || self.reader_done.is_set() {
            return Err(LspError::NotRunning);
        }
        self.send_now(response_message(id, result))
    }

    /// `shutdown` (if the server is initialized), `exit`, close its stdin, wait for it to go away
    /// within `timeout`, then kill it. Only the first call does anything.
    pub(crate) fn shutdown(&self, timeout: Duration) -> Result<()> {
        if self.shutdown_requested.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let deadline = Deadline::after(timeout);
        if !self.is_exited() {
            let waiter = {
                let mut state = lock(&self.state);
                // Never delivered: the server is going away.
                state.queue.clear();
                if state.initialized && !self.reader_done.is_set() {
                    let id = state.allocate_id()?;
                    let (tx, rx) = crossbeam_channel::bounded(1);
                    state.shutdown_waiter = Some((id, tx));
                    let _ = self.writer.send(WriterMsg::Send(request_message(id, "shutdown", Value::Null)));
                    Some(rx)
                } else {
                    None
                }
            };
            if let Some(rx) = waiter {
                if rx.recv_timeout(deadline.remaining()).is_err() {
                    log::warn!("language server did not answer shutdown in time");
                }
                lock(&self.state).shutdown_waiter = None;
            }
            let _ = self.writer.send(WriterMsg::Send(notification_message("exit", Value::Null)));
            let _ = self.writer.send(WriterMsg::Stop);
        }
        match &self.child {
            Some(child) => {
                if !self.process_exited.wait_timeout(deadline.remaining()) {
                    log::warn!("language server did not exit in time; killing it");
                    kill_tree(child);
                    self.process_exited.wait_timeout(KILL_GRACE);
                }
            }
            None => {
                if !self.reader_done.wait_timeout(deadline.remaining()) {
                    // The peer never closed the stream; the connection is over for us anyway.
                    self.emit_exited(None);
                }
            }
        }
        Ok(())
    }

    /// The handle was dropped: stop writing (closing the server's stdin) and kill the process.
    /// A requested stop, so its exit is not reported as unexpected.
    pub(crate) fn close(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        let _ = self.writer.send(WriterMsg::Stop);
        if let Some(child) = &self.child
            && !self.process_exited.is_set()
        {
            kill_tree(child);
        }
    }

    // ----- sending -----------------------------------------------------------------------

    fn ensure_open(&self) -> Result<()> {
        if self.shutdown_requested.load(Ordering::SeqCst) || self.is_exited() || self.reader_done.is_set() {
            Err(LspError::NotRunning)
        } else {
            Ok(())
        }
    }

    fn send_now(&self, message: Value) -> Result<()> {
        self.writer.send(WriterMsg::Send(message)).map_err(|_| LspError::NotRunning)
    }

    /// Sends now if initialized, else queues. Called with the state lock held, so the order of
    /// messages is the order of calls even across the `initialized` flush.
    fn send_or_queue(&self, state: &mut State, message: Value) -> Result<()> {
        if state.initialized {
            return self.send_now(message);
        }
        if state.queue.len() >= MAX_QUEUED {
            return Err(LspError::Protocol(format!(
                "more than {MAX_QUEUED} messages queued before the language server initialized"
            )));
        }
        state.queue.push(message);
        Ok(())
    }

    // ----- events ------------------------------------------------------------------------

    fn emit(&self, event: ServerEvent) {
        let sink = lock(&self.events);
        if !sink.exited {
            let _ = sink.tx.send(event);
        }
    }

    fn emit_exited(&self, code: Option<i32>) {
        {
            let mut sink = lock(&self.events);
            if sink.exited {
                return;
            }
            sink.exited = true;
            let _ = sink.tx.send(ServerEvent::Exited { code });
        }
        let _ = self.writer.send(WriterMsg::Stop);
        // Wake a shutdown() waiting for an answer that will never come.
        lock(&self.state).shutdown_waiter = None;
    }

    fn is_exited(&self) -> bool {
        lock(&self.events).exited
    }

    // ----- threads -----------------------------------------------------------------------

    fn writer_loop(self: Arc<Self>, rx: &Receiver<WriterMsg>, writer: Box<dyn Write + Send>) {
        let mut out = BufWriter::new(writer);
        while let Ok(WriterMsg::Send(message)) = rx.recv() {
            log::trace!("lsp → {message}");
            if let Err(e) = transport::write_message(&mut out, &message) {
                if !self.shutdown_requested.load(Ordering::SeqCst) {
                    self.emit(ServerEvent::Error(format!("failed to write to the language server: {e}")));
                }
                return;
            }
        }
        // Stopped: dropping `out` closes the server's stdin.
    }

    fn reader_loop(self: Arc<Self>, reader: Box<dyn Read + Send>) {
        let mut frames = FrameReader::new(BufReader::with_capacity(64 * 1024, reader));
        let mut malformed = 0u32;
        loop {
            match frames.next_message() {
                Incoming::Message(message) => {
                    malformed = 0;
                    self.dispatch(message);
                }
                Incoming::Malformed(problem) => {
                    self.emit(ServerEvent::Error(format!("malformed message from the language server: {problem}")));
                    malformed += 1;
                    if malformed >= MAX_CONSECUTIVE_MALFORMED {
                        self.emit(ServerEvent::Error(
                            "too many malformed messages from the language server; disconnecting".to_owned(),
                        ));
                        break;
                    }
                }
                Incoming::Eof => break,
                Incoming::Failed(e) => {
                    if !self.shutdown_requested.load(Ordering::SeqCst) {
                        self.emit(ServerEvent::Error(format!("failed to read from the language server: {e}")));
                    }
                    break;
                }
            }
        }
        self.reader_finished();
    }

    fn reader_finished(&self) {
        self.reader_done.set();
        lock(&self.state).shutdown_waiter = None;
        let Some(child) = &self.child else {
            self.emit_exited(None);
            return;
        };
        // The reaper reports the exit; make sure there is one to report.
        if !self.process_exited.wait_timeout(EXIT_GRACE) {
            log::warn!("language server closed its output but is still running; killing it");
            kill_tree(child);
            if !self.process_exited.wait_timeout(KILL_GRACE) {
                self.emit_exited(None);
            }
        }
    }

    fn stderr_loop(self: Arc<Self>, stderr: ChildStderr) {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.by_ref().take(MAX_STDERR_LINE_BYTES).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => self.record_stderr(&line),
            }
        }
        self.stderr_done.set();
    }

    fn record_stderr(&self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        log::info!(target: "codecraft_lsp::stderr", "{text}");
        let line = match text.char_indices().nth(MAX_STDERR_LINE_CHARS) {
            Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or(text)),
            None => text.to_owned(),
        };
        let mut tail = lock(&self.stderr_tail);
        while tail.len() >= STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    /// The server's process id (spawned servers only).
    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| lock(c).id())
    }

    fn reaper_loop(self: Arc<Self>) {
        let Some(child) = &self.child else { return };
        let status = loop {
            let polled = lock(child).try_wait();
            match polled {
                Ok(Some(status)) => break Some(status),
                Ok(None) => thread::sleep(REAP_POLL),
                Err(e) => {
                    log::warn!("could not wait for the language server: {e}");
                    break None;
                }
            }
        };
        self.process_exited.set();
        // Helpers the server started (proc-macro servers, cargo) share its process group:
        // end any that are left so nothing outlives the server.
        signal_group(lock(child).id(), "-TERM");
        // Deliver what the server wrote before it went away, and let stderr catch up.
        self.reader_done.wait_timeout(DRAIN_GRACE);
        self.stderr_done.wait_timeout(DRAIN_GRACE);
        if !self.shutdown_requested.load(Ordering::SeqCst) {
            self.emit(ServerEvent::Error(self.exit_report(status)));
        }
        self.emit_exited(status.and_then(|status| status.code()));
    }

    fn exit_report(&self, status: Option<ExitStatus>) -> String {
        let mut report = match status {
            Some(status) => format!("language server exited unexpectedly ({status})"),
            None => "language server exited unexpectedly".to_owned(),
        };
        let tail = lock(&self.stderr_tail);
        if !tail.is_empty() {
            report.push_str("; last stderr output:");
            for line in tail.iter() {
                report.push('\n');
                report.push_str(line);
            }
        }
        report
    }

    // ----- dispatch ----------------------------------------------------------------------

    fn dispatch(&self, message: Value) {
        log::trace!("lsp ← {message}");
        let Value::Object(mut message) = message else {
            self.emit(ServerEvent::Error(format!("unexpected message from the language server: {message}")));
            return;
        };
        let method = match message.remove("method") {
            Some(Value::String(method)) => Some(method),
            Some(other) => {
                self.emit(ServerEvent::Error(format!("message with a non-string method: {other}")));
                return;
            }
            None => None,
        };
        let id = message.remove("id").filter(|id| !id.is_null());
        let params = message.remove("params").unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.on_server_request(id, method, params),
            (Some(method), None) => self.on_notification(method, params),
            (None, id) => self.on_response(id, message.remove("result"), message.remove("error")),
        }
    }

    fn on_response(&self, id: Option<Value>, result: Option<Value>, error: Option<Value>) {
        let outcome = match error.filter(|error| !error.is_null()) {
            Some(error) => Err(response_error(&error)),
            None => Ok(result.unwrap_or(Value::Null)),
        };
        let Some(id) = id.as_ref().and_then(id_as_i64) else {
            match outcome {
                Err(e) => self.emit(ServerEvent::Error(format!("language server error {}: {}", e.code, e.message))),
                Ok(_) => log::warn!("ignoring a response without a usable id"),
            }
            return;
        };
        enum Route {
            Initialize,
            Shutdown(Sender<()>),
            Request(String),
            Unknown,
        }
        let route = {
            let mut state = lock(&self.state);
            if state.initialize_id == Some(id) {
                state.initialize_id = None;
                Route::Initialize
            } else if state.shutdown_waiter.as_ref().is_some_and(|(waiting, _)| *waiting == id) {
                state.shutdown_waiter.take().map_or(Route::Unknown, |(_, tx)| Route::Shutdown(tx))
            } else {
                state.pending.remove(&id).map_or(Route::Unknown, Route::Request)
            }
        };
        match route {
            Route::Initialize => self.on_initialize_response(outcome),
            Route::Shutdown(tx) => {
                let _ = tx.send(());
            }
            Route::Request(method) => self.emit(ServerEvent::Response { id: RequestId(id), method, result: outcome }),
            Route::Unknown => log::warn!("ignoring a response to unknown request {id}"),
        }
    }

    fn on_initialize_response(&self, outcome: std::result::Result<Value, ResponseError>) {
        match outcome {
            Ok(result) => {
                let (capabilities, warning) = parse_capabilities(result);
                if let Some(warning) = warning {
                    self.emit(ServerEvent::Error(warning));
                }
                {
                    let mut state = lock(&self.state);
                    state.capabilities = Some(capabilities.clone());
                    state.initialized = true;
                    if !self.shutdown_requested.load(Ordering::SeqCst) {
                        let _ = self.writer.send(WriterMsg::Send(notification_message("initialized", json!({}))));
                        for message in state.queue.drain(..) {
                            let _ = self.writer.send(WriterMsg::Send(message));
                        }
                    }
                }
                self.emit(ServerEvent::Initialized(Box::new(capabilities)));
            }
            Err(error) => {
                let failed: Vec<(i64, String)> = {
                    let mut state = lock(&self.state);
                    state.init_error = Some(error.message.clone());
                    let queued = std::mem::take(&mut state.queue);
                    queued
                        .iter()
                        .filter_map(|message| message.get("id").and_then(Value::as_i64))
                        .filter_map(|id| state.pending.remove(&id).map(|method| (id, method)))
                        .collect()
                };
                self.emit(ServerEvent::Error(format!(
                    "the language server failed to initialize: {} (code {})",
                    error.message, error.code
                )));
                for (id, method) in failed {
                    let result = Err(ResponseError {
                        code: SERVER_NOT_INITIALIZED,
                        message: "the language server failed to initialize".to_owned(),
                    });
                    self.emit(ServerEvent::Response { id: RequestId(id), method, result });
                }
            }
        }
    }

    fn on_notification(&self, method: String, params: Value) {
        let event = match method.as_str() {
            "textDocument/publishDiagnostics" => match serde_json::from_value(params) {
                Ok(params) => ServerEvent::Diagnostics(params),
                Err(e) => ServerEvent::Error(format!("invalid {method} notification: {e}")),
            },
            "$/progress" => match self.progress_update(&params) {
                Some(update) => ServerEvent::Progress(update),
                None => ServerEvent::Notification { method, params },
            },
            "window/showMessage" => match serde_json::from_value::<lsp_types::ShowMessageParams>(params) {
                Ok(params) => ServerEvent::ShowMessage { level: params.typ, message: params.message },
                Err(e) => ServerEvent::Error(format!("invalid {method} notification: {e}")),
            },
            "window/logMessage" => match serde_json::from_value::<lsp_types::LogMessageParams>(params) {
                Ok(params) => ServerEvent::Log { level: params.typ, message: params.message },
                Err(e) => ServerEvent::Error(format!("invalid {method} notification: {e}")),
            },
            _ => ServerEvent::Notification { method, params },
        };
        self.emit(event);
    }

    /// A work-done `$/progress` payload; `None` for anything else (e.g. partial results).
    fn progress_update(&self, params: &Value) -> Option<ProgressUpdate> {
        let token = match params.get("token")? {
            Value::String(token) => token.clone(),
            Value::Number(token) => token.to_string(),
            _ => return None,
        };
        let value = params.get("value")?;
        let kind = value.get("kind")?.as_str()?;
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let message = text("message");
        let percentage = value.get("percentage").and_then(|p| {
            // `as` saturates for floats; the spec says unsigned integer but be lenient.
            p.as_u64().or_else(|| p.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))
        });
        let percentage = percentage.map(|p| u32::try_from(p).unwrap_or(u32::MAX));
        let mut state = lock(&self.state);
        let titles = &mut state.progress_titles;
        let (title, done) = match kind {
            "begin" => {
                let title = text("title");
                if titles.len() >= MAX_PROGRESS_TOKENS {
                    titles.clear();
                }
                if let Some(title) = &title {
                    titles.insert(token.clone(), title.clone());
                }
                (title, false)
            }
            "report" => (titles.get(&token).cloned(), false),
            "end" => (titles.remove(&token), true),
            _ => return None,
        };
        Some(ProgressUpdate { token, title, message, percentage, done })
    }

    fn on_server_request(&self, id: Value, method: String, params: Value) {
        let reply = match method.as_str() {
            "workspace/configuration" => Some(self.configuration_reply(&params)),
            "window/workDoneProgress/create" | "client/registerCapability" | "client/unregisterCapability" => {
                Some(Ok(Value::Null))
            }
            "workspace/workspaceFolders" => Some(Ok(self.workspace_folders.clone())),
            _ => None,
        };
        match reply {
            Some(result) => {
                if self.send_now(response_message(id, result)).is_err() {
                    log::debug!("could not answer {method}: the writer is gone");
                }
            }
            None => self.emit(ServerEvent::ServerRequest { id, method, params }),
        }
    }

    fn configuration_reply(&self, params: &Value) -> std::result::Result<Value, ResponseError> {
        let items = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
        if items > MAX_CONFIGURATION_ITEMS {
            return Err(ResponseError {
                code: INVALID_PARAMS,
                message: format!("too many configuration items ({items})"),
            });
        }
        let value = self.configuration.clone().unwrap_or(Value::Null);
        Ok(Value::Array(vec![value; items]))
    }
}

/// Server capabilities from an `initialize` result. If lsp-types cannot parse them all, keep the
/// ones it can and report the rest rather than losing everything.
fn parse_capabilities(result: Value) -> (lsp_types::ServerCapabilities, Option<String>) {
    let Some(capabilities) = (match result {
        Value::Object(mut result) => result.remove("capabilities"),
        _ => None,
    }) else {
        return (Default::default(), Some("the initialize response has no capabilities".to_owned()));
    };
    let error = match serde_json::from_value(capabilities.clone()) {
        Ok(parsed) => return (parsed, None),
        Err(e) => e,
    };
    let Value::Object(fields) = capabilities else {
        return (Default::default(), Some(format!("could not parse the server capabilities: {error}")));
    };
    let mut kept = Map::new();
    let mut dropped = Vec::new();
    for (key, value) in fields {
        let single = Value::Object(Map::from_iter([(key.clone(), value.clone())]));
        if serde_json::from_value::<lsp_types::ServerCapabilities>(single).is_ok() {
            kept.insert(key, value);
        } else {
            dropped.push(key);
        }
    }
    let parsed = serde_json::from_value(Value::Object(kept)).unwrap_or_default();
    (parsed, Some(format!("ignored server capabilities CodeCraft could not parse ({}): {error}", dropped.join(", "))))
}

fn response_error(error: &Value) -> ResponseError {
    ResponseError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(INTERNAL_ERROR),
        message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
    }
}

fn id_as_i64(id: &Value) -> Option<i64> {
    id.as_i64().or_else(|| id.as_str().and_then(|id| id.parse().ok()))
}

fn request_message(id: i64, method: &str, params: Value) -> Value {
    let mut message = Map::new();
    message.insert("jsonrpc".to_owned(), "2.0".into());
    message.insert("id".to_owned(), id.into());
    message.insert("method".to_owned(), method.into());
    // JSON-RPC allows omitting params but not `null` params (`shutdown`, `exit`).
    if !params.is_null() {
        message.insert("params".to_owned(), params);
    }
    Value::Object(message)
}

fn notification_message(method: &str, params: Value) -> Value {
    let mut message = Map::new();
    message.insert("jsonrpc".to_owned(), "2.0".into());
    message.insert("method".to_owned(), method.into());
    if !params.is_null() {
        message.insert("params".to_owned(), params);
    }
    Value::Object(message)
}

fn response_message(id: Value, result: std::result::Result<Value, ResponseError>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": error.code, "message": error.message },
        }),
    }
}

/// Kill a spawned server and everything in its process group.
fn kill_tree(child: &Mutex<Child>) {
    let mut child = lock(child);
    signal_group(child.id(), "-KILL");
    if let Err(e) = child.kill() {
        log::debug!("could not kill the language server: {e}");
    }
}

/// Send a signal to a process group with the `kill` command (no unsafe). Best effort.
fn signal_group(pgid: u32, signal: &str) {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .args([signal, "--", &format!("-{pgid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Err(e) = status {
            log::debug!("kill {signal} -{pgid}: {e}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_handles_huge_timeouts() {
        let deadline = Deadline::after(Duration::MAX);
        assert!(!deadline.expired());
        assert!(deadline.remaining() > Duration::from_secs(1));
        let deadline = Deadline::after(Duration::ZERO);
        assert!(deadline.expired());
        assert_eq!(deadline.remaining(), Duration::ZERO);
    }

    #[test]
    fn latch_times_out_and_wakes() {
        let latch = Arc::new(Latch::default());
        assert!(!latch.wait_timeout(Duration::from_millis(20)));
        let setter = Arc::clone(&latch);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            setter.set();
        });
        assert!(latch.wait_timeout(Duration::from_secs(10)));
        assert!(latch.is_set());
        handle.join().unwrap();
    }

    #[test]
    fn messages_omit_null_params() {
        assert_eq!(
            request_message(3, "shutdown", Value::Null),
            json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown"})
        );
        assert_eq!(notification_message("exit", Value::Null), json!({"jsonrpc": "2.0", "method": "exit"}));
        assert_eq!(
            notification_message("initialized", json!({})),
            json!({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        );
    }

    #[test]
    fn responses_keep_null_results() {
        assert_eq!(response_message(json!("a"), Ok(Value::Null)), json!({"jsonrpc": "2.0", "id": "a", "result": null}));
        let error = ResponseError { code: -1, message: "no".to_owned() };
        assert_eq!(
            response_message(json!(1), Err(error)),
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "no"}})
        );
    }

    #[test]
    fn ids_may_be_numeric_strings() {
        assert_eq!(id_as_i64(&json!(7)), Some(7));
        assert_eq!(id_as_i64(&json!("8")), Some(8));
        assert_eq!(id_as_i64(&json!("x")), None);
        assert_eq!(id_as_i64(&json!(1.5)), None);
    }

    #[test]
    fn response_errors_tolerate_missing_fields() {
        assert_eq!(response_error(&json!({})), ResponseError { code: INTERNAL_ERROR, message: String::new() });
        assert_eq!(
            response_error(&json!({"code": -32800, "message": "cancelled"})),
            ResponseError { code: -32800, message: "cancelled".to_owned() }
        );
    }

    #[test]
    fn unparseable_capabilities_are_dropped_individually() {
        let (caps, warning) = parse_capabilities(json!({
            "capabilities": { "hoverProvider": true, "definitionProvider": "not a bool or object" }
        }));
        assert_eq!(caps.hover_provider, Some(lsp_types::HoverProviderCapability::Simple(true)));
        assert!(caps.definition_provider.is_none());
        assert!(warning.unwrap().contains("definitionProvider"));
        let (caps, warning) = parse_capabilities(json!({"capabilities": {"hoverProvider": true}}));
        assert!(caps.hover_provider.is_some());
        assert!(warning.is_none());
        let (_, warning) = parse_capabilities(json!(null));
        assert!(warning.is_some());
    }
}
