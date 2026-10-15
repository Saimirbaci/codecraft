//! Command lines for running each agent CLI headless (flags verified against each CLI's `--help`
//! on the development machine: claude 2.1, codex-cli 0.162, grok 1.0.41, gemini 0.39).
//!
//! MCP servers (CodeCraft's tools) reach each CLI the way it documents: Claude Code
//! `--mcp-config`, Codex `-c mcp_servers.…`, Gemini a per-run settings file named by
//! `GEMINI_CLI_SYSTEM_DEFAULTS_PATH` (its "system defaults" layer, below the user's own
//! `~/.gemini/settings.json`; `mcpServers` are merged by name across layers), never the user's
//! files. Grok's headless mode reads MCP servers only from its config files
//! (`~/.grok/config.toml`, `.grok/config.toml`; its `GROK_CONFIG` overlay may not add servers),
//! so it gets none: the user registers CodeCraft once (`grok mcp add …`, printed by CodeCraft)
//! and the job's environment ([`CliRequest::env`]) tells that server which IDE and job it is for.

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
    /// command line, Gemini from a settings file in [`CliRequest::settings_dir`]; Grok only
    /// through ACP (Ask mode).
    pub mcp_servers: Vec<crate::McpServer>,
    /// Extra environment for the agent process (the IDE's port and the job's tag, for an MCP
    /// server the user registered with the CLI itself).
    pub env: Vec<(String, String)>,
    /// A private directory for this run's settings files (Gemini's MCP servers). Without one,
    /// Gemini gets no MCP servers.
    pub settings_dir: Option<PathBuf>,
}

/// The env var naming Gemini CLI's system-defaults settings file (documented in Gemini CLI's
/// configuration reference; the lowest settings layer).
pub const GEMINI_SYSTEM_DEFAULTS_ENV: &str = "GEMINI_CLI_SYSTEM_DEFAULTS_PATH";
/// The env var that moves Gemini CLI's system settings file (and with it the default
/// system-defaults location).
const GEMINI_SYSTEM_SETTINGS_ENV: &str = "GEMINI_CLI_SYSTEM_SETTINGS_PATH";
/// The per-run settings file CodeCraft writes for Gemini.
pub const GEMINI_SETTINGS_FILE: &str = "gemini-system-defaults.json";
/// System defaults larger than this are not merged (they are not settings).
const MAX_SETTINGS_BYTES: u64 = 1 << 20;

/// How a backend's headless (Auto/Review) runs get MCP servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpSupport {
    /// On the command line (Claude Code, Codex).
    Flags,
    /// From a per-run settings file CodeCraft writes (Gemini).
    SettingsFile,
    /// Only from the user's own configuration (Grok).
    UserConfig,
}

pub fn mcp_support(backend: Backend) -> McpSupport {
    match backend {
        Backend::Claude | Backend::Codex => McpSupport::Flags,
        Backend::Gemini => McpSupport::SettingsFile,
        Backend::Grok => McpSupport::UserConfig,
    }
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
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// Write the settings files that give the CLI `req.mcp_servers` when it reads them from a file
/// (Gemini) into `req.settings_dir`; returns the environment that points the CLI at them. Empty
/// for CLIs that take MCP servers on the command line or not at all.
pub fn mcp_settings(req: &CliRequest) -> crate::Result<Vec<(String, String)>> {
    if req.mcp_servers.is_empty() || mcp_support(req.backend) != McpSupport::SettingsFile {
        return Ok(Vec::new());
    }
    let Some(dir) = &req.settings_dir else { return Ok(Vec::new()) };
    let base = read_json_object(&gemini_system_defaults_path());
    let merged = gemini_settings(base, &req.mcp_servers);
    create_private_dir(dir)?;
    let path = dir.join(GEMINI_SETTINGS_FILE);
    let text = serde_json::to_string_pretty(&merged).map_err(|e| crate::AgentError::Protocol(e.to_string()))?;
    write_private(&path, text.as_bytes())?;
    Ok(vec![(GEMINI_SYSTEM_DEFAULTS_ENV.to_string(), path.to_string_lossy().into_owned())])
}

/// Gemini settings: `base` (the machine's own system defaults, kept) with `servers` added to its
/// `mcpServers` (Gemini merges `mcpServers` by name across its settings layers, so the user's own
/// servers stay, and a server the user gave the same name wins).
pub fn gemini_settings(
    base: serde_json::Map<String, serde_json::Value>,
    servers: &[crate::McpServer],
) -> serde_json::Value {
    use serde_json::{Value, json};
    let mut root = base;
    let entry = root.entry("mcpServers").or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    if let Some(map) = entry.as_object_mut() {
        for s in servers {
            let env: serde_json::Map<String, Value> = s.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            map.insert(
                s.name.clone(),
                json!({ "command": s.command, "args": s.args, "env": env, "description": "CodeCraft IDE tools" }),
            );
        }
    }
    Value::Object(root)
}

/// Where Gemini CLI reads its system defaults (as its configuration reference describes):
/// `$GEMINI_CLI_SYSTEM_DEFAULTS_PATH`, else `system-defaults.json` next to the system settings
/// file (`$GEMINI_CLI_SYSTEM_SETTINGS_PATH` or the platform's location).
fn gemini_system_defaults_path() -> PathBuf {
    if let Some(p) = std::env::var_os(GEMINI_SYSTEM_DEFAULTS_ENV).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let settings = std::env::var_os(GEMINI_SYSTEM_SETTINGS_ENV)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                PathBuf::from("/Library/Application Support/GeminiCli/settings.json")
            } else if cfg!(windows) {
                PathBuf::from("C:\\ProgramData\\gemini-cli\\settings.json")
            } else {
                PathBuf::from("/etc/gemini-cli/settings.json")
            }
        });
    settings.parent().map(|d| d.join("system-defaults.json")).unwrap_or_else(|| PathBuf::from("system-defaults.json"))
}

/// A small JSON object file, or an empty object (missing, too large, not an object).
fn read_json_object(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let small = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= MAX_SETTINGS_BYTES);
    if !small {
        return serde_json::Map::new();
    }
    match std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
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
            env: Vec::new(),
            settings_dir: None,
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
        // Grok and Gemini headless get none on the command line (Gemini reads a settings file).
        for b in [Backend::Grok, Backend::Gemini] {
            let mut r = req(b);
            r.mcp_servers = vec![server.clone()];
            assert_eq!(args(&r), args(&req(b)));
        }
    }

    fn bridge() -> crate::McpServer {
        crate::McpServer {
            name: "codecraft".into(),
            command: PathBuf::from("/bin/codecraft"),
            args: vec!["--mcp-bridge".into(), "--port".into(), "9".into(), "--job".into(), "4".into()],
            env: vec![("CODECRAFT_JOB_KEY".into(), "k".into())],
        }
    }

    #[test]
    fn gemini_gets_its_servers_from_a_private_settings_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut r = req(Backend::Gemini);
        r.mcp_servers = vec![bridge()];
        // No directory: nothing is written.
        assert!(mcp_settings(&r).unwrap().is_empty());
        r.settings_dir = Some(tmp.path().join("job-4"));
        let env = mcp_settings(&r).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, GEMINI_SYSTEM_DEFAULTS_ENV);
        let file = PathBuf::from(&env[0].1);
        assert!(file.starts_with(tmp.path()), "the file is in the job's directory: {file:?}");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let server = &v["mcpServers"]["codecraft"];
        assert_eq!(server["command"], "/bin/codecraft");
        assert_eq!(server["args"][4], "4");
        assert_eq!(server["env"]["CODECRAFT_JOB_KEY"], "k");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o077, 0);
        }
        // The command line is unchanged (Gemini has no flag for MCP servers).
        assert_eq!(args(&r), args(&req(Backend::Gemini)));
        // Claude, Codex and Grok write nothing.
        for b in [Backend::Claude, Backend::Codex, Backend::Grok] {
            let mut r = req(b);
            r.mcp_servers = vec![bridge()];
            r.settings_dir = Some(tmp.path().join(b.binary()));
            assert!(mcp_settings(&r).unwrap().is_empty(), "{b:?}");
            assert!(!tmp.path().join(b.binary()).exists());
        }
        assert_eq!(mcp_support(Backend::Grok), McpSupport::UserConfig);
    }

    #[test]
    fn gemini_settings_keep_the_machine_defaults() {
        let mut base = serde_json::Map::new();
        base.insert("general".into(), serde_json::json!({ "vimMode": true }));
        base.insert("mcpServers".into(), serde_json::json!({ "admin": { "command": "x" } }));
        let v = gemini_settings(base, &[bridge()]);
        assert_eq!(v["general"]["vimMode"], true);
        assert_eq!(v["mcpServers"]["admin"]["command"], "x");
        assert_eq!(v["mcpServers"]["codecraft"]["command"], "/bin/codecraft");
        let mut odd = serde_json::Map::new();
        odd.insert("mcpServers".into(), serde_json::json!([1, 2]));
        assert!(gemini_settings(odd, &[bridge()])["mcpServers"]["codecraft"].is_object());
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"[not an object").unwrap();
        assert!(read_json_object(&bad).is_empty());
        assert!(read_json_object(&dir.path().join("missing.json")).is_empty());
        assert!(read_json_object(dir.path()).is_empty(), "a directory");
    }

    #[test]
    fn extra_environment_reaches_the_command() {
        let mut r = req(Backend::Grok);
        r.env = vec![("CODECRAFT_IDE_PORT".into(), "4100".into())];
        let cmd = build_command(&r);
        assert!(cmd.get_envs().any(|(k, v)| k == "CODECRAFT_IDE_PORT" && v.is_some_and(|v| v == "4100")));
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
