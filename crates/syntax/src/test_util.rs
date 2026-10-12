//! Helpers shared by the per-language test modules.

use crate::{HighlightKind, Highlights, Language, highlight};

/// Highlights `text` and checks the per-line invariants: one entry per line, spans sorted,
/// non-overlapping, non-empty, never `Plain`, within the line, adjacent same-kind spans merged.
pub(crate) fn checked(language: Language, text: &str) -> Highlights {
    let h = highlight(language, text);
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(h.len_lines(), lines.len(), "line count for {language:?}");
    for (i, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        let mut prev: Option<crate::LineSpan> = None;
        for s in h.line(i) {
            assert!(s.start < s.end, "empty span {s:?} on line {i}");
            assert!(s.end <= len, "span {s:?} past end of line {i} (len {len})");
            assert_ne!(s.kind, HighlightKind::Plain, "plain span on line {i}");
            if let Some(p) = prev {
                assert!(p.end <= s.start, "overlap {p:?} {s:?} on line {i}");
                assert!(!(p.end == s.start && p.kind == s.kind), "unmerged {p:?} {s:?} on line {i}");
            }
            prev = Some(*s);
        }
    }
    h
}

/// `(span text, kind)` for every span, line by line.
pub(crate) fn spans(language: Language, text: &str) -> Vec<(String, HighlightKind)> {
    let h = checked(language, text);
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        for s in h.line(i) {
            out.push((chars[s.start..s.end].iter().collect(), s.kind));
        }
    }
    out
}

/// Kind of the first span whose text is exactly `needle`.
pub(crate) fn kind_of(language: Language, text: &str, needle: &str) -> Option<HighlightKind> {
    spans(language, text).into_iter().find(|(t, _)| t == needle).map(|(_, k)| k)
}

/// Kind at (line, char column), `Plain` when no span covers it.
pub(crate) fn kind_at(language: Language, text: &str, line: usize, col: usize) -> HighlightKind {
    checked(language, text)
        .line(line)
        .iter()
        .find(|s| s.start <= col && col < s.end)
        .map_or(HighlightKind::Plain, |s| s.kind)
}

/// Asserts that `needle` is highlighted as one span of `kind`.
#[track_caller]
pub(crate) fn assert_kind(language: Language, text: &str, needle: &str, kind: HighlightKind) {
    let all = spans(language, text);
    assert!(
        all.iter().any(|(t, k)| t == needle && *k == kind),
        "expected {needle:?} as {kind:?} in {text:?}; spans: {all:?}"
    );
}

/// Char offset of the `nth` (0-based) occurrence of `needle` in `text`.
pub(crate) fn offset_of(text: &str, needle: &str, nth: usize) -> usize {
    let (byte, _) = text.match_indices(needle).nth(nth).unwrap();
    text.get(..byte).unwrap().chars().count()
}
