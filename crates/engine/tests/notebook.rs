//! Jupyter notebooks and View as DataFrame (M12 track PY): editing through commands with the
//! document kept in sync and saved with unknown metadata and cell ids intact, the built-in
//! kernel against the real `python3` (skipped without it; pandas/numpy parts skip without them),
//! DataFrame pages from the kernel and the Python Console, broken notebooks, and every command
//! with hostile params. Never touches `~/.codecraft`; never installs anything.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{CommandError, Session, registry};

const NOTEBOOK: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "intro001",
   "metadata": {
    "tags": [
     "keep"
    ]
   },
   "source": [
    "# Demo"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "code0001",
   "metadata": {
    "custom": {
     "answer": 42
    }
   },
   "outputs": [],
   "source": [
    "x = 20\n",
    "print('hello')\n",
    "x + 1"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  },
  "my_tool": {
   "setting": true
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

fn wait<F: Fn(&mut Session) -> bool>(s: &mut Session, what: &str, done: F) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(40) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; status: {}", s.status);
}

fn setup() -> (tempfile::TempDir, PathBuf, Session) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let nb = root.join("demo.ipynb");
    std::fs::write(&nb, NOTEBOOK).unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&tmp.path().join("home"));
    s.python.probe = Some(codecraft_engine::python_env::Probe::isolated(Vec::new()));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait(&mut s, "project", |s| !s.jobs_running());
    s.execute("file.open", &json!({ "path": nb })).unwrap();
    s.pump();
    (tmp, nb, s)
}

fn state(s: &mut Session) -> Value {
    s.execute("notebook.state", &Value::Null).unwrap()
}

fn python3() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("python3")).find(|p| p.is_file())
}

fn idle(s: &mut Session) -> bool {
    let st = state(s);
    st["kernel"]["status"] == "idle" && st["queued"] == 0
}

#[test]
fn editing_syncs_the_document_and_saves_with_metadata_intact() {
    let (_tmp, path, mut s) = setup();
    let doc = s.ws.active().unwrap().1.id;
    assert!(codecraft_engine::jupyter::shows_notebook(&s, doc));
    let st = state(&mut s);
    assert_eq!(st["cells"].as_array().unwrap().len(), 2);
    assert_eq!(st["cells"][1]["source"], "x = 20\nprint('hello')\nx + 1");
    assert_eq!(st["cells"][0]["type"], "markdown");

    s.execute("notebook.selectCell", &json!({ "index": 1 })).unwrap();
    s.execute("notebook.addCellBelow", &json!({ "source": "y = x * 2" })).unwrap();
    s.execute("notebook.addCellAbove", &json!({ "index": 0, "type": "markdown", "source": "Top" })).unwrap();
    s.execute("notebook.setCellSource", &json!({ "index": 1, "source": "# Demo title" })).unwrap();
    s.execute("notebook.moveCellDown", &json!({ "index": 0 })).unwrap();
    s.execute("notebook.setCellType", &json!({ "index": 3, "type": "raw" })).unwrap();
    let st = state(&mut s);
    let kinds: Vec<_> = st["cells"].as_array().unwrap().iter().map(|c| c["type"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["markdown", "markdown", "code", "raw"]);
    assert_eq!(st["cells"][0]["source"], "# Demo title");
    assert_eq!(st["cells"][1]["source"], "Top");
    assert_eq!(st["selected"], 1);
    s.execute("notebook.selectCell", &json!({ "index": 3 })).unwrap();
    s.execute("notebook.deleteCell", &Value::Null).unwrap();
    assert_eq!(state(&mut s)["selected"], 2);

    // The document follows once typing pauses, and is saved as nbformat JSON.
    wait(&mut s, "sync", |s| s.ws.doc(doc).unwrap().doc.is_modified() && !state(s)["dirty"].as_bool().unwrap());
    s.execute("file.saveAll", &Value::Null).unwrap();
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["metadata"]["my_tool"]["setting"], true);
    assert_eq!(saved["cells"][0]["id"], "intro001");
    assert_eq!(saved["cells"][0]["metadata"]["tags"], json!(["keep"]));
    assert_eq!(saved["cells"][0]["source"], json!(["# Demo title"]));
    assert_eq!(saved["cells"][2]["metadata"]["custom"]["answer"], 42);
    assert_eq!(saved["cells"][2]["id"], "code0001");
    assert_eq!(saved["cells"][1]["id"].as_str().unwrap().len(), 8, "new cells get ids");

    s.execute("notebook.setCellSource", &json!({ "index": 0, "source": "changed" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    assert!(std::fs::read_to_string(&path).unwrap().contains("changed"), "saving flushes pending edits");
    // An external change reloads (the document is unmodified).
    std::fs::write(&path, NOTEBOOK).unwrap();
    s.execute("file.reloadAllFromDisk", &Value::Null).ok();
    s.execute("file.synchronize", &Value::Null).ok();
    wait(&mut s, "reload", |s| state(s)["cells"].as_array().unwrap().len() == 2);

    // Bad cell indices and types are errors, not panics.
    for (id, p) in [
        ("notebook.deleteCell", json!({ "index": 99 })),
        ("notebook.setCellType", json!({ "type": "nope" })),
        ("notebook.setCellType", json!({})),
        ("notebook.setCellSource", json!({ "source": 5 })),
        ("notebook.selectCell", json!({ "index": 99 })),
        ("notebook.sendInput", json!({ "text": "x" })),
    ] {
        assert!(matches!(s.execute(id, &p), Err(CommandError::BadParams(_) | CommandError::Failed(_))), "{id} {p}");
    }
    s.shutdown();
}

#[test]
fn new_notebooks_and_broken_ones() {
    let (_tmp, path, mut s) = setup();
    let dir = path.parent().unwrap();
    let r = s.execute("notebook.new", &json!({ "path": "sub/new" })).unwrap();
    let new = PathBuf::from(r["path"].as_str().unwrap());
    assert!(new.ends_with("sub/new.ipynb"));
    assert_eq!(state(&mut s)["cells"].as_array().unwrap().len(), 1);
    assert!(s.execute("notebook.new", &json!({ "path": new })).is_err(), "never overwrites");

    let broken = dir.join("broken.ipynb");
    std::fs::write(&broken, "{ not json").unwrap();
    s.execute("file.open", &json!({ "path": broken })).unwrap();
    s.pump();
    let doc = s.ws.active().unwrap().1.id;
    assert!(!codecraft_engine::jupyter::shows_notebook(&s, doc), "a broken notebook shows as text");
    let st = state(&mut s);
    assert!(st["error"].as_str().unwrap().contains("JSON"), "{st}");
    assert!(matches!(s.execute("notebook.addCellBelow", &Value::Null), Err(CommandError::Failed(_))));
    // Fixing the text in the editor makes it a notebook again.
    s.execute("edit.selectAll", &Value::Null).unwrap();
    s.execute("editor.type", &json!({ "text": "{\"cells\": [], \"metadata\": {}, \"nbformat\": 4, \"nbformat_minor\": 5}" })).unwrap();
    s.pump();
    assert!(codecraft_engine::jupyter::shows_notebook(&s, doc));
    // A path that is not a notebook.
    assert!(matches!(s.execute("notebook.state", &json!({ "path": dir.join("x.py") })), Err(CommandError::BadParams(_))));
    s.shutdown();
}

#[test]
fn kernel_runs_cells_with_outputs_counts_and_variables() {
    let Some(_) = python3() else {
        eprintln!("python3 not installed; skipping");
        return;
    };
    let (_tmp, path, mut s) = setup();
    s.execute("notebook.addCellBelow", &json!({ "index": 1, "source": "import sys\nprint('oops', file=sys.stderr)\nrows = [{'a': 3, 'b': 'x'}, {'a': 1, 'b': 'y'}, {'a': 2, 'b': None}]\nrows" })).unwrap();
    s.execute("notebook.addCellBelow", &json!({ "index": 2, "source": "raise ValueError('boom')" })).unwrap();
    s.execute("notebook.addCellBelow", &json!({ "index": 3, "source": "never = 1" })).unwrap();
    s.execute("notebook.runAll", &Value::Null).unwrap();
    wait(&mut s, "run all", idle);
    let st = state(&mut s);
    let cells = st["cells"].as_array().unwrap();
    assert_eq!(cells[1]["executionCount"], 1);
    let outs = cells[1]["outputs"].as_array().unwrap();
    assert_eq!(outs[0], json!({ "type": "stdout", "text": "hello\n" }));
    assert_eq!(outs[1]["type"], "result");
    assert_eq!(outs[1]["text"], "21");
    assert_eq!(cells[2]["executionCount"], 2);
    assert!(cells[2]["outputs"].as_array().unwrap().iter().any(|o| o["type"] == "stderr" && o["text"] == "oops\n"));
    assert_eq!(cells[3]["outputs"][0]["type"], "error");
    assert_eq!(cells[3]["outputs"][0]["ename"], "ValueError");
    assert_eq!(cells[4]["executionCount"], Value::Null, "an error stops the queue");
    let vars = st["vars"].as_array().unwrap();
    let rows = vars.iter().find(|v| v["name"] == "rows").unwrap();
    assert_eq!(rows["viewable"], true);
    assert_eq!(rows["shape"], "(3, 2)");

    // View as DataFrame over the kernel: sort, filter, paging.
    s.execute("python.dataframe.view", &json!({ "expression": "rows" })).unwrap();
    wait(&mut s, "frame", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    let v = s.jupyter.frames.active().unwrap().clone();
    assert_eq!(v.error, None);
    assert_eq!(v.columns, ["a", "b"]);
    assert_eq!(v.total, 3);
    s.execute("python.dataframe.sort", &json!({ "column": 0 })).unwrap();
    wait(&mut s, "sorted", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!(v.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), [json!(1), json!(2), json!(3)]);
    assert_eq!(v.labels, ["1", "2", "0"]);
    s.execute("python.dataframe.sort", &json!({ "column": 1, "descending": true })).unwrap();
    wait(&mut s, "sorted desc", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!(v.rows.iter().map(|r| r[1].clone()).collect::<Vec<_>>(), [json!("y"), json!("x"), Value::Null]);
    s.execute("python.dataframe.filter", &json!({ "text": "Y" })).unwrap();
    wait(&mut s, "filtered", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    assert_eq!(s.jupyter.frames.active().unwrap().total, 1);
    s.execute("python.dataframe.filter", &json!({ "text": "" })).unwrap();
    s.execute("python.dataframe.page", &json!({ "pageSize": 1, "page": 2 })).unwrap();
    wait(&mut s, "page", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!((v.start, v.rows.len(), v.page_count()), (2, 1, 3));
    s.execute("python.dataframe.previousPage", &Value::Null).unwrap();
    wait(&mut s, "prev", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    assert_eq!(s.jupyter.frames.active().unwrap().start, 1);
    // Not a table: an error in the viewer, not a crash.
    s.execute("python.dataframe.view", &json!({ "expression": "x" })).unwrap();
    wait(&mut s, "not a table", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    assert!(s.jupyter.frames.active().unwrap().error.as_deref().unwrap().contains("not a DataFrame"));
    let st = s.execute("python.dataframe.state", &Value::Null).unwrap();
    assert_eq!(st["views"].as_array().unwrap().len(), 2);

    // input(), interrupt, restart.
    s.execute("notebook.setCellSource", &json!({ "index": 4, "source": "name = input('who? ')\nprint('hi', name)" })).unwrap();
    s.execute("notebook.runCell", &json!({ "index": 4 })).unwrap();
    wait(&mut s, "input", |s| !state(s)["awaitingInput"].is_null());
    s.execute("notebook.sendInput", &json!({ "text": "ada" })).unwrap();
    wait(&mut s, "input answered", idle);
    let out = state(&mut s)["cells"][4]["outputs"][0]["text"].as_str().unwrap().to_string();
    assert!(out.contains("who? ada\n") && out.contains("hi ada"), "{out}");
    #[cfg(unix)]
    {
        s.execute("notebook.setCellSource", &json!({ "index": 4, "source": "import time\nwhile True:\n    time.sleep(0.01)" })).unwrap();
        s.execute("notebook.runCell", &json!({ "index": 4 })).unwrap();
        wait(&mut s, "running", |s| state(s)["cells"][4]["running"] == true);
        std::thread::sleep(Duration::from_millis(200));
        s.execute("notebook.interruptKernel", &Value::Null).unwrap();
        wait(&mut s, "interrupted", idle);
        assert_eq!(state(&mut s)["cells"][4]["outputs"][0]["ename"], "KeyboardInterrupt");
    }
    s.execute("notebook.restartKernel", &json!({ "runAll": true })).unwrap();
    wait(&mut s, "restart and run all", idle);
    assert_eq!(state(&mut s)["cells"][1]["executionCount"], 1, "counts start over");

    // Process Output saw the kernel.
    let po = s.execute("python.processOutput.state", &Value::Null).unwrap();
    assert!(po["processes"].as_array().unwrap().iter().any(|p| p["name"] == "Kernel: demo.ipynb"), "{po}");
    assert!(s.python.process_output.text().contains("Kernel ready"));

    // Outputs are saved in the file.
    s.execute("file.saveAll", &Value::Null).unwrap();
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["cells"][1]["outputs"][0]["text"], json!(["hello\n"]));
    assert_eq!(saved["cells"][1]["execution_count"], 1);

    // A kernel that dies is reported in the cell.
    s.execute("notebook.setCellSource", &json!({ "index": 4, "source": "import os\nos._exit(7)" })).unwrap();
    s.execute("notebook.runCell", &json!({ "index": 4 })).unwrap();
    wait(&mut s, "died", |s| state(s)["kernel"]["status"] == "not started");
    let st = state(&mut s);
    assert!(st["cells"][4]["outputs"][0]["text"].as_str().unwrap().contains("exited with code 7"), "{st}");
    // Running again starts a new kernel.
    s.execute("notebook.runCell", &json!({ "index": 1 })).unwrap();
    wait(&mut s, "rerun", |s| idle(s) && state(s)["cells"][1]["executionCount"] == 1);
    s.execute("notebook.shutdownKernel", &Value::Null).unwrap();
    assert_eq!(state(&mut s)["kernel"]["status"], "not started");
    s.shutdown();
}

fn has_module(py: &Path, module: &str) -> bool {
    std::process::Command::new(py).args(["-c", &format!("import {module}")]).output().is_ok_and(|o| o.status.success())
}

#[test]
fn pandas_and_numpy_pages() {
    let Some(py) = python3() else { return };
    if !has_module(&py, "pandas") || !has_module(&py, "numpy") {
        eprintln!("pandas/numpy not installed; skipping");
        return;
    }
    let (_tmp, _path, mut s) = setup();
    s.execute(
        "notebook.setCellSource",
        &json!({ "index": 1, "source": "import pandas as pd, numpy as np\ndf = pd.DataFrame({'n': range(1000), 's': ['v%d' % (i % 7) for i in range(1000)]})\narr = np.arange(12).reshape(3, 4)\ncube = np.zeros((2, 2, 2))" }),
    )
    .unwrap();
    s.execute("notebook.runCell", &json!({ "index": 1 })).unwrap();
    wait(&mut s, "run", idle);
    let st = state(&mut s);
    let vars = st["vars"].as_array().unwrap();
    let find = |n: &str| vars.iter().find(|v| v["name"] == n).unwrap().clone();
    assert_eq!(find("df")["viewable"], true);
    assert_eq!(find("df")["shape"], "(1000, 2)");
    assert_eq!(find("arr")["shape"], "(3, 4)");
    assert!(find("cube").get("viewable").is_none(), "3-D arrays need a slice");

    s.execute("python.dataframe.view", &json!({ "expression": "df", "source": "notebook" })).unwrap();
    let ready = |s: &mut Session| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none());
    wait(&mut s, "df", ready);
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!((v.total, v.rows.len()), (1000, 100), "only one page comes over");
    assert_eq!(v.dtypes, ["int64", "object"]);
    s.execute("python.dataframe.sort", &json!({ "column": 0, "descending": true })).unwrap();
    wait(&mut s, "df sorted", ready);
    assert_eq!(s.jupyter.frames.active().unwrap().rows[0][0], json!(999));
    s.execute("python.dataframe.filter", &json!({ "text": "v3" })).unwrap();
    wait(&mut s, "df filtered", ready);
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!(v.total, 143);
    assert_eq!(v.rows[0][1], json!("v3"));
    s.execute("python.dataframe.view", &json!({ "expression": "arr[1:]" })).unwrap();
    wait(&mut s, "arr", ready);
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!(v.columns, ["0", "1", "2", "3"]);
    assert_eq!(v.rows[0][0], json!(4));
    s.execute("python.dataframe.view", &json!({ "expression": "df['n']" })).unwrap();
    wait(&mut s, "series", ready);
    assert_eq!(s.jupyter.frames.active().unwrap().kind, "Series");
    s.shutdown();
}

#[test]
fn console_variables_open_in_the_viewer() {
    let Some(_) = python3() else { return };
    let (_tmp, _path, mut s) = setup();
    s.execute("python.console.start", &Value::Null).unwrap();
    wait(&mut s, "console", |s| s.python.console.ready);
    s.execute("python.console.execute", &json!({ "code": "table = [(i, str(i) * 2) for i in range(250)]" })).unwrap();
    wait(&mut s, "console run", |s| s.python.console.running.is_none());
    let v = s.python.console.vars.iter().find(|v| v.name == "table").unwrap();
    assert!(v.viewable);
    s.execute("python.dataframe.view", &json!({ "expression": "table", "source": "console" })).unwrap();
    wait(&mut s, "page", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    let v = s.jupyter.frames.active().unwrap();
    assert_eq!((v.total, v.rows.len(), v.columns.len()), (250, 100, 2));
    s.execute("python.dataframe.nextPage", &Value::Null).unwrap();
    wait(&mut s, "next", |s| s.jupyter.frames.active().is_some_and(|v| v.loading.is_none()));
    assert_eq!(s.jupyter.frames.active().unwrap().rows[0][1], json!("100100"));
    assert!(s.python.process_output.text().contains("[Python Console]"));
    s.execute("python.dataframe.close", &Value::Null).unwrap();
    assert!(s.jupyter.frames.views.is_empty());
    assert!(s.execute("python.dataframe.nextPage", &Value::Null).is_err());
    s.shutdown();
}

#[test]
fn track_commands_never_panic_with_a_notebook_open() {
    let (tmp, _path, mut s) = setup();
    let missing = tmp.path().join("nope.ipynb").display().to_string();
    let params = [
        Value::Null,
        json!({}),
        json!([1]),
        json!({ "index": u64::MAX, "type": "markdown", "source": "😀\u{0}" }),
        json!({ "index": -1, "page": -1, "pageSize": 0, "column": -3, "start": u64::MAX }),
        json!({ "path": missing }),
        json!({ "path": 7, "expression": "", "text": 5, "name": [] }),
        json!({ "expression": "x" , "source": "nowhere" }),
        json!({ "column": null, "descending": "yes", "limit": u64::MAX }),
    ];
    let prefixes = ["notebook.", "python.dataframe.", "python.processOutput.", "python.format.", "django.", "view.toolWindows.pythonProcessOutput"];
    for spec in registry().all() {
        if !prefixes.iter().any(|p| spec.id.starts_with(p)) {
            continue;
        }
        for p in &params {
            if let Err(CommandError::Failed(msg)) = s.execute(spec.id, p) {
                assert!(!msg.starts_with("internal error"), "{} panicked with {p}: {msg}", spec.id);
            }
            s.pump();
        }
    }
    s.shutdown();
}
