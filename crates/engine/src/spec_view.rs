//! The Spec View: the project's specs (`specs/<feature>/{requirements,design,tasks}.md`), the
//! code that points at their requirements (`// spec: REQ-12`, `#[spec("REQ-12")]`), and for each
//! requirement whether it is in sync (`codecraft_spec` does the reading and the rules).
//!
//! The project is scanned on a worker thread when the tool window is first shown (or an agent
//! asks); files the watcher reports as changed are re-read; open documents are re-scanned for
//! references as they change. Nothing here edits code or rewrites a spec file: the only writes
//! are `spec.new` (a scaffold in a new directory) and `spec.markSynced` (the private baseline
//! in `.codecraft/spec-state.json`). Implementing a changed requirement stays with the agent
//! tools and the user's review.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use regex::Regex;
use serde_json::{Value, json};

use codecraft_spec as sp;
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::{opt_str, req_path, req_str, req_usize};
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
    truncated: bool,
}

enum Msg {
    /// A full scan finished.
    Full(Scan),
    /// The spec files were read again.
    Features(Vec<sp::Feature>, sp::Baseline),
    /// Some code files were rescanned (an empty list removes the file).
    Files(Vec<(PathBuf, Vec<sp::CodeLink>)>),
}

#[derive(Default)]
pub struct SpecState {
    pub features: Vec<sp::Feature>,
    /// References in code, by file.
    pub files: BTreeMap<PathBuf, Vec<sp::CodeLink>>,
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

    pub fn feature(&self, name: &str) -> Option<&sp::Feature> {
        self.features.iter().find(|f| f.name == name)
    }

    /// Where a requirement stands: the baseline, the links and the text decide.
    pub fn status_of(&self, feature: &sp::Feature, req: &sp::Requirement) -> sp::SpecStatus {
        let synced = self.baseline.requirements.get(&sp::requirement_key(&feature.name, &req.id)).map(String::as_str);
        sp::status(req, self.index.links_of(&feature.name, &req.id).len(), synced)
    }

    fn rebuild_index(&mut self) {
        self.index = sp::build_index(&self.features, &self.files);
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

/// The references on the lines `find_in_files` matched, by file.
fn scan_code(root: &Path, cancel: &AtomicBool) -> (BTreeMap<PathBuf, Vec<sp::CodeLink>>, bool) {
    let mut out: BTreeMap<PathBuf, Vec<sp::CodeLink>> = BTreeMap::new();
    let Some(re) = reference_regex() else { return (out, false) };
    let opts =
        codecraft_fs::FindOptions { max_results: MAX_MATCHES, max_file_size: MAX_FILE_BYTES, ..Default::default() };
    let Ok(matches) = codecraft_fs::find_in_files(root, re, &opts, cancel) else { return (out, false) };
    let truncated = matches.len() >= MAX_MATCHES;
    for m in matches {
        if !is_code_path(root, &m.path) {
            continue;
        }
        let line = u32::try_from(m.line.saturating_add(1)).unwrap_or(u32::MAX);
        for mut link in sp::scan_links(&m.line_text) {
            link.line = line;
            out.entry(m.path.clone()).or_default().push(link);
        }
    }
    (out, truncated)
}

fn scan_file(path: &Path) -> Vec<sp::CodeLink> {
    if std::fs::metadata(path).map(|m| !m.is_file() || m.len() > MAX_FILE_BYTES).unwrap_or(true) {
        return Vec::new();
    }
    codecraft_fs::read_text_file(path).map(|t| sp::scan_links(&t)).unwrap_or_default()
}

/// Read everything (blocking; runs on a worker thread).
fn scan_all(root: &Path, cancel: &AtomicBool) -> Scan {
    let features = sp::load_features(root);
    let baseline = sp::load_baseline(root);
    let (files, truncated) = scan_code(root, cancel);
    Scan { features, baseline, files, truncated }
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
        let out: Vec<(PathBuf, Vec<sp::CodeLink>)> = code
            .into_iter()
            .map(|p| {
                let links = scan_file(&p);
                (p, links)
            })
            .collect();
        let _ = tx.send(Msg::Files(out));
    });
    if spawned.is_ok() {
        s.spec.others.push(rx);
    }
}

fn set_file(st: &mut SpecState, path: PathBuf, links: Vec<sp::CodeLink>) -> bool {
    if links.is_empty() {
        st.files.remove(&path).is_some()
    } else if st.files.get(&path) != Some(&links) {
        st.files.insert(path, links);
        true
    } else {
        false
    }
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
                for (p, links) in list {
                    if s.ws.doc_by_path(&p).is_none() {
                        reindex |= set_file(&mut s.spec, p, links);
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
            updates.push((p.clone(), rev, sp::scan_links(&od.doc.text())));
        }
        for (p, rev, links) in updates {
            s.spec.doc_revs.insert(p.clone(), rev);
            reindex |= set_file(&mut s.spec, p, links);
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
                "tasks": tasks,
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
        "tasks": tasks,
    }))
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

fn mark_synced(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let name = req_str(p, "feature")?;
    if !sp::is_feature_name(name) {
        return Err(CommandError::BadParams(format!("`{name}` is not a feature name")));
    }
    // The files as they are on disk now, not the (possibly older) scan.
    let feature = sp::load_feature(&root.join(sp::SPEC_DIR).join(name))
        .ok_or_else(|| CommandError::BadParams(format!("no feature `{name}`")))?;
    let mut baseline = sp::load_baseline(&root);
    let n = sp::mark_synced(&mut baseline, &feature, opt_str(p, "id")).map_err(|e| cmd_error(&e))?;
    sp::save_baseline(&root, &baseline).map_err(|e| cmd_error(&e))?;
    s.spec.baseline = baseline;
    s.spec.rebuild_index();
    s.status = format!("Marked {n} requirement{} in sync", if n == 1 { "" } else { "s" });
    Ok(json!({ "feature": name, "recorded": n }))
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
            "→ {pending, truncated, specDir, features: [{name, title, dir, files: {requirements, design, tasks}, requirements: [{id, title, status: specChanged|unlinked|linked|inSync, ears, line, links, tasks}], design: [{level, text, line}], tasks: {done, total}, diagnostics: [{file, line, severity, message}]}], counts, codeReferences, danglingLinks: [{path, line, reference}], ambiguousLinks}. Lines are 1-based. Starts a scan if none ran yet; `pending` is true until it finishes (over MCP, pass `waitMs` to wait for it)",
            Context::Global,
            always,
            |s, _| -> CmdResult {
                let current = s.ws.project.as_ref().map(|p| p.root.clone());
                if current.is_some() && (!s.spec.started || s.spec.project != current) {
                    refresh(s);
                }
                Ok(list_json(s))
            },
        ),
        spec(
            "spec.show",
            "Show Requirement",
            None,
            "{feature, id} → the requirement's full text, status, fingerprint, the code that points at it and the tasks that name it",
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
            "spec.markSynced",
            "Mark Requirements In Sync",
            None,
            "{feature, id?} — records the current text of the feature's requirements (or one) as implemented: a later edit shows as \"spec changed\". Kept in .codecraft/spec-state.json",
            Context::Global,
            has_project,
            mark_synced,
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
