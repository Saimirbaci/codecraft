//! CodeCraft headless CLI: drive the same engine the IDE uses.
//!
//! ```text
//! codecraft-cli commands [--json]              list every command (id, label, shortcut, menu)
//! codecraft-cli catalog --json                 the observed menu catalog with live flags
//! codecraft-cli check <project> [--clippy]     cargo check/clippy through the engine; prints problems
//! codecraft-cli run <project> <id> [json]      open a project, run one command, print the result
//! codecraft-cli configs <project> <list | add | update <i> | copy <i> | delete <i> | run <name|i>> [flags]
//!                                              manage the project's run configurations
//!   flags: --name N  --kind cargo|command|python|cpp|zig|npm  --command "…"
//!          --env A=1 (repeatable)  --env-file .env  --cwd dir
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::catalog::CATALOG;
use codecraft_engine::{Session, registry};
use codecraft_workspace::Severity;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("commands") => commands(args.iter().any(|a| a == "--json")),
        Some("catalog") => catalog(),
        Some("check") => check(args.get(1), args.iter().any(|a| a == "--clippy")),
        Some("run") => run(args.get(1), args.get(2), args.get(3)),
        Some("configs") => configs(args.get(1), args.get(2..).unwrap_or_default()),
        _ => Err("usage: codecraft-cli <commands [--json] | catalog --json | check <project> [--clippy] | run <project> <command-id> [json-params] | configs <project> <list|add|update|copy|delete|run> [flags]>".into()),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("codecraft-cli: {e}");
            ExitCode::FAILURE
        }
    }
}

fn commands(as_json: bool) -> Result<ExitCode, String> {
    let reg = registry();
    let list: Vec<Value> = reg
        .all()
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.label,
                "shortcuts": reg.shortcuts(c.id),
                "menu": reg.menu_path(c.id),
                "params": c.params,
            })
        })
        .collect();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?);
    } else {
        for c in reg.all() {
            let sc = reg.shortcuts(c.id).join(", ");
            println!("{:<52} {:<40} {sc}", c.id, c.label);
        }
        println!("\n{} commands", reg.all().len());
    }
    Ok(ExitCode::SUCCESS)
}

fn catalog() -> Result<ExitCode, String> {
    let reg = registry();
    let list: Vec<Value> = CATALOG
        .iter()
        .map(|c| {
            json!({
                "path": c.path,
                "label": c.label,
                "shortcut": c.shortcut,
                "id": c.id,
                "submenu": c.submenu,
                "scope": format!("{:?}", c.scope),
                "live": reg.contains(c.id),
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&list).map_err(|e| e.to_string())?);
    Ok(ExitCode::SUCCESS)
}

fn pump_until(s: &mut Session, timeout: Duration, mut done: impl FnMut(&Session) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        s.pump();
        if done(s) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn open_project(path: Option<&String>) -> Result<Session, String> {
    let path = PathBuf::from(path.ok_or("missing project path")?);
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": path })).map_err(|e| e.to_string())?;
    if !pump_until(&mut s, Duration::from_secs(120), |s| !s.jobs_running()) {
        return Err("timed out loading the project".into());
    }
    Ok(s)
}

fn check(path: Option<&String>, clippy: bool) -> Result<ExitCode, String> {
    let mut s = open_project(path)?;
    let cmd = if clippy { "cargo.clippy" } else { "cargo.check" };
    s.execute(cmd, &Value::Null).map_err(|e| e.to_string())?;
    if !pump_until(&mut s, Duration::from_secs(1800), |s| !s.jobs_running()) {
        return Err("timed out waiting for cargo".into());
    }
    let mut errors = 0usize;
    for (path, problems) in s.ws.all_problems() {
        for p in problems {
            if p.severity == Severity::Error {
                errors += 1;
            }
            println!(
                "{}:{}:{}: {}: {}",
                path.display(),
                p.start.line + 1,
                p.start.column + 1,
                p.severity.label(),
                p.message
            );
        }
    }
    let (e, w) = s.ws.problem_counts();
    println!("{e} error(s), {w} warning(s)");
    Ok(if errors > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn run(path: Option<&String>, id: Option<&String>, params: Option<&String>) -> Result<ExitCode, String> {
    let mut s = open_project(path)?;
    let id = id.ok_or("missing command id")?;
    let params: Value = match params {
        Some(p) => serde_json::from_str(p).map_err(|e| format!("params: {e}"))?,
        None => Value::Null,
    };
    let result = s.execute(id, &params).map_err(|e| e.to_string())?;
    pump_until(&mut s, Duration::from_secs(1800), |s| !s.jobs_running());
    println!("{}", serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?);
    if !s.status.is_empty() {
        eprintln!("{}", s.status);
    }
    for run in &s.ws.runs {
        for line in &run.lines {
            println!("{}", line.text);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `configs <project> <action> …`: `runConfig.*` with command-line flags instead of JSON.
fn configs(path: Option<&String>, rest: &[String]) -> Result<ExitCode, String> {
    let action = rest.first().map(String::as_str).ok_or("missing action (list, add, update, copy, delete, run)")?;
    let (index, flags) = match action {
        "update" | "copy" | "delete" | "run" => (rest.get(1), rest.get(2..).unwrap_or_default()),
        _ => (None, rest.get(1..).unwrap_or_default()),
    };
    let params = config_params(flags)?;
    let mut s = open_project(path)?;
    let index_of = |what: &str| -> Result<usize, String> {
        let arg = index.ok_or_else(|| format!("`{what}` needs a configuration index"))?;
        arg.parse::<usize>().map_err(|_| format!("`{arg}` is not a configuration index (see `configs <project> list`)"))
    };
    let result = match action {
        "list" => s.execute("runConfig.list", &Value::Null),
        "add" => s.execute("runConfig.create", &params),
        "update" => {
            let mut p = params;
            if let Value::Object(m) = &mut p {
                m.insert("index".into(), json!(index_of(action)?));
            }
            s.execute("runConfig.update", &p)
        }
        "copy" => s.execute("runConfig.copy", &json!({ "index": index_of(action)? })),
        "delete" => s.execute("runConfig.delete", &json!({ "index": index_of(action)? })),
        "run" => {
            let arg = index.ok_or("`run` needs a configuration name or index")?;
            let i = match arg.parse::<usize>() {
                Ok(i) => i,
                Err(_) => s
                    .run_configs
                    .iter()
                    .position(|c| &c.name == arg)
                    .ok_or_else(|| format!("no run configuration named `{arg}`"))?,
            };
            s.execute("run.run", &json!({ "config": i }))
        }
        other => return Err(format!("unknown action `{other}` (list, add, update, copy, delete, run)")),
    }
    .map_err(|e| e.to_string())?;
    pump_until(&mut s, Duration::from_secs(1800), |s| !s.jobs_running());
    println!("{}", serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?);
    if action == "run" {
        for run in &s.ws.runs {
            for line in &run.lines {
                println!("{}", line.text);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `--name N --kind K --command C --env A=1 --env-file F --cwd D` as `runConfig.create` params.
fn config_params(flags: &[String]) -> Result<Value, String> {
    let mut p = serde_json::Map::new();
    let mut env: Vec<String> = Vec::new();
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("`{flag}` needs a value"));
        match flag.as_str() {
            "--name" => p.insert("name".into(), json!(value()?)),
            "--kind" => p.insert("kind".into(), json!(value()?)),
            "--command" => p.insert("command".into(), json!(value()?)),
            "--env" => {
                env.push(value()?);
                None
            }
            "--env-file" => p.insert("envFile".into(), json!(value()?)),
            "--cwd" => p.insert("workingDir".into(), json!(value()?)),
            other => return Err(format!("unknown flag `{other}`")),
        };
    }
    if !env.is_empty() {
        p.insert("env".into(), json!(env));
    }
    Ok(Value::Object(p))
}
