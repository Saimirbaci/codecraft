//! The three-way merge view for a conflicted file (Git › Resolve Conflicts › Merge…): our
//! version on the left, theirs on the right, the result in the middle. Non-conflicting changes
//! are applied up front; each conflict is resolved by accepting the left side, the right side,
//! both, or neither. Applying writes the result and marks the file resolved (`git add`).
//!
//! The three versions come from the index stages (`:1:` base, `:2:` ours, `:3:` theirs) and are
//! loaded on a worker thread ([`crate::vcs::open_merge`]).
//!
//! The result is an editable text (the merge editor's centre pane). Each change owns a byte
//! range of it: accepting a side replaces that range, and typing ([`MergeView::set_text`])
//! moves the ranges along and marks the changes it touches as edited (resolved by hand).

use std::ops::Range;
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
    /// The result as it stands (editable).
    pub text: String,
    /// Byte range of each chunk in [`Self::text`].
    pub ranges: Vec<Range<usize>>,
    /// Chunks the user edited by hand in the result (they count as resolved).
    pub edited: Vec<bool>,
}

fn push_both(out: &mut String, first: &str, second: &str) {
    out.push_str(first);
    if !first.is_empty() && !first.ends_with('\n') && !second.is_empty() {
        out.push('\n');
    }
    out.push_str(second);
}

/// The 0-based line of a byte offset.
pub fn line_of(text: &str, byte: usize) -> usize {
    let b = byte.min(text.len());
    text.as_bytes().get(..b).map_or(0, |s| s.iter().filter(|c| **c == b'\n').count())
}

impl MergeView {
    pub fn new(path: String, abs: PathBuf, base: String, ours: String, theirs: String, labels: (String, String)) -> Self {
        let chunks = merge3::chunks(&base, &ours, &theirs);
        let resolutions = chunks.iter().map(|c| Resolution::natural(c.kind)).collect();
        let n = chunks.len();
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
            text: String::new(),
            ranges: Vec::with_capacity(n),
            edited: vec![false; n],
        };
        v.rebuild();
        v.selected = v.next_unresolved(0).unwrap_or(0);
        v
    }

    /// One chunk's text in the result for its resolution (`after_newline`: the text before it
    /// ends a line, so conflict markers need no line break first).
    fn render(&self, index: usize, after_newline: bool) -> String {
        let Some((base, ours, theirs)) = self.texts(index) else { return String::new() };
        let mut out = String::new();
        match self.resolutions.get(index).copied().flatten() {
            Some(Resolution::Ours) => out = ours,
            Some(Resolution::Theirs) => out = theirs,
            Some(Resolution::OursThenTheirs) => push_both(&mut out, &ours, &theirs),
            Some(Resolution::TheirsThenOurs) => push_both(&mut out, &theirs, &ours),
            Some(Resolution::Base) => out = base,
            None => {
                if !after_newline {
                    out.push('\n');
                }
                out.push_str(&format!("<<<<<<< {}\n", self.ours_label));
                out.push_str(&ours);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("=======\n");
                out.push_str(&theirs);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&format!(">>>>>>> {}\n", self.theirs_label));
            }
        }
        out
    }

    /// Build the result from scratch (drops hand edits).
    fn rebuild(&mut self) {
        let b = merge3::split_lines(&self.base);
        let mut out = String::with_capacity(self.base.len().max(self.ours.len()));
        let mut ranges = Vec::with_capacity(self.chunks.len());
        let mut pos = 0usize;
        for (i, c) in self.chunks.iter().enumerate() {
            out.push_str(&merge3::slice(&b, &(pos..c.base.start.max(pos))));
            let start = out.len();
            let after_newline = out.is_empty() || out.ends_with('\n');
            out.push_str(&self.render(i, after_newline));
            ranges.push(start..out.len());
            pos = pos.max(c.base.end);
        }
        out.push_str(&merge3::slice(&b, &(pos..b.len())));
        self.text = out;
        self.ranges = ranges;
        self.edited = vec![false; self.chunks.len()];
    }

    /// Replace one chunk's range of the result with its resolution's text.
    fn rerender(&mut self, index: usize) {
        let Some(range) = self.ranges.get(index).cloned() else { return };
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len()).max(start);
        if !self.text.is_char_boundary(start) || !self.text.is_char_boundary(end) {
            // Never expected (ranges move with edits); rebuild rather than slice mid-character.
            self.rebuild();
            return;
        }
        let after_newline = start == 0 || self.text.as_bytes().get(start - 1) == Some(&b'\n');
        let new = self.render(index, after_newline);
        let delta = new.len() as isize - (end - start) as isize;
        self.text.replace_range(start..end, &new);
        if let Some(r) = self.ranges.get_mut(index) {
            *r = start..start + new.len();
        }
        for r in self.ranges.iter_mut().skip(index + 1) {
            r.start = (r.start as isize + delta).max(0) as usize;
            r.end = (r.end as isize + delta).max(r.start as isize) as usize;
        }
        if let Some(e) = self.edited.get_mut(index) {
            *e = false;
        }
    }

    /// The result edited by hand: move the chunk ranges along with the edit and mark the
    /// chunks it touches as edited.
    pub fn set_text(&mut self, new: String) {
        let old = std::mem::take(&mut self.text);
        let (ob, nb) = (old.as_bytes(), new.as_bytes());
        let mut p = ob.iter().zip(nb).take_while(|(a, b)| a == b).count();
        while p > 0 && !(old.is_char_boundary(p) && new.is_char_boundary(p)) {
            p -= 1;
        }
        let max_q = ob.len().min(nb.len()) - p;
        let mut q = ob.iter().rev().zip(nb.iter().rev()).take(max_q).take_while(|(a, b)| a == b).count();
        while q > 0 && !(old.is_char_boundary(ob.len() - q) && new.is_char_boundary(nb.len() - q)) {
            q -= 1;
        }
        let (old_end, new_end) = (ob.len() - q, nb.len() - q);
        if p == old_end && p == new_end {
            self.text = new;
            return;
        }
        let delta = new_end as isize - old_end as isize;
        for (r, e) in self.ranges.iter_mut().zip(self.edited.iter_mut()) {
            let touches =
                if r.start >= r.end { p <= r.start && r.start <= old_end } else { r.start < old_end && p < r.end };
            if r.end <= p && !touches {
                continue;
            }
            if r.start >= old_end && !touches {
                r.start = (r.start as isize + delta).max(0) as usize;
                r.end = (r.end as isize + delta).max(r.start as isize) as usize;
                continue;
            }
            // Overlaps the edit: grow to cover what replaced it.
            let start = r.start.min(p);
            let end = if r.end >= old_end { (r.end as isize + delta).max(start as isize) as usize } else { new_end };
            *r = start..end.max(start);
            *e = true;
        }
        self.text = new;
    }

    /// Conflicting chunks in total.
    pub fn conflicts(&self) -> usize {
        self.chunks.iter().filter(|c| c.kind == ChunkKind::Conflict).count()
    }

    fn is_unresolved(&self, i: usize) -> bool {
        self.resolutions.get(i).is_some_and(Option::is_none) && !self.edited.get(i).copied().unwrap_or(false)
    }

    /// Changes neither accepted nor edited.
    pub fn unresolved(&self) -> usize {
        (0..self.chunks.len()).filter(|i| self.is_unresolved(*i)).count()
    }

    /// The first unresolved chunk at or after `from` (wrapping around).
    pub fn next_unresolved(&self, from: usize) -> Option<usize> {
        let n = self.resolutions.len();
        (0..n).map(|i| (from + i) % n.max(1)).find(|i| self.is_unresolved(*i))
    }

    /// The previous unresolved chunk before `from` (wrapping around).
    pub fn prev_unresolved(&self, from: usize) -> Option<usize> {
        let n = self.resolutions.len();
        (1..=n).map(|k| (from + n - k) % n.max(1)).find(|i| self.is_unresolved(*i))
    }

    /// The merged text (unresolved chunks with conflict markers) and how many are unresolved.
    pub fn result(&self) -> (String, usize) {
        (self.text.clone(), self.unresolved())
    }

    /// The 0-based line range of a chunk in the result.
    pub fn result_lines(&self, index: usize) -> Option<Range<usize>> {
        let r = self.ranges.get(index)?;
        let start = line_of(&self.text, r.start);
        let open_last_line = r.end > r.start && !self.text.as_bytes().get(..r.end).is_some_and(|s| s.ends_with(b"\n"));
        let end = line_of(&self.text, r.end) + usize::from(open_last_line);
        Some(start..end.max(start))
    }

    pub fn resolve(&mut self, index: usize, r: Option<Resolution>) -> Result<(), CommandError> {
        let slot = self
            .resolutions
            .get_mut(index)
            .ok_or_else(|| CommandError::BadParams(format!("there is no change #{index}")))?;
        *slot = r;
        self.rerender(index);
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
        for i in 0..self.chunks.len() {
            self.rerender(i);
        }
    }

    /// Apply every non-conflicting change (the view's starting state).
    pub fn apply_non_conflicting(&mut self) {
        let mut changed = Vec::new();
        for (i, (slot, c)) in self.resolutions.iter_mut().zip(&self.chunks).enumerate() {
            if c.kind != ChunkKind::Conflict {
                *slot = Resolution::natural(c.kind);
                changed.push(i);
            }
        }
        for i in changed {
            self.rerender(i);
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
                    "edited": self.edited.get(i).copied().unwrap_or(false),
                    "resultLines": self.result_lines(i).map(|r| [r.start, r.end]),
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
        assert_eq!(j["chunks"][1]["resultLines"], json!([3, 8]));
        assert!(parse_resolution("left").unwrap().is_some());
        assert!(parse_resolution("none").unwrap().is_none());
        assert!(parse_resolution("sideways").is_err());
    }

    #[test]
    fn the_result_is_editable_and_ranges_follow_edits() {
        let mut v = view();
        assert_eq!(v.ranges.len(), 2);
        assert_eq!(&v.text[v.ranges[0].clone()], "A\n");
        // Type a line before everything: both ranges move, nothing is marked edited.
        v.set_text(format!("// top\n{}", v.text));
        assert_eq!(&v.text[v.ranges[0].clone()], "A\n");
        assert!(v.text[v.ranges[1].clone()].starts_with("<<<<<<< HEAD"));
        assert_eq!(v.edited, vec![false, false]);
        assert_eq!(v.unresolved(), 1);
        // Resolve the conflict by hand: replace the marker block.
        let r = v.ranges[1].clone();
        let mut t = v.text.clone();
        t.replace_range(r, "MINE\n");
        v.set_text(t);
        assert_eq!(v.edited, vec![false, true]);
        assert_eq!(v.unresolved(), 0, "a hand-edited conflict counts as resolved");
        assert_eq!(v.result().0, "// top\nA\nb\nc\nMINE\n");
        assert_eq!(v.result_lines(1), Some(4..5));
        // Accepting a side replaces the edited range; the top line stays.
        v.resolve(1, Some(Resolution::Theirs)).unwrap();
        assert_eq!(v.result().0, "// top\nA\nb\nc\nTHEIRS\n");
        assert_eq!(v.edited, vec![false, false]);
        // An edit inside the first change marks only it.
        let t = v.text.replacen("A\n", "AA\n", 1);
        v.set_text(t);
        assert_eq!(v.edited, vec![true, false]);
        assert_eq!(&v.text[v.ranges[1].clone()], "THEIRS\n");
        // Resetting re-creates the markers in place.
        v.resolve(1, None).unwrap();
        assert!(v.result().0.ends_with(">>>>>>> MERGE_HEAD\n"));
        assert_eq!(v.prev_unresolved(0), Some(1));
        // Non-ASCII edits never split characters; unchanged text is a no-op.
        v.set_text(v.text.replacen("b\n", "ß€\n", 1));
        v.set_text(v.text.clone());
        assert!(v.text.contains("ß€"));
        v.set_text(String::new());
        assert!(v.ranges.iter().all(|r| r.end <= v.text.len()));
        assert_eq!(line_of("a\nb\n", 99), 2);
    }

    #[test]
    fn a_conflict_at_the_end_without_newline() {
        let mut v = MergeView::new(
            "f".into(),
            PathBuf::from("/nonexistent/f"),
            "x\ny".into(),
            "x\nours".into(),
            "x\ntheirs".into(),
            ("HEAD".into(), "B".into()),
        );
        assert_eq!(v.result().0, "x\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> B\n");
        v.resolve(0, Some(Resolution::OursThenTheirs)).unwrap();
        assert_eq!(v.result().0, "x\nours\ntheirs");
        assert_eq!(v.result_lines(0), Some(1..3));
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
