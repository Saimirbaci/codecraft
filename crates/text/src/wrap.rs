//! Soft wrap: split long lines into visual rows by display column (tabs expand to the next tab
//! stop) and move carets up and down by visual row.
//!
//! Rows break after whitespace when the row has some, otherwise at the width. Widths are in
//! display columns of a monospace font; every char counts as one column except tabs.

use crate::util::advance_column;
use crate::{Document, Selection, SelectionSet};

/// Soft-wrap geometry: the row width in display columns and the tab width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapConfig {
    pub width: usize,
    pub tab: usize,
}

impl WrapConfig {
    pub fn new(width: usize, tab: usize) -> Self {
        Self { width: width.max(1), tab: tab.max(1) }
    }
}

/// Display column where each char of `chars` starts, plus the end column as a last entry.
pub fn display_columns(chars: &[char], tab: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(chars.len() + 1);
    let mut col = 0usize;
    for c in chars {
        out.push(col);
        col = advance_column(col, *c, tab);
    }
    out.push(col);
    out
}

/// Char columns where the visual rows of `line` start. Always begins with 0; a line that fits
/// (or an empty one) has exactly one row.
pub fn wrap_line(line: &str, cfg: WrapConfig) -> Vec<usize> {
    let chars: Vec<char> = line.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    wrap_chars(&chars, cfg)
}

/// [`wrap_line`] over chars.
pub fn wrap_chars(chars: &[char], cfg: WrapConfig) -> Vec<usize> {
    let width = cfg.width.max(1);
    let vc = display_columns(chars, cfg.tab.max(1));
    let mut starts = vec![0usize];
    let mut row_start = 0usize;
    let mut last_break: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        let end = vc.get(i + 1).copied().unwrap_or(0);
        let row_width = |rs: usize| end.saturating_sub(vc.get(rs).copied().unwrap_or(0));
        if i > row_start && row_width(row_start) > width {
            let b = match last_break {
                Some(b) if b > row_start && b <= i => b,
                _ => i,
            };
            starts.push(b);
            row_start = b;
            last_break = None;
            if i > row_start && row_width(row_start) > width {
                starts.push(i);
                row_start = i;
            }
        }
        if c.is_whitespace() {
            last_break = Some(i + 1);
        }
    }
    starts
}

/// The row (index into `starts`) that shows char column `col`. A column at a row start belongs
/// to that row (the caret is drawn at the start of the next row, not the end of the previous).
pub fn row_of_column(starts: &[usize], col: usize) -> usize {
    starts.partition_point(|s| *s <= col).saturating_sub(1)
}

/// Char column range `start..end` of row `row`; the last row ends at `len`.
pub fn row_range(starts: &[usize], row: usize, len: usize) -> std::ops::Range<usize> {
    let start = starts.get(row).copied().unwrap_or(0).min(len);
    let end = starts.get(row + 1).copied().unwrap_or(len).min(len);
    start..end.max(start)
}

fn line_chars(doc: &Document, line: usize) -> Vec<char> {
    doc.line_text(line).unwrap_or_default().chars().filter(|c| *c != '\n' && *c != '\r').collect()
}

/// The caret column within a row for a goal display column (relative to the row start).
fn column_in_row(chars: &[char], vc: &[usize], starts: &[usize], row: usize, goal: usize) -> usize {
    let len = chars.len();
    let r = row_range(starts, row, len);
    let last_row = row + 1 >= starts.len();
    let base = vc.get(r.start).copied().unwrap_or(0);
    // On a wrapped row the caret cannot sit at the next row's start (it would show there).
    let max = if last_row { r.end } else { r.end.saturating_sub(1).max(r.start) };
    // The last column whose display position is at or before the goal.
    let mut col = r.start;
    while col < max && vc.get(col + 1).copied().unwrap_or(usize::MAX).saturating_sub(base) <= goal {
        col += 1;
    }
    col
}

/// Move every caret one visual row up or down (keeping a goal display column within the row).
/// On the first/last row of the document the caret stays where it is, like line motion.
pub fn move_visual(doc: &Document, sels: &SelectionSet, down: bool, cfg: WrapConfig, extend: bool) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let moved = sels.ranges().iter().map(|s| move_one(doc, s, down, cfg, extend)).collect();
    SelectionSet::from_vec(moved, sels.primary_index())
}

fn move_one(doc: &Document, s: &Selection, down: bool, cfg: WrapConfig, extend: bool) -> Selection {
    let line = doc.char_to_line(s.head);
    let chars = line_chars(doc, line);
    let vc = display_columns(&chars, cfg.tab);
    let starts = wrap_chars(&chars, cfg);
    let col = s.head.saturating_sub(doc.line_start(line)).min(chars.len());
    let row = row_of_column(&starts, col);
    let row_base = starts.get(row).and_then(|c| vc.get(*c)).copied().unwrap_or(0);
    let goal = s.goal_column.unwrap_or_else(|| vc.get(col).copied().unwrap_or(0).saturating_sub(row_base));
    let target = if down {
        if row + 1 < starts.len() {
            Some((line, row + 1))
        } else if line + 1 < doc.len_lines() {
            Some((line + 1, 0))
        } else {
            None
        }
    } else if row > 0 {
        Some((line, row - 1))
    } else if line > 0 {
        let prev = line_chars(doc, line - 1);
        Some((line - 1, wrap_chars(&prev, cfg).len().saturating_sub(1)))
    } else {
        None
    };
    let head = match target {
        None => s.head,
        Some((l, r)) => {
            let (tc, tvc, ts) = if l == line {
                (chars.clone(), vc.clone(), starts.clone())
            } else {
                let c = line_chars(doc, l);
                let v = display_columns(&c, cfg.tab);
                let st = wrap_chars(&c, cfg);
                (c, v, st)
            };
            doc.line_start(l) + column_in_row(&tc, &tvc, &ts, r, goal)
        }
    };
    let anchor = if extend { s.anchor } else { head };
    Selection { anchor, head, goal_column: Some(goal) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(line: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let starts = wrap_line(line, WrapConfig::new(width, 4));
        (0..starts.len()).map(|r| chars[row_range(&starts, r, chars.len())].iter().collect()).collect()
    }

    #[test]
    fn short_and_empty_lines_have_one_row() {
        assert_eq!(wrap_line("", WrapConfig::new(10, 4)), vec![0]);
        assert_eq!(wrap_line("hello", WrapConfig::new(10, 4)), vec![0]);
        assert_eq!(wrap_line("0123456789", WrapConfig::new(10, 4)), vec![0]);
    }

    #[test]
    fn breaks_after_whitespace() {
        assert_eq!(rows("hello world again", 12), vec!["hello world ", "again"]);
        assert_eq!(rows("aaa bbb ccc ddd", 8), vec!["aaa bbb ", "ccc ddd"]);
    }

    #[test]
    fn hard_breaks_long_words_and_tiny_widths() {
        assert_eq!(rows("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows("abc", 1), vec!["a", "b", "c"]);
        // Width 0 is treated as 1 (never loops or panics).
        assert_eq!(wrap_line("ab", WrapConfig::new(0, 0)), vec![0, 1]);
    }

    #[test]
    fn tabs_count_to_the_next_stop() {
        // "\tx\t" is 8 columns wide with tab 4: the row breaks after the first tab.
        assert_eq!(wrap_line("\tx\ty", WrapConfig::new(5, 4)), vec![0, 1]);
        assert_eq!(wrap_line("ab\tc", WrapConfig::new(4, 4)), vec![0, 3]);
        assert_eq!(display_columns(&['\t', 'a'], 4), vec![0, 4, 5]);
    }

    #[test]
    fn row_lookup() {
        let starts = vec![0, 6, 12];
        assert_eq!(row_of_column(&starts, 0), 0);
        assert_eq!(row_of_column(&starts, 5), 0);
        assert_eq!(row_of_column(&starts, 6), 1);
        assert_eq!(row_of_column(&starts, 99), 2);
        assert_eq!(row_range(&starts, 2, 14), 12..14);
        assert_eq!(row_range(&starts, 7, 14), 0..14);
    }

    #[test]
    fn caret_moves_by_visual_row() {
        // Line 0 wraps into "hello world " / "again"; line 1 is "x".
        let doc = Document::from_text("hello world again\nx");
        let cfg = WrapConfig::new(12, 4);
        let s = SelectionSet::caret(2);
        let down = move_visual(&doc, &s, true, cfg, false);
        assert_eq!(down.primary().head, 14, "same line, second row, goal column 2");
        let down2 = move_visual(&doc, &down, true, cfg, false);
        assert_eq!(down2.primary().head, 19, "next line clamps to its end");
        let up = move_visual(&doc, &down2, false, cfg, false);
        assert_eq!(up.primary().head, 14, "goal column kept");
        let up2 = move_visual(&doc, &up, false, cfg, false);
        assert_eq!(up2.primary().head, 2);
        // First row: stays.
        assert_eq!(move_visual(&doc, &up2, false, cfg, false).primary().head, 2);
        // Extend keeps the anchor.
        let ext = move_visual(&doc, &SelectionSet::caret(2), true, cfg, true);
        assert_eq!((ext.primary().anchor, ext.primary().head), (2, 14));
    }

    #[test]
    fn caret_on_wrapped_row_never_lands_on_the_next_row_start() {
        let doc = Document::from_text("aaaa bbbb\nzzzz");
        let cfg = WrapConfig::new(5, 4);
        // From the end of line 1 (goal 4), up goes to the last row of line 0 ("bbbb", end of line).
        let s = SelectionSet::caret(doc.len_chars());
        let up = move_visual(&doc, &s, false, cfg, false);
        assert_eq!(up.primary().head, 9);
        // Up again: first row "aaaa " ends before the next row's start (col 4 at most).
        let up2 = move_visual(&doc, &up, false, cfg, false);
        assert!(up2.primary().head <= 4, "{}", up2.primary().head);
    }

    #[test]
    fn hostile_offsets_are_clamped() {
        let doc = Document::from_text("abc");
        let s = SelectionSet::caret(999);
        let moved = move_visual(&doc, &s, true, WrapConfig::new(2, 4), false);
        assert!(moved.primary().head <= 3);
    }
}
