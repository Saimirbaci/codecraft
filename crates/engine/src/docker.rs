//! Docker: connections, containers, images, volumes, networks and Compose projects through the `docker` CLI (M11 track D).
//!
//! Everything goes through the `docker` program (no Docker API crates): lists use
//! `--format '{{json .}}'` (one JSON object per line) and are parsed by the pure `parse_*`
//! functions; every call runs on a worker thread and reports through a channel drained by
//! [`pump`]. Container logs follow `docker logs -f` in a child process that is killed when the
//! log view closes or the session shuts down. [`DockerState::program`] is the test seam: tests
//! point it at a fake `docker` script.

pub mod devcontainer;
pub mod run;

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use codecraft_cargo::{RunEvent, RunHandle};
use codecraft_workspace::NotificationLevel;

use crate::Session;
use crate::services::ServicesSection;

/// Most items read from one list command.
const MAX_ITEMS: usize = 5_000;
/// Most bytes read from one command's stdout or stderr.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// Lines kept per log view.
pub const MAX_LOG_LINES: usize = 20_000;
/// Log views open at once.
const MAX_LOG_VIEWS: usize = 20;
/// Lines kept in the operations output.
const MAX_OUTPUT_LINES: usize = 500;
/// Automatic refresh interval while the Docker section is visible.
pub const REFRESH_EVERY: Duration = Duration::from_secs(10);
/// Timeout of list and inspect calls.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout of start/stop/remove/prune/compose calls.
const OP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Timeout of `docker pull`.
const PULL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

// ---------------------------------------------------------------- model

/// A `docker context` (a connection to a daemon).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerContext {
    pub name: String,
    pub description: String,
    pub endpoint: String,
    pub current: bool,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub command: String,
    /// `running`, `exited`, `paused`, `created`, `restarting`, `dead`.
    pub state: String,
    /// Human status ("Up 3 days", "Exited (0) 2 hours ago").
    pub status: String,
    pub ports: String,
    pub created: String,
    pub labels: Vec<(String, String)>,
}

impl Container {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn compose_project(&self) -> Option<&str> {
        self.label("com.docker.compose.project").filter(|p| !p.is_empty())
    }

    pub fn compose_service(&self) -> Option<&str> {
        self.label("com.docker.compose.service").filter(|p| !p.is_empty())
    }

    pub fn is_running(&self) -> bool {
        self.state == "running" || self.state == "restarting"
    }

    pub fn is_paused(&self) -> bool {
        self.state == "paused"
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub repository: String,
    pub tag: String,
    pub size: String,
    pub created: String,
}

impl Image {
    /// `repo:tag`, `repo`, or the id for an untagged image.
    pub fn reference(&self) -> String {
        match (self.repository.as_str(), self.tag.as_str()) {
            ("" | "<none>", _) => self.id.clone(),
            (r, "" | "<none>") => r.to_string(),
            (r, t) => format!("{r}:{t}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
}

/// Containers grouped by their `com.docker.compose.project` label.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeProject {
    pub name: String,
    /// `com.docker.compose.project.working_dir`.
    pub working_dir: String,
    /// `com.docker.compose.project.config_files`.
    pub config_files: Vec<String>,
    /// Container ids, in list order.
    pub containers: Vec<String>,
    pub running: usize,
}

/// One refresh of the selected connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
}

impl Snapshot {
    pub fn compose_projects(&self) -> Vec<ComposeProject> {
        compose_projects(&self.containers)
    }

    pub fn container(&self, id: &str) -> Option<&Container> {
        self.containers.iter().find(|c| c.id == id || c.name == id)
    }
}

/// What a prune removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PruneKind {
    System,
    Containers,
    Images,
    Volumes,
    Networks,
}

impl PruneKind {
    pub const ALL: [PruneKind; 5] = [Self::System, Self::Containers, Self::Images, Self::Volumes, Self::Networks];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "system" => Self::System,
            "containers" => Self::Containers,
            "images" => Self::Images,
            "volumes" => Self::Volumes,
            "networks" => Self::Networks,
            _ => return None,
        })
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Containers => "containers",
            Self::Images => "images",
            Self::Volumes => "volumes",
            Self::Networks => "networks",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System (stopped containers, unused networks, dangling images, build cache)",
            Self::Containers => "Stopped Containers",
            Self::Images => "Dangling Images",
            Self::Volumes => "Unused Volumes",
            Self::Networks => "Unused Networks",
        }
    }

    /// The `docker` arguments (`-f`: the confirmation happens in CodeCraft).
    pub fn args(self) -> Vec<String> {
        let a: &[&str] = match self {
            Self::System => &["system", "prune", "-f"],
            Self::Containers => &["container", "prune", "-f"],
            Self::Images => &["image", "prune", "-f"],
            Self::Volumes => &["volume", "prune", "-f"],
            Self::Networks => &["network", "prune", "-f"],
        };
        a.iter().map(|s| (*s).to_string()).collect()
    }
}

/// A destructive action waiting for confirmation (`docker.prune` without `confirm: true`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirm {
    pub prune: PruneKind,
    pub message: String,
}

/// A container's streamed log (`docker logs -f`).
#[derive(Debug)]
pub struct LogView {
    pub id: u64,
    pub container: String,
    pub title: String,
    pub lines: VecDeque<String>,
    pub running: bool,
    handle: Option<RunHandle>,
    events: Option<Receiver<RunEvent>>,
}

impl LogView {
    fn stop(&mut self) {
        if let Some(h) = self.handle.take()
            && let Err(e) = h.kill()
        {
            log::warn!("docker logs: {e}");
        }
        self.events = None;
        self.running = false;
    }
}

/// `docker inspect` output, pretty-printed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspect {
    pub title: String,
    pub json: String,
}

/// Registries from the Docker CLI config (`~/.docker/config.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryStatus {
    /// Registries with stored credentials (`auths`) or a credential helper (`credHelpers`).
    pub registries: Vec<String>,
    /// The credential store (`credsStore`), e.g. `osxkeychain`, `desktop`.
    pub creds_store: Option<String>,
    pub config_path: String,
}

enum Msg {
    Contexts(Result<Vec<DockerContext>, String>),
    Snapshot { generation: u64, result: Result<Snapshot, String> },
    DevContainers(Result<Vec<Container>, String>),
    Op { label: String, result: Result<String, String>, refresh: bool },
    Inspect { title: String, result: Result<String, String> },
}

#[derive(Debug)]
pub struct DockerState {
    /// The `docker` program (a name looked up on `PATH`, or a path). Tests point it at a fake.
    pub program: String,
    /// The Dev Containers CLI (`devcontainer`, a name or a path); used when it exists.
    pub devcontainer_program: String,
    /// The Docker CLI config directory (default: `$DOCKER_CONFIG`, else `~/.docker`).
    pub config_dir: Option<PathBuf>,
    /// Connections from `docker context ls`.
    pub contexts: Vec<DockerContext>,
    pub contexts_loaded: bool,
    /// The selected connection (a context name); `None` = the CLI's current context.
    pub connection: Option<String>,
    pub data: Snapshot,
    /// A refresh finished at least once.
    pub loaded: bool,
    pub loading: bool,
    /// Why the last refresh failed (daemon not running, `docker` missing…).
    pub error: Option<String>,
    /// Containers created for Dev Containers (`devcontainer.local_folder` label).
    pub dev_containers: Vec<Container>,
    pub logs: Vec<LogView>,
    /// The log view shown in the Services tool window.
    pub active_log: Option<u64>,
    pub inspect: Option<Inspect>,
    pub confirm: Option<Confirm>,
    /// Labels of operations in flight ("Stopping web").
    pub operations: Vec<String>,
    /// Output of finished operations, newest last.
    pub output: VecDeque<String>,
    pub registry: Option<RegistryStatus>,
    last_refresh: Option<Instant>,
    generation: u64,
    pending: usize,
    channel: Option<(Sender<Msg>, Receiver<Msg>)>,
    next_log: u64,
}

impl Default for DockerState {
    fn default() -> Self {
        Self {
            program: "docker".into(),
            devcontainer_program: "devcontainer".into(),
            config_dir: None,
            contexts: Vec::new(),
            contexts_loaded: false,
            connection: None,
            data: Snapshot::default(),
            loaded: false,
            loading: false,
            error: None,
            dev_containers: Vec::new(),
            logs: Vec::new(),
            active_log: None,
            inspect: None,
            confirm: None,
            operations: Vec::new(),
            output: VecDeque::new(),
            registry: None,
            last_refresh: None,
            generation: 0,
            pending: 0,
            channel: None,
            next_log: 1,
        }
    }
}

impl DockerState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        self.pending > 0 || self.logs.iter().any(|l| l.running)
    }

    /// The `docker` program, resolved on `PATH` and the usual tool directories.
    pub fn program_path(&self) -> PathBuf {
        resolve_program(&self.program)
    }

    /// The `devcontainer` CLI when it is installed.
    pub fn devcontainer_cli(&self) -> Option<PathBuf> {
        Some(resolve_program(&self.devcontainer_program)).filter(|p| p.is_file())
    }

    /// `--context <name>` for the selected connection.
    pub fn base_args(&self) -> Vec<String> {
        match &self.connection {
            Some(c) => vec!["--context".into(), c.clone()],
            None => Vec::new(),
        }
    }

    /// The selected connection's name for display.
    pub fn connection_label(&self) -> String {
        self.connection
            .clone()
            .or_else(|| self.contexts.iter().find(|c| c.current).map(|c| c.name.clone()))
            .unwrap_or_else(|| "Docker".into())
    }

    pub fn log(&self, id: u64) -> Option<&LogView> {
        self.logs.iter().find(|l| l.id == id)
    }

    fn sender(&mut self) -> Sender<Msg> {
        let (tx, _) = self.channel.get_or_insert_with(unbounded);
        tx.clone()
    }

    fn spawn(&mut self, job: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.sender();
        self.pending += 1;
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    fn push_output(&mut self, text: &str) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            self.output.push_back(line.to_string());
        }
        while self.output.len() > MAX_OUTPUT_LINES {
            self.output.pop_front();
        }
    }
}

/// A program name resolved on `PATH` (plus the usual tool directories); a path is kept.
pub fn resolve_program(program: &str) -> PathBuf {
    let p = PathBuf::from(program);
    if p.components().count() > 1 {
        return p;
    }
    codecraft_lsp::which(program).unwrap_or(p)
}

// ---------------------------------------------------------------- parsing

/// The JSON objects of `--format '{{json .}}'` output (one per line; other lines are skipped).
pub fn parse_json_lines(text: &str) -> Vec<Map<String, Value>> {
    text.lines()
        .filter_map(|l| match serde_json::from_str::<Value>(l.trim()) {
            Ok(Value::Object(m)) => Some(m),
            _ => None,
        })
        .take(MAX_ITEMS)
        .collect()
}

/// A field as text (numbers and booleans too; missing or null = "").
fn field(m: &Map<String, Value>, key: &str) -> String {
    match m.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn flag(m: &Map<String, Value>, key: &str) -> bool {
    match m.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// `docker context ls --format '{{json .}}'`.
pub fn parse_contexts(text: &str) -> Vec<DockerContext> {
    parse_json_lines(text)
        .iter()
        .map(|m| DockerContext {
            name: field(m, "Name"),
            description: field(m, "Description"),
            endpoint: field(m, "DockerEndpoint"),
            current: flag(m, "Current"),
            error: field(m, "Error"),
        })
        .filter(|c| !c.name.is_empty())
        .collect()
}

/// `docker ps -a --format '{{json .}}'`.
pub fn parse_containers(text: &str) -> Vec<Container> {
    parse_json_lines(text)
        .iter()
        .map(|m| Container {
            id: field(m, "ID"),
            name: field(m, "Names").split(',').next().unwrap_or("").trim_start_matches('/').to_string(),
            image: field(m, "Image"),
            command: field(m, "Command").trim_matches('"').to_string(),
            state: field(m, "State").to_ascii_lowercase(),
            status: field(m, "Status"),
            ports: field(m, "Ports"),
            created: field(m, "CreatedAt"),
            labels: parse_labels(&field(m, "Labels")),
        })
        .filter(|c| !c.id.is_empty())
        .collect()
}

/// `docker images --format '{{json .}}'`.
pub fn parse_images(text: &str) -> Vec<Image> {
    parse_json_lines(text)
        .iter()
        .map(|m| Image {
            id: field(m, "ID"),
            repository: field(m, "Repository"),
            tag: field(m, "Tag"),
            size: field(m, "Size"),
            created: field(m, "CreatedSince"),
        })
        .filter(|i| !i.id.is_empty())
        .collect()
}

/// `docker volume ls --format '{{json .}}'`.
pub fn parse_volumes(text: &str) -> Vec<Volume> {
    parse_json_lines(text)
        .iter()
        .map(|m| Volume { name: field(m, "Name"), driver: field(m, "Driver"), mountpoint: field(m, "Mountpoint") })
        .filter(|v| !v.name.is_empty())
        .collect()
}

/// `docker network ls --format '{{json .}}'`.
pub fn parse_networks(text: &str) -> Vec<Network> {
    parse_json_lines(text)
        .iter()
        .map(|m| Network {
            id: field(m, "ID"),
            name: field(m, "Name"),
            driver: field(m, "Driver"),
            scope: field(m, "Scope"),
        })
        .filter(|n| !n.id.is_empty())
        .collect()
}

/// The `Labels` column: `k=v,k2=v2`. A value may itself contain commas (Compose's
/// `config_files`), so a piece that does not start with a label key continues the previous value.
pub fn parse_labels(s: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for piece in s.split(',') {
        let key = piece.split_once('=').map(|(k, _)| k);
        let is_key = key.is_some_and(|k| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        });
        match (is_key, piece.split_once('='), out.last_mut()) {
            (true, Some((k, v)), _) => out.push((k.to_string(), v.to_string())),
            (_, _, Some((_, last))) => {
                last.push(',');
                last.push_str(piece);
            }
            _ => {}
        }
    }
    out
}

/// Containers grouped by Compose project, in order of first appearance.
pub fn compose_projects(containers: &[Container]) -> Vec<ComposeProject> {
    let mut out: Vec<ComposeProject> = Vec::new();
    for c in containers {
        let Some(name) = c.compose_project() else { continue };
        let i = match out.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                out.push(ComposeProject { name: name.to_string(), ..Default::default() });
                out.len() - 1
            }
        };
        let Some(p) = out.get_mut(i) else { continue };
        if p.working_dir.is_empty()
            && let Some(w) = c.label("com.docker.compose.project.working_dir")
        {
            p.working_dir = w.to_string();
        }
        if p.config_files.is_empty()
            && let Some(f) = c.label("com.docker.compose.project.config_files")
        {
            p.config_files = f.split(',').map(str::trim).filter(|f| !f.is_empty()).map(String::from).collect();
        }
        p.containers.push(c.id.clone());
        if c.is_running() {
            p.running += 1;
        }
    }
    out
}

/// The Docker CLI config: registries with credentials and the credential store.
pub fn parse_registry_config(text: &str) -> RegistryStatus {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut registries: Vec<String> = Vec::new();
    for key in ["auths", "credHelpers"] {
        if let Some(m) = v.get(key).and_then(Value::as_object) {
            for k in m.keys() {
                if !registries.contains(k) {
                    registries.push(k.clone());
                }
            }
        }
    }
    let creds_store = v.get("credsStore").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    RegistryStatus { registries, creds_store, config_path: String::new() }
}

/// Pretty-printed JSON, or the text unchanged when it is not JSON.
pub fn pretty_json(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.to_string())
}

// ---------------------------------------------------------------- running the CLI

/// A finished CLI call.
#[derive(Clone, Debug, Default)]
pub(crate) struct CliOutput {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    /// stdout on success, else the error text.
    fn into_result(self) -> Result<String, String> {
        if self.ok {
            Ok(self.stdout)
        } else {
            let e = self.stderr.trim();
            Err(if e.is_empty() { self.stdout.trim().to_string() } else { e.to_string() })
        }
    }
}

fn read_capped(r: Option<impl Read>) -> String {
    let mut buf = Vec::new();
    if let Some(r) = r {
        let _ = r.take(MAX_OUTPUT).read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run `program args…` with stdin closed and wait (at most `timeout`) for its output. Called on
/// worker threads only.
pub(crate) fn run_cli(program: &Path, args: &[String], timeout: Duration) -> Result<CliOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out = std::thread::spawn(move || read_capped(out));
    let err = std::thread::spawn(move || read_capped(err));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status {
        Some(st) => Ok(CliOutput { ok: st.success(), stdout, stderr }),
        None => Err(format!("{} timed out after {} s", program.display(), timeout.as_secs())),
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

fn with_base(base: &[String], args: &[&str]) -> Vec<String> {
    base.iter().cloned().chain(args.iter().map(|a| (*a).to_string())).collect()
}

/// Lists of one connection; a failing `ps` (daemon down) fails the whole refresh.
fn load_snapshot(program: &Path, base: &[String]) -> Result<Snapshot, String> {
    let list = |args: &[&str]| -> Result<String, String> {
        run_cli(program, &with_base(base, args), LIST_TIMEOUT)?.into_result()
    };
    let containers = parse_containers(&list(&["ps", "-a", "--format", "{{json .}}"])?);
    let images = list(&["images", "--format", "{{json .}}"]).map(|t| parse_images(&t)).unwrap_or_default();
    let volumes = list(&["volume", "ls", "--format", "{{json .}}"]).map(|t| parse_volumes(&t)).unwrap_or_default();
    let networks = list(&["network", "ls", "--format", "{{json .}}"]).map(|t| parse_networks(&t)).unwrap_or_default();
    Ok(Snapshot { containers, images, volumes, networks })
}

// ---------------------------------------------------------------- actions

/// Load the connections (`docker context ls`) in the background.
pub fn load_contexts(s: &mut Session) {
    let program = s.docker.program_path();
    s.docker.contexts_loaded = true;
    s.docker.spawn(move || {
        let r = run_cli(&program, &strings(&["context", "ls", "--format", "{{json .}}"]), LIST_TIMEOUT)
            .and_then(CliOutput::into_result)
            .map(|t| parse_contexts(&t));
        Msg::Contexts(r)
    });
}

/// Reload containers, images, volumes and networks of the selected connection in the background.
pub fn refresh(s: &mut Session) {
    let d = &mut s.docker;
    d.generation = d.generation.wrapping_add(1);
    d.loading = true;
    d.last_refresh = Some(Instant::now());
    let (program, base, generation) = (d.program_path(), d.base_args(), d.generation);
    d.spawn(move || Msg::Snapshot { generation, result: load_snapshot(&program, &base) });
}

/// Called while the Docker section is visible: loads the connections once, then refreshes every
/// [`REFRESH_EVERY`].
pub fn auto_refresh(s: &mut Session) {
    if !s.docker.contexts_loaded {
        load_contexts(s);
    }
    let due = s.docker.last_refresh.is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
    if due && !s.docker.loading {
        refresh(s);
    }
}

/// Select a connection (`None` = the CLI's current context) and reload.
pub fn select_connection(s: &mut Session, name: Option<String>) {
    if s.docker.connection != name {
        s.docker.connection = name;
        s.docker.data = Snapshot::default();
        s.docker.loaded = false;
        s.docker.error = None;
    }
    refresh(s);
}

/// Run `docker <args>` (after the connection's `--context`) in the background; the output goes
/// to the operations log and the lists reload afterwards when `refresh`.
pub fn run_op(s: &mut Session, label: impl Into<String>, args: Vec<String>, refresh: bool) {
    let label = label.into();
    let timeout = if args.first().is_some_and(|a| a == "pull") { PULL_TIMEOUT } else { OP_TIMEOUT };
    let d = &mut s.docker;
    let (program, mut full) = (d.program_path(), d.base_args());
    full.extend(args);
    d.operations.push(label.clone());
    s.status = format!("{label}…");
    d.spawn(move || {
        let result = run_cli(&program, &full, timeout).and_then(CliOutput::into_result);
        Msg::Op { label, result, refresh }
    });
}

/// `docker inspect` of a container, image, volume or network, shown as JSON.
pub fn inspect(s: &mut Session, kind: &str, id: &str) {
    let args: Vec<String> = match kind {
        "image" => strings(&["image", "inspect", id]),
        "volume" => strings(&["volume", "inspect", id]),
        "network" => strings(&["network", "inspect", id]),
        _ => strings(&["inspect", id]),
    };
    let d = &mut s.docker;
    let (program, mut full) = (d.program_path(), d.base_args());
    full.extend(args);
    let title = format!("Inspect {id}");
    d.spawn(move || {
        let result = run_cli(&program, &full, LIST_TIMEOUT).and_then(CliOutput::into_result).map(|t| pretty_json(&t));
        Msg::Inspect { title, result }
    });
}

/// List the Dev Containers (containers labelled `devcontainer.local_folder`) in the background.
pub fn load_dev_containers(s: &mut Session) {
    let d = &mut s.docker;
    let (program, base) = (d.program_path(), d.base_args());
    d.spawn(move || {
        let args =
            with_base(&base, &["ps", "-a", "--filter", "label=devcontainer.local_folder", "--format", "{{json .}}"]);
        Msg::DevContainers(
            run_cli(&program, &args, LIST_TIMEOUT).and_then(CliOutput::into_result).map(|t| parse_containers(&t)),
        )
    });
}

/// Follow a container's log (`docker logs -f --tail <tail>`) in a log view; returns its id.
pub fn open_logs(s: &mut Session, container: &str, tail: usize) -> Result<u64, String> {
    if let Some(existing) = s.docker.logs.iter().find(|l| l.container == container && l.running) {
        let id = existing.id;
        s.docker.active_log = Some(id);
        s.docker.inspect = None;
        return Ok(id);
    }
    // Drop the oldest finished view when there are too many.
    if s.docker.logs.len() >= MAX_LOG_VIEWS {
        match s.docker.logs.iter().position(|l| !l.running) {
            Some(i) => {
                s.docker.logs.remove(i);
            }
            None => return Err(format!("too many log views (at most {MAX_LOG_VIEWS})")),
        }
    }
    let mut args = s.docker.base_args();
    args.extend(strings(&["logs", "-f", "--tail", &tail.to_string(), container]));
    let cwd = s.project_root().filter(|p| p.is_dir()).unwrap_or_else(std::env::temp_dir);
    let (handle, events) =
        codecraft_cargo::spawn_process(&s.docker.program_path(), &args, &cwd, &[], false).map_err(|e| e.to_string())?;
    let id = s.docker.next_log;
    s.docker.next_log += 1;
    let title = s.docker.data.container(container).map(|c| c.name.clone()).unwrap_or_else(|| container.to_string());
    s.docker.logs.push(LogView {
        id,
        container: container.to_string(),
        title,
        lines: VecDeque::new(),
        running: true,
        handle: Some(handle),
        events: Some(events),
    });
    s.docker.active_log = Some(id);
    s.docker.inspect = None;
    crate::services::show(s, ServicesSection::Docker);
    Ok(id)
}

/// Close a log view, stopping its `docker logs -f`.
pub fn close_logs(s: &mut Session, id: u64) -> bool {
    let Some(i) = s.docker.logs.iter().position(|l| l.id == id) else { return false };
    let mut view = s.docker.logs.remove(i);
    view.stop();
    if s.docker.active_log == Some(id) {
        s.docker.active_log = s.docker.logs.last().map(|l| l.id);
    }
    true
}

/// Docker CLI config directory: the override, `$DOCKER_CONFIG`, else `~/.docker`.
pub fn config_dir(s: &Session) -> Option<PathBuf> {
    s.docker
        .config_dir
        .clone()
        .or_else(|| std::env::var_os("DOCKER_CONFIG").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker")))
}

/// Read the registry login status from the Docker CLI config.
pub fn registry_status(s: &mut Session) -> RegistryStatus {
    let path = config_dir(s).map(|d| d.join("config.json"));
    let text =
        path.as_deref().filter(|p| p.is_file()).and_then(|p| codecraft_fs::read_text_file(p).ok()).unwrap_or_default();
    let mut status = parse_registry_config(&text);
    status.config_path = path.map(|p| p.display().to_string()).unwrap_or_default();
    s.docker.registry = Some(status.clone());
    status
}

/// Kill every log follower (session shutdown).
pub(crate) fn shutdown(s: &mut Session) {
    for l in &mut s.docker.logs {
        l.stop();
    }
}

// ---------------------------------------------------------------- pump

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let msgs: Vec<Msg> = s.docker.channel.as_ref().map(|(_, rx)| rx.try_iter().collect()).unwrap_or_default();
    let mut changed = !msgs.is_empty();
    for m in msgs {
        s.docker.pending = s.docker.pending.saturating_sub(1);
        handle(s, m);
    }
    changed |= pump_logs(&mut s.docker);
    changed
}

fn handle(s: &mut Session, m: Msg) {
    match m {
        Msg::Contexts(Ok(list)) => s.docker.contexts = list,
        Msg::Contexts(Err(e)) => s.docker.push_output(&format!("docker context ls: {e}")),
        Msg::Snapshot { generation, result } => {
            if generation != s.docker.generation {
                return;
            }
            s.docker.loading = false;
            s.docker.loaded = true;
            match result {
                Ok(snap) => {
                    s.docker.data = snap;
                    s.docker.error = None;
                }
                Err(e) => s.docker.error = Some(e),
            }
        }
        Msg::DevContainers(Ok(list)) => s.docker.dev_containers = list,
        Msg::DevContainers(Err(e)) => s.docker.push_output(&format!("Dev Containers: {e}")),
        Msg::Op { label, result, refresh: then_refresh } => {
            if let Some(i) = s.docker.operations.iter().position(|o| *o == label) {
                s.docker.operations.remove(i);
            }
            match result {
                Ok(out) => {
                    s.docker.push_output(&format!("{label}: done"));
                    s.docker.push_output(&out);
                    s.status = format!("{label}: done");
                }
                Err(e) => {
                    s.docker.push_output(&format!("{label}: {e}"));
                    s.notify(NotificationLevel::Error, &label, e);
                }
            }
            if then_refresh {
                refresh(s);
            }
        }
        Msg::Inspect { title, result } => match result {
            Ok(json) => {
                s.docker.inspect = Some(Inspect { title, json });
                s.docker.active_log = None;
            }
            Err(e) => s.notify(NotificationLevel::Error, &title, e),
        },
    }
}

/// Move log lines from the followers into their views (bounded work per call).
fn pump_logs(d: &mut DockerState) -> bool {
    let mut changed = false;
    for view in &mut d.logs {
        let Some(rx) = &view.events else { continue };
        let mut ended = false;
        for ev in rx.try_iter().take(10_000) {
            changed = true;
            match ev {
                RunEvent::Stdout(l) | RunEvent::Stderr(l) => view.lines.push_back(l),
                RunEvent::Error(e) => view.lines.push_back(format!("error: {e}")),
                RunEvent::Exited { code, .. } => {
                    view.lines.push_back(match code {
                        Some(c) => format!("[docker logs exited with code {c}]"),
                        None => "[docker logs stopped]".to_string(),
                    });
                    ended = true;
                }
                RunEvent::Started { .. } | RunEvent::Message(_) | RunEvent::Test(_) => {}
            }
        }
        while view.lines.len() > MAX_LOG_LINES {
            view.lines.pop_front();
        }
        if ended {
            view.running = false;
            view.handle = None;
            view.events = None;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = r#"{"Command":"\"app --port 80\"","CreatedAt":"2026-10-09 17:06:53 -0400 EDT","ID":"12cf16b8f0f6","Image":"web:latest","Labels":"com.docker.compose.project=shop,com.docker.compose.service=web,com.docker.compose.project.config_files=/p/compose.yaml,/p/compose.override.yaml,com.docker.compose.project.working_dir=/p","Names":"shop-web-1","Ports":"0.0.0.0:8080->80/tcp","State":"running","Status":"Up 3 days"}
{"Command":"\"postgres\"","ID":"a5f5452bda4c","Image":"postgres:16","Labels":"com.docker.compose.project=shop,com.docker.compose.service=db","Names":"shop-db-1","State":"exited","Status":"Exited (0) 2 hours ago"}
not json
{"ID":"ffff","Image":"alpine","Labels":"","Names":"/lonely,alias","State":"Paused","Status":"Up (Paused)","Size":12}
{"Names":"no-id"}"#;

    #[test]
    fn parses_containers_and_groups_compose_projects() {
        let cs = parse_containers(PS);
        assert_eq!(cs.len(), 3);
        let web = &cs[0];
        assert_eq!(
            (web.name.as_str(), web.command.as_str(), web.state.as_str()),
            ("shop-web-1", "app --port 80", "running")
        );
        assert_eq!(web.compose_service(), Some("web"));
        assert_eq!(
            web.label("com.docker.compose.project.config_files"),
            Some("/p/compose.yaml,/p/compose.override.yaml")
        );
        assert_eq!(cs[2].name, "lonely");
        assert!(cs[2].is_paused() && !cs[2].is_running());
        let projects = compose_projects(&cs);
        assert_eq!(projects.len(), 1);
        let p = &projects[0];
        assert_eq!(p.name, "shop");
        assert_eq!(p.containers, vec!["12cf16b8f0f6".to_string(), "a5f5452bda4c".to_string()]);
        assert_eq!(p.running, 1);
        assert_eq!(p.working_dir, "/p");
        assert_eq!(p.config_files, vec!["/p/compose.yaml".to_string(), "/p/compose.override.yaml".to_string()]);
    }

    #[test]
    fn parses_contexts_images_volumes_networks() {
        let ctx = parse_contexts(
            "{\"Current\":false,\"Description\":\"d\",\"DockerEndpoint\":\"unix:///a.sock\",\"Error\":\"\",\"Name\":\"default\"}\n{\"Current\":true,\"DockerEndpoint\":\"ssh://h\",\"Name\":\"remote\"}\n",
        );
        assert_eq!(ctx.len(), 2);
        assert!(ctx[1].current && !ctx[0].current);
        assert_eq!(ctx[0].endpoint, "unix:///a.sock");
        let images = parse_images(
            "{\"ID\":\"15541b430d4e\",\"Repository\":\"ghcr.io/x/web\",\"Tag\":\"0.1\",\"Size\":\"71MB\",\"CreatedSince\":\"1 day ago\"}\n{\"ID\":\"abc\",\"Repository\":\"<none>\",\"Tag\":\"<none>\"}",
        );
        assert_eq!(images[0].reference(), "ghcr.io/x/web:0.1");
        assert_eq!(images[1].reference(), "abc");
        let vols = parse_volumes("{\"Driver\":\"local\",\"Mountpoint\":\"/var/lib/x\",\"Name\":\"data\"}");
        assert_eq!(vols[0].name, "data");
        let nets = parse_networks("{\"Driver\":\"bridge\",\"ID\":\"9a54\",\"Name\":\"bridge\",\"Scope\":\"local\"}");
        assert_eq!((nets[0].id.as_str(), nets[0].driver.as_str()), ("9a54", "bridge"));
    }

    #[test]
    fn labels_keep_commas_inside_values() {
        assert_eq!(
            parse_labels("a=1,b=x,y,/z.yml,c.d-e_f=,=bad"),
            vec![("a".into(), "1".into()), ("b".into(), "x,y,/z.yml".into()), ("c.d-e_f".into(), ",=bad".into())]
        );
        assert!(parse_labels("").is_empty());
        assert!(parse_labels(",,,").is_empty());
    }

    #[test]
    fn registry_config_and_hostile_input() {
        let st = parse_registry_config(
            r#"{"auths":{"ghcr.io":{},"https://index.docker.io/v1/":{"auth":"x"}},"credHelpers":{"ghcr.io":"gh","ecr":"aws"},"credsStore":"desktop"}"#,
        );
        assert_eq!(st.registries, vec!["ghcr.io", "https://index.docker.io/v1/", "ecr"]);
        assert_eq!(st.creds_store.as_deref(), Some("desktop"));
        assert_eq!(parse_registry_config("garbage"), RegistryStatus::default());
        for junk in ["", "{", "[1,2]", "{\"ID\":1}", "\u{0}"] {
            let _ = parse_containers(junk);
            let _ = parse_contexts(junk);
            let _ = parse_images(junk);
        }
        assert_eq!(parse_containers("{\"ID\":7,\"Names\":null}")[0].id, "7");
        assert_eq!(pretty_json("[1]"), "[\n  1\n]");
        assert_eq!(pretty_json("nope"), "nope");
    }

    #[test]
    fn prune_kinds_round_trip() {
        for k in PruneKind::ALL {
            assert_eq!(PruneKind::parse(k.key()), Some(k));
            assert!(k.args().iter().any(|a| a == "-f"));
        }
        assert_eq!(PruneKind::parse("everything"), None);
    }
}
