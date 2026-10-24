//! Analysis while typing (M16 track LT), end to end: Ruff on the unsaved buffer through stdin
//! (the real ruff when installed; stand-ins for debouncing, cancellation and stale results),
//! clang-tidy on a copy of the buffer with the file's real compile command (a fake that records
//! what it was given; the real clang-tidy when installed), clangd's live diagnostics, the SQL
//! inspections in `.sql` files, analysis on open for every language, the per-language settings,
//! Power Save mode, agents' `analysis.fileFindings` (revision, `pending`, `waitMs`) and the cost
//! of a keystroke on a 10k-line file.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};
use codecraft_engine::inspections::Tool;
use codecraft_engine::{LspStatus, Session};
use codecraft_lsp::ServerKind;
use codecraft_workspace::ProblemSource;

fn pump_until(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; status: {}; live: {:?}", s.status, s.analysis.editor.live);
}

/// Pump for a while (nothing expected to happen).
fn pump_for(s: &mut Session, ms: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
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
    Project { _tmp: tmp, root, base }
}

fn session(p: &Project, delay_ms: u64) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&p.base.join("home"));
    s.analysis.editor.on_save.debounce = Duration::ZERO;
    s.ws.settings.analysis_while_typing_delay_ms = delay_ms;
    s.execute("file.open", &json!({ "path": p.root })).unwrap();
    pump_until(&mut s, "project scan", 60, |s| s.ws.project.as_ref().is_some_and(|p| p.tree.len() > 1) && !s.jobs_running());
    s
}

fn text(s: &Session) -> String {
    s.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

fn revision(s: &Session) -> u64 {
    s.ws.active().map(|(_, d)| d.doc.revision()).unwrap()
}

fn modified(s: &Session) -> bool {
    s.ws.active().is_some_and(|(_, d)| d.doc.is_modified())
}

fn type_at(s: &mut Session, offset: usize, t: &str) {
    s.execute("editor.setSelection", &json!({ "anchor": offset })).unwrap();
    s.execute("editor.type", &json!({ "text": t })).unwrap();
}

fn analysis_problems(s: &Session, path: &Path) -> Vec<(usize, String)> {
    s.ws.problems_for(path)
        .into_iter()
        .filter(|p| p.source == ProblemSource::Analysis)
        .map(|p| (p.start.line, p.code.clone().unwrap_or_default()))
        .collect()
}

/// `analysis.fileFindings` once nothing is pending for the current revision.
fn settled_findings(s: &mut Session, path: &Path, secs: u64) -> Value {
    let mut out = Value::Null;
    pump_until(s, "live analysis of the current revision", secs, |s| {
        let v = s.execute("analysis.fileFindings", &json!({ "path": path })).unwrap();
        let done = v["pending"] == json!(false) && v["live"]["computedFor"] == v["revision"];
        out = v;
        done
    });
    out
}

fn runs(log: &Path) -> usize {
    std::fs::read_to_string(log).map(|t| t.lines().filter(|l| l.starts_with("run")).count()).unwrap_or(0)
}

/// A stand-in for `ruff check --stdin-filename <path> -`: it records each run, saves what it
/// read on stdin to `<log>.stdin`, sleeps `delay` seconds and reports F401 on line 1 (`os`) when
/// the buffer imports `os`.
fn fake_ruff(dir: &Path, file: &Path, log: &Path, delay: &str) -> PathBuf {
    let report = json!([{
        "code": "F401", "filename": file, "message": "`os` imported but unused",
        "location": {"row": 1, "column": 8}, "end_location": {"row": 1, "column": 10},
        "fix": {"applicability": "safe", "message": "Remove unused import: `os`",
                "edits": [{"content": "", "location": {"row": 1, "column": 1}, "end_location": {"row": 2, "column": 1}}]}
    }]);
    let log = log.display();
    script(
        dir,
        "fake-ruff",
        &format!(
            "echo \"run $*\" >> '{log}'\ncat > '{log}.stdin'\nfor a in \"$@\"; do last=\"$a\"; done\n\
             [ -s '{log}.stdin' ] || cp \"$last\" '{log}.stdin'\nsleep {delay}\necho done >> '{log}'\n\
             if grep -q '^import os' '{log}.stdin'; then cat <<'JSON'\n{report}\nJSON\nelse echo '[]'; fi"
        ),
    )
}

// ---------------------------------------------------------------- Python

#[test]
fn ruff_runs_on_the_unsaved_buffer_as_you_type() {
    let Some(ruff) = which("ruff") else {
        eprintln!("ruff is not installed; skipping");
        return;
    };
    let p = project(&[("app.py", "import sys\n\nprint(sys.argv)\n")]);
    let app = p.root.join("app.py");
    let mut s = session(&p, 150);
    s.inspections.programs.insert(Tool::Ruff, ruff);
    s.execute("file.open", &json!({ "path": app })).unwrap();
    // Analyzed when it opens: clean.
    let v = settled_findings(&mut s, &app, 30);
    assert_eq!(v["findings"], json!([]), "{v:#}");
    assert_eq!(v["live"]["analyzers"], json!(["ruff"]), "{v:#}");
    assert!(s.analysis.editor.live.started >= 1);

    // Type an unused import: underlined without saving.
    type_at(&mut s, 0, "import os\n");
    let v = settled_findings(&mut s, &app, 30);
    assert!(modified(&s), "the buffer was never saved");
    assert_eq!(std::fs::read_to_string(&app).unwrap(), "import sys\n\nprint(sys.argv)\n", "the disk is untouched");
    assert_eq!(analysis_problems(&s, &app), vec![(0, "F401".to_string())], "the real ruff read the buffer");
    let f = &v["findings"][0];
    assert_eq!((f["rule"].as_str(), f["source"].as_str(), f["line"].as_u64()), (Some("F401"), Some("live"), Some(1)), "{v:#}");
    assert_eq!(f["revision"], v["revision"], "computed for the current revision");
    assert_eq!(v["revision"].as_u64(), Some(revision(&s)));
    assert_eq!(v["modified"], json!(true));
    let last = &v["live"]["last"];
    assert_eq!(last["cancelled"], json!(false), "{last}");
    assert!(last["tools"][0].as_str().unwrap().starts_with("ruff: 1 problem"), "{last}");

    // Typing above moves the underline at once (mapped through the edit), then it is re-checked.
    type_at(&mut s, 0, "# header\n");
    s.pump();
    assert_eq!(analysis_problems(&s, &app), vec![(1, "F401".to_string())], "moved with the edit");
    settled_findings(&mut s, &app, 30);
    assert_eq!(analysis_problems(&s, &app), vec![(1, "F401".to_string())]);

    // Use `os`: the finding clears without saving.
    let len = s.ws.active().unwrap().1.doc.len_chars();
    type_at(&mut s, len, "print(os.sep)\n");
    settled_findings(&mut s, &app, 30);
    assert!(analysis_problems(&s, &app).is_empty(), "{:?}", analysis_problems(&s, &app));

    // Undo: the import is unused again; Alt+Enter's fix (analysis.editorFix) removes it.
    s.execute("edit.undo", &Value::Null).unwrap();
    let v = settled_findings(&mut s, &app, 30);
    let key = v["findings"][0]["key"].as_u64().unwrap();
    s.execute("analysis.editorFix", &json!({ "key": key })).unwrap();
    assert_eq!(text(&s), "# header\nimport sys\n\nprint(sys.argv)\n");
    settled_findings(&mut s, &app, 30);
    assert!(analysis_problems(&s, &app).is_empty());
    // The agents' problem list has the live findings too (none left).
    s.shutdown();
}

#[test]
fn typing_200_characters_quickly_runs_the_analyzer_at_most_twice() {
    let p = project(&[("app.py", "import sys\n")]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p, 400);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "0"));
    s.execute("file.open", &json!({ "path": app })).unwrap();
    settled_findings(&mut s, &app, 30);
    let on_open = runs(&log);
    assert_eq!(on_open, 1, "analyzed once when it opened");
    let started = s.analysis.editor.live.started;
    // 200 keystrokes, a pump (a frame) after each, faster than the delay.
    let t0 = Instant::now();
    for i in 0..200 {
        let len = s.ws.active().unwrap().1.doc.len_chars();
        type_at(&mut s, len, if i % 40 == 39 { "\n" } else { "x" });
        s.pump();
        std::thread::sleep(Duration::from_micros(500));
    }
    let typing = t0.elapsed();
    let v = settled_findings(&mut s, &app, 30);
    let typed_runs = s.analysis.editor.live.started - started;
    eprintln!("200 keystrokes in {typing:?}: {typed_runs} live run(s), ruff ran {} time(s)", runs(&log) - on_open);
    assert!(typing < Duration::from_millis(400) || typed_runs <= 3, "the machine is slow: {typing:?}");
    if typing < Duration::from_millis(400) {
        assert!(typed_runs <= 2, "debounced: {typed_runs} runs");
    }
    assert!(runs(&log) - on_open <= typed_runs as usize);
    // The last run read the final buffer.
    assert_eq!(std::fs::read_to_string(format!("{}.stdin", log.display())).unwrap(), text(&s));
    assert_eq!(v["live"]["computedFor"].as_u64(), Some(revision(&s)));
    s.shutdown();
}

#[test]
fn a_newer_edit_cancels_the_run_in_flight() {
    let p = project(&[("app.py", "import os\n")]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p, 100);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "3"));
    s.execute("file.open", &json!({ "path": app })).unwrap();
    pump_until(&mut s, "the run on open", 10, |s| s.analysis.editor.live.running(&app));
    pump_until(&mut s, "ruff started", 10, |_| runs(&log) == 1);
    // An edit while it runs: the run is cancelled (its process killed) at once.
    let t0 = Instant::now();
    type_at(&mut s, 0, "# c\n");
    s.pump();
    assert!(!s.analysis.editor.live.running(&app) || s.analysis.editor.live.history.last().is_some_and(|h| h.cancelled));
    let cancelled = s.analysis.editor.live.history.iter().find(|h| h.cancelled).cloned().expect("a cancelled run");
    assert_eq!(cancelled.revision, 0, "{cancelled:?}");
    assert!(t0.elapsed() < Duration::from_secs(2));
    // The newer revision is analyzed after the delay; the killed run never finished.
    let v = settled_findings(&mut s, &app, 30);
    let done = std::fs::read_to_string(&log).unwrap().lines().filter(|l| *l == "done").count();
    assert_eq!(done, 1, "only the second run finished: {}", std::fs::read_to_string(&log).unwrap());
    assert_eq!(v["live"]["computedFor"].as_u64(), Some(revision(&s)));
    assert!(v["live"]["last"]["revision"].as_u64() == Some(revision(&s)), "{v:#}");
    s.shutdown();
}

#[test]
fn a_result_for_an_older_revision_is_mapped_forward() {
    let p = project(&[("app.py", "import os\n")]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p, 100);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "0"));
    s.execute("file.open", &json!({ "path": app })).unwrap();
    // The run on open starts with the next pump; let it finish without taking it in.
    s.pump();
    assert!(s.analysis.editor.live.running(&app));
    let t0 = Instant::now();
    while !std::fs::read_to_string(&log).unwrap_or_default().contains("done") {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    // The process log precedes delivery of the worker result. Await actual readiness
    // without pumping, so slow scheduling cannot turn this into a cancellation test.
    while !s.analysis.editor.live.result_ready(&app) {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    // Insert a line above before the result is read: it is mapped to the newer text.
    type_at(&mut s, 0, "# c\n");
    s.pump();
    let last = s.analysis.editor.live.history.last().cloned().unwrap();
    assert!(!last.cancelled && last.mapped && last.revision == 0, "{last:?}");
    assert_eq!(analysis_problems(&s, &app), vec![(1, "F401".to_string())], "on `os`, one line down");
    let v = s.execute("analysis.fileFindings", &json!({ "path": app })).unwrap();
    assert_eq!(v["findings"][0]["revision"], json!(0), "computed for revision 0, shown in the current text: {v:#}");
    assert_eq!(v["live"]["computedFor"], json!(0));
    assert_eq!(v["pending"], json!(true), "the current revision is not analyzed yet");
    let v = settled_findings(&mut s, &app, 30);
    assert_eq!(v["live"]["computedFor"].as_u64(), Some(revision(&s)));
    s.shutdown();
}

#[test]
fn settings_power_save_and_save_without_live() {
    let p = project(&[("app.py", "import sys\n")]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p, 100);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "0"));
    s.execute("file.open", &json!({ "path": app })).unwrap();
    settled_findings(&mut s, &app, 30);
    assert_eq!(runs(&log), 1);

    // Power Save mode pauses it.
    s.execute("file.powerSaveMode", &Value::Null).unwrap();
    assert!(s.power_save());
    type_at(&mut s, 0, "import os\n");
    pump_for(&mut s, 500);
    assert_eq!(runs(&log), 1, "paused");
    let v = s.execute("analysis.fileFindings", &json!({ "path": app })).unwrap();
    assert_eq!(v["live"]["enabled"], json!(false));
    assert_eq!(v["live"]["powerSave"], json!(true));
    // Leaving it analyzes what was typed meanwhile.
    s.execute("file.powerSaveMode", &Value::Null).unwrap();
    settled_findings(&mut s, &app, 30);
    assert_eq!(runs(&log), 2);
    assert_eq!(analysis_problems(&s, &app), vec![(0, "F401".to_string())]);

    // The setting off: typing analyzes nothing; a save runs Ruff on save instead.
    s.execute("settings.set", &json!({ "key": "analysis_while_typing_python", "value": false })).unwrap();
    s.execute("settings.set", &json!({ "key": "analysis_while_typing_delay_ms", "value": 50 }))
        .expect_err("the delay is 100–10000 ms");
    type_at(&mut s, 0, "# x\n");
    pump_for(&mut s, 400);
    assert_eq!(runs(&log), 2, "not while typing");
    let v = s.execute("analysis.fileFindings", &json!({ "path": app })).unwrap();
    assert_eq!(v["live"]["enabled"], json!(false));
    s.execute("file.saveAll", &Value::Null).unwrap();
    pump_until(&mut s, "ruff on save", 30, |s| !s.analysis.editor.on_save.busy() && runs(&log) == 3);
    pump_until(&mut s, "the on-save finding", 10, |s| {
        s.analysis.editor.of(&app).iter().any(|e| matches!(e.origin, codecraft_engine::analysis::editor::Origin::OnSave { .. }))
    });
    s.shutdown();
}

#[test]
fn agents_wait_for_the_current_revision_with_wait_ms() {
    let p = project(&[("app.py", "import sys\n")]);
    let app = p.root.join("app.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p, 300);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &app, &log, "0.3"));
    s.execute("file.open", &json!({ "path": app })).unwrap();
    settled_findings(&mut s, &app, 30);
    type_at(&mut s, 0, "import os\n");
    // Without waiting: pending, the older revision's (empty) findings.
    let v = s.execute("analysis.fileFindings", &json!({ "path": app, "waitMs": 100 })).unwrap();
    assert_eq!(v["pending"], json!(true), "{v:#}");
    assert!(v["note"].as_str().unwrap().contains("codecraft_execute"), "{v:#}");
    // Over MCP: codecraft_execute waits (the IDE keeps pumping) for the current revision.
    let r = agent_tools::call_blocking(
        &mut s,
        Caller::new("test agent"),
        "codecraft_execute",
        json!({ "id": "analysis.fileFindings", "params": { "path": "app.py", "waitMs": 30_000 } }),
        Duration::from_secs(60),
    );
    assert!(!r.is_error, "{}", result_text(&r));
    let v = result_json(&r)["result"].clone();
    assert_eq!(v["pending"], json!(false), "{v:#}");
    assert_eq!(v["revision"].as_u64(), Some(revision(&s)));
    assert_eq!(v["live"]["computedFor"], v["revision"]);
    assert_eq!(v["findings"][0]["rule"], json!("F401"), "{v:#}");
    assert!(v.get("timedOut").is_none());
    // A wait that runs out answers with the state so far.
    type_at(&mut s, 0, "# again\n");
    let r = agent_tools::call_blocking(
        &mut s,
        Caller::new("test agent"),
        "codecraft_execute",
        json!({ "id": "analysis.fileFindings", "params": { "path": "app.py", "waitMs": 50 } }),
        Duration::from_secs(60),
    );
    let v = result_json(&r)["result"].clone();
    assert_eq!(v["timedOut"], json!(true), "{v:#}");
    assert_eq!(v["pending"], json!(true));
    // Bad params are errors, not panics.
    assert!(s.execute("analysis.fileFindings", &json!({ "path": app, "waitMs": "soon" })).is_err());
    s.shutdown();
}

#[test]
fn keystrokes_on_a_10k_line_file_stay_cheap() {
    let big: String = (0..10_000).map(|i| format!("value_{i} = {i}  # a comment to make the line longer\n")).collect();
    let p = project(&[("big.py", &big)]);
    let big_py = p.root.join("big.py");
    let log = p.base.join("ruff.log");
    let mut s = session(&p, 500);
    s.inspections.programs.insert(Tool::Ruff, fake_ruff(&p.base, &big_py, &log, "0"));
    s.execute("file.open", &json!({ "path": big_py })).unwrap();
    settled_findings(&mut s, &big_py, 60);
    let measure = |s: &mut Session, live: bool| {
        s.ws.settings.analysis_while_typing_python = live;
        s.pump();
        let mut total = Duration::ZERO;
        for _ in 0..200 {
            type_at(s, 5000, "x");
            let t = Instant::now();
            s.pump();
            total += t.elapsed();
        }
        total / 200
    };
    let off = measure(&mut s, false);
    let on = measure(&mut s, true);
    eprintln!("pump after a keystroke on a 10k-line file: {on:?} with analysis while typing, {off:?} without");
    // The live part is a revision check per open document: within noise of the rest.
    assert!(on <= off * 2 + Duration::from_millis(2), "{on:?} vs {off:?}");
    let started = s.analysis.editor.live.started;
    settled_findings(&mut s, &big_py, 60);
    assert!(s.analysis.editor.live.started - started <= 1, "one run after the burst");
    s.shutdown();
}

// ---------------------------------------------------------------- C/C++

const MAIN_CPP: &str = "#include \"util.h\"\nint main() {\n  return twice(2);\n}\n";

fn cpp_project() -> Project {
    let p = project(&[("src/util.h", "int twice(int x);\n"), ("src/main.cpp", MAIN_CPP)]);
    let db = json!([{
        "directory": p.root.join("build"),
        "file": p.root.join("src/main.cpp"),
        "arguments": ["clang++", "-std=c++17", "-DFOO=1", "-c", "../src/main.cpp", "-o", "main.o"],
    }]);
    write(&p.root, "build/compile_commands.json", &db.to_string());
    p
}

#[test]
fn clang_tidy_runs_on_a_copy_of_the_buffer_with_the_real_compile_command() {
    let p = cpp_project();
    let main = p.root.join("src/main.cpp");
    let log = p.base.join("tidy.log");
    // A stand-in: records its arguments, the compilation database it was given and the file it
    // analyzed, then reports a warning on line 3 of that file.
    let fake = script(
        &p.base,
        "fake-clang-tidy",
        &format!(
            "echo \"run $*\" >> '{log}'\nprev=''\nfor a in \"$@\"; do\n  case \"$a\" in *.cpp) f=\"$a\";; esac\n  \
             if [ \"$prev\" = '-p' ]; then cp \"$a/compile_commands.json\" '{log}.db'; fi\n  prev=\"$a\"\ndone\n\
             cp \"$f\" '{log}.seen'\necho \"$f:3:10: warning: fake finding here [bugprone-fake]\"\nexit 0",
            log = log.display()
        ),
    );
    let mut s = session(&p, 100);
    s.inspections.programs.insert(Tool::ClangTidy, fake);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    // Analyzed when it opens.
    let v = settled_findings(&mut s, &main, 30);
    assert_eq!(v["live"]["analyzers"], json!(["clang_tidy"]), "{v:#}");
    assert_eq!(analysis_problems(&s, &main), vec![(2, "bugprone-fake".to_string())], "{v:#}");
    // Edit without saving: the copy holds the buffer.
    type_at(&mut s, MAIN_CPP.len() - 2, "  // edited\n");
    let v = settled_findings(&mut s, &main, 30);
    assert!(modified(&s));
    assert_eq!(std::fs::read_to_string(format!("{}.seen", log.display())).unwrap(), text(&s), "clang-tidy read the buffer");
    let f = &v["findings"][0];
    assert_eq!((f["analyzer"].as_str(), f["source"].as_str(), f["line"].as_u64()), (Some("clang_tidy"), Some("live"), Some(3)));
    assert_eq!(v["path"], json!(main), "on the real file, not the copy");
    // The real compile command, pointing at the copy, with the file's directory included.
    let db: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}.db", log.display())).unwrap()).unwrap();
    let entry = &db[0];
    assert_eq!(entry["directory"], json!(p.root.join("build")));
    let copy = entry["file"].as_str().unwrap().to_string();
    assert_ne!(Path::new(&copy), main);
    assert!(copy.ends_with("main.cpp"));
    let args: Vec<&str> = entry["arguments"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    assert_eq!(args[0], "clang++");
    assert!(args.contains(&"-DFOO=1") && args.contains(&"-std=c++17"), "{args:?}");
    assert!(args.contains(&format!("-I{}", p.root.join("src").display()).as_str()), "{args:?}");
    assert!(args.contains(&copy.as_str()) && !args.contains(&"../src/main.cpp"), "{args:?}");
    // The temporary copy is gone afterwards.
    assert!(!Path::new(&copy).exists());
    s.shutdown();
}

fn clang_tidy() -> Option<PathBuf> {
    which("clang-tidy").or_else(|| {
        ["/opt/homebrew/opt/llvm/bin/clang-tidy", "/usr/local/opt/llvm/bin/clang-tidy"].iter().map(PathBuf::from).find(|p| p.is_file())
    })
}

#[test]
fn the_real_clang_tidy_sees_unsaved_edits() {
    let Some(tidy) = clang_tidy() else {
        eprintln!("clang-tidy is not installed; skipping");
        return;
    };
    let p = cpp_project();
    let main = p.root.join("src/main.cpp");
    let mut s = session(&p, 100);
    s.inspections.programs.insert(Tool::ClangTidy, tidy);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    let v = settled_findings(&mut s, &main, 60);
    let last = v["live"]["last"]["tools"][0].as_str().unwrap_or_default().to_string();
    assert!(last.starts_with("clang_tidy: "), "{v:#}");
    assert!(!last.contains("failed"), "clang-tidy ran with the copied compile command: {v:#}");
    assert!(v["findings"].as_array().unwrap().is_empty(), "clean on open (util.h found through the copied command): {v:#}");
    // An undeclared name, typed and not saved: a compile error from clang-tidy.
    type_at(&mut s, MAIN_CPP.find("  return").unwrap(), "  int y = undeclared_thing;\n");
    let v = settled_findings(&mut s, &main, 60);
    let rules: Vec<(u64, String)> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["line"].as_u64().unwrap(), f["rule"].as_str().unwrap().to_string()))
        .collect();
    assert!(rules.contains(&(3, "clang-diagnostic-error".to_string())), "{rules:?}");
    assert!(modified(&s));
    s.shutdown();
}

#[test]
fn clangd_reports_live_through_the_language_server() {
    let p = project(&[("main.cpp", "int main() {\n  return 0;\n}\n")]);
    let main = p.root.join("main.cpp");
    if codecraft_lsp::find_server(ServerKind::Clangd, &p.root).is_none() {
        eprintln!("clangd is not installed; skipping");
        return;
    }
    let mut s = session(&p, 100);
    s.lsp_enabled = true;
    s.ws.settings.analysis_while_typing_cpp = false;
    s.execute("file.open", &json!({ "path": main })).unwrap();
    pump_until(&mut s, "clangd ready", 60, |s| s.server_status(ServerKind::Clangd) == LspStatus::Ready);
    type_at(&mut s, "int main() {\n".len(), "  int y = undeclared_thing;\n");
    pump_until(&mut s, "clangd's diagnostic for the unsaved edit", 60, |s| {
        s.ws.problems_for(&main).iter().any(|p| p.source != ProblemSource::Analysis && p.start.line == 1)
    });
    assert!(modified(&s), "never saved");
    s.shutdown();
}

// ---------------------------------------------------------------- SQL, Rust, analysis on open

fn sql_problems(s: &Session, path: &Path) -> Vec<String> {
    s.ws.problems_for(path).into_iter().filter(|p| p.source == ProblemSource::Inspection).filter_map(|p| p.code.clone()).collect()
}

#[test]
fn sql_inspections_run_as_you_type_in_sql_files() {
    let p = project(&[("q.sql", "SELECT 1;\n")]);
    let q = p.root.join("q.sql");
    let mut s = session(&p, 100);
    s.execute("file.open", &json!({ "path": q })).unwrap();
    let v = settled_findings(&mut s, &q, 30);
    assert_eq!(v["live"]["language"], json!("sql"), "{v:#}");
    assert!(sql_problems(&s, &q).is_empty());
    let len = s.ws.active().unwrap().1.doc.len_chars();
    type_at(&mut s, len, "DELETE FROM users;\n");
    settled_findings(&mut s, &q, 30);
    assert!(sql_problems(&s, &q).contains(&"SqlWithoutWhere".to_string()), "{:?}", sql_problems(&s, &q));
    assert!(modified(&s));
    // Off: an edited file waits for a save.
    s.ws.settings.analysis_while_typing_sql = false;
    let without_where = |s: &Session| sql_problems(s, &q).iter().filter(|c| *c == "SqlWithoutWhere").count();
    let len = s.ws.active().unwrap().1.doc.len_chars();
    type_at(&mut s, len, "DELETE FROM orders;\n");
    pump_for(&mut s, 400);
    assert_eq!(without_where(&s), 1, "not re-checked while typing");
    let v = s.execute("analysis.fileFindings", &json!({ "path": q })).unwrap();
    assert_eq!(v["live"]["enabled"], json!(false));
    assert_eq!(v["pending"], json!(false), "nothing runs until a save: {v:#}");
    s.execute("file.saveAll", &Value::Null).unwrap();
    pump_until(&mut s, "the check after the save", 10, |s| without_where(s) == 2);
    s.shutdown();
}

#[test]
fn rust_files_are_analyzed_on_open_with_clippy_when_it_runs_on_save() {
    let Some(_cargo) = which("cargo") else {
        eprintln!("cargo is not installed; skipping");
        return;
    };
    let p = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("src/main.rs", "fn main() {}\n"),
    ]);
    let main = p.root.join("src/main.rs");
    let log = p.base.join("clippy.log");
    let fake = script(&p.base, "fake-cargo", &format!("echo \"run $*\" >> '{}'\nexit 0", log.display()));
    let mut s = session(&p, 100);
    pump_until(&mut s, "cargo metadata", 60, |s| s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some()));
    s.ws.settings.use_clippy = true;
    s.inspections.programs.insert(Tool::Clippy, fake);
    s.execute("file.open", &json!({ "path": main })).unwrap();
    pump_until(&mut s, "clippy on open", 30, |s| !s.analysis.editor.on_save.busy() && runs(&log) >= 1);
    assert!(std::fs::read_to_string(&log).unwrap().contains("clippy"), "{}", std::fs::read_to_string(&log).unwrap());
    // Nothing runs while typing for Rust.
    let before = runs(&log);
    type_at(&mut s, 0, "// x\n");
    pump_for(&mut s, 400);
    assert_eq!(runs(&log), before);
    assert_eq!(s.analysis.editor.live.started, 0);
    s.shutdown();
}

#[test]
fn sql_check_cost_on_a_10k_line_file() {
    let big: String = (0..10_000).map(|i| format!("SELECT a, b FROM t{i} WHERE x = {i};\n")).collect();
    let p = project(&[("big.sql", &big)]);
    let q = p.root.join("big.sql");
    let mut s = session(&p, 100);
    s.execute("file.open", &json!({ "path": q })).unwrap();
    settled_findings(&mut s, &q, 60);
    // Keystrokes before the delay cost a revision check; the check itself runs once after it.
    let mut keystroke = Duration::ZERO;
    for _ in 0..50 {
        type_at(&mut s, 0, "x");
        let t = Instant::now();
        s.pump();
        keystroke += t.elapsed();
    }
    std::thread::sleep(Duration::from_millis(150));
    // The settled check runs on a worker thread: the pump that starts it stays cheap.
    let t = Instant::now();
    s.pump();
    let start = t.elapsed();
    let t = Instant::now();
    let v = settled_findings(&mut s, &q, 60);
    eprintln!(
        "10k-line SQL file: {:?} per keystroke; the pump starting the check {start:?}; the check done in {:?}",
        keystroke / 50,
        t.elapsed()
    );
    assert!(keystroke / 50 < Duration::from_millis(5), "{:?}", keystroke / 50);
    assert!(start < Duration::from_millis(15), "{start:?}");
    assert_eq!(v["live"]["computedFor"].as_u64(), Some(revision(&s)));
    s.shutdown();
}
