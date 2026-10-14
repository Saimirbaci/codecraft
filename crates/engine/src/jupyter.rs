//! Jupyter notebooks in the IDE (M12 track PY): every open `.ipynb` document gets a
//! [`NotebookDoc`] with its parsed [`Notebook`], a kernel ([`Kernel`]) and a run queue.
//!
//! The document's text stays the file's JSON (so Save, Close and Reload work as for any file):
//! notebook edits change the model and are written into the document (a minimal line diff, one
//! undo step) once typing pauses ([`SYNC_DELAY`]) and always before saving; a document changed
//! from outside the notebook editor (reload, undo, an edit in a text view) is parsed again. A
//! notebook that does not parse shows as text with the reason.
//!
//! Running: cells are queued; the next starts when the kernel is idle, its outputs are cleared
//! and the kernel's events fill them; an error stops the rest of the queue (as Jupyter does).
//! The kernel starts on the first run with the project's interpreter, in the notebook's
//! directory. Commands are in [`crate::notebook_cmds`].

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codecraft_text::{EditGroup, SelectionSet};
use codecraft_workspace::DocId;

use crate::Session;
use crate::dataframe::FramesState;
use crate::notebook::{self, CellKind, Notebook};
use crate::notebook_kernel::{Kernel, KernelEvent};
use crate::python_console::ConsoleVar;
use crate::python_output::LineKind;

/// How long notebook edits wait before they are written into the document.
pub const SYNC_DELAY: Duration = Duration::from_millis(250);

/// Whether `path` is a notebook file.
pub fn is_notebook_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
}

/// One open notebook.
#[derive(Debug)]
pub struct NotebookDoc {
    pub doc: DocId,
    pub path: Option<PathBuf>,
    /// The parsed notebook (`None` when the text does not parse: see `error`).
    pub nb: Option<Notebook>,
    pub error: Option<String>,
    /// The document revision the model matches.
    synced_rev: u64,
    /// The model has edits not written into the document yet.
    pub dirty: bool,
    dirty_at: Option<Instant>,
    /// The selected cell.
    pub selected: usize,
    /// Markdown cells shown as source (being edited), by uid.
    pub editing: BTreeSet<u64>,
    pub kernel: Kernel,
    /// Cells waiting to run (uids).
    pub queue: VecDeque<u64>,
    /// The running cell: (request id, cell uid).
    pub running: Option<(u64, u64)>,
    /// The kernel's namespace after the last run.
    pub vars: Vec<ConsoleVar>,
    /// The running cell called `input()` (its prompt).
    pub awaiting_input: Option<String>,
    /// The answer being typed.
    pub input_draft: String,
    /// The kernel's last exit message.
    pub kernel_exit: Option<String>,
}

impl NotebookDoc {
    fn new(doc: DocId, path: Option<PathBuf>, text: &str, rev: u64) -> Self {
        let (nb, error) = match Notebook::parse(text) {
            Ok(nb) => (Some(nb), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            doc,
            path,
            nb,
            error,
            synced_rev: rev,
            dirty: false,
            dirty_at: None,
            selected: 0,
            editing: BTreeSet::new(),
            kernel: Kernel::default(),
            queue: VecDeque::new(),
            running: None,
            vars: Vec::new(),
            awaiting_input: None,
            input_draft: String::new(),
            kernel_exit: None,
        }
    }

    /// The file name.
    pub fn title(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled.ipynb".into())
    }

    /// Mark the model changed (written into the document after [`SYNC_DELAY`]).
    pub fn touch(&mut self) {
        self.dirty = true;
        self.dirty_at = Some(Instant::now());
    }

    /// The cell is queued or running.
    pub fn is_pending(&self, uid: u64) -> bool {
        self.running.is_some_and(|(_, u)| u == uid) || self.queue.contains(&uid)
    }

    pub fn is_running(&self, uid: u64) -> bool {
        self.running.is_some_and(|(_, u)| u == uid)
    }

    /// `idle`, `busy`, `starting`, `not started`.
    pub fn kernel_status(&self) -> &'static str {
        if !self.kernel.is_alive() {
            "not started"
        } else if !self.kernel.ready {
            "starting"
        } else if self.running.is_some() || !self.queue.is_empty() {
            "busy"
        } else {
            "idle"
        }
    }

    /// The cell count, clamping the selection.
    pub fn len(&self) -> usize {
        self.nb.as_ref().map_or(0, |n| n.cells.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.len().saturating_sub(1));
    }

    fn running_cell(&mut self) -> Option<&mut notebook::Cell> {
        let uid = self.running?.1;
        self.nb.as_mut()?.cell_mut(uid)
    }
}

/// Notebooks and View as DataFrame windows.
#[derive(Debug, Default)]
pub struct JupyterState {
    pub notebooks: Vec<NotebookDoc>,
    pub frames: FramesState,
}

impl JupyterState {
    /// Whether kernels are working (the UI keeps polling).
    pub fn busy(&self) -> bool {
        self.notebooks.iter().any(|n| {
            n.dirty || n.running.is_some() || !n.queue.is_empty() || (n.kernel.is_alive() && !n.kernel.ready)
        }) || self.frames.views.iter().any(|v| v.loading.is_some())
    }

    pub fn index_of_doc(&self, doc: DocId) -> Option<usize> {
        self.notebooks.iter().position(|n| n.doc == doc)
    }

    pub fn for_doc(&self, doc: DocId) -> Option<&NotebookDoc> {
        self.notebooks.iter().find(|n| n.doc == doc)
    }

    pub fn for_doc_mut(&mut self, doc: DocId) -> Option<&mut NotebookDoc> {
        self.notebooks.iter_mut().find(|n| n.doc == doc)
    }
}

/// Whether the editor for `doc` shows a notebook (it parsed).
pub fn shows_notebook(s: &Session, doc: DocId) -> bool {
    s.jupyter.for_doc(doc).is_some_and(|n| n.nb.is_some())
}

/// Create views for newly opened notebook documents and drop those of closed ones.
pub(crate) fn follow_docs(s: &mut Session) -> bool {
    let mut changed = false;
    let open: Vec<(DocId, Option<PathBuf>)> = s
        .ws
        .docs()
        .filter(|d| d.path.as_deref().is_some_and(is_notebook_path))
        .map(|d| (d.id, d.path.clone()))
        .collect();
    let before = s.jupyter.notebooks.len();
    s.jupyter.notebooks.retain(|n| open.iter().any(|(d, _)| *d == n.doc));
    changed |= s.jupyter.notebooks.len() != before;
    for (doc, path) in open {
        if s.jupyter.index_of_doc(doc).is_some() {
            continue;
        }
        let Some(od) = s.ws.doc(doc) else { continue };
        let nb = NotebookDoc::new(doc, path, &od.doc.text(), od.doc.revision());
        if let Some(e) = &nb.error {
            s.status = format!("{}: {e} (shown as text)", nb.title());
        }
        s.jupyter.notebooks.push(nb);
        changed = true;
    }
    changed
}

/// Drain kernels, start queued cells, keep documents in sync. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = follow_docs(s);
    for i in 0..s.jupyter.notebooks.len() {
        changed |= reparse_if_changed(s, i);
        changed |= drain_kernel(s, i);
        changed |= start_next(s, i);
        let due = s.jupyter.notebooks.get(i).is_some_and(|n| n.dirty && n.dirty_at.is_none_or(|t| t.elapsed() >= SYNC_DELAY));
        if due {
            flush(s, i);
            changed = true;
        }
    }
    // View as DataFrame answers from the Python Console.
    let frames = std::mem::take(&mut s.python.console.frames);
    for f in frames {
        changed |= s.jupyter.frames.deliver(&f);
    }
    changed
}

/// The document changed under the model (reload, undo, a text edit): parse it again.
fn reparse_if_changed(s: &mut Session, i: usize) -> bool {
    let Some(n) = s.jupyter.notebooks.get(i) else { return false };
    let Some(od) = s.ws.doc(n.doc) else { return false };
    let rev = od.doc.revision();
    if rev == n.synced_rev || n.dirty {
        return false;
    }
    let text = od.doc.text();
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return false };
    n.synced_rev = rev;
    match Notebook::parse(&text) {
        Ok(nb) => {
            // Keep the selection and the run state of cells that still exist (by position).
            n.nb = Some(nb);
            n.error = None;
            n.queue.clear();
            if n.running.is_some() {
                // The running cell's output has nowhere to go: it lands in the log.
                n.running = n.running.map(|(req, _)| (req, 0));
            }
            n.editing.clear();
            n.clamp_selection();
        }
        Err(e) => {
            n.nb = None;
            n.error = Some(e);
        }
    }
    true
}

/// Write the model into the document (one undoable edit; carets of text views are kept).
pub(crate) fn flush(s: &mut Session, i: usize) {
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return };
    n.dirty = false;
    n.dirty_at = None;
    let Some(nb) = &n.nb else { return };
    let text = nb.to_json();
    let doc = n.doc;
    let editor = s.ws.editor_for_doc(doc);
    let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_else(|| SelectionSet::caret(0));
    let Some(od) = s.ws.doc_mut(doc) else { return };
    if od.doc.text() == text {
        let rev = od.doc.revision();
        if let Some(n) = s.jupyter.notebooks.get_mut(i) {
            n.synced_rev = rev;
        }
        return;
    }
    if od.read_only {
        s.status = "The notebook is read-only: changes are not saved".into();
        return;
    }
    let sels = sels.clamp(od.doc.len_chars());
    let edit = crate::reformat::diff_edit(&od.doc, &text, &sels);
    match od.doc.apply(&edit, &sels, EditGroup::Standalone) {
        Ok(()) => {
            let rev = od.doc.revision();
            if let Some(e) = editor {
                s.ws.set_selections(e, edit.selections);
            }
            if let Some(n) = s.jupyter.notebooks.get_mut(i) {
                n.synced_rev = rev;
            }
        }
        Err(e) => s.status = format!("Cannot update the notebook: {e}"),
    }
}

/// Write every notebook's pending edits into its document (before saving).
pub(crate) fn before_save(s: &mut Session) {
    for i in 0..s.jupyter.notebooks.len() {
        if s.jupyter.notebooks.get(i).is_some_and(|n| n.dirty) {
            flush(s, i);
        }
    }
}

/// Stop every kernel (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    for n in &mut s.jupyter.notebooks {
        n.kernel.stop();
        n.queue.clear();
        n.running = None;
    }
}

/// Queue cells (by uid) to run; markdown cells are rendered instead. Starts the kernel if needed.
pub(crate) fn run_cells(s: &mut Session, i: usize, uids: Vec<u64>) -> Result<usize, String> {
    let python = s.python.python_program();
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return Err("no notebook".into()) };
    let Some(nb) = &n.nb else { return Err("the notebook could not be read".into()) };
    let mut queued = 0;
    let mut code = Vec::new();
    for uid in uids {
        let Some(cell) = nb.cells.iter().find(|c| c.uid == uid) else { continue };
        if cell.kind == CellKind::Code {
            code.push(uid);
        }
    }
    for uid in &code {
        if !n.is_pending(*uid) {
            n.queue.push_back(*uid);
            queued += 1;
        }
    }
    if queued > 0 && !n.kernel.is_alive() {
        start_kernel(s, i, &python)?;
    }
    Ok(queued)
}

/// Start (or restart) a notebook's kernel.
pub(crate) fn start_kernel(s: &mut Session, i: usize, python: &Path) -> Result<(), String> {
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return Err("no notebook".into()) };
    let cwd = n.path.as_deref().and_then(Path::parent).map(Path::to_path_buf).or_else(|| s.ws.project.as_ref().map(|p| p.root.clone()));
    let title = format!("Kernel: {}", n.title());
    n.running = None;
    n.awaiting_input = None;
    n.kernel_exit = None;
    let result = n.kernel.start(python, cwd.as_deref());
    let out = &mut s.python.process_output;
    match &result {
        Ok(()) => out.push(&title, LineKind::Command, &format!("{} -u -c <kernel driver>  (in {})", python.display(), cwd.as_deref().map(|d| d.display().to_string()).unwrap_or_else(|| ".".into()))),
        Err(e) => out.push(&title, LineKind::Stderr, e),
    }
    result
}

/// Stop a kernel (the queue is dropped).
pub(crate) fn stop_kernel(s: &mut Session, i: usize) {
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return };
    let was = n.kernel.is_alive();
    n.kernel.stop();
    n.queue.clear();
    n.awaiting_input = None;
    if let Some(cell) = n.running_cell()
        && cell.execution_count.is_none()
    {
        cell.append_stream("stderr", "The kernel was stopped.\n");
    }
    n.running = None;
    n.vars.clear();
    if was {
        let title = format!("Kernel: {}", n.title());
        s.python.process_output.push(&title, LineKind::Info, "Kernel stopped");
    }
}

/// Start the next queued cell when the kernel is idle.
fn start_next(s: &mut Session, i: usize) -> bool {
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return false };
    if !n.kernel.ready || n.running.is_some() || n.kernel.running.is_some() {
        return false;
    }
    while let Some(uid) = n.queue.pop_front() {
        let Some(nb) = &mut n.nb else { return false };
        let Some(cell) = nb.cell_mut(uid) else { continue };
        if cell.kind != CellKind::Code {
            continue;
        }
        let code = cell.source.clone();
        cell.clear_outputs();
        match n.kernel.execute(&code) {
            Ok(req) => {
                n.running = Some((req, uid));
                n.touch();
                return true;
            }
            Err(e) => {
                cell.append_stream("stderr", &format!("{e}\n"));
                n.queue.clear();
                n.touch();
                return true;
            }
        }
    }
    false
}

fn drain_kernel(s: &mut Session, i: usize) -> bool {
    let Some(n) = s.jupyter.notebooks.get_mut(i) else { return false };
    let events = n.kernel.poll();
    if events.is_empty() {
        return false;
    }
    let title = format!("Kernel: {}", n.title());
    let mut log: Vec<(LineKind, String)> = Vec::new();
    let mut frames = Vec::new();
    for e in events {
        let current = |id: Option<u64>, n: &NotebookDoc| id.is_none() || n.running.is_some_and(|(r, _)| Some(r) == id);
        match e {
            KernelEvent::Ready { version } => {
                log.push((LineKind::Info, format!("Kernel ready: Python {version} (pid {})", n.kernel.pid().unwrap_or(0))));
            }
            KernelEvent::Start { .. } => {}
            KernelEvent::Stream { id, name, text } => {
                if current(id, n)
                    && let Some(c) = n.running_cell()
                {
                    c.append_stream(&name, &text);
                    n.touch();
                } else {
                    log.push((if name == "stderr" { LineKind::Stderr } else { LineKind::Stdout }, text));
                }
            }
            KernelEvent::Display { id, data } => {
                if current(id, n)
                    && let Some(c) = n.running_cell()
                {
                    c.push_output(notebook::display_output(data));
                    n.touch();
                }
            }
            KernelEvent::Result { id, count, data } => {
                if current(id, n)
                    && let Some(c) = n.running_cell()
                {
                    c.push_output(notebook::result_output(Some(count), data));
                    n.touch();
                }
            }
            KernelEvent::Error { id, ename, evalue, traceback } => {
                if current(id, n)
                    && let Some(c) = n.running_cell()
                {
                    c.push_output(notebook::error_output(&ename, &evalue, traceback));
                    n.touch();
                }
            }
            KernelEvent::Input { prompt } => n.awaiting_input = Some(prompt),
            KernelEvent::Done { id, status, count, vars } => {
                if let Some(vars) = vars {
                    n.vars = vars;
                }
                if n.running.is_some_and(|(r, _)| Some(r) == id) || id.is_none() {
                    if let Some(c) = n.running_cell() {
                        c.execution_count = Some(count);
                    }
                    n.running = None;
                    n.awaiting_input = None;
                    n.touch();
                    if status == "error" {
                        n.queue.clear();
                    }
                }
            }
            KernelEvent::Frame(v) => frames.push(v),
            KernelEvent::Raw { stderr, text } => {
                if let Some(c) = n.running_cell() {
                    c.append_stream(if stderr { "stderr" } else { "stdout" }, &text);
                    n.touch();
                }
                if stderr {
                    log.push((LineKind::Stderr, text));
                }
            }
            KernelEvent::Exited { code } => {
                let msg = match code {
                    Some(c) => format!("Kernel exited with code {c}"),
                    None => "Kernel exited".to_string(),
                };
                if let Some(c) = n.running_cell() {
                    c.append_stream("stderr", &format!("{msg}: the kernel died while running this cell. Restart it to continue.\n"));
                }
                n.running = None;
                n.queue.clear();
                n.awaiting_input = None;
                n.kernel_exit = Some(msg.clone());
                n.touch();
                log.push((LineKind::Info, msg));
            }
        }
    }
    for (kind, text) in log {
        s.python.process_output.push(&title, kind, &text);
    }
    for f in frames {
        s.jupyter.frames.deliver(&f);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notebook_paths() {
        assert!(is_notebook_path(Path::new("/a/b.ipynb")));
        assert!(is_notebook_path(Path::new("B.IPYNB")));
        assert!(!is_notebook_path(Path::new("b.py")));
        assert!(!is_notebook_path(Path::new("ipynb")));
    }
}
