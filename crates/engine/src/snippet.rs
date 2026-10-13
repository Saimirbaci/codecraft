//! The LSP snippet grammar (`$1`, `${2:default}`, nested placeholders, `${3|a,b|}` choices, `$0`,
//! variables such as `$TM_SELECTED_TEXT` with defaults and regex transforms, `\` escapes), parsed
//! into the text to insert plus the tab stops a live template session walks through.
//!
//! Malformed constructs are inserted literally (like VS Code), so any input produces a result:
//! the parser never panics, nests at most [`MAX_DEPTH`] levels and stays near-linear on hostile
//! input (failed constructs are remembered by position).

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;

/// Deepest placeholder nesting honoured; deeper constructs are inserted as text.
pub const MAX_DEPTH: usize = 32;
/// Longest snippet parsed; longer sources are inserted as plain text.
pub const MAX_LEN: usize = 64 * 1024;

/// A parsed snippet: the text to insert and its tab stops.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// Tab stops in visiting order (`$1`, `$2`, …, then unknown variables); `$0` is [`Snippet::end`].
    pub stops: Vec<TabStop>,
    /// Char offset of the final caret (`$0`), when the snippet has one.
    pub end: Option<usize>,
}

/// One tab stop with every occurrence (mirrors) as char ranges in [`Snippet::text`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabStop {
    pub number: u32,
    pub ranges: Vec<Range<usize>>,
    /// `${1|a,b|}`: the values to choose from (the first is inserted).
    pub choices: Vec<String>,
}

/// Parse a snippet without variable values (unknown variables become placeholders).
pub fn parse(src: &str) -> Snippet {
    parse_with(src, &|_| None)
}

/// Parse a snippet, resolving variables (`TM_SELECTED_TEXT`, `TM_FILENAME`, …) with `vars`.
pub fn parse_with(src: &str, vars: &dyn Fn(&str) -> Option<String>) -> Snippet {
    if src.len() > MAX_LEN {
        return Snippet { text: src.to_string(), ..Snippet::default() };
    }
    let mut p = Parser {
        chars: src.chars().collect(),
        pos: 0,
        failed: HashSet::new(),
        eof_unclosed: false,
        choice_eof: None,
        slash_eof: None,
    };
    let nodes = p.parse_any(None, 0);
    render(&nodes, vars)
}

/// Prefix every line after the first with `indent` (LSP `InsertTextMode.adjustIndentation`).
pub fn adjust_indentation(src: &str, indent: &str) -> String {
    if indent.is_empty() || !src.contains('\n') {
        return src.to_string();
    }
    src.replace('\n', &format!("\n{indent}"))
}

// ------------------------------------------------------------------------------- syntax tree

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Text(String),
    /// `$1`, `${1}`, `${1:children}`, `${1|a,b|}`.
    Stop {
        number: u32,
        children: Vec<Node>,
        choices: Vec<String>,
        placeholder: bool,
    },
    Variable {
        name: String,
        default: Option<Vec<Node>>,
        transform: Option<Transform>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Transform {
    regex: String,
    format: Vec<FormatPart>,
    options: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FormatPart {
    Text(String),
    /// `$1`, `${1}`, `${1:/upcase}`, `${1:+if}`, `${1:-else}`, `${1:?if:else}`.
    Group {
        index: usize,
        case: Option<String>,
        if_set: Option<String>,
        if_unset: Option<String>,
    },
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    /// Start positions of `$` constructs that failed to parse (they do not depend on context).
    failed: HashSet<usize>,
    /// A placeholder ran to the end of input without its `}`: every enclosing one will too.
    eof_unclosed: bool,
    /// A choice (`|}`) / transform (`/`) scan from here reached the end: later ones will too.
    choice_eof: Option<usize>,
    slash_eof: Option<usize>,
}

fn is_var_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_var_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    /// `any*` until `stop` (unescaped) or the end (the caller checks for `stop`).
    fn parse_any(&mut self, stop: Option<char>, depth: usize) -> Vec<Node> {
        let mut out: Vec<Node> = Vec::new();
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if self.eof_unclosed && stop.is_some() {
                break;
            }
            if Some(c) == stop {
                break;
            }
            match c {
                '\\' => match self.at(self.pos + 1) {
                    Some(n @ ('$' | '}' | '\\')) => {
                        text.push(n);
                        self.pos += 2;
                    }
                    _ => {
                        text.push('\\');
                        self.pos += 1;
                    }
                },
                '$' => {
                    let start = self.pos;
                    let parsed = self.parse_dollar(depth);
                    if stop.is_none() {
                        // Back at the top level: nothing is open any more.
                        self.eof_unclosed = false;
                    }
                    match parsed {
                        Some(node) => {
                            if !text.is_empty() {
                                out.push(Node::Text(std::mem::take(&mut text)));
                            }
                            out.push(node);
                        }
                        None => {
                            self.pos = start + 1;
                            text.push('$');
                        }
                    }
                }
                _ => {
                    text.push(c);
                    self.pos += 1;
                }
            }
        }
        if !text.is_empty() {
            out.push(Node::Text(text));
        }
        out
    }

    fn digits(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start || self.pos - start > 9 {
            return None;
        }
        self.chars.get(start..self.pos)?.iter().collect::<String>().parse().ok()
    }

    fn var_name(&mut self) -> Option<String> {
        if !self.peek().is_some_and(is_var_start) {
            return None;
        }
        let start = self.pos;
        while self.peek().is_some_and(is_var_char) {
            self.pos += 1;
        }
        Some(self.chars.get(start..self.pos)?.iter().collect())
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// A `$…` construct at `self.pos`; `None` (position unspecified) when it is not one.
    fn parse_dollar(&mut self, depth: usize) -> Option<Node> {
        let start = self.pos;
        if self.failed.contains(&start) {
            return None;
        }
        let node = self.parse_dollar_inner(depth);
        if node.is_none() && depth < MAX_DEPTH {
            self.failed.insert(start);
        }
        node
    }

    fn parse_dollar_inner(&mut self, depth: usize) -> Option<Node> {
        self.pos += 1; // `$`
        match self.peek()? {
            c if c.is_ascii_digit() => {
                let number = self.digits()?;
                Some(Node::Stop { number, children: Vec::new(), choices: Vec::new(), placeholder: false })
            }
            c if is_var_start(c) => {
                let name = self.var_name()?;
                Some(Node::Variable { name, default: None, transform: None })
            }
            '{' if depth < MAX_DEPTH => {
                self.pos += 1;
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.parse_braced_stop(depth)
                } else {
                    self.parse_braced_variable(depth)
                }
            }
            _ => None,
        }
    }

    fn close(&mut self) -> Option<()> {
        if self.eat('}') {
            Some(())
        } else {
            if self.pos >= self.chars.len() {
                self.eof_unclosed = true;
            }
            None
        }
    }

    fn parse_braced_stop(&mut self, depth: usize) -> Option<Node> {
        let number = self.digits()?;
        match self.peek()? {
            '}' => {
                self.pos += 1;
                Some(Node::Stop { number, children: Vec::new(), choices: Vec::new(), placeholder: false })
            }
            ':' => {
                self.pos += 1;
                let children = self.parse_any(Some('}'), depth + 1);
                self.close()?;
                Some(Node::Stop { number, children, choices: Vec::new(), placeholder: true })
            }
            '|' => {
                self.pos += 1;
                let choices = self.parse_choices()?;
                Some(Node::Stop { number, children: Vec::new(), choices, placeholder: true })
            }
            _ => None,
        }
    }

    /// `a,b,c|}` (after the opening `|`).
    fn parse_choices(&mut self) -> Option<Vec<String>> {
        let start = self.pos;
        if self.choice_eof.is_some_and(|p| start >= p) {
            return None;
        }
        let mut out = Vec::new();
        let mut cur = String::new();
        loop {
            let Some(c) = self.peek() else {
                self.choice_eof = Some(start);
                return None;
            };
            match c {
                '\\' => match self.at(self.pos + 1) {
                    Some(n @ ('$' | '}' | '\\' | ',' | '|')) => {
                        cur.push(n);
                        self.pos += 2;
                    }
                    _ => {
                        cur.push('\\');
                        self.pos += 1;
                    }
                },
                ',' => {
                    out.push(std::mem::take(&mut cur));
                    self.pos += 1;
                }
                '|' => {
                    self.pos += 1;
                    if !self.eat('}') {
                        return None;
                    }
                    out.push(cur);
                    return Some(out);
                }
                _ => {
                    cur.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn parse_braced_variable(&mut self, depth: usize) -> Option<Node> {
        let name = self.var_name()?;
        match self.peek()? {
            '}' => {
                self.pos += 1;
                Some(Node::Variable { name, default: None, transform: None })
            }
            ':' => {
                self.pos += 1;
                let children = self.parse_any(Some('}'), depth + 1);
                self.close()?;
                Some(Node::Variable { name, default: Some(children), transform: None })
            }
            '/' => {
                self.pos += 1;
                let regex = self.until_slash(false)?;
                let format_src = self.until_slash(true)?;
                let mut options = String::new();
                while let Some(c) = self.peek() {
                    if c == '}' {
                        break;
                    }
                    if !c.is_ascii_alphabetic() || options.len() > 8 {
                        return None;
                    }
                    options.push(c);
                    self.pos += 1;
                }
                self.close()?;
                let format = parse_format(&format_src);
                Some(Node::Variable { name, default: None, transform: Some(Transform { regex, format, options }) })
            }
            _ => None,
        }
    }

    /// Raw text up to an unescaped `/` (consumed); `\/` becomes `/`, other escapes stay. With
    /// `groups`, `${…}` format groups are copied whole (`${1:/upcase}` holds a `/`).
    fn until_slash(&mut self, groups: bool) -> Option<String> {
        let start = self.pos;
        if self.slash_eof.is_some_and(|p| start >= p) {
            return None;
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                self.slash_eof = Some(start);
                return None;
            };
            self.pos += 1;
            match c {
                '$' if groups && self.peek() == Some('{') => {
                    out.push('$');
                    while let Some(g) = self.peek() {
                        out.push(g);
                        self.pos += 1;
                        if g == '}' {
                            break;
                        }
                    }
                }
                '/' => return Some(out),
                '\\' => match self.peek() {
                    Some('/') => {
                        out.push('/');
                        self.pos += 1;
                    }
                    Some(n) => {
                        out.push('\\');
                        out.push(n);
                        self.pos += 1;
                    }
                    None => out.push('\\'),
                },
                _ => out.push(c),
            }
        }
    }
}

/// The format part of a transform: `$1`, `${1:/upcase}`, `${1:+yes}`, `${1:?yes:no}`, text.
fn parse_format(src: &str) -> Vec<FormatPart> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut text = String::new();
    let mut i = 0usize;
    let flush = |text: &mut String, out: &mut Vec<FormatPart>| {
        if !text.is_empty() {
            out.push(FormatPart::Text(std::mem::take(text)));
        }
    };
    while let Some(&c) = chars.get(i) {
        if c == '\\' {
            match chars.get(i + 1) {
                Some(&n) => {
                    // `\n`/`\t` are common in transforms; other escapes give the char itself.
                    text.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                    i += 2;
                }
                None => {
                    text.push('\\');
                    i += 1;
                }
            }
            continue;
        }
        if c == '$' {
            // `$12`
            let mut j = i + 1;
            while chars.get(j).is_some_and(char::is_ascii_digit) && j - i <= 3 {
                j += 1;
            }
            if j > i + 1 {
                let index = chars.get(i + 1..j).map(|d| d.iter().collect::<String>()).and_then(|d| d.parse().ok());
                if let Some(index) = index {
                    flush(&mut text, &mut out);
                    out.push(FormatPart::Group { index, case: None, if_set: None, if_unset: None });
                    i = j;
                    continue;
                }
            }
            if chars.get(i + 1) == Some(&'{')
                && let Some((part, next)) = parse_format_group(&chars, i + 2)
            {
                flush(&mut text, &mut out);
                out.push(part);
                i = next;
                continue;
            }
        }
        text.push(c);
        i += 1;
    }
    flush(&mut text, &mut out);
    out
}

/// `1}`, `1:/upcase}`, `1:+x}`, `1:-x}`, `1:x}`, `1:?a:b}` starting at `i`; returns the end.
fn parse_format_group(chars: &[char], mut i: usize) -> Option<(FormatPart, usize)> {
    let start = i;
    while chars.get(i).is_some_and(char::is_ascii_digit) && i - start <= 3 {
        i += 1;
    }
    let index: usize = chars.get(start..i)?.iter().collect::<String>().parse().ok()?;
    let close = chars.get(i..)?.iter().position(|c| *c == '}')? + i;
    let body: String = chars.get(i..close)?.iter().collect();
    let mut part = FormatPart::Group { index, case: None, if_set: None, if_unset: None };
    if let FormatPart::Group { case, if_set, if_unset, .. } = &mut part {
        if let Some(rest) = body.strip_prefix(":/") {
            *case = Some(rest.to_string());
        } else if let Some(rest) = body.strip_prefix(":+") {
            *if_set = Some(rest.to_string());
        } else if let Some(rest) = body.strip_prefix(":?") {
            let (a, b) = rest.split_once(':').unwrap_or((rest, ""));
            *if_set = Some(a.to_string());
            *if_unset = Some(b.to_string());
        } else if let Some(rest) = body.strip_prefix(":-") {
            *if_unset = Some(rest.to_string());
        } else if let Some(rest) = body.strip_prefix(':') {
            *if_unset = Some(rest.to_string());
        } else if !body.is_empty() {
            return None;
        }
    }
    Some((part, close + 1))
}

// ------------------------------------------------------------------------------- rendering

struct Out {
    text: String,
    len: usize,
    stops: BTreeMap<(u8, u32), TabStop>,
    end: Option<usize>,
    /// Unknown variable names → their stop number (visited after the numbered stops).
    unknown: Vec<String>,
}

impl Out {
    fn push(&mut self, s: &str) {
        self.text.push_str(s);
        self.len += s.chars().count();
    }

    fn stop(&mut self, key: (u8, u32), range: Range<usize>, choices: &[String]) {
        let entry = self.stops.entry(key).or_insert_with(|| TabStop { number: key.1, ..TabStop::default() });
        entry.ranges.push(range);
        if entry.choices.is_empty() && !choices.is_empty() {
            entry.choices = choices.to_vec();
        }
    }
}

fn render(nodes: &[Node], vars: &dyn Fn(&str) -> Option<String>) -> Snippet {
    // A mirror (`$1` without a default) shows the default of the first placeholder with one.
    let mut defaults: BTreeMap<u32, String> = BTreeMap::new();
    collect_defaults(nodes, vars, &mut defaults, 0);
    let mut out = Out { text: String::new(), len: 0, stops: BTreeMap::new(), end: None, unknown: Vec::new() };
    emit(nodes, vars, &defaults, &mut out, 0);
    let base = out.stops.keys().filter(|(k, _)| *k == 0).map(|(_, n)| *n).max().unwrap_or(0);
    let mut stops: Vec<TabStop> = out
        .stops
        .into_iter()
        .map(|((kind, n), mut t)| {
            if kind == 1 {
                // Unknown variables come after the numbered stops.
                t.number = base.saturating_add(1).saturating_add(n);
            }
            t
        })
        .collect();
    for s in &mut stops {
        s.ranges.sort_by_key(|r| (r.start, r.end));
        s.ranges.dedup();
    }
    Snippet { text: out.text, stops, end: out.end }
}

fn collect_defaults(
    nodes: &[Node],
    vars: &dyn Fn(&str) -> Option<String>,
    out: &mut BTreeMap<u32, String>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for n in nodes {
        match n {
            Node::Stop { number, children, choices, placeholder: true } => {
                if !out.contains_key(number) {
                    let text = match choices.first() {
                        Some(c) => c.clone(),
                        None => plain(children, vars, &BTreeMap::new(), depth + 1),
                    };
                    out.insert(*number, text);
                }
                collect_defaults(children, vars, out, depth + 1);
            }
            Node::Variable { default: Some(children), .. } => collect_defaults(children, vars, out, depth + 1),
            _ => {}
        }
    }
}

/// The text a node list renders to (no stop bookkeeping).
fn plain(
    nodes: &[Node],
    vars: &dyn Fn(&str) -> Option<String>,
    defaults: &BTreeMap<u32, String>,
    depth: usize,
) -> String {
    let mut out = Out { text: String::new(), len: 0, stops: BTreeMap::new(), end: None, unknown: Vec::new() };
    emit(nodes, vars, defaults, &mut out, depth);
    out.text
}

fn emit(
    nodes: &[Node],
    vars: &dyn Fn(&str) -> Option<String>,
    defaults: &BTreeMap<u32, String>,
    out: &mut Out,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for n in nodes {
        match n {
            Node::Text(t) => out.push(t),
            Node::Stop { number, children, choices, placeholder } => {
                let start = out.len;
                if let Some(first) = choices.first() {
                    out.push(first);
                } else if *placeholder {
                    emit(children, vars, defaults, out, depth + 1);
                } else if let Some(d) = defaults.get(number) {
                    out.push(d);
                }
                let range = start..out.len;
                if *number == 0 {
                    out.end.get_or_insert(start);
                } else {
                    out.stop((0, *number), range, choices);
                }
            }
            Node::Variable { name, default, transform } => match vars(name) {
                Some(value) => {
                    let value = match transform {
                        Some(t) => apply_transform(&value, t),
                        None => value,
                    };
                    match default {
                        Some(children) if value.is_empty() => emit(children, vars, defaults, out, depth + 1),
                        _ => out.push(&value),
                    }
                }
                None => match default {
                    Some(children) => emit(children, vars, defaults, out, depth + 1),
                    None => {
                        // Unknown variable: its name becomes a placeholder (LSP spec).
                        let index = match out.unknown.iter().position(|u| u == name) {
                            Some(i) => i,
                            None => {
                                out.unknown.push(name.clone());
                                out.unknown.len() - 1
                            }
                        };
                        let start = out.len;
                        out.push(name);
                        out.stop((1, u32::try_from(index).unwrap_or(u32::MAX)), start..out.len, &[]);
                    }
                },
            },
        }
    }
}

/// Apply `${VAR/regex/format/options}`; an invalid regex leaves the value unchanged.
fn apply_transform(value: &str, t: &Transform) -> String {
    let mut builder = regex::RegexBuilder::new(&t.regex);
    builder
        .case_insensitive(t.options.contains('i'))
        .multi_line(t.options.contains('m'))
        .dot_matches_new_line(t.options.contains('s'))
        .size_limit(1 << 20);
    let Ok(re) = builder.build() else { return value.to_string() };
    let global = t.options.contains('g');
    let mut out = String::new();
    let mut last = 0usize;
    for caps in re.captures_iter(value) {
        let Some(m) = caps.get(0) else { continue };
        out.push_str(value.get(last..m.start()).unwrap_or(""));
        for part in &t.format {
            match part {
                FormatPart::Text(s) => out.push_str(s),
                FormatPart::Group { index, case, if_set, if_unset } => {
                    let g = caps.get(*index).map(|m| m.as_str()).filter(|s| !s.is_empty());
                    match (g, if_set, if_unset) {
                        (Some(_), Some(yes), _) => out.push_str(yes),
                        (Some(v), None, _) => out.push_str(&apply_case(v, case.as_deref())),
                        (None, _, Some(no)) => out.push_str(no),
                        (None, _, None) => {}
                    }
                }
            }
        }
        last = m.end();
        if !global {
            break;
        }
    }
    out.push_str(value.get(last..).unwrap_or(""));
    out
}

fn apply_case(v: &str, case: Option<&str>) -> String {
    match case {
        Some("upcase") => v.to_uppercase(),
        Some("downcase") => v.to_lowercase(),
        Some("capitalize") => {
            let mut c = v.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
        Some(kind @ ("camelcase" | "pascalcase")) => {
            let words: Vec<&str> = v.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
            let mut s = String::new();
            for (i, w) in words.iter().enumerate() {
                let mut c = w.chars();
                if let Some(f) = c.next() {
                    if i == 0 && kind == "camelcase" {
                        s.extend(f.to_lowercase());
                    } else {
                        s.extend(f.to_uppercase());
                    }
                    s.push_str(c.as_str());
                }
            }
            s
        }
        _ => v.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn stops(s: &Snippet) -> Vec<(u32, Vec<Range<usize>>)> {
        s.stops.iter().map(|t| (t.number, t.ranges.clone())).collect()
    }

    #[test]
    fn tabstops_placeholders_and_final_caret() {
        let s = parse("foo(${1:a}, ${2:b})$0");
        assert_eq!(s.text, "foo(a, b)");
        assert_eq!(stops(&s), vec![(1, vec![4..5]), (2, vec![7..8])]);
        assert_eq!(s.end, Some(9));
        let s = parse("name($0)");
        assert_eq!((s.text.as_str(), s.end, s.stops.len()), ("name()", Some(5), 0));
        let s = parse("$2 then $1 ${3}");
        assert_eq!(s.text, " then  ");
        assert_eq!(stops(&s), vec![(1, vec![6..6]), (2, vec![0..0]), (3, vec![7..7])]);
    }

    #[test]
    fn mirrors_take_the_placeholder_default() {
        let s = parse("let ${1:x} = 1; $1 + $1");
        assert_eq!(s.text, "let x = 1; x + x");
        assert_eq!(stops(&s), vec![(1, vec![4..5, 11..12, 15..16])]);
        // The mirror may come first.
        let s = parse("$1 = ${1:v}");
        assert_eq!(s.text, "v = v");
    }

    #[test]
    fn nested_placeholders() {
        let s = parse("${1:foo(${2:bar})}$0");
        assert_eq!(s.text, "foo(bar)");
        assert_eq!(stops(&s), vec![(1, vec![0..8]), (2, vec![4..7])]);
        assert_eq!(s.end, Some(8));
        let s = parse("match ${1:expr} {\n    ${2:_} => ${3:todo!()},\n}$0");
        assert_eq!(s.text, "match expr {\n    _ => todo!(),\n}");
        assert_eq!(s.stops.len(), 3);
    }

    #[test]
    fn choices() {
        let s = parse("${1|one,two,three|} ${2|a\\,b,c\\|d|}");
        assert_eq!(s.text, "one a,b");
        assert_eq!(s.stops[0].choices, vec!["one", "two", "three"]);
        assert_eq!(s.stops[1].choices, vec!["a,b", "c|d"]);
        assert_eq!(s.stops[1].ranges, vec![4..7]);
        // An unterminated choice is literal text.
        assert_eq!(parse("${1|a,b").text, "${1|a,b");
    }

    #[test]
    fn escapes() {
        assert_eq!(parse(r"\$1 \} \\ \x").text, r"$1 } \ \x");
        assert!(parse(r"\$1").stops.is_empty());
        assert_eq!(parse(r"${1:a\}b}").text, "a}b");
        // A lone `}` and `$` are text.
        assert_eq!(parse("} $ $} ${").text, "} $ $} ${");
        assert_eq!(parse("a$").text, "a$");
    }

    #[test]
    fn variables() {
        let vars = |n: &str| match n {
            "TM_FILENAME" => Some("main.rs".to_string()),
            "TM_SELECTED_TEXT" => Some(String::new()),
            _ => None,
        };
        assert_eq!(parse_with("$TM_FILENAME ${TM_FILENAME}", &vars).text, "main.rs main.rs");
        // Empty value → default; unknown with default → default.
        assert_eq!(parse_with("${TM_SELECTED_TEXT:none} ${NOPE:x}", &vars).text, "none x");
        // Unknown without default: the name, as a placeholder after the numbered stops.
        let s = parse_with("$1 $FOO $FOO", &vars);
        assert_eq!(s.text, " FOO FOO");
        assert_eq!(stops(&s), vec![(1, vec![0..0]), (2, vec![1..4, 5..8])]);
        // Transforms.
        assert_eq!(parse_with(r"${TM_FILENAME/(.*)\.rs/${1:/upcase}/}", &vars).text, "MAIN");
        assert_eq!(parse_with("${TM_FILENAME/[a]/X/g}", &vars).text, "mXin.rs");
        assert_eq!(parse_with("${TM_FILENAME/(z)?.*/${1:?yes:no}/}", &vars).text, "no");
        assert_eq!(parse_with("${TM_FILENAME/(m)/${1:+M}$1/}", &vars).text, "Mmain.rs");
        // An invalid regex keeps the value.
        assert_eq!(parse_with("${TM_FILENAME/(/x/}", &vars).text, "main.rs");
        assert_eq!(apply_case("hello_big world", Some("pascalcase")), "HelloBigWorld");
        assert_eq!(apply_case("Hello world", Some("camelcase")), "helloWorld");
        assert_eq!(apply_case("élan", Some("capitalize")), "Élan");
    }

    #[test]
    fn final_stop_with_placeholder_and_unicode_offsets() {
        let s = parse("é${1:ü}😀${0:end}");
        assert_eq!(s.text, "éü😀end");
        assert_eq!(stops(&s), vec![(1, vec![1..2])]);
        assert_eq!(s.end, Some(3));
    }

    #[test]
    fn indentation_is_adjusted_per_line() {
        assert_eq!(adjust_indentation("fn a() {\n    $0\n}", "    "), "fn a() {\n        $0\n    }");
        assert_eq!(adjust_indentation("x", "\t"), "x");
    }

    #[test]
    fn hostile_input_is_bounded_and_never_panics() {
        let deep = "${1:".repeat(5000);
        let t = std::time::Instant::now();
        let s = parse(&deep);
        assert_eq!(s.text, deep);
        let nested = format!("{}x{}", "${1:".repeat(100), "}".repeat(100));
        let s = parse(&nested);
        assert!(s.text.contains('x'));
        for src in [
            "${",
            "${1",
            "${1:",
            "${1|",
            "${1|a",
            "${a/",
            "${a/b",
            "${a/b/c",
            "${a/b/c/",
            "$99999999999",
            "${9999999999}",
        ] {
            let _ = parse(src);
        }
        let mixed = "${1|a,${2:b}|} ${x/(/$1/} \\".repeat(2000);
        let _ = parse(&mixed);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        // Pseudo-random inputs over the grammar's alphabet.
        let alphabet: Vec<char> = "${}:|,/\\01aZ_ é\n".chars().collect();
        let mut seed = 0x2545F4914F6CDD1Du64;
        for _ in 0..3000 {
            let len = (seed % 40) as usize;
            let mut src = String::new();
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                src.push(alphabet[(seed % alphabet.len() as u64) as usize]);
            }
            let s = parse_with(&src, &|n| (n == "a").then(|| "v".to_string()));
            let n = s.text.chars().count();
            for st in &s.stops {
                assert!(st.ranges.iter().all(|r| r.start <= r.end && r.end <= n), "{src:?} → {s:?}");
            }
            assert!(s.end.is_none_or(|e| e <= n), "{src:?}");
        }
        let long = "a".repeat(MAX_LEN + 1);
        assert_eq!(parse(&long).text.len(), MAX_LEN + 1);
    }
}
