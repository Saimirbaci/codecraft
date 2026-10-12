//! Zig: keywords, primitive types (including arbitrary-width `u7`/`i129`), builtins (`@import`),
//! strings, `\\` multiline strings, chars, numbers and `///`/`//!` doc comments; folds for
//! blocks, comments, multiline strings and import runs; an outline of functions, containers and
//! tests.

use crate::common::{self, Bracket, Doc, Span};
use crate::lex::{self, Lexed, TokSink, Tt};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

fn lex(doc: &Doc) -> Lexed<'_> {
    let ascii = lex::ascii(doc, false);
    let mut sink = TokSink::with_capacity(doc.len() / 4);
    tokenize(doc, &mut sink);
    let mut lexed = Lexed::new(doc, ascii, sink.toks);
    classify(&mut lexed);
    lexed
}

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    lex(doc).spans()
}

pub(crate) fn brackets(doc: &Doc) -> Vec<Bracket> {
    lex(doc).brackets()
}

fn tokenize(doc: &Doc, sink: &mut TokSink) {
    let n = doc.len();
    let mut i = 0;
    while i < n {
        let c = doc.at(i);
        let next = match c {
            c if c.is_whitespace() => i + 1,
            '/' if doc.at(i + 1) == '/' => {
                let e = doc.line_end_at(i);
                let doc_style = (doc.at(i + 2) == '/' && doc.at(i + 3) != '/') || doc.at(i + 2) == '!';
                sink.push(i, e, Tt::LineComment, if doc_style { K::DocComment } else { K::Comment });
                e
            }
            '\\' if doc.at(i + 1) == '\\' => {
                // One line of a multiline string literal.
                let e = doc.line_end_at(i);
                sink.push(i, e, Tt::Lit, K::String);
                e
            }
            '"' => {
                let (e, ok) = lex::scan_quoted(doc, i + 1, '"', true, false);
                sink.push(i, e, Tt::Lit, if ok { K::String } else { K::Invalid });
                e
            }
            '\'' => {
                let (e, ok) = lex::scan_quoted(doc, i + 1, '\'', true, false);
                sink.push(i, e, Tt::Lit, if ok { K::Char } else { K::Invalid });
                e
            }
            '@' if doc.at(i + 1) == '"' => {
                // `@"any identifier"`.
                let (e, ok) = lex::scan_quoted(doc, i + 2, '"', true, false);
                sink.push(i, e, Tt::Other, if ok { K::Ident } else { K::Invalid });
                e
            }
            '@' if lex::is_ident_start(doc.at(i + 1)) => {
                let e = doc.scan_while(i + 1, lex::is_ident_char);
                sink.push(i, e, Tt::Other, K::Macro);
                e
            }
            c if c.is_ascii_digit() => {
                let e = lex::number_end(doc, i, false);
                sink.push(i, e, Tt::Lit, K::Number);
                e
            }
            c if lex::is_ident_start(c) => {
                let e = doc.scan_while(i, lex::is_ident_char);
                sink.push(i, e, Tt::Word, K::Ident);
                e
            }
            c if c.is_ascii_punctuation() => {
                sink.sym(i, c);
                i + 1
            }
            _ => {
                sink.push(i, i + 1, Tt::Other, K::Invalid);
                i + 1
            }
        };
        i = next.max(i + 1);
    }
}

fn is_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"addrspace"
            | b"align"
            | b"allowzero"
            | b"and"
            | b"anyframe"
            | b"anytype"
            | b"asm"
            | b"async"
            | b"await"
            | b"break"
            | b"callconv"
            | b"catch"
            | b"comptime"
            | b"const"
            | b"continue"
            | b"defer"
            | b"else"
            | b"enum"
            | b"errdefer"
            | b"error"
            | b"export"
            | b"extern"
            | b"fn"
            | b"for"
            | b"if"
            | b"inline"
            | b"linksection"
            | b"noalias"
            | b"noinline"
            | b"nosuspend"
            | b"opaque"
            | b"or"
            | b"orelse"
            | b"packed"
            | b"pub"
            | b"resume"
            | b"return"
            | b"struct"
            | b"suspend"
            | b"switch"
            | b"test"
            | b"threadlocal"
            | b"try"
            | b"union"
            | b"unreachable"
            | b"usingnamespace"
            | b"var"
            | b"volatile"
            | b"while"
    )
}

fn is_primitive(w: &[u8]) -> bool {
    if let [b'u' | b'i', digits @ ..] = w
        && !digits.is_empty()
        && digits.len() <= 5
        && digits.iter().all(u8::is_ascii_digit)
    {
        return true;
    }
    matches!(
        w,
        b"isize"
            | b"usize"
            | b"c_char"
            | b"c_short"
            | b"c_ushort"
            | b"c_int"
            | b"c_uint"
            | b"c_long"
            | b"c_ulong"
            | b"c_longlong"
            | b"c_ulonglong"
            | b"c_longdouble"
            | b"f16"
            | b"f32"
            | b"f64"
            | b"f80"
            | b"f128"
            | b"bool"
            | b"anyopaque"
            | b"void"
            | b"noreturn"
            | b"type"
            | b"anyerror"
            | b"comptime_int"
            | b"comptime_float"
    )
}

fn is_container(w: &[u8]) -> bool {
    matches!(w, b"struct" | b"enum" | b"union" | b"opaque")
}

fn classify(lx: &mut Lexed<'_>) {
    let kinds: Vec<(usize, K)> = (0..lx.len()).filter(|&p| lx.is_word_tok(p)).map(|p| (p, word_kind(lx, p))).collect();
    for (p, kind) in kinds {
        lx.set(p, kind);
    }
}

fn word_kind(lx: &Lexed<'_>, p: usize) -> K {
    let w = lx.word(p);
    if lx.after_member_access(p, false) {
        return if lx.is_call(p + 1, false) { K::Function } else { K::Field };
    }
    if matches!(w, b"true" | b"false" | b"null" | b"undefined") {
        return K::Boolean;
    }
    if is_keyword(w) {
        return K::Keyword;
    }
    if is_primitive(w) {
        return K::Type;
    }
    let prev = p.checked_sub(1).map(|q| lx.word(q));
    if prev == Some(b"fn") {
        return K::Function;
    }
    // `const name = struct {`: a type.
    if matches!(prev, Some(b"const" | b"var")) && lx.is_sym(p + 1, '=') {
        let q = if matches!(lx.word(p + 2), b"extern" | b"packed") { p + 3 } else { p + 2 };
        if is_container(lx.word(q)) {
            return K::Type;
        }
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
// Folding

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let lx = lex(doc);
    let mut out = Vec::new();
    lx.bracket_folds(&mut out);
    lx.comment_folds(&mut out);
    // Runs of `\\` multiline-string lines.
    let mut lines = lex::LineCursor::new(&doc.index);
    let mut run: Option<(usize, usize)> = None;
    for t in &lx.toks {
        if t.tt == Tt::Lit && doc.at(t.start) == '\\' {
            let line = lines.line_of(t.start);
            run = match run {
                Some((start, end)) if end + 1 == line => Some((start, line)),
                other => {
                    if let Some((start, end)) = other {
                        common::push_fold(&mut out, start, end, FoldKind::Block);
                    }
                    Some((line, line))
                }
            };
        }
    }
    if let Some((start, end)) = run {
        common::push_fold(&mut out, start, end, FoldKind::Block);
    }
    import_folds(&lx, &mut out);
    common::finish_folds(out)
}

/// Runs of top-level `const x = @import("…");` and `const y = x.path;` declarations.
fn import_folds(lx: &Lexed<'_>, out: &mut Vec<FoldRange>) {
    let mut stmts: Vec<(bool, usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut at_start = true;
    let mut p = 0;
    while p < lx.len() {
        if depth == 0 && at_start {
            at_start = false;
            if let Some(end) = import_decl(lx, p) {
                stmts.push((true, lx.line(p), lx.line(end)));
                p = end + 1;
                at_start = true;
                continue;
            }
            stmts.push((false, lx.line(p), lx.line(p)));
        }
        match lx.sym(p) {
            Some('(' | '[' | '{') => depth += 1,
            Some(')' | ']') => depth = depth.saturating_sub(1),
            Some('}') => {
                depth = depth.saturating_sub(1);
                at_start = depth == 0;
            }
            Some(';') => at_start = depth == 0,
            _ => {}
        }
        p += 1;
    }
    lex::statement_runs(&stmts, FoldKind::Imports, out);
}

/// `[pub] const name = @import("x")[.a.b];` or `[pub] const name = a.b.c;` at `p`: the `;`.
fn import_decl(lx: &Lexed<'_>, p: usize) -> Option<usize> {
    let mut q = if lx.is_word(p, "pub") { p + 1 } else { p };
    if !(lx.is_word(q, "const") && lx.is_word_tok(q + 1) && lx.is_sym(q + 2, '=')) {
        return None;
    }
    q += 3;
    if lx.tt(q) == Some(Tt::Other) && lx.chars(q) == ['@', 'i', 'm', 'p', 'o', 'r', 't'] {
        if !(lx.is_sym(q + 1, '(') && lx.tt(q + 2) == Some(Tt::Lit) && lx.is_sym(q + 3, ')')) {
            return None;
        }
        q += 4;
    } else if lx.is_word_tok(q) && lx.is_sym(q + 1, '.') {
        q += 1;
    } else {
        return None;
    }
    while lx.is_sym(q, '.') && lx.is_word_tok(q + 1) {
        q += 2;
    }
    lx.is_sym(q, ';').then_some(q)
}

// ---------------------------------------------------------------------------------------------
// Outline

pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    let lx = lex(doc);
    let ends = lx.group_ends();
    Outliner { lx: &lx, ends, items: Vec::new(), frames: Vec::new(), depth: 0 }.run()
}

/// Walks container-level declarations; fn bodies, test bodies and initializers are skipped.
struct Outliner<'a, 'b> {
    lx: &'b Lexed<'a>,
    ends: Vec<Option<usize>>,
    items: Vec<OutlineItem>,
    frames: Vec<bool>,
    depth: usize,
}

impl Outliner<'_, '_> {
    fn run(mut self) -> Vec<OutlineItem> {
        let mut p = 0;
        while p < self.lx.len() {
            let next = match self.lx.sym(p) {
                Some('}') => {
                    if self.frames.pop() == Some(true) {
                        self.depth = self.depth.saturating_sub(1);
                    }
                    p + 1
                }
                Some('{' | '(' | '[') => self.skip_group(p),
                Some(_) => p + 1,
                None => self.declaration(p),
            };
            p = next.max(p + 1);
        }
        self.items
    }

    fn skip_group(&self, p: usize) -> usize {
        self.ends.get(p).copied().flatten().map_or(self.lx.len(), |e| e + 1)
    }

    fn push(&mut self, p: usize, name: String, kind: SymbolKind) {
        if let Some(item) = self.lx.item(p, name, kind, self.depth) {
            self.items.push(item);
        }
    }

    /// Past the `;` or `,` ending a declaration or field (groups skipped); stops before `}`.
    fn skip_statement(&self, p: usize) -> usize {
        let mut q = p;
        while q < self.lx.len() {
            match self.lx.sym(q) {
                Some('{' | '(' | '[') => q = self.skip_group(q),
                Some(';' | ',') => return q + 1,
                Some('}') => return q,
                _ => q += 1,
            }
        }
        q
    }

    fn declaration(&mut self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = p;
        loop {
            match lx.word(q) {
                b"pub" | b"export" | b"inline" | b"noinline" | b"threadlocal" => q += 1,
                b"extern" => q += if lx.tt(q + 1) == Some(Tt::Lit) { 2 } else { 1 },
                _ => break,
            }
        }
        match lx.word(q) {
            b"fn" => {
                if lx.is_word_tok(q + 1) {
                    self.push(q + 1, lx.text(q + 1), SymbolKind::Function);
                }
                self.skip_fn(q + 1)
            }
            b"const" | b"var" if lx.is_word_tok(q + 1) && lx.is_sym(q + 2, '=') => {
                let mut r = q + 3;
                if matches!(lx.word(r), b"extern" | b"packed") {
                    r += 1;
                }
                let kind = match lx.word(r) {
                    b"struct" | b"union" | b"opaque" => SymbolKind::Struct,
                    b"enum" | b"error" => SymbolKind::Enum,
                    _ => return self.skip_statement(q + 3),
                };
                let error_set = lx.is_word(r, "error");
                r += 1;
                if lx.is_sym(r, '(') {
                    r = self.skip_group(r);
                }
                if !lx.is_sym(r, '{') {
                    return self.skip_statement(r);
                }
                self.push(q + 1, lx.text(q + 1), kind);
                if error_set {
                    return self.skip_statement(r);
                }
                self.frames.push(true);
                self.depth += 1;
                r + 1
            }
            b"test" => {
                let name = match lx.tt(q + 1) {
                    Some(Tt::Lit | Tt::Word) => format!("test {}", lx.text(q + 1)),
                    _ => "test".to_string(),
                };
                self.push(q, name, SymbolKind::Function);
                let body = if lx.is_sym(q + 1, '{') { q + 1 } else { q + 2 };
                if lx.is_sym(body, '{') { self.skip_group(body) } else { q + 1 }
            }
            b"comptime" if lx.is_sym(q + 1, '{') => self.skip_group(q + 1),
            _ => self.skip_statement(q),
        }
    }

    /// From a fn name: past the parameters, the return type (which may hold `error{…}` or
    /// `struct {…}` groups) and the body, or past `;` for a prototype.
    fn skip_fn(&self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = p;
        while q < lx.len() {
            match lx.sym(q) {
                Some('{') => {
                    let type_group = q
                        .checked_sub(1)
                        .is_some_and(|r| matches!(lx.word(r), b"error" | b"struct" | b"enum" | b"union" | b"opaque"));
                    if !type_group {
                        return self.skip_group(q);
                    }
                    q = self.skip_group(q);
                }
                Some('(' | '[') => q = self.skip_group(q),
                Some(';') => return q + 1,
                Some('}') => return q,
                _ => q += 1,
            }
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, checked, kind_at, offset_of};
    use crate::{
        FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
        outline,
    };

    const Z: Language = Language::Zig;

    fn fold(start_line: usize, end_line: usize, kind: FoldKind) -> FoldRange {
        FoldRange { start_line, end_line, kind }
    }

    #[test]
    fn keywords_types_builtins() {
        let text =
            "const std = @import(\"std\");\npub fn add(a: u32, b: i7) !usize {\n    return @as(usize, a) + b;\n}";
        assert_kind(Z, text, "const", K::Keyword);
        assert_kind(Z, text, "std", K::Ident);
        assert_kind(Z, text, "@import", K::Macro);
        assert_kind(Z, text, "\"std\"", K::String);
        assert_kind(Z, text, "pub", K::Keyword);
        assert_kind(Z, text, "fn", K::Keyword);
        assert_kind(Z, text, "add", K::Function);
        assert_kind(Z, text, "u32", K::Type);
        assert_kind(Z, text, "i7", K::Type);
        assert_kind(Z, text, "usize", K::Type);
        assert_kind(Z, text, "@as", K::Macro);
        assert_kind(Z, text, "return", K::Keyword);
    }

    #[test]
    fn literals_fields_calls_constants() {
        let text = "var x: ?*T = null; x = undefined; const ok = true and !false; list.append(item); p.len; const MAX = 0x_FF; @\"weird name\"";
        assert_kind(Z, text, "null", K::Boolean);
        assert_kind(Z, text, "undefined", K::Boolean);
        assert_kind(Z, text, "true", K::Boolean);
        assert_kind(Z, text, "false", K::Boolean);
        assert_kind(Z, text, "and", K::Keyword);
        assert_kind(Z, text, "append", K::Function);
        assert_kind(Z, text, "len", K::Field);
        assert_kind(Z, text, "MAX", K::Constant);
        assert_kind(Z, text, "0x_FF", K::Number);
        assert_kind(Z, text, "T", K::Type);
        assert_kind(Z, text, "@\"weird name\"", K::Ident);
        assert_kind(Z, "const point = struct {};", "point", K::Type);
        assert_kind(Z, "for (0..10) |i| {}", "0", K::Number);
        assert_kind(Z, "for (0..10) |i| {}", "10", K::Number);
    }

    #[test]
    fn strings_chars_multiline_and_comments() {
        let text = "/// Doc\n//! Top\n// plain\nconst s =\n    \\\\line one \"x\"\n    \\\\line two\n;\nconst c = '\\n';\nconst e = \"a\\\"b\";";
        assert_kind(Z, text, "/// Doc", K::DocComment);
        assert_kind(Z, text, "//! Top", K::DocComment);
        assert_kind(Z, text, "// plain", K::Comment);
        assert_kind(Z, text, "\\\\line one \"x\"", K::String);
        assert_kind(Z, text, "\\\\line two", K::String);
        assert_kind(Z, text, "'\\n'", K::Char);
        assert_kind(Z, text, "\"a\\\"b\"", K::String);
        assert_kind(Z, "const s = \"open\nconst t = 1;", "\"open", K::Invalid);
        assert_eq!(kind_at(Z, "const s = \"open\nconst t = 1;", 1, 0), K::Keyword);
    }

    #[test]
    fn non_ascii_and_crlf() {
        let h = checked(Z, "const s = \"ü\"; // ä\r\n");
        let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(
            cols,
            [
                (0, 5, K::Keyword),
                (6, 7, K::Ident),
                (8, 9, K::Operator),
                (10, 13, K::String),
                (13, 14, K::Punctuation),
                (15, 20, K::Comment)
            ]
        );
    }

    #[test]
    fn brackets_skip_strings_and_comments() {
        let text = "fn f() void {\n    _ = \"}\"; // }\n    _ = '}';\n}";
        assert_eq!(matching_bracket(Z, text, offset_of(text, "{", 0)), Some(text.chars().count() - 1));
    }

    #[test]
    fn folds() {
        let text = "\
const std = @import(\"std\");
const mem = std.mem;
const Allocator = std.mem.Allocator;
/// a
/// b
pub fn main() void {
    const s =
        \\\\one
        \\\\two
    ;
}
";
        assert_eq!(
            folding_ranges(Z, text),
            vec![
                fold(0, 2, FoldKind::Imports),
                fold(3, 4, FoldKind::Comment),
                fold(5, 10, FoldKind::Block),
                fold(7, 8, FoldKind::Block)
            ]
        );
    }

    #[test]
    fn outline_lists_functions_containers_and_tests() {
        let text = "\
const std = @import(\"std\");
pub const Point = struct {
    x: i32 = 0,
    y: i32,
    pub fn len(self: Point) f32 {
        const Inner = struct { fn hidden() void {} };
        return 0;
    }
    const Kind = enum(u8) { a, b };
};
const Err = error{ Oops, Bad };
const Tagged = union(enum) { a: u8 };
extern \"c\" fn write(fd: c_int) isize;
fn fallible() error{Oops}!void {
    return error.Oops;
}
test \"point length\" {
    try std.testing.expect(true);
}
test {}
";
        let items: Vec<(String, SymbolKind, usize)> =
            outline(Z, text).into_iter().map(|i| (i.name, i.kind, i.depth)).collect();
        assert_eq!(
            items,
            vec![
                ("Point".into(), SymbolKind::Struct, 0),
                ("len".into(), SymbolKind::Function, 1),
                ("Kind".into(), SymbolKind::Enum, 1),
                ("Err".into(), SymbolKind::Enum, 0),
                ("Tagged".into(), SymbolKind::Struct, 0),
                ("write".into(), SymbolKind::Function, 0),
                ("fallible".into(), SymbolKind::Function, 0),
                ("test \"point length\"".into(), SymbolKind::Function, 0),
                ("test".into(), SymbolKind::Function, 0),
            ]
        );
        assert_eq!(
            outline(Z, text)[0],
            OutlineItem { name: "Point".into(), kind: SymbolKind::Struct, line: 1, column: 10, depth: 0 }
        );
    }

    #[test]
    fn every_function_handles_garbage() {
        for text in ["", "\\\\", "@", "@\"", "'", "fn", "const x =", "test", "}", "{"] {
            let _ = checked(Z, text);
            let _ = folding_ranges(Z, text);
            let _ = outline(Z, text);
            let _ = matching_bracket(Z, text, 0);
        }
    }
}
