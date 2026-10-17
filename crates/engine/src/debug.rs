//! Debugger sessions (DAP): breakpoints, threads, frames, variables, watches and the debug
//! console. OWNED BY the debugger track.
//!
//! A session goes through three phases, none of which blocks the UI thread:
//! 1. **Build**: cargo / zig / a user command runs as a child process; its output goes to the
//!    debug console and the executable is read from its artifacts.
//! 2. **Prepare**: a worker thread finds the adapter (lldb-dap, debugpy, …), works out launch
//!    arguments (Rust pretty printers need `rustc --print sysroot`) and spawns it.
//! 3. **Live**: DAP events are drained in [`pump`]: `initialize` → `launch` → (on `initialized`)
//!    breakpoints + `configurationDone` → `stopped` / `continued` / `output` / `exited` …
//!
//! Everything the UI, the CLI or an agent reads is plain data on [`DebugState`]; the process
//! handles stay private. Breakpoints persist per project in `<project>/.codecraft/breakpoints.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation, CargoMessage, RunEvent, RunHandle, TargetSelector};
use codecraft_dap::{self as dap, AdapterKind, DapClient, DapEvent, RequestId};
use codecraft_syntax::{Language, SymbolKind};
use codecraft_text::SelectionSet;
use codecraft_workspace::{NotificationLevel, ToolWindowId};

use crate::{CommandError, Session};
pub use codecraft_dap::processes::{ProcessInfo, ProcessKind};

pub(crate) mod actions;
pub(crate) mod agent;
mod children;
mod cpp_values;
pub mod exceptions;
// M16 track RT: sessions on SSH hosts and in Docker containers.
pub(crate) mod on_target;
mod python_values;
mod rust_values;
#[cfg(test)]
mod sessions_tests;

pub use actions::{CompletionEntry, Completions, ExceptionStop, StepTarget, StepTargets, call_candidates};
pub use children::ProcessEntry;
pub use exceptions::ExceptionBreakpoints;
pub(crate) use python_values::is_grouping_node;

/// The debug console keeps at most this many lines.
pub const MAX_CONSOLE_LINES: usize = 20_000;
/// How long an adapter has to end the session after `disconnect` before its group is killed.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);
/// Events handled per frame per channel (the rest wait for the next frame).
const MAX_EVENTS_PER_PUMP: usize = 4000;
const MAX_FRAMES: i64 = 200;
/// Variables shown per node (adapters can return huge arrays).
const MAX_CHILDREN: usize = 2000;
const MAX_INLINE_CHARS: usize = 60;
/// How far (in lines) a breakpoint follows its line when text is inserted or deleted above it.
const MAX_LINE_DRIFT: usize = 2000;
const BREAKPOINTS_FILE: &str = "breakpoints.json";

// ---------------------------------------------------------------- data model

/// A line breakpoint. Lines are zero-based (editor lines); DAP gets them one-based.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: u64,
    pub path: PathBuf,
    pub line: usize,
    pub enabled: bool,
    pub condition: Option<String>,
    /// A logpoint: print this instead of stopping (`{expr}` interpolates).
    pub log_message: Option<String>,
    /// E.g. `>= 3` (adapter-specific syntax).
    pub hit_condition: Option<String>,
    /// Removed after its first hit.
    pub temporary: bool,
    /// What the running adapter said; `None` outside a session.
    pub verified: Option<bool>,
    /// The adapter's explanation when it could not verify the breakpoint.
    pub message: Option<String>,
}

/// A watch expression and its value at the current stop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Watch {
    pub expression: String,
    pub value: Option<String>,
    pub type_name: Option<String>,
    /// Non-zero when the value has children.
    pub reference: i64,
    pub error: Option<String>,
}

impl Watch {
    fn new(expression: String) -> Self {
        Self { expression, value: None, type_name: None, reference: 0, error: None }
    }

    fn clear(&mut self) {
        self.value = None;
        self.type_name = None;
        self.reference = 0;
        self.error = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugConsoleKind {
    Stdout,
    Stderr,
    /// Messages from CodeCraft or the adapter (build output, "process exited", …).
    Info,
    /// An expression typed into the console.
    Input,
    Result,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleLine {
    pub kind: DebugConsoleKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum DebugStatus {
    /// Building the program before debugging it.
    Building,
    /// Starting the debug adapter and the program.
    Starting,
    Running,
    #[serde(rename_all = "camelCase")]
    Stopped {
        reason: String,
        description: Option<String>,
        thread: Option<i64>,
    },
    Exited {
        code: Option<i64>,
    },
    Failed {
        message: String,
    },
}

impl DebugStatus {
    /// The session is over (exited or failed).
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Failed { .. })
    }

    pub fn label(&self) -> String {
        match self {
            Self::Building => "Building…".into(),
            Self::Starting => "Starting…".into(),
            Self::Running => "Running".into(),
            Self::Stopped { reason, description, .. } => match description {
                Some(d) if !d.is_empty() => format!("Paused: {d}"),
                _ => format!("Paused ({reason})"),
            },
            Self::Exited { code: Some(code) } => format!("Exited with code {code}"),
            Self::Exited { code: None } => "Stopped".into(),
            Self::Failed { message } => format!("Failed: {message}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub id: i64,
    pub name: String,
    /// The source file, when there is one on disk.
    pub path: Option<PathBuf>,
    /// Zero-based.
    pub line: usize,
    /// Zero-based.
    pub column: usize,
    /// Library or generated code (shown dimmed).
    pub subtle: bool,
    /// The frame's program counter as the adapter gives it (`0x…`), for the disassembly.
    #[serde(default)]
    pub instruction_pointer: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub name: String,
    pub reference: i64,
    pub expensive: bool,
    /// `arguments`, `locals`, `registers`, … (DAP's hint).
    #[serde(default)]
    pub presentation_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariableInfo {
    pub name: String,
    pub value: String,
    pub type_name: Option<String>,
    /// Non-zero when the variable has children.
    pub reference: i64,
    pub evaluate_name: Option<String>,
    /// Where the value lives (`0x…`), when the adapter says.
    #[serde(default)]
    pub memory_reference: Option<String>,
}

/// A value painted after the end of a source line while suspended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineValue {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

/// The result of Evaluate Expression (Alt+F8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub expression: String,
    /// The value, or the error text.
    pub result: Result<VariableInfo, String>,
    /// Still waiting for the adapter.
    pub pending: bool,
}

/// A form in the Debug tool window's chooser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DebugForm {
    /// Debug Core Dump…: a core file and the program that crashed.
    CoreDump,
    /// Remote Debug…: a gdbserver/lldb-server address and a local copy of the program.
    Remote,
    /// Attach to Python Debug Server…: a `debugpy --listen` address and path mappings.
    PythonRemote,
}

/// The tabs of the Debug tool window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugTab {
    #[default]
    Variables,
    Console,
    Breakpoints,
    /// Hex and ASCII view of the debuggee's memory.
    Memory,
    /// Instructions around the program counter; stepping steps by instruction while shown.
    Disassembly,
    Registers,
}

/// One debug session as the UI sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugSession {
    /// The session's id: `debug.sessions` lists them and every debug command takes it as its
    /// `session` param (M15 track DM). 0 for a session made outside the engine (tests).
    #[serde(default)]
    pub id: u64,
    /// The session that started this one (a Python subprocess's parent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    /// When it last stopped, on a counter shared by every session (0: never).
    #[serde(default)]
    pub stop_order: u64,
    pub name: String,
    /// `lldb-dap`, `debugpy`, … (empty until the adapter starts).
    pub adapter: String,
    pub status: Option<DebugStatus>,
    pub threads: Vec<ThreadInfo>,
    /// The selected thread.
    pub thread: Option<i64>,
    pub frames: Vec<FrameInfo>,
    /// Index of the selected frame.
    pub frame: usize,
    pub scopes: Vec<ScopeInfo>,
    /// Children by variables reference (scopes, structured values, watches).
    pub variables: HashMap<i64, Vec<VariableInfo>>,
    /// References being fetched.
    pub loading: BTreeSet<i64>,
    /// The current execution point (selected frame), for the editor.
    pub execution: Option<(PathBuf, usize)>,
    pub inline_values: Vec<InlineValue>,
    pub debuggee_pid: Option<u32>,
    /// The value the function just stepped out of returned (`(return) name`), when known.
    #[serde(default)]
    pub return_value: Option<VariableInfo>,
    /// What an exception stop threw.
    #[serde(default)]
    pub exception: Option<ExceptionStop>,
    /// The breakpoints (our ids) the stop hit.
    #[serde(default)]
    pub hit_breakpoints: Vec<u64>,
    /// The processes of a multi-process session (Python subprocesses); empty with one process.
    #[serde(default)]
    pub processes: Vec<ProcessEntry>,
}

impl DebugSession {
    pub fn status(&self) -> DebugStatus {
        self.status.clone().unwrap_or(DebugStatus::Starting)
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.status, Some(DebugStatus::Stopped { .. }))
    }

    pub fn selected_frame(&self) -> Option<&FrameInfo> {
        self.frames.get(self.frame)
    }

    fn clear_stop(&mut self) {
        self.frames.clear();
        self.frame = 0;
        self.scopes.clear();
        self.variables.clear();
        self.loading.clear();
        self.execution = None;
        self.inline_values.clear();
        self.return_value = None;
        self.exception = None;
        self.hit_breakpoints.clear();
    }
}

/// What to debug. Serialized as `{"kind": "cargo" | "executable" | "python" | "zig" | "zigFile" |
/// "node", …}` (the `debug.launch` command takes it as its `target` param).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DebugTarget {
    /// A `cargo run` or `cargo test` invocation (a run configuration): build it, then debug the
    /// binary (a test binary runs the filtered test with `--exact --nocapture`).
    #[serde(rename_all = "camelCase")]
    Cargo { name: String, invocation: CargoInvocation },
    /// Any native program (C, C++, …), optionally built first by a command (`["cmake", "--build",
    /// "build"]`).
    #[serde(rename_all = "camelCase")]
    Executable {
        name: String,
        program: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        build: Vec<String>,
    },
    /// A Python script or module under debugpy.
    #[serde(rename_all = "camelCase")]
    Python {
        name: String,
        #[serde(default)]
        program: Option<PathBuf>,
        #[serde(default)]
        module: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        env: Vec<(String, String)>,
        /// The interpreter (a virtualenv's); `python3` from `PATH` if unset.
        #[serde(default)]
        python: Option<PathBuf>,
        #[serde(default = "yes")]
        just_my_code: bool,
    },
    /// `zig build` in `root`, then `zig-out/bin/<exe>` (the newest executable when unset).
    #[serde(rename_all = "camelCase")]
    Zig {
        name: String,
        root: PathBuf,
        #[serde(default)]
        exe: Option<String>,
        #[serde(default)]
        args: Vec<String>,
    },
    /// A single Zig file built with `zig build-exe`.
    #[serde(rename_all = "camelCase")]
    ZigFile {
        name: String,
        path: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    /// A Node.js program under js-debug.
    #[serde(rename_all = "camelCase")]
    Node {
        name: String,
        program: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// A running process (Run › Attach to Process…): lldb-dap for native and Rust programs,
    /// debugpy for Python. Stopping detaches and leaves the process running.
    #[serde(rename_all = "camelCase")]
    Attach {
        name: String,
        pid: u32,
        #[serde(default)]
        process: ProcessKind,
    },
    /// Post-mortem debugging of a core dump (lldb-dap or CodeLLDB).
    #[serde(rename_all = "camelCase")]
    CoreDump {
        name: String,
        core: PathBuf,
        /// The executable that crashed (for symbols); lldb finds it from the core when unset.
        #[serde(default)]
        program: Option<PathBuf>,
    },
    /// A program under `gdbserver`, `lldb-server gdbserver` or `debugserver` on another machine
    /// (or locally), connected with `gdb-remote host:port`. Stopping detaches.
    #[serde(rename_all = "camelCase")]
    Remote {
        name: String,
        /// `host:port` of a gdbserver / `lldb-server gdbserver` / debugserver (`gdb-remote`), or
        /// `connect://host:port` of an `lldb-server platform`, which launches `program` there.
        address: String,
        /// A local copy of the remote executable, for symbols (required with a platform).
        #[serde(default)]
        program: Option<PathBuf>,
        /// The LLDB platform for `connect://` addresses (`remote-linux` by default).
        #[serde(default)]
        platform: Option<String>,
        /// Program arguments (platform mode).
        #[serde(default)]
        args: Vec<String>,
    },
    /// A Python program started with `python -m debugpy --listen host:port …` (or that called
    /// `debugpy.listen()`), here or on another machine. Stopping detaches.
    #[serde(rename_all = "camelCase")]
    PythonRemote {
        name: String,
        host: String,
        port: u16,
        /// Local directories and where the same files are on the remote machine.
        #[serde(default)]
        path_mappings: Vec<dap::launch::PathMapping>,
        #[serde(default = "yes")]
        just_my_code: bool,
    },
    /// Run › Attach to an Unstarted Process…: wait for a process with this name (or this
    /// executable's name) to start, then attach (lldb `process attach --name X --waitfor`).
    /// Stopping detaches.
    #[serde(rename_all = "camelCase")]
    AttachWait { name: String, process: String },
    /// `debug` run on an SSH host or in a Docker image/container/Compose service (M16 track RT):
    /// built there when it builds, its debug server (debugpy, lldb-server/gdbserver) started
    /// there and its port forwarded; the local adapter attaches with the paths mapped.
    #[serde(rename_all = "camelCase")]
    OnTarget { name: String, target: crate::run_targets::RunTarget, debug: Box<DebugTarget> },
}

fn yes() -> bool {
    true
}

impl DebugTarget {
    pub fn name(&self) -> &str {
        match self {
            Self::Cargo { name, .. }
            | Self::Executable { name, .. }
            | Self::Python { name, .. }
            | Self::Zig { name, .. }
            | Self::ZigFile { name, .. }
            | Self::Node { name, .. }
            | Self::Attach { name, .. }
            | Self::CoreDump { name, .. }
            | Self::Remote { name, .. }
            | Self::PythonRemote { name, .. }
            | Self::AttachWait { name, .. }
            | Self::OnTarget { name, .. } => name,
        }
    }
}

/// The Attach to Process list: the machine's processes (the user's own first), filtered by a
/// query on the name, command line or pid.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPicker {
    pub processes: Vec<ProcessInfo>,
    pub query: String,
    /// Index into [`AttachPicker::visible`].
    pub selected: usize,
    /// `ps` is still running.
    pub loading: bool,
    pub error: Option<String>,
    /// Show other users' processes too.
    pub all_users: bool,
    pub user: Option<String>,
}

impl AttachPicker {
    /// Indices of the processes that match the query (and the user filter).
    pub fn visible(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        self.processes
            .iter()
            .enumerate()
            .filter(|(_, p)| self.all_users || self.user.as_ref().is_none_or(|u| &p.user == u))
            .filter(|(_, p)| {
                q.is_empty()
                    || p.name.to_lowercase().contains(&q)
                    || p.command.to_lowercase().contains(&q)
                    || p.pid.to_string().starts_with(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_process(&self) -> Option<&ProcessInfo> {
        self.visible().get(self.selected).and_then(|i| self.processes.get(*i))
    }
}

/// Open the Attach to Process list and list the processes on a worker thread.
pub(crate) fn open_attach_picker(s: &mut Session) {
    let previous = s.debug.attach.take().unwrap_or_default();
    s.debug.attach = Some(AttachPicker {
        loading: true,
        query: previous.query,
        all_users: previous.all_users,
        user: dap::processes::current_user(),
        ..AttachPicker::default()
    });
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("debug-ps".into()).spawn(move || {
        let _ = tx.send(dap::processes::list_processes());
    });
    match spawned {
        Ok(_) => s.debug.attach_rx = Some(rx),
        Err(e) => {
            if let Some(p) = &mut s.debug.attach {
                p.loading = false;
                p.error = Some(format!("cannot start a worker thread: {e}"));
            }
        }
    }
}

fn pump_attach(s: &mut Session) -> bool {
    let Some(rx) = &s.debug.attach_rx else { return false };
    let result = match rx.try_recv() {
        Ok(r) => r,
        Err(crossbeam_channel::TryRecvError::Empty) => return false,
        Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the process list failed".into()),
    };
    s.debug.attach_rx = None;
    let me = std::process::id();
    if let Some(p) = &mut s.debug.attach {
        p.loading = false;
        match result {
            Ok(mut list) => {
                list.retain(|x| x.pid != me);
                // Newest (highest pid) first, like the most recently started programs.
                list.sort_by_key(|a| std::cmp::Reverse(a.pid));
                p.processes = list;
                p.selected = 0;
                p.error = None;
            }
            Err(e) => p.error = Some(e),
        }
    }
    true
}

/// One debug session in the Debug tool window's session tabs (`debug.sessions`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: u64,
    pub name: String,
    /// `lldb-dap`, `debugpy`, … (empty until the adapter starts).
    pub adapter: String,
    pub status: DebugStatus,
    /// The session the Debug tool window, the editor and commands without `session` act on.
    pub selected: bool,
    /// Building, starting, running or paused (not over).
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// The state of a session that is not the selected one. The selected session's state lives in
/// [`DebugState`]'s own fields (so every command, the Debug tool window and the editor act on
/// it); the others wait here, are pumped in turn, and swap places with it when selected.
pub(crate) struct Parked {
    session: Option<DebugSession>,
    rt: Option<Runtime>,
    console: Vec<ConsoleLine>,
    evaluation: Option<Evaluation>,
    hover: Option<Evaluation>,
    completions: Option<Completions>,
    step_targets: Option<StepTargets>,
    last_error: Option<String>,
    views: crate::debug_views::DebugViews,
    last_target: Option<DebugTarget>,
    last_options: crate::run_config_store::DebuggerOptions,
    /// This session's watch values (the expressions are shared by every session).
    watches: Vec<Watch>,
}

impl Parked {
    fn new(session: DebugSession, rt: Option<Runtime>, views: crate::debug_views::DebugViews) -> Self {
        Self {
            session: Some(session),
            rt,
            console: Vec::new(),
            evaluation: None,
            hover: None,
            completions: None,
            step_targets: None,
            last_error: None,
            views,
            last_target: None,
            last_options: Default::default(),
            watches: Vec::new(),
        }
    }

    fn id(&self) -> u64 {
        self.session.as_ref().map_or(0, |x| x.id)
    }

    fn live(&self) -> bool {
        self.rt.is_some()
    }
}

impl Drop for Parked {
    /// A parked session dropped (closed, or the IDE closing) leaves no processes behind.
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            let pid = self.session.as_ref().and_then(|x| x.debuggee_pid);
            let exited = rt.exited;
            rt.kill(pid, exited);
        }
    }
}

/// At most this many debug sessions run at once (subprocesses count as well).
pub const MAX_SESSIONS: usize = 48;

/// Give `watches` the values a session had for the same expressions (none for new ones).
fn restore_watches(watches: &mut [Watch], values: &[Watch]) {
    for w in watches {
        match values.iter().find(|v| v.expression == w.expression) {
            Some(v) => {
                w.value = v.value.clone();
                w.type_name = v.type_name.clone();
                w.reference = v.reference;
                w.error = v.error.clone();
            }
            None => w.clear(),
        }
    }
}

/// [`DebugState::settling`] for any session.
fn settling_of(session: Option<&DebugSession>, rt: Option<&Runtime>) -> bool {
    let Some(rt) = rt else { return false };
    match &rt.phase {
        Phase::Build { .. } | Phase::Prepare { .. } => true,
        Phase::Live { .. } => {
            let stopped = session.is_some_and(DebugSession::is_stopped);
            !stopped || rt.pending.values().any(Pending::blocks_settling)
        }
    }
}

/// The debugger's state on the [`Session`].
pub struct DebugState {
    pub breakpoints: Vec<Breakpoint>,
    /// Watch expressions (shared by every session) with the selected session's values.
    pub watches: Vec<Watch>,
    /// The selected session's debug console.
    pub console: Vec<ConsoleLine>,
    /// The selected (or last) session; the other sessions are in `debug.sessions`.
    pub session: Option<DebugSession>,
    pub tab: DebugTab,
    /// Expanded nodes in the Variables tree (kept across steps and sessions).
    pub expanded: BTreeSet<String>,
    /// Evaluate Expression's last result.
    pub evaluation: Option<Evaluation>,
    /// Asks the UI to focus the Evaluate field, prefilled with this text (Alt+F8).
    pub focus_evaluate: Option<String>,
    /// The Debug tool window shows a configuration chooser (Run › Debug…).
    pub choosing: bool,
    /// The chooser shows the Core Dump or Remote Debug form.
    pub form: Option<DebugForm>,
    /// Stop when a Rust program panics (a `rust_panic` function breakpoint), like RustRover.
    pub break_on_panic: bool,
    /// The last target debugged (Rerun).
    pub last_target: Option<DebugTarget>,
    /// The run configuration's debugger options of the last session (M13 track RC).
    pub last_options: crate::run_config_store::DebuggerOptions,
    /// Run › Attach to Process…'s process list while it is open.
    pub attach: Option<AttachPicker>,
    /// Memory, Disassembly, Registers, watchpoints and the console history.
    pub views: crate::debug_views::DebugViews,
    /// Code › Show Assembly.
    pub asm: crate::asm::AsmState,
    /// C++ and Python exception breakpoints (Rust panics: `break_on_panic`).
    pub exceptions: ExceptionBreakpoints,
    /// The breakpoint whose properties popup is open (the editor gutter's right-click).
    pub breakpoint_popup: Option<u64>,
    /// The value of the expression under the mouse while paused.
    pub hover: Option<Evaluation>,
    /// The debug console's completion list.
    pub completions: Option<Completions>,
    /// Smart Step Into's chooser.
    pub step_targets: Option<StepTargets>,
    /// The last failed debugger action (for agents' stop summary).
    pub last_error: Option<String>,
    pub(crate) agent: agent::AgentState,
    hover_epoch: u64,
    attach_rx: Option<Receiver<Result<Vec<ProcessInfo>, String>>>,
    next_id: u64,
    /// The project whose breakpoints are loaded (`Some(None)`: none open, loaded nothing).
    loaded_for: Option<Option<PathBuf>>,
    /// Each breakpoint's line text, to follow it when lines are inserted or deleted above.
    anchors: HashMap<u64, String>,
    /// Document revision per breakpoint file at the last line check.
    revisions: HashMap<PathBuf, u64>,
    rt: Option<Runtime>,
    /// The sessions that are not selected (M15 track DM).
    parked: Vec<Parked>,
    next_session: u64,
    /// The shared stop counter ([`DebugSession::stop_order`]).
    stop_counter: u64,
    /// Sessions closed while still running: removed once their adapter is gone.
    closing: BTreeSet<u64>,
}

impl Default for DebugState {
    fn default() -> Self {
        Self {
            breakpoints: Vec::new(),
            watches: Vec::new(),
            console: Vec::new(),
            session: None,
            tab: DebugTab::default(),
            expanded: BTreeSet::new(),
            evaluation: None,
            focus_evaluate: None,
            choosing: false,
            form: None,
            break_on_panic: true,
            last_target: None,
            last_options: Default::default(),
            attach: None,
            views: crate::debug_views::DebugViews::default(),
            asm: crate::asm::AsmState::default(),
            exceptions: ExceptionBreakpoints::default(),
            breakpoint_popup: None,
            hover: None,
            completions: None,
            step_targets: None,
            last_error: None,
            agent: agent::AgentState::default(),
            hover_epoch: 0,
            attach_rx: None,
            next_id: 1,
            loaded_for: None,
            anchors: HashMap::new(),
            revisions: HashMap::new(),
            rt: None,
            parked: Vec::new(),
            next_session: 1,
            stop_counter: 0,
            closing: BTreeSet::new(),
        }
    }
}

impl DebugState {
    /// A session exists and is not over.
    pub fn is_active(&self) -> bool {
        self.rt.is_some() && self.session.as_ref().is_some_and(|s| !s.status().is_finished())
    }

    pub fn is_stopped(&self) -> bool {
        self.rt.is_some() && self.session.as_ref().is_some_and(DebugSession::is_stopped)
    }

    pub fn is_running(&self) -> bool {
        self.rt.is_some() && self.session.as_ref().is_some_and(|s| s.status() == DebugStatus::Running)
    }

    /// Background work is in flight (the UI keeps polling): any session, the process list, Show
    /// Assembly.
    pub fn busy(&self) -> bool {
        self.rt.is_some() || self.parked.iter().any(Parked::live) || self.attach_rx.is_some() || self.asm.running()
    }

    /// Something will change without user input: building, starting, running, or answers still
    /// pending. False once the program is paused with everything loaded (or the session ended).
    pub fn settling(&self) -> bool {
        settling_of(self.session.as_ref(), self.rt.as_ref())
    }

    /// Every session (the selected one and the others), oldest first, for the session tabs.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let selected = self.session.as_ref().map(|x| x.id);
        let info = |x: &DebugSession, live: bool| SessionInfo {
            id: x.id,
            name: x.name.clone(),
            adapter: x.adapter.clone(),
            status: x.status(),
            selected: Some(x.id) == selected,
            live: live && !x.status().is_finished(),
            parent: x.parent,
            pid: x.debuggee_pid,
        };
        let mut out: Vec<SessionInfo> = self
            .parked
            .iter()
            .filter_map(|p| p.session.as_ref().map(|x| info(x, p.live())))
            .chain(self.session.as_ref().map(|x| info(x, self.rt.is_some())))
            .collect();
        out.sort_by_key(|x| x.id);
        out
    }

    /// How many sessions there are (over ones included).
    pub fn session_count(&self) -> usize {
        self.parked.len() + usize::from(self.session.is_some())
    }

    /// How many sessions are not over.
    pub fn live_count(&self) -> usize {
        self.sessions().iter().filter(|x| x.live).count()
    }

    /// The selected session's id.
    pub fn selected_id(&self) -> Option<u64> {
        self.session.as_ref().map(|x| x.id)
    }

    /// The selected session debugs a Rust program under LLDB.
    pub fn rust_session(&self) -> bool {
        self.rt.as_ref().is_some_and(Runtime::is_rust)
    }

    /// Embedders and tests: add a session without an adapter (it is listed, can be selected and
    /// closed, and is never live).
    pub fn add_inert_session(&mut self, mut session: DebugSession) -> u64 {
        let id = self.next_session;
        self.next_session = self.next_session.saturating_add(1);
        session.id = id;
        if self.session.is_none() {
            self.session = Some(session);
        } else {
            self.parked.push(Parked::new(session, None, self.views.fresh()));
        }
        id
    }

    fn parked_index(&self, id: u64) -> Option<usize> {
        self.parked.iter().position(|p| p.id() == id)
    }

    /// The session `id`'s data and runtime, wherever it is.
    fn find(&self, id: u64) -> Option<(&DebugSession, Option<&Runtime>)> {
        if let Some(x) = self.session.as_ref().filter(|x| x.id == id) {
            return Some((x, self.rt.as_ref()));
        }
        self.parked.iter().find(|p| p.id() == id).and_then(|p| p.session.as_ref().map(|x| (x, p.rt.as_ref())))
    }

    /// Every session's data and runtime.
    fn all(&self) -> impl Iterator<Item = (&DebugSession, Option<&Runtime>)> {
        self.session
            .as_ref()
            .map(|x| (x, self.rt.as_ref()))
            .into_iter()
            .chain(self.parked.iter().filter_map(|p| p.session.as_ref().map(|x| (x, p.rt.as_ref()))))
    }

    /// The live sessions started by `id` (subprocesses), and theirs.
    fn descendants(&self, id: u64) -> Vec<u64> {
        let links: Vec<(u64, Option<u64>)> = self.all().map(|(x, _)| (x.id, x.parent)).collect();
        let mut out = Vec::new();
        let mut frontier = vec![id];
        while let Some(parent) = frontier.pop() {
            let found: Vec<u64> = links
                .iter()
                .filter(|(c, p)| *p == Some(parent) && *c != id && !out.contains(c))
                .map(|(c, _)| *c)
                .collect();
            for child in found {
                out.push(child);
                frontier.push(child);
            }
            if out.len() > MAX_SESSIONS {
                break;
            }
        }
        out
    }

    /// Another session than the selected one is not over.
    fn others_live(&self) -> bool {
        self.parked.iter().any(Parked::live)
    }

    /// Exchange the selected session with parked session `i` (field by field).
    fn exchange(&mut self, i: usize) -> bool {
        if self.session.is_none() {
            // Nothing selected: the parked one simply comes forward.
            if i >= self.parked.len() {
                return false;
            }
            let mut p = self.parked.remove(i);
            self.session = p.session.take();
            self.rt = p.rt.take();
            self.console = std::mem::take(&mut p.console);
            self.evaluation = p.evaluation.take();
            self.hover = p.hover.take();
            self.completions = p.completions.take();
            self.step_targets = p.step_targets.take();
            self.last_error = p.last_error.take();
            let history = std::mem::take(&mut self.views.console_history);
            self.views = std::mem::take(&mut p.views);
            self.views.console_history = history;
            self.last_target = p.last_target.take();
            self.last_options = std::mem::take(&mut p.last_options);
            restore_watches(&mut self.watches, &p.watches);
            return true;
        }
        let Some(p) = self.parked.get_mut(i) else { return false };
        std::mem::swap(&mut self.session, &mut p.session);
        std::mem::swap(&mut self.rt, &mut p.rt);
        std::mem::swap(&mut self.console, &mut p.console);
        std::mem::swap(&mut self.evaluation, &mut p.evaluation);
        std::mem::swap(&mut self.hover, &mut p.hover);
        std::mem::swap(&mut self.completions, &mut p.completions);
        std::mem::swap(&mut self.step_targets, &mut p.step_targets);
        std::mem::swap(&mut self.last_error, &mut p.last_error);
        std::mem::swap(&mut self.views, &mut p.views);
        // The console's input history is the IDE's, not a session's.
        std::mem::swap(&mut self.views.console_history, &mut p.views.console_history);
        std::mem::swap(&mut self.last_target, &mut p.last_target);
        std::mem::swap(&mut self.last_options, &mut p.last_options);
        let mine = self.watches.clone();
        restore_watches(&mut self.watches, &p.watches);
        p.watches = mine;
        true
    }

    /// Make session `id` the selected one (state only: no editor navigation). False if there is
    /// no such session.
    fn bring_forward(&mut self, id: u64) -> bool {
        if self.selected_id() == Some(id) {
            return true;
        }
        match self.parked_index(id) {
            Some(i) => self.exchange(i),
            None => false,
        }
    }

    /// Put the selected session among the parked ones (a new one is about to start).
    fn park_selected(&mut self) {
        let Some(session) = self.session.take() else { return };
        let mut p = Parked::new(session, self.rt.take(), std::mem::take(&mut self.views));
        self.views = p.views.fresh();
        p.console = std::mem::take(&mut self.console);
        p.evaluation = self.evaluation.take();
        p.hover = self.hover.take();
        p.completions = self.completions.take();
        p.step_targets = self.step_targets.take();
        p.last_error = self.last_error.take();
        p.last_target = self.last_target.clone();
        p.last_options = self.last_options.clone();
        p.watches = self.watches.clone();
        for w in &mut self.watches {
            w.clear();
        }
        self.parked.push(p);
    }

    /// Drop the selected session (closed): the newest live session, else the newest one, comes
    /// forward.
    fn drop_selected(&mut self) {
        if let Some(rt) = self.rt.take() {
            let pid = self.session.as_ref().and_then(|x| x.debuggee_pid);
            let exited = rt.exited;
            rt.kill(pid, exited);
        }
        self.session = None;
        self.console.clear();
        self.evaluation = None;
        self.hover = None;
        self.completions = None;
        self.step_targets = None;
        self.last_error = None;
        self.views.reset();
        for w in &mut self.watches {
            w.clear();
        }
        let next = self
            .parked
            .iter()
            .enumerate()
            .filter(|(_, p)| p.live())
            .max_by_key(|(_, p)| p.id())
            .or_else(|| self.parked.iter().enumerate().max_by_key(|(_, p)| p.id()))
            .map(|(i, _)| i);
        if let Some(i) = next {
            self.exchange(i);
        }
    }

    fn next_session_id(&mut self) -> u64 {
        let id = self.next_session;
        self.next_session = self.next_session.saturating_add(1);
        id
    }

    /// Breakpoints in one file.
    pub fn breakpoints_in(&self, path: &Path) -> impl Iterator<Item = &Breakpoint> {
        self.breakpoints.iter().filter(move |b| b.path == path)
    }

    pub fn breakpoint_at(&self, path: &Path, line: usize) -> Option<&Breakpoint> {
        self.breakpoints.iter().find(|b| b.path == path && b.line == line)
    }

    fn push_console(&mut self, kind: DebugConsoleKind, text: impl Into<String>) {
        let text = text.into();
        // Adapters send output in chunks; split into lines, continuing an unterminated last line.
        let mut parts = text.split('\n').peekable();
        let continues = self.console.last().is_some_and(|l| l.kind == kind && l.text.ends_with('\u{0}'));
        let mut first = true;
        while let Some(part) = parts.next() {
            let last = parts.peek().is_none();
            if last && part.is_empty() && !first {
                break;
            }
            let part = part.strip_suffix('\r').unwrap_or(part);
            let mut line = crate::runs::strip_ansi(part);
            if last && !text.ends_with('\n') && matches!(kind, DebugConsoleKind::Stdout | DebugConsoleKind::Stderr) {
                // Unterminated: mark it so the next chunk continues the line.
                line.push('\u{0}');
            }
            if first
                && continues
                && let Some(prev) = self.console.last_mut()
            {
                prev.text.pop();
                prev.text.push_str(&line);
            } else {
                self.console.push(ConsoleLine { kind, text: line });
            }
            first = false;
        }
        if self.console.len() > MAX_CONSOLE_LINES {
            let excess = self.console.len() - MAX_CONSOLE_LINES;
            self.console.drain(..excess);
        }
    }

    /// Add console output (the views report failures here).
    pub(crate) fn push_console_line(&mut self, kind: DebugConsoleKind, text: impl Into<String>) {
        self.push_console(kind, text);
    }

    fn info(&mut self, text: impl Into<String>) {
        self.push_console(DebugConsoleKind::Info, text);
    }

    fn error(&mut self, text: impl Into<String>) {
        self.push_console(DebugConsoleKind::Error, text);
    }
}

impl Drop for DebugState {
    /// A session dropped without [`shutdown`] (tests, an aborted window) still leaves no
    /// processes behind.
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            let pid = self.session.as_ref().and_then(|x| x.debuggee_pid);
            let exited = rt.exited;
            rt.kill(pid, exited);
        }
    }
}

/// The text of a console line (without the internal continuation marker).
pub fn console_text(line: &ConsoleLine) -> &str {
    line.text.strip_suffix('\u{0}').unwrap_or(&line.text)
}

// ---------------------------------------------------------------- runtime

/// How to get from a [`DebugTarget`] to a running adapter.
#[derive(Clone, Debug)]
struct LaunchPlan {
    name: String,
    build: Option<BuildStep>,
    run: RunSpec,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
struct BuildStep {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    cargo: bool,
    pick: ArtifactPick,
}

#[derive(Clone, Debug)]
enum ArtifactPick {
    /// A cargo artifact with an executable (by target name when known).
    Cargo {
        name: Option<String>,
    },
    /// `zig-out/bin`: the named executable, else the newest one.
    ZigOut {
        dir: PathBuf,
        name: Option<String>,
    },
    Fixed(PathBuf),
}

#[derive(Clone, Debug)]
enum RunSpec {
    Native { program: Option<PathBuf>, rust: bool },
    Python { program: Option<PathBuf>, module: Option<String>, python: Option<PathBuf>, just_my_code: bool },
    Node { program: PathBuf },
    Attach { pid: u32, process: ProcessKind },
    Core { core: PathBuf, program: Option<PathBuf> },
    Remote { address: String, program: Option<PathBuf> },
    /// `lldb-server platform`: launch `program` on the remote machine.
    Platform { platform: String, url: String, program: PathBuf },
    PythonRemote { host: String, port: u16, mappings: Vec<dap::launch::PathMapping>, just_my_code: bool },
    AttachWait { process: String },
}

struct Prepared {
    client: DapClient,
    events: Receiver<DapEvent>,
    adapter: String,
    kind: AdapterKind,
    launch: Value,
}

enum Phase {
    Build { handle: RunHandle, events: Receiver<RunEvent>, executables: Vec<(String, PathBuf)> },
    Prepare { rx: Receiver<Result<Prepared, String>> },
    Live { client: DapClient, events: Receiver<DapEvent> },
}

/// What a request in flight is for.
#[derive(Clone, Debug)]
enum Pending {
    Initialize,
    Launch,
    ConfigurationDone,
    SetBreakpoints {
        ids: Vec<Option<u64>>,
    },
    Setup,
    Threads,
    StackTrace {
        epoch: u64,
        reveal: bool,
    },
    Scopes {
        epoch: u64,
    },
    Variables {
        epoch: u64,
        reference: i64,
    },
    Evaluate {
        epoch: u64,
        purpose: EvalPurpose,
    },
    /// continue / next / stepIn / stepOut: a failure means we are still stopped.
    Resume,
    /// A console command that resumes (`thread step-inst`): an error in its output means we are
    /// still stopped.
    ResumeCommand,
    /// Reset Frame through the LLDB console (`thread return`): the stack is read again after.
    ResetFrame,
    /// Memory, Disassembly, Registers and watchpoint requests (see `debug_views`).
    View {
        epoch: u64,
        request: crate::debug_views::ViewRequest,
    },
    /// Jump to Line, Set Value, step targets, completions, return values (see `actions`).
    Action {
        epoch: u64,
        request: actions::ActionRequest,
    },
    /// The Rust panic function breakpoint (`fallback`: `rust_begin_unwind` was tried already).
    PanicBreakpoint {
        fallback: bool,
    },
    Other,
}

impl Pending {
    fn blocks_settling(&self) -> bool {
        matches!(
            self,
            Self::Threads
                | Self::StackTrace { .. }
                | Self::Scopes { .. }
                | Self::Variables { .. }
                | Self::Evaluate { .. }
                | Self::Resume
                | Self::ResumeCommand
                | Self::View { .. }
                | Self::SetBreakpoints { .. }
                | Self::ResetFrame
        ) || matches!(self, Self::Action { request, .. } if request.blocks_settling())
    }
}

#[derive(Clone, Debug)]
enum EvalPurpose {
    Watch(String),
    Console,
    Evaluate(String),
    /// The value under the mouse.
    Hover(String),
    /// An agent's `debug.evaluate` (answered through its token).
    Agent(u64),
}

struct Runtime {
    phase: Phase,
    plan: LaunchPlan,
    /// Bumped on every stop, resume and frame or thread selection; older answers are dropped.
    epoch: u64,
    pending: HashMap<RequestId, Pending>,
    kind: AdapterKind,
    capabilities: dap::Capabilities,
    /// Adapter breakpoint id → ours.
    adapter_ids: HashMap<i64, u64>,
    /// Variables reference → tree path of the node it belongs to.
    ref_paths: HashMap<i64, String>,
    /// Files we sent breakpoints for (so removing the last one clears them in the adapter).
    sent: BTreeSet<PathBuf>,
    run_to_cursor: Option<(PathBuf, usize)>,
    /// `initialized` arrived and configuration was sent.
    configured: bool,
    /// The debuggee exited (no need to kill it).
    exited: bool,
    /// Stopped by the user or ended: adapter exit is expected.
    stopping: bool,
    stops: usize,
    /// The stop before a resume request, restored if the request fails.
    last_stop: Option<DebugStatus>,
    project_root: Option<PathBuf>,
    canonical_root: Option<PathBuf>,
    /// `launch` arguments, sent once `initialize` answers.
    launch_args: Option<Value>,
    /// The run configuration's debugger options (init commands, stop at entry, debuggee env).
    options: crate::run_config_store::DebuggerOptions,
    /// The last step command (a Step Out's stop reads the return value).
    last_step: Option<Step>,
    /// The function a Step Out left (labels the return value).
    step_from_function: Option<String>,
    /// Subprocess connections being made (each becomes a session of its own).
    children: children::Children,
    /// The adapter's ids of the Rust panic function breakpoint (a stop there is a panic).
    panic_breakpoints: Vec<i64>,
    /// The session whose adapter started this one (a debugpy subprocess's parent).
    child_of: Option<u64>,
    /// A session on an SSH host or in a container: its debug server and port forward (M16 RT).
    on_target: Option<Box<on_target::OnTarget>>,
}

impl Runtime {
    fn new(plan: LaunchPlan, phase: Phase, project_root: Option<PathBuf>) -> Self {
        let canonical_root = project_root.as_ref().and_then(|r| std::fs::canonicalize(r).ok());
        Self {
            phase,
            plan,
            epoch: 0,
            pending: HashMap::new(),
            kind: AdapterKind::Other,
            capabilities: dap::Capabilities::default(),
            adapter_ids: HashMap::new(),
            ref_paths: HashMap::new(),
            sent: BTreeSet::new(),
            run_to_cursor: None,
            configured: false,
            exited: false,
            stopping: false,
            stops: 0,
            last_stop: None,
            project_root,
            canonical_root,
            launch_args: None,
            options: Default::default(),
            last_step: None,
            step_from_function: None,
            children: children::Children::default(),
            panic_breakpoints: Vec::new(),
            child_of: None,
            on_target: None,
        }
    }

    /// A Rust program under an LLDB adapter (values get Rust's formatting clean-up).
    fn is_rust(&self) -> bool {
        matches!(self.plan.run, RunSpec::Native { rust: true, .. } | RunSpec::Attach { process: ProcessKind::Rust, .. })
            || self.on_target.as_ref().is_some_and(|o| o.is_rust())
    }

    /// Attached to a process we did not start (or a core dump, or a remote program whose pid
    /// means nothing here): stopping detaches and never kills it.
    fn attached(&self) -> bool {
        // A session on a target started its program there: stopping ends it (M16 RT).
        self.on_target.is_none()
            && matches!(
                self.plan.run,
                RunSpec::Attach { .. }
                    | RunSpec::Core { .. }
                    | RunSpec::Remote { .. }
                    | RunSpec::AttachWait { .. }
                    | RunSpec::PythonRemote { .. }
            )
    }

    /// End the session with `disconnect`: detach from an attached process, else terminate.
    fn end(&mut self, grace: Duration) {
        self.children.end_all(grace);
        if let Some(client) = self.client() {
            if self.attached() { client.detach(grace) } else { client.terminate(grace) }
        }
    }

    fn client(&self) -> Option<&DapClient> {
        match &self.phase {
            Phase::Live { client, .. } => Some(client),
            _ => None,
        }
    }

    /// Send a request and remember what it is for. False if the adapter is gone.
    fn request(&mut self, pending: Pending, send: impl FnOnce(&DapClient) -> dap::Result<RequestId>) -> bool {
        let Some(client) = self.client() else { return false };
        match send(client) {
            Ok(id) => {
                self.pending.insert(id, pending);
                true
            }
            Err(e) => {
                log::debug!("debug request failed: {e}");
                false
            }
        }
    }

    /// The path the adapter knows a file by (symlinks resolved, as in debug info).
    fn to_adapter(&self, path: &Path) -> PathBuf {
        // A native session on a target: the program was built there, with its paths (M16 RT).
        if let Some(map) = self.on_target.as_ref().and_then(|o| o.native_map())
            && let Some(remote) = on_target::to_target(map, path)
        {
            return remote;
        }
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// A path from the adapter, mapped back into the project as the user opened it.
    fn map_from_adapter(&self, path: &Path) -> PathBuf {
        if let Some(map) = self.on_target.as_ref().and_then(|o| o.native_map())
            && let Some(local) = on_target::from_target(map, path)
        {
            return local;
        }
        if let (Some(root), Some(canonical)) = (&self.project_root, &self.canonical_root)
            && root != canonical
            && let Ok(rest) = path.strip_prefix(canonical)
        {
            return root.join(rest);
        }
        crate::normalize_path(path)
    }

    /// Kill everything now (window closing).
    fn kill(mut self, pid: Option<u32>, exited: bool) {
        self.children.kill_all();
        match self.phase {
            Phase::Build { handle, .. } => {
                let _ = handle.kill();
            }
            Phase::Prepare { .. } => {
                // The worker's client is dropped (and killed) when its send fails.
            }
            Phase::Live { client, .. } => client.kill(),
        }
        if !exited && let Some(pid) = pid {
            kill_debuggee(pid);
        }
    }
}

/// A debuggee that may have left its adapter's process group (lldb launches it in its own).
fn kill_debuggee(pid: u32) {
    dap::kill_process_group(pid, "KILL");
    dap::kill_process(pid, "KILL");
}

// ---------------------------------------------------------------- targets → plans

fn plan_for(target: &DebugTarget) -> Result<LaunchPlan, String> {
    match target {
        DebugTarget::Cargo { name, invocation } => cargo_plan(name, invocation),
        DebugTarget::Executable { name, program, args, cwd, env, build } => {
            let build = build.split_first().map(|(cmd, rest)| BuildStep {
                program: PathBuf::from(cmd),
                args: rest.to_vec(),
                cwd: cwd.clone().or_else(|| program.parent().map(Path::to_path_buf)).unwrap_or_default(),
                env: env.clone(),
                cargo: false,
                pick: ArtifactPick::Fixed(program.clone()),
            });
            if build.is_none() && !program.is_file() {
                return Err(format!("{} does not exist", program.display()));
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build,
                run: RunSpec::Native { program: Some(program.clone()), rust: false },
                args: args.clone(),
                cwd: cwd.clone().or_else(|| program.parent().map(Path::to_path_buf)),
                env: env.clone(),
            })
        }
        DebugTarget::Python { name, program, module, args, cwd, env, python, just_my_code } => {
            if program.is_none() && module.is_none() {
                return Err("a Python target needs a program or a module".into());
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::Python {
                    program: program.clone(),
                    module: module.clone(),
                    python: python.clone(),
                    just_my_code: *just_my_code,
                },
                args: python_debug_args(program.as_deref(), module.as_deref(), args),
                cwd: cwd.clone().or_else(|| program.as_ref().and_then(|p| p.parent().map(Path::to_path_buf))),
                env: env.clone(),
            })
        }
        DebugTarget::Zig { name, root, exe, args } => {
            let zig = dap::discover::which("zig").ok_or("zig is not installed (https://ziglang.org/download/)")?;
            Ok(LaunchPlan {
                name: name.clone(),
                build: Some(BuildStep {
                    program: zig,
                    args: vec!["build".into()],
                    cwd: root.clone(),
                    env: Vec::new(),
                    cargo: false,
                    pick: ArtifactPick::ZigOut { dir: root.join("zig-out").join("bin"), name: exe.clone() },
                }),
                run: RunSpec::Native { program: None, rust: false },
                args: args.clone(),
                cwd: Some(root.clone()),
                env: Vec::new(),
            })
        }
        DebugTarget::ZigFile { name, path, args } => {
            let zig = dap::discover::which("zig").ok_or("zig is not installed (https://ziglang.org/download/)")?;
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "main".into());
            let out_dir = std::env::temp_dir().join("codecraft-zig");
            std::fs::create_dir_all(&out_dir).map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
            let out = out_dir.join(&stem);
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            Ok(LaunchPlan {
                name: name.clone(),
                build: Some(BuildStep {
                    program: zig,
                    args: vec!["build-exe".into(), path.display().to_string(), format!("-femit-bin={}", out.display())],
                    cwd: dir.clone(),
                    env: Vec::new(),
                    cargo: false,
                    pick: ArtifactPick::Fixed(out),
                }),
                run: RunSpec::Native { program: None, rust: false },
                args: args.clone(),
                cwd: Some(dir),
                env: Vec::new(),
            })
        }
        DebugTarget::Attach { name, pid, process } => {
            if *pid == 0 || *pid == std::process::id() {
                return Err(format!("cannot attach to process {pid}"));
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::Attach { pid: *pid, process: *process },
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            })
        }
        DebugTarget::CoreDump { name, core, program } => {
            if !core.is_file() {
                return Err(format!("{} does not exist", core.display()));
            }
            if let Some(program) = program.as_ref().filter(|p| !p.is_file()) {
                return Err(format!("{} does not exist", program.display()));
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::Core { core: core.clone(), program: program.clone() },
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            })
        }
        DebugTarget::Remote { name, address, program, platform, args } if address.trim().starts_with("connect://") => {
            if !dap::launch::valid_platform_url(address) {
                return Err(format!("`{address}` is not a connect://host:port address"));
            }
            let platform = platform.clone().unwrap_or_else(|| "remote-linux".into());
            if !dap::launch::valid_platform_name(&platform) {
                return Err(format!("`{platform}` is not an LLDB platform name (remote-linux, remote-macosx, …)"));
            }
            let program = program.clone().ok_or("a platform connection needs the program to launch there")?;
            if !program.is_file() {
                return Err(format!("{} does not exist", program.display()));
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::Platform { platform, url: address.trim().to_owned(), program },
                args: args.clone(),
                cwd: None,
                env: Vec::new(),
            })
        }
        DebugTarget::Remote { name, address, program, .. } => {
            if !dap::launch::valid_remote_address(address) {
                return Err(format!("`{address}` is not a host:port address"));
            }
            if let Some(program) = program.as_ref().filter(|p| !p.is_file()) {
                return Err(format!("{} does not exist", program.display()));
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::Remote { address: address.trim().to_owned(), program: program.clone() },
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            })
        }
        DebugTarget::PythonRemote { name, host, port, path_mappings, just_my_code } => {
            if !dap::launch::valid_remote_address(&format!("{}:{port}", host.trim())) {
                return Err(format!("`{host}:{port}` is not a host:port address"));
            }
            if path_mappings.len() > 64
                || path_mappings.iter().any(|m| m.local_root.trim().is_empty() || m.remote_root.trim().is_empty())
            {
                return Err("each path mapping needs a local and a remote directory".into());
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::PythonRemote {
                    host: host.trim().to_owned(),
                    port: *port,
                    mappings: path_mappings.clone(),
                    just_my_code: *just_my_code,
                },
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            })
        }
        DebugTarget::AttachWait { name, process } => {
            if !dap::launch::valid_wait_name(process) {
                return Err(format!("`{process}` is not a process name"));
            }
            Ok(LaunchPlan {
                name: name.clone(),
                build: None,
                run: RunSpec::AttachWait { process: process.trim().to_owned() },
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            })
        }
        DebugTarget::Node { name, program, args, cwd } => Ok(LaunchPlan {
            name: name.clone(),
            build: None,
            run: RunSpec::Node { program: program.clone() },
            args: args.clone(),
            cwd: cwd.clone().or_else(|| program.parent().map(Path::to_path_buf)),
            env: Vec::new(),
        }),
        DebugTarget::OnTarget { name, target, .. } => {
            Err(format!("{name} runs on {}: start it with the session (debug.start)", target.label()))
        }
    }
}

/// A Python web framework's arguments for debugging: its auto-reloader restarts the server in
/// a new process (breakpoints would stop in a process the debugger didn't start), so Django's
/// `runserver` gets `--noreload`, uvicorn loses `--reload` (and its `--reload-*` options) and
/// `flask run` gets `--no-reload --no-debugger` (Werkzeug's debugger would catch exceptions
/// first), as PyCharm does.
pub fn python_debug_args(program: Option<&Path>, module: Option<&str>, args: &[String]) -> Vec<String> {
    let manage_py = program.and_then(Path::file_name).is_some_and(|n| n == "manage.py");
    match module {
        _ if manage_py => {
            let mut out = args.to_vec();
            if args.first().is_some_and(|a| a == "runserver") && !args.iter().any(|a| a == "--noreload") {
                out.push("--noreload".into());
            }
            out
        }
        Some("uvicorn") => {
            let mut out = Vec::new();
            let mut skip_value = false;
            for a in args {
                if skip_value {
                    skip_value = false;
                    continue;
                }
                match a.as_str() {
                    "--reload" => {}
                    "--reload-dir" | "--reload-include" | "--reload-exclude" | "--reload-delay" => skip_value = true,
                    other if other.starts_with("--reload-") || other.starts_with("--reload=") => {}
                    _ => out.push(a.clone()),
                }
            }
            out
        }
        Some("flask") => {
            let mut out: Vec<String> = args.iter().filter(|a| *a != "--reload" && *a != "--debugger").cloned().collect();
            if out.iter().any(|a| a == "run") {
                for flag in ["--no-reload", "--no-debugger"] {
                    if !out.iter().any(|a| a == flag) {
                        out.push(flag.into());
                    }
                }
            }
            out
        }
        _ => args.to_vec(),
    }
}

fn cargo_plan(name: &str, inv: &CargoInvocation) -> Result<LaunchPlan, String> {
    let mut build = inv.clone();
    build.json_messages = true;
    build.test_filter = None;
    build.test_exact = false;
    build.program_args.clear();
    let target_name = |t: &Option<TargetSelector>| match t {
        Some(
            TargetSelector::Bin(n) | TargetSelector::Example(n) | TargetSelector::Test(n) | TargetSelector::Bench(n),
        ) => Some(n.clone()),
        _ => None,
    };
    let (pick, args) = match inv.command {
        CargoCommand::Run => {
            build.command = CargoCommand::Build;
            (target_name(&inv.target), inv.program_args.clone())
        }
        CargoCommand::Test => {
            if !build.extra_args.iter().any(|a| a == "--no-run") {
                build.extra_args.push("--no-run".into());
            }
            let mut args = Vec::new();
            if let Some(filter) = inv.test_filter.as_deref().filter(|f| !f.is_empty()) {
                args.push(filter.to_owned());
                if inv.test_exact {
                    args.push("--exact".into());
                }
            }
            args.extend(inv.program_args.iter().filter(|a| *a != "--nocapture").cloned());
            args.push("--nocapture".into());
            (target_name(&inv.target), args)
        }
        other => return Err(format!("cargo {} configurations can't be debugged", other.subcommand())),
    };
    Ok(LaunchPlan {
        name: name.to_owned(),
        build: Some(BuildStep {
            program: codecraft_cargo::find_cargo(),
            args: build.args(),
            cwd: inv.cwd.clone(),
            env: inv.env.clone(),
            cargo: true,
            pick: ArtifactPick::Cargo { name: pick },
        }),
        run: RunSpec::Native { program: None, rust: true },
        args,
        cwd: Some(inv.cwd.clone()),
        env: inv.env.clone(),
    })
}

/// The executable a build produced.
fn pick_artifact(pick: &ArtifactPick, executables: &[(String, PathBuf)]) -> Result<PathBuf, String> {
    match pick {
        ArtifactPick::Fixed(path) => {
            if path.is_file() {
                Ok(path.clone())
            } else {
                Err(format!("the build did not produce {}", path.display()))
            }
        }
        ArtifactPick::Cargo { name } => {
            let named = name.as_ref().and_then(|n| {
                let alt = n.replace('-', "_");
                executables.iter().rev().find(|(t, _)| t == n || *t == alt)
            });
            match (named, executables) {
                (Some((_, path)), _) => Ok(path.clone()),
                (None, [(_, only)]) => Ok(only.clone()),
                (None, []) => Err("the build produced no executable".into()),
                (None, many) => Err(format!(
                    "the build produced {} executables ({}); debug a single target",
                    many.len(),
                    many.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                )),
            }
        }
        ArtifactPick::ZigOut { dir, name } => {
            if let Some(name) = name {
                let path = dir.join(name);
                return if path.is_file() { Ok(path) } else { Err(format!("{} was not built", path.display())) };
            }
            let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            entries
                .flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                .max_by_key(|(modified, _)| *modified)
                .map(|(_, path)| path)
                .ok_or_else(|| format!("no executable in {}", dir.display()))
        }
    }
}

/// The plan with a configuration's debuggee environment added (it wins over the plan's).
fn with_options(mut plan: LaunchPlan, options: &crate::run_config_store::DebuggerOptions) -> LaunchPlan {
    for (k, v) in &options.env {
        plan.env.retain(|(o, _)| o != k);
        plan.env.push((k.clone(), v.clone()));
    }
    plan
}

/// Runs on a worker thread: find the adapter, work out launch arguments, spawn it.
fn prepare(
    plan: &LaunchPlan,
    program: Option<PathBuf>,
    options: &crate::run_config_store::DebuggerOptions,
) -> Result<Prepared, String> {
    let extra_init = |mut init: Vec<String>, kind: AdapterKind| {
        if matches!(kind, AdapterKind::LldbDap | AdapterKind::CodeLldb) {
            init.extend(options.init_commands.iter().cloned());
        }
        init
    };
    let (adapter, launch) = match &plan.run {
        RunSpec::Native { program: fixed, rust } => {
            let program = program.or_else(|| fixed.clone()).ok_or("nothing to debug: no executable")?;
            let adapter = dap::discover::find_native().map_err(|e| e.to_string())?;
            let init_commands = if *rust && adapter.kind == AdapterKind::LldbDap {
                dap::discover::rust_sysroot(None)
                    .map(|s| dap::discover::rust_lldb_init_commands(&s))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let init_commands = extra_init(init_commands, adapter.kind);
            let launch = dap::launch::native(
                adapter.kind,
                &dap::launch::NativeLaunch {
                    name: plan.name.clone(),
                    program,
                    args: plan.args.clone(),
                    cwd: plan.cwd.clone(),
                    env: plan.env.clone(),
                    init_commands,
                    stop_on_entry: options.stop_at_entry,
                    source_languages: if *rust { vec!["rust".into()] } else { Vec::new() },
                },
            );
            (adapter, launch)
        }
        RunSpec::Python { program, module, python, just_my_code } => {
            let adapter = dap::discover::find_debugpy(python.as_deref()).map_err(|e| e.to_string())?;
            let django = program.as_deref().and_then(Path::file_name).is_some_and(|n| n == "manage.py");
            let launch = dap::launch::python(&dap::launch::PythonLaunch {
                name: plan.name.clone(),
                program: program.clone(),
                module: module.clone(),
                args: plan.args.clone(),
                cwd: plan.cwd.clone(),
                env: plan.env.clone(),
                python: python.clone(),
                just_my_code: options.just_my_code.unwrap_or(*just_my_code),
                stop_on_entry: options.stop_at_entry,
                sub_process: options.sub_process.unwrap_or(true),
                django,
                jinja: module.as_deref() == Some("flask"),
            });
            (adapter, launch)
        }
        RunSpec::PythonRemote { host, port, mappings, just_my_code } => {
            // `debugpy --listen` serves its own adapter there: talk DAP to it directly.
            let just_my_code = options.just_my_code.unwrap_or(*just_my_code);
            let launch = dap::launch::python_remote(&plan.name, host, *port, mappings, just_my_code);
            let (client, events) = connect_remote(host, *port)?;
            return Ok(Prepared { client, events, adapter: "debugpy (remote)".into(), kind: AdapterKind::Debugpy, launch });
        }
        RunSpec::Platform { platform, url, program } => {
            let adapter = dap::discover::find_native().map_err(|e| e.to_string())?;
            let init = extra_init(rust_init_commands(adapter.kind), adapter.kind);
            let args = dap::launch::native_platform(adapter.kind, &plan.name, platform, url, program, &plan.args, &init);
            (adapter, args)
        }
        RunSpec::Attach { pid, process: ProcessKind::Python } => {
            let adapter = dap::discover::find_debugpy(None).map_err(|e| e.to_string())?;
            (adapter, dap::launch::python_attach(&plan.name, *pid, options.just_my_code.unwrap_or(true)))
        }
        RunSpec::Attach { pid, process } => {
            let adapter = dap::discover::find_native().map_err(|e| e.to_string())?;
            let init_commands = if *process == ProcessKind::Rust && adapter.kind == AdapterKind::LldbDap {
                dap::discover::rust_sysroot(None)
                    .map(|s| dap::discover::rust_lldb_init_commands(&s))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let init_commands = extra_init(init_commands, adapter.kind);
            let args = dap::launch::native_attach(adapter.kind, &plan.name, *pid, &init_commands);
            (adapter, args)
        }
        RunSpec::Core { core, program } => {
            let adapter = dap::discover::find_native().map_err(|e| e.to_string())?;
            let init = rust_init_commands(adapter.kind);
            let args = dap::launch::native_core(adapter.kind, &plan.name, program.as_deref(), core, &init);
            (adapter, args)
        }
        RunSpec::AttachWait { process } => {
            let adapter = dap::discover::find_native().map_err(|e| e.to_string())?;
            let init = rust_init_commands(adapter.kind);
            let args = dap::launch::native_attach_wait(adapter.kind, &plan.name, process, &init);
            (adapter, args)
        }
        RunSpec::Remote { address, program } => {
            let adapter = dap::discover::find_native().map_err(|e| e.to_string())?;
            // The configuration's init commands too (a session on a target maps its sources).
            let init = extra_init(rust_init_commands(adapter.kind), adapter.kind);
            let args = dap::launch::native_remote(adapter.kind, &plan.name, program.as_deref(), address, &init);
            (adapter, args)
        }
        RunSpec::Node { program } => {
            let adapter = dap::discover::find_js_debug().map_err(|e| e.to_string())?;
            let launch = dap::launch::node(&dap::launch::NodeLaunch {
                name: plan.name.clone(),
                program: program.clone(),
                args: plan.args.clone(),
                cwd: plan.cwd.clone(),
                env: plan.env.clone(),
                runtime: None,
            });
            (adapter, launch)
        }
    };
    let (client, events) = DapClient::spawn(&adapter).map_err(|e| e.to_string())?;
    Ok(Prepared { client, events, adapter: adapter.name, kind: adapter.kind, launch })
}

/// How long a remote debug server has to accept the connection (it may still be starting).
const REMOTE_CONNECT_WINDOW: Duration = Duration::from_secs(20);

/// Connect to a debug adapter listening on `host:port`, retrying while it starts (worker thread).
fn connect_remote(host: &str, port: u16) -> Result<(DapClient, Receiver<DapEvent>), String> {
    let deadline = std::time::Instant::now() + REMOTE_CONNECT_WINDOW;
    loop {
        match DapClient::connect_tcp_host("debugpy", host, port, Duration::from_secs(3)) {
            Ok(connected) => return Ok(connected),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(format!("cannot connect to the debug server at {host}:{port}: {e}"));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Rust pretty printers for lldb-dap (programs that may or may not be Rust: core dumps, remote).
fn rust_init_commands(kind: AdapterKind) -> Vec<String> {
    if kind != AdapterKind::LldbDap {
        return Vec::new();
    }
    dap::discover::rust_sysroot(None).map(|s| dap::discover::rust_lldb_init_commands(&s)).unwrap_or_default()
}

fn adapter_id(kind: AdapterKind) -> &'static str {
    match kind {
        AdapterKind::LldbDap => "lldb-dap",
        AdapterKind::CodeLldb => "lldb",
        AdapterKind::Debugpy => "debugpy",
        AdapterKind::JsDebug => "pwa-node",
        AdapterKind::Other => "codecraft",
    }
}

/// A Rust value as RustRover shows it. lldb-dap prints a pointer-typed value with a summary as
/// `0x000000010000c1f0 "hello"` (a `&str` whose formatter gives the text): keep the summary only.
/// Values without a summary (plain pointers, numbers) are unchanged.
pub fn rust_value(raw: String) -> String {
    let Some(rest) = raw.strip_prefix("0x") else { return raw };
    let digits = rest.chars().take_while(char::is_ascii_hexdigit).count();
    match rest.get(digits..).and_then(|r| r.strip_prefix(' ')) {
        Some(summary) if digits > 0 && (summary.starts_with(['"', '\'']) || summary.starts_with("size=")) => {
            summary.to_owned()
        }
        _ => raw,
    }
}

/// Output without LLDB's "This version of LLDB has no plugin for the language "rust"" warning,
/// which Xcode's LLDB prints in every Rust session although the toolchain's formatters read the
/// values (QA #22).
pub fn without_rust_plugin_warning(output: &str) -> String {
    if !output.contains("no plugin for the language") {
        return output.to_owned();
    }
    output
        .split_inclusive('\n')
        .filter(|l| !(l.contains("no plugin for the language") && l.to_ascii_lowercase().contains("rust")))
        .collect()
}

// ---------------------------------------------------------------- starting and stopping

/// Start debugging `target` (stopping a session that is still running).
pub(crate) fn start(s: &mut Session, target: DebugTarget) -> Result<(), CommandError> {
    start_with(s, target, Default::default())
}

/// Run › Debugging Actions › Compile and Reload Modified Files. Rust and C/C++ can't swap new
/// code into a running program (no hot swap), so the session's target is rebuilt and the session
/// restarted with the same debugger options; breakpoints, watches and expanded nodes are kept.
/// Returns the status message.
pub(crate) fn compile_and_reload(s: &mut Session) -> Result<String, CommandError> {
    let target =
        s.debug.last_target.clone().ok_or_else(|| CommandError::Disabled("there is no debug session".into()))?;
    // On a target: rebuild there and restart (M16 RT).
    if let DebugTarget::OnTarget { name, target: t, .. } = &target {
        let message = format!("{name}: rebuilding on {} and restarting (breakpoints kept)", t.label());
        let options = s.debug.last_options.clone();
        start_with(s, target.clone(), options)?;
        s.status = message.clone();
        return Ok(message);
    }
    let plan = plan_for(&target).map_err(CommandError::Failed)?;
    if matches!(
        plan.run,
        RunSpec::Attach { .. }
            | RunSpec::Core { .. }
            | RunSpec::Remote { .. }
            | RunSpec::AttachWait { .. }
            | RunSpec::PythonRemote { .. }
            | RunSpec::Platform { .. }
    ) {
        return Err(CommandError::Failed(format!(
            "{}: CodeCraft didn't start this program (attached, core dump or remote), so it can't rebuild and restart it",
            target.name()
        )));
    }
    let rebuilds = plan.build.is_some();
    let name = target.name().to_owned();
    let options = s.debug.last_options.clone();
    start_with(s, target, options)?;
    let message = if rebuilds {
        format!(
            "{name}: rebuilding and restarting — Rust and C/C++ have no hot swap, so the debug session restarts with the new build (breakpoints kept)"
        )
    } else {
        format!("{name}: restarted with the modified files (no build step; breakpoints kept)")
    };
    s.status = message.clone();
    Ok(message)
}

/// Start debugging `target` with a run configuration's debugger options.
pub(crate) fn start_with(
    s: &mut Session,
    target: DebugTarget,
    options: crate::run_config_store::DebuggerOptions,
) -> Result<(), CommandError> {
    // A command's `target` param runs what it starts there (M16 RT).
    let target = on_target::wrap(s, &crate::run_targets::RunTarget::Local, target);
    // On an SSH host or in a container (M16 RT): the target side plans the session.
    let (plan, remote) = match &target {
        DebugTarget::OnTarget { name, target: t, debug } => {
            let (plan, on) = on_target::plan(s, name, t, debug).map_err(CommandError::Failed)?;
            (plan, Some(Box::new(on)))
        }
        other => (plan_for(other).map_err(CommandError::Failed)?, None),
    };
    // Debugging a configuration that is being debugged restarts it (RustRover's single
    // instance); other sessions keep running next to the new one.
    let same: Vec<u64> = s
        .debug
        .all()
        .filter(|(x, rt)| rt.is_some() && x.parent.is_none() && x.name == plan.name)
        .map(|(x, _)| x.id)
        .collect();
    for id in same {
        with_session(s, id, |s| {
            stop(s);
            s.debug.drop_selected();
        });
    }
    // Sessions that are over make room (their tabs close).
    s.debug.parked.retain(Parked::live);
    let live = s.debug.live_count();
    if live >= MAX_SESSIONS {
        return Err(CommandError::Failed(format!(
            "{live} debug sessions are running; stop one first (Run › Debugging Actions › Stop All Programs)"
        )));
    }
    if s.debug.rt.is_some() {
        s.debug.park_selected();
    } else {
        s.debug.drop_selected();
        s.debug.parked.retain(Parked::live);
        if s.debug.session.is_some() {
            s.debug.park_selected();
        }
    }
    ensure_loaded(s);
    let _ = s.save_all();
    if !s.debug.others_live() {
        reset_verification(s);
    }
    for w in &mut s.debug.watches {
        w.clear();
    }
    s.debug.evaluation = None;
    s.debug.hover = None;
    s.debug.completions = None;
    s.debug.step_targets = None;
    s.debug.last_error = None;
    s.debug.choosing = false;
    s.debug.views.reset();
    s.debug.last_target = Some(target.clone());
    s.debug.last_options = options.clone();
    s.debug.console.clear();
    let id = s.debug.next_session_id();
    s.debug.session = Some(DebugSession { id, name: plan.name.clone(), ..DebugSession::default() });
    s.ws.layout.show(ToolWindowId::Debug);
    let root = s.project_root();
    let phase = match &plan.build {
        Some(build) => {
            s.debug.tab = DebugTab::Console;
            let line = match &remote {
                Some(on) => on_target::describe(on),
                None => std::iter::once(build.program.display().to_string())
                    .chain(build.args.iter().map(|a| codecraft_cargo::shell_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            s.debug.info(line);
            let (handle, events) =
                codecraft_cargo::spawn_process(&build.program, &build.args, &build.cwd, &build.env, build.cargo)
                    .map_err(|e| CommandError::Failed(e.to_string()))?;
            set_status(s, DebugStatus::Building);
            Phase::Build { handle, events, executables: Vec::new() }
        }
        None => {
            set_status(s, DebugStatus::Starting);
            Phase::Prepare { rx: spawn_prepare(plan.clone(), None, options.clone()) }
        }
    };
    let mut rt = Runtime::new(plan, phase, root);
    rt.options = options;
    if let Some(on) = remote {
        s.status = format!("Debugging {} on {}", target.name(), on.label());
        rt.on_target = Some(on);
        s.debug.rt = Some(rt);
        return Ok(());
    }
    s.debug.rt = Some(rt);
    s.status = format!("Debugging {}", target.name());
    Ok(())
}

fn spawn_prepare(
    plan: LaunchPlan,
    program: Option<PathBuf>,
    options: crate::run_config_store::DebuggerOptions,
) -> Receiver<Result<Prepared, String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("debug-prepare".into()).spawn(move || {
        let result = prepare(&with_options(plan, &options), program, &options);
        // A dropped receiver (session cancelled) drops — and so kills — the client.
        let _ = tx.send(result);
    });
    if let Err(e) = spawned {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(Err(format!("cannot start a worker thread: {e}")));
        return rx;
    }
    rx
}

fn set_status(s: &mut Session, status: DebugStatus) {
    if let Some(session) = &mut s.debug.session {
        session.status = Some(status);
    }
}

/// Stop the selected session (and the subprocess sessions it started): kill a build, cancel a
/// start, or end the debuggee (`disconnect`).
pub(crate) fn stop(s: &mut Session) -> bool {
    let children = s.debug.selected_id().map(|id| s.debug.descendants(id)).unwrap_or_default();
    let stopped = stop_selected(s);
    for id in children {
        if s.debug.find(id).is_some_and(|(_, rt)| rt.is_some()) {
            with_session(s, id, stop_selected);
        }
    }
    stopped
}

/// Stop every session (Stop All). Returns how many were running.
pub(crate) fn stop_all(s: &mut Session) -> usize {
    let ids: Vec<u64> = s.debug.sessions().into_iter().filter(|x| x.live).map(|x| x.id).collect();
    for id in &ids {
        if s.debug.find(*id).is_some_and(|(_, rt)| rt.is_some()) {
            with_session(s, *id, stop_selected);
        }
    }
    if !ids.is_empty() {
        s.status = format!("Stopped {} debug session{}", ids.len(), if ids.len() == 1 { "" } else { "s" });
    }
    ids.len()
}

/// Run `f` with session `id` selected, then select the previous one again (unless `f` selected
/// another one itself, e.g. by starting a session). Unknown ids run `f` on the selected session.
pub(crate) fn with_session<R>(s: &mut Session, id: u64, f: impl FnOnce(&mut Session) -> R) -> R {
    let previous = s.debug.selected_id();
    if previous == Some(id) || !s.debug.bring_forward(id) {
        return f(s);
    }
    let result = f(s);
    if let Some(previous) = previous
        && s.debug.selected_id() == Some(id)
    {
        s.debug.bring_forward(previous);
    }
    result
}

/// Run `f` on every session that is not over (the selected one first).
fn for_each_live(s: &mut Session, mut f: impl FnMut(&mut Session)) {
    if s.debug.rt.is_some() {
        f(s);
    }
    let ids: Vec<u64> = s.debug.parked.iter().filter(|p| p.live()).map(Parked::id).collect();
    for id in ids {
        with_session(s, id, &mut f);
    }
}

/// Run `f` on every session that is not over; how many times it returned true.
pub(crate) fn each_session(s: &mut Session, mut f: impl FnMut(&mut Session) -> bool) -> usize {
    let mut count = 0;
    for_each_live(s, |s| {
        if f(s) {
            count += 1;
        }
    });
    count
}

/// A session by id (a number or a numeric string) or by name (the newest live one with that
/// name first).
pub(crate) fn resolve_session(d: &DebugState, v: &Value) -> Result<u64, CommandError> {
    let list = d.sessions();
    let by_id = |id: u64| {
        list.iter().find(|x| x.id == id).map(|x| x.id).ok_or_else(|| {
            CommandError::BadParams(format!("no debug session {id} (debug.sessions lists them)"))
        })
    };
    match v {
        Value::Number(n) => by_id(n.as_u64().ok_or_else(|| CommandError::BadParams("`session` must be an id".into()))?),
        Value::String(text) => {
            let text = text.trim();
            if let Ok(id) = text.parse::<u64>()
                && list.iter().any(|x| x.id == id)
            {
                return Ok(id);
            }
            let pick = |exact: bool| {
                list.iter()
                    .filter(|x| if exact { x.name == text } else { x.name.eq_ignore_ascii_case(text) })
                    .max_by_key(|x| (x.live, x.id))
                    .map(|x| x.id)
            };
            pick(true).or_else(|| pick(false)).ok_or_else(|| {
                let names: Vec<String> = list.iter().map(|x| format!("{} ({})", x.name, x.id)).collect();
                CommandError::BadParams(if names.is_empty() {
                    format!("no debug session `{text}`: nothing is being debugged")
                } else {
                    format!("no debug session `{text}`; have: {}", names.join(", "))
                })
            })
        }
        _ => Err(CommandError::BadParams("`session` is a session id or name".into())),
    }
}

/// The session a command's `session` param names, for the commands that act on a session
/// (`debug.*`, `run.debuggingActions.*`); `None` without the param.
pub(crate) fn session_scope(s: &Session, id: &str, params: &Value) -> Result<Option<u64>, CommandError> {
    if !(id.starts_with("debug.") || id.starts_with("run.debuggingActions.")) {
        return Ok(None);
    }
    if matches!(id, "debug.selectSession" | "debug.closeSession" | "debug.sessions" | "debug.poll") {
        return Ok(None);
    }
    match params.get("session") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => resolve_session(&s.debug, v).map(Some),
    }
}

/// `debug.selectSession`: show session `id` in the Debug tool window and the editor.
pub(crate) fn select_session(s: &mut Session, id: u64) -> Result<(), CommandError> {
    if !s.debug.bring_forward(id) {
        return Err(CommandError::BadParams(format!("no debug session {id}")));
    }
    s.debug.choosing = false;
    s.ws.layout.show(ToolWindowId::Debug);
    let Some(session) = s.debug.session.as_ref() else { return Ok(()) };
    s.status = format!("{}: {}", session.name, session.status().label());
    let frame = session.selected_frame().map(|f| f.id);
    if let Some((path, line)) = session.execution.clone() {
        reveal_line(s, &path, line);
    }
    // Watches added while another session was selected get this session's values.
    if s.debug.is_stopped()
        && let Some(rt) = s.debug.rt.as_mut()
    {
        let epoch = rt.epoch;
        for w in s.debug.watches.iter().filter(|w| w.value.is_none() && w.error.is_none()) {
            let purpose = EvalPurpose::Watch(w.expression.clone());
            rt.request(Pending::Evaluate { epoch, purpose }, |c| c.evaluate(&w.expression, frame, "watch"));
        }
    }
    refresh_processes(&mut s.debug);
    Ok(())
}

/// `debug.closeSession`: stop session `id` if it runs and close its tab (once its adapter is
/// gone, for a running one).
pub(crate) fn close_session(s: &mut Session, id: u64) -> Result<(), CommandError> {
    let (_, rt) = s.debug.find(id).ok_or_else(|| CommandError::BadParams(format!("no debug session {id}")))?;
    if rt.is_some() {
        with_session(s, id, stop);
        if s.debug.find(id).is_some_and(|(_, rt)| rt.is_some()) {
            s.debug.closing.insert(id);
            return Ok(());
        }
    }
    remove_session(s, id);
    Ok(())
}

fn remove_session(s: &mut Session, id: u64) {
    s.debug.closing.remove(&id);
    if s.debug.selected_id() == Some(id) {
        s.debug.drop_selected();
    } else if let Some(i) = s.debug.parked_index(id) {
        s.debug.parked.remove(i);
    }
    refresh_processes(&mut s.debug);
}

/// Copy each family's process list (a parent and its subprocess sessions) into its sessions, for
/// the stop summary and the status line.
fn refresh_processes(d: &mut DebugState) {
    struct Link {
        id: u64,
        parent: Option<u64>,
        name: String,
        pid: Option<u32>,
        stopped: bool,
    }
    let links: Vec<Link> = d
        .all()
        .map(|(x, _)| Link {
            id: x.id,
            parent: x.parent,
            name: x.name.clone(),
            pid: x.debuggee_pid,
            stopped: x.is_stopped(),
        })
        .collect();
    let root = |mut id: u64| {
        for _ in 0..MAX_SESSIONS {
            match links.iter().find(|l| l.id == id).and_then(|l| l.parent) {
                Some(parent) if links.iter().any(|l| l.id == parent) => id = parent,
                _ => break,
            }
        }
        id
    };
    let entries = |me: u64| -> Vec<ProcessEntry> {
        let family = root(me);
        let members: Vec<&Link> = links.iter().filter(|l| root(l.id) == family).collect();
        if members.len() < 2 {
            return Vec::new();
        }
        members
            .into_iter()
            .map(|l| ProcessEntry {
                label: if l.parent.is_none() { "Main process".into() } else { l.name.clone() },
                pid: l.pid,
                focused: l.id == me,
                stopped: l.stopped,
            })
            .collect()
    };
    if let Some(x) = d.session.as_mut() {
        x.processes = entries(x.id);
    }
    for p in &mut d.parked {
        if let Some(x) = p.session.as_mut() {
            x.processes = entries(x.id);
        }
    }
}

/// Stop the selected session only.
fn stop_selected(s: &mut Session) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    rt.stopping = true;
    let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
    let finished = match &rt.phase {
        Phase::Build { handle, .. } => {
            let _ = handle.kill();
            true
        }
        Phase::Prepare { .. } => true,
        Phase::Live { .. } => {
            rt.end(TERMINATE_GRACE);
            if !rt.exited
                && let Some(pid) = pid
            {
                // Make sure the debuggee goes too, even if it left the adapter's group.
                let _ = std::thread::Builder::new().name("debug-reaper".into()).spawn(move || {
                    std::thread::sleep(TERMINATE_GRACE + Duration::from_millis(500));
                    kill_debuggee(pid);
                });
            }
            false
        }
    };
    if finished {
        s.debug.rt = None;
    }
    end_session(s, DebugStatus::Exited { code: None }, "Debugging stopped");
    true
}

/// Mark the session over and clear the stop state.
fn end_session(s: &mut Session, status: DebugStatus, message: &str) {
    if let Some(session) = &mut s.debug.session {
        if session.status().is_finished() {
            return;
        }
        if let (DebugStatus::Exited { code: None }, Some(DebugStatus::Exited { code: Some(_) })) =
            (&status, &session.status)
        {
            return;
        }
        session.clear_stop();
        session.status = Some(status);
    }
    // Another session's breakpoints are still set.
    if !s.debug.others_live() {
        reset_verification(s);
    }
    s.debug.views.on_session_end();
    s.status = message.to_owned();
}

fn reset_verification(s: &mut Session) {
    for bp in &mut s.debug.breakpoints {
        bp.verified = None;
        bp.message = None;
    }
}

/// Stop background work (window closing): kill adapters, builds and debuggees right away.
pub(crate) fn shutdown(s: &mut Session) {
    if let Some(rt) = s.debug.rt.take() {
        let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
        let exited = rt.exited;
        rt.kill(pid, exited);
    }
    for p in &mut s.debug.parked {
        if let Some(rt) = p.rt.take() {
            let pid = p.session.as_ref().and_then(|x| x.debuggee_pid);
            let exited = rt.exited;
            rt.kill(pid, exited);
        }
    }
}

// ---------------------------------------------------------------- pump

/// Drain background events of every session; true if anything changed. A session that stops
/// comes forward (as RustRover selects the session that hit a breakpoint), and when a selected
/// subprocess session ends its parent comes back.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = ensure_loaded(s);
    changed |= pump_attach(s);
    changed |= crate::asm::pump(s);
    changed |= follow_edits(s);
    let before = s.debug.session.as_ref().map(|x| (x.id, x.status().is_finished()));
    changed |= pump_selected(s);
    let ids: Vec<u64> = s.debug.parked.iter().filter(|p| p.live()).map(Parked::id).collect();
    for id in ids {
        let previous = s.debug.selected_id();
        if !s.debug.bring_forward(id) {
            continue;
        }
        let order = s.debug.session.as_ref().map_or(0, |x| x.stop_order);
        changed |= pump_selected(s);
        let stopped =
            s.debug.session.as_ref().is_some_and(|x| x.id == id && x.stop_order != order && x.is_stopped());
        if !stopped
            && s.debug.selected_id() == Some(id)
            && let Some(previous) = previous
        {
            s.debug.bring_forward(previous);
        }
    }
    changed |= back_to_parent(s, before);
    // Sessions closed while running go once their adapter is gone.
    let closed: Vec<u64> = s
        .debug
        .closing
        .iter()
        .copied()
        .filter(|id| s.debug.find(*id).is_none_or(|(_, rt)| rt.is_none()))
        .collect();
    for id in closed {
        remove_session(s, id);
        changed = true;
    }
    if changed {
        refresh_processes(&mut s.debug);
    }
    changed
}

/// The selected subprocess session just ended: select its parent again (when it still runs).
fn back_to_parent(s: &mut Session, before: Option<(u64, bool)>) -> bool {
    let Some((id, false)) = before else { return false };
    let Some(session) = s.debug.session.as_ref().filter(|x| x.id == id && x.status().is_finished()) else {
        return false;
    };
    let Some(parent) = session.parent else { return false };
    let label = session.name.clone();
    if !s.debug.find(parent).is_some_and(|(_, rt)| rt.is_some()) || !s.debug.bring_forward(parent) {
        return false;
    }
    let Some(session) = s.debug.session.as_ref() else { return true };
    let execution = session.execution.clone();
    s.status = format!("{label} exited; showing {}", session.name);
    s.debug.info(format!("{label} exited"));
    if let Some((path, line)) = execution {
        reveal_line(s, &path, line);
    }
    true
}

/// Drain the selected session's events.
fn pump_selected(s: &mut Session) -> bool {
    // A session on a target: its debug server's output (M16 RT).
    let launcher = on_target::pump(s);
    pump_phase(s) || launcher
}

fn pump_phase(s: &mut Session) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    match &mut rt.phase {
        Phase::Build { events, .. } => {
            let batch: Vec<RunEvent> = events.try_iter().take(MAX_EVENTS_PER_PUMP).collect();
            if batch.is_empty() {
                return false;
            }
            for event in batch {
                on_build_event(s, event);
            }
            true
        }
        Phase::Prepare { rx } => match rx.try_recv() {
            Ok(Ok(prepared)) => {
                on_prepared(s, prepared);
                true
            }
            Ok(Err(message)) => {
                fail(s, &message);
                true
            }
            Err(crossbeam_channel::TryRecvError::Empty) => false,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                fail(s, "the debugger did not start");
                true
            }
        },
        Phase::Live { events, .. } => {
            let batch: Vec<DapEvent> = events.try_iter().take(MAX_EVENTS_PER_PUMP).collect();
            let has_children = !rt.children.is_empty();
            let mut changed = !batch.is_empty();
            for event in batch {
                on_dap_event(s, event);
            }
            if has_children {
                changed |= children::pump(s);
            }
            changed
        }
    }
}

fn fail(s: &mut Session, message: &str) {
    s.debug.error(message);
    if let Some(rt) = s.debug.rt.take() {
        let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
        let exited = rt.exited;
        rt.kill(pid, exited);
    }
    end_session(s, DebugStatus::Failed { message: message.to_owned() }, "Debugging failed");
    s.notify(NotificationLevel::Error, "Debugger", message.to_owned());
    s.debug.tab = DebugTab::Console;
    s.ws.layout.show(ToolWindowId::Debug);
}

fn on_build_event(s: &mut Session, event: RunEvent) {
    // A session on a target: its preflight, remote build and launcher (M16 RT).
    if s.debug.rt.as_ref().is_some_and(|rt| rt.on_target.is_some()) {
        on_target::on_build_event(s, event);
        return;
    }
    on_local_build_event(s, event);
}

fn on_local_build_event(s: &mut Session, event: RunEvent) {
    match event {
        RunEvent::Started { .. } | RunEvent::Test(_) => {}
        RunEvent::Stdout(line) => s.debug.push_console(DebugConsoleKind::Info, line),
        RunEvent::Stderr(line) => s.debug.push_console(DebugConsoleKind::Info, line),
        RunEvent::Error(e) => s.debug.error(e),
        RunEvent::Message(CargoMessage::CompilerArtifact { target_name, executable: Some(exe), .. }) => {
            if let Some(Runtime { phase: Phase::Build { executables, .. }, .. }) = s.debug.rt.as_mut() {
                executables.push((target_name, exe));
            }
        }
        RunEvent::Message(CargoMessage::CompilerMessage { diagnostic, .. }) => {
            let root = s.debug.rt.as_ref().and_then(|rt| rt.plan.build.as_ref().map(|b| b.cwd.clone()));
            if let Some(root) = root
                && let Some(problem) = crate::runs::problem_from_diagnostic(&root, &diagnostic)
            {
                s.ws.add_cargo_problem(problem);
            }
        }
        RunEvent::Message(_) => {}
        RunEvent::Exited { success, code, .. } => {
            let Some(rt) = s.debug.rt.as_mut() else { return };
            if rt.stopping {
                return;
            }
            if !success {
                let code = code.map_or("a signal".to_string(), |c| c.to_string());
                fail(s, &format!("the build failed (exit code {code})"));
                return;
            }
            let Phase::Build { executables, .. } = &rt.phase else { return };
            let Some(build) = &rt.plan.build else { return };
            match pick_artifact(&build.pick, executables) {
                Ok(program) => {
                    let plan = rt.plan.clone();
                    let options = rt.options.clone();
                    rt.phase = Phase::Prepare { rx: spawn_prepare(plan, Some(program.clone()), options) };
                    s.debug.info(format!("Debugging {}", program.display()));
                    set_status(s, DebugStatus::Starting);
                }
                Err(message) => fail(s, &message),
            }
        }
    }
}

fn on_prepared(s: &mut Session, prepared: Prepared) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let Prepared { client, events, adapter, kind, launch } = prepared;
    rt.kind = kind;
    rt.phase = Phase::Live { client, events };
    // The launch arguments wait for the `initialize` answer.
    rt.launch_args = Some(launch);
    rt.request(Pending::Initialize, |c| c.initialize(adapter_id(kind)));
    if let Some(session) = &mut s.debug.session {
        session.adapter = adapter;
    }
}

// ---------------------------------------------------------------- DAP events

fn on_dap_event(s: &mut Session, event: DapEvent) {
    match event {
        DapEvent::Response(response) => on_response(s, response),
        DapEvent::Event { event, body } => on_event(s, &event, &body),
        DapEvent::Request { seq, command, arguments } if command == "startDebugging" => {
            // debugpy's subprocesses (js-debug's child sessions are refused there).
            children::start_debugging(s, seq, &arguments);
        }
        DapEvent::Request { seq, command, .. } => {
            if let Some(client) = s.debug.rt.as_ref().and_then(Runtime::client) {
                let _ = client.respond(seq, &command, Err(format!("CodeCraft does not support {command} yet")));
            }
            s.debug.error(format!("The debug adapter asked for {command}, which CodeCraft does not support yet."));
        }
        DapEvent::Error(e) => {
            if !s.debug.rt.as_ref().is_some_and(|rt| rt.stopping) {
                s.debug.error(e);
            }
        }
        DapEvent::Exited { .. } => on_adapter_exit(s),
    }
}

fn on_adapter_exit(s: &mut Session) {
    let Some(rt) = s.debug.rt.take() else { return };
    let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
    // A subprocess's connection closes when the subprocess ends: it is finishing (never killed
    // unless the user stopped it).
    let child_ended = rt.child_of.is_some() && !rt.stopping;
    let (stopping, exited) = (rt.stopping || child_ended, rt.exited || child_ended);
    rt.kill(pid, exited);
    if stopping {
        end_session(s, DebugStatus::Exited { code: None }, "Debugging finished");
    } else {
        s.debug.error("The debug adapter exited unexpectedly.");
        end_session(s, DebugStatus::Failed { message: "the debug adapter exited".into() }, "Debugging failed");
    }
}

fn on_response(s: &mut Session, response: dap::Response) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let Some(pending) = rt.pending.remove(&response.id) else { return };
    let epoch_now = rt.epoch;
    match pending {
        Pending::Initialize => match response.result {
            Ok(body) => {
                rt.capabilities = dap::types::parse(&body).unwrap_or_default();
                let args = rt.launch_args.take().unwrap_or(Value::Null);
                let attach = args.get("request").and_then(Value::as_str) == Some("attach");
                if !rt.request(Pending::Launch, |c| if attach { c.attach(args) } else { c.launch(args) }) {
                    fail(s, "the debug adapter stopped answering");
                }
            }
            Err(e) => fail(s, &format!("the debug adapter failed to initialize: {e}")),
        },
        Pending::Launch => match response.result {
            Ok(_) => {
                if rt.configured {
                    started(s);
                }
            }
            Err(e) if rt.attached() => fail(s, &format!("could not attach: {e}")),
            Err(e) => fail(s, &format!("could not start the program: {e}")),
        },
        Pending::ConfigurationDone => match response.result {
            Ok(_) => started(s),
            Err(e) => s.debug.error(format!("configurationDone failed: {e}")),
        },
        Pending::SetBreakpoints { ids } => {
            let body: dap::BreakpointsBody = match response.result {
                Ok(body) => dap::types::parse(&body).unwrap_or_default(),
                Err(e) => {
                    s.debug.error(format!("Could not set breakpoints: {e}"));
                    return;
                }
            };
            // With several sessions a breakpoint is set when any of them set it.
            let others = s.debug.parked.iter().any(Parked::live);
            for (theirs, ours) in body.breakpoints.iter().zip(ids) {
                let Some(ours) = ours else { continue };
                if let Some(id) = theirs.id {
                    rt.adapter_ids.insert(id, ours);
                }
                if let Some(bp) = s.debug.breakpoints.iter_mut().find(|b| b.id == ours) {
                    if others && !theirs.verified && bp.verified == Some(true) {
                        continue;
                    }
                    bp.verified = Some(theirs.verified);
                    bp.message = theirs.message.clone();
                }
            }
        }
        Pending::Setup | Pending::Other => {
            if let Err(e) = response.result {
                s.debug.error(format!("{}: {e}", response.command));
            }
        }
        Pending::Threads => {
            let Ok(body) = response.result else { return };
            let body: dap::ThreadsBody = dap::types::parse(&body).unwrap_or_default();
            let Some(session) = s.debug.session.as_mut() else { return };
            if !session.is_stopped() {
                return;
            }
            session.threads = body.threads.into_iter().map(|t| ThreadInfo { id: t.id, name: t.name }).collect();
            if session.thread.is_none()
                && let Some(first) = session.threads.first().map(|t| t.id)
            {
                session.thread = Some(first);
                let epoch = rt.epoch;
                rt.request(Pending::StackTrace { epoch, reveal: true }, |c| c.stack_trace(first, 0, MAX_FRAMES));
            }
        }
        Pending::StackTrace { epoch, reveal } => {
            if epoch != epoch_now {
                return;
            }
            match response.result {
                Ok(body) => on_stack_trace(s, &body, reveal),
                Err(e) => s.debug.error(format!("Could not read the stack: {e}")),
            }
        }
        Pending::Scopes { epoch } => {
            if epoch != epoch_now {
                return;
            }
            let Ok(body) = response.result else { return };
            let body: dap::ScopesBody = dap::types::parse(&body).unwrap_or_default();
            let scopes: Vec<ScopeInfo> = body
                .scopes
                .into_iter()
                .map(|x| ScopeInfo {
                    name: x.name,
                    reference: x.variables_reference,
                    expensive: x.expensive,
                    presentation_hint: x.presentation_hint,
                })
                .collect();
            if let Some(session) = s.debug.session.as_mut() {
                session.scopes = scopes.clone();
            }
            crate::debug_views::on_scopes(s);
            for (i, scope) in scopes.iter().enumerate() {
                let path = scope_path(&scope.name);
                if (i == 0 && !scope.expensive) || s.debug.expanded.contains(&path) {
                    request_variables(s, scope.reference, path);
                }
            }
        }
        Pending::Variables { epoch, reference } => {
            if epoch != epoch_now {
                return;
            }
            let rust = rt.is_rust();
            let python = rt.kind == AdapterKind::Debugpy;
            let lldb = matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb);
            let Some(session) = s.debug.session.as_mut() else { return };
            session.loading.remove(&reference);
            let vars: Vec<VariableInfo> = match response.result {
                Ok(body) => dap::types::parse::<dap::VariablesBody>(&body)
                    .unwrap_or_default()
                    .variables
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(|v| {
                        let type_name = v.type_.filter(|t| !t.is_empty());
                        VariableInfo {
                            name: v.name,
                            value: if rust {
                                rust_values::scalar(type_name.as_deref(), rust_value(v.value))
                            } else {
                                v.value
                            },
                            type_name,
                            reference: v.variables_reference,
                            evaluate_name: v.evaluate_name,
                            memory_reference: v.memory_reference,
                        }
                    })
                    .collect(),
                Err(e) => vec![VariableInfo {
                    name: "<error>".into(),
                    value: e,
                    type_name: None,
                    reference: 0,
                    evaluate_name: None,
                    memory_reference: None,
                }],
            };
            session.variables.insert(reference, vars.clone());
            if rust {
                // Enums, arrays and boxes LLDB leaves as `T @ 0x…` get readable summaries.
                rust_values::summarize(s, reference, &vars);
            } else if python {
                // DataFrames, Series, arrays and plain objects.
                python_values::summarize(s, reference, &vars);
            } else if lldb {
                // C/C++ references read as the value they refer to (QA #23).
                cpp_values::summarize(s, reference, &vars);
            }
            let Some(session) = s.debug.session.as_mut() else { return };
            let Some(rt) = s.debug.rt.as_ref() else { return };
            let is_locals = session.scopes.first().is_some_and(|x| x.reference == reference);
            let parent = rt.ref_paths.get(&reference).cloned().unwrap_or_default();
            for v in vars.iter().filter(|v| v.reference > 0) {
                let path = child_path(&parent, &v.name);
                if s.debug.expanded.contains(&path) {
                    request_variables(s, v.reference, path);
                }
            }
            if is_locals {
                actions::pick_return_value(s, &vars);
                compute_inline_values(s, &vars);
            }
        }
        Pending::Evaluate { epoch, purpose } => on_evaluate(s, epoch == epoch_now, purpose, response.result),
        Pending::ResetFrame => {
            let result = response.result.and_then(|body| {
                let b = dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default();
                match dap::lldb::error_of(&b.result) {
                    Some(e) => Err(e),
                    None => Ok(b.result),
                }
            });
            match result {
                Ok(text) => {
                    let text = text.trim();
                    if !text.is_empty() {
                        s.debug.push_console(DebugConsoleKind::Result, text.to_owned());
                    }
                    s.status = "Frame reset".into();
                }
                Err(e) => s.debug.error(format!("Reset Frame: {e}")),
            }
            refresh_stack(s);
        }
        Pending::View { epoch, request } => {
            crate::debug_views::on_response(s, epoch == epoch_now, request, response.result);
        }
        Pending::Action { epoch, request } => actions::on_response(s, epoch == epoch_now, request, response.result),
        Pending::PanicBreakpoint { fallback } => exceptions::on_panic_breakpoint(s, fallback, response.result),
        Pending::Resume | Pending::ResumeCommand => {
            // A console command answers even when it fails; its output says so.
            let result = match (response.command == "evaluate", response.result) {
                (true, Ok(body)) => match dap::types::parse::<dap::EvaluateBody>(&body)
                    .ok()
                    .and_then(|b| dap::lldb::error_of(&b.result))
                {
                    Some(e) => Err(e),
                    None => Ok(body),
                },
                (_, result) => result,
            };
            if let Err(e) = result {
                s.debug.last_error = Some(e.clone());
                s.debug.error(e);
                // Still where we were: show it again.
                let Some(rt) = s.debug.rt.as_mut() else { return };
                let Some(previous) = rt.last_stop.take() else { return };
                rt.epoch += 1;
                let epoch = rt.epoch;
                let thread = s.debug.session.as_ref().and_then(|x| x.thread);
                if let Some(session) = s.debug.session.as_mut() {
                    session.status = Some(previous);
                }
                if let Some(thread) = thread {
                    rt.request(Pending::StackTrace { epoch, reveal: false }, |c| c.stack_trace(thread, 0, MAX_FRAMES));
                }
            }
        }
    }
}

/// `launch` and `configurationDone` both answered: the program runs.
fn started(s: &mut Session) {
    if let Some(session) = &mut s.debug.session
        && matches!(session.status, Some(DebugStatus::Starting))
    {
        session.status = Some(DebugStatus::Running);
    }
}

fn on_event(s: &mut Session, event: &str, body: &Value) {
    match event {
        "initialized" => configure(s),
        "stopped" => on_stopped(s, dap::types::parse(body).unwrap_or_default()),
        "continued" => {
            let Some(rt) = s.debug.rt.as_mut() else { return };
            if s.debug.session.as_ref().is_some_and(DebugSession::is_stopped) {
                rt.epoch += 1;
                if let Some(session) = &mut s.debug.session {
                    session.status = Some(DebugStatus::Running);
                    session.clear_stop();
                }
            }
        }
        "exited" => {
            let ev: dap::ExitedEvent = dap::types::parse(body).unwrap_or_default();
            if let Some(rt) = s.debug.rt.as_mut() {
                rt.exited = true;
            }
            s.debug.info(format!("\nProcess finished with exit code {}", ev.exit_code));
            if let Some(session) = &mut s.debug.session {
                session.clear_stop();
                session.status = Some(DebugStatus::Exited { code: Some(ev.exit_code) });
            }
            reset_verification(s);
            s.status = format!("Debugging finished (exit code {})", ev.exit_code);
        }
        "terminated" => {
            if let Some(rt) = s.debug.rt.as_mut() {
                // A subprocess that ends on its own is finishing: let it (its parent waits
                // for its exit code), only detach from it.
                let child_ended = rt.child_of.is_some() && !rt.stopping;
                rt.stopping = true;
                if child_ended {
                    rt.exited = true;
                    rt.children.end_all(TERMINATE_GRACE);
                    if let Some(client) = rt.client() {
                        client.detach(Duration::from_millis(500));
                    }
                } else {
                    rt.end(TERMINATE_GRACE);
                }
            }
            end_session(s, DebugStatus::Exited { code: None }, "Debugging finished");
        }
        "output" => {
            let ev: dap::OutputEvent = dap::types::parse(body).unwrap_or_default();
            let kind = match ev.category.as_deref() {
                Some("stdout") => DebugConsoleKind::Stdout,
                Some("stderr") => DebugConsoleKind::Stderr,
                Some("telemetry") => return,
                Some("important") => DebugConsoleKind::Error,
                _ => DebugConsoleKind::Info,
            };
            // debugpy injects itself into a process with LLDB/GDB: when the OS refuses, say so now
            // rather than after the adapter's long connection timeout.
            let injecting = s.debug.rt.as_ref().is_some_and(|rt| {
                matches!(rt.plan.run, RunSpec::Attach { process: ProcessKind::Python, .. }) && !rt.configured
            });
            let refused = injecting && ev.output.contains("attach failed");
            let message = ev.output.lines().find(|l| l.contains("attach failed")).map(|l| l.trim().to_owned());
            // Xcode's LLDB warns that it has no Rust plugin in every Rust session; the toolchain's
            // formatters read the values anyway, so the warning only confuses (QA #22).
            let output = if s.debug.rt.as_ref().is_some_and(Runtime::is_rust) {
                without_rust_plugin_warning(&ev.output)
            } else {
                ev.output
            };
            if output.is_empty() {
                return;
            }
            s.debug.push_console(kind, output);
            if refused {
                let message = message.unwrap_or_else(|| "attach failed".into());
                fail(s, &format!("could not attach: {}", message.trim_start_matches("error: ")));
            }
        }
        "breakpoint" => {
            let ev: dap::BreakpointEvent = dap::types::parse(body).unwrap_or_default();
            let Some(rt) = s.debug.rt.as_ref() else { return };
            let Some(ours) = ev.breakpoint.id.and_then(|id| rt.adapter_ids.get(&id)).copied() else { return };
            if let Some(bp) = s.debug.breakpoints.iter_mut().find(|b| b.id == ours) {
                match ev.reason.as_str() {
                    "removed" => bp.verified = None,
                    // lldb-dap briefly unresolves every breakpoint when the process relocates at
                    // launch and does not always resolve them again: only trust a downgrade that
                    // says why.
                    _ if !ev.breakpoint.verified && ev.breakpoint.message.is_none() && bp.verified == Some(true) => {}
                    _ => {
                        bp.verified = Some(ev.breakpoint.verified);
                        bp.message = ev.breakpoint.message;
                    }
                }
            }
        }
        "thread" => {
            let Some(rt) = s.debug.rt.as_mut() else { return };
            if s.debug.session.as_ref().is_some_and(DebugSession::is_stopped) {
                rt.request(Pending::Threads, DapClient::threads);
            }
        }
        "process" => {
            let ev: dap::ProcessEvent = dap::types::parse(body).unwrap_or_default();
            // An attached process is not ours to kill when the session ends.
            if s.debug.rt.as_ref().is_some_and(Runtime::attached) {
                return;
            }
            if let (Some(session), Some(pid)) =
                (&mut s.debug.session, ev.system_process_id.and_then(|p| u32::try_from(p).ok()))
            {
                session.debuggee_pid = Some(pid);
            }
        }
        _ => {}
    }
}

/// `initialized`: send breakpoints and the rest of the configuration, then `configurationDone`.
fn configure(s: &mut Session) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    if rt.configured {
        return;
    }
    rt.configured = true;
    let paths: BTreeSet<PathBuf> = s.debug.breakpoints.iter().filter(|b| b.enabled).map(|b| b.path.clone()).collect();
    for path in paths {
        send_breakpoints_here(s, &path);
    }
    let break_on_panic = s.debug.break_on_panic;
    let settings = s.debug.exceptions.clone();
    let Some(rt) = s.debug.rt.as_mut() else { return };
    // Rust panics, C++ throw/catch, Python raised/uncaught/user-uncaught (View Breakpoints).
    exceptions::send(rt, &settings, break_on_panic);
    if rt.capabilities.supports_configuration_done_request {
        rt.request(Pending::ConfigurationDone, DapClient::configuration_done);
    } else {
        started(s);
    }
}

/// One file's enabled breakpoints as DAP wants them, with our ids (`None` for a Run to Cursor
/// location at `cursor`).
fn breakpoint_list(
    dbg: &DebugState,
    path: &Path,
    cursor: Option<&(PathBuf, usize)>,
) -> (Vec<dap::SourceBreakpoint>, Vec<Option<u64>>) {
    let mut list = Vec::new();
    let mut ids = Vec::new();
    for bp in dbg.breakpoints.iter().filter(|b| b.path == path && b.enabled) {
        list.push(dap::SourceBreakpoint {
            line: line_to_dap(bp.line),
            column: None,
            condition: bp.condition.clone().filter(|c| !c.trim().is_empty()),
            hit_condition: bp.hit_condition.clone().filter(|c| !c.trim().is_empty()),
            log_message: bp.log_message.clone().filter(|c| !c.is_empty()),
        });
        ids.push(Some(bp.id));
    }
    if let Some((cursor_path, line)) = cursor
        && cursor_path == path
        && !list.iter().any(|b| b.line == line_to_dap(*line))
    {
        list.push(dap::SourceBreakpoint { line: line_to_dap(*line), ..Default::default() });
        ids.push(None);
    }
    (list, ids)
}

/// What kind of debugger a source file's breakpoints are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    Native,
    Python,
    Js,
    /// Anything else (sent to every session).
    Any,
}

fn source_kind(path: &Path) -> SourceKind {
    match Language::from_path(path) {
        Language::Rust | Language::C | Language::Cpp | Language::Zig => SourceKind::Native,
        Language::Python | Language::DjangoTemplate => SourceKind::Python,
        Language::JavaScript | Language::TypeScript => SourceKind::Js,
        _ => SourceKind::Any,
    }
}

impl Runtime {
    /// The breakpoints of `path` go to this session's adapter (Python breakpoints to debugpy,
    /// Rust/C/C++/Zig ones to LLDB, JavaScript/TypeScript ones to js-debug).
    fn accepts(&self, path: &Path) -> bool {
        let mine = match &self.plan.run {
            RunSpec::Python { .. } | RunSpec::PythonRemote { .. } | RunSpec::Attach { process: ProcessKind::Python, .. } => {
                SourceKind::Python
            }
            RunSpec::Node { .. } => SourceKind::Js,
            _ => SourceKind::Native,
        };
        let theirs = source_kind(path);
        theirs == SourceKind::Any || theirs == mine
    }
}

/// Send one file's breakpoints to every session of the matching language (breakpoints are
/// shared by all sessions).
fn send_breakpoints(s: &mut Session, path: &Path) {
    let path = path.to_path_buf();
    for_each_live(s, |s| send_breakpoints_here(s, &path));
}

/// Send one file's breakpoints (and a pending Run to Cursor location) to the selected session's
/// adapter.
fn send_breakpoints_here(s: &mut Session, path: &Path) {
    let cursor = s.debug.rt.as_ref().and_then(|rt| rt.run_to_cursor.clone());
    let (list, ids) = breakpoint_list(&s.debug, path, cursor.as_ref());
    let Some(rt) = s.debug.rt.as_mut() else { return };
    if !rt.configured || !rt.accepts(path) {
        return;
    }
    let adapter_path = rt.to_adapter(path);
    if list.is_empty() {
        if !rt.sent.remove(path) {
            return;
        }
    } else {
        rt.sent.insert(path.to_path_buf());
    }
    rt.request(Pending::SetBreakpoints { ids }, |c| c.set_breakpoints(&adapter_path, &list));
}

fn line_to_dap(line: usize) -> i64 {
    i64::try_from(line).unwrap_or(i64::MAX - 1).saturating_add(1)
}

fn line_from_dap(line: i64) -> usize {
    usize::try_from(line.saturating_sub(1)).unwrap_or(0)
}

fn on_stopped(s: &mut Session, mut ev: dap::StoppedEvent) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    // lldb-dap reports the panic function breakpoint as a plain breakpoint.
    if ev.hit_breakpoint_ids.iter().any(|id| rt.panic_breakpoints.contains(id)) {
        ev.reason = "panic".into();
        ev.description = Some("Rust panic".into());
    }
    rt.epoch += 1;
    rt.stops += 1;
    rt.last_stop = None;
    let first = rt.stops == 1;
    let epoch = rt.epoch;
    let cursor = rt.run_to_cursor.take();
    let last_step = rt.last_step.take();
    // Temporary breakpoints go away once hit.
    let hit: Vec<u64> = ev.hit_breakpoint_ids.iter().filter_map(|id| rt.adapter_ids.get(id)).copied().collect();
    s.debug.stop_counter = s.debug.stop_counter.saturating_add(1);
    let order = s.debug.stop_counter;
    let Some(session) = s.debug.session.as_mut() else { return };
    session.stop_order = order;
    session.clear_stop();
    session.hit_breakpoints = hit.clone();
    if let Some(thread) = ev.thread_id {
        session.thread = Some(thread);
    }
    session.status = Some(DebugStatus::Stopped {
        reason: ev.reason.clone(),
        description: ev.description.clone(),
        thread: ev.thread_id,
    });
    let thread = session.thread;
    for w in &mut s.debug.watches {
        w.clear();
    }
    s.debug.views.on_stop();
    rt.ref_paths.clear();
    rt.request(Pending::Threads, DapClient::threads);
    if let Some(thread) = thread {
        rt.request(Pending::StackTrace { epoch, reveal: true }, |c| c.stack_trace(thread, 0, MAX_FRAMES));
    }
    // A breakpoint that was hit is evidently set, whatever earlier events said.
    for bp in s.debug.breakpoints.iter_mut().filter(|b| hit.contains(&b.id)) {
        bp.verified = Some(true);
        bp.message = None;
    }
    let temporary: Vec<(u64, PathBuf)> = s
        .debug
        .breakpoints
        .iter()
        .filter(|b| b.temporary && hit.contains(&b.id))
        .map(|b| (b.id, b.path.clone()))
        .collect();
    for (id, _) in &temporary {
        remove_breakpoint_by_id(s, *id);
    }
    if let Some((path, _)) = cursor {
        send_breakpoints_here(s, &path);
    }
    s.ws.layout.show(ToolWindowId::Debug);
    if first {
        s.debug.tab = DebugTab::Variables;
    }
    s.status = match &ev.description {
        Some(d) if !d.is_empty() => format!("Paused: {d}"),
        _ => format!("Paused ({})", ev.reason),
    };
    s.debug.step_targets = None;
    s.debug.hover = None;
    crate::debug_views::after_stop(s);
    actions::after_stop(s, &ev.reason, ev.thread_id, last_step);
    refresh_processes(&mut s.debug);
}

fn on_stack_trace(s: &mut Session, body: &Value, reveal: bool) {
    let Some(rt) = s.debug.rt.as_ref() else { return };
    let body: dap::StackTraceBody = dap::types::parse(body).unwrap_or_default();
    let frames: Vec<FrameInfo> = body
        .stack_frames
        .into_iter()
        .map(|f| {
            let path = f.source.as_ref().and_then(|x| x.path.as_deref()).map(|p| rt.map_from_adapter(p));
            let deemphasized = f.source.as_ref().and_then(|x| x.presentation_hint.as_deref()) == Some("deemphasize");
            let subtle = deemphasized
                || matches!(f.presentation_hint.as_deref(), Some("subtle" | "label"))
                || !path.as_ref().is_some_and(|p| p.is_file());
            FrameInfo {
                id: f.id,
                name: f.name,
                path,
                line: line_from_dap(f.line),
                column: line_from_dap(f.column),
                subtle,
                instruction_pointer: f.instruction_pointer_reference,
            }
        })
        .collect();
    let exists = |f: &FrameInfo| f.path.as_ref().is_some_and(|p| p.is_file());
    let reason = match s.debug.session.as_ref().map(DebugSession::status) {
        Some(DebugStatus::Stopped { reason, .. }) => reason,
        _ => String::new(),
    };
    let root = rt.project_root.clone();
    let in_project = |f: &FrameInfo| match (&root, &f.path) {
        (Some(root), Some(p)) => p.starts_with(root) && p.is_file(),
        _ => false,
    };
    // A panic or exception stops deep in the runtime: show the user's code that caused it.
    let index = if matches!(reason.as_str(), "exception" | "function breakpoint" | "panic") {
        frames.iter().position(in_project).or_else(|| frames.iter().position(exists))
    } else if frames.first().is_some_and(exists) {
        Some(0)
    } else {
        frames.iter().position(exists)
    }
    .unwrap_or(0);
    let temporary_hit = frames.first().and_then(|top| {
        s.debug
            .breakpoints
            .iter()
            .find(|b| b.temporary && Some(&b.path) == top.path.as_ref() && b.line == top.line && reason == "breakpoint")
            .map(|b| b.id)
    });
    if let Some(session) = s.debug.session.as_mut() {
        session.frames = frames;
    }
    if let Some(id) = temporary_hit {
        remove_breakpoint_by_id(s, id);
    }
    select_frame(s, index, reveal);
}

/// Make frame `index` current: its scopes, watches and (with `reveal`) its source in the editor.
fn select_frame(s: &mut Session, index: usize, reveal: bool) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let Some(session) = s.debug.session.as_mut() else { return };
    let Some(frame) = session.frames.get(index).cloned() else { return };
    rt.epoch += 1;
    let epoch = rt.epoch;
    rt.ref_paths.clear();
    session.frame = index;
    session.scopes.clear();
    session.variables.clear();
    session.loading.clear();
    session.inline_values.clear();
    session.execution = frame.path.clone().filter(|p| p.is_file()).map(|p| (p, frame.line));
    rt.request(Pending::Scopes { epoch }, |c| c.scopes(frame.id));
    let watches: Vec<String> = s.debug.watches.iter().map(|w| w.expression.clone()).collect();
    for w in &mut s.debug.watches {
        w.clear();
    }
    for expression in watches {
        let purpose = EvalPurpose::Watch(expression.clone());
        rt.request(Pending::Evaluate { epoch, purpose }, |c| c.evaluate(&expression, Some(frame.id), "watch"));
    }
    let execution = session.execution.clone();
    crate::debug_views::on_frame_selected(s);
    if reveal && let Some((path, line)) = execution {
        reveal_line(s, &path, line);
    }
}

/// Open `path` and put the caret at the start of `line` (no navigation history entry).
fn reveal_line(s: &mut Session, path: &Path, line: usize) {
    let Ok(editor) = s.open_file(path) else { return };
    let offset =
        s.ws.editor(editor)
            .and_then(|e| s.ws.doc(e.doc))
            .map(|d| d.doc.position_to_offset(codecraft_text::Position::new(line, 0)))
            .unwrap_or(0);
    s.ws.set_selections(editor, SelectionSet::caret(offset));
    if let Some(e) = s.ws.editor_mut(editor) {
        e.reveal_caret = true;
    }
}

/// Tree path separator (never part of a variable name).
const SEP: char = '\u{1f}';

/// The tree path of a scope node.
pub fn scope_path(name: &str) -> String {
    format!("s:{name}")
}

/// The tree path of a watch's value node.
pub fn watch_path(expression: &str) -> String {
    format!("w:{expression}")
}

/// The tree path of the Evaluate Expression result.
pub const EVALUATION_PATH: &str = "e:";

pub fn child_path(parent: &str, name: &str) -> String {
    format!("{parent}{SEP}{name}")
}

fn request_variables(s: &mut Session, reference: i64, path: String) {
    if reference <= 0 {
        return;
    }
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let Some(session) = s.debug.session.as_mut() else { return };
    if session.variables.contains_key(&reference) || session.loading.contains(&reference) {
        return;
    }
    rt.ref_paths.insert(reference, path);
    let epoch = rt.epoch;
    if rt.request(Pending::Variables { epoch, reference }, |c| c.variables(reference)) {
        session.loading.insert(reference);
    }
}

fn on_evaluate(s: &mut Session, current: bool, purpose: EvalPurpose, result: Result<Value, String>) {
    let rust = s.debug.rt.as_ref().is_some_and(Runtime::is_rust);
    let parsed = result.map(|body| {
        let mut body = dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default();
        if rust {
            body.result = rust_values::scalar(body.type_.as_deref(), rust_value(std::mem::take(&mut body.result)));
        }
        body
    });
    match purpose {
        EvalPurpose::Watch(expression) => {
            if !current {
                return;
            }
            let mut fetch = None;
            for w in s.debug.watches.iter_mut().filter(|w| w.expression == expression) {
                match &parsed {
                    Ok(body) => {
                        w.value = Some(body.result.clone());
                        w.type_name = body.type_.clone().filter(|t| !t.is_empty());
                        w.reference = body.variables_reference;
                        w.error = None;
                        fetch = Some(body.variables_reference);
                    }
                    Err(e) => {
                        w.error = Some(e.clone());
                        w.value = None;
                    }
                }
            }
            let path = watch_path(&expression);
            if let Some(reference) = fetch
                && s.debug.expanded.contains(&path)
            {
                request_variables(s, reference, path);
            }
        }
        EvalPurpose::Console => match parsed {
            Ok(body) => {
                // Older lldb-dap echoes `(lldb) command` before a command's output.
                let text = dap::lldb::strip_echo(&body.result).trim_end_matches('\n');
                if !text.is_empty() {
                    let kind =
                        if text.starts_with("error:") { DebugConsoleKind::Error } else { DebugConsoleKind::Result };
                    s.debug.push_console(kind, text.to_owned());
                }
            }
            Err(e) => s.debug.push_console(DebugConsoleKind::Error, e),
        },
        EvalPurpose::Hover(expression) => {
            if !current || s.debug.hover.as_ref().is_none_or(|h| h.expression != expression) {
                return;
            }
            let result = parsed.map(|body| VariableInfo {
                name: expression.clone(),
                value: body.result,
                type_name: body.type_.filter(|t| !t.is_empty()),
                reference: body.variables_reference,
                evaluate_name: Some(expression.clone()),
                memory_reference: body.memory_reference,
            });
            s.debug.hover = Some(Evaluation { expression, result, pending: false });
        }
        EvalPurpose::Agent(token) => match parsed {
            Ok(body) => {
                let value = VariableInfo {
                    name: String::new(),
                    value: dap::lldb::strip_echo(&body.result).trim_end_matches('\n').to_owned(),
                    type_name: body.type_.filter(|t| !t.is_empty()),
                    reference: body.variables_reference,
                    evaluate_name: None,
                    memory_reference: body.memory_reference,
                };
                let mut json = agent::variable_json(&value);
                if let Some(map) = json.as_object_mut() {
                    map.remove("name");
                }
                if value.reference > 0 && current {
                    // One level of children in the same answer.
                    let reference = value.reference;
                    let request = actions::ActionRequest::AgentChildren { token, value: json.clone() };
                    let sent = s.debug.rt.as_mut().is_some_and(|rt| {
                        let epoch = rt.epoch;
                        rt.request(Pending::Action { epoch, request }, |c| c.variables(reference))
                    });
                    if sent {
                        return;
                    }
                }
                agent::deliver(s, token, Ok(json));
            }
            Err(e) => agent::deliver(s, token, Err(e)),
        },
        EvalPurpose::Evaluate(expression) => {
            let result = parsed.map(|body| VariableInfo {
                name: expression.clone(),
                value: body.result,
                type_name: body.type_.filter(|t| !t.is_empty()),
                reference: body.variables_reference,
                evaluate_name: Some(expression.clone()),
                memory_reference: body.memory_reference,
            });
            let reference = result.as_ref().map(|v| v.reference).unwrap_or(0);
            s.debug.evaluation = Some(Evaluation { expression, result, pending: false });
            if s.debug.expanded.contains(EVALUATION_PATH) {
                request_variables(s, reference, EVALUATION_PATH.to_owned());
            }
        }
    }
}

// ---------------------------------------------------------------- actions (commands)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Over,
    Into,
    Out,
    Continue,
}

/// The thread to step: the selected one, else the stopped one, else the first.
fn current_thread(s: &Session) -> Option<i64> {
    let session = s.debug.session.as_ref()?;
    session
        .thread
        .or(match &session.status {
            Some(DebugStatus::Stopped { thread, .. }) => *thread,
            _ => None,
        })
        .or_else(|| session.threads.first().map(|t| t.id))
}

/// Step or continue. While the Disassembly tab is shown, Step Over and Step Into go by one
/// instruction (like CLion's disassembly view).
pub(crate) fn step(s: &mut Session, step: Step) -> Result<(), CommandError> {
    let by_instruction = s.debug.tab == DebugTab::Disassembly && matches!(step, Step::Over | Step::Into);
    step_with(s, step, by_instruction)
}

/// Step by one machine instruction (`over` steps over calls).
pub(crate) fn step_instruction(s: &mut Session, over: bool) -> Result<(), CommandError> {
    step_with(s, if over { Step::Over } else { Step::Into }, true)
}

fn step_with(s: &mut Session, step: Step, by_instruction: bool) -> Result<(), CommandError> {
    if !s.debug.is_stopped() {
        return Err(CommandError::Disabled("the program is not paused".into()));
    }
    let thread = current_thread(s).ok_or_else(|| CommandError::Failed("no thread to step".into()))?;
    let frame = s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id);
    let function = s.debug.session.as_ref().and_then(|x| x.frames.first()).map(|f| f.name.clone());
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    let previous = s.debug.session.as_ref().and_then(|x| x.status.clone());
    let by_instruction = by_instruction && matches!(step, Step::Over | Step::Into);
    let lldb = matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb);
    let sent = if by_instruction && rt.capabilities.supports_stepping_granularity {
        let command = if step == Step::Over { "next" } else { "stepIn" };
        rt.request(Pending::Resume, |c| c.step_by(command, thread, "instruction"))
    } else if by_instruction && lldb {
        // Older lldb-dap has no stepping granularity; its console steps by instruction.
        let command = dap::lldb::step_instruction_command(step == Step::Over);
        rt.request(Pending::ResumeCommand, |c| c.evaluate(command, frame, "repl"))
    } else if by_instruction {
        return Err(CommandError::Failed(format!("{} cannot step by instruction", adapter_id(rt.kind))));
    } else {
        rt.request(Pending::Resume, |c| match step {
            Step::Over => c.next(thread),
            Step::Into => c.step_in(thread),
            Step::Out => c.step_out(thread),
            Step::Continue => c.continue_(thread),
        })
    };
    if !sent {
        return Err(CommandError::Failed("the debugger is not responding".into()));
    }
    rt.last_stop = previous;
    rt.last_step = Some(step);
    rt.step_from_function = function;
    rt.epoch += 1;
    if let Some(session) = &mut s.debug.session {
        session.status = Some(DebugStatus::Running);
        session.clear_stop();
    }
    s.debug.views.on_resume();
    s.debug.last_error = None;
    s.debug.step_targets = None;
    s.status = "Running".into();
    Ok(())
}

/// Run › Debugging Actions › Reset Frame: drop the selected frame and return to its caller.
/// Adapters with `restartFrame` restart it; LLDB adapters without it pop it with `thread
/// return` (side effects are not undone). Returns how it was done.
pub(crate) fn reset_frame(s: &mut Session) -> Result<&'static str, CommandError> {
    if !s.debug.is_stopped() {
        return Err(CommandError::Disabled("the program is not paused".into()));
    }
    let (frame, index, count) = {
        let session = s.debug.session.as_ref().ok_or_else(|| CommandError::Disabled("not debugging".into()))?;
        let frame = session.selected_frame().map(|f| f.id).ok_or_else(|| CommandError::Failed("no frame is selected".into()))?;
        (frame, session.frame, session.frames.len())
    };
    if index + 1 >= count {
        return Err(CommandError::Failed("the outermost frame cannot be reset".into()));
    }
    let previous = s.debug.session.as_ref().and_then(|x| x.status.clone());
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    if rt.capabilities.supports_restart_frame {
        if !rt.request(Pending::Resume, |c| c.restart_frame(frame)) {
            return Err(CommandError::Failed("the debugger is not responding".into()));
        }
        rt.last_stop = previous;
        rt.epoch += 1;
        if let Some(session) = &mut s.debug.session {
            session.status = Some(DebugStatus::Running);
            session.clear_stop();
        }
        s.debug.views.on_resume();
        return Ok("restartFrame");
    }
    if matches!(rt.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb) {
        if !rt.request(Pending::ResetFrame, |c| c.evaluate("thread return", Some(frame), "repl")) {
            return Err(CommandError::Failed("the debugger is not responding".into()));
        }
        s.debug.push_console(DebugConsoleKind::Input, "> thread return".to_owned());
        return Ok("thread return");
    }
    Err(CommandError::Failed(format!("{} cannot reset frames (it has no restartFrame request)", adapter_id(rt.kind))))
}

/// Read the threads and the current thread's stack again (the stop is unchanged).
fn refresh_stack(s: &mut Session) {
    let thread = current_thread(s);
    let Some(rt) = s.debug.rt.as_mut() else { return };
    rt.epoch += 1;
    let epoch = rt.epoch;
    rt.ref_paths.clear();
    if let Some(session) = s.debug.session.as_mut() {
        session.clear_stop();
    }
    s.debug.views.on_stop();
    rt.request(Pending::Threads, DapClient::threads);
    if let Some(thread) = thread {
        rt.request(Pending::StackTrace { epoch, reveal: true }, |c| c.stack_trace(thread, 0, MAX_FRAMES));
    }
}

pub(crate) fn pause(s: &mut Session) -> Result<(), CommandError> {
    if !s.debug.is_running() {
        return Err(CommandError::Disabled("the program is not running".into()));
    }
    let thread = current_thread(s).unwrap_or(0);
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    if rt.request(Pending::Other, |c| c.pause(thread)) {
        Ok(())
    } else {
        Err(CommandError::Failed("the debugger is not responding".into()))
    }
}

pub(crate) fn run_to_cursor(s: &mut Session, path: PathBuf, line: usize) -> Result<(), CommandError> {
    if !s.debug.is_stopped() {
        return Err(CommandError::Disabled("the program is not paused".into()));
    }
    if let Some(rt) = s.debug.rt.as_mut() {
        rt.run_to_cursor = Some((path.clone(), line));
    }
    send_breakpoints_here(s, &path);
    step(s, Step::Continue)
}

/// Evaluate in the selected frame: for the console (`repl`) or Evaluate Expression (`watch`).
pub(crate) fn evaluate(s: &mut Session, expression: &str, console: bool) -> Result<(), CommandError> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err(CommandError::BadParams("the expression is empty".into()));
    }
    let frame = s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id);
    let Some(rt) = s.debug.rt.as_mut().filter(|rt| rt.client().is_some()) else {
        return Err(CommandError::Disabled("the debugger is not running".into()));
    };
    let epoch = rt.epoch;
    let (purpose, context) =
        if console { (EvalPurpose::Console, "repl") } else { (EvalPurpose::Evaluate(expression.to_owned()), "watch") };
    if !rt.request(Pending::Evaluate { epoch, purpose }, |c| c.evaluate(expression, frame, context)) {
        return Err(CommandError::Failed("the debugger is not responding".into()));
    }
    if console {
        s.debug.push_console(DebugConsoleKind::Input, format!("> {expression}"));
        s.debug.views.remember_command(expression);
    } else {
        s.debug.evaluation =
            Some(Evaluation { expression: expression.to_owned(), result: Err(String::new()), pending: true });
    }
    Ok(())
}

/// An agent's evaluation in `frame` (`context`: watch, repl, hover or clipboard); the answer
/// goes to `token` with one level of children.
pub(crate) fn evaluate_for_agent(
    s: &mut Session,
    expression: &str,
    frame: Option<i64>,
    context: &str,
    token: u64,
) -> Result<(), CommandError> {
    let Some(rt) = s.debug.rt.as_mut().filter(|rt| rt.client().is_some()) else {
        return Err(CommandError::Disabled("the debugger is not running".into()));
    };
    let epoch = rt.epoch;
    if !rt.request(Pending::Evaluate { epoch, purpose: EvalPurpose::Agent(token) }, |c| c.evaluate(expression, frame, context)) {
        return Err(CommandError::Failed("the debugger is not responding".into()));
    }
    Ok(())
}

pub(crate) fn select_thread(s: &mut Session, id: i64) -> Result<(), CommandError> {
    if !s.debug.is_stopped() {
        return Err(CommandError::Disabled("the program is not paused".into()));
    }
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    let Some(session) = s.debug.session.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    if !session.threads.iter().any(|t| t.id == id) {
        return Err(CommandError::BadParams(format!("no thread {id}")));
    }
    session.thread = Some(id);
    session.clear_stop();
    rt.epoch += 1;
    let epoch = rt.epoch;
    rt.request(Pending::StackTrace { epoch, reveal: true }, |c| c.stack_trace(id, 0, MAX_FRAMES));
    Ok(())
}

pub(crate) fn select_frame_cmd(s: &mut Session, index: usize) -> Result<(), CommandError> {
    let count = s.debug.session.as_ref().map_or(0, |x| x.frames.len());
    if !s.debug.is_stopped() {
        return Err(CommandError::Disabled("the program is not paused".into()));
    }
    if index >= count {
        return Err(CommandError::BadParams(format!("no frame {index}")));
    }
    select_frame(s, index, true);
    Ok(())
}

/// Expand or collapse a node of the Variables tree; expanding fetches its children.
pub(crate) fn set_expanded(s: &mut Session, path: &str, reference: i64, expanded: bool) {
    if expanded {
        s.debug.expanded.insert(path.to_owned());
        request_variables(s, reference, path.to_owned());
    } else {
        s.debug.expanded.remove(path);
    }
}

pub(crate) fn add_watch(s: &mut Session, expression: &str) -> Result<usize, CommandError> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err(CommandError::BadParams("the expression is empty".into()));
    }
    // The project's saved watches first (they are kept with its breakpoints).
    ensure_loaded(s);
    s.debug.watches.push(Watch::new(expression.to_owned()));
    let index = s.debug.watches.len() - 1;
    save(s);
    // Evaluate now if paused.
    let frame = s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id);
    if s.debug.is_stopped()
        && let Some(rt) = s.debug.rt.as_mut()
    {
        let epoch = rt.epoch;
        let purpose = EvalPurpose::Watch(expression.to_owned());
        rt.request(Pending::Evaluate { epoch, purpose }, |c| c.evaluate(expression, frame, "watch"));
    }
    Ok(index)
}

pub(crate) fn remove_watch(s: &mut Session, index: usize) -> Result<(), CommandError> {
    ensure_loaded(s);
    if index >= s.debug.watches.len() {
        return Err(CommandError::BadParams(format!("no watch {index}")));
    }
    s.debug.watches.remove(index);
    save(s);
    Ok(())
}

// ---------------------------------------------------------------- for debug_views

/// The live adapter as the Memory, Disassembly and Registers views need it.
pub(crate) struct LiveAdapter {
    pub kind: AdapterKind,
    pub capabilities: dap::Capabilities,
}

impl LiveAdapter {
    /// lldb-dap or CodeLLDB: LLDB console commands are a fallback for missing requests.
    pub fn is_lldb(&self) -> bool {
        matches!(self.kind, AdapterKind::LldbDap | AdapterKind::CodeLldb)
    }

    pub fn name(&self) -> &'static str {
        adapter_id(self.kind)
    }
}

/// The adapter of a running session (`None` while building or starting, or when it is over).
pub(crate) fn live_adapter(s: &Session) -> Option<LiveAdapter> {
    let rt = s.debug.rt.as_ref()?;
    rt.client()?;
    Some(LiveAdapter { kind: rt.kind, capabilities: rt.capabilities.clone() })
}

/// Tests: a running session talking to `client` (a scripted adapter), already configured.
#[cfg(test)]
pub(crate) fn install_fake(
    s: &mut Session,
    client: DapClient,
    events: Receiver<DapEvent>,
    kind: AdapterKind,
    capabilities: dap::Capabilities,
) {
    install_fake_named(s, client, events, kind, capabilities, "fake");
}

/// [`install_fake`] with a name (a Python session for a debugpy `kind`); returns its id. A
/// running fake stays as another session.
#[cfg(test)]
pub(crate) fn install_fake_named(
    s: &mut Session,
    client: DapClient,
    events: Receiver<DapEvent>,
    kind: AdapterKind,
    capabilities: dap::Capabilities,
    name: &str,
) -> u64 {
    let run = if kind == AdapterKind::Debugpy {
        RunSpec::Python { program: Some("/fake/app.py".into()), module: None, python: None, just_my_code: true }
    } else {
        RunSpec::Native { program: Some("/fake/program".into()), rust: false }
    };
    let plan = LaunchPlan { name: name.into(), build: None, run, args: Vec::new(), cwd: None, env: Vec::new() };
    let mut rt = Runtime::new(plan, Phase::Live { client, events }, None);
    rt.kind = kind;
    rt.capabilities = capabilities;
    rt.configured = true;
    // A second fake runs next to the first (multi-session tests).
    if s.debug.rt.is_some() {
        s.debug.park_selected();
    }
    s.debug.views.reset();
    let id = s.debug.next_session_id();
    s.debug.session = Some(DebugSession {
        id,
        name: name.into(),
        adapter: "fake".into(),
        status: Some(DebugStatus::Running),
        ..DebugSession::default()
    });
    s.debug.rt = Some(rt);
    id
}

/// Send a view request (answered in `debug_views::on_response`). False if the adapter is gone.
pub(crate) fn view_request(
    s: &mut Session,
    request: crate::debug_views::ViewRequest,
    send: impl FnOnce(&DapClient) -> dap::Result<RequestId>,
) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    let epoch = rt.epoch;
    rt.request(Pending::View { epoch, request }, send)
}

/// Read the selected frame's variables again (after memory was written).
pub(crate) fn reload_frame(s: &mut Session) {
    let index = s.debug.session.as_ref().map_or(0, |x| x.frame);
    if s.debug.is_stopped() {
        select_frame(s, index, false);
    }
}

// ---------------------------------------------------------------- breakpoints

/// Properties for [`set_breakpoint`]; `None` leaves a property as it is.
#[derive(Clone, Debug, Default)]
pub(crate) struct BreakpointProps {
    pub enabled: Option<bool>,
    pub condition: Option<Option<String>>,
    pub log_message: Option<Option<String>>,
    pub hit_condition: Option<Option<String>>,
    pub temporary: Option<bool>,
}

fn line_text(s: &Session, path: &Path, line: usize) -> Option<String> {
    match s.ws.doc_by_path(path).and_then(|d| s.ws.doc(d)) {
        Some(d) => d.doc.line_text(line).map(|t| t.trim().to_owned()),
        None => {
            let text = codecraft_fs::read_text_file(path).ok()?;
            text.lines().nth(line).map(|t| t.trim().to_owned())
        }
    }
}

/// Create or update the breakpoint at `path:line`. Returns its id.
pub(crate) fn set_breakpoint(s: &mut Session, path: &Path, line: usize, props: BreakpointProps) -> u64 {
    ensure_loaded(s);
    let path = crate::normalize_path(path);
    let id = match s.debug.breakpoints.iter_mut().find(|b| b.path == path && b.line == line) {
        Some(bp) => {
            if let Some(v) = props.enabled {
                bp.enabled = v;
            }
            if let Some(v) = props.condition {
                bp.condition = v.filter(|c| !c.trim().is_empty());
            }
            if let Some(v) = props.log_message {
                bp.log_message = v.filter(|c| !c.is_empty());
            }
            if let Some(v) = props.hit_condition {
                bp.hit_condition = v.filter(|c| !c.trim().is_empty());
            }
            if let Some(v) = props.temporary {
                bp.temporary = v;
            }
            bp.id
        }
        None => {
            let id = s.debug.next_id;
            s.debug.next_id = s.debug.next_id.saturating_add(1);
            s.debug.breakpoints.push(Breakpoint {
                id,
                path: path.clone(),
                line,
                enabled: props.enabled.unwrap_or(true),
                condition: props.condition.flatten().filter(|c| !c.trim().is_empty()),
                log_message: props.log_message.flatten().filter(|c| !c.is_empty()),
                hit_condition: props.hit_condition.flatten().filter(|c| !c.trim().is_empty()),
                temporary: props.temporary.unwrap_or(false),
                verified: None,
                message: None,
            });
            if let Some(text) = line_text(s, &path, line) {
                s.debug.anchors.insert(id, text);
            }
            id
        }
    };
    after_breakpoints_changed(s, &path);
    id
}

pub(crate) fn remove_breakpoint_by_id(s: &mut Session, id: u64) -> bool {
    let Some(index) = s.debug.breakpoints.iter().position(|b| b.id == id) else { return false };
    let bp = s.debug.breakpoints.remove(index);
    s.debug.anchors.remove(&id);
    after_breakpoints_changed(s, &bp.path);
    true
}

pub(crate) fn remove_all_breakpoints(s: &mut Session) -> usize {
    let paths: BTreeSet<PathBuf> = s.debug.breakpoints.iter().map(|b| b.path.clone()).collect();
    let count = s.debug.breakpoints.len();
    s.debug.breakpoints.clear();
    s.debug.anchors.clear();
    for path in paths {
        send_breakpoints(s, &path);
    }
    save(s);
    count
}

/// Toggle a line breakpoint; returns whether one exists afterwards.
pub(crate) fn toggle_breakpoint(s: &mut Session, path: &Path, line: usize, temporary: bool) -> bool {
    ensure_loaded(s);
    let path = crate::normalize_path(path);
    match s.debug.breakpoint_at(&path, line).map(|b| b.id) {
        Some(id) => {
            remove_breakpoint_by_id(s, id);
            false
        }
        None => {
            set_breakpoint(s, &path, line, BreakpointProps { temporary: Some(temporary), ..Default::default() });
            true
        }
    }
}

fn after_breakpoints_changed(s: &mut Session, path: &Path) {
    send_breakpoints(s, path);
    save(s);
}

/// Follow breakpoints when lines are inserted or deleted above them (by their line's text).
fn follow_edits(s: &mut Session) -> bool {
    if s.debug.breakpoints.is_empty() {
        return false;
    }
    let paths: BTreeSet<PathBuf> = s.debug.breakpoints.iter().map(|b| b.path.clone()).collect();
    let mut moved_paths = Vec::new();
    for path in paths {
        let Some(od) = s.ws.doc_by_path(&path).and_then(|d| s.ws.doc(d)) else { continue };
        let revision = od.doc.revision();
        if s.debug.revisions.get(&path) == Some(&revision) {
            continue;
        }
        s.debug.revisions.insert(path.clone(), revision);
        let doc = &od.doc;
        let lines = doc.len_lines();
        let mut moved = false;
        for bp in s.debug.breakpoints.iter_mut().filter(|b| b.path == path) {
            let current = doc.line_text(bp.line).map(|t| t.trim().to_owned());
            let Some(anchor) = s.debug.anchors.get(&bp.id).cloned() else {
                if let Some(text) = current {
                    s.debug.anchors.insert(bp.id, text);
                }
                continue;
            };
            if current.as_deref() == Some(anchor.as_str()) || anchor.is_empty() {
                continue;
            }
            let found = (1..=MAX_LINE_DRIFT).find_map(|d| {
                [bp.line.checked_add(d), bp.line.checked_sub(d)]
                    .into_iter()
                    .flatten()
                    .find(|&l| l < lines && doc.line_text(l).is_some_and(|t| t.trim() == anchor))
            });
            match found {
                Some(line) => {
                    bp.line = line;
                    moved = true;
                }
                None => {
                    // The line itself was edited: the breakpoint stays and follows the new text.
                    if bp.line >= lines {
                        bp.line = lines.saturating_sub(1);
                        moved = true;
                    }
                    if let Some(text) = doc.line_text(bp.line) {
                        s.debug.anchors.insert(bp.id, text.trim().to_owned());
                    }
                }
            }
        }
        if moved {
            moved_paths.push(path);
        }
    }
    let changed = !moved_paths.is_empty();
    for path in moved_paths {
        send_breakpoints(s, &path);
    }
    if changed {
        save(s);
    }
    changed
}

// ---------------------------------------------------------------- persistence

#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BreakpointsFile {
    version: u32,
    breakpoints: Vec<StoredBreakpoint>,
    watches: Vec<String>,
    break_on_panic: Option<bool>,
    /// C++ and Python exception breakpoints (absent: the defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    exceptions: Option<ExceptionBreakpoints>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredBreakpoint {
    /// Relative to the project root when inside it.
    path: PathBuf,
    /// One-based, as people read it.
    line: usize,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hit_condition: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    temporary: bool,
}

fn breakpoints_file(root: &Path) -> PathBuf {
    root.join(".codecraft").join(BREAKPOINTS_FILE)
}

/// Load the project's breakpoints and watches once per project. True if anything was loaded.
pub(crate) fn ensure_loaded(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if s.debug.loaded_for.as_ref() == Some(&root) {
        return false;
    }
    s.debug.loaded_for = Some(root.clone());
    let Some(root) = root else { return false };
    let file = breakpoints_file(&root);
    let Ok(text) = std::fs::read_to_string(&file) else { return false };
    let stored: BreakpointsFile = match serde_json::from_str(&text) {
        Ok(stored) => stored,
        Err(e) => {
            log::warn!("ignoring corrupt {}: {e}", file.display());
            return false;
        }
    };
    // Breakpoints set before the project was open stay unless the file has the same line.
    s.debug.breakpoints.retain(|b| !b.path.starts_with(&root));
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for b in stored.breakpoints {
        if b.line == 0 {
            continue;
        }
        let path = crate::normalize_path(&if b.path.is_absolute() { b.path } else { root.join(b.path) });
        let line = b.line - 1;
        if s.debug.breakpoint_at(&path, line).is_some() {
            continue;
        }
        let id = s.debug.next_id;
        s.debug.next_id = s.debug.next_id.saturating_add(1);
        let text = texts.entry(path.clone()).or_insert_with(|| codecraft_fs::read_text_file(&path).ok());
        if let Some(anchor) = text.as_deref().and_then(|t| t.lines().nth(line)) {
            s.debug.anchors.insert(id, anchor.trim().to_owned());
        }
        s.debug.breakpoints.push(Breakpoint {
            id,
            path,
            line,
            enabled: b.enabled,
            condition: b.condition,
            log_message: b.log_message,
            hit_condition: b.hit_condition,
            temporary: b.temporary,
            verified: None,
            message: None,
        });
    }
    if !stored.watches.is_empty() {
        s.debug.watches = stored.watches.into_iter().filter(|w| !w.trim().is_empty()).map(Watch::new).collect();
    }
    if let Some(v) = stored.break_on_panic {
        s.debug.break_on_panic = v;
    }
    if let Some(e) = stored.exceptions {
        s.debug.exceptions = e;
    }
    true
}

/// Write the project's breakpoints and watches (only once there is something to keep).
pub(crate) fn save(s: &mut Session) {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return };
    if s.debug.loaded_for.as_ref() != Some(&Some(root.clone())) {
        return;
    }
    let file = breakpoints_file(&root);
    if s.debug.breakpoints.is_empty()
        && s.debug.watches.is_empty()
        && s.debug.exceptions.is_default()
        && s.debug.break_on_panic
        && !file.exists()
    {
        return;
    }
    let stored = BreakpointsFile {
        version: 1,
        breakpoints: s
            .debug
            .breakpoints
            .iter()
            .filter(|b| !b.temporary)
            .map(|b| StoredBreakpoint {
                path: b.path.strip_prefix(&root).map(Path::to_path_buf).unwrap_or_else(|_| b.path.clone()),
                line: b.line.saturating_add(1),
                enabled: b.enabled,
                condition: b.condition.clone(),
                log_message: b.log_message.clone(),
                hit_condition: b.hit_condition.clone(),
                temporary: false,
            })
            .collect(),
        watches: s.debug.watches.iter().map(|w| w.expression.clone()).collect(),
        break_on_panic: (!s.debug.break_on_panic).then_some(false),
        exceptions: (!s.debug.exceptions.is_default()).then(|| s.debug.exceptions.clone()),
    };
    let text = match serde_json::to_string_pretty(&stored) {
        Ok(text) => text,
        Err(e) => {
            log::warn!("cannot serialize breakpoints: {e}");
            return;
        }
    };
    let result =
        file.parent().map_or(Ok(()), std::fs::create_dir_all).map_err(|e| e.to_string()).and_then(|()| {
            codecraft_fs::write_text_file_atomic(&file, &format!("{text}\n")).map_err(|e| e.to_string())
        });
    if let Err(e) = result {
        log::warn!("cannot save {}: {e}", file.display());
        s.status = format!("Cannot save breakpoints: {e}");
    }
}

// ---------------------------------------------------------------- inline values

fn compute_inline_values(s: &mut Session, locals: &[VariableInfo]) {
    let Some(session) = s.debug.session.as_ref() else { return };
    let Some((path, line)) = session.execution.clone() else { return };
    let text = match s.ws.doc_by_path(&path).and_then(|d| s.ws.doc(d)) {
        Some(d) => d.doc.text(),
        None => match codecraft_fs::read_text_file(&path) {
            Ok(text) => text,
            Err(_) => return,
        },
    };
    let values = inline_values_for(&text, Language::from_path(&path), line, locals);
    if let Some(session) = s.debug.session.as_mut() {
        session.inline_values =
            values.into_iter().map(|(line, text)| InlineValue { path: path.clone(), line, text }).collect();
    }
}

/// IntelliJ-style inline values: for each line of the current function up to the execution
/// line, the locals it mentions with their values (`total = 6   items = size=3`).
pub fn inline_values_for(
    text: &str,
    language: Language,
    exec_line: usize,
    locals: &[VariableInfo],
) -> Vec<(usize, String)> {
    let names: BTreeMap<&str, &VariableInfo> = locals
        .iter()
        .filter(|v| !v.name.is_empty() && v.name.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .map(|v| (v.name.as_str(), v))
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    let start = codecraft_syntax::outline(language, text)
        .into_iter()
        .filter(|i| i.kind == SymbolKind::Function && i.line <= exec_line)
        .map(|i| i.line)
        .max()
        .unwrap_or_else(|| exec_line.saturating_sub(40))
        .max(exec_line.saturating_sub(200));
    let mut out = Vec::new();
    for (line, content) in text.lines().enumerate().skip(start).take(exec_line.saturating_sub(start) + 1) {
        let code = match language.line_comment() {
            Some(comment) => content.split(comment).next().unwrap_or(content),
            None => content,
        };
        let mut seen = BTreeSet::new();
        let mut parts = Vec::new();
        for word in code.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if let Some(v) = names.get(word)
                && seen.insert(word)
            {
                let mut value: String = v.value.chars().take(MAX_INLINE_CHARS).collect();
                if v.value.chars().count() > MAX_INLINE_CHARS {
                    value.push('…');
                }
                parts.push(format!("{word} = {value}"));
            }
        }
        if !parts.is_empty() {
            out.push((line, parts.join("   ")));
        }
    }
    out
}

/// A JSON snapshot of the debugger for agents and the CLI (`debug.state`).
pub(crate) fn snapshot(s: &Session) -> Value {
    json!({
        "active": s.debug.is_active(),
        "session": s.debug.session,
        "sessions": s.debug.sessions(),
        "selectedSession": s.debug.selected_id(),
        "breakpoints": s.debug.breakpoints,
        "watches": s.debug.watches,
        "evaluation": s.debug.evaluation,
        "breakOnPanic": s.debug.break_on_panic,
        "exceptionBreakpoints": exceptions::state(s),
        "breakpointPopup": s.debug.breakpoint_popup,
        "hover": s.debug.hover,
        "completions": s.debug.completions,
        "stepTargets": s.debug.step_targets,
        "lastError": s.debug.last_error,
        "views": s.debug.views,
        "console": s.debug.console.iter().rev().take(200).rev().map(|l| json!({
            "kind": l.kind, "text": console_text(l),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_values_drop_the_pointer_before_a_summary() {
        assert_eq!(rust_value("0x000000010000c1f0 \"hello\"".into()), "\"hello\"");
        assert_eq!(rust_value("0x10 'c'".into()), "'c'");
        assert_eq!(rust_value("0x0000600000c04000 size=3".into()), "size=3");
        // Plain pointers, numbers and strings stay as they are.
        assert_eq!(rust_value("0x000000016fdfe9a0".into()), "0x000000016fdfe9a0");
        assert_eq!(rust_value("0x \"x\"".into()), "0x \"x\"");
        assert_eq!(rust_value("0xzz \"x\"".into()), "0xzz \"x\"");
        assert_eq!(rust_value("42".into()), "42");
        assert_eq!(rust_value("\"0x1 a\"".into()), "\"0x1 a\"");
        assert_eq!(rust_value("0x1é \"x\"".into()), "0x1é \"x\"");
        assert_eq!(rust_value(String::new()), "");
    }

    #[test]
    fn attach_picker_filters_by_user_and_query() {
        let p = |pid: u32, user: &str, cmd: &str| ProcessInfo {
            pid,
            user: user.into(),
            name: cmd.split('/').next_back().unwrap_or(cmd).into(),
            command: cmd.into(),
            kind: dap::processes::guess_kind(cmd),
        };
        let mut picker = AttachPicker {
            processes: vec![
                p(10, "me", "/a/target/debug/app"),
                p(11, "root", "/sbin/launchd"),
                p(120, "me", "/usr/bin/python3"),
            ],
            user: Some("me".into()),
            ..AttachPicker::default()
        };
        assert_eq!(picker.visible(), vec![0, 2]);
        picker.all_users = true;
        assert_eq!(picker.visible(), vec![0, 1, 2]);
        picker.query = "PYTHON".into();
        assert_eq!(picker.visible(), vec![2]);
        picker.query = "1".into();
        assert_eq!(picker.visible(), vec![0, 1, 2]);
        picker.query = "12".into();
        assert_eq!(picker.selected_process().map(|x| x.pid), Some(120));
        picker.selected = 5;
        assert!(picker.selected_process().is_none());
    }

    #[test]
    fn attach_commands_never_panic_on_bad_params() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        assert!(s.execute("run.attachToProcess", &json!({ "pid": -1 })).is_err());
        assert!(s.execute("run.attachToProcess", &json!({ "pid": 99999999999u64 })).is_err());
        assert!(s.execute("run.attachToProcess", &json!({ "pid": 5, "kind": "cobol" })).is_err());
        assert!(s.execute("run.attachToProcess", &json!({ "pid": 0 })).is_err());
        assert!(s.execute("debug.attach.confirm", &json!({})).is_err(), "disabled while closed");
        s.debug.attach = Some(AttachPicker::default());
        assert!(s.execute("debug.attach.confirm", &json!({ "index": 99 })).is_err());
        s.execute("debug.attach.move", &json!({ "delta": -99 })).unwrap();
        s.execute("debug.attach.move", &json!({ "delta": i64::MAX })).unwrap();
        s.execute("debug.attach.filter", &json!({ "query": "x" })).unwrap();
        s.execute("debug.attach.cancel", &Value::Null).unwrap();
        assert!(s.debug.attach.is_none());
    }

    fn var(name: &str, value: &str) -> VariableInfo {
        VariableInfo {
            name: name.into(),
            value: value.into(),
            type_name: None,
            reference: 0,
            evaluate_name: None,
            memory_reference: None,
        }
    }

    #[test]
    fn inline_values_cover_the_function_up_to_the_execution_line() {
        let text = "fn other() {}\n\nfn main() {\n    let total = 6;\n    let items = vec![1];\n    // total here\n    println!(\"{total}\");\n    let later = 1;\n}\n";
        let locals = [var("total", "6"), var("items", "size=1"), var("later", "0")];
        let values = inline_values_for(text, Language::Rust, 6, &locals);
        assert_eq!(
            values,
            vec![(3, "total = 6".to_owned()), (4, "items = size=1".to_owned()), (6, "total = 6".to_owned())]
        );
        assert!(inline_values_for(text, Language::Rust, 6, &[]).is_empty());
        let long = var("total", &"x".repeat(200));
        let values = inline_values_for(text, Language::Rust, 3, &[long]);
        assert!(values[0].1.ends_with('…'));
        // Hostile execution lines don't panic.
        assert!(inline_values_for(text, Language::Rust, usize::MAX, &locals).len() <= 9);
    }

    #[test]
    fn console_output_is_split_into_lines_and_continued() {
        let mut d = DebugState::default();
        d.push_console(DebugConsoleKind::Stdout, "hello ");
        d.push_console(DebugConsoleKind::Stdout, "world\nsecond\r\nthird");
        d.push_console(DebugConsoleKind::Stdout, " line\n");
        let texts: Vec<&str> = d.console.iter().map(console_text).collect();
        assert_eq!(texts, ["hello world", "second", "third line"]);
        d.info("built\n");
        d.info("a\n\nb");
        let texts: Vec<&str> = d.console.iter().map(console_text).collect();
        assert_eq!(texts, ["hello world", "second", "third line", "built", "a", "", "b"]);
    }

    #[test]
    fn console_is_capped() {
        let mut d = DebugState::default();
        for i in 0..(MAX_CONSOLE_LINES + 10) {
            d.info(format!("{i}"));
        }
        assert_eq!(d.console.len(), MAX_CONSOLE_LINES);
        assert_eq!(d.console.first().map(console_text), Some("10"));
    }

    #[test]
    fn cargo_plans_build_then_run_the_binary_or_the_test() {
        let mut run = CargoInvocation::new(CargoCommand::Run, "/w");
        run.package = Some("app".into());
        run.target = Some(TargetSelector::Bin("app".into()));
        run.program_args = vec!["--flag".into()];
        let plan = cargo_plan("Run app", &run).unwrap();
        let build = plan.build.unwrap();
        assert_eq!(build.args.first().map(String::as_str), Some("build"));
        assert!(build.args.iter().any(|a| a.starts_with("--message-format")));
        assert!(!build.args.contains(&"--flag".to_owned()));
        assert_eq!(plan.args, ["--flag"]);

        let mut test = CargoInvocation::new(CargoCommand::Test, "/w");
        test.target = Some(TargetSelector::Lib);
        test.test_filter = Some("tests::adds".into());
        test.test_exact = true;
        let plan = cargo_plan("Test adds", &test).unwrap();
        let build = plan.build.unwrap();
        assert!(build.args.contains(&"--no-run".to_owned()));
        assert!(!build.args.contains(&"tests::adds".to_owned()));
        assert_eq!(plan.args, ["tests::adds", "--exact", "--nocapture"]);
        assert!(cargo_plan("x", &CargoInvocation::new(CargoCommand::Clippy, "/w")).is_err());
    }

    #[test]
    fn artifacts_are_picked_by_target_name() {
        let exes =
            vec![("dep_bin".to_owned(), PathBuf::from("/t/dep")), ("my_app".to_owned(), PathBuf::from("/t/app"))];
        let pick = |name: Option<&str>| pick_artifact(&ArtifactPick::Cargo { name: name.map(str::to_owned) }, &exes);
        assert_eq!(pick(Some("my-app")).unwrap(), PathBuf::from("/t/app"));
        assert!(pick(None).unwrap_err().contains("2 executables"));
        assert_eq!(pick_artifact(&ArtifactPick::Cargo { name: None }, &exes[..1]).unwrap(), PathBuf::from("/t/dep"));
        assert!(pick_artifact(&ArtifactPick::Cargo { name: None }, &[]).is_err());
        assert!(pick_artifact(&ArtifactPick::Fixed("/definitely/missing".into()), &[]).is_err());
    }

    #[test]
    fn targets_round_trip_as_json() {
        let target = DebugTarget::Python {
            name: "a".into(),
            program: Some("/a.py".into()),
            module: None,
            args: vec![],
            cwd: None,
            env: vec![],
            python: None,
            just_my_code: true,
        };
        let v = serde_json::to_value(&target).unwrap();
        assert_eq!(v["kind"], "python");
        assert_eq!(serde_json::from_value::<DebugTarget>(v).unwrap(), target);
        let exe: DebugTarget =
            serde_json::from_value(json!({"kind": "executable", "name": "x", "program": "/bin/ls"})).unwrap();
        assert!(matches!(exe, DebugTarget::Executable { ref args, .. } if args.is_empty()));
        assert!(serde_json::from_value::<DebugTarget>(json!({"kind": "nope"})).is_err());
        let attach: DebugTarget =
            serde_json::from_value(json!({"kind": "attach", "name": "a", "pid": 42, "process": "rust"})).unwrap();
        assert_eq!(attach, DebugTarget::Attach { name: "a".into(), pid: 42, process: ProcessKind::Rust });
        let plan = plan_for(&attach).unwrap();
        assert!(matches!(plan.run, RunSpec::Attach { pid: 42, process: ProcessKind::Rust }));
        assert!(plan.build.is_none());
        let own = DebugTarget::Attach { name: "me".into(), pid: std::process::id(), process: ProcessKind::Native };
        assert!(plan_for(&own).is_err());
        assert!(plan_for(&DebugTarget::Attach { name: "0".into(), pid: 0, process: ProcessKind::Native }).is_err());
    }

    #[test]
    fn lines_convert_without_overflow() {
        assert_eq!(line_to_dap(0), 1);
        assert_eq!(line_to_dap(usize::MAX), i64::MAX);
        assert_eq!(line_from_dap(1), 0);
        assert_eq!(line_from_dap(0), 0);
        assert_eq!(line_from_dap(-5), 0);
    }
}
