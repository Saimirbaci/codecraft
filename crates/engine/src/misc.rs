//! Menu sweep (M10 track F): state and helpers behind the smaller menu items: preview tabs,
//! file encodings and BOMs, file type associations, Power Save Mode, background processes,
//! Compare With, Database Changes, Context Info and the GitHub/GitLab account status.
//! Commands are in `misc_cmds.rs`.

pub mod encoding;
pub mod export;
pub mod fill;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError, bounded};
use serde::{Deserialize, Serialize};

use codecraft_fs::FileMask;
use codecraft_syntax::Language;
use codecraft_workspace::{EditorId, Settings};

use crate::{LspStatus, Session};

/// What the Processes window lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessesMode {
    /// Run › Stop Background Processes…: everything that can be stopped.
    Stop,
    /// Run › Show Running List: running run configurations.
    Running,
    /// Window › Processes › Show: background tasks (indexing, loading) and processes.
    Tasks,
}

/// A background process the user can stop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// `run`, `debug`, `languageServer`, `agent`, `database` or `git`.
    pub kind: String,
    pub id: String,
    pub title: String,
    pub detail: String,
}

/// Compare With… / Compare with Clipboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareView {
    pub title: String,
    pub left_title: String,
    pub right_title: String,
    pub left: String,
    pub right: String,
}

/// File › File Properties › Associate with File Type… dialog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssociateDialog {
    pub pattern: String,
    pub language: String,
}

/// Git › GitHub/GitLab › Manage Accounts… result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsView {
    pub title: String,
    pub text: String,
    pub loading: bool,
    /// How to add an account (shown, never run).
    pub hint: String,
}

#[derive(Default)]
pub struct MiscState {
    /// Preview editors and the document revision they were opened at.
    pub(crate) preview_revs: BTreeMap<EditorId, u64>,
    pub compare: Option<CompareView>,
    /// Compare With…: the file chooser is open (with this filter).
    pub compare_picker: Option<String>,
    /// Compare with Clipboard ran without text: the shell may supply the system clipboard.
    pub clipboard_request: bool,
    pub processes: Option<ProcessesMode>,
    pub encoding_dialog: bool,
    pub associate: Option<AssociateDialog>,
    pub hidden_tabs: bool,
    pub accounts: Option<AccountsView>,
    accounts_rx: Option<Receiver<String>>,
    /// Test hook: the `gh`/`glab` programs to run instead of the ones on `PATH`.
    pub(crate) cli_override: BTreeMap<&'static str, PathBuf>,
    /// Colour tokens (`keyword` → `#RRGGBB`) for HTML export; the shell keeps them in sync with
    /// its theme.
    pub palette: BTreeMap<String, String>,
    /// Background tasks were running at the last pump (Processes › Auto Show).
    tasks_seen: bool,
}

impl std::fmt::Debug for MiscState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiscState").field("processes", &self.processes).field("compare", &self.compare.is_some()).finish()
    }
}

impl Session {
    /// File › Power Save Mode is on: skip background work that only adds polish.
    pub fn power_save(&self) -> bool {
        self.ws.settings.power_save_mode
    }
}

// ---------------------------------------------------------------- file types

/// Every language, for Associate with File Type.
pub const LANGUAGES: [Language; 17] = [
    Language::Rust,
    Language::Toml,
    Language::Markdown,
    Language::Json,
    Language::Yaml,
    Language::Shell,
    Language::C,
    Language::Cpp,
    Language::Python,
    Language::Zig,
    Language::TypeScript,
    Language::JavaScript,
    Language::Sql,
    Language::Dockerfile,
    Language::Http,
    // Django/Jinja templates (M13 track ST).
    Language::DjangoTemplate,
    Language::PlainText,
];

/// A language by display name (`Rust`, `C++`, `Plain text`), case-insensitively; LSP ids work too.
pub fn language_by_name(name: &str) -> Option<Language> {
    let n = name.trim();
    LANGUAGES.into_iter().find(|l| l.name().eq_ignore_ascii_case(n) || l.lsp_id().eq_ignore_ascii_case(n))
}

/// The language of `path`: a user association (file name glob) first, else by extension.
pub fn language_for(settings: &Settings, path: &Path) -> Language {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    settings
        .file_type_associations
        .iter()
        .filter(|(pattern, _)| !pattern.trim().is_empty() && FileMask::parse(pattern).matches(name))
        .find_map(|(_, lang)| language_by_name(lang))
        .unwrap_or_else(|| Language::from_path(path))
}

/// A glob for associating `path`: `*.ext`, or the file name when it has no extension.
pub fn default_pattern(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("*.{ext}"),
        None => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

/// Re-detect the language of every open document (after an association changed).
pub(crate) fn reapply_languages(s: &mut Session) {
    let changed: Vec<_> = s
        .ws
        .docs()
        .filter_map(|d| {
            let lang = language_for(&s.ws.settings, d.path.as_deref()?);
            (lang != d.language).then_some((d.id, lang))
        })
        .collect();
    for (id, lang) in changed {
        if let Some(od) = s.ws.doc_mut(id) {
            od.language = lang;
            od.semantic = None;
            od.inlay_hints = None;
            od.lsp_synced = None;
        }
        s.lsp_did_open(id);
    }
}

// ---------------------------------------------------------------- opening files

/// After `Session::open_file` showed `editor`: an explicit open keeps a preview tab, and the
/// group's tab limit closes surplus tabs.
pub(crate) fn after_open(s: &mut Session, editor: EditorId) {
    if let Some(e) = s.ws.editor_mut(editor) {
        e.preview = false;
    }
    s.misc.preview_revs.remove(&editor);
    enforce_tab_limit(s, editor);
}

/// Close the leftmost unpinned, unmodified tabs of the focused group beyond the tab limit.
fn enforce_tab_limit(s: &mut Session, keep: EditorId) {
    let limit = s.ws.settings.editor_tab_limit;
    if limit == 0 {
        return;
    }
    loop {
        let tabs = s.ws.tabs().to_vec();
        if tabs.len() <= limit {
            return;
        }
        let victim = tabs.into_iter().find(|id| {
            *id != keep
                && s.ws.editor(*id).is_some_and(|e| !e.pinned)
                && s.ws.editor(*id).and_then(|e| s.ws.doc(e.doc)).is_some_and(|d| !d.doc.is_modified())
        });
        match victim {
            Some(v) => crate::window_cmds::close(s, v),
            None => return,
        }
    }
}

/// The focused group's preview tab, if it is still a preview.
pub fn preview_editor(s: &Session) -> Option<EditorId> {
    s.ws.tabs().iter().copied().find(|id| s.ws.editor(*id).is_some_and(|e| e.preview))
}

/// Open `path` in the preview tab (single click in the Project view): replaces the current
/// preview tab, keeps its place in the tab bar, and leaves the keyboard focus where it is.
pub(crate) fn open_preview(s: &mut Session, path: &Path) -> Result<EditorId, crate::CommandError> {
    let path = crate::normalize_path(path);
    let shown = s.ws.doc_by_path(&path).and_then(|d| s.ws.editor_for_doc(d));
    if !s.ws.settings.preview_tab || shown.is_some() {
        if let Some(e) = shown {
            // Already open: just select it (a preview stays a preview).
            s.ws.set_active_editor(e);
            return Ok(e);
        }
        return s.open_file(&path);
    }
    let old = preview_editor(s);
    let old_index = old.and_then(|o| s.ws.tabs().iter().position(|t| *t == o));
    let focus_effects = s.effects.iter().filter(|e| **e == crate::Effect::FocusEditor).count();
    let id = s.open_file(&path)?;
    // Keep the focus in the Project view.
    let mut seen = 0usize;
    s.effects.retain(|e| {
        if *e == crate::Effect::FocusEditor {
            seen += 1;
            return seen <= focus_effects;
        }
        true
    });
    if let Some(old) = old.filter(|o| *o != id) {
        crate::window_cmds::close(s, old);
        if let Some(i) = old_index {
            let g = s.ws.focused_group();
            s.ws.move_tab(id, g, i);
        }
    }
    let rev = s.ws.editor(id).and_then(|e| s.ws.doc(e.doc)).map(|d| d.doc.revision()).unwrap_or(0);
    if let Some(e) = s.ws.editor_mut(id) {
        e.preview = true;
    }
    s.misc.preview_revs.insert(id, rev);
    Ok(id)
}

/// Make a preview tab permanent (Keep Tab Open, an edit, a double click).
pub(crate) fn keep_open(s: &mut Session, id: EditorId) -> bool {
    s.misc.preview_revs.remove(&id);
    match s.ws.editor_mut(id) {
        Some(e) if e.preview => {
            e.preview = false;
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- database changes

/// One table's pending grid edits (View › Tool Windows › Database Changes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableChanges {
    pub result: u64,
    pub title: String,
    pub source: String,
    pub pending_rows: usize,
    /// The statements Submit runs.
    pub statements: Vec<String>,
    pub submitting: bool,
    /// Why it cannot be submitted (no primary key, …).
    pub not_editable: Option<String>,
    /// Why the last submit failed.
    pub error: Option<String>,
}

/// Every result with pending edits.
pub fn database_changes(s: &Session) -> Vec<TableChanges> {
    s.db.results
        .iter()
        .filter(|r| !r.edits.is_empty())
        .map(|r| TableChanges {
            result: r.id,
            title: r.title.clone(),
            source: r.source.clone(),
            pending_rows: r.edits.pending_rows(),
            statements: s.db.edit_statements(r.id).unwrap_or_default(),
            submitting: r.edits.submitting,
            not_editable: s.db.editable(r.id).err(),
            error: r.edits.error.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------- background processes

/// Processes that Stop Background Processes can stop.
pub fn processes(s: &Session) -> Vec<ProcessInfo> {
    let mut v = Vec::new();
    for r in s.ws.runs.iter().filter(|r| s.runs.is_running(r.id)) {
        v.push(ProcessInfo {
            kind: "run".into(),
            id: r.id.0.to_string(),
            title: r.title.clone(),
            detail: r.command_line.clone(),
        });
    }
    if s.debug.is_active() {
        v.push(ProcessInfo { kind: "debug".into(), id: "0".into(), title: "Debug session".into(), detail: String::new() });
    }
    for (kind, l) in &s.servers {
        v.push(ProcessInfo {
            kind: "languageServer".into(),
            id: format!("{kind:?}"),
            title: l.name.clone(),
            detail: kind.label().into(),
        });
    }
    for j in s.agents.jobs.iter().filter(|j| j.is_running()) {
        v.push(ProcessInfo {
            kind: "agent".into(),
            id: j.id.0.to_string(),
            title: j.title.clone(),
            detail: format!("{:?}", j.backend),
        });
    }
    if s.db.running() {
        v.push(ProcessInfo {
            kind: "database".into(),
            id: "queries".into(),
            title: "Database queries".into(),
            detail: String::new(),
        });
    }
    if s.vcs.op.is_some() {
        v.push(ProcessInfo { kind: "git".into(), id: "op".into(), title: "Git operation".into(), detail: String::new() });
    }
    v
}

/// Stop one background process. False if it is not running (any more).
pub(crate) fn stop_process(s: &mut Session, kind: &str, id: &str) -> bool {
    match kind {
        "run" => match id.parse::<u64>() {
            Ok(n) => crate::runs::stop(s, codecraft_workspace::RunTabId(n)),
            Err(_) => false,
        },
        "debug" => s.debug.is_active() && crate::debug::stop(s),
        "languageServer" => {
            let Some(k) = s.servers.keys().copied().find(|k| format!("{k:?}") == id) else { return false };
            let Some(l) = s.servers.remove(&k) else { return false };
            // Shutting down waits for the process; do it off the UI thread.
            std::thread::spawn(move || l.shutdown());
            let status = LspStatus::Stopped("stopped by the user; restart it from the status bar".into());
            if k == codecraft_lsp::ServerKind::RustAnalyzer {
                s.lsp_status = status.clone();
            }
            s.server_status.insert(k, status);
            for od in s.ws.docs_mut() {
                if crate::server_kind(od.language) == Some(k) {
                    od.lsp_synced = None;
                }
            }
            true
        }
        "agent" => match id.parse::<u64>() {
            Ok(n) => crate::agents::stop(s, crate::agents::JobId(n)).is_ok(),
            Err(_) => false,
        },
        "database" => s.db.cancel(None) > 0,
        "git" => crate::vcs::cancel(s),
        _ => false,
    }
}

// ---------------------------------------------------------------- accounts

/// Run `<program> auth status` in the background; the result shows in the accounts window.
pub(crate) fn start_accounts(s: &mut Session, program: &'static str, title: &str, hint: &str) {
    let exe = s.misc.cli_override.get(program).cloned().unwrap_or_else(|| PathBuf::from(program));
    let (tx, rx) = bounded(1);
    let name = program.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(auth_status(&exe, &name));
    });
    s.misc.accounts_rx = Some(rx);
    s.misc.accounts =
        Some(AccountsView { title: title.into(), text: String::new(), loading: true, hint: hint.into() });
}

/// Output of `auth status` (it prints to stderr), or how to install the CLI.
fn auth_status(exe: &Path, name: &str) -> String {
    let child = std::process::Command::new(exe)
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("NO_COLOR", "1")
        .env("GH_PROMPT_DISABLED", "1")
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!("`{name}` was not found on PATH. Install it to manage accounts.");
        }
        Err(e) => return format!("cannot run `{name}`: {e}"),
    };
    // `auth status` may reach the network: give up after a while instead of waiting forever.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("`{name} auth status` did not answer in time.");
            }
        }
    }
    match child.wait_with_output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let text: String = crate::runs::strip_ansi(text.trim()).chars().take(20_000).collect();
            if text.is_empty() { format!("`{name} auth status` printed nothing.") } else { text }
        }
        Err(e) => format!("cannot read `{name}` output: {e}"),
    }
}

// ---------------------------------------------------------------- pump

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    // An edit makes a preview tab permanent; closed editors drop out.
    let revs: Vec<(EditorId, u64)> = s.misc.preview_revs.iter().map(|(e, r)| (*e, *r)).collect();
    for (id, rev) in revs {
        let now = s.ws.editor(id).filter(|e| e.preview).and_then(|e| s.ws.doc(e.doc)).map(|d| d.doc.revision());
        if now != Some(rev) {
            keep_open(s, id);
            changed = true;
        }
    }
    if let Some(rx) = &s.misc.accounts_rx {
        match rx.try_recv() {
            Ok(text) => {
                if let Some(v) = s.misc.accounts.as_mut() {
                    v.text = text;
                    v.loading = false;
                }
                s.misc.accounts_rx = None;
                changed = true;
            }
            Err(TryRecvError::Disconnected) => {
                if let Some(v) = s.misc.accounts.as_mut() {
                    v.text = "the account check stopped unexpectedly".into();
                    v.loading = false;
                }
                s.misc.accounts_rx = None;
                changed = true;
            }
            Err(TryRecvError::Empty) => {}
        }
    }
    // Window › Processes › Auto Show.
    let tasks = !s.ws.tasks.is_empty();
    if tasks && !s.misc.tasks_seen && s.ws.settings.processes_auto_show && s.misc.processes.is_none() {
        s.misc.processes = Some(ProcessesMode::Tasks);
        changed = true;
    }
    s.misc.tasks_seen = tasks;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associations_override_extension_detection() {
        let mut st = Settings::default();
        assert_eq!(language_for(&st, Path::new("/p/a.inc")), Language::PlainText);
        st.file_type_associations.insert("*.inc".into(), "rust".into());
        st.file_type_associations.insert("Justfile".into(), "Shell Script".into());
        st.file_type_associations.insert("*.bad".into(), "Klingon".into());
        assert_eq!(language_for(&st, Path::new("/p/a.INC")), Language::Rust);
        assert_eq!(language_for(&st, Path::new("/p/Justfile")), Language::Shell);
        assert_eq!(language_for(&st, Path::new("/p/x.bad")), Language::PlainText);
        assert_eq!(language_for(&st, Path::new("/p/x.py")), Language::Python);
        assert_eq!(default_pattern(Path::new("/p/a.inc")), "*.inc");
        assert_eq!(default_pattern(Path::new("/p/Justfile")), "Justfile");
        assert_eq!(language_by_name("c++"), Some(Language::Cpp));
        assert_eq!(language_by_name("plaintext"), Some(Language::PlainText));
    }
}
