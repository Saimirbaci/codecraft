//! Shared token model for the hand-written lexers of C/C++, Python, Zig, TypeScript/JavaScript
//! and SQL: a flat token list in char offsets, significant-token navigation for the
//! context-sensitive word classification, and the bracket, fold and outline helpers built on it.
//!
//! Brackets come from `Sym` tokens only, so brackets inside strings, comments, regex literals
//! and JSX text never take part in matching or folding.

use crate::common::{self, Bracket, Doc, LineIndex, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

/// Token class; the highlight kind lives in [`Tok::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tt {
    /// Identifier or keyword; classified after lexing.
    Word,
    /// One punctuation, operator or bracket char.
    Sym(char),
    /// Number, string, char or regex literal (or one segment of an interpolated string).
    Lit,
    LineComment,
    BlockComment,
    /// Any other significant token with a fixed kind (directive, decorator, builtin, tag name…).
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tok {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) tt: Tt,
    pub(crate) kind: K,
    pub(crate) flags: u8,
}

/// The token starts a statement (Python).
pub(crate) const STMT: u8 = 1;
/// The token starts a logical line (Python).
pub(crate) const LINE: u8 = 2;
/// The token belongs to a preprocessor directive (C/C++).
pub(crate) const DIRECTIVE: u8 = 4;

impl Tok {
    pub(crate) fn is_comment(&self) -> bool {
        matches!(self.tt, Tt::LineComment | Tt::BlockComment)
    }
}

/// Default kind of a symbol char.
pub(crate) fn sym_kind(c: char) -> K {
    match c {
        '(' | ')' | '[' | ']' | '{' | '}' => K::Bracket,
        ',' | ';' | '.' | ':' => K::Punctuation,
        _ => K::Operator,
    }
}

/// ASCII projection of the text, one byte per char (non-ASCII chars become `0x80`), so word
/// tests compare byte slices without allocating.
pub(crate) fn ascii(doc: &Doc, lowercase: bool) -> Vec<u8> {
    doc.chars
        .iter()
        .map(|&c| match u8::try_from(c) {
            Ok(b) if b.is_ascii() => {
                if lowercase {
                    b.to_ascii_lowercase()
                } else {
                    b
                }
            }
            _ => 0x80,
        })
        .collect()
}

/// Collects tokens while lexing and remembers the last few significant ones.
#[derive(Debug, Default)]
pub(crate) struct TokSink {
    pub(crate) toks: Vec<Tok>,
    /// Indices of the last three significant tokens, most recent first.
    recent: [Option<usize>; 3],
}

impl TokSink {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self { toks: Vec::with_capacity(capacity), recent: [None; 3] }
    }

    pub(crate) fn push(&mut self, start: usize, end: usize, tt: Tt, kind: K) {
        self.push_flags(start, end, tt, kind, 0);
    }

    pub(crate) fn push_flags(&mut self, start: usize, end: usize, tt: Tt, kind: K, flags: u8) {
        if end <= start || self.toks.last().is_some_and(|t| start < t.end) {
            return;
        }
        let tok = Tok { start, end, tt, kind, flags };
        if !tok.is_comment() {
            self.recent = [Some(self.toks.len()), self.recent[0], self.recent[1]];
        }
        self.toks.push(tok);
    }

    pub(crate) fn sym(&mut self, i: usize, c: char) {
        self.push(i, i + 1, Tt::Sym(c), sym_kind(c));
    }

    /// The `k`-th most recent significant token (0 = the last one), `k < 3`.
    pub(crate) fn recent(&self, k: usize) -> Option<&Tok> {
        self.recent.get(k).copied().flatten().and_then(|i| self.toks.get(i))
    }

    pub(crate) fn recent_sym(&self, k: usize) -> Option<char> {
        match self.recent(k)?.tt {
            Tt::Sym(c) => Some(c),
            _ => None,
        }
    }

    /// Tokens `k` and `k + 1` back touch (no whitespace between).
    pub(crate) fn recent_adjacent(&self, k: usize) -> bool {
        matches!((self.recent(k + 1), self.recent(k)), (Some(a), Some(b)) if a.end == b.start)
    }
}

pub(crate) fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

pub(crate) fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// End of a numeric literal starting at `i` (a digit, or a `.` before a digit): digits, letters
/// (radix prefixes, exponents, suffixes), `_`, one `.` that is not part of `..`, a sign after an
/// exponent marker (`e` in decimal, `p` in hex), and `'` between digits when `quote_sep` (C++14).
pub(crate) fn number_end(doc: &Doc, i: usize, quote_sep: bool) -> usize {
    let n = doc.len();
    let hex = doc.at(i) == '0' && matches!(doc.at(i + 1), 'x' | 'X');
    let mut j = i;
    let mut dot = false;
    while j < n {
        let c = doc.at(j);
        if c.is_ascii_alphanumeric() || c == '_' {
            let exponent = if hex { matches!(c, 'p' | 'P') } else { matches!(c, 'e' | 'E') };
            if exponent && matches!(doc.at(j + 1), '+' | '-') && doc.at(j + 2).is_ascii_digit() {
                j += 3;
                dot = true;
            } else {
                j += 1;
            }
        } else if c == '.' && !dot && doc.at(j + 1) != '.' {
            let next = doc.at(j + 1);
            let exponent_next = matches!(next, 'e' | 'E') && (doc.at(j + 2).is_ascii_digit());
            if next.is_ascii_digit() || exponent_next || !(is_ident_start(next) || next == '$') {
                dot = true;
                j += 1;
            } else {
                break;
            }
        } else if c == '\'' && quote_sep && j > i && doc.at(j + 1).is_ascii_hexdigit() {
            j += 1;
        } else {
            break;
        }
    }
    j.max(i + 1)
}

/// A `.` at `i` starts a number like `.5` (not a member access like `x.0` or a range `..5`).
pub(crate) fn dot_number(doc: &Doc, i: usize) -> bool {
    doc.at(i) == '.'
        && doc.at(i + 1).is_ascii_digit()
        && i.checked_sub(1).is_none_or(|k| {
            let p = doc.at(k);
            !(is_ident_char(p) || matches!(p, '.' | ')' | ']' | '}' | '$'))
        })
}

/// Scans a quoted literal whose body starts at `body` and ends at the next unescaped `quote`.
/// With `escapes`, a backslash skips the next char (a newline after it continues the literal).
/// Without `multiline`, an unescaped newline ends it unterminated, before the newline.
/// Returns (end past the closing quote or where scanning stopped, terminated).
pub(crate) fn scan_quoted(doc: &Doc, body: usize, quote: char, escapes: bool, multiline: bool) -> (usize, bool) {
    let n = doc.len();
    let mut j = body;
    while j < n {
        let c = doc.at(j);
        if c == '\\' && escapes {
            j += if doc.at(j + 1) == '\r' && doc.at(j + 2) == '\n' { 3 } else { 2 };
            continue;
        }
        if c == quote {
            return (j + 1, true);
        }
        if c == '\n' && !multiline {
            return (j, false);
        }
        j += 1;
    }
    (n, false)
}

/// End of the block comment opening at `i` (`/*`), past its `*/`, and whether it was closed.
pub(crate) fn block_comment_end(doc: &Doc, i: usize, nested: bool) -> (usize, bool) {
    let n = doc.len();
    let mut depth = 0usize;
    let mut j = i;
    while j < n {
        if doc.at(j) == '/' && doc.at(j + 1) == '*' && (nested || depth == 0) {
            depth += 1;
            j += 2;
        } else if doc.at(j) == '*' && doc.at(j + 1) == '/' {
            depth = depth.saturating_sub(1);
            j += 2;
            if depth == 0 {
                return (j, true);
            }
        } else {
            j += 1;
        }
    }
    (n, false)
}

/// SCREAMING_CASE: at least two chars, upper-case letters, digits and underscores only.
pub(crate) fn is_screaming(chars: &[char]) -> bool {
    chars.len() >= 2
        && chars.iter().any(|c| c.is_uppercase())
        && chars.iter().all(|&c| c.is_uppercase() || c.is_ascii_digit() || c == '_')
}

pub(crate) fn is_capitalized(chars: &[char]) -> bool {
    chars.first().is_some_and(|c| c.is_uppercase())
}

/// Line lookups for offsets queried in increasing order (amortized O(1)).
pub(crate) struct LineCursor<'a> {
    index: &'a LineIndex,
    line: usize,
}

impl<'a> LineCursor<'a> {
    pub(crate) fn new(index: &'a LineIndex) -> Self {
        Self { index, line: 0 }
    }

    pub(crate) fn line_of(&mut self, offset: usize) -> usize {
        if offset < self.index.line_start(self.line) {
            self.line = self.index.line_of(offset);
            return self.line;
        }
        while let Some(next) = self.index.next_line_start(self.line)
            && next <= offset
        {
            self.line += 1;
        }
        self.line
    }
}

/// A lexed document: tokens plus the positions of the significant ones. Positions `p` used by
/// the accessors index the significant tokens; out-of-range positions read as nothing.
pub(crate) struct Lexed<'a> {
    pub(crate) doc: &'a Doc,
    pub(crate) ascii: Vec<u8>,
    pub(crate) toks: Vec<Tok>,
    pub(crate) sig: Vec<usize>,
}

impl<'a> Lexed<'a> {
    pub(crate) fn new(doc: &'a Doc, ascii: Vec<u8>, toks: Vec<Tok>) -> Self {
        let mut lexed = Self { doc, ascii, toks, sig: Vec::new() };
        lexed.refilter(|t| !t.is_comment());
        lexed
    }

    /// Recomputes the significant positions as the tokens satisfying `keep`.
    pub(crate) fn refilter(&mut self, keep: impl Fn(&Tok) -> bool) {
        self.sig = self.toks.iter().enumerate().filter(|(_, t)| keep(t)).map(|(i, _)| i).collect();
    }

    pub(crate) fn len(&self) -> usize {
        self.sig.len()
    }

    pub(crate) fn tok(&self, p: usize) -> Option<&Tok> {
        self.sig.get(p).and_then(|&i| self.toks.get(i))
    }

    pub(crate) fn tt(&self, p: usize) -> Option<Tt> {
        self.tok(p).map(|t| t.tt)
    }

    pub(crate) fn sym(&self, p: usize) -> Option<char> {
        match self.tt(p)? {
            Tt::Sym(c) => Some(c),
            _ => None,
        }
    }

    pub(crate) fn is_sym(&self, p: usize, c: char) -> bool {
        self.sym(p) == Some(c)
    }

    /// `p` and `p + 1` are the symbols `a` and `b`, touching (`->`, `::`, `=>`).
    pub(crate) fn is_pair(&self, p: usize, a: char, b: char) -> bool {
        self.is_sym(p, a) && self.is_sym(p + 1, b) && self.adjacent(p)
    }

    pub(crate) fn is_word_tok(&self, p: usize) -> bool {
        self.tt(p) == Some(Tt::Word)
    }

    /// ASCII bytes of the word at `p` (lower-cased for SQL); empty when `p` is not a word.
    pub(crate) fn word(&self, p: usize) -> &[u8] {
        match self.tok(p) {
            Some(t) if t.tt == Tt::Word => self.ascii.get(t.start..t.end).unwrap_or(&[]),
            _ => &[],
        }
    }

    pub(crate) fn is_word(&self, p: usize, w: &str) -> bool {
        self.word(p) == w.as_bytes()
    }

    pub(crate) fn chars(&self, p: usize) -> &'a [char] {
        let doc: &'a Doc = self.doc;
        self.tok(p).map_or(&[], |t| doc.slice(t.start, t.end))
    }

    pub(crate) fn text(&self, p: usize) -> String {
        self.chars(p).iter().collect()
    }

    pub(crate) fn has_flag(&self, p: usize, flag: u8) -> bool {
        self.tok(p).is_some_and(|t| t.flags & flag != 0)
    }

    /// `p` and `p + 1` touch (no whitespace or comment between).
    pub(crate) fn adjacent(&self, p: usize) -> bool {
        matches!((self.tok(p), self.tok(p + 1)), (Some(a), Some(b)) if a.end == b.start)
    }

    pub(crate) fn set(&mut self, p: usize, kind: K) {
        if let Some(t) = self.sig.get(p).and_then(|&i| self.toks.get_mut(i)) {
            t.kind = kind;
        }
    }

    pub(crate) fn kind(&self, p: usize) -> Option<K> {
        self.tok(p).map(|t| t.kind)
    }

    pub(crate) fn line(&self, p: usize) -> usize {
        self.tok(p).map_or(0, |t| self.doc.index.line_of(t.start))
    }

    pub(crate) fn end_line(&self, p: usize) -> usize {
        self.tok(p).map_or(0, |t| self.doc.index.line_of(t.end.saturating_sub(1).max(t.start)))
    }

    pub(crate) fn col(&self, p: usize) -> usize {
        self.tok(p).map_or(0, |t| t.start.saturating_sub(self.doc.index.line_start(self.doc.index.line_of(t.start))))
    }

    /// A newline separates `p - 1` and `p`.
    pub(crate) fn newline_before(&self, p: usize) -> bool {
        match (p.checked_sub(1).and_then(|q| self.tok(q)), self.tok(p)) {
            (Some(a), Some(b)) => self.doc.slice(a.end, b.start).contains(&'\n'),
            _ => true,
        }
    }

    /// For each opening bracket, the position of the closer that balances it (counting all
    /// bracket kinds alike), if any.
    pub(crate) fn group_ends(&self) -> Vec<Option<usize>> {
        let mut ends = vec![None; self.len()];
        let mut stack = Vec::new();
        for p in 0..self.len() {
            match self.sym(p) {
                Some('(' | '[' | '{') => stack.push(p),
                Some(')' | ']' | '}') => {
                    if let Some(open) = stack.pop()
                        && let Some(slot) = ends.get_mut(open)
                    {
                        *slot = Some(p);
                    }
                }
                _ => {}
            }
        }
        ends
    }

    pub(crate) fn spans(&self) -> Vec<Span> {
        let mut sink = SpanSink::with_capacity(self.toks.len());
        for t in &self.toks {
            sink.push(t.start, t.end, t.kind);
        }
        sink.into_spans()
    }

    /// Bracket tokens of the whole text (ignores the significant filter).
    pub(crate) fn brackets(&self) -> Vec<Bracket> {
        self.toks
            .iter()
            .filter_map(|t| match t.tt {
                Tt::Sym(ch) if common::is_open(ch) || common::is_close(ch) => Some(Bracket { offset: t.start, ch }),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn bracket_folds(&self, out: &mut Vec<FoldRange>) {
        let mut lines = LineCursor::new(&self.doc.index);
        let brackets = self.brackets();
        common::bracket_folds(brackets.iter().map(|b| (b.ch, lines.line_of(b.offset))), out);
    }

    /// Runs of whole-line line comments of one kind (comment or doc comment) on consecutive
    /// lines, and multi-line block comments.
    pub(crate) fn comment_folds(&self, out: &mut Vec<FoldRange>) {
        let mut lines = LineCursor::new(&self.doc.index);
        // (kind, first line, last line)
        let mut run: Option<(K, usize, usize)> = None;
        let mut last_code_line: Option<usize> = None;
        let flush = |run: &mut Option<(K, usize, usize)>, out: &mut Vec<FoldRange>| {
            if let Some((_, start, end)) = run.take() {
                common::push_fold(out, start, end, FoldKind::Comment);
            }
        };
        for t in &self.toks {
            let line = lines.line_of(t.start);
            let end_line = lines.line_of(t.end.saturating_sub(1).max(t.start));
            match t.tt {
                Tt::LineComment => {
                    let alone = last_code_line.is_none_or(|l| l < line);
                    match run {
                        Some((kind, start, end)) if alone && kind == t.kind && end + 1 == line => {
                            run = Some((kind, start, line));
                        }
                        _ => {
                            flush(&mut run, out);
                            if alone {
                                run = Some((t.kind, line, line));
                            }
                        }
                    }
                }
                Tt::BlockComment => {
                    flush(&mut run, out);
                    common::push_fold(out, line, end_line, FoldKind::Comment);
                    last_code_line = Some(end_line);
                }
                _ => {
                    flush(&mut run, out);
                    last_code_line = Some(end_line);
                }
            }
        }
        flush(&mut run, out);
    }

    pub(crate) fn item(&self, p: usize, name: String, kind: SymbolKind, depth: usize) -> Option<OutlineItem> {
        self.tok(p)?;
        Some(OutlineItem { name, kind, line: self.line(p), column: self.col(p), depth })
    }

    /// `q` starts call arguments: `(`, or (with `generics`) `<…>(` right after the word at `q - 1`.
    pub(crate) fn is_call(&self, q: usize, generics: bool) -> bool {
        const MAX_GENERIC: usize = 48;
        if self.is_sym(q, '(') {
            return true;
        }
        if !(generics && self.is_sym(q, '<') && q.checked_sub(1).is_some_and(|w| self.adjacent(w))) {
            return false;
        }
        let mut depth = 0usize;
        for r in q..self.len().min(q + MAX_GENERIC) {
            match self.tt(r) {
                Some(Tt::Sym('<')) => depth += 1,
                Some(Tt::Sym('>')) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self.is_sym(r + 1, '(');
                    }
                }
                Some(Tt::Sym(',' | '.' | ':' | '*' | '&' | '[' | ']' | '?' | '|') | Tt::Word | Tt::Lit) => {}
                _ => return false,
            }
        }
        false
    }

    /// The word at `p` follows a member access: `.` (not `..`), or `->` when `arrow`.
    pub(crate) fn after_member_access(&self, p: usize, arrow: bool) -> bool {
        let Some(q) = p.checked_sub(1) else {
            return false;
        };
        if self.is_sym(q, '.') {
            return !q.checked_sub(1).is_some_and(|r| self.is_sym(r, '.') && self.adjacent(r));
        }
        arrow && q.checked_sub(1).is_some_and(|r| self.is_pair(r, '-', '>'))
    }
}

/// Folds runs of at least two consecutive statements flagged `true`, from the first start line
/// to the last end line. `stmts` are (flag, start line, end line) in document order.
pub(crate) fn statement_runs(stmts: &[(bool, usize, usize)], kind: FoldKind, out: &mut Vec<FoldRange>) {
    let mut run: Option<(usize, usize, usize)> = None; // (start, end, count)
    let flush = |run: Option<(usize, usize, usize)>, out: &mut Vec<FoldRange>| {
        if let Some((start, end, count)) = run
            && count >= 2
        {
            common::push_fold(out, start, end, kind);
        }
    };
    for &(flag, start, end) in stmts {
        match (flag, run) {
            (true, Some((s, _, count))) => run = Some((s, end, count + 1)),
            (true, None) => run = Some((start, end, 1)),
            (false, r) => {
                flush(r, out);
                run = None;
            }
        }
    }
    flush(run, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        let cases = [
            ("0x1F;", 4),
            ("1.5e-3)", 6),
            ("0x1p+3 ", 6),
            ("0x1e+5", 4),
            ("1..5", 1),
            ("1_000u32 ", 8),
            ("3.f", 1),
            ("3. ", 2),
            ("1.e5", 4),
            ("1'000'000;", 9),
            ("12'", 2),
            ("7", 1),
        ];
        for (text, len) in cases {
            assert_eq!(number_end(&Doc::new(text), 0, true), len, "{text}");
        }
        let doc = Doc::new("1'000");
        assert_eq!(number_end(&doc, 0, false), 1);
        assert!(dot_number(&Doc::new(".5"), 0));
        assert!(!dot_number(&Doc::new("x.5"), 1));
        assert!(!dot_number(&Doc::new("..5"), 1));
    }

    #[test]
    fn quoted_and_comments() {
        let doc = Doc::new("\"a\\\"b\" x");
        assert_eq!(scan_quoted(&doc, 1, '"', true, false), (6, true));
        let doc = Doc::new("'ab\ncd'");
        assert_eq!(scan_quoted(&doc, 1, '\'', true, false), (3, false));
        assert_eq!(scan_quoted(&doc, 1, '\'', true, true), (7, true));
        let doc = Doc::new("'a\\\r\nb'");
        assert_eq!(scan_quoted(&doc, 1, '\'', true, false), (7, true));
        let doc = Doc::new("/* /* */ */x");
        assert_eq!(block_comment_end(&doc, 0, true), (11, true));
        assert_eq!(block_comment_end(&doc, 0, false), (8, true));
        assert_eq!(block_comment_end(&Doc::new("/* x"), 0, false), (4, false));
    }

    #[test]
    fn helpers() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert!(is_screaming(&c("MAX_2")));
        assert!(!is_screaming(&c("A")));
        assert!(!is_screaming(&c("Max")));
        assert!(is_capitalized(&c("Ünïcode")));
        let doc = Doc::new("é\nab\n\nc");
        assert_eq!(ascii(&doc, false), vec![0x80, b'\n', b'a', b'b', b'\n', b'\n', b'c']);
        let mut lines = LineCursor::new(&doc.index);
        assert_eq!(lines.line_of(0), 0);
        assert_eq!(lines.line_of(3), 1);
        assert_eq!(lines.line_of(6), 3);
        assert_eq!(lines.line_of(1), 0);
        assert_eq!(lines.line_of(99), 3);
    }

    #[test]
    fn statement_runs_need_two() {
        let mut out = Vec::new();
        statement_runs(
            &[(true, 0, 0), (true, 1, 2), (false, 3, 3), (true, 4, 4), (true, 6, 6)],
            FoldKind::Imports,
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                FoldRange { start_line: 0, end_line: 2, kind: FoldKind::Imports },
                FoldRange { start_line: 4, end_line: 6, kind: FoldKind::Imports },
            ]
        );
    }
}
