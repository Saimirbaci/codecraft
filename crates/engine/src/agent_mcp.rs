//! MCP transports for the engine tools outside the IDE window:
//!
//! - [`serve_stdio`]: the headless server behind `codecraft-cli mcp <project>`: a long-lived
//!   session whose pump runs continuously, so debug sessions, profiles and test runs live across
//!   calls.
//! - [`bridge_main`]: `codecraft --mcp-bridge` / `codecraft-cli --mcp-bridge`, a stdio MCP server
//!   that forwards to the IDE's running server (what built-in agent jobs are given).
//! - [`job_mcp_server`]: the MCP server entry a built-in agent job gets, and [`job_env`]: the
//!   environment that lets a bridge the user registered with an agent CLI (`--ide`) find this
//!   IDE and the job.
//! - [`client_config`]: ready-to-paste configuration for each MCP client
//!   (`codecraft-cli mcp --print-config`, Agents › Connect an External Agent).

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
/// The connection name of the stdio client in [`agent_tools::CallKey`].
const STDIO_CONNECTION: &str = "stdio";

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
                    agent_tools::submit_call(
                        s,
                        caller.clone(),
                        Some(agent_tools::CallKey::new(STDIO_CONNECTION, call.request)),
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
                Ok(ServerEvent::Cancelled { request, .. }) => {
                    busy = true;
                    agent_tools::cancel(s, STDIO_CONNECTION, &request);
                }
                Ok(ServerEvent::Disconnected { .. }) => {
                    // Let answers already queued reach the writer.
                    let until = Instant::now() + Duration::from_millis(200);
                    while Instant::now() < until && s.agent_tools.busy() {
                        s.pump();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    agent_tools::client_gone(s, STDIO_CONNECTION);
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

/// Env vars CodeCraft sets for the agent CLIs it launches: the IDE server's port and lock
/// directory, the job id and its key. A bridge started with `--ide` (one the user registered with
/// the CLI, e.g. `grok mcp add codecraft -- codecraft-cli mcp --ide`) reads them to reach this IDE
/// as the job.
pub const ENV_IDE_PORT: &str = "CODECRAFT_IDE_PORT";
pub const ENV_IDE_LOCK_DIR: &str = "CODECRAFT_IDE_LOCK_DIR";
pub const ENV_JOB: &str = "CODECRAFT_JOB";
pub const ENV_JOB_KEY: &str = "CODECRAFT_JOB_KEY";

/// Parsed `--mcp-bridge` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeArgs {
    /// 0 with `discover`: found from the environment or the lock files.
    pub port: u16,
    pub lock_dir: Option<PathBuf>,
    pub job: Option<u64>,
    /// `--ide`: find the IDE (the launching job's environment, else the IDE whose project holds
    /// the working directory).
    pub discover: bool,
}

/// `--port N | --ide [--lock-dir D] [--job J]` (the flag itself may be included).
pub fn parse_bridge_args(args: &[String]) -> Result<BridgeArgs, String> {
    let mut out = BridgeArgs::default();
    let mut it = args.iter().filter(|a| *a != BRIDGE_FLAG);
    let mut port = None;
    while let Some(a) = it.next() {
        if a == "--ide" {
            out.discover = true;
            continue;
        }
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
    out.port = match port {
        Some(p) => p,
        None if out.discover => 0,
        None => return Err("the bridge needs --port (the IDE server's port) or --ide".into()),
    };
    Ok(out)
}

/// Where a bridge connects and who it says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeTarget {
    pub args: BridgeArgs,
    /// The job's key ([`crate::agent_tools::job_key`]), from `$CODECRAFT_JOB_KEY`.
    pub job_key: Option<String>,
}

/// Resolve `--ide` and the job tag: an explicit `--port` wins; else the launching job's
/// environment (`env`); else the live CodeCraft whose project holds `cwd`.
pub fn resolve_bridge(
    mut args: BridgeArgs,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<BridgeTarget, String> {
    let from_env = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    if args.discover {
        let env_port = from_env(ENV_IDE_PORT).and_then(|p| p.trim().parse::<u16>().ok());
        if args.port == 0
            && let Some(p) = env_port
        {
            args.port = p;
            if args.lock_dir.is_none() {
                args.lock_dir = from_env(ENV_IDE_LOCK_DIR).map(PathBuf::from);
            }
        }
        if args.job.is_none() && env_port.is_some_and(|p| p == args.port) {
            args.job = from_env(ENV_JOB).and_then(|j| j.trim().parse::<u64>().ok());
        }
        if args.port == 0 {
            let dir = args.lock_dir.clone().or_else(codecraft_mcp::lockfile::default_lock_dir).ok_or(
                "no CodeCraft IDE found: no lock directory (~/.claude/ide); pass --port",
            )?;
            let (port, _) = codecraft_mcp::lockfile::find_for_dir(&dir, crate::ide_server::IDE_NAME, cwd)
                .ok_or_else(|| {
                    format!(
                        "no running CodeCraft has a project that contains {}; open it in CodeCraft, or pass --port",
                        cwd.display()
                    )
                })?;
            args.port = port;
            args.lock_dir = Some(dir);
        }
    }
    let job_key = args.job.and_then(|_| from_env(ENV_JOB_KEY));
    Ok(BridgeTarget { args, job_key })
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
    let cwd = std::env::current_dir().unwrap_or_default();
    let target = match resolve_bridge(parsed, &|k| std::env::var(k).ok(), &cwd) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("codecraft mcp bridge: {e}");
            return 1;
        }
    };
    let token = match bridge_token(&target.args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("codecraft mcp bridge: {e}");
            return 1;
        }
    };
    let cfg = codecraft_mcp::bridge::BridgeConfig { port: target.args.port, token, client_tag: client_tag(&target) };
    let stdin = std::io::BufReader::new(std::io::stdin());
    match codecraft_mcp::bridge::run(&cfg, stdin, std::io::stdout()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("codecraft mcp bridge: {e}");
            1
        }
    }
}

/// The `clientInfo` fields the bridge adds to the agent's `initialize`: the job and its key.
pub fn client_tag(target: &BridgeTarget) -> serde_json::Map<String, Value> {
    let mut tag = serde_json::Map::new();
    if let Some(job) = target.args.job {
        tag.insert(agent_tools::CLIENT_INFO_JOB.into(), json!(job));
        if let Some(key) = &target.job_key {
            tag.insert(agent_tools::CLIENT_INFO_JOB_KEY.into(), json!(key));
        }
    }
    tag
}

/// The program agents launch for the bridge: this executable (the IDE, or the CLI).
fn bridge_program() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// The CodeCraft MCP server a built-in agent job gets: the bridge to this IDE's server, tagged
/// with the job (its key travels in the server's environment, not on the command line). `None`
/// while the IDE server is not running.
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
    let env = vec![(ENV_JOB_KEY.to_string(), agent_tools::job_key(s, job))];
    Some(McpServer { name: SERVER_NAME.into(), command: program, args, env })
}

/// The environment an agent CLI that CodeCraft launches for `job` gets ([`ENV_IDE_PORT`], …):
/// a bridge the user registered with the CLI itself (`mcp --ide`) finds this IDE and the job
/// with it. Empty while the IDE server is not running.
pub(crate) fn job_env(s: &Session, job: JobId) -> Vec<(String, String)> {
    let Some(port) = s.ide.port() else { return Vec::new() };
    let mut env = vec![(ENV_IDE_PORT.to_string(), port.to_string())];
    if let Some(dir) = s.ide.lock_path().as_deref().and_then(Path::parent) {
        env.push((ENV_IDE_LOCK_DIR.to_string(), dir.display().to_string()));
    }
    env.push((ENV_JOB.to_string(), job.0.to_string()));
    env.push((ENV_JOB_KEY.to_string(), agent_tools::job_key(s, job)));
    env
}

// ---------------------------------------------------------------- client configuration

/// MCP clients CodeCraft prints configuration for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientKind {
    Claude,
    Codex,
    Gemini,
    Cursor,
    Grok,
    /// The `mcpServers` JSON most clients read.
    Json,
}

impl ClientKind {
    pub const ALL: [ClientKind; 6] =
        [ClientKind::Claude, ClientKind::Codex, ClientKind::Gemini, ClientKind::Cursor, ClientKind::Grok, ClientKind::Json];

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Grok => "grok",
            Self::Json => "json",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini CLI",
            Self::Cursor => "Cursor",
            Self::Grok => "Grok",
            Self::Json => "Any MCP client (JSON)",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let s = match s.as_str() {
            "claude-code" | "claudecode" => "claude",
            "gemini-cli" => "gemini",
            other => other,
        };
        Self::ALL.into_iter().find(|k| k.id() == s)
    }
}

/// The command an MCP client launches for CodeCraft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    /// The server's name in the client (`codecraft`, `codecraft-ide`).
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// For every project (a client's user scope) rather than this one.
    pub user_scope: bool,
}

/// `codecraft-cli mcp <project> --allow <allow>`: the headless server for one project.
pub fn headless_server(cli: &str, project: &str, allow: &str) -> ServerCommand {
    let mut args = vec!["mcp".to_string(), project.to_string()];
    if !allow.trim().is_empty() {
        args.push("--allow".into());
        args.push(allow.trim().to_string());
    }
    ServerCommand { name: SERVER_NAME.into(), command: cli.to_string(), args, user_scope: false }
}

/// The bridge to the running IDE that finds it on its own (`mcp --ide` of the CLI, or
/// `--mcp-bridge --ide` of the desktop app): one entry for every project.
pub fn ide_bridge_server(program: &str, is_cli: bool) -> ServerCommand {
    let args = if is_cli { vec!["mcp".into(), "--ide".into()] } else { vec![BRIDGE_FLAG.into(), "--ide".into()] };
    ServerCommand { name: format!("{SERVER_NAME}-ide"), command: program.to_string(), args, user_scope: true }
}

/// A word for a POSIX shell: as it is when it is plain, else single-quoted.
fn shell_word(w: &str) -> String {
    let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=,@%+".contains(c));
    if plain { w.to_string() } else { format!("'{}'", w.replace('\'', "'\\''")) }
}

fn command_line(server: &ServerCommand) -> String {
    std::iter::once(server.command.as_str())
        .chain(server.args.iter().map(String::as_str))
        .map(shell_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ready-to-paste configuration of `server` for `client`.
pub fn client_config(client: ClientKind, server: &ServerCommand) -> String {
    let name = &server.name;
    let cmd = command_line(server);
    let servers = |typed: bool| {
        let mut entry = json!({ "command": server.command, "args": server.args });
        if typed && let Some(o) = entry.as_object_mut() {
            o.insert("type".into(), json!("stdio"));
        }
        let mut map = serde_json::Map::new();
        map.insert(name.clone(), entry);
        serde_json::to_string_pretty(&json!({ "mcpServers": map })).unwrap_or_default()
    };
    match client {
        ClientKind::Claude => {
            let scope = if server.user_scope { " --scope user" } else { "" };
            format!("claude mcp add{scope} {name} -- {cmd}")
        }
        ClientKind::Codex => {
            let args = server.args.iter().map(|a| json!(a).to_string()).collect::<Vec<_>>().join(", ");
            format!(
                "codex mcp add {name} -- {cmd}\n\n# or in ~/.codex/config.toml:\n[mcp_servers.{name}]\ncommand = {}\nargs = [{args}]",
                json!(server.command)
            )
        }
        ClientKind::Gemini => {
            let file = if server.user_scope { "~/.gemini/settings.json" } else { ".gemini/settings.json in the project" };
            format!("# {file} (merge into \"mcpServers\"):\n{}", servers(false))
        }
        ClientKind::Cursor => {
            let file = if server.user_scope { "~/.cursor/mcp.json" } else { ".cursor/mcp.json in the project" };
            format!("# {file}:\n{}", servers(false))
        }
        ClientKind::Grok => {
            let scope = if server.user_scope { "" } else { " --scope project" };
            format!("grok mcp add{scope} {name} -- {cmd}")
        }
        ClientKind::Json => servers(true),
    }
}

/// `codecraft-cli` next to `exe` (the desktop app ships it alongside), or `exe` itself when it
/// is the CLI.
pub fn find_cli(exe: &Path) -> Option<PathBuf> {
    let name = exe.file_stem().and_then(|n| n.to_str()).unwrap_or_default();
    if name.starts_with("codecraft-cli") {
        return Some(exe.to_path_buf());
    }
    let sibling = exe.with_file_name(if cfg!(windows) { "codecraft-cli.exe" } else { "codecraft-cli" });
    sibling.is_file().then_some(sibling)
}

/// What Agents › Connect an External Agent shows (`agent.connectExternal`): for every client,
/// the entry that reaches this running IDE (its policy and approval cards apply) and the
/// headless server for the project.
pub fn connect_snippets(s: &Session) -> Value {
    let exe = std::env::current_exe().unwrap_or_default();
    let cli = find_cli(&exe);
    let project = s.project_root();
    let ide = match &cli {
        Some(c) => ide_bridge_server(&c.display().to_string(), true),
        None => ide_bridge_server(&exe.display().to_string(), false),
    };
    let headless = match (&cli, &project) {
        (Some(c), Some(p)) => Some(headless_server(&c.display().to_string(), &p.display().to_string(), "run,debug,profile")),
        _ => None,
    };
    let clients: Vec<Value> = ClientKind::ALL
        .into_iter()
        .map(|k| {
            json!({
                "id": k.id(),
                "label": k.label(),
                "ide": client_config(k, &ide),
                "headless": headless.as_ref().map(|h| client_config(k, h)),
            })
        })
        .collect();
    let mut notes = vec![
        "The IDE entry reaches the CodeCraft window whose project holds the agent's working directory; this IDE's \
         Agent Access policy and approval prompts apply."
            .to_string(),
        "Built-in Claude Code and Codex jobs get CodeCraft's tools without this. Grok's and Antigravity's headless \
         modes read MCP servers only from their own config (`grok mcp add`, `agy mcp add`): add the entry once and \
         their jobs use it (tagged with the job)."
            .to_string(),
    ];
    if headless.is_none() {
        notes.push(if cli.is_none() {
            "The headless server is `codecraft-cli mcp <project>`; codecraft-cli was not found next to CodeCraft \
             (cargo install --path apps/codecraft-cli)."
                .to_string()
        } else {
            "Open a project for the headless entry.".to_string()
        });
    }
    json!({
        "running": s.ide.is_running(),
        "port": s.ide.port(),
        "binary": cli.as_ref().unwrap_or(&exe),
        "project": project,
        "clients": clients,
        "notes": notes,
    })
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
        assert_eq!(a, BridgeArgs { port: 4100, lock_dir: Some("/x".into()), job: Some(7), discover: false });
        let d = parse_bridge_args(&args(&["--mcp-bridge", "--ide"])).unwrap();
        assert!(d.discover && d.port == 0);
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
        let a = BridgeArgs { port: 4242, lock_dir: Some(dir.path().to_path_buf()), job: None, discover: false };
        assert_eq!(bridge_token(&a).unwrap(), "tok");
        let missing = BridgeArgs { port: 4243, lock_dir: Some(dir.path().to_path_buf()), job: None, discover: false };
        if std::env::var(TOKEN_ENV).is_err() {
            assert!(bridge_token(&missing).is_err());
        }
        assert_eq!(bridge_main(&args(&["--mcp-bridge"])), 2);
    }

    #[test]
    fn discovery_uses_the_job_environment_then_the_lock_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let info = codecraft_mcp::lockfile::LockInfo {
            pid: std::process::id(),
            workspace_folders: vec![project.display().to_string()],
            ide_name: crate::ide_server::IDE_NAME.into(),
            transport: "ws".into(),
            running_in_windows: false,
            auth_token: "tok".into(),
        };
        let locks = dir.path().join("locks");
        codecraft_mcp::lockfile::write(&locks, 4300, &info).unwrap();
        let ide = BridgeArgs { discover: true, lock_dir: Some(locks.clone()), ..BridgeArgs::default() };
        // No job environment: the IDE whose project holds the working directory, untagged.
        let none = |_: &str| None;
        let t = resolve_bridge(ide.clone(), &none, &project.join("src")).unwrap();
        assert_eq!((t.args.port, t.args.job, t.job_key.clone()), (4300, None, None));
        assert!(client_tag(&t).is_empty());
        let e = resolve_bridge(ide.clone(), &none, dir.path()).unwrap_err();
        assert!(e.contains("no running CodeCraft"), "{e}");
        // Launched by a job (a worktree outside the project): the environment names IDE and job.
        let env = |k: &str| match k {
            ENV_IDE_PORT => Some("4555".to_string()),
            ENV_JOB => Some("3".to_string()),
            ENV_JOB_KEY => Some("secret".to_string()),
            ENV_IDE_LOCK_DIR => Some("/locks".to_string()),
            _ => None,
        };
        let t = resolve_bridge(BridgeArgs { discover: true, ..BridgeArgs::default() }, &env, Path::new("/wt/3")).unwrap();
        assert_eq!((t.args.port, t.args.job), (4555, Some(3)));
        assert_eq!(t.args.lock_dir.as_deref(), Some(Path::new("/locks")));
        let tag = client_tag(&t);
        assert_eq!(tag["codecraftJob"], 3);
        assert_eq!(tag["codecraftJobKey"], "secret");
        // An explicit port to another IDE is not tagged with the environment's job.
        let other = BridgeArgs { discover: true, port: 4600, ..BridgeArgs::default() };
        let t = resolve_bridge(other, &env, Path::new("/wt/3")).unwrap();
        assert_eq!((t.args.port, t.args.job), (4600, None));
        // Without --ide the environment is ignored except for the key of an explicit job.
        let plain = BridgeArgs { port: 4555, job: Some(3), ..BridgeArgs::default() };
        let t = resolve_bridge(plain, &env, Path::new("/x")).unwrap();
        assert_eq!(t.job_key.as_deref(), Some("secret"));
    }

    #[test]
    fn client_configs_for_every_client() {
        let h = headless_server("/opt/Code Craft/codecraft-cli", "/w/app", "run,debug");
        let ide = ide_bridge_server("/usr/bin/codecraft-cli", true);
        assert_eq!(
            client_config(ClientKind::Claude, &h),
            "claude mcp add codecraft -- '/opt/Code Craft/codecraft-cli' mcp /w/app --allow run,debug"
        );
        assert_eq!(client_config(ClientKind::Claude, &ide), "claude mcp add --scope user codecraft-ide -- /usr/bin/codecraft-cli mcp --ide");
        let codex = client_config(ClientKind::Codex, &h);
        assert!(codex.contains("[mcp_servers.codecraft]") && codex.contains(r#"args = ["mcp", "/w/app", "--allow", "run,debug"]"#), "{codex}");
        assert!(codex.starts_with("codex mcp add codecraft -- "));
        for k in [ClientKind::Gemini, ClientKind::Cursor, ClientKind::Json] {
            let text = client_config(k, &h);
            let json_part = text.lines().skip_while(|l| l.starts_with('#')).collect::<Vec<_>>().join("\n");
            let v: Value = serde_json::from_str(&json_part).unwrap_or_else(|e| panic!("{k:?}: {e}: {text}"));
            assert_eq!(v["mcpServers"]["codecraft"]["command"], "/opt/Code Craft/codecraft-cli", "{k:?}");
            assert_eq!(v["mcpServers"]["codecraft"]["args"][1], "/w/app");
        }
        assert!(client_config(ClientKind::Gemini, &ide).contains("~/.gemini/settings.json"));
        assert_eq!(client_config(ClientKind::Grok, &ide), "grok mcp add codecraft-ide -- /usr/bin/codecraft-cli mcp --ide");
        assert!(client_config(ClientKind::Grok, &h).starts_with("grok mcp add --scope project codecraft -- "));
        assert_eq!(ide_bridge_server("/A/codecraft", false).args, ["--mcp-bridge", "--ide"]);
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        for k in ClientKind::ALL {
            assert_eq!(ClientKind::parse(k.id()), Some(k));
        }
        assert_eq!(ClientKind::parse("Claude-Code"), Some(ClientKind::Claude));
        assert_eq!(ClientKind::parse("vim"), None);
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("codecraft");
        assert_eq!(find_cli(&app), None);
        std::fs::write(tmp.path().join("codecraft-cli"), b"").unwrap();
        assert_eq!(find_cli(&app), Some(tmp.path().join("codecraft-cli")));
        assert_eq!(find_cli(Path::new("/x/codecraft-cli")), Some(PathBuf::from("/x/codecraft-cli")));
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
