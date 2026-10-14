//! Notebook and View as DataFrame commands (M12 track PY). Every notebook command acts on the
//! active editor's notebook, or on `path` (opened when needed); cell commands default to the
//! selected cell.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::dataframe::{FrameSource, MAX_PAGE_SIZE};
use crate::jupyter::{self, NotebookDoc};
use crate::notebook::{self, CellKind, Notebook, OutputView};
use crate::params::{bool_or, opt_path, opt_str, opt_usize};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "notebook.new",
            "New Jupyter Notebook",
            None,
            "{path: string} — create an empty notebook file (one code cell) and open it",
            Context::Global,
            always,
            new_notebook,
        ),
        spec(
            "notebook.state",
            "Notebook State",
            None,
            "{path?} → {path, error, selected, kernel: {status, version, pid}, cells: [{index, id, type, source, \
             executionCount, pending, outputs: [{type, text, mime: [..]}]}], vars, awaitingInput}",
            Context::Global,
            always,
            state,
        ),
        spec("notebook.selectCell", "Select Cell", None, "{index: int, path?}", Context::Global, always, select_cell),
        spec(
            "notebook.addCellAbove",
            "Add Cell Above",
            None,
            "{index?, type?: code|markdown|raw (default code), source?, path?} — insert a cell above and select it",
            Context::Global,
            always,
            |s, p| add_cell(s, p, false),
        ),
        spec(
            "notebook.addCellBelow",
            "Add Cell Below",
            None,
            "{index?, type?: code|markdown|raw (default code), source?, path?} — insert a cell below and select it",
            Context::Global,
            always,
            |s, p| add_cell(s, p, true),
        ),
        spec("notebook.deleteCell", "Delete Cell", None, "{index?, path?}", Context::Global, always, delete_cell),
        spec("notebook.moveCellUp", "Move Cell Up", None, "{index?, path?}", Context::Global, always, |s, p| {
            move_cell(s, p, true)
        }),
        spec("notebook.moveCellDown", "Move Cell Down", None, "{index?, path?}", Context::Global, always, |s, p| {
            move_cell(s, p, false)
        }),
        spec(
            "notebook.setCellType",
            "Change Cell Type",
            None,
            "{type: code|markdown|raw, index?, path?} — code → markdown drops the outputs",
            Context::Global,
            always,
            set_cell_type,
        ),
        spec(
            "notebook.setCellSource",
            "Set Cell Source",
            None,
            "{source: string, index?, path?}",
            Context::Global,
            always,
            set_cell_source,
        ),
        spec(
            "notebook.editMarkdown",
            "Edit Markdown Cell",
            None,
            "{index?, editing?: bool (default true), path?} — show a Markdown cell's source (false renders it)",
            Context::Global,
            always,
            edit_markdown,
        ),
        spec(
            "notebook.runCell",
            "Run Cell",
            None,
            "{index?, advance?: bool, path?} — run the cell (render a Markdown cell); `advance` selects the next \
             cell, adding one at the end (Shift+Enter)",
            Context::Global,
            always,
            run_cell,
        ),
        spec("notebook.runAll", "Run All Cells", None, "{path?}", Context::Global, always, |s, p| {
            run_range(s, p, Range::All)
        }),
        spec(
            "notebook.runAbove",
            "Run All Above",
            None,
            "{index?, path?} — the cells above the selected one",
            Context::Global,
            always,
            |s, p| run_range(s, p, Range::Above),
        ),
        spec(
            "notebook.runBelow",
            "Run Cell and Below",
            None,
            "{index?, path?} — the selected cell and every cell after it",
            Context::Global,
            always,
            |s, p| run_range(s, p, Range::Below),
        ),
        spec("notebook.startKernel", "Start Kernel", None, "{path?}", Context::Global, always, start_kernel),
        spec(
            "notebook.restartKernel",
            "Restart Kernel",
            None,
            "{path?, runAll?: bool} — a fresh kernel (variables are lost); `runAll` runs every cell after",
            Context::Global,
            always,
            restart_kernel,
        ),
        spec(
            "notebook.interruptKernel",
            "Interrupt Kernel",
            None,
            "{path?} — KeyboardInterrupt in the running cell",
            Context::Global,
            always,
            interrupt_kernel,
        ),
        spec("notebook.shutdownKernel", "Shut Down Kernel", None, "{path?}", Context::Global, always, |s, p| {
            let i = target(s, p)?;
            jupyter::stop_kernel(s, i);
            ok()
        }),
        spec("notebook.clearOutputs", "Clear Cell Outputs", None, "{index?, path?}", Context::Global, always, clear_outputs),
        spec("notebook.clearAllOutputs", "Clear All Outputs", None, "{path?}", Context::Global, always, |s, p| {
            edit(s, p, |n| {
                if let Some(nb) = &mut n.nb {
                    nb.clear_all_outputs();
                }
                Ok(Value::Null)
            })
        }),
        spec(
            "notebook.sendInput",
            "Send Input to Kernel",
            None,
            "{text?: string, path?} — answer the running cell's input() (default: the input field)",
            Context::Global,
            always,
            send_input,
        ),
        // ------------------------------------------------------------ View as DataFrame
        spec(
            "python.dataframe.view",
            "View as DataFrame",
            None,
            "{expression: string (a variable or any expression), source?: console|notebook (default: the active \
             notebook, else the console), path?} — open a paged grid over a DataFrame, Series, 1-D/2-D array or list \
             of rows",
            Context::Global,
            always,
            frame_view,
        ),
        spec(
            "python.dataframe.page",
            "DataFrame Page",
            None,
            "{page?: int, start?: int, pageSize?: int (1–2000), colStart?: int} — fetch another page of the shown viewer",
            Context::Global,
            always,
            frame_page,
        ),
        spec("python.dataframe.nextPage", "Next DataFrame Page", None, "", Context::Global, always, |s, _| {
            frame_step(s, 1)
        }),
        spec("python.dataframe.previousPage", "Previous DataFrame Page", None, "", Context::Global, always, |s, _| {
            frame_step(s, -1)
        }),
        spec(
            "python.dataframe.sort",
            "Sort DataFrame",
            None,
            "{column: int|null (over all columns), descending?: bool} — sort in the process (null: original order)",
            Context::Global,
            always,
            frame_sort,
        ),
        spec(
            "python.dataframe.filter",
            "Filter DataFrame",
            None,
            "{text: string (empty clears), column?: int} — rows whose cells contain the text (case-insensitive)",
            Context::Global,
            always,
            frame_filter,
        ),
        spec("python.dataframe.refresh", "Refresh DataFrame", None, "", Context::Global, always, |s, _| {
            request_frame(s)?;
            ok()
        }),
        spec("python.dataframe.select", "Show DataFrame Viewer", None, "{index: int}", Context::Global, always, |s, p| {
            let i = opt_usize(p, "index")?.ok_or_else(|| CommandError::BadParams("missing `index`".into()))?;
            if i >= s.jupyter.frames.views.len() {
                return Err(CommandError::BadParams(format!("no viewer {i}")));
            }
            s.jupyter.frames.active = i;
            ok()
        }),
        spec("python.dataframe.close", "Close DataFrame Viewer", None, "{index?: int}", Context::Global, always, |s, p| {
            let i = opt_usize(p, "index")?.unwrap_or(s.jupyter.frames.active);
            s.jupyter.frames.close(i);
            ok()
        }),
        spec(
            "python.dataframe.state",
            "DataFrame Viewer State",
            None,
            "→ {views: [{expr, source}], active, view: {expr, kind, shape, total, start, pageSize, columns, dtypes, \
             labels, rows, sort, filter, error, loading, summary}}",
            Context::Global,
            always,
            frame_state,
        ),
    ]
}

// ---------------------------------------------------------------- targets

/// The notebook a command acts on: `path` (opened when needed), else the active editor's.
fn target(s: &mut Session, p: &Value) -> Result<usize, CommandError> {
    if let Some(path) = opt_path(p, "path") {
        if !jupyter::is_notebook_path(&path) {
            return Err(CommandError::BadParams(format!("{} is not a notebook (.ipynb)", path.display())));
        }
        let path = crate::normalize_path(&path);
        if let Some(i) = s.jupyter.notebooks.iter().position(|n| n.path.as_deref() == Some(path.as_path())) {
            return Ok(i);
        }
        s.open_file(&path)?;
        jupyter::follow_docs(s);
        return s
            .jupyter
            .notebooks
            .iter()
            .position(|n| n.path.as_deref() == Some(path.as_path()))
            .ok_or_else(|| CommandError::Failed("cannot open the notebook".into()));
    }
    let doc = s.ws.active().map(|(_, d)| d.id).ok_or_else(|| CommandError::Disabled("no notebook is open".into()))?;
    if s.jupyter.index_of_doc(doc).is_none() {
        jupyter::follow_docs(s);
    }
    s.jupyter.index_of_doc(doc).ok_or_else(|| CommandError::Disabled("the active editor is not a notebook".into()))
}

/// A parsed notebook to edit, and the cell index (`index`, else the selection).
fn cell_index(n: &NotebookDoc, p: &Value) -> Result<usize, CommandError> {
    let len = n.len();
    let i = opt_usize(p, "index")?.unwrap_or(n.selected);
    if i >= len {
        return Err(CommandError::BadParams(if len == 0 { "the notebook has no cells".into() } else { format!("no cell {i} (the notebook has {len})") }));
    }
    Ok(i)
}

fn notebook_mut(n: &mut NotebookDoc) -> Result<&mut Notebook, CommandError> {
    match &mut n.nb {
        Some(nb) => Ok(nb),
        None => Err(CommandError::Failed(format!(
            "the notebook could not be read: {}",
            n.error.as_deref().unwrap_or("unknown error")
        ))),
    }
}

/// Run an edit on the target notebook and mark it changed.
fn edit(s: &mut Session, p: &Value, f: impl FnOnce(&mut NotebookDoc) -> CmdResult) -> CmdResult {
    let i = target(s, p)?;
    let read_only = s.jupyter.notebooks.get(i).and_then(|n| s.ws.doc(n.doc)).is_some_and(|d| d.read_only);
    if read_only {
        return Err(CommandError::Failed("the notebook is read-only".into()));
    }
    let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
    notebook_mut(n)?;
    let r = f(n)?;
    n.touch();
    Ok(r)
}

fn kind_param(p: &Value) -> Result<Option<CellKind>, CommandError> {
    match opt_str(p, "type") {
        None => Ok(None),
        Some(t) => CellKind::parse(t)
            .map(Some)
            .ok_or_else(|| CommandError::BadParams(format!("unknown cell type `{t}` (code, markdown or raw)"))),
    }
}

// ---------------------------------------------------------------- notebook commands

fn new_notebook(s: &mut Session, p: &Value) -> CmdResult {
    let path = opt_path(p, "path").ok_or_else(|| CommandError::BadParams("missing `path`".into()))?;
    let path = match (path.is_relative(), s.project_root()) {
        (true, Some(root)) => root.join(path),
        _ => path,
    };
    let path: PathBuf = if jupyter::is_notebook_path(&path) { path } else { path.with_extension("ipynb") };
    if path.exists() {
        return Err(CommandError::Failed(format!("{} already exists", path.display())));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    }
    std::fs::write(&path, Notebook::empty().to_json())
        .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
    s.jobs_rescan();
    s.open_file(&path)?;
    jupyter::follow_docs(s);
    Ok(json!({ "path": path }))
}

fn output_json(o: &Value) -> Value {
    const MAX: usize = 20_000;
    let clip = |t: &str| notebook::clip(t, MAX).0.to_string();
    match notebook::output_view(o) {
        OutputView::Stream { stderr, text } => {
            json!({ "type": if stderr { "stderr" } else { "stdout" }, "text": clip(text) })
        }
        OutputView::Data { execution_count, data } => {
            let text = notebook::mime_text(data, "text/plain").unwrap_or_default();
            json!({
                "type": if execution_count.is_some() { "result" } else { "display" },
                "text": clip(&text),
                "mime": data.keys().collect::<Vec<_>>(),
            })
        }
        OutputView::Error { ename, evalue, traceback } => json!({
            "type": "error",
            "ename": ename,
            "evalue": evalue,
            "text": clip(&notebook::strip_ansi(&traceback.join("\n"))),
        }),
        OutputView::Other { output_type } => json!({ "type": output_type }),
    }
}

fn state(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    let n = s.jupyter.notebooks.get(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
    let cells: Vec<Value> = n
        .nb
        .as_ref()
        .map(|nb| {
            nb.cells
                .iter()
                .enumerate()
                .map(|(idx, c)| {
                    json!({
                        "index": idx,
                        "id": c.id(),
                        "type": c.kind.as_str(),
                        "source": c.source,
                        "executionCount": c.execution_count,
                        "pending": n.is_pending(c.uid),
                        "running": n.is_running(c.uid),
                        "editing": n.editing.contains(&c.uid),
                        "outputs": c.outputs.iter().map(output_json).collect::<Vec<_>>(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "path": n.path,
        "error": n.error,
        "selected": n.selected,
        "dirty": n.dirty,
        "kernel": {
            "status": n.kernel_status(),
            "version": n.kernel.version,
            "pid": n.kernel.pid(),
            "interpreter": n.kernel.interpreter,
            "exit": n.kernel_exit,
        },
        "cells": cells,
        "vars": n.vars,
        "awaitingInput": n.awaiting_input,
        "queued": n.queue.len(),
    }))
}

fn select_cell(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
    let idx = opt_usize(p, "index")?.ok_or_else(|| CommandError::BadParams("missing `index`".into()))?;
    if idx >= n.len() {
        return Err(CommandError::BadParams(format!("no cell {idx}")));
    }
    n.selected = idx;
    ok()
}

fn add_cell(s: &mut Session, p: &Value, below: bool) -> CmdResult {
    let kind = kind_param(p)?.unwrap_or(CellKind::Code);
    let source = opt_str(p, "source").unwrap_or("").to_string();
    let index = opt_usize(p, "index")?;
    edit(s, p, |n| {
        let len = n.len();
        let base = index.unwrap_or(n.selected).min(len);
        let at = if below && len > 0 { base + 1 } else { base };
        let nb = notebook_mut(n)?;
        let at = nb.insert_cell(at, kind, &source);
        let uid = nb.cells.get(at).map(|c| c.uid);
        n.selected = at;
        if kind == CellKind::Markdown
            && let Some(uid) = uid
        {
            n.editing.insert(uid);
        }
        Ok(json!({ "index": at }))
    })
}

fn delete_cell(s: &mut Session, p: &Value) -> CmdResult {
    edit(s, p, |n| {
        let i = cell_index(n, p)?;
        let cell = notebook_mut(n)?.delete_cell(i).map_err(CommandError::BadParams)?;
        n.queue.retain(|u| *u != cell.uid);
        n.editing.remove(&cell.uid);
        n.clamp_selection_pub();
        Ok(json!({ "deleted": i }))
    })
}

fn move_cell(s: &mut Session, p: &Value, up: bool) -> CmdResult {
    edit(s, p, |n| {
        let i = cell_index(n, p)?;
        let to = notebook_mut(n)?.move_cell(i, up).map_err(CommandError::BadParams)?;
        n.selected = to;
        Ok(json!({ "index": to }))
    })
}

fn set_cell_type(s: &mut Session, p: &Value) -> CmdResult {
    let kind = kind_param(p)?.ok_or_else(|| CommandError::BadParams("missing `type` (code, markdown or raw)".into()))?;
    edit(s, p, |n| {
        let i = cell_index(n, p)?;
        let nb = notebook_mut(n)?;
        nb.set_kind(i, kind).map_err(CommandError::BadParams)?;
        let uid = nb.cells.get(i).map(|c| c.uid).unwrap_or(0);
        if kind != CellKind::Code {
            n.queue.retain(|u| *u != uid);
        }
        if kind == CellKind::Markdown {
            n.editing.insert(uid);
        }
        Ok(Value::Null)
    })
}

fn set_cell_source(s: &mut Session, p: &Value) -> CmdResult {
    let source = match p.get("source") {
        Some(Value::String(t)) => t.clone(),
        _ => return Err(CommandError::BadParams("missing `source` (a string)".into())),
    };
    if source.len() > crate::notebook_kernel::MAX_CODE_BYTES {
        return Err(CommandError::BadParams("the cell source is too large".into()));
    }
    edit(s, p, |n| {
        let i = cell_index(n, p)?;
        notebook_mut(n)?.set_source(i, &source).map_err(CommandError::BadParams)?;
        Ok(Value::Null)
    })
}

fn edit_markdown(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
    let idx = cell_index(n, p)?;
    let uid = notebook_mut(n)?.cells.get(idx).map(|c| c.uid).unwrap_or(0);
    n.selected = idx;
    if bool_or(p, "editing", true) {
        n.editing.insert(uid);
    } else {
        n.editing.remove(&uid);
    }
    ok()
}

fn run_cell(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    let advance = bool_or(p, "advance", false);
    let (uid, kind, idx) = {
        let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
        let idx = cell_index(n, p)?;
        let c = notebook_mut(n)?.cells.get(idx).ok_or_else(|| CommandError::BadParams("no cell".into()))?;
        (c.uid, c.kind, idx)
    };
    let queued = if kind == CellKind::Code {
        jupyter::run_cells(s, i, vec![uid]).map_err(CommandError::Failed)?
    } else {
        if let Some(n) = s.jupyter.notebooks.get_mut(i) {
            n.editing.remove(&uid);
        }
        0
    };
    if advance && let Some(n) = s.jupyter.notebooks.get_mut(i) {
        let len = n.len();
        if idx + 1 >= len {
            if let Some(nb) = &mut n.nb {
                nb.insert_cell(len, CellKind::Code, "");
            }
            n.touch();
        }
        n.selected = idx + 1;
    }
    Ok(json!({ "queued": queued }))
}

enum Range {
    All,
    Above,
    Below,
}

fn run_range(s: &mut Session, p: &Value, range: Range) -> CmdResult {
    let i = target(s, p)?;
    let uids: Vec<u64> = {
        let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
        let sel = match range {
            Range::All => 0,
            _ => cell_index(n, p)?,
        };
        let nb = notebook_mut(n)?;
        let cells = nb.cells.iter().map(|c| c.uid);
        match range {
            Range::All => cells.collect(),
            Range::Above => cells.take(sel).collect(),
            Range::Below => cells.skip(sel).collect(),
        }
    };
    let queued = jupyter::run_cells(s, i, uids).map_err(CommandError::Failed)?;
    Ok(json!({ "queued": queued }))
}

fn start_kernel(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    if s.jupyter.notebooks.get(i).is_some_and(|n| n.kernel.is_alive()) {
        return ok();
    }
    let python = s.python.python_program();
    jupyter::start_kernel(s, i, &python).map_err(CommandError::Failed)?;
    ok()
}

fn restart_kernel(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    jupyter::stop_kernel(s, i);
    let python = s.python.python_program();
    jupyter::start_kernel(s, i, &python).map_err(CommandError::Failed)?;
    if let Some(n) = s.jupyter.notebooks.get_mut(i)
        && let Some(nb) = &mut n.nb
    {
        // Execution counts start over with the new kernel.
        if bool_or(p, "runAll", false) {
            nb.clear_all_outputs();
            n.touch();
        }
    }
    if bool_or(p, "runAll", false) {
        return run_range(s, p, Range::All);
    }
    s.status = "Kernel restarted".into();
    ok()
}

fn interrupt_kernel(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
    n.queue.clear();
    if n.running.is_none() {
        return ok();
    }
    n.kernel.interrupt().map_err(CommandError::Failed)?;
    ok()
}

fn clear_outputs(s: &mut Session, p: &Value) -> CmdResult {
    edit(s, p, |n| {
        let i = cell_index(n, p)?;
        notebook_mut(n)?.clear_outputs(i).map_err(CommandError::BadParams)?;
        Ok(Value::Null)
    })
}

fn send_input(s: &mut Session, p: &Value) -> CmdResult {
    let i = target(s, p)?;
    let n = s.jupyter.notebooks.get_mut(i).ok_or_else(|| CommandError::Failed("no notebook".into()))?;
    if n.awaiting_input.is_none() {
        return Err(CommandError::Failed("the kernel is not waiting for input".into()));
    }
    let text = match opt_str(p, "text") {
        Some(t) => t.to_string(),
        None => std::mem::take(&mut n.input_draft),
    };
    let text = text.trim_end_matches(['\n', '\r']).to_string();
    n.kernel.send_input(&text).map_err(CommandError::Failed)?;
    n.awaiting_input = None;
    if let Some(cell) = n.running.and_then(|(_, uid)| n.nb.as_mut()?.cell_mut(uid)) {
        cell.append_stream("stdout", &format!("{text}\n"));
    }
    n.touch();
    ok()
}

impl NotebookDoc {
    fn clamp_selection_pub(&mut self) {
        self.selected = self.selected.min(self.len().saturating_sub(1));
    }
}

// ---------------------------------------------------------------- View as DataFrame

fn frame_view(s: &mut Session, p: &Value) -> CmdResult {
    let expr = opt_str(p, "expression")
        .or_else(|| opt_str(p, "name"))
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(|| CommandError::BadParams("missing `expression` (a variable name or expression)".into()))?
        .to_string();
    if expr.len() > 10_000 {
        return Err(CommandError::BadParams("the expression is too long".into()));
    }
    let notebook_doc = |s: &mut Session, i: usize| {
        s.jupyter.notebooks.get(i).map(|n| n.doc).ok_or_else(|| CommandError::Failed("no notebook".into()))
    };
    let source = match opt_str(p, "source") {
        Some("console") => FrameSource::Console,
        Some("notebook") => {
            let i = target(s, p)?;
            FrameSource::notebook(notebook_doc(s, i)?)
        }
        Some(other) => return Err(CommandError::BadParams(format!("unknown source `{other}` (console or notebook)"))),
        None => match target(s, p) {
            Ok(i) => FrameSource::notebook(notebook_doc(s, i)?),
            Err(_) => FrameSource::Console,
        },
    };
    s.jupyter.frames.open(source, &expr);
    request_frame(s)?;
    Ok(json!({ "index": s.jupyter.frames.active }))
}

/// Ask the shown viewer's process for its page.
fn request_frame(s: &mut Session) -> Result<(), CommandError> {
    let req = s.jupyter.frames.next_req();
    let view = s.jupyter.frames.active_mut().ok_or_else(|| CommandError::Disabled("no DataFrame viewer is open".into()))?;
    let msg = view.request(req);
    let source = view.source.clone();
    let sent = match &source {
        FrameSource::Console => s.python.console.send_op(&msg),
        FrameSource::Notebook { doc } => s
            .jupyter
            .notebooks
            .iter()
            .find(|n| n.doc.0 == *doc)
            .ok_or_else(|| "the notebook was closed".to_string())
            .and_then(|n| n.kernel.request(&msg)),
    };
    let view = s.jupyter.frames.active_mut().ok_or_else(|| CommandError::Disabled("no DataFrame viewer is open".into()))?;
    match sent {
        Ok(()) => {
            view.loading = Some(req);
            Ok(())
        }
        Err(e) => {
            view.loading = None;
            view.error = Some(e.clone());
            Err(CommandError::Failed(e))
        }
    }
}

fn frame_page(s: &mut Session, p: &Value) -> CmdResult {
    let view = s.jupyter.frames.active_mut().ok_or_else(|| CommandError::Disabled("no DataFrame viewer is open".into()))?;
    if let Some(size) = opt_usize(p, "pageSize")? {
        view.page_size = size.clamp(1, MAX_PAGE_SIZE);
    }
    if let Some(page) = opt_usize(p, "page")? {
        view.start = (page as u64).saturating_mul(view.page_size as u64);
    }
    if let Some(start) = opt_usize(p, "start")? {
        view.start = start as u64;
    }
    if let Some(c) = opt_usize(p, "colStart")? {
        view.col_start = c;
    }
    request_frame(s)?;
    ok()
}

fn frame_step(s: &mut Session, delta: i64) -> CmdResult {
    let view = s.jupyter.frames.active_mut().ok_or_else(|| CommandError::Disabled("no DataFrame viewer is open".into()))?;
    let size = view.page_size.max(1) as u64;
    let last = view.page_count().saturating_sub(1);
    let page = if delta < 0 { view.page().saturating_sub(1) } else { (view.page() + 1).min(last) };
    view.start = page * size;
    request_frame(s)?;
    ok()
}

fn frame_sort(s: &mut Session, p: &Value) -> CmdResult {
    let view = s.jupyter.frames.active_mut().ok_or_else(|| CommandError::Disabled("no DataFrame viewer is open".into()))?;
    view.sort = match p.get("column") {
        None => return Err(CommandError::BadParams("missing `column` (an index, or null)".into())),
        Some(Value::Null) => None,
        Some(_) => {
            let c = opt_usize(p, "column")?.ok_or_else(|| CommandError::BadParams("bad `column`".into()))?;
            Some((c, !bool_or(p, "descending", false)))
        }
    };
    view.start = 0;
    request_frame(s)?;
    ok()
}

fn frame_filter(s: &mut Session, p: &Value) -> CmdResult {
    let view = s.jupyter.frames.active_mut().ok_or_else(|| CommandError::Disabled("no DataFrame viewer is open".into()))?;
    let text = match p.get("text") {
        Some(Value::String(t)) => t.chars().take(1000).collect(),
        Some(Value::Null) | None => String::new(),
        Some(_) => return Err(CommandError::BadParams("`text` must be a string".into())),
    };
    view.filter = text;
    view.filter_column = opt_usize(p, "column")?;
    view.start = 0;
    request_frame(s)?;
    ok()
}

fn frame_state(s: &mut Session, _: &Value) -> CmdResult {
    let f = &s.jupyter.frames;
    Ok(json!({
        "views": f.views.iter().map(|v| json!({ "expr": v.expr, "source": v.source })).collect::<Vec<_>>(),
        "active": f.active,
        "view": f.active().map(|v| {
            let mut j = serde_json::to_value(v).unwrap_or(Value::Null);
            if let Some(m) = j.as_object_mut() {
                m.insert("summary".into(), Value::String(v.summary()));
            }
            j
        }),
    }))
}
