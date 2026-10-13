//! Query history and saved (favourite) queries per data source.
//!
//! Each project's data source has a JSON file under `$CODECRAFT_HOME/database/history/`
//! (default `~/.codecraft`): `<project hash>-<data source id>.json`. Files load on first use and
//! are written by a background thread after changes (never on the UI thread).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Sender, unbounded};
use serde::{Deserialize, Serialize};

/// Entries kept per data source (the oldest are dropped).
pub const MAX_HISTORY: usize = 500;
/// History files larger than this are not read (and are set aside).
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Statements longer than this are not recorded.
const MAX_SQL_CHARS: usize = 100_000;

/// One executed statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub sql: String,
    /// When it last ran (Unix seconds).
    pub at: u64,
    /// How many times it ran.
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

/// A query saved under a name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub name: String,
    pub sql: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct HistoryFile {
    version: u32,
    /// Oldest first.
    entries: Vec<HistoryEntry>,
    saved: Vec<SavedQuery>,
}

/// The query history popup of a console.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPopup {
    pub source: String,
    /// Words that entries must contain (edited by the UI in place).
    pub filter: String,
}

/// History files of the current project, loaded lazily.
#[derive(Default)]
pub struct HistoryStore {
    dir_override: Option<PathBuf>,
    project: Option<PathBuf>,
    files: HashMap<String, HistoryFile>,
    dirty: BTreeSet<String>,
    writer: Option<Sender<(PathBuf, String)>>,
    /// The last load problem (a corrupt file is set aside as `.json.bad`).
    pub error: Option<String>,
}

/// FNV-1a: a stable hash of the project path for file names.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl HistoryStore {
    /// Keep history files in `dir` (tests and embedders).
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.flush();
        self.dir_override = Some(dir);
        self.files.clear();
    }

    pub(crate) fn set_project(&mut self, root: Option<PathBuf>) {
        self.flush();
        self.project = root;
        self.files.clear();
    }

    /// Where history files live.
    pub fn dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.dir_override {
            return Some(d.clone());
        }
        let home = match std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            Some(h) => PathBuf::from(h),
            None => {
                let home =
                    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
                PathBuf::from(home).join(".codecraft")
            }
        };
        Some(home.join("database").join("history"))
    }

    /// The history file of a data source of the current project.
    pub fn file_path(&self, source: &str) -> Option<PathBuf> {
        let project = self.project.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        let mut id: String = source
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .take(60)
            .collect();
        if id.is_empty() {
            id = "_".into();
        }
        Some(self.dir()?.join(format!("{:016x}-{id}.json", fnv1a(&project))))
    }

    fn file(&mut self, source: &str) -> &mut HistoryFile {
        if !self.files.contains_key(source) {
            let loaded = self.load(source);
            self.files.insert(source.to_string(), loaded);
        }
        self.files.entry(source.to_string()).or_default()
    }

    fn load(&mut self, source: &str) -> HistoryFile {
        let Some(path) = self.file_path(source) else { return HistoryFile::default() };
        let Ok(meta) = std::fs::metadata(&path) else { return HistoryFile::default() };
        let parsed = if meta.len() > MAX_FILE_BYTES {
            Err("too large".to_string())
        } else {
            std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<HistoryFile>(&t).map_err(|e| e.to_string()))
        };
        match parsed {
            Ok(mut f) => {
                f.entries.retain(|e| !e.sql.trim().is_empty());
                let extra = f.entries.len().saturating_sub(MAX_HISTORY);
                f.entries.drain(..extra);
                f
            }
            Err(e) => {
                let backup = path.with_extension("json.bad");
                let _ = std::fs::rename(&path, &backup);
                self.error = Some(format!("{} is not valid ({e}); moved it to {}", path.display(), backup.display()));
                HistoryFile::default()
            }
        }
    }

    /// Record an executed statement (a repeat moves to the top and counts up).
    pub fn record(&mut self, source: &str, sql: &str) {
        let sql = sql.trim();
        if sql.is_empty() || sql.chars().count() > MAX_SQL_CHARS {
            return;
        }
        let f = self.file(source);
        let count = match f.entries.iter().position(|e| e.sql == sql) {
            Some(i) => f.entries.remove(i).count.saturating_add(1),
            None => 1,
        };
        f.entries.push(HistoryEntry { sql: sql.to_string(), at: now(), count });
        let extra = f.entries.len().saturating_sub(MAX_HISTORY);
        f.entries.drain(..extra);
        self.dirty.insert(source.to_string());
    }

    /// Entries, newest first, that contain every word of `filter` (case-insensitive).
    pub fn entries(&mut self, source: &str, filter: &str) -> Vec<HistoryEntry> {
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        self.file(source)
            .entries
            .iter()
            .rev()
            .filter(|e| {
                let lower = e.sql.to_lowercase();
                words.iter().all(|w| lower.contains(w.as_str()))
            })
            .cloned()
            .collect()
    }

    pub fn clear(&mut self, source: &str) -> usize {
        let f = self.file(source);
        let n = f.entries.len();
        f.entries.clear();
        self.dirty.insert(source.to_string());
        n
    }

    pub fn saved(&mut self, source: &str) -> Vec<SavedQuery> {
        self.file(source).saved.clone()
    }

    /// Save (or replace) a named query.
    pub fn save_query(&mut self, source: &str, name: &str, sql: &str) {
        let f = self.file(source);
        let q = SavedQuery { name: name.trim().to_string(), sql: sql.trim().to_string() };
        match f.saved.iter_mut().find(|s| s.name == q.name) {
            Some(existing) => *existing = q,
            None => f.saved.push(q),
        }
        self.dirty.insert(source.to_string());
    }

    pub fn remove_saved(&mut self, source: &str, name: &str) -> bool {
        let f = self.file(source);
        let before = f.saved.len();
        f.saved.retain(|s| s.name != name);
        let removed = f.saved.len() != before;
        if removed {
            self.dirty.insert(source.to_string());
        }
        removed
    }

    /// Hand changed files to the writer thread.
    pub(crate) fn flush(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let dirty = std::mem::take(&mut self.dirty);
        for source in dirty {
            let (Some(path), Some(file)) = (self.file_path(&source), self.files.get(&source)) else { continue };
            let file = HistoryFile { version: 1, ..file.clone() };
            let Ok(json) = serde_json::to_string_pretty(&file) else { continue };
            if let Some(w) = self.writer()
                && w.send((path, json)).is_err()
            {
                self.writer = None;
            }
        }
    }

    fn writer(&mut self) -> Option<&Sender<(PathBuf, String)>> {
        if self.writer.is_none() {
            let (tx, rx) = unbounded::<(PathBuf, String)>();
            let spawned = std::thread::Builder::new().name("db-history".into()).spawn(move || {
                for (path, json) in rx {
                    if let Err(e) = write(&path, &json) {
                        log::warn!("cannot save the query history {}: {e}", path.display());
                    }
                }
            });
            if spawned.is_ok() {
                self.writer = Some(tx);
            }
        }
        self.writer.as_ref()
    }
}

fn write(path: &Path, json: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    codecraft_fs::write_text_file_atomic(path, &format!("{json}\n")).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(path: &Path, needle: &str) -> String {
        for _ in 0..500 {
            if let Ok(t) = std::fs::read_to_string(path)
                && t.contains(needle)
            {
                return t;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{} never contained {needle}", path.display());
    }

    #[test]
    fn records_dedupes_filters_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = HistoryStore::default();
        h.set_dir(dir.path().to_path_buf());
        h.set_project(Some(PathBuf::from("/p")));
        h.record("pg", "select 1");
        h.record("pg", "  select 2 ");
        h.record("pg", "select 1");
        h.record("pg", "   ");
        let e = h.entries("pg", "");
        assert_eq!(e.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select 1", "select 2"]);
        assert_eq!(e[0].count, 2);
        assert_eq!(h.entries("pg", "SELECT 2").len(), 1);
        assert!(h.entries("other", "").is_empty());
        h.save_query("pg", "ones", "select 1");
        h.save_query("pg", "ones", "select 11");
        assert_eq!(h.saved("pg"), [SavedQuery { name: "ones".into(), sql: "select 11".into() }]);
        h.flush();
        let path = h.file_path("pg").unwrap();
        assert!(path.starts_with(dir.path()));
        wait_for(&path, "select 11");

        // Another store (a new session) reads it back; other projects have their own files.
        let mut h2 = HistoryStore::default();
        h2.set_dir(dir.path().to_path_buf());
        h2.set_project(Some(PathBuf::from("/p")));
        assert_eq!(h2.entries("pg", "").len(), 2);
        assert!(h2.remove_saved("pg", "ones"));
        assert!(!h2.remove_saved("pg", "ones"));
        assert_eq!(h2.clear("pg"), 2);
        h2.set_project(Some(PathBuf::from("/q")));
        assert!(h2.entries("pg", "").is_empty());
    }

    #[test]
    fn caps_entries_and_sets_corrupt_files_aside() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = HistoryStore::default();
        h.set_dir(dir.path().to_path_buf());
        for i in 0..MAX_HISTORY + 20 {
            h.record("s", &format!("select {i}"));
        }
        let e = h.entries("s", "");
        assert_eq!(e.len(), MAX_HISTORY);
        assert_eq!(e[0].sql, format!("select {}", MAX_HISTORY + 19));
        let path = h.file_path("../evil/../id").unwrap();
        assert_eq!(path.parent(), Some(dir.path()));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(h.entries("../evil/../id", "").is_empty());
        assert!(h.error.is_some());
        assert!(path.with_extension("json.bad").exists());
    }
}
