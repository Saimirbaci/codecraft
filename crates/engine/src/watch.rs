//! Live file watching for the open project (RustRover runs `fsnotifier` for this): changes made
//! by a terminal, git or an agent reload unmodified open files, refresh the project tree and
//! tell version control.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codecraft_fs::ProjectWatcher;

use crate::Session;

/// Batch changes until the file system has been quiet this long…
const QUIET: Duration = Duration::from_millis(150);
/// …or until the oldest pending change is this old.
const MAX_DELAY: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct Watch {
    watcher: Option<ProjectWatcher>,
    pending: BTreeSet<PathBuf>,
    first_pending: Option<Instant>,
    last_event: Option<Instant>,
}

impl Watch {
    pub(crate) fn is_active(&self) -> bool {
        self.watcher.is_some()
    }
}

impl Session {
    /// Whether the project's files are being watched for external changes.
    pub fn is_watching(&self) -> bool {
        self.watch.is_active()
    }

    /// Start watching the project root (replacing a previous watch).
    pub(crate) fn start_watching(&mut self, root: &Path) {
        let excludes = codecraft_fs::ScanOptions::default().exclude_dirs;
        match ProjectWatcher::start(root, &excludes) {
            Ok(w) => self.watch.watcher = Some(w),
            Err(e) => log::warn!("file watching is off: {e}"),
        }
        self.watch.pending.clear();
        self.watch.first_pending = None;
    }

    pub(crate) fn stop_watching(&mut self) {
        self.watch = Watch::default();
    }

    /// Reload an open document from disk if the user has no unsaved edits in it.
    /// Returns true if it changed.
    pub(crate) fn reload_if_unmodified(&mut self, path: &Path) -> bool {
        let Some(id) = self.ws.doc_by_path(path) else { return false };
        let Some(od) = self.ws.doc(id) else { return false };
        if od.doc.is_modified() {
            return false;
        }
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == od.disk_mtime {
            return false;
        }
        let Ok(text) = codecraft_fs::read_text_file(path) else { return false };
        let editor = self.ws.editor_for_doc(id);
        let sels = editor.and_then(|e| self.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        let mut changed = false;
        if let Some(od) = self.ws.doc_mut(id) {
            if od.doc.text() != text.replace("\r\n", "\n") && od.doc.replace_all(&text, &sels).is_ok() {
                od.doc.mark_saved();
                changed = true;
            }
            od.disk_mtime = mtime;
        }
        if let Some(e) = editor {
            let len = self.ws.doc(id).map(|d| d.doc.len_chars()).unwrap_or(0);
            self.ws.set_selections(e, sels.clamp(len));
        }
        changed
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(w) = &s.watch.watcher else { return false };
    let fresh = w.drain();
    let now = Instant::now();
    if !fresh.is_empty() {
        s.watch.last_event = Some(now);
        s.watch.first_pending.get_or_insert(now);
        s.watch.pending.extend(fresh);
    }
    let due = s.watch.first_pending.is_some_and(|t| now.duration_since(t) >= MAX_DELAY)
        || s.watch.last_event.is_some_and(|t| now.duration_since(t) >= QUIET);
    if s.watch.pending.is_empty() || !due {
        return false;
    }
    let paths: Vec<PathBuf> = std::mem::take(&mut s.watch.pending).into_iter().collect();
    s.watch.first_pending = None;
    let mut changed = false;
    for p in &paths {
        changed |= s.reload_if_unmodified(p);
    }
    // Created or deleted files change the project tree.
    let tree_changed = s.ws.project.as_ref().is_some_and(|proj| {
        paths.iter().any(|p| {
            let in_tree = proj.tree.find(p).is_some();
            let exists = p.exists();
            // Local history snapshots are written on every save; they never need a rescan.
            let snapshot = p.starts_with(crate::local_history::HistoryStore::dir_for(&proj.root));
            in_tree != exists && !snapshot && !p.components().any(|c| c.as_os_str() == ".git")
        })
    });
    if tree_changed {
        s.jobs_rescan();
        changed = true;
    }
    crate::vcs::files_changed(s, &paths);
    crate::todo::files_changed(s, &paths);
    crate::local_history::files_changed(s, &paths);
    changed
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    fn pump_until(s: &mut Session, mut done: impl FnMut(&Session) -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(15) {
            s.pump();
            if done(s) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        false
    }

    #[test]
    fn external_edits_reload_and_new_files_appear() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        assert!(
            pump_until(&mut s, |s| s.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()) && s.watch.is_active())
        );
        s.execute("file.open", &json!({ "path": file })).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        // An external edit (e.g. from a terminal or an agent) reloads the unmodified document.
        std::fs::write(&file, "two\n").unwrap();
        assert!(pump_until(&mut s, |s| s.ws.active().is_some_and(|(_, d)| d.doc.text() == "two\n")));
        // A new file shows up in the project tree.
        let new = root.join("b.txt");
        std::fs::write(&new, "b").unwrap();
        assert!(pump_until(&mut s, |s| s.ws.project.as_ref().is_some_and(|p| p.tree.find(&new).is_some())));
        // Unsaved edits are never clobbered.
        s.execute("editor.type", &json!({ "text": "mine " })).unwrap();
        std::fs::write(&file, "three\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));
        for _ in 0..20 {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(s.ws.active().is_some_and(|(_, d)| d.doc.text().starts_with("mine ")));
        s.shutdown();
    }
}
