//! SSH servers, deployment, File Transfer log, Remote Host browsing and Remote Development
//! (M11 track S).
//!
//! Everything that talks to a server runs the system `ssh`/`rsync`/`scp` on a worker thread as an
//! *operation* (a list of [`Step`]s); [`pump`] applies the result (log entry, listing, opening a
//! downloaded file, a sync preview…). Each server has separate interactive and bulk ControlMaster
//! connections (`ssh -N`); masters are closed on [`shutdown`].
//!
//! Test seams: [`RemoteState::home_override`] (instead of `$CODECRAFT_HOME`),
//! [`RemoteState::ssh_config_override`] (instead of `~/.ssh/config`) and [`Programs`] (fake
//! `ssh`/`rsync`/`scp` scripts).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::Serialize;
use serde_json::Value;

use codecraft_remote::command as rcmd;
use codecraft_remote::listing;
use codecraft_remote::rsync;
use codecraft_remote::server::{self, ServerConfig, ServersFile, SshServer};
use codecraft_remote::{SshConfig, SshOptions};
use codecraft_workspace::{NotificationLevel, ToolWindowId};

use crate::{CommandError, Session};

pub use codecraft_remote::listing::EntryKind;
pub use codecraft_remote::rsync::Change;
pub use codecraft_remote::server::ServerSource;
pub use codecraft_remote::{Direction, RemoteEntry};

/// File Transfer entries kept.
const MAX_LOG: usize = 500;
/// Output kept per process (listings, rsync previews).
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
/// Remote files opened from Remote Host larger than this are refused.
pub const MAX_OPEN_BYTES: u64 = 50 * 1024 * 1024;
/// Do not restart a failed master more often than this.
const MASTER_RETRY: Duration = Duration::from_secs(30);

/// The programs run for remote work (names on PATH or paths; tests use fake scripts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Programs {
    pub ssh: String,
    pub rsync: String,
    pub scp: String,
}

impl Default for Programs {
    fn default() -> Self {
        Self { ssh: "ssh".into(), rsync: "rsync".into(), scp: "scp".into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state", content = "error")]
pub enum TransferStatus {
    Running,
    Done,
    Failed(String),
}

/// One line of the File Transfer tool window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferEntry {
    pub id: u64,
    /// Start time, milliseconds since the epoch.
    pub time_ms: u64,
    pub server: String,
    pub summary: String,
    pub status: TransferStatus,
    /// What the tools printed (itemized changes, errors).
    pub output: String,
}

/// The Remote Host tool window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBrowser {
    pub server: Option<String>,
    /// The listed folder (absolute, `~` or `~/…`).
    pub path: String,
    pub entries: Vec<RemoteEntry>,
    pub loading: bool,
    pub error: Option<String>,
    /// The selected entry's name.
    pub selected: Option<String>,
}

/// "Choose a server" popup: running `command` with `params` + `{server}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerPicker {
    pub title: String,
    pub command: String,
    pub params: Value,
    pub servers: Vec<String>,
}

/// One item of a sync preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncItem {
    /// Relative to the synced folder.
    pub path: String,
    pub direction: Direction,
    pub change: Change,
    pub is_dir: bool,
    pub selected: bool,
}

/// Sync with Deployed: the differences found by `rsync --dry-run --itemize-changes`, applied
/// with `remote.sync.apply`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPreview {
    pub server: String,
    pub local: PathBuf,
    pub remote: String,
    pub items: Vec<SyncItem>,
    #[serde(skip)]
    pub excludes: Vec<String>,
}

/// Remote Host › Delete asks first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmDelete {
    pub server: String,
    pub paths: Vec<String>,
}

/// Text fields of the server form (Deployment › Configuration…), edited by the UI in place.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerForm {
    pub name: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub identity_file: String,
}

/// The mapping form of the selected server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingForm {
    pub local: String,
    pub remote: String,
    /// Excluded paths separated by `;` or new lines.
    pub excluded: String,
}

/// Tools › Deployment › Configuration….
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentDialog {
    pub selected: Option<String>,
    pub mapping: MappingForm,
    /// Adding a CodeCraft-defined server.
    pub new_server: Option<ServerForm>,
    pub error: Option<String>,
}

/// File › Remote Development….
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDevDialog {
    pub server: String,
    pub path: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state", content = "message")]
pub enum TestStatus {
    Running,
    Ok(String),
    Failed(String),
}

/// A unit of worker-thread work.
#[derive(Clone, Debug)]
pub(crate) enum Step {
    /// Run a program; stdin from a file, stdout to a file (else captured).
    Output(String),
    Measure { label: String, program: String, args: Vec<String> },
    Run {
        program: String,
        args: Vec<String>,
        stdin: Option<PathBuf>,
        stdout_to: Option<PathBuf>,
    },
    Mkdir(PathBuf),
    RefreshMirror {
        local: PathBuf,
        incoming: PathBuf,
        baseline: PathBuf,
        protected: Arc<std::sync::Mutex<std::collections::BTreeSet<PathBuf>>>,
    },
    CheckedTree {
        root: PathBuf, server: SshServer, options: SshOptions,
        from: PathBuf, to: Option<PathBuf>, directory: bool, create: bool,
        remote_from: String, remote_to: Option<String>, baseline: PathBuf,
        rsync: String,
    },
    CheckedUpload {
        root: PathBuf,
        server: SshServer,
        options: SshOptions,
        local: PathBuf,
        remote: String,
        baseline: PathBuf,
        snapshot: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    Write {
        path: PathBuf,
        text: String,
    },
}

impl Step {
    pub(crate) fn run(program: &str, args: Vec<String>) -> Step {
        Step::Run { program: program.to_string(), args, stdin: None, stdout_to: None }
    }
}

/// What to do when an operation finishes.
#[derive(Clone, Debug)]
pub(crate) enum Follow {
    Nothing,
    ProjectUpload {
        local: PathBuf,
    },
    /// Reload open documents that changed on disk.
    Reload,
    AutoProbe,
    LspProbe { server: String, generation: u64, kind: codecraft_lsp::ServerKind },
    LspInstall { kind: codecraft_lsp::ServerKind },
    Listing {
        server: String,
        path: String,
    },
    RefreshBrowser,
    ExternalSource { local: PathBuf, line: usize, column: usize, utf16: bool },
    OpenRemote {
        server: String,
        remote: String,
        local: PathBuf,
    },
    Compare {
        server: String,
        remote: String,
        local: PathBuf,
        fetched: PathBuf,
    },
    SyncPreview {
        server: String,
        local: PathBuf,
        remote: String,
        excludes: Vec<String>,
    },
    Test {
        server: String,
    },
    RemoteDev {
        server: String,
        remote: String,
        local: PathBuf,
        open_window: bool,
    },
}

/// Connection state shown in a remote project's status bar and command responses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionState {
    Cached,
    Connecting,
    Connected,
    Disconnected,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProject {
    pub server: String,
    pub destination: String,
    pub remote: String,
    pub connection: ConnectionState,
    pub error: Option<String>,
    /// Saved files awaiting a checked upload; failures pause the queue without discarding it.
    pub pending_uploads: std::collections::BTreeSet<PathBuf>,
}

struct OpDone {
    id: u64,
    result: Result<Vec<String>, String>,
}

struct Master {
    child: Option<Child>,
    started: Instant,
    server: SshServer,
    options: SshOptions,
    stderr: Arc<std::sync::Mutex<String>>,
}

/// Recent frame-section costs. Warnings are limited to one per section per five seconds.
#[derive(Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameSection {
    pub worst_ms: f64,
    pub last_ms: f64,
    pub stalls: u64,
    #[serde(skip)]
    window: Option<Instant>,
    #[serde(skip)]
    warned: Option<Instant>,
}
impl FrameSection {
    fn record(&mut self, name: &str, elapsed: Duration, now: Instant) {
        let ms = elapsed.as_secs_f64() * 1000.0;
        if self.window.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(60)) {
            self.window = Some(now); self.worst_ms = 0.0;
        }
        self.last_ms = ms;
        self.worst_ms = self.worst_ms.max(ms);
        if ms > 30.0 {
            self.stalls = self.stalls.saturating_add(1);
            if self.warned.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(5)) {
                log::warn!("frame stall section={name} duration_ms={ms:.1}");
                self.warned = Some(now);
            }
        }
    }
}

#[derive(Default, Debug)]
struct RefreshDebounce {
    pending: bool,
    last_event: Option<Instant>,
    last_sync: Option<Instant>,
    level: usize,
    events: u64,
    refreshes: u64,
    reason: &'static str,
}
impl RefreshDebounce {
    fn event(&mut self, now: Instant) {
        if self.last_event.is_some_and(|at| now.saturating_duration_since(at) > Duration::from_secs(60)) { self.level = 0; }
        self.pending = true; self.last_event = Some(now);
        self.events = self.events.saturating_add(1);
    }
    fn spacing(&self) -> Duration { Duration::from_secs([2, 5, 15, 30].get(self.level).copied().unwrap_or(30)) }
    fn due(&self, now: Instant) -> bool {
        self.pending && self.last_event.is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_secs(2))
            && self.last_sync.is_none_or(|at| now.saturating_duration_since(at) >= self.spacing())
    }
    fn started(&mut self, now: Instant) {
        self.pending = false; self.last_sync = Some(now);
        self.level = self.level.saturating_add(1).min(3);
        self.refreshes = self.refreshes.saturating_add(1);
    }
}

pub struct RemoteState {
    /// Instead of `$CODECRAFT_HOME` (default `~/.codecraft`): `ssh.json` and the caches.
    pub home_override: Option<PathBuf>,
    /// Instead of `~/.ssh/config`.
    pub ssh_config_override: Option<PathBuf>,
    pub programs: Programs,
    /// Separate interactive and bulk shared connections per server; fixtures can turn sharing off.
    pub share_connections: bool,
    /// Directory for control sockets (default: a short path under `/tmp`, macOS limits socket
    /// paths to 104 bytes).
    pub control_dir_override: Option<PathBuf>,
    pub log: Vec<TransferEntry>,
    pub browser: RemoteBrowser,
    pub picker: Option<ServerPicker>,
    pub sync_preview: Option<SyncPreview>,
    pub confirm: Option<ConfirmDelete>,
    pub dialog: Option<DeploymentDialog>,
    /// Tools › Deployment › Options… is open.
    pub options_open: bool,
    pub remote_dev: Option<RemoteDevDialog>,
    pub project: Option<RemoteProject>,
    /// Engine commands and pump hold the mirror lock; nested commands reuse it.
    pub(crate) mirror_guard_active: bool,
    pub(crate) protected: Arc<std::sync::Mutex<std::collections::BTreeSet<PathBuf>>>,
    pub auto_refresh: bool,
    pub helpers_disabled: bool,
    pub frame_sections: BTreeMap<String, FrameSection>,
    debounce: RefreshDebounce,
    watcher_reason: Arc<std::sync::Mutex<String>>,
    reload: Option<Receiver<ReloadFiles>>,
    pub(crate) tree_refreshes: u64,
    pub poll_interval_secs: u64,
    poll_idle: usize,
    poll_signature: Option<String>,
    refresh_pending: bool,
    pub(crate) lsp_tools: HashMap<(String, codecraft_lsp::ServerKind), Option<String>>,
    pub(crate) lsp_retry: HashMap<codecraft_lsp::ServerKind, (u32, Instant)>,
    pub(crate) lsp_failures: HashMap<codecraft_lsp::ServerKind, u32>,
    pub(crate) local_lsp: std::collections::HashSet<codecraft_lsp::ServerKind>,
    pub(crate) absolute_root: Option<String>,
    pub window_focused: bool,
    pub(crate) last_refresh: Instant,
    pub conflicts: Vec<PathBuf>,
    pub conflicts_open: bool,
    tree_queue: std::collections::VecDeque<(String, Step)>,
    watcher: Option<(Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>)>,
    /// Test Connection results by server.
    pub tests: BTreeMap<String, TestStatus>,
    servers: Option<(Vec<SshServer>, Vec<String>)>,
    /// Files opened from Remote Host: local cache file → (server, remote path).
    opened: HashMap<PathBuf, (String, String)>,
    masters: HashMap<String, Master>,
    control_generation: HashMap<String, u64>,
    ops: HashMap<u64, (Follow, bool)>,
    op_hosts: HashMap<u64, String>,
    op_started: HashMap<u64, Instant>,
    lsp_started: HashMap<codecraft_lsp::ServerKind, Instant>,
    watcher_started: Option<Instant>,
    tx: Sender<OpDone>,
    rx: Receiver<OpDone>,
    cancel: Arc<AtomicBool>,
    nonce: u64,
    next_id: u64,
}

impl std::fmt::Debug for RemoteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteState").field("log", &self.log.len()).field("ops", &self.ops.len()).finish()
    }
}

impl Default for RemoteState {
    fn default() -> Self {
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let (tx, rx) = unbounded();
        Self {
            home_override: None,
            ssh_config_override: None,
            programs: Programs::default(),
            share_connections: true,
            control_dir_override: None,
            log: Vec::new(),
            browser: RemoteBrowser::default(),
            picker: None,
            sync_preview: None,
            confirm: None,
            dialog: None,
            options_open: false,
            remote_dev: None,
            project: None,
            mirror_guard_active: false,
            protected: Default::default(),
            auto_refresh: true,
            helpers_disabled: false,
            frame_sections: Default::default(),
            debounce: Default::default(),
            watcher_reason: Default::default(),
            reload: None,
            tree_refreshes: 0,
            poll_interval_secs: 3,
            poll_idle: 0,
            poll_signature: None,
            refresh_pending: false,
            lsp_tools: HashMap::new(),
            local_lsp: Default::default(),
            lsp_retry: Default::default(),
            lsp_failures: Default::default(),
            absolute_root: None,
            window_focused: true,
            last_refresh: Instant::now(),
            conflicts: Vec::new(),
            conflicts_open: false,
            tree_queue: Default::default(),
            watcher: None,
            tests: BTreeMap::new(),
            servers: None,
            opened: HashMap::new(),
            masters: HashMap::new(),
            control_generation: HashMap::new(),
            ops: HashMap::new(),
            op_hosts: HashMap::new(),
            op_started: HashMap::new(),
            lsp_started: HashMap::new(),
            watcher_started: None,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            nonce: NONCE.fetch_add(1, Ordering::Relaxed),
            next_id: 1,
        }
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// FNV-1a, for short stable file names.
pub(crate) fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A file-name-safe form of a server name.
pub(crate) fn safe_name(s: &str) -> String {
    let n: String =
        s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).take(40).collect();
    if n.is_empty() || n.starts_with('.') { format!("s{n}") } else { n }
}

impl RemoteState {
    /// Whether background work is in flight.
    pub fn auto_refresh_mode(&self) -> &'static str {
        if !self.auto_refresh { "paused" }
        else if self.helpers_disabled { "helpers disabled" }
        else if self.watcher.as_ref().is_some_and(|(_, streaming, _)| streaming.load(Ordering::Acquire)) { "inotify" }
        else if self.poll_interval_secs == 0 { "polling disabled" }
        else { "polling" }
    }

    /// Postpone background polling while the user is interacting with the project.
    pub fn note_activity(&mut self) {
        self.poll_idle = 0;
        self.last_refresh = Instant::now();
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        if focused && !self.window_focused { self.note_activity(); }
        self.window_focused = focused;
    }

    pub fn operations_pending(&self) -> bool {
        !self.ops.is_empty() || !self.tree_queue.is_empty() || self.reload.is_some()
            || (self.auto_refresh && !self.helpers_disabled && self.project.as_ref().is_some_and(|p| p.connection == ConnectionState::Connected)
                && (self.refresh_pending || self.debounce.pending))
    }

    pub fn busy(&self) -> bool {
        self.reload.is_some() || self.ops.values().any(|(follow, _)| !matches!(follow, Follow::AutoProbe | Follow::LspProbe { .. })) || !self.tree_queue.is_empty()
    }

    /// `$CODECRAFT_HOME`, else `~/.codecraft`.
    pub fn home(&self) -> Option<PathBuf> {
        if let Some(h) = &self.home_override {
            return Some(h.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home));
        }
        Some(user_home()?.join(".codecraft"))
    }

    /// `ssh.json` (CodeCraft-defined servers).
    pub fn servers_file(&self) -> Option<PathBuf> {
        Some(self.home()?.join("ssh.json"))
    }

    pub fn ssh_config_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.ssh_config_override {
            return Some(p.clone());
        }
        Some(user_home()?.join(".ssh").join("config"))
    }

    /// Downloads, opened remote files and Remote Development mirrors.
    pub fn cache_dir(&self) -> Option<PathBuf> {
        Some(self.home()?.join("remote"))
    }

    /// The servers (CodeCraft-defined first, then `~/.ssh/config` hosts), loaded once.
    pub fn servers(&mut self) -> &[SshServer] {
        if self.servers.is_none() {
            self.servers = Some(self.load_servers());
        }
        self.servers.as_ref().map(|(s, _)| s.as_slice()).unwrap_or(&[])
    }

    /// The servers if they were loaded already (never reads files; for the UI's lists).
    pub fn loaded_servers(&self) -> &[SshServer] {
        self.servers.as_ref().map(|(s, _)| s.as_slice()).unwrap_or(&[])
    }

    /// Problems met reading `~/.ssh/config` and `ssh.json`.
    pub fn server_warnings(&self) -> &[String] {
        self.servers.as_ref().map(|(_, w)| w.as_slice()).unwrap_or(&[])
    }

    /// Forget the loaded servers (re-read on next use).
    pub fn reload_servers(&mut self) {
        self.servers = None;
    }

    fn load_servers(&self) -> (Vec<SshServer>, Vec<String>) {
        load_servers_at(self.ssh_config_path().as_deref(), self.servers_file().as_deref())
    }

    /// `ssh.json` (empty when missing).
    pub fn own_servers(&self) -> Result<ServersFile, String> {
        match self.servers_file() {
            Some(path) => read_servers_file(&path),
            None => Ok(ServersFile::default()),
        }
    }

    pub(crate) fn save_own_servers(&mut self, file: &ServersFile) -> Result<(), String> {
        let path = self.servers_file().ok_or("cannot find the home directory; set CODECRAFT_HOME")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = file.to_json().map_err(|e| e.to_string())?;
        write_atomic(&path, &text)?;
        self.reload_servers();
        Ok(())
    }

    pub fn server(&mut self, name: &str) -> Option<SshServer> {
        self.servers().iter().find(|s| s.name == name).cloned()
    }

    /// Whether a server comes from `~/.ssh/config` (read-only in the dialogs).
    pub fn is_config_host(&mut self, name: &str) -> bool {
        self.server(name).is_some_and(|x| x.source == server::ServerSource::SshConfig)
    }

    pub fn server_names(&mut self) -> Vec<String> {
        self.servers().iter().map(|s| s.name.clone()).collect()
    }

    fn control_dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.control_dir_override {
            return Some(d.clone());
        }
        if !cfg!(unix) {
            return None;
        }
        let user: String =
            std::env::var("USER").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
        Some(PathBuf::from(format!("/tmp/codecraft-ssh-{user}")))
    }

    /// The control socket of a server for this session (short: macOS limits socket paths).
    fn control_path(&self, server: &str) -> Option<PathBuf> {
        let dir = self.control_dir()?;
        let generation = self.control_generation.get(server).copied().unwrap_or(0);
        let name = format!(
            "{}-{}-{generation}-{}",
            std::process::id(),
            self.nonce,
            short_hash(server).get(..12).unwrap_or("")
        );
        Some(dir.join(name))
    }

    /// The ssh options for a server (the control socket when connections are shared).
    pub(crate) fn ssh_options(&self, server: &str) -> SshOptions {
        let control_path = if self.share_connections {
            self.control_path(server).and_then(|p| p.to_str().map(str::to_string))
        } else {
            None
        };
        SshOptions { program: self.programs.ssh.clone(), control_path, connect_timeout: Some(15) }
    }

    pub(crate) fn bulk_options(&self, server: &str) -> SshOptions {
        self.ssh_options(&format!("{server}\0bulk"))
    }

    pub(crate) fn ensure_bulk_master(&mut self, server: &SshServer) {
        self.ensure_master_role(server, true);
    }

    /// Start the server's control master if it is not running (non-blocking).
    pub(crate) fn ensure_master(&mut self, server: &SshServer) {
        self.ensure_master_role(server, false);
    }

    fn ensure_master_role(&mut self, server: &SshServer, bulk: bool) {
        let key = if bulk { format!("{}\0bulk", server.name) } else { server.name.clone() };
        if !self.share_connections {
            return;
        }
        if let Some(m) = self.masters.get_mut(&key) {
            let alive = m.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
            let persistent = m.options.control_path.as_ref().is_some_and(|p| Path::new(p).exists());
            if alive || persistent || m.started.elapsed() < MASTER_RETRY {
                return;
            }
        }
        let opts = self.ssh_options(&key);
        let Some(dir) = self.control_dir() else { return };
        let setup = if opts.control_path.as_ref().is_some_and(|path| path.len() >= 100) {
            Err("control socket path exceeds the macOS socket limit; use a shorter control directory".to_owned())
        } else { create_private_dir(&dir).map_err(|error| error.to_string()) };
        if let Err(reason) = setup {
            self.masters.insert(key, Master { child: None, started: Instant::now(), server: server.clone(), options: opts, stderr: Arc::new(std::sync::Mutex::new(reason)) });
            return;
        }
        if let Some(p) = &opts.control_path {
            // A socket left by a crashed earlier run of this process id would stop the master.
            let _ = std::fs::remove_file(p);
        }
        let Ok(Some(args)) = rcmd::ssh_master(server, &opts) else { return };
        let stderr = Arc::new(std::sync::Mutex::new(String::new()));
        let mut child = Command::new(resolve_program(&self.programs.ssh))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                *stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = e.to_string();
                log::warn!("ssh master for {}: {e}", server.name);
            })
            .ok();
        if let Some(pipe) = child.as_mut().and_then(|child| child.stderr.take()) {
            let output = stderr.clone();
            std::thread::spawn(move || {
                *output.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = String::from_utf8_lossy(&read_capped(pipe, MAX_STDERR)).into_owned();
            });
        }
        self.masters.insert(
            key,
            Master { child, started: Instant::now(), server: server.clone(), options: opts, stderr },
        );
    }

    /// Whether `rsync` can be used (else transfers fall back to `scp`).
    pub fn has_rsync(&self) -> bool {
        let p = Path::new(&self.programs.rsync);
        if p.components().count() > 1 { p.is_file() } else { codecraft_lsp::which(&self.programs.rsync).is_some() }
    }

    /// The remote file a local cache file was opened from (Remote Host).
    pub fn opened_remote(&self, local: &Path) -> Option<&(String, String)> {
        self.opened.get(local)
    }

    fn next(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn push_log(&mut self, entry: TransferEntry) {
        self.log.push(entry);
        if self.log.len() > MAX_LOG {
            let extra = self.log.len() - MAX_LOG;
            self.log.drain(..extra);
        }
    }
}

fn read_servers_file(path: &Path) -> Result<ServersFile, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => ServersFile::parse(&t).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ServersFile::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The servers of an ssh config file and an `ssh.json`, with the problems met reading them.
pub(crate) fn load_servers_at(ssh_config: Option<&Path>, servers_file: Option<&Path>) -> (Vec<SshServer>, Vec<String>) {
    let mut warnings = Vec::new();
    let config = match ssh_config {
        Some(p) => match SshConfig::load(p, user_home().as_deref()) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(format!("{}: {e}", p.display()));
                SshConfig::default()
            }
        },
        None => SshConfig::default(),
    };
    warnings.extend(config.warnings.iter().cloned());
    let own = match servers_file.map(read_servers_file).unwrap_or_else(|| Ok(ServersFile::default())) {
        Ok(f) => f.servers,
        Err(e) => {
            warnings.push(e);
            Vec::new()
        }
    };
    (server::merge(&config, &own), warnings)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SSH socket directory must not be a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.is_dir() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let meta = std::fs::metadata(dir)?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn resolve_program(program: &str) -> PathBuf {
    let p = PathBuf::from(program);
    if p.components().count() > 1 {
        return p;
    }
    codecraft_lsp::which(program).unwrap_or(p)
}

// ---------------------------------------------------------------- operations

/// Read a pipe to the end, keeping at most `cap` bytes.
fn read_capped(mut r: impl Read, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(buf.get(..n.min(room)).unwrap_or(&[]));
            }
        }
    }
    out
}

fn tail(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.get(lines.len().saturating_sub(max_lines)..).unwrap_or(&[]).join("\n")
}

/// Run one program to completion (killed when `cancel` is set). `Ok(stdout)` on exit code 0.
struct ActiveInvocation(u64);
type ActiveInvocations = std::sync::Mutex<BTreeMap<u64, (String, String, Instant)>>;
fn active_invocations() -> &'static ActiveInvocations {
    static ACTIVE: std::sync::OnceLock<ActiveInvocations> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}
impl ActiveInvocation {
    fn start(program: &str, args: &[String]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = Path::new(program).file_name().and_then(|name| name.to_str()).unwrap_or("process");
        let kind = if name == "rsync" { "rsync" } else if args.last().is_some_and(|arg| arg.contains("find .")) { "find" } else { name };
        active_invocations().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id,
            (INVOCATION_HOST.with(|host| host.borrow().clone()), kind.to_owned(), Instant::now()));
        Self(id)
    }
}
impl Drop for ActiveInvocation {
    fn drop(&mut self) { active_invocations().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.0); }
}

fn run_process(
    program: &str,
    args: &[String],
    stdin: Option<&Path>,
    stdout_to: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let started = Instant::now();
    let mut cmd = Command::new(resolve_program(program));
    cmd.args(args).stderr(Stdio::piped());
    match stdin {
        Some(p) => cmd.stdin(Stdio::from(File::open(p).map_err(|e| format!("{}: {e}", p.display()))?)),
        None => cmd.stdin(Stdio::null()),
    };
    match stdout_to {
        Some(p) => cmd.stdout(Stdio::from(File::create(p).map_err(|e| format!("{}: {e}", p.display()))?)),
        None => cmd.stdout(Stdio::piped()),
    };
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {program}: {e}"))?;
    let _active = ActiveInvocation::start(program, args);
    let out = child.stdout.take().map(|o| std::thread::spawn(move || read_capped(o, MAX_OUTPUT)));
    let err = child.stderr.take().map(|e| std::thread::spawn(move || read_capped(e, MAX_STDERR)));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                return Err(format!("{program}: {e}"));
            }
        }
    };
    let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err.and_then(|h| h.join().ok()).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let control_path = args.iter().find_map(|arg| arg.strip_prefix("ControlPath=").map(str::to_owned)).or_else(|| {
        args.iter().find_map(|arg| arg.split_whitespace().find_map(|word| word.strip_prefix("ControlPath=").map(|word| word.trim_matches('\'').to_owned())))
    });
    let name = Path::new(program).file_name().and_then(|name| name.to_str()).unwrap_or("process");
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    log::debug!(target: "codecraft_engine::remote", "{name}: {duration_ms} ms; control socket configured={}; success={}", control_path.is_some(), status.success());
    let mut history = invocations().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    history.push_back(Invocation { host: INVOCATION_HOST.with(|host| host.borrow().clone()), program: name.into(), duration_ms, socket_present: control_path.as_ref().is_some_and(|path| Path::new(path).exists()), control_path, success: status.success() });
    while history.len() > 100 { history.pop_front(); }
    drop(history);
    if status.success() {
        return Ok(stdout);
    }
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    let msg = tail(&String::from_utf8_lossy(&stderr), 6);
    let code = status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "killed".into());
    Err(if msg.is_empty() { format!("{name} failed ({code})") } else { format!("{name} failed ({code}): {msg}") })
}

fn run_steps(steps: &[Step], cancel: &AtomicBool) -> Result<Vec<String>, String> {
    // Serialize the entire transfer, including the snapshot download, against other windows'
    // uploads. Local saves use a separate short-lived lock and can queue during network I/O.
    let transfer = steps.iter().find_map(|step| match step {
        Step::RefreshMirror { local, .. } => Some(transfer_lock(local)),
        Step::CheckedUpload { root, .. } | Step::CheckedTree { root, .. } => Some(transfer_lock(root)),
        _ => None,
    });
    let _transfer_guard = transfer.as_ref().map(|lock| lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    let mut outputs = Vec::new();
    for step in steps {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        match step {
            Step::Output(text) => outputs.push(text.clone()),
            Step::Measure { label, program, args } => {
                let started = Instant::now();
                let result = run_process(program, args, None, None, cancel);
                outputs.push(format!("{label}: {} ms; {}", started.elapsed().as_millis(), match result { Ok(output) => format!("ok {output}"), Err(error) => error }));
            }
            Step::Run { program, args, stdin, stdout_to } => {
                outputs.push(run_process(program, args, stdin.as_deref(), stdout_to.as_deref(), cancel)?);
            }
            Step::CheckedTree { root, server, options, from, to, directory, create, remote_from, remote_to, baseline, rsync } => {
                use codecraft_remote::quote::{quote_path, sh_c};
                let source = baseline.join(from.strip_prefix(root).map_err(|e| e.to_string())?);
                let stage = format!("{remote_from}.codecraft-tree-{}", short_hash(&format!("{}-{}", std::process::id(), now_ms())));
                let p = quote_path(remote_from);
                let absent = |path: &str| format!("test ! -e {} && test ! -L {}", quote_path(path), quote_path(path));
                if !create {
                    if !source.exists() { return Err("No accepted baseline for this path; refresh and compare before changing the remote tree".into()); }
                    let spec = rcmd::RsyncSpec { direction: Direction::Upload, local: &source, remote: &stage, dir: *directory,
                        excludes: &[], dry_run: false, delete: false, update: false, files_from: None };
                    let args = rcmd::rsync_args(server, options, &spec).map_err(|e| e.to_string())?;
                    run_process(rsync, &args, None, None, cancel)?;
                }
                let check = if *create { absent(remote_from) } else if *directory {
                    format!("test -d {p} && test ! -L {p} && diff -qr {p} {} >/dev/null", quote_path(&stage))
                } else { format!("test -f {p} && test ! -L {p} && cmp -s {p} {}", quote_path(&stage)) };
                let action = if *create { format!("mkdir -p -- {p}") }
                    else if let Some(dest) = remote_to { format!("{} && mv -- {p} {}", absent(dest), quote_path(dest)) }
                    else { format!("rm -r -- {p}") };
                let cleanup = format!("rm -r -f -- {}", quote_path(&stage));
                let command = sh_c(&format!("trap {} 0; if {check}; then {action}; else echo 'Remote tree changed: refresh and compare before retrying' >&2; exit 3; fi", codecraft_remote::quote::sh_quote(&cleanup)));
                let args = rcmd::ssh_exec(server, options, &command).map_err(|e| e.to_string())?;
                run_process(&options.program, &args, None, None, cancel)?;
                let lock = mirror_lock(root);
                let _guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if *create { std::fs::create_dir_all(source).map_err(|e| e.to_string())?; }
                else if let Some(to) = to {
                    let dest = baseline.join(to.strip_prefix(root).map_err(|e| e.to_string())?);
                    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
                    std::fs::rename(source, dest).map_err(|e| e.to_string())?;
                } else if *directory { std::fs::remove_dir_all(source).map_err(|e| e.to_string())?; }
                else { std::fs::remove_file(source).map_err(|e| e.to_string())?; }
            }
            Step::CheckedUpload { root, server, options, local, remote, baseline, snapshot } => {
                let lock = mirror_lock(root);
                let guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                if let Some(parent) = snapshot.parent() {
                    create_private_dir(parent).map_err(|e| e.to_string())?;
                }
                codecraft_remote::mirror::capture(local, snapshot).map_err(|e| e.to_string())?;
                let expected = snapshot.with_extension("base");
                let has_base = baseline.is_file();
                if has_base {
                    codecraft_remote::mirror::capture(baseline, &expected).map_err(|e| e.to_string())?;
                }
                drop(guard);
                let token = short_hash(&snapshot.display().to_string());
                let expected_remote = format!("{remote}.codecraft-base-{token}");
                let result = (|| -> Result<String, String> {
                    if has_base {
                        let command = codecraft_remote::quote::sh_c(&format!(
                            "umask 077; {}",
                            rcmd::put_command(&expected_remote)
                        ));
                        let args = rcmd::ssh_exec(server, options, &command).map_err(|e| e.to_string())?;
                        run_process(&options.program, &args, Some(&expected), None, cancel)?;
                    }
                    let command =
                        rcmd::checked_put_command(remote, has_base.then_some(expected_remote.as_str()), &token);
                    let args = rcmd::ssh_exec(server, options, &command).map_err(|e| e.to_string())?;
                    run_process(&options.program, &args, Some(snapshot), None, cancel)
                })();
                if result.is_ok() {
                    let _guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    codecraft_remote::mirror::capture(snapshot, baseline).map_err(|e| e.to_string())?;
                }
                let _ = std::fs::remove_file(snapshot);
                let _ = std::fs::remove_file(expected);
                outputs.push(result?);
            }
            Step::RefreshMirror { local, incoming, baseline, protected } => {
                let lock = mirror_lock(local);
                let report = codecraft_remote::mirror::refresh_coordinated(local, incoming, baseline, |rel, apply| {
                    let _guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if cancel.load(Ordering::Relaxed) { return Err(codecraft_remote::RemoteError::Invalid("cancelled".into())); }
                    let dirty = protected.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(rel);
                    let paths = if dirty { std::collections::BTreeSet::from([rel.to_path_buf()]) } else { Default::default() };
                    apply(&paths)
                }).map_err(|e| e.to_string())?;
                outputs.push(serde_json::to_string(&report).map_err(|e| e.to_string())?);
            }
            Step::Mkdir(p) => std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?,
            Step::Rename { from, to } => {
                if let Some(dir) = to.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                }
                std::fs::rename(from, to).map_err(|e| format!("{}: {e}", to.display()))?;
            }
            Step::Write { path, text } => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                }
                std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
    }
    Ok(outputs)
}

/// Start an operation on a worker thread. `logged` operations show in File Transfer.
pub(crate) fn start_op(
    s: &mut Session,
    server: &str,
    summary: impl Into<String>,
    steps: Vec<Step>,
    follow: Follow,
    logged: bool,
) -> Result<u64, CommandError> {
    let r = &mut s.remote;
    if logged || matches!(follow, Follow::Listing { .. } | Follow::OpenRemote { .. }) {
        r.poll_idle = 0;
        r.last_refresh = Instant::now();
    }
    let id = r.next();
    let tx = r.tx.clone();
    let cancel = r.cancel.clone();
    let host = server.to_owned();
    std::thread::Builder::new()
        .name(format!("remote-{id}"))
        .spawn(move || {
            INVOCATION_HOST.with(|current| *current.borrow_mut() = host);
            let result = run_steps(&steps, &cancel);
            let _ = tx.send(OpDone { id, result });
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    r.ops.insert(id, (follow, logged));
    r.op_started.insert(id, Instant::now());
    r.op_hosts.insert(id, server.to_owned());
    let summary = summary.into();
    if logged {
        r.push_log(TransferEntry {
            id,
            time_ms: now_ms(),
            server: server.to_string(),
            summary: summary.clone(),
            status: TransferStatus::Running,
            output: String::new(),
        });
        s.status = format!("{summary}…");
    }
    Ok(id)
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    s.db.tunnels.configure(crate::ssh_tunnel::TunnelEnv {
        ssh: s.remote.programs.ssh.clone(),
        ssh_config: s.remote.ssh_config_path(),
        servers_file: s.remote.servers_file(),
    });
    while let Ok(done) = s.remote.rx.try_recv() {
        changed = true;
        let Some((follow, logged)) = s.remote.ops.remove(&done.id) else { continue };
        s.remote.op_hosts.remove(&done.id);
        s.remote.op_started.remove(&done.id);
        if logged && let Some(e) = s.remote.log.iter_mut().find(|e| e.id == done.id) {
            match &done.result {
                Ok(outs) => {
                    e.status = TransferStatus::Done;
                    let text = outs.join("\n");
                    e.output = text.chars().take(64 * 1024).collect();
                    s.status = format!("{}: done", e.summary);
                }
                Err(err) => {
                    e.status = TransferStatus::Failed(err.clone());
                    let title = e.summary.clone();
                    s.notify(NotificationLevel::Error, &title, err.clone());
                }
            }
        }
        finish(s, follow, done.result);
    }
    if s.remote.ops.is_empty() && let Some((server, step)) = s.remote.tree_queue.pop_front() {
        if let Err(error) = start_op(s, &server, "Apply checked remote tree operation", vec![step], Follow::Reload, true) {
            s.notify(NotificationLevel::Error, "Remote tree operation", error.to_string());
        }
        changed = true;
    }
    // A mirror's saves and refreshes share one queue. Snapshot and check each upload against
    // the last accepted remote contents; a failure pauses the queue without discarding edits.
    if !s.remote.busy() && s.remote.conflicts.is_empty()
        && s.remote
            .project
            .as_ref()
            .is_some_and(|p| p.connection == ConnectionState::Connected && !p.pending_uploads.is_empty())
    {
        let automatic =
            s.project_root().and_then(|root| crate::deployment::load(&root).ok()).is_some_and(|cfg| cfg.auto_upload);
        let next = s
            .remote
            .project
            .as_mut()
            .filter(|p| p.connection == ConnectionState::Connected && automatic)
            .and_then(|p| p.pending_uploads.pop_first().map(|path| (p.server.clone(), path)));
        if let Some((server, local)) = next {
            let result = (|| -> Result<(), CommandError> {
                let root = s.project_root().ok_or_else(|| failed("remote project is closed"))?;
                let rel = codecraft_remote::deploy::rel_path(&root, &local)
                    .ok_or_else(|| failed("pending upload is outside the project"))?;
                let cfg = crate::deployment::load(&root).map_err(failed)?;
                let dep = cfg
                    .server(&server)
                    .ok_or_else(|| failed("restore the remote project's deployment mapping before uploading"))?;
                if let Some(remote) = dep.remote_for(&rel) {
                    crate::deployment::upload_file_op(s, &server, &local, &remote, "Upload")?;
                } else {
                    persist_uploads(s);
                }
                Ok(())
            })();
            if let Err(error) = result
                && let Some(project) = &mut s.remote.project
            {
                project.pending_uploads.insert(local);
                project.connection = ConnectionState::Failed;
                project.error = Some(error.to_string());
                persist_uploads(s);
            }
            changed = true;
        }
    }
    for key in s.remote.masters.keys().cloned().collect::<Vec<_>>() {
        if let Some(master) = s.remote.masters.get_mut(&key)
            && master.child.as_mut().is_none_or(|child| matches!(child.try_wait(), Ok(Some(_)))) {
                let reason = master.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                if !reason.is_empty() && let Some(project) = &mut s.remote.project {
                    let warning = format!("multiplexing unavailable: {}", tail(&reason, 3));
                    if project.error.as_ref() != Some(&warning) { project.error = Some(warning); changed = true; }
                }
        }
    }
    if s.remote.project.as_ref().is_some_and(|project| project.connection == ConnectionState::Connected) {
        let due = s.remote.lsp_retry.iter().filter(|(_, (_, at))| *at <= Instant::now()).map(|(kind, _)| *kind).collect::<Vec<_>>();
        for kind in due {
            if interactive_busy(s) || s.remote.helpers_disabled { continue; }
            s.remote.lsp_retry.remove(&kind);
            s.server_status.remove(&kind);
            s.ensure_server(kind);
        }
    }
    apply_reload(s);
    maintain_watcher(s);
    let interactive = interactive_busy(s);
    if let Some((_, streaming, dirty)) = &s.remote.watcher
        && streaming.load(Ordering::Acquire) && dirty.swap(false, Ordering::AcqRel) {
        s.remote.debounce.event(Instant::now());
        s.remote.debounce.reason = "inotify filesystem event";
    }
    if s.remote.debounce.due(Instant::now()) { s.remote.refresh_pending = true; }
    // A listing may have started while a probe was in flight. Retain the detected change
    // until the interactive operation finishes instead of dropping a busy refresh request.
    if s.remote.auto_refresh && !s.remote.helpers_disabled && !interactive && s.remote.refresh_pending && s.remote.ops.is_empty() && s.remote.tree_queue.is_empty()
        && s.remote.project.as_ref().is_some_and(|project| project.connection == ConnectionState::Connected) {
        s.remote.debounce.started(Instant::now());
        if let Err(error) = s.execute("remote.syncFromRemote", &Value::Null) {
            s.remote.refresh_pending = false;
            s.notify(NotificationLevel::Error, "Automatic remote refresh", error.to_string());
        }
        return true;
    }
    let interval = poll_interval(s.remote.poll_interval_secs, s.remote.poll_idle, s.remote.window_focused);
    if s.remote.auto_refresh && !s.remote.helpers_disabled && !interactive && s.remote.auto_refresh_mode() != "inotify"
        && s.remote.poll_interval_secs != 0 && s.remote.ops.is_empty() && s.remote.tree_queue.is_empty()
        && s.remote.project.as_ref().is_some_and(|p| p.connection == ConnectionState::Connected)
        && s.remote.last_refresh.elapsed() >= interval {
        s.remote.last_refresh = Instant::now();
        if let Err(error) = auto_probe(s) { s.notify(NotificationLevel::Warning, "Automatic remote refresh", error.to_string()); }
        changed = true;
    }
    changed
}

pub(crate) fn interactive_busy(s: &Session) -> bool {
    s.terminal.tabs.iter().any(|tab| tab.remote && tab.alive() && tab.term.recently_active(Duration::from_secs(3)))
}

type ReloadFiles = Vec<(PathBuf, String, Option<std::time::SystemTime>, Vec<(codecraft_workspace::DocId, u64)>)>;
fn start_reload(s: &mut Session, paths: Vec<PathBuf>) {
    let paths = paths.into_iter().filter_map(|path| {
        let versions: Vec<_> = s.ws.docs().filter(|doc| doc.path.as_ref() == Some(&path)).map(|doc| (doc.id, doc.doc.revision())).collect();
        (!versions.is_empty()).then_some((path, versions))
    }).collect::<Vec<_>>();
    let (tx, rx) = crossbeam_channel::bounded(1);
    if std::thread::Builder::new().name("remote-reload".into()).spawn(move || {
        for (path, versions) in paths {
            let Ok(text) = codecraft_fs::read_text_file(&path) else { continue };
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            // One buffered file, rather than retaining every open file's text at once.
            if tx.send(vec![(path, text, mtime, versions)]).is_err() { break; }
        }
    }).is_ok() { s.remote.reload = Some(rx); }
    s.remote.tree_refreshes = s.remote.tree_refreshes.saturating_add(1);
    s.jobs_rescan();
}
fn apply_reload(s: &mut Session) {
    let Some(rx) = s.remote.reload.as_ref() else { return };
    let files = match rx.try_recv() {
        Ok(files) => files,
        Err(crossbeam_channel::TryRecvError::Empty) => return,
        Err(crossbeam_channel::TryRecvError::Disconnected) => { s.remote.reload = None; return; }
    };
    for (path, text, mtime, versions) in files {
        for (id, revision) in versions {
            if s.ws.doc(id).is_none_or(|doc| doc.path.as_ref() != Some(&path) || doc.doc.is_modified() || doc.doc.revision() != revision) { continue; }
            let editor = s.ws.editor_for_doc(id);
            let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
            if let Some(doc) = s.ws.doc_mut(id) {
                if doc.doc.replace_all(&text, &sels).is_ok() { doc.doc.mark_saved(); }
                doc.disk_mtime = mtime;
            }
            if let Some(editor) = editor {
                let len = s.ws.doc(id).map_or(0, |d| d.doc.len_chars());
                s.ws.set_selections(editor, sels.clamp(len));
            }
        }
    }
}

pub(crate) fn protect_buffers(s: &Session) {
    if s.remote.project.is_none() { return; }
    let paths = s.project_root().map(|root| s.ws.docs().filter(|d| d.doc.is_modified())
        .filter_map(|d| d.path.as_ref()?.strip_prefix(&root).ok().map(Path::to_path_buf)).collect()).unwrap_or_default();
    *s.remote.protected.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = paths;
}

fn finish(s: &mut Session, follow: Follow, result: Result<Vec<String>, String>) {
    match follow {
        Follow::Nothing => {}
        Follow::LspProbe { server, generation, kind } => {
            if s.remote.project.as_ref().is_none_or(|project| project.server != server) || s.remote.control_generation.get(&server).copied().unwrap_or(0) != generation { return; }
            let tool = result.ok().and_then(|out| out.last().cloned()).and_then(|out| out.lines().last().map(str::to_owned)).filter(|tool| matches!(tool.as_str(), "rust-analyzer" | "clangd" | "basedpyright-langserver" | "pyright-langserver"));
            s.remote.lsp_tools.insert((server.clone(), kind), tool.clone());
            if tool.is_some() {
                if interactive_busy(s) { s.remote.lsp_retry.insert(kind, (0, Instant::now() + Duration::from_secs(3))); }
                else { s.restart_server(Some(kind)); }
            }
            else {
                let message = format!("{} is not installed on {server}. Install: {}. Run remote.lsp.install, or remote.lsp.localFallback to analyse the local mirror without the remote toolchain.", kind.label(), remote_install_hint(kind));
                s.set_status(kind, crate::LspStatus::Unavailable(message.clone()));
                s.notify(NotificationLevel::Warning, &format!("{} is not installed on {server}", kind.label()), message);
            }
        }
        Follow::LspInstall { kind } => {
            if result.is_ok() {
                if let Some(project) = &s.remote.project { s.remote.lsp_tools.remove(&(project.server.clone(), kind)); }
                s.restart_server(Some(kind));
            }
        }
        Follow::AutoProbe => match result {
            Ok(output) => {
                let signature = output.join("\n").trim().to_owned();
                let changed = s.remote.poll_signature.as_ref().is_some_and(|old| old != &signature);
                s.remote.poll_signature = Some(signature);
                s.remote.poll_idle = if changed { 0 } else { s.remote.poll_idle.saturating_add(1).min(3) };
                if changed { s.remote.debounce.event(Instant::now()); s.remote.debounce.reason = "metadata fingerprint changed"; }
            }
            Err(error) => {
                *s.remote.watcher_reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = format!("poll failed: {error}");
                s.remote.poll_idle = 3;
            },
        },
        Follow::ProjectUpload { local } => {
            let conflict = result.as_ref().err().is_some_and(|error| error.contains("Remote file changed"));
            if result.is_ok() {
                s.lsp_did_save(&local);
                if let Some(project) = &mut s.remote.project { project.error = None; }
            }
            if let Err(error) = result
                && let Some(project) = &mut s.remote.project
                && s.ws.project.as_ref().is_some_and(|p| local.starts_with(&p.root))
            {
                project.pending_uploads.insert(local.clone());
                project.connection = if conflict { ConnectionState::Connected } else { ConnectionState::Failed };
                project.error = Some(error);
            }
            if conflict {
                if let Some(rel) = s.project_root().and_then(|root| local.strip_prefix(root).ok().map(Path::to_path_buf))
                    && !s.remote.conflicts.contains(&rel) { s.remote.conflicts.push(rel); }
                // Pause uploads, fetch the incoming copy, and leave resolution usable without
                // a reconnect/retry loop. Transport failures still pause the disconnected queue.
                if let Err(error) = s.execute("remote.syncFromRemote", &Value::Null) {
                    s.notify(NotificationLevel::Error, "Fetch remote conflict", error.to_string());
                }
            }
            persist_uploads(s);
        }
        Follow::Reload => {
            if result.is_ok() {
                let _ = s.execute("file.synchronize", &Value::Null);
            }
        }
        Follow::Listing { server, path } => {
            let b = &mut s.remote.browser;
            if b.server.as_deref() != Some(server.as_str()) || b.path != path {
                return;
            }
            b.loading = false;
            match result {
                Ok(outs) => {
                    let (pwd, entries) = listing::parse_pwd_ls(outs.last().map(String::as_str).unwrap_or(""));
                    if let Some(pwd) = pwd {
                        b.path = pwd;
                    }
                    b.entries = entries;
                    b.error = None;
                    if b.selected.as_ref().is_some_and(|n| !b.entries.iter().any(|e| &e.name == n)) {
                        b.selected = None;
                    }
                }
                Err(e) => {
                    b.entries.clear();
                    b.error = Some(e);
                }
            }
        }
        Follow::RefreshBrowser => {
            if let Err(e) = &result {
                s.remote.browser.error = Some(e.clone());
            }
            let _ = refresh_browser(s);
        }
        Follow::ExternalSource { local, line, column, utf16 } => {
            let column = if utf16 {
                codecraft_fs::read_text_file(&local).ok().map(|text| {
                    let document = codecraft_text::Document::from_text(&text);
                    let offset = document.utf16_to_offset(u32::try_from(line).unwrap_or(u32::MAX), u32::try_from(column).unwrap_or(u32::MAX));
                    document.offset_to_position(offset).column
                }).unwrap_or(0)
            } else { column };
            if result.is_ok() && let Err(error) = s.navigate_to_position(&local, line, column) {
                s.notify(NotificationLevel::Error, "Cannot open remote source", error.to_string());
            }
        }
        Follow::OpenRemote { server, remote, local } => {
            if result.is_ok() {
                s.remote.opened.insert(crate::normalize_path(&local), (server, remote));
                if let Err(e) = s.open_file(&local) {
                    s.notify(NotificationLevel::Error, "Cannot open the downloaded file", e.to_string());
                }
            }
        }
        Follow::Compare { server, remote, local, fetched } => {
            if result.is_err() {
                return;
            }
            let read = |p: &Path| codecraft_fs::read_text_file(p).map_err(|e| e.to_string());
            match (read(&local), read(&fetched)) {
                (Ok(left), Ok(right)) => {
                    let name = local.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                    s.misc.compare = Some(crate::misc::CompareView {
                        title: format!("{name}: local vs {server}"),
                        left_title: local.display().to_string(),
                        right_title: format!("{server}:{remote}"),
                        left,
                        right,
                    });
                }
                (Err(e), _) | (_, Err(e)) => s.notify(NotificationLevel::Error, "Cannot compare", e),
            }
            let _ = std::fs::remove_file(&fetched);
        }
        Follow::SyncPreview { server, local, remote, excludes } => {
            let Ok(outs) = result else { return };
            // Outputs: [mkdir, upload preview, download preview].
            let mut items = Vec::new();
            for (out, direction) in outs.iter().skip(1).zip([Direction::Upload, Direction::Download]) {
                for c in rsync::significant(&rsync::parse_itemize(out)) {
                    // A new directory shows up as its files too.
                    if c.change == Change::Create && c.kind == codecraft_remote::ItemKind::Dir {
                        continue;
                    }
                    items.push(SyncItem {
                        path: c.path,
                        direction,
                        change: c.change,
                        is_dir: c.kind == codecraft_remote::ItemKind::Dir,
                        selected: true,
                    });
                }
            }
            // Changed on both sides (same time, different size): upload unless chosen otherwise.
            let uploads: Vec<String> =
                items.iter().filter(|i| i.direction == Direction::Upload).map(|i| i.path.clone()).collect();
            for i in items.iter_mut().filter(|i| i.direction == Direction::Download) {
                if uploads.contains(&i.path) {
                    i.selected = false;
                }
            }
            let n = items.len();
            s.remote.sync_preview = Some(SyncPreview { server, local, remote, items, excludes });
            s.status = if n == 0 { "Sync: no differences".into() } else { format!("Sync: {n} difference(s)") };
        }
        Follow::Test { server } => {
            let absolute = result.as_ref().ok().and_then(|outputs| outputs.iter().flat_map(|s| s.lines())
                .find_map(|line| line.strip_prefix("CODECRAFT-ROOT=").filter(|p| p.starts_with('/')).map(str::to_owned)));
            if let Some(signature) = result.as_ref().ok().and_then(|outputs| outputs.iter().flat_map(|out| out.lines()).find_map(|line| line.strip_prefix("CODECRAFT-FINGERPRINT="))) {
                s.remote.poll_signature = Some(signature.to_owned());
                s.remote.poll_idle = 0;
                s.remote.last_refresh = Instant::now();
            }
            let restart = absolute.is_some() && s.remote.project.as_ref().is_some_and(|p| p.server == server);
            if restart { s.remote.absolute_root = absolute; }
            let status = match result {
                Ok(_) => TestStatus::Ok("Connected".into()),
                Err(e) => TestStatus::Failed(e),
            };
            s.status = match &status {
                TestStatus::Ok(_) => format!("{server}: connection successful"),
                TestStatus::Failed(e) => format!("{server}: {e}"),
                TestStatus::Running => String::new(),
            };
            if let Some(p) = &mut s.remote.project
                && p.server == server
            {
                match &status {
                    TestStatus::Ok(_) => {
                        p.connection = ConnectionState::Connected;
                        p.error = None;
                    }
                    TestStatus::Failed(e) => {
                        p.connection = ConnectionState::Failed;
                        p.error = Some(e.clone());
                    }
                    TestStatus::Running => p.connection = ConnectionState::Connecting,
                }
            }
            s.remote.tests.insert(server, status);
            if restart && s.lsp_enabled {
                let kinds: std::collections::BTreeSet<_> = s.servers.keys().copied()
                    .chain(s.ws.docs().filter_map(|d| crate::lsp::server_kind(d.language))).collect();
                stop_project_servers(s);
                if let Some(root) = s.project_root() { s.start_lsp(&root); }
                for kind in kinds { s.ensure_server(kind); }
            }
        }
        Follow::RemoteDev { server, remote, local, open_window } => match result {
            Ok(outputs) => {
                s.remote.refresh_pending = false;
                if let Some(report) = outputs.last().and_then(|t| serde_json::from_str::<codecraft_remote::mirror::Refresh>(t).ok()) {
                    let paths: Vec<_> = report.changed.iter().map(|p| local.join(p)).collect();
                    if !paths.is_empty() { s.remote_files_changed(&paths); start_reload(s, paths); stop_watcher(&mut s.remote); }
                    s.remote.conflicts = report.conflicts.clone();
                    if !report.conflicts.is_empty() {
                    s.notify(NotificationLevel::Warning, "Remote sync conflicts", format!(
                            "Preserved {} local file(s): {}. Incoming versions are in the sibling .snapshots folder. Compare before uploading.",
                            report.conflicts.len(), report.conflicts.iter().take(10).map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                        ));
                }
                }
                if open_window {
                    crate::remote_cmds::remote_dev_ready(s, &server, &remote, &local);
                }
            }
            Err(error) => {
                if let Some(d) = &mut s.remote.remote_dev {
                    d.error = Some(error.clone());
                }
                if let Some(p) = &mut s.remote.project {
                    p.connection = ConnectionState::Failed;
                    p.error = Some(error);
                }
            }
        },
    }
}

/// List the browser's folder again.
pub(crate) fn refresh_browser(s: &mut Session) -> Result<(), CommandError> {
    let Some(name) = s.remote.browser.server.clone() else { return Ok(()) };
    let path = s.remote.browser.path.clone();
    let server = s.remote.server(&name).ok_or_else(|| CommandError::Failed(format!("unknown SSH server `{name}`")))?;
    let opts = s.remote.ssh_options(&name);
    s.remote.ensure_master(&server);
    let args = rcmd::ssh_exec(&server, &opts, &rcmd::pwd_ls_command(&path)).map_err(failed)?;
    s.remote.browser.loading = true;
    s.remote.browser.error = None;
    start_op(
        s,
        &name,
        format!("List {path}"),
        vec![Step::run(&opts.program, args)],
        Follow::Listing { server: name.clone(), path },
        false,
    )?;
    s.ws.layout.show(ToolWindowId::RemoteHost);
    Ok(())
}

pub(crate) fn failed(e: impl std::fmt::Display) -> CommandError {
    CommandError::Failed(e.to_string())
}

/// After files were saved: upload the ones opened from Remote Host, and (Automatic Upload) the
/// project's mapped files to the default deployment server.
pub(crate) fn after_save(s: &mut Session, saved: &[PathBuf]) {
    if saved.is_empty() {
        return;
    }
    if s.remote.project.is_some() {
        let paths = s
            .project_root()
            .and_then(|root| {
                let cfg = crate::deployment::load(&root).ok()?;
                if !cfg.auto_upload {
                    return None;
                }
                let dep = cfg.default_deployment()?;
                Some(
                    saved
                        .iter()
                        .filter(|path| {
                            codecraft_remote::deploy::rel_path(&root, path)
                                .and_then(|rel| dep.remote_for(&rel))
                                .is_some()
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();
        if let Some(project) = &mut s.remote.project {
            project.pending_uploads.extend(paths);
            if project.connection != ConnectionState::Connected {
                s.status = "Remote upload paused: reconnect and review remote changes before uploading".into();
            }
        }
        persist_uploads(s);
        return;
    }
    let mut remote_files: Vec<(String, String, PathBuf)> = Vec::new();
    for p in saved {
        let key = crate::normalize_path(p);
        if let Some((server, remote)) = s.remote.opened.get(&key) {
            remote_files.push((server.clone(), remote.clone(), key));
        }
    }
    let mut auto: Vec<(String, String, PathBuf)> = Vec::new();
    if let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone())
        && let Ok(cfg) = crate::deployment::load(&root)
        && cfg.auto_upload
        && let Some(dep) = cfg.default_deployment()
    {
        for p in saved {
            let key = crate::normalize_path(p);
            if remote_files.iter().any(|(_, _, l)| l == &key) {
                continue;
            }
            if let Some(rel) = codecraft_remote::deploy::rel_path(&root, &key)
                && let Some(remote) = dep.remote_for(&rel)
            {
                auto.push((dep.server.clone(), remote, key));
            }
        }
    }
    for (server, remote, local) in remote_files.into_iter().chain(auto) {
        if let Err(e) = crate::deployment::upload_file_op(s, &server, &local, &remote, "Upload") {
            s.notify(NotificationLevel::Error, "Automatic upload failed", e.to_string());
        }
    }
}

/// Stop background work and close every connection (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    stop_watcher(&mut s.remote);
    s.remote.cancel.store(true, Ordering::Relaxed);
    let masters: Vec<Master> = s.remote.masters.drain().map(|(_, m)| m).collect();
    for mut m in masters {
        std::thread::spawn(move || {
            // Ask the master to exit (removes its socket), then make sure it is gone.
            if let Ok(Some(args)) = rcmd::ssh_exit_master(&m.server, &m.options)
                && let Ok(mut c) = Command::new(resolve_program(&m.options.program))
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            {
                let deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < deadline && matches!(c.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Some(c) = &mut m.child {
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Some(p) = &m.options.control_path {
                let _ = std::fs::remove_file(p);
            }
        });
    }
}

/// Add or replace a CodeCraft-defined server in `ssh.json`.
pub(crate) fn add_server(s: &mut Session, cfg: ServerConfig) -> Result<(), CommandError> {
    cfg.validate().map_err(failed)?;
    let mut file = s.remote.own_servers().map_err(CommandError::Failed)?;
    let name = cfg.name.trim().to_string();
    file.servers.retain(|c| c.name.trim() != name);
    if file.servers.len() >= 500 {
        return Err(CommandError::Failed("too many servers".into()));
    }
    file.servers.push(ServerConfig { name, ..cfg });
    s.remote.save_own_servers(&file).map_err(CommandError::Failed)
}

/// Whether the File Transfer log has a running entry.
pub fn transfers_running(s: &Session) -> bool {
    s.remote.log.iter().any(|e| e.status == TransferStatus::Running)
}

/// Restore the remote marker when a cached mirror is opened (including Recent Projects).
pub(crate) fn project_opened(s: &mut Session, root: &Path) {
    stop_watcher(&mut s.remote);
    s.remote.project = None;
    s.remote.reload = None;
    s.remote.debounce = Default::default();
    s.remote.watcher_reason = Default::default();
    s.remote.conflicts.clear();
    s.remote.absolute_root = None;
    s.remote.last_refresh = Instant::now();
    let Ok(cfg) = crate::deployment::load(root) else { return };
    s.remote.refresh_pending = false;
    s.remote.auto_refresh = !cfg.pause_remote_refresh;
    s.remote.helpers_disabled = cfg.disable_remote_helpers;
    s.remote.poll_interval_secs = cfg.remote_poll_interval_secs.unwrap_or(3);
    let Some(name) = cfg.remote_development.clone() else { return };
    let Some(remote) = cfg.server(&name).and_then(|d| d.remote_for("")) else { return };
    let destination = s
        .remote
        .server(&name)
        .map(|server| match server.user {
            Some(user) => format!("{user}@{}", server.host),
            None => server.host,
        })
        .unwrap_or_else(|| name.clone());
    let pending_uploads = match load_uploads(root) {
        Ok(paths) => paths,
        Err(error) => {
            s.notify(
                NotificationLevel::Warning,
                "Cannot restore pending remote uploads",
                format!("{error}. Saved files remain local; review and upload them explicitly."),
            );
            Default::default()
        }
    };
    s.remote.project = Some(RemoteProject {
        server: name,
        destination,
        remote,
        connection: ConnectionState::Cached,
        error: None,
        pending_uploads,
    });
    let _ = s.execute("remote.project.reconnect", &Value::Null);
}

/// Close this host's master off the UI thread. Other hosts remain usable.
pub(crate) fn disconnect_master(s: &mut Session, server: &str) {
    s.remote.lsp_tools.retain(|(host, _), _| host != server);
    s.remote.local_lsp.clear();
    s.remote.lsp_retry.clear();
    s.remote.lsp_failures.clear();
    s.remote.poll_idle = 0;
    stop_watcher(&mut s.remote);
    if !server.ends_with("\0bulk") { disconnect_master(s, &format!("{server}\0bulk")); }
    let generation = s.remote.control_generation.entry(server.into()).or_default();
    *generation = generation.saturating_add(1);
    if let Some(mut master) = s.remote.masters.remove(server) {
        std::thread::spawn(move || {
            if let Ok(Some(args)) = rcmd::ssh_exit_master(&master.server, &master.options) {
                let _ = run_process(&master.options.program, &args, None, None, &AtomicBool::new(false));
            }
            if let Some(child) = &mut master.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(path) = master.options.control_path {
                let _ = std::fs::remove_file(path);
            }
        });
    }
}

/// Persistent snapshots used by both refresh and checked uploads.
pub(crate) fn mirror_paths(local: &Path) -> Result<(PathBuf, PathBuf), CommandError> {
    let parent = local.parent().ok_or_else(|| failed("remote cache has no parent"))?;
    let leaf = local.file_name().ok_or_else(|| failed("remote cache has no name"))?;
    Ok((parent.join(".snapshots").join(leaf), parent.join(".baselines").join(leaf)))
}

/// Serialize mirror writes across project windows. UI saves use try_lock and retain the buffer
/// when a worker owns the mirror; only workers may wait for the lock.
type MirrorLocks = std::sync::Mutex<BTreeMap<PathBuf, std::sync::Weak<std::sync::Mutex<()>>>>;

pub(crate) fn mirror_lock(root: &Path) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<MirrorLocks> = std::sync::OnceLock::new();
    shared_lock(&LOCKS, root)
}

fn transfer_lock(root: &Path) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<MirrorLocks> = std::sync::OnceLock::new();
    shared_lock(&LOCKS, root)
}

fn shared_lock(locks: &std::sync::OnceLock<MirrorLocks>, root: &Path) -> Arc<std::sync::Mutex<()>> {
    let mut locks = locks.get_or_init(Default::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = crate::normalize_path(root);
    if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(std::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

const PENDING_UPLOADS: &str = ".codecraft/remote-pending.json";

fn load_uploads(root: &Path) -> Result<std::collections::BTreeSet<PathBuf>, String> {
    let file = match File::open(root.join(PENDING_UPLOADS)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.to_string()),
    };
    let mut text = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() > 1024 * 1024 {
        return Err("pending remote uploads exceed the 1 MiB metadata limit".into());
    }
    let paths: Vec<String> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if paths.len() > 10000 {
        return Err("pending remote uploads exceed 10000 files".into());
    }
    let mut result = std::collections::BTreeSet::new();
    for rel in paths {
        let path = crate::normalize_path(&root.join(&rel));
        if codecraft_remote::deploy::rel_path(root, &path).is_none_or(|p| p.is_empty()) {
            return Err("pending remote upload path is outside the project".into());
        }
        result.insert(path);
    }
    Ok(result)
}

pub(crate) fn persist_uploads(s: &mut Session) {
    let Some(root) = s.project_root() else { return };
    let Some(project) = &s.remote.project else { return };
    let paths: Vec<_> =
        project.pending_uploads.iter().filter_map(|p| codecraft_remote::deploy::rel_path(&root, p)).collect();
    let result = serde_json::to_string(&paths)
        .map_err(|e| e.to_string())
        .and_then(|text| write_atomic(&root.join(PENDING_UPLOADS), &text));
    if let Err(error) = result {
        s.notify(NotificationLevel::Error, "Cannot remember pending remote uploads", error);
    }
}

/// Stop remote language services without waiting for their shutdown handshakes on the UI thread.
pub(crate) fn stop_project_servers(s: &mut Session) {
    s.remote.lsp_started.clear();
    for server in std::mem::take(&mut s.servers).into_values() {
        std::thread::spawn(move || server.shutdown());
    }
    s.server_status.clear();
    s.lsp_status = crate::LspStatus::Off;
    for doc in s.ws.docs_mut() { doc.lsp_synced = None; }
}

pub(crate) fn external_cache(s: &Session) -> Option<PathBuf> {
    let project = s.remote.project.as_ref()?;
    Some(s.remote.cache_dir()?.join(".sources").join(safe_name(&project.server)))
}

/// Translate server configuration into an SSH stdio process; no network work runs here.
pub(crate) fn lsp_transport(s: &mut Session, kind: codecraft_lsp::ServerKind, config: &mut codecraft_lsp::ServerConfig)
    -> Result<Arc<dyn codecraft_lsp::path_mapper::PathMapper>, CommandError>
{
    let project = s.remote.project.clone().ok_or_else(|| failed("no remote project"))?;
    let remote = s.remote.absolute_root.clone().ok_or_else(|| failed("Reconnect to resolve the remote workspace before starting language services"))?;
    if project.connection != ConnectionState::Connected { return Err(failed("remote language services are paused; Reconnect to resume")); }
    let server = crate::deployment::server(s, &project.server)?;
    let opts = s.remote.ssh_options(&project.server);
    let tool = match kind {
        codecraft_lsp::ServerKind::RustAnalyzer => "rust-analyzer",
        codecraft_lsp::ServerKind::Clangd => "clangd",
        codecraft_lsp::ServerKind::Python => "basedpyright-langserver",
        _ => return Err(failed("this language has no SSH language transport yet")),
    };
    let tool = s.remote.lsp_tools.get(&(project.server.clone(), kind)).and_then(|tool| tool.as_deref()).unwrap_or(tool);
    s.remote.lsp_started.insert(kind, Instant::now());
    let launch = format!("exec {tool}{}", if kind == codecraft_lsp::ServerKind::Python { " --stdio" } else { "" });
    let script = format!("{}{LSP_PATH}{}", codecraft_remote::target_script::prelude(&remote, &[]).map_err(failed)?, launch);
    config.command = resolve_program(&opts.program);
    config.args = rcmd::ssh_exec(&server, &opts, &codecraft_remote::quote::sh_c(&script)).map_err(failed)?;
    config.args.insert(0, "-T".into());
    let mut mapper = codecraft_lsp::path_mapper::RootMapper::new(&config.root.to_string_lossy(), &remote);
    if let Some(cache) = external_cache(s) { mapper = mapper.with_external_cache(&cache.to_string_lossy()); }
    Ok(Arc::new(mapper))
}

/// Fetch external source files on demand. Cache files never participate in deployment saves.
pub(crate) fn fetch_external(s: &mut Session, local: &Path, line: usize, column: usize) -> Result<bool, CommandError> {
    fetch_external_position(s, local, line, column, false)
}

pub(crate) fn fetch_external_lsp(s: &mut Session, local: &Path, line: usize, column: usize) -> Result<bool, CommandError> {
    fetch_external_position(s, local, line, column, true)
}

fn fetch_external_position(s: &mut Session, local: &Path, line: usize, column: usize, utf16: bool) -> Result<bool, CommandError> {
    let Some(cache) = external_cache(s) else { return Ok(false) };
    let Ok(rel) = local.strip_prefix(&cache) else { return Ok(false) };
    if local.is_file() { return Ok(false); }
    if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) || rel.as_os_str().is_empty() {
        return Err(failed("invalid remote source cache path"));
    }
    let project = s.remote.project.clone().ok_or_else(|| failed("remote project closed"))?;
    let host = crate::deployment::server(s, &project.server)?;
    let opts = s.remote.ssh_options(&project.server);
    let remote = format!("/{}", rel.to_string_lossy());
    let q = codecraft_remote::quote::quote_path(&remote);
    let script = codecraft_remote::quote::sh_c(&format!("test -f {q} && test ! -L {q} && test \"$(wc -c < {q})\" -le {MAX_OPEN_BYTES} && head -c {MAX_OPEN_BYTES} -- {q}"));
    let args = rcmd::ssh_exec(&host, &opts, &script).map_err(failed)?;
    let parent = local.parent().ok_or_else(|| failed("cache source has no parent"))?.to_path_buf();
    let temp = local.with_extension("codecraft-fetch");
    start_op(s, &project.server, format!("Fetch remote source {remote}"), vec![Step::Mkdir(parent),
        Step::Run { program: opts.program, args, stdin: None, stdout_to: Some(temp.clone()) },
        Step::Rename { from: temp, to: local.to_path_buf() }],
        Follow::ExternalSource { local: local.to_path_buf(), line, column, utf16 }, true)?;
    Ok(true)
}

/// Queue resource operations after their local transaction. A failed check preserves the local
/// result and the old baseline; the transfer log tells the user how to reconcile it.
pub(crate) fn tree_operation(s: &mut Session, from: &Path, to: Option<&Path>, directory: bool, create: bool) -> Result<(), CommandError> {
    let Some(project) = s.remote.project.clone() else { return Ok(()) };
    let root = s.project_root().ok_or_else(|| failed("project closed"))?;
    let relative = codecraft_remote::deploy::rel_path(&root, from).filter(|p| !p.is_empty()).ok_or_else(|| failed("tree operation is outside the remote project"))?;
    let cfg = crate::deployment::load(&root).map_err(failed)?;
    let dep = cfg.server(&project.server).ok_or_else(|| failed("restore deployment mapping"))?;
    let remote_from = dep.remote_for(&relative).ok_or_else(|| failed("path is excluded from deployment"))?;
    let remote_to = to.map(|p| codecraft_remote::deploy::rel_path(&root, p).and_then(|rel| dep.remote_for(&rel))
        .ok_or_else(|| failed("destination is outside deployment mapping"))).transpose()?;
    if project.connection != ConnectionState::Connected { return Err(failed("Reconnect before changing the remote project tree; local changes are preserved")); }
    if create && !directory { after_save(s, &[from.to_path_buf()]); return Ok(()); }
    let server = crate::deployment::server(s, &project.server)?;
    let options = s.remote.ssh_options(&project.server);
    let (_, baseline) = mirror_paths(&root)?;
    let step = Step::CheckedTree { root, server, options, from: from.to_path_buf(), to: to.map(Path::to_path_buf),
        directory, create, remote_from, remote_to, baseline, rsync: s.remote.programs.rsync.clone() };
    s.remote.tree_queue.push_back((project.server, step));
    if let Some(to) = to {
        if let Some(project) = &mut s.remote.project {
            let moved: Vec<_> = project.pending_uploads.iter().filter_map(|p| p.strip_prefix(from).ok().map(|rel| (p.clone(), to.join(rel)))).collect();
            for (old, new) in moved { project.pending_uploads.remove(&old); project.pending_uploads.insert(new); }
        }
        let mut saved = vec![];
        if to.is_file() { saved.push(to.to_path_buf()); }
        saved.extend(s.ws.docs().filter_map(|d| d.path.as_ref().filter(|p| p.starts_with(to)).cloned()));
        after_save(s, &saved);
    } else if !create && let Some(project) = &mut s.remote.project {
        project.pending_uploads.retain(|p| !p.starts_with(from));
    }
    Ok(())
}

pub(crate) fn map_output_path(s: &Session, path: &Path) -> PathBuf {
    use codecraft_lsp::path_mapper::Direction;
    let normalized = crate::normalize_path(path);
    let path = normalized.as_path();
    let Some(root) = s.project_root() else { return path.to_path_buf() };
    if path.starts_with(&root) || external_cache(s).is_some_and(|cache| path.starts_with(cache)) { return path.to_path_buf(); }
    let Some(remote) = &s.remote.absolute_root else { return path.to_path_buf() };
    let mut mapper = codecraft_lsp::path_mapper::RootMapper::new(&root.to_string_lossy(), remote);
    if let Some(cache) = external_cache(s) { mapper = mapper.with_external_cache(&cache.to_string_lossy()); }
    mapper.map_path(&path.to_string_lossy(), Direction::ToClient).map(PathBuf::from).unwrap_or_else(|| path.to_path_buf())
}

pub(crate) fn map_output_line(s: &Session, line: &str) -> String {
    match (s.project_root(), s.remote.absolute_root.as_ref()) {
        (Some(root), Some(remote)) if !remote.is_empty() => {
            let mut output = String::with_capacity(line.len());
            let mut cursor = 0;
            for (start, matched) in line.match_indices(remote) {
                let end = start.saturating_add(matched.len());
                if line.get(end..).is_some_and(|tail| tail.is_empty() || tail.starts_with('/')) {
                    output.push_str(line.get(cursor..start).unwrap_or_default());
                    output.push_str(&root.to_string_lossy());
                    cursor = end;
                }
            }
            output.push_str(line.get(cursor..).unwrap_or_default());
            output
        },
        _ => line.to_owned(),
    }
}

fn poll_interval(base: u64, idle: usize, focused: bool) -> Duration {
    let seconds = [3_u64, 10, 30, 60].get(idle).copied().unwrap_or(60).max(base);
    Duration::from_secs(if focused { seconds } else { seconds.saturating_mul(5) })
}

// One remote-side fingerprint: only a fixed-size count/checksum crosses the network.
pub(crate) fn fingerprint_script(remote: &str) -> String {
    let prune = codecraft_remote::deploy::DEFAULT_EXCLUDES.iter().map(|name| format!("-name {}", codecraft_remote::quote::sh_quote(name))).collect::<Vec<_>>().join(" -o ");
    format!(r#"cd {} || exit 1; cc_probe=$(mktemp) || exit 44; trap 'rm -f "$cc_probe"' 0; if (ulimit -f 2048; timeout 5 find . -maxdepth 12 \( {prune} \) -prune -o -type f -printf '%p %T@ %s\n') > "$cc_probe"; then cksum < "$cc_probe"; else echo 'bounded poll exceeded time/size limit or find/timeout unavailable; use explicit refresh' >&2; exit 44; fi"#, codecraft_remote::quote::quote_path(remote))
}

fn auto_probe(s: &mut Session) -> Result<(), CommandError> {
    let project = s.remote.project.clone().ok_or_else(|| failed("project closed"))?;
    let server = crate::deployment::server(s, &project.server)?;
    let options = s.remote.ssh_options(&project.server);
    let script = fingerprint_script(&project.remote);
    let args = rcmd::ssh_exec(&server, &options, &codecraft_remote::quote::sh_c(&script)).map_err(failed)?;
    start_op(s, &project.server, "Probe remote project changes", vec![Step::run(&options.program, args)], Follow::AutoProbe, false)?;
    Ok(())
}

fn stop_watcher(remote: &mut RemoteState) {
    remote.watcher_started = None;
    if let Some((cancel, _, _)) = remote.watcher.take() { cancel.store(true, Ordering::Release); }
}

/// inotify is an optional optimization. A failed/missing watcher leaves the bounded,
/// metadata fingerprint poll active; both transports reuse the project's ControlMaster.
fn watcher_script(remote: &str) -> String {
    format!(r#"cd {} || exit 1;
command -v inotifywait >/dev/null 2>&1 || {{ echo 'fallback: inotifywait unavailable'; exit 42; }};
cc_watch=$(mktemp) || exit 43; trap 'rm -f "$cc_watch" "$cc_watch.dirs"' 0;
(timeout 5 find . \( -name .git -o -name .codecraft -o -name .idea -o -name target -o -name node_modules -o -name .claude -o -name .cache -o -name __pycache__ \) -prune -o -type d -exec sh -c '
cc_cr=$(printf "\r");
for cc_dir do
  case "$cc_dir" in *"
"*|*"$cc_cr"*) exit 45;; esac;
  printf "%s\n" "$cc_dir";
done' codecraft {{}} +; echo "CODECRAFT-DONE=$?") | head -n 2050 > "$cc_watch";
[ "$(tail -n 1 "$cc_watch")" = CODECRAFT-DONE=0 ] || {{ echo 'fallback: directory preflight exceeds 2048 directories/5s, unsupported directory name, or find/timeout unavailable'; exit 43; }};
cc_count=$(wc -l < "$cc_watch");
[ "$cc_count" -le 2049 ] || {{ echo 'fallback: tree exceeds 2048 directories'; exit 43; }};
cc_limit=$(cat /proc/sys/fs/inotify/max_user_watches 2>/dev/null);
case "$cc_limit" in ''|*[!0-9]*) ;; *) [ "$cc_count" -le "$((cc_limit / 4))" ] || {{ echo 'fallback: inotify watch budget too small'; exit 43; }};; esac;
sed '$d' "$cc_watch" > "$cc_watch.dirs";
printf 'ready\n'; inotifywait -m -q -P --fromfile "$cc_watch.dirs" -e modify,create,delete,move,attrib --format changed --exclude '(^|/)(\.git|\.codecraft|\.idea|target|node_modules|__pycache__|\.claude|\.cache)(/|$)'"#, codecraft_remote::quote::quote_path(remote))
}

fn maintain_watcher(s: &mut Session) {
    let connected = s.remote.auto_refresh && !s.remote.helpers_disabled && s.remote.project.as_ref().is_some_and(|p| p.connection == ConnectionState::Connected);
    if !connected { stop_watcher(&mut s.remote); return; }
    if s.remote.watcher.is_some() || interactive_busy(s) { return; }
    let Some(project) = s.remote.project.clone() else { return };
    let Some(server) = s.remote.server(&project.server) else { return };
    let opts = s.remote.ssh_options(&project.server);
    let command = codecraft_remote::quote::sh_c(&watcher_script(&project.remote));
    let Ok(mut args) = rcmd::ssh_exec(&server, &opts, &command) else { return };
    args.insert(0, "-T".into());
    let cancel = Arc::new(AtomicBool::new(false));
    let streaming = Arc::new(AtomicBool::new(false));
    let dirty = Arc::new(AtomicBool::new(true));
    let flags = (cancel.clone(), streaming.clone(), dirty.clone());
    let reason = Arc::new(std::sync::Mutex::new(String::new()));
    s.remote.watcher_reason = reason.clone();
    let spawned = std::thread::Builder::new().name("remote-watch".into()).spawn(move || {
        let mut command = Command::new(resolve_program(&opts.program));
        command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let Ok(mut child) = command.spawn() else { return };
        if let Some(errors) = child.stderr.take() {
            let reason = reason.clone();
            let _ = std::thread::Builder::new().name("remote-watch-errors".into()).spawn(move || {
                let mut text = String::new(); let _ = errors.take(4096).read_to_string(&mut text);
                if !text.is_empty() { *reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = text; }
            });
        }
        if let Some(mut output) = child.stdout.take() {
            let running = streaming.clone();
            let _ = std::thread::Builder::new().name("remote-watch-events".into()).spawn(move || {
                let mut ready = [0; 6];
                if output.read_exact(&mut ready).is_err() || &ready != b"ready\n" {
                    let mut text = String::from_utf8_lossy(&ready).into_owned();
                    let _ = output.take(4096).read_to_string(&mut text);
                    *reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = text;
                    return;
                }
                running.store(true, Ordering::Release);
                let mut bytes = [0; 4096];
                while let Ok(n) = output.read(&mut bytes) {
                    if n == 0 { break; }
                    dirty.store(true, Ordering::Release);
                }
                running.store(false, Ordering::Release);
            });
        }
        while !cancel.load(Ordering::Acquire) && matches!(child.try_wait(), Ok(None)) { std::thread::sleep(Duration::from_millis(50)); }
        if cancel.load(Ordering::Acquire) {
            #[cfg(unix)]
            { let _ = Command::new("kill").args(["-TERM", "--", &format!("-{}", child.id())]).stdout(Stdio::null()).stderr(Stdio::null()).status(); }
            let _ = child.kill();
        }
        let _ = child.wait();
        streaming.store(false, Ordering::Release);
    });
    if spawned.is_ok() { s.remote.watcher = Some(flags); s.remote.watcher_started = Some(Instant::now()); }
}

pub(crate) fn stop_project_watcher(s: &mut Session) { stop_watcher(&mut s.remote); }

impl Drop for RemoteState {
    fn drop(&mut self) {
        stop_watcher(self);
        self.cancel.store(true, Ordering::Release);
        for master in self.masters.values_mut() { if let Some(child) = &mut master.child { let _ = child.kill(); } }
    }
}

/// Resolve terminal links using the remote shell's saved directory and the mirror mapping.
pub fn terminal_path(s: &Session, tab: usize, raw: &str) -> Option<PathBuf> {
    let shell = s.terminal.tabs.get(tab)?;
    if !shell.remote { return s.terminal.resolve_path(tab, raw, s.project_root().as_deref()); }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() { path } else { shell.cwd.join(path) };
    let mapped = map_output_path(s, &path);
    (mapped.is_file() || external_cache(s).is_some_and(|cache| mapped.starts_with(cache))).then_some(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(short_hash("a").len(), 16);
        assert_ne!(short_hash("a"), short_hash("b"));
        assert_eq!(safe_name("my server/1"), "my_server_1");
        assert_eq!(safe_name(".x"), "s.x");
        assert_eq!(safe_name(""), "s");
        assert_eq!(tail("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(read_capped(&b"hello"[..], 3), b"hel");
    }

    #[test]
    fn control_paths_are_short() {
        let r = RemoteState::default();
        if let Some(p) = r.control_path("a-very-long-server-name-that-goes-on-and-on-and-on.example.com") {
            assert!(p.as_os_str().len() < 100, "{}", p.display());
        }
    }

    #[test]
    fn processes_report_failures_and_capture_output() {
        let cancel = AtomicBool::new(false);
        let out = run_process("sh", &["-c".into(), "echo hi".into()], None, None, &cancel).unwrap();
        assert_eq!(out.trim(), "hi");
        let err = run_process("sh", &["-c".into(), "echo boom >&2; exit 3".into()], None, None, &cancel).unwrap_err();
        assert!(err.contains("exit code 3") && err.contains("boom"), "{err}");
        assert!(run_process("/nonexistent/prog", &[], None, None, &cancel).is_err());
        let cancel = AtomicBool::new(true);
        assert!(run_process("sh", &["-c".into(), "sleep 5".into()], None, None, &cancel).is_err());
    }
}

#[cfg(test)]
mod pending_upload_tests {
    use super::*;

    #[test]
    fn network_transfer_lock_does_not_block_saving_local_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one").unwrap();
        let mut session = Session::new();
        session.isolate_home(&dir.path().join("home"));
        session.lsp_enabled = false;
        session.ws.project = Some(codecraft_workspace::Project::new(root.clone()));
        session.remote.project = Some(RemoteProject {
            server: "fixture".into(),
            destination: "fixture".into(),
            remote: "~/project".into(),
            connection: ConnectionState::Disconnected,
            error: None,
            pending_uploads: Default::default(),
        });
        session.open_file(&root.join("a.txt")).unwrap();
        let id = session.ws.active().unwrap().1.id;
        session
            .ws
            .doc_mut(id)
            .unwrap()
            .doc
            .replace_all("saved during transfer", &codecraft_text::SelectionSet::caret(0))
            .unwrap();
        let lock = transfer_lock(&root);
        let _guard = lock.lock().unwrap();
        assert_eq!(session.save_all().unwrap(), 1);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "saved during transfer");
    }

    #[test]
    fn malformed_and_escaping_pending_paths_are_actionable_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codecraft")).unwrap();
        std::fs::write(dir.path().join(PENDING_UPLOADS), "[").unwrap();
        assert!(load_uploads(dir.path()).is_err());
        std::fs::write(dir.path().join(PENDING_UPLOADS), r#"["../outside"]"#).unwrap();
        assert!(load_uploads(dir.path()).unwrap_err().contains("outside"));
        std::fs::write(dir.path().join(PENDING_UPLOADS), r#"["src/a.rs"]"#).unwrap();
        assert!(load_uploads(dir.path()).unwrap().contains(&dir.path().join("src/a.rs")));
    }
}

const LSP_PATH: &str = "export PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\"; ";

/// Returns true only after a successful version probe, cached until reconnect.
pub(crate) fn prepare_lsp(s: &mut Session, kind: codecraft_lsp::ServerKind) -> bool {
    let Some(project) = s.remote.project.clone() else { return true };
    if s.remote.local_lsp.contains(&kind) { return true; }
    if !matches!(kind, codecraft_lsp::ServerKind::RustAnalyzer | codecraft_lsp::ServerKind::Clangd | codecraft_lsp::ServerKind::Python) {
        s.set_status(kind, crate::LspStatus::Unavailable("SSH transport is not implemented for this language server".into()));
        return false;
    }
    if s.remote.helpers_disabled { return false; }
    if interactive_busy(s) {
        s.remote.lsp_retry.entry(kind).or_insert((0, Instant::now() + Duration::from_secs(3)));
        return false;
    }
    let key = (project.server.clone(), kind);
    if let Some(tool) = s.remote.lsp_tools.get(&key) { return tool.is_some(); }
    if s.remote.ops.values().any(|(follow, _)| matches!(follow, Follow::LspProbe { kind: k, .. } if *k == kind)) { return false; }
    let Some(server) = s.remote.server(&project.server) else { return false };
    let opts = s.remote.ssh_options(&project.server);
    let script = if kind == codecraft_lsp::ServerKind::Python {
        format!("{LSP_PATH}if basedpyright-langserver --version >/dev/null 2>&1; then echo basedpyright-langserver; elif pyright-langserver --version >/dev/null 2>&1; then echo pyright-langserver; else exit 127; fi")
    } else {
        format!("{LSP_PATH}{} --version >/dev/null 2>&1 && echo {}", kind.label(), kind.label())
    };
    let root = s.remote.absolute_root.as_deref().unwrap_or(&project.remote);
    let prefix = match codecraft_remote::target_script::prelude(root, &[]) {
        Ok(prefix) => prefix,
        Err(error) => { s.set_status(kind, crate::LspStatus::Unavailable(error.to_string())); return false; }
    };
    let script = format!("{prefix}{script}");
    let Ok(args) = rcmd::ssh_exec(&server, &opts, &codecraft_remote::quote::sh_c(&script)) else { return false };
    let generation = s.remote.control_generation.get(&project.server).copied().unwrap_or(0);
    s.set_status(kind, crate::LspStatus::Starting);
    if let Err(error) = start_op(s, &project.server, "Probe remote language server", vec![Step::run(&opts.program, args)], Follow::LspProbe { server: project.server.clone(), generation, kind }, false) {
        s.set_status(kind, crate::LspStatus::Stopped(error.to_string()));
        s.notify(NotificationLevel::Error, "Remote language server probe", error.to_string());
    }
    false
}

pub(crate) fn lsp_action(s: &mut Session, params: &Value, local: bool) -> crate::commands::CmdResult {
    let kind = match params.get("language").and_then(Value::as_str).unwrap_or("rust") {
        "rust" => codecraft_lsp::ServerKind::RustAnalyzer,
        "cpp" | "clangd" => codecraft_lsp::ServerKind::Clangd,
        "python" => codecraft_lsp::ServerKind::Python,
        _ => return Err(failed("language must be rust, cpp or python")),
    };
    let project = s.remote.project.clone().ok_or_else(|| failed("no remote project"))?;
    if local {
        s.remote.local_lsp.insert(kind);
        s.notify(NotificationLevel::Warning, "Local language server fallback", "Analysing the local mirror without the remote toolchain.");
        s.restart_server(Some(kind));
        return Ok(Value::Null);
    }
    s.remote.local_lsp.remove(&kind);
    let server = crate::deployment::server(s, &project.server)?;
    let opts = s.remote.ssh_options(&project.server);
    let install = match kind {
        codecraft_lsp::ServerKind::RustAnalyzer => "rustup component add rust-analyzer",
        codecraft_lsp::ServerKind::Clangd => "if command -v apt-get >/dev/null; then sudo -n apt-get install -y clangd; elif command -v dnf >/dev/null; then sudo -n dnf install -y clang-tools-extra; elif command -v pacman >/dev/null; then sudo -n pacman -S --noconfirm clang; else echo 'Install clangd with the host package manager' >&2; exit 1; fi",
        _ => "python3 -m pip install --user basedpyright",
    };
    let install = params.get("command").and_then(Value::as_str).unwrap_or(install);
    let root = s.remote.absolute_root.as_deref().unwrap_or(&project.remote);
    let prefix = codecraft_remote::target_script::prelude(root, &[]).map_err(failed)?;
    let args = rcmd::ssh_exec(&server, &opts, &codecraft_remote::quote::sh_c(&format!("{prefix}{LSP_PATH}{install}"))).map_err(failed)?;
    let id = start_op(s, &project.server, "Install remote language server", vec![Step::run(&opts.program, args)], Follow::LspInstall { kind }, true)?;
    s.ws.layout.show(ToolWindowId::FileTransfer);
    Ok(serde_json::json!({"operation": id}))
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Invocation {
    host: String,
    program: String,
    duration_ms: u64,
    control_path: Option<String>,
    socket_present: bool,
    success: bool,
}

thread_local! {
    static INVOCATION_HOST: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn invocations() -> &'static std::sync::Mutex<std::collections::VecDeque<Invocation>> {
    static HISTORY: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<Invocation>>> = std::sync::OnceLock::new();
    HISTORY.get_or_init(Default::default)
}

pub(crate) fn diagnostics(s: &mut Session, _: &Value) -> crate::commands::CmdResult {
    let project = s.remote.project.clone().ok_or_else(|| failed("no remote project"))?;
    let server = crate::deployment::server(s, &project.server)?;
    let opts = s.remote.ssh_options(&project.server);
    let masters: Vec<_> = [project.server.clone(), format!("{}\0bulk", project.server)].iter().map(|key| {
        match s.remote.masters.get_mut(key) {
            Some(master) => {
                let alive = master.child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)));
                let reason = master.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                serde_json::json!({"host": project.server, "bulk": key.ends_with("\0bulk"), "running": alive,
                    "uptimeMs": master.started.elapsed().as_millis(), "socket": master.options.control_path,
                    "socketPresent": master.options.control_path.as_ref().is_some_and(|path| Path::new(path).exists()),
                    "error": if reason.is_empty() { None } else { Some(format!("multiplexing unavailable: {reason}")) }})
            }
            None => serde_json::json!({"bulk": key.ends_with("\0bulk"), "running": false}),
        }
    }).collect();
    let history = invocations().lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().filter(|entry| entry.host == project.server).cloned().collect::<Vec<_>>();
    let active: Vec<_> = active_invocations().lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter()
        .filter(|(_, (host, _, _))| host == &project.server)
        .map(|(id, (_, kind, at))| serde_json::json!({"id": id, "kind": kind, "ageMs": at.elapsed().as_millis()})).collect();
    let report = serde_json::json!({"host": project.server, "masters": masters, "inFlight": s.remote.ops.iter().filter(|(id, _)| s.remote.op_hosts.get(id).is_some_and(|host| host == &project.server)).map(|(id, (follow, _))| serde_json::json!({"id": id, "ageMs": s.remote.op_started.get(id).map(|at| at.elapsed().as_millis()), "kind": format!("{follow:?}").split_whitespace().next().unwrap_or("operation")})).collect::<Vec<_>>(), "invocations": history,
        "terminals": s.terminal.tabs.iter().filter(|tab| tab.remote).map(|tab| serde_json::json!({"tab": tab.id, "echoMedianUs": tab.term.echo_latency().0, "echoSamples": tab.term.echo_latency().1, "busy": tab.term.recently_active(Duration::from_secs(3))})).collect::<Vec<_>>(),
        "processes": {"scope": "CodeCraft-owned helper channels; excludes user shell processes and helper descendants", "active": active, "activeCount": active.len(), "languageServerCount": s.remote.lsp_started.keys().filter(|kind| s.servers.contains_key(kind) && !s.remote.local_lsp.contains(kind)).count(), "watcherCount": usize::from(s.remote.auto_refresh_mode() == "inotify"), "watcher": s.remote.watcher_started.filter(|_| s.remote.auto_refresh_mode() == "inotify").map(|at| serde_json::json!({"kind": "inotifywait", "ageMs": at.elapsed().as_millis()})), "languageServers": s.remote.lsp_started.iter().filter(|(kind, _)| s.servers.contains_key(kind) && !s.remote.local_lsp.contains(kind)).map(|(kind, at)| serde_json::json!({"kind": kind.label(), "ageMs": at.elapsed().as_millis()})).collect::<Vec<_>>()},
        "frameSections": s.remote.frame_sections, "watcherReason": *s.remote.watcher_reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        "refresh": {"lastReason": s.remote.debounce.reason, "events": s.remote.debounce.events, "count": s.remote.debounce.refreshes, "pending": s.remote.debounce.pending || s.remote.refresh_pending, "spacingSecs": s.remote.debounce.spacing().as_secs(), "lastEventAgeMs": s.remote.debounce.last_event.map(|at| at.elapsed().as_millis()), "treeRefreshes": s.remote.tree_refreshes, "interactiveDeferred": interactive_busy(s)}});
    let mut check = rcmd::ssh_exit_master(&server, &opts).map_err(failed)?.unwrap_or_default();
    if let Some(index) = check.iter().position(|arg| arg == "exit") && let Some(arg) = check.get_mut(index) { *arg = "check".into(); }
    let shared = rcmd::ssh_exec(&server, &opts, "true").map_err(failed)?;
    let mut fresh = opts.clone(); fresh.control_path = None;
    let fresh_args = rcmd::ssh_exec(&server, &fresh, "true").map_err(failed)?;
    // Timing lives in invocation history; run check last so a missing master doesn't skip RTTs.
    let mut steps = vec![Step::Output(serde_json::to_string_pretty(&report).map_err(|error| failed(error.to_string()))?),
        Step::Measure { label: "shared RTT".into(), program: opts.program.clone(), args: shared },
        Step::Measure { label: "fresh RTT".into(), program: opts.program.clone(), args: fresh_args },
        Step::Measure { label: "host loadavg / CPUs".into(), program: opts.program.clone(), args: rcmd::ssh_exec(&server, &opts, "cat /proc/loadavg; getconf _NPROCESSORS_ONLN").map_err(failed)? }];
    if !check.is_empty() { steps.push(Step::Measure { label: "master check".into(), program: opts.program.clone(), args: check }); }
    let id = start_op(s, &project.server, "Connection diagnostics (shared RTT, fresh RTT, master check)", steps, Follow::Nothing, true)?;
    s.ws.layout.show(ToolWindowId::FileTransfer);
    Ok(serde_json::json!({"operation": id, "report": report}))
}

#[cfg(test)]
mod real_host_regressions {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn watcher_preflight_caps_trees_and_refuses_line_injection_before_installing_watches() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let capture = dir.path().join("watched");
        for (program, script) in [("timeout", "#!/bin/sh\nshift; exec \"$@\"\n"),
            ("inotifywait", "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --fromfile) cat \"$2\" > \"$WATCH_CAPTURE\"; shift 2;; *) shift;; esac; done\n")] {
            let path = bin.join(program);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let run = || Command::new("/bin/sh").arg("-c").arg(watcher_script(&root.to_string_lossy()))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display())).env("WATCH_CAPTURE", &capture).output().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        assert!(run().status.success());
        assert_eq!(std::fs::read_to_string(&capture).unwrap().lines().count(), 2);
        std::fs::remove_file(&capture).unwrap();
        let hostile = root.join("odd\nfolder");
        std::fs::create_dir(&hostile).unwrap();
        assert!(!run().status.success());
        assert!(!capture.exists());
        std::fs::remove_dir(hostile).unwrap();
        for index in 0..2048 { std::fs::create_dir(root.join(format!("d{index}"))).unwrap(); }
        let output = run();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("fallback"));
        assert!(!capture.exists());
    }
    #[test]
    fn event_storm_has_one_trailing_refresh_and_increasing_spacing() {
        let start = Instant::now();
        let mut debounce = RefreshDebounce::default();
        for ms in 0..10_000 {
            let now = start + Duration::from_millis(ms);
            debounce.event(now);
            assert!(!debounce.due(now));
        }
        let now = start + Duration::from_secs(12);
        assert!(debounce.due(now));
        debounce.started(now);
        assert!(!debounce.due(now + Duration::from_secs(2)));
        assert_eq!(debounce.refreshes, 1);
        for seconds in [5, 15, 30, 30] {
            debounce.event(now);
            assert_eq!(debounce.spacing(), Duration::from_secs(seconds));
            assert!(!debounce.due(now + Duration::from_secs(seconds - 1)));
            assert!(debounce.due(now + Duration::from_secs(seconds)));
            debounce.started(now);
        }
        debounce.event(now + Duration::from_secs(61));
        assert_eq!(debounce.spacing(), Duration::from_secs(2));
    }

    #[test]
    fn stall_threshold_warning_limit_and_recent_maximum() {
        let mut section = FrameSection::default();
        let now = Instant::now();
        section.record("test", Duration::from_millis(30), now);
        assert_eq!(section.stalls, 0);
        section.record("test", Duration::from_millis(40), now);
        section.record("test", Duration::from_millis(80), now + Duration::from_secs(1));
        assert_eq!(section.stalls, 2);
        assert_eq!(section.warned, Some(now));
        assert_eq!(section.worst_ms, 80.0);
        section.record("test", Duration::from_millis(31), now + Duration::from_secs(6));
        assert_eq!(section.warned, Some(now + Duration::from_secs(6)));
        section.record("test", Duration::from_millis(1), now + Duration::from_secs(61));
        assert_eq!(section.worst_ms, 1.0);
    }

    #[test]
    fn unchanged_refresh_does_not_reload_or_rescan_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        session.isolate_home(dir.path());
        finish(&mut session, Follow::RemoteDev { server: "fake".into(), remote: "/project".into(), local: dir.path().to_path_buf(), open_window: false },
            Ok(vec![serde_json::to_string(&codecraft_remote::mirror::Refresh::default()).unwrap()]));
        assert_eq!(session.remote.tree_refreshes, 0);
        assert!(session.remote.reload.is_none());
        assert!(!session.jobs_running());
    }

    #[test]
    fn delayed_reload_cannot_replace_a_newer_saved_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "old").unwrap();
        let mut session = Session::new();
        session.lsp_enabled = false;
        session.isolate_home(dir.path());
        session.open_file(&path).unwrap();
        let id = session.ws.active().unwrap().1.id;
        let revision = session.ws.doc(id).unwrap().doc.revision();
        let doc = session.ws.doc_mut(id).unwrap();
        doc.doc.replace_all("newer saved text", &codecraft_text::SelectionSet::caret(0)).unwrap();
        doc.doc.mark_saved();
        let saved_revision = session.ws.doc(id).unwrap().doc.revision();
        let (tx, rx) = crossbeam_channel::bounded(2);
        tx.send(vec![(path.clone(), "stale worker result".into(), None, vec![(id, revision)])]).unwrap();
        tx.send(vec![(path, "fresh worker result".into(), None, vec![(id, saved_revision)])]).unwrap();
        drop(tx);
        session.remote.reload = Some(rx);
        apply_reload(&mut session);
        assert_eq!(session.ws.doc(id).unwrap().doc.text(), "newer saved text");
        assert!(session.remote.reload.is_some());
        apply_reload(&mut session);
        assert_eq!(session.ws.doc(id).unwrap().doc.text(), "fresh worker result");
        apply_reload(&mut session);
        assert!(session.remote.reload.is_none());
    }
    #[test]
    fn user_activity_and_focus_reset_idle_polling() {
        let mut remote = RemoteState::default();
        remote.set_window_focused(false);
        remote.poll_idle = 3;
        remote.set_window_focused(true);
        assert_eq!(remote.poll_idle, 0);
        remote.poll_idle = 3;
        remote.note_activity();
        assert_eq!(remote.poll_idle, 0);
        assert!(remote.last_refresh.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bulk_socket_cannot_alias_another_servers_primary_socket() {
        let remote = RemoteState::default();
        assert_ne!(remote.bulk_options("web").control_path, remote.ssh_options("web:bulk").control_path);
    }

    #[test]
    fn idle_intervals_are_bounded_and_unfocused_polling_is_slower() {
        for (idle, seconds) in [(0, 3), (1, 10), (2, 30), (3, 60), (100, 60)] {
            assert_eq!(poll_interval(3, idle, true), Duration::from_secs(seconds));
            assert_eq!(poll_interval(3, idle, false), Duration::from_secs(seconds * 5));
        }
        assert_eq!(poll_interval(120, 0, true), Duration::from_secs(120));
    }
}

fn remote_install_hint(kind: codecraft_lsp::ServerKind) -> &'static str {
    match kind {
        codecraft_lsp::ServerKind::RustAnalyzer => "rustup component add rust-analyzer",
        codecraft_lsp::ServerKind::Clangd => "install clangd with the host package manager (apt: clangd; dnf: clang-tools-extra)",
        codecraft_lsp::ServerKind::Python => "python3 -m pip install --user basedpyright (or npm install -g pyright)",
        _ => kind.install_hint(),
    }
}

impl Session {
    /// Record a named UI/pump cost; diagnostics retains the current 60-second window maximum.
    pub fn record_frame_section(&mut self, name: &str, elapsed: Duration) {
        self.remote.frame_sections.entry(name.to_owned()).or_default().record(name, elapsed, Instant::now());
    }
}
