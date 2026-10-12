//! End-to-end against a real rust-analyzer (skips when it is not installed).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{LspStatus, Popup, Session};
use codecraft_workspace::ToolWindowId;

const MAIN: &str = "fn main() {\n    let unused = 5;\n    println!(\"{}\", greet(\"x\"));\n}\n\nfn greet(name: &str) -> String {\n    format!(\"{name}!\")\n}\n";

fn project(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ra_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, MAIN).unwrap();
    main
}

fn wait(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}; status: {:?} / {}", s.lsp_status, s.status);
}

fn caret(s: &mut Session, offset: usize) {
    s.execute("editor.setSelection", &json!({ "anchor": offset })).unwrap();
}

fn offset_of(s: &Session, needle: &str) -> usize {
    let text = s.ws.active().unwrap().1.doc.text();
    let byte = text.find(needle).unwrap();
    text[..byte].chars().count()
}

#[test]
fn rust_analyzer_end_to_end() {
    if codecraft_lsp::find_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let main = project(tmp.path());
    let mut s = Session::new();
    s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
    wait(&mut s, "project + server start", 120, |s| s.lsp_status == LspStatus::Ready);
    s.execute("file.open", &json!({ "path": main })).unwrap();

    // Diagnostics: the unused variable warning arrives from rust-analyzer (native or flycheck).
    wait(&mut s, "diagnostics", 180, |s| s.ws.problems_for(&main).iter().any(|p| p.message.contains("unused")));

    // Go to declaration of `greet` from its call.
    let call = offset_of(&s, "greet(\"x\")");
    caret(&mut s, call + 1);
    s.execute("navigate.declarationOrUsages", &Value::Null).unwrap();
    let decl = offset_of(&s, "greet(name");
    wait(&mut s, "goto declaration", 60, |s| {
        s.ws.active().is_some_and(|(e, _)| {
            let h = e.selections.primary().head;
            h >= decl && h <= decl + 5
        })
    });

    // Cmd+B on the declaration shows usages in the Find tool window.
    s.execute("navigate.declarationOrUsages", &Value::Null).unwrap();
    wait(&mut s, "usages", 60, |s| s.ws.find.as_ref().is_some_and(|f| !f.matches.is_empty()));
    assert!(s.ws.layout.is_visible(ToolWindowId::Find));

    // Quick documentation.
    caret(&mut s, call + 1);
    s.execute("view.quickDocumentation", &Value::Null).unwrap();
    wait(&mut s, "hover", 60, |s| matches!(&s.popup, Some(Popup::Hover { text, .. }) if text.contains("fn greet")));
    s.popup = None;

    // Completion: type `gre` at the start of a new line in main and accept `greet`.
    let line_end = offset_of(&s, "    println!");
    caret(&mut s, line_end);
    s.execute("editor.type", &json!({ "text": "    gre" })).unwrap();
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    wait(
        &mut s,
        "completion",
        60,
        |s| matches!(&s.popup, Some(Popup::Completion(c)) if !c.pending && c.visible.iter().any(|i| c.items[*i].label.starts_with("greet"))),
    );
    if let Some(Popup::Completion(c)) = &mut s.popup {
        c.selected = c.visible.iter().position(|i| c.items[*i].label.starts_with("greet")).unwrap();
    }
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    let text = s.ws.active().unwrap().1.doc.text();
    assert!(text.contains("    greet()"), "{text}");
    // The caret sits inside the parentheses, like RustRover.
    let (e, d) = s.ws.active().unwrap();
    assert_eq!(d.doc.char_at(e.selections.primary().head), Some(')'));
    s.execute("edit.undo", &Value::Null).unwrap();
    s.execute("edit.undo", &Value::Null).unwrap();

    // Rename `greet` → `salute` across the file.
    let at = offset_of(&s, "greet(name") + 1;
    caret(&mut s, at);
    s.execute("refactor.rename", &json!({ "newName": "salute" })).unwrap();
    wait(&mut s, "rename", 60, |s| s.ws.active().is_some_and(|(_, d)| d.doc.text().matches("salute").count() == 2));

    s.shutdown();
}
