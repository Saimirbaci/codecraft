//! Persisted IDE state: settings, layout, recent projects, last project.

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
    /// Files open in the last project (reopened with it).
    pub open_files: Vec<PathBuf>,
    /// Inner size of the main window in points, restored on the next launch.
    pub window_size: Option<[f32; 2]>,
    /// Last size of each floating window (dialogs, settings, …), by key.
    pub window_sizes: std::collections::BTreeMap<String, [f32; 2]>,
}

fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODECRAFT_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("state.json"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/CodeCraft")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("codecraft")
    };
    Some(dir.join("state.json"))
}

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
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    log::warn!("cannot save {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("cannot serialize state: {e}"),
        }
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
            s.ws.tabs.iter().filter_map(|t| s.ws.editor(*t)).filter_map(|e| s.ws.doc(e.doc)?.path.clone()).collect();
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
}
