//! Split editors: a binary tree of splitters whose leaves are editor groups (each with its own
//! tab bar and active tab), like RustRover's Window › Editor Tabs › Split Right/Down.

use serde::{Deserialize, Serialize};

use crate::EditorId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GroupId(pub u64);

/// How a splitter arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SplitAxis {
    /// Side by side (Split Right).
    Horizontal,
    /// Stacked (Split Down).
    Vertical,
}

impl SplitAxis {
    pub fn flipped(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

/// One editor group: its tabs in display order and the selected one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorGroup {
    pub tabs: Vec<EditorId>,
    pub active: Option<EditorId>,
}

/// Smallest share a splitter gives either side (also the share of a minimized side).
pub const MIN_RATIO: f32 = 0.05;

/// The split layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SplitNode {
    Group(GroupId),
    Split {
        axis: SplitAxis,
        /// Share of the first child, in `MIN_RATIO..=1 - MIN_RATIO`.
        ratio: f32,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

impl Default for SplitNode {
    fn default() -> Self {
        Self::Group(GroupId::default())
    }
}

/// A side of the focused editor that Stretch Editor to … moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stretch {
    Top,
    Left,
    Bottom,
    Right,
}

pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() { r.clamp(MIN_RATIO, 1.0 - MIN_RATIO) } else { 0.5 }
}

impl SplitNode {
    /// Groups in display order (left to right, top to bottom).
    pub fn leaves(&self) -> Vec<GroupId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<GroupId>) {
        match self {
            Self::Group(g) => out.push(*g),
            Self::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    pub fn contains(&self, g: GroupId) -> bool {
        match self {
            Self::Group(x) => *x == g,
            Self::Split { first, second, .. } => first.contains(g) || second.contains(g),
        }
    }

    pub fn is_split(&self) -> bool {
        matches!(self, Self::Split { .. })
    }

    /// Replace the leaf `g` with `f(leaf)`; false if `g` is not in the tree.
    pub(crate) fn replace_leaf(&mut self, g: GroupId, f: impl FnOnce(SplitNode) -> SplitNode) -> bool {
        let Some(slot) = self.leaf_slot(g) else { return false };
        let leaf = std::mem::take(slot);
        *slot = f(leaf);
        true
    }

    fn leaf_slot(&mut self, g: GroupId) -> Option<&mut SplitNode> {
        match self {
            Self::Group(x) if *x == g => Some(self),
            Self::Group(_) => None,
            Self::Split { first, second, .. } => {
                if first.contains(g) {
                    first.leaf_slot(g)
                } else {
                    second.leaf_slot(g)
                }
            }
        }
    }

    /// Remove the leaf `g`; its sibling takes the parent splitter's place. The last leaf stays.
    pub(crate) fn remove_leaf(&mut self, g: GroupId) -> bool {
        if !self.is_split() || !self.contains(g) {
            return false;
        }
        let tree = std::mem::take(self);
        *self = tree.without(g).unwrap_or_default();
        true
    }

    fn without(self, g: GroupId) -> Option<SplitNode> {
        match self {
            Self::Group(x) if x == g => None,
            leaf @ Self::Group(_) => Some(leaf),
            Self::Split { axis, ratio, first, second } => match (first.without(g), second.without(g)) {
                (Some(a), Some(b)) => Some(Self::Split { axis, ratio, first: Box::new(a), second: Box::new(b) }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// The splitter whose direct child contains `g`, innermost first along the path from `g` to
    /// the root: `(axis, ratio, g is in the first child)`.
    fn ancestors_mut(&mut self, g: GroupId) -> Vec<(&mut SplitAxis, &mut f32, bool)> {
        let mut out = Vec::new();
        let mut node = self;
        loop {
            match node {
                Self::Group(_) => break,
                Self::Split { axis, ratio, first, second } => {
                    let in_first = first.contains(g);
                    if !in_first && !second.contains(g) {
                        break;
                    }
                    out.push((axis, ratio, in_first));
                    node = if in_first { first.as_mut() } else { second.as_mut() };
                }
            }
        }
        out.reverse();
        out
    }

    /// Split the leaf `g`: it becomes the first child, `new` the second.
    pub(crate) fn split_leaf(&mut self, g: GroupId, new: GroupId, axis: SplitAxis) -> bool {
        self.replace_leaf(g, |leaf| Self::Split {
            axis,
            ratio: 0.5,
            first: Box::new(leaf),
            second: Box::new(Self::Group(new)),
        })
    }

    /// Change Splitter Orientation: flip the splitter directly around `g`.
    pub(crate) fn flip_parent(&mut self, g: GroupId) -> bool {
        match self.ancestors_mut(g).into_iter().next() {
            Some((axis, _, _)) => {
                *axis = axis.flipped();
                true
            }
            None => false,
        }
    }

    /// Whether every splitter on the way to `g` gives it the largest share.
    pub(crate) fn is_maximized(&mut self, g: GroupId) -> bool {
        let anc = self.ancestors_mut(g);
        !anc.is_empty()
            && anc.into_iter().all(|(_, r, first)| {
                let mine = if first { *r } else { 1.0 - *r };
                mine >= 1.0 - MIN_RATIO - 1e-3
            })
    }

    pub(crate) fn maximize(&mut self, g: GroupId) {
        for (_, r, first) in self.ancestors_mut(g) {
            *r = if first { 1.0 - MIN_RATIO } else { MIN_RATIO };
        }
    }

    /// Equal sizes: runs of splitters along one axis share their space evenly between leaves.
    pub(crate) fn normalize(&mut self) {
        if let Self::Split { axis, ratio, first, second } = self {
            let (a, b) = (first.weight(*axis), second.weight(*axis));
            *ratio = clamp_ratio(a as f32 / (a + b).max(1) as f32);
            first.normalize();
            second.normalize();
        }
    }

    fn weight(&self, along: SplitAxis) -> usize {
        match self {
            Self::Split { axis, first, second, .. } if *axis == along => first.weight(along) + second.weight(along),
            _ => 1,
        }
    }

    /// Stretch Editor to …: move the nearest matching splitter edge of `g` by `step`.
    pub(crate) fn stretch(&mut self, g: GroupId, side: Stretch, step: f32) -> bool {
        let (want_axis, want_first) = match side {
            Stretch::Right => (SplitAxis::Horizontal, true),
            Stretch::Left => (SplitAxis::Horizontal, false),
            Stretch::Bottom => (SplitAxis::Vertical, true),
            Stretch::Top => (SplitAxis::Vertical, false),
        };
        for (axis, r, first) in self.ancestors_mut(g) {
            if *axis == want_axis && first == want_first {
                *r = clamp_ratio(if first { *r + step } else { *r - step });
                return true;
            }
        }
        false
    }

    /// Splitters in pre-order (the index the UI uses to drag one).
    pub fn splitter_count(&self) -> usize {
        match self {
            Self::Group(_) => 0,
            Self::Split { first, second, .. } => 1 + first.splitter_count() + second.splitter_count(),
        }
    }

    /// Set the ratio of the `index`th splitter in pre-order; false if there is none.
    pub(crate) fn set_ratio(&mut self, index: usize, value: f32) -> bool {
        let mut i = index;
        self.set_ratio_rec(&mut i, clamp_ratio(value))
    }

    fn set_ratio_rec(&mut self, i: &mut usize, value: f32) -> bool {
        match self {
            Self::Group(_) => false,
            Self::Split { ratio, first, second, .. } => {
                if *i == 0 {
                    *ratio = value;
                    return true;
                }
                *i -= 1;
                first.set_ratio_rec(i, value) || second.set_ratio_rec(i, value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(n: u64) -> GroupId {
        GroupId(n)
    }

    #[test]
    fn split_remove_and_leaves() {
        let mut t = SplitNode::default();
        assert!(t.split_leaf(g(0), g(1), SplitAxis::Horizontal));
        assert!(t.split_leaf(g(1), g(2), SplitAxis::Vertical));
        assert_eq!(t.leaves(), vec![g(0), g(1), g(2)]);
        assert_eq!(t.splitter_count(), 2);
        assert!(!t.split_leaf(g(9), g(3), SplitAxis::Vertical));
        assert!(t.remove_leaf(g(1)));
        assert_eq!(t.leaves(), vec![g(0), g(2)]);
        assert!(t.remove_leaf(g(0)));
        assert_eq!(t, SplitNode::Group(g(2)));
        // The last group is never removed.
        assert!(!t.remove_leaf(g(2)));
        assert_eq!(t.leaves(), vec![g(2)]);
    }

    #[test]
    fn maximize_normalize_flip_and_stretch() {
        let mut t = SplitNode::default();
        t.split_leaf(g(0), g(1), SplitAxis::Horizontal);
        t.split_leaf(g(1), g(2), SplitAxis::Horizontal);
        assert!(!t.is_maximized(g(2)));
        t.maximize(g(2));
        assert!(t.is_maximized(g(2)));
        assert!(!t.is_maximized(g(0)));
        t.normalize();
        // Three side-by-side groups get a third each.
        let SplitNode::Split { ratio, .. } = &t else { panic!() };
        assert!((ratio - 1.0 / 3.0).abs() < 1e-4);
        assert!(t.flip_parent(g(2)));
        let SplitNode::Split { second, .. } = &t else { panic!() };
        assert!(matches!(**second, SplitNode::Split { axis: SplitAxis::Vertical, .. }));
        // g(0) is the first child of a horizontal splitter: it can grow to the right only.
        assert!(t.stretch(g(0), Stretch::Right, 0.1));
        assert!(!t.stretch(g(0), Stretch::Left, 0.1));
        assert!(t.stretch(g(2), Stretch::Top, 0.1));
        assert!(!t.stretch(g(2), Stretch::Bottom, 0.1));
        for _ in 0..50 {
            t.stretch(g(0), Stretch::Right, 0.1);
        }
        let SplitNode::Split { ratio, .. } = &t else { panic!() };
        assert!(*ratio <= 1.0 - MIN_RATIO);
    }

    #[test]
    fn set_ratio_by_preorder_index() {
        let mut t = SplitNode::default();
        t.split_leaf(g(0), g(1), SplitAxis::Horizontal);
        t.split_leaf(g(0), g(2), SplitAxis::Vertical);
        assert!(t.set_ratio(1, 0.25));
        assert!(!t.set_ratio(2, 0.25));
        assert!(t.set_ratio(0, f32::NAN));
        let SplitNode::Split { ratio, first, .. } = &t else { panic!() };
        assert_eq!(*ratio, 0.5);
        assert!(matches!(**first, SplitNode::Split { ratio, .. } if ratio == 0.25));
    }
}
