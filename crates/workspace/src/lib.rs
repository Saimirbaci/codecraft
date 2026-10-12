//! CodeCraft workspace model (L1): pure data, no I/O, no threads.
//!
//! What the IDE has open: the project (file tree + Cargo metadata), documents and their editors
//! (tabs), problems from the language server and Cargo, run/build consoles, find results,
//! navigation history, tool-window layout, notifications and background tasks. The engine owns
//! one [`Workspace`] and mutates it from commands and service events; the UI reads it.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod document;
mod layout;
mod problems;
mod project;
mod run;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use document::{DocId, Editor, EditorId, OpenDocument};
pub use layout::{Layout, Side, ToolWindowId};
pub use problems::{Problem, ProblemSource, Severity};
pub use project::Project;
pub use run::{ConsoleKind, ConsoleLine, RunStatus, RunTab, RunTabId, TestRecord};

use codecraft_fs::FileMatch;
use codecraft_text::search::SearchQuery;
use codecraft_text::{Document, SelectionSet};

/// A place in a file, for navigation history and "go to" results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: PathBuf,
    /// Char offset in the document.
    pub offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub level: NotificationLevel,
    pub title: String,
    pub message: String,
}

/// A background task shown in the status bar (indexing, cargo metadata, …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub title: String,
    pub message: Option<String>,
    pub percentage: Option<u32>,
}

/// Results shown in the Find tool window (Find in Files, Find Usages).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindResults {
    pub title: String,
    pub query: SearchQuery,
    pub matches: Vec<FileMatch>,
    pub searching: bool,
    pub selected: usize,
}

/// User preferences (Settings dialog). Missing fields load as defaults, so older settings files
/// keep working.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub editor_font_size: f32,
    pub ui_font_size: f32,
    pub line_spacing: f32,
    pub tab_size: usize,
    pub show_whitespace: bool,
    pub soft_wrap: bool,
    pub show_line_numbers: bool,
    pub show_inlay_hints: bool,
    /// Run `cargo check` (or clippy) on save, like RustRover's external linter.
    pub check_on_save: bool,
    pub use_clippy: bool,
    /// Explicit rust-analyzer binary; `None` = discover.
    pub rust_analyzer_path: Option<PathBuf>,
    pub theme: String,
    /// Restart a language server whose process group uses more than this (MiB); 0 = never.
    pub lsp_memory_limit_mb: u64,
    /// Smaller rust-analyzer caches and no eager dependency indexing.
    pub rust_analyzer_low_memory: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor_font_size: 13.0,
            ui_font_size: 13.0,
            line_spacing: 1.2,
            tab_size: 4,
            show_whitespace: false,
            soft_wrap: false,
            show_line_numbers: true,
            show_inlay_hints: true,
            check_on_save: false,
            use_clippy: false,
            rust_analyzer_path: None,
            theme: "Dark".into(),
            lsp_memory_limit_mb: 0,
            rust_analyzer_low_memory: false,
        }
    }
}

/// Everything the IDE has open.
#[derive(Debug, Default)]
pub struct Workspace {
    pub project: Option<Project>,
    docs: BTreeMap<DocId, OpenDocument>,
    editors: BTreeMap<EditorId, Editor>,
    /// Editor tabs in display order.
    pub tabs: Vec<EditorId>,
    pub active_editor: Option<EditorId>,
    lsp_problems: BTreeMap<PathBuf, Vec<Problem>>,
    cargo_problems: BTreeMap<PathBuf, Vec<Problem>>,
    pub runs: Vec<RunTab>,
    pub active_run: Option<RunTabId>,
    pub find: Option<FindResults>,
    pub notifications: Vec<Notification>,
    pub tasks: BTreeMap<String, BackgroundTask>,
    pub layout: Layout,
    pub settings: Settings,
    pub recent_files: Vec<PathBuf>,
    pub recent_projects: Vec<PathBuf>,
    history: Vec<Location>,
    history_pos: usize,
    next_doc: u64,
    next_editor: u64,
    next_run: u64,
}

const MAX_RECENT: usize = 50;
const MAX_HISTORY: usize = 200;
const MAX_NOTIFICATIONS: usize = 100;

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------ documents

    /// Add a document; returns the existing one if `path` is already open.
    pub fn add_document(&mut self, path: Option<PathBuf>, text: &str) -> DocId {
        if let Some(p) = &path
            && let Some(id) = self.doc_by_path(p)
        {
            return id;
        }
        self.next_doc += 1;
        let id = DocId(self.next_doc);
        if let Some(p) = &path {
            push_recent(&mut self.recent_files, p.clone());
        }
        self.docs.insert(id, OpenDocument::new(id, path, Document::from_text(text)));
        id
    }

    pub fn doc(&self, id: DocId) -> Option<&OpenDocument> {
        self.docs.get(&id)
    }

    pub fn doc_mut(&mut self, id: DocId) -> Option<&mut OpenDocument> {
        self.docs.get_mut(&id)
    }

    pub fn docs(&self) -> impl Iterator<Item = &OpenDocument> {
        self.docs.values()
    }

    pub fn docs_mut(&mut self) -> impl Iterator<Item = &mut OpenDocument> {
        self.docs.values_mut()
    }

    pub fn doc_by_path(&self, path: &Path) -> Option<DocId> {
        self.docs.values().find(|d| d.path.as_deref() == Some(path)).map(|d| d.id)
    }

    /// Remove a document that no editor shows any more.
    fn drop_doc_if_unused(&mut self, id: DocId) -> Option<OpenDocument> {
        if self.editors.values().any(|e| e.doc == id) {
            return None;
        }
        self.docs.remove(&id)
    }

    // ------------------------------------------------------------ editors

    /// Show `doc` in a tab: reuse an existing editor for it, else open a new tab after the active one.
    pub fn open_editor(&mut self, doc: DocId) -> Option<EditorId> {
        if !self.docs.contains_key(&doc) {
            return None;
        }
        if let Some(e) = self.editors.values().find(|e| e.doc == doc) {
            let id = e.id;
            self.active_editor = Some(id);
            return Some(id);
        }
        self.next_editor += 1;
        let id = EditorId(self.next_editor);
        self.editors.insert(id, Editor::new(id, doc));
        let at = self.active_editor.and_then(|a| self.tabs.iter().position(|t| *t == a)).map(|i| i + 1);
        match at {
            Some(i) if i <= self.tabs.len() => self.tabs.insert(i, id),
            _ => self.tabs.push(id),
        }
        self.active_editor = Some(id);
        Some(id)
    }

    pub fn editor(&self, id: EditorId) -> Option<&Editor> {
        self.editors.get(&id)
    }

    pub fn editor_mut(&mut self, id: EditorId) -> Option<&mut Editor> {
        self.editors.get_mut(&id)
    }

    pub fn active(&self) -> Option<(&Editor, &OpenDocument)> {
        let e = self.editors.get(&self.active_editor?)?;
        let d = self.docs.get(&e.doc)?;
        Some((e, d))
    }

    /// The active editor and its document, mutably.
    pub fn active_mut(&mut self) -> Option<(&mut Editor, &mut OpenDocument)> {
        let id = self.active_editor?;
        let e = self.editors.get_mut(&id)?;
        let d = self.docs.get_mut(&e.doc)?;
        Some((e, d))
    }

    /// Close a tab. Returns the closed document if no other editor shows it (so the caller can
    /// notify the language server). Activates the neighbouring tab like RustRover (the one to
    /// the left, or the new first tab).
    pub fn close_editor(&mut self, id: EditorId) -> Option<OpenDocument> {
        let editor = self.editors.remove(&id)?;
        let idx = self.tabs.iter().position(|t| *t == id);
        self.tabs.retain(|t| *t != id);
        if self.active_editor == Some(id) {
            self.active_editor = match idx {
                Some(i) if i > 0 => self.tabs.get(i - 1).copied(),
                _ => self.tabs.first().copied(),
            };
        }
        self.drop_doc_if_unused(editor.doc)
    }

    pub fn editor_for_doc(&self, doc: DocId) -> Option<EditorId> {
        self.editors.values().find(|e| e.doc == doc).map(|e| e.id)
    }

    // ------------------------------------------------------------ problems

    pub fn set_lsp_problems(&mut self, path: PathBuf, problems: Vec<Problem>) {
        if problems.is_empty() {
            self.lsp_problems.remove(&path);
        } else {
            self.lsp_problems.insert(path, problems);
        }
    }

    pub fn clear_cargo_problems(&mut self) {
        self.cargo_problems.clear();
    }

    pub fn add_cargo_problem(&mut self, problem: Problem) {
        let list = self.cargo_problems.entry(problem.path.clone()).or_default();
        if !list.contains(&problem) {
            list.push(problem);
        }
    }

    /// LSP and Cargo problems for one file, errors first then by position.
    pub fn problems_for(&self, path: &Path) -> Vec<&Problem> {
        let mut v: Vec<&Problem> = self
            .lsp_problems
            .get(path)
            .into_iter()
            .flatten()
            .chain(self.cargo_problems.get(path).into_iter().flatten())
            .collect();
        v.sort_by_key(|p| (p.start, p.severity));
        v
    }

    /// Every problem, grouped by file (sorted by path).
    pub fn all_problems(&self) -> BTreeMap<&Path, Vec<&Problem>> {
        let mut out: BTreeMap<&Path, Vec<&Problem>> = BTreeMap::new();
        for (path, list) in self.lsp_problems.iter().chain(self.cargo_problems.iter()) {
            out.entry(path.as_path()).or_default().extend(list.iter());
        }
        for list in out.values_mut() {
            list.sort_by_key(|p| (p.severity, p.start));
        }
        out
    }

    pub fn problem_counts(&self) -> (usize, usize) {
        let all = self.lsp_problems.values().chain(self.cargo_problems.values()).flatten();
        all.fold((0, 0), |(e, w), p| match p.severity {
            Severity::Error => (e + 1, w),
            Severity::Warning => (e, w + 1),
            _ => (e, w),
        })
    }

    // ------------------------------------------------------------ runs

    pub fn add_run(&mut self, title: impl Into<String>, command_line: impl Into<String>, build: bool) -> RunTabId {
        self.next_run += 1;
        let id = RunTabId(self.next_run);
        self.runs.push(RunTab::new(id, title.into(), command_line.into(), build));
        self.active_run = Some(id);
        id
    }

    pub fn run(&self, id: RunTabId) -> Option<&RunTab> {
        self.runs.iter().find(|r| r.id == id)
    }

    pub fn run_mut(&mut self, id: RunTabId) -> Option<&mut RunTab> {
        self.runs.iter_mut().find(|r| r.id == id)
    }

    pub fn close_run(&mut self, id: RunTabId) {
        self.runs.retain(|r| r.id != id);
        if self.active_run == Some(id) {
            self.active_run = self.runs.last().map(|r| r.id);
        }
    }

    // ------------------------------------------------------------ navigation

    /// Record a location before jumping away (Back/Forward history, Recent Locations).
    pub fn push_location(&mut self, loc: Location) {
        self.history.truncate(self.history_pos);
        if self.history.last() != Some(&loc) {
            self.history.push(loc);
        }
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            self.history.drain(..excess);
        }
        self.history_pos = self.history.len();
    }

    /// Step back; `current` is recorded so Forward can return to it.
    pub fn back(&mut self, current: Option<Location>) -> Option<Location> {
        if self.history_pos == 0 {
            return None;
        }
        if self.history_pos == self.history.len()
            && let Some(cur) = current
            && self.history.last() != Some(&cur)
        {
            self.history.push(cur);
        }
        self.history_pos -= 1;
        self.history.get(self.history_pos).cloned()
    }

    pub fn forward(&mut self) -> Option<Location> {
        if self.history_pos + 1 >= self.history.len() {
            return None;
        }
        self.history_pos += 1;
        self.history.get(self.history_pos).cloned()
    }

    pub fn can_go_back(&self) -> bool {
        self.history_pos > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_pos + 1 < self.history.len()
    }

    pub fn history(&self) -> &[Location] {
        &self.history
    }

    // ------------------------------------------------------------ misc

    pub fn notify(&mut self, level: NotificationLevel, title: impl Into<String>, message: impl Into<String>) {
        self.notifications.push(Notification { level, title: title.into(), message: message.into() });
        if self.notifications.len() > MAX_NOTIFICATIONS {
            self.notifications.remove(0);
        }
    }

    pub fn note_recent_project(&mut self, root: PathBuf) {
        push_recent(&mut self.recent_projects, root);
    }

    /// Location of the primary caret in the active editor.
    pub fn current_location(&self) -> Option<Location> {
        let (e, d) = self.active()?;
        Some(Location { path: d.path.clone()?, offset: e.selections.primary().head })
    }

    /// Selections of an editor, clamped to its document.
    pub fn set_selections(&mut self, id: EditorId, sels: SelectionSet) {
        let Some(e) = self.editors.get(&id) else { return };
        let len = self.docs.get(&e.doc).map(|d| d.doc.len_chars()).unwrap_or(0);
        if let Some(e) = self.editors.get_mut(&id) {
            e.selections = sels.clamp(len);
        }
    }
}

fn push_recent(list: &mut Vec<PathBuf>, path: PathBuf) {
    list.retain(|p| *p != path);
    list.insert(0, path);
    list.truncate(MAX_RECENT);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reuse_and_close_tabs() {
        let mut ws = Workspace::new();
        let a = ws.add_document(Some("/p/a.rs".into()), "fn a() {}");
        let b = ws.add_document(Some("/p/b.rs".into()), "fn b() {}");
        assert_eq!(ws.add_document(Some("/p/a.rs".into()), "ignored"), a);
        let ea = ws.open_editor(a).unwrap();
        let eb = ws.open_editor(b).unwrap();
        assert_eq!(ws.tabs, vec![ea, eb]);
        assert_eq!(ws.open_editor(a), Some(ea));
        assert_eq!(ws.active_editor, Some(ea));
        let closed = ws.close_editor(ea).unwrap();
        assert_eq!(closed.path.as_deref(), Some(Path::new("/p/a.rs")));
        assert_eq!(ws.active_editor, Some(eb));
        assert!(ws.doc(a).is_none());
        assert_eq!(ws.recent_files.first().map(PathBuf::as_path), Some(Path::new("/p/b.rs")));
    }

    #[test]
    fn back_forward() {
        let mut ws = Workspace::new();
        let l = |o| Location { path: "/x.rs".into(), offset: o };
        ws.push_location(l(1));
        ws.push_location(l(2));
        assert_eq!(ws.back(Some(l(3))), Some(l(2)));
        assert_eq!(ws.back(None), Some(l(1)));
        assert_eq!(ws.back(None), None);
        assert_eq!(ws.forward(), Some(l(2)));
        assert_eq!(ws.forward(), Some(l(3)));
        assert_eq!(ws.forward(), None);
    }

    #[test]
    fn problems_merge_and_count() {
        let mut ws = Workspace::new();
        let p = |sev, line| Problem {
            path: "/x.rs".into(),
            start: codecraft_text::Position::new(line, 0),
            end: codecraft_text::Position::new(line, 1),
            severity: sev,
            message: "m".into(),
            code: None,
            source: ProblemSource::RustAnalyzer,
        };
        ws.set_lsp_problems("/x.rs".into(), vec![p(Severity::Warning, 3), p(Severity::Error, 5)]);
        ws.add_cargo_problem(Problem { source: ProblemSource::Cargo, ..p(Severity::Error, 1) });
        assert_eq!(ws.problem_counts(), (2, 1));
        let lines: Vec<usize> = ws.problems_for(Path::new("/x.rs")).iter().map(|p| p.start.line).collect();
        assert_eq!(lines, vec![1, 3, 5]);
        ws.set_lsp_problems("/x.rs".into(), Vec::new());
        assert_eq!(ws.problem_counts(), (1, 0));
    }
}
