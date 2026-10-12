//! Markdown (CommonMark subset): ATX and setext headings, emphasis, inline code, fenced code,
//! links, block quotes, list markers, thematic breaks, table pipes and HTML comments.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

pub(crate) struct Heading {
    /// Line of the heading text (for setext headings, the line above the underline).
    pub(crate) line: usize,
    pub(crate) level: usize,
    pub(crate) name: String,
    /// Char column of the name.
    pub(crate) col: usize,
}

pub(crate) struct Analysis {
    pub(crate) spans: Vec<Span>,
    pub(crate) headings: Vec<Heading>,
    /// Fenced code blocks as (opening line, closing line).
    pub(crate) fences: Vec<(usize, usize)>,
    /// HTML comments spanning lines, as (first line, last line).
    pub(crate) comments: Vec<(usize, usize)>,
}

struct Fence {
    ch: char,
    len: usize,
    start_line: usize,
}

/// The previous line, when it was plain paragraph text (a setext heading candidate).
struct Paragraph {
    start: usize,
    end: usize,
    line: usize,
    col: usize,
}

fn at(l: &[char], i: usize) -> char {
    l.get(i).copied().unwrap_or('\0')
}

pub(crate) fn analyze(doc: &Doc) -> Analysis {
    let mut sink = SpanSink::with_capacity(doc.len() / 16);
    let (mut headings, mut fences, mut comments) = (Vec::new(), Vec::new(), Vec::new());
    let mut fence: Option<Fence> = None;
    let mut comment_start: Option<usize> = None;
    let mut paragraph: Option<Paragraph> = None;
    let last_line = doc.index.len_lines().saturating_sub(1);
    for line in 0..doc.index.len_lines() {
        let ls = doc.index.line_start(line);
        let le = doc.index.line_end(line);
        let l = doc.slice(ls, le);
        let prev_paragraph = paragraph.take();
        if let Some(start_line) = comment_start {
            match find_seq(l, 0, &COMMENT_CLOSE) {
                Some(j) => {
                    sink.push(ls, ls + j + 3, K::Comment);
                    comments.push((start_line, line));
                    comment_start = Inline::new(l, ls).run(j + 3, false, &mut sink).then_some(line);
                }
                None => sink.push(ls, le, K::Comment),
            }
            continue;
        }
        if let Some(f) = &fence {
            sink.push(ls, le, K::Code);
            if closes_fence(l, f) {
                fences.push((f.start_line, line));
                fence = None;
            }
            continue;
        }
        let Some(first) = l.iter().position(|c| !c.is_whitespace()) else {
            continue;
        };
        let indent = indent_width(l, first);
        let trim = l.iter().rposition(|c| !c.is_whitespace()).map_or(first, |p| p + 1);
        if indent <= 3 {
            if let Some((ch, len)) = fence_open(l, first) {
                sink.push(ls + first, le, K::Code);
                fence = Some(Fence { ch, len, start_line: line });
                continue;
            }
            if let Some(level) = atx_level(l, first) {
                sink.push(ls + first, ls + trim, K::Heading);
                let (ns, ne) = atx_name(l, first + level, trim);
                if ne > ns {
                    let name = l.get(ns..ne).unwrap_or(&[]).iter().collect();
                    headings.push(Heading { line, level, name, col: ns });
                }
                continue;
            }
            if let Some(p) = &prev_paragraph
                && let Some(level) = setext_level(l, first, trim)
            {
                sink.truncate_from(p.start);
                sink.push(p.start, p.end, K::Heading);
                sink.push(ls + first, ls + trim, K::Heading);
                headings.push(Heading { line: p.line, level, name: doc.string(p.start, p.end), col: p.col });
                continue;
            }
            if is_thematic_break(l, first) {
                sink.push(ls + first, ls + trim, K::Punctuation);
                continue;
            }
            if at(l, first) == '>' {
                sink.push(ls + first, ls + trim, K::Comment);
                continue;
            }
        }
        let mut from = first;
        let mut is_paragraph = indent <= 3;
        if let Some(marker_end) = list_marker(l, first) {
            sink.push(ls + first, ls + marker_end, K::Punctuation);
            from = marker_end;
            while matches!(at(l, from), ' ' | '\t') {
                from += 1;
            }
            if at(l, from) == '['
                && matches!(at(l, from + 1), ' ' | 'x' | 'X')
                && at(l, from + 2) == ']'
                && (from + 3 >= l.len() || at(l, from + 3).is_whitespace())
            {
                sink.push(ls + from, ls + from + 3, K::Punctuation);
                from += 3;
            }
            is_paragraph = false;
        }
        let table = at(l, first) == '|';
        if table {
            is_paragraph = false;
        }
        if Inline::new(l, ls).run(from, table, &mut sink) {
            comment_start = Some(line);
            is_paragraph = false;
        }
        if is_paragraph {
            paragraph = Some(Paragraph { start: ls + first, end: ls + trim, line, col: first });
        }
    }
    if let Some(f) = fence {
        fences.push((f.start_line, last_line));
    }
    if let Some(start) = comment_start {
        comments.push((start, last_line));
    }
    Analysis { spans: sink.into_spans(), headings, fences, comments }
}

/// Columns of leading whitespace up to `first` (tabs advance to the next multiple of 4).
fn indent_width(l: &[char], first: usize) -> usize {
    l.iter().take(first).fold(0, |w, &c| if c == '\t' { w + 4 - w % 4 } else { w + 1 })
}

fn run_len(l: &[char], i: usize, c: char) -> usize {
    l.iter().skip(i).take_while(|&&x| x == c).count()
}

const COMMENT_OPEN: [char; 4] = ['<', '!', '-', '-'];
const COMMENT_CLOSE: [char; 3] = ['-', '-', '>'];

fn starts_at(l: &[char], i: usize, pat: &[char]) -> bool {
    l.get(i..i + pat.len()) == Some(pat)
}

fn find_seq(l: &[char], from: usize, pat: &[char]) -> Option<usize> {
    (from..l.len()).find(|&j| starts_at(l, j, pat))
}

/// An opening code fence at `first`: (fence char, run length).
fn fence_open(l: &[char], first: usize) -> Option<(char, usize)> {
    let ch = at(l, first);
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = run_len(l, first, ch);
    if len < 3 || (ch == '`' && l.iter().skip(first + len).any(|&c| c == '`')) {
        return None;
    }
    Some((ch, len))
}

fn closes_fence(l: &[char], fence: &Fence) -> bool {
    let Some(first) = l.iter().position(|c| !c.is_whitespace()) else {
        return false;
    };
    let len = run_len(l, first, fence.ch);
    indent_width(l, first) <= 3 && len >= fence.len && l.iter().skip(first + len).all(|c| c.is_whitespace())
}

/// `#` … `######` followed by a space or the line end.
fn atx_level(l: &[char], first: usize) -> Option<usize> {
    let level = run_len(l, first, '#');
    ((1..=6).contains(&level) && (first + level >= l.len() || matches!(at(l, first + level), ' ' | '\t')))
        .then_some(level)
}

/// The heading text range: after the `#`s, without a closing `#` sequence.
fn atx_name(l: &[char], after_hashes: usize, trim: usize) -> (usize, usize) {
    let mut start = after_hashes;
    while start < trim && at(l, start).is_whitespace() {
        start += 1;
    }
    let mut end = trim;
    while end > start && at(l, end - 1) == '#' {
        end -= 1;
    }
    if end == start {
        return (start, start);
    }
    if end < trim && !at(l, end - 1).is_whitespace() {
        // `# C#`: the `#` is part of the text.
        return (start, trim);
    }
    while end > start && at(l, end - 1).is_whitespace() {
        end -= 1;
    }
    (start, end)
}

/// `===` (level 1) or `---` (level 2) under a paragraph line.
fn setext_level(l: &[char], first: usize, trim: usize) -> Option<usize> {
    let ch = at(l, first);
    let level = match ch {
        '=' => 1,
        '-' => 2,
        _ => return None,
    };
    l.get(first..trim)?.iter().all(|&c| c == ch).then_some(level)
}

/// `---`, `***` or `___` (three or more, spaces allowed between).
fn is_thematic_break(l: &[char], first: usize) -> bool {
    let ch = at(l, first);
    if !matches!(ch, '-' | '*' | '_') {
        return false;
    }
    let mut count = 0;
    for &c in l.iter().skip(first) {
        if c == ch {
            count += 1;
        } else if !c.is_whitespace() {
            return false;
        }
    }
    count >= 3
}

/// End of a list marker (`-`, `+`, `*`, `1.`, `1)`) at `first`.
fn list_marker(l: &[char], first: usize) -> Option<usize> {
    let followed_by_space = |e: usize| e >= l.len() || matches!(at(l, e), ' ' | '\t');
    if matches!(at(l, first), '-' | '+' | '*') && followed_by_space(first + 1) {
        return Some(first + 1);
    }
    let digits = l.iter().skip(first).take_while(|c| c.is_ascii_digit()).count();
    let end = first + digits + 1;
    ((1..=9).contains(&digits) && matches!(at(l, first + digits), '.' | ')') && followed_by_space(end)).then_some(end)
}

/// Remembers the result of a forward search so that repeated searches from increasing
/// positions stay linear overall. Valid because each predicate depends only on the position.
#[derive(Clone, Copy, Default)]
struct Finder {
    searched_from: usize,
    found: Option<usize>,
    used: bool,
}

impl Finder {
    fn find(&mut self, from: usize, n: usize, pred: impl Fn(usize) -> bool) -> Option<usize> {
        if self.used && self.searched_from <= from {
            match self.found {
                None => return None,
                Some(p) if p >= from => return Some(p),
                Some(_) => {}
            }
        }
        let found = (from..n).find(|&j| pred(j));
        *self = Finder { searched_from: from, found, used: true };
        found
    }
}

const MAX_TICKS: usize = 8;

/// Inline scanning of one line.
struct Inline<'a> {
    l: &'a [char],
    /// Document offset of the line start.
    base: usize,
    ticks: [Finder; MAX_TICKS + 1],
    emphasis: [Finder; 4],
    label_close: Finder,
    ref_close: Finder,
    paren_close: Finder,
}

impl<'a> Inline<'a> {
    fn new(l: &'a [char], base: usize) -> Self {
        Self {
            l,
            base,
            ticks: [Finder::default(); MAX_TICKS + 1],
            emphasis: [Finder::default(); 4],
            label_close: Finder::default(),
            ref_close: Finder::default(),
            paren_close: Finder::default(),
        }
    }

    /// Highlights `from..` of the line; returns whether an HTML comment is left open.
    fn run(mut self, from: usize, table: bool, sink: &mut SpanSink) -> bool {
        let l = self.l;
        let n = l.len();
        let line_first = l.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
        let mut i = from;
        while i < n {
            let c = at(l, i);
            let next = match c {
                '\\' => i + 2,
                '`' => {
                    let k = run_len(l, i, '`');
                    match self.code_end(i, k) {
                        Some(e) => {
                            sink.push(self.base + i, self.base + e, K::Code);
                            e
                        }
                        None => i + k,
                    }
                }
                '*' | '_' => {
                    let k = run_len(l, i, c);
                    match self.emphasis_end(i, k, c) {
                        Some(e) => {
                            sink.push(self.base + i, self.base + e, K::Emphasis);
                            e
                        }
                        None => i + k,
                    }
                }
                '!' | '[' => {
                    let bracket = if c == '!' { i + 1 } else { i };
                    match (at(l, bracket) == '[').then(|| self.link_end(bracket, line_first)).flatten() {
                        Some(e) => {
                            sink.push(self.base + i, self.base + e, K::Link);
                            e
                        }
                        None => i + 1,
                    }
                }
                '<' if starts_at(l, i, &COMMENT_OPEN) => match find_seq(l, i + 4, &COMMENT_CLOSE) {
                    Some(j) => {
                        sink.push(self.base + i, self.base + j + 3, K::Comment);
                        j + 3
                    }
                    None => {
                        sink.push(self.base + i, self.base + n, K::Comment);
                        return true;
                    }
                },
                '<' => match autolink_end(l, i) {
                    Some(e) => {
                        sink.push(self.base + i, self.base + e, K::Link);
                        e
                    }
                    None => i + 1,
                },
                '|' if table => {
                    sink.push(self.base + i, self.base + i + 1, K::Punctuation);
                    i + 1
                }
                _ => i + 1,
            };
            i = next.max(i + 1);
        }
        false
    }

    /// End of a code span opened by `k` backticks at `i`.
    fn code_end(&mut self, i: usize, k: usize) -> Option<usize> {
        let l = self.l;
        let finder = self.ticks.get_mut(k)?;
        let close = finder
            .find(i + k, l.len(), |j| at(l, j) == '`' && run_len(l, j, '`') == k && (j == 0 || at(l, j - 1) != '`'))?;
        Some(close + k)
    }

    /// End of `*x*`, `**x**`, `_x_` or `__x__` opened by a run of `k` `c`s at `i`.
    fn emphasis_end(&mut self, i: usize, k: usize, c: char) -> Option<usize> {
        let l = self.l;
        let after = at(l, i + k);
        if i + k >= l.len() || after.is_whitespace() {
            return None;
        }
        if c == '_' && i > 0 && at(l, i - 1).is_alphanumeric() {
            return None;
        }
        let d = if k >= 2 { 2 } else { 1 };
        let slot = usize::from(c == '_') * 2 + d - 1;
        let finder = self.emphasis.get_mut(slot)?;
        let close = finder.find(i + k, l.len(), |j| {
            let run_ok = (0..d).all(|o| at(l, j + o) == c);
            let isolated = d == 2 || (at(l, j + 1) != c && at(l, j.wrapping_sub(1)) != c);
            let flank_ok = j > 0 && !at(l, j - 1).is_whitespace();
            let word_ok = c == '*' || !at(l, j + d).is_alphanumeric();
            run_ok && isolated && flank_ok && word_ok
        })?;
        Some(close + d)
    }

    /// End of `[text](url)`, `[text][ref]` or a `[ref]: url` definition starting at `i`.
    fn link_end(&mut self, i: usize, line_first: usize) -> Option<usize> {
        let l = self.l;
        let n = l.len();
        let close = self.label_close.find(i + 1, n, |j| at(l, j) == ']')?;
        match at(l, close + 1) {
            '(' => self.paren_close.find(close + 2, n, |j| at(l, j) == ')').map(|j| j + 1),
            '[' => self.ref_close.find(close + 2, n, |j| at(l, j) == ']').map(|j| j + 1),
            ':' if i == line_first => Some(l.iter().rposition(|c| !c.is_whitespace()).map_or(n, |p| p + 1)),
            _ => None,
        }
    }
}

/// `<scheme:…>` or `<user@host>`.
fn autolink_end(l: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < l.len() {
        match at(l, j) {
            '>' => {
                let inner = l.get(i + 1..j)?;
                return (!inner.is_empty() && inner.iter().any(|&c| c == ':' || c == '@')).then_some(j + 1);
            }
            ' ' | '\t' | '<' => return None,
            _ => j += 1,
        }
    }
    None
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let analysis = analyze(doc);
    let lines = doc.index.len_lines();
    let mut out = Vec::new();
    // Last non-blank line at or before each line.
    let mut last_non_blank = Vec::with_capacity(lines);
    let mut current = None;
    for line in 0..lines {
        if doc.first_non_ws(line).is_some() {
            current = Some(line);
        }
        last_non_blank.push(current);
    }
    let end_before = |limit: usize| limit.checked_sub(1).and_then(|l| last_non_blank.get(l).copied().flatten());
    // A section runs until the next heading of the same or a higher level.
    let mut open: Vec<(usize, usize)> = Vec::new();
    for h in &analysis.headings {
        while let Some(&(level, start)) = open.last()
            && level >= h.level
        {
            open.pop();
            if let Some(end) = end_before(h.line) {
                common::push_fold(&mut out, start, end, FoldKind::Block);
            }
        }
        open.push((h.level, h.line));
    }
    for (_, start) in open {
        if let Some(end) = end_before(lines) {
            common::push_fold(&mut out, start, end, FoldKind::Block);
        }
    }
    for &(start, end) in &analysis.fences {
        common::push_fold(&mut out, start, end, FoldKind::Block);
    }
    for &(start, end) in &analysis.comments {
        common::push_fold(&mut out, start, end, FoldKind::Comment);
    }
    common::finish_folds(out)
}

pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    analyze(doc)
        .headings
        .into_iter()
        .map(|h| OutlineItem {
            name: h.name,
            kind: SymbolKind::Section,
            line: h.line,
            column: h.col,
            depth: h.level.saturating_sub(1),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_at, kind_of, offset_of, spans};
    use crate::{
        FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
        outline,
    };

    const M: Language = Language::Markdown;

    #[test]
    fn atx_headings() {
        assert_kind(M, "# Title", "# Title", K::Heading);
        assert_kind(M, "###### Six ##", "###### Six ##", K::Heading);
        assert_eq!(kind_at(M, "####### seven", 0, 0), K::Plain);
        assert_eq!(kind_at(M, "#hashtag", 0, 0), K::Plain);
        assert_kind(M, "  ## Indented", "## Indented", K::Heading);
        assert_eq!(kind_at(M, "    # code", 0, 4), K::Plain);
    }

    #[test]
    fn setext_headings() {
        let text = "Title\n=====\n\nSub\n---\n\n- item\n---";
        assert_kind(M, text, "Title", K::Heading);
        assert_kind(M, text, "=====", K::Heading);
        assert_kind(M, text, "Sub", K::Heading);
        let all = spans(M, text);
        // After a list item, `---` is a thematic break.
        assert_eq!(all.last(), Some(&("---".to_string(), K::Punctuation)));
        let items = outline(M, text);
        assert_eq!(items.iter().map(|i| (i.name.as_str(), i.depth)).collect::<Vec<_>>(), [("Title", 0), ("Sub", 1)]);
    }

    #[test]
    fn emphasis_forms() {
        assert_kind(M, "a *em* b", "*em*", K::Emphasis);
        assert_kind(M, "a **strong** b", "**strong**", K::Emphasis);
        assert_kind(M, "a _em_ b", "_em_", K::Emphasis);
        assert_kind(M, "a __strong__ b", "__strong__", K::Emphasis);
        assert_kind(M, "*a **b** c*", "*a **b** c*", K::Emphasis);
        // Not emphasis: snake_case words, spaced asterisks, unclosed.
        assert!(spans(M, "snake_case_name").is_empty());
        assert!(spans(M, "2 * 3 * 4").is_empty());
        assert!(spans(M, "*open").is_empty());
    }

    #[test]
    fn inline_code_and_fences() {
        assert_kind(M, "use `foo()` here", "`foo()`", K::Code);
        assert_kind(M, "``a ` b``", "``a ` b``", K::Code);
        assert!(spans(M, "`unclosed").is_empty());
        let text = "```rust\nfn main() { *x* }\n```\n# After";
        assert_kind(M, text, "```rust", K::Code);
        assert_kind(M, text, "fn main() { *x* }", K::Code);
        assert_kind(M, text, "```", K::Code);
        assert_kind(M, text, "# After", K::Heading);
        let tilde = "~~~~\n# not heading\n~~~\nstill code\n~~~~\n";
        assert_eq!(kind_of(M, tilde, "# not heading"), Some(K::Code));
        assert_eq!(kind_of(M, tilde, "still code"), Some(K::Code));
    }

    #[test]
    fn links_and_autolinks() {
        assert_kind(M, "see [docs](https://x.y/a_b) now", "[docs](https://x.y/a_b)", K::Link);
        assert_kind(M, "![logo](img.png)", "![logo](img.png)", K::Link);
        assert_kind(M, "[text][ref]", "[text][ref]", K::Link);
        assert_kind(M, "[ref]: https://example.com", "[ref]: https://example.com", K::Link);
        assert_kind(M, "<https://example.com>", "<https://example.com>", K::Link);
        assert!(spans(M, "a [bracket] b").is_empty());
        assert!(spans(M, "a < b > c").is_empty());
    }

    #[test]
    fn quotes_lists_breaks_tables() {
        assert_kind(M, "> quoted *text*", "> quoted *text*", K::Comment);
        assert_kind(M, "- item *em*", "-", K::Punctuation);
        assert_kind(M, "- item *em*", "*em*", K::Emphasis);
        assert_kind(M, "12. twelve", "12.", K::Punctuation);
        assert_kind(M, "3) three", "3)", K::Punctuation);
        assert_kind(M, "- [x] done", "[x]", K::Punctuation);
        assert_kind(M, "***", "***", K::Punctuation);
        assert_kind(M, "* * *", "* * *", K::Punctuation);
        assert_kind(M, "| a | b |", "|", K::Punctuation);
    }

    #[test]
    fn html_comments() {
        let text = "a <!-- x --> b\n<!-- open\nstill\n--> *after*";
        assert_kind(M, text, "<!-- x -->", K::Comment);
        assert_kind(M, text, "<!-- open", K::Comment);
        assert_kind(M, text, "still", K::Comment);
        assert_kind(M, text, "-->", K::Comment);
        assert_kind(M, text, "*after*", K::Emphasis);
    }

    #[test]
    fn escapes() {
        assert!(spans(M, r"\*not em\*").is_empty());
    }

    #[test]
    fn non_ascii_columns() {
        let h = crate::test_util::checked(M, "ü *ä* `ö`");
        let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(cols, [(2, 5, K::Emphasis), (6, 9, K::Code)]);
    }

    #[test]
    fn outline_headings() {
        let text = "# A\ntext\n## B\n### C #\n# D\n```\n# no\n```";
        assert_eq!(
            outline(M, text),
            vec![
                OutlineItem { name: "A".into(), kind: SymbolKind::Section, line: 0, column: 2, depth: 0 },
                OutlineItem { name: "B".into(), kind: SymbolKind::Section, line: 2, column: 3, depth: 1 },
                OutlineItem { name: "C".into(), kind: SymbolKind::Section, line: 3, column: 4, depth: 2 },
                OutlineItem { name: "D".into(), kind: SymbolKind::Section, line: 4, column: 2, depth: 0 },
            ]
        );
        assert_eq!(outline(M, "# C#")[0].name, "C#");
        assert!(outline(M, "#").is_empty());
    }

    #[test]
    fn folds_sections_fences_comments() {
        let text = "# A\nintro\n\n## B\nb text\n\n# C\n```\ncode\n```\n<!--\nx\n-->\n";
        assert_eq!(
            folding_ranges(M, text),
            vec![
                FoldRange { start_line: 0, end_line: 4, kind: FoldKind::Block },
                FoldRange { start_line: 3, end_line: 4, kind: FoldKind::Block },
                FoldRange { start_line: 6, end_line: 12, kind: FoldKind::Block },
                FoldRange { start_line: 7, end_line: 9, kind: FoldKind::Block },
                FoldRange { start_line: 10, end_line: 12, kind: FoldKind::Comment },
            ]
        );
    }

    #[test]
    fn brackets_match_in_prose() {
        let text = "a (b [c] d) `(`";
        assert_eq!(matching_bracket(M, text, 2), Some(offset_of(text, ")", 0)));
        assert_eq!(matching_bracket(M, text, offset_of(text, "[", 0)), Some(offset_of(text, "]", 0)));
    }
}
