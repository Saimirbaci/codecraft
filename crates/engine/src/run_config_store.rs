//! User run configurations (Run › Edit Configurations…), persisted per project in
//! `<project>/.codecraft/run-configurations.json`, next to the configurations derived from Cargo
//! targets and the other build systems (`runs::derive_configs`, `run_configs::derive_polyglot`).
//!
//! A user configuration is either a Cargo command (`run --package app --bin app -- --verbose`,
//! like RustRover's Cargo configuration's Command field) or any other command line (`make test`,
//! `sh -c 'a && b'`), with environment variables and a working directory (empty = the project).
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
}

impl RunKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cargo" => Ok(Self::Cargo),
            "command" | "shell" | "process" => Ok(Self::Command),
            other => Err(format!("unknown configuration type `{other}` (cargo or command)")),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo",
            Self::Command => "Command",
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
    /// Empty = the project root (the Cargo workspace root for Cargo); relative = from the project.
    pub working_dir: String,
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
        self.to_run_config(project, project).map(|_| ())
    }

    /// The runnable configuration; Cargo runs default to `cargo_root`.
    pub fn to_run_config(&self, project: &Path, cargo_root: &Path) -> Result<RunConfig, String> {
        let name = self.name.trim().to_string();
        match self.kind {
            RunKind::Cargo => {
                let mut inv =
                    parse_cargo(&self.command, &self.cwd(project, cargo_root)).map_err(|e| format!("`{name}`: {e}"))?;
                inv.env = self.env.clone();
                Ok(RunConfig::cargo(name, inv))
            }
            RunKind::Command => {
                let words = split_words(&self.command).map_err(|e| format!("`{name}`: {e}"))?;
                let (program, args) = words.split_first().ok_or_else(|| format!("`{name}`: the command is empty"))?;
                let mut cmd = ProcessCommand::new(program.clone(), &[], self.cwd(project, project));
                cmd.args = args.to_vec();
                cmd.env = self.env.clone();
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
                working_dir: rel(&cmd.cwd),
            };
        }
        let mut quiet = c.invocation.clone();
        quiet.json_messages = false;
        Self {
            name: c.name.clone(),
            kind: RunKind::Cargo,
            command: join_words(&quiet.args()),
            env: c.invocation.env.clone(),
            working_dir: rel(&c.invocation.cwd),
        }
    }
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
        *self = Self::default();
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
    list.extend(s.run_store.derived.iter().cloned());
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
