//! Linked work trees (`git worktree`): list, add (on an existing or a new branch) and remove.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Git, GitError, Output, Result, valid_ref_name};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    /// The checked-out commit (`None` for a bare entry).
    pub head: Option<String>,
    /// The checked-out branch (short name), `None` when detached.
    pub branch: Option<String>,
    /// The main work tree (the first entry).
    pub main: bool,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    /// Its directory is gone (`git worktree prune` would remove it).
    pub prunable: bool,
}

/// Parse `git worktree list --porcelain`.
pub fn parse_list(text: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in text.lines() {
        if line.is_empty() {
            out.extend(cur.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            out.extend(cur.take());
            cur = Some(Worktree { path: PathBuf::from(value), ..Worktree::default() });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        match key {
            "HEAD" => w.head = Some(value.to_string()),
            "branch" => w.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).to_string()),
            "bare" => w.bare = true,
            "detached" => w.detached = true,
            "locked" => w.locked = true,
            "prunable" => w.prunable = true,
            _ => {}
        }
    }
    out.extend(cur);
    if let Some(first) = out.first_mut() {
        first.main = true;
    }
    out
}

pub fn list(git: &Git) -> Result<Vec<Worktree>> {
    Ok(parse_list(&git.read_text(["worktree", "list", "--porcelain"])?))
}

/// Add a work tree at `path` checking out `branch`; with `new_branch`, create the branch first
/// (from `base`, default `HEAD`).
pub fn add(git: &Git, path: &Path, branch: &str, new_branch: bool, base: Option<&str>) -> Result<Output> {
    let branch = branch.trim();
    if branch.is_empty() || (new_branch && !valid_ref_name(branch)) || branch.starts_with('-') {
        return Err(GitError::Invalid(format!("'{branch}' is not a valid branch name")));
    }
    if path.as_os_str().is_empty() {
        return Err(GitError::Invalid("choose a directory for the work tree".into()));
    }
    if path.exists() && std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(GitError::Invalid(format!("{} already exists and is not empty", path.display())));
    }
    let target = path.display().to_string();
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if new_branch {
        args.extend(["-b".into(), branch.to_string(), "--".into(), target]);
        if let Some(b) = base.map(str::trim).filter(|b| !b.is_empty()) {
            args.push(b.to_string());
        }
    } else {
        args.extend(["--".into(), target, branch.to_string()]);
    }
    git.run(args)
}

/// Remove a linked work tree (`force`: even with local changes).
pub fn remove(git: &Git, path: &Path, force: bool) -> Result<Output> {
    let target = path.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target);
    git.run(args)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_util::{Sandbox, git_available, write};

    #[test]
    fn porcelain_is_parsed() {
        let text = "worktree /r\nHEAD 1111\nbranch refs/heads/main\n\nworktree /r-wt\nHEAD 2222\ndetached\nlocked reason\n\n\
                    worktree /gone\nHEAD 3333\nbranch refs/heads/feature/x\nprunable gitdir file points to non-existent location\n";
        let wts = parse_list(text);
        assert_eq!(wts.len(), 3);
        assert!(wts[0].main && wts[0].branch.as_deref() == Some("main"));
        assert!(wts[1].detached && wts[1].locked && wts[1].branch.is_none() && !wts[1].main);
        assert_eq!(wts[2].branch.as_deref(), Some("feature/x"));
        assert!(wts[2].prunable);
        assert!(parse_list("").is_empty());
        assert!(parse_list("HEAD x\nbranch y\n").is_empty());
    }

    #[test]
    fn add_list_remove() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("f.txt", "1\n")]);
        let wt = sb.root().join("r-feature");
        add(&git, &wt, "feature", true, None).unwrap();
        assert!(wt.join("f.txt").is_file());
        git.run(["branch", "other"]).unwrap();
        let wt2 = sb.root().join("r-other");
        add(&git, &wt2, "other", false, None).unwrap();
        let wts = list(&git).unwrap();
        assert_eq!(wts.len(), 3, "{wts:?}");
        assert_eq!(std::fs::canonicalize(&wts[0].path).unwrap(), std::fs::canonicalize(&root).unwrap());
        assert!(wts.iter().any(|w| w.branch.as_deref() == Some("feature")));
        // Bad requests are refused before running git.
        assert!(matches!(add(&git, &wt, "x", true, None), Err(GitError::Invalid(_))));
        assert!(matches!(add(&git, &sb.root().join("n"), "a b", true, None), Err(GitError::Invalid(_))));
        // A dirty work tree needs force.
        write(&wt, "f.txt", "dirty\n");
        assert!(remove(&git, &wt, false).is_err());
        remove(&git, &wt, true).unwrap();
        remove(&git, &wt2, false).unwrap();
        assert_eq!(list(&git).unwrap().len(), 1);
    }
}
