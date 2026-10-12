//! POSIX shell / bash: comments, strings (with `$var` inside double quotes), variables,
//! keywords, here-documents, operators and function definitions.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K};

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    let mut lexer = Lexer {
        doc,
        sink: SpanSink::with_capacity(doc.len() / 6),
        i: 0,
        command: true,
        for_state: ForState::None,
        after_function: false,
        heredocs: Vec::new(),
    };
    lexer.run();
    lexer.sink.into_spans()
}

/// Tracks `for NAME in` / `case WORD in` / `select NAME in`, where `in` is a keyword.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ForState {
    None,
    /// After `for`/`case`/`select`: the next word is the name.
    Name,
    /// After the name: `in` is a keyword.
    In,
}

struct Lexer<'a> {
    doc: &'a Doc,
    sink: SpanSink,
    i: usize,
    /// At command position (where keywords and assignments are recognized).
    command: bool,
    for_state: ForState,
    after_function: bool,
    /// Pending here-documents: (delimiter, strip leading tabs).
    heredocs: Vec<(Vec<char>, bool)>,
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "function"
            | "while"
            | "until"
            | "select"
            | "in"
    )
}

/// Chars that end an unquoted word.
fn ends_word(c: char) -> bool {
    c.is_whitespace()
        || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>' | '\'' | '"' | '`' | '$' | '\\' | '{' | '}' | '[' | ']')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl Lexer<'_> {
    fn run(&mut self) {
        let doc = self.doc;
        let n = doc.len();
        while self.i < n {
            let i = self.i;
            let c = doc.at(i);
            let next = match c {
                '\n' => {
                    self.command = true;
                    if self.heredocs.is_empty() { i + 1 } else { self.heredoc_bodies(i + 1) }
                }
                c if c.is_whitespace() => i + 1,
                '\\' => i + 2,
                '#' if i == 0 || ends_word(doc.at(i - 1)) => {
                    let e = doc.line_end_at(i);
                    self.sink.push(i, e, K::Comment);
                    e
                }
                '\'' => {
                    let e = doc.find_str(i + 1, "'").map(|j| j + 1);
                    self.sink.push(i, e.unwrap_or(n), if e.is_some() { K::String } else { K::Invalid });
                    self.word_seen("");
                    e.unwrap_or(n)
                }
                '"' => {
                    let e = self.double_quoted(i);
                    self.word_seen("");
                    e
                }
                '$' => self.dollar(i),
                '`' => {
                    self.sink.push(i, i + 1, K::Operator);
                    i + 1
                }
                '(' | ')' | '{' | '}' | '[' | ']' => {
                    self.sink.push(i, i + 1, K::Bracket);
                    self.command = matches!(c, '(' | '{');
                    i + 1
                }
                ';' | '|' | '&' => {
                    let e = doc.scan_while(i, |c| matches!(c, ';' | '|' | '&'));
                    self.sink.push(i, e, K::Operator);
                    self.command = true;
                    e
                }
                '<' | '>' => self.redirect(i),
                _ => self.word(i),
            };
            self.i = next.max(i + 1);
        }
    }

    /// Notes a non-keyword word for `for x in` tracking.
    fn word_seen(&mut self, word: &str) {
        self.command = false;
        self.for_state = match self.for_state {
            ForState::Name => ForState::In,
            ForState::In if word == "in" => ForState::None,
            _ => ForState::None,
        };
    }

    fn word(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let e = doc.scan_while(i, |c| !ends_word(c)).max(i + 1);
        let word = doc.string(i, e);
        if self.command
            && let Some(name_len) = assignment_name_len(&word)
        {
            self.sink.push(i, i + name_len, K::Field);
            let op_len = if word.chars().nth(name_len) == Some('+') { 2 } else { 1 };
            self.sink.push(i + name_len, i + name_len + op_len, K::Operator);
            let value = i + name_len + op_len;
            if value < e && doc.slice(value, e).iter().all(char::is_ascii_digit) {
                self.sink.push(value, e, K::Number);
            }
            return e;
        }
        let keyword = is_keyword(&word) && (self.command || (word == "in" && self.for_state == ForState::In));
        if keyword {
            self.sink.push(i, e, K::Keyword);
            match word.as_str() {
                "for" | "case" | "select" => {
                    self.for_state = ForState::Name;
                    self.command = false;
                }
                "function" => {
                    self.after_function = true;
                    self.command = false;
                }
                "in" | "done" | "fi" | "esac" => {
                    self.for_state = ForState::None;
                    self.command = false;
                }
                _ => self.command = true,
            }
            return e;
        }
        if std::mem::take(&mut self.after_function) || self.is_function_definition(e) {
            self.sink.push(i, e, K::Function);
        } else if word.chars().all(|c| c.is_ascii_digit()) {
            self.sink.push(i, e, K::Number);
        }
        self.word_seen(&word);
        e
    }

    /// `name()` follows: the word ending at `e` names a function.
    fn is_function_definition(&self, e: usize) -> bool {
        let doc = self.doc;
        let open = doc.scan_while(e, |c| c == ' ' || c == '\t');
        doc.at(open) == '(' && doc.at(doc.scan_while(open + 1, |c| c == ' ' || c == '\t')) == ')'
    }

    /// A `"…"` string; `$var`, `${…}` inside are variables, `$(…)` and backticks are left plain.
    fn double_quoted(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let mut seg = i;
        let mut j = i + 1;
        while j < n {
            match doc.at(j) {
                '\\' => j += 2,
                '"' => {
                    self.sink.push(seg, j + 1, K::String);
                    return j + 1;
                }
                '$' => {
                    if let Some(e) = variable_end(doc, j) {
                        self.sink.push(seg, j, K::String);
                        self.sink.push(j, e, K::Field);
                        seg = e;
                        j = e;
                    } else if doc.at(j + 1) == '(' {
                        self.sink.push(seg, j, K::String);
                        let e = group_end(doc, j + 1, '(', ')');
                        seg = e;
                        j = e;
                    } else {
                        j += 1;
                    }
                }
                '`' => {
                    self.sink.push(seg, j, K::String);
                    let e = doc.find_str(j + 1, "`").map_or(n, |k| k + 1);
                    seg = e;
                    j = e;
                }
                _ => j += 1,
            }
        }
        self.sink.push(seg, n, K::Invalid);
        n
    }

    fn dollar(&mut self, i: usize) -> usize {
        let doc = self.doc;
        if let Some(e) = variable_end(doc, i) {
            self.sink.push(i, e, K::Field);
            self.word_seen("");
            return e;
        }
        match doc.at(i + 1) {
            '\'' => {
                // `$'…'` (ANSI-C quoting): backslash escapes apply.
                let mut j = i + 2;
                while j < doc.len() && doc.at(j) != '\'' {
                    j += if doc.at(j) == '\\' { 2 } else { 1 };
                }
                let e = (j + 1).min(doc.len());
                self.sink.push(i, e, if j < doc.len() { K::String } else { K::Invalid });
                self.word_seen("");
                e
            }
            '(' | '"' => {
                self.sink.push(i, i + 1, K::Operator);
                i + 1
            }
            _ => i + 1,
        }
    }

    /// Redirections; `<<`/`<<-` also register a here-document.
    fn redirect(&mut self, i: usize) -> usize {
        let doc = self.doc;
        if doc.starts_with_at(i, "<<<") {
            self.sink.push(i, i + 3, K::Operator);
            return i + 3;
        }
        if doc.starts_with_at(i, "<<") {
            let strip_tabs = doc.at(i + 2) == '-';
            let op_end = if strip_tabs { i + 3 } else { i + 2 };
            self.sink.push(i, op_end, K::Operator);
            let start = doc.scan_while(op_end, |c| c == ' ' || c == '\t');
            let (delimiter, end) = match doc.at(start) {
                q @ ('\'' | '"') => {
                    let close = doc.slice(start + 1, doc.line_end_at(start)).iter().position(|&c| c == q);
                    match close {
                        Some(p) => (doc.slice(start + 1, start + 1 + p).to_vec(), start + p + 2),
                        None => (Vec::new(), start),
                    }
                }
                '\\' => {
                    let e = doc.scan_while(start + 1, |c| !ends_word(c));
                    (doc.slice(start + 1, e).to_vec(), e)
                }
                _ => {
                    let e = doc.scan_while(start, |c| !ends_word(c));
                    (doc.slice(start, e).to_vec(), e)
                }
            };
            if !delimiter.is_empty() {
                self.sink.push(start, end, K::String);
                self.heredocs.push((delimiter, strip_tabs));
            }
            return end.max(op_end);
        }
        let e = doc.scan_while(i, |c| c == '<' || c == '>');
        let e = if matches!(doc.at(e), '&' | '|') { e + 1 } else { e };
        self.sink.push(i, e, K::Operator);
        e
    }

    /// Consumes the bodies of pending here-documents starting at `start` (a line start);
    /// returns the offset after the last terminator line.
    fn heredoc_bodies(&mut self, start: usize) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let mut i = start;
        for (delimiter, strip_tabs) in std::mem::take(&mut self.heredocs) {
            let body = i;
            loop {
                if i >= n {
                    self.sink.push(body, n, K::String);
                    return n;
                }
                let le = doc.line_end_at(i);
                let mut first = i;
                if strip_tabs {
                    first = doc.scan_while(i, |c| c == '\t').min(le);
                }
                let last = if doc.at(le.saturating_sub(1)) == '\r' && le > first { le - 1 } else { le };
                let is_end = doc.slice(first, last) == delimiter.as_slice();
                i = le + 1;
                if is_end {
                    self.sink.push(body, le, K::String);
                    break;
                }
            }
        }
        i.min(n)
    }
}

/// `NAME=` / `NAME+=` at the start of a word: the name's length.
fn assignment_name_len(word: &str) -> Option<usize> {
    let mut chars = word.chars();
    if !chars.next().is_some_and(is_name_start) {
        return None;
    }
    let len = 1 + chars.clone().take_while(|&c| is_name_char(c)).count();
    let rest: String = word.chars().skip(len).take(2).collect();
    (rest.starts_with('=') || rest == "+=").then_some(len)
}

/// End of a variable reference at `i` (`$name`, `${…}`, `$1`, `$@`, …).
fn variable_end(doc: &Doc, i: usize) -> Option<usize> {
    let c = doc.at(i + 1);
    if c == '{' {
        return Some(group_end(doc, i + 1, '{', '}'));
    }
    if is_name_start(c) {
        return Some(doc.scan_while(i + 1, is_name_char));
    }
    (c.is_ascii_digit() || matches!(c, '@' | '*' | '#' | '?' | '$' | '!' | '-')).then_some(i + 2)
}

/// Past the `close` matching the `open` at `i` (nesting counted), or the text end.
fn group_end(doc: &Doc, i: usize, open: char, close: char) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    while j < doc.len() {
        let c = doc.at(j);
        if c == '\\' {
            j += 2;
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return j + 1;
            }
        }
        j += 1;
    }
    doc.len()
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let spans = spans(doc);
    let mut out = Vec::new();
    let brackets = common::code_brackets(&doc.chars, &spans);
    common::bracket_folds(brackets.iter().map(|b| (b.ch, doc.index.line_of(b.offset))), &mut out);
    // `if … fi`, `case … esac`, `do … done`.
    let mut open: Vec<(char, usize)> = Vec::new();
    for span in spans.iter().filter(|s| s.kind == K::Keyword) {
        let line = doc.index.line_of(span.start);
        let mark = match doc.string(span.start, span.end).as_str() {
            "if" => Some(('i', true)),
            "fi" => Some(('i', false)),
            "case" => Some(('c', true)),
            "esac" => Some(('c', false)),
            "do" => Some(('d', true)),
            "done" => Some(('d', false)),
            _ => None,
        };
        match mark {
            Some((kind, true)) => open.push((kind, line)),
            Some((kind, false)) => {
                if let Some(at) = open.iter().rposition(|&(k, _)| k == kind) {
                    if let Some(&(_, start)) = open.get(at) {
                        common::push_fold(&mut out, start, line, FoldKind::Block);
                    }
                    open.truncate(at);
                }
            }
            None => {}
        }
    }
    common::flag_runs(&common::hash_comment_lines(doc, &spans), FoldKind::Comment, &mut out);
    common::finish_folds(out)
}

#[cfg(test)]
mod tests {
    use crate::test_util::{assert_kind, kind_at, kind_of, offset_of, spans};
    use crate::{FoldKind, FoldRange, HighlightKind as K, Language, folding_ranges, matching_bracket, outline};

    const S: Language = Language::Shell;

    #[test]
    fn comments_strings_variables() {
        let text = "#!/bin/sh\necho \"hi $USER ${HOME}/x\" 'lit $x' $1 $@ # done\nurl=a#b";
        assert_kind(S, text, "#!/bin/sh", K::Comment);
        assert_kind(S, text, "\"hi ", K::String);
        assert_kind(S, text, "$USER", K::Field);
        assert_kind(S, text, "${HOME}", K::Field);
        assert_kind(S, text, "/x\"", K::String);
        assert_kind(S, text, "'lit $x'", K::String);
        assert_kind(S, text, "$1", K::Field);
        assert_kind(S, text, "$@", K::Field);
        assert_kind(S, text, "# done", K::Comment);
        assert_eq!(kind_of(S, text, "a#b"), None);
        assert_kind(S, text, "url", K::Field);
    }

    #[test]
    fn keywords_only_in_command_position() {
        let text = "if [ -f x ]; then\n  echo done in fi\nfi\nfor f in a b; do echo $f; done";
        assert_eq!(kind_at(S, text, 0, 0), K::Keyword);
        assert_kind(S, text, "then", K::Keyword);
        assert_eq!(kind_at(S, text, 1, 7), K::Plain); // `done` as an argument
        assert_eq!(kind_at(S, text, 1, 12), K::Plain); // `in` as an argument
        assert_eq!(kind_at(S, text, 2, 0), K::Keyword);
        assert_eq!(kind_at(S, text, 3, 6), K::Keyword); // `in` after `for f`
        assert_kind(S, text, "do", K::Keyword);
        assert_kind(S, text, "done", K::Keyword);
        assert_kind(S, "case $x in\n a) ;;\nesac", "in", K::Keyword);
    }

    #[test]
    fn assignments_functions_numbers_operators() {
        let text = "N=3 PATH+=:/x cmd 2>&1 | grep -c x && true\nfunction build {\n:\n}\nhelper() { :; }";
        assert_kind(S, text, "N", K::Field);
        assert_kind(S, text, "=", K::Operator);
        assert_kind(S, text, "3", K::Number);
        assert_kind(S, text, "PATH", K::Field);
        assert_kind(S, text, "+=", K::Operator);
        assert_kind(S, text, "2", K::Number);
        assert_kind(S, text, ">&", K::Operator);
        assert_kind(S, text, "|", K::Operator);
        assert_kind(S, text, "&&", K::Operator);
        assert_kind(S, text, "function", K::Keyword);
        assert_kind(S, text, "build", K::Function);
        assert_kind(S, text, "helper", K::Function);
        assert_kind(S, text, "{", K::Bracket);
    }

    #[test]
    fn heredocs_are_strings() {
        let text = "cat <<EOF\nDon't $stop\nEOF\necho 'after'\ncat <<-'X'\n\tbody\n\tX\nls";
        assert_kind(S, text, "<<", K::Operator);
        assert_kind(S, text, "Don't $stop", K::String);
        assert_kind(S, text, "EOF", K::String);
        assert_kind(S, text, "'after'", K::String);
        assert_kind(S, text, "<<-", K::Operator);
        assert_kind(S, text, "\tbody", K::String);
        assert_eq!(kind_at(S, text, 7, 0), K::Plain);
        // Unterminated: the body runs to the end.
        assert_eq!(kind_at(S, "cat <<E\nx\ny", 2, 0), K::String);
    }

    #[test]
    fn unterminated_quotes_are_invalid() {
        assert_kind(S, "echo 'oops", "'oops", K::Invalid);
        assert_kind(S, "echo \"oops", "\"oops", K::Invalid);
    }

    #[test]
    fn command_substitution_in_double_quotes() {
        let all = spans(S, "x=\"a $(date \"+%s\") b\"");
        assert!(all.contains(&("\"a ".to_string(), K::String)), "{all:?}");
        assert!(all.contains(&(" b\"".to_string(), K::String)), "{all:?}");
    }

    #[test]
    fn non_ascii_columns() {
        let h = crate::test_util::checked(S, "echo \"ü $x\" # ä");
        let cols: Vec<(usize, usize, K)> = h.line(0).iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(cols, [(5, 8, K::String), (8, 10, K::Field), (10, 11, K::String), (12, 15, K::Comment)]);
    }

    #[test]
    fn brackets_skip_strings_and_comments() {
        let text = "f() {\n  echo \"}\" # }\n}";
        assert_eq!(matching_bracket(S, text, offset_of(text, "{", 0)), Some(text.chars().count() - 1));
        assert_eq!(matching_bracket(S, text, 1), Some(2));
    }

    #[test]
    fn folds_blocks_keywords_comments() {
        let text = "# a\n# b\nif x; then\n  y\nfi\nf() {\n  for i in 1 2; do\n    :\n  done\n}\n";
        assert_eq!(
            folding_ranges(S, text),
            vec![
                FoldRange { start_line: 0, end_line: 1, kind: FoldKind::Comment },
                FoldRange { start_line: 2, end_line: 4, kind: FoldKind::Block },
                FoldRange { start_line: 5, end_line: 9, kind: FoldKind::Block },
                FoldRange { start_line: 6, end_line: 8, kind: FoldKind::Block },
            ]
        );
        assert!(outline(S, text).is_empty());
    }
}
