use serde::{Deserialize, Serialize};

use crate::change::{Assoc, ChangeSet};

/// One selection (or caret when `anchor == head`), in char offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Sticky column for vertical motion (chars from line start), kept across Up/Down.
    #[serde(default)]
    pub goal_column: Option<usize>,
}

impl Selection {
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head, goal_column: None }
    }

    pub fn caret(at: usize) -> Self {
        Self::new(at, at)
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.start()..self.end()
    }

    /// Clamp both ends to `len` chars.
    pub fn clamp(&self, len: usize) -> Self {
        Self { anchor: self.anchor.min(len), head: self.head.min(len), goal_column: self.goal_column }
    }

    /// Map through changes; an empty caret sticks after inserted text.
    pub fn map(&self, changes: &ChangeSet) -> Self {
        if self.is_empty() {
            let p = changes.map_pos(self.head, Assoc::After);
            return Self::caret(p);
        }
        let forward = self.head >= self.anchor;
        let (s, e) = (changes.map_pos(self.start(), Assoc::After), changes.map_pos(self.end(), Assoc::Before));
        let e = e.max(s);
        if forward { Self::new(s, e) } else { Self::new(e, s) }
    }
}

/// A non-empty, sorted, non-overlapping set of selections with one primary.
///
/// Deserialization goes through [`SelectionSet::from_vec`], so a decoded set is normalized too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "SelectionSetRepr")]
pub struct SelectionSet {
    ranges: Vec<Selection>,
    primary: usize,
}

/// Wire shape of [`SelectionSet`] (same JSON as the derived form), normalized on the way in.
#[derive(Deserialize)]
struct SelectionSetRepr {
    ranges: Vec<Selection>,
    #[serde(default)]
    primary: usize,
}

impl From<SelectionSetRepr> for SelectionSet {
    fn from(repr: SelectionSetRepr) -> Self {
        Self::from_vec(repr.ranges, repr.primary)
    }
}

impl Default for SelectionSet {
    fn default() -> Self {
        Self::caret(0)
    }
}

impl SelectionSet {
    pub fn single(sel: Selection) -> Self {
        Self { ranges: vec![sel], primary: 0 }
    }

    pub fn caret(at: usize) -> Self {
        Self::single(Selection::caret(at))
    }

    /// Build from any list: sorts, merges overlapping selections and carets that touch a
    /// selection (a caret at either end of a selection, or two carets at one offset), keeps the
    /// primary. Two non-empty selections that only touch (`a.end == b.start`) stay separate, so
    /// adjacent occurrences can be selected. An empty list becomes a caret at 0.
    pub fn from_vec(ranges: Vec<Selection>, primary: usize) -> Self {
        if ranges.is_empty() {
            return Self::default();
        }
        let primary_sel = ranges.get(primary).or(ranges.first()).copied().unwrap_or(Selection::caret(0));
        let mut sorted = ranges;
        sorted.sort_by_key(|s| (s.start(), s.end()));
        let mut merged: Vec<Selection> = Vec::with_capacity(sorted.len());
        for s in sorted {
            if let Some(last) = merged.last_mut() {
                let overlaps = s.start() < last.end();
                let caret_touch = s.start() == last.end() && (s.is_empty() || last.is_empty());
                if overlaps || caret_touch {
                    let start = last.start().min(s.start());
                    let end = last.end().max(s.end());
                    // The direction comes from the first non-empty selection of the two.
                    let forward = if last.is_empty() { s.head >= s.anchor } else { last.head >= last.anchor };
                    let goal_column = last.goal_column.or(s.goal_column);
                    *last = if forward {
                        Selection { anchor: start, head: end, goal_column }
                    } else {
                        Selection { anchor: end, head: start, goal_column }
                    };
                    continue;
                }
            }
            merged.push(s);
        }
        let primary =
            merged.iter().position(|s| s.start() <= primary_sel.start() && primary_sel.end() <= s.end()).unwrap_or(0);
        Self { ranges: merged, primary }
    }

    pub fn ranges(&self) -> &[Selection] {
        &self.ranges
    }

    pub fn primary(&self) -> Selection {
        self.ranges.get(self.primary).copied().unwrap_or(Selection::caret(0))
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Add a selection; it becomes the primary.
    pub fn push(&mut self, sel: Selection) {
        let mut v = std::mem::take(&mut self.ranges);
        v.push(sel);
        let idx = v.len() - 1;
        *self = Self::from_vec(v, idx);
    }

    /// Keep only the primary selection.
    pub fn collapse_to_primary(&self) -> Self {
        Self::single(self.primary())
    }

    pub fn clamp(&self, len: usize) -> Self {
        Self::from_vec(self.ranges.iter().map(|s| s.clamp(len)).collect(), self.primary)
    }

    pub fn map(&self, changes: &ChangeSet) -> Self {
        Self::from_vec(self.ranges.iter().map(|s| s.map(changes)).collect(), self.primary)
    }

    /// Replace every selection through `f`, keeping the primary index.
    pub fn transform(&self, mut f: impl FnMut(Selection) -> Selection) -> Self {
        Self::from_vec(self.ranges.iter().map(|s| f(*s)).collect(), self.primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Change;

    #[test]
    fn from_vec_sorts_and_merges_overlaps() {
        let set = SelectionSet::from_vec(vec![Selection::new(5, 8), Selection::new(0, 2), Selection::new(6, 10)], 0);
        assert_eq!(set.ranges(), &[Selection::new(0, 2), Selection::new(5, 10)]);
        assert_eq!(set.primary_index(), 1);
        let back = SelectionSet::from_vec(vec![Selection::new(4, 1), Selection::new(3, 6)], 1);
        assert_eq!(back.ranges(), &[Selection::new(6, 1)]);
    }

    #[test]
    fn from_vec_merges_carets_touching_a_selection() {
        // Regression: a caret at either end of a selection used to survive next to it.
        let end = SelectionSet::from_vec(vec![Selection::new(2, 5), Selection::caret(5)], 1);
        assert_eq!(end.ranges(), &[Selection::new(2, 5)]);
        assert_eq!(end.primary_index(), 0);
        let start = SelectionSet::from_vec(vec![Selection::caret(2), Selection::new(5, 2)], 0);
        assert_eq!(start.ranges(), &[Selection::new(5, 2)]);
        let carets = SelectionSet::from_vec(vec![Selection::caret(3), Selection::caret(3), Selection::caret(4)], 2);
        assert_eq!(carets.ranges(), &[Selection::caret(3), Selection::caret(4)]);
        assert_eq!(carets.primary_index(), 1);
    }

    #[test]
    fn from_vec_keeps_adjacent_selections_apart() {
        let set = SelectionSet::from_vec(vec![Selection::new(2, 4), Selection::new(0, 2)], 0);
        assert_eq!(set.ranges(), &[Selection::new(0, 2), Selection::new(2, 4)]);
        assert_eq!(set.primary_index(), 1);
    }

    #[test]
    fn from_vec_keeps_goal_column_and_handles_edge_input() {
        let a = Selection { anchor: 3, head: 3, goal_column: Some(7) };
        let set = SelectionSet::from_vec(vec![a, Selection::caret(3)], 1);
        assert_eq!(set.primary().goal_column, Some(7));
        assert_eq!(SelectionSet::from_vec(Vec::new(), 4), SelectionSet::caret(0));
        let set = SelectionSet::from_vec(vec![Selection::caret(1)], 99);
        assert_eq!(set.primary(), Selection::caret(1));
    }

    #[test]
    fn deserialization_normalizes() {
        let repr = SelectionSetRepr {
            ranges: vec![Selection::new(9, 4), Selection::new(5, 6), Selection::caret(1)],
            primary: 7,
        };
        let set = SelectionSet::from(repr);
        assert_eq!(set.ranges(), &[Selection::caret(1), Selection::new(9, 4)]);
        let empty = SelectionSet::from(SelectionSetRepr { ranges: Vec::new(), primary: 0 });
        assert_eq!(empty, SelectionSet::caret(0));
    }

    #[test]
    fn push_clamp_transform() {
        let mut set = SelectionSet::caret(4);
        set.push(Selection::new(0, 2));
        assert_eq!(set.primary(), Selection::new(0, 2));
        assert_eq!(set.len(), 2);
        let clamped = set.clamp(1);
        assert_eq!(clamped.ranges(), &[Selection::new(0, 1)]);
        let moved = set.transform(|s| Selection::new(s.anchor + 1, s.head + 1));
        assert_eq!(moved.ranges(), &[Selection::new(1, 3), Selection::caret(5)]);
        assert_eq!(set.collapse_to_primary(), SelectionSet::single(Selection::new(0, 2)));
    }

    #[test]
    fn map_through_changes() {
        let cs = ChangeSet::new(vec![Change::insert(2, "xx"), Change::delete(5, 7)]).unwrap();
        assert_eq!(Selection::caret(2).map(&cs), Selection::caret(4));
        assert_eq!(Selection::caret(6).map(&cs), Selection::caret(7));
        // Text inserted at either edge of a selection stays outside it.
        assert_eq!(Selection::new(2, 4).map(&cs), Selection::new(4, 6));
        assert_eq!(Selection::new(8, 1).map(&cs), Selection::new(8, 1));
        // A selection whose text is deleted collapses.
        assert_eq!(Selection::new(5, 7).map(&cs), Selection::caret(7));
        let set = SelectionSet::from_vec(vec![Selection::caret(5), Selection::caret(7)], 1).map(&cs);
        assert_eq!(set.ranges(), &[Selection::caret(7)]);
    }
}
