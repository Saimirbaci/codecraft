//! The three-way merge view for a conflicted file (Git › Resolve Conflicts › Merge…): our
//! version on the left, theirs on the right, the result in the middle. Non-conflicting changes
//! are applied up front; each conflict is resolved by accepting the left side, the right side,
//! both, or neither. Applying writes the result and marks the file resolved (`git add`).
//!
//! The three versions come from the index stages (`:1:` base, `:2:` ours, `:3:` theirs) and are
//! loaded on a worker thread ([`crate::vcs::open_merge`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_vcs::merge3::{self, ChunkKind, MergeChunk, Resolution};
use codecraft_workspace::NotificationLevel;

use crate::{CommandError, Session};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeView {
    /// Repository-relative path.
    pub path: String,
    pub abs: PathBuf,
    pub base: String,
    pub ours: String,
    pub theirs: String,
    /// Conflict marker labels (`HEAD`, `MERGE_HEAD`, …).
    pub ours_label: String,
    pub theirs_label: String,
    pub chunks: Vec<MergeChunk>,
    /// One per chunk; `None` = unresolved (only conflicts start that way).
    pub resolutions: Vec<Option<Resolution>>,
    /// The chunk the view is focused on.
    pub selected: usize,
}

impl MergeView {
    pub fn new(path: String, abs: PathBuf, base: String, ours: String, theirs: String, labels: (String, String)) -> Self {
        let chunks = merge3::chunks(&base, &ours, &theirs);
        let resolutions = chunks.iter().map(|c| Resolution::natural(c.kind)).collect();
        let mut v = Self {
            path,
            abs,
            base,
            ours,
            theirs,
            ours_label: labels.0,
            theirs_label: labels.1,
            chunks,
            resolutions,
            selected: 0,
        };
        v.selected = v.next_unresolved(0).unwrap_or(0);
        v
    }

    /// Conflicting chunks in total.
    pub fn conflicts(&self) -> usize {
        self.chunks.iter().filter(|c| c.kind == ChunkKind::Conflict).count()
    }

    pub fn unresolved(&self) -> usize {
        self.resolutions.iter().filter(|r| r.is_none()).count()
    }

    /// The first unresolved chunk at or after `from` (wrapping around).
    pub fn next_unresolved(&self, from: usize) -> Option<usize> {
        let n = self.resolutions.len();
        (0..n).map(|i| (from + i) % n.max(1)).find(|i| self.resolutions.get(*i).is_some_and(Option::is_none))
    }

    /// The merged text (unresolved chunks with conflict markers) and how many are unresolved.
    pub fn result(&self) -> (String, usize) {
        merge3::compose(
            &self.base,
            &self.ours,
            &self.theirs,
            &self.chunks,
            &self.resolutions,
            (&self.ours_label, &self.theirs_label),
        )
    }

    pub fn resolve(&mut self, index: usize, r: Option<Resolution>) -> Result<(), CommandError> {
        let slot = self
            .resolutions
            .get_mut(index)
            .ok_or_else(|| CommandError::BadParams(format!("there is no change #{index}")))?;
        *slot = r;
        if r.is_some() {
            self.selected = self.next_unresolved(index).unwrap_or(index);
        } else {
            self.selected = index;
        }
        Ok(())
    }

    /// Resolve every conflict with one side (Accept Left / Accept Right for the whole file).
    pub fn resolve_all(&mut self, r: Resolution) {
        for (slot, c) in self.resolutions.iter_mut().zip(&self.chunks) {
            *slot = match (c.kind, r) {
                // A change made on the other side only doesn't conflict: it stays applied.
                (ChunkKind::Ours, Resolution::Theirs) | (ChunkKind::Theirs, Resolution::Ours) => {
                    Some(Resolution::natural(c.kind).unwrap_or(r))
                }
                _ => Some(r),
            };
        }
    }

    /// Apply every non-conflicting change (the view's starting state).
    pub fn apply_non_conflicting(&mut self) {
        for (slot, c) in self.resolutions.iter_mut().zip(&self.chunks) {
            if c.kind != ChunkKind::Conflict {
                *slot = Resolution::natural(c.kind);
            }
        }
    }

    /// The chunk's text in each version: (base, ours, theirs).
    pub fn texts(&self, index: usize) -> Option<(String, String, String)> {
        let c = self.chunks.get(index)?;
        let (b, o, t) = (merge3::split_lines(&self.base), merge3::split_lines(&self.ours), merge3::split_lines(&self.theirs));
        Some((merge3::slice(&b, &c.base), merge3::slice(&o, &c.ours), merge3::slice(&t, &c.theirs)))
    }

    pub fn to_json(&self) -> Value {
        let (result, unresolved) = self.result();
        let chunks: Vec<Value> = (0..self.chunks.len())
            .filter_map(|i| {
                let c = self.chunks.get(i)?;
                let (base, ours, theirs) = self.texts(i)?;
                Some(json!({
                    "index": i,
                    "kind": c.kind,
                    "base": [c.base.start, c.base.end],
                    "ours": [c.ours.start, c.ours.end],
                    "theirs": [c.theirs.start, c.theirs.end],
                    "resolution": self.resolutions.get(i).copied().flatten(),
                    "baseText": base,
                    "oursText": ours,
                    "theirsText": theirs,
                }))
            })
            .collect();
        json!({
            "path": self.path,
            "chunks": chunks,
            "conflicts": self.conflicts(),
            "unresolved": unresolved,
            "selected": self.selected,
            "result": result,
        })
    }
}

/// Parse a resolution param (`"none"` clears it).
pub(crate) fn parse_resolution(s: &str) -> Result<Option<Resolution>, CommandError> {
    if s == "none" || s == "unresolved" {
        return Ok(None);
    }
    Resolution::parse(s).map(Some).ok_or_else(|| {
        CommandError::BadParams(format!("unknown resolution '{s}' (ours, theirs, both, theirsThenOurs, base, none)"))
    })
}

/// Write the merge result to the file (and its open editor) and mark it resolved.
pub(crate) fn apply(s: &mut Session, allow_unresolved: bool) -> Result<(), CommandError> {
    let view = s.vcs.merge.as_ref().ok_or_else(|| CommandError::Failed("no merge is open".into()))?;
    let (text, unresolved) = view.result();
    if unresolved > 0 && !allow_unresolved {
        return Err(CommandError::Failed(format!(
            "{unresolved} conflict{} left: resolve {} first",
            if unresolved == 1 { " is" } else { "s are" },
            if unresolved == 1 { "it" } else { "them" }
        )));
    }
    let (abs, rel) = (view.abs.clone(), view.path.clone());
    codecraft_fs::write_text_file_atomic(&abs, &text).map_err(|e| CommandError::Failed(e.to_string()))?;
    // An open editor shows the result (undoably) and is in sync with the disk.
    if let Some(id) = s.ws.doc_by_path(&abs) {
        let editor = s.ws.editor_for_doc(id);
        let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        if let Some(od) = s.ws.doc_mut(id) {
            od.doc.replace_all(&text, &sels).map_err(|e| CommandError::Failed(e.to_string()))?;
            od.doc.mark_saved();
            od.disk_mtime = std::fs::metadata(&abs).and_then(|m| m.modified()).ok();
        }
        if let Some(e) = editor {
            s.after_text_change(e);
        }
    }
    s.vcs.merge = None;
    if unresolved > 0 {
        s.notify(NotificationLevel::Warning, "Merge", format!("{rel} saved with {unresolved} unresolved conflicts"));
        return Ok(());
    }
    crate::vcs::simple_op(s, "Mark Resolved", false, move |git| codecraft_vcs::ops::mark_resolved(git, &rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> MergeView {
        MergeView::new(
            "f.txt".into(),
            PathBuf::from("/nonexistent/f.txt"),
            "a\nb\nc\nd\n".into(),
            "A\nb\nc\nOURS\n".into(),
            "a\nb\nc\nTHEIRS\n".into(),
            ("HEAD".into(), "MERGE_HEAD".into()),
        )
    }

    #[test]
    fn resolving_chunks_builds_the_result() {
        let mut v = view();
        assert_eq!(v.chunks.len(), 2);
        assert_eq!((v.conflicts(), v.unresolved(), v.selected), (1, 1, 1));
        let (text, n) = v.result();
        assert_eq!(n, 1);
        assert!(text.starts_with("A\nb\nc\n<<<<<<< HEAD\nOURS\n=======\nTHEIRS\n>>>>>>> MERGE_HEAD\n"), "{text}");
        v.resolve(1, Some(Resolution::OursThenTheirs)).unwrap();
        assert_eq!(v.result(), ("A\nb\nc\nOURS\nTHEIRS\n".to_string(), 0));
        assert!(v.resolve(9, None).is_err());
        v.resolve_all(Resolution::Theirs);
        // The one-sided change stays applied; the conflict takes theirs.
        assert_eq!(v.result().0, "A\nb\nc\nTHEIRS\n");
        v.resolve(0, Some(Resolution::Base)).unwrap();
        v.resolve(1, None).unwrap();
        assert_eq!(v.unresolved(), 1);
        v.apply_non_conflicting();
        assert_eq!(v.resolutions[0], Some(Resolution::Ours));
        let j = v.to_json();
        assert_eq!(j["chunks"][1]["kind"], "Conflict");
        assert_eq!(j["chunks"][1]["oursText"], "OURS\n");
        assert_eq!(j["unresolved"], 1);
        assert!(parse_resolution("left").unwrap().is_some());
        assert!(parse_resolution("none").unwrap().is_none());
        assert!(parse_resolution("sideways").is_err());
    }

    #[test]
    fn apply_refuses_unresolved_conflicts() {
        let mut s = Session::new();
        assert!(apply(&mut s, false).is_err(), "no merge open");
        s.vcs.merge = Some(view());
        let err = apply(&mut s, false).unwrap_err();
        assert!(err.to_string().contains("1 conflict is left"), "{err}");
        assert!(s.vcs.merge.is_some());
    }
}
