//! TypeScript and JavaScript: keywords (contextual ones only in context), template literals
//! with nested `${…}` code, regex literals (told apart from division by the previous token),
//! JSX/TSX elements, decorators, private names and numbers; folds for blocks, comments, import
//! runs and multi-line JSX elements; an outline of functions, classes and their methods,
//! interfaces, type aliases, enums and namespaces.

use crate::common::{self, Bracket, Doc, Span};
use crate::lex::{self, Lexed, TokSink, Tt};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

struct Analysis<'a> {
    lexed: Lexed<'a>,
    /// Multi-line JSX elements: (offset of `<`, offset of the closing tag's `>`).
    elements: Vec<(usize, usize)>,
}

fn analyze(doc: &Doc, ts: bool) -> Analysis<'_> {
    let ascii = lex::ascii(doc, false);
    let (toks, elements) = {
        let mut lexer = Lexer {
            doc,
            ascii: &ascii,
            sink: TokSink::with_capacity(doc.len() / 4),
            ts,
            modes: Vec::new(),
            elements: Vec::new(),
            no_regex_before: 0,
            close_marks: None,
        };
        lexer.run();
        (lexer.sink.toks, lexer.elements)
    };
    let mut lexed = Lexed::new(doc, ascii, toks);
    classify(&mut lexed, ts);
    Analysis { lexed, elements }
}

pub(crate) fn spans(doc: &Doc, ts: bool) -> Vec<Span> {
    analyze(doc, ts).lexed.spans()
}

pub(crate) fn brackets(doc: &Doc, ts: bool) -> Vec<Bracket> {
    analyze(doc, ts).lexed.brackets()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Code inside `${…}` or a JSX `{…}`; a `}` with `braces == 0` ends it.
    Code { braces: usize },
    /// The literal part of a template.
    Template,
    /// Inside a JSX tag (`<name attr={…}>`); `start` is the offset of its `<`.
    Tag { closing: bool, named: bool, start: usize },
    /// Between an element's opening and closing tags.
    Children { start: usize },
}

struct Lexer<'a> {
    doc: &'a Doc,
    ascii: &'a [u8],
    sink: TokSink,
    ts: bool,
    /// Nested contexts; empty means top-level code.
    modes: Vec<Mode>,
    elements: Vec<(usize, usize)>,
    /// A regex scan failed on the line ending here: no more regex attempts before it.
    no_regex_before: usize,
    /// Offsets of every `</` and `/>`, computed on first use (JSX plausibility check).
    close_marks: Option<Vec<usize>>,
}

/// How far ahead a JSX element's closing `</` or `/>` must appear for `<` to start one.
const JSX_WINDOW: usize = 20_000;

fn is_word_start(c: char) -> bool {
    lex::is_ident_start(c) || c == '$'
}

fn is_word_char(c: char) -> bool {
    lex::is_ident_char(c) || c == '$'
}

impl Lexer<'_> {
    fn push(&mut self, start: usize, end: usize, tt: Tt, kind: K) {
        self.sink.push(start, end, tt, kind);
    }

    fn run(&mut self) {
        let n = self.doc.len();
        let mut i = 0;
        if self.doc.starts_with_at(0, "#!") {
            i = self.doc.line_end_at(0);
            self.push(0, i, Tt::LineComment, K::Comment);
        }
        while i < n {
            let depth = self.modes.len();
            let next = match self.modes.last().copied() {
                None | Some(Mode::Code { .. }) => self.code(i),
                Some(Mode::Template) => self.template(i, i),
                Some(Mode::Tag { closing, named, start }) => self.tag(i, closing, named, start),
                Some(Mode::Children { .. }) => self.children(i),
            };
            // Progress: every step consumes a char or leaves a mode.
            i = if next > i || self.modes.len() < depth { next } else { i + 1 };
        }
    }

    fn code(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let c = doc.at(i);
        match c {
            c if c.is_whitespace() => i + 1,
            '/' if doc.at(i + 1) == '/' => {
                let e = doc.line_end_at(i);
                let doc_style = doc.at(i + 2) == '/' && doc.at(i + 3) != '/';
                self.push(i, e, Tt::LineComment, if doc_style { K::DocComment } else { K::Comment });
                e
            }
            '/' if doc.at(i + 1) == '*' => {
                let (e, _) = lex::block_comment_end(doc, i, false);
                let doc_style = doc.at(i + 2) == '*' && !matches!(doc.at(i + 3), '*' | '/');
                self.push(i, e, Tt::BlockComment, if doc_style { K::DocComment } else { K::Comment });
                e
            }
            '/' => {
                if i >= self.no_regex_before && self.regex_allowed() {
                    match regex_end(doc, i) {
                        Some(e) => {
                            self.push(i, e, Tt::Lit, K::String);
                            return e;
                        }
                        None => self.no_regex_before = doc.line_end_at(i),
                    }
                }
                self.sym(i, '/');
                i + 1
            }
            '\'' | '"' => {
                let (e, ok) = lex::scan_quoted(doc, i + 1, c, true, false);
                self.push(i, e, Tt::Lit, if ok { K::String } else { K::Invalid });
                e
            }
            '`' => {
                self.modes.push(Mode::Template);
                self.template(i, i + 1)
            }
            c if c.is_ascii_digit() => self.number(i),
            '.' if lex::dot_number(doc, i) => self.number(i),
            c if is_word_start(c) => {
                let e = doc.scan_while(i, is_word_char);
                self.push(i, e, Tt::Word, K::Ident);
                e
            }
            '#' if is_word_start(doc.at(i + 1)) => {
                // Private name.
                let e = doc.scan_while(i + 1, is_word_char);
                self.push(i, e, Tt::Other, K::Field);
                e
            }
            '@' if is_word_start(doc.at(i + 1)) => {
                // Decorator: `@name.attr`, arguments stay code.
                let mut e = doc.scan_while(i + 1, is_word_char);
                while doc.at(e) == '.' && is_word_start(doc.at(e + 1)) {
                    e = doc.scan_while(e + 1, is_word_char);
                }
                self.push(i, e, Tt::Other, K::Attribute);
                e
            }
            '<' if self.jsx_start(i) => {
                self.push(i, i + 1, Tt::Other, K::Punctuation);
                self.modes.push(Mode::Tag { closing: false, named: false, start: i });
                i + 1
            }
            '{' => {
                if let Some(Mode::Code { braces }) = self.modes.last_mut() {
                    *braces += 1;
                }
                self.sym(i, '{');
                i + 1
            }
            '}' => {
                match self.modes.last_mut() {
                    Some(Mode::Code { braces: 0 }) => {
                        self.modes.pop();
                    }
                    Some(Mode::Code { braces }) => *braces -= 1,
                    _ => {}
                }
                self.sym(i, '}');
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

    fn sym(&mut self, i: usize, c: char) {
        self.push(i, i + 1, Tt::Sym(c), lex::sym_kind(c));
    }

    fn number(&mut self, i: usize) -> usize {
        let e = lex::number_end(self.doc, i, false);
        self.push(i, e, Tt::Lit, K::Number);
        e
    }

    /// The literal part of a template from `from` (its segment starts at `seg`): stops at `${`
    /// (entering code) or at the closing backtick.
    fn template(&mut self, seg: usize, from: usize) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let mut j = from;
        while j < n {
            match doc.at(j) {
                '\\' => j += 2,
                '`' => {
                    self.push(seg, j + 1, Tt::Lit, K::String);
                    self.modes.pop();
                    return j + 1;
                }
                '$' if doc.at(j + 1) == '{' => {
                    self.push(seg, j, Tt::Lit, K::String);
                    self.push(j, j + 1, Tt::Other, K::Punctuation);
                    self.sym(j + 1, '{');
                    self.modes.push(Mode::Code { braces: 0 });
                    return j + 2;
                }
                _ => j += 1,
            }
        }
        self.push(seg, n, Tt::Lit, K::Invalid);
        self.modes.pop();
        n
    }

    /// A `/` here starts a regex: the previous token cannot end an operand.
    fn regex_allowed(&self) -> bool {
        let Some(prev) = self.sink.recent(0) else {
            return true;
        };
        match prev.tt {
            Tt::Sym(')' | ']') => false,
            Tt::Sym(c @ ('+' | '-')) if self.sink.recent_sym(1) == Some(c) && self.sink.recent_adjacent(0) => {
                // `x++ / y` divides; `a + ++/re/.lastIndex` is exotic.
                !matches!(self.sink.recent(2).map(|t| t.tt), Some(Tt::Word | Tt::Lit | Tt::Sym(')' | ']')))
            }
            Tt::Sym(_) => true,
            Tt::Word => matches!(
                self.ascii.get(prev.start..prev.end).unwrap_or(&[]),
                b"return"
                    | b"typeof"
                    | b"instanceof"
                    | b"in"
                    | b"of"
                    | b"new"
                    | b"delete"
                    | b"void"
                    | b"throw"
                    | b"case"
                    | b"do"
                    | b"else"
                    | b"yield"
                    | b"await"
            ),
            _ => false,
        }
    }

    /// A `<` at `i` opens a JSX element: it is in expression position, followed by a tag name
    /// or `>`, is not a TypeScript generic (`<T,>`, `<T extends U>`, `<T>(…) =>`), and a
    /// closing `</` or `/>` follows within [`JSX_WINDOW`].
    fn jsx_start(&mut self, i: usize) -> bool {
        let doc = self.doc;
        let allowed = match self.sink.recent(0) {
            None => true,
            Some(prev) => match prev.tt {
                Tt::Sym('>') => self.sink.recent_sym(1) == Some('=') && self.sink.recent_adjacent(0),
                Tt::Sym(c) => matches!(c, '(' | ',' | '=' | ':' | '[' | '!' | '&' | '|' | '?' | '{' | '}' | ';'),
                Tt::Word => matches!(
                    self.ascii.get(prev.start..prev.end).unwrap_or(&[]),
                    b"return" | b"yield" | b"await" | b"default" | b"case"
                ),
                _ => false,
            },
        };
        if !allowed {
            return false;
        }
        let next = doc.at(i + 1);
        if next != '>' {
            if !is_word_start(next) {
                return false;
            }
            let name_end = doc.scan_while(i + 1, |c| is_word_char(c) || matches!(c, '.' | '-' | ':'));
            let after = doc.scan_while(name_end, char::is_whitespace);
            if self.ts {
                let generic = doc.at(after) == ','
                    || (doc.starts_with_at(after, "extends") && !is_word_char(doc.at(after + 7)))
                    || (doc.at(after) == '>' && doc.at(doc.scan_while(after + 1, char::is_whitespace)) == '(');
                if generic {
                    return false;
                }
            }
        }
        self.close_mark_within(i)
    }

    fn close_mark_within(&mut self, i: usize) -> bool {
        let doc = self.doc;
        let marks = self.close_marks.get_or_insert_with(|| {
            (0..doc.len())
                .filter(|&j| (doc.at(j) == '<' && doc.at(j + 1) == '/') || (doc.at(j) == '/' && doc.at(j + 1) == '>'))
                .collect()
        });
        let at = marks.partition_point(|&m| m <= i);
        marks.get(at).is_some_and(|&m| m - i <= JSX_WINDOW)
    }

    /// One step inside a JSX tag.
    fn tag(&mut self, i: usize, closing: bool, named: bool, start: usize) -> usize {
        let doc = self.doc;
        let c = doc.at(i);
        match c {
            c if c.is_whitespace() => i + 1,
            c if is_word_start(c) => {
                let e = doc.scan_while(i, |c| is_word_char(c) || matches!(c, '-' | ':' | '.'));
                if named {
                    self.push(i, e, Tt::Other, K::Field);
                } else {
                    self.push(i, e, Tt::Other, K::Type);
                    if let Some(Mode::Tag { named, .. }) = self.modes.last_mut() {
                        *named = true;
                    }
                }
                e
            }
            '=' => {
                self.sym(i, '=');
                i + 1
            }
            '"' | '\'' => {
                let (e, ok) = lex::scan_quoted(doc, i + 1, c, false, false);
                self.push(i, e, Tt::Lit, if ok { K::String } else { K::Invalid });
                e
            }
            '{' => {
                self.sym(i, '{');
                self.modes.push(Mode::Code { braces: 0 });
                i + 1
            }
            '/' if doc.at(i + 1) == '/' || doc.at(i + 1) == '*' => self.code(i),
            '/' if doc.at(i + 1) == '>' => {
                self.push(i, i + 2, Tt::Other, K::Punctuation);
                self.modes.pop();
                if closing {
                    self.close_children(i + 1);
                }
                i + 2
            }
            '>' => {
                self.push(i, i + 1, Tt::Other, K::Punctuation);
                self.modes.pop();
                if closing {
                    self.close_children(i);
                } else {
                    self.modes.push(Mode::Children { start });
                }
                i + 1
            }
            _ => {
                // Not a tag after all (or broken while typing): back to code.
                self.modes.pop();
                i
            }
        }
    }

    /// A closing tag ended at `end`: leaves the element's children.
    fn close_children(&mut self, end: usize) {
        if let Some(Mode::Children { start }) = self.modes.last().copied() {
            self.modes.pop();
            self.elements.push((start, end));
        }
    }

    /// Text between tags (left plain) up to a nested tag, closing tag or `{` expression.
    fn children(&mut self, from: usize) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let mut j = from;
        while j < n {
            match doc.at(j) {
                '{' => {
                    self.sym(j, '{');
                    self.modes.push(Mode::Code { braces: 0 });
                    return j + 1;
                }
                '<' if doc.at(j + 1) == '/' => {
                    self.push(j, j + 2, Tt::Other, K::Punctuation);
                    self.modes.push(Mode::Tag { closing: true, named: false, start: j });
                    return j + 2;
                }
                '<' if doc.at(j + 1) == '>' || is_word_start(doc.at(j + 1)) => {
                    self.push(j, j + 1, Tt::Other, K::Punctuation);
                    self.modes.push(Mode::Tag { closing: false, named: false, start: j });
                    return j + 1;
                }
                _ => j += 1,
            }
        }
        n
    }
}

/// End of a regex literal starting with the `/` at `i` (past its flags), or `None` when the
/// line ends first.
fn regex_end(doc: &Doc, i: usize) -> Option<usize> {
    let n = doc.len();
    let mut j = i + 1;
    let mut class = false;
    while j < n {
        match doc.at(j) {
            '\n' => return None,
            '\\' => {
                if doc.at(j + 1) == '\n' {
                    return None;
                }
                j += 2;
                continue;
            }
            '[' => class = true,
            ']' => class = false,
            '/' if !class => return Some(doc.scan_while(j + 1, is_word_char)),
            _ => {}
        }
        j += 1;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Classification

fn is_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"break"
            | b"case"
            | b"catch"
            | b"class"
            | b"const"
            | b"continue"
            | b"debugger"
            | b"default"
            | b"delete"
            | b"do"
            | b"else"
            | b"export"
            | b"extends"
            | b"finally"
            | b"for"
            | b"function"
            | b"if"
            | b"import"
            | b"in"
            | b"instanceof"
            | b"new"
            | b"return"
            | b"switch"
            | b"throw"
            | b"try"
            | b"typeof"
            | b"var"
            | b"void"
            | b"while"
            | b"with"
            | b"yield"
            | b"let"
            | b"static"
            | b"await"
            | b"enum"
            | b"implements"
            | b"interface"
            | b"package"
            | b"private"
            | b"protected"
            | b"public"
    )
}

/// Words that are keywords only where followed by what they introduce (`as T`, `get x()`).
fn is_contextual(w: &[u8], ts: bool) -> bool {
    matches!(w, b"as" | b"async" | b"from" | b"get" | b"set" | b"of")
        || (ts
            && matches!(
                w,
                b"type"
                    | b"declare"
                    | b"namespace"
                    | b"module"
                    | b"abstract"
                    | b"readonly"
                    | b"keyof"
                    | b"infer"
                    | b"is"
                    | b"asserts"
                    | b"satisfies"
                    | b"override"
                    | b"accessor"
                    | b"unique"
                    | b"global"
                    | b"out"
            ))
}

fn is_ts_type(w: &[u8]) -> bool {
    matches!(w, b"string" | b"number" | b"boolean" | b"any" | b"unknown" | b"never" | b"object" | b"symbol" | b"bigint")
}

fn classify(lx: &mut Lexed<'_>, ts: bool) {
    let ends = lx.group_ends();
    let kinds: Vec<(usize, K)> =
        (0..lx.len()).filter(|&p| lx.is_word_tok(p)).map(|p| (p, word_kind(lx, &ends, p, ts))).collect();
    for (p, kind) in kinds {
        lx.set(p, kind);
    }
}

/// The token at `q` is a template literal touching the word before it (a tagged template).
fn is_tagged_template(lx: &Lexed<'_>, q: usize) -> bool {
    lx.tt(q) == Some(Tt::Lit) && lx.chars(q).first() == Some(&'`') && q.checked_sub(1).is_some_and(|w| lx.adjacent(w))
}

fn word_kind(lx: &Lexed<'_>, ends: &[Option<usize>], p: usize, ts: bool) -> K {
    let w = lx.word(p);
    let call = lx.is_call(p + 1, ts) || is_tagged_template(lx, p + 1);
    if lx.after_member_access(p, false) {
        return if call { K::Function } else { K::Field };
    }
    match w {
        b"true" | b"false" | b"null" | b"undefined" => return K::Boolean,
        b"this" | b"super" => return K::SelfKeyword,
        _ => {}
    }
    if is_keyword(w) {
        return K::Keyword;
    }
    if is_contextual(w, ts) {
        let introduces = match lx.tt(p + 1) {
            Some(Tt::Word | Tt::Other) => true,
            Some(Tt::Lit) => !matches!(lx.chars(p + 1).first(), Some(c) if c.is_ascii_digit()),
            Some(Tt::Sym('{' | '[' | '*')) => true,
            Some(Tt::Sym('(')) => matches!(w, b"async" | b"keyof"),
            _ => false,
        };
        if introduces {
            return K::Keyword;
        }
    }
    if ts && is_ts_type(w) && !lx.is_sym(p + 1, ':') && !call {
        return K::Type;
    }
    match p.checked_sub(1).map(|q| lx.word(q)) {
        Some(b"function") => return K::Function,
        Some(b"class" | b"interface" | b"enum") => return K::Type,
        Some(b"type") if ts && matches!(lx.sym(p + 1), Some('=' | '<')) => return K::Type,
        _ => {}
    }
    let chars = lx.chars(p);
    if lex::is_screaming(chars) {
        return K::Constant;
    }
    if lex::is_capitalized(chars) {
        return K::Type;
    }
    if call || function_init(lx, ends, p + 1) {
        return K::Function;
    }
    K::Ident
}

/// At `q` (just after a declared name): an optional `: Type` then `= function`, `= async …`,
/// `= (…) =>`, `= x =>` or `= <T>(…) =>`.
fn function_init(lx: &Lexed<'_>, ends: &[Option<usize>], q: usize) -> bool {
    const MAX_ANNOTATION: usize = 64;
    let mut r = q;
    if lx.is_sym(r, ':') {
        let limit = lx.len().min(r + MAX_ANNOTATION);
        while r < limit && !(lx.is_sym(r, '=') && !lx.is_pair(r, '=', '>')) {
            match lx.sym(r) {
                Some('(' | '[' | '{') => r = ends.get(r).copied().flatten().map_or(limit, |e| e + 1),
                Some(';' | ')' | ']' | '}' | ',') => return false,
                _ => r += 1,
            }
        }
    }
    if !lx.is_sym(r, '=') || lx.is_pair(r, '=', '=') || lx.is_pair(r, '=', '>') {
        return false;
    }
    r += 1;
    if lx.is_word(r, "async") {
        r += 1;
    }
    if lx.is_word(r, "function") {
        return true;
    }
    if lx.is_word_tok(r) {
        return lx.is_pair(r + 1, '=', '>');
    }
    if lx.is_sym(r, '<') {
        // Generic arrow: skip `<…>`.
        let limit = lx.len().min(r + MAX_ANNOTATION);
        let mut depth = 0usize;
        while r < limit {
            match lx.sym(r) {
                Some('<') => depth += 1,
                Some('>') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        r += 1;
                        break;
                    }
                }
                _ => {}
            }
            r += 1;
        }
    }
    if !lx.is_sym(r, '(') {
        return false;
    }
    let Some(close) = ends.get(r).copied().flatten() else {
        return false;
    };
    let mut s = close + 1;
    if lx.is_sym(s, ':') {
        // Return type annotation.
        let limit = lx.len().min(s + MAX_ANNOTATION);
        while s < limit && !lx.is_pair(s, '=', '>') {
            match lx.sym(s) {
                Some('(' | '[' | '{') => s = ends.get(s).copied().flatten().map_or(limit, |e| e + 1),
                Some(';' | ')' | ']' | '}' | ',') => return false,
                _ => s += 1,
            }
        }
    }
    lx.is_pair(s, '=', '>')
}

// ---------------------------------------------------------------------------------------------
// Folding

pub(crate) fn folding_ranges(doc: &Doc, ts: bool) -> Vec<FoldRange> {
    let Analysis { lexed: lx, elements } = analyze(doc, ts);
    let mut out = Vec::new();
    lx.bracket_folds(&mut out);
    lx.comment_folds(&mut out);
    for (start, end) in elements {
        common::push_fold(&mut out, doc.index.line_of(start), doc.index.line_of(end), FoldKind::Block);
    }
    import_folds(&lx, &mut out);
    common::finish_folds(out)
}

/// Runs of `import …` statements and `const x = require(…)` declarations.
fn import_folds(lx: &Lexed<'_>, out: &mut Vec<FoldRange>) {
    let ends = lx.group_ends();
    let mut stmts: Vec<(bool, usize, usize)> = Vec::new();
    let mut in_code = false;
    let mut p = 0;
    while p < lx.len() {
        if let Some(end) = import_stmt(lx, &ends, p) {
            stmts.push((true, lx.line(p), lx.end_line(end)));
            in_code = false;
            p = end + 1;
            continue;
        }
        if !in_code {
            stmts.push((false, lx.line(p), lx.line(p)));
            in_code = true;
        }
        p += 1;
    }
    lex::statement_runs(&stmts, FoldKind::Imports, out);
}

/// An import statement starting at `p`: the position of its last token.
fn import_stmt(lx: &Lexed<'_>, ends: &[Option<usize>], p: usize) -> Option<usize> {
    const MAX_TOKENS: usize = 512;
    let statement_start = p.checked_sub(1).is_none_or(|q| matches!(lx.sym(q), Some(';' | '}')) || lx.newline_before(p));
    if !statement_start {
        return None;
    }
    let semi = |q: usize| if lx.is_sym(q + 1, ';') { q + 1 } else { q };
    if lx.is_word(p, "import") && !matches!(lx.sym(p + 1), Some('(' | '.')) {
        let limit = lx.len().min(p + MAX_TOKENS);
        let mut q = p + 1;
        while q < limit {
            match lx.tt(q) {
                Some(Tt::Sym(';')) => return Some(q),
                Some(Tt::Sym('{')) => q = ends.get(q).copied().flatten()? + 1,
                // `import "x"` or `… from "x"`.
                Some(Tt::Lit) if q == p + 1 || lx.is_word(q - 1, "from") => return Some(semi(q)),
                _ => q += 1,
            }
        }
        return None;
    }
    // `const x = require("y")`, `const { a } = require("y")`.
    if matches!(lx.word(p), b"const" | b"let" | b"var") {
        let mut q = p + 1;
        if lx.is_sym(q, '{') || lx.is_sym(q, '[') {
            q = ends.get(q).copied().flatten()? + 1;
        } else if lx.is_word_tok(q) {
            q += 1;
        } else {
            return None;
        }
        if lx.is_sym(q, '=') && lx.is_word(q + 1, "require") && lx.is_sym(q + 2, '(') {
            let mut end = ends.get(q + 2).copied().flatten()?;
            while lx.is_sym(end + 1, '.') && lx.is_word_tok(end + 2) {
                end += 2;
            }
            return Some(semi(end));
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Outline

pub(crate) fn outline(doc: &Doc, ts: bool) -> Vec<OutlineItem> {
    let Analysis { lexed, .. } = analyze(doc, ts);
    let ends = lexed.group_ends();
    Outliner { lx: &lexed, ends, items: Vec::new(), frames: Vec::new(), depth: 0 }.run()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frame {
    /// A class body: members are methods and properties.
    Class { deeper: bool },
    /// A namespace or module body: statements.
    Namespace { deeper: bool },
}

/// Walks statements at file and namespace level and members of class bodies; function bodies,
/// initializers and every other group are skipped whole.
struct Outliner<'a, 'b> {
    lx: &'b Lexed<'a>,
    ends: Vec<Option<usize>>,
    items: Vec<OutlineItem>,
    frames: Vec<Frame>,
    depth: usize,
}

impl Outliner<'_, '_> {
    fn run(mut self) -> Vec<OutlineItem> {
        let mut p = 0;
        while p < self.lx.len() {
            let next = match self.lx.sym(p) {
                Some('}') => {
                    if let Some(Frame::Class { deeper: true } | Frame::Namespace { deeper: true }) = self.frames.pop() {
                        self.depth = self.depth.saturating_sub(1);
                    }
                    p + 1
                }
                Some('{' | '(' | '[') => self.skip_group(p),
                Some(_) => p + 1,
                None if matches!(self.frames.last(), Some(Frame::Class { .. })) => self.member(p),
                None => self.statement(p),
            };
            p = next.max(p + 1);
        }
        self.items
    }

    fn skip_group(&self, p: usize) -> usize {
        self.ends.get(p).copied().flatten().map_or(self.lx.len(), |e| e + 1)
    }

    fn push(&mut self, p: usize, kind: SymbolKind) {
        if let Some(item) = self.lx.item(p, self.lx.text(p), kind, self.depth) {
            self.items.push(item);
        }
    }

    fn enter(&mut self, brace: usize, frame: Frame) -> usize {
        if let Frame::Class { deeper: true } | Frame::Namespace { deeper: true } = frame {
            self.depth += 1;
        }
        self.frames.push(frame);
        brace + 1
    }

    /// Skips decorators (with their arguments) and the given modifier words.
    fn skip_modifiers(&self, p: usize, modifiers: &[&str]) -> usize {
        let lx = self.lx;
        let mut q = p;
        loop {
            if lx.kind(q) == Some(K::Attribute) && lx.tt(q) == Some(Tt::Other) {
                q += 1;
                if lx.is_sym(q, '(') {
                    q = self.skip_group(q);
                }
            } else if lx.is_word_tok(q)
                && modifiers.iter().any(|m| lx.is_word(q, m))
                && !matches!(lx.sym(q + 1), Some('(' | '=' | ':' | ';' | '?' | '<' | ','))
            {
                q += 1;
            } else {
                return q;
            }
        }
    }

    /// Ends a statement: `;`, a stray `}`, or (automatic semicolon insertion) a new line that
    /// cannot continue the previous token.
    fn skip_statement(&self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = p;
        while q < lx.len() {
            if q > p && lx.newline_before(q) && ends_operand(lx, q - 1) && !continues(lx, q) {
                return q;
            }
            match lx.sym(q) {
                Some('{' | '(' | '[') => q = self.skip_group(q),
                Some(';') => return q + 1,
                Some('}') => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// Skips a function's parameters, return type and body (or the `;` of a signature).
    fn skip_function(&self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = p;
        let mut params = false;
        while q < lx.len() {
            match lx.sym(q) {
                Some('(') => {
                    q = self.skip_group(q);
                    params = true;
                }
                Some('{') if params && !lx.is_sym(q - 1, ':') => return self.skip_group(q),
                Some('{' | '[') => q = self.skip_group(q),
                Some(';') => return q + 1,
                Some('}') => return q,
                _ if params && lx.newline_before(q) && ends_operand(lx, q - 1) && !continues(lx, q) => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// Past a header up to and including its `{…}` body (for interfaces, enums).
    fn skip_body(&self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = p;
        while q < lx.len() {
            match lx.sym(q) {
                Some('{') => return self.skip_group(q),
                Some('(' | '[') => q = self.skip_group(q),
                Some(';') => return q + 1,
                Some('}') => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// The `{` opening a class or namespace body after a header at `p`, if any.
    fn body_brace(&self, p: usize) -> Option<usize> {
        let lx = self.lx;
        let mut q = p;
        while q < lx.len() {
            match lx.sym(q) {
                Some('{') => return Some(q),
                Some('(' | '[') => q = self.skip_group(q),
                Some(';' | '}') => return None,
                _ => q += 1,
            }
        }
        None
    }

    fn statement(&mut self, p: usize) -> usize {
        let lx = self.lx;
        let q = self.skip_modifiers(p, &["export", "default", "declare", "abstract"]);
        let q = if lx.is_word(q, "async") && lx.is_word(q + 1, "function") { q + 1 } else { q };
        match lx.word(q) {
            b"function" => {
                let name = if lx.is_sym(q + 1, '*') { q + 2 } else { q + 1 };
                if lx.is_word_tok(name) {
                    self.push(name, SymbolKind::Function);
                }
                self.skip_function(name)
            }
            b"class" => self.class(q),
            b"interface" if lx.is_word_tok(q + 1) => {
                self.push(q + 1, SymbolKind::Trait);
                self.skip_body(q + 2)
            }
            b"type" if lx.is_word_tok(q + 1) && matches!(lx.sym(q + 2), Some('=' | '<')) => {
                self.push(q + 1, SymbolKind::TypeAlias);
                self.skip_statement(q + 2)
            }
            b"enum" if lx.is_word_tok(q + 1) => {
                self.push(q + 1, SymbolKind::Enum);
                self.skip_body(q + 2)
            }
            b"const" if lx.is_word(q + 1, "enum") && lx.is_word_tok(q + 2) => {
                self.push(q + 2, SymbolKind::Enum);
                self.skip_body(q + 3)
            }
            b"namespace" | b"module" if matches!(lx.tt(q + 1), Some(Tt::Word | Tt::Lit)) => {
                let mut r = q + 1;
                while lx.is_sym(r + 1, '.') && lx.is_word_tok(r + 2) {
                    r += 2;
                }
                if !lx.is_sym(r + 1, '{') {
                    return self.skip_statement(q + 1);
                }
                let name: String = (q + 1..=r).flat_map(|s| lx.chars(s).iter()).collect();
                if let Some(item) = lx.item(q + 1, name, SymbolKind::Module, self.depth) {
                    self.items.push(item);
                }
                self.enter(r + 1, Frame::Namespace { deeper: true })
            }
            b"global" if lx.is_sym(q + 1, '{') => {
                self.push(q, SymbolKind::Module);
                self.enter(q + 1, Frame::Namespace { deeper: true })
            }
            b"const" | b"let" | b"var" => {
                if lx.is_word_tok(q + 1) && function_init(lx, &self.ends, q + 2) {
                    self.push(q + 1, SymbolKind::Function);
                }
                self.skip_statement(q + 1)
            }
            _ => self.skip_statement(q),
        }
    }

    fn class(&mut self, q: usize) -> usize {
        let lx = self.lx;
        let named = lx.is_word_tok(q + 1) && !lx.is_word(q + 1, "extends") && !lx.is_word(q + 1, "implements");
        if named {
            self.push(q + 1, SymbolKind::Struct);
        }
        match self.body_brace(q + 1) {
            Some(brace) => self.enter(brace, Frame::Class { deeper: named }),
            None => self.skip_statement(q + 1),
        }
    }

    /// One class member at `p`.
    fn member(&mut self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = self.skip_modifiers(
            p,
            &[
                "public",
                "private",
                "protected",
                "static",
                "readonly",
                "abstract",
                "override",
                "declare",
                "async",
                "accessor",
                "get",
                "set",
            ],
        );
        if lx.is_word(q, "static") && lx.is_sym(q + 1, '{') {
            return self.skip_group(q + 1);
        }
        if lx.is_sym(q, '*') {
            q += 1;
        }
        let name = q;
        let named = match lx.tt(q) {
            Some(Tt::Word | Tt::Other | Tt::Lit) => {
                q += 1;
                true
            }
            Some(Tt::Sym('[')) => {
                q = self.skip_group(q);
                false
            }
            _ => return self.skip_statement(q),
        };
        if lx.is_sym(q, '?') || lx.is_sym(q, '!') {
            q += 1;
        }
        if lx.is_sym(q, '(') || lx.is_sym(q, '<') {
            if named {
                self.push(name, SymbolKind::Function);
            }
            return self.skip_function(q);
        }
        if named && function_init(lx, &self.ends, q) {
            self.push(name, SymbolKind::Function);
        }
        self.skip_statement(q)
    }
}

/// The token at `q` can end an expression (so a following newline may end the statement).
fn ends_operand(lx: &Lexed<'_>, q: usize) -> bool {
    match lx.tt(q) {
        Some(Tt::Word) => !matches!(
            lx.word(q),
            b"return" | b"typeof" | b"new" | b"in" | b"of" | b"instanceof" | b"extends" | b"implements" | b"as"
        ),
        Some(Tt::Lit | Tt::Other | Tt::Sym(')' | ']' | '}')) => true,
        Some(Tt::Sym('+' | '-')) => q.checked_sub(1).is_some_and(|r| lx.sym(r) == lx.sym(q) && lx.adjacent(r)),
        _ => false,
    }
}

/// The token at `q` continues the previous line's expression.
fn continues(lx: &Lexed<'_>, q: usize) -> bool {
    match lx.tt(q) {
        Some(Tt::Sym(c)) => {
            matches!(
                c,
                '.' | ',' | '?' | ':' | '=' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '<' | '>' | '(' | '['
            )
        }
        Some(Tt::Word) => {
            matches!(lx.word(q), b"in" | b"instanceof" | b"as" | b"satisfies" | b"extends" | b"implements")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
