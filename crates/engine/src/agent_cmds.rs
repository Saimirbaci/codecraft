//! Coding-agent commands: start/stop/follow up jobs, review their changes, answer approvals.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_agent::Backend;

use crate::agents::{self, JobId, StartRequest, mode_id, mode_support, parse_mode};
use crate::approvals::{AgentMode, ApprovalId, Decision};
use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::{bool_or, opt_str, req_str, req_usize};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "agent.start",
            "Start Agent Job",
            None,
            "{backend: \"claude\"|\"codex\"|\"grok\"|\"gemini\", mode: \"auto\"|\"ask\"|\"review\", prompt: string, \
             attachments?: [path], cwd?: path, title?: string, includeActiveFile?: bool (default true), \
             includeSelection?: bool (default true)} → {job}",
            Context::Global,
            always,
            start,
        ),
        spec("agent.stop", "Stop Agent Job", None, "{job: int}", Context::Global, always, |s, p| {
            agents::stop(s, job(p)?)?;
            Ok(Value::Null)
        }),
        spec(
            "agent.followUp",
            "Agent Follow-Up",
            None,
            "{job: int, prompt: string} — continue the job's session",
            Context::Global,
            always,
            |s, p| {
                let prompt = req_str(p, "prompt")?.to_string();
                agents::follow_up(s, job(p)?, &prompt)?;
                Ok(Value::Null)
            },
        ),
        spec(
            "agent.review.accept",
            "Accept Agent Change",
            None,
            "{job: int, path: string} — write the agent's version of one file into the project",
            Context::Global,
            always,
            |s, p| decide(s, p, true, true),
        ),
        spec(
            "agent.review.reject",
            "Reject Agent Change",
            None,
            "{job: int, path: string}",
            Context::Global,
            always,
            |s, p| decide(s, p, true, false),
        ),
        spec(
            "agent.review.acceptAll",
            "Accept All Agent Changes",
            None,
            "{job: int}",
            Context::Global,
            always,
            |s, p| decide(s, p, false, true),
        ),
        spec(
            "agent.review.rejectAll",
            "Reject All Agent Changes",
            None,
            "{job: int}",
            Context::Global,
            always,
            |s, p| decide(s, p, false, false),
        ),
        spec(
            "agent.discard",
            "Discard Agent Job",
            None,
            "{job: int} — stop it, remove its worktree and branch, forget it",
            Context::Global,
            always,
            |s, p| {
                agents::discard(s, job(p)?)?;
                Ok(Value::Null)
            },
        ),
        spec(
            "agent.approve",
            "Answer Agent Approval",
            None,
            "{id: int, decision: \"allow\"|\"allowAlways\"|\"deny\"} — answer a pending approval from any source",
            Context::Global,
            always,
            approve,
        ),
        spec(
            "agent.backends",
            "List Agent Backends",
            None,
            "{refresh?: bool} → [{id, label, installed, path, modes, unavailable: {mode: reason}, installHint}]",
            Context::Global,
            always,
            backends,
        ),
        spec("agent.jobs", "List Agent Jobs", None, "→ [job summary]", Context::Global, always, |s, _| {
            Ok(Value::Array(s.agents.jobs.iter().map(agents::Job::summary).collect()))
        }),
        spec(
            "agent.job",
            "Agent Job Details",
            None,
            "{job: int} → the job with its activity feed and review",
            Context::Global,
            always,
            |s, p| {
                let id = job(p)?;
                s.agents.job(id).map(agents::Job::detail).ok_or_else(|| no_job(id))
            },
        ),
        spec(
            "agent.settings",
            "Agent Settings",
            None,
            "{scrubApiKeys?: bool, worktreesDir?: path, binaries?: {backend: path}, acp?: {backend: [program, args…]}} \
             → the settings",
            Context::Global,
            always,
            settings,
        ),
    ]
}

fn no_job(id: JobId) -> CommandError {
    CommandError::BadParams(format!("no agent job {}", id.0))
}

fn job(p: &Value) -> Result<JobId, CommandError> {
    let n = req_usize(p, "job")?;
    Ok(JobId(u64::try_from(n).unwrap_or(u64::MAX)))
}

fn backend(p: &Value) -> Result<Backend, CommandError> {
    let id = req_str(p, "backend")?;
    Backend::from_id(id)
        .ok_or_else(|| CommandError::BadParams(format!("unknown backend `{id}` (claude, codex, grok, gemini)")))
}

fn start(s: &mut Session, p: &Value) -> CmdResult {
    let backend = backend(p)?;
    let mode = opt_str(p, "mode").unwrap_or("review");
    let mode = parse_mode(mode)
        .ok_or_else(|| CommandError::BadParams(format!("unknown mode `{mode}` (auto, ask, review)")))?;
    let prompt = req_str(p, "prompt")?.to_string();
    let attachments = match p.get("attachments") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str().map(PathBuf::from).ok_or_else(|| CommandError::BadParams("attachments must be paths".into()))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(CommandError::BadParams("attachments must be an array of paths".into())),
    };
    let req = StartRequest {
        backend,
        mode,
        prompt,
        attachments,
        cwd: crate::params::opt_path(p, "cwd"),
        title: opt_str(p, "title").map(str::to_string),
        include_file: bool_or(p, "includeActiveFile", true),
        include_selection: bool_or(p, "includeSelection", true),
    };
    let id = agents::start(s, req)?;
    Ok(json!({ "job": id.0 }))
}

/// `one`: a single file (`path` param) or all pending ones.
fn decide(s: &mut Session, p: &Value, one: bool, accept: bool) -> CmdResult {
    let id = job(p)?;
    let path = if one { Some(req_str(p, "path")?.to_string()) } else { None };
    let n = agents::review_decide(s, id, path.as_deref(), accept)?;
    Ok(json!({ "decided": n }))
}

fn approve(s: &mut Session, p: &Value) -> CmdResult {
    let id = ApprovalId(u64::try_from(req_usize(p, "id")?).unwrap_or(u64::MAX));
    let decision = match req_str(p, "decision")? {
        "allow" => Decision::Allow,
        "allowAlways" => Decision::AllowAlways,
        "deny" => Decision::Deny,
        other => return Err(CommandError::BadParams(format!("unknown decision `{other}` (allow, allowAlways, deny)"))),
    };
    if !s.approvals.decide(id, decision) {
        return Err(CommandError::Failed(format!("approval {} is not pending", id.0)));
    }
    Ok(Value::Null)
}

fn backends(s: &mut Session, p: &Value) -> CmdResult {
    if bool_or(p, "refresh", false) {
        s.agents.refresh_backends();
    }
    let list: Vec<Value> = s
        .agents
        .backends()
        .iter()
        .map(|info| {
            let mut modes = Vec::new();
            let mut unavailable = serde_json::Map::new();
            for m in [AgentMode::Auto, AgentMode::Ask, AgentMode::Review] {
                match mode_support(info, m) {
                    Ok(()) => modes.push(mode_id(m)),
                    Err(why) => {
                        unavailable.insert(mode_id(m).into(), json!(why));
                    }
                }
            }
            json!({
                "id": info.backend.id(),
                "label": info.backend.label(),
                "installed": info.installed(),
                "path": info.path,
                "acp": info.acp,
                "modes": modes,
                "unavailable": unavailable,
                "installHint": info.backend.install_hint(),
            })
        })
        .collect();
    Ok(Value::Array(list))
}

fn settings(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(v) = p.get("scrubApiKeys") {
        s.agents.settings.scrub_api_keys =
            v.as_bool().ok_or_else(|| CommandError::BadParams("scrubApiKeys must be a boolean".into()))?;
    }
    if let Some(v) = p.get("worktreesDir") {
        s.agents.settings.worktrees_dir = match v {
            Value::Null => None,
            Value::String(d) if !d.is_empty() => Some(crate::normalize_path(std::path::Path::new(d))),
            _ => return Err(CommandError::BadParams("worktreesDir must be a path or null".into())),
        };
    }
    if let Some(map) = p.get("binaries").and_then(Value::as_object) {
        for (k, v) in map {
            let b = Backend::from_id(k).ok_or_else(|| CommandError::BadParams(format!("unknown backend `{k}`")))?;
            match v.as_str().filter(|s| !s.is_empty()) {
                Some(path) => s.agents.settings.overrides.binaries.insert(b, PathBuf::from(path)),
                None => s.agents.settings.overrides.binaries.remove(&b),
            };
        }
        s.agents.refresh_backends();
    }
    if let Some(map) = p.get("acp").and_then(Value::as_object) {
        for (k, v) in map {
            let b = Backend::from_id(k).ok_or_else(|| CommandError::BadParams(format!("unknown backend `{k}`")))?;
            let cmd: Vec<String> = v
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            if cmd.is_empty() {
                s.agents.settings.overrides.acp.remove(&b);
            } else {
                s.agents.settings.overrides.acp.insert(b, cmd);
            }
        }
        s.agents.refresh_backends();
    }
    serde_json::to_value(&s.agents.settings).map_err(|e| CommandError::Failed(e.to_string()))
}
