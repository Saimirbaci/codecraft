//! Operations that change the repository: commit, push/pull/fetch, branches and tags, stash,
//! rollback, reset, patches, remotes, clone and conflict resolution.
//!
//! Each returns git's [`Output`] (hook output, merge messages) or an error carrying git's
//! message. Commands are echoed to the [`Git`]'s console sink when it has one.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::status::ConflictKind;
use crate::{Git, GitError, Output, Result, pathspec, valid_ref_name};

/// More paths than this go through `--pathspec-from-file` instead of the command line.
const MAX_ARG_PATHS: usize = 200;

/// `args -- paths`, or `args --pathspec-from-file=- --pathspec-file-nul` with the paths on stdin.
fn with_paths(git: &Git, mut args: Vec<String>, paths: &[String]) -> Result<Output> {
    if paths.len() > MAX_ARG_PATHS {
        args.push("--pathspec-from-file=-".into());
        args.push("--pathspec-file-nul".into());
        let mut input = Vec::new();
        for p in paths {
            input.extend_from_slice(pathspec(p).as_bytes());
            input.push(0);
        }
        git.cmd(args).stdin(input).run()
    } else {
        args.push("--".into());
        args.extend(paths.iter().map(|p| pathspec(p)));
        git.run(args)
    }
}

// ---------------------------------------------------------------- commit

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub message: String,
    /// Commit only these repository-relative paths (`git commit --only`); `None` commits the
    /// index as it is. Untracked paths in `add` are added first.
    pub paths: Option<Vec<String>>,
    /// Untracked files to add before committing.
    pub add: Vec<String>,
    pub amend: bool,
    /// `Name <email>`
    pub author: Option<String>,
    pub sign_off: bool,
}

/// Commit; hooks run as usual and their output is in the result. Returns the output and the new
/// `HEAD` commit.
pub fn commit(git: &Git, req: &CommitRequest) -> Result<(Output, String)> {
    if req.message.trim().is_empty() && !req.amend {
        return Err(GitError::Invalid("the commit message is empty".into()));
    }
    if !req.add.is_empty() {
        with_paths(git, vec!["add".into()], &req.add)?;
    }
    let mut args: Vec<String> = vec!["commit".into(), "--cleanup=strip".into()];
    if req.message.trim().is_empty() {
        args.push("--no-edit".into());
    } else {
        args.push("-F".into());
        args.push("-".into());
    }
    if req.amend {
        args.push("--amend".into());
    }
    if let Some(a) = req.author.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        args.push(format!("--author={a}"));
    }
    if req.sign_off {
        args.push("--signoff".into());
    }
    let out = match &req.paths {
        Some(paths) if !paths.is_empty() => {
            if paths.len() > MAX_ARG_PATHS {
                // `--pathspec-from-file` reads stdin, which the message needs: use a temp file.
                let file = std::env::temp_dir().join(format!("codecraft-commit-{}.txt", std::process::id()));
                std::fs::write(&file, &req.message).map_err(|e| GitError::Io(e.to_string()))?;
                if let Some(i) = args.iter().position(|a| a == "-")
                    && let Some(slot) = args.get_mut(i)
                {
                    *slot = file.display().to_string();
                }
                args.push("--only".into());
                let r = with_paths(git, args, paths);
                let _ = std::fs::remove_file(&file);
                r?
            } else {
                args.push("--only".into());
                args.push("--".into());
                args.extend(paths.iter().map(|p| pathspec(p)));
                git.cmd(args).stdin(req.message.as_bytes().to_vec()).run()?
            }
        }
        _ => git.cmd(args).stdin(req.message.as_bytes().to_vec()).run()?,
    };
    let head = git.read_text(["rev-parse", "HEAD"])?.trim().to_string();
    Ok((out, head))
}

// ---------------------------------------------------------------- remotes: push, pull, fetch

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    pub remote: String,
    /// The local branch to push.
    pub branch: String,
    /// The branch on the remote (defaults to `branch`).
    pub remote_branch: Option<String>,
    pub set_upstream: bool,
    pub force_with_lease: bool,
    pub tags: bool,
}

pub fn push(git: &Git, req: &PushRequest) -> Result<Output> {
    if req.remote.is_empty() || req.branch.is_empty() {
        return Err(GitError::Invalid("choose a remote and a branch to push".into()));
    }
    let target = req.remote_branch.as_deref().filter(|b| !b.is_empty()).unwrap_or(&req.branch);
    let mut args: Vec<String> = vec!["push".into(), "--progress".into()];
    if req.set_upstream {
        args.push("--set-upstream".into());
    }
    if req.force_with_lease {
        args.push("--force-with-lease".into());
    }
    if req.tags {
        args.push("--follow-tags".into());
    }
    args.push(req.remote.clone());
    args.push(format!("refs/heads/{}:refs/heads/{target}", req.branch));
    git.cmd(args).network().run()
}

/// Commits on `branch` that the remote branch doesn't have (what a push would send).
pub fn outgoing(git: &Git, branch: &str, remote_branch: Option<&str>) -> Result<Vec<crate::Commit>> {
    let mut args: Vec<String> = vec!["log".into(), "--max-count=500".into()];
    args.push("--format=%x1e%H%x00%P%x00%an%x00%ae%x00%at%x00%D%x00%s".into());
    args.push("--decorate=full".into());
    args.push(format!("refs/heads/{branch}"));
    match remote_branch {
        Some(rb) if git.read(["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{rb}")]).is_ok() => {
            args.push(format!("^refs/remotes/{rb}"));
        }
        // A branch new on the remote: everything not on any remote-tracking branch.
        _ => {
            args.push("--not".into());
            args.push("--remotes".into());
        }
    }
    args.push("--".into());
    let out = git.read(args)?;
    crate::log::log_records(&out)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub remote: Option<String>,
    pub branch: Option<String>,
    /// `Some(true)` rebase, `Some(false)` merge, `None` = the user's `pull.rebase` (merge if unset).
    pub rebase: Option<bool>,
    /// Stash local changes around the update (Update Project does).
    pub autostash: bool,
}

pub fn pull(git: &Git, req: &PullRequest) -> Result<Output> {
    let mut args: Vec<String> = vec!["pull".into(), "--progress".into(), "--no-edit".into()];
    let rebase = match req.rebase {
        Some(r) => Some(r),
        None => {
            let configured = git.cmd(["config", "--get", "pull.rebase"]).read_only().allow_exit(1).run()?;
            // Unset: merge, git's long-standing default (newer git refuses to guess).
            configured.text().trim().is_empty().then_some(false)
        }
    };
    match rebase {
        Some(true) => args.push("--rebase".into()),
        Some(false) => args.push("--no-rebase".into()),
        None => {}
    }
    if req.autostash {
        args.push("--autostash".into());
    }
    if let Some(r) = &req.remote {
        args.push(r.clone());
        if let Some(b) = &req.branch {
            args.push(b.clone());
        }
    }
    git.cmd(args).network().run()
}

/// Fetch one remote, or all of them.
pub fn fetch(git: &Git, remote: Option<&str>) -> Result<Output> {
    let mut args = vec!["fetch", "--progress", "--prune"];
    match remote {
        Some(r) => args.push(r),
        None => args.push("--all"),
    }
    git.cmd(args).network().run()
}

/// Clone `url` into `target` (which must not exist or be empty).
pub fn clone(git: &Git, url: &str, target: &Path) -> Result<Output> {
    if url.trim().is_empty() {
        return Err(GitError::Invalid("enter a repository URL".into()));
    }
    let parent = target.parent().ok_or_else(|| GitError::Invalid("choose a directory".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| GitError::Io(format!("{}: {e}", parent.display())))?;
    let target_s = target.display().to_string();
    git.in_dir(parent).cmd(["clone", "--progress", "--", url.trim(), &target_s]).network().run()
}

// ---------------------------------------------------------------- branches and tags

/// Check out a local branch, or a remote one as a new tracking branch (`origin/x` → `x`).
pub fn checkout(git: &Git, name: &str, remote: bool) -> Result<Output> {
    if remote {
        let local = name.split_once('/').map(|(_, b)| b).unwrap_or(name);
        let exists = git.read(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{local}")]).is_ok();
        if exists {
            return git.run(["checkout", local, "--"]);
        }
        return git.run(["checkout", "--track", name, "--"]);
    }
    git.run(["checkout", name, "--"])
}

/// Create `name` at `start` (default `HEAD`), optionally checking it out.
pub fn create_branch(git: &Git, name: &str, start: Option<&str>, checkout: bool) -> Result<Output> {
    if !valid_ref_name(name) {
        return Err(GitError::Invalid(format!("'{name}' is not a valid branch name")));
    }
    let mut args = if checkout { vec!["checkout", "-b", name] } else { vec!["branch", "--", name] };
    if let Some(s) = start.filter(|s| !s.is_empty()) {
        if checkout {
            args.push(s);
        } else {
            args = vec!["branch", name, s];
        }
    }
    if checkout {
        args.push("--");
    }
    git.run(args)
}

pub fn rename_branch(git: &Git, old: &str, new: &str) -> Result<Output> {
    if !valid_ref_name(new) {
        return Err(GitError::Invalid(format!("'{new}' is not a valid branch name")));
    }
    git.run(["branch", "-m", old, new])
}

/// Delete a local branch (`force` for unmerged ones) or a branch on its remote.
pub fn delete_branch(git: &Git, name: &str, remote: bool, force: bool) -> Result<Output> {
    if remote {
        let (r, b) = name.split_once('/').ok_or_else(|| GitError::Invalid(format!("not a remote branch: {name}")))?;
        return git.cmd(["push", "--progress", r, "--delete", b]).network().run();
    }
    git.run(["branch", if force { "-D" } else { "-d" }, "--", name])
}

/// Merge `branch` into the current branch (no editor: the default message).
pub fn merge(git: &Git, branch: &str) -> Result<Output> {
    git.run(["merge", "--no-edit", branch, "--"])
}

/// Rebase the current branch onto `onto`.
pub fn rebase(git: &Git, onto: &str) -> Result<Output> {
    git.run(["rebase", onto])
}

/// A lightweight tag, or an annotated one with a message.
pub fn create_tag(git: &Git, name: &str, rev: Option<&str>, message: Option<&str>) -> Result<Output> {
    if !valid_ref_name(name) {
        return Err(GitError::Invalid(format!("'{name}' is not a valid tag name")));
    }
    let mut args: Vec<String> = vec!["tag".into()];
    if let Some(m) = message.filter(|m| !m.trim().is_empty()) {
        args.push("-a".into());
        args.push("-m".into());
        args.push(m.to_string());
    }
    args.push(name.to_string());
    if let Some(r) = rev.filter(|r| !r.is_empty()) {
        args.push(r.to_string());
    }
    git.run(args)
}

pub fn delete_tag(git: &Git, name: &str) -> Result<Output> {
    git.run(["tag", "-d", name])
}

// ---------------------------------------------------------------- stash

pub fn stash_push(git: &Git, message: Option<&str>, include_untracked: bool, paths: &[String]) -> Result<Output> {
    let mut args: Vec<String> = vec!["stash".into(), "push".into()];
    if include_untracked {
        args.push("--include-untracked".into());
    }
    if let Some(m) = message.filter(|m| !m.trim().is_empty()) {
        args.push("-m".into());
        args.push(m.to_string());
    }
    if paths.is_empty() { git.run(args) } else { with_paths(git, args, paths) }
}

/// Apply (or pop) `stash@{index}`; `reinstate_index` also restores what was staged.
pub fn stash_apply(git: &Git, index: usize, pop: bool, reinstate_index: bool) -> Result<Output> {
    let r = format!("stash@{{{index}}}");
    let mut args = vec!["stash", if pop { "pop" } else { "apply" }];
    if reinstate_index {
        args.push("--index");
    }
    args.push(&r);
    git.run(args)
}

pub fn stash_drop(git: &Git, index: usize) -> Result<Output> {
    git.run(["stash", "drop", &format!("stash@{{{index}}}")])
}

// ---------------------------------------------------------------- rollback, reset

/// Revert files to `HEAD`: `tracked` paths (in `HEAD`) are restored in the index and the work
/// tree; `added` paths (new in the index) are unstaged and, with `delete_added`, deleted.
pub fn rollback(git: &Git, root: &Path, tracked: &[String], added: &[String], delete_added: bool) -> Result<Output> {
    let mut out = Output::default();
    if !tracked.is_empty() {
        out = with_paths(
            git,
            vec!["restore".into(), "--source=HEAD".into(), "--staged".into(), "--worktree".into()],
            tracked,
        )?;
    }
    if !added.is_empty() {
        out = with_paths(git, vec!["rm".into(), "--cached".into(), "-f".into(), "-q".into(), "-r".into()], added)?;
        if delete_added {
            for p in added {
                let abs = root.join(p);
                if abs.is_file() {
                    std::fs::remove_file(&abs).map_err(|e| GitError::Io(format!("{}: {e}", abs.display())))?;
                }
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
    Keep,
}

impl ResetMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "soft" => Some(Self::Soft),
            "mixed" => Some(Self::Mixed),
            "hard" => Some(Self::Hard),
            "keep" => Some(Self::Keep),
            _ => None,
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
            Self::Keep => "--keep",
        }
    }
}

pub fn reset(git: &Git, rev: &str, mode: ResetMode) -> Result<Output> {
    if rev.trim().is_empty() || rev.starts_with('-') {
        return Err(GitError::Invalid("choose a commit to reset to".into()));
    }
    git.run(["reset", mode.flag(), rev.trim(), "--"])
}

// ---------------------------------------------------------------- patches

/// A patch of local changes: `tracked` paths against `HEAD` and `untracked` files as new files
/// (binary-safe; applies with `git apply`).
pub fn create_patch(git: &Git, tracked: &[String], untracked: &[String]) -> Result<String> {
    let base = ["--binary", "--no-ext-diff", "--no-color", "--src-prefix=a/", "--dst-prefix=b/", "--no-renames"];
    let mut patch = String::new();
    if !tracked.is_empty() {
        let mut args: Vec<String> = vec!["diff".into(), "HEAD".into()];
        args.extend(base.iter().map(|s| s.to_string()));
        args.push("--".into());
        args.extend(tracked.iter().map(|p| pathspec(p)));
        patch.push_str(&git.cmd(args).read_only().text()?);
    }
    for p in untracked {
        let mut args: Vec<String> = vec!["diff".into(), "--no-index".into()];
        args.extend(base.iter().map(|s| s.to_string()));
        args.push("--".into());
        args.push("/dev/null".into());
        args.push(p.clone());
        patch.push_str(&git.cmd(args).read_only().allow_exit(1).text()?);
    }
    Ok(patch)
}

/// Apply a patch to the work tree (`check`: only test whether it applies).
pub fn apply_patch(git: &Git, patch: &[u8], check: bool) -> Result<Output> {
    if patch.iter().all(u8::is_ascii_whitespace) {
        return Err(GitError::Invalid("the patch is empty".into()));
    }
    let mut args = vec!["apply", "--whitespace=nowarn"];
    if check {
        args.push("--check");
    }
    args.push("-");
    git.cmd(args).stdin(patch.to_vec()).run()
}

// ---------------------------------------------------------------- remotes

pub fn add_remote(git: &Git, name: &str, url: &str) -> Result<Output> {
    if !valid_ref_name(name) || name.contains('/') {
        return Err(GitError::Invalid(format!("'{name}' is not a valid remote name")));
    }
    if url.trim().is_empty() {
        return Err(GitError::Invalid("enter the remote's URL".into()));
    }
    git.run(["remote", "add", "--", name, url.trim()])
}

pub fn remove_remote(git: &Git, name: &str) -> Result<Output> {
    git.run(["remote", "remove", name])
}

pub fn set_remote_url(git: &Git, name: &str, url: &str) -> Result<Output> {
    if url.trim().is_empty() {
        return Err(GitError::Invalid("enter the remote's URL".into()));
    }
    git.run(["remote", "set-url", "--", name, url.trim()])
}

// ---------------------------------------------------------------- conflicts

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// The current branch ("Yours").
    Ours,
    /// The branch being merged ("Theirs").
    Theirs,
}

/// Resolve a conflict by taking one side entirely (deleting the file if that side deleted it).
pub fn accept_side(git: &Git, path: &str, kind: ConflictKind, side: Side) -> Result<Output> {
    let exists = match side {
        Side::Ours => kind.ours_exists(),
        Side::Theirs => kind.theirs_exists(),
    };
    let spec = pathspec(path);
    if exists {
        let flag = if side == Side::Ours { "--ours" } else { "--theirs" };
        git.run(["checkout", flag, "--", &spec])?;
        git.run(["add", "--", &spec])
    } else {
        git.run(["rm", "-q", "--", &spec])
    }
}

/// Mark a conflicted file resolved (as it is in the work tree).
pub fn mark_resolved(git: &Git, path: &str) -> Result<Output> {
    if git.dir().join(path).exists() {
        git.run(["add", "--", &pathspec(path)])
    } else {
        git.run(["rm", "-q", "--cached", "--", &pathspec(path)])
    }
}

/// The three versions of a conflicted file: (base, ours, theirs); `None` where a side lacks it.
pub fn conflict_versions(git: &Git, path: &str) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let stage = |n: u8| crate::log::text_at(git, &format!(":{n}"), path);
    Ok((stage(1)?, stage(2)?, stage(3)?))
}

/// Abort or continue the operation in progress.
pub fn abort_operation(git: &Git, state: crate::RepoState) -> Result<Output> {
    let args = state.abort_args().ok_or_else(|| GitError::Invalid("no merge or rebase is in progress".into()))?;
    git.run(args)
}

pub fn continue_operation(git: &Git, state: crate::RepoState) -> Result<Output> {
    let args = state.continue_args().ok_or_else(|| GitError::Invalid("no merge or rebase is in progress".into()))?;
    git.run(args)
}

/// `git diff` of one file (`staged`: index vs `HEAD`; otherwise work tree vs index).
pub fn diff_text(git: &Git, path: &str, staged: bool) -> Result<String> {
    let mut args = vec!["diff", "--no-ext-diff", "--no-color"];
    if staged {
        args.push("--cached");
    }
    let spec = pathspec(path);
    args.extend(["--", &spec]);
    git.cmd(args).read_only().text()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::status::{FileState, StatusOptions, status};
    use crate::test_util::{Sandbox, git_available, write};
    use crate::{CancelToken, ConsoleLine, RepoState, refs};

    fn state(git: &Git, path: &str) -> Option<FileState> {
        status(git, StatusOptions::default()).unwrap().file(path).map(|f| f.state())
    }

    #[test]
    fn commit_selected_files_with_options_and_hooks() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        write(&root, "a.txt", "a2\n");
        write(&root, "b.txt", "b2\n");
        write(&root, "new.txt", "n\n");
        // A hook prints to stderr: its output comes back.
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho checking from hook >&2\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let echo = git.clone().with_sink(tx);
        let req = CommitRequest {
            message: "Change a\n\nbody".into(),
            paths: Some(vec!["a.txt".into(), "new.txt".into()]),
            add: vec!["new.txt".into()],
            author: Some("Other Person <other@example.com>".into()),
            sign_off: true,
            ..Default::default()
        };
        let (out, head) = commit(&echo, &req).unwrap();
        assert!(out.combined().contains("checking from hook"), "{}", out.combined());
        assert_eq!(head.len(), 40);
        let console: Vec<ConsoleLine> = rx.try_iter().collect();
        assert!(console.iter().any(|l| matches!(l, ConsoleLine::Command(c) if c.starts_with("git commit"))));
        // Only the selected files were committed.
        assert_eq!(state(&git, "a.txt"), None);
        assert_eq!(state(&git, "new.txt"), None);
        assert_eq!(state(&git, "b.txt"), Some(FileState::Modified));
        let msg = git.read_text(["log", "-1", "--format=%an|%B"]).unwrap();
        assert!(msg.starts_with("Other Person|Change a\n\nbody"), "{msg}");
        assert!(msg.contains("Signed-off-by: Test User <test@example.com>"));
        // Amend with a new message.
        commit(&git, &CommitRequest { message: "Changed a".into(), amend: true, ..Default::default() }).unwrap();
        assert_eq!(git.read_text(["log", "-1", "--format=%s"]).unwrap().trim(), "Changed a");
        assert_eq!(git.read_text(["rev-list", "--count", "HEAD"]).unwrap().trim(), "2");
        // A failing hook stops the commit and its message is reported.
        std::fs::write(&hook, "#!/bin/sh\necho 'lint failed' >&2\nexit 1\n").unwrap();
        let err = commit(
            &git,
            &CommitRequest { message: "x".into(), paths: Some(vec!["b.txt".into()]), ..Default::default() },
        )
        .unwrap_err();
        assert!(err.to_string().contains("lint failed"), "{err}");
        assert!(commit(&git, &CommitRequest::default()).is_err(), "empty message");
    }

    #[test]
    fn many_paths_go_through_pathspec_files() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("keep.txt", "k\n")]);
        let names: Vec<String> = (0..MAX_ARG_PATHS + 5).map(|i| format!("f{i} [x].txt")).collect();
        for n in &names {
            write(&root, n, "x\n");
        }
        write(&root, "keep.txt", "changed\n");
        commit(
            &git,
            &CommitRequest {
                message: "many".into(),
                paths: Some(names.clone()),
                add: names.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let st = status(&git, StatusOptions::default()).unwrap();
        assert_eq!(st.files.len(), 1, "{:?}", st.files);
        assert_eq!(st.files[0].path, "keep.txt");
    }

    #[test]
    fn push_pull_fetch_with_a_bare_remote() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let remote = sb.root().join("remote.git");
        sb.git(&sb.root()).run(["init", "-q", "--bare", "-b", "main", &remote.display().to_string()]).unwrap();
        let (a, ga) = sb.repo("a", &[("f.txt", "1\n")]);
        ga.run(["remote", "add", "origin", &remote.display().to_string()]).unwrap();
        let outgoing_before = outgoing(&ga, "main", Some("origin/main")).unwrap();
        assert_eq!(outgoing_before.len(), 1);
        let (tx, rx) = crossbeam_channel::unbounded();
        push(
            &ga.clone().with_sink(tx),
            &PushRequest { remote: "origin".into(), branch: "main".into(), set_upstream: true, ..Default::default() },
        )
        .unwrap();
        assert!(rx.try_iter().any(|l| matches!(l, ConsoleLine::Command(c) if c.contains("push"))));
        let st = status(&ga, StatusOptions::default()).unwrap();
        assert_eq!(st.branch.upstream.as_deref(), Some("origin/main"));
        assert!(outgoing(&ga, "main", Some("origin/main")).unwrap().is_empty());
        // A second clone pushes a change; the first fetches and pulls it.
        let b = sb.root().join("b");
        clone(&sb.git(&sb.root()), &remote.display().to_string(), &b).unwrap();
        let gb = sb.git(&b);
        write(&b, "f.txt", "2\n");
        commit(
            &gb,
            &CommitRequest { message: "from b".into(), paths: Some(vec!["f.txt".into()]), ..Default::default() },
        )
        .unwrap();
        push(&gb, &PushRequest { remote: "origin".into(), branch: "main".into(), ..Default::default() }).unwrap();
        fetch(&ga, None).unwrap();
        let st = status(&ga, StatusOptions::default()).unwrap();
        assert_eq!(st.branch.behind, 1);
        // Pull with a local uncommitted change (autostash keeps it).
        write(&a, "other.txt", "local\n");
        ga.run(["add", "other.txt"]).unwrap();
        pull(&ga, &PullRequest { autostash: true, ..Default::default() }).unwrap();
        assert_eq!(std::fs::read_to_string(a.join("f.txt")).unwrap(), "2\n");
        assert_eq!(state(&ga, "other.txt"), Some(FileState::Added));
        // Remote branches: push a new one, delete it on the remote.
        create_branch(&ga, "feature", None, true).unwrap();
        push(
            &ga,
            &PushRequest {
                remote: "origin".into(),
                branch: "feature".into(),
                set_upstream: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(refs::branches(&ga).unwrap().iter().any(|b| b.remote && b.name == "origin/feature"));
        delete_branch(&ga, "origin/feature", true, false).unwrap();
        fetch(&ga, Some("origin")).unwrap();
        assert!(!refs::branches(&ga).unwrap().iter().any(|b| b.remote && b.name == "origin/feature"));
    }

    #[test]
    fn authentication_failures_are_reported_without_prompting() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (_, git) = sb.repo("r", &[("f.txt", "1\n")]);
        // A remote that needs credentials we can't ask for: an HTTP server that always answers 401
        // would need the network, so use a helper-less URL whose credential lookup fails at once.
        git.run(["remote", "add", "origin", "https://user@127.0.0.1:9/repo.git"]).unwrap();
        let err = fetch(&git, Some("origin")).unwrap_err();
        // Either refused connection or an authentication error; never a hang.
        assert!(matches!(err, GitError::Failed { .. } | GitError::AuthRequired(_)), "{err:?}");
    }

    #[test]
    fn long_operations_can_be_cancelled() {
        if !git_available() || !cfg!(unix) {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("f.txt", "1\n")]);
        // A hook that would run for a minute stands in for a slow push.
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 60\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(&root, "f.txt", "2\n");
        let token = CancelToken::new();
        let g = git.clone().with_cancel(token.clone());
        let t = std::thread::spawn(move || {
            commit(
                &g,
                &CommitRequest { message: "slow".into(), paths: Some(vec!["f.txt".into()]), ..Default::default() },
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let start = std::time::Instant::now();
        token.cancel();
        let r = t.join().unwrap();
        assert!(matches!(r, Err(GitError::Cancelled)), "{r:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn branches_merge_rebase_tags() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("f.txt", "1\n")]);
        create_branch(&git, "feature", None, true).unwrap();
        write(&root, "g.txt", "g\n");
        commit(
            &git,
            &CommitRequest {
                message: "g".into(),
                paths: Some(vec!["g.txt".into()]),
                add: vec!["g.txt".into()],
                ..Default::default()
            },
        )
        .unwrap();
        checkout(&git, "main", false).unwrap();
        assert!(!root.join("g.txt").exists());
        merge(&git, "feature").unwrap();
        assert!(root.join("g.txt").exists());
        rename_branch(&git, "feature", "done").unwrap();
        assert!(create_branch(&git, "bad name", None, false).is_err());
        create_branch(&git, "side", Some("HEAD~1"), false).unwrap();
        checkout(&git, "side", false).unwrap();
        write(&root, "h.txt", "h\n");
        commit(
            &git,
            &CommitRequest {
                message: "h".into(),
                paths: Some(vec!["h.txt".into()]),
                add: vec!["h.txt".into()],
                ..Default::default()
            },
        )
        .unwrap();
        rebase(&git, "main").unwrap();
        assert!(root.join("g.txt").exists() && root.join("h.txt").exists());
        checkout(&git, "main", false).unwrap();
        assert!(delete_branch(&git, "side", false, false).is_err(), "unmerged");
        delete_branch(&git, "side", false, true).unwrap();
        delete_branch(&git, "done", false, false).unwrap();
        create_tag(&git, "v1", None, Some("Release 1")).unwrap();
        create_tag(&git, "light", Some("HEAD~1"), None).unwrap();
        assert_eq!(refs::tags(&git).unwrap().len(), 2);
        delete_tag(&git, "light").unwrap();
        assert_eq!(git.read_text(["cat-file", "-t", "v1"]).unwrap().trim(), "tag");
    }

    #[test]
    fn stash_rollback_reset_patch_remotes() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        // Stash including untracked, then pop.
        write(&root, "a.txt", "a2\n");
        write(&root, "u.txt", "u\n");
        stash_push(&git, Some("wip"), true, &[]).unwrap();
        assert!(!root.join("u.txt").exists());
        let list = refs::stashes(&git).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].message.contains("wip"));
        stash_apply(&git, 0, true, false).unwrap();
        assert!(root.join("u.txt").exists());
        assert!(refs::stashes(&git).unwrap().is_empty());
        stash_push(&git, None, false, &["a.txt".to_string()]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        stash_apply(&git, 0, false, false).unwrap();
        stash_drop(&git, 0).unwrap();
        // Patch of local changes (with an untracked file) applies elsewhere.
        let patch = create_patch(&git, &["a.txt".to_string()], &["u.txt".to_string()]).unwrap();
        assert!(patch.contains("+a2") && patch.contains("new file mode"), "{patch}");
        // Rollback: modified and added files.
        write(&root, "n.txt", "n\n");
        git.run(["add", "n.txt"]).unwrap();
        rollback(&git, &root, &["a.txt".to_string()], &["n.txt".to_string()], false).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
        assert_eq!(state(&git, "n.txt"), Some(FileState::Untracked));
        std::fs::remove_file(root.join("n.txt")).unwrap();
        std::fs::remove_file(root.join("u.txt")).unwrap();
        apply_patch(&git, patch.as_bytes(), true).unwrap();
        apply_patch(&git, patch.as_bytes(), false).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a2\n");
        assert!(root.join("u.txt").exists());
        assert!(apply_patch(&git, b"  ", false).is_err());
        assert!(apply_patch(&git, b"garbage", false).is_err());
        // Reset: soft keeps changes staged, hard drops them.
        git.run(["add", "-A"]).unwrap();
        git.run(["commit", "-qm", "two"]).unwrap();
        reset(&git, "HEAD~1", ResetMode::Soft).unwrap();
        assert_eq!(state(&git, "a.txt"), Some(FileState::Modified));
        reset(&git, "HEAD", ResetMode::Hard).unwrap();
        assert_eq!(state(&git, "a.txt"), None);
        assert!(reset(&git, "--hard", ResetMode::Hard).is_err());
        // Remotes.
        add_remote(&git, "origin", "https://example.invalid/x.git").unwrap();
        set_remote_url(&git, "origin", "https://example.invalid/y.git").unwrap();
        assert_eq!(refs::remotes(&git).unwrap()[0].fetch_url, "https://example.invalid/y.git");
        assert!(add_remote(&git, "bad/name", "x").is_err());
        remove_remote(&git, "origin").unwrap();
        assert!(refs::remotes(&git).unwrap().is_empty());
        assert_eq!(RepoState::detect(&root.join(".git")), RepoState::Normal);
    }

    #[test]
    fn conflicts_take_ours_theirs_or_resolve() {
        if !git_available() {
            return;
        }
        let sb = Sandbox::new();
        let (root, git) = sb.repo("r", &[("c.txt", "base\n"), ("d.txt", "base\n"), ("e.txt", "base\n")]);
        create_branch(&git, "other", None, true).unwrap();
        for f in ["c.txt", "d.txt", "e.txt"] {
            write(&root, f, "theirs\n");
        }
        git.run(["commit", "-qam", "theirs"]).unwrap();
        checkout(&git, "main", false).unwrap();
        for f in ["c.txt", "d.txt", "e.txt"] {
            write(&root, f, "ours\n");
        }
        git.run(["commit", "-qam", "ours"]).unwrap();
        let err = merge(&git, "other").unwrap_err();
        assert!(err.to_string().to_lowercase().contains("conflict"), "{err}");
        let info = crate::discover(&git).unwrap().unwrap();
        assert_eq!(info.state(), RepoState::Merging);
        let st = status(&git, StatusOptions::default()).unwrap();
        assert_eq!(st.conflicts().count(), 3);
        let (base, ours, theirs) = conflict_versions(&git, "c.txt").unwrap();
        assert_eq!(
            (base.as_deref(), ours.as_deref(), theirs.as_deref()),
            (Some("base\n"), Some("ours\n"), Some("theirs\n"))
        );
        accept_side(&git, "c.txt", ConflictKind::BothModified, Side::Ours).unwrap();
        accept_side(&git, "d.txt", ConflictKind::BothModified, Side::Theirs).unwrap();
        write(&root, "e.txt", "merged by hand\n");
        mark_resolved(&git, "e.txt").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("c.txt")).unwrap(), "ours\n");
        assert_eq!(std::fs::read_to_string(root.join("d.txt")).unwrap(), "theirs\n");
        assert_eq!(status(&git, StatusOptions::default()).unwrap().conflicts().count(), 0);
        continue_operation(&git, RepoState::Merging).unwrap();
        assert_eq!(info.state(), RepoState::Normal);
        assert_eq!(git.read_text(["rev-list", "--count", "HEAD"]).unwrap().trim(), "4");
        assert!(abort_operation(&git, RepoState::Normal).is_err());
        // Abort a second conflicting merge.
        checkout(&git, "other", false).unwrap();
        write(&root, "c.txt", "again theirs\n");
        git.run(["commit", "-qam", "t2"]).unwrap();
        checkout(&git, "main", false).unwrap();
        write(&root, "c.txt", "again ours\n");
        git.run(["commit", "-qam", "o2"]).unwrap();
        assert!(merge(&git, "other").is_err());
        abort_operation(&git, info.state()).unwrap();
        assert_eq!(info.state(), RepoState::Normal);
        assert!(diff_text(&git, "c.txt", false).unwrap().is_empty());
    }
}
