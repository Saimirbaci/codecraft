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
