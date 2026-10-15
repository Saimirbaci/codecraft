//! MCP transports for the engine tools outside the IDE window:
//!
//! - [`serve_stdio`]: the headless server behind `codecraft-cli mcp <project>`: a long-lived
//!   session whose pump runs continuously, so debug sessions, profiles and test runs live across
//!   calls.
//! - [`bridge_main`]: `codecraft --mcp-bridge` / `codecraft-cli --mcp-bridge`, a stdio MCP server
//!   that forwards to the IDE's running server (what built-in agent jobs are given).
//! - [`job_mcp_server`]: the MCP server entry a built-in agent job gets.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_agent::McpServer;
use codecraft_mcp::{ServerConfig, ServerEvent, StdioServer, ToolResult, engine_tools};

use crate::Session;
use crate::agent_tools::{self, Caller};
use crate::agents::JobId;

/// The flag both binaries take to run the bridge.
pub const BRIDGE_FLAG: &str = "--mcp-bridge";
/// The MCP server name agents see (`mcp__codecraft__debug_start`, …).
pub const SERVER_NAME: &str = "codecraft";
/// Env var with the IDE server's token, when the lock file is not readable.
pub const TOKEN_ENV: &str = "CODECRAFT_MCP_TOKEN";

/// Serve the engine tools over `input`/`output` until the input ends. Pumps `s` continuously
/// (sleeping briefly when idle). `name` is the server name in `initialize`.
pub fn serve_stdio<R, W>(s: &mut Session, input: R, output: W) -> Result<(), String>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let mut config = ServerConfig::claude_code("CodeCraft", Vec::new(), None);
    config.tools = engine_tools::engine_tools();
    let (server, events) = StdioServer::start(config, input, output).map_err(|e| e.to_string())?;
    let mut caller = Caller::new("MCP client");
    loop {
        let mut busy = s.pump();
        loop {
            match events.try_recv() {
                Ok(ServerEvent::ToolCall(call)) => {
                    busy = true;
                    let reply = call.reply;
                    agent_tools::submit(
                        s,
                        caller.clone(),
                        &call.name,
                        call.arguments,
                        Box::new(move |r: ToolResult| {
                            reply.send(r);
                        }),
                    );
                }
                Ok(ServerEvent::Initialized { client_info, .. }) => {
                    caller = Caller::from_client_info(s, &client_info, "MCP client");
                }
                Ok(ServerEvent::Disconnected { .. }) => {
                    // Let answers already queued reach the writer.
                    let until = Instant::now() + Duration::from_millis(200);
                    while Instant::now() < until && s.agent_tools.busy() {
                        s.pump();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return Ok(());
                }
                Ok(_) => {}
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if server.is_closed() && !s.agent_tools.busy() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(if busy { 1 } else { 10 }));
    }
}

/// Parsed `--mcp-bridge` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeArgs {
    pub port: u16,
    pub lock_dir: Option<PathBuf>,
    pub job: Option<u64>,
}

/// `--port N [--lock-dir D] [--job J]` (the flag itself may be included).
pub fn parse_bridge_args(args: &[String]) -> Result<BridgeArgs, String> {
    let mut out = BridgeArgs::default();
    let mut it = args.iter().filter(|a| *a != BRIDGE_FLAG);
    let mut port = None;
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("`{a}` needs a value"));
        match a.as_str() {
            "--port" | "--connect" => {
                let v = value()?;
                port = Some(v.parse::<u16>().map_err(|_| format!("`{v}` is not a port"))?);
            }
            "--lock-dir" => out.lock_dir = Some(PathBuf::from(value()?)),
            "--job" => {
                let v = value()?;
                out.job = Some(v.parse::<u64>().map_err(|_| format!("`{v}` is not a job id"))?);
            }
            other => return Err(format!("unknown bridge option `{other}`")),
        }
    }
    out.port = port.ok_or("the bridge needs --port (the IDE server's port)")?;
    Ok(out)
}

/// The token for the IDE server on `port`: from its lock file, else `$CODECRAFT_MCP_TOKEN`.
fn bridge_token(args: &BridgeArgs) -> Result<String, String> {
    let dir = args.lock_dir.clone().or_else(codecraft_mcp::lockfile::default_lock_dir);
    if let Some(dir) = dir
        && let Ok(info) = codecraft_mcp::lockfile::read(&codecraft_mcp::lockfile::lock_path(&dir, args.port))
    {
        return Ok(info.auth_token);
    }
    std::env::var(TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("no CodeCraft IDE server on port {} (no lock file, no ${TOKEN_ENV})", args.port))
}

/// Run the stdio bridge to the IDE (blocks until the agent closes stdin). Returns the exit code.
pub fn bridge_main(args: &[String]) -> i32 {
    let parsed = match parse_bridge_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("codecraft mcp bridge: {e}");
            return 2;
        }
    };
    let token = match bridge_token(&parsed) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("codecraft mcp bridge: {e}");
            return 1;
        }
    };
    let mut tag = serde_json::Map::new();
    if let Some(job) = parsed.job {
        tag.insert("codecraftJob".into(), json!(job));
    }
    let cfg = codecraft_mcp::bridge::BridgeConfig { port: parsed.port, token, client_tag: tag };
    let stdin = std::io::BufReader::new(std::io::stdin());
    match codecraft_mcp::bridge::run(&cfg, stdin, std::io::stdout()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("codecraft mcp bridge: {e}");
            1
        }
    }
}

/// The program agents launch for the bridge: this executable (the IDE, or the CLI).
fn bridge_program() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// The CodeCraft MCP server a built-in agent job gets: the bridge to this IDE's server, tagged
/// with the job. `None` while the IDE server is not running.
pub(crate) fn job_mcp_server(s: &Session, job: JobId) -> Option<McpServer> {
    let port = s.ide.port()?;
    let program = bridge_program()?;
    let mut args = vec![BRIDGE_FLAG.to_string(), "--port".into(), port.to_string()];
    if let Some(dir) = s.ide.lock_path().as_deref().and_then(Path::parent) {
        args.push("--lock-dir".into());
        args.push(dir.display().to_string());
    }
    args.push("--job".into());
    args.push(job.0.to_string());
    Some(McpServer { name: SERVER_NAME.into(), command: program, args, env: Vec::new() })
}

/// Ready-to-paste client configuration for the headless server (`codecraft-cli mcp --help`).
pub fn client_config_help(cli: &str, project: &str) -> String {
    let json_cfg = json!({
        "mcpServers": { SERVER_NAME: { "type": "stdio", "command": cli, "args": ["mcp", project, "--allow", "run,debug,profile"] } }
    });
    let pretty = serde_json::to_string_pretty(&json_cfg).unwrap_or_default();
    format!(
        "Claude Code:\n  claude mcp add {SERVER_NAME} -- {cli} mcp {project} --allow run,debug,profile\n\n\
         Codex (~/.codex/config.toml):\n  [mcp_servers.{SERVER_NAME}]\n  command = {cli:?}\n  args = [\"mcp\", {project:?}, \"--allow\", \"run,debug,profile\"]\n\n\
         Any MCP client (JSON):\n{pretty}\n"
    )
}

/// The engine tool names (docs, `--help`).
pub fn tool_names() -> Vec<String> {
    engine_tools::engine_tools().into_iter().map(|t| t.name).collect()
}

/// `{name: capability}` of every engine tool for the default arguments (docs and tests).
pub fn tool_capabilities() -> Value {
    let m: serde_json::Map<String, Value> = tool_names()
        .into_iter()
        .map(|n| {
            let cap = match n.as_str() {
                engine_tools::EXECUTE => "per command".to_string(),
                engine_tools::DB_QUERY => "read (db-write for modifying SQL)".to_string(),
                _ => crate::agent_tools::static_capability(&n).map(|c| c.id().to_string()).unwrap_or_default(),
            };
            (n, json!(cap))
        })
        .collect();
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bridge_args_parse() {
        let a =
            parse_bridge_args(&args(&["--mcp-bridge", "--port", "4100", "--lock-dir", "/x", "--job", "7"])).unwrap();
        assert_eq!(a, BridgeArgs { port: 4100, lock_dir: Some("/x".into()), job: Some(7) });
        assert!(parse_bridge_args(&args(&["--port", "99999"])).is_err());
        assert!(parse_bridge_args(&args(&["--job", "1"])).is_err());
        assert!(parse_bridge_args(&args(&["--port"])).is_err());
        assert!(parse_bridge_args(&args(&["--bogus"])).is_err());
        assert_eq!(parse_bridge_args(&args(&["--connect", "5"])).unwrap().port, 5);
    }

    #[test]
    fn bridge_token_comes_from_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let info = codecraft_mcp::lockfile::LockInfo {
            pid: std::process::id(),
            workspace_folders: vec![],
            ide_name: "CodeCraft".into(),
            transport: "ws".into(),
            running_in_windows: false,
            auth_token: "tok".into(),
        };
        codecraft_mcp::lockfile::write(dir.path(), 4242, &info).unwrap();
        let a = BridgeArgs { port: 4242, lock_dir: Some(dir.path().to_path_buf()), job: None };
        assert_eq!(bridge_token(&a).unwrap(), "tok");
        let missing = BridgeArgs { port: 4243, lock_dir: Some(dir.path().to_path_buf()), job: None };
        if std::env::var(TOKEN_ENV).is_err() {
            assert!(bridge_token(&missing).is_err());
        }
        assert_eq!(bridge_main(&args(&["--mcp-bridge"])), 2);
    }

    #[test]
    fn help_snippets_mention_every_client() {
        let h = client_config_help("/usr/local/bin/codecraft-cli", "/p");
        assert!(h.contains("claude mcp add codecraft -- /usr/local/bin/codecraft-cli mcp /p"));
        assert!(h.contains("[mcp_servers.codecraft]"));
        assert!(h.contains("\"mcpServers\""));
        let caps = tool_capabilities();
        assert_eq!(caps["debug_start"], "debug");
        assert_eq!(caps["file_read"], "read");
        assert_eq!(caps["file_write"], "write");
    }
}
