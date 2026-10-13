//! Python interpreters, packages and the Python Console (M11 track Y).
//!
//! * **Interpreters** ([`crate::python_env`]): the project's selection lives in
//!   `<project>/.codecraft/python.json`; without one the project's virtualenv is used, else
//!   `python3`. Run configurations, debugpy, pytest and the Python language server all ask
//!   [`crate::python_env::project_python`]. Detection runs on a worker thread.
//! * **Packages**: `uv pip` for virtualenvs when uv is installed, else `<python> -m pip`: list
//!   (`--format json`), outdated, install, upgrade and uninstall run as background processes whose
//!   output is kept in [`PythonState::log`]; the list refreshes after each change.
//! * **Console** ([`crate::python_console`]).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crossbeam_channel::{Receiver, TryRecvError, bounded};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use codecraft_cargo::{RunEvent, RunHandle};
use codecraft_workspace::{NotificationLevel, ToolWindowId};

use crate::python_console::Console;
use crate::python_env::{self, Interpreter, Probe};
use crate::{LspStatus, ServerKind, Session};

/// Lines kept in the package operations log.
const MAX_LOG_LINES: usize = 2000;
/// Stdout lines kept from a listing.
const MAX_LIST_LINES: usize = 100_000;
/// Packages kept from a listing.
const MAX_PACKAGES: usize = 20_000;

/// An installed package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// The newest version when it is newer (from the outdated listing).
    pub latest: Option<String>,
}

/// What a background process does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PkgOp {
    List { outdated: bool, then_outdated: bool },
    Install { spec: String },
    Upgrade { name: String },
    Uninstall { name: String },
    CreateVenv { dir: PathBuf, select: bool },
}

impl PkgOp {
    fn title(&self) -> String {
        match self {
            Self::List { outdated: false, .. } => "Listing packages".into(),
            Self::List { outdated: true, .. } => "Checking for updates".into(),
            Self::Install { spec } => format!("Installing {spec}"),
            Self::Upgrade { name } => format!("Upgrading {name}"),
            Self::Uninstall { name } => format!("Uninstalling {name}"),
            Self::CreateVenv { dir, .. } => format!("Creating virtualenv {}", dir.display()),
        }
    }

    fn is_list(&self) -> bool {
        matches!(self, Self::List { .. })
    }
}

/// A running package or virtualenv process.
pub struct PkgJob {
    pub op: PkgOp,
    pub title: String,
    handle: RunHandle,
    rx: Receiver<RunEvent>,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl std::fmt::Debug for PkgJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkgJob").field("op", &self.op).finish()
    }
}

/// Tools › Create Virtualenv… dialog (from the interpreter popup).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateVenvDialog {
    /// Relative to the project (`.venv`) or absolute.
    pub dir: String,
    /// The base interpreter; `None` = the default.
    pub base: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct PythonState {
    /// The project these values belong to.
    pub root: Option<PathBuf>,
    /// The interpreter in use (selection or virtualenv); `None` = `python3` from `PATH`.
    pub interpreter: Option<PathBuf>,
    /// Whether the user chose it (vs. the project's virtualenv by default).
    pub explicit: bool,
    /// The project looks like a Python project (checked when it opens).
    pub python_project: bool,
    pub interpreters: Vec<Interpreter>,
    /// Detection has run for this project.
    pub detected: bool,
    detect_rx: Option<Receiver<Vec<Interpreter>>>,
    pub packages: Vec<Package>,
    /// The interpreter the package list belongs to.
    pub packages_for: Option<PathBuf>,
    pub packages_error: Option<String>,
    /// Search box of the Python Packages tool window.
    pub filter: String,
    /// The package selected in the tool window.
    pub selected_package: Option<String>,
    /// Install fields of the tool window.
    pub install_name: String,
    pub install_version: String,
    pub jobs: Vec<PkgJob>,
    /// Output of package operations (newest last).
    pub log: Vec<String>,
    /// A package waiting for the uninstall confirmation.
    pub confirm_uninstall: Option<String>,
    pub create_venv: Option<CreateVenvDialog>,
    /// Tools › Run setup.py Task… chooser (the typed task).
    pub setup_task: Option<String>,
    pub console: Console,
    /// Detection and tool lookup (tests point it at fake directories).
    pub probe: Option<Probe>,
}

impl PythonState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        self.detect_rx.is_some() || !self.jobs.is_empty() || self.console.running.is_some()
    }

    /// The probe in use.
    pub fn probe(&self) -> Probe {
        self.probe.clone().unwrap_or_else(Probe::from_env)
    }

    /// The interpreter program to run (`python3` when none is configured).
    pub fn python_program(&self) -> PathBuf {
        self.interpreter.clone().or_else(|| codecraft_lsp::which("python3")).unwrap_or_else(|| PathBuf::from("python3"))
    }

    /// `Python 3.12 (.venv)`, or `python3` when nothing is configured.
    pub fn interpreter_label(&self) -> String {
        let Some(path) = &self.interpreter else { return "Python: python3".into() };
        if let Some(i) = self.interpreters.iter().find(|i| &i.path == path) {
            return i.label();
        }
        let venv = python_env::venv_of(path);
        let name = venv
            .as_deref()
            .and_then(Path::file_name)
            .or_else(|| path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match venv.as_deref().and_then(python_env::pyvenv_version) {
            Some(v) => format!("Python {} ({name})", python_env::short_version(&v)),
            None => format!("Python ({name})"),
        }
    }

    /// Installed packages matching the search box.
    pub fn visible_packages(&self) -> Vec<&Package> {
        let q = self.filter.trim().to_ascii_lowercase();
        self.packages.iter().filter(|p| q.is_empty() || p.name.to_ascii_lowercase().contains(&q)).collect()
    }

    /// Whether a package operation (not a listing) is running.
    pub fn changing(&self) -> bool {
        self.jobs.iter().any(|j| !j.op.is_list())
    }

    fn log(&mut self, line: impl Into<String>) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.drain(..MAX_LOG_LINES / 4);
        }
        self.log.push(line.into());
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = follow_project(s);
    // Detection.
    if let Some(rx) = &s.python.detect_rx {
        match rx.try_recv() {
            Ok(list) => {
                s.python.interpreters = list;
                s.python.detect_rx = None;
                s.python.detected = true;
                changed = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                s.python.detect_rx = None;
                s.python.detected = true;
                changed = true;
            }
        }
    }
    // Package processes.
    let mut finished = Vec::new();
    for (i, job) in s.python.jobs.iter_mut().enumerate() {
        loop {
            match job.rx.try_recv() {
                Ok(RunEvent::Stdout(line)) => {
                    changed = true;
                    if job.stdout.len() < MAX_LIST_LINES {
                        job.stdout.push(line);
                    }
                }
                Ok(RunEvent::Stderr(line)) | Ok(RunEvent::Error(line)) => {
                    changed = true;
                    if job.stderr.len() < MAX_LOG_LINES {
                        job.stderr.push(crate::runs::strip_ansi(&line));
                    }
                }
                Ok(RunEvent::Exited { success, .. }) => {
                    finished.push((i, success));
                    break;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished.push((i, false));
                    break;
                }
            }
        }
    }
    for (i, success) in finished.into_iter().rev() {
        if i < s.python.jobs.len() {
            let job = s.python.jobs.remove(i);
            finish_job(s, job, success);
            changed = true;
        }
    }
    // Console.
    let poll = s.python.console.poll();
    changed |= poll.changed;
    if let Some(status) = poll.finished {
        changed = true;
        let c = &mut s.python.console;
        if let Some(input) = c.pending_input.take() {
            if status == "incomplete" {
                // The driver wants more lines: take the input back and continue it.
                c.take_back_input();
                c.draft = crate::python_console::continue_draft(&input);
            } else {
                c.remember(&input);
            }
        }
    }
    changed
}

/// Reset per-project state when the project changes.
fn follow_project(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root == s.python.root {
        return false;
    }
    s.python.root = root.clone();
    s.python.interpreters.clear();
    s.python.detected = false;
    s.python.detect_rx = None;
    s.python.packages.clear();
    s.python.packages_for = None;
    s.python.packages_error = None;
    s.python.confirm_uninstall = None;
    s.python.create_venv = None;
    s.python.setup_task = None;
    for job in s.python.jobs.drain(..) {
        let _ = job.handle.kill();
    }
    s.python.console.stop();
    refresh_interpreter(s);
    true
}

/// Re-read the project's interpreter (after a selection or a new virtualenv).
pub(crate) fn refresh_interpreter(s: &mut Session) {
    let root = s.python.root.clone();
    s.python.explicit = root.as_deref().and_then(python_env::selected_interpreter).is_some();
    s.python.python_project = root.as_deref().is_some_and(python_env::is_python_project);
    s.python.interpreter = root.as_deref().and_then(python_env::project_python);
}

/// Stop the console and package processes (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    s.python.console.stop();
    for job in s.python.jobs.drain(..) {
        let _ = job.handle.kill();
    }
}

/// Detect interpreters on a worker thread.
pub(crate) fn start_detect(s: &mut Session) {
    if s.python.detect_rx.is_some() {
        return;
    }
    let root = s.python.root.clone().or_else(|| s.project_root());
    let probe = s.python.probe();
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(python_env::detect(root.as_deref(), &probe));
    });
    s.python.detect_rx = Some(rx);
}

/// Select (or with `None` clear) the project's interpreter: persisted, then the Python language
/// server restarts with it.
pub(crate) fn select(s: &mut Session, path: Option<&Path>) -> Result<(), String> {
    let root = s.project_root().ok_or("no project is open")?;
    if let Some(p) = path
        && !p.is_file()
    {
        return Err(format!("{} is not an interpreter", p.display()));
    }
    let old = crate::run_configs::python_for(&root);
    python_env::save_selection(&root, path)?;
    s.python.explicit = path.is_some();
    s.python.interpreter = python_env::project_python(&root);
    // Derived Python run configurations (pytest, manage.py, …) follow the new interpreter.
    let new = crate::run_configs::python_for(&root);
    for c in &mut s.run_store.derived {
        if let Some(cmd) = &mut c.command
            && cmd.program == old
        {
            cmd.program = new.clone();
        }
    }
    crate::run_config_store::rebuild(s);
    s.python.packages.clear();
    s.python.packages_for = None;
    s.status = format!("Python interpreter: {}", s.python.interpreter_label());
    if s.lsp_enabled && !matches!(s.server_status(ServerKind::Python), LspStatus::Off) {
        s.restart_server(Some(ServerKind::Python));
    }
    Ok(())
}

// ---------------------------------------------------------------- packages

/// `pip list --format json` (and `--outdated`) output → packages.
pub fn parse_pip_list(json: &str) -> Result<Vec<Package>, String> {
    // pip may print notices before the JSON (`[notice] A new release…` goes to stderr, but
    // wrappers sometimes print to stdout).
    let start = json.find('[').ok_or("no package list in the output")?;
    let body = json.get(start..).unwrap_or("");
    let v: Value = serde_json::from_str(body.trim()).map_err(|e| format!("cannot read the package list: {e}"))?;
    let items = v.as_array().ok_or("the package list is not an array")?;
    let mut out: Vec<Package> = items
        .iter()
        .take(MAX_PACKAGES)
        .filter_map(|i| {
            Some(Package {
                name: i.get("name").and_then(Value::as_str)?.to_string(),
                version: i.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
                latest: i.get("latest_version").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect();
    out.sort_by_key(|p| p.name.to_ascii_lowercase());
    Ok(out)
}

/// PEP 503 normalized name (`Foo_Bar` → `foo-bar`).
pub fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// A requirement from a name and an optional version: `requests`, `requests==2.31.0`,
/// `requests>=2`. Names and versions are checked so nothing reaches pip as an option.
pub fn requirement(name: &str, version: Option<&str>) -> Result<String, String> {
    static NAME: OnceLock<Option<Regex>> = OnceLock::new();
    static VERSION: OnceLock<Option<Regex>> = OnceLock::new();
    let name_re = NAME
        .get_or_init(|| {
            Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,200}[A-Za-z0-9])?(?:\[[A-Za-z0-9._,-]{1,200}\])?$").ok()
        })
        .as_ref()
        .ok_or("internal error: bad name pattern")?;
    let version_re = VERSION
        .get_or_init(|| Regex::new(r"^(?:[=<>!~]{1,3}\s*)?[A-Za-z0-9*][A-Za-z0-9.*+!_-]{0,100}(?:\s*,\s*[=<>!~]{1,3}\s*[A-Za-z0-9*][A-Za-z0-9.*+!_-]{0,100}){0,5}$").ok())
        .as_ref()
        .ok_or("internal error: bad version pattern")?;
    let name = name.trim();
    if !name_re.is_match(name) {
        return Err(format!("`{name}` is not a valid package name"));
    }
    match version.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(name.to_string()),
        Some(v) if !version_re.is_match(v) => Err(format!("`{v}` is not a valid version")),
        Some(v) if v.starts_with(['=', '<', '>', '!', '~']) => Ok(format!("{name}{}", v.replace(' ', ""))),
        Some(v) => Ok(format!("{name}=={v}")),
    }
}

/// The program and arguments for a package operation: `uv pip …` for a virtualenv when uv is
/// installed, else `<python> -m pip …`.
pub fn pip_command(python: &Path, uv: Option<&Path>, op: &PkgOp) -> Option<(PathBuf, Vec<String>)> {
    let py = python.display().to_string();
    let use_uv = uv.filter(|_| python_env::venv_of(python).is_some());
    let (program, mut args): (PathBuf, Vec<String>) = match use_uv {
        Some(uv) => (uv.to_path_buf(), vec!["pip".into()]),
        None => (python.to_path_buf(), vec!["-m".into(), "pip".into()]),
    };
    let uv_python = |args: &mut Vec<String>| {
        if use_uv.is_some() {
            args.extend(["--python".to_string(), py.clone()]);
        }
    };
    match op {
        PkgOp::List { outdated, .. } => {
            args.extend(["list".into(), "--format".into(), "json".into()]);
            if *outdated {
                args.push("--outdated".into());
            }
            uv_python(&mut args);
        }
        PkgOp::Install { spec } => {
            args.push("install".into());
            uv_python(&mut args);
            args.push(spec.clone());
        }
        PkgOp::Upgrade { name } => {
            args.extend(["install".into(), "--upgrade".into()]);
            uv_python(&mut args);
            args.push(name.clone());
        }
        PkgOp::Uninstall { name } => {
            args.push("uninstall".into());
            if use_uv.is_some() {
                uv_python(&mut args);
            } else {
                args.push("-y".into());
            }
            args.push(name.clone());
        }
        PkgOp::CreateVenv { .. } => return None,
    }
    if use_uv.is_none() {
        args.push("--disable-pip-version-check".into());
    }
    Some((program, args))
}

fn spawn_job(s: &mut Session, op: PkgOp, program: &Path, args: &[String]) -> Result<(), String> {
    let cwd = s.python.root.clone().or_else(|| s.project_root()).unwrap_or_else(std::env::temp_dir);
    let (handle, rx) = codecraft_cargo::spawn_process(program, args, &cwd, &[], false)
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let title = op.title();
    if !op.is_list() {
        let line = format!(
            "$ {} {}",
            program.display(),
            args.iter().map(|a| codecraft_cargo::shell_quote(a)).collect::<Vec<_>>().join(" ")
        );
        s.python.log(line);
        s.status = format!("{title}…");
    }
    s.python.jobs.push(PkgJob { op, title, handle, rx, stdout: Vec::new(), stderr: Vec::new() });
    Ok(())
}

/// Start a package operation for the project's interpreter.
pub(crate) fn start_package_op(s: &mut Session, op: PkgOp) -> Result<(), String> {
    if !op.is_list() && s.python.changing() {
        return Err("another package operation is running".into());
    }
    if op.is_list() {
        // One listing at a time.
        if s.python.jobs.iter().any(|j| j.op.is_list()) {
            return Ok(());
        }
    }
    let python = s.python.python_program();
    let uv = s.python.probe().which("uv");
    let (program, args) = pip_command(&python, uv.as_deref(), &op).ok_or("not a package operation")?;
    if let PkgOp::List { outdated: false, .. } = op {
        s.python.packages_error = None;
    }
    spawn_job(s, op, &program, &args)
}

/// Create a virtualenv (`uv venv` or `python -m venv`), optionally selecting it when done.
pub(crate) fn create_venv(s: &mut Session, dir: &Path, base: Option<&Path>, select: bool) -> Result<(), String> {
    if dir.exists() && dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} already exists and is not empty", dir.display()));
    }
    if s.python.jobs.iter().any(|j| matches!(j.op, PkgOp::CreateVenv { .. })) {
        return Err("a virtualenv is already being created".into());
    }
    let uv = s.python.probe().which("uv");
    let (program, args) = python_env::venv_command(dir, base, uv.as_deref());
    spawn_job(s, PkgOp::CreateVenv { dir: dir.to_path_buf(), select }, &program, &args)
}

fn finish_job(s: &mut Session, job: PkgJob, success: bool) {
    let PkgJob { op, title, stdout, stderr, .. } = job;
    if !op.is_list() {
        for l in stdout.iter().chain(stderr.iter()) {
            s.python.log(l.clone());
        }
    }
    let why = || stderr.iter().rev().find(|l| !l.trim().is_empty()).cloned().unwrap_or_else(|| "it failed".into());
    match op {
        PkgOp::List { outdated, then_outdated } => {
            if !success {
                if !outdated {
                    s.python.packages_error = Some(format!("Cannot list packages: {}", why()));
                }
                return;
            }
            match parse_pip_list(&stdout.join("\n")) {
                Ok(list) if outdated => {
                    for p in &mut s.python.packages {
                        let n = normalize_name(&p.name);
                        p.latest = list.iter().find(|o| normalize_name(&o.name) == n).and_then(|o| o.latest.clone());
                    }
                }
                Ok(list) => {
                    // Keep the known newest versions of packages that did not change.
                    let old = std::mem::take(&mut s.python.packages);
                    s.python.packages = list
                        .into_iter()
                        .map(|mut p| {
                            p.latest = old
                                .iter()
                                .find(|o| o.name == p.name && o.version == p.version)
                                .and_then(|o| o.latest.clone());
                            p
                        })
                        .collect();
                    s.python.packages_for = Some(s.python.python_program());
                    s.python.packages_error = None;
                    if then_outdated {
                        let _ = start_package_op(s, PkgOp::List { outdated: true, then_outdated: false });
                    }
                }
                Err(e) if !outdated => s.python.packages_error = Some(e),
                Err(_) => {}
            }
        }
        PkgOp::CreateVenv { dir, select: sel } => {
            if success {
                s.python.log(format!("Created {}", dir.display()));
                s.ws.notify(NotificationLevel::Info, "Virtualenv created", dir.display().to_string());
                if sel
                    && let Some(py) = python_env::venv_python(&dir)
                    && let Err(e) = select(s, Some(&py))
                {
                    s.status = e;
                }
                if !sel {
                    refresh_interpreter(s);
                }
                s.python.detected = false;
                start_detect(s);
                s.jobs_rescan();
            } else {
                let msg = why();
                s.python.log(format!("Failed: {msg}"));
                s.ws.notify(NotificationLevel::Error, "Cannot create the virtualenv", msg);
            }
        }
        _ => {
            if success {
                s.status = format!("{title}: done");
                s.python.log(format!("{title}: done"));
            } else {
                let msg = why();
                s.status = format!("{title} failed: {msg}");
                s.python.log(format!("{title} failed"));
                s.ws.notify(NotificationLevel::Error, format!("{title} failed"), msg);
            }
            let _ = start_package_op(s, PkgOp::List { outdated: false, then_outdated: false });
        }
    }
}

// ---------------------------------------------------------------- missing modules

/// Import names whose package has another name on PyPI.
const IMPORT_TO_PACKAGE: &[(&str, &str)] = &[
    ("yaml", "PyYAML"),
    ("cv2", "opencv-python"),
    ("PIL", "Pillow"),
    ("sklearn", "scikit-learn"),
    ("skimage", "scikit-image"),
    ("bs4", "beautifulsoup4"),
    ("dateutil", "python-dateutil"),
    ("dotenv", "python-dotenv"),
    ("jwt", "PyJWT"),
    ("serial", "pyserial"),
    ("Crypto", "pycryptodome"),
    ("OpenSSL", "pyOpenSSL"),
    ("google.protobuf", "protobuf"),
    ("attr", "attrs"),
    ("magic", "python-magic"),
    ("docx", "python-docx"),
    ("pptx", "python-pptx"),
    ("fitz", "PyMuPDF"),
    ("MySQLdb", "mysqlclient"),
    ("psycopg2", "psycopg2-binary"),
    ("gi", "PyGObject"),
    ("win32api", "pywin32"),
    ("zmq", "pyzmq"),
    ("usb", "pyusb"),
    ("Levenshtein", "python-Levenshtein"),
];

/// The PyPI package that provides an import (`yaml` → `PyYAML`, `requests.adapters` → `requests`).
pub fn package_for_module(module: &str) -> String {
    let module = module.trim();
    if let Some((_, p)) = IMPORT_TO_PACKAGE.iter().find(|(m, _)| module == *m || module.starts_with(&format!("{m}."))) {
        return (*p).to_string();
    }
    module.split('.').next().unwrap_or(module).to_string()
}

/// The module of `ModuleNotFoundError: No module named 'x'` / `ImportError: No module named x`.
pub fn missing_module(line: &str) -> Option<String> {
    let rest = line.split("No module named").nth(1)?.trim();
    let rest = rest.trim_start_matches(['\'', '"']);
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.').collect();
    (!name.is_empty()).then_some(name)
}

/// The module of an unresolved-import diagnostic: pyright/basedpyright's `Import "x" could not
/// be resolved`, mypy's `Cannot find implementation or library stub for module named "x"`,
/// pylint's `Unable to import 'x'`, or a `No module named` message.
pub fn unresolved_import(message: &str) -> Option<String> {
    if message.contains("from source") || message.contains("stub file not found") {
        // The package is installed; only its stubs are missing.
        return None;
    }
    let quoted = |m: &str| -> Option<String> {
        let start = m.find(['"', '\''])?;
        let q = m.get(start..start + 1)?;
        let rest = m.get(start + 1..)?;
        let end = rest.find(q)?;
        rest.get(..end).map(str::to_owned).filter(|s| !s.is_empty())
    };
    if message.starts_with("Import ") && message.contains("could not be resolved") {
        return quoted(message);
    }
    if message.contains("library stub for module named") || message.starts_with("Unable to import") {
        return quoted(message);
    }
    missing_module(message)
}

/// Modules reported missing by the most recent runs (`ModuleNotFoundError`), newest first.
pub fn missing_modules_in_runs(s: &Session) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tab in s.ws.runs.iter().rev().take(5) {
        for l in tab.lines.iter().rev().take(400) {
            if (l.text.contains("ModuleNotFoundError") || l.text.contains("ImportError"))
                && let Some(m) = missing_module(&l.text)
                && !out.contains(&m)
            {
                out.push(m);
            }
        }
    }
    out
}

/// "Install package X" context actions for the unresolved imports at the caret of a Python file
/// (shown next to the language server's actions).
pub(crate) fn install_actions(s: &Session) -> Vec<crate::popup::ActionItem> {
    let Some((e, d)) = s.ws.active() else { return Vec::new() };
    if d.language != codecraft_syntax::Language::Python {
        return Vec::new();
    }
    let Some(path) = d.path.clone() else { return Vec::new() };
    let caret = d.doc.offset_to_position(e.selections.primary().head);
    let mut modules: Vec<String> =
        s.ws.problems_for(&path)
            .into_iter()
            .filter(|p| p.start.line <= caret.line && caret.line <= p.end.line)
            .filter_map(|p| unresolved_import(&p.message))
            .collect();
    modules.dedup();
    modules
        .into_iter()
        .map(|m| {
            let package = package_for_module(&m);
            crate::popup::ActionItem {
                title: format!("Install package {package}"),
                kind: Some("quickfix".into()),
                payload: serde_json::json!({
                    "title": format!("Install package {package}"),
                    "codecraftCommand": "python.packages.install",
                    "codecraftParams": { "name": package },
                }),
            }
        })
        .collect()
}

/// Whether the project or the active file is Python (status bar widget, menus).
pub fn python_context(s: &Session) -> bool {
    s.ws.active().is_some_and(|(_, d)| d.language == codecraft_syntax::Language::Python) || s.python.python_project
}

/// Show a tool window.
pub(crate) fn show(s: &mut Session, tw: ToolWindowId) {
    s.ws.layout.show(tw);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pip_lists() {
        let list = parse_pip_list(
            r#"[{"name": "requests", "version": "2.31.0"}, {"name": "Babel", "version": "2.1"}, {"version": "1"}]"#,
        )
        .unwrap();
        assert_eq!(list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["Babel", "requests"]);
        let out = parse_pip_list(
            "noise\n[{\"name\": \"pip\", \"version\": \"23.0\", \"latest_version\": \"24.2\", \"latest_filetype\": \"wheel\"}]",
        )
        .unwrap();
        assert_eq!(out[0].latest.as_deref(), Some("24.2"));
        assert!(parse_pip_list("").is_err());
        assert!(parse_pip_list("[{").is_err());
        assert!(parse_pip_list("{\"a\": [1]}").is_err());
        assert_eq!(normalize_name("Foo__Bar.baz"), "foo-bar-baz");
    }

    #[test]
    fn requirements_are_validated() {
        assert_eq!(requirement("requests", None).unwrap(), "requests");
        assert_eq!(requirement("requests", Some("2.31.0")).unwrap(), "requests==2.31.0");
        assert_eq!(requirement(" uvicorn[standard] ", Some(">= 0.20, <1")).unwrap(), "uvicorn[standard]>=0.20,<1");
        assert_eq!(requirement("a", Some("  ")).unwrap(), "a");
        for bad in ["--index-url", "-e", "", "a b", "x;rm", "../x", "pkg @ http://x"] {
            assert!(requirement(bad, None).is_err(), "{bad}");
        }
        assert!(requirement("ok", Some("--pre")).is_err());
        assert!(requirement("ok", Some("1 2")).is_err());
    }

    #[test]
    fn pip_and_uv_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join(".venv");
        std::fs::create_dir_all(venv.join("bin")).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "version = 3.12.0\n").unwrap();
        let py = venv.join("bin/python");
        let uv = Path::new("/bin/uv");
        let (p, a) = pip_command(&py, Some(uv), &PkgOp::List { outdated: true, then_outdated: false }).unwrap();
        assert_eq!(p, uv);
        assert_eq!(a, ["pip", "list", "--format", "json", "--outdated", "--python", py.to_str().unwrap()]);
        let (_, a) = pip_command(&py, Some(uv), &PkgOp::Uninstall { name: "x".into() }).unwrap();
        assert_eq!(a, ["pip", "uninstall", "--python", py.to_str().unwrap(), "x"]);
        // A system interpreter (no virtualenv) uses pip even with uv installed.
        let sys = Path::new("/usr/bin/python3");
        let (p, a) = pip_command(sys, Some(uv), &PkgOp::Uninstall { name: "x".into() }).unwrap();
        assert_eq!(p, sys);
        assert_eq!(a, ["-m", "pip", "uninstall", "-y", "x", "--disable-pip-version-check"]);
        let (_, a) = pip_command(sys, None, &PkgOp::Upgrade { name: "x".into() }).unwrap();
        assert_eq!(a, ["-m", "pip", "install", "--upgrade", "x", "--disable-pip-version-check"]);
        assert!(pip_command(sys, None, &PkgOp::CreateVenv { dir: "/x".into(), select: false }).is_none());
    }

    #[test]
    fn missing_modules_and_unresolved_imports() {
        assert_eq!(missing_module("ModuleNotFoundError: No module named 'yaml'").as_deref(), Some("yaml"));
        assert_eq!(missing_module("ImportError: No module named foo.bar").as_deref(), Some("foo.bar"));
        assert_eq!(missing_module("nothing here"), None);
        assert_eq!(package_for_module("yaml"), "PyYAML");
        assert_eq!(package_for_module("google.protobuf.message"), "protobuf");
        assert_eq!(package_for_module("requests.adapters"), "requests");
        assert_eq!(unresolved_import("Import \"numpy\" could not be resolved").as_deref(), Some("numpy"));
        assert_eq!(unresolved_import("Import \"numpy\" could not be resolved from source"), None);
        assert_eq!(
            unresolved_import("Cannot find implementation or library stub for module named \"attr\"").as_deref(),
            Some("attr")
        );
        assert_eq!(unresolved_import("Unable to import 'flask'").as_deref(), Some("flask"));
        assert_eq!(unresolved_import("\"x\" is not defined"), None);
        assert_eq!(unresolved_import("Import \""), None);
    }

    #[test]
    fn install_actions_for_unresolved_imports_at_the_caret() {
        use codecraft_text::Position;
        use codecraft_workspace::{Problem, ProblemSource, Severity};
        let mut s = Session::new();
        let path = PathBuf::from("/p/m.py");
        let d = s.ws.add_document(Some(path.clone()), "import yaml\nx = 1\n");
        s.ws.open_editor(d);
        let problem = |line: usize, message: &str| Problem {
            path: path.clone(),
            start: Position::new(line, 7),
            end: Position::new(line, 11),
            severity: Severity::Error,
            message: message.into(),
            code: None,
            source: ProblemSource::RustAnalyzer,
        };
        s.ws.set_lsp_problems(
            path.clone(),
            vec![problem(0, "Import \"yaml\" could not be resolved"), problem(1, "Import \"x\" could not be resolved")],
        );
        let items = install_actions(&s);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Install package PyYAML");
        assert_eq!(items[0].payload["codecraftCommand"], "python.packages.install");
        assert_eq!(items[0].payload["codecraftParams"]["name"], "PyYAML");
        // Not offered in other languages.
        let rs = s.ws.add_document(Some(PathBuf::from("/p/a.rs")), "fn main() {}\n");
        s.ws.open_editor(rs);
        assert!(install_actions(&s).is_empty());
    }
}
