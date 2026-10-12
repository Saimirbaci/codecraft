//! Git worktrees for review mode.
//!
//! A review job runs in its own worktree on branch `codecraft/agent-<id>`, placed under
//! `~/.codecraft/worktrees/<project-hash>/<id>` so the project directory is never touched.
//!
//! The user's uncommitted work comes along: `git stash create` records tracked changes (staged
//! and unstaged) as a commit *without* touching the user's working tree, index or stash list,
//! and `git stash apply <commit>` replays it in the worktree as a proper merge on the same base
//! (binary files included). Untracked, non-ignored files are copied. That state is then
//! committed on the job's branch as the **baseline**, so the review shows only what the agent
//! changed, not what the user had in progress. (Alternatives: `git diff | git apply` loses
//! binary changes and needs care with the index; copying the whole tree is slow and drags in
//! ignored build output.)
//!
//! When the job ends, `git add -A` + `git diff --cached <baseline>` in the worktree lists every
//! file the agent added, modified or deleted, including untracked files and anything it
//! committed itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::event::ChangeKind;
use crate::{AgentError, Result};

/// Larger files are listed but not diffed as text.
pub const MAX_REVIEW_FILE_BYTES: u64 = 4 << 20;
/// At most this many changed files are reported.
pub const MAX_REVIEW_FILES: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// The user's repository root.
    pub repo: PathBuf,
    /// The worktree directory.
    pub path: PathBuf,
    pub branch: String,
    /// The commit holding the project state the agent started from.
    pub baseline: String,
    /// The project directory relative to the repository root (empty when they are the same).
    pub prefix: PathBuf,
}

impl Worktree {
    /// Where the agent runs: the project's directory inside the worktree.
    pub fn cwd(&self) -> PathBuf {
        self.path.join(&self.prefix)
    }
}

/// One file the agent changed, relative to the repository root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub rel: PathBuf,
    pub kind: ChangeKind,
    /// Content at the baseline (`None` for added files).
    pub original: Option<Vec<u8>>,
    /// Content in the worktree (`None` for deleted files).
    pub proposed: Option<Vec<u8>>,
    /// Too large to load; `original`/`proposed` are `None` and accepting copies the file.
    pub too_large: bool,
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).stdin(Stdio::null());
    // Never prompt, never page, never run hooks or sign.
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_PAGER", "cat");
    c.args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"]);
    c.env("PATH", crate::backend::enriched_path());
    c
}

/// Run git and return stdout; a non-zero exit is an error with git's stderr.
fn run(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.output().map_err(|e| AgentError::Git(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        return Err(AgentError::Git(format!("git {} failed: {err}", args.join(" "))));
    }
    Ok(out.stdout)
}

fn run_str(cmd: &mut Command) -> Result<String> {
    Ok(String::from_utf8_lossy(&run(cmd)?).trim().to_string())
}

/// Stable short hash of a path (FNV-1a), for per-project directories.
pub fn project_hash(path: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(32).collect();
    format!("{name}-{h:016x}")
}

/// The default base directory: `~/.codecraft/worktrees`.
pub fn default_base_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codecraft").join("worktrees"))
}

/// Whether `dir` is inside a git work tree.
pub fn is_repo(dir: &Path) -> bool {
    run_str(git(dir).args(["rev-parse", "--is-inside-work-tree"])).is_ok_and(|s| s == "true")
}

/// Create a worktree for job `id` from the project at `project`, bringing the user's
/// uncommitted changes and untracked files along.
pub fn create_worktree(project: &Path, base_dir: &Path, id: &str) -> Result<Worktree> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(AgentError::Git(format!("invalid worktree id {id:?}")));
    }
    let repo = PathBuf::from(
        run_str(git(project).args(["rev-parse", "--show-toplevel"]))
            .map_err(|_| AgentError::Git(format!("{} is not in a git repository", project.display())))?,
    );
    let prefix = PathBuf::from(run_str(git(project).args(["rev-parse", "--show-prefix"]))?);
    let head = run_str(git(&repo).args(["rev-parse", "--verify", "HEAD"]))
        .map_err(|_| AgentError::Git("review mode needs a repository with at least one commit".into()))?;
    let stash = run_str(git(&repo).args(["stash", "create"]))?;
    let untracked = run(git(&repo).args(["ls-files", "--others", "--exclude-standard", "-z"]))?;

    let path = base_dir.join(project_hash(&repo)).join(id);
    if path.exists() {
        return Err(AgentError::Git(format!("{} already exists", path.display())));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let branch = format!("codecraft/agent-{id}");
    let _ = run(git(&repo).args(["worktree", "prune"]));
    run(git(&repo).arg("worktree").arg("add").arg("-B").arg(&branch).arg(&path).arg(&head))?;
    let mut wt = Worktree { repo: repo.clone(), path: path.clone(), branch, baseline: head, prefix };

    let prepared = (|| -> Result<()> {
        let mut changed = false;
        if !stash.is_empty() {
            run(git(&path).args(["stash", "apply", &stash]))?;
            changed = true;
        }
        for rel in untracked.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let rel = PathBuf::from(String::from_utf8_lossy(rel).into_owned());
            if !is_safe_rel(&rel) {
                continue;
            }
            let (from, to) = (repo.join(&rel), path.join(&rel));
            let Ok(meta) = std::fs::symlink_metadata(&from) else { continue };
            if !meta.is_file() {
                continue;
            }
            if let Some(dir) = to.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if let Err(e) = std::fs::copy(&from, &to) {
                log::warn!("worktree: cannot copy {}: {e}", rel.display());
                continue;
            }
            changed = true;
        }
        if changed {
            run(git(&path).args(["add", "-A"]))?;
            run(git(&path).args([
                "-c",
                "user.name=CodeCraft",
                "-c",
                "user.email=codecraft@localhost",
                "commit",
                "--no-verify",
                "--allow-empty",
                "-q",
                "-m",
                "codecraft: baseline (uncommitted changes)",
            ]))?;
            wt.baseline = run_str(git(&path).args(["rev-parse", "HEAD"]))?;
        }
        Ok(())
    })();
    if let Err(e) = prepared {
        let _ = remove_worktree(&wt, base_dir);
        return Err(e);
    }
    Ok(wt)
}

/// A relative path without `..` or root components.
fn is_safe_rel(p: &Path) -> bool {
    !p.as_os_str().is_empty() && p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Every file the agent changed relative to the baseline.
pub fn changed_files(wt: &Worktree) -> Result<Vec<ChangedFile>> {
    run(git(&wt.path).args(["add", "-A"]))?;
    let out = run(git(&wt.path).args(["diff", "--cached", "--name-status", "--no-renames", "-z", &wt.baseline]))?;
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        if files.len() >= MAX_REVIEW_FILES {
            break;
        }
        let rel = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        if !is_safe_rel(&rel) {
            continue;
        }
        let kind = match status.first() {
            Some(b'A') => ChangeKind::Added,
            Some(b'D') => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        let disk = wt.path.join(&rel);
        let too_large = std::fs::metadata(&disk).is_ok_and(|m| m.len() > MAX_REVIEW_FILE_BYTES);
        let original = if kind == ChangeKind::Added || too_large {
            None
        } else {
            let spec = format!("{}:{}", wt.baseline, rel.to_string_lossy());
            run(git(&wt.path).args(["show", &spec])).ok()
        };
        let proposed = if kind == ChangeKind::Deleted || too_large { None } else { std::fs::read(&disk).ok() };
        files.push(ChangedFile { rel, kind, original, proposed, too_large });
    }
    Ok(files)
}

/// Remove the worktree directory and its branch. Only directories under `base_dir` are ever
/// deleted.
pub fn remove_worktree(wt: &Worktree, base_dir: &Path) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(e) = run(git(&wt.repo).arg("worktree").arg("remove").arg("--force").arg(&wt.path)) {
        errors.push(e.to_string());
    }
    if wt.path.exists()
        && wt.path.starts_with(base_dir)
        && wt.path != base_dir
        && let Err(e) = std::fs::remove_dir_all(&wt.path)
    {
        errors.push(format!("cannot remove {}: {e}", wt.path.display()));
    }
    let _ = run(git(&wt.repo).args(["worktree", "prune"]));
    if let Err(e) = run(git(&wt.repo).args(["branch", "-D", &wt.branch])) {
        errors.push(e.to_string());
    }
    if let Some(parent) = wt.path.parent()
        && parent.starts_with(base_dir)
        && parent != base_dir
    {
        // Remove the per-project directory once it is empty.
        let _ = std::fs::remove_dir(parent);
    }
    if wt.path.exists() || (!errors.is_empty() && branch_exists(&wt.repo, &wt.branch)) {
        return Err(AgentError::Git(errors.join("; ")));
    }
    Ok(())
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    run(git(repo).args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])).is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn have_git() -> bool {
        Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = git(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    }

    pub(crate) fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        sh(d, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(d.join("keep.txt"), "keep\n").unwrap();
        std::fs::write(d.join(".gitignore"), "target/\n").unwrap();
        sh(d, &["add", "-A"]);
        sh(d, &["commit", "-q", "-m", "init"]);
        tmp
    }

    #[test]
    fn worktree_carries_uncommitted_work_and_reports_only_agent_changes() {
        if !have_git() {
            eprintln!("skipping: git not found");
            return;
        }
        let repo = repo();
        let r = repo.path();
        // The user's uncommitted work: a modification, a staged new file, an untracked file and
        // an ignored one.
        std::fs::write(r.join("src/a.rs"), "fn a() { /* wip */ }\n").unwrap();
        std::fs::write(r.join("staged.txt"), "staged\n").unwrap();
        sh(r, &["add", "staged.txt"]);
        std::fs::write(r.join("notes.md"), "untracked\n").unwrap();
        std::fs::create_dir_all(r.join("target")).unwrap();
        std::fs::write(r.join("target/big.bin"), "ignored").unwrap();

        let base = tempfile::tempdir().unwrap();
        let wt = create_worktree(r, base.path(), "7-1").unwrap();
        assert!(wt.path.starts_with(base.path()));
        assert_eq!(wt.branch, "codecraft/agent-7-1");
        assert_eq!(std::fs::read_to_string(wt.path.join("src/a.rs")).unwrap(), "fn a() { /* wip */ }\n");
        assert!(wt.path.join("staged.txt").exists());
        assert!(wt.path.join("notes.md").exists());
        assert!(!wt.path.join("target/big.bin").exists());
        // The user's tree is untouched.
        assert_eq!(std::fs::read_to_string(r.join("src/a.rs")).unwrap(), "fn a() { /* wip */ }\n");
        assert!(changed_files(&wt).unwrap().is_empty(), "the baseline has the user's work");

        // The agent edits, adds and deletes.
        std::fs::write(wt.path.join("src/a.rs"), "fn a() { done() }\n").unwrap();
        std::fs::write(wt.path.join("src/new.rs"), "new\n").unwrap();
        std::fs::remove_file(wt.path.join("keep.txt")).unwrap();
        let mut files = changed_files(&wt).unwrap();
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        let summary: Vec<(String, ChangeKind)> =
            files.iter().map(|f| (f.rel.to_string_lossy().into_owned(), f.kind)).collect();
        assert_eq!(
            summary,
            vec![
                ("keep.txt".into(), ChangeKind::Deleted),
                ("src/a.rs".into(), ChangeKind::Modified),
                ("src/new.rs".into(), ChangeKind::Added),
            ]
        );
        let a = files.iter().find(|f| f.rel == Path::new("src/a.rs")).unwrap();
        assert_eq!(a.original.as_deref(), Some(&b"fn a() { /* wip */ }\n"[..]));
        assert_eq!(a.proposed.as_deref(), Some(&b"fn a() { done() }\n"[..]));

        remove_worktree(&wt, base.path()).unwrap();
        assert!(!wt.path.exists());
        assert!(!branch_exists(r, &wt.branch));
    }

    #[test]
    fn subdirectory_projects_and_errors() {
        if !have_git() {
            return;
        }
        let repo = repo();
        let base = tempfile::tempdir().unwrap();
        let wt = create_worktree(&repo.path().join("src"), base.path(), "sub").unwrap();
        assert_eq!(wt.prefix, PathBuf::from("src/"));
        assert!(wt.cwd().join("a.rs").exists());
        remove_worktree(&wt, base.path()).unwrap();

        let plain = tempfile::tempdir().unwrap();
        assert!(create_worktree(plain.path(), base.path(), "x").is_err());
        assert!(create_worktree(repo.path(), base.path(), "../evil").is_err());
        assert!(!is_repo(plain.path()));
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(project_hash(Path::new("/a/proj")), project_hash(Path::new("/a/proj")));
        assert_ne!(project_hash(Path::new("/a/proj")), project_hash(Path::new("/b/proj")));
        assert!(project_hash(Path::new("/a/my proj!")).starts_with("myproj-"));
    }
}
