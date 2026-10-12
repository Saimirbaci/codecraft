use super::*;
use crate::EditGroup;
use crate::test_util::{parse, render, run};

fn cfg() -> OpsConfig {
    OpsConfig::default()
}

fn tabs() -> OpsConfig {
    OpsConfig { indent_unit: "\t".into(), ..OpsConfig::default() }
}

fn no_close() -> OpsConfig {
    OpsConfig { auto_close: false, ..OpsConfig::default() }
}

fn typed(spec: &str, ch: char) -> String {
    run(spec, |d, s| type_char(d, s, ch, &cfg()))
}

fn typed_all(spec: &str, chars: &str) -> String {
    let (mut doc, mut sels) = parse(spec);
    for ch in chars.chars() {
        let edit = type_char(&doc, &sels, ch, &cfg());
        doc.apply(&edit, &sels, EditGroup::Typing).unwrap();
        sels = edit.selections;
    }
    render(&doc, &sels)
}

// ---- insert_text ----------------------------------------------------------------------------------

#[test]
fn insert_text_at_carets_and_selections() {
    assert_eq!(run("ab|c", |d, s| insert_text(d, s, "XY")), "abXY|c");
    assert_eq!(run("a«bc»d", |d, s| insert_text(d, s, "X")), "aX|d");
    assert_eq!(run("a|b|c", |d, s| insert_text(d, s, "-")), "a-|b-|c");
    assert_eq!(run("|", |d, s| insert_text(d, s, "hi")), "hi|");
    assert_eq!(run("é|😀", |d, s| insert_text(d, s, "ü")), "éü|😀");
}

#[test]
fn insert_text_normalizes_crlf() {
    assert_eq!(run("a|b", |d, s| insert_text(d, s, "x\r\ny\rz")), "ax\ny\nz|b");
}

#[test]
fn insert_text_clamps_out_of_range_selections() {
    let doc = Document::from_text("abc");
    let sels = SelectionSet::from_vec(vec![Selection::new(usize::MAX, 99), Selection::caret(1)], 0);
    let edit = insert_text(&doc, &sels, "!");
    let mut doc2 = doc.clone();
    doc2.apply(&edit, &sels, EditGroup::Standalone).unwrap();
    assert_eq!(doc2.text(), "a!bc!");
    assert_eq!(render(&doc2, &edit.selections), "a!|bc!|");
}

// ---- type_char ------------------------------------------------------------------------------------

#[test]
fn type_char_auto_closes_brackets_before_space_closer_or_eol() {
    assert_eq!(typed("foo|", '('), "foo(|)");
    assert_eq!(typed("foo| bar", '['), "foo[|] bar");
    assert_eq!(typed("(|)", '{'), "({|})");
    assert_eq!(typed("x|\ny", '{'), "x{|}\ny");
    assert_eq!(typed("|foo", '('), "(|foo");
    assert_eq!(typed("|,", '('), "(|,");
}

#[test]
fn type_char_types_over_closers_and_quotes() {
    assert_eq!(typed("(|)", ')'), "()|");
    assert_eq!(typed("[a|]", ']'), "[a]|");
    assert_eq!(typed("(|]", ')'), "()|]");
    assert_eq!(typed("\"abc|\"", '"'), "\"abc\"|");
    assert_eq!(typed("'a|'", '\''), "'a'|");
    assert_eq!(typed_all("|", "(x)"), "(x)|");
    assert_eq!(typed_all("|", "\"s\""), "\"s\"|");
}

#[test]
fn type_char_double_quote_rules() {
    assert_eq!(typed("x = |", '"'), "x = \"|\"");
    assert_eq!(typed("r|", '"'), "r\"|");
    assert_eq!(typed("'|", '"'), "'\"|");
    assert_eq!(typed("|abc", '"'), "\"|abc");
    assert_eq!(typed("(|)", '"'), "(\"|\")");
}

#[test]
fn type_char_single_quote_skips_lifetimes() {
    assert_eq!(typed("&|", '\''), "&'|");
    assert_eq!(typed("Foo<|", '\''), "Foo<'|");
    assert_eq!(typed("b|", '\''), "b'|");
    assert_eq!(typed("x = |", '\''), "x = '|'");
    assert_eq!(typed("(|)", '\''), "('|')");
}

#[test]
fn type_char_wraps_selection() {
    assert_eq!(typed("«foo»", '('), "(«foo»)");
    assert_eq!(typed("»foo«", '['), "[»foo«]");
    assert_eq!(typed("a «b» c", '"'), "a \"«b»\" c");
    assert_eq!(typed("«x»", '\''), "'«x»'");
    assert_eq!(typed("«foo»", 'z'), "z|");
}

#[test]
fn type_char_without_auto_close() {
    assert_eq!(run("foo|", |d, s| type_char(d, s, '(', &no_close())), "foo(|");
    assert_eq!(run("(|)", |d, s| type_char(d, s, ')', &no_close())), "()|)");
    assert_eq!(run("«foo»", |d, s| type_char(d, s, '(', &no_close())), "(|");
}

#[test]
fn type_char_multi_caret() {
    assert_eq!(typed("a|\nb|", '('), "a(|)\nb(|)");
    assert_eq!(typed("a|b|", 'x'), "ax|bx|");
    assert_eq!(typed("(|)(|)", ')'), "()|()|");
    assert_eq!(typed("«a» «b»", '{'), "{«a»} {«b»}");
}

#[test]
fn type_char_newline_char_is_normalized() {
    assert_eq!(typed("a|b", '\r'), "a\n|b");
}

// ---- backspace / delete ---------------------------------------------------------------------------

#[test]
fn backspace_basics() {
    assert_eq!(run("ab|c", |d, s| backspace(d, s, &cfg())), "a|c");
    assert_eq!(run("|abc", |d, s| backspace(d, s, &cfg())), "|abc");
    assert_eq!(run("a«bc»d", |d, s| backspace(d, s, &cfg())), "a|d");
    assert_eq!(run("a\n|b", |d, s| backspace(d, s, &cfg())), "a|b");
    assert_eq!(run("|", |d, s| backspace(d, s, &cfg())), "|");
}

#[test]
fn backspace_deletes_empty_pairs() {
    assert_eq!(run("(|)", |d, s| backspace(d, s, &cfg())), "|");
    assert_eq!(run("x[|]y", |d, s| backspace(d, s, &cfg())), "x|y");
    assert_eq!(run("\"|\"", |d, s| backspace(d, s, &cfg())), "|");
    assert_eq!(run("'|'", |d, s| backspace(d, s, &cfg())), "|");
    assert_eq!(run("(|]", |d, s| backspace(d, s, &cfg())), "|]");
    assert_eq!(run("(|)", |d, s| backspace(d, s, &no_close())), "|)");
}

#[test]
fn backspace_in_indentation_goes_to_previous_stop() {
    assert_eq!(run("        |foo", |d, s| backspace(d, s, &cfg())), "    |foo");
    assert_eq!(run("      |foo", |d, s| backspace(d, s, &cfg())), "    |foo");
    assert_eq!(run("    |", |d, s| backspace(d, s, &cfg())), "|");
    assert_eq!(run(" |x", |d, s| backspace(d, s, &cfg())), "|x");
    assert_eq!(run("\t\t|x", |d, s| backspace(d, s, &tabs())), "\t|x");
    assert_eq!(run("  \t|x", |d, s| backspace(d, s, &cfg())), "|x");
    assert_eq!(run("x   |y", |d, s| backspace(d, s, &cfg())), "x  |y");
}

#[test]
fn backspace_multi_caret_overlaps_merge() {
    assert_eq!(run("ab|c|d", |d, s| backspace(d, s, &cfg())), "a|d");
    assert_eq!(run("    |  |x", |d, s| backspace(d, s, &cfg())), "|x");
    assert_eq!(run("a|\nb|\nc|", |d, s| backspace(d, s, &cfg())), "|\n|\n|");
    assert_eq!(run("(|)(|)", |d, s| backspace(d, s, &cfg())), "|");
}

#[test]
fn backspace_and_delete_whole_clusters() {
    assert_eq!(run("a😀|b", |d, s| backspace(d, s, &cfg())), "a|b");
    assert_eq!(run("a👍🏽|b", |d, s| backspace(d, s, &cfg())), "a|b");
    assert_eq!(run("e\u{301}|", |d, s| backspace(d, s, &cfg())), "|");
    assert_eq!(run("x👨\u{200D}👩|", |d, s| backspace(d, s, &cfg())), "x|");
    assert_eq!(run("🇫🇷🇩🇪|", |d, s| backspace(d, s, &cfg())), "🇫🇷|");
    assert_eq!(run("|👍🏽b", delete_forward), "|b");
    assert_eq!(run("|e\u{301}x", delete_forward), "|x");
    assert_eq!(run("|🇫🇷🇩🇪", delete_forward), "|🇩🇪");
}

#[test]
fn backspace_on_crlf_document_keeps_line_ending() {
    let mut doc = Document::from_text("ab\r\ncd\r\n");
    let sels = SelectionSet::caret(3);
    let edit = backspace(&doc, &sels, &cfg());
    doc.apply(&edit, &sels, EditGroup::Typing).unwrap();
    assert_eq!(doc.text(), "abcd\n");
    assert_eq!(doc.to_disk_text(), "abcd\r\n");
    assert_eq!(edit.selections, SelectionSet::caret(2));
}

#[test]
fn delete_forward_basics() {
    assert_eq!(run("a|bc", delete_forward), "a|c");
    assert_eq!(run("abc|", delete_forward), "abc|");
    assert_eq!(run("a|\nb", delete_forward), "a|b");
    assert_eq!(run("«ab»c", delete_forward), "|c");
    assert_eq!(run("|a|b|c", delete_forward), "|");
}

#[test]
fn delete_word_backward_cases() {
    assert_eq!(run("foo bar|", delete_word_backward), "foo |");
    assert_eq!(run("foo   |", delete_word_backward), "foo|");
    assert_eq!(run("foo.bar|", delete_word_backward), "foo.|");
    assert_eq!(run("foo..|", delete_word_backward), "foo|");
    assert_eq!(run("foo\n|bar", delete_word_backward), "foo|bar");
    assert_eq!(run("foo {\n    |bar", delete_word_backward), "foo {|bar");
    assert_eq!(run("|abc", delete_word_backward), "|abc");
    assert_eq!(run("a«bc»", delete_word_backward), "a|");
    assert_eq!(run("one| two|", delete_word_backward), "| |");
}

#[test]
fn delete_word_forward_cases() {
    assert_eq!(run("|foo bar", delete_word_forward), "| bar");
    assert_eq!(run("foo|   bar", delete_word_forward), "foo|bar");
    assert_eq!(run("foo|\n    bar", delete_word_forward), "foo|bar");
    assert_eq!(run("foo|\nbar", delete_word_forward), "foo|bar");
    assert_eq!(run("foo|", delete_word_forward), "foo|");
    assert_eq!(run("|a_b1(", delete_word_forward), "|(");
}

// ---- newline --------------------------------------------------------------------------------------

#[test]
fn newline_keeps_indent() {
    assert_eq!(run("foo|", |d, s| newline(d, s, &cfg())), "foo\n|");
    assert_eq!(run("    foo|", |d, s| newline(d, s, &cfg())), "    foo\n    |");
    assert_eq!(run("\tfo|o", |d, s| newline(d, s, &cfg())), "\tfo\n\t|o");
    assert_eq!(run("  |  foo", |d, s| newline(d, s, &cfg())), "  \n  |  foo");
    assert_eq!(run("|", |d, s| newline(d, s, &cfg())), "\n|");
}

#[test]
fn newline_after_opener_and_between_pair() {
    assert_eq!(run("fn f() {|}", |d, s| newline(d, s, &cfg())), "fn f() {\n    |\n}");
    assert_eq!(run("    if x {|}", |d, s| newline(d, s, &cfg())), "    if x {\n        |\n    }");
    assert_eq!(run("foo(|", |d, s| newline(d, s, &cfg())), "foo(\n    |");
    assert_eq!(run("[|]", |d, s| newline(d, s, &cfg())), "[\n    |\n]");
    assert_eq!(run("x {  |", |d, s| newline(d, s, &cfg())), "x {  \n    |");
    assert_eq!(run("{|)", |d, s| newline(d, s, &cfg())), "{\n    |)");
    assert_eq!(run("\t{|}", |d, s| newline(d, s, &tabs())), "\t{\n\t\t|\n\t}");
}

#[test]
fn newline_replaces_selection_and_handles_multi_caret() {
    assert_eq!(run("a«bc»d", |d, s| newline(d, s, &cfg())), "a\n|d");
    assert_eq!(run("  a|\n  b|", |d, s| newline(d, s, &cfg())), "  a\n  |\n  b\n  |");
}

#[test]
fn start_new_line_cases() {
    assert_eq!(run("fo|o\nbar", start_new_line), "foo\n|\nbar");
    assert_eq!(run("    fo|o", start_new_line), "    foo\n    |");
    assert_eq!(run("fn x() {|", start_new_line), "fn x() {\n    |");
    assert_eq!(run("a|b|c", start_new_line), "abc\n|");
    assert_eq!(run("a|\nb|", start_new_line), "a\n|\nb\n|");
    assert_eq!(run("\tx\n\ty {|", start_new_line), "\tx\n\ty {\n\t\t|");
}

// ---- indent / unindent ----------------------------------------------------------------------------

#[test]
fn indent_inserts_to_next_stop() {
    assert_eq!(run("|foo", |d, s| indent(d, s, &cfg())), "    |foo");
    assert_eq!(run("ab|c", |d, s| indent(d, s, &cfg())), "ab  |c");
    assert_eq!(run("\t|x", |d, s| indent(d, s, &cfg())), "\t    |x");
    assert_eq!(run("|x", |d, s| indent(d, s, &tabs())), "\t|x");
    assert_eq!(run("a«b»c", |d, s| indent(d, s, &cfg())), "a   |c");
    assert_eq!(run("|a|", |d, s| indent(d, s, &cfg())), "    |a   |");
}

#[test]
fn indent_multi_line_selection() {
    assert_eq!(run("«foo\nbar»", |d, s| indent(d, s, &cfg())), "«    foo\n    bar»");
    assert_eq!(run("f«oo\n\nbar\n»baz", |d, s| indent(d, s, &cfg())), "    f«oo\n\n    bar\n»baz");
    assert_eq!(run("»a\nb«", |d, s| indent(d, s, &tabs())), "»\ta\n\tb«");
}

#[test]
fn unindent_removes_one_level() {
    assert_eq!(run("    |foo", |d, s| unindent(d, s, &cfg())), "|foo");
    assert_eq!(run("      foo|", |d, s| unindent(d, s, &cfg())), "  foo|");
    assert_eq!(run("  fo|o", |d, s| unindent(d, s, &cfg())), "fo|o");
    assert_eq!(run("\t\tx|", |d, s| unindent(d, s, &tabs())), "\tx|");
    assert_eq!(run("fo|o", |d, s| unindent(d, s, &cfg())), "fo|o");
    assert_eq!(run("  «a\n    b»\nc", |d, s| unindent(d, s, &cfg())), "«a\nb»\nc");
    assert_eq!(run("  |a|\n  b", |d, s| unindent(d, s, &cfg())), "|a|\n  b");
}

// ---- comments -------------------------------------------------------------------------------------

#[test]
fn line_comment_toggles_and_moves_caret_down() {
    assert_eq!(run("fo|o\nbar", |d, s| toggle_line_comment(d, s, &cfg())), "// foo\nba|r");
    assert_eq!(run("// fo|o\nbar", |d, s| toggle_line_comment(d, s, &cfg())), "foo\nbar|");
    assert_eq!(run("//foo|\nx", |d, s| toggle_line_comment(d, s, &cfg())), "foo\nx|");
    assert_eq!(run("a\nb|", |d, s| toggle_line_comment(d, s, &cfg())), "a\n// b|");
}

#[test]
fn line_comment_at_min_indent_over_selection() {
    let spec = "«    foo\n        bar\n\n»baz";
    assert_eq!(run(spec, |d, s| toggle_line_comment(d, s, &cfg())), "«    // foo\n    //     bar\n\n»baz");
    let commented = "«    // foo\n    //     bar\n»baz";
    assert_eq!(run(commented, |d, s| toggle_line_comment(d, s, &cfg())), "«    foo\n        bar\n»baz");
    // Mixed: comment everything.
    assert_eq!(run("«// a\nb»", |d, s| toggle_line_comment(d, s, &cfg())), "«// // a\n// b»");
}

#[test]
fn line_comment_blank_lines_and_config() {
    assert_eq!(run("|\nx", |d, s| toggle_line_comment(d, s, &cfg())), "// \n|x");
    assert_eq!(run("  |", |d, s| toggle_line_comment(d, s, &cfg())), "  // |");
    let hash = OpsConfig { line_comment: Some("#".into()), ..cfg() };
    assert_eq!(run("«a\nb»", |d, s| toggle_line_comment(d, s, &hash)), "«# a\n# b»");
    let none = OpsConfig { line_comment: None, ..cfg() };
    assert_eq!(run("a|", |d, s| toggle_line_comment(d, s, &none)), "a|");
}

#[test]
fn line_comment_multi_caret() {
    assert_eq!(run("a|\nb|\nc", |d, s| toggle_line_comment(d, s, &cfg())), "// a\n// b|\nc|");
    assert_eq!(run("a|b|\nc", |d, s| toggle_line_comment(d, s, &cfg())), "// ab\nc|");
}

#[test]
fn block_comment_wraps_and_unwraps() {
    assert_eq!(run("«foo»", |d, s| toggle_block_comment(d, s, &cfg())), "/*«foo»*/");
    assert_eq!(run("/*«foo»*/", |d, s| toggle_block_comment(d, s, &cfg())), "«foo»");
    assert_eq!(run("«/*foo*/»", |d, s| toggle_block_comment(d, s, &cfg())), "«foo»");
    assert_eq!(run("»foo«", |d, s| toggle_block_comment(d, s, &cfg())), "/*»foo«*/");
    assert_eq!(run("a|b", |d, s| toggle_block_comment(d, s, &cfg())), "a/*|*/b");
    assert_eq!(run("a/*|*/b", |d, s| toggle_block_comment(d, s, &cfg())), "a|b");
    assert_eq!(run("x /* fo|o */ y", |d, s| toggle_block_comment(d, s, &cfg())), "x  fo|o  y");
    assert_eq!(run("/* a */ b|", |d, s| toggle_block_comment(d, s, &cfg())), "/* a */ b/*|*/");
    let none = OpsConfig { block_comment: None, ..cfg() };
    assert_eq!(run("«a»", |d, s| toggle_block_comment(d, s, &none)), "«a»");
}

// ---- line operations ------------------------------------------------------------------------------

#[test]
fn duplicate_line_and_selection() {
    assert_eq!(run("fo|o\nbar", duplicate), "foo\nfo|o\nbar");
    assert_eq!(run("ab|c", duplicate), "abc\nab|c");
    assert_eq!(run("a«bc»d", duplicate), "abc«bc»d");
    assert_eq!(run("»bc«", duplicate), "bc»bc«");
    assert_eq!(run("a|b|c", duplicate), "abc\na|b|c");
    assert_eq!(run("a|\nb|", duplicate), "a\na|\nb\nb|");
    assert_eq!(run("|", duplicate), "\n|");
    assert_eq!(run("«a\nb»", duplicate), "a\nb«a\nb»");
    assert_eq!(run("|x «y»", duplicate), "|x y«y»");
}

#[test]
fn delete_line_cases() {
    assert_eq!(run("foo\nb|ar\nbaz", delete_line), "foo\nb|az");
    assert_eq!(run("foo\nbar|\nx", delete_line), "foo\nx|");
    assert_eq!(run("foo\nba|r", delete_line), "fo|o");
    assert_eq!(run("on|ly", delete_line), "|");
    assert_eq!(run("foo\nba|r\n", delete_line), "foo\n|");
    assert_eq!(run("a|\nb|\nc", delete_line), "c|");
    assert_eq!(run("a\n«b\nc\n»d", delete_line), "a\n|d");
    assert_eq!(run("a|\nb\nc|", delete_line), "b|");
}

#[test]
fn move_lines_up_and_down() {
    assert_eq!(move_lines_spec("a\nb|\nc", true), Some("b|\na\nc".into()));
    assert_eq!(move_lines_spec("a|\nb", true), None);
    assert_eq!(move_lines_spec("a|\nb\nc", false), Some("b\na|\nc".into()));
    assert_eq!(move_lines_spec("a\nb|", false), None);
    assert_eq!(move_lines_spec("x\n«a\nb\n»c", true), Some("«a\nb\n»x\nc".into()));
    assert_eq!(move_lines_spec("«a\nb\n»c\nd", false), Some("c\n«a\nb\n»d".into()));
    assert_eq!(move_lines_spec("0\na|\nb|\nc", true), Some("a|\nb|\n0\nc".into()));
    assert_eq!(move_lines_spec("0\na|\n1\nb|", true), Some("a|\n0\nb|\n1".into()));
    assert_eq!(move_lines_spec("ab|c\n", false), Some("\nab|c".into()));
}

fn move_lines_spec(spec: &str, up: bool) -> Option<String> {
    let (doc, sels) = parse(spec);
    move_lines(&doc, &sels, up)?;
    Some(run(spec, |d, s| move_lines(d, s, up).unwrap()))
}

#[test]
fn join_lines_cases() {
    assert_eq!(run("fo|o\n    bar", join_lines), "foo| bar");
    assert_eq!(run("foo  |\nbar", join_lines), "foo| bar");
    assert_eq!(run("// a|\n// b", join_lines), "// a| b");
    assert_eq!(run("a|\n// b", join_lines), "a| // b");
    assert_eq!(run("foo|\n\nbar", join_lines), "foo|\nbar");
    assert_eq!(run("|\nbar", join_lines), "|bar");
    assert_eq!(run("«a\nb\n»c", join_lines), "a b| c");
    assert_eq!(run("last|", join_lines), "last|");
    assert_eq!(run("a|\nb|\nc", join_lines), "a b| c");
    assert_eq!(run("a|\nb\nc|\nd", join_lines), "a| b\nc| d");
}

#[test]
fn sort_and_reverse_lines() {
    assert_eq!(run("c\na|\nb\n", sort_lines), "a\nb|\nc\n");
    assert_eq!(run("c\nb\na", reverse_lines), "|a\nb\nc");
    assert_eq!(run("z\n«c\na\n»b", sort_lines), "z\n«a\nc\n»b");
    assert_eq!(run("«3\n1\n2»", reverse_lines), "«2\n1\n3»");
    assert_eq!(run("|", sort_lines), "|");
    assert_eq!(run("a\nb|", sort_lines), "a\nb|");
}

#[test]
fn toggle_case_cases() {
    assert_eq!(run("«Foo»", toggle_case), "«FOO»");
    assert_eq!(run("«FOO»", toggle_case), "«foo»");
    assert_eq!(run("fo|o bar", toggle_case), "FO|O bar");
    assert_eq!(run("«straße»", toggle_case), "«STRASSE»");
    assert_eq!(run("»éa«", toggle_case), "»ÉA«");
    assert_eq!(run(" | ", toggle_case), " | ");
    assert_eq!(run("a| b|", toggle_case), "A| B|");
}

#[test]
fn transpose_cases() {
    assert_eq!(run("ab|c", transpose), "acb|");
    assert_eq!(run("abc|", transpose), "acb|");
    assert_eq!(run("|abc", transpose), "|abc");
    assert_eq!(run("a|", transpose), "a|");
    assert_eq!(run("a|b\nc", transpose), "ba|\nc");
    assert_eq!(run("é|😀", transpose), "😀é|");
}

// ---- clipboard ------------------------------------------------------------------------------------

#[test]
fn copy_text_whole_lines_and_selections() {
    let (doc, sels) = parse("ab|c\nd|ef");
    assert_eq!(copy_text(&doc, &sels), ("abc\ndef\n".into(), true));
    let (doc, sels) = parse("a|b|c\nd");
    assert_eq!(copy_text(&doc, &sels), ("abc\n".into(), true));
    let (doc, sels) = parse("«ab» c «d»|e");
    assert_eq!(copy_text(&doc, &sels), ("ab\nd".into(), false));
    let (doc, sels) = parse("|");
    assert_eq!(copy_text(&doc, &sels), ("\n".into(), true));
}

#[test]
fn cut_removes_what_was_copied() {
    let (doc, sels) = parse("a\nb|\nc");
    let (text, whole, edit) = cut(&doc, &sels);
    assert_eq!((text.as_str(), whole), ("b\n", true));
    assert_eq!(run("a\nb|\nc", |d, s| cut(d, s).2), "a\nc|");
    assert_eq!(run("«ab»c|d", |d, s| cut(d, s).2), "|c|d");
    assert!(!edit.is_noop());
}

#[test]
fn paste_plain_whole_line_and_distributed() {
    assert_eq!(run("a|b", |d, s| paste(d, s, "XY", false)), "aXY|b");
    assert_eq!(run("a«b»c", |d, s| paste(d, s, "X\r\nY", false)), "aX\nY|c");
    assert_eq!(run("a\nb|c", |d, s| paste(d, s, "line\n", true)), "a\nline\nb|c");
    assert_eq!(run("|x", |d, s| paste(d, s, "line", true)), "line\n|x");
    assert_eq!(run("«x»", |d, s| paste(d, s, "line\n", true)), "line\n|");
    assert_eq!(run("a|\nb|", |d, s| paste(d, s, "1\n2", false)), "a1|\nb2|");
    assert_eq!(run("a|\nb|", |d, s| paste(d, s, "1\n2\n", true)), "1\na|\n2\nb|");
    assert_eq!(run("a|\nb|", |d, s| paste(d, s, "1\n2\n3", false)), "a1\n2\n3|\nb1\n2\n3|");
}

#[test]
fn copy_then_paste_round_trip_multi_caret() {
    let (doc, sels) = parse("«one» «two»");
    let (clip, whole) = copy_text(&doc, &sels);
    assert_eq!(run("x| y|", |d, s| paste(d, s, &clip, whole)), "xone| ytwo|");
}

// ---- edge documents -------------------------------------------------------------------------------

#[test]
fn every_op_is_a_noop_or_valid_on_an_empty_document() {
    let doc = Document::new();
    let sels = SelectionSet::caret(0);
    let c = cfg();
    let edits = [
        insert_text(&doc, &sels, ""),
        backspace(&doc, &sels, &c),
        delete_forward(&doc, &sels),
        delete_word_backward(&doc, &sels),
        delete_word_forward(&doc, &sels),
        unindent(&doc, &sels, &c),
        delete_line(&doc, &sels),
        join_lines(&doc, &sels),
        sort_lines(&doc, &sels),
        reverse_lines(&doc, &sels),
        toggle_case(&doc, &sels),
        transpose(&doc, &sels),
        cut(&doc, &sels).2,
    ];
    for e in edits {
        assert!(e.is_noop(), "{e:?}");
        assert_eq!(e.selections, sels);
    }
    assert!(move_lines(&doc, &sels, true).is_none());
    assert!(move_lines(&doc, &sels, false).is_none());
}

#[test]
fn end_of_document_without_trailing_newline() {
    assert_eq!(run("a\nbc|", |d, s| newline(d, s, &cfg())), "a\nbc\n|");
    assert_eq!(run("a\nbc|", duplicate), "a\nbc\nbc|");
    assert_eq!(run("a\nbc|", |d, s| toggle_line_comment(d, s, &cfg())), "a\n// bc|");
    assert_eq!(run("a\nbc|", delete_forward), "a\nbc|");
    assert_eq!(run("a\nb|c", start_new_line), "a\nbc\n|");
}

#[test]
fn crlf_document_ops_use_normalized_offsets() {
    let mut doc = Document::from_text("fn f() {\r\n    x\r\n}\r\n");
    let sels = SelectionSet::caret(doc.line_end(1));
    let edit = newline(&doc, &sels, &cfg());
    doc.apply(&edit, &sels, EditGroup::Typing).unwrap();
    assert_eq!(doc.to_disk_text(), "fn f() {\r\n    x\r\n    \r\n}\r\n");
    let sels = edit.selections;
    let edit = toggle_line_comment(&doc, &SelectionSet::caret(0), &cfg());
    doc.apply(&edit, &sels, EditGroup::Standalone).unwrap();
    assert_eq!(doc.to_disk_text(), "// fn f() {\r\n    x\r\n    \r\n}\r\n");
}

#[test]
fn unicode_line_separator_lines() {
    // U+2028 is ordinary content: only \n separates lines.
    let (doc, sels) = parse("a\u{2028}b|\nc");
    assert_eq!(doc.len_lines(), 2);
    assert_eq!(doc.line_len(0), 3);
    let edit = delete_line(&doc, &sels);
    let mut d = doc.clone();
    d.apply(&edit, &sels, EditGroup::Standalone).unwrap();
    assert_eq!(d.text(), "c");
}
