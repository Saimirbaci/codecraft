//! JSON (with `//` and `/* */` comments, as in JSONC): keys, strings, numbers, literals.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldRange, HighlightKind as K};

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    let n = doc.len();
    let mut sink = SpanSink::with_capacity(n / 4);
    let mut i = 0;
    while i < n {
        let c = doc.at(i);
        let next = match c {
            '/' if doc.at(i + 1) == '/' => {
                let e = doc.line_end_at(i);
                sink.push(i, e, K::Comment);
                e
            }
            '/' if doc.at(i + 1) == '*' => {
                let e = doc.find_str(i + 2, "*/").map_or(n, |j| j + 2);
                sink.push(i, e, K::Comment);
                e
            }
            '"' => {
                let (e, terminated) = string_end(doc, i);
                let kind = if !terminated {
                    K::Invalid
                } else if followed_by_colon(doc, e) {
                    K::Key
                } else {
                    K::String
                };
                sink.push(i, e, kind);
                e
            }
            '{' | '}' | '[' | ']' => {
                sink.push(i, i + 1, K::Bracket);
                i + 1
            }
            ',' | ':' => {
                sink.push(i, i + 1, K::Punctuation);
                i + 1
            }
            '-' | '+' | '.' | '0'..='9' => {
                let e = doc.scan_while(i + 1, |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'));
                sink.push(i, e, K::Number);
                e
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let e = doc.scan_while(i + 1, |c| c.is_alphanumeric() || c == '_' || c == '$');
                let kind = match doc.string(i, e).as_str() {
                    "true" | "false" | "null" => K::Boolean,
                    "NaN" | "Infinity" => K::Number,
                    _ => K::Invalid,
                };
                sink.push(i, e, kind);
                e
            }
            c if c.is_whitespace() => i + 1,
            _ => {
                sink.push(i, i + 1, K::Invalid);
                i + 1
            }
        };
        i = next.max(i + 1);
    }
    sink.into_spans()
}

/// End of the string starting at `i` (past the closing quote), and whether it was closed on
/// its line.
fn string_end(doc: &Doc, i: usize) -> (usize, bool) {
    let n = doc.len();
    let mut j = i + 1;
    while j < n {
        match doc.at(j) {
            '\\' if doc.at(j + 1) == '\n' => return (j + 1, false),
            '\\' => j += 2,
            '"' => return (j + 1, true),
            '\n' => return (j, false),
            _ => j += 1,
        }
    }
    (n, false)
}

fn followed_by_colon(doc: &Doc, from: usize) -> bool {
    doc.at(doc.scan_while(from, char::is_whitespace)) == ':'
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let spans = spans(doc);
    let mut out = Vec::new();
    let brackets = common::code_brackets(&doc.chars, &spans);
    common::bracket_folds(brackets.iter().map(|b| (b.ch, doc.index.line_of(b.offset))), &mut out);
    common::multiline_comment_folds(&doc.index, &spans, &mut out);
    common::finish_folds(out)
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_of, offset_of, spans};
    use crate::{FoldKind, FoldRange, HighlightKind as K, Language, folding_ranges, matching_bracket, outline};

    const J: Language = Language::Json;

    #[test]
    fn keys_strings_numbers_literals() {
        let text = r#"{"name": "codecraft", "n": -1.5e3, "ok": true, "none": null, "list": [1, 2]}"#;
        assert_kind(J, text, r#""name""#, K::Key);
        assert_kind(J, text, r#""codecraft""#, K::String);
        assert_kind(J, text, "-1.5e3", K::Number);
        assert_kind(J, text, "true", K::Boolean);
        assert_kind(J, text, "null", K::Boolean);
        assert_kind(J, text, "{", K::Bracket);
        assert_kind(J, text, "[", K::Bracket);
        assert_kind(J, text, ":", K::Punctuation);
        assert_kind(J, text, ",", K::Punctuation);
    }

    #[test]
    fn key_with_space_before_colon_and_escapes() {
        let text = "{\n  \"a\\\"b\"  : \"x\\\\\"\n}";
        assert_kind(J, text, "\"a\\\"b\"", K::Key);
        assert_kind(J, text, "\"x\\\\\"", K::String);
    }

    #[test]
    fn key_with_colon_on_next_line() {
        let text = "{\"k\"\n: 1}";
        assert_eq!(kind_of(J, text, "\"k\""), Some(K::Key));
    }

    #[test]
    fn unterminated_string_is_invalid_to_end_of_line() {
        let all = spans(J, "{\"abc\n}");
        assert_eq!(all[1], ("\"abc".to_string(), K::Invalid));
        assert_eq!(all[2], ("}".to_string(), K::Bracket));
    }

    #[test]
    fn comments_and_garbage() {
        let text = "// head\n{ /* a\n b */ \"k\": 1, @ }";
        assert_kind(J, text, "// head", K::Comment);
        assert_kind(J, text, "/* a", K::Comment);
        assert_kind(J, text, " b */", K::Comment);
        assert_kind(J, text, "@", K::Invalid);
        assert_eq!(kind_of(J, "[undefined]", "undefined"), Some(K::Invalid));
    }

    #[test]
    fn non_ascii_columns() {
        let text = "{\"ключ\": \"значение\"}";
        let all = spans(J, text);
        assert_eq!(all[1], ("\"ключ\"".to_string(), K::Key));
        assert_eq!(all[3], ("\"значение\"".to_string(), K::String));
    }

    #[test]
    fn brackets_skip_strings() {
        let text = r#"{"a": "}{", "b": [1, "]"]}"#;
        assert_eq!(matching_bracket(J, text, 0), Some(text.chars().count() - 1));
        let open = offset_of(text, "[", 0);
        assert_eq!(matching_bracket(J, text, open), Some(offset_of(text, "]", 1)));
        assert_eq!(matching_bracket(J, "{[}", 0), None);
    }

    #[test]
    fn folds_blocks() {
        let text = "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {}\n}";
        assert_eq!(
            folding_ranges(J, text),
            vec![
                FoldRange { start_line: 0, end_line: 6, kind: FoldKind::Block },
                FoldRange { start_line: 1, end_line: 4, kind: FoldKind::Block },
            ]
        );
        assert!(outline(J, text).is_empty());
    }
}
