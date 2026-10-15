//! Per-hunk staging: put one changed region of a file into the index (or take it out) without
//! touching the rest of the file, like `git add -p` / `git reset -p` for a single hunk.
//!
//! The gutter markers compare the work tree with `HEAD`, so a hunk is known by its line range in
//! the work tree (`start..end`) and in `HEAD` (`old_start..old_end`). Staging diffs the index
//! against the work tree and applies the changes overlapping the work-tree range; unstaging diffs
//! `HEAD` against the index and reverts the changes overlapping the `HEAD` range.

use std::ops::Range;

use crate::lines::{LineChange, line_changes, revert};
use crate::log::text_at;
use crate::{Git, GitError, Result, pathspec};

/// Whether two half-open line ranges touch: overlapping, or an empty range (an insertion or
/// deletion point) at or inside the other.
pub fn ranges_touch(a: &Range<usize>, b: &Range<usize>) -> bool {
    match (a.is_empty(), b.is_empty()) {
        (false, false) => a.start < b.end && b.start < a.end,
        (true, false) => b.start <= a.start && a.start <= b.end,
        (false, true) => a.start <= b.start && b.start <= a.end,
        (true, true) => a.start == b.start,
    }
}

/// `base` with one change applied forward: base lines `old_start..old_end` replaced by
/// `current` lines `start..end`.
pub fn apply_forward(base: &str, current: &str, change: &LineChange) -> String {
    let inverse = LineChange {
        kind: change.kind,
        start: change.old_start,
        end: change.old_end,
        old_start: change.start,
        old_end: change.end,
    };
    revert(current, base, &inverse)
}

/// The index content after staging the changes of `current` (the work tree) that touch
/// `range` (work-tree lines). `None` when nothing there differs from the index.
pub fn stage_text(index: &str, current: &str, range: &Range<usize>) -> Option<String> {
    let picked: Vec<LineChange> =
        line_changes(index, current).into_iter().filter(|c| ranges_touch(&(c.start..c.end), range)).collect();
    if picked.is_empty() {
        return None;
    }
    let mut text = index.to_string();
    // From the last change backwards so earlier base line numbers stay valid.
    for c in picked.iter().rev() {
        text = apply_forward(&text, current, c);
    }
    Some(text)
}

/// The index content after unstaging the staged changes that touch `head_range` (lines of the
/// `HEAD` version). `None` when nothing staged is there.
pub fn unstage_text(head: &str, index: &str, head_range: &Range<usize>) -> Option<String> {
    let picked: Vec<LineChange> = line_changes(head, index)
        .into_iter()
        .filter(|c| ranges_touch(&(c.old_start..c.old_end), head_range))
        .collect();
    if picked.is_empty() {
        return None;
    }
    let mut text = index.to_string();
    for c in picked.iter().rev() {
        text = revert(head, &text, c);
    }
    Some(text)
}

/// The file's mode in the index (`100644` when it has none yet).
/// Whether a change (work tree vs `HEAD`) is in the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StageState {
    /// None of its lines are staged.
    Unstaged,
    /// All of it is staged (the work tree matches the index there).
    Staged,
    /// Some lines are staged, others not.
    Partly,
}

impl StageState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unstaged => "Not staged",
            Self::Staged => "Staged",
            Self::Partly => "Partly staged",
        }
    }

    pub fn can_stage(self) -> bool {
        self != Self::Staged
    }

    pub fn can_unstage(self) -> bool {
        self != Self::Unstaged
    }
}

/// Compare the index with `HEAD` and the work tree around one change (as the gutter markers
/// compute it: `current` against `head`). `None` if neither side differs there (stale marker).
pub fn stage_state(head: &str, index: &str, current: &str, change: &LineChange) -> Option<StageState> {
    let unstaged = stage_text(index, current, &(change.start..change.end)).is_some();
    let staged = unstage_text(head, index, &(change.old_start..change.old_end)).is_some();
    match (staged, unstaged) {
        (false, true) => Some(StageState::Unstaged),
        (true, false) => Some(StageState::Staged),
        (true, true) => Some(StageState::Partly),
        (false, false) => None,
    }
}

fn index_mode(git: &Git, path: &str) -> Result<String> {
    let out = git.read_text(["ls-files", "-s", "--", &pathspec(path)])?;
    Ok(out.split_whitespace().next().filter(|m| m.len() == 6).unwrap_or("100644").to_string())
}

/// Replace a file's index entry with `text` (work-tree form: git's clean filters apply, as for
/// `git add`). The work tree is not touched.
pub fn write_index(git: &Git, path: &str, text: &str) -> Result<()> {
    let mode = index_mode(git, path)?;
    let path_arg = format!("--path={path}");
    let oid = git.cmd(["hash-object", "-w", "--stdin", &path_arg]).stdin(text.as_bytes().to_vec()).text()?;
    let oid = oid.trim();
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(GitError::Parse(format!("hash-object printed {oid:?}")));
    }
    git.run(["update-index", "--add", "--cacheinfo", &format!("{mode},{oid},{path}")])?;
    Ok(())
}

/// Stage the part of `current` (the file's work-tree text, possibly unsaved) that touches the
/// work-tree lines `range`. Returns false when that region already matches the index.
pub fn stage_hunk(git: &Git, path: &str, current: &str, range: Range<usize>) -> Result<bool> {
    let index = text_at(git, "", path)?
        .ok_or_else(|| GitError::Invalid(format!("{path} is not in the index: add the file first")))?;
    match stage_text(&index, current, &range) {
        Some(text) => write_index(git, path, &text).map(|()| true),
        None => Ok(false),
    }
}

/// Unstage the staged changes touching `HEAD` lines `head_range`. Returns false when nothing
/// there is staged.
pub fn unstage_hunk(git: &Git, path: &str, head_range: Range<usize>) -> Result<bool> {
    let index = text_at(git, "", path)?.ok_or_else(|| GitError::Invalid(format!("{path} is not in the index")))?;
    let head = text_at(git, "HEAD", path)?.unwrap_or_default();
    match unstage_text(&head, &index, &head_range) {
        Some(text) => write_index(git, path, &text).map(|()| true),
        None => Ok(false),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available, write};

    #[test]
    fn touching_ranges() {
        assert!(ranges_touch(&(1..3), &(2..4)));
        assert!(!ranges_touch(&(1..3), &(3..4)));
        assert!(ranges_touch(&(3..3), &(1..3)), "a deletion point at the end");
        assert!(ranges_touch(&(1..1), &(1..3)));
        assert!(!ranges_touch(&(5..5), &(1..3)));
        assert!(ranges_touch(&(2..2), &(2..2)));
        assert!(!ranges_touch(&(2..2), &(3..3)));
    }

    #[test]
    fn stage_and_unstage_text() {
        let index = "a\nb\nc\nd\ne\n";
        let current = "a\nB\nc\nd\nE\nf\n";
        // Stage only the first change (line 1).
        let staged = stage_text(index, current, &(1..2)).unwrap();
        assert_eq!(staged, "a\nB\nc\nd\ne\n");
        // The second region (lines 4..6) next.
        let staged2 = stage_text(&staged, current, &(4..6)).unwrap();
        assert_eq!(staged2, current);
        assert!(stage_text(current, current, &(0..6)).is_none());
        // Unstage the first change by its HEAD lines (1..2).
        let head = index;
        let back = unstage_text(head, &staged2, &(1..2)).unwrap();
        assert_eq!(back, "a\nb\nc\nd\nE\nf\n");
        assert!(unstage_text(head, head, &(0..1)).is_none());
        // Deletions and additions at the end of the file.
        assert_eq!(stage_text("a\nb\n", "a\n", &(1..1)).unwrap(), "a\n");
        assert_eq!(stage_text("a\n", "a\nz\n", &(1..2)).unwrap(), "a\nz\n");
        assert_eq!(apply_forward("x\n", "", &line_changes("x\n", "")[0]), "");
    }

    #[test]
    fn stage_state_compares_head_index_and_work_tree() {
        let head = "a\nb\nc\nd\ne\n";
        let current = "a\nB\nc\nd\nE\nE2\n";
        let changes = line_changes(head, current);
        assert_eq!(changes.len(), 2);
        // Nothing staged.
        assert_eq!(stage_state(head, head, current, &changes[0]), Some(StageState::Unstaged));
        // The first change staged entirely.
        let index = stage_text(head, current, &(1..2)).unwrap();
        assert_eq!(stage_state(head, &index, current, &changes[0]), Some(StageState::Staged));
        assert_eq!(stage_state(head, &index, current, &changes[1]), Some(StageState::Unstaged));
        // Half of the second change staged (E staged, E2 not).
        let partly = "a\nB\nc\nd\nE\n";
        assert_eq!(stage_state(head, partly, current, &changes[1]), Some(StageState::Partly));
        // A stale marker.
        assert_eq!(stage_state(head, head, head, &changes[0]), None);
        assert!(StageState::Partly.can_stage() && StageState::Partly.can_unstage());
        assert!(!StageState::Staged.can_stage() && !StageState::Unstaged.can_unstage());
        assert_eq!(StageState::Partly.label(), "Partly staged");
    }

    #[test]
    fn hunks_go_in_and_out_of_the_index() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n")]);
        let current = "1\nTWO\n3\n4\n5\n6\n7\nEIGHT\nnine\n";
        write(&root, "f.txt", current);
        assert!(stage_hunk(&git, "f.txt", current, 7..9).unwrap());
        let cached = git.read_text(["diff", "--cached", "--", "f.txt"]).unwrap();
        assert!(cached.contains("+EIGHT") && cached.contains("+nine") && !cached.contains("TWO"), "{cached}");
        let unstaged = git.read_text(["diff", "--", "f.txt"]).unwrap();
        assert!(unstaged.contains("+TWO") && !unstaged.contains("EIGHT"), "{unstaged}");
        // Already staged: nothing to do.
        assert!(!stage_hunk(&git, "f.txt", current, 7..9).unwrap());
        // Unstage by HEAD lines (7..8 = "8").
        assert!(unstage_hunk(&git, "f.txt", 7..8).unwrap());
        assert!(git.read_text(["diff", "--cached", "--", "f.txt"]).unwrap().is_empty());
        assert!(!unstage_hunk(&git, "f.txt", 7..8).unwrap());
        // The work tree is untouched throughout.
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), current);
        // Executable mode survives a staged hunk.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            write(&root, "x.sh", "echo 1\n");
            std::fs::set_permissions(root.join("x.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
            git.run(["add", "x.sh"]).unwrap();
            git.run(["commit", "-q", "-m", "x"]).unwrap();
            assert!(stage_hunk(&git, "x.sh", "echo 2\n", 0..1).unwrap());
            assert!(git.read_text(["ls-files", "-s", "x.sh"]).unwrap().starts_with("100755"));
        }
        // Untracked files can't be staged by hunk.
        write(&root, "new.txt", "n\n");
        assert!(matches!(stage_hunk(&git, "new.txt", "n\n", 0..1), Err(GitError::Invalid(_))));
    }
}
