//! Analysis in the editor (M15 track EA), end to end: findings of the analysis on save (the real
//! ruff, cargo clippy and clangd when installed; fakes for debouncing and cancellation) and of
//! Inspect Code shown as editor problems, moving with edits, cleared when fixed or re-run,
//! Alt+Enter fixes and suppressions, F2 navigation, `analysis.fileFindings` for agents, the
//! setting that turns it off, cargo-deny's network fetch only when the profile allows it, and
//! the Python language-server settings.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::inspections::Tool;
use codecraft_engine::{Popup, Session};
use codecraft_workspace::ProblemSource;

fn pump_until(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}; status: {}; on save: {:?}", s.status, s.analysis.editor.on_save);
}

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("/usr/bin/which").arg(name).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())).filter(|p| p.is_file())
}

struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    base: PathBuf,
}

fn project(files: &[(&str, &str)]) -> Project {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    for (rel, text) in files {
        write(&root, rel, text);
    }
    Project { _tmp: tmp, root, home: base.join("home"), base }
}

fn session(p: &Project) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&p.home);
    s.analysis.editor.on_save.debounce = Duration::ZERO;
    // These tests are about the analysis on save; analysis while typing has its own
    // (`analysis_live.rs`, M16 LT).
    s.ws.settings.analysis_while_typing_python = false;
    s.ws.settings.analysis_while_typing_cpp = false;
    s.execute("file.open", &json!({ "path": p.root })).unwrap();
    pump_until(&mut s, "project scan", 60, |s| s.ws.project.as_ref().is_some_and(|p| p.tree.len() > 1) && !s.jobs_running());
    s
}

fn text(s: &Session) -> String {
    s.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

fn caret_at(s: &mut Session, needle: &str) {
    let at = text(s).find(needle).unwrap();
    let chars = text(s)[..at].chars().count();
    s.execute("editor.setSelection", &json!({ "anchor": chars })).unwrap();
}

fn caret_line(s: &Session) -> usize {
    let (e, d) = s.ws.active().unwrap();
    d.doc.char_to_line(e.selections.primary().head)
}

/// Edit the active document and save it (the analysis on save starts).
fn type_and_save(s: &mut Session, at_end: &str) {
    let len = s.ws.active().unwrap().1.doc.len_chars();
    s.execute("editor.setSelection", &json!({ "anchor": len })).unwrap();
    s.execute("editor.type", &json!({ "text": at_end })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
}

fn analysis_problems(s: &Session, path: &Path) -> Vec<(usize, String)> {
    s.ws.problems_for(path)
        .into_iter()
        .filter(|p| p.source == ProblemSource::Analysis)
        .map(|p| (p.start.line, p.code.clone().unwrap_or_default()))
        .collect()
}

fn action_titles(s: &Session) -> Vec<String> {
    match &s.popup {
        Some(Popup::Actions { items, .. }) => items.iter().map(|i| i.title.clone()).collect(),
        other => panic!("no context actions: {other:?}"),
    }
}

const APP_PY: &str = "import os\nimport sys\n\nprint(sys.argv)\n";

/// A stand-in for `ruff check --output-format=json` reporting F401 on `os` in `file` (line 1),
/// recording each call in `log` and sleeping `delay` seconds first.
fn fake_ruff(dir: &Path, file: &Path, log: &Path, delay: &str) -> PathBuf {
    let report = json!([{
        "code": "F401", "filename": file, "message": "`os` imported but unused",
        "location": {"row": 1, "column": 8}, "end_location": {"row": 1, "column": 10},
        "fix": {"applicability": "safe", "message": "Remove unused import: `os`",
                "edits": [{"content": "", "location": {"row": 1, "column": 1}, "end_location": {"row": 2, "column": 1}}]},
        "url": "https://docs.astral.sh/ruff/rules/unused-import"
    }]);
    script(dir, "fake-ruff", &format!("echo run >> '{}'\nsleep {delay}\ncat <<'JSON'\n{report}\nJSON\nexit 1", log.display()))
}

fn runs(log: &Path) -> usize {
    std::fs::read_to_string(log).map(|t| t.lines().count()).unwrap_or(0)
}

#[test]
fn ruff_on_save_findings_move_with_edits_and_alt_enter_fixes_them() {
    let Some(ruff) = which("ruff") else {
        eprintln!("ruff is not installed; skipping");
        return;
    };
    let p = project(&[("app.py", APP_PY)]);
    let mut s = session(&p);
    s.inspections.programs.insert(Tool::Ruff, ruff);
    let app = p.root.join("app.py");
    s.execute("file.open", &json!({ "path": app })).unwrap();
    type_and_save(&mut s, "x = 1\n");
    pump_until(&mut s, "ruff on save", 30, |s| !s.analysis.editor.on_save.busy() && !analysis_problems(s, &app).is_empty());
    assert_eq!(analysis_problems(&s, &app), vec![(0, "F401".to_string())], "the real ruff found the unused import");
    let summary = s.analysis.editor.on_save.history.last().cloned().unwrap();
    assert!(!summary.cancelled && summary.tools.iter().any(|t| t.starts_with("ruff:")), "{summary:?}");

    // Agents read the file's findings: absolute path, 1-based lines, the fix.
    let v = s.execute("analysis.fileFindings", &json!({ "path": "app.py" })).unwrap();
    assert_eq!(v["path"], json!(app));
    let f = &v["findings"][0];
    assert_eq!((f["analyzer"].as_str(), f["rule"].as_str(), f["line"].as_u64()), (Some("ruff"), Some("F401"), Some(1)));
    assert_eq!(f["source"], "onSave");
    assert!(f["fixes"][0]["title"].as_str().unwrap().contains("Remove unused import"), "{v}");

    // A line typed above moves the underline down with the text (unsaved).
    s.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
    s.execute("editor.type", &json!({ "text": "# top\n" })).unwrap();
    s.pump();
    assert_eq!(analysis_problems(&s, &app), vec![(1, "F401".to_string())]);
    // F2 goes to it.
    s.execute("editor.setSelection", &json!({ "anchor": 0 })).unwrap();
    s.execute("navigate.nextHighlightedError", &Value::Null).unwrap();
    assert_eq!(caret_line(&s), 1);

    // The hover names the analyzer, the rule and the fix.
    let hover = codecraft_engine::analysis::editor::hover_lines(&s, &app, codecraft_text::Position::new(1, 8));
    assert!(hover[0].starts_with("Ruff F401: `os` imported but unused"), "{hover:?}");

    // Alt+Enter on the underline: the analyzer's fix and suppression (no language server here).
    caret_at(&mut s, "os\n");
    s.execute("code.showContextActions", &Value::Null).unwrap();
    let titles = action_titles(&s);
    let fix = titles.iter().position(|t| t.starts_with("Ruff: Remove unused import")).unwrap_or_else(|| panic!("{titles:?}"));
    assert!(titles.iter().any(|t| t == "Suppress with # noqa: F401"), "{titles:?}");
    s.execute("code.applyContextAction", &json!({ "index": fix })).unwrap();
    s.pump();
    assert_eq!(text(&s), "# top\nimport sys\n\nprint(sys.argv)\nx = 1\n");
    assert!(analysis_problems(&s, &app).is_empty(), "the fixed finding is gone");
    assert!(s.execute("analysis.editorFix", &json!({ "key": f["key"] })).is_err(), "its key is stale now");

    // Saving re-runs ruff: clean.
    s.execute("file.saveAll", &Value::Null).unwrap();
    pump_until(&mut s, "second run", 30, |s| !s.analysis.editor.on_save.busy());
    assert!(analysis_problems(&s, &app).is_empty());
    s.shutdown();
}

#[test]
fn suppression_from_alt_enter_and_edits_on_the_flagged_text() {
    let p = project(&[("app.py", APP_PY)]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let fake = fake_ruff(&p.base, &app, &log, "0");
    let mut s = session(&p);
    s.inspections.programs.insert(Tool::Ruff, fake);
    s.execute("file.open", &json!({ "path": app })).unwrap();
    s.execute("analysis.analyzeFile", &json!({ "path": app })).unwrap();
    pump_until(&mut s, "analysis", 30, |s| !s.analysis.editor.on_save.busy());
    assert_eq!(analysis_problems(&s, &app), vec![(0, "F401".to_string())]);
    // Suppress through Alt+Enter: `# noqa: F401` at the line end, the finding leaves.
    caret_at(&mut s, "os");
    s.execute("code.showContextActions", &Value::Null).unwrap();
    let titles = action_titles(&s);
    let i = titles.iter().position(|t| t == "Suppress with # noqa: F401").unwrap();
    s.execute("code.applyContextAction", &json!({ "index": i })).unwrap();
    s.pump();
    assert!(text(&s).starts_with("import os  # noqa: F401\n"), "{}", text(&s));
    assert!(analysis_problems(&s, &app).is_empty());

    // Typing inside a flagged range drops the finding (it is stale until the next save).
    s.execute("analysis.analyzeFile", &json!({ "path": app })).unwrap();
    pump_until(&mut s, "re-analysis", 30, |s| !s.analysis.editor.on_save.busy());
    assert_eq!(analysis_problems(&s, &app).len(), 1);
    s.execute("editor.setSelection", &json!({ "anchor": 8 })).unwrap();
    s.execute("editor.type", &json!({ "text": "z" })).unwrap();
    s.pump();
    assert!(analysis_problems(&s, &app).is_empty());
    s.shutdown();
}

#[test]
fn saves_are_debounced_and_a_newer_save_cancels_the_run() {
    let p = project(&[("app.py", APP_PY)]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p);
    s.analysis.editor.on_save.debounce = Duration::from_millis(300);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "2"));
    s.execute("file.open", &json!({ "path": app })).unwrap();
    // Three quick saves: one run.
    for i in 0..3 {
        type_and_save(&mut s, &format!("a{i} = 1\n"));
        s.pump();
    }
    assert!(s.analysis.editor.on_save.pending(&app));
    pump_until(&mut s, "the run starts", 10, |s| s.analysis.editor.on_save.running_for(&app));
    std::thread::sleep(Duration::from_millis(300));
    // Saving again while it runs cancels it; the next run covers the file.
    type_and_save(&mut s, "b = 2\n");
    pump_until(&mut s, "both runs", 30, |s| !s.analysis.editor.on_save.busy());
    let h = &s.analysis.editor.on_save.history;
    assert_eq!(h.len(), 2, "{h:?}");
    assert!(h[0].cancelled && !h[1].cancelled, "{h:?}");
    // Three saves, then one more: at most two processes (the cancelled one may be killed before
    // it starts on a loaded machine).
    assert!((1..=2).contains(&runs(&log)), "the debounce merged the first three saves: {}", runs(&log));
    // The UI thread never waited: the fake takes 2 s per run, a pump returns at once.
    let t = Instant::now();
    type_and_save(&mut s, "c = 3\n");
    std::thread::sleep(Duration::from_millis(350));
    s.pump();
    assert!(t.elapsed() < Duration::from_millis(1500));
    assert!(s.analysis.editor.on_save.running_for(&app));
    pump_until(&mut s, "third run", 30, |s| !s.analysis.editor.on_save.busy());
    assert_eq!(analysis_problems(&s, &app), vec![(0, "F401".to_string())]);

    // Turned off: saves analyze nothing.
    let before = runs(&log);
    s.execute("settings.set", &json!({ "key": "analysis_on_save", "value": false })).unwrap();
    type_and_save(&mut s, "d = 4\n");
    s.pump();
    assert!(!s.analysis.editor.on_save.busy());
    assert_eq!(runs(&log), before);
    s.shutdown();
}

#[test]
fn inspect_code_findings_show_in_the_editor_until_fixed_or_closed() {
    let p = project(&[("app.py", APP_PY)]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p);
    s.ws.settings.analysis_on_save = false;
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "0"));
    s.execute("code.inspectCode", &json!({ "scope": "project", "tools": ["ruff"] })).unwrap();
    pump_until(&mut s, "inspection", 30, |s| !s.inspections.busy());
    s.pump();
    assert_eq!(analysis_problems(&s, &app), vec![(0, "F401".to_string())], "shown before the file is even opened");
    // The agent lists show it once, with its id.
    let v = s.execute("analysis.problems", &json!({})).unwrap();
    let analysis: Vec<&Value> = v["problems"].as_array().unwrap().iter().filter(|p| p["source"] == "analysis").collect();
    assert_eq!(analysis.len(), 1, "{v}");
    s.execute("file.open", &json!({ "path": app })).unwrap();
    s.pump();
    let v = s.execute("analysis.fileFindings", &json!({})).unwrap();
    assert_eq!(v["findings"][0]["source"], "inspection");
    assert_eq!(v["findings"][0]["id"], 0);
    // Fixing it in the Inspection Results tool window clears the underline.
    s.execute("inspections.fix", &json!({ "id": 0 })).unwrap();
    s.pump();
    s.pump();
    assert!(!text(&s).contains("import os"), "{}", text(&s));
    assert!(analysis_problems(&s, &app).is_empty());
    // A new run shows it again; closing the results clears it.
    s.execute("file.saveAll", &Value::Null).unwrap();
    std::fs::write(&app, APP_PY).unwrap();
    s.execute("file.reloadAllFromDisk", &Value::Null).ok();
    s.execute("code.inspectCode", &json!({ "scope": "project", "tools": ["ruff"] })).unwrap();
    pump_until(&mut s, "second inspection", 30, |s| !s.inspections.busy());
    s.pump();
    assert_eq!(analysis_problems(&s, &app).len(), 1);
    s.execute("inspections.close", &Value::Null).unwrap();
    s.pump();
    assert!(analysis_problems(&s, &app).is_empty());
    s.shutdown();
}

#[test]
fn clippy_on_save_when_turned_on_without_rust_analyzer() {
    let clippy = Command::new("cargo").args(["clippy", "--version"]).output().is_ok_and(|o| o.status.success());
    if !clippy {
        eprintln!("cargo clippy is not installed; skipping");
        return;
    }
    let p = project(&[
        ("Cargo.toml", "[package]\nname = \"onsave\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
    ]);
    let main = p.root.join("src/main.rs");
    let mut s = session(&p);
    pump_until(&mut s, "cargo metadata", 120, |s| s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some()));
    s.execute("file.open", &json!({ "path": main })).unwrap();
    // Off by default (Settings › Rust › External linter): nothing runs on save.
    type_and_save(&mut s, "fn twice(x: i32) -> i32 {\n    return x * 2;\n}\n");
    s.pump();
    assert!(!s.analysis.editor.on_save.busy());
    s.ws.settings.use_clippy = true;
    type_and_save(&mut s, "\n");
    pump_until(&mut s, "clippy on save", 300, |s| !s.analysis.editor.on_save.busy());
    let last = s.analysis.editor.on_save.history.last().cloned().unwrap();
    assert!(last.tools.iter().any(|t| t.starts_with("clippy:")), "{last:?}");
    let codes: Vec<String> = analysis_problems(&s, &main).into_iter().map(|(_, c)| c).collect();
    assert!(codes.iter().any(|c| c == "clippy::needless_return"), "{codes:?} {last:?}");
    s.shutdown();
}

#[test]
fn clangd_on_save_for_cpp() {
    let Some(clangd) = which("clangd") else {
        eprintln!("clangd is not installed; skipping");
        return;
    };
    let p = project(&[("main.cpp", "int main() {\n    return undefined_name;\n}\n")]);
    let main = p.root.join("main.cpp");
    let mut s = session(&p);
    s.inspections.programs.insert(Tool::Clangd, clangd);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    type_and_save(&mut s, "\n");
    pump_until(&mut s, "clangd on save", 120, |s| !s.analysis.editor.on_save.busy());
    let last = s.analysis.editor.on_save.history.last().cloned().unwrap();
    assert!(last.tools.iter().any(|t| t.starts_with("clangd:")), "{last:?}");
    let v = s.execute("analysis.fileFindings", &json!({ "path": main })).unwrap();
    let found = v["findings"].as_array().unwrap();
    // clang-tidy (when installed) reports the same compile error since M16 (it ran with no
    // checks before, see `DEFAULT_TIDY_CHECKS`); one report is kept.
    assert!(
        found.iter().any(|f| (f["analyzer"] == "clangd" || f["analyzer"] == "clang_tidy")
            && f["line"] == 2
            && f["message"].as_str().unwrap_or("").contains("undefined_name")),
        "{v}"
    );
    s.shutdown();
}

#[test]
fn cargo_deny_fetches_advisories_only_when_the_profile_allows_it() {
    let p = project(&[
        ("Cargo.toml", "[package]\nname = \"deny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("src/lib.rs", "pub fn a() {}\n"),
    ]);
    let log = p.base.join("tools.log");
    let deny = script(&p.base, "fake-cargo-deny", &format!("echo \"deny $*\" >> '{}'\nexit 0", log.display()));
    let audit = script(&p.base, "fake-cargo-audit", &format!("echo \"audit $*\" >> '{}'\necho '{{}}'\nexit 0", log.display()));
    let mut s = session(&p);
    s.ws.settings.analysis_on_save = false;
    s.inspections.programs.insert(Tool::CargoDeny, deny);
    s.inspections.programs.insert(Tool::CargoAudit, audit);
    // Only the advisory tools and Clippy are asked for through the profile: keep the run small.
    let inspect = |s: &mut Session| {
        s.execute("code.inspectCode", &json!({ "scope": "project" })).unwrap();
        pump_until(s, "inspection", 300, |s| !s.inspections.busy());
        std::fs::read_to_string(&log).unwrap_or_default()
    };
    for a in ["clippy", "rust_analyzer", "cargo_check"] {
        s.execute("analysis.profile.setAnalyzer", &json!({ "analyzer": a, "enabled": false })).unwrap();
    }
    // Default: neither tool runs (they would download the RustSec database); the run says why.
    assert_eq!(inspect(&mut s), "");
    let r = s.inspections.results.clone().unwrap();
    let skipped = r.tools.iter().find(|t| matches!(t.tool, Tool::CargoAudit | Tool::CargoDeny)).unwrap();
    assert!(skipped.ok && skipped.outcome.contains("network"), "{skipped:?}");
    // A deny.toml without advisories: cargo deny checks licenses, bans and sources offline.
    write(&p.root, "deny.toml", "[licenses]\nallow = [\"MIT\"]\n");
    let out = inspect(&mut s);
    assert!(out.contains("deny --offline --format json --color never check licenses bans sources"), "{out}");
    assert!(!out.contains("advisories") && !out.contains("audit"), "{out}");
    // The profile turns advisories on (Settings › Editor › Inspections): the full check.
    std::fs::remove_file(&log).unwrap();
    let v = s.execute("analysis.profile.setOption", &json!({ "option": "advisories", "value": true })).unwrap();
    assert_eq!(v["advisories"], true);
    let out = inspect(&mut s);
    assert!(out.contains("deny --format json --color never check") && !out.contains("--offline"), "{out}");
    std::fs::remove_file(p.root.join("deny.toml")).unwrap();
    std::fs::remove_file(&log).unwrap();
    let out = inspect(&mut s);
    assert!(out.contains("audit audit --json"), "without deny.toml: cargo audit: {out}");
    assert!(s.execute("analysis.profile.setOption", &json!({ "option": "advisories", "value": "yes" })).is_err());
    s.shutdown();
}

#[test]
fn python_language_server_settings_and_status() {
    let p = project(&[("app.py", APP_PY)]);
    let mut s = session(&p);
    let v = s.execute("python.languageServer", &Value::Null).unwrap();
    assert_eq!(v["preference"], "auto");
    // A managed basedpyright (Install basedpyright puts it in $CODECRAFT_HOME/tools/bin) is the
    // server CodeCraft would start, unless the project's virtualenv has one.
    script(&p.home.join("tools/bin"), "basedpyright-langserver", "exit 0");
    let v = s.execute("python.languageServer", &Value::Null).unwrap();
    assert_eq!(v["server"]["name"], "basedpyright", "{v}");
    assert_eq!(v["server"]["program"], json!(p.home.join("tools/bin/basedpyright-langserver")));
    script(&p.root.join(".venv/bin"), "pylsp", "exit 0");
    let v = s.execute("python.languageServer", &Value::Null).unwrap();
    assert_eq!(v["server"]["name"], "pylsp", "the project's virtualenv first: {v}");
    s.execute("settings.set", &json!({ "key": "python_language_server", "value": "basedpyright" })).unwrap();
    let v = s.execute("python.languageServer", &Value::Null).unwrap();
    assert_eq!(v["server"]["name"], "basedpyright");
    s.execute("settings.set", &json!({ "key": "python_language_server", "value": "none" })).unwrap();
    assert!(s.execute("python.languageServer", &Value::Null).unwrap()["server"].is_null());
    assert!(s.execute("settings.set", &json!({ "key": "python_language_server", "value": "jedi" })).is_err());
    s.execute("settings.set", &json!({ "key": "python_type_checking", "value": "strict" })).unwrap();
    assert!(s.execute("settings.set", &json!({ "key": "python_type_checking", "value": "loud" })).is_err());
    assert_eq!(s.execute("python.languageServer", &Value::Null).unwrap()["typeChecking"], "strict");
    s.shutdown();
}

#[test]
fn hostile_params_for_the_new_commands() {
    let p = project(&[("app.py", APP_PY)]);
    let mut s = session(&p);
    for (id, params) in [
        ("analysis.fileFindings", json!({ "path": 7 })),
        ("analysis.fileFindings", json!({ "path": "/does/not/exist.py" })),
        ("analysis.editorFix", json!({ "key": u64::MAX, "fix": u64::MAX })),
        ("analysis.editorFix", json!({ "key": -1 })),
        ("analysis.editorSuppress", json!({})),
        ("analysis.analyzeFile", json!({ "path": "/does/not/exist.py" })),
        ("analysis.analyzeFile", json!({ "path": p.root.join("README.md") })),
        ("python.languageServer", json!([1, 2])),
    ] {
        match s.execute(id, &params) {
            Ok(_) => {}
            Err(e) => assert!(!e.to_string().starts_with("internal error"), "{id} {params}: {e}"),
        }
    }
    // A file without findings: an empty list and a hint.
    let v = s.execute("analysis.fileFindings", &json!({ "path": "app.py" })).unwrap();
    assert!(v["findings"].as_array().unwrap().is_empty());
    assert!(v["next"].as_str().unwrap().contains("analysis.analyzeFile"));
    s.shutdown();
}
