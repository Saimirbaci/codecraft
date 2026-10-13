//! CodeCraft engine (L2): the [`Session`], its background services and the command registry.
//!
//! Every user-visible action is a command (`commands.rs`), dispatched by id with JSON params from
//! the UI, the CLI or an agent. Long-running work (language server, cargo, project scans, find in
//! files) runs on worker threads; [`Session::pump`] drains their results once per frame.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod agent_cmds;
pub mod agents;
pub mod approvals;
pub mod catalog;
mod code_cmds;
pub mod commands;
pub mod commit_ai;
mod convert;
pub mod database;
mod database_cmds;
pub mod debug;
mod debug_cmds;
mod decorations;
mod edit_cmds;
mod editing_cmds;
mod editor_cmds;
pub mod feedback;
mod feedback_cmds;
mod file_cmds;
mod ide_cmds;
pub mod ide_server;
mod jobs;
mod lang_cmds;
mod lsp;
mod navigate_cmds;
mod params;
pub mod refactor;
mod refactor_cmds;
pub mod popup;
mod run_cmds;
pub mod run_configs;
mod runs;
mod split_cmds;
pub mod terminal;
mod terminal_cmds;
pub mod testing;
mod testing_cmds;
pub mod vcs;
mod vcs_cmds;
mod view_cmds;
mod watch;
mod window_cmds;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use codecraft_agent as agent;
pub use codecraft_cargo as cargo;
pub use codecraft_dap as dap;
pub use codecraft_db as db;
pub use codecraft_fs as fs;
pub use codecraft_lsp::ServerKind;
pub use codecraft_mcp as mcp;
pub use codecraft_syntax as syntax;
pub use codecraft_term as term;
pub use codecraft_text as text;
pub use codecraft_vcs as vcs_git;
pub use codecraft_workspace as workspace;
pub use commands::{CatalogItem, CommandError, CommandSpec, Context, Registry, Scope, registry};
pub use lsp::server_kind;
pub use popup::{FindBar, Popup};
pub use runs::{ProcessCommand, RunConfig, derive_configs};
pub use view_cmds::IMPLEMENTED_TOOL_WINDOWS;

use codecraft_text::ops::OpsConfig;
use codecraft_text::{Document, Edit, EditGroup, SelectionSet};
use codecraft_workspace::{DocId, EditorId, Location, NotificationLevel, Workspace};

/// Side effects the UI shell performs on the engine's behalf.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// Put text on the system clipboard.
    CopyToClipboard(String),
    /// Ask the user for a file or directory to open, then run `file.open` with `{path}`.
    PickOpenPath,
    /// Ask for a directory for a new Cargo project, then run `file.new.project` with `{path}`.
    PickNewProjectPath,
    /// Close the application window.
    Quit,
    /// Give keyboard focus to the active editor.
    FocusEditor,
    /// Open a URL in the browser.
    OpenUrl(String),
}

/// State of the language server connection, for the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LspStatus {
    #[default]
    Off,
    /// rust-analyzer could not be found; the message says how to install it.
    Unavailable(String),
    Starting,
    Ready,
    /// The server stopped (crash or exit); the message has details.
    Stopped(String),
}

/// One IDE window's worth of state and services.
pub struct Session {
    pub ws: Workspace,
    pub popup: Option<Popup>,
    pub find_bar: Option<FindBar>,
    /// Transient message for the status bar.
    pub status: String,
    pub lsp_status: LspStatus,
    /// Effects for the UI to perform; drained by the shell each frame.
    pub effects: Vec<Effect>,
    /// Internal clipboard (text, whole-line flag); mirrors what we last copied.
    pub clipboard: Option<(String, bool)>,
    pub run_configs: Vec<RunConfig>,
    pub selected_run_config: usize,
    /// Lines per page for Page Up/Down; the UI updates it from the editor height.
    pub page_lines: usize,
    /// Start language servers for opened projects (the headless CLI turns this off).
    pub lsp_enabled: bool,
    /// Pending human approvals for agent actions (shared by every agent integration).
    pub approvals: approvals::Approvals,
    pub debug: debug::DebugState,
    pub db: database::DbState,
    pub agents: agents::AgentsState,
    pub ide: ide_server::IdeServerState,
    pub terminal: terminal::TerminalState,
    pub vcs: vcs::VcsState,
    /// Project file watcher (external changes).
    watch: watch::Watch,
    /// The feedback overlay's draft and records (feedback track).
    pub feedback: feedback::FeedbackState,
    /// Test tree, history and coverage (M6).
    pub testing: testing::TestingState,
    /// Refactoring previews and hierarchies (M7).
    pub refactor: refactor::RefactorState,
    /// Previous selections for Shrink Selection (per editor, most recent last).
    selection_stack: Vec<(EditorId, SelectionSet)>,
    /// Last edit location (Navigate › Last Edit Location).
    last_edit: Option<Location>,
    /// Running language servers, one per kind.
    servers: std::collections::BTreeMap<codecraft_lsp::ServerKind, lsp::LspState>,
    /// Last known status of each server kind (including unavailable/stopped ones).
    server_status: std::collections::BTreeMap<codecraft_lsp::ServerKind, LspStatus>,
    server_memory_mb: std::collections::BTreeMap<codecraft_lsp::ServerKind, u64>,
    server_memory: lsp::ServerMemory,
    decor: decorations::DecorState,
    runs: runs::Runs,
    jobs: jobs::Jobs,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            ws: Workspace::new(),
            popup: None,
            find_bar: None,
            status: String::new(),
            lsp_status: LspStatus::Off,
            effects: Vec::new(),
            clipboard: None,
            run_configs: Vec::new(),
            selected_run_config: 0,
            page_lines: 40,
            lsp_enabled: true,
            approvals: approvals::Approvals::default(),
            debug: debug::DebugState::default(),
            db: database::DbState::default(),
            agents: agents::AgentsState::default(),
            ide: ide_server::IdeServerState::default(),
            terminal: terminal::TerminalState::default(),
            vcs: vcs::VcsState::default(),
            watch: watch::Watch::default(),
            feedback: feedback::FeedbackState::default(),
            testing: testing::TestingState::default(),
            refactor: refactor::RefactorState::default(),
            selection_stack: Vec::new(),
            last_edit: None,
            servers: Default::default(),
            server_status: Default::default(),
            server_memory_mb: Default::default(),
            server_memory: Default::default(),
            decor: Default::default(),
            runs: runs::Runs::default(),
            jobs: jobs::Jobs::default(),
        }
    }

    /// Run a command by id. Never panics: a panic inside a command is caught and reported.
    pub fn execute(&mut self, id: &str, params: &Value) -> Result<Value, CommandError> {
        let reg = registry();
        let Some(spec) = reg.get(id) else {
            return Err(CommandError::Unknown(id.to_string()));
        };
        (spec.enabled)(self).map_err(CommandError::Disabled)?;
        let run = spec.run;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(self, params)));
        match result {
            Ok(r) => r,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                log::error!("command {id} panicked: {msg}");
                Err(CommandError::Failed(format!("internal error in {id}: {msg}")))
            }
        }
    }

    /// Whether a command can run now (`Err` carries the reason it is disabled).
    pub fn enabled(&self, id: &str) -> Result<(), String> {
        match registry().get(id) {
            Some(spec) => (spec.enabled)(self),
            None => Err("not implemented yet".into()),
        }
    }

    /// Drain background services. Returns true if anything changed (the UI should repaint).
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        changed |= self.pump_lsp();
        changed |= runs::pump(self);
        changed |= jobs::pump(self);
        changed |= self.approvals.pump();
        changed |= debug::pump(self);
        changed |= database::pump(self);
        changed |= agents::pump(self);
        changed |= ide_server::pump(self);
        changed |= terminal::pump(self);
        changed |= vcs::pump(self);
        changed |= watch::pump(self);
        changed |= testing::pump(self);
        changed |= refactor::pump(self);
        changed
    }

    /// Whether background work is in flight (the UI keeps repainting to poll).
    pub fn busy(&self) -> bool {
        !self.servers.is_empty()
            || self.runs.any_running()
            || self.jobs.any_running()
            || self.agents.any_active()
            || self.ide.busy()
            || self.db.busy()
            || self.debug.busy()
            || self.vcs.busy()
    }

    /// Whether processes or jobs (not the language server) are still running.
    pub fn jobs_running(&self) -> bool {
        self.runs.any_running() || self.jobs.any_running() || self.db.busy() || self.debug.settling()
    }

    /// Stop the language server and running processes (window closing).
    pub fn shutdown(&mut self) {
        debug::shutdown(self);
        database::shutdown(self);
        agents::shutdown(self);
        ide_server::shutdown(self);
        terminal::shutdown(self);
        vcs::shutdown(self);
        self.stop_watching();
        self.approvals.deny_all();
        runs::stop_all(self);
        self.stop_lsp();
    }

    // ------------------------------------------------------------ helpers for commands

    pub(crate) fn active_ids(&self) -> Option<(EditorId, DocId)> {
        let (e, d) = self.ws.active()?;
        Some((e.id, d.id))
    }

    pub(crate) fn ops_config(&self, doc: DocId) -> OpsConfig {
        let mut cfg = OpsConfig { tab_width: self.ws.settings.tab_size, ..OpsConfig::default() };
        if let Some(d) = self.ws.doc(doc) {
            cfg.indent_unit = d.doc.detect_indent_unit();
            if cfg.indent_unit != "\t" {
                cfg.indent_unit = " ".repeat(self.ws.settings.tab_size.clamp(1, 16));
            }
            cfg.line_comment = d.language.line_comment().map(str::to_string);
            cfg.block_comment = d.language.block_comment().map(|(a, b)| (a.to_string(), b.to_string()));
        }
        cfg
    }

    /// Compute an edit on the active editor and apply it (undoable). Returns false without an editor.
    pub(crate) fn edit_active(
        &mut self,
        group: EditGroup,
        f: impl FnOnce(&Document, &SelectionSet, &OpsConfig) -> Edit,
    ) -> Result<bool, CommandError> {
        let Some((eid, did)) = self.active_ids() else { return Ok(false) };
        let cfg = self.ops_config(did);
        let Some((editor, od)) = self.ws.active_mut() else { return Ok(false) };
        if od.read_only {
            return Err(CommandError::Failed("the file is read-only".into()));
        }
        let before = editor.selections.clone();
        let edit = f(&od.doc, &before, &cfg);
        if !edit.is_noop() {
            od.doc.apply(&edit, &before, group).map_err(|e| CommandError::Failed(e.to_string()))?;
        }
        editor.selections = edit.selections.clamp(od.doc.len_chars());
        editor.reveal_caret = true;
        let path = od.path.clone();
        let head = editor.selections.primary().head;
        if !edit.is_noop() {
            if let Some(path) = path {
                self.last_edit = Some(Location { path, offset: head });
            }
            self.after_text_change(eid);
        }
        Ok(true)
    }

    /// Replace the active editor's selections (caret motion); breaks the typing undo group.
    pub(crate) fn select_active(&mut self, f: impl FnOnce(&Document, &SelectionSet) -> SelectionSet) -> bool {
        let Some((editor, od)) = self.ws.active_mut() else { return false };
        let sels = f(&od.doc, &editor.selections).clamp(od.doc.len_chars());
        if sels != editor.selections {
            od.doc.break_undo_group();
        }
        editor.selections = sels;
        editor.reveal_caret = true;
        if let Some(Popup::Hover { .. } | Popup::ParameterInfo { .. }) = self.popup {
            self.popup = None;
        }
        true
    }

    /// Housekeeping after the text of an editor's document changed.
    fn after_text_change(&mut self, editor: EditorId) {
        if let Some(Popup::Hover { .. }) = self.popup {
            self.popup = None;
        }
        if let Some(Popup::Completion(c)) = &self.popup
            && c.editor != editor
        {
            self.popup = None;
        }
        self.refresh_completion_filter();
        if let Some(fb) = &mut self.find_bar {
            fb.invalidate();
        }
    }

    /// Open (or focus) a file in an editor tab; returns its editor.
    pub fn open_file(&mut self, path: &Path) -> Result<EditorId, CommandError> {
        let path = normalize_path(path);
        let doc = match self.ws.doc_by_path(&path) {
            Some(d) => d,
            None => {
                let text = codecraft_fs::read_text_file(&path).map_err(|e| CommandError::Failed(e.to_string()))?;
                let id = self.ws.add_document(Some(path.clone()), &text);
                if let Some(od) = self.ws.doc_mut(id) {
                    od.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    od.read_only = std::fs::metadata(&path).map(|m| m.permissions().readonly()).unwrap_or(false);
                }
                self.lsp_did_open(id);
                id
            }
        };
        let editor = self.ws.open_editor(doc).ok_or_else(|| CommandError::Failed("cannot open editor".into()))?;
        if let Some(p) = &mut self.ws.project
            && path.starts_with(&p.root)
        {
            p.selected = Some(path.clone());
        }
        self.effects.push(Effect::FocusEditor);
        Ok(editor)
    }

    /// Open a file and put the caret at a char offset (recording navigation history).
    pub fn navigate_to(&mut self, path: &Path, offset: usize) -> Result<(), CommandError> {
        if let Some(cur) = self.ws.current_location() {
            self.ws.push_location(cur);
        }
        self.goto_location(path, offset)
    }

    /// Open a file at (line, char column), zero-based.
    pub fn navigate_to_position(&mut self, path: &Path, line: usize, column: usize) -> Result<(), CommandError> {
        let editor = self.open_file(path)?;
        let offset = self
            .ws
            .editor(editor)
            .and_then(|e| self.ws.doc(e.doc))
            .map(|d| d.doc.position_to_offset(codecraft_text::Position::new(line, column)))
            .unwrap_or(0);
        self.navigate_to(path, offset)
    }

    /// Jump without recording history (used by Back/Forward).
    fn goto_location(&mut self, path: &Path, offset: usize) -> Result<(), CommandError> {
        let editor = self.open_file(path)?;
        let len = self.ws.editor(editor).and_then(|e| self.ws.doc(e.doc)).map(|d| d.doc.len_chars()).unwrap_or(0);
        self.ws.set_selections(editor, SelectionSet::caret(offset.min(len)));
        if let Some(e) = self.ws.editor_mut(editor) {
            e.reveal_caret = true;
        }
        Ok(())
    }

    pub(crate) fn notify(&mut self, level: NotificationLevel, title: &str, message: impl Into<String>) {
        let message = message.into();
        self.status = if message.is_empty() { title.to_string() } else { format!("{title}: {message}") };
        self.ws.notify(level, title, message);
    }

    /// Save every modified document to disk (RustRover's Save All, Cmd+S).
    pub fn save_all(&mut self) -> Result<usize, CommandError> {
        let mut saved = 0usize;
        let mut errors = Vec::new();
        let mut saved_paths = Vec::new();
        for od in self.ws.docs_mut() {
            let Some(path) = od.path.clone() else { continue };
            if !od.doc.is_modified() {
                continue;
            }
            match codecraft_fs::write_text_file_atomic(&path, &od.doc.to_disk_text()) {
                Ok(()) => {
                    od.doc.mark_saved();
                    od.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    saved += 1;
                    saved_paths.push(path);
                }
                Err(e) => errors.push(e.to_string()),
            }
        }
        for p in &saved_paths {
            self.lsp_did_save(p);
        }
        if !errors.is_empty() {
            let msg = errors.join("\n");
            self.notify(NotificationLevel::Error, "Cannot save files", msg.clone());
            return Err(CommandError::Failed(msg));
        }
        Ok(saved)
    }

    /// The project root, or the directory of the active file.
    pub fn project_root(&self) -> Option<PathBuf> {
        if let Some(p) = &self.ws.project {
            return Some(p.root.clone());
        }
        let (_, d) = self.ws.active()?;
        d.path.as_deref()?.parent().map(Path::to_path_buf)
    }
}

/// Absolute, lexically normalized path (no `.`/`..`), without touching the file system.
pub fn normalize_path(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
