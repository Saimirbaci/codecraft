//! The agent subcommands (M14 track AX): the headless MCP server and one-shot debug, analyze,
//! profile and test runs. All of them go through the same engine tools agents call over MCP
//! (`codecraft_engine::agent_tools`), so the CLI and agents see the same results.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_access::{AgentAccess, Capability};
use codecraft_engine::agent_mcp;
use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};

const MCP_USAGE: &str = "usage: codecraft-cli mcp <project> [--allow run,debug,profile,write,db-write] [--no-lsp]
       codecraft-cli mcp --connect <port> [--lock-dir dir]
       codecraft-cli mcp --ide [--lock-dir dir]
       codecraft-cli mcp --print-config <claude|codex|gemini|cursor|grok|json> [project] [--allow caps] [--ide]

Serves CodeCraft's engine tools (debugging, analysis, profiling, tests, runs, databases) over MCP
on stdin/stdout. The session stays open across calls: debug sessions, profiles and test runs live
until the client disconnects. Reading is always allowed; everything else is denied unless listed
in --allow (there is nobody to approve it). `--connect` bridges to a running CodeCraft IDE instead
(its access policy and approval prompts apply); `--ide` finds that IDE itself (an agent job's
environment, else the CodeCraft window whose project holds the working directory), so one entry
in a client's user configuration serves every project. `--print-config` prints the client's
configuration for this binary (the headless server for [project], default the current directory,
or with --ide the IDE bridge) without writing any file.";

/// `mcp --print-config <client> [project] [--allow caps] [--ide]`.
fn print_config(args: &Args) -> Result<ExitCode, String> {
    let client = args.value("--print-config").unwrap_or_default();
    let kind = agent_mcp::ClientKind::parse(client).ok_or_else(|| {
        let ids: Vec<&str> = agent_mcp::ClientKind::ALL.iter().map(|k| k.id()).collect();
        format!("unknown client `{client}` ({})", ids.join(", "))
    })?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program's path: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe).display().to_string();
    let server = if args.has("--ide") {
        agent_mcp::ide_bridge_server(&exe, true)
    } else {
        let project = match args.positional.first() {
            Some(p) => PathBuf::from(p),
            None => std::env::current_dir().map_err(|e| format!("no current directory: {e}"))?,
        };
        if !project.is_dir() {
            return Err(format!("{} is not a directory", project.display()));
        }
        let project = project.canonicalize().unwrap_or(project);
        let allow = args.values("--allow");
        parse_allow(&allow)?;
        let allow = if allow.is_empty() { "run,debug,profile".to_string() } else { allow.join(",") };
        agent_mcp::headless_server(&exe, &project.display().to_string(), &allow)
    };
    println!("{}", agent_mcp::client_config(kind, &server));
    Ok(ExitCode::SUCCESS)
}

/// The one-shot commands act for the person who ran them: every capability is allowed.
fn everything() -> AgentAccess {
    AgentAccess::headless(&Capability::ALL)
}

fn call(s: &mut Session, name: &str, args: Value, timeout: Duration) -> Result<Value, String> {
    let r = agent_tools::call_blocking(s, Caller::new("codecraft-cli"), name, args, timeout);
    if r.is_error { Err(result_text(&r).to_string()) } else { Ok(result_json(&r)) }
}

fn print_json(v: &Value) -> Result<(), String> {
    println!("{}", serde_json::to_string_pretty(v).map_err(|e| e.to_string())?);
    Ok(())
}

/// `--flag value` pairs and positional arguments.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String], valued: &[&str], switches: &[&str]) -> Result<Self, String> {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            if valued.contains(&a.as_str()) {
                let v = it.next().ok_or_else(|| format!("`{a}` needs a value"))?;
                flags.push((a.clone(), Some(v.clone())));
            } else if switches.contains(&a.as_str()) {
                flags.push((a.clone(), None));
            } else if a.starts_with("--") {
                return Err(format!("unknown option `{a}`"));
            } else {
                positional.push(a.clone());
            }
        }
        Ok(Self { positional, flags })
    }

    fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|(f, _)| f == flag)
    }

    fn value(&self, flag: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(f, _)| f == flag).and_then(|(_, v)| v.as_deref())
    }

    fn values(&self, flag: &str) -> Vec<&str> {
        self.flags.iter().filter(|(f, _)| f == flag).filter_map(|(_, v)| v.as_deref()).collect()
    }
}

/// `--allow run,debug --allow write` as capabilities.
fn parse_allow(values: &[&str]) -> Result<Vec<Capability>, String> {
    let mut caps = Vec::new();
    for v in values {
        for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let cap = if part == "all" {
                caps.extend(Capability::ALL);
                continue;
            } else {
                Capability::parse(part).ok_or_else(|| {
                    format!("unknown capability `{part}` (read, run, debug, profile, write, db-write, all)")
                })?
            };
            caps.push(cap);
        }
    }
    Ok(caps)
}

// ---------------------------------------------------------------- mcp

pub(crate) fn mcp(raw: &[String]) -> Result<ExitCode, String> {
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "codecraft-cli".into());
        println!("{MCP_USAGE}\n");
        println!("{}", agent_mcp::client_config_help(&exe, "/path/to/project"));
        println!("Tools ({}):", agent_mcp::tool_names().len());
        if let Value::Object(caps) = agent_mcp::tool_capabilities() {
            for (name, cap) in caps {
                println!("  {name:<26} {}", cap.as_str().unwrap_or_default());
            }
        }
        println!("\nSee docs/agents.md for every tool with an example call and result.");
        return Ok(ExitCode::SUCCESS);
    }
    let args = Args::parse(raw, &["--allow", "--connect", "--lock-dir", "--print-config"], &["--no-lsp", "--ide"])?;
    if args.value("--print-config").is_some() {
        return print_config(&args);
    }
    let port = args.value("--connect");
    if port.is_some() || args.has("--ide") {
        let mut bridge = vec![agent_mcp::BRIDGE_FLAG.to_string()];
        match port {
            Some(port) => bridge.extend(["--port".to_string(), port.to_string()]),
            None => bridge.push("--ide".into()),
        }
        if let Some(dir) = args.value("--lock-dir") {
            bridge.push("--lock-dir".into());
            bridge.push(dir.to_string());
        }
        return Ok(crate::exit_code(agent_mcp::bridge_main(&bridge)));
    }
    let project = args.positional.first().ok_or(MCP_USAGE)?;
    let allowed = parse_allow(&args.values("--allow"))?;
    let root = PathBuf::from(project);
    if !root.is_dir() {
        return Err(format!("{project} is not a directory"));
    }
    let mut s = Session::new();
    s.lsp_enabled = !args.has("--no-lsp");
    // The IDE integration (a lock file for Claude Code's /ide) is for the desktop app only.
    s.execute("ide.disable", &Value::Null).map_err(|e| e.to_string())?;
    s.agent_access = AgentAccess::headless(&allowed);
    s.execute("file.open", &json!({ "path": root })).map_err(|e| e.to_string())?;
    let caps: Vec<&str> =
        Capability::ALL.into_iter().filter(|c| *c == Capability::Read || allowed.contains(c)).map(|c| c.id()).collect();
    eprintln!("codecraft-cli mcp: serving {} on stdio (allowed: {})", root.display(), caps.join(", "));
    let stdin = BufReader::new(std::io::stdin());
    let result = agent_mcp::serve_stdio(&mut s, stdin, std::io::stdout());
    s.shutdown();
    result.map(|()| ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- debug

/// `file:line` (the file relative to the project or absolute).
fn parse_break(spec: &str) -> Result<(String, u64), String> {
    let (file, line) = spec.rsplit_once(':').ok_or_else(|| format!("`{spec}` is not file:line"))?;
    let line = line.parse::<u64>().ok().filter(|l| *l > 0).ok_or_else(|| format!("`{line}` is not a line number"))?;
    if file.is_empty() {
        return Err(format!("`{spec}` has no file"));
    }
    Ok((file.to_string(), line))
}

fn secs(args: &Args, flag: &str, default: u64) -> Result<Duration, String> {
    match args.value(flag) {
        Some(v) => v.parse::<u64>().map(Duration::from_secs).map_err(|_| format!("`{v}` is not a number of seconds")),
        None => Ok(Duration::from_secs(default)),
    }
}

/// A configuration name or index, or a file to debug.
fn target_args(project: &Path, target: &str) -> Value {
    let as_file = if Path::new(target).is_absolute() { PathBuf::from(target) } else { project.join(target) };
    if as_file.is_file() {
        return json!({ "file": as_file });
    }
    match target.parse::<u64>() {
        Ok(i) => json!({ "config": i }),
        Err(_) => json!({ "config": target }),
    }
}

fn print_stop(stop: &Value) {
    let state = stop["state"].as_str().unwrap_or("?");
    match state {
        "stopped" => {
            let reason = stop["reason"].as_str().unwrap_or("?");
            let thread = stop["thread"]["name"].as_str().unwrap_or("");
            println!("Stopped ({reason}) in thread {thread}");
            for f in stop["frames"].as_array().into_iter().flatten() {
                let at = match (f["file"].as_str(), f["line"].as_u64()) {
                    (Some(file), Some(line)) => format!("{file}:{line}"),
                    _ => "<no source>".into(),
                };
                println!("  #{} {} at {at}", f["index"], f["name"].as_str().unwrap_or("?"));
            }
            if let Some(ex) = stop["frames"][0]["excerpt"].as_str() {
                println!("\n{ex}\n");
            }
            if let Some(locals) = stop["locals"].as_array() {
                println!("Locals:");
                for v in locals {
                    let ty = v["type"].as_str().map(|t| format!(": {t}")).unwrap_or_default();
                    println!("  {}{ty} = {}", v["name"].as_str().unwrap_or("?"), v["value"].as_str().unwrap_or(""));
                }
            }
        }
        "exited" => println!("The program exited (code {}) without stopping", stop["exitCode"]),
        "failed" => println!("Debugging failed: {}", stop["message"].as_str().unwrap_or("?")),
        other => println!("Not stopped yet ({other}); timed out"),
    }
    if let Some(out) = stop["output"].as_array().filter(|o| !o.is_empty()) {
        println!("Output:");
        for l in out {
            println!("  {}", l.as_str().unwrap_or(""));
        }
    }
}

pub(crate) fn debug(raw: &[String]) -> Result<ExitCode, String> {
    let args = Args::parse(raw, &["--break", "--eval", "--timeout", "--frames"], &["--json"])?;
    let usage =
        "usage: codecraft-cli debug <project> <config|file> --break file:line [--eval expr]… [--json] [--timeout secs]";
    let project = args.positional.first().ok_or(usage)?;
    let target = args.positional.get(1).ok_or(usage)?;
    let breaks = args.values("--break").into_iter().map(parse_break).collect::<Result<Vec<_>, _>>()?;
    let timeout = secs(&args, "--timeout", 300)?;
    let frames = args.value("--frames").and_then(|f| f.parse::<u64>().ok()).unwrap_or(5);
    let mut s = crate::open_project(Some(project))?;
    s.agent_access = everything();
    let root = s.project_root().unwrap_or_else(|| PathBuf::from(project));
    for (file, line) in &breaks {
        call(&mut s, "debug_breakpoint_set", json!({ "path": file, "line": line }), timeout)?;
    }
    call(&mut s, "debug_start", target_args(&root, target), timeout)?;
    let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let stop = call(
        &mut s,
        "debug_wait_for_stop",
        json!({ "timeoutMs": ms, "frames": frames }),
        timeout + Duration::from_secs(5),
    )?;
    let mut evaluations = Vec::new();
    if stop["state"] == "stopped" {
        for expr in args.values("--eval") {
            let r = agent_tools::call_blocking(
                &mut s,
                Caller::new("codecraft-cli"),
                "debug_evaluate",
                json!({ "expression": expr }),
                timeout,
            );
            evaluations.push(if r.is_error {
                json!({ "expression": expr, "error": result_text(&r) })
            } else {
                result_json(&r)
            });
        }
    }
    let _ = call(&mut s, "debug_control", json!({ "action": "stop" }), Duration::from_secs(30));
    // Let the adapter and the debuggee go away.
    crate::pump_until(&mut s, Duration::from_secs(10), |s| !s.debug.busy());
    s.shutdown();
    let stopped = stop["state"] == "stopped";
    if args.has("--json") {
        print_json(&json!({ "stop": stop, "evaluations": evaluations }))?;
    } else {
        print_stop(&stop);
        for e in &evaluations {
            match e.get("error").and_then(Value::as_str) {
                Some(err) => println!("{} → error: {err}", e["expression"].as_str().unwrap_or("?")),
                None => println!("{} = {}", e["expression"].as_str().unwrap_or("?"), e["value"].as_str().unwrap_or("")),
            }
        }
    }
    Ok(if stopped { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

// ---------------------------------------------------------------- analyze

fn sarif(findings: &[Value], root: &Path) -> Value {
    let mut rules: Vec<String> = Vec::new();
    let results: Vec<Value> = findings
        .iter()
        .map(|f| {
            let rule = f["inspection"].as_str().unwrap_or("problem").to_string();
            if !rules.contains(&rule) {
                rules.push(rule.clone());
            }
            let level = match f["severity"].as_str().unwrap_or("") {
                s if s.eq_ignore_ascii_case("error") => "error",
                s if s.eq_ignore_ascii_case("warning") => "warning",
                _ => "note",
            };
            let file = f["file"].as_str().unwrap_or("");
            let uri = Path::new(file).strip_prefix(root).map(|p| p.display().to_string()).unwrap_or_else(|_| file.to_string());
            json!({
                "ruleId": rule,
                "level": level,
                "message": { "text": f["message"].as_str().unwrap_or("") },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": uri, "uriBaseId": "%SRCROOT%" },
                        "region": { "startLine": f["line"].as_u64().unwrap_or(1), "startColumn": f["column"].as_u64().unwrap_or(1) },
                    }
                }],
            })
        })
        .collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": {
                "name": "CodeCraft",
                "informationUri": "https://github.com/codecraft-ide",
                "version": env!("CARGO_PKG_VERSION"),
                "rules": rules.iter().map(|r| json!({ "id": r })).collect::<Vec<_>>(),
            } },
            "originalUriBaseIds": { "%SRCROOT%": { "uri": format!("file://{}/", root.display()) } },
            "results": results,
        }],
    })
}

pub(crate) fn analyze(raw: &[String]) -> Result<ExitCode, String> {
    let args = Args::parse(raw, &["--scope", "--format", "--tools", "--timeout", "--limit"], &[])?;
    let project = args
        .positional
        .first()
        .ok_or("usage: codecraft-cli analyze <project> [--scope path] [--format json|sarif|text] [--tools a,b]")?;
    let format = args.value("--format").unwrap_or("text");
    if !matches!(format, "text" | "json" | "sarif") {
        return Err(format!("unknown format `{format}` (json, sarif, text)"));
    }
    let timeout = secs(&args, "--timeout", 1800)?;
    let mut s = crate::open_project(Some(project))?;
    s.agent_access = everything();
    let root = s.project_root().unwrap_or_else(|| PathBuf::from(project));
    let mut params = json!({ "scope": "project", "limit": args.value("--limit").and_then(|l| l.parse::<u64>().ok()).unwrap_or(2000) });
    if let (Some(scope), Some(o)) = (args.value("--scope"), params.as_object_mut()) {
        let path = if Path::new(scope).is_absolute() { PathBuf::from(scope) } else { root.join(scope) };
        let kind = if path.is_dir() { "directory" } else { "file" };
        o.insert("scope".into(), json!(kind));
        o.insert("path".into(), json!(path));
    }
    if let (Some(tools), Some(o)) = (args.value("--tools"), params.as_object_mut()) {
        o.insert("tools".into(), json!(tools.split(',').map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>()));
    }
    if let Some(o) = params.as_object_mut() {
        o.insert("timeoutMs".into(), json!(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)));
    }
    let res = call(&mut s, "inspect", params, timeout + Duration::from_secs(10))?;
    s.shutdown();
    let findings = res["findings"].as_array().cloned().unwrap_or_default();
    let errors =
        findings.iter().filter(|f| f["severity"].as_str().is_some_and(|x| x.eq_ignore_ascii_case("error"))).count();
    match format {
        "json" => print_json(&res)?,
        "sarif" => print_json(&sarif(&findings, &root))?,
        _ => {
            for f in &findings {
                println!(
                    "{}:{}:{}: {}: {} [{}]",
                    f["file"].as_str().unwrap_or("?"),
                    f["line"].as_u64().unwrap_or(0),
                    f["column"].as_u64().unwrap_or(0),
                    f["severity"].as_str().unwrap_or("?").to_lowercase(),
                    f["message"].as_str().unwrap_or(""),
                    f["inspection"].as_str().unwrap_or(""),
                );
            }
            println!("{} finding(s), {errors} error(s); tools: {}", findings.len(), res["tools"]);
            if let Some(note) = res.get("note").and_then(Value::as_str) {
                println!("{note}");
            }
        }
    }
    Ok(if errors > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

// ---------------------------------------------------------------- profile

pub(crate) fn profile(raw: &[String]) -> Result<ExitCode, String> {
    let args = Args::parse(raw, &["--seconds", "--limit", "--timeout"], &["--json"])?;
    let usage = "usage: codecraft-cli profile <project> <config> [--seconds N] [--json]";
    let project = args.positional.first().ok_or(usage)?;
    let config = args.positional.get(1).ok_or(usage)?;
    let limit = args.value("--limit").and_then(|l| l.parse::<u64>().ok()).unwrap_or(20);
    let timeout = secs(&args, "--timeout", 1800)?;
    let mut s = crate::open_project(Some(project))?;
    s.agent_access = everything();
    let cfg = match config.parse::<u64>() {
        Ok(i) => json!(i),
        Err(_) => json!(config),
    };
    let started = call(&mut s, "profile_start", json!({ "config": cfg }), timeout)?;
    eprintln!("profiling with {}…", started["tool"].as_str().unwrap_or("the profiler"));
    if let Some(n) = args.value("--seconds") {
        let n = n.parse::<u64>().map_err(|_| format!("`{n}` is not a number of seconds"))?;
        let until = Instant::now() + Duration::from_secs(n);
        while Instant::now() < until && !s.profiler.recordings.is_empty() {
            s.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let name = if s.profiler.recordings.is_empty() { "profile_hotspots" } else { "profile_stop" };
    let hot = call(&mut s, name, json!({ "limit": limit, "timeoutMs": ms }), timeout + Duration::from_secs(10))?;
    s.shutdown();
    if args.has("--json") {
        print_json(&hot)?;
    } else {
        println!("{:>7} {:>7}  function", "self%", "total%");
        for h in hot["hotspots"].as_array().into_iter().flatten() {
            let at = match (h["file"].as_str(), h["line"].as_u64()) {
                (Some(f), Some(l)) => format!("  {f}:{l}"),
                (Some(f), None) => format!("  {f}"),
                _ => String::new(),
            };
            println!(
                "{:>7.1} {:>7.1}  {}{at}",
                h["selfPercent"].as_f64().unwrap_or(0.0),
                h["totalPercent"].as_f64().unwrap_or(0.0),
                h["name"].as_str().unwrap_or("?")
            );
        }
        println!("snapshot: {}", hot["snapshot"].as_str().unwrap_or("?"));
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- test

pub(crate) fn test(raw: &[String]) -> Result<ExitCode, String> {
    let args = Args::parse(raw, &["--timeout", "--file"], &["--json"])?;
    let project =
        args.positional.first().ok_or("usage: codecraft-cli test <project> [filter] [--file path] [--json]")?;
    let timeout = secs(&args, "--timeout", 3600)?;
    let mut s = crate::open_project(Some(project))?;
    s.agent_access = everything();
    let mut params = json!({ "timeoutMs": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX) });
    if let Some(o) = params.as_object_mut() {
        if let Some(filter) = args.positional.get(1) {
            o.insert("name".into(), json!(filter));
        } else if let Some(file) = args.value("--file") {
            o.insert("path".into(), json!(file));
        }
    }
    let res = call(&mut s, "test_run", params, timeout + Duration::from_secs(10))?;
    s.shutdown();
    let failed = res["counts"]["failed"].as_u64().unwrap_or(0);
    if args.has("--json") {
        print_json(&res)?;
    } else {
        let c = &res["counts"];
        println!(
            "{}: {} passed, {failed} failed, {} ignored",
            res["title"].as_str().unwrap_or("Tests"),
            c["passed"],
            c["ignored"]
        );
        for f in res["failures"].as_array().into_iter().flatten() {
            let at = match (f["file"].as_str(), f["line"].as_u64()) {
                (Some(file), Some(line)) => format!(" ({file}:{line})"),
                _ => String::new(),
            };
            println!("FAILED {}{at}", f["name"].as_str().unwrap_or("?"));
            if let Some(m) = f["message"].as_str() {
                println!("  {}", m.lines().take(5).collect::<Vec<_>>().join("\n  "));
            }
        }
        if let Some(note) = res.get("note").and_then(Value::as_str) {
            println!("{note}");
        }
    }
    Ok(if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaks_and_allow_lists_parse() {
        assert_eq!(parse_break("src/main.rs:12").unwrap(), ("src/main.rs".to_string(), 12));
        assert_eq!(parse_break("C:/x.rs:3").unwrap(), ("C:/x.rs".to_string(), 3));
        assert!(parse_break("main.rs").is_err());
        assert!(parse_break("main.rs:0").is_err());
        assert!(parse_break(":4").is_err());
        assert_eq!(
            parse_allow(&["run,debug", "db-write"]).unwrap(),
            vec![Capability::Run, Capability::Debug, Capability::DbWrite]
        );
        assert_eq!(parse_allow(&["all"]).unwrap().len(), Capability::ALL.len());
        assert!(parse_allow(&["root"]).is_err());
        let a = Args::parse(
            &["p".into(), "--json".into(), "--eval".into(), "x".into(), "--eval".into(), "y".into()],
            &["--eval"],
            &["--json"],
        )
        .unwrap();
        assert_eq!(a.positional, ["p"]);
        assert!(a.has("--json"));
        assert_eq!(a.values("--eval"), ["x", "y"]);
        assert!(Args::parse(&["--bogus".into()], &[], &[]).is_err());
        assert!(Args::parse(&["--eval".into()], &["--eval"], &[]).is_err());
    }

    #[test]
    fn sarif_has_rules_and_relative_uris() {
        let f = vec![
            json!({ "file": "/p/src/a.rs", "line": 3, "column": 2, "severity": "Warning", "message": "m", "inspection": "clippy::x" }),
        ];
        let v = sarif(&f, Path::new("/p"));
        assert_eq!(v["version"], "2.1.0");
        let r = &v["runs"][0]["results"][0];
        assert_eq!(r["level"], "warning");
        assert_eq!(r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"], "src/a.rs");
        assert_eq!(v["runs"][0]["tool"]["driver"]["rules"][0]["id"], "clippy::x");
    }

    #[test]
    fn targets_are_files_or_configurations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.py"), "").unwrap();
        assert!(target_args(dir.path(), "app.py")["file"].is_string());
        assert_eq!(target_args(dir.path(), "2")["config"], 2);
        assert_eq!(target_args(dir.path(), "Run app")["config"], "Run app");
    }
}
