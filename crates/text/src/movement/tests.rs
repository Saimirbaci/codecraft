use super::*;
use crate::test_util::{parse, render, sel};

fn mv(spec: &str, motion: Motion) -> String {
    sel(spec, |d, s| move_selections(d, s, motion, false))
}

fn ext(spec: &str, motion: Motion) -> String {
    sel(spec, |d, s| move_selections(d, s, motion, true))
}

/// Apply `motion` `n` times, carrying the goal column.
fn mv_n(spec: &str, motion: Motion, n: usize) -> String {
    let (doc, mut sels) = parse(spec);
    for _ in 0..n {
        sels = move_selections(&doc, &sels, motion, false);
    }
    render(&doc, &sels)
}

#[test]
fn left_right_basics() {
    assert_eq!(mv("ab|c", Motion::Left), "a|bc");
    assert_eq!(mv("ab|c", Motion::Right), "abc|");
    assert_eq!(mv("|abc", Motion::Left), "|abc");
    assert_eq!(mv("abc|", Motion::Right), "abc|");
    assert_eq!(mv("a\n|b", Motion::Left), "a|\nb");
    assert_eq!(mv("a|\nb", Motion::Right), "a\n|b");
    assert_eq!(mv("|", Motion::Right), "|");
}

#[test]
fn left_right_collapse_or_extend_selections() {
    assert_eq!(mv("a«bc»d", Motion::Left), "a|bcd");
    assert_eq!(mv("a»bc«d", Motion::Right), "abc|d");
    assert_eq!(ext("a«bc»d", Motion::Right), "a«bcd»");
    assert_eq!(ext("a«bc»d", Motion::Left), "a«b»cd");
    assert_eq!(ext("ab|c", Motion::Left), "a»b«c");
    assert_eq!(ext("a«b»c", Motion::Left), "a|bc");
}

#[test]
fn left_right_step_over_clusters() {
    assert_eq!(mv("a👍🏽|b", Motion::Left), "a|👍🏽b");
    assert_eq!(mv("a|👍🏽b", Motion::Right), "a👍🏽|b");
    assert_eq!(mv("|e\u{301}x", Motion::Right), "e\u{301}|x");
    assert_eq!(mv("x👨\u{200D}👩\u{200D}👧|", Motion::Left), "x|👨\u{200D}👩\u{200D}👧");
    assert_eq!(mv("|🇫🇷🇩🇪", Motion::Right), "🇫🇷|🇩🇪");
    assert_eq!(mv("🇫🇷🇩🇪|", Motion::Left), "🇫🇷|🇩🇪");
    assert_eq!(mv("é|😀", Motion::Right), "é😀|");
}

#[test]
fn up_down_keep_goal_column() {
    assert_eq!(mv("abcdef|\nab\nabcdef", Motion::Down), "abcdef\nab|\nabcdef");
    assert_eq!(mv_n("abcdef|\nab\nabcdef", Motion::Down, 2), "abcdef\nab\nabcdef|");
    assert_eq!(mv_n("abcdef\nab\nabc|def", Motion::Up, 2), "abc|def\nab\nabcdef");
    let (doc, sels) = parse("abcdef|\nab");
    let down = move_selections(&doc, &sels, Motion::Down, false);
    assert_eq!(down.primary().goal_column, Some(6));
    let right = move_selections(&doc, &down, Motion::Right, false);
    assert_eq!(right.primary().goal_column, None);
}

#[test]
fn up_down_at_edges_stay() {
    assert_eq!(mv("ab|c\nd", Motion::Up), "ab|c\nd");
    assert_eq!(mv("a\nb|c", Motion::Down), "a\nb|c");
    assert_eq!(mv("|", Motion::Down), "|");
    assert_eq!(mv("a«b»c\nxyz", Motion::Down), "abc\nxy|z");
    assert_eq!(ext("a|bc\nxyz", Motion::Down), "a«bc\nx»yz");
}

#[test]
fn page_up_down_move_by_lines() {
    let doc = "0\n1\n2\n3|\n4\n5";
    assert_eq!(mv(doc, Motion::PageUp(2)), "0\n1|\n2\n3\n4\n5");
    assert_eq!(mv(doc, Motion::PageUp(10)), "0|\n1\n2\n3\n4\n5");
    assert_eq!(mv(doc, Motion::PageDown(10)), "0\n1\n2\n3\n4\n5|");
    assert_eq!(mv("abc|\n\nabc\nabc", Motion::PageDown(2)), "abc\n\nabc|\nabc");
    assert_eq!(mv(doc, Motion::PageDown(0)), "0\n1\n2\n3\n4|\n5");
}

#[test]
fn word_right_stops_at_word_ends() {
    assert_eq!(mv("|foo  bar.baz", Motion::WordRight), "foo|  bar.baz");
    assert_eq!(mv("foo|  bar.baz", Motion::WordRight), "foo  bar|.baz");
    assert_eq!(mv("foo  bar|.baz", Motion::WordRight), "foo  bar.|baz");
    assert_eq!(mv("foo  bar.|baz", Motion::WordRight), "foo  bar.baz|");
    assert_eq!(mv("foo|  \nbar", Motion::WordRight), "foo  |\nbar");
    assert_eq!(mv("foo|\n    bar", Motion::WordRight), "foo\n    bar|");
    assert_eq!(mv("foo|\n\nbar", Motion::WordRight), "foo\n|\nbar");
    assert_eq!(mv("a_b1|", Motion::WordRight), "a_b1|");
    assert_eq!(mv("|héllo wörld", Motion::WordRight), "héllo| wörld");
}

#[test]
fn word_left_stops_at_word_starts() {
    assert_eq!(mv("foo  bar.baz|", Motion::WordLeft), "foo  bar.|baz");
    assert_eq!(mv("foo  bar.|baz", Motion::WordLeft), "foo  bar|.baz");
    assert_eq!(mv("foo  bar|.baz", Motion::WordLeft), "foo  |bar.baz");
    assert_eq!(mv("foo  |bar", Motion::WordLeft), "|foo  bar");
    assert_eq!(mv("foo\n|bar", Motion::WordLeft), "foo|\nbar");
    assert_eq!(mv("x {\n    |let", Motion::WordLeft), "x {|\n    let");
    assert_eq!(mv("|abc", Motion::WordLeft), "|abc");
    assert_eq!(ext("foo bar|", Motion::WordLeft), "foo »bar«");
}

#[test]
fn smart_home_toggles() {
    assert_eq!(mv("    foo|", Motion::LineStart), "    |foo");
    assert_eq!(mv("    |foo", Motion::LineStart), "|    foo");
    assert_eq!(mv("|    foo", Motion::LineStart), "    |foo");
    assert_eq!(mv("fo|o", Motion::LineStart), "|foo");
    assert_eq!(mv("|foo", Motion::LineStart), "|foo");
    assert_eq!(mv("a\n  |", Motion::LineStart), "a\n|  ");
    assert_eq!(ext("  ab|c", Motion::LineStart), "  »ab«c");
}

#[test]
fn line_end_and_document_bounds() {
    assert_eq!(mv("a|bc\nd", Motion::LineEnd), "abc|\nd");
    assert_eq!(mv("a|bc\nd", Motion::DocEnd), "abc\nd|");
    assert_eq!(mv("abc\nd|", Motion::DocStart), "|abc\nd");
    assert_eq!(ext("ab|c\nd", Motion::DocEnd), "ab«c\nd»");
    // U+2028 is not a line break (matches rustc / rust-analyzer).
    assert_eq!(mv("a\u{2028}b|", Motion::LineStart), "|a\u{2028}b");
    assert_eq!(mv("|a\u{2028}b", Motion::LineEnd), "a\u{2028}b|");
}

#[test]
fn carets_merge_when_they_meet() {
    assert_eq!(mv("|a|b", Motion::Left), "|ab");
    assert_eq!(mv("a\n|b\n|c", Motion::DocStart), "|a\nb\nc");
    // Selections that only touch stay separate (adjacent occurrences must be selectable).
    assert_eq!(ext("a|b|c", Motion::Right), "a«b»«c»");
    assert_eq!(ext("a|b|cd", Motion::DocEnd), "a«bcd»");
}

#[test]
fn out_of_range_selections_are_clamped() {
    let doc = Document::from_text("abc");
    let sels = SelectionSet::from_vec(vec![Selection::new(usize::MAX, 50)], 0);
    let moved = move_selections(&doc, &sels, Motion::Left, false);
    assert_eq!(moved, SelectionSet::caret(2));
    let moved = move_selections(&doc, &SelectionSet::caret(77), Motion::Up, true);
    assert_eq!(moved.primary().head, 3);
}

#[test]
fn word_and_line_selection() {
    let (doc, _) = parse("let foo_bar = 1;");
    assert_eq!(word_range_at(&doc, 6), 4..11);
    assert_eq!(word_range_at(&doc, 11), 4..11);
    assert_eq!(word_range_at(&doc, 12), 12..12);
    assert_eq!(word_range_at(&doc, 99), 16..16);
    assert_eq!(select_word_at(&doc, 0), Selection::new(0, 3));
    let (doc, _) = parse("ab\ncd\nef");
    assert_eq!(select_line(&doc, 1), Selection::new(3, 6));
    assert_eq!(select_line(&doc, 2), Selection::new(6, 8));
    assert_eq!(select_line(&doc, 9), Selection::new(6, 8));
    assert_eq!(select_all(&doc), SelectionSet::single(Selection::new(0, 8)));
}

#[test]
fn extend_selection_grows_outward() {
    let steps = [
        "fn f() {\n    foo(bar, b|az);\n}",
        "fn f() {\n    foo(bar, «baz»);\n}",
        "fn f() {\n    foo(«bar, baz»);\n}",
        "fn f() {\n    foo«(bar, baz)»;\n}",
        "fn f() {\n    «foo(bar, baz);»\n}",
        "fn f() {\n«    foo(bar, baz);\n»}",
        "fn f() {«\n    foo(bar, baz);\n»}",
        "fn f() «{\n    foo(bar, baz);\n}»",
        "«fn f() {\n    foo(bar, baz);\n}»",
    ];
    for w in steps.windows(2) {
        assert_eq!(sel(w[0], extend_selection), w[1], "from {:?}", w[0]);
    }
    assert_eq!(sel(steps[8], extend_selection), steps[8]);
}

#[test]
fn extend_selection_quotes_and_empty_pairs() {
    assert_eq!(sel("x(\"a b|\")", extend_selection), "x(\"a «b»\")");
    assert_eq!(sel("x(\"a «b»\")", extend_selection), "x(\"«a b»\")");
    assert_eq!(sel("x(\"«a b»\")", extend_selection), "x(«\"a b\"»)");
    assert_eq!(sel("f(|)", extend_selection), "f«()»");
    assert_eq!(sel("|", extend_selection), "|");
}

#[test]
fn add_caret_above_and_below() {
    assert_eq!(sel("abc|\nab\nabc", |d, s| add_caret_vertical(d, s, true)), "abc|\nab|\nabc");
    let (doc, sels) = parse("abc|\nab\nabc");
    let once = add_caret_vertical(&doc, &sels, true);
    let twice = add_caret_vertical(&doc, &once, true);
    assert_eq!(render(&doc, &twice), "abc|\nab|\nabc|");
    assert_eq!(twice.primary().head, 10);
    assert_eq!(sel("a|\nb", |d, s| add_caret_vertical(d, s, false)), "a|\nb");
    assert_eq!(sel("a\nb|", |d, s| add_caret_vertical(d, s, true)), "a\nb|");
}

#[test]
fn next_occurrence_selects_word_then_adds_matches() {
    let (doc, sels) = parse("fo|o bar foo foo");
    let s1 = add_next_occurrence(&doc, &sels);
    assert_eq!(render(&doc, &s1), "«foo» bar foo foo");
    let s2 = add_next_occurrence(&doc, &s1);
    assert_eq!(render(&doc, &s2), "«foo» bar «foo» foo");
    let s3 = add_next_occurrence(&doc, &s2);
    assert_eq!(render(&doc, &s3), "«foo» bar «foo» «foo»");
    assert_eq!(s3.primary().start(), 12);
    let s4 = add_next_occurrence(&doc, &s3);
    assert_eq!(s4, s3);
    let back = unselect_occurrence(&doc, &s3);
    assert_eq!(render(&doc, &back), "«foo» bar «foo» foo");
    assert_eq!(back.primary().start(), 8);
}

#[test]
fn next_occurrence_wraps_and_is_case_sensitive() {
    let (doc, _) = parse("Foo foo x foo");
    let sels = SelectionSet::single(Selection::new(10, 13));
    let s1 = add_next_occurrence(&doc, &sels);
    assert_eq!(render(&doc, &s1), "Foo «foo» x «foo»");
    assert_eq!(s1.primary().start(), 4);
    assert_eq!(sel("|  x", add_next_occurrence), "|  x");
    let (doc, _) = parse("é😀é😀");
    let s = add_next_occurrence(&doc, &SelectionSet::single(Selection::new(0, 2)));
    assert_eq!(render(&doc, &s), "«é😀»«é😀»");
}

#[test]
fn unselect_occurrence_wraps_primary() {
    let (doc, _) = parse("a a a");
    let sels = SelectionSet::from_vec(vec![Selection::new(0, 1), Selection::new(2, 3), Selection::new(4, 5)], 0);
    let out = unselect_occurrence(&doc, &sels);
    assert_eq!(render(&doc, &out), "a «a» «a»");
    assert_eq!(out.primary().start(), 4);
    assert_eq!(unselect_occurrence(&doc, &SelectionSet::caret(1)), SelectionSet::caret(1));
}

#[test]
fn select_all_occurrences_whole_word_from_caret() {
    assert_eq!(sel("fo|o foobar foo", select_all_occurrences), "«foo» foobar «foo»");
    assert_eq!(sel("«foo» foobar foo", select_all_occurrences), "«foo» «foo»bar «foo»");
    assert_eq!(sel("aaaa«aa»", select_all_occurrences), "«aa»«aa»«aa»");
    assert_eq!(sel(" | ", select_all_occurrences), " | ");
    let (doc, sels) = parse("x foo y fo|o");
    assert_eq!(select_all_occurrences(&doc, &sels).primary().start(), 8);
}

#[test]
fn carets_to_line_ends() {
    assert_eq!(sel("«ab\ncd\nef»", add_carets_to_line_ends), "ab|\ncd|\nef|");
    assert_eq!(sel("«ab\ncd\n»ef", add_carets_to_line_ends), "ab|\ncd|\nef");
    assert_eq!(sel("a|b|\ncd", add_carets_to_line_ends), "ab|\ncd");
}
