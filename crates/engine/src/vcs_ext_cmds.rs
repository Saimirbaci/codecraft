//! More Git commands: per-hunk staging from the gutter change popup, the three-way merge view,
//! work trees (Git › New Worktree…, Worktrees…) and GitHub through the `gh` CLI (Clone
//! Repository, Create Pull Request). Menu commands open their dialog; `vcs.*` commands take
//! explicit params and do the work, so agents and the CLI drive them like the UI does.
//!
//! Commands that call `gh` (which talks to GitHub with the user's login) only run with explicit
//! params (`fetch: true`, a `title`), never from defaults alone.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_vcs::github::{GH_INSTALL_HINT, PullRequestRequest};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{bool_or, opt_str, opt_usize, req_str};
use crate::vcs::{self, Dialog};
use crate::{CommandError, Session};

fn has_repo(s: &Session) -> Result<(), String> {
    if s.vcs.repo.is_some() {
        return Ok(());
    }
    Err(s.vcs.unavailable.clone().unwrap_or_else(|| "the project is not a Git repository".into()))
}

fn idle(s: &Session) -> Result<(), String> {
    has_repo(s)?;
    match &s.vcs.op {
        Some(op) => Err(format!("“{}” is running", op.label)),
        None => Ok(()),
    }
}

fn has_branch(s: &Session) -> Result<(), String> {
    idle(s)?;
    s.vcs.branch_name().map(|_| ()).ok_or_else(|| "HEAD is detached: check out a branch first".into())
}

fn file_in_repo(s: &Session) -> Result<(), String> {
    idle(s)?;
    let (_, d) = s.ws.active().ok_or("no editor is open")?;
    let path = d.path.as_deref().ok_or("the file is not saved")?;
    s.vcs.rel(path).map(|_| ()).ok_or_else(|| "the file is outside the repository".into())
}

fn merge_open(s: &Session) -> Result<(), String> {
    s.vcs.merge.as_ref().map(|_| ()).ok_or_else(|| "no merge is open".into())
}

/// The hunk popup's document and line, or the active editor's caret line (or `line`).
fn hunk_target(s: &Session, p: &Value) -> Result<(codecraft_workspace::DocId, usize), CommandError> {
    let line = opt_usize(p, "line")?;
    if let (Some(h), None) = (&s.vcs.hunk, line) {
        return Ok((h.doc, h.change.start));
    }
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor".into()))?;
    Ok((d.id, line.unwrap_or_else(|| d.doc.char_to_line(e.selections.primary().head))))
}

fn rel_path(s: &Session, p: &str) -> String {
    let pb = PathBuf::from(p);
    if pb.is_absolute() { s.vcs.rel(&pb).unwrap_or_else(|| p.to_string()) } else { p.to_string() }
}

fn set_dialog(s: &mut Session, d: Dialog) {
    s.vcs.dialog = Some(d);
    s.vcs.dialog_error = None;
    s.vcs.branches_popup = false;
}

/// Open the Clone dialog (from File › New, Git › Clone… or Git › GitHub/GitLab).
fn open_clone(s: &mut Session, p: &Value, github: bool) -> CmdResult {
    let base = s
        .project_root()
        .and_then(|r| r.parent().map(std::path::Path::to_path_buf))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_default();
    let url = opt_str(p, "url").unwrap_or("").to_string();
    set_dialog(s, Dialog::Clone { directory: base.display().to_string(), url });
    s.vcs.clone_github = github;
    ok()
}

fn worktree_json(w: &codecraft_vcs::worktree::Worktree) -> Value {
    json!({
        "path": w.path,
        "branch": w.branch,
        "head": w.head,
        "main": w.main,
        "detached": w.detached,
        "locked": w.locked,
        "prunable": w.prunable,
    })
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        // ----------------------------------------------------------- hunks
        spec(
            "vcs.stageHunk",
            "Stage Lines",
            None,
            "{line?: int} — stage only the change at the caret line (or the given 0-based line, or the open change popup)",
            g,
            file_in_repo,
            |s, p| {
                let (doc, line) = hunk_target(s, p)?;
                vcs::stage_hunk(s, doc, line, true)?;
                ok()
            },
        ),
        spec(
            "vcs.unstageHunk",
            "Unstage Lines",
            None,
            "{line?: int} — take the staged part of the change at the caret line (or the given line) out of the index",
            g,
            file_in_repo,
            |s, p| {
                let (doc, line) = hunk_target(s, p)?;
                vcs::stage_hunk(s, doc, line, false)?;
                ok()
            },
        ),
        // ----------------------------------------------------------- merge view
        spec(
            "vcs.mergeView",
            "Merge Conflicts…",
            None,
            "{path: string} — open the three-way merge view for a conflicted file (loads in the background)",
            g,
            idle,
            |s, p| {
                let rel = rel_path(s, req_str(p, "path")?);
                vcs::open_merge(s, rel)?;
                ok()
            },
        ),
        spec(
            "vcs.merge.state",
            "Merge State",
            None,
            "→ {path, chunks: [{index, kind, base, ours, theirs, resolution, baseText, oursText, theirsText}], conflicts, unresolved, result}",
            g,
            merge_open,
            |s, _| Ok(s.vcs.merge.as_ref().map(|m| m.to_json()).unwrap_or(Value::Null)),
        ),
        spec(
            "vcs.merge.resolve",
            "Resolve Change",
            None,
            "{chunk: int, resolution: \"ours\"|\"theirs\"|\"both\"|\"theirsThenOurs\"|\"base\"|\"none\"} — Accept Left/Right/Both, ignore, or reset one change",
            g,
            merge_open,
            |s, p| {
                let index = opt_usize(p, "chunk")?
                    .ok_or_else(|| CommandError::BadParams("`chunk` (int) is required".into()))?;
                let r = crate::vcs_merge::parse_resolution(req_str(p, "resolution")?)?;
                let m = s.vcs.merge.as_mut().ok_or_else(|| CommandError::Failed("no merge is open".into()))?;
                m.resolve(index, r)?;
                ok()
            },
        ),
        spec(
            "vcs.merge.resolveAll",
            "Accept All",
            None,
            "{resolution: \"ours\"|\"theirs\"} — resolve every conflict with one side",
            g,
            merge_open,
            |s, p| {
                let r = crate::vcs_merge::parse_resolution(req_str(p, "resolution")?)?
                    .ok_or_else(|| CommandError::BadParams("choose ours or theirs".into()))?;
                if let Some(m) = s.vcs.merge.as_mut() {
                    m.resolve_all(r);
                }
                ok()
            },
        ),
        spec(
            "vcs.merge.applyNonConflicting",
            "Apply Non-Conflicting Changes",
            None,
            "",
            g,
            merge_open,
            |s, _| {
                if let Some(m) = s.vcs.merge.as_mut() {
                    m.apply_non_conflicting();
                }
                ok()
            },
        ),
        spec(
            "vcs.merge.select",
            "Select Change",
            None,
            "{chunk?: int, next?: bool} — focus a change, or the next (previous with next: false) unresolved one",
            g,
            merge_open,
            |s, p| {
                let chunk = opt_usize(p, "chunk")?;
                let next = bool_or(p, "next", true);
                let Some(m) = s.vcs.merge.as_mut() else { return ok() };
                let n = m.chunks.len();
                m.selected = match chunk {
                    Some(c) if c < n => c,
                    Some(c) => return Err(CommandError::BadParams(format!("there is no change #{c}"))),
                    None if n == 0 => 0,
                    None if next => m.next_unresolved((m.selected + 1) % n).unwrap_or(m.selected),
                    None => (1..=n)
                        .map(|k| (m.selected + n - k) % n)
                        .find(|i| m.resolutions.get(*i).is_some_and(Option::is_none))
                        .unwrap_or(m.selected),
                };
                Ok(json!({ "selected": m.selected }))
            },
        ),
        spec(
            "vcs.merge.apply",
            "Apply Merge",
            None,
            "{allowUnresolved?: bool} — write the result and mark the file resolved (refuses while conflicts are left unless allowUnresolved, which saves with markers and leaves it conflicted)",
            g,
            merge_open,
            |s, p| {
                crate::vcs_merge::apply(s, bool_or(p, "allowUnresolved", false))?;
                ok()
            },
        ),
        spec("vcs.merge.close", "Close Merge", None, "", g, always, |s, _| {
            s.vcs.merge = None;
            s.vcs.merge_loading = None;
            ok()
        }),
        // ----------------------------------------------------------- worktrees
        spec(
            "git.newWorktree",
            "New Worktree…",
            None,
            "{branch?: string} — open the New Worktree dialog",
            g,
            idle,
            |s, p| {
                let root = s.vcs.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let branch = opt_str(p, "branch").unwrap_or("").to_string();
                let suffix = if branch.is_empty() { "worktree".to_string() } else { branch.replace('/', "-") };
                let path = root.parent().map(|d| d.join(format!("{name}-{suffix}"))).unwrap_or_default();
                set_dialog(
                    s,
                    Dialog::NewWorktree {
                        path: path.display().to_string(),
                        branch,
                        new_branch: true,
                        base: String::new(),
                    },
                );
                ok()
            },
        ),
        spec(
            "vcs.addWorktree",
            "Create Worktree",
            None,
            "{path?: path, branch?: string, newBranch?: bool, base?: string, open?: bool} — add a work tree (defaults from the New Worktree dialog)",
            g,
            idle,
            |s, p| {
                let (mut path, mut branch, mut new_branch, mut base) = match &s.vcs.dialog {
                    Some(Dialog::NewWorktree { path, branch, new_branch, base }) => {
                        (path.clone(), branch.clone(), *new_branch, base.clone())
                    }
                    _ => (String::new(), String::new(), true, String::new()),
                };
                if let Some(v) = opt_str(p, "path") {
                    path = v.to_string();
                }
                if let Some(v) = opt_str(p, "branch") {
                    branch = v.to_string();
                }
                new_branch = bool_or(p, "newBranch", new_branch);
                if let Some(v) = opt_str(p, "base") {
                    base = v.to_string();
                }
                if path.trim().is_empty() || branch.trim().is_empty() {
                    s.vcs.dialog_error = Some("Enter a directory and a branch".into());
                    return Err(CommandError::BadParams("`path` and `branch` are required".into()));
                }
                let path = crate::normalize_path(std::path::Path::new(path.trim()));
                let base = Some(base).filter(|b| !b.trim().is_empty());
                vcs::add_worktree(s, path, branch.trim().to_string(), new_branch, base, bool_or(p, "open", false))?;
                ok()
            },
        ),
        spec("git.worktrees", "Worktrees…", None, "Open the Worktrees dialog", g, has_repo, |s, _| {
            set_dialog(s, Dialog::Worktrees { selected: None });
            vcs::load_worktrees(s);
            ok()
        }),
        spec(
            "vcs.worktrees",
            "List Worktrees",
            None,
            "{refresh?: bool} → {loading, worktrees: [{path, branch, head, main, detached, locked, prunable}]} (refresh reloads in the background)",
            g,
            has_repo,
            |s, p| {
                if bool_or(p, "refresh", false) {
                    vcs::load_worktrees(s);
                }
                let list: Vec<Value> = s.vcs.worktrees.iter().map(worktree_json).collect();
                Ok(json!({ "loading": s.vcs.worktrees_loading, "worktrees": list }))
            },
        ),
        spec(
            "vcs.removeWorktree",
            "Remove Worktree",
            None,
            "{path: path, force?: bool} — remove a linked work tree (force: even with local changes)",
            g,
            idle,
            |s, p| {
                let path = PathBuf::from(req_str(p, "path")?);
                vcs::remove_worktree(s, path, bool_or(p, "force", false))?;
                ok()
            },
        ),
        spec(
            "vcs.openWorktree",
            "Open Worktree",
            None,
            "{path: path} — open a work tree as the project",
            g,
            has_repo,
            |s, p| {
                let path = PathBuf::from(req_str(p, "path")?);
                if !s.vcs.worktrees.iter().any(|w| w.path == path) || !path.is_dir() {
                    return Err(CommandError::Failed(format!("{} is not a work tree of this repository", path.display())));
                }
                s.vcs.dialog = None;
                s.execute("file.open", &json!({ "path": path }))
            },
        ),
        // ----------------------------------------------------------- clone, GitHub
        spec(
            "file.new.projectFromVersionControl",
            "Project from Version Control…",
            None,
            "{url?: string} — open the Clone Repository dialog",
            g,
            always,
            |s, p| open_clone(s, p, false),
        ),
        spec(
            "git.gitHub.cloneRepository",
            "Clone Repository…",
            None,
            "{url?: string} — open the Clone Repository dialog with your GitHub repositories (through `gh`)",
            g,
            always,
            |s, p| open_clone(s, p, true),
        ),
        spec(
            "git.gitLab.cloneRepository",
            "Clone Repository…",
            None,
            "{url?: string} — open the Clone Repository dialog",
            g,
            always,
            |s, p| open_clone(s, p, false),
        ),
        spec(
            "vcs.github.status",
            "GitHub CLI Status",
            None,
            "→ {gh: path|null, hint, remote}",
            g,
            always,
            |s, _| {
                let gh = s.vcs.gh_path();
                Ok(json!({
                    "gh": gh,
                    "hint": if gh.is_none() { Some(GH_INSTALL_HINT) } else { None },
                    "remote": s.vcs.github_remote().map(|r| r.name.clone()),
                }))
            },
        ),
        spec(
            "vcs.github.repositories",
            "GitHub Repositories",
            None,
            "{fetch?: bool} → {loading, repositories: [{nameWithOwner, url, description}], error} (fetch: true asks `gh` in the background)",
            g,
            always,
            |s, p| {
                if bool_or(p, "fetch", false) {
                    vcs::load_github_repos(s)?;
                }
                let (repos, error) = match &s.vcs.github_repos {
                    Some(Ok(list)) => (json!(list), Value::Null),
                    Some(Err(e)) => (json!([]), json!(e)),
                    None => (json!([]), Value::Null),
                };
                Ok(json!({ "loading": s.vcs.github_loading, "repositories": repos, "error": error }))
            },
        ),
        spec(
            "git.gitHub.createPullRequest",
            "Create Pull Request…",
            None,
            "Open the Create Pull Request dialog for the current branch",
            g,
            has_branch,
            |s, _| {
                let head = s.vcs.status.branch.oid.clone();
                let title = s
                    .vcs
                    .log
                    .commits
                    .iter()
                    .find(|c| Some(&c.hash) == head.as_ref())
                    .map(|c| c.subject.clone())
                    .or_else(|| s.vcs.branch_name().map(str::to_string))
                    .unwrap_or_default();
                set_dialog(s, Dialog::CreatePullRequest { title, body: String::new(), base: String::new(), draft: false });
                if s.vcs.gh_path().is_none() {
                    s.vcs.dialog_error = Some(GH_INSTALL_HINT.into());
                }
                ok()
            },
        ),
        spec(
            "vcs.createPullRequest",
            "Create Pull Request",
            None,
            "{title: string, body?: string, base?: string, draft?: bool, open?: bool} — push the branch if needed, then `gh pr create` (open: show it in the browser)",
            g,
            has_branch,
            |s, p| {
                let title = req_str(p, "title")?.to_string();
                let req = PullRequestRequest {
                    title,
                    body: opt_str(p, "body").unwrap_or("").to_string(),
                    base: opt_str(p, "base").map(str::to_string),
                    head: String::new(),
                    draft: bool_or(p, "draft", false),
                };
                if let Err(e) = vcs::create_pull_request(s, req, bool_or(p, "open", false)) {
                    s.vcs.dialog_error = Some(e.to_string());
                    return Err(e);
                }
                ok()
            },
        ),
    ]
}
