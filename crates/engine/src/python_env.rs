//! Python interpreters: the project's selection (persisted in `<project>/.codecraft/python.json`),
//! virtualenv lookup, and detection of the interpreters on the machine (project virtualenvs, uv,
//! poetry, conda, pyenv and system Pythons).
//!
//! Detection runs external programs (`uv python list`, `poetry env info -p`, `conda env list`,
//! `pyenv root`, `python --version`), so it only runs on worker threads, each program with a
//! timeout. [`project_python`] only reads files and is cheap enough for any thread: run
//! configurations, debugpy, pytest and the Python language server all go through it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where the project's interpreter selection lives, relative to the project root.
pub const SELECTION_FILE: &str = ".codecraft/python.json";
/// Directories checked for a project virtualenv, in order.
pub const VENV_DIRS: [&str; 4] = [".venv", "venv", ".env", "env"];
/// Interpreters kept per detection (a PATH full of shims cannot flood the popup).
const MAX_INTERPRETERS: usize = 64;
/// Output kept from a probe program.
const MAX_PROBE_OUTPUT: u64 = 4 << 20;
/// Selection files larger than this are ignored.
const MAX_SELECTION_BYTES: u64 = 64 * 1024;

/// Where an interpreter comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterpreterKind {
    /// A virtualenv inside the project (`.venv`, `venv`, …).
    Venv,
    /// A Python installed by uv (`uv python install`).
    Uv,
    /// A poetry environment.
    Poetry,
    /// A conda environment.
    Conda,
    /// A pyenv version.
    Pyenv,
    /// A Python on `PATH` or in a well-known location.
    System,
}

impl InterpreterKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Venv => "Virtualenv",
            Self::Uv => "uv",
            Self::Poetry => "Poetry",
            Self::Conda => "Conda",
            Self::Pyenv => "pyenv",
            Self::System => "System",
        }
    }
}

/// A detected interpreter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interpreter {
    /// The `python` executable.
    pub path: PathBuf,
    /// `3.12.1` when known.
    pub version: Option<String>,
    pub kind: InterpreterKind,
    /// A short name: the virtualenv or environment directory, else the executable's name.
    pub name: String,
}

impl Interpreter {
    /// `Python 3.12 (.venv)`.
    pub fn label(&self) -> String {
        let version = self.version.as_deref().map(short_version).unwrap_or_default();
        let head = if version.is_empty() { "Python".to_string() } else { format!("Python {version}") };
        format!("{head} ({})", self.name)
    }
}

/// `3.12.1` → `3.12`.
pub fn short_version(v: &str) -> String {
    v.split('.').take(2).collect::<Vec<_>>().join(".")
}

// ---------------------------------------------------------------- virtualenvs and selection

/// The interpreter of a virtualenv directory (`bin/python`, `Scripts/python.exe`).
pub fn venv_python(dir: &Path) -> Option<PathBuf> {
    ["bin/python", "bin/python3", "Scripts/python.exe"].iter().map(|rel| dir.join(rel)).find(|p| p.is_file())
}

/// The project's own virtualenv interpreter (`.venv`, `venv`, `.env`, `env`).
pub fn project_venv(root: &Path) -> Option<PathBuf> {
    VENV_DIRS.iter().map(|d| root.join(d)).filter(|d| is_venv_dir(d) || d.join("bin/python").is_file()).find_map(|d| {
        // `.env` and `env` are often dotenv files or unrelated folders: they need a pyvenv.cfg.
        let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(name, ".env" | "env") && !is_venv_dir(&d) {
            return None;
        }
        venv_python(&d)
    })
}

/// A directory with a `pyvenv.cfg` (made by `venv`, virtualenv or uv).
pub fn is_venv_dir(dir: &Path) -> bool {
    dir.join("pyvenv.cfg").is_file()
}

/// The virtualenv an interpreter belongs to (`<venv>/bin/python` → `<venv>`).
pub fn venv_of(python: &Path) -> Option<PathBuf> {
    let dir = python.parent()?.parent()?;
    is_venv_dir(dir).then(|| dir.to_path_buf())
}

/// The Python version recorded in a virtualenv's `pyvenv.cfg` (`version = 3.12.1`,
/// `version_info = 3.12.1.final.0`).
pub fn pyvenv_version(dir: &Path) -> Option<String> {
    let text = read_capped(&dir.join("pyvenv.cfg"), MAX_SELECTION_BYTES)?;
    parse_pyvenv_version(&text)
}

pub fn parse_pyvenv_version(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        let k = k.trim();
        if k != "version" && k != "version_info" {
            return None;
        }
        let v: Vec<&str> =
            v.trim().split('.').take_while(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty()).collect();
        (v.len() >= 2).then(|| v.join("."))
    })
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SelectionFile {
    interpreter: Option<PathBuf>,
}

/// The interpreter chosen for the project, when one was chosen and still exists.
pub fn selected_interpreter(root: &Path) -> Option<PathBuf> {
    let text = read_capped(&root.join(SELECTION_FILE), MAX_SELECTION_BYTES)?;
    let file: SelectionFile = serde_json::from_str(&text).ok()?;
    let p = file.interpreter?;
    let p = if p.is_absolute() { p } else { root.join(p) };
    p.is_file().then_some(p)
}

/// Persist (or with `None` clear) the project's interpreter. Paths inside the project are stored
/// relative so the project can move.
pub fn save_selection(root: &Path, interpreter: Option<&Path>) -> Result<(), String> {
    let file = root.join(SELECTION_FILE);
    let Some(p) = interpreter else {
        return match std::fs::remove_file(&file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", file.display())),
        };
    };
    let stored = p.strip_prefix(root).map(Path::to_path_buf).unwrap_or_else(|_| p.to_path_buf());
    let json = serde_json::to_string_pretty(&SelectionFile { interpreter: Some(stored) }).map_err(|e| e.to_string())?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&file, json + "\n").map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// The interpreter the project uses: the selection, else its virtualenv. `None` = `python3` from
/// `PATH`.
pub fn project_python(root: &Path) -> Option<PathBuf> {
    selected_interpreter(root).or_else(|| project_venv(root))
}

/// A tool installed next to the project's interpreter (`<venv>/bin/mypy`), else on `PATH`.
pub fn python_tool(root: &Path, name: &str) -> Option<PathBuf> {
    project_python(root)
        .and_then(|py| py.parent().map(|d| d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))))
        .filter(|p| p.is_file())
        .or_else(|| codecraft_lsp::which(name))
}

/// Whether the directory looks like a Python project.
pub fn is_python_project(root: &Path) -> bool {
    ["pyproject.toml", "requirements.txt", "setup.py", "setup.cfg", "Pipfile", "manage.py"]
        .iter()
        .any(|f| root.join(f).is_file())
        || project_venv(root).is_some()
}

fn read_capped(path: &Path, max: u64) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    f.take(max).read_to_string(&mut text).ok()?;
    Some(text)
}

// ---------------------------------------------------------------- detection

/// Where detection looks (tests point it at fake directories and scripts).
#[derive(Clone, Debug)]
pub struct Probe {
    /// Directories searched for `python3`, `uv`, `poetry`, `conda`, `pyenv`.
    pub dirs: Vec<PathBuf>,
    /// The home directory (`~/.pyenv`, `~/miniconda3`, …); `None` skips them.
    pub home: Option<PathBuf>,
    /// Also look in `/usr/bin`, `/opt/homebrew/bin`, … (off in tests).
    pub well_known: bool,
    /// Time allowed for each probe program.
    pub timeout: Duration,
}

impl Probe {
    /// `PATH` plus the usual user bin directories.
    pub fn from_env() -> Self {
        let mut dirs: Vec<PathBuf> =
            std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        for d in codecraft_lsp::extra_bin_dirs() {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        Self {
            dirs,
            home: std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from),
            well_known: true,
            timeout: Duration::from_secs(10),
        }
    }

    /// Only `dirs` (no home, no well-known locations): for tests.
    pub fn isolated(dirs: Vec<PathBuf>) -> Self {
        Self { dirs, home: None, well_known: false, timeout: Duration::from_secs(10) }
    }

    /// An executable named `name` in the probe's directories.
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        self.dirs.iter().map(|d| d.join(&exe)).find(|p| is_executable(p))
    }
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

/// Run a probe program: its stdout when it exits successfully within `timeout`. Killed on
/// timeout; never blocks longer.
pub fn run_capture(program: &Path, args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    // The reader reports through a channel: a grandchild keeping stdout open cannot hold us.
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.take(MAX_PROBE_OUTPUT).read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(500));
                return rx.recv_timeout(left).ok();
            }
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `Python 3.12.1` → `3.12.1`.
pub fn parse_version_output(text: &str) -> Option<String> {
    let v = text.trim().strip_prefix("Python")?.trim();
    let v: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    (v.contains('.')).then_some(v)
}

/// The paths and versions of `uv python list --only-installed --output-format json`.
pub fn parse_uv_python_list(json: &str) -> Vec<(PathBuf, Option<String>)> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    items
        .iter()
        .filter_map(|i| {
            let path = i.get("path").and_then(Value::as_str).filter(|p| !p.is_empty())?;
            let version = i.get("version").and_then(Value::as_str).map(str::to_owned);
            Some((PathBuf::from(path), version))
        })
        .collect()
}

/// The environment directories of `conda env list --json` (`{"envs": [...]}`).
pub fn parse_conda_envs(json: &str) -> Vec<PathBuf> {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v.get("envs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Names a system interpreter can have on `PATH` (`python3`, `python3.12`, `python`).
fn is_python_name(name: &str) -> bool {
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name == "python" || name == "python3" {
        return true;
    }
    name.strip_prefix("python3.").is_some_and(|minor| !minor.is_empty() && minor.chars().all(|c| c.is_ascii_digit()))
}

struct Found {
    list: Vec<Interpreter>,
    canonical: Vec<PathBuf>,
}

impl Found {
    fn add(&mut self, path: PathBuf, kind: InterpreterKind, name: String, version: Option<String>) {
        if self.list.len() >= MAX_INTERPRETERS || !path.is_file() || self.list.iter().any(|i| i.path == path) {
            return;
        }
        // System and uv Pythons are often symlinks to the same binary (`python3` → `python3.12`);
        // virtualenvs link to their base interpreter but are distinct environments.
        if !matches!(kind, InterpreterKind::Venv | InterpreterKind::Poetry | InterpreterKind::Conda) {
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if self.canonical.contains(&canonical) {
                return;
            }
            self.canonical.push(canonical);
        }
        self.list.push(Interpreter { path, version, kind, name });
    }
}

fn dir_name(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string())
}

/// Every interpreter found for the project (project virtualenvs first, then poetry, uv, conda,
/// pyenv and system Pythons). Runs programs: call from a worker thread.
pub fn detect(root: Option<&Path>, probe: &Probe) -> Vec<Interpreter> {
    let mut found = Found { list: Vec::new(), canonical: Vec::new() };
    // Project virtualenvs.
    if let Some(root) = root {
        for d in VENV_DIRS {
            let dir = root.join(d);
            if !is_venv_dir(&dir) && !(matches!(d, ".venv" | "venv") && dir.join("bin/python").is_file()) {
                continue;
            }
            if let Some(py) = venv_python(&dir) {
                found.add(py, InterpreterKind::Venv, d.to_string(), pyvenv_version(&dir));
            }
        }
        if let Some(sel) = selected_interpreter(root) {
            let venv = venv_of(&sel);
            let name = venv.as_deref().map(dir_name).unwrap_or_else(|| dir_name(&sel));
            let version = venv.as_deref().and_then(pyvenv_version);
            let kind = if venv.is_some() { InterpreterKind::Venv } else { InterpreterKind::System };
            found.add(sel, kind, name, version);
        }
    }
    // Poetry: only for poetry projects (it is slow and would create an environment elsewhere).
    if let Some(root) = root
        && std::fs::read_to_string(root.join("pyproject.toml")).is_ok_and(|t| t.contains("[tool.poetry"))
        && let Some(poetry) = probe.which("poetry")
        && let Some(out) = run_capture(&poetry, &["env", "info", "-p"], Some(root), probe.timeout)
    {
        let dir = PathBuf::from(out.trim());
        if let Some(py) = venv_python(&dir) {
            let version = pyvenv_version(&dir);
            found.add(py, InterpreterKind::Poetry, dir_name(&dir), version);
        }
    }
    // uv-managed Pythons (and the system ones uv sees).
    if let Some(uv) = probe.which("uv")
        && let Some(out) =
            run_capture(&uv, &["python", "list", "--only-installed", "--output-format", "json"], None, probe.timeout)
    {
        for (path, version) in parse_uv_python_list(&out) {
            let s = path.to_string_lossy();
            let kind = if s.contains("/uv/python/") || s.contains("\\uv\\python\\") {
                InterpreterKind::Uv
            } else {
                InterpreterKind::System
            };
            let name = dir_name(&path);
            found.add(path, kind, name, version);
        }
    }
    // Conda environments.
    let conda = probe.which("conda").or_else(|| {
        let home = probe.home.as_ref()?;
        ["miniconda3/bin/conda", "anaconda3/bin/conda", "miniforge3/bin/conda", "mambaforge/bin/conda"]
            .iter()
            .map(|r| home.join(r))
            .find(|p| is_executable(p))
    });
    if let Some(conda) = conda
        && let Some(out) = run_capture(&conda, &["env", "list", "--json"], None, probe.timeout)
    {
        for env in parse_conda_envs(&out) {
            if let Some(py) = venv_python(&env).or_else(|| Some(env.join("python.exe")).filter(|p| p.is_file())) {
                found.add(py, InterpreterKind::Conda, dir_name(&env), None);
            }
        }
    }
    // pyenv versions.
    let pyenv_root = probe
        .which("pyenv")
        .and_then(|pyenv| run_capture(&pyenv, &["root"], None, probe.timeout))
        .map(|o| PathBuf::from(o.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| probe.home.as_ref().map(|h| h.join(".pyenv")));
    if let Some(versions) = pyenv_root.map(|r| r.join("versions"))
        && let Ok(rd) = std::fs::read_dir(&versions)
    {
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs.into_iter().take(MAX_INTERPRETERS) {
            if let Some(py) = venv_python(&dir) {
                let name = dir_name(&dir);
                let version = name.chars().next().filter(char::is_ascii_digit).map(|_| name.clone());
                found.add(py, InterpreterKind::Pyenv, name, version);
            }
        }
    }
    // System Pythons on PATH and in well-known places.
    let mut dirs = probe.dirs.clone();
    if probe.well_known {
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            let d = PathBuf::from(d);
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    for dir in dirs {
        // pyenv shims are not interpreters.
        if dir.components().any(|c| c.as_os_str() == "shims") {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| is_python_name(n))
            .collect();
        names.sort();
        for n in names {
            let path = dir.join(&n);
            if !is_executable(&path) || found.list.iter().any(|i| i.path == path) {
                continue;
            }
            let version =
                run_capture(&path, &["--version"], None, probe.timeout).as_deref().and_then(parse_version_output);
            // Something named python that does not answer `--version` is not usable.
            if version.is_none() {
                continue;
            }
            found.add(path, InterpreterKind::System, n, version);
        }
    }
    found.list
}

/// The command that creates a virtualenv at `dir`: `uv venv` when uv is installed, else
/// `<base> -m venv` (`base` defaults to `python3`).
pub fn venv_command(dir: &Path, base: Option<&Path>, uv: Option<&Path>) -> (PathBuf, Vec<String>) {
    let dir = dir.display().to_string();
    match uv {
        Some(uv) => {
            let mut args = vec!["venv".to_string(), dir];
            if let Some(b) = base {
                args.extend(["--python".to_string(), b.display().to_string()]);
            }
            (uv.to_path_buf(), args)
        }
        None => {
            let base = base.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("python3"));
            (base, vec!["-m".into(), "venv".into(), dir])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn venv(dir: &Path, version: &str) -> PathBuf {
        script(&dir.join("bin/python"), &format!("echo Python {version}"));
        std::fs::write(dir.join("pyvenv.cfg"), format!("home = /usr/bin\nversion = {version}\n")).unwrap();
        dir.join("bin/python")
    }

    #[test]
    fn parses_versions_and_tool_output() {
        assert_eq!(parse_version_output("Python 3.12.1\n").as_deref(), Some("3.12.1"));
        assert_eq!(parse_version_output("Python 3.13.0rc1").as_deref(), Some("3.13.0"));
        assert_eq!(parse_version_output("nope"), None);
        assert_eq!(parse_pyvenv_version("home = /x\nversion_info = 3.11.4.final.0\n").as_deref(), Some("3.11.4"));
        assert_eq!(parse_pyvenv_version("version = 3.12.1\n").as_deref(), Some("3.12.1"));
        assert_eq!(parse_pyvenv_version("garbage"), None);
        let uv = r#"[{"path":"/u/python3.12","version":"3.12.9"},{"version":"x"},{"path":""}]"#;
        assert_eq!(parse_uv_python_list(uv), vec![(PathBuf::from("/u/python3.12"), Some("3.12.9".into()))]);
        assert!(parse_uv_python_list("{not json").is_empty());
        assert_eq!(
            parse_conda_envs(r#"{"envs": ["/c", "/c/envs/ml", 3]}"#),
            vec![PathBuf::from("/c"), PathBuf::from("/c/envs/ml")]
        );
        assert!(parse_conda_envs("[]").is_empty());
        assert_eq!(short_version("3.12.1"), "3.12");
        assert!(is_python_name("python3.12") && is_python_name("python") && !is_python_name("python3-config"));
        assert!(!is_python_name("python3.") && !is_python_name("pythonw"));
    }

    #[test]
    fn selection_overrides_the_virtualenv_and_survives_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_eq!(project_python(root), None);
        let py = venv(&root.join(".venv"), "3.12.1");
        assert_eq!(project_python(root), Some(py.clone()));
        let other = venv(&root.join("envs/other"), "3.11.2");
        save_selection(root, Some(&other)).unwrap();
        // Stored relative to the project.
        let saved = std::fs::read_to_string(root.join(SELECTION_FILE)).unwrap();
        assert!(saved.contains("envs/other/bin/python"), "{saved}");
        assert_eq!(project_python(root), Some(other.clone()));
        // A selection that no longer exists falls back to the virtualenv.
        std::fs::remove_file(&other).unwrap();
        assert_eq!(project_python(root), Some(py.clone()));
        save_selection(root, None).unwrap();
        save_selection(root, None).unwrap();
        assert!(!root.join(SELECTION_FILE).exists());
        // A corrupt selection file is ignored.
        std::fs::write(root.join(SELECTION_FILE), "{{{").unwrap();
        assert_eq!(project_python(root), Some(py));
    }

    #[test]
    fn dot_env_needs_a_pyvenv_cfg() {
        let tmp = tempfile::tempdir().unwrap();
        script(&tmp.path().join(".env/bin/python"), "true");
        assert_eq!(project_venv(tmp.path()), None);
        std::fs::write(tmp.path().join(".env/pyvenv.cfg"), "version = 3.10.0\n").unwrap();
        assert_eq!(project_venv(tmp.path()), Some(tmp.path().join(".env/bin/python")));
        assert!(is_python_project(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn detects_venvs_uv_poetry_conda_pyenv_and_system_pythons() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[tool.poetry]\nname = \"x\"\n").unwrap();
        let project_py = venv(&root.join(".venv"), "3.12.1");

        let bin = tmp.path().join("bin");
        // Poetry's environment.
        let poetry_env = tmp.path().join("cache/pypoetry/virtualenvs/x-py3.11");
        let poetry_py = venv(&poetry_env, "3.11.4");
        script(&bin.join("poetry"), &format!("[ \"$1 $2 $3\" = 'env info -p' ] && echo {}", poetry_env.display()));
        // uv-managed Python.
        let uv_py = tmp.path().join("share/uv/python/cpython-3.13/bin/python3.13");
        script(&uv_py, "echo Python 3.13.0");
        script(&bin.join("uv"), &format!("echo '[{{\"path\":\"{}\",\"version\":\"3.13.0\"}}]'", uv_py.display()));
        // Conda.
        let conda_env = tmp.path().join("conda/envs/ml");
        script(&conda_env.join("bin/python"), "echo Python 3.9.1");
        script(&bin.join("conda"), &format!("echo '{{\"envs\": [\"{}\"]}}'", conda_env.display()));
        // pyenv.
        let pyenv_root = tmp.path().join("pyenv");
        script(&pyenv_root.join("versions/3.8.18/bin/python"), "echo Python 3.8.18");
        script(&bin.join("pyenv"), &format!("echo {}", pyenv_root.display()));
        // System pythons; the symlink is a duplicate; a broken one is skipped.
        script(&bin.join("python3.10"), "echo Python 3.10.11");
        std::os::unix::fs::symlink(bin.join("python3.10"), bin.join("python3")).unwrap();
        script(&bin.join("python"), "exit 1");

        let found = detect(Some(&root), &Probe::isolated(vec![bin.clone()]));
        let kinds: Vec<(InterpreterKind, &Path)> = found.iter().map(|i| (i.kind, i.path.as_path())).collect();
        assert_eq!(
            kinds,
            vec![
                (InterpreterKind::Venv, project_py.as_path()),
                (InterpreterKind::Poetry, poetry_py.as_path()),
                (InterpreterKind::Uv, uv_py.as_path()),
                (InterpreterKind::Conda, conda_env.join("bin/python").as_path()),
                (InterpreterKind::Pyenv, pyenv_root.join("versions/3.8.18/bin/python").as_path()),
                (InterpreterKind::System, bin.join("python3").as_path()),
            ],
            "{found:#?}"
        );
        assert_eq!(found[0].label(), "Python 3.12 (.venv)");
        assert_eq!(found[1].version.as_deref(), Some("3.11.4"));
        assert_eq!(found[4].version.as_deref(), Some("3.8.18"));
        assert_eq!(found[5].version.as_deref(), Some("3.10.11"));

        // Without the tools only the project and PATH remain.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        script(&plain.join("python3"), "echo Python 3.10.11");
        let found = detect(Some(&root), &Probe::isolated(vec![plain.clone()]));
        assert_eq!(found.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn probes_time_out_and_failures_are_none() {
        let tmp = tempfile::tempdir().unwrap();
        let slow = tmp.path().join("slow");
        script(&slow, "sleep 5");
        let start = Instant::now();
        assert_eq!(run_capture(&slow, &[], None, Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(4));
        let fail = tmp.path().join("fail");
        script(&fail, "echo out; exit 3");
        assert_eq!(run_capture(&fail, &[], None, Duration::from_secs(5)), None);
        assert_eq!(run_capture(&tmp.path().join("missing"), &[], None, Duration::from_secs(1)), None);
    }

    #[test]
    fn venv_command_prefers_uv() {
        let (p, a) = venv_command(Path::new("/p/.venv"), None, Some(Path::new("/bin/uv")));
        assert_eq!((p, a), (PathBuf::from("/bin/uv"), vec!["venv".to_string(), "/p/.venv".into()]));
        let (p, a) = venv_command(Path::new("/p/.venv"), Some(Path::new("/py")), None);
        assert_eq!((p, a), (PathBuf::from("/py"), vec!["-m".to_string(), "venv".into(), "/p/.venv".into()]));
    }
}
