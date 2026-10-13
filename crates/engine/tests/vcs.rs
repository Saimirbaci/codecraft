//! Git integration through engine commands, on temp repositories with an isolated identity:
//! status and gutter markers, hunk rollback, commit, branches, conflicts, push/pull with a bare
//! remote, stash, blame, log, and hostile params on every Git command.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::vcs::{ConsoleKind, Dialog};
use codecraft_engine::vcs_git::{FileState, LineChangeKind};
use codecraft_engine::{CommandError, Session, registry};

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

struct Env {
    _dir: tempfile::TempDir,
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
        Self { _dir: dir, root, home }
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

    /// A repository with one commit, opened as the project.
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

/// Wait for the running operation and the refresh after it.
fn finish_op(s: &mut Session) {
    settle(s, "operation", |s| s.vcs.op.is_none());
    // The refresh after an operation.
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
fn markers_follow_edits_and_hunks_roll_back() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("src/a.rs", "one\ntwo\nthree\nfour\n")]);
    let file = env.root.join("src/a.rs");
    s.execute("file.open", &json!({ "path": file })).unwrap();
    let doc = s.ws.active().unwrap().1.id;
    settle(&mut s, "no markers", |s| s.vcs.markers(doc).is_empty() && !s.vcs.busy());
    // Modify line 2 and add a line after line 4: two markers.
    s.execute("editor.setSelection", &json!({ "anchor": 4, "head": 7 })).unwrap();
    s.execute("editor.type", &json!({ "text": "TWO" })).unwrap();
    let end = s.ws.active().unwrap().1.doc.len_chars();
    s.execute("editor.setSelection", &json!({ "anchor": end, "head": end })).unwrap();
    s.execute("editor.type", &json!({ "text": "five\n" })).unwrap();
    settle(&mut s, "markers", |s| s.vcs.markers(doc).len() == 2);
    let m = s.vcs.markers(doc).to_vec();
    assert_eq!((m[0].kind, m[0].start, m[0].end), (LineChangeKind::Modified, 1, 2));
    assert_eq!((m[1].kind, m[1].start, m[1].end), (LineChangeKind::Added, 4, 5));
    // The popup shows the old content; rolling back restores it (undoably).
    s.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    assert_eq!(s.vcs.hunk.as_ref().unwrap().old_text, "two");
    assert_eq!(s.vcs.hunk.as_ref().unwrap().new_text, "TWO");
    s.execute("vcs.rollbackHunk", &Value::Null).unwrap();
    assert!(s.vcs.hunk.is_none());
    assert_eq!(s.ws.active().unwrap().1.doc.text(), "one\ntwo\nthree\nfour\nfive\n");
    settle(&mut s, "one marker", |s| s.vcs.markers(doc).len() == 1);
    // Next/previous change navigation.
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
    s.execute("vcs.nextChange", &Value::Null).unwrap();
    let (e, d) = s.ws.active().unwrap();
    assert_eq!(d.doc.char_to_line(e.selections.primary().head), 4);
    s.execute("vcs.rollbackHunk", &json!({ "line": 4 })).unwrap();
    assert_eq!(s.ws.active().unwrap().1.doc.text(), "one\ntwo\nthree\nfour\n");
    settle(&mut s, "clean", |s| s.vcs.markers(doc).is_empty());
    s.execute("edit.undo", &Value::Null).unwrap();
    assert!(s.ws.active().unwrap().1.doc.text().ends_with("five\n"));
    s.execute("edit.redo", &Value::Null).unwrap();
    // Deleting the first line shows a deletion marker.
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
    s.execute("editor.deleteLine", &Value::Null).unwrap();
    assert_eq!(s.ws.active().unwrap().1.doc.text(), "two\nthree\nfour\n");
    settle(&mut s, "deletion marker", |s| {
        s.vcs.markers(doc).iter().any(|c| c.kind == LineChangeKind::Deleted && c.start == 0)
    });
    s.shutdown();
}

#[test]
fn commit_flow_with_unversioned_files_and_log() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.txt", "a\n"), ("b.txt", "b\n"), (".gitignore", "target/\n")]);
    env.write("a.txt", "a2\n");
    env.write("b.txt", "b2\n");
    env.write("new.txt", "new\n");
    env.write("target/out.bin", "x");
    s.execute("vcs.refresh", &Value::Null).unwrap();
    settle(&mut s, "status", |s| s.vcs.status.files.len() >= 4);
    assert_eq!(state(&s, "a.txt"), Some(FileState::Modified));
    assert_eq!(state(&s, "new.txt"), Some(FileState::Untracked));
    assert_eq!(s.vcs.file_state(&env.root.join("target/out.bin")), Some(FileState::Ignored));
    assert_eq!(s.vcs.dir_state(&env.root.join("target")), Some(FileState::Ignored));
    assert_eq!(s.vcs.file_state(&env.root.join("a.txt")), Some(FileState::Modified));
    // Defaults: tracked changes included, unversioned not.
    assert_eq!(s.vcs.included_paths(), vec!["a.txt".to_string(), "b.txt".to_string()]);
    s.execute("vcs.include", &json!({ "path": "b.txt", "included": false })).unwrap();
    s.execute("vcs.include", &json!({ "path": env.root.join("new.txt"), "included": true })).unwrap();
    s.execute("vcs.selectChange", &json!({ "path": "a.txt" })).unwrap();
    settle(&mut s, "preview", |s| s.vcs.preview.as_ref().is_some_and(|p| !p.loading));
    let p = s.vcs.preview.clone().unwrap();
    assert_eq!((p.old.as_str(), p.new.as_str()), ("a\n", "a2\n"));
    // No message: refused.
    assert!(s.execute("vcs.commit", &Value::Null).is_err());
    s.execute("git.commit", &Value::Null).unwrap();
    assert!(s.vcs.focus_message);
    s.execute("vcs.commit", &json!({ "message": "Change a, add new" })).unwrap();
    assert!(s.vcs.op.is_some());
    // Only one operation at a time.
    assert!(s.execute("vcs.commit", &json!({ "message": "again" })).is_err());
    finish_op(&mut s);
    settle(&mut s, "clean a.txt", |s| state(s, "a.txt").is_none() && state(s, "new.txt").is_none());
    assert_eq!(state(&s, "b.txt"), Some(FileState::Modified));
    assert!(s.vcs.commit.message.is_empty());
    assert!(s.vcs.console.iter().any(|c| c.kind == ConsoleKind::Command && c.text.starts_with("git commit")));
    // The log shows it with its details.
    s.execute("git.showGitLog", &Value::Null).unwrap();
    settle(&mut s, "log", |s| s.vcs.log.commits.len() == 2 && !s.vcs.log.loading);
    assert_eq!(s.vcs.log.commits[0].subject, "Change a, add new");
    assert_eq!(s.vcs.log.graph.len(), 2);
    let hash = s.vcs.log.commits[0].hash.clone();
    s.execute("vcs.selectCommit", &json!({ "hash": hash })).unwrap();
    settle(&mut s, "details", |s| s.vcs.log.details.is_some());
    assert_eq!(s.vcs.log.details.as_ref().unwrap().files.len(), 2);
    s.execute("vcs.selectCommitFile", &json!({ "path": "a.txt" })).unwrap();
    settle(&mut s, "commit diff", |s| s.vcs.log.file_diff.as_ref().is_some_and(|d| !d.loading));
    assert_eq!(s.vcs.log.file_diff.as_ref().unwrap().new, "a2\n");
    s.execute("vcs.log", &json!({ "text": "initial" })).unwrap();
    settle(&mut s, "filtered log", |s| s.vcs.log.commits.len() == 1 && !s.vcs.log.loading);
    // Amend fills the message from the last commit.
    s.execute("vcs.setAmend", &json!({ "amend": true })).unwrap();
    assert_eq!(s.vcs.commit.message, "Change a, add new");
    // Status JSON for agents.
    let st = s.execute("vcs.status", &Value::Null).unwrap();
    assert_eq!(st["branch"], "main");
    assert!(st["files"].as_array().unwrap().iter().any(|f| f["path"] == "b.txt" && f["state"] == "modified"));
    // Rollback (dialog, then confirm).
    s.execute("git.uncommittedChanges.rollback", &json!({ "files": ["b.txt"] })).unwrap();
    assert!(matches!(s.vcs.dialog, Some(Dialog::Rollback { .. })));
    s.execute("vcs.rollback", &Value::Null).unwrap();
    finish_op(&mut s);
    assert_eq!(std::fs::read_to_string(env.root.join("b.txt")).unwrap(), "b\n");
    s.shutdown();
}

#[test]
fn branches_conflicts_and_stash() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("c.txt", "base\n")]);
    s.execute("git.newBranch", &Value::Null).unwrap();
    if let Some(Dialog::NewBranch { name, .. }) = &mut s.vcs.dialog {
        *name = "feature".into();
    }
    s.execute("vcs.newBranch", &Value::Null).unwrap();
    finish_op(&mut s);
    settle(&mut s, "on feature", |s| s.vcs.branch_name() == Some("feature"));
    assert!(s.vcs.branches.iter().any(|b| b.name == "feature" && b.current));
    env.write("c.txt", "theirs\n");
    s.execute("vcs.commit", &json!({ "message": "theirs", "files": ["c.txt"] })).unwrap();
    finish_op(&mut s);
    s.execute("vcs.checkout", &json!({ "branch": "main" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "on main", |s| s.vcs.branch_name() == Some("main"));
    env.write("c.txt", "ours\n");
    s.execute("vcs.commit", &json!({ "message": "ours", "files": ["c.txt"] })).unwrap();
    finish_op(&mut s);
    // Merge conflicts: the conflicts dialog opens; accept yours, then continue.
    s.execute("vcs.merge", &json!({ "branch": "feature" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "conflict", |s| state(s, "c.txt") == Some(FileState::Conflicted));
    settle(&mut s, "conflicts dialog", |s| matches!(s.vcs.dialog, Some(Dialog::Conflicts { .. })));
    assert_eq!(s.vcs.state, codecraft_engine::vcs_git::RepoState::Merging);
    s.execute("vcs.acceptYours", &json!({ "path": "c.txt" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "resolved", |s| s.vcs.conflicts().is_empty());
    s.execute("vcs.continueOperation", &Value::Null).unwrap();
    finish_op(&mut s);
    settle(&mut s, "merged", |s| s.vcs.state == codecraft_engine::vcs_git::RepoState::Normal);
    assert_eq!(std::fs::read_to_string(env.root.join("c.txt")).unwrap(), "ours\n");
    // Stash and unstash.
    env.write("c.txt", "wip\n");
    s.execute("vcs.refresh", &Value::Null).unwrap();
    settle(&mut s, "modified", |s| state(s, "c.txt") == Some(FileState::Modified));
    s.execute("vcs.stash", &json!({ "message": "my wip" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "stashed", |s| s.vcs.stashes.len() == 1 && state(s, "c.txt").is_none());
    s.execute("git.uncommittedChanges.unstashChanges", &Value::Null).unwrap();
    s.execute("vcs.unstash", &Value::Null).unwrap();
    finish_op(&mut s);
    settle(&mut s, "unstashed", |s| s.vcs.stashes.is_empty() && state(s, "c.txt") == Some(FileState::Modified));
    // Tags, rename and delete.
    s.execute("vcs.newTag", &json!({ "name": "v1" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "tag", |s| s.vcs.tags.iter().any(|t| t.name == "v1"));
    s.execute("vcs.renameBranch", &json!({ "old": "feature", "new": "done" })).unwrap();
    finish_op(&mut s);
    s.execute("vcs.deleteBranch", &json!({ "name": "done" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "deleted", |s| !s.vcs.branches.iter().any(|b| b.name == "done" || b.name == "feature"));
    assert!(s.execute("vcs.newBranch", &json!({ "name": "bad name" })).is_err());
    s.shutdown();
}

#[test]
fn push_and_update_project_with_a_bare_remote() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let remote = env.home.parent().unwrap().join("remote.git");
    env.git(env.home.parent().unwrap(), &["init", "-q", "--bare", "-b", "main", &remote.display().to_string()]);
    let mut s = env.session(&[("f.txt", "1\n")]);
    s.execute("vcs.addRemote", &json!({ "name": "origin", "url": remote.display().to_string() })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "remote", |s| s.vcs.remotes.len() == 1);
    // Update Project without an upstream is refused with a clear message.
    let err = s.execute("git.updateProject", &Value::Null).unwrap_err();
    assert!(err.to_string().contains("tracked branch"), "{err}");
    s.execute("git.push", &Value::Null).unwrap();
    settle(
        &mut s,
        "outgoing",
        |s| matches!(&s.vcs.dialog, Some(Dialog::Push { commits: Some(c), .. }) if c.len() == 1),
    );
    s.execute("vcs.push", &Value::Null).unwrap();
    finish_op(&mut s);
    settle(&mut s, "upstream", |s| s.vcs.status.branch.upstream.as_deref() == Some("origin/main"));
    // Someone else pushes; Update Project brings it in.
    let other = env.home.parent().unwrap().join("other");
    env.git(env.home.parent().unwrap(), &["clone", "-q", &remote.display().to_string(), &other.display().to_string()]);
    std::fs::write(other.join("f.txt"), "2\n").unwrap();
    env.git(&other, &["commit", "-qam", "from other"]);
    env.git(&other, &["push", "-q"]);
    s.execute("git.fetch", &Value::Null).unwrap();
    finish_op(&mut s);
    settle(&mut s, "behind", |s| s.vcs.status.branch.behind == 1);
    s.execute("git.updateProject", &Value::Null).unwrap();
    finish_op(&mut s);
    settle(&mut s, "updated", |s| s.vcs.status.branch.behind == 0);
    assert_eq!(std::fs::read_to_string(env.root.join("f.txt")).unwrap(), "2\n");
    assert!(s.vcs.console.iter().any(|c| c.text.starts_with("git pull")));
    s.shutdown();
}

#[test]
fn annotate_and_compare_the_current_file() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.txt", "one\ntwo\n")]);
    s.execute("file.open", &json!({ "path": env.root.join("a.txt") })).unwrap();
    let doc = s.ws.active().unwrap().1.id;
    s.execute("git.currentFile.annotate", &Value::Null).unwrap();
    settle(&mut s, "blame", |s| s.vcs.blame_for(doc).is_some_and(|b| b.blame.is_some()));
    let b = s.vcs.blame_for(doc).unwrap().blame.clone().unwrap();
    assert_eq!(b.lines.len(), 2);
    assert_eq!(b.line(0).unwrap().summary, "initial");
    s.execute("git.currentFile.annotate", &Value::Null).unwrap();
    assert!(s.vcs.blame_for(doc).is_none());
    s.execute("editor.type", &json!({ "text": "zero\n" })).unwrap();
    s.execute("git.currentFile.compareWithRevision", &json!({ "rev": "HEAD" })).unwrap();
    settle(&mut s, "compare", |s| s.vcs.compare.as_ref().is_some_and(|c| !c.loading));
    let c = s.vcs.compare.clone().unwrap();
    assert_eq!(c.old, "one\ntwo\n");
    assert_eq!(c.new, "zero\none\ntwo\n");
    s.execute("git.currentFile.showHistory", &Value::Null).unwrap();
    assert_eq!(s.vcs.log.path.as_deref(), Some("a.txt"));
    s.shutdown();
}

#[test]
fn no_git_command_panics_in_a_repository() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.rs", "fn main() {}\n")]);
    s.execute("file.open", &json!({ "path": env.root.join("a.rs") })).unwrap();
    env.write("a.rs", "fn main() { changed }\n");
    let missing = env.root.join("missing").display().to_string();
    let hostile = [
        Value::Null,
        json!({}),
        json!([1]),
        json!("x"),
        json!({ "line": u64::MAX, "index": u64::MAX }),
        json!({ "path": missing, "files": [missing, 7], "hash": "zzzz", "branch": "-rf" }),
        json!({ "name": "", "url": "", "rev": "--hard", "mode": "sideways", "onto": "" }),
        json!({ "message": "\u{0}😀", "files": [], "amend": "yes" }),
        json!({ "text": "garbage patch", "path": "" }),
        json!({ "all": false }),
    ];
    for spec in registry().all().iter().filter(|c| c.id.starts_with("git.") || c.id.starts_with("vcs.")) {
        for params in &hostile {
            // Never clone from the network or into odd places.
            if spec.id == "vcs.clone" && params.get("url").is_some() {
                continue;
            }
            if let Err(CommandError::Failed(msg)) = s.execute(spec.id, params)
                && msg.starts_with("internal error")
            {
                panic!("{} panicked with {params}: {msg}", spec.id);
            }
            let start = Instant::now();
            while s.vcs.op.is_some() && start.elapsed() < Duration::from_secs(20) {
                s.pump();
                std::thread::sleep(Duration::from_millis(10));
            }
            s.pump();
        }
    }
    s.shutdown();
}

/// A one-shot HTTP server answering like Ollama's `/api/chat`; returns its URL and the request.
fn stub_ollama(reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let h = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = conn.read(&mut chunk).unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0usize);
                if body.len() >= len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let body = reply.to_string();
        let _ = write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        String::from_utf8_lossy(&buf).into_owned()
    });
    (url, h)
}

#[test]
fn generate_commit_message_with_ollama() {
    let curl = Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success());
    if !git_available() || !curl {
        return;
    }
    let e = Env::new();
    let mut s = e.session(&[("a.txt", "one\n")]);
    let (url, server) = stub_ollama(r#"{"message":{"role":"assistant","content":"Change a.txt\n"}}"#);
    s.execute("settings.set", &json!({ "key": "commit_ai_provider", "value": "ollama" })).unwrap();
    s.execute("settings.set", &json!({ "key": "ollama_url", "value": url })).unwrap();

    // Nothing included yet.
    e.write("a.txt", "two\n");
    settle(&mut s, "change", |s| s.vcs.status.file("a.txt").is_some());
    s.execute("vcs.generateMessage", &Value::Null).unwrap();
    assert!(s.vcs.generating);
    settle(&mut s, "message", |s| !s.vcs.generating);
    assert_eq!(s.vcs.commit.message, "Change a.txt");
    let request = server.join().unwrap();
    assert!(request.contains("POST /api/chat") && request.contains("+two"), "{request}");
}

#[test]
fn generate_commit_message_reports_a_missing_key() {
    if !git_available() {
        return;
    }
    let e = Env::new();
    let mut s = e.session(&[("a.txt", "one\n")]);
    e.write("a.txt", "two\n");
    settle(&mut s, "change", |s| s.vcs.status.file("a.txt").is_some());
    s.execute("settings.set", &json!({ "key": "commit_ai_provider", "value": "nope" })).unwrap();
    assert!(matches!(s.execute("vcs.generateMessage", &Value::Null), Err(CommandError::Failed(_))));
    assert!(!s.vcs.generating);
}
