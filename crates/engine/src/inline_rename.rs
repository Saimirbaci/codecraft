//! In-place rename (Shift+F6, RustRover's default): when `prepareRename` gives the symbol's
//! range, the name is edited right in the editor. Its occurrences in the file
//! (`documentHighlight`) are selected and framed, so typing changes them all (a one-variable live
//! template session does the mirroring); Enter commits through LSP rename (multi-file results open
//! the refactoring preview), Escape restores the original name. Clicking elsewhere or Tab commits.
//!
//! Commit and cancel first undo the local edits, so the server renames from the original text
//! and the rename is a single undo step. Agents can use `editor.inlineRename.commit {newName}` or
//! keep calling `refactor.rename {newName}`, which never starts an in-place session.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_lsp::ResponseError;
use codecraft_lsp::lsp_types as lt;
use codecraft_text::{Selection, SelectionSet};
use codecraft_workspace::{DocId, EditorId};

use crate::Session;
use crate::commands::{CmdResult, ok};
use crate::completion::LookupPending;
use crate::convert;
use crate::editing::TemplateSession;
use crate::lsp::Pending;

/// Most undo steps a commit/cancel takes back before restoring the text wholesale.
const MAX_UNDO: usize = 500;

/// An in-place rename in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineRename {
    pub editor: EditorId,
    pub doc: DocId,
    /// The symbol's name before renaming.
    pub original: String,
    /// Char offset of the renamed occurrence in the original text.
    pub origin: usize,
    /// Which occurrence (in the session's sorted ranges) is the one renamed.
    pub primary: usize,
    #[serde(skip)]
    original_text: String,
    /// Edits made while renaming (an upper bound for the undo steps to take back).
    #[serde(skip)]
    edits: usize,
}

/// Waiting for the occurrences before starting.
pub(crate) struct HighlightsPending {
    editor: EditorId,
    revision: u64,
    range: Range<usize>,
    name: String,
}

pub(crate) fn is_active(s: &Session) -> Result<(), String> {
    s.completion.rename.as_ref().map(|_| ()).ok_or_else(|| "no in-place rename is in progress".into())
}

/// `prepareRename` returned `range` for `name`: start renaming in place (after asking for the
/// occurrences). Returns false when the rename popup should be used instead.
pub(crate) fn try_start(s: &mut Session, editor: EditorId, range: &lt::Range, name: &str) -> bool {
    if !s.ws.settings.rename_in_place || s.ws.active_editor() != Some(editor) {
        return false;
    }
    let Some(d) = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)) else { return false };
    let r = convert::from_lsp_range(&d.doc, range);
    if r.is_empty() || d.doc.slice(r.clone()) != name {
        return false;
    }
    let (revision, path) = (d.doc.revision(), d.path.clone());
    let pending = HighlightsPending { editor, revision, range: r.clone(), name: name.to_string() };
    let sent = path.ok_or(()).and_then(|p| {
        let pos = range.start;
        s.lsp_send(
            |c| c.document_highlight(&p, pos.line, pos.character),
            Pending::Lookup(LookupPending::RenameHighlights(pending)),
        )
        .map_err(|_| ())
    });
    if sent.is_err() {
        begin(s, editor, revision, r, name, Vec::new());
    }
    true
}

pub(crate) fn on_highlights(s: &mut Session, p: HighlightsPending, result: Result<Value, ResponseError>) {
    let Some(d) = s.ws.editor(p.editor).and_then(|e| s.ws.doc(e.doc)) else { return };
    let highlights: Vec<lt::DocumentHighlight> =
        result.ok().and_then(|v| serde_json::from_value(v).ok()).flatten().unwrap_or_default();
    let ranges: Vec<Range<usize>> =
        highlights.iter().take(10_000).map(|h| convert::from_lsp_range(&d.doc, &h.range)).collect();
    begin(s, p.editor, p.revision, p.range, &p.name, ranges);
}

/// Select the occurrences and start the session (only if nothing changed since `revision`).
fn begin(
    s: &mut Session,
    editor: EditorId,
    revision: u64,
    primary: Range<usize>,
    name: &str,
    others: Vec<Range<usize>>,
) {
    let Some((eid, did)) = s.active_ids() else { return };
    let Some(d) = s.ws.doc(did) else { return };
    if eid != editor || d.doc.revision() != revision {
        s.status = "Rename cancelled: the file changed".into();
        return;
    }
    let mut ranges: Vec<Range<usize>> = others
        .into_iter()
        .filter(|r| r.end <= d.doc.len_chars() && !r.is_empty() && d.doc.slice(r.clone()) == name)
        .collect();
    ranges.push(primary.clone());
    ranges.sort_by_key(|r| (r.start, r.end));
    ranges.dedup();
    // Overlapping ranges would make the mirrored edits ambiguous: keep the first of each.
    let mut clean: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if clean.last().is_none_or(|l| r.start >= l.end) {
            clean.push(r);
        }
    }
    let primary_idx = clean.iter().position(|r| *r == primary).unwrap_or(0);
    let original_text = d.doc.text();
    let sels = SelectionSet::from_vec(clean.iter().map(|r| Selection::new(r.start, r.end)).collect(), primary_idx);
    s.popup = None;
    s.select_active(|_, _| sels);
    let count = clean.len();
    s.editing.session =
        Some(TemplateSession { editor, revision, vars: vec![clean], current: 0, end: None, choices: Vec::new() });
    s.completion.rename = Some(InlineRename {
        editor,
        doc: did,
        original: name.to_string(),
        origin: primary.start,
        primary: primary_idx,
        original_text,
        edits: 0,
    });
    s.status = format!("Rename '{name}' ({count} occurrence(s) in this file): Enter to refactor, Escape to cancel");
}

/// Hook from `editing::after_edit`: count the edits made while renaming.
pub(crate) fn on_edit(s: &mut Session, editor: EditorId) {
    if let Some(r) = &mut s.completion.rename
        && r.editor == editor
    {
        r.edits = r.edits.saturating_add(1);
    }
}

/// The name currently typed at the renamed occurrence.
fn typed_name(s: &Session, r: &InlineRename) -> Option<String> {
    let t = s.editing.session.as_ref()?;
    let range = t.vars.first()?.get(r.primary)?.clone();
    let d = s.ws.doc(r.doc)?;
    (range.end <= d.doc.len_chars()).then(|| d.doc.slice(range))
}

/// Take the local edits back (the server renames from the original text).
fn revert(s: &mut Session, r: &InlineRename) {
    if s.editing.session.as_ref().is_some_and(|t| t.editor == r.editor) {
        s.editing.session = None;
    }
    let Some(od) = s.ws.doc_mut(r.doc) else { return };
    let mut steps = 0usize;
    while od.doc.text() != r.original_text && steps < r.edits.min(MAX_UNDO) && od.doc.can_undo() {
        if od.doc.undo().is_none() {
            break;
        }
        steps += 1;
    }
    if od.doc.text() != r.original_text {
        let _ = od.doc.replace_all(&r.original_text, &SelectionSet::caret(r.origin));
    }
    let len = od.doc.len_chars();
    if let Some(e) = s.ws.editor_mut(r.editor) {
        e.selections = SelectionSet::caret(r.origin.min(len));
        e.reveal_caret = true;
    }
    s.after_text_change(r.editor);
}

/// Enter: rename through the language server (`new_name`, else the typed name).
pub(crate) fn commit(s: &mut Session, new_name: Option<&str>) -> CmdResult {
    let Some(r) = s.completion.rename.take() else { return ok() };
    let name = new_name.map(str::to_string).or_else(|| typed_name(s, &r)).unwrap_or_default();
    let name = name.trim().to_string();
    revert(s, &r);
    if name.is_empty() || name == r.original {
        s.status = "Rename: the name did not change".into();
        return Ok(json!({ "renamed": false }));
    }
    if s.ws.active_editor() != Some(r.editor) {
        s.status = "Rename cancelled: the editor changed".into();
        return Ok(json!({ "renamed": false }));
    }
    crate::refactor::rename(s, Some(&name), false)?;
    Ok(json!({ "renamed": true, "from": r.original, "to": name }))
}

/// Escape: restore the original name.
pub(crate) fn cancel(s: &mut Session) -> CmdResult {
    let Some(r) = s.completion.rename.take() else { return ok() };
    revert(s, &r);
    s.status = "Rename cancelled".into();
    ok()
}

/// End a rename whose session ended: Tab or a click outside commits; an undo past its start drops it.
pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(r) = s.completion.rename.clone() else { return false };
    let revision = s.ws.doc(r.doc).map(|d| d.doc.revision());
    let session = s.editing.session.as_ref().filter(|t| t.editor == r.editor && t.vars.len() == 1);
    let result = match (session, revision) {
        (_, None) => {
            s.completion.rename = None;
            return true;
        }
        // Tab (or another snippet) ended the session.
        (None, _) => commit(s, None),
        (Some(t), Some(rev)) if t.revision != rev => {
            // Undo/redo moved the text under the session: let it go.
            s.editing.session = None;
            s.completion.rename = None;
            return true;
        }
        (Some(t), _) => {
            let inside = s.ws.active().is_some_and(|(e, _)| {
                let caret = e.selections.primary().head;
                e.id == r.editor && t.vars.iter().flatten().any(|x| x.start <= caret && caret <= x.end)
            });
            if inside {
                return false;
            }
            commit(s, None)
        }
    };
    if let Err(e) = result {
        s.status = e.to_string();
    }
    true
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init, clippy::reversed_empty_ranges)]
mod tests {
    use super::*;
    use codecraft_syntax::Language;

    const SRC: &str = "fn f() {\n    let count = 1;\n    let y = count + count;\n}\n";

    fn session() -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        let did = s.ws.add_document(None, SRC);
        if let Some(d) = s.ws.doc_mut(did) {
            d.language = Language::Rust;
        }
        s.ws.open_editor(did).unwrap();
        s
    }

    fn text(s: &Session) -> String {
        s.ws.active().unwrap().1.doc.text()
    }

    fn occurrences() -> Vec<Range<usize>> {
        SRC.match_indices("count").map(|(b, _)| b..b + 5).collect()
    }

    fn start(s: &mut Session) {
        let editor = s.ws.active().unwrap().0.id;
        let revision = s.ws.active().unwrap().1.doc.revision();
        let occ = occurrences();
        begin(s, editor, revision, occ[1].clone(), "count", occ.clone());
    }

    #[test]
    fn typing_mirrors_and_escape_restores() {
        let mut s = session();
        start(&mut s);
        let r = s.completion.rename.clone().unwrap();
        assert_eq!((r.original.as_str(), r.primary), ("count", 1));
        assert_eq!(s.ws.active().unwrap().0.selections.len(), 3);
        s.execute("editor.type", &json!({ "text": "total" })).unwrap();
        assert_eq!(text(&s), SRC.replace("count", "total"));
        s.execute("editor.backspace", &Value::Null).unwrap();
        assert_eq!(text(&s), SRC.replace("count", "tota"));
        assert!(s.completion.rename.is_some());
        s.execute("editor.escape", &Value::Null).unwrap();
        assert_eq!(text(&s), SRC);
        assert!(s.completion.rename.is_none() && s.editing.session.is_none());
        // The caret is back on the renamed occurrence; nothing to redo into the half-renamed state matters.
        assert_eq!(s.ws.active().unwrap().0.selections.primary().head, occurrences()[1].start);
    }

    #[test]
    fn enter_commits_from_the_original_text() {
        let mut s = session();
        start(&mut s);
        s.execute("editor.type", &json!({ "text": "n" })).unwrap();
        // No language server here: the text is restored and the rename request fails cleanly.
        let r = s.execute("editor.enter", &Value::Null);
        assert!(r.is_err(), "{r:?}");
        assert_eq!(text(&s), SRC);
        assert!(s.completion.rename.is_none());
        // Unchanged name: nothing to do.
        start(&mut s);
        let r = s.execute("editor.inlineRename.commit", &Value::Null).unwrap();
        assert_eq!(r["renamed"], false);
        assert!(s.execute("editor.inlineRename.cancel", &Value::Null).is_err(), "disabled without a rename");
    }

    #[test]
    fn clicking_outside_commits_and_undo_drops() {
        let mut s = session();
        start(&mut s);
        s.execute("editor.type", &json!({ "text": "c2" })).unwrap();
        s.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
        s.pump();
        assert!(s.completion.rename.is_none());
        assert_eq!(text(&s), SRC, "reverted before asking the server");
        // Undo while renaming: the rename just ends.
        start(&mut s);
        s.execute("editor.type", &json!({ "text": "z" })).unwrap();
        s.execute("edit.undo", &Value::Null).unwrap();
        s.pump();
        assert!(s.completion.rename.is_none());
        assert_eq!(text(&s), SRC);
    }

    #[test]
    fn stale_or_foreign_starts_are_refused() {
        let mut s = session();
        let editor = s.ws.active().unwrap().0.id;
        let occ = occurrences();
        begin(&mut s, editor, 999, occ[0].clone(), "count", Vec::new());
        assert!(s.completion.rename.is_none());
        begin(&mut s, EditorId(4242), 0, occ[0].clone(), "count", Vec::new());
        assert!(s.completion.rename.is_none());
        // Hostile highlight ranges are filtered.
        let revision = s.ws.active().unwrap().1.doc.revision();
        begin(
            &mut s,
            editor,
            revision,
            occ[0].clone(),
            "count",
            vec![0..9999, 3..1, 0..0, occ[0].start..occ[0].start + 2],
        );
        let t = s.editing.session.as_ref().unwrap();
        assert_eq!(t.vars[0], vec![occ[0].clone()]);
        let r = lt::Range::default();
        s.ws.settings.rename_in_place = false;
        assert!(!try_start(&mut s, editor, &r, "count"));
    }
}
