//! Django and Jinja2 templates: HTML with template syntax layered on top.
//!
//! Highlighting:
//!
//! - HTML: tag names → `Keyword`, attribute names → `Key`, attribute values → `String`, `<`,
//!   `</`, `>`, `/>` → `Punctuation`, `<!-- … -->` → `Comment`, `<!DOCTYPE …>` → `Attribute`,
//!   entities (`&amp;`, `&#38;`) → `Constant`. `<script>`/`<style>` bodies are left plain.
//! - Template syntax (also inside HTML tags and attribute values): the delimiters `{{ }}`,
//!   `{% %}` (with their `-`/`+` whitespace control) → `Macro`; `{# … #}` and
//!   `{% comment %} … {% endcomment %}` → `Comment`; the tag name after `{%` → `Keyword`;
//!   keywords (`and or not in is if else …`) → `Keyword`; `true false none True False None` →
//!   `Boolean`; strings → `String`; numbers → `Number`; filter names after `|` and test names
//!   after `is` → `Function`; names after `.` → `Field`; other names → `Ident`; operators and
//!   `|` → `Operator`; brackets → `Bracket`; `, : .` → `Punctuation`. The content of
//!   `{% raw %}` / `{% verbatim %}` blocks is plain.
//!
//! Unterminated constructs extend to the end of the text (an unterminated string inside a tag
//! ends at the tag's closing delimiter).
//!
//! Folding: block tag pairs (`{% block %}…{% endblock %}`, `for`, `if`, `with`, `macro`, …),
//! multi-line HTML elements, and multi-line comments. Outline: `{% block name %}` (`Section`
//! named `block name`) and `{% macro name(…) %}` (`Function`), nested by block.

use crate::common::{self, Doc, Span, SpanSink};
use crate::{FoldKind, FoldRange, HighlightKind as K, OutlineItem, SymbolKind};

/// Template tags that open a block closed by `end<tag>` (`set` only in its block form, without
/// `=`).
pub const BLOCK_TAGS: &[&str] = &[
    "autoescape",
    "block",
    "blocktrans",
    "blocktranslate",
    "cache",
    "call",
    "comment",
    "filter",
    "for",
    "if",
    "ifchanged",
    "ifequal",
    "ifnotequal",
    "language",
    "localize",
    "localtime",
    "macro",
    "partialdef",
    "raw",
    "set",
    "spaceless",
    "timezone",
    "verbatim",
    "with",
];

/// Built-in Django and Jinja2 tags with a short description.
pub const TAGS: &[(&str, &str)] = &[
    ("autoescape", "Control auto-escaping in the block"),
    ("block", "Define a block that child templates can override"),
    ("blocktrans", "Translate a block of text (Django)"),
    ("blocktranslate", "Translate a block of text (Django)"),
    ("break", "Leave the loop (Jinja loop controls)"),
    ("cache", "Cache a template fragment (Django)"),
    ("call", "Call a macro with a caller block (Jinja)"),
    ("comment", "Ignore everything up to endcomment (Django)"),
    ("continue", "Skip to the next iteration (Jinja loop controls)"),
    ("csrf_token", "Insert the CSRF token (Django)"),
    ("cycle", "Cycle among values on each loop iteration (Django)"),
    ("debug", "Output debugging information"),
    ("do", "Evaluate an expression without output (Jinja)"),
    ("elif", "Else-if branch"),
    ("else", "Else branch"),
    ("empty", "Content when the for loop has no items (Django)"),
    ("extends", "Inherit from a parent template"),
    ("filter", "Apply filters to the block"),
    ("firstof", "Output the first true argument (Django)"),
    ("for", "Loop over a sequence"),
    ("from", "Import names from another template (Jinja)"),
    ("if", "Conditional block"),
    ("ifchanged", "Output only when the value changed (Django)"),
    ("include", "Render another template here"),
    ("import", "Import a template's macros (Jinja)"),
    ("language", "Switch the active language (Django)"),
    ("load", "Load a template tag library (Django)"),
    ("localize", "Control localization of values (Django)"),
    ("localtime", "Control time zone conversion (Django)"),
    ("lorem", "Random lorem ipsum text (Django)"),
    ("macro", "Define a reusable macro (Jinja)"),
    ("now", "The current date and time (Django)"),
    ("partial", "Render a named partial (Django)"),
    ("partialdef", "Define a named partial (Django)"),
    ("querystring", "Build a URL query string (Django)"),
    ("raw", "Output the content unprocessed (Jinja)"),
    ("regroup", "Group a list by an attribute (Django)"),
    ("resetcycle", "Reset a cycle (Django)"),
    ("set", "Assign a variable (Jinja)"),
    ("spaceless", "Remove whitespace between HTML tags"),
    ("static", "Link to a static file (Django)"),
    ("templatetag", "Output a template syntax character (Django)"),
    ("timezone", "Set the time zone (Django)"),
    ("trans", "Translate a string"),
    ("translate", "Translate a string (Django)"),
    ("url", "Reverse a URL by name (Django)"),
    ("verbatim", "Output the content unprocessed (Django)"),
    ("widthratio", "Compute a ratio (Django)"),
    ("with", "Bind variables for the block"),
];

/// Built-in Django and Jinja2 filters.
pub const FILTERS: &[&str] = &[
    "abs",
    "add",
    "addslashes",
    "attr",
    "batch",
    "capfirst",
    "capitalize",
    "center",
    "count",
    "cut",
    "date",
    "default",
    "default_if_none",
    "dictsort",
    "dictsortreversed",
    "divisibleby",
    "e",
    "escape",
    "escapejs",
    "escapeseq",
    "filesizeformat",
    "first",
    "float",
    "floatformat",
    "force_escape",
    "forceescape",
    "format",
    "get_digit",
    "groupby",
    "indent",
    "int",
    "iriencode",
    "items",
    "join",
    "json_script",
    "last",
    "length",
    "length_is",
    "linebreaks",
    "linebreaksbr",
    "linenumbers",
    "list",
    "ljust",
    "lower",
    "make_list",
    "map",
    "max",
    "min",
    "phone2numeric",
    "pluralize",
    "pprint",
    "random",
    "reject",
    "rejectattr",
    "replace",
    "reverse",
    "rjust",
    "round",
    "safe",
    "safeseq",
    "select",
    "selectattr",
    "slice",
    "slugify",
    "sort",
    "string",
    "stringformat",
    "striptags",
    "sum",
    "time",
    "timesince",
    "timeuntil",
    "title",
    "tojson",
    "trim",
    "truncate",
    "truncatechars",
    "truncatechars_html",
    "truncatewords",
    "truncatewords_html",
    "unique",
    "unordered_list",
    "upper",
    "urlencode",
    "urlize",
    "urlizetrunc",
    "wordcount",
    "wordwrap",
    "xmlattr",
    "yesno",
];

const KEYWORDS: &[&str] = &[
    "and",
    "or",
    "not",
    "in",
    "is",
    "if",
    "else",
    "as",
    "with",
    "without",
    "only",
    "ignore",
    "missing",
    "recursive",
    "scoped",
    "context",
    "import",
    "from",
    "by",
    "reversed",
];

const BOOLEANS: &[&str] = &["true", "false", "none", "True", "False", "None"];

/// HTML elements without an end tag.
const VOID: &[&str] =
    &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// How far down the open-block stack an end tag looks for its opener.
const MAX_RECOVERY: usize = 64;

/// How far back [`completion_context`] looks for the start of a template construct.
const MAX_LOOKBACK: usize = 4096;

/// A string literal in a template tag that names another template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateRef {
    /// The tag (`extends`, `include`, `import`, `from`, `embed`).
    pub tag: String,
    /// The template name (the string's contents).
    pub name: String,
    /// Char offsets of the string's contents (without the quotes).
    pub start: usize,
    pub end: usize,
}

/// What [`completion_context`] found before the caret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// A tag name after `{%`.
    Tag,
    /// A tag name starting with `end` after `{%`.
    EndTag,
    /// A filter name after `|`.
    Filter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagCompletion {
    pub kind: CompletionKind,
    /// Char offset where the partial word starts (the caret when nothing is typed yet).
    pub start: usize,
    /// The partial word typed so far.
    pub prefix: String,
}

/// A `{% … %}` construct.
#[derive(Clone, Debug, Default)]
struct Tag {
    name: String,
    start: usize,
    end: usize,
    /// The first name after the tag name (`content` in `{% block content %}`).
    arg: Option<(String, usize)>,
    /// String literals: (contents, start, end, before any other name).
    strings: Vec<(String, usize, usize, bool)>,
    /// An `=` assignment appears (`{% set x = 1 %}` is not a block).
    assign: bool,
}

/// An HTML start or end tag.
#[derive(Clone, Debug)]
struct Element {
    name: String,
    close: bool,
    self_closing: bool,
    start: usize,
}

struct Lexed {
    spans: Vec<Span>,
    tags: Vec<Tag>,
    elements: Vec<Element>,
}

#[derive(Default)]
struct CodeInfo {
    first: Option<(String, usize)>,
    second: Option<(String, usize)>,
    strings: Vec<(String, usize, usize, bool)>,
    assign: bool,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_template_start(doc: &Doc, i: usize) -> bool {
    doc.at(i) == '{' && matches!(doc.at(i + 1), '{' | '%' | '#')
}

struct Lexer<'a> {
    doc: &'a Doc,
    sink: SpanSink,
    tags: Vec<Tag>,
    elements: Vec<Element>,
}

fn lex(doc: &Doc) -> Lexed {
    let mut lx = Lexer { doc, sink: SpanSink::with_capacity(doc.len() / 6), tags: Vec::new(), elements: Vec::new() };
    let n = doc.len();
    let mut i = 0;
    while i < n {
        let c = doc.at(i);
        if is_template_start(doc, i) {
            i = lx.template(i).max(i + 1);
            continue;
        }
        if c == '<'
            && let Some(e) = lx.markup(i)
        {
            i = e.max(i + 1);
            continue;
        }
        if c == '&'
            && let Some(e) = entity_end(doc, i)
        {
            lx.sink.push(i, e, K::Constant);
            i = e;
            continue;
        }
        i += 1;
    }
    Lexed { spans: lx.sink.into_spans(), tags: lx.tags, elements: lx.elements }
}

/// `&name;`, `&#123;` or `&#x1F;` at `i`: its end.
fn entity_end(doc: &Doc, i: usize) -> Option<usize> {
    let mut j = i + 1;
    let numeric = doc.at(j) == '#';
    if numeric {
        j += 1;
        if matches!(doc.at(j), 'x' | 'X') {
            j += 1;
        }
    }
    let body = j;
    while j < doc.len() && j - body < 32 && doc.at(j).is_ascii_alphanumeric() {
        j += 1;
    }
    (j > body && doc.at(j) == ';').then_some(j + 1)
}

impl Lexer<'_> {
    /// A template construct at `i` (`{{`, `{%` or `{#`): its end.
    fn template(&mut self, i: usize) -> usize {
        let doc = self.doc;
        match doc.at(i + 1) {
            '#' => {
                let end = doc.find_str(i + 2, "#}").map_or(doc.len(), |e| e + 2);
                self.sink.push(i, end, K::Comment);
                end
            }
            '{' => {
                let body = open_end(doc, i);
                self.sink.push(i, body, K::Macro);
                self.code(body, '}').0
            }
            _ => self.block_tag(i),
        }
    }

    /// `{% … %}` at `i`.
    fn block_tag(&mut self, i: usize) -> usize {
        let doc = self.doc;
        let body = open_end(doc, i);
        self.sink.push(i, body, K::Macro);
        let (end, info) = self.code(body, '%');
        let name = info.first.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
        self.tags.push(Tag {
            name: name.clone(),
            start: i,
            end,
            arg: info.second,
            strings: info.strings,
            assign: info.assign,
        });
        match name.as_str() {
            "comment" => {
                // Everything up to and including `{% endcomment %}` is a comment.
                let (close_start, close_end) = match find_end_tag(doc, end, "endcomment") {
                    Some(s) => (s, tag_end(doc, s)),
                    None => (doc.len(), doc.len()),
                };
                self.sink.truncate_from(i);
                self.sink.push(i, close_end, K::Comment);
                if close_start < doc.len() {
                    self.tags.push(Tag {
                        name: "endcomment".into(),
                        start: close_start,
                        end: close_end,
                        ..Tag::default()
                    });
                }
                close_end
            }
            "raw" | "verbatim" => {
                // Plain content; the end tag is lexed normally.
                find_end_tag(doc, end, &format!("end{name}")).unwrap_or(doc.len())
            }
            _ => end,
        }
    }

    /// Template code from `p` up to the closing `close}` delimiter (`}` for `}}`, `%` for
    /// `%}`). Returns the end of the construct and what it contains.
    fn code(&mut self, mut p: usize, close: char) -> (usize, CodeInfo) {
        let doc = self.doc;
        let n = doc.len();
        let mut info = CodeInfo::default();
        // The previous significant token: '|', '.', 'i' (the word `is`), 'n' (`is not`), or ' '.
        let mut prev = ' ';
        let mut words = 0usize;
        while p < n {
            let c = doc.at(p);
            if matches!(c, '-' | '+') && doc.at(p + 1) == close && doc.at(p + 2) == '}' {
                self.sink.push(p, p + 3, K::Macro);
                return (p + 3, info);
            }
            if c == close && doc.at(p + 1) == '}' {
                self.sink.push(p, p + 2, K::Macro);
                return (p + 2, info);
            }
            if c.is_whitespace() {
                p += 1;
                continue;
            }
            if c == '"' || c == '\'' {
                let (end, content_end) = string_end(doc, p, c, close);
                self.sink.push(p, end, K::String);
                info.strings.push((doc.string(p + 1, content_end), p + 1, content_end, words <= 1));
                p = end.max(p + 1);
                prev = ' ';
                continue;
            }
            if is_ident_start(c) {
                let e = doc.scan_while(p, is_ident_char);
                let word = doc.string(p, e);
                let kind = if words == 0 && close == '%' {
                    K::Keyword
                } else if prev == '|' || prev == 'i' || prev == 'n' {
                    if prev == 'i' && word == "not" { K::Keyword } else { K::Function }
                } else if prev == '.' {
                    K::Field
                } else if BOOLEANS.contains(&word.as_str()) {
                    K::Boolean
                } else if KEYWORDS.contains(&word.as_str()) {
                    K::Keyword
                } else {
                    K::Ident
                };
                self.sink.push(p, e, kind);
                prev = match (prev, word.as_str()) {
                    ('i', "not") => 'n',
                    (_, "is") if kind == K::Keyword => 'i',
                    _ => ' ',
                };
                match words {
                    0 => info.first = Some((word, p)),
                    1 => info.second = Some((word, p)),
                    _ => {}
                }
                words += 1;
                p = e;
                continue;
            }
            if c.is_ascii_digit() {
                let e = doc.scan_while(p, |c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
                self.sink.push(p, e, K::Number);
                p = e;
                prev = ' ';
                continue;
            }
            let kind = match c {
                '(' | ')' | '[' | ']' | '{' | '}' => K::Bracket,
                ',' | ':' | '.' => K::Punctuation,
                '|' | '+' | '-' | '*' | '/' | '%' | '<' | '>' | '!' | '~' | '=' => K::Operator,
                _ => K::Plain,
            };
            if c == '=' && doc.at(p + 1) != '=' && !matches!(doc.at(p.wrapping_sub(1)), '=' | '!' | '<' | '>') {
                info.assign = true;
            }
            self.sink.push(p, p + 1, kind);
            prev = if matches!(c, '|' | '.') { c } else { ' ' };
            p += 1;
        }
        (n, info)
    }

    /// HTML markup at `i` (which holds `<`): its end, or `None` when `<` is just text.
    fn markup(&mut self, i: usize) -> Option<usize> {
        let doc = self.doc;
        if doc.starts_with_at(i, "<!--") {
            let end = doc.find_str(i + 4, "-->").map_or(doc.len(), |e| e + 3);
            self.sink.push(i, end, K::Comment);
            return Some(end);
        }
        if doc.at(i + 1) == '!' {
            let end = (i + 2..doc.len()).find(|&j| doc.at(j) == '>').map_or(doc.len(), |e| e + 1);
            self.sink.push(i, end, K::Attribute);
            return Some(end);
        }
        let close = doc.at(i + 1) == '/';
        let name_start = if close { i + 2 } else { i + 1 };
        if !doc.at(name_start).is_ascii_alphabetic() {
            return None;
        }
        Some(self.tag(i, name_start, close))
    }

    fn tag(&mut self, i: usize, name_start: usize, close: bool) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let name_end = doc.scan_while(name_start, |c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '_' | '.'));
        let name = doc.string(name_start, name_end).to_ascii_lowercase();
        self.sink.push(i, name_start, K::Punctuation);
        self.sink.push(name_start, name_end, K::Keyword);
        let mut p = name_end;
        let mut self_closing = false;
        let mut closed = false;
        while p < n {
            let c = doc.at(p);
            if c == '>' {
                self.sink.push(p, p + 1, K::Punctuation);
                p += 1;
                closed = true;
                break;
            }
            if c == '/' && doc.at(p + 1) == '>' {
                self.sink.push(p, p + 2, K::Punctuation);
                p += 2;
                self_closing = true;
                closed = true;
                break;
            }
            if is_template_start(doc, p) {
                p = self.template(p).max(p + 1);
                continue;
            }
            if c == '<' {
                // An unterminated tag: the `<` starts something new.
                break;
            }
            if c.is_whitespace() {
                p += 1;
                continue;
            }
            if c == '"' || c == '\'' {
                p = self.attr_string(p, c);
                continue;
            }
            if c == '=' {
                self.sink.push(p, p + 1, K::Operator);
                p += 1;
                continue;
            }
            let e = doc.scan_while(p, |c| !c.is_whitespace() && !matches!(c, '=' | '>' | '/' | '<' | '"' | '\'' | '{'));
            if e > p {
                // After `=` an unquoted value, else an attribute name.
                let before = (name_end..p).rev().find(|&j| !doc.at(j).is_whitespace());
                let kind = if before.is_some_and(|b| doc.at(b) == '=') { K::String } else { K::Key };
                self.sink.push(p, e, kind);
                p = e;
            } else {
                p += 1;
            }
        }
        self.elements.push(Element { name: name.clone(), close, self_closing, start: i });
        if closed && !close && !self_closing && (name == "script" || name == "style") {
            return self.raw_text(p, &name);
        }
        p
    }

    /// A quoted attribute value at `p`, with template constructs inside it.
    fn attr_string(&mut self, p: usize, quote: char) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let mut seg = p;
        let mut j = p + 1;
        while j < n {
            let c = doc.at(j);
            if c == quote {
                self.sink.push(seg, j + 1, K::String);
                return j + 1;
            }
            if is_template_start(doc, j) {
                self.sink.push(seg, j, K::String);
                j = self.template(j).max(j + 1);
                seg = j;
                continue;
            }
            j += 1;
        }
        self.sink.push(seg, n, K::String);
        n
    }

    /// The body of `<script>`/`<style>` from `p`: plain except template constructs, up to `</name`.
    fn raw_text(&mut self, mut p: usize, name: &str) -> usize {
        let doc = self.doc;
        let n = doc.len();
        let len = name.chars().count();
        while p < n {
            if doc.at(p) == '<' && doc.at(p + 1) == '/' {
                let word = doc.string(p + 2, p + 2 + len);
                if word.eq_ignore_ascii_case(name) {
                    return p;
                }
            }
            if is_template_start(doc, p) {
                p = self.template(p).max(p + 1);
                continue;
            }
            p += 1;
        }
        n
    }
}

/// End of an opening delimiter (`{{`, `{%`, plus `-`/`+`) at `i`.
fn open_end(doc: &Doc, i: usize) -> usize {
    if matches!(doc.at(i + 2), '-' | '+') { i + 3 } else { i + 2 }
}

/// A string at `p` inside template code: (end of the literal, end of its contents). An
/// unterminated string ends before the closing delimiter, or at the end of the text.
fn string_end(doc: &Doc, p: usize, quote: char, close: char) -> (usize, usize) {
    let n = doc.len();
    let mut j = p + 1;
    while j < n {
        let c = doc.at(j);
        if c == '\\' {
            j += 2;
            continue;
        }
        if c == quote {
            return (j + 1, j);
        }
        if c == close && doc.at(j + 1) == '}' {
            return (j, j);
        }
        j += 1;
    }
    (n, n)
}

/// The start of the first `{% word … %}` at or after `from`.
fn find_end_tag(doc: &Doc, from: usize, word: &str) -> Option<usize> {
    let mut at = from;
    while let Some(j) = doc.find_str(at, "{%") {
        let mut k = open_end(doc, j);
        k = doc.scan_while(k, char::is_whitespace);
        let e = doc.scan_while(k, is_ident_char);
        if doc.string(k, e) == word {
            return Some(j);
        }
        at = j + 2;
    }
    None
}

/// The end of the `{% … %}` tag starting at `start` (or the end of the text).
fn tag_end(doc: &Doc, start: usize) -> usize {
    doc.find_str(start + 2, "%}").map_or(doc.len(), |e| e + 2)
}

pub(crate) fn spans(doc: &Doc) -> Vec<Span> {
    lex(doc).spans
}

fn opens_block(t: &Tag) -> bool {
    BLOCK_TAGS.contains(&t.name.as_str()) && !(t.name == "set" && t.assign)
}

/// Pairs block tags: calls `open(tag)` for an opener and `close(opener, tag)` when an end tag
/// matches one on the stack.
fn pair_tags<'a>(tags: impl Iterator<Item = &'a Tag>, mut on_close: impl FnMut(&'a Tag, &'a Tag)) -> Vec<&'a Tag> {
    let mut stack: Vec<&Tag> = Vec::new();
    for t in tags {
        if opens_block(t) {
            stack.push(t);
        } else if let Some(base) = t.name.strip_prefix("end") {
            let back = stack.iter().rev().take(MAX_RECOVERY).position(|o| o.name == base);
            if let Some(back) = back {
                let at = stack.len().saturating_sub(back + 1);
                if let Some(open) = stack.get(at) {
                    on_close(open, t);
                }
                stack.truncate(at);
            }
        }
    }
    stack
}

pub(crate) fn folding_ranges(doc: &Doc) -> Vec<FoldRange> {
    let lx = lex(doc);
    let mut out = Vec::new();
    common::multiline_comment_folds(&doc.index, &lx.spans, &mut out);
    let line = |o: usize| doc.index.line_of(o);
    pair_tags(lx.tags.iter(), |open, close| {
        common::push_fold(&mut out, line(open.start), line(close.start), FoldKind::Block);
    });
    let mut stack: Vec<&Element> = Vec::new();
    for e in &lx.elements {
        if e.self_closing || VOID.contains(&e.name.as_str()) {
            continue;
        }
        if !e.close {
            stack.push(e);
            continue;
        }
        let back = stack.iter().rev().take(MAX_RECOVERY).position(|o| o.name == e.name);
        if let Some(back) = back {
            let at = stack.len().saturating_sub(back + 1);
            if let Some(open) = stack.get(at) {
                common::push_fold(&mut out, line(open.start), line(e.start), FoldKind::Block);
            }
            stack.truncate(at);
        }
    }
    common::finish_folds(out)
}

pub(crate) fn outline(doc: &Doc) -> Vec<OutlineItem> {
    let lx = lex(doc);
    let mut out = Vec::new();
    // Open blocks: whether each one is listed in the outline.
    let mut stack: Vec<(&str, bool)> = Vec::new();
    for t in &lx.tags {
        if opens_block(t) {
            let item = match (t.name.as_str(), &t.arg) {
                ("block", Some((name, at))) => Some((format!("block {name}"), *at, SymbolKind::Section)),
                ("macro", Some((name, at))) => Some((name.clone(), *at, SymbolKind::Function)),
                _ => None,
            };
            if let Some((name, at, kind)) = &item {
                let line = doc.index.line_of(*at);
                out.push(OutlineItem {
                    name: name.clone(),
                    kind: *kind,
                    line,
                    column: at.saturating_sub(doc.index.line_start(line)),
                    depth: stack.iter().filter(|(_, listed)| *listed).count(),
                });
            }
            stack.push((t.name.as_str(), item.is_some()));
        } else if let Some(base) = t.name.strip_prefix("end") {
            let back = stack.iter().rev().take(MAX_RECOVERY).position(|(n, _)| *n == base);
            if let Some(back) = back {
                stack.truncate(stack.len().saturating_sub(back + 1));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- public helpers

/// Template names in `{% extends %}`, `{% include %}` (every string before `with`/`only`/
/// `ignore`, so `{% include ["a.html", "b.html"] %}` gives both), `{% import %}`,
/// `{% from "x" import y %}` and `{% embed %}`, in document order.
pub fn template_references(text: &str) -> Vec<TemplateRef> {
    let doc = Doc::new(text);
    let lx = lex(&doc);
    let mut out = Vec::new();
    for t in &lx.tags {
        let strings: Vec<&(String, usize, usize, bool)> = match t.name.as_str() {
            "include" => t.strings.iter().filter(|s| s.3).collect(),
            "extends" | "import" | "from" | "embed" => t.strings.iter().take(1).filter(|s| s.3).collect(),
            _ => continue,
        };
        for (name, start, end, _) in strings {
            out.push(TemplateRef { tag: t.name.clone(), name: name.clone(), start: *start, end: *end });
        }
    }
    out
}

/// The template reference whose string (quotes included) contains `offset`.
pub fn template_reference_at(text: &str, offset: usize) -> Option<TemplateRef> {
    template_references(text).into_iter().find(|r| r.start.saturating_sub(1) <= offset && offset <= r.end + 1)
}

/// What can be completed at `offset`: a tag name right after `{%` (an end tag when the typed
/// word starts with `end`), or a filter name after `|` inside `{{ }}` or `{% %}`.
pub fn completion_context(text: &str, offset: usize) -> Option<TagCompletion> {
    let chars: Vec<char> = text.chars().collect();
    let offset = offset.min(chars.len());
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    let mut start = offset;
    while start > 0 && is_ident_char(at(start - 1)) && offset - start < 256 {
        start -= 1;
    }
    let prefix: String = chars.get(start..offset).unwrap_or(&[]).iter().collect();
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut j = start;
    while j > 0 && matches!(at(j - 1), ' ' | '\t') && start - j < 256 {
        j -= 1;
    }
    let k = if j > 0 && matches!(at(j - 1), '-' | '+') { j - 1 } else { j };
    if k >= 2 && at(k - 1) == '%' && at(k - 2) == '{' {
        let kind = if prefix.starts_with("end") { CompletionKind::EndTag } else { CompletionKind::Tag };
        return Some(TagCompletion { kind, start, prefix });
    }
    if j >= 1 && at(j - 1) == '|' && !(j >= 2 && at(j - 2) == '|') && inside_code(&chars, j - 1) {
        return Some(TagCompletion { kind: CompletionKind::Filter, start, prefix });
    }
    None
}

/// Whether `pos` is inside `{{ … }}` or `{% … %}` (looking back a bounded distance).
fn inside_code(chars: &[char], pos: usize) -> bool {
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    let stop = pos.saturating_sub(MAX_LOOKBACK).max(1);
    for i in (stop..=pos).rev() {
        let (a, b) = (at(i - 1), at(i));
        if (a == '}' || a == '%') && b == '}' {
            return false;
        }
        if a == '{' && (b == '{' || b == '%') {
            return true;
        }
    }
    false
}

/// Block tags opened before `offset` (tags ending at or before it) and not closed yet,
/// innermost last.
pub fn open_blocks(text: &str, offset: usize) -> Vec<String> {
    let doc = Doc::new(text);
    let lx = lex(&doc);
    pair_tags(lx.tags.iter().filter(|t| t.end <= offset), |_, _| {}).into_iter().map(|t| t.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{assert_kind, kind_at, offset_of};
    use crate::{FoldKind, FoldRange, Language, folding_ranges, highlight, matching_bracket, outline as outline_of};

    const L: Language = Language::DjangoTemplate;

    #[test]
    fn html_highlighting() {
        let text = "<!DOCTYPE html>\n<div class=\"a b\" id=x data-on>&amp; text &#38;</div>\n<!-- note -->\n<br/>";
        assert_kind(L, text, "<!DOCTYPE html>", K::Attribute);
        assert_kind(L, text, "div", K::Keyword);
        assert_kind(L, text, "class", K::Key);
        assert_kind(L, text, "\"a b\"", K::String);
        assert_kind(L, text, "x", K::String);
        assert_kind(L, text, "data-on", K::Key);
        assert_kind(L, text, "&amp;", K::Constant);
        assert_kind(L, text, "&#38;", K::Constant);
        assert_kind(L, text, "<!-- note -->", K::Comment);
        assert_kind(L, text, "</", K::Punctuation);
        assert_kind(L, text, "/>", K::Punctuation);
        assert_eq!(kind_at(L, text, 1, 37), K::Plain, "text stays plain");
    }

    #[test]
    fn template_highlighting() {
        let text = "{% extends \"base.html\" %}\n{{- user.name|lower|default('x') -}}\n{# c #}\n{% if a is not defined and b == 2 %}{% set x = True %}{% endif %}";
        assert_kind(L, text, "{%", K::Macro);
        assert_kind(L, text, "%}", K::Macro);
        assert_kind(L, text, "{{-", K::Macro);
        assert_kind(L, text, "-}}", K::Macro);
        assert_kind(L, text, "extends", K::Keyword);
        assert_kind(L, text, "\"base.html\"", K::String);
        assert_kind(L, text, "user", K::Ident);
        assert_kind(L, text, "name", K::Field);
        assert_kind(L, text, "lower", K::Function);
        assert_kind(L, text, "default", K::Function);
        assert_kind(L, text, "'x'", K::String);
        assert_kind(L, text, "{# c #}", K::Comment);
        assert_kind(L, text, "is", K::Keyword);
        assert_kind(L, text, "defined", K::Function);
        assert_kind(L, text, "and", K::Keyword);
        assert_kind(L, text, "2", K::Number);
        assert_kind(L, text, "==", K::Operator);
        assert_kind(L, text, "True", K::Boolean);
        assert_kind(L, text, "endif", K::Keyword);
    }

    #[test]
    fn templates_inside_tags_and_attribute_values() {
        let text = "<a href=\"/u/{{ user.id }}/\" {% if on %}class=\"on\"{% endif %}>x</a>";
        assert_kind(L, text, "\"/u/", K::String);
        assert_kind(L, text, "id", K::Field);
        assert_kind(L, text, "/\"", K::String);
        assert_kind(L, text, "if", K::Keyword);
        assert_kind(L, text, "class", K::Key);
        assert_kind(L, text, "\"on\"", K::String);
    }

    #[test]
    fn comment_raw_and_script_blocks() {
        let text = "{% comment %}\n{{ x }} <b>\n{% endcomment %}\n{% raw %}{{ y }}{% endraw %}<script>if (a<b) {{ z }}</script>";
        assert_eq!(kind_at(L, text, 0, 0), K::Comment);
        assert_eq!(kind_at(L, text, 1, 3), K::Comment);
        assert_eq!(kind_at(L, text, 2, 14), K::Comment);
        let raw_line = 3;
        assert_eq!(kind_at(L, text, raw_line, 10), K::Plain, "raw content is plain");
        assert_kind(L, text, "endraw", K::Keyword);
        assert_kind(L, text, "script", K::Keyword);
        assert_kind(L, text, "z", K::Ident);
        // `<b` inside the script is not a tag.
        assert!(!crate::test_util::spans(L, text).iter().any(|(t, k)| t == "b" && *k == K::Keyword));
    }

    #[test]
    fn unterminated_constructs_run_to_the_end() {
        let text = "{{ a\nb";
        assert_eq!(kind_at(L, text, 1, 0), K::Ident);
        let text = "{# open\nstill";
        assert_eq!(kind_at(L, text, 1, 2), K::Comment);
        let text = "{% if \"open %}\nnext";
        assert_kind(L, text, "%}", K::Macro);
        assert_eq!(kind_at(L, text, 1, 0), K::Plain);
        let text = "<div class=\"x\nmore";
        assert_eq!(kind_at(L, text, 1, 1), K::String);
    }

    #[test]
    fn folds_and_outline() {
        let text = "{% block content %}\n<div>\n  {% for x in xs %}\n    {{ x }}\n  {% endfor %}\n</div>\n{% endblock %}\n{% macro m(a) %}\n{{ a }}\n{% endmacro %}\n{# a\nb #}\n{% set y = 1 %}\n{% set z %}\nq\n{% endset %}";
        let folds = folding_ranges(L, text);
        let f = |s, e, kind| FoldRange { start_line: s, end_line: e, kind };
        for want in [
            f(0, 6, FoldKind::Block),
            f(1, 5, FoldKind::Block),
            f(2, 4, FoldKind::Block),
            f(7, 9, FoldKind::Block),
            f(10, 11, FoldKind::Comment),
            f(13, 15, FoldKind::Block),
        ] {
            assert!(folds.contains(&want), "missing {want:?} in {folds:?}");
        }
        assert!(!folds.iter().any(|x| x.start_line == 12), "set with = is not a block");
        let items = outline_of(L, text);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "block content");
        assert_eq!(items[0].kind, SymbolKind::Section);
        assert_eq!((items[0].line, items[0].column, items[0].depth), (0, 9, 0));
        assert_eq!(items[1].name, "m");
        assert_eq!(items[1].kind, SymbolKind::Function);
        let nested = outline_of(L, "{% block a %}{% block b %}{% endblock %}{% endblock %}{% block c %}");
        assert_eq!(
            nested.iter().map(|i| (i.name.as_str(), i.depth)).collect::<Vec<_>>(),
            vec![("block a", 0), ("block b", 1), ("block c", 0)]
        );
    }

    #[test]
    fn references() {
        let text = "{% extends \"base.html\" %}{% include 'a.html' with x=\"no\" %}{% include [\"b.html\", 'c.html'] ignore missing %}{% from \"m.html\" import f %}{% import 'n.html' as n %}{% extends var %}{{ \"s.html\" }}";
        let refs = template_references(text);
        let names: Vec<(&str, &str)> = refs.iter().map(|r| (r.tag.as_str(), r.name.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("extends", "base.html"),
                ("include", "a.html"),
                ("include", "b.html"),
                ("include", "c.html"),
                ("from", "m.html"),
                ("import", "n.html")
            ]
        );
        let base = offset_of(text, "base.html", 0);
        assert_eq!(refs[0].start, base);
        assert_eq!(refs[0].end, base + 9);
        assert_eq!(template_reference_at(text, base - 1).map(|r| r.name), Some("base.html".into()));
        assert_eq!(template_reference_at(text, base + 4).map(|r| r.name), Some("base.html".into()));
        assert_eq!(template_reference_at(text, base + 10).map(|r| r.name), Some("base.html".into()));
        assert_eq!(template_reference_at(text, 0), None);
        assert_eq!(template_reference_at(text, usize::MAX), None);
    }

    #[test]
    fn completion_contexts() {
        let c = |t: &str| completion_context(t, t.chars().count());
        assert_eq!(c("{% "), Some(TagCompletion { kind: CompletionKind::Tag, start: 3, prefix: String::new() }));
        assert_eq!(c("<p>{%- bl"), Some(TagCompletion { kind: CompletionKind::Tag, start: 7, prefix: "bl".into() }));
        assert_eq!(c("{%end"), Some(TagCompletion { kind: CompletionKind::EndTag, start: 2, prefix: "end".into() }));
        assert_eq!(c("{{ x|lo"), Some(TagCompletion { kind: CompletionKind::Filter, start: 5, prefix: "lo".into() }));
        assert_eq!(
            c("{% for a in b| "),
            Some(TagCompletion { kind: CompletionKind::Filter, start: 15, prefix: String::new() })
        );
        assert_eq!(c("{{ x }} a|b"), None);
        assert_eq!(c("{{ a || b"), None);
        assert_eq!(c("{% if x %} y"), None);
        assert_eq!(c("plain"), None);
        assert_eq!(c(""), None);
        assert_eq!(completion_context("{% ", usize::MAX).map(|c| c.kind), Some(CompletionKind::Tag));
    }

    #[test]
    fn open_block_stack() {
        let text = "{% block a %}{% for x in y %}{% if z %}{% endif %}<p>{% set q %}{% endset %}{% set r = 1 %}";
        assert_eq!(open_blocks(text, text.chars().count()), vec!["block", "for"]);
        assert_eq!(open_blocks(text, 13), vec!["block"]);
        assert_eq!(open_blocks(text, 0), Vec::<String>::new());
        assert_eq!(open_blocks("{% endfor %}{% block %}{% endblock %}", 999), Vec::<String>::new());
    }

    #[test]
    fn detection_and_language_metadata() {
        use std::path::Path;
        assert_eq!(Language::from_path(Path::new("app/templates/a/b.html")), L);
        assert_eq!(Language::from_path(Path::new("templates/x.htm")), L);
        assert_eq!(Language::from_path(Path::new("x.jinja2")), L);
        assert_eq!(Language::from_path(Path::new("conf.yml.j2")), L);
        assert_eq!(Language::from_path(Path::new("static/index.html")), Language::PlainText);
        assert_eq!(Language::from_path(Path::new("templates")), Language::PlainText);
        assert_eq!(L.name(), "Django Template");
        assert_eq!(L.lsp_id(), "django-html");
        assert_eq!(L.block_comment(), Some(("{#", "#}")));
    }

    #[test]
    fn hostile_input_never_panics() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let soup = [
            "{",
            "}",
            "%",
            "#",
            "{{",
            "}}",
            "{%",
            "%}",
            "{#",
            "#}",
            "-",
            "+",
            "<",
            ">",
            "/",
            "\"",
            "'",
            "|",
            ".",
            "=",
            "&",
            ";",
            "!",
            "--",
            "<!--",
            "-->",
            "endblock",
            "block ",
            "raw",
            "comment",
            "endraw",
            "\n",
            " ",
            "x",
            "😀",
            "é",
            "\\",
            "<script>",
            "</script>",
            "include",
            "set ",
            "\u{0}",
        ];
        for _ in 0..500 {
            let mut text = String::new();
            for _ in 0..(seed % 60) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                text.push_str(soup[(seed % soup.len() as u64) as usize]);
            }
            let n = text.chars().count();
            let _ = crate::test_util::checked(L, &text);
            let _ = folding_ranges(L, &text);
            let _ = outline_of(L, &text);
            for off in [0, n / 2, n, n + 1, usize::MAX] {
                let _ = matching_bracket(L, &text, off);
                let _ = completion_context(&text, off);
                let _ = template_reference_at(&text, off);
                let _ = open_blocks(&text, off);
            }
            let _ = template_references(&text);
        }
        assert_eq!(highlight(L, "").len_lines(), 1);
    }
}
