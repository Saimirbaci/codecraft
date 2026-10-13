//! Optimize Imports for Rust against a real rust-analyzer (skips when it is not installed): the
//! `use` groups are sorted and the unused ones removed through the `remove_unused_imports` assist.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{LspStatus, Session};

const MAIN: &str = "use std::fmt::Write;\nuse std::collections::HashMap;\nuse std::collections::BTreeMap;\n\nfn main() {\n    let m: HashMap<u8, u8> = HashMap::new();\n    println!(\"{}\", m.len());\n}\n";

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
fn optimize_imports_removes_unused_rust_imports() {
    if codecraft_lsp::find_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"oi_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let main = tmp.path().join("src/main.rs");
    std::fs::write(&main, MAIN).unwrap();
    let mut s = Session::new();
    s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
    wait(&mut s, "server start", 120, |s| s.lsp_status == LspStatus::Ready);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    // The unused-import diagnostics mean the file is analysed.
    wait(&mut s, "diagnostics", 180, |s| s.ws.problems_for(&main).iter().any(|p| p.message.contains("unused")));
    s.execute("code.optimizeImports", &Value::Null).unwrap();
    wait(&mut s, "unused imports removed", 60, |s| {
        s.ws.active().is_some_and(|(_, d)| !d.doc.text().contains("BTreeMap"))
    });
    let text = s.ws.active().unwrap().1.doc.text();
    assert!(text.starts_with("use std::collections::HashMap;\n"), "{text}");
    assert!(!text.contains("fmt::Write"), "{text}");
    s.shutdown();
}
