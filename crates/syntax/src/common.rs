//! Shared plumbing for the language modules: document-level spans in char offsets and their
//! conversion to per-line spans, bracket matching and fold-range helpers.

use crate::{FoldKind, FoldRange, HighlightKind, Highlights, LineSpan};

/// A highlight span over the whole document, in char offsets (it may cross lines).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: HighlightKind,
}

/// Collects spans in document order. Plain and empty spans are dropped, a span overlapping the
/// previous one is clipped, and touching spans of the same kind are merged, so the result is
/// always sorted and non-overlapping.
#[derive(Debug, Default)]
pub(crate) struct SpanSink {
    spans: Vec<Span>,
}

impl SpanSink {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self { spans: Vec::with_capacity(capacity) }
    }

    pub(crate) fn push(&mut self, start: usize, end: usize, kind: HighlightKind) {
        if kind == HighlightKind::Plain {
            return;
        }
        let start = self.spans.last().map_or(start, |last| start.max(last.end));
        if end <= start {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.kind == kind
            && last.end == start
        {
            last.end = end;
            return;
        }
        self.spans.push(Span { start, end, kind });
    }

    /// Drops every span at or after `offset`, clipping one that straddles it.
    pub(crate) fn truncate_from(&mut self, offset: usize) {
        while self.spans.last().is_some_and(|s| s.start >= offset) {
            self.spans.pop();
        }
        if let Some(last) = self.spans.last_mut()
            && last.end > offset
        {
            last.end = offset;
        }
    }

    pub(crate) fn into_spans(self) -> Vec<Span> {
        self.spans
    }
}

/// Char offsets of line starts. Lines are split on `\n` exactly: a text with `k` newlines has
/// `k + 1` lines.
#[derive(Clone, Debug)]
pub(crate) struct LineIndex {
    starts: Vec<usize>,
    total: usize,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        starts.push(0);
        let mut chars = 0usize;
        for &b in text.as_bytes() {
            // Count UTF-8 lead bytes only: one per char.
            if b & 0xC0 != 0x80 {
                chars += 1;
            }
            if b == b'\n' {
                starts.push(chars);
            }
        }
        Self { starts, total: chars }
    }

    pub(crate) fn from_chars(chars: &[char]) -> Self {
        let mut starts = vec![0];
        starts.extend(chars.iter().enumerate().filter(|(_, c)| **c == '\n').map(|(i, _)| i + 1));
        Self { starts, total: chars.len() }
    }

    pub(crate) fn len_lines(&self) -> usize {
        self.starts.len()
    }

    /// Total number of chars in the text.
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    /// Line containing char `offset` (offsets past the end map to the last line).
    pub(crate) fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    pub(crate) fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.total)
    }

    /// Char offset just past the line's content: its `\n`, or the end of the text.
    pub(crate) fn line_end(&self, line: usize) -> usize {
        match self.starts.get(line.saturating_add(1)) {
            Some(&next) => next.saturating_sub(1),
            None => self.total,
        }
    }

    pub(crate) fn next_line_start(&self, line: usize) -> Option<usize> {
        self.starts.get(line.saturating_add(1)).copied()
    }
}

/// A text as chars plus its line index, for the hand-written lexers.
#[derive(Clone, Debug)]
pub(crate) struct Doc {
    pub(crate) chars: Vec<char>,
    pub(crate) index: LineIndex,
}

impl Doc {
    pub(crate) fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let index = LineIndex::from_chars(&chars);
        Self { chars, index }
    }

    pub(crate) fn len(&self) -> usize {
        self.chars.len()
    }

    /// The char at `i`, or `'\0'` past the end. Callers bound their loops by [`Doc::len`].
    pub(crate) fn at(&self, i: usize) -> char {
        self.chars.get(i).copied().unwrap_or('\0')
    }

    /// Offset of the end of the line containing `i` (its `\n`, or the end of the text).
    pub(crate) fn line_end_at(&self, i: usize) -> usize {
        self.index.line_end(self.index.line_of(i))
    }

    /// The chars of `start..end`, clamped to the text.
    pub(crate) fn slice(&self, start: usize, end: usize) -> &[char] {
        let end = end.min(self.chars.len());
        self.chars.get(start.min(end)..end).unwrap_or(&[])
    }

    pub(crate) fn string(&self, start: usize, end: usize) -> String {
        self.slice(start, end).iter().collect()
    }

    pub(crate) fn starts_with_at(&self, i: usize, pat: &str) -> bool {
        pat.chars().enumerate().all(|(k, c)| self.chars.get(i + k) == Some(&c))
    }

    /// First offset `>= from` where `pat` starts, if any.
    pub(crate) fn find_str(&self, from: usize, pat: &str) -> Option<usize> {
        let first = pat.chars().next()?;
        (from..self.len()).find(|&j| self.at(j) == first && self.starts_with_at(j, pat))
    }

    /// First offset `>= i` whose char fails `pred` (or the end of the text).
    pub(crate) fn scan_while(&self, i: usize, pred: impl Fn(char) -> bool) -> usize {
        let mut j = i;
        while j < self.len() && pred(self.at(j)) {
            j += 1;
        }
        j
    }

    /// First non-whitespace offset of `line`, if the line is not blank.
    pub(crate) fn first_non_ws(&self, line: usize) -> Option<usize> {
        let (start, end) = (self.index.line_start(line), self.index.line_end(line));
        (start..end).find(|&j| !self.at(j).is_whitespace())
    }

    /// End of `start..end` with trailing whitespace removed.
    pub(crate) fn trim_end(&self, start: usize, end: usize) -> usize {
        let mut e = end;
        while e > start && self.at(e - 1).is_whitespace() {
            e -= 1;
        }
        e
    }
}

/// Splits document spans into per-line spans.
pub(crate) fn to_highlights(index: &LineIndex, spans: &[Span]) -> Highlights {
    let mut lines: Vec<Vec<LineSpan>> = vec![Vec::new(); index.len_lines()];
    for span in spans {
        let end = span.end.min(index.total());
        let mut start = span.start;
        if start >= end {
            continue;
        }
        let mut line = index.line_of(start);
        loop {
            let line_start = index.line_start(line);
            let seg_end = end.min(index.line_end(line));
            if seg_end > start
                && let Some(spans) = lines.get_mut(line)
            {
                push_line_span(
                    spans,
                    LineSpan {
                        start: start.saturating_sub(line_start),
                        end: seg_end.saturating_sub(line_start),
                        kind: span.kind,
                    },
                );
            }
            match index.next_line_start(line) {
                Some(next) if next < end => {
                    line += 1;
                    start = next;
                }
                _ => break,
            }
        }
    }
    Highlights::from_lines(lines)
}

pub(crate) fn push_line_span(spans: &mut Vec<LineSpan>, span: LineSpan) {
    let start = spans.last().map_or(span.start, |last| span.start.max(last.end));
    if span.end <= start {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.kind == span.kind
        && last.end == start
    {
        last.end = span.end;
        return;
    }
    spans.push(LineSpan { start, ..span });
}

/// Kind of the span covering char `offset`, if any.
pub(crate) fn kind_at(spans: &[Span], offset: usize) -> Option<HighlightKind> {
    let idx = spans.partition_point(|s| s.start <= offset).checked_sub(1)?;
    spans.get(idx).filter(|s| offset < s.end).map(|s| s.kind)
}

pub(crate) fn is_open(c: char) -> bool {
    matches!(c, '(' | '[' | '{')
}

pub(crate) fn is_close(c: char) -> bool {
    matches!(c, ')' | ']' | '}')
}

/// The partner of a bracket char.
pub(crate) fn partner(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        ')' => Some('('),
        '[' => Some(']'),
        ']' => Some('['),
        '{' => Some('}'),
        '}' => Some('{'),
        _ => None,
    }
}

/// A bracket that takes part in matching (not inside a string or comment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bracket {
    pub(crate) offset: usize,
    pub(crate) ch: char,
}

/// Brackets of `chars` outside string-like and comment spans.
pub(crate) fn code_brackets(chars: &[char], spans: &[Span]) -> Vec<Bracket> {
    let mut out = Vec::new();
    let mut si = 0;
    for (offset, &ch) in chars.iter().enumerate() {
        if !is_open(ch) && !is_close(ch) {
            continue;
        }
        while spans.get(si).is_some_and(|s| s.end <= offset) {
            si += 1;
        }
        let literal = spans.get(si).is_some_and(|s| s.start <= offset && is_literal_kind(s.kind));
        if !literal {
            out.push(Bracket { offset, ch });
        }
    }
    out
}

fn is_literal_kind(kind: HighlightKind) -> bool {
    use HighlightKind as K;
    matches!(kind, K::String | K::Char | K::Comment | K::DocComment | K::Key | K::Invalid)
}

/// The partner of the bracket at `offset`, else of the bracket just before `offset`.
/// `brackets` must be sorted by offset.
pub(crate) fn match_bracket(brackets: &[Bracket], offset: usize) -> Option<usize> {
    [Some(offset), offset.checked_sub(1)]
        .into_iter()
        .flatten()
        .filter_map(|at| brackets.binary_search_by_key(&at, |b| b.offset).ok())
        .find_map(|idx| match_from(brackets, idx))
}

fn match_from(brackets: &[Bracket], idx: usize) -> Option<usize> {
    let first = brackets.get(idx)?;
    let mut stack: Vec<char> = Vec::new();
    if is_open(first.ch) {
        for b in brackets.get(idx..)? {
            if is_open(b.ch) {
                stack.push(b.ch);
                continue;
            }
            let open = stack.pop()?;
            if partner(open) != Some(b.ch) {
                return None;
            }
            if stack.is_empty() {
                return Some(b.offset);
            }
        }
    } else {
        for b in brackets.get(..=idx)?.iter().rev() {
            if is_close(b.ch) {
                stack.push(b.ch);
                continue;
            }
            let close = stack.pop()?;
            if partner(b.ch) != Some(close) {
                return None;
            }
            if stack.is_empty() {
                return Some(b.offset);
            }
        }
    }
    None
}

/// How far down the stack a closing bracket looks for its opener when brackets are mismatched.
const MAX_RECOVERY: usize = 64;

/// Block folds for `(bracket char, line)` pairs in document order. A closer with no opener
/// nearby on the stack is ignored; one that skips openers closes them too.
pub(crate) fn bracket_folds(brackets: impl IntoIterator<Item = (char, usize)>, out: &mut Vec<FoldRange>) {
    let mut stack: Vec<(char, usize)> = Vec::new();
    for (ch, line) in brackets {
        if is_open(ch) {
            stack.push((ch, line));
            continue;
        }
        let back = stack.iter().rev().take(MAX_RECOVERY).position(|&(open, _)| partner(open) == Some(ch));
        if let Some(back) = back {
            let at = stack.len().saturating_sub(back + 1);
            if let Some(&(_, open_line)) = stack.get(at) {
                push_fold(out, open_line, line, FoldKind::Block);
            }
            stack.truncate(at);
        }
    }
}

pub(crate) fn push_fold(out: &mut Vec<FoldRange>, start_line: usize, end_line: usize, kind: FoldKind) {
    if end_line > start_line {
        out.push(FoldRange { start_line, end_line, kind });
    }
}

/// Folds every run of at least two consecutive flagged lines.
pub(crate) fn flag_runs(flags: &[bool], kind: FoldKind, out: &mut Vec<FoldRange>) {
    let mut start: Option<usize> = None;
    for (line, &flag) in flags.iter().enumerate() {
        match (flag, start) {
            (true, None) => start = Some(line),
            (false, Some(s)) => {
                push_fold(out, s, line.saturating_sub(1), kind);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        push_fold(out, s, flags.len().saturating_sub(1), kind);
    }
}

/// For `#`-comment languages: which lines hold nothing but a comment.
pub(crate) fn hash_comment_lines(doc: &Doc, spans: &[Span]) -> Vec<bool> {
    (0..doc.index.len_lines())
        .map(|line| {
            doc.first_non_ws(line)
                .is_some_and(|f| doc.at(f) == '#' && kind_at(spans, f) == Some(HighlightKind::Comment))
        })
        .collect()
}

/// Comment folds for spans of kind `Comment` that cross lines (block comments).
pub(crate) fn multiline_comment_folds(index: &LineIndex, spans: &[Span], out: &mut Vec<FoldRange>) {
    for span in spans.iter().filter(|s| s.kind == HighlightKind::Comment) {
        let start = index.line_of(span.start);
        let end = index.line_of(span.end.saturating_sub(1));
        push_fold(out, start, end, FoldKind::Comment);
    }
}

/// Sorts folds by start line (outermost first), dropping duplicates and single-line ranges.
pub(crate) fn finish_folds(mut folds: Vec<FoldRange>) -> Vec<FoldRange> {
    folds.retain(|f| f.end_line > f.start_line);
    folds.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(b.end_line.cmp(&a.end_line))
            .then(kind_rank(a.kind).cmp(&kind_rank(b.kind)))
    });
    folds.dedup_by(|later, earlier| later.start_line == earlier.start_line && later.end_line == earlier.end_line);
    folds
}

fn kind_rank(kind: FoldKind) -> u8 {
    match kind {
        FoldKind::Region => 0,
        FoldKind::Imports => 1,
        FoldKind::Comment => 2,
        FoldKind::Block => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HighlightKind as K;

    #[test]
    fn line_index_counts_chars_not_bytes() {
        let index = LineIndex::new("é\nab\n");
        assert_eq!(index.len_lines(), 3);
        assert_eq!(index.total(), 5);
        assert_eq!(index.line_start(1), 2);
        assert_eq!(index.line_end(0), 1);
        assert_eq!(index.line_end(2), 5);
        assert_eq!(index.line_of(0), 0);
        assert_eq!(index.line_of(2), 1);
        assert_eq!(index.line_of(99), 2);
        let from_chars = LineIndex::from_chars(&"é\nab\n".chars().collect::<Vec<_>>());
        assert_eq!(from_chars.starts, index.starts);
    }

    #[test]
    fn sink_merges_and_clips() {
        let mut sink = SpanSink::default();
        sink.push(0, 2, K::Keyword);
        sink.push(2, 4, K::Keyword);
        sink.push(3, 6, K::String);
        sink.push(6, 6, K::Number);
        sink.push(6, 9, K::Plain);
        let spans = sink.into_spans();
        assert_eq!(
            spans,
            vec![Span { start: 0, end: 4, kind: K::Keyword }, Span { start: 4, end: 6, kind: K::String }]
        );
    }

    #[test]
    fn sink_truncates() {
        let mut sink = SpanSink::default();
        sink.push(0, 5, K::Comment);
        sink.push(7, 9, K::Comment);
        sink.truncate_from(3);
        assert_eq!(sink.into_spans(), vec![Span { start: 0, end: 3, kind: K::Comment }]);
    }

    #[test]
    fn spans_split_across_lines() {
        let text = "ab\ncd\n\nef";
        let index = LineIndex::new(text);
        let h = to_highlights(&index, &[Span { start: 1, end: 9, kind: K::Comment }]);
        assert_eq!(h.len_lines(), 4);
        assert_eq!(h.line(0), &[LineSpan { start: 1, end: 2, kind: K::Comment }]);
        assert_eq!(h.line(1), &[LineSpan { start: 0, end: 2, kind: K::Comment }]);
        assert!(h.line(2).is_empty());
        assert_eq!(h.line(3), &[LineSpan { start: 0, end: 2, kind: K::Comment }]);
    }

    #[test]
    fn bracket_matching_handles_both_directions_and_mismatch() {
        let bs: Vec<Bracket> = "([]{})".chars().enumerate().map(|(offset, ch)| Bracket { offset, ch }).collect();
        assert_eq!(match_bracket(&bs, 0), Some(5));
        assert_eq!(match_bracket(&bs, 5), Some(0));
        assert_eq!(match_bracket(&bs, 6), Some(0));
        assert_eq!(match_bracket(&bs, 1), Some(2));
        let bad: Vec<Bracket> = "([)]".chars().enumerate().map(|(offset, ch)| Bracket { offset, ch }).collect();
        assert_eq!(match_bracket(&bad, 0), None);
        assert_eq!(match_bracket(&[], 0), None);
    }

    #[test]
    fn bracket_folds_recover_from_mismatch() {
        let mut out = Vec::new();
        bracket_folds([('{', 0), ('(', 1), ('}', 3), (']', 4), ('[', 5), (']', 7)], &mut out);
        let out = finish_folds(out);
        assert_eq!(
            out,
            vec![
                FoldRange { start_line: 0, end_line: 3, kind: FoldKind::Block },
                FoldRange { start_line: 5, end_line: 7, kind: FoldKind::Block },
            ]
        );
    }

    #[test]
    fn flag_runs_need_two_lines() {
        let mut out = Vec::new();
        flag_runs(&[true, false, true, true, true, false, true, true], FoldKind::Comment, &mut out);
        assert_eq!(
            out,
            vec![
                FoldRange { start_line: 2, end_line: 4, kind: FoldKind::Comment },
                FoldRange { start_line: 6, end_line: 7, kind: FoldKind::Comment },
            ]
        );
    }

    #[test]
    fn finish_folds_sorts_and_dedups() {
        let f = |s, e, kind| FoldRange { start_line: s, end_line: e, kind };
        let out = finish_folds(vec![
            f(3, 4, FoldKind::Block),
            f(0, 2, FoldKind::Block),
            f(0, 5, FoldKind::Block),
            f(0, 2, FoldKind::Block),
            f(1, 1, FoldKind::Block),
        ]);
        assert_eq!(out, vec![f(0, 5, FoldKind::Block), f(0, 2, FoldKind::Block), f(3, 4, FoldKind::Block)]);
    }
}
