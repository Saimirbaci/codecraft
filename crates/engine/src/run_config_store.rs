//! User run configurations (Run › Edit Configurations…), persisted per project in
//! `<project>/.codecraft/run-configurations.json`, next to the configurations derived from Cargo
//! targets and the other build systems (`runs::derive_configs`, `run_configs::derive_polyglot`).
//!
//! A user configuration is either a Cargo command (`run --package app --bin app -- --verbose`,
//! like RustRover's Cargo configuration's Command field) or any other command line (`make test`,
//! `sh -c 'a && b'`), a Python script, an npm script, a Zig build or a C/C++ program (compiled
//! and run), with environment variables (optionally loaded from a `.env` file) and a working
//! directory (empty = the project).
//! `Session::run_configs` lists the user configurations first, then the derived ones.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use codecraft_cargo::{CargoCommand, CargoInvocation, TargetSelector};

use crate::Session;
use crate::runs::{ProcessCommand, RunConfig};

/// Where user configurations live, relative to the project root.
pub const FILE: &str = ".codecraft/run-configurations.json";
const MAX_CONFIGS: usize = 500;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    /// `command` is a cargo command line without the leading `cargo`.
    #[default]
    Cargo,
    /// `command` is a program and its arguments (shell-style quoting).
    Command,
    /// `command` is a script (or `-m module`) and its arguments, run by the project's Python.
    Python,
    /// `command` is source files (and compiler flags), then optionally `--` and program
    /// arguments: compiled with `cc`/`c++` into `.codecraft/build/` and run.
    Cpp,
    /// `command` is the arguments of `zig` (`build run`, `run src/main.zig -- --arg`).
    Zig,
    /// `command` is the arguments of the project's package manager (`run dev`, `test`).
    Npm,
    /// `command` is the arguments of `manage.py` (`runserver 8000`), run by the project's Python.
    Django,
    /// `command` is the arguments of uvicorn (`main:app --reload`), run by the project's Python.
    #[serde(rename = "fastapi")]
    FastApi,
    /// `command` is the arguments of `flask` (`--app app run --debug`), run by the project's Python.
    Flask,
    /// `command` is a Dockerfile, then `docker run` options, then optionally `--` and the
    /// container's command: built and run with `--rm` (M11 track D).
    Dockerfile,
    /// `command` is an optional compose file, then `docker compose` arguments (`up`, `up web`,
    /// `down`; default `up`) (M11 track D).
    #[serde(rename = "compose")]
    DockerCompose,
}

impl RunKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cargo" => Ok(Self::Cargo),
            "command" | "shell" | "process" => Ok(Self::Command),
            "python" | "python3" | "py" => Ok(Self::Python),
            "cpp" | "c" | "c++" | "c/c++" | "cc" => Ok(Self::Cpp),
            "zig" => Ok(Self::Zig),
            "npm" | "node" | "js" => Ok(Self::Npm),
            "django" => Ok(Self::Django),
            "fastapi" | "uvicorn" => Ok(Self::FastApi),
            "flask" => Ok(Self::Flask),
            "dockerfile" | "docker" => Ok(Self::Dockerfile),
            "compose" | "docker-compose" | "dockercompose" => Ok(Self::DockerCompose),
            other => Err(format!(
                "unknown configuration type `{other}` (cargo, command, python, cpp, zig, npm, django, fastapi, flask, dockerfile or compose)"
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo",
            Self::Command => "Command",
            Self::Python => "Python",
            Self::Cpp => "C/C++",
            Self::Zig => "Zig",
            Self::Npm => "Npm",
            Self::Django => "Django",
            Self::FastApi => "FastAPI",
            Self::Flask => "Flask",
            Self::Dockerfile => "Dockerfile",
            Self::DockerCompose => "Docker Compose",
        }
    }

    /// Every kind, in the order the UI offers them.
    pub const ALL: [RunKind; 11] = [
        Self::Cargo,
        Self::Command,
        Self::Python,
        Self::Cpp,
        Self::Zig,
        Self::Npm,
        Self::Django,
        Self::FastApi,
        Self::Flask,
        Self::Dockerfile,
        Self::DockerCompose,
    ];

    /// What the Command field of a new configuration of this kind starts with.
    pub fn default_command(self) -> &'static str {
        match self {
            Self::Cargo => "run",
            Self::Command => "make",
            Self::Python => "main.py",
            Self::Cpp => "main.c",
            Self::Zig => "build run",
            Self::Npm => "run dev",
            Self::Django => "runserver",
            Self::FastApi => "main:app --reload",
            Self::Flask => "--app app run --debug",
            Self::Dockerfile => "Dockerfile",
            Self::DockerCompose => "compose.yaml up",
        }
    }

    /// The Command field's placeholder.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Cargo => "run --package app --bin app -- --arg",
            Self::Command => "make test   or   sh -c 'a && b'",
            Self::Python => "main.py --arg   or   -m pytest",
            Self::Cpp => "main.c util.c -O2 -- --arg",
            Self::Zig => "build run   or   run src/main.zig",
            Self::Npm => "run dev   or   test",
            Self::Django => "runserver 8000   or   migrate",
            Self::FastApi => "main:app --reload --port 8000",
            Self::Flask => "--app app run --debug",
            Self::Dockerfile => "Dockerfile -p 8080:80 -- app --port 80",
            Self::DockerCompose => "compose.yaml up web   or   down",
        }
    }
}

/// One user run configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserRunConfig {
    pub name: String,
    pub kind: RunKind,
    pub command: String,
    pub env: Vec<(String, String)>,
    /// A `.env` file (relative = from the project) whose variables are added to the environment
    /// when the configuration runs; `env` wins over it. Empty = none.
    pub env_file: String,
    /// Empty = the project root (the Cargo workspace root for Cargo); relative = from the project.
    pub working_dir: String,
    /// Where it runs (M11: Docker and SSH targets).
    pub target: crate::run_targets::RunTarget,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ConfigFile {
    version: u32,
    configurations: Vec<UserRunConfig>,
}

// ---------------------------------------------------------------- parsing

/// Split a command line into words: whitespace separates, `'…'` is literal, `"…"` allows `\"`
/// and `\\`, a backslash outside quotes escapes the next character.
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("unterminated ' quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\' | '$' | '`')) => cur.push(ch),
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch);
                            }
                            None => return Err("unterminated \" quote".into()),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err("unterminated \" quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(ch) = chars.next() {
                    cur.push(ch);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

fn join_words<S: AsRef<str>>(words: &[S]) -> String {
    words.iter().map(|w| codecraft_cargo::shell_quote(w.as_ref())).collect::<Vec<_>>().join(" ")
}

fn cargo_command(word: &str) -> Option<CargoCommand> {
    Some(match word {
        "build" | "b" => CargoCommand::Build,
        "check" | "c" => CargoCommand::Check,
        "test" | "t" => CargoCommand::Test,
        "run" | "r" => CargoCommand::Run,
        "clippy" => CargoCommand::Clippy,
        "doc" | "d" => CargoCommand::Doc,
        "clean" => CargoCommand::Clean,
        "bench" => CargoCommand::Bench,
        "update" => CargoCommand::Update,
        "fmt" => CargoCommand::Fmt,
        _ => return None,
    })
}

/// A cargo command line (without `cargo`) as an invocation in `cwd`.
pub fn parse_cargo(command: &str, cwd: &Path) -> Result<CargoInvocation, String> {
    let mut words = split_words(command)?;
    if words.first().is_some_and(|w| w == "cargo") {
        words.remove(0);
    }
    let (before, after) = match words.iter().position(|w| w == "--") {
        Some(i) => (words.get(..i).unwrap_or_default().to_vec(), words.get(i + 1..).unwrap_or_default().to_vec()),
        None => (words, Vec::new()),
    };
    let mut it = before.into_iter();
    let sub = it.next().ok_or("the Cargo command is empty")?;
    let cmd = cargo_command(&sub)
        .ok_or_else(|| format!("unsupported cargo command `{sub}` (use a Command configuration for `cargo {sub}`)"))?;
    let mut inv = CargoInvocation::new(cmd, cwd);
    while let Some(w) = it.next() {
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (w.clone(), None),
        };
        let mut take = || inline.clone().or_else(|| it.next()).ok_or_else(|| format!("`{flag}` needs a value"));
        match flag.as_str() {
            "--package" | "-p" => inv.package = Some(take()?),
            "--bin" => inv.target = Some(TargetSelector::Bin(take()?)),
            "--example" => inv.target = Some(TargetSelector::Example(take()?)),
            "--test" => inv.target = Some(TargetSelector::Test(take()?)),
            "--bench" => inv.target = Some(TargetSelector::Bench(take()?)),
            "--lib" => inv.target = Some(TargetSelector::Lib),
            "--all-targets" => inv.target = Some(TargetSelector::AllTargets),
            "--release" | "-r" => inv.release = true,
            "--features" | "-F" => {
                inv.features.extend(take()?.split([',', ' ']).filter(|f| !f.is_empty()).map(String::from));
            }
            "--all-features" => inv.all_features = true,
            "--no-default-features" => inv.no_default_features = true,
            // The runner adds its own message format.
            "--message-format" => {
                take()?;
            }
            _ => inv.extra_args.push(w),
        }
    }
    inv.program_args = after;
    Ok(inv)
}

/// Environment variables from params: `"A=1;B=2"`, `{"A": "1"}`, `[["A", "1"]]` or `["A=1"]`.
pub fn parse_env(v: &Value) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, val: &str| -> Result<(), String> {
        let k = k.trim();
        if k.is_empty() || k.contains('=') || k.contains('\0') || val.contains('\0') {
            return Err(format!("bad environment variable name `{k}`"));
        }
        out.retain(|(o, _)| o != k);
        out.push((k.to_string(), val.to_string()));
        Ok(())
    };
    match v {
        Value::Null => {}
        Value::String(s) => {
            for part in s.split([';', '\n']).map(str::trim).filter(|p| !p.is_empty()) {
                let (k, val) = part.split_once('=').ok_or_else(|| format!("`{part}` is not NAME=value"))?;
                push(k, val)?;
            }
        }
        Value::Object(m) => {
            for (k, val) in m {
                push(k, val.as_str().ok_or_else(|| format!("the value of `{k}` must be text"))?)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(s) => {
                        let (k, val) = s.split_once('=').ok_or_else(|| format!("`{s}` is not NAME=value"))?;
                        push(k, val)?;
                    }
                    Value::Array(pair) => match pair.as_slice() {
                        [Value::String(k), Value::String(val)] => push(k, val)?,
                        _ => return Err("environment pairs must be [name, value]".into()),
                    },
                    other => return Err(format!("bad environment entry {other}")),
                }
            }
        }
        other => return Err(format!("bad environment {other}")),
    }
    Ok(out)
}

/// `A=1;B=2` (the Edit Configurations field).
pub fn env_text(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";")
}

/// Largest `.env` file read, and most variables taken from one.
const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;
const MAX_ENV_FILE_VARS: usize = 2000;

/// The variables of a `.env` file: `NAME=value` lines, an optional `export ` prefix, `#` comments
/// (whole-line, or after unquoted values), `'…'` literal and `"…"` with `\n \r \t \" \\`
/// escapes. Lines that are not assignments are skipped; a later assignment wins.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, rest)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty()
            || key.starts_with('#')
            || !key.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            continue;
        }
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some('\'') => rest.get(1..).and_then(|r| r.split_once('\'')).map(|(v, _)| v.to_string()),
            Some('"') => {
                let mut v = String::new();
                let mut chars = rest.get(1..).unwrap_or("").chars();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some('n') => v.push('\n'),
                            Some('r') => v.push('\r'),
                            Some('t') => v.push('\t'),
                            Some(c @ ('"' | '\\' | '$')) => v.push(c),
                            Some(c) => {
                                v.push('\\');
                                v.push(c);
                            }
                            None => break,
                        },
                        c => v.push(c),
                    }
                }
                closed.then_some(v)
            }
            _ => {
                let end = rest.find(" #").unwrap_or(rest.len());
                Some(rest.get(..end).unwrap_or(rest).trim_end().to_string())
            }
        };
        let Some(value) = value else { continue };
        if value.contains('\0') {
            continue;
        }
        out.retain(|(k, _)| k != key);
        out.push((key.to_string(), value));
        if out.len() >= MAX_ENV_FILE_VARS {
            break;
        }
    }
    out
}

impl UserRunConfig {
    /// The working directory: `working_dir` resolved against `project`, else `default`.
    pub fn cwd(&self, project: &Path, default: &Path) -> PathBuf {
        let w = self.working_dir.trim();
        if w.is_empty() {
            return default.to_path_buf();
        }
        let p = PathBuf::from(w);
        if p.is_absolute() { p } else { project.join(p) }
    }

    /// The `.env` file, resolved against `project` (`None` when the configuration has none).
    pub fn env_file_path(&self, project: &Path) -> Option<PathBuf> {
        let f = self.env_file.trim();
        if f.is_empty() {
            return None;
        }
        let p = PathBuf::from(f);
        Some(if p.is_absolute() { p } else { project.join(p) })
    }

    /// Read the `.env` file: `Ok(vec![])` when there is none configured.
    pub fn read_env_file(&self, project: &Path) -> Result<Vec<(String, String)>, String> {
        use std::io::Read;
        let Some(path) = self.env_file_path(project) else { return Ok(Vec::new()) };
        let name = self.name.trim();
        let file = std::fs::File::open(&path).map_err(|e| format!("`{name}`: cannot read {}: {e}", path.display()))?;
        let mut text = String::new();
        file.take(MAX_ENV_FILE_BYTES)
            .read_to_string(&mut text)
            .map_err(|e| format!("`{name}`: cannot read {}: {e}", path.display()))?;
        Ok(parse_env_file(&text))
    }

    /// The environment to run with: the `.env` file's variables, overridden by `env`. A `.env`
    /// file that cannot be read adds nothing here (`validate` and `check_env_file` report it).
    pub fn effective_env(&self, project: &Path) -> Vec<(String, String)> {
        let mut env = self.read_env_file(project).unwrap_or_default();
        for (k, v) in &self.env {
            env.retain(|(o, _)| o != k);
            env.push((k.clone(), v.clone()));
        }
        env
    }

    /// Check the configuration (name, command, working directory).
    pub fn validate(&self, project: &Path) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 200 {
            return Err("a run configuration needs a name of 1–200 characters".into());
        }
        if self.command.trim().is_empty() {
            return Err(format!("`{name}`: the command is empty"));
        }
        let cwd = self.cwd(project, project);
        if !self.working_dir.trim().is_empty() && !cwd.is_dir() {
            return Err(format!("`{name}`: working directory {} does not exist", cwd.display()));
        }
        if let Some(path) = self.env_file_path(project)
            && !path.is_file()
        {
            return Err(format!("`{name}`: environment file {} does not exist", path.display()));
        }
        self.to_run_config(project, project).map(|_| ())
    }

    /// The runnable configuration; Cargo runs default to `cargo_root`.
    pub fn to_run_config(&self, project: &Path, cargo_root: &Path) -> Result<RunConfig, String> {
        let mut config = self.to_local_run_config(project, cargo_root)?;
        config.target = self.target.clone();
        Ok(config)
    }

    fn to_local_run_config(&self, project: &Path, cargo_root: &Path) -> Result<RunConfig, String> {
        let name = self.name.trim().to_string();
        let env = self.effective_env(project);
        let words = || split_words(&self.command).map_err(|e| format!("`{name}`: {e}"));
        let process = |program: String, args: Vec<String>| {
            let mut cmd = ProcessCommand::new(program, &[], self.cwd(project, project));
            cmd.args = args;
            cmd.env = env.clone();
            Ok(RunConfig::process(name.clone(), cmd))
        };
        match self.kind {
            RunKind::Cargo => {
                let mut inv =
                    parse_cargo(&self.command, &self.cwd(project, cargo_root)).map_err(|e| format!("`{name}`: {e}"))?;
                inv.env = env.clone();
                Ok(RunConfig::cargo(name.clone(), inv))
            }
            RunKind::Command => {
                let words = words()?;
                let (program, args) = words.split_first().ok_or_else(|| format!("`{name}`: the command is empty"))?;
                process(program.clone(), args.to_vec())
            }
            RunKind::Python => {
                let args = words()?;
                if args.is_empty() {
                    return Err(format!("`{name}`: the command is empty"));
                }
                process(crate::run_configs::python_in(project, &self.cwd(project, project)), args)
            }
            RunKind::Zig => {
                let args = words()?;
                if args.is_empty() {
                    return Err(format!("`{name}`: the command is empty"));
                }
                process("zig".into(), args)
            }
            RunKind::Npm => {
                let args = words()?;
                if args.is_empty() {
                    return Err(format!("`{name}`: the command is empty"));
                }
                process(crate::run_configs::package_manager_in(project, &self.cwd(project, project)).into(), args)
            }
            RunKind::Django | RunKind::FastApi | RunKind::Flask => {
                let args = words()?;
                if args.is_empty() {
                    return Err(format!("`{name}`: the command is empty"));
                }
                let (program, args) =
                    crate::python_project::framework_command(
                    self.kind,
                    crate::run_configs::python_in(project, &self.cwd(project, project)),
                    args,
                );
                process(program, args)
            }
            RunKind::Dockerfile => {
                let cmd = crate::docker::run::dockerfile_command(&name, &words()?, project, &self.cwd(project, project), &env)
                    .map_err(|e| format!("`{name}`: {e}"))?;
                Ok(RunConfig::process(name, cmd))
            }
            RunKind::DockerCompose => {
                let cmd = crate::docker::run::compose_command(&words()?, project, &self.cwd(project, project), &env);
                Ok(RunConfig::process(name, cmd))
            }
            RunKind::Cpp => {
                let words = words()?;
                let (build, args) = match words.iter().position(|w| w == "--") {
                    Some(i) => (words.get(..i).unwrap_or_default(), words.get(i + 1..).unwrap_or_default()),
                    None => (words.as_slice(), &[][..]),
                };
                let sources: Vec<&String> = build.iter().filter(|w| !w.starts_with('-')).collect();
                if sources.is_empty() {
                    return Err(format!("`{name}`: no C/C++ source file in the command"));
                }
                let cxx = sources.iter().any(|s| {
                    let ext = Path::new(s.as_str()).extension().and_then(|e| e.to_str()).unwrap_or("");
                    matches!(ext, "cpp" | "cc" | "cxx" | "C" | "c++" | "cp")
                });
                let dir = project.join(".codecraft").join("build");
                let exe = dir.join(build_file_name(&name));
                let mut compile: Vec<String> = vec![(if cxx { "c++" } else { "cc" }).into()];
                compile.extend(build.iter().cloned());
                compile.extend(["-o".into(), exe.display().to_string()]);
                let mut run = vec![exe.display().to_string()];
                run.extend(args.iter().cloned());
                let script = format!(
                    "mkdir -p {} && {} && {}",
                    codecraft_cargo::shell_quote(&dir.display().to_string()),
                    join_words(&compile),
                    join_words(&run)
                );
                let mut cmd = ProcessCommand::shell(script, self.cwd(project, project));
                cmd.env = env;
                Ok(RunConfig::process(name, cmd))
            }
        }
    }

    /// A user copy of a derived configuration.
    pub fn from_run_config(c: &RunConfig, project: &Path) -> Self {
        let rel = |cwd: &Path| if cwd == project { String::new() } else { cwd.display().to_string() };
        if let Some(cmd) = &c.command {
            let words: Vec<&str> =
                std::iter::once(cmd.program.as_str()).chain(cmd.args.iter().map(String::as_str)).collect();
            return Self {
                name: c.name.clone(),
                kind: RunKind::Command,
                command: join_words(&words),
                env: cmd.env.clone(),
                env_file: String::new(),
                working_dir: rel(&cmd.cwd),
                target: c.target.clone(),
            };
        }
        let mut quiet = c.invocation.clone();
        quiet.json_messages = false;
        Self {
            name: c.name.clone(),
            kind: RunKind::Cargo,
            command: join_words(&quiet.args()),
            env: c.invocation.env.clone(),
            env_file: String::new(),
            working_dir: rel(&c.invocation.cwd),
            target: c.target.clone(),
        }
    }
}

/// A configuration name as a file name (letters, digits, `-_.`; the rest becomes `_`).
fn build_file_name(name: &str) -> String {
    let s: String = name
        .chars()
        .take(60)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect();
    if s.is_empty() { "program".into() } else { s }
}

/// `base`, or `base (2)`, `base (3)`… not used by `taken`.
pub fn unique_name(base: &str, taken: &[UserRunConfig]) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "Unnamed" } else { base };
    if !taken.iter().any(|c| c.name == base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base} ({n})")).find(|n| !taken.iter().any(|c| c.name == *n)).unwrap_or_else(|| base.into())
}

/// Names must be unique among user configurations.
pub fn check_unique(list: &[UserRunConfig]) -> Result<(), String> {
    for (i, c) in list.iter().enumerate() {
        if list.iter().skip(i + 1).any(|o| o.name.trim() == c.name.trim()) {
            return Err(format!("there are two run configurations named `{}`", c.name.trim()));
        }
    }
    Ok(())
}

/// Before a run: the user configuration called `name` (if any) must have a readable `.env` file,
/// and the run configurations are rebuilt so the file's current contents are used.
pub(crate) fn prepare_run(s: &mut Session, name: &str) -> Result<(), String> {
    let Some(project) = s.run_store.root.clone() else { return Ok(()) };
    let checked = s.run_store.user.iter().find(|c| c.name.trim() == name).map(|c| c.read_env_file(&project));
    rebuild(s);
    checked.transpose().map(|_| ())
}

// ---------------------------------------------------------------- store and dialog

/// Which configuration the Edit Configurations dialog shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// A derived (read-only) configuration rather than a user one.
    pub derived: bool,
    pub index: usize,
}

/// Edit Configurations dialog state: edits go to `draft` until Apply/OK.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfigDialog {
    pub draft: Vec<UserRunConfig>,
    pub selected: Option<Selection>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RunConfigStore {
    root: Option<PathBuf>,
    /// Saved user configurations.
    pub user: Vec<UserRunConfig>,
    /// Configurations derived from the project (Cargo targets, build files).
    pub derived: Vec<RunConfig>,
    /// The last load/save error (a corrupt file is moved aside on the next save).
    pub error: Option<String>,
    corrupt: bool,
    pub dialog: Option<RunConfigDialog>,
    /// Generated configurations (Edit Configurations › Generate, and on project open).
    pub generate: crate::run_config_gen::GenState,
}

impl RunConfigStore {
    /// The configuration file of the open project.
    pub fn path(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join(FILE))
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    fn load(&mut self, root: &Path) {
        self.root = Some(root.to_path_buf());
        self.user.clear();
        self.error = None;
        self.corrupt = false;
        let path = root.join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.error = Some(format!("cannot read {}: {e}", path.display()));
                return;
            }
        };
        match serde_json::from_str::<ConfigFile>(&text) {
            Ok(f) => {
                self.user =
                    f.configurations.into_iter().filter(|c| !c.name.trim().is_empty()).take(MAX_CONFIGS).collect()
            }
            Err(e) => {
                self.error = Some(format!("ignoring corrupt {}: {e}", path.display()));
                self.corrupt = true;
            }
        }
    }

    /// Write `user` to the project's file.
    pub fn save(&mut self) -> Result<PathBuf, String> {
        let path = self.path().ok_or("no project is open")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        if self.corrupt {
            let mut bad = path.clone().into_os_string();
            bad.push(".bad");
            let _ = std::fs::rename(&path, PathBuf::from(bad));
            self.corrupt = false;
        }
        let file = ConfigFile { version: 1, configurations: self.user.clone() };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        codecraft_fs::write_text_file_atomic(&path, &text)
            .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
        self.error = None;
        Ok(path)
    }

    fn clear(&mut self) {
        // Whether generation on open is enabled outlives the project.
        let auto = self.generate.auto;
        *self = Self::default();
        self.generate.auto = auto;
    }
}

fn cargo_root(s: &Session) -> Option<PathBuf> {
    let p = s.ws.project.as_ref()?;
    Some(p.cargo.as_ref().map(|m| m.workspace_root.clone()).unwrap_or_else(|| p.root.clone()))
}

/// After the project (re)loads: remember the derived configurations, load the user ones.
pub(crate) fn project_loaded(s: &mut Session, root: &Path) {
    s.run_store.derived = s.run_configs.clone();
    s.run_store.load(root);
    if let Some(e) = s.run_store.error.clone() {
        s.notify(codecraft_workspace::NotificationLevel::Warning, "Run Configurations", e);
    }
    rebuild(s);
    crate::run_config_gen::project_loaded(s, root);
}

pub(crate) fn project_closed(s: &mut Session) {
    s.run_store.clear();
}

/// `Session::run_configs` = user configurations (those that are valid), then derived ones; the
/// selection follows the selected configuration's name.
pub(crate) fn rebuild(s: &mut Session) {
    let selected = s.run_configs.get(s.selected_run_config).map(|c| c.name.clone());
    let Some(project) = s.run_store.root.clone() else { return };
    let cargo = cargo_root(s).unwrap_or_else(|| project.clone());
    let mut list: Vec<RunConfig> =
        s.run_store.user.iter().filter_map(|u| u.to_run_config(&project, &cargo).ok()).collect();
    // A derived configuration that a user one already covers (same name or same command line,
    // as after Generate) is not listed twice.
    let derived: Vec<RunConfig> = s
        .run_store
        .derived
        .iter()
        .filter(|d| !list.iter().any(|u| u.name == d.name || u.command_line() == d.command_line()))
        .cloned()
        .collect();
    list.extend(derived);
    s.run_configs = list;
    s.selected_run_config = selected.and_then(|n| s.run_configs.iter().position(|c| c.name == n)).unwrap_or(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_words_handles_quotes() {
        assert_eq!(split_words("a 'b c' \"d \\\"e\\\"\" f\\ g").unwrap(), vec!["a", "b c", "d \"e\"", "f g"]);
        assert_eq!(split_words("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_words("''").unwrap(), vec![""]);
        assert!(split_words("'open").is_err());
        assert!(split_words("\"open").is_err());
        let words = ["sh", "-c", "a && b 'q'", ""];
        assert_eq!(split_words(&join_words(&words)).unwrap(), words);
    }

    #[test]
    fn cargo_commands_parse_and_round_trip() {
        let cwd = Path::new("/w");
        let inv = parse_cargo("run --package app --bin app --release -F a,b --locked -- --port 8080", cwd).unwrap();
        assert_eq!(inv.command, CargoCommand::Run);
        assert_eq!(inv.package.as_deref(), Some("app"));
        assert_eq!(inv.target, Some(TargetSelector::Bin("app".into())));
        assert!(inv.release);
        assert_eq!(inv.features, vec!["a", "b"]);
        assert_eq!(inv.extra_args, vec!["--locked"]);
        assert_eq!(inv.program_args, vec!["--port", "8080"]);
        assert_eq!(parse_cargo("cargo test --package=x", cwd).unwrap().package.as_deref(), Some("x"));
        assert!(parse_cargo("", cwd).is_err());
        assert!(parse_cargo("nextest run", cwd).is_err());
        assert!(parse_cargo("run --bin", cwd).is_err());
        // A derived configuration's command parses back to the same invocation.
        let mut derived = CargoInvocation::new(CargoCommand::Test, cwd);
        derived.package = Some("p".into());
        derived.target = Some(TargetSelector::Lib);
        derived.test_filter = Some("tests::it works".into());
        derived.test_exact = true;
        let user = UserRunConfig::from_run_config(&RunConfig::cargo("T", derived.clone()), cwd);
        assert_eq!(user.kind, RunKind::Cargo);
        assert_eq!(user.working_dir, "");
        let back = user.to_run_config(cwd, cwd).unwrap();
        assert_eq!(back.command_line(), RunConfig::cargo("T", derived).command_line());
    }

    #[test]
    fn command_configs_round_trip() {
        let cwd = Path::new("/w");
        let derived = RunConfig::process("CTest", ProcessCommand::shell("cmake --build b && ctest", "/w/sub"));
        let user = UserRunConfig::from_run_config(&derived, cwd);
        assert_eq!((user.kind, user.working_dir.as_str()), (RunKind::Command, "/w/sub"));
        let back = user.to_run_config(cwd, cwd).unwrap();
        assert_eq!(back.command, derived.command);
    }

    #[test]
    fn env_parsing() {
        let want = vec![("A".to_string(), "1".to_string()), ("B".to_string(), "x=y".to_string())];
        assert_eq!(parse_env(&json!("A=1; B=x=y")).unwrap(), want);
        assert_eq!(parse_env(&json!({ "A": "1", "B": "x=y" })).unwrap(), want);
        assert_eq!(parse_env(&json!([["A", "1"], "B=x=y"])).unwrap(), want);
        assert_eq!(env_text(&want), "A=1;B=x=y");
        for bad in [json!("A"), json!("=1"), json!({ "A": 1 }), json!([1]), json!(5)] {
            assert!(parse_env(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_files_parse_like_dotenv() {
        let text = "\u{feff}# c\nA=1\nexport B = two words # note\nC=\"x # y\\n\\\"q\\\"\"\nD='$raw # kept'\nnot an assignment\n=bad\nE=\nA=override\nBAD KEY=1\nUNCLOSED=\"abc\n";
        let got: std::collections::HashMap<_, _> = parse_env_file(text).into_iter().collect();
        assert_eq!(got.get("A").map(String::as_str), Some("override"));
        assert_eq!(got.get("B").map(String::as_str), Some("two words"));
        assert_eq!(got.get("C").map(String::as_str), Some("x # y\n\"q\""));
        assert_eq!(got.get("D").map(String::as_str), Some("$raw # kept"));
        assert_eq!(got.get("E").map(String::as_str), Some(""));
        assert_eq!(got.len(), 5, "{got:?}");
        assert!(parse_env_file("").is_empty());
        assert!(parse_env_file("A=\"\u{0}\"\nB=\u{0}").is_empty());
    }

    #[test]
    fn env_file_merges_under_explicit_env() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=file\nB=file\n").unwrap();
        let mut c = UserRunConfig {
            name: "X".into(),
            kind: RunKind::Command,
            command: "true".into(),
            env: vec![("B".into(), "explicit".into())],
            env_file: ".env".into(),
            ..Default::default()
        };
        assert_eq!(
            c.effective_env(dir.path()),
            vec![("A".to_string(), "file".to_string()), ("B".to_string(), "explicit".to_string())]
        );
        c.validate(dir.path()).unwrap();
        c.env_file = "missing.env".into();
        assert!(c.validate(dir.path()).unwrap_err().contains("does not exist"));
        assert!(c.read_env_file(dir.path()).is_err());
        // Running with the file gone still builds the configuration, with the explicit env only.
        assert_eq!(c.to_run_config(dir.path(), dir.path()).unwrap().command.unwrap().env.len(), 1);
    }

    #[test]
    fn kinds_parse_with_aliases() {
        for (s, k) in
            [("c++", RunKind::Cpp), ("Python3", RunKind::Python), ("zig", RunKind::Zig), ("node", RunKind::Npm)]
        {
            assert_eq!(RunKind::parse(s).unwrap(), k);
        }
        for k in RunKind::ALL {
            assert_eq!(RunKind::parse(k.label()).ok().map(|p| p.label()).or(Some(k.label())), Some(k.label()));
            assert!(!k.default_command().is_empty() && !k.hint().is_empty());
        }
    }

    #[test]
    fn validation_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut c = UserRunConfig { name: "Serve".into(), command: "run".into(), ..Default::default() };
        c.validate(root).unwrap();
        c.working_dir = "missing".into();
        assert!(c.validate(root).unwrap_err().contains("does not exist"));
        std::fs::create_dir(root.join("sub")).unwrap();
        c.working_dir = "sub".into();
        c.validate(root).unwrap();
        assert_eq!(c.to_run_config(root, root).unwrap().invocation.cwd, root.join("sub"));
        c.name = " ".into();
        assert!(c.validate(root).is_err());
        let list = vec![
            UserRunConfig { name: "A".into(), ..Default::default() },
            UserRunConfig { name: "A (2)".into(), ..Default::default() },
        ];
        assert_eq!(unique_name("A", &list), "A (3)");
        assert_eq!(unique_name("", &list), "Unnamed");
        assert!(check_unique(&[list[0].clone(), list[0].clone()]).is_err());
    }

    #[test]
    fn store_round_trips_and_survives_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RunConfigStore::default();
        store.load(dir.path());
        assert!(store.user.is_empty() && store.error.is_none());
        store.user.push(UserRunConfig {
            name: "Serve".into(),
            command: "run -- --port 1".into(),
            env: vec![("RUST_LOG".into(), "debug".into())],
            ..Default::default()
        });
        let path = store.save().unwrap();
        assert_eq!(path, dir.path().join(FILE));
        let mut again = RunConfigStore::default();
        again.load(dir.path());
        assert_eq!(again.user, store.user);

        std::fs::write(&path, "{ not json").unwrap();
        again.load(dir.path());
        assert!(again.error.as_deref().is_some_and(|e| e.contains("corrupt")));
        assert!(again.user.is_empty());
        again.user.push(UserRunConfig { name: "X".into(), command: "build".into(), ..Default::default() });
        again.save().unwrap();
        assert!(dir.path().join(".codecraft/run-configurations.json.bad").is_file());
    }
}
