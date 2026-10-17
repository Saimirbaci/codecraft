//! Debugging on a run target (M16 track RT): a configuration whose target is an SSH host or a
//! Docker image/container/Compose service is debugged there, as PyCharm's remote interpreters
//! and CLion's remote toolchains do.
//!
//! The session's phases (none blocks the UI thread):
//!
//! 1. **Preflight / remote build** (the session's Build phase): the project is synced (SSH),
//!    the target reports the project folder's absolute path ([`script::ROOT_MARK`]) and checks
//!    for the interpreter and debugpy, or a gdb-remote server (lldb-server, gdbserver,
//!    debugserver) — a missing tool fails the session with the command to install it there;
//!    Cargo builds and build steps run on the target (`cargo build --message-format=json`: the
//!    executable is read from its artifacts, as for local sessions).
//! 2. **Launch** (still the Build phase, now the launcher's process): `python -m debugpy
//!    --listen <port> --wait-for-client …` or `lldb-server gdbserver host:port -- exe args` /
//!    `gdbserver host:port exe args` starts on the target; the port is forwarded (`ssh -N -L`
//!    with the same port number, a published Docker port, or a `docker exec` socket relay); the
//!    script announces [`script::LISTEN_MARK`] once the port is open there. A native session's
//!    executable is copied back first (SSH) for its symbols.
//! 3. **Prepare / Live**: the local adapter connects: debugpy over the forward with path
//!    mappings (`pathMappings` local root ↔ the target's folder), lldb-dap with `gdb-remote` and
//!    `target.source-map`; breakpoints are sent with the target's paths. The launcher keeps
//!    running (a native program's output shows in the console) until the session ends;
//!    dropping the session kills it and the forward.

use std::path::{Path, PathBuf};

use codecraft_cargo::{RunEvent, RunHandle};
use codecraft_dap as dap;
use crossbeam_channel::Receiver;

use super::{
    ArtifactPick, BuildStep, DebugConsoleKind, DebugStatus, DebugTarget, LaunchPlan, Phase, RunSpec, fail,
    pick_artifact, set_status, spawn_prepare,
};
use crate::run_targets::RunTarget;
use crate::runs::ProcessCommand;
use crate::target_exec::{Ctx, Extras, Forward, RUN_DIR, script};
use crate::Session;

/// Lines of output kept for markers and error messages.
const KEEP_LINES: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Stage {
    Preflight,
    Launch,
    Live,
}

#[derive(Clone, Debug)]
enum Lang {
    Python {
        python: String,
        program: Option<PathBuf>,
        module: Option<String>,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        just_my_code: bool,
    },
    Native {
        /// A fixed program (local path); `None`: the Cargo build's artifact.
        program: Option<PathBuf>,
        /// The program is uploaded to the target (it isn't built there).
        upload: bool,
        /// The Cargo target to pick among the build's executables.
        pick: Option<String>,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
        rust: bool,
    },
}

/// A debug session's target side (owned by its `Runtime`).
pub(crate) struct OnTarget {
    ctx: Ctx,
    lang: Lang,
    stage: Stage,
    /// The debug server's port on the target.
    port: u16,
    root_abs: Option<String>,
    lines: Vec<String>,
    /// The launcher (debug server) once the session is live.
    launcher: Option<(RunHandle, Receiver<RunEvent>)>,
    forward: Option<Forward>,
    /// The program's paths on the target and here (native sessions).
    remote_program: Option<String>,
    local_program: Option<PathBuf>,
}

impl std::fmt::Debug for OnTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnTarget").field("target", &self.ctx.label).field("stage", &self.stage).finish()
    }
}

impl Drop for OnTarget {
    fn drop(&mut self) {
        if let Some((h, _)) = self.launcher.take() {
            let _ = h.kill();
        }
        self.forward = None;
    }
}

impl OnTarget {
    /// The target's label (`SSH: web`).
    pub(crate) fn label(&self) -> &str {
        &self.ctx.label
    }

    fn push_line(&mut self, line: &str) {
        if self.lines.len() >= KEEP_LINES {
            self.lines.remove(0);
        }
        self.lines.push(line.to_string());
    }

    fn missing(&self) -> Option<String> {
        self.lines.iter().rev().find_map(|l| script::marker(l, script::MISSING_MARK)).map(str::to_string)
    }

    fn last_line(&self) -> String {
        self.lines.iter().rev().find(|l| !l.trim().is_empty() && !l.contains("CODECRAFT-")).cloned().unwrap_or_default()
    }

    /// A local path under the project as the same path on the target (absolute once the
    /// target's folder is known).
    fn remote_path(&self, local: &Path) -> String {
        let rel = codecraft_remote::deploy::rel_path(&self.ctx.local_root, local);
        match (&self.root_abs, rel) {
            (Some(root), Some(rel)) if rel.is_empty() => root.clone(),
            (Some(root), Some(rel)) => format!("{}/{rel}", root.trim_end_matches('/')),
            (None, Some(_)) => self.ctx.to_remote(local).unwrap_or_else(|| local.display().to_string()),
            (_, None) => local.display().to_string(),
        }
    }

    fn remote_word(&self, word: &str) -> String {
        let (prefix, value) = match word.split_once('=') {
            Some((k, v)) if k.starts_with('-') => (format!("{k}="), v),
            _ => (String::new(), word),
        };
        let p = Path::new(value);
        if p.is_absolute() && p.starts_with(&self.ctx.local_root) {
            format!("{prefix}{}", self.remote_path(p))
        } else {
            word.to_string()
        }
    }

    /// Native sessions: the project root here and there (breakpoints are sent with the
    /// target's paths; frames come back mapped by `target.source-map`, or here).
    pub(crate) fn native_map(&self) -> Option<(&Path, &str)> {
        match (&self.lang, &self.root_abs) {
            (Lang::Native { .. }, Some(root)) => Some((self.ctx.local_root.as_path(), root.as_str())),
            _ => None,
        }
    }

    /// A Rust program (values get Rust's formatting clean-up).
    pub(crate) fn is_rust(&self) -> bool {
        matches!(self.lang, Lang::Native { rust: true, .. })
    }

    /// A debugpy subprocess's adapter port on the target as the port to connect to here.
    pub(crate) fn local_port(&self, remote: u16) -> u16 {
        match &self.forward {
            Some(f) if f.remote_port == remote => f.local_port,
            _ => remote,
        }
    }
}

/// A local path as the adapter sees it on the target (native sessions on a target).
pub(crate) fn to_target(map: (&Path, &str), path: &Path) -> Option<PathBuf> {
    let (local_root, remote_root) = map;
    let canonical = std::fs::canonicalize(local_root).ok();
    let rest = path
        .strip_prefix(local_root)
        .ok()
        .or_else(|| canonical.as_deref().and_then(|c| path.strip_prefix(c).ok()))?;
    Some(Path::new(remote_root).join(rest))
}

/// A path the adapter reported on the target, as the local path.
pub(crate) fn from_target(map: (&Path, &str), path: &Path) -> Option<PathBuf> {
    let (local_root, remote_root) = map;
    let rest = path.strip_prefix(remote_root).ok()?;
    Some(local_root.join(rest))
}

fn bad(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The session plan for debugging `inner` on `target`, and its target side.
pub(super) fn plan(
    s: &mut Session,
    name: &str,
    target: &RunTarget,
    inner: &DebugTarget,
) -> Result<(LaunchPlan, OnTarget), String> {
    let ctx = Ctx::resolve(s, target).map_err(bad)?;
    let port = crate::target_exec::free_port()?;
    let root = ctx.local_root.clone();
    let mut extras = Extras { sync: true, ..Default::default() };
    let preflight_head = |cwd: &str, env: &[(String, String)]| -> Result<String, String> {
        let mut out = script::prelude(cwd, env).map_err(bad)?;
        out.push_str(&script::print_root(&ctx.remote_root));
        Ok(out)
    };
    let (lang, script_text, cargo, run) = match inner {
        DebugTarget::Python { program, module, args, cwd, env, python, just_my_code, .. } => {
            if program.is_none() && module.is_none() {
                return Err("a Python target needs a program or a module".into());
            }
            let py = ctx.python_for(python.as_deref());
            let cwd = cwd
                .clone()
                .or_else(|| program.as_ref().and_then(|p| p.parent().map(Path::to_path_buf)))
                .unwrap_or_else(|| root.clone());
            let mut text = preflight_head(&ctx.remote_root, &[])?;
            text.push_str(&script::require_program(&py, &script::python_missing_message(&py, &ctx.label)));
            text.push_str(&script::require_python_module(&py, "debugpy", &script::debugpy_missing_message(&py, &ctx.label)));
            text.push_str("exit 0");
            let lang = Lang::Python {
                python: py,
                program: program.clone(),
                module: module.clone(),
                args: args.clone(),
                cwd,
                env: env.clone(),
                just_my_code: *just_my_code,
            };
            let run = RunSpec::PythonRemote { host: "127.0.0.1".into(), port, mappings: Vec::new(), just_my_code: *just_my_code };
            (lang, text, false, run)
        }
        DebugTarget::Cargo { name: cname, invocation } => {
            let local = super::cargo_plan(cname, invocation)?;
            let build = local.build.as_ref().ok_or("nothing to build")?;
            let pick = match &build.pick {
                ArtifactPick::Cargo { name } => name.clone(),
                _ => None,
            };
            let cmd = ProcessCommand {
                program: "cargo".into(),
                args: build.args.clone(),
                cwd: build.cwd.clone(),
                env: build.env.clone(),
                build: true,
            };
            let rcwd = ctx.to_remote(&cmd.cwd).unwrap_or_else(|| ctx.remote_root.clone());
            let mut text = preflight_head(&rcwd, &ctx.env_for(&cmd))?;
            text.push_str(&script::require_debug_server(&ctx.label));
            text.push_str(&script::require_program("cargo", &format!(
                "cargo was not found on {}: install Rust there (https://rustup.rs) or use a target image that has it",
                ctx.label
            )));
            text.push_str("exec cargo");
            for a in &cmd.args {
                text.push(' ');
                text.push_str(&codecraft_remote::quote::sh_quote(&ctx.map_word(a)));
            }
            let lang = Lang::Native {
                program: None,
                upload: false,
                pick,
                args: local.args.clone(),
                cwd: local.cwd.clone().unwrap_or_else(|| root.clone()),
                env: local.env.clone(),
                rust: true,
            };
            (lang, text, true, RunSpec::Remote { address: format!("127.0.0.1:{port}"), program: None })
        }
        DebugTarget::Executable { program, args, cwd, env, build, .. } => {
            let cwd = cwd.clone().or_else(|| program.parent().map(Path::to_path_buf)).unwrap_or_else(|| root.clone());
            let rcwd = ctx.to_remote(&cwd).unwrap_or_else(|| ctx.remote_root.clone());
            let mut text = preflight_head(&rcwd, env)?;
            text.push_str(&script::require_debug_server(&ctx.label));
            let in_project = program.starts_with(&root);
            let upload = build.is_empty() && (ctx.is_ssh() || !in_project);
            if upload {
                if !program.is_file() {
                    return Err(format!("{} does not exist", program.display()));
                }
                let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "program".into());
                let remote = format!("{RUN_DIR}/bin/{name}");
                extras.uploads.push((program.clone(), remote.clone()));
                text.push_str(&format!("chmod +x {} 2>/dev/null; ", codecraft_remote::quote::quote_path(&codecraft_remote::command::join(&ctx.remote_root, &remote))));
            }
            match build.split_first() {
                Some((b, rest)) => {
                    text.push_str("exec ");
                    text.push_str(&codecraft_remote::quote::sh_quote(&ctx.remote_program(b)));
                    for a in rest {
                        text.push(' ');
                        text.push_str(&codecraft_remote::quote::sh_quote(&ctx.map_word(a)));
                    }
                }
                None => text.push_str("exit 0"),
            }
            let fixed = if upload {
                let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "program".into());
                root.join(RUN_DIR).join("bin").join(name)
            } else {
                program.clone()
            };
            let lang = Lang::Native {
                program: Some(fixed),
                upload,
                pick: None,
                args: args.clone(),
                cwd,
                env: env.clone(),
                rust: false,
            };
            (lang, text, false, RunSpec::Remote { address: format!("127.0.0.1:{port}"), program: None })
        }
        other => {
            return Err(format!(
                "{} can't be debugged on {}: Python programs and tests, Cargo targets and native programs can",
                other.name(),
                ctx.label
            ));
        }
    };
    let wrapped = ctx.process(&script_text, &extras)?;
    let build = BuildStep {
        program: PathBuf::from(&wrapped.program),
        args: wrapped.args.clone(),
        cwd: wrapped.cwd.clone(),
        env: wrapped.env.clone(),
        cargo,
        pick: ArtifactPick::Cargo { name: None },
    };
    let plan = LaunchPlan { name: name.to_string(), build: Some(build), run, args: Vec::new(), cwd: None, env: Vec::new() };
    let on = OnTarget {
        ctx,
        lang,
        stage: Stage::Preflight,
        port,
        root_abs: None,
        lines: Vec::new(),
        launcher: None,
        forward: None,
        remote_program: None,
        local_program: None,
    };
    Ok((plan, on))
}

/// The first console line of a session on a target.
pub(super) fn describe(on: &OnTarget) -> String {
    match &on.lang {
        Lang::Python { python, .. } => format!("Preparing to debug on {} with {python} (debugpy)…", on.ctx.label),
        Lang::Native { pick: Some(_), .. } | Lang::Native { program: None, .. } => {
            format!("Building on {} (cargo), then debugging there through a gdb-remote server…", on.ctx.label)
        }
        Lang::Native { .. } => format!("Preparing to debug on {} through a gdb-remote server…", on.ctx.label),
    }
}

fn on_mut(s: &mut Session) -> Option<&mut OnTarget> {
    s.debug.rt.as_mut().and_then(|rt| rt.on_target.as_deref_mut())
}

/// A build-phase event of a session on a target (preflight, remote build or the launcher).
pub(super) fn on_build_event(s: &mut Session, event: RunEvent) {
    let stage = match on_mut(s) {
        Some(on) => on.stage.clone(),
        None => return,
    };
    match event {
        RunEvent::Stdout(line) | RunEvent::Stderr(line) => {
            let mut live = false;
            if let Some(on) = on_mut(s) {
                on.push_line(&line);
                live = stage == Stage::Launch && script::marker(&line, script::LISTEN_MARK).is_some();
            }
            if !line.contains("CODECRAFT-") {
                let kind = if stage == Stage::Launch { DebugConsoleKind::Stdout } else { DebugConsoleKind::Info };
                s.debug.push_console(kind, line);
            }
            if live {
                go_live(s);
            }
        }
        RunEvent::Exited { success, code, .. } => {
            if s.debug.rt.as_ref().is_some_and(|rt| rt.stopping) {
                return;
            }
            let code_text = code.map_or("a signal".to_string(), |c| c.to_string());
            let Some(on) = on_mut(s) else { return };
            let label = on.ctx.label.clone();
            match stage {
                Stage::Preflight if success => {
                    let executables = match s.debug.rt.as_ref().map(|rt| &rt.phase) {
                        Some(Phase::Build { executables, .. }) => executables.clone(),
                        _ => Vec::new(),
                    };
                    if let Err(e) = launch(s, &executables) {
                        fail(s, &e);
                    }
                }
                Stage::Preflight => {
                    let msg = on.missing().unwrap_or_else(|| {
                        let last = on.last_line();
                        format!(
                            "preparing {label} failed (exit code {code_text}){}",
                            if last.is_empty() { String::new() } else { format!(": {last}") }
                        )
                    });
                    fail(s, &msg);
                }
                Stage::Launch => {
                    let msg = on.missing().unwrap_or_else(|| {
                        let last = on.last_line();
                        format!(
                            "the debug server on {label} exited (exit code {code_text}) before it listened{}",
                            if last.is_empty() { String::new() } else { format!(": {last}") }
                        )
                    });
                    fail(s, &msg);
                }
                Stage::Live => {}
            }
        }
        // Compiler artifacts and messages of a remote Cargo build: as for local builds.
        other => super::on_local_build_event(s, other),
    }
}

/// The preflight (and remote build) succeeded: start the debug server on the target and the
/// forward.
fn launch(s: &mut Session, executables: &[(String, PathBuf)]) -> Result<(), String> {
    let Some(on) = on_mut(s) else { return Ok(()) };
    let root_abs = on.ctx.root_abs(on.lines.iter().map(String::as_str)).ok_or_else(|| {
        format!("{} did not report where the project is (does `{}` exist there?)", on.ctx.label, on.ctx.remote_root)
    })?;
    on.root_abs = Some(root_abs.clone());
    let host = on.ctx.listen_host();
    let mut extras = Extras { ports: vec![on.port], ..Default::default() };
    let text = match on.lang.clone() {
        Lang::Python { python, program, module, args, cwd, env, .. } => {
            let mut t = script::prelude(&on.remote_path(&cwd), &env).map_err(bad)?;
            let script_path = program.as_deref().map(|p| on.remote_path(p));
            let args: Vec<String> = args.iter().map(|a| on.remote_word(a)).collect();
            t.push_str(
                &script::debugpy_server(&python, host, on.port, script_path.as_deref(), module.as_deref(), &args, &on.ctx.label)
                    .map_err(bad)?,
            );
            t
        }
        Lang::Native { program, upload, pick, args, cwd, env, .. } => {
            let remote_exe = match &program {
                Some(p) => on.remote_path(p),
                None => {
                    // The remote Cargo build's artifact (a path on the target).
                    let exe = pick_artifact(&ArtifactPick::Cargo { name: pick.clone() }, executables)?;
                    exe.display().to_string()
                }
            };
            // A local copy for symbols: the program itself (uploaded), the Docker mount, or a
            // download from the SSH host.
            let local = match &program {
                Some(p) if upload => p.clone(),
                _ => match on.ctx.to_local(&remote_exe, &root_abs).filter(|p| !on.ctx.is_ssh() && p.is_file()) {
                    Some(p) => p,
                    None => {
                        let name = Path::new(&remote_exe).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "program".into());
                        let local = on.ctx.local_root.join(RUN_DIR).join("bin").join(name);
                        if let Some(dir) = local.parent() {
                            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                        }
                        let _ = std::fs::remove_file(&local);
                        extras.before.push((remote_exe.clone(), local.clone()));
                        // A macOS host keeps the debug info next to the program.
                        let mut dsym = local.clone().into_os_string();
                        dsym.push(".dSYM");
                        extras.before_optional.push((format!("{remote_exe}.dSYM"), PathBuf::from(dsym)));
                        local
                    }
                },
            };
            on.remote_program = Some(remote_exe.clone());
            on.local_program = Some(local);
            let args: Vec<String> = args.iter().map(|a| on.remote_word(a)).collect();
            let mut t = script::prelude(&on.remote_path(&cwd), &env).map_err(bad)?;
            t.push_str(&script::gdb_server(host, on.port, &remote_exe, &args, &on.ctx.label));
            t
        }
    };
    let cmd = on.ctx.process(&text, &extras)?;
    let forward = on.ctx.forward(on.port)?;
    let local_port = forward.local_port;
    on.forward = Some(forward);
    on.stage = Stage::Launch;
    let label = on.ctx.label.clone();
    let (local_root, lang) = (on.ctx.local_root.clone(), on.lang.clone());
    let local_program = on.local_program.clone();
    let (handle, events) = codecraft_cargo::spawn_process(Path::new(&cmd.program), &cmd.args, &cmd.cwd, &cmd.env, false)
        .map_err(|e| format!("cannot start the debug server on {label}: {e}"))?;
    let Some(rt) = s.debug.rt.as_mut() else { return Ok(()) };
    // The final plan: what the local adapter connects to.
    rt.plan.run = match lang {
        Lang::Python { just_my_code, .. } => RunSpec::PythonRemote {
            host: "127.0.0.1".into(),
            port: local_port,
            mappings: vec![dap::launch::PathMapping {
                local_root: local_root.display().to_string(),
                remote_root: root_abs.clone(),
            }],
            just_my_code,
        },
        Lang::Native { .. } => {
            rt.options.init_commands.push(format!(
                "settings set target.source-map {} {}",
                lldb_word(&root_abs),
                lldb_word(&local_root.display().to_string())
            ));
            RunSpec::Remote { address: format!("127.0.0.1:{local_port}"), program: local_program }
        }
    };
    rt.phase = Phase::Build { handle, events, executables: Vec::new() };
    set_status(s, DebugStatus::Starting);
    s.debug.info(format!("Starting the debug server on {label} (port {})…", local_port));
    Ok(())
}

/// A word of an LLDB command (quoted when it has spaces).
fn lldb_word(w: &str) -> String {
    if w.contains([' ', '"', '\\']) { format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\"")) } else { w.to_string() }
}

/// The debug server listens: connect the local adapter (the launcher keeps running).
fn go_live(s: &mut Session) {
    let Some(rt) = s.debug.rt.as_mut() else { return };
    let phase = std::mem::replace(&mut rt.phase, Phase::Prepare { rx: crossbeam_channel::never() });
    let Phase::Build { handle, events, .. } = phase else {
        rt.phase = phase;
        return;
    };
    let plan = rt.plan.clone();
    let options = rt.options.clone();
    let program = match &plan.run {
        RunSpec::Remote { program, .. } => program.clone(),
        _ => None,
    };
    rt.phase = Phase::Prepare { rx: spawn_prepare(plan, program, options) };
    if let Some(on) = rt.on_target.as_deref_mut() {
        on.launcher = Some((handle, events));
        on.stage = Stage::Live;
    }
    s.debug.info("The debug server is listening; attaching…".to_string());
}

/// Drain the launcher of a live session on a target. True if anything happened.
pub(super) fn pump(s: &mut Session) -> bool {
    let Some(on) = on_mut(s) else { return false };
    let python = matches!(on.lang, Lang::Python { .. });
    let Some((_, events)) = on.launcher.as_ref() else { return false };
    let batch: Vec<RunEvent> = events.try_iter().take(2000).collect();
    if batch.is_empty() {
        if let Some(f) = on.forward.as_mut()
            && let Some(why) = f.failure()
        {
            on.forward = None;
            s.debug.error(why);
            return true;
        }
        return false;
    }
    let label = on.ctx.label.clone();
    for event in batch {
        match event {
            // debugpy sends the program's output itself (redirectOutput).
            RunEvent::Stdout(line) if !python && !line.contains("CODECRAFT-") => {
                s.debug.push_console(DebugConsoleKind::Stdout, line);
            }
            RunEvent::Stderr(line) if !python && !line.contains("CODECRAFT-") => {
                s.debug.push_console(DebugConsoleKind::Stderr, line);
            }
            RunEvent::Exited { code, .. } => {
                if let Some(on) = on_mut(s) {
                    on.launcher = None;
                }
                s.debug.info(format!(
                    "The debug server on {label} ended{}",
                    code.map_or(String::new(), |c| format!(" (exit code {c})"))
                ));
            }
            _ => {}
        }
    }
    true
}

/// Whether a debug target starts a program (so it can run on a target); attaching to a
/// process, a core dump and remote debug servers stay as they are.
fn starts_program(t: &DebugTarget) -> bool {
    matches!(t, DebugTarget::Cargo { .. } | DebugTarget::Executable { .. } | DebugTarget::Python { .. })
}

/// `inner` (as the configuration or caret says) debugged on the effective target: the
/// command's `target` param, else `configured`; `inner` itself on the local machine or when it
/// doesn't start a program.
pub(crate) fn wrap(s: &Session, configured: &RunTarget, inner: DebugTarget) -> DebugTarget {
    match crate::target_exec::effective(s, configured) {
        RunTarget::Local => inner,
        _ if !starts_program(&inner) => inner,
        target => DebugTarget::OnTarget { name: inner.name().to_string(), target, debug: Box::new(inner) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_map_both_ways() {
        let map = (Path::new("/p/app"), "/srv/app");
        assert_eq!(to_target(map, Path::new("/p/app/src/main.c")), Some(PathBuf::from("/srv/app/src/main.c")));
        assert_eq!(to_target(map, Path::new("/elsewhere/x.c")), None);
        assert_eq!(from_target(map, Path::new("/srv/app/src/main.c")), Some(PathBuf::from("/p/app/src/main.c")));
        assert_eq!(from_target(map, Path::new("/usr/include/stdio.h")), None);
        assert_eq!(lldb_word("/a b"), "\"/a b\"");
        assert_eq!(lldb_word("/ab"), "/ab");
    }
}
