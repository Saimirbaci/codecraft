//! Rust: tokens from `rustc_lexer`, classified with lexical heuristics; brackets, folds and the
//! outline come from the same token stream, so strings and comments never confuse them.

use ra_ap_rustc_lexer::{self as lexer, FrontmatterAllowed, LiteralKind, TokenKind};

use crate::common::{self, Bracket, LineIndex, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind, Highlights, OutlineItem, SymbolKind};

/// One lexer token with byte and char positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tok {
    pub(crate) kind: TokenKind,
    /// Byte range in the text.
    pub(crate) start: usize,
    pub(crate) end: usize,
    /// Char range in the text.
    pub(crate) cstart: usize,
    pub(crate) cend: usize,
    /// Line and char column of the first char.
    pub(crate) line: usize,
    pub(crate) col: usize,
    /// Line of the last char.
    pub(crate) end_line: usize,
}

pub(crate) struct Cursor {
    pub(crate) byte: usize,
    pub(crate) ch: usize,
    pub(crate) line: usize,
    /// Char offset of the start of the current line.
    pub(crate) line_start: usize,
}

/// Tokenizes the whole text; the tokens cover it exactly. A leading shebang line becomes a
/// line comment.
pub(crate) fn lex(text: &str) -> Vec<Tok> {
    let mut toks = Vec::with_capacity(text.len() / 3 + 1);
    let mut cur = Cursor { byte: 0, ch: 0, line: 0, line_start: 0 };
    if let Some(n) = lexer::strip_shebang(text) {
        push_tok(&mut toks, text, &mut cur, n, TokenKind::LineComment { doc_style: None });
    }
    let rest = text.get(cur.byte..).unwrap_or("");
    for token in lexer::tokenize(rest, FrontmatterAllowed::No) {
        let len = token.len as usize;
        if len == 0 {
            break;
        }
        push_tok(&mut toks, text, &mut cur, len, token.kind);
    }
    if cur.byte < text.len() {
        let rest = text.len() - cur.byte;
        push_tok(&mut toks, text, &mut cur, rest, TokenKind::Unknown);
    }
    toks
}

pub(crate) fn push_tok(toks: &mut Vec<Tok>, text: &str, cur: &mut Cursor, len: usize, kind: TokenKind) {
    let start = cur.byte;
    let mut end = start.saturating_add(len).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    let slice = text.get(start..end).unwrap_or("");
    let (mut chars, mut newlines, mut chars_before_last_line) = (0usize, 0usize, 0usize);
    for &b in slice.as_bytes() {
        if b & 0xC0 != 0x80 {
            chars += 1;
        }
        if b == b'\n' {
            newlines += 1;
            chars_before_last_line = chars;
        }
    }
    toks.push(Tok {
        kind,
        start,
        end,
        cstart: cur.ch,
        cend: cur.ch + chars,
        line: cur.line,
        col: cur.ch.saturating_sub(cur.line_start),
        end_line: cur.line + newlines,
    });
    if newlines > 0 {
        cur.line_start = cur.ch + chars_before_last_line;
    }
    cur.byte = end;
    cur.ch += chars;
    cur.line += newlines;
}

fn tok_text<'a>(text: &'a str, tok: &Tok) -> &'a str {
    text.get(tok.start..tok.end).unwrap_or("")
}

pub(crate) fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::LineComment { .. }
            | TokenKind::BlockComment { .. }
            | TokenKind::Frontmatter { .. }
    )
}

fn bracket_char(kind: TokenKind) -> Option<char> {
    match kind {
        TokenKind::OpenParen => Some('('),
        TokenKind::CloseParen => Some(')'),
        TokenKind::OpenBracket => Some('['),
        TokenKind::CloseBracket => Some(']'),
        TokenKind::OpenBrace => Some('{'),
        TokenKind::CloseBrace => Some('}'),
        _ => None,
    }
}

/// The significant (non-whitespace, non-comment) tokens, addressed by position `p`.
pub(crate) struct Sig<'a> {
    text: &'a str,
    toks: &'a [Tok],
    pub(crate) idx: Vec<usize>,
}

impl<'a> Sig<'a> {
    pub(crate) fn new(text: &'a str, toks: &'a [Tok]) -> Self {
        let idx = toks.iter().enumerate().filter(|(_, t)| !is_trivia(t.kind)).map(|(i, _)| i).collect();
        Self { text, toks, idx }
    }

    pub(crate) fn len(&self) -> usize {
        self.idx.len()
    }

    fn tok(&self, p: usize) -> Option<&'a Tok> {
        self.idx.get(p).and_then(|&i| self.toks.get(i))
    }

    pub(crate) fn kind(&self, p: usize) -> Option<TokenKind> {
        self.tok(p).map(|t| t.kind)
    }

    pub(crate) fn is(&self, p: usize, kind: TokenKind) -> bool {
        self.kind(p) == Some(kind)
    }

    fn text(&self, p: usize) -> &'a str {
        self.tok(p).map_or("", |t| tok_text(self.text, t))
    }

    /// Text of a plain (non-raw) identifier or keyword.
    fn word(&self, p: usize) -> Option<&'a str> {
        self.tok(p).filter(|t| t.kind == TokenKind::Ident).map(|t| tok_text(self.text, t))
    }

    fn is_word(&self, p: usize, word: &str) -> bool {
        self.word(p) == Some(word)
    }

    fn is_ident(&self, p: usize) -> bool {
        matches!(self.kind(p), Some(TokenKind::Ident | TokenKind::RawIdent))
    }

    fn is_open(&self, p: usize) -> bool {
        matches!(self.kind(p), Some(TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace))
    }

    /// `p` and `p + 1` touch (no whitespace between).
    fn adjacent(&self, p: usize) -> bool {
        matches!((self.tok(p), self.tok(p + 1)), (Some(a), Some(b)) if a.end == b.start)
    }

    /// For each opening bracket, the position of the closer that balances it (counting all
    /// bracket kinds alike), if any.
    pub(crate) fn group_ends(&self) -> Vec<Option<usize>> {
        let mut ends = vec![None; self.len()];
        let mut stack = Vec::new();
        for p in 0..self.len() {
            match self.kind(p) {
                Some(TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace) => stack.push(p),
                Some(TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace) => {
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
}

// ---------------------------------------------------------------------------------------------
// Highlighting

pub(crate) fn highlight(text: &str) -> Highlights {
    let toks = lex(text);
    let kinds = classify(text, &toks);
    let mut sink = SpanSink::with_capacity(toks.len());
    for (tok, &kind) in toks.iter().zip(&kinds) {
        sink.push(tok.cstart, tok.cend, kind);
    }
    common::to_highlights(&LineIndex::new(text), &sink.into_spans())
}

/// Highlight kind per token.
pub(crate) fn classify(text: &str, toks: &[Tok]) -> Vec<HighlightKind> {
    let mut kinds: Vec<HighlightKind> = toks.iter().map(|t| base_kind(t.kind)).collect();
    let sig = Sig::new(text, toks);
    let ends = sig.group_ends();
    let mut p = 0;
    while p < sig.len() {
        p = match sig.kind(p) {
            Some(TokenKind::Pound) => attribute(&sig, &|o| ends.get(o).copied().flatten(), p, &mut kinds),
            Some(TokenKind::Ident | TokenKind::RawIdent) => ident(&sig, p, &mut kinds),
            _ => p + 1,
        };
    }
    kinds
}

pub(crate) fn base_kind(kind: TokenKind) -> HighlightKind {
    use HighlightKind as K;
    match kind {
        TokenKind::LineComment { doc_style } | TokenKind::BlockComment { doc_style, .. } => {
            if doc_style.is_some() {
                K::DocComment
            } else {
                K::Comment
            }
        }
        TokenKind::Frontmatter { .. } => K::Comment,
        TokenKind::Whitespace | TokenKind::Eof => K::Plain,
        TokenKind::Ident | TokenKind::RawIdent => K::Ident,
        TokenKind::ForcedKeywordIdent => K::Keyword,
        TokenKind::InvalidIdent
        | TokenKind::UnknownPrefix
        | TokenKind::UnknownPrefixLifetime
        | TokenKind::GuardedStrPrefix
        | TokenKind::Unknown => K::Invalid,
        TokenKind::Literal { kind, .. } => literal_kind(kind),
        TokenKind::Lifetime { starts_with_number: true } => K::Invalid,
        TokenKind::Lifetime { .. } | TokenKind::RawLifetime => K::Lifetime,
        TokenKind::Semi
        | TokenKind::Comma
        | TokenKind::Dot
        | TokenKind::Colon
        | TokenKind::Pound
        | TokenKind::Dollar => K::Punctuation,
        TokenKind::OpenParen
        | TokenKind::CloseParen
        | TokenKind::OpenBrace
        | TokenKind::CloseBrace
        | TokenKind::OpenBracket
        | TokenKind::CloseBracket => K::Bracket,
        TokenKind::At
        | TokenKind::Tilde
        | TokenKind::Question
        | TokenKind::Eq
        | TokenKind::Bang
        | TokenKind::Lt
        | TokenKind::Gt
        | TokenKind::Minus
        | TokenKind::And
        | TokenKind::Or
        | TokenKind::Plus
        | TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Caret
        | TokenKind::Percent => K::Operator,
    }
}

fn literal_kind(kind: LiteralKind) -> HighlightKind {
    use HighlightKind as K;
    let ok = |terminated: bool, kind: HighlightKind| if terminated { kind } else { K::Invalid };
    match kind {
        LiteralKind::Int { empty_int, .. } => ok(!empty_int, K::Number),
        LiteralKind::Float { empty_exponent, .. } => ok(!empty_exponent, K::Number),
        LiteralKind::Char { terminated } | LiteralKind::Byte { terminated } => ok(terminated, K::Char),
        LiteralKind::Str { terminated } | LiteralKind::ByteStr { terminated } | LiteralKind::CStr { terminated } => {
            ok(terminated, K::String)
        }
        LiteralKind::RawStr { n_hashes } | LiteralKind::RawByteStr { n_hashes } | LiteralKind::RawCStr { n_hashes } => {
            ok(n_hashes.is_some(), K::String)
        }
    }
}

fn set(kinds: &mut [HighlightKind], sig: &Sig<'_>, p: usize, kind: HighlightKind) {
    if let Some(slot) = sig.idx.get(p).and_then(|&i| kinds.get_mut(i)) {
        *slot = kind;
    }
}

/// `#[...]` / `#![...]` at `p`: marks the whole attribute and returns the next position.
/// `close_of(open)` is the position of the closer that balances the opener at `open`.
pub(crate) fn attribute(
    sig: &Sig<'_>,
    close_of: &dyn Fn(usize) -> Option<usize>,
    p: usize,
    kinds: &mut [HighlightKind],
) -> usize {
    let open = if sig.is(p + 1, TokenKind::OpenBracket) {
        p + 1
    } else if sig.is(p + 1, TokenKind::Bang) && sig.is(p + 2, TokenKind::OpenBracket) {
        p + 2
    } else {
        return p + 1;
    };
    let close = close_of(open).filter(|&c| sig.is(c, TokenKind::CloseBracket));
    let Some(close) = close else {
        // Unterminated (typically mid-typing): mark just the opener.
        for q in p..=open {
            set(kinds, sig, q, HighlightKind::Attribute);
        }
        return open + 1;
    };
    if let (Some(&first), Some(&last)) = (sig.idx.get(p), sig.idx.get(close)) {
        for i in first..=last {
            if let (Some(tok), Some(slot)) = (sig.toks.get(i), kinds.get_mut(i))
                && !is_trivia(tok.kind)
            {
                *slot = HighlightKind::Attribute;
            }
        }
    }
    close + 1
}

/// Classifies the identifier at `p` (and a macro `!` after it); returns the next position.
pub(crate) fn ident(sig: &Sig<'_>, p: usize, kinds: &mut [HighlightKind]) -> usize {
    let raw = sig.is(p, TokenKind::RawIdent);
    let full = sig.text(p);
    let name = if raw { full.get(2..).unwrap_or(full) } else { full };
    if sig.is(p + 1, TokenKind::Bang) && sig.adjacent(p) {
        if !raw && name == "macro_rules" {
            set(kinds, sig, p, HighlightKind::Macro);
            set(kinds, sig, p + 1, HighlightKind::Macro);
            if sig.is_ident(p + 2) {
                set(kinds, sig, p + 2, HighlightKind::Macro);
                return p + 3;
            }
            return p + 2;
        }
        if sig.is_open(p + 2) && !matches!(name, "self" | "Self" | "crate" | "super" | "true" | "false") {
            set(kinds, sig, p, HighlightKind::Macro);
            set(kinds, sig, p + 1, HighlightKind::Macro);
            return p + 2;
        }
    }
    set(kinds, sig, p, word_kind(sig, p, name, raw));
    p + 1
}

fn word_kind(sig: &Sig<'_>, p: usize, name: &str, raw: bool) -> HighlightKind {
    use HighlightKind as K;
    let prev = p.checked_sub(1);
    let prev_word = prev.and_then(|q| sig.word(q));
    if !raw {
        match name {
            "true" | "false" => return K::Boolean,
            "self" | "Self" | "crate" | "super" => return K::SelfKeyword,
            _ => {}
        }
        if is_keyword(name) || is_contextual_keyword(sig, p, name) {
            return K::Keyword;
        }
        if is_primitive(name) {
            return K::Type;
        }
    }
    match prev_word {
        Some("fn") => return K::Function,
        Some("struct" | "enum" | "trait" | "type" | "union") => return K::Type,
        _ => {}
    }
    if is_screaming(name) {
        return K::Constant;
    }
    if name.chars().next().is_some_and(char::is_uppercase) {
        return K::Type;
    }
    if is_call(sig, p + 1) {
        return K::Function;
    }
    if let Some(q) = prev
        && sig.is(q, TokenKind::Dot)
        && !q.checked_sub(1).is_some_and(|r| sig.is(r, TokenKind::Dot))
    {
        return K::Field;
    }
    K::Ident
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "static"
            | "struct"
            | "trait"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            // Reserved for future use.
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "try"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
    )
}

/// Weak keywords, only in the position where they act as keywords.
fn is_contextual_keyword(sig: &Sig<'_>, p: usize, name: &str) -> bool {
    let next = sig.word(p + 1);
    match name {
        "union" => sig.is_ident(p + 1),
        "auto" => next == Some("trait"),
        "default" => {
            matches!(next, Some("fn" | "type" | "const" | "unsafe" | "async" | "impl" | "pub" | "extern"))
        }
        "raw" => p.checked_sub(1).is_some_and(|q| sig.is(q, TokenKind::And)) && matches!(next, Some("const" | "mut")),
        "safe" => matches!(next, Some("fn" | "static")),
        _ => false,
    }
}

fn is_primitive(word: &str) -> bool {
    matches!(
        word,
        "bool"
            | "char"
            | "str"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f16"
            | "f32"
            | "f64"
            | "f128"
    )
}

/// SCREAMING_CASE: at least two chars, upper-case letters, digits and underscores only.
fn is_screaming(name: &str) -> bool {
    let mut len = 0usize;
    let mut upper = false;
    for c in name.chars() {
        len += 1;
        if c.is_uppercase() {
            upper = true;
        } else if !(c.is_ascii_digit() || c == '_') {
            return false;
        }
    }
    len >= 2 && upper
}

/// `q` starts call arguments: `(`, or a turbofish `::<…>` followed by `(`.
fn is_call(sig: &Sig<'_>, q: usize) -> bool {
    const MAX_TURBOFISH: usize = 64;
    if sig.is(q, TokenKind::OpenParen) {
        return true;
    }
    if !(sig.is(q, TokenKind::Colon) && sig.is(q + 1, TokenKind::Colon) && sig.is(q + 2, TokenKind::Lt)) {
        return false;
    }
    let mut depth = 0usize;
    for r in q + 2..sig.len().min(q + 2 + MAX_TURBOFISH) {
        match sig.kind(r) {
            Some(TokenKind::Lt) => depth += 1,
            Some(TokenKind::Gt) if !is_arrow(sig, r) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return sig.is(r + 1, TokenKind::OpenParen);
                }
            }
            Some(TokenKind::OpenBrace | TokenKind::CloseBrace | TokenKind::Semi) => return false,
            _ => {}
        }
    }
    false
}

/// The `>` at `r` is the tail of `->`.
fn is_arrow(sig: &Sig<'_>, r: usize) -> bool {
    r.checked_sub(1).is_some_and(|q| sig.is(q, TokenKind::Minus) && sig.adjacent(q))
}

// ---------------------------------------------------------------------------------------------
// Brackets and folding

/// Bracket tokens (brackets in strings, chars and comments are not tokens of their own).
pub(crate) fn brackets(text: &str) -> Vec<Bracket> {
    lex(text).iter().filter_map(|t| bracket_char(t.kind).map(|ch| Bracket { offset: t.cstart, ch })).collect()
}

pub(crate) fn folding_ranges(text: &str) -> Vec<FoldRange> {
    folding_ranges_from(text, &lex(text))
}

/// Fold regions from an existing token stream of `text`.
pub(crate) fn folding_ranges_from(text: &str, toks: &[Tok]) -> Vec<FoldRange> {
    let mut out = Vec::new();
    common::bracket_folds(toks.iter().filter_map(|t| bracket_char(t.kind).map(|c| (c, t.line))), &mut out);
    comment_folds(toks, &mut out);
    import_folds(&Sig::new(text, toks), &mut out);
    common::finish_folds(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommentClass {
    Plain,
    OuterDoc,
    InnerDoc,
}

/// Runs of whole-line `//` comments of one class, and multi-line block comments.
fn comment_folds(toks: &[Tok], out: &mut Vec<FoldRange>) {
    let flush = |run: Option<(CommentClass, usize, usize)>, out: &mut Vec<FoldRange>| {
        if let Some((_, start, end)) = run {
            common::push_fold(out, start, end, FoldKind::Comment);
        }
    };
    let mut run: Option<(CommentClass, usize, usize)> = None;
    let mut last_code_line: Option<usize> = None;
    for tok in toks {
        match tok.kind {
            TokenKind::Whitespace => {}
            TokenKind::LineComment { doc_style } => {
                let class = match doc_style {
                    None => CommentClass::Plain,
                    Some(lexer::DocStyle::Outer) => CommentClass::OuterDoc,
                    Some(lexer::DocStyle::Inner) => CommentClass::InnerDoc,
                };
                let alone = last_code_line.is_none_or(|l| l < tok.line);
                match run {
                    Some((c, start, end)) if alone && c == class && end + 1 == tok.line => {
                        run = Some((c, start, tok.line));
                    }
                    _ => {
                        flush(run.take(), out);
                        if alone {
                            run = Some((class, tok.line, tok.line));
                        }
                    }
                }
            }
            TokenKind::BlockComment { .. } => {
                flush(run.take(), out);
                common::push_fold(out, tok.line, tok.end_line, FoldKind::Comment);
                last_code_line = Some(tok.end_line);
            }
            _ => {
                flush(run.take(), out);
                last_code_line = Some(tok.end_line);
            }
        }
    }
    flush(run, out);
}

/// Runs of at least two consecutive `use` declarations.
fn import_folds(sig: &Sig<'_>, out: &mut Vec<FoldRange>) {
    let mut p = 0;
    while p < sig.len() {
        let Some((start_line, mut end_line, mut next)) = use_item(sig, p) else {
            p += 1;
            continue;
        };
        let mut count = 1;
        while let Some((_, end, after)) = use_item(sig, next) {
            count += 1;
            end_line = end;
            next = after;
        }
        if count >= 2 {
            common::push_fold(out, start_line, end_line, FoldKind::Imports);
        }
        p = next;
    }
}

/// A `use …;` item (optionally `pub`/`pub(…)`) starting at `p`: (first line, line of `;`,
/// position after `;`).
fn use_item(sig: &Sig<'_>, p: usize) -> Option<(usize, usize, usize)> {
    const MAX_USE_TOKENS: usize = 4096;
    let start_line = sig.tok(p)?.line;
    let mut q = p;
    if sig.is_word(q, "pub") {
        q += 1;
        if sig.is(q, TokenKind::OpenParen) {
            q = (q..sig.len().min(q + 16)).find(|&r| sig.is(r, TokenKind::CloseParen))? + 1;
        }
    }
    if !sig.is_word(q, "use") || sig.is(q + 1, TokenKind::Lt) {
        return None;
    }
    let mut depth = 0usize;
    for r in q + 1..sig.len().min(q + MAX_USE_TOKENS) {
        match sig.kind(r) {
            Some(TokenKind::OpenBrace) => depth += 1,
            Some(TokenKind::CloseBrace) => depth = depth.checked_sub(1)?,
            Some(TokenKind::Semi) if depth == 0 => return Some((start_line, sig.tok(r)?.line, r + 1)),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Outline

pub(crate) fn outline(text: &str) -> Vec<OutlineItem> {
    let toks = lex(text);
    let sig = Sig::new(text, &toks);
    let ends = sig.group_ends();
    Outliner { sig, ends, items: Vec::new(), frames: Vec::new(), depth: 0 }.run()
}

/// Walks item-level tokens. Bodies of `impl`/`trait`/`mod` (and `extern` blocks) are entered;
/// every other `{…}`, `(…)` and `[…]` at item level is skipped whole, so the contents of fn
/// bodies, struct fields and macro invocations never produce items.
struct Outliner<'a> {
    sig: Sig<'a>,
    ends: Vec<Option<usize>>,
    items: Vec<OutlineItem>,
    /// One entry per entered `{`: whether it increased the depth.
    frames: Vec<bool>,
    depth: usize,
}

impl Outliner<'_> {
    fn run(mut self) -> Vec<OutlineItem> {
        let mut p = 0;
        while p < self.sig.len() {
            p = match self.sig.kind(p) {
                Some(TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket) => self.skip_group(p),
                Some(TokenKind::CloseBrace) => {
                    if self.frames.pop() == Some(true) {
                        self.depth = self.depth.saturating_sub(1);
                    }
                    p + 1
                }
                Some(TokenKind::Pound) => {
                    if self.sig.is(p + 1, TokenKind::OpenBracket) {
                        self.skip_group(p + 1)
                    } else if self.sig.is(p + 1, TokenKind::Bang) && self.sig.is(p + 2, TokenKind::OpenBracket) {
                        self.skip_group(p + 2)
                    } else {
                        p + 1
                    }
                }
                Some(TokenKind::Ident) => self.item(p),
                _ => p + 1,
            };
        }
        self.items
    }

    /// Handles the word at `p`; returns the next position.
    fn item(&mut self, p: usize) -> usize {
        let sig = &self.sig;
        match sig.text(p) {
            "fn" => {
                self.named(p + 1, SymbolKind::Function);
                self.skip_item(p + 1)
            }
            "struct" => {
                self.named(p + 1, SymbolKind::Struct);
                self.skip_item(p + 1)
            }
            "union" if sig.is_ident(p + 1) => {
                self.named(p + 1, SymbolKind::Struct);
                self.skip_item(p + 1)
            }
            "enum" => {
                self.named(p + 1, SymbolKind::Enum);
                self.skip_item(p + 1)
            }
            "trait" => {
                self.named(p + 1, SymbolKind::Trait);
                self.container(p + 1, true)
            }
            "impl" => {
                let name = self.impl_name(p);
                self.push(name, SymbolKind::Impl, p);
                self.container(p + 1, true)
            }
            "mod" => {
                self.named(p + 1, SymbolKind::Module);
                if self.sig.is(p + 2, TokenKind::OpenBrace) { self.enter(p + 2, true) } else { p + 2 }
            }
            "const" => match sig.kind(p + 1) {
                Some(TokenKind::Ident) if matches!(sig.text(p + 1), "fn" | "unsafe" | "async" | "extern") => p + 1,
                Some(TokenKind::Ident | TokenKind::RawIdent) => {
                    if sig.text(p + 1) != "_" {
                        self.named(p + 1, SymbolKind::Const);
                    }
                    self.skip_to_semi(p + 1)
                }
                _ => p + 1,
            },
            "static" => {
                let q = if sig.is_word(p + 1, "mut") { p + 2 } else { p + 1 };
                self.named(q, SymbolKind::Static);
                self.skip_to_semi(q)
            }
            "type" => {
                self.named(p + 1, SymbolKind::TypeAlias);
                self.skip_to_semi(p + 1)
            }
            "macro_rules" if sig.is(p + 1, TokenKind::Bang) => {
                self.named(p + 2, SymbolKind::Macro);
                self.skip_macro_body(p + 3)
            }
            "macro" if sig.is_ident(p + 1) => {
                self.named(p + 1, SymbolKind::Macro);
                self.skip_item(p + 2)
            }
            "extern" => {
                if sig.is_word(p + 1, "crate") {
                    self.skip_to_semi(p + 1)
                } else if sig.is(p + 1, TokenKind::OpenBrace) {
                    self.enter(p + 1, false)
                } else if matches!(sig.kind(p + 1), Some(TokenKind::Literal { .. }))
                    && sig.is(p + 2, TokenKind::OpenBrace)
                {
                    self.enter(p + 2, false)
                } else {
                    p + 1
                }
            }
            "use" => self.skip_to_semi(p + 1),
            _ if sig.is(p + 1, TokenKind::Bang) => self.skip_macro_body(p + 2),
            _ => p + 1,
        }
    }

    /// Records an item named by the identifier at `p`, if there is one.
    fn named(&mut self, p: usize, kind: SymbolKind) {
        if self.sig.is_ident(p) {
            let name = self.sig.text(p).to_string();
            self.push(name, kind, p);
        }
    }

    fn push(&mut self, name: String, kind: SymbolKind, p: usize) {
        if let Some(tok) = self.sig.tok(p) {
            self.items.push(OutlineItem { name, kind, line: tok.line, column: tok.col, depth: self.depth });
        }
    }

    /// `impl Trait for Type` / `impl Type`, without the impl's own generic parameters.
    fn impl_name(&self, p: usize) -> String {
        const MAX_NAME: usize = 200;
        let mut start = p + 1;
        if self.sig.is(start, TokenKind::Lt) {
            start = self.skip_angle(start);
        }
        let mut end = start;
        while end < self.sig.len() {
            match self.sig.kind(end) {
                Some(TokenKind::OpenBrace | TokenKind::CloseBrace | TokenKind::Semi) => break,
                Some(TokenKind::Ident) if self.sig.text(end) == "where" => break,
                Some(TokenKind::OpenParen | TokenKind::OpenBracket) => end = self.skip_group(end),
                _ => end += 1,
            }
        }
        let mut name = String::from("impl");
        let (Some(&first), Some(&last)) =
            (self.sig.idx.get(start), end.checked_sub(1).and_then(|e| self.sig.idx.get(e)))
        else {
            return name;
        };
        for tok in self.sig.toks.get(first..=last).unwrap_or(&[]) {
            if is_trivia(tok.kind) {
                if !name.ends_with(' ') {
                    name.push(' ');
                }
            } else {
                if !name.ends_with(' ') && name == "impl" {
                    name.push(' ');
                }
                name.push_str(tok_text(self.sig.text, tok));
            }
            if name.chars().count() > MAX_NAME {
                name = name.chars().take(MAX_NAME).collect();
                name.push('…');
                break;
            }
        }
        name.trim_end().to_string()
    }

    /// Past the `<…>` starting at `p` (a `>` of `->` does not close).
    fn skip_angle(&self, p: usize) -> usize {
        let mut depth = 0usize;
        let mut q = p;
        while q < self.sig.len() {
            match self.sig.kind(q) {
                Some(TokenKind::Lt) => depth += 1,
                Some(TokenKind::Gt) if !is_arrow(&self.sig, q) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return q + 1;
                    }
                }
                Some(TokenKind::OpenBrace | TokenKind::CloseBrace | TokenKind::Semi) => return q,
                _ => {}
            }
            q += 1;
        }
        q
    }

    /// Past the group opened at `p` (to the end of the text if it never closes).
    fn skip_group(&self, p: usize) -> usize {
        self.ends.get(p).copied().flatten().map_or(self.sig.len(), |e| e + 1)
    }

    fn enter(&mut self, brace: usize, deeper: bool) -> usize {
        self.frames.push(deeper);
        if deeper {
            self.depth += 1;
        }
        brace + 1
    }

    /// Scans a `trait`/`impl` header from `p`: enters its `{` body, or stops after `;`.
    fn container(&mut self, p: usize, deeper: bool) -> usize {
        let mut q = p;
        while q < self.sig.len() {
            match self.sig.kind(q) {
                Some(TokenKind::OpenBrace) => return self.enter(q, deeper),
                Some(TokenKind::OpenParen | TokenKind::OpenBracket) => q = self.skip_group(q),
                Some(TokenKind::Semi) => return q + 1,
                Some(TokenKind::CloseBrace) => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// Skips an item header and its `{…}` body or terminating `;`. Stops before a stray `}`.
    fn skip_item(&self, p: usize) -> usize {
        let mut q = p;
        while q < self.sig.len() {
            match self.sig.kind(q) {
                Some(TokenKind::OpenBrace) => return self.skip_group(q),
                Some(TokenKind::OpenParen | TokenKind::OpenBracket) => q = self.skip_group(q),
                Some(TokenKind::Semi) => return q + 1,
                Some(TokenKind::CloseBrace) => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// Skips to just past the `;` ending a `const`/`static`/`type`/`use` item.
    fn skip_to_semi(&self, p: usize) -> usize {
        let mut q = p;
        while q < self.sig.len() {
            match self.sig.kind(q) {
                Some(TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket) => q = self.skip_group(q),
                Some(TokenKind::Semi) => return q + 1,
                Some(TokenKind::CloseBrace) => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// Skips a macro body group at `p` and an optional `;` after it.
    fn skip_macro_body(&self, p: usize) -> usize {
        let mut q = p;
        // `macro_rules! name` bodies and `ident! ident { … }` forms: skip a name first.
        if self.sig.is_ident(q) && self.sig.is_open(q + 1) {
            q += 1;
        }
        if self.sig.is_open(q) {
            q = self.skip_group(q);
        }
        if self.sig.is(q, TokenKind::Semi) { q + 1 } else { q }
    }
}

#[cfg(test)]
mod tests;
