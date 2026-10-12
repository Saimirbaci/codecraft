//! Line-level changes of a document against a base text (its `HEAD` content), for the editor's
//! gutter change markers and per-hunk rollback.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffOp, TextDiff};

/// A diff that takes longer than this falls back to a coarser result.
const DIFF_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LineChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A run of changed lines. Line numbers are 0-based; ranges are half-open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineChange {
    pub kind: LineChangeKind,
    /// Lines of the current text (`start == end` for a deletion: the lines were removed just
    /// before line `start`).
    pub start: usize,
    pub end: usize,
    /// Lines of the base text.
    pub old_start: usize,
    pub old_end: usize,
}

impl LineChange {
    /// Whether the marker for this change covers `line` of the current text (a deletion marks the
    /// line after the removed ones, or the last line at the end of the file).
    pub fn covers(&self, line: usize) -> bool {
        if self.start == self.end {
            line == self.start || line + 1 == self.start
        } else {
            (self.start..self.end).contains(&line)
        }
    }
}

/// The changes turning `base` into `current` (both with `\n` line breaks).
pub fn line_changes(base: &str, current: &str) -> Vec<LineChange> {
    if base == current {
        return Vec::new();
    }
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).timeout(DIFF_TIMEOUT).diff_lines(base, current);
    let mut out: Vec<LineChange> = Vec::new();
    // Adjacent non-equal ops form one change.
    let mut pending: Option<(usize, usize, usize, usize)> = None;
    let flush = |p: Option<(usize, usize, usize, usize)>, out: &mut Vec<LineChange>| {
        let Some((os, oe, ns, ne)) = p else { return };
        let kind = match (oe > os, ne > ns) {
            (false, true) => LineChangeKind::Added,
            (true, false) => LineChangeKind::Deleted,
            _ => LineChangeKind::Modified,
        };
        out.push(LineChange { kind, start: ns, end: ne, old_start: os, old_end: oe });
    };
    for op in diff.ops() {
        if let DiffOp::Equal { .. } = op {
            flush(pending.take(), &mut out);
            continue;
        }
        let (o, n) = (op.old_range(), op.new_range());
        pending = Some(match pending {
            Some((os, oe, ns, ne)) => (os.min(o.start), oe.max(o.end), ns.min(n.start), ne.max(n.end)),
            None => (o.start, o.end, n.start, n.end),
        });
    }
    flush(pending.take(), &mut out);
    out
}

/// The base lines a change replaced (with their line breaks), i.e. what rolling it back puts in
/// place of the current lines `start..end`.
pub fn base_text(base: &str, change: &LineChange) -> String {
    base.split_inclusive('\n').skip(change.old_start).take(change.old_end.saturating_sub(change.old_start)).collect()
}

/// `current` with one change rolled back to its base content.
pub fn revert(base: &str, current: &str, change: &LineChange) -> String {
    let mut out = String::with_capacity(current.len());
    let lines: Vec<&str> = current.split_inclusive('\n').collect();
    for l in lines.iter().take(change.start) {
        out.push_str(l);
    }
    out.push_str(&base_text(base, change));
    for l in lines.iter().skip(change.end) {
        out.push_str(l);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn kinds(base: &str, cur: &str) -> Vec<(LineChangeKind, usize, usize)> {
        line_changes(base, cur).iter().map(|c| (c.kind, c.start, c.end)).collect()
    }

    #[test]
    fn added_modified_deleted() {
        let base = "a\nb\nc\nd\n";
        assert!(line_changes(base, base).is_empty());
        assert_eq!(kinds(base, "a\nb\nX\nc\nd\n"), vec![(LineChangeKind::Added, 2, 3)]);
        assert_eq!(kinds(base, "a\nB\nc\nd\n"), vec![(LineChangeKind::Modified, 1, 2)]);
        assert_eq!(kinds(base, "a\nd\n"), vec![(LineChangeKind::Deleted, 1, 1)]);
        assert_eq!(kinds(base, "a\nb\nc\n"), vec![(LineChangeKind::Deleted, 3, 3)]);
        assert_eq!(kinds("", "x\ny\n"), vec![(LineChangeKind::Added, 0, 2)]);
        // A replaced block with extra lines is one modification.
        assert_eq!(kinds(base, "a\nB\nB2\nc\nd\n"), vec![(LineChangeKind::Modified, 1, 3)]);
        // Missing final newline modifies the last line.
        assert_eq!(kinds(base, "a\nb\nc\nd"), vec![(LineChangeKind::Modified, 3, 4)]);
    }

    #[test]
    fn revert_restores_the_base() {
        let base = "a\nb\nc\nd\n";
        for cur in ["a\nb\nX\nc\nd\n", "a\nB\nc\nd\n", "a\nd\n", "a\nb\nc\n", "a\nb\nc\nd", "", "z\n"] {
            let mut text = cur.to_string();
            // Roll back from the last change so earlier line numbers stay valid.
            for ch in line_changes(base, cur).iter().rev() {
                text = revert(base, &text, ch);
            }
            assert_eq!(text, base, "from {cur:?}");
        }
        let ch = &line_changes(base, "a\nd\n")[0];
        assert_eq!(base_text(base, ch), "b\nc\n");
        assert!(ch.covers(0) && ch.covers(1) && !ch.covers(2));
    }

    #[test]
    fn hostile_ranges_do_not_panic() {
        let ch = LineChange { kind: LineChangeKind::Modified, start: 50, end: 10, old_start: 99, old_end: 3 };
        assert_eq!(base_text("a\n", &ch), "");
        let _ = revert("a\n", "b\n", &ch);
    }
}
