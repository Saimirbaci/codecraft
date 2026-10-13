//! Python project helpers: Django / FastAPI / Flask run configurations (detected from the
//! project and as user configuration kinds), doctest runs, and Tools › Create setup.py / Run
//! setup.py Task.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::run_config_store::RunKind;
use crate::runs::{ProcessCommand, RunConfig};

/// Source read per candidate file when looking for an application object.
const MAX_SCAN_BYTES: u64 = 256 * 1024;

/// Files that usually hold a FastAPI or Flask application, relative to the project.
const APP_FILES: [&str; 9] =
    ["main.py", "app.py", "server.py", "api.py", "wsgi.py", "asgi.py", "app/main.py", "src/main.py", "src/app.py"];

/// The common `setup.py` commands offered by Run setup.py Task….
pub const SETUP_TASKS: [&str; 9] =
    ["build", "sdist", "bdist_wheel", "install", "develop", "clean", "check", "egg_info", "test"];

fn read_head(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    f.take(MAX_SCAN_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

fn app_regex(class: &str) -> Option<&'static Regex> {
    static FASTAPI: OnceLock<Option<Regex>> = OnceLock::new();
    static FLASK: OnceLock<Option<Regex>> = OnceLock::new();
    let cell = if class == "FastAPI" { &FASTAPI } else { &FLASK };
    cell.get_or_init(|| {
        Regex::new(&format!(r"(?m)^([A-Za-z_][A-Za-z0-9_]*)\s*(?::\s*[A-Za-z_.]+\s*)?=\s*(?:[a-z_]+\.)?{class}\(")).ok()
    })
    .as_ref()
}

/// The application variable a module defines (`app = FastAPI()` → `app`).
pub fn app_variable(source: &str, class: &str) -> Option<String> {
    app_regex(class)?.captures(source)?.get(1).map(|m| m.as_str().to_string())
}

/// `app/main.py` → (`app.main`, the directory to run in). Files under `src/` run from `src`.
fn module_of(root: &Path, rel: &str) -> (String, PathBuf) {
    let (dir, rel) = match rel.strip_prefix("src/") {
        Some(r) => (root.join("src"), r),
        None => (root.to_path_buf(), rel),
    };
    (rel.trim_end_matches(".py").replace('/', "."), dir)
}

/// Run configurations for the web frameworks the project uses: `manage.py runserver` (Django),
/// `uvicorn module:app --reload` (FastAPI), `flask --app module:app run --debug` (Flask).
pub fn framework_configs(root: &Path, python: &str) -> Vec<RunConfig> {
    let mut out = Vec::new();
    if read_head(&root.join("manage.py")).is_some_and(|t| t.contains("django") || t.contains("DJANGO_SETTINGS_MODULE"))
    {
        out.push(RunConfig::process("Django server", ProcessCommand::new(python, &["manage.py", "runserver"], root)));
    }
    let mut fastapi = false;
    let mut flask = false;
    for rel in APP_FILES {
        let Some(text) = read_head(&root.join(rel)) else { continue };
        if !fastapi && let Some(var) = app_variable(&text, "FastAPI") {
            let (module, dir) = module_of(root, rel);
            let target = format!("{module}:{var}");
            out.push(RunConfig::process(
                format!("FastAPI {target}"),
                ProcessCommand::new(python, &["-m", "uvicorn", target.as_str(), "--reload"], dir),
            ));
            fastapi = true;
        }
        if !flask && let Some(var) = app_variable(&text, "Flask") {
            let (module, dir) = module_of(root, rel);
            let target = format!("{module}:{var}");
            out.push(RunConfig::process(
                format!("Flask {target}"),
                ProcessCommand::new(python, &["-m", "flask", "--app", target.as_str(), "run", "--debug"], dir),
            ));
            flask = true;
        }
    }
    out
}

/// The program and arguments of a framework run configuration kind: `python manage.py <args>`,
/// `python -m uvicorn <args>`, `python -m flask <args>`.
pub fn framework_command(kind: RunKind, python: String, args: Vec<String>) -> (String, Vec<String>) {
    let prefix: &[&str] = match kind {
        RunKind::Django => &["manage.py"],
        RunKind::FastApi => &["-m", "uvicorn"],
        RunKind::Flask => &["-m", "flask"],
        _ => &[],
    };
    let mut all: Vec<String> = prefix.iter().map(|s| (*s).to_string()).collect();
    all.extend(args);
    (python, all)
}

/// `python -m doctest -v file.py` as a run configuration.
pub fn doctest_config(root: &Path, file: &Path, python: &str) -> RunConfig {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let shown = file.strip_prefix(root).unwrap_or(file).display().to_string();
    RunConfig::process(
        format!("Doctests in {name}"),
        ProcessCommand::new(python, &["-m", "doctest", "-v", shown.as_str()], root),
    )
}

/// Whether a Python source has doctests (`>>> ` in a docstring or comment line).
pub fn has_doctests(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(">>> "))
}

/// Fields of a generated `setup.py`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub author_email: String,
    pub url: String,
    pub license: String,
}

/// Python string literal (JSON escaping is valid Python for strings).
fn py_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Requirements from `requirements.txt` (no comments, options or editable installs).
pub fn requirements(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty() && !l.starts_with('-'))
        .map(str::to_owned)
        .take(500)
        .collect()
}

/// The text of a new `setup.py`.
pub fn setup_py(info: &SetupInfo, requirements: &[String]) -> String {
    let mut out = String::from("from setuptools import find_packages, setup\n\nsetup(\n");
    let field = |out: &mut String, key: &str, value: &str| out.push_str(&format!("    {key}={},\n", py_str(value)));
    field(&mut out, "name", &info.name);
    field(&mut out, "version", &info.version);
    out.push_str("    packages=find_packages(exclude=[\"tests\", \"tests.*\"]),\n");
    field(&mut out, "url", &info.url);
    field(&mut out, "license", &info.license);
    field(&mut out, "author", &info.author);
    field(&mut out, "author_email", &info.author_email);
    field(&mut out, "description", &info.description);
    if !requirements.is_empty() {
        out.push_str("    install_requires=[\n");
        for r in requirements {
            out.push_str(&format!("        {},\n", py_str(r)));
        }
        out.push_str("    ],\n");
    }
    out.push_str(")\n");
    out
}

/// Defaults for a new `setup.py`: the project's name (from `pyproject.toml` when it has one).
pub fn default_setup_info(root: &Path) -> SetupInfo {
    let pyproject = read_head(&root.join("pyproject.toml")).unwrap_or_default();
    let value = |key: &str| -> Option<String> {
        pyproject.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches(['"', '\'']).to_string()).filter(|v| !v.is_empty())
        })
    };
    let dir = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    SetupInfo {
        name: value("name").unwrap_or(dir),
        version: value("version").unwrap_or_else(|| "0.1.0".into()),
        description: value("description").unwrap_or_default(),
        ..SetupInfo::default()
    }
}

/// A `setup.py` command name (`sdist`, `bdist_wheel`).
pub fn valid_task(task: &str) -> bool {
    let mut chars = task.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && task.len() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_frameworks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(framework_configs(root, "py").is_empty());
        std::fs::write(root.join("manage.py"), "import os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'x')\n")
            .unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/main.py"), "from fastapi import FastAPI\n\napi: FastAPI = FastAPI(title='x')\n")
            .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.py"), "import flask\nserver = flask.Flask(__name__)\n").unwrap();
        let configs = framework_configs(root, "py");
        let lines: Vec<(String, String)> = configs.iter().map(|c| (c.name.clone(), c.command_line())).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0].0, "Django server");
        assert!(lines[0].1.ends_with("py manage.py runserver"), "{lines:?}");
        assert_eq!(lines[1].0, "FastAPI app.main:api");
        assert!(lines[1].1.contains("-m uvicorn app.main:api --reload"));
        assert_eq!(lines[2].0, "Flask app:server");
        assert_eq!(configs[2].command.as_ref().unwrap().cwd, root.join("src"));
        assert_eq!(app_variable("x = 1\n# app = FastAPI()\n", "FastAPI"), None);
        assert_eq!(app_variable("app = Flask(__name__)", "Flask").as_deref(), Some("app"));
    }

    #[test]
    fn framework_commands() {
        let (p, a) = framework_command(RunKind::Django, "py".into(), vec!["runserver".into(), "8001".into()]);
        assert_eq!((p.as_str(), a), ("py", vec!["manage.py".to_string(), "runserver".into(), "8001".into()]));
        let (_, a) = framework_command(RunKind::FastApi, "py".into(), vec!["main:app".into()]);
        assert_eq!(a, ["-m", "uvicorn", "main:app"]);
        let (_, a) = framework_command(RunKind::Flask, "py".into(), vec!["run".into()]);
        assert_eq!(a, ["-m", "flask", "run"]);
    }

    #[test]
    fn setup_py_and_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname = \"demo\"\nversion = \"1.2\"\n").unwrap();
        let info = default_setup_info(tmp.path());
        assert_eq!((info.name.as_str(), info.version.as_str()), ("demo", "1.2"));
        let reqs = requirements("requests>=2  # http\n\n-e .\n# comment\nflask\n");
        assert_eq!(reqs, ["requests>=2", "flask"]);
        let text = setup_py(&SetupInfo { description: "say \"hi\"".into(), ..info }, &reqs);
        assert!(text.contains("name=\"demo\",") && text.contains("version=\"1.2\","));
        assert!(text.contains("description=\"say \\\"hi\\\"\","));
        assert!(text.contains("install_requires=[\n        \"requests>=2\",\n        \"flask\",\n    ],"));
        assert!(
            valid_task("bdist_wheel")
                && valid_task("egg_info")
                && !valid_task("--help")
                && !valid_task("a b")
                && !valid_task("")
        );
    }

    #[test]
    fn doctests() {
        assert!(has_doctests("def f():\n    \"\"\"\n    >>> f()\n    1\n    \"\"\"\n"));
        assert!(!has_doctests("x = 1\n"));
        let c = doctest_config(Path::new("/p"), Path::new("/p/pkg/m.py"), "py");
        assert_eq!(c.name, "Doctests in m.py");
        assert_eq!(c.command.as_ref().unwrap().args, ["-m", "doctest", "-v", "pkg/m.py"]);
        assert_eq!(c.command.as_ref().unwrap().cwd, Path::new("/p"));
    }
}
