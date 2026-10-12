//! Watching a project directory for changes made outside the IDE (terminal, git, agents).

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crossbeam_channel::{Receiver, unbounded};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{FsError, Result};

/// Git metadata whose changes matter (commits, checkouts, staging) even though `.git` is
/// otherwise ignored.
fn is_interesting_git_path(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    s == ".git/HEAD"
        || s == ".git/index"
        || s.starts_with(".git/refs/")
        || s == ".git/MERGE_HEAD"
        || s == ".git/FETCH_HEAD"
}

/// A recursive watcher of a project directory. Changes arrive in batches through [`drain`].
///
/// [`drain`]: ProjectWatcher::drain
pub struct ProjectWatcher {
    root: PathBuf,
    exclude_dirs: Vec<String>,
    rx: Receiver<Vec<PathBuf>>,
    // Dropping the watcher stops the OS watch.
    _watcher: RecommendedWatcher,
}

impl ProjectWatcher {
    /// Start watching `root`. Paths under a directory named in `exclude_dirs` (e.g. `target`,
    /// `node_modules`) are dropped, except the interesting parts of `.git`.
    pub fn start(root: &Path, exclude_dirs: &[String]) -> Result<Self> {
        let (tx, rx) = unbounded();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                if matches!(ev.kind, EventKind::Access(_)) {
                    return;
                }
                let _ = tx.send(ev.paths);
            }
            Err(e) => log::debug!("file watcher: {e}"),
        })
        .map_err(|e| FsError::Other(format!("cannot watch {}: {e}", root.display())))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| FsError::Other(format!("cannot watch {}: {e}", root.display())))?;
        Ok(Self { root: root.to_path_buf(), exclude_dirs: exclude_dirs.to_vec(), rx, _watcher: watcher })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Changed paths since the last call (deduplicated, filtered, sorted).
    pub fn drain(&self) -> Vec<PathBuf> {
        let mut out = BTreeSet::new();
        while let Ok(paths) = self.rx.try_recv() {
            for p in paths {
                if self.keep(&p) {
                    out.insert(p);
                }
                if out.len() > 50_000 {
                    break;
                }
            }
        }
        out.into_iter().collect()
    }

    fn keep(&self, path: &Path) -> bool {
        // FSEvents reports canonical paths (/private/tmp vs /tmp); compare on both forms.
        let rel = path.strip_prefix(&self.root).ok().map(Path::to_path_buf).or_else(|| {
            std::fs::canonicalize(&self.root).ok().and_then(|r| path.strip_prefix(r).ok().map(Path::to_path_buf))
        });
        let Some(rel) = rel else { return false };
        if is_interesting_git_path(&rel) {
            return true;
        }
        !rel.components().any(|c| match c {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                name == ".git" || self.exclude_dirs.iter().any(|d| *d == name)
            }
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(w: &ProjectWatcher, mut pred: impl FnMut(&[PathBuf]) -> bool) -> Vec<PathBuf> {
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < Duration::from_secs(10) {
            seen.extend(w.drain());
            if pred(&seen) {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        seen
    }

    #[test]
    fn reports_changes_and_filters_excluded_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        let w = ProjectWatcher::start(&root, &["target".to_string()]).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(root.join("target/junk.o"), "x").unwrap();
        std::fs::write(root.join(".git/objects-like"), "x").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        let seen =
            wait_for(&w, |s| s.iter().any(|p| p.ends_with("main.rs")) && s.iter().any(|p| p.ends_with(".git/HEAD")));
        assert!(seen.iter().any(|p| p.ends_with("main.rs")), "{seen:?}");
        assert!(seen.iter().any(|p| p.ends_with(".git/HEAD")), "{seen:?}");
        assert!(!seen.iter().any(|p| p.ends_with("junk.o")), "{seen:?}");
        assert!(!seen.iter().any(|p| p.ends_with("objects-like")), "{seen:?}");
    }

    #[test]
    fn bad_root_is_an_error() {
        assert!(ProjectWatcher::start(Path::new("/definitely/not/here"), &[]).is_err());
    }
}
