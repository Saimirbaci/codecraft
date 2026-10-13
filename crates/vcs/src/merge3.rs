//! Three-way merge of a conflicted file: the base version and the two sides ("ours" = the
//! current branch, "theirs" = the branch being merged) are split into chunks. Changes made on
//! one side only (or the same on both) merge cleanly; overlapping different changes are
//! conflicts the user resolves chunk by chunk. [`compose`] builds the result, writing standard
//! conflict markers for chunks left unresolved.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::lines::line_changes;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkKind {
    /// Changed on our side only.
    Ours,
    /// Changed on their side only.
    Theirs,
    /// Both sides made the same change.
    Same,
    /// Both sides changed the region differently.
    Conflict,
}

/// One region where at least one side differs from the base. Ranges are 0-based, half-open line
/// ranges of each version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeChunk {
    pub kind: ChunkKind,
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

/// How a chunk is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    /// Keep our lines (Accept Left).
    Ours,
    /// Take their lines (Accept Right).
    Theirs,
    /// Ours, then theirs.
    OursThenTheirs,
    /// Theirs, then ours.
    TheirsThenOurs,
    /// Keep the base lines (ignore both changes).
    Base,
}

impl Resolution {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ours" | "left" | "yours" => Some(Self::Ours),
            "theirs" | "right" => Some(Self::Theirs),
            "both" | "oursThenTheirs" => Some(Self::OursThenTheirs),
            "theirsThenOurs" => Some(Self::TheirsThenOurs),
            "base" | "ignore" => Some(Self::Base),
            _ => None,
        }
    }

    /// The resolution a non-conflicting chunk gets by default.
    pub fn natural(kind: ChunkKind) -> Option<Self> {
        match kind {
            ChunkKind::Ours | ChunkKind::Same => Some(Self::Ours),
            ChunkKind::Theirs => Some(Self::Theirs),
            ChunkKind::Conflict => None,
        }
    }
}

/// A text's lines, each with its line break.
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The lines `range` of `lines` joined (empty when out of range).
pub fn slice(lines: &[&str], range: &Range<usize>) -> String {
    lines.get(range.start.min(lines.len())..range.end.min(lines.len())).unwrap_or_default().concat()
}

#[derive(Clone, Copy, Debug)]
struct Hunk {
    theirs: bool,
    base: (usize, usize),
    side: (usize, usize),
}

/// The chunks of a three-way merge, in order.
pub fn chunks(base: &str, ours: &str, theirs: &str) -> Vec<MergeChunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for (is_theirs, side) in [(false, ours), (true, theirs)] {
        for c in line_changes(base, side) {
            hunks.push(Hunk { theirs: is_theirs, base: (c.old_start, c.old_end), side: (c.start, c.end) });
        }
    }
    hunks.sort_by_key(|h| (h.base.0, h.base.1, h.theirs));
    // Group hunks whose base ranges overlap or touch (adjacent edits conflict, as in git).
    let mut groups: Vec<(usize, usize, Vec<Hunk>)> = Vec::new();
    for h in hunks {
        match groups.last_mut() {
            Some((_, end, members)) if h.base.0 <= *end => {
                *end = (*end).max(h.base.1);
                members.push(h);
            }
            _ => groups.push((h.base.0, h.base.1, vec![h])),
        }
    }
    let ours_lines = split_lines(ours);
    let theirs_lines = split_lines(theirs);
    // Line-count difference (side − base) of each side's hunks before the current group.
    let mut delta = [0isize, 0isize];
    let mut out = Vec::with_capacity(groups.len());
    for (bs, be, members) in groups {
        let mut ranges = [(0usize, 0usize); 2];
        let mut present = [false; 2];
        for (i, slot) in ranges.iter_mut().enumerate() {
            let before = delta[i];
            let inside: isize = members
                .iter()
                .filter(|h| usize::from(h.theirs) == i)
                .map(|h| {
                    present[i] = true;
                    (h.side.1 as isize - h.side.0 as isize) - (h.base.1 as isize - h.base.0 as isize)
                })
                .sum();
            let start = (bs as isize).saturating_add(before).max(0) as usize;
            let end = (be as isize).saturating_add(before).saturating_add(inside).max(start as isize) as usize;
            *slot = (start, end);
            delta[i] = before.saturating_add(inside);
        }
        let (o, t) = (ranges[0].0..ranges[0].1, ranges[1].0..ranges[1].1);
        let kind = match present {
            [true, false] => ChunkKind::Ours,
            [false, true] => ChunkKind::Theirs,
            _ if slice(&ours_lines, &o) == slice(&theirs_lines, &t) => ChunkKind::Same,
            _ => ChunkKind::Conflict,
        };
        out.push(MergeChunk { kind, base: bs..be, ours: o, theirs: t });
    }
    out
}

/// The merged text: base lines outside the chunks, each chunk's resolution inside them. A chunk
/// without one (`None`) is written with conflict markers labelled `ours_label`/`theirs_label`.
/// Returns the text and the number of chunks left unresolved.
pub fn compose(
    base: &str,
    ours: &str,
    theirs: &str,
    chunks: &[MergeChunk],
    resolutions: &[Option<Resolution>],
    labels: (&str, &str),
) -> (String, usize) {
    let (b, o, t) = (split_lines(base), split_lines(ours), split_lines(theirs));
    let mut out = String::with_capacity(base.len().max(ours.len()));
    let mut pos = 0usize;
    let mut unresolved = 0;
    for (i, c) in chunks.iter().enumerate() {
        out.push_str(&slice(&b, &(pos..c.base.start.max(pos))));
        let (ours_text, theirs_text) = (slice(&o, &c.ours), slice(&t, &c.theirs));
        match resolutions.get(i).copied().flatten() {
            Some(Resolution::Ours) => out.push_str(&ours_text),
            Some(Resolution::Theirs) => out.push_str(&theirs_text),
            Some(Resolution::OursThenTheirs) => push_both(&mut out, &ours_text, &theirs_text),
            Some(Resolution::TheirsThenOurs) => push_both(&mut out, &theirs_text, &ours_text),
            Some(Resolution::Base) => out.push_str(&slice(&b, &c.base)),
            None => {
                unresolved += 1;
                ensure_newline(&mut out);
                out.push_str(&format!("<<<<<<< {}\n", labels.0));
                out.push_str(&ours_text);
                ensure_newline(&mut out);
                out.push_str("=======\n");
                out.push_str(&theirs_text);
                ensure_newline(&mut out);
                out.push_str(&format!(">>>>>>> {}\n", labels.1));
            }
        }
        pos = pos.max(c.base.end);
    }
    out.push_str(&slice(&b, &(pos..b.len())));
    (out, unresolved)
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn push_both(out: &mut String, first: &str, second: &str) {
    out.push_str(first);
    if !first.is_empty() && !first.ends_with('\n') && !second.is_empty() {
        out.push('\n');
    }
    out.push_str(second);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn resolve_naturally(cs: &[MergeChunk]) -> Vec<Option<Resolution>> {
        cs.iter().map(|c| Resolution::natural(c.kind)).collect()
    }

    #[test]
    fn non_conflicting_changes_merge() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\nf\n";
        let cs = chunks(base, ours, theirs);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].kind, ChunkKind::Ours);
        assert_eq!(cs[1].kind, ChunkKind::Theirs);
        assert_eq!((cs[1].base.clone(), cs[1].ours.clone(), cs[1].theirs.clone()), (4..5, 4..5, 4..6));
        let (text, left) = compose(base, ours, theirs, &cs, &resolve_naturally(&cs), ("HEAD", "x"));
        assert_eq!((text.as_str(), left), ("A\nb\nc\nd\nE\nf\n", 0));
    }

    #[test]
    fn line_shifts_on_one_side_keep_ranges_aligned() {
        let base = "1\n2\n3\n4\n5\n";
        let ours = "0\n0\n1\n2\n3\n4\nX\n";
        let theirs = "1\n2\n3\n4\nY\n";
        let cs = chunks(base, ours, theirs);
        assert_eq!(cs.len(), 2, "{cs:?}");
        assert_eq!(cs[0].kind, ChunkKind::Ours);
        assert_eq!(cs[1].kind, ChunkKind::Conflict);
        assert_eq!((cs[1].base.clone(), cs[1].ours.clone(), cs[1].theirs.clone()), (4..5, 6..7, 4..5));
        let mut res = resolve_naturally(&cs);
        let (text, left) = compose(base, ours, theirs, &cs, &res, ("HEAD", "feature"));
        assert_eq!(left, 1);
        assert_eq!(text, "0\n0\n1\n2\n3\n4\n<<<<<<< HEAD\nX\n=======\nY\n>>>>>>> feature\n");
        for (r, want) in [
            (Resolution::Ours, "X\n"),
            (Resolution::Theirs, "Y\n"),
            (Resolution::OursThenTheirs, "X\nY\n"),
            (Resolution::TheirsThenOurs, "Y\nX\n"),
            (Resolution::Base, "5\n"),
        ] {
            res[1] = Some(r);
            let (text, left) = compose(base, ours, theirs, &cs, &res, ("HEAD", "feature"));
            assert_eq!(left, 0);
            assert_eq!(text, format!("0\n0\n1\n2\n3\n4\n{want}"), "{r:?}");
        }
    }

    #[test]
    fn same_changes_and_adjacent_edits() {
        let base = "a\nb\nc\n";
        let cs = chunks(base, "a\nB\nc\n", "a\nB\nc\n");
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ChunkKind::Same);
        // Adjacent edits on different sides conflict.
        let cs = chunks(base, "A\nb\nc\n", "a\nB\nc\n");
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].kind, ChunkKind::Conflict);
        assert_eq!(cs[0].base, 0..2);
        assert_eq!(cs[0].ours, 0..2);
        assert_eq!(cs[0].theirs, 0..2);
        // Insertions at the same point conflict; missing final newlines get one before markers.
        let cs = chunks("x", "x\nours", "x\ntheirs");
        assert_eq!(cs.len(), 1);
        let (text, left) = compose("x", "x\nours", "x\ntheirs", &cs, &[None], ("a", "b"));
        assert_eq!(left, 1);
        assert_eq!(text, "<<<<<<< a\nx\nours\n=======\nx\ntheirs\n>>>>>>> b\n");
        // Everything identical: no chunks.
        assert!(chunks(base, base, base).is_empty());
        assert_eq!(compose(base, base, base, &[], &[], ("a", "b")).0, base);
        // One side deleted everything.
        let cs = chunks(base, "", base);
        assert_eq!(cs[0].kind, ChunkKind::Ours);
        assert_eq!(compose(base, "", base, &cs, &resolve_naturally(&cs), ("a", "b")).0, "");
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn hostile_chunks_do_not_panic() {
        let bad = MergeChunk { kind: ChunkKind::Conflict, base: 9..3, ours: 50..60, theirs: 2..1 };
        let _ = compose("a\n", "b\n", "c\n", &[bad.clone(), bad], &[None, Some(Resolution::Base)], ("x", "y"));
        assert_eq!(slice(&["a\n"], &(5..9)), "");
    }
}
