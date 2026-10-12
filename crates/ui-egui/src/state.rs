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

    pub fn apply(&self, s: &mut Session) {
        if let Some(settings) = &self.settings {
            s.ws.settings = settings.clone();
        }
        if let Some(layout) = &self.layout {
            s.ws.layout = layout.clone();
        }
        s.ws.recent_projects = self.recent_projects.iter().filter(|p| p.is_dir()).cloned().collect();
    }

    pub fn capture(&mut self, s: &Session) {
        self.settings = Some(s.ws.settings.clone());
        self.layout = Some(s.ws.layout.clone());
        self.recent_projects = s.ws.recent_projects.clone();
        self.last_project = s.ws.project.as_ref().map(|p| p.root.clone());
        self.open_files =
            s.ws.tabs.iter().filter_map(|t| s.ws.editor(*t)).filter_map(|e| s.ws.doc(e.doc)?.path.clone()).collect();
    }
}
