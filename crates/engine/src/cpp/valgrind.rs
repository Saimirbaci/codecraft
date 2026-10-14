//! Valgrind Memcheck (track CL): Run › Run with Valgrind Memcheck runs the selected
//! configuration under `valgrind --xml=yes --xml-file=…` (Linux; on macOS and Windows Valgrind
//! does not exist and the command says so), and Run › Import Valgrind XML Results reads a saved
//! report. The XML becomes a tree of errors (kind, message, stacks with source links) shown in
//! the run's Memcheck view.
//!
//! How the configuration is wrapped: a build-then-run wrapper (`sh -c '<build> && exec "$@"'`)
//! gets Valgrind in front of its `exec`; a plain program becomes `valgrind <opts> <program>`;
//! other shell scripts run under `--trace-children=yes` (build tools skipped) with one XML file
//! per process (`%p`); Cargo runs get Valgrind as the target runner
//! (`CARGO_TARGET_<TRIPLE>_RUNNER`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{ConsoleKind, NotificationLevel, RunStatus, RunTabId, ToolWindowId};

use super::xml::{self, Element};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{opt_path, opt_usize, req_usize};
use crate::runs::{ProcessCommand, RunConfig};
use crate::{CommandError, Session};

/// Errors kept per result.
const MAX_ERRORS: usize = 10_000;
const MAX_FRAMES: usize = 200;
const MAX_XML_BYTES: u64 = 256 * 1024 * 1024;
/// Build tools a traced shell script runs (not worth checking).
const SKIP_CHILDREN: &str =
    "*/cmake,*/ctest,*/make,*/gmake,*/ninja,*/cc,*/c++,*/gcc*,*/g++*,*/clang*,*/ld,*/ld.*,*/as,*/mkdir,*/rm,*/cargo,*/rustc";

pub const UNAVAILABLE: &str = "Valgrind is not available on this system: it runs on Linux. Use a sanitizer instead \
    (AddressSanitizer finds the same memory errors): cpp.runConfig.setSanitizers for a C/C++ configuration or \
    cmake.setSanitizers for the CMake profiles.";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemFrame {
    pub ip: String,
    pub function: Option<String>,
    pub object: Option<String>,
    /// `dir/file` when Valgrind knows the source.
    pub path: Option<PathBuf>,
    /// 0-based.
    pub line: Option<usize>,
    /// In the project's own sources.
    pub project: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemStack {
    /// `Invalid write of size 4`, `Address 0x… is 0 bytes after a block of size 16 alloc'd`.
    pub title: String,
    pub frames: Vec<MemFrame>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemError {
    /// `InvalidWrite`, `Leak_DefinitelyLost`, `UninitCondition`, …
    pub kind: String,
    pub what: String,
    pub tid: Option<u64>,
    pub stacks: Vec<MemStack>,
    pub leaked_bytes: Option<u64>,
    pub leaked_blocks: Option<u64>,
    /// How many times it happened (`errorcounts`).
    pub count: u64,
    /// The first project frame (else the first frame with a source).
    pub location: Option<(PathBuf, usize)>,
    /// The process it came from (several with `--trace-children`).
    pub pid: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemcheckResult {
    pub errors: Vec<MemError>,
    /// `./app arg` of each checked process.
    pub commands: Vec<String>,
    /// The report ended properly (`<status><state>FINISHED`).
    pub finished: bool,
    pub fatal_signal: Option<String>,
    pub source: String,
}

impl MemcheckResult {
    /// Errors by kind label, for the tree's top level.
    pub fn by_kind(&self) -> Vec<(String, Vec<usize>)> {
        let mut out: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, e) in self.errors.iter().enumerate() {
            let label = kind_label(&e.kind).to_string();
            match out.iter_mut().find(|(k, _)| *k == label) {
                Some((_, v)) => v.push(i),
                None => out.push((label, vec![i])),
            }
        }
        out
    }
}

/// CLion-style group names for Memcheck error kinds.
pub fn kind_label(kind: &str) -> &str {
    match kind {
        "InvalidRead" => "Invalid read",
        "InvalidWrite" => "Invalid write",
        "InvalidFree" => "Invalid free",
        "MismatchedFree" => "Mismatched free / delete / delete[]",
        "InvalidJump" => "Invalid jump",
        "UninitCondition" => "Uninitialized value in conditional jump",
        "UninitValue" => "Use of uninitialized value",
        "SyscallParam" => "Syscall param",
        "ClientCheck" => "Client check",
        "Overlap" => "Overlapping memory",
        "Leak_DefinitelyLost" => "Definitely lost",
        "Leak_IndirectlyLost" => "Indirectly lost",
        "Leak_PossiblyLost" => "Possibly lost",
        "Leak_StillReachable" => "Still reachable",
        "InvalidMemPool" => "Invalid mempool",
        "FishyValue" => "Fishy value",
        other => other,
    }
}

fn frame(f: &Element, in_project: &dyn Fn(&Path) -> bool) -> MemFrame {
    let path = match (f.child_text("dir"), f.child_text("file")) {
        (Some(d), Some(file)) if !d.is_empty() => Some(Path::new(d).join(file)),
        (_, Some(file)) if !file.is_empty() => Some(PathBuf::from(file)),
        _ => None,
    };
    MemFrame {
        ip: f.child_text("ip").unwrap_or("").to_string(),
        function: f.child_text("fn").filter(|x| !x.is_empty()).map(str::to_string),
        object: f.child_text("obj").filter(|x| !x.is_empty()).map(str::to_string),
        project: path.as_deref().is_some_and(in_project),
        line: f.child_text("line").and_then(|l| l.parse::<usize>().ok()).map(|l| l.saturating_sub(1)),
        path,
    }
}

fn stack(e: &Element, title: String, in_project: &dyn Fn(&Path) -> bool) -> MemStack {
    MemStack { title, frames: e.children_named("frame").take(MAX_FRAMES).map(|f| frame(f, in_project)).collect() }
}

/// Parse a Memcheck XML report (`--xml=yes`). Truncated reports (the program crashed or was
/// killed) give the errors read so far.
pub fn parse_xml(text: &str, in_project: &dyn Fn(&Path) -> bool) -> MemcheckResult {
    let doc = xml::parse(text);
    let mut out = MemcheckResult::default();
    let Some(root) = doc.find("valgrindoutput") else { return out };
    let pid = root.child_text("pid").and_then(|p| p.parse().ok());
    if let Some(argv) = root.child("args").and_then(|a| a.child("argv")) {
        let words: Vec<&str> = argv
            .children
            .iter()
            .filter(|c| c.name == "exe" || c.name == "arg")
            .map(|c| c.text.trim())
            .collect();
        out.commands.push(words.join(" "));
    }
    out.finished = root.children_named("status").any(|s| s.child_text("state") == Some("FINISHED"));
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(ec) = root.child("errorcounts") {
        for pair in ec.children_named("pair") {
            if let (Some(u), Some(c)) = (pair.child_text("unique"), pair.child_text("count").and_then(|c| c.parse().ok())) {
                counts.insert(u.to_string(), c);
            }
        }
    }
    for e in root.children.iter() {
        if out.errors.len() >= MAX_ERRORS {
            break;
        }
        if e.name == "fatal_signal" {
            let name = e.child_text("signame").unwrap_or("signal");
            out.fatal_signal = Some(format!("Process terminated with {name}"));
            let mut err = MemError {
                kind: "FatalSignal".into(),
                what: format!("Process terminated with {name}"),
                tid: e.child_text("tid").and_then(|t| t.parse().ok()),
                count: 1,
                pid,
                ..MemError::default()
            };
            if let Some(st) = e.child("stack") {
                err.stacks.push(stack(st, err.what.clone(), in_project));
            }
            finish(&mut err);
            out.errors.push(err);
            continue;
        }
        if e.name != "error" {
            continue;
        }
        let what = e
            .child_text("what")
            .map(str::to_string)
            .or_else(|| e.child("xwhat").and_then(|x| x.child_text("text")).map(str::to_string))
            .unwrap_or_default();
        let mut err = MemError {
            kind: e.child_text("kind").unwrap_or("").to_string(),
            what: what.clone(),
            tid: e.child_text("tid").and_then(|t| t.parse().ok()),
            leaked_bytes: e.child("xwhat").and_then(|x| x.child_text("leakedbytes")).and_then(|b| b.parse().ok()),
            leaked_blocks: e.child("xwhat").and_then(|x| x.child_text("leakedblocks")).and_then(|b| b.parse().ok()),
            count: e.child_text("unique").and_then(|u| counts.get(u)).copied().unwrap_or(1),
            pid,
            ..MemError::default()
        };
        // Stacks follow their headings: the error's own, then each `auxwhat` / `xauxwhat`.
        let mut title = what;
        for c in &e.children {
            match c.name.as_str() {
                "auxwhat" => title = c.text.trim().to_string(),
                "xauxwhat" => title = c.child_text("text").unwrap_or("").to_string(),
                "origin" => {
                    if let Some(st) = c.child("stack") {
                        let t = c.child_text("what").unwrap_or("Uninitialised value was created").to_string();
                        err.stacks.push(stack(st, t, in_project));
                    }
                }
                "stack" => err.stacks.push(stack(c, std::mem::take(&mut title), in_project)),
                _ => {}
            }
        }
        finish(&mut err);
        out.errors.push(err);
    }
    out
}

fn finish(e: &mut MemError) {
    let frames = || e.stacks.iter().flat_map(|s| &s.frames);
    e.location = frames()
        .find(|f| f.project)
        .or_else(|| frames().find(|f| f.path.is_some() && f.line.is_some()))
        .and_then(|f| Some((f.path.clone()?, f.line?)));
}

/// Merge the reports of several processes (`--trace-children`).
pub fn merge(results: Vec<MemcheckResult>, source: String) -> MemcheckResult {
    let mut out = MemcheckResult { source, finished: !results.is_empty(), ..MemcheckResult::default() };
    for r in results {
        out.finished &= r.finished;
        out.commands.extend(r.commands);
        if out.fatal_signal.is_none() {
            out.fatal_signal = r.fatal_signal;
        }
        for e in r.errors {
            if out.errors.len() < MAX_ERRORS {
                out.errors.push(e);
            }
        }
    }
    out
}

// ---------------------------------------------------------------- running

/// The default Memcheck options (CLion's defaults).
pub fn default_options() -> Vec<String> {
    ["--tool=memcheck", "--leak-check=full", "--leak-resolution=med", "--track-origins=yes", "--vgdb=no"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// The host triple for `CARGO_TARGET_<TRIPLE>_RUNNER`.
fn host_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "macos" => format!("{arch}-apple-darwin"),
        "freebsd" => format!("{arch}-unknown-freebsd"),
        os => format!("{arch}-unknown-{os}"),
    }
}

/// Wrap `config` to run under Valgrind writing XML to `<prefix>.%p.xml`.
pub fn wrap(config: &RunConfig, valgrind: &Path, options: &[String], prefix: &Path) -> Result<ProcessCommand, String> {
    let mut opts: Vec<String> = options.to_vec();
    opts.push("--xml=yes".into());
    opts.push(format!("--xml-file={}.%p.xml", prefix.display()));
    let vg = valgrind.display().to_string();
    let Some(cmd) = &config.command else {
        // Cargo: Valgrind becomes the target runner.
        let inv = &config.invocation;
        let mut c = ProcessCommand::new("cargo", &[], inv.cwd.clone());
        c.args = codecraft_cargo::CargoInvocation { json_messages: false, ..inv.clone() }.args();
        c.env = inv.env.clone();
        if opts.iter().any(|o| o.contains(char::is_whitespace)) || vg.contains(char::is_whitespace) {
            return Err("paths with spaces cannot be used in a Cargo runner".into());
        }
        let var = format!("CARGO_TARGET_{}_RUNNER", host_triple().to_ascii_uppercase().replace('-', "_"));
        c.env.push((var, std::iter::once(vg).chain(opts).collect::<Vec<_>>().join(" ")));
        return Ok(c);
    };
    let program = Path::new(&cmd.program).file_name().and_then(|n| n.to_str()).unwrap_or(&cmd.program).to_string();
    let shell = matches!(program.as_str(), "sh" | "bash" | "zsh");
    let mut out = cmd.clone();
    let wrapper = shell
        && cmd.args.first().is_some_and(|a| a == "-c")
        && cmd.args.get(2).is_some_and(|a| a == crate::testing::cpp::WRAPPER_ARG0 || a == "codecraft-run")
        && cmd.args.get(1).is_some_and(|s| s.trim_end().ends_with("exec \"$@\""));
    if wrapper {
        let script = cmd.args.get(1).cloned().unwrap_or_default();
        let base = script.trim_end().strip_suffix("exec \"$@\"").unwrap_or(&script).to_string();
        let quoted: Vec<String> =
            std::iter::once(vg).chain(opts).map(|a| codecraft_cargo::shell_quote(&a)).collect();
        if let Some(s) = out.args.get_mut(1) {
            *s = format!("{base}exec {} \"$@\"", quoted.join(" "));
        }
        return Ok(out);
    }
    out.program = vg;
    let mut args = opts;
    if shell {
        args.push("--trace-children=yes".into());
        args.push(format!("--trace-children-skip={SKIP_CHILDREN}"));
    }
    args.push(cmd.program.clone());
    args.extend(cmd.args.iter().cloned());
    out.args = args;
    Ok(out)
}

/// A run waiting for its XML.
struct PendingRun {
    tab: RunTabId,
    prefix: PathBuf,
}

#[derive(Default)]
pub struct ValgrindState {
    pub results: BTreeMap<u64, MemcheckResult>,
    /// Options before `--xml=yes` (default: [`default_options`]).
    pub options: Option<Vec<String>>,
    /// Import Valgrind XML Results without a path: the shell asks for one.
    pub import_requested: bool,
    /// Which view a tab shows: Memcheck (true) or the console.
    pub show_results: BTreeMap<u64, bool>,
    pending: Vec<PendingRun>,
}

impl ValgrindState {
    pub fn result(&self, tab: RunTabId) -> Option<&MemcheckResult> {
        self.results.get(&tab.0)
    }

    pub fn running(&self, tab: RunTabId) -> bool {
        self.pending.iter().any(|p| p.tab == tab)
    }
}

/// Read every `<prefix>.<pid>.xml` (removing them).
fn collect_reports(prefix: &Path, in_project: &dyn Fn(&Path) -> bool) -> Vec<MemcheckResult> {
    let Some(dir) = prefix.parent() else { return Vec::new() };
    let Some(stem) = prefix.file_name().and_then(|n| n.to_str()).map(|n| format!("{n}.")) else { return Vec::new() };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&stem) && n.ends_with(".xml")))
                .take(256)
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let ok = std::fs::metadata(&f).is_ok_and(|m| m.len() <= MAX_XML_BYTES);
        if ok && let Ok(bytes) = std::fs::read(&f) {
            out.push(parse_xml(&String::from_utf8_lossy(&bytes), in_project));
        }
        let _ = std::fs::remove_file(&f);
    }
    out
}

fn project_filter(s: &Session) -> impl Fn(&Path) -> bool + use<> {
    let root = s.project_root();
    move |p: &Path| root.as_ref().is_some_and(|r| p.starts_with(r)) && p.is_file()
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut done = Vec::new();
    for (i, p) in s.cpp.valgrind.pending.iter().enumerate() {
        let running = s.ws.run(p.tab).is_some_and(|t| t.status == RunStatus::Running);
        if !running {
            done.push(i);
        }
    }
    if done.is_empty() {
        return false;
    }
    let in_project = project_filter(s);
    for i in done.into_iter().rev() {
        let p = s.cpp.valgrind.pending.remove(i);
        let reports = collect_reports(&p.prefix, &in_project);
        let title = s.ws.run(p.tab).map(|t| t.title.clone()).unwrap_or_default();
        if reports.is_empty() {
            if let Some(t) = s.ws.run_mut(p.tab) {
                t.push(ConsoleKind::System, "Valgrind wrote no XML report".to_string());
            }
            continue;
        }
        let result = merge(reports, title.clone());
        let n = result.errors.len();
        s.cpp.valgrind.results.insert(p.tab.0, result);
        s.cpp.valgrind.show_results.insert(p.tab.0, n > 0);
        let level = if n > 0 { NotificationLevel::Warning } else { NotificationLevel::Info };
        s.notify(level, "Valgrind Memcheck", format!("{title}: {n} error(s)"));
    }
    true
}

fn run_with_valgrind(s: &mut Session, p: &Value) -> CmdResult {
    let config = match opt_usize(p, "config")? {
        Some(i) => s.run_configs.get(i).cloned().ok_or_else(|| CommandError::BadParams(format!("no run configuration {i}")))?,
        None => s
            .run_configs
            .get(s.selected_run_config)
            .cloned()
            .ok_or_else(|| CommandError::Disabled("no run configuration".into()))?,
    };
    if !config.target.is_local() {
        return Err(CommandError::Failed("Valgrind runs local configurations only".into()));
    }
    let valgrind = match s.cpp.tools.get("valgrind") {
        Some(v) => v.clone(),
        None if cfg!(any(target_os = "macos", windows)) => {
            s.notify(NotificationLevel::Warning, "Valgrind Memcheck", UNAVAILABLE);
            return Err(CommandError::Failed(UNAVAILABLE.into()));
        }
        None => super::tool(s, "valgrind").map_err(CommandError::Failed)?,
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let prefix = std::env::temp_dir().join(format!("codecraft-valgrind-{}-{seq}", std::process::id()));
    let options = s.cpp.valgrind.options.clone().unwrap_or_else(default_options);
    let cmd = wrap(&config, &valgrind, &options, &prefix).map_err(CommandError::Failed)?;
    let title = format!("Valgrind: {}", config.name);
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    s.cpp.valgrind.pending.push(PendingRun { tab, prefix });
    Ok(json!({ "tab": tab.0, "commandLine": cmd.command_line() }))
}

/// Show a result in a new (finished) Run tab.
pub(crate) fn show_result(s: &mut Session, result: MemcheckResult, title: &str, note: &str) -> RunTabId {
    let tab = s.ws.add_run(title, note, false);
    if let Some(t) = s.ws.run_mut(tab) {
        t.push(ConsoleKind::System, note.to_string());
        for c in &result.commands {
            t.push(ConsoleKind::System, format!("Checked: {c}"));
        }
        t.status = RunStatus::Succeeded;
        t.duration = Some(std::time::Duration::ZERO);
    }
    s.cpp.valgrind.show_results.insert(tab.0, true);
    s.cpp.valgrind.results.insert(tab.0, result);
    s.ws.layout.show(ToolWindowId::Run);
    tab
}

fn import(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        s.cpp.valgrind.import_requested = true;
        return ok();
    };
    s.cpp.valgrind.import_requested = false;
    let path = super::resolve_path(s, &path);
    let meta = std::fs::metadata(&path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
    if meta.len() > MAX_XML_BYTES {
        return Err(CommandError::Failed(format!("{} is too large", path.display())));
    }
    let bytes = std::fs::read(&path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
    let text = String::from_utf8_lossy(&bytes);
    if !text.contains("<valgrindoutput") {
        return Err(CommandError::Failed(format!("{} is not a Valgrind XML report", path.display())));
    }
    let in_project = project_filter(s);
    let mut result = parse_xml(&text, &in_project);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    result.source = name.clone();
    let n = result.errors.len();
    let tab = show_result(s, result, &format!("Valgrind: {name}"), &format!("Imported {}", path.display()));
    Ok(json!({ "tab": tab.0, "errors": n }))
}

fn tab_param(s: &Session, p: &Value) -> Result<RunTabId, CommandError> {
    if let Some(t) = p.get("tab").and_then(Value::as_u64) {
        return Ok(RunTabId(t));
    }
    s.ws.active_run.ok_or_else(|| CommandError::Disabled("no run".into()))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "run.runWithValgrindMemcheck",
            "Run with Valgrind Memcheck",
            None,
            "{config?: int} — run the selected (or given) configuration under Valgrind Memcheck; the errors \
             appear in the run's Memcheck view (Linux; Valgrind does not exist on macOS)",
            Context::Global,
            |s| if s.run_configs.is_empty() { Err("no run configuration".into()) } else { Ok(()) },
            run_with_valgrind,
        ),
        spec(
            "run.importValgrindXmlResults",
            "Import Valgrind XML Results",
            None,
            "{path?: string} — read a Valgrind `--xml=yes` report into a Memcheck view (asks for the file \
             without `path`)",
            Context::Global,
            always,
            import,
        ),
        spec(
            "valgrind.results",
            "Valgrind Memcheck Results",
            None,
            "{tab?: int} — the Memcheck errors of a run (the active Run tab by default)",
            Context::Global,
            always,
            |s, p| {
                let tab = tab_param(s, p)?;
                let r = s.cpp.valgrind.result(tab);
                Ok(json!({ "tab": tab.0, "running": s.cpp.valgrind.running(tab), "result": r,
                           "groups": r.map(MemcheckResult::by_kind) }))
            },
        ),
        spec(
            "valgrind.navigate",
            "Go to Memcheck Frame",
            None,
            "{tab?: int, error: int, stack?: int, frame?: int} — open an error's frame (its most likely location \
             without stack/frame)",
            Context::Global,
            always,
            |s, p| {
                let tab = tab_param(s, p)?;
                let ei = req_usize(p, "error")?;
                let e = s
                    .cpp
                    .valgrind
                    .result(tab)
                    .and_then(|r| r.errors.get(ei))
                    .ok_or_else(|| CommandError::BadParams(format!("no error {ei}")))?;
                let loc = match (opt_usize(p, "stack")?, opt_usize(p, "frame")?) {
                    (Some(si), Some(fi)) => e
                        .stacks
                        .get(si)
                        .and_then(|st| st.frames.get(fi))
                        .and_then(|f| Some((f.path.clone()?, f.line?))),
                    _ => e.location.clone(),
                };
                let (path, line) = loc.ok_or_else(|| CommandError::Failed("this frame has no source location".into()))?;
                if !path.is_file() {
                    return Err(CommandError::Failed(format!("{} does not exist", path.display())));
                }
                s.navigate_to_position(&path, line, 0)?;
                ok()
            },
        ),
        spec(
            "valgrind.setOptions",
            "Valgrind Options",
            None,
            "{options: string} — Memcheck options (default `--tool=memcheck --leak-check=full \
             --leak-resolution=med --track-origins=yes --vgdb=no`; empty restores it)",
            Context::Global,
            always,
            |s, p| {
                let text = crate::params::req_str(p, "options")?;
                let words = crate::run_config_store::split_words(text).map_err(CommandError::BadParams)?;
                if words.iter().any(|w| w.starts_with("--xml")) {
                    return Err(CommandError::BadParams("--xml options are set by CodeCraft".into()));
                }
                s.cpp.valgrind.options = (!words.is_empty()).then_some(words);
                Ok(json!({ "options": s.cpp.valgrind.options.clone().unwrap_or_else(default_options) }))
            },
        ),
        spec(
            "valgrind.showResults",
            "Show Memcheck Results",
            None,
            "{tab?: int, show?: bool} — show the run's Memcheck errors (or its console)",
            Context::Global,
            always,
            |s, p| {
                let tab = tab_param(s, p)?;
                let cur = s.cpp.valgrind.show_results.get(&tab.0).copied().unwrap_or(false);
                let show = p.get("show").and_then(Value::as_bool).unwrap_or(!cur);
                s.cpp.valgrind.show_results.insert(tab.0, show);
                ok()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = include_str!("fixtures/memcheck.xml");

    fn in_project(p: &Path) -> bool {
        p.starts_with("/proj")
    }

    #[test]
    fn memcheck_report() {
        let r = parse_xml(XML, &in_project);
        assert!(r.finished);
        assert_eq!(r.commands, vec!["./app --flag"]);
        let kinds: Vec<&str> = r.errors.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["InvalidWrite", "UninitCondition", "Leak_DefinitelyLost"]);
        let w = &r.errors[0];
        assert_eq!(w.what, "Invalid write of size 4");
        assert_eq!(w.count, 2);
        assert_eq!(w.stacks.len(), 2);
        assert_eq!(w.stacks[1].title, "Address 0x4a4a050 is 0 bytes after a block of size 16 alloc'd");
        assert_eq!(w.stacks[0].frames[0].function.as_deref(), Some("main"));
        assert_eq!(w.location, Some((PathBuf::from("/proj/main.c"), 5)));
        assert!(!w.stacks[1].frames[0].project);
        let u = &r.errors[1];
        assert_eq!(u.stacks.len(), 2);
        assert_eq!(u.stacks[1].title, "Uninitialised value was created by a stack allocation");
        let l = &r.errors[2];
        assert_eq!((l.leaked_bytes, l.leaked_blocks), (Some(16), Some(1)));
        assert!(l.what.contains("definitely lost"));
        let groups = r.by_kind();
        assert_eq!(groups[0].0, "Invalid write");
        assert_eq!(groups[2], ("Definitely lost".to_string(), vec![2]));
    }

    #[test]
    fn truncated_and_hostile_reports() {
        let cut = parse_xml(&XML[..XML.find("<state>FINISHED").unwrap()], &in_project);
        assert!(!cut.finished);
        assert!(!cut.errors.is_empty());
        assert!(parse_xml("", &in_project).errors.is_empty());
        assert!(parse_xml("<valgrindoutput><error><stack><frame><line>x</line></frame></stack></error>", &in_project).errors[0].location.is_none());
        let fatal = parse_xml("<valgrindoutput><fatal_signal><tid>1</tid><signame>SIGSEGV</signame><stack><frame><fn>f</fn><dir>/proj</dir><file>a.c</file><line>3</line></frame></stack></fatal_signal></valgrindoutput>", &in_project);
        assert_eq!(fatal.fatal_signal.as_deref(), Some("Process terminated with SIGSEGV"));
        assert_eq!(fatal.errors[0].location, Some((PathBuf::from("/proj/a.c"), 2)));
        let merged = merge(vec![parse_xml(XML, &in_project), cut], "x".into());
        assert!(!merged.finished);
        assert_eq!(merged.commands.len(), 2);
    }

    #[test]
    fn wrapping_configurations() {
        let vg = Path::new("/usr/bin/valgrind");
        let prefix = Path::new("/tmp/vg-1");
        let opts = default_options();
        // A plain program.
        let direct = RunConfig::process("app", ProcessCommand::new("/b/app", &["--x"], "/b"));
        let c = wrap(&direct, vg, &opts, prefix).unwrap();
        assert_eq!(c.program, "/usr/bin/valgrind");
        assert!(c.args.contains(&"--xml-file=/tmp/vg-1.%p.xml".to_string()));
        assert_eq!(&c.args[c.args.len() - 2..], ["/b/app", "--x"]);
        // The build-then-run wrapper: Valgrind goes in front of `exec`.
        let mut w = ProcessCommand::shell("'cmake' --build b --target app && exec \"$@\"", "/b");
        w.args.extend(["codecraft-run".to_string(), "/b/app".into()]);
        let c = wrap(&RunConfig::process("app", w), vg, &opts, prefix).unwrap();
        assert_eq!(c.program, "sh");
        assert!(c.args[1].starts_with("'cmake' --build b --target app && exec /usr/bin/valgrind --tool=memcheck"), "{}", c.args[1]);
        assert!(c.args[1].ends_with("--xml-file=/tmp/vg-1.%p.xml \"$@\""), "{}", c.args[1]);
        assert_eq!(&c.args[2..], ["codecraft-run", "/b/app"]);
        // Other scripts are traced, build tools skipped.
        let script = RunConfig::process("Run x", ProcessCommand::shell("cmake --build b && ./b/x", "/p"));
        let c = wrap(&script, vg, &opts, prefix).unwrap();
        assert!(c.args.contains(&"--trace-children=yes".to_string()));
        assert_eq!(&c.args[c.args.len() - 3..], ["sh", "-c", "cmake --build b && ./b/x"]);
        // Cargo: the target runner.
        let cargo = RunConfig::cargo("Run app", codecraft_cargo::CargoInvocation::new(codecraft_cargo::CargoCommand::Run, "/w"));
        let c = wrap(&cargo, vg, &opts, prefix).unwrap();
        assert_eq!(c.program, "cargo");
        let (k, v) = c.env.last().unwrap();
        assert!(k.starts_with("CARGO_TARGET_") && k.ends_with("_RUNNER"), "{k}");
        assert!(v.starts_with("/usr/bin/valgrind --tool=memcheck"));
        assert!(wrap(&cargo, Path::new("/a b/valgrind"), &opts, prefix).is_err());
    }
}
