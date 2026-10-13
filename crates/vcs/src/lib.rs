//! CodeCraft version control over the `git` CLI (L0, no workspace deps): repository discovery,
//! status, line changes for gutter markers, commits, push/pull/fetch, branches and tags, log with
//! a commit graph, blame, stash, rollback, reset, patches, remotes, clone and conflicts, per-hunk
//! staging, three-way merge, work trees and GitHub through the `gh` CLI.
//!
//! We drive the `git` binary rather than a library so the user's configuration, hooks,
//! credential helpers, SSH agent and commit signing all work as they do in a terminal. Every
//! function here blocks: callers run them on worker threads. Read-only queries pass
//! `--no-optional-locks`; nothing ever prompts on a terminal (`GIT_TERMINAL_PROMPT=0`, SSH in
//! batch mode), so a remote that needs credentials fails with [`GitError::AuthRequired`].
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod blame;
pub mod cmd;
pub mod github;
pub mod hunks;
pub mod lines;
pub mod log;
pub mod merge3;
pub mod ops;
pub mod refs;
pub mod repo;
pub mod status;
pub mod worktree;

pub use blame::{Blame, BlameCommit, blame};
pub use cmd::{CancelToken, ConsoleLine, Git, Output, Sink};
pub use lines::{LineChange, LineChangeKind, line_changes};
pub use log::{ChangedFile, Commit, CommitDetails, GraphRow, LogQuery, RefKind, RefLabel, graph};
pub use refs::{Branch, Remote, Stash, Tag};
pub use repo::{RepoInfo, RepoState, discover};
pub use status::{BranchStatus, Change, ConflictKind, FileState, FileStatus, RepoStatus, StatusOptions};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed (or not on PATH)")]
    GitNotFound,
    #[error("not a git repository")]
    NotARepo,
    /// The remote wants credentials and we may not prompt for them.
    #[error("authentication required: {0}")]
    AuthRequired(String),
    #[error("{message}")]
    Failed { command: String, code: Option<i32>, message: String },
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Io(String),
    #[error("unexpected git output: {0}")]
    Parse(String),
    #[error("{0}")]
    TooLarge(String),
    /// A request we refuse before running git (bad branch name, empty message, …).
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// A literal pathspec: `:`-prefixed and glob characters in file names mean nothing special.
pub fn pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

/// Whether `name` is acceptable as a new branch or tag name (a conservative subset of
/// `git check-ref-format`).
pub fn valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name != "@"
        && !name.chars().any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod test_util {
    use std::path::{Path, PathBuf};

    use crate::Git;

    /// A temp directory with an isolated git identity (no user config, no hooks from `$HOME`).
    pub struct Sandbox {
        pub dir: tempfile::TempDir,
        pub home: PathBuf,
    }

    impl Sandbox {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = std::fs::canonicalize(dir.path()).unwrap().join("home");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(
                home.join(".gitconfig"),
                "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
                 [commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n",
            )
            .unwrap();
            Self { dir, home }
        }

        pub fn root(&self) -> PathBuf {
            std::fs::canonicalize(self.dir.path()).unwrap()
        }

        pub fn git(&self, dir: &Path) -> Git {
            Git::new(dir)
                .with_env("HOME", &self.home)
                .with_env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
                .with_env("GIT_CONFIG_NOSYSTEM", "1")
                .with_env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
                .with_env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        }

        /// A fresh repository at `<sandbox>/<name>` with one commit of `files`.
        pub fn repo(&self, name: &str, files: &[(&str, &str)]) -> (PathBuf, Git) {
            let path = self.root().join(name);
            std::fs::create_dir_all(&path).unwrap();
            let git = self.git(&path);
            git.run(["init", "-q", "-b", "main"]).unwrap();
            for (f, text) in files {
                write(&path, f, text);
            }
            if !files.is_empty() {
                git.run(["add", "-A"]).unwrap();
                git.run(["commit", "-q", "-m", "initial"]).unwrap();
            }
            (path, git)
        }
    }

    pub fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(p, text).unwrap();
    }

    pub fn git_available() -> bool {
        std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn ref_names() {
        for ok in ["main", "feature/x", "v1.0", "a-b_c"] {
            assert!(valid_ref_name(ok), "{ok}");
        }
        for bad in ["", "-x", "a..b", "a b", "x.lock", "a:b", "x/", "@", "a@{b", "a~1", "a\tb"] {
            assert!(!valid_ref_name(bad), "{bad}");
        }
        assert_eq!(pathspec(":odd*"), ":(literal):odd*");
    }
}
