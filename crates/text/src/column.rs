//! Column (box) selection: a rectangle of display columns across lines, made of one selection
//! per line (multi-caret). Display columns expand tabs to the next tab stop; lines shorter than
//! the rectangle get a caret at their end.
//!
//! Virtual space: a box may extend past the end of short lines. Their carets are drawn at the
//! box's column, and typing into the box ([`type_in_box`]) pads those lines with spaces first,
//! so the text lands in a straight column.

use crate::util::EditBuilder;
use crate::wrap::display_columns;
use crate::{Document, Edit, Selection, SelectionSet};

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

/// Display width of a line (tabs expanded).
pub fn line_width(doc: &Document, line: usize, tab: usize) -> usize {
    display_columns(&line_chars(doc, line), tab).last().copied().unwrap_or(0)
}

/// Whether the box between `anchor` and `head` reaches past the end of any of its lines (so
/// some carets sit in virtual space).
pub fn has_virtual_space(doc: &Document, anchor: BoxPoint, head: BoxPoint, tab: usize) -> bool {
    let last = doc.len_lines().saturating_sub(1);
    let (l0, l1) = (anchor.line.min(head.line).min(last), anchor.line.max(head.line).min(last));
    let l1 = l1.min(l0.saturating_add(MAX_BOX_LINES));
    let right = anchor.column.max(head.column);
    (l0..=l1).any(|l| line_width(doc, l, tab) < right)
}

/// Longest padding [`type_in_box`] adds to one line (guards against absurd columns).
pub const MAX_VIRTUAL_PAD: usize = 4096;

/// Type `text` into the box between `anchor` and `head` when it reaches into virtual space:
/// every line's part of the box is replaced by `text`, and lines that end left of the box are
/// first padded with spaces up to its left column. One caret per line ends up after the text.
///
/// Returns the edit and the display column the carets end at, or `None` when nothing in the box
/// lies past a line end (ordinary typing applies) or `text` spans lines.
pub fn type_in_box(doc: &Document, anchor: BoxPoint, head: BoxPoint, tab: usize, text: &str) -> Option<(Edit, usize)> {
    if text.is_empty() || text.contains(['\n', '\r']) || !has_virtual_space(doc, anchor, head, tab) {
        return None;
    }
    let last = doc.len_lines().saturating_sub(1);
    let a_line = anchor.line.min(last);
    let h_line = head.line.min(last);
    let (l0, l1) = (a_line.min(h_line), a_line.max(h_line));
    let l1 = l1.min(l0.saturating_add(MAX_BOX_LINES));
    let left = anchor.column.min(head.column);
    let right = anchor.column.max(head.column);
    let n = text.chars().count();
    let mut b = EditBuilder::new(l1 - l0 + 1);
    let mut primary = 0;
    for line in l0..=l1 {
        let chars = line_chars(doc, line);
        let width = display_columns(&chars, tab).last().copied().unwrap_or(0);
        let start = doc.line_start(line);
        if line == h_line {
            primary = line - l0;
        }
        let (from, to, insert) = if width < left {
            let pad = (left - width).min(MAX_VIRTUAL_PAD);
            let end = start + chars.len();
            (end, end, format!("{}{text}", " ".repeat(pad)))
        } else {
            let a = start + char_column_at_display(&chars, left, tab);
            let h = start + char_column_at_display(&chars, right, tab);
            (a, h, text.to_string())
        };
        let len = insert.chars().count();
        match b.replace(from, to, insert) {
            Some(at) => b.caret(at + len),
            None => b.keep(&Selection::caret(from)),
        }
    }
    let typed: Vec<char> = text.chars().collect();
    let width = display_columns(&typed, tab).last().copied().unwrap_or(n);
    let edit = b.finish(doc, primary, &SelectionSet::caret(doc.line_start(h_line)));
    Some((edit, left.saturating_add(width)))
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

    fn apply(doc: &mut Document, edit: &Edit) {
        let before = SelectionSet::caret(0);
        doc.apply(edit, &before, crate::EditGroup::Typing).unwrap();
    }

    #[test]
    fn typing_in_virtual_space_pads_short_lines() {
        let mut doc = Document::from_text("abcdef\nx\n\nmnopqr");
        // A zero-width box at column 4 over all four lines: lines 1 and 2 end before it.
        let (a, h) = (BoxPoint::new(0, 4), BoxPoint::new(3, 4));
        assert!(has_virtual_space(&doc, a, h, 4));
        let (edit, col) = type_in_box(&doc, a, h, 4, "Z").unwrap();
        assert_eq!(col, 5);
        assert_eq!(edit.selections.len(), 4);
        apply(&mut doc, &edit);
        assert_eq!(doc.text(), "abcdZef\nx   Z\n    Z\nmnopZqr");
        // Every caret sits right after its `Z`.
        let carets: Vec<usize> = edit.selections.ranges().iter().map(|s| s.head).collect();
        for c in carets {
            assert_eq!(doc.char_at(c - 1), Some('Z'));
        }
        assert_eq!(edit.selections.primary().head, doc.line_start(3) + 5);
    }

    #[test]
    fn typing_replaces_the_box_where_lines_reach_into_it() {
        let mut doc = Document::from_text("abcdef\nab\nabcdef");
        // Box columns 3..5: line 1 ends at 2 (left of the box), the others are replaced.
        let (edit, col) = type_in_box(&doc, BoxPoint::new(0, 3), BoxPoint::new(2, 5), 4, "--").unwrap();
        assert_eq!(col, 5);
        apply(&mut doc, &edit);
        assert_eq!(doc.text(), "abc--f\nab --\nabc--f");
        // A line ending inside the box: its tail is replaced, no padding.
        let mut doc = Document::from_text("abcd\nabcdefgh\na");
        let (edit, _) = type_in_box(&doc, BoxPoint::new(0, 2), BoxPoint::new(2, 6), 4, "x").unwrap();
        apply(&mut doc, &edit);
        assert_eq!(doc.text(), "abx\nabxgh\na x");
    }

    #[test]
    fn ordinary_boxes_and_multiline_text_are_left_alone() {
        let doc = Document::from_text("abcdef\nghijkl");
        assert!(!has_virtual_space(&doc, BoxPoint::new(0, 1), BoxPoint::new(1, 3), 4));
        assert!(type_in_box(&doc, BoxPoint::new(0, 1), BoxPoint::new(1, 3), 4, "x").is_none());
        assert!(type_in_box(&doc, BoxPoint::new(0, 9), BoxPoint::new(1, 9), 4, "a\nb").is_none());
        assert!(type_in_box(&doc, BoxPoint::new(0, 9), BoxPoint::new(1, 9), 4, "").is_none());
        assert_eq!(line_width(&Document::from_text("\tab"), 0, 4), 6);
    }

    #[test]
    fn hostile_virtual_columns_are_capped() {
        let mut doc = Document::from_text("a\nb");
        let (edit, _) =
            type_in_box(&doc, BoxPoint::new(usize::MAX, usize::MAX), BoxPoint::new(0, usize::MAX), 4, "x").unwrap();
        apply(&mut doc, &edit);
        assert!(doc.len_chars() < 2 * MAX_VIRTUAL_PAD + 10);
    }

    #[test]
    fn hostile_points_are_clamped() {
        let doc = Document::from_text("ab");
        let sels = box_selection(&doc, BoxPoint::new(usize::MAX, usize::MAX), BoxPoint::new(7, 0), 0);
        assert_eq!(sels.len(), 1);
        assert_eq!(box_point_of(&doc, 99, 4), BoxPoint::new(0, 2));
    }
}
