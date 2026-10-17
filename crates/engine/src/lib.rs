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
// M14 track AX: the agent control plane (MCP tools over the engine, access policy).
pub mod agent_access;
pub mod agent_mcp;
pub mod agent_tools;
mod agent_tools_cmds;
// M15 track AI: Review jobs' tool calls in their worktree.
mod agent_worktree;
pub mod agent_sessions;
pub mod appearance;
mod bookmark_cmds;
pub mod bookmarks;
pub mod agents;
pub mod app_cmds;
pub mod approvals;
pub mod catalog;
pub mod before_launch;
pub mod command_completion;
pub mod modify_code;
pub mod typescript_tool;
pub mod zig_tool;
pub mod cargo_audit;
pub mod cargo_deps;
mod code_cmds;
pub mod commands;
pub mod completion;
pub mod commit_ai;
mod convert;
pub mod cpp;
pub mod database;
mod database_cmds;
pub mod debug;
mod debug_cmds;
mod debug_agent_cmds;
pub mod debug_views;
pub mod asm;
mod decorations;
pub mod deployment;
pub mod docker;
mod docker_cmds;
pub mod docker_target;
mod edit_cmds;
pub mod editing;
mod editing_cmds;
mod editor_cmds;
pub mod feedback;
mod feedback_cmds;
mod file_cmds;
pub mod github_prs;
mod ide_cmds;
pub mod ide_server;
pub mod http_client;
mod http_cmds;
mod inspection_cmds;
pub mod inspections;
pub mod inline_rename;
mod jobs;
pub mod jupyter;
pub mod keymaps;
mod lang_cmds;
pub mod local_history;
pub mod macro_expand;
mod local_history_cmds;
mod lsp;
pub mod misc;
mod misc_cmds;
mod navigate_cmds;
pub mod notebook;
mod notebook_cmds;
pub mod notebook_kernel;
pub mod dataframe;
mod params;
pub mod refactor;
mod refactor_cmds;
pub mod remote;
mod remote_cmds;
pub mod run_targets;
mod render_cmds;
mod reformat;
pub mod popup;
pub mod profiler;
mod profiler_agent_cmds;
mod profiler_cmds;
pub mod profiler_convert;
pub mod profiler_plan;
pub mod profiler_report;
pub mod python;
mod python_cmds;
pub mod python_console;
pub mod python_env;
pub mod python_format;
pub mod python_output;
mod python_lint;
pub mod python_project;
mod run_cmds;
pub mod run_config_gen;
mod run_config_cmds;
pub mod run_config_store;
pub mod run_configs;
mod runs;
mod rust_cmds;
pub mod rust_repl;
pub mod rust_tools;
pub mod scratch;
pub mod services;
pub mod session_store;
pub mod settings;
mod settings_cmds;
pub mod prefs_cmds;
pub mod snippet;
pub mod paste;
mod split_cmds;
pub mod sql_lang;
mod sql_lang_cmds;
pub mod analysis;
pub mod copyright;
pub mod data_flow;
pub mod duplicates;
pub mod file_templates;
pub mod hot_exit;
pub mod inline_completion;
pub mod layouts;
pub mod macros;
pub mod markdown_export;
pub mod tasks_contexts;
pub mod presentation;
pub mod ssr;
pub mod ssr_cmds;
pub mod user_store;
pub mod ssh_target;
// M16 track RT: run, debug, test and profile on SSH hosts and Docker containers.
pub mod target_exec;
mod target_cmds;
pub mod target_python;
pub mod ssh_tunnel;
pub mod structure;
pub mod templates;
pub mod templates_django;
pub mod todo;
pub mod terminal;
pub mod terminal_shell;
mod terminal_cmds;
pub mod testing;
mod testing_cmds;
pub mod test_file;
pub mod vcs;
mod vcs_cmds;
mod vcs_ext_cmds;
pub mod vcs_merge;
pub mod usages;
mod folding;
mod tool_window_cmds;
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
    /// Open a path in a new project window (its own session).
    OpenProjectWindow(PathBuf),
    /// Focus the next (+1) or previous (-1) project window.
    SwitchProjectWindow(i32),
    /// Enter (true) or leave (false) full screen (View › Appearance).
    SetFullScreen(bool),
    /// Ask for a file to open (or, with `save`, a file to write), then run `command` with the
    /// chosen path as param `param`. `extensions` filter the dialog (`["json"]`).
    PickFile { title: String, command: String, param: String, save: bool, extensions: Vec<String>, default_name: Option<String> },
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
    /// What agents connected over MCP may do (M14 track AX).
    pub agent_access: agent_access::AgentAccess,
    /// Engine tool calls from agents that are waiting (M14 track AX).
    pub agent_tools: agent_tools::AgentToolsState,
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
    /// Live templates, choosers, navigation bar, column mode, soft-wrap geometry (M3b).
    pub editing: editing::EditingState,
    /// Completion bookkeeping and the in-place rename (M10 track B).
    pub completion: completion::CompletionState,
    /// Line bookmarks (track X).
    pub bookmarks: bookmarks::BookmarksState,
    /// The TODO tool window's scan (track X).
    pub todo: todo::TodoState,
    /// Local History snapshots and window (track X).
    pub local_history: local_history::LocalHistoryState,
    /// Scratch files (track X).
    pub scratch: scratch::ScratchState,
    /// Inspect Code results, Code Cleanup and stack traces (M10 track D).
    pub inspections: inspections::InspectionsState,
    /// Settings dialog draft and Quick Switch Scheme (M8).
    pub prefs: settings::PrefsState,
    /// Font listing and loading, the Code Style preview, per-project analysis levels (M13 ST).
    pub prefs_services: settings::PrefsServices,
    /// User run configurations and the Edit Configurations dialog (M8).
    pub run_store: run_config_store::RunConfigStore,
    /// Menu sweep state: preview tabs, Compare With, processes, encodings (M10 track F).
    pub misc: misc::MiscState,
    /// Session save/restore switches (M10 track A; off unless the app turns it on).
    pub session_store: session_store::SessionStore,
    /// Hot exit: unsaved buffers kept across restarts (M13 track SR; off unless the app turns it on).
    pub hot_exit: hot_exit::HotExitState,
    /// View › Appearance modes, Zoom IDE popup, Memory Indicator (M10 track G).
    pub appearance: appearance::AppearanceState,
    /// Edit › Paste › Paste from History… ring (M10 track G).
    pub clip_history: paste::ClipHistory,
    /// Highlighted usages, recent Find Usages and the Find Usages Settings dialog (M10 track G).
    pub usages: usages::UsagesState,
    /// Docker in Services, Docker run targets, Dev Containers (M11 track D).
    pub docker: docker::DockerState,
    /// SSH servers, deployment, Remote Host, tunnels, Remote Development (M11 track S).
    pub remote: remote::RemoteState,
    /// HTTP Client requests, environments and responses (M11 track H).
    pub http: http_client::HttpState,
    /// Python interpreters, packages and console (M11 track Y).
    pub python: python::PythonState,
    /// Profiler runs and snapshots (M11 track P).
    pub profiler: profiler::ProfilerState,
    /// SQL in Rust strings and SQL editing (M11 track Q).
    pub sql_lang: sql_lang::SqlLangState,
    /// The Services tool window's selected section.
    pub services: services::ServicesState,
    /// Macro expansion, Cargo.toml dependencies, the Rust REPL, advisories (M12 track RS).
    pub rust: rust_tools::RustState,
    /// Jupyter notebooks, their kernels and View as DataFrame (M12 track PY).
    pub jupyter: jupyter::JupyterState,
    /// Ruff/Black/isort runs for Python files (M12 track PY).
    pub py_format: python_format::FormatState,
    /// Edit › Find › Search/Replace Structurally (M12 track ED).
    pub ssr: ssr_cmds::SsrState,
    /// Edit › Macros (M12 track ED).
    pub macros: macros::MacroState,
    /// View › Appearance › Presentation Assistant (M12 track ED).
    pub presentation: presentation::PresentationState,
    /// Code › Analyze Code popups (M12 track ED).
    pub analysis: analysis::AnalysisState,
    /// Code › Analyze Code › Locate Duplicates (M12 track ED).
    pub duplicates: duplicates::DuplicatesState,
    /// Code › Analyze Code › Data Flow to/from Here (M12 track ED).
    pub data_flow: data_flow::DataFlowState,
    /// Window › Layouts (M12 track ED).
    pub layouts: layouts::LayoutsState,
    /// File › Save File as Template / New from Template (M12 track ED).
    pub file_templates: file_templates::FileTemplatesState,
    /// Code › Update Copyright (M12 track ED).
    pub copyright: copyright::CopyrightState,
    /// Tools › Tasks & Contexts (M12 track ED).
    pub tasks: tasks_contexts::TasksState,
    /// Code › Code Completion › Call Inline Completion (M12 track ED).
    pub inline: inline_completion::InlineState,
    /// CLion features: CMake, C++ tests, Valgrind, sanitizers, Conan/vcpkg (M12 track CL).
    pub cpp: cpp::CppState,
    /// A Run/Debug waiting for its configuration's before-launch tasks (M13 track RC).
    pub launch: before_launch::LaunchState,
    /// Run targets: the `target` param of a command and remote interpreters (M16 track RT).
    pub targets: target_exec::TargetsState,
    /// The Zig tool window's build steps (M13 track RC).
    pub zig: zig_tool::ZigState,
    /// The TypeScript tool window: tsc errors and tsconfig files (M13 track RC).
    pub typescript: typescript_tool::TsState,
    /// Code › Modify Selected Code… (M13 track RC).
    pub modify: modify_code::ModifyState,
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
    /// The directory [`Session::isolate_home`] was given (sessions this one opens for agents'
    /// worktrees are isolated the same way; M15 AI).
    pub(crate) isolated_home: Option<std::path::PathBuf>,
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
            agent_access: agent_access::AgentAccess::default(),
            agent_tools: agent_tools::AgentToolsState::default(),
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
            editing: editing::EditingState::default(),
            completion: completion::CompletionState::default(),
            bookmarks: bookmarks::BookmarksState::default(),
            todo: todo::TodoState::default(),
            local_history: local_history::LocalHistoryState::default(),
            scratch: scratch::ScratchState::default(),
            inspections: inspections::InspectionsState::default(),
            prefs: settings::PrefsState::default(),
            prefs_services: settings::PrefsServices::default(),
            run_store: run_config_store::RunConfigStore::default(),
            misc: misc::MiscState::default(),
            session_store: session_store::SessionStore::default(),
            hot_exit: hot_exit::HotExitState::default(),
            appearance: appearance::AppearanceState::default(),
            clip_history: paste::ClipHistory::default(),
            usages: usages::UsagesState::default(),
            docker: docker::DockerState::default(),
            remote: remote::RemoteState::default(),
            http: http_client::HttpState::default(),
            python: python::PythonState::default(),
            profiler: profiler::ProfilerState::default(),
            sql_lang: sql_lang::SqlLangState::default(),
            services: services::ServicesState::default(),
            rust: rust_tools::RustState::default(),
            jupyter: jupyter::JupyterState::default(),
            py_format: python_format::FormatState::default(),
            ssr: ssr_cmds::SsrState::default(),
            macros: macros::MacroState::default(),
            presentation: presentation::PresentationState::default(),
            analysis: analysis::AnalysisState::default(),
            duplicates: duplicates::DuplicatesState::default(),
            data_flow: data_flow::DataFlowState::default(),
            layouts: layouts::LayoutsState::default(),
            file_templates: file_templates::FileTemplatesState::default(),
            copyright: copyright::CopyrightState::default(),
            tasks: tasks_contexts::TasksState::default(),
            inline: inline_completion::InlineState::default(),
            cpp: cpp::CppState::default(),
            launch: before_launch::LaunchState::default(),
            targets: target_exec::TargetsState::default(),
            zig: zig_tool::ZigState::default(),
            typescript: typescript_tool::TsState::default(),
            modify: modify_code::ModifyState::default(),
            selection_stack: Vec::new(),
            last_edit: None,
            servers: Default::default(),
            server_status: Default::default(),
            server_memory_mb: Default::default(),
            server_memory: Default::default(),
            decor: Default::default(),
            runs: runs::Runs::default(),
            jobs: jobs::Jobs::default(),
            isolated_home: None,
        }
    }

    /// Run a command by id. Never panics: a panic inside a command is caught and reported.
    pub fn execute(&mut self, id: &str, params: &Value) -> Result<Value, CommandError> {
        // Run, debug, test and profile commands run on the target their `target` param names (M16 RT).
        if let Some(target) = target_exec::scope(self, id, params)? {
            return target_exec::with_target(self, target, |s| s.execute_scoped(id, params));
        }
        self.execute_scoped(id, params)
    }

    fn execute_scoped(&mut self, id: &str, params: &Value) -> Result<Value, CommandError> {
        // Debug commands act on the session their `session` param names (M15 track DM).
        if let Some(session) = debug::session_scope(self, id, params)? {
            return debug::with_session(self, session, |s| s.execute_here(id, params));
        }
        self.execute_here(id, params)
    }

    fn execute_here(&mut self, id: &str, params: &Value) -> Result<Value, CommandError> {
        let reg = registry();
        let Some(spec) = reg.get(id) else {
            return Err(CommandError::Unknown(id.to_string()));
        };
        (spec.enabled)(self).map_err(CommandError::Disabled)?;
        // Closing connections with an open transaction asks Commit / Rollback / Cancel first.
        if let Some(r) = database::tx::intercept(self, spec.id, params) {
            return r;
        }
        let run = spec.run;
        // Only top-level commands are recorded in macros and announced (not the steps of a
        // command that runs others).
        let top_level = self.macros.depth == 0;
        self.macros.depth = self.macros.depth.saturating_add(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(self, params)));
        self.macros.depth = self.macros.depth.saturating_sub(1);
        // Other views of an edited document follow the edit (split editors).
        self.ws.sync_views();
        match result {
            Ok(r) => {
                if top_level && r.is_ok() {
                    macros::record(self, spec.id, params);
                    presentation::note(self, spec.id);
                }
                r
            }
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

    /// Keep every per-user file under `dir` instead of `$CODECRAFT_HOME` (default `~/.codecraft`):
    /// scratches, live templates, profiler snapshots, query history, SSH servers, remote caches
    /// and saved agent sessions (which this also turns on). For tests and embedders that must
    /// never touch the user's real files.
    pub fn isolate_home(&mut self, dir: &std::path::Path) {
        self.isolated_home = Some(dir.to_path_buf());
        self.agents.sessions.dir_override = Some(dir.join(agent_sessions::DIR_NAME));
        self.scratch.dir_override = Some(dir.join("scratches"));
        self.hot_exit.fallback_dir = Some(dir.join("unsaved"));
        self.terminal.data_dir = Some(dir.join("terminals"));
        // Review worktrees too, unless a test chose a directory.
        if self.agents.settings.worktrees_dir.is_none() {
            self.agents.settings.worktrees_dir = Some(dir.join("worktrees"));
        }
        self.editing.templates.set_path(dir.join("templates.json"));
        self.profiler.dir_override = Some(dir.join("profiler"));
        self.db.history.set_dir(dir.join("database").join("history"));
        self.remote.home_override = Some(dir.to_path_buf());
        self.remote.ssh_config_override = Some(dir.join("ssh_config"));
        self.rust.deps.cache_dir_override = Some(dir.join("cache").join("crates-index"));
        self.ssr.set_home(dir.to_path_buf());
        self.macros.set_home(dir.to_path_buf());
        self.layouts.set_home(dir.to_path_buf());
        self.file_templates.set_home(dir.to_path_buf());
        self.copyright.set_home(dir.to_path_buf());
        self.agent_access.path_override = Some(dir.join("agent-access.json"));
        // IDE inspection profiles and managed analyzers (M14 CA).
        self.analysis.set_home(dir.to_path_buf());
        // C/C++ tools (cmake, valgrind, conan, …) only from explicit overrides.
        self.cpp.isolated = true;
        // zig and tsc too (M13 track RC).
        self.zig.isolated = true;
        self.typescript.isolated = true;
        // Never the real GitHub CLI (it acts on the user's account): tests point it at a fake.
        self.vcs.gh_program = dir.join("gh-isolated");
        // Nor the user's Docker (QA M15: a UI test showing Services › Docker would have listed the
        // real daemon's containers); tests that want Docker point `docker.program` at a fake.
        self.docker.program = dir.join("docker-isolated").display().to_string();
        self.docker.devcontainer_program = dir.join("devcontainer-isolated").display().to_string();
        self.docker.config_dir = Some(dir.join("docker"));
        // Font scans look only in `dir/fonts`, never at the user's fonts.
        self.prefs_services.fonts.dirs_override = Some(vec![dir.join("fonts")]);
    }

    /// Drain background services. Returns true if anything changed (the UI should repaint).
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        changed |= self.pump_lsp();
        changed |= completion::pump(self);
        changed |= runs::pump(self);
        changed |= jobs::pump(self);
        changed |= self.approvals.pump();
        changed |= debug::pump(self);
        changed |= database::pump(self);
        changed |= agents::pump(self);
        changed |= ide_server::pump(self);
        changed |= agent_tools::pump(self);
        changed |= terminal::pump(self);
        changed |= vcs::pump(self);
        changed |= watch::pump(self);
        changed |= testing::pump(self);
        changed |= refactor::pump(self);
        changed |= bookmarks::pump(self);
        changed |= todo::pump(self);
        changed |= settings::pump(self);
        changed |= local_history::pump(self);
        changed |= inspections::pump(self);
        changed |= inspections::fixes::pump(self);
        changed |= misc::pump(self);
        changed |= appearance::pump(self);
        changed |= docker::pump(self);
        changed |= remote::pump(self);
        changed |= http_client::pump(self);
        changed |= python::pump(self);
        changed |= jupyter::pump(self);
        changed |= python_format::pump(self);
        changed |= profiler::pump(self);
        changed |= target_python::pump(self);
        changed |= sql_lang::pump(self);
        changed |= run_config_gen::pump(self);
        changed |= rust_tools::pump(self);
        changed |= ssr_cmds::pump(self);
        changed |= duplicates::pump(self);
        changed |= data_flow::pump(self);
        changed |= tasks_contexts::pump(self);
        changed |= inline_completion::pump(self);
        changed |= cpp::pump(self);
        changed |= before_launch::pump(self);
        changed |= zig_tool::pump(self);
        changed |= typescript_tool::pump(self);
        changed |= modify_code::pump(self);
        session_store::autosave(self);
        hot_exit::autosave(self);
        self.ws.sync_views();
        changed
    }

    /// Whether background work is in flight (the UI keeps repainting to poll).
    pub fn busy(&self) -> bool {
        !self.servers.is_empty()
            || self.runs.any_running()
            || self.jobs.any_running()
            || self.agents.any_active()
            || self.ide.busy()
            || self.agent_tools.busy()
            || self.db.busy()
            || self.debug.busy()
            || self.vcs.busy()
            || self.testing.busy()
            || self.inspections.busy()
            || self.docker.busy()
            || self.remote.busy()
            || self.http.busy()
            || self.python.busy()
            || self.jupyter.busy()
            || self.py_format.busy()
            || self.profiler.busy()
            || self.targets.python.busy()
            || self.sql_lang.busy()
            || self.rust.busy()
            || self.ssr.busy()
            || self.duplicates.busy()
            || self.data_flow.busy()
            || self.inline.busy()
            || self.cpp.busy()
            || self.launch.busy()
            || self.zig.busy()
            || self.typescript.busy()
            || self.modify.busy()
            // Analysis while typing waits for its delay, then runs (M16 LT).
            || self.analysis.editor.live.busy()
            || self.analysis.editor.on_save.busy()
    }

    /// Whether processes or jobs (not the language server) are still running.
    pub fn jobs_running(&self) -> bool {
        self.runs.any_running()
            || self.jobs.any_running()
            || self.db.busy()
            || self.debug.settling()
            || self.inspections.busy()
            || self.run_store.generate.busy()
            || self.ssr.busy()
            || self.duplicates.busy()
            || self.data_flow.busy()
            || self.cpp.busy()
            || self.launch.busy()
            || self.zig.busy()
            || self.typescript.busy()
            || self.analysis.editor.on_save.busy()
    }

    /// Stop the language server and running processes (window closing).
    pub fn shutdown(&mut self) {
        session_store::save_on_close(self);
        debug::shutdown(self);
        database::shutdown(self);
        agents::shutdown(self);
        agent_tools::shutdown(self);
        ide_server::shutdown(self);
        terminal::shutdown(self);
        vcs::shutdown(self);
        inspections::shutdown(self);
        remote::shutdown(self);
        docker::shutdown(self);
        python::shutdown(self);
        jupyter::shutdown(self);
        profiler::shutdown(self);
        rust_tools::shutdown(self);
        ssr_cmds::shutdown(self);
        duplicates::shutdown(self);
        data_flow::shutdown(self);
        cpp::shutdown(self);
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
            // Settings › Editor › Code Style per language; the file's own indentation wins.
            let indent = settings::code_style::indent(&self.ws.settings, d.language);
            cfg.tab_width = indent.tab_size;
            cfg.indent_unit = settings::code_style::indent_unit(&d.doc, indent);
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
            editing::after_edit(self, eid, &edit.changes);
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
                let (text, encoding, bom) = misc::encoding::read_document(&path).map_err(CommandError::Failed)?;
                let language = misc::language_for(&self.ws.settings, &path);
                let id = self.ws.add_document(Some(path.clone()), &text);
                if let Some(od) = self.ws.doc_mut(id) {
                    od.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    od.read_only = std::fs::metadata(&path).map(|m| m.permissions().readonly()).unwrap_or(false);
                    od.encoding = encoding;
                    od.bom = bom;
                    od.language = language;
                }
                self.lsp_did_open(id);
                analysis::on_save::after_open(self, &path);
                id
            }
        };
        let editor = self.ws.open_editor(doc).ok_or_else(|| CommandError::Failed("cannot open editor".into()))?;
        misc::after_open(self, editor);
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
        jupyter::before_save(self);
        local_history::before_save(self);
        // Restored unsaved changes whose file changed on disk meanwhile wait for the user.
        let blocked = hot_exit::blocked(&self.hot_exit);
        for od in self.ws.docs_mut() {
            let Some(path) = od.path.clone() else { continue };
            if !od.doc.is_modified() {
                continue;
            }
            if blocked.contains(&od.id) {
                errors.push(format!(
                    "{}: it changed on disk after its unsaved changes were kept; compare with disk, then keep or discard them",
                    path.display()
                ));
                continue;
            }
            match misc::encoding::write_document(&path, od) {
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
        local_history::after_save(self, &saved_paths);
        remote::after_save(self, &saved_paths);
        python_format::after_save(self, &saved_paths);
        // M15 EA: Ruff / Clippy / clang-tidy on the saved files, in the background.
        analysis::on_save::after_save(self, &saved_paths);
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
