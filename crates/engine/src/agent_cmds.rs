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
             includeSelection?: bool (default true)} → {job, session} (`session`: the saved session's id, \
             see agent.sessions.*)",
            Context::Global,
            always,
            start,
        ),
        spec(
            "agent.sendToChat",
            "Send to Chat",
            None,
            "{text?: string} — queue the editor's selection (or `text`) as a code snippet in the Agent prompt and \
             show the Agent tool window → {queued}",
            Context::Editor,
            has_chat_text,
            |s, p| {
                let snippet = match opt_str(p, "text").filter(|t| !t.trim().is_empty()) {
                    Some(t) => format!("```\n{}\n```\n", t.trim_end_matches('\n')),
                    None => agents::selection_snippet(s)
                        .ok_or_else(|| CommandError::Failed("select some text to send to the chat".into()))?,
                };
                s.agents.chat_inbox.push(snippet);
                s.ws.layout.show(codecraft_workspace::ToolWindowId::Agent);
                Ok(json!({ "queued": s.agents.chat_inbox.len() }))
            },
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
            "agent.connectExternal",
            "Connect an External Agent",
            None,
            "→ {running, port, binary, project, clients: [{id, label, ide, headless}], notes} — ready-to-paste \
             configuration that gives Claude Code, Codex, Gemini CLI, Cursor, Grok or any MCP client CodeCraft's \
             tools: `ide` reaches this running IDE (its Agent Access policy and approvals apply), `headless` is \
             `codecraft-cli mcp <project>`. Nothing is written to the clients' files.",
            Context::Global,
            always,
            |s, _| {
                let v = crate::agent_mcp::connect_snippets(s);
                s.agents.connect_shown = Some(v.clone());
                Ok(v)
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

fn has_chat_text(s: &Session) -> Result<(), String> {
    let (e, _) = s.ws.active().ok_or("no editor is open")?;
    if e.selections.primary().is_empty() { Err("select some text first".into()) } else { Ok(()) }
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
    let session = s.agents.job(id).and_then(|j| j.session_key.clone());
    Ok(json!({ "job": id.0, "session": session }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_file(body: &str) -> (Session, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, body).unwrap();
        let mut s = Session::new();
        s.open_file(&file).unwrap();
        (s, dir)
    }

    #[test]
    fn send_to_chat_needs_a_selection() {
        let (mut s, _dir) = session_with_file("fn main() {}\n");
        assert!(s.enabled("agent.sendToChat").is_err());
        assert!(s.execute("agent.sendToChat", &json!({})).is_err());
        assert!(s.agents.chat_inbox.is_empty());
    }

    #[test]
    fn send_to_chat_queues_the_selection_and_shows_the_agent() {
        let (mut s, _dir) = session_with_file("fn main() {}\n");
        s.execute("edit.selectAll", &json!({})).unwrap();
        s.execute("agent.sendToChat", &json!({})).unwrap();
        let snippet = s.agents.chat_inbox.first().cloned().unwrap_or_default();
        assert!(snippet.contains("a.rs:1"), "{snippet}");
        assert!(snippet.contains("fn main() {}"), "{snippet}");
        assert!(s.ws.layout.is_visible(codecraft_workspace::ToolWindowId::Agent));
    }

    #[test]
    fn send_to_chat_accepts_explicit_text() {
        let (mut s, _dir) = session_with_file("x\n");
        s.execute("edit.selectAll", &json!({})).unwrap();
        s.execute("agent.sendToChat", &json!({ "text": "SELECT 1;" })).unwrap();
        assert_eq!(s.agents.chat_inbox, vec!["```\nSELECT 1;\n```\n".to_string()]);
    }
}
