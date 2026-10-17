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
mod session_state;
mod splits;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use document::{
    DocId, Editor, EditorId, InlayHint, InlayKind, InlayLabelPart, InlayLocation, OpenDocument, SemanticSpan,
    TextEncoding,
};
pub use layout::{Layout, Side, ToolWindowId};
pub use problems::{Problem, ProblemSource, Severity};
pub use project::Project;
pub use run::{ConsoleKind, ConsoleLine, RunStatus, RunTab, RunTabId, TestRecord};
pub use session_state::{
    GroupSnapshot, MAX_GROUPS, MAX_TABS, MAX_TERMINALS, RestoreSummary, SNAPSHOT_VERSION, SelectionSnapshot,
    SessionSnapshot, TabSnapshot, TerminalSnapshot,
};
pub use splits::{EditorGroup, GroupId, MIN_RATIO, SplitAxis, SplitNode, Stretch};

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

/// Code › Analyze Code › Configure Current File Analysis: how much of a file's analysis is
/// highlighted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AnalysisLevel {
    /// No problems are highlighted.
    None,
    /// Only syntax errors (errors without a compiler error code).
    Syntax,
    /// Errors and warnings (no weak warnings or hints).
    Essential,
    /// Everything.
    #[default]
    All,
}

impl AnalysisLevel {
    pub const ALL: [AnalysisLevel; 4] = [Self::None, Self::Syntax, Self::Essential, Self::All];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Syntax => "Syntax",
            Self::Essential => "Essential",
            Self::All => "All Problems",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Self::None),
            "syntax" => Some(Self::Syntax),
            "essential" => Some(Self::Essential),
            "all" | "all problems" | "inspections" => Some(Self::All),
            _ => None,
        }
    }

    /// Whether a problem is highlighted at this level.
    pub fn shows(self, p: &Problem) -> bool {
        match self {
            Self::None => false,
            Self::Syntax => {
                p.severity == Severity::Error
                    && p.code.as_deref().is_none_or(|c| c.to_ascii_lowercase().contains("syntax"))
            }
            Self::Essential => matches!(p.severity, Severity::Error | Severity::Warning),
            Self::All => true,
        }
    }
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
    /// Which whitespace `show_whitespace` paints.
    pub whitespace_mode: WhitespaceMode,
    pub soft_wrap: bool,
    /// Edit › Find › Scroll to Results During Typing: select the first match as the query is typed.
    pub find_scroll_to_results: bool,
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
    /// Commit message generation: `openrouter`, `ollama`, `claude` or `openai`.
    pub commit_ai_provider: String,
    /// Model name; empty = the provider's default.
    pub commit_ai_model: String,
    /// Base URL of the Ollama server.
    pub ollama_url: String,
    /// API keys by provider id. An empty entry falls back to the provider's environment variable.
    pub commit_ai_keys: BTreeMap<String, String>,
    /// Active keymap (`macOS` or `Windows/Linux`); empty = the platform's default.
    pub keymap: String,
    /// User shortcut changes: keymap name → command id → the command's shortcuts (an empty list
    /// removes all of them).
    pub keymap_overrides: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// Editor colour scheme (a built-in or user scheme name); empty = the one matching `theme`.
    pub color_scheme: String,
    /// User colour schemes (Settings › Editor › Color Scheme).
    pub user_schemes: Vec<ColorScheme>,
    /// Show the menus in the macOS global menu bar instead of inside the window (macOS only).
    pub native_menu_bar: bool,
    /// Code Completion: show the list automatically while typing.
    pub completion_auto_popup: bool,
    /// Code Completion: delay before the automatic list appears (ms).
    pub completion_auto_popup_delay_ms: u64,
    /// Code Completion: insert argument placeholders when completing a function (rust-analyzer
    /// `fill_arguments`); off = `name()` with the caret inside, like RustRover.
    pub completion_argument_placeholders: bool,
    /// Refactor › Rename edits the name in the editor (off = the rename dialog).
    pub rename_in_place: bool,
    /// Reopen the last project(s) and their editors on startup (System Settings).
    pub reopen_last_project: bool,
    /// Hot exit (System Settings): unsaved changes and untitled buffers are kept in
    /// `.codecraft/unsaved/` across restarts instead of being written or asked about on quit.
    pub hot_exit: bool,
    /// Generate run configurations for the project's entry points when a project without saved
    /// configurations opens.
    pub generate_run_configs: bool,
    /// File › Power Save Mode: no completion auto-popup, inlay/semantic requests, check on save
    /// or live TODO scanning.
    pub power_save_mode: bool,
    /// File › File Properties › Associate with File Type: file name glob (`*.inc`, `Justfile`) →
    /// language name (`Rust`, `Plain text`, …).
    pub file_type_associations: BTreeMap<String, String>,
    /// Right margin (columns) for Edit › Fill Paragraph.
    pub right_margin: usize,
    /// Settings › Editor › Editor Tabs: show the tab bar.
    pub show_editor_tabs: bool,
    /// Close the least recently opened unmodified, unpinned tab beyond this many per group; 0 = no limit.
    pub editor_tab_limit: usize,
    /// A single click in the Project view opens a preview tab.
    pub preview_tab: bool,
    /// Window › Processes › Auto Show: open the Processes window when a background task starts.
    pub processes_auto_show: bool,
    /// View › Appearance › Compact Mode: tighter spacing, lower toolbar and status bar.
    pub compact_mode: bool,
    /// View › Appearance › Zoom IDE: scale of the whole UI (1.0 = 100%).
    pub ide_zoom: f32,
    /// Editor font size in View › Appearance › Enter Presentation Mode.
    pub presentation_font_size: f32,
    /// View › Appearance › Toolbar.
    pub show_toolbar: bool,
    /// View › Appearance › Navigation Bar placement.
    pub navigation_bar: NavigationBar,
    /// View › Appearance › Navigation Bar › Show Members: the symbol at the caret ends the path.
    pub navigation_bar_members: bool,
    /// View › Appearance › Tool Window Bars (the stripes with the tool window buttons).
    pub show_tool_window_bars: bool,
    /// View › Appearance › Status Bar.
    pub show_status_bar: bool,
    /// View › Appearance › Status Bar Widgets: visibility by widget key (`lineColumnNumber`, …);
    /// a missing key uses the widget's default.
    pub status_bar_widgets: BTreeMap<String, bool>,
    /// Edit › Find Usages › Find Usages Settings….
    pub find_usages: FindUsagesOptions,
    /// Code › Reformat Code for Python: `auto` (Ruff, else Black), `ruff`, `black` or `none`.
    pub python_formatter: String,
    /// Reformat Python files with that formatter when they are saved.
    pub python_reformat_on_save: bool,
    /// Optimize imports (Ruff's isort rules, or isort) of Python files when they are saved.
    pub python_optimize_imports_on_save: bool,
    // ---- M13 track ST: settings and keymaps
    /// Settings › Editor › Font: the editor font family (a system font); empty = the built-in
    /// choice (a coding font when installed, else the platform's monospace font).
    pub editor_font_family: String,
    /// Settings › Editor › Code Style › <language>: tab and indent per language id (`rust`,
    /// `python`, …); a missing language uses `tab_size`.
    pub indent_by_language: BTreeMap<String, LanguageIndent>,
    /// Settings › Keymap: the user's keymaps, each a copy of a parent keymap whose shortcuts it
    /// inherits (its own changes are in `keymap_overrides` under its name).
    pub user_keymaps: Vec<UserKeymap>,
    /// Settings › Editor › TODO: the patterns the TODO tool window looks for in comments.
    pub todo_patterns: Vec<TodoPattern>,
    /// Settings › Editor › TODO: named filters, each showing only some patterns' items.
    pub todo_filters: Vec<TodoFilter>,
    /// Languages & Frameworks › Rust: ask crates.io for crate names and versions in Cargo.toml.
    pub crates_io_lookups: bool,
    /// Code › Code Completion › Enable Command Completion: typing `..` lists IDE actions.
    pub command_completion: bool,
    // ---- M15 track EA: analysis in the editor
    /// Settings › Editor › Inspections: analyze a file when it is saved (Ruff for Python, Clippy
    /// for Rust when `use_clippy` is on and rust-analyzer is not running, clang-tidy/clangd for
    /// C/C++) with the active inspection profile; findings show in the editor.
    pub analysis_on_save: bool,
    /// Languages & Frameworks › Python › Language server: `auto` (the project's virtualenv, then
    /// the managed basedpyright, then `PATH`), `basedpyright`, `pyright`, `pylsp`, `ty` or `none`.
    pub python_language_server: String,
    /// Languages & Frameworks › Python › Type checking (pyright/basedpyright
    /// `typeCheckingMode`): empty = the project's configuration, else `off`, `basic`,
    /// `standard`, `strict` (basedpyright also `recommended`, `all`).
    pub python_type_checking: String,
    // ---- M16 track LT: analysis while typing
    /// Settings › Editor › Inspections › Analyze while typing › Python: Ruff re-runs on the
    /// unsaved buffer (through stdin) shortly after the last edit.
    pub analysis_while_typing_python: bool,
    /// … › C/C++: clang-tidy re-runs on a temporary copy of the buffer with the file's compile
    /// command (clangd's diagnostics are live through the language server either way).
    pub analysis_while_typing_cpp: bool,
    /// … › SQL: CodeCraft's SQL inspections re-run in `.sql` files and database consoles.
    pub analysis_while_typing_sql: bool,
    /// … › Delay: how long after the last edit the analyzers start (ms).
    pub analysis_while_typing_delay_ms: u64,
}

/// Tab and indent of one language (Settings › Editor › Code Style).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageIndent {
    /// Columns a tab character takes.
    pub tab_size: usize,
    /// Columns one indent level takes (spaces, unless `use_tab`).
    pub indent_size: usize,
    /// Indent with tab characters.
    pub use_tab: bool,
}

impl Default for LanguageIndent {
    fn default() -> Self {
        Self { tab_size: 4, indent_size: 4, use_tab: false }
    }
}

/// A user keymap (Settings › Keymap › Duplicate): a name and the keymap it inherits from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserKeymap {
    pub name: String,
    /// A built-in keymap or another user keymap.
    pub parent: String,
}

/// A TODO pattern: a regular expression matched in comments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TodoPattern {
    pub pattern: String,
    pub case_sensitive: bool,
}

/// A TODO filter: which patterns' items the TODO tool window shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TodoFilter {
    pub name: String,
    /// Pattern texts (as in [`TodoPattern::pattern`]).
    pub patterns: Vec<String>,
}

/// RustRover's default TODO patterns.
pub fn default_todo_patterns() -> Vec<TodoPattern> {
    vec![
        TodoPattern { pattern: r"\btodo\b.*".into(), case_sensitive: false },
        TodoPattern { pattern: r"\bfixme\b.*".into(), case_sensitive: false },
    ]
}

/// Where View › Appearance › Navigation Bar shows the path of the active file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationBar {
    /// A bar above the editor.
    Top,
    /// The left part of the status bar (RustRover's New UI default).
    #[default]
    InStatusBar,
    Hidden,
}

/// Which usages Find Usages lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    #[default]
    Project,
    /// Only the file the search started in.
    CurrentFile,
}

/// Edit › Find Usages › Find Usages Settings….
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FindUsagesOptions {
    pub scope: UsageScope,
    /// Keep usages in test code (`tests/` directories, `*_test.*`/`test_*.*` files).
    pub include_tests: bool,
}

impl Default for FindUsagesOptions {
    fn default() -> Self {
        Self { scope: UsageScope::Project, include_tests: true }
    }
}

/// Which whitespace the editor paints while Show Whitespaces is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhitespaceMode {
    /// Every space and tab.
    #[default]
    All,
    /// Indentation and whitespace at line ends.
    LeadingTrailing,
    /// Whitespace at line ends only.
    Trailing,
}

/// A user colour scheme: a built-in scheme plus colour overrides by token name (`keyword`,
/// `editor_bg`, …) as `#RRGGBB`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorScheme {
    pub name: String,
    pub base: String,
    pub colors: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor_font_size: 13.0,
            ui_font_size: 13.0,
            line_spacing: 1.2,
            tab_size: 4,
            show_whitespace: false,
            whitespace_mode: WhitespaceMode::All,
            soft_wrap: false,
            find_scroll_to_results: true,
            show_line_numbers: true,
            show_inlay_hints: true,
            check_on_save: false,
            use_clippy: false,
            rust_analyzer_path: None,
            theme: "Dark".into(),
            lsp_memory_limit_mb: 0,
            rust_analyzer_low_memory: false,
            commit_ai_provider: "openrouter".into(),
            commit_ai_model: String::new(),
            ollama_url: "http://localhost:11434".into(),
            commit_ai_keys: BTreeMap::new(),
            keymap: String::new(),
            keymap_overrides: BTreeMap::new(),
            color_scheme: String::new(),
            user_schemes: Vec::new(),
            native_menu_bar: cfg!(target_os = "macos"),
            completion_auto_popup: true,
            completion_auto_popup_delay_ms: 100,
            completion_argument_placeholders: false,
            rename_in_place: true,
            reopen_last_project: true,
            hot_exit: true,
            generate_run_configs: true,
            power_save_mode: false,
            file_type_associations: BTreeMap::new(),
            right_margin: 100,
            show_editor_tabs: true,
            editor_tab_limit: 0,
            preview_tab: true,
            processes_auto_show: false,
            compact_mode: false,
            ide_zoom: 1.0,
            presentation_font_size: 24.0,
            show_toolbar: true,
            navigation_bar: NavigationBar::InStatusBar,
            navigation_bar_members: true,
            show_tool_window_bars: true,
            show_status_bar: true,
            status_bar_widgets: BTreeMap::new(),
            find_usages: FindUsagesOptions::default(),
            python_formatter: "auto".into(),
            python_reformat_on_save: false,
            python_optimize_imports_on_save: false,
            editor_font_family: String::new(),
            indent_by_language: BTreeMap::new(),
            user_keymaps: Vec::new(),
            todo_patterns: default_todo_patterns(),
            todo_filters: Vec::new(),
            crates_io_lookups: true,
            command_completion: false,
            analysis_on_save: true,
            python_language_server: "auto".into(),
            python_type_checking: String::new(),
            analysis_while_typing_python: true,
            analysis_while_typing_cpp: true,
            analysis_while_typing_sql: true,
            analysis_while_typing_delay_ms: 500,
        }
    }
}

/// Everything the IDE has open.
#[derive(Debug, Default)]
pub struct Workspace {
    pub project: Option<Project>,
    docs: BTreeMap<DocId, OpenDocument>,
    editors: BTreeMap<EditorId, Editor>,
    /// Editor groups (split editors) and their layout; read through [`Workspace::tabs`],
    /// [`Workspace::active_editor`] and the split accessors.
    groups: BTreeMap<GroupId, EditorGroup>,
    split: SplitNode,
    focused: GroupId,
    next_group: u64,
    /// Per editor: the document revision its view was last synced to and the char offset of
    /// its first visible line then (see `sync_views`).
    view_revs: BTreeMap<EditorId, (u64, usize)>,
    lsp_problems: BTreeMap<PathBuf, Vec<Problem>>,
    cargo_problems: BTreeMap<PathBuf, Vec<Problem>>,
    /// CodeCraft's own inspections (unresolved names in SQL, …): per inspection, per file.
    inspection_problems: BTreeMap<String, BTreeMap<PathBuf, Vec<Problem>>>,
    pub runs: Vec<RunTab>,
    pub active_run: Option<RunTabId>,
    pub find: Option<FindResults>,
    pub notifications: Vec<Notification>,
    pub tasks: BTreeMap<String, BackgroundTask>,
    pub layout: Layout,
    pub settings: Settings,
    pub recent_files: Vec<PathBuf>,
    pub recent_projects: Vec<PathBuf>,
    /// Per-file highlighting level (Configure Current File Analysis); files not listed: All.
    pub analysis_levels: BTreeMap<PathBuf, AnalysisLevel>,
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

    /// Show `doc` in a tab: reuse its editor in the focused group, else focus another group
    /// that shows it (like RustRover), else open a new tab after the active one in the focused
    /// group.
    pub fn open_editor(&mut self, doc: DocId) -> Option<EditorId> {
        if !self.docs.contains_key(&doc) {
            return None;
        }
        if let Some(id) = self.editor_for_doc(doc) {
            self.set_active_editor(id);
            return Some(id);
        }
        let id = self.new_editor(doc, None)?;
        self.insert_tab(self.focused, id);
        Some(id)
    }

    /// A new editor for `doc`, copying the view state (carets, scroll, folds) of `like`.
    fn new_editor(&mut self, doc: DocId, like: Option<EditorId>) -> Option<EditorId> {
        let rev = self.docs.get(&doc)?.doc.revision();
        self.next_editor += 1;
        let id = EditorId(self.next_editor);
        let editor = match like.and_then(|l| self.editors.get(&l)) {
            Some(l) => Editor { id, pinned: false, preview: false, reveal_caret: true, ..l.clone() },
            None => Editor::new(id, doc),
        };
        let top = self.docs.get(&doc).map(|d| top_offset(&d.doc, editor.scroll_line)).unwrap_or(0);
        self.editors.insert(id, editor);
        self.view_revs.insert(id, (rev, top));
        Some(id)
    }

    /// Put `id` in group `g` after its active tab, and make it active there.
    fn insert_tab(&mut self, g: GroupId, id: EditorId) {
        let group = self.groups.entry(g).or_default();
        let at = group.active.and_then(|a| group.tabs.iter().position(|t| *t == a)).map(|i| i + 1);
        match at {
            Some(i) if i <= group.tabs.len() => group.tabs.insert(i, id),
            _ => group.tabs.push(id),
        }
        group.active = Some(id);
    }

    pub fn editor(&self, id: EditorId) -> Option<&Editor> {
        self.editors.get(&id)
    }

    /// Whether `editor` soft-wraps: its own View › Active Editor › Soft-Wrap choice, else the
    /// `soft_wrap` setting.
    pub fn soft_wrap_of(&self, editor: EditorId) -> bool {
        self.editors.get(&editor).and_then(|e| e.soft_wrap).unwrap_or(self.settings.soft_wrap)
    }

    pub fn editor_mut(&mut self, id: EditorId) -> Option<&mut Editor> {
        self.editors.get_mut(&id)
    }

    /// Tabs of the focused group, in display order.
    pub fn tabs(&self) -> &[EditorId] {
        self.groups.get(&self.focused).map(|g| g.tabs.as_slice()).unwrap_or(&[])
    }

    /// The focused group's selected editor.
    pub fn active_editor(&self) -> Option<EditorId> {
        self.groups.get(&self.focused).and_then(|g| g.active)
    }

    /// Select an editor's tab and focus its group. False if no group shows it.
    pub fn set_active_editor(&mut self, id: EditorId) -> bool {
        let Some(g) = self.group_of(id) else { return false };
        self.focused = g;
        if let Some(group) = self.groups.get_mut(&g) {
            group.active = Some(id);
        }
        true
    }

    /// Every tab of every group, groups in display order.
    pub fn all_tabs(&self) -> Vec<EditorId> {
        self.split.leaves().iter().filter_map(|g| self.groups.get(g)).flat_map(|g| g.tabs.iter().copied()).collect()
    }

    pub fn active(&self) -> Option<(&Editor, &OpenDocument)> {
        let e = self.editors.get(&self.active_editor()?)?;
        let d = self.docs.get(&e.doc)?;
        Some((e, d))
    }

    /// The active editor and its document, mutably.
    pub fn active_mut(&mut self) -> Option<(&mut Editor, &mut OpenDocument)> {
        let id = self.active_editor()?;
        let e = self.editors.get_mut(&id)?;
        let d = self.docs.get_mut(&e.doc)?;
        Some((e, d))
    }

    /// Close a tab. Returns the closed document if no other editor shows it (so the caller can
    /// notify the language server). Activates the neighbouring tab like RustRover (the one to
    /// the left, or the new first tab); a split group that loses its last tab goes away and its
    /// neighbour takes the space (and the focus, if it had it).
    pub fn close_editor(&mut self, id: EditorId) -> Option<OpenDocument> {
        let editor = self.editors.remove(&id)?;
        self.view_revs.remove(&id);
        if let Some(g) = self.group_of(id) {
            self.detach_tab(g, id);
            if self.groups.get(&g).is_some_and(|gr| gr.tabs.is_empty()) {
                self.remove_group(g);
            }
        }
        self.drop_doc_if_unused(editor.doc)
    }

    /// Take `id` out of group `g`'s tabs, selecting the neighbour if it was selected.
    fn detach_tab(&mut self, g: GroupId, id: EditorId) {
        let Some(group) = self.groups.get_mut(&g) else { return };
        let idx = group.tabs.iter().position(|t| *t == id);
        group.tabs.retain(|t| *t != id);
        if group.active == Some(id) {
            group.active = match idx {
                Some(i) if i > 0 => group.tabs.get(i - 1).copied(),
                _ => group.tabs.first().copied(),
            };
        }
    }

    /// Drop an (empty) split group; the last group always stays.
    fn remove_group(&mut self, g: GroupId) {
        let order = self.split.leaves();
        if order.len() < 2 || !self.split.remove_leaf(g) {
            return;
        }
        self.groups.remove(&g);
        if self.focused == g {
            let idx = order.iter().position(|x| *x == g).unwrap_or(0);
            let left = self.split.leaves();
            self.focused = left.get(idx.saturating_sub(1)).or_else(|| left.first()).copied().unwrap_or_default();
        }
    }

    /// An editor showing `doc`: the focused group's, else the first in display order.
    pub fn editor_for_doc(&self, doc: DocId) -> Option<EditorId> {
        self.editors_for_doc(doc).into_iter().next()
    }

    /// Every editor showing `doc`, the focused group's first.
    pub fn editors_for_doc(&self, doc: DocId) -> Vec<EditorId> {
        let mut order = self.split.leaves();
        order.retain(|g| *g != self.focused);
        order.insert(0, self.focused);
        order
            .iter()
            .filter_map(|g| self.groups.get(g))
            .flat_map(|g| g.tabs.iter().copied())
            .filter(|id| self.editors.get(id).is_some_and(|e| e.doc == doc))
            .collect()
    }

    // ------------------------------------------------------------ editor groups (splits)

    pub fn focused_group(&self) -> GroupId {
        self.focused
    }

    /// Focus a group (its selected tab becomes the active editor). False if there is no such group.
    pub fn focus_group(&mut self, g: GroupId) -> bool {
        if !self.split.contains(g) {
            return false;
        }
        self.focused = g;
        true
    }

    pub fn group(&self, g: GroupId) -> Option<&EditorGroup> {
        self.groups.get(&g)
    }

    /// Groups in display order (left to right, top to bottom).
    pub fn group_ids(&self) -> Vec<GroupId> {
        self.split.leaves()
    }

    pub fn group_of(&self, id: EditorId) -> Option<GroupId> {
        self.split.leaves().into_iter().find(|g| self.groups.get(g).is_some_and(|gr| gr.tabs.contains(&id)))
    }

    pub fn split_layout(&self) -> &SplitNode {
        &self.split
    }

    pub fn is_split(&self) -> bool {
        self.split.is_split()
    }

    /// Split Right / Split Down (and … and Move): a new group next to the focused one showing
    /// its active document. With `move_tab` the tab moves there (when the group has others);
    /// otherwise the new group gets a second view of the document. Focuses the new group.
    pub fn split_editor(&mut self, axis: SplitAxis, move_tab: bool) -> Option<GroupId> {
        let active = self.active_editor()?;
        let doc = self.editors.get(&active)?.doc;
        let from = self.focused;
        self.next_group += 1;
        let g = GroupId(self.next_group);
        if !self.split.split_leaf(from, g, axis) {
            return None;
        }
        let alone = self.groups.get(&from).is_none_or(|gr| gr.tabs.len() < 2);
        let id = if move_tab && !alone {
            self.detach_tab(from, active);
            active
        } else {
            self.new_editor(doc, Some(active))?
        };
        self.groups.insert(g, EditorGroup { tabs: vec![id], active: Some(id) });
        self.focused = g;
        Some(g)
    }

    /// Unsplit: the focused group absorbs its sibling(s) under the nearest splitter. Tabs whose
    /// document it already shows are closed; the others move over with their view state.
    pub fn unsplit(&mut self) -> bool {
        let SplitNode::Split { .. } = &self.split else { return false };
        let siblings = sibling_leaves(&self.split, self.focused);
        if siblings.is_empty() {
            return false;
        }
        self.merge_into_focused(&siblings);
        true
    }

    /// Unsplit All: every group merges into the focused one.
    pub fn unsplit_all(&mut self) -> bool {
        let others: Vec<GroupId> = self.split.leaves().into_iter().filter(|g| *g != self.focused).collect();
        if others.is_empty() {
            return false;
        }
        self.merge_into_focused(&others);
        true
    }

    fn merge_into_focused(&mut self, from: &[GroupId]) {
        let target = self.focused;
        for g in from {
            let Some(group) = self.groups.remove(g) else {
                self.split.remove_leaf(*g);
                continue;
            };
            for id in group.tabs {
                let doc = self.editors.get(&id).map(|e| e.doc);
                let shown = self
                    .groups
                    .get(&target)
                    .is_some_and(|t| t.tabs.iter().any(|t| self.editors.get(t).map(|e| e.doc) == doc && doc.is_some()));
                if shown {
                    self.editors.remove(&id);
                    self.view_revs.remove(&id);
                } else {
                    let t = self.groups.entry(target).or_default();
                    t.tabs.push(id);
                    if t.active.is_none() {
                        t.active = Some(id);
                    }
                }
            }
            self.split.remove_leaf(*g);
        }
    }

    /// Go to Next/Previous Splitter: focus the group `delta` steps away in display order (wrapping).
    pub fn cycle_group(&mut self, delta: isize) -> bool {
        let order = self.split.leaves();
        if order.len() < 2 {
            return false;
        }
        let idx = order.iter().position(|g| *g == self.focused).unwrap_or(0);
        let next = (idx as isize + delta).rem_euclid(order.len() as isize) as usize;
        if let Some(g) = order.get(next) {
            self.focused = *g;
        }
        true
    }

    /// Change Splitter Orientation of the splitter around the focused group.
    pub fn flip_split(&mut self) -> bool {
        self.split.flip_parent(self.focused)
    }

    /// Maximize Editor / Normalize Splits: give the focused group (almost) all the space, or,
    /// when it already has it, make every split even again. Returns true when maximized.
    pub fn toggle_maximize(&mut self) -> bool {
        if self.split.is_maximized(self.focused) {
            self.split.normalize();
            false
        } else {
            self.split.maximize(self.focused);
            true
        }
    }

    /// Stretch Editor to …: move the focused group's edge on that side by `step` (a share of
    /// the splitter). False when no splitter is on that side.
    pub fn stretch_editor(&mut self, side: Stretch, step: f32) -> bool {
        self.split.stretch(self.focused, side, step)
    }

    /// Drag a splitter (pre-order index, see [`SplitNode::splitter_count`]).
    pub fn set_split_ratio(&mut self, index: usize, ratio: f32) -> bool {
        self.split.set_ratio(index, ratio)
    }

    /// Move a tab to another group at `index` (drag and drop between tab bars). If the target
    /// already shows the document, that tab is selected instead and the dragged one closed.
    /// An emptied source group goes away. Focuses the target.
    pub fn move_tab(&mut self, id: EditorId, target: GroupId, index: usize) -> bool {
        let Some(from) = self.group_of(id) else { return false };
        if !self.split.contains(target) {
            return false;
        }
        let doc = self.editors.get(&id).map(|e| e.doc);
        if from == target {
            let Some(group) = self.groups.get_mut(&target) else { return false };
            group.tabs.retain(|t| *t != id);
            let at = index.min(group.tabs.len());
            group.tabs.insert(at, id);
            group.active = Some(id);
            self.focused = target;
            return true;
        }
        let existing = self
            .groups
            .get(&target)
            .and_then(|t| t.tabs.iter().copied().find(|t| self.editors.get(t).map(|e| e.doc) == doc && doc.is_some()));
        self.detach_tab(from, id);
        match existing {
            Some(other) => {
                self.editors.remove(&id);
                self.view_revs.remove(&id);
                if let Some(t) = self.groups.get_mut(&target) {
                    t.active = Some(other);
                }
            }
            None => {
                let t = self.groups.entry(target).or_default();
                let at = index.min(t.tabs.len());
                t.tabs.insert(at, id);
                t.active = Some(id);
            }
        }
        self.focused = target;
        if self.groups.get(&from).is_none_or(|g| g.tabs.is_empty()) {
            self.remove_group(from);
            self.focused = target;
        }
        true
    }

    /// Keep views of edited documents consistent: an editor that did not make an edit (any view
    /// of the document other than [`editor_for_doc`](Self::editor_for_doc), which edit commands
    /// use) has its carets and scroll position mapped through the document's changes. Call after
    /// running commands; cheap when nothing changed.
    pub fn sync_views(&mut self) {
        let ids: Vec<EditorId> = self.editors.keys().copied().collect();
        for id in ids {
            let Some(e) = self.editors.get(&id) else { continue };
            let Some(od) = self.docs.get(&e.doc) else { continue };
            let rev = od.doc.revision();
            let seen = self.view_revs.get(&id).copied();
            if let Some((r, _)) = seen
                && r == rev
            {
                let top = top_offset(&od.doc, e.scroll_line);
                self.view_revs.insert(id, (rev, top));
                continue;
            }
            let primary = self.editor_for_doc(e.doc) == Some(id);
            let mapped = match seen {
                Some((seen, top)) if !primary => {
                    od.doc.changes_since(seen).map(|sets| map_view(e, &od.doc, &sets, top))
                }
                _ => None,
            };
            let len = od.doc.len_chars();
            let Some(e) = self.editors.get_mut(&id) else { continue };
            if let Some((sels, scroll)) = mapped {
                e.selections = sels;
                e.scroll_line = scroll;
            }
            e.selections = e.selections.clamp(len);
            let top = self.docs.get(&e.doc).map(|d| top_offset(&d.doc, e.scroll_line)).unwrap_or(0);
            self.view_revs.insert(id, (rev, top));
        }
    }

    // ------------------------------------------------------------ problems

    pub fn set_lsp_problems(&mut self, path: PathBuf, problems: Vec<Problem>) {
        if problems.is_empty() {
            self.lsp_problems.remove(&path);
        } else {
            self.lsp_problems.insert(path, problems);
        }
    }

    /// Replace the problems one of CodeCraft's own inspections found in `path`.
    pub fn set_inspection_problems(&mut self, inspection: &str, path: PathBuf, problems: Vec<Problem>) {
        let files = self.inspection_problems.entry(inspection.to_string()).or_default();
        if problems.is_empty() {
            files.remove(&path);
        } else {
            files.insert(path, problems);
        }
    }

    fn inspection_problems_for(&self, path: &Path) -> Vec<&Problem> {
        self.inspection_problems.values().filter_map(|files| files.get(path)).flatten().collect()
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
            .chain(self.inspection_problems_for(path))
            .collect();
        v.sort_by_key(|p| (p.start, p.severity));
        v
    }

    /// The highlighting level of a file (Configure Current File Analysis).
    pub fn analysis_level(&self, path: &Path) -> AnalysisLevel {
        self.analysis_levels.get(path).copied().unwrap_or_default()
    }

    /// The problems the editor highlights in a file: [`Workspace::problems_for`] filtered by the
    /// file's analysis level.
    pub fn highlighted_problems_for(&self, path: &Path) -> Vec<&Problem> {
        let level = self.analysis_level(path);
        self.problems_for(path).into_iter().filter(|p| level.shows(p)).collect()
    }

    /// Every problem, grouped by file (sorted by path).
    pub fn all_problems(&self) -> BTreeMap<&Path, Vec<&Problem>> {
        let mut out: BTreeMap<&Path, Vec<&Problem>> = BTreeMap::new();
        let inspections = self.inspection_problems.values().flat_map(|files| files.iter());
        for (path, list) in self.lsp_problems.iter().chain(self.cargo_problems.iter()).chain(inspections) {
            out.entry(path.as_path()).or_default().extend(list.iter());
        }
        for list in out.values_mut() {
            list.sort_by_key(|p| (p.severity, p.start));
        }
        out
    }

    pub fn problem_counts(&self) -> (usize, usize) {
        let inspections = self.inspection_problems.values().flat_map(|files| files.values());
        let all = self.lsp_problems.values().chain(self.cargo_problems.values()).chain(inspections).flatten();
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

/// Groups under the sibling of the focused group's leaf (what Unsplit merges).
fn sibling_leaves(tree: &SplitNode, g: GroupId) -> Vec<GroupId> {
    match tree {
        SplitNode::Group(_) => Vec::new(),
        SplitNode::Split { first, second, .. } => {
            if **first == SplitNode::Group(g) {
                second.leaves()
            } else if **second == SplitNode::Group(g) {
                first.leaves()
            } else if first.contains(g) {
                sibling_leaves(first, g)
            } else {
                sibling_leaves(second, g)
            }
        }
    }
}

/// Char offset of the first visible line for a scroll position.
fn top_offset(doc: &Document, scroll_line: f32) -> usize {
    let line = if scroll_line.is_finite() && scroll_line > 0.0 { scroll_line as usize } else { 0 };
    doc.line_start(line)
}

/// An editor's selections and scroll line mapped through changes made elsewhere.
fn map_view(e: &Editor, doc: &Document, sets: &[&codecraft_text::ChangeSet], top: usize) -> (SelectionSet, f32) {
    let mut sels = e.selections.clone();
    let mut top = top;
    for cs in sets {
        sels = sels.map(cs);
        top = cs.map_pos(top, codecraft_text::Assoc::Before);
    }
    let frac = if e.scroll_line.is_finite() { e.scroll_line.fract().max(0.0) } else { 0.0 };
    (sels.clamp(doc.len_chars()), doc.char_to_line(top) as f32 + frac)
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
        assert_eq!(ws.tabs(), &[ea, eb]);
        assert_eq!(ws.open_editor(a), Some(ea));
        assert_eq!(ws.active_editor(), Some(ea));
        let closed = ws.close_editor(ea).unwrap();
        assert_eq!(closed.path.as_deref(), Some(Path::new("/p/a.rs")));
        assert_eq!(ws.active_editor(), Some(eb));
        assert!(ws.doc(a).is_none());
        assert_eq!(ws.recent_files.first().map(PathBuf::as_path), Some(Path::new("/p/b.rs")));
    }

    fn two_docs() -> (Workspace, DocId, DocId, EditorId, EditorId) {
        let mut ws = Workspace::new();
        let a = ws.add_document(Some("/p/a.rs".into()), "fn a() {}\nlet x = 1;\n");
        let b = ws.add_document(Some("/p/b.rs".into()), "fn b() {}");
        let ea = ws.open_editor(a).unwrap();
        let eb = ws.open_editor(b).unwrap();
        (ws, a, b, ea, eb)
    }

    #[test]
    fn split_right_shows_a_second_view_and_close_removes_the_group() {
        let (mut ws, _a, b, ea, eb) = two_docs();
        let g0 = ws.focused_group();
        ws.set_selections(eb, SelectionSet::caret(3));
        let g1 = ws.split_editor(SplitAxis::Horizontal, false).unwrap();
        assert_eq!(ws.group_ids(), vec![g0, g1]);
        assert_eq!(ws.focused_group(), g1);
        let view = ws.active_editor().unwrap();
        assert_ne!(view, eb);
        assert_eq!(ws.editor(view).unwrap().doc, b);
        assert_eq!(ws.editor(view).unwrap().selections, SelectionSet::caret(3));
        assert_eq!(ws.editors_for_doc(b), vec![view, eb]);
        assert_eq!(ws.all_tabs(), vec![ea, eb, view]);
        // Closing one view keeps the document; closing the split's last tab unsplits.
        assert!(ws.close_editor(view).is_none());
        assert!(!ws.is_split());
        assert_eq!(ws.focused_group(), g0);
        assert_eq!(ws.active_editor(), Some(eb));
        assert!(ws.doc(b).is_some());
    }

    #[test]
    fn split_and_move_moves_the_tab_unless_it_is_alone() {
        let (mut ws, _, _, ea, eb) = two_docs();
        let g0 = ws.focused_group();
        let g1 = ws.split_editor(SplitAxis::Vertical, true).unwrap();
        assert_eq!(ws.group(g0).unwrap().tabs, vec![ea]);
        assert_eq!(ws.group(g0).unwrap().active, Some(ea));
        assert_eq!(ws.group(g1).unwrap().tabs, vec![eb]);
        // Alone in its group: Split and Move copies instead of leaving an empty group.
        let g2 = ws.split_editor(SplitAxis::Horizontal, true).unwrap();
        assert_eq!(ws.group(g1).unwrap().tabs, vec![eb]);
        assert_eq!(ws.group(g2).unwrap().tabs.len(), 1);
        assert!(matches!(ws.split_layout(), SplitNode::Split { axis: SplitAxis::Vertical, .. }));
    }

    #[test]
    fn opening_a_file_shown_in_another_group_focuses_it() {
        let (mut ws, a, b, ea, eb) = two_docs();
        let g0 = ws.focused_group();
        ws.split_editor(SplitAxis::Horizontal, true).unwrap();
        assert_eq!(ws.open_editor(a), Some(ea));
        assert_eq!(ws.focused_group(), g0);
        assert_eq!(ws.editor_for_doc(b), Some(eb));
        let c = ws.add_document(Some("/p/c.rs".into()), "");
        let ec = ws.open_editor(c).unwrap();
        assert_eq!(ws.group_of(ec), Some(g0));
        assert!(ws.set_active_editor(eb));
        assert_ne!(ws.focused_group(), g0);
        assert!(!ws.set_active_editor(EditorId(999)));
    }

    #[test]
    fn unsplit_merges_siblings_into_the_focused_group() {
        let (mut ws, _, b, ea, eb) = two_docs();
        let g0 = ws.focused_group();
        let g1 = ws.split_editor(SplitAxis::Horizontal, false).unwrap();
        let view = ws.active_editor().unwrap();
        let c = ws.add_document(Some("/p/c.rs".into()), "");
        let ec = ws.open_editor(c).unwrap();
        assert_eq!(ws.group_of(ec), Some(g1));
        let g2 = ws.split_editor(SplitAxis::Vertical, false).unwrap();
        assert_eq!(ws.group_ids(), vec![g0, g1, g2]);
        // From g0, the sibling subtree (g1, g2) merges in: the second view of b closes.
        assert!(ws.focus_group(g0));
        assert!(ws.unsplit());
        assert_eq!(ws.group_ids(), vec![g0]);
        assert_eq!(ws.tabs(), &[ea, eb, ec]);
        assert!(ws.editor(view).is_none());
        assert_eq!(ws.editors_for_doc(b), vec![eb]);
        assert!(!ws.unsplit());
        assert!(!ws.unsplit_all());
    }

    #[test]
    fn unsplit_all_cycle_flip_and_maximize() {
        let (mut ws, ..) = two_docs();
        let g0 = ws.focused_group();
        let g1 = ws.split_editor(SplitAxis::Horizontal, true).unwrap();
        let g2 = ws.split_editor(SplitAxis::Horizontal, false).unwrap();
        assert!(ws.cycle_group(1));
        assert_eq!(ws.focused_group(), g0);
        assert!(ws.cycle_group(-1));
        assert_eq!(ws.focused_group(), g2);
        assert!(ws.flip_split());
        assert!(ws.toggle_maximize());
        assert!(!ws.toggle_maximize());
        assert!(ws.stretch_editor(Stretch::Top, 0.1));
        assert!(ws.set_split_ratio(0, 0.3));
        assert!(ws.focus_group(g1));
        assert!(ws.unsplit_all());
        assert_eq!(ws.group_ids(), vec![g1]);
        assert_eq!(ws.all_tabs().len(), 2);
        assert!(!ws.cycle_group(1));
    }

    #[test]
    fn move_tab_between_groups() {
        let (mut ws, _, _, ea, eb) = two_docs();
        let g0 = ws.focused_group();
        let g1 = ws.split_editor(SplitAxis::Horizontal, true).unwrap();
        assert!(ws.move_tab(ea, g1, 0));
        // g0 emptied and went away.
        assert_eq!(ws.group_ids(), vec![g1]);
        assert_eq!(ws.tabs(), &[ea, eb]);
        assert!(ws.move_tab(ea, g1, 5));
        assert_eq!(ws.tabs(), &[eb, ea]);
        assert!(!ws.move_tab(ea, g0, 0));
        // Dropping a view on a group that already shows its document selects that tab.
        let g2 = ws.split_editor(SplitAxis::Horizontal, false).unwrap();
        let view = ws.active_editor().unwrap();
        assert!(ws.move_tab(view, g1, 0));
        assert!(ws.editor(view).is_none());
        assert_eq!(ws.active_editor(), Some(ea));
        assert!(!ws.group_ids().contains(&g2));
    }

    #[test]
    fn other_views_follow_edits_made_in_one_view() {
        let (mut ws, a, _, ea, _) = two_docs();
        assert!(ws.set_active_editor(ea));
        ws.split_editor(SplitAxis::Horizontal, false).unwrap();
        let view = ws.active_editor().unwrap();
        // The original view looks at line 1 with its caret on `x`.
        ws.set_selections(ea, SelectionSet::caret(14));
        ws.editor_mut(ea).unwrap().scroll_line = 1.0;
        ws.sync_views();
        // Typing two lines at the top of the focused view (the editor edit commands use).
        assert_eq!(ws.editor_for_doc(a), Some(view));
        let od = ws.doc_mut(a).unwrap();
        let edit = codecraft_text::Edit {
            changes: codecraft_text::ChangeSet::single(0, 0, "// 1\n// 2\n"),
            selections: SelectionSet::caret(10),
        };
        od.doc.apply(&edit, &SelectionSet::caret(0), codecraft_text::EditGroup::Standalone).unwrap();
        ws.set_selections(view, SelectionSet::caret(10));
        ws.sync_views();
        assert_eq!(ws.editor(view).unwrap().selections, SelectionSet::caret(10));
        assert_eq!(ws.editor(ea).unwrap().selections, SelectionSet::caret(24));
        assert_eq!(ws.editor(ea).unwrap().scroll_line, 3.0);
        // Nothing changes on a second sync.
        ws.sync_views();
        assert_eq!(ws.editor(ea).unwrap().selections, SelectionSet::caret(24));
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
        // Inspection problems join in, per inspection.
        let warning = Problem { source: ProblemSource::Inspection, ..p(Severity::Warning, 2) };
        ws.set_inspection_problems("sql", "/x.rs".into(), vec![warning]);
        assert_eq!(ws.problem_counts(), (1, 1));
        assert_eq!(ws.problems_for(Path::new("/x.rs")).len(), 2);
        assert_eq!(ws.all_problems().get(Path::new("/x.rs")).map(Vec::len), Some(2));
        ws.set_inspection_problems("sql", "/x.rs".into(), Vec::new());
        assert_eq!(ws.problem_counts(), (1, 0));
    }
}
