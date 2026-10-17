//! Before-launch tasks of run configurations (M13 track RC): RustRover's "Before launch" list in
//! Run/Debug Configurations. Run (`run.run`) and Debug (`run.debug`) of a configuration with tasks
//! first run each task in order, each in its own Run/Build tab, waiting for it to finish: Build
//! (the configuration's own build: `cargo build` of its package and target, the CMake target;
//! else the project's build), Run Another Configuration (with that configuration's own tasks
//! first) and Run External Tool (a command line). A task that fails or is stopped stops the
//! launch with a message naming it; when all succeed the configuration starts (or is debugged).
//!
//! Nothing blocks: the tasks are processes the Run tool window already runs; [`pump`] watches
//! their tabs.

use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation, TargetSelector};
use codecraft_workspace::{NotificationLevel, RunStatus, RunTabId};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::run_config_store::{BeforeLaunchTask, RunKind, split_words};
use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

/// Nesting of Run Another Configuration (a → b → c …) allowed before it counts as a loop.
const MAX_DEPTH: usize = 8;
/// Steps in one launch (after expanding other configurations' tasks).
const MAX_STEPS: usize = 64;

/// What starts once the tasks succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Run,
    Debug,
}

/// One expanded task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Step {
    /// Build `owner` (the configuration whose task it is).
    Build { owner: String },
    /// Run the configuration `name` and wait for it to end.
    Run { name: String },
    /// A command line, in `working_dir` (empty = the project).
    External { owner: String, command: String, working_dir: String },
}

impl Step {
    pub fn label(&self) -> String {
        match self {
            Self::Build { owner } => format!("Build '{owner}'"),
            Self::Run { name } => format!("Run '{name}'"),
            Self::External { command, .. } => format!("Run external tool '{}'", command.trim()),
        }
    }
}

/// A launch waiting for its before-launch tasks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLaunch {
    /// The configuration that starts at the end.
    pub config: String,
    pub mode: LaunchMode,
    /// Steps not started yet.
    pub steps: VecDeque<Step>,
    /// The running step and its tab.
    pub current: Option<(Step, RunTabId)>,
    /// Finished steps (labels).
    pub done: Vec<String>,
    /// The command's `target` param: the configuration starts there at the end (M16 RT); the
    /// tasks themselves run here, as for a configuration's own target.
    pub target: Option<crate::run_targets::RunTarget>,
}

#[derive(Debug, Default)]
pub struct LaunchState {
    pub pending: Option<PendingLaunch>,
    /// Why the last launch was stopped.
    pub last_error: Option<String>,
}

impl LaunchState {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
}

fn user<'a>(s: &'a Session, name: &str) -> Option<&'a crate::run_config_store::UserRunConfig> {
    s.run_store.user.iter().find(|c| c.name.trim() == name)
}

/// The steps for `name`'s tasks, other configurations' tasks expanded before them.
pub fn expand(s: &Session, name: &str) -> Result<Vec<Step>, String> {
    let mut out = Vec::new();
    expand_into(s, name, &mut vec![name.to_string()], &mut out)?;
    Ok(out)
}

fn expand_into(s: &Session, name: &str, stack: &mut Vec<String>, out: &mut Vec<Step>) -> Result<(), String> {
    let Some(c) = user(s, name) else { return Ok(()) };
    for task in c.before_launch.clone() {
        if out.len() >= MAX_STEPS {
            return Err(format!("`{name}`: more than {MAX_STEPS} before-launch tasks"));
        }
        match task {
            BeforeLaunchTask::Build => out.push(Step::Build { owner: name.to_string() }),
            BeforeLaunchTask::External { command, working_dir } => {
                out.push(Step::External { owner: name.to_string(), command, working_dir })
            }
            BeforeLaunchTask::RunConfiguration { name: other } => {
                let other = other.trim().to_string();
                if stack.contains(&other) {
                    return Err(format!(
                        "`{name}`: before-launch tasks run in a loop ({} → {other})",
                        stack.join(" → ")
                    ));
                }
                if !s.run_configs.iter().any(|c| c.name == other) {
                    return Err(format!("`{name}`: before launch: there is no run configuration named `{other}`"));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("`{name}`: before-launch configurations nest too deep"));
                }
                stack.push(other.clone());
                expand_into(s, &other, stack, out)?;
                stack.pop();
                out.push(Step::Run { name: other });
            }
        }
    }
    Ok(())
}

/// Called by Run/Debug of configuration `name`: `Ok(None)` when it has no tasks (start it now),
/// else the tasks start and the result describes the pending launch.
pub(crate) fn begin(s: &mut Session, name: &str, mode: LaunchMode) -> Result<Option<Value>, CommandError> {
    let steps = expand(s, name).map_err(CommandError::Failed)?;
    if steps.is_empty() {
        return Ok(None);
    }
    if let Some(p) = &s.launch.pending {
        return Err(CommandError::Failed(format!(
            "'{}' is still running its before-launch tasks (Stop cancels them)",
            p.config
        )));
    }
    let labels: Vec<String> = steps.iter().map(Step::label).collect();
    s.launch.last_error = None;
    s.launch.pending =
        Some(PendingLaunch {
            config: name.to_string(),
            mode,
            steps: steps.into(),
            current: None,
            done: Vec::new(),
            target: s.targets.scoped.clone(),
        });
    let scoped = s.targets.scoped.take();
    advance(s);
    s.targets.scoped = scoped;
    if let Some(e) = s.launch.last_error.clone().filter(|_| s.launch.pending.is_none()) {
        return Err(CommandError::Failed(e));
    }
    Ok(Some(json!({ "config": name, "beforeLaunch": labels, "pending": s.launch.pending.is_some() })))
}

/// Start the next step, or the configuration when none is left.
fn advance(s: &mut Session) {
    loop {
        let Some(p) = s.launch.pending.as_mut() else { return };
        let Some(step) = p.steps.pop_front() else {
            finish(s);
            return;
        };
        match start_step(s, &step) {
            Ok(Some(tab)) => {
                s.status = format!("Before launch: {}…", step.label());
                if let Some(p) = s.launch.pending.as_mut() {
                    p.current = Some((step, tab));
                }
                return;
            }
            Ok(None) => {
                if let Some(p) = s.launch.pending.as_mut() {
                    p.done.push(format!("{} (nothing to do)", step.label()));
                }
            }
            Err(e) => {
                fail(s, &format!("{}: {e}", step.label()));
                return;
            }
        }
    }
}

fn fail(s: &mut Session, why: &str) {
    let config = s.launch.pending.take().map(|p| p.config).unwrap_or_default();
    let message = format!("Before launch: {why}. '{config}' was not started.");
    s.status = message.clone();
    s.notify(NotificationLevel::Error, "Before launch", message.clone());
    s.launch.last_error = Some(message);
}

/// Every task succeeded: start the configuration (without its tasks again).
fn finish(s: &mut Session) {
    let Some(p) = s.launch.pending.take() else { return };
    let Some(index) = s.run_configs.iter().position(|c| c.name == p.config) else {
        let message = format!("Before launch: done, but the run configuration '{}' is gone", p.config);
        s.status = message.clone();
        s.launch.last_error = Some(message);
        return;
    };
    let params = json!({ "config": index, "beforeLaunch": false });
    let start = |s: &mut Session| match p.mode {
        LaunchMode::Run => crate::run_cmds::run_selected(s, &params),
        LaunchMode::Debug => crate::debug_cmds::debug_config(s, &params),
    };
    let started = match p.target.clone() {
        Some(t) => crate::target_exec::with_target(s, t, start),
        None => start(s),
    };
    if let Err(e) = started {
        let message = format!("'{}' could not start after its before-launch tasks: {e}", p.config);
        s.status = message.clone();
        s.notify(NotificationLevel::Error, "Before launch", message.clone());
        s.launch.last_error = Some(message);
    }
}

fn project_root(s: &Session) -> Result<PathBuf, String> {
    s.run_store
        .root()
        .map(std::path::Path::to_path_buf)
        .or_else(|| s.project_root())
        .ok_or_else(|| "no project is open".to_string())
}

/// The build of a Cargo configuration: `cargo build` of its package, target and features (test
/// configurations build their test targets).
pub fn build_invocation(inv: &CargoInvocation) -> CargoInvocation {
    let mut b = inv.clone();
    b.test_filter = None;
    b.test_exact = false;
    b.program_args.clear();
    match inv.command {
        CargoCommand::Test => {
            b.command = CargoCommand::Build;
            if matches!(b.target, None | Some(TargetSelector::Lib)) {
                b.extra_args.push("--tests".into());
            }
        }
        CargoCommand::Bench => {
            b.command = CargoCommand::Build;
            if matches!(b.target, None | Some(TargetSelector::Lib)) {
                b.extra_args.push("--benches".into());
            }
        }
        CargoCommand::Run => b.command = CargoCommand::Build,
        _ => {
            b = CargoInvocation::new(CargoCommand::Build, inv.cwd.clone());
            b.package = inv.package.clone();
            b.env = inv.env.clone();
        }
    }
    b
}

/// Start a step; `Ok(None)` when there is nothing to do.
fn start_step(s: &mut Session, step: &Step) -> Result<Option<RunTabId>, String> {
    let root = project_root(s)?;
    match step {
        Step::Build { owner } => {
            let title = format!("Build '{owner}'");
            let config = s.run_configs.iter().find(|c| c.name == *owner).cloned();
            if let Some(inv) = config.as_ref().and_then(|c| c.cargo_invocation()) {
                let tab = crate::runs::start(s, build_invocation(inv), &title, true).map_err(|e| e.to_string())?;
                return Ok(Some(tab));
            }
            let kind = user(s, owner).map(|c| c.kind);
            match kind {
                Some(RunKind::CMake) => {
                    let Some(app) = user(s, owner).map(|c| c.cmake_parts(&root)).transpose()? else {
                        return Ok(None);
                    };
                    let q = |p: &std::path::Path| codecraft_cargo::shell_quote(&p.display().to_string());
                    let script = format!(
                        "{{ test -f {} || {}; }} && cmake --build {} --target {}",
                        q(&app.build_dir.join("CMakeCache.txt")),
                        app.configure,
                        q(&app.build_dir),
                        codecraft_cargo::shell_quote(&app.target)
                    );
                    let mut cmd = ProcessCommand::shell(script, &root);
                    cmd.build = true;
                    crate::runs::start_process(s, &cmd, &title).map(Some).map_err(|e| e.to_string())
                }
                // A C/C++ configuration compiles its sources when it runs.
                Some(RunKind::Cpp) => Ok(None),
                _ => {
                    let cargo = s.ws.project.as_ref().and_then(|p| p.cargo.as_ref()).map(|m| m.workspace_root.clone());
                    if let Some(cargo_root) = cargo.or_else(|| root.join("Cargo.toml").is_file().then(|| root.clone()))
                    {
                        let inv = CargoInvocation::new(CargoCommand::Build, cargo_root);
                        return crate::runs::start(s, inv, &title, true).map(Some).map_err(|e| e.to_string());
                    }
                    match crate::run_configs::build_config(&root) {
                        Some(build) => crate::runs::start_config(s, &build).map(Some).map_err(|e| e.to_string()),
                        None => Ok(None),
                    }
                }
            }
        }
        Step::Run { name } => {
            crate::run_config_store::prepare_run(s, name)?;
            let config = s
                .run_configs
                .iter()
                .find(|c| c.name == *name)
                .cloned()
                .ok_or_else(|| format!("there is no run configuration named `{name}`"))?;
            crate::runs::start_config(s, &config).map(Some).map_err(|e| e.to_string())
        }
        Step::External { command, working_dir, .. } => {
            let words = split_words(command)?;
            let (program, args) = words.split_first().ok_or("the command is empty")?;
            let dir = working_dir.trim();
            let cwd = if dir.is_empty() {
                root.clone()
            } else {
                let p = PathBuf::from(dir);
                if p.is_absolute() { p } else { root.join(p) }
            };
            if !cwd.is_dir() {
                return Err(format!("working directory {} does not exist", cwd.display()));
            }
            let mut cmd = ProcessCommand::new(program.clone(), &[], cwd);
            cmd.args = args.to_vec();
            let title = format!("Before launch: {}", command.trim());
            crate::runs::start_process(s, &cmd, &title).map(Some).map_err(|e| e.to_string())
        }
    }
}

/// Watch the running step's tab: next step when it succeeded, stop the launch when it failed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let Some((step, tab)) = s.launch.pending.as_ref().and_then(|p| p.current.clone()) else { return false };
    let status = s.ws.run(tab).map(|t| t.status);
    match status {
        // Still running (or its exit event is not applied yet).
        Some(RunStatus::Running) => false,
        Some(RunStatus::Succeeded) => {
            if let Some(p) = s.launch.pending.as_mut() {
                p.current = None;
                p.done.push(step.label());
            }
            advance(s);
            true
        }
        Some(RunStatus::Failed(code)) => {
            let code = code.map_or("a signal".to_string(), |c| format!("exit code {c}"));
            fail(s, &format!("{} failed ({code})", step.label()));
            true
        }
        Some(RunStatus::Stopped) => {
            fail(s, &format!("{} was stopped", step.label()));
            true
        }
        None => {
            fail(s, &format!("{} was closed", step.label()));
            true
        }
    }
}

/// Cancel the pending launch (stops the running task).
pub(crate) fn cancel(s: &mut Session) -> bool {
    let Some(p) = s.launch.pending.take() else { return false };
    if let Some((_, tab)) = p.current {
        crate::runs::stop(s, tab);
    }
    s.status = format!("Before launch of '{}' cancelled", p.config);
    true
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "runConfig.beforeLaunch.state",
            "Before Launch State",
            None,
            "→ {pending: {config, mode, steps, current, done}|null, lastError} — the launch waiting for its \
             before-launch tasks",
            Context::Global,
            always,
            |s, _| {
                Ok(json!({
                    "pending": s.launch.pending,
                    "lastError": s.launch.last_error,
                }))
            },
        ),
        spec(
            "runConfig.beforeLaunch.cancel",
            "Cancel Before Launch",
            None,
            "Stop the running before-launch task and don't start the configuration",
            Context::Global,
            always,
            |s, _| {
                cancel(s);
                ok()
            },
        ),
        spec(
            "runConfig.beforeLaunch.preview",
            "Preview Before Launch",
            None,
            "{name: string} → [label] — the tasks Run/Debug of that configuration would run, in order",
            Context::Global,
            always,
            preview,
        ),
    ]
}

fn preview(s: &mut Session, p: &Value) -> CmdResult {
    let name = crate::params::req_str(p, "name")?;
    let steps = expand(s, name.trim()).map_err(CommandError::Failed)?;
    Ok(json!(steps.iter().map(Step::label).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_of_cargo_configurations() {
        let cwd = std::path::Path::new("/w");
        let mut run = CargoInvocation::new(CargoCommand::Run, cwd);
        run.package = Some("app".into());
        run.target = Some(TargetSelector::Bin("app".into()));
        run.program_args = vec!["--port".into(), "1".into()];
        run.release = true;
        let b = build_invocation(&run);
        assert_eq!(b.command, CargoCommand::Build);
        assert!(b.program_args.is_empty() && b.release);
        assert_eq!(b.target, Some(TargetSelector::Bin("app".into())));

        let mut test = CargoInvocation::new(CargoCommand::Test, cwd);
        test.target = Some(TargetSelector::Lib);
        test.test_filter = Some("tests::x".into());
        test.test_exact = true;
        let b = build_invocation(&test);
        assert_eq!(b.command, CargoCommand::Build);
        assert!(b.test_filter.is_none() && !b.test_exact);
        assert!(b.extra_args.contains(&"--tests".to_string()));

        let mut it = CargoInvocation::new(CargoCommand::Test, cwd);
        it.target = Some(TargetSelector::Test("api".into()));
        assert!(!build_invocation(&it).extra_args.contains(&"--tests".to_string()));

        let mut clippy = CargoInvocation::new(CargoCommand::Clippy, cwd);
        clippy.package = Some("p".into());
        let b = build_invocation(&clippy);
        assert_eq!((b.command, b.package.as_deref()), (CargoCommand::Build, Some("p")));
    }

    #[test]
    fn labels() {
        assert_eq!(Step::Build { owner: "A".into() }.label(), "Build 'A'");
        assert_eq!(Step::Run { name: "B".into() }.label(), "Run 'B'");
        let e = Step::External { owner: "A".into(), command: " make gen ".into(), working_dir: String::new() };
        assert_eq!(e.label(), "Run external tool 'make gen'");
    }
}
