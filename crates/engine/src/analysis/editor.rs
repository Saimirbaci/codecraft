//! Analysis findings in the editor (M15 track EA): the findings of Inspect Code and of the
//! analysis on save (`on_save.rs`) shown like language-server diagnostics.
//!
//! A per-file store keeps every finding with the text its positions refer to (a cheap snapshot
//! of the open document at a revision). Each pump maps the findings of edited documents through
//! the document's change log, so underlines move with the text; a finding whose own text is
//! edited is dropped (it comes back on the next save if it is still there). The store is
//! published to the workspace as [`ProblemSource::Analysis`] problems, so the editor's
//! underlines, error stripe, gutter marks, the inspection widget, the Problems tool window and
//! Next/Previous Highlighted Error (F2/Shift+F2) all include them. A finding that repeats a
//! language-server diagnostic (rust-analyzer's flycheck reports Clippy too) is kept for agents
//! but not shown twice. Advisories (cargo audit / cargo deny) are kept for agents only: they
//! have their own Cargo.toml highlighting.
//!
//! Alt+Enter offers the analyzer's fixes and its suppression for the finding at the caret
//! (`analysis.editorFix`, `analysis.editorSuppress`); agents read a file's findings with
//! `analysis.fileFindings`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use codecraft_text::{Assoc, ChangeSet, Document, Position};
use codecraft_workspace::{Problem, ProblemSource, Severity};

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::inspections::{Finding, Fix, Tool};
use crate::params::{opt_usize, req_usize};
use crate::popup::ActionItem;
use crate::{CommandError, Session};

use super::on_save;

/// The workspace's inspection key for these problems.
pub const INSPECTION_KEY: &str = "analysis";
/// Findings kept per file (a lint storm in one file cannot slow every frame).
pub const MAX_PER_FILE: usize = 2000;
/// Files whose language-server problems are compared each pump (open documents only).
const MAX_DEDUPE_FILES: usize = 64;

/// Where a finding came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Origin {
    /// An Inspect Code run: `id` is the problem id `analysis.fix` takes while the run's results
    /// are the current ones.
    Inspection { job: u64, id: usize },
    /// The analysis on save (`run` counts the runs of the session).
    OnSave { run: u64 },
}

/// One finding in a file.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorFinding {
    /// Stable for the session: `analysis.editorFix {key}`.
    pub key: u64,
    /// Positions are those of [`FileFindings`]'s base text (the current text after a pump).
    pub finding: Finding,
    pub origin: Origin,
    /// Shown in the editor (not a duplicate of a language-server diagnostic, not an advisory).
    pub shown: bool,
}

/// The findings of one file.
#[derive(Debug, Default)]
pub struct FileFindings {
    pub list: Vec<EditorFinding>,
    /// The document text positions refer to (revision, snapshot); `None` = the file on disk.
    base: Option<(u64, Document)>,
    /// Analyzers re-run on save since the last Inspect Code (their Inspect Code findings for the
    /// file are superseded).
    pub saved_tools: BTreeSet<Tool>,
}

/// The store (in [`super::AnalysisState`]).
#[derive(Debug, Default)]
pub struct EditorFindings {
    pub files: BTreeMap<PathBuf, FileFindings>,
    next_key: u64,
    /// Open documents when an Inspect Code job started: job → path → (revision, snapshot).
    starts: HashMap<u64, BTreeMap<PathBuf, (u64, Document)>>,
    /// The Inspect Code results last taken in: (job, resolved count).
    synced: Option<(u64, usize)>,
    /// Files whose workspace problems must be published again.
    dirty: BTreeSet<PathBuf>,
    /// Files published to the workspace (cleared when they have nothing left).
    published: BTreeSet<PathBuf>,
    /// A fingerprint of each open file's other problems (language server, build) at the last
    /// publish: a change re-runs the duplicate check.
    others: HashMap<PathBuf, u64>,
    /// Analysis on save.
    pub on_save: on_save::OnSaveState,
}

impl EditorFindings {
    /// A finding by key.
    pub fn get(&self, key: u64) -> Option<&EditorFinding> {
        self.files.values().flat_map(|f| f.list.iter()).find(|f| f.key == key)
    }

    /// Findings of a file (in its current text once pumped).
    pub fn of(&self, path: &Path) -> &[EditorFinding] {
        self.files.get(path).map(|f| f.list.as_slice()).unwrap_or(&[])
    }
}

// ---------------------------------------------------------------- mapping through edits

/// Whether a change set touches the char range `a..b` (a zero-width range is touched by a
/// replacement covering it; insertions at either end leave it alone).
fn touches(cs: &ChangeSet, a: usize, b: usize) -> bool {
    let end = b.max(a.saturating_add(1));
    cs.changes().iter().any(|c| {
        if c.start == c.end { c.start > a && c.start < b } else { c.start < end && c.end > a }
    })
}

/// Map a range of `base` through `sets` into `now`; `None` when an edit touched it.
fn map_range(base: &Document, now: &Document, sets: &[&ChangeSet], start: Position, end: Position) -> Option<(Position, Position)> {
    let mut a = base.position_to_offset(start);
    let mut b = base.position_to_offset(end).max(a);
    for cs in sets {
        if touches(cs, a, b) {
            return None;
        }
        let zero = a == b;
        a = cs.map_pos(a, Assoc::After);
        b = if zero { a } else { cs.map_pos(b, Assoc::Before).max(a) };
    }
    let len = now.len_chars();
    Some((now.offset_to_position(a.min(len)), now.offset_to_position(b.min(len))))
}

/// Move one finding (and the changes of its fixes in the same file); `false` drops it.
fn map_finding(f: &mut Finding, path: &Path, base: &Document, now: &Document, sets: &[&ChangeSet]) -> bool {
    let Some((start, end)) = map_range(base, now, sets, f.start, f.end) else { return false };
    f.start = start;
    f.end = end;
    f.fixes.retain_mut(|fix| {
        fix.changes.iter_mut().filter(|c| c.path == path).all(|c| match map_range(base, now, sets, c.start, c.end) {
            Some((s, e)) => {
                c.start = s;
                c.end = e;
                true
            }
            None => false,
        })
    });
    true
}

/// Bring a file's findings to the open document's current text. Returns whether they moved.
fn sync_file(path: &Path, ff: &mut FileFindings, doc: Option<&Document>) -> bool {
    let Some(doc) = doc else {
        // Closed: positions stay those of the last text seen (saved, or on disk).
        ff.base = None;
        return false;
    };
    let rev = doc.revision();
    let Some((base_rev, base)) = &ff.base else {
        ff.base = Some((rev, doc.snapshot()));
        return false;
    };
    if *base_rev == rev {
        return false;
    }
    match doc.changes_since(*base_rev) {
        Some(sets) => ff.list.retain_mut(|e| map_finding(&mut e.finding, path, base, doc, &sets)),
        // Too much happened since: the positions cannot be trusted.
        None => ff.list.clear(),
    }
    ff.base = Some((rev, doc.snapshot()));
    true
}

/// Map new findings whose positions refer to `at` (a snapshot taken when the file was saved or
/// an inspection started) into the current document.
fn map_new(path: &Path, findings: &mut Vec<Finding>, at: Option<&(u64, Document)>, doc: Option<&Document>) {
    let (Some((rev, base)), Some(doc)) = (at, doc) else { return };
    if *rev == doc.revision() {
        return;
    }
    match doc.changes_since(*rev) {
        Some(sets) => findings.retain_mut(|f| map_finding(f, path, base, doc, &sets)),
        None => findings.clear(),
    }
}

fn doc_in<'a>(ws: &'a codecraft_workspace::Workspace, path: &Path) -> Option<&'a Document> {
    ws.doc_by_path(path).and_then(|id| ws.doc(id)).map(|d| &d.doc)
}

fn doc_of<'a>(s: &'a Session, path: &Path) -> Option<&'a Document> {
    doc_in(&s.ws, path)
}

// ---------------------------------------------------------------- taking findings in

/// An Inspect Code job started: remember the open documents' texts its positions will refer to.
pub(crate) fn job_started(s: &mut Session, job: u64) {
    let snaps: BTreeMap<PathBuf, (u64, Document)> = s
        .ws
        .docs()
        .filter_map(|d| d.path.clone().map(|p| (p, (d.doc.revision(), d.doc.snapshot()))))
        .collect();
    let store = &mut s.analysis.editor;
    // Only the newest few jobs matter.
    if store.starts.len() > 8 {
        let oldest: Vec<u64> = store.starts.keys().copied().collect::<BTreeSet<u64>>().into_iter().take(4).collect();
        for j in oldest {
            store.starts.remove(&j);
        }
    }
    store.starts.insert(job, snaps);
}

/// An Inspect Code job ended: take its findings in.
pub(crate) fn job_ended(s: &mut Session, job: u64, cancelled: bool) {
    let snaps = s.analysis.editor.starts.remove(&job);
    if !cancelled {
        sync_results(s, snaps);
    }
}

/// Imported (offline) results describe someone else's files: not shown in the editor.
fn offline(r: &crate::inspections::InspectionResults) -> bool {
    r.scope_label.starts_with("Offline (")
}

/// Follow `Session::inspections.results`: a new run replaces the Inspect Code findings of its
/// scope; fixed or suppressed problems leave; closed results take their findings along.
fn sync_results(s: &mut Session, snaps: Option<BTreeMap<PathBuf, (u64, Document)>>) {
    let identity = s.inspections.results.as_ref().filter(|r| !offline(r)).map(|r| (r.job, r.resolved.len()));
    if identity == s.analysis.editor.synced && snaps.is_none() {
        return;
    }
    let previous = s.analysis.editor.synced;
    s.analysis.editor.synced = identity;
    let Some((job, _)) = identity else {
        // Closed (or replaced by offline results): Inspect Code findings go.
        drop_inspection_findings(s, |_| true);
        return;
    };
    if previous.is_some_and(|(j, _)| j == job) && snaps.is_none() {
        // Same run: drop what was fixed or suppressed since.
        let resolved: BTreeSet<usize> = s.inspections.results.as_ref().map(|r| r.resolved.clone()).unwrap_or_default();
        let store = &mut s.analysis.editor;
        for (path, ff) in store.files.iter_mut() {
            let before = ff.list.len();
            ff.list.retain(|e| !matches!(e.origin, Origin::Inspection { job: j, id } if j == job && resolved.contains(&id)));
            if ff.list.len() != before {
                store.dirty.insert(path.clone());
            }
        }
        return;
    }
    // A new run.
    let Some(results) = s.inspections.results.clone() else { return };
    let scope = results.scope.clone();
    let reported: BTreeSet<PathBuf> = results.findings.iter().map(|f| f.path.clone()).collect();
    let in_scope = |p: &Path| match (&scope.kind, &scope.path) {
        (crate::inspections::ScopeKind::Project, _) => true,
        (crate::inspections::ScopeKind::File, Some(f)) => p == f,
        (crate::inspections::ScopeKind::Directory, Some(d)) => p.starts_with(d),
        _ => reported.contains(p),
    };
    drop_inspection_findings(s, in_scope);
    let mut by_file: BTreeMap<PathBuf, Vec<Finding>> = BTreeMap::new();
    for f in results.open() {
        by_file.entry(f.path.clone()).or_default().push(f.clone());
    }
    for (path, mut found) in by_file {
        let at = snaps.as_ref().and_then(|m| m.get(&path));
        // Bring the file's existing findings to the current text, then map the new ones there.
        let doc = doc_in(&s.ws, &path);
        let store = &mut s.analysis.editor;
        let ff = store.files.entry(path.clone()).or_default();
        sync_file(&path, ff, doc);
        map_new(&path, &mut found, at, doc);
        // Without a snapshot from the start, the positions are those of the disk text.
        if ff.base.is_none() {
            ff.base = doc.map(|d| (d.revision(), d.snapshot()));
        }
        ff.saved_tools.clear();
        for f in found {
            if ff.list.len() >= MAX_PER_FILE {
                break;
            }
            let id = f.id;
            store.next_key = store.next_key.saturating_add(1);
            let key = store.next_key;
            ff.list.push(EditorFinding { key, finding: f, origin: Origin::Inspection { job, id }, shown: false });
        }
        sort(&mut ff.list);
        store.dirty.insert(path);
    }
    // Files of the scope without problems now are clean.
    for path in s.analysis.editor.files.keys().filter(|p| in_scope(p)).cloned().collect::<Vec<_>>() {
        if let Some(ff) = s.analysis.editor.files.get_mut(&path) {
            ff.saved_tools.clear();
        }
    }
}

fn drop_inspection_findings(s: &mut Session, which: impl Fn(&Path) -> bool) {
    let store = &mut s.analysis.editor;
    for (path, ff) in store.files.iter_mut() {
        if !which(path) {
            continue;
        }
        let before = ff.list.len();
        ff.list.retain(|e| !matches!(e.origin, Origin::Inspection { .. }));
        if ff.list.len() != before {
            store.dirty.insert(path.clone());
        }
    }
}

fn sort(list: &mut [EditorFinding]) {
    list.sort_by(|a, b| {
        (a.finding.start, a.finding.severity, a.key).cmp(&(b.finding.start, b.finding.severity, b.key))
    });
}

/// The analysis on save of `path` finished: its findings of `tools` replace the old ones.
/// `at` is the document's text when it was saved (`None`: the file was not open).
pub(crate) fn take_saved(
    s: &mut Session,
    path: &Path,
    tools: &BTreeSet<Tool>,
    mut found: Vec<Finding>,
    at: Option<&(u64, Document)>,
    run: u64,
) {
    let doc = doc_in(&s.ws, path);
    let store = &mut s.analysis.editor;
    let ff = store.files.entry(path.to_path_buf()).or_default();
    sync_file(path, ff, doc);
    map_new(path, &mut found, at, doc);
    if ff.base.is_none() {
        ff.base = doc.map(|d| (d.revision(), d.snapshot()));
    }
    ff.list.retain(|e| !tools.contains(&e.finding.tool));
    ff.saved_tools.extend(tools.iter().copied());
    for f in found {
        if ff.list.len() >= MAX_PER_FILE {
            break;
        }
        store.next_key = store.next_key.saturating_add(1);
        let key = store.next_key;
        ff.list.push(EditorFinding { key, finding: f, origin: Origin::OnSave { run }, shown: false });
    }
    sort(&mut ff.list);
    store.dirty.insert(path.to_path_buf());
}

// ---------------------------------------------------------------- publishing

fn advisory(t: Tool) -> bool {
    matches!(t, Tool::CargoAudit | Tool::CargoDeny)
}

fn same_problem(p: &Problem, f: &Finding) -> bool {
    let first = |m: &str| m.lines().next().unwrap_or("").trim().to_string();
    p.start.line == f.start.line
        && (p.code.as_deref().is_some_and(|c| c == f.rule() || f.code.as_deref() == Some(c))
            || first(&p.message) == first(&f.message))
}

/// The other problems of a file (language server, build, CodeCraft's own inspections).
fn others(s: &Session, path: &Path) -> Vec<Problem> {
    s.ws.problems_for(path).into_iter().filter(|p| p.source != ProblemSource::Analysis).cloned().collect()
}

/// A fingerprint of a file's other problems (cheap: no copies; checked each pump).
fn fingerprint<'a>(list: impl Iterator<Item = &'a Problem>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in list.filter(|p| p.source != ProblemSource::Analysis) {
        (p.start.line, &p.message, &p.code).hash(&mut h);
    }
    h.finish()
}

fn problem_of(f: &Finding) -> Problem {
    Problem {
        path: f.path.clone(),
        start: f.start,
        end: f.end,
        severity: f.severity,
        message: f.message.clone(),
        code: Some(f.rule().to_string()),
        source: ProblemSource::Analysis,
    }
}

fn publish(s: &mut Session, path: &Path) {
    let other = others(s, path);
    let fp = fingerprint(other.iter());
    let store = &mut s.analysis.editor;
    store.others.insert(path.to_path_buf(), fp);
    let mut problems = Vec::new();
    if let Some(ff) = store.files.get_mut(path) {
        for e in ff.list.iter_mut() {
            e.shown = !advisory(e.finding.tool) && !other.iter().any(|p| same_problem(p, &e.finding));
            if e.shown {
                problems.push(problem_of(&e.finding));
            }
        }
    }
    let empty = problems.is_empty();
    s.ws.set_inspection_problems(INSPECTION_KEY, path.to_path_buf(), problems);
    let store = &mut s.analysis.editor;
    if empty {
        store.published.remove(path);
    } else {
        store.published.insert(path.to_path_buf());
    }
    if store.files.get(path).is_some_and(|f| f.list.is_empty() && f.saved_tools.is_empty()) {
        store.files.remove(path);
    }
}

/// Map findings through edits, take in changed results, finish on-save runs, publish. Returns
/// whether anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = on_save::pump(s);
    sync_results(s, None);
    // Open documents edited since the last pump.
    let ws = &s.ws;
    let store = &mut s.analysis.editor;
    for (path, ff) in store.files.iter_mut() {
        if sync_file(path, ff, doc_in(ws, path)) {
            store.dirty.insert(path.clone());
        }
    }
    // Language-server diagnostics changed: check the duplicates again (open files only).
    let open: Vec<PathBuf> = s
        .ws
        .docs()
        .filter_map(|d| d.path.clone())
        .filter(|p| s.analysis.editor.files.contains_key(p))
        .take(MAX_DEDUPE_FILES)
        .collect();
    for path in open {
        let fp = fingerprint(s.ws.problems_for(&path).into_iter());
        if s.analysis.editor.others.get(&path) != Some(&fp) {
            s.analysis.editor.dirty.insert(path);
        }
    }
    let dirty = std::mem::take(&mut s.analysis.editor.dirty);
    for path in &dirty {
        publish(s, path);
    }
    // Files that left the store entirely.
    let gone: Vec<PathBuf> =
        s.analysis.editor.published.iter().filter(|p| !s.analysis.editor.files.contains_key(*p)).cloned().collect();
    for p in gone {
        s.ws.set_inspection_problems(INSPECTION_KEY, p.clone(), Vec::new());
        s.analysis.editor.published.remove(&p);
    }
    changed |= !dirty.is_empty();
    changed
}

// ---------------------------------------------------------------- reading (editor, agents)

/// Whether an Inspect Code finding was re-analyzed on save since (agents' lists skip it).
pub fn superseded(s: &Session, f: &Finding) -> bool {
    s.analysis.editor.files.get(&f.path).is_some_and(|ff| ff.saved_tools.contains(&f.tool))
}

/// Findings of the analysis on save (Inspect Code's are in `Session::inspections.results`).
pub fn saved_findings<'a>(s: &'a Session, under: Option<&'a Path>) -> impl Iterator<Item = &'a EditorFinding> + 'a {
    s.analysis
        .editor
        .files
        .iter()
        .filter(move |(p, _)| under.is_none_or(|u| p.starts_with(u)))
        .flat_map(|(_, ff)| ff.list.iter())
        .filter(|e| matches!(e.origin, Origin::OnSave { .. }))
}

/// The shown findings at a position of a file (the caret, the pointer).
pub fn at<'a>(s: &'a Session, path: &Path, pos: Position) -> Vec<&'a EditorFinding> {
    s.analysis.editor.of(path).iter().filter(|e| e.shown && e.finding.start <= pos && pos <= e.finding.end).collect()
}

/// The hover text of the findings at a position: analyzer, rule, message and what Alt+Enter
/// offers.
pub fn hover_lines(s: &Session, path: &Path, pos: Position) -> Vec<String> {
    at(s, path, pos).into_iter().map(|e| hover_text(&e.finding)).collect()
}

/// Every shown finding of a file with its hover text: (start, end, text), for the editor to
/// look up under the pointer.
pub fn hover_entries(s: &Session, path: &Path) -> Vec<(Position, Position, String)> {
    s.analysis.editor.of(path).iter().filter(|e| e.shown).map(|e| (e.finding.start, e.finding.end, hover_text(&e.finding))).collect()
}

/// `Ruff F401: `os` imported but unused` + the fixes and the suppression.
pub fn hover_text(f: &Finding) -> String {
    let mut out = format!("{} {}: {}", f.tool.label(), f.rule(), f.message.trim());
    let fixes: Vec<&Fix> = f.fixes.iter().filter(|x| x.applicability.is_offered()).collect();
    for fix in &fixes {
        out.push_str(&format!("\nFix: {} (⌥⏎)", fix.title));
    }
    if f.code.is_some() && !advisory(f.tool) && f.tool != Tool::ZigFmt {
        out.push_str(&format!("\n{} (⌥⏎)", crate::inspections::suppress_label(f)));
    }
    if let Some(url) = crate::inspections::finding_url(f) {
        out.push_str(&format!("\n{url}"));
    }
    out
}

/// The caret's file and position in the active editor.
fn caret(s: &Session) -> Option<(PathBuf, Position)> {
    let (e, d) = s.ws.active()?;
    let path = d.path.clone()?;
    Some((path, d.doc.offset_to_position(e.selections.primary().head)))
}

/// The findings Alt+Enter acts on: those at the caret, else those on the caret's line.
fn at_caret(s: &Session) -> Vec<&EditorFinding> {
    let Some((path, pos)) = caret(s) else { return Vec::new() };
    let here = at(s, &path, pos);
    if !here.is_empty() {
        return here;
    }
    s.analysis
        .editor
        .of(&path)
        .iter()
        .filter(|e| e.shown && e.finding.start.line <= pos.line && pos.line <= e.finding.end.line)
        .collect()
}

/// Alt+Enter items for the findings at the caret: each fix, then the suppression.
pub(crate) fn context_actions(s: &Session) -> Vec<ActionItem> {
    let mut items = Vec::new();
    for e in at_caret(s) {
        let f = &e.finding;
        for (i, fix) in f.fixes.iter().enumerate().filter(|(_, x)| x.applicability.is_offered()) {
            let title = format!("{}: {}", f.tool.label(), fix.title);
            items.push(ActionItem {
                title: title.clone(),
                kind: Some("quickfix".into()),
                payload: json!({ "title": title, "codecraftCommand": "analysis.editorFix", "codecraftParams": { "key": e.key, "fix": i } }),
            });
        }
        if f.code.is_some() && !advisory(f.tool) && f.tool != Tool::ZigFmt {
            let title = crate::inspections::suppress_label(f);
            items.push(ActionItem {
                title: title.clone(),
                kind: Some("suppress".into()),
                payload: json!({ "title": title, "codecraftCommand": "analysis.editorSuppress", "codecraftParams": { "key": e.key } }),
            });
        }
    }
    items
}

/// Whether Alt+Enter has analyzer items at the caret (Show Context Actions without a language
/// server).
pub(crate) fn has_context_actions(s: &Session) -> bool {
    !context_actions(s).is_empty()
}

/// Show Context Actions with only the analyzer's items (no language server for the file).
pub(crate) fn show_context_actions(s: &mut Session) -> CmdResult {
    let items = context_actions(s);
    let Some(editor) = s.ws.active().map(|(e, _)| e.id) else { return Ok(Value::Null) };
    if items.is_empty() {
        s.status = "No context actions available here".into();
        return Ok(json!({ "items": 0 }));
    }
    let n = items.len();
    s.popup = Some(crate::Popup::Actions { editor, title: "Context Actions".into(), items, selected: 0, pending: false });
    Ok(json!({ "items": n }))
}

// ---------------------------------------------------------------- fixing

fn find_key(s: &Session, key: u64) -> Result<EditorFinding, CommandError> {
    s.analysis.editor.get(key).cloned().ok_or_else(|| {
        CommandError::Failed(format!("no analysis finding {key} (it was fixed, edited or re-analyzed; see analysis.fileFindings)"))
    })
}

/// Apply a fix (positions in the current text), remove the finding and move the file's others.
fn apply(s: &mut Session, e: &EditorFinding, fix: Fix) -> CmdResult {
    let title = fix.title.clone();
    let path = e.finding.path.clone();
    let open = doc_of(s, &path).is_some();
    let (edit, sel, texts) =
        crate::inspections::fixes::fixes_to_workspace_edit(&[(0, fix)], &|p| s.current_text(p)).map_err(CommandError::Failed)?;
    if sel.accepted.is_empty() {
        return Err(CommandError::Failed("the fix no longer matches the file; save it to analyze it again".into()));
    }
    let changed = crate::refactor::apply_or_preview(s, &title, &edit, false)?;
    // A fix touching several files waits in the refactoring preview: the finding stays until
    // the edit lands (it then moves or drops with it, or goes on the next analysis).
    if changed == 0 && s.refactor.preview.as_ref().is_some_and(|pv| pv.title == title) {
        return Ok(json!({ "applied": false, "preview": true, "title": title, "path": path }));
    }
    // Closed files: move the other findings through the splices (open ones follow the edit).
    if !open
        && let (Some(splices), Some(before)) = (sel.splices.get(&path), texts.get(&path))
        && let Ok(after) = crate::inspections::fixes::splice_text(before, splices)
    {
        remap_closed(s, &path, before, &after, splices);
    }
    remove(s, e.key);
    // Keep the Inspection Results in step: the problem is resolved there too.
    if let Origin::Inspection { job, id } = e.origin
        && let Some(r) = s.inspections.results.as_mut().filter(|r| r.job == job)
    {
        r.resolved.insert(id);
        s.analysis.editor.synced = Some((job, r.resolved.len()));
    }
    s.status = format!("{title}: applied");
    Ok(json!({ "applied": true, "title": title, "files": changed.max(1), "path": path }))
}

fn remap_closed(s: &mut Session, path: &Path, before: &str, after: &str, splices: &[crate::inspections::fixes::Splice]) {
    let old = Document::from_text(before);
    let new = Document::from_text(after);
    let Ok(cs) = ChangeSet::new(splices.iter().map(|(r, t)| codecraft_text::Change::new(r.start, r.end, t.clone())).collect())
    else {
        return;
    };
    if let Some(ff) = s.analysis.editor.files.get_mut(path) {
        ff.list.retain_mut(|e| map_finding(&mut e.finding, path, &old, &new, &[&cs]));
        ff.base = None;
        s.analysis.editor.dirty.insert(path.to_path_buf());
    }
}

fn remove(s: &mut Session, key: u64) {
    let store = &mut s.analysis.editor;
    for (path, ff) in store.files.iter_mut() {
        let before = ff.list.len();
        ff.list.retain(|e| e.key != key);
        if ff.list.len() != before {
            store.dirty.insert(path.clone());
        }
    }
}

fn editor_fix(s: &mut Session, p: &Value) -> CmdResult {
    pump(s);
    let key = u64::try_from(req_usize(p, "key")?).unwrap_or(u64::MAX);
    let e = find_key(s, key)?;
    let index = opt_usize(p, "fix")?.unwrap_or(0);
    let fix = e
        .finding
        .fixes
        .get(index)
        .filter(|f| f.applicability.is_offered())
        .cloned()
        .ok_or_else(|| CommandError::Failed(format!("finding {key} has no fix {index}")))?;
    apply(s, &e, fix)
}

fn editor_suppress(s: &mut Session, p: &Value) -> CmdResult {
    pump(s);
    let key = u64::try_from(req_usize(p, "key")?).unwrap_or(u64::MAX);
    let e = find_key(s, key)?;
    let text = s
        .current_text(&e.finding.path)
        .ok_or_else(|| CommandError::Failed(format!("{} cannot be read", e.finding.path.display())))?;
    let change = crate::inspections::fixes::suppress_change(&e.finding, &text).map_err(CommandError::Failed)?;
    let fix = Fix {
        title: crate::inspections::suppress_label(&e.finding),
        applicability: crate::inspections::Applicability::MachineApplicable,
        changes: vec![change],
    };
    apply(s, &e, fix)
}

// ---------------------------------------------------------------- agents

fn level(sev: Severity) -> &'static str {
    super::profiles::Level::of(sev).id()
}

/// A finding as agents read it: absolute path, 1-based positions.
pub fn finding_json(e: &EditorFinding) -> Value {
    let f = &e.finding;
    let mut v = json!({
        "key": e.key,
        "analyzer": f.tool.id(),
        "rule": f.rule(),
        "severity": level(f.severity),
        "line": f.start.line + 1,
        "column": f.start.column + 1,
        "endLine": f.end.line + 1,
        "endColumn": f.end.column + 1,
        "message": f.message,
        "shown": e.shown,
        "fixes": f.fixes.iter().enumerate().filter(|(_, x)| x.applicability.is_offered())
            .map(|(i, x)| json!({ "fix": i, "title": x.title, "safe": x.applicability.is_safe() })).collect::<Vec<_>>(),
        "suppressible": f.code.is_some() && !advisory(f.tool) && f.tool != Tool::ZigFmt,
    });
    if let Some(o) = v.as_object_mut() {
        match e.origin {
            Origin::Inspection { job, id } => {
                o.insert("source".into(), json!("inspection"));
                o.insert("job".into(), json!(job));
                o.insert("id".into(), json!(id));
            }
            Origin::OnSave { run } => {
                o.insert("source".into(), json!("onSave"));
                o.insert("run".into(), json!(run));
            }
        }
        if let Some(url) = crate::inspections::finding_url(f) {
            o.insert("url".into(), json!(url));
        }
    }
    v
}

fn file_findings(s: &mut Session, p: &Value) -> CmdResult {
    pump(s);
    let path = match crate::params::opt_path(p, "path") {
        Some(p) => {
            let p = if p.is_absolute() { p } else { s.project_root().map(|r| r.join(&p)).unwrap_or(p) };
            crate::normalize_path(&p)
        }
        None => s
            .ws
            .active()
            .and_then(|(_, d)| d.path.clone())
            .ok_or_else(|| CommandError::BadParams("`path` is required (no file is open)".into()))?,
    };
    let findings: Vec<Value> = s.analysis.editor.of(&path).iter().map(finding_json).collect();
    let others: Vec<Value> = others(s, &path)
        .iter()
        .map(|p| {
            json!({
                "source": match p.source {
                    ProblemSource::RustAnalyzer => "languageServer",
                    ProblemSource::Cargo => "build",
                    _ => "inspection",
                },
                "severity": level(p.severity),
                "line": p.start.line + 1,
                "column": p.start.column + 1,
                "endLine": p.end.line + 1,
                "endColumn": p.end.column + 1,
                "message": p.message,
                "code": p.code,
            })
        })
        .collect();
    let pending = s.analysis.editor.on_save.pending(&path);
    let running = s.analysis.editor.on_save.running_for(&path);
    let next = if findings.is_empty() && !pending && !running {
        "Save the file (or call analysis.analyzeFile) to analyze it; fix with analysis.editorFix {key, fix}"
    } else {
        "Fix with analysis.editorFix {key, fix}; suppress with analysis.editorSuppress {key}"
    };
    Ok(json!({
        "path": path,
        "findings": findings,
        "otherProblems": others,
        "onSave": {
            "enabled": s.ws.settings.analysis_on_save,
            "pending": pending,
            "running": running,
            "last": s.analysis.editor.on_save.last_for(&path),
        },
        "next": next,
    }))
}

/// The Python language server: the setting, what runs or would run, the managed basedpyright.
fn python_language_server(s: &mut Session, _: &Value) -> CmdResult {
    let status = match s.server_status(codecraft_lsp::ServerKind::Python) {
        crate::LspStatus::Ready => json!("ready"),
        crate::LspStatus::Starting => json!("starting"),
        crate::LspStatus::Unavailable(m) => json!({ "unavailable": m }),
        crate::LspStatus::Stopped(m) => json!({ "stopped": m }),
        crate::LspStatus::Off => json!("off"),
    };
    let found = s.python_server_command();
    let managed = super::install::installed(s, "basedpyright");
    let missing = found.is_none();
    Ok(json!({
        "preference": s.ws.settings.python_language_server,
        "typeChecking": s.ws.settings.python_type_checking,
        "running": s.server_name(codecraft_lsp::ServerKind::Python),
        "status": status,
        "server": found.map(|c| json!({ "name": c.name, "program": c.program, "args": c.args })),
        "managedBasedpyright": managed,
        "next": if missing {
            "no Python language server: analysis.installTool {tool: \"basedpyright\", confirm: true} installs one into CodeCraft's tools (asks the user first), or settings.set python_language_server"
        } else {
            "settings.set {key: \"python_language_server\", value: auto|basedpyright|pyright|pylsp|ty|none} to change it"
        },
    }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "python.languageServer",
            "Python Language Server",
            None,
            "→ {preference, typeChecking, running, status, server: {name, program, args}|null, managedBasedpyright} — which Python language server runs (Settings › Languages & Frameworks › Python)",
            Context::Global,
            always,
            python_language_server,
        ),
        spec(
            "analysis.fileFindings",
            "File Analysis Findings",
            None,
            "{path?: string (default: the active file)} → {path, findings: [{key, analyzer, rule, severity, line, column, endLine, endColumn, message, fixes: [{fix, title, safe}], suppressible, source: inspection|onSave, shown}], otherProblems: [language server / build problems], onSave: {enabled, pending, running, last}} — the findings shown in the editor for one file (absolute path, 1-based lines), current with unsaved edits",
            Context::Global,
            always,
            file_findings,
        ),
        spec(
            "analysis.editorFix",
            "Apply Analyzer Fix",
            None,
            "{key: int, fix?: int (default 0)} — apply a fix of an editor finding (analysis.fileFindings; Alt+Enter offers the same) to the current text",
            Context::Global,
            always,
            editor_fix,
        ),
        spec(
            "analysis.editorSuppress",
            "Suppress Analyzer Finding",
            None,
            "{key: int} — suppress an editor finding with the analyzer's comment (`# noqa: F401`, `#[allow(lint)]`, `// NOLINT(check)`, …)",
            Context::Global,
            always,
            editor_suppress,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::inspections::{Applicability, TextChange};
    use codecraft_text::{EditGroup, SelectionSet};

    fn edit(doc: &mut Document, start: usize, end: usize, text: &str) {
        let sel = SelectionSet::caret(0);
        let e = codecraft_text::Edit { changes: ChangeSet::single(start, end, text), selections: sel.clone() };
        doc.apply(&e, &sel, EditGroup::Standalone).unwrap();
    }

    fn finding(path: &Path, l0: usize, c0: usize, l1: usize, c1: usize) -> Finding {
        let mut f = Finding::new(Tool::Ruff, "F401 unused-import", Severity::Warning, path.to_path_buf(), Position::new(l0, c0));
        f.end = Position::new(l1, c1);
        f.code = Some("F401".into());
        f.message = "`os` imported but unused".into();
        f
    }

    #[test]
    fn ranges_move_with_edits_and_drop_when_touched() {
        let path = Path::new("/p/a.py");
        let mut doc = Document::from_text("import os\nimport sys\nx = 1\n");
        let base = doc.snapshot();
        let rev = doc.revision();
        // Insert a line at the top: everything moves down one line.
        edit(&mut doc, 0, 0, "# hi\n");
        let sets = doc.changes_since(rev).unwrap();
        let mut f = finding(path, 1, 7, 1, 10);
        assert!(map_finding(&mut f, path, &base, &doc, &sets));
        assert_eq!((f.start, f.end), (Position::new(2, 7), Position::new(2, 10)));
        // Typing inside the flagged text drops the finding.
        let base = doc.snapshot();
        let rev = doc.revision();
        let at = doc.position_to_offset(Position::new(2, 8));
        edit(&mut doc, at, at, "Z");
        let sets = doc.changes_since(rev).unwrap();
        assert!(!map_finding(&mut f, path, &base, &doc, &sets));
        // Insertions at the edges leave it alone.
        assert!(!touches(&ChangeSet::single(5, 5, "x"), 5, 9));
        assert!(!touches(&ChangeSet::single(9, 9, "x"), 5, 9));
        assert!(touches(&ChangeSet::single(4, 6, ""), 5, 9));
        assert!(touches(&ChangeSet::single(5, 6, ""), 5, 5), "a zero-width finding under a deletion");
        assert!(!touches(&ChangeSet::single(1, 5, ""), 5, 5));
    }

    #[test]
    fn fixes_move_and_overlapping_ones_drop() {
        let path = Path::new("/p/a.py");
        let mut doc = Document::from_text("import os\nimport sys\n");
        let base = doc.snapshot();
        let rev = doc.revision();
        let mut f = finding(path, 1, 7, 1, 10);
        let change = |l0, c0, l1, c1| TextChange {
            path: path.to_path_buf(),
            start: Position::new(l0, c0),
            end: Position::new(l1, c1),
            new_text: String::new(),
        };
        f.fixes = vec![
            Fix { title: "remove".into(), applicability: Applicability::MachineApplicable, changes: vec![change(1, 0, 2, 0)] },
            Fix { title: "other".into(), applicability: Applicability::MachineApplicable, changes: vec![change(0, 0, 0, 3)] },
        ];
        // Edit line 0 (inside the second fix's text): the first fix moves, the second goes.
        edit(&mut doc, 1, 2, "MM");
        let sets = doc.changes_since(rev).unwrap();
        assert!(map_finding(&mut f, path, &base, &doc, &sets));
        assert_eq!(f.fixes.len(), 1);
        assert_eq!(f.fixes[0].title, "remove");
        assert_eq!(f.fixes[0].changes[0].start, Position::new(1, 0));
    }

    #[test]
    fn hostile_positions_never_panic() {
        let path = Path::new("/p/a.py");
        let doc = Document::from_text("x\n");
        let mut f = finding(path, usize::MAX, usize::MAX, usize::MAX, 0);
        let mut d2 = Document::from_text("x\n");
        let rev = d2.revision();
        edit(&mut d2, 0, 1, "yy");
        let sets = d2.changes_since(rev).unwrap();
        let _ = map_finding(&mut f, path, &doc, &d2, &sets);
        let mut ff = FileFindings::default();
        ff.list.push(EditorFinding { key: 1, finding: f, origin: Origin::OnSave { run: 1 }, shown: true });
        ff.base = Some((rev, doc.snapshot()));
        assert!(sync_file(path, &mut ff, Some(&d2)), "mapped through the edit");
        assert!(!sync_file(path, &mut ff, Some(&d2)), "nothing new");
        assert!(!sync_file(path, &mut ff, None));
        // A revision the log no longer has (or a future one) clears the file.
        ff.list.push(EditorFinding { key: 2, finding: finding(path, 0, 0, 0, 1), origin: Origin::OnSave { run: 1 }, shown: true });
        ff.base = Some((u64::MAX, doc.snapshot()));
        assert!(sync_file(path, &mut ff, Some(&d2)));
        assert!(ff.list.is_empty());
    }

    #[test]
    fn hover_text_names_the_analyzer_rule_and_fixes() {
        let mut f = finding(Path::new("/p/a.py"), 0, 7, 0, 9);
        f.fixes.push(Fix { title: "Remove unused import: `os`".into(), applicability: Applicability::MachineApplicable, changes: vec![] });
        let t = hover_text(&f);
        assert!(t.starts_with("Ruff F401: `os` imported but unused"), "{t}");
        assert!(t.contains("Fix: Remove unused import: `os`"), "{t}");
        assert!(t.contains("# noqa: F401"), "{t}");
    }

    #[test]
    fn python_server_config_and_hints() {
        assert!(crate::lsp::python_config(None, "").is_none());
        let v = crate::lsp::python_config(Some(Path::new("/v/bin/python")), "strict").unwrap();
        assert_eq!(v["typeCheckingMode"], "strict");
        assert_eq!(v["analysis"]["typeCheckingMode"], "strict");
        assert_eq!(v["pythonPath"], "/v/bin/python");
        let v = crate::lsp::python_config(Some(Path::new("/v/bin/python")), "").unwrap();
        assert!(v.get("typeCheckingMode").is_none());
        assert!(crate::lsp::python_server_hint("auto").contains("Install basedpyright"));
        assert!(crate::lsp::python_server_hint("none").contains("turned off"));
        assert!(crate::lsp::python_server_hint("pyright").starts_with("pyright was not found"));
    }

    #[test]
    fn cargo_deny_stays_offline_without_advisories() {
        use crate::cargo_audit::deny_args;
        let off = deny_args(Tool::CargoDeny, false, true).join(" ");
        assert_eq!(off, "--offline --format json --color never check licenses bans sources");
        assert_eq!(deny_args(Tool::CargoDeny, true, true).join(" "), "--format json --color never check");
        assert_eq!(deny_args(Tool::CargoDeny, true, false).join(" "), "--format json --color never check advisories");
        assert_eq!(deny_args(Tool::CargoAudit, true, false), ["audit", "--json"]);
        // The built-in profiles keep the network off.
        for p in super::super::profiles::InspectionProfile::builtins() {
            assert!(!p.advisories, "{}", p.name);
        }
    }

    #[test]
    fn duplicates_of_language_server_diagnostics() {
        let f = finding(Path::new("/p/a.py"), 3, 0, 3, 2);
        let p = |line, code: Option<&str>, msg: &str| Problem {
            path: "/p/a.py".into(),
            start: Position::new(line, 0),
            end: Position::new(line, 1),
            severity: Severity::Warning,
            message: msg.into(),
            code: code.map(str::to_string),
            source: ProblemSource::RustAnalyzer,
        };
        assert!(same_problem(&p(3, Some("F401"), "x"), &f));
        assert!(same_problem(&p(3, None, "`os` imported but unused\nmore"), &f));
        assert!(!same_problem(&p(4, Some("F401"), "x"), &f));
        assert!(!same_problem(&p(3, Some("E501"), "other"), &f));
    }
}
