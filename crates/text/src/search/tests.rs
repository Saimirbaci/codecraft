use super::*;
use crate::EditGroup;
use crate::test_util::{parse, render};

fn q(pattern: &str) -> SearchQuery {
    SearchQuery { pattern: pattern.into(), case_sensitive: true, ..SearchQuery::default() }
}

fn found(text: &str, query: &SearchQuery) -> Vec<std::ops::Range<usize>> {
    find_all(&Document::from_text(text), query, usize::MAX).unwrap()
}

#[test]
fn find_all_plain_and_case() {
    assert_eq!(found("foo Foo foo", &q("foo")), vec![0..3, 8..11]);
    assert_eq!(found("foo Foo foo", &SearchQuery::plain("FOO")), vec![0..3, 4..7, 8..11]);
    assert_eq!(found("a.b a+b", &q("a.b")), vec![0..3]);
    assert_eq!(found("aaaa", &q("aa")), vec![0..2, 2..4]);
    assert_eq!(found("abc", &q("")), Vec::<std::ops::Range<usize>>::new());
    assert_eq!(found("", &q("x")), Vec::<std::ops::Range<usize>>::new());
}

#[test]
fn find_all_reports_char_offsets_for_non_ascii() {
    assert_eq!(found("é😀foo👍🏽foo", &q("foo")), vec![2..5, 7..10]);
    assert_eq!(found("ÄäÄ", &SearchQuery::plain("ä")), vec![0..1, 1..2, 2..3]);
    let re = SearchQuery { regex: true, ..q("😀+") };
    assert_eq!(found("a😀😀b😀", &re), vec![1..3, 4..5]);
}

#[test]
fn find_all_whole_word() {
    let ww = SearchQuery { whole_word: true, ..q("foo") };
    assert_eq!(found("foo foobar foo_x xfoo (foo)", &ww), vec![0..3, 23..26]);
    let arrow = SearchQuery { whole_word: true, ..q("->") };
    assert_eq!(found("a->b ->", &arrow), vec![1..3, 5..7]);
    let re = SearchQuery { whole_word: true, regex: true, ..q("fo+") };
    assert_eq!(found("fooo xfoo foo", &re), vec![0..4, 10..13]);
}

#[test]
fn find_all_regex_is_multiline_and_skips_empty_matches() {
    let re = SearchQuery { regex: true, ..q("^fn \\w+") };
    assert_eq!(found("fn a\n  fn b\nfn c", &re), vec![0..4, 12..16]);
    let empty = SearchQuery { regex: true, ..q("x*") };
    assert_eq!(found("axxb", &empty), vec![1..3]);
    let end = SearchQuery { regex: true, ..q("\\d$") };
    assert_eq!(found("a1\nb2\nc3x", &end), vec![1..2, 4..5]);
}

#[test]
fn find_all_limit_and_errors() {
    let doc = Document::from_text("x x x x");
    assert_eq!(find_all(&doc, &q("x"), 2).unwrap(), vec![0..1, 2..3]);
    assert_eq!(find_all(&doc, &q("x"), 0).unwrap(), Vec::<std::ops::Range<usize>>::new());
    let bad = SearchQuery { regex: true, ..q("(unclosed") };
    assert!(matches!(find_all(&doc, &bad, 10), Err(TextError::BadPattern(_))));
    assert!(matches!(find_next(&doc, &bad, 0, true, true), Err(TextError::BadPattern(_))));
    assert!(replace_all(&doc, &bad, "y", &SelectionSet::caret(0)).is_err());
    // A literal pattern full of regex syntax is fine.
    assert_eq!(found("(unclosed", &q("(unclosed")), vec![0..9]);
}

#[test]
fn find_next_forward_backward_wrap() {
    let doc = Document::from_text("ab ab ab");
    let query = q("ab");
    assert_eq!(find_next(&doc, &query, 0, true, false).unwrap(), Some(0..2));
    assert_eq!(find_next(&doc, &query, 1, true, false).unwrap(), Some(3..5));
    assert_eq!(find_next(&doc, &query, 7, true, false).unwrap(), None);
    assert_eq!(find_next(&doc, &query, 7, true, true).unwrap(), Some(0..2));
    assert_eq!(find_next(&doc, &query, 6, false, false).unwrap(), Some(3..5));
    assert_eq!(find_next(&doc, &query, 0, false, false).unwrap(), None);
    assert_eq!(find_next(&doc, &query, 0, false, true).unwrap(), Some(6..8));
    assert_eq!(find_next(&doc, &query, 999, true, true).unwrap(), Some(0..2));
    assert_eq!(find_next(&doc, &q(""), 0, true, true).unwrap(), None);
}

#[test]
fn find_next_respects_word_boundaries_before_from() {
    let doc = Document::from_text("foobar bar");
    let ww = SearchQuery { whole_word: true, ..q("bar") };
    assert_eq!(find_next(&doc, &ww, 3, true, false).unwrap(), Some(7..10));
    let doc = Document::from_text("é😀x é😀x");
    assert_eq!(find_next(&doc, &q("x"), 3, true, false).unwrap(), Some(6..7));
    assert_eq!(find_next(&doc, &q("x"), 6, false, false).unwrap(), Some(2..3));
    let empty = SearchQuery { regex: true, ..q("z*") };
    assert_eq!(find_next(&doc, &empty, 0, true, true).unwrap(), None);
}

fn replaced(spec: &str, query: &SearchQuery, replacement: &str) -> String {
    let (mut doc, sels) = parse(spec);
    let original = doc.text();
    let edit = replace_all(&doc, query, replacement, &sels).unwrap();
    doc.apply(&edit, &sels, EditGroup::Standalone).unwrap();
    let out = render(&doc, &edit.selections);
    doc.undo();
    assert_eq!(doc.text(), original);
    out
}

#[test]
fn replace_all_literal() {
    assert_eq!(replaced("a foo b foo|", &q("foo"), "x"), "a x b x|");
    assert_eq!(replaced("|a.b a.b", &q("a.b"), "$1"), "|$1 $1");
    assert_eq!(replaced("x|", &q("x"), "a\r\nb"), "a\nb|");
    assert_eq!(replaced("é😀é|", &q("é"), "ee"), "ee😀ee|");
    assert_eq!(replaced("ab|c", &q("zz"), "y"), "ab|c");
    assert_eq!(replaced("a«bc»d", &q("bc"), "XYZ"), "a«XYZ»d");
}

#[test]
fn replace_all_regex_groups() {
    let re = SearchQuery { regex: true, ..q("(\\w+)=(\\w+)") };
    assert_eq!(replaced("|a=1, b=2", &re, "$2=$1"), "|1=a, 2=b");
    assert_eq!(replaced("|a=1", &re, "${1}x"), "|ax");
    assert_eq!(replaced("|a=1", &re, "$1x"), "|ax");
    assert_eq!(replaced("|a=1", &re, "$$1\\$2"), "|$1$2");
    assert_eq!(replaced("|a=1", &re, "$1\\n$2\\t\\\\"), "|a\n1\t\\");
    let named = SearchQuery { regex: true, ..q("(?P<k>\\w)=") };
    assert_eq!(replaced("|a=1", &named, "${k}:"), "|a:1");
    let lines = SearchQuery { regex: true, ..q("^(\\w)") };
    assert_eq!(replaced("a\nb|", &lines, "// $1"), "// a\n// b|");
    let ci = SearchQuery { regex: true, case_sensitive: false, ..q("x") };
    assert_eq!(replaced("|xX", &ci, "y"), "|yy");
}

#[test]
fn replace_all_empty_pattern_is_noop() {
    let doc = Document::from_text("abc");
    let edit = replace_all(&doc, &q(""), "x", &SelectionSet::caret(9)).unwrap();
    assert!(edit.is_noop());
    assert_eq!(edit.selections, SelectionSet::caret(3));
}

#[test]
fn to_regex_flags() {
    assert!(q("a").to_regex().unwrap().is_match("a"));
    assert!(!q("a").to_regex().unwrap().is_match("A"));
    assert!(SearchQuery::plain("a").to_regex().unwrap().is_match("A"));
    let re = SearchQuery { regex: true, ..q("^b$") };
    assert!(re.to_regex().unwrap().is_match("a\nb\nc"));
}
