use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use codecraft_cargo::CargoMetadata;
use codecraft_fs::FileTree;

/// The open project: a directory, its file tree and (for Cargo projects) metadata.
#[derive(Clone, Debug, Default)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub tree: FileTree,
    pub cargo: Option<CargoMetadata>,
    /// Why Cargo metadata is missing (not a Cargo project, `cargo metadata` failed, …).
    pub cargo_error: Option<String>,
    /// Expanded directories in the Project view.
    pub expanded: BTreeSet<PathBuf>,
    /// Selected node in the Project view.
    pub selected: Option<PathBuf>,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        let name =
            root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
        let mut expanded = BTreeSet::new();
        expanded.insert(root.clone());
        Self { root, name, expanded, ..Self::default() }
    }

    /// Path relative to the project root, for display (falls back to the full path).
    pub fn relative<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, str> {
        match path.strip_prefix(&self.root) {
            Ok(rel) => rel.to_string_lossy(),
            Err(_) => path.to_string_lossy(),
        }
    }

    /// Expand every ancestor directory of `path` (Select In › Project View).
    pub fn reveal(&mut self, path: &Path) {
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if !dir.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            cur = dir.parent();
        }
        self.selected = Some(path.to_path_buf());
    }
}
