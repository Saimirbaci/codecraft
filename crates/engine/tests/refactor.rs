//! Refactorings: file operations and previews headlessly, and the LSP refactorings, hierarchies
//! and file renames against a real rust-analyzer (skipped when it is not installed).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::refactor::{RefactorDialog, UsageKind};
use codecraft_engine::{LspStatus, Popup, Session};
use codecraft_workspace::ToolWindowId;

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

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn open_project(s: &mut Session, root: &Path) {
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait(s, "project scan", 60, |s| s.ws.project.as_ref().is_some_and(|p| p.tree.len() > 1) && !s.jobs_running());
}

#[test]
fn file_operations_without_a_language_server() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main = write(&root, "src/main.rs", "mod util;\nmod extra;\n\nfn main() {\n    util::f();\n}\n");
    let util = write(&root, "src/util.rs", "pub fn f() {}\n");
    let extra = write(&root, "src/extra.rs", "// extra\n");
    let notes = write(&root, "notes.txt", "see notes.txt\n");
    std::fs::create_dir_all(root.join("src/sub")).unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    open_project(&mut s, &root);

    // Copy into a directory, then a second copy is refused.
    let r = s.execute("refactor.copyFile", &json!({ "path": util, "to": root.join("src/sub") })).unwrap();
    assert_eq!(r["to"], json!(root.join("src/sub/util.rs")));
    assert_eq!(read(&root.join("src/sub/util.rs")), "pub fn f() {}\n");
    assert!(s.execute("refactor.copyFile", &json!({ "path": util, "to": root.join("src/sub") })).is_err());

    // Rename in place (no server: no `mod` updates, just the move), following the open editor.
    s.execute("file.open", &json!({ "path": extra })).unwrap();
    let r = s.execute("refactor.renameFile", &json!({ "path": extra, "newName": "more.rs" })).unwrap();
    assert_eq!(r["done"], true);
    let more = root.join("src/more.rs");
    assert!(more.is_file() && !extra.exists());
    assert_eq!(s.ws.active().unwrap().1.path.as_deref(), Some(more.as_path()));
    assert!(s.execute("refactor.renameFile", &json!({ "path": more, "newName": "a/b.rs" })).is_err());

    // Move into a directory.
    assert!(s.execute("refactor.safeDelete", &json!({ "path": root, "force": true })).is_err());
    assert!(root.join("Cargo.toml").exists() || root.join("src").exists());
    let r = s.execute("refactor.moveFile", &json!({ "path": more, "to": root.join("src/sub") })).unwrap();
    assert_eq!(r["to"], json!(root.join("src/sub/more.rs")));
    assert!(root.join("src/sub/more.rs").is_file());
    assert!(s.execute("refactor.moveFile", &json!({ "path": root.join("src"), "to": root.join("src/sub") })).is_err());

    // Without a destination the dialog opens; cancel closes it.
    s.execute("refactor.moveFile", &json!({ "path": util })).unwrap();
    assert!(matches!(s.refactor.dialog, Some(RefactorDialog::MoveFile { .. })));
    s.execute("refactor.dialog.cancel", &Value::Null).unwrap();
    assert!(s.refactor.dialog.is_none());

    // Safe Delete: the usage search runs in the background, then the confirmation deletes the
    // file and its `mod` declaration.
    s.execute("refactor.safeDelete", &json!({ "path": util })).unwrap();
    wait(&mut s, "usages", 30, |s| {
        matches!(&s.refactor.dialog, Some(RefactorDialog::SafeDelete { usages: Some(_), .. }))
    });
    let Some(RefactorDialog::SafeDelete { usages: Some(usages), .. }) = s.refactor.dialog.clone() else {
        unreachable!()
    };
    assert!(usages.iter().any(|u| u.kind == UsageKind::ModDeclaration && u.path == main && u.line == 0));
    assert!(usages.iter().any(|u| u.kind == UsageKind::Reference && u.path == main && u.line == 4));
    // The confirmation must name the dialog's file.
    assert!(s.execute("refactor.safeDelete.confirm", &json!({ "path": notes })).is_err());
    assert!(util.exists());
    let report = s.execute("refactor.safeDelete.confirm", &json!({ "path": util })).unwrap();
    assert_eq!(report["errors"], json!([]));
    assert!(!util.exists());
    assert_eq!(read(&main), "mod extra;\n\nfn main() {\n    util::f();\n}\n");

    // Forced delete of a plain file.
    s.execute("refactor.safeDelete", &json!({ "path": notes, "force": true })).unwrap();
    assert!(!notes.exists());

    // An agent-provided workspace edit: preview, exclude one entry, apply.
    let created = root.join("src/new.rs");
    let uri = |p: &Path| codecraft_lsp::path_to_uri(p).unwrap().as_str().to_string();
    let edit = json!({ "documentChanges": [
        { "kind": "create", "uri": uri(&created) },
        { "textDocument": { "uri": uri(&created), "version": null },
          "edits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "pub fn n() {}\n" }] },
        { "textDocument": { "uri": uri(&main), "version": null },
          "edits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "mod new;\n" }] },
    ]});
    let r = s.execute("refactor.preview.open", &json!({ "title": "Add module", "edit": edit })).unwrap();
    assert_eq!(r["entries"].as_array().unwrap().len(), 3);
    let state = s.execute("refactor.state", &Value::Null).unwrap();
    assert!(state["preview"]["diff"].as_str().unwrap().contains("+pub fn n() {}"));
    s.execute("refactor.preview.include", &json!({ "index": 2, "include": false })).unwrap();
    let report = s.execute("refactor.preview.apply", &Value::Null).unwrap();
    assert_eq!(report["applied"], 2);
    assert_eq!(report["skipped"], 1);
    assert_eq!(read(&created), "pub fn n() {}\n");
    assert!(!read(&main).contains("mod new;"));
    assert!(s.refactor.preview.is_none());
    assert!(s.execute("refactor.preview.apply", &Value::Null).is_err());
    s.shutdown();
}

// ---------------------------------------------------------------- rust-analyzer

const MAIN: &str = r#"mod util;

trait Speak {
    fn speak(&self) -> String;
    fn loud(&self) -> String {
        self.speak().to_uppercase()
    }
}

struct Dog;

impl Speak for Dog {
    fn speak(&self) -> String {
        String::from("woof")
    }
}

struct Cat;

impl Speak for Cat {}

fn main() {
    let total = 1 + 2;
    println!("{} {}", util::greet("x"), total);
    println!("{}", Dog.speak());
}
"#;

fn offset_of(s: &Session, needle: &str) -> usize {
    let text = s.ws.active().unwrap().1.doc.text();
    let byte = text.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {text}"));
    text[..byte].chars().count()
}

fn select(s: &mut Session, anchor: usize, head: usize) {
    s.execute("editor.setSelection", &json!({ "anchor": anchor, "head": head })).unwrap();
}

fn text(s: &Session) -> String {
    s.ws.active().unwrap().1.doc.text()
}

fn caret_line(s: &Session) -> usize {
    let (e, d) = s.ws.active().unwrap();
    d.doc.char_to_line(e.selections.primary().head)
}

#[test]
fn rust_analyzer_refactorings() {
    if codecraft_lsp::find_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"ra_refactor\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    );
    let main = write(&root, "src/main.rs", MAIN);
    let util = write(&root, "src/util.rs", "pub fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n");
    let mut s = Session::new();
    open_project(&mut s, &root);
    wait(&mut s, "server start", 120, |s| s.lsp_status == LspStatus::Ready);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    // Analysis is done once the missing-items error for `Cat` is reported.
    wait(&mut s, "diagnostics", 180, |s| s.ws.problems_for(&main).iter().any(|p| p.message.contains("speak")));

    // Rename refused by the server: a conflict dialog, no edit.
    let at = offset_of(&s, "fn main");
    select(&mut s, at, at);
    s.execute("refactor.rename", &Value::Null).unwrap();
    wait(
        &mut s,
        "rename refusal",
        60,
        |s| matches!(&s.popup, Some(Popup::Message { title, .. }) if title == "Cannot Rename"),
    );
    s.popup = None;

    // Rename asks first (prepareRename) and offers the symbol's name in the popup (in-place
    // rename is off here; `completion_e2e.rs` covers it).
    // (rust-analyzer may still be loading the `util` module: retry while it says no.)
    s.ws.settings.rename_in_place = false;
    let at = offset_of(&s, "greet(\"x\")") + 1;
    let start = Instant::now();
    loop {
        select(&mut s, at, at);
        s.popup = None;
        s.execute("refactor.rename", &Value::Null).unwrap();
        wait(&mut s, "rename popup", 60, |s| matches!(&s.popup, Some(Popup::Rename { .. } | Popup::Message { .. })));
        if matches!(&s.popup, Some(Popup::Rename { name, .. }) if name == "greet")
            || start.elapsed() > Duration::from_secs(60)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(matches!(&s.popup, Some(Popup::Rename { name, .. }) if name == "greet"), "{:?}", s.popup);

    // Call hierarchy of `greet`: called from `main`.
    s.popup = None;
    s.execute("navigate.callHierarchy", &Value::Null).unwrap();
    assert!(s.ws.layout.is_visible(ToolWindowId::Hierarchy));
    wait(&mut s, "callers", 60, |s| {
        s.refactor.hierarchy.as_ref().is_some_and(|h| h.nodes.iter().any(|n| n.name == "main" && n.depth == 1))
    });
    let callees = s.execute("hierarchy.setDirection", &json!({ "direction": "callees" }));
    assert!(callees.is_ok(), "{callees:?}");
    wait(&mut s, "callees", 60, |s| {
        s.refactor.hierarchy.as_ref().is_some_and(|h| {
            h.roots.first().and_then(|r| h.nodes.get(*r)).is_some_and(|n| n.children.is_some() && !n.loading)
        })
    });

    // Rename across two files opens the preview; applying changes the open document and the
    // closed file.
    let at = offset_of(&s, "greet(\"x\")") + 1;
    select(&mut s, at, at);
    s.execute("refactor.rename", &json!({ "newName": "salute" })).unwrap();
    wait(&mut s, "rename preview", 60, |s| s.refactor.preview.is_some());
    assert_eq!(s.refactor.preview.as_ref().unwrap().files(), 2);
    let report = s.execute("refactor.preview.apply", &Value::Null).unwrap();
    assert_eq!(report["errors"], json!([]));
    assert!(text(&s).contains("util::salute(\"x\")"));
    assert!(read(&util).contains("pub fn salute("));

    // Refactor This on an expression lists assists plus Rename…
    let at = offset_of(&s, "1 + 2");
    select(&mut s, at, at + 5);
    s.execute("refactor.refactorThis", &Value::Null).unwrap();
    wait(
        &mut s,
        "refactor this",
        60,
        |s| matches!(&s.popup, Some(Popup::Actions { pending: false, items, .. }) if items.len() > 1),
    );
    let Some(Popup::Actions { items, .. }) = &s.popup else { unreachable!() };
    assert_eq!(items[0].title, "Rename…");
    assert!(items.iter().any(|i| i.title.contains("variable")), "{items:?}");
    s.popup = None;

    // Extract Variable (single file: applied at once).
    select(&mut s, at, at + 5);
    s.execute("refactor.extract.variable", &Value::Null).unwrap();
    wait(&mut s, "extract variable", 60, |s| text(s).contains("= 1 + 2;") && !text(s).contains("let total = 1 + 2;"));

    // Implement Methods on `impl Speak for Cat {}`.
    let at = offset_of(&s, "impl Speak for Cat") + 5;
    select(&mut s, at, at);
    s.execute("code.implementMethods", &Value::Null).unwrap();
    wait(&mut s, "implement members", 60, |s| text(s).matches("fn speak(&self) -> String").count() == 3);

    // Generate on a struct lists generate assists.
    let at = offset_of(&s, "struct Dog") + 8;
    select(&mut s, at, at);
    s.execute("code.generate", &Value::Null).unwrap();
    wait(
        &mut s,
        "generate",
        60,
        |s| matches!(&s.popup, Some(Popup::Actions { pending: false, items, .. }) if !items.is_empty()),
    );
    s.popup = None;

    // Super Method: from Dog's `speak` to the trait's declaration.
    let at = offset_of(&s, "fn speak(&self) -> String {\n        String::from") + 4;
    select(&mut s, at, at);
    s.execute("navigate.superMethod", &Value::Null).unwrap();
    wait(&mut s, "super method", 60, |s| caret_line(s) == 3);

    // Type hierarchy of the trait: its implementations.
    let at = offset_of(&s, "trait Speak") + 7;
    select(&mut s, at, at);
    s.execute("navigate.typeHierarchy", &Value::Null).unwrap();
    wait(&mut s, "implementations", 60, |s| {
        s.refactor.hierarchy.as_ref().is_some_and(|h| {
            // Implementations ("impl Speak for Dog") or, with server type hierarchies, the types.
            h.nodes.iter().any(|n| n.depth == 1 && n.name.contains("Dog"))
                && h.nodes.iter().any(|n| n.depth == 1 && n.name.contains("Cat"))
        })
    });

    // Renaming util.rs asks rust-analyzer for the `mod` update and previews it.
    s.execute("refactor.renameFile", &json!({ "path": util, "newName": "helpers.rs" })).unwrap();
    wait(&mut s, "file rename preview", 60, |s| s.refactor.preview.is_some() || root.join("src/helpers.rs").exists());
    let preview = s.refactor.preview.clone().expect("rust-analyzer's `mod` update is previewed");
    assert!(preview.entries.iter().any(|e| e.after.contains("mod helpers;")), "{preview:?}");
    let report = s.execute("refactor.preview.apply", &Value::Null).unwrap();
    assert_eq!(report["errors"], json!([]));
    let doc = s.ws.doc_by_path(&main).and_then(|id| s.ws.doc(id)).unwrap().doc.text();
    assert!(doc.contains("mod helpers;") && doc.contains("helpers::salute"), "{doc}");
    assert!(root.join("src/helpers.rs").is_file() && !util.exists());
    s.shutdown();
}
