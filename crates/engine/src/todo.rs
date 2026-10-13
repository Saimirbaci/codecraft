//! The TODO tool window: `TODO` and `FIXME` in comments across the project (RustRover's default
//! patterns `\btodo\b.*` and `\bfixme\b.*`, case-insensitive), grouped by file.
//!
//! The project is scanned on a worker thread (ignore rules respected, `find_in_files` prefilters
//! candidate lines, the language's lexer decides what is a comment). Open documents are
//! re-read from memory as they change while the tool window is shown, and files changed on disk
//! are rescanned when the file watcher reports them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::{HighlightKind, Language};
use codecraft_workspace::ToolWindowId;

use crate::Session;
use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::{opt_usize, req_path, req_usize};

/// Items kept per scan (a generated file full of TODOs cannot flood the tool window).
pub const MAX_ITEMS: usize = 5000;
/// Longest item text kept.
const MAX_TEXT: usize = 300;
/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Changed files rescanned at once (a branch switch touching more triggers a full scan).
const MAX_RESCAN: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TodoKind {
    Todo,
    Fixme,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Zero-based line and char column of the keyword.
    pub line: usize,
    pub column: usize,
    pub kind: TodoKind,
    /// From the keyword to the end of the line (comment closers trimmed).
    pub text: String,
}

enum Msg {
    /// A full scan finished.
    Full(BTreeMap<PathBuf, Vec<TodoItem>>, bool),
    /// Some files were rescanned (an empty list removes the file).
    Files(Vec<(PathBuf, Vec<TodoItem>)>),
}

#[derive(Default)]
pub struct TodoState {
    pub files: BTreeMap<PathBuf, Vec<TodoItem>>,
    pub scanning: bool,
    /// The scan stopped at [`MAX_ITEMS`].
    pub truncated: bool,
    /// The project root the results belong to.
    project: Option<PathBuf>,
    /// A full scan was started for `project`.
    started: bool,
    full: Option<Receiver<Msg>>,
    rescans: Vec<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
    /// Revision of each open document the results were taken from.
    doc_revs: HashMap<PathBuf, u64>,
}

impl TodoState {
    pub fn count(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    pub fn busy(&self) -> bool {
        self.scanning || !self.rescans.is_empty()
    }
}

fn keyword() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(todo|fixme)\b").ok()).as_ref()
}

/// TODO items of a text in `language`: keyword matches inside comments (anywhere in plain text).
pub fn scan_text(language: Language, text: &str) -> Vec<TodoItem> {
    let Some(re) = keyword() else { return Vec::new() };
    if !re.is_match(text) {
        return Vec::new();
    }
    let anywhere = language == Language::PlainText;
    let hl = (!anywhere).then(|| codecraft_syntax::highlight(language, text));
    let mut out = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        let Some(m) = re.find(line) else { continue };
        // Later matches on the line may be in a comment even if the first is not (`s = "todo" // TODO`).
        let found = re.find_iter(line).find(|m| {
            anywhere
                || hl.as_ref().is_some_and(|h| {
                    let col = line.get(..m.start()).map(|p| p.chars().count()).unwrap_or(0);
                    h.line(line_no).iter().any(|sp| {
                        sp.start <= col
                            && col < sp.end
                            && matches!(sp.kind, HighlightKind::Comment | HighlightKind::DocComment)
                    })
                })
        });
        let Some(found) = found.or(anywhere.then_some(m)) else { continue };
        let column = line.get(..found.start()).map(|p| p.chars().count()).unwrap_or(0);
        let rest = line.get(found.start()..).unwrap_or("");
        let kind = if found.as_str().eq_ignore_ascii_case("fixme") { TodoKind::Fixme } else { TodoKind::Todo };
        out.push(TodoItem { line: line_no, column, kind, text: clean(rest) });
        if out.len() >= MAX_ITEMS {
            break;
        }
    }
    out
}

fn clean(rest: &str) -> String {
    let mut t = rest.trim_end().trim_end_matches('\r');
    for closer in ["*/", "-->"] {
        t = t.strip_suffix(closer).unwrap_or(t).trim_end();
    }
    t.chars().take(MAX_TEXT).collect()
}

fn scan_file(path: &Path) -> Vec<TodoItem> {
    if std::fs::metadata(path).map(|m| !m.is_file() || m.len() > MAX_FILE_BYTES).unwrap_or(true) {
        return Vec::new();
    }
    match codecraft_fs::read_text_file(path) {
        Ok(text) => scan_text(Language::from_path(path), &text),
        Err(_) => Vec::new(),
    }
}

fn in_private_dir(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| rel.starts_with(".codecraft"))
}

/// Scan the whole project (blocking; runs on a worker thread).
pub fn scan_project(root: &Path, cancel: &AtomicBool) -> (BTreeMap<PathBuf, Vec<TodoItem>>, bool) {
    let mut out = BTreeMap::new();
    let Some(re) = keyword() else { return (out, false) };
    let mut opts = codecraft_fs::FindOptions { max_results: MAX_ITEMS * 4, max_file_size: MAX_FILE_BYTES, ..Default::default() };
    opts.scan.exclude_dirs.push(".codecraft".into());
    let Ok(matches) = codecraft_fs::find_in_files(root, re, &opts, cancel) else { return (out, false) };
    let mut paths: Vec<PathBuf> = matches.into_iter().map(|m| m.path).collect();
    paths.dedup();
    let mut total = 0usize;
    let mut truncated = false;
    for p in paths {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let items = scan_file(&p);
        if items.is_empty() {
            continue;
        }
        total = total.saturating_add(items.len());
        out.insert(p, items);
        if total >= MAX_ITEMS {
            truncated = true;
            break;
        }
    }
    (out, truncated)
}

/// Start a full scan of the open project (replacing a running one).
pub(crate) fn refresh(s: &mut Session) {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return };
    if s.todo.project.as_ref() != Some(&root) {
        s.todo = TodoState { project: Some(root.clone()), ..TodoState::default() };
    }
    if let Some(c) = s.todo.cancel.take() {
        c.store(true, Ordering::Relaxed);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded(1);
    let flag = Arc::clone(&cancel);
    let spawned = std::thread::Builder::new().name("codecraft-todo".into()).spawn(move || {
        let (files, truncated) = scan_project(&root, &flag);
        if !flag.load(Ordering::Relaxed) {
            let _ = tx.send(Msg::Full(files, truncated));
        }
    });
    s.todo.started = true;
    match spawned {
        Ok(_) => {
            s.todo.full = Some(rx);
            s.todo.cancel = Some(cancel);
            s.todo.scanning = true;
            s.todo.doc_revs.clear();
        }
        Err(e) => log::warn!("cannot start the TODO scan: {e}"),
    }
}

/// Rescan files the watcher reported (after a save, a git checkout, an agent's edit).
pub(crate) fn files_changed(s: &mut Session, paths: &[PathBuf]) {
    let Some(root) = s.todo.project.clone().filter(|_| s.todo.started) else { return };
    let paths: Vec<PathBuf> = paths
        .iter()
        .filter(|p| p.starts_with(&root) && !in_private_dir(&root, p))
        .filter(|p| {
            // Files the scan would see: already listed, deleted, or outside excluded directories
            // (new files reach the project tree only after the rescan the watcher triggers).
            let excluded = codecraft_fs::ScanOptions::default().exclude_dirs;
            s.todo.files.contains_key(*p)
                || !p.exists()
                || p.strip_prefix(&root).is_ok_and(|rel| {
                    !rel.components().any(|c| excluded.iter().any(|x| c.as_os_str() == x.as_str()))
                })
        })
        .cloned()
        .collect();
    if paths.is_empty() {
        return;
    }
    if paths.len() > MAX_RESCAN {
        refresh(s);
        return;
    }
    // Open documents are taken from memory instead.
    for p in &paths {
        s.todo.doc_revs.remove(p);
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-todo-files".into()).spawn(move || {
        let out: Vec<(PathBuf, Vec<TodoItem>)> = paths.into_iter().map(|p| {
            let items = scan_file(&p);
            (p, items)
        }).collect();
        let _ = tx.send(Msg::Files(out));
    });
    if spawned.is_ok() {
        s.todo.rescans.push(rx);
    }
}

fn set_file(st: &mut TodoState, path: PathBuf, items: Vec<TodoItem>) -> bool {
    if items.is_empty() {
        st.files.remove(&path).is_some()
    } else if st.files.get(&path) != Some(&items) {
        st.files.insert(path, items);
        true
    } else {
        false
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root != s.todo.project {
        // A different project (or none): drop the old results.
        if let Some(c) = s.todo.cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        s.todo = TodoState { project: root.clone(), ..TodoState::default() };
        changed = true;
    }
    if root.is_some() && !s.todo.started && s.ws.layout.is_visible(ToolWindowId::Todo) {
        // First time the tool window is shown for this project.
        refresh(s);
        changed = true;
    }
    if let Some(rx) = &s.todo.full {
        match rx.try_recv() {
            Ok(Msg::Full(files, truncated)) => {
                s.todo.files = files;
                s.todo.truncated = truncated;
                s.todo.scanning = false;
                s.todo.full = None;
                s.todo.doc_revs.clear();
                changed = true;
            }
            Ok(Msg::Files(_)) | Err(TryRecvError::Disconnected) => {
                s.todo.scanning = false;
                s.todo.full = None;
                changed = true;
            }
            Err(TryRecvError::Empty) => {}
        }
    }
    let mut i = 0;
    while let Some(rx) = s.todo.rescans.get(i) {
        match rx.try_recv() {
            Ok(Msg::Files(list)) => {
                for (p, items) in list {
                    if s.ws.doc_by_path(&p).is_none() {
                        changed |= set_file(&mut s.todo, p, items);
                    }
                }
                s.todo.rescans.remove(i);
            }
            Ok(Msg::Full(..)) | Err(TryRecvError::Disconnected) => {
                s.todo.rescans.remove(i);
            }
            Err(TryRecvError::Empty) => i += 1,
        }
    }
    // Open documents (unsaved edits included) while the tool window is shown.
    if let Some(root) = root.filter(|_| !s.todo.scanning && s.ws.layout.is_visible(ToolWindowId::Todo)) {
        let mut updates = Vec::new();
        for od in s.ws.docs() {
            let Some(p) = od.path.as_ref().filter(|p| p.starts_with(&root) && !in_private_dir(&root, p)) else {
                continue;
            };
            let rev = od.doc.revision();
            if s.todo.doc_revs.get(p) == Some(&rev) {
                continue;
            }
            updates.push((p.clone(), rev, scan_text(od.language, &od.doc.text())));
        }
        for (p, rev, items) in updates {
            s.todo.doc_revs.insert(p.clone(), rev);
            changed |= set_file(&mut s.todo, p, items);
        }
    }
    changed
}

fn list_json(s: &Session) -> Value {
    let files: Vec<Value> = s
        .todo
        .files
        .iter()
        .map(|(p, items)| json!({ "path": p, "items": items }))
        .collect();
    json!({ "scanning": s.todo.busy(), "truncated": s.todo.truncated, "count": s.todo.count(), "files": files })
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec("todo.refresh", "Refresh TODO", None, "Rescans the project", Context::Global, has_project, |s, _| {
            refresh(s);
            ok()
        }),
        spec(
            "todo.list",
            "List TODO Items",
            None,
            "Returns {scanning, truncated, count, files: [{path, items: [{line, column, kind, text}]}]}; starts a scan if none ran yet",
            Context::Global,
            always,
            |s, _| -> CmdResult {
                let current = s.ws.project.as_ref().map(|p| p.root.clone());
                if current.is_some() && (!s.todo.started || s.todo.project != current) {
                    refresh(s);
                }
                Ok(list_json(s))
            },
        ),
        spec("todo.open", "Go to TODO Item", None, "{path, line, column?}", Context::Global, always, |s, p| {
            let path = req_path(p, "path")?;
            let line = req_usize(p, "line")?;
            let column = opt_usize(p, "column")?.unwrap_or(0);
            s.navigate_to_position(&path, line, column)?;
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_todos_in_comments_only() {
        let src = "// TODO: first\nlet s = \"todo not this\"; // fixme later */\n/* Todo: block */\nfn todo() {}\n/// TODO doc\n";
        let items = scan_text(Language::Rust, src);
        let got: Vec<(usize, usize, &str)> = items.iter().map(|i| (i.line, i.column, i.text.as_str())).collect();
        assert_eq!(
            got,
            vec![(0, 3, "TODO: first"), (1, 28, "fixme later"), (2, 3, "Todo: block"), (4, 4, "TODO doc")]
        );
        assert_eq!(items[1].kind, TodoKind::Fixme);
        // Python and shell comments; words containing the keyword do not count.
        let py = scan_text(Language::Python, "x = 1  # TODO(me): fix\ntodos = 2\n# mastodon\n");
        assert_eq!(py.len(), 1);
        assert_eq!(py[0].text, "TODO(me): fix");
        // Plain text matches anywhere.
        assert_eq!(scan_text(Language::PlainText, "todo: buy milk").len(), 1);
    }

    #[test]
    fn hostile_text_does_not_panic() {
        let weird = "// TODO é\u{0}\r\n/* unterminated TODO\n\u{feff}FIXME 🦀🦀 ".repeat(50);
        for lang in [Language::Rust, Language::Python, Language::Markdown, Language::Json, Language::PlainText] {
            let _ = scan_text(lang, &weird);
        }
        let long = format!("// TODO {}", "x".repeat(10_000));
        assert!(scan_text(Language::Rust, &long)[0].text.chars().count() <= MAX_TEXT);
    }

    #[test]
    fn project_scan_respects_ignores_and_private_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".codecraft/local-history")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "// TODO one\nfn a() {}\n").unwrap();
        std::fs::write(root.join("src/x.py"), "# FIXME two\n").unwrap();
        std::fs::write(root.join("target/gen.rs"), "// TODO ignored\n").unwrap();
        std::fs::write(root.join(".codecraft/local-history/b.txt"), "TODO ignored\n").unwrap();
        let (files, truncated) = scan_project(&root, &AtomicBool::new(false));
        assert!(!truncated);
        let keys: Vec<_> = files.keys().map(|p| p.strip_prefix(&root).unwrap().to_path_buf()).collect();
        assert_eq!(keys, vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/x.py")]);
    }

    #[test]
    fn session_scans_updates_and_navigates() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n// TODO first\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.execute("todo.list", &json!({})).unwrap();
        let wait = |s: &mut Session, f: &dyn Fn(&Session) -> bool| {
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(15) {
                s.pump();
                if f(s) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        assert!(wait(&mut s, &|s| !s.todo.busy() && s.todo.count() == 1));
        // Unsaved edits show while the tool window is visible.
        s.ws.layout.show(ToolWindowId::Todo);
        s.execute("todo.open", &json!({ "path": root.join("a.rs"), "line": 1, "column": 3 })).unwrap();
        s.execute("editor.type", &json!({ "text": "FIXME second " })).unwrap();
        assert!(wait(&mut s, &|s| s.todo.count() == 1
            && s.todo.files.values().next().is_some_and(|v| v[0].kind == TodoKind::Fixme)));
        // A file changed on disk is rescanned.
        std::fs::write(root.join("b.py"), "# TODO py\n").unwrap();
        files_changed(&mut s, &[root.join("b.py")]);
        assert!(wait(&mut s, &|s| s.todo.files.contains_key(&root.join("b.py"))));
        std::fs::remove_file(root.join("b.py")).unwrap();
        files_changed(&mut s, &[root.join("b.py")]);
        assert!(wait(&mut s, &|s| !s.todo.files.contains_key(&root.join("b.py"))));
        let v = s.execute("todo.list", &json!({})).unwrap();
        assert_eq!(v["count"], 1);
        assert!(s.execute("todo.open", &json!({ "path": root.join("missing.rs"), "line": 0 })).is_err());
    }
}
