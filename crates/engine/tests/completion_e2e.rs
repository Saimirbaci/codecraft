//! Completion depth and in-place rename against a real rust-analyzer (skips when it is not
//! installed): argument placeholders walked with Tab, an auto-import through the resolved
//! item's `additionalTextEdits`, and Shift+F6 renaming in the editor (commit and Escape).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{LspStatus, Popup, Session};

const MAIN: &str = "mod other {\n    pub struct Helper;\n}\n\nmod util {\n    pub fn greet(name: &str, times: u8) -> String {\n        name.repeat(times as usize)\n    }\n}\n\nfn main() {\n    let unused = 5;\n    let count = 1;\n    let total = count + count;\n    println!(\"{total}\");\n}\n";

fn wait(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}; status: {:?} / {} / popup {:?}", s.lsp_status, s.status, s.popup);
}

fn text(s: &Session) -> String {
    s.ws.active().unwrap().1.doc.text()
}

fn offset_of(s: &Session, needle: &str) -> usize {
    let t = text(s);
    t[..t.find(needle).unwrap()].chars().count()
}

fn caret(s: &mut Session, offset: usize) {
    s.execute("editor.setSelection", &json!({ "anchor": offset })).unwrap();
}

fn selected(s: &Session) -> String {
    let (e, d) = s.ws.active().unwrap();
    d.doc.slice(e.selections.primary().range())
}

fn typ(s: &mut Session, t: &str) {
    s.execute("editor.type", &json!({ "text": t })).unwrap();
}

/// Ask for completion until an item whose label starts with `label` shows up, then select it.
fn complete(s: &mut Session, label: &str) {
    let start = Instant::now();
    loop {
        s.popup = None;
        s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
        wait(s, "completion", 60, |s| !matches!(&s.popup, Some(Popup::Completion(c)) if c.pending));
        let found = match &mut s.popup {
            Some(Popup::Completion(c)) => {
                let pos = c.visible.iter().position(|i| c.items[*i].label.starts_with(label));
                if let Some(p) = pos {
                    c.selected = p;
                }
                pos.is_some()
            }
            _ => false,
        };
        if found {
            return;
        }
        // Still indexing: ask again.
        assert!(start.elapsed() < Duration::from_secs(90), "no `{label}` completion: {:?}", s.popup);
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn start(dir: &Path) -> Session {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"completion_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, MAIN).unwrap();
    let mut s = Session::new();
    // Before the server starts: rust-analyzer then completes `greet(${1:name}, ${2:times})`.
    s.ws.settings.completion_argument_placeholders = true;
    s.ws.settings.completion_auto_popup = false;
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    wait(&mut s, "server start", 120, |s| s.lsp_status == LspStatus::Ready);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    wait(&mut s, "analysis", 180, |s| s.ws.problems_for(&main).iter().any(|p| p.message.contains("unused")));
    s
}

#[test]
fn completion_and_in_place_rename_with_rust_analyzer() {
    if codecraft_lsp::find_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut s = start(&root);

    // A function with parameters: placeholders for the arguments, walked with Tab.
    let line = offset_of(&s, "    println!");
    caret(&mut s, line);
    typ(&mut s, "    let g = util::gre");
    complete(&mut s, "greet");
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    assert!(text(&s).contains("let g = util::greet(name, times)"), "{}", text(&s));
    assert_eq!(selected(&s), "name");
    typ(&mut s, "\"x\"");
    s.execute("editor.tab", &Value::Null).unwrap();
    assert_eq!(selected(&s), "times");
    typ(&mut s, "2");
    s.execute("editor.tab", &Value::Null).unwrap();
    assert!(text(&s).contains("util::greet(\"x\", 2)"), "{}", text(&s));
    assert!(s.editing.session.is_none());
    typ(&mut s, ";\n");

    // Auto-import: the resolved item carries `use other::Helper;`, applied with the accept.
    typ(&mut s, "    let _h = Helpe");
    complete(&mut s, "Helper");
    wait(&mut s, "resolve", 30, |s| {
        matches!(&s.popup, Some(Popup::Completion(c))
            if c.visible.get(c.selected).is_some_and(|i| c.items[*i].resolved))
    });
    let before_accept = text(&s);
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    let after = text(&s);
    assert!(after.contains("use other::Helper;") || after.contains("use crate::other::Helper;"), "{after}");
    assert!(after.contains("let _h = Helper"), "{after}");
    // One undo takes back the item and its import.
    s.execute("edit.undo", &Value::Null).unwrap();
    assert_eq!(text(&s), before_accept);
    s.execute("edit.redo", &Value::Null).unwrap();

    // In-place rename (Shift+F6): every occurrence in the file is edited together.
    let at = offset_of(&s, "count = 1") + 2;
    caret(&mut s, at);
    s.execute("refactor.rename", &Value::Null).unwrap();
    wait(&mut s, "in-place rename", 60, |s| s.completion.rename.is_some());
    assert_eq!(s.ws.active().unwrap().0.selections.len(), 3);
    assert!(s.popup.is_none());
    typ(&mut s, "amount");
    assert_eq!(text(&s).matches("amount").count(), 3);
    s.execute("editor.enter", &Value::Null).unwrap();
    wait(&mut s, "rename applied", 60, |s| {
        let t = text(s);
        t.contains("let amount = 1;") && t.contains("amount + amount") && !t.contains("count")
    });
    assert!(s.completion.rename.is_none());

    // Escape restores the name.
    let at = offset_of(&s, "amount + amount");
    caret(&mut s, at);
    s.execute("refactor.rename", &Value::Null).unwrap();
    wait(&mut s, "in-place rename", 60, |s| s.completion.rename.is_some());
    let before = text(&s);
    typ(&mut s, "zzz");
    assert!(text(&s).contains("zzz + zzz"));
    s.execute("editor.escape", &Value::Null).unwrap();
    assert_eq!(text(&s), before);
    assert!(s.completion.rename.is_none() && s.editing.session.is_none());

    // Agents keep the headless rename.
    let at = offset_of(&s, "total = ") + 1;
    caret(&mut s, at);
    s.execute("refactor.rename", &json!({ "newName": "sum" })).unwrap();
    wait(&mut s, "headless rename", 60, |s| text(s).contains("let sum = ") && text(s).contains("{sum}"));
    assert!(s.completion.rename.is_none());

    s.shutdown();
}
