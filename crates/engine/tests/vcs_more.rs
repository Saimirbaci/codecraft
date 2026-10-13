//! Git follow-ups through engine commands, on temp repositories with an isolated identity:
//! per-hunk staging, the three-way merge view, work trees, and GitHub through a fake `gh`
//! (never the real one: `Session::vcs.gh_program` points at a script in the temp dir, and pushes
//! to the "GitHub" remote go to a local bare repository through `url.<x>.pushInsteadOf`).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::Dialog;
use codecraft_engine::vcs_git::FileState;

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

struct Env {
    _dir: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
             [commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        Self { _dir: dir, base, root, home }
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), self.home.display().to_string()),
            ("GIT_CONFIG_GLOBAL".into(), self.home.join(".gitconfig").display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).envs(self.env()).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn session(&self, files: &[(&str, &str)]) -> Session {
        self.git(&self.root, &["init", "-q", "-b", "main"]);
        for (f, t) in files {
            self.write(f, t);
        }
        self.git(&self.root, &["add", "-A"]);
        self.git(&self.root, &["commit", "-qm", "initial"]);
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.vcs.set_git_env(self.env());
        // Never the real GitHub CLI.
        s.vcs.gh_program = self.base.join("no-gh-here");
        s.execute("file.open", &json!({ "path": self.root })).unwrap();
        settle(&mut s, "repository", |s| s.vcs.repo.is_some() && s.vcs.status.branch.head.is_some());
        s
    }
}

fn settle(s: &mut Session, what: &str, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let console: Vec<String> = s.vcs.console.iter().map(|c| c.text.clone()).collect();
    panic!("timed out waiting for {what}; status: {}; console: {console:#?}", s.status);
}

fn finish_op(s: &mut Session) {
    settle(s, "operation", |s| s.vcs.op.is_none());
    std::thread::sleep(Duration::from_millis(50));
    settle(s, "refresh", |s| !s.vcs.busy() || s.vcs.op.is_none() && !s.vcs.log.loading);
    for _ in 0..5 {
        s.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn state(s: &Session, rel: &str) -> Option<FileState> {
    s.vcs.status.file(rel).map(|f| f.state())
}

#[test]
fn hunks_are_staged_and_unstaged_from_the_gutter() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.txt", "1\n2\n3\n4\n5\n6\n7\n8\n")]);
    s.execute("file.open", &json!({ "path": env.root.join("a.txt") })).unwrap();
    let doc = s.ws.active().unwrap().1.id;
    // Change line 2 and append a line: two markers.
    s.execute("editor.setSelection", &json!({ "anchor": 2, "head": 3 })).unwrap();
    s.execute("editor.type", &json!({ "text": "TWO" })).unwrap();
    let end = s.ws.active().unwrap().1.doc.len_chars();
    s.execute("editor.setSelection", &json!({ "anchor": end, "head": end })).unwrap();
    s.execute("editor.type", &json!({ "text": "nine\n" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    settle(&mut s, "markers", |s| s.vcs.markers(doc).len() == 2);
    // Stage the last change through its popup.
    s.execute("vcs.showHunk", &json!({ "line": 8 })).unwrap();
    s.execute("vcs.stageHunk", &Value::Null).unwrap();
    finish_op(&mut s);
    assert!(s.vcs.hunk.is_none(), "the popup closes");
    let cached = env.git(&env.root, &["diff", "--cached"]);
    assert!(cached.contains("+nine") && !cached.contains("TWO"), "{cached}");
    // Staging it again is reported, not silently ignored.
    s.execute("vcs.stageHunk", &json!({ "line": 8 })).unwrap();
    finish_op(&mut s);
    assert!(s.vcs.dialog_error.as_deref().unwrap_or("").contains("already staged"), "{:?}", s.vcs.dialog_error);
    // Stage line 2 by caret line, then unstage the appended line.
    s.execute("vcs.stageHunk", &json!({ "line": 1 })).unwrap();
    finish_op(&mut s);
    s.execute("vcs.unstageHunk", &json!({ "line": 8 })).unwrap();
    finish_op(&mut s);
    let cached = env.git(&env.root, &["diff", "--cached"]);
    assert!(cached.contains("+TWO") && !cached.contains("nine"), "{cached}");
    let unstaged = env.git(&env.root, &["diff"]);
    assert!(unstaged.contains("+nine") && !unstaged.contains("+TWO"), "{unstaged}");
    assert_eq!(state(&s, "a.txt"), Some(FileState::Modified));
    s.shutdown();
}

#[test]
fn conflicts_resolve_in_the_merge_view() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("c.txt", "head\nmiddle\nend\n")]);
    env.git(&env.root, &["checkout", "-qb", "feature"]);
    env.write("c.txt", "head\nTHEIRS\nend\nadded by them\n");
    env.git(&env.root, &["commit", "-qam", "theirs"]);
    env.git(&env.root, &["checkout", "-q", "main"]);
    env.write("c.txt", "HEAD\nOURS\nend\n");
    env.git(&env.root, &["commit", "-qam", "ours"]);
    s.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    finish_op(&mut s);
    s.execute("vcs.merge", &json!({ "branch": "feature" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "conflict", |s| state(s, "c.txt") == Some(FileState::Conflicted));
    s.execute("file.open", &json!({ "path": env.root.join("c.txt") })).unwrap();
    s.execute("vcs.mergeView", &json!({ "path": "c.txt" })).unwrap();
    settle(&mut s, "merge view", |s| s.vcs.merge.is_some());
    let st = s.execute("vcs.merge.state", &Value::Null).unwrap();
    let chunks = st["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 2, "{st}");
    assert_eq!(chunks[0]["kind"], "Conflict");
    assert_eq!(chunks[0]["oursText"], "HEAD\nOURS\n");
    assert_eq!(chunks[0]["theirsText"], "head\nTHEIRS\n");
    assert_eq!(chunks[1]["kind"], "Theirs");
    assert_eq!(st["unresolved"], 1);
    // Applying with a conflict left is refused.
    assert!(s.execute("vcs.merge.apply", &Value::Null).is_err());
    assert!(s.execute("vcs.merge.resolve", &json!({ "chunk": 7, "resolution": "ours" })).is_err());
    assert!(s.execute("vcs.merge.resolve", &json!({ "chunk": 0, "resolution": "sideways" })).is_err());
    s.execute("vcs.merge.resolve", &json!({ "chunk": 0, "resolution": "both" })).unwrap();
    let st = s.execute("vcs.merge.state", &Value::Null).unwrap();
    assert_eq!(st["unresolved"], 0);
    assert_eq!(st["result"], "HEAD\nOURS\nhead\nTHEIRS\nend\nadded by them\n");
    s.execute("vcs.merge.apply", &Value::Null).unwrap();
    finish_op(&mut s);
    assert!(s.vcs.merge.is_none());
    assert_eq!(std::fs::read_to_string(env.root.join("c.txt")).unwrap(), "HEAD\nOURS\nhead\nTHEIRS\nend\nadded by them\n");
    // The open editor shows the result and is not modified.
    let (_, d) = s.ws.active().unwrap();
    assert_eq!(d.doc.text(), "HEAD\nOURS\nhead\nTHEIRS\nend\nadded by them\n");
    assert!(!d.doc.is_modified());
    settle(&mut s, "resolved", |s| s.vcs.conflicts().is_empty());
    assert_eq!(state(&s, "c.txt"), Some(FileState::Modified));
    // A file without a conflict has no merge view.
    assert!(s.execute("vcs.mergeView", &json!({ "path": "c.txt" })).is_err());
    s.shutdown();
}

#[test]
fn worktrees_are_added_listed_opened_and_removed() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("f.txt", "1\n")]);
    s.execute("git.newWorktree", &json!({ "branch": "feature/x" })).unwrap();
    let path = match &s.vcs.dialog {
        Some(Dialog::NewWorktree { path, branch, new_branch, .. }) => {
            assert_eq!(branch, "feature/x");
            assert!(*new_branch);
            PathBuf::from(path)
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(path, env.base.join("project-feature-x"));
    s.execute("vcs.addWorktree", &Value::Null).unwrap();
    finish_op(&mut s);
    assert!(path.join("f.txt").is_file(), "{:?}", s.vcs.dialog_error);
    s.execute("git.worktrees", &Value::Null).unwrap();
    settle(&mut s, "worktrees", |s| s.vcs.worktrees.len() == 2 && !s.vcs.worktrees_loading);
    let list = s.execute("vcs.worktrees", &Value::Null).unwrap();
    let wts = list["worktrees"].as_array().unwrap();
    assert_eq!(wts[0]["main"], true);
    assert_eq!(wts[1]["branch"], "feature/x");
    // The main work tree can't be removed; bad branches are refused.
    assert!(s.execute("vcs.removeWorktree", &json!({ "path": env.root })).is_err());
    assert!(s.execute("vcs.addWorktree", &json!({ "path": env.base.join("w2"), "branch": "" })).is_err());
    s.execute("vcs.addWorktree", &json!({ "path": env.base.join("w2"), "branch": "-x" })).unwrap();
    finish_op(&mut s);
    assert!(!env.base.join("w2").exists());
    // Remove the linked one (it has a local change: force).
    std::fs::write(path.join("f.txt"), "dirty\n").unwrap();
    s.execute("vcs.removeWorktree", &json!({ "path": path })).unwrap();
    finish_op(&mut s);
    assert!(path.exists(), "a dirty work tree needs force");
    s.execute("vcs.removeWorktree", &json!({ "path": path, "force": true })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "removed", |s| s.vcs.worktrees.len() == 1 && !s.vcs.worktrees_loading);
    assert!(!path.exists());
    // Open a new one as the project.
    s.execute("vcs.addWorktree", &json!({ "path": env.base.join("w3"), "branch": "w3", "open": true })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "opened", |s| s.ws.project.as_ref().is_some_and(|p| p.root.ends_with("w3")));
    s.shutdown();
}

#[cfg(unix)]
fn fake_gh(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-gh");
    let log = dir.join("gh.log");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n\
             case \"$1 $2\" in\n\
             'pr create') echo 'https://github.com/o/r/pull/42' ;;\n\
             'repo list') echo '[{{\"nameWithOwner\":\"o/r\",\"url\":\"https://github.com/o/r\",\"description\":null}}]' ;;\n\
             *) exit 1 ;;\n\
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
fn github_pull_requests_and_repositories_through_a_fake_gh() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("f.txt", "1\n")]);
    // Without gh: the dialog shows the install hint and the command fails with it.
    s.execute("git.gitHub.createPullRequest", &Value::Null).unwrap();
    assert!(s.vcs.dialog_error.as_deref().unwrap_or("").contains("brew install gh"));
    assert!(s.execute("vcs.github.repositories", &json!({ "fetch": true })).is_err());
    assert!(s.execute("vcs.createPullRequest", &json!({ "title": "x" })).is_err());
    let st = s.execute("vcs.github.status", &Value::Null).unwrap();
    assert!(st["gh"].is_null() && st["hint"].as_str().unwrap().contains("gh auth login"));
    // A "GitHub" remote that really is a local bare repository.
    let bare = env.base.join("remote.git");
    env.git(&env.base, &["init", "-q", "--bare", "-b", "main", &bare.display().to_string()]);
    env.git(
        &env.root,
        &["config", "--global", &format!("url.{}.pushInsteadOf", bare.display()), "https://github.com/o/r.git"],
    );
    env.git(&env.root, &["remote", "add", "origin", "https://github.com/o/r.git"]);
    s.vcs.gh_program = fake_gh(&env.base);
    s.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    settle(&mut s, "remote", |s| s.vcs.github_remote().is_some());
    env.git(&env.root, &["checkout", "-qb", "topic"]);
    env.write("f.txt", "2\n");
    env.git(&env.root, &["commit", "-qam", "Topic change"]);
    s.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    settle(&mut s, "on topic", |s| s.vcs.branch_name() == Some("topic"));
    // Hostile or missing params never reach gh.
    assert!(s.execute("vcs.createPullRequest", &Value::Null).is_err());
    assert!(s.execute("vcs.createPullRequest", &json!({ "title": "  " })).is_err());
    assert!(!env.base.join("gh.log").exists());
    s.execute("vcs.createPullRequest", &json!({ "title": "Topic", "body": "Why", "base": "main", "draft": true }))
        .unwrap();
    finish_op(&mut s);
    assert_eq!(s.vcs.last_pull_request.as_deref(), Some("https://github.com/o/r/pull/42"), "{:?}", s.vcs.dialog_error);
    // The branch was pushed first (to the local bare repository).
    let pushed = env.git(&bare, &["rev-parse", "refs/heads/topic"]);
    assert_eq!(pushed.trim(), env.git(&env.root, &["rev-parse", "HEAD"]).trim());
    let log = std::fs::read_to_string(env.base.join("gh.log")).unwrap();
    assert!(log.contains("pr\ncreate\n--title\nTopic\n--body\nWhy\n--head\ntopic\n--base\nmain\n--draft"), "{log}");
    // Repositories for the Clone dialog.
    s.execute("git.gitHub.cloneRepository", &Value::Null).unwrap();
    assert!(s.vcs.clone_github);
    let r = s.execute("vcs.github.repositories", &Value::Null).unwrap();
    assert_eq!(r["repositories"], json!([]), "nothing is fetched without fetch: true");
    s.execute("vcs.github.repositories", &json!({ "fetch": true })).unwrap();
    settle(&mut s, "repositories", |s| s.vcs.github_repos.is_some());
    let r = s.execute("vcs.github.repositories", &Value::Null).unwrap();
    assert_eq!(r["repositories"][0]["nameWithOwner"], "o/r");
    s.execute("file.new.projectFromVersionControl", &json!({ "url": "o/r" })).unwrap();
    assert!(matches!(&s.vcs.dialog, Some(Dialog::Clone { url, .. }) if url == "o/r"));
    s.shutdown();
}
