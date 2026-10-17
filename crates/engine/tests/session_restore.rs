//! Session restore (M10 track A): a project's editors (splits, tabs, carets, scroll, folds,
//! pinned tabs) and tool-window layout are saved to `<project>/.codecraft/workspace.json` (or the
//! fallback directory) and come back when the project opens again.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::text::{Selection, SelectionSet};
use codecraft_engine::workspace::{NotificationLevel, SplitNode, ToolWindowId};
use codecraft_engine::{CommandError, Session};

fn wait_idle(s: &mut Session) {
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A session that never touches the real config directory.
fn session(fallback: &Path, enabled: bool) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.session_store.fallback_dir = Some(fallback.to_path_buf());
    if enabled {
        s.session_store.enable();
    }
    s
}

fn open(s: &mut Session, path: &Path) {
    s.execute("file.open", &json!({ "path": path })).unwrap();
    wait_idle(s);
}

fn project(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\n").unwrap();
    let long: String = (0..200).map(|i| format!("// line {i}\n")).collect();
    std::fs::write(root.join("src/main.rs"), format!("fn main() {{\n    println!(\"hi\");\n}}\n{long}")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(root.join("notes.md"), "# Notes\n\ntext\n").unwrap();
    root
}

/// Paths shown by each group, in display order.
fn layout_paths(s: &Session) -> Vec<Vec<String>> {
    s.ws.group_ids()
        .iter()
        .map(|g| {
            s.ws.group(*g)
                .unwrap()
                .tabs
                .iter()
                .map(|t| {
                    let d = s.ws.doc(s.ws.editor(*t).unwrap().doc).unwrap();
                    d.path.as_ref().unwrap().file_name().unwrap().to_string_lossy().into_owned()
                })
                .collect()
        })
        .collect()
}

fn active_name(s: &Session) -> Option<String> {
    let (_, d) = s.ws.active()?;
    Some(d.path.as_ref()?.file_name()?.to_string_lossy().into_owned())
}

/// main.rs (caret, scroll, fold, pinned) and lib.rs on the left; notes.md split to the right.
fn arrange(s: &mut Session, root: &Path) {
    open(s, &root.join("src/main.rs"));
    let main = s.ws.active_editor().unwrap();
    s.ws.set_selections(main, SelectionSet::from_vec(vec![Selection::new(3, 7), Selection::caret(20)], 0));
    let e = s.ws.editor_mut(main).unwrap();
    e.scroll_line = 42.0;
    e.folded.insert(0);
    e.pinned = true;
    open(s, &root.join("src/lib.rs"));
    open(s, &root.join("notes.md"));
    s.execute("window.editorTabs.splitAndMoveRight", &Value::Null).unwrap();
    s.execute("view.toolWindows.problems", &Value::Null).unwrap();
}

fn assert_arranged(s: &Session) {
    assert_eq!(layout_paths(s), vec![vec!["main.rs", "lib.rs"], vec!["notes.md"]]);
    assert!(matches!(s.ws.split_layout(), SplitNode::Split { .. }));
    assert_eq!(active_name(s).as_deref(), Some("notes.md"), "the focused group comes back focused");
    let left = s.ws.group(s.ws.group_ids()[0]).unwrap();
    let main = s.ws.editor(left.tabs[0]).unwrap();
    assert_eq!(main.selections.ranges(), &[Selection::new(3, 7), Selection::caret(20)]);
    assert_eq!(main.scroll_line, 42.0);
    assert!(main.pinned && main.folded.contains(&0));
    assert!(s.ws.layout.is_visible(ToolWindowId::Problems));
}

#[test]
fn save_and_restore_through_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path(), "demo");
    let cfg = tmp.path().join("cfg");
    let mut s = session(&cfg, false);
    open(&mut s, &root);
    arrange(&mut s, &root);
    let state = s.execute("session.state", &Value::Null).unwrap();
    assert_eq!(state["groups"].as_array().unwrap().len(), 2);
    assert_eq!(state["focused"], 1);
    let saved = s.execute("session.save", &Value::Null).unwrap();
    let file = root.join(".codecraft/workspace.json");
    assert_eq!(saved["file"], json!(file));
    assert_eq!(saved["tabs"], 3);
    assert!(file.is_file());
    s.shutdown();
    assert!(!cfg.exists(), "a writable project needs no fallback");

    // Autosave is off: opening the project shows no tabs until session.restore.
    let mut s = session(&cfg, false);
    open(&mut s, &root);
    assert!(s.ws.all_tabs().is_empty());
    let r = s.execute("session.restore", &Value::Null).unwrap();
    assert_eq!((r["tabs"].as_u64(), r["groups"].as_u64()), (Some(3), Some(2)));
    assert_arranged(&s);

    // Restoring from the state JSON an agent read works too, and keeps unrelated open tabs.
    let mut t = session(&cfg, false);
    open(&mut t, &root);
    open(&mut t, &root.join("Makefile"));
    t.execute("session.restore", &json!({ "snapshot": state })).unwrap();
    assert_eq!(layout_paths(&t), vec![vec!["main.rs", "lib.rs"], vec!["notes.md", "Makefile"]]);

    // A snapshot file given by path.
    let copy = tmp.path().join("copy.json");
    s.execute("session.save", &json!({ "path": copy })).unwrap();
    let mut u = session(&cfg, false);
    open(&mut u, &root);
    let r = u.execute("session.restore", &json!({ "path": copy })).unwrap();
    assert_eq!(r["file"], json!(copy));
    assert_arranged(&u);
    for x in [&mut s, &mut t, &mut u] {
        x.shutdown();
    }
}

#[test]
fn autosave_on_switch_and_restore_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let a = project(tmp.path(), "alpha");
    let b = project(tmp.path(), "beta");
    let cfg = tmp.path().join("cfg");
    let mut s = session(&cfg, true);
    open(&mut s, &a);
    assert!(s.ws.all_tabs().is_empty(), "nothing saved yet");
    arrange(&mut s, &a);
    // Switching projects saves the old one's session.
    open(&mut s, &b);
    assert!(a.join(".codecraft/workspace.json").is_file());
    assert!(s.ws.all_tabs().is_empty());
    open(&mut s, &b.join("src/lib.rs"));
    // …and switching back restores it.
    open(&mut s, &a);
    assert_arranged(&s);
    // Closing the project saves too: b has its lib.rs tab.
    s.execute("file.closeProject", &Value::Null).unwrap();
    s.shutdown();
    let mut t = session(&cfg, true);
    open(&mut t, &b);
    assert_eq!(layout_paths(&t), vec![vec!["lib.rs"]]);
    t.shutdown();
}

#[test]
fn missing_files_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path(), "demo");
    let cfg = tmp.path().join("cfg");
    let mut s = session(&cfg, false);
    open(&mut s, &root);
    arrange(&mut s, &root);
    s.execute("session.save", &Value::Null).unwrap();
    s.shutdown();
    std::fs::remove_file(root.join("notes.md")).unwrap();
    std::fs::remove_file(root.join("src/lib.rs")).unwrap();
    let mut s = session(&cfg, false);
    open(&mut s, &root);
    let r = s.execute("session.restore", &Value::Null).unwrap();
    assert_eq!(r["tabs"], 1);
    assert_eq!(r["skipped"].as_array().unwrap().len(), 2);
    assert_eq!(layout_paths(&s), vec![vec!["main.rs"]], "the emptied group goes away");
    assert!(!s.ws.is_split());
    assert_eq!(s.ws.docs().count(), 1, "no stray documents");
    s.shutdown();
}

#[test]
fn corrupt_snapshot_is_reported_and_moved_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path(), "demo");
    let cfg = tmp.path().join("cfg");
    std::fs::create_dir_all(root.join(".codecraft")).unwrap();
    std::fs::write(root.join(".codecraft/workspace.json"), "{ \"groups\": [ {\"tabs\": 7 ").unwrap();
    let mut s = session(&cfg, true);
    open(&mut s, &root);
    assert!(s.ws.all_tabs().is_empty());
    assert!(s.ws.notifications.iter().any(|n| n.level == NotificationLevel::Warning && n.title.contains("restore")));
    assert!(root.join(".codecraft/workspace.json.bad").is_file());
    assert!(!root.join(".codecraft/workspace.json").exists());
    // Nothing saved and nothing to restore: an error, not a panic.
    assert!(matches!(s.execute("session.restore", &Value::Null), Err(CommandError::Failed(_))));
    // Bad params.
    assert!(matches!(
        s.execute("session.restore", &json!({ "snapshot": { "split": 5 } })),
        Err(CommandError::BadParams(_))
    ));
    let missing = tmp.path().join("nope.json");
    assert!(s.execute("session.restore", &json!({ "path": missing })).is_err());
    s.shutdown();
}

/// Cheap deterministic pseudo-random values for the fuzz test.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn value(&mut self, depth: u32, paths: &[Value]) -> Value {
        match self.next() % if depth > 4 { 5 } else { 9 } {
            0 => Value::Null,
            1 => json!(self.next().is_multiple_of(2)),
            2 => json!([0u64, 1, 2, 7, u64::MAX, usize::MAX as u64][(self.next() % 6) as usize]),
            3 => json!([-1.0, 0.5, 1e30, -1e30, 3.0][(self.next() % 5) as usize]),
            4 => paths[(self.next() as usize) % paths.len()].clone(),
            5 => Value::Array((0..self.next() % 4).map(|_| self.value(depth + 1, paths)).collect()),
            6 => {
                let keys = [
                    "split",
                    "groups",
                    "focused",
                    "layout",
                    "tabs",
                    "active",
                    "path",
                    "selections",
                    "anchor",
                    "head",
                    "line",
                    "column",
                    "primary",
                    "scroll_line",
                    "scroll_x",
                    "folded",
                    "pinned",
                    "Group",
                    "Split",
                    "axis",
                    "ratio",
                    "first",
                    "second",
                    "left",
                    "bottom_height",
                ];
                let mut m = serde_json::Map::new();
                for _ in 0..self.next() % 6 {
                    let k = keys[(self.next() as usize) % keys.len()];
                    m.insert(k.into(), self.value(depth + 1, paths));
                }
                Value::Object(m)
            }
            7 => json!({ "Group": self.next() % 4 }),
            _ => json!({
                "Split": {
                    "axis": if self.next().is_multiple_of(2) { "Horizontal" } else { "Vertical" },
                    "ratio": 0.3,
                    "first": self.value(depth + 1, paths),
                    "second": self.value(depth + 1, paths),
                }
            }),
        }
    }
}

#[test]
fn hostile_snapshots_never_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path(), "demo");
    let cfg = tmp.path().join("cfg");
    let mut s = session(&cfg, false);
    open(&mut s, &root);
    let paths: Vec<Value> = ["src/main.rs", "src/lib.rs", "notes.md", "missing.rs", "src", ""]
        .iter()
        .map(|p| json!(root.join(p)))
        .chain([json!("relative/x.rs"), json!("/")])
        .collect();
    let mut rng = Lcg(7);
    for i in 0..400 {
        let tab = || json!({ "path": paths[i % paths.len()], "selections": [{ "anchor": { "line": i * 13, "column": 99 }, "head": { "line": 0, "column": 0 } }], "folded": [i, 1_000_000], "scroll_line": i });
        let snapshot = if i % 2 == 0 {
            rng.value(0, &paths)
        } else {
            json!({
                "split": rng.value(1, &paths),
                "groups": [{ "tabs": [tab(), tab()], "active": i }, { "tabs": [tab()] }, rng.value(2, &paths)],
                "focused": i,
                "layout": rng.value(3, &paths),
            })
        };
        match s.execute("session.restore", &json!({ "snapshot": snapshot })) {
            Ok(_) | Err(CommandError::BadParams(_)) => {}
            // A null snapshot means "the saved one", and there is none.
            Err(CommandError::Failed(m)) if snapshot.is_null() && m.starts_with("no saved session") => {}
            Err(e) => panic!("{e} for {snapshot}"),
        }
        assert!(s.ws.group_ids().contains(&s.ws.focused_group()));
        for id in s.ws.all_tabs() {
            let e = s.ws.editor(id).unwrap();
            let d = s.ws.doc(e.doc).unwrap();
            assert!(e.selections.ranges().iter().all(|r| r.end() <= d.doc.len_chars()));
            assert!(e.scroll_line.is_finite());
        }
        // The state stays serializable.
        s.execute("session.state", &Value::Null).unwrap();
    }
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn read_only_project_saves_to_the_fallback_directory() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path(), "demo");
    let cfg = tmp.path().join("cfg");
    let mut s = session(&cfg, true);
    open(&mut s, &root);
    arrange(&mut s, &root);
    // Other features may have made `.codecraft` already: lock it too.
    let dirs = [root.join(".codecraft"), root.clone()];
    for d in dirs.iter().filter(|d| d.is_dir()) {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    // Root ignores permissions; nothing to check then.
    let writable =
        std::fs::write(root.join("probe"), "x").is_ok() || std::fs::write(dirs[0].join("probe"), "x").is_ok();
    s.shutdown();
    for d in dirs.iter().rev().filter(|d| d.is_dir()) {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    if writable {
        return;
    }
    assert!(!root.join(".codecraft/workspace.json").exists());
    let saved: Vec<_> = std::fs::read_dir(&cfg).unwrap().collect();
    assert_eq!(saved.len(), 1, "one snapshot under the fallback directory");
    let mut t = session(&cfg, true);
    open(&mut t, &root);
    assert_arranged(&t);
    t.shutdown();
}

#[test]
fn reopen_setting_is_a_documented_bool() {
    let mut s = Session::new();
    assert_eq!(s.execute("settings.get", &json!({ "key": "reopen_last_project" })).unwrap(), json!(true));
    s.execute("settings.set", &json!({ "key": "reopen_last_project", "value": false })).unwrap();
    assert!(!s.ws.settings.reopen_last_project);
    assert!(s.execute("settings.set", &json!({ "key": "reopen_last_project", "value": "no" })).is_err());
    let def = codecraft_engine::settings::find("reopen_last_project").unwrap();
    assert_eq!(def.page, codecraft_engine::settings::PAGE_SYSTEM);
}

#[cfg(unix)]
fn wait_terminal_text(s: &mut Session, tab: u64, needle: &str) {
    let start = Instant::now();
    loop {
        s.pump();
        let r = s.execute("terminal.read", &json!({ "tab": tab, "scrollback": true })).unwrap();
        if r["text"].as_str().unwrap().contains(needle) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no `{needle}` in:\n{}", r["text"]);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn terminals_come_back_with_their_names_and_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let a = project(&dir, "alpha");
    let b = project(&dir, "beta");
    let cfg = dir.join("cfg");
    let mut s = session(&cfg, true);
    open(&mut s, &a);
    let tab = |v: Value| v["tab"].as_u64().unwrap();
    tab(s.execute("terminal.new", &Value::Null).unwrap());
    let api = tab(s.execute("terminal.new", &json!({ "title": "API server" })).unwrap());
    // A program tab is not kept (it would run again).
    s.execute("terminal.new", &json!({ "command": "/bin/sleep", "args": ["30"] })).unwrap();
    // /bin/sh reports no directory (OSC 7): the shell process is asked.
    s.execute("terminal.sendText", &json!({ "tab": api, "text": "cd src && echo moved-$((1+1))\r" })).unwrap();
    wait_terminal_text(&mut s, api, "moved-2");
    s.execute("terminal.activate", &json!({ "tab": api })).unwrap();
    let state = s.execute("session.state", &Value::Null).unwrap();
    assert_eq!(state["terminals"].as_array().unwrap().len(), 2, "{state}");
    // Closing the window saves the session, then ends the shells.
    s.shutdown();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(a.join(".codecraft/workspace.json")).unwrap()).unwrap();
    let terms = saved["terminals"].as_array().unwrap();
    assert_eq!(terms.len(), 2, "{saved}");
    assert_eq!((&terms[0]["name"], &terms[0]["cwd"]), (&Value::Null, &json!(a)));
    assert_eq!((&terms[1]["name"], &terms[1]["cwd"]), (&json!("API server"), &json!(a.join("src"))));
    assert!(terms.iter().all(|t| t["key"].is_string()), "{saved}");
    assert_eq!(saved["terminal_active"], 1);

    // Restart: the shells start again, named, in their directories, the same tab selected.
    let mut t = session(&cfg, true);
    open(&mut t, &a);
    let tabs = &t.terminal.tabs;
    assert_eq!(tabs.len(), 2);
    assert_eq!((tabs[0].title(), tabs[0].renamed), ("Local", false));
    assert_eq!((tabs[1].title(), tabs[1].renamed), ("API server", true));
    assert_eq!(t.terminal.active, 1);
    let api = tabs[1].id;
    t.execute("terminal.sendText", &json!({ "tab": api, "text": "echo at-$(pwd -P)\r" })).unwrap();
    wait_terminal_text(&mut t, api, &format!("at-{}", a.join("src").display()));

    // Another project in this window: alpha's terminals close (and are saved); beta has none.
    open(&mut t, &b);
    assert!(t.terminal.tabs.is_empty());
    open(&mut t, &a);
    assert_eq!(t.terminal.tabs.len(), 2, "back again");
    t.shutdown();

    // A hostile snapshot: too many tabs, long names, relative or missing directories.
    let mut u = session(&cfg, false);
    open(&mut u, &b);
    let bad = json!({ "snapshot": { "terminals": [{ "cwd": 7 }] } });
    assert!(matches!(u.execute("session.restore", &bad), Err(CommandError::BadParams(_))));
    let many: Vec<Value> = (0..40)
        .map(|i| match i % 3 {
            0 => json!({ "name": "x".repeat(500), "cwd": "relative/dir" }),
            1 => json!({ "cwd": "/no/such/dir" }),
            _ => json!({ "name": "  ", "cwd": b }),
        })
        .collect();
    u.execute("session.restore", &json!({ "snapshot": { "terminals": many, "terminal_active": 99 } })).unwrap();
    let tabs = &u.terminal.tabs;
    assert_eq!(tabs.len(), codecraft_engine::terminal::MAX_TABS);
    assert!(tabs.iter().all(|t| t.name.chars().count() <= 110 && t.cwd.is_dir()));
    assert!(tabs.iter().any(|t| t.cwd == b) && tabs.iter().filter(|t| t.renamed).count() == 11);
    assert_eq!(u.terminal.active, 0);
    u.shutdown();
}
