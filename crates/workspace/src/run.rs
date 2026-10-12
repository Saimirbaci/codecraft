use std::time::Duration;

use serde::{Deserialize, Serialize};

use codecraft_cargo::TestOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunTabId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleKind {
    Stdout,
    Stderr,
    /// Lines the IDE adds (the command line, "Process finished with exit code 0").
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleLine {
    pub kind: ConsoleKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed(Option<i32>),
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRecord {
    pub name: String,
    pub outcome: TestOutcome,
}

/// One tab in the Run (or Build) tool window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTab {
    pub id: RunTabId,
    pub title: String,
    pub command_line: String,
    /// Shown in the Build tool window instead of Run.
    pub build: bool,
    pub status: RunStatus,
    pub lines: Vec<ConsoleLine>,
    pub tests: Vec<TestRecord>,
    pub duration: Option<Duration>,
    /// Lines dropped from the front because of the console cap.
    pub dropped_lines: usize,
}

/// Console cap (RustRover's default cycle buffer is 1024 KB; we cap lines instead).
pub const MAX_CONSOLE_LINES: usize = 100_000;

impl RunTab {
    pub fn new(id: RunTabId, title: String, command_line: String, build: bool) -> Self {
        let lines = vec![ConsoleLine { kind: ConsoleKind::System, text: command_line.clone() }];
        Self {
            id,
            title,
            command_line,
            build,
            status: RunStatus::Running,
            lines,
            tests: Vec::new(),
            duration: None,
            dropped_lines: 0,
        }
    }

    pub fn push(&mut self, kind: ConsoleKind, text: impl Into<String>) {
        self.lines.push(ConsoleLine { kind, text: text.into() });
        if self.lines.len() > MAX_CONSOLE_LINES {
            let excess = self.lines.len() - MAX_CONSOLE_LINES;
            self.lines.drain(..excess);
            self.dropped_lines += excess;
        }
    }

    pub fn is_running(&self) -> bool {
        self.status == RunStatus::Running
    }

    pub fn test_counts(&self) -> (usize, usize, usize) {
        self.tests.iter().fold((0, 0, 0), |(p, f, i), t| match t.outcome {
            TestOutcome::Ok => (p + 1, f, i),
            TestOutcome::Failed => (p, f + 1, i),
            TestOutcome::Ignored => (p, f, i + 1),
        })
    }
}
