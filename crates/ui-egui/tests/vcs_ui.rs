//! The Git UI driven headlessly on temp repositories: the commit flow in the Commit tool window
//! (Cmd+K, check an unversioned file, type a message, Commit) and gutter change markers after an
//! edit (marker popup, Rollback).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::vcs_git::{FileState, LineChangeKind};
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn env(home: &Path) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), home.display().to_string()),
        ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
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
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

fn harness(root: &Path, home: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.vcs.set_git_env(env(home));
    settle(&mut h, "repository", |a| a.session.vcs.repo.is_some() && a.session.vcs.status.branch.head.is_some());
    h
}

#[test]
fn commit_flow_in_the_commit_tool_window() {
    if !git_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo(dir.path(), &[("a.txt", "a\n")]);
    std::fs::write(root.join("a.txt"), "a2\n").unwrap();
    std::fs::write(root.join("new.txt"), "new\n").unwrap();
    let mut h = harness(&root, &home);
    h.state_mut().session.execute("vcs.refresh", &json!({})).unwrap();
    settle(&mut h, "changes", |a| a.session.vcs.status.files.len() == 2);

    // Cmd+K shows the Commit tool window with the message box focused.
    h.key_press_modifiers(CMD, Key::K);
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::Commit));
    assert!(h.get_by_label("Commit Message").is_focused());
    // The unversioned file is not included until checked.
    assert_eq!(h.state().session.vcs.included_paths(), vec!["a.txt".to_string()]);
    h.get_by_label("Include new.txt").click();
    h.run_steps(2);
    assert_eq!(h.state().session.vcs.included_paths(), vec!["a.txt".to_string(), "new.txt".to_string()]);
    // Selecting a change previews its diff.
    h.get_by_label("a.txt").click();
    settle(&mut h, "preview", |a| a.session.vcs.preview.as_ref().is_some_and(|p| !p.loading && p.new == "a2\n"));
    // Type the message and commit.
    h.get_by_label("Commit Message").focus();
    h.run_steps(1);
    h.get_by_label("Commit Message").type_text("Add new.txt");
    h.run_steps(2);
    assert_eq!(h.state().session.vcs.commit.message, "Add new.txt");
    h.get_by_role_and_label(Role::Button, "Commit").click();
    h.run_steps(1);
    settle(&mut h, "commit", |a| {
        a.session.vcs.op.is_none() && a.session.vcs.status.files.is_empty() && a.session.vcs.commit.message.is_empty()
    });
    assert_eq!(git(&root, &home, &["log", "-1", "--format=%s"]).trim(), "Add new.txt");
    assert_eq!(git(&root, &home, &["show", "--name-only", "--format=", "HEAD"]).trim(), "a.txt\nnew.txt");
    // Cmd+Enter in the message box commits too.
    std::fs::write(root.join("a.txt"), "a3\n").unwrap();
    h.state_mut().session.execute("vcs.refresh", &json!({})).unwrap();
    settle(&mut h, "modified", |a| a.session.vcs.status.files.len() == 1);
    h.get_by_label("Commit Message").focus();
    h.run_steps(1);
    h.get_by_label("Commit Message").type_text("Third");
    h.run_steps(1);
    h.key_press_modifiers(CMD, Key::Enter);
    settle(&mut h, "second commit", |a| a.session.vcs.op.is_none() && a.session.vcs.status.files.is_empty());
    assert_eq!(git(&root, &home, &["log", "-1", "--format=%s"]).trim(), "Third");
    h.state_mut().session.shutdown();
}

#[test]
fn gutter_markers_after_an_edit_and_hunk_rollback() {
    if !git_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = repo(dir.path(), &[("main.rs", "fn main() {\n    let x = 1;\n}\n")]);
    let mut h = harness(&root, &home);
    let file = root.join("main.rs");
    h.state_mut().session.execute("file.open", &json!({ "path": file })).unwrap();
    h.run_steps(2);
    let doc = h.state().session.ws.active().unwrap().1.id;
    // Edit line 2 through the editor: a modified marker appears.
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 20, "head": 21 })).unwrap();
    h.state_mut().session.execute("editor.type", &json!({ "text": "42" })).unwrap();
    settle(&mut h, "marker", |a| {
        a.session.vcs.markers(doc).iter().any(|c| c.kind == LineChangeKind::Modified && c.start == 1)
    });
    // The tab shows the file as modified (blue).
    assert_eq!(h.state().session.vcs.file_state(&file), None, "not saved yet: git sees no change");
    h.run_steps(2);
    // The marker popup: Rollback restores the committed line.
    h.state_mut().session.execute("vcs.showHunk", &json!({ "line": 1 })).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label("Modified lines").is_some());
    h.get_by_label("Rollback").click();
    h.run_steps(2);
    assert_eq!(h.state().session.ws.active().unwrap().1.doc.text(), "fn main() {\n    let x = 1;\n}\n");
    settle(&mut h, "no markers", |a| a.session.vcs.markers(doc).is_empty());
    assert!(h.state().session.vcs.hunk.is_none());
    // Saving a change colours the file as modified.
    h.state_mut().session.execute("editor.type", &json!({ "text": "// x\n" })).unwrap();
    h.state_mut().session.save_all().unwrap();
    h.state_mut().session.execute("vcs.refresh", &json!({})).unwrap();
    settle(&mut h, "modified state", |a| a.session.vcs.file_state(&file) == Some(FileState::Modified));
    h.state_mut().session.shutdown();
}
