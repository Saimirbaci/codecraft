//! M13 track VC in the UI, headless on temp repositories: changelists in the Commit tool window,
//! the Shelve dialog and Shelf tab, the three-pane merge editor (accept arrows, typing in the
//! result, Apply), the staged state in the change popup, and the Pull Requests tab on a fake
//! `gh` (never the real GitHub CLI).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::vcs::{CommitTab, GitTab};
use codecraft_engine::vcs_git::hunks::StageState;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn env(home: &Path) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), home.display().to_string()),
        ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        // Only local transports: nothing here may reach the network.
        ("GIT_ALLOW_PROTOCOL".into(), "file".into()),
    ]
}

fn git(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).envs(env(home)).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository with one commit of `files`; returns (root, home).
fn repo(dir: &Path, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let base = dir.canonicalize().unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Test User\n\temail = test@example.com\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &home, &["init", "-q", "-b", "main"]);
    for (f, t) in files {
        std::fs::write(root.join(f), t).unwrap();
    }
    git(&root, &home, &["add", "-A"]);
    git(&root, &home, &["commit", "-qm", "initial"]);
    (root, home)
}

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {} / {:?}", h.state().session.status, h.state().session.vcs.dialog_error);
}

fn harness(root: &Path, home: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.vcs.set_git_env(env(home));
    h.state_mut().session.vcs.gh_program = home.join("no-gh-here");
    settle(&mut h, "repository", |a| a.session.vcs.repo.is_some() && a.session.vcs.status.branch.head.is_some());
    h
}

/// With `CODECRAFT_SNAPSHOT_DIR` set, save a picture of the window (for reviewing the layout).
fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn idle(a: &App) -> bool {
    a.session.vcs.op.is_none()
}

#[test]
fn changelists_and_the_shelf_in_the_commit_tool_window() {
    if !git_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo(dir.path(), &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    std::fs::write(root.join("a.txt"), "a2\n").unwrap();
    std::fs::write(root.join("b.txt"), "b2\n").unwrap();
    let mut h = harness(&root, &home);
    h.state_mut().session.execute("vcs.refresh", &json!({})).unwrap();
    settle(&mut h, "changes", |a| a.session.vcs.changes().len() == 2);
    h.state_mut().session.execute("git.commit", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Commit));
    // New Changelist… from the toolbar: the dialog creates it with b.txt moved in.
    h.state_mut().session.execute("vcs.changelist.new", &json!({ "files": ["b.txt"] })).unwrap();
    h.run_steps(2);
    h.get_by_label("Changelist Name").focus();
    h.run_steps(1);
    h.get_by_label("Changelist Name").type_text("Feature");
    h.run_steps(1);
    h.get_by_role_and_label(Role::Button, "Create").click();
    h.run_steps(3);
    assert_eq!(h.state().session.vcs.changelists.list_of("b.txt"), Some("Feature"));
    // Both lists are groups with their own Include checkbox.
    assert!(h.query_by_label("Include Feature").is_some());
    snapshot(&mut h, "vc-changelists");
    h.get_by_label("Include Feature").click();
    h.run_steps(2);
    assert_eq!(
        h.state().session.vcs.included_paths(),
        vec!["a.txt".to_string()],
        "unchecking a list excludes its files"
    );
    // Shelve the Feature list from the dialog.
    h.state_mut().session.execute("git.uncommittedChanges.shelveChanges", &json!({ "changelist": "Feature" })).unwrap();
    h.run_steps(2);
    h.get_by_label("Shelf Name").focus();
    h.run_steps(1);
    h.get_by_label("Shelf Name").type_text("Later");
    h.run_steps(1);
    h.get_by_role_and_label(Role::Button, "Shelve Changes").click();
    h.run_steps(1);
    settle(&mut h, "shelved", |a| idle(a) && a.session.vcs.shelves.len() == 1);
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b\n");
    // The Shelf tab lists it; Unshelve brings the change back.
    h.state_mut().session.vcs.commit_tab = CommitTab::Shelf;
    h.run_steps(3);
    assert!(h.query_by_label("Shelf (1)").is_some());
    snapshot(&mut h, "vc-shelf");
    h.get_by_label("Unshelve Later").click();
    h.run_steps(1);
    settle(&mut h, "unshelved", |a| idle(a) && a.session.vcs.shelves.is_empty() && !a.session.vcs.shelves_loading);
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b2\n");
    h.state_mut().session.shutdown();
}

#[test]
fn the_merge_editor_accepts_sides_and_takes_typing() {
    if !git_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo(dir.path(), &[("c.txt", "head\nmiddle\nend\n")]);
    git(&root, &home, &["checkout", "-qb", "feature"]);
    std::fs::write(root.join("c.txt"), "head\nTHEIRS\nend\n").unwrap();
    git(&root, &home, &["commit", "-qam", "theirs"]);
    git(&root, &home, &["checkout", "-q", "main"]);
    std::fs::write(root.join("c.txt"), "head\nOURS\nend\n").unwrap();
    git(&root, &home, &["commit", "-qam", "ours"]);
    let _ = Command::new("git").args(["merge", "feature"]).current_dir(&root).envs(env(&home)).output().unwrap();
    let mut h = harness(&root, &home);
    h.state_mut().session.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    settle(&mut h, "conflict", |a| !a.session.vcs.conflicts().is_empty());
    h.state_mut().session.execute("vcs.mergeView", &json!({ "path": "c.txt" })).unwrap();
    settle(&mut h, "merge editor", |a| a.session.vcs.merge.is_some());
    h.run_steps(3);
    assert!(h.query_by_label("1 of 1 conflict unresolved").is_some());
    assert!(h.query_by_label("Yours (HEAD)").is_some() && h.query_by_label("Theirs (MERGE_HEAD)").is_some());
    snapshot(&mut h, "vc-merge-editor");
    // « takes theirs.
    h.get_by_label("Accept theirs for change 1").click();
    h.run_steps(3);
    assert_eq!(h.state().session.vcs.merge.as_ref().unwrap().text, "head\nTHEIRS\nend\n");
    assert!(h.query_by_label("All changes are resolved (1 conflict)").is_some());
    // » takes yours instead.
    h.get_by_label("Accept yours for change 1").click();
    h.run_steps(3);
    assert_eq!(h.state().session.vcs.merge.as_ref().unwrap().text, "head\nOURS\nend\n");
    // Type into the result: the edit lands in the model.
    h.get_by_label("Merge Result").focus();
    h.run_steps(1);
    h.get_by_label("Merge Result").type_text("// merged\n");
    h.run_steps(3);
    let text = h.state().session.vcs.merge.as_ref().unwrap().text.clone();
    assert!(text.contains("// merged"), "{text}");
    h.get_by_role_and_label(Role::Button, "Apply").click();
    h.run_steps(1);
    settle(&mut h, "applied", |a| idle(a) && a.session.vcs.merge.is_none() && a.session.vcs.conflicts().is_empty());
    assert_eq!(std::fs::read_to_string(root.join("c.txt")).unwrap(), text);
    h.state_mut().session.shutdown();
}

#[test]
fn the_change_popup_says_whether_the_hunk_is_staged() {
    if !git_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo(dir.path(), &[("main.rs", "fn main() {\n    let x = 1;\n}\n")]);
    let mut h = harness(&root, &home);
    h.state_mut().session.execute("file.open", &json!({ "path": root.join("main.rs") })).unwrap();
    h.run_steps(2);
    let doc = h.state().session.ws.active().unwrap().1.id;
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 20, "head": 21 })).unwrap();
    h.state_mut().session.execute("editor.type", &json!({ "text": "42" })).unwrap();
    settle(&mut h, "marker", |a| !a.session.vcs.markers(doc).is_empty());
    h.state_mut().session.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    settle(&mut h, "stage state", |a| a.session.vcs.hunk.as_ref().is_some_and(|p| !p.stage_loading));
    h.run_steps(2);
    assert_eq!(h.state().session.vcs.hunk.as_ref().unwrap().stage, Some(StageState::Unstaged));
    assert!(h.query_by_label("Not staged").is_some());
    snapshot(&mut h, "vc-hunk-popup");
    h.state_mut().session.save_all().unwrap();
    h.get_by_label("Stage").click();
    h.run_steps(1);
    settle(&mut h, "staged", |a| idle(a) && a.session.vcs.hunk.is_none());
    h.state_mut().session.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    settle(&mut h, "stage state again", |a| a.session.vcs.hunk.as_ref().is_some_and(|p| !p.stage_loading));
    h.run_steps(2);
    assert!(h.query_by_label("Staged").is_some());
    h.state_mut().session.shutdown();
}

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
             'pr list') echo '[{{\"number\":7,\"title\":\"Add x\",\"state\":\"OPEN\",\"author\":{{\"login\":\"ann\"}},\"headRefName\":\"x\",\"baseRefName\":\"main\",\"labels\":[],\"url\":\"https://github.com/o/r/pull/7\"}}]' ;;\n\
             'pr view') echo '{{\"number\":7,\"title\":\"Add x\",\"body\":\"Adds the x function.\",\"state\":\"OPEN\",\"author\":{{\"login\":\"ann\"}},\"headRefName\":\"x\",\"baseRefName\":\"main\",\"files\":[{{\"path\":\"x.rs\",\"additions\":1,\"deletions\":0}}]}}' ;;\n\
             'pr diff') printf 'diff --git a/x.rs b/x.rs\\n--- /dev/null\\n+++ b/x.rs\\n@@ -0,0 +1 @@\\n+fn x() {{}}\\n' ;;\n\
             api*) echo '[{{\"id\":1,\"path\":\"x.rs\",\"line\":1,\"body\":\"Looks good\",\"user\":{{\"login\":\"bob\"}}}}]' ;;\n\
             'pr checkout') echo 'Switched to branch x' ;;\n\
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
fn the_pull_requests_tab_on_a_fake_gh() {
    if !git_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo(dir.path(), &[("f.txt", "1\n")]);
    git(&root, &home, &["remote", "add", "origin", "https://github.com/o/r.git"]);
    let mut h = harness(&root, &home);
    h.state_mut().session.vcs.gh_program = fake_gh(&home);
    h.state_mut().session.execute("vcs.refresh", &json!({ "full": true })).unwrap();
    settle(&mut h, "GitHub remote", |a| a.session.vcs.github_remote().is_some());
    h.state_mut().session.execute("git.gitHub.viewPullRequests", &json!({})).unwrap();
    assert_eq!(h.state().session.vcs.git_tab, GitTab::PullRequests);
    settle(&mut h, "list", |a| a.session.vcs.prs.loaded && !a.session.vcs.prs.loading);
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Git));
    h.get_by_label("#7 Add x").click();
    h.run_steps(1);
    settle(&mut h, "details", |a| a.session.vcs.prs.details.is_some() && !a.session.vcs.prs.busy());
    h.run_steps(3);
    assert!(h.query_by_label("Adds the x function.").is_some());
    assert!(h.query_by_label("Looks good").is_some(), "the review comment on the selected file");
    assert!(h.query_by_label("+fn x() {}").is_some(), "the file's diff");
    snapshot(&mut h, "vc-pull-requests");
    h.get_by_role_and_label(Role::Button, "Checkout").click();
    h.run_steps(1);
    settle(&mut h, "checkout", idle);
    let log = std::fs::read_to_string(home.join("gh.log")).unwrap();
    assert!(log.contains("pr\ncheckout\n7\n"), "{log}");
    h.state_mut().session.shutdown();
}
