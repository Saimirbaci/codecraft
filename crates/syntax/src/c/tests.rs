use crate::test_util::{assert_kind, checked, kind_at, kind_of, offset_of, spans};
use crate::{
    FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
    outline,
};

const C: Language = Language::C;
const CPP: Language = Language::Cpp;

fn fold(start_line: usize, end_line: usize, kind: FoldKind) -> FoldRange {
    FoldRange { start_line, end_line, kind }
}

fn names(language: Language, text: &str) -> Vec<(String, SymbolKind, usize)> {
    outline(language, text).into_iter().map(|i| (i.name, i.kind, i.depth)).collect()
}

// ------------------------------------------------------------------------------------------
// Highlighting

#[test]
fn keywords_types_functions() {
    let text = "static const unsigned int count = 0;\nsize_t len(struct node *n) { return n->next ? 1 : 0; }";
    assert_kind(C, text, "static", K::Keyword);
    assert_kind(C, text, "const", K::Keyword);
    assert_kind(C, text, "unsigned", K::Type);
    assert_kind(C, text, "int", K::Type);
    assert_kind(C, text, "count", K::Ident);
    assert_kind(C, text, "0", K::Number);
    assert_kind(C, text, "size_t", K::Type);
    assert_kind(C, text, "len", K::Function);
    assert_kind(C, text, "struct", K::Keyword);
    assert_kind(C, text, "node", K::Type);
    assert_kind(C, text, "return", K::Keyword);
    assert_kind(C, text, "next", K::Field);
    assert_kind(C, text, "->", K::Operator);
    assert_kind(C, text, "{", K::Bracket);
    assert_kind(C, text, ";", K::Punctuation);
}

#[test]
fn constants_fields_calls_booleans() {
    let text = "if (MAX_LEN > p.size && ok(true)) Point q = make(false); x.run();";
    assert_kind(C, text, "MAX_LEN", K::Constant);
    assert_kind(C, text, "size", K::Field);
    assert_kind(C, text, "ok", K::Function);
    assert_kind(C, text, "true", K::Boolean);
    assert_kind(C, text, "false", K::Boolean);
    assert_kind(C, text, "Point", K::Type);
    assert_kind(C, text, "run", K::Function);
    // SCREAMING_CASE calls are macros.
    assert_kind(C, "ASSERT(x);", "ASSERT", K::Macro);
}

#[test]
fn cpp_keywords_and_std_types() {
    let text = "template <typename T> class Box : public Base { std::vector<T> items; std::string name; \
                bool f() const noexcept override { return this != nullptr; } };";
    for kw in ["template", "typename", "class", "public", "const", "noexcept", "override", "return"] {
        assert_kind(CPP, text, kw, K::Keyword);
    }
    assert_kind(CPP, text, "Box", K::Type);
    assert_kind(CPP, text, "vector", K::Type);
    assert_kind(CPP, text, "string", K::Type);
    assert_kind(CPP, text, "this", K::SelfKeyword);
    assert_kind(CPP, text, "nullptr", K::Boolean);
    assert_kind(CPP, text, "f", K::Function);
    // `class` and `nullptr` are plain identifiers in C.
    assert_kind(C, "int class = nullptr;", "class", K::Ident);
    assert_kind(C, "int class = nullptr;", "nullptr", K::Ident);
    // `override` is contextual.
    assert_kind(CPP, "int override = 1;", "override", K::Ident);
    assert_kind(CPP, "class A final : B {};", "final", K::Keyword);
    assert_kind(CPP, "void f() const final;", "final", K::Keyword);
    // Template calls.
    assert_kind(CPP, "auto v = std::make_unique<Foo>(1);", "make_unique", K::Function);
    assert_kind(CPP, "if (a < b) c(d);", "a", K::Ident);
}

#[test]
fn preprocessor_directives() {
    let text = "#include <stdio.h>\n#include \"local.h\"\n  #  define BUF_SIZE 64\n#ifdef DEBUG\n#if defined(X) && Y\n#endif\nint a = BUF_SIZE;";
    assert_kind(C, text, "#include", K::Macro);
    assert_kind(C, text, "<stdio.h>", K::String);
    assert_kind(C, text, "\"local.h\"", K::String);
    assert_kind(C, text, "#  define", K::Macro);
    assert_kind(C, text, "BUF_SIZE", K::Macro);
    assert_kind(C, text, "64", K::Number);
    assert_kind(C, text, "#ifdef", K::Macro);
    assert_kind(C, text, "DEBUG", K::Macro);
    assert_kind(C, text, "defined", K::Keyword);
    assert_kind(C, text, "#endif", K::Macro);
    // Uses of a macro defined in the file.
    assert_eq!(kind_at(C, text, 6, 8), K::Macro);
    // `#` that is not at the start of a line is an operator (stringification).
    assert_kind(C, "#define S(x) #x\nint b = a # c;", "#", K::Operator);
    assert_kind(C, "#error don't do this\nint x;", "don't do this", K::String);
}

#[test]
fn directive_continuation_lines() {
    let text = "#define TWICE(x) \\\n  ((x) + (x))\nint y;";
    let h = checked(C, text);
    assert_eq!(kind_at(C, text, 0, 8), K::Macro);
    assert!(!h.line(1).is_empty());
    // The continuation line is still part of the directive, so `#` there is an operator.
    assert_kind(C, "#define S(x) \\\n  #x\n", "#", K::Operator);
}

#[test]
fn if_zero_is_a_comment() {
    let text = "#if 0\nint dead() { don't }\n#if 1\nnested();\n#endif\n#else\nint live;\n#endif";
    assert_eq!(kind_at(C, text, 1, 0), K::Comment);
    assert_eq!(kind_at(C, text, 3, 0), K::Comment);
    assert_eq!(kind_at(C, text, 4, 1), K::Comment);
    assert_eq!(kind_at(C, text, 5, 0), K::Macro);
    assert_kind(C, text, "int", K::Type);
    // Empty region and a trailing comment on the `#if 0` line.
    assert_eq!(kind_at(C, "#if 0 // off\n#endif\nint x;", 1, 0), K::Macro);
    assert_eq!(kind_at(C, "#if 0\n#endif\nint x;", 2, 0), K::Type);
}

#[test]
fn strings_chars_and_prefixes() {
    let text = r#"auto a = "x\"y"; auto b = L"w"; auto c = u8"u"; char d = '\n'; wchar_t e = L'x'; auto s = "lit"s;"#;
    assert_kind(CPP, text, r#""x\"y""#, K::String);
    assert_kind(CPP, text, "L\"w\"", K::String);
    assert_kind(CPP, text, "u8\"u\"", K::String);
    assert_kind(CPP, text, "'\\n'", K::Char);
    assert_kind(CPP, text, "L'x'", K::Char);
    assert_kind(CPP, text, "\"lit\"s", K::String);
    assert_kind(C, "char *s = \"open\nint x;", "\"open", K::Invalid);
    assert_kind(C, "int x = 'a", "'a", K::Invalid);
}

#[test]
fn raw_strings_span_lines() {
    let text = "auto r = R\"sql(SELECT \"x\" )\n -- )\" not yet\n)sql\"; int z;";
    let all = spans(CPP, text);
    assert!(all.contains(&("R\"sql(SELECT \"x\" )".into(), K::String)), "{all:?}");
    assert!(all.contains(&(" -- )\" not yet".into(), K::String)), "{all:?}");
    assert!(all.contains(&(")sql\"".into(), K::String)), "{all:?}");
    assert_kind(CPP, text, "int", K::Type);
    assert_kind(CPP, "auto r = LR\"(a)\";", "LR\"(a)\"", K::String);
    // Unterminated: runs to the end, flagged.
    assert_eq!(kind_at(CPP, "R\"x(abc\nint y;", 1, 0), K::Invalid);
    // A bad delimiter flags the line only.
    assert_eq!(kind_at(CPP, "R\"a b(x)a b\";\nint y;", 1, 0), K::Type);
}

#[test]
fn numbers() {
    let text = "x = 0x1Fu + 0b1010 + 017 + 1'000'000 + 1.5e-3f + .5 + 10ULL + 0x1.8p3;";
    for n in ["0x1Fu", "0b1010", "017", "1'000'000", "1.5e-3f", ".5", "10ULL", "0x1.8p3"] {
        assert_kind(CPP, text, n, K::Number);
    }
    assert_kind(CPP, "a.b.c", "b", K::Field);
}

#[test]
fn comments_and_doc_comments() {
    let text =
        "/// Doc\n//! Inner\n//// banner\n// plain\n/** block doc */\n/*! qt */\n/* plain */\n/**/\n/*** stars */";
    assert_kind(C, text, "/// Doc", K::DocComment);
    assert_kind(C, text, "//! Inner", K::DocComment);
    assert_kind(C, text, "//// banner", K::Comment);
    assert_kind(C, text, "// plain", K::Comment);
    assert_kind(C, text, "/** block doc */", K::DocComment);
    assert_kind(C, text, "/*! qt */", K::DocComment);
    assert_kind(C, text, "/* plain */", K::Comment);
    assert_kind(C, text, "/**/", K::Comment);
    assert_kind(C, text, "/*** stars */", K::Comment);
    // A backslash continues a line comment.
    assert_eq!(kind_at(C, "// a \\\nstill comment\nint x;", 1, 0), K::Comment);
}

#[test]
fn attributes() {
    let text = "[[nodiscard]] int f(); [[deprecated(\"no\")]] void g(); int v = a[b[0]];";
    assert_kind(CPP, text, "[[nodiscard]]", K::Attribute);
    assert_kind(CPP, text, "[[deprecated(\"no\")]]", K::Attribute);
    assert_kind(CPP, text, "[", K::Bracket);
}

#[test]
fn non_ascii_and_crlf() {
    let h = checked(C, "char *s = \"ü\"; // ä\r\nint x;\r\n");
    let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
    assert_eq!(
        cols,
        [
            (0, 4, K::Type),
            (5, 6, K::Operator),
            (6, 7, K::Ident),
            (8, 9, K::Operator),
            (10, 13, K::String),
            (13, 14, K::Punctuation),
            (15, 20, K::Comment)
        ]
    );
    assert_eq!(kind_of(C, "int é = 1;", "é"), Some(K::Ident));
}

#[test]
fn brackets_skip_strings_comments_and_chars() {
    let text = "int f() {\n  char c = '}'; /* } */ puts(\"}\");\n}";
    assert_eq!(matching_bracket(C, text, offset_of(text, "{", 0)), Some(text.chars().count() - 1));
    assert_eq!(matching_bracket(C, text, 5), Some(6));
    let raw = "auto s = R\"(})\"; { }";
    assert_eq!(matching_bracket(CPP, raw, offset_of(raw, "{", 0)), Some(raw.chars().count() - 1));
}

// ------------------------------------------------------------------------------------------
// Folding

#[test]
fn folds_blocks_comments_includes_and_conditionals() {
    let text = "\
#include <a.h>
#include <b.h>

#include \"c.h\"
// one
// two
#ifdef X
int f(void) {
  return 0;
}
#endif
/* a
   b */
#pragma region Helpers
int g;
#pragma endregion
";
    assert_eq!(
        folding_ranges(C, text),
        vec![
            fold(0, 3, FoldKind::Imports),
            fold(4, 5, FoldKind::Comment),
            fold(6, 10, FoldKind::Block),
            fold(7, 9, FoldKind::Block),
            fold(11, 12, FoldKind::Comment),
            fold(13, 15, FoldKind::Block),
        ]
    );
}

#[test]
fn macro_braces_do_not_break_block_folds() {
    let text = "#define BEGIN {\nvoid f() {\n  x();\n}\n";
    assert_eq!(folding_ranges(C, text), vec![fold(1, 3, FoldKind::Block)]);
}

// ------------------------------------------------------------------------------------------
// Outline

#[test]
fn c_outline() {
    let text = "\
#define MAX 10
typedef unsigned long ulong;
typedef struct node { int v; } node_t;
typedef void (*handler)(int);
struct point { int x, y; };
enum color { RED, GREEN };
union value { int i; float f; };
int prototype(int a);
static int helper(int a) {
    struct inner { int z; };
    return a;
}
struct point make_point(void) { struct point p = {0}; return p; }
int table[] = { 1, 2 };
";
    assert_eq!(
        names(C, text),
        vec![
            ("MAX".into(), SymbolKind::Macro, 0),
            ("ulong".into(), SymbolKind::TypeAlias, 0),
            ("node".into(), SymbolKind::Struct, 0),
            ("node_t".into(), SymbolKind::TypeAlias, 0),
            ("handler".into(), SymbolKind::TypeAlias, 0),
            ("point".into(), SymbolKind::Struct, 0),
            ("color".into(), SymbolKind::Enum, 0),
            ("value".into(), SymbolKind::Struct, 0),
            ("helper".into(), SymbolKind::Function, 0),
            ("make_point".into(), SymbolKind::Function, 0),
        ]
    );
    let items = outline(C, text);
    assert_eq!(items[0], OutlineItem { name: "MAX".into(), kind: SymbolKind::Macro, line: 0, column: 8, depth: 0 });
    assert_eq!(items[8].line, 8);
    assert_eq!(items[8].column, 11);
}

#[test]
fn cpp_outline() {
    let text = "\
namespace app::core {
template <typename T>
class Widget : public Base<T>, private Other {
public:
    Widget(int x) : x_(x), y_{x} {}
    ~Widget() override {}
    void draw() const;
    int size() const noexcept { return 0; }
    bool operator==(const Widget& o) const { return true; }
    struct Inner { void go() {} };
private:
    int x_ = 0;
};
using Id = unsigned;
enum class Mode : int { A, B };
void Widget::draw() const {
    auto f = [](int a) { return a; };
}
}
extern \"C\" {
int c_api(void) { return 1; }
}
namespace {
void hidden() {}
}
";
    assert_eq!(
        names(CPP, text),
        vec![
            ("app::core".into(), SymbolKind::Module, 0),
            ("Widget".into(), SymbolKind::Struct, 1),
            ("Widget".into(), SymbolKind::Function, 2),
            ("~Widget".into(), SymbolKind::Function, 2),
            ("size".into(), SymbolKind::Function, 2),
            ("operator==".into(), SymbolKind::Function, 2),
            ("Inner".into(), SymbolKind::Struct, 2),
            ("go".into(), SymbolKind::Function, 3),
            ("Id".into(), SymbolKind::TypeAlias, 1),
            ("Mode".into(), SymbolKind::Enum, 1),
            ("Widget::draw".into(), SymbolKind::Function, 1),
            ("c_api".into(), SymbolKind::Function, 0),
            ("hidden".into(), SymbolKind::Function, 0),
        ]
    );
}

#[test]
fn outline_ignores_uses_of_types_and_declarations() {
    let text = "struct Foo;\nstruct Foo *make(void);\nstruct Foo foo = { 0 };\nclass Bar;\nfriend class Baz;\n";
    assert!(outline(CPP, text).is_empty(), "{:?}", outline(CPP, text));
}

#[test]
fn every_function_handles_empty_and_garbage() {
    for language in [C, CPP] {
        for text in ["", "#", "#define", "R\"", "'", "\"", "/*", "}", "{{{", "operator", "class", "typedef", "\\"] {
            let _ = checked(language, text);
            let _ = folding_ranges(language, text);
            let _ = outline(language, text);
            let _ = matching_bracket(language, text, 0);
        }
    }
}
