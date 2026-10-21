//! Run, debug, test and profile on a run target: an SSH host or a Docker image, container or
//! Compose service (M16 track RT), as PyCharm's remote interpreters and CLion's remote toolchains
//! do.
//!
//! - [`Ctx`]: a target resolved for the open project — how to run a script there (`ssh … sh -c`
//!   after syncing the project, or `docker run|exec|compose run … sh -c`), where the project is
//!   there (the deployment mapping, or the container mount) and how to reach a port there
//!   ([`Forward`]: `ssh -N -L`, a published port, or a `docker exec` socket relay).
//! - Target references for commands and agents ([`resolve_ref`]): `local`, a saved target's name
//!   (Run › Manage Targets…, `.codecraft/targets.json`), an SSH server name, `ssh:<server>`,
//!   `docker:<image>`, `container:<name>`, `compose:<service>`, or a target object.
//! - The `target` param ([`scope`]): run, debug, test and profile commands take one and run
//!   there instead of the configuration's own target.
//!
//! Nothing here blocks the UI thread: scripts become local processes the Run window, the debugger
//! or the profiler start; [`capture`] (probes such as interpreter detection) runs on workers.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_remote::command as rcmd;
use codecraft_remote::deploy;
use codecraft_remote::quote::{self, sh_quote};
pub use codecraft_remote::target_script as script;

use crate::docker_target::DockerTarget;
use crate::run_targets::RunTarget;
use crate::runs::ProcessCommand;
use crate::ssh_target::SshTarget;
use crate::{CommandError, Session};

/// Where CodeCraft keeps files a target run writes for it (test reports, profiles, a copy of a
/// remotely built binary), under the project (so a Docker mount shares it; git-ignored with the
/// rest of `.codecraft/`).
pub const RUN_DIR: &str = ".codecraft/remote-run";

/// Saved targets of the project (Run › Manage Targets…).
pub const TARGETS_FILE: &str = ".codecraft/targets.json";

// ---------------------------------------------------------------- saved targets

/// A named target (Run › Manage Targets… › Add).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedTarget {
    pub name: String,
    pub target: RunTarget,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct TargetsFile {
    version: u32,
    targets: Vec<SavedTarget>,
}

/// The project's saved targets (an unreadable file reads as none).
pub fn saved_targets(root: &Path) -> Vec<SavedTarget> {
    let path = root.join(TARGETS_FILE);
    let Ok(text) = codecraft_fs::read_text_file(&path) else { return Vec::new() };
    serde_json::from_str::<TargetsFile>(&text).map(|f| f.targets).unwrap_or_default()
}

/// Save the project's targets.
pub fn save_targets(root: &Path, targets: &[SavedTarget]) -> Result<(), String> {
    let path = root.join(TARGETS_FILE);
    let file = TargetsFile { version: 1, targets: targets.to_vec() };
    let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// A stable key of a target (for interpreter lists): `ssh:web`, `docker:image:rust:1`, …
pub fn target_key(t: &RunTarget) -> String {
    match t {
        RunTarget::Local => "local".into(),
        RunTarget::Ssh(s) => format!("ssh:{}:{}", s.server.trim(), s.remote_dir.trim()),
        RunTarget::Docker(d) if !d.service.trim().is_empty() => format!("docker:service:{}", d.service.trim()),
        RunTarget::Docker(d) if !d.container.trim().is_empty() => format!("docker:container:{}", d.container.trim()),
        RunTarget::Docker(d) => format!("docker:image:{}", d.image.trim()),
    }
}

/// The target's Python interpreter (empty = the configuration's, by name).
pub fn target_python_of(t: &RunTarget) -> &str {
    match t {
        RunTarget::Local => "",
        RunTarget::Ssh(s) => s.python.trim(),
        RunTarget::Docker(d) => d.python.trim(),
    }
}

/// Set the target's Python interpreter.
pub fn set_target_python(t: &mut RunTarget, python: &str) -> Result<(), String> {
    match t {
        RunTarget::Local => Err("the local machine's interpreter is chosen in Settings › Python, not on a target".into()),
        RunTarget::Ssh(s) => {
            s.python = python.trim().to_string();
            Ok(())
        }
        RunTarget::Docker(d) => {
            d.python = python.trim().to_string();
            Ok(())
        }
    }
}

/// Resolve a target reference: a target object (`{kind: ssh|docker|local, …}`) or a name —
/// `local`, a saved target, `ssh:<server>`, `docker:<image>`, `image:<image>`, `container:<name>`,
/// `compose:<service>` (or `compose:<file>:<service>`), an SSH server's name, or a target the
/// project's configurations use (as Manage Targets lists it).
pub fn resolve_ref(s: &mut Session, v: &Value) -> Result<RunTarget, CommandError> {
    if v.is_object() {
        return serde_json::from_value(v.clone()).map_err(|e| CommandError::BadParams(format!("bad `target`: {e}")));
    }
    let Some(name) = v.as_str().map(str::trim) else {
        return Err(CommandError::BadParams("`target` is a target name or a {kind, …} object".into()));
    };
    if name.is_empty() {
        return Err(CommandError::BadParams("`target` is empty".into()));
    }
    if matches!(name.to_ascii_lowercase().as_str(), "local" | "local machine" | "localhost" | "this machine") {
        return Ok(RunTarget::Local);
    }
    let saved = s.project_root().map(|r| saved_targets(&r)).unwrap_or_default();
    if let Some(t) = saved.iter().find(|t| t.name == name).or_else(|| saved.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
    {
        return Ok(t.target.clone());
    }
    let docker = |f: &dyn Fn(&mut DockerTarget)| {
        let mut d = DockerTarget::default();
        f(&mut d);
        RunTarget::Docker(d)
    };
    if let Some((kind, rest)) = name.split_once(':') {
        let rest = rest.trim().trim_start_matches("//");
        let ok = !rest.is_empty();
        match kind.trim().to_ascii_lowercase().as_str() {
            "ssh" if ok => return Ok(RunTarget::Ssh(SshTarget { server: rest.to_string(), ..Default::default() })),
            "docker" | "image" if ok => return Ok(docker(&|d| d.image = rest.to_string())),
            "container" if ok => return Ok(docker(&|d| d.container = rest.to_string())),
            "compose" | "service" if ok => {
                return Ok(match rest.rsplit_once(':') {
                    Some((file, service)) if file.contains('.') => docker(&|d| {
                        d.compose_file = file.to_string();
                        d.service = service.to_string();
                    }),
                    _ => docker(&|d| d.service = rest.to_string()),
                });
            }
            _ => {}
        }
    }
    if s.remote.server(name).is_some() {
        return Ok(RunTarget::Ssh(SshTarget { server: name.to_string(), ..Default::default() }));
    }
    let rows = crate::cpp::debugging::target_rows(s);
    if let Some(r) = rows.iter().find(|r| r.name == name && r.kind != "local") {
        return Ok(r.target.clone());
    }
    let mut known: Vec<String> = saved.iter().map(|t| t.name.clone()).collect();
    known.extend(rows.iter().filter(|r| r.kind != "local").map(|r| r.name.clone()));
    known.sort();
    known.dedup();
    Err(CommandError::BadParams(format!(
        "unknown target `{name}`: use local, a saved target or SSH server ({}), ssh:<server>, docker:<image>, container:<name> or compose:<service>",
        if known.is_empty() { "none here".to_string() } else { known.join(", ") }
    )))
}

// ---------------------------------------------------------------- the `target` param

/// Commands that take a `target` (name or object) and run there.
pub(crate) const TARGET_COMMANDS: &[&str] = &[
    "build.buildProject",
    "build.rebuildProject",
    "cargo.check",
    "cargo.clippy",
    "cargo.test",
    "cargo.doc",
    "cargo.runTarget",
    "run.rerun",
    "tools.rust.reformatCargoProjectWithRustfmt",
    "run.run",
    "run.debug",
    "run.contextConfiguration",
    "run.testsInFile",
    "run.profile",
    "test.runFiltered",
    "test.rerun",
    "test.rerunFailed",
    "debug.atCaret",
    "debug.test",
    "debug.testsInFile",
    "debug.executable",
    "profiler.record",
    "profiler.atCaret",
    "profiler.profileMemory",
];

/// The target a command's `target` param names (`None`: the command takes none or has none).
pub(crate) fn scope(s: &mut Session, id: &str, params: &Value) -> Result<Option<RunTarget>, CommandError> {
    if !TARGET_COMMANDS.contains(&id) {
        return Ok(None);
    }
    match params.get("target") {
        None | Some(Value::Null) => Ok(s.remote.project.as_ref().map(|p| RunTarget::Ssh(SshTarget { server: p.server.clone(), sync: false, ..Default::default() }))),
        Some(v) => resolve_ref(s, v).map(Some),
    }
}

/// Run `f` with `target` as the run target of whatever it starts.
pub(crate) fn with_target<R>(s: &mut Session, target: RunTarget, f: impl FnOnce(&mut Session) -> R) -> R {
    let previous = s.targets.scoped.replace(target);
    let r = f(s);
    s.targets.scoped = previous;
    r
}

/// The target a command's `target` param chose (`None`: none, or `local`).
pub(crate) fn override_target(s: &Session) -> Option<RunTarget> {
    match &s.targets.scoped {
        Some(target) => (!target.is_local()).then(|| target.clone()),
        None => s.remote.project.as_ref().map(|p| RunTarget::Ssh(SshTarget { server: p.server.clone(), sync: false, ..Default::default() })),
    }
}

/// Where something configured to run on `configured` runs: the `target` param wins.
pub(crate) fn effective(s: &Session, configured: &RunTarget) -> RunTarget {
    match &s.targets.scoped {
        Some(t) => t.clone(),
        None if configured.is_local() => s.remote.project.as_ref().map(|p| RunTarget::Ssh(SshTarget { server: p.server.clone(), sync: false, ..Default::default() })).unwrap_or_else(|| configured.clone()),
        None => configured.clone(),
    }
}

// ---------------------------------------------------------------- state

/// Targets state of a session: the scoped `target` param and the remote interpreters
/// (Settings › Python › Remote interpreters).
#[derive(Default)]
pub struct TargetsState {
    pub(crate) scoped: Option<RunTarget>,
    pub python: crate::target_python::RemotePythonState,
}

impl std::fmt::Debug for TargetsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetsState").field("scoped", &self.scoped).finish()
    }
}

// ---------------------------------------------------------------- a resolved target

#[derive(Clone, Debug)]
enum Via {
    Ssh { r: crate::ssh_target::Resolved, sync: bool, sync_line: String, rsync: Option<String>, scp: String },
    Docker { docker: String, base: Vec<String>, t: DockerTarget },
}

/// A target resolved for the open project.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub target: RunTarget,
    /// `SSH: web`, `Docker: python:3.12`, …
    pub label: String,
    pub local_root: PathBuf,
    /// The project folder on the target as configured (absolute for Docker; `~/app` possible
    /// for SSH: see [`Ctx::root_abs`]).
    pub remote_root: String,
    via: Via,
}

/// What a target script needs besides the script.
#[derive(Clone, Debug, Default)]
pub struct Extras {
    /// SSH: upload the project first (when the target syncs).
    pub sync: bool,
    /// Docker image/Compose targets: publish these ports on the host's loopback.
    pub ports: Vec<u16>,
    /// Local file → remote path, copied before the script runs.
    pub uploads: Vec<(PathBuf, String)>,
    /// Remote path → local path, copied before the script runs.
    pub before: Vec<(String, PathBuf)>,
    /// Remote folder → local folder copied before the script runs when it exists there (a
    /// macOS host's `.dSYM` next to an executable).
    pub before_optional: Vec<(String, PathBuf)>,
    /// Remote path → local path (`true`: a folder), copied after it ran (its status is kept).
    pub after: Vec<(String, PathBuf, bool)>,
    /// Show in the Build tool window.
    pub build: bool,
}

impl Ctx {
    /// Resolve `target` for the open project (reads the server list and deployment mapping, no
    /// network). Fails for the local machine.
    pub fn resolve(s: &mut Session, target: &RunTarget) -> Result<Ctx, CommandError> {
        match target {
            RunTarget::Local => Err(CommandError::Failed("the local machine is not a remote target".into())),
            RunTarget::Ssh(t) => {
                let r = crate::ssh_target::resolve(s, t)?;
                let sync_line = if t.sync { crate::ssh_target::sync_line(s, &r)? } else { String::new() };
                let rsync = s.remote.has_rsync().then(|| s.remote.programs.rsync.clone());
                Ok(Ctx {
                    target: target.clone(),
                    label: format!("SSH: {}", t.server.trim()),
                    local_root: r.root.clone(),
                    remote_root: r.remote_root.clone(),
                    via: Via::Ssh { sync: t.sync, sync_line, rsync, scp: s.remote.programs.scp.clone(), r },
                })
            }
            RunTarget::Docker(t) => {
                if t.image.trim().is_empty() && t.container.trim().is_empty() && t.service.trim().is_empty() {
                    return Err(CommandError::Failed(
                        "the Docker target needs an image, a container or a Compose service".into(),
                    ));
                }
                let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
                Ok(Ctx {
                    target: target.clone(),
                    label: format!("Docker: {}", t.label()),
                    local_root: root,
                    remote_root: t.mount_path(),
                    via: Via::Docker {
                        docker: s.docker.program_path().display().to_string(),
                        base: s.docker.base_args(),
                        t: t.clone(),
                    },
                })
            }
        }
    }

    pub fn is_ssh(&self) -> bool {
        matches!(self.via, Via::Ssh { .. })
    }

    /// The interpreter set on the target (empty = by name).
    pub fn python(&self) -> Option<String> {
        Some(target_python_of(&self.target).to_string()).filter(|p| !p.is_empty())
    }

    /// The interpreter to use on the target for a configuration's `program` (`python3` when the
    /// configuration names a local path).
    pub fn python_for(&self, local: Option<&Path>) -> String {
        if let Some(p) = self.python() {
            return p;
        }
        local
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| n.starts_with("python"))
            .unwrap_or_else(|| "python3".into())
    }

    /// Ports the target publishes itself (`docker run -p`): no forwarder needed.
    fn publishes(&self) -> bool {
        matches!(&self.via, Via::Docker { t, .. } if t.publishes_ports())
    }

    /// The address a debug server listens on inside the target: all interfaces in a container
    /// that publishes the port (Docker's proxy connects to the container's interface), else
    /// loopback.
    pub fn listen_host(&self) -> &'static str {
        if self.publishes() { "0.0.0.0" } else { "127.0.0.1" }
    }

    /// A local path under the project as the same path on the target.
    pub fn to_remote(&self, local: &Path) -> Option<String> {
        let rel = deploy::rel_path(&self.local_root, local)?;
        Some(rcmd::join(&self.remote_root, &rel))
    }

    /// A word of a command line with a local project path (`/p/x`, `--k=/p/x`) mapped.
    pub fn map_word(&self, word: &str) -> String {
        let (prefix, value) = match word.split_once('=') {
            Some((k, v)) if k.starts_with('-') => (format!("{k}="), v),
            _ => (String::new(), word),
        };
        if !Path::new(value).is_absolute() {
            return word.to_string();
        }
        match self.to_remote(Path::new(value)) {
            Some(m) => format!("{prefix}{m}"),
            None => word.to_string(),
        }
    }

    /// The program to run on the target for a local `program`: a project path mapped, a local
    /// tool path by name, a Python interpreter replaced by the target's.
    pub fn remote_program(&self, program: &str) -> String {
        if let Some(py) = crate::docker_target::target_python(target_python_of(&self.target), program) {
            return py;
        }
        let p = Path::new(program);
        if !p.is_absolute() {
            return program.to_string();
        }
        self.to_remote(p)
            .or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| program.to_string())
    }

    /// The project folder's absolute path on the target: configured absolute, else reported by
    /// a script's [`script::ROOT_MARK`] line among `lines`.
    pub fn root_abs<'a>(&self, lines: impl IntoIterator<Item = &'a str>) -> Option<String> {
        if self.remote_root.starts_with('/') {
            return Some(self.remote_root.trim_end_matches('/').to_string()).filter(|r| !r.is_empty()).or(Some("/".into()));
        }
        lines
            .into_iter()
            .filter_map(|l| script::marker(l, script::ROOT_MARK))
            .find(|r| r.starts_with('/'))
            .map(|r| r.trim_end_matches('/').to_string())
    }

    /// A path on the target (under `root_abs`) as the local path.
    pub fn to_local(&self, remote: &str, root_abs: &str) -> Option<PathBuf> {
        let root = root_abs.trim_end_matches('/');
        let rel = remote.strip_prefix(root)?.strip_prefix('/').or_else(|| (remote == root).then_some(""))?;
        Some(if rel.is_empty() { self.local_root.clone() } else { self.local_root.join(rel) })
    }

    /// The script that runs `cmd` (local paths) in its mapped folder on the target:
    /// [`script::prelude`] + `exec program args`.
    pub fn run_script(&self, cmd: &ProcessCommand) -> Result<String, String> {
        let cwd = self.to_remote(&cmd.cwd).unwrap_or_else(|| self.remote_root.clone());
        let mut out = script::prelude(&cwd, &self.env_for(cmd)).map_err(|e| e.to_string())?;
        out.push_str("exec ");
        out.push_str(&sh_quote(&self.remote_program(&cmd.program)));
        for a in &cmd.args {
            out.push(' ');
            out.push_str(&sh_quote(&self.map_word(a)));
        }
        Ok(out)
    }

    /// `cmd`'s environment on the target (Cargo builds in a container use their own target
    /// directory, as Run does).
    pub fn env_for(&self, cmd: &ProcessCommand) -> Vec<(String, String)> {
        let mut env = cmd.env.clone();
        let cargo = Path::new(&cmd.program).file_name().is_some_and(|n| n == "cargo");
        if cargo && matches!(self.via, Via::Docker { .. }) && !env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
            env.push(("CARGO_TARGET_DIR".into(), format!("{}/target/docker", self.remote_root)));
        }
        env
    }

    /// Whether a remote path is inside the Docker mount (so the file is local already).
    fn under_mount(&self, remote: &str) -> bool {
        let root = self.remote_root.trim_end_matches('/');
        remote == root || remote.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
    }

    /// A remote path made absolute against the remote project folder.
    fn remote_abs(&self, remote: &str) -> String {
        if remote.starts_with('/') || remote.starts_with('~') { remote.to_string() } else { rcmd::join(&self.remote_root, remote) }
    }

    /// The local process that runs `script` on the target (`sh -c` there).
    pub fn process(&self, script_text: &str, x: &Extras) -> Result<ProcessCommand, String> {
        match &self.via {
            Via::Ssh { r, sync, sync_line, rsync, scp } => {
                let mut steps: Vec<String> = Vec::new();
                if x.sync && *sync {
                    steps.push(sync_line.clone());
                }
                for (local, remote) in &x.uploads {
                    let put = rcmd::ssh_exec(&r.server, &r.opts, &rcmd::put_command(&self.remote_abs(remote)))
                        .map_err(|e| e.to_string())?;
                    let words: Vec<String> = std::iter::once(r.opts.program.clone()).chain(put).collect();
                    steps.push(format!("{} < {}", quote::join(&words), sh_quote(&local.display().to_string())));
                }
                for (remote, local) in &x.before {
                    steps.push(self.ssh_download(r, rsync.as_deref(), scp, &self.remote_abs(remote), local, false)?);
                }
                for (remote, local) in &x.before_optional {
                    let dl = self.ssh_download(r, rsync.as_deref(), scp, &self.remote_abs(remote), local, true)?;
                    steps.push(format!("{{ {dl}; }} 2>/dev/null || true"));
                }
                let run = crate::ssh_target::exec_words(r, script_text).map_err(|e| e.to_string())?;
                let last = if x.after.is_empty() {
                    format!("exec {}", quote::join(&run))
                } else {
                    let mut tail = String::new();
                    for (remote, local, dir) in &x.after {
                        tail.push_str("; ");
                        tail.push_str(&self.ssh_download(r, rsync.as_deref(), scp, &self.remote_abs(remote), local, *dir)?);
                    }
                    format!("{}; cc_st=$?{tail}; exit $cc_st", quote::join(&run))
                };
                steps.push(format!("{{ {last}; }}"));
                let mut p = ProcessCommand::shell(steps.join(" && "), self.local_root.clone());
                p.build = x.build;
                Ok(p)
            }
            Via::Docker { docker, base, t } => {
                let mut before: Vec<String> = Vec::new();
                let mut after: Vec<String> = Vec::new();
                let container = t.container.trim();
                let exec = !container.is_empty() && t.service.trim().is_empty();
                for (local, remote) in &x.uploads {
                    if local.starts_with(&self.local_root) {
                        continue;
                    }
                    if !exec {
                        return Err(format!("{} is not in the project, so the container can't see it", local.display()));
                    }
                    before.push(quote::join(&[
                        docker.clone(),
                        "cp".into(),
                        local.display().to_string(),
                        format!("{container}:{}", self.remote_abs(remote)),
                    ]));
                }
                let copy_out = |remote: &str, local: &Path, out: &mut Vec<String>| -> Result<(), String> {
                    let remote = self.remote_abs(remote);
                    if self.under_mount(&remote) {
                        return Ok(());
                    }
                    if !exec {
                        return Err(format!("{remote} is outside the mounted project ({})", self.remote_root));
                    }
                    let parent = local.parent().map(|p| p.display().to_string()).unwrap_or_else(|| ".".into());
                    out.push(format!(
                        "mkdir -p {} && {}",
                        sh_quote(&parent),
                        quote::join(&[docker.clone(), "cp".into(), format!("{container}:{remote}"), local.display().to_string()])
                    ));
                    Ok(())
                };
                for (remote, local) in &x.before {
                    copy_out(remote, local, &mut before)?;
                }
                for (remote, local, _) in &x.after {
                    copy_out(remote, local, &mut after)?;
                }
                let mut args = crate::docker_target::docker_args(base, t, &self.local_root, self.remote_root.clone(), &[], &x.ports)?;
                args.extend(["sh".into(), "-c".into(), script_text.to_string()]);
                if before.is_empty() && after.is_empty() {
                    let mut p = ProcessCommand::new(docker.clone(), &[], self.local_root.clone());
                    p.args = args;
                    p.build = x.build;
                    return Ok(p);
                }
                let run: Vec<String> = std::iter::once(docker.clone()).chain(args).collect();
                let mut line = before.join(" && ");
                if !line.is_empty() {
                    line.push_str(" && ");
                }
                if after.is_empty() {
                    line.push_str(&format!("exec {}", quote::join(&run)));
                } else {
                    line.push_str(&format!("{{ {}; cc_st=$?; {}; exit $cc_st; }}", quote::join(&run), after.join("; ")));
                }
                let mut p = ProcessCommand::shell(line, self.local_root.clone());
                p.build = x.build;
                Ok(p)
            }
        }
    }

    fn ssh_download(
        &self,
        r: &crate::ssh_target::Resolved,
        rsync: Option<&str>,
        scp: &str,
        remote: &str,
        local: &Path,
        dir: bool,
    ) -> Result<String, String> {
        let words: Vec<String> = match rsync {
            Some(rsync) => {
                let spec = rcmd::RsyncSpec {
                    direction: rcmd::Direction::Download,
                    local,
                    remote,
                    dir,
                    excludes: &[],
                    dry_run: false,
                    delete: false,
                    update: false,
                    files_from: None,
                };
                std::iter::once(rsync.to_string())
                    .chain(rcmd::rsync_args(&r.server, &r.opts, &spec).map_err(|e| e.to_string())?)
                    .collect()
            }
            None => std::iter::once(scp.to_string())
                .chain(
                    rcmd::scp_args(&r.server, &r.opts, rcmd::Direction::Download, local, remote, dir)
                        .map_err(|e| e.to_string())?,
                )
                .collect(),
        };
        let parent = if dir { local.to_path_buf() } else { local.parent().map(Path::to_path_buf).unwrap_or_default() };
        Ok(format!("mkdir -p {} && {} >/dev/null", sh_quote(&parent.display().to_string()), quote::join(&words)))
    }

    /// The local process that runs a probe `script` (no project upload).
    pub fn probe(&self, script_text: &str) -> Result<ProcessCommand, String> {
        self.process(script_text, &Extras::default())
    }

    /// Make `remote_port` on the target reachable on a local port: `ssh -N -L` (the same port
    /// number), the port the container publishes, or a `docker exec` relay (another port).
    pub fn forward(&self, remote_port: u16) -> Result<Forward, String> {
        match &self.via {
            Via::Ssh { r, .. } => {
                let args = rcmd::ssh_tunnel(&r.server, &r.opts, remote_port, "127.0.0.1", remote_port)
                    .map_err(|e| e.to_string())?;
                let mut child = Command::new(crate::remote::resolve_program(&r.opts.program))
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("cannot run ssh to forward port {remote_port}: {e}"))?;
                let errors = Arc::new(Mutex::new(String::new()));
                if let Some(mut err) = child.stderr.take() {
                    let errors = errors.clone();
                    let _ = std::thread::Builder::new().name("ssh-forward-stderr".into()).spawn(move || {
                        let mut buf = [0u8; 4096];
                        while let Ok(n) = err.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            let mut e = errors.lock().unwrap_or_else(PoisonError::into_inner);
                            if e.len() < 16 * 1024 {
                                e.push_str(&String::from_utf8_lossy(buf.get(..n).unwrap_or(&[])));
                            }
                        }
                    });
                }
                Ok(Forward { local_port: remote_port, remote_port, child: Some(child), relay: None, errors })
            }
            Via::Docker { t, .. } if t.publishes_ports() => Ok(Forward {
                local_port: remote_port,
                remote_port,
                child: None,
                relay: None,
                errors: Arc::default(),
            }),
            Via::Docker { docker, base, t } => {
                let mut argv: Vec<String> = if t.context.trim().is_empty() {
                    base.clone()
                } else {
                    vec!["--context".into(), t.context.trim().to_string()]
                };
                argv.extend(["exec".into(), "-i".into(), t.container.trim().to_string()]);
                argv.extend(["sh".into(), "-c".into(), script::relay(remote_port)]);
                let relay = Relay::start(docker.clone(), argv)?;
                Ok(Forward { local_port: relay.port, remote_port, child: None, relay: Some(relay), errors: Arc::default() })
            }
        }
    }
}

// ---------------------------------------------------------------- forwarding

/// A forwarded port; dropping it closes the forward.
pub struct Forward {
    pub local_port: u16,
    pub remote_port: u16,
    child: Option<Child>,
    relay: Option<Relay>,
    errors: Arc<Mutex<String>>,
}

impl std::fmt::Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forward").field("local_port", &self.local_port).field("remote_port", &self.remote_port).finish()
    }
}

impl Forward {
    /// Why the forward ended (ssh exited), if it did.
    pub fn failure(&mut self) -> Option<String> {
        let child = self.child.as_mut()?;
        let status = child.try_wait().ok().flatten()?;
        let err = self.errors.lock().unwrap_or_else(PoisonError::into_inner).trim().to_string();
        Some(format!("the port forward ended ({status}){}", if err.is_empty() { String::new() } else { format!(": {err}") }))
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.relay = None;
    }
}

/// A local TCP listener whose connections are piped through `docker exec -i <container> <relay>`.
struct Relay {
    port: u16,
    stop: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Child>>>,
}

impl Relay {
    fn start(docker: String, argv: Vec<String>) -> Result<Relay, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local port: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let children: Arc<Mutex<Vec<Child>>> = Arc::default();
        let (stop2, children2) = (stop.clone(), children.clone());
        std::thread::Builder::new()
            .name("docker-relay".into())
            .spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(e) = relay_connection(stream, &docker, &argv, &children2) {
                                log::warn!("docker relay: {e}");
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(30));
                        }
                        Err(e) => {
                            log::warn!("docker relay: {e}");
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            })
            .map_err(|e| format!("cannot start the relay thread: {e}"))?;
        Ok(Relay { port, stop, children })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        for c in children.iter_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        children.clear();
    }
}

fn relay_connection(stream: TcpStream, docker: &str, argv: &[String], children: &Mutex<Vec<Child>>) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    let mut child = Command::new(crate::remote::resolve_program(docker))
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run docker exec for the relay: {e}"))?;
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the relay has no pipes".into());
    };
    let mut up = stream.try_clone().map_err(|e| e.to_string())?;
    let mut down = stream;
    let _ = std::thread::Builder::new().name("docker-relay-up".into()).spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = up.read(&mut buf) {
            if n == 0 || stdin.write_all(buf.get(..n).unwrap_or(&[])).is_err() {
                break;
            }
            let _ = stdin.flush();
        }
    });
    let _ = std::thread::Builder::new().name("docker-relay-down".into()).spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 || down.write_all(buf.get(..n).unwrap_or(&[])).is_err() {
                break;
            }
        }
        let _ = down.shutdown(std::net::Shutdown::Both);
    });
    children.lock().unwrap_or_else(PoisonError::into_inner).push(child);
    Ok(())
}

/// A free local TCP port.
pub fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local port: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- probes (worker threads)

/// A finished probe.
#[derive(Clone, Debug, Default)]
pub struct Captured {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    /// The [`script::MISSING_MARK`] message, else the last stderr line.
    pub fn error(&self) -> String {
        let all = format!("{}\n{}", self.stderr, self.stdout);
        if let Some(m) = all.lines().find_map(|l| script::marker(l, script::MISSING_MARK)) {
            return m.to_string();
        }
        let last = self.stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let code = self.code.map_or("a signal".to_string(), |c| format!("exit code {c}"));
        if last.is_empty() { format!("the command failed ({code})") } else { format!("{last} ({code})") }
    }
}

/// Output kept per stream.
const CAPTURE_CAP: usize = 4 << 20;

/// Run `cmd` and capture its output, killing it after `timeout` (blocking: worker threads only).
pub fn capture(cmd: &ProcessCommand, timeout: Duration) -> Result<Captured, String> {
    let program = if Path::new(&cmd.program).components().count() > 1 {
        PathBuf::from(&cmd.program)
    } else {
        codecraft_lsp::which(&cmd.program).unwrap_or_else(|| PathBuf::from(&cmd.program))
    };
    let mut c = Command::new(&program);
    c.args(&cmd.args).envs(cmd.env.iter().cloned()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if cmd.cwd.is_dir() {
        c.current_dir(&cmd.cwd);
    }
    let mut child = c.spawn().map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let read = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(r) = r {
                let _ = r.take(CAPTURE_CAP as u64).read_to_end(&mut out);
            }
            String::from_utf8_lossy(&out).into_owned()
        })
    };
    let out = read(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = read(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok(Captured { success: status.success(), code: status.code(), stdout, stderr })
}

// ---------------------------------------------------------------- paths in results

/// Map the target's paths in a profile back to local ones (`from` → `to`, both absolute).
pub fn remap_profile(p: &mut codecraft_profile::Profile, from: &str, to: &str) {
    let from = from.trim_end_matches('/');
    if from.is_empty() || from == "/" {
        return;
    }
    let map = |v: &mut String| {
        if let Some(rest) = v.strip_prefix(from)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            *v = format!("{to}{rest}");
        }
    };
    for f in &mut p.frames {
        if let Some(file) = f.file.as_mut() {
            map(file);
        }
        if let Some(m) = f.module.as_mut() {
            map(m);
        }
    }
    if let Some(mem) = p.memory.as_mut() {
        for a in &mut mem.at_peak {
            if let Some(file) = a.file.as_mut() {
                map(file);
            }
        }
        if let Some(file) = mem.peak_snapshot_file.as_mut() {
            map(file);
        }
    }
}

/// `{target, label}` for results.
pub fn describe(ctx: &Ctx) -> Value {
    json!({ "target": ctx.label, "remoteRoot": ctx.remote_root })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_session() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&root.join("home"));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(root.join("home/ssh_config"), "Host web\n  HostName web.example\n  User me\n").unwrap();
        s.remote.share_connections = false;
        s.open_project(&project);
        (dir, s)
    }

    #[test]
    fn target_refs_resolve() {
        let (dir, mut s) = ssh_session();
        let root = s.project_root().unwrap();
        assert_eq!(resolve_ref(&mut s, &json!("local")).unwrap(), RunTarget::Local);
        assert!(matches!(resolve_ref(&mut s, &json!("web")).unwrap(), RunTarget::Ssh(t) if t.server == "web"));
        assert!(matches!(resolve_ref(&mut s, &json!("ssh:other")).unwrap(), RunTarget::Ssh(t) if t.server == "other"));
        assert!(matches!(resolve_ref(&mut s, &json!("docker:python:3.12")).unwrap(), RunTarget::Docker(t) if t.image == "python:3.12"));
        assert!(matches!(resolve_ref(&mut s, &json!("container:dev")).unwrap(), RunTarget::Docker(t) if t.container == "dev"));
        assert!(matches!(resolve_ref(&mut s, &json!("compose:web")).unwrap(), RunTarget::Docker(t) if t.service == "web"));
        assert!(matches!(
            resolve_ref(&mut s, &json!("compose:deploy/compose.yaml:api")).unwrap(),
            RunTarget::Docker(t) if t.service == "api" && t.compose_file == "deploy/compose.yaml"
        ));
        let obj = resolve_ref(&mut s, &json!({ "kind": "docker", "image": "x", "python": "/v/py" })).unwrap();
        assert_eq!(target_python_of(&obj), "/v/py");
        let err = resolve_ref(&mut s, &json!("nowhere")).unwrap_err().to_string();
        assert!(err.contains("unknown target `nowhere`") && err.contains("web"), "{err}");
        assert!(resolve_ref(&mut s, &json!(5)).is_err());
        assert!(resolve_ref(&mut s, &json!("")).is_err());
        assert!(resolve_ref(&mut s, &json!({ "kind": "mars" })).is_err());
        // Saved targets win over servers.
        save_targets(&root, &[SavedTarget { name: "web".into(), target: obj.clone() }]).unwrap();
        assert_eq!(resolve_ref(&mut s, &json!("WEB")).unwrap(), obj);
        assert_eq!(saved_targets(&root).len(), 1);
        std::fs::write(root.join(TARGETS_FILE), "{garbage").unwrap();
        assert!(saved_targets(&root).is_empty());
        drop(dir);
    }

    #[test]
    fn scope_applies_to_listed_commands_only() {
        let (_dir, mut s) = ssh_session();
        assert!(scope(&mut s, "run.manageTargets.assign", &json!({ "target": "web" })).unwrap().is_none());
        assert!(scope(&mut s, "debug.launch", &json!({ "target": { "kind": "python" } })).unwrap().is_none());
        let t = scope(&mut s, "run.run", &json!({ "target": "web" })).unwrap().unwrap();
        assert!(override_target(&s).is_none());
        with_target(&mut s, t.clone(), |s| {
            assert_eq!(override_target(s), Some(t.clone()));
            assert_eq!(effective(s, &RunTarget::Local), t);
        });
        assert!(override_target(&s).is_none());
        assert!(scope(&mut s, "run.run", &json!({ "target": "nowhere" })).is_err());
        with_target(&mut s, RunTarget::Local, |s| assert!(override_target(s).is_none()));
    }

    #[test]
    fn ssh_ctx_maps_paths_and_builds_processes() {
        let (_dir, mut s) = ssh_session();
        let root = s.project_root().unwrap();
        let t = RunTarget::Ssh(SshTarget { server: "web".into(), remote_dir: "~/app".into(), ..Default::default() });
        let ctx = Ctx::resolve(&mut s, &t).unwrap();
        assert!(ctx.is_ssh());
        assert_eq!(ctx.label, "SSH: web");
        assert_eq!(ctx.to_remote(&root.join("src/a.py")).as_deref(), Some("~/app/src/a.py"));
        assert_eq!(ctx.map_word(&format!("--x={}", root.join("a").display())), "--x=~/app/a");
        assert_eq!(ctx.map_word("/elsewhere"), "/elsewhere");
        assert_eq!(ctx.remote_program("/usr/local/bin/python3"), "python3");
        assert_eq!(ctx.python_for(Some(Path::new("/x/.venv/bin/python3.12"))), "python3.12");
        assert_eq!(ctx.python_for(None), "python3");
        assert_eq!(ctx.listen_host(), "127.0.0.1");
        assert_eq!(ctx.root_abs(["x", "CODECRAFT-REMOTE-ROOT=/home/me/app"]).as_deref(), Some("/home/me/app"));
        assert_eq!(ctx.root_abs(["nothing"]), None);
        assert_eq!(ctx.to_local("/home/me/app/src/a.py", "/home/me/app"), Some(root.join("src/a.py")));
        assert_eq!(ctx.to_local("/home/me/appx/a.py", "/home/me/app"), None);
        let x = Extras {
            sync: true,
            uploads: vec![(PathBuf::from("/tmp/helper.py"), ".codecraft/remote-run/helper.py".into())],
            after: vec![(".codecraft/remote-run/out.json".into(), root.join(".codecraft/remote-run/out.json"), false)],
            ..Default::default()
        };
        let p = ctx.process("echo hi", &x).unwrap();
        assert_eq!(p.program, "sh");
        let line = &p.args[1];
        assert!(line.contains("rsync") || line.contains("scp"), "{line}");
        assert!(line.contains("< /tmp/helper.py"), "{line}");
        assert!(line.contains("cc_st=$?") && line.ends_with("exit $cc_st; }"), "{line}");
        let probe = ctx.probe("echo hi").unwrap();
        assert!(!probe.args[1].contains("rsync") && probe.args[1].contains("exec "), "{:?}", probe.args);
        let script = ctx.run_script(&ProcessCommand::new("/usr/bin/python3", &[&root.join("m.py").display().to_string()], &root)).unwrap();
        assert!(script.starts_with("cd \"$HOME\"/app || exit 1;") && script.ends_with("exec python3 '~/app/m.py'"), "{script}");
        // An absolute remote folder is its own absolute root.
        let abs = Ctx::resolve(&mut s, &RunTarget::Ssh(SshTarget { server: "web".into(), remote_dir: "/srv/app/".into(), ..Default::default() })).unwrap();
        assert_eq!(abs.root_abs(std::iter::empty()).as_deref(), Some("/srv/app"));
        assert!(Ctx::resolve(&mut s, &RunTarget::Local).is_err());
    }

    #[test]
    fn docker_ctx_processes_and_relays() {
        let (_dir, mut s) = ssh_session();
        let root = s.project_root().unwrap();
        s.docker.program = "/fake/docker".into();
        let image = RunTarget::Docker(DockerTarget { image: "python:3".into(), ..Default::default() });
        let ctx = Ctx::resolve(&mut s, &image).unwrap();
        assert_eq!(ctx.listen_host(), "0.0.0.0");
        assert_eq!(ctx.remote_root, "/workspace");
        let x = Extras { ports: vec![5678], after: vec![("/workspace/.codecraft/remote-run/o".into(), root.join("o"), false)], ..Default::default() };
        let p = ctx.process("echo hi", &x).unwrap();
        assert_eq!(p.program, "/fake/docker");
        assert!(p.args.windows(2).any(|w| w == ["-p", "127.0.0.1:5678:5678"]), "{:?}", p.args);
        assert_eq!(&p.args[p.args.len() - 3..], ["sh", "-c", "echo hi"]);
        // Outside the mount, an image can't hand a file back.
        let bad = Extras { after: vec![("/tmp/o".into(), root.join("o"), false)], ..Default::default() };
        assert!(ctx.process("x", &bad).is_err());
        let container = RunTarget::Docker(DockerTarget { container: "dev".into(), ..Default::default() });
        let ctx = Ctx::resolve(&mut s, &container).unwrap();
        assert_eq!(ctx.listen_host(), "127.0.0.1");
        let x = Extras { after: vec![("/tmp/o".into(), root.join("o"), false)], ..Default::default() };
        let p = ctx.process("x", &x).unwrap();
        assert!(p.args[1].contains("/fake/docker cp dev:/tmp/o"), "{:?}", p.args);
        let empty = RunTarget::Docker(DockerTarget::default());
        assert!(Ctx::resolve(&mut s, &empty).is_err());
        let fwd = ctx.forward(1).unwrap();
        assert_ne!(fwd.local_port, 0);
        drop(fwd);
    }

    #[test]
    fn profiles_map_back_to_local_paths() {
        let mut p = codecraft_profile::Profile::default();
        p.frames.push(codecraft_profile::Frame {
            name: "f".into(),
            file: Some("/srv/app/main.py".into()),
            ..Default::default()
        });
        p.frames.push(codecraft_profile::Frame { name: "g".into(), file: Some("/srv/appx/m.py".into()), ..Default::default() });
        remap_profile(&mut p, "/srv/app/", "/local/p");
        assert_eq!(p.frames[0].file.as_deref(), Some("/local/p/main.py"));
        assert_eq!(p.frames[1].file.as_deref(), Some("/srv/appx/m.py"));
        remap_profile(&mut p, "/", "/x");
        assert_eq!(p.frames[0].file.as_deref(), Some("/local/p/main.py"));
    }

    #[cfg(unix)]
    #[test]
    fn capture_runs_and_times_out() {
        let ok = capture(&ProcessCommand::new("sh", &["-c", "echo out; echo err >&2"], "/"), Duration::from_secs(10)).unwrap();
        assert!(ok.success && ok.stdout.trim() == "out" && ok.stderr.trim() == "err");
        let missing = capture(
            &ProcessCommand::new("sh", &["-c", "echo 'CODECRAFT-MISSING: install x' >&2; exit 127"], "/"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(missing.error(), "install x");
        let failed = capture(&ProcessCommand::new("sh", &["-c", "echo nope >&2; exit 2"], "/"), Duration::from_secs(10)).unwrap();
        assert_eq!(failed.error(), "nope (exit code 2)");
        assert!(capture(&ProcessCommand::new("sleep", &["5"], "/"), Duration::from_millis(200)).is_err());
        assert!(capture(&ProcessCommand::new("/no/such/program", &[], "/"), Duration::from_secs(1)).is_err());
    }
}
