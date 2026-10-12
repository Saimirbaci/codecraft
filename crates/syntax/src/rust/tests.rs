use crate::test_util::{assert_kind, checked, kind_at, kind_of, offset_of, spans};
use crate::{
    FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
    outline,
};

const R: Language = Language::Rust;

fn fold(start_line: usize, end_line: usize, kind: FoldKind) -> FoldRange {
    FoldRange { start_line, end_line, kind }
}

// ------------------------------------------------------------------------------------------
// Highlighting

#[test]
fn keywords_and_declarations() {
    let text = "pub async fn main() -> Result<(), Error> { let mut x = 5u8; return; }";
    assert_kind(R, text, "pub", K::Keyword);
    assert_kind(R, text, "async", K::Keyword);
    assert_kind(R, text, "fn", K::Keyword);
    assert_kind(R, text, "main", K::Function);
    assert_kind(R, text, "->", K::Operator);
    assert_kind(R, text, "Result", K::Type);
    assert_kind(R, text, "Error", K::Type);
    assert_kind(R, text, "let", K::Keyword);
    assert_kind(R, text, "mut", K::Keyword);
    assert_kind(R, text, "x", K::Ident);
    assert_kind(R, text, "=", K::Operator);
    assert_kind(R, text, "5u8", K::Number);
    assert_kind(R, text, ";", K::Punctuation);
    assert_kind(R, text, "return", K::Keyword);
}

#[test]
fn reserved_and_contextual_keywords() {
    assert_kind(R, "x.await; dyn Trait; try { } yield", "await", K::Keyword);
    assert_kind(R, "x.await; dyn Trait; try { } yield", "dyn", K::Keyword);
    assert_kind(R, "x.await; dyn Trait; try { } yield", "try", K::Keyword);
    assert_kind(R, "x.await; dyn Trait; try { } yield", "yield", K::Keyword);
    assert_kind(R, "union Bits { a: u8 }", "union", K::Keyword);
    assert_kind(R, "union Bits { a: u8 }", "Bits", K::Type);
    assert_kind(R, "a.union(&b)", "union", K::Function);
    assert_kind(R, "let union = 1;", "union", K::Ident);
    assert_kind(R, "let p = &raw const x;", "raw", K::Keyword);
    assert_kind(R, "let raw = 1;", "raw", K::Ident);
    assert_kind(R, "default fn f() {}", "default", K::Keyword);
    assert_kind(R, "auto trait Send {}", "auto", K::Keyword);
    assert_kind(R, "extern { safe fn f(); }", "safe", K::Keyword);
    // `gen` is only reserved in edition 2024; `rng.gen()` is too common to flag.
    assert_kind(R, "rng.gen()", "gen", K::Function);
}

#[test]
fn literals_booleans_self() {
    let text = "if true && !false { self.x = Self::new(crate::A, super::B) }";
    assert_kind(R, text, "true", K::Boolean);
    assert_kind(R, text, "false", K::Boolean);
    assert_kind(R, text, "self", K::SelfKeyword);
    assert_kind(R, text, "Self", K::SelfKeyword);
    assert_kind(R, text, "crate", K::SelfKeyword);
    assert_kind(R, text, "super", K::SelfKeyword);
    assert_kind(R, text, "&&", K::Operator);
    assert_kind(R, text, "!", K::Operator);
}

#[test]
fn primitive_types() {
    for prim in ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i64", "isize", "f32", "f64", "bool", "char", "str"]
    {
        let text = format!("let x: {prim} = y;");
        assert_kind(R, &text, prim, K::Type);
    }
    assert_kind(R, "u8::MAX", "u8", K::Type);
}

#[test]
fn macros() {
    let text = "println!(\"{}\", x); vec![1]; m!{}; a != b;";
    assert_kind(R, text, "println!", K::Macro);
    assert_kind(R, text, "vec!", K::Macro);
    assert_kind(R, text, "m!", K::Macro);
    assert_kind(R, text, "!=", K::Operator);
    let def = "macro_rules! my_macro { ($x:expr) => { $x }; }";
    assert_kind(R, def, "macro_rules!", K::Macro);
    assert_kind(R, def, "my_macro", K::Macro);
    assert_kind(R, def, "$", K::Punctuation);
    assert_kind(R, def, "expr", K::Ident);
    assert_kind(R, def, "=>", K::Operator);
    // A space before `!` or no bracket after it: not a macro.
    assert_kind(R, "x ! (y)", "x", K::Ident);
    assert_kind(R, "r#try!(x)", "r#try!", K::Macro);
}

#[test]
fn functions_fields_types_constants() {
    let text = "fn decl() { foo(1); x.method(); x.field; Vec::new(); v.iter().collect::<Vec<_>>(); }";
    assert_kind(R, text, "decl", K::Function);
    assert_kind(R, text, "foo", K::Function);
    assert_kind(R, text, "method", K::Function);
    assert_kind(R, text, "field", K::Field);
    assert_kind(R, text, "Vec", K::Type);
    assert_kind(R, text, "new", K::Function);
    assert_kind(R, text, "iter", K::Function);
    assert_kind(R, text, "collect", K::Function);
    assert_kind(R, "generic::<T>(1)", "generic", K::Function);
    assert_kind(R, "path::<T>::Assoc", "path", K::Ident);
    assert_kind(R, "f::<fn() -> u8>()", "f", K::Function);

    let consts = "const MAX_LEN: usize = 2; static A1: u8 = 0; let t: T = X; Some(v); None";
    assert_kind(R, consts, "MAX_LEN", K::Constant);
    assert_kind(R, consts, "A1", K::Constant);
    assert_kind(R, consts, "T", K::Type);
    assert_kind(R, consts, "X", K::Type);
    assert_kind(R, consts, "Some", K::Type);
    assert_kind(R, consts, "None", K::Type);

    let decls = "struct lower; enum e {} trait t {} type alias = u8;";
    assert_kind(R, decls, "lower", K::Type);
    assert_kind(R, decls, "e", K::Type);
    assert_kind(R, decls, "t", K::Type);
    assert_kind(R, decls, "alias", K::Type);
    assert_kind(R, "fn Odd() {}", "Odd", K::Function);
}

#[test]
fn range_end_is_not_a_field() {
    assert_kind(R, "for i in 0..len {}", "len", K::Ident);
    assert_kind(R, "a.0.b", "b", K::Field);
    assert_kind(R, "1.max(2)", "max", K::Function);
}

#[test]
fn raw_identifiers() {
    assert_kind(R, "let r#type = 1;", "r#type", K::Ident);
    assert_kind(R, "r#match(x)", "r#match", K::Function);
    assert_kind(R, "fn r#fn() {}", "r#fn", K::Function);
}

#[test]
fn lifetimes_and_chars() {
    let text = "fn f<'a>(x: &'a str) -> &'static str { 'outer: loop { break 'outer; } let c = 'c'; let b = b'\\n'; }";
    assert_kind(R, text, "'a", K::Lifetime);
    assert_kind(R, text, "'static", K::Lifetime);
    assert_kind(R, text, "'outer", K::Lifetime);
    assert_kind(R, text, "'c'", K::Char);
    assert_kind(R, text, "b'\\n'", K::Char);
}

#[test]
fn attributes_cover_their_contents() {
    let text = "#[derive(Debug)]\n#![allow(dead_code)]\n#[cfg(all(unix, not(test)))]\n#[doc = \"a ] b\"]\nfn f() {}";
    assert_kind(R, text, "#[derive(Debug)]", K::Attribute);
    assert_kind(R, text, "#![allow(dead_code)]", K::Attribute);
    // Whitespace separates spans; every token inside is Attribute.
    for (line, len) in [(2, 28), (3, 16)] {
        let h = checked(R, text);
        assert!(h.line(line).iter().all(|s| s.kind == K::Attribute), "line {line}");
        assert_eq!(h.line(line).last().map(|s| s.end), Some(len));
    }
    assert_kind(R, text, "fn", K::Keyword);
    // Multi-line attribute.
    let multi = "#[cfg_attr(\n    feature = \"x\",\n    derive(Clone)\n)]\nstruct S;";
    assert_eq!(kind_at(R, multi, 1, 4), K::Attribute);
    assert_eq!(kind_at(R, multi, 3, 1), K::Attribute);
    assert_eq!(kind_of(R, multi, "S"), Some(K::Type));
    // Unterminated: only the opener is marked, the rest is lexed normally.
    let open = "#[derive(Debug)\nfn g() {}";
    assert_kind(R, open, "#[", K::Attribute);
    assert_kind(R, open, "g", K::Function);
    // A lone `#` (e.g. in macro patterns) is punctuation.
    assert_kind(R, "quote! { #x }", "#", K::Punctuation);
}

#[test]
fn comments_and_doc_comments() {
    let text = "// plain\n/// outer doc\n//! inner doc\n//// four slashes\n/** block doc */\n/*! inner block */\n/* plain block */\n/**/";
    assert_kind(R, text, "// plain", K::Comment);
    assert_kind(R, text, "/// outer doc", K::DocComment);
    assert_kind(R, text, "//! inner doc", K::DocComment);
    assert_kind(R, text, "//// four slashes", K::Comment);
    assert_kind(R, text, "/** block doc */", K::DocComment);
    assert_kind(R, text, "/*! inner block */", K::DocComment);
    assert_kind(R, text, "/* plain block */", K::Comment);
    assert_kind(R, text, "/**/", K::Comment);
}

#[test]
fn nested_multiline_block_comment_is_split_per_line() {
    let text = "a /* one\n /* two */ still\n end */ b";
    let h = checked(R, text);
    assert_eq!(h.line(0).last().map(|s| (s.start, s.end, s.kind)), Some((2, 8, K::Comment)));
    assert_eq!(h.line(1).iter().map(|s| (s.start, s.end, s.kind)).collect::<Vec<_>>(), [(0, 16, K::Comment)]);
    assert_eq!(h.line(2).first().map(|s| (s.start, s.end, s.kind)), Some((0, 7, K::Comment)));
    assert_eq!(kind_at(R, text, 2, 8), K::Ident);
    // Unbalanced nesting runs to the end of the text.
    let open = "/* /* */\nfn f() {}";
    assert_eq!(kind_at(R, open, 1, 0), K::Comment);
}

#[test]
fn strings_of_every_flavour() {
    let text =
        r###"let a = "s\"q"; let b = r#"raw "q""#; let c = b"by"; let d = br"rb"; let e = c"cs"; let f = cr#"x"#;"###;
    assert_kind(R, text, r#""s\"q""#, K::String);
    assert_kind(R, text, r##"r#"raw "q""#"##, K::String);
    assert_kind(R, text, r#"b"by""#, K::String);
    assert_kind(R, text, r#"br"rb""#, K::String);
    assert_kind(R, text, r#"c"cs""#, K::String);
    assert_kind(R, text, r##"cr#"x"#"##, K::String);
}

#[test]
fn multiline_raw_string_is_split_per_line() {
    let text = "let s = r#\"first\n \"quoted\" // not a comment\nlast\"#; x";
    let h = checked(R, text);
    assert_eq!(h.line(0).last().map(|s| (s.start, s.end, s.kind)), Some((8, 16, K::String)));
    assert_eq!(h.line(1).iter().map(|s| (s.start, s.end, s.kind)).collect::<Vec<_>>(), [(0, 26, K::String)]);
    assert_eq!(h.line(2).first().map(|s| (s.start, s.end, s.kind)), Some((0, 6, K::String)));
    assert_eq!(kind_at(R, text, 2, 6), K::Punctuation);
    assert_eq!(kind_at(R, text, 2, 8), K::Ident);
}

#[test]
fn numbers() {
    for num in ["0xFF", "0o17", "0b1010_0101", "1_000", "1e10", "2.5f32", "3.0", "7usize", "1E-3"] {
        assert_kind(R, &format!("let n = {num};"), num, K::Number);
    }
}

#[test]
fn unterminated_literals_are_invalid() {
    assert_kind(R, "let s = \"abc", "let", K::Keyword);
    assert_eq!(kind_at(R, "let s = \"abc\nnext", 1, 0), K::Invalid);
    assert_kind(R, "let s = r#\"abc", "r#\"abc", K::Invalid);
    assert_kind(R, "let s = b\"abc", "b\"abc", K::Invalid);
    assert_kind(R, "let c = '\\n", "'\\n", K::Invalid);
    assert_kind(R, "let n = 0x;", "0x", K::Invalid);
    assert_kind(R, "let n = 1e;", "1e", K::Invalid);
    assert_kind(R, "a № b", "№", K::Invalid);
}

#[test]
fn operators_punctuation_brackets() {
    let text = "a += b * c - d / e % f ^ g | h & i << j >> k; x?; y @ z; ~w; p::q, r: s. t";
    for op in ["+=", "*", "-", "/", "%", "^", "|", "&", "<<", ">>", "?", "@", "~"] {
        assert_kind(R, text, op, K::Operator);
    }
    for punct in ["::", ",", ":", "."] {
        assert_kind(R, text, punct, K::Punctuation);
    }
    assert_kind(R, "f(a[0]) {}", "(", K::Bracket);
    assert_kind(R, "f(a[0]) {}", "])", K::Bracket);
}

#[test]
fn non_ascii_uses_char_columns() {
    let text = "let ü = \"ñ€\"; // é😀\nfn 名前() {}";
    let h = checked(R, text);
    let line0: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
    assert_eq!(
        line0,
        [
            (0, 3, K::Keyword),
            (4, 5, K::Ident),
            (6, 7, K::Operator),
            (8, 12, K::String),
            (12, 13, K::Punctuation),
            (14, 19, K::Comment),
        ]
    );
    let line1: Vec<(usize, usize, K)> = h.line(1).iter().map(|s| (s.start, s.end, s.kind)).collect();
    assert_eq!(line1, [(0, 2, K::Keyword), (3, 5, K::Function), (5, 7, K::Bracket), (8, 10, K::Bracket)]);
}

#[test]
fn carriage_returns_do_not_break_lines() {
    let text = "fn a() {}\r\n// c\r\nlet s = \"x\";\r\n";
    let h = checked(R, text);
    assert_eq!(h.len_lines(), 4);
    assert_eq!(kind_at(R, text, 0, 3), K::Function);
    assert_eq!(kind_at(R, text, 2, 8), K::String);
    assert_eq!(folding_ranges(R, "fn a() {\r\n}\r\n"), vec![fold(0, 1, FoldKind::Block)]);
}

#[test]
fn shebang_is_a_comment_but_inner_attribute_is_not() {
    let text = "#!/usr/bin/env rust-script\nfn main() {}";
    assert_kind(R, text, "#!/usr/bin/env rust-script", K::Comment);
    assert_kind(R, text, "main", K::Function);
    assert_kind(R, "#![no_std]", "#![no_std]", K::Attribute);
}

#[test]
fn empty_and_newline_only_texts() {
    assert_eq!(checked(R, "").len_lines(), 1);
    assert_eq!(checked(R, "\n\n").len_lines(), 3);
    assert!(spans(R, "   \n\t").is_empty());
}

// ------------------------------------------------------------------------------------------
// Bracket matching

#[test]
fn matches_brackets_skipping_literals_and_comments() {
    let text = "fn f() { let s = \"}\"; let c = '{'; /* } */ // }\n}";
    let open = offset_of(text, "{", 0);
    let close = text.chars().count() - 1;
    assert_eq!(matching_bracket(R, text, open), Some(close));
    assert_eq!(matching_bracket(R, text, close), Some(open));
    // Just after a bracket.
    assert_eq!(matching_bracket(R, text, close + 1), Some(open));
    assert_eq!(matching_bracket(R, text, 5), Some(4));
    // Inside a string: no match.
    assert_eq!(matching_bracket(R, text, offset_of(text, "\"}\"", 0) + 1), None);
}

#[test]
fn bracket_at_offset_wins_over_previous() {
    let text = "(a)(b)";
    assert_eq!(matching_bracket(R, text, 3), Some(5));
    assert_eq!(matching_bracket(R, text, 2), Some(0));
    // Unbalanced at the offset: fall back to the bracket before it.
    assert_eq!(matching_bracket(R, "(a)(", 3), Some(0));
}

#[test]
fn angle_brackets_are_not_matched_and_unbalanced_is_none() {
    assert_eq!(matching_bracket(R, "Vec<u8>", 3), None);
    assert_eq!(matching_bracket(R, "fn f( {", 4), None);
    assert_eq!(matching_bracket(R, "( ]", 0), None);
    assert_eq!(matching_bracket(R, "()", 99), None);
    assert_eq!(matching_bracket(R, "", 0), None);
}

#[test]
fn bracket_offsets_are_chars() {
    let text = "let é = (\"ü\", [1]);";
    let open = offset_of(text, "(", 0);
    assert_eq!(open, 8);
    assert_eq!(matching_bracket(R, text, open), Some(text.chars().count() - 2));
    assert_eq!(matching_bracket(R, text, offset_of(text, "[", 0)), Some(offset_of(text, "]", 0)));
}

#[test]
fn attribute_brackets_match() {
    let text = "#[derive(Debug)]";
    assert_eq!(matching_bracket(R, text, 1), Some(15));
    assert_eq!(matching_bracket(R, text, 8), Some(14));
}

// ------------------------------------------------------------------------------------------
// Folding

#[test]
fn folds_blocks_comments_and_imports() {
    let text = "\
use std::fmt;
use std::{
    io,
    path::Path,
};
pub use crate::x;

// first
// second
fn main() {
    let v = vec![
        1, // trailing
        2,
    ];
    /* block
       comment */
}
/// doc a
/// doc b
// plain after doc
struct S;
";
    assert_eq!(
        folding_ranges(R, text),
        vec![
            fold(0, 5, FoldKind::Imports),
            fold(1, 4, FoldKind::Block),
            fold(7, 8, FoldKind::Comment),
            fold(9, 16, FoldKind::Block),
            fold(10, 13, FoldKind::Block),
            fold(14, 15, FoldKind::Comment),
            fold(17, 18, FoldKind::Comment),
        ]
    );
}

#[test]
fn single_use_and_single_line_blocks_do_not_fold() {
    assert!(folding_ranges(R, "use a::b;\nfn f() {}\n").is_empty());
    assert!(folding_ranges(R, "// only one\nfn f() { g() }").is_empty());
}

#[test]
fn imports_with_attributes_and_comments_between() {
    let text = "use a;\n// note\n\nuse b;\n#[cfg(test)]\nuse c;\nuse d;";
    assert_eq!(folding_ranges(R, text), vec![fold(0, 3, FoldKind::Imports), fold(5, 6, FoldKind::Imports)]);
    // `use<'a>` precise capturing is not an import.
    assert!(folding_ranges(R, "fn f() -> impl Sized + use<'a> {}\nfn g() -> impl Sized + use<> {}").is_empty());
}

#[test]
fn trailing_comments_do_not_join_runs() {
    let text = "let a = 1; // x\n// y\nlet b = 2; // z";
    assert!(folding_ranges(R, text).is_empty());
}

#[test]
fn unbalanced_brackets_still_fold_what_they_can() {
    let text = "fn f() {\n    (\n}\nfn g() {\n}\n";
    assert_eq!(folding_ranges(R, text), vec![fold(0, 2, FoldKind::Block), fold(3, 4, FoldKind::Block)]);
}

// ------------------------------------------------------------------------------------------
// Outline

const OUTLINE_SRC: &str = "\
//! Crate docs
use std::fmt;

pub struct Foo<T> { x: T }
enum Kind { A, B }
pub(crate) trait Shape: Debug {
    const SIDES: u32;
    type Output;
    fn area(&self) -> f64;
    fn provided(&self) { fn inner() {} struct Hidden; }
}
impl<T: Clone + Fn() -> u8> fmt::Display for Foo<T> where T: Debug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }
}
impl Kind {
    pub const fn new() -> Self { Kind::A }
    async unsafe fn run() {}
}
mod inner {
    pub static mut COUNTER: u32 = 0;
    pub type Alias = Vec<u8>;
    extern \"C\" { fn ext(); }
    mod deeper { fn leaf() {} }
}
mod decl;
#[cfg(test)]
macro_rules! my_macro { () => { fn not_an_item() {} } }
const LIMIT: [u8; 2] = { [1, 2] };
const _: () = ();
unsafe extern \"C\" fn callback() {}
union U { a: u8 }
lazy_static! { static ref HIDDEN: u8 = 1; }
";

fn item(name: &str, kind: SymbolKind, line: usize, column: usize, depth: usize) -> OutlineItem {
    OutlineItem { name: name.to_string(), kind, line, column, depth }
}

#[test]
fn outline_lists_items_with_depth() {
    use SymbolKind as S;
    assert_eq!(
        outline(R, OUTLINE_SRC),
        vec![
            item("Foo", S::Struct, 3, 11, 0),
            item("Kind", S::Enum, 4, 5, 0),
            item("Shape", S::Trait, 5, 17, 0),
            item("SIDES", S::Const, 6, 10, 1),
            item("Output", S::TypeAlias, 7, 9, 1),
            item("area", S::Function, 8, 7, 1),
            item("provided", S::Function, 9, 7, 1),
            item("impl fmt::Display for Foo<T>", S::Impl, 11, 0, 0),
            item("fmt", S::Function, 12, 7, 1),
            item("impl Kind", S::Impl, 14, 0, 0),
            item("new", S::Function, 15, 17, 1),
            item("run", S::Function, 16, 20, 1),
            item("inner", S::Module, 18, 4, 0),
            item("COUNTER", S::Static, 19, 19, 1),
            item("Alias", S::TypeAlias, 20, 13, 1),
            item("ext", S::Function, 21, 20, 1),
            item("deeper", S::Module, 22, 8, 1),
            item("leaf", S::Function, 22, 20, 2),
            item("decl", S::Module, 24, 4, 0),
            item("my_macro", S::Macro, 26, 13, 0),
            item("LIMIT", S::Const, 27, 6, 0),
            item("callback", S::Function, 29, 21, 0),
            item("U", S::Struct, 30, 6, 0),
        ]
    );
}

#[test]
fn outline_impl_names() {
    let names = |text: &str| outline(R, text).into_iter().map(|i| i.name).collect::<Vec<_>>();
    assert_eq!(names("impl<T> From<T> for Wrapper<T> {}"), ["impl From<T> for Wrapper<T>"]);
    assert_eq!(names("unsafe impl Send for X {}"), ["impl Send for X"]);
    assert_eq!(names("impl !Sync for X {}"), ["impl !Sync for X"]);
    assert_eq!(names("impl<'a> Trait for &'a mut [u8]\nwhere\n  Self: Sized {}"), ["impl Trait for &'a mut [u8]"]);
    assert_eq!(names("impl   Spaced   /* c */  for\n  Y {}"), ["impl Spaced for Y"]);
    assert_eq!(names("impl"), ["impl"]);
}

#[test]
fn outline_columns_are_chars() {
    let items = outline(R, "/* é */ fn ü() {}");
    assert_eq!(items, vec![item("ü", SymbolKind::Function, 0, 11, 0)]);
}

#[test]
fn outline_survives_unbalanced_input() {
    let items = outline(R, "impl A {\n fn a() {\n}\nfn b() {}");
    assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["impl A", "a", "b"]);
    assert!(outline(R, "}}} fn").is_empty());
    let after_stray = outline(R, "} fn x() {}");
    assert_eq!(after_stray, vec![item("x", SymbolKind::Function, 0, 5, 0)]);
}
