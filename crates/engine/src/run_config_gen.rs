//! Generated run configurations: Run › Edit Configurations › Generate (and once, when a project
//! without saved configurations opens) proposes editable configurations for the project's main
//! entry points, per ecosystem and per sub-project (`backend/`, `frontend/`, …):
//!
//! - **Rust**: one `cargo run` per binary target of the workspace, and the tests.
//! - **Python**: Django (`manage.py`), FastAPI and Flask applications, Streamlit apps,
//!   `if __name__ == "__main__"` scripts with a conventional entry-point name (`main.py`,
//!   `app.py`, …), packages with a `__main__.py`, `[project.scripts]`, and pytest. Ordinary
//!   modules never get a configuration of their own.
//! - **C/C++**: CMake executables (and CTest), Meson executables, Make (`make`, `make run`,
//!   `make test`), or, without a build system, the sources that define `main()`.
//! - **Zig**: the `build.zig` steps, or `zig run` of the `main.zig` that defines `pub fn main`.
//! - **JavaScript/TypeScript** (`dev`, `start`, `test`, `build` scripts), **Go** (`go run` of
//!   `package main`), **Docker** (compose file, Dockerfile).
//!
//! Detection reads a bounded number of directories and file heads and runs on a worker thread.
//! An agent can refine the result (`runConfig.generate` with `agent`): it gets the detected list
//! and the project, and replies with JSON that [`parse_agent_reply`] turns into proposals.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crossbeam_channel::{Receiver, TryRecvError, bounded};

use codecraft_agent::Backend;
use codecraft_cargo::CargoMetadata;
use codecraft_workspace::{BackgroundTask, NotificationLevel};

use crate::agents::{Activity, JobId, JobStatus};
use crate::approvals::AgentMode;
use crate::run_config_store::{RunKind, UserRunConfig, unique_name};
use crate::run_configs::CMAKE_BUILD_DIR;
use crate::{CommandError, Session};

/// Directory levels below the project root that are searched for sub-projects.
const MAX_DEPTH: usize = 3;
/// Directories read at most.
const MAX_DIRS: usize = 1500;
/// Entries read per directory at most.
const MAX_ENTRIES: usize = 2000;
/// Bytes read per inspected file.
const MAX_HEAD: u64 = 128 * 1024;
/// Proposals at most (per ecosystem limits are lower).
pub const MAX_PROPOSALS: usize = 60;

/// Directories never searched (dependencies, build output, environments).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "venv",
    "env",
    "__pycache__",
    "site-packages",
    "dist",
    "build",
    "out",
    "zig-out",
    "zig-cache",
    "vendor",
    "third_party",
    "external",
    "deps",
    "builddir",
    "bin",
    "obj",
    "coverage",
];

/// Python files whose `if __name__ == "__main__"` makes them the project's entry point.
const PY_ENTRY_FILES: &[&str] =
    &["main.py", "app.py", "run.py", "cli.py", "server.py", "manage.py", "__main__.py", "src/main.py", "src/app.py"];
/// Files that usually hold a FastAPI/Flask application or a Streamlit app.
const PY_APP_FILES: &[&str] = &[
    "main.py",
    "app.py",
    "server.py",
    "api.py",
    "wsgi.py",
    "asgi.py",
    "streamlit_app.py",
    "app/main.py",
    "app/app.py",
    "app/__init__.py",
    "src/main.py",
    "src/app.py",
];
const PY_MARKERS: &[&str] =
    &["pyproject.toml", "requirements.txt", "setup.py", "setup.cfg", "Pipfile", "manage.py", "uv.lock", "poetry.lock"];
const C_EXTS: &[&str] = &["c", "cc", "cpp", "cxx", "c++"];
const COMPOSE_FILES: &[&str] = &["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"];

/// Where a proposal comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Detected,
    Agent,
}

/// One proposed configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proposal {
    pub config: UserRunConfig,
    /// Why it is proposed ("binary `app` of package `app`").
    pub reason: String,
    /// The ecosystem ("Rust", "Python", "C/C++", "Zig", …).
    pub group: String,
    pub source: Source,
    /// A saved configuration already runs the same command (same kind, command and directory).
    pub exists: bool,
    /// Ticked in the Generate view (new ones are; existing ones are not).
    pub selected: bool,
}

impl Proposal {
    fn new(group: &str, name: String, kind: RunKind, command: impl Into<String>, dir: &str, reason: String) -> Self {
        Self {
            config: UserRunConfig {
                name,
                kind,
                command: command.into(),
                working_dir: dir.to_string(),
                ..Default::default()
            },
            reason,
            group: group.into(),
            source: Source::Detected,
            exists: false,
            selected: true,
        }
    }
}

/// The same configuration as far as running goes (names may differ).
pub fn same_run(a: &UserRunConfig, b: &UserRunConfig) -> bool {
    a.kind == b.kind
        && a.command.split_whitespace().eq(b.command.split_whitespace())
        && a.working_dir.trim().trim_end_matches('/') == b.working_dir.trim().trim_end_matches('/')
}

/// Mark proposals that `saved` already covers (and untick them), and make the names of the rest
/// unique against `saved` and each other.
pub fn reconcile(proposals: &mut [Proposal], saved: &[UserRunConfig]) {
    let mut taken: Vec<UserRunConfig> = saved.to_vec();
    for p in proposals.iter_mut() {
        if let Some(existing) = saved.iter().find(|c| same_run(c, &p.config)) {
            p.exists = true;
            p.selected = false;
            p.config.name = existing.name.clone();
            continue;
        }
        p.exists = false;
        p.config.name = unique_name(&p.config.name, &taken);
        taken.push(p.config.clone());
    }
}

// ---------------------------------------------------------------- the project's directories

/// A directory of the project with its file and subdirectory names.
struct Dir {
    /// Relative to the root, `/`-separated; empty for the root.
    rel: String,
    path: PathBuf,
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
}

impl Dir {
    fn has(&self, name: &str) -> bool {
        self.files.contains(name)
    }

    fn read(&self, name: &str) -> Option<String> {
        self.has(name).then(|| read_head(&self.path.join(name))).flatten()
    }
}

fn read_head(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    f.take(MAX_HEAD).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn skipped(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name) || name.starts_with("cmake-build-")
}

/// The project's directories, breadth first (the root first), skipping hidden directories,
/// dependencies and build output.
fn scan(root: &Path) -> Vec<Dir> {
    let mut out: Vec<Dir> = Vec::new();
    let mut queue: std::collections::VecDeque<(String, usize)> = std::collections::VecDeque::from([(String::new(), 0)]);
    while let Some((rel, depth)) = queue.pop_front() {
        if out.len() >= MAX_DIRS {
            break;
        }
        let path = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let Ok(entries) = std::fs::read_dir(&path) else { continue };
        let mut dir = Dir { rel: rel.clone(), path, files: BTreeSet::new(), dirs: BTreeSet::new() };
        for entry in entries.flatten().take(MAX_ENTRIES) {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Symlinked directories are not followed (no cycles, no escaping the project).
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    dir.dirs.insert(name);
                }
                Ok(t) if t.is_file() || t.is_symlink() => {
                    dir.files.insert(name);
                }
                _ => {}
            }
        }
        if depth < MAX_DEPTH {
            for d in dir.dirs.iter().filter(|d| !skipped(d)) {
                let child = if rel.is_empty() { d.clone() } else { format!("{rel}/{d}") };
                queue.push_back((child, depth + 1));
            }
        }
        out.push(dir);
    }
    out
}

/// `rel` is `dir` or below it.
fn under(rel: &str, dir: &str) -> bool {
    dir.is_empty() || rel == dir || rel.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// `name`, prefixed with the sub-project directory (`backend: name`) outside the root.
fn label(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}: {name}") }
}

/// The directories that `marker` makes the top of a sub-project (nested ones belong to it).
fn tops(dirs: &[Dir], marker: impl Fn(&Dir) -> bool) -> Vec<&Dir> {
    let mut out: Vec<&Dir> = Vec::new();
    for d in dirs {
        if marker(d) && !out.iter().any(|t| under(&d.rel, &t.rel)) {
            out.push(d);
        }
    }
    out
}

// ---------------------------------------------------------------- detection

/// Propose configurations for the project at `root`. `cargo` is the root workspace's metadata
/// when it loaded.
pub fn detect(root: &Path, cargo: Option<&CargoMetadata>) -> Vec<Proposal> {
    let dirs = scan(root);
    let mut out = Vec::new();
    rust(root, &dirs, cargo, &mut out);
    python(&dirs, &mut out);
    c_cpp(&dirs, &mut out);
    zig(&dirs, &mut out);
    javascript(&dirs, &mut out);
    go(&dirs, &mut out);
    docker(&dirs, &mut out);
    // Keep the first of configurations that would run the same thing.
    let mut unique: Vec<Proposal> = Vec::new();
    for p in out {
        if !unique.iter().any(|u| same_run(&u.config, &p.config)) {
            unique.push(p);
        }
        if unique.len() >= MAX_PROPOSALS {
            break;
        }
    }
    reconcile(&mut unique, &[]);
    unique
}

fn rust(root: &Path, dirs: &[Dir], cargo: Option<&CargoMetadata>, out: &mut Vec<Proposal>) {
    let mut covered: Vec<String> = Vec::new();
    if let Some(meta) = cargo {
        let dir = meta
            .workspace_root
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let packages: Vec<_> = meta.workspace_packages().collect();
        let multi = packages.len() > 1;
        let mut bins = 0;
        for p in &packages {
            for t in p.targets.iter().filter(|t| t.is_bin()) {
                if bins >= 15 {
                    break;
                }
                bins += 1;
                let command = if multi || p.targets.iter().filter(|t| t.is_bin()).count() > 1 {
                    format!("run --package {} --bin {}", p.name, t.name)
                } else {
                    "run".to_string()
                };
                out.push(Proposal::new(
                    "Rust",
                    label(&dir, &format!("Run {}", t.name)),
                    RunKind::Cargo,
                    command,
                    &dir,
                    format!("binary `{}` of package `{}`", t.name, p.name),
                ));
            }
        }
        let test = if multi { "test --workspace" } else { "test" };
        out.push(Proposal::new(
            "Rust",
            label(&dir, "Test workspace"),
            RunKind::Cargo,
            test,
            &dir,
            "the workspace's tests".into(),
        ));
        covered.push(dir);
    }
    // Cargo projects without metadata (sub-projects, or cargo metadata failed): read Cargo.toml.
    for d in tops(dirs, |d| d.has("Cargo.toml")) {
        if covered.iter().any(|c| under(&d.rel, c) || under(c, &d.rel)) {
            continue;
        }
        let Some(manifest) = d.read("Cargo.toml") else { continue };
        let package = toml_string(&manifest, "package", "name");
        let src = d.path.join("src");
        if let Some(name) = &package
            && src.join("main.rs").is_file()
        {
            out.push(Proposal::new(
                "Rust",
                label(&d.rel, &format!("Run {name}")),
                RunKind::Cargo,
                "run",
                &d.rel,
                format!("binary of package `{name}`"),
            ));
        }
        if let Ok(entries) = std::fs::read_dir(src.join("bin")) {
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.strip_suffix(".rs").map(str::to_string))
                .collect();
            names.sort();
            for bin in names.into_iter().take(10) {
                out.push(Proposal::new(
                    "Rust",
                    label(&d.rel, &format!("Run {bin}")),
                    RunKind::Cargo,
                    format!("run --bin {bin}"),
                    &d.rel,
                    format!("binary `src/bin/{bin}.rs`"),
                ));
            }
        }
        let workspace = section_lines(&manifest, "workspace").is_some();
        out.push(Proposal::new(
            "Rust",
            label(&d.rel, "Test workspace"),
            RunKind::Cargo,
            if workspace { "test --workspace" } else { "test" },
            &d.rel,
            "the crate's tests".into(),
        ));
    }
}

/// The lines of a TOML table (`[name]`), up to the next table header.
fn section_lines<'a>(text: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let header = format!("[{name}]");
    let mut lines = text.lines();
    lines.by_ref().find(|l| l.trim() == header)?;
    Some(lines.take_while(|l| !l.trim_start().starts_with('[')).collect())
}

/// `key = "value"` pairs of a TOML table (quoted values only).
fn toml_pairs(text: &str, table: &str) -> Vec<(String, String)> {
    let Some(lines) = section_lines(text, table) else { return Vec::new() };
    lines
        .into_iter()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let k = k.trim().trim_matches('"').to_string();
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|v| v.split_once('"')).map(|(v, _)| v.to_string())?;
            (!k.is_empty() && !k.starts_with('#')).then_some((k, v))
        })
        .collect()
}

fn toml_string(text: &str, table: &str, key: &str) -> Option<String> {
    toml_pairs(text, table).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn main_guard(text: &str) -> bool {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^if\s+__name__\s*==\s*['"]__main__['"]\s*:"#).ok())
        .as_ref()
        .is_some_and(|re| re.is_match(text))
}

/// `src/app/main.py` run from `dir`: the module (`app.main`) and the directory to run in.
fn py_module(dir: &str, rel: &str) -> (String, String) {
    let (run_dir, rel) = match rel.strip_prefix("src/") {
        Some(r) => (if dir.is_empty() { "src".to_string() } else { format!("{dir}/src") }, r),
        None => (dir.to_string(), rel),
    };
    let module = rel.trim_end_matches(".py").trim_end_matches("/__init__").replace('/', ".");
    (module, run_dir)
}

fn python(dirs: &[Dir], out: &mut Vec<Proposal>) {
    let roots = tops(dirs, |d| {
        PY_MARKERS.iter().any(|m| d.has(m))
            // A project without packaging files: only the root, and only with an entry script.
            || (d.rel.is_empty()
                && PY_ENTRY_FILES.iter().any(|f| d.read(f).is_some_and(|t| main_guard(&t))))
    });
    for d in roots.into_iter().take(6) {
        let read = |rel: &str| read_head(&d.path.join(rel));
        let mut used: BTreeSet<&str> = BTreeSet::new();
        let mut entries = 0;

        if let Some(text) = d.read("manage.py")
            && (text.contains("django") || text.contains("DJANGO_SETTINGS_MODULE"))
        {
            out.push(Proposal::new(
                "Python",
                label(&d.rel, "Django server"),
                RunKind::Django,
                "runserver",
                &d.rel,
                "Django project (manage.py)".into(),
            ));
            used.insert("manage.py");
            entries += 1;
        }

        let (mut fastapi, mut flask, mut streamlit) = (false, false, false);
        for &rel in PY_APP_FILES {
            let Some(text) = read(rel) else { continue };
            if !fastapi && let Some(var) = crate::python_project::app_variable(&text, "FastAPI") {
                let (module, run_dir) = py_module(&d.rel, rel);
                out.push(Proposal::new(
                    "Python",
                    label(&d.rel, &format!("FastAPI {module}:{var}")),
                    RunKind::FastApi,
                    format!("{module}:{var} --reload"),
                    &run_dir,
                    format!("FastAPI application `{var}` in {rel}"),
                ));
                fastapi = true;
                used.insert(rel);
            }
            if !flask && let Some(var) = crate::python_project::app_variable(&text, "Flask") {
                let (module, run_dir) = py_module(&d.rel, rel);
                out.push(Proposal::new(
                    "Python",
                    label(&d.rel, &format!("Flask {module}:{var}")),
                    RunKind::Flask,
                    format!("--app {module}:{var} run --debug"),
                    &run_dir,
                    format!("Flask application `{var}` in {rel}"),
                ));
                flask = true;
                used.insert(rel);
            }
            if !streamlit && (text.contains("import streamlit") || text.contains("from streamlit")) {
                out.push(Proposal::new(
                    "Python",
                    label(&d.rel, &format!("Streamlit {rel}")),
                    RunKind::Python,
                    format!("-m streamlit run {rel}"),
                    &d.rel,
                    format!("Streamlit app {rel}"),
                ));
                streamlit = true;
                used.insert(rel);
            }
        }
        entries += usize::from(fastapi) + usize::from(flask) + usize::from(streamlit);

        // Scripts with a conventional entry-point name and a main guard.
        for &rel in PY_ENTRY_FILES {
            if used.contains(rel) || rel == "__main__.py" {
                continue;
            }
            if read(rel).is_some_and(|t| main_guard(&t)) {
                out.push(Proposal::new(
                    "Python",
                    label(&d.rel, &format!("Run {rel}")),
                    RunKind::Python,
                    rel,
                    &d.rel,
                    format!("{rel} runs as a script (`if __name__ == \"__main__\"`)"),
                ));
                entries += 1;
            }
        }

        // Packages runnable with `python -m` (`pkg/__main__.py`, `src/pkg/__main__.py`).
        let mut packages: Vec<(String, String)> = Vec::new();
        for sub in dirs.iter().filter(|s| s.has("__main__.py") && s.has("__init__.py")) {
            let Some(rel) = sub.rel.strip_prefix(&d.rel).map(|r| r.trim_start_matches('/')) else { continue };
            if !under(&sub.rel, &d.rel) {
                continue;
            }
            let (module, run_dir) = py_module(&d.rel, &format!("{rel}.py"));
            if !module.is_empty() && !module.contains("tests") && packages.len() < 4 {
                packages.push((module, run_dir));
            }
        }
        for (module, run_dir) in packages {
            out.push(Proposal::new(
                "Python",
                label(&d.rel, &format!("python -m {module}")),
                RunKind::Python,
                format!("-m {module}"),
                &run_dir,
                format!("package `{module}` has a __main__.py"),
            ));
            entries += 1;
        }

        // Console scripts declared by the project.
        if let Some(pyproject) = d.read("pyproject.toml") {
            let mut scripts = toml_pairs(&pyproject, "project.scripts");
            scripts.extend(toml_pairs(&pyproject, "tool.poetry.scripts"));
            for (name, target) in scripts.into_iter().take(5) {
                let Some((module, func)) = target.split_once(':') else { continue };
                let (module, func) = (module.trim(), func.trim());
                let ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.'));
                if !ident(module) || !ident(func) {
                    continue;
                }
                let run_dir = if d.dirs.contains("src")
                    && !d.path.join(module.split('.').next().unwrap_or(module)).exists()
                {
                    label_path(&d.rel, "src")
                } else {
                    d.rel.clone()
                };
                out.push(Proposal::new(
                    "Python",
                    label(&d.rel, &name),
                    RunKind::Python,
                    format!("-c \"import sys; from {module} import {func}; sys.exit({func}())\""),
                    &run_dir,
                    format!("console script `{name}` = {target}"),
                ));
                entries += 1;
            }
        }

        let pytest_configured = ["pytest.ini", "conftest.py", "tox.ini"].iter().any(|f| d.has(f))
            || d.read("pyproject.toml").is_some_and(|t| t.contains("pytest"))
            || d.read("setup.cfg").is_some_and(|t| t.contains("pytest"))
            || d.read("requirements-dev.txt").is_some_and(|t| t.contains("pytest"))
            || d.read("requirements.txt").is_some_and(|t| t.contains("pytest"));
        let has_tests = d.dirs.contains("tests") || d.dirs.contains("test");
        if pytest_configured || (has_tests && entries > 0) || (has_tests && PY_MARKERS.iter().any(|m| d.has(m))) {
            out.push(Proposal::new(
                "Python",
                label(&d.rel, "pytest"),
                RunKind::Python,
                "-m pytest",
                &d.rel,
                "the project's tests".into(),
            ));
        }
    }
}

fn label_path(dir: &str, sub: &str) -> String {
    if dir.is_empty() { sub.to_string() } else { format!("{dir}/{sub}") }
}

/// `name(` calls of a build file (`add_executable(`, `executable(`) and their first argument.
fn first_args(text: &str, call: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|t| t.find(call)) {
        let at = from + i;
        let start = at + call.len();
        from = start;
        // A whole word: `add_executable(` but not `my_add_executable(`.
        if at > 0 && lower.as_bytes().get(at - 1).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
            continue;
        }
        let name: String = text
            .get(start..)
            .unwrap_or("")
            .trim_start()
            .trim_start_matches(['\'', '"'])
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .collect();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn c_main(text: &str) -> bool {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*(?:int|void|auto)\s+main\s*\(").ok())
        .as_ref()
        .is_some_and(|re| re.is_match(text))
}

fn sh(script: String) -> String {
    format!("sh -c {}", codecraft_cargo::shell_quote(&script))
}

fn c_cpp(dirs: &[Dir], out: &mut Vec<Proposal>) {
    // CMake: the top CMakeLists.txt builds every executable below it.
    let cmake_tops = tops(dirs, |d| d.has("CMakeLists.txt"));
    for d in cmake_tops.iter().take(4) {
        let configure =
            format!("cmake -S . -B {CMAKE_BUILD_DIR} -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON");
        out.push(Proposal::new(
            "C/C++",
            label(&d.rel, "CMake: Build"),
            RunKind::Command,
            sh(format!("{configure} && cmake --build {CMAKE_BUILD_DIR}")),
            &d.rel,
            "CMakeLists.txt".into(),
        ));
        let mut testing = false;
        let mut exes = 0;
        for sub in dirs.iter().filter(|s| under(&s.rel, &d.rel) && s.has("CMakeLists.txt")) {
            let Some(text) = sub.read("CMakeLists.txt") else { continue };
            testing |= text.to_ascii_lowercase().contains("enable_testing");
            let sub_rel = sub.rel.strip_prefix(&d.rel).unwrap_or("").trim_start_matches('/');
            for exe in first_args(&text, "add_executable(") {
                if exes >= 12 || exe.starts_with("${") {
                    continue;
                }
                exes += 1;
                let bin = if sub_rel.is_empty() {
                    format!("./{CMAKE_BUILD_DIR}/{exe}")
                } else {
                    format!("./{CMAKE_BUILD_DIR}/{sub_rel}/{exe}")
                };
                out.push(Proposal::new(
                    "C/C++",
                    label(&d.rel, &format!("Run {exe}")),
                    RunKind::Command,
                    sh(format!("{configure} && cmake --build {CMAKE_BUILD_DIR} --target {exe} && {bin}")),
                    &d.rel,
                    format!("add_executable({exe}) in {}", label_path(&sub.rel, "CMakeLists.txt")),
                ));
            }
        }
        if testing {
            out.push(Proposal::new(
                "C/C++",
                label(&d.rel, "CTest"),
                RunKind::Command,
                sh(format!(
                    "{configure} && cmake --build {CMAKE_BUILD_DIR} && ctest --test-dir {CMAKE_BUILD_DIR} --output-on-failure"
                )),
                &d.rel,
                "enable_testing() in CMakeLists.txt".into(),
            ));
        }
    }

    // Meson.
    let meson_tops = tops(dirs, |d| d.has("meson.build"));
    for d in meson_tops.iter().take(4) {
        let setup = "{ [ -d builddir ] || meson setup builddir; }";
        out.push(Proposal::new(
            "C/C++",
            label(&d.rel, "Meson: Build"),
            RunKind::Command,
            sh(format!("{setup} && meson compile -C builddir")),
            &d.rel,
            "meson.build".into(),
        ));
        let mut exes = 0;
        for sub in dirs.iter().filter(|s| under(&s.rel, &d.rel) && s.has("meson.build")) {
            let Some(text) = sub.read("meson.build") else { continue };
            let sub_rel = sub.rel.strip_prefix(&d.rel).unwrap_or("").trim_start_matches('/');
            for exe in first_args(&text, "executable(") {
                if exes >= 12 {
                    continue;
                }
                exes += 1;
                let bin = label_path(&label_path("builddir", sub_rel), &exe);
                out.push(Proposal::new(
                    "C/C++",
                    label(&d.rel, &format!("Run {exe}")),
                    RunKind::Command,
                    sh(format!("{setup} && meson compile -C builddir {exe} && ./{bin}")),
                    &d.rel,
                    format!("executable('{exe}') in {}", label_path(&sub.rel, "meson.build")),
                ));
            }
        }
    }

    // Make (when no CMake/Meson project covers the directory).
    let build_tops: Vec<&str> = cmake_tops.iter().chain(meson_tops.iter()).map(|d| d.rel.as_str()).collect();
    for d in tops(dirs, |d| d.has("Makefile") || d.has("makefile") || d.has("GNUmakefile")).into_iter().take(4) {
        if build_tops.iter().any(|t| under(&d.rel, t)) {
            continue;
        }
        let text = d.read("Makefile").or_else(|| d.read("makefile")).or_else(|| d.read("GNUmakefile")).unwrap_or_default();
        out.push(Proposal::new("Make", label(&d.rel, "make"), RunKind::Command, "make", &d.rel, "Makefile".into()));
        let targets = crate::run_configs::make_targets(&text);
        for t in ["run", "test", "check"] {
            if targets.iter().any(|x| x == t) {
                out.push(Proposal::new(
                    "Make",
                    label(&d.rel, &format!("make {t}")),
                    RunKind::Command,
                    format!("make {t}"),
                    &d.rel,
                    format!("`{t}` target of the Makefile"),
                ));
            }
        }
    }

    // No build system: the root (and src/) sources that define main().
    let has_build = |d: &Dir| ["CMakeLists.txt", "meson.build", "Makefile", "makefile", "build.zig"].iter().any(|f| d.has(f));
    let Some(root) = dirs.first().filter(|d| d.rel.is_empty()) else { return };
    if has_build(root) || !build_tops.is_empty() || root.has("Cargo.toml") {
        return;
    }
    let include = if root.dirs.contains("include") { " -Iinclude" } else { "" };
    for sub in ["", "src"] {
        let Some(d) = dirs.iter().find(|d| d.rel == sub) else { continue };
        let sources: Vec<String> = d
            .files
            .iter()
            .filter(|f| Path::new(f).extension().and_then(|e| e.to_str()).is_some_and(|e| C_EXTS.contains(&e)))
            .take(50)
            .map(|f| label_path(sub, f))
            .collect();
        let mains: Vec<&String> =
            sources.iter().filter(|f| read_head(&root.path.join(f)).is_some_and(|t| c_main(&t))).collect();
        let quote = |f: &str| codecraft_cargo::shell_quote(f);
        match mains.as_slice() {
            [] => {}
            [main] => {
                // One program: every source of the directory.
                let stem = Path::new(main.as_str()).file_stem().and_then(|s| s.to_str()).unwrap_or("main");
                let files: Vec<String> = sources.iter().map(|f| quote(f)).collect();
                out.push(Proposal::new(
                    "C/C++",
                    format!("Run {stem}"),
                    RunKind::Cpp,
                    format!("{}{include}", files.join(" ")),
                    "",
                    format!("{main} defines main()"),
                ));
            }
            several => {
                // Several programs side by side (exercises, tools): one configuration each.
                for main in several.iter().take(8) {
                    let stem = Path::new(main.as_str()).file_stem().and_then(|s| s.to_str()).unwrap_or("main");
                    out.push(Proposal::new(
                        "C/C++",
                        format!("Run {stem}"),
                        RunKind::Cpp,
                        format!("{}{include}", quote(main)),
                        "",
                        format!("{main} defines main()"),
                    ));
                }
            }
        }
    }
}

fn zig(dirs: &[Dir], out: &mut Vec<Proposal>) {
    static STEP: OnceLock<Option<Regex>> = OnceLock::new();
    let step = STEP.get_or_init(|| Regex::new(r#"\.step\(\s*"([A-Za-z0-9_\-]+)""#).ok()).as_ref();
    let zig_tops = tops(dirs, |d| d.has("build.zig"));
    for d in zig_tops.iter().take(4) {
        let text = d.read("build.zig").unwrap_or_default();
        out.push(Proposal::new("Zig", label(&d.rel, "zig build"), RunKind::Zig, "build", &d.rel, "build.zig".into()));
        let mut steps: Vec<String> = Vec::new();
        if let Some(re) = step {
            for c in re.captures_iter(&text) {
                if let Some(m) = c.get(1)
                    && !matches!(m.as_str(), "install" | "uninstall")
                    && !steps.iter().any(|s| s == m.as_str())
                    && steps.len() < 8
                {
                    steps.push(m.as_str().to_string());
                }
            }
        }
        for s in steps {
            out.push(Proposal::new(
                "Zig",
                label(&d.rel, &format!("zig build {s}")),
                RunKind::Zig,
                format!("build {s}"),
                &d.rel,
                format!("`{s}` step of build.zig"),
            ));
        }
    }
    if !zig_tops.is_empty() {
        return;
    }
    let Some(root) = dirs.first() else { return };
    for rel in ["src/main.zig", "main.zig"] {
        let Some(text) = read_head(&root.path.join(rel)) else { continue };
        if !text.contains("pub fn main") {
            continue;
        }
        out.push(Proposal::new(
            "Zig",
            format!("zig run {rel}"),
            RunKind::Zig,
            format!("run {rel}"),
            "",
            format!("{rel} defines `pub fn main`"),
        ));
        if text.contains("test \"") || text.contains("test {") {
            out.push(Proposal::new(
                "Zig",
                format!("zig test {rel}"),
                RunKind::Zig,
                format!("test {rel}"),
                "",
                format!("tests in {rel}"),
            ));
        }
        break;
    }
}

fn javascript(dirs: &[Dir], out: &mut Vec<Proposal>) {
    for d in tops(dirs, |d| d.has("package.json")).into_iter().take(6) {
        let Some(pkg) = d.read("package.json").and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        let Some(scripts) = pkg.get("scripts").and_then(Value::as_object) else { continue };
        let pm = crate::run_configs::package_manager(&d.path);
        for name in ["dev", "start", "serve", "test", "build"] {
            if !scripts.contains_key(name) || (name == "serve" && scripts.contains_key("dev")) {
                continue;
            }
            out.push(Proposal::new(
                "JavaScript",
                label(&d.rel, &format!("{pm} run {name}")),
                RunKind::Npm,
                format!("run {name}"),
                &d.rel,
                format!("`{name}` script of package.json"),
            ));
        }
    }
}

fn go(dirs: &[Dir], out: &mut Vec<Proposal>) {
    for d in tops(dirs, |d| d.has("go.mod")).into_iter().take(4) {
        let is_main = |dir: &Dir| {
            dir.files
                .iter()
                .filter(|f| f.ends_with(".go") && !f.ends_with("_test.go"))
                .take(40)
                .any(|f| read_head(&dir.path.join(f)).is_some_and(|t| t.lines().any(|l| l.trim() == "package main")))
        };
        let mut mains: Vec<String> = Vec::new();
        if is_main(d) {
            mains.push(".".into());
        }
        let cmd = label_path(&d.rel, "cmd");
        for sub in dirs.iter().filter(|s| s.rel.rsplit_once('/').is_some_and(|(p, _)| p == cmd)) {
            if mains.len() < 8 && is_main(sub) {
                mains.push(format!("./{}", sub.rel.strip_prefix(&d.rel).unwrap_or(&sub.rel).trim_start_matches('/')));
            }
        }
        for m in mains {
            let name = if m == "." { "go run .".to_string() } else { format!("go run {m}") };
            out.push(Proposal::new(
                "Go",
                label(&d.rel, &name),
                RunKind::Command,
                format!("go run {m}"),
                &d.rel,
                format!("`package main` in {m}"),
            ));
        }
        out.push(Proposal::new(
            "Go",
            label(&d.rel, "go test ./..."),
            RunKind::Command,
            "go test ./...",
            &d.rel,
            "go.mod".into(),
        ));
    }
}

fn docker(dirs: &[Dir], out: &mut Vec<Proposal>) {
    let Some(root) = dirs.first().filter(|d| d.rel.is_empty()) else { return };
    if let Some(f) = COMPOSE_FILES.iter().find(|f| root.has(f)) {
        out.push(Proposal::new(
            "Docker",
            "Docker Compose: up".into(),
            RunKind::DockerCompose,
            format!("{f} up"),
            "",
            (*f).to_string(),
        ));
    } else if root.has("Dockerfile") {
        out.push(Proposal::new(
            "Docker",
            "Dockerfile".into(),
            RunKind::Dockerfile,
            "Dockerfile",
            "",
            "Dockerfile".into(),
        ));
    }
}

// ---------------------------------------------------------------- the agent

/// The prompt that asks a coding agent to propose configurations (it may read the project;
/// it must not change it).
pub fn agent_prompt(detected: &[Proposal]) -> String {
    let list: Vec<Value> = detected
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.config.name,
                "kind": kind_id(p.config.kind),
                "command": p.config.command,
                "workingDir": p.config.working_dir,
            })
        })
        .collect();
    let detected = serde_json::to_string_pretty(&list).unwrap_or_else(|_| "[]".into());
    format!(
        "Propose run configurations for this project in the CodeCraft IDE. Look at the project's \
build files, manifests and entry points (read files only; do not change, create or delete \
anything and do not run commands).\n\n\
Propose one configuration per real entry point and per main task: applications and services \
(binaries, web servers such as Django/FastAPI/Flask, CLIs, `__main__` packages, CMake/Meson/Zig \
executables), the test suites, and the build when it is a separate step. Do NOT propose one per \
source file, per example or per module. Cover every sub-project (for example `backend/` and \
`frontend/`) with `workingDir` set to its directory, relative to the project root.\n\n\
Kinds and what `command` holds:\n\
- cargo: cargo arguments without `cargo` (`run --package app --bin app`, `test --workspace`)\n\
- python: python arguments (`main.py`, `-m pkg`, `-m pytest`), run with the project's interpreter\n\
- django: manage.py arguments (`runserver`)\n\
- fastapi: uvicorn arguments (`app.main:app --reload`)\n\
- flask: flask arguments (`--app app:app run --debug`)\n\
- cpp: C/C++ source files and compiler flags, then optionally `--` and program arguments (compiled and run)\n\
- zig: zig arguments (`build run`, `run src/main.zig`)\n\
- npm: package-manager arguments (`run dev`, `test`)\n\
- compose: a compose file then docker compose arguments (`compose.yaml up`)\n\
- dockerfile: a Dockerfile then docker run options\n\
- command: any program and its arguments (`make test`, `sh -c 'cmake ... && ./build/app'`)\n\n\
CodeCraft detected these already; keep the good ones (same name and command), fix or drop wrong \
ones, and add what is missing:\n```json\n{detected}\n```\n\n\
Reply with only a JSON array, in a ```json code block, of objects \
{{\"name\", \"kind\", \"command\", \"workingDir\", \"env\" (optional object), \"reason\" (one short sentence)}}."
    )
}

/// The id `RunKind::parse` accepts.
pub fn kind_id(kind: RunKind) -> &'static str {
    kind.id()
}

/// The first JSON array in `text`: a ```json block's, else the first `[` that parses.
fn json_array(text: &str) -> Option<Vec<Value>> {
    let mut candidates: Vec<&str> = Vec::new();
    for (i, block) in text.split("```").enumerate() {
        if i % 2 == 1 {
            candidates.push(block.strip_prefix("json").unwrap_or(block));
        }
    }
    candidates.push(text);
    for c in candidates {
        for (i, _) in c.match_indices('[').take(50) {
            let Some(rest) = c.get(i..) else { continue };
            let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Vec<Value>>();
            if let Some(Ok(items)) = stream.next() {
                return Some(items);
            }
        }
    }
    None
}

/// The agent's reply as proposals: entries with an unknown kind, no name, no command or a
/// working directory outside the project are dropped (each with a note in the second list).
pub fn parse_agent_reply(text: &str, root: &Path) -> Result<(Vec<Proposal>, Vec<String>), String> {
    let items = json_array(text).ok_or("the agent's reply has no JSON array of configurations")?;
    let mut out: Vec<Proposal> = Vec::new();
    let mut dropped = Vec::new();
    for item in items.into_iter().take(MAX_PROPOSALS) {
        let str_of = |k: &str| item.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
        let name = str_of("name");
        let command = str_of("command");
        let dir = str_of("workingDir");
        let kind = match RunKind::parse(&str_of("kind")) {
            Ok(k) => k,
            Err(e) => {
                dropped.push(format!("`{name}`: {e}"));
                continue;
            }
        };
        if name.is_empty() || command.is_empty() {
            dropped.push(format!("`{name}`: no name or command"));
            continue;
        }
        if Path::new(&dir).is_absolute() || dir.split(['/', '\\']).any(|c| c == "..") {
            dropped.push(format!("`{name}`: working directory `{dir}` is outside the project"));
            continue;
        }
        let env = item.get("env").map(crate::run_config_store::parse_env).transpose().unwrap_or_default();
        let mut p = Proposal::new("Agent", name, kind, command, &dir, str_of("reason"));
        p.config.env = env.unwrap_or_default();
        p.source = Source::Agent;
        if let Err(e) = p.config.validate(root) {
            dropped.push(e);
            continue;
        }
        if !out.iter().any(|o| same_run(&o.config, &p.config)) {
            out.push(p);
        }
    }
    reconcile(&mut out, &[]);
    Ok((out, dropped))
}

// ---------------------------------------------------------------- session state

/// Background task id shown while detection runs.
const TASK: &str = "runConfig.generate";

/// What a finished detection is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// Show the proposals in Edit Configurations › Generate.
    Review,
    /// Then ask this agent to refine them.
    Agent(Backend, AgentMode),
    /// A project without saved configurations opened: save them.
    AutoSave,
}

/// The Generate view of Edit Configurations.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenView {
    pub proposals: Vec<Proposal>,
    /// What it is waiting for ("Detecting…", "Waiting for Claude Code…"); `None` = ready.
    pub busy: Option<String>,
    pub error: Option<String>,
    /// Entries of the agent's reply that were dropped, and why.
    pub notes: Vec<String>,
    /// The agent job writing the proposals (its feed is in the Agents tool window).
    pub agent_job: Option<u64>,
}

#[derive(Debug, Default)]
pub struct GenState {
    pub view: Option<GenView>,
    /// Generate configurations when a project without saved ones opens (the desktop app turns
    /// this on; the `generate_run_configs` setting can turn it off).
    pub auto: bool,
    detection: Option<(Receiver<Vec<Proposal>>, PathBuf, Purpose)>,
    agent: Option<(JobId, PathBuf)>,
}

impl GenState {
    /// Detection is running.
    pub fn busy(&self) -> bool {
        self.detection.is_some()
    }
}

/// Start detecting in the background.
fn spawn(s: &mut Session, root: PathBuf, purpose: Purpose) {
    let cargo = s.ws.project.as_ref().filter(|p| p.root == root).and_then(|p| p.cargo.clone());
    let (tx, rx) = bounded(1);
    let dir = root.clone();
    std::thread::spawn(move || {
        let _ = tx.send(detect(&dir, cargo.as_ref()));
    });
    s.run_store.generate.detection = Some((rx, root, purpose));
    s.ws.tasks.insert(
        TASK.into(),
        BackgroundTask { title: "Detecting run configurations".into(), message: None, percentage: None },
    );
}

/// The configurations proposals are compared with: the dialog's draft, else the saved ones.
fn current(s: &Session) -> Vec<UserRunConfig> {
    s.run_store.dialog.as_ref().map(|d| d.draft.clone()).unwrap_or_else(|| s.run_store.user.clone())
}

/// After a project loads: generate its configurations once if it has none saved yet.
pub(crate) fn project_loaded(s: &mut Session, root: &Path) {
    s.run_store.generate.view = None;
    s.run_store.generate.agent = None;
    let file_missing = !root.join(crate::run_config_store::FILE).exists();
    if s.run_store.generate.auto
        && s.ws.settings.generate_run_configs
        && file_missing
        && s.run_store.user.is_empty()
        && s.run_store.error.is_none()
    {
        spawn(s, root.to_path_buf(), Purpose::AutoSave);
    }
}

/// `runConfig.generate`: detect (and optionally ask an agent) and show the proposals.
pub(crate) fn start(s: &mut Session, root: PathBuf, agent: Option<(Backend, AgentMode)>) -> Result<(), CommandError> {
    if let Some((backend, mode)) = agent {
        s.agents.refresh_backends();
        let info = s
            .agents
            .backends()
            .iter()
            .find(|b| b.backend == backend)
            .cloned()
            .ok_or_else(|| CommandError::Failed("unknown agent".into()))?;
        crate::agents::mode_support(&info, mode).map_err(CommandError::Failed)?;
    }
    if let Some((job, _)) = s.run_store.generate.agent.take() {
        let _ = crate::agents::stop(s, job);
    }
    let purpose = agent.map_or(Purpose::Review, |(b, m)| Purpose::Agent(b, m));
    s.run_store.generate.view = Some(GenView { busy: Some("Detecting entry points…".into()), ..GenView::default() });
    spawn(s, root, purpose);
    Ok(())
}

/// Close the Generate view (an agent still writing proposals is stopped).
pub(crate) fn cancel(s: &mut Session) {
    s.run_store.generate.view = None;
    if let Some((job, _)) = s.run_store.generate.agent.take() {
        let _ = crate::agents::stop(s, job);
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let ready = match &s.run_store.generate.detection {
        Some((rx, _, _)) => match rx.try_recv() {
            Ok(list) => Some(list),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Vec::new()),
        },
        None => None,
    };
    if let Some(mut list) = ready
        && let Some((_, root, purpose)) = s.run_store.generate.detection.take()
    {
        s.ws.tasks.remove(TASK);
        changed = true;
        if s.run_store.root() == Some(root.as_path()) {
            detected(s, &root, purpose, &mut list);
        }
    }
    changed |= pump_agent(s);
    changed
}

fn detected(s: &mut Session, root: &Path, purpose: Purpose, list: &mut Vec<Proposal>) {
    match purpose {
        Purpose::AutoSave => {
            // Only if nothing was saved meanwhile.
            if list.is_empty() || !s.run_store.user.is_empty() || root.join(crate::run_config_store::FILE).exists() {
                return;
            }
            s.run_store.user = list.iter().map(|p| p.config.clone()).collect();
            match s.run_store.save() {
                Ok(_) => {
                    crate::run_config_store::rebuild(s);
                    let n = list.len();
                    s.notify(
                        NotificationLevel::Info,
                        "Run Configurations",
                        format!(
                            "Created {n} run configuration{} from the project (Run › Edit Configurations… to change them)",
                            if n == 1 { "" } else { "s" }
                        ),
                    );
                }
                Err(e) => {
                    s.run_store.user.clear();
                    s.notify(NotificationLevel::Warning, "Run Configurations", e);
                }
            }
        }
        Purpose::Review => {
            let saved = current(s);
            reconcile(list, &saved);
            if let Some(v) = &mut s.run_store.generate.view {
                v.proposals = std::mem::take(list);
                v.busy = None;
            }
        }
        Purpose::Agent(backend, mode) => {
            if s.run_store.generate.view.is_none() {
                return;
            }
            let prompt = agent_prompt(list);
            let req = crate::agents::StartRequest {
                backend,
                mode,
                prompt,
                attachments: Vec::new(),
                cwd: Some(root.to_path_buf()),
                title: Some("Generate run configurations".into()),
                include_file: false,
                include_selection: false,
            };
            let saved = current(s);
            reconcile(list, &saved);
            let started = crate::agents::start(s, req);
            if let Some(v) = &mut s.run_store.generate.view {
                // The detected ones stay visible (and usable) while the agent works.
                v.proposals = std::mem::take(list);
                match started {
                    Ok(job) => {
                        v.busy = Some(format!("Waiting for {}…", backend.label()));
                        v.agent_job = Some(job.0);
                    }
                    Err(e) => {
                        v.busy = None;
                        v.error = Some(format!("{} could not start: {e}", backend.label()));
                    }
                }
            }
            if let Some(job) = s.run_store.generate.view.as_ref().and_then(|v| v.agent_job) {
                s.run_store.generate.agent = Some((JobId(job), root.to_path_buf()));
            }
        }
    }
}

/// The agent's text: its messages and final result.
fn reply_text(job: &crate::agents::Job) -> String {
    let mut out = String::new();
    for a in &job.activity {
        match a {
            Activity::Text { text } | Activity::Result { text: Some(text), .. } => {
                out.push_str(text);
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}

fn pump_agent(s: &mut Session) -> bool {
    let Some((id, root)) = s.run_store.generate.agent.clone() else { return false };
    let Some(job) = s.agents.job(id) else {
        s.run_store.generate.agent = None;
        if let Some(v) = &mut s.run_store.generate.view {
            v.busy = None;
            v.error = Some("the agent job is gone".into());
        }
        return true;
    };
    if job.is_running() {
        return false;
    }
    let (status, text, label) = (job.status, reply_text(job), job.backend.label());
    s.run_store.generate.agent = None;
    let saved = current(s);
    let Some(v) = &mut s.run_store.generate.view else { return true };
    v.busy = None;
    match parse_agent_reply(&text, &root) {
        Ok((mut list, dropped)) => {
            reconcile(&mut list, &saved);
            // Detected proposals the agent did not keep are dropped; it saw them all.
            v.proposals = list;
            v.notes = dropped;
            v.error = None;
        }
        Err(e) => {
            v.error = Some(match status {
                JobStatus::Failed | JobStatus::Stopped => {
                    format!("{label} did not finish ({}); showing the detected configurations", status.label())
                }
                _ => format!("{label}: {e}; showing the detected configurations"),
            });
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn names(ps: &[Proposal]) -> Vec<String> {
        ps.iter().map(|p| p.config.name.clone()).collect()
    }

    fn find<'a>(ps: &'a [Proposal], name: &str) -> &'a Proposal {
        ps.iter().find(|p| p.config.name == name).unwrap_or_else(|| panic!("no `{name}` in {:?}", names(ps)))
    }

    #[test]
    fn python_entry_points_but_not_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "pyproject.toml", "[project]\nname = \"svc\"\n\n[project.scripts]\nsvc-cli = \"svc.cli:main\"\n\n[tool.pytest.ini_options]\n");
        write(r, "manage.py", "import os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'site.settings')\n");
        write(r, "app/main.py", "from fastapi import FastAPI\napi = FastAPI()\n");
        write(r, "svc/__init__.py", "");
        write(r, "svc/__main__.py", "print(1)\n");
        write(r, "svc/cli.py", "def main():\n    return 0\n");
        write(r, "run.py", "def go(): pass\n\nif __name__ == \"__main__\":\n    go()\n");
        // Plain modules and scripts with other names get nothing.
        write(r, "svc/util.py", "if __name__ == '__main__':\n    pass\n");
        write(r, "scripts/migrate_data.py", "if __name__ == '__main__':\n    pass\n");
        write(r, "tests/test_a.py", "def test_a(): pass\n");
        let ps = detect(r, None);
        assert_eq!(find(&ps, "Django server").config.kind, RunKind::Django);
        let fast = find(&ps, "FastAPI app.main:api");
        assert_eq!((fast.config.kind, fast.config.command.as_str()), (RunKind::FastApi, "app.main:api --reload"));
        assert_eq!(find(&ps, "Run run.py").config.command, "run.py");
        assert_eq!(find(&ps, "python -m svc").config.command, "-m svc");
        assert!(find(&ps, "svc-cli").config.command.contains("from svc.cli import main"));
        assert_eq!(find(&ps, "pytest").config.command, "-m pytest");
        let all = names(&ps).join("|");
        assert!(!all.contains("util") && !all.contains("migrate"), "{all}");
        assert!(ps.iter().all(|p| p.config.validate(r).is_ok()), "{ps:?}");
    }

    #[test]
    fn flask_in_src_layout_runs_from_src() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "requirements.txt", "flask\n");
        write(r, "src/app.py", "from flask import Flask\napp = Flask(__name__)\n");
        let ps = detect(r, None);
        let flask = find(&ps, "Flask app:app");
        assert_eq!(flask.config.command, "--app app:app run --debug");
        assert_eq!(flask.config.working_dir, "src");
    }

    #[test]
    fn rust_and_python_sub_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "Cargo.toml", "[package]\nname = \"core\"\nversion = \"0.1.0\"\n");
        write(r, "src/main.rs", "fn main() {}\n");
        write(r, "src/bin/tool.rs", "fn main() {}\n");
        write(r, "web/requirements.txt", "flask\n");
        write(r, "web/app.py", "from flask import Flask\nserver = Flask(__name__)\n");
        // Inside the Rust target dir and node_modules nothing is looked at.
        write(r, "target/debug/build/x/main.py", "if __name__ == '__main__': pass\n");
        write(r, "node_modules/x/package.json", "{\"scripts\": {\"dev\": \"x\"}}");
        let ps = detect(r, None);
        assert_eq!(find(&ps, "Run core").config.command, "run");
        assert_eq!(find(&ps, "Run tool").config.command, "run --bin tool");
        assert_eq!(find(&ps, "Test workspace").config.command, "test");
        let flask = find(&ps, "web: Flask app:server");
        assert_eq!(flask.config.working_dir, "web");
        assert!(!names(&ps).iter().any(|n| n.contains("npm")), "{:?}", names(&ps));
    }

    #[test]
    fn rust_from_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let bin = |n: &str| codecraft_cargo::Target { name: n.into(), kind: vec!["bin".into()], ..Default::default() };
        let pkg = |n: &str, targets| codecraft_cargo::Package {
            id: n.into(),
            name: n.into(),
            manifest_path: r.join(n).join("Cargo.toml"),
            targets,
            ..Default::default()
        };
        let meta = CargoMetadata {
            workspace_root: r.to_path_buf(),
            packages: vec![pkg("server", vec![bin("server")]), pkg("lib", vec![])],
            workspace_members: vec!["server".into(), "lib".into()],
            ..Default::default()
        };
        write(r, "server/Cargo.toml", "[package]\nname = \"server\"\n");
        write(r, "server/src/main.rs", "fn main() {}\n");
        let ps = detect(r, Some(&meta));
        assert_eq!(find(&ps, "Run server").config.command, "run --package server --bin server");
        assert_eq!(find(&ps, "Test workspace").config.command, "test --workspace");
        // The member's Cargo.toml is covered by the metadata.
        assert_eq!(ps.iter().filter(|p| p.group == "Rust").count(), 2, "{:?}", names(&ps));
    }

    #[test]
    fn cmake_executables_in_subdirectories_and_ctest() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "CMakeLists.txt", "project(x)\nenable_testing()\nadd_subdirectory(tools)\nadd_executable(app main.cpp)\n");
        write(r, "tools/CMakeLists.txt", "add_executable(gen gen.cpp)\nmy_add_executable(nope)\n");
        write(r, "main.cpp", "int main() {}\n");
        let ps = detect(r, None);
        assert!(find(&ps, "CMake: Build").config.command.starts_with("sh -c "));
        assert!(find(&ps, "Run app").config.command.contains("./cmake-build-debug/app"));
        assert!(find(&ps, "Run gen").config.command.contains("./cmake-build-debug/tools/gen"));
        assert!(names(&ps).contains(&"CTest".to_string()));
        assert!(!names(&ps).iter().any(|n| n.contains("nope")));
        // CMake covers main.cpp: no compile-it-directly configuration.
        assert!(!ps.iter().any(|p| p.config.kind == RunKind::Cpp));
        assert!(ps.iter().all(|p| p.config.validate(r).is_ok()));
    }

    #[test]
    fn plain_c_sources_with_main() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "src/main.c", "#include \"util.h\"\nint main(void) { return util(); }\n");
        write(r, "src/util.c", "int util(void) { return 0; }\n");
        write(r, "include/util.h", "int util(void);\n");
        let ps = detect(r, None);
        let p = find(&ps, "Run main");
        assert_eq!(p.config.kind, RunKind::Cpp);
        assert_eq!(p.config.command, "src/main.c src/util.c -Iinclude");

        // Several programs: one each, alone.
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "a.cpp", "int main() {}\n");
        write(r, "b.cpp", "int main(int argc, char** argv) {}\n");
        write(r, "c.cpp", "int helper() { return 1; }\n");
        let ps = detect(r, None);
        assert_eq!(find(&ps, "Run a").config.command, "a.cpp");
        assert_eq!(find(&ps, "Run b").config.command, "b.cpp");
        assert!(!names(&ps).contains(&"Run c".to_string()));
    }

    #[test]
    fn zig_build_steps_or_main() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(
            r,
            "build.zig",
            "pub fn build(b: *std.Build) void {\n  const run_step = b.step(\"run\", \"Run the app\");\n  const t = b.step(\"test\", \"Run tests\");\n  _ = b.step(\"install\", \"x\");\n}\n",
        );
        write(r, "src/main.zig", "pub fn main() !void {}\n");
        let ps = detect(r, None);
        assert_eq!(names(&ps), vec!["zig build", "zig build run", "zig build test"]);

        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "src/main.zig", "pub fn main() !void {}\ntest \"x\" {}\n");
        write(r, "src/other.zig", "pub fn helper() void {}\n");
        let ps = detect(r, None);
        assert_eq!(names(&ps), vec!["zig run src/main.zig", "zig test src/main.zig"]);
    }

    #[test]
    fn make_js_go_and_docker() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "Makefile", "all:\n\tcc x.c\nrun:\n\t./a.out\nlint:\n\ttrue\n");
        write(r, "frontend/package.json", r#"{"scripts": {"dev": "vite", "lint": "eslint", "test": "vitest"}}"#);
        write(r, "frontend/pnpm-lock.yaml", "");
        write(r, "svc/go.mod", "module svc\n");
        write(r, "svc/cmd/api/main.go", "package main\nfunc main() {}\n");
        write(r, "compose.yaml", "services: {}\n");
        let ps = detect(r, None);
        let n = names(&ps);
        assert!(n.contains(&"make".into()) && n.contains(&"make run".into()) && !n.contains(&"make lint".into()), "{n:?}");
        assert_eq!(find(&ps, "frontend: pnpm run dev").config.working_dir, "frontend");
        assert!(n.contains(&"frontend: pnpm run test".into()) && !n.iter().any(|x| x.contains("lint")));
        assert_eq!(find(&ps, "svc: go run ./cmd/api").config.command, "go run ./cmd/api");
        assert_eq!(find(&ps, "Docker Compose: up").config.kind, RunKind::DockerCompose);
    }

    #[test]
    fn reconcile_marks_existing_and_renames_clashes() {
        let mut ps = vec![
            Proposal::new("Rust", "Run app".into(), RunKind::Cargo, "run", "", String::new()),
            Proposal::new("Rust", "Test workspace".into(), RunKind::Cargo, "test", "", String::new()),
        ];
        let saved = vec![
            UserRunConfig { name: "Mine".into(), kind: RunKind::Cargo, command: "run ".into(), ..Default::default() },
            UserRunConfig { name: "Test workspace".into(), kind: RunKind::Command, command: "x".into(), ..Default::default() },
        ];
        reconcile(&mut ps, &saved);
        assert!(ps[0].exists && !ps[0].selected);
        assert_eq!(ps[0].config.name, "Mine");
        assert!(!ps[1].exists && ps[1].selected);
        assert_eq!(ps[1].config.name, "Test workspace (2)");
    }

    #[test]
    fn agent_replies_are_parsed_and_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        std::fs::create_dir_all(r.join("api")).unwrap();
        let reply = "Here you go:\n```json\n[\n {\"name\": \"API\", \"kind\": \"fastapi\", \"command\": \"main:app --reload\", \"workingDir\": \"api\", \"env\": {\"DEBUG\": \"1\"}, \"reason\": \"the service\"},\n {\"name\": \"Bad\", \"kind\": \"cobol\", \"command\": \"x\"},\n {\"name\": \"Escape\", \"kind\": \"command\", \"command\": \"ls\", \"workingDir\": \"../x\"},\n {\"name\": \"Gone\", \"kind\": \"command\", \"command\": \"ls\", \"workingDir\": \"nope\"},\n {\"name\": \"Tests\", \"kind\": \"cargo\", \"command\": \"test\"}\n]\n```\nDone.";
        let (ps, dropped) = parse_agent_reply(reply, r).unwrap();
        assert_eq!(names(&ps), vec!["API", "Tests"]);
        assert_eq!(ps[0].config.env, vec![("DEBUG".to_string(), "1".to_string())]);
        assert_eq!(ps[0].reason, "the service");
        assert_eq!(ps[0].source, Source::Agent);
        assert_eq!(dropped.len(), 3, "{dropped:?}");
        // A bare array works too; garbage is an error, not a panic.
        assert_eq!(parse_agent_reply("[{\"name\":\"x\",\"kind\":\"cargo\",\"command\":\"run\"}]", r).unwrap().0.len(), 1);
        assert!(parse_agent_reply("no [ json here [[", r).is_err());
        assert!(parse_agent_reply("", r).is_err());
    }

    #[test]
    fn prompt_lists_detected_configurations() {
        let p = Proposal::new("Rust", "Run app".into(), RunKind::Cargo, "run", "", String::new());
        let prompt = agent_prompt(&[p]);
        assert!(prompt.contains("\"name\": \"Run app\"") && prompt.contains("```json"));
        assert!(prompt.contains("do not change"));
    }

    #[test]
    fn hostile_trees_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "a/b/c/d/e/f/requirements.txt", "flask\n");
        write(r, "a/b/c/d/e/f/app.py", "from flask import Flask\napp = Flask(__name__)\n");
        write(r, "pyproject.toml", "[project.scripts]\nbad = \"x; rm -rf /:main\"\n[project\n");
        write(r, "package.json", "{not json");
        write(r, "CMakeLists.txt", "add_executable(");
        let ps = detect(r, None);
        assert!(!names(&ps).iter().any(|n| n.contains("Flask") || n == "bad"), "{:?}", names(&ps));
        assert!(detect(&r.join("missing"), None).is_empty());
    }
}
