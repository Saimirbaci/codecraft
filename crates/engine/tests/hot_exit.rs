//! Nothing lost on restart (M13 track SR): hot exit keeps unsaved and untitled buffers in
//! `.codecraft/unsaved/` (the config directory for read-only projects), autosaves them while they
//! change, brings them back modified when the project opens again, flags files that changed on
//! disk meanwhile and cleans up after saving or discarding; per-tab soft-wrap and column mode
//! come back with the tab. Quit and reopen are two `Session`s on the same temp directory; every
//! per-user file lives in a temp home.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use codecraft_engine::hot_exit::{self, Conflict};
use codecraft_engine::workspace::{DocId, NotificationLevel};
use codecraft_engine::{CommandError, Session};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    proj: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (home, proj) = (root.join("home"), root.join("proj"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(proj.join("b.txt"), "bee\n").unwrap();
    Env { _tmp: tmp, home, proj }
}

fn wait_idle(s: &mut Session) {
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A session like the desktop app's (sessions saved, hot exit on, autosave every pump), its
/// per-user files under the temp home, the project open.
fn session(e: &Env) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&e.home);
    s.session_store.enable();
    s.session_store.fallback_dir = Some(e.home.join("workspaces"));
    s.hot_exit.enabled = true;
    s.hot_exit.every = Duration::ZERO;
    s.execute("file.open", &json!({ "path": e.proj })).unwrap();
    wait_idle(&mut s);
    s
}

fn open(s: &mut Session, path: &Path) -> DocId {
    s.execute("file.open", &json!({ "path": path })).unwrap();
    s.ws.doc_by_path(path).unwrap()
}

fn type_text(s: &mut Session, text: &str) {
    s.execute("editor.type", &json!({ "text": text })).unwrap();
}

/// An untitled buffer with `text` in a new tab.
fn untitled(s: &mut Session, text: &str) -> DocId {
    let d = s.ws.add_document(None, "");
    s.ws.open_editor(d).unwrap();
    type_text(s, text);
    d
}

fn text_of(s: &Session, d: DocId) -> String {
    s.ws.doc(d).unwrap().doc.text()
}

fn kept_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Autosave and wait for the writer.
fn settle(s: &mut Session) {
    s.pump();
    assert!(s.hot_exit.flush(Duration::from_secs(10)));
}

/// What a quit does in the desktop app.
fn quit(mut s: Session) {
    hot_exit::quit_save(&mut s);
    s.shutdown();
}

fn untitled_docs(s: &Session) -> Vec<DocId> {
    s.ws.docs().filter(|d| d.path.is_none()).map(|d| d.id).collect()
}

#[test]
fn quit_keeps_unsaved_and_untitled_buffers_and_reopen_restores_them() {
    let e = env();
    let a = e.proj.join("src/a.rs");
    let mut s = session(&e);
    open(&mut s, &e.proj.join("b.txt"));
    let da = open(&mut s, &a);
    type_text(&mut s, "// mine\n");
    let du = untitled(&mut s, "scratch notes");
    settle(&mut s);
    let dir = e.proj.join(hot_exit::DIR);
    assert_eq!(kept_files(&dir).len(), 2, "the modified file and the untitled buffer; not the clean b.txt");
    assert!(dir.join(".gitignore").is_file(), "kept out of version control");
    // Only written again when the text changes.
    let writes = s.hot_exit.writes;
    for _ in 0..3 {
        settle(&mut s);
    }
    assert_eq!(s.hot_exit.writes, writes);
    type_text(&mut s, "x");
    settle(&mut s);
    assert_eq!(s.hot_exit.writes, writes + 1);
    assert_eq!(text_of(&s, du), "scratch notesx");
    s.ws.set_active_editor(s.ws.editor_for_doc(da).unwrap());
    quit(s);
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn a() {}\n", "quitting does not write the file");
    assert_eq!(kept_files(&dir).len(), 2);

    // Reopen: both come back modified, with a notice.
    let mut s = session(&e);
    let da = s.ws.doc_by_path(&a).unwrap();
    let od = s.ws.doc(da).unwrap();
    assert_eq!(od.doc.text(), "// mine\nfn a() {}\n");
    assert!(od.doc.is_modified());
    assert!(s.ws.editor_for_doc(da).is_some(), "it has a tab");
    let restored = s.hot_exit.restored.get(&da).unwrap();
    assert_eq!(restored.conflict, Conflict::None);
    let docs = untitled_docs(&s);
    assert_eq!(docs.len(), 1);
    let du = docs[0];
    assert_eq!(text_of(&s, du), "scratch notesx");
    assert!(s.ws.editor_for_doc(du).is_some());
    assert!(s.ws.notifications.iter().any(|n| n.title == "Restored unsaved changes" && n.message.contains("a.rs")));
    let state = s.execute("hotExit.state", &Value::Null).unwrap();
    assert_eq!(state["restored"].as_array().unwrap().len(), 2, "{state}");
    assert_eq!(state["buffers"].as_array().unwrap().len(), 2, "{state}");

    // Compare with disk shows the diff view.
    let r = s.execute("hotExit.compareWithDisk", &json!({ "doc": da.0 })).unwrap();
    assert_eq!(r["identical"], false);
    let cmp = s.misc.compare.clone().unwrap();
    assert_eq!(cmp.left, "fn a() {}\n");
    assert_eq!(cmp.right, "// mine\nfn a() {}\n");
    assert!(s.execute("hotExit.compareWithDisk", &json!({ "doc": du.0 })).is_err(), "an untitled buffer has no file");

    // The restore is one undo step back to the disk text.
    s.ws.set_active_editor(s.ws.editor_for_doc(da).unwrap());
    s.execute("edit.undo", &Value::Null).unwrap();
    assert_eq!(text_of(&s, da), "fn a() {}\n");
    s.execute("edit.redo", &Value::Null).unwrap();
    assert_eq!(text_of(&s, da), "// mine\nfn a() {}\n");

    // Saving writes it and its kept copy goes; discarding the untitled buffer closes it.
    s.execute("file.saveAll", &Value::Null).unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "// mine\nfn a() {}\n");
    settle(&mut s);
    assert!(!s.hot_exit.restored.contains_key(&da), "the banner goes once saved");
    assert_eq!(kept_files(&dir).len(), 1);
    s.execute("hotExit.discard", &json!({ "doc": du.0 })).unwrap();
    assert!(s.ws.doc(du).is_none());
    settle(&mut s);
    assert!(kept_files(&dir).is_empty());
    quit(s);

    // Nothing is restored twice.
    let s = session(&e);
    assert!(s.hot_exit.restored.is_empty());
    assert!(untitled_docs(&s).is_empty());
    quit(s);
}

#[test]
fn a_crash_loses_at_most_the_last_seconds() {
    let e = env();
    let a = e.proj.join("src/a.rs");
    let mut s = session(&e);
    open(&mut s, &a);
    type_text(&mut s, "one ");
    settle(&mut s);
    type_text(&mut s, "two ");
    settle(&mut s);
    // No quit: the process dies (the session is dropped without any save).
    drop(s);
    let mut s = session(&e);
    let da = s.ws.doc_by_path(&a).unwrap();
    assert_eq!(text_of(&s, da), "one two fn a() {}\n");
    assert!(s.ws.doc(da).unwrap().doc.is_modified());
    // Closing the tab saves the file (RustRover style), so its kept copy goes too.
    s.ws.set_active_editor(s.ws.editor_for_doc(da).unwrap());
    s.execute("window.editorTabs.closeTab", &Value::Null).unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "one two fn a() {}\n");
    settle(&mut s);
    assert!(kept_files(&e.proj.join(hot_exit::DIR)).is_empty());
    quit(s);
}

fn set_mtime(path: &Path, t: SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(t).unwrap();
}

#[test]
fn a_file_changed_on_disk_meanwhile_is_flagged_not_overwritten() {
    let e = env();
    let a = e.proj.join("src/a.rs");
    let b = e.proj.join("b.txt");
    let mut s = session(&e);
    open(&mut s, &b);
    type_text(&mut s, "B ");
    open(&mut s, &a);
    type_text(&mut s, "// mine\n");
    quit(s);
    // Meanwhile another tool changes a.rs and deletes b.txt.
    std::fs::write(&a, "fn changed() {}\n").unwrap();
    set_mtime(&a, SystemTime::now() + Duration::from_secs(5));
    std::fs::remove_file(&b).unwrap();

    let mut s = session(&e);
    let da = s.ws.doc_by_path(&a).unwrap();
    let db = s.ws.doc_by_path(&b).unwrap();
    assert_eq!(text_of(&s, da), "// mine\nfn a() {}\n", "the kept changes are shown");
    assert_eq!(s.hot_exit.restored[&da].conflict, Conflict::ChangedOnDisk);
    assert_eq!(s.hot_exit.restored[&db].conflict, Conflict::DeletedOnDisk);
    assert_eq!(text_of(&s, db), "B bee\n");
    assert!(s.ws.notifications.iter().any(|n| n.level == NotificationLevel::Warning
        && n.title.contains("Changed on disk")
        && n.message.contains("a.rs")));
    // Saving does not overwrite the changed file, nor recreate the deleted one.
    assert!(
        matches!(s.execute("file.saveAll", &Value::Null), Err(CommandError::Failed(m)) if m.contains("changed on disk"))
    );
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn changed() {}\n");
    assert!(!b.exists());
    // Compare with disk: the other tool's text against the kept changes.
    s.execute("hotExit.compareWithDisk", &json!({ "doc": da.0 })).unwrap();
    assert_eq!(s.misc.compare.as_ref().unwrap().left, "fn changed() {}\n");
    // Closing the tab keeps the changes for next time instead of writing them.
    s.ws.set_active_editor(s.ws.editor_for_doc(da).unwrap());
    s.execute("window.editorTabs.closeTab", &Value::Null).unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn changed() {}\n");
    quit(s);

    // Still flagged after another restart (the old baseline is kept).
    let mut s = session(&e);
    let da = s.ws.doc_by_path(&a).unwrap();
    let db = s.ws.doc_by_path(&b).unwrap();
    assert_eq!(s.hot_exit.restored[&da].conflict, Conflict::ChangedOnDisk);
    // Keep mine for a.rs (then it saves), discard for b.txt (back to nothing on disk).
    s.execute("hotExit.keepChanges", &json!({ "doc": da.0 })).unwrap();
    s.execute("hotExit.discard", &json!({ "doc": db.0 })).unwrap();
    assert!(!s.ws.doc(db).unwrap().doc.is_modified());
    s.execute("file.saveAll", &Value::Null).unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "// mine\nfn a() {}\n");
    assert!(!b.exists(), "discarding never writes");
    settle(&mut s);
    assert!(kept_files(&e.proj.join(hot_exit::DIR)).is_empty());
    assert!(s.execute("hotExit.keepChanges", &json!({ "doc": da.0 })).is_err() || s.hot_exit.restored.is_empty());
    quit(s);
}

#[cfg(unix)]
#[test]
fn a_read_only_project_keeps_its_buffers_in_the_config_directory() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    let a = e.proj.join("src/a.rs");
    let mut s = session(&e);
    open(&mut s, &a);
    type_text(&mut s, "ro ");
    std::fs::set_permissions(&e.proj, std::fs::Permissions::from_mode(0o555)).unwrap();
    quit(s);
    let own = e.proj.join(hot_exit::DIR);
    let fallback = s_fallback(&e);
    let restore_perms = || std::fs::set_permissions(&e.proj, std::fs::Permissions::from_mode(0o755)).unwrap();
    if own.exists() {
        // Running as a user that can write anyway (root): nothing to check.
        restore_perms();
        return;
    }
    assert_eq!(kept_files(&fallback).len(), 1, "in {}", fallback.display());
    assert!(fallback.starts_with(&e.home), "never the real home");
    let s = session(&e);
    let da = s.ws.doc_by_path(&a).unwrap();
    assert_eq!(text_of(&s, da), "ro fn a() {}\n");
    quit(s);
    restore_perms();
}

fn s_fallback(e: &Env) -> PathBuf {
    let mut st = codecraft_engine::hot_exit::HotExitState::default();
    st.fallback_dir = Some(e.home.join("unsaved"));
    st.fallback_for(Some(&e.proj)).unwrap()
}

#[test]
fn with_hot_exit_off_quit_saves_as_before_and_nothing_is_kept() {
    let e = env();
    let a = e.proj.join("src/a.rs");
    let mut s = session(&e);
    open(&mut s, &a);
    type_text(&mut s, "// on ");
    settle(&mut s);
    let dir = e.proj.join(hot_exit::DIR);
    assert_eq!(kept_files(&dir).len(), 1);
    // Turning it off drops what was kept.
    s.execute("settings.set", &json!({ "key": "hot_exit", "value": false })).unwrap();
    assert!(!s.ws.settings.hot_exit);
    settle(&mut s);
    assert!(kept_files(&dir).is_empty());
    type_text(&mut s, "off ");
    settle(&mut s);
    assert!(kept_files(&dir).is_empty());
    assert!(s.execute("hotExit.keepNow", &Value::Null).is_err());
    quit(s);
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "// on off fn a() {}\n", "saved on quit");
    let s = session(&e);
    assert!(s.hot_exit.restored.is_empty());
    quit(s);
}

#[test]
fn oversized_and_too_many_buffers_are_not_kept() {
    let e = env();
    let mut s = session(&e);
    s.hot_exit.max_buffer_bytes = 32;
    s.hot_exit.max_buffers = 1;
    let big = untitled(&mut s, &"x".repeat(100));
    let small = untitled(&mut s, "small");
    let other = untitled(&mut s, "other");
    settle(&mut s);
    let dir = e.proj.join(hot_exit::DIR);
    assert_eq!(kept_files(&dir).len(), 1);
    assert!(s.hot_exit.skipped.get(&big).is_some_and(|r| r.contains("larger")), "{:?}", s.hot_exit.skipped);
    assert!(s.hot_exit.skipped.contains_key(&other) || s.hot_exit.skipped.contains_key(&small));
    assert!(s.ws.notifications.iter().any(|n| n.title.contains("not kept")));
    let r = s.execute("hotExit.keepNow", &Value::Null).unwrap();
    assert_eq!(r["skipped"], 2, "{r}");
    quit(s);
}

#[test]
fn hostile_kept_files_never_panic() {
    let e = env();
    let dir = e.proj.join(hot_exit::DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("b-1.json"), "{ not json").unwrap();
    std::fs::write(dir.join("b-2.json"), json!({ "key": "b-2", "text": "rel", "path": "src/a.rs" }).to_string())
        .unwrap();
    std::fs::write(
        dir.join("b-3.json"),
        json!({ "key": "b-3", "text": "dir", "path": e.proj.join("src") }).to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("b-4.json"), json!({ "key": "../../x", "text": "named by its file" }).to_string()).unwrap();
    std::fs::write(dir.join("b-5.json"), json!({ "text": 5 }).to_string()).unwrap();
    std::fs::write(dir.join("..json"), "{}").unwrap();
    // Two copies of one file (a crash between writes): the newest wins.
    let a = e.proj.join("src/a.rs");
    for (k, t, ms) in [("b-6", "old ", 1u64), ("b-7", "new ", 2u64)] {
        let v = json!({ "key": k, "text": format!("{t}fn a() {{}}\n"), "path": a, "savedMs": ms });
        std::fs::write(dir.join(format!("{k}.json")), v.to_string()).unwrap();
    }
    let mut s = session(&e);
    let untitled = untitled_docs(&s);
    assert_eq!(untitled.len(), 1, "b-4 is a valid untitled buffer");
    assert_eq!(text_of(&s, untitled[0]), "named by its file");
    assert_eq!(text_of(&s, s.ws.doc_by_path(&a).unwrap()), "new fn a() {}\n");
    assert!(!dir.join("b-6.json").exists(), "the older copy goes");
    assert!(dir.join("b-1.json.bad").exists());
    assert!(s.ws.notifications.iter().any(|n| n.title.contains("could not be restored")));
    // Commands with hostile params are errors, never panics.
    for p in [json!({ "doc": -1 }), json!({ "doc": "x" }), json!({ "doc": u64::MAX }), json!({ "doc": 99999 })] {
        for id in ["hotExit.compareWithDisk", "hotExit.keepChanges", "hotExit.discard"] {
            let _ = s.execute(id, &p);
        }
    }
    s.execute("hotExit.restore", &Value::Null).unwrap();
    s.execute("hotExit.state", &json!([1, 2])).unwrap();
    quit(s);
}

#[test]
fn a_second_window_on_the_project_does_not_take_live_buffers() {
    let e = env();
    let a = e.proj.join("src/a.rs");
    let mut s1 = session(&e);
    open(&mut s1, &a);
    type_text(&mut s1, "live ");
    settle(&mut s1);
    let s2 = session(&e);
    assert!(s2.hot_exit.restored.is_empty(), "the first window's buffer is still live");
    let d2 = s2.ws.doc_by_path(&a);
    assert!(d2.is_none_or(|d| !s2.ws.doc(d).unwrap().doc.is_modified()));
    quit(s2);
    quit(s1);
    let s3 = session(&e);
    assert_eq!(text_of(&s3, s3.ws.doc_by_path(&a).unwrap()), "live fn a() {}\n");
    quit(s3);
}

#[test]
fn files_outside_the_project_are_kept_in_the_config_directory_and_a_project_cannot_restore_them() {
    let e = env();
    let outside = e.home.join("notes.txt");
    std::fs::write(&outside, "notes\n").unwrap();
    let mut s = session(&e);
    open(&mut s, &outside);
    type_text(&mut s, "more ");
    settle(&mut s);
    assert!(kept_files(&e.proj.join(hot_exit::DIR)).is_empty(), "not in the project");
    assert_eq!(kept_files(&s_fallback(&e)).len(), 1);
    quit(s);
    let s = session(&e);
    let d = s.ws.doc_by_path(&outside).unwrap();
    assert_eq!(text_of(&s, d), "more notes\n", "restored from the config directory");
    quit(s);

    // A project that carries a kept file naming something outside it (a malicious clone) does
    // not get it opened, let alone saved.
    let dir = e.proj.join(hot_exit::DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let target = e.home.join("victim.txt");
    std::fs::write(&target, "safe\n").unwrap();
    let v = json!({ "key": "b-9", "text": "evil\n", "path": target, "savedMs": 1 });
    std::fs::write(dir.join("b-9.json"), v.to_string()).unwrap();
    let mut s = session(&e);
    assert!(s.ws.doc_by_path(&target).is_none());
    assert!(dir.join("b-9.json.bad").exists());
    assert!(s.ws.notifications.iter().any(|n| n.message.contains("outside the project")));
    let _ = s.save_all();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "safe\n");
    quit(s);
}

#[test]
fn per_tab_soft_wrap_and_column_mode_come_back_with_the_tab() {
    let e = env();
    let a = e.proj.join("src/a.rs");
    let b = e.proj.join("b.txt");
    let mut s = session(&e);
    open(&mut s, &b);
    open(&mut s, &a);
    assert!(!s.ws.settings.soft_wrap);
    let r = s.execute("view.toggleSoftWrap", &Value::Null).unwrap();
    assert_eq!(r["on"], true);
    assert!(!s.ws.settings.soft_wrap, "only this tab");
    assert_eq!(codecraft_engine::appearance::checked(&s, "view.toggleSoftWrap"), Some(true));
    s.execute("edit.columnSelectionMode", &json!({ "on": true })).unwrap();
    s.ws.set_active_editor(s.ws.editor_for_doc(s.ws.doc_by_path(&b).unwrap()).unwrap());
    assert_eq!(codecraft_engine::appearance::checked(&s, "view.toggleSoftWrap"), Some(false));
    s.execute("view.toggleSoftWrap", &json!({ "on": false })).unwrap();
    quit(s);

    let s = session(&e);
    let ea = s.ws.editor_for_doc(s.ws.doc_by_path(&a).unwrap()).unwrap();
    let eb = s.ws.editor_for_doc(s.ws.doc_by_path(&b).unwrap()).unwrap();
    assert_eq!(s.ws.editor(ea).unwrap().soft_wrap, Some(true));
    assert!(s.ws.soft_wrap_of(ea));
    assert_eq!(s.ws.editor(eb).unwrap().soft_wrap, Some(false));
    assert!(s.editing.column_mode.contains(&ea));
    assert!(!s.editing.column_mode.contains(&eb));
    quit(s);
}
