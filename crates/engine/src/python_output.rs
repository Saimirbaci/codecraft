//! View › Tool Windows › Python Process Output (M12 track PY): what the IDE's background Python
//! processes did — the Python Console, notebook kernels, formatters (Ruff/Black/isort) and
//! package operations — for diagnosing them: start and exit, the command lines run, and what
//! they wrote outside their protocol (stderr, crashes).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::ToolWindowId;

use crate::Session;
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, simple, spec};

/// Lines kept.
pub const MAX_LINES: usize = 5000;
/// Longest line kept (chars).
const MAX_LINE_CHARS: usize = 4000;

/// What a line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    /// Started, stopped, exited.
    Info,
    /// A command line that was run.
    Command,
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputLine {
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// `Python Console`, `Kernel: analysis.ipynb`, `Ruff`, `pip`, …
    pub source: String,
    pub kind: LineKind,
    pub text: String,
}

/// The log (newest last).
#[derive(Clone, Debug, Default)]
pub struct ProcessOutput {
    pub lines: VecDeque<OutputLine>,
    /// Bumped on every change (the UI scrolls to new lines).
    pub generation: u64,
}

impl ProcessOutput {
    /// Add a line (multi-line text is split; long lines are cut).
    pub fn push(&mut self, source: &str, kind: LineKind, text: &str) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for line in text.trim_end_matches(['\n', '\r']).split('\n') {
            let line = line.trim_end_matches('\r');
            let text: String = line.chars().take(MAX_LINE_CHARS).collect();
            if self.lines.len() >= MAX_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(OutputLine { at, source: source.to_string(), kind, text });
        }
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// The log as text (`[source] text` lines), for agents and tests.
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| format!("[{}] {}\n", l.source, l.text)).collect()
    }
}

/// A running background Python process, for the tool window's header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub name: String,
    pub pid: Option<u32>,
    pub status: String,
    pub interpreter: Option<String>,
}

/// The Python processes the IDE runs now.
pub fn processes(s: &Session) -> Vec<ProcessInfo> {
    let mut out = Vec::new();
    let c = &s.python.console;
    if c.is_alive() {
        out.push(ProcessInfo {
            name: "Python Console".into(),
            pid: c.process.as_ref().map(|p| p.pid()),
            status: if c.running.is_some() { "running" } else if c.ready { "idle" } else { "starting" }.into(),
            interpreter: c.interpreter.as_ref().map(|p| p.display().to_string()),
        });
    }
    for nb in &s.jupyter.notebooks {
        if nb.kernel.is_alive() {
            out.push(ProcessInfo {
                name: format!("Kernel: {}", nb.title()),
                pid: nb.kernel.pid(),
                status: if nb.kernel.running.is_some() { "busy" } else if nb.kernel.ready { "idle" } else { "starting" }
                    .into(),
                interpreter: nb.kernel.interpreter.as_ref().map(|p| p.display().to_string()),
            });
        }
    }
    for job in &s.python.jobs {
        out.push(ProcessInfo { name: job.title.clone(), pid: None, status: "running".into(), interpreter: None });
    }
    for name in s.py_format.running() {
        out.push(ProcessInfo { name, pid: None, status: "running".into(), interpreter: None });
    }
    out
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("view.toolWindows.pythonProcessOutput", "Python Process Output", always, |s, _| {
            s.ws.layout.toggle(ToolWindowId::PythonProcessOutput);
            ok()
        }),
        simple("python.processOutput.clear", "Clear Python Process Output", always, |s, _| {
            s.python.process_output.clear();
            ok()
        }),
        spec(
            "python.processOutput.state",
            "Python Process Output State",
            None,
            "{limit?: int (default 500)} → {processes: [{name, pid, status, interpreter}], lines: [{at, source, kind, text}]}",
            Context::Global,
            always,
            state,
        ),
    ]
}

fn state(s: &mut Session, p: &Value) -> CmdResult {
    let limit = p.get("limit").and_then(Value::as_u64).map_or(500, |n| n.min(MAX_LINES as u64) as usize);
    let lines = &s.python.process_output.lines;
    let skip = lines.len().saturating_sub(limit);
    Ok(json!({
        "processes": processes(s),
        "lines": lines.iter().skip(skip).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_and_cap() {
        let mut o = ProcessOutput::default();
        o.push("pip", LineKind::Stdout, "a\r\nb\n");
        assert_eq!(o.text(), "[pip] a\n[pip] b\n");
        o.push("x", LineKind::Stderr, &"y".repeat(10_000));
        assert_eq!(o.lines.back().unwrap().text.chars().count(), MAX_LINE_CHARS);
        for i in 0..MAX_LINES + 10 {
            o.push("n", LineKind::Info, &i.to_string());
        }
        assert_eq!(o.lines.len(), MAX_LINES);
        o.clear();
        assert!(o.lines.is_empty());
    }
}
