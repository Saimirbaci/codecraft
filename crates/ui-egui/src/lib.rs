//! CodeCraft's egui shell (L3). Thin: every action is an engine command; this crate paints the
//! workspace and turns input into commands.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod agent_access_ui;
mod agent_panel;
mod appearance_ui;
mod approvals_ui;
mod bookmarks_panel;
mod chrome;
mod cmake_ui;
mod data_grid;
mod database_data_ui;
mod database_dialogs;
mod database_editor_ui;
mod database_panel;
mod database_schema_ui;
mod database_settings_ui;
mod dataframe_ui;
mod debug_dbg_ui;
mod debug_panel;
mod debug_views_ui;
mod diff_view;
mod docker_ui;
mod ed_ui;
mod editor;
mod er_diagram;
mod feedback;
mod fonts;
mod history_ui;
mod hot_exit_ui;
mod http_ui;
mod inspect_ui;
mod inspections_ui;
pub mod keymap;
mod markdown;
mod menus;
mod merge_editor;
mod misc_ui;
mod native_menu;
mod navbar;
mod notebook_ui;
mod panels;
mod popups;
mod profiler_ui;
mod project_windows;
mod pull_requests_ui;
mod python_settings_ui;
mod python_ui;
mod rc_ui;
mod redis_ui;
mod refactor_ui;
mod remote_ui;
mod rust_ui;
mod services_panel;
mod settings_pages;
mod settings_ui;
mod state;
mod terminal_panel;
mod testing_panel;
mod valgrind_ui;
pub mod theme;
mod vcs_changes_ui;
mod vcs_merge_ui;
mod vcs_panels;
mod windows;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, Pos2, Rect};
use serde_json::{Value, json};

use codecraft_engine::{Context, Effect, Popup, Session};
use codecraft_workspace::{DocId, EditorId, NotificationLevel};

pub use editor::editor_id;
pub use state::PersistedState;

/// `(doc, revision, caret)` → the bracket at the caret and its partner.
type BracketCache = Option<((DocId, u64, usize), Option<(usize, usize)>)>;
/// Query → Go to File results `(path, relative path, match indices)`.
type GotoFileCache = Option<(String, Vec<(PathBuf, String, Vec<usize>)>)>;

/// View state that is not part of the engine (layout caches, focus, transient UI).
pub struct UiState {
    pub theme: theme::Theme,
    pub bindings: Vec<keymap::Binding>,
    pub page_lines: usize,
    pub caret_screen: Option<Pos2>,
    pub editor_rect: Option<Rect>,
    pub editor_line_h: f32,
    pub focus_editor: bool,
    pub drag_anchor_active: bool,
    pub bracket_cache: BracketCache,
    pub run_lines_cache: Option<((DocId, u64), Vec<usize>)>,
    pub goto_file_cache: GotoFileCache,
    pub last_popup_query: String,
    pub popup_focus_pending: bool,
    pub find_bar_was_open: bool,
    pub console_follow: bool,
    pub last_shift_tap: Option<Instant>,
    pub shift_clean: bool,
    pub mono_font_name: String,
    pub error: Option<String>,
    pub last_autosave: Instant,
    /// Feedback overlay view state (tool, ink, gesture, capture).
    pub feedback: feedback::FeedbackUi,
    /// Agent tool window and approval cards view state.
    pub agent: agent_panel::AgentUi,
    /// Database tool window, result grids and data source dialog view state.
    pub db: database_panel::DbUi,
    /// Debug tool window view state (input fields, pane sizes).
    pub debug: debug_panel::DebugUi,
    /// Terminal tool window view state (selection drag, wheel, blink, focus).
    pub terminal: terminal_panel::TerminalUi,
    /// Git tool windows, branch popup and dialogs view state.
    pub vcs: vcs_panels::VcsUi,
    /// Refactoring preview, file dialogs and Hierarchy view state.
    pub refactor: refactor_ui::RefactorUi,
    /// Settings dialog, keymap editor, Quick Switch and Edit Configurations view state.
    pub prefs: settings_ui::PrefsUi,
    /// Last size of each floating window, by key (see `windows`); persisted.
    pub window_sizes: std::collections::BTreeMap<String, [f32; 2]>,
    /// Project window requests from this window's effects, handled by the app after the frame.
    pub(crate) window_requests: Vec<project_windows::Request>,
    /// A picked project waiting for This Window / New Window.
    pub(crate) open_choice: Option<PathBuf>,
    /// The window's full-screen state last reported by the platform (changes made outside
    /// CodeCraft, like the green button, are copied into the session).
    pub(crate) viewport_full_screen: Option<bool>,
}

impl UiState {
    fn new() -> Self {
        Self {
            theme: theme::Theme::dark(),
            bindings: keymap::bindings(),
            page_lines: 40,
            caret_screen: None,
            editor_rect: None,
            editor_line_h: 18.0,
            focus_editor: false,
            drag_anchor_active: false,
            bracket_cache: None,
            run_lines_cache: None,
            goto_file_cache: None,
            last_popup_query: String::new(),
            popup_focus_pending: false,
            find_bar_was_open: false,
            console_follow: true,
            last_shift_tap: None,
            shift_clean: false,
            mono_font_name: String::new(),
            error: None,
            last_autosave: Instant::now(),
            feedback: feedback::FeedbackUi::default(),
            agent: agent_panel::AgentUi::default(),
            db: database_panel::DbUi::default(),
            debug: debug_panel::DebugUi::default(),
            terminal: terminal_panel::TerminalUi::default(),
            vcs: vcs_panels::VcsUi::default(),
            refactor: refactor_ui::RefactorUi::default(),
            prefs: settings_ui::PrefsUi::default(),
            window_sizes: Default::default(),
            window_requests: Vec::new(),
            open_choice: None,
            viewport_full_screen: None,
        }
    }
}

/// Params a command run from a key or a menu click needs from the platform: Paste as Plain Text
/// pastes the system clipboard.
pub(crate) fn platform_params(id: &str) -> Value {
    #[cfg(target_os = "macos")]
    if codecraft_engine::registry().canonical(id) == "edit.paste.pasteAsPlainText"
        && let Some(text) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok())
    {
        return json!({ "text": text.replace("\r\n", "\n") });
    }
    let _ = id;
    Value::Null
}

/// Run a command, reporting failures in the status bar (never panics).
pub(crate) fn run_command(s: &mut Session, st: &mut UiState, id: &str, params: &Value) {
    match s.execute(id, params) {
        Ok(_) => st.error = None,
        Err(codecraft_engine::CommandError::Disabled(reason)) => s.status = reason,
        Err(e) => {
            log::warn!("{id}: {e}");
            s.status = e.to_string();
        }
    }
}

pub struct App {
    pub session: Session,
    pub ui: UiState,
    persisted: PersistedState,
    /// Save settings/layout on exit (off for tests and snapshots).
    persist: bool,
    fonts_installed: bool,
    /// The macOS global menu bar (inactive in tests and on other platforms).
    native_menu: native_menu::NativeMenu,
    /// Projects opened in their own windows (each with its own session).
    windows: project_windows::ProjectWindows,
    /// When the persisted state was last checked for a periodic save, and what was saved.
    state_saved: (Instant, String),
}

/// How the desktop app starts.
#[derive(Clone, Debug, Default)]
pub struct Launch {
    /// A path from the command line (file or directory); otherwise the last project(s).
    pub open: Option<PathBuf>,
    /// `--no-restore`: reopen no project and no editors (sessions are still saved).
    pub no_restore: bool,
}

/// How often the persisted state is saved while it changes (so a crash loses little).
const STATE_AUTOSAVE_EVERY: Duration = Duration::from_secs(30);

impl App {
    /// `open`: a path from the command line (file or directory); otherwise the last project.
    pub fn new(open: Option<PathBuf>) -> Self {
        let mut session = Session::new();
        session.session_store.enable();
        session.hot_exit.enabled = true;
        session.run_store.generate.auto = true;
        // Cargo.toml completion and the newer-version inspection ask crates.io (M12 track RS),
        // unless Settings › Languages & Frameworks › Rust › Crates.io lookups is off.
        session.prefs_services.network_lookups = true;
        Self::build(open, PersistedState::load(), true, true, session)
    }

    /// An app with default state that is never saved and never starts rust-analyzer
    /// (tests, offscreen snapshots).
    pub fn ephemeral(open: Option<PathBuf>) -> Self {
        let mut app = Self::build(None, PersistedState::default(), false, false, Session::new());
        app.session.lsp_enabled = false;
        // Settings › Editor › Font never scans the machine's fonts here (tests use fixture dirs
        // through `Session::isolate_home`).
        app.session.prefs_services.fonts.dirs_override = Some(Vec::new());
        if let Some(path) = open {
            run_command(&mut app.session, &mut app.ui, "file.open", &json!({ "path": path }));
        }
        app
    }

    /// Project windows open, the main one included.
    pub fn project_window_count(&self) -> usize {
        self.windows.count()
    }

    /// The session of the `index`th project window opened beside the main one.
    pub fn project_window_session(&self, index: usize) -> Option<&Session> {
        self.windows.session(index)
    }

    /// An app that starts from `persisted` like the desktop app (reopening the last project and
    /// windows with their editors) and saves project sessions (`fallback_dir` for read-only
    /// projects), but never writes the app's own state file and never starts language servers
    /// (tests).
    pub fn restoring(persisted: PersistedState, fallback_dir: PathBuf) -> Self {
        let mut session = Session::new();
        session.lsp_enabled = false;
        session.session_store.enable();
        // Hot exit too, with read-only projects' buffers beside the snapshots.
        session.hot_exit.enabled = true;
        session.hot_exit.fallback_dir = Some(fallback_dir.join("unsaved"));
        session.session_store.fallback_dir = Some(fallback_dir);
        session.prefs_services.fonts.dirs_override = Some(Vec::new());
        Self::build(None, persisted, false, true, session)
    }

    /// Save every project window's session and return the app state as it would be persisted
    /// now (the main window's project and settings, the other windows' projects).
    pub fn capture_state(&mut self) -> PersistedState {
        if let Err(e) = codecraft_engine::session_store::save_now(&mut self.session) {
            log::warn!("cannot save the session: {e}");
        }
        self.windows.save_sessions();
        let mut p = self.persisted.clone();
        p.capture(&self.session, &self.ui);
        p.open_windows = self.windows.project_roots();
        p
    }

    /// `persist`: save the app state file; `reopen`: open the last project and windows (when
    /// the `reopen_last_project` setting allows it).
    fn build(open: Option<PathBuf>, persisted: PersistedState, persist: bool, reopen: bool, session: Session) -> Self {
        let mut session = session;
        persisted.apply(&mut session);
        // crates.io lookups follow their setting where the host allows the network.
        let network = session.prefs_services.network_lookups;
        session.allow_network_lookups(network);
        let mut ui = UiState::new();
        ui.window_sizes = persisted.window_sizes.clone();
        let mut app = Self {
            session,
            ui,
            persisted,
            persist,
            fonts_installed: false,
            native_menu: native_menu::NativeMenu::default(),
            windows: project_windows::ProjectWindows::default(),
            state_saved: (Instant::now(), String::new()),
        };
        let reopen = reopen && app.session.ws.settings.reopen_last_project;
        let explicit = open.is_some();
        let target = open.or_else(|| app.persisted.last_project.clone().filter(|p| reopen && p.is_dir()));
        if let Some(path) = target {
            run_command(&mut app.session, &mut app.ui, "file.open", &json!({ "path": path }));
        }
        // Untitled buffers kept by hot exit while no project was open come back too.
        if reopen && app.session.ws.project.is_none() {
            codecraft_engine::hot_exit::restore_on_open(&mut app.session);
        }
        // The other project windows open at the last exit come back too (not when a path was
        // given on the command line).
        if reopen && !explicit {
            let main = app.session.ws.project.as_ref().map(|p| p.root.clone());
            for path in app.persisted.windows_to_reopen(main.as_deref()) {
                app.windows.open(path, &app.session, &app.persisted);
            }
        }
        app
    }

    /// Copy the project windows' positions and sizes into the persisted state.
    fn remember_window_rects(&mut self) {
        for (root, rect) in self.windows.rects() {
            self.persisted.remember_window_rect(&root, rect);
        }
    }

    /// Save the app state every [`STATE_AUTOSAVE_EVERY`] when it changed.
    fn autosave_state(&mut self) {
        if !self.persist || self.state_saved.0.elapsed() < STATE_AUTOSAVE_EVERY {
            return;
        }
        let mut p = self.persisted.clone();
        p.capture(&self.session, &self.ui);
        p.open_windows = self.windows.project_roots();
        self.state_saved.0 = Instant::now();
        let Ok(json) = serde_json::to_string(&p) else { return };
        if json != self.state_saved.1 {
            p.save();
            self.state_saved.1 = json;
        }
    }

    fn editor_focused(ctx: &egui::Context, s: &Session) -> Option<EditorId> {
        let id = s.ws.active_editor()?;
        ctx.memory(|m| m.has_focus(editor_id(id))).then_some(id)
    }

    /// Keyboard routing: popups first, then shortcuts by context, then text input to the editor.
    fn handle_input(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
        let events = ctx.input(|i| i.events.clone());
        let editor_focused = Self::editor_focused(ctx, s).is_some();
        let text_field_focused = !editor_focused && ctx.memory(|m| m.focused().is_some());
        // The feedback overlay takes all input while it is shown.
        if feedback::intercept_input(ctx, s, st) {
            return;
        }
        // Settings, Edit Configurations and Quick Switch are modal (and record shortcuts).
        if settings_ui::wants_keyboard(s) {
            settings_ui::handle_keys(ctx, &events, s, st);
            return;
        }

        // Double Shift → Search Everywhere.
        for ev in &events {
            match ev {
                Event::Key { pressed: true, .. } | Event::Text(_) | Event::PointerButton { .. } => {
                    st.shift_clean = false;
                    st.last_shift_tap = None;
                }
                Event::ModifiersChanged(m) => {
                    if m.shift && !m.ctrl && !m.alt && !m.mac_cmd {
                        st.shift_clean = true;
                    } else if !m.shift && st.shift_clean {
                        let now = Instant::now();
                        if st.last_shift_tap.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400)) {
                            st.last_shift_tap = None;
                            if let Some(id) = keymap::double_shift_command() {
                                run_command(s, st, id, &Value::Null);
                            }
                        } else {
                            st.last_shift_tap = Some(now);
                        }
                        st.shift_clean = false;
                    }
                }
                _ => {}
            }
        }

        // The navigation bar, template choosers and the save-template dialog.
        if navbar::wants_keyboard(s) {
            navbar::handle_keys(ctx, s, st);
            return;
        }

        // Bookmark, scratch and Local History dialogs read their keys themselves.
        if bookmarks_panel::wants_keyboard(s)
            || history_ui::wants_keyboard(s)
            || misc_ui::wants_keyboard(s)
            || remote_ui::wants_keyboard(s)
            || appearance_ui::wants_keyboard(s)
            || python_ui::wants_keyboard(s)
            || database_editor_ui::wants_keyboard(s)
            || ed_ui::wants_keyboard(s)
            || rc_ui::wants_keyboard(s)
            || rc_ui::gutter_menu_open(ctx)
        {
            return;
        }

        // A shortcut bound to a saved macro plays it.
        if !text_field_focused && ed_ui::macro_shortcut(ctx, s, st) {
            return;
        }

        // Popups with their own keyboard handling.
        if popups::wants_keyboard(s) {
            popups::handle_keys(ctx, s, st);
            if !matches!(
                s.popup,
                Some(Popup::Completion(_)) | Some(Popup::Hover { .. }) | Some(Popup::ParameterInfo { .. })
            ) {
                return;
            }
        }

        // The focused terminal takes every key and text event itself (it runs IDE-global Cmd
        // shortcuts on its own; see `terminal_panel::handle_keys`).
        if terminal_panel::has_focus(ctx) {
            terminal_panel::handle_keys(ctx, &events, s, st);
            return;
        }

        let completion_open = matches!(&s.popup, Some(Popup::Completion(c)) if !c.visible.is_empty());
        let mut consumed_text = false;
        for ev in &events {
            let Event::Key { key, pressed: true, modifiers, .. } = ev else { continue };
            // Completion: Tab accepts.
            if editor_focused && completion_open && *key == Key::Tab && modifiers.is_none() {
                ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
                run_command(s, st, "editor.completion.accept", &Value::Null);
                continue;
            }
            // An inline proposal (ghost text): Tab, Alt+Right and Cmd+Right insert it, Escape
            // dismisses it.
            if editor_focused
                && !completion_open
                && s.ws.active_editor().and_then(|id| codecraft_engine::inline_completion::visible(s, id)).is_some()
            {
                let inline = match key {
                    Key::Tab if modifiers.is_none() => Some("code.codeCompletion.insertInlineProposal"),
                    Key::ArrowRight if modifiers.alt && !modifiers.shift => Some("code.codeCompletion.insertInlineProposalsWord"),
                    Key::ArrowRight if modifiers.command && !modifiers.shift => Some("code.codeCompletion.insertInlineProposalsLine"),
                    Key::Escape => Some("editor.inlineCompletion.dismiss"),
                    _ => None,
                };
                if let Some(id) = inline {
                    ctx.input_mut(|i| i.consume_key(*modifiers, *key));
                    run_command(s, st, id, &Value::Null);
                    continue;
                }
            }
            // Two-stroke shortcuts (`Cmd+K Cmd+C`): a first stroke waits for the second.
            if settings_ui::chord_key(ctx, s, st, *key, modifiers, editor_focused, text_field_focused) {
                consumed_text = true;
                continue;
            }
            let found = st
                .bindings
                .iter()
                .filter(|b| b.shortcut.matches(*key, modifiers))
                .find(|b| match b.context {
                    Context::Editor => editor_focused,
                    Context::Global => {
                        if text_field_focused {
                            b.shortcut.has_command_modifier() && !is_text_editing_command(b.id) || is_function_key(*key)
                        } else {
                            true
                        }
                    }
                })
                .map(|b| (b.id, b.shortcut));
            if let Some((id, sc)) = found {
                ctx.input_mut(|i| i.consume_key(sc.modifiers(), sc.key));
                if sc.has_command_modifier() {
                    consumed_text = true;
                }
                run_command(s, st, id, &platform_params(id));
            }
        }
        if !editor_focused {
            return;
        }
        for ev in &events {
            match ev {
                Event::Text(t) if !consumed_text => {
                    run_command(s, st, "editor.type", &json!({ "text": t }));
                }
                Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() && t != "\n" && t != "\r" && !consumed_text => {
                    run_command(s, st, "editor.type", &json!({ "text": t }));
                }
                Event::Copy => run_command(s, st, "edit.copy", &Value::Null),
                Event::Cut => run_command(s, st, "edit.cut", &Value::Null),
                Event::Paste(t) => run_command(s, st, "edit.paste.paste", &json!({ "text": t })),
                _ => {}
            }
        }
    }

    fn apply_effects(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
        let effects = std::mem::take(&mut s.effects);
        for e in effects {
            match e {
                Effect::CopyToClipboard(t) => ctx.copy_text(t),
                Effect::PickOpenPath => {
                    let start = s.project_root();
                    let mut dialog = rfd::FileDialog::new().set_title("Open File or Project");
                    if let Some(dir) = start {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(p) = dialog.pick_folder() {
                        if s.ws.project.is_some() {
                            // RustRover asks: This Window or New Window (`project_windows::open_choice`).
                            st.open_choice = Some(p);
                        } else {
                            run_command(s, st, "file.open", &json!({ "path": p }));
                        }
                    }
                }
                Effect::PickNewProjectPath => {
                    if let Some(p) = rfd::FileDialog::new().set_title("New Cargo Project Location").save_file() {
                        run_command(s, st, "file.new.project", &json!({ "path": p }));
                    }
                }
                // Quit closes the main window (and with it every project window).
                Effect::Quit => ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close),
                Effect::FocusEditor => st.focus_editor = true,
                Effect::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
                Effect::OpenProjectWindow(p) => st.window_requests.push(project_windows::Request::Open(p)),
                Effect::SwitchProjectWindow(d) => st.window_requests.push(project_windows::Request::Switch(d)),
                Effect::SetFullScreen(on) => ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on)),
                Effect::PickFile { title, command, param, save, extensions, default_name } => {
                    let mut dialog = rfd::FileDialog::new().set_title(title);
                    if let Some(dir) = s.project_root() {
                        dialog = dialog.set_directory(dir);
                    }
                    if !extensions.is_empty() {
                        dialog = dialog.add_filter(extensions.join(", "), &extensions);
                    }
                    if let Some(name) = default_name {
                        dialog = dialog.set_file_name(name);
                    }
                    let picked = if save { dialog.save_file() } else { dialog.pick_file() };
                    if let Some(p) = picked {
                        let mut params = serde_json::Map::new();
                        params.insert(param, json!(p));
                        run_command(s, st, &command, &Value::Object(params));
                    }
                }
            }
        }
    }

    pub(crate) fn window_title(s: &Session) -> String {
        let project = s.ws.project.as_ref().map(|p| p.name.clone());
        let file = s.ws.active().map(|(_, d)| d.title());
        match (project, file) {
            (Some(p), Some(f)) => format!("{p} – {f}"),
            (Some(p), None) => p,
            (None, Some(f)) => f,
            (None, None) => "CodeCraft".into(),
        }
    }

    /// One project window's per-frame logic: settings, focus, engine pump, input, effects, title.
    pub(crate) fn window_logic(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
        // Theme, colour scheme, UI font size and keymap follow the settings.
        settings_ui::sync(ctx, s, st);
        appearance_ui::sync(ctx, s, st);
        // Window focus: RustRover saves on deactivation and synchronizes on activation.
        let focus_changes: Vec<bool> = ctx.input(|i| {
            i.events.iter().filter_map(|e| if let Event::WindowFocused(f) = e { Some(*f) } else { None }).collect()
        });
        for focused in focus_changes {
            if focused {
                run_command(s, st, "file.synchronize", &Value::Null);
            } else {
                let _ = s.save_all();
            }
        }
        if s.pump() {
            ctx.request_repaint();
        }
        s.page_lines = st.page_lines;
        Self::handle_input(ctx, s, st);
        Self::apply_effects(ctx, s, st);
        if s.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(Self::window_title(s)));
    }

    /// Paint one project window. `menu_bar`: show the in-window menu bar (no native one).
    pub(crate) fn paint(
        ui: &mut egui::Ui,
        s: &mut Session,
        st: &mut UiState,
        persisted: &PersistedState,
        menu_bar: bool,
    ) {
        // View › Appearance decides which bars show.
        let bars = codecraft_engine::appearance::chrome(s);
        if menu_bar {
            chrome::menu_bar(ui, s, st);
        }
        if bars.toolbar {
            chrome::toolbar(ui, s, st);
        }
        if bars.status_bar {
            chrome::status_bar(ui, s, st);
        }
        if s.ws.project.is_none() && s.ws.tabs().is_empty() {
            chrome::welcome(ui, s, st, persisted);
        } else {
            if bars.stripes {
                panels::stripes(ui, s, st);
            }
            panels::side_panels(ui, s, st);
            if bars.navigation_top {
                chrome::navigation_bar_top(ui, s, st);
            }
            chrome::editor_area(ui, s, st);
        }
        popups::show(ui, s, st);
        project_windows::open_choice(ui, s, st);
        settings_ui::show(ui, s, st);
        navbar::show_popups(ui, s, st);
        approvals_ui::show(ui, s, st);
        agent_panel::connect_window(ui, s, st);
        testing_panel::windows(ui, s, st);
        valgrind_ui::windows(ui, s, st);
        debug_panel::attach_window(ui, s, st);
        debug_dbg_ui::windows(ui, s, st);
        bookmarks_panel::windows(ui, s, st);
        history_ui::windows(ui, s, st);
        inspections_ui::windows(ui, s, st);
        misc_ui::windows(ui, s, st);
        remote_ui::windows(ui, s, st);
        python_ui::windows(ui, s, st);
        dataframe_ui::windows(ui, s, st);
        profiler_ui::windows(ui, s, st);
        http_ui::windows(ui, s, st);
        database_editor_ui::windows(ui, s, st);
        rust_ui::windows(ui, s, st);
        appearance_ui::windows(ui, s, st);
        database_data_ui::windows(ui, s, st);
        ed_ui::windows(ui, s, st);
        rc_ui::windows(ui, s, st);
        redis_ui::windows(ui, s, st);
        refactor_ui::overlays(ui, s, st);
        feedback::overlay(ui, s, st);
        windows::capture(ui.ctx(), st);
        if st.focus_editor
            && let Some(id) = s.ws.active_editor()
        {
            ui.memory_mut(|m| m.request_focus(editor_id(id)));
            st.focus_editor = false;
        }
        Self::apply_effects(ui.ctx(), s, st);
        // Errors surfaced as notifications pop into the status bar.
        if let Some(n) = s.ws.notifications.last()
            && n.level == NotificationLevel::Error
            && s.status.is_empty()
        {
            s.status = format!("{}: {}", n.title, n.message);
        }
    }
}

fn is_text_editing_command(id: &str) -> bool {
    // Safe Delete is Cmd+Backspace, which text fields use to delete to the line start.
    matches!(
        id,
        "edit.copy"
            | "edit.cut"
            | "edit.paste.paste"
            | "edit.selectAll"
            | "edit.undo"
            | "edit.redo"
            | "edit.delete"
            | "refactor.safeDelete"
    )
}

fn is_function_key(k: Key) -> bool {
    matches!(
        k,
        Key::F1
            | Key::F2
            | Key::F3
            | Key::F4
            | Key::F5
            | Key::F6
            | Key::F7
            | Key::F8
            | Key::F9
            | Key::F10
            | Key::F11
            | Key::F12
    )
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.fonts_installed {
            let (defs, name) = fonts::install(ctx);
            self.ui.mono_font_name = name;
            self.ui.prefs.base_fonts = Some(defs);
            self.ui.theme.apply(ctx);
            self.fonts_installed = true;
        }
        Self::window_logic(ctx, &mut self.session, &mut self.ui);
        // Quitting with open database transactions asks Commit / Rollback / Cancel first.
        if ctx.input(|i| i.viewport().close_requested())
            && codecraft_engine::database::tx::quit_blocked(&mut self.session)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.autosave_state();
        // The native menu shows the focused window's enabled states.
        let focused = self.windows.focused(ctx).and_then(|i| self.windows.session(i));
        self.native_menu.update(ctx, focused.unwrap_or(&self.session));
        // Remember the main window size and position (eframe's own persistence is not enabled).
        let (inner, outer, minimized, full) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.outer_rect, v.minimized.unwrap_or(false), v.fullscreen.unwrap_or(false))
        });
        if let Some(r) = inner {
            let size = [r.width(), r.height()];
            if self.persisted.window_size != Some(size) && !minimized && !full {
                self.persisted.window_size = Some(size);
            }
        }
        if let Some(r) = outer {
            let pos = [r.min.x, r.min.y];
            if self.persisted.window_pos != Some(pos) && !minimized && !full {
                self.persisted.window_pos = Some(pos);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let menu_bar = !self.native_menu.active();
        Self::paint(ui, &mut self.session, &mut self.ui, &self.persisted, menu_bar);
        // Project windows, then what any window's effects asked of the app.
        let ctx = ui.ctx().clone();
        let mut requests: Vec<_> = self.ui.window_requests.drain(..).map(|r| (None, r)).collect();
        self.windows.show(&ctx, &self.persisted, &self.native_menu, &mut requests);
        for (from, request) in requests {
            match request {
                project_windows::Request::Open(path) => {
                    self.windows.open(path, &self.session, &self.persisted);
                    ctx.request_repaint();
                }
                project_windows::Request::Switch(delta) => self.windows.switch(&ctx, from, delta),
            }
        }
        self.windows.sync_settings(&mut self.session);
        codecraft_engine::app_cmds::set_project_window_count(self.windows.count());
        self.remember_window_rects();
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Native menu activations go to the focused window (project windows take theirs in
        // their own frame).
        if raw_input.viewport_id != egui::ViewportId::ROOT {
            return;
        }
        let fired = self.native_menu.take_fired();
        if fired.is_empty() {
            return;
        }
        if self.windows.focused(ctx).is_some() {
            self.windows.defer_native(fired);
            return;
        }
        let held = native_menu::held_modifiers(ctx, &raw_input.events);
        self.native_menu.deliver(fired, ctx, &mut self.session, &mut self.ui, &held, &mut raw_input.events);
    }

    fn on_exit(&mut self) {
        let open_windows = self.windows.project_roots();
        self.remember_window_rects();
        self.windows.shutdown();
        // Hot exit keeps unsaved buffers for the next start (no prompt); off, they are saved.
        codecraft_engine::hot_exit::quit_save(&mut self.session);
        if self.persist {
            self.persisted.capture(&self.session, &self.ui);
            self.persisted.open_windows = open_windows;
            self.persisted.save();
        }
        self.session.shutdown();
    }
}

/// Launch the desktop app.
pub fn run(open: Option<PathBuf>) -> eframe::Result {
    launch(Launch { open, no_restore: false })
}

/// Launch the desktop app with command-line options.
pub fn launch(launch: Launch) -> eframe::Result {
    let Launch { open, no_restore } = launch;
    let persisted = PersistedState::load();
    let size = persisted.saved_window_size().unwrap_or([1400.0, 900.0]);
    let mut viewport =
        egui::ViewportBuilder::default().with_title("CodeCraft").with_inner_size(size).with_min_inner_size([640.0, 400.0]);
    if let Some(pos) = persisted.saved_window_pos() {
        viewport = viewport.with_position(pos);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "CodeCraft",
        options,
        Box::new(move |_cc| {
            let mut session = Session::new();
            session.session_store.autosave = true;
            session.hot_exit.enabled = true;
            session.run_store.generate.auto = true;
            session.session_store.restore_on_open = !no_restore;
            session.prefs_services.network_lookups = true;
            let mut app = App::build(open, persisted, true, !no_restore, session);
            app.native_menu.allowed = true;
            Ok(Box::new(app))
        }),
    )
}
