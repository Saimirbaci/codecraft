//! Git commands: the Git menu (catalog `git.*` ids; shortcuts from RustRover's macOS keymap),
//! Git › Current File (`git.currentFile.*`, a submenu RustRover fills lazily), and `vcs.*`
//! commands the Commit and Git tool windows, the branch widget and agents use.
//!
//! Menu commands open their dialog or tool window; the `vcs.*` commands take explicit params and
//! do the work, so the UI, the CLI and agents drive Git the same way.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_vcs::ops::{PullRequest, PushRequest, ResetMode, Side};
use codecraft_vcs::{FileState, FileStatus};
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{bool_or, opt_str, opt_usize, req_str, req_usize};
use crate::vcs::{self, CommitTab, Dialog, GitTab};
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

fn running(s: &Session) -> Result<(), String> {
    s.vcs.op.as_ref().map(|_| ()).ok_or_else(|| "no Git operation is running".into())
}

fn file_in_repo(s: &Session) -> Result<(), String> {
    has_repo(s)?;
    let (_, d) = s.ws.active().ok_or("no editor is open")?;
    let path = d.path.as_deref().ok_or("the file is not saved")?;
    s.vcs.rel(path).map(|_| ()).ok_or_else(|| "the file is outside the repository".into())
}

fn has_conflicts(s: &Session) -> Result<(), String> {
    idle(s)?;
    if s.vcs.conflicts().is_empty() && s.vcs.state == codecraft_vcs::RepoState::Normal {
        return Err("there are no merge conflicts".into());
    }
    Ok(())
}

/// The active document's repository-relative path.
fn active_rel(s: &Session) -> Result<String, CommandError> {
    let path = s.ws.active().and_then(|(_, d)| d.path.clone()).ok_or_else(|| CommandError::Failed("no file".into()))?;
    s.vcs.rel(&path).ok_or_else(|| CommandError::Failed("the file is outside the repository".into()))
}

fn str_list(p: &Value, key: &str) -> Option<Vec<String>> {
    p.get(key)?.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

/// Accept repository-relative or absolute paths.
fn rel_paths(s: &Session, paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| {
            let pb = PathBuf::from(&p);
            if pb.is_absolute() { s.vcs.rel(&pb).unwrap_or(p) } else { p }
        })
        .collect()
}

fn set_dialog(s: &mut Session, d: Dialog) {
    s.vcs.dialog = Some(d);
    s.vcs.dialog_error = None;
    s.vcs.branches_popup = false;
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        // ----------------------------------------------------------- Git menu
        spec(
            "git.commit",
            "Commit…",
            None,
            "Show the Commit tool window and focus the message",
            g,
            has_repo,
            |s, _| {
                s.vcs.commit_tab = CommitTab::Changes;
                s.vcs.focus_message = true;
                s.ws.layout.show(ToolWindowId::Commit);
                vcs::request_refresh(s, false);
                ok()
            },
        ),
        spec("git.push", "Push…", None, "Open the Push dialog for the current branch", g, has_branch, |s, _| {
            vcs::open_push_dialog(s);
            ok()
        }),
        spec(
            "git.updateProject",
            "Update Project…",
            None,
            "{rebase?: bool} — pull the current branch's upstream (stashing local changes around it)",
            g,
            has_branch,
            |s, p| {
                if s.vcs.status.branch.upstream.is_none() {
                    return Err(CommandError::Failed("The current branch has no tracked branch: push it first".into()));
                }
                let req = PullRequest {
                    rebase: p.get("rebase").and_then(Value::as_bool),
                    autostash: true,
                    ..PullRequest::default()
                };
                vcs::pull(s, req, "Update Project")?;
                ok()
            },
        ),
        spec("git.pull", "Pull…", None, "Open the Pull dialog", g, has_branch, |s, _| {
            let (remote, branch) = match s.vcs.status.branch.upstream.as_deref().and_then(|u| u.split_once('/')) {
                Some((r, b)) => (r.to_string(), b.to_string()),
                None => (s.vcs.remotes.first().map(|r| r.name.clone()).unwrap_or_default(), s.vcs.head_label()),
            };
            set_dialog(s, Dialog::Pull { remote, branch, rebase: false });
            ok()
        }),
        spec("git.fetch", "Fetch", None, "Fetch all remotes", g, idle, |s, _| {
            vcs::fetch(s)?;
            ok()
        }),
        spec("git.merge", "Merge…", None, "Choose a branch to merge into the current one", g, has_branch, |s, _| {
            set_dialog(s, Dialog::ChooseBranch { rebase: false, branch: String::new() });
            ok()
        }),
        spec(
            "git.rebase",
            "Rebase…",
            None,
            "Choose a branch to rebase the current one onto",
            g,
            has_branch,
            |s, _| {
                set_dialog(s, Dialog::ChooseBranch { rebase: true, branch: String::new() });
                ok()
            },
        ),
        spec("git.branches", "Branches…", None, "Show the branches popup", g, has_repo, |s, _| {
            s.vcs.branches_popup = !s.vcs.branches_popup;
            if s.vcs.branches_popup {
                vcs::request_refresh(s, true);
            }
            ok()
        }),
        spec(
            "git.vcsOperationsPopup",
            "VCS Operations…",
            None,
            "Show the branches and VCS operations popup",
            g,
            has_repo,
            |s, _| {
                s.vcs.branches_popup = !s.vcs.branches_popup;
                ok()
            },
        ),
        spec(
            "git.newBranch",
            "New Branch…",
            None,
            "{from?: string} — open the New Branch dialog",
            g,
            idle,
            |s, p| {
                set_dialog(
                    s,
                    Dialog::NewBranch {
                        name: String::new(),
                        from: opt_str(p, "from").map(str::to_string),
                        checkout: true,
                    },
                );
                ok()
            },
        ),
        spec("git.newTag", "New Tag…", None, "{rev?: string} — open the New Tag dialog", g, idle, |s, p| {
            let rev = opt_str(p, "rev").unwrap_or("HEAD").to_string();
            set_dialog(s, Dialog::NewTag { name: String::new(), rev, message: String::new() });
            ok()
        }),
        spec(
            "git.resetHead",
            "Reset HEAD…",
            None,
            "{rev?: string} — open the Reset HEAD dialog",
            g,
            idle,
            |s, p| {
                let rev = opt_str(p, "rev").unwrap_or("HEAD").to_string();
                set_dialog(s, Dialog::Reset { rev, mode: ResetMode::Mixed });
                ok()
            },
        ),
        spec("git.showGitLog", "Show Git Log", None, "Show the Git tool window's Log tab", g, has_repo, |s, _| {
            vcs::show_git_window(s, GitTab::Log);
            ok()
        }),
        spec(
            "git.patch.createPatchFromLocalChanges",
            "Create Patch from Local Changes…",
            None,
            "Open the Create Patch dialog for the changes included in the commit",
            g,
            idle,
            |s, _| {
                let files = s.vcs.included_paths();
                let path = s
                    .vcs
                    .repo
                    .as_ref()
                    .map(|r| r.root.join("local-changes.patch").display().to_string())
                    .unwrap_or_default();
                set_dialog(s, Dialog::CreatePatch { path, files, to_clipboard: false });
                ok()
            },
        ),
        spec("git.patch.applyPatch", "Apply Patch…", None, "Open the Apply Patch dialog", g, idle, |s, _| {
            set_dialog(s, Dialog::ApplyPatch { path: String::new(), text: String::new() });
            ok()
        }),
        spec(
            "git.patch.applyPatchFromClipboard",
            "Apply Patch from Clipboard…",
            None,
            "{text?: string} — open the Apply Patch dialog with the clipboard's text",
            g,
            idle,
            |s, p| {
                let text = opt_str(p, "text")
                    .map(str::to_string)
                    .or_else(|| s.clipboard.as_ref().map(|c| c.0.clone()))
                    .unwrap_or_default();
                set_dialog(s, Dialog::ApplyPatch { path: String::new(), text });
                ok()
            },
        ),
        spec(
            "git.uncommittedChanges.shelveChanges",
            "Shelve Changes…",
            None,
            "{files?: [path], changelist?: string} — open the Shelve Changes dialog with the checked changes (or the given files, or a changelist's); shelves are patches under .codecraft/shelf",
            g,
            idle,
            vcs::shelf::shelve_dialog,
        ),
        spec(
            "git.uncommittedChanges.showShelf",
            "Show Shelf",
            None,
            "Show the Shelf tab of the Commit tool window",
            g,
            has_repo,
            vcs::shelf::show_shelf_cmd,
        ),
        spec(
            "git.uncommittedChanges.stashChanges",
            "Stash Changes…",
            None,
            "Open the Stash dialog",
            g,
            idle,
            stash_dialog,
        ),
        spec(
            "git.uncommittedChanges.unstashChanges",
            "Unstash Changes…",
            None,
            "Open the Unstash dialog",
            g,
            idle,
            |s, _| {
                let selected = s.vcs.stashes.first().map(|st| st.index);
                set_dialog(s, Dialog::Unstash { selected, pop: true, reinstate_index: false });
                vcs::request_refresh(s, true);
                ok()
            },
        ),
        spec(
            "git.uncommittedChanges.rollback",
            "Rollback…",
            None,
            "{files?: [path]} — confirm rolling back files (default: the changes included in the commit)",
            g,
            idle,
            |s, p| {
                let files = str_list(p, "files").map(|f| rel_paths(s, f)).unwrap_or_else(|| {
                    s.vcs.changes().into_iter().filter(|f| s.vcs.is_included(f)).map(|f| f.path.clone()).collect()
                });
                if files.is_empty() {
                    return Err(CommandError::Failed("Select the changes to roll back".into()));
                }
                set_dialog(s, Dialog::Rollback { files, delete_added: false });
                ok()
            },
        ),
        spec("git.manageRemotes", "Manage Remotes…", None, "Open the Git Remotes dialog", g, has_repo, |s, _| {
            set_dialog(s, Dialog::Remotes { name: String::new(), url: String::new() });
            ok()
        }),
        spec(
            "git.clone",
            "Clone…",
            None,
            "{url?: string} — open the Clone Repository dialog",
            g,
            always,
            |s, p| {
                let base = s
                    .project_root()
                    .and_then(|r| r.parent().map(std::path::Path::to_path_buf))
                    .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                    .unwrap_or_default();
                let url = opt_str(p, "url").unwrap_or("").to_string();
                set_dialog(s, Dialog::Clone { directory: base.display().to_string(), url });
                ok()
            },
        ),
        // ----------------------------------------------------------- Git › Current File
        spec(
            "git.currentFile.annotate",
            "Annotate with Git Blame",
            None,
            "Toggle the blame column (author, date, commit) in the active editor's gutter",
            g,
            file_in_repo,
            |s, _| {
                let id = s.ws.active().map(|(_, d)| d.id).ok_or_else(|| CommandError::Failed("no editor".into()))?;
                Ok(json!({ "shown": vcs::toggle_blame(s, id) }))
            },
        ),
        spec(
            "git.currentFile.showHistory",
            "Show History",
            None,
            "The Git log of the active file",
            g,
            file_in_repo,
            |s, _| {
                let rel = active_rel(s)?;
                s.vcs.log.path = Some(rel);
                s.vcs.log.selected = None;
                vcs::show_git_window(s, GitTab::Log);
                vcs::load_log(s, true);
                ok()
            },
        ),
        spec(
            "git.currentFile.showDiff",
            "Show Diff",
            None,
            "Compare the active file with its last committed version (HEAD)",
            g,
            file_in_repo,
            |s, _| {
                let rel = active_rel(s)?;
                vcs::compare_with(s, &rel, "HEAD");
                ok()
            },
        ),
        spec(
            "git.currentFile.compareWithRevision",
            "Compare with Revision…",
            None,
            "{rev?: string} — compare the active file with a revision (without `rev`, choose one)",
            g,
            file_in_repo,
            |s, p| {
                let rel = active_rel(s)?;
                match opt_str(p, "rev") {
                    Some(rev) => vcs::compare_with(s, &rel, rev),
                    None => set_dialog(s, Dialog::Compare { path: rel, rev: "HEAD".into(), branch: false }),
                }
                ok()
            },
        ),
        spec(
            "git.currentFile.compareWithBranch",
            "Compare with Branch…",
            None,
            "{branch?: string} — compare the active file with another branch",
            g,
            file_in_repo,
            |s, p| {
                let rel = active_rel(s)?;
                match opt_str(p, "branch") {
                    Some(b) => vcs::compare_with(s, &rel, b),
                    None => {
                        let other = s
                            .vcs
                            .branches
                            .iter()
                            .find(|b| !b.current)
                            .map(|b| b.name.clone())
                            .unwrap_or_else(|| "HEAD".into());
                        set_dialog(s, Dialog::Compare { path: rel, rev: other, branch: true });
                    }
                }
                ok()
            },
        ),
        spec(
            "git.currentFile.rollback",
            "Rollback…",
            None,
            "Confirm rolling back the active file",
            g,
            file_in_repo,
            |s, _| {
                let rel = active_rel(s)?;
                if s.vcs.status.file(&rel).is_none_or(|f| f.untracked || f.ignored) {
                    return Err(CommandError::Failed("The file has no changes to roll back".into()));
                }
                set_dialog(s, Dialog::Rollback { files: vec![rel], delete_added: false });
                ok()
            },
        ),
        // ----------------------------------------------------------- tool windows
        spec("vcs.refresh", "Refresh VCS Status", None, "", g, has_repo, |s, _| {
            vcs::request_refresh(s, true);
            ok()
        }),
        spec(
            "vcs.status",
            "Git Status",
            None,
            "→ {root, branch, upstream, ahead, behind, state, files: [{path, state, staged}]}",
            g,
            has_repo,
            |s, _| Ok(status_json(s)),
        ),
        spec(
            "vcs.cancel",
            "Cancel Git Operation",
            None,
            "Stop the running push, pull, fetch, clone, …",
            g,
            running,
            |s, _| Ok(json!({ "cancelled": vcs::cancel(s) })),
        ),
        spec("vcs.showConsole", "Show Git Console", None, "", g, always, |s, _| {
            vcs::show_git_window(s, GitTab::Console);
            ok()
        }),
        spec("vcs.closeDialog", "Close Git Dialog", None, "", g, always, |s, _| {
            s.vcs.dialog = None;
            s.vcs.dialog_error = None;
            s.vcs.compare = None;
            s.vcs.hunk = None;
            s.vcs.branches_popup = false;
            ok()
        }),
        spec(
            "vcs.setCommitMessage",
            "Set Commit Message",
            None,
            "{message: string, amend?: bool}",
            g,
            has_repo,
            |s, p| {
                s.vcs.commit.message = req_str(p, "message")?.to_string();
                if let Some(a) = p.get("amend").and_then(Value::as_bool) {
                    set_amend(s, a);
                }
                ok()
            },
        ),
        spec(
            "vcs.generateMessage",
            "Generate Commit Message",
            None,
            "Write the commit message for the included changes with the language model chosen in \
             Settings (commit_ai_provider: openrouter, ollama, claude or openai; commit_ai_model; \
             ollama_url; commit_ai_keys)",
            g,
            |s| {
                has_repo(s)?;
                if s.vcs.generating {
                    return Err("a commit message is being generated".into());
                }
                Ok(())
            },
            |s, _| {
                vcs::generate_message(s)?;
                ok()
            },
        ),
        spec(
            "vcs.setAmend",
            "Amend Commit",
            None,
            "{amend: bool} — amending fills an empty message with the last commit's",
            g,
            has_repo,
            |s, p| {
                set_amend(s, bool_or(p, "amend", !s.vcs.commit.amend));
                ok()
            },
        ),
        spec(
            "vcs.include",
            "Include in Commit",
            None,
            "{path: string, included: bool} | {all: bool} — check or uncheck changes for commit",
            g,
            has_repo,
            include,
        ),
        spec(
            "vcs.selectChange",
            "Show Change Diff",
            None,
            "{path: string} — select a change and preview its diff",
            g,
            has_repo,
            |s, p| {
                let path = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
                vcs::select_change(s, &path);
                ok()
            },
        ),
        spec(
            "vcs.commit",
            "Commit",
            None,
            "{message?: string, files?: [path], amend?: bool, author?: string, signOff?: bool, push?: bool} — commit \
             the given files (default: the changes checked in the Commit tool window); `push` opens the Push \
             dialog afterwards",
            g,
            idle,
            run_commit,
        ),
        spec(
            "vcs.push",
            "Push",
            None,
            "{remote?: string, branch?: string, remoteBranch?: string, setUpstream?: bool, force?: bool} — push \
             (defaults: the Push dialog's choices, else the current branch to its upstream)",
            g,
            has_branch,
            run_push,
        ),
        spec(
            "vcs.pull",
            "Pull",
            None,
            "{remote?: string, branch?: string, rebase?: bool} — pull (defaults: the Pull dialog, else the upstream)",
            g,
            has_branch,
            |s, p| {
                let mut req = PullRequest::default();
                if let Some(Dialog::Pull { remote, branch, rebase }) = &s.vcs.dialog {
                    req.remote = Some(remote.clone()).filter(|r| !r.is_empty());
                    req.branch = Some(branch.clone()).filter(|b| !b.is_empty());
                    req.rebase = Some(*rebase);
                }
                if let Some(r) = opt_str(p, "remote") {
                    req.remote = Some(r.to_string());
                }
                if let Some(b) = opt_str(p, "branch") {
                    req.branch = Some(b.to_string());
                }
                if let Some(r) = p.get("rebase").and_then(Value::as_bool) {
                    req.rebase = Some(r);
                }
                s.vcs.dialog = None;
                vcs::pull(s, req, "Pull")?;
                ok()
            },
        ),
        spec("vcs.checkout", "Checkout Branch", None, "{branch: string, remote?: bool}", g, idle, |s, p| {
            let name = req_str(p, "branch")?.to_string();
            let remote = p
                .get("remote")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| s.vcs.branches.iter().any(|b| b.remote && b.name == name));
            s.vcs.branches_popup = false;
            let label = format!("Checkout {name}");
            vcs::simple_op(s, &label, false, move |git| codecraft_vcs::ops::checkout(git, &name, remote))?;
            ok()
        }),
        spec(
            "vcs.newBranch",
            "Create Branch",
            None,
            "{name?: string, from?: string, checkout?: bool} — defaults from the New Branch dialog",
            g,
            idle,
            |s, p| {
                let (mut name, mut from, mut checkout) = match &s.vcs.dialog {
                    Some(Dialog::NewBranch { name, from, checkout }) => (name.clone(), from.clone(), *checkout),
                    _ => (String::new(), None, true),
                };
                if let Some(n) = opt_str(p, "name") {
                    name = n.to_string();
                }
                if let Some(f) = opt_str(p, "from") {
                    from = Some(f.to_string());
                }
                checkout = bool_or(p, "checkout", checkout);
                let name = name.trim().to_string();
                if !codecraft_vcs::valid_ref_name(&name) {
                    s.vcs.dialog_error = Some(format!("“{name}” is not a valid branch name"));
                    return Err(CommandError::BadParams(format!("invalid branch name: {name:?}")));
                }
                s.vcs.dialog = None;
                s.vcs.branches_popup = false;
                let label = format!("Create Branch {name}");
                vcs::simple_op(s, &label, false, move |git| {
                    codecraft_vcs::ops::create_branch(git, &name, from.as_deref(), checkout)
                })?;
                ok()
            },
        ),
        spec(
            "vcs.renameBranch",
            "Rename Branch",
            None,
            "{old?: string, new?: string} — defaults from the Rename dialog",
            g,
            idle,
            |s, p| {
                let (mut old, mut new) = match &s.vcs.dialog {
                    Some(Dialog::RenameBranch { old, name }) => (old.clone(), name.clone()),
                    _ => (String::new(), String::new()),
                };
                if let Some(o) = opt_str(p, "old") {
                    old = o.to_string();
                }
                if let Some(n) = opt_str(p, "new") {
                    new = n.to_string();
                }
                let new = new.trim().to_string();
                if old.is_empty() || !codecraft_vcs::valid_ref_name(&new) {
                    s.vcs.dialog_error = Some(format!("“{new}” is not a valid branch name"));
                    return Err(CommandError::BadParams("`old` and a valid `new` name are required".into()));
                }
                s.vcs.dialog = None;
                vcs::simple_op(s, "Rename Branch", false, move |git| {
                    codecraft_vcs::ops::rename_branch(git, &old, &new)
                })?;
                ok()
            },
        ),
        spec(
            "vcs.deleteBranch",
            "Delete Branch",
            None,
            "{name: string, remote?: bool, force?: bool} — a remote branch is deleted on its remote",
            g,
            idle,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                let remote = bool_or(p, "remote", s.vcs.branches.iter().any(|b| b.remote && b.name == name));
                let force = bool_or(p, "force", false);
                if !remote && s.vcs.branch_name() == Some(name.as_str()) {
                    return Err(CommandError::Failed("Cannot delete the current branch".into()));
                }
                s.vcs.dialog = None;
                let label = format!("Delete Branch {name}");
                vcs::simple_op(s, &label, false, move |git| {
                    codecraft_vcs::ops::delete_branch(git, &name, remote, force)
                })?;
                ok()
            },
        ),
        spec(
            "vcs.merge",
            "Merge Branch",
            None,
            "{branch: string} — merge into the current branch",
            g,
            has_branch,
            |s, p| {
                let branch = branch_param(s, p)?;
                s.vcs.dialog = None;
                s.vcs.branches_popup = false;
                let label = format!("Merge {branch}");
                vcs::simple_op(s, &label, true, move |git| codecraft_vcs::ops::merge(git, &branch))?;
                ok()
            },
        ),
        spec(
            "vcs.rebase",
            "Rebase onto Branch",
            None,
            "{onto: string} — rebase the current branch",
            g,
            has_branch,
            |s, p| {
                let onto = match opt_str(p, "onto") {
                    Some(o) => o.to_string(),
                    None => branch_param(s, p)?,
                };
                s.vcs.dialog = None;
                s.vcs.branches_popup = false;
                let label = format!("Rebase onto {onto}");
                vcs::simple_op(s, &label, true, move |git| codecraft_vcs::ops::rebase(git, &onto))?;
                ok()
            },
        ),
        spec(
            "vcs.newTag",
            "Create Tag",
            None,
            "{name?: string, rev?: string, message?: string} — an annotated tag when `message` is given",
            g,
            idle,
            |s, p| {
                let (mut name, mut rev, mut message) = match &s.vcs.dialog {
                    Some(Dialog::NewTag { name, rev, message }) => (name.clone(), rev.clone(), message.clone()),
                    _ => (String::new(), String::new(), String::new()),
                };
                if let Some(v) = opt_str(p, "name") {
                    name = v.to_string();
                }
                if let Some(v) = opt_str(p, "rev") {
                    rev = v.to_string();
                }
                if let Some(v) = opt_str(p, "message") {
                    message = v.to_string();
                }
                let name = name.trim().to_string();
                if !codecraft_vcs::valid_ref_name(&name) {
                    s.vcs.dialog_error = Some(format!("“{name}” is not a valid tag name"));
                    return Err(CommandError::BadParams(format!("invalid tag name: {name:?}")));
                }
                s.vcs.dialog = None;
                let label = format!("Create Tag {name}");
                vcs::simple_op(s, &label, false, move |git| {
                    codecraft_vcs::ops::create_tag(git, &name, Some(rev.as_str()), Some(message.as_str()))
                })?;
                ok()
            },
        ),
        spec(
            "vcs.reset",
            "Reset HEAD",
            None,
            "{rev?: string, mode?: soft|mixed|hard|keep} — defaults from the Reset dialog",
            g,
            idle,
            |s, p| {
                let (mut rev, mut mode) = match &s.vcs.dialog {
                    Some(Dialog::Reset { rev, mode }) => (rev.clone(), *mode),
                    _ => ("HEAD".to_string(), ResetMode::Mixed),
                };
                if let Some(r) = opt_str(p, "rev") {
                    rev = r.to_string();
                }
                if let Some(m) = opt_str(p, "mode") {
                    mode = ResetMode::parse(m)
                        .ok_or_else(|| CommandError::BadParams(format!("unknown reset mode {m:?}")))?;
                }
                s.vcs.dialog = None;
                let label = format!("Reset HEAD to {rev}");
                vcs::simple_op(s, &label, false, move |git| codecraft_vcs::ops::reset(git, &rev, mode))?;
                ok()
            },
        ),
        spec(
            "vcs.stash",
            "Stash Changes",
            None,
            "{message?: string, includeUntracked?: bool} — defaults from the Stash dialog",
            g,
            idle,
            |s, p| {
                let (mut message, mut untracked) = match &s.vcs.dialog {
                    Some(Dialog::Stash { message, include_untracked }) => (message.clone(), *include_untracked),
                    _ => (String::new(), false),
                };
                if let Some(m) = opt_str(p, "message") {
                    message = m.to_string();
                }
                untracked = bool_or(p, "includeUntracked", untracked);
                s.vcs.dialog = None;
                let _ = s.save_all();
                vcs::simple_op(s, "Stash", false, move |git| {
                    codecraft_vcs::ops::stash_push(git, Some(&message), untracked, &[])
                })?;
                ok()
            },
        ),
        spec(
            "vcs.unstash",
            "Unstash Changes",
            None,
            "{index?: int, pop?: bool, reinstateIndex?: bool} — apply (or pop) a stash; defaults from the dialog",
            g,
            idle,
            |s, p| {
                let (mut index, mut pop, mut reinstate) = match &s.vcs.dialog {
                    Some(Dialog::Unstash { selected, pop, reinstate_index }) => (*selected, *pop, *reinstate_index),
                    _ => (None, true, false),
                };
                if let Some(i) = opt_usize(p, "index")? {
                    index = Some(i);
                }
                pop = bool_or(p, "pop", pop);
                reinstate = bool_or(p, "reinstateIndex", reinstate);
                let index = index.ok_or_else(|| CommandError::BadParams("choose a stash (`index`)".into()))?;
                s.vcs.dialog = None;
                let label = if pop { "Unstash (pop)" } else { "Unstash (apply)" };
                vcs::simple_op(s, label, true, move |git| codecraft_vcs::ops::stash_apply(git, index, pop, reinstate))?;
                ok()
            },
        ),
        spec("vcs.dropStash", "Drop Stash", None, "{index: int}", g, idle, |s, p| {
            let index = req_usize(p, "index")?;
            if let Some(Dialog::Unstash { selected, .. }) = &mut s.vcs.dialog {
                *selected = None;
            }
            vcs::simple_op(s, "Drop Stash", false, move |git| codecraft_vcs::ops::stash_drop(git, index))?;
            ok()
        }),
        spec(
            "vcs.rollback",
            "Rollback Files",
            None,
            "{files?: [path], deleteAdded?: bool} — restore files to HEAD (defaults from the Rollback dialog)",
            g,
            idle,
            |s, p| {
                let (mut files, mut delete_added) = match &s.vcs.dialog {
                    Some(Dialog::Rollback { files, delete_added }) => (files.clone(), *delete_added),
                    _ => (Vec::new(), false),
                };
                if let Some(f) = str_list(p, "files") {
                    files = rel_paths(s, f);
                }
                delete_added = bool_or(p, "deleteAdded", delete_added);
                vcs::rollback(s, &files, delete_added)?;
                ok()
            },
        ),
        spec(
            "vcs.createPatch",
            "Create Patch",
            None,
            "{path?: string, files?: [path], clipboard?: bool} — write a patch of local changes to `path` or copy it",
            g,
            idle,
            |s, p| {
                let (mut path, mut files, mut clip) = match &s.vcs.dialog {
                    Some(Dialog::CreatePatch { path, files, to_clipboard }) => {
                        (path.clone(), files.clone(), *to_clipboard)
                    }
                    _ => (String::new(), s.vcs.included_paths(), false),
                };
                if let Some(v) = opt_str(p, "path") {
                    path = v.to_string();
                }
                if let Some(f) = str_list(p, "files") {
                    files = rel_paths(s, f);
                }
                clip = bool_or(p, "clipboard", clip);
                let target = if clip {
                    None
                } else {
                    if path.trim().is_empty() {
                        return Err(CommandError::BadParams("`path` (or `clipboard: true`) is required".into()));
                    }
                    let p = PathBuf::from(path.trim());
                    Some(if p.is_absolute() { p } else { s.vcs.abs(path.trim()).unwrap_or(p) })
                };
                vcs::create_patch(s, &files, target)?;
                ok()
            },
        ),
        spec(
            "vcs.applyPatch",
            "Apply Patch",
            None,
            "{path?: string, text?: string} — apply a patch file or text to the working tree",
            g,
            idle,
            |s, p| {
                let (mut path, mut text) = match &s.vcs.dialog {
                    Some(Dialog::ApplyPatch { path, text }) => (path.clone(), text.clone()),
                    _ => (String::new(), String::new()),
                };
                if let Some(v) = opt_str(p, "path") {
                    path = v.to_string();
                }
                if let Some(v) = opt_str(p, "text") {
                    text = v.to_string();
                }
                let bytes = if !text.trim().is_empty() {
                    text.into_bytes()
                } else if !path.trim().is_empty() {
                    std::fs::read(path.trim()).map_err(|e| CommandError::Failed(format!("{}: {e}", path.trim())))?
                } else {
                    return Err(CommandError::BadParams("`path` or `text` is required".into()));
                };
                let _ = s.save_all();
                s.vcs.dialog = None;
                vcs::simple_op(s, "Apply Patch", false, move |git| {
                    codecraft_vcs::ops::apply_patch(git, &bytes, false)
                })?;
                ok()
            },
        ),
        spec("vcs.addRemote", "Add Remote", None, "{name: string, url: string}", g, idle, |s, p| {
            let name = req_str(p, "name")?.trim().to_string();
            let url = req_str(p, "url")?.trim().to_string();
            if let Some(Dialog::Remotes { name: n, url: u }) = &mut s.vcs.dialog {
                n.clear();
                u.clear();
            }
            vcs::simple_op(s, "Add Remote", false, move |git| codecraft_vcs::ops::add_remote(git, &name, &url))?;
            ok()
        }),
        spec("vcs.removeRemote", "Remove Remote", None, "{name: string}", g, idle, |s, p| {
            let name = req_str(p, "name")?.to_string();
            vcs::simple_op(s, "Remove Remote", false, move |git| codecraft_vcs::ops::remove_remote(git, &name))?;
            ok()
        }),
        spec("vcs.setRemoteUrl", "Set Remote URL", None, "{name: string, url: string}", g, idle, |s, p| {
            let name = req_str(p, "name")?.to_string();
            let url = req_str(p, "url")?.to_string();
            vcs::simple_op(s, "Set Remote URL", false, move |git| {
                codecraft_vcs::ops::set_remote_url(git, &name, &url)
            })?;
            ok()
        }),
        spec(
            "vcs.clone",
            "Clone Repository",
            None,
            "{url?: string, directory?: path, open?: bool} — clone into `directory` (defaults from the Clone dialog)",
            g,
            always,
            |s, p| {
                let (mut url, mut dir) = match &s.vcs.dialog {
                    Some(Dialog::Clone { url, directory }) => (url.clone(), directory.clone()),
                    _ => (String::new(), String::new()),
                };
                if let Some(v) = opt_str(p, "url") {
                    url = v.to_string();
                }
                if let Some(v) = opt_str(p, "directory") {
                    dir = v.to_string();
                }
                let url = codecraft_vcs::github::normalize_clone_url(&url);
                if url.is_empty() || dir.trim().is_empty() {
                    s.vcs.dialog_error = Some("Enter a URL and a directory".into());
                    return Err(CommandError::BadParams("`url` and `directory` are required".into()));
                }
                let mut target = PathBuf::from(dir.trim());
                // A parent directory: clone into <dir>/<repo name>.
                if target.is_dir() && std::fs::read_dir(&target).map(|mut d| d.next().is_some()).unwrap_or(false) {
                    target = target.join(repo_name(&url));
                }
                vcs::clone_repo(s, url, target, bool_or(p, "open", true))?;
                ok()
            },
        ),
        // ----------------------------------------------------------- conflicts
        spec(
            "vcs.showConflicts",
            "Resolve Conflicts…",
            None,
            "Show the merge conflicts",
            g,
            has_conflicts,
            |s, _| {
                set_dialog(s, Dialog::Conflicts { selected: None });
                ok()
            },
        ),
        spec("vcs.acceptYours", "Accept Yours", None, "{path: string}", g, idle, |s, p| accept(s, p, Side::Ours)),
        spec("vcs.acceptTheirs", "Accept Theirs", None, "{path: string}", g, idle, |s, p| accept(s, p, Side::Theirs)),
        spec(
            "vcs.markResolved",
            "Mark Resolved",
            None,
            "{path: string} — stage the file as it is now",
            g,
            idle,
            |s, p| {
                let path = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
                let _ = s.save_all();
                vcs::simple_op(s, "Mark Resolved", false, move |git| codecraft_vcs::ops::mark_resolved(git, &path))?;
                ok()
            },
        ),
        spec(
            "vcs.mergeConflict",
            "Merge…",
            None,
            "{path: string} — open a conflicted file (its conflict markers are highlighted)",
            g,
            has_repo,
            |s, p| {
                let rel = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
                let abs = s.vcs.abs(&rel).ok_or_else(|| CommandError::Failed("no repository".into()))?;
                s.vcs.dialog = None;
                s.open_file(&abs)?;
                ok()
            },
        ),
        spec("vcs.abortOperation", "Abort Merge/Rebase", None, "", g, has_conflicts, |s, _| {
            let state = s.vcs.state;
            s.vcs.dialog = None;
            let label = format!("Abort {}", state.label());
            vcs::simple_op(s, &label, false, move |git| codecraft_vcs::ops::abort_operation(git, state))?;
            ok()
        }),
        spec("vcs.continueOperation", "Continue Merge/Rebase", None, "", g, has_conflicts, |s, _| {
            if !s.vcs.conflicts().is_empty() {
                return Err(CommandError::Failed("Resolve the remaining conflicts first".into()));
            }
            let state = s.vcs.state;
            s.vcs.dialog = None;
            let label = format!("Continue {}", state.label());
            vcs::simple_op(s, &label, true, move |git| codecraft_vcs::ops::continue_operation(git, state))?;
            ok()
        }),
        // ----------------------------------------------------------- log
        spec(
            "vcs.log",
            "Filter Git Log",
            None,
            "{text?: string, branch?: string|null, path?: string|null, author?: string} — set the Log tab's filters and \
             reload; → {commits: [{hash, subject, author, time, refs}]} of the first page once loaded",
            g,
            has_repo,
            |s, p| {
                if let Some(t) = opt_str(p, "text") {
                    s.vcs.log.text = t.to_string();
                }
                if let Some(a) = opt_str(p, "author") {
                    s.vcs.log.author = a.to_string();
                }
                if let Some(b) = p.get("branch") {
                    s.vcs.log.branch = b.as_str().filter(|b| !b.is_empty()).map(str::to_string);
                }
                if let Some(path) = p.get("path") {
                    s.vcs.log.path = path
                        .as_str()
                        .filter(|p| !p.is_empty())
                        .map(|x| rel_paths(s, vec![x.to_string()]).pop().unwrap_or_default());
                }
                vcs::show_git_window(s, GitTab::Log);
                vcs::load_log(s, true);
                Ok(json!({ "commits": s.vcs.log.commits.iter().take(50).map(|c| json!({
                    "hash": c.hash, "subject": c.subject, "author": c.author, "time": c.time,
                    "refs": c.refs.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
                })).collect::<Vec<_>>() }))
            },
        ),
        spec("vcs.log.more", "Load More Commits", None, "", g, has_repo, |s, _| {
            vcs::load_log(s, false);
            ok()
        }),
        spec(
            "vcs.selectCommit",
            "Show Commit",
            None,
            "{hash: string} — select a commit in the log and show its details",
            g,
            has_repo,
            |s, p| {
                let hash = req_str(p, "hash")?.to_string();
                vcs::select_commit(s, &hash);
                ok()
            },
        ),
        spec(
            "vcs.selectCommitFile",
            "Show Commit File Diff",
            None,
            "{path: string} — diff of a file in the selected commit",
            g,
            has_repo,
            |s, p| {
                vcs::select_commit_file(s, req_str(p, "path")?);
                ok()
            },
        ),
        spec("vcs.compare", "Compare File with Revision", None, "{path: string, rev: string}", g, has_repo, |s, p| {
            let rel = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
            let rev = req_str(p, "rev")?.to_string();
            s.vcs.dialog = None;
            vcs::compare_with(s, &rel, &rev);
            ok()
        }),
        spec(
            "vcs.diff",
            "Git Diff",
            None,
            "{path: string, staged?: bool} → {diff: string} (unified diff)",
            g,
            has_repo,
            |s, p| {
                let rel = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
                let git = s.vcs.git().ok_or_else(|| CommandError::Failed("no repository".into()))?;
                let diff = codecraft_vcs::ops::diff_text(&git, &rel, bool_or(p, "staged", false))
                    .map_err(|e| CommandError::Failed(e.to_string()))?;
                Ok(json!({ "diff": diff }))
            },
        ),
        // ----------------------------------------------------------- gutter
        spec(
            "vcs.showHunk",
            "Show Change",
            None,
            "{line?: int} — the change popup for the gutter marker at the caret line (or the given 0-based line)",
            Context::Global,
            file_in_repo,
            |s, p| {
                let (doc, line) = doc_line(s, p)?;
                vcs::show_hunk(s, doc, line)?;
                ok()
            },
        ),
        spec(
            "vcs.rollbackHunk",
            "Rollback Lines",
            Some("Alt+Cmd+Z"),
            "{line?: int} — roll back the change at the caret line (or the given 0-based line) to HEAD",
            Context::Editor,
            file_in_repo,
            |s, p| {
                let (doc, line) = match &s.vcs.hunk {
                    Some(h) if opt_usize(p, "line")?.is_none() => (h.doc, h.change.start),
                    _ => doc_line(s, p)?,
                };
                vcs::rollback_hunk(s, doc, line)?;
                ok()
            },
        ),
        spec(
            "vcs.nextChange",
            "Next Change",
            Some("Ctrl+Alt+Shift+Down"),
            "",
            Context::Editor,
            file_in_repo,
            |s, _| goto_change(s, true),
        ),
        spec(
            "vcs.previousChange",
            "Previous Change",
            Some("Ctrl+Alt+Shift+Up"),
            "",
            Context::Editor,
            file_in_repo,
            |s, _| goto_change(s, false),
        ),
        spec(
            "vcs.blame",
            "Git Blame",
            None,
            "{path?: string} → {lines: [{hash, author, time, summary}]}",
            g,
            has_repo,
            |s, p| {
                let rel = match opt_str(p, "path") {
                    Some(x) => rel_paths(s, vec![x.to_string()]).pop().unwrap_or_default(),
                    None => active_rel(s)?,
                };
                let git = s.vcs.git().ok_or_else(|| CommandError::Failed("no repository".into()))?;
                let b = codecraft_vcs::blame(&git, &rel, None).map_err(|e| CommandError::Failed(e.to_string()))?;
                let lines: Vec<Value> = (0..b.lines.len())
                    .filter_map(|i| b.line(i))
                    .map(|c| json!({ "hash": c.hash, "author": c.author, "time": c.time, "summary": c.summary }))
                    .collect();
                Ok(json!({ "lines": lines }))
            },
        ),
    ]
}

fn stash_dialog(s: &mut Session, _: &Value) -> CmdResult {
    let message = s.vcs.commit.message.lines().next().unwrap_or("").to_string();
    set_dialog(s, Dialog::Stash { message, include_untracked: false });
    ok()
}

fn set_amend(s: &mut Session, amend: bool) {
    s.vcs.commit.amend = amend;
    if amend
        && s.vcs.commit.message.trim().is_empty()
        && let Some(git) = s.vcs.git()
        && let Ok(msg) = git.read_text(["log", "-1", "--format=%B"])
    {
        s.vcs.commit.message = msg.trim_end().to_string();
    }
}

fn include(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(all) = p.get("all").and_then(Value::as_bool) {
        let tracked: Vec<String> = s.vcs.changes().into_iter().map(|f| f.path.clone()).collect();
        if all {
            s.vcs.commit.excluded.clear();
        } else {
            s.vcs.commit.excluded.extend(tracked);
            s.vcs.commit.included_untracked.clear();
        }
        return ok();
    }
    let path = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
    let included = bool_or(p, "included", true);
    let untracked = s.vcs.status.file(&path).is_some_and(|f: &FileStatus| f.untracked);
    if untracked {
        if included {
            s.vcs.commit.included_untracked.insert(path);
        } else {
            s.vcs.commit.included_untracked.remove(&path);
        }
    } else if included {
        s.vcs.commit.excluded.remove(&path);
    } else {
        s.vcs.commit.excluded.insert(path);
    }
    ok()
}

fn run_commit(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(m) = opt_str(p, "message") {
        s.vcs.commit.message = m.to_string();
    }
    if let Some(a) = p.get("amend").and_then(Value::as_bool) {
        s.vcs.commit.amend = a;
    }
    if let Some(a) = opt_str(p, "author") {
        s.vcs.commit.author = a.to_string();
    }
    if let Some(so) = p.get("signOff").and_then(Value::as_bool) {
        s.vcs.commit.sign_off = so;
    }
    // The commit check: analyze the files first when "Analyze code" is on (M14 track CA).
    if let Some(held) = crate::analysis::commit_check::before_commit(s, p)? {
        return Ok(held);
    }
    let files = str_list(p, "files").map(|f| rel_paths(s, f));
    vcs::commit(s, files, bool_or(p, "push", false))?;
    ok()
}

fn run_push(s: &mut Session, p: &Value) -> CmdResult {
    let mut req = match &s.vcs.dialog {
        Some(Dialog::Push { remote, branch, remote_branch, set_upstream, force, .. }) => PushRequest {
            remote: remote.clone(),
            branch: branch.clone(),
            remote_branch: Some(remote_branch.clone()),
            set_upstream: *set_upstream,
            force_with_lease: *force,
            tags: false,
        },
        _ => {
            let branch = s.vcs.branch_name().unwrap_or_default().to_string();
            match s.vcs.status.branch.upstream.as_deref().and_then(|u| u.split_once('/')) {
                Some((r, b)) => {
                    PushRequest { remote: r.into(), branch, remote_branch: Some(b.into()), ..PushRequest::default() }
                }
                None => PushRequest {
                    remote: s.vcs.remotes.first().map(|r| r.name.clone()).unwrap_or_default(),
                    remote_branch: Some(branch.clone()),
                    branch,
                    set_upstream: true,
                    ..PushRequest::default()
                },
            }
        }
    };
    if let Some(v) = opt_str(p, "remote") {
        req.remote = v.to_string();
    }
    if let Some(v) = opt_str(p, "branch") {
        req.branch = v.to_string();
    }
    if let Some(v) = opt_str(p, "remoteBranch") {
        req.remote_branch = Some(v.to_string());
    }
    req.set_upstream = bool_or(p, "setUpstream", req.set_upstream);
    req.force_with_lease = bool_or(p, "force", req.force_with_lease);
    if req.remote.is_empty() {
        s.vcs.dialog_error = Some("Add a remote first (Git › Manage Remotes…)".into());
        return Err(CommandError::Failed("the repository has no remote".into()));
    }
    vcs::push(s, req)?;
    ok()
}

fn branch_param(s: &Session, p: &Value) -> Result<String, CommandError> {
    if let Some(b) = opt_str(p, "branch") {
        return Ok(b.to_string());
    }
    match &s.vcs.dialog {
        Some(Dialog::ChooseBranch { branch, .. }) if !branch.is_empty() => Ok(branch.clone()),
        _ => Err(CommandError::BadParams("missing string param `branch`".into())),
    }
}

fn accept(s: &mut Session, p: &Value, side: Side) -> CmdResult {
    let path = rel_paths(s, vec![req_str(p, "path")?.to_string()]).pop().unwrap_or_default();
    let kind = s
        .vcs
        .status
        .file(&path)
        .and_then(|f| f.conflict)
        .ok_or_else(|| CommandError::Failed(format!("{path} has no conflict")))?;
    // The file will change on disk: drop unsaved edits so they don't overwrite the result.
    if let Some(abs) = s.vcs.abs(&path)
        && let Some(id) = s.ws.doc_by_path(&abs)
        && let Some(od) = s.ws.doc_mut(id)
    {
        od.doc.mark_saved();
    }
    let label = if side == Side::Ours { "Accept Yours" } else { "Accept Theirs" };
    vcs::simple_op(s, label, false, move |git| codecraft_vcs::ops::accept_side(git, &path, kind, side))?;
    ok()
}

fn doc_line(s: &Session, p: &Value) -> Result<(codecraft_workspace::DocId, usize), CommandError> {
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor".into()))?;
    let line = match opt_usize(p, "line")? {
        Some(l) => l,
        None => d.doc.char_to_line(e.selections.primary().head),
    };
    Ok((d.id, line))
}

fn goto_change(s: &mut Session, forward: bool) -> CmdResult {
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor".into()))?;
    let caret_line = d.doc.char_to_line(e.selections.primary().head);
    let path = d.path.clone().unwrap_or_default();
    let markers = s.vcs.markers(d.id);
    let target = if forward {
        markers.iter().map(|c| c.start).find(|l| *l > caret_line).or_else(|| markers.first().map(|c| c.start))
    } else {
        markers.iter().rev().map(|c| c.start).find(|l| *l < caret_line).or_else(|| markers.last().map(|c| c.start))
    };
    let Some(line) = target else { return Err(CommandError::Failed("no changes in this file".into())) };
    s.navigate_to_position(&path, line, 0)?;
    ok()
}

/// `https://host/org/name.git` → `name`.
fn repo_name(url: &str) -> String {
    let tail = url.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or("repository");
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    if name.is_empty() { "repository".into() } else { name.to_string() }
}

fn state_name(st: FileState) -> &'static str {
    match st {
        FileState::Modified => "modified",
        FileState::Added => "added",
        FileState::Deleted => "deleted",
        FileState::Renamed => "renamed",
        FileState::Untracked => "unversioned",
        FileState::Ignored => "ignored",
        FileState::Conflicted => "conflicted",
    }
}

fn status_json(s: &Session) -> Value {
    let v = &s.vcs;
    let files: Vec<Value> = v
        .status
        .files
        .iter()
        .filter(|f| !f.ignored)
        .map(|f| {
            json!({
                "path": f.path,
                "origPath": f.orig_path,
                "state": state_name(f.state()),
                "staged": f.staged(),
                "included": v.is_included(f),
            })
        })
        .collect();
    json!({
        "root": v.repo.as_ref().map(|r| r.root.clone()),
        "branch": v.status.branch.head,
        "head": v.status.branch.oid,
        "upstream": v.status.branch.upstream,
        "ahead": v.status.branch.ahead,
        "behind": v.status.branch.behind,
        "state": v.state.label(),
        "stashes": v.stashes.len(),
        "operation": v.op.as_ref().map(|o| o.label.clone()),
        "files": files,
    })
}

#[cfg(test)]
mod tests {
    use super::repo_name;

    #[test]
    fn repo_names_from_urls() {
        assert_eq!(repo_name("https://github.com/org/proj.git"), "proj");
        assert_eq!(repo_name("git@github.com:org/proj.git"), "proj");
        assert_eq!(repo_name("/srv/git/proj/"), "proj");
        assert_eq!(repo_name(""), "repository");
    }
}
