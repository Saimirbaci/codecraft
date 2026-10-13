//! Soft wrap: split long lines into visual rows by display column (tabs expand to the next tab
//! stop) and move carets up and down by visual row.
//!
//! Rows break after whitespace when the row has some, otherwise at the width. Widths are in
//! display columns of a monospace font; every char counts as one column except tabs.
//!
//! With `indent` on (the default, like RustRover's "use original line's indent for wrapped
//! parts"), continuation rows are drawn indented by the line's leading whitespace width
//! ([`continuation_indent`], at most half the row width), so they have that much less room.

use crate::util::advance_column;
use crate::{Document, Selection, SelectionSet};

/// Soft-wrap geometry: the row width in display columns, the tab width and whether
/// continuation rows keep the line's indentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WrapConfig {
    pub width: usize,
    pub tab: usize,
    pub indent: bool,
}

impl WrapConfig {
    /// Continuation rows keep the line's indentation (see [`WrapConfig::with_indent`]).
    pub fn new(width: usize, tab: usize) -> Self {
        Self { width: width.max(1), tab: tab.max(1), indent: true }
    }

    pub fn with_indent(self, indent: bool) -> Self {
        Self { indent, ..self }
    }
}

/// Display columns continuation rows of a line are indented by: the width of its leading
/// whitespace, capped at half the row width; 0 with `indent` off or for a blank line.
pub fn continuation_indent(chars: &[char], cfg: WrapConfig) -> usize {
    if !cfg.indent {
        return 0;
    }
    let tab = cfg.tab.max(1);
    let mut col = 0usize;
    for c in chars {
        match c {
            ' ' | '\t' => col = advance_column(col, *c, tab),
            _ => return col.min(cfg.width.max(1) / 2),
        }
    }
    0
}

/// [`continuation_indent`] of a line's text.
pub fn line_indent(line: &str, cfg: WrapConfig) -> usize {
    let chars: Vec<char> = line.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    continuation_indent(&chars, cfg)
}

/// Display column (from the left edge of the text) where row `row` of a line starts drawing.
pub fn row_offset(row: usize, indent: usize) -> usize {
    if row == 0 { 0 } else { indent }
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
    let indent = continuation_indent(chars, cfg);
    // Room on a row: continuation rows lose the indentation (at least one column is left).
    let room = |row_start: usize| if row_start == 0 { width } else { width.saturating_sub(indent).max(1) };
    let vc = display_columns(chars, cfg.tab.max(1));
    let mut starts = vec![0usize];
    let mut row_start = 0usize;
    let mut last_break: Option<usize> = None;
    // Leading whitespace is never a break opportunity (a row of indentation alone helps nobody).
    let mut in_leading = true;
    for (i, c) in chars.iter().enumerate() {
        let end = vc.get(i + 1).copied().unwrap_or(0);
        let row_width = |rs: usize| end.saturating_sub(vc.get(rs).copied().unwrap_or(0));
        if i > row_start && row_width(row_start) > room(row_start) {
            let b = match last_break {
                Some(b) if b > row_start && b <= i => b,
                _ => i,
            };
            starts.push(b);
            row_start = b;
            last_break = None;
            if i > row_start && row_width(row_start) > room(row_start) {
                starts.push(i);
                row_start = i;
            }
        }
        if !c.is_whitespace() {
            in_leading = false;
        } else if !in_leading {
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

/// The caret column within a row for a goal visual column (from the text's left edge, so a
/// continuation row's indentation counts).
fn column_in_row(chars: &[char], vc: &[usize], starts: &[usize], row: usize, goal: usize, cfg: WrapConfig) -> usize {
    let len = chars.len();
    let r = row_range(starts, row, len);
    let last_row = row + 1 >= starts.len();
    let goal = goal.saturating_sub(row_offset(row, continuation_indent(chars, cfg)));
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
    let goal = s.goal_column.unwrap_or_else(|| {
        vc.get(col).copied().unwrap_or(0).saturating_sub(row_base) + row_offset(row, continuation_indent(&chars, cfg))
    });
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
            doc.line_start(l) + column_in_row(&tc, &tvc, &ts, r, goal, cfg)
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
        // "\tx\t" is 8 columns wide with tab 4: the row breaks before the second tab (the
        // leading tab is indentation, not a break opportunity).
        assert_eq!(wrap_line("\tx\ty", WrapConfig::new(5, 4).with_indent(false)), vec![0, 2]);
        assert_eq!(wrap_line("ab\tc", WrapConfig::new(4, 4)), vec![0, 3]);
        assert_eq!(display_columns(&['\t', 'a'], 4), vec![0, 4, 5]);
    }

    #[test]
    fn continuation_rows_keep_the_indentation() {
        let cfg = WrapConfig::new(12, 4);
        // Four spaces of indentation: continuation rows have 12 - 4 = 8 columns.
        let line = "    aaa bbb ccc ddd eee";
        assert_eq!(line_indent(line, cfg), 4);
        let starts = wrap_line(line, cfg);
        let chars: Vec<char> = line.chars().collect();
        let rows: Vec<String> =
            (0..starts.len()).map(|r| chars[row_range(&starts, r, chars.len())].iter().collect()).collect();
        assert_eq!(rows, vec!["    aaa bbb ", "ccc ddd ", "eee"]);
        for r in 1..rows.len() {
            assert!(row_offset(r, 4) + rows[r].trim_end().len() <= 12, "{rows:?}");
        }
        assert_eq!(row_offset(0, 4), 0);
        // Without indent the second row takes the full width.
        assert_eq!(wrap_line(line, cfg.with_indent(false)), vec![0, 12]);
        // Tabs count to their stop; blank lines have no indent.
        assert_eq!(line_indent("\t\tx", WrapConfig::new(40, 4)), 8);
        assert_eq!(line_indent("    ", cfg), 0);
        assert_eq!(line_indent("x", cfg), 0);
    }

    #[test]
    fn indentation_is_capped_at_half_the_width() {
        let cfg = WrapConfig::new(10, 4);
        let line = format!("{}abcdefghijklmnop", " ".repeat(30));
        assert_eq!(line_indent(&line, cfg), 5);
        let starts = wrap_line(&line, cfg);
        // Every continuation row fits in the 5 columns left.
        let n = line.chars().count();
        for r in 1..starts.len() {
            let range = row_range(&starts, r, n);
            assert!(range.len() <= 5 && !range.is_empty(), "{starts:?}");
        }
        // A tiny width still makes progress.
        assert!(wrap_line("    abcdef", WrapConfig::new(1, 4)).len() >= 6);
    }

    #[test]
    fn leading_whitespace_is_not_a_break_opportunity() {
        // Without the rule, the row would break right after the indentation.
        let starts = wrap_line("        abcdefghij", WrapConfig::new(12, 4).with_indent(false));
        assert_eq!(starts, vec![0, 12]);
    }

    #[test]
    fn caret_keeps_its_visual_column_across_indented_rows() {
        // Row 0 "    aaa bbb " (x 0..12), row 1 "ccc ddd " drawn from x 4.
        let doc = Document::from_text("    aaa bbb ccc ddd eee");
        let cfg = WrapConfig::new(12, 4);
        // Caret before the first `a` (x 4) goes down to the start of row 1 (also x 4).
        let down = move_visual(&doc, &SelectionSet::caret(4), true, cfg, false);
        assert_eq!(down.primary().head, 12);
        // Caret at line start (x 0) goes down to row 1's start too (x 4 is the leftmost there).
        let down0 = move_visual(&doc, &SelectionSet::caret(0), true, cfg, false);
        assert_eq!(down0.primary().head, 12);
        // From row 1's start back up lands at x 4 on row 0.
        let up = move_visual(&doc, &down, false, cfg, false);
        assert_eq!(up.primary().head, 4);
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
