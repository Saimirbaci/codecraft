//! Editing operations. Each takes the document and current selections and returns an [`Edit`]
//! (changes in pre-edit coordinates + selections after). They never mutate; the caller applies
//! the edit with [`Document::apply`]. All operations act on every caret (multi-caret aware).
//!
//! Selections are clamped to the document first, so out-of-range offsets are harmless. The
//! per-keystroke operations ([`insert_text`], [`type_char`], [`backspace`], [`delete_forward`],
//! [`newline`]) only look at the text around each caret, never at the whole document.

use crate::util::{
    EditBuilder, advance_column, block_of, closer_of, directed, indent_info, indent_prefix, indent_width, is_ident,
    is_indent_char, line_blocks, next_boundary, normalize_newlines, opener_of, prev_boundary, sel_lines,
    starts_with_at, visual_column, word_left, word_range_at, word_right,
};
use crate::{Assoc, Change, ChangeSet, Document, Edit, Selection, SelectionSet};

/// Per-language editing settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpsConfig {
    /// One level of indentation, e.g. four spaces or `"\t"`.
    pub indent_unit: String,
    pub tab_width: usize,
    /// Insert the closing bracket/quote when typing an opening one.
    pub auto_close: bool,
    pub line_comment: Option<String>,
    pub block_comment: Option<(String, String)>,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            indent_unit: "    ".into(),
            tab_width: 4,
            auto_close: true,
            line_comment: Some("//".into()),
            block_comment: Some(("/*".into(), "*/".into())),
        }
    }
}

/// Upper bound for one indent level built from config (guards against absurd settings).
const MAX_INDENT_WIDTH: usize = 256;

/// Replace every selection with `text` (paste of plain text, completion insert, etc.).
pub fn insert_text(doc: &Document, sels: &SelectionSet, text: &str) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let text = normalize_newlines(text);
    let n = text.chars().count();
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        match b.replace(s.start(), s.end(), text.as_str()) {
            Some(at) => b.caret(at + n),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

fn is_quote(c: char) -> bool {
    c == '"' || c == '\''
}

fn is_closer(c: char) -> bool {
    opener_of(c).is_some()
}

/// The closer to insert after typing `ch` at a caret between `prev` and `next`, if any.
fn auto_closer(ch: char, prev: Option<char>, next: Option<char>) -> Option<char> {
    if !next.is_none_or(|c| c.is_whitespace() || is_closer(c)) {
        return None;
    }
    match ch {
        '(' | '[' | '{' => closer_of(ch),
        // Not after an identifier (`r"`, `b"`) or right after another quote.
        '"' => (!prev.is_some_and(|c| is_ident(c) || is_quote(c))).then_some('"'),
        // Rust lifetimes: `'a`, `&'a`, `<'a`.
        '\'' => (!prev.is_some_and(|c| is_ident(c) || c == '&' || c == '<')).then_some('\''),
        _ => None,
    }
}

/// The closing char used to wrap a selection when typing `ch`.
fn wrap_closer(ch: char) -> Option<char> {
    if is_quote(ch) { Some(ch) } else { closer_of(ch) }
}

/// Type one char: auto-closes `( [ { " '` (not `'` after an identifier char, Rust lifetimes),
/// types over an auto-inserted closer, wraps a non-empty selection in brackets/quotes.
///
/// Auto-close only happens before whitespace, a closing bracket or the end of the line; `"` is
/// not closed after an identifier char or a quote, `'` not after an identifier char, `&` or `<`.
/// Typing a closing bracket or quote right before the same char moves over it. All of this is
/// off when `cfg.auto_close` is false.
pub fn type_char(doc: &Document, sels: &SelectionSet, ch: char, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let typed = normalize_newlines(&ch.to_string());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        if !s.is_empty() {
            if cfg.auto_close
                && let Some(close) = wrap_closer(ch)
            {
                let open_at = b.replace(s.start(), s.start(), typed.as_str());
                let close_at = b.replace(s.end(), s.end(), close.to_string());
                match (open_at, close_at) {
                    (Some(o), Some(c)) => b.push(directed(s, o + 1, c)),
                    _ => b.keep(s),
                }
            } else {
                match b.replace(s.start(), s.end(), typed.as_str()) {
                    Some(at) => b.caret(at + 1),
                    None => b.keep(s),
                }
            }
            continue;
        }
        let p = s.head;
        let prev = if p > 0 { doc.char_at(p - 1) } else { None };
        let next = doc.char_at(p);
        if cfg.auto_close && next == Some(ch) && (is_closer(ch) || is_quote(ch)) {
            b.caret(b.map(p + 1));
            continue;
        }
        let text = match auto_closer(ch, prev, next).filter(|_| cfg.auto_close) {
            Some(close) => format!("{typed}{close}"),
            None => typed.clone(),
        };
        match b.replace(p, p, text) {
            Some(at) => b.caret(at + 1),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// `open` and `close` form an empty auto-closed pair.
fn is_pair(open: char, close: char) -> bool {
    closer_of(open) == Some(close) || (is_quote(open) && open == close)
}

/// When the caret at `p` sits in a line's leading whitespace, the start of the deletion back to
/// the previous indent stop.
fn indent_stop_start(doc: &Document, p: usize, cfg: &OpsConfig) -> Option<usize> {
    let line = doc.char_to_line(p);
    let ls = doc.line_start(line);
    if p <= ls {
        return None;
    }
    let prefix = doc.rope().get_slice(ls..p)?;
    if !prefix.chars().all(is_indent_char) {
        return None;
    }
    let tab = cfg.tab_width;
    let col = prefix.chars().fold(0, |c, ch| advance_column(c, ch, tab));
    let w = indent_width(&cfg.indent_unit, tab).min(MAX_INDENT_WIDTH);
    let stop = col.saturating_sub(1) / w * w;
    let mut v = 0;
    let mut i = 0;
    for ch in prefix.chars() {
        if v >= stop {
            break;
        }
        v = advance_column(v, ch, tab);
        i += 1;
    }
    Some(ls + i.min(p - ls - 1))
}

/// Delete each non-empty selection; returns false for an empty one (left to the caller).
fn delete_selection(b: &mut EditBuilder, s: &Selection) -> bool {
    if s.is_empty() {
        return false;
    }
    match b.replace(s.start(), s.end(), "") {
        Some(at) => b.caret(at),
        None => b.keep(s),
    }
    true
}

/// Delete `start..end` for a caret.
fn delete_for_caret(b: &mut EditBuilder, s: &Selection, start: usize, end: usize) {
    if start == end {
        b.keep(s);
        return;
    }
    match b.replace(start, end, "") {
        Some(at) => b.caret(at),
        None => b.keep(s),
    }
}

/// Backspace: delete the selection, or the char before each caret. Between an empty pair
/// (`()`, `""`, …) delete both. In leading whitespace, delete back to the previous indent stop.
///
/// "Char" means a user-perceived char: combining marks and emoji sequences go as one.
pub fn backspace(doc: &Document, sels: &SelectionSet, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        if delete_selection(&mut b, s) {
            continue;
        }
        let p = s.head;
        if p == 0 {
            b.keep(s);
            continue;
        }
        let pair = doc.char_at(p - 1).zip(doc.char_at(p)).is_some_and(|(a, c)| is_pair(a, c));
        let (start, end) = if cfg.auto_close && pair {
            (p - 1, p + 1)
        } else if let Some(start) = indent_stop_start(doc, p, cfg) {
            (start, p)
        } else {
            (prev_boundary(doc, p), p)
        };
        delete_for_caret(&mut b, s, start, end);
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Delete: the selection, or the char after each caret.
pub fn delete_forward(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        if !delete_selection(&mut b, s) {
            delete_for_caret(&mut b, s, s.head, next_boundary(doc, s.head));
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Alt+Backspace: delete to the previous word start (or the selection).
///
/// Stops at word starts and word ends, so a whitespace run goes on its own; at a line start it
/// joins with the previous line.
pub fn delete_word_backward(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        if !delete_selection(&mut b, s) {
            delete_for_caret(&mut b, s, word_left(doc, s.head, true), s.head);
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Alt+Delete: delete to the next word end (or the selection).
///
/// Stops at word ends and word starts, so a whitespace run goes on its own; at a line end it
/// joins the next line (dropping its indentation).
pub fn delete_word_forward(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        if !delete_selection(&mut b, s) {
            delete_for_caret(&mut b, s, s.head, word_right(doc, s.head, true));
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// The last char before `pos` on its line that is not a space or tab.
fn last_non_blank_before(doc: &Document, pos: usize, line_start: usize) -> Option<char> {
    let mut it = doc.rope().get_chars_at(pos)?;
    let mut i = pos;
    while i > line_start {
        let c = it.prev()?;
        i -= 1;
        if !is_indent_char(c) {
            return Some(c);
        }
    }
    None
}

/// Enter: newline keeping the current indentation; one extra level after an opening bracket;
/// between a bracket pair (`{|}`) put the closer on its own line.
pub fn newline(doc: &Document, sels: &SelectionSet, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        let (start, end) = (s.start(), s.end());
        let line = doc.char_to_line(start);
        let ls = doc.line_start(line);
        let indent = indent_prefix(doc, line, start - ls);
        let mut text = String::with_capacity(2 * indent.len() + cfg.indent_unit.len() + 2);
        text.push('\n');
        text.push_str(&indent);
        let mut caret = text.chars().count();
        if let Some(close) = last_non_blank_before(doc, start, ls).and_then(closer_of) {
            text.push_str(&cfg.indent_unit);
            caret = text.chars().count();
            if doc.char_at(end) == Some(close) {
                text.push('\n');
                text.push_str(&indent);
            }
        }
        match b.replace(start, end, text) {
            Some(at) => b.caret(at + caret),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Shift+Cmd+Enter (Complete Current Statement) is out of scope here; this inserts a newline
/// after the current line with matching indentation (Shift+Enter, "Start New Line").
///
/// After a line ending in an opening bracket the new line gets one more level (the document's
/// detected indent unit). Several carets on one line add one line.
pub fn start_new_line(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    let mut unit: Option<String> = None;
    let mut done: Option<(usize, usize)> = None;
    for s in sels.ranges() {
        let line = doc.char_to_line(s.head);
        if let Some((l, caret)) = done
            && l == line
        {
            b.caret(caret);
            continue;
        }
        let ls = doc.line_start(line);
        let le = doc.line_end(line);
        let mut text = String::from("\n");
        text.push_str(&indent_prefix(doc, line, usize::MAX));
        if last_non_blank_before(doc, le, ls).and_then(closer_of).is_some() {
            text.push_str(unit.get_or_insert_with(|| doc.detect_indent_unit()));
        }
        let n = text.chars().count();
        match b.replace(le, le, text) {
            Some(at) => {
                b.caret(at + n);
                done = Some((line, at + n));
            }
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Map a selection through `changes`, its start and end with the given associations.
fn map_sel(s: &Selection, changes: &ChangeSet, start: Assoc, end: Assoc) -> Selection {
    let (a, e) = (changes.map_pos(s.start(), start), changes.map_pos(s.end(), end));
    directed(s, a, e.max(a))
}

/// Finish a line-based edit: selections mapped through the changes by `f`.
fn line_edit(
    doc: &Document,
    sels: &SelectionSet,
    changes: Vec<Change>,
    mut f: impl FnMut(&Selection, &ChangeSet) -> Selection,
) -> Edit {
    let Ok(changes) = ChangeSet::new(changes) else { return Edit::none(sels.clone()) };
    let len = changes.len_after(doc.len_chars());
    let mapped = sels.ranges().iter().map(|s| f(s, &changes)).collect();
    Edit { selections: SelectionSet::from_vec(mapped, sels.primary_index()).clamp(len), changes }
}

/// The configured indent unit, never empty.
fn unit_or_spaces(cfg: &OpsConfig) -> String {
    if cfg.indent_unit.is_empty() {
        " ".repeat(cfg.tab_width.clamp(1, MAX_INDENT_WIDTH))
    } else {
        cfg.indent_unit.clone()
    }
}

/// Tab: with a multi-line selection indent every line; otherwise insert indentation up to the
/// next indent stop.
///
/// When any selection spans lines, every non-empty line touched by any selection gets one
/// indent unit. Otherwise each selection is replaced by spaces up to the next stop (a tab when
/// the indent unit uses tabs).
pub fn indent(doc: &Document, sels: &SelectionSet, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let unit = unit_or_spaces(cfg);
    let multi = sels.ranges().iter().any(|s| doc.char_to_line(s.start()) != doc.char_to_line(s.end()));
    if multi {
        let mut changes = Vec::new();
        for (l0, l1) in line_blocks(doc, sels.ranges(), false) {
            for l in l0..=l1 {
                if doc.line_len(l) > 0 {
                    changes.push(Change::insert(doc.line_start(l), unit.as_str()));
                }
            }
        }
        return line_edit(doc, &sels, changes, |s, cs| map_sel(s, cs, Assoc::Before, Assoc::Before));
    }
    let w = indent_width(&unit, cfg.tab_width).min(MAX_INDENT_WIDTH);
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        let text = if unit.contains('\t') {
            unit.clone()
        } else {
            " ".repeat(w - visual_column(doc, s.start(), cfg.tab_width) % w)
        };
        let n = text.chars().count();
        match b.replace(s.start(), s.end(), text) {
            Some(at) => b.caret(at + n),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Shift+Tab: remove one indent level from every line touched by a selection.
///
/// One level is a leading tab, or up to one indent width of leading spaces.
pub fn unindent(doc: &Document, sels: &SelectionSet, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let w = indent_width(&unit_or_spaces(cfg), cfg.tab_width).min(MAX_INDENT_WIDTH);
    let mut changes = Vec::new();
    for (l0, l1) in line_blocks(doc, sels.ranges(), false) {
        for l in l0..=l1 {
            let ls = doc.line_start(l);
            let n = match doc.rope().get_chars_at(ls) {
                Some(mut it) => match it.next() {
                    Some('\t') => 1,
                    Some(' ') => 1 + it.take(w - 1).take_while(|c| *c == ' ').count(),
                    _ => 0,
                },
                None => 0,
            };
            if n > 0 {
                changes.push(Change::delete(ls, ls + n.min(doc.line_len(l))));
            }
        }
    }
    line_edit(doc, &sels, changes, |s, cs| map_sel(s, cs, Assoc::Before, Assoc::Before))
}

/// Cmd+/: if every non-blank touched line is commented, uncomment; else comment all at the
/// minimum indentation (IntelliJ style, `// ` with a space). Caret moves down one line when
/// the selection is empty, like RustRover.
///
/// Each group of selections sharing lines toggles on its own; blank lines are left alone
/// (a block of only blank lines gets the prefix at its end). Uncommenting removes `// ` or `//`.
pub fn toggle_line_comment(doc: &Document, sels: &SelectionSet, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let Some(prefix) = cfg.line_comment.as_deref().filter(|p| !p.is_empty()) else { return Edit::none(sels) };
    let prefix_len = prefix.chars().count();
    let marker = format!("{prefix} ");
    let mut changes = Vec::new();
    for (l0, l1) in line_blocks(doc, sels.ranges(), false) {
        let lines: Vec<(usize, usize, bool)> = (l0..=l1)
            .map(|l| {
                let (indent, blank) = indent_info(doc, l);
                (l, indent, blank)
            })
            .collect();
        let code: Vec<&(usize, usize, bool)> = lines.iter().filter(|(_, _, blank)| !blank).collect();
        if code.is_empty() {
            for &(l, _, _) in &lines {
                changes.push(Change::insert(doc.line_end(l), marker.as_str()));
            }
            continue;
        }
        let commented = code.iter().all(|&&(l, indent, _)| starts_with_at(doc, doc.line_start(l) + indent, prefix));
        let min_indent = code.iter().map(|(_, indent, _)| *indent).min().unwrap_or(0);
        for &&(l, indent, _) in &code {
            let ls = doc.line_start(l);
            if commented {
                let at = ls + indent;
                let space = usize::from(doc.char_at(at + prefix_len) == Some(' '));
                changes.push(Change::delete(at, at + prefix_len + space));
            } else {
                changes.push(Change::insert(ls + min_indent, marker.as_str()));
            }
        }
    }
    let last_line = doc.len_lines().saturating_sub(1);
    line_edit(doc, &sels, changes, |s, cs| {
        if !s.is_empty() {
            return map_sel(s, cs, Assoc::Before, Assoc::After);
        }
        let pos = doc.offset_to_position(s.head);
        if pos.line >= last_line {
            return Selection::caret(cs.map_pos(s.head, Assoc::After));
        }
        let goal = s.goal_column.unwrap_or(pos.column);
        let below = doc.line_start(pos.line + 1) + goal.min(doc.line_len(pos.line + 1));
        let at = cs.map_pos(below, Assoc::Before);
        Selection { anchor: at, head: at, goal_column: Some(goal) }
    })
}

fn find_seq(hay: &[char], pat: &[char], from: usize) -> Option<usize> {
    if pat.is_empty() {
        return None;
    }
    (from..hay.len()).find(|&i| hay.get(i..i + pat.len()) == Some(pat))
}

fn rfind_seq(hay: &[char], pat: &[char]) -> Option<usize> {
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    (0..=hay.len() - pat.len()).rev().find(|&i| hay.get(i..i + pat.len()) == Some(pat))
}

/// Longest line scanned for an enclosing block comment around a caret.
const MAX_COMMENT_SCAN: usize = 100_000;

/// A block comment on the caret's line that contains `p`: (open marker start, close marker start).
fn enclosing_block_comment(doc: &Document, p: usize, open: &str, close: &str) -> Option<(usize, usize)> {
    let line = doc.char_to_line(p);
    let ls = doc.line_start(line);
    let le = doc.line_end(line);
    if le - ls > MAX_COMMENT_SCAN {
        return None;
    }
    let chars: Vec<char> = doc.rope().get_slice(ls..le)?.chars().collect();
    let open: Vec<char> = open.chars().collect();
    let close: Vec<char> = close.chars().collect();
    let col = p.checked_sub(ls)?;
    let o = rfind_seq(chars.get(..col)?, &open)?;
    let c = find_seq(&chars, &close, o + open.len())?;
    (c >= col).then_some((ls + o, ls + c))
}

/// Alt+Cmd+/: wrap the selection in a block comment, or unwrap if already wrapped.
///
/// A selection is unwrapped when it starts and ends with the markers or sits right between
/// them. A caret inside a block comment on its line unwraps it; elsewhere an empty comment is
/// inserted with the caret inside.
pub fn toggle_block_comment(doc: &Document, sels: &SelectionSet, cfg: &OpsConfig) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let Some((open, close)) = cfg.block_comment.as_ref().filter(|(o, c)| !o.is_empty() && !c.is_empty()) else {
        return Edit::none(sels);
    };
    let (ol, cl) = (open.chars().count(), close.chars().count());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        let (a, e) = (s.start(), s.end());
        if s.is_empty() {
            if let Some((o, c)) = enclosing_block_comment(doc, a, open, close) {
                let x = b.replace(o, o + ol, "");
                let _ = b.replace(c, c + cl, "");
                match x {
                    Some(x) => b.caret(x + a.saturating_sub(o + ol)),
                    None => b.keep(s),
                }
            } else {
                match b.replace(a, a, format!("{open}{close}")) {
                    Some(x) => b.caret(x + ol),
                    None => b.keep(s),
                }
            }
            continue;
        }
        let ranges = if e - a >= ol + cl && starts_with_at(doc, a, open) && starts_with_at(doc, e - cl, close) {
            (b.replace(a, a + ol, ""), b.replace(e - cl, e, ""))
        } else if a >= ol && starts_with_at(doc, a - ol, open) && starts_with_at(doc, e, close) {
            (b.replace(a - ol, a, ""), b.replace(e, e + cl, ""))
        } else {
            (b.replace(a, a, open.as_str()).map(|x| x + ol), b.replace(e, e, close.as_str()))
        };
        match ranges {
            (Some(x), Some(y)) => b.push(directed(s, x, y.max(x))),
            _ => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Cmd+D: duplicate the selection, or the whole line when the selection is empty.
///
/// A duplicated selection selects the new copy; a caret moves to the same column on the new
/// line. Several carets on one line duplicate it once; a caret on a line that also holds a
/// non-empty selection stays put.
pub fn duplicate(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let selected_lines = line_blocks(doc, sels.ranges().iter().filter(|s| !s.is_empty()), false);
    let mut b = EditBuilder::new(sels.len());
    let mut dup: Option<(usize, usize)> = None;
    for s in sels.ranges() {
        if !s.is_empty() {
            let text = doc.slice(s.range());
            let n = text.chars().count();
            match b.replace(s.end(), s.end(), text) {
                Some(at) => b.push(directed(s, at, at + n)),
                None => b.keep(s),
            }
            continue;
        }
        let line = doc.char_to_line(s.head);
        if block_of(&selected_lines, line).is_some() {
            b.keep(s);
            continue;
        }
        let ls = doc.line_start(line);
        let col = s.head - ls;
        let start = match dup {
            Some((l, start)) if l == line => Some(start),
            _ => {
                let le = doc.line_end(line);
                let text = format!("\n{}", doc.slice(ls..le));
                b.replace(le, le, text).map(|at| at + 1)
            }
        };
        match start {
            Some(start) => {
                dup = Some((line, start));
                b.caret(start + col);
            }
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Delete the lines touched by each selection (Cmd+Backspace in the editor).
///
/// The caret keeps its column on the line that moves into place (the previous line when the
/// last line is deleted).
pub fn delete_line(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let blocks = line_blocks(doc, sels.ranges(), true);
    let mut cols: Vec<Option<usize>> = vec![None; blocks.len()];
    for s in sels.ranges() {
        if let Some(slot) = block_of(&blocks, sel_lines(doc, s).0).and_then(|i| cols.get_mut(i))
            && slot.is_none()
        {
            *slot = Some(doc.offset_to_position(s.head).column);
        }
    }
    let primary = block_of(&blocks, sel_lines(doc, &sels.primary()).0).unwrap_or(0);
    let last_line = doc.len_lines().saturating_sub(1);
    let mut b = EditBuilder::new(blocks.len());
    for (i, &(l0, l1)) in blocks.iter().enumerate() {
        let col = cols.get(i).copied().flatten().unwrap_or(0);
        if l1 < last_line {
            if let Some(at) = b.replace(doc.line_start(l0), doc.line_start(l1 + 1), "") {
                b.caret(at + col.min(doc.line_len(l1 + 1)));
            }
        } else if l0 > 0 {
            let caret = b.map(doc.line_start(l0 - 1)) + col.min(doc.line_len(l0 - 1));
            b.replace(doc.line_end(l0 - 1), doc.len_chars(), "");
            b.caret(caret);
        } else {
            b.replace(0, doc.len_chars(), "");
            b.caret(0);
        }
    }
    b.finish(doc, primary, &sels)
}

/// Alt+Shift+Up/Down: move the touched lines up or down one line. `None` at the document edge.
pub fn move_lines(doc: &Document, sels: &SelectionSet, up: bool) -> Option<Edit> {
    let sels = sels.clamp(doc.len_chars());
    let blocks = line_blocks(doc, sels.ranges(), true);
    let last_line = doc.len_lines().saturating_sub(1);
    if (up && blocks.first()?.0 == 0) || (!up && blocks.last()?.1 >= last_line) {
        return None;
    }
    let mut changes = Vec::with_capacity(blocks.len());
    let mut shifts: Vec<isize> = Vec::with_capacity(blocks.len());
    for &(l0, l1) in &blocks {
        let block = doc.slice(doc.line_start(l0)..doc.line_end(l1));
        if up {
            let above_start = doc.line_start(l0 - 1);
            let above = doc.slice(above_start..doc.line_end(l0 - 1));
            shifts.push(-(above.chars().count() as isize + 1));
            changes.push(Change::new(above_start, doc.line_end(l1), format!("{block}\n{above}")));
        } else {
            let below = doc.slice(doc.line_start(l1 + 1)..doc.line_end(l1 + 1));
            shifts.push(below.chars().count() as isize + 1);
            changes.push(Change::new(doc.line_start(l0), doc.line_end(l1 + 1), format!("{below}\n{block}")));
        }
    }
    let changes = ChangeSet::new(changes).ok()?;
    let moved = sels
        .ranges()
        .iter()
        .map(|s| {
            let d = block_of(&blocks, sel_lines(doc, s).0).and_then(|i| shifts.get(i)).copied().unwrap_or(0);
            Selection {
                anchor: crate::util::shift(s.anchor, d),
                head: crate::util::shift(s.head, d),
                goal_column: s.goal_column,
            }
        })
        .collect();
    let len = changes.len_after(doc.len_chars());
    Some(Edit { selections: SelectionSet::from_vec(moved, sels.primary_index()).clamp(len), changes })
}

/// Ctrl+Shift+J: join the next line onto the current one (or all lines of the selection),
/// removing the leading whitespace of the joined line and a `//` comment prefix if both are comments.
///
/// Trailing whitespace of the upper line goes too; the parts are separated by one space (none
/// when either side is empty). The caret lands at the last join point.
pub fn join_lines(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let last_line = doc.len_lines().saturating_sub(1);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for s in sels.ranges() {
        let l0 = doc.char_to_line(s.start());
        let l1 = doc.char_to_line(s.end()).max(l0 + 1).min(last_line);
        if let Some(prev) = blocks.last_mut()
            && l0 <= prev.1
        {
            prev.1 = prev.1.max(l1);
            continue;
        }
        blocks.push((l0, l1));
    }
    let mut b = EditBuilder::new(blocks.len());
    let mut carets: Vec<Option<usize>> = vec![None; blocks.len()];
    for (i, &(l0, l1)) in blocks.iter().enumerate() {
        let mut prev_rest = 0;
        for k in l0..l1 {
            let ls = doc.line_start(k);
            let (indent_k, blank_k) = indent_info(doc, k);
            let trailing = match doc.rope().get_chars_at(doc.line_end(k)) {
                Some(mut it) => {
                    std::iter::from_fn(|| it.prev()).take(doc.line_len(k)).take_while(|c| is_indent_char(*c)).count()
                }
                None => 0,
            };
            let content_end = (doc.line_end(k) - trailing).max(prev_rest).max(ls);
            let next_start = doc.line_start(k + 1);
            let next_end = doc.line_end(k + 1);
            let (indent_n, _) = indent_info(doc, k + 1);
            let mut rest = next_start + indent_n;
            if starts_with_at(doc, ls + indent_k, "//") && starts_with_at(doc, rest, "//") {
                rest += 2;
                while rest < next_end && doc.char_at(rest).is_some_and(is_indent_char) {
                    rest += 1;
                }
            }
            let sep = if blank_k || content_end <= ls || rest >= next_end { "" } else { " " };
            if let Some(at) = b.replace(content_end, rest, sep)
                && let Some(slot) = carets.get_mut(i)
            {
                *slot = Some(at);
            }
            prev_rest = rest;
        }
    }
    let Ok(changes) = ChangeSet::new(b.into_changes()) else { return Edit::none(sels) };
    // Selections without a join (on the last line) map through the changes.
    let out = sels
        .ranges()
        .iter()
        .map(|s| match block_of(&blocks, doc.char_to_line(s.start())).and_then(|i| carets.get(i).copied().flatten()) {
            Some(at) => Selection::caret(at),
            None => map_sel(s, &changes, Assoc::After, Assoc::After),
        })
        .collect();
    let len = changes.len_after(doc.len_chars());
    let selections = SelectionSet::from_vec(out, sels.primary_index()).clamp(len);
    Edit { changes, selections }
}

/// Reorder the lines of each block (or of the whole document when every selection is empty).
fn permute_lines(doc: &Document, sels: &SelectionSet, f: impl Fn(&mut Vec<String>)) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let blocks = if sels.ranges().iter().all(Selection::is_empty) {
        let mut last = doc.len_lines().saturating_sub(1);
        if last > 0 && doc.line_len(last) == 0 {
            last -= 1;
        }
        vec![(0, last)]
    } else {
        line_blocks(doc, sels.ranges().iter().filter(|s| !s.is_empty()), false)
    };
    let mut changes = Vec::new();
    for (l0, l1) in blocks {
        if l1 <= l0 {
            continue;
        }
        let original: Vec<String> = (l0..=l1).map(|l| doc.slice(doc.line_start(l)..doc.line_end(l))).collect();
        let mut lines = original.clone();
        f(&mut lines);
        if lines != original {
            changes.push(Change::new(doc.line_start(l0), doc.line_end(l1), lines.join("\n")));
        }
    }
    // Same lines, same total length: the selections stay where they are.
    line_edit(doc, &sels, changes, |s, _| Selection::new(s.anchor, s.head))
}

/// Sort the selected lines (whole document when the selection is empty).
///
/// Lexicographic by code point; a trailing empty line of the document stays last.
pub fn sort_lines(doc: &Document, sels: &SelectionSet) -> Edit {
    permute_lines(doc, sels, |lines| lines.sort())
}

/// Reverse the selected lines (whole document when the selection is empty).
pub fn reverse_lines(doc: &Document, sels: &SelectionSet) -> Edit {
    permute_lines(doc, sels, |lines| lines.reverse())
}

/// Shift+Cmd+U: toggle case of the selection (or the word at the caret): if it has any
/// lowercase letter, upper-case it; otherwise lower-case it.
pub fn toggle_case(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        let range = if s.is_empty() { word_range_at(doc, s.head) } else { s.range() };
        if range.is_empty() || !b.fits(range.start) {
            b.keep(s);
            continue;
        }
        let text = doc.slice(range.clone());
        let toggled = if text.chars().any(char::is_lowercase) { text.to_uppercase() } else { text.to_lowercase() };
        if toggled == text {
            b.keep(s);
            continue;
        }
        let n = toggled.chars().count();
        match b.replace(range.start, range.end, toggled) {
            Some(at) if s.is_empty() => b.caret(at + (s.head - range.start).min(n)),
            Some(at) => b.push(directed(s, at, at + n)),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Swap the chars on either side of each caret.
///
/// The caret moves past the swapped pair; at a line end the two chars before it are swapped
/// (macOS Ctrl+T). Non-empty selections and carets at a line start are left alone.
pub fn transpose(doc: &Document, sels: &SelectionSet) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        let p = s.head;
        let line = doc.char_to_line(p);
        let (ls, le) = (doc.line_start(line), doc.line_end(line));
        let at = if !s.is_empty() || p <= ls {
            None
        } else if p >= le {
            (le >= ls + 2).then(|| le - 2)
        } else {
            Some(p - 1)
        };
        let swap = at.filter(|a| b.fits(*a)).and_then(|a| Some((a, doc.char_at(a)?, doc.char_at(a + 1)?)));
        let Some((a, x, y)) = swap else {
            b.keep(s);
            continue;
        };
        match b.replace(a, a + 2, format!("{y}{x}")) {
            Some(post) => b.caret(post + 2),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

/// Text for Copy. Empty selections copy their whole lines; the flag says so (whole-line paste).
/// Multiple carets join with `\n`.
///
/// Whole lines always end with `\n` (several carets on one line copy it once). When any
/// selection is non-empty, only the non-empty selections are copied.
pub fn copy_text(doc: &Document, sels: &SelectionSet) -> (String, bool) {
    let sels = sels.clamp(doc.len_chars());
    if sels.ranges().iter().all(Selection::is_empty) {
        let mut out = String::new();
        let mut last = None;
        for s in sels.ranges() {
            let line = doc.char_to_line(s.head);
            if last == Some(line) {
                continue;
            }
            last = Some(line);
            out.push_str(&doc.slice(doc.line_start(line)..doc.line_end(line)));
            out.push('\n');
        }
        return (out, true);
    }
    let parts: Vec<String> = sels.ranges().iter().filter(|s| !s.is_empty()).map(|s| doc.slice(s.range())).collect();
    (parts.join("\n"), false)
}

/// Cut: like [`copy_text`] plus the edit that removes it (whole lines for empty selections).
pub fn cut(doc: &Document, sels: &SelectionSet) -> (String, bool, Edit) {
    let (text, whole_line) = copy_text(doc, sels);
    if whole_line {
        return (text, true, delete_line(doc, sels));
    }
    let sels = sels.clamp(doc.len_chars());
    let mut b = EditBuilder::new(sels.len());
    for s in sels.ranges() {
        if !delete_selection(&mut b, s) {
            b.keep(s);
        }
    }
    (text, false, b.finish(doc, sels.primary_index(), &sels))
}

/// Paste. A whole-line clip with empty carets inserts above the caret line. When the clip has
/// as many lines as there are carets, each caret gets one line.
pub fn paste(doc: &Document, sels: &SelectionSet, text: &str, whole_line: bool) -> Edit {
    let sels = sels.clamp(doc.len_chars());
    let text = normalize_newlines(text);
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let parts: Vec<&str> = body.split('\n').collect();
    let per_caret = sels.len() > 1 && parts.len() == sels.len();
    let mut b = EditBuilder::new(sels.len());
    if whole_line && sels.ranges().iter().all(Selection::is_empty) {
        let mut last = None;
        for (i, s) in sels.ranges().iter().enumerate() {
            let line = doc.char_to_line(s.head);
            if last != Some(line) {
                last = Some(line);
                let ls = doc.line_start(line);
                let clip =
                    if per_caret { format!("{}\n", parts.get(i).copied().unwrap_or("")) } else { format!("{body}\n") };
                b.replace(ls, ls, clip);
            }
            b.caret(b.map(s.head));
        }
        return b.finish(doc, sels.primary_index(), &sels);
    }
    for (i, s) in sels.ranges().iter().enumerate() {
        let clip = if per_caret { parts.get(i).copied().unwrap_or("") } else { text.as_str() };
        let n = clip.chars().count();
        match b.replace(s.start(), s.end(), clip) {
            Some(at) => b.caret(at + n),
            None => b.keep(s),
        }
    }
    b.finish(doc, sels.primary_index(), &sels)
}

#[cfg(test)]
mod tests;
