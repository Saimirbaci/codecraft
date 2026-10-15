//! View › Tool Windows › Zig (M13 track RC): the build steps of the project's `build.zig`
//! (`zig build -l`, listed on a worker thread since it compiles the build script), run a step
//! (`zig build <step>` in the Run tool window) and debug one that produces a program (`run`,
//! `install`: built, then the newest executable in `zig-out/bin` under lldb-dap).

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::debug::DebugTarget;
use crate::params::{opt_path, req_str};
use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

/// Most steps listed.
const MAX_STEPS: usize = 500;

/// One `zig build` step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZigStep {
    pub name: String,
    pub description: String,
    /// `zig build` without a step runs it.
    pub default: bool,
}

#[derive(Debug, Default)]
pub struct ZigState {
    /// The directory with `build.zig` the steps belong to.
    pub root: Option<PathBuf>,
    pub steps: Vec<ZigStep>,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: Option<usize>,
    rx: Option<Receiver<Result<Vec<ZigStep>, String>>>,
    /// The `zig` to run (tests point it at a fake); `None` = from `PATH`.
    pub program: Option<PathBuf>,
    /// `Session::isolate_home`: only `program`, never the user's `zig`.
    pub isolated: bool,
}

impl ZigState {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
}

/// Parse `zig build -l`: `  name (default)   description` per step.
pub fn parse_steps(text: &str) -> Vec<ZigStep> {
    let mut out: Vec<ZigStep> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.ends_with(':') || !line.starts_with([' ', '\t']) && t.contains(": ") {
            continue;
        }
        let name: String = t.chars().take_while(|c| !c.is_whitespace()).collect();
        if name.is_empty() || name.starts_with('-') || name.contains(['(', ')', '/', '\\']) {
            continue;
        }
        let rest = t.get(name.len()..).unwrap_or("").trim_start();
        let (default, rest) = match rest.strip_prefix("(default)") {
            Some(r) => (true, r.trim_start()),
            None => (false, rest),
        };
        if !out.iter().any(|s| s.name == name) {
            out.push(ZigStep { name, description: rest.trim().to_string(), default });
        }
        if out.len() >= MAX_STEPS {
            break;
        }
    }
    out
}

/// The `zig` to run.
fn zig(s: &Session) -> Result<PathBuf, String> {
    if let Some(p) = &s.zig.program {
        return Ok(p.clone());
    }
    if s.zig.isolated {
        return Err("zig is not available in this session".into());
    }
    codecraft_lsp::which("zig").ok_or_else(|| "zig is not installed (https://ziglang.org/download/)".into())
}

/// The build root: the given directory, else the project root (or the first child with a
/// `build.zig`).
fn build_root(s: &Session, p: &Value) -> Result<PathBuf, CommandError> {
    if let Some(dir) = opt_path(p, "dir") {
        let dir = crate::cpp::resolve_path(s, &dir);
        return if dir.join("build.zig").is_file() {
            Ok(dir)
        } else {
            Err(CommandError::BadParams(format!("{} has no build.zig", dir.display())))
        };
    }
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    project_build_root(&root).ok_or_else(|| CommandError::Failed("the project has no build.zig".into()))
}

/// The project's `build.zig` directory: the root, else a direct child.
pub fn project_build_root(root: &Path) -> Option<PathBuf> {
    if root.join("build.zig").is_file() {
        return Some(root.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("build.zig").is_file())
        .take(50)
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

/// Whether the Zig tool window applies (a `build.zig` in the project).
pub fn is_zig_project(s: &Session) -> bool {
    s.project_root().is_some_and(|r| project_build_root(&r).is_some())
}

/// List the steps on a worker thread.
pub(crate) fn refresh(s: &mut Session, root: PathBuf) -> Result<(), CommandError> {
    let zig = zig(s).map_err(CommandError::Failed)?;
    let (tx, rx) = crossbeam_channel::bounded(1);
    let dir = root.clone();
    std::thread::Builder::new()
        .name("codecraft-zig-steps".into())
        .spawn(move || {
            let out = std::process::Command::new(&zig)
                .args(["build", "-l"])
                .current_dir(&dir)
                .stdin(std::process::Stdio::null())
                .output();
            let r = match out {
                Ok(o) if o.status.success() => Ok(parse_steps(&String::from_utf8_lossy(&o.stdout))),
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    let first: String =
                        err.lines().filter(|l| !l.trim().is_empty()).take(6).collect::<Vec<_>>().join("\n");
                    Err(if first.is_empty() { format!("zig build -l failed ({})", o.status) } else { first })
                }
                Err(e) => Err(format!("cannot run {}: {e}", zig.display())),
            };
            let _ = tx.send(r);
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    s.zig.root = Some(root);
    s.zig.loading = true;
    s.zig.error = None;
    s.zig.rx = Some(rx);
    Ok(())
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(rx) = &s.zig.rx else { return false };
    let r = match rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("listing the build steps stopped unexpectedly".into()),
    };
    s.zig.rx = None;
    s.zig.loading = false;
    match r {
        Ok(steps) => {
            s.zig.steps = steps;
            s.zig.selected = s.zig.selected.filter(|i| *i < s.zig.steps.len());
        }
        Err(e) => s.zig.error = Some(e),
    }
    true
}

fn step_param(s: &Session, p: &Value) -> Result<String, CommandError> {
    let name = req_str(p, "step")?.trim().to_string();
    if name.is_empty() || name.starts_with('-') || name.chars().any(char::is_whitespace) {
        return Err(CommandError::BadParams(format!("`{name}` is not a build step")));
    }
    if !s.zig.steps.is_empty() && !s.zig.steps.iter().any(|st| st.name == name) {
        return Err(CommandError::BadParams(format!("build.zig has no step `{name}`")));
    }
    Ok(name)
}

fn run_step(s: &mut Session, p: &Value) -> CmdResult {
    let step = step_param(s, p)?;
    let root = build_root(s, p)?;
    let zig = zig(s).map_err(CommandError::Failed)?;
    let mut cmd = ProcessCommand::new(zig.display().to_string(), &["build", step.as_str()], root);
    cmd.build = step == "install";
    let title = format!("zig build {step}");
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    Ok(json!({ "tab": tab.0, "title": title }))
}

fn debug_step(s: &mut Session, p: &Value) -> CmdResult {
    let step = step_param(s, p)?;
    let root = build_root(s, p)?;
    zig(s).map_err(CommandError::Failed)?;
    let default = s.zig.steps.iter().any(|st| st.name == step && st.default);
    if !(step == "run" || step.starts_with("run") || step == "install" || default) {
        return Err(CommandError::Failed(format!(
            "`{step}` doesn't produce a program to debug (debug `run` or `install`: the newest executable in zig-out/bin)"
        )));
    }
    let args: Vec<String> = match p.get("args").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).take(200).collect(),
        None => Vec::new(),
    };
    let name = format!("zig build {step}");
    crate::debug::start(s, DebugTarget::Zig { name: name.clone(), root, exe: None, args })?;
    Ok(json!({ "debugging": name }))
}

fn state(s: &Session) -> Value {
    json!({
        "root": s.zig.root,
        "steps": s.zig.steps,
        "loading": s.zig.loading,
        "error": s.zig.error,
        "selected": s.zig.selected,
    })
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec("view.toolWindows.zig", "Zig", None, "", Context::Global, always, |s, _| {
            s.ws.layout.toggle(ToolWindowId::Zig);
            if s.ws.layout.is_visible(ToolWindowId::Zig) && s.zig.steps.is_empty() && !s.zig.loading {
                if let Some(root) = s.project_root().and_then(|r| project_build_root(&r)) {
                    if let Err(e) = refresh(s, root) {
                        s.zig.error = Some(e.to_string());
                    }
                } else {
                    s.zig.error = Some("The project has no build.zig".into());
                }
            }
            ok()
        }),
        spec(
            "zig.refresh",
            "Reload Zig Build Steps",
            None,
            "{dir?: path} — list the build steps of build.zig (`zig build -l`, in the background) → poll zig.steps",
            Context::Global,
            has_project,
            |s, p| {
                let root = build_root(s, p)?;
                refresh(s, root)?;
                s.ws.layout.show(ToolWindowId::Zig);
                ok()
            },
        ),
        spec(
            "zig.steps",
            "Zig Build Steps",
            None,
            "→ {root, steps: [{name, description, default}], loading, error}",
            Context::Global,
            always,
            |s, _| Ok(state(s)),
        ),
        spec("zig.select", "Select Zig Build Step", None, "{index: int}", Context::Global, always, |s, p| {
            let i = crate::params::req_usize(p, "index")?;
            if i >= s.zig.steps.len() {
                return Err(CommandError::BadParams(format!("no step {i}")));
            }
            s.zig.selected = Some(i);
            ok()
        }),
        spec(
            "zig.runStep",
            "Run Zig Build Step",
            None,
            "{step: string, dir?: path} — `zig build <step>` in the Run tool window → {tab}",
            Context::Global,
            has_project,
            run_step,
        ),
        spec(
            "zig.debugStep",
            "Debug Zig Build Step",
            None,
            "{step: string, dir?: path, args?: [string]} — build, then debug the newest executable in zig-out/bin \
             (steps that install or run a program)",
            Context::Global,
            has_project,
            debug_step,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zig_build_list() {
        let text = "Steps:\n  install (default)            Copy build artifacts to prefix path\n  uninstall                    Remove build artifacts from prefix path\n  run                          Run the app\n  test                         Run unit tests\n\nGeneral Options:\n  -p, --prefix [path]          Where to install files (default: zig-out)\n";
        let steps = parse_steps(text);
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["install", "uninstall", "run", "test"]);
        assert!(steps[0].default && !steps[2].default);
        assert_eq!(steps[2].description, "Run the app");
        // Older zig prints the steps without a heading.
        let bare = parse_steps("  install (default)  Copy\n  run  Run\n");
        assert_eq!(bare.len(), 2);
        assert!(parse_steps("").is_empty());
        assert!(parse_steps("error: no build.zig\n").is_empty());
    }
}
