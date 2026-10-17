//! Cargo processes behind the Run and Build tool windows, and run configurations.

use std::path::{Path, PathBuf};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

use codecraft_cargo::{
    CargoCommand, CargoInvocation, CargoMessage, CargoMetadata, Level, RunEvent, RunHandle, TargetSelector,
};
use codecraft_text::Position;
use codecraft_workspace::{
    ConsoleKind, Problem, ProblemSource, RunStatus, RunTabId, Severity, TestRecord, ToolWindowId,
};

use crate::{CommandError, Session};

/// A run configuration: a Cargo invocation (derived from Cargo targets), or any other process
/// (CMake, Make, Zig, npm scripts, pytest, …) when `command` is set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfig {
    pub name: String,
    /// The Cargo invocation; meaningful only when `command` is `None` (see [`RunConfig::cargo_invocation`]).
    pub invocation: CargoInvocation,
    #[serde(default)]
    pub command: Option<ProcessCommand>,
    /// Where it runs (local by default; Docker and SSH targets come from M11 tracks D and S).
    #[serde(default)]
    pub target: crate::run_targets::RunTarget,
}

/// A plain process run configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCommand {
    /// Program name (resolved on PATH and the usual tool dirs) or path.
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Show in the Build tool window instead of Run.
    #[serde(default)]
    pub build: bool,
}

impl ProcessCommand {
    pub fn new(program: impl Into<String>, args: &[&str], cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: cwd.into(),
            env: Vec::new(),
            build: false,
        }
    }

    /// `sh -c <script>` (for chained steps such as configure && build && run).
    pub fn shell(script: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: cwd.into(),
            env: Vec::new(),
            build: false,
        }
    }

    pub fn command_line(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(codecraft_cargo::shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn resolved_program(&self) -> PathBuf {
        let p = PathBuf::from(&self.program);
        if p.components().count() > 1 {
            return p;
        }
        codecraft_lsp::which(&self.program).unwrap_or(p)
    }
}

impl RunConfig {
    pub fn cargo(name: impl Into<String>, invocation: CargoInvocation) -> Self {
        Self { name: name.into(), invocation, command: None, target: Default::default() }
    }

    pub fn process(name: impl Into<String>, command: ProcessCommand) -> Self {
        let invocation = CargoInvocation::new(CargoCommand::Run, command.cwd.clone());
        Self { name: name.into(), invocation, command: Some(command), target: Default::default() }
    }

    /// The Cargo invocation, for Cargo configurations only.
    pub fn cargo_invocation(&self) -> Option<&CargoInvocation> {
        self.command.is_none().then_some(&self.invocation)
    }

    pub fn command_line(&self) -> String {
        match &self.command {
            Some(c) => c.command_line(),
            None => self.invocation.command_line(),
        }
    }
}

struct Proc {
    tab: RunTabId,
    handle: RunHandle,
    events: Receiver<RunEvent>,
    /// Cargo's working directory (compiler paths are relative to the workspace root).
    root: PathBuf,
    /// Run next when this one succeeds (Rebuild = clean, then build).
    then: Option<CargoInvocation>,
}

#[derive(Default)]
pub(crate) struct Runs {
    procs: Vec<Proc>,
}

/// A session dropped without `shutdown()` (a test that panicked, a window torn down) must not
/// leave its programs running: a `RunHandle` doesn't kill its process when dropped. `kill` sends
/// SIGTERM to the process group and returns (SIGKILL follows later if needed), so this doesn't wait.
impl Drop for Runs {
    fn drop(&mut self) {
        for p in &self.procs {
            if p.handle.is_running() {
                let _ = p.handle.kill();
            }
        }
    }
}

impl Runs {
    pub(crate) fn any_running(&self) -> bool {
        !self.procs.is_empty()
    }

    pub(crate) fn is_running(&self, tab: RunTabId) -> bool {
        self.procs.iter().any(|p| p.tab == tab)
    }

    /// The process id behind a running tab (it leads its own process group on Unix).
    pub(crate) fn pid(&self, tab: RunTabId) -> Option<u32> {
        self.procs.iter().find(|p| p.tab == tab).map(|p| p.handle.pid())
    }
}

/// Run configurations for a Cargo workspace: one `cargo run` per binary target, plus
/// `cargo test` for the workspace and each package with tests.
pub fn derive_configs(meta: &CargoMetadata) -> Vec<RunConfig> {
    let root = meta.workspace_root.clone();
    let mut out = Vec::new();
    let multi = meta.workspace_packages().count() > 1;
    for p in meta.workspace_packages() {
        for t in p.targets.iter().filter(|t| t.is_bin()) {
            let mut inv = CargoInvocation::new(CargoCommand::Run, root.clone());
            inv.package = Some(p.name.clone());
            inv.target = Some(TargetSelector::Bin(t.name.clone()));
            out.push(RunConfig::cargo(format!("Run {}", t.name), inv));
        }
    }
    for p in meta.workspace_packages() {
        for t in p.targets.iter().filter(|t| t.is_example()) {
            let mut inv = CargoInvocation::new(CargoCommand::Run, root.clone());
            inv.package = Some(p.name.clone());
            inv.target = Some(TargetSelector::Example(t.name.clone()));
            out.push(RunConfig::cargo(format!("Run example {}", t.name), inv));
        }
    }
    out.push(RunConfig::cargo("Test workspace", CargoInvocation::new(CargoCommand::Test, root.clone())));
    if multi {
        for p in meta.workspace_packages() {
            let mut inv = CargoInvocation::new(CargoCommand::Test, root.clone());
            inv.package = Some(p.name.clone());
            out.push(RunConfig::cargo(format!("Test {}", p.name), inv));
        }
    }
    out
}

/// Start a cargo invocation in a new Run (or Build) tab. Saves all files first, like RustRover.
pub(crate) fn start(s: &mut Session, inv: CargoInvocation, title: &str, build: bool) -> Result<RunTabId, CommandError> {
    let _ = s.save_all();
    // A command's `target` param runs it on an SSH host or in a container (M16 track RT).
    if let Some(target) = crate::target_exec::override_target(s) {
        let mut cmd = cargo_command(&inv);
        cmd.build = build;
        return start_on_target(s, &cmd, title, &target);
    }
    start_chain(s, inv, None, title, build)
}

/// A Cargo invocation as a plain `cargo …` process (human-readable output, for targets).
fn cargo_command(inv: &CargoInvocation) -> ProcessCommand {
    let mut c = ProcessCommand::new("cargo", &[], inv.cwd.clone());
    c.args = CargoInvocation { json_messages: false, ..inv.clone() }.args();
    c.env = inv.env.clone();
    c
}

pub(crate) fn start_chain(
    s: &mut Session,
    inv: CargoInvocation,
    then: Option<CargoInvocation>,
    title: &str,
    build: bool,
) -> Result<RunTabId, CommandError> {
    let (handle, events) = codecraft_cargo::spawn(&inv).map_err(|e| CommandError::Failed(e.to_string()))?;
    let tab = s.ws.add_run(title, inv.command_line(), build);
    crate::testing::track_cargo(s, tab, &inv, title);
    if matches!(inv.command, CargoCommand::Build | CargoCommand::Check | CargoCommand::Clippy | CargoCommand::Clean) {
        s.ws.clear_cargo_problems();
    }
    s.ws.layout.show(if build { ToolWindowId::Build } else { ToolWindowId::Run });
    s.runs.procs.push(Proc { tab, handle, events, root: inv.cwd.clone(), then });
    s.status = format!("{title}: running");
    Ok(tab)
}

/// Start any run configuration (saves all files first).
pub(crate) fn start_config(s: &mut Session, config: &RunConfig) -> Result<RunTabId, CommandError> {
    let target = crate::target_exec::effective(s, &config.target);
    if !target.is_local() {
        // On another machine or in a container: run the plain command line through the target.
        let cmd = match &config.command {
            Some(c) => c.clone(),
            None => cargo_command(&config.invocation),
        };
        return start_on_target(s, &cmd, &config.name, &target);
    }
    // `target: local` overrides a configuration's own target.
    if !config.target.is_local() {
        let local = RunConfig { target: Default::default(), ..config.clone() };
        return start_config(s, &local);
    }
    match &config.command {
        Some(cmd) => start_process(s, cmd, &config.name),
        None => start(s, config.invocation.clone(), &config.name, false),
    }
}

/// Start a plain process in a Run (or Build) tab. Saves all files first.
pub(crate) fn start_process(s: &mut Session, cmd: &ProcessCommand, title: &str) -> Result<RunTabId, CommandError> {
    // A command's `target` param runs it on an SSH host or in a container (M16 track RT).
    if let Some(target) = crate::target_exec::override_target(s) {
        return start_on_target(s, cmd, title, &target);
    }
    start_process_here(s, cmd, title)
}

/// [`start_process`] on this machine, whatever the command's `target` param says (for wrapped
/// commands that already run on a target).
pub(crate) fn start_process_here(s: &mut Session, cmd: &ProcessCommand, title: &str) -> Result<RunTabId, CommandError> {
    let _ = s.save_all();
    // Test processes may get extra arguments (pytest: `-v` and a JUnit report).
    let (configured, (cmd, junit)) = (cmd, crate::testing::instrument_process(cmd));
    spawn_tracked(s, &cmd, configured, junit, title, None)
}

/// Spawn `cmd` in a new tab, tracking test results as for `configured`.
fn spawn_tracked(
    s: &mut Session,
    cmd: &ProcessCommand,
    configured: &ProcessCommand,
    junit: Option<PathBuf>,
    title: &str,
    display: Option<String>,
) -> Result<RunTabId, CommandError> {
    let program = cmd.resolved_program();
    let (handle, events) = codecraft_cargo::spawn_process(&program, &cmd.args, &cmd.cwd, &cmd.env, false)
        .map_err(|e| CommandError::Failed(e.to_string()))?;
    let tab = s.ws.add_run(title, display.unwrap_or_else(|| cmd.command_line()), cmd.build);
    crate::testing::track_process(s, tab, configured, junit, title);
    s.ws.layout.show(if cmd.build { ToolWindowId::Build } else { ToolWindowId::Run });
    s.runs.procs.push(Proc { tab, handle, events, root: cmd.cwd.clone(), then: None });
    s.status = format!("{title}: running");
    Ok(tab)
}

/// A test report path as one inside the project's [`crate::target_exec::RUN_DIR`] (so the
/// target writes it where CodeCraft can read it: in the Docker mount, or downloaded from the
/// SSH host).
fn relocate_report(root: &Path, mut cmd: ProcessCommand, report: Option<PathBuf>) -> (ProcessCommand, Option<PathBuf>) {
    let Some(old) = report else { return (cmd, None) };
    if old.starts_with(root) {
        return (cmd, Some(old));
    }
    let name = old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "report.xml".into());
    let new = root.join(crate::target_exec::RUN_DIR).join(name);
    let (from, to) = (old.display().to_string(), new.display().to_string());
    for a in &mut cmd.args {
        if a.contains(&from) {
            *a = a.replace(&from, &to);
        }
    }
    (cmd, Some(new))
}

/// Run `cmd` (local paths) on `target` in a Run tab: the target's wrapper (`ssh`/`docker`), with
/// test reports written into the project and copied back from an SSH host (M16 track RT).
pub(crate) fn start_on_target(
    s: &mut Session,
    cmd: &ProcessCommand,
    title: &str,
    target: &crate::run_targets::RunTarget,
) -> Result<RunTabId, CommandError> {
    use crate::run_targets::RunTarget;
    let _ = s.save_all();
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let (instrumented, report) = crate::testing::instrument_process(cmd);
    let (instrumented, report) = relocate_report(&root, instrumented, report);
    if let Some(r) = &report {
        if let Some(dir) = r.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(r);
    }
    let wrapped = match target {
        RunTarget::Local => instrumented.clone(),
        RunTarget::Ssh(t) => {
            let downloads: Vec<(String, PathBuf)> = report
                .iter()
                .filter_map(|r| Some((codecraft_remote::deploy::rel_path(&root, r)?, r.clone())))
                .collect();
            crate::ssh_target::wrap_with(s, t, instrumented, &downloads)?
        }
        RunTarget::Docker(_) => crate::run_targets::wrap(s, target, instrumented)?,
    };
    // The Run window and test results show the command as configured, and where it runs.
    let display = format!("{} (on {})", cmd.command_line(), target.label());
    let tab = spawn_tracked(s, &wrapped, cmd, report, title, Some(display))?;
    crate::testing::set_run_target(s, tab, target);
    if let Some(t) = s.ws.run_mut(tab) {
        t.push(codecraft_workspace::ConsoleKind::System, format!("Running on {}", target.label()));
    }
    Ok(tab)
}

pub(crate) fn stop(s: &mut Session, tab: RunTabId) -> bool {
    let Some(p) = s.runs.procs.iter_mut().find(|p| p.tab == tab) else { return false };
    p.then = None;
    if let Err(e) = p.handle.kill() {
        log::warn!("stop: {e}");
    }
    if let Some(t) = s.ws.run_mut(tab) {
        t.status = RunStatus::Stopped;
    }
    true
}

pub(crate) fn stop_all(s: &mut Session) {
    let tabs: Vec<RunTabId> = s.runs.procs.iter().map(|p| p.tab).collect();
    for t in tabs {
        stop(s, t);
    }
    // A profile's Cargo build runs outside `procs` (M15 PP).
    for t in s.profiler.build_tabs() {
        crate::profiler::stop(s, t);
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let mut finished: Vec<usize> = Vec::new();
    let mut follow_ups: Vec<(CargoInvocation, RunTabId)> = Vec::new();
    for idx in 0..s.runs.procs.len() {
        let Some(proc) = s.runs.procs.get(idx) else { continue };
        let (tab, root) = (proc.tab, proc.root.clone());
        let mut batch = Vec::new();
        for _ in 0..2000 {
            match proc.events.try_recv() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
        }
        if batch.is_empty() {
            continue;
        }
        changed = true;
        for ev in batch {
            if apply_event(s, tab, &root, ev) {
                finished.push(idx);
                if let Some(p) = s.runs.procs.get_mut(idx)
                    && let Some(next) = p.then.take()
                    && s.ws.run(tab).is_some_and(|t| t.status == RunStatus::Succeeded)
                {
                    follow_ups.push((next, tab));
                }
            }
        }
    }
    for idx in finished.into_iter().rev() {
        if idx < s.runs.procs.len() {
            s.runs.procs.remove(idx);
        }
    }
    for (inv, tab) in follow_ups {
        let title = s.ws.run(tab).map(|t| t.title.clone()).unwrap_or_else(|| "Build".into());
        let build = s.ws.run(tab).is_some_and(|t| t.build);
        if let Err(e) = start_chain(s, inv, None, &title, build) {
            s.status = e.to_string();
        }
    }
    changed
}

/// Apply one event to its tab; returns true when the process finished.
fn apply_event(s: &mut Session, tab: RunTabId, root: &Path, ev: RunEvent) -> bool {
    match ev {
        RunEvent::Started { .. } => false,
        RunEvent::Stdout(line) => {
            if let Some(t) = s.ws.run_mut(tab) {
                t.push(ConsoleKind::Stdout, strip_ansi(&line));
            }
            false
        }
        RunEvent::Stderr(line) => {
            if let Some(t) = s.ws.run_mut(tab) {
                t.push(ConsoleKind::Stderr, strip_ansi(&line));
            }
            false
        }
        RunEvent::Message(CargoMessage::CompilerMessage { diagnostic, .. }) => {
            if let Some(problem) = problem_from_diagnostic(root, &diagnostic) {
                s.ws.add_cargo_problem(problem);
            }
            false
        }
        RunEvent::Message(_) => false,
        RunEvent::Test(codecraft_cargo::TestEvent::Result { name, outcome }) => {
            if let Some(t) = s.ws.run_mut(tab) {
                t.tests.push(TestRecord { name, outcome });
            }
            false
        }
        RunEvent::Test(_) => false,
        RunEvent::Error(e) => {
            if let Some(t) = s.ws.run_mut(tab) {
                t.push(ConsoleKind::System, e);
            }
            false
        }
        RunEvent::Exited { code, success, duration } => {
            if let Some(t) = s.ws.run_mut(tab) {
                if t.status != RunStatus::Stopped {
                    t.status = if success { RunStatus::Succeeded } else { RunStatus::Failed(code) };
                }
                t.duration = Some(duration);
                let code_text = code.map_or("signal".to_string(), |c| c.to_string());
                t.push(ConsoleKind::System, String::new());
                t.push(ConsoleKind::System, format!("Process finished with exit code {code_text}"));
                let title = t.title.clone();
                s.status = format!("{title}: {}", if success { "finished" } else { "failed" });
            }
            true
        }
    }
}

/// A rustc diagnostic as a problem in its primary span's file. Notes and help are folded in.
pub(crate) fn problem_from_diagnostic(root: &Path, d: &codecraft_cargo::Diagnostic) -> Option<Problem> {
    let severity = match d.level {
        Level::Error | Level::Ice => Severity::Error,
        Level::Warning => Severity::Warning,
        Level::Note | Level::Help => Severity::Info,
        Level::FailureNote => return None,
    };
    let span = d.primary_span()?;
    let file = PathBuf::from(&span.file_name);
    let path = if file.is_absolute() { file } else { root.join(file) };
    Some(Problem {
        path: crate::normalize_path(&path),
        start: Position::new(span.line_start.saturating_sub(1), span.column_start.saturating_sub(1)),
        end: Position::new(span.line_end.saturating_sub(1), span.column_end.saturating_sub(1)),
        severity,
        message: d.message.clone(),
        code: d.code.clone(),
        source: ProblemSource::Cargo,
    })
}

/// Remove ANSI escape sequences (CSI and OSC) from console output.
pub(crate) fn strip_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session dropped without `shutdown()` stops what it started (a profiler recording left
    /// running for a day by a test that panicked, M16).
    #[cfg(unix)]
    #[test]
    fn dropping_the_session_stops_its_processes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&tmp.path().join("home"));
        let cmd = ProcessCommand::new("sleep", &["30"], tmp.path());
        start_process(&mut s, &cmd, "sleeper").unwrap();
        let pid = s.runs.procs.first().map(|p| p.handle.pid()).unwrap();
        let alive = |pid: u32| {
            std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|o| {
                    let st = String::from_utf8_lossy(&o.stdout);
                    !st.trim().is_empty() && !st.trim().starts_with('Z')
                })
        };
        assert!(alive(pid));
        drop(s);
        let start = std::time::Instant::now();
        while alive(pid) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!alive(pid), "sleep {pid} still runs after its session was dropped");
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[31merror\u{1b}[0m: x"), "error: x");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"), "link");
        assert_eq!(strip_ansi("\u{1b}"), "");
    }

    #[test]
    fn configs_from_metadata() {
        let meta = CargoMetadata {
            workspace_root: "/w".into(),
            target_directory: "/w/target".into(),
            packages: vec![codecraft_cargo::Package {
                id: "p".into(),
                name: "app".into(),
                targets: vec![codecraft_cargo::Target {
                    name: "app".into(),
                    kind: vec!["bin".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            workspace_members: vec!["p".into()],
        };
        let c = derive_configs(&meta);
        assert_eq!(c.first().map(|c| c.name.as_str()), Some("Run app"));
        assert!(c.iter().any(|c| c.invocation.command == CargoCommand::Test));
    }
}
