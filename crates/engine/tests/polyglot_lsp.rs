//! Language servers other than rust-analyzer, end to end (each part skips when the server is
//! not installed): clangd for C++, zls for Zig. One session runs both servers at once.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{LspStatus, ServerKind, Session};

fn wait(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&Session) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("timed out waiting for {what}");
    false
}

fn installed(kind: ServerKind, root: &Path) -> bool {
    let found = codecraft_lsp::find_server(kind, root).is_some();
    if !found {
        eprintln!("{} not installed; skipping that part", kind.label());
    }
    found
}

#[test]
fn clangd_and_zls_side_by_side() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cpp = root.join("main.cpp");
    std::fs::write(&cpp, "int main() {\n    int x = undefined_symbol;\n    return x;\n}\n").unwrap();
    let zig = root.join("main.zig");
    // A syntax error: zls reports parse errors on open (type errors need a build).
    std::fs::write(&zig, "pub fn main() void {\n    const x = ;\n}\n").unwrap();

    let mut s = Session::new();
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait(&mut s, "project", 60, |s| s.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));

    let want_clangd = installed(ServerKind::Clangd, root);
    let want_zls = installed(ServerKind::Zls, root);

    if want_clangd {
        s.execute("file.open", &json!({ "path": cpp })).unwrap();
        assert!(wait(&mut s, "clangd ready", 60, |s| s.server_status(ServerKind::Clangd) == LspStatus::Ready));
        assert!(
            wait(&mut s, "clangd diagnostics", 60, |s| s
                .ws
                .problems_for(&cpp)
                .iter()
                .any(|p| p.message.contains("undefined_symbol"))),
            "clangd reported no diagnostic"
        );
        // Hover goes to clangd (the active file is C++).
        s.execute("editor.setSelection", &json!({ "anchor": 20 })).unwrap();
        assert!(s.execute("view.quickDocumentation", &Value::Null).is_ok());
    }
    if want_zls {
        s.execute("file.open", &json!({ "path": zig })).unwrap();
        assert!(wait(&mut s, "zls ready", 60, |s| s.server_status(ServerKind::Zls) == LspStatus::Ready));
        // zls reports parse/semantic errors on open (the exact text varies by version).
        assert!(
            wait(&mut s, "zls diagnostics", 30, |s| !s.ws.problems_for(&zig).is_empty()),
            "zls reported no parse error"
        );
    }
    if want_clangd && want_zls {
        // Both run at once, each with its own status.
        assert_eq!(s.server_status(ServerKind::Clangd), LspStatus::Ready);
        assert_eq!(s.server_status(ServerKind::Zls), LspStatus::Ready);
        let status = s.execute("lsp.status", &Value::Null).unwrap();
        assert!(status.to_string().contains("clangd"));
    }
    s.shutdown();
}
