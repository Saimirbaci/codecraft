//! Code › Folding beyond Expand/Collapse (All) and Toggle Folding (`code_cmds.rs`): Expand and
//! Collapse Recursively, Expand to Level and Expand All to Level 1–5, Expand/Collapse Doc
//! Comments, Fold Selection / Remove Region and Fold Code Block.
//!
//! Folds are line regions identified by their start line (`Editor::folded`); the regions come
//! from the document (`OpenDocument::folding_ranges`, which includes Fold Selection regions).

use serde_json::{Value, json};

use codecraft_syntax::{FoldKind, FoldRange};
use codecraft_workspace::{DocId, EditorId};

use crate::Session;
use crate::commands::{CmdResult, CommandSpec, Context, has_editor, ok, spec};

/// The active editor, its document's fold regions and the caret line.
fn regions(s: &mut Session) -> Option<(EditorId, DocId, Vec<FoldRange>, usize)> {
    let (eid, did) = s.active_ids()?;
    let caret_line = s.ws.active().map(|(e, d)| d.doc.char_to_line(e.selections.primary().head))?;
    let ranges = s.ws.doc_mut(did).map(|d| d.folding_ranges().to_vec())?;
    Some((eid, did, ranges, caret_line))
}

fn contains(outer: &FoldRange, inner: &FoldRange) -> bool {
    outer.start_line <= inner.start_line && inner.end_line <= outer.end_line
}

/// Nesting depth of each region (0 = top level), for regions sorted by start line, outermost
/// first.
fn depths(ranges: &[FoldRange]) -> Vec<usize> {
    let mut stack: Vec<FoldRange> = Vec::new();
    ranges
        .iter()
        .map(|r| {
            while stack.last().is_some_and(|top| !contains(top, r)) {
                stack.pop();
            }
            let depth = stack.len();
            stack.push(*r);
            depth
        })
        .collect()
}

/// The innermost region containing `line` (optionally of one kind).
fn innermost(ranges: &[FoldRange], line: usize, kind: Option<FoldKind>) -> Option<FoldRange> {
    ranges
        .iter()
        .enumerate()
        .filter(|(_, r)| r.start_line <= line && line <= r.end_line && kind.is_none_or(|k| r.kind == k))
        // Later entries with the same start are nested deeper.
        .max_by_key(|(i, r)| (r.start_line, *i))
        .map(|(_, r)| *r)
}

/// Collapse (`true`) or expand the regions `pick` selects; returns how many changed state.
fn set_folded(
    s: &mut Session,
    eid: EditorId,
    ranges: &[FoldRange],
    pick: impl Fn(usize, &FoldRange) -> Option<bool>,
) -> usize {
    let Some(editor) = s.ws.editor_mut(eid) else { return 0 };
    let mut changed = 0;
    for (i, r) in ranges.iter().enumerate() {
        let Some(collapse) = pick(i, r) else { continue };
        let did = if collapse { editor.folded.insert(r.start_line) } else { editor.folded.remove(&r.start_line) };
        changed += usize::from(did);
    }
    changed
}

fn no_region(s: &mut Session) -> CmdResult {
    s.status = "No folding region at the caret".into();
    Ok(json!({ "changed": 0 }))
}

/// Expand or collapse the region at the caret and every region inside it.
fn recursively(s: &mut Session, collapse: bool) -> CmdResult {
    let Some((eid, _, ranges, line)) = regions(s) else { return ok() };
    let Some(target) = innermost(&ranges, line, None) else { return no_region(s) };
    let changed = set_folded(s, eid, &ranges, |_, r| contains(&target, r).then_some(collapse));
    Ok(json!({ "changed": changed }))
}

/// Expand to Level `level` (`all`: the whole file, else the region at the caret): regions
/// nested less deeply than `level` are expanded, deeper ones collapsed.
fn to_level(s: &mut Session, level: usize, all: bool) -> CmdResult {
    let Some((eid, _, ranges, line)) = regions(s) else { return ok() };
    let depth = depths(&ranges);
    let changed = if all {
        set_folded(s, eid, &ranges, |i, _| Some(depth.get(i).copied().unwrap_or(0) >= level))
    } else {
        let Some(target) = innermost(&ranges, line, None) else { return no_region(s) };
        let base = ranges.iter().position(|r| *r == target).and_then(|i| depth.get(i).copied()).unwrap_or(0);
        set_folded(s, eid, &ranges, |i, r| {
            contains(&target, r).then(|| depth.get(i).copied().unwrap_or(0).saturating_sub(base) >= level)
        })
    };
    Ok(json!({ "changed": changed }))
}

/// Documentation comments: `///`, `//!`, `/** */`, `/*! */` and Python docstrings.
fn is_doc_comment(s: &Session, did: DocId, r: &FoldRange) -> bool {
    if r.kind != FoldKind::Comment {
        return false;
    }
    let first = s.ws.doc(did).and_then(|d| d.doc.line_text(r.start_line)).unwrap_or_default();
    let t = first.trim_start();
    ["///", "//!", "/**", "/*!", "\"\"\"", "'''"].iter().any(|p| t.starts_with(p)) && !t.starts_with("////")
}

fn doc_comments(s: &mut Session, collapse: bool) -> CmdResult {
    let Some((eid, did, ranges, _)) = regions(s) else { return ok() };
    let docs: Vec<bool> = ranges.iter().map(|r| is_doc_comment(s, did, r)).collect();
    let changed = set_folded(s, eid, &ranges, |i, _| docs.get(i).copied().unwrap_or(false).then_some(collapse));
    Ok(json!({ "changed": changed }))
}

/// Fold Selection / Remove Region: a selection over several lines becomes a folded region; with
/// no selection, the Fold Selection region at the caret is removed.
fn fold_selection(s: &mut Session, _: &Value) -> CmdResult {
    let Some((eid, did)) = s.active_ids() else { return ok() };
    let Some((sel, a, b)) = s.ws.active().map(|(e, d)| {
        let sel = e.selections.primary();
        let a = d.doc.char_to_line(sel.start());
        let mut b = d.doc.char_to_line(sel.end());
        // A selection ending at a line start does not include that line.
        if b > a && d.doc.line_start(b) == sel.end() {
            b -= 1;
        }
        (sel, a, b)
    }) else {
        return ok();
    };
    let Some(d) = s.ws.doc_mut(did) else { return ok() };
    let mut manual = d.manual_folds().to_vec();
    if !sel.is_empty() {
        if b <= a {
            s.status = "Select several lines to fold them".into();
            return Ok(json!({ "folded": false }));
        }
        let region = FoldRange { start_line: a, end_line: b, kind: FoldKind::Region };
        if !manual.contains(&region) {
            manual.push(region);
            manual.sort_by_key(|r| (r.start_line, std::cmp::Reverse(r.end_line)));
        }
        d.set_manual_folds(manual);
        if let Some(e) = s.ws.editor_mut(eid) {
            e.folded.insert(a);
        }
        s.select_active(|_, _| codecraft_text::SelectionSet::caret(sel.start()));
        return Ok(json!({ "folded": true, "startLine": a, "endLine": b }));
    }
    let line = a;
    let Some(region) = innermost(&manual, line, None) else {
        s.status = "No Fold Selection region at the caret".into();
        return Ok(json!({ "removed": false }));
    };
    manual.retain(|r| *r != region);
    d.set_manual_folds(manual);
    if let Some(e) = s.ws.editor_mut(eid) {
        e.folded.remove(&region.start_line);
    }
    Ok(json!({ "removed": true, "startLine": region.start_line }))
}

/// Fold Code Block: collapse the innermost `{ … }` block around the caret.
fn fold_code_block(s: &mut Session, _: &Value) -> CmdResult {
    let Some((eid, _, ranges, line)) = regions(s) else { return ok() };
    let Some(block) = innermost(&ranges, line, Some(FoldKind::Block)) else {
        s.status = "No code block at the caret".into();
        return Ok(json!({ "folded": false }));
    };
    if let Some(e) = s.ws.editor_mut(eid) {
        e.folded.insert(block.start_line);
    }
    Ok(json!({ "folded": true, "startLine": block.start_line }))
}

macro_rules! level {
    ($id:literal, $label:literal, $n:literal, $all:literal) => {
        spec($id, $label, None, "", Context::Editor, has_editor, |s, _| to_level(s, $n, $all))
    };
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // `Alt+Cmd++` in the menu: `=` is the key that types `+` without Shift (as Expand's `Cmd+=`).
        spec(
            "code.folding.expandRecursively",
            "Expand Recursively",
            Some("Alt+Cmd+="),
            "",
            Context::Editor,
            has_editor,
            |s, _| recursively(s, false),
        ),
        spec(
            "code.folding.collapseRecursively",
            "Collapse Recursively",
            Some("Alt+Cmd+-"),
            "",
            Context::Editor,
            has_editor,
            |s, _| recursively(s, true),
        ),
        level!("code.folding.expandToLevel.1", "1", 1, false),
        level!("code.folding.expandToLevel.2", "2", 2, false),
        level!("code.folding.expandToLevel.3", "3", 3, false),
        level!("code.folding.expandToLevel.4", "4", 4, false),
        level!("code.folding.expandToLevel.5", "5", 5, false),
        level!("code.folding.expandAllToLevel.1", "1", 1, true),
        level!("code.folding.expandAllToLevel.2", "2", 2, true),
        level!("code.folding.expandAllToLevel.3", "3", 3, true),
        level!("code.folding.expandAllToLevel.4", "4", 4, true),
        level!("code.folding.expandAllToLevel.5", "5", 5, true),
        spec("code.folding.expandDocComments", "Expand Doc Comments", None, "", Context::Editor, has_editor, |s, _| {
            doc_comments(s, false)
        }),
        spec(
            "code.folding.collapseDocComments",
            "Collapse Doc Comments",
            None,
            "",
            Context::Editor,
            has_editor,
            |s, _| doc_comments(s, true),
        ),
        spec(
            "code.folding.foldSelectionRemoveRegion",
            "Fold Selection / Remove Region",
            None,
            "Folds the selected lines into a region; without a selection, removes the Fold Selection region at the caret",
            Context::Editor,
            has_editor,
            fold_selection,
        ),
        spec("code.folding.foldCodeBlock", "Fold Code Block", None, "", Context::Editor, has_editor, fold_code_block),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_syntax::Language;

    // Regions: 0 `mod a` 0–12 › 2 `fn f` 2–10 › 4 `if` 4–8 › 5 `loop` 5–7; doc comment 13–14; 15 `fn g` 15–17.
    const SRC: &str = "mod a {\n    // x\n    fn f() {\n        let a = 1;\n        if a > 0 {\n            loop {\n                break;\n            }\n        }\n        let b = 2;\n    }\n    // y\n}\n/// Docs\n/// more\nfn g() {\n    1;\n}\n";

    fn session() -> Session {
        let mut s = Session::new();
        let d = s.ws.add_document(None, SRC);
        if let Some(od) = s.ws.doc_mut(d) {
            od.language = Language::Rust;
        }
        s.ws.open_editor(d);
        s
    }

    fn folded(s: &Session) -> Vec<usize> {
        s.ws.active().unwrap().0.folded.iter().copied().collect()
    }

    fn caret_line(s: &mut Session, line: usize) {
        let off = s.ws.active().unwrap().1.doc.line_start(line) + 4;
        s.execute("editor.setSelection", &json!({ "anchor": off })).unwrap();
    }

    #[test]
    fn regions_and_depths() {
        let mut s = session();
        let (_, _, ranges, _) = regions(&mut s).unwrap();
        let starts: Vec<(usize, usize)> = ranges.iter().map(|r| (r.start_line, r.end_line)).collect();
        for want in [(0, 12), (2, 10), (4, 8), (5, 7), (13, 14), (15, 17)] {
            assert!(starts.contains(&want), "{want:?} not in {starts:?}");
        }
        let d = depths(&ranges);
        let depth_of = |l: usize| ranges.iter().position(|r| r.start_line == l).map(|i| d[i]);
        assert_eq!(
            (depth_of(0), depth_of(2), depth_of(4), depth_of(5), depth_of(15)),
            (Some(0), Some(1), Some(2), Some(3), Some(0))
        );
    }

    #[test]
    fn recursively_folds_the_region_at_the_caret_and_its_children() {
        let mut s = session();
        caret_line(&mut s, 3);
        s.execute("code.folding.collapseRecursively", &Value::Null).unwrap();
        assert_eq!(folded(&s), [2, 4, 5]);
        s.execute("code.folding.expandRecursively", &Value::Null).unwrap();
        assert!(folded(&s).is_empty());
        caret_line(&mut s, 16);
        s.execute("code.folding.collapseRecursively", &Value::Null).unwrap();
        assert_eq!(folded(&s), [15]);
    }

    #[test]
    fn expand_all_to_level_and_expand_to_level() {
        let mut s = session();
        s.execute("code.folding.expandAllToLevel.1", &Value::Null).unwrap();
        // Top level open; everything nested collapsed.
        let f = folded(&s);
        assert!(!f.contains(&0) && !f.contains(&15) && f.contains(&2) && f.contains(&4), "{f:?}");
        s.execute("code.folding.expandAllToLevel.2", &Value::Null).unwrap();
        let f = folded(&s);
        assert!(!f.contains(&2) && f.contains(&4) && f.contains(&5), "{f:?}");
        // At the caret: `fn f` to level 1 opens it and collapses its children only.
        s.execute("code.folding.expandAllToLevel.5", &Value::Null).unwrap();
        assert!(folded(&s).is_empty());
        caret_line(&mut s, 3);
        s.execute("code.folding.expandToLevel.1", &Value::Null).unwrap();
        assert_eq!(folded(&s), [4, 5]);
        s.execute("code.folding.expandToLevel.2", &Value::Null).unwrap();
        assert_eq!(folded(&s), [5]);
    }

    #[test]
    fn doc_comments_fold_and_unfold() {
        let mut s = session();
        s.execute("code.folding.collapseDocComments", &Value::Null).unwrap();
        assert_eq!(folded(&s), [13]);
        s.execute("code.folding.expandDocComments", &Value::Null).unwrap();
        assert!(folded(&s).is_empty());
    }

    #[test]
    fn toggle_folding_and_fold_code_block() {
        let mut s = session();
        caret_line(&mut s, 6);
        s.execute("code.folding.foldCodeBlock", &Value::Null).unwrap();
        assert_eq!(folded(&s), [5]);
        caret_line(&mut s, 3);
        s.execute("code.folding.toggleFolding", &Value::Null).unwrap();
        assert_eq!(folded(&s), [2, 5]);
        s.execute("code.folding.toggleFolding", &Value::Null).unwrap();
        assert_eq!(folded(&s), [5]);
        s.execute("code.folding.toggleFolding", &json!({ "line": 15 })).unwrap();
        assert_eq!(folded(&s), [5, 15]);
        // The old id still works.
        s.execute("code.folding.toggle", &json!({ "line": 15 })).unwrap();
        assert_eq!(folded(&s), [5]);
    }

    #[test]
    fn fold_selection_creates_and_removes_a_region() {
        let mut s = session();
        let doc = s.ws.active().unwrap().1.doc.clone();
        let (a, b) = (doc.line_start(9), doc.line_start(12));
        s.execute("editor.setSelection", &json!({ "anchor": a, "head": b })).unwrap();
        let v = s.execute("code.folding.foldSelectionRemoveRegion", &Value::Null).unwrap();
        assert_eq!((v["startLine"].as_u64(), v["endLine"].as_u64()), (Some(9), Some(11)));
        assert_eq!(folded(&s), [9]);
        let d = s.ws.active().unwrap().1;
        assert!(d.manual_folds().iter().any(|r| (r.start_line, r.end_line) == (9, 11)));
        // No selection on the region: removed.
        caret_line(&mut s, 9);
        let v = s.execute("code.folding.foldSelectionRemoveRegion", &Value::Null).unwrap();
        assert_eq!(v["removed"], true);
        assert!(folded(&s).is_empty());
        assert!(s.ws.active().unwrap().1.manual_folds().is_empty());
        // One line is not foldable; nothing to remove is a message.
        s.execute("editor.setSelection", &json!({ "anchor": a, "head": a + 3 })).unwrap();
        assert_eq!(s.execute("code.folding.foldSelectionRemoveRegion", &Value::Null).unwrap()["folded"], false);
        caret_line(&mut s, 3);
        assert_eq!(s.execute("code.folding.foldSelectionRemoveRegion", &Value::Null).unwrap()["removed"], false);
    }

    #[test]
    fn nothing_foldable_is_not_an_error() {
        let mut s = Session::new();
        let d = s.ws.add_document(None, "plain");
        s.ws.open_editor(d);
        for id in ["code.folding.expandRecursively", "code.folding.expandToLevel.3", "code.folding.foldCodeBlock"] {
            s.execute(id, &Value::Null).unwrap();
        }
        assert!(folded(&s).is_empty());
    }
}
