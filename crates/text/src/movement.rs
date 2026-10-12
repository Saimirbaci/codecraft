//! Caret motion and selection building (multi-caret aware).
//!
//! Every function clamps the incoming selections to the document, so stale or hostile offsets
//! are harmless.

use crate::util::{
    closer_of, directed, indent_info, is_ident, line_full_end, next_boundary, opener_of, prev_boundary, sel_lines,
    word_left, word_right,
};
use crate::{Document, Position, Selection, SelectionSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    /// Alt+Left: previous word start (IntelliJ: camel humps off by default).
    WordLeft,
    /// Alt+Right: next word end.
    WordRight,
    /// Cmd+Left / Home: smart home, first non-whitespace char, then column 0 (toggles).
    LineStart,
    /// Cmd+Right / End.
    LineEnd,
    DocStart,
    DocEnd,
    /// Move up by this many lines.
    PageUp(usize),
    /// Move down by this many lines.
    PageDown(usize),
}

/// Move (or with `extend`, extend) every selection. Left/Right on a non-empty selection without
/// `extend` collapse to its start/end. Up/Down keep `goal_column`.
///
/// Left/Right step over whole user-perceived chars (combining marks, emoji sequences). Word
/// motions treat identifier runs and punctuation runs as words and stay on the caret's line
/// (from a line start/end they step onto the neighbouring line). Up/Down on the first/last line
/// leave the caret where it is.
pub fn move_selections(doc: &Document, sels: &SelectionSet, motion: Motion, extend: bool) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let moved = sels.ranges().iter().map(|s| move_one(doc, s, motion, extend)).collect();
    SelectionSet::from_vec(moved, sels.primary_index())
}

fn move_one(doc: &Document, s: &Selection, motion: Motion, extend: bool) -> Selection {
    if !extend && !s.is_empty() {
        match motion {
            Motion::Left => return Selection::caret(s.start()),
            Motion::Right => return Selection::caret(s.end()),
            _ => {}
        }
    }
    let head = s.head;
    let (head, goal_column) = match motion {
        Motion::Left => (prev_boundary(doc, head), None),
        Motion::Right => (next_boundary(doc, head), None),
        Motion::Up => vertical(doc, s, 1, true),
        Motion::Down => vertical(doc, s, 1, false),
        Motion::PageUp(n) => vertical(doc, s, n.max(1), true),
        Motion::PageDown(n) => vertical(doc, s, n.max(1), false),
        Motion::WordLeft => (word_left(doc, head, false), None),
        Motion::WordRight => (word_right(doc, head, false), None),
        Motion::LineStart => (smart_home(doc, head), None),
        Motion::LineEnd => (doc.line_end(doc.char_to_line(head)), None),
        Motion::DocStart => (0, None),
        Motion::DocEnd => (doc.len_chars(), None),
    };
    let anchor = if extend { s.anchor } else { head };
    Selection { anchor, head, goal_column }
}

/// Head after moving `lines` lines up or down, and the goal column to keep.
fn vertical(doc: &Document, s: &Selection, lines: usize, up: bool) -> (usize, Option<usize>) {
    let pos = doc.offset_to_position(s.head);
    let goal = s.goal_column.unwrap_or(pos.column);
    let last = doc.len_lines().saturating_sub(1);
    let target = if up { pos.line.saturating_sub(lines) } else { pos.line.saturating_add(lines).min(last) };
    if target == pos.line {
        return (s.head, Some(goal));
    }
    (doc.position_to_offset(Position::new(target, goal)), Some(goal))
}

/// First non-whitespace char of the line, or column 0 when already there.
fn smart_home(doc: &Document, head: usize) -> usize {
    let line = doc.char_to_line(head);
    let start = doc.line_start(line);
    let first = start + indent_info(doc, line).0;
    if head == first { start } else { first }
}

pub fn select_all(doc: &Document) -> SelectionSet {
    SelectionSet::single(Selection::new(0, doc.len_chars()))
}

/// The identifier-like word (alphanumeric or `_`) touching `at`; empty range at `at` if none.
pub fn word_range_at(doc: &Document, at: usize) -> std::ops::Range<usize> {
    crate::util::word_range_at(doc, at)
}

pub fn select_word_at(doc: &Document, at: usize) -> Selection {
    let r = word_range_at(doc, at);
    Selection::new(r.start, r.end)
}

/// The whole line including its newline (triple click).
pub fn select_line(doc: &Document, line: usize) -> Selection {
    let line = line.min(doc.len_lines().saturating_sub(1));
    Selection::new(doc.line_start(line), line_full_end(doc, line))
}

/// Most chars scanned for an enclosing bracket in each direction.
const MAX_BRACKET_SCAN: usize = 1_000_000;
/// Longest line scanned for enclosing quotes.
const MAX_QUOTE_LINE: usize = 100_000;

/// The nearest bracket pair around `start..end`: (opener offset, closer offset).
fn enclosing_brackets(doc: &Document, start: usize, end: usize) -> Option<(usize, usize)> {
    let mut it = doc.rope().get_chars_at(start)?;
    let mut pending: Vec<char> = Vec::new();
    let mut i = start;
    let (open_at, open) = loop {
        if start - i >= MAX_BRACKET_SCAN {
            return None;
        }
        let c = it.prev()?;
        i -= 1;
        if opener_of(c).is_some() {
            pending.push(c);
        } else if closer_of(c).is_some() {
            match pending.pop() {
                Some(_) => {}
                None => break (i, c),
            }
        }
    };
    let close = closer_of(open)?;
    let mut depth = 0usize;
    for (j, c) in doc.rope().get_chars_at(end)?.enumerate().take(MAX_BRACKET_SCAN) {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some((open_at, end + j));
            }
            depth -= 1;
        }
    }
    None
}

/// The double-quoted string on one line around `start..end`: (open quote, close quote).
fn enclosing_quotes(doc: &Document, start: usize, end: usize) -> Option<(usize, usize)> {
    let line = doc.char_to_line(start);
    if doc.char_to_line(end) != line {
        return None;
    }
    let ls = doc.line_start(line);
    let le = doc.line_end(line);
    if le - ls > MAX_QUOTE_LINE {
        return None;
    }
    let mut quotes = Vec::new();
    let mut escaped = false;
    for (i, c) in doc.rope().get_slice(ls..le)?.chars().enumerate() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            quotes.push(ls + i);
        }
    }
    let (pairs, _) = quotes.as_chunks::<2>();
    pairs.iter().find(|[o, c]| *o < start && end <= *c).map(|[o, c]| (*o, *c))
}

/// Alt+Up fallback without a language server: word → inside the nearest enclosing brackets
/// or quotes → including them → line → whole document.
///
/// Each step picks the smallest of those ranges that strictly contains the selection.
pub fn extend_selection(doc: &Document, sels: &SelectionSet) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let grown = sels.ranges().iter().map(|s| extend_one(doc, s)).collect();
    SelectionSet::from_vec(grown, sels.primary_index())
}

fn extend_one(doc: &Document, s: &Selection) -> Selection {
    let (start, end) = (s.start(), s.end());
    let mut best: Option<std::ops::Range<usize>> = None;
    let mut consider = |r: std::ops::Range<usize>| {
        let grows = r.start <= start && end <= r.end && r.len() > end - start;
        if grows && best.as_ref().is_none_or(|b| r.len() < b.len()) {
            best = Some(r);
        }
    };
    consider(word_range_at(doc, start));
    if let Some((o, c)) = enclosing_brackets(doc, start, end) {
        consider(o + 1..c);
        consider(o..c + 1);
    }
    if let Some((o, c)) = enclosing_quotes(doc, start, end) {
        consider(o + 1..c);
        consider(o..c + 1);
    }
    let (l0, l1) = (doc.char_to_line(start), doc.char_to_line(end));
    if l0 == l1 {
        let ls = doc.line_start(l0);
        consider(ls + indent_info(doc, l0).0..doc.line_end(l0));
    }
    consider(doc.line_start(l0)..line_full_end(doc, l1));
    consider(0..doc.len_chars());
    match best {
        Some(r) => directed(s, r.start, r.end),
        None => *s,
    }
}

/// Add a caret on the line above or below the primary caret, at the same goal column.
pub fn add_caret_vertical(doc: &Document, sels: &SelectionSet, below: bool) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let primary = sels.primary();
    let pos = doc.offset_to_position(primary.head);
    let goal = primary.goal_column.unwrap_or(pos.column);
    let target = if below { pos.line + 1 } else { pos.line.wrapping_sub(1) };
    if target >= doc.len_lines() {
        return sels;
    }
    let at = doc.position_to_offset(Position::new(target, goal));
    let mut out = sels;
    out.push(Selection { anchor: at, head: at, goal_column: Some(goal) });
    out
}

/// Most selections one occurrence action creates.
const MAX_OCCURRENCES: usize = 10_000;

/// Whether `start..end` overlaps an existing selection (ranges sorted and disjoint).
fn overlaps_selection(sels: &SelectionSet, start: usize, end: usize) -> bool {
    let ranges = sels.ranges();
    let i = ranges.partition_point(|r| r.end() <= start);
    ranges.get(i).is_some_and(|r| r.start() < end)
}

/// Char offsets of the occurrences of `needle` (non-overlapping, in document order).
fn occurrences<'a>(doc: &'a Document, text: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    text.match_indices(needle).filter_map(|(b, _)| doc.rope().try_byte_to_char(b).ok())
}

/// Ctrl+G: with an empty primary selection select the word; otherwise add a selection on the
/// next occurrence (case-sensitive) of the primary selection's text, wrapping around.
pub fn add_next_occurrence(doc: &Document, sels: &SelectionSet) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let primary = sels.primary();
    if primary.is_empty() {
        let word = word_range_at(doc, primary.head);
        if word.is_empty() {
            return sels;
        }
        let mut ranges = sels.ranges().to_vec();
        if let Some(slot) = ranges.get_mut(sels.primary_index()) {
            *slot = Selection::new(word.start, word.end);
        }
        return SelectionSet::from_vec(ranges, sels.primary_index());
    }
    let needle = doc.slice(primary.range());
    let n = primary.end() - primary.start();
    let text = doc.text();
    let from = primary.end();
    let after = occurrences(doc, &text, &needle).filter(|s| *s >= from);
    let before = occurrences(doc, &text, &needle).take_while(|s| *s < from);
    let found = after.chain(before).find(|s| !overlaps_selection(&sels, *s, s + n));
    let mut out = sels.clone();
    if let Some(s) = found {
        out.push(Selection::new(s, s + n));
    }
    out
}

/// Ctrl+Shift+G: remove the most recently added occurrence (the primary), if more than one.
///
/// The selection before it (wrapping to the last) becomes primary.
pub fn unselect_occurrence(_doc: &Document, sels: &SelectionSet) -> SelectionSet {
    if sels.len() <= 1 {
        return sels.clone();
    }
    let p = sels.primary_index();
    let mut ranges = sels.ranges().to_vec();
    if p < ranges.len() {
        ranges.remove(p);
    }
    let primary = if p == 0 { ranges.len().saturating_sub(1) } else { p - 1 };
    SelectionSet::from_vec(ranges, primary)
}

/// Ctrl+Cmd+G: select every occurrence of the primary selection's text (or word at caret).
///
/// From a caret, only whole-word occurrences of the word count. Capped at 10 000 selections.
pub fn select_all_occurrences(doc: &Document, sels: &SelectionSet) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let primary = sels.primary();
    let (range, whole_word) =
        if primary.is_empty() { (word_range_at(doc, primary.head), true) } else { (primary.range(), false) };
    if range.is_empty() {
        return sels;
    }
    let needle = doc.slice(range.clone());
    let n = range.end - range.start;
    let text = doc.text();
    let mut found = Vec::new();
    let mut primary_index = 0;
    for s in occurrences(doc, &text, &needle) {
        let e = s + n;
        let bounded = s == 0 || !doc.char_at(s - 1).is_some_and(is_ident);
        if whole_word && !(bounded && !doc.char_at(e).is_some_and(is_ident)) {
            continue;
        }
        if s == range.start {
            primary_index = found.len();
        }
        found.push(Selection::new(s, e));
        if found.len() >= MAX_OCCURRENCES {
            break;
        }
    }
    if found.is_empty() {
        return sels;
    }
    SelectionSet::from_vec(found, primary_index)
}

/// Alt+Shift+G: one caret at the end of every line touched by the selections.
pub fn add_carets_to_line_ends(doc: &Document, sels: &SelectionSet) -> SelectionSet {
    let sels = sels.clamp(doc.len_chars());
    let primary_line = doc.char_to_line(sels.primary().head);
    let mut carets = Vec::new();
    let mut primary = 0;
    for s in sels.ranges() {
        let (l0, l1) = sel_lines(doc, s);
        for line in l0..=l1 {
            if line == primary_line {
                primary = carets.len();
            }
            carets.push(Selection::caret(doc.line_end(line)));
        }
    }
    SelectionSet::from_vec(carets, primary)
}

#[cfg(test)]
mod tests;
