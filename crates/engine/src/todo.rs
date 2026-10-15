//! The TODO tool window: `TODO` and `FIXME` in comments across the project (RustRover's default
//! patterns `\btodo\b.*` and `\bfixme\b.*`, case-insensitive), grouped by file.
//!
//! The patterns are configurable (Settings › Editor › TODO, [`Settings::todo_patterns`]): each a
//! regular expression, case-sensitive or not. Named filters ([`Settings::todo_filters`]) show only
//! some patterns' items (`todo.setFilter`); changing the patterns rescans the project.
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
use codecraft_workspace::{Settings, TodoPattern, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::{opt_str, opt_usize, req_path, req_usize};
use crate::{CommandError, Session};

/// Most TODO patterns.
pub const MAX_PATTERNS: usize = 50;
/// Longest TODO pattern.
pub const MAX_PATTERN_LEN: usize = 500;
/// Compiled size limit of one pattern (a hostile regex cannot take the scan down).
const REGEX_SIZE_LIMIT: usize = 1 << 20;

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
    /// The pattern that matched (its text, as in Settings › Editor › TODO).
    #[serde(default)]
    pub pattern: String,
}

/// Compiled TODO patterns.
#[derive(Debug)]
pub struct Patterns {
    /// Any of them (prefilter and first match on a line); `None` matches nothing.
    any: Option<Regex>,
    each: Vec<(String, Regex)>,
}

fn build(p: &TodoPattern) -> Result<Regex, String> {
    regex::RegexBuilder::new(&p.pattern)
        .case_insensitive(!p.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| format!("pattern `{}`: {e}", p.pattern))
}

/// Validate TODO patterns: 1–[`MAX_PATTERNS`] unique, non-empty regular expressions that
/// compile and cannot match an empty string (they would match every line).
pub fn check_patterns(list: &[TodoPattern]) -> Result<Vec<TodoPattern>, String> {
    if list.len() > MAX_PATTERNS {
        return Err(format!("at most {MAX_PATTERNS} TODO patterns"));
    }
    let mut out: Vec<TodoPattern> = Vec::new();
    for p in list {
        let pattern = p.pattern.trim().to_string();
        if pattern.is_empty() || pattern.chars().count() > MAX_PATTERN_LEN {
            return Err(format!("a TODO pattern needs 1–{MAX_PATTERN_LEN} characters"));
        }
        if out.iter().any(|o| o.pattern == pattern) {
            return Err(format!("the TODO pattern `{pattern}` is listed twice"));
        }
        let p = TodoPattern { pattern, case_sensitive: p.case_sensitive };
        if build(&p)?.is_match("") {
            return Err(format!("the TODO pattern `{}` matches empty text", p.pattern));
        }
        out.push(p);
    }
    Ok(out)
}

/// Compile patterns (invalid ones are left out; with none left, nothing matches).
pub fn compile(list: &[TodoPattern]) -> Patterns {
    let mut each: Vec<(String, Regex)> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for p in list.iter().take(MAX_PATTERNS) {
        let Some(r) = build(p).ok().filter(|r| !r.is_match("")) else { continue };
        parts.push(format!("(?{}:{})", if p.case_sensitive { "-i" } else { "i" }, p.pattern));
        each.push((p.pattern.clone(), r));
    }
    let any = (!parts.is_empty())
        .then(|| regex::RegexBuilder::new(&parts.join("|")).size_limit(REGEX_SIZE_LIMIT * 4).build().ok())
        .flatten();
    Patterns { any, each }
}

impl Patterns {
    /// The pattern texts, in order.
    pub fn names(&self) -> Vec<&str> {
        self.each.iter().map(|(p, _)| p.as_str()).collect()
    }
}

/// The default patterns, compiled once.
fn default_patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| compile(&codecraft_workspace::default_todo_patterns()))
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
    /// The TODO filter shown (a [`Settings::todo_filters`] name); `None` = every pattern.
    pub filter: Option<String>,
    /// The project root the results belong to.
    project: Option<PathBuf>,
    /// A full scan was started for `project`.
    started: bool,
    full: Option<Receiver<Msg>>,
    rescans: Vec<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
    /// Revision of each open document the results were taken from.
    doc_revs: HashMap<PathBuf, u64>,
    /// The patterns the results were found with (and their source in the settings).
    patterns: Option<(Vec<TodoPattern>, Arc<Patterns>)>,
}

impl TodoState {
    pub fn count(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    pub fn busy(&self) -> bool {
        self.scanning || !self.rescans.is_empty()
    }

    /// The items the current filter shows, by file (files without any are left out).
    pub fn visible(&self, settings: &Settings) -> Vec<(&PathBuf, Vec<&TodoItem>)> {
        let allowed = self.filter_patterns(settings);
        self.files
            .iter()
            .filter_map(|(p, items)| {
                let shown: Vec<&TodoItem> =
                    items.iter().filter(|i| allowed.as_ref().is_none_or(|a| a.contains(&i.pattern))).collect();
                (!shown.is_empty()).then_some((p, shown))
            })
            .collect()
    }

    /// The patterns of the current filter (`None` = all).
    fn filter_patterns(&self, settings: &Settings) -> Option<Vec<String>> {
        let name = self.filter.as_ref()?;
        settings.todo_filters.iter().find(|f| f.name == *name).map(|f| f.patterns.clone())
    }
}

/// The compiled patterns of `settings` (cached in the state until the settings change).
fn patterns_for(st: &mut TodoState, settings: &Settings) -> Arc<Patterns> {
    match &st.patterns {
        Some((src, p)) if *src == settings.todo_patterns => Arc::clone(p),
        _ => {
            let p = Arc::new(compile(&settings.todo_patterns));
            st.patterns = Some((settings.todo_patterns.clone(), Arc::clone(&p)));
            p
        }
    }
}

/// TODO items of a text in `language` with the default patterns.
pub fn scan_text(language: Language, text: &str) -> Vec<TodoItem> {
    scan_text_with(language, text, default_patterns())
}

/// TODO items of a text in `language`: pattern matches inside comments (anywhere in plain text).
pub fn scan_text_with(language: Language, text: &str, pats: &Patterns) -> Vec<TodoItem> {
    let Some(re) = &pats.any else { return Vec::new() };
    if !re.is_match(text) {
        return Vec::new();
    }
    let anywhere = language == Language::PlainText;
    let hl = (!anywhere).then(|| codecraft_syntax::highlight(language, text));
    let mut out = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        if !re.is_match(line) {
            continue;
        }
        let in_comment = |start: usize| {
            anywhere
                || hl.as_ref().is_some_and(|h| {
                    let col = line.get(..start).map(|p| p.chars().count()).unwrap_or(0);
                    h.line(line_no).iter().any(|sp| {
                        sp.start <= col
                            && col < sp.end
                            && matches!(sp.kind, HighlightKind::Comment | HighlightKind::DocComment)
                    })
                })
        };
        // The leftmost match in a comment of any pattern (a later match on the line may be in a
        // comment even if the first is not: `s = "todo" // TODO`).
        let found = pats
            .each
            .iter()
            .filter_map(|(name, r)| r.find_iter(line).find(|m| in_comment(m.start())).map(|m| (m.start(), name)))
            .min_by_key(|(start, _)| *start);
        let Some((start, name)) = found else { continue };
        let column = line.get(..start).map(|p| p.chars().count()).unwrap_or(0);
        let rest = line.get(start..).unwrap_or("");
        let kind = if rest.get(..5).is_some_and(|k| k.eq_ignore_ascii_case("fixme")) {
            TodoKind::Fixme
        } else {
            TodoKind::Todo
        };
        out.push(TodoItem { line: line_no, column, kind, text: clean(rest), pattern: name.clone() });
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

fn scan_file(path: &Path, pats: &Patterns) -> Vec<TodoItem> {
    if std::fs::metadata(path).map(|m| !m.is_file() || m.len() > MAX_FILE_BYTES).unwrap_or(true) {
        return Vec::new();
    }
    match codecraft_fs::read_text_file(path) {
        Ok(text) => scan_text_with(Language::from_path(path), &text, pats),
        Err(_) => Vec::new(),
    }
}

fn in_private_dir(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| rel.starts_with(".codecraft"))
}

/// Scan the whole project with the default patterns (blocking).
pub fn scan_project(root: &Path, cancel: &AtomicBool) -> (BTreeMap<PathBuf, Vec<TodoItem>>, bool) {
    scan_project_with(root, default_patterns(), cancel)
}

/// Scan the whole project (blocking; runs on a worker thread).
pub fn scan_project_with(
    root: &Path,
    pats: &Patterns,
    cancel: &AtomicBool,
) -> (BTreeMap<PathBuf, Vec<TodoItem>>, bool) {
    let mut out = BTreeMap::new();
    let Some(re) = &pats.any else { return (out, false) };
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
        let items = scan_file(&p, pats);
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
        let filter = s.todo.filter.take();
        s.todo = TodoState { project: Some(root.clone()), filter, ..TodoState::default() };
    }
    if let Some(c) = s.todo.cancel.take() {
        c.store(true, Ordering::Relaxed);
    }
    let pats = patterns_for(&mut s.todo, &s.ws.settings);
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded(1);
    let flag = Arc::clone(&cancel);
    let spawned = std::thread::Builder::new().name("codecraft-todo".into()).spawn(move || {
        let (files, truncated) = scan_project_with(&root, &pats, &flag);
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
    let pats = patterns_for(&mut s.todo, &s.ws.settings);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-todo-files".into()).spawn(move || {
        let out: Vec<(PathBuf, Vec<TodoItem>)> = paths.into_iter().map(|p| {
            let items = scan_file(&p, &pats);
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
        let filter = s.todo.filter.take();
        s.todo = TodoState { project: root.clone(), filter, ..TodoState::default() };
        changed = true;
    }
    if root.is_some() && !s.todo.started && s.ws.layout.is_visible(ToolWindowId::Todo) {
        // First time the tool window is shown for this project.
        refresh(s);
        changed = true;
    }
    // Settings › Editor › TODO changed the patterns: scan again with the new ones.
    if s.todo.started && s.todo.patterns.as_ref().is_some_and(|(src, _)| *src != s.ws.settings.todo_patterns) {
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
    if let Some(root) = root.filter(|_| !s.todo.scanning && !s.power_save() && s.ws.layout.is_visible(ToolWindowId::Todo)) {
        let pats = patterns_for(&mut s.todo, &s.ws.settings);
        let mut updates = Vec::new();
        for od in s.ws.docs() {
            let Some(p) = od.path.as_ref().filter(|p| p.starts_with(&root) && !in_private_dir(&root, p)) else {
                continue;
            };
            let rev = od.doc.revision();
            if s.todo.doc_revs.get(p) == Some(&rev) {
                continue;
            }
            updates.push((p.clone(), rev, scan_text_with(od.language, &od.doc.text(), &pats)));
        }
        for (p, rev, items) in updates {
            s.todo.doc_revs.insert(p.clone(), rev);
            changed |= set_file(&mut s.todo, p, items);
        }
    }
    changed
}

fn list_json(s: &Session) -> Value {
    let visible = s.todo.visible(&s.ws.settings);
    let count: usize = visible.iter().map(|(_, items)| items.len()).sum();
    let files: Vec<Value> = visible.iter().map(|(p, items)| json!({ "path": p, "items": items })).collect();
    json!({
        "scanning": s.todo.busy(),
        "truncated": s.todo.truncated,
        "count": count,
        "total": s.todo.count(),
        "filter": s.todo.filter,
        "files": files,
    })
}

fn filters_json(s: &Session) -> Value {
    json!({
        "filter": s.todo.filter,
        "filters": s.ws.settings.todo_filters,
        "patterns": s.ws.settings.todo_patterns,
    })
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
            "Returns {scanning, truncated, count, total, filter, files: [{path, items: [{line, column, kind, text, pattern}]}]} (only the current filter's items; `total` counts all); starts a scan if none ran yet",
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
        spec(
            "todo.filters",
            "TODO Filters",
            None,
            "→ {filter, filters: [{name, patterns}], patterns: [{pattern, case_sensitive}]} (edit them with settings.set todo_patterns / todo_filters)",
            Context::Global,
            always,
            |s, _| Ok(filters_json(s)),
        ),
        spec(
            "todo.setFilter",
            "Filter TODO Items",
            None,
            "{name?: string} — show only the items of a TODO filter's patterns (no name: every pattern)",
            Context::Global,
            always,
            |s, p| {
                match opt_str(p, "name").map(str::trim).filter(|n| !n.is_empty()) {
                    Some(name) => {
                        let f = s
                            .ws
                            .settings
                            .todo_filters
                            .iter()
                            .find(|f| f.name.eq_ignore_ascii_case(name))
                            .ok_or_else(|| CommandError::BadParams(format!("no TODO filter `{name}`")))?;
                        s.todo.filter = Some(f.name.clone());
                    }
                    None => s.todo.filter = None,
                }
                Ok(filters_json(s))
            },
        ),
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

    fn pat(p: &str, case_sensitive: bool) -> TodoPattern {
        TodoPattern { pattern: p.into(), case_sensitive }
    }

    #[test]
    fn custom_patterns_and_case_sensitivity() {
        let pats = compile(&[pat(r"\bHACK\b.*", true), pat(r"\bnote\b.*", false), pat("(", false)]);
        assert_eq!(pats.names(), vec![r"\bHACK\b.*", r"\bnote\b.*"], "invalid patterns are left out");
        let src = "// HACK: one\n// hack: not (case-sensitive)\n// NOTE two\n// TODO not a pattern now\n";
        let items = scan_text_with(Language::Rust, src, &pats);
        let got: Vec<(usize, &str, &str)> = items.iter().map(|i| (i.line, i.text.as_str(), i.pattern.as_str())).collect();
        assert_eq!(got, vec![(0, "HACK: one", r"\bHACK\b.*"), (2, "NOTE two", r"\bnote\b.*")]);
        // No usable pattern: nothing matches.
        assert!(scan_text_with(Language::Rust, src, &compile(&[])).is_empty());
        // Validation.
        assert!(check_patterns(&[pat(r"\btodo\b.*", false)]).is_ok());
        for bad in [vec![pat("", false)], vec![pat("(", false)], vec![pat("a*", false)], vec![pat("x", false), pat("x", true)]] {
            assert!(check_patterns(&bad).is_err(), "{bad:?}");
        }
        let many: Vec<TodoPattern> = (0..=MAX_PATTERNS).map(|i| pat(&format!("x{i}"), false)).collect();
        assert!(check_patterns(&many).is_err());
    }

    #[test]
    fn filters_select_patterns_and_settings_changes_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.rs"), "// TODO one\n// FIXME two\n// HACK three\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.execute(
            "settings.set",
            &json!({ "key": "todo_filters", "value": [{ "name": "Only FIXME", "patterns": [r"\bfixme\b.*"] }] }),
        )
        .unwrap();
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
        assert!(wait(&mut s, &|s| !s.todo.busy() && s.todo.count() == 2));
        s.execute("todo.setFilter", &json!({ "name": "only fixme" })).unwrap();
        let v = s.execute("todo.list", &json!({})).unwrap();
        assert_eq!((v["count"].as_u64(), v["total"].as_u64()), (Some(1), Some(2)));
        assert_eq!(v["files"][0]["items"][0]["text"], "FIXME two");
        assert!(s.execute("todo.setFilter", &json!({ "name": "nope" })).is_err());
        // A new pattern rescans the project.
        let patterns = json!([
            { "pattern": r"\btodo\b.*", "case_sensitive": false },
            { "pattern": r"\bfixme\b.*", "case_sensitive": false },
            { "pattern": r"\bHACK\b.*", "case_sensitive": true },
        ]);
        s.execute("settings.set", &json!({ "key": "todo_patterns", "value": patterns })).unwrap();
        assert!(wait(&mut s, &|s| !s.todo.busy() && s.todo.count() == 3));
        s.execute("todo.setFilter", &json!({})).unwrap();
        assert_eq!(s.execute("todo.list", &json!({})).unwrap()["count"], 3);
        // Removing a pattern drops it from the filters that used it.
        s.execute("settings.set", &json!({ "key": "todo_patterns", "value": [{ "pattern": r"\btodo\b.*" }] })).unwrap();
        assert!(s.ws.settings.todo_filters[0].patterns.is_empty());
        assert!(
            s.execute("settings.set", &json!({ "key": "todo_patterns", "value": [{ "pattern": "(" }] })).is_err(),
            "bad regexes are rejected"
        );
    }
}
