//! Highlights kept up to date across edits ([`IncrementalHighlighter`]).
//!
//! The result after any sequence of [`update`](IncrementalHighlighter::update)s is exactly
//! [`highlight`](crate::highlight) of the current text; only the amount of work differs.
//!
//! - **Rust** is truly incremental. `rustc_lexer` tokenization is context-free from any token
//!   boundary (only a shebang line is special, and such texts are re-lexed in full), so tokens are
//!   re-lexed from a little before the edit until a new token boundary coincides with a shifted
//!   old one at or after the edit. Classification then re-runs only over a window of significant
//!   tokens: it starts at a position the classification loop visited before, at least
//!   [`LOOKAHEAD`] significant tokens before the change (the turbofish lookahead in `is_call` reads
//!   up to 68 tokens ahead) and before any attribute whose bracket was left unbalanced (an opener's
//!   partner depends only on the brackets after it, so balanced attributes before the window are
//!   unaffected); it stops at the first loop visit at least [`LOOKBEHIND`] tokens past the
//!   change that was also a visit before (from there the loop sees exactly the old inputs). Only
//!   the lines covered by the re-lexed or re-classified tokens are rebuilt and spliced in.
//! - **Every other language** (plain text has no spans and is trivially cheap) is re-lexed in full on each update (their lexers use document-wide
//!   context: C `#define`s, Markdown setext headings and reference links, JS regex/template
//!   context, Python statement lookahead, …), which is correct by construction.

use std::ops::Range;

use ra_ap_rustc_lexer::{self as lexer, FrontmatterAllowed, TokenKind};

use crate::common::{self, SpanSink};
use crate::rust::{self, Cursor, Sig, Tok};
use crate::{FoldRange, HighlightKind, Highlights, Language, LineSpan};

/// Chars re-lexed before an edit: more than `rustc_lexer` ever peeks past a token's end.
const LEX_BACKUP: usize = 16;
/// Significant tokens re-classified before the first changed one (classifying position `p`
/// reads positions up to `p + 68`).
const LOOKAHEAD: usize = 70;
/// Significant tokens a classification reads behind its position.
const LOOKBEHIND: usize = 2;

/// Per-token classification-loop marks.
const NOT_VISITED: u8 = 0;
const VISITED: u8 = 1;
/// A visited `#` starting an attribute whose `[` has no `]` partner (its partner may lie after
/// any later edit, so windows never start after it).
const OPEN_ATTRIBUTE: u8 = 2;

/// Highlights kept up to date across edits.
#[derive(Clone, Debug)]
pub struct IncrementalHighlighter {
    language: Language,
    highlights: Highlights,
    /// Token stream and classification for Rust; `None` for other languages.
    rust: Option<RustState>,
}

#[derive(Clone, Debug, Default)]
struct RustState {
    toks: Vec<Tok>,
    kinds: Vec<HighlightKind>,
    /// One mark per token (`NOT_VISITED`, `VISITED`, `OPEN_ATTRIBUTE`).
    marks: Vec<u8>,
    /// The text started with a shebang line.
    shebang: bool,
}

impl IncrementalHighlighter {
    /// Full lex of `text`.
    pub fn new(language: Language, text: &str) -> Self {
        let mut h = Self { language, highlights: Highlights::default(), rust: None };
        h.reset(language, text);
        h
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn highlights(&self) -> &Highlights {
        &self.highlights
    }

    /// `text` is the whole new text; chars `start..old_end` of the previous text were replaced by
    /// chars `start..new_end` of `text` (char offsets; everything outside is unchanged). Returns
    /// the range of lines (in the new text) whose spans may have changed: lines before it are
    /// unchanged, and lines from its end on are the old lines shifted by the line-count change.
    /// Inconsistent inputs fall back to a full re-lex.
    pub fn update(&mut self, text: &str, start: usize, old_end: usize, new_end: usize) -> Range<usize> {
        match self.language {
            Language::Rust => {
                if let Some(range) = self.update_rust(text, start, old_end, new_end) {
                    return range;
                }
                let old = std::mem::take(&mut self.highlights);
                self.reset(Language::Rust, text);
                changed_lines(&old, &self.highlights)
            }
            language => {
                let new = crate::highlight(language, text);
                let range = changed_lines(&self.highlights, &new);
                self.highlights = new;
                range
            }
        }
    }

    /// Full re-lex (language change, unknown edit).
    pub fn reset(&mut self, language: Language, text: &str) {
        self.language = language;
        if language == Language::Rust {
            let toks = rust::lex(text);
            let sig = Sig::new(text, &toks);
            let ends = sig.group_ends();
            let mut kinds: Vec<HighlightKind> = toks.iter().map(|t| rust::base_kind(t.kind)).collect();
            let mut marks = vec![NOT_VISITED; toks.len()];
            let close_of = |o: usize| ends.get(o).copied().flatten();
            run_classify(&sig, &close_of, 0, &mut kinds, &mut marks, &|_, _| false);
            let mut sink = SpanSink::with_capacity(toks.len());
            for (tok, &kind) in toks.iter().zip(&kinds) {
                sink.push(tok.cstart, tok.cend, kind);
            }
            self.highlights = common::to_highlights(&common::LineIndex::new(text), &sink.into_spans());
            let shebang = lexer::strip_shebang(text).is_some();
            self.rust = Some(RustState { toks, kinds, marks, shebang });
        } else {
            self.rust = None;
            self.highlights = crate::highlight(language, text);
        }
    }

    /// Fold regions of `text`, which must be the text of the last `new`/`update`/`reset`. Rust
    /// reuses the cached tokens instead of lexing again; equal to
    /// [`folding_ranges`](crate::folding_ranges).
    pub fn folding_ranges(&self, text: &str) -> Vec<FoldRange> {
        match &self.rust {
            Some(st) if self.language == Language::Rust => rust::folding_ranges_from(text, &st.toks),
            _ => crate::folding_ranges(self.language, text),
        }
    }

    /// The incremental Rust path; `None` asks for a full re-lex.
    fn update_rust(&mut self, text: &str, start: usize, old_end: usize, new_end: usize) -> Option<Range<usize>> {
        let old_line_count = self.highlights.len_lines();
        let st = self.rust.as_mut()?;
        // A shebang line changes how the start is tokenized: such texts are lexed in full.
        if st.shebang || lexer::strip_shebang(text).is_some() {
            return None;
        }
        let old_chars = st.toks.last().map_or(0, |t| t.cend);
        let old_bytes = st.toks.last().map_or(0, |t| t.end);
        if start > old_end || old_end > old_chars || start > new_end {
            return None;
        }

        // 1. Re-lex from a token boundary a little before the edit until the streams re-join.
        let back = start.saturating_sub(LEX_BACKUP);
        let restart = st.toks.partition_point(|t| t.cend <= back);
        let mut cur = match st.toks.get(restart) {
            Some(t) => Cursor { byte: t.start, ch: t.cstart, line: t.line, line_start: t.cstart.saturating_sub(t.col) },
            None if st.toks.is_empty() => Cursor { byte: 0, ch: 0, line: 0, line_start: 0 },
            None => return None,
        };
        let rest = text.get(cur.byte..)?;
        let mut fresh: Vec<Tok> = Vec::new();
        let mut join: Option<usize> = None;
        for token in lexer::tokenize(rest, FrontmatterAllowed::No) {
            let len = token.len as usize;
            if len == 0 {
                break;
            }
            rust::push_tok(&mut fresh, text, &mut cur, len, token.kind);
            if cur.ch >= new_end {
                // The same suffix follows both boundaries: from here the old tokens are valid.
                let old_pos = (cur.ch - new_end).checked_add(old_end)?;
                let j = st.toks.partition_point(|t| t.cstart < old_pos);
                if j >= restart && st.toks.get(j).is_some_and(|t| t.cstart == old_pos) {
                    join = Some(j);
                    break;
                }
            }
        }
        let join = match join {
            Some(j) => {
                let old = st.toks.get(j)?;
                // The suffixes must have the same length, or the caller described the edit wrongly.
                if text.len().checked_sub(cur.byte)? != old_bytes.checked_sub(old.start)? {
                    return None;
                }
                j
            }
            None => {
                if cur.byte < text.len() {
                    let rest = text.len() - cur.byte;
                    rust::push_tok(&mut fresh, text, &mut cur, rest, TokenKind::Unknown);
                }
                st.toks.len()
            }
        };

        // 2. Splice the tokens, shifting the tail.
        if let Some(first_tail) = st.toks.get(join).copied() {
            let (db, dc) = (diff(cur.byte, first_tail.start), diff(cur.ch, first_tail.cstart));
            let dl = diff(cur.line, first_tail.line);
            for t in st.toks.get_mut(join..).unwrap_or(&mut []) {
                let same_line = t.line == first_tail.line;
                t.start = shift(t.start, db);
                t.end = shift(t.end, db);
                t.cstart = shift(t.cstart, dc);
                t.cend = shift(t.cend, dc);
                t.line = shift(t.line, dl);
                t.end_line = shift(t.end_line, dl);
                if same_line {
                    t.col = t.cstart.saturating_sub(cur.line_start);
                }
            }
        }
        let n0 = restart;
        let n1 = restart + fresh.len();
        let base: Vec<HighlightKind> = fresh.iter().map(|t| rust::base_kind(t.kind)).collect();
        if join > st.toks.len() || restart > join {
            return None;
        }
        st.toks.splice(restart..join, fresh);
        st.kinds.splice(restart..join, base);
        st.marks.splice(restart..join, std::iter::repeat_n(NOT_VISITED, n1 - n0));

        // 3. Re-classify a window around the change.
        let sig = Sig::new(text, &st.toks);
        let cs0 = sig.idx.partition_point(|&t| t < n0);
        let cs1 = sig.idx.partition_point(|&t| t < n1);
        let mut p0 = cs0.saturating_sub(LOOKAHEAD);
        while p0 > 0 && sig.idx.get(p0).and_then(|&t| st.marks.get(t)).is_none_or(|&m| m == NOT_VISITED) {
            p0 -= 1;
        }
        let p0_tok = sig.idx.get(p0).copied().unwrap_or(st.toks.len());
        if let Some(t) = st.marks.get(..p0_tok).and_then(|m| m.iter().position(|&m| m == OPEN_ATTRIBUTE)) {
            p0 = sig.idx.partition_point(|&i| i < t);
        }
        let close_of = |o: usize| closer(&sig, o);
        let stop = |p: usize, marks: &[u8]| {
            p >= cs1 + LOOKBEHIND && sig.idx.get(p).and_then(|&t| marks.get(t)).is_some_and(|&m| m != NOT_VISITED)
        };
        let p_stop = run_classify(&sig, &close_of, p0, &mut st.kinds, &mut st.marks, &stop);

        // 4. Rebuild the lines covered by the changed tokens.
        let t_a = n0.min(sig.idx.get(p0).copied().unwrap_or(n0));
        let t_b = n1.max(sig.idx.get(p_stop).copied().unwrap_or(st.toks.len()));
        let a_lo = st.toks.get(t_a).map_or(start, |t| t.cstart).min(start);
        let a_hi = t_b.checked_sub(1).and_then(|i| st.toks.get(i)).map_or(new_end, |t| t.cend).max(new_end);
        let new_line_count = st.toks.last().map_or(1, |t| t.end_line + 1);
        let (first_line, lines) = rebuild_lines(text, &st.toks, &st.kinds, a_lo, a_hi)?;
        let last_line = first_line + lines.len().checked_sub(1)?;
        let old_last = (last_line + old_line_count).checked_sub(new_line_count)?;
        if old_last < first_line || old_last >= old_line_count {
            return None;
        }
        self.highlights.lines.splice(first_line..=old_last, lines);
        if self.highlights.lines.len() != new_line_count {
            return None;
        }
        Some(first_line..last_line + 1)
    }
}

/// Runs the classification loop (the same handlers as `rust::classify`) from the visit position
/// `p` until `stop` holds at a visit or the tokens end, rewriting `kinds` and `marks` for every
/// token it passes. Returns the position it stopped at.
fn run_classify(
    sig: &Sig<'_>,
    close_of: &dyn Fn(usize) -> Option<usize>,
    mut p: usize,
    kinds: &mut [HighlightKind],
    marks: &mut [u8],
    stop: &dyn Fn(usize, &[u8]) -> bool,
) -> usize {
    while p < sig.len() && !stop(p, marks) {
        let kind = sig.kind(p);
        let (next, mark) = match kind {
            Some(TokenKind::Pound) => {
                let mark = attribute_mark(sig, close_of, p);
                (rust::attribute(sig, close_of, p, kinds), mark)
            }
            Some(TokenKind::Ident | TokenKind::RawIdent) => (rust::ident(sig, p, kinds), VISITED),
            _ => (p + 1, VISITED),
        };
        let next = next.max(p + 1);
        // Handlers write every significant token in `p..next` except when they skip just `p`
        // without classifying it (a lone `#`, an operator, …): that one keeps its base kind.
        if next == p + 1
            && !matches!(kind, Some(TokenKind::Ident | TokenKind::RawIdent))
            && let Some(k) = kind
            && let Some(slot) = sig.idx.get(p).and_then(|&t| kinds.get_mut(t))
        {
            *slot = rust::base_kind(k);
        }
        if let Some(m) = sig.idx.get(p).and_then(|&t| marks.get_mut(t)) {
            *m = mark;
        }
        for q in p + 1..next {
            if let Some(m) = sig.idx.get(q).and_then(|&t| marks.get_mut(t)) {
                *m = NOT_VISITED;
            }
        }
        p = next;
    }
    p
}

/// `OPEN_ATTRIBUTE` for a `#[`/`#![` whose `[` has no `]` partner, else `VISITED`.
fn attribute_mark(sig: &Sig<'_>, close_of: &dyn Fn(usize) -> Option<usize>, p: usize) -> u8 {
    let open = if sig.is(p + 1, TokenKind::OpenBracket) {
        p + 1
    } else if sig.is(p + 1, TokenKind::Bang) && sig.is(p + 2, TokenKind::OpenBracket) {
        p + 2
    } else {
        return VISITED;
    };
    if close_of(open).is_some_and(|c| sig.is(c, TokenKind::CloseBracket)) { VISITED } else { OPEN_ATTRIBUTE }
}

/// The closer balancing the opener at `open` (any bracket kinds), like `Sig::group_ends`.
fn closer(sig: &Sig<'_>, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for q in open..sig.len() {
        match sig.kind(q) {
            Some(TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace) => depth += 1,
            Some(TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(q);
                }
            }
            _ => {}
        }
    }
    None
}

/// Per-line spans for every line touching chars `a_lo..=a_hi`: (first line, spans per line).
fn rebuild_lines(
    text: &str,
    toks: &[Tok],
    kinds: &[HighlightKind],
    a_lo: usize,
    a_hi: usize,
) -> Option<(usize, Vec<Vec<LineSpan>>)> {
    // Locate a_lo: scan from the start of the token containing it.
    let ti = toks.partition_point(|t| t.cend <= a_lo).min(toks.len().saturating_sub(1));
    let (byte, mut ch, mut line, mut line_start) = match toks.get(ti) {
        Some(t) => (t.start, t.cstart, t.line, t.cstart.saturating_sub(t.col)),
        None => (0, 0, 0, 0),
    };
    let mut chars = text.get(byte..)?.chars();
    while ch < a_lo {
        let c = chars.next()?;
        ch += 1;
        if c == '\n' {
            line += 1;
            line_start = ch;
        }
    }
    // Line starts from `line` through the line holding a_hi, and where that line ends.
    let first_line = line;
    let mut starts = vec![line_start];
    let mut end = None;
    for c in chars {
        if c == '\n' {
            if ch >= a_hi {
                end = Some(ch);
                break;
            }
            starts.push(ch + 1);
        }
        ch += 1;
    }
    // `end`: the `\n` ending the last line, or the end of the text.
    let (last_end, has_next) = match end {
        Some(e) => (e, true),
        None => (ch, false),
    };
    let n = starts.len();
    let line_end = |i: usize| match starts.get(i + 1) {
        Some(&next) => next.saturating_sub(1),
        None => last_end,
    };
    let next_start = |i: usize| match starts.get(i + 1) {
        Some(&next) => Some(next),
        None if has_next => Some(last_end + 1),
        None => None,
    };
    let lo = starts.first().copied().unwrap_or(0);
    let mut lines: Vec<Vec<LineSpan>> = vec![Vec::new(); n];
    let first_tok = toks.partition_point(|t| t.cend <= lo);
    for (tok, &kind) in toks.get(first_tok..)?.iter().zip(kinds.get(first_tok..)?) {
        if tok.cstart >= last_end {
            break;
        }
        if kind == HighlightKind::Plain {
            continue;
        }
        let mut s = tok.cstart.max(lo);
        let e = tok.cend;
        let mut li = starts.partition_point(|&st| st <= s).saturating_sub(1);
        loop {
            let ls = starts.get(li).copied().unwrap_or(lo);
            let seg_end = e.min(line_end(li));
            if seg_end > s
                && let Some(spans) = lines.get_mut(li)
            {
                common::push_line_span(spans, LineSpan { start: s - ls, end: seg_end - ls, kind });
            }
            match next_start(li) {
                Some(next) if next < e && li + 1 < n => {
                    li += 1;
                    s = next;
                }
                _ => break,
            }
        }
    }
    Some((first_line, lines))
}

/// Lines that differ between two highlight sets (prefix and suffix compared), in `new`'s lines.
fn changed_lines(old: &Highlights, new: &Highlights) -> Range<usize> {
    let (a, b) = (&old.lines, &new.lines);
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    prefix..b.len() - suffix
}

fn diff(a: usize, b: usize) -> isize {
    (a as isize).wrapping_sub(b as isize)
}

fn shift(x: usize, d: isize) -> usize {
    x.checked_add_signed(d).unwrap_or(0)
}

#[cfg(test)]
mod tests;
