//! Code › Analyze Code › Data Flow to Here… / Data Flow from Here…: a tree of where the value of
//! the variable at the caret comes from (declarations, assignments, the arguments passed for a
//! parameter at every call site) or where it goes (variables initialized or assigned from it,
//! parameters of the functions it is passed to, returns, struct fields).
//!
//! The analysis is lexical (the same token stream as Search Structurally) and approximate: it
//! follows names within a function and calls by name across the project's files of the same
//! language (`fn` for Rust and Zig, `function` for JavaScript/TypeScript, `def` for Python). It
//! runs on a worker thread; depth and size are capped.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_text::Document;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, ok, spec};
use crate::params::{bool_or, opt_path, opt_usize};
use crate::ssr::{self, TokKind, Token};
use crate::{CommandError, Session};

/// Levels followed below the root.
pub const MAX_DEPTH: usize = 5;
/// Nodes per tree.
const MAX_NODES: usize = 400;
/// Project files considered for calls.
const MAX_FILES: usize = 3000;
const MAX_FILE_CHARS: usize = 1 << 20;
const MAX_LABEL: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Where the value comes from.
    ToHere,
    /// Where the value goes.
    FromHere,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlowKind {
    Variable,
    Declaration,
    Parameter,
    Assignment,
    CallArgument,
    Return,
    Field,
    Use,
    /// Already shown above (a cycle).
    Recursive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowNode {
    pub kind: FlowKind,
    pub label: String,
    pub path: PathBuf,
    /// Char offset, zero-based line and char column of the node's code.
    pub offset: usize,
    pub line: usize,
    pub column: usize,
    pub children: Vec<FlowNode>,
}

impl FlowNode {
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(FlowNode::count).sum::<usize>()
    }
}

/// A file of the analysis.
pub struct FileText {
    pub path: PathBuf,
    pub language: Language,
    pub text: String,
}

struct Parsed {
    toks: Vec<Token>,
    partner: Vec<Option<usize>>,
    /// The enclosing open bracket of each token.
    parent: Vec<Option<usize>>,
    doc: Document,
    funcs: Vec<Func>,
    language: Language,
}

#[derive(Clone, Debug)]
struct Func {
    name: String,
    /// Token of the `fn`/`function`/`def` keyword.
    start: usize,
    params: (usize, usize),
    /// Body token range (exclusive end).
    body: (usize, usize),
    /// Parameter names and their token indexes (`self` included).
    param_names: Vec<(String, usize)>,
}

fn parse(f: &FileText) -> Parsed {
    let toks = ssr::tokenize(f.language, &f.text);
    let partner = ssr::partners(&toks);
    let mut parent = vec![None; toks.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind == TokKind::Close
            && let Some(o) = partner.get(i).copied().flatten()
        {
            while let Some(top) = stack.pop() {
                if top == o {
                    break;
                }
            }
        }
        if let Some(slot) = parent.get_mut(i) {
            *slot = stack.last().copied();
        }
        if t.kind == TokKind::Open && partner.get(i).copied().flatten().is_some() {
            stack.push(i);
        }
    }
    let doc = Document::from_text(&f.text);
    let mut p = Parsed { toks, partner, parent, doc, funcs: Vec::new(), language: f.language };
    p.funcs = functions(&p);
    p
}

fn keyword_for(l: Language) -> &'static str {
    match l {
        Language::JavaScript | Language::TypeScript => "function",
        Language::Python => "def",
        _ => "fn",
    }
}

fn functions(p: &Parsed) -> Vec<Func> {
    let kw = keyword_for(p.language);
    let mut out = Vec::new();
    for (i, t) in p.toks.iter().enumerate() {
        if !(matches!(t.kind, TokKind::Keyword | TokKind::Ident) && t.text == kw) {
            continue;
        }
        let Some(name) = p.toks.get(i + 1).filter(|n| n.kind == TokKind::Ident) else { continue };
        // Skip generics up to the parameter list.
        let mut j = i + 2;
        while let Some(x) = p.toks.get(j) {
            if x.kind == TokKind::Open && x.text == "(" {
                break;
            }
            if x.text == "{" || x.text == ";" || j > i + 64 {
                j = usize::MAX;
                break;
            }
            j += 1;
        }
        let Some(close) = p.partner.get(j).copied().flatten() else { continue };
        let body = if p.language == Language::Python {
            python_body(p, i, close)
        } else {
            let mut k = close + 1;
            let mut found = None;
            while let Some(x) = p.toks.get(k) {
                if x.text == ";" && x.kind == TokKind::Punct {
                    break;
                }
                if x.kind == TokKind::Open && x.text == "{" {
                    found = p.partner.get(k).copied().flatten().map(|c| (k, c + 1));
                    break;
                }
                if k > close + 256 {
                    break;
                }
                k += 1;
            }
            found
        };
        let Some(body) = body else { continue };
        out.push(Func { name: name.text.clone(), start: i, params: (j, close), body, param_names: params(p, j, close) });
    }
    out
}

/// A Python function body: the tokens after `):` on lines indented deeper than the `def`.
fn python_body(p: &Parsed, def: usize, close: usize) -> Option<(usize, usize)> {
    let def_tok = p.toks.get(def)?;
    let def_line = p.doc.char_to_line(def_tok.start);
    let indent = p.doc.indent_of_line(def_line).chars().count();
    let colon = (close + 1..p.toks.len().min(close + 64)).find(|k| p.toks.get(*k).is_some_and(|t| t.punct_text(":")))?;
    let start = colon + 1;
    let mut end = start;
    while let Some(t) = p.toks.get(end) {
        let line = p.doc.char_to_line(t.start);
        if line > def_line && p.doc.indent_of_line(line).chars().count() <= indent && line_starts_with(p, end) {
            break;
        }
        end += 1;
    }
    (end > start).then_some((start, end))
}

fn line_starts_with(p: &Parsed, i: usize) -> bool {
    match (i.checked_sub(1).and_then(|k| p.toks.get(k)), p.toks.get(i)) {
        (Some(prev), Some(t)) => p.doc.char_to_line(prev.start) != p.doc.char_to_line(t.start),
        _ => true,
    }
}

trait TokExt {
    fn punct_text(&self, s: &str) -> bool;
}

impl TokExt for Token {
    fn punct_text(&self, s: &str) -> bool {
        self.kind == TokKind::Punct && self.text == s
    }
}

/// Parameter names (first identifier of each comma-separated parameter, skipping `mut`, `&`, …).
fn params(p: &Parsed, open: usize, close: usize) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut seg_start = open + 1;
    let mut i = open + 1;
    while i <= close {
        let at_end = i == close;
        let is_comma = p.toks.get(i).is_some_and(|t| t.punct_text(",")) && p.parent.get(i).copied().flatten() == Some(open);
        if at_end || is_comma {
            if let Some((name, at)) = (seg_start..i).find_map(|k| {
                let t = p.toks.get(k)?;
                let word = matches!(t.kind, TokKind::Ident | TokKind::Keyword);
                (word && !matches!(t.text.as_str(), "mut" | "ref" | "const" | "readonly" | "public" | "private")).then(|| (t.text.clone(), k))
            }) {
                out.push((name, at));
            }
            seg_start = i + 1;
        }
        i += 1;
    }
    out
}

/// The innermost function whose keyword..body contains token `i`.
fn func_at(p: &Parsed, i: usize) -> Option<&Func> {
    p.funcs.iter().filter(|f| f.start <= i && i < f.body.1).min_by_key(|f| f.body.1 - f.start)
}

fn is_assign_at(p: &Parsed, j: usize) -> Option<usize> {
    // `=`, or a compound `+=` (two joined tokens); returns the index of the `=`.
    let t = p.toks.get(j)?;
    let eq = if t.punct_text("=") {
        j
    } else if t.kind == TokKind::Punct
        && matches!(t.text.as_str(), "+" | "-" | "*" | "/" | "%" | "|" | "&" | "^")
        && p.toks.get(j + 1).is_some_and(|n| n.punct_text("=") && n.joined)
    {
        j + 1
    } else {
        return None;
    };
    let next = p.toks.get(eq + 1).filter(|n| n.joined).map(|n| n.text.as_str());
    let prev = if p.toks.get(eq).is_some_and(|x| x.joined) && eq == j {
        j.checked_sub(1).and_then(|k| p.toks.get(k)).map(|x| x.text.as_str())
    } else {
        None
    };
    (!matches!(next, Some("=" | ">")) && !matches!(prev, Some("=" | "!" | "<" | ">"))).then_some(eq)
}

/// Statement token range around `i`: from after the previous `;`/`}` at the same nesting level to
/// the next `;` (exclusive) or the end of the enclosing group.
fn statement(p: &Parsed, i: usize) -> (usize, usize) {
    let parent = p.parent.get(i).copied().flatten();
    let lo = parent.map_or(0, |o| o + 1);
    let hi = parent.and_then(|o| p.partner.get(o).copied().flatten()).unwrap_or(p.toks.len());
    let python = p.language == Language::Python;
    let mut start = lo;
    let mut j = i;
    while j > lo {
        j -= 1;
        if p.parent.get(j).copied().flatten() != parent {
            continue;
        }
        let Some(t) = p.toks.get(j) else { break };
        if t.punct_text(";") || t.kind == TokKind::Close && t.text == "}" || t.kind == TokKind::Open && t.text == "{" {
            start = j + 1;
            break;
        }
        if python && line_starts_with(p, j) {
            start = j;
            break;
        }
    }
    let mut end = hi;
    let mut k = i + 1;
    while k < hi {
        if p.parent.get(k).copied().flatten() == parent
            && let Some(t) = p.toks.get(k)
            && (t.punct_text(";") || t.kind == TokKind::Close && t.text == "}" && k > i || python && line_starts_with(p, k))
        {
            end = k;
            break;
        }
        k += 1;
    }
    (start, end.max(start))
}

fn text_of(p: &Parsed, f: &FileText, a: usize, b: usize) -> String {
    if a >= b {
        return String::new();
    }
    let (Some(s), Some(e)) = (p.toks.get(a), p.toks.get(b - 1)) else { return String::new() };
    let raw: String = f.text.chars().skip(s.start).take(e.end.saturating_sub(s.start)).collect();
    let one: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > MAX_LABEL { format!("{}…", one.chars().take(MAX_LABEL).collect::<String>()) } else { one }
}

/// The single variable an expression is (`x`, `&x`, `*x`, `x.clone()`, `mut x`), if it is one.
fn single_var(p: &Parsed, a: usize, b: usize) -> Option<usize> {
    let toks = p.toks.get(a..b)?;
    let words: Vec<usize> = (a..b).filter(|k| p.toks.get(*k).is_some_and(|t| t.kind == TokKind::Ident)).collect();
    let first = *words.first()?;
    let rest_ok = toks.iter().enumerate().all(|(n, t)| {
        let k = a + n;
        k == first
            || matches!(t.text.as_str(), "&" | "*" | "mut" | "." | "(" | ")" | "clone" | "to_owned" | "to_string" | "into" | "copied" | "cloned")
    });
    rest_ok.then_some(first)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Param,
    Decl { init: Option<(usize, usize)> },
    Write { rhs: (usize, usize) },
    Read,
}

fn role(p: &Parsed, func: &Func, i: usize) -> Role {
    if func.params.0 < i && i < func.params.1 {
        return Role::Param;
    }
    let (s, e) = statement(p, i);
    let first = p.toks.get(s).map(|t| t.text.as_str()).unwrap_or("");
    let decl_kw = matches!(first, "let" | "var" | "const" | "for") || p.language == Language::Python && first == "for";
    // The first top-level `=` of the statement.
    let parent = p.parent.get(i).copied().flatten();
    let eq = (s..e).find(|k| p.parent.get(*k).copied().flatten() == parent && is_assign_at(p, *k) == Some(*k));
    if decl_kw && eq.is_none_or(|q| i < q) {
        if first == "for" {
            let in_at = (s..e).find(|k| p.toks.get(*k).is_some_and(|t| t.text == "in" || t.text == "of"));
            if in_at.is_some_and(|x| i < x) {
                let iter_end = (in_at.unwrap_or(s) + 1..e)
                    .find(|k| p.toks.get(*k).is_some_and(|t| t.kind == TokKind::Open && t.text == "{" || t.punct_text(":")))
                    .unwrap_or(e);
                return Role::Decl { init: in_at.map(|x| (x + 1, iter_end)) };
            }
            return Role::Read;
        }
        return Role::Decl { init: eq.map(|q| (q + 1, e)) };
    }
    // `x = …` / `x += …`: the variable directly before the operator.
    if let Some(q) = is_assign_at(p, i + 1) {
        return Role::Write { rhs: (q + 1, e) };
    }
    if p.language == Language::Python && eq.is_some_and(|q| i < q) && s == i {
        return Role::Write { rhs: (eq.unwrap_or(i) + 1, e) };
    }
    Role::Read
}

struct Ctx<'a> {
    files: &'a [FileText],
    parsed: Vec<Option<Parsed>>,
    nodes: usize,
    visited: HashSet<(usize, usize, String)>,
}

impl Ctx<'_> {
    fn parsed(&mut self, fi: usize) -> Option<&Parsed> {
        if self.parsed.get(fi).is_some_and(Option::is_none) {
            let f = self.files.get(fi)?;
            if let Some(slot) = self.parsed.get_mut(fi) {
                *slot = Some(parse(f));
            }
        }
        self.parsed.get(fi).and_then(Option::as_ref)
    }

    fn node(&mut self, fi: usize, tok: usize, kind: FlowKind, label: String) -> Option<FlowNode> {
        if self.nodes >= MAX_NODES {
            return None;
        }
        self.nodes += 1;
        let path = self.files.get(fi)?.path.clone();
        let p = self.parsed(fi)?;
        let offset = p.toks.get(tok).map(|t| t.start).unwrap_or(0);
        let pos = p.doc.offset_to_position(offset.min(p.doc.len_chars()));
        Some(FlowNode { kind, label, path, offset, line: pos.line, column: pos.column, children: Vec::new() })
    }

    /// Occurrences of `name` as a variable in a function's parameters and body.
    fn occurrences(&mut self, fi: usize, func: &Func, name: &str) -> Vec<usize> {
        let Some(p) = self.parsed(fi) else { return Vec::new() };
        (func.params.0..func.body.1)
            .filter(|k| {
                p.toks.get(*k).is_some_and(|t| t.kind == TokKind::Ident && t.text == name)
                    && !k.checked_sub(1).and_then(|j| p.toks.get(j)).is_some_and(|x| x.punct_text(".") || x.punct_text(":") && x.joined)
            })
            .collect()
    }

    fn sources(&mut self, fi: usize, func: &Func, name: &str, depth: usize) -> Vec<FlowNode> {
        if depth > MAX_DEPTH || !self.visited.insert((fi, func.start, format!("to:{name}"))) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let f = match <[FileText]>::get(self.files, fi) {
            Some(f) => f,
            None => return out,
        };
        for occ in self.occurrences(fi, func, name) {
            let Some(p) = self.parsed(fi) else { break };
            match role(p, func, occ) {
                Role::Param => {
                    let label = format!("parameter '{name}' of {}", func.name);
                    let index = func.param_names.iter().position(|(_, t)| *t == occ).unwrap_or(0);
                    let has_self = func.param_names.first().is_some_and(|(n, _)| n == "self");
                    let fname = func.name.clone();
                    let Some(mut node) = self.node(fi, occ, FlowKind::Parameter, label) else { break };
                    node.children = self.call_sites(fi, &fname, index, has_self, depth + 1);
                    out.push(node);
                }
                Role::Decl { init } => {
                    let (s, e) = statement(p, occ);
                    let label = text_of(p, f, s, e);
                    let next = init.and_then(|(a, b)| single_var(p, a, b)).and_then(|v| p.toks.get(v).map(|t| t.text.clone()));
                    let Some(mut node) = self.node(fi, s, FlowKind::Declaration, label) else { break };
                    if let Some(v) = next {
                        node.children = self.sources(fi, func, &v, depth + 1);
                    }
                    out.push(node);
                }
                Role::Write { rhs } => {
                    let (s, e) = statement(p, occ);
                    let label = text_of(p, f, s, e);
                    let next = single_var(p, rhs.0, rhs.1).and_then(|v| p.toks.get(v).map(|t| t.text.clone()));
                    let Some(mut node) = self.node(fi, s, FlowKind::Assignment, label) else { break };
                    if let Some(v) = next {
                        node.children = self.sources(fi, func, &v, depth + 1);
                    }
                    out.push(node);
                }
                Role::Read => {}
            }
        }
        out
    }

    /// Calls of `fname` across the files: the argument passed for parameter `index`.
    fn call_sites(&mut self, def_file: usize, fname: &str, index: usize, has_self: bool, depth: usize) -> Vec<FlowNode> {
        let mut out = Vec::new();
        if depth > MAX_DEPTH {
            return out;
        }
        let lang = self.files.get(def_file).map(|f| f.language);
        for fi in 0..self.files.len() {
            if self.files.get(fi).map(|f| f.language) != lang {
                continue;
            }
            let Some(p) = self.parsed(fi) else { continue };
            let calls: Vec<(usize, usize, bool)> = p
                .toks
                .iter()
                .enumerate()
                .filter(|(k, t)| {
                    t.kind == TokKind::Ident
                        && t.text == fname
                        && p.toks.get(k + 1).is_some_and(|n| n.kind == TokKind::Open && n.text == "(")
                        && !k.checked_sub(1).and_then(|j| p.toks.get(j)).is_some_and(|x| x.text == keyword_for(p.language))
                })
                .map(|(k, _)| {
                    let method = k.checked_sub(1).and_then(|j| p.toks.get(j)).is_some_and(|x| x.punct_text("."));
                    (k, k + 1, method)
                })
                .collect();
            for (k, open, method) in calls {
                if self.nodes >= MAX_NODES {
                    return out;
                }
                let files: &[FileText] = self.files;
                let Some(f) = files.get(fi) else { continue };
                let Some(p) = self.parsed(fi) else { break };
                let Some(close) = p.partner.get(open).copied().flatten() else { continue };
                let arg_index = if has_self && method { index.checked_sub(1) } else { Some(index) };
                let Some(arg_index) = arg_index else { continue };
                let args = split_args(p, open, close);
                let Some(&(a, b)) = args.get(arg_index) else { continue };
                let label = format!("{fname}(…{}…)", text_of(p, f, a, b));
                let next = single_var(p, a, b).and_then(|v| p.toks.get(v).map(|t| t.text.clone()));
                let caller = func_at(p, k).cloned();
                let Some(mut node) = self.node(fi, a, FlowKind::CallArgument, label) else { break };
                if let (Some(v), Some(caller)) = (next, caller) {
                    node.children = self.sources(fi, &caller, &v, depth + 1);
                }
                out.push(node);
            }
        }
        out
    }

    fn sinks(&mut self, fi: usize, func: &Func, name: &str, after: usize, depth: usize) -> Vec<FlowNode> {
        if depth > MAX_DEPTH || !self.visited.insert((fi, func.start, format!("from:{name}"))) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let f = match <[FileText]>::get(self.files, fi) {
            Some(f) => f,
            None => return out,
        };
        for occ in self.occurrences(fi, func, name) {
            if occ < after {
                continue;
            }
            let Some(p) = self.parsed(fi) else { break };
            if role(p, func, occ) != Role::Read {
                continue;
            }
            let (s, e) = statement(p, occ);
            let first = p.toks.get(s).map(|t| t.text.clone()).unwrap_or_default();
            let parent = p.parent.get(occ).copied().flatten();
            let label = text_of(p, f, s, e);
            // `let w = …x…` / `w = …x…`: follow w.
            let decl = matches!(first.as_str(), "let" | "var" | "const");
            let eq = (s..e).find(|k| p.parent.get(*k).copied().flatten() == p.parent.get(s).copied().flatten() && is_assign_at(p, *k) == Some(*k));
            if let Some(q) = eq.filter(|q| *q < occ) {
                let target = (s..q).rev().find(|k| p.toks.get(*k).is_some_and(|t| t.kind == TokKind::Ident)).and_then(|k| p.toks.get(k).map(|t| (k, t.text.clone())));
                let kind = if decl { FlowKind::Declaration } else { FlowKind::Assignment };
                let Some(mut node) = self.node(fi, s, kind, label) else { break };
                if let Some((k, w)) = target
                    && w != name
                {
                    node.children = self.sinks(fi, func, &w, k, depth + 1);
                }
                out.push(node);
                continue;
            }
            // An argument of a call `f(…, x, …)`.
            if let Some(open) = parent
                && p.toks.get(open).is_some_and(|t| t.text == "(")
                && let Some(callee) = open.checked_sub(1).and_then(|c| p.toks.get(c)).filter(|t| t.kind == TokKind::Ident && p.toks.get(open).is_some_and(|o| o.joined))
                && let Some(close) = p.partner.get(open).copied().flatten()
            {
                let callee = callee.text.clone();
                let method = open.checked_sub(2).and_then(|c| p.toks.get(c)).is_some_and(|t| t.punct_text("."));
                let index = split_args(p, open, close).iter().position(|(a, b)| *a <= occ && occ < *b).unwrap_or(0);
                let label = format!("{callee}(…{name}…) argument #{}", index + 1);
                let Some(mut node) = self.node(fi, occ, FlowKind::CallArgument, label) else { break };
                node.children = self.flow_into_callee(fi, &callee, index, method, depth + 1);
                out.push(node);
                continue;
            }
            // `Struct { field: x }`.
            if let Some(field) = occ
                .checked_sub(1)
                .and_then(|c| p.toks.get(c))
                .filter(|t| t.punct_text(":"))
                .and_then(|_| occ.checked_sub(2).and_then(|c| p.toks.get(c)))
                .filter(|t| t.kind == TokKind::Ident)
                .filter(|_| parent.and_then(|o| p.toks.get(o)).is_some_and(|o| o.text == "{"))
            {
                let label = format!("field '{}' = {name}", field.text);
                if let Some(node) = self.node(fi, occ, FlowKind::Field, label) {
                    out.push(node);
                }
                continue;
            }
            // The whole statement around the use (out of argument lists and indexes).
            let mut top = occ;
            while let Some(o) = p.parent.get(top).copied().flatten()
                && p.toks.get(o).is_some_and(|t| t.text != "{")
            {
                top = o;
            }
            let (s, e) = statement(p, top);
            let first = p.toks.get(s).map(|t| t.text.as_str()).unwrap_or_default();
            let block = p.parent.get(top).copied().flatten();
            // `return x` or the tail expression of the function.
            let tail = block == Some(func.body.0) && e == func.body.1.saturating_sub(1) && p.language == Language::Rust;
            let kind = if first == "return" || tail { FlowKind::Return } else { FlowKind::Use };
            let label = text_of(p, f, s, e);
            if let Some(node) = self.node(fi, s, kind, label) {
                out.push(node);
            }
        }
        out
    }

    /// Follow a value into the parameter `index` of every function named `callee`.
    fn flow_into_callee(&mut self, from_file: usize, callee: &str, index: usize, method: bool, depth: usize) -> Vec<FlowNode> {
        let mut out = Vec::new();
        let lang = self.files.get(from_file).map(|f| f.language);
        for fi in 0..self.files.len() {
            if self.files.get(fi).map(|f| f.language) != lang {
                continue;
            }
            let Some(p) = self.parsed(fi) else { continue };
            let defs: Vec<Func> = p.funcs.iter().filter(|f| f.name == callee).cloned().collect();
            for def in defs {
                let has_self = def.param_names.first().is_some_and(|(n, _)| n == "self");
                let pi = if method && has_self { index + 1 } else { index };
                let Some((pname, ptok)) = def.param_names.get(pi).cloned() else { continue };
                let label = format!("parameter '{pname}' of {}", def.name);
                let Some(mut node) = self.node(fi, ptok, FlowKind::Parameter, label) else { return out };
                node.children = self.sinks(fi, &def, &pname, ptok, depth + 1);
                out.push(node);
            }
        }
        out
    }
}

/// Top-level comma-separated argument token ranges of a call's `(…)`.
fn split_args(p: &Parsed, open: usize, close: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut a = open + 1;
    for k in open + 1..close {
        if p.toks.get(k).is_some_and(|t| t.punct_text(",")) && p.parent.get(k).copied().flatten() == Some(open) {
            out.push((a, k));
            a = k + 1;
        }
    }
    if a < close {
        out.push((a, close));
    }
    out
}

/// Analyze the variable at `offset` in `files[current]`.
pub fn analyze(files: &[FileText], current: usize, offset: usize, dir: Direction) -> Result<FlowNode, String> {
    let mut ctx = Ctx { files, parsed: (0..files.len()).map(|_| None).collect(), nodes: 0, visited: HashSet::new() };
    let p = ctx.parsed(current).ok_or("no such file")?;
    let at = p
        .toks
        .iter()
        .position(|t| t.kind == TokKind::Ident && t.start <= offset && offset <= t.end)
        .ok_or("put the caret on a variable")?;
    let name = p.toks.get(at).map(|t| t.text.clone()).unwrap_or_default();
    let func = func_at(p, at).cloned().ok_or_else(|| format!("'{name}' is not inside a function"))?;
    let role_here = role(p, &func, at);
    let label = match dir {
        Direction::ToHere => format!("{name} (data flow to here)"),
        Direction::FromHere => format!("{name} (data flow from here)"),
    };
    let mut root = ctx.node(current, at, FlowKind::Variable, label).ok_or("nothing to analyze")?;
    root.children = match dir {
        Direction::ToHere => ctx.sources(current, &func, &name, 0),
        Direction::FromHere => {
            // From a declaration, a parameter or a write: the reads after it; from a read: after
            // the read itself.
            let after = match role_here {
                Role::Read => at + 1,
                _ => at,
            };
            ctx.sinks(current, &func, &name, after, 0)
        }
    };
    Ok(root)
}

// ---------------------------------------------------------------- state and commands

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowResult {
    pub direction: Direction,
    pub root: Option<FlowNode>,
    pub error: Option<String>,
    pub done: bool,
}

pub struct FlowRun {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<FlowNode, String>>,
}

#[derive(Default)]
pub struct DataFlowState {
    pub result: Option<FlowResult>,
    pub running: Option<FlowRun>,
}

impl DataFlowState {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }
}

fn start(s: &mut Session, p: &Value, dir: Direction) -> CmdResult {
    let (path, offset, language, text) = match opt_path(p, "path") {
        Some(path) => {
            let path = crate::normalize_path(&path);
            let text = s.current_text(&path).ok_or_else(|| CommandError::Failed(format!("{} cannot be read", path.display())))?;
            let offset = opt_usize(p, "offset")?.unwrap_or(0);
            (path.clone(), offset, Language::from_path(&path), text)
        }
        None => {
            let (e, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor is open".into()))?;
            let path = d.path.clone().unwrap_or_else(|| PathBuf::from("untitled"));
            let offset = opt_usize(p, "offset")?.unwrap_or(e.selections.primary().head);
            (path, offset, d.language, d.doc.text())
        }
    };
    let mut files = vec![FileText { path: path.clone(), language, text }];
    let mut others: Vec<(PathBuf, Option<String>)> = Vec::new();
    if let Some(project) = &s.ws.project {
        for f in project.tree.files().take(50_000) {
            if others.len() >= MAX_FILES {
                break;
            }
            if f.path != path && Language::from_path(&f.path) == language {
                let open = s.ws.doc_by_path(&f.path).and_then(|d| s.ws.doc(d)).map(|d| d.doc.text());
                others.push((f.path.clone(), open));
            }
        }
    }
    if let Some(run) = s.data_flow.running.take() {
        run.cancel.store(true, Ordering::Relaxed);
    }
    s.data_flow.result = Some(FlowResult { direction: dir, root: None, error: None, done: false });
    let job = move |cancel: &AtomicBool| -> Result<FlowNode, String> {
        for (p, open) in others {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let text = match open {
                Some(t) => t,
                None => match codecraft_fs::read_text_file(&p) {
                    Ok(t) => Document::from_text(&t).text(),
                    Err(_) => continue,
                },
            };
            if text.chars().count() <= MAX_FILE_CHARS {
                files.push(FileText { language: Language::from_path(&p), path: p, text });
            }
        }
        analyze(&files, 0, offset, dir)
    };
    let cancel = Arc::new(AtomicBool::new(false));
    if bool_or(p, "sync", false) {
        let r = job(&cancel);
        finish(s, r);
    } else {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let flag = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("codecraft-dataflow".into())
            .spawn(move || {
                let _ = tx.send(job(&flag));
            })
            .map_err(|e| CommandError::Failed(format!("cannot start: {e}")))?;
        s.data_flow.running = Some(FlowRun { cancel, rx });
        s.status = "Analyzing data flow…".into();
    }
    Ok(json!({ "started": true, "result": s.data_flow.result }))
}

fn finish(s: &mut Session, r: Result<FlowNode, String>) {
    let Some(res) = &mut s.data_flow.result else { return };
    res.done = true;
    match r {
        Ok(root) => {
            s.status = format!("Data flow: {} node(s)", root.count().saturating_sub(1));
            res.root = Some(root);
        }
        Err(e) => {
            s.status = format!("Data flow: {e}");
            res.error = Some(e);
        }
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(run) = &s.data_flow.running else { return false };
    match run.rx.try_recv() {
        Ok(r) => {
            s.data_flow.running = None;
            finish(s, r);
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            s.data_flow.running = None;
            finish(s, Err("the analysis stopped unexpectedly".into()));
            true
        }
    }
}

pub(crate) fn shutdown(s: &mut Session) {
    if let Some(run) = s.data_flow.running.take() {
        run.cancel.store(true, Ordering::Relaxed);
    }
}

fn node_at<'a>(root: &'a FlowNode, path: &[usize]) -> Option<&'a FlowNode> {
    let mut n = root;
    for i in path {
        n = n.children.get(*i)?;
    }
    Some(n)
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.analyzeCode.dataFlowToHere",
            "Data Flow to Here…",
            None,
            "{path?: string, offset?: int, sync?: bool} — where the value of the variable at the caret comes from: declarations, assignments and the arguments passed for a parameter at its call sites (a tree, approximated lexically)",
            Context::Global,
            has_editor,
            |s, p| start(s, p, Direction::ToHere),
        ),
        spec(
            "code.analyzeCode.dataFlowFromHere",
            "Data Flow from Here…",
            None,
            "{path?: string, offset?: int, sync?: bool} — where the value of the variable at the caret goes: variables it initializes or is assigned to, parameters of the functions it is passed to, returns and struct fields",
            Context::Global,
            has_editor,
            |s, p| start(s, p, Direction::FromHere),
        ),
        spec("dataFlow.results", "Data Flow", None, "→ the last data flow tree", Context::Global, always, |s, _| {
            Ok(json!({ "running": s.data_flow.busy(), "result": s.data_flow.result }))
        }),
        spec("dataFlow.close", "Close Data Flow", None, "", Context::Global, always, |s, _| {
            shutdown(s);
            s.data_flow.result = None;
            ok()
        }),
        spec(
            "dataFlow.open",
            "Open Data Flow Node",
            None,
            "{node: [int]} — open a node's code (child indexes from the root)",
            Context::Global,
            |s| s.data_flow.result.as_ref().and_then(|r| r.root.as_ref()).map(|_| ()).ok_or_else(|| "no data flow tree".into()),
            |s, p| {
                let path: Vec<usize> = p
                    .get("node")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_u64().and_then(|n| usize::try_from(n).ok())).collect())
                    .unwrap_or_default();
                let (file, offset) = s
                    .data_flow
                    .result
                    .as_ref()
                    .and_then(|r| r.root.as_ref())
                    .and_then(|root| node_at(root, &path))
                    .map(|n| (n.path.clone(), n.offset))
                    .ok_or_else(|| CommandError::BadParams("no such node".into()))?;
                if !Path::new(&file).is_file() {
                    return Err(CommandError::Failed(format!("{} is not a file", file.display())));
                }
                s.navigate_to(&file, offset)?;
                ok()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn file(path: &str, text: &str) -> FileText {
        FileText { path: path.into(), language: Language::from_path(Path::new(path)), text: text.into() }
    }

    fn labels(n: &FlowNode, depth: usize, out: &mut Vec<String>) {
        for c in &n.children {
            out.push(format!("{}{}", "  ".repeat(depth), c.label));
            labels(c, depth + 1, out);
        }
    }

    fn tree(files: &[FileText], needle: &str, nth: usize, dir: Direction) -> Vec<String> {
        let text = &files[0].text;
        let byte = text.match_indices(needle).nth(nth).unwrap().0;
        let offset = text[..byte].chars().count();
        let root = analyze(files, 0, offset, dir).unwrap();
        let mut out = Vec::new();
        labels(&root, 0, &mut out);
        out
    }

    const MAIN: &str = "fn main() {\n    let base = read();\n    let mut total = base;\n    total += 1;\n    report(total, 2);\n}\n\nfn report(value: u32, width: u32) {\n    let shown = value;\n    println!(\"{}\", shown);\n}\n";

    #[test]
    fn to_here_follows_declarations_assignments_and_call_sites() {
        let files = [file("/m.rs", MAIN)];
        // `shown` in report ← value (parameter) ← call site total ← let total = base ← let base = read()
        let t = tree(&files, "shown)", 0, Direction::ToHere);
        assert_eq!(
            t,
            [
                "let shown = value",
                "  parameter 'value' of report",
                "    report(…total…)",
                "      let mut total = base",
                "        let base = read()",
                "      total += 1",
            ]
        );
    }

    #[test]
    fn from_here_follows_assignments_and_calls_into_parameters() {
        let files = [file("/m.rs", MAIN)];
        let t = tree(&files, "base", 0, Direction::FromHere);
        assert_eq!(
            t,
            [
                "let mut total = base",
                "  report(…total…) argument #1",
                "    parameter 'value' of report",
                "      let shown = value",
                "        println!(\"{}\", shown)",
            ]
        );
    }

    #[test]
    fn methods_returns_and_other_files() {
        let a = "impl S {\n    fn set(&mut self, v: i32) -> i32 {\n        let w = v;\n        w\n    }\n}\nfn go(s: &mut S) {\n    let n = 5;\n    s.set(n);\n}\n";
        let files = [file("/a.rs", a), file("/b.rs", "fn other(s: &mut S) { s.set(7); }\n"), file("/c.py", "def set(v): pass\n")];
        let t = tree(&files, "v: i32", 0, Direction::ToHere);
        assert_eq!(t, ["parameter 'v' of set", "  set(…n…)", "    let n = 5", "  set(…7…)"]);
        let t = tree(&files, "w = v", 0, Direction::FromHere);
        assert!(t.iter().any(|l| l.trim() == "w"), "{t:?}");
        assert!(analyze(&files, 0, 0, Direction::ToHere).is_err(), "`impl` is not a variable in a function");
    }

    #[test]
    fn python_and_hostile_inputs() {
        let py = "def f(a):\n    b = a\n    g(b)\n\ndef g(x):\n    return x\n";
        let files = [file("/m.py", py)];
        let t = tree(&files, "a)", 0, Direction::FromHere);
        assert!(t.iter().any(|l| l.contains("b = a")), "{t:?}");
        for text in ["", "fn", "fn (", "fn x(", "fn x() {", "let", "x = = =", "😀", "fn f(a,,,) { a = a; a(a(a)); }"] {
            let files = [file("/h.rs", text)];
            for off in [0, 1, 3, 100] {
                let _ = analyze(&files, 0, off, Direction::ToHere);
                let _ = analyze(&files, 0, off, Direction::FromHere);
            }
        }
        // A self-referencing flow terminates.
        let files = [file("/r.rs", "fn f(a: u8) { let b = a; f(b); }\n")];
        let t = tree(&files, "b = a", 0, Direction::ToHere);
        assert!(!t.is_empty());
    }
}
