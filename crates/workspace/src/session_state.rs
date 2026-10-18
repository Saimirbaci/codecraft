//! Session snapshot: what a project window shows, as serde data (RustRover keeps the same in
//! `.idea/workspace.xml`): the split tree of editor groups, each group's tabs (file, pinned,
//! carets, scroll, folds), the focused group, the tool-window layout and the Terminal's shell
//! tabs (name and directory; the engine starts new shells for them). The engine persists it
//! per project and opens the files; [`Workspace::restore_session`] rebuilds the editors.
//!
//! A snapshot comes from disk, so it is hostile: indices, positions, sizes and the tree shape are
//! all checked, and whatever does not fit is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use codecraft_text::{Position, Selection, SelectionSet};

use crate::splits::clamp_ratio;
use crate::{DocId, EditorGroup, EditorId, GroupId, Layout, SplitNode, Workspace, top_offset};

/// Format version written into new snapshots.
pub const SNAPSHOT_VERSION: u32 = 1;
/// At most this many editor groups are restored.
pub const MAX_GROUPS: usize = 64;
/// At most this many tabs (over all groups) are restored.
pub const MAX_TABS: usize = 512;
const MAX_SELECTIONS: usize = 4096;
const MAX_FOLDS: usize = 65_536;
/// Deeper splitter nesting than this is dropped.
const MAX_DEPTH: usize = 32;
const MAX_TOOL_SIZES: usize = 128;

/// A project window's editors and tool-window layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSnapshot {
    pub version: u32,
    /// The project root the snapshot was taken in (informational).
    pub project: Option<PathBuf>,
    /// The split tree; each leaf `GroupId(i)` is the index of a group in `groups`.
    pub split: SplitNode,
    /// Editor groups in display order.
    pub groups: Vec<GroupSnapshot>,
    /// Index in `groups` of the focused group.
    pub focused: usize,
    pub layout: Option<Layout>,
    /// The Terminal's shell tabs, in order (programs such as `ssh` or `claude` are not kept).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terminals: Vec<TerminalSnapshot>,
    /// Index in `terminals` of the selected tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_active: Option<usize>,
}

/// At most this many terminal tabs are restored.
pub const MAX_TERMINALS: usize = 32;

/// One Terminal tab: a shell (or Claude Code) is started again in `cwd` when the project opens.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSnapshot {
    /// The tab's stable key: its own command history and saved output are kept under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The name the user gave the tab (`None`: the automatic "Local", "Local (2)", …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The shell's working directory when the snapshot was taken.
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "TerminalKind::is_shell")]
    pub kind: TerminalKind,
    /// The Claude Code session running in the tab (in its shell, or as the tab's program):
    /// restoring resumes exactly this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeSnapshot>,
}

/// What a terminal tab runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalKind {
    /// The login shell.
    #[default]
    Shell,
    /// Claude Code itself (Open Claude Code in Terminal).
    ClaudeCode,
}

impl TerminalKind {
    pub fn is_shell(&self) -> bool {
        *self == Self::Shell
    }
}

/// A Claude Code session in a terminal tab.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeSnapshot {
    pub session: String,
    /// Its transcript file: when it is gone, the session cannot be resumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<PathBuf>,
}

impl Default for SessionSnapshot {
    fn default() -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            project: None,
            split: SplitNode::default(),
            groups: Vec::new(),
            focused: 0,
            layout: None,
            terminals: Vec::new(),
            terminal_active: None,
        }
    }
}

/// One editor group: its tabs in display order and the selected one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupSnapshot {
    pub tabs: Vec<TabSnapshot>,
    /// Index in `tabs` of the selected tab.
    pub active: Option<usize>,
}

/// One tab: a file and its view state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabSnapshot {
    pub path: PathBuf,
    pub pinned: bool,
    /// Selections as (line, char column) positions, so a file edited outside the IDE still
    /// gets its carets near the same place.
    pub selections: Vec<SelectionSnapshot>,
    /// Index in `selections` of the primary selection.
    pub primary: usize,
    pub scroll_line: f32,
    pub scroll_x: f32,
    /// Start lines of collapsed fold regions.
    pub folded: Vec<usize>,
    /// The tab's own soft-wrap choice (`None`: the setting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_wrap: Option<bool>,
    /// Edit › Column Selection Mode was on in this tab.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub column_mode: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionSnapshot {
    pub anchor: Position,
    pub head: Position,
}

/// What [`Workspace::restore_session`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSummary {
    /// Editor groups after the restore.
    pub groups: usize,
    /// Tabs restored from the snapshot.
    pub tabs: usize,
    /// Files the snapshot names that could not be opened (missing, unreadable), once each.
    pub skipped: Vec<PathBuf>,
    /// Restored editors whose tab had column selection mode on (the engine keeps that mode).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub column_mode: Vec<EditorId>,
}

impl SessionSnapshot {
    /// Parse a snapshot; any malformed input is an error, never a panic.
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// Every distinct file the snapshot shows, in display order (at most [`MAX_TABS`]).
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for tab in self.groups.iter().take(MAX_GROUPS).flat_map(|g| g.tabs.iter()) {
            if out.len() >= MAX_TABS {
                break;
            }
            if !tab.path.as_os_str().is_empty() && seen.insert(tab.path.clone()) {
                out.push(tab.path.clone());
            }
        }
        out
    }

    /// Number of tabs over all groups.
    pub fn tab_count(&self) -> usize {
        self.groups.iter().map(|g| g.tabs.len()).fold(0, usize::saturating_add)
    }
}

/// State while [`Workspace::restore_session`] walks the snapshot's tree.
struct Build<'a, F> {
    snap: &'a SessionSnapshot,
    doc_for: F,
    /// Lookups by path (each path is opened once).
    docs: BTreeMap<PathBuf, Option<DocId>>,
    /// Snapshot group index → the group made for it.
    made: BTreeMap<usize, GroupId>,
    tabs: usize,
    column_mode: Vec<EditorId>,
}

impl Workspace {
    /// The current editors and layout as a snapshot. Tabs without a file (untitled buffers)
    /// are left out.
    pub fn session_snapshot(&self) -> SessionSnapshot {
        self.session_snapshot_with(|_| false)
    }

    /// [`Self::session_snapshot`], with `column_mode` telling which editors are in column
    /// selection mode (state the engine keeps).
    pub fn session_snapshot_with(&self, column_mode: impl Fn(EditorId) -> bool) -> SessionSnapshot {
        let leaves = self.split.leaves();
        let index: BTreeMap<GroupId, usize> = leaves.iter().enumerate().map(|(i, g)| (*g, i)).collect();
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            project: self.project.as_ref().map(|p| p.root.clone()),
            split: remap(&self.split, &index),
            groups: leaves.iter().map(|g| self.group_snapshot(*g, &column_mode)).collect(),
            focused: index.get(&self.focused).copied().unwrap_or(0),
            layout: Some(self.layout.clone()),
            terminals: Vec::new(),
            terminal_active: None,
        }
    }

    fn group_snapshot(&self, g: GroupId, column_mode: &impl Fn(EditorId) -> bool) -> GroupSnapshot {
        let Some(group) = self.groups.get(&g) else { return GroupSnapshot::default() };
        let mut out = GroupSnapshot::default();
        for id in &group.tabs {
            let Some(e) = self.editors.get(id) else { continue };
            let Some(od) = self.docs.get(&e.doc) else { continue };
            let Some(path) = od.path.clone() else { continue };
            if group.active == Some(*id) {
                out.active = Some(out.tabs.len());
            }
            let pos = |at: usize| od.doc.offset_to_position(at);
            out.tabs.push(TabSnapshot {
                path,
                pinned: e.pinned,
                selections: e
                    .selections
                    .ranges()
                    .iter()
                    .map(|s| SelectionSnapshot { anchor: pos(s.anchor), head: pos(s.head) })
                    .collect(),
                primary: e.selections.primary_index(),
                scroll_line: finite_non_negative(e.scroll_line),
                scroll_x: finite_non_negative(e.scroll_x),
                folded: e.folded.iter().copied().collect(),
                soft_wrap: e.soft_wrap,
                column_mode: column_mode(*id),
            });
        }
        out
    }

    /// Rebuild the editor groups from `snap`. `doc_for` gives the open document for a path (the
    /// engine opens the files first); `None` means it could not be opened and its tabs are
    /// skipped. Tabs open before the restore whose document the restored layout does not show
    /// stay open in the focused group, so nothing is lost; documents are never closed here.
    /// Selections, scroll positions and folds are clamped to the documents; a snapshot that
    /// yields no tab leaves one empty group. The tool-window layout is applied when present.
    pub fn restore_session(
        &mut self,
        snap: &SessionSnapshot,
        doc_for: impl FnMut(&Path) -> Option<DocId>,
    ) -> RestoreSummary {
        let mut previous = self.all_tabs();
        let ungrouped: Vec<EditorId> = self.editors.keys().copied().filter(|id| !previous.contains(id)).collect();
        previous.extend(ungrouped);
        self.groups.clear();
        let mut b =
            Build { snap, doc_for, docs: BTreeMap::new(), made: BTreeMap::new(), tabs: 0, column_mode: Vec::new() };
        let tree = self.build_node(&snap.split, 0, &mut b);
        // Restored views of a document replace the old ones.
        let restored: BTreeSet<EditorId> = self.groups.values().flat_map(|g| g.tabs.iter().copied()).collect();
        let shown: BTreeSet<DocId> = restored.iter().filter_map(|id| self.editors.get(id)).map(|e| e.doc).collect();
        let mut leftovers = Vec::new();
        for id in previous {
            match self.editors.get(&id).map(|e| e.doc) {
                Some(doc) if !shown.contains(&doc) => leftovers.push(id),
                _ => {
                    self.editors.remove(&id);
                    self.view_revs.remove(&id);
                }
            }
        }
        match tree {
            Some(t) => self.split = t,
            None => {
                let g = self.fresh_group();
                self.split = SplitNode::Group(g);
                self.groups.insert(g, EditorGroup::default());
            }
        }
        let leaves = self.split.leaves();
        self.focused = b
            .made
            .get(&snap.focused)
            .copied()
            .filter(|g| leaves.contains(g))
            .or_else(|| leaves.first().copied())
            .unwrap_or_default();
        if !leftovers.is_empty() {
            let group = self.groups.entry(self.focused).or_default();
            group.tabs.extend(leftovers);
            if group.active.is_none() {
                group.active = group.tabs.first().copied();
            }
        }
        if let Some(layout) = &snap.layout {
            self.layout = sanitize_layout(layout);
        }
        let mut skipped: Vec<PathBuf> = b.docs.iter().filter(|(_, d)| d.is_none()).map(|(p, _)| p.clone()).collect();
        skipped.sort();
        RestoreSummary { groups: leaves.len(), tabs: b.tabs, skipped, column_mode: b.column_mode }
    }

    /// Close `doc` if no editor shows it (a document opened for a restore whose tabs were all
    /// dropped). Returns it so the caller can tell the language server.
    pub fn drop_unshown_doc(&mut self, doc: DocId) -> Option<crate::OpenDocument> {
        self.drop_doc_if_unused(doc)
    }

    fn fresh_group(&mut self) -> GroupId {
        self.next_group = self.next_group.saturating_add(1);
        GroupId(self.next_group)
    }

    fn build_node<F: FnMut(&Path) -> Option<DocId>>(
        &mut self,
        node: &SplitNode,
        depth: usize,
        b: &mut Build<'_, F>,
    ) -> Option<SplitNode> {
        if depth > MAX_DEPTH {
            return None;
        }
        match node {
            SplitNode::Group(GroupId(i)) => {
                let i = usize::try_from(*i).ok()?;
                if b.made.contains_key(&i) || b.made.len() >= MAX_GROUPS {
                    return None;
                }
                let snap = b.snap.groups.get(i)?;
                let g = self.build_group(snap, b)?;
                b.made.insert(i, g);
                Some(SplitNode::Group(g))
            }
            SplitNode::Split { axis, ratio, first, second } => {
                let a = self.build_node(first, depth + 1, b);
                let c = self.build_node(second, depth + 1, b);
                match (a, c) {
                    (Some(a), Some(c)) => Some(SplitNode::Split {
                        axis: *axis,
                        ratio: clamp_ratio(*ratio),
                        first: Box::new(a),
                        second: Box::new(c),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    /// A group with the tabs of `snap` that could be opened; `None` if none could.
    fn build_group<F: FnMut(&Path) -> Option<DocId>>(
        &mut self,
        snap: &GroupSnapshot,
        b: &mut Build<'_, F>,
    ) -> Option<GroupId> {
        let mut group = EditorGroup::default();
        let mut docs_here = BTreeSet::new();
        for (i, tab) in snap.tabs.iter().enumerate() {
            if b.tabs >= MAX_TABS {
                break;
            }
            if tab.path.as_os_str().is_empty() {
                continue;
            }
            let doc = match b.docs.get(&tab.path) {
                Some(d) => *d,
                None => {
                    let d = (b.doc_for)(&tab.path).filter(|d| self.docs.contains_key(d));
                    b.docs.insert(tab.path.clone(), d);
                    d
                }
            };
            // One view of a document per group, like the rest of the model.
            let Some(doc) = doc.filter(|d| docs_here.insert(*d)) else { continue };
            let Some(id) = self.restore_editor(doc, tab) else { continue };
            if tab.column_mode {
                b.column_mode.push(id);
            }
            group.tabs.push(id);
            b.tabs += 1;
            // The selected tab, or the nearest restored one before it.
            if snap.active.is_some_and(|a| i <= a) || group.active.is_none() {
                group.active = Some(id);
            }
        }
        if group.tabs.is_empty() {
            return None;
        }
        let g = self.fresh_group();
        self.groups.insert(g, group);
        Some(g)
    }

    fn restore_editor(&mut self, doc: DocId, tab: &TabSnapshot) -> Option<EditorId> {
        let id = self.new_editor(doc, None)?;
        let od = self.docs.get(&doc)?;
        let text = &od.doc;
        let lines = text.len_lines();
        let ranges: Vec<Selection> = tab
            .selections
            .iter()
            .take(MAX_SELECTIONS)
            .map(|s| Selection::new(text.position_to_offset(s.anchor), text.position_to_offset(s.head)))
            .collect();
        let selections = SelectionSet::from_vec(ranges, tab.primary).clamp(text.len_chars());
        let max_line = lines.saturating_sub(1) as f32;
        let scroll_line = finite_non_negative(tab.scroll_line).min(max_line);
        let folded = tab.folded.iter().take(MAX_FOLDS).copied().filter(|l| *l < lines).collect();
        let top = top_offset(text, scroll_line);
        let rev = text.revision();
        let e = self.editors.get_mut(&id)?;
        e.selections = selections;
        e.scroll_line = scroll_line;
        e.scroll_x = finite_non_negative(tab.scroll_x).min(1.0e7);
        e.folded = folded;
        e.pinned = tab.pinned;
        e.soft_wrap = tab.soft_wrap;
        // Keep the restored scroll position rather than jumping to the caret.
        e.reveal_caret = false;
        self.view_revs.insert(id, (rev, top));
        Some(id)
    }
}

/// The tree with each group id replaced by its display index.
fn remap(node: &SplitNode, index: &BTreeMap<GroupId, usize>) -> SplitNode {
    match node {
        SplitNode::Group(g) => SplitNode::Group(GroupId(index.get(g).copied().unwrap_or(0) as u64)),
        SplitNode::Split { axis, ratio, first, second } => SplitNode::Split {
            axis: *axis,
            ratio: clamp_ratio(*ratio),
            first: Box::new(remap(first, index)),
            second: Box::new(remap(second, index)),
        },
    }
}

fn finite_non_negative(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

/// A layout with panel sizes forced into a sane range (a hostile snapshot must not make a panel
/// zero or a million pixels wide).
fn sanitize_layout(l: &Layout) -> Layout {
    let d = Layout::default();
    let size = |v: f32, fallback: f32| if v.is_finite() && (40.0..=10_000.0).contains(&v) { v } else { fallback };
    Layout {
        left: l.left,
        bottom: l.bottom,
        right: l.right,
        left_width: size(l.left_width, d.left_width),
        bottom_height: size(l.bottom_height, d.bottom_height),
        right_width: size(l.right_width, d.right_width),
        tool_sizes: l
            .tool_sizes
            .iter()
            .filter(|(k, v)| k.len() <= 64 && v.is_finite() && (40.0..=10_000.0).contains(*v))
            .take(MAX_TOOL_SIZES)
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        pane_sizes: l
            .pane_sizes
            .iter()
            .filter(|(k, v)| k.len() <= 64 && v.is_finite() && (40.0..=10_000.0).contains(*v))
            .take(MAX_TOOL_SIZES)
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        all_hidden: l.all_hidden,
        last_active: l.last_active,
        group_tabs: l.group_tabs,
        ..Layout::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SplitAxis, ToolWindowId};

    fn text(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    /// A workspace split as `[a.rs] | ([a.rs, b.rs] over [b.rs, c.rs])` with carets, scroll,
    /// folds and a pinned tab; the bottom-right group is focused.
    fn sample() -> (Workspace, [DocId; 3]) {
        let mut ws = Workspace::new();
        let a = ws.add_document(Some("/p/a.rs".into()), &text(100));
        let b = ws.add_document(Some("/p/b.rs".into()), &text(50));
        let c = ws.add_document(Some("/p/c.rs".into()), &text(10));
        let ea = ws.open_editor(a).unwrap();
        ws.set_selections(ea, SelectionSet::from_vec(vec![Selection::new(7, 12), Selection::caret(40)], 1));
        let e = ws.editor_mut(ea).unwrap();
        e.scroll_line = 20.5;
        e.scroll_x = 12.0;
        e.folded.insert(3);
        e.pinned = true;
        ws.split_editor(SplitAxis::Horizontal, false).unwrap();
        let eb = ws.open_editor(b).unwrap();
        ws.split_editor(SplitAxis::Vertical, false).unwrap();
        ws.open_editor(c).unwrap();
        // b.rs is selected in the middle group; c.rs in the focused bottom-right group.
        ws.set_active_editor(eb);
        ws.cycle_group(1);
        ws.layout.toggle(ToolWindowId::Problems);
        ws.layout.bottom_height = 333.0;
        (ws, [a, b, c])
    }

    fn restore_into(snap: &SessionSnapshot, files: &[(&str, usize)]) -> (Workspace, RestoreSummary) {
        let mut ws = Workspace::new();
        let ids: BTreeMap<PathBuf, DocId> =
            files.iter().map(|(p, n)| (PathBuf::from(p), ws.add_document(Some((*p).into()), &text(*n)))).collect();
        let summary = ws.restore_session(snap, |p| ids.get(p).copied());
        (ws, summary)
    }

    fn tab_paths(ws: &Workspace, g: GroupId) -> Vec<String> {
        ws.group(g)
            .unwrap()
            .tabs
            .iter()
            .map(|t| ws.doc(ws.editor(*t).unwrap().doc).unwrap().path.as_ref().unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn round_trip_with_splits_carets_scroll_folds_and_layout() {
        let (ws, _) = sample();
        let snap = ws.session_snapshot();
        assert_eq!(snap.groups.len(), 3);
        assert_eq!(snap.tab_count(), 5);
        let json = serde_json::to_string(&snap).unwrap();
        let back = SessionSnapshot::from_json(&json).unwrap();
        assert_eq!(back, snap);

        let (rws, summary) = restore_into(&back, &[("/p/a.rs", 100), ("/p/b.rs", 50), ("/p/c.rs", 10)]);
        assert_eq!(summary, RestoreSummary { groups: 3, tabs: 5, skipped: vec![], column_mode: vec![] });
        assert_eq!(rws.session_snapshot(), snap, "restoring and snapshotting again is the identity");
        let groups = rws.group_ids();
        assert_eq!(tab_paths(&rws, groups[0]), ["/p/a.rs"]);
        assert_eq!(tab_paths(&rws, groups[1]), ["/p/a.rs", "/p/b.rs"]);
        assert_eq!(tab_paths(&rws, groups[2]), ["/p/b.rs", "/p/c.rs"]);
        assert_eq!(rws.focused_group(), groups[2]);
        let (e, d) = rws.active().unwrap();
        assert_eq!(d.path.as_deref(), Some(Path::new("/p/c.rs")));
        assert!(!e.reveal_caret);
        // The first group's view of a.rs.
        let first = rws.editor(rws.group(groups[0]).unwrap().tabs[0]).unwrap();
        assert_eq!(first.selections.ranges(), &[Selection::new(7, 12), Selection::caret(40)]);
        assert_eq!(first.selections.primary_index(), 1);
        assert_eq!((first.scroll_line, first.scroll_x, first.pinned), (20.5, 12.0, true));
        assert!(first.folded.contains(&3));
        let middle = rws.group(groups[1]).unwrap();
        assert_eq!(rws.editor(middle.active.unwrap()).unwrap().doc, rws.doc_by_path(Path::new("/p/b.rs")).unwrap());
        assert!(matches!(rws.split_layout(), SplitNode::Split { axis: SplitAxis::Horizontal, .. }));
        assert!(rws.layout.is_visible(ToolWindowId::Problems));
        assert_eq!(rws.layout.bottom_height, 333.0);
    }

    #[test]
    fn missing_files_are_skipped_and_empty_groups_collapse() {
        let (ws, _) = sample();
        let snap = ws.session_snapshot();
        // Only c.rs exists now (and it is shorter): the groups without it go away.
        let (rws, summary) = restore_into(&snap, &[("/p/c.rs", 2)]);
        assert_eq!(summary.skipped, vec![PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs")]);
        assert_eq!(summary.tabs, 1);
        assert_eq!(rws.group_ids().len(), 1);
        assert!(!rws.is_split());
        assert_eq!(rws.active().unwrap().1.path.as_deref(), Some(Path::new("/p/c.rs")));

        // Nothing exists: one empty group, no panic.
        let (rws, summary) = restore_into(&snap, &[]);
        assert_eq!((summary.groups, summary.tabs), (1, 0));
        assert!(rws.tabs().is_empty() && rws.active().is_none());
    }

    #[test]
    fn selections_scroll_and_folds_clamp_to_shorter_files() {
        let (ws, _) = sample();
        let snap = ws.session_snapshot();
        let (rws, _) = restore_into(&snap, &[("/p/a.rs", 2), ("/p/b.rs", 1), ("/p/c.rs", 1)]);
        for id in rws.all_tabs() {
            let e = rws.editor(id).unwrap();
            let d = rws.doc(e.doc).unwrap();
            assert!(e.selections.ranges().iter().all(|s| s.end() <= d.doc.len_chars()));
            assert!(e.scroll_line <= d.doc.len_lines() as f32);
            assert!(e.folded.iter().all(|l| *l < d.doc.len_lines()));
        }
    }

    #[test]
    fn tabs_already_open_stay_and_restored_views_replace_old_ones() {
        let (ws, _) = sample();
        let snap = ws.session_snapshot();
        let mut rws = Workspace::new();
        let a = rws.add_document(Some("/p/a.rs".into()), &text(100));
        let x = rws.add_document(Some("/p/x.rs".into()), "x");
        rws.open_editor(a).unwrap();
        rws.open_editor(x).unwrap();
        rws.restore_session(&snap, |p| rws_doc(p, a));
        // a.rs comes back in its two restored groups (the old view goes); x.rs (not in the
        // snapshot) joins the focused group.
        assert_eq!(rws.editors_for_doc(a).len(), 2);
        assert_eq!(rws.editors_for_doc(x).len(), 1);
        assert_eq!(rws.group_of(rws.editor_for_doc(x).unwrap()), Some(rws.focused_group()));
        assert_eq!(rws.all_tabs().len(), 3);
    }

    fn rws_doc(p: &Path, a: DocId) -> Option<DocId> {
        (p == Path::new("/p/a.rs")).then_some(a)
    }

    #[test]
    fn hostile_snapshots_never_panic() {
        let tab = |p: &str| TabSnapshot {
            path: p.into(),
            selections: vec![SelectionSnapshot {
                anchor: Position { line: usize::MAX, column: usize::MAX },
                head: Position { line: 0, column: usize::MAX },
            }],
            primary: usize::MAX,
            scroll_line: f32::NAN,
            scroll_x: f32::INFINITY,
            folded: vec![usize::MAX, 0],
            ..Default::default()
        };
        let mut deep = SplitNode::Group(GroupId(0));
        for i in 0..200 {
            deep = SplitNode::Split {
                axis: SplitAxis::Vertical,
                ratio: f32::NAN,
                first: Box::new(deep),
                second: Box::new(SplitNode::Group(GroupId(i % 5))),
            };
        }
        let mut layout = Layout { left_width: f32::NAN, bottom_height: -5.0, right_width: 1e30, ..Layout::default() };
        layout.tool_sizes.insert("x".into(), f32::NAN);
        layout.pane_sizes.insert("x".into(), f32::INFINITY);
        let snaps = [
            SessionSnapshot {
                split: deep,
                groups: vec![
                    GroupSnapshot { tabs: vec![tab("/p/a.rs"), tab("/p/a.rs"), tab("")], active: Some(usize::MAX) },
                    GroupSnapshot { tabs: vec![], active: Some(3) },
                    GroupSnapshot { tabs: vec![tab("/p/b.rs")], active: None },
                ],
                focused: usize::MAX,
                layout: Some(layout),
                ..Default::default()
            },
            SessionSnapshot { split: SplitNode::Group(GroupId(u64::MAX)), ..Default::default() },
            SessionSnapshot {
                split: SplitNode::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 7.0,
                    first: Box::new(SplitNode::Group(GroupId(0))),
                    second: Box::new(SplitNode::Group(GroupId(0))),
                },
                groups: vec![GroupSnapshot { tabs: vec![tab("/p/a.rs")], active: Some(0) }],
                ..Default::default()
            },
        ];
        for snap in &snaps {
            let (rws, summary) = restore_into(snap, &[("/p/a.rs", 3), ("/p/b.rs", 0)]);
            assert!(summary.groups >= 1);
            assert!(rws.group_ids().contains(&rws.focused_group()));
            let mut seen = BTreeSet::new();
            for g in rws.group_ids() {
                assert!(seen.insert(g), "a group appears once in the tree");
            }
            assert!(rws.layout.left_width.is_finite() && rws.layout.bottom_height > 0.0);
            assert!(rws.layout.tool_sizes.is_empty());
            assert!(rws.layout.pane_sizes.is_empty());
            for id in rws.all_tabs() {
                let e = rws.editor(id).unwrap();
                assert!(e.scroll_line.is_finite() && e.scroll_x.is_finite());
            }
            // Snapshotting the result is well-formed again.
            let again = rws.session_snapshot();
            assert_eq!(again.groups.len(), rws.group_ids().len());
        }
        // The duplicate leaf is dropped rather than shown twice.
        let (rws, _) = restore_into(&snaps[2], &[("/p/a.rs", 3)]);
        assert!(!rws.is_split());
    }

    #[test]
    fn per_tab_soft_wrap_and_column_mode_round_trip() {
        let mut ws = Workspace::new();
        let a = ws.add_document(Some("/p/a.rs".into()), &text(10));
        let b = ws.add_document(Some("/p/b.rs".into()), &text(10));
        let ea = ws.open_editor(a).unwrap();
        let eb = ws.open_editor(b).unwrap();
        ws.editor_mut(ea).unwrap().soft_wrap = Some(true);
        assert!(ws.soft_wrap_of(ea));
        assert!(!ws.soft_wrap_of(eb), "b follows the setting");
        ws.settings.soft_wrap = true;
        assert!(ws.soft_wrap_of(eb));
        ws.editor_mut(eb).unwrap().soft_wrap = Some(false);
        assert!(!ws.soft_wrap_of(eb));
        let snap = ws.session_snapshot_with(|id| id == eb);
        let tabs = &snap.groups[0].tabs;
        assert_eq!((tabs[0].soft_wrap, tabs[0].column_mode), (Some(true), false));
        assert_eq!((tabs[1].soft_wrap, tabs[1].column_mode), (Some(false), true));
        // The plain snapshot has no column mode; older files without the fields still load.
        assert!(ws.session_snapshot().groups[0].tabs.iter().all(|t| !t.column_mode));
        let json = serde_json::to_string(&snap).unwrap();
        let back = SessionSnapshot::from_json(&json).unwrap();
        let (rws, summary) = restore_into(&back, &[("/p/a.rs", 10), ("/p/b.rs", 10)]);
        assert_eq!(summary.column_mode.len(), 1);
        let restored_b = rws.editor(summary.column_mode[0]).unwrap();
        assert_eq!(rws.doc(restored_b.doc).unwrap().path.as_deref(), Some(Path::new("/p/b.rs")));
        assert_eq!(restored_b.soft_wrap, Some(false));
        let doc_a = rws.doc_by_path(Path::new("/p/a.rs")).unwrap();
        let restored_a = rws.editor(rws.editor_for_doc(doc_a).unwrap()).unwrap();
        assert_eq!(restored_a.soft_wrap, Some(true));
        let old = SessionSnapshot::from_json(r#"{"groups": [{"tabs": [{"path": "/p/a.rs"}]}]}"#).unwrap();
        assert_eq!((old.groups[0].tabs[0].soft_wrap, old.groups[0].tabs[0].column_mode), (None, false));
    }

    #[test]
    fn corrupt_json_is_an_error_and_partial_json_uses_defaults() {
        for bad in ["", "{", "null", "{\"groups\": 5}", "{\"split\": {\"Nope\": 1}}", "\u{0}"] {
            assert!(SessionSnapshot::from_json(bad).is_err(), "{bad:?}");
        }
        // Nesting deeper than the parser's recursion limit is an error, not a stack overflow.
        let deep =
            format!("{{\"split\":{}{}", "{\"Split\":{\"axis\":\"Vertical\",\"ratio\":0.5,\"first\":".repeat(5000), "x");
        assert!(SessionSnapshot::from_json(&deep).is_err());
        let snap = SessionSnapshot::from_json("{\"groups\": [{\"tabs\": [{\"path\": \"/p/a.rs\"}]}]}").unwrap();
        assert_eq!(snap.paths(), vec![PathBuf::from("/p/a.rs")]);
        let (rws, summary) = restore_into(&snap, &[("/p/a.rs", 3)]);
        assert_eq!(summary.tabs, 1);
        assert_eq!(rws.active().unwrap().0.selections, SelectionSet::caret(0));
    }
}
