//! IDE server for external agents: Claude Code connects to CodeCraft the way it connects to
//! VS Code or JetBrains IDEs (`/ide`, or automatically from a terminal CodeCraft launched).
//!
//! The server (`codecraft-mcp`) runs while a project is open and writes the discovery lock file
//! for it. Tool calls arrive as events; [`pump`] answers them against the session. `openDiff`
//! becomes an approval card ([`crate::approvals`]): the reply waits until the user accepts
//! (the edit is applied through the open document, undoable, and saved) or rejects it.
//! Selection changes are pushed to Claude Code, debounced.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::{Value, json};

use codecraft_mcp::{
    ClientId, ClientSummary, IdeServer, Reply, ServerConfig, ServerEvent, ToolCall, ToolResult, tools,
};
use codecraft_text::{Selection, SelectionSet};
use codecraft_workspace::{DocId, NotificationLevel, Problem, Severity};

use crate::approvals::{ApprovalId, ApprovalKind, Decision};
use crate::{Effect, Session};

/// Name in Claude Code's `/ide` list, in the lock file and as the approval source.
pub const IDE_NAME: &str = "CodeCraft";
const APPROVAL_SOURCE: &str = "Claude Code";
const SELECTION_DEBOUNCE: Duration = Duration::from_millis(100);

/// An `openDiff` waiting for the user's decision.
struct PendingDiff {
    approval: ApprovalId,
    decision: Receiver<Decision>,
    reply: Reply,
    client: ClientId,
    request: Value,
    path: PathBuf,
    proposed: String,
    tab_name: String,
}

struct Running {
    server: IdeServer,
    events: Receiver<ServerEvent>,
    root: PathBuf,
    diffs: Vec<PendingDiff>,
}

/// What identifies a selection for change detection (cheap to compute every frame).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectionKey {
    doc: DocId,
    path: Option<PathBuf>,
    anchor: usize,
    head: usize,
    revision: u64,
}

pub struct IdeServerState {
    enabled: bool,
    lock_dir: Option<PathBuf>,
    running: Option<Running>,
    /// The project root a start failed for (not retried until restart or another project).
    failed_root: Option<PathBuf>,
    last_error: Option<String>,
    /// Last selection seen and when it changed (debounce), and the last one sent.
    observed: Option<(SelectionKey, Instant)>,
    sent: Option<SelectionKey>,
    /// The most recent selection payload (for `getLatestSelection`).
    latest_selection: Option<Value>,
}

impl Default for IdeServerState {
    fn default() -> Self {
        Self {
            enabled: true,
            lock_dir: None,
            running: None,
            failed_root: None,
            last_error: None,
            observed: None,
            sent: None,
            latest_selection: None,
        }
    }
}

impl IdeServerState {
    /// Whether the server should run when a project is open (`ide.enable` / `ide.disable`).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The port Claude Code connects to, while running.
    pub fn port(&self) -> Option<u16> {
        self.running.as_ref().map(|r| r.server.port())
    }

    /// The lock file Claude Code discovers the server through, while running.
    pub fn lock_path(&self) -> Option<PathBuf> {
        self.running.as_ref().and_then(|r| r.server.lock_path().map(Path::to_path_buf))
    }

    pub fn clients(&self) -> Vec<ClientSummary> {
        self.running.as_ref().map(|r| r.server.clients()).unwrap_or_default()
    }

    pub fn client_count(&self) -> usize {
        self.running.as_ref().map_or(0, |r| r.server.client_count())
    }

    /// `openDiff` requests waiting for the user.
    pub fn pending_diffs(&self) -> usize {
        self.running.as_ref().map_or(0, |r| r.diffs.len())
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Environment for a terminal or process CodeCraft launches, so `claude` started there
    /// connects to this IDE on its own. Empty while the server is not running.
    ///
    /// `CLAUDE_CODE_SSE_PORT` picks this server's lock file; `FORCE_CODE_TERMINAL` makes
    /// `claude` treat the terminal as an IDE terminal (auto-connect); `ENABLE_IDE_INTEGRATION`
    /// is the older switch; loopback is added to `NO_PROXY`/`no_proxy` so an HTTP proxy in the
    /// environment never intercepts the WebSocket.
    pub fn env_for_child(&self) -> Vec<(String, String)> {
        let Some(port) = self.port() else { return Vec::new() };
        let mut env = vec![
            (codecraft_mcp::ENV_PORT.to_string(), port.to_string()),
            (codecraft_mcp::ENV_ENABLE.to_string(), "true".to_string()),
            ("FORCE_CODE_TERMINAL".to_string(), "true".to_string()),
        ];
        for key in ["NO_PROXY", "no_proxy"] {
            env.push((key.to_string(), with_loopback(std::env::var(key).ok().as_deref())));
        }
        env
    }

    /// Write lock files to `dir` instead of Claude Code's directory (`~/.claude/ide`). Also
    /// lets the server run in sessions without background services (`Session::lsp_enabled`
    /// off), which otherwise never touch the user's real lock directory.
    pub fn set_lock_dir(&mut self, dir: Option<PathBuf>) {
        self.lock_dir = dir;
        self.failed_root = None;
    }

    /// Whether the UI should keep polling (a client is connected and may be waiting on us).
    pub fn busy(&self) -> bool {
        self.client_count() > 0
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.failed_root = None;
    }

    pub(crate) fn status_json(&self) -> Value {
        let clients: Vec<Value> = self
            .clients()
            .iter()
            .map(|c| json!({ "id": c.id.0, "initialized": c.initialized, "client": c.info }))
            .collect();
        json!({
            "enabled": self.enabled,
            "running": self.is_running(),
            "port": self.port(),
            "lockFile": self.lock_path(),
            "root": self.running.as_ref().map(|r| r.root.clone()),
            "clients": clients,
            "pendingDiffs": self.pending_diffs(),
            "error": self.last_error,
            "env": self.env_for_child().into_iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        })
    }
}

/// A `NO_PROXY` list with `127.0.0.1` and `localhost` added (kept once).
fn with_loopback(existing: Option<&str>) -> String {
    let mut items: Vec<String> =
        existing.unwrap_or_default().split(',').map(str::trim).filter(|i| !i.is_empty()).map(str::to_string).collect();
    for host in ["127.0.0.1", "localhost"] {
        if !items.iter().any(|i| i == host) {
            items.push(host.to_string());
        }
    }
    items.join(",")
}

// ---------------------------------------------------------------- lifecycle

/// The project the server should serve now, if any.
fn wanted_root(s: &Session) -> Option<PathBuf> {
    if !s.ide.enabled || !(s.lsp_enabled || s.ide.lock_dir.is_some()) {
        return None;
    }
    s.ws.project.as_ref().map(|p| p.root.clone())
}

/// Start, stop or move the server to match the open project.
fn reconcile(s: &mut Session) -> bool {
    let want = wanted_root(s);
    let have = s.ide.running.as_ref().map(|r| r.root.clone());
    if want == have || (have.is_none() && want.is_some() && want == s.ide.failed_root) {
        return false;
    }
    stop(s);
    let Some(root) = want else { return true };
    let Some(lock_dir) = s.ide.lock_dir.clone().or_else(codecraft_mcp::lockfile::default_lock_dir) else {
        s.ide.last_error = Some("no home directory for ~/.claude/ide".into());
        s.ide.failed_root = Some(root);
        return true;
    };
    // Claude Code compares its (symlink-resolved) working directory with these folders.
    let mut folders = vec![root.clone()];
    if let Ok(real) = root.canonicalize()
        && real != root
    {
        folders.push(real);
    }
    let mut config = ServerConfig::claude_code(IDE_NAME, folders, Some(lock_dir));
    // CodeCraft's engine tools next to Claude Code's IDE tools (M14 track AX).
    config.tools.extend(codecraft_mcp::engine_tools::engine_tools());
    match IdeServer::start(config) {
        Ok((server, events)) => {
            s.ide.last_error = None;
            s.ide.failed_root = None;
            s.ide.running = Some(Running { server, events, root, diffs: Vec::new() });
        }
        Err(e) => {
            log::warn!("IDE server failed to start: {e}");
            s.ide.last_error = Some(e.to_string());
            s.ide.failed_root = Some(root);
        }
    }
    true
}

/// Stop the server: pending diffs are rejected and their cards removed.
fn stop(s: &mut Session) {
    let Some(mut running) = s.ide.running.take() else { return };
    for d in running.diffs.drain(..) {
        s.approvals.decide(d.approval, Decision::Deny);
        d.reply.send(rejected(&d.tab_name));
    }
    running.server.stop();
    s.ide.observed = None;
    s.ide.sent = None;
}

/// Stop and start again (`ide.restart`).
pub(crate) fn restart(s: &mut Session) {
    stop(s);
    s.ide.failed_root = None;
    reconcile(s);
}

/// Bring the server in line with the settings now (used by the commands).
pub(crate) fn apply_settings(s: &mut Session) {
    s.ide.failed_root = None;
    reconcile(s);
}

/// Drain background events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = reconcile(s);
    let events: Vec<ServerEvent> = match &s.ide.running {
        Some(r) => r.events.try_iter().collect(),
        None => return changed,
    };
    for event in events {
        changed = true;
        handle_event(s, event);
    }
    changed |= poll_diffs(s);
    track_selection(s);
    changed
}

/// Stop background work (window closing).
pub(crate) fn shutdown(s: &mut Session) {
    stop(s);
}

fn handle_event(s: &mut Session, event: ServerEvent) {
    match event {
        ServerEvent::Connected { client } => log::info!("Claude Code client {} connected", client.0),
        ServerEvent::Initialized { client_info, .. } => {
            let name = client_info.get("name").and_then(Value::as_str).unwrap_or("Claude Code");
            s.status = format!("{name} connected to {IDE_NAME}");
            // Let the new client know where the user is right away.
            s.ide.sent = None;
            s.ide.observed = None;
        }
        ServerEvent::ToolCall(call) => handle_tool(s, call),
        ServerEvent::Cancelled { client, request } => {
            // An engine tool waiting for a card, a stop, test results… (M15 AI).
            crate::agent_tools::cancel(s, &connection(client), &request);
            let Some(r) = &mut s.ide.running else { return };
            let (gone, keep): (Vec<_>, Vec<_>) =
                r.diffs.drain(..).partition(|d| d.client == client && d.request == request);
            r.diffs = keep;
            for d in gone {
                s.approvals.decide(d.approval, Decision::Deny);
                d.reply.send(rejected(&d.tab_name));
            }
        }
        ServerEvent::Disconnected { client } => {
            // Its waiting engine tools go, and their approval cards with them (M15 AI).
            crate::agent_tools::client_gone(s, &connection(client));
            let Some(r) = &mut s.ide.running else { return };
            let (gone, keep): (Vec<_>, Vec<_>) = r.diffs.drain(..).partition(|d| d.client == client);
            r.diffs = keep;
            for d in gone {
                s.approvals.decide(d.approval, Decision::Deny);
            }
            if r.server.client_count() == 0 {
                s.status = "Claude Code disconnected".into();
            }
        }
    }
}

// ---------------------------------------------------------------- tools

/// The connection name of an IDE server client in [`crate::agent_tools::CallKey`].
fn connection(client: ClientId) -> String {
    format!("ide:{}", client.0)
}

fn handle_tool(s: &mut Session, call: ToolCall) {
    let ToolCall { client, request, name, arguments, reply } = call;
    let args = if arguments.is_object() { arguments } else { json!({}) };
    // Engine tools (debugging, analysis, profiling, …) go through the access policy (M14 AX).
    if codecraft_mcp::engine_tools::is_engine_tool(&name) {
        let info = s.ide.clients().into_iter().find(|c| c.id == client).map(|c| c.info).unwrap_or_default();
        let caller = crate::agent_tools::Caller::from_client_info(s, &info, APPROVAL_SOURCE);
        let key = crate::agent_tools::CallKey::new(connection(client), request);
        crate::agent_tools::submit_call(
            s,
            caller,
            Some(key),
            &name,
            args,
            Box::new(move |r| {
                reply.send(r);
            }),
        );
        return;
    }
    let result = match name.as_str() {
        tools::OPEN_DIFF => return open_diff(s, client, request, &args, reply),
        tools::OPEN_FILE => open_file(s, &args),
        tools::GET_CURRENT_SELECTION => {
            ToolResult::json(&current_selection(s).unwrap_or_else(|| no_selection("No active editor found")))
        }
        tools::GET_LATEST_SELECTION => {
            let v = current_selection(s).or_else(|| s.ide.latest_selection.clone());
            ToolResult::json(&v.unwrap_or_else(|| no_selection("No selection available")))
        }
        tools::GET_OPEN_EDITORS => ToolResult::json(&open_editors(s)),
        tools::GET_WORKSPACE_FOLDERS => ToolResult::json(&workspace_folders(s)),
        tools::GET_DIAGNOSTICS => diagnostics(s, &args),
        tools::CHECK_DOCUMENT_DIRTY => check_dirty(s, &args),
        tools::SAVE_DOCUMENT => save_document(s, &args),
        tools::CLOSE_TAB => {
            let tab = args.get("tab_name").and_then(Value::as_str).unwrap_or_default().to_string();
            dismiss_diffs(s, |d| d.tab_name == tab);
            ToolResult::text("TAB_CLOSED")
        }
        tools::CLOSE_ALL_DIFF_TABS => {
            let n = dismiss_diffs(s, |_| true);
            ToolResult::text(format!("CLOSED_{n}_DIFF_TABS"))
        }
        other => ToolResult::error(format!("unknown tool: {other}")),
    };
    reply.send(result);
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// A path from a tool argument: absolute, or relative to the project; `file://` URIs accepted.
pub(crate) fn resolve_path(s: &Session, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = if raw.starts_with("file:") { uri_to_path(raw)? } else { PathBuf::from(raw) };
    let path = if path.is_absolute() { path } else { s.project_root()?.join(path) };
    Some(rebase_to_project(s, crate::normalize_path(&path)))
}

/// Claude Code reports symlink-resolved paths (`/private/tmp/…` for `/tmp/…` on macOS); map a
/// path inside the real project directory onto the project root as CodeCraft opened it, so it
/// matches open documents.
fn rebase_to_project(s: &Session, path: PathBuf) -> PathBuf {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return path };
    if path.starts_with(&root) {
        return path;
    }
    let Ok(real_root) = root.canonicalize() else { return path };
    match path.strip_prefix(&real_root) {
        Ok(rel) => root.join(rel),
        Err(_) => path,
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let parsed: codecraft_lsp::lsp_types::Uri = uri.parse().ok()?;
    codecraft_lsp::uri_to_path(&parsed)
}

fn path_to_uri(path: &Path) -> String {
    codecraft_lsp::path_to_uri(path)
        .map(|u| u.as_str().to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

fn no_selection(message: &str) -> Value {
    json!({ "success": false, "message": message })
}

/// `{line, character}` (zero-based; UTF-16 columns, as editors and LSP count them).
fn lsp_position(doc: &codecraft_text::Document, offset: usize) -> Value {
    let (line, character) = doc.offset_to_utf16(offset);
    json!({ "line": line, "character": character })
}

/// The active editor's primary selection in Claude Code's selection shape.
fn current_selection(s: &Session) -> Option<Value> {
    let (editor, od) = s.ws.active()?;
    let sel = editor.selections.primary();
    let len = od.doc.len_chars();
    let (start, end) = (sel.start().min(len), sel.end().min(len));
    let (file_path, file_url) = match &od.path {
        Some(p) => (p.display().to_string(), path_to_uri(p)),
        None => (od.title(), format!("untitled:{}", od.title())),
    };
    Some(json!({
        "success": true,
        "text": od.doc.slice(start..end),
        "filePath": file_path,
        "fileUrl": file_url,
        "selection": {
            "start": lsp_position(&od.doc, start),
            "end": lsp_position(&od.doc, end),
            "isEmpty": start == end,
        },
    }))
}

fn open_editors(s: &Session) -> Value {
    let active = s.ws.active_editor();
    let tabs: Vec<Value> =
        s.ws.all_tabs()
            .iter()
            .filter_map(|id| {
                let e = s.ws.editor(*id)?;
                let od = s.ws.doc(e.doc)?;
                let uri = od.path.as_deref().map(path_to_uri).unwrap_or_else(|| format!("untitled:{}", od.title()));
                Some(json!({
                    "uri": uri,
                    "isActive": active == Some(*id),
                    "label": od.title(),
                    "languageId": od.language.lsp_id(),
                    "isDirty": od.doc.is_modified(),
                    "isPinned": e.pinned,
                    "fileName": od.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| od.title()),
                }))
            })
            .collect();
    json!({ "tabs": tabs })
}

fn workspace_folders(s: &Session) -> Value {
    match &s.ws.project {
        Some(p) => json!({
            "success": true,
            "folders": [{ "name": p.name, "uri": path_to_uri(&p.root), "path": p.root.display().to_string() }],
            "rootPath": p.root.display().to_string(),
        }),
        None => json!({ "success": true, "folders": [], "rootPath": Value::Null }),
    }
}

/// Severity names as the reference IDE integrations send them.
fn severity_name(sev: Severity) -> &'static str {
    match sev {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Info",
        Severity::Hint => "Hint",
    }
}

fn diagnostic_json(s: &Session, p: &Problem) -> Value {
    // Problems are in char columns; convert to UTF-16 through the open document when there is one.
    let pos = |at: codecraft_text::Position| match s.ws.doc_by_path(&p.path).and_then(|id| s.ws.doc(id)) {
        Some(od) => lsp_position(&od.doc, od.doc.position_to_offset(at)),
        None => json!({ "line": at.line, "character": at.column }),
    };
    let source = match p.source {
        codecraft_workspace::ProblemSource::RustAnalyzer => "rust-analyzer",
        codecraft_workspace::ProblemSource::Cargo => "cargo",
        codecraft_workspace::ProblemSource::Inspection => "codecraft",
        codecraft_workspace::ProblemSource::Analysis => "codecraft-analysis",
    };
    let mut d = json!({
        "message": p.message,
        "severity": severity_name(p.severity),
        "range": { "start": pos(p.start), "end": pos(p.end) },
        "source": source,
    });
    if let (Some(code), Some(obj)) = (&p.code, d.as_object_mut()) {
        obj.insert("code".into(), json!(code));
    }
    d
}

fn diagnostics(s: &Session, args: &Value) -> ToolResult {
    let only = match str_arg(args, "uri").filter(|u| !u.trim().is_empty()) {
        Some(u) => match resolve_path(s, u) {
            Some(p) => Some((u, p)),
            None => return ToolResult::error(format!("not a file URI or path: {u}")),
        },
        None => None,
    };
    let files: Vec<Value> = match only {
        // Claude Code checks that `[0].uri` is the URI it asked for, so echo it verbatim (a
        // file without problems still gets an entry).
        Some((uri, path)) => {
            let list: Vec<Value> = s.ws.problems_for(&path).iter().map(|p| diagnostic_json(s, p)).collect();
            vec![json!({ "uri": uri, "diagnostics": list })]
        }
        None => {
            s.ws.all_problems()
                .iter()
                .map(|(path, list)| {
                    let list: Vec<Value> = list.iter().map(|p| diagnostic_json(s, p)).collect();
                    json!({ "uri": path_to_uri(path), "diagnostics": list })
                })
                .collect()
        }
    };
    ToolResult::json(&Value::Array(files))
}

fn check_dirty(s: &Session, args: &Value) -> ToolResult {
    let Some(raw) = str_arg(args, "filePath") else { return ToolResult::error("filePath is required") };
    let Some(path) = resolve_path(s, raw) else { return ToolResult::error(format!("invalid path: {raw}")) };
    let v = match s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)) {
        Some(od) => json!({
            "success": true,
            "filePath": path.display().to_string(),
            "isDirty": od.doc.is_modified(),
            "isUntitled": false,
        }),
        None => json!({ "success": false, "message": format!("Document not open: {}", path.display()) }),
    };
    ToolResult::json(&v)
}

/// Write one open document to disk (Save for a single file).
fn save_doc(s: &mut Session, id: DocId) -> Result<(), String> {
    let Some(od) = s.ws.doc_mut(id) else { return Err("document is not open".into()) };
    let Some(path) = od.path.clone() else { return Err("the document has no file".into()) };
    codecraft_fs::write_text_file_atomic(&path, &od.doc.to_disk_text()).map_err(|e| e.to_string())?;
    od.doc.mark_saved();
    od.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    s.lsp_did_save(&path);
    Ok(())
}

fn save_document(s: &mut Session, args: &Value) -> ToolResult {
    let Some(raw) = str_arg(args, "filePath") else { return ToolResult::error("filePath is required") };
    let Some(path) = resolve_path(s, raw) else { return ToolResult::error(format!("invalid path: {raw}")) };
    let file = path.display().to_string();
    let Some(id) = s.ws.doc_by_path(&path) else {
        return ToolResult::json(&json!({ "success": false, "message": format!("Document not open: {file}") }));
    };
    let v = match save_doc(s, id) {
        Ok(()) => json!({ "success": true, "filePath": file, "saved": true, "message": "Document saved successfully" }),
        Err(e) => json!({ "success": false, "filePath": file, "message": format!("Failed to save: {e}") }),
    };
    ToolResult::json(&v)
}

/// Char offset of the first match of `needle` at or after char offset `from`.
fn find_text(text: &str, needle: &str, from: usize) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let from_byte = text.char_indices().nth(from).map_or(text.len(), |(b, _)| b);
    let rel = text.get(from_byte..)?.find(needle)?;
    let start_byte = from_byte.checked_add(rel)?;
    let start = text.get(..start_byte)?.chars().count();
    Some((start, start.saturating_add(needle.chars().count())))
}

fn open_file(s: &mut Session, args: &Value) -> ToolResult {
    let Some(raw) = str_arg(args, "filePath") else { return ToolResult::error("filePath is required") };
    let Some(path) = resolve_path(s, raw) else { return ToolResult::error(format!("invalid path: {raw}")) };
    let front = args.get("makeFrontmost").and_then(Value::as_bool).unwrap_or(true);
    let prev_active = s.ws.active_editor();
    let effects_before = s.effects.len();
    let editor = match s.open_file(&path) {
        Ok(e) => e,
        Err(e) => return ToolResult::error(format!("cannot open {}: {e}", path.display())),
    };
    let Some(od) = s.ws.editor(editor).and_then(|e| s.ws.doc(e.doc)) else {
        return ToolResult::error("the editor closed");
    };
    let doc = &od.doc;
    let line_count = doc.len_lines();
    let language = od.language.lsp_id();
    let line_of = |key: &str| {
        args.get(key).and_then(Value::as_u64).map(|n| usize::try_from(n.saturating_sub(1)).unwrap_or(usize::MAX))
    };
    let range = if let Some(start_text) = str_arg(args, "startText").filter(|t| !t.is_empty()) {
        let text = doc.text();
        find_text(&text, start_text, 0).map(|(a, b)| {
            let end = str_arg(args, "endText")
                .filter(|t| !t.is_empty())
                .and_then(|t| find_text(&text, t, b))
                .map_or(b, |(_, e)| e);
            let end = if args.get("selectToEndOfLine").and_then(Value::as_bool).unwrap_or(false) {
                doc.line_end(doc.char_to_line(end))
            } else {
                end
            };
            (a, end)
        })
    } else if let Some(start_line) = line_of("startLine") {
        let last = line_count.saturating_sub(1);
        let start_line = start_line.min(last);
        let end_line = line_of("endLine").unwrap_or(start_line).clamp(start_line, last);
        Some((doc.line_start(start_line), doc.line_end(end_line)))
    } else {
        None
    };
    if let Some((a, b)) = range {
        if front && let Some(cur) = s.ws.current_location() {
            s.ws.push_location(cur);
        }
        s.ws.set_selections(editor, SelectionSet::single(Selection::new(a, b)));
        if let Some(e) = s.ws.editor_mut(editor) {
            e.reveal_caret = true;
        }
    }
    if front {
        return ToolResult::text(format!("Opened file: {}", path.display()));
    }
    // Background open: keep the user's tab and focus.
    if let Some(prev) = prev_active.filter(|p| s.ws.editor(*p).is_some()) {
        s.ws.set_active_editor(prev);
    }
    let tail = s.effects.split_off(effects_before.min(s.effects.len()));
    s.effects.extend(tail.into_iter().filter(|e| *e != Effect::FocusEditor));
    ToolResult::json(&json!({
        "success": true,
        "filePath": path.display().to_string(),
        "languageId": language,
        "lineCount": line_count,
    }))
}

// ---------------------------------------------------------------- openDiff

fn saved(content: &str) -> ToolResult {
    ToolResult::texts([tools::FILE_SAVED.to_string(), content.to_string()])
}

fn rejected(tab_name: &str) -> ToolResult {
    ToolResult::texts([tools::DIFF_REJECTED.to_string(), tab_name.to_string()])
}

fn open_diff(s: &mut Session, client: ClientId, request: Value, args: &Value, reply: Reply) {
    let target =
        str_arg(args, "new_file_path").filter(|p| !p.trim().is_empty()).or_else(|| str_arg(args, "old_file_path"));
    let Some(raw) = target else {
        reply.send(ToolResult::error("new_file_path is required"));
        return;
    };
    let Some(path) = resolve_path(s, raw) else {
        reply.send(ToolResult::error(format!("invalid path: {raw}")));
        return;
    };
    let Some(proposed) = str_arg(args, "new_file_contents").map(str::to_string) else {
        reply.send(ToolResult::error("new_file_contents is required"));
        return;
    };
    let tab_name = str_arg(args, "tab_name").unwrap_or_default().to_string();
    // The original is what the user sees: the open document, else the file, else nothing (new file).
    let original = match s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)) {
        Some(od) => od.doc.text(),
        None => codecraft_fs::read_text_file(&path).unwrap_or_default(),
    };
    let shown =
        s.ws.project.as_ref().map(|p| p.relative(&path).into_owned()).unwrap_or_else(|| path.display().to_string());
    let verb = if path.exists() { "Edit" } else { "Create" };
    let title = format!("{verb} {shown}");
    let kind = ApprovalKind::FileEdit { path: path.clone(), original, proposed: proposed.clone() };
    let (approval, decision) = s.approvals.sink().ask(APPROVAL_SOURCE, title, kind);
    let Some(r) = &mut s.ide.running else {
        reply.send(rejected(&tab_name));
        return;
    };
    r.diffs.push(PendingDiff { approval, decision, reply, client, request, path, proposed, tab_name });
    s.status = format!("Claude Code proposes changes to {shown}");
}

/// Remove pending diffs matching `f` (Claude closed their tabs): their cards go away and the
/// waiting `openDiff` calls are answered as rejected. Returns how many were removed.
fn dismiss_diffs(s: &mut Session, f: impl Fn(&PendingDiff) -> bool) -> usize {
    let Some(r) = &mut s.ide.running else { return 0 };
    let (gone, keep): (Vec<_>, Vec<_>) = r.diffs.drain(..).partition(|d| f(d));
    r.diffs = keep;
    let n = gone.len();
    for d in gone {
        // Make sure the card is in the pending list before removing it.
        s.approvals.pump();
        s.approvals.decide(d.approval, Decision::Deny);
        d.reply.send(rejected(&d.tab_name));
    }
    n
}

/// Answer diffs the user decided on.
fn poll_diffs(s: &mut Session) -> bool {
    let Some(r) = &mut s.ide.running else { return false };
    let mut decided = Vec::new();
    let mut i = 0;
    while i < r.diffs.len() {
        let Some(d) = r.diffs.get(i) else { break };
        match d.decision.try_recv() {
            Ok(decision) => decided.push((r.diffs.remove(i), decision)),
            Err(crossbeam_channel::TryRecvError::Disconnected) => decided.push((r.diffs.remove(i), Decision::Deny)),
            Err(crossbeam_channel::TryRecvError::Empty) => i += 1,
        }
    }
    let changed = !decided.is_empty();
    for (d, decision) in decided {
        match decision {
            Decision::Allow | Decision::AllowAlways => match apply_edit(s, &d.path, &d.proposed) {
                Ok(()) => {
                    s.status = format!("Applied Claude Code's changes to {}", d.path.display());
                    d.reply.send(saved(&d.proposed));
                }
                Err(e) => {
                    s.notify(NotificationLevel::Error, "Cannot apply Claude Code's changes", e.clone());
                    d.reply.send(ToolResult::error(format!("the IDE could not write {}: {e}", d.path.display())));
                }
            },
            Decision::Deny => {
                s.status = "Rejected Claude Code's changes".into();
                d.reply.send(rejected(&d.tab_name));
            }
        }
    }
    changed
}

/// Write `content` to `path`: through the open document (one undo step, then saved) or
/// straight to disk.
pub(crate) fn apply_edit(s: &mut Session, path: &Path, content: &str) -> Result<(), String> {
    if let Some(id) = s.ws.doc_by_path(path) {
        let editor = s.ws.editor_for_doc(id);
        let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        let Some(od) = s.ws.doc_mut(id) else { return Err("document closed".into()) };
        if od.read_only {
            return Err("the file is read-only".into());
        }
        od.doc.replace_all(content, &sels).map_err(|e| e.to_string())?;
        if let Some(e) = editor {
            let len = s.ws.doc(id).map_or(0, |d| d.doc.len_chars());
            s.ws.set_selections(e, sels.clamp(len));
        }
        if let Some(e) = editor {
            s.after_text_change(e);
        }
        return save_doc(s, id);
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let existed = path.exists();
    codecraft_fs::write_text_file_atomic(path, content).map_err(|e| e.to_string())?;
    if !existed {
        s.jobs_rescan();
    }
    Ok(())
}

// ---------------------------------------------------------------- selection notifications

fn selection_key(s: &Session) -> Option<SelectionKey> {
    let (e, od) = s.ws.active()?;
    let sel = e.selections.primary();
    Some(SelectionKey {
        doc: od.id,
        path: od.path.clone(),
        anchor: sel.anchor,
        head: sel.head,
        revision: od.doc.revision(),
    })
}

/// Push `selection_changed` once the selection has been stable for the debounce interval.
fn track_selection(s: &mut Session) {
    let Some(key) = selection_key(s) else { return };
    let now = Instant::now();
    match &s.ide.observed {
        Some((k, _)) if *k == key => {}
        _ => s.ide.observed = Some((key.clone(), now)),
    }
    let Some((_, since)) = &s.ide.observed else { return };
    if s.ide.sent.as_ref() == Some(&key) || now.duration_since(*since) < SELECTION_DEBOUNCE {
        return;
    }
    s.ide.sent = Some(key);
    let Some(payload) = current_selection(s) else { return };
    if let Some(r) = &s.ide.running {
        let mut params = payload.clone();
        if let Some(obj) = params.as_object_mut() {
            obj.remove("success");
        }
        r.server.notify(tools::SELECTION_CHANGED, params);
    }
    s.ide.latest_selection = Some(payload);
}

/// Send the active selection (or caret line) to Claude Code's prompt as an @-mention.
pub(crate) fn send_at_mention(s: &mut Session) -> Result<usize, String> {
    let (e, od) = s.ws.active().ok_or("no editor is open")?;
    let path = od.path.clone().ok_or("the file has not been saved yet")?;
    let sel = e.selections.primary();
    let start_line = od.doc.char_to_line(sel.start());
    let mut end_line = od.doc.char_to_line(sel.end());
    // A selection ending at the start of a line does not include that line.
    if end_line > start_line && od.doc.line_start(end_line) == sel.end() {
        end_line -= 1;
    }
    let r = s.ide.running.as_ref().ok_or("the IDE server is not running")?;
    if r.server.client_count() == 0 {
        return Err("Claude Code is not connected".into());
    }
    let params = json!({ "filePath": path.display().to_string(), "lineStart": start_line, "lineEnd": end_line });
    Ok(r.server.notify(tools::AT_MENTIONED, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_text_uses_char_offsets() {
        let text = "é😀 fn main() {\n    body\n}";
        assert_eq!(find_text(text, "fn", 0), Some((3, 5)));
        assert_eq!(find_text(text, "}", 6), Some((24, 25)));
        assert_eq!(find_text(text, "nope", 0), None);
        assert_eq!(find_text(text, "", 0), None);
        assert_eq!(find_text(text, "fn", 10_000), None);
    }

    #[test]
    fn no_proxy_gets_loopback_once() {
        assert_eq!(with_loopback(None), "127.0.0.1,localhost");
        assert_eq!(with_loopback(Some("corp.example, localhost")), "corp.example,localhost,127.0.0.1");
        assert_eq!(with_loopback(Some("127.0.0.1,localhost")), "127.0.0.1,localhost");
    }

    #[test]
    fn env_is_empty_until_running() {
        let st = IdeServerState::default();
        assert!(st.env_for_child().is_empty());
        assert!(st.is_enabled() && !st.is_running() && st.port().is_none());
    }
}
