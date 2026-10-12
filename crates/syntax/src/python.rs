//! Python: keywords (soft keywords `match`/`case`/`type` in statement position), decorators,
//! strings with every prefix (f-string replacement fields are code), docstrings and numbers;
//! indentation-based block folds and an outline of classes, functions and module constants.

use crate::common::{self, Bracket, Doc, Span};
use crate::lex::{self, LINE, Lexed, STMT, TokSink, Tt};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

fn lex(doc: &Doc) -> Lexed<'_> {
    let ascii = lex::ascii(doc, false);
    let toks = {
        let mut lexer = Lexer {
            doc,
            ascii: &ascii,
            sink: TokSink::with_capacity(doc.len() / 4),
            depth: 0,
            stmt: true,
            line: true,
            modes: Vec::new(),
        };
        lexer.run();
        lexer.sink.toks
    };
    let mut lexed = Lexed::new(doc, ascii, toks);
    classify(&mut lexed);
    lexed
}

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    lex(doc).spans()
}

pub(crate) fn brackets(doc: &Doc) -> Vec<Bracket> {
    lex(doc).brackets()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The literal part of an f-string.
    FStr { quote: char, triple: bool, raw: bool },
    /// The code of a replacement field; `nest` counts brackets opened inside it.
    Field { nest: usize },
    /// A format spec (after `:` in a replacement field).
    Spec,
}

struct Lexer<'a> {
    doc: &'a Doc,
    ascii: &'a [u8],
    sink: TokSink,
    /// Bracket depth of top-level code: newlines inside brackets do not end a statement.
    depth: usize,
    /// The next significant token starts a statement / a logical line.
    stmt: bool,
    line: bool,
    modes: Vec<Mode>,
}

/// String prefix letters: (raw, formatted), or `None` when `word` is not a prefix.
fn string_prefix(word: &[u8]) -> Option<(bool, bool)> {
    if word.is_empty() || word.len() > 2 {
        return None;
    }
    let lower: Vec<u8> = word.iter().map(u8::to_ascii_lowercase).collect();
    let ok =
        matches!(lower.as_slice(), b"r" | b"u" | b"b" | b"f" | b"t" | b"br" | b"rb" | b"fr" | b"rf" | b"tr" | b"rt");
    ok.then(|| (lower.contains(&b'r'), lower.contains(&b'f') || lower.contains(&b't')))
}

impl Lexer<'_> {
    fn push(&mut self, start: usize, end: usize, tt: Tt, kind: K) {
        let mut flags = 0;
        if !matches!(tt, Tt::LineComment | Tt::BlockComment) && self.stmt && end > start {
            flags = STMT | if self.line { LINE } else { 0 };
            self.stmt = false;
            self.line = false;
        }
        self.sink.push_flags(start, end, tt, kind, flags);
    }

    fn sym(&mut self, i: usize, c: char) {
        self.push(i, i + 1, Tt::Sym(c), lex::sym_kind(c));
    }

    fn run(&mut self) {
        let n = self.doc.len();
        let mut i = 0;
        while i < n {
            let modes = self.modes.len();
            let next = match self.modes.last().copied() {
                None | Some(Mode::Field { .. }) => self.code(i),
                Some(Mode::FStr { quote, triple, raw }) => self.fstring(i, i, quote, triple, raw),
                Some(Mode::Spec) => self.spec(i),
            };
            // Progress: every step consumes a char or leaves a mode.
            i = if next > i || self.modes.len() < modes { next } else { i + 1 };
        }
    }

    fn code(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let c = doc.at(i);
        match c {
            '\n' => {
                self.newline();
                i + 1
            }
            c if c.is_whitespace() => i + 1,
            '\\' => {
                // Line continuation.
                if doc.at(i + 1) == '\n' {
                    i + 2
                } else if doc.at(i + 1) == '\r' && doc.at(i + 2) == '\n' {
                    i + 3
                } else {
                    self.push(i, i + 1, Tt::Other, K::Invalid);
                    i + 1
                }
            }
            '#' => {
                let e = doc.line_end_at(i);
                self.push(i, e, Tt::LineComment, K::Comment);
                e
            }
            '\'' | '"' => self.string(i, i, false, false),
            c if c.is_ascii_digit() => self.number(i),
            '.' if lex::dot_number(doc, i) => self.number(i),
            c if lex::is_ident_start(c) => {
                let e = doc.scan_while(i, lex::is_ident_char);
                if matches!(doc.at(e), '\'' | '"')
                    && let Some((raw, formatted)) = string_prefix(self.ascii.get(i..e).unwrap_or(&[]))
                {
                    return self.string(i, e, raw, formatted);
                }
                self.push(i, e, Tt::Word, K::Ident);
                e
            }
            '@' if self.stmt && self.modes.is_empty() => {
                // Decorator: `@name.attr`, arguments stay code.
                let mut e = doc.scan_while(i + 1, |c| c == ' ' || c == '\t');
                while lex::is_ident_start(doc.at(e)) {
                    e = doc.scan_while(e, lex::is_ident_char);
                    if doc.at(e) == '.' && lex::is_ident_start(doc.at(e + 1)) {
                        e += 1;
                    }
                }
                self.push(i, e.max(i + 1), Tt::Other, K::Attribute);
                e.max(i + 1)
            }
            '(' | '[' | '{' => {
                match self.modes.last_mut() {
                    Some(Mode::Field { nest }) => *nest += 1,
                    _ => self.depth += 1,
                }
                self.sym(i, c);
                i + 1
            }
            ')' | ']' | '}' => {
                match self.modes.last_mut() {
                    Some(Mode::Field { nest: 0 }) if c == '}' => {
                        self.modes.pop();
                    }
                    Some(Mode::Field { nest }) => *nest = nest.saturating_sub(1),
                    _ => self.depth = self.depth.saturating_sub(1),
                }
                self.sym(i, c);
                i + 1
            }
            ':' if self.modes.last() == Some(&Mode::Field { nest: 0 }) => {
                self.sym(i, c);
                if let Some(mode) = self.modes.last_mut() {
                    *mode = Mode::Spec;
                }
                i + 1
            }
            ';' => {
                self.sym(i, c);
                if self.modes.is_empty() && self.depth == 0 {
                    self.stmt = true;
                }
                i + 1
            }
            c if c.is_ascii_punctuation() => {
                self.sym(i, c);
                i + 1
            }
            _ => {
                self.push(i, i + 1, Tt::Other, K::Invalid);
                i + 1
            }
        }
    }

    /// A newline in code: ends the logical line at the top level; inside a replacement field of
    /// a single-line f-string it ends the (unterminated) string.
    fn newline(&mut self) {
        let single = self.modes.iter().rposition(|m| matches!(m, Mode::FStr { triple: false, .. }));
        let triple = self.modes.iter().rposition(|m| matches!(m, Mode::FStr { triple: true, .. }));
        if let Some(at) = single
            && triple.is_none_or(|t| t < at)
        {
            self.modes.truncate(at);
        }
        if self.modes.is_empty() && self.depth == 0 {
            self.stmt = true;
            self.line = true;
        }
    }

    fn number(&mut self, i: usize) -> usize {
        let e = lex::number_end(self.doc, i, false);
        self.push(i, e, Tt::Lit, K::Number);
        e
    }

    /// A string literal whose prefix starts at `start` and opening quote is at `q`.
    fn string(&mut self, start: usize, q: usize, raw: bool, formatted: bool) -> usize {
        let doc = self.doc;
        let quote = doc.at(q);
        let triple = doc.at(q + 1) == quote && doc.at(q + 2) == quote;
        let body = if triple { q + 3 } else { q + 1 };
        if formatted {
            self.modes.push(Mode::FStr { quote, triple, raw });
            return self.fstring(start, body, quote, triple, raw);
        }
        let (e, ok) =
            if triple { triple_end(doc, body, quote) } else { lex::scan_quoted(doc, body, quote, true, false) };
        self.push(start, e, Tt::Lit, if ok { K::String } else { K::Invalid });
        e
    }

    /// Scans the literal part of an f-string from `j` (its segment starts at `seg`): stops at a
    /// replacement field (entering it) or at the closing quote (leaving the f-string).
    fn fstring(&mut self, seg: usize, from: usize, quote: char, triple: bool, raw: bool) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let mut j = from;
        while j < n {
            let c = doc.at(j);
            if c == '\\' {
                if raw {
                    j += if doc.at(j + 1) == quote || doc.at(j + 1) == '\\' { 2 } else { 1 };
                } else if doc.at(j + 1) == 'N' && doc.at(j + 2) == '{' {
                    // `\N{NAME}`: braces of a named escape are not a field.
                    let le = doc.line_end_at(j);
                    j = (j + 3..le).find(|&k| doc.at(k) == '}').map_or(j + 3, |k| k + 1);
                } else {
                    j += 2;
                }
                continue;
            }
            match c {
                '{' if doc.at(j + 1) == '{' => j += 2,
                '}' if doc.at(j + 1) == '}' => j += 2,
                '{' => {
                    self.push(seg, j, Tt::Lit, K::String);
                    self.sym(j, '{');
                    self.modes.push(Mode::Field { nest: 0 });
                    return j + 1;
                }
                c if c == quote && (!triple || (doc.at(j + 1) == quote && doc.at(j + 2) == quote)) => {
                    let e = if triple { j + 3 } else { j + 1 };
                    self.push(seg, e, Tt::Lit, K::String);
                    self.modes.pop();
                    return e;
                }
                '\n' if !triple => {
                    self.push(seg, j, Tt::Lit, K::Invalid);
                    self.modes.pop();
                    return j;
                }
                _ => j += 1,
            }
        }
        self.push(seg, n, Tt::Lit, K::Invalid);
        self.modes.pop();
        n
    }

    /// A format spec: literal text until the field's `}`; `{…}` inside is a nested field.
    fn spec(&mut self, from: usize) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let (quote, triple) = self
            .modes
            .iter()
            .rev()
            .find_map(|m| match m {
                Mode::FStr { quote, triple, .. } => Some((*quote, *triple)),
                _ => None,
            })
            .unwrap_or(('\0', true));
        let mut j = from;
        while j < n {
            match doc.at(j) {
                '{' => {
                    self.push(from, j, Tt::Lit, K::String);
                    self.sym(j, '{');
                    self.modes.push(Mode::Field { nest: 0 });
                    return j + 1;
                }
                '}' => {
                    self.push(from, j, Tt::Lit, K::String);
                    self.sym(j, '}');
                    self.modes.pop();
                    return j + 1;
                }
                '\n' if !triple => break,
                c if c == quote => break,
                _ => j += 1,
            }
        }
        // The string ends inside the spec: hand back to the f-string.
        self.push(from, j, Tt::Lit, K::String);
        self.modes.pop();
        j
    }
}

/// End of a triple-quoted body starting at `body`, past the closing quotes, and whether it closed.
fn triple_end(doc: &Doc, body: usize, quote: char) -> (usize, bool) {
    let n = doc.len();
    let mut j = body;
    while j < n {
        match doc.at(j) {
            '\\' => j += 2,
            c if c == quote && doc.at(j + 1) == quote && doc.at(j + 2) == quote => return (j + 3, true),
            _ => j += 1,
        }
    }
    (n, false)
}

// ---------------------------------------------------------------------------------------------
// Classification

fn is_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"and"
            | b"as"
            | b"assert"
            | b"async"
            | b"await"
            | b"break"
            | b"class"
            | b"continue"
            | b"def"
            | b"del"
            | b"elif"
            | b"else"
            | b"except"
            | b"finally"
            | b"for"
            | b"from"
            | b"global"
            | b"if"
            | b"import"
            | b"in"
            | b"is"
            | b"lambda"
            | b"nonlocal"
            | b"not"
            | b"or"
            | b"pass"
            | b"raise"
            | b"return"
            | b"try"
            | b"while"
            | b"with"
            | b"yield"
    )
}

fn is_builtin_type(w: &[u8]) -> bool {
    matches!(
        w,
        b"int"
            | b"str"
            | b"float"
            | b"bool"
            | b"bytes"
            | b"bytearray"
            | b"list"
            | b"dict"
            | b"set"
            | b"frozenset"
            | b"tuple"
            | b"object"
            | b"complex"
            | b"type"
            | b"range"
            | b"memoryview"
    )
}

/// The string token at `p` is triple-quoted and neither an f-string nor bytes.
fn is_plain_triple(lx: &Lexed<'_>, p: usize) -> bool {
    let chars = lx.chars(p);
    let prefix = chars.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    let prefix_ok = chars.iter().take(prefix).all(|c| matches!(c, 'r' | 'R' | 'u' | 'U'));
    let rest = chars.get(prefix..).unwrap_or(&[]);
    prefix_ok && rest.len() >= 3 && (rest.starts_with(&['"'; 3]) || rest.starts_with(&['\''; 3]))
}

fn classify(lx: &mut Lexed<'_>) {
    let stmt_starts: Vec<usize> = (0..lx.len()).filter(|&p| lx.has_flag(p, STMT)).collect();
    let mut next_stmt = 0usize;
    // Current statement: its first word, the bracket depth inside it, and the position of the
    // `:` ending the latest `def`/`class` header.
    let mut header = false;
    let mut depth = 0usize;
    let mut header_colon: Option<usize> = None;
    let mut kinds: Vec<(usize, K)> = Vec::new();
    for p in 0..lx.len() {
        if lx.has_flag(p, STMT) {
            while stmt_starts.get(next_stmt).is_some_and(|&s| s <= p) {
                next_stmt += 1;
            }
            let first = if lx.is_word(p, "async") { p + 1 } else { p };
            header = lx.is_word(first, "def") || lx.is_word(first, "class");
            depth = 0;
        }
        let stmt_end = stmt_starts.get(next_stmt).map_or(lx.len(), |&s| s).saturating_sub(1);
        match lx.tt(p) {
            Some(Tt::Sym('(' | '[' | '{')) => depth += 1,
            Some(Tt::Sym(')' | ']' | '}')) => depth = depth.saturating_sub(1),
            Some(Tt::Sym(':')) if header && depth == 0 => {
                header = false;
                header_colon = Some(p);
            }
            Some(Tt::Lit)
                if lx.kind(p) == Some(K::String)
                    && (p == 0 || header_colon == Some(p - 1))
                    && stmt_end == p
                    && is_plain_triple(lx, p) =>
            {
                kinds.push((p, K::DocComment));
            }
            Some(Tt::Word) => kinds.push((p, word_kind(lx, p, stmt_end))),
            _ => {}
        }
    }
    for (p, kind) in kinds {
        lx.set(p, kind);
    }
}

fn word_kind(lx: &Lexed<'_>, p: usize, stmt_end: usize) -> K {
    let w = lx.word(p);
    if lx.after_member_access(p, false) {
        return if lx.is_call(p + 1, false) { K::Function } else { K::Field };
    }
    match w {
        b"True" | b"False" | b"None" => return K::Boolean,
        b"self" | b"cls" => return K::SelfKeyword,
        _ => {}
    }
    if is_keyword(w) {
        return K::Keyword;
    }
    let at_stmt = lx.has_flag(p, STMT);
    match w {
        b"match" | b"case"
            if at_stmt
                && lx.is_sym(stmt_end, ':')
                && stmt_end > p + 1
                && !matches!(lx.sym(p + 1), Some('=' | '.' | ',' | ')' | ':')) =>
        {
            return K::Keyword;
        }
        b"type" if at_stmt && lx.is_word_tok(p + 1) && matches!(lx.sym(p + 2), Some('=' | '[')) => {
            return K::Keyword;
        }
        _ => {}
    }
    match p.checked_sub(1).map(|q| lx.word(q)) {
        Some(b"def") => return K::Function,
        Some(b"class") => return K::Type,
        _ => {}
    }
    if is_builtin_type(w) {
        return K::Type;
    }
    let chars = lx.chars(p);
    if lex::is_screaming(chars) {
        return K::Constant;
    }
    if lex::is_capitalized(chars) {
        return K::Type;
    }
    if lx.is_call(p + 1, false) {
        return K::Function;
    }
    K::Ident
}

// ---------------------------------------------------------------------------------------------
// Logical lines, folding and outline

/// A logical line: its first and last significant positions and its indentation.
#[derive(Clone, Copy, Debug)]
struct Logical {
    first: usize,
    last: usize,
    indent: usize,
}

fn logical_lines(lx: &Lexed<'_>) -> Vec<Logical> {
    let mut out: Vec<Logical> = Vec::new();
    for p in 0..lx.len() {
        if lx.has_flag(p, LINE) || out.is_empty() {
            out.push(Logical { first: p, last: p, indent: lx.col(p) });
        } else if let Some(last) = out.last_mut() {
            last.last = p;
        }
    }
    out
}

/// The first word of a logical line, skipping `async`.
fn head<'b>(lx: &'b Lexed<'_>, l: &Logical) -> (&'b [u8], usize) {
    if lx.is_word(l.first, "async") { (lx.word(l.first + 1), l.first + 1) } else { (lx.word(l.first), l.first) }
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let lx = lex(doc);
    let mut out = Vec::new();
    lx.bracket_folds(&mut out);
    lx.comment_folds(&mut out);
    for t in &lx.toks {
        if t.tt == Tt::Lit && matches!(t.kind, K::String | K::DocComment) {
            let start = doc.index.line_of(t.start);
            let end = doc.index.line_of(t.end.saturating_sub(1).max(t.start));
            common::push_fold(
                &mut out,
                start,
                end,
                if t.kind == K::DocComment { FoldKind::Comment } else { FoldKind::Block },
            );
        }
    }
    let lines = logical_lines(&lx);
    let stmts: Vec<(bool, usize, usize)> = lines
        .iter()
        .map(|l| (matches!(lx.word(l.first), b"import" | b"from"), lx.line(l.first), lx.end_line(l.last)))
        .collect();
    lex::statement_runs(&stmts, FoldKind::Imports, &mut out);
    block_folds(&lx, &lines, &mut out);
    common::finish_folds(out)
}

/// Compound statements (`def`, `class`, `if`, `for`, `while`, `with`, `try`, …): from the
/// header line to the last line of the indented block.
fn block_folds(lx: &Lexed<'_>, lines: &[Logical], out: &mut Vec<FoldRange>) {
    // Open headers: (indent, start line).
    let mut open: Vec<(usize, usize)> = Vec::new();
    let mut prev_end: Option<usize> = None;
    for l in lines {
        while let Some(&(indent, start)) = open.last()
            && indent >= l.indent
        {
            if let Some(end) = prev_end {
                common::push_fold(out, start, end, FoldKind::Block);
            }
            open.pop();
        }
        let (word, _) = head(lx, l);
        let compound = matches!(
            word,
            b"def"
                | b"class"
                | b"if"
                | b"elif"
                | b"else"
                | b"for"
                | b"while"
                | b"with"
                | b"try"
                | b"except"
                | b"finally"
                | b"match"
                | b"case"
        );
        if compound && lx.is_sym(l.last, ':') {
            open.push((l.indent, lx.line(l.first)));
        }
        prev_end = Some(lx.end_line(l.last));
    }
    if let Some(end) = prev_end {
        for (_, start) in open {
            common::push_fold(out, start, end, FoldKind::Block);
        }
    }
}

pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    let lx = lex(doc);
    let mut items = Vec::new();
    // Enclosing `def`/`class` indents.
    let mut scopes: Vec<usize> = Vec::new();
    for l in logical_lines(&lx) {
        while scopes.last().is_some_and(|&indent| indent >= l.indent) {
            scopes.pop();
        }
        let (word, at) = head(&lx, &l);
        let kind = match word {
            b"def" => Some(SymbolKind::Function),
            b"class" => Some(SymbolKind::Struct),
            _ => None,
        };
        if let Some(kind) = kind {
            if lx.is_word_tok(at + 1)
                && let Some(item) = lx.item(at + 1, lx.text(at + 1), kind, scopes.len())
            {
                items.push(item);
            }
            scopes.push(l.indent);
        } else if l.indent == 0
            && lx.is_word_tok(l.first)
            && lex::is_screaming(lx.chars(l.first))
            && (lx.is_sym(l.first + 1, ':') || (lx.is_sym(l.first + 1, '=') && !lx.is_pair(l.first + 1, '=', '=')))
            && let Some(item) = lx.item(l.first, lx.text(l.first), SymbolKind::Const, 0)
        {
            items.push(item);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, checked, kind_at, kind_of, offset_of, spans};
    use crate::{
        FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
        outline,
    };

    const P: Language = Language::Python;

    fn fold(start_line: usize, end_line: usize, kind: FoldKind) -> FoldRange {
        FoldRange { start_line, end_line, kind }
    }

    #[test]
    fn keywords_functions_classes() {
        let text = "async def fetch(self, url: str) -> None:\n    return await self.get(url) if url else None\nclass Shape(Base):\n    pass";
        assert_kind(P, text, "async", K::Keyword);
        assert_kind(P, text, "def", K::Keyword);
        assert_kind(P, text, "fetch", K::Function);
        assert_kind(P, text, "self", K::SelfKeyword);
        assert_kind(P, text, "str", K::Type);
        assert_kind(P, text, "->", K::Operator);
        assert_kind(P, text, "None", K::Boolean);
        assert_kind(P, text, "return", K::Keyword);
        assert_kind(P, text, "get", K::Function);
        assert_kind(P, text, "url", K::Ident);
        assert_kind(P, text, "if", K::Keyword);
        assert_kind(P, text, "Shape", K::Type);
        assert_kind(P, text, "Base", K::Type);
        assert_kind(P, text, "pass", K::Keyword);
    }

    #[test]
    fn constants_fields_calls_booleans() {
        let text = "MAX_SIZE = len(items) and obj.attr or True\nprint(False)";
        assert_kind(P, text, "MAX_SIZE", K::Constant);
        assert_kind(P, text, "len", K::Function);
        assert_kind(P, text, "attr", K::Field);
        assert_kind(P, text, "True", K::Boolean);
        assert_kind(P, text, "False", K::Boolean);
        assert_kind(P, text, "print", K::Function);
        assert_kind(P, text, "and", K::Keyword);
    }

    #[test]
    fn soft_keywords() {
        let text = "match command:\n    case [x, y]:\n        pass\n    case _:\n        pass\nmatch = 1\nmatch(x)\ntype Alias = int\ntype = 3";
        assert_eq!(kind_at(P, text, 0, 0), K::Keyword);
        assert_eq!(kind_at(P, text, 1, 4), K::Keyword);
        assert_eq!(kind_at(P, text, 3, 4), K::Keyword);
        assert_eq!(kind_at(P, text, 5, 0), K::Ident);
        assert_eq!(kind_at(P, text, 6, 0), K::Function);
        assert_eq!(kind_at(P, text, 7, 0), K::Keyword);
        assert_eq!(kind_at(P, text, 8, 0), K::Type);
    }

    #[test]
    fn decorators() {
        let text = "@property\n@app.route(\"/x\")\ndef f(): pass\nm = a @ b";
        assert_kind(P, text, "@property", K::Attribute);
        assert_kind(P, text, "@app.route", K::Attribute);
        assert_kind(P, text, "\"/x\"", K::String);
        assert_kind(P, text, "@", K::Operator);
    }

    #[test]
    fn string_prefixes_and_quotes() {
        let text = r#"a = r'\d' + b"x" + Rb'y' + BR"z" + u'w' + 'it\'s' + "q""#;
        for s in [r"r'\d'", "b\"x\"", "Rb'y'", "BR\"z\"", "u'w'", r"'it\'s'", "\"q\""] {
            assert_kind(P, text, s, K::String);
        }
        assert_kind(P, "x = 'open\ny = 1", "'open", K::Invalid);
        // Not a prefix: an identifier followed by a string.
        assert_kind(P, "rx'a'", "rx", K::Ident);
    }

    #[test]
    fn triple_quoted_strings_span_lines() {
        let text = "s = '''one\n\"two\" 'x'\nthree'''\nn = 1";
        assert_eq!(kind_at(P, text, 1, 0), K::String);
        assert_eq!(kind_at(P, text, 2, 0), K::String);
        assert_kind(P, text, "1", K::Number);
        assert_eq!(kind_at(P, "s = \"\"\"never closed\nx", 1, 0), K::Invalid);
        assert_kind(P, "e = '' + \"\"", "''", K::String);
    }

    #[test]
    fn f_strings_have_code_fields() {
        let text = r#"msg = f"hi {user.name!r:>{width}} {{lit}} {d["k"]}" + rf'\{x}'"#;
        let all = spans(P, text);
        assert!(all.contains(&("f\"hi ".into(), K::String)), "{all:?}");
        assert!(all.contains(&("user".into(), K::Ident)), "{all:?}");
        assert!(all.contains(&("name".into(), K::Field)), "{all:?}");
        assert!(all.contains(&(">".into(), K::String)), "{all:?}");
        assert!(all.contains(&("width".into(), K::Ident)), "{all:?}");
        assert!(all.contains(&(" {{lit}} ".into(), K::String)), "{all:?}");
        assert!(all.contains(&("\"k\"".into(), K::String)), "{all:?}");
        assert!(all.contains(&("rf'\\".into(), K::String)), "{all:?}");
        assert!(all.contains(&("x".into(), K::Ident)), "{all:?}");
        // Brackets of fields match; braces in the literal part do not count.
        let t = "f'{a[1]} {{'";
        assert_eq!(matching_bracket(P, t, offset_of(t, "{", 0)), Some(offset_of(t, "}", 0)));
        assert_eq!(matching_bracket(P, t, offset_of(t, "{{", 0) + 1), None);
        // Unterminated: the line ends the string, the next line is code.
        assert_eq!(kind_at(P, "x = f'{a\ny = 1", 1, 0), K::Ident);
        assert_eq!(kind_at(P, "x = f'abc\ny = 1", 1, 0), K::Ident);
        // Triple-quoted f-strings span lines.
        assert_eq!(kind_at(P, "f\"\"\"a\n{b}\nc\"\"\"", 2, 0), K::String);
        assert_eq!(kind_at(P, "f\"\"\"a\n{b}\nc\"\"\"", 1, 1), K::Ident);
        assert_kind(P, "f'\\N{DASH} {x}'", "f'\\N{DASH} ", K::String);
    }

    #[test]
    fn docstrings() {
        let text = "\"\"\"Module doc.\"\"\"\nimport os\n\nclass A:\n    '''Class doc.'''\n    def f(self, x: int = 1) -> str:\n        \"\"\"Method\n        doc.\"\"\"\n        s = \"\"\"not a doc\"\"\"\n        \"\"\"also not\"\"\"\n";
        assert_eq!(kind_at(P, text, 0, 0), K::DocComment);
        assert_eq!(kind_at(P, text, 4, 4), K::DocComment);
        assert_eq!(kind_at(P, text, 6, 8), K::DocComment);
        assert_eq!(kind_at(P, text, 7, 0), K::DocComment);
        assert_eq!(kind_at(P, text, 8, 12), K::String);
        assert_eq!(kind_at(P, text, 9, 8), K::String);
        assert_kind(P, "def f(): '''one-liner'''", "'''one-liner'''", K::DocComment);
        assert_kind(P, "x = 1\n'''late'''", "'''late'''", K::String);
        assert_kind(P, "def f():\n    f'''{x}'''", "f'''", K::String);
    }

    #[test]
    fn numbers_and_comments() {
        let text = "x = 0x_FF + 0o17 + 0b1 + 1_000.5e-3 + 3j + .5  # note";
        for n in ["0x_FF", "0o17", "0b1", "1_000.5e-3", "3j", ".5"] {
            assert_kind(P, text, n, K::Number);
        }
        assert_kind(P, text, "# note", K::Comment);
        assert_eq!(kind_of(P, "x = '#' # c", "'#'"), Some(K::String));
    }

    #[test]
    fn non_ascii_and_crlf() {
        let h = checked(P, "s = 'ü' # ä\r\nnaïve = 1\r\n");
        let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(cols, [(0, 1, K::Ident), (2, 3, K::Operator), (4, 7, K::String), (8, 12, K::Comment)]);
        assert_kind(P, "naïve = 1", "naïve", K::Ident);
    }

    #[test]
    fn folds_blocks_strings_imports_comments() {
        let text = "\
import os
from sys import (
    argv,
)
# a
# b
def f(x):
    '''Doc
    string.'''
    if x:

        return [
            1,
        ]
    return 0

class A:
    def g(self): pass
    def h(self):
        pass
s = '''a
b'''
";
        assert_eq!(
            folding_ranges(P, text),
            vec![
                fold(0, 3, FoldKind::Imports),
                fold(1, 3, FoldKind::Block),
                fold(4, 5, FoldKind::Comment),
                fold(6, 14, FoldKind::Block),
                fold(7, 8, FoldKind::Comment),
                fold(9, 13, FoldKind::Block),
                fold(11, 13, FoldKind::Block),
                fold(16, 19, FoldKind::Block),
                fold(18, 19, FoldKind::Block),
                fold(20, 21, FoldKind::Block),
            ]
        );
    }

    #[test]
    fn multiline_headers_fold_from_the_header() {
        let text = "def f(\n    a,\n    b,\n):\n    return a\nx = 1\n";
        assert_eq!(folding_ranges(P, text), vec![fold(0, 4, FoldKind::Block), fold(0, 3, FoldKind::Block)]);
    }

    #[test]
    fn outline_nests_by_indentation() {
        let text = "\
MAX_RETRIES = 3
TIMEOUT: float = 1.5
lower = 1
VERSION == 2
class Client(Base):
    LIMIT = 1
    def __init__(self):
        def inner():
            pass
    async def fetch(self):
        pass
    @property
    def name(self): return 1
def main():
    pass
if TYPE_CHECKING:
    class Hidden: pass
";
        let items: Vec<(String, SymbolKind, usize)> =
            outline(P, text).into_iter().map(|i| (i.name, i.kind, i.depth)).collect();
        assert_eq!(
            items,
            vec![
                ("MAX_RETRIES".into(), SymbolKind::Const, 0),
                ("TIMEOUT".into(), SymbolKind::Const, 0),
                ("Client".into(), SymbolKind::Struct, 0),
                ("__init__".into(), SymbolKind::Function, 1),
                ("inner".into(), SymbolKind::Function, 2),
                ("fetch".into(), SymbolKind::Function, 1),
                ("name".into(), SymbolKind::Function, 1),
                ("main".into(), SymbolKind::Function, 0),
                ("Hidden".into(), SymbolKind::Struct, 0),
            ]
        );
        assert_eq!(
            outline(P, text)[2],
            OutlineItem { name: "Client".into(), kind: SymbolKind::Struct, line: 4, column: 6, depth: 0 }
        );
    }

    #[test]
    fn brackets_skip_strings_and_comments() {
        let text = "f(')', \"(\", # )\n  x)";
        assert_eq!(matching_bracket(P, text, 1), Some(text.chars().count() - 1));
    }

    #[test]
    fn every_function_handles_garbage() {
        for text in ["", "'", "\"\"\"", "f'{", "f'{x:", "f'{x:{", "@", "\\", "def", "class", ":", "f\"\"\"{\n", "rb'''"]
        {
            let _ = checked(P, text);
            let _ = folding_ranges(P, text);
            let _ = outline(P, text);
            let _ = matching_bracket(P, text, 0);
        }
    }
}
