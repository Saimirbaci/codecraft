//! Request history under `<project>/.codecraft/httpRequests/`: an index (`history.json`, the
//! last [`MAX_ENTRIES`] requests, newest first), each response body in its own file
//! (`2026-10-13T121314.200.json`) and `http-requests-log.http`, the requests as a `.http` file
//! with `<> response-file` references.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::HttpError;
use crate::env::write_text;
use crate::util;

pub const INDEX: &str = "history.json";
pub const LOG: &str = "http-requests-log.http";
pub const MAX_ENTRIES: usize = 50;
/// Request bodies longer than this are cut in the history.
const MAX_REQUEST_TEXT: usize = 64 << 10;
/// Largest index read.
const MAX_INDEX: u64 = 32 << 20;

/// Serializes index updates from concurrent workers.
static LOCK: Mutex<()> = Mutex::new(());

/// The history directory of a project.
pub fn dir(project_root: &Path) -> PathBuf {
    project_root.join(".codecraft").join("httpRequests")
}

/// One request in the history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix seconds.
    pub time: u64,
    #[serde(default)]
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    /// The request as sent, as `.http` text (long bodies cut).
    pub request: String,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// The response body's file name in the history directory.
    #[serde(default)]
    pub body_file: Option<String>,
}

/// Cut request text for the history.
pub fn request_text(text: &str) -> String {
    match text.char_indices().nth(MAX_REQUEST_TEXT) {
        Some((b, _)) => format!("{}\n… (cut)", text.get(..b).unwrap_or_default()),
        None => text.to_string(),
    }
}

/// A fresh temporary file name for a body being received.
pub fn temp_body_path(dir: &Path) -> PathBuf {
    let mut r = util::Rng::new();
    dir.join(format!("receiving-{:016x}.tmp", r.next()))
}

/// Rename a received body to `2026-10-13T121314.<status>.<ext>` (unique). Returns the name.
pub fn finish_body_file(dir: &Path, temp: &Path, time: u64, status: u16, ext: &str) -> Result<String, HttpError> {
    let (y, mo, d, h, mi, s) = util::civil(time);
    let stem = format!("{y:04}-{mo:02}-{d:02}T{h:02}{mi:02}{s:02}.{status}");
    let name = (0..1000)
        .map(|i| if i == 0 { format!("{stem}.{ext}") } else { format!("{stem}-{i}.{ext}") })
        .find(|n| !dir.join(n).exists())
        .ok_or_else(|| HttpError::Io("too many responses with the same time".into()))?;
    std::fs::rename(temp, dir.join(&name)).map_err(|e| HttpError::Io(format!("cannot save the response: {e}")))?;
    Ok(name)
}

/// The history, newest first (empty when missing or unreadable).
pub fn load(dir: &Path) -> Vec<HistoryEntry> {
    let p = dir.join(INDEX);
    if std::fs::metadata(&p).map(|m| m.len() > MAX_INDEX).unwrap_or(true) {
        return Vec::new();
    }
    std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Add `entry` (newest first), drop the oldest beyond [`MAX_ENTRIES`] with their body files, and
/// rewrite the index and the log. Returns the history.
pub fn record(dir: &Path, entry: HistoryEntry) -> Result<Vec<HistoryEntry>, HttpError> {
    let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    std::fs::create_dir_all(dir).map_err(|e| HttpError::Io(format!("cannot create {}: {e}", dir.display())))?;
    let mut entries = load(dir);
    entries.insert(0, entry);
    for old in entries.drain(MAX_ENTRIES.min(entries.len())..) {
        if let Some(f) = old.body_file.filter(|f| !f.contains(['/', '\\']) && !f.starts_with('.')) {
            let _ = std::fs::remove_file(dir.join(f));
        }
    }
    let json = serde_json::to_string_pretty(&entries).map_err(|e| HttpError::Io(e.to_string()))?;
    write_text(&dir.join(INDEX), &json)?;
    write_text(&dir.join(LOG), &log_text(&entries))?;
    Ok(entries)
}

/// Remove the whole history (index, log and body files).
pub fn clear(dir: &Path) -> Result<(), HttpError> {
    let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    for e in load(dir) {
        if let Some(f) = e.body_file.filter(|f| !f.contains(['/', '\\']) && !f.starts_with('.')) {
            let _ = std::fs::remove_file(dir.join(f));
        }
    }
    for f in [INDEX, LOG] {
        let p = dir.join(f);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| HttpError::Io(format!("cannot remove {}: {e}", p.display())))?;
        }
    }
    Ok(())
}

/// The history as a `.http` file.
pub fn log_text(entries: &[HistoryEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str("### ");
        out.push_str(&util::iso_timestamp(e.time));
        if let Some(n) = &e.name {
            out.push(' ');
            out.push_str(n);
        }
        out.push('\n');
        out.push_str(e.request.trim_end());
        out.push_str("\n\n");
        match (&e.body_file, &e.error) {
            (Some(f), _) => out.push_str(&format!("<> {f}\n\n")),
            (None, Some(err)) => out.push_str(&format!("# Failed: {}\n\n", err.replace('\n', " "))),
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: u64, body: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            time: 1_760_000_000 + i,
            method: "GET".into(),
            url: format!("http://h/{i}"),
            request: format!("GET http://h/{i}\n"),
            status: Some(200),
            body_file: body.map(str::to_string),
            ..HistoryEntry::default()
        }
    }

    #[test]
    fn records_trims_and_writes_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dir(tmp.path());
        let temp = temp_body_path(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&temp, "{}").unwrap();
        let name = finish_body_file(&dir, &temp, 1_760_358_896, 200, "json").unwrap();
        assert_eq!(name, "2025-10-13T123456.200.json");
        std::fs::write(&temp, "{}").unwrap();
        assert_eq!(finish_body_file(&dir, &temp, 1_760_358_896, 200, "json").unwrap(), "2025-10-13T123456.200-1.json");
        record(&dir, entry(0, Some(&name))).unwrap();
        for i in 1..=MAX_ENTRIES as u64 {
            record(&dir, entry(i, None)).unwrap();
        }
        let all = load(&dir);
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all[0].url, format!("http://h/{MAX_ENTRIES}"));
        // The dropped entry's body file is gone.
        assert!(!dir.join(&name).exists());
        let log = std::fs::read_to_string(dir.join(LOG)).unwrap();
        assert!(log.starts_with("### "));
        assert!(log.contains("GET http://h/1\n"));
        clear(&dir).unwrap();
        assert!(load(&dir).is_empty());
    }

    #[test]
    fn log_references_bodies_and_errors() {
        let mut failed = entry(1, None);
        failed.error = Some("connection refused".into());
        let log = log_text(&[entry(0, Some("a.200.json")), failed]);
        assert!(log.contains("<> a.200.json"));
        assert!(log.contains("# Failed: connection refused"));
        assert_eq!(request_text("abc"), "abc");
    }
}
