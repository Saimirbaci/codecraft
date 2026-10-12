//! Repository discovery and the in-progress operation (merge, rebase, …).

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Git, GitError, Result};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    /// The work tree root, spelled like the directory discovery started from (so it shares a
    /// prefix with the project's paths even when they go through a symlink, e.g. `/var` vs
    /// `/private/var` on macOS).
    pub root: PathBuf,
    /// The work tree root as git reports it (canonical).
    pub toplevel: PathBuf,
    /// The repository's git directory (absolute; `.git/worktrees/<name>` for a linked worktree).
    pub git_dir: PathBuf,
}

impl RepoInfo {
    /// `path` relative to the work tree root with `/` separators, if it is inside it.
    pub fn rel(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).or_else(|_| path.strip_prefix(&self.toplevel)).ok()?;
        let parts: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// The absolute path of a repository-relative path.
    pub fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for part in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(part);
        }
        p
    }

    /// Whether `path` is inside the work tree (or its git directory).
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root) || path.starts_with(&self.toplevel) || path.starts_with(&self.git_dir)
    }

    pub fn state(&self) -> RepoState {
        RepoState::detect(&self.git_dir)
    }
}

/// An operation git left in progress (usually waiting for conflicts to be resolved).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoState {
    #[default]
    Normal,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
}

impl RepoState {
    pub fn detect(git_dir: &Path) -> Self {
        if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
            Self::Rebasing
        } else if git_dir.join("MERGE_HEAD").is_file() {
            Self::Merging
        } else if git_dir.join("CHERRY_PICK_HEAD").is_file() {
            Self::CherryPicking
        } else if git_dir.join("REVERT_HEAD").is_file() {
            Self::Reverting
        } else {
            Self::Normal
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "",
            Self::Merging => "Merging",
            Self::Rebasing => "Rebasing",
            Self::CherryPicking => "Cherry-picking",
            Self::Reverting => "Reverting",
        }
    }

    /// The command that aborts the operation.
    pub fn abort_args(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Normal => None,
            Self::Merging => Some(&["merge", "--abort"]),
            Self::Rebasing => Some(&["rebase", "--abort"]),
            Self::CherryPicking => Some(&["cherry-pick", "--abort"]),
            Self::Reverting => Some(&["revert", "--abort"]),
        }
    }

    /// The command that continues it once conflicts are resolved.
    pub fn continue_args(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Normal => None,
            Self::Merging => Some(&["commit", "--no-edit"]),
            Self::Rebasing => Some(&["rebase", "--continue"]),
            Self::CherryPicking => Some(&["cherry-pick", "--continue"]),
            Self::Reverting => Some(&["revert", "--continue"]),
        }
    }
}

/// Find the repository containing `git.dir()`. `Ok(None)` outside a repository (or in a bare
/// one, which has no work tree to show).
pub fn discover(git: &Git) -> Result<Option<RepoInfo>> {
    let out = git
        .cmd(["rev-parse", "--is-bare-repository", "--show-toplevel", "--absolute-git-dir", "--show-prefix"])
        .read_only()
        .run();
    let text = match out {
        Ok(o) => o.text(),
        Err(GitError::Failed { message, .. }) if message.to_lowercase().contains("not a git repository") => {
            return Ok(None);
        }
        Err(GitError::Failed { message, .. }) if message.contains("must be run in a work tree") => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut lines = text.lines();
    if lines.next() == Some("true") {
        return Ok(None);
    }
    let (Some(top), Some(git_dir)) = (lines.next(), lines.next()) else {
        // Inside the .git directory: no work tree here.
        return Ok(None);
    };
    let prefix = lines.next().unwrap_or("");
    let toplevel = PathBuf::from(top);
    let mut root = git.dir().to_path_buf();
    let depth = prefix.split('/').filter(|s| !s.is_empty()).count();
    let mut ok = true;
    for _ in 0..depth {
        ok &= root.pop();
    }
    if !ok || !root.is_dir() {
        root = toplevel.clone();
    }
    Ok(Some(RepoInfo { root, toplevel, git_dir: PathBuf::from(git_dir) }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available};

    #[test]
    fn discovers_from_a_subdirectory_and_maps_paths() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, _) = sb.repo("r", &[("src/a.rs", "a\n")]);
        let info = discover(&sb.git(&root.join("src"))).unwrap().unwrap();
        assert_eq!(info.root, root);
        assert_eq!(info.rel(&root.join("src/a.rs")).as_deref(), Some("src/a.rs"));
        assert_eq!(info.abs("src/a.rs"), root.join("src/a.rs"));
        assert!(info.git_dir.ends_with(".git"));
        assert_eq!(info.state(), RepoState::Normal);
        assert!(info.rel(Path::new("/elsewhere/x")).is_none());
        // Outside any repository.
        let plain = sb.root().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // The sandbox itself is in a temp dir that is not a repository.
        assert!(discover(&sb.git(&plain)).unwrap().is_none());
    }

    #[test]
    fn missing_directory_is_an_error_not_a_panic() {
        let git = Git::new("/definitely/not/here");
        assert!(discover(&git).is_err());
    }
}
