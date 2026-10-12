//! C and C++: preprocessor directives, prefixed and raw strings, chars, numbers with digit
//! separators, Doxygen comments and `[[attributes]]`; folds for blocks, comments, `#include`
//! runs, `#if`…`#endif` and `#pragma region`; an outline of definitions.
//!
//! `#if 0` regions are highlighted as comments. Names defined with `#define` in the file are
//! highlighted as macros wherever they are used.

use std::collections::HashSet;

use crate::common::{self, Doc, Span};
use crate::lex::{self, DIRECTIVE, Lexed, TokSink, Tt};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dn {
    Include,
    Define,
    If,
    /// `#else`, `#elif…`: neither opens nor closes a region.
    Else,
    Endif,
    Region,
    EndRegion,
    Other,
}

/// A preprocessor directive: its kind, the offset of its `#`, the offset where its logical
/// line ends, and the macro name it defines (`#define NAME`).
#[derive(Clone, Copy, Debug)]
struct Directive {
    kind: Dn,
    start: usize,
    end: usize,
    name: Option<(usize, usize)>,
}

struct Analysis<'a> {
    lexed: Lexed<'a>,
    directives: Vec<Directive>,
}

fn analyze(doc: &Doc, cpp: bool) -> Analysis<'_> {
    let ascii = lex::ascii(doc, false);
    let (toks, directives) = {
        let mut lexer = Lexer {
            doc,
            ascii: &ascii,
            sink: TokSink::with_capacity(doc.len() / 4),
            cpp,
            line_code: false,
            directive_end: 0,
            disabled: None,
            directives: Vec::new(),
        };
        lexer.run();
        (lexer.sink.toks, lexer.directives)
    };
    let mut lexed = Lexed::new(doc, ascii, toks);
    classify(&mut lexed, cpp, &directives);
    Analysis { lexed, directives }
}

pub(crate) fn spans(doc: &Doc, cpp: bool) -> Vec<Span> {
    analyze(doc, cpp).lexed.spans()
}

pub(crate) fn brackets(doc: &Doc, cpp: bool) -> Vec<common::Bracket> {
    analyze(doc, cpp).lexed.brackets()
}

struct Lexer<'a> {
    doc: &'a Doc,
    ascii: &'a [u8],
    sink: TokSink,
    cpp: bool,
    /// A significant token already started on the current line.
    line_code: bool,
    /// End of the current directive's logical line; tokens before it belong to the directive.
    directive_end: usize,
    /// A pending `#if 0` region: (start, end).
    disabled: Option<(usize, usize)>,
    directives: Vec<Directive>,
}

fn is_hspace(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// End of the logical line containing `i`: its `\n` (or the text end), following
/// backslash-newline continuations.
fn logical_line_end(doc: &Doc, i: usize) -> usize {
    let mut e = doc.line_end_at(i);
    loop {
        let mut k = e;
        if k > 0 && doc.at(k - 1) == '\r' {
            k -= 1;
        }
        if k > i && doc.at(k.saturating_sub(1)) == '\\' && e < doc.len() {
            e = doc.line_end_at(e + 1);
        } else {
            return e;
        }
    }
}

impl Lexer<'_> {
    fn push(&mut self, start: usize, end: usize, tt: Tt, kind: K) {
        let flags = if start < self.directive_end { DIRECTIVE } else { 0 };
        if !matches!(tt, Tt::LineComment | Tt::BlockComment) {
            self.line_code = true;
        }
        self.sink.push_flags(start, end, tt, kind, flags);
    }

    fn run(&mut self) {
        let doc = self.doc;
        let n = doc.len();
        let mut i = 0;
        while i < n {
            if let Some((start, end)) = self.disabled
                && start == i
            {
                self.disabled = None;
                if end > start {
                    let e = doc.trim_end(start, end);
                    self.sink.push(start, e, Tt::BlockComment, K::Comment);
                    i = end;
                    continue;
                }
            }
            let c = doc.at(i);
            let next = match c {
                '\n' => {
                    self.line_code = false;
                    i + 1
                }
                c if c.is_whitespace() || c == '\\' => i + 1,
                '/' if doc.at(i + 1) == '/' => self.line_comment(i),
                '/' if doc.at(i + 1) == '*' => {
                    let (e, _) = lex::block_comment_end(doc, i, false);
                    let doc_style =
                        (doc.at(i + 2) == '*' && !matches!(doc.at(i + 3), '*' | '/')) || doc.at(i + 2) == '!';
                    self.push(i, e, Tt::BlockComment, if doc_style { K::DocComment } else { K::Comment });
                    e
                }
                '#' if !self.line_code && i >= self.directive_end => self.directive(i),
                '"' => self.string(i, i),
                '\'' => self.char_lit(i, i),
                c if c.is_ascii_digit() => self.number(i),
                '.' if lex::dot_number(doc, i) => self.number(i),
                c if lex::is_ident_start(c) => self.ident(i),
                '@' | '`' | '$' => {
                    self.push(i, i + 1, Tt::Other, K::Invalid);
                    i + 1
                }
                c if c.is_ascii_punctuation() => {
                    self.push(i, i + 1, Tt::Sym(c), lex::sym_kind(c));
                    i + 1
                }
                _ => {
                    self.push(i, i + 1, Tt::Other, K::Invalid);
                    i + 1
                }
            };
            i = next.max(i + 1);
        }
    }

    /// `//` comment to the end of the line, continued by a trailing backslash.
    fn line_comment(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let e = logical_line_end(doc, i);
        let doc_style = (doc.at(i + 2) == '/' && doc.at(i + 3) != '/') || doc.at(i + 2) == '!';
        self.push(i, e, Tt::LineComment, if doc_style { K::DocComment } else { K::Comment });
        e
    }

    fn number(&mut self, i: usize) -> usize {
        let e = lex::number_end(self.doc, i, true);
        self.push(i, e, Tt::Lit, K::Number);
        e
    }

    /// A string whose opening quote is at `quote` (a prefix starts at `start`).
    fn string(&mut self, start: usize, quote: usize) -> usize {
        let (mut e, ok) = lex::scan_quoted(self.doc, quote + 1, '"', true, false);
        if ok && self.cpp && lex::is_ident_start(self.doc.at(e)) {
            // User-defined literal suffix: `"abc"s`, `"x"_sv`.
            e = self.doc.scan_while(e, lex::is_ident_char);
        }
        self.push(start, e, Tt::Lit, if ok { K::String } else { K::Invalid });
        e
    }

    fn char_lit(&mut self, start: usize, quote: usize) -> usize {
        let (e, ok) = lex::scan_quoted(self.doc, quote + 1, '\'', true, false);
        self.push(start, e, Tt::Lit, if ok { K::Char } else { K::Invalid });
        e
    }

    /// `R"delim( … )delim"`, possibly over several lines; `quote` is the `"`.
    fn raw_string(&mut self, start: usize, quote: usize) -> usize {
        const MAX_DELIMITER: usize = 16;
        let doc = self.doc;
        let open = doc.scan_while(quote + 1, |c| !matches!(c, '(' | ')' | '\\' | '"') && !c.is_whitespace());
        if doc.at(open) != '(' || open - quote - 1 > MAX_DELIMITER {
            let e = doc.line_end_at(start);
            self.push(start, e, Tt::Lit, K::Invalid);
            return e;
        }
        let mut close = String::from(")");
        close.extend(doc.slice(quote + 1, open));
        close.push('"');
        match doc.find_str(open + 1, &close) {
            Some(j) => {
                let e = j + close.chars().count();
                self.push(start, e, Tt::Lit, K::String);
                e
            }
            None => {
                self.push(start, doc.len(), Tt::Lit, K::Invalid);
                doc.len()
            }
        }
    }

    fn ident(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let e = doc.scan_while(i, lex::is_ident_char);
        let word = self.ascii.get(i..e).unwrap_or(&[]);
        match (word, doc.at(e)) {
            (b"L" | b"u" | b"U" | b"u8", '"') => return self.string(i, e),
            (b"L" | b"u" | b"U" | b"u8", '\'') => return self.char_lit(i, e),
            (b"R" | b"LR" | b"uR" | b"UR" | b"u8R", '"') => return self.raw_string(i, e),
            _ => {}
        }
        self.push(i, e, Tt::Word, K::Ident);
        e
    }

    /// A directive starting with the `#` at `i`; returns where lexing continues.
    fn directive(&mut self, i: usize) -> usize {
        let doc = self.doc;
        self.directive_end = logical_line_end(doc, i);
        let name_start = doc.scan_while(i + 1, is_hspace);
        let name_end = doc.scan_while(name_start, lex::is_ident_char);
        let head_end = if name_end > name_start { name_end } else { i + 1 };
        self.push(i, head_end, Tt::Other, K::Macro);
        let name = self.ascii.get(name_start..name_end).unwrap_or(&[]);
        let kind = match name {
            b"include" | b"include_next" | b"import" => Dn::Include,
            b"define" => Dn::Define,
            b"if" | b"ifdef" | b"ifndef" => Dn::If,
            b"else" | b"elif" | b"elifdef" | b"elifndef" => Dn::Else,
            b"endif" => Dn::Endif,
            b"pragma" => self.pragma_kind(name_end),
            _ => Dn::Other,
        };
        let mut directive = Directive { kind, start: i, end: self.directive_end, name: None };
        let arg = doc.scan_while(name_end, is_hspace);
        let next = match name {
            b"include" | b"include_next" | b"import" if doc.at(arg) == '<' => {
                match (arg + 1..self.directive_end).find(|&j| doc.at(j) == '>') {
                    Some(close) => {
                        self.push(arg, close + 1, Tt::Lit, K::String);
                        close + 1
                    }
                    None => head_end,
                }
            }
            b"define" | b"undef" | b"ifdef" | b"ifndef" | b"elifdef" | b"elifndef"
                if lex::is_ident_start(doc.at(arg)) =>
            {
                let e = doc.scan_while(arg, lex::is_ident_char);
                self.push(arg, e, Tt::Other, K::Macro);
                if kind == Dn::Define {
                    directive.name = Some((arg, e));
                }
                e
            }
            b"error" | b"warning" => {
                let e = doc.trim_end(arg, self.directive_end);
                self.push(arg, e, Tt::Lit, K::String);
                e.max(head_end)
            }
            b"if" if self.is_zero_condition(arg) => {
                let start = (self.directive_end + 1).min(doc.len());
                self.disabled = Some((start, disabled_end(doc, start)));
                head_end
            }
            _ => head_end,
        };
        self.directives.push(directive);
        next
    }

    fn pragma_kind(&self, from: usize) -> Dn {
        let doc = self.doc;
        let s = doc.scan_while(from, is_hspace);
        let e = doc.scan_while(s, lex::is_ident_char);
        match self.ascii.get(s..e).unwrap_or(&[]) {
            b"region" => Dn::Region,
            b"endregion" => Dn::EndRegion,
            _ => Dn::Other,
        }
    }

    /// The directive's condition at `from` is the literal `0` (optionally followed by a comment).
    fn is_zero_condition(&self, from: usize) -> bool {
        let doc = self.doc;
        if doc.at(from) != '0' {
            return false;
        }
        let after = doc.scan_while(from + 1, |c| c.is_whitespace() && c != '\n');
        after >= self.directive_end || doc.starts_with_at(after, "//") || doc.starts_with_at(after, "/*")
    }
}

/// Start of the line holding the `#else`/`#elif`/`#endif` that ends an `#if 0` region whose
/// body starts at `start` (nested conditionals are skipped), or the text end.
fn disabled_end(doc: &Doc, start: usize) -> usize {
    let mut depth = 0usize;
    let mut line = doc.index.line_of(start);
    while line < doc.index.len_lines() {
        let line_start = doc.index.line_start(line);
        if line_start < start {
            line += 1;
            continue;
        }
        if let Some(f) = doc.first_non_ws(line)
            && doc.at(f) == '#'
        {
            let s = doc.scan_while(f + 1, is_hspace);
            let e = doc.scan_while(s, lex::is_ident_char);
            match doc.string(s, e).as_str() {
                "if" | "ifdef" | "ifndef" => depth += 1,
                "endif" if depth > 0 => depth -= 1,
                "endif" | "else" | "elif" | "elifdef" | "elifndef" if depth == 0 => return line_start,
                _ => {}
            }
        }
        line += 1;
    }
    doc.len()
}

// ---------------------------------------------------------------------------------------------
// Classification

fn is_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"auto"
            | b"break"
            | b"case"
            | b"const"
            | b"continue"
            | b"default"
            | b"do"
            | b"else"
            | b"enum"
            | b"extern"
            | b"for"
            | b"goto"
            | b"if"
            | b"inline"
            | b"register"
            | b"restrict"
            | b"return"
            | b"sizeof"
            | b"static"
            | b"struct"
            | b"switch"
            | b"typedef"
            | b"union"
            | b"volatile"
            | b"while"
            | b"_Alignas"
            | b"_Alignof"
            | b"_Atomic"
            | b"_Generic"
            | b"_Noreturn"
            | b"_Static_assert"
            | b"_Thread_local"
            | b"alignas"
            | b"alignof"
            | b"static_assert"
            | b"thread_local"
            | b"typeof"
            | b"typeof_unqual"
            | b"constexpr"
            | b"asm"
            | b"__asm__"
            | b"__attribute__"
            | b"__declspec"
            | b"__inline"
            | b"__restrict"
            | b"__extension__"
            | b"__volatile__"
            | b"_Pragma"
    )
}

fn is_cpp_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"and"
            | b"and_eq"
            | b"bitand"
            | b"bitor"
            | b"catch"
            | b"class"
            | b"co_await"
            | b"co_return"
            | b"co_yield"
            | b"compl"
            | b"concept"
            | b"consteval"
            | b"constinit"
            | b"const_cast"
            | b"decltype"
            | b"delete"
            | b"dynamic_cast"
            | b"explicit"
            | b"export"
            | b"friend"
            | b"mutable"
            | b"namespace"
            | b"new"
            | b"noexcept"
            | b"not"
            | b"not_eq"
            | b"operator"
            | b"or"
            | b"or_eq"
            | b"private"
            | b"protected"
            | b"public"
            | b"reinterpret_cast"
            | b"requires"
            | b"static_cast"
            | b"template"
            | b"throw"
            | b"try"
            | b"typeid"
            | b"typename"
            | b"using"
            | b"virtual"
            | b"xor"
            | b"xor_eq"
    )
}

fn is_primitive(w: &[u8]) -> bool {
    matches!(
        w,
        b"void"
            | b"char"
            | b"short"
            | b"int"
            | b"long"
            | b"float"
            | b"double"
            | b"signed"
            | b"unsigned"
            | b"bool"
            | b"_Bool"
            | b"_Complex"
            | b"_Imaginary"
            | b"wchar_t"
            | b"char8_t"
            | b"char16_t"
            | b"char32_t"
            | b"__int128"
            | b"_BitInt"
    )
}

/// Common standard-library type names, highlighted as types after `std::`.
fn is_std_type(w: &[u8]) -> bool {
    matches!(
        w,
        b"string"
            | b"wstring"
            | b"u8string"
            | b"u16string"
            | b"u32string"
            | b"string_view"
            | b"vector"
            | b"map"
            | b"multimap"
            | b"unordered_map"
            | b"unordered_multimap"
            | b"set"
            | b"multiset"
            | b"unordered_set"
            | b"array"
            | b"deque"
            | b"list"
            | b"forward_list"
            | b"queue"
            | b"priority_queue"
            | b"stack"
            | b"pair"
            | b"tuple"
            | b"optional"
            | b"variant"
            | b"any"
            | b"expected"
            | b"unique_ptr"
            | b"shared_ptr"
            | b"weak_ptr"
            | b"function"
            | b"span"
            | b"bitset"
            | b"complex"
            | b"atomic"
            | b"mutex"
            | b"thread"
            | b"jthread"
            | b"future"
            | b"promise"
            | b"ostream"
            | b"istream"
            | b"iostream"
            | b"ostringstream"
            | b"istringstream"
            | b"stringstream"
            | b"ifstream"
            | b"ofstream"
            | b"fstream"
            | b"exception"
            | b"runtime_error"
            | b"logic_error"
            | b"invalid_argument"
            | b"out_of_range"
            | b"error_code"
            | b"initializer_list"
            | b"byte"
    )
}

/// Words that never name a function or class even when `(` follows.
fn is_reserved(w: &[u8], cpp: bool) -> bool {
    is_keyword(w) || is_primitive(w) || (cpp && is_cpp_keyword(w))
}

fn classify(lx: &mut Lexed<'_>, cpp: bool, directives: &[Directive]) {
    let defines: HashSet<Vec<u8>> =
        directives.iter().filter_map(|d| d.name).filter_map(|(s, e)| lx.ascii.get(s..e).map(<[u8]>::to_vec)).collect();
    let ends = lx.group_ends();
    let mut kinds: Vec<Option<K>> = vec![None; lx.len()];
    let mut p = 0;
    while p < lx.len() {
        match lx.tt(p) {
            Some(Tt::Sym('[')) if lx.is_pair(p, '[', '[') && !follows_value(lx, p) => {
                // `[[attribute]]`: everything up to the matching `]]`.
                let close = ends.get(p).copied().flatten().filter(|&c| lx.is_sym(c, ']'));
                let last = close.unwrap_or(p + 1);
                for slot in kinds.get_mut(p..=last).into_iter().flatten() {
                    *slot = Some(K::Attribute);
                }
                p = last + 1;
            }
            Some(Tt::Word) => {
                if let Some(slot) = kinds.get_mut(p) {
                    *slot = Some(word_kind(lx, p, cpp, &defines));
                }
                p += 1;
            }
            _ => p += 1,
        }
    }
    for (p, kind) in kinds.into_iter().enumerate() {
        if let Some(kind) = kind {
            lx.set(p, kind);
        }
    }
}

/// The token before `p` ends a value (`a[`, `f()[`), so `[` is a subscript.
fn follows_value(lx: &Lexed<'_>, p: usize) -> bool {
    p.checked_sub(1).is_some_and(|q| matches!(lx.tt(q), Some(Tt::Word | Tt::Lit | Tt::Sym(')' | ']'))))
}

fn word_kind(lx: &Lexed<'_>, p: usize, cpp: bool, defines: &HashSet<Vec<u8>>) -> K {
    let w = lx.word(p);
    if lx.after_member_access(p, true) {
        return if lx.is_call(p + 1, cpp) { K::Function } else { K::Field };
    }
    match w {
        b"true" | b"false" => return K::Boolean,
        b"nullptr" if cpp => return K::Boolean,
        b"this" if cpp => return K::SelfKeyword,
        b"defined" | b"__has_include" | b"__has_cpp_attribute" if lx.has_flag(p, DIRECTIVE) => return K::Keyword,
        b"override" | b"final" | b"import" | b"module" if cpp => {
            let contextual = match w {
                // After a parameter list or its qualifiers, or after a class name.
                b"override" | b"final" => p.checked_sub(1).is_some_and(|q| {
                    lx.is_sym(q, ')')
                        || matches!(lx.word(q), b"const" | b"noexcept" | b"volatile" | b"final" | b"override")
                        || (q > 0
                            && matches!(lx.word(q - 1), b"class" | b"struct" | b"union")
                            && matches!(lx.sym(p + 1), Some('{' | ':')))
                }),
                _ => p == 0 || lx.is_sym(p - 1, ';') || lx.is_sym(p - 1, '}') || lx.is_word(p - 1, "export"),
            };
            return if contextual { K::Keyword } else { K::Ident };
        }
        _ => {}
    }
    if is_keyword(w) || (cpp && is_cpp_keyword(w)) {
        return K::Keyword;
    }
    if is_primitive(w) || (w.len() > 2 && w.ends_with(b"_t")) {
        return K::Type;
    }
    let prev = p.checked_sub(1);
    if cpp && is_std_type(w) && prev.is_some_and(|q| q >= 2 && lx.is_pair(q - 1, ':', ':') && lx.is_word(q - 2, "std"))
    {
        return K::Type;
    }
    if prev.is_some_and(|q| matches!(lx.word(q), b"struct" | b"class" | b"union" | b"enum" | b"typename" | b"concept"))
    {
        return K::Type;
    }
    if !defines.is_empty() && defines.contains(w) {
        return K::Macro;
    }
    let chars = lx.chars(p);
    if lex::is_screaming(chars) {
        return if lx.is_sym(p + 1, '(') { K::Macro } else { K::Constant };
    }
    if lex::is_capitalized(chars) {
        return K::Type;
    }
    if lx.is_call(p + 1, cpp) {
        return K::Function;
    }
    K::Ident
}

// ---------------------------------------------------------------------------------------------
// Folding

pub(crate) fn folding_ranges(doc: &Doc, cpp: bool) -> Vec<FoldRange> {
    let Analysis { lexed: lx, directives } = analyze(doc, cpp);
    let mut out = Vec::new();
    // Brackets of code only: macro bodies often hold unbalanced braces.
    let mut lines = lex::LineCursor::new(&doc.index);
    let code_brackets = lx.toks.iter().filter(|t| t.flags & DIRECTIVE == 0).filter_map(|t| match t.tt {
        Tt::Sym(c) if common::is_open(c) || common::is_close(c) => Some((c, t.start)),
        _ => None,
    });
    let pairs: Vec<(char, usize)> = code_brackets.map(|(c, start)| (c, lines.line_of(start))).collect();
    common::bracket_folds(pairs, &mut out);
    lx.comment_folds(&mut out);
    directive_folds(&lx, &directives, &mut out);
    common::finish_folds(out)
}

/// `#if`…`#endif` and `#pragma region` blocks, and runs of `#include` lines.
fn directive_folds(lx: &Lexed<'_>, directives: &[Directive], out: &mut Vec<FoldRange>) {
    let index = &lx.doc.index;
    let mut conditionals: Vec<usize> = Vec::new();
    let mut regions: Vec<usize> = Vec::new();
    for d in directives {
        let line = index.line_of(d.start);
        match d.kind {
            Dn::If => conditionals.push(line),
            Dn::Endif => {
                if let Some(start) = conditionals.pop() {
                    common::push_fold(out, start, line, FoldKind::Block);
                }
            }
            Dn::Region => regions.push(line),
            Dn::EndRegion => {
                if let Some(start) = regions.pop() {
                    common::push_fold(out, start, line, FoldKind::Block);
                }
            }
            _ => {}
        }
    }
    // Statements: each directive is one; each run of code between directives is another.
    let mut stmts: Vec<(bool, usize, usize)> = Vec::new();
    let mut next_directive = directives.iter().peekable();
    let mut in_code = false;
    for t in &lx.toks {
        if t.is_comment() {
            continue;
        }
        if t.flags & DIRECTIVE != 0 {
            while let Some(d) = next_directive.peek() {
                if d.start > t.start {
                    break;
                }
                if d.start == t.start {
                    let end = index.line_of(d.end.saturating_sub(1).max(d.start));
                    stmts.push((d.kind == Dn::Include, index.line_of(d.start), end));
                    in_code = false;
                }
                next_directive.next();
            }
        } else if !in_code {
            let line = index.line_of(t.start);
            stmts.push((false, line, line));
            in_code = true;
        }
    }
    lex::statement_runs(&stmts, FoldKind::Imports, out);
}

// ---------------------------------------------------------------------------------------------
// Outline

pub(crate) fn outline(doc: &Doc, cpp: bool) -> Vec<OutlineItem> {
    let Analysis { mut lexed, directives } = analyze(doc, cpp);
    lexed.refilter(|t| !t.is_comment() && t.flags & DIRECTIVE == 0);
    let ends = lexed.group_ends();
    let outliner = Outliner { lx: &lexed, ends, cpp, items: Vec::new(), frames: Vec::new(), depth: 0 };
    let mut items = outliner.run();
    for d in &directives {
        if let Some((s, e)) = d.name {
            let line = doc.index.line_of(s);
            let column = s.saturating_sub(doc.index.line_start(line));
            items.push(OutlineItem { name: doc.string(s, e), kind: SymbolKind::Macro, line, column, depth: 0 });
        }
    }
    items.sort_by_key(|item| (item.line, item.column));
    items
}

/// Walks declarations at file, namespace and class level. Function bodies, initializers and
/// every other group are skipped whole, so their contents never produce items.
struct Outliner<'a, 'b> {
    lx: &'b Lexed<'a>,
    ends: Vec<Option<usize>>,
    cpp: bool,
    items: Vec<OutlineItem>,
    /// One entry per entered `{`: whether it increased the depth.
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

    fn push(&mut self, p: usize, name: String, kind: SymbolKind) {
        if let Some(item) = self.lx.item(p, name, kind, self.depth) {
            self.items.push(item);
        }
    }

    fn skip_group(&self, p: usize) -> usize {
        self.ends.get(p).copied().flatten().map_or(self.lx.len(), |e| e + 1)
    }

    fn enter(&mut self, brace: usize, deeper: bool) -> usize {
        self.frames.push(deeper);
        if deeper {
            self.depth += 1;
        }
        brace + 1
    }

    /// Past the `;` ending a declaration (groups skipped); stops before a stray `}`.
    fn skip_to_semi(&self, p: usize) -> usize {
        let mut q = p;
        while q < self.lx.len() {
            match self.lx.sym(q) {
                Some('{' | '(' | '[') => q = self.skip_group(q),
                Some(';') => return q + 1,
                Some('}') => return q,
                _ => q += 1,
            }
        }
        q
    }

    /// Past the `<…>` starting at `p`; stops at `;`, `{` or `}`.
    fn skip_angle(&self, p: usize) -> usize {
        let mut depth = 0usize;
        let mut q = p;
        while q < self.lx.len() {
            match self.lx.sym(q) {
                Some('<') => depth += 1,
                Some('>') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return q + 1;
                    }
                }
                Some('(' | '[') => {
                    q = self.skip_group(q);
                    continue;
                }
                Some(';' | '{' | '}') => return q,
                _ => {}
            }
            q += 1;
        }
        q
    }

    /// Joins the texts of tokens `from..=to` (a qualified name like `ns::Type::~Type`).
    fn joined(&self, from: usize, to: usize) -> String {
        let mut name = String::new();
        for q in from..=to {
            if q > from && self.lx.is_word_tok(q) && self.lx.is_word_tok(q - 1) {
                name.push(' ');
            }
            name.extend(self.lx.chars(q));
        }
        name
    }

    /// Start of the qualified name ending with the word at `q` (`A::B::name`, `~T`).
    fn qualified_start(&self, q: usize) -> usize {
        let mut r = q;
        loop {
            if r >= 1 && self.lx.is_sym(r - 1, '~') {
                r -= 1;
            }
            if r >= 3 && self.lx.is_pair(r - 2, ':', ':') && self.lx.is_word_tok(r - 3) {
                r -= 3;
            } else {
                return r;
            }
        }
    }

    /// One declaration at container level starting at `p`; returns where the next begins.
    fn declaration(&mut self, p: usize) -> usize {
        let lx = self.lx;
        let mut q = p;
        // (position, name) of the last `name(…)` seen.
        let mut candidate: Option<(usize, String)> = None;
        let mut init_list = false;
        while q < lx.len() {
            match lx.tt(q) {
                Some(Tt::Sym(';')) => return q + 1,
                Some(Tt::Sym('}')) => return q,
                Some(Tt::Sym('{')) => {
                    let member_init = init_list && !matches!(lx.sym(q.wrapping_sub(1)), Some(')' | '}'));
                    if !member_init && let Some((at, name)) = candidate.take() {
                        self.push(at, name, SymbolKind::Function);
                        return self.skip_group(q);
                    }
                    q = self.skip_group(q);
                }
                Some(Tt::Sym('=')) => {
                    let compound = q > 0 && matches!(lx.sym(q - 1), Some('=' | '<' | '>' | '!')) && lx.adjacent(q - 1);
                    if lx.is_pair(q, '=', '=') || compound {
                        q += 2;
                    } else {
                        return self.skip_to_semi(q + 1);
                    }
                }
                Some(Tt::Sym(':')) => {
                    if lx.is_pair(q, ':', ':') {
                        q += 2;
                    } else if candidate.is_some() {
                        init_list = true;
                        q += 1;
                    } else {
                        // `public:`, a bit-field width or a label.
                        return q + 1;
                    }
                }
                Some(Tt::Sym('(' | '[')) => q = self.skip_group(q),
                Some(Tt::Word) => {
                    let w = lx.word(q);
                    match w {
                        b"namespace" if self.cpp => return self.namespace(q),
                        b"class" | b"struct" | b"union" | b"enum" if !init_list => {
                            if let Some(next) = self.class(q) {
                                return next;
                            }
                            q += 1;
                        }
                        b"typedef" => return self.typedef(q),
                        b"using" if self.cpp => return self.using(q),
                        b"template" if lx.is_sym(q + 1, '<') => q = self.skip_angle(q + 1),
                        b"extern" if lx.tt(q + 1) == Some(Tt::Lit) && lx.is_sym(q + 2, '{') => {
                            return self.enter(q + 2, false);
                        }
                        b"operator" if self.cpp => {
                            let (name, params) = self.operator_name(q);
                            candidate = Some((self.qualified_start(q), name));
                            q = if lx.is_sym(params, '(') { self.skip_group(params) } else { params };
                        }
                        // A member initializer: `x_(x)`.
                        _ if init_list && lx.is_sym(q + 1, '(') => q = self.skip_group(q + 1),
                        _ if lx.is_sym(q + 1, '(') && !is_reserved(w, self.cpp) => {
                            let start = self.qualified_start(q);
                            candidate = Some((start, self.joined(start, q)));
                            q = self.skip_group(q + 1);
                        }
                        _ => q += 1,
                    }
                }
                _ => q += 1,
            }
        }
        q
    }

    /// `operator==`, `operator()`, `operator bool`…: (name, position of the parameter list).
    fn operator_name(&self, q: usize) -> (String, usize) {
        const MAX_TOKENS: usize = 6;
        let lx = self.lx;
        let mut name = self.joined(self.qualified_start(q), q);
        let mut r = q + 1;
        if lx.is_sym(r, '(') && lx.is_sym(r + 1, ')') {
            name.push_str("()");
            return (name, r + 2);
        }
        while r < lx.len().min(q + 1 + MAX_TOKENS) && !lx.is_sym(r, '(') {
            if lx.is_word_tok(r) {
                name.push(' ');
            }
            name.extend(lx.chars(r));
            r += 1;
        }
        (name, r)
    }

    /// `class`/`struct`/`union`/`enum` at `q`. A definition (`… Name … {`) is recorded and its
    /// body entered (skipped for enums); returns `None` for a use of the type in a declaration.
    fn class(&mut self, q: usize) -> Option<usize> {
        const MAX_HEAD: usize = 128;
        let lx = self.lx;
        let kind = if lx.is_word(q, "enum") { SymbolKind::Enum } else { SymbolKind::Struct };
        let mut r = q + 1;
        if kind == SymbolKind::Enum && (lx.is_word(r, "class") || lx.is_word(r, "struct")) {
            r += 1;
        }
        // Attributes: `[[…]]`, `alignas(…)`, `__attribute__((…))`, `__declspec(…)`.
        loop {
            if lx.is_sym(r, '[') {
                r = self.skip_group(r);
            } else if lx.is_word_tok(r) && is_reserved(lx.word(r), self.cpp) && lx.is_sym(r + 1, '(') {
                r = self.skip_group(r + 1);
            } else {
                break;
            }
        }
        let mut name: Option<(usize, String)> = None;
        if lx.is_word_tok(r) && !lx.is_word(r, "final") {
            let start = r;
            while lx.is_pair(r + 1, ':', ':') && lx.is_word_tok(r + 3) {
                r += 3;
            }
            name = Some((start, self.joined(start, r)));
            r += 1;
        }
        if lx.is_sym(r, '<') {
            r = self.skip_angle(r);
        }
        if lx.is_word(r, "final") {
            r += 1;
        }
        if lx.is_sym(r, ':') && !lx.is_pair(r, ':', ':') {
            // Base clause (`: public Base<T>, Other`) or an enum's underlying type.
            let limit = lx.len().min(r + MAX_HEAD);
            r += 1;
            while r < limit && !lx.is_sym(r, '{') {
                match lx.tt(r) {
                    Some(Tt::Sym('<')) => r = self.skip_angle(r),
                    Some(Tt::Word | Tt::Sym(':' | ',' | '.')) => r += 1,
                    _ => return None,
                }
            }
        }
        if !lx.is_sym(r, '{') {
            return None;
        }
        let named = name.is_some();
        if let Some((at, name)) = name {
            self.push(at, name, kind);
        }
        Some(if kind == SymbolKind::Enum { self.skip_group(r) } else { self.enter(r, named) })
    }

    fn namespace(&mut self, q: usize) -> usize {
        let lx = self.lx;
        let mut r = q + 1;
        let mut name: Option<(usize, String)> = None;
        if lx.is_word_tok(r) {
            let start = r;
            while lx.is_pair(r + 1, ':', ':') && lx.is_word_tok(r + 3) {
                r += 3;
            }
            name = Some((start, self.joined(start, r)));
            r += 1;
        }
        if !lx.is_sym(r, '{') {
            return self.skip_to_semi(r);
        }
        let named = name.is_some();
        if let Some((at, name)) = name {
            self.push(at, name, SymbolKind::Module);
        }
        self.enter(r, named)
    }

    /// `typedef … Name;`: a type alias (the name inside `(*Name)` for function pointers), plus
    /// the tagged struct/union/enum it defines, if any.
    fn typedef(&mut self, q: usize) -> usize {
        let lx = self.lx;
        let mut r = q + 1;
        let mut alias: Option<usize> = None;
        let mut fn_ptr: Option<usize> = None;
        while r < lx.len() {
            match lx.tt(r) {
                Some(Tt::Sym(';')) => break,
                Some(Tt::Sym('}')) => return r,
                Some(Tt::Sym('(')) => {
                    if matches!(lx.sym(r + 1), Some('*' | '^' | '&')) && lx.is_word_tok(r + 2) && fn_ptr.is_none() {
                        fn_ptr = Some(r + 2);
                    }
                    r = self.skip_group(r);
                }
                Some(Tt::Sym('{' | '[')) => r = self.skip_group(r),
                Some(Tt::Word) => {
                    let w = lx.word(r);
                    if matches!(w, b"struct" | b"union" | b"enum" | b"class")
                        && lx.is_word_tok(r + 1)
                        && lx.is_sym(r + 2, '{')
                    {
                        let kind = if w == b"enum" { SymbolKind::Enum } else { SymbolKind::Struct };
                        self.push(r + 1, lx.text(r + 1), kind);
                        r = self.skip_group(r + 2);
                        continue;
                    }
                    if !is_reserved(w, self.cpp) {
                        alias = Some(r);
                    }
                    r += 1;
                }
                _ => r += 1,
            }
        }
        if let Some(at) = fn_ptr.or(alias) {
            self.push(at, lx.text(at), SymbolKind::TypeAlias);
        }
        (r + 1).min(lx.len())
    }

    /// `using Name = …;` is an alias; `using namespace …;` and `using A::b;` are skipped.
    fn using(&mut self, q: usize) -> usize {
        let lx = self.lx;
        if lx.is_word_tok(q + 1) && !lx.is_word(q + 1, "namespace") && lx.is_sym(q + 2, '=') {
            self.push(q + 1, lx.text(q + 1), SymbolKind::TypeAlias);
        }
        self.skip_to_semi(q + 1)
    }
}

#[cfg(test)]
mod tests;
