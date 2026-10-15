//! Managed analyzer installs: basedpyright (and mypy) with `uv tool install` into
//! `$CODECRAFT_HOME/tools` (`UV_TOOL_DIR=tools/uv`, `UV_TOOL_BIN_DIR=tools/bin`), only when the
//! user (or an agent) asks with `confirm: true`. Nothing is installed globally; Inspect Code
//! finds the managed copy after the project's and `PATH`'s.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossbeam_channel::{Receiver, TryRecvError};
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_workspace::NotificationLevel;

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::{bool_or, req_str};
use crate::{CommandError, Session};

/// Tools that can be installed this way (uv package = executable name).
pub const INSTALLABLE: [&str; 2] = ["basedpyright", "mypy"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallOutcome {
    pub tool: String,
    pub ok: bool,
    pub message: String,
}

pub struct InstallJob {
    pub tool: String,
    rx: Receiver<Result<String, String>>,
    #[allow(dead_code)]
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct InstallState {
    pub running: Option<InstallJob>,
    pub last: Option<InstallOutcome>,
    /// The `uv` program (tests point it at a fake; isolated sessions have no other).
    pub uv_override: Option<PathBuf>,
}

impl std::fmt::Debug for InstallState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallState")
            .field("running", &self.running.as_ref().map(|r| r.tool.clone()))
            .field("last", &self.last)
            .finish()
    }
}

fn tools_dir(s: &Session) -> Result<PathBuf, String> {
    s.analysis.profiles.home.require("tools")
}

fn uv(s: &Session) -> Option<PathBuf> {
    if let Some(p) = &s.analysis.install.uv_override {
        return Some(p.clone());
    }
    if s.cpp.isolated {
        return None;
    }
    codecraft_lsp::which("uv")
}

/// Whether a managed copy of the tool exists.
pub fn installed(s: &Session, tool: &str) -> Option<PathBuf> {
    let p = tools_dir(s).ok()?.join("bin").join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    p.is_file().then_some(p)
}

fn install_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let tool = req_str(p, "tool")?.trim().to_ascii_lowercase();
    if !INSTALLABLE.contains(&tool.as_str()) {
        return Err(CommandError::BadParams(format!("'{tool}' cannot be installed here ({})", INSTALLABLE.join(", "))));
    }
    let dir = tools_dir(s).map_err(CommandError::Failed)?;
    let args = vec!["tool".to_string(), "install".to_string(), tool.clone()];
    let shown = format!("UV_TOOL_DIR={0}/uv UV_TOOL_BIN_DIR={0}/bin uv {1}", dir.display(), args.join(" "));
    if !bool_or(p, "confirm", false) {
        return Ok(json!({
            "needsConfirmation": true,
            "tool": tool,
            "command": shown,
            "installDir": dir.join("bin"),
            "installed": installed(s, &tool),
        }));
    }
    if s.analysis.install.running.is_some() {
        return Err(CommandError::Failed("an install is already running".into()));
    }
    let uv = uv(s).ok_or_else(|| CommandError::Failed("uv is not installed (https://docs.astral.sh/uv/)".into()))?;
    std::fs::create_dir_all(&dir).map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    let env = vec![
        ("UV_TOOL_DIR".to_string(), dir.join("uv").display().to_string()),
        ("UV_TOOL_BIN_DIR".to_string(), dir.join("bin").display().to_string()),
    ];
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let name = tool.clone();
    let bin = dir.join("bin").join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    std::thread::Builder::new()
        .name("codecraft-install".into())
        .spawn(move || {
            let r = super::analyzers::run_capture(&uv, &args, &dir, &env, &flag).and_then(|out| {
                if out.success && bin.is_file() {
                    Ok(format!("{name} installed in {}", bin.display()))
                } else if out.success {
                    Err(format!("uv finished but {} is missing", bin.display()))
                } else {
                    Err(out.summary())
                }
            });
            let _ = tx.send(r);
        })
        .map_err(|e| CommandError::Failed(format!("cannot start the install: {e}")))?;
    s.analysis.install.running = Some(InstallJob { tool: tool.clone(), rx, cancel });
    s.status = format!("Installing {tool}…");
    Ok(json!({ "started": true, "tool": tool, "command": shown }))
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(job) = &s.analysis.install.running else { return false };
    let result = match job.rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("the install stopped unexpectedly".into()),
    };
    let tool = job.tool.clone();
    s.analysis.install.running = None;
    let (ok, message) = match result {
        Ok(m) => (true, m),
        Err(e) => (false, format!("installing {tool} failed: {e}")),
    };
    s.status = message.clone();
    s.notify(if ok { NotificationLevel::Info } else { NotificationLevel::Error }, "Install", message.clone());
    // The managed basedpyright is the Python language server from now on (M15 EA).
    if ok && tool == "basedpyright" {
        s.restart_python_server();
    }
    s.analysis.install.last = Some(InstallOutcome { tool, ok, message });
    true
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "analysis.installTool",
        "Install Analyzer",
        None,
        "{tool: \"basedpyright\"|\"mypy\", confirm?: bool} — install with `uv tool install` into $CODECRAFT_HOME/tools (never globally); without `confirm: true` only describes what it would run",
        Context::Global,
        always,
        install_cmd,
    )]
}
