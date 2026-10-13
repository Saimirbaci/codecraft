//! Editor rendering commands (track C): Navigate › Custom Folding…, inlay hint navigation and
//! queries, and typing into a box selection that reaches into virtual space.

use serde_json::{Value, json};

use codecraft_text::EditGroup;
use codecraft_text::column::{BoxPoint, box_selection, type_in_box};
use codecraft_workspace::{EditorId, InlayKind};

use crate::commands::{CmdResult, CommandError, CommandSpec, Context, has_editor, ok, spec};
use crate::convert::utf16_col_to_char;
use crate::editing::ColumnBlock;
use crate::lsp::GotoKind;
use crate::params::{opt_usize, req_usize};
use crate::popup::LocationItem;
use crate::{Popup, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "navigate.navigateInFile.customFolding",
            "Custom Folding…",
            None,
            "",
            Context::Global,
            has_editor,
            custom_folding,
        ),
        spec(
            "editor.inlayHints",
            "Inlay Hints",
            None,
            "{line?: int} — the active document's current inlay hints (all, or one zero-based line)",
            Context::Editor,
            has_editor,
            inlay_hints,
        ),
        spec(
            "editor.inlayHint.navigate",
            "Go to Inlay Hint Target",
            None,
            "{offset: int, part?: int} — the hint at char offset: go to its label part's location, \
             or for a type hint without one, to the type's declaration",
            Context::Editor,
            has_editor,
            navigate_inlay,
        ),
    ]
}

/// Navigate › Custom Folding… (Alt+Cmd+.): the active file's custom folding regions, to jump to.
fn custom_folding(s: &mut Session, _: &Value) -> CmdResult {
    let Some((_, d)) = s.ws.active() else { return ok() };
    let text = d.doc.text();
    let regions = codecraft_syntax::custom_regions(d.language, &text);
    if regions.is_empty() {
        s.status = "No custom folding regions in this file".into();
        return Ok(json!({ "regions": 0 }));
    }
    let Some(path) = d.path.clone() else {
        s.status = "Custom folding regions need a saved file".into();
        return Ok(json!({ "regions": regions.len() }));
    };
    let items: Vec<LocationItem> = regions
        .iter()
        .map(|r| {
            let indent = d.doc.line_text(r.start_line).map(|t| t.chars().take_while(|c| c.is_whitespace()).count());
            LocationItem {
                path: path.clone(),
                line: r.start_line,
                column: indent.unwrap_or(0),
                preview: if r.name.is_empty() { "(unnamed region)".into() } else { r.name.clone() },
            }
        })
        .collect();
    let n = items.len();
    s.popup = Some(Popup::Locations { title: "Custom Folding".into(), items, selected: 0 });
    Ok(json!({ "regions": n }))
}

fn kind_name(k: InlayKind) -> &'static str {
    match k {
        InlayKind::Type => "type",
        InlayKind::Parameter => "parameter",
        InlayKind::Other => "other",
    }
}

fn inlay_hints(s: &mut Session, p: &Value) -> CmdResult {
    let line = opt_usize(p, "line")?;
    let Some((_, d)) = s.ws.active() else { return ok() };
    let revision = d.doc.revision();
    let hints = d.inlay_hints.as_ref().filter(|(r, _)| *r == revision).map(|(_, v)| v.as_slice()).unwrap_or(&[]);
    let out: Vec<Value> = hints
        .iter()
        .filter(|h| line.is_none_or(|l| d.doc.char_to_line(h.offset) == l))
        .take(5000)
        .map(|h| {
            json!({
                "offset": h.offset,
                "label": h.label,
                "kind": kind_name(h.kind),
                "tooltip": h.tooltip,
                "parts": h.parts.iter().map(|part| json!({
                    "value": part.value,
                    "tooltip": part.tooltip,
                    "location": part.location.as_ref().map(|l| json!({ "path": l.path, "line": l.line })),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "hints": out, "current": d.inlay_hints.as_ref().is_some_and(|(r, _)| *r == revision) }))
}

/// Cmd+click on an inlay hint.
fn navigate_inlay(s: &mut Session, p: &Value) -> CmdResult {
    let offset = req_usize(p, "offset")?;
    let part = opt_usize(p, "part")?;
    let Some((_, d)) = s.ws.active() else { return ok() };
    let revision = d.doc.revision();
    let hint = d
        .inlay_hints
        .as_ref()
        .filter(|(r, _)| *r == revision)
        .and_then(|(_, v)| v.iter().find(|h| h.offset == offset))
        .cloned()
        .ok_or_else(|| CommandError::Failed(format!("no inlay hint at offset {offset}")))?;
    // The clicked part's location, else the first part that has one.
    let target = part
        .and_then(|i| hint.parts.get(i))
        .and_then(|x| x.location.clone())
        .or_else(|| part.is_none().then(|| hint.parts.iter().find_map(|x| x.location.clone())).flatten());
    if let Some(loc) = target {
        let text = match s.ws.doc_by_path(&loc.path).and_then(|id| s.ws.doc(id)) {
            Some(od) => od.doc.line_text(loc.line).unwrap_or_default(),
            None => codecraft_fs::read_text_file(&loc.path)
                .ok()
                .and_then(|t| t.lines().nth(loc.line).map(str::to_string))
                .unwrap_or_default(),
        };
        let column = utf16_col_to_char(&text, loc.utf16_column);
        s.navigate_to_position(&loc.path, loc.line, column)?;
        return Ok(json!({ "navigated": true }));
    }
    if hint.kind == InlayKind::Type {
        // The hint follows the binding it describes: ask for that binding's type declaration.
        let at = offset.saturating_sub(1);
        s.execute("editor.setSelection", &json!({ "anchor": at }))?;
        s.lsp_goto(GotoKind::TypeDefinition)?;
        return Ok(json!({ "navigated": "typeDefinition" }));
    }
    s.status = "Nothing to go to from this hint".into();
    Ok(json!({ "navigated": false }))
}

/// An editor's box selection when it is current (same text and carets as when it was made).
fn block_of(s: &Session, editor: EditorId) -> Option<ColumnBlock> {
    let b = s.editing.block.filter(|b| b.editor == editor)?;
    let e = s.ws.editor(editor)?;
    let d = s.ws.doc(e.doc)?;
    let tab = s.ws.settings.tab_size.max(1);
    (b.revision == d.doc.revision() && box_selection(&d.doc, b.anchor, b.head, tab) == e.selections).then_some(b)
}

/// The active editor's current box selection.
pub(crate) fn current_block(s: &Session) -> Option<ColumnBlock> {
    block_of(s, s.ws.active_editor()?)
}

impl Session {
    /// The box selection an editor shows, as `(anchor, head)` lines and display columns, while
    /// it is current. Its columns may lie past line ends (virtual space), where the view draws
    /// the carets.
    pub fn column_box(&self, editor: EditorId) -> Option<(BoxPoint, BoxPoint)> {
        block_of(self, editor).map(|b| (b.anchor, b.head))
    }
}

/// Typing hook: text typed into a box selection that reaches past the end of some lines pads
/// them with spaces first, so it lands in a straight column. `None` = ordinary typing applies.
pub(crate) fn type_in_virtual_box(s: &mut Session, text: &str) -> Option<CmdResult> {
    let block = current_block(s)?;
    let tab = s.ws.settings.tab_size.max(1);
    let (_, d) = s.ws.active()?;
    let (edit, column) = type_in_box(&d.doc, block.anchor, block.head, tab, text)?;
    if let Err(e) = s.edit_active(EditGroup::Typing, move |_, _, _| edit) {
        return Some(Err(e));
    }
    // Keep the (now zero-width) box so further typing continues in the same column.
    if let Some((e, d)) = s.ws.active() {
        let anchor = BoxPoint::new(block.anchor.line, column);
        let head = BoxPoint::new(block.head.line, column);
        s.editing.block = Some(ColumnBlock { editor: e.id, anchor, head, revision: d.doc.revision() });
    }
    Some(ok())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use codecraft_workspace::{InlayHint, InlayLabelPart, InlayLocation};

    fn session(text: &str, name: &str) -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        let p = dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        s.open_file(&p).unwrap();
        (dir, s)
    }

    fn text(s: &Session) -> String {
        s.ws.active().unwrap().1.doc.text()
    }

    #[test]
    fn custom_folding_lists_regions_and_jumps() {
        let (_d, mut s) = session("fn a() {}\n    // region Helpers\nfn b() {}\n    // endregion\n", "a.rs");
        let r = s.execute("navigate.navigateInFile.customFolding", &Value::Null).unwrap();
        assert_eq!(r["regions"], 1);
        let Some(Popup::Locations { title, items, .. }) = &s.popup else { panic!("{:?}", s.popup) };
        assert_eq!(title, "Custom Folding");
        assert_eq!((items[0].line, items[0].column, items[0].preview.as_str()), (1, 4, "Helpers"));
        let (path, line, column) = (items[0].path.clone(), items[0].line, items[0].column);
        s.execute("navigate.openLocation", &json!({ "path": path, "line": line, "column": column })).unwrap();
        assert_eq!(s.ws.active().unwrap().0.selections.primary().head, 14);
        // The region folds like any other range.
        let folds = s.ws.active_mut().unwrap().1.folding_ranges().to_vec();
        assert!(folds.iter().any(|f| f.start_line == 1 && f.end_line == 3));
        // Nothing to list: a status message, no popup.
        let (_d2, mut s2) = session("fn main() {}\n", "b.rs");
        assert_eq!(s2.execute("navigate.navigateInFile.customFolding", &Value::Null).unwrap()["regions"], 0);
        assert!(s2.popup.is_none());
        assert!(crate::registry().shortcuts("navigate.navigateInFile.customFolding").contains(&"Alt+Cmd+."));
    }

    #[test]
    fn typing_into_virtual_space_pads_and_keeps_the_column() {
        let (_d, mut s) = session("abcdef\nx\nabcdef\n", "c.rs");
        // Alt+drag past the end of line 1: a zero-width box at column 4 over three lines.
        s.execute(
            "editor.boxSelect",
            &json!({ "anchor": { "line": 0, "column": 4 }, "head": { "line": 2, "column": 4 } }),
        )
        .unwrap();
        assert!(current_block(&s).is_some());
        s.execute("editor.type", &json!({ "text": "Z" })).unwrap();
        assert_eq!(text(&s), "abcdZef\nx   Z\nabcdZef\n");
        s.execute("editor.type", &json!({ "text": "Y" })).unwrap();
        assert_eq!(text(&s), "abcdZYef\nx   ZY\nabcdZYef\n");
        // One undo step removes the typing and the padding of the first keystroke's group.
        s.execute("edit.undo", &Value::Null).unwrap();
        assert_eq!(text(&s), "abcdef\nx\nabcdef\n");
    }

    #[test]
    fn ordinary_boxes_type_as_before() {
        let (_d, mut s) = session("abcdef\nabcdef\n", "d.rs");
        s.execute(
            "editor.boxSelect",
            &json!({ "anchor": { "line": 0, "column": 1 }, "head": { "line": 1, "column": 2 } }),
        )
        .unwrap();
        s.execute("editor.type", &json!({ "text": "-" })).unwrap();
        assert_eq!(text(&s), "a-cdef\na-cdef\n");
    }

    #[test]
    fn inlay_hints_query_and_navigation() {
        let (dir, mut s) = session("fn main() {\n    let v = f(1);\n}\n", "e.rs");
        let target = dir.path().join("lib.rs");
        std::fs::write(&target, "// x\npub struct Vec😀Thing;\n").unwrap();
        let revision = s.ws.active().unwrap().1.doc.revision();
        let hints = vec![
            InlayHint {
                offset: 21,
                label: ": VecThing".into(),
                kind: InlayKind::Type,
                pad_left: false,
                pad_right: false,
                parts: vec![
                    InlayLabelPart { value: ": ".into(), tooltip: None, location: None },
                    InlayLabelPart {
                        value: "VecThing".into(),
                        tooltip: Some("struct VecThing".into()),
                        // UTF-16 column 11 is after "pub struct ": char column 11.
                        location: Some(InlayLocation { path: target.clone(), line: 1, utf16_column: 11 }),
                    },
                ],
                tooltip: None,
            },
            InlayHint {
                offset: 27,
                label: "x:".into(),
                kind: InlayKind::Parameter,
                pad_left: false,
                pad_right: true,
                parts: Vec::new(),
                tooltip: Some("parameter x".into()),
            },
        ];
        s.ws.active_mut().unwrap().1.inlay_hints = Some((revision, hints));
        let all = s.execute("editor.inlayHints", &json!({ "line": 1 })).unwrap();
        assert_eq!(all["hints"].as_array().unwrap().len(), 2);
        assert_eq!(all["hints"][0]["parts"][1]["tooltip"], "struct VecThing");
        assert_eq!(s.execute("editor.inlayHints", &json!({ "line": 0 })).unwrap()["hints"], json!([]));
        // A parameter hint without a location: nothing to go to.
        assert_eq!(s.execute("editor.inlayHint.navigate", &json!({ "offset": 27 })).unwrap()["navigated"], false);
        assert!(s.execute("editor.inlayHint.navigate", &json!({ "offset": 3 })).is_err());
        assert!(s.execute("editor.inlayHint.navigate", &json!({})).is_err());
        // The type part's location opens the file at its char column.
        let r = s.execute("editor.inlayHint.navigate", &json!({ "offset": 21, "part": 1 })).unwrap();
        assert_eq!(r["navigated"], true);
        let (e, d) = s.ws.active().unwrap();
        assert_eq!(d.path.as_deref(), Some(target.as_path()));
        assert_eq!(e.selections.primary().head, d.doc.line_start(1) + 11);
    }
}
