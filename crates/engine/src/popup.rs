//! Popups and the find bar: engine-side state, so agents can read and drive them like the UI.

use std::ops::Range;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use codecraft_text::search::SearchQuery;
use codecraft_workspace::EditorId;

/// A symbol in Go to Symbol / Go to Class results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolItem {
    pub name: String,
    /// "fn", "struct", "enum", "trait", "mod", …
    pub kind: String,
    /// Container (module / impl), if known.
    pub container: Option<String>,
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

/// An entry of the File Structure popup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructureItem {
    pub name: String,
    pub kind: String,
    pub detail: Option<String>,
    pub depth: usize,
    pub line: usize,
    pub column: usize,
}

/// A location choice (several declarations / implementations).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationItem {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    /// The line's text, trimmed.
    pub preview: String,
}

/// One completion proposal (already converted out of LSP types).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionItem {
    pub label: String,
    /// Shown greyed after the label (signature / type).
    pub detail: Option<String>,
    /// "fn", "method", "struct", "field", "keyword", "snippet", …
    pub kind: String,
    /// Text matched against what the user typed.
    pub filter_text: String,
    pub sort_text: String,
    /// Replacement for `[replace_start, caret)` when accepted.
    pub insert_text: String,
    /// Caret position within `insert_text` after accepting (from the snippet's `$0`).
    #[serde(default)]
    pub cursor: Option<usize>,
    /// Char offset where the replaced range starts (from the item's text edit, or the word start).
    pub replace_start: Option<usize>,
    /// Extra edits (auto-import), char offsets in the document at request time.
    pub additional_edits: Vec<(Range<usize>, String)>,
    pub documentation: Option<String>,
    pub deprecated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionPopup {
    pub editor: EditorId,
    /// Start of the identifier being completed (char offset).
    pub anchor: usize,
    pub items: Vec<CompletionItem>,
    /// Indices into `items` matching the current prefix, best first.
    pub visible: Vec<usize>,
    pub selected: usize,
    /// Waiting for the language server.
    pub pending: bool,
}

/// A code action (Alt+Enter) or other choice backed by the language server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub title: String,
    pub kind: Option<String>,
    /// The raw LSP CodeAction or Command, resolved and applied on accept.
    pub payload: serde_json::Value,
}

/// Modal popups. At most one is open.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Popup {
    /// Navigate › Line:Column… (Cmd+L).
    GotoLine {
        input: String,
    },
    /// Navigate › File… (Shift+Cmd+O).
    GotoFile {
        query: String,
        selected: usize,
    },
    /// Navigate › Symbol… (Alt+Cmd+O) and Type… (Cmd+O, `types_only`).
    GotoSymbol {
        query: String,
        selected: usize,
        types_only: bool,
        items: Vec<SymbolItem>,
        pending: bool,
    },
    /// Search Everywhere (double Shift): files, symbols and actions.
    SearchEverywhere {
        query: String,
        selected: usize,
        symbols: Vec<SymbolItem>,
        pending: bool,
    },
    /// Help › Find Action… (Shift+Cmd+A).
    FindAction {
        query: String,
        selected: usize,
    },
    /// View › Recent Files (Cmd+E).
    RecentFiles {
        query: String,
        selected: usize,
    },
    /// View › Recent Locations (Shift+Cmd+E).
    RecentLocations {
        selected: usize,
    },
    /// Navigate › File Structure (Cmd+F12).
    FileStructure {
        query: String,
        selected: usize,
        items: Vec<StructureItem>,
        pending: bool,
    },
    /// Edit › Find › Find in Files… / Replace in Files….
    FindInFiles {
        query: SearchQuery,
        mask: String,
        replace: Option<String>,
    },
    /// Refactor › Rename… (Shift+F6).
    Rename {
        editor: EditorId,
        offset: usize,
        name: String,
    },
    Completion(CompletionPopup),
    /// Quick Documentation (F1) / Error Description (Cmd+F1).
    Hover {
        editor: EditorId,
        offset: usize,
        text: String,
    },
    /// Parameter Info (Cmd+P).
    ParameterInfo {
        editor: EditorId,
        offset: usize,
        text: String,
    },
    /// Show Context Actions (Alt+Enter).
    Actions {
        editor: EditorId,
        title: String,
        items: Vec<ActionItem>,
        selected: usize,
        pending: bool,
    },
    /// Choose among several targets (declarations, implementations).
    Locations {
        title: String,
        items: Vec<LocationItem>,
        selected: usize,
    },
    /// Run › Run… (Ctrl+Alt+R): choose a run configuration.
    RunConfigs {
        selected: usize,
    },
    /// RustRover › Settings… (Cmd+,).
    Settings,
    /// RustRover › About.
    About,
    /// File › New › File / Directory / Cargo Crate: name prompt.
    NewItem {
        kind: NewItemKind,
        dir: PathBuf,
        name: String,
    },
    /// A simple message (errors from actions that need confirmation-free feedback).
    Message {
        title: String,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NewItemKind {
    File,
    Directory,
    CargoCrate,
}

/// The editor's find/replace bar (Cmd+F / Cmd+R).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindBar {
    pub query: SearchQuery,
    /// `Some` when the replace row is shown.
    pub replace: Option<String>,
    /// Matches in the active document (char ranges), refreshed when stale.
    pub matches: Vec<Range<usize>>,
    /// Index of the current match.
    pub current: Option<usize>,
    /// The document revision and query the matches were computed for.
    pub computed_for: Option<(u64, SearchQuery)>,
    pub error: Option<String>,
}

impl FindBar {
    pub fn invalidate(&mut self) {
        self.computed_for = None;
    }
}
