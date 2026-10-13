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

mod agent_panel;
mod approvals_ui;
mod bookmarks_panel;
mod chrome;
mod data_grid;
mod database_panel;
mod debug_panel;
mod diff_view;
mod editor;
mod feedback;
mod fonts;
mod history_ui;
pub mod keymap;
mod markdown;
mod menus;
mod native_menu;
mod navbar;
mod panels;
mod popups;
mod project_windows;
mod refactor_ui;
mod settings_ui;
mod state;
mod terminal_panel;
mod testing_panel;
pub mod theme;
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
        }
    }
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
}

impl App {
    /// `open`: a path from the command line (file or directory); otherwise the last project.
    pub fn new(open: Option<PathBuf>) -> Self {
        Self::build(open, PersistedState::load(), true)
    }

    /// An app with default state that is never saved and never starts rust-analyzer
    /// (tests, offscreen snapshots).
    pub fn ephemeral(open: Option<PathBuf>) -> Self {
        let mut app = Self::build(None, PersistedState::default(), false);
        app.session.lsp_enabled = false;
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

    fn build(open: Option<PathBuf>, persisted: PersistedState, persist: bool) -> Self {
        let mut session = Session::new();
        persisted.apply(&mut session);
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
        };
        let target = open.or_else(|| app.persisted.last_project.clone().filter(|p| p.is_dir()));
        if let Some(path) = target {
            run_command(&mut app.session, &mut app.ui, "file.open", &json!({ "path": path }));
        }
        app
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
        if bookmarks_panel::wants_keyboard(s) || history_ui::wants_keyboard(s) {
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
                run_command(s, st, id, &Value::Null);
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
                Event::Paste(t) => run_command(s, st, "edit.paste", &json!({ "text": t })),
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
        if menu_bar {
            chrome::menu_bar(ui, s, st);
        }
        chrome::toolbar(ui, s, st);
        chrome::status_bar(ui, s, st);
        if s.ws.project.is_none() && s.ws.tabs().is_empty() {
            chrome::welcome(ui, s, st, persisted);
        } else {
            panels::stripes(ui, s, st);
            panels::side_panels(ui, s, st);
            chrome::editor_area(ui, s, st);
        }
        popups::show(ui, s, st);
        project_windows::open_choice(ui, s, st);
        settings_ui::show(ui, s, st);
        navbar::show_popups(ui, s, st);
        approvals_ui::show(ui, s, st);
        testing_panel::windows(ui, s, st);
        debug_panel::attach_window(ui, s, st);
        bookmarks_panel::windows(ui, s, st);
        history_ui::windows(ui, s, st);
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
            | "edit.paste"
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
            self.ui.mono_font_name = fonts::install(ctx);
            self.ui.theme.apply(ctx);
            self.fonts_installed = true;
        }
        Self::window_logic(ctx, &mut self.session, &mut self.ui);
        // The native menu shows the focused window's enabled states.
        let focused = self.windows.focused(ctx).and_then(|i| self.windows.session(i));
        self.native_menu.update(ctx, focused.unwrap_or(&self.session));
        // Remember the main window size (eframe's own persistence is not enabled).
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            let size = [r.width(), r.height()];
            if self.persisted.window_size != Some(size) && !ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
                self.persisted.window_size = Some(size);
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
        self.windows.shutdown();
        let _ = self.session.save_all();
        if self.persist {
            self.persisted.capture(&self.session, &self.ui);
            self.persisted.save();
        }
        self.session.shutdown();
    }
}

/// Launch the desktop app.
pub fn run(open: Option<PathBuf>) -> eframe::Result {
    let persisted = PersistedState::load();
    let size = persisted.saved_window_size().unwrap_or([1400.0, 900.0]);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CodeCraft")
            .with_inner_size(size)
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "CodeCraft",
        options,
        Box::new(move |_cc| {
            let mut app = App::build(open, persisted, true);
            app.native_menu.allowed = true;
            Ok(Box::new(app))
        }),
    )
}
