//! Dockerfile: instructions (case-insensitive, at the start of a logical line), `--flag`
//! options, the base image and stage name of `FROM … AS name`, `KEY=value` pairs of
//! `ENV`/`ARG`/`LABEL`, strings, `$VAR`/`${VAR}` references, exposed ports, comments, parser
//! directives (`# syntax=…`, `# escape=…`) and line continuations (the escape character, `\`
//! by default, at the end of a line).

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

const INSTRUCTIONS: [&str; 18] = [
    "FROM",
    "RUN",
    "CMD",
    "LABEL",
    "MAINTAINER",
    "EXPOSE",
    "ENV",
    "ADD",
    "COPY",
    "ENTRYPOINT",
    "VOLUME",
    "USER",
    "WORKDIR",
    "ARG",
    "ONBUILD",
    "STOPSIGNAL",
    "HEALTHCHECK",
    "SHELL",
];

fn is_instruction(word: &str) -> bool {
    INSTRUCTIONS.iter().any(|i| i.eq_ignore_ascii_case(word))
}

/// Where the lexer is within the current instruction (kept across continuation lines).
#[derive(Default)]
struct State {
    /// The instruction, upper-case ("" for a line that does not start with one).
    instr: String,
    /// Arguments seen so far (flags excluded).
    args: usize,
    /// The previous argument was `AS` (in `FROM`).
    after_as: bool,
}

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    lex(doc).0
}

/// Spans plus, per line, whether it continues the previous line's instruction.
fn lex(doc: &Doc) -> (Vec<Span>, Vec<bool>) {
    let lines = doc.index.len_lines();
    let mut sink = SpanSink::with_capacity(doc.len() / 8);
    let mut continued_flags = vec![false; lines];
    let mut escape = '\\';
    let mut directives = true;
    let mut continued = false;
    let mut state = State::default();
    for line in 0..lines {
        if let Some(flag) = continued_flags.get_mut(line) {
            *flag = continued;
        }
        let le = doc.index.line_end(line);
        let Some(first) = doc.first_non_ws(line) else {
            directives = false;
            continue;
        };
        let end = doc.trim_end(first, le);
        if doc.at(first) == '#' {
            // Parser directives come first, before any instruction, comment or blank line.
            let text = doc.string(first + 1, end);
            let directive = directives && !continued && parse_directive(&text);
            if directive {
                if let Some(e) = directive_value(&text, "escape").and_then(|v| v.chars().next()) {
                    escape = if e == '`' { '`' } else { '\\' };
                }
                sink.push(first, end, K::Attribute);
            } else {
                directives = false;
                sink.push(first, end, K::Comment);
            }
            continue;
        }
        directives = false;
        let mut i = first;
        if !continued {
            state = State::default();
            let e = doc.scan_while(first, |c| c.is_ascii_alphabetic());
            let word = doc.string(first, e);
            if is_instruction(&word) {
                sink.push(first, e, K::Keyword);
                state.instr = word.to_ascii_uppercase();
                i = e;
                if state.instr == "ONBUILD" {
                    let s = doc.scan_while(e, |c| c == ' ' || c == '\t');
                    let e2 = doc.scan_while(s, |c| c.is_ascii_alphabetic());
                    let next = doc.string(s, e2);
                    if is_instruction(&next) {
                        sink.push(s, e2, K::Keyword);
                        state.instr = next.to_ascii_uppercase();
                        i = e2;
                    }
                }
            }
        }
        let cont = end > i && doc.at(end - 1) == escape;
        let body_end = if cont { end - 1 } else { end };
        args(doc, &mut sink, &mut state, i, body_end, escape);
        if cont {
            sink.push(end - 1, end, K::Operator);
        }
        continued = cont;
    }
    (sink.into_spans(), continued_flags)
}

/// `# name=value` with a known directive name.
fn parse_directive(text: &str) -> bool {
    let Some((name, _)) = text.split_once('=') else { return false };
    matches!(name.trim().to_ascii_lowercase().as_str(), "syntax" | "escape" | "check")
}

fn directive_value(text: &str, name: &str) -> Option<String> {
    let (n, v) = text.split_once('=')?;
    n.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
}

/// Instructions whose leading `--flag` options are highlighted.
fn takes_flags(instr: &str) -> bool {
    matches!(instr, "FROM" | "RUN" | "COPY" | "ADD" | "HEALTHCHECK")
}

/// Lex the arguments in `start..end` (one physical line).
fn args(doc: &Doc, sink: &mut SpanSink, state: &mut State, start: usize, end: usize, escape: char) {
    let mut i = start;
    while i < end {
        let c = doc.at(i);
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == escape {
            i = (i + 2).min(end);
            continue;
        }
        match c {
            '"' | '\'' => {
                let close = string_end(doc, i, end, c, escape);
                sink.push(i, close.unwrap_or(end), if close.is_some() { K::String } else { K::Invalid });
                i = close.unwrap_or(end);
                // A quoted word is an argument (`CMD ["a"]`, `ENV A="b"`).
                state.args += 1;
            }
            '$' => {
                let e = variable_end(doc, i, end);
                if e > i + 1 {
                    sink.push(i, e, K::Field);
                }
                i = e.max(i + 1);
            }
            '[' | ']' => {
                sink.push(i, i + 1, K::Bracket);
                i += 1;
            }
            _ => i = word(doc, sink, state, i, end, escape),
        }
    }
}

/// Past the closing `quote` of the string at `i` (escapes skipped), `None` if unterminated.
fn string_end(doc: &Doc, i: usize, end: usize, quote: char, escape: char) -> Option<usize> {
    let mut j = i + 1;
    while j < end {
        let c = doc.at(j);
        if c == escape && quote == '"' {
            j += 2;
            continue;
        }
        if c == quote {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// End of `$NAME`, `${…}` (within the line), or `i + 1` for a lone `$`.
fn variable_end(doc: &Doc, i: usize, end: usize) -> usize {
    match doc.at(i + 1) {
        '{' => (i + 2..end).find(|&j| doc.at(j) == '}').map_or(end, |j| j + 1),
        c if c.is_ascii_alphabetic() || c == '_' => {
            doc.scan_while(i + 1, |c| c.is_ascii_alphanumeric() || c == '_').min(end)
        }
        _ => i + 1,
    }
}

fn ends_word(c: char, escape: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '$' | '[' | ']') || c == escape
}

/// One unquoted word starting at `i`; returns its end.
fn word(doc: &Doc, sink: &mut SpanSink, state: &mut State, i: usize, end: usize, escape: char) -> usize {
    let e = doc.scan_while(i, |c| !ends_word(c, escape)).min(end);
    let e = e.max(i + 1);
    let text = doc.string(i, e);
    let instr = state.instr.as_str();
    // `--flag` / `--flag=value` before the first argument.
    if state.args == 0 && takes_flags(instr) && text.starts_with("--") {
        let name_end = text.find('=').map_or(e, |k| i + text.get(..k).map_or(0, |s| s.chars().count()));
        sink.push(i, name_end, K::Attribute);
        if name_end < e {
            sink.push(name_end, name_end + 1, K::Operator);
        }
        return e;
    }
    let first_word = state.args == 0;
    state.args += 1;
    match instr {
        "FROM" => {
            if text.eq_ignore_ascii_case("AS") && !first_word {
                sink.push(i, e, K::Keyword);
                state.after_as = true;
                return e;
            }
            if first_word {
                sink.push(i, e, K::Type);
            } else if state.after_as {
                sink.push(i, e, K::Function);
                state.after_as = false;
            }
        }
        "ENV" | "ARG" | "LABEL" => {
            match text.find('=') {
                Some(k) => {
                    let key_end = i + text.get(..k).map_or(0, |s| s.chars().count());
                    sink.push(i, key_end, K::Key);
                    sink.push(key_end, key_end + 1, K::Operator);
                }
                // Legacy `ENV KEY value`: the first word is the key.
                None if first_word => sink.push(i, e, K::Key),
                None => {}
            }
        }
        "EXPOSE" => {
            if text.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                sink.push(i, e, K::Number);
            }
        }
        "HEALTHCHECK" if first_word && (text.eq_ignore_ascii_case("CMD") || text.eq_ignore_ascii_case("NONE")) => {
            sink.push(i, e, K::Keyword);
        }
        "STOPSIGNAL" => sink.push(i, e, K::Constant),
        _ => {}
    }
    e
}

/// Comment runs and multi-line (continued) instructions.
pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let (spans, continued) = lex(doc);
    let mut out = Vec::new();
    common::flag_runs(&common::hash_comment_lines(doc, &spans), FoldKind::Comment, &mut out);
    let mut start: Option<usize> = None;
    for (line, &c) in continued.iter().enumerate() {
        match (c, start) {
            (false, Some(s)) => {
                common::push_fold(&mut out, s, line.saturating_sub(1), FoldKind::Block);
                start = None;
            }
            (true, None) => start = Some(line.saturating_sub(1)),
            _ => {}
        }
    }
    if let Some(s) = start {
        common::push_fold(&mut out, s, continued.len().saturating_sub(1), FoldKind::Block);
    }
    common::finish_folds(out)
}

/// Build stages: `FROM image AS name` (named after the stage, else the image).
pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    let (spans, continued) = lex(doc);
    let mut out = Vec::new();
    for s in spans.iter().filter(|s| s.kind == K::Keyword) {
        let line = doc.index.line_of(s.start);
        if continued.get(line).copied().unwrap_or(false) || !doc.string(s.start, s.end).eq_ignore_ascii_case("FROM") {
            continue;
        }
        let le = doc.index.line_end(line);
        let on_line = |k: K| spans.iter().find(|x| x.kind == k && x.start > s.start && x.start < le);
        let Some(named) = on_line(K::Function).or_else(|| on_line(K::Type)) else { continue };
        out.push(OutlineItem {
            name: doc.string(named.start, named.end),
            kind: SymbolKind::Module,
            line,
            column: named.start.saturating_sub(doc.index.line_start(line)),
            depth: 0,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_of};
    use crate::{FoldKind, FoldRange, HighlightKind as K, Language, folding_ranges, outline};

    const D: Language = Language::Dockerfile;

    #[test]
    fn instructions_images_and_stages() {
        let text = "# syntax=docker/dockerfile:1\n# build\nFROM --platform=$BUILDPLATFORM rust:1.80 AS builder\nworkdir /app\nCOPY --from=builder /app/target/release/app /usr/local/bin/app\nEXPOSE 8080/tcp 9090\nSTOPSIGNAL SIGTERM\n";
        assert_kind(D, text, "# syntax=docker/dockerfile:1", K::Attribute);
        assert_kind(D, text, "# build", K::Comment);
        assert_kind(D, text, "FROM", K::Keyword);
        assert_kind(D, text, "--platform", K::Attribute);
        assert_kind(D, text, "$BUILDPLATFORM", K::Field);
        assert_kind(D, text, "rust:1.80", K::Type);
        assert_kind(D, text, "AS", K::Keyword);
        assert_kind(D, text, "builder", K::Function);
        assert_kind(D, text, "workdir", K::Keyword);
        assert_kind(D, text, "--from", K::Attribute);
        assert_kind(D, text, "8080/tcp", K::Number);
        assert_kind(D, text, "9090", K::Number);
        assert_kind(D, text, "SIGTERM", K::Constant);
    }

    #[test]
    fn env_strings_exec_form_and_continuations() {
        let text = "ENV RUST_LOG=debug NAME=\"a b\"\nARG VERSION\nCMD [\"app\", \"--port\", \"80\"]\nRUN apt-get update && \\\n    apt-get install -y curl \\\n    # comment inside\n    && rm -rf /var/lib/apt/lists/*\nONBUILD RUN make\nHEALTHCHECK --interval=5s CMD curl -f http://localhost/ || exit 1\n";
        assert_kind(D, text, "RUST_LOG", K::Key);
        assert_kind(D, text, "\"a b\"", K::String);
        assert_kind(D, text, "VERSION", K::Key);
        assert_kind(D, text, "[", K::Bracket);
        assert_kind(D, text, "\"--port\"", K::String);
        assert_kind(D, text, "\\", K::Operator);
        assert_kind(D, text, "# comment inside", K::Comment);
        // `apt-get` on a continuation line is not an instruction.
        assert_eq!(kind_of(D, text, "apt-get"), None);
        assert_kind(D, text, "ONBUILD", K::Keyword);
        assert_kind(D, text, "RUN", K::Keyword);
        assert_kind(D, text, "--interval", K::Attribute);
        assert_kind(D, text, "CMD", K::Keyword);
    }

    #[test]
    fn escape_directive_and_unterminated_strings() {
        let text = "# escape=`\nRUN echo a `\n  b\nFROM x\nCMD \"open";
        assert_kind(D, text, "# escape=`", K::Attribute);
        assert_kind(D, text, "`", K::Operator);
        assert_eq!(kind_of(D, text, "b"), None);
        assert_kind(D, text, "\"open", K::Invalid);
        // A directive after an instruction is a plain comment.
        assert_kind(D, "FROM x\n# syntax=y", "# syntax=y", K::Comment);
    }

    #[test]
    fn folds_and_outline() {
        let text = "# a\n# b\nFROM rust AS build\nRUN a \\\n  b \\\n  c\nFROM debian\nCOPY x y\n";
        let folds = folding_ranges(D, text);
        assert!(folds.contains(&FoldRange { start_line: 0, end_line: 1, kind: FoldKind::Comment }), "{folds:?}");
        assert!(folds.contains(&FoldRange { start_line: 3, end_line: 5, kind: FoldKind::Block }), "{folds:?}");
        let items: Vec<(String, usize)> = outline(D, text).into_iter().map(|i| (i.name, i.line)).collect();
        assert_eq!(items, vec![("build".to_string(), 2), ("debian".to_string(), 6)]);
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for text in
            ["", "\\", "FROM", "FROM --", "ENV =", "${", "$", "\"", "ONBUILD", "# escape=", "RUN \\\n", "é\\\nü"]
        {
            let _ = crate::highlight(D, text);
            let _ = folding_ranges(D, text);
            let _ = outline(D, text);
        }
    }
}
