//! The connection: shared state, the writer/reader/stderr/reaper threads and message dispatch.
//!
//! Modelled on `codecraft-lsp`'s client. Every thread holds an `Arc<Inner>`; [`crate::DapClient`]
//! is the handle whose `Drop` kills a spawned adapter (and its process group, which is where the
//! debuggee usually lives). Threads are never joined. Events go through one gate so
//! [`DapEvent::Exited`] is sent exactly once and last.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde_json::{Map, Value, json};

use crate::transport::{self, FrameReader, Incoming};
use crate::{AdapterCommand, AdapterTransport, DapError, DapEvent, RequestId, Response, Result};

const STDERR_TAIL_LINES: usize = 50;
const MAX_STDERR_LINE_BYTES: u64 = 16 * 1024;
const MAX_STDERR_LINE_CHARS: usize = 1000;
/// After this many malformed messages in a row the adapter is considered broken.
const MAX_CONSECUTIVE_MALFORMED: u32 = 32;
const REAP_POLL: Duration = Duration::from_millis(30);
/// How long an adapter may live on after closing its stdout before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long the reaper lets the reader and stderr drain after the process exited.
const DRAIN_GRACE: Duration = Duration::from_secs(1);
/// How long a TCP adapter may take to start listening.
pub(crate) const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FOREVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
        let deadline = Instant::now().checked_add(timeout);
        let mut set = lock(&self.set);
        while !*set {
            let left = match deadline {
                Some(at) => at.saturating_duration_since(Instant::now()),
                None => FOREVER,
            };
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
    tx: Sender<DapEvent>,
    exited: bool,
}

#[derive(Default)]
struct State {
    next_seq: i64,
    /// In-flight request seq → command.
    pending: HashMap<i64, String>,
}

impl State {
    fn allocate_seq(&mut self) -> Result<i64> {
        let seq = self.next_seq.max(1);
        self.next_seq = seq.checked_add(1).ok_or_else(|| DapError::Protocol("sequence numbers exhausted".into()))?;
        Ok(seq)
    }
}

pub(crate) struct Inner {
    name: String,
    state: Mutex<State>,
    events: Mutex<EventSink>,
    writer: Sender<WriterMsg>,
    child: Option<Mutex<Child>>,
    pid: Option<u32>,
    stderr_tail: Mutex<VecDeque<String>>,
    /// A stop was requested (terminate, kill or drop): the exit is expected.
    stop_requested: AtomicBool,
    /// The reaper collected the adapter's exit status (its pid may be reused from here on).
    /// Only written while holding the child lock.
    reaped: AtomicBool,
    reader_done: Latch,
    process_exited: Latch,
    stderr_done: Latch,
}

struct Streams {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<Child>,
    /// The adapter's diagnostic output (stderr; also stdout for TCP adapters).
    logs: Vec<Box<dyn Read + Send>>,
}

pub(crate) fn connect(
    name: &str,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
) -> Result<(Arc<Inner>, Receiver<DapEvent>)> {
    start(name, Streams { reader, writer, child: None, logs: Vec::new() })
}

/// Start an adapter. For a TCP adapter this waits (up to [`TCP_CONNECT_TIMEOUT`]) for it to
/// listen, so call it from a worker thread.
pub(crate) fn spawn(adapter: &AdapterCommand) -> Result<(Arc<Inner>, Receiver<DapEvent>)> {
    let command_name = adapter.program.display().to_string();
    let port = match &adapter.transport {
        AdapterTransport::Stdio => None,
        AdapterTransport::Tcp { .. } => Some(free_port()?),
    };
    let args: Vec<String> = match (&adapter.transport, port) {
        (AdapterTransport::Tcp { port_arg }, Some(port)) => adapter
            .args
            .iter()
            .cloned()
            .chain(port_arg.iter().map(|a| a.replace("{port}", &port.to_string())))
            .collect(),
        _ => adapter.args.clone(),
    };
    let mut command = Command::new(&adapter.program);
    command.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if port.is_some() {
        command.stdin(Stdio::null());
    }
    for (key, value) in &adapter.env {
        if key.is_empty() || key.contains(['=', '\0']) {
            log::warn!("ignoring invalid environment variable name {key:?}");
            continue;
        }
        command.env(key, value);
    }
    if let Some(cwd) = adapter.cwd.as_deref().filter(|d| d.is_dir()) {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        // Its own process group: stopping kills the adapter and everything it started.
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|source| DapError::Spawn { command: command_name.clone(), source })?;
    log::info!("started debug adapter {command_name} (pid {})", child.id());
    let fail = |child: &mut Child, message: String| {
        crate::kill_process_group(child.id(), "KILL");
        let _ = child.kill();
        let _ = child.wait();
        DapError::Spawn { command: command_name.clone(), source: io::Error::other(message) }
    };
    let streams = match port {
        None => {
            let (Some(stdin), Some(stdout), Some(stderr)) =
                (child.stdin.take(), child.stdout.take(), child.stderr.take())
            else {
                return Err(fail(&mut child, "the adapter's stdio pipes are unavailable".into()));
            };
            Streams {
                reader: Box::new(stdout),
                writer: Box::new(stdin),
                child: Some(child),
                logs: vec![Box::new(stderr)],
            }
        }
        Some(port) => {
            let stream = match connect_tcp(port, &mut child) {
                Ok(stream) => stream,
                Err(e) => return Err(fail(&mut child, e)),
            };
            let writer = match stream.try_clone() {
                Ok(writer) => writer,
                Err(e) => return Err(fail(&mut child, e.to_string())),
            };
            let mut logs: Vec<Box<dyn Read + Send>> = Vec::new();
            if let Some(stdout) = child.stdout.take() {
                logs.push(Box::new(stdout));
            }
            if let Some(stderr) = child.stderr.take() {
                logs.push(Box::new(stderr));
            }
            Streams { reader: Box::new(stream), writer: Box::new(writer), child: Some(child), logs }
        }
    };
    start(&adapter.name, streams)
}

/// A port nobody listens on right now (the adapter binds it a moment later).
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn connect_tcp(port: u16, child: &mut Child) -> std::result::Result<std::net::TcpStream, String> {
    let deadline = Instant::now() + TCP_CONNECT_TIMEOUT;
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("the adapter exited before it listened on port {port} ({status})"));
                }
                if Instant::now() >= deadline {
                    return Err(format!("the adapter did not listen on port {port}: {e}"));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn start(name: &str, streams: Streams) -> Result<(Arc<Inner>, Receiver<DapEvent>)> {
    let Streams { reader, writer, child, logs } = streams;
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (writer_tx, writer_rx) = crossbeam_channel::unbounded();
    let pid = child.as_ref().map(Child::id);
    let inner = Arc::new(Inner {
        name: name.to_owned(),
        state: Mutex::new(State::default()),
        events: Mutex::new(EventSink { tx: event_tx, exited: false }),
        writer: writer_tx,
        child: child.map(Mutex::new),
        pid,
        stderr_tail: Mutex::new(VecDeque::new()),
        stop_requested: AtomicBool::new(false),
        reaped: AtomicBool::new(false),
        reader_done: Latch::default(),
        process_exited: Latch::default(),
        stderr_done: Latch::default(),
    });
    if let Err(e) = inner.start_threads(writer_rx, writer, reader, logs) {
        inner.kill();
        if let Some(child) = &inner.child {
            let _ = lock(child).wait();
        }
        return Err(DapError::Io(e));
    }
    Ok((inner, event_rx))
}

impl Inner {
    fn start_threads(
        self: &Arc<Self>,
        writer_rx: Receiver<WriterMsg>,
        writer: Box<dyn Write + Send>,
        reader: Box<dyn Read + Send>,
        logs: Vec<Box<dyn Read + Send>>,
    ) -> io::Result<()> {
        let me = Arc::clone(self);
        thread::Builder::new().name("dap-writer".to_owned()).spawn(move || me.writer_loop(&writer_rx, writer))?;
        let me = Arc::clone(self);
        thread::Builder::new().name("dap-reader".to_owned()).spawn(move || me.reader_loop(reader))?;
        if logs.is_empty() {
            self.stderr_done.set();
        }
        let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(logs.len()));
        for log in logs {
            let me = Arc::clone(self);
            let remaining = Arc::clone(&remaining);
            thread::Builder::new().name("dap-stderr".to_owned()).spawn(move || {
                me.stderr_loop(log);
                if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                    me.stderr_done.set();
                }
            })?;
        }
        if self.child.is_some() {
            let me = Arc::clone(self);
            thread::Builder::new().name("dap-reaper".to_owned()).spawn(move || me.reaper_loop())?;
        }
        Ok(())
    }

    // ----- public API backing -------------------------------------------------------------

    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        lock(&self.stderr_tail).iter().cloned().collect()
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.is_exited() && !self.reader_done.is_set()
    }

    pub(crate) fn request(&self, command: &str, arguments: Value) -> Result<RequestId> {
        self.ensure_open()?;
        let mut state = lock(&self.state);
        let seq = state.allocate_seq()?;
        let mut message = Map::new();
        message.insert("seq".into(), seq.into());
        message.insert("type".into(), "request".into());
        message.insert("command".into(), command.into());
        if !arguments.is_null() {
            message.insert("arguments".into(), arguments);
        }
        self.send_now(Value::Object(message))?;
        state.pending.insert(seq, command.to_owned());
        Ok(RequestId(seq))
    }

    /// Answers a reverse request (adapter → client).
    pub(crate) fn respond(
        &self,
        request_seq: i64,
        command: &str,
        result: std::result::Result<Value, String>,
    ) -> Result<()> {
        if self.is_exited() || self.reader_done.is_set() {
            return Err(DapError::NotRunning);
        }
        let seq = lock(&self.state).allocate_seq()?;
        self.send_now(response_message(seq, request_seq, command, result))
    }

    /// Ask the adapter to end the session (`disconnect`, terminating the debuggee), then make
    /// sure the adapter and its process group are gone within `grace`. Never blocks: the waiting
    /// happens on a helper thread. Only the first call does anything.
    pub(crate) fn terminate(self: &Arc<Self>, grace: Duration) {
        if self.stop_requested.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.is_exited() && !self.reader_done.is_set() {
            let seq = lock(&self.state).allocate_seq().unwrap_or(i64::MAX);
            let _ = self.writer.send(WriterMsg::Send(json!({
                "seq": seq,
                "type": "request",
                "command": "disconnect",
                "arguments": { "restart": false, "terminateDebuggee": true },
            })));
        }
        let me = Arc::clone(self);
        let spawned = thread::Builder::new().name("dap-terminate".to_owned()).spawn(move || {
            let gone = match &me.child {
                Some(_) => me.process_exited.wait_timeout(grace),
                None => me.reader_done.wait_timeout(grace),
            };
            if !gone {
                log::info!("debug adapter {} did not exit in time; killing it", me.name);
            }
            // Even an adapter that exited may leave its debuggee behind in the group.
            me.kill_now();
            if me.child.is_none() {
                me.emit_exited(None);
            }
        });
        if spawned.is_err() {
            self.kill_now();
        }
    }

    /// Kill the adapter's process group right away (no grace).
    pub(crate) fn kill(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        self.kill_now();
    }

    fn kill_now(&self) {
        let _ = self.writer.send(WriterMsg::Stop);
        let Some(child) = &self.child else { return };
        let mut child = lock(child);
        // Holding the lock keeps the reaper from collecting the leader, so its pid (the group id)
        // can't have been reused while we signal it.
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        // The adapter already had its chance to end the session (`disconnect`); SIGKILL the whole
        // group so the debuggee can't outlive it.
        if !crate::kill_process_group(child.id(), "KILL") {
            let _ = child.kill();
        }
    }

    // ----- sending -----------------------------------------------------------------------

    fn ensure_open(&self) -> Result<()> {
        if self.stop_requested.load(Ordering::SeqCst) || self.is_exited() || self.reader_done.is_set() {
            Err(DapError::NotRunning)
        } else {
            Ok(())
        }
    }

    fn send_now(&self, message: Value) -> Result<()> {
        self.writer.send(WriterMsg::Send(message)).map_err(|_| DapError::NotRunning)
    }

    // ----- events ------------------------------------------------------------------------

    fn emit(&self, event: DapEvent) {
        let sink = lock(&self.events);
        if !sink.exited {
            let _ = sink.tx.send(event);
        }
    }

    fn emit_exited(&self, code: Option<i32>) {
        let mut sink = lock(&self.events);
        if sink.exited {
            return;
        }
        sink.exited = true;
        let _ = sink.tx.send(DapEvent::Exited { code });
        drop(sink);
        let _ = self.writer.send(WriterMsg::Stop);
    }

    fn is_exited(&self) -> bool {
        lock(&self.events).exited
    }

    // ----- threads -----------------------------------------------------------------------

    fn writer_loop(self: Arc<Self>, rx: &Receiver<WriterMsg>, writer: Box<dyn Write + Send>) {
        let mut out = BufWriter::new(writer);
        while let Ok(WriterMsg::Send(message)) = rx.recv() {
            log::trace!("dap → {message}");
            if let Err(e) = transport::write_message(&mut out, &message) {
                if !self.stop_requested.load(Ordering::SeqCst) {
                    self.emit(DapEvent::Error(format!("failed to write to the debug adapter: {e}")));
                }
                return;
            }
        }
        // Stopped: dropping `out` closes the adapter's stdin.
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
                    self.emit(DapEvent::Error(format!("malformed message from the debug adapter: {problem}")));
                    malformed += 1;
                    if malformed >= MAX_CONSECUTIVE_MALFORMED {
                        self.emit(DapEvent::Error(
                            "too many malformed messages from the debug adapter; disconnecting".to_owned(),
                        ));
                        break;
                    }
                }
                Incoming::Eof => break,
                Incoming::Failed(e) => {
                    if !self.stop_requested.load(Ordering::SeqCst) {
                        self.emit(DapEvent::Error(format!("failed to read from the debug adapter: {e}")));
                    }
                    break;
                }
            }
        }
        self.reader_finished();
    }

    fn reader_finished(&self) {
        self.reader_done.set();
        if self.child.is_none() {
            self.emit_exited(None);
            return;
        }
        // The reaper reports the exit; make sure there is one to report.
        if !self.process_exited.wait_timeout(EXIT_GRACE) {
            log::warn!("debug adapter closed its output but is still running; killing it");
            self.kill_now();
            if !self.process_exited.wait_timeout(EXIT_GRACE) {
                self.emit_exited(None);
            }
        }
    }

    fn stderr_loop(&self, stream: Box<dyn Read + Send>) {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.by_ref().take(MAX_STDERR_LINE_BYTES).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => self.record_stderr(&line),
            }
        }
    }

    fn record_stderr(&self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        log::debug!(target: "codecraft_dap::stderr", "{text}");
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

    fn reaper_loop(self: Arc<Self>) {
        let Some(child) = &self.child else { return };
        let status = loop {
            let polled = {
                let mut guard = lock(child);
                let polled = guard.try_wait();
                if !matches!(polled, Ok(None)) {
                    self.reaped.store(true, Ordering::SeqCst);
                }
                polled
            };
            match polled {
                Ok(Some(status)) => break Some(status),
                Ok(None) => thread::sleep(REAP_POLL),
                Err(e) => {
                    log::warn!("could not wait for the debug adapter: {e}");
                    break None;
                }
            }
        };
        self.process_exited.set();
        // Deliver what the adapter wrote before it went away, and let stderr catch up.
        self.reader_done.wait_timeout(DRAIN_GRACE);
        self.stderr_done.wait_timeout(DRAIN_GRACE);
        let unexpected = !self.stop_requested.load(Ordering::SeqCst) && !status.is_some_and(|s| s.success());
        if unexpected {
            self.emit(DapEvent::Error(self.exit_report(status)));
        }
        self.emit_exited(status.and_then(|status| status.code()));
    }

    fn exit_report(&self, status: Option<ExitStatus>) -> String {
        let mut report = match status {
            Some(status) => format!("debug adapter {} exited unexpectedly ({status})", self.name),
            None => format!("debug adapter {} exited unexpectedly", self.name),
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
        log::trace!("dap ← {message}");
        let Value::Object(mut message) = message else {
            self.emit(DapEvent::Error(format!("unexpected message from the debug adapter: {message}")));
            return;
        };
        let kind = message.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        match kind.as_str() {
            "response" => self.on_response(&mut message),
            "event" => {
                let event = message.get("event").and_then(Value::as_str).unwrap_or_default().to_owned();
                let body = message.remove("body").unwrap_or(Value::Null);
                if event.is_empty() {
                    self.emit(DapEvent::Error("an event without a name from the debug adapter".to_owned()));
                } else {
                    self.emit(DapEvent::Event { event, body });
                }
            }
            "request" => {
                let seq = message.get("seq").and_then(Value::as_i64).unwrap_or(0);
                let command = message.get("command").and_then(Value::as_str).unwrap_or_default().to_owned();
                let arguments = message.remove("arguments").unwrap_or(Value::Null);
                self.on_reverse_request(seq, command, arguments);
            }
            other => self.emit(DapEvent::Error(format!("unknown message type {other:?} from the debug adapter"))),
        }
    }

    fn on_response(&self, message: &mut Map<String, Value>) {
        let request_seq = message.get("request_seq").and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
        let success = message.get("success").and_then(Value::as_bool).unwrap_or(false);
        let response_command = message.get("command").and_then(Value::as_str).unwrap_or_default().to_owned();
        let error_text = || {
            // `message` is a short machine-readable reason; `body.error.format` the human text.
            let detail = message
                .get("body")
                .and_then(|b| b.get("error"))
                .and_then(|e| e.get("format"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let short = message.get("message").and_then(Value::as_str).map(str::to_owned);
            detail.or(short).filter(|t| !t.is_empty()).unwrap_or_else(|| "request failed".to_owned())
        };
        let Some(request_seq) = request_seq else {
            if !success {
                self.emit(DapEvent::Error(format!("debug adapter error: {}", error_text())));
            }
            return;
        };
        let command = lock(&self.state).pending.remove(&request_seq);
        let Some(command) = command else {
            log::debug!("ignoring a response to unknown request {request_seq} ({response_command})");
            return;
        };
        let result = if success { Ok(message.remove("body").unwrap_or(Value::Null)) } else { Err(error_text()) };
        self.emit(DapEvent::Response(Response { id: RequestId(request_seq), command, result }));
    }

    fn on_reverse_request(&self, seq: i64, command: String, arguments: Value) {
        let reply = match command.as_str() {
            // No integrated terminal: refusing makes adapters fall back to their own console.
            "runInTerminal" => Some(Err("CodeCraft runs programs in the debug console, not a terminal".to_owned())),
            "startDebugging" => None,
            _ => Some(Err(format!("CodeCraft does not support the {command} request"))),
        };
        match reply {
            Some(result) => {
                if self.respond(seq, &command, result).is_err() {
                    log::debug!("could not answer {command}: the writer is gone");
                }
            }
            None => self.emit(DapEvent::Request { seq, command, arguments }),
        }
    }
}

fn response_message(seq: i64, request_seq: i64, command: &str, result: std::result::Result<Value, String>) -> Value {
    match result {
        Ok(body) => json!({
            "seq": seq, "type": "response", "request_seq": request_seq, "command": command, "success": true,
            "body": body,
        }),
        Err(message) => json!({
            "seq": seq, "type": "response", "request_seq": request_seq, "command": command, "success": false,
            "message": message,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(latch.wait_timeout(Duration::MAX));
    }

    #[test]
    fn responses_carry_body_or_message() {
        assert_eq!(
            response_message(5, 2, "startDebugging", Ok(json!({}))),
            json!({"seq": 5, "type": "response", "request_seq": 2, "command": "startDebugging", "success": true, "body": {}})
        );
        assert_eq!(
            response_message(6, 3, "runInTerminal", Err("no".into())),
            json!({"seq": 6, "type": "response", "request_seq": 3, "command": "runInTerminal", "success": false, "message": "no"})
        );
    }
}
