//! Conservative three-way refresh of a local SSH mirror. Run on a worker thread.
//!
//! Incoming is an incremental rsync snapshot; baseline contains the last accepted contents.
//! Local-only changes remain local. Two-sided changes stay untouched and are reported.
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{RemoteError, RemoteResult};

pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const EXCLUDED: &[&str] = &[".codecraft", ".git", "target", "node_modules", "__pycache__", ".claude", ".cache"];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refresh {
    pub updated: usize,
    pub deleted: usize,
    pub conflicts: Vec<PathBuf>,
    #[serde(default)]
    pub changed: Vec<PathBuf>,
}

fn io(e: std::io::Error) -> RemoteError {
    RemoteError::Io(e.to_string())
}

type Entries = (BTreeSet<PathBuf>, BTreeSet<PathBuf>);

fn files(root: &Path) -> RemoteResult<Entries> {
    let mut out = BTreeSet::new();
    let mut folders = BTreeSet::new();
    let mut dirs = vec![PathBuf::new()];
    let mut count = 0usize;
    while let Some(rel) = dirs.pop() {
        if rel.components().count() > 256 {
            return Err(RemoteError::Invalid("remote project exceeds 256 folder levels".into()));
        }
        for entry in fs::read_dir(root.join(&rel)).map_err(io)? {
            let entry = entry.map_err(io)?;
            count = count.saturating_add(1);
            if count > MAX_ENTRIES {
                return Err(RemoteError::Invalid("remote project exceeds 100000 entries; add exclusions".into()));
            }
            if EXCLUDED.iter().any(|n| entry.file_name() == *n) {
                continue;
            }
            let ty = entry.file_type().map_err(io)?;
            let path = rel.join(entry.file_name());
            if ty.is_dir() {
                folders.insert(path.clone());
                dirs.push(path);
            } else if ty.is_file() {
                if entry.metadata().map_err(io)?.len() > MAX_FILE_BYTES {
                    return Err(RemoteError::Invalid(format!("{} exceeds the 50 MiB mirror limit", path.display())));
                }
                out.insert(path);
            } else {
                // Do not follow remote symlinks out of the mirror or permit special files.
                return Err(RemoteError::Invalid(format!("{} is not a regular file or directory", path.display())));
            }
        }
    }
    Ok((out, folders))
}

/// Reject symlinks and file/directory collisions in *every* local path component.
fn safe_path(root: &Path, rel: &Path) -> RemoteResult<PathBuf> {
    let mut path = root.to_path_buf();
    for part in std::iter::once(Path::new("")).chain(rel.ancestors()) {
        match fs::symlink_metadata(root.join(part)) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(RemoteError::Invalid(format!("refusing mirror symlink {}", root.join(part).display())));
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
            _ => {}
        }
    }
    path.push(rel);
    Ok(path)
}

fn equal(a: &Path, b: &Path) -> RemoteResult<bool> {
    let open = |p: &Path| -> RemoteResult<Option<File>> {
        match fs::symlink_metadata(p) {
            Ok(m) if m.is_file() && m.len() <= MAX_FILE_BYTES => File::open(p).map(Some).map_err(io),
            Ok(_) => Err(RemoteError::Invalid(format!("{} is not a bounded regular file", p.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(e)),
        }
    };
    match (open(a)?, open(b)?) {
        (None, None) => Ok(true),
        (Some(mut a), Some(mut b)) => {
            let mut ab = [0; 16384];
            let mut bb = [0; 16384];
            let mut total = 0u64;
            loop {
                let an = a.read(&mut ab).map_err(io)?;
                let bn = b.read(&mut bb).map_err(io)?;
                total = total.saturating_add(an as u64);
                if total > MAX_FILE_BYTES {
                    return Err(RemoteError::Invalid("file grew beyond the mirror limit".into()));
                }
                if an != bn || ab.get(..an) != bb.get(..bn) {
                    return Ok(false);
                }
                if an == 0 {
                    return Ok(true);
                }
            }
        }
        _ => Ok(false),
    }
}

fn replace(source: &Path, dest: &Path) -> RemoteResult<()> {
    if !source.exists() {
        match fs::remove_file(dest) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io(e)),
        }
    }
    let parent = dest.parent().ok_or_else(|| RemoteError::Invalid("mirror file has no parent".into()))?;
    fs::create_dir_all(parent).map_err(io)?;
    let tmp =
        dest.with_file_name(format!(".{}.codecraft-refresh", dest.file_name().unwrap_or_default().to_string_lossy()));
    let mut to = OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(io)?;
    let result = (|| {
        let mut from = File::open(source).map_err(io)?.take(MAX_FILE_BYTES.saturating_add(1));
        let bytes = std::io::copy(&mut from, &mut to).map_err(io)?;
        if bytes > MAX_FILE_BYTES {
            return Err(RemoteError::Invalid("file grew beyond the mirror limit".into()));
        }
        to.flush().map_err(io)?;
        fs::set_permissions(&tmp, fs::metadata(source).map_err(io)?.permissions()).map_err(io)?;
        fs::rename(&tmp, dest).map_err(io)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Capture a regular file for a checked upload, preserving exactly the bytes sent as baseline.
pub fn capture(source: &Path, dest: &Path) -> RemoteResult<()> {
    let meta = fs::symlink_metadata(source).map_err(io)?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(RemoteError::Invalid(format!(
            "{} is not a regular file within the 50 MiB limit",
            source.display()
        )));
    }
    replace(source, dest)
}

/// Refresh without silently overwriting local changes. Conflicting incoming files remain in
/// `incoming` for inspection; a later refresh resolves a conflict when both copies agree.
/// Callers must serialize refreshes and saves for the same mirror.
pub fn refresh(local: &Path, incoming: &Path, baseline: &Path) -> RemoteResult<Refresh> {
    refresh_protected(local, incoming, baseline, &BTreeSet::new())
}

/// Dirty editor paths, relative to the mirror, are never changed or accepted as a baseline.
pub fn refresh_protected(
    local: &Path,
    incoming: &Path,
    baseline: &Path,
    protected: &BTreeSet<PathBuf>,
) -> RemoteResult<Refresh> {
    refresh_coordinated(local, incoming, baseline, |_, apply| apply(protected))
}

/// One file/directory transaction, called under the owner's short-lived mirror guard.
pub type MirrorApply<'a> = dyn FnMut(&BTreeSet<PathBuf>) -> RemoteResult<()> + 'a;

/// Scan on the worker without the UI guard; coordinate each apply separately, resampling
/// dirty buffers at every file. The caller must serialize transfers/baseline writers.
pub fn refresh_coordinated(
    local: &Path,
    incoming: &Path,
    baseline: &Path,
    mut coordinate: impl FnMut(&Path, &mut MirrorApply<'_>) -> RemoteResult<()>,
) -> RemoteResult<Refresh> {
    fs::create_dir_all(local).map_err(io)?;
    fs::create_dir_all(baseline).map_err(io)?;
    safe_path(local, Path::new(""))?;
    safe_path(incoming, Path::new(""))?;
    safe_path(baseline, Path::new(""))?;
    let (remote_files, remote_dirs) = files(incoming)?;
    let (base_files, base_dirs) = files(baseline)?;
    let paths: BTreeSet<_> = remote_files.union(&base_files).cloned().collect();
    let mut report = Refresh::default();
    for rel in &remote_dirs {
        coordinate(rel, &mut |_| {
            let here = match safe_path(local, rel) {
                Ok(path) => path,
                Err(_) => {
                    report.conflicts.push(rel.clone());
                    return Ok(());
                }
            };
            if here.is_dir() || (!here.exists() && !base_dirs.contains(rel)) {
                if !here.exists() { report.changed.push(rel.clone()); }
                fs::create_dir_all(&here).map_err(io)?;
                fs::create_dir_all(baseline.join(rel)).map_err(io)?;
            } else if here.exists() {
                report.conflicts.push(rel.clone());
            }
            Ok(())
        })?;
    }
    for rel in paths {
        coordinate(&rel, &mut |protected| {
            let here = match safe_path(local, &rel) {
                Ok(p) => p,
                Err(_) => {
                    report.conflicts.push(rel.clone());
                    return Ok(());
                }
            };
            let there = incoming.join(&rel);
            let base = baseline.join(&rel);
            if protected.contains(&rel) {
                if !equal(&there, &base)? {
                    report.conflicts.push(rel.clone());
                }
                return Ok(());
            }
            let same = equal(&here, &there).unwrap_or(false);
            if same {
                if !equal(&there, &base)? { replace(&there, &base)?; }
            } else if equal(&here, &base).unwrap_or(false) {
                replace(&there, &here)?;
                replace(&there, &base)?;
                report.changed.push(rel.clone());
                if remote_files.contains(&rel) {
                    report.updated = report.updated.saturating_add(1);
                } else {
                    report.deleted = report.deleted.saturating_add(1);
                }
            } else if !equal(&there, &base)? {
                report.conflicts.push(rel.clone());
            }
            Ok(())
        })?;
    }
    // Remove only empty directories, deepest first; never remove a local-only file recursively.
    for rel in base_dirs.difference(&remote_dirs).collect::<Vec<_>>().into_iter().rev() {
        coordinate(rel, &mut |_| {
            if let Ok(here) = safe_path(local, rel)
                && fs::remove_dir(here).is_ok() { report.changed.push(rel.clone()); }
            let _ = fs::remove_dir(baseline.join(rel));
            Ok(())
        })?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinated_refresh_resamples_dirty_paths_between_file_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let incoming = dir.path().join("incoming");
        let baseline = dir.path().join("baseline");
        for root in [&local, &incoming, &baseline] { fs::create_dir_all(root).unwrap(); }
        for name in ["a", "b"] {
            fs::write(local.join(name), "old").unwrap();
            fs::write(baseline.join(name), "old").unwrap();
            fs::write(incoming.join(name), "new").unwrap();
        }
        let mut calls = 0;
        let report = refresh_coordinated(&local, &incoming, &baseline, |_, apply| {
            calls += 1;
            let protected = if calls == 2 { BTreeSet::from([PathBuf::from("b")]) } else { BTreeSet::new() };
            apply(&protected)
        }).unwrap();
        assert_eq!(calls, 2);
        assert_eq!(fs::read_to_string(local.join("a")).unwrap(), "new");
        assert_eq!(fs::read_to_string(local.join("b")).unwrap(), "old");
        assert_eq!(fs::read_to_string(baseline.join("b")).unwrap(), "old");
        assert_eq!(report.conflicts, [PathBuf::from("b")]);
    }

    #[test]
    fn unchanged_refresh_preserves_baseline_and_empty_directory_changes_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let incoming = dir.path().join("incoming");
        let baseline = dir.path().join("baseline");
        fs::create_dir_all(&incoming).unwrap();
        fs::write(incoming.join("a"), "same").unwrap();
        refresh(&local, &incoming, &baseline).unwrap();
        let before = fs::metadata(baseline.join("a")).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(refresh(&local, &incoming, &baseline).unwrap().changed.is_empty());
        assert_eq!(fs::metadata(baseline.join("a")).unwrap().modified().unwrap(), before);
        fs::create_dir(incoming.join("empty")).unwrap();
        assert_eq!(refresh(&local, &incoming, &baseline).unwrap().changed, [PathBuf::from("empty")]);
        fs::remove_dir(incoming.join("empty")).unwrap();
        assert_eq!(refresh(&local, &incoming, &baseline).unwrap().changed, [PathBuf::from("empty")]);
    }

    #[test]
    fn unsaved_buffer_paths_keep_disk_and_baseline_until_explicit_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let (local, incoming, base) = (dir.path().join("l"), dir.path().join("r"), dir.path().join("b"));
        fs::create_dir_all(&incoming).unwrap();
        fs::write(incoming.join("a"), "base").unwrap();
        refresh(&local, &incoming, &base).unwrap();
        fs::write(incoming.join("a"), "new remote").unwrap();
        let protected = BTreeSet::from([PathBuf::from("a")]);
        let report = refresh_protected(&local, &incoming, &base, &protected).unwrap();
        assert_eq!(report.conflicts, vec![PathBuf::from("a")]);
        assert!(report.changed.is_empty());
        assert_eq!(fs::read_to_string(local.join("a")).unwrap(), "base");
        assert_eq!(fs::read_to_string(base.join("a")).unwrap(), "base");
        fs::remove_file(incoming.join("a")).unwrap();
        assert_eq!(refresh_protected(&local, &incoming, &base, &protected).unwrap().conflicts.len(), 1);
        let report = refresh(&local, &incoming, &base).unwrap();
        assert_eq!(report.changed, vec![PathBuf::from("a")]);
        assert_eq!(report.deleted, 1);
    }

    #[test]
    fn refresh_preserves_local_changes_and_detects_conflicts_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let remote = dir.path().join("remote");
        let base = dir.path().join("base");
        fs::create_dir_all(&remote).unwrap();
        fs::write(remote.join("a"), "one").unwrap();
        assert_eq!(refresh(&local, &remote, &base).unwrap().updated, 1);
        assert_eq!(refresh(&local, &remote, &base).unwrap().updated, 0);
        fs::write(local.join("a"), "local").unwrap();
        assert!(refresh(&local, &remote, &base).unwrap().conflicts.is_empty());
        fs::write(remote.join("a"), "remote").unwrap();
        assert_eq!(refresh(&local, &remote, &base).unwrap().conflicts, vec![PathBuf::from("a")]);
        assert_eq!(fs::read_to_string(local.join("a")).unwrap(), "local");
        fs::copy(remote.join("a"), local.join("a")).unwrap();
        assert!(refresh(&local, &remote, &base).unwrap().conflicts.is_empty());
        fs::remove_file(remote.join("a")).unwrap();
        assert_eq!(refresh(&local, &remote, &base).unwrap().deleted, 1);
        assert!(!local.join("a").exists());
    }

    #[test]
    fn local_deletes_and_legacy_cache_edits_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let (local, remote, base) = (dir.path().join("l"), dir.path().join("r"), dir.path().join("b"));
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&local).unwrap();
        fs::write(remote.join("a"), "remote").unwrap();
        fs::write(local.join("a"), "legacy local edit").unwrap();
        assert_eq!(refresh(&local, &remote, &base).unwrap().conflicts.len(), 1);
        fs::copy(remote.join("a"), local.join("a")).unwrap();
        refresh(&local, &remote, &base).unwrap();
        fs::remove_file(local.join("a")).unwrap();
        assert_eq!(refresh(&local, &remote, &base).unwrap().updated, 0);
        assert!(!local.join("a").exists());
    }

    #[test]
    fn empty_folders_and_local_only_files_survive_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let (local, remote, base) = (dir.path().join("l"), dir.path().join("r"), dir.path().join("b"));
        fs::create_dir_all(remote.join("empty/nested")).unwrap();
        refresh(&local, &remote, &base).unwrap();
        assert!(local.join("empty/nested").is_dir());
        fs::write(local.join("empty/only-here"), "local").unwrap();
        fs::remove_dir_all(remote.join("empty")).unwrap();
        refresh(&local, &remote, &base).unwrap();
        assert!(!local.join("empty/nested").exists());
        assert_eq!(fs::read_to_string(local.join("empty/only-here")).unwrap(), "local");
    }

    #[test]
    fn oversized_input_is_an_error_and_does_not_change_the_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (local, remote, base) = (dir.path().join("l"), dir.path().join("r"), dir.path().join("b"));
        fs::create_dir_all(&remote).unwrap();
        File::create(remote.join("large")).unwrap().set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(refresh(&local, &remote, &base).unwrap_err().to_string().contains("50 MiB"));
        assert!(!local.join("large").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_write_outside_the_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let (local, remote, base, outside) =
            (dir.path().join("l"), dir.path().join("r"), dir.path().join("b"), dir.path().join("outside"));
        for p in [&local, &remote.join("sub"), &outside] {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(remote.join("sub/a"), "remote").unwrap();
        std::os::unix::fs::symlink(&outside, local.join("sub")).unwrap();
        let conflicts = refresh(&local, &remote, &base).unwrap().conflicts;
        assert!(conflicts.contains(&PathBuf::from("sub/a")));
        assert!(!outside.join("a").exists());
        std::os::unix::fs::symlink(&outside, remote.join("link")).unwrap();
        assert!(refresh(&local, &remote, &base).is_err());
    }
}
