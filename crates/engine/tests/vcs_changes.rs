//! M13 track VC through engine commands, on temp repositories with an isolated identity:
//! changelists, shelve/unshelve, the editable merge result, the staged state in the change
//! popup, and GitHub through a fake `gh` (pull requests, Share Project, Sync Fork, gists). Never
//! the real GitHub CLI: `Session::vcs.gh_program` points at a script in the temp dir, and remotes
//! named after github.com resolve to local bare repositories through `url.<x>.insteadOf`.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::{CommitTab, Dialog, GitTab};
use codecraft_engine::vcs_git::FileState;
use codecraft_engine::vcs_git::hunks::StageState;

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

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap_or_default()
    }

    fn open(&self) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&self.base.join("cc-home"));
        s.vcs.set_git_env(self.env());
        // Never the real GitHub CLI.
        s.vcs.gh_program = self.base.join("no-gh-here");
        s.execute("file.open", &json!({ "path": self.root })).unwrap();
        s
    }

    fn session(&self, files: &[(&str, &str)]) -> Session {
        self.git(&self.root, &["init", "-q", "-b", "main"]);
        for (f, t) in files {
            self.write(f, t);
        }
        self.git(&self.root, &["add", "-A"]);
        self.git(&self.root, &["commit", "-qm", "initial"]);
        let mut s = self.open();
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
    panic!(
        "timed out waiting for {what}; status: {}; error: {:?}; console: {console:#?}",
        s.status, s.vcs.dialog_error
    );
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

fn refresh(s: &mut Session, what: &str, done: impl FnMut(&Session) -> bool) {
    s.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    settle(s, what, done);
}

fn list_files(st: &Value, name: &str) -> Vec<String> {
    st["lists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == name)
        .map(|l| l["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

#[test]
fn changelists_group_changes_and_commit_one_list() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    env.write("a.txt", "a2\n");
    env.write("b.txt", "b2\n");
    refresh(&mut s, "two changes", |s| s.vcs.status.files.len() == 2);
    let st = s.execute("vcs.changelists", &Value::Null).unwrap();
    assert_eq!(st["active"], "Changes");
    assert_eq!(list_files(&st, "Changes"), vec!["a.txt", "b.txt"]);
    // Only the default list: nothing is written to the project.
    assert!(!env.root.join(".codecraft/changelists.json").exists());
    // Without a name the dialog opens; bad names are refused.
    s.execute("vcs.changelist.new", &json!({ "files": ["b.txt"] })).unwrap();
    assert!(matches!(&s.vcs.dialog, Some(Dialog::ChangeList { original: None, files, .. }) if files == &["b.txt"]));
    assert!(s.execute("vcs.changelist.new", &json!({ "name": "  " })).is_err());
    assert!(s.vcs.dialog_error.is_some());
    s.execute("vcs.changelist.new", &json!({ "name": "Feature", "comment": "Feature work", "files": ["b.txt"] }))
        .unwrap();
    assert!(s.vcs.dialog.is_none());
    assert!(s.execute("vcs.changelist.new", &json!({ "name": "Feature" })).is_err(), "duplicate");
    // New changes go to the active list.
    s.execute("vcs.changelist.setActive", &json!({ "name": "Feature" })).unwrap();
    env.write("c.txt", "c2\n");
    refresh(&mut s, "three changes", |s| s.vcs.changes().len() == 3);
    let st = s.execute("vcs.changelists", &Value::Null).unwrap();
    assert_eq!(list_files(&st, "Changes"), vec!["a.txt"]);
    assert_eq!(list_files(&st, "Feature"), vec!["b.txt", "c.txt"]);
    assert!(env.root.join(".codecraft/changelists.json").exists());
    // Move a file by absolute path, then back.
    s.execute("vcs.changelist.move", &json!({ "paths": [env.root.join("c.txt")], "to": "Changes" })).unwrap();
    assert_eq!(s.vcs.changelists.list_of("c.txt"), Some("Changes"));
    s.execute("vcs.changelist.move", &json!({ "paths": ["c.txt"], "to": "Feature" })).unwrap();
    assert!(s.execute("vcs.changelist.move", &json!({ "paths": ["c.txt"], "to": "Nope" })).is_err());
    assert!(s.execute("vcs.changelist.move", &json!({ "to": "Feature" })).is_err());
    s.execute("vcs.changelist.move", &json!({ "paths": ["a.txt"], "to": "Scratch", "create": true })).unwrap();
    assert_eq!(s.vcs.changelists.list_of("a.txt"), Some("Scratch"));
    // Include one list: exactly its files are checked.
    let r = s.execute("vcs.changelist.include", &json!({ "name": "Feature" })).unwrap();
    assert_eq!(r["files"], json!(["b.txt", "c.txt"]));
    assert_eq!(s.vcs.included_paths(), vec!["b.txt".to_string(), "c.txt".to_string()]);
    // Commit only the Feature list; the message defaults to its comment.
    s.vcs.commit.message.clear();
    s.execute("vcs.changelist.commit", &json!({ "name": "Feature" })).unwrap();
    finish_op(&mut s);
    assert_eq!(env.git(&env.root, &["log", "-1", "--format=%s"]).trim(), "Feature work");
    assert_eq!(env.git(&env.root, &["show", "--name-only", "--format=", "HEAD"]).trim(), "b.txt\nc.txt");
    settle(&mut s, "committed files leave", |s| s.vcs.changelists.list_of("b.txt").is_none());
    assert_eq!(s.vcs.changelists.list_of("a.txt"), Some("Scratch"));
    assert!(s.execute("vcs.changelist.commit", &json!({ "name": "Feature" })).is_err(), "nothing left in it");
    // Edit: without params the dialog; rename keeps it active.
    s.execute("vcs.changelist.edit", &json!({ "name": "Feature" })).unwrap();
    assert!(
        matches!(&s.vcs.dialog, Some(Dialog::ChangeList { original: Some(o), active: true, .. }) if o == "Feature")
    );
    s.execute("vcs.changelist.edit", &json!({ "name": "Feature", "newName": "Done" })).unwrap();
    assert_eq!(s.vcs.changelists.active, "Done");
    assert!(s.execute("vcs.changelist.delete", &json!({ "name": "Done" })).is_err(), "active");
    s.execute("vcs.changelist.delete", &json!({ "name": "Scratch" })).unwrap();
    assert_eq!(s.vcs.changelists.list_of("a.txt"), Some("Done"), "files move to the active list");
    // Tools › Tasks & Contexts: the labels name the active task; Add Change List makes a list for it.
    assert_eq!(
        codecraft_engine::appearance::dynamic_label(&s, "tools.tasksContexts.addChangeListForDefaultTask").as_deref(),
        Some("Add Change List for 'Default task'")
    );
    s.execute("tools.tasksContexts.openTask", &json!({ "summary": "Fix bug 7" })).unwrap();
    assert_eq!(
        codecraft_engine::appearance::dynamic_label(&s, "tools.tasksContexts.editDefaultTask").as_deref(),
        Some("Edit 'Fix bug 7'")
    );
    let r = s.execute("tools.tasksContexts.addChangeListForDefaultTask", &Value::Null).unwrap();
    assert_eq!(r["name"], "Fix bug 7");
    assert_eq!(s.vcs.changelists.active, "Fix bug 7");
    assert_eq!(s.vcs.commit_tab, CommitTab::Changes);
    s.execute("tools.tasksContexts.addChangeListForDefaultTask", &Value::Null).unwrap();
    assert_eq!(
        s.vcs.changelists.lists.iter().filter(|l| l.name == "Fix bug 7").count(),
        1,
        "activated, not duplicated"
    );
    s.shutdown();
    // The lists survive a restart.
    let mut s2 = env.open();
    settle(&mut s2, "repository again", |s| s.vcs.repo.is_some() && !s.vcs.status.files.is_empty());
    assert_eq!(s2.vcs.changelists.active, "Fix bug 7");
    assert_eq!(s2.vcs.changelists.list_of("a.txt"), Some("Done"));
    s2.shutdown();
}

#[test]
fn shelve_and_unshelve_through_commands() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    env.write("a.txt", "a2\n");
    env.write("b.txt", "b2\n");
    env.write("new.txt", "fresh\n");
    refresh(&mut s, "changes", |s| s.vcs.status.files.len() == 3);
    // The menu item opens the dialog with the checked changes (unversioned files unchecked).
    s.execute("git.uncommittedChanges.shelveChanges", &Value::Null).unwrap();
    assert!(matches!(&s.vcs.dialog, Some(Dialog::Shelve { files, .. }) if files == &["a.txt", "b.txt"]));
    assert!(s.execute("vcs.shelve", &json!({ "files": [] })).is_err());
    s.execute("vcs.shelve", &json!({ "name": "WIP", "files": ["a.txt", "new.txt"] })).unwrap();
    finish_op(&mut s);
    assert!(s.vcs.dialog.is_none());
    assert_eq!(env.read("a.txt"), "a\n");
    assert!(!env.root.join("new.txt").exists());
    assert_eq!(env.read("b.txt"), "b2\n", "not shelved: untouched");
    settle(&mut s, "shelf listed", |s| s.vcs.shelves.len() == 1 && !s.vcs.shelves_loading);
    let list = s.execute("vcs.shelves", &Value::Null).unwrap();
    assert_eq!(list["shelves"][0]["name"], "WIP");
    assert_eq!(list["shelves"][0]["files"].as_array().unwrap().len(), 2);
    let id = list["shelves"][0]["id"].as_str().unwrap().to_string();
    // The shelf directory is invisible to git.
    refresh(&mut s, "only b", |s| s.vcs.changes().len() == 1 && s.vcs.unversioned().is_empty());
    assert_eq!(s.vcs.changes()[0].path, "b.txt");
    // Preview a shelved file's patch.
    s.execute("vcs.shelf.select", &json!({ "id": id, "path": "a.txt" })).unwrap();
    settle(&mut s, "patch", |s| s.vcs.shelf_patch.is_some());
    let patch = s.execute("vcs.shelf.patch", &Value::Null).unwrap();
    assert!(patch["patch"].as_str().unwrap().contains("+a2"), "{patch}");
    assert!(s.execute("vcs.shelf.select", &json!({ "id": id, "path": "zzz" })).is_err());
    assert!(s.execute("vcs.shelf.select", &json!({ "id": "../../etc" })).is_err());
    // Show Shelf.
    s.execute("git.uncommittedChanges.showShelf", &Value::Null).unwrap();
    assert_eq!(s.vcs.commit_tab, CommitTab::Shelf);
    // Unshelve one file; the other stays shelved.
    s.execute("vcs.unshelve", &json!({ "id": id, "files": ["a.txt"] })).unwrap();
    finish_op(&mut s);
    assert_eq!(env.read("a.txt"), "a2\n");
    settle(&mut s, "one left", |s| s.vcs.shelves.first().is_some_and(|sh| sh.files.len() == 1));
    s.execute("vcs.shelf.rename", &json!({ "id": id, "name": "Rest" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "renamed", |s| s.vcs.shelves.first().is_some_and(|sh| sh.name == "Rest"));
    // Unshelve the rest (keeping it), then delete it.
    s.execute("vcs.unshelve", &json!({ "id": id, "keep": true })).unwrap();
    finish_op(&mut s);
    assert_eq!(env.read("new.txt"), "fresh\n");
    settle(&mut s, "kept", |s| s.vcs.shelves.len() == 1);
    assert!(s.execute("vcs.unshelve", &json!({ "id": "nope" })).is_err());
    s.execute("vcs.shelf.delete", &json!({ "id": id })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "deleted", |s| s.vcs.shelves.is_empty() && !s.vcs.shelves_loading);
    assert!(s.execute("vcs.unshelve", &json!({ "id": id })).is_err(), "the shelf is empty");
    s.shutdown();
}

#[test]
fn the_merge_result_is_edited_by_hand_and_applied() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("c.txt", "head\nmiddle\nend\n")]);
    env.git(&env.root, &["checkout", "-qb", "feature"]);
    env.write("c.txt", "head\nTHEIRS\nend\n");
    env.git(&env.root, &["commit", "-qam", "theirs"]);
    env.git(&env.root, &["checkout", "-q", "main"]);
    env.write("c.txt", "head\nOURS\nend\n");
    env.git(&env.root, &["commit", "-qam", "ours"]);
    s.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    finish_op(&mut s);
    s.execute("vcs.merge", &json!({ "branch": "feature" })).unwrap();
    finish_op(&mut s);
    settle(&mut s, "conflict", |s| s.vcs.status.file("c.txt").map(|f| f.state()) == Some(FileState::Conflicted));
    s.execute("vcs.mergeView", &json!({ "path": "c.txt" })).unwrap();
    settle(&mut s, "merge view", |s| s.vcs.merge.is_some());
    let st = s.execute("vcs.merge.state", &Value::Null).unwrap();
    assert_eq!(st["unresolved"], 1);
    assert_eq!(st["chunks"][0]["resultLines"], json!([1, 6]));
    // Edit the conflict block by hand: it counts as resolved.
    let text = st["result"].as_str().unwrap();
    let edited = text.replace("<<<<<<< HEAD\nOURS\n=======\nTHEIRS\n>>>>>>> MERGE_HEAD\n", "BOTH OF US\n");
    let r = s.execute("vcs.merge.setResult", &json!({ "text": edited })).unwrap();
    assert_eq!(r["unresolved"], 0);
    assert!(s.execute("vcs.merge.setResult", &Value::Null).is_err());
    let st = s.execute("vcs.merge.state", &Value::Null).unwrap();
    assert_eq!(st["chunks"][0]["edited"], true);
    s.execute("vcs.merge.apply", &Value::Null).unwrap();
    finish_op(&mut s);
    assert_eq!(env.read("c.txt"), "head\nBOTH OF US\nend\n");
    settle(&mut s, "resolved", |s| s.vcs.conflicts().is_empty());
    s.shutdown();
}

#[test]
fn the_change_popup_shows_whether_a_hunk_is_staged() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("a.txt", "1\n2\n3\n4\n5\n6\n7\n8\n")]);
    s.execute("file.open", &json!({ "path": env.root.join("a.txt") })).unwrap();
    let doc = s.ws.active().unwrap().1.id;
    s.execute("editor.setSelection", &json!({ "anchor": 2, "head": 3 })).unwrap();
    s.execute("editor.type", &json!({ "text": "TWO" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    settle(&mut s, "marker", |s| s.vcs.markers(doc).len() == 1);
    s.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    settle(&mut s, "stage state", |s| s.vcs.hunk.as_ref().is_some_and(|h| !h.stage_loading));
    assert_eq!(s.vcs.hunk.as_ref().unwrap().stage, Some(StageState::Unstaged));
    s.execute("vcs.stageHunk", &Value::Null).unwrap();
    finish_op(&mut s);
    s.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    settle(&mut s, "stage state again", |s| s.vcs.hunk.as_ref().is_some_and(|h| !h.stage_loading));
    assert_eq!(s.vcs.hunk.as_ref().unwrap().stage, Some(StageState::Staged));
    // Change the staged line again in the editor: now partly staged.
    s.vcs.hunk = None;
    s.execute("editor.setSelection", &json!({ "anchor": 2, "head": 5 })).unwrap();
    s.execute("editor.type", &json!({ "text": "2b" })).unwrap();
    settle(&mut s, "marker moved", |s| s.vcs.markers(doc).len() == 1 && !s.vcs.busy());
    s.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    settle(&mut s, "partly", |s| s.vcs.hunk.as_ref().is_some_and(|h| !h.stage_loading));
    assert_eq!(s.vcs.hunk.as_ref().unwrap().stage, Some(StageState::Partly));
    s.shutdown();
}

/// A fake `gh` that logs its arguments and answers like the real one. `repo create` adds the
/// remote itself, as the real one does.
#[cfg(unix)]
fn fake_gh(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-gh");
    let log = dir.join("gh.log");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{log}'\n\
             case \"$1 $2\" in\n\
             'pr list') echo '[{{\"number\":7,\"title\":\"Add x\",\"state\":\"OPEN\",\"author\":{{\"login\":\"ann\"}},\"headRefName\":\"x\",\"baseRefName\":\"main\",\"labels\":[{{\"name\":\"bug\"}}],\"url\":\"https://github.com/o/r/pull/7\"}}]' ;;\n\
             'pr view') echo '{{\"number\":7,\"title\":\"Add x\",\"body\":\"Adds x.\",\"state\":\"OPEN\",\"author\":{{\"login\":\"ann\"}},\"headRefName\":\"x\",\"baseRefName\":\"main\",\"url\":\"https://github.com/o/r/pull/7\",\"files\":[{{\"path\":\"x.rs\",\"additions\":2,\"deletions\":0}}],\"reviews\":[],\"comments\":[],\"mergeable\":\"MERGEABLE\"}}' ;;\n\
             'pr diff') printf 'diff --git a/x.rs b/x.rs\\nnew file mode 100644\\n--- /dev/null\\n+++ b/x.rs\\n@@ -0,0 +1,2 @@\\n+fn x() {{}}\\n+fn y() {{}}\\n' ;;\n\
             api*) echo '[{{\"id\":1,\"path\":\"x.rs\",\"line\":2,\"body\":\"Name it better\",\"user\":{{\"login\":\"bob\"}}}}]' ;;\n\
             'pr checkout') echo 'Switched to branch x' ;;\n\
             'pr merge') echo 'Merged pull request #7' ;;\n\
             'repo create') git -C \"$6\" remote add \"$8\" \"https://github.com/me/$3.git\" && echo \"https://github.com/me/$3\" ;;\n\
             'repo sync') echo \"Synced the main branch of $3\" ;;\n\
             'gist create') echo 'https://gist.github.com/me/abc' ;;\n\
             *) echo \"unknown $1 $2\" >&2; exit 1 ;;\n\
             esac\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn pull_requests_share_sync_and_gists_through_a_fake_gh() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    let mut s = env.session(&[("f.txt", "1\n")]);
    // No GitHub remote yet: View Pull Requests and Sync Fork are disabled.
    assert!(s.enabled("git.gitHub.viewPullRequests").is_err());
    assert!(s.enabled("git.gitHub.syncFork").is_err());
    // Share Project on GitHub without gh: the dialog explains how to install it.
    s.execute("git.gitHub.shareProjectOnGitHub", &Value::Null).unwrap();
    assert!(matches!(&s.vcs.dialog, Some(Dialog::ShareOnGitHub { name, private: true, .. }) if name == "project"));
    assert!(s.vcs.dialog_error.as_deref().unwrap_or("").contains("brew install gh"));
    assert!(s.execute("vcs.github.share", &json!({ "name": "project" })).is_err());
    // Git may only use local transports: the fetch after Sync Fork is refused, never sent.
    let mut git_env = env.env();
    git_env.push(("GIT_ALLOW_PROTOCOL".into(), "file".into()));
    s.vcs.set_git_env(git_env);
    s.vcs.gh_program = fake_gh(&env.base);
    assert!(s.execute("vcs.github.share", &json!({ "name": "../evil" })).is_err());
    s.execute("vcs.github.share", &json!({ "name": "project", "description": "Mine", "private": false })).unwrap();
    finish_op(&mut s);
    assert_eq!(s.vcs.last_github_url.as_deref(), Some("https://github.com/me/project"), "{:?}", s.vcs.dialog_error);
    let log = std::fs::read_to_string(env.base.join("gh.log")).unwrap();
    assert!(log.contains("repo\ncreate\nproject\n--public\n--source\n"), "{log}");
    assert!(log.contains("--remote\norigin\n--push\n--description\nMine\n"), "{log}");
    settle(&mut s, "GitHub remote", |s| s.vcs.github_remote().is_some());
    assert!(s.enabled("git.gitHub.shareProjectOnGitHub").is_err(), "already shared");
    // View Pull Requests lists them; selecting one loads details, the diff and review comments.
    s.execute("git.gitHub.viewPullRequests", &Value::Null).unwrap();
    assert_eq!(s.vcs.git_tab, GitTab::PullRequests);
    settle(&mut s, "pull requests", |s| s.vcs.prs.loaded && !s.vcs.prs.loading);
    assert_eq!(s.vcs.prs.list.len(), 1, "{:?}", s.vcs.prs.error);
    assert_eq!(s.vcs.prs.list[0].author.login, "ann");
    assert!(s.execute("vcs.pr.list", &json!({ "state": "sideways" })).is_err());
    let st =
        s.execute("vcs.pr.list", &json!({ "state": "all", "author": "ann", "label": "bug", "fetch": true })).unwrap();
    assert_eq!(st["filter"]["state"], "all");
    settle(&mut s, "filtered", |s| !s.vcs.prs.loading);
    let log = std::fs::read_to_string(env.base.join("gh.log")).unwrap();
    assert!(log.contains("pr\nlist\n--state\nall\n--limit\n100\n--author\nann\n--label\nbug\n"), "{log}");
    assert!(s.execute("vcs.pr.select", &json!({ "number": 0 })).is_err());
    s.execute("vcs.pr.select", &json!({ "number": 7 })).unwrap();
    settle(&mut s, "details", |s| !s.vcs.prs.busy() && s.vcs.prs.details.is_some());
    let st = s.execute("vcs.pr.state", &Value::Null).unwrap();
    assert_eq!(st["details"]["body"], "Adds x.");
    assert_eq!(st["files"], json!(["x.rs"]));
    assert_eq!(st["selectedFile"], "x.rs");
    assert!(st["patch"].as_str().unwrap().contains("+fn y() {}"));
    assert_eq!(st["comments"][0]["body"], "Name it better");
    assert!(s.execute("vcs.pr.selectFile", &json!({ "path": "nope.rs" })).is_err());
    s.execute("vcs.pr.selectFile", &json!({ "path": "x.rs" })).unwrap();
    // Checkout and merge (with a method) run as operations.
    s.execute("vcs.pr.checkout", &Value::Null).unwrap();
    finish_op(&mut s);
    assert!(s.vcs.dialog_error.is_none(), "{:?}", s.vcs.dialog_error);
    assert!(s.execute("vcs.pr.merge", &json!({ "method": "octopus" })).is_err());
    s.execute("vcs.pr.merge", &json!({ "number": 7, "method": "squash", "deleteBranch": true })).unwrap();
    finish_op(&mut s);
    let log = std::fs::read_to_string(env.base.join("gh.log")).unwrap();
    assert!(log.contains("pr\ncheckout\n7\n"), "{log}");
    assert!(log.contains("pr\nmerge\n7\n--squash\n--delete-branch\n"), "{log}");
    let r = s.execute("vcs.pr.openInBrowser", &Value::Null).unwrap();
    assert_eq!(r["url"], "https://github.com/o/r/pull/7");
    // Sync Fork: gh repo sync for the fork, then a fetch from the remote.
    s.execute("git.gitHub.syncFork", &Value::Null).unwrap();
    finish_op(&mut s);
    assert!(s.vcs.dialog_error.is_none(), "{:?}", s.vcs.dialog_error);
    let log = std::fs::read_to_string(env.base.join("gh.log")).unwrap();
    assert!(log.contains("repo\nsync\nme/project\n"), "{log}");
    let console: String = s.vcs.console.iter().map(|c| format!("{}\n", c.text)).collect();
    assert!(console.contains("fetch"), "the remote was fetched (and refused here): {console}");
    // Create Gist for the current file.
    s.execute("file.open", &json!({ "path": env.root.join("f.txt") })).unwrap();
    s.execute("git.gitHub.createGist", &Value::Null).unwrap();
    assert!(matches!(&s.vcs.dialog, Some(Dialog::CreateGist { public: false, .. })));
    s.execute("vcs.github.createGist", &json!({ "path": "f.txt", "description": "snippet" })).unwrap();
    finish_op(&mut s);
    assert_eq!(s.vcs.last_github_url.as_deref(), Some("https://gist.github.com/me/abc"));
    let log = std::fs::read_to_string(env.base.join("gh.log")).unwrap();
    assert!(log.contains("gist\ncreate\n") && log.contains("--desc\nsnippet\n"), "{log}");
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn share_makes_a_plain_folder_a_repository_first() {
    if !git_available() {
        return;
    }
    let env = Env::new();
    env.write("main.rs", "fn main() {}\n");
    let mut s = env.open();
    for _ in 0..20 {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.vcs.repo.is_none());
    s.vcs.gh_program = fake_gh(&env.base);
    s.execute("vcs.github.share", &json!({ "name": "project" })).unwrap();
    settle(&mut s, "share", |s| s.vcs.op.is_none());
    settle(&mut s, "repository", |s| s.vcs.repo.is_some() && s.vcs.github_remote().is_some());
    assert_eq!(env.git(&env.root, &["log", "-1", "--format=%s"]).trim(), "Initial commit");
    assert_eq!(env.git(&env.root, &["show", "--name-only", "--format=", "HEAD"]).trim(), "main.rs");
    s.shutdown();
}
