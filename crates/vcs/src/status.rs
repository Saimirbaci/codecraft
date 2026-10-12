//! `git status --porcelain=v2`: the branch, its upstream, and every changed, untracked,
//! ignored (on demand) and conflicted file.

use serde::{Deserialize, Serialize};

use crate::{Git, GitError, Result};

/// At most this many entries are reported (a huge untracked tree would otherwise flood the UI).
pub const MAX_ENTRIES: usize = 20_000;

/// One side (index or work tree) of a file's status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Change {
    #[default]
    Unmodified,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    /// Unmerged (conflict).
    Unmerged,
}

impl Change {
    fn from_code(c: char) -> Self {
        match c {
            'M' => Self::Modified,
            'T' => Self::TypeChanged,
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            'C' => Self::Copied,
            'U' => Self::Unmerged,
            _ => Self::Unmodified,
        }
    }
}

/// The kind of a merge conflict (`git status` XY codes for unmerged paths).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    /// `DD`
    BothDeleted,
    /// `AU`
    AddedByUs,
    /// `UD`
    DeletedByThem,
    /// `UA`
    AddedByThem,
    /// `DU`
    DeletedByUs,
    /// `AA`
    BothAdded,
    /// `UU`
    BothModified,
}

impl ConflictKind {
    fn from_xy(xy: &str) -> Self {
        match xy {
            "DD" => Self::BothDeleted,
            "AU" => Self::AddedByUs,
            "UD" => Self::DeletedByThem,
            "UA" => Self::AddedByThem,
            "DU" => Self::DeletedByUs,
            "AA" => Self::BothAdded,
            _ => Self::BothModified,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::BothDeleted => "both deleted",
            Self::AddedByUs => "added by us",
            Self::DeletedByThem => "deleted by them",
            Self::AddedByThem => "added by them",
            Self::DeletedByUs => "deleted by us",
            Self::BothAdded => "both added",
            Self::BothModified => "both modified",
        }
    }

    /// Whether "our" side has the file.
    pub fn ours_exists(self) -> bool {
        !matches!(self, Self::BothDeleted | Self::DeletedByUs | Self::AddedByThem)
    }

    /// Whether "their" side has the file.
    pub fn theirs_exists(self) -> bool {
        !matches!(self, Self::BothDeleted | Self::DeletedByThem | Self::AddedByUs)
    }
}

/// What the UI shows for a file (its colour in the project tree, tabs and change lists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FileState {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Conflicted,
}

impl FileState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "Modified",
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Renamed => "Renamed",
            Self::Untracked => "Unversioned",
            Self::Ignored => "Ignored",
            Self::Conflicted => "Merge conflict",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    /// Repository-relative path with `/` separators.
    pub path: String,
    /// The source of a rename or copy.
    pub orig_path: Option<String>,
    pub index: Change,
    pub worktree: Change,
    pub untracked: bool,
    pub ignored: bool,
    pub conflict: Option<ConflictKind>,
}

impl FileStatus {
    pub fn state(&self) -> FileState {
        if self.conflict.is_some() {
            FileState::Conflicted
        } else if self.untracked {
            FileState::Untracked
        } else if self.ignored {
            FileState::Ignored
        } else if self.index == Change::Deleted || self.worktree == Change::Deleted {
            FileState::Deleted
        } else if self.index == Change::Added {
            FileState::Added
        } else if matches!(self.index, Change::Renamed | Change::Copied) {
            FileState::Renamed
        } else {
            FileState::Modified
        }
    }

    /// Whether the file exists in `HEAD` under this path.
    pub fn in_head(&self) -> bool {
        !self.untracked && !self.ignored && !matches!(self.index, Change::Added | Change::Renamed | Change::Copied)
    }

    /// Whether the index differs from `HEAD`.
    pub fn staged(&self) -> bool {
        !matches!(self.index, Change::Unmodified | Change::Unmerged)
    }
}

/// The current branch and its upstream.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchStatus {
    /// `HEAD`'s commit (`None` before the first commit).
    pub oid: Option<String>,
    /// The branch name (`None` when `HEAD` is detached).
    pub head: Option<String>,
    /// e.g. `origin/main`.
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatus {
    pub branch: BranchStatus,
    pub files: Vec<FileStatus>,
    pub stash_count: usize,
    /// More than [`MAX_ENTRIES`] entries: the list is cut.
    pub truncated: bool,
}

impl RepoStatus {
    pub fn file(&self, path: &str) -> Option<&FileStatus> {
        self.files.iter().find(|f| f.path == path)
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.conflict.is_some())
    }

    /// Tracked changes (not untracked or ignored).
    pub fn changes(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| !f.untracked && !f.ignored)
    }

    pub fn untracked(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.untracked)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOptions {
    /// Also list ignored files (directories that are ignored as a whole are listed once).
    pub ignored: bool,
    /// List untracked files (each file, not just their directories).
    pub untracked: bool,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self { ignored: false, untracked: true }
    }
}

/// Run `git status` and parse it.
pub fn status(git: &Git, opts: StatusOptions) -> Result<RepoStatus> {
    let mut args = vec!["status", "--porcelain=v2", "--branch", "--show-stash", "-z"];
    args.push(if opts.untracked { "--untracked-files=all" } else { "--untracked-files=no" });
    if opts.ignored {
        args.push("--ignored=matching");
    }
    let out = git.read(args)?;
    parse(&out)
}

/// Parse `git status --porcelain=v2 --branch --show-stash -z` output.
pub fn parse(out: &[u8]) -> Result<RepoStatus> {
    let mut st = RepoStatus::default();
    let mut records = out.split(|b| *b == 0).map(|r| String::from_utf8_lossy(r).into_owned());
    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(h) = rec.strip_prefix("# ") {
            parse_header(&mut st, h);
            continue;
        }
        if st.files.len() >= MAX_ENTRIES {
            st.truncated = true;
            continue;
        }
        let kind = rec.chars().next().unwrap_or(' ');
        match kind {
            '1' => {
                let f: Vec<&str> = rec.splitn(9, ' ').collect();
                let (Some(xy), Some(path)) = (f.get(1), f.get(8)) else { return Err(bad(&rec)) };
                st.files.push(entry(xy, path, None));
            }
            '2' => {
                let f: Vec<&str> = rec.splitn(10, ' ').collect();
                let (Some(xy), Some(path)) = (f.get(1), f.get(9)) else { return Err(bad(&rec)) };
                let orig = records.next().filter(|s| !s.is_empty());
                st.files.push(entry(xy, path, orig));
            }
            'u' => {
                let f: Vec<&str> = rec.splitn(11, ' ').collect();
                let (Some(xy), Some(path)) = (f.get(1), f.get(10)) else { return Err(bad(&rec)) };
                let mut e = entry(xy, path, None);
                e.conflict = Some(ConflictKind::from_xy(xy));
                st.files.push(e);
            }
            '?' | '!' => {
                let path = rec.get(2..).unwrap_or_default().to_string();
                st.files.push(FileStatus {
                    path,
                    orig_path: None,
                    index: Change::Unmodified,
                    worktree: Change::Unmodified,
                    untracked: kind == '?',
                    ignored: kind == '!',
                    conflict: None,
                });
            }
            _ => return Err(bad(&rec)),
        }
    }
    Ok(st)
}

fn bad(rec: &str) -> GitError {
    GitError::Parse(format!("status entry {:?}", rec.chars().take(120).collect::<String>()))
}

fn entry(xy: &str, path: &str, orig: Option<String>) -> FileStatus {
    let mut c = xy.chars();
    let x = c.next().unwrap_or('.');
    let y = c.next().unwrap_or('.');
    FileStatus {
        path: path.to_string(),
        orig_path: orig,
        index: Change::from_code(x),
        worktree: Change::from_code(y),
        untracked: false,
        ignored: false,
        conflict: None,
    }
}

fn parse_header(st: &mut RepoStatus, h: &str) {
    let (key, value) = h.split_once(' ').unwrap_or((h, ""));
    match key {
        "branch.oid" => st.branch.oid = (value != "(initial)").then(|| value.to_string()),
        "branch.head" => st.branch.head = (value != "(detached)").then(|| value.to_string()),
        "branch.upstream" => st.branch.upstream = Some(value.to_string()),
        "branch.ab" => {
            for part in value.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    st.branch.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    st.branch.behind = n.parse().unwrap_or(0);
                }
            }
        }
        "stash" => st.stash_count = value.trim().parse().unwrap_or(0),
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available, write};

    #[test]
    fn parses_every_record_kind() {
        let raw = b"# branch.oid 1234\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
# stash 3\0\
1 .M N... 100644 100644 100644 aaa bbb src/with space.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\0old name.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? new.txt\0! target/\0";
        let st = parse(raw).unwrap();
        assert_eq!(st.branch.head.as_deref(), Some("main"));
        assert_eq!(st.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.branch.ahead, st.branch.behind), (2, 1));
        assert_eq!(st.stash_count, 3);
        assert_eq!(st.files.len(), 5);
        assert_eq!(st.files[0].path, "src/with space.rs");
        assert_eq!(st.files[0].state(), FileState::Modified);
        assert_eq!(st.files[1].path, "new name.rs");
        assert_eq!(st.files[1].orig_path.as_deref(), Some("old name.rs"));
        assert_eq!(st.files[1].state(), FileState::Renamed);
        assert_eq!(st.files[2].conflict, Some(ConflictKind::BothModified));
        assert_eq!(st.files[2].state(), FileState::Conflicted);
        assert_eq!(st.files[3].state(), FileState::Untracked);
        assert_eq!(st.files[4].state(), FileState::Ignored);
        // Detached and unborn.
        let st = parse(b"# branch.oid (initial)\0# branch.head (detached)\0").unwrap();
        assert_eq!(st.branch.oid, None);
        assert_eq!(st.branch.head, None);
        // Garbage is an error, not a panic.
        assert!(parse(b"1 MM\0").is_err());
        assert!(parse(b"x?\0").is_err());
        assert!(parse(b"\xff\xfe").is_err());
    }

    #[test]
    fn status_of_a_real_repository() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("a.txt", "a\n"), ("b.txt", "b\n"), (".gitignore", "*.log\n")]);
        write(&root, "a.txt", "changed\n");
        std::fs::remove_file(root.join("b.txt")).unwrap();
        write(&root, "new/ü.txt", "x\n");
        write(&root, "debug.log", "x\n");
        write(&root, "staged.txt", "s\n");
        git.run(["add", "staged.txt"]).unwrap();
        let st = status(&git, StatusOptions::default()).unwrap();
        assert_eq!(st.branch.head.as_deref(), Some("main"));
        let state = |p: &str| st.file(p).map(FileStatus::state);
        assert_eq!(state("a.txt"), Some(FileState::Modified));
        assert_eq!(state("b.txt"), Some(FileState::Deleted));
        assert_eq!(state("new/ü.txt"), Some(FileState::Untracked));
        assert_eq!(state("staged.txt"), Some(FileState::Added));
        assert!(st.file("staged.txt").unwrap().staged());
        assert!(!st.file("staged.txt").unwrap().in_head());
        assert_eq!(state("debug.log"), None);
        let st = status(&git, StatusOptions { ignored: true, untracked: true }).unwrap();
        assert_eq!(st.file("debug.log").map(FileStatus::state), Some(FileState::Ignored));
    }
}
