//! Version control state: the project's Git repository, its status and change list, gutter line
//! markers for open documents, the log, blame, stashes, conflicts and the operations console.
//! OWNED BY the Git track.
//!
//! Every git command runs on a worker thread (`codecraft_vcs` blocks); results come back over one
//! channel that [`pump`] drains each frame. Status is refreshed when the file watcher reports a
//! change (including `.git/HEAD`, `.git/index` and refs) and after every operation; there is no
//! periodic polling. Operations that change the repository run one at a time, echo their
//! commands and output to the console, and can be cancelled ([`cancel`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};

use codecraft_vcs::github::{GhRepo, PullRequestRequest};
use codecraft_vcs::lines::{self, LineChange};
use codecraft_vcs::worktree::Worktree;
use codecraft_vcs::ops::{PullRequest, PushRequest, ResetMode};
use codecraft_vcs::{
    Blame, Branch, CancelToken, Commit, CommitDetails, ConsoleLine, FileState, FileStatus, Git, GitError, GraphRow,
    LogQuery, Remote, RepoInfo, RepoState, RepoStatus, Stash, StatusOptions, Tag,
};
use codecraft_workspace::{BackgroundTask, DocId, NotificationLevel, ToolWindowId};

use crate::{CommandError, Effect, Session};

pub mod changelists;
pub mod shelf;

/// Commits per log page.
pub const LOG_PAGE: usize = 200;
/// Console lines kept.
const MAX_CONSOLE: usize = 5000;
/// Gutter markers are recomputed once typing has paused this long.
const MARKER_DEBOUNCE: Duration = Duration::from_millis(150);
/// Annotations are recomputed once edits have paused this long.
const BLAME_DEBOUNCE: Duration = Duration::from_millis(800);
/// Files larger than this get no gutter markers.
const MAX_MARKER_BYTES: usize = 4 << 20;
const TASK_OP: &str = "vcs-op";

// ---------------------------------------------------------------- public view state

/// One line of the Git tool window's Console tab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub kind: ConsoleKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleKind {
    /// `git …` as it was run.
    Command,
    Output,
    /// The operation failed (git's message).
    Error,
    /// Notes from CodeCraft (operation finished, cancelled).
    Info,
}

/// Tabs of the Git tool window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitTab {
    #[default]
    Log,
    Console,
    /// Git › GitHub › View Pull Requests.
    PullRequests,
}

/// Tabs of the Commit tool window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitTab {
    #[default]
    Changes,
    Stashes,
    /// Shelved changes (`.codecraft/shelf`).
    Shelf,
}

/// The commit message box and which changes are included (checkboxes).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitForm {
    pub message: String,
    pub amend: bool,
    pub sign_off: bool,
    /// `Name <email>`; empty = the configured identity.
    pub author: String,
    /// Tracked changes the user unchecked (all are included by default).
    pub excluded: BTreeSet<String>,
    /// Unversioned files the user checked (none are by default).
    pub included_untracked: BTreeSet<String>,
    /// The change selected in the list (its diff is previewed).
    pub selected: Option<String>,
}

/// Old and new text of one file, for a diff view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub old_title: String,
    pub new_title: String,
    pub old: String,
    pub new: String,
    /// Binary or too large: no text diff.
    pub note: Option<String>,
    pub loading: bool,
}

/// The Git tool window's Log tab.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogView {
    /// Text filter (message, or a hash prefix).
    pub text: String,
    /// Branch filter (`None` = all branches).
    pub branch: Option<String>,
    /// Path filter (repository-relative), set by Show History.
    pub path: Option<String>,
    pub author: String,
    pub commits: Vec<Commit>,
    pub graph: Vec<GraphRow>,
    pub loading: bool,
    pub has_more: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub selected: Option<String>,
    pub details: Option<CommitDetails>,
    pub details_loading: bool,
    pub selected_file: Option<String>,
    pub file_diff: Option<FileDiff>,
    #[serde(skip)]
    generation: u64,
}

impl LogView {
    fn query(&self, skip: usize) -> LogQuery {
        LogQuery {
            skip,
            limit: LOG_PAGE,
            branch: self.branch.clone(),
            path: self.path.clone(),
            text: Some(self.text.clone()).filter(|t| !t.trim().is_empty()),
            author: Some(self.author.clone()).filter(|t| !t.trim().is_empty()),
        }
    }
}

/// Annotations (blame) shown for a document.
#[derive(Clone, Debug, Default)]
pub struct BlameView {
    pub blame: Option<Arc<Blame>>,
    pub error: Option<String>,
    pub loading: bool,
    revision: u64,
    seen_revision: u64,
    seen_at: Option<Instant>,
}

/// The popup opened by clicking a gutter change marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkPopup {
    pub doc: DocId,
    pub change: LineChange,
    /// The lines the change replaced (empty for an addition).
    pub old_text: String,
    /// The current lines.
    pub new_text: String,
    /// Whether the change is in the index (`None` while loading or unknown).
    #[serde(default)]
    pub stage: Option<codecraft_vcs::hunks::StageState>,
    #[serde(default)]
    pub stage_loading: bool,
}

/// Forms of the Git dialogs. The UI edits their fields in place and confirms with the matching
/// `vcs.*` command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dialog {
    Push {
        remote: String,
        branch: String,
        remote_branch: String,
        set_upstream: bool,
        force: bool,
        /// Commits to push (`None` while loading).
        commits: Option<Vec<Commit>>,
    },
    Pull {
        remote: String,
        branch: String,
        rebase: bool,
    },
    NewBranch {
        name: String,
        from: Option<String>,
        checkout: bool,
    },
    RenameBranch {
        old: String,
        name: String,
    },
    DeleteBranch {
        name: String,
        remote: bool,
        force: bool,
    },
    NewTag {
        name: String,
        rev: String,
        message: String,
    },
    Reset {
        rev: String,
        mode: ResetMode,
    },
    /// Merge a branch into the current one, or rebase onto it.
    ChooseBranch {
        rebase: bool,
        branch: String,
    },
    Stash {
        message: String,
        include_untracked: bool,
    },
    Unstash {
        selected: Option<usize>,
        pop: bool,
        reinstate_index: bool,
    },
    Rollback {
        files: Vec<String>,
        delete_added: bool,
    },
    CreatePatch {
        path: String,
        files: Vec<String>,
        to_clipboard: bool,
    },
    ApplyPatch {
        path: String,
        text: String,
    },
    Remotes {
        name: String,
        url: String,
    },
    Clone {
        url: String,
        directory: String,
    },
    Conflicts {
        selected: Option<String>,
    },
    /// Pick a revision or branch to compare the current file with.
    Compare {
        path: String,
        rev: String,
        branch: bool,
    },
    /// Git › New Worktree…
    NewWorktree {
        path: String,
        branch: String,
        new_branch: bool,
        /// Start point of a new branch (empty: `HEAD`).
        base: String,
    },
    /// Git › Worktrees…: the list is in [`VcsState::worktrees`].
    Worktrees {
        selected: Option<PathBuf>,
    },
    /// Git › GitHub › Create Pull Request…
    CreatePullRequest {
        title: String,
        body: String,
        /// Target branch (empty: the repository's default branch).
        base: String,
        draft: bool,
    },
    /// Git › Uncommitted Changes › Shelve Changes…
    Shelve {
        name: String,
        files: Vec<String>,
        /// Keep the changes in the work tree too.
        keep: bool,
    },
    /// New Changelist… (`original: None`) or Edit Changelist….
    ChangeList {
        original: Option<String>,
        name: String,
        comment: String,
        active: bool,
        /// Files to move into a new list.
        files: Vec<String>,
    },
    /// Git › GitHub › Share Project on GitHub.
    ShareOnGitHub {
        name: String,
        description: String,
        private: bool,
        remote: String,
    },
    /// Git › GitHub › Create Gist….
    CreateGist {
        path: String,
        description: String,
        public: bool,
    },
}

/// The operation running now.
#[derive(Clone, Debug)]
pub struct RunningOp {
    pub label: String,
    pub started: Instant,
    cancel: CancelToken,
}

// ---------------------------------------------------------------- internal

#[derive(Clone, Debug)]
enum OpKind {
    Plain,
    Commit {
        push_after: bool,
    },
    Clone {
        target: PathBuf,
        open: bool,
    },
    /// May leave conflicts: show them when it ends.
    MayConflict,
    Patch {
        clipboard: bool,
    },
    /// A work tree was added or removed: reload the list (and open the new one).
    Worktrees {
        open: Option<PathBuf>,
    },
    /// The summary is the new pull request's URL.
    PullRequest {
        open: bool,
    },
    /// A hunk was staged or unstaged: close its popup.
    Hunk,
    /// Shelve / unshelve / delete: reload the shelf (unshelving may leave conflicts).
    Shelf,
    /// Share Project on GitHub created the repository: discover it again.
    Share {
        root: PathBuf,
    },
    /// A `gh` action: reload the pull requests, and open the URL it printed.
    Gh {
        reload_prs: bool,
        open_url: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum HeadText {
    Loading,
    /// `None`: not in `HEAD`, binary or too large.
    Ready(Option<Arc<String>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Base {
    /// No markers (untracked, ignored, outside the repository).
    None,
    /// New in the index: everything is added.
    Empty,
    /// Compare with `HEAD`.
    Head,
}

#[derive(Clone, Debug, Default)]
struct Markers {
    changes: Arc<Vec<LineChange>>,
    /// Document revision and base the changes were computed for.
    revision: Option<u64>,
    key: Option<(Option<String>, String)>,
    pending: bool,
    seen_revision: u64,
    seen_at: Option<Instant>,
}

/// A conflicted file's (base, ours, theirs) versions.
type MergeVersions = (Option<String>, Option<String>, Option<String>);

/// Branches, tags, remotes and stashes.
type Refs = (Vec<Branch>, Vec<Tag>, Vec<Remote>, Vec<Stash>);

struct Snapshot {
    status: RepoStatus,
    state: RepoState,
    refs: Option<Refs>,
}

enum Event {
    Discovered { root: PathBuf, result: Result<Option<RepoInfo>, GitError> },
    Refreshed(Result<Snapshot, GitError>),
    Head { oid: Option<String>, rel: String, text: Option<Arc<String>> },
    Markers { doc: DocId, revision: u64, key: (Option<String>, String), changes: Vec<LineChange> },
    LogPage { generation: u64, skip: usize, result: Result<Vec<Commit>, GitError> },
    Details { hash: String, result: Result<CommitDetails, GitError> },
    CommitFile { hash: String, result: FileDiff },
    Preview(FileDiff),
    Compare(FileDiff),
    Outgoing { branch: String, result: Result<Vec<Commit>, GitError> },
    Blame { doc: DocId, revision: u64, result: Result<Blame, GitError> },
    OpDone { label: String, kind: OpKind, result: Result<String, GitError> },
    Worktrees(Result<Vec<Worktree>, GitError>),
    GhRepos(Result<Vec<GhRepo>, GitError>),
    MergeLoaded { rel: String, labels: (String, String), result: Result<MergeVersions, GitError> },
    Generated(Result<String, String>),
    Shelves(Vec<codecraft_vcs::shelf::Shelf>),
    ShelfPatch { id: String, path: String, result: Result<String, GitError> },
    HunkStage { doc: DocId, start: usize, state: Option<codecraft_vcs::hunks::StageState> },
}

pub struct VcsState {
    /// The project's repository (`None`: no project, not a repository, or git is missing).
    pub repo: Option<RepoInfo>,
    /// Why there is no repository when that is surprising (git missing, …).
    pub unavailable: Option<String>,
    pub status: RepoStatus,
    pub state: RepoState,
    pub branches: Vec<Branch>,
    pub tags: Vec<Tag>,
    pub remotes: Vec<Remote>,
    pub stashes: Vec<Stash>,
    pub log: LogView,
    pub console: Vec<ConsoleEntry>,
    pub commit: CommitForm,
    /// Diff of the change selected in the Commit tool window.
    pub preview: Option<FileDiff>,
    /// A diff window (Compare with Revision/Branch, Show Diff).
    pub compare: Option<FileDiff>,
    pub dialog: Option<Dialog>,
    /// A message for the open dialog (validation or a failed operation).
    pub dialog_error: Option<String>,
    pub branches_popup: bool,
    pub hunk: Option<HunkPopup>,
    pub git_tab: GitTab,
    pub commit_tab: CommitTab,
    /// The UI should focus the commit message box (Cmd+K).
    pub focus_message: bool,
    /// A commit message is being generated by a language model.
    pub generating: bool,
    pub op: Option<RunningOp>,
    /// Annotations per document (Git › Current File › Annotate).
    pub blame: HashMap<DocId, BlameView>,
    /// The three-way merge view of a conflicted file.
    pub merge: Option<crate::vcs_merge::MergeView>,
    /// The file whose merge view is loading.
    pub merge_loading: Option<String>,
    /// Linked work trees (Git › Worktrees…), the main one first.
    pub worktrees: Vec<Worktree>,
    pub worktrees_loading: bool,
    /// The GitHub CLI (`gh` on `PATH` by default; tests point it at a fake).
    pub gh_program: PathBuf,
    /// The user's GitHub repositories for the Clone dialog (`None`: not loaded).
    pub github_repos: Option<Result<Vec<GhRepo>, String>>,
    pub github_loading: bool,
    /// The Clone dialog was opened from Git › GitHub (lists the user's repositories).
    pub clone_github: bool,
    /// The last pull request created.
    pub last_pull_request: Option<String>,
    /// Changelists of the project (`.codecraft/changelists.json`).
    pub changelists: codecraft_vcs::changelist::ChangeLists,
    changelists_for: Option<PathBuf>,
    /// Shelved changes, newest first.
    pub shelves: Vec<codecraft_vcs::shelf::Shelf>,
    pub shelves_loading: bool,
    /// The shelf (and file) selected in the Shelf tab.
    pub shelf_selected: Option<(String, Option<String>)>,
    /// Patch of the selected shelved file: (shelf id, path, text or error).
    pub shelf_patch: Option<(String, String, Result<String, String>)>,
    /// Pull requests (Git › GitHub › View Pull Requests).
    pub prs: crate::github_prs::PrState,
    /// The last repository or gist created on GitHub.
    pub last_github_url: Option<String>,
    markers: HashMap<DocId, Markers>,
    heads: HashMap<String, HeadText>,
    head_oid: Option<String>,
    /// Per-path state (`rel` → state) and directories containing tracked changes.
    states: HashMap<String, FileState>,
    changed_dirs: HashSet<String>,
    ignored_dirs: Vec<String>,
    discovered_for: Option<PathBuf>,
    discovering: bool,
    refreshing: bool,
    refresh_again: Option<bool>,
    /// `.codecraft/.gitignore` was checked this session (see `ignore_workspace_state`).
    state_ignore_checked: bool,
    show_conflicts_after_refresh: bool,
    git_env: Vec<(OsString, OsString)>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    console_tx: Sender<ConsoleLine>,
    console_rx: Receiver<ConsoleLine>,
}

impl Default for VcsState {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        let (console_tx, console_rx) = unbounded();
        Self {
            repo: None,
            unavailable: None,
            status: RepoStatus::default(),
            state: RepoState::Normal,
            branches: Vec::new(),
            tags: Vec::new(),
            remotes: Vec::new(),
            stashes: Vec::new(),
            log: LogView::default(),
            console: Vec::new(),
            commit: CommitForm::default(),
            preview: None,
            compare: None,
            dialog: None,
            dialog_error: None,
            branches_popup: false,
            hunk: None,
            git_tab: GitTab::Log,
            commit_tab: CommitTab::Changes,
            focus_message: false,
            generating: false,
            op: None,
            blame: HashMap::new(),
            merge: None,
            merge_loading: None,
            worktrees: Vec::new(),
            worktrees_loading: false,
            gh_program: PathBuf::from("gh"),
            github_repos: None,
            github_loading: false,
            clone_github: false,
            last_pull_request: None,
            changelists: codecraft_vcs::changelist::ChangeLists::default(),
            changelists_for: None,
            shelves: Vec::new(),
            shelves_loading: false,
            shelf_selected: None,
            shelf_patch: None,
            prs: crate::github_prs::PrState::default(),
            last_github_url: None,
            markers: HashMap::new(),
            heads: HashMap::new(),
            head_oid: None,
            states: HashMap::new(),
            changed_dirs: HashSet::new(),
            ignored_dirs: Vec::new(),
            discovered_for: None,
            discovering: false,
            refreshing: false,
            refresh_again: None,
            state_ignore_checked: false,
            show_conflicts_after_refresh: false,
            git_env: Vec::new(),
            tx,
            rx,
            console_tx,
            console_rx,
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(format!("vcs-{name}")).spawn(f) {
        log::error!("cannot start a git worker: {e}");
    }
}

fn err_text(e: &GitError) -> String {
    match e {
        GitError::AuthRequired(m) => format!(
            "Authentication required. CodeCraft never prompts for credentials: set up a credential helper \
             (e.g. `git config --global credential.helper osxkeychain`) or load your SSH key into the agent \
             (`ssh-add`), then retry.\n{m}"
        ),
        other => other.to_string(),
    }
}

impl VcsState {
    /// Environment for every git command (tests isolate `HOME` and `GIT_CONFIG_GLOBAL`).
    pub fn set_git_env(&mut self, env: Vec<(String, String)>) {
        self.git_env = env.into_iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect();
    }

    fn git_in(&self, dir: &Path) -> Git {
        let mut g = Git::new(dir);
        for (k, v) in &self.git_env {
            g = g.with_env(k, v);
        }
        g
    }

    /// A git invoker in the repository root.
    pub fn git(&self) -> Option<Git> {
        self.repo.as_ref().map(|r| self.git_in(&r.root))
    }

    pub fn is_repo(&self) -> bool {
        self.repo.is_some()
    }

    /// The repository of the project at `root` is not known yet, or its status is being read
    /// (agents' `inspect {scope: "uncommitted"}` right after the start waits for it).
    pub fn settling_for(&self, root: &Path) -> bool {
        self.discovered_for.as_deref() != Some(root) || self.discovering || (self.repo.is_some() && self.refreshing)
    }

    /// Background work is in flight (the UI keeps repainting).
    pub fn busy(&self) -> bool {
        self.generating
            || self.op.is_some()
            || self.refreshing
            || self.discovering
            || self.log.loading
            || self.merge_loading.is_some()
            || self.worktrees_loading
            || self.github_loading
            || self.shelves_loading
            || self.prs.busy()
            || self.hunk.as_ref().is_some_and(|h| h.stage_loading)
            || self.markers.values().any(|m| m.pending || m.seen_at.is_some())
            || self.blame.values().any(|b| b.loading || b.seen_at.is_some())
    }

    /// The current branch name (`None` when detached or outside a repository).
    pub fn branch_name(&self) -> Option<&str> {
        self.status.branch.head.as_deref()
    }

    /// What the branch widget shows: the branch, or the short hash when detached.
    pub fn head_label(&self) -> String {
        match (&self.status.branch.head, &self.status.branch.oid) {
            (Some(b), _) => b.clone(),
            (None, Some(oid)) => oid.get(..8).unwrap_or(oid).to_string(),
            (None, None) => "No commits".into(),
        }
    }

    /// The repository-relative path of an absolute path.
    pub fn rel(&self, path: &Path) -> Option<String> {
        self.repo.as_ref()?.rel(path)
    }

    pub fn abs(&self, rel: &str) -> Option<PathBuf> {
        self.repo.as_ref().map(|r| r.abs(rel))
    }

    /// The VCS state of a file (`None`: unchanged or outside the repository).
    pub fn file_state(&self, path: &Path) -> Option<FileState> {
        let rel = self.rel(path)?;
        if let Some(s) = self.states.get(&rel) {
            return Some(*s);
        }
        self.ignored_dirs
            .iter()
            .any(|d| rel.starts_with(d.as_str()) || format!("{rel}/") == *d)
            .then_some(FileState::Ignored)
    }

    /// The colour state of a directory: ignored, or containing tracked changes (modified).
    pub fn dir_state(&self, path: &Path) -> Option<FileState> {
        let rel = self.rel(path)?;
        if self.ignored_dirs.iter().any(|d| format!("{rel}/").starts_with(d.as_str())) {
            return Some(FileState::Ignored);
        }
        self.changed_dirs.contains(&rel).then_some(FileState::Modified)
    }

    /// Gutter change markers of an open document.
    pub fn markers(&self, doc: DocId) -> &[LineChange] {
        self.markers.get(&doc).map(|m| m.changes.as_slice()).unwrap_or_default()
    }

    /// Annotations of an open document, if Annotate is on for it.
    pub fn blame_for(&self, doc: DocId) -> Option<&BlameView> {
        self.blame.get(&doc)
    }

    /// Tracked changes (staged or not) and conflicts, sorted by path.
    pub fn changes(&self) -> Vec<&FileStatus> {
        let mut v: Vec<&FileStatus> = self.status.changes().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    pub fn unversioned(&self) -> Vec<&FileStatus> {
        let mut v: Vec<&FileStatus> = self.status.untracked().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    pub fn conflicts(&self) -> Vec<&FileStatus> {
        self.status.conflicts().collect()
    }

    /// Whether a change is checked for commit.
    pub fn is_included(&self, f: &FileStatus) -> bool {
        if f.untracked {
            self.commit.included_untracked.contains(&f.path)
        } else {
            !self.commit.excluded.contains(&f.path)
        }
    }

    /// Paths checked for commit (tracked first, then unversioned).
    pub fn included_paths(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.changes().into_iter().filter(|f| self.is_included(f)).map(|f| f.path.clone()).collect();
        v.extend(self.unversioned().into_iter().filter(|f| self.is_included(f)).map(|f| f.path.clone()));
        v
    }

    fn console_push(&mut self, kind: ConsoleKind, text: impl Into<String>) {
        self.console.push(ConsoleEntry { kind, text: text.into() });
        if self.console.len() > MAX_CONSOLE {
            let drop = self.console.len() - MAX_CONSOLE;
            self.console.drain(..drop);
        }
    }
}

// ---------------------------------------------------------------- pump / shutdown / watcher

/// Drain background events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = discover_if_needed(s);
    // Console lines (progress lines replace each other).
    let mut last_progress = false;
    while let Ok(line) = s.vcs.console_rx.try_recv() {
        changed = true;
        match line {
            ConsoleLine::Command(c) => {
                s.vcs.console_push(ConsoleKind::Command, c);
                last_progress = false;
            }
            ConsoleLine::Output { text, progress, .. } => {
                if text.trim().is_empty() && !progress {
                    continue;
                }
                if last_progress && let Some(last) = s.vcs.console.last_mut() {
                    last.text = text;
                } else {
                    s.vcs.console_push(ConsoleKind::Output, text);
                }
                last_progress = progress;
            }
            ConsoleLine::Exit(_) => last_progress = false,
        }
    }
    while let Ok(ev) = s.vcs.rx.try_recv() {
        changed = true;
        handle(s, ev);
    }
    changed |= crate::github_prs::pump(s);
    changed |= update_markers(s);
    changed |= update_blame(s);
    changed
}

/// Stop background work (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    if let Some(op) = s.vcs.op.take() {
        op.cancel.cancel();
    }
    s.ws.tasks.remove(TASK_OP);
}

/// Files changed on disk (from the project file watcher): refresh status as needed.
pub(crate) fn files_changed(s: &mut Session, paths: &[PathBuf]) {
    let Some(repo) = &s.vcs.repo else { return };
    let mut status = false;
    let mut full = false;
    for p in paths {
        let in_git_dir = p.starts_with(&repo.git_dir) || p.components().any(|c| c.as_os_str() == ".git");
        if in_git_dir {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let noise = name.ends_with(".lock")
                || p.components().any(|c| matches!(c.as_os_str().to_str(), Some("objects" | "logs" | "lfs")));
            if noise {
                continue;
            }
            if name == "index" {
                status = true;
            } else {
                full = true;
            }
        } else if repo.contains(p) {
            status = true;
        }
    }
    if full || status {
        request_refresh(s, full);
    }
}

fn discover_if_needed(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root == s.vcs.discovered_for {
        return false;
    }
    // The project changed: forget the old repository.
    let env = std::mem::take(&mut s.vcs.git_env);
    let gh_program = std::mem::take(&mut s.vcs.gh_program);
    let last_github_url = s.vcs.last_github_url.take();
    let open_dialog_free = VcsState { git_env: env, gh_program, last_github_url, ..VcsState::default() };
    let old_console = std::mem::take(&mut s.vcs.console);
    if let Some(op) = s.vcs.op.take() {
        op.cancel.cancel();
    }
    s.vcs = open_dialog_free;
    s.vcs.console = old_console;
    s.vcs.discovered_for = root.clone();
    s.ws.tasks.remove(TASK_OP);
    let Some(root) = root else { return true };
    s.vcs.discovering = true;
    let git = s.vcs.git_in(&root);
    let tx = s.vcs.tx.clone();
    spawn("discover", move || {
        let result = codecraft_vcs::discover(&git);
        let _ = tx.send(Event::Discovered { root, result });
    });
    true
}

/// What `.codecraft/.gitignore` keeps out of the repository: CodeCraft's per-user workspace
/// state, like IntelliJ's `.idea/.gitignore` (`/workspace.xml`, `/shelf/`). Shared project
/// settings (run configurations, data sources, inspection profiles and baseline, deployment,
/// code style) stay versionable.
pub const STATE_GITIGNORE: &str = "# CodeCraft's per-user state (like .idea/workspace.xml); shared settings stay versioned\n\
/changelists.json\n/shelf/\n/breakpoints.json\n/bookmarks.json\n/tasks.json\n/local-history/\n\
/test-history.json\n/build/\n/coverage/\n/profiler/\n/cache/\n/consoles/\n/scratches/\n/feedback/\n";

/// QA M15: the first changelist, shelf, breakpoint or bookmark wrote `.codecraft/…` into the
/// project and it showed in the Commit tool window as an unversioned file to add. When a status
/// lists untracked files under the project's `.codecraft/` and it has no `.gitignore`, one is
/// written (once per session, so a user who deletes it keeps it deleted) and status is read
/// again.
fn ignore_workspace_state(s: &mut Session) {
    if s.vcs.state_ignore_checked {
        return;
    }
    let (Some(repo), Some(project)) = (s.vcs.repo.as_ref(), s.project_root()) else { return };
    let dir = project.join(".codecraft");
    let Some(prefix) = repo.rel(&dir) else { return };
    let prefix = format!("{prefix}/");
    let untracked = s.vcs.status.files.iter().any(|f| f.untracked && !f.ignored && f.path.starts_with(&prefix));
    if !untracked {
        return;
    }
    s.vcs.state_ignore_checked = true;
    let file = dir.join(".gitignore");
    if file.exists() {
        return;
    }
    match std::fs::write(&file, STATE_GITIGNORE) {
        Ok(()) => request_refresh(s, false),
        Err(e) => log::warn!("cannot write {}: {e}", file.display()),
    }
}

/// Re-read status (and with `full`, branches, tags, remotes and stashes) in the background.
pub fn request_refresh(s: &mut Session, full: bool) {
    let Some(git) = s.vcs.git() else { return };
    if s.vcs.refreshing {
        s.vcs.refresh_again = Some(full || s.vcs.refresh_again.unwrap_or(false));
        return;
    }
    s.vcs.refreshing = true;
    let git_dir = s.vcs.repo.as_ref().map(|r| r.git_dir.clone()).unwrap_or_default();
    let tx = s.vcs.tx.clone();
    spawn("status", move || {
        let result = (|| {
            let status = codecraft_vcs::status::status(&git, StatusOptions { ignored: true, untracked: true })?;
            let state = RepoState::detect(&git_dir);
            let refs = if full {
                Some((
                    codecraft_vcs::refs::branches(&git)?,
                    codecraft_vcs::refs::tags(&git)?,
                    codecraft_vcs::refs::remotes(&git)?,
                    codecraft_vcs::refs::stashes(&git)?,
                ))
            } else {
                None
            };
            Ok(Snapshot { status, state, refs })
        })();
        let _ = tx.send(Event::Refreshed(result));
    });
}

fn handle(s: &mut Session, ev: Event) {
    match ev {
        Event::Discovered { root, result } => {
            if s.vcs.discovered_for.as_deref() != Some(root.as_path()) {
                return;
            }
            s.vcs.discovering = false;
            match result {
                Ok(Some(info)) => {
                    s.vcs.repo = Some(info);
                    s.vcs.unavailable = None;
                    changelists::load(s);
                    request_refresh(s, true);
                    load_log(s, true);
                    shelf::load(s);
                }
                Ok(None) => s.vcs.unavailable = None,
                Err(e) => s.vcs.unavailable = Some(e.to_string()),
            }
        }
        Event::Refreshed(result) => {
            s.vcs.refreshing = false;
            match result {
                Ok(snap) => apply_snapshot(s, snap),
                Err(e) => log::warn!("git status: {e}"),
            }
            if let Some(full) = s.vcs.refresh_again.take() {
                request_refresh(s, full);
            }
        }
        Event::Generated(result) => {
            s.vcs.generating = false;
            match result {
                Ok(message) => {
                    s.vcs.commit.message = message;
                    s.status = "Commit message generated".into();
                }
                Err(e) => s.notify(NotificationLevel::Error, "Cannot generate the commit message", e),
            }
        }
        Event::Head { oid, rel, text } => {
            if oid == s.vcs.head_oid {
                s.vcs.heads.insert(rel, HeadText::Ready(text));
            }
        }
        Event::Markers { doc, revision, key, changes } => {
            if let Some(m) = s.vcs.markers.get_mut(&doc) {
                m.pending = false;
                m.changes = Arc::new(changes);
                m.revision = Some(revision);
                m.key = Some(key);
            }
        }
        Event::LogPage { generation, skip, result } => {
            if generation != s.vcs.log.generation {
                return;
            }
            let log = &mut s.vcs.log;
            log.loading = false;
            log.loaded = true;
            match result {
                Ok(page) => {
                    log.error = None;
                    log.has_more = page.len() >= LOG_PAGE;
                    log.commits.truncate(skip);
                    log.commits.extend(page);
                    log.graph = codecraft_vcs::graph(&log.commits);
                    // Keep the selection if the commit is still listed.
                    if let Some(sel) = &log.selected
                        && !log.commits.iter().any(|c| &c.hash == sel)
                        && skip == 0
                    {
                        log.selected = None;
                        log.details = None;
                        log.selected_file = None;
                        log.file_diff = None;
                    }
                }
                Err(e) => log.error = Some(e.to_string()),
            }
        }
        Event::Details { hash, result } => {
            if s.vcs.log.selected.as_deref() != Some(hash.as_str()) {
                return;
            }
            s.vcs.log.details_loading = false;
            match result {
                Ok(d) => s.vcs.log.details = Some(d),
                Err(e) => s.vcs.log.error = Some(e.to_string()),
            }
        }
        Event::CommitFile { hash, result } => {
            if s.vcs.log.selected.as_deref() == Some(hash.as_str())
                && s.vcs.log.selected_file.as_deref() == Some(result.path.as_str())
            {
                s.vcs.log.file_diff = Some(result);
            }
        }
        Event::Preview(diff) => {
            if s.vcs.commit.selected.as_deref() == Some(diff.path.as_str()) {
                s.vcs.preview = Some(diff);
            }
        }
        Event::Compare(diff) => {
            if s.vcs.compare.as_ref().is_some_and(|c| c.path == diff.path) {
                s.vcs.compare = Some(diff);
            }
        }
        Event::Outgoing { branch, result } => {
            if let Some(Dialog::Push { branch: b, commits, .. }) = &mut s.vcs.dialog
                && *b == branch
            {
                match result {
                    Ok(list) => *commits = Some(list),
                    Err(e) => {
                        *commits = Some(Vec::new());
                        s.vcs.dialog_error = Some(e.to_string());
                    }
                }
            }
        }
        Event::Blame { doc, revision, result } => {
            if let Some(b) = s.vcs.blame.get_mut(&doc) {
                b.loading = false;
                b.revision = revision;
                match result {
                    Ok(bl) => {
                        b.blame = Some(Arc::new(bl));
                        b.error = None;
                    }
                    Err(e) => b.error = Some(e.to_string()),
                }
            }
        }
        Event::OpDone { label, kind, result } => op_done(s, &label, kind, result),
        Event::Worktrees(result) => {
            s.vcs.worktrees_loading = false;
            match result {
                Ok(list) => s.vcs.worktrees = list,
                Err(e) => s.vcs.dialog_error = Some(err_text(&e)),
            }
        }
        Event::GhRepos(result) => {
            s.vcs.github_loading = false;
            s.vcs.github_repos = Some(result.map_err(|e| err_text(&e)));
        }
        Event::Shelves(list) => {
            s.vcs.shelves_loading = false;
            s.vcs.shelves = list;
            if let Some((id, _)) = &s.vcs.shelf_selected
                && !s.vcs.shelves.iter().any(|sh| &sh.id == id)
            {
                s.vcs.shelf_selected = None;
                s.vcs.shelf_patch = None;
            }
        }
        Event::ShelfPatch { id, path, result } => {
            if s.vcs.shelf_selected.as_ref().is_some_and(|(i, p)| *i == id && p.as_deref() == Some(path.as_str())) {
                s.vcs.shelf_patch = Some((id, path, result.map_err(|e| e.to_string())));
            }
        }
        Event::HunkStage { doc, start, state } => {
            if let Some(h) = &mut s.vcs.hunk
                && h.doc == doc
                && h.change.start == start
            {
                h.stage = state;
                h.stage_loading = false;
            }
        }
        Event::MergeLoaded { rel, labels, result } => {
            if s.vcs.merge_loading.as_deref() != Some(rel.as_str()) {
                return;
            }
            s.vcs.merge_loading = None;
            let Some(abs) = s.vcs.abs(&rel) else { return };
            match result {
                Ok((base, Some(ours), Some(theirs))) => {
                    s.vcs.dialog = None;
                    s.vcs.merge = Some(crate::vcs_merge::MergeView::new(
                        rel,
                        abs,
                        base.unwrap_or_default(),
                        ours,
                        theirs,
                        labels,
                    ));
                }
                // Deleted on one side, or binary: only Accept Yours/Theirs apply.
                Ok(_) => {
                    s.vcs.dialog_error =
                        Some(format!("{rel} was deleted on one side or is binary: accept yours or theirs"));
                }
                Err(e) => s.vcs.dialog_error = Some(err_text(&e)),
            }
        }
    }
}

fn apply_snapshot(s: &mut Session, snap: Snapshot) {
    let head_moved = snap.status.branch.oid != s.vcs.head_oid;
    s.vcs.head_oid = snap.status.branch.oid.clone();
    if head_moved {
        s.vcs.heads.clear();
        for b in s.vcs.blame.values_mut() {
            b.revision = u64::MAX;
        }
    }
    let mut states = HashMap::new();
    let mut changed_dirs = HashSet::new();
    let mut ignored_dirs = Vec::new();
    for f in &snap.status.files {
        if f.ignored && f.path.ends_with('/') {
            ignored_dirs.push(f.path.clone());
            continue;
        }
        states.insert(f.path.clone(), f.state());
        if !f.untracked && !f.ignored {
            let mut dir = f.path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                changed_dirs.insert(parent.to_string());
                dir = parent;
            }
            changed_dirs.insert(String::new());
        }
    }
    s.vcs.states = states;
    s.vcs.changed_dirs = changed_dirs;
    s.vcs.ignored_dirs = ignored_dirs;
    // Forget checkbox choices for files that are no longer changed.
    let present: HashSet<&str> = snap.status.files.iter().map(|f| f.path.as_str()).collect();
    s.vcs.commit.excluded.retain(|p| present.contains(p.as_str()));
    s.vcs.commit.included_untracked.retain(|p| present.contains(p.as_str()));
    if s.vcs.commit.selected.as_deref().is_some_and(|p| !present.contains(p)) {
        s.vcs.commit.selected = None;
        s.vcs.preview = None;
    }
    let refs_changed = snap.refs.is_some();
    s.vcs.status = snap.status;
    s.vcs.state = snap.state;
    ignore_workspace_state(s);
    changelists::sync(s);
    if let Some((branches, tags, remotes, stashes)) = snap.refs {
        s.vcs.branches = branches;
        s.vcs.tags = tags;
        s.vcs.remotes = remotes;
        s.vcs.stashes = stashes;
    }
    if (refs_changed || head_moved) && s.vcs.log.loaded {
        load_log(s, true);
    }
    // The selected change's diff may be stale.
    if let Some(sel) = s.vcs.commit.selected.clone() {
        select_change(s, &sel);
    }
    // Only a refresh that started after the operation ended shows its conflicts.
    if s.vcs.show_conflicts_after_refresh && s.vcs.refresh_again.is_none() {
        s.vcs.show_conflicts_after_refresh = false;
        if !s.vcs.conflicts().is_empty() {
            s.vcs.dialog = Some(Dialog::Conflicts { selected: None });
            s.vcs.dialog_error = None;
        }
    }
    if s.vcs.conflicts().is_empty()
        && matches!(s.vcs.dialog, Some(Dialog::Conflicts { .. }))
        && s.vcs.state == RepoState::Normal
    {
        s.vcs.dialog = None;
    }
}

// ---------------------------------------------------------------- gutter markers

fn base_for(s: &Session, rel: &str) -> Base {
    match s.vcs.status.file(rel) {
        Some(f) if f.untracked || f.ignored => Base::None,
        Some(f) if f.conflict.is_some() => Base::None,
        Some(f) if !f.in_head() => Base::Empty,
        _ => {
            let ignored = s.vcs.file_state(&s.vcs.abs(rel).unwrap_or_default()) == Some(FileState::Ignored);
            if ignored || s.vcs.head_oid.is_none() { Base::None } else { Base::Head }
        }
    }
}

fn update_markers(s: &mut Session) -> bool {
    let Some(repo) = s.vcs.repo.clone() else {
        let had = !s.vcs.markers.is_empty();
        s.vcs.markers.clear();
        return had;
    };
    let now = Instant::now();
    let mut changed = false;
    let docs: Vec<(DocId, PathBuf, u64)> =
        s.ws.docs().filter_map(|d| Some((d.id, d.path.clone()?, d.doc.revision()))).collect();
    let open: HashSet<DocId> = docs.iter().map(|(id, _, _)| *id).collect();
    let before = s.vcs.markers.len();
    s.vcs.markers.retain(|id, _| open.contains(id));
    changed |= before != s.vcs.markers.len();
    if s.vcs.refreshing && s.vcs.states.is_empty() && s.vcs.head_oid.is_none() {
        // The first status hasn't arrived yet.
        return changed;
    }
    for (id, path, revision) in docs {
        let Some(rel) = repo.rel(&path) else { continue };
        let base = base_for(s, &rel);
        let key = (s.vcs.head_oid.clone(), rel.clone());
        let entry = s.vcs.markers.entry(id).or_default();
        if base == Base::None {
            if !entry.changes.is_empty() {
                entry.changes = Arc::new(Vec::new());
                changed = true;
            }
            entry.revision = Some(revision);
            entry.key = Some(key);
            entry.seen_at = None;
            continue;
        }
        let stale = entry.revision != Some(revision) || entry.key.as_ref() != Some(&key);
        if !stale || entry.pending {
            continue;
        }
        // Debounce while typing (the first computation for a document is immediate).
        if entry.seen_revision != revision {
            let first = entry.revision.is_none();
            entry.seen_revision = revision;
            entry.seen_at = if first { None } else { Some(now) };
        }
        if entry.seen_at.is_some_and(|t| now.duration_since(t) < MARKER_DEBOUNCE) {
            continue;
        }
        let base_text = match base {
            Base::Empty => Some(Arc::new(String::new())),
            Base::Head => match s.vcs.heads.get(&rel) {
                Some(HeadText::Ready(t)) => t.clone(),
                Some(HeadText::Loading) => continue,
                None => {
                    load_head(s, &rel);
                    continue;
                }
            },
            Base::None => None,
        };
        let Some(base_text) = base_text else {
            // Not in HEAD (or binary): no markers.
            if let Some(e) = s.vcs.markers.get_mut(&id) {
                e.changes = Arc::new(Vec::new());
                e.revision = Some(revision);
                e.key = Some(key);
            }
            continue;
        };
        let Some(text) = s.ws.doc(id).map(|d| d.doc.text()) else { continue };
        if text.len() > MAX_MARKER_BYTES {
            continue;
        }
        if let Some(e) = s.vcs.markers.get_mut(&id) {
            e.pending = true;
            e.seen_at = None;
        }
        let tx = s.vcs.tx.clone();
        spawn("markers", move || {
            let changes = lines::line_changes(&base_text, &text);
            let _ = tx.send(Event::Markers { doc: id, revision, key, changes });
        });
        changed = true;
    }
    changed
}

fn load_head(s: &mut Session, rel: &str) {
    let Some(git) = s.vcs.git() else { return };
    s.vcs.heads.insert(rel.to_string(), HeadText::Loading);
    let oid = s.vcs.head_oid.clone();
    let rel = rel.to_string();
    let tx = s.vcs.tx.clone();
    spawn("head", move || {
        let text = match codecraft_vcs::log::text_at(&git, "HEAD", &rel) {
            Ok(Some(t)) if t.len() <= MAX_MARKER_BYTES => Some(Arc::new(t.replace("\r\n", "\n"))),
            _ => None,
        };
        let _ = tx.send(Event::Head { oid, rel, text });
    });
}

/// The change under `line` of a document.
pub fn change_at(s: &Session, doc: DocId, line: usize) -> Option<LineChange> {
    let m = s.vcs.markers(doc);
    m.iter().find(|c| c.start != c.end && c.covers(line)).or_else(|| m.iter().find(|c| c.covers(line))).cloned()
}

/// The `HEAD` text the markers of a document were computed against.
fn head_text(s: &Session, doc: DocId) -> Option<Arc<String>> {
    let path = s.ws.doc(doc)?.path.clone()?;
    let rel = s.vcs.rel(&path)?;
    match base_for(s, &rel) {
        Base::Empty => Some(Arc::new(String::new())),
        Base::Head => match s.vcs.heads.get(&rel) {
            Some(HeadText::Ready(t)) => t.clone(),
            _ => None,
        },
        Base::None => None,
    }
}

/// Open the change popup for the marker at `line` of a document.
pub fn show_hunk(s: &mut Session, doc: DocId, line: usize) -> Result<(), CommandError> {
    let change = change_at(s, doc, line).ok_or_else(|| CommandError::Failed("no change on this line".into()))?;
    let base = head_text(s, doc).ok_or_else(|| CommandError::Failed("the committed version is not loaded".into()))?;
    let new_text =
        s.ws.doc(doc)
            .map(|d| (change.start..change.end).filter_map(|l| d.doc.line_text(l)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
    let old_text = lines::base_text(&base, &change);
    let old_text = old_text.strip_suffix('\n').unwrap_or(&old_text).to_string();
    let current = s.ws.doc(doc).map(|d| d.doc.text()).unwrap_or_default();
    let rel = s.ws.doc(doc).and_then(|d| d.path.clone()).and_then(|p| s.vcs.rel(&p));
    let git = s.vcs.git();
    let stage_loading = rel.is_some() && git.is_some();
    s.vcs.hunk = Some(HunkPopup { doc, change: change.clone(), old_text, new_text, stage: None, stage_loading });
    if let (Some(rel), Some(git)) = (rel, git) {
        let tx = s.vcs.tx.clone();
        spawn("hunk-stage", move || {
            // The index version (`:path`); a file not in the index has nothing staged.
            let index = match codecraft_vcs::log::text_at(&git, "", &rel) {
                Ok(Some(t)) => t.replace("\r\n", "\n"),
                _ => base.as_str().to_string(),
            };
            let state = codecraft_vcs::hunks::stage_state(&base, &index, &current, &change);
            let _ = tx.send(Event::HunkStage { doc, start: change.start, state });
        });
    }
    Ok(())
}

/// Roll back the change at `line` of a document to its `HEAD` content (one undoable edit).
pub fn rollback_hunk(s: &mut Session, doc: DocId, line: usize) -> Result<(), CommandError> {
    let change = change_at(s, doc, line).ok_or_else(|| CommandError::Failed("no change on this line".into()))?;
    let base = head_text(s, doc).ok_or_else(|| CommandError::Failed("the committed version is not loaded".into()))?;
    let replacement = lines::base_text(&base, &change);
    let editor = s.ws.editor_for_doc(doc);
    let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
    let Some(od) = s.ws.doc_mut(doc) else { return Err(CommandError::Failed("the document is closed".into())) };
    if od.read_only {
        return Err(CommandError::Failed("the file is read-only".into()));
    }
    let d = &od.doc;
    let line_pos = |l: usize| if l >= d.len_lines() { d.len_chars() } else { d.line_start(l) };
    let start = line_pos(change.start);
    let end = line_pos(change.end).max(start);
    let changes = codecraft_text::ChangeSet::single(start, end, replacement.clone());
    let len_after = changes.len_after(d.len_chars());
    let caret = codecraft_text::SelectionSet::caret(start.min(len_after));
    let edit = codecraft_text::Edit { changes, selections: caret };
    od.doc
        .apply(&edit, &sels, codecraft_text::EditGroup::Standalone)
        .map_err(|e| CommandError::Failed(e.to_string()))?;
    s.vcs.hunk = None;
    if let Some(e) = editor {
        s.after_text_change(e);
    }
    // Recompute at once.
    if let Some(m) = s.vcs.markers.get_mut(&doc) {
        m.seen_at = None;
        m.seen_revision = 0;
    }
    Ok(())
}

// ---------------------------------------------------------------- blame

/// Toggle annotations for a document.
pub fn toggle_blame(s: &mut Session, doc: DocId) -> bool {
    if s.vcs.blame.remove(&doc).is_some() {
        return false;
    }
    s.vcs.blame.insert(doc, BlameView { loading: false, revision: u64::MAX, ..BlameView::default() });
    true
}

fn update_blame(s: &mut Session) -> bool {
    let Some(repo) = s.vcs.repo.clone() else { return false };
    let now = Instant::now();
    let mut changed = false;
    let ids: Vec<DocId> = s.vcs.blame.keys().copied().collect();
    for id in ids {
        let Some((path, revision, text)) =
            s.ws.doc(id).and_then(|d| Some((d.path.clone()?, d.doc.revision(), d.doc.text())))
        else {
            s.vcs.blame.remove(&id);
            changed = true;
            continue;
        };
        let Some(rel) = repo.rel(&path) else { continue };
        let Some(b) = s.vcs.blame.get_mut(&id) else { continue };
        if b.loading || b.revision == revision {
            continue;
        }
        if b.revision != u64::MAX && b.blame.is_some() {
            if b.seen_revision != revision {
                b.seen_revision = revision;
                b.seen_at = Some(now);
            }
            if b.seen_at.is_some_and(|t| now.duration_since(t) < BLAME_DEBOUNCE) {
                continue;
            }
        }
        b.seen_at = None;
        b.loading = true;
        changed = true;
        let git = s.vcs.git_in(&repo.root);
        let tx = s.vcs.tx.clone();
        spawn("blame", move || {
            let result = codecraft_vcs::blame(&git, &rel, Some(&text));
            let _ = tx.send(Event::Blame { doc: id, revision, result });
        });
    }
    changed
}

// ---------------------------------------------------------------- log

/// (Re)load the log from the first page (`reset`) or the next page.
pub fn load_log(s: &mut Session, reset: bool) {
    let Some(git) = s.vcs.git() else { return };
    if !reset && (s.vcs.log.loading || !s.vcs.log.has_more) {
        return;
    }
    s.vcs.log.generation += 1;
    s.vcs.log.loading = true;
    let skip = if reset { 0 } else { s.vcs.log.commits.len() };
    let q = s.vcs.log.query(skip);
    let generation = s.vcs.log.generation;
    let tx = s.vcs.tx.clone();
    spawn("log", move || {
        let result = codecraft_vcs::log::log(&git, &q);
        let _ = tx.send(Event::LogPage { generation, skip, result });
    });
}

pub fn select_commit(s: &mut Session, hash: &str) {
    let Some(git) = s.vcs.git() else { return };
    let log = &mut s.vcs.log;
    log.selected = Some(hash.to_string());
    log.details = None;
    log.details_loading = true;
    log.selected_file = None;
    log.file_diff = None;
    let hash = hash.to_string();
    let tx = s.vcs.tx.clone();
    spawn("details", move || {
        let result = codecraft_vcs::log::show_commit(&git, &hash);
        let _ = tx.send(Event::Details { hash, result });
    });
}

pub fn select_commit_file(s: &mut Session, path: &str) {
    let Some(git) = s.vcs.git() else { return };
    let Some(details) = s.vcs.log.details.clone() else { return };
    let Some(file) = details.files.iter().find(|f| f.path == path).cloned() else { return };
    s.vcs.log.selected_file = Some(path.to_string());
    s.vcs.log.file_diff = Some(FileDiff { path: path.to_string(), loading: true, ..FileDiff::default() });
    let tx = s.vcs.tx.clone();
    spawn("commit-file", move || {
        let hash = details.hash.clone();
        let short = hash.get(..8).unwrap_or(&hash).to_string();
        let parent = details.parents.first().cloned();
        let old_path = file.orig_path.clone().unwrap_or_else(|| file.path.clone());
        let old = match &parent {
            Some(p) => codecraft_vcs::log::file_at(&git, p, &old_path),
            None => Ok(None),
        };
        let new = codecraft_vcs::log::file_at(&git, &hash, &file.path);
        let result = file_diff(path_string(&file.path), old, new, parent.as_deref().map(short_hash), short);
        let _ = tx.send(Event::CommitFile { hash, result });
    });
}

fn short_hash(h: &str) -> String {
    h.get(..8).unwrap_or(h).to_string()
}

fn path_string(p: &str) -> String {
    p.to_string()
}

fn file_diff(
    path: String,
    old: Result<Option<Vec<u8>>, GitError>,
    new: Result<Option<Vec<u8>>, GitError>,
    old_title: Option<String>,
    new_title: String,
) -> FileDiff {
    let mut d =
        FileDiff { path, old_title: old_title.unwrap_or_else(|| "(none)".into()), new_title, ..FileDiff::default() };
    let mut text = |r: Result<Option<Vec<u8>>, GitError>| -> String {
        match r {
            Ok(Some(b)) if b.contains(&0) => {
                d.note = Some("Binary file".into());
                String::new()
            }
            Ok(Some(b)) => String::from_utf8_lossy(&b).replace("\r\n", "\n"),
            Ok(None) => String::new(),
            Err(e) => {
                d.note = Some(e.to_string());
                String::new()
            }
        }
    };
    let old = text(old);
    let new = text(new);
    d.old = old;
    d.new = new;
    d
}

// ---------------------------------------------------------------- commit tool window

/// Select a change in the Commit tool window and load its diff (HEAD vs the working copy).
pub fn select_change(s: &mut Session, rel: &str) {
    let Some(git) = s.vcs.git() else { return };
    let Some(abs) = s.vcs.abs(rel) else { return };
    let file = s.vcs.status.file(rel).cloned();
    s.vcs.commit.selected = Some(rel.to_string());
    if s.vcs.preview.as_ref().is_none_or(|p| p.path != rel) {
        s.vcs.preview = Some(FileDiff { path: rel.to_string(), loading: true, ..FileDiff::default() });
    }
    // An open document shows its unsaved text, like the editor.
    let buffer = s.ws.doc_by_path(&abs).and_then(|id| s.ws.doc(id)).map(|d| d.doc.text());
    let rel = rel.to_string();
    let tx = s.vcs.tx.clone();
    spawn("preview", move || {
        let in_head = file.as_ref().is_none_or(|f| !f.untracked && f.index != codecraft_vcs::Change::Added);
        let old_path = file.as_ref().and_then(|f| f.orig_path.clone()).unwrap_or_else(|| rel.clone());
        let old = if in_head { codecraft_vcs::log::file_at(&git, "HEAD", &old_path) } else { Ok(None) };
        let new = match buffer {
            Some(t) => Ok(Some(t.into_bytes())),
            None => match std::fs::read(&abs) {
                Ok(b) if b.len() > codecraft_vcs::log::MAX_BLOB => {
                    Err(GitError::TooLarge("the file is too large to compare".into()))
                }
                Ok(b) => Ok(Some(b)),
                Err(_) => Ok(None),
            },
        };
        let diff = file_diff(rel, old, new, Some("HEAD".into()), "Your version".into());
        let _ = tx.send(Event::Preview(diff));
    });
}

/// Compare a file's working copy with a revision (Compare with Revision/Branch, Show Diff).
pub fn compare_with(s: &mut Session, rel: &str, rev: &str) {
    let Some(git) = s.vcs.git() else { return };
    let Some(abs) = s.vcs.abs(rel) else { return };
    let buffer = s.ws.doc_by_path(&abs).and_then(|id| s.ws.doc(id)).map(|d| d.doc.text());
    s.vcs.compare = Some(FileDiff { path: rel.to_string(), loading: true, ..FileDiff::default() });
    let rel = rel.to_string();
    let rev = rev.to_string();
    let tx = s.vcs.tx.clone();
    spawn("compare", move || {
        let old = codecraft_vcs::log::file_at(&git, &rev, &rel);
        let new = match buffer {
            Some(t) => Ok(Some(t.into_bytes())),
            None => Ok(std::fs::read(&abs).ok()),
        };
        let diff = file_diff(rel, old, new, Some(rev), "Your version".into());
        let _ = tx.send(Event::Compare(diff));
    });
}

/// Load the commits a push would send into the Push dialog.
pub fn load_outgoing(s: &mut Session, branch: &str, remote_branch: Option<String>) {
    let Some(git) = s.vcs.git() else { return };
    let branch = branch.to_string();
    let tx = s.vcs.tx.clone();
    spawn("outgoing", move || {
        let result = codecraft_vcs::ops::outgoing(&git, &branch, remote_branch.as_deref());
        let _ = tx.send(Event::Outgoing { branch, result });
    });
}

// ---------------------------------------------------------------- operations

/// Run a repository-changing operation on a worker thread (one at a time). Its commands and
/// output go to the console; it ends with a notification and a refresh.
fn start_op<F>(s: &mut Session, label: &str, kind: OpKind, f: F) -> Result<(), CommandError>
where
    F: FnOnce(&Git) -> Result<String, GitError> + Send + 'static,
{
    if let Some(op) = &s.vcs.op {
        return Err(CommandError::Failed(format!("Wait for “{}” to finish (or cancel it)", op.label)));
    }
    let git = match &kind {
        OpKind::Clone { target, .. } => s.vcs.git_in(target.parent().unwrap_or(Path::new("/"))),
        OpKind::Share { root } => s.vcs.git_in(root),
        _ => s.vcs.git().ok_or_else(|| CommandError::Failed("the project is not a Git repository".into()))?,
    };
    let cancel = CancelToken::new();
    let git = git.with_sink(s.vcs.console_tx.clone()).with_cancel(cancel.clone());
    s.vcs.console_push(ConsoleKind::Info, format!("{label}…"));
    s.vcs.op = Some(RunningOp { label: label.to_string(), started: Instant::now(), cancel });
    s.ws.tasks.insert(TASK_OP.into(), BackgroundTask { title: format!("{label}…"), message: None, percentage: None });
    s.status = format!("{label}…");
    let tx = s.vcs.tx.clone();
    let label = label.to_string();
    spawn("op", move || {
        let result = f(&git);
        let _ = tx.send(Event::OpDone { label, kind, result });
    });
    Ok(())
}

fn op_done(s: &mut Session, label: &str, kind: OpKind, result: Result<String, GitError>) {
    s.vcs.op = None;
    s.ws.tasks.remove(TASK_OP);
    match &result {
        Ok(summary) => {
            let summary_line = summary.lines().last().unwrap_or_default().to_string();
            s.vcs.console_push(ConsoleKind::Info, format!("{label}: done"));
            match &kind {
                OpKind::Patch { clipboard: true } => {
                    s.effects.push(Effect::CopyToClipboard(summary.clone()));
                    s.clipboard = Some((summary.clone(), false));
                    s.notify(NotificationLevel::Info, label, "Patch copied to the clipboard");
                }
                _ => s.notify(NotificationLevel::Info, label, summary_line),
            }
            s.vcs.dialog_error = None;
        }
        Err(GitError::Cancelled) => {
            s.vcs.console_push(ConsoleKind::Info, format!("{label}: cancelled"));
            s.status = format!("{label}: cancelled");
        }
        Err(e) => {
            let text = err_text(e);
            s.vcs.console_push(ConsoleKind::Error, text.clone());
            s.notify(NotificationLevel::Error, &format!("{label} failed"), text.clone());
            s.vcs.dialog_error = Some(text);
        }
    }
    match kind {
        OpKind::Commit { push_after } if result.is_ok() => {
            s.vcs.commit.message.clear();
            s.vcs.commit.amend = false;
            s.vcs.commit.excluded.clear();
            s.vcs.commit.included_untracked.clear();
            if push_after {
                open_push_dialog(s);
            }
        }
        OpKind::Clone { target, open } if result.is_ok() => {
            s.vcs.dialog = None;
            if open && let Err(e) = s.execute("file.open", &serde_json::json!({ "path": target })) {
                s.status = e.to_string();
            }
            return;
        }
        OpKind::MayConflict => s.vcs.show_conflicts_after_refresh = true,
        OpKind::Worktrees { open } => {
            load_worktrees(s);
            if let (Ok(_), Some(path)) = (&result, open) {
                s.vcs.dialog = None;
                if let Err(e) = s.execute("file.open", &serde_json::json!({ "path": path })) {
                    s.status = e.to_string();
                }
                return;
            }
        }
        OpKind::PullRequest { open } => {
            if let Ok(url) = &result {
                s.vcs.dialog = None;
                s.vcs.last_pull_request = Some(url.clone());
                if open {
                    s.effects.push(Effect::OpenUrl(url.clone()));
                }
            }
        }
        OpKind::Hunk if result.is_ok() => s.vcs.hunk = None,
        OpKind::Shelf => {
            shelf::load(s);
            s.vcs.show_conflicts_after_refresh = true;
        }
        OpKind::Share { .. } => {
            if let Ok(url) = &result {
                s.vcs.dialog = None;
                s.vcs.last_github_url = url.lines().map(str::trim).rfind(|l| l.starts_with("https://")).map(str::to_string);
            }
            // The project may have just become a repository: discover it again.
            if s.vcs.repo.is_none() {
                s.vcs.discovered_for = None;
                return;
            }
        }
        OpKind::Gh { reload_prs, open_url } => {
            if let Ok(out) = &result {
                s.vcs.dialog = None;
                if let Some(url) = out.lines().map(str::trim).rfind(|l| l.starts_with("https://")) {
                    s.vcs.last_github_url = Some(url.to_string());
                    if open_url {
                        s.effects.push(Effect::OpenUrl(url.to_string()));
                    }
                }
            }
            if reload_prs {
                crate::github_prs::reload(s);
            }
        }
        _ => {}
    }
    request_refresh(s, true);
}

/// Cancel the running operation.
pub fn cancel(s: &mut Session) -> bool {
    match &s.vcs.op {
        Some(op) => {
            op.cancel.cancel();
            true
        }
        None => false,
    }
}

fn summary(out: &codecraft_vcs::Output) -> String {
    out.combined()
}

/// Ask the configured language model for a commit message for the included changes; the result
/// replaces the message box when it arrives.
pub fn generate_message(s: &mut Session) -> Result<(), CommandError> {
    if s.vcs.generating {
        return Err(CommandError::Failed("A commit message is already being generated".into()));
    }
    let git = s.vcs.git().ok_or_else(|| CommandError::Failed("the project is not a Git repository".into()))?;
    let files = s.vcs.included_paths();
    if files.is_empty() {
        return Err(CommandError::Failed("Select the changes to describe".into()));
    }
    let cfg = crate::commit_ai::Config::from_settings(&s.ws.settings).map_err(CommandError::Failed)?;
    // Fail on a missing key before any work.
    crate::commit_ai::build_request(&cfg, "").map_err(CommandError::Failed)?;
    let untracked: Vec<(String, PathBuf)> = files
        .iter()
        .filter(|f| s.vcs.status.file(f).is_some_and(|st| st.untracked))
        .filter_map(|f| s.vcs.abs(f).map(|abs| (f.clone(), abs)))
        .collect();
    s.vcs.generating = true;
    s.status = format!("Generating a commit message with {}…", cfg.provider.label());
    let tx = s.vcs.tx.clone();
    spawn("generate-message", move || {
        let mut diff = String::new();
        for f in &files {
            for staged in [true, false] {
                if let Ok(d) = codecraft_vcs::ops::diff_text(&git, f, staged) {
                    diff.push_str(&d);
                }
            }
        }
        // New files have no diff yet: show their start.
        for (rel, abs) in &untracked {
            if let Ok(text) = std::fs::read_to_string(abs) {
                let head: String = text.chars().take(2000).collect();
                diff.push_str(&format!("new file: {rel}\n{head}\n"));
            }
        }
        let files = files.clone();
        let _ = tx.send(Event::Generated(crate::commit_ai::generate(&cfg, &files, &diff)));
    });
    Ok(())
}

/// Commit the given files (repository-relative), optionally opening the push dialog afterwards.
pub fn commit(s: &mut Session, files: Option<Vec<String>>, push_after: bool) -> Result<(), CommandError> {
    let form = s.vcs.commit.clone();
    if form.message.trim().is_empty() && !form.amend {
        return Err(CommandError::Failed("Enter a commit message".into()));
    }
    let files = files.unwrap_or_else(|| s.vcs.included_paths());
    if files.is_empty() && !form.amend {
        return Err(CommandError::Failed("Select the changes to commit".into()));
    }
    // Untracked files are added first; a staged rename needs its old path too.
    let mut add = Vec::new();
    let mut paths = Vec::new();
    for f in &files {
        match s.vcs.status.file(f) {
            Some(st) if st.untracked => add.push(f.clone()),
            Some(st) => {
                if let Some(o) = &st.orig_path
                    && !paths.contains(o)
                {
                    paths.push(o.clone());
                }
            }
            None => {}
        }
        paths.push(f.clone());
    }
    // Save open documents first, as RustRover does.
    let _ = s.save_all();
    let req = codecraft_vcs::ops::CommitRequest {
        message: form.message.clone(),
        paths: (!paths.is_empty()).then_some(paths),
        add,
        amend: form.amend,
        author: Some(form.author.clone()).filter(|a| !a.trim().is_empty()),
        sign_off: form.sign_off,
    };
    let n = files.len();
    start_op(s, if form.amend { "Amend Commit" } else { "Commit" }, OpKind::Commit { push_after }, move |git| {
        let (out, head) = codecraft_vcs::ops::commit(git, &req)?;
        let short = head.get(..8).unwrap_or(&head).to_string();
        let mut msg = format!("{n} file{} committed: {short}", if n == 1 { "" } else { "s" });
        let hooks = summary(&out);
        if !hooks.is_empty() {
            msg = format!("{hooks}\n{msg}");
        }
        Ok(msg)
    })
}

/// Open the Push dialog for the current branch.
pub fn open_push_dialog(s: &mut Session) {
    let Some(branch) = s.vcs.branch_name().map(str::to_string) else {
        s.status = "Push needs a branch (HEAD is detached)".into();
        return;
    };
    let (remote, remote_branch, set_upstream) =
        match s.vcs.status.branch.upstream.as_deref().and_then(|u| u.split_once('/')) {
            Some((r, b)) => (r.to_string(), b.to_string(), false),
            None => (
                s.vcs
                    .remotes
                    .iter()
                    .find(|r| r.name == "origin")
                    .or(s.vcs.remotes.first())
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
                branch.clone(),
                true,
            ),
        };
    let upstream = s.vcs.status.branch.upstream.clone();
    s.vcs.dialog =
        Some(Dialog::Push { remote, branch: branch.clone(), remote_branch, set_upstream, force: false, commits: None });
    s.vcs.dialog_error = None;
    load_outgoing(s, &branch, upstream);
}

pub fn push(s: &mut Session, req: PushRequest) -> Result<(), CommandError> {
    s.vcs.dialog = None;
    let label = format!("Push {} to {}", req.branch, req.remote);
    start_op(s, &label, OpKind::Plain, move |git| {
        let out = codecraft_vcs::ops::push(git, &req)?;
        let text = summary(&out);
        Ok(if text.is_empty() { "Pushed".into() } else { text })
    })
}

pub fn pull(s: &mut Session, req: PullRequest, label: &str) -> Result<(), CommandError> {
    start_op(s, label, OpKind::MayConflict, move |git| {
        let out = codecraft_vcs::ops::pull(git, &req)?;
        Ok(summary(&out))
    })
}

pub fn fetch(s: &mut Session) -> Result<(), CommandError> {
    start_op(s, "Fetch", OpKind::Plain, |git| {
        codecraft_vcs::ops::fetch(git, None)?;
        Ok("Fetched all remotes".into())
    })
}

/// A simple operation: run `f` and report `done`.
pub(crate) fn simple_op<F>(s: &mut Session, label: &str, may_conflict: bool, f: F) -> Result<(), CommandError>
where
    F: FnOnce(&Git) -> Result<codecraft_vcs::Output, GitError> + Send + 'static,
{
    let kind = if may_conflict { OpKind::MayConflict } else { OpKind::Plain };
    let done = label.to_string();
    start_op(s, label, kind, move |git| {
        let out = f(git)?;
        let text = summary(&out);
        Ok(if text.is_empty() { format!("{done}: done") } else { text })
    })
}

pub fn rollback(s: &mut Session, files: &[String], delete_added: bool) -> Result<(), CommandError> {
    let root =
        s.vcs.repo.as_ref().map(|r| r.root.clone()).ok_or_else(|| CommandError::Failed("no repository".into()))?;
    let mut tracked = Vec::new();
    let mut added = Vec::new();
    for f in files {
        match s.vcs.status.file(f) {
            Some(st) if st.untracked || st.ignored => {}
            Some(st) if st.conflict.is_some() => tracked.push(f.clone()),
            Some(st) if !st.in_head() => {
                added.push(f.clone());
                if let Some(o) = &st.orig_path {
                    tracked.push(o.clone());
                }
            }
            Some(_) => tracked.push(f.clone()),
            None => {}
        }
    }
    if tracked.is_empty() && added.is_empty() {
        return Err(CommandError::Failed("Nothing to roll back".into()));
    }
    // Unsaved edits in rolled-back files would be written back over the restored content.
    for f in tracked.iter().chain(&added) {
        if let Some(abs) = s.vcs.abs(f)
            && let Some(id) = s.ws.doc_by_path(&abs)
            && let Some(od) = s.ws.doc_mut(id)
            && od.doc.is_modified()
        {
            od.doc.mark_saved();
        }
    }
    s.vcs.dialog = None;
    let n = tracked.len() + added.len();
    start_op(s, "Rollback", OpKind::Plain, move |git| {
        codecraft_vcs::ops::rollback(git, &root, &tracked, &added, delete_added)?;
        Ok(format!("{n} file{} rolled back", if n == 1 { "" } else { "s" }))
    })
}

pub fn create_patch(s: &mut Session, files: &[String], path: Option<PathBuf>) -> Result<(), CommandError> {
    let mut tracked = Vec::new();
    let mut untracked = Vec::new();
    for f in files {
        match s.vcs.status.file(f) {
            Some(st) if st.untracked => untracked.push(f.clone()),
            Some(_) => tracked.push(f.clone()),
            None => {}
        }
    }
    if tracked.is_empty() && untracked.is_empty() {
        return Err(CommandError::Failed("There are no local changes to put in a patch".into()));
    }
    s.vcs.dialog = None;
    let clipboard = path.is_none();
    start_op(s, "Create Patch", OpKind::Patch { clipboard }, move |git| {
        let patch = codecraft_vcs::ops::create_patch(git, &tracked, &untracked)?;
        match path {
            Some(p) => {
                std::fs::write(&p, &patch).map_err(|e| GitError::Io(format!("{}: {e}", p.display())))?;
                Ok(format!("Patch saved to {}", p.display()))
            }
            None => Ok(patch),
        }
    })
}

pub fn clone_repo(s: &mut Session, url: String, target: PathBuf, open: bool) -> Result<(), CommandError> {
    if target.exists() && std::fs::read_dir(&target).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(CommandError::Failed(format!("{} already exists and is not empty", target.display())));
    }
    let t = target.clone();
    start_op(s, "Clone", OpKind::Clone { target, open }, move |git| {
        codecraft_vcs::ops::clone(git, &url, &t)?;
        Ok(format!("Cloned into {}", t.display()))
    })
}

/// Show the Git tool window on a tab.
pub fn show_git_window(s: &mut Session, tab: GitTab) {
    s.vcs.git_tab = tab;
    s.ws.layout.show(ToolWindowId::Git);
    if tab == GitTab::Log && !s.vcs.log.loaded {
        load_log(s, true);
    }
}

// ---------------------------------------------------------------- hunk staging, merge, worktrees, GitHub

impl VcsState {
    /// The installed `gh` (GitHub CLI), if any. Looks on `PATH`; starts no process.
    pub fn gh_path(&self) -> Option<PathBuf> {
        codecraft_vcs::github::find_program(&self.gh_program, std::env::var_os("PATH").as_deref())
    }

    /// The first remote on github.com.
    pub fn github_remote(&self) -> Option<&Remote> {
        self.remotes.iter().find(|r| {
            codecraft_vcs::github::is_github_url(&r.fetch_url) || codecraft_vcs::github::is_github_url(&r.push_url)
        })
    }

    pub(crate) fn gh_in(&self, program: PathBuf, dir: &Path) -> codecraft_vcs::github::Gh {
        let mut gh = codecraft_vcs::github::Gh::new(program, dir);
        for (k, v) in &self.git_env {
            gh = gh.with_env(k, v);
        }
        gh
    }
}

/// Stage (or unstage) the change at `line` of a document: only those lines go into (or out of)
/// the index; the work tree and the rest of the file are untouched.
pub fn stage_hunk(s: &mut Session, doc: DocId, line: usize, stage: bool) -> Result<(), CommandError> {
    let change = change_at(s, doc, line).ok_or_else(|| CommandError::Failed("no change on this line".into()))?;
    let od = s.ws.doc(doc).ok_or_else(|| CommandError::Failed("the document is closed".into()))?;
    let path = od.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved".into()))?;
    let text = od.doc.to_disk_text();
    let rel = s.vcs.rel(&path).ok_or_else(|| CommandError::Failed("the file is outside the repository".into()))?;
    let label = if stage { "Stage Hunk" } else { "Unstage Hunk" };
    start_op(s, label, OpKind::Hunk, move |git| {
        let done = if stage {
            codecraft_vcs::hunks::stage_hunk(git, &rel, &text, change.start..change.end)?
        } else {
            codecraft_vcs::hunks::unstage_hunk(git, &rel, change.old_start..change.old_end)?
        };
        if !done {
            let why = if stage { "These lines are already staged" } else { "These lines are not staged" };
            return Err(GitError::Invalid(why.into()));
        }
        let n = change.end.saturating_sub(change.start).max(1);
        Ok(format!("{n} line{} of {rel} {}", if n == 1 { "" } else { "s" }, if stage { "staged" } else { "unstaged" }))
    })
}

/// Load the three versions of a conflicted file and open the merge view when they arrive.
pub fn open_merge(s: &mut Session, rel: String) -> Result<(), CommandError> {
    let git = s.vcs.git().ok_or_else(|| CommandError::Failed("the project is not a Git repository".into()))?;
    if s.vcs.status.file(&rel).and_then(|f| f.conflict).is_none() {
        return Err(CommandError::Failed(format!("{rel} has no conflict")));
    }
    let theirs = match s.vcs.state {
        RepoState::Merging => "MERGE_HEAD",
        RepoState::Rebasing => "REBASE_HEAD",
        RepoState::CherryPicking => "CHERRY_PICK_HEAD",
        RepoState::Reverting => "REVERT_HEAD",
        RepoState::Normal => "theirs",
    };
    let labels = ("HEAD".to_string(), theirs.to_string());
    s.vcs.merge_loading = Some(rel.clone());
    s.vcs.dialog_error = None;
    let tx = s.vcs.tx.clone();
    spawn("merge", move || {
        let result = codecraft_vcs::ops::conflict_versions(&git, &rel);
        let _ = tx.send(Event::MergeLoaded { rel, labels, result });
    });
    Ok(())
}

/// Reload the work tree list in the background.
pub fn load_worktrees(s: &mut Session) {
    let Some(git) = s.vcs.git() else { return };
    s.vcs.worktrees_loading = true;
    let tx = s.vcs.tx.clone();
    spawn("worktrees", move || {
        let _ = tx.send(Event::Worktrees(codecraft_vcs::worktree::list(&git)));
    });
}

pub fn add_worktree(
    s: &mut Session,
    path: PathBuf,
    branch: String,
    new_branch: bool,
    base: Option<String>,
    open: bool,
) -> Result<(), CommandError> {
    let open_path = open.then(|| path.clone());
    start_op(s, "New Worktree", OpKind::Worktrees { open: open_path }, move |git| {
        codecraft_vcs::worktree::add(git, &path, &branch, new_branch, base.as_deref())?;
        Ok(format!("Work tree for {branch} created at {}", path.display()))
    })
}

pub fn remove_worktree(s: &mut Session, path: PathBuf, force: bool) -> Result<(), CommandError> {
    let main = s.vcs.worktrees.first().map(|w| w.path.clone());
    let root = s.vcs.repo.as_ref().map(|r| (r.root.clone(), r.toplevel.clone()));
    let is_current = root.is_some_and(|(a, b)| path == a || path == b);
    if main.as_deref() == Some(path.as_path()) || is_current {
        return Err(CommandError::Failed("The main or current work tree cannot be removed".into()));
    }
    start_op(s, "Remove Worktree", OpKind::Worktrees { open: None }, move |git| {
        codecraft_vcs::worktree::remove(git, &path, force)?;
        Ok(format!("Work tree {} removed", path.display()))
    })
}

/// Load the user's GitHub repositories (Clone dialog) through `gh`.
pub fn load_github_repos(s: &mut Session) -> Result<(), CommandError> {
    let program = s.vcs.gh_path().ok_or_else(|| CommandError::Failed(codecraft_vcs::github::GH_INSTALL_HINT.into()))?;
    let dir = s.project_root().or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(std::env::temp_dir);
    let gh = s.vcs.gh_in(program, &dir);
    s.vcs.github_loading = true;
    let tx = s.vcs.tx.clone();
    spawn("gh-repos", move || {
        let _ = tx.send(Event::GhRepos(codecraft_vcs::github::list_repos(&gh, 100)));
    });
    Ok(())
}

/// Push the current branch if the remote lacks commits, then `gh pr create`.
pub fn create_pull_request(s: &mut Session, mut req: PullRequestRequest, open: bool) -> Result<(), CommandError> {
    let program = s.vcs.gh_path().ok_or_else(|| CommandError::Failed(codecraft_vcs::github::GH_INSTALL_HINT.into()))?;
    let remote = s
        .vcs
        .github_remote()
        .map(|r| r.name.clone())
        .ok_or_else(|| CommandError::Failed("The repository has no GitHub remote (Git › Manage Remotes…)".into()))?;
    let branch =
        s.vcs.branch_name().map(str::to_string).ok_or_else(|| CommandError::Failed("Check out a branch first".into()))?;
    if req.title.trim().is_empty() {
        return Err(CommandError::BadParams("enter a title".into()));
    }
    req.head = branch.clone();
    let root = s.vcs.repo.as_ref().map(|r| r.root.clone()).unwrap_or_default();
    let gh = s.vcs.gh_in(program, &root);
    let upstream = s.vcs.status.branch.upstream.clone();
    let ahead = s.vcs.status.branch.ahead;
    start_op(s, "Create Pull Request", OpKind::PullRequest { open }, move |git| {
        if upstream.is_none() || ahead > 0 {
            let (r, rb) = match upstream.as_deref().and_then(|u| u.split_once('/')) {
                Some((r, b)) => (r.to_string(), b.to_string()),
                None => (remote, branch.clone()),
            };
            let push = PushRequest {
                remote: r,
                branch,
                remote_branch: Some(rb),
                set_upstream: upstream.is_none(),
                ..PushRequest::default()
            };
            codecraft_vcs::ops::push(git, &push)?;
        }
        codecraft_vcs::github::create_pull_request(&gh, &req)
    })
}

// ---------------------------------------------------------------- operations for other modules

/// Run a GitHub CLI action as the repository operation (one at a time, cancellable, reported in
/// the console and a notification). `f` gets the git invoker and captures its own `gh`.
pub(crate) fn start_gh_op<F>(
    s: &mut Session,
    label: &str,
    reload_prs: bool,
    open_url: bool,
    f: F,
) -> Result<(), CommandError>
where
    F: FnOnce(&Git) -> Result<String, GitError> + Send + 'static,
{
    start_op(s, label, OpKind::Gh { reload_prs, open_url }, f)
}

/// Share Project on GitHub: runs in `root`, which may not be a repository yet.
pub(crate) fn start_share_op<F>(s: &mut Session, root: PathBuf, f: F) -> Result<(), CommandError>
where
    F: FnOnce(&Git) -> Result<String, GitError> + Send + 'static,
{
    start_op(s, "Share Project on GitHub", OpKind::Share { root }, f)
}
