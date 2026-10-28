//! The data a project's specs parse into. Lines are 1-based throughout.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::code::Symbol;

/// How a requirement's sentence is built, after the EARS templates (Easy Approach to
/// Requirements Syntax). The keywords are matched in upper case, as EARS writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EarsKind {
    /// `THE SYSTEM SHALL …`
    Ubiquitous,
    /// `WHEN <trigger> THE SYSTEM SHALL …`
    EventDriven,
    /// `WHILE <state> THE SYSTEM SHALL …`
    StateDriven,
    /// `IF <condition> THEN THE SYSTEM SHALL …`
    Unwanted,
    /// `WHERE <feature> THE SYSTEM SHALL …`
    Optional,
    /// More than one of WHEN / WHILE / WHERE.
    Complex,
    /// No `SHALL`: prose that cannot be checked mechanically.
    Unstructured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// A problem found while reading a spec file. Reading never fails on bad content: it reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// File name inside the feature directory (`requirements.md`).
    pub file: String,
    pub line: u32,
    pub severity: Severity,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requirement {
    /// `REQ-12`: stable, never reused; links and history refer to it.
    pub id: String,
    pub number: u32,
    /// Empty for the bullet form (`- REQ-3: …`).
    pub title: String,
    /// The requirement's sentence(s), without the heading.
    pub text: String,
    /// Line of the heading (or bullet).
    pub line: u32,
    /// Last line that belongs to the requirement.
    pub end_line: u32,
    pub ears: EarsKind,
    /// Fingerprint of title and text (whitespace-insensitive), 16 hex digits.
    pub hash: String,
}

impl Requirement {
    /// What lists show: the title, else the first line of the text.
    pub fn display_title(&self) -> String {
        let t = if self.title.is_empty() { self.text.lines().next().unwrap_or("") } else { self.title.as_str() };
        t.chars().take(100).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskState {
    Todo,
    /// `- [-]`
    Doing,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// The numbering written in front of the text (`2.1`), empty when there is none.
    pub id: String,
    pub text: String,
    pub state: TaskState,
    /// Nesting: 0 for a top-level item.
    pub depth: u8,
    pub line: u32,
    /// Requirement ids (`REQ-3`) the task says it implements.
    pub requirements: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub line: u32,
}

/// How a code reference to a requirement is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkKind {
    /// `// spec: REQ-12`
    Comment,
    /// `#[spec("REQ-12")]` (or `@spec("REQ-12")`)
    Attribute,
}

/// A reference to a requirement found in code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeLink {
    /// `auth` in `auth/REQ-3`; `None` for a bare `REQ-3`.
    pub feature: Option<String>,
    pub id: String,
    pub line: u32,
    pub kind: LinkKind,
    /// The item the reference is attached to, when it could be delimited (see [`crate::attach_symbols`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Symbol>,
}

/// One feature: a directory of `requirements.md`, `design.md` and `tasks.md`.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Feature {
    /// The directory name.
    pub name: String,
    pub dir: PathBuf,
    /// The first level-1 heading of `requirements.md`.
    pub title: Option<String>,
    pub requirements_file: Option<PathBuf>,
    pub design_file: Option<PathBuf>,
    pub tasks_file: Option<PathBuf>,
    pub requirements: Vec<Requirement>,
    pub design: Vec<Heading>,
    pub tasks: Vec<Task>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Feature {
    pub fn requirement(&self, id: &str) -> Option<&Requirement> {
        self.requirements.iter().find(|r| r.id == id)
    }

    /// `(done, total)` over all tasks.
    pub fn task_progress(&self) -> (usize, usize) {
        let done = self.tasks.iter().filter(|t| t.state == TaskState::Done).count();
        (done, self.tasks.len())
    }
}
