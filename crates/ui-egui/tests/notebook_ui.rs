//! The notebook editor, Data View and Python Process Output (M12 track PY) driven headlessly:
//! cells and outputs render (text, error, PNG), toolbar buttons add cells, Run All runs in the
//! real `python3` (skipped without it) and a variable opens in the Data View.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::python_env::Probe;
use codecraft_ui_egui::App;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

fn notebook() -> String {
    json!({
        "cells": [
            { "cell_type": "markdown", "id": "m1", "metadata": {}, "source": ["# Notebook Title\n", "Some prose."] },
            { "cell_type": "code", "id": "c1", "metadata": {}, "execution_count": 4, "source": ["rows = [[1, 'a'], [2, 'b']]\n", "print('printed-text')"],
              "outputs": [
                  { "output_type": "stream", "name": "stdout", "text": ["saved-output\n"] },
                  { "output_type": "display_data", "data": { "image/png": PNG, "text/plain": ["<Figure>"] }, "metadata": {} },
                  { "output_type": "error", "ename": "KeyError", "evalue": "'k'", "traceback": ["\u{1b}[0;31mKeyError\u{1b}[0m: 'k'"] }
              ] }
        ],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    })
    .to_string()
}

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

fn cells(h: &mut Harness<'_, App>) -> Vec<Value> {
    let st = h.state_mut().session.execute("notebook.state", &Value::Null).unwrap();
    st["cells"].as_array().unwrap().clone()
}

#[test]
fn notebook_editor_renders_cells_and_runs_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("demo.ipynb");
    std::fs::write(&path, notebook()).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe({
        let root = root.clone();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.python.probe = Some(Probe::isolated(Vec::new()));
    settle(&mut h, "project", |a| a.session.ws.project.is_some() && !a.session.jobs_running());
    h.state_mut().session.execute("file.open", &json!({ "path": path })).unwrap();
    h.run_steps(4);
    assert!(h.query_by_label("Notebook Title").is_some(), "the Markdown cell is rendered");
    assert!(h.query_by_label("saved-output").is_some(), "stream output");
    assert!(h.query_by_label("KeyError: 'k'").is_some(), "error output");
    assert!(h.query_by_label("[4]").is_some(), "execution count");
    assert!(h.query_by_label("Variables").is_some());

    // The toolbar adds a cell below the selection.
    h.get_by_label("+ Markdown").click();
    h.run_steps(3);
    let c = cells(&mut h);
    assert_eq!(c.len(), 3);
    assert_eq!(c[1]["type"], "markdown");
    // Typing in the raw JSON never happens: the document only changes through the model.
    let doc = h.state().session.ws.active().unwrap().1.id;
    assert!(codecraft_engine::jupyter::shows_notebook(&h.state().session, doc));

    if std::env::split_paths(&std::env::var_os("PATH").unwrap()).all(|d| !d.join("python3").is_file()) {
        eprintln!("python3 not installed; skipping the kernel part");
        return;
    }
    h.get_by_label("⏩").click();
    settle(&mut h, "run all", |a| {
        a.session.jupyter.notebooks.first().is_some_and(|n| n.kernel_status() == "idle" && !n.vars.is_empty())
    });
    h.run_steps(3);
    assert!(h.query_by_label("printed-text").is_some(), "new output replaced the saved one");
    assert!(h.query_by_label("saved-output").is_none());
    assert!(h.query_by_label("[1]").is_some());
    // The variable opens in the Data View.
    h.get_by_label("⊞ View").click();
    settle(&mut h, "frame", |a| a.session.jupyter.frames.active().is_some_and(|v| v.loading.is_none() && !v.rows.is_empty()));
    h.run_steps(3);
    assert!(h.query_by_label("Data View").is_some());
    assert!(h.query_by_label("page 1 / 1").is_some());

    // Python Process Output lists the kernel.
    h.state_mut().session.execute("view.toolWindows.pythonProcessOutput", &Value::Null).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("● Kernel: demo.ipynb").count() == 1, "the running kernel");
    h.state_mut().session.shutdown();
}
