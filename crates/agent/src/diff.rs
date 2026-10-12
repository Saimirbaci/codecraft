//! Line diffs for review and approval views (and, later, VCS), built on `similar`.
//!
//! [`diff`] groups changes into hunks with context; changed line pairs carry word-level
//! emphasis. [`side_by_side`] pairs a hunk's deletions with its insertions for a two-column view.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, TextDiff};

/// Inline (word) emphasis is computed only for replaced blocks up to this many lines.
const MAX_INLINE_LINES: usize = 60;
/// Longest a line diff may take before falling back to a coarser result.
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LineTag {
    Equal,
    Insert,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub tag: LineTag,
    /// 1-based line number in the old text.
    pub old: Option<usize>,
    /// 1-based line number in the new text.
    pub new: Option<usize>,
    /// The line without its line break.
    pub text: String,
    /// Word-level pieces `(emphasized, text)`; empty when the whole line is one piece.
    pub segments: Vec<(bool, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// `@@ -a,b +c,d @@`
    pub fn header(&self) -> String {
        format!("@@ -{},{} +{},{} @@", self.old_start, self.old_len, self.new_start, self.new_len)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

fn strip_eol(s: &str) -> String {
    s.strip_suffix('\n').map(|s| s.strip_suffix('\r').unwrap_or(s)).unwrap_or(s).to_string()
}

/// Hunks with `context` unchanged lines around each change.
pub fn diff(old: &str, new: &str, context: usize) -> Vec<Hunk> {
    let td = TextDiff::configure().algorithm(Algorithm::Patience).timeout(DIFF_TIMEOUT).diff_lines(old, new);
    let mut hunks = Vec::new();
    for group in td.grouped_ops(context) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let mut lines = Vec::new();
        for op in &group {
            let small = op.old_range().len() <= MAX_INLINE_LINES && op.new_range().len() <= MAX_INLINE_LINES;
            if small && op.tag() == similar::DiffTag::Replace {
                for ch in td.iter_inline_changes(op) {
                    let segments: Vec<(bool, String)> = ch
                        .iter_strings_lossy()
                        .map(|(emph, s)| (emph, strip_eol(&s)))
                        .filter(|(_, s)| !s.is_empty())
                        .collect();
                    let text: String = segments.iter().map(|(_, s)| s.as_str()).collect();
                    lines.push(DiffLine {
                        tag: tag(ch.tag()),
                        old: ch.old_index().map(|i| i + 1),
                        new: ch.new_index().map(|i| i + 1),
                        text,
                        segments,
                    });
                }
            } else {
                for ch in td.iter_changes(op) {
                    lines.push(DiffLine {
                        tag: tag(ch.tag()),
                        old: ch.old_index().map(|i| i + 1),
                        new: ch.new_index().map(|i| i + 1),
                        text: strip_eol(ch.value()),
                        segments: Vec::new(),
                    });
                }
            }
        }
        hunks.push(Hunk {
            old_start: old_range.start + usize::from(!old_range.is_empty()),
            old_len: old_range.len(),
            new_start: new_range.start + usize::from(!new_range.is_empty()),
            new_len: new_range.len(),
            lines,
        });
    }
    hunks
}

fn tag(t: ChangeTag) -> LineTag {
    match t {
        ChangeTag::Equal => LineTag::Equal,
        ChangeTag::Insert => LineTag::Insert,
        ChangeTag::Delete => LineTag::Delete,
    }
}

pub fn stats(hunks: &[Hunk]) -> DiffStats {
    let mut s = DiffStats::default();
    for l in hunks.iter().flat_map(|h| &h.lines) {
        match l.tag {
            LineTag::Insert => s.added += 1,
            LineTag::Delete => s.removed += 1,
            LineTag::Equal => {}
        }
    }
    s
}

/// One row of a two-column view; a side is `None` where the other side has extra lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideRow<'a> {
    pub left: Option<&'a DiffLine>,
    pub right: Option<&'a DiffLine>,
}

/// Pair each run of deletions with the insertions that follow it.
pub fn side_by_side(hunk: &Hunk) -> Vec<SideRow<'_>> {
    let mut rows = Vec::new();
    let lines = &hunk.lines;
    let mut i = 0;
    while let Some(line) = lines.get(i) {
        match line.tag {
            LineTag::Equal => {
                rows.push(SideRow { left: Some(line), right: Some(line) });
                i += 1;
            }
            LineTag::Delete | LineTag::Insert => {
                let start = i;
                while lines.get(i).is_some_and(|l| l.tag == LineTag::Delete) {
                    i += 1;
                }
                let dels = lines.get(start..i).unwrap_or_default();
                let ins_start = i;
                while lines.get(i).is_some_and(|l| l.tag == LineTag::Insert) {
                    i += 1;
                }
                let ins = lines.get(ins_start..i).unwrap_or_default();
                for k in 0..dels.len().max(ins.len()) {
                    rows.push(SideRow { left: dels.get(k), right: ins.get(k) });
                }
            }
        }
    }
    rows
}

/// A `diff -u` style text (for copying and for agents reading command results).
pub fn unified_text(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
    let hunks = diff(old, new, 3);
    if hunks.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {old_name}\n+++ {new_name}\n");
    for h in &hunks {
        out.push_str(&h.header());
        out.push('\n');
        for l in &h.lines {
            out.push(match l.tag {
                LineTag::Equal => ' ',
                LineTag::Insert => '+',
                LineTag::Delete => '-',
            });
            out.push_str(&l.text);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn hunks_and_stats() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nC\nd\ne\nf\ng\nh\ni\n";
        let h = diff(old, new, 1);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].header(), "@@ -2,3 +2,3 @@");
        assert_eq!(stats(&h), DiffStats { added: 2, removed: 1 });
        let del = h[0].lines.iter().find(|l| l.tag == LineTag::Delete).unwrap();
        assert_eq!((del.old, del.new, del.text.as_str()), (Some(3), None, "c"));
    }

    #[test]
    fn inline_emphasis_on_changed_words() {
        let h = diff("let x = 1;\n", "let x = 2;\n", 3);
        let ins = h[0].lines.iter().find(|l| l.tag == LineTag::Insert).unwrap();
        assert_eq!(ins.text, "let x = 2;");
        assert!(ins.segments.iter().any(|(e, s)| *e && s.contains('2')));
        assert!(ins.segments.iter().any(|(e, _)| !*e));
    }

    #[test]
    fn side_by_side_pairs_changes() {
        let h = diff("a\nb\nc\n", "a\nB\nB2\nc\n", 3);
        let rows = side_by_side(&h[0]);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].left.map(|l| l.text.as_str()), Some("b"));
        assert_eq!(rows[1].right.map(|l| l.text.as_str()), Some("B"));
        assert!(rows[2].left.is_none());
        assert_eq!(rows[2].right.map(|l| l.text.as_str()), Some("B2"));
    }

    #[test]
    fn edge_cases() {
        assert!(diff("same\n", "same\n", 3).is_empty());
        let added = diff("", "x\ny", 3);
        assert_eq!(added[0].header(), "@@ -0,0 +1,2 @@");
        let removed = diff("x\r\ny\r\n", "", 3);
        assert_eq!(removed[0].lines[0].text, "x");
        assert_eq!(unified_text("a\n", "b\n", "a/f", "b/f"), "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n+b\n");
        // Non-ASCII and no trailing newline.
        let h = diff("héllo wörld", "héllo wörld!", 0);
        assert_eq!(stats(&h), DiffStats { added: 1, removed: 1 });
    }
}
