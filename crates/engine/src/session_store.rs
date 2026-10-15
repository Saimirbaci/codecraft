//! Session restore (M10 track A): each project's editors (split groups, tabs, carets, scroll,
//! folds, pinned tabs) and tool-window layout are saved as a
//! [`SessionSnapshot`](codecraft_workspace::SessionSnapshot) in `<project>/.codecraft/workspace.json`
//! (or, when the project directory is not writable, under the config directory keyed by the
//! project path) and restored when the project opens again, like RustRover's `workspace.xml`.
//!
//! With [`SessionStore::autosave`] on (the desktop app), the session is saved when the project
//! closes or another one replaces it, when the window closes, and every 30 s while it changes
//! (written on a worker thread), so a crash loses little. Tests and the CLI leave it off; the
//! `session.save`/`session.restore`/`session.state` commands work either way.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_workspace::{DocId, NotificationLevel, RestoreSummary, SessionSnapshot};

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::opt_path;
use crate::{CommandError, Effect, Session};

/// The snapshot file inside a project.
pub const FILE: &str = ".codecraft/workspace.json";
/// How often a changed session is saved while autosave is on.
pub const AUTOSAVE_EVERY: Duration = Duration::from_secs(30);

/// Session persistence switches and bookkeeping (one per [`Session`]).
#[derive(Debug, Default)]
pub struct SessionStore {
    /// Save the session on project close/switch, window close and periodically when it changed.
    pub autosave: bool,
    /// Restore a project's saved session when it opens.
    pub restore_on_open: bool,
    /// Where snapshots of projects with a read-only directory go; `None` = `workspaces/` under
    /// [`config_dir`]. Tests and embedders set it.
    pub fallback_dir: Option<PathBuf>,
    /// The project root and JSON last written (or being written), to skip unchanged saves.
    last_saved: Option<(PathBuf, String)>,
    last_check: Option<Instant>,
    /// Held while a snapshot file is written, so a final save never races an autosave thread.
    write_lock: Arc<Mutex<()>>,
}

impl SessionStore {
    /// Turn saving and restoring on together (what the desktop app does).
    pub fn enable(&mut self) {
        self.autosave = true;
        self.restore_on_open = true;
    }

    /// The fallback snapshot file for `root`.
    fn fallback_file(&self, root: &Path) -> Option<PathBuf> {
        let dir = match &self.fallback_dir {
            Some(d) => d.clone(),
            None => config_dir()?.join("workspaces"),
        };
        let name: String = root
            .file_name()
            .map(|n| {
                n.to_string_lossy().chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_')).take(40).collect()
            })
            .unwrap_or_default();
        Some(dir.join(format!("{name}-{:016x}.json", fnv1a(root.to_string_lossy().as_bytes()))))
    }
}

/// The app's configuration directory: `$CODECRAFT_CONFIG_DIR`, else `~/Library/Application
/// Support/CodeCraft` on macOS and `$XDG_CONFIG_HOME/codecraft` (or `~/.config/codecraft`) elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODECRAFT_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/CodeCraft")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("codecraft")
    })
}

/// 64-bit FNV-1a: a stable (across builds) key for a project path.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The session snapshot, with each tab's column selection mode (kept by the engine).
pub fn snapshot(s: &Session) -> SessionSnapshot {
    s.ws.session_snapshot_with(|id| s.editing.column_mode.contains(&id))
}

fn project_root(s: &Session) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.clone())
}

/// The snapshot file to read for `root`: the project's own, or the fallback one, whichever is
/// newer when both exist.
pub fn saved_file(store: &SessionStore, root: &Path) -> Option<PathBuf> {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let own = Some(root.join(FILE)).filter(|p| p.is_file());
    let fallback = store.fallback_file(root).filter(|p| p.is_file());
    match (own, fallback) {
        (Some(a), Some(b)) => Some(if modified(&b) > modified(&a) { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// Write `json` for `root`: into the project, else into the fallback directory.
fn write_snapshot(store: &SessionStore, root: &Path, json: &str) -> Result<PathBuf, String> {
    let _guard = store.write_lock.lock().unwrap_or_else(PoisonError::into_inner);
    write_files(root, store.fallback_file(root), json)
}

fn write_files(root: &Path, fallback: Option<PathBuf>, json: &str) -> Result<PathBuf, String> {
    let own = root.join(FILE);
    // Never recreate a project directory that went away.
    let own_err = if root.is_dir() {
        match write_file(&own, json) {
            Ok(()) => return Ok(own),
            Err(e) => e,
        }
    } else {
        format!("{} does not exist", root.display())
    };
    let fallback = fallback.ok_or_else(|| format!("{own_err}; no configuration directory for a fallback"))?;
    write_file(&fallback, json).map(|()| fallback).map_err(|e| format!("{own_err}; {e}"))
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    codecraft_fs::write_text_file_atomic(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn to_json(snap: &SessionSnapshot) -> Result<String, String> {
    serde_json::to_string_pretty(snap).map_err(|e| format!("cannot serialize the session: {e}"))
}

/// Save the open project's session now. `Ok(None)` without a project.
pub fn save_now(s: &mut Session) -> Result<Option<PathBuf>, String> {
    let Some(root) = project_root(s) else { return Ok(None) };
    let json = to_json(&snapshot(s))?;
    let path = write_snapshot(&s.session_store, &root, &json)?;
    s.session_store.last_saved = Some((root, json));
    Ok(Some(path))
}

/// Read the saved snapshot of `root`. A corrupt file is moved aside to `<file>.bad` (so it is
/// reported once) and is an error.
pub fn load(store: &SessionStore, root: &Path) -> Result<Option<(PathBuf, SessionSnapshot)>, String> {
    let Some(path) = saved_file(store, root) else { return Ok(None) };
    let parsed = codecraft_fs::read_text_file(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| SessionSnapshot::from_json(&text));
    match parsed {
        Ok(snap) => Ok(Some((path, snap))),
        Err(e) => {
            let mut bad = path.clone().into_os_string();
            bad.push(".bad");
            if let Err(re) = std::fs::rename(&path, &bad) {
                log::warn!("cannot move aside {}: {re}", path.display());
            }
            Err(format!("{}: {e}", path.display()))
        }
    }
}

/// Open the snapshot's files (through the engine, so language detection and the language server
/// see them) and rebuild the editors. Missing or unreadable files are skipped.
pub fn restore(s: &mut Session, snap: &SessionSnapshot) -> RestoreSummary {
    let mut docs: Vec<(PathBuf, Option<(DocId, bool)>)> = Vec::new();
    for path in snap.paths() {
        let doc = open_doc(s, &path);
        docs.push((path, doc));
    }
    let summary =
        s.ws.restore_session(snap, |p| docs.iter().find(|(q, _)| q == p).and_then(|(_, d)| d.map(|(id, _)| id)));
    for id in &summary.column_mode {
        s.editing.column_mode.insert(*id);
    }
    // Documents opened for tabs the restore dropped (a group the tree does not show, over a cap)
    // are closed again.
    for (_, doc) in &docs {
        if let Some((id, true)) = doc
            && let Some(od) = s.ws.drop_unshown_doc(*id)
            && od.lsp_synced.is_some()
            && let Some(path) = &od.path
        {
            s.lsp_did_close(path);
        }
    }
    if summary.tabs > 0 {
        s.effects.push(Effect::FocusEditor);
    }
    summary
}

/// The document for `path` and whether it was opened just now (without a tab). `None` if it is
/// not a readable text file.
fn open_doc(s: &mut Session, path: &Path) -> Option<(DocId, bool)> {
    let path = crate::normalize_path(path);
    if let Some(d) = s.ws.doc_by_path(&path) {
        return Some((d, false));
    }
    if !path.is_file() {
        return None;
    }
    let text =
        codecraft_fs::read_text_file(&path).map_err(|e| log::info!("not restoring {}: {e}", path.display())).ok()?;
    let id = s.ws.add_document(Some(path.clone()), &text);
    if let Some(od) = s.ws.doc_mut(id) {
        let meta = std::fs::metadata(&path).ok();
        od.disk_mtime = meta.as_ref().and_then(|m| m.modified().ok());
        od.read_only = meta.is_some_and(|m| m.permissions().readonly());
    }
    s.lsp_did_open(id);
    Some((id, true))
}

/// Before the project closes (or another replaces it): save its session if autosave is on.
pub(crate) fn save_on_close(s: &mut Session) {
    if !s.session_store.autosave {
        return;
    }
    if let Err(e) = save_now(s) {
        log::warn!("cannot save the session: {e}");
    }
}

/// After a project opened: restore its saved session if that is on.
pub(crate) fn restore_on_open(s: &mut Session) {
    if !s.session_store.restore_on_open {
        return;
    }
    let Some(root) = project_root(s) else { return };
    match load(&s.session_store, &root) {
        Ok(Some((_, snap))) => {
            let summary = restore(s, &snap);
            if !summary.skipped.is_empty() {
                log::info!("session restore skipped {} missing file(s)", summary.skipped.len());
            }
            // What was just restored needs no save until it changes.
            if let Ok(json) = to_json(&snapshot(s)) {
                s.session_store.last_saved = Some((root, json));
            }
        }
        Ok(None) => {}
        Err(e) => s.notify(NotificationLevel::Warning, "Cannot restore the editors", e),
    }
}

/// Periodic save (called from `Session::pump`): every [`AUTOSAVE_EVERY`], when the session
/// changed, write it on a worker thread.
pub(crate) fn autosave(s: &mut Session) {
    let store = &mut s.session_store;
    if !store.autosave {
        return;
    }
    let now = Instant::now();
    match store.last_check {
        Some(t) if now.duration_since(t) < AUTOSAVE_EVERY => return,
        None => {
            store.last_check = Some(now);
            return;
        }
        _ => store.last_check = Some(now),
    }
    let Some(root) = project_root(s) else { return };
    let Ok(json) = to_json(&snapshot(s)) else { return };
    let store = &mut s.session_store;
    if store.last_saved.as_ref().is_some_and(|(r, j)| *r == root && *j == json) {
        return;
    }
    store.last_saved = Some((root.clone(), json.clone()));
    let lock = store.write_lock.clone();
    let fallback = store.fallback_file(&root);
    let spawned = std::thread::Builder::new().name("codecraft-session-save".into()).spawn(move || {
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(e) = write_files(&root, fallback, &json) {
            log::warn!("cannot save the session: {e}");
        }
    });
    if let Err(e) = spawned {
        log::warn!("cannot start the session save thread: {e}");
        s.session_store.last_saved = None;
    }
}

// ---------------------------------------------------------------- commands

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "session.state",
            "Session State",
            None,
            "→ the session snapshot: {version, project, split (tree whose leaves index `groups`), groups: [{tabs: [{path, pinned, selections: [{anchor: {line, column}, head}], primary, scroll_line, scroll_x, folded}], active}], focused, layout}",
            Context::Global,
            always,
            state,
        ),
        spec(
            "session.save",
            "Save Session",
            None,
            "{path?: string} — save the open editors and layout to `path`, or to the project's `.codecraft/workspace.json` (the config directory when the project is read-only) → {file, groups, tabs}",
            Context::Global,
            always,
            save,
        ),
        spec(
            "session.restore",
            "Restore Session",
            None,
            "{snapshot?: object, path?: string} — reopen editors from a snapshot (as `session.state` returns), a snapshot file, or the project's saved session; missing files are skipped; tabs open now stay → {file, groups, tabs, skipped}",
            Context::Global,
            always,
            restore_cmd,
        ),
    ]
}

fn state(s: &mut Session, _: &Value) -> CmdResult {
    serde_json::to_value(snapshot(s)).map_err(|e| CommandError::Failed(e.to_string()))
}

fn save(s: &mut Session, p: &Value) -> CmdResult {
    let snap = snapshot(s);
    let (groups, tabs) = (snap.groups.len(), snap.tab_count());
    let file = match opt_path(p, "path") {
        Some(path) => {
            let path = crate::normalize_path(&path);
            let json = to_json(&snap).map_err(CommandError::Failed)?;
            write_file(&path, &json).map_err(CommandError::Failed)?;
            path
        }
        None => match save_now(s).map_err(CommandError::Failed)? {
            Some(path) => path,
            None => return Err(CommandError::Failed("no project is open; pass `path`".into())),
        },
    };
    s.status = format!("Session saved ({tabs} tab(s))");
    Ok(json!({ "file": file, "groups": groups, "tabs": tabs }))
}

fn restore_cmd(s: &mut Session, p: &Value) -> CmdResult {
    let (file, snap) = if let Some(v) = p.get("snapshot").filter(|v| !v.is_null()) {
        let snap: SessionSnapshot =
            serde_json::from_value(v.clone()).map_err(|e| CommandError::BadParams(format!("snapshot: {e}")))?;
        (None, snap)
    } else if let Some(path) = opt_path(p, "path") {
        let path = crate::normalize_path(&path);
        let text = codecraft_fs::read_text_file(&path).map_err(|e| CommandError::Failed(e.to_string()))?;
        let snap =
            SessionSnapshot::from_json(&text).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
        (Some(path), snap)
    } else {
        let root = project_root(s)
            .ok_or_else(|| CommandError::Failed("no project is open; pass `path` or `snapshot`".into()))?;
        match load(&s.session_store, &root).map_err(CommandError::Failed)? {
            Some((path, snap)) => (Some(path), snap),
            None => return Err(CommandError::Failed(format!("no saved session for {}", root.display()))),
        }
    };
    let summary = restore(s, &snap);
    s.status = match summary.skipped.len() {
        0 => format!("Restored {} tab(s)", summary.tabs),
        n => format!("Restored {} tab(s); {n} file(s) are missing", summary.tabs),
    };
    Ok(json!({ "file": file, "groups": summary.groups, "tabs": summary.tabs, "skipped": summary.skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_names_are_stable_and_distinct() {
        let store = SessionStore { fallback_dir: Some("/cfg".into()), ..Default::default() };
        let a = store.fallback_file(Path::new("/x/my proj")).unwrap();
        let b = store.fallback_file(Path::new("/y/my proj")).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, store.fallback_file(Path::new("/x/my proj")).unwrap());
        assert!(a.starts_with("/cfg"));
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("myproj-"));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert!(store.fallback_file(Path::new("/")).is_some());
    }
}
