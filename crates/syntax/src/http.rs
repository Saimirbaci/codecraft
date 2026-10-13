//! HTTP request files (`.http`, `.rest`): `###` separators, comments and `# @tags`, file
//! variables (`@name = value`), request lines (method, URL, version), headers, bodies (JSON
//! bodies lexed as JSON), `{{variables}}`, `< file` references and response handlers
//! (`> {% … %}`, `>> file`, `<> file`). Line-based.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Preamble,
    Url,
    Headers,
    Body { json: Option<bool> },
    Handler { script: bool },
}

fn is_comment(doc: &Doc, f: usize) -> bool {
    doc.at(f) == '#' || doc.starts_with_at(f, "//")
}

fn is_trailer(doc: &Doc, ls: usize, le: usize) -> bool {
    let c = doc.at(ls);
    ls < le
        && ((c == '>' && (doc.at(ls + 1) == ' ' || doc.at(ls + 1) == '>' || doc.at(ls + 1) == '{' || ls + 1 >= le))
            || doc.starts_with_at(ls, "<>"))
}

fn is_method_char(c: char) -> bool {
    c.is_ascii_uppercase() || c == '-'
}

/// Longest `{{variable}}` looked for (keeps lexing linear on hostile lines).
const MAX_VAR: usize = 256;

/// The `}}` closing a `{{` at `i`, within the line.
fn var_close(doc: &Doc, i: usize, end: usize) -> Option<usize> {
    (i + 2..end.saturating_sub(1).min(i + MAX_VAR)).find(|&j| doc.at(j) == '}' && doc.at(j + 1) == '}')
}

/// The `%}` closing a script, within `from..end`.
fn script_close(doc: &Doc, from: usize, end: usize) -> Option<usize> {
    (from..end.saturating_sub(1)).find(|&j| doc.at(j) == '%' && doc.at(j + 1) == '}')
}

/// Push `start..end` as `kind`, with `{{…}}` inside as `Macro`.
fn with_vars(doc: &Doc, sink: &mut SpanSink, start: usize, end: usize, kind: K) {
    let mut i = start;
    let mut run = start;
    while i + 1 < end {
        if doc.at(i) == '{'
            && doc.at(i + 1) == '{'
            && let Some(c) = var_close(doc, i, end)
        {
            sink.push(run, i, kind);
            sink.push(i, c + 2, K::Macro);
            i = c + 2;
            run = i;
            continue;
        }
        i += 1;
    }
    sink.push(run, end, kind);
}

fn comment_line(doc: &Doc, sink: &mut SpanSink, f: usize, end: usize) {
    // `# @name value`: the tag stands out.
    let mut j = f + if doc.at(f) == '#' { 1 } else { 2 };
    j = doc.scan_while(j, |c| c == ' ' || c == '\t').min(end);
    if doc.at(j) == '@' && j < end {
        let tag_end = doc.scan_while(j + 1, |c| c.is_alphanumeric() || c == '-' || c == '_').min(end);
        sink.push(f, j, K::Comment);
        sink.push(j, tag_end, K::Attribute);
        sink.push(tag_end, end, K::Comment);
    } else {
        sink.push(f, end, K::Comment);
    }
}

/// `@name = value`; false when the line is not a variable.
fn variable_line(doc: &Doc, sink: &mut SpanSink, f: usize, end: usize) -> bool {
    if doc.at(f) != '@' {
        return false;
    }
    let name_end = doc.scan_while(f + 1, |c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.')).min(end);
    if name_end == f + 1 {
        return false;
    }
    sink.push(f, name_end, K::Key);
    let mut j = doc.scan_while(name_end, |c| c == ' ' || c == '\t').min(end);
    if doc.at(j) == '=' && j < end {
        sink.push(j, j + 1, K::Operator);
        j = doc.scan_while(j + 1, |c| c == ' ' || c == '\t').min(end);
    }
    with_vars(doc, sink, j, end, K::String);
    true
}

fn request_line(doc: &Doc, sink: &mut SpanSink, f: usize, end: usize) {
    let word_end = doc.scan_while(f, |c| !c.is_whitespace()).min(end);
    let mut url_start = f;
    if (f..word_end).all(|j| is_method_char(doc.at(j))) && word_end > f {
        sink.push(f, word_end, K::Keyword);
        url_start = doc.scan_while(word_end, |c| c == ' ' || c == '\t').min(end);
    }
    // A trailing `HTTP/x` is the version.
    let mut url_end = end;
    let last_ws = (url_start..end).rev().find(|&j| doc.at(j).is_whitespace());
    if let Some(ws) = last_ws
        && doc.starts_with_at(ws + 1, "HTTP/")
    {
        url_end = doc.trim_end(url_start, ws);
        with_vars(doc, sink, url_start, url_end, K::Link);
        sink.push(ws + 1, end, K::Constant);
        return;
    }
    with_vars(doc, sink, url_start, url_end, K::Link);
}

fn header_line(doc: &Doc, sink: &mut SpanSink, f: usize, end: usize) {
    match (f..end).find(|&j| doc.at(j) == ':') {
        Some(colon) => {
            with_vars(doc, sink, f, colon, K::Key);
            sink.push(colon, colon + 1, K::Punctuation);
            let v = doc.scan_while(colon + 1, |c| c == ' ' || c == '\t').min(end);
            with_vars(doc, sink, v, end, K::String);
        }
        None => sink.push(f, end, K::Invalid),
    }
}

/// A body line lexed as JSON (strings, keys, numbers, literals, brackets) with `{{…}}`.
fn json_line(doc: &Doc, sink: &mut SpanSink, start: usize, end: usize) {
    let mut i = start;
    while i < end {
        let c = doc.at(i);
        let next = if c == '{' && doc.at(i + 1) == '{' {
            match var_close(doc, i, end) {
                Some(close) => {
                    sink.push(i, close + 2, K::Macro);
                    close + 2
                }
                None => {
                    sink.push(i, i + 1, K::Bracket);
                    i + 1
                }
            }
        } else {
            match c {
                '"' => {
                    let mut j = i + 1;
                    while j < end && doc.at(j) != '"' {
                        j += if doc.at(j) == '\\' { 2 } else { 1 };
                    }
                    let e = (j + 1).min(end);
                    let after = doc.scan_while(e, |c| c == ' ' || c == '\t');
                    let kind = if doc.at(after) == ':' { K::Key } else { K::String };
                    with_vars(doc, sink, i, e, kind);
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
                '-' | '0'..='9' => {
                    let e =
                        doc.scan_while(i + 1, |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')).min(end);
                    sink.push(i, e, K::Number);
                    e
                }
                c if c.is_alphabetic() => {
                    let e = doc.scan_while(i + 1, char::is_alphanumeric).min(end);
                    if matches!(doc.string(i, e).as_str(), "true" | "false" | "null") {
                        sink.push(i, e, K::Boolean);
                    }
                    e
                }
                _ => i + 1,
            }
        };
        i = next.max(i + 1);
    }
}

fn file_ref(doc: &Doc, sink: &mut SpanSink, f: usize, end: usize, marker: usize) {
    sink.push(f, f + marker, K::Operator);
    let p = doc.scan_while(f + marker, |c| c == ' ' || c == '\t').min(end);
    with_vars(doc, sink, p, end, K::Link);
}

/// A handler/redirect line; returns whether a `{% … %}` script stays open.
fn trailer_line(doc: &Doc, sink: &mut SpanSink, ls: usize, end: usize) -> bool {
    if doc.starts_with_at(ls, ">>!") {
        file_ref(doc, sink, ls, end, 3);
        return false;
    }
    if doc.starts_with_at(ls, ">>") || doc.starts_with_at(ls, "<>") {
        file_ref(doc, sink, ls, end, 2);
        return false;
    }
    sink.push(ls, ls + 1, K::Operator);
    let rest = doc.scan_while(ls + 1, |c| c == ' ' || c == '\t').min(end);
    if doc.starts_with_at(rest, "{%") {
        let close = script_close(doc, rest + 2, end);
        sink.push(rest, close.map_or(end, |c| c + 2), K::Code);
        return close.is_none();
    }
    with_vars(doc, sink, rest, end, K::Link);
    false
}

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    let mut sink = SpanSink::with_capacity(doc.len() / 6);
    let mut state = State::Preamble;
    for line in 0..doc.index.len_lines() {
        let ls = doc.index.line_start(line);
        let end = doc.trim_end(ls, doc.index.line_end(line));
        let first = doc.first_non_ws(line).filter(|&f| f < end);
        if doc.starts_with_at(ls, "###") {
            sink.push(ls, end, K::Comment);
            state = State::Preamble;
            continue;
        }
        if let State::Handler { script: true } = state {
            let close = script_close(doc, ls, end);
            sink.push(ls, close.map_or(end, |c| c + 2), K::Code);
            state = State::Handler { script: close.is_none() };
            continue;
        }
        let Some(f) = first else {
            state = match state {
                State::Url | State::Headers => State::Body { json: None },
                s => s,
            };
            continue;
        };
        state = match state {
            State::Preamble => {
                if is_comment(doc, f) {
                    comment_line(doc, &mut sink, f, end);
                    State::Preamble
                } else if variable_line(doc, &mut sink, f, end) {
                    State::Preamble
                } else {
                    request_line(doc, &mut sink, f, end);
                    State::Url
                }
            }
            State::Url | State::Headers if is_trailer(doc, ls, end) => {
                State::Handler { script: trailer_line(doc, &mut sink, ls, end) }
            }
            State::Url | State::Headers => {
                if is_comment(doc, f) {
                    comment_line(doc, &mut sink, f, end);
                    state
                } else if state == State::Url && f > ls {
                    with_vars(doc, &mut sink, f, end, K::Link);
                    State::Url
                } else {
                    header_line(doc, &mut sink, f, end);
                    State::Headers
                }
            }
            State::Body { .. } if is_trailer(doc, ls, end) => {
                State::Handler { script: trailer_line(doc, &mut sink, ls, end) }
            }
            State::Body { json } => {
                let json = json.unwrap_or(matches!(doc.at(f), '{' | '['));
                if doc.starts_with_at(f, "<@") {
                    file_ref(doc, &mut sink, f, end, 2);
                } else if doc.starts_with_at(f, "< ") {
                    file_ref(doc, &mut sink, f, end, 1);
                } else if json {
                    json_line(doc, &mut sink, f, end);
                } else if is_comment(doc, f) && f == ls {
                    sink.push(f, end, K::Comment);
                } else {
                    with_vars(doc, &mut sink, f, end, K::Plain);
                }
                State::Body { json: Some(json) }
            }
            State::Handler { .. } => {
                if is_trailer(doc, ls, end) {
                    State::Handler { script: trailer_line(doc, &mut sink, ls, end) }
                } else {
                    state
                }
            }
        };
    }
    sink.into_spans()
}

/// Request blocks: (block start line, request line, last content line, title).
fn blocks(doc: &Doc) -> Vec<(usize, Option<usize>, usize, String)> {
    let mut out: Vec<(usize, Option<usize>, usize, String)> = Vec::new();
    let mut cur: Option<(usize, Option<usize>, usize, String)> = None;
    let mut name: Option<String> = None;
    for line in 0..doc.index.len_lines() {
        let ls = doc.index.line_start(line);
        let end = doc.trim_end(ls, doc.index.line_end(line));
        let first = doc.first_non_ws(line).filter(|&f| f < end);
        if doc.starts_with_at(ls, "###") {
            out.extend(cur.take());
            let n = doc.string(ls, end).trim_start_matches('#').trim().to_string();
            name = (!n.is_empty()).then_some(n);
            cur = Some((line, None, line, String::new()));
            continue;
        }
        let Some(f) = first else { continue };
        let block = cur.get_or_insert_with(|| (line, None, line, String::new()));
        block.2 = line;
        if block.1.is_none() {
            if is_comment(doc, f) {
                let t = doc.string(f, end);
                let t = t.trim_start_matches(['#', '/']).trim_start();
                if let Some(n) = t.strip_prefix("@name") {
                    let n = n.trim_start_matches([' ', '=']).trim();
                    if !n.is_empty() {
                        name = Some(n.to_string());
                    }
                }
            } else if doc.at(f) != '@' {
                block.1 = Some(line);
                block.3 = name.take().unwrap_or_else(|| doc.string(f, end));
            }
        }
    }
    out.extend(cur);
    out
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let mut out = Vec::new();
    for (start, request, last, _) in blocks(doc) {
        if request.is_some() {
            common::push_fold(&mut out, start, last, FoldKind::Block);
        }
    }
    let spans = spans(doc);
    let brackets = common::code_brackets(&doc.chars, &spans);
    common::bracket_folds(brackets.iter().map(|b| (b.ch, doc.index.line_of(b.offset))), &mut out);
    common::finish_folds(out)
}

pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    blocks(doc)
        .into_iter()
        .filter_map(|(_, request, _, title)| {
            let line = request?;
            let column = doc.first_non_ws(line).map_or(0, |f| f - doc.index.line_start(line));
            Some(OutlineItem { name: title, kind: SymbolKind::Section, line, column, depth: 0 })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_at, spans};
    use crate::{FoldKind, FoldRange, HighlightKind as K, Language, OutlineItem, SymbolKind, folding_ranges, outline};

    const H: Language = Language::Http;

    const SAMPLE: &str = "@host = localhost:{{port}}
### Users
# @name listUsers
GET http://{{host}}/users?page=1 HTTP/1.1
Accept: application/json
// skipped

###
POST https://example.org/items
    ?draft=true
Content-Type: application/json

{\"name\": \"{{$uuid}}\", \"n\": -1.5, \"ok\": true}

> {%
  client.log(\"x\");
%}
>> ./out.json

###
PUT /upload
Host: example.org

< ./file.bin
";

    #[test]
    fn lexes_requests() {
        assert_kind(H, SAMPLE, "@host", K::Key);
        assert_kind(H, SAMPLE, "=", K::Operator);
        assert_kind(H, SAMPLE, "localhost:", K::String);
        assert_kind(H, SAMPLE, "{{port}}", K::Macro);
        assert_kind(H, SAMPLE, "### Users", K::Comment);
        assert_kind(H, SAMPLE, "@name", K::Attribute);
        assert_kind(H, SAMPLE, "GET", K::Keyword);
        assert_kind(H, SAMPLE, "http://", K::Link);
        assert_kind(H, SAMPLE, "{{host}}", K::Macro);
        assert_kind(H, SAMPLE, "/users?page=1", K::Link);
        assert_kind(H, SAMPLE, "HTTP/1.1", K::Constant);
        assert_kind(H, SAMPLE, "Accept", K::Key);
        assert_kind(H, SAMPLE, "application/json", K::String);
        assert_kind(H, SAMPLE, "// skipped", K::Comment);
        assert_kind(H, SAMPLE, "?draft=true", K::Link);
        assert_kind(H, SAMPLE, "\"name\"", K::Key);
        assert_kind(H, SAMPLE, "{{$uuid}}", K::Macro);
        assert_kind(H, SAMPLE, "-1.5", K::Number);
        assert_kind(H, SAMPLE, "true", K::Boolean);
        assert_kind(H, SAMPLE, "{%", K::Code);
        assert_kind(H, SAMPLE, "  client.log(\"x\");", K::Code);
        assert_kind(H, SAMPLE, ">>", K::Operator);
        assert_kind(H, SAMPLE, "./out.json", K::Link);
        assert_kind(H, SAMPLE, "<", K::Operator);
        assert_kind(H, SAMPLE, "./file.bin", K::Link);
        assert_eq!(kind_at(H, SAMPLE, 21, 0), K::Key);
    }

    #[test]
    fn outline_and_folds() {
        assert_eq!(
            outline(H, SAMPLE),
            vec![
                OutlineItem { name: "listUsers".into(), kind: SymbolKind::Section, line: 3, column: 0, depth: 0 },
                OutlineItem {
                    name: "POST https://example.org/items".into(),
                    kind: SymbolKind::Section,
                    line: 8,
                    column: 0,
                    depth: 0
                },
                OutlineItem { name: "PUT /upload".into(), kind: SymbolKind::Section, line: 20, column: 0, depth: 0 },
            ]
        );
        let folds = folding_ranges(H, SAMPLE);
        assert!(folds.contains(&FoldRange { start_line: 1, end_line: 5, kind: FoldKind::Block }));
        assert!(folds.contains(&FoldRange { start_line: 7, end_line: 17, kind: FoldKind::Block }));
    }

    #[test]
    fn hostile_input() {
        for t in ["", "###", "{{", "GET {{", "> {%", "@", "@=", "\r\n\r\n", "POST x\n\n{\"a\n", "<>", ">>!"] {
            let _ = spans(H, t);
            let _ = folding_ranges(H, t);
            let _ = outline(H, t);
        }
    }
}
