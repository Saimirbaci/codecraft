//! TOML: table headers, keys, strings (basic, literal, multi-line), numbers, dates, booleans.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

/// A `[table]` or `[[array.of.tables]]` header.
pub(crate) struct Header {
    pub(crate) line: usize,
    /// Char column of the name inside the brackets.
    pub(crate) col: usize,
    pub(crate) name: String,
}

pub(crate) struct Analysis {
    pub(crate) spans: Vec<Span>,
    pub(crate) headers: Vec<Header>,
}

/// Lexes the document. Keys are expected at the start of a top-level line and after `{`/`,`
/// in an inline table; everything after `=` and inside arrays is a value.
pub(crate) fn analyze(doc: &Doc) -> Analysis {
    let n = doc.len();
    let mut sink = SpanSink::with_capacity(n / 4);
    let mut headers = Vec::new();
    // Open `[` (array) and `{` (inline table) brackets in value position.
    let mut stack: Vec<char> = Vec::new();
    let mut expect_key = true;
    let mut at_line_start = true;
    let mut i = 0;
    while i < n {
        let c = doc.at(i);
        if c == '\n' {
            i += 1;
            at_line_start = true;
            if stack.is_empty() {
                expect_key = true;
            }
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let line_start = std::mem::replace(&mut at_line_start, false);
        if line_start && !stack.is_empty() && starts_top_level_line(doc, i) {
            // An unclosed array or inline table: recover at a header or `key =` line.
            stack.clear();
            expect_key = true;
        }
        let next = match c {
            '#' => {
                let e = doc.line_end_at(i);
                sink.push(i, e, K::Comment);
                e
            }
            '[' if line_start && stack.is_empty() => {
                expect_key = false;
                header(doc, i, &mut sink, &mut headers)
            }
            '"' | '\'' => {
                let (e, terminated) = string_end(doc, i);
                let kind = if !terminated {
                    K::Invalid
                } else if expect_key {
                    K::Key
                } else {
                    K::String
                };
                sink.push(i, e, kind);
                e
            }
            '[' | '{' => {
                sink.push(i, i + 1, K::Bracket);
                stack.push(c);
                expect_key = c == '{';
                i + 1
            }
            ']' | '}' => {
                sink.push(i, i + 1, K::Bracket);
                let open = if c == ']' { '[' } else { '{' };
                if stack.last() == Some(&open) {
                    stack.pop();
                }
                expect_key = false;
                i + 1
            }
            ',' => {
                sink.push(i, i + 1, K::Punctuation);
                expect_key = stack.last() == Some(&'{');
                i + 1
            }
            '=' => {
                sink.push(i, i + 1, K::Operator);
                expect_key = false;
                i + 1
            }
            '.' if expect_key => {
                sink.push(i, i + 1, K::Punctuation);
                i + 1
            }
            _ if expect_key && is_bare_key_char(c) => {
                let e = doc.scan_while(i, is_bare_key_char);
                sink.push(i, e, K::Key);
                e
            }
            _ => {
                let e = value_end(doc, i);
                let kind = if expect_key { K::Invalid } else { value_kind(&doc.string(i, e)) };
                sink.push(i, e, kind);
                e
            }
        };
        i = next.max(i + 1);
    }
    Analysis { spans: sink.into_spans(), headers }
}

fn is_bare_key_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Whether the line starting at `i` looks like a `key = …` line or a `[table]` header, which
/// cannot appear inside a value.
fn starts_top_level_line(doc: &Doc, i: usize) -> bool {
    let end = doc.line_end_at(i);
    if doc.at(i) == '[' {
        let open = if doc.at(i + 1) == '[' { i + 2 } else { i + 1 };
        let first = doc.at(open);
        let body_end = (i..end).find(|&j| doc.at(j) == '#').unwrap_or(end);
        let body_end = doc.trim_end(i, body_end);
        let inner: String = doc.string(open, body_end).trim_end_matches(']').trim().to_string();
        return (first.is_alphabetic() || first == '_' || first == '"' || first == '\'')
            && doc.at(body_end.saturating_sub(1)) == ']'
            && !inner.contains(',')
            && !matches!(inner.as_str(), "true" | "false" | "inf" | "nan");
    }
    let mut j = i;
    while j < end {
        let c = doc.at(j);
        if is_bare_key_char(c) || c == '.' || c == ' ' || c == '\t' {
            j += 1;
        } else if c == '"' || c == '\'' {
            j = doc.slice(j + 1, end).iter().position(|&q| q == c).map_or(end, |p| j + 1 + p + 1);
        } else {
            return c == '=' && j > i;
        }
    }
    false
}

/// Lexes a header line starting at `i`; returns the offset after it.
fn header(doc: &Doc, i: usize, sink: &mut SpanSink, headers: &mut Vec<Header>) -> usize {
    let line_end = doc.line_end_at(i);
    let open = if doc.at(i + 1) == '[' { i + 2 } else { i + 1 };
    let double = open == i + 2;
    let mut j = open;
    let mut close = None;
    while j < line_end {
        match doc.at(j) {
            q @ ('"' | '\'') => {
                j = doc.slice(j + 1, line_end).iter().position(|&c| c == q).map_or(line_end, |p| j + 1 + p + 1);
            }
            ']' => {
                close = Some(j);
                break;
            }
            '#' => break,
            _ => j += 1,
        }
    }
    let end = match close {
        Some(c) if double && doc.at(c + 1) == ']' => c + 2,
        Some(c) => c + 1,
        None => doc.trim_end(i, j),
    };
    sink.push(i, end, K::Heading);
    let inner_end = close.unwrap_or(j);
    let name_start = doc.scan_while(open, |c| c == ' ' || c == '\t').min(inner_end);
    let name_end = doc.trim_end(name_start, inner_end);
    let line = doc.index.line_of(i);
    headers.push(Header {
        line,
        col: name_start.saturating_sub(doc.index.line_start(line)),
        name: doc.string(name_start, name_end),
    });
    end
}

/// End of the string starting at `i` and whether it is terminated. `"""`/`'''` strings span
/// lines; the others end at the line end.
fn string_end(doc: &Doc, i: usize) -> (usize, bool) {
    let n = doc.len();
    let q = doc.at(i);
    if doc.at(i + 1) == q && doc.at(i + 2) == q {
        let mut j = i + 3;
        while j < n {
            let c = doc.at(j);
            if q == '"' && c == '\\' {
                j += 2;
                continue;
            }
            if c == q && doc.at(j + 1) == q && doc.at(j + 2) == q {
                // Up to two quotes right before the closing delimiter belong to the content.
                let mut e = j + 3;
                let mut extra = 0;
                while extra < 2 && e < n && doc.at(e) == q {
                    e += 1;
                    extra += 1;
                }
                return (e, true);
            }
            j += 1;
        }
        return (n, false);
    }
    let mut j = i + 1;
    while j < n {
        match doc.at(j) {
            '\n' => return (j, false),
            '\\' if q == '"' && doc.at(j + 1) == '\n' => return (j + 1, false),
            '\\' if q == '"' => j += 2,
            c if c == q => return (j + 1, true),
            _ => j += 1,
        }
    }
    (n, false)
}

/// End of a bare value (number, date, boolean, …) starting at `i`.
fn value_end(doc: &Doc, i: usize) -> usize {
    let is_value_char =
        |c: char| !c.is_whitespace() && !matches!(c, ',' | '[' | ']' | '{' | '}' | '#' | '=' | '"' | '\'');
    let e = doc.scan_while(i, is_value_char).max(i + 1);
    // `1979-05-27 07:32:00`: a date and a time separated by one space.
    let word = doc.slice(i, e);
    let is_date = word.len() == 10
        && word.iter().enumerate().all(|(k, c)| if k == 4 || k == 7 { *c == '-' } else { c.is_ascii_digit() });
    if is_date
        && doc.at(e) == ' '
        && doc.at(e + 1).is_ascii_digit()
        && doc.at(e + 2).is_ascii_digit()
        && doc.at(e + 3) == ':'
    {
        return doc.scan_while(e + 1, is_value_char);
    }
    e
}

fn value_kind(word: &str) -> K {
    match word {
        "true" | "false" => return K::Boolean,
        "inf" | "+inf" | "-inf" | "nan" | "+nan" | "-nan" => return K::Number,
        _ => {}
    }
    let digits = word.strip_prefix(['+', '-']).unwrap_or(word);
    if digits.starts_with(|c: char| c.is_ascii_digit())
        // A space only occurs in a `date time` value (see `value_end`).
        && digits.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '+' | '-' | ' '))
    {
        K::Number
    } else {
        K::Invalid
    }
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let analysis = analyze(doc);
    let mut out = Vec::new();
    // A table runs to its last content line before the next header.
    let content: Vec<bool> =
        (0..doc.index.len_lines()).map(|line| doc.first_non_ws(line).is_some_and(|f| doc.at(f) != '#')).collect();
    let header_lines: Vec<usize> = analysis.headers.iter().map(|h| h.line).collect();
    for (k, &start) in header_lines.iter().enumerate() {
        let limit = header_lines.get(k + 1).copied().unwrap_or(content.len());
        let end = (start..limit).rev().find(|&l| content.get(l).copied().unwrap_or(false)).unwrap_or(start);
        common::push_fold(&mut out, start, end, FoldKind::Block);
    }
    let brackets = common::code_brackets(&doc.chars, &analysis.spans);
    common::bracket_folds(brackets.iter().map(|b| (b.ch, doc.index.line_of(b.offset))), &mut out);
    common::flag_runs(&common::hash_comment_lines(doc, &analysis.spans), FoldKind::Comment, &mut out);
    common::finish_folds(out)
}

pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    analyze(doc)
        .headers
        .into_iter()
        .map(|h| OutlineItem { name: h.name, kind: SymbolKind::Section, line: h.line, column: h.col, depth: 0 })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_at, kind_of, offset_of, spans};
    use crate::{
        FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, matching_bracket,
        outline,
    };

    const T: Language = Language::Toml;

    const CARGO: &str = r#"# Manifest
[package]
name = "codecraft"   # trailing
version = '0.1.0'
edition = 2024
publish = false

[dependencies]
serde = { version = "1", features = ["derive"] }
"quoted.key" = 1
a.b-c.d = 2.5e-3

[[bin]]
name = "cc"
"#;

    #[test]
    fn headers_keys_values() {
        assert_kind(T, CARGO, "# Manifest", K::Comment);
        assert_kind(T, CARGO, "[package]", K::Heading);
        assert_kind(T, CARGO, "[[bin]]", K::Heading);
        assert_kind(T, CARGO, "name", K::Key);
        assert_kind(T, CARGO, "=", K::Operator);
        assert_kind(T, CARGO, "\"codecraft\"", K::String);
        assert_kind(T, CARGO, "# trailing", K::Comment);
        assert_kind(T, CARGO, "'0.1.0'", K::String);
        assert_kind(T, CARGO, "2024", K::Number);
        assert_kind(T, CARGO, "false", K::Boolean);
        assert_kind(T, CARGO, "version", K::Key);
        assert_kind(T, CARGO, "\"derive\"", K::String);
        assert_kind(T, CARGO, "{", K::Bracket);
        assert_kind(T, CARGO, "\"quoted.key\"", K::Key);
        assert_kind(T, CARGO, "b-c", K::Key);
        assert_kind(T, CARGO, ".", K::Punctuation);
        assert_kind(T, CARGO, "2.5e-3", K::Number);
    }

    #[test]
    fn inline_table_keys_after_comma() {
        let text = "x = { a = 1, b = \"s\" }";
        let all = spans(T, text);
        let keys: Vec<&str> = all.iter().filter(|(_, k)| *k == K::Key).map(|(t, _)| t.as_str()).collect();
        assert_eq!(keys, ["x", "a", "b"]);
    }

    #[test]
    fn dates_and_special_numbers() {
        assert_kind(T, "d = 1979-05-27T07:32:00Z", "1979-05-27T07:32:00Z", K::Number);
        assert_kind(T, "d = 1979-05-27 07:32:00", "1979-05-27 07:32:00", K::Number);
        assert_kind(T, "d = 07:32:00", "07:32:00", K::Number);
        assert_kind(T, "h = 0xDEAD_beef", "0xDEAD_beef", K::Number);
        assert_kind(T, "i = -inf", "-inf", K::Number);
        assert_kind(T, "i = +1_000", "+1_000", K::Number);
        assert_kind(T, "x = bogus", "bogus", K::Invalid);
    }

    #[test]
    fn multiline_strings_span_lines() {
        let text = "s = \"\"\"\nline [1]\n\"\"\"\"\nt = '''\n# not a comment\n'''\nk = 1";
        assert_eq!(kind_at(T, text, 1, 0), K::String);
        assert_eq!(kind_at(T, text, 2, 3), K::String); // the fourth quote is content
        assert_eq!(kind_at(T, text, 4, 0), K::String);
        assert_eq!(kind_of(T, text, "k"), Some(K::Key));
        // Brackets inside the string do not match.
        assert_eq!(matching_bracket(T, text, offset_of(text, "[", 0)), None);
    }

    #[test]
    fn unterminated_strings_are_invalid() {
        assert_kind(T, "a = \"open\nb = 1", "\"open", K::Invalid);
        assert_eq!(kind_of(T, "a = \"open\nb = 1", "b"), Some(K::Key));
        assert_eq!(kind_at(T, "a = '''\nnever", 1, 0), K::Invalid);
    }

    #[test]
    fn multiline_array_values_are_not_keys() {
        let text = "features = [\n  \"a\",\n  \"b\", # c\n]\nnext = true";
        assert_kind(T, text, "\"a\"", K::String);
        assert_kind(T, text, "# c", K::Comment);
        assert_eq!(kind_of(T, text, "next"), Some(K::Key));
    }

    #[test]
    fn unclosed_array_recovers_at_next_key_or_header() {
        let text = "a = [1,\nb = 2\n[t]\nc = [\n[u]";
        assert_eq!(kind_of(T, text, "b"), Some(K::Key));
        assert_eq!(kind_of(T, text, "[u]"), Some(K::Heading));
        // A nested array line inside an array stays a value.
        let nested = "m = [\n  [1, 2],\n  [true],\n]";
        assert_eq!(kind_of(T, nested, "true"), Some(K::Boolean));
    }

    #[test]
    fn header_with_quoted_part_and_comment() {
        let text = "[ a.\"b]c\" ] # note\nk = 1";
        assert_kind(T, text, "[ a.\"b]c\" ]", K::Heading);
        assert_kind(T, text, "# note", K::Comment);
        let items = outline(T, text);
        assert_eq!(items[0].name, "a.\"b]c\"");
        assert_eq!(items[0].column, 2);
    }

    #[test]
    fn non_ascii_columns() {
        let text = "ключ = \"значение\" # ü";
        let h = crate::test_util::checked(T, text);
        let line = h.line(0);
        assert_eq!((line[0].start, line[0].end, line[0].kind), (0, 4, K::Key));
        assert_eq!((line[2].start, line[2].end, line[2].kind), (7, 17, K::String));
        assert_eq!((line[3].start, line[3].end, line[3].kind), (18, 21, K::Comment));
    }

    #[test]
    fn outline_lists_tables() {
        assert_eq!(
            outline(T, CARGO),
            vec![
                OutlineItem { name: "package".into(), kind: SymbolKind::Section, line: 1, column: 1, depth: 0 },
                OutlineItem { name: "dependencies".into(), kind: SymbolKind::Section, line: 7, column: 1, depth: 0 },
                OutlineItem { name: "bin".into(), kind: SymbolKind::Section, line: 12, column: 2, depth: 0 },
            ]
        );
    }

    #[test]
    fn folds_tables_arrays_comments() {
        let text = "# a\n# b\n[t]\nx = [\n  1,\n]\n\n# about u\n[u]\ny = 1\n";
        assert_eq!(
            folding_ranges(T, text),
            vec![
                FoldRange { start_line: 0, end_line: 1, kind: FoldKind::Comment },
                FoldRange { start_line: 2, end_line: 5, kind: FoldKind::Block },
                FoldRange { start_line: 3, end_line: 5, kind: FoldKind::Block },
                FoldRange { start_line: 8, end_line: 9, kind: FoldKind::Block },
            ]
        );
    }

    #[test]
    fn brackets_match_in_values_and_headers() {
        let text = "a = [[1], {b = 2}]";
        assert_eq!(matching_bracket(T, text, 4), Some(text.chars().count() - 1));
        assert_eq!(matching_bracket(T, "[t]", 0), Some(2));
        assert_eq!(matching_bracket(T, "a = \"[\"", 5), None);
    }
}
