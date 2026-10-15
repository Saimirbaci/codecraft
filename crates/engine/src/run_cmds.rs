//! Build and Run menu commands (Cargo-backed), plus Rust test/run at the caret.

use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation, TargetSelector};
use codecraft_syntax::{Language, SymbolKind};

use crate::commands::{CmdResult, CommandSpec, Context, has_project, ok, simple, spec};
use crate::params::{opt_path, opt_usize};
use crate::{CommandError, Popup, RunConfig, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("build.buildProject", "Build Project", has_project, build_project),
        simple("build.rebuildProject", "Rebuild Project", has_project, rebuild),
        spec("cargo.check", "Cargo Check", None, "", Context::Global, has_project, |s, _| {
            cargo(s, CargoCommand::Check, "Check", true)
        }),
        spec("cargo.clippy", "Run Clippy", None, "", Context::Global, has_project, |s, _| {
            cargo(s, CargoCommand::Clippy, "Clippy", true)
        }),
        spec("cargo.test", "Test Workspace", None, "", Context::Global, has_project, |s, _| {
            cargo(s, CargoCommand::Test, "Test", false)
        }),
        spec("cargo.doc", "Cargo Doc", None, "", Context::Global, has_project, |s, _| {
            cargo(s, CargoCommand::Doc, "Doc", true)
        }),
        spec(
            "run.run",
            "Run",
            None,
            "{config?: int, beforeLaunch?: bool (default true: run the configuration's before-launch tasks first)} — run the selected (or given) run configuration",
            Context::Global,
            has_project,
            run_selected,
        ),
        simple("run.chooseAndRun", "Run…", has_project, |s, _| {
            s.popup = Some(Popup::RunConfigs { selected: s.selected_run_config });
            ok()
        }),
        simple("run.stop", "Stop", has_project, stop),
        spec(
            "run.contextConfiguration",
            "Run Context Configuration",
            Some("Ctrl+Shift+R"),
            "{path?: string, line?: int} — run the test or main function at the caret (or given line)",
            Context::Editor,
            |s| crate::http_client::or_http_editor(s, has_project),
            run_at_caret,
        ),
        spec(
            "cargo.runTarget",
            "Run Cargo Target",
            None,
            "{package: string, kind: bin|example|test|bench|lib, name: string}",
            Context::Global,
            has_project,
            run_target,
        ),
        spec(
            "run.rerun",
            "Rerun",
            Some("Cmd+R"),
            "Rerun the active Run tab's command",
            Context::Global,
            has_project,
            rerun,
        ),
    ]
}

fn root(s: &Session) -> Result<std::path::PathBuf, CommandError> {
    s.ws.project
        .as_ref()
        .map(|p| p.cargo.as_ref().map(|m| m.workspace_root.clone()).unwrap_or_else(|| p.root.clone()))
        .ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

fn cargo(s: &mut Session, cmd: CargoCommand, title: &str, build: bool) -> CmdResult {
    let inv = CargoInvocation::new(cmd, root(s)?);
    let tab = crate::runs::start(s, inv, title, build)?;
    Ok(json!({ "tab": tab.0 }))
}

fn rebuild(s: &mut Session, _: &Value) -> CmdResult {
    let root = root(s)?;
    let mut clean = CargoInvocation::new(CargoCommand::Clean, root.clone());
    clean.json_messages = false;
    let build = CargoInvocation::new(CargoCommand::Build, root);
    let _ = s.save_all();
    let tab = crate::runs::start_chain(s, clean, Some(build), "Rebuild", true)?;
    Ok(json!({ "tab": tab.0 }))
}

fn is_cargo_project(s: &Session) -> bool {
    s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some() || p.root.join("Cargo.toml").is_file())
}

/// Build Project: Cargo, else the project's own build (CMake, Zig, npm run build, make).
fn build_project(s: &mut Session, p: &Value) -> CmdResult {
    if is_cargo_project(s) {
        return cargo(s, CargoCommand::Build, "Build", true);
    }
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let _ = p;
    match crate::run_configs::build_config(&root) {
        Some(config) => {
            let tab = crate::runs::start_config(s, &config)?;
            Ok(json!({ "tab": tab.0, "config": config.name }))
        }
        None => Err(CommandError::Failed(
            "No build system found (Cargo, CMake, Make, Zig or a package.json build script)".into(),
        )),
    }
}

pub(crate) fn run_selected(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(i) = opt_usize(p, "config")? {
        if i >= s.run_configs.len() {
            return Err(CommandError::BadParams(format!("no run configuration {i}")));
        }
        s.selected_run_config = i;
    }
    s.popup = None;
    if let Some(name) = s.run_configs.get(s.selected_run_config).map(|c| c.name.clone()) {
        crate::run_config_store::prepare_run(s, &name).map_err(CommandError::Failed)?;
        // Before-launch tasks first (M13 track RC); the configuration starts when they succeed.
        if crate::params::bool_or(p, "beforeLaunch", true)
            && let Some(v) = crate::before_launch::begin(s, &name, crate::before_launch::LaunchMode::Run)?
        {
            return Ok(v);
        }
    }
    // Remote Debug, Core Dump and Python Debug Server configurations only debug (M14 track DBG).
    let selected = s.run_configs.get(s.selected_run_config).map(|c| c.name.clone());
    if selected.is_some_and(|n| s.run_store.user.iter().any(|c| c.name.trim() == n && c.kind.debug_only())) {
        return crate::debug_cmds::debug_config(s, &json!({ "beforeLaunch": false }));
    }
    let config = match s.run_configs.get(s.selected_run_config) {
        Some(c) => c.clone(),
        None if is_cargo_project(s) => RunConfig::cargo("cargo run", CargoInvocation::new(CargoCommand::Run, root(s)?)),
        None => return run_at_caret(s, &Value::Null),
    };
    let tab = crate::runs::start_config(s, &config)?;
    Ok(json!({ "tab": tab.0, "config": config.name }))
}

fn stop(s: &mut Session, _: &Value) -> CmdResult {
    // Cmd+F2 stops the debug session first, like RustRover.
    if s.debug.is_active() {
        crate::debug::stop(s);
        return ok();
    }
    let target =
        s.ws.active_run
            .filter(|t| s.runs.is_running(*t))
            .or_else(|| s.ws.runs.iter().rev().find(|r| s.runs.is_running(r.id)).map(|r| r.id));
    match target {
        Some(t) => {
            crate::runs::stop(s, t);
            ok()
        }
        None => {
            s.status = "Nothing is running".into();
            ok()
        }
    }
}

fn rerun(s: &mut Session, _: &Value) -> CmdResult {
    let Some(tab) = s.ws.active_run.and_then(|t| s.ws.run(t)).cloned() else { return ok() };
    // Test runs remember their exact invocation (filters included).
    if let Some(started) = crate::testing::rerun_tab(s, tab.id) {
        return started.map(|t| json!({ "tab": t.0 }));
    }
    // Find the configuration by title, else rerun as a plain command line is not possible; fall back to Run.
    if let Some(idx) = s.run_configs.iter().position(|c| c.name == tab.title) {
        return run_selected(s, &json!({ "config": idx }));
    }
    match tab.title.as_str() {
        "Build" => cargo(s, CargoCommand::Build, "Build", true),
        "Check" => cargo(s, CargoCommand::Check, "Check", true),
        "Clippy" => cargo(s, CargoCommand::Clippy, "Clippy", true),
        "Test" => cargo(s, CargoCommand::Test, "Test", false),
        _ => run_selected(s, &Value::Null),
    }
}

/// What to run for a line: a `#[test]` fn → `cargo test <path> --exact`, `fn main` → `cargo run`,
/// a `mod tests` → all tests in that module.
pub(crate) fn context_invocation(
    s: &Session,
    path: &std::path::Path,
    line: usize,
) -> Option<(String, CargoInvocation)> {
    let project = s.ws.project.as_ref()?;
    let meta = project.cargo.as_ref()?;
    let package = meta.package_for_file(path)?;
    let text = match s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        Some(d) => d.doc.text(),
        None => codecraft_fs::read_text_file(path).ok()?,
    };
    if Language::from_path(path) != Language::Rust {
        return None;
    }
    let outline = codecraft_syntax::outline(Language::Rust, &text);
    // The item declared on (or nearest above) the line.
    let item = outline.iter().filter(|i| i.line <= line).max_by_key(|i| i.line)?;
    let lines: Vec<&str> = text.lines().collect();
    let is_test_fn = item.kind == SymbolKind::Function && {
        let from = item.line.saturating_sub(4);
        lines
            .get(from..item.line)
            .is_some_and(|ls| ls.iter().any(|l| l.trim_start().starts_with("#[test]") || l.contains("::test]")))
    };
    // Which target contains the file.
    let target = package
        .targets
        .iter()
        .filter(|t| t.src_path.parent().is_some_and(|d| path.starts_with(d)))
        .max_by_key(|t| t.src_path.parent().map(|d| d.as_os_str().len()).unwrap_or(0))?;
    let selector = if target.is_lib() {
        TargetSelector::Lib
    } else if target.is_bin() {
        TargetSelector::Bin(target.name.clone())
    } else if target.is_test() {
        TargetSelector::Test(target.name.clone())
    } else if target.is_example() {
        TargetSelector::Example(target.name.clone())
    } else if target.is_bench() {
        TargetSelector::Bench(target.name.clone())
    } else {
        return None;
    };
    // Module path from the file location and enclosing inline modules.
    let src_dir = target.src_path.parent()?;
    let mut module: Vec<String> = Vec::new();
    if let Ok(rel) = path.strip_prefix(src_dir) {
        let comps: Vec<String> = rel.with_extension("").iter().map(|c| c.to_string_lossy().into_owned()).collect();
        for (i, c) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if last && (c == "mod" || path == target.src_path) {
                continue;
            }
            module.push(c.clone());
        }
    }
    let mut enclosing: Vec<&codecraft_syntax::OutlineItem> =
        outline.iter().filter(|i| i.kind == SymbolKind::Module && i.line < item.line && i.depth < item.depth).collect();
    enclosing.sort_by_key(|i| i.depth);
    let mut depth_seen = std::collections::BTreeMap::new();
    for m in enclosing {
        depth_seen.insert(m.depth, m.name.clone());
    }
    module.extend(depth_seen.into_values());

    let root = meta.workspace_root.clone();
    if is_test_fn {
        let mut full = module.clone();
        full.push(item.name.clone());
        let name = full.join("::");
        let mut inv = CargoInvocation::new(CargoCommand::Test, root);
        inv.package = Some(package.name.clone());
        inv.target = Some(selector);
        inv.test_filter = Some(name.clone());
        inv.test_exact = true;
        return Some((format!("Test {}", item.name), inv));
    }
    if item.kind == SymbolKind::Module {
        let mut full = module.clone();
        full.push(item.name.clone());
        let mut inv = CargoInvocation::new(CargoCommand::Test, root);
        inv.package = Some(package.name.clone());
        inv.target = Some(selector);
        inv.test_filter = Some(format!("{}::", full.join("::")));
        return Some((format!("Test {}", full.join("::")), inv));
    }
    if item.kind == SymbolKind::Function && item.name == "main" && target.is_bin() {
        let mut inv = CargoInvocation::new(CargoCommand::Run, root);
        inv.package = Some(package.name.clone());
        inv.target = Some(TargetSelector::Bin(target.name.clone()));
        return Some((format!("Run {}", target.name), inv));
    }
    None
}

fn run_at_caret(s: &mut Session, p: &Value) -> CmdResult {
    let (path, line) = match (opt_path(p, "path"), opt_usize(p, "line")?) {
        (Some(path), Some(line)) => (path, line),
        _ => {
            let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
            let path = d.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved".into()))?;
            (path, d.doc.char_to_line(e.selections.primary().head))
        }
    };
    if codecraft_http::is_http_file(&path) {
        return crate::http_client::run_at(s, &path, line);
    }
    if let Some((title, inv)) = context_invocation(s, &path, line) {
        let tab = crate::runs::start(s, inv, &title, false)?;
        return Ok(json!({ "tab": tab.0, "title": title }));
    }
    // Other languages: Python, Zig, JS/TS, single-file C/C++.
    let text = match s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)) {
        Some(d) => d.doc.text(),
        None => codecraft_fs::read_text_file(&path).map_err(|e| CommandError::Failed(e.to_string()))?,
    };
    let root = s.project_root().unwrap_or_else(|| path.parent().map(std::path::Path::to_path_buf).unwrap_or_default());
    let Some(config) = crate::run_configs::context_config(&root, &path, &text, line) else {
        s.status = "Nothing to run here".into();
        return ok();
    };
    let tab = crate::runs::start_config(s, &config)?;
    Ok(json!({ "tab": tab.0, "title": config.name }))
}

fn run_target(s: &mut Session, p: &Value) -> CmdResult {
    let package = crate::params::req_str(p, "package")?.to_string();
    let kind = crate::params::req_str(p, "kind")?;
    let name = crate::params::req_str(p, "name")?.to_string();
    let (command, target, title) = match kind {
        "bin" => (CargoCommand::Run, TargetSelector::Bin(name.clone()), format!("Run {name}")),
        "example" => (CargoCommand::Run, TargetSelector::Example(name.clone()), format!("Run example {name}")),
        "test" => (CargoCommand::Test, TargetSelector::Test(name.clone()), format!("Test {name}")),
        "bench" => (CargoCommand::Bench, TargetSelector::Bench(name.clone()), format!("Bench {name}")),
        "lib" => (CargoCommand::Test, TargetSelector::Lib, format!("Test {package} lib")),
        other => return Err(CommandError::BadParams(format!("unknown target kind `{other}`"))),
    };
    let mut inv = CargoInvocation::new(command, root(s)?);
    inv.package = Some(package);
    inv.target = Some(target);
    let tab = crate::runs::start(s, inv, &title, false)?;
    Ok(json!({ "tab": tab.0 }))
}
