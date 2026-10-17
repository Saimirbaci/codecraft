//! Project tree scan (the Project tool window).

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};

use crate::{FileTree, FsError, Node, NodeId, Result, ScanOptions, walk};

pub(crate) fn scan(root: &Path, opts: &ScanOptions) -> Result<FileTree> {
    let (root, meta) = walk::absolute_root(root)?;
    if !meta.is_dir() {
        let source = std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory");
        return Err(FsError::Io { path: root, source });
    }
    let shared = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let mut builder = CollectorBuilder { shared: &shared, count: &count, truncated: &truncated, max: opts.max_entries };
    walk::builder(&root, opts).build_parallel().visit(&mut builder);
    let entries = shared.into_inner().unwrap_or_else(PoisonError::into_inner);
    Ok(FileTree::from_nodes(build_nodes(root, entries), truncated.load(Ordering::Relaxed)))
}

struct Entry {
    path: PathBuf,
    is_dir: bool,
    depth: usize,
}

struct CollectorBuilder<'s> {
    shared: &'s Mutex<Vec<Entry>>,
    count: &'s AtomicUsize,
    truncated: &'s AtomicBool,
    max: usize,
}

impl<'s> ParallelVisitorBuilder<'s> for CollectorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Collector {
            local: Vec::new(),
            shared: self.shared,
            count: self.count,
            truncated: self.truncated,
            max: self.max,
        })
    }
}

/// Per-thread collector; flushes into the shared list when the walk drops it.
struct Collector<'s> {
    local: Vec<Entry>,
    shared: &'s Mutex<Vec<Entry>>,
    count: &'s AtomicUsize,
    truncated: &'s AtomicBool,
    max: usize,
}

impl ParallelVisitor for Collector<'_> {
    fn visit(&mut self, entry: std::result::Result<DirEntry, ignore::Error>) -> WalkState {
        // Unreadable directories, broken symlinks and symlink loops arrive as errors: skip them.
        let Ok(entry) = entry else { return WalkState::Continue };
        if entry.depth() == 0 {
            return WalkState::Continue;
        }
        // A directory's children are only queued after its own visit returns `Continue`, so
        // every accepted entry's parent was accepted too.
        if self.count.fetch_add(1, Ordering::Relaxed) >= self.max {
            self.truncated.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        self.local.push(Entry { depth: entry.depth(), is_dir, path: entry.into_path() });
        WalkState::Continue
    }
}

impl Drop for Collector<'_> {
    fn drop(&mut self) {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.append(&mut self.local);
    }
}

struct TmpNode {
    name: String,
    path: PathBuf,
    is_dir: bool,
    parent: Option<usize>,
    children: Vec<usize>,
    depth: usize,
}

/// Links the walked entries into a tree, sorts every directory's children in Project view order
/// and numbers the nodes in pre-order (so node ids follow the displayed order).
fn build_nodes(root: PathBuf, mut entries: Vec<Entry>) -> Vec<Node> {
    entries.sort_by_key(|e| e.depth);
    let root_name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
    let mut dirs: HashMap<PathBuf, usize> = HashMap::new();
    dirs.insert(root.clone(), 0);
    let mut tmp =
        vec![TmpNode { name: root_name, path: root, is_dir: true, parent: None, children: Vec::new(), depth: 0 }];
    for entry in entries {
        let Some(parent_id) = entry.path.parent().and_then(|p| dirs.get(p)).copied() else { continue };
        let id = tmp.len();
        let Some(parent) = tmp.get_mut(parent_id) else { continue };
        parent.children.push(id);
        let depth = parent.depth.saturating_add(1);
        if entry.is_dir {
            dirs.insert(entry.path.clone(), id);
        }
        let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        tmp.push(TmpNode {
            name,
            path: entry.path,
            is_dir: entry.is_dir,
            parent: Some(parent_id),
            children: Vec::new(),
            depth,
        });
    }
    drop(dirs);

    for i in 0..tmp.len() {
        let Some(node) = tmp.get_mut(i) else { continue };
        let mut children = std::mem::take(&mut node.children);
        children.sort_by(|&a, &b| match (tmp.get(a), tmp.get(b)) {
            (Some(a), Some(b)) => b.is_dir.cmp(&a.is_dir).then_with(|| cmp_names(&a.name, &b.name)),
            _ => a.cmp(&b),
        });
        if let Some(node) = tmp.get_mut(i) {
            node.children = children;
        }
    }

    let mut order = Vec::with_capacity(tmp.len());
    let mut stack = vec![0usize];
    while let Some(i) = stack.pop() {
        order.push(i);
        if let Some(node) = tmp.get(i) {
            stack.extend(node.children.iter().rev());
        }
    }
    let mut new_id = vec![usize::MAX; tmp.len()];
    for (new, &old) in order.iter().enumerate() {
        if let Some(slot) = new_id.get_mut(old) {
            *slot = new;
        }
    }
    let remap = |old: usize| new_id.get(old).copied().filter(|&n| n != usize::MAX).map(NodeId);
    let mut slots: Vec<Option<TmpNode>> = tmp.into_iter().map(Some).collect();
    order
        .iter()
        .filter_map(|&old| {
            let t = slots.get_mut(old)?.take()?;
            Some(Node {
                name: t.name,
                path: t.path,
                is_dir: t.is_dir,
                parent: t.parent.and_then(remap),
                children: t.children.into_iter().filter_map(remap).collect(),
                depth: t.depth,
            })
        })
        .collect()
}

/// Case-insensitive name order, ties broken by the exact name so the order is total.
fn cmp_names(a: &str, b: &str) -> CmpOrdering {
    a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase)).then_with(|| a.cmp(b))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    pub(crate) fn touch(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn opts() -> ScanOptions {
        ScanOptions { exclude_dirs: vec![], ..ScanOptions::default() }
    }

    /// Paths relative to the root, in tree order, dirs suffixed with `/`.
    fn listing(tree: &FileTree) -> Vec<String> {
        tree.iter()
            .skip(1)
            .map(|(id, n)| {
                let rel = tree.relative_path(id).unwrap().to_string_lossy().replace('\\', "/");
                if n.is_dir { format!("{rel}/") } else { rel }
            })
            .collect()
    }

    fn check_invariants(tree: &FileTree) {
        let root = tree.node(tree.root()).unwrap();
        assert!(root.path.is_absolute());
        assert_eq!(root.depth, 0);
        assert_eq!(root.parent, None);
        for (id, node) in tree.iter() {
            assert_eq!(tree.find(&node.path), Some(id));
            for &child in &node.children {
                let c = tree.node(child).unwrap();
                assert_eq!(c.parent, Some(id));
                assert_eq!(c.depth, node.depth + 1);
                assert_eq!(c.path.parent(), Some(node.path.as_path()));
                assert!(child > id, "pre-order numbering");
            }
        }
    }

    #[test]
    fn project_view_order_dirs_first_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["b.rs", "A.rs", "c.rs", "Zeta/z.rs", "alpha/a.rs", "Beta/b.rs"] {
            touch(dir.path(), f, "x");
        }
        let tree = scan(dir.path(), &opts()).unwrap();
        check_invariants(&tree);
        assert_eq!(
            listing(&tree),
            ["alpha/", "alpha/a.rs", "Beta/", "Beta/b.rs", "Zeta/", "Zeta/z.rs", "A.rs", "b.rs", "c.rs"]
        );
        let files: Vec<&str> = tree.files().map(|n| n.name.as_str()).collect();
        assert_eq!(files, ["a.rs", "b.rs", "z.rs", "A.rs", "b.rs", "c.rs"]);
        assert!(!tree.truncated());
        assert_eq!(tree.root_path(), Some(dir.path()));
        assert_eq!(tree.node(tree.root()).unwrap().name, dir.path().file_name().unwrap().to_string_lossy());
    }

    fn gitignore_fixture(with_git_dir: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".gitignore", "*.log\nbuild/\n");
        touch(dir.path(), "src/main.rs", "fn main() {}");
        touch(dir.path(), "src/debug.log", "log");
        touch(dir.path(), "build/out.rs", "x");
        touch(dir.path(), "src/.ignore", "generated.rs\n");
        touch(dir.path(), "src/generated.rs", "x");
        if with_git_dir {
            touch(dir.path(), ".git/HEAD", "ref: refs/heads/main\n");
        }
        dir
    }

    #[test]
    fn gitignore_respected_without_git_repo() {
        let dir = gitignore_fixture(false);
        let tree = scan(dir.path(), &opts()).unwrap();
        check_invariants(&tree);
        assert_eq!(listing(&tree), ["src/", "src/.ignore", "src/main.rs", ".gitignore"]);
    }

    #[test]
    fn gitignore_respected_in_git_repo_and_git_dir_hidden() {
        let dir = gitignore_fixture(true);
        let tree = scan(dir.path(), &opts()).unwrap();
        assert_eq!(listing(&tree), ["src/", "src/.ignore", "src/main.rs", ".gitignore"]);
    }

    #[test]
    fn gitignore_can_be_disabled() {
        let dir = gitignore_fixture(true);
        let tree = scan(dir.path(), &ScanOptions { respect_gitignore: false, ..opts() }).unwrap();
        assert_eq!(
            listing(&tree),
            [
                "build/",
                "build/out.rs",
                "src/",
                "src/.ignore",
                "src/debug.log",
                "src/generated.rs",
                "src/main.rs",
                ".gitignore"
            ]
        );
    }

    #[test]
    fn hidden_files_follow_option_but_git_is_always_hidden() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".env", "A=1");
        touch(dir.path(), ".github/ci.yml", "x");
        touch(dir.path(), ".git/HEAD", "x");
        touch(dir.path(), "sub/.git", "gitdir: ../.git/modules/sub");
        touch(dir.path(), "main.rs", "x");
        let shown = scan(dir.path(), &opts()).unwrap();
        assert_eq!(listing(&shown), [".github/", ".github/ci.yml", "sub/", ".env", "main.rs"]);
        let hidden = scan(dir.path(), &ScanOptions { show_hidden: false, ..opts() }).unwrap();
        assert_eq!(listing(&hidden), ["sub/", "main.rs"]);
    }

    #[test]
    fn exclude_dirs_match_names_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "target/debug/app", "x");
        touch(dir.path(), "crates/a/target/x.rs", "x");
        touch(dir.path(), "crates/a/src/lib.rs", "x");
        touch(dir.path(), "target.rs", "x");
        let o = ScanOptions { exclude_dirs: vec!["target".into()], respect_gitignore: false, ..opts() };
        let tree = scan(dir.path(), &o).unwrap();
        check_invariants(&tree);
        assert_eq!(listing(&tree), ["crates/", "crates/a/", "crates/a/src/", "crates/a/src/lib.rs", "target.rs"]);
    }

    /// CodeCraft's own `.codecraft` folder is hidden like `.idea` (Project view, Search
    /// Everywhere, Find in Files and the watcher all use the default options).
    #[test]
    fn codecraft_state_is_excluded_by_default() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".codecraft/workspace.json", "{}");
        touch(dir.path(), ".codecraft/local-history/a.rs", "x");
        touch(dir.path(), "sub/.codecraft/x.json", "{}");
        touch(dir.path(), ".idea/workspace.xml", "x");
        touch(dir.path(), "src/main.rs", "x");
        let o = ScanOptions { respect_gitignore: false, ..ScanOptions::default() };
        let tree = scan(dir.path(), &o).unwrap();
        check_invariants(&tree);
        assert_eq!(listing(&tree), ["src/", "src/main.rs", "sub/"]);
    }

    #[test]
    fn truncates_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            touch(dir.path(), &format!("d{i}/f.rs"), "x");
        }
        let tree = scan(dir.path(), &ScanOptions { max_entries: 5, ..opts() }).unwrap();
        assert!(tree.truncated());
        assert_eq!(tree.len(), 6);
        check_invariants(&tree);
        let empty = scan(dir.path(), &ScanOptions { max_entries: 0, ..opts() }).unwrap();
        assert!(empty.truncated());
        assert_eq!(empty.len(), 1);
        let exact = scan(dir.path(), &ScanOptions { max_entries: 20, ..opts() }).unwrap();
        assert!(!exact.truncated());
        assert_eq!(exact.len(), 21);
    }

    #[test]
    fn find_locates_scanned_paths() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "src/lib.rs", "x");
        let tree = scan(dir.path(), &opts()).unwrap();
        let id = tree.find(&dir.path().join("src/lib.rs")).unwrap();
        assert_eq!(tree.node(id).unwrap().name, "lib.rs");
        assert_eq!(tree.find(dir.path()), Some(tree.root()));
        assert_eq!(tree.find(&dir.path().join("src/missing.rs")), None);
        assert_eq!(tree.find(Path::new("src/lib.rs")), None);
    }

    #[test]
    fn bad_roots_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(scan(&dir.path().join("missing"), &opts()), Err(FsError::Io { .. })));
        touch(dir.path(), "file.rs", "x");
        assert!(matches!(scan(&dir.path().join("file.rs"), &opts()), Err(FsError::Io { .. })));
        assert!(matches!(scan(Path::new(""), &opts()), Err(FsError::Io { .. })));
    }

    #[test]
    fn relative_root_yields_absolute_paths() {
        let tree = scan(Path::new("src"), &opts()).unwrap();
        assert!(tree.root_path().unwrap().is_absolute());
        assert!(tree.files().any(|n| n.name == "tree.rs"));
        assert!(tree.iter().all(|(_, n)| n.path.is_absolute()));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_followed_without_loops() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "real/inner.rs", "x");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink("..", dir.path().join("real/up")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("broken")).unwrap();
        let tree = scan(dir.path(), &opts()).unwrap();
        check_invariants(&tree);
        let link = tree.node(tree.find(&dir.path().join("link")).unwrap()).unwrap();
        assert!(link.is_dir);
        assert!(tree.find(&dir.path().join("link/inner.rs")).is_some());
        // `real/up` points at an ancestor: listed nowhere, walked never.
        assert!(tree.len() < 20, "{:?}", listing(&tree));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subdirectories_are_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "locked/secret.rs", "x");
        touch(dir.path(), "open/ok.rs", "x");
        let locked = dir.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = fs::read_dir(&locked).is_ok(); // running as root
        let tree = scan(dir.path(), &opts());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let tree = tree.unwrap();
        assert!(tree.find(&dir.path().join("open/ok.rs")).is_some());
        if !readable_anyway {
            let node = tree.node(tree.find(&locked).unwrap()).unwrap();
            assert!(node.children.is_empty());
        }
    }

    #[test]
    fn name_comparison_is_total_and_case_insensitive() {
        assert_eq!(cmp_names("a", "B"), CmpOrdering::Less);
        assert_eq!(cmp_names("B", "a"), CmpOrdering::Greater);
        assert_eq!(cmp_names("A", "a"), CmpOrdering::Less);
        assert_eq!(cmp_names("Äb", "äa"), CmpOrdering::Greater);
        assert_eq!(cmp_names("x", "x"), CmpOrdering::Equal);
    }
}
