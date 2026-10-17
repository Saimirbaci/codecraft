//! Remote Python interpreters (M16 track RT): Settings › Python › Remote interpreters lists the
//! interpreters on an SSH host or in a Docker image/container/Compose service (`python3` on its
//! `PATH` and virtualenvs: the project's `.venv`/`venv`, `~/.virtualenvs`, pyenv, conda, `/opt`),
//! the packages of one, and sets the one a run configuration uses on its target (PyCharm's SSH
//! and Docker interpreters).
//!
//! Detection and package listing run a small script on the target on a worker thread; the
//! results arrive through [`pump`].

use std::collections::BTreeMap;
use std::time::Duration;

use crossbeam_channel::Receiver;
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_remote::target_script::{self as script, RemoteInterpreter, RemotePackage};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, spec};
use crate::params::opt_str;
use crate::run_targets::RunTarget;
use crate::target_exec::{Captured, Ctx, resolve_ref, target_key};
use crate::{CommandError, Session};

/// How long a probe may take (an SSH connection plus a few interpreters).
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// The interpreters found on one target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterpreterList {
    /// `SSH: web`, `Docker: python:3.12`, …
    pub label: String,
    pub target: Option<RunTarget>,
    pub loading: bool,
    pub error: Option<String>,
    pub interpreters: Vec<RemoteInterpreter>,
    /// The project folder on the target (absolute), when the probe reported it.
    pub root: Option<String>,
}

/// The packages of one interpreter on a target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageList {
    pub loading: bool,
    pub error: Option<String>,
    pub packages: Vec<RemotePackage>,
}

enum JobKind {
    Detect,
    Packages(String),
}

struct Job {
    key: String,
    kind: JobKind,
    rx: Receiver<Result<Captured, String>>,
}

/// Remote interpreters of a session.
#[derive(Default)]
pub struct RemotePythonState {
    /// By target key ([`target_key`]).
    pub lists: BTreeMap<String, InterpreterList>,
    /// By `<target key>\n<interpreter>`.
    pub packages: BTreeMap<String, PackageList>,
    /// The target Settings › Python shows (a key of `lists`).
    pub selected: Option<String>,
    /// The interpreter whose packages it shows.
    pub selected_python: Option<String>,
    jobs: Vec<Job>,
}

impl RemotePythonState {
    pub fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// The selected target's list.
    pub fn selected_list(&self) -> Option<&InterpreterList> {
        self.selected.as_ref().and_then(|k| self.lists.get(k))
    }

    /// The selected interpreter's packages.
    pub fn selected_packages(&self) -> Option<&PackageList> {
        let key = self.selected.as_ref()?;
        let py = self.selected_python.as_ref()?;
        self.packages.get(&format!("{key}\n{py}"))
    }
}

fn spawn_probe(cmd: crate::runs::ProcessCommand) -> Receiver<Result<Captured, String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("remote-python-probe".into())
        .spawn(move || {
            let _ = tx.send(crate::target_exec::capture(&cmd, PROBE_TIMEOUT));
        });
    if let Err(e) = spawned {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(Err(format!("cannot start a worker thread: {e}")));
        return rx;
    }
    rx
}

fn target_param(s: &mut Session, p: &Value) -> Result<RunTarget, CommandError> {
    let v = p.get("target").filter(|v| !v.is_null()).ok_or_else(|| {
        CommandError::BadParams("`target` is required (an SSH server, a saved target, docker:<image>, container:<name>, …)".into())
    })?;
    let t = resolve_ref(s, v)?;
    if t.is_local() {
        return Err(CommandError::BadParams(
            "the local machine's interpreters are in Settings › Python (python.interpreter.*)".into(),
        ));
    }
    Ok(t)
}

/// `python.remoteInterpreters.detect {target}`.
fn detect(s: &mut Session, p: &Value) -> CmdResult {
    let target = target_param(s, p)?;
    let ctx = Ctx::resolve(s, &target)?;
    let cmd = ctx.probe(&script::detect_pythons(&ctx.remote_root)).map_err(CommandError::Failed)?;
    let key = target_key(&target);
    let st = &mut s.targets.python;
    st.jobs.retain(|j| !(j.key == key && matches!(j.kind, JobKind::Detect)));
    st.jobs.push(Job { key: key.clone(), kind: JobKind::Detect, rx: spawn_probe(cmd) });
    let entry = st.lists.entry(key.clone()).or_default();
    entry.label = ctx.label.clone();
    entry.target = Some(target);
    entry.loading = true;
    entry.error = None;
    st.selected = Some(key.clone());
    s.status = format!("Detecting Python interpreters on {}…", ctx.label);
    Ok(json!({ "key": key, "target": ctx.label, "pending": true }))
}

/// `python.remoteInterpreters.packages {target, python}`.
fn packages(s: &mut Session, p: &Value) -> CmdResult {
    let target = target_param(s, p)?;
    let python = match opt_str(p, "python").map(str::trim).filter(|x| !x.is_empty()) {
        Some(py) => py.to_string(),
        None => crate::target_exec::target_python_of(&target).to_string(),
    };
    let python = if python.is_empty() { "python3".to_string() } else { python };
    if python.chars().any(|c| c.is_control()) {
        return Err(CommandError::BadParams("bad interpreter path".into()));
    }
    let ctx = Ctx::resolve(s, &target)?;
    let cmd = ctx.probe(&script::list_packages(&python)).map_err(CommandError::Failed)?;
    let key = target_key(&target);
    let pkey = format!("{key}\n{python}");
    let st = &mut s.targets.python;
    st.jobs.retain(|j| !(j.key == key && matches!(&j.kind, JobKind::Packages(x) if *x == python)));
    st.jobs.push(Job { key: key.clone(), kind: JobKind::Packages(python.clone()), rx: spawn_probe(cmd) });
    let entry = st.packages.entry(pkey).or_default();
    entry.loading = true;
    entry.error = None;
    let list = st.lists.entry(key.clone()).or_default();
    if list.label.is_empty() {
        list.label = ctx.label.clone();
        list.target = Some(target);
    }
    st.selected = Some(key.clone());
    st.selected_python = Some(python.clone());
    Ok(json!({ "key": key, "python": python, "target": ctx.label, "pending": true }))
}

/// `python.remoteInterpreters {target?}`: what is known (and what is still loading).
fn list(s: &mut Session, p: &Value) -> CmdResult {
    let only = match p.get("target").filter(|v| !v.is_null()) {
        Some(v) => Some(target_key(&resolve_ref(s, &v.clone())?)),
        None => None,
    };
    let lists: Vec<(String, InterpreterList)> = s
        .targets
        .python
        .lists
        .iter()
        .filter(|(k, _)| only.as_ref().is_none_or(|o| o == *k))
        .map(|(k, l)| (k.clone(), l.clone()))
        .collect();
    let st = &s.targets.python;
    let out: Vec<Value> = lists
        .into_iter()
        .map(|(key, l)| {
            let packages: BTreeMap<&str, &PackageList> = st
                .packages
                .iter()
                .filter_map(|(k, v)| k.strip_prefix(&format!("{key}\n")).map(|py| (py, v)))
                .collect();
            json!({
                "key": key,
                "target": l.label,
                "loading": l.loading,
                "error": l.error,
                "root": l.root,
                "interpreters": l.interpreters,
                "packages": packages,
            })
        })
        .collect();
    Ok(json!({ "targets": out, "busy": st.busy() }))
}

/// `python.remoteInterpreters.use {config, python, target?}`: the interpreter a user run
/// configuration uses on its target (and the target itself with `target`).
fn use_interpreter(s: &mut Session, p: &Value) -> CmdResult {
    let index = match p.get("config") {
        Some(Value::Number(n)) => n.as_u64().and_then(|n| usize::try_from(n).ok()),
        Some(Value::String(name)) => s.run_store.user.iter().position(|c| c.name.trim() == name.trim()),
        _ => None,
    }
    .ok_or_else(|| CommandError::BadParams("`config` is a user run configuration's index or name".into()))?;
    let python = opt_str(p, "python").map(str::trim).unwrap_or_default().to_string();
    if python.chars().any(|c| c.is_control()) {
        return Err(CommandError::BadParams("bad interpreter path".into()));
    }
    let target = match p.get("target").filter(|v| !v.is_null()) {
        Some(_) => Some(target_param(s, p)?),
        None => None,
    };
    let c = s
        .run_store
        .user
        .get_mut(index)
        .ok_or_else(|| CommandError::BadParams(format!("no user run configuration {index}")))?;
    let previous = c.target.clone();
    if let Some(t) = target {
        c.target = t;
    }
    if c.target.is_local() {
        c.target = previous;
        return Err(CommandError::Failed(format!(
            "`{}` runs on this machine: give a `target` (or assign one in Run › Manage Targets…) first",
            c.name
        )));
    }
    crate::target_exec::set_target_python(&mut c.target, &python).map_err(CommandError::Failed)?;
    let (name, label) = (c.name.clone(), c.target.label());
    if let Err(e) = s.run_store.save() {
        if let Some(c) = s.run_store.user.get_mut(index) {
            c.target = previous;
        }
        return Err(CommandError::Failed(e));
    }
    crate::run_config_store::rebuild(s);
    s.status = format!("{name}: Python {} on {label}", if python.is_empty() { "by name" } else { python.as_str() });
    Ok(json!({ "config": name, "target": label, "python": python }))
}

/// Drain finished probes.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let mut i = 0;
    while let Some(job) = s.targets.python.jobs.get(i) {
        let result = match job.rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                i += 1;
                continue;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the probe stopped".into()),
        };
        let job = s.targets.python.jobs.remove(i);
        changed = true;
        let st = &mut s.targets.python;
        match job.kind {
            JobKind::Detect => {
                let entry = st.lists.entry(job.key.clone()).or_default();
                entry.loading = false;
                match result {
                    Ok(out) if out.success => {
                        entry.interpreters = script::parse_interpreters(&out.stdout);
                        entry.root = out.stdout.lines().find_map(|l| script::marker(l, script::ROOT_MARK)).map(str::to_string).filter(|r| !r.is_empty());
                        entry.error = entry.interpreters.is_empty().then(|| {
                            format!(
                                "no Python interpreter found on {}: {}",
                                entry.label,
                                script::python_missing_message("python3", &entry.label)
                            )
                        });
                        s.status = format!("{}: {} Python interpreter(s)", entry.label, entry.interpreters.len());
                    }
                    Ok(out) => entry.error = Some(out.error()),
                    Err(e) => entry.error = Some(e),
                }
            }
            JobKind::Packages(python) => {
                let entry = st.packages.entry(format!("{}\n{python}", job.key)).or_default();
                entry.loading = false;
                match result {
                    Ok(out) if out.success => match script::parse_packages(&out.stdout) {
                        Some(p) => entry.packages = p,
                        None => entry.error = Some(format!("`{python}` printed no package list")),
                    },
                    Ok(out) => entry.error = Some(out.error()),
                    Err(e) => entry.error = Some(e),
                }
            }
        }
    }
    changed
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "python.remoteInterpreters.detect",
            "Detect Remote Python Interpreters",
            None,
            "{target: string|object} — find python3 and virtualenvs on an SSH host or in a Docker image/container/Compose \
             service (worker thread; read the result with python.remoteInterpreters)",
            Context::Global,
            has_project,
            detect,
        ),
        spec(
            "python.remoteInterpreters.packages",
            "List Remote Python Packages",
            None,
            "{target, python?: string} — the installed packages of an interpreter on the target (default: the target's \
             interpreter, else python3)",
            Context::Global,
            has_project,
            packages,
        ),
        spec(
            "python.remoteInterpreters",
            "Remote Python Interpreters",
            None,
            "{target?} — the interpreters (path, version, venv) and packages found on targets so far, and whether a probe \
             is running",
            Context::Global,
            always,
            list,
        ),
        spec(
            "python.remoteInterpreters.use",
            "Use Remote Python Interpreter",
            None,
            "{config: int|string (user run configuration), python: string (empty = by name), target?} — the interpreter \
             the configuration uses on its target (and, with `target`, the target)",
            Context::Global,
            has_project,
            use_interpreter,
        ),
        spec(
            "python.remoteInterpreters.select",
            "Select Remote Python Interpreter",
            None,
            "{key?: string, python?: string} — the target and interpreter Settings › Python shows",
            Context::Global,
            always,
            |s, p| {
                if let Some(k) = opt_str(p, "key") {
                    if !s.targets.python.lists.contains_key(k) {
                        return Err(CommandError::BadParams(format!("no remote interpreters listed for `{k}`")));
                    }
                    s.targets.python.selected = Some(k.to_string());
                }
                if let Some(py) = opt_str(p, "python") {
                    s.targets.python.selected_python = Some(py.to_string()).filter(|x| !x.is_empty());
                }
                Ok(json!({ "key": s.targets.python.selected, "python": s.targets.python.selected_python }))
            },
        ),
    ]
}
