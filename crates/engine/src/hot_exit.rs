//! Hot exit (M13 track SR): nothing typed is lost on quit, on a window close or in a crash.
//!
//! While Settings › Appearance & Behavior › System Settings › *Keep unsaved changes on exit* is
//! on (the default) and the app turned hot exit on ([`HotExitState::enabled`]):
//!
//! - every [`AUTOSAVE_EVERY`] the text of each modified document and each untitled buffer with
//!   text (scratch buffers without a path included) is written to `<project>/.codecraft/unsaved/`
//!   (the configuration directory's `unsaved/<project key>/` when the project is read-only or
//!   none is open): one JSON file per buffer, atomic writes on a writer thread, only when the
//!   text changed since the last write;
//! - quitting or closing the window writes the last state and leaves the files there instead of
//!   saving the documents (and without asking);
//! - when the project opens again its buffers come back modified with a "restored unsaved
//!   changes" banner (Compare with Disk shows the diff view); a file that changed or vanished on
//!   disk meanwhile is flagged and is not saved over until the user keeps or discards the
//!   restored changes;
//! - a buffer's file is deleted as soon as it is saved, reverted, closed (closing a tab saves
//!   it) or discarded.
//!
//! With the setting off, quitting saves the documents as before and nothing is kept. Buffers over
//! [`MAX_BUFFER_BYTES`], beyond [`MAX_BUFFERS`] or past [`MAX_TOTAL_BYTES`] per project are not
//! kept (a warning says which).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_workspace::{DocId, NotificationLevel};

use crate::commands::{CmdResult, CommandSpec, Context, spec};
use crate::misc::CompareView;
use crate::params::opt_usize;
use crate::{CommandError, Session};

/// The backup directory inside a project.
pub const DIR: &str = ".codecraft/unsaved";
/// How often changed buffers are written while hot exit is on.
pub const AUTOSAVE_EVERY: Duration = Duration::from_secs(3);
/// Larger buffers are not kept.
pub const MAX_BUFFER_BYTES: usize = 16 << 20;
/// Bytes of buffers kept per project.
pub const MAX_TOTAL_BYTES: usize = 128 << 20;
/// Buffers kept per project.
pub const MAX_BUFFERS: usize = 200;
/// Version written into each file.
pub const FORMAT_VERSION: u32 = 1;
/// Backup files larger than this are skipped when reading (JSON escaping can double the text).
const MAX_READ_BYTES: u64 = (MAX_BUFFER_BYTES as u64) * 2 + (1 << 20);
const MAX_FILES_SCANNED: usize = 2_000;
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_NOTE_EVERY: Duration = Duration::from_secs(60);

/// Backup files owned by a live session of this process (another window must not restore them).
static CLAIMED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

fn claim(file: &Path) {
    CLAIMED.lock().unwrap_or_else(PoisonError::into_inner).insert(file.to_path_buf());
}

fn release(file: &Path) {
    CLAIMED.lock().unwrap_or_else(PoisonError::into_inner).remove(file);
}

fn claimed(file: &Path) -> bool {
    CLAIMED.lock().unwrap_or_else(PoisonError::into_inner).contains(file)
}

// ------------------------------------------------------------------------------ the file

/// One kept buffer (`<key>.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Backup {
    #[serde(default)]
    pub version: u32,
    pub key: String,
    /// The file the buffer belongs to; `None` for untitled buffers.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub language: Option<Language>,
    /// The buffer's text (`\n` line ends; the file's own line ending is kept by the document).
    pub text: String,
    /// The file's modification time (Unix ms) when the buffer last loaded or saved it: a
    /// different time on restore means the file changed on disk meanwhile.
    #[serde(default)]
    pub disk_mtime_ms: Option<u64>,
    /// When this backup was written (Unix ms).
    #[serde(default)]
    pub saved_ms: u64,
}

/// What happened to a restored buffer's file on disk meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Conflict {
    None,
    /// The file changed on disk after the buffer's changes were kept.
    ChangedOnDisk,
    /// The file no longer exists.
    DeletedOnDisk,
}

/// A buffer restored with unsaved changes (the banner above its editor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    pub key: String,
    pub conflict: Conflict,
    /// When the changes were kept (Unix ms).
    pub saved_ms: u64,
}

struct Owned {
    file: PathBuf,
    rev: u64,
    bytes: usize,
}

enum WriteMsg {
    Write(PathBuf, String),
    Remove(PathBuf),
    Flush(Sender<()>),
}

struct Writer {
    tx: Sender<WriteMsg>,
    errors: Receiver<String>,
}

/// Hot exit state of one [`Session`].
pub struct HotExitState {
    /// Keep buffers and restore them (the desktop app turns this on; tests and the CLI leave it
    /// off unless they ask).
    pub enabled: bool,
    /// Instead of `unsaved/` under [`crate::session_store::config_dir`] for read-only projects
    /// and buffers without a project (tests and embedders; [`Session::isolate_home`]).
    pub fallback_dir: Option<PathBuf>,
    pub every: Duration,
    pub max_buffer_bytes: usize,
    pub max_total_bytes: usize,
    pub max_buffers: usize,
    /// Buffers restored with unsaved changes whose banner is shown.
    pub restored: BTreeMap<DocId, Restored>,
    /// Files written so far (tests check that unchanged buffers are not written again).
    pub writes: u64,
    /// Buffers that were not kept, with the reason (shown once each).
    pub skipped: BTreeMap<DocId, String>,
    /// (project root, backup directory) once resolved for the open project.
    dir: Option<(Option<PathBuf>, PathBuf)>,
    project: Option<PathBuf>,
    owned: HashMap<DocId, Owned>,
    last_tick: Option<Instant>,
    writer: Option<Writer>,
    last_error_note: Option<Instant>,
}

impl Default for HotExitState {
    fn default() -> Self {
        Self {
            enabled: false,
            fallback_dir: None,
            every: AUTOSAVE_EVERY,
            max_buffer_bytes: MAX_BUFFER_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_buffers: MAX_BUFFERS,
            restored: BTreeMap::new(),
            writes: 0,
            skipped: BTreeMap::new(),
            dir: None,
            project: None,
            owned: HashMap::new(),
            last_tick: None,
            writer: None,
            last_error_note: None,
        }
    }
}

impl Drop for HotExitState {
    fn drop(&mut self) {
        // The files stay (a crash keeps them too); only the in-process claim goes.
        for o in self.owned.values() {
            release(&o.file);
        }
    }
}

impl HotExitState {
    /// The fallback base directory (`unsaved/` in the configuration directory).
    fn fallback_base(&self) -> Option<PathBuf> {
        match &self.fallback_dir {
            Some(d) => Some(d.clone()),
            None => crate::session_store::config_dir().map(|d| d.join("unsaved")),
        }
    }

    /// The fallback directory for `root` (`None` = no project).
    pub fn fallback_for(&self, root: Option<&Path>) -> Option<PathBuf> {
        let base = self.fallback_base()?;
        Some(match root {
            Some(r) => base.join(codecraft_agent::git::project_hash(&crate::normalize_path(r))),
            None => base.join("no-project"),
        })
    }

    /// Wait until every queued write is on disk (at most `timeout`). False on timeout.
    pub fn flush(&mut self, timeout: Duration) -> bool {
        let Some(w) = &self.writer else { return true };
        let (tx, rx) = bounded(1);
        if w.tx.send(WriteMsg::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    /// The kept buffers: (document, backup file).
    pub fn owned_files(&self) -> Vec<(DocId, PathBuf)> {
        let mut v: Vec<(DocId, PathBuf)> = self.owned.iter().map(|(d, o)| (*d, o.file.clone())).collect();
        v.sort();
        v
    }

    fn send(&mut self, msg: WriteMsg) -> bool {
        if self.writer.is_none() {
            self.writer = spawn_writer();
        }
        let sent = self.writer.as_ref().is_some_and(|w| w.tx.send(msg).is_ok());
        if !sent {
            self.writer = None;
        }
        sent
    }

    /// Stop owning every file without deleting it (quit: they are restored next time).
    fn forget(&mut self) {
        for (_, o) in self.owned.drain() {
            release(&o.file);
        }
        self.restored.clear();
        self.skipped.clear();
    }
}

/// Whether buffers are kept now (the app turned hot exit on and the setting is on).
pub fn active(s: &Session) -> bool {
    s.hot_exit.enabled && s.ws.settings.hot_exit
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(0)
}

fn ms_of(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok())
}

fn mtime_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(ms_of)
}

/// A backup key is one safe file-name component.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 96
        && !key.starts_with(['.', '-'])
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn new_key() -> String {
    let n = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    format!("b-{}-{:x}-{n}", now_ms(), std::process::id())
}

// ------------------------------------------------------------------------------ writer

fn spawn_writer() -> Option<Writer> {
    let (tx, rx) = unbounded::<WriteMsg>();
    let (etx, errors) = unbounded::<String>();
    std::thread::Builder::new().name("codecraft-hot-exit".into()).spawn(move || writer_loop(&rx, &etx)).ok()?;
    Some(Writer { tx, errors })
}

fn writer_loop(rx: &Receiver<WriteMsg>, errors: &Sender<String>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        // Only the newest message about a file in a batch matters.
        let mut last: HashMap<PathBuf, usize> = HashMap::new();
        for (i, m) in batch.iter().enumerate() {
            if let WriteMsg::Write(p, _) | WriteMsg::Remove(p) = m {
                last.insert(p.clone(), i);
            }
        }
        for (i, msg) in batch.into_iter().enumerate() {
            match msg {
                WriteMsg::Write(path, text) => {
                    if last.get(&path) == Some(&i)
                        && let Err(e) = write_file(&path, &text)
                    {
                        let _ = errors.send(e);
                    }
                }
                WriteMsg::Remove(path) => {
                    if last.get(&path) == Some(&i)
                        && let Err(e) = remove_file(&path)
                    {
                        let _ = errors.send(e);
                    }
                }
                WriteMsg::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
        }
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    codecraft_fs::write_text_file_atomic(path, text).map_err(|e| format!("cannot keep {}: {e}", path.display()))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot delete {}: {e}", path.display())),
    }
}

// ------------------------------------------------------------------------------ where

fn project_root(s: &Session) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.clone())
}

/// The project's own backup directory, if it can be written: created with a `.gitignore` that
/// keeps it out of version control.
fn own_dir(root: &Path) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).ok()?;
    let ignore = dir.join(".gitignore");
    if !ignore.is_file() {
        codecraft_fs::write_text_file_atomic(&ignore, "# Unsaved changes kept by CodeCraft (hot exit).\n*\n").ok()?;
    }
    Some(dir)
}

/// The backup directory for the open project (resolved once per project, created on demand).
fn backup_dir(s: &mut Session) -> Option<PathBuf> {
    let root = project_root(s);
    if let Some((r, d)) = &s.hot_exit.dir
        && *r == root
    {
        return Some(d.clone());
    }
    let dir = match root.as_deref().and_then(own_dir) {
        Some(d) => d,
        None => s.hot_exit.fallback_for(root.as_deref())?,
    };
    s.hot_exit.dir = Some((root, dir.clone()));
    Some(dir)
}

/// Every directory that may hold the backups of `root`: the project's own and the fallback.
fn candidate_dirs(st: &HotExitState, root: Option<&Path>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(r) = root {
        v.push(r.join(DIR));
    }
    if let Some(f) = st.fallback_for(root) {
        v.push(f);
    }
    v
}

// ------------------------------------------------------------------------------ keeping

/// Whether a document has something to keep.
fn needs_backup(od: &codecraft_workspace::OpenDocument) -> bool {
    od.doc.is_modified() || (od.path.is_none() && od.doc.len_chars() > 0)
}

/// What [`sync`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub written: usize,
    pub removed: usize,
    pub skipped: usize,
}

/// Write every buffer whose text changed since its last write; delete the files of buffers that
/// were saved, reverted, closed or discarded. `now`: write on this thread (quit) instead of the
/// writer thread.
pub fn sync(s: &mut Session, now: bool) -> SyncReport {
    let mut report = SyncReport::default();
    // Documents in a stable order: the oldest first.
    let mut ids: Vec<DocId> = s.ws.docs().filter(|d| needs_backup(d)).map(|d| d.id).collect();
    ids.sort();
    // Directories are resolved (and the project's created) only when a new file is needed.
    // Files outside the project are kept in the configuration directory: a project's own
    // directory is trusted only for its own files (see `restore`).
    let root = project_root(s);
    let (inside, outside) = if ids.iter().any(|id| !s.hot_exit.owned.contains_key(id)) {
        (backup_dir(s), s.hot_exit.fallback_for(root.as_deref()))
    } else {
        (None, None)
    };
    let mut total = 0usize;
    let mut kept = 0usize;
    let mut writes: Vec<(PathBuf, String)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for id in &ids {
        let Some(od) = s.ws.doc(*id) else { continue };
        let st = &mut s.hot_exit;
        let rev = od.doc.revision();
        if let Some(o) = st.owned.get(id)
            && o.rev == rev
        {
            total = total.saturating_add(o.bytes);
            kept += 1;
            continue;
        }
        let name = od.path.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| od.title());
        let refuse = |why: String, st: &mut HotExitState, warnings: &mut Vec<String>| {
            if st.skipped.insert(*id, why.clone()).is_none() {
                warnings.push(format!("{name}: {why}"));
            }
        };
        if kept >= st.max_buffers {
            refuse(format!("more than {} unsaved buffers", st.max_buffers), st, &mut warnings);
            report.skipped += 1;
            continue;
        }
        // Chars never exceed bytes: a cheap check before copying the text.
        if od.doc.len_chars() > st.max_buffer_bytes {
            refuse(format!("larger than {} MB", st.max_buffer_bytes >> 20), st, &mut warnings);
            report.skipped += 1;
            continue;
        }
        let text = od.doc.text();
        if text.len() > st.max_buffer_bytes {
            refuse(format!("larger than {} MB", st.max_buffer_bytes >> 20), st, &mut warnings);
            report.skipped += 1;
            continue;
        }
        if total.saturating_add(text.len()) > st.max_total_bytes {
            refuse(format!("over the {} MB kept per project", st.max_total_bytes >> 20), st, &mut warnings);
            report.skipped += 1;
            continue;
        }
        let in_project = match (&od.path, &root) {
            (Some(p), Some(r)) => p.starts_with(r),
            (Some(_), None) => false,
            (None, _) => true,
        };
        let dir = if in_project { inside.clone() } else { outside.clone() };
        let file = match (st.owned.get(id), dir) {
            (Some(o), _) => o.file.clone(),
            (None, Some(d)) => d.join(format!("{}.json", new_key())),
            (None, None) => {
                refuse("no directory to keep it in".into(), st, &mut warnings);
                report.skipped += 1;
                continue;
            }
        };
        st.skipped.remove(id);
        let key = file.file_stem().map(|k| k.to_string_lossy().into_owned()).unwrap_or_else(new_key);
        let backup = Backup {
            version: FORMAT_VERSION,
            key: key.clone(),
            path: od.path.clone(),
            title: od.title(),
            language: Some(od.language),
            disk_mtime_ms: od.disk_mtime.and_then(ms_of),
            saved_ms: now_ms(),
            text,
        };
        let Ok(json) = serde_json::to_string(&backup) else { continue };
        let bytes = backup.text.len();
        claim(&file);
        st.owned.insert(*id, Owned { file: file.clone(), rev, bytes });
        total = total.saturating_add(bytes);
        kept += 1;
        writes.push((file, json));
    }
    // Files of buffers with nothing left to keep.
    let stale: Vec<DocId> = s
        .hot_exit
        .owned
        .keys()
        .copied()
        .filter(|id| !s.ws.doc(*id).is_some_and(needs_backup) || !ids.contains(id))
        .collect();
    let mut removes = Vec::new();
    for id in stale {
        if let Some(o) = s.hot_exit.owned.remove(&id) {
            release(&o.file);
            removes.push(o.file);
        }
        s.hot_exit.skipped.remove(&id);
    }
    // Banners of documents that are saved or gone.
    let gone: Vec<DocId> =
        s.hot_exit.restored.keys().copied().filter(|id| !s.ws.doc(*id).is_some_and(|d| d.doc.is_modified())).collect();
    for id in gone {
        // A buffer without a path keeps its banner while it has text.
        let untitled_with_text = s.ws.doc(id).is_some_and(|d| d.path.is_none() && d.doc.len_chars() > 0);
        if !untitled_with_text {
            s.hot_exit.restored.remove(&id);
        }
    }
    s.hot_exit.skipped.retain(|id, _| ids.contains(id));
    report.written = writes.len();
    report.removed = removes.len();
    let st = &mut s.hot_exit;
    st.writes = st.writes.saturating_add(writes.len() as u64);
    if now {
        // Queued writes land first, so they never overwrite these.
        st.flush(FLUSH_TIMEOUT);
        for (file, json) in writes {
            if let Err(e) = write_file(&file, &json) {
                warnings.push(e);
            }
        }
        for file in removes {
            if let Err(e) = remove_file(&file) {
                warnings.push(e);
            }
        }
    } else {
        for (file, json) in writes {
            if !st.send(WriteMsg::Write(file.clone(), json.clone()))
                && let Err(e) = write_file(&file, &json)
            {
                warnings.push(e);
            }
        }
        for file in removes {
            if !st.send(WriteMsg::Remove(file.clone()))
                && let Err(e) = remove_file(&file)
            {
                warnings.push(e);
            }
        }
    }
    if !warnings.is_empty() {
        s.notify(NotificationLevel::Warning, "Some unsaved changes are not kept", warnings.join("\n"));
    }
    report
}

/// Delete every file this session owns (hot exit was turned off).
fn drop_all(s: &mut Session) {
    let files: Vec<PathBuf> = s.hot_exit.owned.drain().map(|(_, o)| o.file).collect();
    for f in files {
        release(&f);
        if !s.hot_exit.send(WriteMsg::Remove(f.clone())) {
            let _ = remove_file(&f);
        }
    }
    s.hot_exit.restored.retain(|_, r| r.conflict != Conflict::None);
}

fn drain_errors(s: &mut Session) {
    let errors: Vec<String> = match &s.hot_exit.writer {
        Some(w) => w.errors.try_iter().take(32).collect(),
        None => return,
    };
    if errors.is_empty() {
        return;
    }
    for e in &errors {
        log::warn!("hot exit: {e}");
    }
    let st = &mut s.hot_exit;
    if st.last_error_note.is_none_or(|t| t.elapsed() >= ERROR_NOTE_EVERY) {
        st.last_error_note = Some(Instant::now());
        s.notify(NotificationLevel::Warning, "Cannot keep unsaved changes", errors.join("\n"));
    }
}

/// Periodic keeping (called from `Session::pump`).
pub(crate) fn autosave(s: &mut Session) {
    if !s.hot_exit.enabled {
        return;
    }
    drain_errors(s);
    let root = project_root(s);
    if s.hot_exit.project != root {
        // Another project: the old one's buffers were kept (or saved) when it closed.
        s.hot_exit.project = root;
        s.hot_exit.dir = None;
    }
    if !s.ws.settings.hot_exit {
        if !s.hot_exit.owned.is_empty() {
            drop_all(s);
        }
        return;
    }
    let now = Instant::now();
    let st = &mut s.hot_exit;
    match st.last_tick {
        Some(t) if now.duration_since(t) < st.every => return,
        _ => st.last_tick = Some(now),
    }
    let dirty = s.ws.docs().any(needs_backup);
    if !dirty && s.hot_exit.owned.is_empty() && s.hot_exit.restored.is_empty() {
        return;
    }
    sync(s, false);
}

/// Quitting, closing the window or the project: with hot exit on, keep every buffer (written
/// now) and leave the files for the next start; otherwise save the documents as before.
pub fn quit_save(s: &mut Session) {
    if active(s) {
        sync(s, true);
        s.hot_exit.forget();
        return;
    }
    if let Err(e) = s.save_all() {
        log::warn!("saving on close: {e}");
    }
}

/// Documents whose restored changes must not be saved over the file yet (it changed or vanished
/// on disk meanwhile): the user compares, then keeps or discards them.
pub fn blocked(st: &HotExitState) -> Vec<DocId> {
    st.restored.iter().filter(|(_, r)| r.conflict != Conflict::None).map(|(d, _)| *d).collect()
}

/// A tab of a blocked document closed: leave its kept changes for the next start instead of
/// saving them. True if `doc` was blocked (the caller must not write it).
pub(crate) fn keep_on_close(s: &mut Session, doc: DocId) -> bool {
    if !blocked(&s.hot_exit).contains(&doc) {
        return false;
    }
    s.hot_exit.restored.remove(&doc);
    if let Some(o) = s.hot_exit.owned.remove(&doc) {
        release(&o.file);
    }
    s.status = "Its file changed on disk: the unsaved changes are kept for the next time the project opens".into();
    true
}

// ------------------------------------------------------------------------------ restoring

/// Read the backups in `dirs` (newest first per file; corrupt files moved aside).
fn read_backups(dirs: &[PathBuf], warnings: &mut Vec<String>) -> Vec<(PathBuf, Backup)> {
    let mut out: Vec<(PathBuf, Backup)> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for entry in rd.filter_map(Result::ok).take(MAX_FILES_SCANNED) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            if !valid_key(&stem) || claimed(&path) {
                continue;
            }
            match read_backup(&path, &stem) {
                Ok(b) => out.push((path, b)),
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    let mut bad = path.clone().into_os_string();
                    bad.push(".bad");
                    if let Err(re) = std::fs::rename(&path, &bad) {
                        log::warn!("cannot move aside {}: {re}", path.display());
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.1.saved_ms.cmp(&b.1.saved_ms).then_with(|| a.0.cmp(&b.0)));
    out
}

fn read_backup(path: &Path, key: &str) -> Result<Backup, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(format!("too large ({} MB)", meta.len() >> 20));
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut b: Backup = serde_json::from_str(&text).map_err(|e| format!("not a kept buffer ({e})"))?;
    if b.text.len() > MAX_BUFFER_BYTES {
        return Err("too large".into());
    }
    // The file name is the key (a hostile key inside the file is ignored).
    b.key = key.to_string();
    if let Some(p) = &b.path {
        if !p.is_absolute() || p.as_os_str().is_empty() {
            return Err("its file path is not absolute".into());
        }
        b.path = Some(crate::normalize_path(p));
    }
    Ok(b)
}

/// What [`restore`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RestoreReport {
    /// Files restored with unsaved changes.
    pub files: Vec<PathBuf>,
    /// Untitled buffers restored.
    pub untitled: usize,
    /// Files that changed or vanished on disk meanwhile.
    pub conflicts: Vec<PathBuf>,
    /// Backups that were dropped (corrupt, or nothing left to restore).
    pub dropped: usize,
}

/// Bring back the open project's kept buffers (after its editors were restored), or with no
/// project the buffers kept without one (the app's start).
pub fn restore_on_open(s: &mut Session) {
    if !s.hot_exit.enabled {
        return;
    }
    let report = restore(s);
    if report.files.is_empty() && report.untitled == 0 {
        return;
    }
    let mut parts = Vec::new();
    if !report.files.is_empty() {
        let names: Vec<String> = report
            .files
            .iter()
            .take(5)
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        let more = report.files.len().saturating_sub(names.len());
        let mut list = names.join(", ");
        if more > 0 {
            list.push_str(&format!(" and {more} more"));
        }
        parts.push(list);
    }
    if report.untitled > 0 {
        parts.push(format!("{} untitled buffer(s)", report.untitled));
    }
    s.notify(NotificationLevel::Info, "Restored unsaved changes", parts.join("; "));
    if !report.conflicts.is_empty() {
        let names: Vec<String> = report.conflicts.iter().map(|p| p.display().to_string()).collect();
        s.notify(
            NotificationLevel::Warning,
            "Changed on disk while unsaved changes were kept",
            format!(
                "{}: the editor shows the kept changes and the file is left as it is; compare with disk, then keep or discard them",
                names.join(", ")
            ),
        );
    }
}

/// Restore the backups of the open project (also `hotExit.restore`).
pub fn restore(s: &mut Session) -> RestoreReport {
    let mut report = RestoreReport::default();
    let root = project_root(s);
    let dirs = candidate_dirs(&s.hot_exit, root.as_deref());
    let mut warnings = Vec::new();
    let backups = read_backups(&dirs, &mut warnings);
    // One backup per file: the newest wins, older ones (left by a crash) go.
    let mut newest: HashMap<PathBuf, usize> = HashMap::new();
    for (i, (_, b)) in backups.iter().enumerate() {
        if let Some(p) = &b.path {
            newest.insert(p.clone(), i);
        }
    }
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let own = root.as_ref().map(|r| r.join(DIR));
    for (i, (file, b)) in backups.into_iter().enumerate() {
        // A project's own directory comes with the project (a clone could carry one): it may
        // only restore the project's files, never one elsewhere on the machine.
        let in_own = own.as_deref().is_some_and(|d| file.starts_with(d));
        if in_own
            && let (Some(p), Some(r)) = (&b.path, &root)
            && !p.starts_with(r)
        {
            warnings.push(format!("{}: names {} outside the project; ignored", file.display(), p.display()));
            let mut bad = file.clone().into_os_string();
            bad.push(".bad");
            let _ = std::fs::rename(&file, &bad);
            report.dropped += 1;
            continue;
        }
        if let Some(p) = &b.path
            && (newest.get(p) != Some(&i) || !seen_paths.insert(p.clone()))
        {
            let _ = remove_file(&file);
            report.dropped += 1;
            continue;
        }
        match restore_one(s, &file, b) {
            Ok(Some((path, conflict))) => match path {
                Some(p) => {
                    if conflict != Conflict::None {
                        report.conflicts.push(p.clone());
                    }
                    report.files.push(p);
                }
                None => report.untitled += 1,
            },
            Ok(None) => {
                let _ = remove_file(&file);
                report.dropped += 1;
            }
            Err(e) => {
                warnings.push(format!("{}: {e}", file.display()));
                report.dropped += 1;
            }
        }
    }
    if !warnings.is_empty() {
        s.notify(NotificationLevel::Warning, "Some unsaved changes could not be restored", warnings.join("\n"));
    }
    report
}

/// Restore one buffer. `Ok(None)`: nothing differs from the file (the backup can go).
fn restore_one(s: &mut Session, file: &Path, b: Backup) -> Result<Option<(Option<PathBuf>, Conflict)>, String> {
    let (doc, conflict) = match &b.path {
        Some(path) => {
            let exists = path.is_file();
            if path.exists() && !exists {
                return Err(format!("{} is not a file", path.display()));
            }
            let doc = match s.ws.doc_by_path(path) {
                Some(d) => d,
                None if exists => {
                    s.open_file(path).map_err(|e| e.to_string())?;
                    s.ws.doc_by_path(path).ok_or("cannot open the file")?
                }
                None => s.ws.add_document(Some(path.clone()), ""),
            };
            let current = s.ws.doc(doc).map(|d| d.doc.text()).unwrap_or_default();
            if exists && current == b.text {
                return Ok(None);
            }
            let conflict = if !exists {
                Conflict::DeletedOnDisk
            } else if b.disk_mtime_ms.is_some() && mtime_ms(path) != b.disk_mtime_ms {
                Conflict::ChangedOnDisk
            } else {
                Conflict::None
            };
            (doc, conflict)
        }
        None => {
            if b.text.is_empty() {
                return Ok(None);
            }
            let doc = s.ws.add_document(None, "");
            if let Some(od) = s.ws.doc_mut(doc) {
                od.language = b.language.unwrap_or_default();
            }
            (doc, Conflict::None)
        }
    };
    // The restored text is one undoable edit: Undo goes back to what is on disk.
    let editor = match s.ws.editor_for_doc(doc) {
        Some(e) => Some(e),
        None => s.ws.open_editor(doc),
    };
    let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
    let od = s.ws.doc_mut(doc).ok_or("the document closed")?;
    od.doc.replace_all(&b.text, &sels).map_err(|e| e.to_string())?;
    if conflict == Conflict::ChangedOnDisk {
        // Keep the old baseline, so the conflict is found again if the app quits before the user
        // decides (the kept file is rewritten with this time).
        od.disk_mtime = b.disk_mtime_ms.map(|ms| UNIX_EPOCH + Duration::from_millis(ms));
    }
    let rev = od.doc.revision();
    if let Some(e) = editor {
        let len = s.ws.doc(doc).map(|d| d.doc.len_chars()).unwrap_or(0);
        s.ws.set_selections(e, sels.clamp(len));
        s.after_text_change(e);
    }
    claim(file);
    s.hot_exit.owned.insert(doc, Owned { file: file.to_path_buf(), rev, bytes: b.text.len() });
    s.hot_exit.restored.insert(doc, Restored { key: b.key, conflict, saved_ms: b.saved_ms });
    Ok(Some((b.path, conflict)))
}

// ------------------------------------------------------------------------------ commands

/// The `doc` param, else the active editor's document.
fn doc_param(s: &Session, p: &Value) -> Result<DocId, CommandError> {
    if let Some(n) = opt_usize(p, "doc")? {
        let id = DocId(u64::try_from(n).unwrap_or(u64::MAX));
        return s.ws.doc(id).map(|_| id).ok_or_else(|| CommandError::BadParams(format!("no document {n}")));
    }
    s.ws.active().map(|(_, d)| d.id).ok_or_else(|| CommandError::Disabled("no editor is open".into()))
}

fn has_restored(s: &Session) -> Result<(), String> {
    if s.hot_exit.restored.is_empty() { Err("no restored unsaved changes".into()) } else { Ok(()) }
}

fn has_editor_or_docs(s: &Session) -> Result<(), String> {
    if s.ws.docs().next().is_some() { Ok(()) } else { Err("no document is open".into()) }
}

fn conflict_label(c: Conflict) -> &'static str {
    match c {
        Conflict::None => "none",
        Conflict::ChangedOnDisk => "changedOnDisk",
        Conflict::DeletedOnDisk => "deletedOnDisk",
    }
}

fn state(s: &mut Session, _: &Value) -> CmdResult {
    let root = project_root(s);
    let buffers: Vec<Value> = s
        .hot_exit
        .owned_files()
        .into_iter()
        .map(|(d, f)| {
            let od = s.ws.doc(d);
            json!({ "doc": d.0, "path": od.and_then(|o| o.path.clone()), "title": od.map(|o| o.title()), "file": f })
        })
        .collect();
    let restored: Vec<Value> = s
        .hot_exit
        .restored
        .iter()
        .map(|(d, r)| {
            json!({
                "doc": d.0,
                "path": s.ws.doc(*d).and_then(|o| o.path.clone()),
                "conflict": conflict_label(r.conflict),
                "savedMs": r.saved_ms,
            })
        })
        .collect();
    let skipped: Vec<Value> = s.hot_exit.skipped.iter().map(|(d, why)| json!({ "doc": d.0, "reason": why })).collect();
    Ok(json!({
        "enabled": s.hot_exit.enabled,
        "on": s.ws.settings.hot_exit,
        "dirs": candidate_dirs(&s.hot_exit, root.as_deref()),
        "buffers": buffers,
        "restored": restored,
        "skipped": skipped,
    }))
}

fn backup_now(s: &mut Session, _: &Value) -> CmdResult {
    if !s.hot_exit.enabled {
        return Err(CommandError::Failed("hot exit is not used in this session".into()));
    }
    if !s.ws.settings.hot_exit {
        return Err(CommandError::Failed("hot exit is off (Settings › System Settings)".into()));
    }
    let r = sync(s, true);
    let dir = s.hot_exit.dir.as_ref().map(|(_, d)| d.clone());
    s.status = format!("Kept {} unsaved buffer(s)", s.hot_exit.owned.len());
    Ok(json!({ "written": r.written, "removed": r.removed, "skipped": r.skipped, "dir": dir }))
}

/// The text of `path` on disk, as the editor would show it.
fn disk_text(path: &Path) -> Result<String, String> {
    crate::misc::encoding::read_document(path).map(|(t, _, _)| t.replace("\r\n", "\n"))
}

fn compare_with_disk(s: &mut Session, p: &Value) -> CmdResult {
    let doc = doc_param(s, p)?;
    let od = s.ws.doc(doc).ok_or_else(|| CommandError::Failed("the document closed".into()))?;
    let path = od.path.clone().ok_or_else(|| CommandError::Failed("an untitled buffer has no file".into()))?;
    let mine = od.doc.text();
    let name = od.title();
    let disk = if path.is_file() { disk_text(&path).map_err(CommandError::Failed)? } else { String::new() };
    let identical = disk == mine;
    s.misc.compare = Some(CompareView {
        title: format!("{name}: disk vs unsaved changes"),
        left_title: if path.is_file() { format!("{} (on disk)", path.display()) } else { "(deleted on disk)".into() },
        right_title: "Unsaved changes".into(),
        left: disk,
        right: mine,
    });
    Ok(json!({ "identical": identical }))
}

fn keep_changes(s: &mut Session, p: &Value) -> CmdResult {
    let doc = doc_param(s, p)?;
    let was = s.hot_exit.restored.remove(&doc).map(|r| r.conflict);
    if was.is_none() {
        return Err(CommandError::Failed("this buffer has no restored changes".into()));
    }
    s.status = "The restored changes are kept; saving writes them to the file".into();
    Ok(json!({ "conflict": was.map(conflict_label) }))
}

fn discard(s: &mut Session, p: &Value) -> CmdResult {
    let doc = doc_param(s, p)?;
    s.hot_exit.restored.remove(&doc);
    let od = s.ws.doc(doc).ok_or_else(|| CommandError::Failed("the document closed".into()))?;
    match od.path.clone() {
        Some(path) => {
            let text = if path.is_file() { disk_text(&path).map_err(CommandError::Failed)? } else { String::new() };
            let editor = s.ws.editor_for_doc(doc);
            let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if let Some(od) = s.ws.doc_mut(doc) {
                if od.doc.text() != text {
                    od.doc.replace_all(&text, &sels).map_err(|e| CommandError::Failed(e.to_string()))?;
                }
                od.doc.mark_saved();
                od.disk_mtime = mtime;
            }
            if let Some(e) = editor {
                let len = s.ws.doc(doc).map(|d| d.doc.len_chars()).unwrap_or(0);
                s.ws.set_selections(e, sels.clamp(len));
                s.after_text_change(e);
            }
        }
        None => {
            // An untitled buffer: discarding closes it.
            for e in s.ws.editors_for_doc(doc) {
                s.ws.close_editor(e);
            }
        }
    }
    if let Some(o) = s.hot_exit.owned.remove(&doc) {
        release(&o.file);
        if !s.hot_exit.send(WriteMsg::Remove(o.file.clone())) {
            let _ = remove_file(&o.file);
        }
    }
    s.status = "Discarded the restored changes".into();
    Ok(Value::Null)
}

fn restore_cmd(s: &mut Session, _: &Value) -> CmdResult {
    if !s.hot_exit.enabled {
        return Err(CommandError::Failed("hot exit is not used in this session".into()));
    }
    let r = restore(s);
    serde_json::to_value(r).map_err(|e| CommandError::Failed(e.to_string()))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "hotExit.state",
            "Unsaved Changes Kept",
            None,
            "→ {enabled, on (the setting), dirs, buffers: [{doc, path, title, file}], restored: [{doc, path, conflict: \
             none|changedOnDisk|deletedOnDisk, savedMs}], skipped: [{doc, reason}]}",
            Context::Global,
            crate::commands::always,
            state,
        ),
        spec(
            "hotExit.keepNow",
            "Keep Unsaved Changes Now",
            None,
            "→ {written, removed, skipped, dir} — write every unsaved and untitled buffer to `.codecraft/unsaved/` now",
            Context::Global,
            crate::commands::always,
            backup_now,
        ),
        spec(
            "hotExit.restore",
            "Restore Unsaved Changes",
            None,
            "→ {files, untitled, conflicts, dropped} — bring back the kept buffers of the open project",
            Context::Global,
            crate::commands::always,
            restore_cmd,
        ),
        spec(
            "hotExit.compareWithDisk",
            "Compare with Disk",
            None,
            "{doc?: int} → {identical} — show the unsaved changes against the file on disk (the active editor's \
             document by default)",
            Context::Global,
            has_editor_or_docs,
            compare_with_disk,
        ),
        spec(
            "hotExit.keepChanges",
            "Keep Restored Changes",
            None,
            "{doc?: int} — hide the restored-changes banner; a file that changed on disk can be saved over again",
            Context::Global,
            has_restored,
            keep_changes,
        ),
        spec(
            "hotExit.discard",
            "Discard Restored Changes",
            None,
            "{doc?: int} — go back to the file on disk (an untitled buffer closes) and delete its kept copy",
            Context::Global,
            has_restored,
            discard,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_safe_and_unique() {
        let a = new_key();
        let b = new_key();
        assert_ne!(a, b);
        assert!(valid_key(&a) && valid_key(&b));
        for bad in ["", ".x", "-x", "a/b", "..", "a b", &"x".repeat(97)] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn fallback_dirs_are_per_project() {
        let mut st = HotExitState::default();
        st.fallback_dir = Some("/cfg/unsaved".into());
        let a = st.fallback_for(Some(Path::new("/x/proj"))).unwrap();
        let b = st.fallback_for(Some(Path::new("/y/proj"))).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("/cfg/unsaved"));
        assert_eq!(st.fallback_for(None).unwrap(), PathBuf::from("/cfg/unsaved/no-project"));
        assert_eq!(candidate_dirs(&st, Some(Path::new("/x/proj")))[0], PathBuf::from("/x/proj/.codecraft/unsaved"));
    }

    #[test]
    fn hostile_backup_files_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("b-1.json"), "{ nope").unwrap();
        std::fs::write(d.join("b-2.json"), json!({ "key": "x", "text": "t", "path": "relative/x.rs" }).to_string())
            .unwrap();
        std::fs::write(d.join("b-3.json"), json!({ "key": "../../etc", "text": "ok" }).to_string()).unwrap();
        std::fs::write(d.join(".hidden.json"), "{}").unwrap();
        std::fs::create_dir(d.join("b-4.json")).unwrap();
        let huge = std::fs::File::create(d.join("b-5.json")).unwrap();
        huge.set_len(MAX_READ_BYTES + 1).unwrap();
        let mut warnings = Vec::new();
        let found = read_backups(&[d.to_path_buf(), d.join("missing")], &mut warnings);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1.key, "b-3", "the file name is the key");
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(d.join("b-1.json.bad").exists(), "corrupt files are moved aside");
        assert!(read_backups(&[d.to_path_buf()], &mut Vec::new()).len() == 1, "and reported once");
    }
}
