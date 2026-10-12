use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use codecraft_syntax::{FoldRange, Highlights, Language};
use codecraft_text::{Document, SelectionSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DocId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EditorId(pub u64);

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
    highlight_cache: Option<(u64, Highlights)>,
    fold_cache: Option<(u64, Vec<FoldRange>)>,
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
            highlight_cache: None,
            fold_cache: None,
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

    /// Lexical highlights, recomputed when the revision changes.
    pub fn highlights(&mut self) -> &Highlights {
        let rev = self.doc.revision();
        if self.highlight_cache.as_ref().is_none_or(|(r, _)| *r != rev) {
            let h = codecraft_syntax::highlight(self.language, &self.doc.text());
            self.highlight_cache = Some((rev, h));
        }
        match &self.highlight_cache {
            Some((_, h)) => h,
            None => empty_highlights(),
        }
    }

    pub fn folding_ranges(&mut self) -> &[FoldRange] {
        let rev = self.doc.revision();
        if self.fold_cache.as_ref().is_none_or(|(r, _)| *r != rev) {
            let f = codecraft_syntax::folding_ranges(self.language, &self.doc.text());
            self.fold_cache = Some((rev, f));
        }
        self.fold_cache.as_ref().map(|(_, f)| f.as_slice()).unwrap_or(&[])
    }

    /// Bring the highlight and fold caches up to date (then read them with the `cached_*` accessors).
    pub fn refresh_caches(&mut self) {
        let _ = self.highlights();
        let _ = self.folding_ranges();
    }

    /// Highlights as of the last [`refresh_caches`](Self::refresh_caches) (may be stale).
    pub fn cached_highlights(&self) -> &Highlights {
        self.highlight_cache.as_ref().map(|(_, h)| h).unwrap_or_else(|| empty_highlights())
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
