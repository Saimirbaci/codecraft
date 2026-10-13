//! Column (box) selection: a rectangle of display columns across lines, made of one selection
//! per line (multi-caret). Display columns expand tabs to the next tab stop; lines shorter than
//! the rectangle get a caret at their end.

use crate::wrap::display_columns;
use crate::{Document, Selection, SelectionSet};

/// One corner of a box: a line and a display column (may lie past the line's end).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BoxPoint {
    pub line: usize,
    pub column: usize,
}

impl BoxPoint {
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

fn line_chars(doc: &Document, line: usize) -> Vec<char> {
    doc.line_text(line).unwrap_or_default().chars().filter(|c| *c != '\n' && *c != '\r').collect()
}

/// The char column of a line that sits at display column `display` (the first char starting at
/// or after it; the line end when the line is shorter).
pub fn char_column_at_display(chars: &[char], display: usize, tab: usize) -> usize {
    let vc = display_columns(chars, tab);
    vc.iter().position(|c| *c >= display).unwrap_or(chars.len()).min(chars.len())
}

/// Line and display column of a char offset.
pub fn box_point_of(doc: &Document, offset: usize, tab: usize) -> BoxPoint {
    let offset = offset.min(doc.len_chars());
    let line = doc.char_to_line(offset);
    let chars = line_chars(doc, line);
    let col = offset.saturating_sub(doc.line_start(line)).min(chars.len());
    let vc = display_columns(&chars, tab);
    BoxPoint { line, column: vc.get(col).copied().unwrap_or(0) }
}

/// Most lines a box selection spans (guards against absurd input).
pub const MAX_BOX_LINES: usize = 100_000;

/// The selections for the box between `anchor` and `head`: one per line, each running from the
/// anchor's column to the head's column (so typing replaces the block on every line). The
/// primary selection is on the head's line.
pub fn box_selection(doc: &Document, anchor: BoxPoint, head: BoxPoint, tab: usize) -> SelectionSet {
    let last = doc.len_lines().saturating_sub(1);
    let a_line = anchor.line.min(last);
    let h_line = head.line.min(last);
    let (l0, l1) = (a_line.min(h_line), a_line.max(h_line));
    let l1 = l1.min(l0.saturating_add(MAX_BOX_LINES));
    let mut ranges = Vec::with_capacity(l1 - l0 + 1);
    let mut primary = 0;
    for line in l0..=l1 {
        let chars = line_chars(doc, line);
        let start = doc.line_start(line);
        let a = start + char_column_at_display(&chars, anchor.column, tab);
        let h = start + char_column_at_display(&chars, head.column, tab);
        if line == h_line {
            primary = ranges.len();
        }
        ranges.push(Selection::new(a, h));
    }
    SelectionSet::from_vec(ranges, primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(doc: &Document, sels: &SelectionSet) -> Vec<String> {
        sels.ranges().iter().map(|s| doc.slice(s.range())).collect()
    }

    #[test]
    fn box_over_three_lines() {
        let doc = Document::from_text("abcdef\nghijkl\nmnopqr\n");
        let sels = box_selection(&doc, BoxPoint::new(0, 1), BoxPoint::new(2, 4), 4);
        assert_eq!(texts(&doc, &sels), vec!["bcd", "hij", "nop"]);
        assert_eq!(sels.primary().head, doc.line_start(2) + 4);
        // Upward and leftward: same block, heads on the left.
        let back = box_selection(&doc, BoxPoint::new(2, 4), BoxPoint::new(0, 1), 4);
        assert_eq!(texts(&doc, &back), vec!["bcd", "hij", "nop"]);
        assert_eq!(back.primary().head, 1);
    }

    #[test]
    fn short_lines_get_a_caret_at_their_end() {
        let doc = Document::from_text("abcdef\nx\nmnopqr");
        let sels = box_selection(&doc, BoxPoint::new(0, 3), BoxPoint::new(2, 5), 4);
        assert_eq!(sels.len(), 3);
        assert_eq!(texts(&doc, &sels), vec!["de", "", "pq"]);
        assert_eq!(sels.ranges()[1], Selection::caret(doc.line_start(1) + 1));
    }

    #[test]
    fn zero_width_box_is_multi_caret() {
        let doc = Document::from_text("aaa\nbbb\nccc");
        let sels = box_selection(&doc, BoxPoint::new(0, 2), BoxPoint::new(2, 2), 4);
        assert_eq!(sels.len(), 3);
        assert!(sels.ranges().iter().all(Selection::is_empty));
    }

    #[test]
    fn tabs_use_display_columns() {
        let doc = Document::from_text("\tab\n    cd");
        let p = box_point_of(&doc, 1, 4);
        assert_eq!(p, BoxPoint::new(0, 4));
        let sels = box_selection(&doc, BoxPoint::new(0, 4), BoxPoint::new(1, 5), 4);
        assert_eq!(texts(&doc, &sels), vec!["a", "c"]);
    }

    #[test]
    fn hostile_points_are_clamped() {
        let doc = Document::from_text("ab");
        let sels = box_selection(&doc, BoxPoint::new(usize::MAX, usize::MAX), BoxPoint::new(7, 0), 0);
        assert_eq!(sels.len(), 1);
        assert_eq!(box_point_of(&doc, 99, 4), BoxPoint::new(0, 2));
    }
}
