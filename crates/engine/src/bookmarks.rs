//! Line bookmarks (Edit › Bookmarks, F3): plain or with a mnemonic (`0`–`9`, `A`–`Z`), an
//! optional description, persisted per project in `<project>/.codecraft/bookmarks.json`.
//!
//! A bookmark of an open document follows edits: its line start is kept as a char offset at the
//! document revision it was last seen at and mapped through the document's change log
//! ([`Document::changes_since`](codecraft_text::Document::changes_since)) on every [`pump`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use codecraft_text::Assoc;

use crate::Session;

/// Bookmarks kept per project (a corrupt or hostile file cannot make us hold more).
pub const MAX_BOOKMARKS: usize = 2000;
/// Longest description kept.
const MAX_DESCRIPTION: usize = 500;
/// Longest line preview.
const MAX_PREVIEW: usize = 160;
/// Files read for previews of bookmarks in closed files.
const MAX_PREVIEW_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn preview_of(line: &str) -> String {
    line.trim().chars().take(MAX_PREVIEW).collect()
}

/// Previews of bookmarks in files that are not open (read once, when the project loads).
fn fill_previews(items: &mut [Bookmark]) {
    let mut cache: std::collections::HashMap<PathBuf, Option<Vec<String>>> = std::collections::HashMap::new();
    for b in items.iter_mut() {
        let lines = cache.entry(b.path.clone()).or_insert_with(|| {
            let small = std::fs::metadata(&b.path).is_ok_and(|m| m.len() <= MAX_PREVIEW_FILE_BYTES);
            small
                .then(|| codecraft_fs::read_text_file(&b.path).ok())
                .flatten()
                .map(|t| t.lines().map(str::to_string).collect())
        });
        b.preview = lines.as_ref().and_then(|l| l.get(b.line)).map(|l| preview_of(l)).unwrap_or_default();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Absolute path (relative to the project root in the file).
    pub path: PathBuf,
    /// Zero-based line.
    pub line: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mnemonic: Option<char>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The line's text (trimmed, shortened) for lists.
    #[serde(skip)]
    pub preview: String,
    /// `(document revision, line start offset)` while the file is open.
    #[serde(skip)]
    anchor: Option<(u64, usize)>,
}

impl Bookmark {
    pub fn new(path: PathBuf, line: usize) -> Self {
        Self { path, line, mnemonic: None, description: String::new(), preview: String::new(), anchor: None }
    }
}

/// The mnemonic chooser (Toggle Bookmark with Mnemonic, Alt+F3) for one line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MnemonicDialog {
    pub path: PathBuf,
    pub line: usize,
    /// The line's current mnemonic, if it has one.
    pub current: Option<char>,
    pub description: String,
}

/// Edit Bookmark Description dialog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescriptionDialog {
    pub index: usize,
    pub description: String,
}

#[derive(Debug, Default)]
pub struct BookmarksState {
    pub items: Vec<Bookmark>,
    /// The project root `items` belong to (`None`: no project, nothing persisted).
    loaded_for: Option<PathBuf>,
    /// The mnemonic chooser is open.
    pub mnemonic_dialog: Option<MnemonicDialog>,
    pub description_dialog: Option<DescriptionDialog>,
    /// The Show Bookmarks popup (Cmd+F3) is open.
    pub popup: bool,
    /// The last load or save error.
    pub error: Option<String>,
}

impl BookmarksState {
    /// Bookmarked lines of `path` with their mnemonics, sorted by line.
    pub fn lines_in(&self, path: &Path) -> Vec<(usize, Option<char>)> {
        let mut v: Vec<_> = self.items.iter().filter(|b| b.path == path).map(|b| (b.line, b.mnemonic)).collect();
        v.sort_unstable();
        v
    }

    pub fn find(&self, path: &Path, line: usize) -> Option<usize> {
        self.items.iter().position(|b| b.path == path && b.line == line)
    }

    pub fn by_mnemonic(&self, m: char) -> Option<usize> {
        let m = m.to_ascii_uppercase();
        self.items.iter().position(|b| b.mnemonic == Some(m))
    }
}

/// A valid mnemonic (`0`–`9`, `A`–`Z`), upper-cased.
pub fn normalize_mnemonic(c: char) -> Option<char> {
    let c = c.to_ascii_uppercase();
    (c.is_ascii_digit() || c.is_ascii_uppercase()).then_some(c)
}

pub fn file_path(root: &Path) -> PathBuf {
    root.join(".codecraft").join("bookmarks.json")
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    #[serde(default)]
    bookmarks: Vec<Bookmark>,
}

/// Read the bookmarks of `root` (paths made absolute; invalid entries dropped).
pub fn load(root: &Path) -> Result<Vec<Bookmark>, String> {
    let path = file_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let file: FileFormat =
        serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    let mut out: Vec<Bookmark> = Vec::new();
    for mut b in file.bookmarks.into_iter().take(MAX_BOOKMARKS) {
        // Only paths inside the project.
        if b.path.is_absolute() || b.path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            continue;
        }
        b.path = root.join(&b.path);
        b.mnemonic = b.mnemonic.and_then(normalize_mnemonic);
        if b.mnemonic.is_some() && out.iter().any(|o| o.mnemonic == b.mnemonic) {
            b.mnemonic = None;
        }
        b.description = truncate(&b.description);
        b.anchor = None;
        if !out.iter().any(|o| o.path == b.path && o.line == b.line) {
            out.push(b);
        }
    }
    Ok(out)
}

/// Write `items` under `root` (paths stored relative to it; files outside are left out).
pub fn save(root: &Path, items: &[Bookmark]) -> Result<(), String> {
    let bookmarks: Vec<Bookmark> = items
        .iter()
        .filter_map(|b| {
            let rel = b.path.strip_prefix(root).ok()?;
            Some(Bookmark { path: rel.to_path_buf(), anchor: None, preview: String::new(), ..b.clone() })
        })
        .collect();
    let path = file_path(root);
    if bookmarks.is_empty() && !path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&FileFormat { bookmarks }).map_err(|e| e.to_string())?;
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| e.to_string())
}

fn truncate(s: &str) -> String {
    s.chars().take(MAX_DESCRIPTION).collect()
}

/// Load the open project's bookmarks when the project changed. Returns true if it did.
pub(crate) fn ensure_loaded(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root == s.bookmarks.loaded_for {
        return false;
    }
    s.bookmarks.items.clear();
    s.bookmarks.mnemonic_dialog = None;
    s.bookmarks.description_dialog = None;
    s.bookmarks.popup = false;
    s.bookmarks.error = None;
    if let Some(r) = &root {
        match load(r) {
            Ok(mut items) => {
                fill_previews(&mut items);
                s.bookmarks.items = items;
            }
            Err(e) => s.bookmarks.error = Some(e),
        }
    }
    s.bookmarks.loaded_for = root;
    true
}

/// Persist (when a project is open). Errors are kept for the tool window, never fatal.
pub(crate) fn persist(s: &mut Session) {
    let Some(root) = s.bookmarks.loaded_for.clone() else { return };
    // A file that failed to parse is left alone rather than overwritten.
    if s.bookmarks.error.as_deref().is_some_and(|e| e.starts_with("cannot parse")) {
        return;
    }
    if let Err(e) = save(&root, &s.bookmarks.items) {
        log::warn!("bookmarks: {e}");
        s.bookmarks.error = Some(e);
    }
}

/// Move bookmarks of open documents with their text. Returns true if any line changed.
pub(crate) fn sync(s: &mut Session) -> bool {
    let mut moved = false;
    let ws = &s.ws;
    for b in &mut s.bookmarks.items {
        let Some(od) = ws.doc_by_path(&b.path).and_then(|id| ws.doc(id)) else {
            b.anchor = None;
            continue;
        };
        let doc = &od.doc;
        let rev = doc.revision();
        let last_line = doc.len_lines().saturating_sub(1);
        let line = match b.anchor {
            Some((r, _)) if r == rev => continue,
            Some((r, offset)) => match doc.changes_since(r) {
                Some(sets) => {
                    let mapped = sets.iter().fold(offset, |o, cs| cs.map_pos(o, Assoc::After));
                    doc.char_to_line(mapped.min(doc.len_chars()))
                }
                None => b.line.min(last_line),
            },
            None => b.line.min(last_line),
        };
        if line != b.line {
            b.line = line;
            moved = true;
        }
        b.anchor = Some((rev, doc.line_start(line)));
        b.preview = doc.line_text(line).map(|t| preview_of(&t)).unwrap_or_default();
    }
    if moved {
        // Deleted lines can fold two bookmarks onto one: keep the first.
        let mut seen = std::collections::HashSet::new();
        s.bookmarks.items.retain(|b| seen.insert((b.path.clone(), b.line)));
    }
    moved
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let loaded = ensure_loaded(s);
    let moved = sync(s);
    if moved {
        persist(s);
    }
    loaded || moved
}

/// Add or remove a bookmark on `line` of `path`. Returns whether the line is now bookmarked.
pub(crate) fn toggle(s: &mut Session, path: PathBuf, line: usize) -> Result<bool, String> {
    ensure_loaded(s);
    sync(s);
    let now = match s.bookmarks.find(&path, line) {
        Some(i) => {
            s.bookmarks.items.remove(i);
            false
        }
        None => {
            if s.bookmarks.items.len() >= MAX_BOOKMARKS {
                return Err(format!("too many bookmarks (at most {MAX_BOOKMARKS})"));
            }
            s.bookmarks.items.push(Bookmark::new(path, line));
            sync(s);
            true
        }
    };
    persist(s);
    Ok(now)
}

/// Give `line` the mnemonic `m` (taking it from another bookmark); the same mnemonic again
/// removes the bookmark. Returns whether the line is now bookmarked.
pub(crate) fn set_mnemonic(s: &mut Session, path: PathBuf, line: usize, m: char) -> Result<bool, String> {
    let m = normalize_mnemonic(m).ok_or_else(|| format!("'{m}' is not a mnemonic (use 0-9 or A-Z)"))?;
    ensure_loaded(s);
    sync(s);
    let here = s.bookmarks.find(&path, line);
    if let Some(i) = here
        && s.bookmarks.items.get(i).is_some_and(|b| b.mnemonic == Some(m))
    {
        s.bookmarks.items.remove(i);
        persist(s);
        return Ok(false);
    }
    for b in &mut s.bookmarks.items {
        if b.mnemonic == Some(m) {
            b.mnemonic = None;
        }
    }
    match here {
        Some(i) => {
            if let Some(b) = s.bookmarks.items.get_mut(i) {
                b.mnemonic = Some(m);
            }
        }
        None => {
            if s.bookmarks.items.len() >= MAX_BOOKMARKS {
                return Err(format!("too many bookmarks (at most {MAX_BOOKMARKS})"));
            }
            let mut b = Bookmark::new(path, line);
            b.mnemonic = Some(m);
            s.bookmarks.items.push(b);
            sync(s);
        }
    }
    persist(s);
    Ok(true)
}

pub(crate) fn set_description(s: &mut Session, index: usize, description: &str) -> Result<(), String> {
    let b = s.bookmarks.items.get_mut(index).ok_or_else(|| format!("no bookmark {index}"))?;
    b.description = truncate(description.trim());
    persist(s);
    Ok(())
}

/// Bookmarks in display order: by file, then line.
pub fn sorted(items: &[Bookmark]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|a, b| {
        let (x, y) = (items.get(*a), items.get(*b));
        x.map(|x| (&x.path, x.line)).cmp(&y.map(|y| (&y.path, y.line)))
    });
    idx
}

/// The bookmarked line after (`forward`) or before `line` in `lines`, wrapping around.
pub fn next_line(lines: &[usize], line: usize, forward: bool) -> Option<usize> {
    if forward {
        lines.iter().copied().find(|l| *l > line).or_else(|| lines.first().copied())
    } else {
        lines.iter().rev().copied().find(|l| *l < line).or_else(|| lines.last().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_save_roundtrip_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut b = Bookmark::new(root.join("src/main.rs"), 3);
        b.mnemonic = Some('1');
        b.description = "entry".into();
        let outside = Bookmark::new(PathBuf::from("/elsewhere/x.rs"), 1);
        save(root, &[b.clone(), outside]).unwrap();
        let text = std::fs::read_to_string(file_path(root)).unwrap();
        assert!(text.contains("src/main.rs") && !text.contains(&root.display().to_string()), "{text}");
        assert!(!text.contains("elsewhere"));
        assert_eq!(load(root).unwrap(), vec![b]);
    }

    #[test]
    fn hostile_file_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".codecraft")).unwrap();
        let text = r#"{"bookmarks":[
            {"path":"/etc/passwd","line":1},
            {"path":"../up.rs","line":1},
            {"path":"a.rs","line":2,"mnemonic":"x"},
            {"path":"b.rs","line":2,"mnemonic":"X"},
            {"path":"a.rs","line":2},
            {"path":"c.rs","line":18446744073709551615,"mnemonic":"!"}
        ]}"#;
        std::fs::write(file_path(root), text).unwrap();
        let items = load(root).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].mnemonic, Some('X'));
        assert_eq!(items[1].mnemonic, None, "duplicate mnemonic dropped");
        assert_eq!(items[2].mnemonic, None, "invalid mnemonic dropped");
        std::fs::write(file_path(root), "{not json").unwrap();
        assert!(load(root).unwrap_err().contains("cannot parse"));
    }

    #[test]
    fn next_and_previous_wrap() {
        let lines = [2, 5, 9];
        assert_eq!(next_line(&lines, 5, true), Some(9));
        assert_eq!(next_line(&lines, 9, true), Some(2));
        assert_eq!(next_line(&lines, 2, false), Some(9));
        assert_eq!(next_line(&lines, 6, false), Some(5));
        assert_eq!(next_line(&[], 6, false), None);
    }

    #[test]
    fn mnemonics() {
        assert_eq!(normalize_mnemonic('a'), Some('A'));
        assert_eq!(normalize_mnemonic('7'), Some('7'));
        assert_eq!(normalize_mnemonic('é'), None);
        assert_eq!(normalize_mnemonic(' '), None);
    }
}
