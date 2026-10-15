//! The M14 analyzers inside an Inspect Code run (worker side): rust-analyzer, clangd, the Clang
//! Static Analyzer, cppcheck and the SQL inspections; generating `compile_commands.json` for
//! CMake projects first; and what happens to every finding afterwards (the inspection profile,
//! `NOLINT` comments, one report when two analyzers find the same problem).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;

use codecraft_cargo::RunEvent;

use super::cpp;
use crate::Session;
use crate::inspections::{self, CANCELLED, Finding, JobSpec, ScopeKind, Tool, ToolRun, find_program, wanted};

/// Output kept per stream of a captured process.
const MAX_CAPTURE_BYTES: usize = 16 << 20;

/// Programs and directories the M14 analyzers need, resolved on the UI thread.
#[derive(Clone, Debug, Default)]
pub struct AnalyzerEnv {
    pub clang: Option<PathBuf>,
    pub clangd: Option<PathBuf>,
    /// `cmake`, its configure arguments and the build directory of the active CMake profile
    /// (projects with a `CMakeLists.txt`).
    pub cmake: Option<(PathBuf, Vec<String>, PathBuf)>,
    /// `$CODECRAFT_HOME/tools/bin` (Install basedpyright puts it there).
    pub managed_bin: Option<PathBuf>,
    /// Isolated sessions (tests): no analyzer is looked up on `PATH` unless overridden.
    pub isolated: bool,
}

impl AnalyzerEnv {
    /// A managed tool when installed.
    pub fn managed_tool(&self, name: &str) -> Option<PathBuf> {
        let p = self.managed_bin.as_ref()?.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        p.is_file().then_some(p)
    }
}

/// The environment of a run.
pub(crate) fn env(s: &Session, root: &Path) -> AnalyzerEnv {
    let isolated = s.cpp.isolated;
    let look = |name: &str| if isolated { None } else { codecraft_lsp::which(name) };
    let cmake = if root.join("CMakeLists.txt").is_file() {
        crate::cpp::tool(s, "cmake").ok().and_then(|program| {
            let profile = crate::cpp::cmake::active_profile(root)?;
            let args = crate::cpp::cmake::configure_args(root, &profile);
            Some((program, args, profile.build_dir))
        })
    } else {
        None
    };
    AnalyzerEnv {
        clang: look("clang"),
        clangd: look("clangd"),
        cmake,
        managed_bin: s.analysis.profiles.home.file("tools").map(|t| t.join("bin")),
        isolated,
    }
}

/// The directory with the project's `compile_commands.json`: the project root, `build`,
/// `cmake-build-debug`, `out` or the active CMake profile's build directory.
pub(crate) fn compile_db_dir(spec: &JobSpec) -> Option<PathBuf> {
    let root = &spec.root;
    let mut dirs: Vec<PathBuf> = ["", "build", "cmake-build-debug", "out"]
        .iter()
        .map(|d| if d.is_empty() { root.clone() } else { root.join(d) })
        .collect();
    if let Some((_, _, build)) = &spec.env.cmake {
        dirs.insert(0, build.clone());
    }
    dirs.into_iter().find(|d| d.join("compile_commands.json").is_file())
}

fn c_wanted(spec: &JobSpec) -> bool {
    [Tool::Clangd, Tool::ClangAnalyzer, Tool::ClangTidy, Tool::Cppcheck].into_iter().any(|t| wanted(spec, t))
}

/// CMake projects without a compilation database: configure the active profile once
/// (`-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`) so the C/C++ analyzers see the real flags.
pub(crate) fn prepare_cpp(spec: &JobSpec, cancel: &AtomicBool, progress: &dyn Fn(&str)) {
    if !cpp::has_c_files(&spec.scope.files) || !c_wanted(spec) || compile_db_dir(spec).is_some() {
        return;
    }
    let Some((cmake, args, build)) = &spec.env.cmake else { return };
    progress("Generating compile_commands.json (cmake)…");
    match run_capture(cmake, args, &spec.root, &[], cancel) {
        Ok(out) if !build.join("compile_commands.json").is_file() => {
            log::warn!("cmake did not write compile_commands.json: {}", out.summary());
            progress("cmake did not write compile_commands.json; analyzing with default flags");
        }
        Ok(_) => {}
        Err(e) => log::warn!("cmake: {e}"),
    }
}

// ---------------------------------------------------------------- processes

/// A finished process with all its output (up to [`MAX_CAPTURE_BYTES`] per stream).
#[derive(Debug, Default)]
pub struct Capture {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub success: bool,
    pub code: Option<i32>,
}

impl Capture {
    /// The last stderr lines, else the exit code.
    pub fn summary(&self) -> String {
        let lines: Vec<&str> = self.stderr.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let tail = lines.iter().rev().take(3).rev().copied().collect::<Vec<_>>().join(" | ");
        if tail.is_empty() { format!("exit code {}", self.code.map_or("?".into(), |c| c.to_string())) } else { tail }
    }
}

/// Run a program to completion keeping stdout and stderr; killed when `cancel` is set.
pub(crate) fn run_capture(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    cancel: &AtomicBool,
) -> Result<Capture, String> {
    let (handle, rx) =
        codecraft_cargo::spawn_process(program, args, cwd, env, false).map_err(|e| format!("{}: {e}", program.display()))?;
    let mut out = Capture::default();
    let (mut out_bytes, mut err_bytes) = (0usize, 0usize);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = handle.kill();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(RunEvent::Exited { .. }) | Err(RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
            }
            return Err(CANCELLED.into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(RunEvent::Stdout(line)) => {
                out_bytes = out_bytes.saturating_add(line.len());
                if out_bytes <= MAX_CAPTURE_BYTES {
                    out.stdout.push(line);
                }
            }
            Ok(RunEvent::Stderr(line)) => {
                err_bytes = err_bytes.saturating_add(line.len());
                if err_bytes <= MAX_CAPTURE_BYTES {
                    out.stderr.push(crate::runs::strip_ansi(&line));
                }
            }
            Ok(RunEvent::Error(e)) => out.stderr.push(e),
            Ok(RunEvent::Exited { code, success, .. }) => {
                out.code = code;
                out.success = success;
                return Ok(out);
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(out),
        }
    }
}

// ---------------------------------------------------------------- running them

fn missing(spec: &JobSpec, tool: Tool, tools: &mut Vec<ToolRun>) {
    // Only an explicit request for a missing tool is worth reporting.
    if spec.tools.as_ref().is_some_and(|t| t.contains(&tool)) {
        let hint = match tool {
            Tool::RustAnalyzer => "not installed (rustup component add rust-analyzer)",
            Tool::Cppcheck => "not installed (brew install cppcheck / apt install cppcheck)",
            Tool::ClangAnalyzer | Tool::Clangd => "not installed (Xcode command line tools or LLVM)",
            _ => "not installed",
        };
        tools.push(ToolRun { tool, command_line: String::new(), outcome: hint.into(), ok: false });
    }
}

/// The program of an M14 analyzer: an override, else (outside isolated sessions) what is
/// installed.
fn program(spec: &JobSpec, tool: Tool) -> Option<PathBuf> {
    if let Some(p) = spec.programs.get(&tool) {
        return Some(p.clone());
    }
    if spec.env.isolated {
        return None;
    }
    find_program(spec, tool)
}

fn record(
    result: Result<(ToolRun, Vec<Finding>), String>,
    tool: Tool,
    tools: &mut Vec<ToolRun>,
    findings: &mut Vec<Finding>,
) -> bool {
    match result {
        Ok((run, found)) => {
            tools.push(run);
            findings.extend(found);
            false
        }
        Err(e) if e == CANCELLED => true,
        Err(e) => {
            tools.push(ToolRun { tool, command_line: String::new(), outcome: format!("failed: {e}"), ok: false });
            false
        }
    }
}

/// Run every applicable M14 analyzer; `true` when cancelled.
pub(crate) fn run(
    spec: &JobSpec,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
    tools: &mut Vec<ToolRun>,
    findings: &mut Vec<Finding>,
) -> bool {
    let files = &spec.scope.files;
    let project_wide = matches!(spec.scope.scope.kind, ScopeKind::Project);
    let has_rust = files.iter().any(|f| f.extension().is_some_and(|e| e == "rs"));
    let stop = || cancel.load(Ordering::Relaxed);

    // rust-analyzer: the whole Cargo workspace (results are filtered to the scope).
    if wanted(spec, Tool::RustAnalyzer)
        && let Some(cargo_root) = &spec.cargo_root
        && (has_rust || project_wide)
    {
        match program(spec, Tool::RustAnalyzer) {
            Some(p) => {
                if record(super::rust_analyzer::run(&p, cargo_root, cancel, progress), Tool::RustAnalyzer, tools, findings) {
                    return true;
                }
            }
            None => missing(spec, Tool::RustAnalyzer, tools),
        }
    }
    if stop() {
        return true;
    }
    if cpp::has_c_files(files) {
        let db = compile_db_dir(spec);
        if wanted(spec, Tool::Clangd) {
            match program(spec, Tool::Clangd) {
                Some(p) => {
                    if record(cpp::run_clangd(&p, &spec.root, db.as_deref(), files, cancel, progress), Tool::Clangd, tools, findings) {
                        return true;
                    }
                }
                None => missing(spec, Tool::Clangd, tools),
            }
        }
        if stop() {
            return true;
        }
        if wanted(spec, Tool::ClangAnalyzer) && files.iter().any(|f| cpp::is_tu(f)) {
            match program(spec, Tool::ClangAnalyzer) {
                Some(p) => {
                    let extra = spec.profile.clang_analyzer_args();
                    let r = cpp::run_clang_analyzer(&p, &spec.root, db.as_deref(), files, &extra, cancel, progress);
                    if record(r, Tool::ClangAnalyzer, tools, findings) {
                        return true;
                    }
                }
                None => missing(spec, Tool::ClangAnalyzer, tools),
            }
        }
        if stop() {
            return true;
        }
        if wanted(spec, Tool::Cppcheck) && files.iter().any(|f| cpp::is_tu(f)) {
            match program(spec, Tool::Cppcheck) {
                Some(p) => {
                    let enable = spec.profile.cppcheck_enable();
                    let r = cpp::run_cppcheck(&p, &spec.root, db.as_deref(), files, project_wide, &enable, cancel, progress);
                    if record(r, Tool::Cppcheck, tools, findings) {
                        return true;
                    }
                }
                None => missing(spec, Tool::Cppcheck, tools),
            }
        }
    }
    if stop() {
        return true;
    }
    if wanted(spec, Tool::Sql) && super::sql::has_sql(files) {
        let (run, found, cancelled) = super::sql::run(files, &spec.root, cancel, progress);
        if cancelled {
            return true;
        }
        tools.push(run);
        findings.extend(found);
    }
    stop()
}

// ---------------------------------------------------------------- after the run

/// Which analyzer's report wins when two find the same problem (lower first).
fn priority(t: Tool) -> u8 {
    match t {
        Tool::Clippy | Tool::CargoCheck => 0,
        Tool::RustAnalyzer => 1,
        Tool::ClangTidy => 0,
        Tool::Clangd => 1,
        Tool::ClangAnalyzer => 2,
        Tool::Cppcheck => 3,
        _ => 0,
    }
}

/// The profile's switches and severities, `NOLINT` comments for C/C++ analyzers, and one
/// report per problem across analyzers of the same language (same file, line and rule, or
/// message).
pub(crate) fn postprocess(spec: &JobSpec, findings: Vec<Finding>) -> Vec<Finding> {
    let explicit = |t: Tool| spec.tools.as_ref().is_some_and(|l| l.contains(&t) || (t == Tool::Clippy && l.contains(&Tool::CargoCheck)));
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut kept: Vec<Finding> = Vec::with_capacity(findings.len());
    for mut f in findings {
        let ex = explicit(f.tool);
        if !spec.profile.apply(&mut f, ex) {
            continue;
        }
        if matches!(f.tool, Tool::ClangAnalyzer | Tool::Clangd) {
            let text = texts.entry(f.path.clone()).or_insert_with(|| inspections::read_normalized(&f.path));
            if text.as_deref().is_some_and(|t| cpp::honours_nolint(&f, t)) {
                continue;
            }
        }
        kept.push(f);
    }
    dedupe_across(kept)
}

fn normalized_message(m: &str) -> String {
    m.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Drop a finding another analyzer of the same language already reports (by rule or message
/// on the same line), keeping the higher-priority analyzer's (it usually has the fix).
pub(crate) fn dedupe_across(mut findings: Vec<Finding>) -> Vec<Finding> {
    let mut order: Vec<usize> = (0..findings.len()).collect();
    order.sort_by_key(|&i| findings.get(i).map_or(u8::MAX, |f| priority(f.tool)));
    let mut seen: HashSet<(&'static str, PathBuf, usize, String)> = HashSet::new();
    let mut owner: HashMap<(&'static str, PathBuf, usize, String), Tool> = HashMap::new();
    let mut drop = vec![false; findings.len()];
    for i in order {
        let Some(f) = findings.get(i) else { continue };
        let lang = f.tool.language();
        let keys = [
            (lang, f.path.clone(), f.start.line, format!("rule:{}", f.rule())),
            (lang, f.path.clone(), f.start.line, format!("msg:{}", normalized_message(&f.message))),
        ];
        let dup = keys.iter().any(|k| owner.get(k).is_some_and(|t| *t != f.tool));
        if dup {
            if let Some(d) = drop.get_mut(i) {
                *d = true;
            }
            continue;
        }
        for k in keys {
            if seen.insert(k.clone()) {
                owner.insert(k, f.tool);
            }
        }
    }
    let mut i = 0usize;
    findings.retain(|_| {
        let keep = !drop.get(i).copied().unwrap_or(false);
        i += 1;
        keep
    });
    findings
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use codecraft_text::Position;
    use codecraft_workspace::Severity;

    fn f(tool: Tool, rule: &str, line: usize, msg: &str) -> Finding {
        let mut f = Finding::new(tool, rule, Severity::Warning, "/w/a.rs".into(), Position::new(line, 0));
        f.code = Some(rule.into());
        f.message = msg.into();
        f
    }

    #[test]
    fn duplicates_across_analyzers_keep_the_better_report() {
        let out = dedupe_across(vec![
            f(Tool::RustAnalyzer, "E0308", 3, "expected i32, found &str"),
            f(Tool::Clippy, "E0308", 3, "mismatched types"),
            f(Tool::RustAnalyzer, "unresolved-module", 0, "unresolved module"),
            f(Tool::Clippy, "clippy::x", 5, "a"),
            f(Tool::Clippy, "clippy::x", 5, "a"),
            f(Tool::Clangd, "-Wunused-variable", 2, "unused variable 'x'"),
            f(Tool::ClangAnalyzer, "-Wunused-variable", 2, "unused variable 'x'"),
            f(Tool::ClangAnalyzer, "deadcode.DeadStores", 2, "Value stored to 'x' is never read"),
        ]);
        let got: Vec<(Tool, &str)> = out.iter().map(|f| (f.tool, f.rule())).collect();
        assert_eq!(
            got,
            [
                (Tool::Clippy, "E0308"),
                (Tool::RustAnalyzer, "unresolved-module"),
                (Tool::Clippy, "clippy::x"),
                (Tool::Clippy, "clippy::x"),
                (Tool::Clangd, "-Wunused-variable"),
                (Tool::ClangAnalyzer, "deadcode.DeadStores"),
            ],
            "same-tool duplicates are finalize's job"
        );
    }

    #[test]
    fn captured_processes_keep_all_output() {
        #[cfg(unix)]
        {
            let cancel = AtomicBool::new(false);
            let script = "i=0; while [ $i -lt 500 ]; do echo err$i 1>&2; i=$((i+1)); done; echo out; exit 3";
            let out = run_capture(Path::new("/bin/sh"), &["-c".into(), script.into()], Path::new("/"), &[], &cancel).unwrap();
            assert_eq!(out.stderr.len(), 500);
            assert_eq!(out.stdout, ["out"]);
            assert_eq!((out.success, out.code), (false, Some(3)));
            assert_eq!(out.summary(), "err497 | err498 | err499");
            assert!(run_capture(Path::new("/nonexistent/x"), &[], Path::new("/"), &[], &cancel).is_err());
        }
    }
}
