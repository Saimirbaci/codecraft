//! The directory walker shared by the tree scan and Find in Files.

use std::path::{Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};

use crate::{FsError, Result, ScanOptions};

/// A walker over `root` applying the scan rules: ignore files (also outside git repositories),
/// hidden files, `.git` always hidden, excluded directory names at any depth, symlinks followed
/// with loop detection.
pub(crate) fn builder(root: &Path, opts: &ScanOptions) -> WalkBuilder {
    let respect = opts.respect_gitignore;
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .hidden(!opts.show_hidden)
        .parents(respect)
        .ignore(respect)
        .git_ignore(respect)
        .git_global(respect)
        .git_exclude(respect)
        .require_git(false)
        .follow_links(true);
    let exclude_dirs = opts.exclude_dirs.clone();
    builder.filter_entry(move |entry| keep(entry, &exclude_dirs));
    builder
}

fn keep(entry: &DirEntry, exclude_dirs: &[String]) -> bool {
    let name = entry.file_name();
    if name == ".git" {
        return false;
    }
    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
    !(is_dir && entry.depth() > 0 && exclude_dirs.iter().any(|d| name == d.as_str()))
}

/// `root` as an absolute path (no symlink resolution), checked to exist.
pub(crate) fn absolute_root(root: &Path) -> Result<(PathBuf, std::fs::Metadata)> {
    let io_err = |source| FsError::Io { path: root.to_path_buf(), source };
    let abs = std::path::absolute(root).map_err(io_err)?;
    let meta = std::fs::metadata(&abs).map_err(io_err)?;
    Ok((abs, meta))
}
