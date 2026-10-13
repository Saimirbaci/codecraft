//! Persisted IDE state: settings, layout, recent projects, last project and the other project
//! windows (each project's editors are saved with the project, see
//! `codecraft_engine::session_store`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use codecraft_engine::Session;
use codecraft_workspace::{Layout, Settings};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    pub settings: Option<Settings>,
    pub layout: Option<Layout>,
    pub recent_projects: Vec<PathBuf>,
    pub last_project: Option<PathBuf>,
    /// Files open in the last project (superseded by the per-project session snapshot; kept so
    /// older state files load).
    pub open_files: Vec<PathBuf>,
    /// Projects open in their own windows besides the main one at exit (reopened on start).
    pub open_windows: Vec<PathBuf>,
    /// Inner size of the main window in points, restored on the next launch.
    pub window_size: Option<[f32; 2]>,
    /// Last size of each floating window (dialogs, settings, …), by key.
    pub window_sizes: std::collections::BTreeMap<String, [f32; 2]>,
}

fn config_path() -> Option<PathBuf> {
    codecraft_engine::session_store::config_dir().map(|d| d.join("state.json"))
}

/// At most this many project windows are reopened besides the main one.
const MAX_REOPEN_WINDOWS: usize = 16;

impl PersistedState {
    /// Load; a missing or corrupt file yields defaults (never fatal).
    pub fn load() -> Self {
        let Some(path) = config_path() else { return Self::default() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("ignoring corrupt {}: {e}", path.display());
            Self::default()
        })
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            log::warn!("cannot create {}: {e}", dir.display());
            return;
        }
        match serde_json::to_string_pretty(self) {
            // Atomic, so a crash mid-write never leaves a truncated state file.
            Ok(text) => {
                if let Err(e) = codecraft_fs::write_text_file_atomic(&path, &text) {
                    log::warn!("cannot save {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("cannot serialize state: {e}"),
        }
    }

    /// The project windows to reopen beside a main window showing `main`: existing directories,
    /// each once, not the main project.
    pub fn windows_to_reopen(&self, main: Option<&std::path::Path>) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for p in &self.open_windows {
            if p.is_dir() && Some(p.as_path()) != main && !out.contains(p) {
                out.push(p.clone());
            }
        }
        out.truncate(MAX_REOPEN_WINDOWS);
        out
    }

    /// The remembered main window size, if it is sane.
    pub fn saved_window_size(&self) -> Option<[f32; 2]> {
        self.window_size.filter(|[w, h]| {
            w.is_finite() && h.is_finite() && (400.0..=10000.0).contains(w) && (300.0..=10000.0).contains(h)
        })
    }

    pub fn apply(&self, s: &mut Session) {
        if let Some(settings) = &self.settings {
            s.ws.settings = settings.clone();
        }
        if let Some(layout) = &self.layout {
            s.ws.layout = layout.clone();
        }
        s.ws.recent_projects = self.recent_projects.iter().filter(|p| p.is_dir()).cloned().collect();
    }

    pub fn capture(&mut self, s: &Session, ui: &crate::UiState) {
        self.settings = Some(s.ws.settings.clone());
        self.layout = Some(s.ws.layout.clone());
        self.recent_projects = s.ws.recent_projects.clone();
        self.last_project = s.ws.project.as_ref().map(|p| p.root.clone());
        self.window_sizes = ui.window_sizes.clone();
        self.open_files =
            s.ws.all_tabs()
                .iter()
                .filter_map(|t| s.ws.editor(*t))
                .filter_map(|e| s.ws.doc(e.doc)?.path.clone())
                .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_sizes_round_trip_and_bad_values_are_ignored() {
        let mut p = PersistedState { window_size: Some([1500.0, 950.0]), ..Default::default() };
        p.window_sizes.insert("settings".into(), [600.0, 400.0]);
        let back: PersistedState = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back.saved_window_size(), Some([1500.0, 950.0]));
        assert_eq!(back.window_sizes.get("settings"), Some(&[600.0, 400.0]));
        for bad in [[0.0, 0.0], [f32::NAN, 800.0], [1e9, 800.0], [10.0, 10.0]] {
            assert_eq!(PersistedState { window_size: Some(bad), ..Default::default() }.saved_window_size(), None);
        }
    }

    #[test]
    fn windows_to_reopen_skip_missing_duplicate_and_main_projects() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let p = PersistedState {
            open_windows: vec![
                a.path().into(),
                a.path().join("gone"),
                b.path().into(),
                a.path().into(),
                std::path::PathBuf::new(),
            ],
            ..Default::default()
        };
        assert_eq!(p.windows_to_reopen(None), vec![a.path().to_path_buf(), b.path().to_path_buf()]);
        assert_eq!(p.windows_to_reopen(Some(a.path())), vec![b.path().to_path_buf()]);
        // Older state files without the field load with no windows.
        let old: PersistedState = serde_json::from_str("{\"last_project\": \"/x\"}").unwrap();
        assert!(old.open_windows.is_empty());
    }
}
