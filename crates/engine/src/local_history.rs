//! Local History (File › Local History): snapshots of project files taken when CodeCraft saves
//! them and when they change on disk, plus labels, kept in `<project>/.codecraft/local-history`
//! (an `index.json` and content-addressed blobs) for a few days and a bounded size.
//!
//! Saving records the file's previous disk content first when the history does not have it yet
//! (so the first save of a file can be reverted), then the saved text. The file watcher records
//! external changes of files that are open or already have history. Identical consecutive
//! snapshots of a file are recorded once.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Session;

/// Snapshots older than this are dropped (RustRover keeps five working days).
pub const MAX_AGE_MS: u64 = 5 * 24 * 60 * 60 * 1000;
/// Total blob size kept; the oldest snapshots go first.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Files larger than this are not recorded.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Index entries kept.
pub const MAX_ENTRIES: usize = 10_000;
/// External changes recorded per watcher batch (a branch switch is not worth thousands of copies).
const MAX_EXTERNAL_BATCH: usize = 100;
/// Longest label.
const MAX_LABEL: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// The file's content when the history first saw it.
    Original,
    /// Saved by CodeCraft.
    Saved,
    /// Changed on disk by something else (git, a terminal, an agent).
    External,
    /// Restored from local history.
    Reverted,
    /// A user label (Put Label…), project-wide.
    Label,
}

impl EntryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Saved => "Saved",
            Self::External => "External change",
            Self::Reverted => "Reverted",
            Self::Label => "Label",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    /// Milliseconds since the Unix epoch.
    pub time_ms: u64,
    pub kind: EntryKind,
    /// Project-relative path (`None` for labels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Content blob name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(default)]
    pub size: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    entries: Vec<Entry>,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// FNV-1a: a stable content hash for blob names (not security relevant: names are also checked
/// by length and the content is compared on dedupe).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn blob_name(content: &str) -> String {
    format!("{:016x}-{}", fnv1a(content.as_bytes()), content.len())
}

fn valid_blob_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A project's local history on disk.
#[derive(Debug)]
pub struct HistoryStore {
    root: PathBuf,
    dir: PathBuf,
    entries: Vec<Entry>,
    next_id: u64,
    /// Records since the last prune.
    since_prune: usize,
}

impl HistoryStore {
    pub fn dir_for(root: &Path) -> PathBuf {
        root.join(".codecraft").join("local-history")
    }

    /// Open (or start) the history of `root`. A corrupt index is set aside and a new one started.
    pub fn open(root: &Path, now: u64) -> Self {
        let dir = Self::dir_for(root);
        let index_path = dir.join("index.json");
        let entries = match std::fs::read_to_string(&index_path) {
            Ok(text) => match serde_json::from_str::<Index>(&text) {
                Ok(ix) => ix.entries,
                Err(e) => {
                    log::warn!("local history index is corrupt ({e}); starting a new one");
                    let _ = std::fs::rename(&index_path, dir.join(format!("index.corrupt-{now}.json")));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        // Drop entries a hand-edited or hostile index could abuse.
        let entries: Vec<Entry> = entries
            .into_iter()
            .filter(|e| {
                e.blob.as_deref().is_none_or(valid_blob_name)
                    && e.path.as_deref().is_none_or(|p| {
                        p.is_relative() && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
                    })
            })
            .take(MAX_ENTRIES)
            .collect();
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0).saturating_add(1);
        let mut store = Self { root: root.to_path_buf(), dir, entries, next_id, since_prune: 0 };
        store.prune(now);
        store
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn rel(&self, path: &Path) -> Option<PathBuf> {
        let rel = path.strip_prefix(&self.root).ok()?;
        if rel.as_os_str().is_empty() || rel.starts_with(".codecraft") || rel.starts_with(".git") {
            return None;
        }
        Some(rel.to_path_buf())
    }

    pub fn abs(&self, entry: &Entry) -> Option<PathBuf> {
        entry.path.as_ref().map(|p| self.root.join(p))
    }

    /// Snapshots of `path`, oldest first.
    pub fn file_entries(&self, path: &Path) -> Vec<&Entry> {
        let Some(rel) = self.rel(path) else { return Vec::new() };
        self.entries.iter().filter(|e| e.path.as_ref() == Some(&rel)).collect()
    }

    pub fn has_history(&self, path: &Path) -> bool {
        self.rel(path).is_some_and(|rel| self.entries.iter().any(|e| e.path.as_ref() == Some(&rel)))
    }

    /// The content of a snapshot.
    pub fn content(&self, id: u64) -> Result<String, String> {
        let e = self.entry(id).ok_or_else(|| format!("no local history entry {id}"))?;
        let blob = e.blob.as_deref().ok_or("a label has no content")?;
        if !valid_blob_name(blob) {
            return Err("invalid snapshot name".into());
        }
        std::fs::read_to_string(self.dir.join("blobs").join(blob)).map_err(|e| format!("cannot read the snapshot: {e}"))
    }

    /// The snapshot before `id` of the same file.
    pub fn previous(&self, id: u64) -> Option<&Entry> {
        let e = self.entry(id)?;
        let path = e.path.as_ref()?;
        self.entries.iter().rev().find(|o| o.path.as_ref() == Some(path) && o.id < id && o.blob.is_some())
    }

    /// Record `content` of `path`. Returns the new entry id, or `None` when nothing was
    /// recorded (outside the project, too large, or the same as the file's last snapshot).
    pub fn record(&mut self, path: &Path, content: &str, kind: EntryKind, now: u64) -> Result<Option<u64>, String> {
        let Some(rel) = self.rel(path) else { return Ok(None) };
        if content.len() as u64 > MAX_FILE_BYTES {
            return Ok(None);
        }
        let blob = blob_name(content);
        let last = self.entries.iter().rev().find(|e| e.path.as_ref() == Some(&rel) && e.blob.is_some());
        if last.is_some_and(|e| e.blob.as_deref() == Some(blob.as_str())) {
            return Ok(None);
        }
        let kind = if last.is_none() && kind == EntryKind::External { EntryKind::Original } else { kind };
        let blobs = self.dir.join("blobs");
        std::fs::create_dir_all(&blobs).map_err(|e| format!("cannot create {}: {e}", blobs.display()))?;
        let blob_path = blobs.join(&blob);
        if !blob_path.exists() {
            codecraft_fs::write_text_file_atomic(&blob_path, content).map_err(|e| e.to_string())?;
        }
        let id = self.push(Entry {
            id: 0,
            time_ms: now,
            kind,
            path: Some(rel),
            label: None,
            blob: Some(blob),
            size: content.len() as u64,
        });
        self.after_change(now)?;
        Ok(Some(id))
    }

    /// A project-wide label.
    pub fn put_label(&mut self, label: &str, now: u64) -> Result<u64, String> {
        let label: String = label.trim().chars().take(MAX_LABEL).collect();
        if label.is_empty() {
            return Err("the label is empty".into());
        }
        let id =
            self.push(Entry { id: 0, time_ms: now, kind: EntryKind::Label, path: None, label: Some(label), blob: None, size: 0 });
        self.after_change(now)?;
        Ok(id)
    }

    fn push(&mut self, mut e: Entry) -> u64 {
        e.id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let id = e.id;
        self.entries.push(e);
        id
    }

    fn after_change(&mut self, now: u64) -> Result<(), String> {
        self.since_prune += 1;
        if self.since_prune >= 50 || self.entries.len() > MAX_ENTRIES {
            self.prune(now);
        }
        self.save_index()
    }

    fn save_index(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let text = serde_json::to_string(&Index { entries: self.entries.clone() }).map_err(|e| e.to_string())?;
        codecraft_fs::write_text_file_atomic(&self.dir.join("index.json"), &text).map_err(|e| e.to_string())
    }

    /// Drop old entries, keep the total size and count bounded, delete unreferenced blobs.
    pub fn prune(&mut self, now: u64) {
        self.since_prune = 0;
        let before = self.entries.len();
        self.entries.retain(|e| now.saturating_sub(e.time_ms) <= MAX_AGE_MS);
        // Distinct blobs count once.
        loop {
            let mut seen = HashSet::new();
            let total: u64 = self
                .entries
                .iter()
                .filter(|e| e.blob.as_ref().is_some_and(|b| seen.insert(b.clone())))
                .map(|e| e.size)
                .sum();
            if (total <= MAX_TOTAL_BYTES && self.entries.len() <= MAX_ENTRIES) || self.entries.is_empty() {
                break;
            }
            let drop = (self.entries.len() / 10).max(1);
            self.entries.drain(..drop.min(self.entries.len()));
        }
        let referenced: HashSet<&str> = self.entries.iter().filter_map(|e| e.blob.as_deref()).collect();
        if let Ok(rd) = std::fs::read_dir(self.dir.join("blobs")) {
            for f in rd.flatten() {
                let name = f.file_name();
                let Some(name) = name.to_str() else { continue };
                if valid_blob_name(name) && !referenced.contains(name) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        if self.entries.len() != before {
            let _ = self.save_index();
        }
    }
}

// ---------------------------------------------------------------- session integration

/// What the history window shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    /// One file's snapshots (Show History).
    File { path: PathBuf },
    /// Snapshots that changed the given lines of a file (Show History for Selection).
    Selection { path: PathBuf, start_line: usize, end_line: usize },
    /// Every file's snapshots and the labels (Show Project History).
    Project,
    /// The most recent snapshots and labels (Recent Changes).
    Recent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub id: u64,
    pub time_ms: u64,
    pub kind: EntryKind,
    pub path: Option<PathBuf>,
    pub label: Option<String>,
}

/// The Local History window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryView {
    pub mode: ViewMode,
    pub title: String,
    /// Newest first.
    pub revisions: Vec<Revision>,
    pub selected: Option<usize>,
    /// The diff of the selected revision: `old` → `new` (empty for labels).
    pub old_text: String,
    pub new_text: String,
    /// What the two sides are (`"14:02 Saved"`, `"Current"`).
    pub old_title: String,
    pub new_title: String,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct LocalHistoryState {
    store: Option<HistoryStore>,
    loaded_for: Option<PathBuf>,
    pub view: Option<HistoryView>,
    /// The Put Label dialog's text while it is open.
    pub label_dialog: Option<String>,
    pub error: Option<String>,
}

impl LocalHistoryState {
    pub fn store(&self) -> Option<&HistoryStore> {
        self.store.as_ref()
    }
}

/// Open the project's history when the project changed.
pub(crate) fn ensure_loaded(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root == s.local_history.loaded_for {
        return false;
    }
    s.local_history.store = root.as_deref().map(|r| HistoryStore::open(r, now_ms()));
    s.local_history.loaded_for = root;
    s.local_history.view = None;
    s.local_history.label_dialog = None;
    true
}

fn record(s: &mut Session, path: &Path, content: &str, kind: EntryKind) {
    ensure_loaded(s);
    let Some(store) = s.local_history.store.as_mut() else { return };
    if let Err(e) = store.record(path, content, kind, now_ms()) {
        log::warn!("local history: {e}");
        s.local_history.error = Some(e);
    }
}

/// Before CodeCraft writes modified documents: keep their current disk content if the history
/// does not have it yet.
pub(crate) fn before_save(s: &mut Session) {
    ensure_loaded(s);
    let Some(store) = s.local_history.store.as_ref() else { return };
    let paths: Vec<PathBuf> = s
        .ws
        .docs()
        .filter(|d| d.doc.is_modified())
        .filter_map(|d| d.path.clone())
        .filter(|p| store.rel(p).is_some())
        .collect();
    for p in paths {
        if std::fs::metadata(&p).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        if let Ok(text) = codecraft_fs::read_text_file(&p) {
            record(s, &p, &text, EntryKind::External);
        }
    }
}

/// After CodeCraft saved `paths`: record what was written.
pub(crate) fn after_save(s: &mut Session, paths: &[PathBuf]) {
    for p in paths {
        let Some(text) = s.ws.doc_by_path(p).and_then(|id| s.ws.doc(id)).map(|d| d.doc.to_disk_text()) else {
            continue;
        };
        record(s, p, &text, EntryKind::Saved);
    }
}

/// The watcher saw `paths` change: record open files and files with history.
pub(crate) fn files_changed(s: &mut Session, paths: &[PathBuf]) {
    ensure_loaded(s);
    let Some(store) = s.local_history.store.as_ref() else { return };
    let wanted: Vec<PathBuf> = paths
        .iter()
        .filter(|p| store.rel(p).is_some())
        .filter(|p| s.ws.doc_by_path(p).is_some() || store.has_history(p))
        .take(MAX_EXTERNAL_BATCH)
        .cloned()
        .collect();
    for p in wanted {
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        if let Ok(text) = codecraft_fs::read_text_file(&p) {
            record(s, &p, &text, EntryKind::External);
        }
    }
}

pub(crate) fn put_label(s: &mut Session, label: &str) -> Result<u64, String> {
    ensure_loaded(s);
    let store = s.local_history.store.as_mut().ok_or("no project is open")?;
    let id = store.put_label(label, now_ms())?;
    s.local_history.label_dialog = None;
    refresh_view(s);
    Ok(id)
}

/// The current text of `path`: the open document's, else the file's.
fn current_text(s: &Session, path: &Path) -> String {
    if let Some(d) = s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        return d.doc.to_disk_text();
    }
    codecraft_fs::read_text_file(path).unwrap_or_default()
}

fn revision(e: &Entry, store: &HistoryStore) -> Revision {
    Revision { id: e.id, time_ms: e.time_ms, kind: e.kind, path: store.abs(e), label: e.label.clone() }
}

/// Whether the change from `old` to `new` touches lines `start..=end` of `new`.
pub fn touches_lines(old: &str, new: &str, start: usize, end: usize) -> bool {
    let old = old.replace("\r\n", "\n");
    let new = new.replace("\r\n", "\n");
    codecraft_vcs::lines::line_changes(&old, &new).iter().any(|c| {
        if c.start == c.end {
            c.start >= start && c.start <= end.saturating_add(1)
        } else {
            c.start <= end && c.end > start
        }
    })
}

fn revisions_for(s: &Session, mode: &ViewMode) -> Vec<Revision> {
    let Some(store) = s.local_history.store.as_ref() else { return Vec::new() };
    let mut out: Vec<Revision> = match mode {
        ViewMode::File { path } => store.file_entries(path).into_iter().map(|e| revision(e, store)).collect(),
        ViewMode::Selection { path, start_line, end_line } => {
            let current = current_text(s, path);
            let entries = store.file_entries(path);
            // Revisions after which the selected lines (as they read now) changed.
            let mut keep = Vec::new();
            for (i, e) in entries.iter().enumerate() {
                let Ok(old) = store.content(e.id) else { continue };
                let next = match entries.get(i + 1) {
                    Some(n) => store.content(n.id).unwrap_or_default(),
                    None => current.clone(),
                };
                if old != next && touches_lines(&old, &current, *start_line, *end_line) {
                    keep.push(revision(e, store));
                }
            }
            keep
        }
        ViewMode::Project => store.entries().iter().map(|e| revision(e, store)).collect(),
        ViewMode::Recent => {
            let n = store.entries().len();
            store.entries().iter().skip(n.saturating_sub(100)).map(|e| revision(e, store)).collect()
        }
    };
    out.reverse();
    out
}

fn fmt_time(ms: u64) -> String {
    // Local time is not available without a time zone database; show UTC like the history file.
    let secs = ms / 1000;
    let (h, m, sec) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let days = secs / 86_400;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{sec:02} UTC")
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// "Just now", "5 min ago", "3 h ago", else the date and time.
pub fn when(ms: u64, now: u64) -> String {
    let ago = now.saturating_sub(ms) / 1000;
    match ago {
        0..=59 => "Just now".into(),
        60..=3599 => format!("{} min ago", ago / 60),
        3600..=86_399 => format!("{} h ago", ago / 3600),
        _ => fmt_time(ms),
    }
}

/// `"5 min ago · Saved"`.
pub fn describe(r: &Revision) -> String {
    let what = match (&r.kind, &r.label) {
        (EntryKind::Label, Some(l)) => format!("Label: {l}"),
        (k, _) => k.label().to_string(),
    };
    format!("{} · {what}", when(r.time_ms, now_ms()))
}

/// Open the history window.
pub(crate) fn show(s: &mut Session, mode: ViewMode) -> Result<(), String> {
    ensure_loaded(s);
    if s.local_history.store.is_none() {
        return Err("Local History needs an open project".into());
    }
    let title = match &mode {
        ViewMode::File { path } => format!("Local History: {}", path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
        ViewMode::Selection { path, start_line, end_line } => format!(
            "Local History: {} lines {}–{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            start_line.saturating_add(1),
            end_line.saturating_add(1)
        ),
        ViewMode::Project => "Local History: Project".into(),
        ViewMode::Recent => "Recent Changes".into(),
    };
    let revisions = revisions_for(s, &mode);
    s.local_history.view = Some(HistoryView {
        mode,
        title,
        revisions,
        selected: None,
        old_text: String::new(),
        new_text: String::new(),
        old_title: String::new(),
        new_title: String::new(),
        error: None,
    });
    if s.local_history.view.as_ref().is_some_and(|v| !v.revisions.is_empty()) {
        select(s, 0)?;
    }
    Ok(())
}

/// Recompute the open view's revisions (after a save or a label).
pub(crate) fn refresh_view(s: &mut Session) {
    let Some(v) = s.local_history.view.as_ref() else { return };
    let mode = v.mode.clone();
    let selected_id = v.selected.and_then(|i| v.revisions.get(i)).map(|r| r.id);
    let revisions = revisions_for(s, &mode);
    let index = selected_id.and_then(|id| revisions.iter().position(|r| r.id == id));
    if let Some(v) = s.local_history.view.as_mut() {
        v.revisions = revisions;
        v.selected = None;
    }
    if let Some(i) = index {
        let _ = select(s, i);
    }
}

/// Select a revision and load its diff.
pub(crate) fn select(s: &mut Session, index: usize) -> Result<(), String> {
    let view = s.local_history.view.as_ref().ok_or("the Local History window is not open")?;
    let r = view.revisions.get(index).cloned().ok_or_else(|| format!("no revision {index}"))?;
    let file_mode = matches!(view.mode, ViewMode::File { .. } | ViewMode::Selection { .. });
    let store = s.local_history.store.as_ref().ok_or("no project is open")?;
    let (old, new, old_title, new_title, error) = match (&r.kind, &r.path) {
        (EntryKind::Label, _) | (_, None) => (String::new(), String::new(), String::new(), String::new(), None),
        (_, Some(path)) => match store.content(r.id) {
            Ok(content) if file_mode => (content, current_text(s, path), describe(&r), "Current".into(), None),
            Ok(content) => {
                // Project views show what the revision changed.
                let prev = store.previous(r.id);
                let old = prev.and_then(|p| store.content(p.id).ok()).unwrap_or_default();
                let old_title = prev.map(|p| describe(&revision(p, store))).unwrap_or_else(|| "(none)".into());
                (old, content, old_title, describe(&r), None)
            }
            Err(e) => (String::new(), String::new(), String::new(), String::new(), Some(e)),
        },
    };
    if let Some(v) = s.local_history.view.as_mut() {
        v.selected = Some(index);
        v.old_text = old;
        v.new_text = new;
        v.old_title = old_title;
        v.new_title = new_title;
        v.error = error;
    }
    Ok(())
}

/// Restore the file of revision `id` to that revision (an undoable edit when the file is open).
pub(crate) fn revert(s: &mut Session, id: u64) -> Result<PathBuf, String> {
    ensure_loaded(s);
    let store = s.local_history.store.as_ref().ok_or("no project is open")?;
    let entry = store.entry(id).ok_or_else(|| format!("no local history entry {id}"))?;
    let path = store.abs(entry).ok_or("a label cannot be reverted to")?;
    let content = store.content(id)?;
    if s.ws.doc_by_path(&path).is_some() {
        s.open_file(&path).map_err(|e| e.to_string())?;
        s.edit_active(codecraft_text::EditGroup::Standalone, |doc, sels, _| {
            crate::reformat::diff_edit(doc, &content, sels)
        })
        .map_err(|e| e.to_string())?;
    } else {
        if let Ok(text) = codecraft_fs::read_text_file(&path) {
            record(s, &path, &text, EntryKind::External);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        codecraft_fs::write_text_file_atomic(&path, &content).map_err(|e| e.to_string())?;
        record(s, &path, &content, EntryKind::Reverted);
    }
    refresh_view(s);
    Ok(path)
}

pub(crate) fn pump(s: &mut Session) -> bool {
    ensure_loaded(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000_000;

    #[test]
    fn records_dedupes_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("src/a.rs");
        let mut st = HistoryStore::open(root, T0);
        let a = st.record(&file, "one\n", EntryKind::External, T0).unwrap().unwrap();
        assert_eq!(st.entry(a).unwrap().kind, EntryKind::Original);
        assert_eq!(st.record(&file, "one\n", EntryKind::Saved, T0 + 1).unwrap(), None, "same content");
        let b = st.record(&file, "two\n", EntryKind::Saved, T0 + 2).unwrap().unwrap();
        assert_eq!(st.content(a).unwrap(), "one\n");
        assert_eq!(st.content(b).unwrap(), "two\n");
        assert_eq!(st.previous(b).unwrap().id, a);
        // Outside the project and the private directory are ignored.
        assert_eq!(st.record(Path::new("/elsewhere/x.rs"), "x", EntryKind::Saved, T0).unwrap(), None);
        assert_eq!(st.record(&root.join(".codecraft/x"), "x", EntryKind::Saved, T0).unwrap(), None);
        let l = st.put_label("before refactoring", T0 + 3).unwrap();
        assert!(st.put_label("   ", T0).is_err());
        assert!(st.content(l).is_err());
        // Reopen: the index persisted.
        let st2 = HistoryStore::open(root, T0 + 10);
        assert_eq!(st2.entries().len(), 3);
        assert_eq!(st2.file_entries(&file).len(), 2);
    }

    #[test]
    fn prunes_by_age_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut st = HistoryStore::open(root, T0);
        st.record(&root.join("old.txt"), "old", EntryKind::Saved, T0).unwrap();
        st.record(&root.join("new.txt"), "new", EntryKind::Saved, T0 + MAX_AGE_MS).unwrap();
        st.prune(T0 + MAX_AGE_MS + 1);
        assert_eq!(st.entries().len(), 1);
        let blobs: Vec<_> = std::fs::read_dir(HistoryStore::dir_for(root).join("blobs")).unwrap().collect();
        assert_eq!(blobs.len(), 1, "the old blob was deleted");
        // Oversized content is not recorded.
        let big = "x".repeat(MAX_FILE_BYTES as usize + 1);
        assert_eq!(st.record(&root.join("big.txt"), &big, EntryKind::Saved, T0).unwrap(), None);
    }

    #[test]
    fn corrupt_and_hostile_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hdir = HistoryStore::dir_for(root);
        std::fs::create_dir_all(&hdir).unwrap();
        std::fs::write(hdir.join("index.json"), "{oops").unwrap();
        let st = HistoryStore::open(root, T0);
        assert!(st.entries().is_empty());
        let text = format!(
            r#"{{"entries":[
                {{"id":1,"time_ms":{T0},"kind":"Saved","path":"../../etc/passwd","blob":"00","size":1}},
                {{"id":2,"time_ms":{T0},"kind":"Saved","path":"a.txt","blob":"../../x","size":1}},
                {{"id":3,"time_ms":{T0},"kind":"Saved","path":"/abs","blob":"00","size":1}},
                {{"id":18446744073709551615,"time_ms":{T0},"kind":"Saved","path":"a.txt","blob":"0f","size":1}}
            ]}}"#
        );
        std::fs::write(hdir.join("index.json"), text).unwrap();
        let mut st = HistoryStore::open(root, T0);
        assert_eq!(st.entries().len(), 1);
        assert!(st.content(u64::MAX).is_err(), "missing blob is an error, not a panic");
        // The id counter saturates instead of overflowing.
        st.record(&root.join("b.txt"), "b", EntryKind::Saved, T0).unwrap();
    }

    #[test]
    fn selection_filter() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\n";
        assert!(touches_lines(old, new, 1, 1));
        assert!(!touches_lines(old, new, 3, 3));
        assert!(touches_lines("a\nc\n", "a\nb\nc\n", 0, 1));
    }

    #[test]
    fn dates() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_time(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        let _ = fmt_time(u64::MAX);
        assert_eq!(when(T0, T0 + 5_000), "Just now");
        assert_eq!(when(T0, T0 + 300_000), "5 min ago");
        assert_eq!(when(T0 + 10, T0), "Just now");
    }
}
