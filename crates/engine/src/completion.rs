//! Code completion (M10 track B): requests with their trigger context (stale responses are
//! dropped by generation), ranking (case-insensitive prefixes and camel humps over the server's
//! `filterText`, then its `sortText` relevance, preselect), the delayed auto-popup with the
//! server's trigger characters, re-requests for incomplete lists, `completionItem/resolve` for the
//! selected item (documentation beside the list, auto-import `additionalTextEdits`), accepting an
//! item (LSP snippets start a live template session: Tab/Shift+Tab, mirrors, choices, `$0`) and
//! Type-Matching completion (Ctrl+Shift+Space).
//!
//! The popup state is [`crate::popup::CompletionPopup`]; this module's [`CompletionState`] keeps
//! the bookkeeping (generations, timers) and the in-place rename ([`crate::inline_rename`]).

use std::ops::Range;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_lsp::lsp_types as lt;
use codecraft_lsp::{ResponseError, ServerKind};
use codecraft_text::{Assoc, Change, ChangeSet, Document, Edit, EditGroup, Selection, SelectionSet};
use codecraft_workspace::{DocId, EditorId};

use crate::commands::{CmdResult, CommandSpec, Context, has_lsp_editor, ok, spec};
use crate::convert::{self, from_lsp_pos};
use crate::editing::{Chooser, ChooserItem, ChooserKind, TemplateSession};
use crate::lsp::Pending;
use crate::popup::{CompletionItem, CompletionPopup};
use crate::{CommandError, Popup, Session, snippet};

/// How long the selected item stays selected before it is resolved.
const RESOLVE_DELAY: Duration = Duration::from_millis(80);
/// Most items kept from one response.
const MAX_ITEMS: usize = 2000;
/// Longest pattern/name the camel-hump matcher handles (longer ones use plain prefix matching).
const MAX_PATTERN: usize = 64;
const MAX_NAME: usize = 256;

/// Completion bookkeeping on the [`Session`].
#[derive(Default)]
pub struct CompletionState {
    /// Generation of the newest completion request; responses for older ones are dropped.
    pub(crate) generation: u64,
    auto: Option<AutoPopup>,
    /// The selected (generation, item) and when it becomes due for resolving.
    resolve_key: Option<(u64, usize)>,
    resolve_due: Option<Instant>,
    resolve_sent: Option<(u64, usize)>,
    /// Type-Matching: waiting for signature help to tell the expected type (generation).
    expected_pending: Option<u64>,
    /// In-place rename in progress (Shift+F6).
    pub rename: Option<crate::inline_rename::InlineRename>,
}

impl CompletionState {
    /// Whether an auto-popup is scheduled (for tests and the UI's repaint).
    pub fn auto_scheduled(&self) -> bool {
        self.auto.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AutoPopup {
    editor: EditorId,
    anchor: usize,
    trigger: Trigger,
    due: Instant,
}

/// What started a request (LSP `CompletionTriggerKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    Invoked,
    Character(char),
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Basic,
    TypeMatching,
}

/// Completion-related LSP requests besides `textDocument/completion` itself.
pub(crate) enum LookupPending {
    /// Documentation and edits of the selected item.
    Resolve { generation: u64, item: usize },
    /// An item accepted before it was resolved: its auto-import edits join the accept.
    AcceptResolve(Box<Accepted>),
    /// Type-Matching: signature help for the expected type.
    ExpectedType { generation: u64 },
    /// In-place rename: the symbol's occurrences in the file.
    RenameHighlights(crate::inline_rename::HighlightsPending),
}

/// What an accept did, to add the resolved `additionalTextEdits` to it afterwards.
pub(crate) struct Accepted {
    editor: EditorId,
    doc: DocId,
    /// Document revision right after the accept.
    revision: u64,
    /// The document before the accept (LSP ranges refer to it).
    before: Document,
    before_sels: SelectionSet,
    /// The accept's changes, in `before` coordinates.
    changes: ChangeSet,
    /// The replaced range (edits overlapping it are dropped).
    replaced: Range<usize>,
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.codeCompletion.typeMatching",
            "Type-Matching",
            None,
            "Completion filtered to the expected type at the caret (from `let x: T =` or the parameter being passed); ranked by relevance when the type is unknown",
            Context::Editor,
            has_lsp_editor,
            type_matching,
        ),
        spec(
            "editor.inlineRename.commit",
            "Finish In-Place Rename",
            None,
            "{newName?: string} — rename the symbol being renamed in place (default: the name typed in the editor)",
            Context::Editor,
            crate::inline_rename::is_active,
            |s, p| crate::inline_rename::commit(s, crate::params::opt_str(p, "newName")),
        ),
        spec(
            "editor.inlineRename.cancel",
            "Cancel In-Place Rename",
            None,
            "Restore the original name",
            Context::Editor,
            crate::inline_rename::is_active,
            |s, _| crate::inline_rename::cancel(s),
        ),
    ]
}

// ------------------------------------------------------------------------------- ranking

/// How a typed prefix matches a name: `tier` 0 = whole name, 1 = prefix, 2 = camel humps from the
/// first char, 3 = humps from a later word start; higher `score` is better within a tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameMatch {
    pub tier: u8,
    pub score: i64,
    /// Matched char indices in the name (for highlighting).
    pub indices: Vec<usize>,
}

fn eq_ignore_case(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Word starts of a name: its first char, a letter/digit after a separator, an upper-case letter
/// after a lower-case one or a digit, and the last capital of an acronym (`HTMLParser` → `P`).
fn word_starts(n: &[char]) -> Vec<bool> {
    let mut out = vec![false; n.len()];
    for (i, c) in n.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| n.get(p));
        let next = n.get(i + 1);
        out[i] = match prev {
            None => true,
            Some(p) if !p.is_alphanumeric() => c.is_alphanumeric(),
            Some(p) if c.is_uppercase() && (p.is_lowercase() || p.is_ascii_digit()) => true,
            Some(p) if c.is_uppercase() && p.is_uppercase() => next.is_some_and(|x| x.is_lowercase()),
            Some(p) if c.is_ascii_digit() => !p.is_ascii_digit(),
            _ => false,
        };
    }
    out
}

/// Match `pattern` against `name` IntelliJ-style: case-insensitive prefix, else camel humps
/// (`hm` → `HashMap`, `gm` → `get_mut`), else humps starting at a later word (`map` → `HashMap`).
pub fn match_name(pattern: &str, name: &str) -> Option<NameMatch> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    if p.is_empty() {
        return Some(NameMatch { tier: 1, score: 0, indices: Vec::new() });
    }
    if p.len() > n.len() {
        return None;
    }
    if p.iter().zip(&n).all(|(a, b)| eq_ignore_case(*a, *b)) {
        let same_case = p.iter().zip(&n).filter(|(a, b)| a == b).count() as i64;
        let tier = if p.len() == n.len() { 0 } else { 1 };
        let score = 1000 + same_case * 10 - (n.len() - p.len()) as i64;
        return Some(NameMatch { tier, score, indices: (0..p.len()).collect() });
    }
    if p.len() > MAX_PATTERN || n.len() > MAX_NAME {
        return None;
    }
    let starts = word_starts(&n);
    let hump_starts: Vec<usize> = (0..n.len()).filter(|i| starts[*i]).collect();
    // best[i * len + j]: best score matching p[i..] with p[i] at n[j], and where p[i + 1] goes.
    let len = n.len();
    let mut best: Vec<Option<(i64, usize)>> = vec![None; p.len() * len];
    for i in (0..p.len()).rev() {
        for j in 0..len {
            if !eq_ignore_case(p[i], n[j]) {
                continue;
            }
            let base = 1 + i64::from(p[i] == n[j]) + if starts[j] { 2 } else { 0 };
            if i + 1 == p.len() {
                best[i * len + j] = Some((base, usize::MAX));
                continue;
            }
            let mut choice: Option<(i64, usize)> = None;
            if let Some(Some((sc, _))) = best.get((i + 1) * len + j + 1).filter(|_| j + 1 < len) {
                choice = Some((sc + 3, j + 1));
            }
            for &k in hump_starts.iter().filter(|k| **k > j + 1) {
                if let Some(Some((sc, _))) = best.get((i + 1) * len + k) {
                    let cand = sc - 1;
                    if choice.is_none_or(|(c, _)| cand > c) {
                        choice = Some((cand, k));
                    }
                }
            }
            best[i * len + j] = choice.map(|(sc, next)| (sc + base, next));
        }
    }
    let mut first: Option<(u8, i64, usize)> = None;
    for &j in &hump_starts {
        if let Some(Some((sc, _))) = best.get(j) {
            let (tier, sc) = if j == 0 { (2u8, sc + 5) } else { (3u8, *sc) };
            if first.is_none_or(|(t, s, _)| (tier, -sc) < (t, -s)) {
                first = Some((tier, sc, j));
            }
        }
    }
    let (tier, score, mut j) = first?;
    let mut indices = Vec::with_capacity(p.len());
    for i in 0..p.len() {
        indices.push(j);
        match best.get(i * len + j).copied().flatten() {
            Some((_, next)) if next != usize::MAX => j = next,
            _ => break,
        }
    }
    Some(NameMatch { tier, score, indices })
}

/// Indices of `visible` items, best first, with label highlights. `relevance_first` sorts by the
/// server's relevance (`sortText`) before the match quality (Type-Matching without a type).
pub fn rank(items: &[CompletionItem], prefix: &str, relevance_first: bool) -> Vec<(usize, Vec<usize>)> {
    let mut scored: Vec<(u8, i64, usize, Vec<usize>)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let m = match_name(prefix, &it.filter_text)?;
            let highlight = if it.filter_text == it.label {
                m.indices.clone()
            } else {
                match_name(prefix, &it.label).map(|l| l.indices).unwrap_or_default()
            };
            Some((m.tier, m.score, i, highlight))
        })
        .collect();
    scored.sort_by(|a, b| {
        let (ia, ib) = (&items[a.2], &items[b.2]);
        let rel = ia.sort_text.cmp(&ib.sort_text);
        let tier = a.0.cmp(&b.0);
        let primary = if relevance_first { rel.then(tier) } else { tier.then(rel) };
        primary
            .then(ia.deprecated.cmp(&ib.deprecated))
            .then(b.1.cmp(&a.1))
            .then(ia.label.len().cmp(&ib.label.len()))
            .then(ia.label.cmp(&ib.label))
    });
    scored.into_iter().map(|(_, _, i, h)| (i, h)).collect()
}

// ------------------------------------------------------------------------------- type matching

/// The type an item produces: a function's return type, else a value's type.
fn value_type(kind: &str, detail: Option<&str>) -> Option<String> {
    let d = detail?.trim();
    match kind {
        "fn" | "method" => {
            let ret = d.rsplit_once("->").map(|(_, r)| r.trim()).unwrap_or("()");
            let ret = ret.split(" where ").next().unwrap_or(ret).trim();
            Some(ret.to_string())
        }
        "variable" | "field" | "const" | "variant" | "static" => Some(d.to_string()),
        _ => None,
    }
}

fn normalize_type(t: &str) -> String {
    let mut s: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    loop {
        if let Some(r) = s.strip_prefix("&mut") {
            s = r.to_string();
        } else if let Some(r) = s.strip_prefix('&') {
            s = r.to_string();
        } else {
            break;
        }
    }
    s
}

/// Whether a value of type `actual` fits where `expected` is wanted (references and generic
/// arguments are not compared).
pub fn types_match(expected: &str, actual: &str) -> bool {
    let (e, a) = (normalize_type(expected), normalize_type(actual));
    if e.is_empty() || a.is_empty() {
        return false;
    }
    if e == a {
        return true;
    }
    let erase = |t: &str| t.split('<').next().unwrap_or(t).to_string();
    erase(&e) == erase(&a) && (e.contains('<') || a.contains('<'))
}

/// `let x: T = ` / `const X: T = ` right before the caret → `T`.
pub fn lexical_expected_type(before: &str) -> Option<String> {
    let line = before.rsplit('\n').next().unwrap_or(before);
    let re = regex::Regex::new(r"(?:let|const|static)\s+(?:mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:\s*(.+?)\s*=\s*$").ok()?;
    re.captures(line).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string()).filter(|t| !t.is_empty())
}

/// The active parameter's type from signature help (`name: &str` → `&str`).
fn expected_from_signature(help: &lt::SignatureHelp) -> Option<String> {
    let sig = help.signatures.get(help.active_signature.unwrap_or(0) as usize)?;
    let active = sig.active_parameter.or(help.active_parameter).unwrap_or(0) as usize;
    let param = sig.parameters.as_ref()?.get(active)?;
    let text = match &param.label {
        lt::ParameterLabel::Simple(s) => s.clone(),
        lt::ParameterLabel::LabelOffsets([a, b]) => {
            let units: Vec<u16> = sig.label.encode_utf16().collect();
            String::from_utf16_lossy(units.get(*a as usize..*b as usize)?)
        }
    };
    let (_, ty) = text.split_once(':')?;
    let ty = ty.trim();
    (!ty.is_empty()).then(|| ty.to_string())
}

fn type_matching(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let caret = e.selections.primary().head;
    let mut anchor = caret;
    while anchor > 0 && d.doc.char_at(anchor - 1).is_some_and(is_ident_char) {
        anchor -= 1;
    }
    let line_start = d.doc.line_start(d.doc.char_to_line(anchor));
    let lexical = lexical_expected_type(&d.doc.slice(line_start..anchor));
    request(s, anchor, Mode::TypeMatching, Trigger::Invoked)?;
    let generation = s.completion.generation;
    if let Some(Popup::Completion(c)) = &mut s.popup {
        c.expected_type = lexical.clone();
    }
    if lexical.is_none() {
        let (_, path, _, pos) = s.caret_for_lsp()?;
        let sent = s.lsp_send(
            |c| c.signature_help(&path, pos.line, pos.character),
            Pending::Lookup(LookupPending::ExpectedType { generation }),
        );
        if sent.is_ok() {
            s.completion.expected_pending = Some(generation);
        }
    }
    refresh_filter(s);
    ok()
}

// ------------------------------------------------------------------------------- requests

pub(crate) fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Request completion at the caret; `anchor` starts the identifier being completed.
pub(crate) fn request(s: &mut Session, anchor: usize, mode: Mode, trigger: Trigger) -> Result<(), CommandError> {
    let (editor, path, _, pos) = s.caret_for_lsp()?;
    s.completion.generation = s.completion.generation.wrapping_add(1);
    let generation = s.completion.generation;
    let auto = matches!(trigger, Trigger::Character(_)) || s.completion.auto.is_some_and(|a| a.anchor == anchor);
    let keep = trigger == Trigger::Incomplete;
    match &mut s.popup {
        Some(Popup::Completion(c)) if keep && c.editor == editor && c.anchor == anchor => c.generation = generation,
        _ => {
            s.popup = Some(Popup::Completion(CompletionPopup {
                editor,
                anchor,
                pending: true,
                auto,
                generation,
                type_matching: mode == Mode::TypeMatching,
                ..CompletionPopup::default()
            }));
        }
    }
    let uri = codecraft_lsp::path_to_uri(&path).map_err(|e| CommandError::Failed(e.to_string()))?;
    let context = match trigger {
        Trigger::Invoked => json!({ "triggerKind": 1 }),
        Trigger::Character(c) => json!({ "triggerKind": 2, "triggerCharacter": c.to_string() }),
        Trigger::Incomplete => json!({ "triggerKind": 3 }),
    };
    let params = json!({
        "textDocument": { "uri": uri.as_str() },
        "position": { "line": pos.line, "character": pos.character },
        "context": context,
    });
    let sent = s.lsp_send(
        |c| c.request_raw("textDocument/completion", params),
        Pending::Completion { editor, anchor, generation },
    );
    if sent.is_err() && !keep {
        s.popup = None;
    }
    sent
}

/// Convert one LSP item (`raw` is kept for `completionItem/resolve`).
fn convert_item(doc: &Document, raw: Value) -> Option<CompletionItem> {
    let it: lt::CompletionItem = serde_json::from_value(raw.clone()).ok()?;
    let snippet = it.insert_text_format == Some(lt::InsertTextFormat::SNIPPET);
    let (raw_text, replace_start) = match &it.text_edit {
        Some(lt::CompletionTextEdit::Edit(te)) => (te.new_text.clone(), Some(from_lsp_pos(doc, &te.range.start))),
        Some(lt::CompletionTextEdit::InsertAndReplace(ir)) => {
            (ir.new_text.clone(), Some(from_lsp_pos(doc, &ir.replace.start)))
        }
        None => (it.insert_text.clone().unwrap_or_else(|| it.label.clone()), None),
    };
    let (insert_text, cursor, snippet_src) = if snippet {
        let parsed = snippet::parse(&raw_text);
        let cursor = parsed.end.or_else(|| parsed.stops.first().and_then(|t| t.ranges.first()).map(|r| r.start));
        (parsed.text, cursor, Some(raw_text))
    } else {
        (raw_text, None, None)
    };
    let additional_edits = it
        .additional_text_edits
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|e| (convert::from_lsp_range(doc, &e.range), e.new_text.clone()))
        .collect();
    let detail = it
        .label_details
        .as_ref()
        .and_then(|d| d.description.clone().or_else(|| d.detail.clone()))
        .or(it.detail.clone());
    let kind = it.kind.map(crate::lsp::completion_kind).unwrap_or("text").to_string();
    let signature = it.detail.clone().or_else(|| it.label_details.as_ref().and_then(|d| d.description.clone()));
    let deprecated = it.deprecated.unwrap_or(false)
        || it.tags.as_ref().is_some_and(|t| t.contains(&lt::CompletionItemTag::DEPRECATED));
    Some(CompletionItem {
        filter_text: it.filter_text.clone().unwrap_or_else(|| it.label.clone()),
        sort_text: it.sort_text.clone().unwrap_or_else(|| it.label.clone()),
        value_type: value_type(&kind, signature.as_deref()),
        label: it.label,
        detail,
        kind,
        insert_text,
        cursor,
        replace_start,
        additional_edits,
        documentation: it.documentation.as_ref().map(convert::documentation_text),
        deprecated,
        snippet: snippet_src,
        adjust_indent: it.insert_text_mode != Some(lt::InsertTextMode::AS_IS),
        preselect: it.preselect.unwrap_or(false),
        resolved: false,
        lsp: Some(raw),
    })
}

/// A `textDocument/completion` response.
pub(crate) fn on_completion(s: &mut Session, editor: EditorId, anchor: usize, generation: u64, value: Value) {
    let Some(Popup::Completion(popup)) = &s.popup else { return };
    if popup.editor != editor || popup.anchor != anchor || popup.generation != generation {
        return;
    }
    let (raw_items, incomplete) = match value {
        Value::Array(v) => (v, false),
        Value::Object(mut m) => {
            let incomplete = m.get("isIncomplete").and_then(Value::as_bool).unwrap_or(false);
            match m.remove("items") {
                Some(Value::Array(v)) => (v, incomplete),
                _ => (Vec::new(), incomplete),
            }
        }
        _ => (Vec::new(), false),
    };
    let Some(od) = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)) else { return };
    let items: Vec<CompletionItem> =
        raw_items.into_iter().take(MAX_ITEMS).filter_map(|raw| convert_item(&od.doc, raw)).collect();
    let auto = popup.auto;
    if let Some(Popup::Completion(popup)) = &mut s.popup {
        popup.items = items;
        popup.pending = false;
        popup.incomplete = incomplete;
        popup.selected = 0;
        popup.visible.clear();
    }
    refresh_filter(s);
    // Preselect the item the server marks when it is among the best matches.
    if let Some(Popup::Completion(popup)) = &mut s.popup {
        let best = popup.visible.first().and_then(|i| popup.items.get(*i)).map(|it| match_tier(&popup.prefix, it));
        if let Some(pos) = popup.visible.iter().position(|i| {
            popup.items.get(*i).is_some_and(|it| it.preselect && Some(match_tier(&popup.prefix, it)) == best)
        }) {
            popup.selected = pos;
        }
    } else if !auto {
        s.status = "No suggestions".into();
    }
}

fn match_tier(prefix: &str, it: &CompletionItem) -> u8 {
    match_name(prefix, &it.filter_text).map(|m| m.tier).unwrap_or(u8::MAX)
}

/// Re-filter the list against the identifier typed since the anchor; closes it when the caret
/// left the identifier or nothing matches. Keeps the selected item when the user moved off the
/// best match and it still matches.
pub(crate) fn refresh_filter(s: &mut Session) {
    let Some(Popup::Completion(popup)) = &s.popup else { return };
    let Some(e) = s.ws.editor(popup.editor) else {
        s.popup = None;
        return;
    };
    let Some(d) = s.ws.doc(e.doc) else { return };
    let caret = e.selections.primary().head;
    if caret < popup.anchor {
        s.popup = None;
        return;
    }
    let prefix = d.doc.slice(popup.anchor..caret);
    if prefix.chars().any(|c| !is_ident_char(c)) {
        s.popup = None;
        return;
    }
    let pending = popup.pending;
    let expected_pending = s.completion.expected_pending == Some(popup.generation);
    let (ranked, header) = if popup.type_matching {
        type_matched(&popup.items, &prefix, popup.expected_type.as_deref(), expected_pending)
    } else {
        (rank(&popup.items, &prefix, false), None)
    };
    if ranked.is_empty() && !pending {
        s.popup = None;
        return;
    }
    let previous = popup.visible.get(popup.selected).copied().filter(|_| popup.selected > 0);
    if let Some(Popup::Completion(popup)) = &mut s.popup {
        popup.prefix = prefix;
        popup.header = header;
        popup.visible = ranked.iter().map(|(i, _)| *i).collect();
        popup.highlights = ranked.into_iter().map(|(_, h)| h).collect();
        popup.selected = previous.and_then(|p| popup.visible.iter().position(|i| *i == p)).unwrap_or(0);
    }
}

/// Type-Matching: items of the expected type, else everything by relevance (with a header saying so).
fn type_matched(
    items: &[CompletionItem],
    prefix: &str,
    expected: Option<&str>,
    expected_pending: bool,
) -> (Vec<(usize, Vec<usize>)>, Option<String>) {
    match expected {
        Some(t) => {
            let ranked = rank(items, prefix, true);
            let matching: Vec<(usize, Vec<usize>)> = ranked
                .iter()
                .filter(|(i, _)| {
                    items.get(*i).and_then(|it| it.value_type.as_deref()).is_some_and(|v| types_match(t, v))
                })
                .cloned()
                .collect();
            if matching.is_empty() {
                (ranked, Some(format!("No suggestions of type {t}; sorted by relevance")))
            } else {
                (matching, Some(format!("Type-Matching: {t}")))
            }
        }
        None if expected_pending => {
            (rank(items, prefix, true), Some("Type-Matching: finding the expected type…".into()))
        }
        None => (rank(items, prefix, true), Some("Expected type unknown; sorted by relevance".into())),
    }
}

// ------------------------------------------------------------------------------- auto-popup

/// The trigger characters of a server (Rust's `.` and `::` when it announces none).
fn trigger_characters(s: &Session, kind: ServerKind) -> Vec<char> {
    let announced: Vec<char> = s
        .server_capabilities(kind)
        .and_then(|c| c.completion_provider)
        .and_then(|p| p.trigger_characters)
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            let mut ch = t.chars();
            match (ch.next(), ch.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        })
        .collect();
    if announced.is_empty() { vec!['.', ':'] } else { announced }
}

/// Whether typing `typed` (after `prev`, with `run_start` the first char of the identifier run
/// before it) should pop completion up.
fn char_triggers(typed: char, prev: Option<char>, run_start: Option<char>) -> bool {
    match typed {
        // `::` only (a single `:` is a type ascription or a label).
        ':' => prev == Some(':'),
        // Member access, not a float (`1.`) or a range (`..`).
        '.' => match prev {
            Some(p) if is_ident_char(p) => !run_start.is_some_and(|c| c.is_ascii_digit()),
            Some(p) => matches!(p, ')' | ']' | '?' | '"' | '>' | '}'),
            None => false,
        },
        // Too noisy as triggers: every call, char literal/lifetime, space or newline.
        '(' | '\'' | ' ' | '\t' | '\n' | ',' | '=' => false,
        _ => true,
    }
}

/// After a typed char: re-request an incomplete list, or schedule the auto-popup.
pub(crate) fn after_typed(s: &mut Session, typed: char) {
    if let Some(Popup::Completion(c)) = &s.popup {
        if c.incomplete && is_ident_char(typed) && c.items.iter().any(|i| i.lsp.is_some()) {
            let (anchor, mode) = (c.anchor, if c.type_matching { Mode::TypeMatching } else { Mode::Basic });
            let _ = request(s, anchor, mode, Trigger::Incomplete);
        }
        return;
    }
    // File › Power Save Mode (track F) turns the auto-popup off too.
    if !s.ws.settings.completion_auto_popup
        || s.power_save()
        || s.completion.rename.is_some()
        || s.lsp_ready().is_err()
    {
        return;
    }
    let Some((e, d)) = s.ws.active() else { return };
    if e.selections.len() != 1 {
        return;
    }
    let Some(kind) = crate::lsp::server_kind(d.language) else { return };
    let caret = e.selections.primary().head;
    let prev = caret.checked_sub(2).and_then(|i| d.doc.char_at(i));
    let trigger = if is_ident_char(typed) {
        // The start of a new identifier (not inside one or a number).
        (!typed.is_ascii_digit() && !prev.is_some_and(is_ident_char))
            .then_some((caret.saturating_sub(1), Trigger::Invoked))
    } else {
        let mut run = caret.saturating_sub(1);
        while run > 0 && d.doc.char_at(run - 1).is_some_and(is_ident_char) {
            run -= 1;
        }
        let run_start = (run + 1 < caret).then(|| d.doc.char_at(run)).flatten();
        (trigger_characters(s, kind).contains(&typed) && char_triggers(typed, prev, run_start))
            .then_some((caret, Trigger::Character(typed)))
    };
    let Some((anchor, trigger)) = trigger else { return };
    let editor = e.id;
    let delay = Duration::from_millis(s.ws.settings.completion_auto_popup_delay_ms.min(5000));
    s.completion.auto = Some(AutoPopup { editor, anchor, trigger, due: Instant::now() + delay });
    if delay.is_zero() {
        pump_auto(s);
    }
}

fn pump_auto(s: &mut Session) -> bool {
    let Some(a) = s.completion.auto else { return false };
    if Instant::now() < a.due {
        return false;
    }
    let still_valid = s.popup.is_none()
        && s.ws.active().is_some_and(|(e, d)| {
            let caret = e.selections.primary().head;
            e.id == a.editor
                && e.selections.len() == 1
                && caret >= a.anchor
                && caret.saturating_sub(a.anchor) <= 256
                && d.doc.slice(a.anchor..caret).chars().all(is_ident_char)
        });
    if still_valid {
        let _ = request(s, a.anchor, Mode::Basic, a.trigger);
    }
    s.completion.auto = None;
    true
}

// ------------------------------------------------------------------------------- resolve

fn server_for_editor(s: &Session, editor: EditorId) -> Option<ServerKind> {
    s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)).and_then(|d| crate::lsp::server_kind(d.language))
}

fn resolves(s: &Session, kind: ServerKind) -> bool {
    s.server_capabilities(kind).and_then(|c| c.completion_provider).and_then(|p| p.resolve_provider).unwrap_or(false)
}

/// Resolve the selected item once it stayed selected for [`RESOLVE_DELAY`].
fn pump_resolve(s: &mut Session) -> bool {
    let Some(Popup::Completion(c)) = &s.popup else {
        s.completion.resolve_key = None;
        return false;
    };
    let Some(&index) = c.visible.get(c.selected) else { return false };
    let Some(item) = c.items.get(index) else { return false };
    if item.resolved || item.lsp.is_none() {
        return false;
    }
    let key = (c.generation, index);
    if s.completion.resolve_key != Some(key) {
        s.completion.resolve_key = Some(key);
        s.completion.resolve_due = Some(Instant::now() + RESOLVE_DELAY);
        return false;
    }
    if s.completion.resolve_sent == Some(key) || s.completion.resolve_due.is_some_and(|d| Instant::now() < d) {
        return false;
    }
    let (editor, raw) = (c.editor, item.lsp.clone().unwrap_or(Value::Null));
    s.completion.resolve_sent = Some(key);
    let Some(kind) = server_for_editor(s, editor).filter(|k| resolves(s, *k)) else {
        if let Some(Popup::Completion(c)) = &mut s.popup
            && let Some(it) = c.items.get_mut(index)
        {
            it.resolved = true;
        }
        return false;
    };
    let _ = s.lsp_send_to(
        kind,
        |c| c.request_raw("completionItem/resolve", raw),
        Pending::Lookup(LookupPending::Resolve { generation: key.0, item: index }),
    );
    false
}

/// Fold a resolved item into the popup's copy.
fn on_resolved(s: &mut Session, generation: u64, index: usize, value: Value) {
    let Some(Popup::Completion(c)) = &s.popup else { return };
    if c.generation != generation {
        return;
    }
    let Some(od) = s.ws.editor(c.editor).and_then(|e| s.ws.doc(e.doc)) else { return };
    let Ok(it) = serde_json::from_value::<lt::CompletionItem>(value.clone()) else { return };
    let edits: Option<Vec<(Range<usize>, String)>> = it
        .additional_text_edits
        .as_ref()
        .map(|v| v.iter().map(|e| (convert::from_lsp_range(&od.doc, &e.range), e.new_text.clone())).collect());
    let Some(Popup::Completion(c)) = &mut s.popup else { return };
    let Some(item) = c.items.get_mut(index) else { return };
    if let Some(doc) = it.documentation.as_ref().map(convert::documentation_text) {
        item.documentation = Some(doc);
    }
    if let Some(edits) = edits {
        item.additional_edits = edits;
    }
    if item.detail.is_none() {
        item.detail = it.detail.clone();
    }
    if let Some(sig) = &it.detail
        && item.value_type.is_none()
    {
        item.value_type = value_type(&item.kind, Some(sig));
    }
    item.resolved = true;
    item.lsp = Some(value);
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = pump_auto(s);
    changed |= pump_resolve(s);
    changed |= crate::inline_rename::pump(s);
    changed
}

/// Responses to [`LookupPending`] requests (errors included).
pub(crate) fn on_response(s: &mut Session, pending: LookupPending, result: Result<Value, ResponseError>) {
    match pending {
        LookupPending::Resolve { generation, item } => match result {
            Ok(v) => on_resolved(s, generation, item, v),
            Err(_) => {
                if let Some(Popup::Completion(c)) = &mut s.popup
                    && c.generation == generation
                    && let Some(it) = c.items.get_mut(item)
                {
                    it.resolved = true;
                }
            }
        },
        LookupPending::AcceptResolve(acc) => {
            if let Ok(v) = result {
                late_additional_edits(s, *acc, v);
            }
        }
        LookupPending::ExpectedType { generation } => {
            if s.completion.expected_pending == Some(generation) {
                s.completion.expected_pending = None;
            }
            let help: Option<lt::SignatureHelp> = result.ok().and_then(|v| serde_json::from_value(v).ok()).flatten();
            let expected = help.as_ref().and_then(expected_from_signature);
            if let Some(Popup::Completion(c)) = &mut s.popup
                && c.generation == generation
            {
                c.expected_type = expected;
            }
            refresh_filter(s);
        }
        LookupPending::RenameHighlights(p) => crate::inline_rename::on_highlights(s, p, result),
    }
}

// ------------------------------------------------------------------------------- accept

/// Values for snippet variables (`TM_FILENAME`, `TM_SELECTED_TEXT`, `CLIPBOARD`, …).
fn snippet_variables(s: &Session, start: usize, end: usize) -> impl Fn(&str) -> Option<String> + use<> {
    let mut vars: Vec<(&'static str, String)> = Vec::new();
    if let Some((e, d)) = s.ws.active() {
        let sel = e.selections.primary();
        let line = d.doc.char_to_line(start);
        vars.push(("TM_SELECTED_TEXT", if sel.is_empty() { String::new() } else { d.doc.slice(sel.range()) }));
        vars.push(("TM_CURRENT_LINE", d.doc.line_text(line).unwrap_or_default()));
        vars.push(("TM_CURRENT_WORD", d.doc.slice(start..end)));
        vars.push(("TM_LINE_INDEX", line.to_string()));
        vars.push(("TM_LINE_NUMBER", (line + 1).to_string()));
        if let Some(p) = &d.path {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let base = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            vars.push(("TM_FILENAME", name));
            vars.push(("TM_FILENAME_BASE", base));
            vars.push(("TM_FILEPATH", p.display().to_string()));
            if let Some(dir) = p.parent() {
                vars.push(("TM_DIRECTORY", dir.display().to_string()));
            }
            if let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) {
                if let Ok(rel) = p.strip_prefix(&root) {
                    vars.push(("RELATIVE_FILEPATH", rel.display().to_string()));
                }
                vars.push((
                    "WORKSPACE_NAME",
                    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                ));
            }
        }
        if let Some(c) = d.language.line_comment() {
            vars.push(("LINE_COMMENT", c.to_string()));
        }
        if let Some((a, b)) = d.language.block_comment() {
            vars.push(("BLOCK_COMMENT_START", a.to_string()));
            vars.push(("BLOCK_COMMENT_END", b.to_string()));
        }
    }
    if let Some((text, _)) = &s.clipboard {
        vars.push(("CLIPBOARD", text.clone()));
    }
    move |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone())
}

fn selection_over(ranges: &[Range<usize>], fallback: usize) -> SelectionSet {
    if ranges.is_empty() {
        return SelectionSet::caret(fallback);
    }
    SelectionSet::from_vec(ranges.iter().map(|r| Selection::new(r.start, r.end)).collect(), 0)
}

/// Accept a completion item: replace the typed identifier, apply its auto-import edits in the same
/// undo step and start a live template session over a snippet's placeholders.
pub(crate) fn accept(s: &mut Session, p: &Value) -> CmdResult {
    let Some(Popup::Completion(c)) = s.popup.take() else { return ok() };
    let idx = crate::params::opt_usize(p, "index")?.unwrap_or(c.selected);
    let Some(item) = c.visible.get(idx).and_then(|i| c.items.get(*i)).cloned() else { return ok() };
    let Some((eid, did)) = s.active_ids() else { return ok() };
    if eid != c.editor {
        return ok();
    }
    let Some((e, d)) = s.ws.active() else { return ok() };
    let caret = e.selections.primary().head;
    let start = item.replace_start.unwrap_or(c.anchor).min(caret);
    // Replace the rest of the identifier after the caret too (RustRover's Tab behaviour).
    let mut end = caret;
    while d.doc.char_at(end).is_some_and(is_ident_char) {
        end += 1;
    }
    let indent = d.doc.indent_of_line(d.doc.char_to_line(start));
    let before = d.doc.snapshot();
    let before_sels = e.selections.clone();
    let adjust = |t: &str| if item.adjust_indent { snippet::adjust_indentation(t, &indent) } else { t.to_string() };
    let parsed = match &item.snippet {
        Some(src) => {
            let vars = snippet_variables(s, start, end);
            snippet::parse_with(&adjust(src), &vars)
        }
        None => snippet::Snippet { text: adjust(&item.insert_text), stops: Vec::new(), end: item.cursor },
    };
    let len = parsed.text.chars().count();
    let mut changes = vec![Change::new(start, end, parsed.text.clone())];
    for (r, text) in &item.additional_edits {
        if r.end <= start || r.start >= end {
            changes.push(Change::new(r.start, r.end, text.clone()));
        }
    }
    let cs = ChangeSet::new(changes).or_else(|_| ChangeSet::new(vec![Change::new(start, end, parsed.text.clone())]));
    let cs = cs.map_err(|e| CommandError::Failed(e.to_string()))?;
    let inserted_start = cs.map_pos(end, Assoc::After).saturating_sub(len);
    let shift = |r: &Range<usize>| inserted_start + r.start.min(len)..inserted_start + r.end.min(len);
    let vars: Vec<Vec<Range<usize>>> = parsed.stops.iter().map(|t| t.ranges.iter().map(shift).collect()).collect();
    let choices: Vec<Vec<String>> = parsed.stops.iter().map(|t| t.choices.clone()).collect();
    let final_caret = inserted_start + parsed.end.map_or(len, |e| e.min(len));
    let selections = selection_over(vars.first().map(Vec::as_slice).unwrap_or(&[]), final_caret);
    s.edit_active(EditGroup::Standalone, |_, _, _| Edit { changes: cs.clone(), selections })?;
    let revision = s.ws.doc(did).map(|d| d.doc.revision()).unwrap_or(0);
    if !vars.is_empty() {
        s.editing.session =
            Some(TemplateSession { editor: eid, revision, vars, current: 0, end: Some(final_caret), choices });
        offer_choices(s);
    }
    // Not resolved yet: its auto-import edits are added when the server answers.
    if !item.resolved
        && let Some(raw) = item.lsp.clone()
        && let Some(kind) = server_for_editor(s, eid).filter(|k| resolves(s, *k))
    {
        let acc = Accepted { editor: eid, doc: did, revision, before, before_sels, changes: cs, replaced: start..end };
        let _ = s.lsp_send_to(
            kind,
            |c| c.request_raw("completionItem/resolve", raw),
            Pending::Lookup(LookupPending::AcceptResolve(Box::new(acc))),
        );
    }
    Ok(json!({ "inserted": item.label, "placeholders": s.editing.session.as_ref().map_or(0, |t| t.vars.len()) }))
}

/// The resolved item of an accept arrived: add its `additionalTextEdits` to the accept's undo
/// step (when nothing changed since), else apply them as their own step.
fn late_additional_edits(s: &mut Session, acc: Accepted, value: Value) {
    let Ok(it) = serde_json::from_value::<lt::CompletionItem>(value) else { return };
    let edits: Vec<(Range<usize>, String)> = it
        .additional_text_edits
        .unwrap_or_default()
        .iter()
        .map(|e| (convert::from_lsp_range(&acc.before, &e.range), e.new_text.clone()))
        .filter(|(r, _)| r.end <= acc.replaced.start || r.start >= acc.replaced.end)
        .collect();
    if edits.is_empty() {
        return;
    }
    // The same edits in post-accept coordinates.
    let extra: Vec<Change> = edits
        .iter()
        .map(|(r, t)| {
            let a = acc.changes.map_pos(r.start, Assoc::Before);
            Change::new(a, acc.changes.map_pos(r.end, Assoc::After).max(a), t.clone())
        })
        .collect();
    let Ok(extra) = ChangeSet::new(extra) else { return };
    let Some(od) = s.ws.doc(acc.doc) else { return };
    let current = od.doc.revision();
    if current == acc.revision {
        let mut combined: Vec<Change> = acc.changes.changes().to_vec();
        combined.extend(edits.iter().map(|(r, t)| Change::new(r.start, r.end, t.clone())));
        let Ok(combined) = ChangeSet::new(combined) else { return };
        let sels = s.ws.editor(acc.editor).map(|e| e.selections.map(&extra)).unwrap_or_else(|| SelectionSet::caret(0));
        let Some(od) = s.ws.doc_mut(acc.doc) else { return };
        if od.doc.undo().is_none() {
            return;
        }
        let edit = Edit { changes: combined, selections: sels.clone() };
        if od.doc.apply(&edit, &acc.before_sels, EditGroup::Standalone).is_err() {
            // Put the accept back as it was.
            let _ = od.doc.redo();
            return;
        }
        let len = od.doc.len_chars();
        if let Some(e) = s.ws.editor_mut(acc.editor) {
            e.selections = sels.clamp(len);
        }
        crate::editing::after_edit(s, acc.editor, &extra);
        s.after_text_change(acc.editor);
        return;
    }
    // The user typed since: map the edits through those changes and apply them on their own.
    let Some(sets) = od.doc.changes_since(acc.revision) else { return };
    let mut mapped: Vec<Change> = extra.changes().to_vec();
    for set in sets {
        for c in &mut mapped {
            let a = set.map_pos(c.start, Assoc::Before);
            c.end = set.map_pos(c.end, Assoc::After).max(a);
            c.start = a;
        }
    }
    let Ok(mapped) = ChangeSet::new(mapped) else { return };
    if s.ws.active_editor() == Some(acc.editor) {
        let _ = s.edit_active(EditGroup::Standalone, |_, sel, _| Edit {
            changes: mapped.clone(),
            selections: sel.map(&mapped),
        });
    }
}

/// When the current snippet placeholder offers choices (`${1|a,b|}`), show them.
pub(crate) fn offer_choices(s: &mut Session) {
    let Some(t) = &s.editing.session else { return };
    let Some(choices) = t.choices.get(t.current).filter(|c| c.len() > 1) else { return };
    let items =
        choices.iter().map(|c| ChooserItem { label: c.clone(), detail: String::new(), key: c.clone() }).collect();
    s.editing.chooser = Some(Chooser {
        kind: ChooserKind::SnippetChoice,
        editor: t.editor,
        title: "Choose".into(),
        items,
        query: String::new(),
        selected: 0,
    });
}

/// Put `value` into every occurrence of the current placeholder (a snippet choice).
pub(crate) fn apply_choice(s: &mut Session, value: &str) -> CmdResult {
    let Some((eid, did)) = s.active_ids() else { return ok() };
    let revision = s.ws.doc(did).map(|d| d.doc.revision()).unwrap_or(0);
    let Some(t) = s.editing.session_for(eid, revision).cloned() else { return ok() };
    let ranges = t.current_ranges().to_vec();
    let changes: Vec<Change> = ranges.iter().map(|r| Change::new(r.start, r.end, value)).collect();
    let cs = ChangeSet::new(changes).map_err(|e| CommandError::Failed(e.to_string()))?;
    let sels: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| {
            let a = cs.map_pos(r.start, Assoc::Before);
            a..cs.map_pos(r.end, Assoc::After).max(a)
        })
        .collect();
    let selections = selection_over(&sels, 0);
    s.edit_active(EditGroup::Standalone, |_, _, _| Edit { changes: cs.clone(), selections })?;
    Ok(json!({ "chosen": value }))
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use codecraft_syntax::Language;

    fn item(label: &str, sort: &str) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            filter_text: label.into(),
            sort_text: sort.into(),
            insert_text: label.into(),
            ..CompletionItem::default()
        }
    }

    fn labels(items: &[CompletionItem], prefix: &str) -> Vec<String> {
        rank(items, prefix, false).into_iter().map(|(i, _)| items[i].label.clone()).collect()
    }

    #[test]
    fn prefix_and_camel_humps() {
        assert_eq!(match_name("has", "HashMap").map(|m| (m.tier, m.indices)), Some((1, vec![0, 1, 2])));
        let m = match_name("hm", "HashMap").unwrap();
        assert_eq!((m.tier, m.indices), (2, vec![0, 4]));
        assert_eq!(match_name("hasma", "HashMap").unwrap().indices, vec![0, 1, 2, 4, 5]);
        assert_eq!(match_name("gm", "get_mut").unwrap().indices, vec![0, 4]);
        assert_eq!(match_name("map", "HashMap").map(|m| (m.tier, m.indices)), Some((3, vec![4, 5, 6])));
        assert_eq!(match_name("hp", "HTMLParser").unwrap().indices, vec![0, 4]);
        assert_eq!(match_name("HASHMAP", "HashMap").unwrap().tier, 0);
        assert!(match_name("ash", "HashMap").is_none());
        assert!(match_name("xyz", "HashMap").is_none());
        assert!(match_name("longer", "short").is_none());
        assert_eq!(match_name("", "x").unwrap().tier, 1);
        // Case-matching prefixes score higher.
        assert!(match_name("Str", "String").unwrap().score > match_name("str", "String").unwrap().score);
        // Unicode and huge inputs.
        assert!(match_name("é", "Élan").is_some());
        let long = "a".repeat(5000);
        assert!(match_name("ab", &long).is_none());
        assert!(match_name(&"a".repeat(300), &long).is_some());
    }

    #[test]
    fn ranking_uses_tiers_then_sort_text() {
        let items = vec![
            item("to_owned", "0003"),
            item("to_string", "0001"),
            item("into_iter", "0000"),
            item("ToString", "0002"),
            item("iter", "0004"),
        ];
        // Prefix matches first (relevance among them), then humps from a later word.
        assert_eq!(labels(&items, "to"), vec!["to_string", "ToString", "to_owned"]);
        assert_eq!(labels(&items, "it"), vec!["iter", "into_iter"]);
        assert_eq!(labels(&items, "ts"), vec!["to_string", "ToString"]);
        // Empty prefix: the server's order.
        assert_eq!(labels(&items, ""), vec!["into_iter", "to_string", "ToString", "to_owned", "iter"]);
        // filterText is matched, the label highlighted.
        let mut it = item("greet(…)", "1");
        it.filter_text = "greet".into();
        let r = rank(&[it], "gr", false);
        assert_eq!(r, vec![(0, vec![0, 1])]);
        // Relevance first (Type-Matching without a type).
        let items = vec![item("abc", "2"), item("xAb", "1")];
        assert_eq!(rank(&items, "ab", false).first().map(|r| r.0), Some(0));
        assert_eq!(rank(&items, "ab", true).first().map(|r| r.0), Some(1));
    }

    #[test]
    fn expected_types() {
        assert_eq!(lexical_expected_type("    let name: String = ").as_deref(), Some("String"));
        assert_eq!(lexical_expected_type("let mut v: Vec<u8> =  ").as_deref(), Some("Vec<u8>"));
        assert_eq!(lexical_expected_type("const N: usize = ").as_deref(), Some("usize"));
        assert_eq!(lexical_expected_type("let x = "), None);
        assert!(types_match("String", "String"));
        assert!(types_match("&str", "str"));
        assert!(types_match("Vec<String>", "Vec<T>"));
        assert!(!types_match("String", "i32"));
        assert!(!types_match("", "i32"));
        assert_eq!(value_type("fn", Some("fn(&str) -> String")).as_deref(), Some("String"));
        assert_eq!(value_type("method", Some("fn(&self)")).as_deref(), Some("()"));
        assert_eq!(value_type("variable", Some("i32")).as_deref(), Some("i32"));
        assert_eq!(value_type("struct", Some("Foo")), None);
        let help = lt::SignatureHelp {
            signatures: vec![lt::SignatureInformation {
                label: "fn greet(name: &str, n: u8) -> String".into(),
                documentation: None,
                parameters: Some(vec![
                    lt::ParameterInformation { label: lt::ParameterLabel::LabelOffsets([9, 19]), documentation: None },
                    lt::ParameterInformation { label: lt::ParameterLabel::Simple("n: u8".into()), documentation: None },
                ]),
                active_parameter: None,
            }],
            active_signature: Some(0),
            active_parameter: Some(0),
        };
        assert_eq!(expected_from_signature(&help).as_deref(), Some("&str"));
        let mut h2 = help.clone();
        h2.active_parameter = Some(1);
        assert_eq!(expected_from_signature(&h2).as_deref(), Some("u8"));
        h2.active_parameter = Some(9);
        assert_eq!(expected_from_signature(&h2), None);
    }

    #[test]
    fn type_matching_filters_or_falls_back_to_relevance() {
        let mut a = item("name", "2");
        a.value_type = Some("String".into());
        let mut b = item("count", "1");
        b.value_type = Some("usize".into());
        let items = vec![a, b];
        let (r, h) = type_matched(&items, "", Some("String"), false);
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0]);
        assert_eq!(h.as_deref(), Some("Type-Matching: String"));
        let (r, h) = type_matched(&items, "", Some("bool"), false);
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 0]);
        assert!(h.unwrap().contains("sorted by relevance"));
        let (_, h) = type_matched(&items, "", None, true);
        assert!(h.unwrap().contains("finding"));
    }

    #[test]
    fn trigger_rules() {
        assert!(char_triggers(':', Some(':'), None));
        assert!(!char_triggers(':', Some('x'), None));
        assert!(char_triggers('.', Some('x'), Some('x')));
        assert!(!char_triggers('.', Some('1'), Some('1')));
        assert!(char_triggers('.', Some(')'), None));
        assert!(!char_triggers('.', Some('.'), None));
        assert!(!char_triggers('(', Some('f'), None));
        assert!(char_triggers('/', Some('"'), None));
    }

    // ------------------------------------------------------------- session-level (no server)

    fn session(text: &str) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        let did = s.ws.add_document(None, text);
        if let Some(d) = s.ws.doc_mut(did) {
            d.language = Language::Rust;
        }
        s.ws.open_editor(did).unwrap();
        s
    }

    fn text(s: &Session) -> String {
        s.ws.active().unwrap().1.doc.text()
    }

    fn sel(s: &Session) -> (usize, usize) {
        let p = s.ws.active().unwrap().0.selections.primary();
        (p.start(), p.end())
    }

    fn open_popup(s: &mut Session, anchor: usize, items: Vec<CompletionItem>) {
        let editor = s.ws.active().unwrap().0.id;
        s.popup = Some(Popup::Completion(CompletionPopup { editor, anchor, items, ..CompletionPopup::default() }));
        refresh_filter(s);
    }

    fn snippet_item(label: &str, src: &str) -> CompletionItem {
        let mut it = item(label, "1");
        it.snippet = Some(src.into());
        it.insert_text = snippet::parse(src).text;
        it.adjust_indent = true;
        it.resolved = true;
        it
    }

    #[test]
    fn accepting_a_snippet_walks_its_placeholders() {
        let mut s = session("fn main() {\n    gre\n}");
        s.execute("editor.setSelection", &json!({ "anchor": 19 })).unwrap();
        open_popup(&mut s, 16, vec![snippet_item("greet", "greet(${1:name}, ${2:n})$0")]);
        let r = s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(r["placeholders"], 2);
        assert_eq!(text(&s), "fn main() {\n    greet(name, n)\n}");
        assert_eq!(sel(&s), (22, 26));
        s.execute("editor.type", &json!({ "text": "\"x\"" })).unwrap();
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(sel(&s), (27, 28));
        s.execute("editor.shiftTab", &Value::Null).unwrap();
        assert_eq!(sel(&s), (22, 25));
        s.execute("editor.tab", &Value::Null).unwrap();
        s.execute("editor.type", &json!({ "text": "3" })).unwrap();
        s.execute("editor.tab", &Value::Null).unwrap();
        // `$0`: after the call, session over.
        assert_eq!(text(&s), "fn main() {\n    greet(\"x\", 3)\n}");
        assert_eq!(sel(&s), (29, 29));
        assert!(s.editing.session.is_none());
        // One undo step per edit: the accept is its own.
        for _ in 0..3 {
            s.execute("edit.undo", &Value::Null).unwrap();
        }
        assert_eq!(text(&s), "fn main() {\n    gre\n}");
    }

    #[test]
    fn mirrors_choices_and_indentation() {
        let mut s = session("    ");
        s.execute("editor.setSelection", &json!({ "anchor": 4 })).unwrap();
        open_popup(&mut s, 4, vec![snippet_item("letm", "let ${1:x} = ${2|1,2,3|};\nprintln!(\"{}\", $1);$0")]);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(text(&s), "    let x = 1;\n    println!(\"{}\", x);");
        s.execute("editor.type", &json!({ "text": "val" })).unwrap();
        assert_eq!(text(&s), "    let val = 1;\n    println!(\"{}\", val);");
        // The choice placeholder opens a chooser; picking one fills it.
        s.execute("editor.tab", &Value::Null).unwrap();
        let c = s.editing.chooser.as_ref().expect("choices offered");
        assert_eq!(c.kind, ChooserKind::SnippetChoice);
        assert_eq!(c.items.len(), 3);
        s.execute("editor.chooser.accept", &json!({ "index": 2 })).unwrap();
        assert_eq!(text(&s), "    let val = 3;\n    println!(\"{}\", val);");
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(sel(&s).0, text(&s).chars().count());
    }

    #[test]
    fn additional_edits_join_the_accept_and_undo_together() {
        let mut s = session("fn f() {\n    Has\n}");
        s.execute("editor.setSelection", &json!({ "anchor": 16 })).unwrap();
        let mut it = item("HashMap", "1");
        it.additional_edits = vec![(0..0, "use std::collections::HashMap;\n\n".into())];
        it.resolved = true;
        open_popup(&mut s, 13, vec![it]);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(text(&s), "use std::collections::HashMap;\n\nfn f() {\n    HashMap\n}");
        assert_eq!(sel(&s).0, text(&s).find("HashMap\n}").unwrap() + 7);
        s.execute("edit.undo", &Value::Null).unwrap();
        assert_eq!(text(&s), "fn f() {\n    Has\n}");
    }

    #[test]
    fn late_resolve_merges_into_the_accept() {
        let mut s = session("fn f() {\n    Has\n}");
        s.execute("editor.setSelection", &json!({ "anchor": 16 })).unwrap();
        let (eid, did) = s.active_ids().unwrap();
        let before = s.ws.doc(did).unwrap().doc.snapshot();
        let before_sels = s.ws.active().unwrap().0.selections.clone();
        let mut it = item("HashMap", "1");
        it.lsp = Some(json!({ "label": "HashMap" }));
        open_popup(&mut s, 13, vec![it]);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(text(&s), "fn f() {\n    HashMap\n}");
        let revision = s.ws.doc(did).unwrap().doc.revision();
        let acc = Accepted {
            editor: eid,
            doc: did,
            revision,
            before,
            before_sels,
            changes: ChangeSet::single(13, 16, "HashMap"),
            replaced: 13..16,
        };
        let resolved = json!({
            "label": "HashMap",
            "additionalTextEdits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "use std::collections::HashMap;\n" }],
        });
        on_response(&mut s, LookupPending::AcceptResolve(Box::new(acc)), Ok(resolved));
        assert_eq!(text(&s), "use std::collections::HashMap;\nfn f() {\n    HashMap\n}");
        // The caret followed; one undo removes both.
        assert_eq!(sel(&s).0, text(&s).find("HashMap\n}").unwrap() + 7);
        s.execute("edit.undo", &Value::Null).unwrap();
        assert_eq!(text(&s), "fn f() {\n    Has\n}");
    }

    #[test]
    fn late_resolve_after_typing_is_its_own_step() {
        let mut s = session("x\n");
        s.execute("editor.setSelection", &json!({ "anchor": 1 })).unwrap();
        let (eid, did) = s.active_ids().unwrap();
        let before = s.ws.doc(did).unwrap().doc.snapshot();
        let before_sels = s.ws.active().unwrap().0.selections.clone();
        let mut it = item("xy", "1");
        it.lsp = Some(json!({ "label": "xy" }));
        open_popup(&mut s, 0, vec![it]);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        let revision = s.ws.doc(did).unwrap().doc.revision();
        s.execute("editor.type", &json!({ "text": ";" })).unwrap();
        let acc = Accepted {
            editor: eid,
            doc: did,
            revision,
            before,
            before_sels,
            changes: ChangeSet::single(0, 1, "xy"),
            replaced: 0..1,
        };
        let resolved = json!({
            "label": "xy",
            "additionalTextEdits": [{ "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } }, "newText": "use a::xy;" }],
        });
        on_response(&mut s, LookupPending::AcceptResolve(Box::new(acc)), Ok(resolved));
        assert_eq!(text(&s), "xy;\nuse a::xy;");
    }

    #[test]
    fn resolve_fills_documentation_and_edits() {
        let mut s = session("Has");
        s.execute("editor.setSelection", &json!({ "anchor": 3 })).unwrap();
        let mut it = item("HashMap", "1");
        it.lsp = Some(json!({ "label": "HashMap" }));
        open_popup(&mut s, 0, vec![it]);
        let generation = match &s.popup {
            Some(Popup::Completion(c)) => c.generation,
            _ => panic!(),
        };
        let resolved = json!({
            "label": "HashMap",
            "documentation": { "kind": "markdown", "value": "A hash map." },
            "additionalTextEdits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "use std::collections::HashMap;\n" }],
        });
        // A stale generation is ignored.
        on_response(&mut s, LookupPending::Resolve { generation: generation + 1, item: 0 }, Ok(resolved.clone()));
        let Some(Popup::Completion(c)) = &s.popup else { panic!() };
        assert!(!c.items[0].resolved);
        on_response(&mut s, LookupPending::Resolve { generation, item: 0 }, Ok(resolved));
        let Some(Popup::Completion(c)) = &s.popup else { panic!() };
        assert!(c.items[0].resolved);
        assert_eq!(c.items[0].documentation.as_deref(), Some("A hash map."));
        assert_eq!(c.items[0].additional_edits.len(), 1);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(text(&s), "use std::collections::HashMap;\nHashMap");
    }

    #[test]
    fn responses_convert_filter_preselect_and_drop_stale() {
        let mut s = session("let v = Ve");
        s.execute("editor.setSelection", &json!({ "anchor": 10 })).unwrap();
        let editor = s.ws.active().unwrap().0.id;
        s.popup = Some(Popup::Completion(CompletionPopup {
            editor,
            anchor: 8,
            pending: true,
            generation: 7,
            ..CompletionPopup::default()
        }));
        let list = json!({
            "isIncomplete": true,
            "items": [
                { "label": "Vec", "kind": 22, "sortText": "02" },
                { "label": "VecDeque", "kind": 22, "sortText": "01", "preselect": true },
                { "label": "vec!", "kind": 15, "filterText": "vec", "insertTextFormat": 2, "insertText": "vec![$0]", "sortText": "03" },
                { "label": "Other", "sortText": "00" },
            ],
        });
        // Stale (older generation): dropped.
        on_completion(&mut s, editor, 8, 6, list.clone());
        assert!(matches!(&s.popup, Some(Popup::Completion(c)) if c.pending));
        on_completion(&mut s, editor, 8, 7, list);
        let Some(Popup::Completion(c)) = &s.popup else { panic!() };
        assert!(c.incomplete && !c.pending);
        let vis: Vec<&str> = c.visible.iter().map(|i| c.items[*i].label.as_str()).collect();
        assert_eq!(vis, vec!["VecDeque", "Vec", "vec!"]);
        assert_eq!(c.highlights[0], vec![0, 1]);
        // The preselected item is selected.
        assert_eq!(c.selected, 0);
        let snip = c.items.iter().find(|i| i.label == "vec!").unwrap();
        assert_eq!((snip.insert_text.as_str(), snip.cursor), ("vec![]", Some(5)));
        // Typing filters; the caret leaving the identifier closes the list.
        s.execute("editor.type", &json!({ "text": "cD" })).unwrap();
        let Some(Popup::Completion(c)) = &s.popup else { panic!() };
        assert_eq!(c.visible.len(), 1);
        s.execute("editor.type", &json!({ "text": " " })).unwrap();
        assert!(s.popup.is_none());
    }

    #[test]
    fn auto_popup_needs_a_ready_server_and_respects_the_setting() {
        let mut s = session("");
        s.execute("editor.type", &json!({ "text": "a" })).unwrap();
        assert!(!s.completion.auto_scheduled());
        assert!(s.popup.is_none());
        s.ws.settings.completion_auto_popup = false;
        after_typed(&mut s, 'a');
        assert!(!s.completion.auto_scheduled());
        s.ws.settings.completion_auto_popup = true;
        s.ws.settings.power_save_mode = true;
        after_typed(&mut s, 'a');
        assert!(!s.completion.auto_scheduled());
    }

    #[test]
    fn hostile_params_never_panic() {
        let mut s = session("abc");
        for p in [json!({ "index": 99 }), json!({ "index": -1 }), json!(null), json!({ "index": "x" })] {
            let _ = s.execute("editor.completion.accept", &p);
        }
        open_popup(&mut s, 99, vec![item("abc", "1")]);
        let _ = s.execute("editor.completion.accept", &json!({ "index": 0 }));
        let editor = s.ws.active().unwrap().0.id;
        let mut it = item("x", "1");
        it.additional_edits = vec![(1..99, "z".into()), (0..2, "y".into())];
        it.replace_start = Some(500);
        s.popup = Some(Popup::Completion(CompletionPopup {
            editor,
            anchor: 0,
            items: vec![it],
            visible: vec![0],
            ..CompletionPopup::default()
        }));
        let _ = s.execute("editor.completion.accept", &Value::Null);
        on_completion(&mut s, editor, 0, 0, json!({ "items": "nope" }));
        on_completion(&mut s, editor, 0, 0, json!([{ "nolabel": 1 }, 5, null]));
        on_response(&mut s, LookupPending::Resolve { generation: 0, item: 99 }, Ok(json!(42)));
        on_response(&mut s, LookupPending::ExpectedType { generation: 0 }, Ok(json!("x")));
        let _ = apply_choice(&mut s, "v");
    }
}
