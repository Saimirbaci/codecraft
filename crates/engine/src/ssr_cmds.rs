//! Edit › Find › Search Structurally… / Replace Structurally…: the dialog, saved templates, the
//! search worker and applying replacements (see [`crate::ssr`] for the template language).
//!
//! A search runs on a worker thread over the scope's files of the template's language (open
//! documents use their editor text) and streams results into the Find tool window. A Replace
//! search computes each match's replacement; the preview lists them (occurrences can be
//! excluded) and Replace All applies them as one undoable edit per file, refusing files that
//! changed since the search.

use std::collections::{BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_fs::FileMatch;
use codecraft_syntax::Language;
use codecraft_text::{Change, ChangeSet, EditGroup, SelectionSet};
use codecraft_workspace::{FindResults, NotificationLevel, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::inspections::ScopeKind;
use crate::params::{bool_or, opt_path, opt_str, opt_usize, req_str};
use crate::ssr::{self, Constraint, SavedTemplate};
use crate::user_store::{Store, UserHome};
use crate::{CommandError, Session};

/// Per-user saved templates.
pub const TEMPLATES_FILE: &str = "structural-search.json";
/// Files larger than this are skipped (chars).
const MAX_FILE_CHARS: usize = 4 << 20;
/// Files searched per run.
const MAX_FILES: usize = 20_000;
/// Matches kept per run.
pub const MAX_HITS: usize = 20_000;
/// Matched text kept per hit for display.
const MAX_SHOWN_CHARS: usize = 400;

// ---------------------------------------------------------------- model

/// The Search/Replace Structurally dialog (the UI edits the draft fields directly).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsrDialog {
    /// Replace Structurally (shows the replacement template).
    pub replace_mode: bool,
    pub search: String,
    pub replace: String,
    /// Language id (see [`ssr::LANGUAGES`]).
    pub language: String,
    /// `project`, `file`, `directory`, `openFiles` or `uncommitted`.
    pub scope: String,
    pub path: Option<PathBuf>,
    pub constraints: Vec<Constraint>,
    /// Name to save the template under.
    pub template_name: String,
    pub error: Option<String>,
}

/// One match (with its replacement in a Replace search).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsrHit {
    pub id: usize,
    /// Char range in the file's text.
    pub start: usize,
    pub end: usize,
    /// Zero-based line and char column of the start, and of the end.
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    /// The first line of the match.
    pub line_text: String,
    /// The matched text (capped).
    pub matched: String,
    pub replacement: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsrFile {
    pub path: PathBuf,
    /// Hash of the text that was searched (Replace refuses files that changed since).
    pub text_hash: u64,
    pub hits: Vec<SsrHit>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsrResults {
    pub search: String,
    pub replace: Option<String>,
    pub language: String,
    pub scope_label: String,
    pub files: Vec<SsrFile>,
    pub files_searched: usize,
    pub truncated: bool,
    pub done: bool,
    /// Hits left out of Replace All.
    pub excluded: BTreeSet<usize>,
}

impl SsrResults {
    pub fn hit_count(&self) -> usize {
        self.files.iter().map(|f| f.hits.len()).sum()
    }
}

enum Msg {
    File(SsrFile),
    Done { searched: usize, truncated: bool },
}

pub struct SsrRun {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Msg>,
}

pub struct SsrState {
    pub dialog: Option<SsrDialog>,
    /// The replacement preview window is open.
    pub preview: bool,
    pub results: Option<SsrResults>,
    pub running: Option<SsrRun>,
    pub templates: Store<SavedTemplate>,
    pub home: UserHome,
    next_hit: usize,
}

impl Default for SsrState {
    fn default() -> Self {
        Self {
            dialog: None,
            preview: false,
            results: None,
            running: None,
            templates: Store::new(TEMPLATES_FILE),
            home: UserHome::default(),
            next_hit: 0,
        }
    }
}

impl SsrState {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// Saved templates first, then the built-in ones not shadowed by a saved one.
    pub fn all_templates(&mut self) -> Vec<(SavedTemplate, bool)> {
        self.templates.ensure(&self.home);
        let mut out: Vec<(SavedTemplate, bool)> = self.templates.items.iter().cloned().map(|t| (t, false)).collect();
        for b in ssr::builtin_templates() {
            if !out.iter().any(|(t, _)| t.name == b.name) {
                out.push((b, true));
            }
        }
        out
    }

    /// Point the saved templates at another home directory (tests).
    pub fn set_home(&mut self, dir: PathBuf) {
        self.home.dir_override = Some(dir);
        self.templates.reset();
    }
}

// ---------------------------------------------------------------- commands

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "edit.find.searchStructurally",
            "Search Structurally…",
            None,
            "{search?: string, language?: string, scope?: \"project\"|\"file\"|\"directory\"|\"openFiles\"|\"uncommitted\", path?: string, constraints?: [{name, regex?, invert?, min?, max?, unbounded?}], sync?: bool} — search with a `$var$` template into the Find tool window. Without `search` opens the dialog (prefilled with the selection).",
            Context::Global,
            always,
            |s, p| run_or_dialog(s, p, false),
        ),
        spec(
            "edit.find.replaceStructurally",
            "Replace Structurally…",
            None,
            "{search?: string, replace?: string, language?, scope?, path?, constraints?, sync?: bool, apply?: bool} — find matches and compute replacements (`$var$` substituted) for the preview; `apply` (with `sync`) replaces at once. Without `search` opens the dialog.",
            Context::Global,
            always,
            |s, p| run_or_dialog(s, p, true),
        ),
        spec("ssr.dialog.run", "Find", None, "Run the dialog's search (or replace preview)", Context::Global, has_dialog, |s, _| {
            let d = s.ssr.dialog.clone().ok_or_else(|| CommandError::Failed("no dialog".into()))?;
            let params = json!({
                "search": d.search,
                "replace": if d.replace_mode { Value::String(d.replace.clone()) } else { Value::Null },
                "language": d.language,
                "scope": d.scope,
                "path": d.path,
                "constraints": d.constraints,
            });
            match start(s, &params, d.replace_mode) {
                Ok(v) => {
                    s.ssr.dialog = None;
                    Ok(v)
                }
                Err(e) => {
                    if let Some(d) = &mut s.ssr.dialog {
                        d.error = Some(e.to_string());
                    }
                    Err(e)
                }
            }
        }),
        spec("ssr.dialog.close", "Close", None, "", Context::Global, always, |s, _| {
            s.ssr.dialog = None;
            ok()
        }),
        spec(
            "ssr.dialog.load",
            "Load Template",
            None,
            "{name: string} — put a saved or built-in template into the dialog",
            Context::Global,
            has_dialog,
            |s, p| {
                let name = req_str(p, "name")?;
                let (t, _) = s
                    .ssr
                    .all_templates()
                    .into_iter()
                    .find(|(t, _)| t.name == name)
                    .ok_or_else(|| CommandError::BadParams(format!("no template named '{name}'")))?;
                if let Some(d) = &mut s.ssr.dialog {
                    d.search = t.search.clone();
                    if let Some(r) = &t.replace {
                        d.replace = r.clone();
                    }
                    if ssr::parse_language(&t.language).is_some() {
                        d.language = t.language.clone();
                    }
                    d.constraints = t.constraints.clone();
                    d.template_name = t.name.clone();
                    d.error = None;
                }
                ok()
            },
        ),
        spec(
            "ssr.dialog.constraint",
            "Edit Variable Constraint",
            None,
            "{name: string, regex?: string, invert?: bool, min?: int, max?: int, unbounded?: bool} — set a variable's constraint in the dialog",
            Context::Global,
            has_dialog,
            |s, p| {
                let c = constraint_from(p)?;
                if let Some(d) = &mut s.ssr.dialog {
                    d.constraints.retain(|x| x.name != c.name);
                    d.constraints.push(c);
                }
                ok()
            },
        ),
        spec(
            "ssr.templates.list",
            "Structural Search Templates",
            None,
            "→ [{name, language, search, replace, constraints, builtin}]",
            Context::Global,
            always,
            |s, _| {
                let list: Vec<Value> = s
                    .ssr
                    .all_templates()
                    .into_iter()
                    .map(|(t, builtin)| {
                        let mut v = serde_json::to_value(&t).unwrap_or(Value::Null);
                        v["builtin"] = Value::Bool(builtin);
                        v
                    })
                    .collect();
                Ok(Value::Array(list))
            },
        ),
        spec(
            "ssr.templates.save",
            "Save Template",
            None,
            "{name?: string, search?: string, replace?: string, language?: string, constraints?: [...]} — save a template (default: the dialog's) under `name`",
            Context::Global,
            always,
            save_template,
        ),
        spec("ssr.templates.delete", "Delete Template", None, "{name: string}", Context::Global, always, |s, p| {
            let name = req_str(p, "name")?.to_string();
            s.ssr.templates.ensure(&s.ssr.home);
            let before = s.ssr.templates.items.len();
            s.ssr.templates.items.retain(|t| t.name != name);
            if s.ssr.templates.items.len() == before {
                return Err(CommandError::BadParams(format!("no saved template named '{name}'")));
            }
            let home = s.ssr.home.clone();
            s.ssr.templates.save(&home).map_err(CommandError::Failed)?;
            ok()
        }),
        spec("ssr.results", "Structural Search Results", None, "→ the last search: files, hits, replacements", Context::Global, always, |s, _| {
            Ok(json!({ "running": s.ssr.busy(), "results": s.ssr.results }))
        }),
        spec("ssr.cancel", "Stop Structural Search", None, "", Context::Global, always, |s, _| {
            cancel(s);
            ok()
        }),
        spec(
            "ssr.preview.exclude",
            "Exclude Occurrence",
            None,
            "{id: int, exclude?: bool} — leave an occurrence out of Replace All (or put it back)",
            Context::Global,
            has_results,
            |s, p| {
                let id = opt_usize(p, "id")?.ok_or_else(|| CommandError::BadParams("missing `id`".into()))?;
                let r = s.ssr.results.as_mut().ok_or_else(|| CommandError::Failed("no results".into()))?;
                if !r.files.iter().any(|f| f.hits.iter().any(|h| h.id == id)) {
                    return Err(CommandError::BadParams(format!("no occurrence {id}")));
                }
                let exclude = bool_or(p, "exclude", !r.excluded.contains(&id));
                if exclude {
                    r.excluded.insert(id);
                } else {
                    r.excluded.remove(&id);
                }
                ok()
            },
        ),
        spec("ssr.preview.close", "Close Preview", None, "", Context::Global, always, |s, _| {
            s.ssr.preview = false;
            ok()
        }),
        spec(
            "ssr.replace.apply",
            "Replace All",
            None,
            "Apply the previewed replacements (not the excluded ones): one undoable edit per file → {replaced, files, skipped}",
            Context::Global,
            has_replacements,
            |s, _| apply(s),
        ),
        spec(
            "ssr.open",
            "Open Occurrence",
            None,
            "{id: int} — open the file at an occurrence",
            Context::Global,
            has_results,
            |s, p| {
                let id = opt_usize(p, "id")?.ok_or_else(|| CommandError::BadParams("missing `id`".into()))?;
                let (path, start) = s
                    .ssr
                    .results
                    .as_ref()
                    .and_then(|r| r.files.iter().find_map(|f| f.hits.iter().find(|h| h.id == id).map(|h| (f.path.clone(), h.start))))
                    .ok_or_else(|| CommandError::BadParams(format!("no occurrence {id}")))?;
                s.navigate_to(&path, start)?;
                ok()
            },
        ),
    ]
}

fn has_dialog(s: &Session) -> Result<(), String> {
    s.ssr.dialog.as_ref().map(|_| ()).ok_or_else(|| "the structural search dialog is not open".into())
}

fn has_results(s: &Session) -> Result<(), String> {
    s.ssr.results.as_ref().map(|_| ()).ok_or_else(|| "no structural search results".into())
}

fn has_replacements(s: &Session) -> Result<(), String> {
    match &s.ssr.results {
        Some(r) if r.replace.is_some() && r.done && r.hit_count() > 0 => Ok(()),
        Some(r) if r.replace.is_some() && !r.done => Err("the search is still running".into()),
        _ => Err("no replacements to apply (run Replace Structurally first)".into()),
    }
}

fn constraint_from(p: &Value) -> Result<Constraint, CommandError> {
    let name = req_str(p, "name")?.trim().to_string();
    if name.is_empty() {
        return Err(CommandError::BadParams("`name` is empty".into()));
    }
    Ok(Constraint {
        name,
        regex: opt_str(p, "regex").unwrap_or_default().to_string(),
        invert: bool_or(p, "invert", false),
        min: opt_usize(p, "min")?,
        max: opt_usize(p, "max")?,
        unbounded: bool_or(p, "unbounded", false),
    })
}

fn constraints_param(p: &Value) -> Result<Vec<Constraint>, CommandError> {
    match p.get("constraints") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a.iter().map(constraint_from).collect(),
        Some(_) => Err(CommandError::BadParams("`constraints` must be an array".into())),
    }
}

/// The selection of the active editor (a template to start from), if it is on one line or short.
fn selection_text(s: &Session) -> Option<String> {
    let (e, d) = s.ws.active()?;
    let sel = e.selections.primary();
    if sel.is_empty() || sel.end() - sel.start() > 2000 {
        return None;
    }
    Some(d.doc.slice(sel.start()..sel.end()))
}

fn run_or_dialog(s: &mut Session, p: &Value, replace_mode: bool) -> CmdResult {
    if opt_str(p, "search").is_some() {
        let v = start(s, p, replace_mode)?;
        if replace_mode && bool_or(p, "apply", false) {
            if s.ssr.busy() {
                return Err(CommandError::BadParams("`apply` needs `sync: true`".into()));
            }
            return apply(s);
        }
        return Ok(v);
    }
    let language = s
        .ws
        .active()
        .map(|(_, d)| d.language)
        .filter(|l| ssr::LANGUAGES.iter().any(|(_, x)| x == l))
        .unwrap_or(Language::Rust);
    let previous = s.ssr.dialog.take();
    let search = selection_text(s).or_else(|| previous.as_ref().map(|d| d.search.clone())).unwrap_or_default();
    s.ssr.dialog = Some(SsrDialog {
        replace_mode,
        search,
        replace: previous.as_ref().map(|d| d.replace.clone()).unwrap_or_default(),
        language: ssr::language_id(language).to_string(),
        scope: if s.ws.project.is_some() { "project".into() } else { "file".into() },
        path: None,
        constraints: previous.map(|d| d.constraints).unwrap_or_default(),
        template_name: String::new(),
        error: None,
    });
    Ok(json!({ "dialog": true }))
}

fn save_template(s: &mut Session, p: &Value) -> CmdResult {
    let d = s.ssr.dialog.clone();
    let name = opt_str(p, "name")
        .map(str::to_string)
        .or_else(|| d.as_ref().map(|d| d.template_name.clone()))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| CommandError::BadParams("the template needs a name".into()))?;
    let search = opt_str(p, "search").map(str::to_string).or_else(|| d.as_ref().map(|d| d.search.clone())).unwrap_or_default();
    let language = opt_str(p, "language")
        .map(str::to_string)
        .or_else(|| d.as_ref().map(|d| d.language.clone()))
        .unwrap_or_else(|| "rust".into());
    let lang = ssr::parse_language(&language).ok_or_else(|| CommandError::BadParams(format!("unknown language '{language}'")))?;
    let replace = match opt_str(p, "replace") {
        Some(r) => Some(r.to_string()),
        None => d.as_ref().filter(|d| d.replace_mode && !d.replace.is_empty()).map(|d| d.replace.clone()),
    };
    let constraints = if p.get("constraints").is_some() {
        constraints_param(p)?
    } else {
        d.as_ref().map(|d| d.constraints.clone()).unwrap_or_default()
    };
    ssr::compile(lang, &search, &constraints).map_err(CommandError::BadParams)?;
    if let Some(r) = &replace {
        ssr::check_replacement(&search, r).map_err(CommandError::BadParams)?;
    }
    let t = SavedTemplate { name: name.clone(), language: ssr::language_id(lang).into(), search, replace, constraints };
    let home = s.ssr.home.clone();
    s.ssr.templates.ensure(&home);
    s.ssr.templates.items.retain(|x| x.name != name);
    s.ssr.templates.items.push(t);
    let path = s.ssr.templates.save(&home).map_err(CommandError::Failed)?;
    if let Some(d) = &mut s.ssr.dialog {
        d.template_name = name.clone();
    }
    s.status = format!("Saved structural search template '{name}'");
    Ok(json!({ "path": path }))
}

// ---------------------------------------------------------------- running a search

struct Job {
    pattern: ssr::Pattern,
    replace: Option<String>,
    files: Vec<(PathBuf, Option<String>)>,
}

fn files_for_scope(s: &Session, scope: &str, path: Option<PathBuf>, language: Language) -> Result<(String, Vec<PathBuf>), CommandError> {
    let open: Vec<(PathBuf, Language)> = s.ws.docs().filter_map(|d| d.path.clone().map(|p| (p, d.language))).collect();
    let lang_of = |p: &Path| open.iter().find(|(q, _)| q == p).map(|(_, l)| *l).unwrap_or_else(|| Language::from_path(p));
    let (label, files) = match scope {
        "openFiles" | "open" | "open_files" => {
            let files: Vec<PathBuf> = open.iter().map(|(p, _)| p.clone()).collect();
            ("Open files".to_string(), files)
        }
        other => {
            let kind = ScopeKind::parse(other)
                .ok_or_else(|| CommandError::BadParams(format!("unknown scope `{other}` (project, file, directory, openFiles, uncommitted)")))?;
            let r = crate::inspections::resolve_scope(s, kind, path)?;
            if kind == ScopeKind::File {
                return Ok((r.label, r.files));
            }
            (r.label, r.files)
        }
    };
    Ok((label, files.into_iter().filter(|p| lang_of(p) == language).take(MAX_FILES).collect()))
}

/// Start a search (or a replace preview) from params.
fn start(s: &mut Session, p: &Value, replace_mode: bool) -> CmdResult {
    let search = req_str(p, "search")?.to_string();
    let language = match opt_str(p, "language").filter(|l| !l.trim().is_empty()) {
        Some(l) => ssr::parse_language(l).ok_or_else(|| CommandError::BadParams(format!("unknown language '{l}'")))?,
        None => s.ws.active().map(|(_, d)| d.language).unwrap_or(Language::Rust),
    };
    let constraints = constraints_param(p)?;
    let pattern = ssr::compile(language, &search, &constraints).map_err(CommandError::BadParams)?;
    let replace = if replace_mode {
        let r = opt_str(p, "replace").unwrap_or_default().to_string();
        ssr::check_replacement(&search, &r).map_err(CommandError::BadParams)?;
        Some(r)
    } else {
        None
    };
    let default_scope = if s.ws.project.is_some() { "project" } else { "file" };
    let scope = opt_str(p, "scope").filter(|x| !x.is_empty()).unwrap_or(default_scope).to_string();
    let (label, paths) = files_for_scope(s, &scope, opt_path(p, "path"), language)?;
    let files: Vec<(PathBuf, Option<String>)> = paths
        .into_iter()
        .map(|path| {
            let text = s.ws.doc_by_path(&path).and_then(|d| s.ws.doc(d)).map(|d| d.doc.text());
            (path, text)
        })
        .collect();
    cancel(s);
    s.ssr.next_hit = 0;
    s.ssr.results = Some(SsrResults {
        search: search.clone(),
        replace: replace.clone(),
        language: ssr::language_id(language).into(),
        scope_label: label,
        ..SsrResults::default()
    });
    let title = format!("{} '{}'", if replace_mode { "Replace Structurally" } else { "Search Structurally" }, one_line(&search));
    s.ws.find = Some(FindResults { title, searching: true, ..FindResults::default() });
    s.ws.layout.show(ToolWindowId::Find);
    let job = Job { pattern, replace, files };
    let count = job.files.len();
    if bool_or(p, "sync", false) {
        let flag = AtomicBool::new(false);
        let mut msgs = Vec::new();
        run_job(&job, &flag, &mut |m| msgs.push(m));
        for m in msgs {
            receive(s, m);
        }
    } else {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let flag = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("codecraft-ssr".into())
            .spawn(move || {
                run_job(&job, &flag, &mut |m| {
                    let _ = tx.send(m);
                });
            })
            .map_err(|e| CommandError::Failed(format!("cannot start the search: {e}")))?;
        s.ssr.running = Some(SsrRun { cancel, rx });
        s.status = "Searching structurally…".into();
    }
    if replace_mode {
        s.ssr.preview = true;
    }
    Ok(json!({ "files": count, "started": true, "hits": s.ssr.results.as_ref().map(SsrResults::hit_count) }))
}

fn one_line(t: &str) -> String {
    let line: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 60 { format!("{}…", line.chars().take(60).collect::<String>()) } else { line }
}

pub(crate) fn text_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn run_job(job: &Job, cancel: &AtomicBool, send: &mut dyn FnMut(Msg)) {
    let mut searched = 0usize;
    let mut truncated = false;
    let mut hits = 0usize;
    for (path, open_text) in &job.files {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let text = match open_text {
            Some(t) => t.clone(),
            None => match codecraft_fs::read_text_file(path) {
                Ok(t) => codecraft_text::Document::from_text(&t).text(),
                Err(_) => continue,
            },
        };
        if text.len() > MAX_FILE_CHARS.saturating_mul(4) || text.chars().count() > MAX_FILE_CHARS {
            continue;
        }
        searched += 1;
        let found = ssr::find(&job.pattern, &text, job.replace.is_none());
        truncated |= found.truncated;
        if found.matches.is_empty() {
            continue;
        }
        let chars: Vec<char> = text.chars().collect();
        let doc = codecraft_text::Document::from_text(&text);
        let mut file = SsrFile { path: path.clone(), text_hash: text_hash(&text), hits: Vec::new() };
        for m in &found.matches {
            if hits >= MAX_HITS {
                truncated = true;
                break;
            }
            hits += 1;
            let a = doc.offset_to_position(m.start);
            let b = doc.offset_to_position(m.end);
            let matched: String = chars.get(m.start..m.end).map(|c| c.iter().take(MAX_SHOWN_CHARS).collect()).unwrap_or_default();
            file.hits.push(SsrHit {
                id: 0,
                start: m.start,
                end: m.end,
                line: a.line,
                column: a.column,
                end_line: b.line,
                end_column: b.column,
                line_text: doc.line_text(a.line).unwrap_or_default().chars().take(500).collect(),
                matched,
                replacement: job.replace.as_ref().map(|r| ssr::replacement(r, m, &chars)),
            });
        }
        send(Msg::File(file));
        if hits >= MAX_HITS {
            break;
        }
    }
    send(Msg::Done { searched, truncated });
}

fn receive(s: &mut Session, m: Msg) {
    match m {
        Msg::File(mut f) => {
            for h in &mut f.hits {
                h.id = s.ssr.next_hit;
                s.ssr.next_hit += 1;
            }
            if let Some(find) = &mut s.ws.find {
                for h in &f.hits {
                    let end_column = if h.end_line == h.line { h.end_column } else { h.line_text.chars().count() };
                    find.matches.push(FileMatch {
                        path: f.path.clone(),
                        line: h.line,
                        column: h.column,
                        end_column: end_column.max(h.column),
                        line_text: h.line_text.clone(),
                    });
                }
            }
            if let Some(r) = &mut s.ssr.results {
                r.files.push(f);
            }
        }
        Msg::Done { searched, truncated } => {
            if let Some(find) = &mut s.ws.find {
                find.searching = false;
            }
            let Some(r) = &mut s.ssr.results else { return };
            r.files_searched = searched;
            r.truncated = truncated;
            r.done = true;
            let n = r.hit_count();
            let files = r.files.len();
            s.status = format!(
                "Structural search: {n} match{} in {files} file(s){}",
                if n == 1 { "" } else { "es" },
                if truncated { " (truncated)" } else { "" }
            );
        }
    }
}

pub(crate) fn cancel(s: &mut Session) {
    if let Some(r) = s.ssr.running.take() {
        r.cancel.store(true, Ordering::Relaxed);
        if let Some(find) = &mut s.ws.find {
            find.searching = false;
        }
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(run) = &s.ssr.running else { return false };
    let mut msgs = Vec::new();
    let mut finished = false;
    loop {
        match run.rx.try_recv() {
            Ok(m) => {
                if matches!(m, Msg::Done { .. }) {
                    finished = true;
                }
                msgs.push(m);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                finished = true;
                break;
            }
        }
        if msgs.len() > 2000 {
            break;
        }
    }
    let changed = !msgs.is_empty() || finished;
    for m in msgs {
        receive(s, m);
    }
    if finished {
        s.ssr.running = None;
        if let Some(r) = &mut s.ssr.results {
            r.done = true;
        }
        if let Some(find) = &mut s.ws.find {
            find.searching = false;
        }
    }
    changed
}

pub(crate) fn shutdown(s: &mut Session) {
    cancel(s);
}

// ---------------------------------------------------------------- applying replacements

fn apply(s: &mut Session) -> CmdResult {
    let results = s.ssr.results.clone().ok_or_else(|| CommandError::Failed("no structural search results".into()))?;
    if results.replace.is_none() {
        return Err(CommandError::Failed("the last search has no replacement".into()));
    }
    let mut replaced = 0usize;
    let mut files_changed = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for f in &results.files {
        if !seen.insert(f.path.clone()) {
            continue;
        }
        let hits: Vec<&SsrHit> = f.hits.iter().filter(|h| !results.excluded.contains(&h.id) && h.replacement.is_some()).collect();
        if hits.is_empty() {
            continue;
        }
        let doc_id = match s.ws.doc_by_path(&f.path) {
            Some(d) => d,
            None => {
                let Ok(text) = codecraft_fs::read_text_file(&f.path) else {
                    skipped.push(format!("{} cannot be read", f.path.display()));
                    continue;
                };
                let id = s.ws.add_document(Some(f.path.clone()), &text);
                if let Some(od) = s.ws.doc_mut(id) {
                    od.language = Language::from_path(&f.path);
                }
                s.lsp_did_open(id);
                id
            }
        };
        let Some(od) = s.ws.doc(doc_id) else { continue };
        if od.read_only {
            skipped.push(format!("{} is read-only", f.path.display()));
            continue;
        }
        if text_hash(&od.doc.text()) != f.text_hash {
            skipped.push(format!("{} changed since the search", f.path.display()));
            continue;
        }
        let len = od.doc.len_chars();
        let mut changes: Vec<Change> = Vec::new();
        let mut last_end = 0usize;
        for h in hits {
            if h.start < last_end || h.end > len || h.start > h.end {
                continue;
            }
            changes.push(Change::new(h.start, h.end, h.replacement.clone().unwrap_or_default()));
            last_end = h.end;
        }
        let n = changes.len();
        let Ok(set) = ChangeSet::new(changes) else {
            skipped.push(format!("{}: overlapping occurrences", f.path.display()));
            continue;
        };
        let editor = s.ws.editor_for_doc(doc_id);
        let before = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_else(|| SelectionSet::caret(0));
        let after = before.map(&set);
        let edit = codecraft_text::Edit { changes: set, selections: after.clone() };
        let Some(od) = s.ws.doc_mut(doc_id) else { continue };
        if let Err(e) = od.doc.apply(&edit, &before, EditGroup::Standalone) {
            skipped.push(format!("{}: {e}", f.path.display()));
            continue;
        }
        if let Some(e) = editor {
            s.ws.set_selections(e, after);
        }
        replaced += n;
        files_changed += 1;
    }
    let _ = s.save_all();
    s.ssr.results = None;
    s.ssr.preview = false;
    if let Some(find) = &mut s.ws.find {
        find.matches.clear();
        find.title = format!("Replaced {replaced} occurrence(s)");
    }
    s.status = format!("Replace Structurally: {replaced} occurrence(s) in {files_changed} file(s)");
    if !skipped.is_empty() {
        s.notify(NotificationLevel::Warning, "Replace Structurally", skipped.join("\n"));
    }
    Ok(json!({ "replaced": replaced, "files": files_changed, "skipped": skipped }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("p");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&tmp.path().join("home"));
        s.execute("file.open", &json!({ "path": root })).unwrap();
        let start = std::time::Instant::now();
        while s.ws.project.as_ref().is_none_or(|p| p.tree.is_empty()) && start.elapsed().as_secs() < 30 {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        (tmp, s)
    }

    #[test]
    fn search_streams_into_the_find_window_and_replace_applies_per_file() {
        let (tmp, mut s) = project(&[
            ("src/a.rs", "fn a() {\n    let x = foo().unwrap();\n    bar.unwrap();\n}\n"),
            ("src/b.rs", "fn b() { baz.unwrap() }\n"),
            ("src/c.py", "x.unwrap()\n"),
        ]);
        let root = s.ws.project.as_ref().unwrap().root.clone();
        s.execute("edit.find.searchStructurally", &json!({ "search": "$e$.unwrap()", "language": "rust" })).unwrap();
        let start = std::time::Instant::now();
        while s.ssr.busy() && start.elapsed().as_secs() < 30 {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let r = s.ssr.results.clone().unwrap();
        assert!(r.done);
        assert_eq!(r.hit_count(), 3, "{r:?}");
        assert_eq!(s.ws.find.as_ref().unwrap().matches.len(), 3);
        assert!(s.ws.layout.is_visible(ToolWindowId::Find));

        // Replace with a preview; exclude one occurrence.
        let v = s
            .execute(
                "edit.find.replaceStructurally",
                &json!({ "search": "$e$.unwrap()", "replace": "$e$.expect(\"x\")", "language": "rust", "sync": true }),
            )
            .unwrap();
        assert_eq!(v["hits"], 3);
        assert!(s.ssr.preview);
        let bar = s.ssr.results.as_ref().unwrap().files.iter().flat_map(|f| &f.hits).find(|h| h.matched == "bar.unwrap()").unwrap().id;
        s.execute("ssr.preview.exclude", &json!({ "id": bar })).unwrap();
        let v = s.execute("ssr.replace.apply", &json!({})).unwrap();
        assert_eq!(v["replaced"], 2);
        assert_eq!(v["files"], 2);
        let a = std::fs::read_to_string(root.join("src/a.rs")).unwrap();
        assert_eq!(a, "fn a() {\n    let x = foo().expect(\"x\");\n    bar.unwrap();\n}\n");
        assert_eq!(std::fs::read_to_string(root.join("src/b.rs")).unwrap(), "fn b() { baz.expect(\"x\") }\n");
        // One undo step per file.
        let d = s.ws.doc_by_path(&root.join("src/a.rs")).unwrap();
        s.ws.doc_mut(d).unwrap().doc.undo();
        assert!(s.ws.doc(d).unwrap().doc.text().contains("foo().unwrap()"));
        drop(tmp);
    }

    #[test]
    fn stale_files_are_skipped_and_templates_persist() {
        let (_tmp, mut s) = project(&[("src/a.rs", "fn a() { x.unwrap(); }\n")]);
        let root = s.ws.project.as_ref().unwrap().root.clone();
        s.execute(
            "edit.find.replaceStructurally",
            &json!({ "search": "$e$.unwrap()", "replace": "$e$?", "language": "rust", "sync": true }),
        )
        .unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() { y.unwrap(); }\n").unwrap();
        let v = s.execute("ssr.replace.apply", &json!({})).unwrap();
        assert_eq!(v["replaced"], 0);
        assert_eq!(v["skipped"].as_array().unwrap().len(), 1);

        // The dialog: open, load a built-in template, save it under a new name, reload.
        s.execute("edit.find.searchStructurally", &json!({})).unwrap();
        s.execute("ssr.dialog.load", &json!({ "name": "println! calls" })).unwrap();
        assert_eq!(s.ssr.dialog.as_ref().unwrap().search, "println!($args$)");
        s.execute("ssr.templates.save", &json!({ "name": "mine" })).unwrap();
        s.ssr.templates.reset();
        let list = s.execute("ssr.templates.list", &json!({})).unwrap();
        let mine = list.as_array().unwrap().iter().find(|t| t["name"] == "mine").unwrap();
        assert_eq!(mine["builtin"], false);
        assert_eq!(mine["constraints"][0]["unbounded"], true);
        s.execute("ssr.templates.delete", &json!({ "name": "mine" })).unwrap();
        assert!(s.execute("ssr.templates.delete", &json!({ "name": "mine" })).is_err());

        // Bad templates are reported in the dialog.
        s.ssr.dialog.as_mut().unwrap().search = "foo(".into();
        assert!(s.execute("ssr.dialog.run", &json!({})).is_err());
        assert!(s.ssr.dialog.as_ref().unwrap().error.is_some());
        assert!(s.execute("edit.find.replaceStructurally", &json!({ "search": "$a$", "replace": "$b$" })).is_err());
        assert!(s.execute("edit.find.searchStructurally", &json!({ "search": "x", "scope": "galaxy" })).is_err());
    }
}
