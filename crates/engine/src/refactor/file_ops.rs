//! File operations: Move (F6), Copy (F5), Rename and Safe Delete (with a usage check) for files
//! and directories. The usage search is pure (it takes a reader) so it runs on a worker thread
//! and in tests; the rest plans operations as preview entries.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::preview::EntryKind;

/// Most usages one safe-delete check reports.
pub const MAX_USAGES: usize = 500;
/// Most files one copy may create.
const MAX_COPY_ENTRIES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageKind {
    /// `mod foo;` in the parent module (removed together with the file).
    ModDeclaration,
    /// A path or textual reference that will break.
    Reference,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub path: PathBuf,
    /// Zero-based line and char column.
    pub line: usize,
    pub column: usize,
    pub text: String,
    pub kind: UsageKind,
}

/// The Rust module a file or directory defines: `foo.rs`/`foo/mod.rs`/`foo/` → `foo`.
/// Crate roots (`main.rs`, `lib.rs`) and non-Rust files define none.
pub fn module_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if path.extension().is_some_and(|e| e == "rs") {
        let stem = path.file_stem()?.to_str()?;
        return match stem {
            "mod" => path.parent()?.file_name()?.to_str().map(str::to_string),
            "main" | "lib" | "build" => None,
            s => Some(s.to_string()),
        };
    }
    if path.extension().is_none() && is_identifier(name) {
        return Some(name.to_string());
    }
    None
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// The module item a path stands for: `foo/mod.rs` is the directory `foo`.
fn module_node(path: &Path) -> PathBuf {
    if path.file_name().is_some_and(|n| n == "mod.rs")
        && let Some(dir) = path.parent()
    {
        return dir.to_path_buf();
    }
    path.to_path_buf()
}

/// Files that may contain `mod <name>;` for `path`: the `.rs` files next to it (crate roots,
/// `mod.rs`) and the parent directory's own module file (`a/foo.rs` → `a.rs`).
pub fn parent_module_candidates(path: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let node = module_node(path);
    let Some(dir) = node.parent() else { return Vec::new() };
    let mut out: Vec<PathBuf> = files
        .iter()
        .filter(|f| f.parent() == Some(dir) && f.extension().is_some_and(|e| e == "rs") && !f.starts_with(&node))
        .cloned()
        .collect();
    if let (Some(grand), Some(dir_name)) = (dir.parent(), dir.file_name()) {
        let mut own = grand.join(dir_name);
        own.set_extension("rs");
        if files.contains(&own) && !out.contains(&own) {
            out.push(own);
        }
    }
    out.sort();
    out
}

/// What would break if `path` (a file or directory) were deleted. `files` are the project's
/// files; `read` returns a file's text. Files under `path` are not searched.
pub fn find_usages(path: &Path, files: &[PathBuf], read: &dyn Fn(&Path) -> Option<String>) -> Vec<Usage> {
    let mut out = Vec::new();
    let searchable = |f: &&PathBuf| !f.starts_with(path);
    if let Some(name) = module_name(path).filter(|_| is_rust_module(path, files)) {
        let escaped = regex::escape(&name);
        let (Ok(decl), Ok(reference)) = (
            Regex::new(&format!(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+{escaped}\s*;")),
            Regex::new(&format!(r"\b{escaped}::|::{escaped}\b")),
        ) else {
            return out;
        };
        let candidates = parent_module_candidates(path, files);
        for f in files.iter().filter(searchable).filter(|f| f.extension().is_some_and(|e| e == "rs")) {
            let Some(text) = read(f) else { continue };
            let is_parent = candidates.contains(f);
            for (i, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                let (kind, column) = if is_parent && decl.is_match(line) {
                    (UsageKind::ModDeclaration, line.chars().count().saturating_sub(trimmed.chars().count()))
                } else if let Some(m) = reference.find(line) {
                    (UsageKind::Reference, line.get(..m.start()).unwrap_or("").chars().count())
                } else {
                    continue;
                };
                out.push(Usage {
                    path: f.clone(),
                    line: i,
                    column,
                    text: line.trim().chars().take(300).collect(),
                    kind,
                });
                if out.len() >= MAX_USAGES {
                    return out;
                }
            }
        }
        return out;
    }
    // Other files: textual references to the file name (Cargo.toml paths, includes, docs).
    let Some(name) = path.file_name().and_then(|n| n.to_str()).filter(|n| n.len() >= 3) else { return out };
    for f in files.iter().filter(searchable) {
        let Some(text) = read(f) else { continue };
        for (i, line) in text.lines().enumerate() {
            if let Some(byte) = line.find(name) {
                // Whole file names only (`a.rs` must not match `data.rs`).
                let before_ok = line.get(..byte).and_then(|b| b.chars().last()).is_none_or(|c| !is_name_char(c));
                let after_ok =
                    line.get(byte + name.len()..).and_then(|a| a.chars().next()).is_none_or(|c| !is_name_char(c));
                if before_ok && after_ok {
                    out.push(Usage {
                        path: f.clone(),
                        line: i,
                        column: line.get(..byte).unwrap_or("").chars().count(),
                        text: line.trim().chars().take(300).collect(),
                        kind: UsageKind::Reference,
                    });
                    if out.len() >= MAX_USAGES {
                        return out;
                    }
                }
            }
        }
    }
    out
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// A `.rs` file, or a directory holding `.rs` files.
fn is_rust_module(path: &Path, files: &[PathBuf]) -> bool {
    if path.extension().is_some_and(|e| e == "rs") {
        return true;
    }
    files.iter().any(|f| f.starts_with(path) && f.extension().is_some_and(|e| e == "rs"))
}

/// Safe delete as operations: remove the `mod` declarations, then delete the file/directory.
pub fn safe_delete_ops(path: &Path, usages: &[Usage], is_dir: bool) -> Vec<EntryKind> {
    let mut ops: Vec<EntryKind> = Vec::new();
    let mut by_file: std::collections::BTreeMap<&Path, Vec<usize>> = Default::default();
    for u in usages.iter().filter(|u| u.kind == UsageKind::ModDeclaration) {
        by_file.entry(u.path.as_path()).or_default().push(u.line);
    }
    for (file, lines) in by_file {
        let edits = lines
            .into_iter()
            .filter_map(|l| u32::try_from(l).ok())
            .map(|l| codecraft_lsp::lsp_types::TextEdit {
                range: codecraft_lsp::lsp_types::Range {
                    start: codecraft_lsp::lsp_types::Position::new(l, 0),
                    end: codecraft_lsp::lsp_types::Position::new(l.saturating_add(1), 0),
                },
                new_text: String::new(),
            })
            .collect();
        ops.push(EntryKind::Edit { path: file.to_path_buf(), edits });
    }
    ops.push(EntryKind::Delete { path: path.to_path_buf(), recursive: is_dir, ignore_if_not_exists: false });
    ops
}

/// Where a move/copy of `from` to `to` lands: into `to` when it is an existing directory,
/// else at `to` itself.
pub fn destination(from: &Path, to: &Path) -> Result<PathBuf, String> {
    let dest = if to.is_dir() {
        let name = from.file_name().ok_or_else(|| format!("{} has no file name", from.display()))?;
        to.join(name)
    } else {
        to.to_path_buf()
    };
    if dest == from {
        return Err("the source and the destination are the same".into());
    }
    if dest.starts_with(from) {
        return Err(format!("cannot move or copy {} into itself", from.display()));
    }
    if dest.exists() && !same_file(from, &dest) {
        return Err(format!("{} already exists", dest.display()));
    }
    Ok(dest)
}

/// Case-only renames on case-insensitive file systems see the destination "existing".
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// A file name the user typed: no separators, not `.`/`..`, not empty.
pub fn valid_file_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the name is empty".into());
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!("`{name}` is not a valid file name"));
    }
    Ok(name)
}

/// Copy a file or a directory tree (refuses to overwrite; capped).
pub fn copy_path(from: &Path, to: &Path) -> Result<usize, String> {
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    let io = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let meta = std::fs::symlink_metadata(from).map_err(|e| io(from, e))?;
    if !meta.is_dir() {
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        }
        std::fs::copy(from, to).map_err(|e| io(from, e))?;
        return Ok(1);
    }
    let mut count = 0usize;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        std::fs::create_dir_all(&dst).map_err(|e| io(&dst, e))?;
        for entry in std::fs::read_dir(&src).map_err(|e| io(&src, e))? {
            let entry = entry.map_err(|e| io(&src, e))?;
            count += 1;
            if count > MAX_COPY_ENTRIES {
                return Err(format!("{} has too many files to copy", from.display()));
            }
            let (s, d) = (entry.path(), dst.join(entry.file_name()));
            let ft = entry.file_type().map_err(|e| io(&s, e))?;
            if ft.is_dir() {
                stack.push((s, d));
            } else if ft.is_file() {
                std::fs::copy(&s, &d).map_err(|e| io(&s, e))?;
            }
            // Symlinks are skipped (a copy should not alias the original's targets).
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::HashMap;

    use super::*;

    fn files(root: &Path, rel: &[&str]) -> Vec<PathBuf> {
        rel.iter().map(|r| root.join(r)).collect()
    }

    #[test]
    fn module_names() {
        assert_eq!(module_name(Path::new("/p/src/foo.rs")).as_deref(), Some("foo"));
        assert_eq!(module_name(Path::new("/p/src/foo/mod.rs")).as_deref(), Some("foo"));
        assert_eq!(module_name(Path::new("/p/src/foo")).as_deref(), Some("foo"));
        assert_eq!(module_name(Path::new("/p/src/main.rs")), None);
        assert_eq!(module_name(Path::new("/p/README.md")), None);
        assert_eq!(module_name(Path::new("/p/my-dir")), None);
    }

    #[test]
    fn parent_candidates() {
        let r = Path::new("/p");
        let all = files(r, &["src/main.rs", "src/a.rs", "src/a/foo.rs", "src/a/bar.rs", "src/b/mod.rs", "src/b/c.rs"]);
        assert_eq!(parent_module_candidates(&r.join("src/a.rs"), &all), files(r, &["src/main.rs"]));
        assert_eq!(parent_module_candidates(&r.join("src/a/foo.rs"), &all), files(r, &["src/a/bar.rs", "src/a.rs"]));
        assert_eq!(parent_module_candidates(&r.join("src/b/mod.rs"), &all), files(r, &["src/a.rs", "src/main.rs"]));
        assert_eq!(parent_module_candidates(&r.join("src/b/c.rs"), &all), files(r, &["src/b/mod.rs"]));
    }

    #[test]
    fn usages_of_a_module() {
        let r = Path::new("/p");
        let all = files(r, &["src/main.rs", "src/util.rs", "src/other.rs", "src/deep/x.rs"]);
        let texts: HashMap<PathBuf, &str> = [
            (r.join("src/main.rs"), "mod other;\npub(crate) mod util;\n// mod util;\nfn main() { util::f(); }\n"),
            (r.join("src/util.rs"), "pub fn f() {}\n"),
            (r.join("src/other.rs"), "use crate::util;\nfn utility() {}\n"),
            (r.join("src/deep/x.rs"), "mod util;\n"),
        ]
        .into_iter()
        .collect();
        let read = |p: &Path| texts.get(p).map(|t| (*t).to_string());
        let u = find_usages(&r.join("src/util.rs"), &all, &read);
        let kinds: Vec<(String, usize, UsageKind)> =
            u.iter().map(|u| (u.path.file_name().unwrap().to_string_lossy().into_owned(), u.line, u.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("main.rs".into(), 1, UsageKind::ModDeclaration),
                ("main.rs".into(), 3, UsageKind::Reference),
                ("other.rs".into(), 0, UsageKind::Reference),
            ]
        );
        assert_eq!(u[1].column, 12);
        let ops = safe_delete_ops(&r.join("src/util.rs"), &u, false);
        assert_eq!(ops.len(), 2);
        match &ops[0] {
            EntryKind::Edit { path, edits } => {
                assert_eq!(path, &r.join("src/main.rs"));
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].range.start.line, 1);
                assert_eq!(edits[0].range.end.line, 2);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&ops[1], EntryKind::Delete { recursive: false, .. }));
    }

    #[test]
    fn usages_of_other_files_are_whole_names() {
        let r = Path::new("/p");
        let all = files(r, &["Cargo.toml", "notes.md", "data.json"]);
        let read = |p: &Path| match p.file_name()?.to_str()? {
            "Cargo.toml" => Some("include = [\"data.json\"]\n".to_string()),
            "notes.md" => Some("see mydata.json and data.jsonx\n".to_string()),
            _ => Some(String::new()),
        };
        let u = find_usages(&r.join("data.json"), &all, &read);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].column, 12);
    }

    #[test]
    fn destinations_and_names() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        let dir = tmp.path().join("sub");
        std::fs::write(&a, "x").unwrap();
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(destination(&a, &dir).unwrap(), dir.join("a.rs"));
        assert_eq!(destination(&a, &tmp.path().join("b.rs")).unwrap(), tmp.path().join("b.rs"));
        assert!(destination(&a, &a).is_err());
        assert!(destination(&dir, &dir.join("inner")).is_err());
        std::fs::write(dir.join("a.rs"), "y").unwrap();
        assert!(destination(&a, &dir).unwrap_err().contains("already exists"));
        assert!(valid_file_name(" ok.rs ").is_ok());
        for bad in ["", "..", "a/b", "a\\b"] {
            assert!(valid_file_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn copy_files_and_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("a.rs"), "a").unwrap();
        std::fs::write(src.join("inner/b.rs"), "b").unwrap();
        let dst = tmp.path().join("copy");
        assert_eq!(copy_path(&src, &dst).unwrap(), 3);
        assert_eq!(std::fs::read_to_string(dst.join("inner/b.rs")).unwrap(), "b");
        assert!(copy_path(&src, &dst).is_err());
        assert_eq!(copy_path(&src.join("a.rs"), &tmp.path().join("x/a2.rs")).unwrap(), 1);
        assert!(copy_path(&tmp.path().join("missing"), &tmp.path().join("m2")).is_err());
    }
}
