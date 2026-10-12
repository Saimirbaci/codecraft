//! The engine side of the language servers: one server per language (rust-analyzer, clangd,
//! a Python server, zls, a TypeScript server), document sync, diagnostics, memory supervision,
//! and routing of responses to popups, navigation and edits.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::{Value, json};

use codecraft_lsp::lsp_types as lt;
use codecraft_lsp::{LspClient, RequestId, ResponseError, ServerConfig, ServerEvent, ServerKind};
use codecraft_syntax::Language;
use codecraft_workspace::{BackgroundTask, DocId, EditorId, FindResults, NotificationLevel, ToolWindowId};

use crate::convert::{self, from_lsp_pos, utf16_col_to_char};
use crate::popup::{ActionItem, CompletionItem, CompletionPopup, LocationItem, StructureItem, SymbolItem};
use crate::{CommandError, LspStatus, Popup, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GotoKind {
    /// Cmd+B: declaration, or usages when already on the declaration.
    DeclarationOrUsages,
    Implementation,
    TypeDefinition,
    /// Alt+Space: show the definition's source in a popup.
    QuickDefinition,
}

pub(crate) enum Pending {
    Goto { kind: GotoKind, editor: EditorId, offset: usize, path: PathBuf },
    Hover { editor: EditorId, offset: usize },
    Completion { editor: EditorId, anchor: usize },
    SignatureHelp { editor: EditorId, offset: usize },
    Formatting { doc: DocId, revision: u64 },
    Rename,
    DocumentSymbol,
    WorkspaceSymbol { query: String },
    References { title: String },
    CodeActions,
    CodeActionResolve,
    ExecuteCommand,
    SemanticTokens { doc: DocId, revision: u64, kind: ServerKind },
    InlayHints { doc: DocId, revision: u64 },
}

/// One running language server.
pub(crate) struct LspState {
    /// "rust-analyzer", "clangd", "basedpyright", …
    pub(crate) name: String,
    client: LspClient,
    events: Receiver<ServerEvent>,
    pending: HashMap<RequestId, Pending>,
    ready: bool,
    /// Process (and process group) id, for memory supervision.
    pub(crate) pid: Option<u32>,
    /// Semantic token legend (when the server provides semantic tokens).
    legend: Option<lt::SemanticTokensLegend>,
    /// The server provides inlay hints.
    inlay: bool,
}

impl LspState {
    pub(crate) fn shutdown(self) {
        if let Err(e) = self.client.shutdown(Duration::from_secs(2)) {
            log::warn!("{} shutdown: {e}", self.name);
        }
    }
}

/// Memory supervision: a background sampler of the servers' process groups.
#[derive(Default)]
pub(crate) struct ServerMemory {
    rx: Option<Receiver<BTreeMap<u32, u64>>>,
    last_sample: Option<Instant>,
}

/// Resident memory (KiB) per process group, from `ps` (no unsafe, works on macOS and Linux).
fn sample_process_groups() -> BTreeMap<u32, u64> {
    let mut out = BTreeMap::new();
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pgid=,rss="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut it = line.split_whitespace();
        if let (Some(pgid), Some(rss)) =
            (it.next().and_then(|v| v.parse::<u32>().ok()), it.next().and_then(|v| v.parse::<u64>().ok()))
        {
            *out.entry(pgid).or_insert(0) += rss;
        }
    }
    out
}

const MEMORY_SAMPLE_EVERY: Duration = Duration::from_secs(10);

/// The server that handles a language.
pub fn server_kind(language: Language) -> Option<ServerKind> {
    match language {
        Language::Rust => Some(ServerKind::RustAnalyzer),
        Language::C | Language::Cpp => Some(ServerKind::Clangd),
        Language::Python => Some(ServerKind::Python),
        Language::Zig => Some(ServerKind::Zls),
        Language::TypeScript | Language::JavaScript => Some(ServerKind::TypeScript),
        _ => None,
    }
}

/// rust-analyzer settings (sent as initializationOptions and for `workspace/configuration`).
fn ra_config(use_clippy: bool, low_memory: bool) -> Value {
    let mut cfg = json!({
        "checkOnSave": true,
        "check": { "command": if use_clippy { "clippy" } else { "check" } },
        "cargo": { "buildScripts": { "enable": true } },
        "procMacro": { "enable": true },
        "workspace": { "symbol": { "search": { "kind": "all_symbols", "limit": 256 } } },
        // RustRover inserts `name()` with the caret inside, not argument placeholders.
        "completion": { "callable": { "snippets": "add_parentheses" } },
        "inlayHints": { "typeHints": { "enable": true }, "parameterHints": { "enable": true } },
    });
    if low_memory && let Some(obj) = cfg.as_object_mut() {
        // Smaller query caches, no eager indexing of every dependency at startup.
        obj.insert("lru".into(), json!({ "capacity": 64 }));
        obj.insert("cachePriming".into(), json!({ "enable": false }));
        obj.insert("numThreads".into(), json!(2));
    }
    cfg
}

pub(crate) const INSTALL_HINT: &str = "rust-analyzer was not found. Install it with `rustup component add rust-analyzer`, \
     or set its path in Settings. Highlighting, Cargo build/run/test and search keep working without it.";

impl Session {
    /// The server kind for the active editor's language.
    fn active_kind(&self) -> Option<ServerKind> {
        self.ws.active().and_then(|(_, d)| server_kind(d.language))
    }

    /// Status of one language server.
    pub fn server_status(&self, kind: ServerKind) -> LspStatus {
        match self.servers.get(&kind) {
            Some(l) if l.ready => LspStatus::Ready,
            Some(_) => LspStatus::Starting,
            None => self.server_status.get(&kind).cloned().unwrap_or_default(),
        }
    }

    /// Resident memory of a server and its helpers, in MiB (when sampled).
    pub fn server_memory_mb(&self, kind: ServerKind) -> Option<u64> {
        self.server_memory_mb.get(&kind).copied()
    }

    /// (semantic tokens, inlay hints) support of a ready server.
    pub(crate) fn server_decoration_support(&self, kind: ServerKind) -> (bool, bool) {
        self.servers.get(&kind).filter(|l| l.ready).map(|l| (l.legend.is_some(), l.inlay)).unwrap_or((false, false))
    }

    pub(crate) fn server_legend(&self, kind: ServerKind) -> Option<lt::SemanticTokensLegend> {
        self.servers.get(&kind).and_then(|l| l.legend.clone())
    }

    /// Display name of a running server ("basedpyright", "clangd", …).
    pub fn server_name(&self, kind: ServerKind) -> Option<&str> {
        self.servers.get(&kind).map(|l| l.name.as_str())
    }

    fn set_status(&mut self, kind: ServerKind, status: LspStatus) {
        if kind == ServerKind::RustAnalyzer {
            self.lsp_status = status.clone();
        }
        self.server_status.insert(kind, status);
    }

    /// Whether the server for the active editor's language is ready.
    pub(crate) fn lsp_ready(&self) -> Result<(), String> {
        let Some(kind) = self.active_kind().or_else(|| self.servers.keys().next().copied()) else {
            return Err("no language server for this file type".into());
        };
        match self.servers.get(&kind) {
            Some(l) if l.ready => Ok(()),
            Some(l) => Err(format!("{} is starting", l.name)),
            None => Err(match self.server_status.get(&kind) {
                Some(LspStatus::Unavailable(m) | LspStatus::Stopped(m)) => m.clone(),
                _ => format!("{} is not running", kind.label()),
            }),
        }
    }

    /// Start (or restart) rust-analyzer for the project root.
    pub(crate) fn start_lsp(&mut self, root: &Path) {
        self.start_server(ServerKind::RustAnalyzer, root);
    }

    /// Start a server of `kind` unless one runs (or it was found unavailable).
    pub(crate) fn ensure_server(&mut self, kind: ServerKind) {
        if self.servers.contains_key(&kind) || !self.lsp_enabled {
            return;
        }
        if matches!(self.server_status.get(&kind), Some(LspStatus::Unavailable(_) | LspStatus::Stopped(_))) {
            return;
        }
        let Some(root) = self.project_root() else { return };
        self.start_server(kind, &root);
    }

    fn start_server(&mut self, kind: ServerKind, root: &Path) {
        if let Some(old) = self.servers.remove(&kind) {
            old.shutdown();
        }
        if !self.lsp_enabled {
            return;
        }
        let command = if kind == ServerKind::RustAnalyzer {
            self.ws
                .settings
                .rust_analyzer_path
                .clone()
                .filter(|p| p.exists())
                .map(|program| codecraft_lsp::ServerCommand { program, args: Vec::new(), name: "rust-analyzer".into() })
                .or_else(|| codecraft_lsp::find_server(kind, root))
        } else {
            codecraft_lsp::find_server(kind, root)
        };
        let Some(command) = command else {
            let hint = if kind == ServerKind::RustAnalyzer {
                INSTALL_HINT.to_string()
            } else {
                format!("{} was not found. Install it with: {}", kind.label(), kind.install_hint())
            };
            self.set_status(kind, LspStatus::Unavailable(hint.clone()));
            self.ws.notify(NotificationLevel::Warning, format!("{} not found", kind.label()), hint);
            return;
        };
        let mut config = ServerConfig::rust_analyzer(command.program.clone(), root.to_path_buf());
        config.args = command.args.clone();
        match kind {
            ServerKind::RustAnalyzer => {
                let settings = ra_config(self.ws.settings.use_clippy, self.ws.settings.rust_analyzer_low_memory);
                config.initialization_options = Some(settings.clone());
                config.configuration = Some(settings);
            }
            ServerKind::Python => {
                // Point pyright/basedpyright at the project's virtualenv when there is one.
                let venv = [".venv/bin/python", "venv/bin/python"].iter().map(|p| root.join(p)).find(|p| p.is_file());
                if let Some(py) = venv {
                    config.configuration = Some(json!({ "pythonPath": py, "analysis": { "autoSearchPaths": true } }));
                }
            }
            _ => {}
        }
        let name = command.name.clone();
        match LspClient::spawn(config) {
            Ok((client, events)) => {
                let pid = client.pid();
                self.servers.insert(
                    kind,
                    LspState {
                        name: name.clone(),
                        client,
                        events,
                        pending: HashMap::new(),
                        ready: false,
                        pid,
                        legend: None,
                        inlay: false,
                    },
                );
                self.set_status(kind, LspStatus::Starting);
                self.ws.tasks.insert(
                    format!("lsp-start:{kind:?}"),
                    BackgroundTask { title: format!("Starting {name}"), message: None, percentage: None },
                );
            }
            Err(e) => {
                let msg = format!("Cannot start {name}: {e}");
                self.set_status(kind, LspStatus::Stopped(msg.clone()));
                self.ws.notify(NotificationLevel::Error, &name, msg);
            }
        }
    }

    /// Restart a server (or every running one), clearing a previous "stopped" state.
    pub(crate) fn restart_server(&mut self, kind: Option<ServerKind>) {
        let Some(root) = self.project_root() else { return };
        let kinds: Vec<ServerKind> = match kind {
            Some(k) => vec![k],
            None => self.servers.keys().copied().collect(),
        };
        for k in kinds {
            self.server_status.remove(&k);
            for od in self.ws.docs_mut() {
                if server_kind(od.language) == Some(k) {
                    od.lsp_synced = None;
                }
            }
            self.start_server(k, &root);
        }
    }

    pub(crate) fn stop_lsp(&mut self) {
        let servers = std::mem::take(&mut self.servers);
        // Shut servers down in parallel so closing a polyglot project stays quick.
        let handles: Vec<_> = servers.into_values().map(|l| std::thread::spawn(move || l.shutdown())).collect();
        for h in handles {
            let _ = h.join();
        }
        self.lsp_status = LspStatus::Off;
        self.server_status.clear();
        self.server_memory_mb.clear();
        for od in self.ws.docs_mut() {
            od.lsp_synced = None;
        }
    }

    fn client_for_path(&self, path: &Path) -> Option<&LspClient> {
        let kind = server_kind(Language::from_path(path))?;
        self.servers.get(&kind).filter(|l| l.ready).map(|l| &l.client)
    }

    /// Tell the language's server about a newly opened document (starting the server if needed).
    pub(crate) fn lsp_did_open(&mut self, doc: DocId) {
        let Some(kind) = self.ws.doc(doc).and_then(|d| server_kind(d.language)) else { return };
        self.ensure_server(kind);
        let Some(l) = self.servers.get(&kind).filter(|l| l.ready) else { return };
        let Some(od) = self.ws.doc_mut(doc) else { return };
        let Some(path) = od.path.clone() else { return };
        if od.lsp_synced.is_some() {
            return;
        }
        od.lsp_version = 1;
        match l.client.did_open(&path, od.language.lsp_id(), od.lsp_version, &od.doc.text()) {
            Ok(()) => od.lsp_synced = Some(od.doc.revision()),
            Err(e) => log::warn!("didOpen {}: {e}", path.display()),
        }
    }

    pub(crate) fn lsp_did_close(&mut self, path: &Path) {
        if let Some(c) = self.client_for_path(path)
            && let Err(e) = c.did_close(path)
        {
            log::warn!("didClose {}: {e}", path.display());
        }
    }

    pub(crate) fn lsp_did_save(&mut self, path: &Path) {
        if let Some(id) = self.ws.doc_by_path(path) {
            self.lsp_sync(id);
        }
        if let Some(c) = self.client_for_path(path)
            && let Err(e) = c.did_save(path)
        {
            log::warn!("didSave {}: {e}", path.display());
        }
    }

    /// Send the document's current text if its server has an older revision.
    pub(crate) fn lsp_sync(&mut self, doc: DocId) {
        let Some(kind) = self.ws.doc(doc).and_then(|d| server_kind(d.language)) else { return };
        let Some(l) = self.servers.get(&kind).filter(|l| l.ready) else { return };
        let Some(od) = self.ws.doc_mut(doc) else { return };
        if !od.needs_lsp_sync() {
            return;
        }
        let Some(path) = od.path.clone() else { return };
        od.lsp_version = od.lsp_version.saturating_add(1);
        match l.client.did_change_full(&path, od.lsp_version, &od.doc.text()) {
            Ok(()) => od.lsp_synced = Some(od.doc.revision()),
            Err(e) => log::warn!("didChange {}: {e}", path.display()),
        }
    }

    /// The active editor's caret in LSP terms, after syncing its document.
    pub(crate) fn caret_for_lsp(&mut self) -> Result<(EditorId, PathBuf, usize, lt::Position), CommandError> {
        self.lsp_ready().map_err(CommandError::Disabled)?;
        let (eid, did) = self.active_ids().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
        self.lsp_sync(did);
        let (e, d) = self.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
        let path = d.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved on disk".into()))?;
        let offset = e.selections.primary().head;
        Ok((eid, path, offset, convert::lsp_pos(&d.doc, offset)))
    }

    /// Send a request to the active editor's server (or the first ready one without an editor).
    pub(crate) fn lsp_send(
        &mut self,
        send: impl FnOnce(&LspClient) -> codecraft_lsp::Result<RequestId>,
        pending: Pending,
    ) -> Result<(), CommandError> {
        let kind = self
            .active_kind()
            .filter(|k| self.servers.get(k).is_some_and(|l| l.ready))
            .or_else(|| self.servers.iter().find(|(_, l)| l.ready).map(|(k, _)| *k))
            .ok_or_else(|| CommandError::Disabled("no language server is ready".into()))?;
        self.lsp_send_to(kind, send, pending)
    }

    pub(crate) fn lsp_send_to(
        &mut self,
        kind: ServerKind,
        send: impl FnOnce(&LspClient) -> codecraft_lsp::Result<RequestId>,
        pending: Pending,
    ) -> Result<(), CommandError> {
        let l = self
            .servers
            .get_mut(&kind)
            .filter(|l| l.ready)
            .ok_or_else(|| CommandError::Disabled(format!("{} is not ready", kind.label())))?;
        let id = send(&l.client).map_err(|e| CommandError::Failed(e.to_string()))?;
        l.pending.insert(id, pending);
        Ok(())
    }

    pub(crate) fn lsp_goto(&mut self, kind: GotoKind) -> Result<(), CommandError> {
        let (editor, path, offset, pos) = self.caret_for_lsp()?;
        let p = path.clone();
        self.lsp_send(
            |c| match kind {
                GotoKind::DeclarationOrUsages | GotoKind::QuickDefinition => c.definition(&p, pos.line, pos.character),
                GotoKind::Implementation => c.implementation(&p, pos.line, pos.character),
                GotoKind::TypeDefinition => c.type_definition(&p, pos.line, pos.character),
            },
            Pending::Goto { kind, editor, offset, path },
        )?;
        self.status = "Searching…".into();
        Ok(())
    }

    pub(crate) fn lsp_references(&mut self, title: String) -> Result<(), CommandError> {
        let (_, path, _, pos) = self.caret_for_lsp()?;
        self.lsp_send(|c| c.references(&path, pos.line, pos.character, true), Pending::References { title })
    }

    pub(crate) fn lsp_hover(&mut self) -> Result<(), CommandError> {
        let (editor, path, offset, pos) = self.caret_for_lsp()?;
        self.lsp_send(|c| c.hover(&path, pos.line, pos.character), Pending::Hover { editor, offset })
    }

    pub(crate) fn lsp_signature_help(&mut self) -> Result<(), CommandError> {
        let (editor, path, offset, pos) = self.caret_for_lsp()?;
        self.lsp_send(|c| c.signature_help(&path, pos.line, pos.character), Pending::SignatureHelp { editor, offset })
    }

    /// Request completion at the caret; `anchor` is the start of the identifier being typed.
    pub(crate) fn lsp_completion(&mut self, anchor: usize) -> Result<(), CommandError> {
        let (editor, path, _, pos) = self.caret_for_lsp()?;
        self.popup = Some(Popup::Completion(CompletionPopup {
            editor,
            anchor,
            items: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            pending: true,
        }));
        self.lsp_send(|c| c.completion(&path, pos.line, pos.character), Pending::Completion { editor, anchor })
    }

    pub(crate) fn lsp_format(&mut self) -> Result<(), CommandError> {
        let (_, path, _, _) = self.caret_for_lsp()?;
        let (_, did) = self.active_ids().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
        let revision = self.ws.doc(did).map(|d| d.doc.revision()).unwrap_or(0);
        let tab = u32::try_from(self.ws.settings.tab_size).unwrap_or(4);
        self.lsp_send(|c| c.formatting(&path, tab, true), Pending::Formatting { doc: did, revision })
    }

    pub(crate) fn lsp_rename(&mut self, editor: EditorId, offset: usize, new_name: &str) -> Result<(), CommandError> {
        self.lsp_ready().map_err(CommandError::Disabled)?;
        let doc = self.ws.editor(editor).map(|e| e.doc).ok_or_else(|| CommandError::Failed("editor closed".into()))?;
        self.lsp_sync(doc);
        let od = self.ws.doc(doc).ok_or_else(|| CommandError::Failed("document closed".into()))?;
        let path = od.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved on disk".into()))?;
        let pos = convert::lsp_pos(&od.doc, offset);
        self.lsp_send(|c| c.rename(&path, pos.line, pos.character, new_name), Pending::Rename)
    }

    pub(crate) fn lsp_document_symbols(&mut self) -> Result<(), CommandError> {
        let (_, path, _, _) = self.caret_for_lsp()?;
        self.lsp_send(|c| c.document_symbol(&path), Pending::DocumentSymbol)
    }

    /// Query every ready server; results are merged as they arrive.
    pub(crate) fn lsp_workspace_symbols(&mut self, query: &str) -> Result<(), CommandError> {
        let ready: Vec<ServerKind> = self.servers.iter().filter(|(_, l)| l.ready).map(|(k, _)| *k).collect();
        if ready.is_empty() {
            return Err(CommandError::Disabled("no language server is ready".into()));
        }
        for kind in ready {
            let q = query.to_string();
            self.lsp_send_to(kind, |c| c.workspace_symbol(&q), Pending::WorkspaceSymbol { query: query.to_string() })?;
        }
        Ok(())
    }

    pub(crate) fn lsp_code_actions(&mut self) -> Result<(), CommandError> {
        let (editor, path, offset, _) = self.caret_for_lsp()?;
        let (range, diagnostics) = {
            let (e, d) = self.ws.active().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
            let sel = e.selections.primary();
            let range = convert::lsp_range(&d.doc, sel.start()..sel.end());
            let pos = d.doc.offset_to_position(offset);
            let diags: Vec<lt::Diagnostic> = self
                .ws
                .problems_for(&path)
                .into_iter()
                .filter(|p| p.start <= pos && pos <= p.end)
                .map(|p| {
                    let s = d.doc.position_to_offset(p.start);
                    let en = d.doc.position_to_offset(p.end);
                    lt::Diagnostic {
                        range: convert::lsp_range(&d.doc, s..en),
                        severity: None,
                        message: p.message.clone(),
                        ..lt::Diagnostic::default()
                    }
                })
                .collect();
            (range, diags)
        };
        self.popup = Some(Popup::Actions {
            editor,
            title: "Context Actions".into(),
            items: Vec::new(),
            selected: 0,
            pending: true,
        });
        self.lsp_send(|c| c.code_action(&path, range, diagnostics), Pending::CodeActions)
    }

    /// Apply a chosen code action (resolving it first if the server wants that).
    pub(crate) fn apply_code_action(&mut self, payload: &Value) -> Result<(), CommandError> {
        if payload.get("edit").is_none() && payload.get("data").is_some() {
            let p = payload.clone();
            return self.lsp_send(|c| c.request_raw("codeAction/resolve", p), Pending::CodeActionResolve);
        }
        self.finish_code_action(payload)
    }

    fn finish_code_action(&mut self, payload: &Value) -> Result<(), CommandError> {
        if let Some(edit) = payload.get("edit") {
            let edit: lt::WorkspaceEdit =
                serde_json::from_value(edit.clone()).map_err(|e| CommandError::Failed(format!("bad edit: {e}")))?;
            self.apply_workspace_edit(&edit)?;
        }
        // A bare Command, or a CodeAction carrying one.
        let command = if payload.get("title").is_some() && payload.get("command").is_some_and(Value::is_string) {
            Some(payload.clone())
        } else {
            payload.get("command").cloned()
        };
        if let Some(cmd) = command
            && let Some(name) = cmd.get("command").and_then(Value::as_str)
        {
            let args = cmd.get("arguments").cloned().unwrap_or(Value::Array(Vec::new()));
            let params = json!({ "command": name, "arguments": args });
            let _ = self.lsp_send(|c| c.request_raw("workspace/executeCommand", params), Pending::ExecuteCommand);
        }
        Ok(())
    }

    // ------------------------------------------------------------ event pump

    pub(crate) fn pump_lsp(&mut self) -> bool {
        let mut changed = false;
        // Sync edited documents (full text; cheap enough at typing speed).
        if self.servers.values().any(|l| l.ready) {
            let dirty: Vec<DocId> = self.ws.docs().filter(|d| d.needs_lsp_sync()).map(|d| d.id).collect();
            for d in dirty {
                self.lsp_sync(d);
            }
        }
        let kinds: Vec<ServerKind> = self.servers.keys().copied().collect();
        for kind in kinds {
            for _ in 0..256 {
                let Some(event) = self.servers.get(&kind).and_then(|l| l.events.try_recv().ok()) else { break };
                changed = true;
                self.handle_lsp_event(kind, event);
            }
        }
        changed |= self.pump_server_memory();
        self.request_decorations();
        changed
    }

    /// Sample server memory every few seconds; restart a server over the configured limit.
    fn pump_server_memory(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.server_memory.rx
            && let Ok(sample) = rx.try_recv()
        {
            self.server_memory.rx = None;
            changed = true;
            let limit = self.ws.settings.lsp_memory_limit_mb;
            let mut over = Vec::new();
            for (kind, l) in &self.servers {
                let Some(pid) = l.pid else { continue };
                let mb = sample.get(&pid).copied().unwrap_or(0) / 1024;
                self.server_memory_mb.insert(*kind, mb);
                if limit > 0 && mb > limit {
                    over.push((*kind, mb));
                }
            }
            for (kind, mb) in over {
                self.ws.notify(
                    NotificationLevel::Warning,
                    kind.label(),
                    format!("used {mb} MiB (limit {limit} MiB); restarting it"),
                );
                self.restart_server(Some(kind));
            }
        }
        let due = self.server_memory.last_sample.is_none_or(|t| t.elapsed() >= MEMORY_SAMPLE_EVERY);
        if !self.servers.is_empty() && self.server_memory.rx.is_none() && due {
            let (tx, rx) = crossbeam_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send(sample_process_groups());
            });
            self.server_memory.rx = Some(rx);
            self.server_memory.last_sample = Some(Instant::now());
        }
        changed
    }

    fn handle_lsp_event(&mut self, kind: ServerKind, event: ServerEvent) {
        let name = self.servers.get(&kind).map(|l| l.name.clone()).unwrap_or_else(|| kind.label().to_string());
        match event {
            ServerEvent::Initialized(caps) => {
                if let Some(l) = self.servers.get_mut(&kind) {
                    l.ready = true;
                    l.legend = match &caps.semantic_tokens_provider {
                        Some(lt::SemanticTokensServerCapabilities::SemanticTokensOptions(o)) => Some(o.legend.clone()),
                        Some(lt::SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(r)) => {
                            Some(r.semantic_tokens_options.legend.clone())
                        }
                        None => None,
                    };
                    l.inlay = caps.inlay_hint_provider.is_some();
                }
                self.set_status(kind, LspStatus::Ready);
                self.ws.tasks.remove(&format!("lsp-start:{kind:?}"));
                let docs: Vec<DocId> =
                    self.ws.docs().filter(|d| server_kind(d.language) == Some(kind)).map(|d| d.id).collect();
                for d in docs {
                    self.lsp_did_open(d);
                }
            }
            ServerEvent::Diagnostics(params) => {
                let Some(path) = codecraft_lsp::uri_to_path(&params.uri) else { return };
                let doc = self.ws.doc_by_path(&path).and_then(|id| self.ws.doc(id)).map(|d| &d.doc);
                let problems =
                    params.diagnostics.iter().map(|d| convert::diagnostic_to_problem(&path, doc, d)).collect();
                self.ws.set_lsp_problems(path, problems);
            }
            ServerEvent::Progress(p) => {
                let key = format!("lsp:{kind:?}:{}", p.token);
                if p.done {
                    self.ws.tasks.remove(&key);
                } else {
                    let entry = self.ws.tasks.entry(key).or_insert(BackgroundTask {
                        title: p.title.clone().unwrap_or_else(|| name.clone()),
                        message: None,
                        percentage: None,
                    });
                    if let Some(t) = p.title {
                        entry.title = t;
                    }
                    entry.message = p.message;
                    entry.percentage = p.percentage;
                }
            }
            ServerEvent::ShowMessage { level, message } => {
                let lvl = if level == lt::MessageType::ERROR {
                    NotificationLevel::Error
                } else if level == lt::MessageType::WARNING {
                    NotificationLevel::Warning
                } else {
                    NotificationLevel::Info
                };
                self.ws.notify(lvl, &name, message);
            }
            ServerEvent::Log { message, .. } => log::debug!("{name}: {message}"),
            ServerEvent::Notification { method, .. } => log::trace!("{name} notification {method}"),
            ServerEvent::ServerRequest { id, method, params } => self.handle_server_request(kind, id, &method, params),
            ServerEvent::Response { id, result, .. } => {
                let pending = self.servers.get_mut(&kind).and_then(|l| l.pending.remove(&id));
                if let Some(p) = pending {
                    self.handle_response(p, result);
                }
            }
            ServerEvent::Exited { code } => {
                let msg = format!("{name} stopped (exit code {})", code.map_or("unknown".into(), |c| c.to_string()));
                self.servers.remove(&kind);
                self.server_memory_mb.remove(&kind);
                self.set_status(kind, LspStatus::Stopped(msg.clone()));
                let prefix = format!("lsp:{kind:?}");
                self.ws.tasks.retain(|k, _| !k.starts_with(&prefix) && *k != format!("lsp-start:{kind:?}"));
                for od in self.ws.docs_mut() {
                    if server_kind(od.language) == Some(kind) {
                        od.lsp_synced = None;
                    }
                }
                self.ws.notify(NotificationLevel::Warning, &name, msg);
            }
            ServerEvent::Error(e) => log::warn!("{name}: {e}"),
        }
    }

    fn handle_server_request(&mut self, kind: ServerKind, id: Value, method: &str, params: Value) {
        let reply = match method {
            "workspace/applyEdit" => {
                let applied = serde_json::from_value::<lt::ApplyWorkspaceEditParams>(params)
                    .map_err(|e| e.to_string())
                    .and_then(|p| self.apply_workspace_edit(&p.edit).map_err(|e| e.to_string()));
                match applied {
                    Ok(_) => Ok(json!({ "applied": true })),
                    Err(e) => Ok(json!({ "applied": false, "failureReason": e })),
                }
            }
            "window/showMessageRequest" => {
                if let Some(msg) = params.get("message").and_then(Value::as_str) {
                    let name = self.server_name(kind).unwrap_or("language server").to_string();
                    self.ws.notify(NotificationLevel::Info, &name, msg);
                }
                Ok(Value::Null)
            }
            "window/showDocument" => {
                let path = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .and_then(|u| u.parse::<lt::Uri>().ok())
                    .and_then(|u| codecraft_lsp::uri_to_path(&u));
                if let Some(p) = path {
                    let _ = self.navigate_to(&p, 0);
                }
                Ok(json!({ "success": true }))
            }
            // The server's analysis changed (e.g. indexing finished): drop cached decorations
            // so the next pump re-requests them.
            "workspace/inlayHint/refresh" | "workspace/semanticTokens/refresh" => {
                let inlays = method.contains("inlayHint");
                for od in self.ws.docs_mut() {
                    if server_kind(od.language) == Some(kind) {
                        if inlays {
                            od.inlay_hints = None;
                        } else {
                            od.semantic = None;
                        }
                    }
                }
                Ok(Value::Null)
            }
            _ => Err(ResponseError { code: -32601, message: format!("unsupported request {method}") }),
        };
        if let Some(l) = self.servers.get(&kind)
            && let Err(e) = l.client.respond(id, reply)
        {
            log::warn!("respond to {method}: {e}");
        }
    }

    fn handle_response(&mut self, pending: Pending, result: Result<Value, ResponseError>) {
        match pending {
            Pending::SemanticTokens { doc, revision, kind } => {
                return self.on_semantic_tokens(doc, revision, kind, result.ok());
            }
            Pending::InlayHints { doc, revision } => return self.on_inlay_hints(doc, revision, result.ok()),
            _ => {}
        }
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                // -32800 = request cancelled, -32801 = content modified: silent.
                if e.code != -32800 && e.code != -32801 {
                    self.status = format!("rust-analyzer: {}", e.message);
                }
                if matches!(pending, Pending::Completion { .. } | Pending::CodeActions | Pending::DocumentSymbol) {
                    self.close_pending_popup();
                }
                return;
            }
        };
        match pending {
            Pending::Goto { kind, editor, offset, path } => self.on_goto(kind, editor, offset, &path, value),
            Pending::Hover { editor, offset } => {
                let hover: Option<lt::Hover> = serde_json::from_value(value).ok().flatten();
                match hover.map(|h| convert::hover_text(&h.contents)).filter(|t| !t.trim().is_empty()) {
                    Some(text) => self.popup = Some(Popup::Hover { editor, offset, text }),
                    None => self.status = "No documentation found".into(),
                }
            }
            Pending::SignatureHelp { editor, offset } => {
                let help: Option<lt::SignatureHelp> = serde_json::from_value(value).ok().flatten();
                let text = help.and_then(|h| {
                    let active = h.active_signature.unwrap_or(0) as usize;
                    h.signatures.get(active).or(h.signatures.first()).map(|s| s.label.clone())
                });
                match text {
                    Some(text) => self.popup = Some(Popup::ParameterInfo { editor, offset, text }),
                    None => self.status = "No parameters".into(),
                }
            }
            Pending::Completion { editor, anchor } => self.on_completion(editor, anchor, value),
            Pending::Formatting { doc, revision } => {
                let edits: Option<Vec<lt::TextEdit>> = serde_json::from_value(value).ok().flatten();
                let Some(edits) = edits.filter(|e| !e.is_empty()) else { return };
                let current = self.ws.doc(doc).map(|d| d.doc.revision());
                if current != Some(revision) {
                    self.status = "Formatting skipped: the file changed".into();
                    return;
                }
                if let Some(path) = self.ws.doc(doc).and_then(|d| d.path.clone())
                    && let Err(e) = self.apply_text_edits_to_path(&path, &edits)
                {
                    self.status = e.to_string();
                }
            }
            Pending::Rename => {
                let edit: Option<lt::WorkspaceEdit> = serde_json::from_value(value).ok().flatten();
                match edit.map(|e| self.apply_workspace_edit(&e)) {
                    Some(Ok(n)) => self.status = format!("Renamed in {n} file(s)"),
                    Some(Err(e)) => self.status = e.to_string(),
                    None => self.status = "Nothing to rename".into(),
                }
            }
            Pending::DocumentSymbol => self.on_document_symbols(value),
            Pending::WorkspaceSymbol { query } => self.on_workspace_symbols(&query, value),
            Pending::References { title } => self.on_references(title, value),
            Pending::CodeActions => self.on_code_actions(value),
            Pending::CodeActionResolve => {
                if let Err(e) = self.finish_code_action(&value) {
                    self.status = e.to_string();
                }
            }
            Pending::ExecuteCommand => {}
            // Handled before the error check above.
            Pending::SemanticTokens { .. } | Pending::InlayHints { .. } => {}
        }
    }

    fn close_pending_popup(&mut self) {
        let pending = match &self.popup {
            Some(Popup::Completion(c)) => c.pending,
            Some(Popup::Actions { pending, .. }) | Some(Popup::FileStructure { pending, .. }) => *pending,
            _ => false,
        };
        if pending {
            self.popup = None;
        }
    }

    /// Locations from a definition-like response.
    fn locations_of(value: Value) -> Vec<(PathBuf, lt::Range)> {
        let resp: Option<lt::GotoDefinitionResponse> = serde_json::from_value(value).ok().flatten();
        let mut out = Vec::new();
        match resp {
            Some(lt::GotoDefinitionResponse::Scalar(l)) => out.push(l),
            Some(lt::GotoDefinitionResponse::Array(v)) => out.extend(v),
            Some(lt::GotoDefinitionResponse::Link(links)) => out
                .extend(links.into_iter().map(|l| lt::Location { uri: l.target_uri, range: l.target_selection_range })),
            None => {}
        }
        out.into_iter().filter_map(|l| codecraft_lsp::uri_to_path(&l.uri).map(|p| (p, l.range))).collect()
    }

    /// Line text of a file (from the open document, else disk).
    fn line_of(&self, path: &Path, line: usize) -> String {
        if let Some(id) = self.ws.doc_by_path(path)
            && let Some(d) = self.ws.doc(id)
        {
            return d.doc.line_text(line).unwrap_or_default();
        }
        codecraft_fs::read_text_file(path)
            .ok()
            .and_then(|t| t.lines().nth(line).map(str::to_string))
            .unwrap_or_default()
    }

    fn location_item(&self, path: &Path, range: &lt::Range) -> LocationItem {
        let text = self.line_of(path, range.start.line as usize);
        LocationItem {
            path: path.to_path_buf(),
            line: range.start.line as usize,
            column: utf16_col_to_char(&text, range.start.character),
            preview: text.trim().to_string(),
        }
    }

    fn on_goto(&mut self, kind: GotoKind, editor: EditorId, offset: usize, origin: &Path, value: Value) {
        self.status.clear();
        let locs = Self::locations_of(value);
        if locs.is_empty() {
            self.status = match kind {
                GotoKind::Implementation => "No implementations found".into(),
                GotoKind::TypeDefinition => "Cannot find type declaration".into(),
                _ => "Cannot find declaration to go to".into(),
            };
            return;
        }
        if kind == GotoKind::QuickDefinition {
            if let Some((path, range)) = locs.first() {
                let start = range.start.line as usize;
                let snippet: Vec<String> = (start..start + 15).map(|l| self.line_of(path, l)).collect();
                let text = format!("```rust\n{}\n```\n{}", snippet.join("\n"), path.display());
                self.popup = Some(Popup::Hover { editor, offset, text });
            }
            return;
        }
        // On the declaration itself → show usages (RustRover's "Declaration or Usages").
        if kind == GotoKind::DeclarationOrUsages
            && let [(path, range)] = locs.as_slice()
            && path == origin
            && let Some(d) = self.ws.editor(editor).and_then(|e| self.ws.doc(e.doc))
        {
            let r = convert::from_lsp_range(&d.doc, range);
            if r.start <= offset && offset <= r.end {
                let name = d.doc.slice(r);
                if let Err(e) = self.lsp_references(format!("Usages of '{name}'")) {
                    self.status = e.to_string();
                }
                return;
            }
        }
        if let [(path, range)] = locs.as_slice() {
            let item = self.location_item(path, range);
            if let Err(e) = self.navigate_to_position(path, item.line, item.column) {
                self.status = e.to_string();
            }
            return;
        }
        let items: Vec<LocationItem> = locs.iter().map(|(p, r)| self.location_item(p, r)).collect();
        let title = match kind {
            GotoKind::Implementation => "Choose Implementation",
            _ => "Choose Declaration",
        };
        self.popup = Some(Popup::Locations { title: title.into(), items, selected: 0 });
    }

    fn on_references(&mut self, title: String, value: Value) {
        let locs: Vec<lt::Location> = serde_json::from_value(value).ok().flatten().unwrap_or_default();
        let mut matches = Vec::new();
        for l in locs.iter().take(5000) {
            let Some(path) = codecraft_lsp::uri_to_path(&l.uri) else { continue };
            let text = self.line_of(&path, l.range.start.line as usize);
            let column = utf16_col_to_char(&text, l.range.start.character);
            let end_column = if l.range.end.line == l.range.start.line {
                utf16_col_to_char(&text, l.range.end.character)
            } else {
                text.chars().count()
            };
            matches.push(codecraft_fs::FileMatch {
                path,
                line: l.range.start.line as usize,
                column,
                end_column,
                line_text: text.chars().take(500).collect(),
            });
        }
        matches.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
        self.status = format!("{} usage(s) found", matches.len());
        self.ws.find = Some(FindResults { title, matches, ..FindResults::default() });
        self.ws.layout.show(ToolWindowId::Find);
    }

    fn on_completion(&mut self, editor: EditorId, anchor: usize, value: Value) {
        let resp: Option<lt::CompletionResponse> = serde_json::from_value(value).ok().flatten();
        let items = match resp {
            Some(lt::CompletionResponse::Array(v)) => v,
            Some(lt::CompletionResponse::List(l)) => l.items,
            None => Vec::new(),
        };
        let Some(Popup::Completion(popup)) = &self.popup else { return };
        if popup.editor != editor || popup.anchor != anchor {
            return;
        }
        let Some(od) = self.ws.editor(editor).and_then(|e| self.ws.doc(e.doc)) else { return };
        let doc = &od.doc;
        let converted: Vec<CompletionItem> = items
            .into_iter()
            .take(2000)
            .map(|it| {
                let snippet = it.insert_text_format == Some(lt::InsertTextFormat::SNIPPET);
                let (raw_text, replace_start) = match &it.text_edit {
                    Some(lt::CompletionTextEdit::Edit(te)) => {
                        (te.new_text.clone(), Some(from_lsp_pos(doc, &te.range.start)))
                    }
                    Some(lt::CompletionTextEdit::InsertAndReplace(ir)) => {
                        (ir.new_text.clone(), Some(from_lsp_pos(doc, &ir.replace.start)))
                    }
                    None => (it.insert_text.clone().unwrap_or_else(|| it.label.clone()), None),
                };
                let (insert_text, cursor) =
                    if snippet { convert::snippet_with_cursor(&raw_text) } else { (raw_text, None) };
                let additional_edits = it
                    .additional_text_edits
                    .unwrap_or_default()
                    .iter()
                    .map(|e| (convert::from_lsp_range(doc, &e.range), e.new_text.clone()))
                    .collect();
                let detail = it
                    .label_details
                    .as_ref()
                    .and_then(|d| d.description.clone().or_else(|| d.detail.clone()))
                    .or(it.detail.clone());
                CompletionItem {
                    filter_text: it.filter_text.clone().unwrap_or_else(|| it.label.clone()),
                    sort_text: it.sort_text.clone().unwrap_or_else(|| it.label.clone()),
                    label: it.label,
                    detail,
                    kind: it.kind.map(completion_kind).unwrap_or("text").to_string(),
                    insert_text,
                    cursor,
                    replace_start,
                    additional_edits,
                    documentation: it.documentation.as_ref().map(convert::documentation_text),
                    deprecated: it.deprecated.unwrap_or(false),
                }
            })
            .collect();
        if let Some(Popup::Completion(popup)) = &mut self.popup {
            popup.items = converted;
            popup.pending = false;
        }
        self.refresh_completion_filter();
    }

    /// Re-filter the completion list against the identifier typed since the anchor; closes it
    /// when the caret left the identifier or nothing matches.
    pub(crate) fn refresh_completion_filter(&mut self) {
        let Some(Popup::Completion(popup)) = &self.popup else { return };
        let Some(e) = self.ws.editor(popup.editor) else {
            self.popup = None;
            return;
        };
        let Some(d) = self.ws.doc(e.doc) else { return };
        let caret = e.selections.primary().head;
        if caret < popup.anchor {
            self.popup = None;
            return;
        }
        let prefix = d.doc.slice(popup.anchor..caret);
        if prefix.chars().any(|c| !(c.is_alphanumeric() || c == '_')) {
            self.popup = None;
            return;
        }
        let pending = popup.pending;
        let mut scored: Vec<(i64, usize)> = popup
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| codecraft_fs::fuzzy_match(&prefix, &it.filter_text).map(|m| (m.score, i)))
            .collect();
        scored.sort_by(|a, b| {
            let sa = popup.items.get(a.1).map(|i| i.sort_text.as_str()).unwrap_or("");
            let sb = popup.items.get(b.1).map(|i| i.sort_text.as_str()).unwrap_or("");
            if prefix.is_empty() { sa.cmp(sb) } else { b.0.cmp(&a.0).then(sa.cmp(sb)) }
        });
        let visible: Vec<usize> = scored.into_iter().map(|(_, i)| i).collect();
        if visible.is_empty() && !pending {
            self.popup = None;
            return;
        }
        if let Some(Popup::Completion(popup)) = &mut self.popup {
            popup.visible = visible;
            popup.selected = popup.selected.min(popup.visible.len().saturating_sub(1));
        }
    }

    fn on_document_symbols(&mut self, value: Value) {
        let resp: Option<lt::DocumentSymbolResponse> = serde_json::from_value(value).ok().flatten();
        let mut items = Vec::new();
        fn walk(syms: &[lt::DocumentSymbol], depth: usize, out: &mut Vec<StructureItem>) {
            if depth > 32 {
                return;
            }
            for s in syms {
                out.push(StructureItem {
                    name: s.name.clone(),
                    kind: symbol_kind(s.kind).into(),
                    detail: s.detail.clone(),
                    depth,
                    line: s.selection_range.start.line as usize,
                    column: s.selection_range.start.character as usize,
                });
                if let Some(children) = &s.children {
                    walk(children, depth + 1, out);
                }
            }
        }
        match resp {
            Some(lt::DocumentSymbolResponse::Nested(v)) => walk(&v, 0, &mut items),
            Some(lt::DocumentSymbolResponse::Flat(v)) => items.extend(v.into_iter().map(|s| StructureItem {
                name: s.name,
                kind: symbol_kind(s.kind).into(),
                detail: s.container_name,
                depth: 0,
                line: s.location.range.start.line as usize,
                column: s.location.range.start.character as usize,
            })),
            None => {}
        }
        if let Some(Popup::FileStructure { items: target, pending, .. }) = &mut self.popup
            && !items.is_empty()
        {
            *target = items;
            *pending = false;
        } else if let Some(Popup::FileStructure { pending, .. }) = &mut self.popup {
            *pending = false;
        }
    }

    fn on_workspace_symbols(&mut self, query: &str, value: Value) {
        let resp: Option<lt::WorkspaceSymbolResponse> = serde_json::from_value(value).ok().flatten();
        let mut items: Vec<SymbolItem> = Vec::new();
        match resp {
            Some(lt::WorkspaceSymbolResponse::Flat(v)) => {
                for s in v {
                    if let Some(path) = codecraft_lsp::uri_to_path(&s.location.uri) {
                        items.push(SymbolItem {
                            name: s.name,
                            kind: symbol_kind(s.kind).into(),
                            container: s.container_name,
                            path,
                            line: s.location.range.start.line as usize,
                            column: s.location.range.start.character as usize,
                        });
                    }
                }
            }
            Some(lt::WorkspaceSymbolResponse::Nested(v)) => {
                for s in v {
                    let (uri, range) = match s.location {
                        lt::OneOf::Left(l) => (l.uri, Some(l.range)),
                        lt::OneOf::Right(w) => (w.uri, None),
                    };
                    if let Some(path) = codecraft_lsp::uri_to_path(&uri) {
                        items.push(SymbolItem {
                            name: s.name,
                            kind: symbol_kind(s.kind).into(),
                            container: s.container_name,
                            path,
                            line: range.map(|r| r.start.line as usize).unwrap_or(0),
                            column: range.map(|r| r.start.character as usize).unwrap_or(0),
                        });
                    }
                }
            }
            None => {}
        }
        match &mut self.popup {
            Some(Popup::GotoSymbol { query: q, items: target, pending, types_only, .. }) if q == query => {
                if *types_only {
                    items.retain(|i| {
                        matches!(i.kind.as_str(), "struct" | "enum" | "trait" | "type" | "union" | "class")
                    });
                }
                for it in items {
                    if !target.contains(&it) {
                        target.push(it);
                    }
                }
                *pending = false;
            }
            Some(Popup::SearchEverywhere { query: q, symbols, pending, .. }) if q == query => {
                for it in items {
                    if !symbols.contains(&it) {
                        symbols.push(it);
                    }
                }
                *pending = false;
            }
            _ => {}
        }
    }

    fn on_code_actions(&mut self, value: Value) {
        let list: Vec<Value> = match value {
            Value::Array(v) => v,
            _ => Vec::new(),
        };
        let items: Vec<ActionItem> = list
            .into_iter()
            .filter_map(|a| {
                let title = a.get("title").and_then(Value::as_str)?.to_string();
                let kind = a.get("kind").and_then(Value::as_str).map(str::to_string);
                Some(ActionItem { title, kind, payload: a })
            })
            .collect();
        if let Some(Popup::Actions { items: target, pending, .. }) = &mut self.popup {
            if items.is_empty() {
                self.popup = None;
                self.status = "No context actions available here".into();
            } else {
                *target = items;
                *pending = false;
            }
        }
    }
}

fn completion_kind(k: lt::CompletionItemKind) -> &'static str {
    match k {
        lt::CompletionItemKind::METHOD => "method",
        lt::CompletionItemKind::FUNCTION | lt::CompletionItemKind::CONSTRUCTOR => "fn",
        lt::CompletionItemKind::FIELD | lt::CompletionItemKind::PROPERTY => "field",
        lt::CompletionItemKind::VARIABLE => "variable",
        lt::CompletionItemKind::CLASS | lt::CompletionItemKind::STRUCT => "struct",
        lt::CompletionItemKind::INTERFACE => "trait",
        lt::CompletionItemKind::MODULE => "mod",
        lt::CompletionItemKind::ENUM => "enum",
        lt::CompletionItemKind::ENUM_MEMBER => "variant",
        lt::CompletionItemKind::KEYWORD => "keyword",
        lt::CompletionItemKind::SNIPPET => "snippet",
        lt::CompletionItemKind::CONSTANT => "const",
        lt::CompletionItemKind::TYPE_PARAMETER => "type",
        _ => "text",
    }
}

fn symbol_kind(k: lt::SymbolKind) -> &'static str {
    match k {
        lt::SymbolKind::FUNCTION => "fn",
        lt::SymbolKind::METHOD => "method",
        lt::SymbolKind::STRUCT | lt::SymbolKind::CLASS => "struct",
        lt::SymbolKind::ENUM => "enum",
        lt::SymbolKind::INTERFACE => "trait",
        lt::SymbolKind::MODULE | lt::SymbolKind::NAMESPACE => "mod",
        lt::SymbolKind::CONSTANT => "const",
        lt::SymbolKind::FIELD | lt::SymbolKind::PROPERTY => "field",
        lt::SymbolKind::ENUM_MEMBER => "variant",
        lt::SymbolKind::TYPE_PARAMETER => "type",
        lt::SymbolKind::VARIABLE => "static",
        lt::SymbolKind::OBJECT => "impl",
        _ => "symbol",
    }
}
