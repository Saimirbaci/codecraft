//! YAML: keys, scalars (quoted, plain, block), comments, sequence markers, anchors, tags and
//! flow collections. Line-based; quoted scalars end at the line end.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K};

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    let mut sink = SpanSink::with_capacity(doc.len() / 8);
    // Indent of the line that opened a block scalar (`|` / `>`), while inside it.
    let mut block: Option<usize> = None;
    for line in 0..doc.index.len_lines() {
        let ls = doc.index.line_start(line);
        let le = doc.index.line_end(line);
        let first = doc.first_non_ws(line);
        let indent = doc.scan_while(ls, |c| c == ' ').min(le) - ls;
        if let Some(parent) = block {
            match first {
                None => continue,
                Some(f) if indent > parent => {
                    sink.push(f, doc.trim_end(f, le), K::String);
                    continue;
                }
                Some(_) => block = None,
            }
        }
        if let Some(f) = first {
            block = scan_line(doc, &mut sink, ls, f, le, indent);
        }
    }
    sink.into_spans()
}

fn is_ws(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\r'
}

fn is_flow_indicator(c: char) -> bool {
    matches!(c, ',' | '[' | ']' | '{' | '}')
}

/// A `:` at `i` that separates a key from its value.
fn is_value_colon(doc: &Doc, i: usize, le: usize, flow: usize) -> bool {
    let next = doc.at(i + 1);
    doc.at(i) == ':' && (i + 1 >= le || is_ws(next) || (flow > 0 && is_flow_indicator(next)))
}

/// Lexes one non-blank line; returns the line's indent if it opens a block scalar.
fn scan_line(doc: &Doc, sink: &mut SpanSink, ls: usize, first: usize, le: usize, indent: usize) -> Option<usize> {
    let mut i = first;
    if first == ls {
        if (doc.starts_with_at(i, "---") || doc.starts_with_at(i, "...")) && (i + 3 >= le || is_ws(doc.at(i + 3))) {
            sink.push(i, i + 3, K::Punctuation);
            i += 3;
        } else if doc.at(i) == '%' {
            sink.push(i, doc.trim_end(i, le), K::Keyword);
            return None;
        }
    }
    let mut flow = 0usize;
    let mut node_start = true;
    let mut block = None;
    while i < le {
        let c = doc.at(i);
        if is_ws(c) {
            i += 1;
            continue;
        }
        let next = match c {
            '#' if i == ls || is_ws(doc.at(i.saturating_sub(1))) => {
                sink.push(i, le, K::Comment);
                le
            }
            '-' | '?' if node_start && flow == 0 && (i + 1 >= le || is_ws(doc.at(i + 1))) => {
                sink.push(i, i + 1, K::Punctuation);
                i + 1
            }
            '"' | '\'' => {
                let e = quoted_end(doc, i, le);
                let after = doc.scan_while(e, is_ws);
                let kind = if after < le && is_value_colon(doc, after, le, flow) { K::Key } else { K::String };
                sink.push(i, e, kind);
                node_start = false;
                e
            }
            '[' | '{' => {
                sink.push(i, i + 1, K::Bracket);
                flow += 1;
                node_start = true;
                i + 1
            }
            ']' | '}' => {
                sink.push(i, i + 1, K::Bracket);
                flow = flow.saturating_sub(1);
                node_start = false;
                i + 1
            }
            ',' if flow > 0 => {
                sink.push(i, i + 1, K::Punctuation);
                node_start = true;
                i + 1
            }
            ':' if is_value_colon(doc, i, le, flow) => {
                sink.push(i, i + 1, K::Punctuation);
                node_start = true;
                i + 1
            }
            '&' | '*' | '!' if i + 1 < le && !is_ws(doc.at(i + 1)) => {
                let e = doc.scan_while(i + 1, |c| !is_ws(c) && !is_flow_indicator(c)).min(le);
                sink.push(i, e, if c == '!' { K::Attribute } else { K::Constant });
                e
            }
            '|' | '>' if node_start && flow == 0 => {
                let e = doc.scan_while(i + 1, |c| matches!(c, '+' | '-' | '0'..='9')).min(le);
                let rest = doc.scan_while(e, is_ws);
                if rest >= le || doc.at(rest) == '#' {
                    sink.push(i, e, K::Operator);
                    block = Some(indent);
                    e
                } else {
                    plain_scalar(doc, sink, i, le, flow)
                }
            }
            _ => {
                node_start = false;
                plain_scalar(doc, sink, i, le, flow)
            }
        };
        i = next.max(i + 1);
    }
    block
}

/// End of a quoted scalar at `i` (past the closing quote, or the line end).
fn quoted_end(doc: &Doc, i: usize, le: usize) -> usize {
    let q = doc.at(i);
    let mut j = i + 1;
    while j < le {
        let c = doc.at(j);
        // `\x` escapes in double quotes, `''` in single quotes.
        let escaped = (q == '"' && c == '\\') || (q == '\'' && c == q && doc.at(j + 1) == q);
        if escaped {
            j += 2;
        } else if c == q {
            return j + 1;
        } else {
            j += 1;
        }
    }
    le
}

/// Lexes a plain scalar starting at `i`; a key when a value `:` follows it.
fn plain_scalar(doc: &Doc, sink: &mut SpanSink, i: usize, le: usize, flow: usize) -> usize {
    let mut j = i;
    while j < le {
        let c = doc.at(j);
        if (c == ':' && is_value_colon(doc, j, le, flow))
            || (c == '#' && j > i && is_ws(doc.at(j - 1)))
            || (flow > 0 && is_flow_indicator(c))
        {
            break;
        }
        j += 1;
    }
    let end = doc.trim_end(i, j).max(i + 1);
    let kind = if j < le && is_value_colon(doc, j, le, flow) { K::Key } else { scalar_kind(&doc.string(i, end)) };
    sink.push(i, end, kind);
    end
}

fn scalar_kind(s: &str) -> K {
    match s {
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" | "yes" | "Yes" | "YES" | "no" | "No" | "NO" | "on"
        | "On" | "ON" | "off" | "Off" | "OFF" | "null" | "Null" | "NULL" | "~" => K::Boolean,
        _ if is_number(s) => K::Number,
        _ => K::String,
    }
}

fn is_number(s: &str) -> bool {
    let t = s.strip_prefix(['+', '-']).unwrap_or(s);
    if matches!(t, ".inf" | ".Inf" | ".INF") || matches!(s, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    if let Some(hex) = t.strip_prefix("0x") {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit() || c == '_');
    }
    if let Some(oct) = t.strip_prefix("0o") {
        return !oct.is_empty() && oct.chars().all(|c| matches!(c, '0'..='7' | '_'));
    }
    t.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '_' | 'e' | 'E' | '+' | '-'))
        && t.replace('_', "").parse::<f64>().is_ok()
}

/// Indentation blocks (a line and the more-indented lines after it; a `- ` item under a key at
/// the same indent counts as nested) and comment runs.
pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let spans = spans(doc);
    let mut out = Vec::new();
    let mut open: Vec<(usize, usize)> = Vec::new();
    let mut last_content: Option<usize> = None;
    let mut close = |open: &mut Vec<(usize, usize)>, below: Option<usize>, last: Option<usize>| {
        while let Some(&(level, start)) = open.last()
            && below.is_none_or(|b| level >= b)
        {
            open.pop();
            if let Some(end) = last {
                common::push_fold(&mut out, start, end, FoldKind::Block);
            }
        }
    };
    for line in 0..doc.index.len_lines() {
        let Some(f) = doc.first_non_ws(line) else { continue };
        if doc.at(f) == '#' {
            continue;
        }
        let indent = f - doc.index.line_start(line);
        let item = doc.at(f) == '-' && is_ws_or_end(doc, f + 1, doc.index.line_end(line));
        let level = indent.saturating_mul(2) + usize::from(item);
        close(&mut open, Some(level), last_content);
        open.push((level, line));
        last_content = Some(line);
    }
    close(&mut open, None, last_content);
    common::flag_runs(&common::hash_comment_lines(doc, &spans), FoldKind::Comment, &mut out);
    common::finish_folds(out)
}

fn is_ws_or_end(doc: &Doc, i: usize, le: usize) -> bool {
    i >= le || is_ws(doc.at(i))
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_at, kind_of, spans};
    use crate::{FoldKind, FoldRange, HighlightKind as K, Language, folding_ranges, matching_bracket, outline};

    const Y: Language = Language::Yaml;

    #[test]
    fn keys_and_scalars() {
        let text = "name: build # the job\nretries: 3\nratio: -1.5e3\nenabled: yes\nnothing: ~\nversion: 1.2.3\nurl: http://x:80/a";
        assert_kind(Y, text, "name", K::Key);
        assert_kind(Y, text, ":", K::Punctuation);
        assert_kind(Y, text, "build", K::String);
        assert_kind(Y, text, "# the job", K::Comment);
        assert_kind(Y, text, "3", K::Number);
        assert_kind(Y, text, "-1.5e3", K::Number);
        assert_kind(Y, text, "yes", K::Boolean);
        assert_kind(Y, text, "~", K::Boolean);
        assert_kind(Y, text, "1.2.3", K::String);
        assert_kind(Y, text, "http://x:80/a", K::String);
    }

    #[test]
    fn quoted_keys_and_values() {
        let text = "\"quoted key\": 'it''s'\n'k': \"a \\\" b\"\nmsg: don't";
        assert_kind(Y, text, "\"quoted key\"", K::Key);
        assert_kind(Y, text, "'it''s'", K::String);
        assert_kind(Y, text, "'k'", K::Key);
        assert_kind(Y, text, "\"a \\\" b\"", K::String);
        assert_kind(Y, text, "don't", K::String);
        assert_kind(Y, "a: \"open", "\"open", K::String);
    }

    #[test]
    fn sequences_documents_anchors_tags() {
        let text =
            "---\nitems:\n  - one\n  - - nested\n  - key: v\nbase: &base\n  x: 1\nother: *base\ntyped: !!str 5\n...";
        assert_eq!(kind_at(Y, text, 0, 0), K::Punctuation);
        assert_kind(Y, text, "-", K::Punctuation);
        assert_kind(Y, text, "one", K::String);
        assert_kind(Y, text, "nested", K::String);
        assert_kind(Y, text, "key", K::Key);
        assert_kind(Y, text, "&base", K::Constant);
        assert_kind(Y, text, "*base", K::Constant);
        assert_kind(Y, text, "!!str", K::Attribute);
        assert_kind(Y, text, "5", K::Number);
        assert_kind(Y, text, "...", K::Punctuation);
        assert_kind(Y, "%YAML 1.2", "%YAML 1.2", K::Keyword);
    }

    #[test]
    fn flow_collections() {
        let text = "m: {a: 1, b: [x, \"y\", true]}";
        assert_kind(Y, text, "{", K::Bracket);
        assert_kind(Y, text, "a", K::Key);
        assert_kind(Y, text, "b", K::Key);
        assert_kind(Y, text, "x", K::String);
        assert_kind(Y, text, "\"y\"", K::String);
        assert_kind(Y, text, "true", K::Boolean);
        assert_kind(Y, text, ",", K::Punctuation);
        assert_eq!(matching_bracket(Y, text, 3), Some(text.chars().count() - 1));
    }

    #[test]
    fn block_scalars() {
        let text = "script: |\n  echo 'hi' # not a comment\n\n  key: not a key\nnext: 1";
        assert_kind(Y, text, "|", K::Operator);
        assert_kind(Y, text, "echo 'hi' # not a comment", K::String);
        assert_kind(Y, text, "key: not a key", K::String);
        assert_eq!(kind_of(Y, text, "next"), Some(K::Key));
        assert_kind(Y, "f: >-\n  folded\n", ">-", K::Operator);
        assert_kind(Y, "f: >-\n  folded\n", "folded", K::String);
        assert_kind(Y, "cmp: a > b", "a > b", K::String);
    }

    #[test]
    fn hash_inside_scalar_is_not_comment() {
        assert_kind(Y, "color: a#b", "a#b", K::String);
        assert!(spans(Y, "# only").iter().all(|(_, k)| *k == K::Comment));
    }

    #[test]
    fn non_ascii_columns() {
        let h = crate::test_util::checked(Y, "ключ: значение");
        let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(cols, [(0, 4, K::Key), (4, 5, K::Punctuation), (6, 14, K::String)]);
    }

    #[test]
    fn folds_indentation_and_comments() {
        let text =
            "# a\n# b\njobs:\n  build:\n    steps:\n    - run: x\n      name: y\n    - run: z\n  test: 1\nlast: 2\n";
        assert_eq!(
            folding_ranges(Y, text),
            vec![
                FoldRange { start_line: 0, end_line: 1, kind: FoldKind::Comment },
                FoldRange { start_line: 2, end_line: 8, kind: FoldKind::Block },
                FoldRange { start_line: 3, end_line: 7, kind: FoldKind::Block },
                FoldRange { start_line: 4, end_line: 7, kind: FoldKind::Block },
                FoldRange { start_line: 5, end_line: 6, kind: FoldKind::Block },
            ]
        );
        assert!(outline(Y, text).is_empty());
    }
}
