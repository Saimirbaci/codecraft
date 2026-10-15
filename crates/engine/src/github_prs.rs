//! GitHub through the `gh` CLI, part two: Git › GitHub › View Pull Requests (the Git tool
//! window's Pull Requests tab: list with state/author/label filters, details, changed files with
//! their diff, read-only review comments, checkout, merge with a method), Share Project on
//! GitHub (`gh repo create --push`), Sync Fork (`gh repo sync`) and Create Gist.
//!
//! Every `gh` call runs on a worker thread: reads go through this module's channel (drained by
//! [`pump`], called from the VCS pump), actions run as the repository operation
//! ([`crate::vcs::start_gh_op`]). Commands that talk to GitHub run only on an explicit action
//! (a menu item, `fetch: true`, a number), never from defaults alone.

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde_json::{Value, json};

use codecraft_vcs::GitError;
use codecraft_vcs::github::{
    GH_INSTALL_HINT, Gh, MergeMethod, PrFilter, PullRequestDetails, PullRequestItem, ReviewComment, ShareRequest,
};
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{bool_or, opt_str, opt_usize};
use crate::vcs::{Dialog, GitTab};
use crate::{CommandError, Effect, Session};

const LIST_LIMIT: usize = 100;

enum Event {
    List { generation: u64, result: Result<Vec<PullRequestItem>, GitError> },
    Details { number: u64, result: Result<Box<PullRequestDetails>, GitError> },
    Diff { number: u64, result: Result<String, GitError> },
    Comments { number: u64, result: Result<Vec<ReviewComment>, GitError> },
}

/// The Pull Requests tab.
pub struct PrState {
    pub filter: PrFilter,
    pub list: Vec<PullRequestItem>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub selected: Option<u64>,
    pub details: Option<PullRequestDetails>,
    pub details_loading: bool,
    pub details_error: Option<String>,
    /// (path, patch) of each changed file (`gh pr diff`).
    pub diff: Vec<(String, String)>,
    pub diff_loading: bool,
    pub comments: Vec<ReviewComment>,
    pub comments_loading: bool,
    pub comments_error: Option<String>,
    pub selected_file: Option<String>,
    /// The merge form.
    pub merge_method: MergeMethod,
    pub delete_branch: bool,
    generation: u64,
    tx: Sender<Event>,
    rx: Receiver<Event>,
}

impl Default for PrState {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            filter: PrFilter::default(),
            list: Vec::new(),
            loading: false,
            loaded: false,
            error: None,
            selected: None,
            details: None,
            details_loading: false,
            details_error: None,
            diff: Vec::new(),
            diff_loading: false,
            comments: Vec::new(),
            comments_loading: false,
            comments_error: None,
            selected_file: None,
            merge_method: MergeMethod::Merge,
            delete_branch: false,
            generation: 0,
            tx,
            rx,
        }
    }
}

impl PrState {
    pub fn busy(&self) -> bool {
        self.loading || self.details_loading || self.diff_loading || self.comments_loading
    }

    /// The patch of the selected file.
    pub fn file_patch(&self) -> Option<&str> {
        let f = self.selected_file.as_deref()?;
        self.diff.iter().find(|(p, _)| p == f).map(|(_, t)| t.as_str())
    }

    /// Review comments on one file.
    pub fn comments_on<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a ReviewComment> + 'a {
        self.comments.iter().filter(move |c| c.path == path)
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(format!("gh-{name}")).spawn(f) {
        log::error!("cannot start a gh worker: {e}");
    }
}

fn err_text(e: &GitError) -> String {
    e.to_string()
}

/// Drain finished `gh` reads; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    while let Ok(ev) = s.vcs.prs.rx.try_recv() {
        changed = true;
        let p = &mut s.vcs.prs;
        match ev {
            Event::List { generation, result } => {
                if generation != p.generation {
                    continue;
                }
                p.loading = false;
                p.loaded = true;
                match result {
                    Ok(list) => {
                        p.error = None;
                        p.list = list;
                    }
                    Err(e) => p.error = Some(err_text(&e)),
                }
            }
            Event::Details { number, result } => {
                if p.selected != Some(number) {
                    continue;
                }
                p.details_loading = false;
                match result {
                    Ok(d) => {
                        p.details_error = None;
                        p.details = Some(*d);
                    }
                    Err(e) => p.details_error = Some(err_text(&e)),
                }
            }
            Event::Diff { number, result } => {
                if p.selected != Some(number) {
                    continue;
                }
                p.diff_loading = false;
                match result {
                    Ok(text) => {
                        p.diff = codecraft_vcs::github::split_diff(&text);
                        if p.selected_file.is_none() {
                            p.selected_file = p.diff.first().map(|(f, _)| f.clone());
                        }
                    }
                    Err(e) => p.details_error = Some(err_text(&e)),
                }
            }
            Event::Comments { number, result } => {
                if p.selected != Some(number) {
                    continue;
                }
                p.comments_loading = false;
                match result {
                    Ok(c) => {
                        p.comments_error = None;
                        p.comments = c;
                    }
                    Err(e) => p.comments_error = Some(err_text(&e)),
                }
            }
        }
    }
    changed
}

/// A `gh` in the repository root (or the project root).
fn gh(s: &Session) -> Result<Gh, CommandError> {
    let program = s.vcs.gh_path().ok_or_else(|| CommandError::Failed(GH_INSTALL_HINT.into()))?;
    let dir = s
        .vcs
        .repo
        .as_ref()
        .map(|r| r.root.clone())
        .or_else(|| s.project_root())
        .ok_or_else(|| CommandError::Failed("open a project first".into()))?;
    Ok(s.vcs.gh_in(program, &dir))
}

/// (Re)load the list with the current filters.
pub fn load_list(s: &mut Session) -> Result<(), CommandError> {
    let gh = gh(s)?;
    let p = &mut s.vcs.prs;
    p.generation = p.generation.wrapping_add(1);
    p.loading = true;
    let (generation, filter, tx) = (p.generation, p.filter.clone(), p.tx.clone());
    spawn("pr-list", move || {
        let result = codecraft_vcs::github::list_pull_requests(&gh, &filter, LIST_LIMIT);
        let _ = tx.send(Event::List { generation, result });
    });
    Ok(())
}

/// Reload what the tab shows after an action (list and the selected pull request).
pub(crate) fn reload(s: &mut Session) {
    if !s.vcs.prs.loaded {
        return;
    }
    if let Err(e) = load_list(s) {
        s.vcs.prs.error = Some(e.to_string());
    }
    if let Some(n) = s.vcs.prs.selected
        && let Err(e) = select(s, n)
    {
        s.vcs.prs.details_error = Some(e.to_string());
    }
}

/// Select a pull request: load its details, diff and review comments.
pub fn select(s: &mut Session, number: u64) -> Result<(), CommandError> {
    let gh = gh(s)?;
    let p = &mut s.vcs.prs;
    if p.selected != Some(number) {
        p.details = None;
        p.diff.clear();
        p.comments.clear();
        p.selected_file = None;
    }
    p.selected = Some(number);
    p.details_loading = true;
    p.diff_loading = true;
    p.comments_loading = true;
    p.details_error = None;
    p.comments_error = None;
    for what in 0..3u8 {
        let (gh, tx) = (gh.clone(), p.tx.clone());
        spawn("pr", move || {
            let _ = match what {
                0 => tx.send(Event::Details {
                    number,
                    result: codecraft_vcs::github::pull_request(&gh, number).map(Box::new),
                }),
                1 => tx.send(Event::Diff { number, result: codecraft_vcs::github::pull_request_diff(&gh, number) }),
                _ => tx.send(Event::Comments { number, result: codecraft_vcs::github::review_comments(&gh, number) }),
            };
        });
    }
    Ok(())
}

fn number_param(s: &Session, p: &Value) -> Result<u64, CommandError> {
    match opt_usize(p, "number")? {
        Some(0) => Err(CommandError::BadParams("`number` must be a pull request number".into())),
        Some(n) => Ok(n as u64),
        None => {
            s.vcs.prs.selected.ok_or_else(|| CommandError::BadParams("select a pull request (or pass `number`)".into()))
        }
    }
}

fn show_tab(s: &mut Session) -> CmdResult {
    s.vcs.git_tab = GitTab::PullRequests;
    s.ws.layout.show(ToolWindowId::Git);
    if !s.vcs.prs.loaded
        && !s.vcs.prs.loading
        && let Err(e) = load_list(s)
    {
        s.vcs.prs.error = Some(e.to_string());
    }
    ok()
}

fn state_json(s: &Session) -> Value {
    let p = &s.vcs.prs;
    json!({
        "filter": p.filter,
        "loading": p.loading,
        "loaded": p.loaded,
        "error": p.error,
        "pullRequests": p.list,
        "selected": p.selected,
        "details": p.details,
        "detailsLoading": p.details_loading,
        "detailsError": p.details_error,
        "files": p.diff.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
        "selectedFile": p.selected_file,
        "patch": p.file_patch(),
        "comments": p.comments,
        "commentsError": p.comments_error,
        "mergeMethod": p.merge_method,
        "deleteBranch": p.delete_branch,
    })
}

fn github_repo(s: &Session) -> Result<(), String> {
    if s.vcs.repo.is_none() {
        return Err(s.vcs.unavailable.clone().unwrap_or_else(|| "the project is not a Git repository".into()));
    }
    if s.vcs.github_remote().is_none() {
        return Err("the repository has no GitHub remote".into());
    }
    Ok(())
}

fn github_idle(s: &Session) -> Result<(), String> {
    github_repo(s)?;
    match &s.vcs.op {
        Some(op) => Err(format!("“{}” is running", op.label)),
        None => Ok(()),
    }
}

fn can_share(s: &Session) -> Result<(), String> {
    if s.ws.project.is_none() {
        return Err("open a project first".into());
    }
    if let Some(op) = &s.vcs.op {
        return Err(format!("“{}” is running", op.label));
    }
    if s.vcs.github_remote().is_some() {
        return Err("the project is already on GitHub".into());
    }
    Ok(())
}

fn file_open(s: &Session) -> Result<(), String> {
    let (_, d) = s.ws.active().ok_or("no editor is open")?;
    d.path.as_ref().map(|_| ()).ok_or_else(|| "the file is not saved".into())
}

fn set_dialog(s: &mut Session, d: Dialog) {
    s.vcs.dialog = Some(d);
    s.vcs.dialog_error = None;
    s.vcs.branches_popup = false;
}

/// Share Project on GitHub: make the project a repository with a first commit if needed, then
/// `gh repo create --source --push`.
fn share(s: &mut Session, req: ShareRequest) -> Result<(), CommandError> {
    let program = s.vcs.gh_path().ok_or_else(|| CommandError::Failed(GH_INSTALL_HINT.into()))?;
    let root = match &s.vcs.repo {
        Some(r) => r.root.clone(),
        None => s.project_root().ok_or_else(|| CommandError::Failed("open a project first".into()))?,
    };
    if !codecraft_vcs::github::valid_repo_name(req.name.trim()) {
        return Err(CommandError::BadParams(format!("'{}' is not a valid repository name", req.name.trim())));
    }
    let gh = s.vcs.gh_in(program, &root);
    let is_repo = s.vcs.repo.is_some();
    let has_head = s.vcs.status.branch.oid.is_some();
    let _ = s.save_all();
    crate::vcs::start_share_op(s, root.clone(), move |git| {
        if !is_repo {
            git.run(["init", "-q"])?;
        }
        if !has_head {
            git.run(["add", "-A"])?;
            git.run(["commit", "-q", "--allow-empty", "-m", "Initial commit"])?;
        }
        codecraft_vcs::github::create_repo(&gh, &root, &req)
    })
}

/// Sync Fork: update the fork's default branch on GitHub from its parent, then fetch it.
fn sync_fork(s: &mut Session) -> Result<(), CommandError> {
    let gh = gh(s)?;
    let remote = s
        .vcs
        .github_remote()
        .cloned()
        .ok_or_else(|| CommandError::Failed("the repository has no GitHub remote".into()))?;
    let url = if remote.fetch_url.is_empty() { remote.push_url.clone() } else { remote.fetch_url.clone() };
    let repo = codecraft_vcs::github::repo_from_url(&url)
        .ok_or_else(|| CommandError::Failed(format!("cannot tell the GitHub repository of {url}")))?;
    let name = remote.name.clone();
    crate::vcs::start_gh_op(s, "Sync Fork", false, false, move |git| {
        let out = codecraft_vcs::github::sync_fork(&gh, &repo)?;
        let out = out.trim();
        let synced = if out.is_empty() { format!("{repo} synced with its parent") } else { out.to_string() };
        // The fork is synced on GitHub either way; a failed fetch is reported, not fatal.
        Ok(match codecraft_vcs::ops::fetch(git, Some(&name)) {
            Ok(_) => format!("{synced}\nFetched {name}"),
            Err(GitError::Cancelled) => return Err(GitError::Cancelled),
            Err(e) => format!("{synced}\nFetching {name} failed: {e}"),
        })
    })
}

fn create_gist(
    s: &mut Session,
    path: PathBuf,
    description: String,
    public: bool,
    open: bool,
) -> Result<(), CommandError> {
    let gh = gh(s)?;
    if !path.is_file() {
        return Err(CommandError::BadParams(format!("{} is not a file", path.display())));
    }
    let _ = s.save_all();
    crate::vcs::start_gh_op(s, "Create Gist", false, open, move |_| {
        codecraft_vcs::github::create_gist(&gh, &path, &description, public)
    })
}

fn project_name(s: &Session) -> String {
    s.project_root()
        .and_then(|r| r.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|n| n.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect())
        .unwrap_or_default()
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        spec(
            "git.gitHub.viewPullRequests",
            "View Pull Requests",
            None,
            "Show the Pull Requests tab (lists the open pull requests through `gh`)",
            g,
            github_repo,
            |s, _| show_tab(s),
        ),
        spec(
            "view.toolWindows.pullRequests",
            "Pull Requests",
            None,
            "Show the Pull Requests tab of the Git tool window",
            g,
            github_repo,
            |s, _| show_tab(s),
        ),
        spec(
            "vcs.pr.list",
            "Pull Requests",
            None,
            "{state?: \"open\"|\"closed\"|\"merged\"|\"all\", author?: string, label?: string, search?: string, fetch?: bool} → the tab's state (filters are set; fetch: true lists through `gh` in the background)",
            g,
            github_repo,
            |s, p| {
                let f = &mut s.vcs.prs.filter;
                for (key, slot) in [
                    ("state", &mut f.state),
                    ("author", &mut f.author),
                    ("label", &mut f.label),
                    ("search", &mut f.search),
                ] {
                    if let Some(v) = opt_str(p, key) {
                        *slot = v.trim().to_string();
                    }
                }
                if !matches!(f.state.as_str(), "open" | "closed" | "merged" | "all") {
                    let bad = std::mem::replace(&mut f.state, "open".into());
                    return Err(CommandError::BadParams(format!("unknown state '{bad}' (open, closed, merged, all)")));
                }
                if bool_or(p, "fetch", false) {
                    load_list(s)?;
                }
                Ok(state_json(s))
            },
        ),
        spec(
            "vcs.pr.select",
            "Select Pull Request",
            None,
            "{number: int} — show a pull request: details, changed files with their diff and review comments (loaded through `gh`)",
            g,
            github_repo,
            |s, p| {
                let n = opt_usize(p, "number")?
                    .filter(|n| *n > 0)
                    .ok_or_else(|| CommandError::BadParams("`number` (int) is required".into()))?;
                select(s, n as u64)?;
                ok()
            },
        ),
        spec(
            "vcs.pr.selectFile",
            "Select Pull Request File",
            None,
            "{path: string} — show one changed file's diff and its review comments",
            g,
            github_repo,
            |s, p| {
                let path = opt_str(p, "path").ok_or_else(|| CommandError::BadParams("`path` is required".into()))?;
                if !s.vcs.prs.diff.iter().any(|(f, _)| f == path) {
                    return Err(CommandError::BadParams(format!("{path} is not changed in this pull request")));
                }
                s.vcs.prs.selected_file = Some(path.to_string());
                ok()
            },
        ),
        spec(
            "vcs.pr.state",
            "Pull Request State",
            None,
            "→ filters, list, selected details, files, patch, comments",
            g,
            always,
            |s, _| Ok(state_json(s)),
        ),
        spec(
            "vcs.pr.checkout",
            "Checkout Pull Request",
            None,
            "{number?: int} — `gh pr checkout`: a local branch for the pull request (default: the selected one)",
            g,
            github_idle,
            |s, p| {
                let n = number_param(s, p)?;
                let gh = gh(s)?;
                crate::vcs::start_gh_op(s, &format!("Checkout #{n}"), false, false, move |_| {
                    let out = codecraft_vcs::github::checkout_pull_request(&gh, n)?;
                    let out = out.trim();
                    Ok(if out.is_empty() { format!("Checked out pull request #{n}") } else { out.to_string() })
                })?;
                ok()
            },
        ),
        spec(
            "vcs.pr.merge",
            "Merge Pull Request",
            None,
            "{number?: int, method?: \"merge\"|\"squash\"|\"rebase\", deleteBranch?: bool} — `gh pr merge` (defaults: the selected one and the tab's merge form)",
            g,
            github_idle,
            |s, p| {
                let n = number_param(s, p)?;
                let method = match opt_str(p, "method") {
                    Some(m) => MergeMethod::parse(m).ok_or_else(|| {
                        CommandError::BadParams(format!("unknown method '{m}' (merge, squash, rebase)"))
                    })?,
                    None => s.vcs.prs.merge_method,
                };
                let delete = bool_or(p, "deleteBranch", s.vcs.prs.delete_branch);
                s.vcs.prs.merge_method = method;
                s.vcs.prs.delete_branch = delete;
                let gh = gh(s)?;
                crate::vcs::start_gh_op(s, &format!("Merge #{n}"), true, false, move |_| {
                    let out = codecraft_vcs::github::merge_pull_request(&gh, n, method, delete)?;
                    let out = out.trim();
                    Ok(if out.is_empty() {
                        format!("Pull request #{n} merged ({})", method.label())
                    } else {
                        out.to_string()
                    })
                })?;
                ok()
            },
        ),
        spec("vcs.pr.openInBrowser", "Open Pull Request in Browser", None, "{number?: int}", g, github_repo, |s, p| {
            let n = number_param(s, p)?;
            let url = s
                .vcs
                .prs
                .list
                .iter()
                .find(|pr| pr.number == n)
                .map(|pr| pr.url.clone())
                .or_else(|| s.vcs.prs.details.as_ref().filter(|d| d.number == n).map(|d| d.url.clone()))
                .filter(|u| u.starts_with("https://"))
                .ok_or_else(|| CommandError::Failed(format!("the URL of #{n} is not loaded")))?;
            s.effects.push(Effect::OpenUrl(url.clone()));
            Ok(json!({ "url": url }))
        }),
        // ----------------------------------------------------------- share, fork, gist
        spec(
            "git.gitHub.shareProjectOnGitHub",
            "Share Project on GitHub",
            None,
            "Open the Share Project on GitHub dialog",
            g,
            can_share,
            |s, _| {
                let name = project_name(s);
                set_dialog(
                    s,
                    Dialog::ShareOnGitHub { name, description: String::new(), private: true, remote: "origin".into() },
                );
                if s.vcs.gh_path().is_none() {
                    s.vcs.dialog_error = Some(GH_INSTALL_HINT.into());
                }
                ok()
            },
        ),
        spec(
            "vcs.github.share",
            "Share on GitHub",
            None,
            "{name: string, description?: string, private?: bool (default true), remote?: string} — create the GitHub repository with `gh repo create` and push (a project that isn't a repository yet is initialised with a first commit)",
            g,
            can_share,
            |s, p| {
                let req = ShareRequest {
                    name: opt_str(p, "name")
                        .ok_or_else(|| CommandError::BadParams("`name` is required".into()))?
                        .trim()
                        .to_string(),
                    description: opt_str(p, "description").unwrap_or("").to_string(),
                    private: bool_or(p, "private", true),
                    remote: opt_str(p, "remote").unwrap_or("origin").to_string(),
                };
                share(s, req).inspect_err(|e| s.vcs.dialog_error = Some(e.to_string()))?;
                ok()
            },
        ),
        spec(
            "git.gitHub.syncFork",
            "Sync Fork",
            None,
            "Update the fork's default branch on GitHub from its parent (`gh repo sync`), then fetch",
            g,
            github_idle,
            |s, _| {
                sync_fork(s)?;
                ok()
            },
        ),
        spec(
            "git.gitHub.createGist",
            "Create Gist…",
            None,
            "Open the Create Gist dialog for the current file",
            g,
            file_open,
            |s, _| {
                let path = s.ws.active().and_then(|(_, d)| d.path.clone()).unwrap_or_default();
                set_dialog(
                    s,
                    Dialog::CreateGist { path: path.display().to_string(), description: String::new(), public: false },
                );
                if s.vcs.gh_path().is_none() {
                    s.vcs.dialog_error = Some(GH_INSTALL_HINT.into());
                }
                ok()
            },
        ),
        spec(
            "vcs.github.createGist",
            "Create Gist",
            None,
            "{path: path, description?: string, public?: bool, open?: bool} — `gh gist create` (secret unless public)",
            g,
            always,
            |s, p| {
                let path = crate::params::req_path(p, "path")?;
                let path = if path.is_absolute() { path } else { s.project_root().unwrap_or_default().join(path) };
                create_gist(
                    s,
                    path,
                    opt_str(p, "description").unwrap_or("").to_string(),
                    bool_or(p, "public", false),
                    bool_or(p, "open", false),
                )
                .inspect_err(|e| s.vcs.dialog_error = Some(e.to_string()))?;
                ok()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn without_a_github_repository_nothing_runs() {
        let mut s = Session::new();
        s.vcs.gh_program = PathBuf::from("/nonexistent/gh");
        assert!(s.execute("git.gitHub.viewPullRequests", &Value::Null).is_err());
        assert!(s.execute("vcs.pr.list", &json!({ "fetch": true })).is_err());
        assert!(s.execute("git.gitHub.shareProjectOnGitHub", &Value::Null).is_err(), "no project");
        assert!(s.execute("git.gitHub.createGist", &Value::Null).is_err());
        assert!(s.execute("vcs.github.createGist", &json!({ "path": "/nonexistent/x" })).is_err());
        let st = s.execute("vcs.pr.state", &Value::Null).unwrap();
        assert_eq!(st["filter"]["state"], "open");
        assert!(!s.vcs.prs.busy());
    }
}
