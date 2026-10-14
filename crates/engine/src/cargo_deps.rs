//! Cargo.toml dependency intelligence (M12 track RS): crate name and version completion from
//! crates.io, a "newer version available" inspection on every dependency with a quick-fix, and
//! Update All Dependencies through the refactoring preview.
//!
//! Lookups run on worker threads ([`index`]) and are cached on disk with a time to live; the engine
//! drains their answers in [`pump`]. Offline (or with lookups turned off) completion offers what
//! the cache knows and the inspection stays quiet: never an error dialog. Network access is off
//! in a bare [`Session`]: the desktop app turns [`DepsState::online`] and [`DepsState::auto`] on, so
//! tests and the headless CLI never reach crates.io.

pub mod index;
pub mod manifest;
pub mod semver;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde_json::{Value, json};

use codecraft_lsp::lsp_types as lt;
use codecraft_text::Position;
use codecraft_workspace::{EditorId, Problem, ProblemSource, Severity};

use crate::popup::{ActionItem, CompletionItem, CompletionPopup};
use crate::{CommandError, Popup, Session};
use index::{CrateVersions, Endpoint, SearchHit};
use manifest::{CompletionSite, Dependency};
use semver::Version;

/// Inspection key of the dependency problems in the Problems view.
pub const INSPECTION: &str = "cargo-dependencies";
/// Lookups running at once.
const MAX_IN_FLIGHT: usize = 6;
/// Lookups queued at most (older ones are dropped).
const MAX_QUEUED: usize = 400;
/// A failed lookup is not retried automatically for this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Quiet time after an edit before an edited manifest is re-inspected.
const DEBOUNCE: Duration = Duration::from_millis(600);
/// Versions offered by version completion.
const MAX_VERSION_ITEMS: usize = 200;

/// A lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum JobKey {
    /// The versions of a crate (lowercased name).
    Versions(String),
    /// A crates.io name search.
    Search(String),
}

enum JobDone {
    Versions(String, Result<CrateVersions, String>),
    Search(String, Result<Vec<SearchHit>, String>),
}

/// A completion popup waiting for a lookup.
#[derive(Clone, Debug)]
struct Waiting {
    editor: EditorId,
    anchor: usize,
    key: JobKey,
}

/// Inspection bookkeeping of one manifest.
#[derive(Clone, Debug)]
struct Analyzed {
    revision: u64,
    changed: Instant,
    done: bool,
}

pub struct DepsState {
    /// Network lookups allowed (the desktop app turns this on; off in tests and the CLI).
    pub online: bool,
    /// Inspect open manifests in the background (the desktop app turns this on).
    pub auto: bool,
    /// The "newer version available" inspection (Settings could turn it off).
    pub check_updates: bool,
    pub index_url: String,
    pub api_url: String,
    /// Instead of `$CODECRAFT_HOME/cache/crates-index` (tests, `Session::isolate_home`).
    pub cache_dir_override: Option<PathBuf>,
    /// How long cached index entries stay fresh.
    pub ttl: Duration,
    crates: HashMap<String, CrateVersions>,
    searches: HashMap<String, Vec<SearchHit>>,
    in_flight: HashSet<JobKey>,
    queue: VecDeque<(JobKey, bool)>,
    failed: HashMap<JobKey, Instant>,
    tx: Sender<JobDone>,
    rx: Receiver<JobDone>,
    waiting: Option<Waiting>,
    analyzed: HashMap<PathBuf, Analyzed>,
    /// The last lookup error (status bar / agents).
    pub last_error: Option<String>,
}

impl Default for DepsState {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            online: false,
            auto: false,
            check_updates: true,
            index_url: index::DEFAULT_INDEX_URL.into(),
            api_url: index::DEFAULT_API_URL.into(),
            cache_dir_override: None,
            ttl: Duration::from_secs(6 * 3600),
            crates: HashMap::new(),
            searches: HashMap::new(),
            in_flight: HashSet::new(),
            queue: VecDeque::new(),
            failed: HashMap::new(),
            tx,
            rx,
            waiting: None,
            analyzed: HashMap::new(),
            last_error: None,
        }
    }
}

impl DepsState {
    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty() || !self.queue.is_empty() || self.analyzed.values().any(|a| !a.done)
    }

    /// The disk cache directory.
    pub fn cache_dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.cache_dir_override {
            return Some(d.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home).join("cache").join("crates-index"));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".codecraft").join("cache").join("crates-index"))
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            index_url: self.index_url.clone(),
            api_url: self.api_url.clone(),
            cache_dir: self.cache_dir(),
            ttl: self.ttl,
            online: self.online,
        }
    }

    /// What is known about a crate (any case).
    pub fn crate_versions(&self, name: &str) -> Option<&CrateVersions> {
        self.crates.get(&name.to_ascii_lowercase())
    }

    /// Remember versions (tests and embedders can seed the cache this way).
    pub fn insert_versions(&mut self, entry: CrateVersions) {
        self.crates.insert(entry.name.to_ascii_lowercase(), entry);
        self.mark_dirty();
    }

    fn mark_dirty(&mut self) {
        for a in self.analyzed.values_mut() {
            a.done = false;
            a.changed = Instant::now().checked_sub(DEBOUNCE).unwrap_or_else(Instant::now);
        }
    }

    fn recently_failed(&self, key: &JobKey) -> bool {
        self.failed.get(key).is_some_and(|t| t.elapsed() < RETRY_AFTER)
    }

    /// Start (or queue) a lookup unless it is running, known, or failed a moment ago.
    pub(crate) fn request(&mut self, key: JobKey, force: bool) -> bool {
        if self.in_flight.contains(&key) || (!force && self.recently_failed(&key)) {
            return false;
        }
        if !force {
            let known = match &key {
                JobKey::Versions(n) => self.crates.contains_key(n),
                JobKey::Search(q) => self.searches.contains_key(q),
            };
            if known {
                return false;
            }
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            if !self.queue.iter().any(|(k, _)| *k == key) {
                if self.queue.len() >= MAX_QUEUED {
                    self.queue.pop_front();
                }
                self.queue.push_back((key, force));
            }
            return true;
        }
        let ep = self.endpoint();
        let tx = self.tx.clone();
        let job = key.clone();
        let spawned = std::thread::Builder::new().name("codecraft-crates-io".into()).spawn(move || {
            let done = match job {
                JobKey::Versions(name) => {
                    let r = index::versions(&ep, &name, force);
                    JobDone::Versions(name, r)
                }
                JobKey::Search(q) => {
                    let r = index::search(&ep, &q);
                    JobDone::Search(q, r)
                }
            };
            let _ = tx.send(done);
        });
        match spawned {
            Ok(_) => {
                self.in_flight.insert(key);
                true
            }
            Err(e) => {
                self.last_error = Some(format!("cannot start a crates.io lookup: {e}"));
                false
            }
        }
    }
}

// ---------------------------------------------------------------- pump

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let mut finished: Vec<JobKey> = Vec::new();
    while let Ok(done) = s.rust.deps.rx.try_recv() {
        changed = true;
        let d = &mut s.rust.deps;
        match done {
            JobDone::Versions(name, r) => {
                let key = JobKey::Versions(name.to_ascii_lowercase());
                d.in_flight.remove(&key);
                match r {
                    Ok(entry) => {
                        d.failed.remove(&key);
                        d.crates.insert(name.to_ascii_lowercase(), entry);
                    }
                    Err(e) => {
                        d.failed.insert(key.clone(), Instant::now());
                        d.last_error = Some(e);
                    }
                }
                finished.push(key);
            }
            JobDone::Search(q, r) => {
                let key = JobKey::Search(q.clone());
                d.in_flight.remove(&key);
                match r {
                    Ok(hits) => {
                        d.failed.remove(&key);
                        if d.searches.len() > 500 {
                            d.searches.clear();
                        }
                        d.searches.insert(q, hits);
                    }
                    Err(e) => {
                        d.failed.insert(key.clone(), Instant::now());
                        d.last_error = Some(e);
                    }
                }
                finished.push(key);
            }
        }
    }
    // Start queued lookups.
    while s.rust.deps.in_flight.len() < MAX_IN_FLIGHT {
        let Some((key, force)) = s.rust.deps.queue.pop_front() else { break };
        s.rust.deps.request(key, force);
    }
    if !finished.is_empty() {
        if finished.iter().any(|k| matches!(k, JobKey::Versions(_))) {
            s.rust.deps.mark_dirty();
        }
        refresh_waiting_completion(s, &finished);
    }
    changed |= analyze_open_manifests(s);
    changed
}

/// A lookup a completion popup waited for arrived: rebuild the popup.
fn refresh_waiting_completion(s: &mut Session, finished: &[JobKey]) {
    let Some(w) = s.rust.deps.waiting.clone() else { return };
    if !finished.contains(&w.key) {
        return;
    }
    s.rust.deps.waiting = None;
    let still_open = matches!(&s.popup, Some(Popup::Completion(c)) if c.editor == w.editor && c.anchor == w.anchor);
    let active = s.ws.active().map(|(e, _)| e.id) == Some(w.editor);
    if still_open && active {
        let auto = matches!(&s.popup, Some(Popup::Completion(c)) if c.auto);
        let _ = complete(s, auto);
    }
}

// ---------------------------------------------------------------- helpers

/// Whether a path is a Cargo manifest.
pub fn is_manifest(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml")
}

/// Whether the active editor shows a Cargo.toml.
pub(crate) fn is_cargo_toml_editor(s: &Session) -> bool {
    s.ws.active().and_then(|(_, d)| d.path.as_deref()).is_some_and(is_manifest)
}

/// `check`, or OK in a Cargo.toml editor.
pub(crate) fn or_cargo_toml(s: &Session, check: fn(&Session) -> Result<(), String>) -> Result<(), String> {
    if is_cargo_toml_editor(s) { Ok(()) } else { check(s) }
}

/// The manifest to act on: `path`, else the active Cargo.toml, else the project's root manifest.
pub(crate) fn target_manifest(s: &Session, path: Option<PathBuf>) -> Result<(PathBuf, String), CommandError> {
    let path = match path {
        Some(p) => crate::normalize_path(&p),
        None => match s.ws.active().and_then(|(_, d)| d.path.clone()).filter(|p| is_manifest(p)) {
            Some(p) => p,
            None => s
                .project_root()
                .map(|r| r.join("Cargo.toml"))
                .filter(|p| p.is_file())
                .ok_or_else(|| CommandError::Failed("no Cargo.toml is open".into()))?,
        },
    };
    let text = match s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)) {
        Some(d) => d.doc.text(),
        None => codecraft_fs::read_text_file(&path)
            .map(|t| codecraft_text::Document::from_text(&t).text())
            .map_err(|e| CommandError::Failed(e.to_string()))?,
    };
    Ok((path, text))
}

/// What the inspection says about one dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepStatus {
    /// Not looked up yet (or the lookup failed).
    Unknown,
    UpToDate,
    Newer(Version),
    NotFound,
    /// No published version satisfies the requirement.
    NoMatch,
    InvalidRequirement,
}

/// The status of a dependency from known versions.
pub fn dep_status(dep: &Dependency, known: Option<&CrateVersions>) -> DepStatus {
    let Some(v) = &dep.version else { return DepStatus::UpToDate };
    let Some(known) = known else { return DepStatus::Unknown };
    if known.not_found {
        return DepStatus::NotFound;
    }
    let Some(req) = semver::VersionReq::parse(&v.req) else { return DepStatus::InvalidRequirement };
    let versions = known.available();
    if let Some(newer) = semver::newer_version(&v.req, &versions) {
        return DepStatus::Newer(newer.clone());
    }
    if !versions.is_empty() && !versions.iter().any(|x| req.matches(x)) {
        return DepStatus::NoMatch;
    }
    DepStatus::UpToDate
}

fn position_at(text_chars: &[char], offset: usize) -> Position {
    let mut line = 0usize;
    let mut col = 0usize;
    for c in text_chars.iter().take(offset) {
        if *c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    Position::new(line, col)
}

/// Problems for a manifest from what is known, and the crates still to look up.
pub fn inspect(path: &Path, text: &str, deps_state: &DepsState) -> (Vec<Problem>, Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut problems = Vec::new();
    let mut missing = Vec::new();
    for dep in manifest::dependencies(text).iter().filter(|d| d.on_crates_io()) {
        let Some(v) = &dep.version else { continue };
        let known = deps_state.crate_versions(&dep.package);
        let problem = |range: &std::ops::Range<usize>, severity: Severity, message: String, code: &str| Problem {
            path: path.to_path_buf(),
            start: position_at(&chars, range.start),
            end: position_at(&chars, range.end),
            severity,
            message,
            code: Some(code.to_string()),
            source: ProblemSource::Inspection,
        };
        match dep_status(dep, known) {
            DepStatus::Unknown => {
                if index::valid_name(&dep.package) {
                    missing.push(dep.package.to_ascii_lowercase());
                }
            }
            DepStatus::UpToDate => {}
            DepStatus::Newer(n) => problems.push(problem(
                &v.range,
                Severity::Info,
                format!("Newer version available: {n}"),
                "newer-version",
            )),
            DepStatus::NotFound => problems.push(problem(
                &dep.key_range,
                Severity::Warning,
                format!("Crate `{}` is not found on crates.io", dep.package),
                "unknown-crate",
            )),
            DepStatus::NoMatch => problems.push(problem(
                &v.range,
                Severity::Warning,
                format!("No published version of `{}` matches `{}`", dep.package, v.req),
                "no-matching-version",
            )),
            DepStatus::InvalidRequirement => problems.push(problem(
                &v.range,
                Severity::Error,
                format!("`{}` is not a valid version requirement", v.req),
                "invalid-version-requirement",
            )),
        }
    }
    missing.sort();
    missing.dedup();
    (problems, missing)
}

/// Re-inspect open manifests that changed (debounced) or whose lookups finished.
fn analyze_open_manifests(s: &mut Session) -> bool {
    if !s.rust.deps.auto || !s.rust.deps.check_updates {
        return false;
    }
    let open: Vec<(PathBuf, u64)> =
        s.ws.docs().filter_map(|d| d.path.clone().filter(|p| is_manifest(p)).map(|p| (p, d.doc.revision()))).collect();
    let mut changed = false;
    let paths: HashSet<PathBuf> = open.iter().map(|(p, _)| p.clone()).collect();
    let closed: Vec<PathBuf> = s.rust.deps.analyzed.keys().filter(|p| !paths.contains(*p)).cloned().collect();
    for p in closed {
        s.rust.deps.analyzed.remove(&p);
        s.ws.set_inspection_problems(INSPECTION, p, Vec::new());
    }
    for (path, revision) in open {
        let entry = s.rust.deps.analyzed.get(&path).cloned();
        match entry {
            Some(a) if a.revision == revision && a.done => continue,
            Some(a) if a.revision != revision => {
                s.rust.deps.analyzed.insert(path, Analyzed { revision, changed: Instant::now(), done: false });
                continue;
            }
            Some(a) if a.changed.elapsed() < DEBOUNCE => continue,
            _ => {}
        }
        let Some(text) = s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)).map(|d| d.doc.text()) else { continue };
        let (problems, missing) = inspect(&path, &text, &s.rust.deps);
        for name in missing.into_iter().take(200) {
            s.rust.deps.request(JobKey::Versions(name), false);
        }
        s.ws.set_inspection_problems(INSPECTION, path.clone(), problems);
        s.rust.deps.analyzed.insert(path, Analyzed { revision, changed: Instant::now(), done: true });
        changed = true;
    }
    changed
}

/// Re-inspect a manifest now (commands and tests): sets its problems and returns them.
pub(crate) fn inspect_now(s: &mut Session, path: &Path, text: &str) -> Vec<Problem> {
    let (problems, _) = inspect(path, text, &s.rust.deps);
    s.ws.set_inspection_problems(INSPECTION, path.to_path_buf(), problems.clone());
    problems
}

// ---------------------------------------------------------------- completion

fn completion_item(
    label: &str,
    insert: String,
    kind: &str,
    detail: Option<String>,
    rank: usize,
    start: usize,
    filter: &str,
) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        detail,
        kind: kind.to_string(),
        filter_text: filter.to_string(),
        sort_text: format!("{rank:06}"),
        insert_text: insert,
        replace_start: Some(start),
        resolved: true,
        ..CompletionItem::default()
    }
}

fn norm(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

/// Completion in Cargo.toml: crate names in dependency tables, versions in version strings.
/// With `auto`, stays quiet when there is nothing to offer.
pub(crate) fn complete(s: &mut Session, auto: bool) -> Result<Value, CommandError> {
    let Some((e, d)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    let editor = e.id;
    let caret = e.selections.primary().head.min(d.doc.len_chars());
    let text = d.doc.text();
    let Some(site) = manifest::completion_site(&text, caret) else {
        if !auto {
            s.status = "No suggestions".into();
        }
        if matches!(&s.popup, Some(Popup::Completion(c)) if c.editor == editor) {
            s.popup = None;
        }
        return Ok(json!({ "items": 0 }));
    };
    let mut items = Vec::new();
    let mut wait: Option<JobKey> = None;
    let anchor;
    match &site {
        CompletionSite::CrateName { start, prefix, key_only } => {
            let trailing = prefix.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').count();
            anchor = caret.saturating_sub(trailing);
            let lead_len = prefix.chars().count().saturating_sub(trailing);
            let lead: String = prefix.chars().take(lead_len).collect();
            let deps = &s.rust.deps;
            // Already declared crates are not offered again.
            let declared: HashSet<String> = manifest::dependencies(&text).iter().map(|d| norm(&d.name)).collect();
            let query = prefix.to_ascii_lowercase();
            let mut names: Vec<(String, Option<String>, Option<String>)> = Vec::new();
            let mut seen = HashSet::new();
            if !query.is_empty() {
                // The longest cached search that the prefix extends.
                let mut q = query.clone();
                loop {
                    if let Some(hits) = deps.searches.get(&q) {
                        for h in hits {
                            if seen.insert(norm(&h.name)) {
                                names.push((h.name.clone(), h.max_version.clone(), h.description.clone()));
                            }
                        }
                        break;
                    }
                    if q.pop().is_none() || q.is_empty() {
                        break;
                    }
                }
                if !deps.searches.contains_key(&query) {
                    wait = Some(JobKey::Search(query.clone()));
                }
            }
            let mut cached: Vec<(&String, &CrateVersions)> = deps.crates.iter().filter(|(_, c)| !c.not_found).collect();
            cached.sort_by(|a, b| a.0.cmp(b.0));
            for (_, c) in cached {
                if seen.insert(norm(&c.name)) {
                    let latest = semver::latest(&c.available()).map(ToString::to_string);
                    names.push((c.name.clone(), latest, None));
                }
            }
            let lead_n = norm(&lead);
            for (rank, (name, version, description)) in names.into_iter().enumerate() {
                let n = norm(&name);
                if declared.contains(&n) || !n.starts_with(&lead_n) || name.len() < lead.len() {
                    continue;
                }
                let filter = name.get(lead.len()..).unwrap_or_default();
                let (insert, cursor) = match (key_only, &version) {
                    (true, _) => (name.clone(), None),
                    (false, Some(v)) => (format!("{name} = \"{v}\""), None),
                    (false, None) => (format!("{name} = \"\""), Some(name.chars().count() + 4)),
                };
                let detail = match (&version, &description) {
                    (Some(v), Some(d)) => Some(format!("{v}  {}", d.chars().take(80).collect::<String>())),
                    (Some(v), None) => Some(v.clone()),
                    (None, Some(d)) => Some(d.chars().take(80).collect()),
                    (None, None) => None,
                };
                let mut it = completion_item(&name, insert, "module", detail, rank, *start, filter);
                it.cursor = cursor;
                items.push(it);
            }
        }
        CompletionSite::Version { package, start, typed } => {
            let trailing = typed.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').count();
            anchor = caret.saturating_sub(trailing);
            let lead: String = typed.chars().take(typed.chars().count().saturating_sub(trailing)).collect();
            let op_len = typed.find(|c: char| c.is_ascii_digit()).unwrap_or(typed.len());
            let op = typed.get(..op_len).unwrap_or("").to_string();
            match s.rust.deps.crate_versions(package) {
                Some(c) if !c.not_found => {
                    let versions = c.newest_first();
                    let latest = semver::latest(&versions).cloned();
                    for (rank, v) in versions.iter().take(MAX_VERSION_ITEMS).enumerate() {
                        let candidate = format!("{op}{v}");
                        let Some(filter) = candidate.strip_prefix(lead.as_str()) else { continue };
                        let detail = if Some(v) == latest.as_ref() {
                            Some("latest".to_string())
                        } else if v.is_prerelease() {
                            Some("pre-release".to_string())
                        } else {
                            None
                        };
                        items.push(completion_item(
                            &candidate,
                            candidate.clone(),
                            "value",
                            detail,
                            rank,
                            *start,
                            filter,
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    if index::valid_name(package) {
                        wait = Some(JobKey::Versions(package.to_ascii_lowercase()));
                    }
                }
            }
        }
    }
    let mut pending = false;
    if let Some(key) = wait {
        let started = s.rust.deps.request(key.clone(), false) || s.rust.deps.in_flight.contains(&key);
        if started {
            pending = true;
            s.rust.deps.waiting = Some(Waiting { editor, anchor, key });
        }
    }
    if items.is_empty() && !pending {
        if !auto {
            s.status = if s.rust.deps.online {
                "No suggestions".into()
            } else {
                "No suggestions (crates.io lookups are off)".into()
            };
        }
        if matches!(&s.popup, Some(Popup::Completion(c)) if c.editor == editor) {
            s.popup = None;
        }
        return Ok(json!({ "items": 0 }));
    }
    let n = items.len();
    s.popup =
        Some(Popup::Completion(CompletionPopup { editor, anchor, items, auto, pending, ..CompletionPopup::default() }));
    s.refresh_completion_filter();
    Ok(json!({ "items": n, "pending": pending }))
}

/// After a typed char in Cargo.toml: (re)open completion on crate names and versions. Returns
/// whether the active editor is a manifest (the language server path is skipped then).
pub(crate) fn after_typed(s: &mut Session, typed: char) -> bool {
    if !is_cargo_toml_editor(s) {
        return false;
    }
    let open = matches!(&s.popup, Some(Popup::Completion(_)));
    let auto_allowed = s.ws.settings.completion_auto_popup && !s.power_save();
    if !open && !auto_allowed {
        return true;
    }
    let Some((e, d)) = s.ws.active() else { return true };
    if e.selections.len() != 1 {
        return true;
    }
    let caret = e.selections.primary().head;
    let relevant = typed.is_alphanumeric() || matches!(typed, '_' | '-' | '.' | '"' | '^' | '~' | '=');
    if !relevant {
        return true;
    }
    let text = d.doc.text();
    match manifest::completion_site(&text, caret) {
        Some(CompletionSite::CrateName { .. }) if typed.is_alphanumeric() || matches!(typed, '_' | '-') => {
            let auto = !matches!(&s.popup, Some(Popup::Completion(c)) if !c.auto);
            let _ = complete(s, auto);
        }
        Some(CompletionSite::Version { .. }) => {
            let auto = !matches!(&s.popup, Some(Popup::Completion(c)) if !c.auto);
            let _ = complete(s, auto);
        }
        _ => {}
    }
    true
}

// ---------------------------------------------------------------- quick-fixes and updates

/// The new requirement for a dependency (`version`, else the newest known version).
fn new_requirement(s: &Session, dep: &Dependency, version: Option<&str>) -> Result<String, CommandError> {
    let field = dep
        .version
        .as_ref()
        .ok_or_else(|| CommandError::Failed(format!("`{}` has no version requirement", dep.name)))?;
    let target = match version {
        Some(v) => Version::parse(v).ok_or_else(|| CommandError::BadParams(format!("`{v}` is not a version")))?,
        None => match dep_status(dep, s.rust.deps.crate_versions(&dep.package)) {
            DepStatus::Newer(v) => v,
            DepStatus::Unknown => {
                return Err(CommandError::Failed(format!(
                    "the versions of `{}` are not known yet (run rust.cargo.checkDependencyUpdates)",
                    dep.package
                )));
            }
            _ => return Err(CommandError::Failed(format!("`{}` is up to date", dep.name))),
        },
    };
    Ok(semver::updated_requirement(&field.req, &target))
}

// `Uri` keys are what lsp-types' WorkspaceEdit uses; they are never mutated here.
#[allow(clippy::mutable_key_type)]
fn workspace_edit(
    path: &Path,
    text: &str,
    edits: Vec<(std::ops::Range<usize>, String)>,
) -> Result<lt::WorkspaceEdit, CommandError> {
    let doc = codecraft_text::Document::from_text(text);
    let uri = codecraft_lsp::path_to_uri(path).map_err(|e| CommandError::Failed(e.to_string()))?;
    let edits: Vec<lt::TextEdit> = edits
        .into_iter()
        .map(|(r, new_text)| lt::TextEdit { range: crate::convert::lsp_range(&doc, r), new_text })
        .collect();
    let mut changes = HashMap::new();
    changes.insert(uri, edits);
    Ok(lt::WorkspaceEdit { changes: Some(changes), ..lt::WorkspaceEdit::default() })
}

/// `rust.cargo.updateDependency`.
pub(crate) fn update_dependency(
    s: &mut Session,
    path: Option<PathBuf>,
    name: &str,
    version: Option<&str>,
) -> Result<Value, CommandError> {
    let (path, text) = target_manifest(s, path)?;
    let deps = manifest::dependencies(&text);
    let dep = deps
        .iter()
        .find(|d| d.name == name)
        .or_else(|| deps.iter().find(|d| norm(&d.package) == norm(name)))
        .ok_or_else(|| CommandError::Failed(format!("no dependency `{name}` in {}", path.display())))?;
    let req = new_requirement(s, dep, version)?;
    let range = dep.version.as_ref().map(|v| v.range.clone()).unwrap_or(0..0);
    let edit = workspace_edit(&path, &text, vec![(range, req.clone())])?;
    crate::refactor::apply_or_preview(s, &format!("Update {}", dep.name), &edit, false)?;
    s.status = format!("{} updated to \"{req}\"", dep.name);
    if let Some(t) = s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)).map(|d| d.doc.text()) {
        inspect_now(s, &path, &t);
    }
    Ok(json!({ "name": dep.name, "requirement": req }))
}

/// `rust.cargo.updateAllDependencies`: every dependency with a newer version, through the preview.
pub(crate) fn update_all(s: &mut Session, path: Option<PathBuf>, preview: bool) -> Result<Value, CommandError> {
    let (path, text) = target_manifest(s, path)?;
    let mut edits = Vec::new();
    let mut updated = Vec::new();
    for dep in manifest::dependencies(&text).iter().filter(|d| d.on_crates_io()) {
        if let DepStatus::Newer(v) = dep_status(dep, s.rust.deps.crate_versions(&dep.package))
            && let Some(field) = &dep.version
        {
            let req = semver::updated_requirement(&field.req, &v);
            updated.push(json!({ "name": dep.name, "from": field.req, "to": req }));
            edits.push((field.range.clone(), req));
        }
    }
    if edits.is_empty() {
        s.status = "Every dependency is up to date".into();
        return Ok(json!({ "updated": [] }));
    }
    let edit = workspace_edit(&path, &text, edits)?;
    crate::refactor::apply_or_preview(s, "Update Dependencies", &edit, preview)?;
    Ok(json!({ "updated": updated, "preview": preview }))
}

/// `rust.cargo.checkDependencyUpdates`: look every dependency up again (ignoring the cache age).
pub(crate) fn check_updates(s: &mut Session, path: Option<PathBuf>) -> Result<Value, CommandError> {
    let (path, text) = target_manifest(s, path)?;
    let mut n = 0usize;
    for dep in manifest::dependencies(&text).iter().filter(|d| d.on_crates_io()) {
        if index::valid_name(&dep.package)
            && s.rust.deps.request(JobKey::Versions(dep.package.to_ascii_lowercase()), true)
        {
            n += 1;
        }
    }
    s.rust.deps.analyzed.remove(&path);
    if !s.rust.deps.online {
        s.status = "crates.io lookups are off: showing cached versions".into();
    } else {
        s.status = format!("Checking {n} dependencies on crates.io…");
    }
    Ok(json!({ "requested": n, "online": s.rust.deps.online }))
}

/// `rust.cargo.dependencies`: the manifest's dependencies with what is known about them.
pub(crate) fn list(s: &mut Session, path: Option<PathBuf>) -> Result<Value, CommandError> {
    let (path, text) = target_manifest(s, path)?;
    let deps: Vec<Value> = manifest::dependencies(&text)
        .iter()
        .map(|d| {
            let known = s.rust.deps.crate_versions(&d.package);
            let latest = known.and_then(|k| semver::latest(&k.available()).map(ToString::to_string));
            let status = match dep_status(d, known) {
                DepStatus::Unknown => json!("unknown"),
                DepStatus::UpToDate => json!("up_to_date"),
                DepStatus::Newer(v) => json!({ "newer": v.to_string() }),
                DepStatus::NotFound => json!("not_found"),
                DepStatus::NoMatch => json!("no_matching_version"),
                DepStatus::InvalidRequirement => json!("invalid_requirement"),
            };
            json!({
                "name": d.name,
                "package": d.package,
                "table": d.table.label(),
                "target": d.target,
                "requirement": d.version.as_ref().map(|v| v.req.clone()),
                "source": d.source,
                "latest": latest,
                "status": if d.on_crates_io() { status } else { json!("not_on_crates_io") },
            })
        })
        .collect();
    Ok(json!({ "path": path, "dependencies": deps }))
}

/// Alt+Enter in Cargo.toml: update the dependency on the caret line, update all, advisories.
pub(crate) fn context_actions(s: &Session) -> Vec<ActionItem> {
    let Some((e, d)) = s.ws.active() else { return Vec::new() };
    let Some(path) = d.path.clone().filter(|p| is_manifest(p)) else { return Vec::new() };
    let text = d.doc.text();
    let line = d.doc.char_to_line(e.selections.primary().head);
    let line_range = d.doc.line_start(line)..d.doc.line_start(line + 1).max(d.doc.line_start(line));
    let on_line = |r: &std::ops::Range<usize>| r.start <= line_range.end && r.end >= line_range.start;
    let mut items = Vec::new();
    let mut any_newer = false;
    for dep in manifest::dependencies(&text).iter().filter(|d| d.on_crates_io()) {
        if let DepStatus::Newer(v) = dep_status(dep, s.rust.deps.crate_versions(&dep.package)) {
            any_newer = true;
            let here = on_line(&dep.key_range) || dep.version.as_ref().is_some_and(|f| on_line(&f.range));
            if here {
                let title = format!("Update '{}' to {v}", dep.name);
                items.push(ActionItem {
                    title: title.clone(),
                    kind: Some("quickfix".into()),
                    payload: json!({
                        "title": title,
                        "codecraftCommand": "rust.cargo.updateDependency",
                        "codecraftParams": { "path": path, "name": dep.name, "version": v.to_string() },
                    }),
                });
            }
        }
    }
    if any_newer {
        items.push(ActionItem {
            title: "Update All Dependencies…".into(),
            kind: Some("quickfix".into()),
            payload: json!({
                "title": "Update All Dependencies…",
                "codecraftCommand": "rust.cargo.updateAllDependencies",
                "codecraftParams": { "path": path },
            }),
        });
    }
    items.extend(crate::cargo_audit::context_actions(s, &path, line));
    items
}

/// Show Context Actions in a Cargo.toml (no language server there).
pub(crate) fn show_context_actions(s: &mut Session) -> Result<Value, CommandError> {
    let items = context_actions(s);
    let Some(editor) = s.ws.active().map(|(e, _)| e.id) else { return Ok(Value::Null) };
    if items.is_empty() {
        s.status = "No context actions available here".into();
        return Ok(json!({ "items": 0 }));
    }
    let n = items.len();
    s.popup = Some(Popup::Actions { editor, title: "Context Actions".into(), items, selected: 0, pending: false });
    Ok(json!({ "items": n }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(name: &str, vs: &[&str]) -> CrateVersions {
        CrateVersions {
            name: name.into(),
            versions: vs.iter().map(|v| index::PublishedVersion { version: (*v).into(), yanked: false }).collect(),
            not_found: false,
            fetched: index::now_secs(),
        }
    }

    #[test]
    fn inspection_reports_newer_unknown_and_bad_requirements() {
        let text = "[dependencies]\nrand = \"0.7\"\nserde = \"1\"\nnope = \"1\"\nbad = \"x.y\"\nnone = \"9\"\nlater = \"2\"\nlocal = { path = \"x\" }\n";
        let mut st = DepsState::default();
        st.insert_versions(versions("rand", &["0.7.3", "0.8.5"]));
        st.insert_versions(versions("serde", &["1.0.0", "1.0.210"]));
        st.insert_versions(CrateVersions { name: "nope".into(), not_found: true, ..Default::default() });
        st.insert_versions(versions("bad", &["1.0.0"]));
        st.insert_versions(versions("none", &["1.0.0"]));
        let (problems, missing) = inspect(Path::new("/p/Cargo.toml"), text, &st);
        let msgs: Vec<(&str, Severity, usize)> =
            problems.iter().map(|p| (p.message.as_str(), p.severity, p.start.line)).collect();
        assert_eq!(
            msgs,
            [
                ("Newer version available: 0.8.5", Severity::Info, 1),
                ("Crate `nope` is not found on crates.io", Severity::Warning, 3),
                ("`x.y` is not a valid version requirement", Severity::Error, 4),
                ("No published version of `none` matches `9`", Severity::Warning, 5),
            ]
        );
        assert_eq!(problems[0].start.column, 8);
        assert_eq!(problems[0].end.column, 11);
        assert_eq!(missing, ["later"]);
    }
}
