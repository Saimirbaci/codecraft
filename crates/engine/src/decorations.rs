//! Language-server decorations of the active document: semantic highlighting (locals vs
//! parameters vs types vs mutable bindings, unresolved references) and inlay hints (`: i32`).
//!
//! Requests go out for the active document only, once its text has been stable briefly, and
//! answers for an outdated revision are dropped (the next pump asks again).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::Value;

use codecraft_lsp::ServerKind;
use codecraft_lsp::lsp_types as lt;
use codecraft_syntax::HighlightKind;
use codecraft_workspace::{DocId, InlayHint, InlayKind, SemanticSpan};

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

impl Session {
    /// Ask the active document's server for semantic tokens and inlay hints when stale.
    pub(crate) fn request_decorations(&mut self) {
        let Some((_, od)) = self.ws.active() else { return };
        let (doc, revision, path, language) = (od.id, od.doc.revision(), od.path.clone(), od.language);
        let Some(path) = path else { return };
        let Some(kind) = server_kind(language) else { return };
        let now = Instant::now();
        let first_seen = match self.decor.seen.get(&doc) {
            Some((r, t)) if *r == revision => *t,
            _ => {
                self.decor.seen.insert(doc, (revision, now));
                now
            }
        };
        if now.duration_since(first_seen) < SETTLE {
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
