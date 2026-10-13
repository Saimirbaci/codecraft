use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use codecraft_syntax::{FoldRange, HighlightKind, Highlights, IncrementalHighlighter, Language};
use codecraft_text::{Assoc, ChangeSet, Document, SelectionSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DocId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EditorId(pub u64);

/// A semantic token from the language server, in char columns within one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
    /// Mutable binding (RustRover underlines these).
    pub mutable: bool,
    /// The reference does not resolve (shown like an error).
    pub unresolved: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InlayKind {
    Type,
    Parameter,
    Other,
}

/// An inline hint drawn between characters (`: i32`, `name:`), at a char offset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlayHint {
    pub offset: usize,
    pub label: String,
    pub kind: InlayKind,
    pub pad_left: bool,
    pub pad_right: bool,
}

/// A document open in the IDE (shared by the editors that show it).
#[derive(Debug)]
pub struct OpenDocument {
    pub id: DocId,
    /// `None` for untitled buffers and scratch files without a path.
    pub path: Option<PathBuf>,
    pub doc: Document,
    pub language: Language,
    pub read_only: bool,
    /// Modification time when last loaded or saved (external change detection).
    pub disk_mtime: Option<SystemTime>,
    /// LSP document version (incremented on every sync).
    pub lsp_version: i32,
    /// Document revision last sent to the language server; `None` = not opened there.
    pub lsp_synced: Option<u64>,
    /// The text the language server has (a cheap snapshot), for incremental sync: changes since
    /// `lsp_synced` are converted to LSP ranges against it.
    pub lsp_text: Option<Document>,
    /// Highlights at a revision, updated incrementally from the document's change log.
    highlight_cache: Option<(u64, IncrementalHighlighter)>,
    fold_cache: Option<(u64, Vec<FoldRange>)>,
    /// Semantic tokens per line, for the revision they were computed at.
    pub semantic: Option<(u64, Vec<Vec<SemanticSpan>>)>,
    /// Inlay hints (sorted by offset), for the revision they were computed at.
    pub inlay_hints: Option<(u64, Vec<InlayHint>)>,
}

impl OpenDocument {
    pub fn new(id: DocId, path: Option<PathBuf>, doc: Document) -> Self {
        let language = path.as_deref().map(Language::from_path).unwrap_or_default();
        Self {
            id,
            path,
            doc,
            language,
            read_only: false,
            disk_mtime: None,
            lsp_version: 0,
            lsp_synced: None,
            lsp_text: None,
            highlight_cache: None,
            fold_cache: None,
            semantic: None,
            inlay_hints: None,
        }
    }

    /// File name for the tab title ("untitled" without a path).
    pub fn title(&self) -> String {
        self.path
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".into())
    }

    /// Lexical highlights, brought up to date when the revision changes (incrementally when
    /// the document still has the changes since the cached revision).
    pub fn highlights(&mut self) -> &Highlights {
        let rev = self.doc.revision();
        if self.highlight_stale(rev) {
            let text = self.doc.text();
            self.update_highlights(rev, &text);
        }
        self.cached_highlights()
    }

    fn highlight_stale(&self, rev: u64) -> bool {
        self.highlight_cache.as_ref().is_none_or(|(r, h)| *r != rev || h.language() != self.language)
    }

    fn update_highlights(&mut self, rev: u64, text: &str) {
        let edit = match &self.highlight_cache {
            Some((r, h)) if h.language() == self.language => {
                self.doc.changes_since(*r).and_then(|sets| covering_edit(&sets, self.doc.len_chars()))
            }
            _ => None,
        };
        match (&mut self.highlight_cache, edit) {
            (Some((r, h)), Some((start, old_end, new_end))) => {
                h.update(text, start, old_end, new_end);
                *r = rev;
            }
            _ => self.highlight_cache = Some((rev, IncrementalHighlighter::new(self.language, text))),
        }
    }

    pub fn folding_ranges(&mut self) -> &[FoldRange] {
        let rev = self.doc.revision();
        if self.fold_cache.as_ref().is_none_or(|(r, _)| *r != rev) {
            let text = self.doc.text();
            if self.highlight_stale(rev) {
                self.update_highlights(rev, &text);
            }
            let f = match &self.highlight_cache {
                Some((_, h)) => h.folding_ranges(&text),
                None => codecraft_syntax::folding_ranges(self.language, &text),
            };
            self.fold_cache = Some((rev, f));
        }
        self.fold_cache.as_ref().map(|(_, f)| f.as_slice()).unwrap_or(&[])
    }

    /// Bring the highlight and fold caches up to date (then read them with the `cached_*` accessors).
    pub fn refresh_caches(&mut self) {
        let _ = self.folding_ranges();
        let _ = self.highlights();
    }

    /// Highlights as of the last [`refresh_caches`](Self::refresh_caches) (may be stale).
    pub fn cached_highlights(&self) -> &Highlights {
        self.highlight_cache.as_ref().map(|(_, h)| h.highlights()).unwrap_or_else(|| empty_highlights())
    }

    /// Fold ranges as of the last [`refresh_caches`](Self::refresh_caches) (may be stale).
    pub fn cached_folds(&self) -> &[FoldRange] {
        self.fold_cache.as_ref().map(|(_, f)| f.as_slice()).unwrap_or(&[])
    }

    /// Whether the language server has an older revision than the document.
    pub fn needs_lsp_sync(&self) -> bool {
        self.lsp_synced.is_some_and(|r| r != self.doc.revision())
    }
}

/// One edit covering everything `sets` changed, as `(start, old_end, new_end)` char offsets:
/// chars `start..old_end` of the text before the first set became `start..new_end` of the text
/// after the last (`len_after` chars long). `None` if the sets are inconsistent with that length.
fn covering_edit(sets: &[&ChangeSet], len_after: usize) -> Option<(usize, usize, usize)> {
    let delta: i128 = sets
        .iter()
        .flat_map(|cs| cs.changes())
        .map(|c| c.text.chars().count() as i128 - c.end.saturating_sub(c.start) as i128)
        .sum();
    let len_before = usize::try_from(len_after as i128 - delta).ok()?;
    let mut range: Option<(usize, usize)> = None;
    let mut len = len_before;
    for cs in sets {
        let (Some(first), Some(last)) = (cs.changes().first(), cs.changes().last()) else { continue };
        let start = first.start;
        let end = cs.map_pos(last.end, Assoc::After);
        range = Some(match range {
            Some((s, e)) => (cs.map_pos(s, Assoc::Before).min(start), cs.map_pos(e, Assoc::After).max(end)),
            None => (start, end),
        });
        len = cs.len_after(len);
    }
    if len != len_after {
        return None;
    }
    let (start, new_end) = range.unwrap_or((0, 0));
    let old_end = (new_end as i128 + len_before as i128 - len_after as i128).max(start as i128);
    Some((start, usize::try_from(old_end).ok()?, new_end))
}

fn empty_highlights() -> &'static Highlights {
    static EMPTY: std::sync::OnceLock<Highlights> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Highlights::default)
}

/// One editor tab: a view of a document with its own carets, scroll and folds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Editor {
    pub id: EditorId,
    pub doc: DocId,
    pub selections: SelectionSet,
    /// First visible line (fractional while scrolling).
    pub scroll_line: f32,
    /// Horizontal scroll in pixels.
    pub scroll_x: f32,
    /// Start lines of collapsed fold regions.
    pub folded: BTreeSet<usize>,
    pub pinned: bool,
    /// Ask the view to scroll the primary caret into view on the next frame.
    pub reveal_caret: bool,
}

impl Editor {
    pub fn new(id: EditorId, doc: DocId) -> Self {
        Self {
            id,
            doc,
            selections: SelectionSet::default(),
            scroll_line: 0.0,
            scroll_x: 0.0,
            folded: BTreeSet::new(),
            pinned: false,
            reveal_caret: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_text::{Change, Edit, EditGroup};

    fn apply(od: &mut OpenDocument, changes: Vec<Change>) {
        let cs = ChangeSet::new(changes).unwrap();
        let sel = SelectionSet::caret(0);
        od.doc.apply(&Edit { changes: cs, selections: sel.clone() }, &sel, EditGroup::Standalone).unwrap();
    }

    fn assert_fresh(od: &mut OpenDocument) {
        od.refresh_caches();
        let text = od.doc.text();
        assert_eq!(od.cached_highlights(), &codecraft_syntax::highlight(od.language, &text), "{text:?}");
        assert_eq!(od.cached_folds(), codecraft_syntax::folding_ranges(od.language, &text).as_slice());
    }

    #[test]
    fn caches_follow_batched_multi_change_edits_undo_and_redo() {
        let src = "/// Doc\nfn main() {\n    let s = r#\"raw\n\"#;\n    /* block\n */ println!(\"{s}\");\n}\n";
        for path in ["/p/a.rs", "/p/a.py", "/p/a.md", "/p/a.txt"] {
            let mut od = OpenDocument::new(DocId(1), Some(path.into()), Document::from_text(src));
            assert_fresh(&mut od);
            // Several revisions between refreshes, each with several changes.
            apply(&mut od, vec![Change::insert(0, "/* "), Change::new(12, 14, "fx"), Change::insert(30, "\n\"")]);
            apply(&mut od, vec![Change::delete(5, 9), Change::insert(20, "#[attr(\n")]);
            assert_fresh(&mut od);
            od.doc.undo();
            assert_fresh(&mut od);
            od.doc.undo();
            od.doc.redo();
            od.doc.set_line_ending(codecraft_text::LineEnding::CrLf);
            assert_fresh(&mut od);
            od.doc.replace_all("fn x() {}", &SelectionSet::caret(0)).unwrap();
            assert_fresh(&mut od);
            // A language change re-lexes from scratch.
            od.language = Language::Rust;
            assert_fresh(&mut od);
        }
    }

    #[test]
    fn covering_edit_spans_all_changes() {
        let a = ChangeSet::new(vec![Change::insert(2, "xy"), Change::delete(8, 10)]).unwrap();
        let b = ChangeSet::new(vec![Change::new(0, 1, "Q")]).unwrap();
        // "0123456789ab" -> "01xy234567ab" -> "Q1xy234567ab"
        assert_eq!(covering_edit(&[&a, &b], 12), Some((0, 10, 10)));
        assert_eq!(covering_edit(&[&a], 12), Some((2, 10, 10)));
        assert_eq!(covering_edit(&[], 5), Some((0, 0, 0)));
        // Lengths that cannot be right are rejected.
        let ins = ChangeSet::single(0, 0, "abc");
        assert_eq!(covering_edit(&[&ins], 1), None);
    }
}
