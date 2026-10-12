//! Crate-private helpers shared by [`crate::ops`], [`crate::movement`] and the document: char
//! classes, cluster and word boundaries, line blocks and the multi-caret edit builder.
//!
//! Everything here works on `doc.rope()` directly (no whole-document copies) so the per-keystroke
//! operations stay cheap on large files.

use crate::{Change, ChangeSet, Document, Edit, Selection, SelectionSet};

/// Chars ropey treats as line separators (it is built with `unicode_lines`). The document
/// normalizes `\r` away, but the Unicode separators can still appear in loaded text.
pub(crate) fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// `\r\n` and lone `\r` become `\n` (the document's internal separator).
pub(crate) fn normalize_newlines(text: &str) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text.to_string() }
}

/// Identifier char: alphanumeric or `_`.
pub(crate) fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Indentation whitespace (space or tab).
pub(crate) fn is_indent_char(c: char) -> bool {
    c == ' ' || c == '\t'
}

pub(crate) fn closer_of(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

pub(crate) fn opener_of(close: char) -> Option<char> {
    match close {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

/// Word-motion class of a char; `None` (outside the document) counts as whitespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Space,
    Ident,
    Punct,
}

pub(crate) fn class(c: Option<char>) -> Class {
    match c {
        None => Class::Space,
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if is_ident(c) => Class::Ident,
        Some(_) => Class::Punct,
    }
}

/// A word (identifier run or punctuation run) starts at the char `cur`, preceded by `prev`.
fn is_word_start(prev: Option<char>, cur: Option<char>) -> bool {
    let c = class(cur);
    c != Class::Space && class(prev) != c
}

/// A word ends between `prev` and `cur`.
fn is_word_end(prev: Option<char>, cur: Option<char>) -> bool {
    let p = class(prev);
    p != Class::Space && class(cur) != p
}

/// Alt+Right target: the next word end, staying on the caret's line (from a line end, on the
/// next line). With `delete`, word starts also stop (Alt+Delete deletes whitespace runs alone).
pub(crate) fn word_right(doc: &Document, pos: usize, delete: bool) -> usize {
    let len = doc.len_chars();
    if pos >= len {
        return len;
    }
    let line = doc.char_to_line(pos);
    let mut max = doc.line_end(line);
    if pos >= max {
        if line + 1 >= doc.len_lines() {
            return max.max(pos);
        }
        max = doc.line_end(line + 1);
    }
    let Some(mut it) = doc.rope().get_chars_at(pos) else { return pos };
    let mut prev = it.next();
    let mut i = pos + 1;
    while i < max {
        let cur = it.next();
        if is_word_end(prev, cur) || (delete && is_word_start(prev, cur)) {
            return i;
        }
        prev = cur;
        i += 1;
    }
    max
}

/// Alt+Left target: the previous word start, at most back to the end of the previous line.
/// With `delete`, word ends also stop.
pub(crate) fn word_left(doc: &Document, pos: usize, delete: bool) -> usize {
    let pos = pos.min(doc.len_chars());
    if pos == 0 {
        return 0;
    }
    let line = doc.char_to_line(pos);
    let min = if line > 0 { doc.line_end(line - 1) } else { 0 };
    let Some(mut it) = doc.rope().get_chars_at(pos) else { return pos - 1 };
    let mut cur = it.prev();
    let mut i = pos - 1;
    while i > min {
        let prev = it.prev();
        if is_word_start(prev, cur) || (delete && is_word_end(prev, cur)) {
            return i;
        }
        cur = prev;
        i -= 1;
    }
    i
}

/// The identifier word touching `at` (the char at `at`, else the one before); empty at `at` if none.
pub(crate) fn word_range_at(doc: &Document, at: usize) -> std::ops::Range<usize> {
    let at = at.min(doc.len_chars());
    let anchor = if doc.char_at(at).is_some_and(is_ident) {
        at
    } else if at > 0 && doc.char_at(at - 1).is_some_and(is_ident) {
        at - 1
    } else {
        return at..at;
    };
    let mut start = anchor;
    if let Some(mut it) = doc.rope().get_chars_at(anchor) {
        while let Some(c) = it.prev() {
            if !is_ident(c) {
                break;
            }
            start -= 1;
        }
    }
    let mut end = anchor;
    if let Some(it) = doc.rope().get_chars_at(anchor) {
        end += it.take_while(|c| is_ident(*c)).count();
    }
    start..end
}

// ---- grapheme-cluster approximation -------------------------------------------------------------

/// Longest run of joined code points treated as one cluster (bounds the work per keystroke).
const MAX_CLUSTER: usize = 32;
const ZWJ: char = '\u{200D}';

/// Combining marks, joiners, variation selectors, emoji modifiers and tags: chars that attach to
/// the previous one. An approximation of Unicode `Grapheme_Extend` without the full tables.
fn is_extend(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A | 0x064B..=0x065F
        | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x0900..=0x0903 | 0x093A..=0x094F
        | 0x0951..=0x0957 | 0x0962..=0x0963 | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200C..=0x200D | 0x20D0..=0x20FF | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0xE0100..=0xE01EF)
}

/// Emoji-like chars that join a preceding ZWJ.
fn is_pictographic(c: char) -> bool {
    matches!(c as u32, 0x2300..=0x23FF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x1F000..=0x1FAFF)
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// The end of the cluster starting at `pos` (Right arrow, Delete).
pub(crate) fn next_boundary(doc: &Document, pos: usize) -> usize {
    let len = doc.len_chars();
    if pos >= len {
        return len;
    }
    let Some(mut it) = doc.rope().get_chars_at(pos) else { return len };
    let Some(first) = it.next() else { return len };
    let mut end = pos + 1;
    if is_line_break(first) {
        return end;
    }
    let mut prev = first;
    let mut flag_open = is_regional_indicator(first);
    for _ in 0..MAX_CLUSTER {
        let Some(c) = it.next() else { break };
        if is_line_break(c) {
            break;
        }
        let ri = flag_open && is_regional_indicator(c);
        if !(is_extend(c) || (prev == ZWJ && is_pictographic(c)) || ri) {
            break;
        }
        if ri {
            flag_open = false;
        }
        prev = c;
        end += 1;
    }
    end
}

/// The start of the cluster ending at `pos` (Left arrow, Backspace).
pub(crate) fn prev_boundary(doc: &Document, pos: usize) -> usize {
    let pos = pos.min(doc.len_chars());
    if pos == 0 {
        return 0;
    }
    let Some(mut it) = doc.rope().get_chars_at(pos) else { return pos - 1 };
    let Some(last) = it.prev() else { return pos - 1 };
    let mut start = pos - 1;
    if is_line_break(last) {
        return start;
    }
    if is_regional_indicator(last) {
        // Flags are pairs: an even run of indicators before `pos` ends with a complete pair.
        let mut run = 1usize;
        let mut back = it.clone();
        while run < 64 {
            match back.prev() {
                Some(c) if is_regional_indicator(c) => run += 1,
                _ => break,
            }
        }
        return if run.is_multiple_of(2) { pos - 2 } else { start };
    }
    let mut cur = last;
    for _ in 0..MAX_CLUSTER {
        let Some(p) = it.prev() else { break };
        if is_line_break(p) || !(is_extend(cur) || (p == ZWJ && is_pictographic(cur))) {
            break;
        }
        start -= 1;
        cur = p;
    }
    start
}

// ---- lines ----------------------------------------------------------------------------------------

/// End of `line` including its separator (the next line's start, or the document end).
pub(crate) fn line_full_end(doc: &Document, line: usize) -> usize {
    if line + 1 < doc.len_lines() { doc.line_start(line + 1) } else { doc.len_chars() }
}

/// Number of leading spaces/tabs on `line`, and whether the line is blank (only whitespace).
pub(crate) fn indent_info(doc: &Document, line: usize) -> (usize, bool) {
    let start = doc.line_start(line);
    let len = doc.line_len(line);
    let n = match doc.rope().get_chars_at(start) {
        Some(it) => it.take(len).take_while(|c| is_indent_char(*c)).count(),
        None => 0,
    };
    let blank =
        n == len || doc.rope().get_slice(start + n..start + len).is_some_and(|s| s.chars().all(char::is_whitespace));
    (n, blank)
}

/// Leading whitespace of `line`, at most `max` chars.
pub(crate) fn indent_prefix(doc: &Document, line: usize, max: usize) -> String {
    let start = doc.line_start(line);
    let len = doc.line_len(line).min(max);
    match doc.rope().get_chars_at(start) {
        Some(it) => it.take(len).take_while(|c| is_indent_char(*c)).collect(),
        None => String::new(),
    }
}

/// Whether the text at `at` starts with `pat`.
pub(crate) fn starts_with_at(doc: &Document, at: usize, pat: &str) -> bool {
    if pat.is_empty() {
        return true;
    }
    let Some(mut it) = doc.rope().get_chars_at(at.min(doc.len_chars())) else { return false };
    pat.chars().all(|p| it.next() == Some(p))
}

/// Lines touched by a selection. A multi-line selection that ends at column 0 does not touch
/// its last line (IntelliJ convention for line-based actions).
pub(crate) fn sel_lines(doc: &Document, sel: &Selection) -> (usize, usize) {
    let l0 = doc.char_to_line(sel.start());
    let mut l1 = doc.char_to_line(sel.end());
    if l1 > l0 && sel.end() == doc.line_start(l1) {
        l1 -= 1;
    }
    (l0, l1)
}

/// The touched lines of all selections merged into sorted, disjoint, inclusive blocks; with
/// `merge_adjacent`, blocks that touch (`a.1 + 1 == b.0`) merge too.
pub(crate) fn line_blocks<'a>(
    doc: &Document,
    sels: impl IntoIterator<Item = &'a Selection>,
    merge_adjacent: bool,
) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for s in sels {
        let (a, b) = sel_lines(doc, s);
        if let Some(last) = out.last_mut()
            && (a <= last.1 || (merge_adjacent && a == last.1 + 1))
        {
            last.1 = last.1.max(b);
            continue;
        }
        out.push((a, b));
    }
    out
}

/// Index of the block containing `line` (blocks sorted and disjoint).
pub(crate) fn block_of(blocks: &[(usize, usize)], line: usize) -> Option<usize> {
    let i = blocks.partition_point(|b| b.1 < line);
    blocks.get(i).filter(|b| b.0 <= line).map(|_| i)
}

/// Display width of one indent level, in columns.
pub(crate) fn indent_width(unit: &str, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    let w: usize = unit.chars().map(|c| if c == '\t' { tab } else { 1 }).sum();
    if w == 0 { tab } else { w }
}

/// Visual column after `chars`, starting at column 0 (tabs advance to the next tab stop).
pub(crate) fn advance_column(col: usize, c: char, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    if c == '\t' { (col / tab + 1) * tab } else { col + 1 }
}

/// Visual column of `pos` on its line.
pub(crate) fn visual_column(doc: &Document, pos: usize, tab_width: usize) -> usize {
    let line = doc.char_to_line(pos);
    let start = doc.line_start(line);
    match doc.rope().get_slice(start..pos.max(start).min(doc.len_chars())) {
        Some(s) => s.chars().fold(0, |col, c| advance_column(col, c, tab_width)),
        None => 0,
    }
}

// ---- edit building --------------------------------------------------------------------------------

/// Post-edit offset of `pos` after a net change of `delta` chars before it.
pub(crate) fn shift(pos: usize, delta: isize) -> usize {
    if delta >= 0 { pos.saturating_add(delta.unsigned_abs()) } else { pos.saturating_sub(delta.unsigned_abs()) }
}

/// Collects changes in document order (pre-edit coordinates) and the resulting selections
/// (post-edit coordinates) for a multi-caret operation.
pub(crate) struct EditBuilder {
    changes: Vec<Change>,
    last_end: usize,
    delta: isize,
    sels: Vec<Selection>,
}

impl EditBuilder {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { changes: Vec::with_capacity(capacity), last_end: 0, delta: 0, sels: Vec::with_capacity(capacity) }
    }

    /// Post-edit offset of a pre-edit offset at or after the end of every change so far.
    pub(crate) fn map(&self, pos: usize) -> usize {
        shift(pos, self.delta)
    }

    /// Whether a change starting at `start` would be in order (not overlap earlier changes).
    pub(crate) fn fits(&self, start: usize) -> bool {
        start >= self.last_end
    }

    /// Replace `start..end` with `text`; returns the post-edit offset of the change start.
    /// Deletions that overlap an earlier change are clipped to what is left (two carets deleting
    /// the same text); any other overlapping change is dropped and `None` returned.
    pub(crate) fn replace(&mut self, start: usize, end: usize, text: impl Into<String>) -> Option<usize> {
        let text = text.into();
        let (mut start, mut end) = (start.min(end), end.max(start));
        if start < self.last_end {
            if !text.is_empty() {
                return None;
            }
            start = self.last_end;
            end = end.max(start);
        }
        let post = self.map(start);
        let removed = end - start;
        let inserted = text.chars().count();
        if removed == 0 && inserted == 0 {
            return Some(post);
        }
        self.delta += inserted as isize - removed as isize;
        self.last_end = end;
        self.changes.push(Change::new(start, end, text));
        Some(post)
    }

    pub(crate) fn push(&mut self, sel: Selection) {
        self.sels.push(sel);
    }

    pub(crate) fn caret(&mut self, at: usize) {
        self.sels.push(Selection::caret(at));
    }

    /// Keep a selection that this operation does not change (mapped past earlier changes).
    pub(crate) fn keep(&mut self, sel: &Selection) {
        let (a, h) = (self.map(sel.anchor.max(self.last_end)), self.map(sel.head.max(self.last_end)));
        self.sels.push(Selection { anchor: a, head: h, goal_column: None });
    }

    pub(crate) fn into_changes(self) -> Vec<Change> {
        self.changes
    }

    pub(crate) fn finish(self, doc: &Document, primary: usize, fallback: &SelectionSet) -> Edit {
        match ChangeSet::new(self.changes) {
            Ok(changes) => {
                let len = changes.len_after(doc.len_chars());
                let selections = SelectionSet::from_vec(self.sels, primary).clamp(len);
                Edit { changes, selections }
            }
            Err(_) => Edit::none(fallback.clone()),
        }
    }
}

/// Selection with the direction of `like`, spanning `start..end`.
pub(crate) fn directed(like: &Selection, start: usize, end: usize) -> Selection {
    if like.head >= like.anchor { Selection::new(start, end) } else { Selection::new(end, start) }
}
