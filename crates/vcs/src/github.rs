//! GitHub through the GitHub CLI (`gh`), when it is installed: the user's repositories for the
//! Clone dialog and Create Pull Request. `gh` keeps its own login (`gh auth login`); we never
//! handle tokens. Like git, `gh` never prompts (`GH_PROMPT_DISABLED`), so a missing login is an
//! error the user can act on.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::{GitError, Result};

/// What to tell the user when `gh` is not installed.
pub const GH_INSTALL_HINT: &str = "The GitHub CLI is not installed. Install it with `brew install gh` (or see \
                                   https://cli.github.com), then sign in with `gh auth login`.";

/// A `gh` invoker bound to a working directory.
#[derive(Clone, Debug)]
pub struct Gh {
    program: PathBuf,
    dir: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Gh {
    pub fn new(program: impl Into<PathBuf>, dir: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), dir: dir.into(), env: Vec::new() }
    }

    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Run `gh` and return its stdout.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(&self.dir)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            command.env(k, v);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let out = command.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::Invalid(GH_INSTALL_HINT.into())
            } else {
                GitError::Io(format!("cannot start gh: {e}"))
            }
        })?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let message = if stderr.is_empty() { stdout.trim().to_string() } else { stderr };
        let lower = message.to_lowercase();
        if lower.contains("gh auth login") || lower.contains("not logged in") || lower.contains("authentication") {
            return Err(GitError::AuthRequired(format!("{message}\nSign in with `gh auth login` in a terminal.")));
        }
        Err(GitError::Failed { command: "gh".into(), code: out.status.code(), message })
    }
}

/// Resolve `program` like a shell would: an explicit path is used if it is a file; a bare name
/// is looked up on `path_env` (`$PATH`). No process is started.
pub fn find_program(program: &Path, path_env: Option<&OsStr>) -> Option<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::split_paths(path_env?).map(|d| d.join(program)).find(|p| p.is_file())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRequest {
    pub title: String,
    pub body: String,
    /// The branch to merge into (default: the repository's default branch).
    pub base: Option<String>,
    /// The branch with the changes (pushed already).
    pub head: String,
    pub draft: bool,
}

/// `gh pr create`; returns the new pull request's URL.
pub fn create_pull_request(gh: &Gh, req: &PullRequestRequest) -> Result<String> {
    if req.title.trim().is_empty() {
        return Err(GitError::Invalid("enter a title for the pull request".into()));
    }
    if req.head.trim().is_empty() {
        return Err(GitError::Invalid("check out a branch first".into()));
    }
    let mut args: Vec<String> = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        req.title.trim().to_string(),
        "--body".into(),
        req.body.clone(),
        "--head".into(),
        req.head.trim().to_string(),
    ];
    if let Some(b) = req.base.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        args.push("--base".into());
        args.push(b.to_string());
    }
    if req.draft {
        args.push("--draft".into());
    }
    let out = gh.run(args)?;
    out.lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("https://") || l.starts_with("http://"))
        .map(str::to_string)
        .ok_or_else(|| GitError::Parse(format!("gh pr create printed no URL: {}", out.trim())))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The signed-in user's repositories (most recently pushed first, as `gh` lists them).
pub fn list_repos(gh: &Gh, limit: usize) -> Result<Vec<GhRepo>> {
    let limit = limit.clamp(1, 1000).to_string();
    let out = gh.run(["repo", "list", "--limit", &limit, "--json", "nameWithOwner,url,description"])?;
    serde_json::from_str(&out).map_err(|e| GitError::Parse(format!("gh repo list: {e}")))
}

/// `owner/repo` shorthand → its GitHub HTTPS URL; anything else unchanged (trimmed).
pub fn normalize_clone_url(input: &str) -> String {
    let s = input.trim();
    let ok = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    match s.split_once('/') {
        Some((owner, repo)) if ok(owner) && ok(repo) && !s.starts_with('.') && !Path::new(s).exists() => {
            format!("https://github.com/{owner}/{}.git", repo.strip_suffix(".git").unwrap_or(repo))
        }
        _ => s.to_string(),
    }
}

// ---------------------------------------------------------------- pull requests

/// A GitHub user as `gh` prints it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GhUser {
    pub login: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GhLabel {
    pub name: String,
}

/// A row of the pull request list (`gh pr list --json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PullRequestItem {
    pub number: u64,
    pub title: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub author: GhUser,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub is_draft: bool,
    pub labels: Vec<GhLabel>,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrReview {
    pub author: GhUser,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, …
    pub state: String,
    pub body: String,
    pub submitted_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrComment {
    pub author: GhUser,
    pub body: String,
    pub created_at: String,
}

/// One pull request in detail (`gh pr view --json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PullRequestDetails {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author: GhUser,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub is_draft: bool,
    pub labels: Vec<GhLabel>,
    pub url: String,
    pub files: Vec<PrFile>,
    pub reviews: Vec<PrReview>,
    pub comments: Vec<PrComment>,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    pub additions: u64,
    pub deletions: u64,
}

/// An inline review comment (REST `pulls/<n>/comments`), read-only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    /// The line in the new version (`None` when the comment is outdated).
    pub line: Option<u64>,
    pub original_line: Option<u64>,
    pub body: String,
    pub user: GhUser,
    pub created_at: String,
    pub diff_hunk: String,
    pub in_reply_to_id: Option<u64>,
}

/// Filters of the pull request list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrFilter {
    /// `open`, `closed`, `merged` or `all`.
    pub state: String,
    pub author: String,
    pub label: String,
    /// Free text (GitHub search syntax).
    pub search: String,
}

impl Default for PrFilter {
    fn default() -> Self {
        Self { state: "open".into(), author: String::new(), label: String::new(), search: String::new() }
    }
}

const PR_LIST_FIELDS: &str = "number,title,state,author,headRefName,baseRefName,isDraft,labels,createdAt,updatedAt,url";
const PR_VIEW_FIELDS: &str = "number,title,body,state,author,headRefName,baseRefName,isDraft,labels,url,files,reviews,comments,mergeable,additions,deletions";

fn parse<T: serde::de::DeserializeOwned>(what: &str, out: &str) -> Result<T> {
    serde_json::from_str(out).map_err(|e| GitError::Parse(format!("{what}: {e}")))
}

/// A filter value passed to `gh` must not look like an option.
fn arg_value(what: &str, v: &str) -> Result<Option<String>> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    if v.starts_with('-') || v.chars().any(char::is_control) {
        return Err(GitError::Invalid(format!("'{v}' is not a valid {what}")));
    }
    Ok(Some(v.to_string()))
}

/// `gh pr list` with filters.
pub fn list_pull_requests(gh: &Gh, filter: &PrFilter, limit: usize) -> Result<Vec<PullRequestItem>> {
    let state = match filter.state.trim() {
        "" => "open",
        s @ ("open" | "closed" | "merged" | "all") => s,
        other => return Err(GitError::Invalid(format!("unknown state '{other}' (open, closed, merged, all)"))),
    };
    let limit = limit.clamp(1, 1000).to_string();
    let mut args: Vec<String> =
        vec!["pr".into(), "list".into(), "--state".into(), state.into(), "--limit".into(), limit];
    for (flag, what, v) in
        [("--author", "author", &filter.author), ("--label", "label", &filter.label), ("--search", "search", &filter.search)]
    {
        if let Some(v) = arg_value(what, v)? {
            args.push(flag.into());
            args.push(v);
        }
    }
    args.push("--json".into());
    args.push(PR_LIST_FIELDS.into());
    parse("gh pr list", &gh.run(args)?)
}

/// `gh pr view <n>`.
pub fn pull_request(gh: &Gh, number: u64) -> Result<PullRequestDetails> {
    let n = number.to_string();
    parse("gh pr view", &gh.run(["pr", "view", &n, "--json", PR_VIEW_FIELDS])?)
}

/// `gh pr diff <n>`: the unified diff of the whole pull request.
pub fn pull_request_diff(gh: &Gh, number: u64) -> Result<String> {
    let n = number.to_string();
    gh.run(["pr", "diff", &n, "--color", "never"])
}

/// The inline review comments of a pull request (first 100).
pub fn review_comments(gh: &Gh, number: u64) -> Result<Vec<ReviewComment>> {
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments?per_page=100");
    parse("review comments", &gh.run(["api", &path])?)
}

/// Split a unified diff into (path, patch) per file, in order.
pub fn split_diff(diff: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for chunk in diff.split_inclusive('\n') {
        if let Some(rest) = chunk.strip_prefix("diff --git ") {
            // `a/<path> b/<path>`: take the b side (renames show the new name).
            let rest = rest.trim_end();
            let path = rest.rsplit_once(" b/").map(|(_, b)| b).unwrap_or(rest).to_string();
            out.push((path, String::new()));
        }
        if let Some((_, text)) = out.last_mut() {
            text.push_str(chunk);
        }
    }
    out
}

/// How `gh pr merge` merges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeMethod {
    #[default]
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "merge" => Some(Self::Merge),
            "squash" => Some(Self::Squash),
            "rebase" => Some(Self::Rebase),
            _ => None,
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Create a merge commit",
            Self::Squash => "Squash and merge",
            Self::Rebase => "Rebase and merge",
        }
    }
}

/// `gh pr merge <n>`.
pub fn merge_pull_request(gh: &Gh, number: u64, method: MergeMethod, delete_branch: bool) -> Result<String> {
    let n = number.to_string();
    let mut args = vec!["pr", "merge", &n, method.flag()];
    if delete_branch {
        args.push("--delete-branch");
    }
    gh.run(args)
}

/// `gh pr checkout <n>`: a local branch for the pull request, checked out.
pub fn checkout_pull_request(gh: &Gh, number: u64) -> Result<String> {
    let n = number.to_string();
    gh.run(["pr", "checkout", &n])
}

// ---------------------------------------------------------------- repositories, forks, gists

/// What Share Project on GitHub creates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRequest {
    pub name: String,
    pub description: String,
    pub private: bool,
    /// The remote to add (`origin`).
    pub remote: String,
}

/// Whether `name` is a valid GitHub repository name (`owner/name` allowed).
pub fn valid_repo_name(name: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty() && p.len() <= 100 && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && p != "." && p != ".."
    };
    match name.split_once('/') {
        Some((o, n)) => ok(o) && ok(n),
        None => ok(name),
    }
}

/// `gh repo create <name> --source <dir> --remote <remote> --push`; returns the repository URL.
pub fn create_repo(gh: &Gh, dir: &Path, req: &ShareRequest) -> Result<String> {
    let name = req.name.trim();
    if !valid_repo_name(name) {
        return Err(GitError::Invalid(format!("'{name}' is not a valid repository name")));
    }
    let remote = if req.remote.trim().is_empty() { "origin" } else { req.remote.trim() };
    if !crate::valid_ref_name(remote) || remote.contains('/') {
        return Err(GitError::Invalid(format!("'{remote}' is not a valid remote name")));
    }
    let mut args: Vec<OsString> = vec![
        "repo".into(),
        "create".into(),
        name.into(),
        if req.private { "--private" } else { "--public" }.into(),
        "--source".into(),
        dir.as_os_str().to_owned(),
        "--remote".into(),
        remote.into(),
        "--push".into(),
    ];
    if !req.description.trim().is_empty() {
        args.push("--description".into());
        args.push(req.description.trim().into());
    }
    let out = gh.run(args)?;
    Ok(out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://"))
        .map(str::to_string)
        .unwrap_or_else(|| out.trim().to_string()))
}

/// `owner/repo` of a GitHub remote URL.
pub fn repo_from_url(url: &str) -> Option<String> {
    let u = url.trim();
    let lower = u.to_lowercase();
    let rest = ["https://github.com/", "http://github.com/", "git@github.com:", "ssh://git@github.com/"]
        .iter()
        .find(|p| lower.starts_with(*p))
        .and_then(|p| u.get(p.len()..))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let full = format!("{owner}/{name}");
    valid_repo_name(&full).then_some(full)
}

/// `gh repo sync <owner/repo>`: bring the fork's default branch on GitHub up to date with its
/// parent.
pub fn sync_fork(gh: &Gh, repo: &str) -> Result<String> {
    if !valid_repo_name(repo) || !repo.contains('/') {
        return Err(GitError::Invalid(format!("'{repo}' is not an owner/repository name")));
    }
    gh.run(["repo", "sync", repo])
}

/// `gh gist create <file>`; returns the gist's URL.
pub fn create_gist(gh: &Gh, file: &Path, description: &str, public: bool) -> Result<String> {
    if !file.is_file() {
        return Err(GitError::Invalid(format!("{} is not a file", file.display())));
    }
    let mut args: Vec<OsString> = vec!["gist".into(), "create".into(), file.as_os_str().to_owned()];
    if !description.trim().is_empty() {
        args.push("--desc".into());
        args.push(description.trim().into());
    }
    if public {
        args.push("--public".into());
    }
    let out = gh.run(args)?;
    out.lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| GitError::Parse(format!("gh gist create printed no URL: {}", out.trim())))
}

/// Whether a remote URL points at github.com.
pub fn is_github_url(url: &str) -> bool {
    let u = url.trim().to_lowercase();
    u.starts_with("https://github.com/")
        || u.starts_with("http://github.com/")
        || u.starts_with("git@github.com:")
        || u.starts_with("ssh://git@github.com/")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A fake `gh` that logs its arguments and answers like the real one.
    #[cfg(unix)]
    fn fake_gh(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("gh");
        let log = dir.join("gh.log");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n\
                 case \"$1 $2\" in\n\
                 'pr create') echo 'Creating pull request'; echo 'https://github.com/o/r/pull/7' ;;\n\
                 'repo list') echo '[{{\"nameWithOwner\":\"o/r\",\"url\":\"https://github.com/o/r\",\"description\":\"d\"}}]' ;;\n\
                 'auth fail') echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4 ;;\n\
                 *) echo \"unknown $1\" >&2; exit 1 ;;\n\
                 esac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn pull_requests_and_repos_through_a_fake_gh() {
        let dir = tempfile::tempdir().unwrap();
        let gh = Gh::new(fake_gh(dir.path()), dir.path());
        let req = PullRequestRequest {
            title: "Add x".into(),
            body: "Body with 'quotes'".into(),
            base: Some("main".into()),
            head: "feature".into(),
            draft: true,
        };
        assert_eq!(create_pull_request(&gh, &req).unwrap(), "https://github.com/o/r/pull/7");
        let log = std::fs::read_to_string(dir.path().join("gh.log")).unwrap();
        assert!(log.contains("--title\nAdd x\n--body\nBody with 'quotes'\n--head\nfeature\n--base\nmain\n--draft"), "{log}");
        let repos = list_repos(&gh, 50).unwrap();
        assert_eq!(repos[0].name_with_owner, "o/r");
        assert!(matches!(gh.run(["auth", "fail"]), Err(GitError::AuthRequired(_))));
        assert!(matches!(gh.run(["nope"]), Err(GitError::Failed { .. })));
        assert!(matches!(
            create_pull_request(&gh, &PullRequestRequest { head: "x".into(), ..Default::default() }),
            Err(GitError::Invalid(_))
        ));
    }

    #[test]
    fn missing_gh_gives_the_install_hint() {
        let dir = tempfile::tempdir().unwrap();
        let gh = Gh::new(dir.path().join("no-such-gh"), dir.path());
        match gh.run(["--version"]) {
            Err(GitError::Invalid(m)) => assert!(m.contains("brew install gh")),
            other => panic!("{other:?}"),
        }
        assert!(find_program(&dir.path().join("no-such-gh"), None).is_none());
        assert!(find_program(Path::new("gh"), Some(dir.path().as_os_str())).is_none());
        std::fs::write(dir.path().join("gh"), "").unwrap();
        assert_eq!(find_program(Path::new("gh"), Some(dir.path().as_os_str())), Some(dir.path().join("gh")));
    }

    #[cfg(unix)]
    fn fake_pr_gh(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("gh");
        let log = dir.join("gh.log");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n\
                 case \"$1 $2\" in\n\
                 'pr list') echo '[{{\"number\":3,\"title\":\"Fix\",\"state\":\"OPEN\",\"author\":{{\"login\":\"ann\"}},\"headRefName\":\"fix\",\"baseRefName\":\"main\",\"isDraft\":false,\"labels\":[{{\"name\":\"bug\"}}],\"url\":\"https://github.com/o/r/pull/3\",\"extra\":1}}]' ;;\n\
                 'pr view') echo '{{\"number\":3,\"title\":\"Fix\",\"body\":\"B\",\"files\":[{{\"path\":\"a.rs\",\"additions\":1,\"deletions\":0}}],\"reviews\":[{{\"author\":{{\"login\":\"bob\"}},\"state\":\"APPROVED\",\"body\":\"ok\"}}],\"mergeable\":\"MERGEABLE\"}}' ;;\n\
                 'pr diff') printf 'diff --git a/a.rs b/a.rs\\n--- a/a.rs\\n+++ b/a.rs\\n@@ -1 +1,2 @@\\n x\\n+y\\ndiff --git a/b c.rs b/b c.rs\\n+z\\n' ;;\n\
                 'api repos/{{owner}}/{{repo}}/pulls/3/comments?per_page=100') echo '[{{\"id\":1,\"path\":\"a.rs\",\"line\":2,\"body\":\"nit\",\"user\":{{\"login\":\"bob\"}}}}]' ;;\n\
                 'pr merge'|'pr checkout') echo done ;;\n\
                 'repo create') echo 'https://github.com/me/new' ;;\n\
                 'repo sync') echo 'Synced' ;;\n\
                 'gist create') echo 'https://gist.github.com/me/1' ;;\n\
                 *) echo \"unknown $1\" >&2; exit 1 ;;\n\
                 esac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn pull_request_list_details_diff_comments_and_actions() {
        let dir = tempfile::tempdir().unwrap();
        let gh = Gh::new(fake_pr_gh(dir.path()), dir.path());
        let filter = PrFilter { author: "ann".into(), label: "bug".into(), ..PrFilter::default() };
        let list = list_pull_requests(&gh, &filter, 50).unwrap();
        assert_eq!((list[0].number, list[0].author.login.as_str(), list[0].labels[0].name.as_str()), (3, "ann", "bug"));
        let log = std::fs::read_to_string(dir.path().join("gh.log")).unwrap();
        assert!(log.contains("pr\nlist\n--state\nopen\n--limit\n50\n--author\nann\n--label\nbug\n--json"), "{log}");
        assert!(list_pull_requests(&gh, &PrFilter { state: "weird".into(), ..PrFilter::default() }, 5).is_err());
        assert!(list_pull_requests(&gh, &PrFilter { author: "--exec=x".into(), ..PrFilter::default() }, 5).is_err());
        let d = pull_request(&gh, 3).unwrap();
        assert_eq!((d.files[0].path.as_str(), d.reviews[0].state.as_str(), d.mergeable.as_str()), ("a.rs", "APPROVED", "MERGEABLE"));
        let diff = pull_request_diff(&gh, 3).unwrap();
        let files = split_diff(&diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "a.rs");
        assert!(files[0].1.ends_with("+y\n"));
        assert_eq!(files[1].0, "b c.rs");
        let c = review_comments(&gh, 3).unwrap();
        assert_eq!((c[0].path.as_str(), c[0].line, c[0].user.login.as_str()), ("a.rs", Some(2), "bob"));
        assert_eq!(merge_pull_request(&gh, 3, MergeMethod::Squash, true).unwrap().trim(), "done");
        assert_eq!(checkout_pull_request(&gh, 3).unwrap().trim(), "done");
        let log = std::fs::read_to_string(dir.path().join("gh.log")).unwrap();
        assert!(log.contains("pr\nmerge\n3\n--squash\n--delete-branch\n"), "{log}");
        let share = ShareRequest { name: "new".into(), description: "D".into(), private: true, remote: String::new() };
        assert_eq!(create_repo(&gh, dir.path(), &share).unwrap(), "https://github.com/me/new");
        let log = std::fs::read_to_string(dir.path().join("gh.log")).unwrap();
        assert!(log.contains("repo\ncreate\nnew\n--private\n--source\n"), "{log}");
        assert!(log.contains("--remote\norigin\n--push\n--description\nD\n"), "{log}");
        assert!(create_repo(&gh, dir.path(), &ShareRequest { name: "../x".into(), ..share.clone() }).is_err());
        assert!(create_repo(&gh, dir.path(), &ShareRequest { remote: "-x".into(), ..share }).is_err());
        assert_eq!(sync_fork(&gh, "me/r").unwrap().trim(), "Synced");
        assert!(sync_fork(&gh, "-x").is_err() && sync_fork(&gh, "noslash").is_err());
        let f = dir.path().join("snippet.rs");
        std::fs::write(&f, "fn x() {}\n").unwrap();
        assert_eq!(create_gist(&gh, &f, "d", true).unwrap(), "https://gist.github.com/me/1");
        assert!(create_gist(&gh, &dir.path().join("none"), "", false).is_err());
    }

    #[test]
    fn repository_names_and_urls() {
        assert_eq!(repo_from_url("git@github.com:me/r.git").as_deref(), Some("me/r"));
        assert_eq!(repo_from_url("https://github.com/me/r/").as_deref(), Some("me/r"));
        assert_eq!(repo_from_url("ssh://git@github.com/me/r.git").as_deref(), Some("me/r"));
        assert_eq!(repo_from_url("https://gitlab.com/me/r"), None);
        assert_eq!(repo_from_url("https://github.com/me"), None);
        assert!(valid_repo_name("my-repo.rs") && valid_repo_name("org/x"));
        assert!(!valid_repo_name("") && !valid_repo_name("a b") && !valid_repo_name("..") && !valid_repo_name("a/b/c"));
        assert_eq!(MergeMethod::parse("rebase"), Some(MergeMethod::Rebase));
        assert_eq!(MergeMethod::parse("x"), None);
        assert!(split_diff("").is_empty());
    }

    #[test]
    fn clone_urls() {
        assert_eq!(normalize_clone_url(" rust-lang/rust "), "https://github.com/rust-lang/rust.git");
        assert_eq!(normalize_clone_url("o/r.git"), "https://github.com/o/r.git");
        assert_eq!(normalize_clone_url("https://x.org/a/b"), "https://x.org/a/b");
        assert_eq!(normalize_clone_url("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(normalize_clone_url("../a/b"), "../a/b");
        assert_eq!(normalize_clone_url("a/b/c"), "a/b/c");
        assert!(is_github_url("git@github.com:o/r.git"));
        assert!(is_github_url("https://GitHub.com/o/r"));
        assert!(!is_github_url("https://gitlab.com/o/r"));
    }
}
