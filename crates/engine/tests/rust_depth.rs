//! M12 track RS: Cargo.toml dependency completion and updates against a local crates.io stand-in,
//! the Rust REPL with a fake evcxr, cargo audit through Inspect Code with a fake tool, and (when
//! rust-analyzer is installed) macro expansion and postfix completion.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::inspections::Tool;
use codecraft_engine::{LspStatus, Popup, Session};
use codecraft_workspace::{NotificationLevel, Severity};

fn wait(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; status: {}", s.status);
}

/// A tiny crates.io stand-in on 127.0.0.1: sparse index files and the search API.
fn crates_io() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let index = |versions: &[(&str, bool)]| {
                versions
                    .iter()
                    .map(|(v, y)| format!("{{\"name\":\"x\",\"vers\":\"{v}\",\"deps\":[],\"cksum\":\"0\",\"features\":{{}},\"yanked\":{y}}}\n"))
                    .collect::<String>()
            };
            let (status, body) = match path.as_str() {
                "/se/rd/serde" => ("200 OK", index(&[("1.0.0", false), ("1.0.100", false), ("1.0.200", true), ("1.0.210", false)])),
                "/ra/nd/rand" => ("200 OK", index(&[("0.7.3", false), ("0.8.5", false), ("0.9.0-alpha.1", false)])),
                p if p.starts_with("/api/v1/crates?q=ser") => (
                    "200 OK",
                    r#"{"crates":[{"name":"serde","max_stable_version":"1.0.210","description":"A serialization framework"},{"name":"serde_json","max_version":"1.0.128"},{"name":"rand","max_version":"0.8.5"}]}"#.to_string(),
                ),
                p if p.starts_with("/api/v1/crates") => ("200 OK", r#"{"crates":[]}"#.to_string()),
                _ => ("404 Not Found", String::new()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, hits)
}

fn session(home: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s
}

fn caret(s: &mut Session, offset: usize) {
    s.execute("editor.setSelection", &json!({ "anchor": offset })).unwrap();
}

fn text(s: &Session) -> String {
    s.ws.active().unwrap().1.doc.text()
}

fn offset_of(s: &Session, needle: &str) -> usize {
    let t = text(s);
    let byte = t.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {t}"));
    t[..byte].chars().count()
}

fn type_chars(s: &mut Session, t: &str) {
    for c in t.chars() {
        s.execute("editor.type", &json!({ "text": c.to_string() })).unwrap();
    }
}

fn labels(s: &Session) -> Vec<String> {
    match &s.popup {
        Some(Popup::Completion(c)) => c.visible.iter().map(|i| c.items[*i].label.clone()).collect(),
        _ => Vec::new(),
    }
}

fn manifest(dir: &Path, deps: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    let path = dir.join("Cargo.toml");
    std::fs::write(
        &path,
        format!("[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}"),
    )
    .unwrap();
    path
}

#[test]
fn cargo_toml_completion_inspection_and_updates() {
    let (url, _) = crates_io();
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let path = manifest(proj.path(), "rand = \"0.7\"\n");
    let mut s = session(home.path());
    s.rust.deps.online = true;
    s.rust.deps.auto = true;
    s.rust.deps.index_url = url.clone();
    s.rust.deps.api_url = url.clone();
    s.execute("file.open", &json!({ "path": path })).unwrap();

    // The newer-version inspection on the rand line.
    wait(&mut s, "newer version problem", 20, |s| {
        s.ws.problems_for(&path).iter().any(|p| p.message == "Newer version available: 0.8.5")
    });
    let p = s.ws.problems_for(&path).into_iter().find(|p| p.message.contains("0.8.5")).unwrap().clone();
    assert_eq!((p.severity, p.start.line, p.start.column, p.end.column), (Severity::Info, 6, 8, 11));
    // The index answer is cached on disk under the isolated home.
    assert!(home.path().join("cache/crates-index/rand.json").is_file());

    // Crate name completion pops up while typing in [dependencies] (search runs in the background).
    let end = text(&s).chars().count();
    caret(&mut s, end);
    type_chars(&mut s, "ser");
    wait(&mut s, "crate names", 20, |s| labels(s).contains(&"serde".to_string()));
    let names = labels(&s);
    assert!(names.contains(&"serde_json".to_string()), "{names:?}");
    assert!(!names.contains(&"rand".to_string()), "declared crates are not offered: {names:?}");
    if let Some(Popup::Completion(c)) = &mut s.popup {
        c.selected = c.visible.iter().position(|i| c.items[*i].label == "serde").unwrap();
    }
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    assert!(text(&s).ends_with("serde = \"1.0.210\""), "{}", text(&s));

    // Version completion inside the string: newest first, yanked versions left out.
    let start = offset_of(&s, "1.0.210");
    s.execute("editor.setSelection", &json!({ "anchor": start, "head": start + 7 })).unwrap();
    s.execute("editor.type", &json!({ "text": "1" })).unwrap();
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    wait(&mut s, "versions", 20, |s| !labels(s).is_empty());
    assert_eq!(labels(&s), ["1.0.210", "1.0.100", "1.0.0"]);
    type_chars(&mut s, ".0.1");
    assert_eq!(labels(&s), ["1.0.100"], "typing narrows the versions");
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    assert!(text(&s).ends_with("serde = \"1.0.100\""), "{}", text(&s));

    // Alt+Enter on the rand line: update it, or update everything.
    let rand = offset_of(&s, "rand");
    caret(&mut s, rand);
    s.execute("code.showContextActions", &Value::Null).unwrap();
    let titles: Vec<String> = match &s.popup {
        Some(Popup::Actions { items, .. }) => items.iter().map(|i| i.title.clone()).collect(),
        other => panic!("no actions: {other:?}"),
    };
    assert_eq!(titles, ["Update 'rand' to 0.8.5", "Update All Dependencies…"]);
    s.execute("code.applyContextAction", &json!({ "index": 0 })).unwrap();
    assert!(text(&s).contains("rand = \"0.8\""), "{}", text(&s));
    s.execute("edit.undo", &Value::Null).unwrap();
    assert!(text(&s).contains("rand = \"0.7\""));

    // Update All Dependencies opens the refactoring preview.
    let r = s.execute("rust.cargo.updateAllDependencies", &Value::Null).unwrap();
    assert_eq!(r["updated"].as_array().unwrap().len(), 1, "{r}");
    assert!(s.refactor.preview.is_some());
    s.refactor.preview = None;

    // The listing for agents.
    let list = s.execute("rust.cargo.dependencies", &Value::Null).unwrap();
    let deps = list["dependencies"].as_array().unwrap();
    assert_eq!(deps[0]["status"]["newer"], "0.8.5");
    assert_eq!(deps[1]["latest"], "1.0.210");
    assert!(s.ws.notifications.iter().all(|n| n.level != NotificationLevel::Error));
}

#[test]
fn offline_completion_is_quiet() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let path = manifest(proj.path(), "");
    let mut s = session(home.path());
    // Lookups off (the default of a bare session): nothing is fetched, nothing fails loudly.
    s.rust.deps.index_url = "http://127.0.0.1:9".into();
    s.execute("file.open", &json!({ "path": path })).unwrap();
    let end = text(&s).chars().count();
    caret(&mut s, end);
    type_chars(&mut s, "serde = \"");
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    wait(&mut s, "lookup to finish", 10, |s| !s.rust.deps.busy());
    s.pump();
    assert!(labels(&s).is_empty());
    assert!(!matches!(&s.popup, Some(Popup::Completion(c)) if c.pending), "{:?}", s.popup);
    assert!(s.ws.notifications.iter().all(|n| n.level != NotificationLevel::Error));
    // Online but unreachable: still quiet.
    s.rust.deps.online = true;
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    wait(&mut s, "unreachable lookup", 20, |s| !s.rust.deps.busy());
    s.pump();
    assert!(labels(&s).is_empty());
    assert!(s.ws.notifications.iter().all(|n| n.level != NotificationLevel::Error));
}

#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn rust_repl_with_a_fake_evcxr() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = script(
        tmp.path(),
        "evcxr",
        "#!/bin/sh\n\
         printf 'Welcome to evcxr. For help, type :help\\n'\n\
         printf '>> '\n\
         while IFS= read -r line; do\n\
           case \"$line\" in\n\
             *panic*) printf 'error: boom\\n' >&2; printf '\\302\\222' ;;\n\
             *) printf 'got: %s\\n' \"$line\"; printf '\\302\\221' ;;\n\
           esac\n\
           printf '>> '\n\
         done\n",
    );
    let mut s = session(tmp.path());
    s.rust.repl.program = Some(fake);
    let r = s.execute("tools.rust.rustRepl", &Value::Null).unwrap();
    assert_eq!(r["started"], true);
    assert!(s.ws.layout.is_visible(codecraft_workspace::ToolWindowId::RustRepl));
    wait(&mut s, "ready", 10, |s| s.rust.repl.ready);

    // A multi-line input travels as one line (U+2028) and its output streams back.
    s.execute("rust.repl.setDraft", &json!({ "text": "let a = 1;\nlet b = 2;" })).unwrap();
    assert_eq!(s.execute("rust.repl.execute", &Value::Null).unwrap()["status"], "running");
    wait(&mut s, "output", 10, |s| s.rust.repl.last_status.as_deref() == Some("ok"));
    let t = s.rust.repl.text();
    assert!(t.contains("got: let a = 1;\u{2028}let b = 2;"), "{t}");
    assert!(s.rust.repl.draft.is_empty());

    // Enter on an incomplete input adds an indented line instead of running it.
    s.execute("rust.repl.setDraft", &json!({ "text": "fn f() {" })).unwrap();
    assert_eq!(s.execute("rust.repl.execute", &Value::Null).unwrap()["status"], "incomplete");
    assert_eq!(s.rust.repl.draft, "fn f() {\n    ");

    // Errors are reported.
    s.execute("rust.repl.execute", &json!({ "code": "panic!()" })).unwrap();
    wait(&mut s, "error", 10, |s| s.rust.repl.last_status.as_deref() == Some("error"));
    assert!(s.rust.repl.text().contains("error: boom"));

    // History.
    s.execute("rust.repl.setDraft", &json!({ "text": "" })).unwrap();
    s.execute("rust.repl.historyPrevious", &Value::Null).unwrap();
    assert_eq!(s.rust.repl.draft, "panic!()");
    s.execute("rust.repl.historyPrevious", &Value::Null).unwrap();
    assert_eq!(s.rust.repl.draft, "let a = 1;\nlet b = 2;");

    // Send Selection from an editor.
    let file = tmp.path().join("x.rs");
    std::fs::write(&file, "let z = 3;\n").unwrap();
    s.execute("file.open", &json!({ "path": file })).unwrap();
    s.execute("rust.repl.sendSelection", &Value::Null).unwrap();
    wait(&mut s, "selection", 10, |s| s.rust.repl.text().contains("got: let z = 3;"));

    s.execute("rust.repl.stop", &Value::Null).unwrap();
    assert!(!s.rust.repl.is_alive());
    let t = s.execute("rust.repl.transcript", &Value::Null).unwrap();
    assert_eq!(t["alive"], false);
    s.shutdown();
}

#[test]
fn rust_repl_without_evcxr_shows_the_install_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = session(tmp.path());
    if codecraft_engine::rust_repl::find_evcxr().is_some() {
        eprintln!("evcxr is installed; the hint is not shown");
        return;
    }
    let r = s.execute("tools.rust.rustRepl", &Value::Null).unwrap();
    assert_eq!(r["installed"], false);
    assert!(s.rust.repl.text().contains("cargo install --locked evcxr_repl"));
    assert!(s.execute("rust.repl.execute", &json!({ "code": "1" })).is_err());
}

#[cfg(unix)]
#[test]
fn cargo_audit_advisories_in_inspect_code() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("app");
    let path = manifest(&proj, "time = \"0.1\"\n");
    let report = r#"{"vulnerabilities":{"found":true,"count":1,"list":[{"advisory":{"id":"RUSTSEC-2020-0071","package":"time","title":"Potential segfault in the time crate","description":"Unix-like operating systems may segfault.","url":"https://rustsec.org/advisories/RUSTSEC-2020-0071"},"versions":{"patched":[">=0.2.23"]},"package":{"name":"time","version":"0.1.45"}}]},"warnings":{}}"#;
    let fake = script(
        tmp.path(),
        "cargo-audit",
        &format!("#!/bin/sh\n[ \"$1\" = audit ] || exit 3\ncat <<'EOF'\n{report}\nEOF\nexit 1\n"),
    );
    let mut s = session(&tmp.path().join("home"));
    s.inspections.programs.insert(Tool::CargoAudit, fake);
    s.execute("file.open", &json!({ "path": proj })).unwrap();
    s.execute("rust.cargo.audit", &json!({ "tool": "cargo-audit" })).unwrap();
    wait(&mut s, "inspection", 30, |s| s.inspections.running.is_none());
    let results = s.inspections.results.clone().unwrap();
    let run = results.tools.iter().find(|t| t.tool == Tool::CargoAudit).unwrap();
    assert!(run.ok, "{run:?}");
    let f = results.findings.iter().find(|f| f.code.as_deref() == Some("RUSTSEC-2020-0071")).unwrap();
    assert_eq!((f.path.clone(), f.start.line, f.severity), (path.clone(), 6, Severity::Error));
    assert_eq!(f.url.as_deref(), Some("https://rustsec.org/advisories/RUSTSEC-2020-0071"));
    assert_eq!(f.fixes[0].changes[0].new_text, "0.2");
    // The advisory shows in the Cargo.toml editor, with Alt+Enter actions there.
    assert!(s.ws.problems_for(&path).iter().any(|p| p.message.contains("Potential segfault")));
    s.execute("file.open", &json!({ "path": path })).unwrap();
    let at = offset_of(&s, "time =");
    caret(&mut s, at);
    s.execute("code.showContextActions", &Value::Null).unwrap();
    let titles: Vec<String> = match &s.popup {
        Some(Popup::Actions { items, .. }) => items.iter().map(|i| i.title.clone()).collect(),
        other => panic!("no actions: {other:?}"),
    };
    assert!(titles.contains(&"Open advisory RUSTSEC-2020-0071".to_string()), "{titles:?}");
    let fix = titles.iter().position(|t| t.starts_with("Update 'time'")).unwrap();
    s.execute("code.applyContextAction", &json!({ "index": fix })).unwrap();
    assert!(text(&s).contains("time = \"0.2\""), "{}", text(&s));
    // A missing tool requested explicitly is reported, not fatal.
    s.inspections.programs.insert(Tool::CargoDeny, tmp.path().join("missing-cargo-deny"));
    s.execute("rust.cargo.audit", &json!({ "tool": "cargo-deny" })).unwrap();
    wait(&mut s, "second inspection", 30, |s| s.inspections.running.is_none());
    let run =
        s.inspections.results.as_ref().unwrap().tools.iter().find(|t| t.tool == Tool::CargoDeny).cloned().unwrap();
    assert!(!run.ok);
}

// ------------------------------------------------------------------ rust-analyzer

const MAIN: &str = "macro_rules! double {\n    ($e:expr) => {\n        $e * 2\n    };\n}\n\n#[derive(Debug)]\nstruct Point {\n    x: i32,\n}\n\nfn main() {\n    let v = double!(21);\n    let p = Point { x: v };\n    println!(\"{p:?}\");\n    let n = 5;\n}\n";

#[test]
fn macro_expansion_and_postfix_completion_with_rust_analyzer() {
    if codecraft_lsp::find_rust_analyzer().is_none() {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ra_macros");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ra_macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, MAIN).unwrap();
    let mut s = Session::new();
    s.isolate_home(&tmp.path().join("home"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    wait(&mut s, "server start", 180, |s| s.lsp_status == LspStatus::Ready);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    // Wait until the server has analysed the file (hover answers on `Point`).
    let at = offset_of(&s, "double!(21)") + 2;
    caret(&mut s, at);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        s.execute("rust.showRecursiveMacroExpansion", &Value::Null).unwrap();
        wait(&mut s, "expansion answer", 60, |s| s.rust.macros.pending.is_none());
        if s.rust.macros.view.is_some() || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let view = s.rust.macros.view.clone().expect("no expansion");
    assert_eq!(view.name.trim_end_matches('!'), "double");
    assert!(view.text.contains("21 * 2"), "{}", view.text);
    wait(&mut s, "formatting", 30, |s| !s.rust.macros.busy());

    // A std macro (needs the sysroot loaded: retry until the server knows it).
    let at = offset_of(&s, "println!") + 2;
    caret(&mut s, at);
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        s.execute("rust.showRecursiveMacroExpansion", &Value::Null).unwrap();
        wait(&mut s, "println expansion answer", 60, |s| s.rust.macros.pending.is_none());
        if s.rust.macros.view.as_ref().is_some_and(|v| v.name.starts_with("println")) {
            break;
        }
        assert!(Instant::now() < deadline, "no println! expansion: {}", s.status);
        std::thread::sleep(Duration::from_millis(500));
    }
    wait(&mut s, "formatting", 30, |s| !s.rust.macros.busy());
    let view = s.rust.macros.view.clone().unwrap();
    assert!(view.text.contains("_print("), "{}", view.text);
    assert!(view.text.contains('\n'), "formatted over several lines: {}", view.text);

    // A derive: recent rust-analyzer builds answer `null` for built-in derives; whatever it
    // answers must not break anything.
    let at = offset_of(&s, "derive(Debug)]") + 2;
    caret(&mut s, at);
    s.execute("rust.showRecursiveMacroExpansion", &Value::Null).unwrap();
    wait(&mut s, "derive expansion answer", 60, |s| s.rust.macros.pending.is_none());
    if let Some(v) = s.rust.macros.view.as_ref().filter(|v| v.name.contains("Debug")) {
        assert!(v.text.contains("impl"), "{}", v.text);
    }
    let r = s.execute("rust.macroExpansion.get", &Value::Null).unwrap();
    assert!(r["expansion"].is_object());
    s.execute("rust.macroExpansion.close", &Value::Null).unwrap();
    assert!(s.rust.macros.view.is_none());

    // Postfix completion (rust-analyzer's snippets): `n.dbg` → `dbg!(n)`.
    let at = offset_of(&s, "let n = 5;") + "let n = 5;".len();
    caret(&mut s, at);
    s.execute("editor.type", &json!({ "text": "\n    n." })).unwrap();
    type_chars(&mut s, "dbg");
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    wait(
        &mut s,
        "postfix items",
        60,
        |s| matches!(&s.popup, Some(Popup::Completion(c)) if !c.pending && !c.visible.is_empty()),
    );
    let pos = match &s.popup {
        Some(Popup::Completion(c)) => c.visible.iter().position(|i| c.items[*i].label == "dbg"),
        _ => None,
    };
    let Some(pos) = pos else { panic!("no dbg postfix item: {:?}", labels(&s)) };
    if let Some(Popup::Completion(c)) = &mut s.popup {
        c.selected = pos;
    }
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    let t = text(&s);
    assert!(t.contains("    dbg!(n)"), "{t}");
    assert!(!t.contains("n.dbg"), "{t}");

    // `.match` inserts a match with tab stops (a live template session).
    s.execute("editor.type", &json!({ "text": ";\n    v." })).unwrap();
    type_chars(&mut s, "match");
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    wait(
        &mut s,
        "match postfix",
        60,
        |s| matches!(&s.popup, Some(Popup::Completion(c)) if !c.pending && !c.visible.is_empty()),
    );
    let pos = match &s.popup {
        Some(Popup::Completion(c)) => c.visible.iter().position(|i| c.items[*i].label == "match"),
        _ => None,
    };
    let Some(pos) = pos else { panic!("no match postfix item: {:?}", labels(&s)) };
    if let Some(Popup::Completion(c)) = &mut s.popup {
        c.selected = pos;
    }
    let r = s.execute("editor.completion.accept", &Value::Null).unwrap();
    let t = text(&s);
    assert!(t.contains("match v {"), "{t}");
    assert!(r["placeholders"].as_u64().unwrap_or(0) >= 1, "{r} {t}");
    s.shutdown();
}
