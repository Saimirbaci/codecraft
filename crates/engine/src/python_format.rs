//! Python formatting (M12 track PY): Code › Reformat Code with Ruff (`ruff format -`) or Black
//! (`black -q -`), Code › Optimize Imports with Ruff's isort rules (`ruff check --select I
//! --fix -`) or isort (`isort -`), and both on save (Settings › Languages & Frameworks › Python).
//!
//! Tools are looked up in the project's virtualenv, next to the selected interpreter, then on
//! `PATH`; as a last resort `<python> -m ruff|black|isort`. They run on a worker thread with the
//! document's text on stdin (the file path as `--stdin-filename`, so the project's
//! configuration applies, in the project root); the result is applied as one undoable edit of
//! the changed lines with carets kept ([`crate::reformat::diff_edit`], like rustfmt), unless the
//! document changed meanwhile. The routing ([`route`]) keeps the commands' behaviour for other
//! languages.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_workspace::{DocId, NotificationLevel};

use crate::commands::{CmdResult, CommandSpec, Context, Enabled, Run, always, spec};
use crate::python_output::LineKind;
use crate::{CommandError, Session};

/// Longest a formatter may run.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Largest output read from a formatter.
const MAX_OUTPUT: u64 = 64 << 20;

/// What to do to a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Format,
    Imports,
    /// Optimize imports, then format (on save).
    Both,
}

/// A tool invocation: the program, its arguments before the stdin marker, a label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub label: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// `python -m tool`: a missing module means "not installed" (try the next one).
    pub module: bool,
}

/// One tool run's result.
#[derive(Debug)]
struct Outcome {
    result: Result<String, String>,
    /// What ran (for the status and the log).
    label: String,
    log: Vec<(LineKind, String)>,
}

struct Job {
    doc: DocId,
    revision: u64,
    op: Op,
    save_after: bool,
    rx: Receiver<Outcome>,
}

/// Formatter jobs.
#[derive(Default)]
pub struct FormatState {
    jobs: Vec<Job>,
    /// Files just saved after a reformat on save (not reformatted again).
    skip_on_save: HashSet<PathBuf>,
}

impl std::fmt::Debug for FormatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormatState").field("jobs", &self.jobs.len()).finish()
    }
}

impl FormatState {
    pub fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// Labels of running jobs (Python Process Output).
    pub fn running(&self) -> Vec<String> {
        self.jobs
            .iter()
            .map(|j| match j.op {
                Op::Format => "Reformat Code".to_string(),
                Op::Imports => "Optimize Imports".to_string(),
                Op::Both => "Optimize Imports and Reformat".to_string(),
            })
            .collect()
    }
}

/// The configured formatter: `auto`, `ruff`, `black` or `none`.
pub fn formatter_setting(s: &Session) -> String {
    let f = s.ws.settings.python_formatter.trim().to_ascii_lowercase();
    match f.as_str() {
        "ruff" | "black" | "none" => f,
        _ => "auto".into(),
    }
}

/// Where a tool's executable is: the project's virtualenv, next to the interpreter, `PATH`.
fn find_tool(s: &Session, name: &str) -> Option<PathBuf> {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let next_to = |py: &Path| py.parent().map(|d| d.join(&exe)).filter(|p| p.is_file());
    s.project_root()
        .and_then(|r| crate::python_env::project_python(&r))
        .and_then(|py| next_to(&py))
        .or_else(|| {
            s.python.interpreter.as_deref().and_then(Path::parent).map(|d| d.join(&exe)).filter(|p| p.is_file())
        })
        .or_else(|| s.python.probe().which(name))
}

fn tool_args(tool: &str, op: Op, file: &str) -> Vec<String> {
    let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    match (tool, op) {
        ("ruff", Op::Imports) => {
            v(&["check", "--select", "I", "--fix", "--exit-zero", "--quiet", "--stdin-filename", file, "-"])
        }
        ("ruff", _) => v(&["format", "--quiet", "--stdin-filename", file, "-"]),
        ("black", _) => v(&["-q", "--stdin-filename", file, "-"]),
        ("isort", _) => v(&["--filename", file, "-"]),
        _ => Vec::new(),
    }
}

fn label(tool: &str) -> &'static str {
    match tool {
        "ruff" => "Ruff",
        "black" => "Black",
        "isort" => "isort",
        _ => "formatter",
    }
}

/// The tools to try for `op`, in order (the first that runs is used).
pub fn candidates(s: &Session, op: Op, file: &str) -> Vec<Invocation> {
    let tools: Vec<&str> = match op {
        Op::Imports => vec!["ruff", "isort"],
        _ => match formatter_setting(s).as_str() {
            "ruff" => vec!["ruff"],
            "black" => vec!["black"],
            "none" => Vec::new(),
            _ => vec!["ruff", "black"],
        },
    };
    let mut out = Vec::new();
    for t in &tools {
        if let Some(program) = find_tool(s, t) {
            out.push(Invocation { label: label(t), program, args: tool_args(t, op, file), module: false });
        }
    }
    let python = s.python.python_program();
    for t in &tools {
        let mut args = vec!["-m".to_string(), (*t).to_string()];
        args.extend(tool_args(t, op, file));
        out.push(Invocation { label: label(t), program: python.clone(), args, module: true });
    }
    out
}

/// Run a program with `input` on stdin; (exit code, stdout, stderr). Killed after `timeout`.
pub fn run_with_input(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    input: &str,
    timeout: Duration,
) -> std::io::Result<(Option<i32>, String, String)> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn()?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("cannot open the formatter's pipes"));
    };
    let input = input.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let read = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.by_ref().take(MAX_OUTPUT).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let start = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(st) => break Some(st),
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let _ = writer.join();
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status {
        Some(st) => Ok((st.code(), stdout, stderr)),
        None => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("timed out after {}s", timeout.as_secs()))),
    }
}

/// Try the candidates in order on `text`.
fn run_candidates(list: &[Invocation], text: &str, cwd: Option<&Path>, what: &str) -> Outcome {
    let mut log = Vec::new();
    for inv in list {
        log.push((LineKind::Command, format!("{} {}", inv.program.display(), inv.args.join(" "))));
        match run_with_input(&inv.program, &inv.args, cwd, text, TIMEOUT) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log.push((LineKind::Stderr, format!("not found: {e}")));
            }
            Err(e) => {
                log.push((LineKind::Stderr, e.to_string()));
                return Outcome { result: Err(format!("{} failed: {e}", inv.label)), label: inv.label.into(), log };
            }
            Ok((code, stdout, stderr)) => {
                if !stderr.trim().is_empty() {
                    log.push((LineKind::Stderr, stderr.trim_end().to_string()));
                }
                let missing_module = inv.module && (stderr.contains("No module named") || code == Some(127));
                if missing_module {
                    continue;
                }
                if code == Some(0) {
                    log.push((LineKind::Info, format!("{} finished", inv.label)));
                    return Outcome { result: Ok(stdout), label: inv.label.into(), log };
                }
                let msg = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                let msg = if msg.is_empty() { format!("exit code {}", code.unwrap_or(-1)) } else { msg };
                return Outcome { result: Err(format!("{}: {msg}", inv.label)), label: inv.label.into(), log };
            }
        }
    }
    let tools = if what == "imports" { "Ruff or isort" } else { "Ruff or Black" };
    Outcome {
        result: Err(format!("{tools} is not installed (pip install ruff); set the formatter in Settings › Languages & Frameworks › Python")),
        label: String::new(),
        log,
    }
}

/// Start formatting a Python document in the background.
pub(crate) fn start(s: &mut Session, doc: DocId, op: Op, save_after: bool) -> Result<(), String> {
    if s.py_format.jobs.iter().any(|j| j.doc == doc) {
        return Err("a formatter is already running for this file".into());
    }
    let od = s.ws.doc(doc).ok_or("no document")?;
    let text = od.doc.text();
    let revision = od.doc.revision();
    let file = od.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "untitled.py".into());
    let cwd = s.project_root();
    let format = if op == Op::Imports { Vec::new() } else { candidates(s, Op::Format, &file) };
    let imports = if op == Op::Format { Vec::new() } else { candidates(s, Op::Imports, &file) };
    if op == Op::Format && formatter_setting(s) == "none" {
        return Err("no Python formatter is configured (Settings › Languages & Frameworks › Python)".into());
    }
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let outcome = match op {
            Op::Format => run_candidates(&format, &text, cwd.as_deref(), "format"),
            Op::Imports => run_candidates(&imports, &text, cwd.as_deref(), "imports"),
            Op::Both => {
                let first = run_candidates(&imports, &text, cwd.as_deref(), "imports");
                let mut log = first.log;
                let sorted = first.result.unwrap_or(text);
                if format.is_empty() {
                    Outcome { result: Ok(sorted), label: first.label, log }
                } else {
                    let second = run_candidates(&format, &sorted, cwd.as_deref(), "format");
                    log.extend(second.log);
                    Outcome { result: second.result, label: second.label, log }
                }
            }
        };
        let _ = tx.send(outcome);
    });
    s.py_format.jobs.push(Job { doc, revision, op, save_after, rx });
    s.status = match op {
        Op::Format => "Reformatting…".into(),
        Op::Imports => "Optimizing imports…".into(),
        Op::Both => "Optimizing imports and reformatting…".into(),
    };
    Ok(())
}

/// Apply finished jobs.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut done = Vec::new();
    s.py_format.jobs.retain(|j| match j.rx.try_recv() {
        Ok(outcome) => {
            done.push((j.doc, j.revision, j.op, j.save_after, outcome));
            false
        }
        Err(crossbeam_channel::TryRecvError::Empty) => true,
        Err(crossbeam_channel::TryRecvError::Disconnected) => false,
    });
    let changed = !done.is_empty();
    for (doc, revision, op, save_after, outcome) in done {
        let source = if outcome.label.is_empty() { "Python formatter".to_string() } else { outcome.label.clone() };
        for (kind, text) in &outcome.log {
            s.python.process_output.push(&source, *kind, text);
        }
        apply(s, doc, revision, op, save_after, outcome);
    }
    changed
}

fn apply(s: &mut Session, doc: DocId, revision: u64, op: Op, save_after: bool, outcome: Outcome) {
    let what = match op {
        Op::Format => "Reformatted",
        Op::Imports => "Optimized imports",
        Op::Both => "Optimized imports and reformatted",
    };
    let formatted = match outcome.result {
        Ok(t) => t,
        Err(e) => {
            s.notify(NotificationLevel::Error, "Python formatter", e);
            return;
        }
    };
    let editor = s.ws.editor_for_doc(doc);
    let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
    let Some(od) = s.ws.doc_mut(doc) else { return };
    if od.doc.revision() != revision {
        s.status = format!("{what} skipped: the file changed while {} ran", outcome.label);
        return;
    }
    if od.read_only {
        s.status = "The file is read-only".into();
        return;
    }
    if formatted.is_empty() && !od.doc.text().trim().is_empty() {
        // A tool that printed nothing for a non-empty file did not format it.
        s.status = format!("{}: no output", outcome.label);
        return;
    }
    let sels = sels.clamp(od.doc.len_chars());
    let edit = crate::reformat::diff_edit(&od.doc, &formatted, &sels);
    let path = od.path.clone();
    if edit.is_noop() {
        s.status = format!("{what} with {} (no changes)", outcome.label);
        return;
    }
    match od.doc.apply(&edit, &sels, codecraft_text::EditGroup::Standalone) {
        Ok(()) => {
            s.status = format!("{what} with {}", outcome.label);
            if let Some(e) = editor {
                s.ws.set_selections(e, edit.selections);
            }
            if save_after && let Some(path) = path {
                s.py_format.skip_on_save.insert(path);
                let _ = s.save_all();
            }
        }
        Err(e) => s.status = e.to_string(),
    }
}

/// Reformat / optimize imports of saved Python files when the settings ask for it.
pub(crate) fn after_save(s: &mut Session, saved: &[PathBuf]) {
    let format = s.ws.settings.python_reformat_on_save && formatter_setting(s) != "none";
    let imports = s.ws.settings.python_optimize_imports_on_save;
    for path in saved {
        if s.py_format.skip_on_save.remove(path) || !(format || imports) {
            continue;
        }
        let Some(doc) = s.ws.doc_by_path(path) else { continue };
        if s.ws.doc(doc).is_none_or(|d| d.language != Language::Python) {
            continue;
        }
        let op = match (imports, format) {
            (true, true) => Op::Both,
            (true, false) => Op::Imports,
            _ => Op::Format,
        };
        if let Err(e) = start(s, doc, op, true) {
            s.status = e;
        }
    }
}

fn active_python(s: &Session) -> Option<DocId> {
    s.ws.active().filter(|(_, d)| d.language == Language::Python).map(|(_, d)| d.id)
}

// ---------------------------------------------------------------- routing

type Pair = (Enabled, Run);
static ORIGINAL: OnceLock<HashMap<&'static str, Pair>> = OnceLock::new();

const REFORMAT: &str = "code.reformatCode";
const REFORMAT_FILE: &str = "code.reformatFile";
const OPTIMIZE_IMPORTS: &str = "code.optimizeImports";

/// Make Reformat Code / Reformat File / Optimize Imports handle Python files.
pub(crate) fn route(specs: &mut [CommandSpec]) {
    let mut orig = HashMap::new();
    for spec in specs.iter_mut() {
        let run: Run = match spec.id {
            REFORMAT => reformat_run,
            REFORMAT_FILE => reformat_file_run,
            OPTIMIZE_IMPORTS => imports_run,
            _ => continue,
        };
        orig.insert(spec.id, (spec.enabled, spec.run));
        spec.run = run;
    }
    let _ = ORIGINAL.set(orig);
}

fn orig_run(id: &str, s: &mut Session, p: &Value) -> CmdResult {
    match ORIGINAL.get().and_then(|m| m.get(id)) {
        Some((_, r)) => r(s, p),
        None => Err(CommandError::Unknown(id.to_string())),
    }
}

fn reformat_python(s: &mut Session, doc: DocId) -> CmdResult {
    start(s, doc, Op::Format, false).map_err(CommandError::Failed)?;
    Ok(json!({ "formatter": formatter_setting(s) }))
}

fn reformat_run(s: &mut Session, p: &Value) -> CmdResult {
    match active_python(s) {
        Some(doc) => reformat_python(s, doc),
        None => orig_run(REFORMAT, s, p),
    }
}

fn reformat_file_run(s: &mut Session, p: &Value) -> CmdResult {
    match active_python(s) {
        Some(doc) => reformat_python(s, doc),
        None => orig_run(REFORMAT_FILE, s, p),
    }
}

fn imports_run(s: &mut Session, p: &Value) -> CmdResult {
    let Some(doc) = active_python(s) else { return orig_run(OPTIMIZE_IMPORTS, s, p) };
    // Without Ruff or isort installed, the language server's Organize Imports.
    let file = s.ws.doc(doc).and_then(|d| d.path.as_ref()).map(|p| p.display().to_string()).unwrap_or_default();
    let has_tool = candidates(s, Op::Imports, &file).iter().any(|c| !c.module);
    if !has_tool && s.lsp_ready().is_ok() {
        return orig_run(OPTIMIZE_IMPORTS, s, p);
    }
    start(s, doc, Op::Imports, false).map_err(CommandError::Failed)?;
    Ok(Value::Null)
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "python.format.tools",
        "Python Formatting Tools",
        None,
        "→ {formatter: auto|ruff|black|none, reformatOnSave, optimizeImportsOnSave, format: [{label, program}], \
         imports: [{label, program}]} — what Reformat Code and Optimize Imports would run for Python",
        Context::Global,
        always,
        |s, _| {
            let list = |op| {
                candidates(s, op, "x.py")
                    .into_iter()
                    .map(|c| json!({ "label": c.label, "program": c.program, "module": c.module }))
                    .collect::<Vec<_>>()
            };
            Ok(json!({
                "formatter": formatter_setting(s),
                "reformatOnSave": s.ws.settings.python_reformat_on_save,
                "optimizeImportsOnSave": s.ws.settings.python_optimize_imports_on_save,
                "format": list(Op::Format),
                "imports": list(Op::Imports),
            }))
        },
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_per_tool() {
        assert_eq!(tool_args("ruff", Op::Format, "a.py"), ["format", "--quiet", "--stdin-filename", "a.py", "-"]);
        assert_eq!(tool_args("black", Op::Format, "a.py"), ["-q", "--stdin-filename", "a.py", "-"]);
        assert!(tool_args("ruff", Op::Imports, "a.py").contains(&"I".to_string()));
        assert_eq!(tool_args("isort", Op::Imports, "a.py"), ["--filename", "a.py", "-"]);
        assert!(tool_args("nope", Op::Format, "a.py").is_empty());
    }

    #[test]
    fn missing_programs_fall_through() {
        let list = vec![Invocation {
            label: "Ruff",
            program: PathBuf::from("/definitely/not/here/ruff"),
            args: Vec::new(),
            module: false,
        }];
        let out = run_candidates(&list, "x = 1\n", None, "format");
        assert!(out.result.unwrap_err().contains("not installed"));
        assert!(out.log.iter().any(|(k, _)| *k == LineKind::Command));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_input_pipes_and_times_out() {
        let (code, out, _) =
            run_with_input(Path::new("/bin/cat"), &[], None, "héllo\n", Duration::from_secs(10)).unwrap();
        assert_eq!((code, out.as_str()), (Some(0), "héllo\n"));
        let err = run_with_input(Path::new("/bin/sleep"), &["5".into()], None, "", Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
}
