//! Edit › Find Usages, beyond Find Usages itself (`navigate_cmds.rs`): Show Usages (a popup at
//! the caret), Find Usages in File and Highlight Usages in File (highlights that stay until
//! Escape), Next/Previous Highlighted Usage, Recent Find Usages and Find Usages Settings….
//!
//! The language server answers when it is ready (`textDocument/references`,
//! `textDocument/documentHighlight`); otherwise, or when it has nothing, the whole-word
//! occurrences of the identifier at the caret stand in, so every action works on any file.

use std::ops::Range;
use std::path::Path;

use serde_json::{Value, json};

use codecraft_fs::FileMatch;
use codecraft_lsp::ResponseError;
use codecraft_lsp::lsp_types as lt;
use codecraft_text::{Document, SelectionSet};
use codecraft_workspace::{EditorId, FindResults, FindUsagesOptions, ToolWindowId, UsageScope};

use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, has_editor, ok, spec};
use crate::convert;
use crate::editing::{Chooser, ChooserItem, ChooserKind};
use crate::lsp::Pending;
use crate::params::opt_usize;
use crate::popup::LocationItem;
use crate::{Popup, Session};

/// Occurrences collected at most (hostile files cannot make the lists unbounded).
const MAX_OCCURRENCES: usize = 10_000;
/// Searches Recent Find Usages remembers.
pub const RECENT_LIMIT: usize = 10;

/// Highlight Usages in File: ranges of one editor's document at one revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageHighlights {
    pub editor: EditorId,
    pub revision: u64,
    pub ranges: Vec<Range<usize>>,
    pub word: String,
}

/// A finished Find Usages search, for Recent Find Usages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentSearch {
    pub title: String,
    pub results: FindResults,
}

#[derive(Default)]
pub struct UsagesState {
    /// Highlighted usages, shown until Escape (or until the document changes).
    pub highlights: Option<UsageHighlights>,
    /// Recent searches, most recent first.
    pub recent: Vec<RecentSearch>,
    /// The Find Usages Settings dialog's draft while it is open.
    pub settings_dialog: Option<FindUsagesOptions>,
}

impl UsagesState {
    /// The highlighted ranges of `editor` while its document is still at `revision`.
    pub fn highlights_for(&self, editor: EditorId, revision: u64) -> Option<&[Range<usize>]> {
        self.highlights.as_ref().filter(|h| h.editor == editor && h.revision == revision).map(|h| h.ranges.as_slice())
    }
}

/// Requests in flight.
pub(crate) enum UsagesPending {
    /// Show Usages: references → a popup.
    Show { editor: EditorId, word: String },
    /// Find/Highlight Usages in File: document highlights → highlights (and Find results).
    InFile { editor: EditorId, revision: u64, word: String, find: bool },
}

// ---------------------------------------------------------------- lexical fallback

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier at (or just before) `offset`.
pub fn word_at(doc: &Document, offset: usize) -> Option<String> {
    let r = codecraft_text::movement::word_range_at(doc, offset);
    let w = doc.slice(r);
    (!w.is_empty() && w.chars().all(is_ident) && !w.chars().all(|c| c.is_ascii_digit())).then_some(w)
}

/// Whole-word occurrences of `word` (char ranges, in order).
pub fn occurrences(doc: &Document, word: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if word.is_empty() {
        return out;
    }
    let text = doc.text();
    let word_chars = word.chars().count();
    let (mut char_pos, mut byte_pos) = (0usize, 0usize);
    for (b, _) in text.match_indices(word) {
        char_pos += text.get(byte_pos..b).map(|t| t.chars().count()).unwrap_or(0);
        byte_pos = b;
        let before = text.get(..b).and_then(|t| t.chars().next_back());
        let after = text.get(b.saturating_add(word.len())..).and_then(|t| t.chars().next());
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            out.push(char_pos..char_pos + word_chars);
            if out.len() >= MAX_OCCURRENCES {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------- results

/// Usages in test code: `tests`/`test` directories and `*_test.*`, `test_*.*`, `*.test.*`,
/// `*.spec.*` files.
pub fn is_test_path(path: &Path) -> bool {
    let in_test_dir = path.parent().is_some_and(|p| {
        p.components().any(|c| matches!(c.as_os_str().to_str(), Some("tests" | "test" | "__tests__")))
    });
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    in_test_dir
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.starts_with("test_")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
}

fn remember(s: &mut Session, results: &FindResults) {
    s.usages.recent.retain(|r| r.title != results.title);
    s.usages.recent.insert(0, RecentSearch { title: results.title.clone(), results: results.clone() });
    s.usages.recent.truncate(RECENT_LIMIT);
}

fn show_results(s: &mut Session, results: FindResults) {
    remember(s, &results);
    s.ws.find = Some(results);
    s.ws.layout.show(ToolWindowId::Find);
}

/// Find Usages results (from the language server): the Find Usages Settings filters apply, the
/// search joins Recent Find Usages, and the Find tool window shows it.
pub(crate) fn show_find_results(s: &mut Session, title: String, mut matches: Vec<FileMatch>) {
    let opts = s.ws.settings.find_usages.clone();
    let mut title = title;
    if opts.scope == UsageScope::CurrentFile
        && let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone())
    {
        matches.retain(|m| m.path == path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        title = format!("{title} in {name}");
    }
    if !opts.include_tests {
        matches.retain(|m| !is_test_path(&m.path));
    }
    s.status = format!("{} usage(s) found", matches.len());
    show_results(s, FindResults { title, matches, ..FindResults::default() });
}

/// Show search `index` of Recent Find Usages in the Find tool window again.
pub(crate) fn show_recent(s: &mut Session, index: usize) -> CmdResult {
    let r = s
        .usages
        .recent
        .get(index)
        .cloned()
        .ok_or_else(|| CommandError::BadParams(format!("no recent search {index}")))?;
    s.status = format!("{} ({} usage(s))", r.title, r.results.matches.len());
    show_results(s, r.results);
    Ok(json!({ "title": r.title }))
}

fn file_matches(doc: &Document, path: &Path, ranges: &[Range<usize>]) -> Vec<FileMatch> {
    ranges
        .iter()
        .map(|r| {
            let start = doc.offset_to_position(r.start);
            let end = doc.offset_to_position(r.end);
            let line_text: String = doc.line_text(start.line).unwrap_or_default().chars().take(500).collect();
            let end_column = if end.line == start.line { end.column } else { line_text.chars().count() };
            FileMatch { path: path.to_path_buf(), line: start.line, column: start.column, end_column, line_text }
        })
        .collect()
}

/// Apply in-file usages: highlight them, and for Find Usages in File list them too.
fn apply_in_file(s: &mut Session, editor: EditorId, revision: u64, word: &str, ranges: Vec<Range<usize>>, find: bool) {
    let Some(d) = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)) else { return };
    if d.doc.revision() != revision {
        s.status = "Usages not shown: the file changed".into();
        return;
    }
    let n = ranges.len();
    let listed = find.then(|| d.path.clone().map(|p| (file_matches(&d.doc, &p, &ranges), d.title()))).flatten();
    s.usages.highlights = Some(UsageHighlights { editor, revision, ranges, word: word.to_string() });
    s.status = format!("{n} usage(s) of '{word}' found in the file (Escape removes the highlighting)");
    if let Some((matches, file)) = listed {
        show_results(
            s,
            FindResults { title: format!("Usages of '{word}' in {file}"), matches, ..FindResults::default() },
        );
    }
}

fn show_popup(s: &mut Session, word: &str, mut items: Vec<LocationItem>) {
    if items.is_empty() {
        s.status = format!("No usages of '{word}' found");
        return;
    }
    items.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
    let title = format!("Usages of '{word}' ({})", items.len());
    s.popup = Some(Popup::Locations { title, items, selected: 0 });
}

/// The language server answered (or failed): fall back to the lexical occurrences when it has
/// nothing.
pub(crate) fn on_response(s: &mut Session, pending: UsagesPending, result: Result<Value, ResponseError>) {
    match pending {
        UsagesPending::Show { editor, word } => {
            let locs: Vec<lt::Location> =
                result.ok().and_then(|v| serde_json::from_value(v).ok()).flatten().unwrap_or_default();
            let mut items: Vec<LocationItem> = locs
                .iter()
                .take(MAX_OCCURRENCES)
                .filter_map(|l| Some(s.location_item(&codecraft_lsp::uri_to_path(&l.uri)?, &l.range)))
                .collect();
            if items.is_empty() {
                items = lexical_items(s, editor, &word);
            }
            if s.ws.active_editor() == Some(editor) {
                show_popup(s, &word, items);
            }
        }
        UsagesPending::InFile { editor, revision, word, find } => {
            let Some(d) = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)) else { return };
            let highlights: Vec<lt::DocumentHighlight> =
                result.ok().and_then(|v| serde_json::from_value(v).ok()).flatten().unwrap_or_default();
            let mut ranges: Vec<Range<usize>> =
                highlights.iter().take(MAX_OCCURRENCES).map(|h| convert::from_lsp_range(&d.doc, &h.range)).collect();
            if ranges.is_empty() {
                ranges = occurrences(&d.doc, &word);
            }
            ranges.sort_by_key(|r| (r.start, r.end));
            apply_in_file(s, editor, revision, &word, ranges, find);
        }
    }
}

fn lexical_items(s: &Session, editor: EditorId, word: &str) -> Vec<LocationItem> {
    let Some(d) = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)) else { return Vec::new() };
    let Some(path) = d.path.clone() else { return Vec::new() };
    occurrences(&d.doc, word)
        .into_iter()
        .map(|r| {
            let pos = d.doc.offset_to_position(r.start);
            let preview = d.doc.line_text(pos.line).unwrap_or_default().trim().to_string();
            LocationItem { path: path.clone(), line: pos.line, column: pos.column, preview }
        })
        .collect()
}

/// The active editor, its document's revision, the caret and the identifier there.
fn target(s: &Session) -> Option<(EditorId, u64, usize, Option<String>, bool)> {
    let (e, d) = s.ws.active()?;
    let caret = e.selections.primary().head;
    Some((e.id, d.doc.revision(), caret, word_at(&d.doc, caret), d.path.is_some()))
}

/// Whether the language server can answer for the active file.
fn use_server(s: &Session, has_path: bool) -> bool {
    has_path && s.lsp_ready().is_ok()
}

// ---------------------------------------------------------------- commands

fn show_usages(s: &mut Session, _: &Value) -> CmdResult {
    let Some((editor, _, _, word, has_path)) = target(s) else { return ok() };
    let Some(word) = word else {
        s.status = "No symbol at the caret".into();
        return ok();
    };
    if use_server(s, has_path) {
        let (_, path, _, pos) = s.caret_for_lsp()?;
        let pending = Pending::Usages(UsagesPending::Show { editor, word });
        s.lsp_send(|c| c.references(&path, pos.line, pos.character, false), pending)?;
        s.status = "Searching…".into();
        return Ok(json!({ "pending": true }));
    }
    let items = lexical_items(s, editor, &word);
    let n = items.len();
    if !has_path {
        s.status = "Show Usages needs a file saved on disk".into();
        return Ok(json!({ "usages": 0 }));
    }
    show_popup(s, &word, items);
    Ok(json!({ "usages": n }))
}

fn in_file(s: &mut Session, find: bool) -> CmdResult {
    let Some((editor, revision, caret, word, has_path)) = target(s) else { return ok() };
    // Highlight Usages again on a highlighted usage removes the highlighting.
    if !find
        && s.usages
            .highlights_for(editor, revision)
            .is_some_and(|r| r.iter().any(|r| r.start <= caret && caret <= r.end))
    {
        s.usages.highlights = None;
        s.status = "Highlighting removed".into();
        return Ok(json!({ "highlighted": 0 }));
    }
    let Some(word) = word else {
        s.status = "No symbol at the caret".into();
        return ok();
    };
    if use_server(s, has_path) {
        let (_, path, _, pos) = s.caret_for_lsp()?;
        let pending = Pending::Usages(UsagesPending::InFile { editor, revision, word, find });
        s.lsp_send(|c| c.document_highlight(&path, pos.line, pos.character), pending)?;
        return Ok(json!({ "pending": true }));
    }
    let ranges =
        s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)).map(|d| occurrences(&d.doc, &word)).unwrap_or_default();
    let n = ranges.len();
    apply_in_file(s, editor, revision, &word, ranges, find);
    Ok(json!({ "highlighted": n }))
}

/// Move the caret to the next (or previous) highlighted usage, wrapping around. Without
/// highlights, the occurrences of the identifier at the caret are used.
fn step(s: &mut Session, forward: bool) -> CmdResult {
    let Some((editor, revision, caret, word, _)) = target(s) else { return ok() };
    let mut ranges = match s.usages.highlights_for(editor, revision) {
        Some(r) if !r.is_empty() => r.to_vec(),
        _ => match (&word, s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc))) {
            (Some(w), Some(d)) => occurrences(&d.doc, w),
            _ => Vec::new(),
        },
    };
    if ranges.is_empty() {
        s.status = "No usages to go to".into();
        return ok();
    }
    ranges.sort_by_key(|r| (r.start, r.end));
    let current = ranges.iter().find(|r| r.start <= caret && caret <= r.end).map_or(caret, |r| r.start);
    let index = if forward {
        ranges.iter().position(|r| r.start > current).unwrap_or(0)
    } else {
        ranges.iter().rposition(|r| r.start < current).unwrap_or(ranges.len() - 1)
    };
    let Some(target) = ranges.get(index).map(|r| r.start) else { return ok() };
    s.select_active(|_, _| SelectionSet::caret(target));
    s.status = format!("Usage {} of {}", index + 1, ranges.len());
    Ok(json!({ "index": index, "count": ranges.len(), "offset": target }))
}

fn recent_find_usages(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(i) = opt_usize(p, "index")? {
        return show_recent(s, i);
    }
    let list: Vec<Value> = s
        .usages
        .recent
        .iter()
        .enumerate()
        .map(|(i, r)| json!({ "index": i, "title": r.title, "usages": r.results.matches.len() }))
        .collect();
    match s.ws.active_editor() {
        Some(editor) => {
            let items = s
                .usages
                .recent
                .iter()
                .enumerate()
                .map(|(i, r)| ChooserItem {
                    label: r.title.clone(),
                    detail: format!("{} usage(s)", r.results.matches.len()),
                    key: i.to_string(),
                })
                .collect();
            s.editing.chooser = Some(Chooser {
                kind: ChooserKind::RecentFindUsages,
                editor,
                title: "Recent Find Usages".into(),
                items,
                query: String::new(),
                selected: 0,
            });
        }
        None => {
            show_recent(s, 0)?;
        }
    }
    Ok(json!({ "searches": list }))
}

fn has_recent(s: &Session) -> Result<(), String> {
    if s.usages.recent.is_empty() { Err("no usages were searched yet".into()) } else { Ok(()) }
}

/// `{scope?, includeTests?}` from params onto `opts`; whether any was given.
fn read_options(p: &Value, opts: &mut FindUsagesOptions) -> Result<bool, CommandError> {
    let mut any = false;
    match p.get("scope") {
        None | Some(Value::Null) => {}
        Some(v) => {
            opts.scope = match v.as_str() {
                Some("project") => UsageScope::Project,
                Some("currentFile" | "current_file" | "file") => UsageScope::CurrentFile,
                _ => {
                    return Err(CommandError::BadParams(format!(
                        "unknown scope {v}; use \"project\" or \"currentFile\""
                    )));
                }
            };
            any = true;
        }
    }
    match p.get("includeTests") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => {
            opts.include_tests = *b;
            any = true;
        }
        Some(v) => return Err(CommandError::BadParams(format!("`includeTests` must be true or false, got {v}"))),
    }
    Ok(any)
}

fn options_json(o: &FindUsagesOptions) -> Value {
    let scope = match o.scope {
        UsageScope::Project => "project",
        UsageScope::CurrentFile => "currentFile",
    };
    json!({ "scope": scope, "includeTests": o.include_tests })
}

fn find_usages_settings(s: &mut Session, p: &Value) -> CmdResult {
    let mut opts = s.ws.settings.find_usages.clone();
    if read_options(p, &mut opts)? {
        s.ws.settings.find_usages = opts.clone();
        s.usages.settings_dialog = None;
    } else {
        s.usages.settings_dialog = Some(opts.clone());
    }
    Ok(options_json(&opts))
}

fn apply_settings(s: &mut Session, p: &Value) -> CmdResult {
    let mut opts = s.usages.settings_dialog.take().unwrap_or_else(|| s.ws.settings.find_usages.clone());
    read_options(p, &mut opts)?;
    s.ws.settings.find_usages = opts.clone();
    if crate::params::bool_or(p, "find", false) {
        s.execute("edit.findUsages.findUsages", &Value::Null)?;
    }
    Ok(options_json(&opts))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "edit.findUsages.showUsages",
            "Show Usages",
            None,
            "Usages of the symbol at the caret in a popup (language server references, else the identifier's occurrences in the file)",
            Context::Editor,
            has_editor,
            show_usages,
        ),
        spec(
            "edit.findUsages.findUsagesInFile",
            "Find Usages in File",
            None,
            "Highlights the usages of the symbol at the caret in this file and lists them in the Find tool window",
            Context::Editor,
            has_editor,
            |s, _| in_file(s, true),
        ),
        spec(
            "edit.findUsages.highlightUsagesInFile",
            "Highlight Usages in File",
            None,
            "Highlights the usages of the symbol at the caret until Escape (again on a usage removes them)",
            Context::Editor,
            has_editor,
            |s, _| in_file(s, false),
        ),
        spec(
            "edit.findUsages.nextHighlightedUsage",
            "Next Highlighted Usage",
            None,
            "→ {index, count, offset}",
            Context::Editor,
            has_editor,
            |s, _| step(s, true),
        ),
        spec(
            "edit.findUsages.previousHighlightedUsage",
            "Previous Highlighted Usage",
            None,
            "→ {index, count, offset}",
            Context::Editor,
            has_editor,
            |s, _| step(s, false),
        ),
        spec(
            "edit.findUsages.recentFindUsages",
            "Recent Find Usages",
            None,
            "{index?: int} — show that search again; without it, choose one → {searches: [{index, title, usages}]}",
            Context::Global,
            has_recent,
            recent_find_usages,
        ),
        spec(
            "edit.findUsages.findUsagesSettings",
            "Find Usages Settings…",
            None,
            "{scope?: \"project\"|\"currentFile\", includeTests?: bool} — set the options; without any, open the dialog → the options",
            Context::Global,
            always,
            find_usages_settings,
        ),
        spec(
            "findUsages.settings.apply",
            "Apply Find Usages Settings",
            None,
            "{scope?, includeTests?, find?: bool} — save the dialog's options (with `find`, run Find Usages)",
            Context::Global,
            always,
            apply_settings,
        ),
        spec("findUsages.settings.close", "Close Find Usages Settings", None, "", Context::Global, always, |s, _| {
            s.usages.settings_dialog = None;
            ok()
        }),
        spec("findUsages.clearHighlights", "Remove Usage Highlighting", None, "", Context::Global, always, |s, _| {
            s.usages.highlights = None;
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str =
        "fn main() {\n    let total = 1;\n    let t2 = total + total_x;\n    println!(\"{}\", total);\n}\n";

    fn session(dir: &Path, text: &str) -> Session {
        let path = dir.join("main.rs");
        std::fs::write(&path, text).unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": path })).unwrap();
        s
    }

    fn caret(s: &Session) -> usize {
        s.ws.active().unwrap().0.selections.primary().head
    }

    fn put(s: &mut Session, offset: usize) {
        s.execute("editor.setSelection", &json!({ "anchor": offset })).unwrap();
    }

    #[test]
    fn occurrences_are_whole_words_and_char_based() {
        let d = Document::from_text("é total totalx _total total");
        assert_eq!(occurrences(&d, "total"), vec![2..7, 22..27]);
        assert!(occurrences(&d, "").is_empty());
        assert_eq!(word_at(&d, 4).as_deref(), Some("total"));
        assert_eq!(word_at(&d, 7).as_deref(), Some("total"));
        assert_eq!(word_at(&Document::from_text("a + 12"), 5), None);
        assert_eq!(word_at(&Document::from_text("   "), 1), None);
    }

    #[test]
    fn highlight_usages_stays_until_escape_and_steps_between_usages() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path(), SRC);
        let first = SRC.find("total").unwrap();
        put(&mut s, first + 1);
        let v = s.execute("edit.findUsages.highlightUsagesInFile", &Value::Null).unwrap();
        assert_eq!(v["highlighted"], 3);
        let (eid, rev) = {
            let (e, d) = s.ws.active().unwrap();
            (e.id, d.doc.revision())
        };
        assert_eq!(s.usages.highlights_for(eid, rev).map(<[_]>::len), Some(3));
        // Caret motion keeps them; Next/Previous go round them.
        let v = s.execute("edit.findUsages.nextHighlightedUsage", &Value::Null).unwrap();
        assert_eq!((v["index"].as_u64(), caret(&s)), (Some(1), SRC.find("total +").unwrap()));
        s.execute("edit.findUsages.nextHighlightedUsage", &Value::Null).unwrap();
        s.execute("edit.findUsages.nextHighlightedUsage", &Value::Null).unwrap();
        assert_eq!(caret(&s), first);
        s.execute("edit.findUsages.previousHighlightedUsage", &Value::Null).unwrap();
        assert_eq!(caret(&s), SRC.rfind("total").unwrap());
        assert!(s.usages.highlights.is_some());
        // Escape removes them.
        s.execute("editor.escape", &Value::Null).unwrap();
        assert!(s.usages.highlights.is_none());
        // Highlight again, then again on a usage: toggled off.
        s.execute("edit.findUsages.highlightUsagesInFile", &Value::Null).unwrap();
        assert!(s.usages.highlights.is_some());
        s.execute("edit.findUsages.highlightUsagesInFile", &Value::Null).unwrap();
        assert!(s.usages.highlights.is_none());
        // Without highlights, Next uses the identifier at the caret.
        put(&mut s, first);
        s.execute("edit.findUsages.nextHighlightedUsage", &Value::Null).unwrap();
        assert_eq!(caret(&s), SRC.find("total +").unwrap());
        // An edit makes highlights stale (not shown).
        s.execute("edit.findUsages.highlightUsagesInFile", &Value::Null).unwrap();
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        let (eid, rev) = {
            let (e, d) = s.ws.active().unwrap();
            (e.id, d.doc.revision())
        };
        assert!(s.usages.highlights_for(eid, rev).is_none());
    }

    #[test]
    fn find_usages_in_file_lists_usages_and_recent_find_usages_brings_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path(), SRC);
        assert!(s.enabled("edit.findUsages.recentFindUsages").is_err());
        put(&mut s, SRC.find("total").unwrap());
        s.execute("edit.findUsages.findUsagesInFile", &Value::Null).unwrap();
        let find = s.ws.find.clone().unwrap();
        assert_eq!(find.title, "Usages of 'total' in main.rs");
        assert_eq!(find.matches.iter().map(|m| (m.line, m.column)).collect::<Vec<_>>(), [(1, 8), (2, 13), (3, 19)]);
        assert!(s.ws.layout.is_visible(ToolWindowId::Find));
        assert!(s.usages.highlights.is_some());
        // Another search, then Recent Find Usages restores the first.
        put(&mut s, SRC.find("t2").unwrap());
        s.execute("edit.findUsages.findUsagesInFile", &Value::Null).unwrap();
        assert_eq!(s.ws.find.as_ref().unwrap().matches.len(), 1);
        let v = s.execute("edit.findUsages.recentFindUsages", &Value::Null).unwrap();
        assert_eq!(v["searches"].as_array().map(Vec::len), Some(2));
        let c = s.editing.chooser.clone().unwrap();
        assert_eq!(c.kind, ChooserKind::RecentFindUsages);
        assert_eq!(c.items[1].label, "Usages of 'total' in main.rs");
        s.execute("editor.chooser.accept", &json!({ "index": 1 })).unwrap();
        assert_eq!(s.ws.find.as_ref().unwrap().matches.len(), 3);
        assert_eq!(s.usages.recent[0].title, "Usages of 'total' in main.rs");
        s.execute("edit.findUsages.recentFindUsages", &json!({ "index": 1 })).unwrap();
        assert_eq!(s.ws.find.as_ref().unwrap().matches.len(), 1);
        assert!(s.execute("edit.findUsages.recentFindUsages", &json!({ "index": 7 })).is_err());
    }

    #[test]
    fn show_usages_opens_a_popup_at_the_caret() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path(), SRC);
        put(&mut s, SRC.find("total").unwrap());
        let v = s.execute("edit.findUsages.showUsages", &Value::Null).unwrap();
        assert_eq!(v["usages"], 3);
        let Some(Popup::Locations { title, items, .. }) = &s.popup else { panic!("{:?}", s.popup) };
        assert_eq!(title, "Usages of 'total' (3)");
        assert_eq!(items[0].preview, "let total = 1;");
        // Nothing at the caret: a message, no popup.
        s.popup = None;
        put(&mut s, SRC.find('{').unwrap());
        s.execute("edit.findUsages.showUsages", &Value::Null).unwrap();
        assert!(s.popup.is_none());
    }

    #[test]
    fn server_answers_are_used_and_an_empty_answer_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path(), SRC);
        let (eid, rev) = {
            let (e, d) = s.ws.active().unwrap();
            (e.id, d.doc.revision())
        };
        // documentHighlight: one range (line 1, chars 8..13).
        let hl =
            json!([{ "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 13 } } }]);
        on_response(
            &mut s,
            UsagesPending::InFile { editor: eid, revision: rev, word: "total".into(), find: false },
            Ok(hl),
        );
        let got = s.usages.highlights_for(eid, rev).unwrap();
        assert_eq!((got.len(), got.first().cloned()), (1, Some(20..25)));
        // An error falls back to the occurrences.
        let err = ResponseError { code: -32603, message: "boom".into() };
        on_response(
            &mut s,
            UsagesPending::InFile { editor: eid, revision: rev, word: "total".into(), find: false },
            Err(err),
        );
        assert_eq!(s.usages.highlights_for(eid, rev).map(<[_]>::len), Some(3));
        // A stale answer is dropped.
        s.usages.highlights = None;
        on_response(
            &mut s,
            UsagesPending::InFile { editor: eid, revision: rev + 1, word: "total".into(), find: false },
            Ok(json!(null)),
        );
        assert!(s.usages.highlights.is_none());
        // references → popup; garbage is an empty answer.
        on_response(&mut s, UsagesPending::Show { editor: eid, word: "total".into() }, Ok(json!({ "nonsense": 1 })));
        assert!(matches!(&s.popup, Some(Popup::Locations { items, .. }) if items.len() == 3));
    }

    #[test]
    fn find_usages_settings_filter_scope_and_tests() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path(), SRC);
        let v = s.execute("edit.findUsages.findUsagesSettings", &Value::Null).unwrap();
        assert_eq!(v, json!({ "scope": "project", "includeTests": true }));
        assert!(s.usages.settings_dialog.is_some());
        s.execute("findUsages.settings.apply", &json!({ "includeTests": false })).unwrap();
        assert!(s.usages.settings_dialog.is_none());
        assert!(!s.ws.settings.find_usages.include_tests);
        let m =
            |p: &Path| FileMatch { path: p.to_path_buf(), line: 0, column: 0, end_column: 1, line_text: String::new() };
        let all = vec![
            m(&s.ws.active().unwrap().1.path.clone().unwrap()),
            m(Path::new("/p/tests/it.rs")),
            m(Path::new("/p/src/other.rs")),
        ];
        show_find_results(&mut s, "Usages of 'x'".into(), all.clone());
        assert_eq!(s.ws.find.as_ref().unwrap().matches.len(), 2);
        s.execute("edit.findUsages.findUsagesSettings", &json!({ "scope": "currentFile", "includeTests": true }))
            .unwrap();
        show_find_results(&mut s, "Usages of 'x'".into(), all);
        let f = s.ws.find.clone().unwrap();
        assert_eq!(f.matches.len(), 1);
        assert_eq!(f.title, "Usages of 'x' in main.rs");
        assert!(s.execute("edit.findUsages.findUsagesSettings", &json!({ "scope": "galaxy" })).is_err());
        assert!(s.execute("edit.findUsages.findUsagesSettings", &json!({ "includeTests": "yes" })).is_err());
        s.execute("findUsages.settings.close", &Value::Null).unwrap();
        assert!(is_test_path(Path::new("/a/src/foo_test.go")) && is_test_path(Path::new("/a/test_x.py")));
        assert!(!is_test_path(Path::new("/a/src/lib.rs")) && !is_test_path(Path::new("/a/latest.rs")));
    }
}
