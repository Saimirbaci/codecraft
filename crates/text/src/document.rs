use ropey::Rope;
use serde::{Deserialize, Serialize};

use crate::util::{is_line_break, normalize_newlines, shift};
use crate::{Change, ChangeSet, Edit, Result, SelectionSet, TextError};

/// Zero-based line and column (column counts chars).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// The line separator a file uses on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }

    /// Status-bar label, as RustRover shows it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
            Self::Cr => "CR",
        }
    }

    /// The first separator found decides; no separator means LF.
    pub fn detect(text: &str) -> Self {
        let bytes = text.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            match b {
                b'\n' => return Self::Lf,
                b'\r' => return if bytes.get(i + 1) == Some(&b'\n') { Self::CrLf } else { Self::Cr },
                _ => {}
            }
        }
        Self::Lf
    }
}

/// How an edit joins the undo history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditGroup {
    /// Merges with the previous `Typing` edit unless the group was broken (caret moved, etc.).
    Typing,
    /// Always its own undo step.
    Standalone,
}

#[derive(Clone, Debug)]
struct Step {
    forward: ChangeSet,
    inverse: ChangeSet,
}

#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    steps: Vec<Step>,
    before: SelectionSet,
    after: SelectionSet,
    group: EditGroup,
}

/// A text document: rope, undo/redo history, revision counter and saved-state tracking.
#[derive(Clone, Debug)]
pub struct Document {
    rope: Rope,
    line_ending: LineEnding,
    saved_line_ending: LineEnding,
    revision: u64,
    history: Vec<Entry>,
    /// Number of applied entries in `history` (entries past it are redoable).
    applied: usize,
    next_entry_id: u64,
    saved_state: u64,
    group_open: bool,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self::from_text("")
    }

    /// Load text, normalizing `\r\n` and `\r` to `\n` and remembering the original separator.
    pub fn from_text(text: &str) -> Self {
        let line_ending = LineEnding::detect(text);
        let normalized = normalize_newlines(text);
        Self {
            rope: Rope::from_str(&normalized),
            line_ending,
            saved_line_ending: line_ending,
            revision: 0,
            history: Vec::new(),
            applied: 0,
            next_entry_id: 1,
            saved_state: 0,
            group_open: false,
        }
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// The text with the document's line separator, ready to write to disk.
    pub fn to_disk_text(&self) -> String {
        let text = self.rope.to_string();
        match self.line_ending {
            LineEnding::Lf => text,
            other => text.replace('\n', other.as_str()),
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Number of lines; always at least 1 (a trailing newline starts a new, empty line).
    pub fn len_lines(&self) -> usize {
        self.rope.len_lines().max(1)
    }

    pub fn line_text(&self, line: usize) -> Option<String> {
        if line >= self.len_lines() {
            return None;
        }
        let l = self.rope.get_line(line)?;
        let mut s = l.to_string();
        if s.ends_with(is_line_break) {
            s.pop();
        }
        Some(s)
    }

    /// Char offset of the start of `line` (clamped to the last line).
    pub fn line_start(&self, line: usize) -> usize {
        let line = line.min(self.len_lines().saturating_sub(1));
        self.rope.try_line_to_char(line).unwrap_or(self.len_chars())
    }

    /// Char offset of the end of `line`, before its newline (clamped).
    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    /// Length of `line` in chars, excluding the newline.
    pub fn line_len(&self, line: usize) -> usize {
        match self.rope.get_line(line.min(self.len_lines().saturating_sub(1))) {
            Some(l) => {
                let n = l.len_chars();
                // Ropey (built with `unicode_lines`) also ends lines at U+2028, U+0085, VT and FF.
                if n > 0 && l.get_char(n - 1).is_some_and(is_line_break) { n - 1 } else { n }
            }
            None => 0,
        }
    }

    pub fn char_at(&self, at: usize) -> Option<char> {
        self.rope.get_char(at)
    }

    pub fn char_to_line(&self, at: usize) -> usize {
        self.rope.try_char_to_line(at.min(self.len_chars())).unwrap_or(0)
    }

    pub fn offset_to_position(&self, at: usize) -> Position {
        let at = at.min(self.len_chars());
        let line = self.char_to_line(at);
        Position { line, column: at - self.line_start(line) }
    }

    /// Clamps the line to the document and the column to the line length.
    pub fn position_to_offset(&self, pos: Position) -> usize {
        let line = pos.line.min(self.len_lines().saturating_sub(1));
        self.line_start(line) + pos.column.min(self.line_len(line))
    }

    /// Text in `range` (clamped to the document).
    pub fn slice(&self, range: std::ops::Range<usize>) -> String {
        let len = self.len_chars();
        let (s, e) = (range.start.min(len), range.end.min(len));
        if s >= e {
            return String::new();
        }
        self.rope.get_slice(s..e).map(|r| r.to_string()).unwrap_or_default()
    }

    /// Increases on every content change, including undo and redo.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn set_line_ending(&mut self, le: LineEnding) {
        if self.line_ending != le {
            self.line_ending = le;
            self.revision += 1;
        }
    }

    /// Apply an edit, recording it for undo. `before` is the selection state to restore on undo.
    ///
    /// `\r\n`/`\r` in inserted text are normalized to `\n` (the stored step matches the rope).
    /// A `Typing` edit merges into the previous `Typing` group only while the group is open and
    /// `before` matches the selections that group ended with (a moved caret starts a new group).
    pub fn apply(&mut self, edit: &Edit, before: &SelectionSet, group: EditGroup) -> Result<()> {
        if edit.changes.is_empty() {
            // A caret-only typing step (typing over a closer) keeps the open group going.
            if group == EditGroup::Typing
                && self.group_open
                && self.applied == self.history.len()
                && let Some(last) = self.history.last_mut()
                && last.group == EditGroup::Typing
                && same_positions(&last.after, before)
            {
                last.after = edit.selections.clamp(self.rope.len_chars());
            }
            return Ok(());
        }
        let forward = edit.changes.with_normalized_newlines();
        let inverse = self.apply_changes(&forward)?;
        let after = edit.selections.clamp(self.len_chars());
        let step = Step { forward, inverse };
        self.history.truncate(self.applied);
        let merge = group == EditGroup::Typing
            && self.group_open
            && self.applied > 0
            && self.history.last().is_some_and(|e| {
                e.group == EditGroup::Typing && e.id != self.saved_state && same_positions(&e.after, before)
            });
        if merge && let Some(last) = self.history.last_mut() {
            last.steps.push(step);
            last.after = after;
        } else {
            let id = self.next_entry_id;
            self.next_entry_id += 1;
            self.history.push(Entry { id, steps: vec![step], before: before.clone(), after, group });
            self.applied = self.history.len();
        }
        self.group_open = group == EditGroup::Typing;
        Ok(())
    }

    /// Replace the whole text as one undoable step (e.g. reload from disk, reformat).
    pub fn replace_all(&mut self, text: &str, selections: &SelectionSet) -> Result<()> {
        let normalized = normalize_newlines(text);
        if normalized == self.text() {
            return Ok(());
        }
        let changes = ChangeSet::single(0, self.len_chars(), normalized);
        let len_after = changes.len_after(self.len_chars());
        let edit = Edit { changes, selections: selections.clamp(len_after) };
        self.apply(&edit, selections, EditGroup::Standalone)
    }

    /// Undo the last group; returns the selections to restore.
    pub fn undo(&mut self) -> Option<SelectionSet> {
        if self.applied == 0 {
            return None;
        }
        let entry = self.history.get(self.applied - 1)?.clone();
        for step in entry.steps.iter().rev() {
            if self.apply_changes(&step.inverse).is_err() {
                return None;
            }
        }
        self.applied -= 1;
        self.group_open = false;
        Some(entry.before)
    }

    /// Redo the next group; returns the selections to restore.
    pub fn redo(&mut self) -> Option<SelectionSet> {
        let entry = self.history.get(self.applied)?.clone();
        for step in &entry.steps {
            if self.apply_changes(&step.forward).is_err() {
                return None;
            }
        }
        self.applied += 1;
        self.group_open = false;
        Some(entry.after)
    }

    pub fn can_undo(&self) -> bool {
        self.applied > 0
    }

    pub fn can_redo(&self) -> bool {
        self.applied < self.history.len()
    }

    /// The next typing edit starts a new undo group (call when the caret moves).
    pub fn break_undo_group(&mut self) {
        self.group_open = false;
    }

    pub fn mark_saved(&mut self) {
        self.saved_state = self.state_id();
        self.saved_line_ending = self.line_ending;
        self.group_open = false;
    }

    pub fn is_modified(&self) -> bool {
        self.state_id() != self.saved_state || self.line_ending != self.saved_line_ending
    }

    /// LSP coordinates: (line, UTF-16 code units from line start).
    pub fn offset_to_utf16(&self, at: usize) -> (u32, u32) {
        let pos = self.offset_to_position(at);
        let line_start = self.line_start(pos.line);
        let units: usize = self
            .rope
            .get_slice(line_start..line_start + pos.column)
            .map(|s| s.chars().map(char::len_utf16).sum())
            .unwrap_or(0);
        (u32::try_from(pos.line).unwrap_or(u32::MAX), u32::try_from(units).unwrap_or(u32::MAX))
    }

    /// From LSP coordinates (clamped; a column inside a surrogate pair rounds down).
    pub fn utf16_to_offset(&self, line: u32, character: u32) -> usize {
        let line = (line as usize).min(self.len_lines().saturating_sub(1));
        let start = self.line_start(line);
        let len = self.line_len(line);
        let mut units = 0usize;
        let mut col = 0usize;
        if let Some(slice) = self.rope.get_slice(start..start + len) {
            for ch in slice.chars() {
                let next = units + ch.len_utf16();
                if next > character as usize {
                    break;
                }
                units = next;
                col += 1;
            }
        }
        start + col
    }

    /// Leading whitespace of `line`.
    pub fn indent_of_line(&self, line: usize) -> String {
        self.line_text(line).map(|t| t.chars().take_while(|c| *c == ' ' || *c == '\t').collect()).unwrap_or_default()
    }

    /// `"\t"` when most indented lines start with a tab, otherwise four spaces.
    pub fn detect_indent_unit(&self) -> String {
        let (mut tabs, mut spaces) = (0usize, 0usize);
        for line in self.rope.lines().take(2000) {
            match line.chars().next() {
                Some('\t') => tabs += 1,
                Some(' ') => spaces += 1,
                _ => {}
            }
        }
        if tabs > spaces { "\t".to_string() } else { "    ".to_string() }
    }

    fn state_id(&self) -> u64 {
        if self.applied == 0 { 0 } else { self.history.get(self.applied - 1).map(|e| e.id).unwrap_or(0) }
    }

    /// Apply changes to the rope; returns the inverse change set. Validates everything first,
    /// so an error leaves the document untouched.
    fn apply_changes(&mut self, changes: &ChangeSet) -> Result<ChangeSet> {
        let len = self.len_chars();
        changes.check_bounds(len)?;
        let mut inverse = Vec::with_capacity(changes.changes().len());
        let mut delta: isize = 0;
        for c in changes.changes() {
            let removed = self.slice(c.start..c.end);
            let start = shift(c.start, delta);
            let inserted = c.text.chars().count();
            inverse.push(Change::new(start, start + inserted, removed));
            delta += inserted as isize - c.end.saturating_sub(c.start) as isize;
        }
        let inverse = ChangeSet::new(inverse)?;
        // Apply back to front so earlier offsets stay valid.
        for c in changes.changes().iter().rev() {
            self.rope.try_remove(c.start..c.end).map_err(|_| TextError::OutOfBounds {
                start: c.start,
                end: c.end,
                len,
            })?;
            self.rope.try_insert(c.start, &c.text).map_err(|_| TextError::OutOfBounds {
                start: c.start,
                end: c.end,
                len,
            })?;
        }
        self.revision += 1;
        Ok(inverse)
    }
}

/// Same anchors and heads (goal columns ignored).
fn same_positions(a: &SelectionSet, b: &SelectionSet) -> bool {
    a.len() == b.len() && a.ranges().iter().zip(b.ranges()).all(|(x, y)| x.anchor == y.anchor && x.head == y.head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Selection;

    fn ins(doc: &Document, at: usize, s: &str) -> Edit {
        let changes = ChangeSet::single(at, at, s);
        let n = s.chars().count();
        let _ = doc;
        Edit { changes, selections: SelectionSet::caret(at + n) }
    }

    #[test]
    fn crlf_roundtrip() {
        let d = Document::from_text("a\r\nb\r\n");
        assert_eq!(d.text(), "a\nb\n");
        assert_eq!(d.line_ending(), LineEnding::CrLf);
        assert_eq!(d.to_disk_text(), "a\r\nb\r\n");
        assert_eq!(d.len_lines(), 3);
        assert_eq!(d.line_text(1).as_deref(), Some("b"));
    }

    #[test]
    fn typing_groups_and_undo_redo() {
        let mut d = Document::from_text("");
        let s0 = SelectionSet::caret(0);
        let e1 = ins(&d, 0, "a");
        d.apply(&e1, &s0, EditGroup::Typing).unwrap();
        let e2 = ins(&d, 1, "b");
        d.apply(&e2, &e1.selections, EditGroup::Typing).unwrap();
        assert_eq!(d.text(), "ab");
        assert!(d.is_modified());
        assert_eq!(d.undo(), Some(s0.clone()));
        assert_eq!(d.text(), "");
        assert!(!d.is_modified());
        d.redo();
        assert_eq!(d.text(), "ab");
        d.mark_saved();
        d.break_undo_group();
        let e3 = ins(&d, 2, "c");
        d.apply(&e3, &e2.selections, EditGroup::Typing).unwrap();
        d.undo();
        assert_eq!(d.text(), "ab");
        assert!(!d.is_modified());
    }

    #[test]
    fn utf16_conversions() {
        let d = Document::from_text("a😀b\nx");
        assert_eq!(d.offset_to_utf16(2), (0, 3));
        assert_eq!(d.utf16_to_offset(0, 3), 2);
        assert_eq!(d.utf16_to_offset(0, 2), 1);
        assert_eq!(d.utf16_to_offset(9, 9), d.len_chars());
    }

    #[test]
    fn out_of_bounds_is_an_error() {
        let mut d = Document::from_text("abc");
        let e = Edit { changes: ChangeSet::single(2, 10, "x"), selections: SelectionSet::caret(0) };
        assert!(d.apply(&e, &SelectionSet::caret(0), EditGroup::Standalone).is_err());
        assert_eq!(d.text(), "abc");
    }

    #[test]
    fn multi_change_inverse() {
        let mut d = Document::from_text("one two three");
        let cs = ChangeSet::new(vec![Change::new(0, 3, "1"), Change::new(8, 13, "3")]).unwrap();
        let sel = SelectionSet::single(Selection::caret(0));
        d.apply(&Edit { changes: cs, selections: sel.clone() }, &sel, EditGroup::Standalone).unwrap();
        assert_eq!(d.text(), "1 two 3");
        d.undo();
        assert_eq!(d.text(), "one two three");
    }

    #[test]
    fn crlf_in_inserted_text_is_normalized_and_undo_restores() {
        // Regression: the inverse used the un-normalized length, so undo deleted one char too many.
        let mut d = Document::from_text("xyz");
        let edit = Edit { changes: ChangeSet::single(0, 0, "a\r\nb\r"), selections: SelectionSet::caret(5) };
        d.apply(&edit, &SelectionSet::caret(0), EditGroup::Standalone).unwrap();
        assert_eq!(d.text(), "a\nb\nxyz");
        assert_eq!(d.undo(), Some(SelectionSet::caret(0)));
        assert_eq!(d.text(), "xyz");
        assert_eq!(d.redo(), Some(SelectionSet::caret(5)));
        assert_eq!(d.text(), "a\nb\nxyz");
        // Selections computed from the raw text are clamped to the real length.
        let mut e = Document::new();
        let edit = Edit { changes: ChangeSet::single(0, 0, "\r\n\r\n"), selections: SelectionSet::caret(4) };
        e.apply(&edit, &SelectionSet::caret(0), EditGroup::Standalone).unwrap();
        assert_eq!(e.text(), "\n\n");
        e.undo();
        assert_eq!(e.redo(), Some(SelectionSet::caret(2)));
    }

    #[test]
    fn typing_after_a_caret_move_starts_a_new_undo_group() {
        let mut d = Document::from_text("0123456789");
        let e1 = ins(&d, 0, "a");
        d.apply(&e1, &SelectionSet::caret(0), EditGroup::Typing).unwrap();
        // The caret moved to 6 without `break_undo_group`: `before` no longer matches.
        let e2 = ins(&d, 6, "b");
        d.apply(&e2, &SelectionSet::caret(6), EditGroup::Typing).unwrap();
        assert_eq!(d.text(), "a01234b56789");
        d.undo();
        assert_eq!(d.text(), "a0123456789");
        d.undo();
        assert_eq!(d.text(), "0123456789");
    }

    #[test]
    fn typing_over_a_closer_keeps_the_undo_group() {
        let mut d = Document::from_text("");
        let mut sels = SelectionSet::caret(0);
        let cfg = crate::ops::OpsConfig::default();
        for ch in "(x)y".chars() {
            let edit = crate::ops::type_char(&d, &sels, ch, &cfg);
            d.apply(&edit, &sels, EditGroup::Typing).unwrap();
            sels = edit.selections;
        }
        assert_eq!(d.text(), "(x)y");
        assert_eq!(d.undo(), Some(SelectionSet::caret(0)));
        assert_eq!(d.text(), "");
        assert_eq!(d.redo(), Some(SelectionSet::caret(4)));
    }

    #[test]
    fn standalone_edits_do_not_merge_and_redo_branch_is_dropped() {
        let mut d = Document::from_text("");
        let e1 = ins(&d, 0, "a");
        d.apply(&e1, &SelectionSet::caret(0), EditGroup::Typing).unwrap();
        let e2 = ins(&d, 1, "b");
        d.apply(&e2, &e1.selections, EditGroup::Standalone).unwrap();
        let e3 = ins(&d, 2, "c");
        d.apply(&e3, &e2.selections, EditGroup::Typing).unwrap();
        d.undo();
        assert_eq!(d.text(), "ab");
        assert!(d.can_redo());
        let e4 = ins(&d, 2, "d");
        d.apply(&e4, &e2.selections, EditGroup::Typing).unwrap();
        assert!(!d.can_redo());
        assert_eq!(d.text(), "abd");
        d.undo();
        d.undo();
        d.undo();
        assert_eq!(d.text(), "");
        assert!(!d.can_undo());
    }

    #[test]
    fn unicode_line_separators_are_content() {
        // Only \n ends a line, as in rustc and rust-analyzer (ropey's unicode_lines is off).
        let d = Document::from_text("a\u{2028}bc\u{85}d\u{c}\ne");
        assert_eq!(d.len_lines(), 2);
        assert_eq!(d.line_len(0), 7);
        assert_eq!(d.line_text(1).as_deref(), Some("e"));
        assert_eq!(d.offset_to_position(d.len_chars()), Position::new(1, 1));
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut d = Document::from_text("one\ntwo");
        d.replace_all("x\r\ny", &SelectionSet::caret(99)).unwrap();
        assert_eq!(d.text(), "x\ny");
        d.replace_all("x\ny", &SelectionSet::caret(0)).unwrap();
        assert_eq!(d.undo(), Some(SelectionSet::caret(99)));
        assert_eq!(d.text(), "one\ntwo");
        assert!(!d.can_undo());
    }

    #[test]
    fn positions_clamp() {
        let d = Document::from_text("ab\ncde");
        assert_eq!(d.position_to_offset(Position::new(1, 99)), 6);
        assert_eq!(d.position_to_offset(Position::new(99, 0)), 3);
        assert_eq!(d.offset_to_position(99), Position::new(1, 3));
    }
}
