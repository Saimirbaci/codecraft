//! Command lines for running each agent CLI headless (flags verified against each CLI's `--help`
//! on the development machine: claude 2.1, codex-cli 0.162, grok 1.0.41, gemini 0.39).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::backend::{Backend, enriched_path};

/// One headless run.
#[derive(Clone, Debug)]
pub struct CliRequest {
    pub backend: Backend,
    pub program: PathBuf,
    pub prompt: String,
    pub cwd: PathBuf,
    /// Skip every permission prompt (Auto and Review modes).
    pub bypass: bool,
    /// Continue an earlier session (follow-ups).
    pub resume: Option<String>,
    /// Follow up on the most recent session in `cwd` when no session id is known (Grok).
    pub continue_last: bool,
    /// Remove `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` so Claude Code uses the subscription
    /// login rather than a (possibly empty) API key.
    pub scrub_api_keys: bool,
    /// MCP servers to give the agent (CodeCraft's tools). Claude Code and Codex take them on the
    /// command line; Grok and Gemini only through ACP (Ask mode).
    pub mcp_servers: Vec<crate::McpServer>,
}

/// A prompt that no argument parser mistakes for a flag.
fn prompt_arg(p: &str) -> String {
    if p.starts_with('-') { format!(" {p}") } else { p.to_string() }
}

/// The arguments for a request (without the program).
pub fn args(req: &CliRequest) -> Vec<String> {
    let prompt = prompt_arg(&req.prompt);
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &str| a.push(s.to_string());
    match req.backend {
        Backend::Claude => {
            push("-p");
            push(&prompt);
            push("--output-format");
            push("stream-json");
            push("--verbose");
            if req.bypass {
                push("--dangerously-skip-permissions");
            }
            if let Some(id) = &req.resume {
                push("--resume");
                push(id);
            }
            if !req.mcp_servers.is_empty() {
                push("--mcp-config");
                push(&crate::McpServer::claude_config(&req.mcp_servers));
            }
        }
        Backend::Codex => {
            // Config overrides go before the subcommand (they apply to every subcommand).
            for server in &req.mcp_servers {
                for o in server.codex_overrides() {
                    push(&o);
                }
            }
            push("exec");
            if req.resume.is_some() {
                push("resume");
            }
            push("--json");
            push("--skip-git-repo-check");
            if req.bypass {
                push("--dangerously-bypass-approvals-and-sandbox");
            }
            if let Some(id) = &req.resume {
                push(id);
            }
            push(&prompt);
        }
        Backend::Grok => {
            push("-p");
            push(&prompt);
            if req.bypass {
                push("--always-approve");
            }
            push("--cwd");
            push(&req.cwd.to_string_lossy());
            push("--output-format");
            push("streaming-messages-json");
            if let Some(id) = &req.resume {
                push("--resume");
                push(id);
            } else if req.continue_last {
                push("--continue");
            }
        }
        Backend::Gemini => {
            push("--output-format");
            push("stream-json");
            push("--approval-mode");
            push(if req.bypass { "yolo" } else { "default" });
            push("--skip-trust");
            if let Some(id) = &req.resume {
                push("--resume");
                push(id);
            }
            push("-p");
            push(&prompt);
        }
    }
    a
}

/// A ready-to-spawn command: program, args, working directory, `PATH` and environment.
pub fn build_command(req: &CliRequest) -> Command {
    let mut cmd = Command::new(&req.program);
    cmd.args(args(req)).current_dir(&req.cwd);
    apply_env(&mut cmd, req.backend, req.scrub_api_keys);
    cmd.stdin(Stdio::null());
    cmd
}

/// `PATH` enrichment and auth scrubbing shared by headless and ACP launches.
pub fn apply_env(cmd: &mut Command, backend: Backend, scrub_api_keys: bool) {
    cmd.env("PATH", enriched_path());
    if backend == Backend::Claude && scrub_api_keys {
        cmd.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN");
    }
}

/// A command for an ACP agent.
pub fn acp_command(backend: Backend, program: &Path, args: &[String], cwd: &Path, scrub_api_keys: bool) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    apply_env(&mut cmd, backend, scrub_api_keys);
    cmd
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn req(backend: Backend) -> CliRequest {
        CliRequest {
            backend,
            program: PathBuf::from(backend.binary()),
            prompt: "fix it".into(),
            cwd: PathBuf::from("/w"),
            bypass: true,
            resume: None,
            continue_last: false,
            scrub_api_keys: true,
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn claude_args() {
        assert_eq!(
            args(&req(Backend::Claude)),
            ["-p", "fix it", "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions"]
        );
        let mut r = req(Backend::Claude);
        r.resume = Some("sess".into());
        r.bypass = false;
        assert_eq!(args(&r), ["-p", "fix it", "--output-format", "stream-json", "--verbose", "--resume", "sess"]);
    }

    #[test]
    fn codex_args() {
        assert_eq!(
            args(&req(Backend::Codex)),
            ["exec", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox", "fix it"]
        );
        let mut r = req(Backend::Codex);
        r.resume = Some("th".into());
        assert_eq!(
            args(&r),
            [
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "th",
                "fix it"
            ]
        );
    }

    #[test]
    fn grok_and_gemini_args() {
        assert_eq!(
            args(&req(Backend::Grok)),
            ["-p", "fix it", "--always-approve", "--cwd", "/w", "--output-format", "streaming-messages-json"]
        );
        let mut r = req(Backend::Grok);
        r.continue_last = true;
        assert_eq!(args(&r).last().map(String::as_str), Some("--continue"));
        assert_eq!(
            args(&req(Backend::Gemini)),
            ["--output-format", "stream-json", "--approval-mode", "yolo", "--skip-trust", "-p", "fix it"]
        );
    }

    #[test]
    fn mcp_servers_reach_claude_and_codex() {
        let server = crate::McpServer {
            name: "codecraft".into(),
            command: PathBuf::from("/bin/codecraft"),
            args: vec!["--mcp-bridge".into(), "--port".into(), "9".into()],
            env: Vec::new(),
        };
        let mut r = req(Backend::Claude);
        r.mcp_servers = vec![server.clone()];
        let a = args(&r);
        let i = a.iter().position(|x| x == "--mcp-config").unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&a[i + 1]).unwrap();
        assert_eq!(cfg["mcpServers"]["codecraft"]["command"], "/bin/codecraft");
        let mut r = req(Backend::Codex);
        r.mcp_servers = vec![server.clone()];
        let a = args(&r);
        assert_eq!(
            &a[..4],
            [
                "-c",
                "mcp_servers.codecraft.command=\"/bin/codecraft\"",
                "-c",
                "mcp_servers.codecraft.args=[\"--mcp-bridge\", \"--port\", \"9\"]"
            ]
        );
        assert_eq!(a[4], "exec");
        // Grok and Gemini headless get none (they take MCP servers through ACP).
        for b in [Backend::Grok, Backend::Gemini] {
            let mut r = req(b);
            r.mcp_servers = vec![server.clone()];
            assert_eq!(args(&r), args(&req(b)));
        }
    }

    #[test]
    fn flag_like_prompt_is_not_a_flag() {
        let mut r = req(Backend::Claude);
        r.prompt = "--help".into();
        assert_eq!(args(&r).get(1).map(String::as_str), Some(" --help"));
    }

    #[test]
    fn env_scrubbed_for_claude_only() {
        let cmd = build_command(&req(Backend::Claude));
        let removed: Vec<_> = cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_owned()).collect();
        assert!(removed.iter().any(|k| k == "ANTHROPIC_API_KEY"));
        assert!(removed.iter().any(|k| k == "ANTHROPIC_AUTH_TOKEN"));
        let mut keep = req(Backend::Claude);
        keep.scrub_api_keys = false;
        assert!(!build_command(&keep).get_envs().any(|(_, v)| v.is_none()));
        assert!(build_command(&req(Backend::Codex)).get_envs().any(|(k, _)| k == "PATH"));
    }
}
