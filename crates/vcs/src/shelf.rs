//! The shelf: local changes set aside as patches, like RustRover's Shelve Changes. Unlike a git
//! stash, a shelf is plain files in the project (`.codecraft/shelf/<id>/`: one binary-safe patch
//! per file plus `shelf.json`), can hold any subset of files, and unshelves whole or file by file.
//!
//! Shelving writes the patches first and only then reverts the files, so a failure never loses
//! the changes. Unshelving applies each patch with `git apply`, falling back to a three-way apply
//! (conflicts are left for the merge view) when the file changed since.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Git, GitError, Result};

const META: &str = "shelf.json";
/// Shelves listed at most (oldest dropped from the list, not deleted).
pub const MAX_SHELVES: usize = 500;
/// Patches larger than this are not shown inline.
pub const MAX_PREVIEW: u64 = 4 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShelvedKind {
    /// Changed in the work tree or the index (in `HEAD`).
    #[default]
    Modified,
    /// New in the index (not in `HEAD`).
    Added,
    /// Not tracked by git.
    Unversioned,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShelvedFile {
    pub path: String,
    pub kind: ShelvedKind,
    /// The patch's file name in the shelf directory.
    pub patch: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shelf {
    pub id: String,
    pub name: String,
    pub created_ms: u64,
    /// `HEAD` when it was shelved.
    pub base: Option<String>,
    pub files: Vec<ShelvedFile>,
}

/// What to shelve.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShelveRequest {
    pub name: String,
    /// (path, kind) of each file.
    pub files: Vec<(String, ShelvedKind)>,
    pub created_ms: u64,
    /// Keep the changes in the work tree (copy them to the shelf only).
    pub keep: bool,
}

/// Whether a shelf id is safe to use as a directory name.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn io(path: &Path, e: std::io::Error) -> GitError {
    GitError::Io(format!("{}: {e}", path.display()))
}

fn shelf_dir(root: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        return Err(GitError::Invalid(format!("'{id}' is not a shelf")));
    }
    Ok(root.join(id))
}

fn write_meta(dir: &Path, shelf: &Shelf) -> Result<()> {
    let json = serde_json::to_string_pretty(shelf).map_err(|e| GitError::Parse(e.to_string()))?;
    let path = dir.join(META);
    let tmp = dir.join(format!("{META}.tmp"));
    std::fs::write(&tmp, json).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io(&path, e))
}

fn read_meta(dir: &Path) -> Option<Shelf> {
    let path = dir.join(META);
    let len = std::fs::metadata(&path).ok()?.len();
    if len > MAX_PREVIEW {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let mut s: Shelf = serde_json::from_str(&text).ok()?;
    s.id = dir.file_name()?.to_str()?.to_string();
    // Patch names come from the file: only plain names inside the shelf.
    s.files.retain(|f| !f.path.is_empty() && valid_patch_name(&f.patch));
    Some(s)
}

fn valid_patch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !name.starts_with('.')
}

/// The shelves under `root`, newest first. Broken entries are skipped.
pub fn list(root: &Path) -> Vec<Shelf> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<Shelf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_str().is_some_and(valid_id))
        .filter_map(|e| read_meta(&e.path()))
        .collect();
    out.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.id.cmp(&a.id)));
    out.truncate(MAX_SHELVES);
    out
}

/// Make sure git never sees the shelf (a `.gitignore` of `*` in its root).
fn ensure_root(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| io(root, e))?;
    let ignore = root.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n").map_err(|e| io(&ignore, e))?;
    }
    Ok(())
}

/// Shelve files: write a patch per file under `root/<new id>/`, then revert them (unless
/// `keep`). Returns the new shelf.
pub fn shelve(git: &Git, root: &Path, req: &ShelveRequest) -> Result<Shelf> {
    if req.files.is_empty() {
        return Err(GitError::Invalid("select the changes to shelve".into()));
    }
    let name = req.name.trim();
    let name = if name.is_empty() { "Shelved changes" } else { name };
    ensure_root(root)?;
    let mut id = req.created_ms.to_string();
    let mut n = 1;
    while root.join(&id).exists() {
        n += 1;
        id = format!("{}-{n}", req.created_ms);
        if n > 1000 {
            return Err(GitError::Io("cannot create a shelf directory".into()));
        }
    }
    let dir = root.join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
    let base = git
        .cmd(["rev-parse", "--verify", "-q", "HEAD"])
        .read_only()
        .text()
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let mut files = Vec::new();
    let result = (|| {
        for (i, (path, kind)) in req.files.iter().enumerate() {
            let patch = match kind {
                ShelvedKind::Unversioned => crate::ops::create_patch(git, &[], std::slice::from_ref(path))?,
                _ => crate::ops::create_patch(git, std::slice::from_ref(path), &[])?,
            };
            if patch.trim().is_empty() {
                continue;
            }
            let file = format!("{i:04}.patch");
            let p = dir.join(&file);
            std::fs::write(&p, patch).map_err(|e| io(&p, e))?;
            files.push(ShelvedFile { path: path.clone(), kind: *kind, patch: file });
        }
        if files.is_empty() {
            return Err(GitError::Invalid("the selected files have no changes to shelve".into()));
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    let shelf = Shelf { id, name: name.to_string(), created_ms: req.created_ms, base, files };
    write_meta(&dir, &shelf)?;
    if !req.keep {
        let mut tracked = Vec::new();
        let mut added = Vec::new();
        for f in &shelf.files {
            match f.kind {
                ShelvedKind::Modified => tracked.push(f.path.clone()),
                ShelvedKind::Added => added.push(f.path.clone()),
                ShelvedKind::Unversioned => {
                    let abs = git.dir().join(&f.path);
                    if abs.is_file() {
                        std::fs::remove_file(&abs).map_err(|e| io(&abs, e))?;
                    }
                }
            }
        }
        crate::ops::rollback(git, git.dir(), &tracked, &added, true)?;
    }
    Ok(shelf)
}

/// The patch text of one shelved file.
pub fn patch_text(root: &Path, id: &str, path: &str) -> Result<String> {
    let dir = shelf_dir(root, id)?;
    let shelf = read_meta(&dir).ok_or_else(|| GitError::Invalid(format!("there is no shelf '{id}'")))?;
    let f = shelf
        .files
        .iter()
        .find(|f| f.path == path)
        .ok_or_else(|| GitError::Invalid(format!("{path} is not in this shelf")))?;
    let p = dir.join(&f.patch);
    let len = std::fs::metadata(&p).map_err(|e| io(&p, e))?.len();
    if len > MAX_PREVIEW {
        return Err(GitError::TooLarge("the patch is too large to show".into()));
    }
    std::fs::read_to_string(&p).map_err(|e| io(&p, e))
}

/// Outcome of an unshelve.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unshelved {
    pub applied: Vec<String>,
    /// Applied with a three-way merge that left conflicts.
    pub conflicted: Vec<String>,
    /// (path, why) of patches that didn't apply.
    pub failed: Vec<(String, String)>,
    /// The shelf was emptied and removed.
    pub removed: bool,
}

/// Apply a shelf's patches (all, or only `paths`) to the work tree. Applied files leave the
/// shelf unless `keep`; an emptied shelf is deleted.
pub fn unshelve(git: &Git, root: &Path, id: &str, paths: Option<&[String]>, keep: bool) -> Result<Unshelved> {
    let dir = shelf_dir(root, id)?;
    let mut shelf = read_meta(&dir).ok_or_else(|| GitError::Invalid(format!("there is no shelf '{id}'")))?;
    let chosen: Vec<ShelvedFile> =
        shelf.files.iter().filter(|f| paths.is_none_or(|ps| ps.contains(&f.path))).cloned().collect();
    if chosen.is_empty() {
        return Err(GitError::Invalid("choose files of the shelf to unshelve".into()));
    }
    let mut out = Unshelved::default();
    for f in &chosen {
        let p = dir.join(&f.patch);
        let patch = std::fs::read(&p).map_err(|e| io(&p, e))?;
        if crate::ops::apply_patch(git, &patch, false).is_ok() {
            out.applied.push(f.path.clone());
            continue;
        }
        // The file changed since: merge three ways (conflicts stay in the file and the index).
        match git.cmd(["apply", "--3way", "--whitespace=nowarn", "-"]).stdin(patch.clone()).allow_exit(1).run() {
            Ok(o) if o.code == Some(0) => out.applied.push(f.path.clone()),
            Ok(o) if o.combined().contains("conflict") => out.conflicted.push(f.path.clone()),
            Ok(o) => out.failed.push((f.path.clone(), o.combined().trim().to_string())),
            Err(e) => out.failed.push((f.path.clone(), e.to_string())),
        }
    }
    if !keep {
        let done: Vec<&String> = out.applied.iter().chain(&out.conflicted).collect();
        let (gone, kept): (Vec<ShelvedFile>, Vec<ShelvedFile>) =
            shelf.files.into_iter().partition(|f| done.contains(&&f.path));
        shelf.files = kept;
        if shelf.files.is_empty() {
            std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
            out.removed = true;
        } else {
            write_meta(&dir, &shelf)?;
            for f in gone {
                let _ = std::fs::remove_file(dir.join(&f.patch));
            }
        }
    }
    Ok(out)
}

/// Delete a shelf (or only some of its files).
pub fn delete(root: &Path, id: &str, paths: Option<&[String]>) -> Result<bool> {
    let dir = shelf_dir(root, id)?;
    let Some(mut shelf) = read_meta(&dir) else {
        return Err(GitError::Invalid(format!("there is no shelf '{id}'")));
    };
    let Some(paths) = paths else {
        std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
        return Ok(true);
    };
    let (gone, kept): (Vec<ShelvedFile>, Vec<ShelvedFile>) =
        shelf.files.into_iter().partition(|f| paths.contains(&f.path));
    shelf.files = kept;
    if shelf.files.is_empty() {
        std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
        return Ok(true);
    }
    write_meta(&dir, &shelf)?;
    for f in gone {
        let _ = std::fs::remove_file(dir.join(&f.patch));
    }
    Ok(false)
}

/// Rename a shelf.
pub fn rename(root: &Path, id: &str, name: &str) -> Result<()> {
    let dir = shelf_dir(root, id)?;
    let mut shelf = read_meta(&dir).ok_or_else(|| GitError::Invalid(format!("there is no shelf '{id}'")))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(GitError::Invalid("enter a name".into()));
    }
    shelf.name = name.to_string();
    write_meta(&dir, &shelf)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available, write};

    #[test]
    fn ids_and_patch_names_are_checked() {
        assert!(valid_id("1700000000000-2"));
        for bad in ["", "..", "a/b", "x y", "../../etc"] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(valid_patch_name("0001.patch"));
        assert!(!valid_patch_name("../x.patch") && !valid_patch_name(".hidden") && !valid_patch_name("a/b"));
        let dir = tempfile::tempdir().unwrap();
        assert!(list(&dir.path().join("none")).is_empty());
        assert!(matches!(patch_text(dir.path(), "../x", "a"), Err(GitError::Invalid(_))));
        assert!(delete(dir.path(), "nope", None).is_err());
        // A broken shelf.json is skipped, not fatal.
        std::fs::create_dir_all(dir.path().join("123")).unwrap();
        std::fs::write(dir.path().join("123").join(META), "{ not json").unwrap();
        std::fs::create_dir_all(dir.path().join("124")).unwrap();
        std::fs::write(
            dir.path().join("124").join(META),
            r#"{"name":"x","files":[{"path":"a","patch":"../../evil"},{"path":"b","patch":"0001.patch"}]}"#,
        )
        .unwrap();
        let l = list(dir.path());
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].files.len(), 1, "the escaping patch name is dropped");
    }

    #[test]
    fn shelve_and_unshelve_whole_and_partially() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("gone.txt", "g\n")]);
        write(&root, "a.txt", "a2\n");
        write(&root, "b.txt", "b2\n");
        write(&root, "new.txt", "brand new\n");
        write(&root, "staged.txt", "staged\n");
        git.run(["add", "staged.txt"]).unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        let shelf_root = root.join(".codecraft").join("shelf");
        let req = ShelveRequest {
            name: "WIP".into(),
            files: vec![
                ("a.txt".into(), ShelvedKind::Modified),
                ("new.txt".into(), ShelvedKind::Unversioned),
                ("staged.txt".into(), ShelvedKind::Added),
                ("gone.txt".into(), ShelvedKind::Modified),
            ],
            created_ms: 1000,
            keep: false,
        };
        let shelf = shelve(&git, &shelf_root, &req).unwrap();
        assert_eq!(shelf.files.len(), 4);
        // Shelved files are reverted; b.txt is untouched; the shelf is ignored by git.
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        assert!(!root.join("new.txt").exists() && !root.join("staged.txt").exists());
        assert!(root.join("gone.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b2\n");
        let st = git.read_text(["status", "--porcelain"]).unwrap();
        assert_eq!(st.trim(), "M b.txt", "{st}");
        let listed = list(&shelf_root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "WIP");
        assert!(patch_text(&shelf_root, &shelf.id, "a.txt").unwrap().contains("+a2"));
        // Partial unshelve: only a.txt.
        let r = unshelve(&git, &shelf_root, &shelf.id, Some(&["a.txt".to_string()]), false).unwrap();
        assert_eq!(r.applied, vec!["a.txt".to_string()]);
        assert!(!r.removed);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a2\n");
        assert_eq!(list(&shelf_root)[0].files.len(), 3);
        // The rest, keeping the shelf.
        let r = unshelve(&git, &shelf_root, &shelf.id, None, true).unwrap();
        assert_eq!(r.applied.len(), 3, "{r:?}");
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "brand new\n");
        assert!(!root.join("gone.txt").exists());
        assert_eq!(list(&shelf_root)[0].files.len(), 3, "kept");
        rename(&shelf_root, &shelf.id, "Renamed").unwrap();
        assert_eq!(list(&shelf_root)[0].name, "Renamed");
        assert!(delete(&shelf_root, &shelf.id, Some(&["new.txt".to_string()])).is_ok());
        assert_eq!(list(&shelf_root)[0].files.len(), 2);
        assert!(delete(&shelf_root, &shelf.id, None).unwrap());
        assert!(list(&shelf_root).is_empty());
    }

    #[test]
    fn unshelve_merges_three_ways_when_the_file_changed() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("f.txt", "1\n2\n3\n4\n5\n")]);
        write(&root, "f.txt", "1\nTWO\n3\n4\n5\n");
        let shelf_root = root.join("shelf");
        let shelf = shelve(
            &git,
            &shelf_root,
            &ShelveRequest {
                files: vec![("f.txt".into(), ShelvedKind::Modified)],
                created_ms: 5,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(shelf.name, "Shelved changes");
        // Meanwhile a commit changes another line: a plain apply would still work; change the
        // same line to force a conflict.
        write(&root, "f.txt", "1\nzwei\n3\n4\n5\n");
        git.run(["commit", "-qam", "zwei"]).unwrap();
        let r = unshelve(&git, &shelf_root, &shelf.id, None, false).unwrap();
        assert!(r.applied.is_empty());
        assert!(!r.conflicted.is_empty() || !r.failed.is_empty(), "{r:?}");
        if !r.conflicted.is_empty() {
            let text = std::fs::read_to_string(root.join("f.txt")).unwrap();
            assert!(text.contains("<<<<<<<") && text.contains("TWO"), "{text}");
        } else {
            assert_eq!(list(&shelf_root).len(), 1, "a failed patch stays shelved");
        }
        // Nothing to shelve is refused.
        assert!(shelve(&git, &shelf_root, &ShelveRequest::default()).is_err());
    }
}
