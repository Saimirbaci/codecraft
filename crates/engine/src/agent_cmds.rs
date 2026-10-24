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
            "agent.context.search",
            "Find Agent Attachments",
            None,
            "{query?: string} → {items: [{path, label, directory}], loading} — files and folders in the background-loaded project tree (up to 30 matches)",
            Context::Global,
            always,
            context_search,
        ),
        spec(
            "agent.context.attach",
            "Attach Agent Context",
            None,
            "{path: string} → {path} — select a file or folder from the project's attachment suggestions",
            Context::Global,
            always,
            context_attach,
        ),
        spec(
            "agent.start",
            "Start Agent Job",
            None,
            "{backend: \"claude\"|\"codex\"|\"grok\"|\"antigravity\", mode: \"auto\"|\"ask\"|\"review\", prompt: string, \
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
            "{job: int, prompt: string, attachments?: [path]} — continue the job's session",
            Context::Global,
            always,
            |s, p| {
                let prompt = req_str(p, "prompt")?.to_string();
                if prompt.trim().is_empty() {
                    return Err(CommandError::BadParams("the prompt is empty".into()));
                }
                let attachments = follow_up_attachments(p)?;
                let id = job(p)?;
                let project = s.agents.job(id).map(|job| job.project.as_path());
                let full = follow_up_prompt(prompt, &attachments, project);
                agents::follow_up(s, id, &full)?;
                if let Some(job) = s.agents.jobs.iter_mut().find(|job| job.id == id) {
                    for path in attachments {
                        if !job.attachments.contains(&path) {
                            job.attachments.push(path);
                        }
                    }
                }
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
        .ok_or_else(|| CommandError::BadParams(format!("unknown backend `{id}` (claude, codex, grok, antigravity)")))
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

fn follow_up_prompt(mut prompt: String, attachments: &[PathBuf], project: Option<&std::path::Path>) -> String {
    if !attachments.is_empty() {
        prompt.push_str("\n\nAttached files and folders:\n");
        for path in attachments {
            // Like a new job, refer to project files relative to the agent's working directory.
            // A resumed Review agent must use its worktree, rather than the original checkout.
            let path = project.and_then(|root| path.strip_prefix(root).ok()).unwrap_or(path);
            if path.as_os_str().is_empty() {
                prompt.push_str("- .\n");
            } else {
                prompt.push_str(&format!("- {}\n", path.display()));
            }
        }
    }
    prompt
}

fn follow_up_attachments(p: &Value) -> Result<Vec<PathBuf>, CommandError> {
    let Some(value) = p.get("attachments") else { return Ok(Vec::new()) };
    let items = value
        .as_array()
        .filter(|items| items.len() <= 100)
        .ok_or_else(|| CommandError::BadParams("attachments must be an array of at most 100 paths".into()))?;
    items
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|p| !p.is_empty() && p.len() <= 32_768)
                .map(PathBuf::from)
                .ok_or_else(|| CommandError::BadParams("each attachment must be a nonempty path".into()))
        })
        .collect()
}

/// Reuse the project scan, which runs on a worker; completion never touches the disk.
fn context_search(s: &mut Session, p: &Value) -> CmdResult {
    let query = opt_str(p, "query").unwrap_or("");
    if query.len() > 1024 {
        return Err(CommandError::BadParams("attachment query is too long".into()));
    }
    let query = query.to_lowercase();
    let Some(project) = &s.ws.project else {
        return Ok(json!({"items": [], "loading": false}));
    };
    // Start with the current directory's children, then nested entries; retain only 30.
    let nodes =
        project.tree.iter().filter(|(_, n)| n.depth == 1).chain(project.tree.iter().filter(|(_, n)| n.depth > 1));
    let items: Vec<_> = nodes
        .filter_map(|(id, n)| {
            // JSON command paths must be UTF-8; never let a filesystem name panic in json!.
            let path = n.path.to_str()?;
            let label = project.tree.relative_path(id)?.display().to_string();
            label.to_lowercase().contains(&query).then(|| {
                json!({
                    "path": path, "label": label, "directory": n.is_dir,
                })
            })
        })
        .take(30)
        .collect();
    Ok(json!({"items": items, "loading": project.tree.is_empty()}))
}

fn context_attach(s: &mut Session, p: &Value) -> CmdResult {
    let path = PathBuf::from(req_str(p, "path")?);
    let project = s.ws.project.as_ref().ok_or_else(|| CommandError::Failed("open a project to attach files".into()))?;
    if project.tree.find(&path).is_none() {
        return Err(CommandError::BadParams("choose a file or folder from the current project".into()));
    }
    Ok(json!({"path": path}))
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
    fn follow_up_attachments_stay_relative_to_the_agents_project() {
        let root = std::env::temp_dir().join("agent-project");
        let outside = std::env::temp_dir().join("external-reference.rs");
        let paths = [root.clone(), root.join("src"), outside.clone()];
        let prompt = follow_up_prompt("Continue".into(), &paths, Some(&root));
        assert!(prompt.contains("- .\n- src\n"), "{prompt}");
        assert!(prompt.contains(&outside.display().to_string()), "{prompt}");
        assert!(!prompt.contains(&root.display().to_string()), "{prompt}");
        assert_eq!(follow_up_prompt("Continue".into(), &[], Some(&root)), "Continue");
        assert!(follow_up_attachments(&json!({"attachments": [null]})).is_err());
        assert!(follow_up_attachments(&json!({"attachments": vec!["a"; 101]})).is_err());
    }

    #[test]
    fn attachment_completion_lists_files_folders_and_filters_without_io() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "demo").unwrap();
        let mut project = codecraft_workspace::Project::new(root.clone());
        project.tree = codecraft_fs::scan(&root, &Default::default()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            // Some Unix filesystems reject such names, so inject a synthetic tree entry.
            let name = std::ffi::OsString::from_vec(b"bad-\xff.rs".to_vec());
            let mut nodes: Vec<_> = project.tree.iter().map(|(_, node)| node.clone()).collect();
            let id = codecraft_fs::NodeId(nodes.len());
            if let Some(root_node) = nodes.first_mut() {
                root_node.children.push(id);
            }
            nodes.push(codecraft_fs::Node {
                name: "bad filename".into(),
                path: root.join(name),
                is_dir: false,
                parent: Some(codecraft_fs::NodeId(0)),
                children: Vec::new(),
                depth: 1,
            });
            project.tree = codecraft_fs::FileTree::from_nodes(nodes, false);
        }
        let mut s = Session::new();
        s.ws.project = Some(project);
        // Delete the source to prove completion only uses the background-loaded tree.
        std::fs::remove_dir_all(root.join("src")).unwrap();
        let result = s.execute("agent.context.search", &json!({})).unwrap();
        let items = result["items"].as_array().unwrap();
        assert_eq!(items.len(), 3, "unrepresentable filesystem paths are skipped without panicking");
        assert!(items.iter().any(|i| i["label"] == "src" && i["directory"] == true));
        assert!(items.iter().any(|i| i["label"] == "README.md" && i["directory"] == false));
        let filtered = s.execute("agent.context.search", &json!({"query": "MAIN.RS"})).unwrap();
        assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
        assert_eq!(filtered["items"][0]["label"], "src/main.rs");
        for path in [root.join("src"), root.join("README.md")] {
            assert_eq!(s.execute("agent.context.attach", &json!({"path": path})).unwrap()["path"], json!(path));
        }
        assert!(s.execute("agent.context.attach", &json!({"path": root.join("../outside")})).is_err());
        assert!(s.execute("agent.context.search", &json!({"query": "a".repeat(1025)})).is_err());
    }

    #[test]
    fn attachment_completion_without_project_and_with_many_entries_is_bounded() {
        let mut s = Session::new();
        assert_eq!(s.execute("agent.context.search", &json!({})).unwrap()["items"], json!([]));
        assert!(s.execute("agent.context.attach", &json!({"path": "/missing"})).is_err());
        let dir = tempfile::tempdir().unwrap();
        for n in 0..40 {
            std::fs::write(dir.path().join(format!("file{n}")), "").unwrap();
        }
        let mut project = codecraft_workspace::Project::new(dir.path().into());
        project.tree = codecraft_fs::scan(dir.path(), &Default::default()).unwrap();
        s.ws.project = Some(project);
        assert_eq!(s.execute("agent.context.search", &json!({})).unwrap()["items"].as_array().unwrap().len(), 30);
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
