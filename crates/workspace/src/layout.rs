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
    DatabaseChanges,
    InspectionResults,
    Profiler,
    PythonConsole,
    PythonPackages,
    FileTransfer,
    RemoteHost,
    /// Tools › Rust › Rust REPL (evcxr).
    RustRepl,
    /// Background Python processes (console, kernels, formatters) for diagnosis (M12 track PY).
    PythonProcessOutput,
    /// CLion's CMake tool window (M12 track CL).
    CMake,
    /// Zig build steps (M13 track RC).
    Zig,
    /// TypeScript compiler errors and tsconfig files (M13 track RC).
    TypeScript,
    /// Spec View: the project's specs (requirements, design, tasks) and the code linked to them.
    /// A CodeCraft extension (RustRover has none); docked under Project on the left.
    Spec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Left,
    Bottom,
    Right,
}

impl ToolWindowId {
    pub const ALL: [ToolWindowId; 32] = [
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
        Self::DatabaseChanges,
        Self::InspectionResults,
        Self::Profiler,
        Self::PythonConsole,
        Self::PythonPackages,
        Self::FileTransfer,
        Self::RemoteHost,
        Self::RustRepl,
        Self::PythonProcessOutput,
        Self::CMake,
        Self::Zig,
        Self::TypeScript,
        Self::Spec,
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
            Self::DatabaseChanges => "Database Changes",
            Self::InspectionResults => "Inspection Results",
            Self::Profiler => "Profiler",
            Self::PythonConsole => "Python Console",
            Self::PythonPackages => "Python Packages",
            Self::FileTransfer => "File Transfer",
            Self::RemoteHost => "Remote Host",
            Self::RustRepl => "Rust REPL",
            Self::PythonProcessOutput => "Python Process Output",
            Self::CMake => "CMake",
            Self::Zig => "Zig",
            Self::TypeScript => "TypeScript",
            Self::Spec => "Spec View",
        }
    }

    pub fn side(self) -> Side {
        match self {
            Self::Project | Self::Commit | Self::Structure | Self::Bookmarks | Self::Spec => Side::Left,
            Self::Cargo
            | Self::Notifications
            | Self::Database
            | Self::Agent
            | Self::Coverage
            | Self::Hierarchy
            | Self::RemoteHost => Side::Right,
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
            Self::DatabaseChanges => "databaseChanges",
            Self::InspectionResults => "inspectionResults",
            Self::Profiler => "profiler",
            Self::PythonConsole => "pythonConsole",
            Self::PythonPackages => "pythonPackages",
            Self::FileTransfer => "fileTransfer",
            Self::RemoteHost => "remoteHost",
            Self::RustRepl => "rustRepl",
            Self::PythonProcessOutput => "pythonProcessOutput",
            Self::CMake => "cmake",
            Self::Zig => "zig",
            Self::TypeScript => "typeScript",
            Self::Spec => "spec",
        }
    }

    /// Docked in the lower half of the left side, below the Project tool window, instead of
    /// replacing it: both can be open at once.
    pub fn is_lower_left(self) -> bool {
        matches!(self, Self::Spec)
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == key)
    }
}

/// The height a lower-left tool window (Spec View) opens with.
pub const LOWER_LEFT_HEIGHT: f32 = 240.0;

/// Which tool window is open on each side, and the panel sizes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub left: Option<ToolWindowId>,
    /// The tool window docked under `left` (see [`ToolWindowId::is_lower_left`]).
    #[serde(default)]
    pub left_lower: Option<ToolWindowId>,
    pub bottom: Option<ToolWindowId>,
    pub right: Option<ToolWindowId>,
    pub left_width: f32,
    pub bottom_height: f32,
    pub right_width: f32,
    /// Remembered size (width, or height for bottom windows) of each tool window, by key.
    #[serde(default)]
    pub tool_sizes: BTreeMap<String, f32>,
    /// Remembered size of the resizable panes inside tool windows (the Git log's details pane,
    /// the commit message/files split, ...), by pane key.
    #[serde(default)]
    pub pane_sizes: BTreeMap<String, f32>,
    /// Distraction-free: hide all tool windows (Cmd+Shift+F12, "Hide All Tool Windows").
    pub all_hidden: bool,
    /// The tool window shown last (Window › Active Tool Window acts on it).
    #[serde(default)]
    pub last_active: Option<ToolWindowId>,
    /// Window › Active Tool Window › Maximize Tool Window: this one fills the window.
    #[serde(default)]
    pub maximized: Option<ToolWindowId>,
    /// Window › Active Tool Window › Group Tabs: tool windows show their tabs as one list.
    #[serde(default)]
    pub group_tabs: bool,
    /// Bumped when sizes change outside a drag (stretch, layout restore) so panels re-read them.
    #[serde(default)]
    pub size_rev: u64,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            left: Some(ToolWindowId::Project),
            left_lower: None,
            bottom: None,
            right: None,
            left_width: 280.0,
            bottom_height: 260.0,
            right_width: 300.0,
            tool_sizes: BTreeMap::new(),
            pane_sizes: BTreeMap::new(),
            all_hidden: false,
            last_active: None,
            maximized: None,
            group_tabs: false,
            size_rev: 0,
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

    /// The remembered size of the pane `key` inside a tool window, or `fallback`.
    pub fn pane_size(&self, key: &str, fallback: f32) -> f32 {
        self.pane_sizes.get(key).copied().filter(|v| v.is_finite() && *v > 0.0).unwrap_or(fallback)
    }

    /// Remember the size of the pane `key` (ignores non-finite or non-positive values; the map
    /// is capped so a runaway caller cannot grow it without bound).
    pub fn remember_pane(&mut self, key: &str, size: f32) {
        if !size.is_finite() || size <= 0.0 {
            return;
        }
        if self.pane_sizes.len() >= 256 && !self.pane_sizes.contains_key(key) {
            return;
        }
        self.pane_sizes.insert(key.to_string(), size);
    }

    pub fn slot(&self, side: Side) -> Option<ToolWindowId> {
        match side {
            Side::Left => self.left,
            Side::Bottom => self.bottom,
            Side::Right => self.right,
        }
    }

    /// The slot `tw` lives in (its side's, or the lower-left one).
    fn slot_mut(&mut self, tw: ToolWindowId) -> &mut Option<ToolWindowId> {
        if tw.is_lower_left() {
            return &mut self.left_lower;
        }
        match tw.side() {
            Side::Left => &mut self.left,
            Side::Bottom => &mut self.bottom,
            Side::Right => &mut self.right,
        }
    }

    pub fn is_visible(&self, tw: ToolWindowId) -> bool {
        let shown = if tw.is_lower_left() { self.left_lower } else { self.slot(tw.side()) };
        !self.all_hidden && shown == Some(tw)
    }

    /// RustRover's Cmd+digit behaviour: hidden → show; shown → hide.
    pub fn toggle(&mut self, tw: ToolWindowId) {
        self.all_hidden = false;
        let slot = self.slot_mut(tw);
        *slot = if *slot == Some(tw) { None } else { Some(tw) };
        if *slot == Some(tw) {
            self.last_active = Some(tw);
        }
    }

    pub fn show(&mut self, tw: ToolWindowId) {
        self.all_hidden = false;
        *self.slot_mut(tw) = Some(tw);
        self.last_active = Some(tw);
    }

    /// The tool window Window › Active Tool Window acts on: the last one shown while it is still
    /// visible, else a visible one (bottom, right, left).
    pub fn active(&self) -> Option<ToolWindowId> {
        if self.all_hidden {
            return None;
        }
        self.last_active
            .filter(|tw| self.is_visible(*tw))
            .or(self.bottom)
            .or(self.right)
            .or(self.left)
            .or(self.left_lower)
    }

    pub fn hide(&mut self, tw: ToolWindowId) {
        let slot = self.slot_mut(tw);
        if *slot == Some(tw) {
            *slot = None;
        }
        if self.maximized == Some(tw) {
            self.maximized = None;
        }
    }

    /// The size a side's open tool window has (its remembered size, else the side's size).
    pub fn current_size(&self, tw: ToolWindowId) -> f32 {
        if tw.is_lower_left() {
            return self.size_of(tw, LOWER_LEFT_HEIGHT);
        }
        let fallback = match tw.side() {
            Side::Left => self.left_width,
            Side::Bottom => self.bottom_height,
            Side::Right => self.right_width,
        };
        self.size_of(tw, fallback)
    }

    /// Set a tool window's size (clamped to 80..=2000) and make the panels pick it up.
    pub fn set_size(&mut self, tw: ToolWindowId, size: f32) {
        if !size.is_finite() {
            return;
        }
        let size = size.clamp(80.0, 2000.0);
        match tw.side() {
            // A lower-left window's size is its height, kept per window.
            Side::Left if tw.is_lower_left() => {}
            Side::Left => self.left_width = size,
            Side::Bottom => self.bottom_height = size,
            Side::Right => self.right_width = size,
        }
        self.remember_size(tw, size);
        self.size_rev = self.size_rev.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_sizes_are_remembered_and_hostile_values_ignored() {
        let mut l = Layout::default();
        assert_eq!(l.pane_size("git.details", 300.0), 300.0);
        l.remember_pane("git.details", 410.0);
        assert_eq!(l.pane_size("git.details", 300.0), 410.0);
        l.remember_pane("git.details", f32::NAN);
        l.remember_pane("git.details", -1.0);
        assert_eq!(l.pane_size("git.details", 300.0), 410.0);
        for i in 0..1000 {
            l.remember_pane(&format!("k{i}"), 100.0);
        }
        assert!(l.pane_sizes.len() <= 256);
    }

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
    fn the_spec_view_sits_under_project_instead_of_replacing_it() {
        let mut l = Layout::default();
        l.toggle(ToolWindowId::Spec);
        assert!(l.is_visible(ToolWindowId::Spec) && l.is_visible(ToolWindowId::Project));
        assert_eq!((l.left, l.left_lower), (Some(ToolWindowId::Project), Some(ToolWindowId::Spec)));
        // Another left window replaces Project, not the Spec View.
        l.toggle(ToolWindowId::Structure);
        assert!(l.is_visible(ToolWindowId::Spec) && !l.is_visible(ToolWindowId::Project));
        assert_eq!(l.active(), Some(ToolWindowId::Structure));
        l.hide(ToolWindowId::Structure);
        l.hide(ToolWindowId::Spec);
        assert_eq!((l.left, l.left_lower), (None, None));
        l.show(ToolWindowId::Spec);
        l.all_hidden = true;
        assert!(!l.is_visible(ToolWindowId::Spec));
        l.all_hidden = false;
        assert_eq!(l.active(), Some(ToolWindowId::Spec));
    }

    #[test]
    fn the_spec_views_size_is_its_height_and_leaves_the_width_alone() {
        let mut l = Layout::default();
        assert_eq!(l.current_size(ToolWindowId::Spec), LOWER_LEFT_HEIGHT);
        let width = l.left_width;
        l.set_size(ToolWindowId::Spec, 333.0);
        assert_eq!((l.current_size(ToolWindowId::Spec), l.left_width), (333.0, width));
    }

    #[test]
    fn a_saved_layout_without_the_lower_slot_still_loads() {
        let mut v = serde_json::to_value(Layout::default()).unwrap();
        v.as_object_mut().unwrap().remove("left_lower");
        let l: Layout = serde_json::from_value(v).unwrap();
        assert_eq!(l.left_lower, None);
    }

    #[test]
    fn keys_roundtrip() {
        for tw in ToolWindowId::ALL {
            assert_eq!(ToolWindowId::from_key(tw.key()), Some(tw));
        }
    }
}
