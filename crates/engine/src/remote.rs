//! SSH servers, deployment, File Transfer log, Remote Host browsing and Remote Development
//! (M11 track S).
//!
//! Everything that talks to a server runs the system `ssh`/`rsync`/`scp` on a worker thread as an
//! *operation* (a list of [`Step`]s); [`pump`] applies the result (log entry, listing, opening a
//! downloaded file, a sync preview…). Each server gets one ControlMaster connection (`ssh -M -N`)
//! that the other invocations share; masters are closed on [`shutdown`].
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
    Run {
        program: String,
        args: Vec<String>,
        stdin: Option<PathBuf>,
        stdout_to: Option<PathBuf>,
    },
    Mkdir(PathBuf),
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
    /// Reload open documents that changed on disk.
    Reload,
    Listing {
        server: String,
        path: String,
    },
    RefreshBrowser,
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
    },
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
}

/// Everything track S keeps in the [`Session`].
pub struct RemoteState {
    /// Instead of `$CODECRAFT_HOME` (default `~/.codecraft`): `ssh.json` and the caches.
    pub home_override: Option<PathBuf>,
    /// Instead of `~/.ssh/config`.
    pub ssh_config_override: Option<PathBuf>,
    pub programs: Programs,
    /// Share one connection per server (ControlMaster); tests turn it off.
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
    /// Test Connection results by server.
    pub tests: BTreeMap<String, TestStatus>,
    servers: Option<(Vec<SshServer>, Vec<String>)>,
    /// Files opened from Remote Host: local cache file → (server, remote path).
    opened: HashMap<PathBuf, (String, String)>,
    masters: HashMap<String, Master>,
    ops: HashMap<u64, (Follow, bool)>,
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
            tests: BTreeMap::new(),
            servers: None,
            opened: HashMap::new(),
            masters: HashMap::new(),
            ops: HashMap::new(),
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
    pub fn busy(&self) -> bool {
        !self.ops.is_empty()
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
        let name = format!("{}-{}-{}", std::process::id(), self.nonce, short_hash(server).get(..12).unwrap_or(""));
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

    /// Start the server's control master if it is not running (non-blocking).
    pub(crate) fn ensure_master(&mut self, server: &SshServer) {
        if !self.share_connections {
            return;
        }
        if let Some(m) = self.masters.get_mut(&server.name) {
            let alive = m.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
            if alive || m.started.elapsed() < MASTER_RETRY {
                return;
            }
        }
        let opts = self.ssh_options(&server.name);
        let Some(dir) = self.control_dir() else { return };
        if create_private_dir(&dir).is_err() {
            return;
        }
        if let Some(p) = &opts.control_path {
            // A socket left by a crashed earlier run of this process id would stop the master.
            let _ = std::fs::remove_file(p);
        }
        let Ok(Some(args)) = rcmd::ssh_master(server, &opts) else { return };
        let child = Command::new(resolve_program(&self.programs.ssh))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| log::warn!("ssh master for {}: {e}", server.name))
            .ok();
        self.masters.insert(
            server.name.clone(),
            Master { child, started: Instant::now(), server: server.clone(), options: opts },
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
fn run_process(
    program: &str,
    args: &[String],
    stdin: Option<&Path>,
    stdout_to: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<String, String> {
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
    if status.success() {
        return Ok(stdout);
    }
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    let msg = tail(&String::from_utf8_lossy(&stderr), 6);
    let code = status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "killed".into());
    Err(if msg.is_empty() { format!("{name} failed ({code})") } else { format!("{name} failed ({code}): {msg}") })
}

fn run_steps(steps: &[Step], cancel: &AtomicBool) -> Result<Vec<String>, String> {
    let mut outputs = Vec::new();
    for step in steps {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        match step {
            Step::Run { program, args, stdin, stdout_to } => {
                outputs.push(run_process(program, args, stdin.as_deref(), stdout_to.as_deref(), cancel)?);
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
    let id = r.next();
    let tx = r.tx.clone();
    let cancel = r.cancel.clone();
    std::thread::Builder::new()
        .name(format!("remote-{id}"))
        .spawn(move || {
            let result = run_steps(&steps, &cancel);
            let _ = tx.send(OpDone { id, result });
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    r.ops.insert(id, (follow, logged));
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
    changed
}

fn finish(s: &mut Session, follow: Follow, result: Result<Vec<String>, String>) {
    match follow {
        Follow::Nothing => {}
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
            let status = match result {
                Ok(_) => TestStatus::Ok("Connected".into()),
                Err(e) => TestStatus::Failed(e),
            };
            s.status = match &status {
                TestStatus::Ok(_) => format!("{server}: connection successful"),
                TestStatus::Failed(e) => format!("{server}: {e}"),
                TestStatus::Running => String::new(),
            };
            s.remote.tests.insert(server, status);
        }
        Follow::RemoteDev { server, remote, local } => {
            if result.is_ok() {
                crate::remote_cmds::remote_dev_ready(s, &server, &remote, &local);
            }
        }
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
    s.remote.cancel.store(true, Ordering::Relaxed);
    let masters: Vec<Master> = s.remote.masters.drain().map(|(_, m)| m).collect();
    for mut m in masters {
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
