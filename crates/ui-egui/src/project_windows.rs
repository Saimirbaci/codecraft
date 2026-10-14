//! Project windows: a project opened "in a new window" gets its own `Session` and view state, shown
//! in its own native window (an egui immediate viewport) beside the main window. Window › Next/
//! Previous Project Window cycle the focus through main → opened windows → main.
//!
//! Settings are app-wide: a change made in any window is copied to every other window's session.

use std::path::PathBuf;

use egui::{RichText, ViewportBuilder, ViewportCommand, ViewportId};
use serde_json::json;

use codecraft_engine::Session;
use codecraft_workspace::Settings;

use crate::native_menu::{NativeMenu, held_modifiers};
use crate::{PersistedState, UiState, run_command};

/// File › Open with a project already open: ask where the picked project goes, like RustRover.
pub(crate) fn open_choice(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(path) = st.open_choice.clone() else { return };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let mut choice: Option<Option<Request>> = None;
    let resp = egui::Modal::new(egui::Id::new("codecraft-open-project-choice")).show(ui.ctx(), |ui| {
        ui.set_max_width(440.0);
        ui.label(RichText::new("Open Project").strong());
        ui.add_space(4.0);
        ui.label(format!("Open '{name}' in this window, or in a new window?"));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("This Window").clicked() {
                choice = Some(None);
                run_command(s, st, "file.open", &json!({ "path": path }));
            }
            if ui.button("New Window").clicked() {
                choice = Some(Some(Request::Open(path.clone())));
            }
            if ui.button("Cancel").clicked() {
                choice = Some(None);
            }
        });
    });
    if choice.is_none() && resp.should_close() {
        choice = Some(None);
    }
    if let Some(request) = choice {
        st.open_choice = None;
        st.window_requests.extend(request);
    }
}

/// What a window's effects ask of the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// Open this path in a new project window.
    Open(PathBuf),
    /// Focus the next (+1) or previous (-1) project window.
    Switch(i32),
}

pub(crate) struct ProjectWindow {
    id: ViewportId,
    pub(crate) session: Session,
    pub(crate) ui: UiState,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct ProjectWindows {
    windows: Vec<ProjectWindow>,
    next_id: u64,
    /// Native menu activations waiting for the focused project window's frame.
    pending_native: Vec<String>,
    /// The settings every window had after the last sync.
    settings: Option<Settings>,
}

impl ProjectWindows {
    /// All project windows, the main one included.
    pub(crate) fn count(&self) -> usize {
        self.windows.len().saturating_add(1)
    }

    /// Open `path` in a new window with the main window's settings.
    pub(crate) fn open(&mut self, path: PathBuf, main: &Session, persisted: &PersistedState) {
        let mut session = Session::new();
        persisted.apply(&mut session);
        session.ws.settings = main.ws.settings.clone();
        session.lsp_enabled = main.lsp_enabled;
        session.run_store.generate.auto = main.run_store.generate.auto;
        session.rust.deps.online = main.rust.deps.online;
        session.rust.deps.auto = main.rust.deps.auto;
        // Save and restore the window's project session like the main window does.
        session.session_store.autosave = main.session_store.autosave;
        session.session_store.restore_on_open = main.session_store.restore_on_open;
        session.session_store.fallback_dir = main.session_store.fallback_dir.clone();
        let mut ui = UiState::new();
        ui.window_sizes = persisted.window_sizes.clone();
        run_command(&mut session, &mut ui, "file.open", &json!({ "path": path }));
        self.next_id = self.next_id.wrapping_add(1);
        let id = ViewportId::from_hash_of(("codecraft-project-window", self.next_id));
        self.windows.push(ProjectWindow { id, session, ui, closed: false });
    }

    /// The index of the focused project window (`None`: the main window or none).
    pub(crate) fn focused(&self, ctx: &egui::Context) -> Option<usize> {
        self.windows.iter().position(|w| ctx.input_for(w.id, |i| i.viewport().focused == Some(true)))
    }

    pub(crate) fn session(&self, index: usize) -> Option<&Session> {
        self.windows.get(index).map(|w| &w.session)
    }

    /// The project of each open window (reopened on the next start).
    pub(crate) fn project_roots(&self) -> Vec<PathBuf> {
        self.windows
            .iter()
            .filter(|w| !w.closed)
            .filter_map(|w| Some(w.session.ws.project.as_ref()?.root.clone()))
            .collect()
    }

    /// Save every window's project session now.
    pub(crate) fn save_sessions(&mut self) {
        for w in &mut self.windows {
            if let Err(e) = codecraft_engine::session_store::save_now(&mut w.session) {
                log::warn!("cannot save the session: {e}");
            }
        }
    }

    /// Keep native menu activations for the focused window's next frame.
    pub(crate) fn defer_native(&mut self, fired: Vec<String>) {
        self.pending_native.extend(fired);
    }

    /// Focus the window `delta` steps from `from` (`None`: the main window), wrapping around.
    pub(crate) fn switch(&self, ctx: &egui::Context, from: Option<usize>, delta: i32) {
        let n = self.count() as i64;
        let current = from.map_or(0, |i| i as i64 + 1);
        let target = (current + i64::from(delta)).rem_euclid(n.max(1));
        let id = match usize::try_from(target - 1).ok().and_then(|i| self.windows.get(i)) {
            Some(w) => w.id,
            None => ViewportId::ROOT,
        };
        ctx.send_viewport_cmd_to(id, ViewportCommand::Focus);
    }

    /// Show every project window for this frame and collect their requests as `(window, request)`.
    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        persisted: &PersistedState,
        native: &NativeMenu,
        requests: &mut Vec<(Option<usize>, Request)>,
    ) {
        if ctx.embed_viewports() {
            // No native windows here (tests, offscreen snapshots): the windows' panels would share
            // one surface with the main window's. Keep their sessions running, unpainted.
            for w in &mut self.windows {
                w.session.pump();
            }
            self.pending_native.clear();
            return;
        }
        let focused = self.focused(ctx);
        let size = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size()).unwrap_or(egui::vec2(1400.0, 900.0));
        let mut pending = std::mem::take(&mut self.pending_native);
        for (index, w) in self.windows.iter_mut().enumerate() {
            let fired = if focused == Some(index) { std::mem::take(&mut pending) } else { Vec::new() };
            let builder = ViewportBuilder::default()
                .with_title(crate::App::window_title(&w.session))
                .with_inner_size(size)
                .with_min_inner_size([640.0, 400.0]);
            ctx.show_viewport_immediate(w.id, builder, |ui, _class| {
                let ctx = ui.ctx().clone();
                if !fired.is_empty() {
                    let held = held_modifiers(&ctx, &ctx.input(|i| i.events.clone()));
                    let mut events = Vec::new();
                    native.deliver(fired.clone(), &ctx, &mut w.session, &mut w.ui, &held, &mut events);
                    ctx.input_mut(|i| i.events.extend(events));
                }
                crate::App::window_logic(&ctx, &mut w.session, &mut w.ui);
                crate::App::paint(ui, &mut w.session, &mut w.ui, persisted, !native.active());
                if ctx.input(|i| i.viewport().close_requested()) {
                    w.closed = true;
                }
            });
            requests.extend(w.ui.window_requests.drain(..).map(|r| (Some(index), r)));
        }
        for w in self.windows.iter_mut().filter(|w| w.closed) {
            let _ = w.session.save_all();
            w.session.shutdown();
        }
        self.windows.retain(|w| !w.closed);
    }

    /// Copy a settings change made in any window to all the others.
    pub(crate) fn sync_settings(&mut self, main: &mut Session) {
        let Some(last) = self.settings.as_ref() else {
            self.settings = Some(main.ws.settings.clone());
            return;
        };
        let changed = if main.ws.settings != *last {
            Some(main.ws.settings.clone())
        } else {
            self.windows.iter().map(|w| &w.session.ws.settings).find(|s| *s != last).cloned()
        };
        if let Some(new) = changed {
            main.ws.settings = new.clone();
            for w in &mut self.windows {
                w.session.ws.settings = new.clone();
            }
            self.settings = Some(new);
        }
    }

    /// Save and stop every project window's session (app exit).
    pub(crate) fn shutdown(&mut self) {
        for w in &mut self.windows {
            let _ = w.session.save_all();
            w.session.shutdown();
        }
        self.windows.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_session() -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s
    }

    #[test]
    fn open_counts_and_inherits_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut main = main_session();
        main.ws.settings.tab_size = 7;
        let mut w = ProjectWindows::default();
        assert_eq!(w.count(), 1);
        w.open(dir.path().to_path_buf(), &main, &PersistedState::default());
        assert_eq!(w.count(), 2);
        let s = w.session(0).unwrap();
        assert_eq!(s.ws.settings.tab_size, 7);
        assert!(!s.lsp_enabled);
        assert!(s.ws.project.is_some(), "the directory opens as the window's project");
        w.shutdown();
        assert_eq!(w.count(), 1);
    }

    #[test]
    fn settings_changes_propagate_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let mut main = main_session();
        let mut w = ProjectWindows::default();
        w.open(dir.path().to_path_buf(), &main, &PersistedState::default());
        w.sync_settings(&mut main);
        // Main → window.
        main.ws.settings.soft_wrap = !main.ws.settings.soft_wrap;
        w.sync_settings(&mut main);
        assert_eq!(w.windows[0].session.ws.settings.soft_wrap, main.ws.settings.soft_wrap);
        // Window → main.
        w.windows[0].session.ws.settings.tab_size = 2;
        w.sync_settings(&mut main);
        assert_eq!(main.ws.settings.tab_size, 2);
        w.shutdown();
    }

    #[test]
    fn open_choice_asks_where_the_project_goes() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut s = main_session();
        s.execute("file.open", &json!({ "path": a.path() })).unwrap();
        let mut st = UiState::new();
        st.open_choice = Some(b.path().to_path_buf());
        let mut h = Harness::new_ui_state(|ui, (s, st): &mut (Session, UiState)| open_choice(ui, s, st), (s, st));
        h.run_steps(2);
        h.get_by_label("New Window").click_accesskit();
        h.run_steps(2);
        assert_eq!(h.state().1.window_requests, vec![Request::Open(b.path().to_path_buf())]);
        assert!(h.state().1.open_choice.is_none());
        // This Window opens it in the same session.
        h.state_mut().1.window_requests.clear();
        h.state_mut().1.open_choice = Some(b.path().to_path_buf());
        h.run_steps(2);
        h.get_by_label("This Window").click_accesskit();
        h.run_steps(2);
        assert!(h.state().1.window_requests.is_empty());
        assert_eq!(h.state().0.ws.project.as_ref().map(|p| p.root.clone()), Some(b.path().to_path_buf()));
        // Cancel does nothing.
        h.state_mut().1.open_choice = Some(a.path().to_path_buf());
        h.run_steps(2);
        h.get_by_label("Cancel").click_accesskit();
        h.run_steps(2);
        assert!(h.state().1.open_choice.is_none() && h.state().1.window_requests.is_empty());
        h.state_mut().0.shutdown();
    }

    #[test]
    fn switch_wraps_around() {
        let dir = tempfile::tempdir().unwrap();
        let main = main_session();
        let mut w = ProjectWindows::default();
        w.open(dir.path().to_path_buf(), &main, &PersistedState::default());
        let ctx = egui::Context::default();
        // Never panics on any index or delta, including a stale window index.
        for from in [None, Some(0), Some(5)] {
            for delta in [-3, -1, 1, 2, i32::MAX, i32::MIN] {
                w.switch(&ctx, from, delta);
            }
        }
        w.shutdown();
    }
}
