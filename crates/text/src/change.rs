use serde::{Deserialize, Serialize};

use crate::{Result, TextError};

/// Replace chars `start..end` (pre-change coordinates) with `text`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Change {
    pub fn new(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self { start, end, text: text.into() }
    }

    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self::new(at, at, text)
    }

    pub fn delete(start: usize, end: usize) -> Self {
        Self::new(start, end, String::new())
    }

    fn inserted_chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// Which side a position at an insertion point sticks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assoc {
    Before,
    After,
}

/// A set of non-overlapping changes, sorted by start, all in pre-change coordinates.
///
/// Deserialization validates like [`ChangeSet::new`], so a decoded set always upholds the
/// invariants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ChangeSetRepr")]
pub struct ChangeSet {
    changes: Vec<Change>,
}

/// Wire shape of [`ChangeSet`] (same JSON as the derived form), validated on the way in.
#[derive(Deserialize)]
struct ChangeSetRepr {
    changes: Vec<Change>,
}

impl TryFrom<ChangeSetRepr> for ChangeSet {
    type Error = TextError;

    fn try_from(repr: ChangeSetRepr) -> Result<Self> {
        Self::new(repr.changes)
    }
}

impl ChangeSet {
    /// Sorts the changes; fails if any two overlap (two insertions at one point are allowed and keep order).
    pub fn new(mut changes: Vec<Change>) -> Result<Self> {
        changes.retain(|c| !(c.start == c.end && c.text.is_empty()));
        for c in &changes {
            if c.end < c.start {
                return Err(TextError::Overlap);
            }
        }
        changes.sort_by_key(|c| (c.start, c.end));
        for w in changes.windows(2) {
            if let [a, b] = w
                && b.start < a.end
            {
                return Err(TextError::Overlap);
            }
        }
        Ok(Self { changes })
    }

    pub fn single(start: usize, end: usize, text: impl Into<String>) -> Self {
        let c = Change::new(start.min(end), end.max(start), text);
        if c.start == c.end && c.text.is_empty() { Self::empty() } else { Self { changes: vec![c] } }
    }

    pub fn empty() -> Self {
        Self { changes: Vec::new() }
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Map a pre-change position to post-change coordinates.
    pub fn map_pos(&self, pos: usize, assoc: Assoc) -> usize {
        let mut delta: isize = 0;
        for c in &self.changes {
            let ins = c.inserted_chars() as isize;
            if c.start == c.end {
                // Pure insertion: a position at the insertion point follows `assoc`.
                if pos < c.start || (pos == c.start && assoc == Assoc::Before) {
                    break;
                }
                delta += ins;
                continue;
            }
            if pos <= c.start {
                break;
            }
            if pos < c.end {
                // Strictly inside a replaced range: snap to the start or end of the new text.
                let base = (c.start as isize + delta).max(0) as usize;
                return match assoc {
                    Assoc::Before => base,
                    Assoc::After => base + ins.max(0) as usize,
                };
            }
            delta += ins - c.end.saturating_sub(c.start) as isize;
        }
        (pos as isize + delta).max(0) as usize
    }

    /// Length in chars of the document after applying to a document of `len_before` chars.
    pub fn len_after(&self, len_before: usize) -> usize {
        let mut len = len_before as isize;
        for c in &self.changes {
            len += c.inserted_chars() as isize - c.end.saturating_sub(c.start) as isize;
        }
        len.max(0) as usize
    }

    /// Validate against a document length: every change in bounds, well-formed, sorted and
    /// non-overlapping.
    pub fn check_bounds(&self, len: usize) -> Result<()> {
        let mut prev_end = 0usize;
        for c in &self.changes {
            if c.end > len || c.start > len {
                return Err(TextError::OutOfBounds { start: c.start, end: c.end, len });
            }
            if c.end < c.start || c.start < prev_end {
                return Err(TextError::Overlap);
            }
            prev_end = c.end;
        }
        Ok(())
    }

    /// The same changes with `\r\n` and `\r` in inserted text turned into `\n`.
    pub(crate) fn with_normalized_newlines(&self) -> Self {
        if !self.changes.iter().any(|c| c.text.contains('\r')) {
            return self.clone();
        }
        let changes = self
            .changes
            .iter()
            .map(|c| Change::new(c.start, c.end, crate::util::normalize_newlines(&c.text)))
            .collect();
        Self { changes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, Edit, EditGroup, SelectionSet};

    #[test]
    fn new_sorts_drops_empty_and_rejects_overlap() {
        let cs = ChangeSet::new(vec![Change::insert(5, "x"), Change::delete(0, 2), Change::insert(3, "")]).unwrap();
        assert_eq!(cs.changes(), &[Change::delete(0, 2), Change::insert(5, "x")]);
        assert_eq!(ChangeSet::new(vec![Change::delete(0, 3), Change::delete(2, 4)]), Err(TextError::Overlap));
        assert_eq!(ChangeSet::new(vec![Change::new(3, 1, "")]), Err(TextError::Overlap));
        // Two insertions at one point keep their order; touching ranges are fine.
        let cs = ChangeSet::new(vec![Change::insert(2, "a"), Change::insert(2, "b"), Change::delete(2, 4)]).unwrap();
        assert_eq!(cs.changes().len(), 3);
        assert_eq!(cs.changes()[1].text, "b");
    }

    #[test]
    fn map_pos_insertions_follow_assoc() {
        let cs = ChangeSet::single(2, 2, "xy");
        assert_eq!(cs.map_pos(1, Assoc::After), 1);
        assert_eq!(cs.map_pos(2, Assoc::Before), 2);
        assert_eq!(cs.map_pos(2, Assoc::After), 4);
        assert_eq!(cs.map_pos(3, Assoc::Before), 5);
    }

    #[test]
    fn map_pos_replacements_and_deletions() {
        let cs = ChangeSet::single(2, 5, "X");
        assert_eq!(cs.map_pos(2, Assoc::After), 2);
        assert_eq!(cs.map_pos(3, Assoc::Before), 2);
        assert_eq!(cs.map_pos(3, Assoc::After), 3);
        assert_eq!(cs.map_pos(5, Assoc::Before), 3);
        assert_eq!(cs.map_pos(9, Assoc::Before), 7);
        let multi =
            ChangeSet::new(vec![Change::insert(0, "ab"), Change::delete(1, 3), Change::new(4, 5, "xyz")]).unwrap();
        assert_eq!(multi.map_pos(0, Assoc::After), 2);
        assert_eq!(multi.map_pos(2, Assoc::Before), 3);
        assert_eq!(multi.map_pos(3, Assoc::After), 3);
        assert_eq!(multi.map_pos(6, Assoc::After), 8);
        assert_eq!(multi.len_after(6), 8);
    }

    #[test]
    fn map_pos_matches_applying_the_changes() {
        // A marker char survives an edit that does not touch it; its new index is map_pos.
        let text = "0123456789";
        let cs = ChangeSet::new(vec![Change::insert(1, "ab"), Change::new(3, 5, "Z"), Change::delete(7, 8)]).unwrap();
        let mut doc = Document::from_text(text);
        doc.apply(
            &Edit { changes: cs.clone(), selections: SelectionSet::caret(0) },
            &SelectionSet::caret(0),
            EditGroup::Standalone,
        )
        .unwrap();
        let after: Vec<char> = doc.text().chars().collect();
        for (i, c) in text.chars().enumerate() {
            if [3, 4, 7].contains(&i) {
                continue;
            }
            assert_eq!(after.get(cs.map_pos(i, Assoc::After)), Some(&c), "char {c} at {i}");
        }
        assert_eq!(cs.len_after(text.len()), after.len());
    }

    #[test]
    fn check_bounds_rejects_malformed_sets() {
        assert!(ChangeSet::single(0, 3, "").check_bounds(3).is_ok());
        assert!(matches!(ChangeSet::single(0, 4, "").check_bounds(3), Err(TextError::OutOfBounds { .. })));
        // Only reachable through the private field (or, before the fix, deserialization).
        let reversed = ChangeSet { changes: vec![Change::new(3, 1, "")] };
        assert_eq!(reversed.check_bounds(5), Err(TextError::Overlap));
        assert_eq!(reversed.len_after(5), 5);
        let unsorted = ChangeSet { changes: vec![Change::delete(2, 3), Change::delete(0, 1)] };
        assert_eq!(unsorted.check_bounds(5), Err(TextError::Overlap));
        let mut doc = Document::from_text("hello");
        let edit = Edit { changes: unsorted, selections: SelectionSet::caret(0) };
        assert!(doc.apply(&edit, &SelectionSet::caret(0), EditGroup::Standalone).is_err());
        assert_eq!(doc.text(), "hello");
        assert!(!doc.can_undo());
    }

    #[test]
    fn deserialization_validates() {
        let ok = ChangeSet::try_from(ChangeSetRepr { changes: vec![Change::insert(4, "b"), Change::insert(1, "a")] });
        assert_eq!(ok.map(|c| c.changes().len()), Ok(2));
        let bad = ChangeSet::try_from(ChangeSetRepr { changes: vec![Change::delete(0, 3), Change::delete(1, 2)] });
        assert_eq!(bad, Err(TextError::Overlap));
    }

    #[test]
    fn normalized_newlines() {
        let cs = ChangeSet::new(vec![Change::insert(0, "a\r\nb"), Change::insert(1, "\r")]).unwrap();
        let n = cs.with_normalized_newlines();
        assert_eq!(n.changes()[0].text, "a\nb");
        assert_eq!(n.changes()[1].text, "\n");
    }
}
