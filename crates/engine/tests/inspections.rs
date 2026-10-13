//! Inspections end to end: real `cargo clippy` on a tiny crate (skipped when cargo or Clippy is
//! missing), quick-fixes and suppressions through the refactoring preview, Code Cleanup, stack
//! trace analysis and Rearrange Code.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
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
    panic!("timed out waiting for {what}; status: {}", s.status);
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

fn clippy_available() -> bool {
    let cargo = codecraft_engine::cargo::find_cargo();
    Command::new(cargo).args(["clippy", "--version"]).output().is_ok_and(|o| o.status.success())
}

fn findings(s: &mut Session) -> Vec<Value> {
    let r = s.execute("inspections.results", &json!({})).unwrap();
    r["findings"].as_array().cloned().unwrap_or_default()
}

fn id_of(fs: &[Value], inspection: &str) -> u64 {
    fs.iter().find(|f| f["inspection"] == inspection).unwrap_or_else(|| panic!("no {inspection} in {fs:#?}"))["id"]
        .as_u64()
        .unwrap()
}

const MAIN: &str = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\nfn main() {\n    let s = \"héllo wörld\"; let unused = 1;\n    let v: Vec<i32> = vec![1, 2, 3];\n    if v.len() == 0 { println!(\"{}\", s); }\n    println!(\"{}\", add(1, 2));\n}\n";

#[test]
fn clippy_inspection_fixes_and_suppression() {
    if !clippy_available() {
        eprintln!("skipping: cargo clippy is not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    write(&root, "Cargo.toml", "[package]\nname = \"lintme\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n");
    let main = write(&root, "src/main.rs", MAIN);
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.ws.settings.use_clippy = true;
    open_project(&mut s, &root);

    // Without a scope: the dialog.
    let r = s.execute("code.inspectCode", &json!({})).unwrap();
    assert_eq!(r["dialog"], true);
    assert!(s.inspections.dialog.is_some());
    s.execute("inspections.dialog.set", &json!({ "scope": "file" })).unwrap();
    s.execute("inspections.dialog.cancel", &json!({})).unwrap();

    s.execute("code.inspectCode", &json!({ "scope": "project", "tools": ["clippy"] })).unwrap();
    assert!(s.ws.layout.is_visible(ToolWindowId::InspectionResults));
    assert!(s.inspections.busy());
    wait(&mut s, "clippy", 300, |s| !s.inspections.busy());
    let r = s.execute("inspections.results", &json!({})).unwrap();
    assert_eq!(r["tools"][0]["tool"], "clippy", "{r:#}");
    assert_eq!(r["tools"][0]["ok"], true, "{r:#}");
    let fs = findings(&mut s);
    assert_eq!(fs.len(), 3, "{fs:#?}");
    let ret = id_of(&fs, "clippy::needless_return");
    let unused = id_of(&fs, "unused_variables");
    let len = id_of(&fs, "clippy::len_zero");
    assert_eq!(fs[ret as usize]["path"], json!(main));

    // Navigate opens the file at the problem.
    s.execute("inspections.navigate", &json!({ "id": ret })).unwrap();
    let (e, d) = s.ws.active().unwrap();
    assert_eq!(d.path.as_deref(), Some(main.as_path()));
    assert_eq!(d.doc.offset_to_position(e.selections.primary().head), codecraft_text::Position::new(1, 4));

    // A single-file quick-fix applies at once (the file is open: its document changes).
    let r = s.execute("inspections.fix", &json!({ "id": ret })).unwrap();
    assert_eq!(r["preview"], false, "{r}");
    let text = s.ws.active().unwrap().1.doc.text();
    assert!(text.starts_with("fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"), "{text}");
    assert_eq!(findings(&mut s).len(), 2);

    // Suppress the unused variable: #[allow] on `fn main`, then the other findings moved down.
    s.execute("inspections.suppress", &json!({ "id": unused })).unwrap();
    let text = s.ws.active().unwrap().1.doc.text();
    assert!(text.contains("}\n\n#[allow(unused_variables)]\nfn main() {\n"), "{text}");
    let fs = findings(&mut s);
    assert_eq!(fs.len(), 1);
    assert_eq!(fs[0]["start"]["line"], 8, "len_zero moved one line down: {fs:#?}");

    // Fix all through the preview, applied by the refactoring preview's command.
    let r = s.execute("inspections.fixAll", &json!({})).unwrap();
    assert_eq!(r["preview"], true);
    assert!(s.refactor.preview.is_some());
    s.execute("refactor.preview.apply", &json!({})).unwrap();
    s.pump();
    assert!(findings(&mut s).is_empty());
    assert!(s.inspections.results.as_ref().unwrap().resolved.contains(&(len as usize)));
    s.save_all().unwrap();
    let saved = read(&main);
    assert!(saved.contains("if v.is_empty() {"), "{saved}");

    // A stale file refuses fixes instead of corrupting it.
    s.execute("inspections.rerun", &json!({})).unwrap();
    wait(&mut s, "clippy rerun", 300, |s| !s.inspections.busy());
    let fs = findings(&mut s);
    assert!(fs.is_empty(), "everything was fixed or suppressed: {fs:#?}");
}

#[test]
fn code_cleanup_fixes_and_formats() {
    if !clippy_available() {
        eprintln!("skipping: cargo clippy is not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    write(&root, "Cargo.toml", "[package]\nname = \"cleanme\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n");
    let main = write(&root, "src/main.rs", "fn main()   {\n    let v: Vec<i32> = vec![1];\n    if v.len() == 0 { println!(\"empty\"); }\n}\n");
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.ws.settings.use_clippy = true;
    open_project(&mut s, &root);
    s.execute("code.codeCleanup", &json!({ "scope": "file", "path": main, "tools": ["clippy"] })).unwrap();
    wait(&mut s, "cleanup", 300, |s| !s.inspections.busy());
    let pv = s.refactor.preview.as_ref().unwrap_or_else(|| panic!("no preview: {}", s.status));
    assert_eq!(pv.title, "Code Cleanup");
    let results = s.execute("inspections.results", &json!({ "all": true })).unwrap();
    s.execute("refactor.preview.apply", &json!({})).unwrap();
    s.pump();
    let text = read(&main);
    assert_eq!(text, "fn main() {\n    let v: Vec<i32> = vec![1];\n    if v.is_empty() {\n        println!(\"empty\");\n    }\n}\n", "{results:#}");
    assert!(findings(&mut s).is_empty());
}

#[test]
fn stack_traces_and_rearrange_without_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main = write(&root, "src/main.rs", "fn main() {\n    helper();\n}\nuse std::fmt;\nfn helper() {}\nconst X: u8 = 1;\n");
    let mut s = Session::new();
    s.lsp_enabled = false;
    open_project(&mut s, &root);

    // The paste dialog is prefilled from the clipboard.
    s.clipboard = Some(("thread 'main' panicked at src/main.rs:2:5:\nboom".into(), false));
    s.execute("code.analyzeStackTraceOrThreadDump", &json!({})).unwrap();
    assert!(s.inspections.stack_dialog.as_ref().unwrap().text.contains("panicked at"));
    let r = s
        .execute(
            "code.analyzeStackTraceOrThreadDump",
            &json!({ "text": "thread 'main' panicked at src/main.rs:2:5:\nboom\nstack backtrace:\n   0: std::panicking::begin_panic\n             at /rustc/abc/library/std/src/panicking.rs:1:1\n   1: app::main\n             at ./src/main.rs:2:5\n" }),
        )
        .unwrap();
    assert_eq!(r["language"], "Rust");
    assert_eq!(r["firstProjectFrame"], 0);
    assert_eq!(r["projectFrames"], 2);
    assert!(s.inspections.stack_dialog.is_none());
    s.execute("inspections.stackTrace.open", &json!({ "line": 6 })).unwrap();
    let (e, d) = s.ws.active().unwrap();
    assert_eq!(d.path.as_deref(), Some(main.as_path()));
    assert_eq!(d.doc.offset_to_position(e.selections.primary().head), codecraft_text::Position::new(1, 4));
    assert!(s.execute("inspections.stackTrace.open", &json!({ "line": 4 })).is_err(), "std frames are not on disk");
    assert!(s.execute("inspections.stackTrace.open", &json!({ "line": 99 })).is_err());

    // Rearrange Code: preview, then apply.
    let r = s.execute("code.rearrangeCode", &json!({})).unwrap();
    assert_eq!(r["preview"], true);
    s.execute("refactor.preview.apply", &json!({})).unwrap();
    let text = s.ws.active().unwrap().1.doc.text();
    assert_eq!(text, "use std::fmt;\n\nconst X: u8 = 1;\n\nfn main() {\n    helper();\n}\nfn helper() {}\n");
    let r = s.execute("code.rearrangeCode", &json!({})).unwrap();
    assert_eq!(r["changed"], false);

    // Results commands without results are disabled, not panics.
    assert!(s.execute("inspections.fix", &json!({ "id": 0 })).is_err());
    assert!(s.execute("inspections.fixAll", &json!({})).is_err());
    assert!(s.execute("code.inspectCode", &json!({ "scope": "nowhere" })).is_err());
    assert!(s.execute("code.inspectCode", &json!({ "scope": "file", "path": root.join("missing.rs") })).is_err());
    assert!(s.execute("code.inspectCode", &json!({ "scope": "uncommitted" })).is_err(), "not a git repository");
}

#[test]
fn ruff_findings_when_installed() {
    let Some(ruff) = codecraft_lsp::which("ruff") else {
        eprintln!("skipping: ruff is not installed");
        return;
    };
    let ok = Command::new(&ruff).arg("--version").output().is_ok_and(|o| o.status.success());
    if !ok {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let app = write(&root, "app.py", "import os\n\n\ndef f(x):\n    return x\n");
    let mut s = Session::new();
    s.lsp_enabled = false;
    open_project(&mut s, &root);
    s.execute("code.inspectCode", &json!({ "scope": "project", "tools": ["ruff"] })).unwrap();
    wait(&mut s, "ruff", 120, |s| !s.inspections.busy());
    let fs = findings(&mut s);
    let id = id_of(&fs, "F401 unused-import");
    s.execute("inspections.suppress", &json!({ "id": id })).unwrap();
    s.save_all().unwrap();
    assert!(read(&app).starts_with("import os  # noqa: F401\n"), "{}", read(&app));
}
