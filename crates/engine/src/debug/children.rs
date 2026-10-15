//! Child debug sessions: a debugged Python program's subprocesses (debugpy `subProcess`). For
//! every new process debugpy sends a `startDebugging` reverse request whose configuration names
//! the adapter's own listening address; CodeCraft connects there (on a worker thread), runs
//! `initialize` → `attach` → breakpoints → `configurationDone` on that connection, and keeps it
//! next to the main one.
//!
//! The Debug tool window shows one process at a time: the one that stopped last. When a child
//! stops, its connection becomes the session's live connection (the parent's goes into the list
//! with its state); when the focused child ends, the parent comes back. Breakpoint and
//! exception-filter changes go to every connection.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_dap::{self as dap, AdapterKind, DapClient, DapEvent, RequestId};

use super::{DebugConsoleKind, DebugStatus, Phase, Runtime};
use crate::Session;

/// At most this many processes are debugged at once (a fork bomb stays a fork bomb, not an IDE
/// hang).
const MAX_CHILDREN: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENTS_PER_PUMP: usize = 2000;

/// One process of a multi-process session, for the UI and agents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEntry {
    pub label: String,
    pub pid: Option<u32>,
    /// The process the Debug tool window shows.
    pub focused: bool,
    /// Paused (as far as CodeCraft knows).
    pub stopped: bool,
}

#[derive(Clone, Debug)]
enum ChildPending {
    Initialize,
    Attach,
    SetBreakpoints { ids: Vec<Option<u64>> },
    Other,
}

struct Conn {
    label: String,
    pid: Option<u32>,
    /// The session's first connection (parked while a child is focused).
    main: bool,
    client: DapClient,
    events: Receiver<DapEvent>,
    pending: HashMap<RequestId, ChildPending>,
    configured: bool,
    attach_args: Option<Value>,
    adapter_ids: HashMap<i64, u64>,
    sent: BTreeSet<PathBuf>,
    /// The status it had when it was parked (`Stopped …` or `Running`).
    status: DebugStatus,
}

struct Connected {
    client: DapClient,
    events: Receiver<DapEvent>,
    configuration: Value,
}

/// The child connections of a session (owned by its `Runtime`).
#[derive(Default)]
pub(crate) struct Children {
    conns: Vec<Conn>,
    connecting: Vec<Receiver<Result<Connected, String>>>,
    /// The label and pid of the focused connection when it is a child.
    focused: Option<(String, Option<u32>)>,
}

impl Children {
    /// A child connection is the session's live one.
    pub(crate) fn child_focused(&self) -> bool {
        self.focused.is_some()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.conns.is_empty() && self.connecting.is_empty() && self.focused.is_none()
    }

    /// Run `f` on every configured child connection.
    pub(crate) fn each_configured(&self, mut f: impl FnMut(&DapClient)) {
        for conn in self.conns.iter().filter(|c| c.configured) {
            f(&conn.client);
        }
    }

    /// End every child: `disconnect` (terminating it), then drop the connection.
    pub(crate) fn end_all(&mut self, grace: Duration) {
        for conn in self.conns.drain(..) {
            conn.client.terminate(grace);
        }
        self.connecting.clear();
    }

    pub(crate) fn kill_all(&mut self) {
        for conn in self.conns.drain(..) {
            conn.client.kill();
        }
        self.connecting.clear();
    }

    /// Send one file's breakpoints to every configured child.
    pub(crate) fn send_breakpoints(&mut self, path: &Path, adapter_path: &Path, list: &[dap::SourceBreakpoint], ids: &[Option<u64>]) {
        for conn in self.conns.iter_mut().filter(|c| c.configured) {
            if list.is_empty() && !conn.sent.remove(path) {
                continue;
            }
            if !list.is_empty() {
                conn.sent.insert(path.to_path_buf());
            }
            if let Ok(id) = conn.client.set_breakpoints(adapter_path, list) {
                conn.pending.insert(id, ChildPending::SetBreakpoints { ids: ids.to_vec() });
            }
        }
    }

    /// The processes for the UI (empty when there is only the main one).
    pub(crate) fn entries(&self, focused_status: Option<&DebugStatus>, main_pid: Option<u32>) -> Vec<ProcessEntry> {
        if self.conns.is_empty() && self.focused.is_none() {
            return Vec::new();
        }
        let focused_stopped = matches!(focused_status, Some(DebugStatus::Stopped { .. }));
        let mut out = vec![match &self.focused {
            Some((label, pid)) => ProcessEntry { label: label.clone(), pid: *pid, focused: true, stopped: focused_stopped },
            None => ProcessEntry { label: "Main process".into(), pid: main_pid, focused: true, stopped: focused_stopped },
        }];
        for conn in &self.conns {
            out.push(ProcessEntry {
                label: if conn.main { "Main process".into() } else { conn.label.clone() },
                pid: if conn.main { main_pid } else { conn.pid },
                focused: false,
                stopped: matches!(conn.status, DebugStatus::Stopped { .. }),
            });
        }
        out
    }
}

/// A `startDebugging` reverse request (from the live connection or a child): answer it, then
/// connect to the address its configuration names on a worker thread.
pub(crate) fn start_debugging(s: &mut Session, from: Option<usize>, seq: i64, arguments: &Value) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let args: dap::StartDebuggingArguments = dap::types::parse(arguments).unwrap_or_default();
    let connect = args.configuration.get("connect");
    let host = connect.and_then(|c| c.get("host")).and_then(Value::as_str).unwrap_or("127.0.0.1").to_owned();
    let port = connect.and_then(|c| c.get("port")).and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok());
    let total = rt.children.conns.len() + rt.children.connecting.len();
    let client = match from {
        None => rt.client(),
        Some(i) => rt.children.conns.get(i).map(|c| &c.client),
    };
    let Some(client) = client else { return };
    let refusal = match (rt.kind, port) {
        (AdapterKind::Debugpy, Some(_)) if total >= MAX_CHILDREN => {
            Some(format!("CodeCraft debugs at most {MAX_CHILDREN} subprocesses at once"))
        }
        (AdapterKind::Debugpy, Some(_)) if args.request == "attach" || args.request.is_empty() => None,
        _ => Some("CodeCraft supports child sessions of debugpy only".to_owned()),
    };
    if let Some(reason) = refusal {
        let _ = client.respond(seq, "startDebugging", Err(reason.clone()));
        s.debug.info(format!("A subprocess is not debugged: {reason}."));
        return;
    }
    let _ = client.respond(seq, "startDebugging", Ok(json!({})));
    let Some(port) = port else { return };
    let configuration = args.configuration.clone();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("debug-child-connect".into()).spawn(move || {
        let result = DapClient::connect_tcp_host("debugpy (subprocess)", &host, port, CONNECT_TIMEOUT)
            .map(|(client, events)| Connected { client, events, configuration })
            .map_err(|e| format!("cannot connect to the subprocess's debug adapter at {host}:{port}: {e}"));
        let _ = tx.send(result);
    });
    match spawned {
        Ok(_) => rt.children.connecting.push(rx),
        Err(e) => s.debug.error(format!("cannot start a worker thread: {e}")),
    }
}

/// Drain the child connections; true if anything happened.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = pump_connecting(s);
    let count = s.debug.rt.as_ref().map_or(0, |rt| rt.children.conns.len());
    // Newest first so that removing one doesn't shift the ones still to drain.
    for i in (0..count).rev() {
        let batch: Vec<DapEvent> = match s.debug.rt.as_ref().and_then(|rt| rt.children.conns.get(i)) {
            Some(conn) => conn.events.try_iter().take(MAX_EVENTS_PER_PUMP).collect(),
            None => continue,
        };
        let mut rest = batch.into_iter();
        while let Some(event) = rest.next() {
            changed = true;
            let focused = matches!(&event, DapEvent::Event { event, .. } if event == "stopped");
            if !on_child_event(s, i, event) {
                if focused {
                    // The connection is the live one now: the rest of its batch goes there.
                    for event in rest.by_ref() {
                        super::on_dap_event(s, event);
                    }
                }
                break;
            }
        }
    }
    changed
}

fn pump_connecting(s: &mut Session) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    let mut done = Vec::new();
    rt.children.connecting.retain(|rx| match rx.try_recv() {
        Ok(result) => {
            done.push(result);
            false
        }
        Err(crossbeam_channel::TryRecvError::Empty) => true,
        Err(crossbeam_channel::TryRecvError::Disconnected) => false,
    });
    let changed = !done.is_empty();
    for result in done {
        match result {
            Ok(connected) => add_child(s, connected),
            Err(e) => s.debug.error(e),
        }
    }
    changed
}

fn add_child(s: &mut Session, connected: Connected) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let Connected { client, events, mut configuration } = connected;
    let pid = configuration.get("subProcessId").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok());
    let label = match pid {
        Some(pid) => format!("Subprocess {pid}"),
        None => "Subprocess".to_owned(),
    };
    if let Some(map) = configuration.as_object_mut() {
        map.insert("request".into(), "attach".into());
    }
    let mut conn = Conn {
        label: label.clone(),
        pid,
        main: false,
        client,
        events,
        pending: HashMap::new(),
        configured: false,
        attach_args: Some(configuration),
        adapter_ids: HashMap::new(),
        sent: BTreeSet::new(),
        status: DebugStatus::Running,
    };
    if let Ok(id) = conn.client.initialize("debugpy") {
        conn.pending.insert(id, ChildPending::Initialize);
    }
    rt.children.conns.push(conn);
    s.debug.info(format!("{label} started (debugging it too)"));
    refresh_entries(s);
}

/// Handle one event of child `i`; false if the child is gone (stop draining it).
fn on_child_event(s: &mut Session, i: usize, event: DapEvent) -> bool {
    match event {
        DapEvent::Response(response) => {
            on_child_response(s, i, response);
            true
        }
        DapEvent::Event { event, body } => match event.as_str() {
            "initialized" => {
                configure_child(s, i);
                true
            }
            "stopped" => {
                focus(s, i);
                super::on_stopped(s, dap::types::parse(&body).unwrap_or_default());
                false
            }
            "output" => {
                let ev: dap::OutputEvent = dap::types::parse(&body).unwrap_or_default();
                let kind = match ev.category.as_deref() {
                    Some("stdout") => DebugConsoleKind::Stdout,
                    Some("stderr") => DebugConsoleKind::Stderr,
                    Some("telemetry") => return true,
                    Some("important") => DebugConsoleKind::Error,
                    _ => DebugConsoleKind::Info,
                };
                s.debug.push_console(kind, ev.output);
                true
            }
            "breakpoint" => {
                let ev: dap::BreakpointEvent = dap::types::parse(&body).unwrap_or_default();
                let ours = s
                    .debug
                    .rt
                    .as_ref()
                    .and_then(|rt| rt.children.conns.get(i))
                    .and_then(|c| ev.breakpoint.id.and_then(|id| c.adapter_ids.get(&id)))
                    .copied();
                if let Some(bp) = ours.and_then(|ours| s.debug.breakpoints.iter_mut().find(|b| b.id == ours))
                    && ev.breakpoint.verified
                {
                    bp.verified = Some(true);
                }
                true
            }
            "exited" | "terminated" => {
                let main = s.debug.rt.as_ref().and_then(|rt| rt.children.conns.get(i)).is_some_and(|c| c.main);
                if main {
                    // The parent finished while a child is shown: the session ends with the child.
                    let code = dap::types::parse::<dap::ExitedEvent>(&body).ok().map(|e| e.exit_code);
                    if let Some(rt) = s.debug.rt.as_mut() {
                        rt.exited |= event == "exited";
                        if let Some(conn) = rt.children.conns.get_mut(i) {
                            conn.status = DebugStatus::Exited { code };
                        }
                    }
                    return true;
                }
                remove_child(s, i, "exited");
                false
            }
            _ => true,
        },
        DapEvent::Request { seq, command, arguments } => {
            if command == "startDebugging" {
                start_debugging(s, Some(i), seq, &arguments);
            } else if let Some(conn) = s.debug.rt.as_ref().and_then(|rt| rt.children.conns.get(i)) {
                let _ = conn.client.respond(seq, &command, Err(format!("CodeCraft does not support {command}")));
            }
            true
        }
        DapEvent::Error(e) => {
            log::debug!("subprocess debug connection: {e}");
            true
        }
        DapEvent::Exited { .. } => {
            let main = s.debug.rt.as_ref().and_then(|rt| rt.children.conns.get(i)).is_some_and(|c| c.main);
            if main {
                // The adapter itself is gone (every connection goes through it).
                super::on_adapter_exit(s);
            } else {
                remove_child(s, i, "ended");
            }
            false
        }
    }
}

fn on_child_response(s: &mut Session, i: usize, response: dap::Response) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let Some(conn) = rt.children.conns.get_mut(i) else { return };
    let Some(pending) = conn.pending.remove(&response.id) else { return };
    match pending {
        ChildPending::Initialize => {
            if response.result.is_err() {
                return remove_child(s, i, "failed to start");
            }
            let args = conn.attach_args.take().unwrap_or(Value::Null);
            if let Ok(id) = conn.client.attach(args) {
                conn.pending.insert(id, ChildPending::Attach);
            }
        }
        ChildPending::Attach => {
            if let Err(e) = response.result {
                s.debug.error(format!("could not debug the subprocess: {e}"));
                remove_child(s, i, "not debugged");
            }
        }
        ChildPending::SetBreakpoints { ids } => {
            let Ok(body) = response.result else { return };
            let body: dap::BreakpointsBody = dap::types::parse(&body).unwrap_or_default();
            for (theirs, ours) in body.breakpoints.iter().zip(ids) {
                let Some(ours) = ours else { continue };
                if let Some(id) = theirs.id {
                    conn.adapter_ids.insert(id, ours);
                }
                if theirs.verified
                    && let Some(bp) = s.debug.breakpoints.iter_mut().find(|b| b.id == ours)
                {
                    bp.verified = Some(true);
                }
            }
        }
        ChildPending::Other => {}
    }
}

/// One file's breakpoints for a child: (our path, the adapter's path, DAP breakpoints, our ids).
type FileBreakpoints = (PathBuf, PathBuf, Vec<dap::SourceBreakpoint>, Vec<Option<u64>>);

/// `initialized` from a child: its breakpoints, exception filters and `configurationDone`.
fn configure_child(s: &mut Session, i: usize) {
    let paths: BTreeSet<PathBuf> = s.debug.breakpoints.iter().filter(|b| b.enabled).map(|b| b.path.clone()).collect();
    let settings = s.debug.exceptions.clone();
    let lists: Vec<FileBreakpoints> = paths
        .into_iter()
        .map(|path| {
            let (list, ids) = super::breakpoint_list(&s.debug, &path, None);
            let adapter_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            (path, adapter_path, list, ids)
        })
        .collect();
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let capabilities = rt.capabilities.clone();
    let Some(conn) = rt.children.conns.get_mut(i) else { return };
    if conn.configured {
        return;
    }
    conn.configured = true;
    for (path, adapter_path, list, ids) in lists {
        if list.is_empty() {
            continue;
        }
        conn.sent.insert(path);
        if let Ok(id) = conn.client.set_breakpoints(&adapter_path, &list) {
            conn.pending.insert(id, ChildPending::SetBreakpoints { ids });
        }
    }
    super::exceptions::send_to(&conn.client, AdapterKind::Debugpy, &capabilities, &settings);
    if let Ok(id) = conn.client.configuration_done() {
        conn.pending.insert(id, ChildPending::Other);
    }
}

/// Make child `i` the session's live connection (it stopped).
fn focus(s: &mut Session, i: usize) {
    let status = s.debug.session.as_ref().map(super::DebugSession::status).unwrap_or(DebugStatus::Running);
    let Some(rt) = s.debug.rt.as_mut() else { return };
    if i >= rt.children.conns.len() {
        return;
    }
    let mut conn = rt.children.conns.remove(i);
    let Phase::Live { client, events } = std::mem::replace(&mut rt.phase, Phase::Live { client: conn.client, events: conn.events })
    else {
        return;
    };
    let parked = Conn {
        label: rt.children.focused.as_ref().map(|(l, _)| l.clone()).unwrap_or_else(|| "Main process".into()),
        pid: rt.children.focused.as_ref().and_then(|(_, p)| *p),
        main: rt.children.focused.is_none(),
        client,
        events,
        pending: HashMap::new(),
        configured: rt.configured,
        attach_args: None,
        adapter_ids: std::mem::replace(&mut rt.adapter_ids, std::mem::take(&mut conn.adapter_ids)),
        sent: std::mem::replace(&mut rt.sent, std::mem::take(&mut conn.sent)),
        status,
    };
    rt.configured = conn.configured;
    // Answers to the old live connection's requests arrive on its own channel now.
    rt.pending.clear();
    rt.epoch += 1;
    // Back to the main process, or on to a child.
    rt.children.focused = if conn.main { None } else { Some((conn.label.clone(), conn.pid)) };
    rt.children.conns.push(parked);
    s.debug.info(format!("{} stopped", conn.label));
    refresh_entries(s);
}

/// Child `i` is over: drop it.
fn remove_child(s: &mut Session, i: usize, how: &str) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    if i >= rt.children.conns.len() {
        return;
    }
    if rt.children.conns.get(i).is_some_and(|c| c.main) {
        return;
    }
    let conn = rt.children.conns.remove(i);
    conn.client.detach(Duration::from_millis(500));
    s.debug.info(format!("{} {how}", conn.label));
    refresh_entries(s);
}

/// The focused child ended (`exited`, `terminated` or its connection closed): show the main
/// process again. False if the focused connection is the main one (the session ends).
pub(crate) fn focused_ended(s: &mut Session) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    let Some((label, _)) = rt.children.focused.clone() else { return false };
    let Some(index) = rt.children.conns.iter().position(|c| c.main && !c.status.is_finished()) else { return false };
    let main = rt.children.conns.remove(index);
    let Phase::Live { client, .. } = std::mem::replace(&mut rt.phase, Phase::Live { client: main.client, events: main.events })
    else {
        return false;
    };
    client.detach(Duration::from_millis(500));
    rt.adapter_ids = main.adapter_ids;
    rt.sent = main.sent;
    rt.configured = main.configured;
    rt.pending.clear();
    rt.epoch += 1;
    rt.children.focused = None;
    let stopped = matches!(main.status, DebugStatus::Stopped { .. });
    if let Some(session) = s.debug.session.as_mut() {
        session.clear_stop();
        session.status = Some(main.status);
    }
    s.debug.info(format!("{label} exited; showing the main process"));
    if stopped {
        super::refresh_stack(s);
    } else {
        s.status = "Running".into();
    }
    refresh_entries(s);
    true
}

/// Copy the process list into the session (for the UI).
pub(crate) fn refresh_entries(s: &mut Session) {
    let Some(rt) = s.debug.rt.as_ref() else { return };
    let status = s.debug.session.as_ref().and_then(|x| x.status.clone());
    let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
    let entries = rt.children.entries(status.as_ref(), pid);
    if let Some(session) = s.debug.session.as_mut() {
        session.processes = entries;
    }
}

/// The label of the process the session shows (agents' stop summary).
pub(crate) fn focused_label(rt: &Runtime) -> Option<String> {
    rt.children.focused.as_ref().map(|(l, _)| l.clone())
}

