//! Language-server decorations of the visible documents: semantic highlighting (locals vs
//! parameters vs types vs mutable bindings, unresolved references) and inlay hints (`: i32`).
//!
//! Requests go out for every document shown in a visible editor group (each group's selected
//! tab; hidden tabs never ask), the focused one first, once its text has been stable briefly.
//! In-flight requests and staleness are tracked per document and kind; answers for an outdated
//! revision are dropped (the next pump asks again).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::Value;

use codecraft_lsp::ServerKind;
use codecraft_lsp::lsp_types as lt;
use codecraft_syntax::HighlightKind;
use codecraft_workspace::{DocId, InlayHint, InlayKind, InlayLabelPart, InlayLocation, SemanticSpan};

use crate::Session;
use crate::convert::utf16_col_to_char;
use crate::lsp::{Pending, server_kind};

/// Wait this long after the last edit before asking (typing does not flood the server).
const SETTLE: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(crate) struct DecorState {
    /// (doc, inlay?) requests in flight.
    inflight: HashSet<(DocId, bool)>,
    /// When each document's current revision was first seen.
    seen: HashMap<DocId, (u64, Instant)>,
}

impl DecorState {
    /// Whether a request of this kind is in flight for `doc`.
    #[cfg(test)]
    pub(crate) fn in_flight(&self, doc: DocId, inlay: bool) -> bool {
        self.inflight.contains(&(doc, inlay))
    }

    /// Whether `doc` at `revision` has been stable for [`SETTLE`] (records a new revision).
    fn settled(&mut self, doc: DocId, revision: u64, now: Instant) -> bool {
        let first_seen = match self.seen.get(&doc) {
            Some((r, t)) if *r == revision => *t,
            _ => {
                self.seen.insert(doc, (revision, now));
                now
            }
        };
        now.duration_since(first_seen) >= SETTLE
    }

    /// Forget documents that are no longer open.
    fn retain(&mut self, open: impl Fn(DocId) -> bool) {
        self.seen.retain(|d, _| open(*d));
        self.inflight.retain(|(d, _)| open(*d));
    }
}

/// Map an LSP semantic token type (+ modifiers) to a highlight class. `None` keeps the
/// lexical highlight (punctuation, operators, …).
pub(crate) fn map_token(token: &str, modifiers: &[&str]) -> Option<HighlightKind> {
    let has = |m: &str| modifiers.contains(&m);
    Some(match token {
        "function" | "method" => HighlightKind::Function,
        "macro" | "builtinAttribute" => HighlightKind::Macro,
        "struct" | "enum" | "interface" | "typeAlias" | "class" | "union" | "typeParameter" | "builtinType"
        | "type" | "selfTypeKeyword" | "trait" => HighlightKind::Type,
        "enumMember" | "constParameter" => HighlightKind::Constant,
        "variable" if has("static") || has("constant") => HighlightKind::Constant,
        "const" | "static" => HighlightKind::Constant,
        "variable" | "parameter" | "label" => HighlightKind::Ident,
        "property" | "field" => HighlightKind::Field,
        "keyword" | "controlFlow" => HighlightKind::Keyword,
        "selfKeyword" => HighlightKind::SelfKeyword,
        "lifetime" => HighlightKind::Lifetime,
        "attribute" | "derive" | "decorator" => HighlightKind::Attribute,
        "string" => HighlightKind::String,
        "number" => HighlightKind::Number,
        "boolean" => HighlightKind::Boolean,
        "comment" if has("documentation") => HighlightKind::DocComment,
        "comment" => HighlightKind::Comment,
        "unresolvedReference" => HighlightKind::Ident,
        _ => return None,
    })
}

/// Decode LSP relative semantic tokens into per-line char-column spans.
pub(crate) fn decode_tokens(
    lines: &dyn Fn(usize) -> Option<String>,
    data: &[lt::SemanticToken],
    legend: &lt::SemanticTokensLegend,
) -> Vec<Vec<SemanticSpan>> {
    let mut out: Vec<Vec<SemanticSpan>> = Vec::new();
    let (mut line, mut col16) = (0u32, 0u32);
    let mut cached: Option<(u32, String)> = None;
    for t in data {
        if t.delta_line > 0 {
            line = line.saturating_add(t.delta_line);
            col16 = t.delta_start;
        } else {
            col16 = col16.saturating_add(t.delta_start);
        }
        let Some(name) = legend.token_types.get(t.token_type as usize).map(|tt| tt.as_str()) else { continue };
        let modifiers: Vec<&str> = legend
            .token_modifiers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < 32 && t.token_modifiers_bitset & (1u32 << i) != 0)
            .map(|(_, m)| m.as_str())
            .collect();
        let unresolved = name == "unresolvedReference";
        let Some(kind) = map_token(name, &modifiers) else { continue };
        if cached.as_ref().is_none_or(|(l, _)| *l != line) {
            cached = Some((line, lines(line as usize).unwrap_or_default()));
        }
        let text = cached.as_ref().map(|(_, t)| t.as_str()).unwrap_or("");
        let start = utf16_col_to_char(text, col16);
        let end = utf16_col_to_char(text, col16.saturating_add(t.length));
        if end <= start {
            continue;
        }
        let l = line as usize;
        if l > 1_000_000 {
            break;
        }
        if out.len() <= l {
            out.resize_with(l + 1, Vec::new);
        }
        if let Some(v) = out.get_mut(l) {
            v.push(SemanticSpan { start, end, kind, mutable: modifiers.contains(&"mutable"), unresolved });
        }
    }
    out
}

fn inlay_label(label: &lt::InlayHintLabel) -> String {
    match label {
        lt::InlayHintLabel::String(s) => s.clone(),
        lt::InlayHintLabel::LabelParts(parts) => parts.iter().map(|p| p.value.as_str()).collect(),
    }
}

/// Longest tooltip kept per hint or part (chars).
const MAX_TOOLTIP: usize = 4000;

fn clip(s: &str) -> String {
    s.chars().take(MAX_TOOLTIP).collect()
}

fn hint_tooltip(t: &lt::InlayHintTooltip) -> String {
    match t {
        lt::InlayHintTooltip::String(s) => clip(s),
        lt::InlayHintTooltip::MarkupContent(m) => clip(&m.value),
    }
}

fn part_tooltip(t: &lt::InlayHintLabelPartTooltip) -> String {
    match t {
        lt::InlayHintLabelPartTooltip::String(s) => clip(s),
        lt::InlayHintLabelPartTooltip::MarkupContent(m) => clip(&m.value),
    }
}

/// The label parts of a hint with their tooltips and locations (empty for a plain label).
pub(crate) fn inlay_parts(label: &lt::InlayHintLabel) -> Vec<InlayLabelPart> {
    let lt::InlayHintLabel::LabelParts(parts) = label else { return Vec::new() };
    parts
        .iter()
        .take(64)
        .map(|p| InlayLabelPart {
            value: p.value.clone(),
            tooltip: p.tooltip.as_ref().map(part_tooltip).filter(|t| !t.trim().is_empty()),
            location: p.location.as_ref().and_then(|l| {
                Some(InlayLocation {
                    path: codecraft_lsp::uri_to_path(&l.uri)?,
                    line: l.range.start.line as usize,
                    utf16_column: l.range.start.character,
                })
            }),
        })
        .collect()
}

impl Session {
    /// Documents shown in a visible editor group (each group's selected tab), the focused
    /// group's first, without duplicates. Hidden tabs are not included.
    pub fn visible_decoration_docs(&self) -> Vec<DocId> {
        let mut out: Vec<DocId> = Vec::new();
        let active = self.ws.active().map(|(e, _)| e.doc);
        let groups = self.ws.group_ids();
        let shown = groups
            .iter()
            .filter_map(|g| self.ws.group(*g).and_then(|gr| gr.active))
            .filter_map(|e| self.ws.editor(e).map(|e| e.doc));
        for d in active.into_iter().chain(shown) {
            if !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }

    /// Ask the servers of the visible documents for semantic tokens and inlay hints when stale.
    pub(crate) fn request_decorations(&mut self) {
        if self.power_save() {
            return;
        }
        let docs = self.visible_decoration_docs();
        let now = Instant::now();
        for doc in docs {
            self.request_decorations_for(doc, now);
        }
        if self.decor.seen.len() > 64 {
            let open: HashSet<DocId> = self.ws.docs().map(|d| d.id).collect();
            self.decor.retain(|d| open.contains(&d));
        }
    }

    /// One document's decoration requests (each kind only when stale and not already asked).
    fn request_decorations_for(&mut self, doc: DocId, now: Instant) {
        let Some(od) = self.ws.doc(doc) else { return };
        let (revision, path, language) = (od.doc.revision(), od.path.clone(), od.language);
        let Some(path) = path else { return };
        let Some(kind) = server_kind(language) else { return };
        if !self.decor.settled(doc, revision, now) {
            return;
        }
        let (has_legend, has_inlay) = self.server_decoration_support(kind);
        let Some(od) = self.ws.doc(doc) else { return };
        let semantic_stale = od.semantic.as_ref().is_none_or(|(r, _)| *r != revision);
        let inlay_stale = od.inlay_hints.as_ref().is_none_or(|(r, _)| *r != revision);
        let full_range = {
            let last = od.doc.len_lines().saturating_sub(1);
            let end = od.doc.offset_to_utf16(od.doc.len_chars());
            lt::Range {
                start: lt::Position { line: 0, character: 0 },
                end: lt::Position { line: end.0.max(last as u32), character: end.1 },
            }
        };
        if has_legend && semantic_stale && !self.decor.inflight.contains(&(doc, false)) {
            self.lsp_sync(doc);
            let p = path.clone();
            if self
                .lsp_send_to(kind, |c| c.semantic_tokens_full(&p), Pending::SemanticTokens { doc, revision, kind })
                .is_ok()
            {
                self.decor.inflight.insert((doc, false));
            }
        }
        if has_inlay && self.ws.settings.show_inlay_hints && inlay_stale && !self.decor.inflight.contains(&(doc, true))
        {
            self.lsp_sync(doc);
            if self
                .lsp_send_to(kind, |c| c.inlay_hints(&path, full_range), Pending::InlayHints { doc, revision })
                .is_ok()
            {
                self.decor.inflight.insert((doc, true));
            }
        }
    }

    pub(crate) fn on_semantic_tokens(&mut self, doc: DocId, revision: u64, kind: ServerKind, value: Option<Value>) {
        self.decor.inflight.remove(&(doc, false));
        let Some(value) = value else { return };
        let Some(legend) = self.server_legend(kind) else { return };
        let tokens: Option<lt::SemanticTokensResult> = serde_json::from_value(value).ok();
        let data = match tokens {
            Some(lt::SemanticTokensResult::Tokens(t)) => t.data,
            Some(lt::SemanticTokensResult::Partial(p)) => p.data,
            None => return,
        };
        let Some(od) = self.ws.doc_mut(doc) else { return };
        if od.doc.revision() != revision {
            return;
        }
        let text_doc = &od.doc;
        let spans = decode_tokens(&|l| text_doc.line_text(l), &data, &legend);
        od.semantic = Some((revision, spans));
    }

    pub(crate) fn on_inlay_hints(&mut self, doc: DocId, revision: u64, value: Option<Value>) {
        self.decor.inflight.remove(&(doc, true));
        let Some(value) = value else { return };
        let hints: Vec<lt::InlayHint> =
            serde_json::from_value::<Option<Vec<lt::InlayHint>>>(value).ok().flatten().unwrap_or_default();
        let Some(od) = self.ws.doc_mut(doc) else { return };
        if od.doc.revision() != revision {
            return;
        }
        let mut out: Vec<InlayHint> = hints
            .iter()
            .take(20_000)
            .map(|h| InlayHint {
                offset: od.doc.utf16_to_offset(h.position.line, h.position.character),
                label: inlay_label(&h.label),
                kind: match h.kind {
                    Some(lt::InlayHintKind::TYPE) => InlayKind::Type,
                    Some(lt::InlayHintKind::PARAMETER) => InlayKind::Parameter,
                    _ => InlayKind::Other,
                },
                pad_left: h.padding_left.unwrap_or(false),
                pad_right: h.padding_right.unwrap_or(false),
                parts: inlay_parts(&h.label),
                tooltip: h.tooltip.as_ref().map(hint_tooltip).filter(|t| !t.trim().is_empty()),
            })
            .filter(|h| !h.label.trim().is_empty())
            .collect();
        out.sort_by_key(|h| h.offset);
        od.inlay_hints = Some((revision, out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_relative_tokens_with_utf16_columns() {
        let legend = lt::SemanticTokensLegend {
            token_types: vec!["variable", "function", "punctuation"]
                .into_iter()
                .map(lt::SemanticTokenType::new)
                .collect(),
            token_modifiers: vec![lt::SemanticTokenModifier::new("mutable")],
        };
        let lines = ["let 😀x = f();", "  x"];
        let get = |l: usize| lines.get(l).map(|s| s.to_string());
        let tok = |dl, ds, len, ty, m| lt::SemanticToken {
            delta_line: dl,
            delta_start: ds,
            length: len,
            token_type: ty,
            token_modifiers_bitset: m,
        };
        // `x` after the emoji: UTF-16 column 6 (emoji = 2 units) → char column 5.
        let data = vec![tok(0, 6, 1, 0, 1), tok(0, 4, 1, 1, 0), tok(0, 1, 1, 2, 0), tok(1, 2, 1, 0, 0)];
        let spans = decode_tokens(&get, &data, &legend);
        assert_eq!(
            spans[0][0],
            SemanticSpan { start: 5, end: 6, kind: HighlightKind::Ident, mutable: true, unresolved: false }
        );
        assert_eq!(spans[0][1].kind, HighlightKind::Function);
        assert_eq!(spans[0].len(), 2, "punctuation keeps the lexical colour");
        assert_eq!(spans[1][0].start, 2);
    }

    #[test]
    fn visible_docs_are_each_groups_selected_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        let open = |s: &mut Session, n: &str| {
            let p = dir.path().join(n);
            std::fs::write(&p, "fn main() {}\n").unwrap();
            let e = s.open_file(&p).unwrap();
            s.ws.editor(e).unwrap().doc
        };
        let a = open(&mut s, "a.rs");
        let b = open(&mut s, "b.rs");
        assert_eq!(s.visible_decoration_docs(), vec![b], "a is a hidden tab");
        s.execute("window.editorTabs.splitRight", &Value::Null).unwrap();
        let c = open(&mut s, "c.rs");
        // The focused group shows c, the other still shows b; a stays hidden.
        assert_eq!(s.visible_decoration_docs(), vec![c, b]);
        assert!(!s.visible_decoration_docs().contains(&a));
        // Without a server nothing is asked (and nothing panics).
        s.request_decorations();
        assert!(!s.decor.in_flight(b, false) && !s.decor.in_flight(c, true));
    }

    #[test]
    fn settling_is_tracked_per_document() {
        let mut st = DecorState::default();
        let t0 = Instant::now();
        let (d1, d2) = (DocId(1), DocId(2));
        assert!(!st.settled(d1, 1, t0));
        assert!(!st.settled(d2, 7, t0 + SETTLE / 2));
        assert!(st.settled(d1, 1, t0 + SETTLE));
        // d2 was first seen later: it is not settled yet.
        assert!(!st.settled(d2, 7, t0 + SETTLE));
        assert!(st.settled(d2, 7, t0 + SETTLE * 2));
        // A new revision of d1 starts its wait again; d2 is unaffected.
        assert!(!st.settled(d1, 2, t0 + SETTLE * 2));
        assert!(st.settled(d2, 7, t0 + SETTLE * 3));
        st.inflight.insert((d1, true));
        assert!(st.in_flight(d1, true) && !st.in_flight(d1, false) && !st.in_flight(d2, true));
        st.retain(|d| d == d2);
        assert!(!st.in_flight(d1, true) && st.seen.contains_key(&d2) && !st.seen.contains_key(&d1));
    }

    #[test]
    fn inlay_label_parts_keep_tooltips_and_locations() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("lib.rs");
        let uri: lt::Uri = codecraft_lsp::path_to_uri(&target).unwrap();
        let label = lt::InlayHintLabel::LabelParts(vec![
            lt::InlayHintLabelPart {
                value: "Vec".into(),
                tooltip: Some(lt::InlayHintLabelPartTooltip::String("struct Vec<T>".into())),
                location: Some(lt::Location {
                    uri,
                    range: lt::Range {
                        start: lt::Position { line: 3, character: 11 },
                        end: lt::Position { line: 3, character: 14 },
                    },
                }),
                command: None,
            },
            lt::InlayHintLabelPart { value: "<i32>".into(), tooltip: None, location: None, command: None },
        ]);
        let parts = inlay_parts(&label);
        assert_eq!(parts.len(), 2);
        assert_eq!(inlay_label(&label), "Vec<i32>");
        assert_eq!(parts[0].tooltip.as_deref(), Some("struct Vec<T>"));
        let loc = parts[0].location.as_ref().unwrap();
        assert_eq!((loc.path.file_name().unwrap().to_str(), loc.line, loc.utf16_column), (Some("lib.rs"), 3, 11));
        assert!(parts[1].location.is_none());
        assert!(inlay_parts(&lt::InlayHintLabel::String("x".into())).is_empty());
    }

    #[test]
    fn hostile_tokens_do_not_panic() {
        let legend = lt::SemanticTokensLegend { token_types: vec![], token_modifiers: vec![] };
        let data = vec![lt::SemanticToken {
            delta_line: u32::MAX,
            delta_start: u32::MAX,
            length: u32::MAX,
            token_type: 99,
            token_modifiers_bitset: u32::MAX,
        }];
        let spans = decode_tokens(&|_| None, &data, &legend);
        assert!(spans.is_empty());
    }
}
