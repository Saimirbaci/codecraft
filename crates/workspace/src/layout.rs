use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Tool windows, with RustRover's default stripe placement and Cmd+digit shortcuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ToolWindowId {
    Project,
    Commit,
    Structure,
    Bookmarks,
    Run,
    Debug,
    Problems,
    Terminal,
    Build,
    Find,
    Services,
    Git,
    Todo,
    Cargo,
    Notifications,
    Database,
    Agent,
    Coverage,
    Hierarchy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Left,
    Bottom,
    Right,
}

impl ToolWindowId {
    pub const ALL: [ToolWindowId; 19] = [
        Self::Project,
        Self::Commit,
        Self::Structure,
        Self::Bookmarks,
        Self::Run,
        Self::Debug,
        Self::Problems,
        Self::Terminal,
        Self::Build,
        Self::Find,
        Self::Services,
        Self::Git,
        Self::Todo,
        Self::Cargo,
        Self::Notifications,
        Self::Database,
        Self::Agent,
        Self::Coverage,
        Self::Hierarchy,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Commit => "Commit",
            Self::Structure => "Structure",
            Self::Bookmarks => "Bookmarks",
            Self::Run => "Run",
            Self::Debug => "Debug",
            Self::Problems => "Problems",
            Self::Terminal => "Terminal",
            Self::Build => "Build",
            Self::Find => "Find",
            Self::Services => "Services",
            Self::Git => "Git",
            Self::Todo => "TODO",
            Self::Cargo => "Cargo",
            Self::Notifications => "Notifications",
            Self::Database => "Database",
            Self::Agent => "Agent",
            Self::Coverage => "Coverage",
            Self::Hierarchy => "Hierarchy",
        }
    }

    pub fn side(self) -> Side {
        match self {
            Self::Project | Self::Commit | Self::Structure | Self::Bookmarks => Side::Left,
            Self::Cargo
            | Self::Notifications
            | Self::Database
            | Self::Agent
            | Self::Coverage
            | Self::Hierarchy => Side::Right,
            _ => Side::Bottom,
        }
    }

    /// The View › Tool Windows shortcut observed in RustRover's macOS keymap.
    pub fn shortcut(self) -> Option<&'static str> {
        match self {
            Self::Commit => Some("Cmd+0"),
            Self::Project => Some("Cmd+1"),
            Self::Bookmarks => Some("Cmd+2"),
            Self::Find => Some("Cmd+3"),
            Self::Run => Some("Cmd+4"),
            Self::Debug => Some("Cmd+5"),
            Self::Problems => Some("Cmd+6"),
            Self::Structure => Some("Cmd+7"),
            Self::Services => Some("Cmd+8"),
            Self::Git => Some("Cmd+9"),
            Self::Terminal => Some("Alt+F12"),
            _ => None,
        }
    }

    /// Stable id fragment used in command ids (`view.toolWindows.<id>`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Commit => "commit",
            Self::Structure => "structure",
            Self::Bookmarks => "bookmarks",
            Self::Run => "run",
            Self::Debug => "debug",
            Self::Problems => "problems",
            Self::Terminal => "terminal",
            Self::Build => "build",
            Self::Find => "find",
            Self::Services => "services",
            Self::Git => "git",
            Self::Todo => "todo",
            Self::Cargo => "cargo",
            Self::Notifications => "notifications",
            Self::Database => "database",
            Self::Agent => "agent",
            Self::Coverage => "coverage",
            Self::Hierarchy => "hierarchy",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == key)
    }
}

/// Which tool window is open on each side, and the panel sizes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub left: Option<ToolWindowId>,
    pub bottom: Option<ToolWindowId>,
    pub right: Option<ToolWindowId>,
    pub left_width: f32,
    pub bottom_height: f32,
    pub right_width: f32,
    /// Remembered size (width, or height for bottom windows) of each tool window, by key.
    #[serde(default)]
    pub tool_sizes: BTreeMap<String, f32>,
    /// Distraction-free: hide all tool windows (Cmd+Shift+F12, "Hide All Tool Windows").
    pub all_hidden: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            left: Some(ToolWindowId::Project),
            bottom: None,
            right: None,
            left_width: 280.0,
            bottom_height: 260.0,
            right_width: 300.0,
            tool_sizes: BTreeMap::new(),
            all_hidden: false,
        }
    }
}

impl Layout {
    /// The remembered size of `tw`, or `fallback` if it was never resized.
    pub fn size_of(&self, tw: ToolWindowId, fallback: f32) -> f32 {
        self.tool_sizes.get(tw.key()).copied().filter(|v| v.is_finite() && *v > 0.0).unwrap_or(fallback)
    }

    /// Remember the size of `tw` (ignores non-finite or non-positive values).
    pub fn remember_size(&mut self, tw: ToolWindowId, size: f32) {
        if size.is_finite() && size > 0.0 {
            self.tool_sizes.insert(tw.key().to_string(), size);
        }
    }

    pub fn slot(&self, side: Side) -> Option<ToolWindowId> {
        match side {
            Side::Left => self.left,
            Side::Bottom => self.bottom,
            Side::Right => self.right,
        }
    }

    fn slot_mut(&mut self, side: Side) -> &mut Option<ToolWindowId> {
        match side {
            Side::Left => &mut self.left,
            Side::Bottom => &mut self.bottom,
            Side::Right => &mut self.right,
        }
    }

    pub fn is_visible(&self, tw: ToolWindowId) -> bool {
        !self.all_hidden && self.slot(tw.side()) == Some(tw)
    }

    /// RustRover's Cmd+digit behaviour: hidden → show; shown → hide.
    pub fn toggle(&mut self, tw: ToolWindowId) {
        self.all_hidden = false;
        let slot = self.slot_mut(tw.side());
        *slot = if *slot == Some(tw) { None } else { Some(tw) };
    }

    pub fn show(&mut self, tw: ToolWindowId) {
        self.all_hidden = false;
        *self.slot_mut(tw.side()) = Some(tw);
    }

    pub fn hide(&mut self, tw: ToolWindowId) {
        let slot = self.slot_mut(tw.side());
        if *slot == Some(tw) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_replaces_on_same_side() {
        let mut l = Layout::default();
        l.toggle(ToolWindowId::Run);
        assert!(l.is_visible(ToolWindowId::Run));
        l.toggle(ToolWindowId::Problems);
        assert!(!l.is_visible(ToolWindowId::Run));
        assert!(l.is_visible(ToolWindowId::Problems));
        l.toggle(ToolWindowId::Problems);
        assert_eq!(l.bottom, None);
        assert!(l.is_visible(ToolWindowId::Project));
    }

    #[test]
    fn keys_roundtrip() {
        for tw in ToolWindowId::ALL {
            assert_eq!(ToolWindowId::from_key(tw.key()), Some(tw));
        }
    }
}
