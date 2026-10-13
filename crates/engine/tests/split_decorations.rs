//! Semantic tokens and inlay hints reach every visible split view, not only the focused one,
//! against a real rust-analyzer (skips when it is not installed).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{LspStatus, Session};

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

#[test]
fn both_split_views_get_decorations() {
    if codecraft_lsp::find_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"split_decor\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let main = tmp.path().join("src/main.rs");
    let other = tmp.path().join("src/other.rs");
    std::fs::write(&main, "mod other;\n\nfn main() {\n    let n = other::twice(2);\n    println!(\"{n}\");\n}\n").unwrap();
    std::fs::write(&other, "pub fn twice(x: u32) -> u32 {\n    let y = x * 2;\n    y\n}\n").unwrap();

    let mut s = Session::new();
    s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
    wait(&mut s, "server start", 120, |s| s.lsp_status == LspStatus::Ready);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    s.execute("window.editorTabs.splitRight", &Value::Null).unwrap();
    s.execute("file.open", &json!({ "path": other })).unwrap();
    let docs = s.visible_decoration_docs();
    assert_eq!(docs.len(), 2, "both files are visible");
    // The focused view shows other.rs; main.rs is in the unfocused group and still decorated.
    wait(&mut s, "semantic tokens in both views", 240, |s| {
        docs.iter().all(|d| {
            s.ws.doc(*d).is_some_and(|od| {
                od.semantic.as_ref().is_some_and(|(r, spans)| *r == od.doc.revision() && !spans.is_empty())
            })
        })
    });
    wait(&mut s, "inlay hints in both views", 120, |s| {
        docs.iter().all(|d| {
            s.ws.doc(*d).is_some_and(|od| {
                od.inlay_hints.as_ref().is_some_and(|(r, hints)| *r == od.doc.revision() && !hints.is_empty())
            })
        })
    });
    s.shutdown();
}
