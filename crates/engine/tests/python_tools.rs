//! Python formatting with fake Ruff/Black/isort scripts (Reformat Code, Optimize Imports, on
//! save, the formatter setting, failures), and Django/Jinja template navigation and completion
//! (M12 track PY). Never runs real formatters; never touches `~/.codecraft`.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::python_env::Probe;
use codecraft_engine::{Popup, Session};

fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait<F: Fn(&Session) -> bool>(s: &mut Session, what: &str, done: F) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; status: {}", s.status);
}

/// A project with `bin/` holding fake tools, an interpreter that has no modules, and `a.py`.
fn setup(text: &str) -> (tempfile::TempDir, PathBuf, PathBuf, Session) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let file = root.join("a.py");
    std::fs::write(&file, text).unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    s.python.probe = Some(Probe::isolated(vec![bin.clone()]));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait(&mut s, "project", |s| !s.jobs_running() && s.python.root.is_some());
    // `python -m <tool>` must not find the real tools.
    let py = tmp.path().join("nopython/python3");
    script(&py, "echo \"No module named $2\" >&2; exit 1");
    s.python.interpreter = Some(py);
    s.execute("file.open", &json!({ "path": file })).unwrap();
    (tmp, bin, file, s)
}

fn text(s: &Session) -> String {
    s.ws.active().unwrap().1.doc.text()
}

#[test]
fn reformat_code_runs_ruff_on_the_text_and_keeps_the_caret() {
    let (_tmp, bin, file, mut s) = setup("x=1\ny =  foo( 2 )\n");
    let log = bin.join("log");
    // A fake `ruff format`: normalizes spaces around `=` and inside parentheses.
    script(
        &bin.join("ruff"),
        &format!("echo \"$@\" >> '{}'\nsed -e 's/ *= */ = /' -e 's/( */(/' -e 's/ *)/)/'", log.display()),
    );
    s.execute("editor.setSelection", &json!({ "anchor": 9, "head": 9 })).unwrap(); // before `foo`
    s.execute("code.reformatCode", &Value::Null).unwrap();
    wait(&mut s, "ruff", |s| !s.py_format.busy());
    assert_eq!(text(&s), "x = 1\ny = foo(2)\n");
    let caret = s.ws.active().unwrap().0.selections.primary().head;
    assert_eq!(text(&s).chars().skip(caret).take(3).collect::<String>(), "foo");
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains(&format!("format --quiet --stdin-filename {} -", file.display())), "{args}");
    assert!(s.status.contains("Ruff"), "{}", s.status);
    // One undo step.
    s.execute("edit.undo", &Value::Null).unwrap();
    assert_eq!(text(&s), "x=1\ny =  foo( 2 )\n");
    // Logged for Python Process Output.
    assert!(s.python.process_output.text().contains("[Ruff]"), "{}", s.python.process_output.text());
    let tools = s.execute("python.format.tools", &Value::Null).unwrap();
    assert_eq!(tools["format"][0]["label"], "Ruff");
    s.shutdown();
}

#[test]
fn the_formatter_setting_picks_black_and_none_disables() {
    let (_tmp, bin, _file, mut s) = setup("a  =  1\n");
    script(&bin.join("ruff"), "echo ruff-ran");
    script(&bin.join("black"), "sed 's/  */ /g'");
    s.execute("settings.set", &json!({ "key": "python_formatter", "value": "black" })).unwrap();
    s.execute("code.reformatFile", &Value::Null).unwrap();
    wait(&mut s, "black", |s| !s.py_format.busy());
    assert_eq!(text(&s), "a = 1\n");
    assert!(s.status.contains("Black"), "{}", s.status);
    s.execute("settings.set", &json!({ "key": "python_formatter", "value": "none" })).unwrap();
    assert!(s.execute("code.reformatCode", &Value::Null).is_err());
    s.shutdown();
}

#[test]
fn failures_and_missing_tools_are_reported() {
    let (_tmp, bin, _file, mut s) = setup("def f(:\n");
    // Nothing installed (the interpreter has no modules either).
    s.execute("code.reformatCode", &Value::Null).unwrap();
    wait(&mut s, "no tool", |s| !s.py_format.busy());
    assert!(s.status.contains("not installed"), "{}", s.status);
    // A syntax error from the tool leaves the text alone.
    script(&bin.join("ruff"), "echo 'error: Failed to parse a.py:1:7: Expected a parameter' >&2; exit 2");
    s.execute("code.reformatCode", &Value::Null).unwrap();
    wait(&mut s, "ruff error", |s| !s.py_format.busy());
    assert_eq!(text(&s), "def f(:\n");
    assert!(s.status.contains("Failed to parse"), "{}", s.status);
    // The file changing while the tool runs: the result is dropped.
    script(&bin.join("ruff"), "sleep 0.3; echo 'formatted'");
    s.execute("code.reformatCode", &Value::Null).unwrap();
    s.execute("editor.type", &json!({ "text": "z" })).unwrap();
    wait(&mut s, "stale", |s| !s.py_format.busy());
    assert!(text(&s).starts_with('z') || text(&s).contains('z'));
    assert!(s.status.contains("changed"), "{}", s.status);
    // Non-Python files keep the IDE's own behaviour.
    let txt = s.project_root().unwrap().join("notes.txt");
    std::fs::write(&txt, "x").unwrap();
    s.execute("file.open", &json!({ "path": txt })).unwrap();
    s.execute("code.reformatCode", &Value::Null).unwrap();
    assert!(s.status.contains("No formatter"), "{}", s.status);
    s.shutdown();
}

#[test]
fn optimize_imports_and_on_save() {
    let (_tmp, bin, file, mut s) = setup("import sys\nimport os\nx=1\n");
    let log = bin.join("log");
    // `ruff check --select I --fix` sorts the import lines; `ruff format` fixes `x=1`.
    script(
        &bin.join("ruff"),
        &format!(
            "echo \"$1\" >> '{}'\nif [ \"$1\" = check ]; then input=$(cat); printf '%s\\n' \"$input\" | grep '^import' | sort; printf '%s\\n' \"$input\" | grep -v '^import'; else sed 's/x=1/x = 1/'; fi",
            log.display()
        ),
    );
    s.execute("code.optimizeImports", &Value::Null).unwrap();
    wait(&mut s, "imports", |s| !s.py_format.busy());
    assert_eq!(text(&s), "import os\nimport sys\nx=1\n");

    // On save: imports then format, and the formatted file is saved.
    s.execute("settings.set", &json!({ "key": "python_reformat_on_save", "value": true })).unwrap();
    s.execute("settings.set", &json!({ "key": "python_optimize_imports_on_save", "value": true })).unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
    s.execute("editor.type", &json!({ "text": "import re\n" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    wait(&mut s, "on save", |s| !s.py_format.busy());
    let want = "import os\nimport re\nimport sys\nx = 1\n";
    assert_eq!(text(&s), want);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), want, "saved again after formatting");
    assert!(!s.ws.active().unwrap().1.doc.is_modified());
    // The save after formatting does not format again (no loop).
    std::thread::sleep(Duration::from_millis(50));
    s.pump();
    assert!(!s.py_format.busy());
    let runs = std::fs::read_to_string(&log).unwrap();
    assert_eq!(runs.lines().filter(|l| *l == "format").count(), 1, "{runs}");
    s.shutdown();
}

#[test]
fn templates_navigate_and_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("site");
    let app = root.join("blog/templates/blog");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(root.join("templates")).unwrap();
    std::fs::write(root.join("templates/base.html"), "<html>{% block content %}{% endblock %}</html>\n").unwrap();
    std::fs::write(app.join("_item.html"), "<li>{{ item.title|upper }}</li>\n").unwrap();
    let page = app.join("list.html");
    std::fs::write(&page, "{% extends \"base.html\" %}\n{% block content %}\n{% for item in items %}{% include \"blog/_item.html\" %}{% \n").unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait(&mut s, "project", |s| !s.jobs_running());
    s.execute("file.open", &json!({ "path": page })).unwrap();
    assert_eq!(s.ws.active().unwrap().1.language, codecraft_engine::syntax::Language::DjangoTemplate);

    // Go to Declaration on the extends string.
    s.execute("editor.setSelection", &json!({ "anchor": 14, "head": 14 })).unwrap();
    s.execute("navigate.declarationOrUsages", &Value::Null).unwrap();
    assert!(s.ws.active().unwrap().1.path.as_deref().unwrap().ends_with("templates/base.html"));
    s.execute("navigate.back", &Value::Null).ok();
    s.execute("file.open", &json!({ "path": page })).unwrap();
    let text = s.ws.active().unwrap().1.doc.text();
    let inc = text.find("blog/_item").unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": inc + 2, "head": inc + 2 })).unwrap();
    s.execute("navigate.declarationOrUsages", &Value::Null).unwrap();
    assert!(s.ws.active().unwrap().1.path.as_deref().unwrap().ends_with("blog/_item.html"));
    // By name, and a missing template.
    s.execute("django.goToTemplate", &json!({ "name": "blog/list.html" })).unwrap();
    assert_eq!(s.ws.active().unwrap().1.path.as_deref(), Some(page.as_path()));
    assert!(s.execute("django.goToTemplate", &json!({ "name": "nope.html" })).is_err());

    // Completion after `{% ` suggests the innermost end tag first.
    let end = s.ws.active().unwrap().1.doc.len_chars() - 1;
    s.execute("editor.setSelection", &json!({ "anchor": end, "head": end })).unwrap();
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no completion popup: {}", s.status) };
    assert_eq!(c.items[0].label, "endfor");
    assert!(c.items.iter().any(|i| i.label == "block"));
    s.shutdown();
}
