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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub name: String,
    pub reference: i64,
    pub expensive: bool,
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

/// The tabs of the Debug tool window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugTab {
    #[default]
    Variables,
    Console,
    Breakpoints,
}

/// One debug session as the UI sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugSession {
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
            | Self::Node { name, .. } => name,
        }
    }
}

/// The debugger's state on the [`Session`].
pub struct DebugState {
    pub breakpoints: Vec<Breakpoint>,
    pub watches: Vec<Watch>,
    pub console: Vec<ConsoleLine>,
    /// The current (or last) session.
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
    /// Stop when a Rust program panics (a `rust_panic` function breakpoint), like RustRover.
    pub break_on_panic: bool,
    /// The last target debugged (Rerun).
    pub last_target: Option<DebugTarget>,
    next_id: u64,
    /// The project whose breakpoints are loaded (`Some(None)`: none open, loaded nothing).
    loaded_for: Option<Option<PathBuf>>,
    /// Each breakpoint's line text, to follow it when lines are inserted or deleted above.
    anchors: HashMap<u64, String>,
    /// Document revision per breakpoint file at the last line check.
    revisions: HashMap<PathBuf, u64>,
    rt: Option<Runtime>,
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
            break_on_panic: true,
            last_target: None,
            next_id: 1,
            loaded_for: None,
            anchors: HashMap::new(),
            revisions: HashMap::new(),
            rt: None,
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

    /// Background work is in flight (the UI keeps polling).
    pub fn busy(&self) -> bool {
        self.rt.is_some()
    }

    /// Something will change without user input: building, starting, running, or answers still
    /// pending. False once the program is paused with everything loaded (or the session ended).
    pub fn settling(&self) -> bool {
        let Some(rt) = &self.rt else { return false };
        match &rt.phase {
            Phase::Build { .. } | Phase::Prepare { .. } => true,
            Phase::Live { .. } => {
                let stopped = self.session.as_ref().is_some_and(DebugSession::is_stopped);
                !stopped || rt.pending.values().any(Pending::blocks_settling)
            }
        }
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
                | Self::SetBreakpoints { .. }
        )
    }
}

#[derive(Clone, Debug)]
enum EvalPurpose {
    Watch(String),
    Console,
    Evaluate(String),
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
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// A path from the adapter, mapped back into the project as the user opened it.
    fn map_from_adapter(&self, path: &Path) -> PathBuf {
        if let (Some(root), Some(canonical)) = (&self.project_root, &self.canonical_root)
            && root != canonical
            && let Ok(rest) = path.strip_prefix(canonical)
        {
            return root.join(rest);
        }
        crate::normalize_path(path)
    }

    /// Kill everything now (window closing).
    fn kill(self, pid: Option<u32>, exited: bool) {
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
                args: args.clone(),
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
        DebugTarget::Node { name, program, args, cwd } => Ok(LaunchPlan {
            name: name.clone(),
            build: None,
            run: RunSpec::Node { program: program.clone() },
            args: args.clone(),
            cwd: cwd.clone().or_else(|| program.parent().map(Path::to_path_buf)),
            env: Vec::new(),
        }),
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

/// Runs on a worker thread: find the adapter, work out launch arguments, spawn it.
fn prepare(plan: &LaunchPlan, program: Option<PathBuf>) -> Result<Prepared, String> {
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
            let launch = dap::launch::native(
                adapter.kind,
                &dap::launch::NativeLaunch {
                    name: plan.name.clone(),
                    program,
                    args: plan.args.clone(),
                    cwd: plan.cwd.clone(),
                    env: plan.env.clone(),
                    init_commands,
                    stop_on_entry: false,
                    source_languages: if *rust { vec!["rust".into()] } else { Vec::new() },
                },
            );
            (adapter, launch)
        }
        RunSpec::Python { program, module, python, just_my_code } => {
            let adapter = dap::discover::find_debugpy(python.as_deref()).map_err(|e| e.to_string())?;
            let launch = dap::launch::python(&dap::launch::PythonLaunch {
                name: plan.name.clone(),
                program: program.clone(),
                module: module.clone(),
                args: plan.args.clone(),
                cwd: plan.cwd.clone(),
                env: plan.env.clone(),
                python: python.clone(),
                just_my_code: *just_my_code,
                stop_on_entry: false,
            });
            (adapter, launch)
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

fn adapter_id(kind: AdapterKind) -> &'static str {
    match kind {
        AdapterKind::LldbDap => "lldb-dap",
        AdapterKind::CodeLldb => "lldb",
        AdapterKind::Debugpy => "debugpy",
        AdapterKind::JsDebug => "pwa-node",
        AdapterKind::Other => "codecraft",
    }
}

// ---------------------------------------------------------------- starting and stopping

/// Start debugging `target` (stopping a session that is still running).
pub(crate) fn start(s: &mut Session, target: DebugTarget) -> Result<(), CommandError> {
    let plan = plan_for(&target).map_err(CommandError::Failed)?;
    if s.debug.rt.is_some() {
        stop(s);
        if let Some(rt) = s.debug.rt.take() {
            let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
            let exited = rt.exited;
            rt.kill(pid, exited);
        }
    }
    ensure_loaded(s);
    let _ = s.save_all();
    reset_verification(s);
    for w in &mut s.debug.watches {
        w.clear();
    }
    s.debug.evaluation = None;
    s.debug.choosing = false;
    s.debug.last_target = Some(target.clone());
    s.debug.console.clear();
    s.debug.session = Some(DebugSession { name: plan.name.clone(), ..DebugSession::default() });
    s.ws.layout.show(ToolWindowId::Debug);
    let root = s.project_root();
    let phase = match &plan.build {
        Some(build) => {
            s.debug.tab = DebugTab::Console;
            let line = std::iter::once(build.program.display().to_string())
                .chain(build.args.iter().map(|a| codecraft_cargo::shell_quote(a)))
                .collect::<Vec<_>>()
                .join(" ");
            s.debug.info(line);
            let (handle, events) =
                codecraft_cargo::spawn_process(&build.program, &build.args, &build.cwd, &build.env, build.cargo)
                    .map_err(|e| CommandError::Failed(e.to_string()))?;
            set_status(s, DebugStatus::Building);
            Phase::Build { handle, events, executables: Vec::new() }
        }
        None => {
            set_status(s, DebugStatus::Starting);
            Phase::Prepare { rx: spawn_prepare(plan.clone(), None) }
        }
    };
    s.debug.rt = Some(Runtime::new(plan, phase, root));
    s.status = format!("Debugging {}", target.name());
    Ok(())
}

fn spawn_prepare(plan: LaunchPlan, program: Option<PathBuf>) -> Receiver<Result<Prepared, String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("debug-prepare".into()).spawn(move || {
        let result = prepare(&plan, program);
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

/// Stop the session: kill a build, cancel a start, or end the debuggee (`disconnect`).
pub(crate) fn stop(s: &mut Session) -> bool {
    let Some(rt) = s.debug.rt.as_mut() else { return false };
    rt.stopping = true;
    let pid = s.debug.session.as_ref().and_then(|x| x.debuggee_pid);
    let finished = match &rt.phase {
        Phase::Build { handle, .. } => {
            let _ = handle.kill();
            true
        }
        Phase::Prepare { .. } => true,
        Phase::Live { client, .. } => {
            client.terminate(TERMINATE_GRACE);
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
    reset_verification(s);
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
}

// ---------------------------------------------------------------- pump

/// Drain background events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = ensure_loaded(s);
    changed |= follow_edits(s);
    let Some(rt) = s.debug.rt.as_mut() else { return changed };
    match &mut rt.phase {
        Phase::Build { events, .. } => {
            let batch: Vec<RunEvent> = events.try_iter().take(MAX_EVENTS_PER_PUMP).collect();
            if batch.is_empty() {
                return changed;
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
            Err(crossbeam_channel::TryRecvError::Empty) => changed,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                fail(s, "the debugger did not start");
                true
            }
        },
        Phase::Live { events, .. } => {
            let batch: Vec<DapEvent> = events.try_iter().take(MAX_EVENTS_PER_PUMP).collect();
            if batch.is_empty() {
                return changed;
            }
            for event in batch {
                on_dap_event(s, event);
            }
            true
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
                    rt.phase = Phase::Prepare { rx: spawn_prepare(plan, Some(program.clone())) };
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
        DapEvent::Request { seq, command, .. } => {
            // Child sessions (js-debug's `startDebugging`) are not supported yet.
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
    let (stopping, exited) = (rt.stopping, rt.exited);
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
                if !rt.request(Pending::Launch, |c| c.launch(args)) {
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
            for (theirs, ours) in body.breakpoints.iter().zip(ids) {
                let Some(ours) = ours else { continue };
                if let Some(id) = theirs.id {
                    rt.adapter_ids.insert(id, ours);
                }
                if let Some(bp) = s.debug.breakpoints.iter_mut().find(|b| b.id == ours) {
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
                .map(|x| ScopeInfo { name: x.name, reference: x.variables_reference, expensive: x.expensive })
                .collect();
            if let Some(session) = s.debug.session.as_mut() {
                session.scopes = scopes.clone();
            }
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
            let Some(session) = s.debug.session.as_mut() else { return };
            session.loading.remove(&reference);
            let vars: Vec<VariableInfo> = match response.result {
                Ok(body) => dap::types::parse::<dap::VariablesBody>(&body)
                    .unwrap_or_default()
                    .variables
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(|v| VariableInfo {
                        name: v.name,
                        value: v.value,
                        type_name: v.type_.filter(|t| !t.is_empty()),
                        reference: v.variables_reference,
                        evaluate_name: v.evaluate_name,
                    })
                    .collect(),
                Err(e) => vec![VariableInfo {
                    name: "<error>".into(),
                    value: e,
                    type_name: None,
                    reference: 0,
                    evaluate_name: None,
                }],
            };
            session.variables.insert(reference, vars.clone());
            let is_locals = session.scopes.first().is_some_and(|x| x.reference == reference);
            let parent = rt.ref_paths.get(&reference).cloned().unwrap_or_default();
            for v in vars.iter().filter(|v| v.reference > 0) {
                let path = child_path(&parent, &v.name);
                if s.debug.expanded.contains(&path) {
                    request_variables(s, v.reference, path);
                }
            }
            if is_locals {
                compute_inline_values(s, &vars);
            }
        }
        Pending::Evaluate { epoch, purpose } => on_evaluate(s, epoch == epoch_now, purpose, response.result),
        Pending::Resume => {
            if let Err(e) = response.result {
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
                rt.stopping = true;
                if let Some(client) = rt.client() {
                    client.terminate(TERMINATE_GRACE);
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
            s.debug.push_console(kind, ev.output);
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
        send_breakpoints(s, &path);
    }
    let break_on_panic = s.debug.break_on_panic;
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let rust = matches!(rt.plan.run, RunSpec::Native { rust: true, .. });
    if rust && break_on_panic && rt.capabilities.supports_function_breakpoints {
        let panic = dap::FunctionBreakpoint { name: "rust_panic".into(), ..Default::default() };
        rt.request(Pending::Setup, |c| c.set_function_breakpoints(&[panic]));
    }
    if !rt.capabilities.exception_breakpoint_filters.is_empty() {
        // Python: stop on uncaught exceptions (debugpy's default in other IDEs too).
        let filters: Vec<String> = rt
            .capabilities
            .exception_breakpoint_filters
            .iter()
            .filter(|f| f.default || (rt.kind == AdapterKind::Debugpy && f.filter == "uncaught"))
            .map(|f| f.filter.clone())
            .collect();
        rt.request(Pending::Setup, |c| c.set_exception_breakpoints(&filters));
    }
    if rt.capabilities.supports_configuration_done_request {
        rt.request(Pending::ConfigurationDone, DapClient::configuration_done);
    } else {
        started(s);
    }
}

/// Send one file's breakpoints (and a pending Run to Cursor location) to the adapter.
fn send_breakpoints(s: &mut Session, path: &Path) {
    let dbg = &mut s.debug;
    let Some(rt) = dbg.rt.as_mut() else { return };
    if !rt.configured {
        return;
    }
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
    if let Some((cursor_path, line)) = &rt.run_to_cursor
        && cursor_path == path
        && !list.iter().any(|b| b.line == line_to_dap(*line))
    {
        list.push(dap::SourceBreakpoint { line: line_to_dap(*line), ..Default::default() });
        ids.push(None);
    }
    if list.is_empty() {
        if !rt.sent.remove(path) {
            return;
        }
    } else {
        rt.sent.insert(path.to_path_buf());
    }
    let adapter_path = rt.to_adapter(path);
    rt.request(Pending::SetBreakpoints { ids }, |c| c.set_breakpoints(&adapter_path, &list));
}

fn line_to_dap(line: usize) -> i64 {
    i64::try_from(line).unwrap_or(i64::MAX - 1).saturating_add(1)
}

fn line_from_dap(line: i64) -> usize {
    usize::try_from(line.saturating_sub(1)).unwrap_or(0)
}

fn on_stopped(s: &mut Session, ev: dap::StoppedEvent) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    rt.epoch += 1;
    rt.stops += 1;
    rt.last_stop = None;
    let first = rt.stops == 1;
    let epoch = rt.epoch;
    let cursor = rt.run_to_cursor.take();
    // Temporary breakpoints go away once hit.
    let hit: Vec<u64> = ev.hit_breakpoint_ids.iter().filter_map(|id| rt.adapter_ids.get(id)).copied().collect();
    let Some(session) = s.debug.session.as_mut() else { return };
    session.clear_stop();
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
        send_breakpoints(s, &path);
    }
    s.ws.layout.show(ToolWindowId::Debug);
    if first {
        s.debug.tab = DebugTab::Variables;
    }
    s.status = match &ev.description {
        Some(d) if !d.is_empty() => format!("Paused: {d}"),
        _ => format!("Paused ({})", ev.reason),
    };
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
    if reveal && let Some((path, line)) = session.execution.clone() {
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
    let parsed = result.map(|body| dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default());
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
                if !body.result.is_empty() {
                    s.debug.push_console(DebugConsoleKind::Result, body.result);
                }
            }
            Err(e) => s.debug.push_console(DebugConsoleKind::Error, e),
        },
        EvalPurpose::Evaluate(expression) => {
            let result = parsed.map(|body| VariableInfo {
                name: expression.clone(),
                value: body.result,
                type_name: body.type_.filter(|t| !t.is_empty()),
                reference: body.variables_reference,
                evaluate_name: Some(expression.clone()),
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

pub(crate) fn step(s: &mut Session, step: Step) -> Result<(), CommandError> {
    if !s.debug.is_stopped() {
        return Err(CommandError::Disabled("the program is not paused".into()));
    }
    let thread = current_thread(s).ok_or_else(|| CommandError::Failed("no thread to step".into()))?;
    let Some(rt) = s.debug.rt.as_mut() else { return Err(CommandError::Disabled("not debugging".into())) };
    let previous = s.debug.session.as_ref().and_then(|x| x.status.clone());
    let sent = rt.request(Pending::Resume, |c| match step {
        Step::Over => c.next(thread),
        Step::Into => c.step_in(thread),
        Step::Out => c.step_out(thread),
        Step::Continue => c.continue_(thread),
    });
    if !sent {
        return Err(CommandError::Failed("the debugger is not responding".into()));
    }
    rt.last_stop = previous;
    rt.epoch += 1;
    if let Some(session) = &mut s.debug.session {
        session.status = Some(DebugStatus::Running);
        session.clear_stop();
    }
    s.status = "Running".into();
    Ok(())
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
    send_breakpoints(s, &path);
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
    } else {
        s.debug.evaluation =
            Some(Evaluation { expression: expression.to_owned(), result: Err(String::new()), pending: true });
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
    if index >= s.debug.watches.len() {
        return Err(CommandError::BadParams(format!("no watch {index}")));
    }
    s.debug.watches.remove(index);
    save(s);
    Ok(())
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
fn ensure_loaded(s: &mut Session) -> bool {
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
    true
}

/// Write the project's breakpoints and watches (only once there is something to keep).
pub(crate) fn save(s: &mut Session) {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return };
    if s.debug.loaded_for.as_ref() != Some(&Some(root.clone())) {
        return;
    }
    let file = breakpoints_file(&root);
    if s.debug.breakpoints.is_empty() && s.debug.watches.is_empty() && !file.exists() {
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
        "breakpoints": s.debug.breakpoints,
        "watches": s.debug.watches,
        "evaluation": s.debug.evaluation,
        "breakOnPanic": s.debug.break_on_panic,
        "console": s.debug.console.iter().rev().take(200).rev().map(|l| json!({
            "kind": l.kind, "text": console_text(l),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, value: &str) -> VariableInfo {
        VariableInfo { name: name.into(), value: value.into(), type_name: None, reference: 0, evaluate_name: None }
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
