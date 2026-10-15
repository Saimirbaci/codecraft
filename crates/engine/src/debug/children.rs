//! Child debug sessions: a debugged Python program's subprocesses (debugpy `subProcess`). For
//! every new process debugpy sends a `startDebugging` reverse request whose configuration names
//! the adapter's own listening address; CodeCraft connects there (on a worker thread) and the
//! connection becomes a debug session of its own (M15 track DM), with the parent's id as its
//! `parent`: `initialize` → `attach` → breakpoints → `configurationDone`, its own threads,
//! frames, variables and console, a tab in the Debug tool window, and the `session` param of
//! every debug command. Stopping the parent stops its subprocess sessions; when a selected
//! subprocess ends, its parent is selected again.

use std::time::Duration;

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_dap::{self as dap, AdapterKind, DapClient, DapEvent};

use super::{DebugSession, DebugStatus, MAX_SESSIONS, Parked, Pending, Phase, Runtime};
use crate::Session;

/// At most this many subprocesses are debugged at once (a fork bomb stays a fork bomb, not an
/// IDE hang).
const MAX_CHILDREN: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// One process of a multi-process session family (a parent and its subprocess sessions), for
/// the UI and agents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEntry {
    pub label: String,
    pub pid: Option<u32>,
    /// The session this list belongs to.
    pub focused: bool,
    /// Paused (as far as CodeCraft knows).
    pub stopped: bool,
}

struct Connected {
    client: DapClient,
    events: Receiver<DapEvent>,
    configuration: Value,
}

/// The subprocess connections a session is making (owned by its `Runtime`).
#[derive(Default)]
pub(crate) struct Children {
    connecting: Vec<Receiver<Result<Connected, String>>>,
}

impl Children {
    pub(crate) fn is_empty(&self) -> bool {
        self.connecting.is_empty()
    }

    /// Give up the connections still being made.
    pub(crate) fn end_all(&mut self, _grace: Duration) {
        self.connecting.clear();
    }

    pub(crate) fn kill_all(&mut self) {
        self.connecting.clear();
    }
}

/// A `startDebugging` reverse request from the selected session's adapter: answer it, then
/// connect to the address its configuration names on a worker thread.
pub(crate) fn start_debugging(s: &mut Session, seq: i64, arguments: &Value) {
    let children = s.debug.all().filter(|(x, rt)| x.parent.is_some() && rt.is_some()).count();
    let total = s.debug.live_count();
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let args: dap::StartDebuggingArguments = dap::types::parse(arguments).unwrap_or_default();
    let connect = args.configuration.get("connect");
    let host = connect.and_then(|c| c.get("host")).and_then(Value::as_str).unwrap_or("127.0.0.1").to_owned();
    let port = connect.and_then(|c| c.get("port")).and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok());
    let pending = rt.children.connecting.len();
    let Some(client) = rt.client() else { return };
    let refusal = match (rt.kind, port) {
        (AdapterKind::Debugpy, Some(_)) if children + pending >= MAX_CHILDREN => {
            Some(format!("CodeCraft debugs at most {MAX_CHILDREN} subprocesses at once"))
        }
        (AdapterKind::Debugpy, Some(_)) if total + pending >= MAX_SESSIONS => {
            Some(format!("CodeCraft debugs at most {MAX_SESSIONS} programs at once"))
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

/// The selected session's subprocess connections that are made: each becomes a session. True
/// if anything happened.
pub(crate) fn pump(s: &mut Session) -> bool {
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

/// A subprocess connection is made: a new (parked) session talks to it, starting with
/// `initialize`; its `attach` uses the configuration debugpy gave.
fn add_child(s: &mut Session, connected: Connected) {
    let id = s.debug.next_session_id();
    let Some(parent_id) = s.debug.selected_id() else { return };
    let views = s.debug.views.fresh();
    let Some(rt) = s.debug.rt.as_ref() else { return };
    let Connected { client, events, mut configuration } = connected;
    let pid = configuration.get("subProcessId").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok());
    let label = match pid {
        Some(pid) => format!("Subprocess {pid}"),
        None => "Subprocess".to_owned(),
    };
    if let Some(map) = configuration.as_object_mut() {
        map.insert("request".into(), "attach".into());
    }
    let mut plan = rt.plan.clone();
    plan.name = label.clone();
    plan.build = None;
    let mut child = Runtime::new(plan, Phase::Live { client, events }, rt.project_root.clone());
    child.kind = AdapterKind::Debugpy;
    child.capabilities = rt.capabilities.clone();
    child.options = rt.options.clone();
    child.child_of = Some(parent_id);
    child.launch_args = Some(configuration);
    child.request(Pending::Initialize, |c| c.initialize("debugpy"));
    let last_target = s.debug.last_target.clone();
    let mut parked = Parked::new(
        DebugSession {
            id,
            parent: Some(parent_id),
            name: label.clone(),
            adapter: "debugpy (subprocess)".into(),
            status: Some(DebugStatus::Starting),
            debuggee_pid: pid,
            ..DebugSession::default()
        },
        Some(child),
        views,
    );
    parked.last_target = last_target;
    parked.watches = s.debug.watches.iter().cloned().map(|mut w| {
        w.clear();
        w
    }).collect();
    s.debug.parked.push(parked);
    s.debug.info(format!("{label} started (debugging it too)"));
}
