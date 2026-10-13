//! Engine-side state of the editor-depth features (track E): the live template session, the
//! chooser popups (Insert Live Template, Surround With, Unwrap/Remove), the Save as Live
//! Template draft, the navigation bar, column selection mode, soft-wrap geometry reported by
//! the view, Cyclic Expand Word and the edit-location history.
//!
//! The UI reads these to draw; agents drive them through the commands in `editing_cmds.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use codecraft_text::column::BoxPoint;
use codecraft_text::{Assoc, ChangeSet};
use codecraft_workspace::{EditorId, Location};

use crate::Session;
use crate::templates::TemplateStore;

/// A live template being filled in: Tab/Shift+Tab move between its variables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateSession {
    pub editor: EditorId,
    /// Document revision the ranges are valid for.
    pub revision: u64,
    /// Every occurrence of each variable, in document char offsets.
    pub vars: Vec<Vec<Range<usize>>>,
    /// The variable being edited.
    pub current: usize,
    /// Where the caret goes after the last variable (`$END$`).
    pub end: Option<usize>,
    /// Values offered for each variable (LSP snippet choices `${1|a,b|}`; empty = free text).
    #[serde(default)]
    pub choices: Vec<Vec<String>>,
}

impl TemplateSession {
    fn map(&mut self, changes: &ChangeSet) {
        for occurrences in &mut self.vars {
            for r in occurrences.iter_mut() {
                let start = changes.map_pos(r.start, Assoc::Before);
                let end = changes.map_pos(r.end, Assoc::After).max(start);
                *r = start..end;
            }
        }
        self.end = self.end.map(|e| changes.map_pos(e, Assoc::After));
    }

    /// The ranges of the current variable.
    pub fn current_ranges(&self) -> &[Range<usize>] {
        self.vars.get(self.current).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Which list a [`Chooser`] shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChooserKind {
    /// Code › Insert Live Template… (Cmd+J).
    InsertTemplate,
    /// Code › Surround With… (Alt+Cmd+T).
    SurroundWith,
    /// Code › Unwrap/Remove….
    UnwrapRemove,
    /// The values of an LSP snippet choice placeholder (`${1|a,b|}`).
    SnippetChoice,
    /// Edit › Paste › Paste from History… (the key is the history index).
    PasteFromHistory,
    /// Edit › Find Usages › Recent Find Usages (the key is the search's index).
    RecentFindUsages,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChooserItem {
    pub label: String,
    pub detail: String,
    /// What accepting runs: a template abbreviation or an unwrap option index.
    pub key: String,
}

/// A small list popup at the caret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chooser {
    pub kind: ChooserKind,
    pub editor: EditorId,
    pub title: String,
    pub items: Vec<ChooserItem>,
    pub query: String,
    /// Index into [`Chooser::visible`].
    pub selected: usize,
}

impl Chooser {
    /// Items matching the query (abbreviation or description prefix/substring), in order.
    pub fn visible(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                q.is_empty() || it.label.to_lowercase().contains(&q) || it.detail.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_item(&self) -> Option<&ChooserItem> {
        self.visible().get(self.selected).and_then(|i| self.items.get(*i))
    }
}

/// Code › Save as Live Template… draft (edited in a dialog).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveTemplateDraft {
    pub abbreviation: String,
    pub description: String,
    pub text: String,
    /// Language id (`rust`, `python`, …).
    pub language: String,
    pub error: Option<String>,
}

/// One element of the navigation bar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavKind {
    Directory,
    File,
    Symbol,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavSegment {
    pub label: String,
    pub kind: NavKind,
    /// The directory or file (for symbols: their file).
    pub path: PathBuf,
    /// Symbols: char offset of the name; its extent end.
    pub offset: Option<usize>,
    pub end: Option<usize>,
}

/// An entry of a navigation bar popup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavItem {
    pub label: String,
    pub kind: NavKind,
    pub path: PathBuf,
    pub offset: Option<usize>,
}

/// Navigate › Jump to Navigation Bar (Cmd+Up): the path to the caret and a popup of the
/// selected element's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavBar {
    pub editor: EditorId,
    pub segments: Vec<NavSegment>,
    pub selected: usize,
    pub items: Vec<NavItem>,
    pub item_selected: usize,
}

/// Cyclic Expand Word in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpandWord {
    pub editor: EditorId,
    pub start: usize,
    pub prefix: String,
    pub candidates: Vec<String>,
    /// Index of the inserted candidate.
    pub index: usize,
    /// Revision and caret right after our last expansion (anything else ends the cycle).
    pub revision: u64,
    pub caret: usize,
}

/// A column-mode block selection, kept so the box survives lines shorter than it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColumnBlock {
    pub editor: EditorId,
    pub anchor: BoxPoint,
    pub head: BoxPoint,
    pub revision: u64,
}

/// Places where text was edited (Navigate › Last/Next Edit Location).
#[derive(Clone, Debug, Default)]
pub(crate) struct EditHistory {
    pub places: Vec<(Location, usize)>,
    /// Position while stepping back through the history; `None` = at the newest.
    pub cursor: Option<usize>,
}

const MAX_EDIT_PLACES: usize = 64;

impl EditHistory {
    pub fn record(&mut self, loc: Location, line: usize) {
        self.cursor = None;
        if let Some((last, last_line)) = self.places.last_mut()
            && last.path == loc.path
            && last_line.abs_diff(line) <= 1
        {
            *last = loc;
            *last_line = line;
            return;
        }
        self.places.push((loc, line));
        if self.places.len() > MAX_EDIT_PLACES {
            self.places.remove(0);
        }
    }
}

/// Track E state on the [`Session`].
#[derive(Default)]
pub struct EditingState {
    pub templates: TemplateStore,
    pub session: Option<TemplateSession>,
    pub chooser: Option<Chooser>,
    pub save_dialog: Option<SaveTemplateDraft>,
    pub navbar: Option<NavBar>,
    /// Editors in column selection mode (Edit › Column Selection Mode).
    pub column_mode: BTreeSet<EditorId>,
    /// Soft-wrap width in display columns per editor, reported by the view each frame.
    pub wrap_widths: BTreeMap<EditorId, usize>,
    pub(crate) expand: Option<ExpandWord>,
    pub(crate) block: Option<ColumnBlock>,
    pub(crate) history: EditHistory,
    /// Set while an editing command applies its own edit (keeps its state across the hook).
    pub(crate) own_edit: bool,
}

impl EditingState {
    /// The view reports how many display columns fit in an editor (0 = not wrapping).
    pub fn set_wrap_width(&mut self, editor: EditorId, columns: usize) {
        if columns == 0 {
            self.wrap_widths.remove(&editor);
        } else {
            self.wrap_widths.insert(editor, columns);
        }
    }

    /// The live template session of an editor, if one is active and current.
    pub fn session_for(&self, editor: EditorId, revision: u64) -> Option<&TemplateSession> {
        self.session.as_ref().filter(|t| t.editor == editor && t.revision == revision)
    }
}

/// Hook from `Session::edit_active`: map live template ranges through the edit, record the
/// edit location and end a Cyclic Expand Word cycle that someone else interrupted.
pub(crate) fn after_edit(s: &mut Session, editor: EditorId, changes: &ChangeSet) {
    let revision = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)).map(|d| d.doc.revision()).unwrap_or(0);
    if let Some(t) = &mut s.editing.session
        && t.editor == editor
    {
        t.map(changes);
        t.revision = revision;
    }
    if !s.editing.own_edit {
        s.editing.expand = None;
        s.editing.block = None;
    }
    crate::inline_rename::on_edit(s, editor);
    let place = s.ws.editor(editor).and_then(|e| {
        let d = s.ws.doc(e.doc)?;
        let head = e.selections.primary().head;
        Some((Location { path: d.path.clone()?, offset: head }, d.doc.char_to_line(head)))
    });
    if let Some((loc, line)) = place {
        s.editing.history.record(loc, line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ranges_follow_typing() {
        let mut t = TemplateSession {
            editor: EditorId(1),
            revision: 0,
            vars: vec![vec![4..4, 10..10], vec![12..15]],
            current: 0,
            end: Some(20),
            choices: Vec::new(),
        };
        // Type "ab" at both occurrences of the first variable.
        let cs = ChangeSet::new(vec![
            codecraft_text::Change::insert(4, "ab"),
            codecraft_text::Change::insert(10, "ab"),
        ])
        .unwrap();
        t.map(&cs);
        assert_eq!(t.vars, vec![vec![4..6, 12..14], vec![16..19]]);
        assert_eq!(t.end, Some(24));
        assert_eq!(t.current_ranges(), &[4..6, 12..14]);
    }

    #[test]
    fn chooser_filters_by_label_and_detail() {
        let item = |l: &str, d: &str| ChooserItem { label: l.into(), detail: d.into(), key: l.into() };
        let mut c = Chooser {
            kind: ChooserKind::InsertTemplate,
            editor: EditorId(1),
            title: String::new(),
            items: vec![item("fn", "Function"), item("for", "for loop"), item("if", "if")],
            query: "fo".into(),
            selected: 0,
        };
        assert_eq!(c.visible(), vec![1]);
        c.query = "FUNC".into();
        assert_eq!(c.selected_item().map(|i| i.key.as_str()), Some("fn"));
        c.selected = 9;
        assert!(c.selected_item().is_none());
    }

    #[test]
    fn edit_history_merges_nearby_lines() {
        let mut h = EditHistory::default();
        let loc = |o| Location { path: PathBuf::from("/a.rs"), offset: o };
        h.record(loc(1), 0);
        h.record(loc(5), 1);
        h.record(loc(90), 10);
        assert_eq!(h.places.len(), 2);
        assert_eq!(h.places[0].0.offset, 5);
        for i in 0..100 {
            h.record(loc(i), i * 3);
        }
        assert_eq!(h.places.len(), MAX_EDIT_PLACES);
    }
}
