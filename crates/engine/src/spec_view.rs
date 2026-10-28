//! The Spec View: the project's specs (`specs/<feature>/{requirements,design,tasks}.md`), the
//! code that points at their requirements (`// spec: REQ-12`, `#[spec("REQ-12")]`), and for each
//! requirement whether it is in sync (`codecraft_spec` does the reading and the rules).
//!
//! The project is scanned on a worker thread when the tool window is first shown (or an agent
//! asks); files the watcher reports as changed are re-read; open documents are re-scanned for
//! references as they change. Nothing here edits code. The writes are `spec.new` (a scaffold in
//! a new directory), `spec.markSynced` (the private baseline in `.codecraft/spec-state.json`) and
//! the edits the user asks for in the view: `spec.addRequirement`, `spec.editRequirement` and
//! `spec.setTask` change one requirement or one checkbox, through the open editor when the file
//! is open (undoable) and never touch anything else in the file. `spec.implement` and
//! `spec.reconcile` hand a requirement to a coding agent (an ordinary agent job, in review mode
//! by default): the first to build it, the second to propose an edit to the spec when the code
//! moved under it. Both come back as a diff the user accepts or rejects.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use regex::Regex;
use serde_json::{Value, json};

use codecraft_agent::Backend;
use codecraft_spec as sp;
use codecraft_workspace::ToolWindowId;

use crate::agents::{self, JobId, JobStatus, StartRequest, mode_id, mode_support, parse_mode};
use crate::approvals::AgentMode;
use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::{opt_str, opt_usize, req_path, req_str, req_usize};
use crate::{CommandError, Session};

/// Code lines scanned for references per project (a generated file full of `spec:` cannot flood).
const MAX_MATCHES: usize = 20_000;
/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Changed files rescanned at once (a branch switch touching more triggers a full scan).
const MAX_RESCAN: usize = 200;

/// What a worker found.
struct Scan {
    features: Vec<sp::Feature>,
    baseline: sp::Baseline,
    files: BTreeMap<PathBuf, Vec<sp::CodeLink>>,
    required: BTreeMap<PathBuf, Vec<sp::PublicItem>>,
    truncated: bool,
}

/// What one code file holds.
#[derive(Default)]
struct FileScan {
    links: Vec<sp::CodeLink>,
    /// The public items nothing covers, when the file asks for it (`// spec: required`).
    unspecified: Option<Vec<sp::PublicItem>>,
}

enum Msg {
    /// A full scan finished.
    Full(Scan),
    /// The spec files were read again.
    Features(Vec<sp::Feature>, sp::Baseline),
    /// Some code files were rescanned (a file with nothing in it is forgotten).
    Files(Vec<(PathBuf, FileScan)>),
}

/// What an agent job started from the view is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentWork {
    /// Build a requirement.
    Implement,
    /// Propose an edit to a requirement the code has moved away from.
    Reconcile,
}

impl AgentWork {
    pub fn id(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Reconcile => "reconcile",
        }
    }
}

/// The agent job started for a requirement.
#[derive(Clone, Copy, Debug)]
pub struct SpecJob {
    pub job: JobId,
    pub work: AgentWork,
}

#[derive(Default)]
pub struct SpecState {
    pub features: Vec<sp::Feature>,
    /// The latest agent job started for each requirement (by [`sp::requirement_key`]).
    pub jobs: BTreeMap<String, SpecJob>,
    /// References in code, by file.
    pub files: BTreeMap<PathBuf, Vec<sp::CodeLink>>,
    /// Files that ask for their public API to be specified (`// spec: required`), with the public
    /// items no reference covers.
    pub required: BTreeMap<PathBuf, Vec<sp::PublicItem>>,
    /// The references resolved against the features.
    pub index: sp::LinkIndex,
    pub baseline: sp::Baseline,
    pub scanning: bool,
    /// The scan stopped at its cap.
    pub truncated: bool,
    /// The project root the results belong to.
    project: Option<PathBuf>,
    /// A full scan was started for `project`.
    started: bool,
    full: Option<Receiver<Msg>>,
    others: Vec<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
    /// Revision of each open document the references were taken from.
    doc_revs: HashMap<PathBuf, u64>,
}

impl SpecState {
    pub fn busy(&self) -> bool {
        self.scanning || !self.others.is_empty()
    }

    /// A scan has run for the open project and nothing is still being read.
    pub fn scanned(&self) -> bool {
        self.started && !self.busy()
    }

    /// Number of references in code.
    pub fn link_count(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    /// Public items in files that require specs and that no reference covers.
    pub fn unspecified_count(&self) -> usize {
        self.required.values().map(Vec::len).sum()
    }

    pub fn feature(&self, name: &str) -> Option<&sp::Feature> {
        self.features.iter().find(|f| f.name == name)
    }

    /// The code that points at a requirement, as the baseline records it.
    pub fn code_items_of(&self, feature: &str, id: &str) -> Vec<sp::CodeItem> {
        match &self.project {
            Some(root) => sp::code_items(root, self.index.links_of(feature, id)),
            None => Vec::new(),
        }
    }

    /// How that code differs from what it was when the requirement was last marked in sync.
    pub fn code_changes_of(&self, feature: &str, id: &str) -> Vec<sp::CodeChange> {
        let synced = self.baseline.code.get(&sp::requirement_key(feature, id));
        sp::code_changes(synced, &self.code_items_of(feature, id))
    }

    /// Where a requirement stands: the baseline, the links, the code and the text decide.
    pub fn status_of(&self, feature: &sp::Feature, req: &sp::Requirement) -> sp::SpecStatus {
        let synced = self.baseline.requirements.get(&sp::requirement_key(&feature.name, &req.id)).map(String::as_str);
        let code_changed = !self.code_changes_of(&feature.name, &req.id).is_empty();
        sp::status(req, self.index.links_of(&feature.name, &req.id).len(), synced, code_changed)
    }

    fn rebuild_index(&mut self) {
        self.index = sp::build_index(&self.features, &self.files);
    }
}

/// The agent job last started for a requirement, if it still exists.
pub fn spec_job<'a>(s: &'a Session, feature: &str, id: &str) -> Option<(SpecJob, &'a agents::Job)> {
    let sj = *s.spec.jobs.get(&sp::requirement_key(feature, id))?;
    s.agents.job(sj.job).map(|job| (sj, job))
}

/// The job is still building, or waiting for its changes to be accepted or rejected.
pub fn is_working(job: &agents::Job) -> bool {
    job.can_follow_up().is_err() || job.status == JobStatus::Reviewing || job.pending_review() > 0
}

fn agent_job_json(s: &Session, feature: &str, id: &str) -> Value {
    match spec_job(s, feature, id) {
        Some((sj, job)) => json!({
            "id": sj.job.0,
            "kind": sj.work.id(),
            "status": job.status.label(),
            "backend": job.backend.id(),
            "mode": mode_id(job.mode),
            "working": is_working(job),
        }),
        None => Value::Null,
    }
}

fn is_doc_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["md", "markdown", "txt", "rst", "adoc", "org"].iter().any(|d| e.eq_ignore_ascii_case(d)))
}

fn in_private_dir(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| rel.starts_with(".codecraft"))
}

/// Code that can hold references: not documentation (which quotes examples), not the specs
/// themselves, not CodeCraft's private directory.
fn is_code_path(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
        && !path.starts_with(root.join(sp::SPEC_DIR))
        && !in_private_dir(root, path)
        && !is_doc_file(path)
}

fn reference_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)spec[:(]").ok()).as_ref()
}

/// The references (and, for files that ask, the unspecified public items) in the files
/// `find_in_files` found a candidate line in.
#[allow(clippy::type_complexity)]
fn scan_code(
    root: &Path,
    cancel: &AtomicBool,
) -> (BTreeMap<PathBuf, Vec<sp::CodeLink>>, BTreeMap<PathBuf, Vec<sp::PublicItem>>, bool) {
    let mut files: BTreeMap<PathBuf, Vec<sp::CodeLink>> = BTreeMap::new();
    let mut required: BTreeMap<PathBuf, Vec<sp::PublicItem>> = BTreeMap::new();
    let Some(re) = reference_regex() else { return (files, required, false) };
    let opts =
        codecraft_fs::FindOptions { max_results: MAX_MATCHES, max_file_size: MAX_FILE_BYTES, ..Default::default() };
    let Ok(matches) = codecraft_fs::find_in_files(root, re, &opts, cancel) else { return (files, required, false) };
    let truncated = matches.len() >= MAX_MATCHES;
    // The candidates are read again whole: the items the references are attached to span lines.
    let candidates: BTreeSet<PathBuf> = matches.into_iter().map(|m| m.path).filter(|p| is_code_path(root, p)).collect();
    for path in candidates {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let scan = scan_file(&path);
        if !scan.links.is_empty() {
            files.insert(path.clone(), scan.links);
        }
        if let Some(items) = scan.unspecified {
            required.insert(path, items);
        }
    }
    (files, required, truncated)
}

/// The references in `text`, each with the item it is attached to, and the public items nothing
/// covers if the file asks for that.
fn scan_text(path: &Path, text: &str) -> FileScan {
    let mut links = sp::scan_links(text);
    sp::attach_symbols(text, path, &mut links);
    let unspecified = sp::requires_spec(text).then(|| sp::unspecified_items(text, path, &links));
    FileScan { links, unspecified }
}

fn scan_file(path: &Path) -> FileScan {
    if std::fs::metadata(path).map(|m| !m.is_file() || m.len() > MAX_FILE_BYTES).unwrap_or(true) {
        return FileScan::default();
    }
    codecraft_fs::read_text_file(path).map(|t| scan_text(path, &t)).unwrap_or_default()
}

/// Read everything (blocking; runs on a worker thread).
fn scan_all(root: &Path, cancel: &AtomicBool) -> Scan {
    let features = sp::load_features(root);
    let baseline = sp::load_baseline(root);
    let (files, required, truncated) = scan_code(root, cancel);
    Scan { features, baseline, files, required, truncated }
}

fn reset(s: &mut Session, root: Option<PathBuf>) {
    if let Some(c) = s.spec.cancel.take() {
        c.store(true, Ordering::Relaxed);
    }
    s.spec = SpecState { project: root, ..SpecState::default() };
}

/// Start a full scan of the open project (replacing a running one).
pub(crate) fn refresh(s: &mut Session) {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return };
    if s.spec.project.as_ref() != Some(&root) {
        reset(s, Some(root.clone()));
    }
    if let Some(c) = s.spec.cancel.take() {
        c.store(true, Ordering::Relaxed);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::bounded(1);
    let flag = Arc::clone(&cancel);
    let spawned = std::thread::Builder::new().name("codecraft-spec".into()).spawn(move || {
        let scan = scan_all(&root, &flag);
        if !flag.load(Ordering::Relaxed) {
            let _ = tx.send(Msg::Full(scan));
        }
    });
    s.spec.started = true;
    match spawned {
        Ok(_) => {
            s.spec.full = Some(rx);
            s.spec.cancel = Some(cancel);
            s.spec.scanning = true;
            s.spec.doc_revs.clear();
        }
        Err(e) => log::warn!("cannot start the spec scan: {e}"),
    }
}

/// Read the spec files again (after one of them changed).
fn reload_features(s: &mut Session, root: PathBuf) {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-spec-files".into()).spawn(move || {
        let _ = tx.send(Msg::Features(sp::load_features(&root), sp::load_baseline(&root)));
    });
    if spawned.is_ok() {
        s.spec.others.push(rx);
    }
}

/// Files the watcher reported (after a save, a git checkout, an agent's edit).
pub(crate) fn files_changed(s: &mut Session, paths: &[PathBuf]) {
    let Some(root) = s.spec.project.clone().filter(|_| s.spec.started) else { return };
    if paths.iter().any(|p| p.starts_with(root.join(sp::SPEC_DIR))) {
        reload_features(s, root.clone());
    }
    let excluded = codecraft_fs::ScanOptions::default().exclude_dirs;
    let code: Vec<PathBuf> = paths
        .iter()
        .filter(|p| is_code_path(&root, p))
        .filter(|p| {
            s.spec.files.contains_key(*p)
                || !p.exists()
                || p.strip_prefix(&root)
                    .is_ok_and(|rel| !rel.components().any(|c| excluded.iter().any(|x| c.as_os_str() == x.as_str())))
        })
        .cloned()
        .collect();
    if code.is_empty() {
        return;
    }
    if code.len() > MAX_RESCAN {
        refresh(s);
        return;
    }
    for p in &code {
        s.spec.doc_revs.remove(p);
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-spec-code".into()).spawn(move || {
        let out: Vec<(PathBuf, FileScan)> = code
            .into_iter()
            .map(|p| {
                let scan = scan_file(&p);
                (p, scan)
            })
            .collect();
        let _ = tx.send(Msg::Files(out));
    });
    if spawned.is_ok() {
        s.spec.others.push(rx);
    }
}

/// Take what a file holds now; says whether anything changed.
fn set_file(st: &mut SpecState, path: PathBuf, scan: FileScan) -> bool {
    let mut changed = false;
    if scan.links.is_empty() {
        changed |= st.files.remove(&path).is_some();
    } else if st.files.get(&path) != Some(&scan.links) {
        st.files.insert(path.clone(), scan.links);
        changed = true;
    }
    match scan.unspecified {
        None => changed |= st.required.remove(&path).is_some(),
        Some(items) if st.required.get(&path) != Some(&items) => {
            st.required.insert(path, items);
            changed = true;
        }
        Some(_) => {}
    }
    changed
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root != s.spec.project {
        // A different project (or none): drop the old results.
        reset(s, root.clone());
        changed = true;
    }
    if root.is_some() && !s.spec.started && s.ws.layout.is_visible(ToolWindowId::Spec) {
        refresh(s);
        changed = true;
    }
    if let Some(rx) = &s.spec.full {
        match rx.try_recv() {
            Ok(Msg::Full(scan)) => {
                s.spec.features = scan.features;
                s.spec.baseline = scan.baseline;
                s.spec.files = scan.files;
                s.spec.required = scan.required;
                s.spec.truncated = scan.truncated;
                s.spec.scanning = false;
                s.spec.full = None;
                s.spec.doc_revs.clear();
                s.spec.rebuild_index();
                changed = true;
            }
            Ok(_) | Err(TryRecvError::Disconnected) => {
                s.spec.scanning = false;
                s.spec.full = None;
                changed = true;
            }
            Err(TryRecvError::Empty) => {}
        }
    }
    let mut i = 0;
    let mut reindex = false;
    while let Some(rx) = s.spec.others.get(i) {
        match rx.try_recv() {
            Ok(Msg::Features(features, baseline)) => {
                s.spec.features = features;
                s.spec.baseline = baseline;
                s.spec.others.remove(i);
                reindex = true;
            }
            Ok(Msg::Files(list)) => {
                for (p, scan) in list {
                    if s.ws.doc_by_path(&p).is_none() {
                        reindex |= set_file(&mut s.spec, p, scan);
                    }
                }
                s.spec.others.remove(i);
            }
            Ok(Msg::Full(_)) | Err(TryRecvError::Disconnected) => {
                s.spec.others.remove(i);
            }
            Err(TryRecvError::Empty) => i += 1,
        }
    }
    // Open documents (unsaved edits included) while the tool window is shown.
    if let Some(root) = root
        .filter(|_| s.spec.started && !s.spec.scanning && !s.power_save() && s.ws.layout.is_visible(ToolWindowId::Spec))
    {
        let mut updates = Vec::new();
        for od in s.ws.docs() {
            let Some(p) = od.path.as_ref().filter(|p| is_code_path(&root, p)) else { continue };
            let rev = od.doc.revision();
            if s.spec.doc_revs.get(p) == Some(&rev) {
                continue;
            }
            updates.push((p.clone(), rev, scan_text(p, &od.doc.text())));
        }
        for (p, rev, scan) in updates {
            s.spec.doc_revs.insert(p.clone(), rev);
            reindex |= set_file(&mut s.spec, p, scan);
        }
    }
    if reindex {
        s.spec.rebuild_index();
        changed = true;
    }
    changed
}

// ---------------------------------------------------------------- JSON for the UI and agents

fn status_name(st: sp::SpecStatus) -> &'static str {
    match st {
        sp::SpecStatus::SpecChanged => "specChanged",
        sp::SpecStatus::CodeChanged => "codeChanged",
        sp::SpecStatus::Unlinked => "unlinked",
        sp::SpecStatus::Linked => "linked",
        sp::SpecStatus::InSync => "inSync",
    }
}

fn path_json(p: &Option<PathBuf>) -> Value {
    p.as_ref().map_or(Value::Null, |p| json!(p))
}

fn feature_json(s: &Session, f: &sp::Feature) -> Value {
    let requirements: Vec<Value> = f
        .requirements
        .iter()
        .map(|r| {
            let tasks = f.tasks.iter().filter(|t| t.requirements.contains(&r.id)).count();
            json!({
                "id": r.id,
                "title": r.display_title(),
                "status": status_name(s.spec.status_of(f, r)),
                "ears": r.ears,
                "line": r.line,
                "links": s.spec.index.links_of(&f.name, &r.id).len(),
                "codeChanges": s.spec.code_changes_of(&f.name, &r.id).len(),
                "tasks": tasks,
                "agentJob": agent_job_json(s, &f.name, &r.id),
            })
        })
        .collect();
    let (done, total) = f.task_progress();
    json!({
        "name": f.name,
        "title": f.title,
        "dir": f.dir,
        "files": {
            "requirements": path_json(&f.requirements_file),
            "design": path_json(&f.design_file),
            "tasks": path_json(&f.tasks_file),
        },
        "requirements": requirements,
        "design": f.design,
        "tasks": { "done": done, "total": total },
        "diagnostics": f.diagnostics,
    })
}

fn list_json(s: &Session) -> Value {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for f in &s.spec.features {
        for r in &f.requirements {
            *counts.entry(status_name(s.spec.status_of(f, r))).or_default() += 1;
        }
    }
    json!({
        // Still reading: `codecraft_execute` with `waitMs` waits for this to clear.
        "pending": s.spec.busy(),
        "truncated": s.spec.truncated,
        "specDir": root.map(|r| r.join(sp::SPEC_DIR)),
        "features": s.spec.features.iter().map(|f| feature_json(s, f)).collect::<Vec<_>>(),
        "counts": counts,
        "codeReferences": s.spec.link_count(),
        "unspecified": { "files": s.spec.required.len(), "items": s.spec.unspecified_count() },
        "danglingLinks": s.spec.index.dangling,
        "ambiguousLinks": s.spec.index.ambiguous,
    })
}

fn show_json(s: &Session, p: &Value) -> CmdResult {
    let feature = req_str(p, "feature")?;
    let id = req_str(p, "id")?;
    let f = s.spec.feature(feature).ok_or_else(|| CommandError::BadParams(format!("no feature `{feature}`")))?;
    let r = f.requirement(id).ok_or_else(|| CommandError::BadParams(format!("{feature} has no requirement {id}")))?;
    let tasks: Vec<_> = f.tasks.iter().filter(|t| t.requirements.contains(&r.id)).collect();
    Ok(json!({
        "feature": f.name,
        "id": r.id,
        "title": r.title,
        "text": r.text,
        "ears": r.ears,
        "status": status_name(s.spec.status_of(f, r)),
        "file": f.requirements_file,
        "line": r.line,
        "endLine": r.end_line,
        "hash": r.hash,
        "syncedHash": s.spec.baseline.requirements.get(&sp::requirement_key(&f.name, &r.id)),
        "links": s.spec.index.links_of(&f.name, &r.id),
        "codeChanges": s.spec.code_changes_of(&f.name, &r.id),
        "tasks": tasks,
        "agentJob": agent_job_json(s, &f.name, &r.id),
    }))
}

/// Start the scan if none ran for the open project (a command that reads the results).
fn ensure_scan(s: &mut Session) {
    let current = s.ws.project.as_ref().map(|p| p.root.clone());
    if current.is_some() && (!s.spec.started || s.spec.project != current) {
        refresh(s);
    }
}

/// The public items no requirement covers, in the files that ask (`// spec: required`).
fn unspecified_json(s: &Session, p: &Value) -> Value {
    let only = opt_str(p, "path").map(|path| {
        let path = PathBuf::from(path);
        match &s.spec.project {
            Some(root) if path.is_relative() => root.join(path),
            _ => path,
        }
    });
    let files: Vec<Value> = s
        .spec
        .required
        .iter()
        .filter(|(path, items)| !items.is_empty() && only.as_ref().is_none_or(|o| o == *path))
        .map(|(path, items)| json!({ "path": path, "count": items.len(), "items": items }))
        .collect();
    json!({
        "pending": s.spec.busy(),
        "optedIn": s.spec.required.len(),
        "total": s.spec.unspecified_count(),
        "files": files,
    })
}

fn cmd_error(e: &sp::SpecError) -> CommandError {
    match e {
        sp::SpecError::Invalid(m) => CommandError::BadParams(m.clone()),
        sp::SpecError::Io(m) => CommandError::Failed(m.clone()),
    }
}

fn project_root(s: &Session) -> Result<PathBuf, CommandError> {
    s.ws.project.as_ref().map(|p| p.root.clone()).ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

fn open_spec(s: &mut Session, p: &Value) -> CmdResult {
    let feature = req_str(p, "feature")?;
    let f = s
        .spec
        .feature(feature)
        .ok_or_else(|| CommandError::BadParams(format!("no feature `{feature}` (scan not finished?)")))?;
    let (path, line) = match opt_str(p, "id") {
        Some(id) => {
            let r = f
                .requirement(id)
                .ok_or_else(|| CommandError::BadParams(format!("{feature} has no requirement {id}")))?;
            (f.requirements_file.clone(), r.line.saturating_sub(1))
        }
        None => {
            let which = opt_str(p, "file").unwrap_or("requirements");
            let path = match which {
                "requirements" => f.requirements_file.clone(),
                "design" => f.design_file.clone(),
                "tasks" => f.tasks_file.clone(),
                other => {
                    return Err(CommandError::BadParams(format!(
                        "file is requirements, design or tasks, not `{other}`"
                    )));
                }
            };
            (path, 0)
        }
    };
    let path = path.ok_or_else(|| CommandError::Failed(format!("{feature} has no such file")))?;
    s.navigate_to_position(&path, usize::try_from(line).unwrap_or(0), 0)?;
    Ok(json!({ "opened": path, "line": line.saturating_add(1) }))
}

fn new_feature(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let name = req_str(p, "name")?;
    // Read the specs as they are now: the id of the first requirement must not collide.
    let existing = sp::load_features(&root);
    let f = sp::new_feature(&root, name, sp::next_requirement_number(&existing)).map_err(|e| cmd_error(&e))?;
    s.ws.layout.show(ToolWindowId::Spec);
    refresh(s);
    s.status = format!("Created specs/{}", f.name);
    if let Some(path) = &f.requirements_file {
        s.open_file(path)?;
    }
    Ok(json!({ "feature": f.name, "dir": f.dir, "requirements": f.requirements_file }))
}

/// Fail while the scan the answer depends on is not there yet (starting it if none ran).
fn require_scan(s: &mut Session) -> Result<(), CommandError> {
    if s.spec.scanned() {
        return Ok(());
    }
    if !s.spec.started {
        refresh(s);
    }
    Err(CommandError::Failed("the specs are still being scanned; try again in a moment".into()))
}

fn mark_synced(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let name = req_str(p, "feature")?;
    if !sp::is_feature_name(name) {
        return Err(CommandError::BadParams(format!("`{name}` is not a feature name")));
    }
    // The code is recorded as the scan has it: do not record a scan that is not there yet.
    require_scan(s)?;
    // The spec files as they are on disk now, not the (possibly older) scan.
    let feature = sp::load_feature(&root.join(sp::SPEC_DIR).join(name))
        .ok_or_else(|| CommandError::BadParams(format!("no feature `{name}`")))?;
    let code: BTreeMap<String, Vec<sp::CodeItem>> = feature
        .requirements
        .iter()
        .map(|r| (sp::requirement_key(&feature.name, &r.id), s.spec.code_items_of(&feature.name, &r.id)))
        .collect();
    let mut baseline = sp::load_baseline(&root);
    let n = sp::mark_synced(&mut baseline, &feature, opt_str(p, "id"), &code).map_err(|e| cmd_error(&e))?;
    sp::save_baseline(&root, &baseline).map_err(|e| cmd_error(&e))?;
    s.spec.baseline = baseline;
    s.spec.rebuild_index();
    s.status = format!("Marked {n} requirement{} in sync", if n == 1 { "" } else { "s" });
    Ok(json!({ "feature": name, "recorded": n }))
}

// ---------------------------------------------------------------- editing

/// The text of `path` as the editor has it (unsaved edits included), else from disk; a file
/// that does not exist yet reads as empty.
fn current_text(s: &Session, path: &Path) -> Result<String, CommandError> {
    if let Some(od) = s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        return Ok(od.doc.text());
    }
    if !path.exists() {
        return Ok(String::new());
    }
    codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(format!("cannot read {}: {e}", path.display())))
}

/// Write an edited spec file (through the open document, else atomically) and read the specs
/// again.
fn write_spec_file(s: &mut Session, path: &Path, text: &str) -> Result<(), CommandError> {
    crate::ide_server::apply_edit(s, path, text).map_err(CommandError::Failed)?;
    if let Some(root) = s.spec.project.clone().filter(|_| s.spec.started) {
        reload_features(s, root);
    }
    Ok(())
}

/// The feature a command names, from the last scan.
fn named_feature<'a>(s: &'a Session, p: &Value) -> Result<&'a sp::Feature, CommandError> {
    let name = req_str(p, "feature")?;
    s.spec.feature(name).ok_or_else(|| CommandError::BadParams(format!("no feature `{name}` (scan not finished?)")))
}

fn add_requirement(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let f = named_feature(s, p)?;
    let (name, dir) = (f.name.clone(), f.dir.clone());
    let title = opt_str(p, "title").unwrap_or_default();
    let body = opt_str(p, "text").unwrap_or_default();
    if title.trim().is_empty() && body.trim().is_empty() {
        return Err(CommandError::BadParams("give the requirement a title or a text".into()));
    }
    let path = dir.join(sp::REQUIREMENTS_FILE);
    let text = current_text(s, &path)?;
    // Numbers are project-wide: take the next one after everything on disk.
    let number = sp::next_requirement_number(&sp::load_features(&root));
    let updated = sp::add_requirement(&text, number, title, body).map_err(|e| cmd_error(&e))?;
    write_spec_file(s, &path, &updated)?;
    let id = format!("REQ-{number}");
    let line = sp::parse_requirements(&updated).requirements.iter().find(|r| r.id == id).map_or(0, |r| r.line);
    s.status = format!("Added {id} to {name}");
    Ok(json!({ "feature": name, "id": id, "file": path, "line": line }))
}

fn edit_requirement(s: &mut Session, p: &Value) -> CmdResult {
    let f = named_feature(s, p)?;
    let (name, file) = (f.name.clone(), f.requirements_file.clone());
    let id = req_str(p, "id")?.to_string();
    let (title, body) = (opt_str(p, "title"), opt_str(p, "text"));
    if title.is_none() && body.is_none() {
        return Err(CommandError::BadParams("pass `title` and/or `text`".into()));
    }
    let path = file.ok_or_else(|| CommandError::Failed(format!("{name} has no requirements.md")))?;
    let text = current_text(s, &path)?;
    let updated = sp::edit_requirement(&text, &id, title, body).map_err(|e| cmd_error(&e))?;
    if updated != text {
        write_spec_file(s, &path, &updated)?;
    }
    let line = sp::parse_requirements(&updated).requirements.iter().find(|r| r.id == id).map_or(0, |r| r.line);
    s.status = format!("Edited {id}");
    Ok(json!({ "feature": name, "id": id, "file": path, "line": line, "changed": updated != text }))
}

fn set_task(s: &mut Session, p: &Value) -> CmdResult {
    let f = named_feature(s, p)?;
    let (name, file) = (f.name.clone(), f.tasks_file.clone());
    let line =
        u32::try_from(req_usize(p, "line")?).map_err(|_| CommandError::BadParams("`line` is out of range".into()))?;
    let state = match req_str(p, "state")? {
        "todo" => sp::TaskState::Todo,
        "doing" => sp::TaskState::Doing,
        "done" => sp::TaskState::Done,
        other => return Err(CommandError::BadParams(format!("state is todo, doing or done, not `{other}`"))),
    };
    let path = file.ok_or_else(|| CommandError::Failed(format!("{name} has no tasks.md")))?;
    let text = current_text(s, &path)?;
    let updated = sp::set_task_state(&text, line, state).map_err(|e| cmd_error(&e))?;
    if updated != text {
        write_spec_file(s, &path, &updated)?;
    }
    Ok(json!({ "feature": name, "line": line, "changed": updated != text }))
}

// ---------------------------------------------------------------- agents

/// The requirement a `spec.implement` / `spec.reconcile` call is about, and for implementing the
/// task of `tasks.md` (by line) it narrows the work to.
fn agent_target<'a>(
    feature: &'a sp::Feature,
    p: &Value,
    work: AgentWork,
) -> Result<(&'a sp::Requirement, Option<&'a sp::Task>), CommandError> {
    let task = match (work, opt_usize(p, "task")?) {
        (AgentWork::Implement, Some(line)) => {
            Some(feature.tasks.iter().find(|t| usize::try_from(t.line).is_ok_and(|l| l == line)).ok_or_else(|| {
                CommandError::BadParams(format!("{} has no task at line {line} of tasks.md", feature.name))
            })?)
        }
        _ => None,
    };
    let id = match (opt_str(p, "id"), task) {
        (Some(id), _) => id,
        (None, Some(t)) => t.requirements.first().map(String::as_str).ok_or_else(|| {
            CommandError::BadParams("that task names no requirement (write `(REQ-n)` in it, or pass `id`)".into())
        })?,
        (None, None) => {
            return Err(CommandError::BadParams("pass `id` (a requirement) or `task` (a line of tasks.md)".into()));
        }
    };
    let req = feature
        .requirement(id)
        .ok_or_else(|| CommandError::BadParams(format!("{} has no requirement {id}", feature.name)))?;
    Ok((req, task))
}

/// The agent to run: the one asked for, else the first installed that can run `mode`.
fn pick_backend(s: &mut Session, p: &Value, mode: AgentMode) -> Result<Backend, CommandError> {
    if let Some(id) = opt_str(p, "backend") {
        return Backend::from_id(id).ok_or_else(|| {
            CommandError::BadParams(format!("unknown backend `{id}` (claude, codex, grok, antigravity)"))
        });
    }
    s.agents.refresh_backends();
    s.agents.backends().iter().find(|b| mode_support(b, mode).is_ok()).map(|b| b.backend).ok_or_else(|| {
        let names: Vec<&str> = Backend::ALL.iter().map(|b| b.label()).collect();
        CommandError::Failed(format!(
            "no coding agent is installed that can run in {} mode ({}); see the Agent tool window",
            mode_id(mode),
            names.join(", ")
        ))
    })
}

/// Hand a requirement to a coding agent: an ordinary agent job (see `agent.start`), with a brief
/// built from the spec files as they are on disk, which are also what a review-mode agent sees.
fn start_agent(s: &mut Session, p: &Value, work: AgentWork) -> CmdResult {
    let root = project_root(s)?;
    let name = req_str(p, "feature")?;
    if !sp::is_feature_name(name) {
        return Err(CommandError::BadParams(format!("`{name}` is not a feature name")));
    }
    // The links and code changes in the brief come from the scan.
    require_scan(s)?;
    let feature = sp::load_feature(&root.join(sp::SPEC_DIR).join(name))
        .ok_or_else(|| CommandError::BadParams(format!("no feature `{name}`")))?;
    let (req, task) = agent_target(&feature, p, work)?;
    let key = sp::requirement_key(&feature.name, &req.id);
    if let Some((sj, job)) = spec_job(s, &feature.name, &req.id)
        && is_working(job)
    {
        return Err(CommandError::Failed(format!(
            "{key} already has an agent job ({} {}, job {}); let it finish, review its changes or stop it first",
            sj.work.id(),
            job.status.label(),
            sj.job.0
        )));
    }
    let mode = opt_str(p, "mode").unwrap_or("review");
    let mode = parse_mode(mode)
        .ok_or_else(|| CommandError::BadParams(format!("unknown mode `{mode}` (auto, ask, review)")))?;
    let backend = pick_backend(s, p, mode)?;
    let links = s.spec.index.links_of(&feature.name, &req.id).to_vec();
    let prompt = match work {
        AgentWork::Implement => sp::implement_prompt(&root, &feature, req, task, &links),
        AgentWork::Reconcile => {
            let synced = s.spec.baseline.requirements.get(&key);
            if synced.is_some_and(|h| *h != req.hash) {
                return Err(CommandError::Failed(format!(
                    "the text of {key} changed since the last sync, so the spec leads: implement it, or mark it in sync to accept the text as it is"
                )));
            }
            let changes = s.spec.code_changes_of(&feature.name, &req.id);
            if changes.is_empty() {
                return Err(CommandError::Failed(format!("the code of {key} has not changed since the last sync")));
            }
            sp::reconcile_prompt(&root, &feature, req, &changes, &links)
        }
    };
    let verb = match work {
        AgentWork::Implement => "Implement",
        AgentWork::Reconcile => "Reconcile",
    };
    let title: String = format!("{verb} {key}: {}", req.display_title()).chars().take(90).collect();
    let id = agents::start(
        s,
        StartRequest {
            backend,
            mode,
            prompt,
            attachments: Vec::new(),
            cwd: None,
            title: Some(title),
            include_file: false,
            include_selection: false,
        },
    )?;
    s.spec.jobs.insert(key.clone(), SpecJob { job: id, work });
    let session = s.agents.job(id).and_then(|j| j.session_key.clone());
    Ok(json!({
        "job": id.0,
        "session": session,
        "feature": feature.name,
        "id": req.id,
        "kind": work.id(),
        "backend": backend.id(),
        "mode": mode_id(mode),
    }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "spec.refresh",
            "Refresh Spec View",
            None,
            "Reads the specs and scans the project for references again",
            Context::Global,
            has_project,
            |s, _| {
                refresh(s);
                ok()
            },
        ),
        spec(
            "spec.list",
            "List Specs",
            None,
            "→ {pending, truncated, specDir, features: [{name, title, dir, files: {requirements, design, tasks}, requirements: [{id, title, status: specChanged|codeChanged|unlinked|linked|inSync, ears, line, links, codeChanges, tasks}], design: [{level, text, line}], tasks: {done, total}, diagnostics: [{file, line, severity, message}]}], counts, codeReferences, unspecified: {files, items}, danglingLinks: [{path, line, reference}], ambiguousLinks}. Lines are 1-based. Starts a scan if none ran yet; `pending` is true until it finishes (over MCP, pass `waitMs` to wait for it)",
            Context::Global,
            always,
            |s, _| -> CmdResult {
                ensure_scan(s);
                Ok(list_json(s))
            },
        ),
        spec(
            "spec.unspecified",
            "List Unspecified Code",
            None,
            "{path?} → {pending, optedIn, total, files: [{path, count, items: [{line, endLine, kind, name}]}]} — public items (Rust pub, Python top-level def/class, TypeScript export, Go exported func/type) that no requirement covers, in files that carry `// spec: required`; `path` limits it to one file. A reference above an item, inside it, or above an item that contains it covers it. Starts a scan if none ran; `pending` is true until it finishes (pass `waitMs` over MCP)",
            Context::Global,
            always,
            |s, p| -> CmdResult {
                ensure_scan(s);
                Ok(unspecified_json(s, p))
            },
        ),
        spec(
            "spec.show",
            "Show Requirement",
            None,
            "{feature, id} → the requirement's full text, status, fingerprint, the code that points at it (with the item each reference is attached to), how that code changed since the last sync (codeChanges: [{path, symbol, line, change: modified|removed|added}]) and the tasks that name it",
            Context::Global,
            has_project,
            |s, p| show_json(s, p),
        ),
        spec(
            "spec.open",
            "Go to Requirement",
            None,
            "{feature, id?, file?: requirements|design|tasks} — opens the requirement (or the file) in the editor",
            Context::Global,
            has_project,
            open_spec,
        ),
        spec(
            "spec.openLink",
            "Go to Linked Code",
            None,
            "{path, line} (1-based) — opens code that points at a requirement",
            Context::Global,
            always,
            |s, p| {
                let path = req_path(p, "path")?;
                let line = req_usize(p, "line")?;
                s.navigate_to_position(&path, line.saturating_sub(1), 0)?;
                ok()
            },
        ),
        spec(
            "spec.new",
            "New Spec…",
            None,
            "{name} — creates specs/<name>/ with requirements.md, design.md and tasks.md (never overwrites) and opens requirements.md",
            Context::Global,
            has_project,
            new_feature,
        ),
        spec(
            "spec.addRequirement",
            "Add Requirement",
            None,
            "{feature, title?, text?} → {id, file, line} — appends a requirement to the feature's requirements.md in its style, with the next free REQ number (project-wide). Goes through the open editor when the file is open; nothing else in the file changes. Refused if `text` would change the structure of the file",
            Context::Global,
            has_project,
            add_requirement,
        ),
        spec(
            "spec.editRequirement",
            "Edit Requirement",
            None,
            "{feature, id, title?, text?} → {id, file, line, changed} — replaces the requirement's title and/or text (a bullet requirement has no title), keeping its id, heading style and every other requirement. A synced requirement then shows as \"spec changed\". Refused if `text` would change the structure of the file (a heading or id typed into it)",
            Context::Global,
            has_project,
            edit_requirement,
        ),
        spec(
            "spec.setTask",
            "Set Task State",
            None,
            "{feature, line (1-based, of the task in tasks.md), state: todo|doing|done} → {changed} — ticks or clears one checkbox",
            Context::Global,
            has_project,
            set_task,
        ),
        spec(
            "spec.markSynced",
            "Mark Requirements In Sync",
            None,
            "{feature, id?} — records the current text of the feature's requirements (or one), and the code that points at them, as implemented: a later edit of the text shows as \"spec changed\", of the code as \"code changed\". Kept in .codecraft/spec-state.json. Fails while the scan is running",
            Context::Global,
            has_project,
            mark_synced,
        ),
        spec(
            "spec.implement",
            "Implement Requirement…",
            None,
            "{feature, id? | task? (line of tasks.md; implements the requirement it names), backend?: claude|codex|grok|antigravity (default: the first installed), mode?: auto|ask|review (default review)} → {job, session, feature, id, kind, backend, mode} — hands the requirement to a coding agent (an ordinary job, see agent.*): it builds it, links the code with `// spec: REQ-n`, adds tests and ticks the task. In review mode its changes come back as a diff to accept or reject. Fails while the scan runs, and while an earlier job for the requirement is still working",
            Context::Global,
            has_project,
            |s, p| start_agent(s, p, AgentWork::Implement),
        ),
        spec(
            "spec.reconcile",
            "Reconcile Requirement…",
            None,
            "{feature, id, backend?, mode?} → {job, session, feature, id, kind, backend, mode} — for a requirement whose linked code changed since the last sync (\"code changed\"): an agent reads the changes, starts its answer with `Classification: refactor|violation|evolution|addition` and, for the last two, edits only the spec files (review mode: a diff of specs/ to accept). Accept the edit, then spec.markSynced. Refused when the text also changed (implement it instead) or the code did not change",
            Context::Global,
            has_project,
            |s, p| start_agent(s, p, AgentWork::Reconcile),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(s: &mut Session, f: &dyn Fn(&Session) -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(15) {
            s.pump();
            if f(s) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        (dir, root, s)
    }

    const REQS: &str = "# Requirements: Login\n\n## REQ-1: Sign in\nWHEN valid credentials are given THE SYSTEM SHALL open a session.\n\n## REQ-2: Lockout\nIF five attempts fail THEN THE SYSTEM SHALL lock the account.\n";

    fn status_of(v: &Value, id: &str) -> String {
        v["features"][0]["requirements"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["id"] == id))
            .and_then(|r| r["status"].as_str())
            .unwrap_or("?")
            .to_string()
    }

    #[test]
    fn scans_specs_and_code_and_resolves_links() {
        let (_d, _root, mut s) = project(&[
            ("specs/login/requirements.md", REQS),
            (
                "specs/login/tasks.md",
                "- [x] 1. Sign in _Requirements: REQ-1_\n- [ ] 2. Lock _Requirements: REQ-2, REQ-8_\n",
            ),
            ("src/auth.rs", "// spec: REQ-1\nfn sign_in() {}\n#[spec(\"login/REQ-1\")]\nfn t() {}\n// spec: REQ-9\n"),
            ("docs/notes.md", "// spec: REQ-2 in a document is an example, not a link\n"),
        ]);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| !s.spec.busy() && !s.spec.features.is_empty()));
        let v = s.execute("spec.list", &json!({})).unwrap();
        assert_eq!(v["features"][0]["name"], "login");
        assert_eq!(v["features"][0]["tasks"], json!({ "done": 1, "total": 2 }));
        assert_eq!(status_of(&v, "REQ-1"), "linked");
        assert_eq!(status_of(&v, "REQ-2"), "unlinked", "documents quote examples; they are not code");
        assert_eq!(v["features"][0]["requirements"][0]["links"], 2);
        assert_eq!(v["features"][0]["requirements"][1]["tasks"], 1);
        assert_eq!(v["codeReferences"], 3);
        assert_eq!(v["danglingLinks"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["danglingLinks"][0]["reference"], "REQ-9");
        assert_eq!(v["danglingLinks"][0]["line"], 5);
        let diag = v["features"][0]["diagnostics"].as_array().unwrap();
        assert!(diag.iter().any(|d| d["message"].as_str().is_some_and(|m| m.starts_with("REQ-8"))), "{diag:?}");
    }

    #[test]
    fn mark_synced_then_an_edit_shows_spec_changed() {
        let (_d, root, mut s) = project(&[("specs/login/requirements.md", REQS), ("src/a.rs", "// spec: REQ-1\n")]);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| !s.spec.busy() && !s.spec.features.is_empty()));
        let r = s.execute("spec.markSynced", &json!({ "feature": "login" })).unwrap();
        assert_eq!(r["recorded"], 2);
        let v = s.execute("spec.list", &json!({})).unwrap();
        assert_eq!((status_of(&v, "REQ-1").as_str(), status_of(&v, "REQ-2").as_str()), ("inSync", "unlinked"));
        // A re-wrapped sentence is not a change; a new word is.
        let rewrapped = REQS.replace("credentials are given", "credentials\n  are given");
        std::fs::write(root.join("specs/login/requirements.md"), &rewrapped).unwrap();
        files_changed(&mut s, &[root.join("specs/login/requirements.md")]);
        assert!(wait(&mut s, &|s| !s.spec.busy()));
        assert_eq!(status_of(&s.execute("spec.list", &json!({})).unwrap(), "REQ-1"), "inSync");
        let edited = REQS.replace("open a session", "open a session within 2 seconds");
        std::fs::write(root.join("specs/login/requirements.md"), &edited).unwrap();
        files_changed(&mut s, &[root.join("specs/login/requirements.md")]);
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-1") == "specChanged"));
        let shown = s.execute("spec.show", &json!({ "feature": "login", "id": "REQ-1" })).unwrap();
        assert_eq!(shown["status"], "specChanged");
        assert_ne!(shown["hash"], shown["syncedHash"]);
        assert_eq!(shown["links"][0]["line"], 1);
        // Marking one requirement leaves the others alone; bad names are rejected.
        assert_eq!(s.execute("spec.markSynced", &json!({ "feature": "login", "id": "REQ-1" })).unwrap()["recorded"], 1);
        assert_eq!(status_of(&s.execute("spec.list", &json!({})).unwrap(), "REQ-1"), "inSync");
        assert!(s.execute("spec.markSynced", &json!({ "feature": "login", "id": "REQ-5" })).is_err());
        assert!(s.execute("spec.markSynced", &json!({ "feature": "../x" })).is_err());
        assert!(s.execute("spec.markSynced", &json!({ "feature": "nope" })).is_err());
    }

    const AUTH: &str = "// spec: REQ-1\nfn sign_in(user: &str) -> bool {\n    check(user)\n}\n\nfn unrelated() {}\n";

    fn show(s: &mut Session, id: &str) -> Value {
        s.execute("spec.show", &json!({ "feature": "login", "id": id })).unwrap()
    }

    #[test]
    fn a_change_to_the_linked_code_shows_as_code_changed_and_comments_do_not() {
        let (_d, root, mut s) = project(&[("specs/login/requirements.md", REQS), ("src/auth.rs", AUTH)]);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| !s.spec.busy() && !s.spec.features.is_empty()));
        let linked = show(&mut s, "REQ-1");
        assert_eq!(linked["links"][0]["symbol"]["signature"], "fn sign_in(user: &str) -> bool");
        assert_eq!(linked["links"][0]["symbol"]["startLine"], 2);
        assert_eq!(linked["links"][0]["symbol"]["endLine"], 4);
        s.execute("spec.markSynced", &json!({ "feature": "login" })).unwrap();
        assert_eq!(status_of(&list_json(&s), "REQ-1"), "inSync");
        let path = root.join("src/auth.rs");

        // Comments, layout and unrelated code are not changes.
        let touched = AUTH
            .replace("    check(user)\n", "    // be careful\n    check( user )\n")
            .replace("fn unrelated() {}", "fn unrelated() { 1; }");
        std::fs::write(&path, touched).unwrap();
        files_changed(&mut s, std::slice::from_ref(&path));
        assert!(wait(&mut s, &|s| !s.spec.busy()));
        assert_eq!(status_of(&list_json(&s), "REQ-1"), "inSync");

        // The linked function's code is.
        std::fs::write(&path, AUTH.replace("check(user)", "check_all(user)")).unwrap();
        files_changed(&mut s, std::slice::from_ref(&path));
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-1") == "codeChanged"));
        let v = list_json(&s);
        assert_eq!(v["counts"]["codeChanged"], 1);
        assert_eq!(v["features"][0]["requirements"][0]["codeChanges"], 1);
        let shown = show(&mut s, "REQ-1");
        assert_eq!(shown["status"], "codeChanged");
        assert_eq!(
            shown["codeChanges"],
            json!([{ "path": "src/auth.rs", "symbol": "fn sign_in(user: &str) -> bool", "line": 1, "change": "modified" }])
        );

        // Syncing again accepts the new code.
        s.execute("spec.markSynced", &json!({ "feature": "login", "id": "REQ-1" })).unwrap();
        assert_eq!(status_of(&list_json(&s), "REQ-1"), "inSync");

        // Removing the reference is a change too (the spec no longer says where it is implemented).
        std::fs::write(&path, AUTH.replace("// spec: REQ-1\n", "").replace("check(user)", "check_all(user)")).unwrap();
        files_changed(&mut s, std::slice::from_ref(&path));
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-1") == "codeChanged"));
        assert_eq!(show(&mut s, "REQ-1")["codeChanges"][0]["change"], "removed");

        // A spec change still wins.
        let edited = REQS.replace("open a session", "open a session within 2 seconds");
        std::fs::write(root.join("specs/login/requirements.md"), edited).unwrap();
        files_changed(&mut s, &[root.join("specs/login/requirements.md")]);
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-1") == "specChanged"));
    }

    #[test]
    fn code_is_only_compared_for_requirements_synced_with_it() {
        // A baseline from before code was fingerprinted: the requirement is synced, its code is not recorded.
        let hash = sp::parse_requirements(REQS).requirements[0].hash.clone();
        let old = format!(r#"{{"version": 1, "requirements": {{"login/REQ-1": "{hash}"}}}}"#);
        let (_d, root, mut s) = project(&[
            ("specs/login/requirements.md", REQS),
            ("src/auth.rs", AUTH),
            (".codecraft/spec-state.json", &old),
        ]);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| !s.spec.busy() && !s.spec.features.is_empty()));
        assert_eq!(status_of(&list_json(&s), "REQ-1"), "inSync");
        let path = root.join("src/auth.rs");
        std::fs::write(&path, AUTH.replace("check(user)", "check_all(user)")).unwrap();
        files_changed(&mut s, &[path]);
        assert!(wait(&mut s, &|s| !s.spec.busy()));
        assert_eq!(status_of(&list_json(&s), "REQ-1"), "inSync", "nothing was recorded to compare with");
    }

    #[test]
    fn mark_synced_waits_for_the_scan() {
        let (_d, _root, mut s) = project(&[("specs/login/requirements.md", REQS), ("src/auth.rs", AUTH)]);
        let early = s.execute("spec.markSynced", &json!({ "feature": "login" }));
        assert!(early.is_err_and(|e| e.to_string().contains("still being scanned")));
        assert!(s.spec.started, "asking started the scan");
        assert!(wait(&mut s, &|s| s.spec.scanned() && !s.spec.features.is_empty()));
        assert!(s.execute("spec.markSynced", &json!({ "feature": "login" })).is_ok());
    }

    #[test]
    fn code_changes_are_picked_up_from_disk_and_from_unsaved_buffers() {
        let (_d, root, mut s) = project(&[("specs/login/requirements.md", REQS), ("src/a.rs", "fn a() {}\n")]);
        s.ws.layout.show(ToolWindowId::Spec);
        assert!(wait(&mut s, &|s| !s.spec.busy() && s.spec.started && !s.spec.features.is_empty()));
        assert_eq!(status_of(&list_json(&s), "REQ-1"), "unlinked");
        // The file changes on disk.
        std::fs::write(root.join("src/b.rs"), "// spec: REQ-1\n").unwrap();
        files_changed(&mut s, &[root.join("src/b.rs")]);
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-1") == "linked"));
        std::fs::remove_file(root.join("src/b.rs")).unwrap();
        files_changed(&mut s, &[root.join("src/b.rs")]);
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-1") == "unlinked"));
        // An unsaved edit in an open editor counts while the Spec View is shown.
        s.execute("spec.openLink", &json!({ "path": root.join("src/a.rs"), "line": 1 })).unwrap();
        s.execute("editor.type", &json!({ "text": "// spec: REQ-2\n" })).unwrap();
        assert!(wait(&mut s, &|s| status_of(&list_json(s), "REQ-2") == "linked"));
    }

    fn scanned_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Session) {
        let (d, root, mut s) = project(files);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| s.spec.scanned() && !s.spec.features.is_empty()));
        (d, root, s)
    }

    #[test]
    fn requirements_are_added_and_edited_in_place_and_a_synced_one_shows_spec_changed() {
        let (_d, root, mut s) = scanned_project(&[
            ("specs/login/requirements.md", REQS),
            ("specs/billing/requirements.md", "## REQ-7: Invoice\nTHE SYSTEM SHALL bill.\n"),
            ("src/a.rs", "// spec: REQ-1\nfn a() {}\n"),
        ]);
        s.execute("spec.markSynced", &json!({ "feature": "login" })).unwrap();
        let file = root.join("specs/login/requirements.md");

        // Added: the next number is project-wide (billing has REQ-7), the rest of the file is untouched.
        let added = s
            .execute(
                "spec.addRequirement",
                &json!({ "feature": "login", "title": "Reset", "text": "WHEN asked THE SYSTEM SHALL mail a link." }),
            )
            .unwrap();
        assert_eq!(added["id"], "REQ-8");
        assert_eq!(added["line"], 9);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with(REQS) && text.ends_with("## REQ-8: Reset\nWHEN asked THE SYSTEM SHALL mail a link.\n"),
            "{text}"
        );
        assert!(wait(&mut s, &|s| s.spec.feature("login").is_some_and(|f| f.requirements.len() == 3) && !s.spec.busy()));

        // Edited: only the requested requirement changes; a synced one becomes "spec changed".
        let edited = s
            .execute("spec.editRequirement", &json!({ "feature": "login", "id": "REQ-1", "text": "WHEN valid credentials are given THE SYSTEM SHALL open a session within 2 s." }))
            .unwrap();
        assert_eq!(edited["changed"], true);
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, text.replace("open a session.", "open a session within 2 s."));
        let status = |s: &mut Session, id: &str| show(s, id)["status"].as_str().unwrap_or("?").to_string();
        assert!(wait(&mut s, &|s| s.spec.feature("login").is_some_and(|f| {
            f.requirement("REQ-1").is_some_and(|r| s.spec.status_of(f, r) == sp::SpecStatus::SpecChanged)
        })));
        assert_eq!(status(&mut s, "REQ-2"), "unlinked");
        // The same text again is not a change; a title-only edit keeps the text.
        let again = s.execute("spec.editRequirement", &json!({ "feature": "login", "id": "REQ-1", "text": "WHEN valid credentials are given THE SYSTEM SHALL open a session within 2 s." })).unwrap();
        assert_eq!(again["changed"], false);
        s.execute("spec.editRequirement", &json!({ "feature": "login", "id": "REQ-2", "title": "Lock out" })).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("## REQ-2: Lock out\nIF five attempts"));
    }

    #[test]
    fn edits_that_would_change_the_file_s_structure_or_name_nothing_are_refused() {
        let (_d, root, mut s) = scanned_project(&[("specs/login/requirements.md", REQS)]);
        let file = root.join("specs/login/requirements.md");
        let bad = [
            ("spec.editRequirement", json!({ "feature": "login", "id": "REQ-1", "text": "x\n## REQ-5: Sneaky\ny" })),
            ("spec.editRequirement", json!({ "feature": "login", "id": "REQ-9", "text": "x" })),
            ("spec.editRequirement", json!({ "feature": "login", "id": "REQ-1" })),
            ("spec.editRequirement", json!({ "feature": "nope", "id": "REQ-1", "text": "x" })),
            ("spec.addRequirement", json!({ "feature": "login" })),
            ("spec.addRequirement", json!({ "feature": "login", "title": "x", "text": "y\n## REQ-1: Dup" })),
            ("spec.addRequirement", json!({ "feature": "../x", "title": "x" })),
            ("spec.setTask", json!({ "feature": "login", "line": 1, "state": "done" })),
            ("spec.setTask", json!({ "feature": "login", "line": 1, "state": "maybe" })),
        ];
        for (id, params) in bad {
            assert!(s.execute(id, &params).is_err(), "{id} {params}");
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), REQS, "nothing was written");
    }

    #[test]
    fn an_open_requirements_file_is_edited_through_the_editor_and_can_be_undone() {
        let (_d, root, mut s) = scanned_project(&[("specs/login/requirements.md", REQS)]);
        let file = root.join("specs/login/requirements.md");
        s.execute("spec.open", &json!({ "feature": "login", "id": "REQ-2" })).unwrap();
        s.execute("spec.editRequirement", &json!({ "feature": "login", "id": "REQ-1", "title": "Login" })).unwrap();
        let in_editor = s.ws.active().map(|(_, d)| d.doc.text()).unwrap();
        assert!(in_editor.contains("## REQ-1: Login\n"), "{in_editor}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), in_editor, "and it was saved");
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(s.ws.active().map(|(_, d)| d.doc.text()).unwrap(), REQS, "one undo step");
    }

    #[test]
    fn task_boxes_are_set_through_the_command() {
        let tasks = "# Tasks\n\n- [ ] 1. Sign in _Requirements: REQ-1_\n- [x] 2. Lock\n";
        let (_d, root, mut s) =
            scanned_project(&[("specs/login/requirements.md", REQS), ("specs/login/tasks.md", tasks)]);
        s.execute("spec.setTask", &json!({ "feature": "login", "line": 3, "state": "done" })).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("specs/login/tasks.md")).unwrap(),
            tasks.replace("- [ ] 1.", "- [x] 1.")
        );
        assert!(
            wait(&mut s, &|s| s.spec.feature("login").is_some_and(|f| f.task_progress() == (2, 2)) && !s.spec.busy())
        );
        let r = s.execute("spec.setTask", &json!({ "feature": "login", "line": 4, "state": "todo" })).unwrap();
        assert_eq!(r["changed"], true);
        assert!(s.execute("spec.setTask", &json!({ "feature": "login", "line": 2, "state": "done" })).is_err());
    }

    const API: &str =
        "// spec: required\n\n// spec: REQ-1\npub fn covered() {}\n\npub fn bare() {}\n\nfn private() {}\n";

    fn unspecified(s: &mut Session) -> Value {
        s.execute("spec.unspecified", &json!({})).unwrap()
    }

    #[test]
    fn public_items_no_requirement_covers_are_listed_for_files_that_ask() {
        let (_d, root, mut s) = scanned_project(&[
            ("specs/login/requirements.md", REQS),
            ("src/api.rs", API),
            ("src/other.rs", "pub fn not_asked_for() {}\n"),
            ("docs/notes.md", "// spec: required\npub fn quoted() {}\n"),
        ]);
        let list = list_json(&s);
        assert_eq!(list["unspecified"], json!({ "files": 1, "items": 1 }), "{list:#}");
        let v = unspecified(&mut s);
        assert_eq!(v["optedIn"], 1);
        assert_eq!(v["total"], 1);
        assert_eq!(v["files"][0]["path"], json!(root.join("src/api.rs")));
        assert_eq!(v["files"][0]["items"][0]["name"], "bare");
        assert_eq!(v["files"][0]["items"][0]["kind"], "fn");
        assert_eq!(v["files"][0]["items"][0]["line"], 6);
        // A path filter; an unknown one lists nothing.
        let one = s.execute("spec.unspecified", &json!({ "path": "src/api.rs" })).unwrap();
        assert_eq!(one["files"].as_array().map(Vec::len), Some(1));
        assert_eq!(s.execute("spec.unspecified", &json!({ "path": "src/other.rs" })).unwrap()["files"], json!([]));

        // Covering the item (on disk) clears it.
        let path = root.join("src/api.rs");
        std::fs::write(&path, API.replace("pub fn bare", "// spec: REQ-2\npub fn bare")).unwrap();
        files_changed(&mut s, std::slice::from_ref(&path));
        assert!(wait(&mut s, &|s| !s.spec.busy() && s.spec.unspecified_count() == 0));
        assert_eq!(unspecified(&mut s)["files"], json!([]));
        assert_eq!(unspecified(&mut s)["optedIn"], 1, "the file still asks");
        // Dropping the marker stops asking.
        std::fs::write(&path, API.replace("// spec: required\n", "")).unwrap();
        files_changed(&mut s, std::slice::from_ref(&path));
        assert!(wait(&mut s, &|s| !s.spec.busy() && s.spec.required.is_empty()));
    }

    #[test]
    fn an_unsaved_edit_covers_an_item_while_the_view_is_shown() {
        let (_d, root, mut s) = project(&[("specs/login/requirements.md", REQS), ("src/api.rs", API)]);
        s.ws.layout.show(ToolWindowId::Spec);
        assert!(wait(&mut s, &|s| s.spec.scanned() && s.spec.unspecified_count() == 1));
        s.execute("spec.openLink", &json!({ "path": root.join("src/api.rs"), "line": 6 })).unwrap();
        s.execute("editor.type", &json!({ "text": "// spec: REQ-2\n" })).unwrap();
        assert!(wait(&mut s, &|s| s.spec.unspecified_count() == 0));
        assert_eq!(std::fs::read_to_string(root.join("src/api.rs")).unwrap(), API, "nothing was saved");
    }

    #[test]
    fn new_spec_scaffolds_opens_and_refuses_duplicates() {
        let (_d, root, mut s) = project(&[("specs/login/requirements.md", REQS)]);
        let r = s.execute("spec.new", &json!({ "name": "Billing Plans" })).unwrap();
        assert_eq!(r["feature"], "billing-plans");
        assert!(root.join("specs/billing-plans/tasks.md").is_file());
        let text = std::fs::read_to_string(root.join("specs/billing-plans/requirements.md")).unwrap();
        assert!(text.contains("## REQ-3:"), "ids continue across features: {text}");
        assert!(s.ws.layout.is_visible(ToolWindowId::Spec));
        assert_eq!(
            s.ws.active().and_then(|(_, d)| d.path.clone()),
            Some(root.join("specs/billing-plans/requirements.md"))
        );
        assert!(wait(&mut s, &|s| s.spec.features.len() == 2 && !s.spec.busy()));
        assert!(s.execute("spec.new", &json!({ "name": "billing plans" })).is_err());
        assert!(s.execute("spec.new", &json!({ "name": "***" })).is_err());
        assert!(s.execute("spec.new", &json!({})).is_err());
    }

    #[test]
    fn open_goes_to_the_requirement_and_rejects_unknowns() {
        let (_d, root, mut s) = project(&[("specs/login/requirements.md", REQS), ("specs/login/design.md", "# D\n")]);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| !s.spec.busy() && !s.spec.features.is_empty()));
        let r = s.execute("spec.open", &json!({ "feature": "login", "id": "REQ-2" })).unwrap();
        assert_eq!(r["line"], 6);
        assert_eq!(s.ws.active().and_then(|(_, d)| d.path.clone()), Some(root.join("specs/login/requirements.md")));
        s.execute("spec.open", &json!({ "feature": "login", "file": "design" })).unwrap();
        assert!(s.execute("spec.open", &json!({ "feature": "login", "file": "tasks" })).is_err(), "no tasks.md");
        assert!(s.execute("spec.open", &json!({ "feature": "login", "id": "REQ-9" })).is_err());
        assert!(s.execute("spec.open", &json!({ "feature": "nope" })).is_err());
        assert!(s.execute("spec.open", &json!({ "feature": "login", "file": "x" })).is_err());
        assert!(s.execute("spec.show", &json!({ "feature": "login", "id": "REQ-9" })).is_err());
    }

    #[test]
    fn a_project_without_specs_lists_nothing_and_commands_need_a_project() {
        let (_d, _root, mut s) = project(&[("src/a.rs", "fn a() {}\n")]);
        s.execute("spec.list", &json!({})).unwrap();
        assert!(wait(&mut s, &|s| s.spec.started && !s.spec.busy()));
        let v = s.execute("spec.list", &json!({})).unwrap();
        assert_eq!(v["features"], json!([]));
        assert_eq!(v["codeReferences"], 0);
        let mut empty = Session::new();
        empty.lsp_enabled = false;
        assert!(empty.execute("spec.new", &json!({ "name": "x" })).is_err());
        assert!(empty.execute("spec.refresh", &json!({})).is_err());
        assert_eq!(empty.execute("spec.list", &json!({})).unwrap()["features"], json!([]));
    }
}
