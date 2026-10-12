//! CodeCraft project files (L0, no workspace deps).
//!
//! An ignore-aware project tree scan (the Project tool window), IntelliJ-style fuzzy matching
//! (Go to File, Search Everywhere), Find in Files, and safe text file I/O (binary detection,
//! size caps, atomic writes).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod find;
mod fuzzy;
mod io;
mod mask;
mod tree;
mod walk;
mod watch;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

pub use fuzzy::FuzzyMatcher;
pub use mask::FileMask;
pub use watch::ProjectWatcher;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} is a binary file")]
    Binary(PathBuf),
    #[error("{0} is not valid UTF-8")]
    NotUtf8(PathBuf),
    #[error("{path} is too large ({size} bytes, limit {limit})")]
    TooLarge { path: PathBuf, size: u64, limit: u64 },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, FsError>;

/// Largest file the editor opens as text.
pub const MAX_TEXT_FILE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    pub respect_gitignore: bool,
    /// Show dotfiles (other than `.git`, which is always hidden).
    pub show_hidden: bool,
    /// Directory names never descended into (RustRover marks `target` as excluded).
    pub exclude_dirs: Vec<String>,
    /// Stop after this many entries; the tree reports `truncated()`.
    pub max_entries: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            show_hidden: true,
            exclude_dirs: vec!["target".into(), ".git".into(), ".idea".into(), "node_modules".into()],
            max_entries: 200_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub usize);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    /// Absolute path.
    pub path: PathBuf,
    pub is_dir: bool,
    pub parent: Option<NodeId>,
    /// Directories first, then files, each sorted case-insensitively (Project view order).
    pub children: Vec<NodeId>,
    pub depth: usize,
}

/// A scanned project directory. Node 0 is the root.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTree {
    nodes: Vec<Node>,
    truncated: bool,
    /// Path lookup for [`FileTree::find`]; derived from `nodes`, so it is not serialized and
    /// never affects equality. Rebuilt lazily after deserialization.
    #[serde(skip)]
    index: PathIndex,
}

impl FileTree {
    pub fn from_nodes(nodes: Vec<Node>, truncated: bool) -> Self {
        let index = PathIndex::default();
        // A fresh OnceLock is always empty, so `set` cannot fail here.
        let _ = index.0.set(PathIndex::build(&nodes));
        Self { nodes, truncated, index }
    }

    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0)
    }

    pub fn root_path(&self) -> Option<&Path> {
        self.nodes.first().map(|n| n.path.as_path())
    }

    /// The node for an absolute path, if scanned.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let entries = self.index.0.get_or_init(|| PathIndex::build(&self.nodes));
        let hash = path_hash(path);
        let start = entries.partition_point(|&(h, _)| h < hash);
        entries
            .get(start..)?
            .iter()
            .take_while(|&&(h, _)| h == hash)
            .map(|&(_, id)| id)
            .find(|&id| self.nodes.get(id).is_some_and(|n| n.path == path))
            .map(NodeId)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Every file node (not directories), in tree order.
    pub fn files(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| !n.is_dir)
    }

    /// Every node with its id, in tree order (pre-order, Project view order).
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter().enumerate().map(|(i, n)| (NodeId(i), n))
    }

    /// A node's path relative to the root (empty for the root itself).
    pub fn relative_path(&self, id: NodeId) -> Option<&Path> {
        let root = self.root_path()?;
        self.node(id)?.path.strip_prefix(root).ok()
    }
}

/// `(hash of path, node index)` sorted by hash: 16 bytes per node instead of a cloned path.
#[derive(Clone, Default)]
struct PathIndex(OnceLock<Vec<(u64, usize)>>);

impl PathIndex {
    fn build(nodes: &[Node]) -> Vec<(u64, usize)> {
        let mut entries: Vec<(u64, usize)> = nodes.iter().enumerate().map(|(i, n)| (path_hash(&n.path), i)).collect();
        entries.sort_unstable();
        entries
    }
}

impl PartialEq for PathIndex {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for PathIndex {}

impl std::fmt::Debug for PathIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PathIndex")
    }
}

fn path_hash(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// Scan `root` into a tree. Unreadable subdirectories are skipped, not fatal.
///
/// Paths are absolute (a relative `root` is resolved against the current directory, without
/// resolving symlinks). Directories named in `exclude_dirs` are left out entirely. Symlinks are
/// followed, except into a directory that is one of their own ancestors.
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<FileTree> {
    tree::scan(root, opts)
}

/// A fuzzy match: higher score is better; `indices` are matched char indices in the candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i64,
    pub indices: Vec<usize>,
}

/// IntelliJ-style matching: case-insensitive subsequence; bonuses for matching at word starts,
/// camel humps (`FB` → `FooBar`), after `_`/`-`/`.`/`/`, consecutive runs, and exact
/// file-name prefix; a pattern containing `/` matches path segments in order. Empty pattern
/// matches everything with score 0.
///
/// Whitespace in the pattern is ignored; `\` in the pattern is treated as `/`. To match many
/// candidates against one pattern, use [`FuzzyMatcher`] (or [`rank`]) to reuse its buffers.
pub fn fuzzy_match(pattern: &str, candidate: &str) -> Option<FuzzyMatch> {
    FuzzyMatcher::new(pattern).fuzzy_match(candidate)
}

/// Rank candidates by [`fuzzy_match`]; returns `(candidate index, match)` best first, at most `limit`.
/// Ties break by shorter candidate (in bytes), then index.
pub fn rank<'a>(
    pattern: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<(usize, FuzzyMatch)> {
    fuzzy::rank(pattern, candidates, limit)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatch {
    pub path: PathBuf,
    /// Zero-based line.
    pub line: usize,
    /// Char column range of the match within the line.
    pub column: usize,
    pub end_column: usize,
    /// The line's text (trimmed to at most 500 chars).
    pub line_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindOptions {
    /// Comma-separated globs on file names, e.g. `*.rs,*.toml`; `!` prefix excludes.
    pub file_mask: Option<String>,
    pub max_results: usize,
    pub max_file_size: u64,
    pub scan: ScanOptions,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self { file_mask: None, max_results: 10_000, max_file_size: 10 * 1024 * 1024, scan: ScanOptions::default() }
    }
}

/// Search every text file under `root` (parallel is welcome), stopping early when `cancel` is set.
/// Binary and over-size files are skipped. Results sorted by path then line.
///
/// The regex runs on each line separately (without its line terminator), so `^`/`$` anchor at
/// line boundaries and matches never span lines; empty matches are not reported. Files that are
/// not valid UTF-8 are skipped. When cancelled, or when `max_results` is reached, the matches
/// found so far are returned (which subset is found first is unspecified).
pub fn find_in_files(
    root: &Path,
    regex: &regex::Regex,
    opts: &FindOptions,
    cancel: &AtomicBool,
) -> Result<Vec<FileMatch>> {
    find::find_in_files(root, regex, opts, cancel)
}

/// Read a text file: errors for binary content (NUL in the first 8 KiB), invalid UTF-8 (a
/// leading UTF-8 BOM is stripped), or more than [`MAX_TEXT_FILE_BYTES`].
pub fn read_text_file(path: &Path) -> Result<String> {
    io::read_text_file_limited(path, MAX_TEXT_FILE_BYTES)
}

/// Write via a temp file in the same directory + rename, keeping the original permissions.
///
/// A symlinked `path` is resolved so the link survives and its target is replaced. A read-only
/// existing file is refused (`PermissionDenied`) rather than silently replaced.
pub fn write_text_file_atomic(path: &Path, contents: &str) -> Result<()> {
    io::write_atomic(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, path: &str, is_dir: bool, parent: Option<usize>, children: Vec<usize>, depth: usize) -> Node {
        Node {
            name: name.into(),
            path: PathBuf::from(path),
            is_dir,
            parent: parent.map(NodeId),
            children: children.into_iter().map(NodeId).collect(),
            depth,
        }
    }

    fn sample() -> FileTree {
        FileTree::from_nodes(
            vec![
                node("p", "/p", true, None, vec![1, 2], 0),
                node("src", "/p/src", true, Some(0), vec![3], 1),
                node("Cargo.toml", "/p/Cargo.toml", false, Some(0), vec![], 1),
                node("lib.rs", "/p/src/lib.rs", false, Some(1), vec![], 2),
            ],
            false,
        )
    }

    #[test]
    fn find_uses_index() {
        let tree = sample();
        assert_eq!(tree.find(Path::new("/p")), Some(NodeId(0)));
        assert_eq!(tree.find(Path::new("/p/src/lib.rs")), Some(NodeId(3)));
        assert_eq!(tree.find(Path::new("/p/src/")), Some(NodeId(1)));
        assert_eq!(tree.find(Path::new("/p/missing")), None);
        assert_eq!(FileTree::default().find(Path::new("/p")), None);
    }

    #[test]
    fn lazily_built_index_and_equality_ignores_it() {
        let tree = sample();
        // What deserialization produces: `#[serde(skip)]` fills the index with its Default.
        let fresh = FileTree { nodes: tree.nodes.clone(), truncated: false, index: PathIndex::default() };
        assert_eq!(fresh, tree);
        assert_eq!(fresh.find(Path::new("/p/Cargo.toml")), Some(NodeId(2)));
        assert_eq!(fresh, tree);
        assert_eq!(fresh.clone().find(Path::new("/p/src")), Some(NodeId(1)));
    }

    #[test]
    fn iter_and_relative_paths() {
        let tree = sample();
        assert_eq!(tree.iter().count(), 4);
        assert_eq!(tree.relative_path(NodeId(3)), Some(Path::new("src/lib.rs")));
        assert_eq!(tree.relative_path(NodeId(0)), Some(Path::new("")));
        assert_eq!(tree.relative_path(NodeId(9)), None);
        let files: Vec<&str> = tree.files().map(|n| n.name.as_str()).collect();
        assert_eq!(files, ["Cargo.toml", "lib.rs"]);
    }
}
