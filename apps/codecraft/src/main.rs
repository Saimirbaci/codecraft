//! Code Craft desktop app.
//!
//! `codecraft [--no-restore] [-r] [PATH[:LINE[:COL]]]…`: open the paths (files or directories), else
//! the last project(s) with their editors. A folder opens in its own window; `-r` reuses the
//! current window. With Code Craft already running, the installed `codecraft` launcher hands the
//! paths to that instance instead (`codecraft-handoff`). `--no-restore` starts without reopening
//! any project or editor.
//! `codecraft --version` prints the version and build.
//! `codecraft --mcp-bridge --port N [--lock-dir D] [--job J]`: an MCP stdio bridge to the
//! running IDE's server (what built-in agent jobs launch; see docs/agents.md); `--ide` instead of
//! `--port` finds the IDE (a job's environment, else the window whose project holds the working
//! directory).

// Release builds on Windows open no console window next to the app. Pipes a parent hands over
// (the MCP bridge's stdio) still work.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::ffi::OsString;
use std::path::PathBuf;

use codecraft_ui_egui::Launch;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // `codecraft --mcp-bridge --port N …`: the stdio MCP server built-in agent jobs are given; it
    // forwards to the running IDE's server (no window).
    let args: Vec<String> = std::env::args_os().skip(1).map(|v| v.to_string_lossy().into_owned()).collect();
    if args.first().map(String::as_str) == Some(codecraft_engine::agent_mcp::BRIDGE_FLAG) {
        std::process::exit(codecraft_engine::agent_mcp::bridge_main(&args));
    }
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        let info = codecraft_engine::about::BuildInfo::current();
        println!("{} {}", info.name, info.version_line());
        return Ok(());
    }
    // Explicit headless protocol diagnostic; never opens projects, agents or user stores.
    if args.first().map(String::as_str) == Some("--selfupdate-health-check") {
        codecraft_selfupdate::health::report_started();
        std::thread::sleep(codecraft_selfupdate::health::healthy_after());
        codecraft_selfupdate::health::report_healthy().map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
        std::thread::sleep(std::time::Duration::from_millis(250));
        println!("CodeCraft headless health protocol OK");
        return Ok(());
    }
    // Hold a shared running lease even if the background supervisor is interrupted.
    let _version_lease = if codecraft_selfupdate::health::is_current_payload()
        && let Ok(id) = std::env::var("CODECRAFT_VERSION_ID")
    {
        let base = codecraft_selfupdate::default_base().map_err(|e| eframe::Error::AppCreation(Box::new(e)))?;
        Some(
            codecraft_selfupdate::Store::new(base, PathBuf::new())
                .app_lease(&id)
                .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?,
        )
    } else {
        None
    };
    codecraft_ui_egui::launch(parse(std::env::args_os().skip(1)))
}

fn parse(args: impl Iterator<Item = OsString>) -> Launch {
    let args: Vec<OsString> = args.collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    let parsed = codecraft_handoff::parse_args(&args, &cwd);
    for option in &parsed.unknown {
        log::warn!("ignoring unknown option {}", option.to_string_lossy());
    }
    Launch {
        open: parsed.request.targets.first().map(|t| t.path.clone()),
        no_restore: parsed.no_restore,
        request: parsed.request,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Launch {
        parse(a.iter().map(OsString::from))
    }

    #[cfg(unix)]
    #[test]
    fn developer_path_preserves_non_utf8_without_a_launcher() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"/tmp/project-\xff".to_vec());
        assert_eq!(parse([path.clone()].into_iter()).open, Some(PathBuf::from(path)));
    }

    #[test]
    fn parses_path_and_no_restore() {
        let l = args(&[]);
        assert!(l.open.is_none() && !l.no_restore);
        let l = args(&["--no-restore", "/p", "/q", "--bogus"]);
        assert!(l.no_restore);
        assert_eq!(l.open, Some(PathBuf::from("/p")));
        let l = args(&["/p"]);
        assert_eq!((l.open, l.no_restore), (Some(PathBuf::from("/p")), false));
    }

    #[test]
    fn several_paths_positions_and_reuse_window() {
        let l = args(&["-r", "/p/a.rs:12:3", "/q"]);
        assert_eq!(l.open, Some(PathBuf::from("/p/a.rs")));
        assert!(l.request.reuse_window);
        assert_eq!(l.request.targets.len(), 2);
        assert_eq!((l.request.targets[0].line, l.request.targets[0].column), (Some(12), Some(3)));
        assert_eq!(l.request.targets[1].path, PathBuf::from("/q"));
        let none = args(&[]);
        assert!(none.request.targets.is_empty() && !none.request.reuse_window);
    }
}
