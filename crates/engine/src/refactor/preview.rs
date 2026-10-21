//! A pending multi-file edit: an LSP `WorkspaceEdit` (plain `changes` or ordered
//! `documentChanges` with create/rename/delete operations) turned into entries with before/after
//! text, reviewed in the preview window and applied entry by entry.
//!
//! Building simulates the operations in order over a small virtual file system (open documents
//! first, then disk), so every entry's `before` is exactly the text it will be applied to. Applying
//! edits open documents through the Session edit helpers (one undo step per file, LSP sync,
//! selections mapped) and writes unopened files atomically.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use codecraft_lsp::lsp_types as lt;
use codecraft_syntax::Language;
use codecraft_text::{Document, Edit, EditGroup, SelectionSet};
use codecraft_workspace::NotificationLevel;

use crate::{CommandError, Session, convert};

/// Most entries one preview holds (a rename touching more files than this is refused).
pub const MAX_ENTRIES: usize = 5000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EntryKind {
    /// Text edits in LSP coordinates relative to the entry's `before` text.
    Edit {
        path: PathBuf,
        edits: Vec<lt::TextEdit>,
    },
    Create {
        path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Delete {
        path: PathBuf,
        recursive: bool,
        ignore_if_not_exists: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewEntry {
    pub kind: EntryKind,
    /// Applied when the preview is applied (the per-file checkbox).
    pub include: bool,
    /// Text before and after this entry (LF line endings; empty for directories).
    pub before: String,
    pub after: String,
}

impl PreviewEntry {
    /// The file the entry is about (the source of a rename).
    pub fn path(&self) -> &Path {
        match &self.kind {
            EntryKind::Edit { path, .. } | EntryKind::Create { path, .. } | EntryKind::Delete { path, .. } => path,
            EntryKind::Rename { from, .. } => from,
        }
    }

    /// "Edit src/main.rs", "Rename src/a.rs → src/b.rs", … (paths relative to `root`).
    pub fn label(&self, root: Option<&Path>) -> String {
        let rel = |p: &Path| match root.and_then(|r| p.strip_prefix(r).ok()) {
            Some(r) if !r.as_os_str().is_empty() => r.display().to_string(),
            _ => p.display().to_string(),
        };
        match &self.kind {
            EntryKind::Edit { path, edits } => format!("{} ({} change{})", rel(path), edits.len(), plural(edits.len())),
            EntryKind::Create { path, .. } => format!("Create {}", rel(path)),
            EntryKind::Rename { from, to, .. } => format!("Rename {} → {}", rel(from), rel(to)),
            EntryKind::Delete { path, .. } => format!("Delete {}", rel(path)),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditPreview {
    /// What produced the edit ("Rename 'greet' to 'salute'", a code action title, …).
    pub title: String,
    pub entries: Vec<PreviewEntry>,
    /// The entry whose diff is shown.
    pub selected: usize,
    /// Run after applying (e.g. `workspace/didRenameFiles` for a move).
    #[serde(default)]
    pub notify_renames: bool,
}

impl EditPreview {
    /// Distinct files touched (sources and targets of renames count once each).
    pub fn files(&self) -> usize {
        let mut set = BTreeSet::new();
        for e in &self.entries {
            set.insert(e.path().to_path_buf());
            if let EntryKind::Rename { to, .. } = &e.kind {
                set.insert(to.clone());
            }
        }
        set.len()
    }

    /// RustRover applies single-file edits at once and previews the rest.
    pub fn needs_preview(&self) -> bool {
        self.files() > 1 || self.entries.iter().any(|e| !matches!(e.kind, EntryKind::Edit { .. }))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A unified diff of every included entry (what agents read before applying).
    pub fn unified_diff(&self, root: Option<&Path>) -> String {
        let mut out = String::new();
        for e in self.entries.iter().filter(|e| e.include) {
            let name = e.label(root);
            match &e.kind {
                EntryKind::Edit { .. } => {
                    out.push_str(&codecraft_agent::diff::unified_text(&e.before, &e.after, &name, &name));
                }
                _ => {
                    out.push_str("# ");
                    out.push_str(&name);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// What applying did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub applied: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
    /// Files whose content or location changed.
    pub files: BTreeSet<PathBuf>,
}

// ---------------------------------------------------------------- building

/// Text normalized like the editor holds it (`\n` line endings).
pub(crate) fn normalized(text: &str) -> String {
    Document::from_text(text).text()
}

/// Apply LSP text edits to a text (LSP coordinates relative to it).
pub(crate) fn apply_edits_to_text(text: &str, edits: &[lt::TextEdit]) -> Result<String, String> {
    let mut doc = Document::from_text(text);
    let changes = convert::text_edits_to_changes(&doc, edits).map_err(|e| e.to_string())?;
    let len = changes.len_after(doc.len_chars());
    let sels = SelectionSet::default().clamp(len);
    doc.apply(&Edit { changes, selections: sels }, &SelectionSet::default(), EditGroup::Standalone)
        .map_err(|e| e.to_string())?;
    Ok(doc.text())
}

/// The virtual file system the operations are simulated on.
struct Vfs<'a> {
    read: &'a dyn Fn(&Path) -> Option<String>,
    /// Known contents (`None` = does not exist). Directories are not tracked, only files.
    files: BTreeMap<PathBuf, Option<String>>,
    /// Renames so far (a path under `to` was at `from` on disk).
    renames: Vec<(PathBuf, PathBuf)>,
    /// Deleted paths (anything under them no longer exists).
    deleted: Vec<PathBuf>,
}

impl Vfs<'_> {
    fn get(&mut self, path: &Path) -> Option<String> {
        if let Some(v) = self.files.get(path) {
            return v.clone();
        }
        if self.deleted.iter().any(|d| path.starts_with(d)) {
            return None;
        }
        // Map back through renames to where the file is on disk now.
        let mut disk = path.to_path_buf();
        for (from, to) in self.renames.iter().rev() {
            if let Ok(rel) = disk.strip_prefix(to) {
                disk = if rel.as_os_str().is_empty() { from.clone() } else { from.join(rel) };
            }
        }
        let v = (self.read)(&disk).map(|t| normalized(&t));
        self.files.insert(path.to_path_buf(), v.clone());
        v
    }

    fn set(&mut self, path: &Path, v: Option<String>) {
        self.files.insert(path.to_path_buf(), v);
    }

    fn rename(&mut self, from: &Path, to: &Path) {
        let moved: Vec<(PathBuf, Option<String>)> =
            self.files.iter().filter(|(p, _)| p.starts_with(from)).map(|(p, v)| (p.clone(), v.clone())).collect();
        for (p, v) in moved {
            self.files.insert(p.clone(), None);
            if let Ok(rel) = p.strip_prefix(from) {
                let target = if rel.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rel) };
                self.files.insert(target, v);
            }
        }
        self.deleted.retain(|d| !d.starts_with(to));
        self.renames.push((from.to_path_buf(), to.to_path_buf()));
    }

    fn delete(&mut self, path: &Path) {
        for (p, v) in self.files.iter_mut() {
            if p.starts_with(path) {
                *v = None;
            }
        }
        self.deleted.push(path.to_path_buf());
    }
}

fn uri_path(uri: &lt::Uri) -> Result<PathBuf, String> {
    codecraft_lsp::uri_to_path(uri).ok_or_else(|| format!("unsupported URI {}", uri.as_str()))
}

fn flag(v: Option<bool>) -> bool {
    v.unwrap_or(false)
}

/// Turn a workspace edit into preview entries. `read` returns a file's current text (the open
/// document's, else the disk's), `None` when it does not exist.
pub fn build(
    title: &str,
    edit: &lt::WorkspaceEdit,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Result<EditPreview, String> {
    simulate(title, edit_ops(edit)?, read)
}

/// The operations of a workspace edit, in order (`documentChanges` wins over `changes`).
pub fn edit_ops(edit: &lt::WorkspaceEdit) -> Result<Vec<EntryKind>, String> {
    let mut ops: Vec<EntryKind> = Vec::new();
    if let Some(dc) = &edit.document_changes {
        let list: Vec<lt::DocumentChangeOperation> = match dc {
            lt::DocumentChanges::Edits(v) => v.iter().cloned().map(lt::DocumentChangeOperation::Edit).collect(),
            lt::DocumentChanges::Operations(v) => v.clone(),
        };
        for op in list {
            ops.push(match op {
                lt::DocumentChangeOperation::Edit(e) => EntryKind::Edit {
                    path: uri_path(&e.text_document.uri)?,
                    edits: e
                        .edits
                        .into_iter()
                        .map(|te| match te {
                            lt::OneOf::Left(t) => t,
                            lt::OneOf::Right(a) => a.text_edit,
                        })
                        .collect(),
                },
                lt::DocumentChangeOperation::Op(lt::ResourceOp::Create(c)) => {
                    let o = c.options.as_ref();
                    EntryKind::Create {
                        path: uri_path(&c.uri)?,
                        overwrite: flag(o.and_then(|o| o.overwrite)),
                        ignore_if_exists: flag(o.and_then(|o| o.ignore_if_exists)),
                    }
                }
                lt::DocumentChangeOperation::Op(lt::ResourceOp::Rename(r)) => {
                    let o = r.options.as_ref();
                    EntryKind::Rename {
                        from: uri_path(&r.old_uri)?,
                        to: uri_path(&r.new_uri)?,
                        overwrite: flag(o.and_then(|o| o.overwrite)),
                        ignore_if_exists: flag(o.and_then(|o| o.ignore_if_exists)),
                    }
                }
                lt::DocumentChangeOperation::Op(lt::ResourceOp::Delete(d)) => {
                    let o = d.options.as_ref();
                    EntryKind::Delete {
                        path: uri_path(&d.uri)?,
                        recursive: flag(o.and_then(|o| o.recursive)),
                        ignore_if_not_exists: flag(o.and_then(|o| o.ignore_if_not_exists)),
                    }
                }
            });
        }
    } else if let Some(changes) = &edit.changes {
        let mut per_file: BTreeMap<PathBuf, Vec<lt::TextEdit>> = BTreeMap::new();
        for (uri, edits) in changes {
            per_file.entry(uri_path(uri)?).or_default().extend(edits.iter().cloned());
        }
        ops.extend(per_file.into_iter().map(|(path, edits)| EntryKind::Edit { path, edits }));
    }
    if ops.len() > MAX_ENTRIES {
        return Err(format!("the refactoring touches too many files ({})", ops.len()));
    }
    Ok(ops)
}

/// Build a preview from explicit operations (file moves, safe delete).
pub fn from_ops(
    title: &str,
    ops: Vec<EntryKind>,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Result<EditPreview, String> {
    simulate(title, ops, read)
}

fn simulate(title: &str, ops: Vec<EntryKind>, read: &dyn Fn(&Path) -> Option<String>) -> Result<EditPreview, String> {
    let mut vfs = Vfs { read, files: BTreeMap::new(), renames: Vec::new(), deleted: Vec::new() };
    let mut entries = Vec::with_capacity(ops.len());
    for kind in ops {
        let (before, after) = match &kind {
            EntryKind::Edit { path, edits } => {
                let Some(before) = vfs.get(path) else {
                    return Err(format!("{} does not exist", path.display()));
                };
                let after = apply_edits_to_text(&before, edits).map_err(|e| format!("{}: {e}", path.display()))?;
                vfs.set(path, Some(after.clone()));
                (before, after)
            }
            EntryKind::Create { path, overwrite, .. } => {
                let existing = vfs.get(path);
                if existing.is_none() || *overwrite {
                    vfs.set(path, Some(String::new()));
                }
                (existing.unwrap_or_default(), String::new())
            }
            EntryKind::Rename { from, to, .. } => {
                let text = vfs.get(from).unwrap_or_default();
                vfs.rename(from, to);
                (text.clone(), text)
            }
            EntryKind::Delete { path, .. } => {
                let text = vfs.get(path).unwrap_or_default();
                vfs.delete(path);
                (text, String::new())
            }
        };
        entries.push(PreviewEntry { kind, include: true, before, after });
    }
    Ok(EditPreview { title: title.to_string(), entries, selected: 0, notify_renames: false })
}

// ---------------------------------------------------------------- applying

impl Session {
    /// A file's current text for previews: the open document's, else the disk's (`\n` line
    /// endings either way).
    pub(crate) fn current_text(&self, path: &Path) -> Option<String> {
        if let Some(d) = self.ws.doc_by_path(path).and_then(|id| self.ws.doc(id)) {
            return Some(d.doc.text());
        }
        if !path.is_file() {
            return None;
        }
        codecraft_fs::read_text_file(path).ok().map(|t| normalized(&t))
    }

    /// Build a preview of `edit` against the current documents and files.
    pub(crate) fn build_preview(&self, title: &str, edit: &lt::WorkspaceEdit) -> Result<EditPreview, CommandError> {
        build(title, edit, &|p| self.current_text(p)).map_err(CommandError::Failed)
    }

    /// Apply the included entries in order. Entries whose file changed since the preview was
    /// built are skipped and reported; the rest still apply.
    pub fn apply_preview(&mut self, preview: &EditPreview) -> ApplyReport {
        let mut report = ApplyReport::default();
        // Excluded renames: later entries addressed to the new path go to the old one.
        let mut alias: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut file_ops = false;
        let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
        for e in &preview.entries {
            if !e.include {
                report.skipped += 1;
                if let EntryKind::Rename { from, to, .. } = &e.kind {
                    alias.push((to.clone(), from.clone()));
                }
                continue;
            }
            if crate::remote::external_cache(self).is_some_and(|cache| e.path().starts_with(&cache)
                || matches!(&e.kind, EntryKind::Rename { to, .. } if to.starts_with(&cache))) {
                report.skipped += 1;
                report.errors.push("Remote toolchain and registry sources are read-only".into());
                continue;
            }
            let resolve = |p: &Path| -> PathBuf {
                let mut out = p.to_path_buf();
                for (to, from) in alias.iter().rev() {
                    if let Ok(rel) = out.strip_prefix(to) {
                        out = if rel.as_os_str().is_empty() { from.clone() } else { from.join(rel) };
                    }
                }
                out
            };
            let result = match &e.kind {
                EntryKind::Edit { path, edits } => {
                    let path = resolve(path);
                    self.apply_entry_edit(&path, &e.before, edits).map(|()| report.files.insert(path))
                }
                EntryKind::Create { path, overwrite, ignore_if_exists } => {
                    file_ops = true;
                    let path = resolve(path);
                    create_file(&path, *overwrite, *ignore_if_exists).map(|()| report.files.insert(path))
                }
                EntryKind::Rename { from, to, overwrite, ignore_if_exists } => {
                    file_ops = true;
                    let (from, to) = (resolve(from), resolve(to));
                    self.rename_path(&from, &to, *overwrite, *ignore_if_exists).map(|done| {
                        if done {
                            renamed.push((from.clone(), to.clone()));
                        }
                        report.files.insert(from);
                        report.files.insert(to)
                    })
                }
                EntryKind::Delete { path, recursive, ignore_if_not_exists } => {
                    file_ops = true;
                    let path = resolve(path);
                    self.delete_path(&path, *recursive, *ignore_if_not_exists).map(|()| report.files.insert(path))
                }
            };
            match result {
                Ok(_) => report.applied += 1,
                Err(msg) => {
                    report.skipped += 1;
                    report.errors.push(msg);
                }
            }
        }
        if file_ops {
            self.jobs_rescan();
        }
        if self.remote.project.is_some() {
            // Publish only entries whose local transaction succeeded. Content-only edits remain
            // unsaved buffers and use the usual checked upload when saved.
            for entry in preview.entries.iter().filter(|entry| entry.include) {
                let result = match &entry.kind {
                    EntryKind::Create { path, .. } if report.files.contains(path) => crate::remote::tree_operation(self, path, None, false, true),
                    EntryKind::Rename { from, to, .. } if renamed.iter().any(|(a,b)| a == from && b == to) => crate::remote::tree_operation(self, from, Some(to), to.is_dir(), false),
                    EntryKind::Delete { path, .. } if report.files.contains(path) => {
                        let directory = self.project_root().and_then(|root| crate::remote::mirror_paths(&root).ok().map(|(_, base)| (root, base)))
                            .and_then(|(root, base)| path.strip_prefix(root).ok().map(|p| base.join(p).is_dir())).unwrap_or(false);
                        crate::remote::tree_operation(self, path, None, directory, false)
                    }
                    _ => Ok(()),
                };
                if let Err(error) = result { report.errors.push(error.to_string()); }
            }
            let saved: Vec<_> = report.files.iter().filter(|path| path.is_file() && self.ws.doc_by_path(path)
                .and_then(|id| self.ws.doc(id)).is_none_or(|d| !d.doc.is_modified())).cloned().collect();
            crate::remote::after_save(self, &saved);
        }
        if preview.notify_renames && !renamed.is_empty() {
            self.notify_did_rename(&renamed);
        }
        if !report.errors.is_empty() {
            self.notify(NotificationLevel::Error, &preview.title, report.errors.join("\n"));
        }
        report
    }

    fn apply_entry_edit(&mut self, path: &Path, before: &str, edits: &[lt::TextEdit]) -> Result<(), String> {
        let current = self.current_text(path).ok_or_else(|| format!("{} does not exist", path.display()))?;
        if current != before {
            return Err(format!("{} changed since the preview was built; run the refactoring again", path.display()));
        }
        if let Some(id) = self.ws.doc_by_path(path) {
            if self.ws.doc(id).is_some_and(|d| d.read_only) {
                return Err(format!("{} is read-only", path.display()));
            }
            return self.apply_text_edits_to_path(path, edits).map_err(|e| e.to_string());
        }
        let disk = codecraft_fs::read_text_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut doc = Document::from_text(&disk);
        let changes = convert::text_edits_to_changes(&doc, edits).map_err(|e| e.to_string())?;
        doc.apply(
            &Edit { changes, selections: SelectionSet::default() },
            &SelectionSet::default(),
            EditGroup::Standalone,
        )
        .map_err(|e| e.to_string())?;
        codecraft_fs::write_text_file_atomic(path, &doc.to_disk_text()).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Write open, modified documents under `root` (a file or directory) to disk.
    pub(crate) fn save_docs_under(&mut self, root: &Path) -> Result<(), String> {
        let mut saved = Vec::new();
        for od in self.ws.docs_mut() {
            let Some(p) = od.path.clone() else { continue };
            if !p.starts_with(root) || !od.doc.is_modified() {
                continue;
            }
            codecraft_fs::write_text_file_atomic(&p, &od.doc.to_disk_text())
                .map_err(|e| format!("{}: {e}", p.display()))?;
            od.doc.mark_saved();
            od.disk_mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            saved.push(p);
        }
        for p in &saved {
            self.lsp_did_save(p);
        }
        Ok(())
    }

    /// Move a file or directory on disk and re-point open documents. Returns false when skipped.
    pub(crate) fn rename_path(
        &mut self,
        from: &Path,
        to: &Path,
        overwrite: bool,
        ignore_if_exists: bool,
    ) -> Result<bool, String> {
        if !from.exists() {
            return Err(format!("{} does not exist", from.display()));
        }
        if to.starts_with(from) && to != from {
            return Err(format!("cannot move {} into itself", from.display()));
        }
        if to.exists() && to != from && !super::file_ops::same_file(from, to) {
            if ignore_if_exists && !overwrite {
                return Ok(false);
            }
            if !overwrite {
                return Err(format!("{} already exists", to.display()));
            }
            remove_any(to)?;
        }
        self.save_docs_under(from)?;
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::rename(from, to).map_err(|e| format!("cannot move {}: {e}", from.display()))?;
        // Re-point documents (the file itself or everything under a moved directory).
        let mut moved: Vec<(codecraft_workspace::DocId, PathBuf, PathBuf)> = Vec::new();
        for od in self.ws.docs() {
            let Some(p) = &od.path else { continue };
            if let Ok(rel) = p.strip_prefix(from) {
                let np = if rel.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rel) };
                moved.push((od.id, p.clone(), np));
            }
        }
        for (id, old, new) in moved {
            self.lsp_did_close(&old);
            if let Some(od) = self.ws.doc_mut(id) {
                od.language = Language::from_path(&new);
                od.path = Some(new.clone());
                od.lsp_synced = None;
                od.semantic = None;
                od.inlay_hints = None;
                od.disk_mtime = std::fs::metadata(&new).and_then(|m| m.modified()).ok();
            }
            self.lsp_did_open(id);
        }
        if let Some(p) = &mut self.ws.project {
            if p.selected.as_deref().is_some_and(|s| s.starts_with(from)) {
                p.selected = Some(to.to_path_buf());
            }
            let expanded: Vec<PathBuf> = p.expanded.iter().filter(|e| e.starts_with(from)).cloned().collect();
            for e in expanded {
                p.expanded.remove(&e);
                if let Ok(rel) = e.strip_prefix(from) {
                    p.expanded.insert(if rel.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rel) });
                }
            }
        }
        Ok(true)
    }

    /// Delete a file or directory, closing its editors without saving.
    pub(crate) fn delete_path(
        &mut self,
        path: &Path,
        recursive: bool,
        ignore_if_not_exists: bool,
    ) -> Result<(), String> {
        if !path.exists() {
            return if ignore_if_not_exists { Ok(()) } else { Err(format!("{} does not exist", path.display())) };
        }
        if path.is_dir() {
            if recursive {
                std::fs::remove_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
            } else {
                std::fs::remove_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        } else {
            std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        self.forget_docs_under(path);
        if let Some(p) = &mut self.ws.project {
            if p.selected.as_deref().is_some_and(|s| s.starts_with(path)) {
                p.selected = path.parent().map(Path::to_path_buf);
            }
            p.expanded.retain(|e| !e.starts_with(path));
        }
        Ok(())
    }

    /// Close every editor of documents under `root` (no save) and detach documents left behind
    /// from their path, so Save All can never resurrect a deleted file.
    fn forget_docs_under(&mut self, root: &Path) {
        let docs: Vec<(codecraft_workspace::DocId, PathBuf)> =
            self.ws.docs().filter_map(|d| d.path.clone().filter(|p| p.starts_with(root)).map(|p| (d.id, p))).collect();
        for (id, path) in docs {
            // Every split group showing it.
            for e in self.ws.editors_for_doc(id) {
                let _: Option<_> = self.ws.close_editor(e);
            }
            self.lsp_did_close(&path);
            if let Some(od) = self.ws.doc_mut(id) {
                od.path = None;
                od.doc.mark_saved();
            }
        }
    }
}

fn create_file(path: &Path, overwrite: bool, ignore_if_exists: bool) -> Result<(), String> {
    if path.exists() {
        if overwrite {
            return codecraft_fs::write_text_file_atomic(path, "").map_err(|e| format!("{}: {e}", path.display()));
        }
        return if ignore_if_exists { Ok(()) } else { Err(format!("{} already exists", path.display())) };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    codecraft_fs::write_text_file_atomic(path, "").map_err(|e| format!("{}: {e}", path.display()))
}

fn remove_any(path: &Path) -> Result<(), String> {
    let r = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    r.map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    // `WorkspaceEdit.changes` is keyed by `lsp_types::Uri`, which clippy flags as a mutable key.
    #![allow(clippy::unwrap_used, clippy::mutable_key_type)]

    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    fn uri(p: &Path) -> lt::Uri {
        codecraft_lsp::path_to_uri(p).unwrap()
    }

    fn te(line: u32, c0: u32, c1: u32, text: &str) -> lt::TextEdit {
        lt::TextEdit {
            range: lt::Range { start: lt::Position::new(line, c0), end: lt::Position::new(line, c1) },
            new_text: text.into(),
        }
    }

    fn reader(files: &[(&Path, &str)]) -> impl Fn(&Path) -> Option<String> + use<> {
        let map: HashMap<PathBuf, String> = files.iter().map(|(p, t)| (p.to_path_buf(), (*t).to_string())).collect();
        move |p| map.get(p).cloned()
    }

    #[test]
    fn changes_map_becomes_one_entry_per_file_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.rs"), tmp.path().join("b.rs"));
        let mut changes = HashMap::new();
        changes.insert(uri(&b), vec![te(0, 3, 8, "salute")]);
        changes.insert(uri(&a), vec![te(1, 4, 9, "salute"), te(0, 0, 0, "// x\n")]);
        let edit = lt::WorkspaceEdit { changes: Some(changes), ..Default::default() };
        let read = reader(&[(&a, "use b;\n    greet();\n"), (&b, "fn greet() {}\r\n")]);
        let p = build("Rename", &edit, &read).unwrap();
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0].path(), a.as_path());
        assert_eq!(p.entries[0].after, "// x\nuse b;\n    salute();\n");
        // CRLF on disk is normalized like the editor does.
        assert_eq!(p.entries[1].before, "fn greet() {}\n");
        assert_eq!(p.entries[1].after, "fn salute() {}\n");
        assert_eq!(p.files(), 2);
        assert!(p.needs_preview());
        assert!(p.unified_diff(Some(tmp.path())).contains("+fn salute() {}"));
    }

    #[test]
    fn document_changes_are_simulated_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new, created) = (tmp.path().join("foo.rs"), tmp.path().join("bar.rs"), tmp.path().join("n/x.rs"));
        let value = json!({ "documentChanges": [
            { "kind": "rename", "oldUri": uri(&old).as_str(), "newUri": uri(&new).as_str() },
            { "textDocument": { "uri": uri(&new).as_str(), "version": null },
              "edits": [{ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "bar" }] },
            { "kind": "create", "uri": uri(&created).as_str() },
            { "textDocument": { "uri": uri(&created).as_str(), "version": null },
              "edits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "// new\n" }] },
            { "kind": "delete", "uri": uri(&tmp.path().join("gone.rs")).as_str(), "options": { "ignoreIfNotExists": true } },
        ]});
        let edit: lt::WorkspaceEdit = serde_json::from_value(value).unwrap();
        let p = build("Move", &edit, &reader(&[(&old, "fn foo() {}\n")])).unwrap();
        assert_eq!(p.entries.len(), 5);
        assert!(matches!(&p.entries[0].kind, EntryKind::Rename { .. }));
        assert_eq!(p.entries[1].before, "fn foo() {}\n");
        assert_eq!(p.entries[1].after, "fn bar() {}\n");
        assert_eq!(p.entries[3].before, "");
        assert_eq!(p.entries[3].after, "// new\n");
        assert!(p.needs_preview());
        assert!(p.entries[0].label(Some(tmp.path())).contains("foo.rs → bar.rs"));
    }

    #[test]
    fn bad_edits_are_errors_not_panics() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        let mut changes = HashMap::new();
        changes.insert(uri(&a), vec![te(0, 0, 5, "x"), te(0, 2, 3, "y")]);
        let edit = lt::WorkspaceEdit { changes: Some(changes.clone()), ..Default::default() };
        assert!(build("t", &edit, &reader(&[(&a, "hello\n")])).unwrap_err().contains("a.rs"));
        // Missing file.
        assert!(build("t", &edit, &reader(&[])).unwrap_err().contains("does not exist"));
        // Out-of-range positions clamp instead of panicking.
        changes.insert(uri(&a), vec![te(u32::MAX, u32::MAX, u32::MAX, "!")]);
        let edit = lt::WorkspaceEdit { changes: Some(changes), ..Default::default() };
        assert_eq!(build("t", &edit, &reader(&[(&a, "hi")])).unwrap().entries[0].after, "hi!");
    }

    #[test]
    fn single_file_edit_needs_no_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        let mut changes = HashMap::new();
        changes.insert(uri(&a), vec![te(0, 0, 0, "x")]);
        let p = build("t", &lt::WorkspaceEdit { changes: Some(changes), ..Default::default() }, &reader(&[(&a, "")]))
            .unwrap();
        assert!(!p.needs_preview());
        assert_eq!(p.files(), 1);
    }

    fn session_with(dir: &Path, files: &[(&str, &str)]) -> Session {
        for (name, text) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let mut s = Session::new();
        s.lsp_enabled = false;
        s
    }

    fn changes(edits: Vec<(PathBuf, Vec<lt::TextEdit>)>) -> lt::WorkspaceEdit {
        let map = edits.into_iter().map(|(p, e)| (uri(&p), e)).collect();
        lt::WorkspaceEdit { changes: Some(map), ..Default::default() }
    }

    #[test]
    fn apply_edits_open_documents_undoably_and_writes_closed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.rs"), tmp.path().join("b.rs"));
        let mut s =
            session_with(tmp.path(), &[("a.rs", "fn greet() {}\n"), ("b.rs", "fn main() {\r\n    greet();\r\n}\r\n")]);
        s.open_file(&a).unwrap();
        let edit = changes(vec![(a.clone(), vec![te(0, 3, 8, "salute")]), (b.clone(), vec![te(1, 4, 9, "salute")])]);
        let p = s.build_preview("Rename", &edit).unwrap();
        let report = s.apply_preview(&p);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.applied, 2);
        // The open document changed in memory (not saved), the closed file on disk, CRLF kept.
        let id = s.ws.doc_by_path(&a).unwrap();
        assert_eq!(s.ws.doc(id).unwrap().doc.text(), "fn salute() {}\n");
        assert!(s.ws.doc(id).unwrap().doc.is_modified());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn greet() {}\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "fn main() {\r\n    salute();\r\n}\r\n");
        assert!(s.ws.doc_by_path(&b).is_none(), "closed files are not loaded as documents");
        // One undo step restores the open document.
        s.execute("edit.undo", &serde_json::Value::Null).unwrap();
        assert_eq!(s.ws.doc(id).unwrap().doc.text(), "fn greet() {}\n");
    }

    #[test]
    fn stale_and_excluded_entries_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.rs"), tmp.path().join("b.rs"));
        let mut s = session_with(tmp.path(), &[("a.rs", "aaa\n"), ("b.rs", "bbb\n")]);
        let edit = changes(vec![(a.clone(), vec![te(0, 0, 3, "AAA")]), (b.clone(), vec![te(0, 0, 3, "BBB")])]);
        let mut p = s.build_preview("t", &edit).unwrap();
        p.entries[1].include = false;
        std::fs::write(&a, "changed meanwhile\n").unwrap();
        let report = s.apply_preview(&p);
        assert_eq!(report.applied, 0);
        assert_eq!(report.skipped, 2);
        assert!(report.errors[0].contains("changed since the preview"));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "changed meanwhile\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bbb\n");
    }

    #[test]
    fn renames_and_deletes_follow_open_documents() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new, gone) =
            (tmp.path().join("src/foo.rs"), tmp.path().join("src/bar.rs"), tmp.path().join("gone.rs"));
        let mut s = session_with(tmp.path(), &[("src/foo.rs", "fn foo() {}\n"), ("gone.rs", "x\n")]);
        s.open_file(&old).unwrap();
        s.open_file(&gone).unwrap();
        // Unsaved edits in the moved file are saved before the move.
        s.execute("editor.type", &serde_json::json!({ "text": "// edited\n" })).ok();
        let ops = vec![
            EntryKind::Rename { from: old.clone(), to: new.clone(), overwrite: false, ignore_if_exists: false },
            EntryKind::Edit { path: new.clone(), edits: vec![te(0, 0, 0, "// moved\n")] },
            EntryKind::Delete { path: gone.clone(), recursive: false, ignore_if_not_exists: false },
        ];
        let p = from_ops("Move", ops, &|p| s.current_text(p)).unwrap();
        let report = s.apply_preview(&p);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!old.exists() && new.exists() && !gone.exists());
        let id = s.ws.doc_by_path(&new).expect("the open document follows the move");
        assert!(s.ws.doc(id).unwrap().doc.text().starts_with("// moved\n"));
        assert!(s.ws.doc_by_path(&gone).is_none());
        assert!(s.ws.docs().all(|d| d.path.as_deref() != Some(gone.as_path())));
        // Save All never resurrects the deleted file.
        s.save_all().unwrap();
        assert!(!gone.exists());
    }

    #[test]
    fn excluded_rename_redirects_later_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join("foo.rs"), tmp.path().join("bar.rs"));
        let mut s = session_with(tmp.path(), &[("foo.rs", "fn foo() {}\n")]);
        let ops = vec![
            EntryKind::Rename { from: old.clone(), to: new.clone(), overwrite: false, ignore_if_exists: false },
            EntryKind::Edit { path: new.clone(), edits: vec![te(0, 3, 6, "bar")] },
        ];
        let mut p = from_ops("Move", ops, &|p| s.current_text(p)).unwrap();
        p.entries[0].include = false;
        let report = s.apply_preview(&p);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "fn bar() {}\n");
        assert!(!new.exists());
    }

    #[test]
    fn rename_refuses_to_clobber() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.rs"), tmp.path().join("b.rs"));
        let mut s = session_with(tmp.path(), &[("a.rs", "a"), ("b.rs", "b")]);
        let err = s.rename_path(&a, &b, false, false).unwrap_err();
        assert!(err.contains("already exists"));
        assert_eq!(s.rename_path(&a, &b, false, true), Ok(false));
        assert!(s.rename_path(&tmp.path().join("missing"), &b, false, false).is_err());
        assert!(s.rename_path(tmp.path(), &tmp.path().join("inner"), false, false).is_err());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
    }
}
