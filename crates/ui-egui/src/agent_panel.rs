//! The Agent tool window: job launcher (backend, mode, prompt, context toggles), the job list,
//! the History of this project's saved sessions (reopen, resume, rename, delete), a live activity
//! feed, follow-ups, and the review of worktree changes with per-file diffs.
//! OWNED BY the agents track; History by the agent-sessions track (M12 AG).

use std::path::{Path, PathBuf};

use egui::{Align, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Modifiers, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent::{Backend, BackendInfo, ChangeKind};
use codecraft_engine::agent_sessions::{self, SessionSummary};
use codecraft_engine::agents::{
    Activity, JobId, JobStatus, ReviewDecision, ReviewFile, ToolState, mode_id, mode_support,
};
use codecraft_engine::approvals::AgentMode;

use crate::diff_view::{self, DiffOptions};
use crate::theme::Theme;
use crate::{UiState, run_command};

/// View state of the Agent tool window (and the approval cards).
pub struct AgentUi {
    pub backend: Backend,
    pub mode: AgentMode,
    pub prompt: String,
    pub attach_file: bool,
    pub attach_selection: bool,
    /// Files and folders the user added with `+`; sent as the job's attachments.
    pub context: Vec<PathBuf>,
    pub selected: Option<JobId>,
    pub follow_up: String,
    /// The review file whose diff is shown (relative path).
    pub review_file: Option<String>,
    pub side_by_side: bool,
    pub show_logs: bool,
    pub approvals_collapsed: bool,
    /// The History list is expanded.
    pub history_open: bool,
    /// A session being renamed: (session id, the new title).
    pub renaming: Option<(String, String)>,
    /// "Delete All" was clicked once; the second click deletes.
    pub confirm_clear: bool,
    /// History search text and filters (status label, backend id; empty = all).
    pub history_query: String,
    pub history_status: String,
    pub history_backend: String,
    /// The Clean Up confirmation: what `agent.sessions.cleanup` would remove.
    pub cleanup: Option<Value>,
    /// Clean Up removes only the worktrees and keeps the sessions in History.
    pub cleanup_worktrees_only: bool,
    /// Connect an External Agent: what `agent.connectExternal` returned, while the window is open.
    pub connect: Option<Value>,
    /// The client shown in that window (its id).
    pub connect_client: String,
    mention_cache: Option<MentionCache>,
    mention_index: usize,
    mention_scroll: bool,
    mention_dismissed: Option<String>,
}

struct MentionCache {
    follow_up: bool,
    query: String,
    root: Option<PathBuf>,
    count: usize,
    result: Value,
    loaded: std::time::Instant,
}

impl Default for AgentUi {
    fn default() -> Self {
        Self {
            backend: Backend::Claude,
            mode: AgentMode::Review,
            prompt: String::new(),
            attach_file: true,
            attach_selection: true,
            context: Vec::new(),
            selected: None,
            follow_up: String::new(),
            review_file: None,
            side_by_side: false,
            show_logs: false,
            approvals_collapsed: false,
            history_open: true,
            renaming: None,
            confirm_clear: false,
            history_query: String::new(),
            history_status: String::new(),
            history_backend: String::new(),
            cleanup: None,
            cleanup_worktrees_only: false,
            connect: None,
            connect_client: "claude".into(),
            mention_cache: None,
            mention_index: 0,
            mention_scroll: false,
            mention_dismissed: None,
        }
    }
}

const MODES: [AgentMode; 3] = [AgentMode::Auto, AgentMode::Ask, AgentMode::Review];

fn mode_label(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Auto => "Auto",
        AgentMode::Ask => "Ask",
        AgentMode::Review => "Review",
    }
}

fn mode_tip(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Auto => "The agent edits the project directly, without asking.",
        AgentMode::Ask => "Approve every file change and command first (Agent Client Protocol).",
        AgentMode::Review => "The agent works in a separate git worktree; accept or reject its changes file by file.",
    }
}

fn status_mark(theme: &Theme, status: JobStatus) -> (&'static str, egui::Color32) {
    match status {
        JobStatus::Preparing | JobStatus::Running => ("●", theme.accent),
        JobStatus::Finished => ("✓", theme.success),
        JobStatus::Failed => ("✗", theme.error),
        JobStatus::Stopped => ("■", theme.text_muted),
        JobStatus::Reviewing => ("●", theme.warning),
        JobStatus::Done => ("✓", theme.text_muted),
        JobStatus::Interrupted => ("⚠", theme.warning),
    }
}

fn rel_display(path: &Path, roots: &[&Path]) -> String {
    roots
        .iter()
        .find_map(|r| path.strip_prefix(r).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn seconds(ms: u64) -> String {
    let s = ms as f64 / 1000.0;
    if s < 60.0 { format!("{s:.1}s") } else { format!("{}m {:02}s", (s / 60.0) as u64, (s % 60.0) as u64) }
}

pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    // Snippets from the editor's Send to Chat.
    for snippet in std::mem::take(&mut s.agents.chat_inbox) {
        if !st.agent.prompt.is_empty() && !st.agent.prompt.ends_with('\n') {
            st.agent.prompt.push('\n');
        }
        st.agent.prompt.push_str(&snippet);
    }
    let backends: Vec<BackendInfo> = s.agents.backends().to_vec();
    new_job(ui, s, st, &theme, &backends);
    ui.add_space(4.0);
    ui.separator();
    history(ui, s, st, &theme);
    if s.agents.jobs.is_empty() {
        return;
    }
    job_list(ui, s, st, &theme);
    let selected =
        st.agent.selected.filter(|id| s.agents.job(*id).is_some()).or_else(|| s.agents.jobs.last().map(|j| j.id));
    st.agent.selected = selected;
    let Some(id) = selected else { return };
    ui.separator();
    job_view(ui, s, st, &theme, id);
}

// ------------------------------------------------------------------------- launcher

fn new_job(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, theme: &Theme, backends: &[BackendInfo]) {
    let current = backends.iter().find(|b| b.backend == st.agent.backend).cloned();
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("agent-backend")
            .selected_text(st.agent.backend.label())
            .width(120.0)
            .show_ui(ui, |ui| {
                for info in backends {
                    let label = if info.installed() {
                        info.backend.label().to_string()
                    } else {
                        format!("{} (not installed)", info.backend.label())
                    };
                    let usable = info.installed() || info.acp.is_some();
                    let resp = ui
                        .add_enabled(usable, egui::Button::selectable(st.agent.backend == info.backend, label))
                        .on_disabled_hover_text(info.backend.install_hint());
                    if resp.clicked() {
                        st.agent.backend = info.backend;
                    }
                }
            })
            .response
            .on_hover_text("Agent");
        egui::ComboBox::from_id_salt("agent-mode")
            .selected_text(mode_label(st.agent.mode))
            .width(80.0)
            .show_ui(ui, |ui| {
                for m in MODES {
                    let support = current.as_ref().map_or(Err(String::new()), |i| mode_support(i, m));
                    let resp =
                        ui.add_enabled(support.is_ok(), egui::Button::selectable(st.agent.mode == m, mode_label(m)));
                    let resp = match &support {
                        Ok(()) => resp.on_hover_text(mode_tip(m)),
                        Err(why) => resp.on_disabled_hover_text(format!("{}\n\n{why}", mode_tip(m))),
                    };
                    if resp.clicked() {
                        st.agent.mode = m;
                    }
                }
            })
            .response
            .on_hover_text(mode_tip(st.agent.mode));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(egui::Button::new(RichText::new("⟳").color(theme.text_muted)).frame(false))
                .on_hover_text("Detect installed agents again")
                .clicked()
            {
                run_command(s, st, "agent.backends", &json!({ "refresh": true }));
            }
            if ui
                .add(egui::Button::new(RichText::new("⇄").color(theme.text_muted)).frame(false))
                .on_hover_text(
                    "Connect an external agent (Claude Code, Codex, Gemini, Cursor, Grok) to CodeCraft's tools",
                )
                .clicked()
            {
                open_connect(s, st);
            }
            // Terminal track: interactive Claude Code in a terminal tab, connected to CodeCraft.
            if ui
                .add(egui::Button::new(RichText::new(">_").color(theme.text_muted)).frame(false))
                .on_hover_text("Open Claude Code in a terminal")
                .clicked()
            {
                run_command(s, st, "terminal.openClaude", &Value::Null);
            }
        });
    });
    let support = current.as_ref().map_or(Err("unknown agent".to_string()), |i| mode_support(i, st.agent.mode));
    if let Err(why) = &support {
        ui.label(RichText::new(why).small().color(theme.warning));
    }

    let prompt_id = Id::new("agent-prompt");
    let mut send =
        ui.memory(|m| m.has_focus(prompt_id)) && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
    let accept_mention = mention_keys(ui, st, prompt_id, false);
    let height = s.ws.layout.pane_size("agent.composer", 82.0);
    let (pane, pane_height) = egui::Resize::default()
        .id_salt("agent-composer-size")
        .default_size([ui.available_width(), height])
        .min_size([80.0, 58.0])
        .max_size([ui.available_width().max(80.0), (ui.available_height() * 0.6).max(58.0)])
        .resizable([false, true])
        .show(ui, |ui| {
            let size = ui.available_size();
            let output = ScrollArea::vertical()
                .id_salt("agent-composer-scroll")
                .auto_shrink([false, false])
                .max_height(size.y)
                .show(ui, |ui| {
                    TextEdit::multiline(&mut st.agent.prompt)
                        .id(prompt_id)
                        .hint_text("Describe the task… (@ to attach, ⌘↩ to send)")
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .min_size(egui::vec2(0.0, (size.y - 8.0).max(1.0)))
                        .show(ui)
                })
                .inner;
            (output, size.y)
        });
    s.ws.layout.remember_pane("agent.composer", pane_height);
    let cursor = if pane.response.has_focus() || pane.response.lost_focus() { pane.cursor_range } else { None };
    attachment_mentions(ui, s, st, theme, cursor, accept_mention, false);
    context_chips(ui, s, st, theme);
    ui.horizontal(|ui| {
        add_context_button(ui, s, st);
        ui.checkbox(&mut st.agent.attach_file, "Current file").on_hover_text("Tell the agent which file is open");
        ui.checkbox(&mut st.agent.attach_selection, "Selection").on_hover_text("Include the selected text");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let ready = support.is_ok() && !st.agent.prompt.trim().is_empty();
            let button = egui::Button::new(RichText::new("Send").color(theme.text)).fill(theme.accent);
            if ui.add_enabled(ready, button).on_hover_text("Start the job (⌘↩)").clicked() {
                send = true;
            }
        });
    });
    if send && support.is_ok() && !st.agent.prompt.trim().is_empty() {
        let Some(attachments) = attachment_paths(&st.agent.context) else {
            s.status = "An attachment path is not UTF-8. Rename it or remove it before sending.".into();
            return;
        };
        let params = json!({
            "backend": st.agent.backend.id(),
            "mode": mode_id(st.agent.mode),
            "prompt": st.agent.prompt,
            "includeActiveFile": st.agent.attach_file,
            "includeSelection": st.agent.attach_selection,
            "attachments": attachments,
        });
        match s.execute("agent.start", &params) {
            Ok(v) => {
                st.agent.prompt.clear();
                st.agent.context.clear();
                st.agent.selected = v.get("job").and_then(Value::as_u64).map(JobId);
                st.agent.review_file = None;
            }
            Err(e) => s.status = e.to_string(),
        }
    }
}

/// JSON paths are strings; validate native picker paths before calling json!.
fn attachment_paths(paths: &[PathBuf]) -> Option<Vec<&str>> {
    paths.iter().map(|path| path.to_str()).collect()
}

/// Byte range of the @ token immediately before a char-based text caret.
/// Whitespace before @ prevents email addresses from opening the picker.
fn mention_range(text: &str, caret: usize) -> Option<std::ops::Range<usize>> {
    let end = text.char_indices().nth(caret).map_or(text.len(), |(i, _)| i);
    let before = text.get(..end)?;
    let start = before.rfind('@')?;
    if before.get(..start)?.chars().next_back().is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    let query = before.get(start.saturating_add(1)..)?;
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some(start..end)
}

fn mention_keys(ui: &egui::Ui, st: &mut UiState, prompt_id: Id, follow_up: bool) -> bool {
    let mut accept_mention = false;
    if ui.memory(|m| m.has_focus(prompt_id))
        && st.agent.mention_cache.as_ref().is_some_and(|cache| cache.follow_up == follow_up)
    {
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                st.agent.mention_index = st.agent.mention_index.saturating_add(1);
                st.agent.mention_scroll = true;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                st.agent.mention_index = st.agent.mention_index.saturating_sub(1);
                st.agent.mention_scroll = true;
            }
            accept_mention = i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab);
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                st.agent.mention_dismissed =
                    Some(if follow_up { st.agent.follow_up.clone() } else { st.agent.prompt.clone() });
            }
        });
    }
    accept_mention
}

fn attachment_mentions(
    ui: &mut egui::Ui,
    s: &mut Session,
    st: &mut UiState,
    theme: &Theme,
    cursor: Option<egui::text::CCursorRange>,
    accept: bool,
    follow_up: bool,
) {
    let text = if follow_up { st.agent.follow_up.clone() } else { st.agent.prompt.clone() };
    let Some(cursor) = cursor else { return };
    let range = cursor.is_empty().then(|| mention_range(&text, cursor.primary.index.into())).flatten();
    let Some(range) = range else {
        st.agent.mention_cache = None;
        st.agent.mention_dismissed = None;
        return;
    };
    if st.agent.mention_dismissed.as_ref() == Some(&text) {
        st.agent.mention_cache = None;
        return;
    }
    let query = text.get(range.start.saturating_add(1)..range.end).unwrap_or("").to_string();
    let root = s.project_root();
    let count = s.ws.project.as_ref().map_or(0, |p| p.tree.len());
    if !st.agent.mention_cache.as_ref().is_some_and(|cache| {
        cache.follow_up == follow_up
            && cache.query == query
            && cache.root == root
            && cache.count == count
            && cache.loaded.elapsed() < std::time::Duration::from_millis(500)
    }) {
        match s.execute("agent.context.search", &json!({"query": query})) {
            Ok(v) => {
                let changed = st
                    .agent
                    .mention_cache
                    .as_ref()
                    .is_none_or(|c| c.follow_up != follow_up || c.query != query || c.root != root);
                st.agent.mention_cache =
                    Some(MentionCache { follow_up, query, root, count, result: v, loaded: std::time::Instant::now() });
                if changed {
                    st.agent.mention_index = 0;
                }
            }
            Err(e) => {
                s.status = e.to_string();
                return;
            }
        }
    }
    let result = st.agent.mention_cache.as_ref().map(|cache| cache.result.clone()).unwrap_or_default();
    let prompt_id = Id::new(if follow_up { "agent-follow-up" } else { "agent-prompt" });
    let mut selected = None;
    Frame::new().stroke(egui::Stroke::new(1.0, theme.separator)).inner_margin(4).show(ui, |ui| {
        ui.label(RichText::new("Attach a file or folder").small().color(theme.text_muted));
        let items = result.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        if items.is_empty() {
            ui.label(if result.get("loading").and_then(Value::as_bool) == Some(true) {
                "Loading project files…"
            } else {
                "No matching files or folders"
            });
        }
        st.agent.mention_index = st.agent.mention_index.min(items.len().saturating_sub(1));
        ScrollArea::vertical().id_salt("agent-mentions").max_height(150.0).show(ui, |ui| {
            for (index, item) in items.into_iter().enumerate() {
                let label = item.get("label").and_then(Value::as_str).unwrap_or("");
                let directory = item.get("directory").and_then(Value::as_bool).unwrap_or(false);
                let highlighted = index == st.agent.mention_index;
                let response =
                    ui.selectable_label(highlighted, format!("{} {label}", if directory { "Folder" } else { "File" }));
                if highlighted && st.agent.mention_scroll {
                    response.scroll_to_me(Some(Align::Center));
                }
                if response.clicked() || (highlighted && accept) {
                    selected = item.get("path").and_then(Value::as_str).map(PathBuf::from);
                }
            }
        });
    });
    st.agent.mention_scroll = false;
    if let Some(path) = selected {
        match s.execute("agent.context.attach", &json!({"path": path})) {
            Ok(_) => {
                add_context(st, path);
                st.agent.mention_cache = None;
                let mut text = text;
                text.replace_range(range.clone(), "");
                let caret = text.get(..range.start).unwrap_or("").chars().count();
                if follow_up {
                    st.agent.follow_up = text;
                } else {
                    st.agent.prompt = text;
                }
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), prompt_id) {
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(caret))));
                    state.store(ui.ctx(), prompt_id);
                }
                ui.memory_mut(|m| m.request_focus(prompt_id));
            }
            Err(e) => s.status = e.to_string(),
        }
    }
}

/// `+`: add files or folders the agent should look at.
fn add_context_button(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let start = s.project_root();
    let dialog = || {
        let d = rfd::FileDialog::new();
        match &start {
            Some(root) => d.set_directory(root),
            None => d,
        }
    };
    ui.menu_button("+", |ui| {
        if ui.button("Add Files…").clicked() {
            ui.close();
            for p in dialog().set_title("Add Files to Agent Context").pick_files().unwrap_or_default() {
                add_context(st, p);
            }
        }
        if ui.button("Add Folder…").clicked() {
            ui.close();
            for p in dialog().set_title("Add Folders to Agent Context").pick_folders().unwrap_or_default() {
                add_context(st, p);
            }
        }
    })
    .response
    .on_hover_text("Add files or folders to the agent's context");
}

fn add_context(st: &mut UiState, path: PathBuf) {
    if !st.agent.context.contains(&path) {
        st.agent.context.push(path);
    }
}

/// The added paths as removable chips.
fn context_chips(ui: &mut egui::Ui, s: &Session, st: &mut UiState, theme: &Theme) {
    if st.agent.context.is_empty() {
        return;
    }
    let root = s.project_root();
    let roots: Vec<&Path> = root.iter().map(PathBuf::as_path).collect();
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (i, p) in st.agent.context.iter().enumerate() {
            let icon = if p.is_dir() { "📁" } else { "📄" };
            let name = rel_display(p, &roots);
            let resp = ui
                .add(egui::Button::new(RichText::new(format!("{icon} {name}  ×")).small().color(theme.text)))
                .on_hover_text(format!("{}\nClick to remove", p.display()));
            if resp.clicked() {
                remove = Some(i);
            }
        }
    });
    if let Some(i) = remove {
        st.agent.context.remove(i);
    }
}

// ------------------------------------------------------------------------- job list

fn job_list(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, theme: &Theme) {
    let rows: Vec<(JobId, String, JobStatus, String)> = s
        .agents
        .jobs
        .iter()
        .rev()
        .map(|j| (j.id, j.title.clone(), j.status, format!("{} · {}", j.backend.label(), mode_label(j.mode))))
        .collect();
    let height = (rows.len() as f32 * 22.0).min(110.0);
    ScrollArea::vertical().id_salt("agent-jobs").max_height(height).auto_shrink([false, true]).show(ui, |ui| {
        for (id, title, status, meta) in rows {
            let (mark, color) = status_mark(theme, status);
            let selected = st.agent.selected == Some(id);
            ui.horizontal(|ui| {
                ui.label(RichText::new(mark).color(color));
                let resp = ui
                    .add(egui::Button::selectable(selected, RichText::new(&title).color(theme.text)).truncate())
                    .on_hover_text(format!("Job {} · {meta} · {}", id.0, status.label()));
                if resp.clicked() {
                    st.agent.selected = Some(id);
                    st.agent.review_file = None;
                }
            });
        }
    });
}

// ------------------------------------------------------------------------- history

/// Earlier sessions of this project (saved across restarts) that are not open as jobs.
/// Status filters offered in the History header: (status label, menu text).
const STATUS_FILTERS: [(&str, &str); 8] = [
    ("", "Any status"),
    ("running", "Running"),
    ("finished", "Finished"),
    ("failed", "Failed"),
    ("interrupted", "Interrupted"),
    ("stopped", "Stopped"),
    ("reviewing", "Reviewing"),
    ("done", "Done"),
];

/// The History filter from the header's fields.
fn history_filter(st: &UiState) -> agent_sessions::Filter {
    let pick = |v: &str| (!v.is_empty()).then(|| v.to_string());
    agent_sessions::Filter {
        query: st.agent.history_query.trim().to_string(),
        status: pick(&st.agent.history_status),
        backend: pick(&st.agent.history_backend),
    }
}

/// The same filter as command params (export).
fn filter_params(st: &UiState) -> Value {
    json!({
        "query": st.agent.history_query.trim(),
        "status": st.agent.history_status,
        "backend": st.agent.history_backend,
    })
}

/// Search, status and backend filters, Export and Clean Up.
fn history_tools(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, cmd: &mut Option<(&'static str, Value)>) {
    ui.horizontal_wrapped(|ui| {
        ui.add(
            TextEdit::singleline(&mut st.agent.history_query)
                .id(Id::new("agent-history-search"))
                .hint_text("Search sessions")
                .desired_width(150.0),
        )
        .on_hover_text("Words in the title, the first prompt, the agent, the branch or the date (YYYY-MM-DD)");
        let status_text =
            STATUS_FILTERS.iter().find(|(v, _)| *v == st.agent.history_status).map_or("Any status", |(_, l)| *l);
        egui::ComboBox::from_id_salt("agent-history-status").selected_text(status_text).width(96.0).show_ui(ui, |ui| {
            for (v, label) in STATUS_FILTERS {
                ui.selectable_value(&mut st.agent.history_status, v.to_string(), label);
            }
        });
        let backend_text = Backend::from_id(&st.agent.history_backend).map_or("Any agent", |b| b.label());
        egui::ComboBox::from_id_salt("agent-history-backend").selected_text(backend_text).width(96.0).show_ui(
            ui,
            |ui| {
                ui.selectable_value(&mut st.agent.history_backend, String::new(), "Any agent");
                for b in Backend::ALL {
                    ui.selectable_value(&mut st.agent.history_backend, b.id().to_string(), b.label());
                }
            },
        );
        ui.menu_button("Export", |ui| {
            for (label, format) in [("As Markdown…", "markdown"), ("As JSON…", "json")] {
                if ui.button(label).on_hover_text("Export the sessions shown").clicked() {
                    let mut p = filter_params(st);
                    if let Some(o) = p.as_object_mut() {
                        o.insert("format".into(), json!(format));
                        o.insert("pick".into(), json!(true));
                    }
                    *cmd = Some(("agent.sessions.export", p));
                    ui.close();
                }
            }
        });
        if ui
            .button("Clean Up…")
            .on_hover_text(
                "Remove interrupted sessions and sessions older than 30 days, with the review worktrees CodeCraft \
                 created for them (asks first)",
            )
            .clicked()
        {
            match s.execute("agent.sessions.cleanup", &json!({})) {
                Ok(plan) => st.agent.cleanup = Some(plan),
                Err(e) => s.status = e.to_string(),
            }
        }
    });
}

/// Open Connect an External Agent (`agent.connectExternal`).
pub(crate) fn open_connect(s: &mut Session, st: &mut UiState) {
    match s.execute("agent.connectExternal", &Value::Null) {
        Ok(v) => st.agent.connect = Some(v),
        Err(e) => s.status = e.to_string(),
    }
}

/// Connect an External Agent: per client, the entry that reaches this IDE and the headless
/// server, to copy into the client's configuration (CodeCraft never edits those files).
pub(crate) fn connect_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    // `agent.connectExternal` from anywhere (Find Action, a menu, the ⇄ button) opens it.
    if let Some(v) = s.agents.connect_shown.take() {
        st.agent.connect = Some(v);
    }
    let Some(info) = st.agent.connect.clone() else { return };
    let theme = st.theme.clone();
    let clients = info.get("clients").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut open = true;
    let mut copied: Option<String> = None;
    egui::Window::new("Connect an External Agent")
        .id(Id::new("codecraft-agent-connect"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(620.0)
        .show(ui.ctx(), |ui| {
            ui.label(
                RichText::new(
                    "Give an agent CLI CodeCraft's tools (debugging, analysis, profiling, tests, databases). Copy \
                     the entry into the client's configuration; CodeCraft doesn't edit those files.",
                )
                .color(theme.text_muted),
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for c in &clients {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or_default();
                    let label = c.get("label").and_then(Value::as_str).unwrap_or(id);
                    if ui.add(egui::Button::selectable(st.agent.connect_client == id, label)).clicked() {
                        st.agent.connect_client = id.to_string();
                    }
                }
            });
            ui.separator();
            let current =
                clients.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(st.agent.connect_client.as_str()));
            let Some(current) = current.or(clients.first()) else { return };
            let mut snippet = |ui: &mut egui::Ui, title: &str, hint: &str, text: &str| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Copy").clicked() {
                            copied = Some(text.to_string());
                        }
                    });
                });
                ui.label(RichText::new(hint).small().color(theme.text_muted));
                Frame::new().fill(theme.editor_bg).corner_radius(CornerRadius::same(4)).inner_margin(Margin::same(6)).show(
                    ui,
                    |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(text).font(FontId::monospace(12.0)).color(theme.text))
                                .selectable(true)
                                .wrap(),
                        );
                    });
                ui.add_space(6.0);
            };
            if let Some(text) = current.get("ide").and_then(Value::as_str) {
                snippet(
                    ui,
                    "This IDE",
                    "Reaches the running CodeCraft window that has the agent's project open; Agent Access and its \
                     approval prompts apply.",
                    text,
                );
            }
            match current.get("headless").and_then(Value::as_str) {
                Some(text) => snippet(
                    ui,
                    "Headless (no IDE window)",
                    "codecraft-cli serves this project itself; only what --allow lists is allowed.",
                    text,
                ),
                None => {
                    ui.label(
                        RichText::new("Headless: not available here (see below).").small().color(theme.text_muted),
                    );
                }
            }
            for n in info.get("notes").and_then(Value::as_array).into_iter().flatten() {
                if let Some(n) = n.as_str() {
                    ui.label(RichText::new(format!("• {n}")).small().color(theme.text_muted));
                }
            }
        });
    if let Some(text) = copied {
        ui.ctx().copy_text(text);
        s.status = "Copied the configuration".into();
    }
    if !open {
        st.agent.connect = None;
    }
}

/// The Clean Up confirmation: what goes, then Delete / Cancel.
fn cleanup_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(plan) = st.agent.cleanup.clone() else { return };
    let theme = st.theme.clone();
    let list = |key: &str| plan.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let sessions = list("sessions");
    let worktrees = list("worktrees");
    let kept = list("keptWorktrees");
    let mut close = false;
    let mut confirm = false;
    let resp = egui::Modal::new(Id::new("codecraft-agent-cleanup")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        ui.label(RichText::new("Clean Up Agent Sessions").strong());
        ui.add_space(4.0);
        if sessions.is_empty() {
            ui.label("Nothing to clean up: no interrupted sessions and none older than 30 days.");
        } else {
            ui.label(format!(
                "{} session(s) and {} review worktree(s) will be removed:",
                sessions.len(),
                worktrees.len()
            ));
            ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for x in &sessions {
                    let title = x.get("title").and_then(Value::as_str).unwrap_or("");
                    let status = x.get("status").and_then(Value::as_str).unwrap_or("");
                    let title = if title.is_empty() { "(untitled)" } else { title };
                    ui.label(RichText::new(format!("• {title} ({status})")).color(theme.text));
                    if let Some(wt) = x.get("worktree").and_then(Value::as_str) {
                        ui.label(RichText::new(format!("    worktree {wt}")).small().color(theme.text_muted));
                    }
                }
            });
            if !kept.is_empty() {
                ui.label(
                    RichText::new(format!("{} worktree(s) CodeCraft did not create are left alone.", kept.len()))
                        .small()
                        .color(theme.warning),
                );
            }
            ui.checkbox(&mut st.agent.cleanup_worktrees_only, "Keep the sessions in History (remove only worktrees)");
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if !sessions.is_empty() && ui.button(RichText::new("Delete").color(theme.error)).clicked() {
                confirm = true;
            }
            if ui.button(if sessions.is_empty() { "Close" } else { "Cancel" }).clicked() {
                close = true;
            }
        });
    });
    if confirm {
        let only = st.agent.cleanup_worktrees_only;
        st.agent.cleanup = None;
        run_command(s, st, "agent.sessions.cleanup", &json!({ "confirm": true, "worktreesOnly": only }));
    } else if close || resp.should_close() {
        st.agent.cleanup = None;
    }
}

fn history(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, theme: &Theme) {
    cleanup_dialog(ui.ctx(), s, st);
    let filter = history_filter(st);
    let total = agent_sessions::history(s).len();
    let rows: Vec<SessionSummary> = agent_sessions::history_filtered(s, &filter).into_iter().cloned().collect();
    let loading = s.agents.sessions.loading;
    let warnings = s.agents.sessions.warnings.clone();
    let mut cmd: Option<(&'static str, Value)> = None;
    let mut select_session: Option<String> = None;
    ui.horizontal(|ui| {
        if ui.small_button("⟳").on_hover_text("Reload saved conversations").clicked() {
            cmd = Some(("agent.sessions.reload", Value::Null));
        }
        let arrow = if st.agent.history_open { "▼" } else { "▶" };
        let count = if filter.is_empty() { rows.len().to_string() } else { format!("{} of {total}", rows.len()) };
        let header = RichText::new(format!("{arrow} History · {count}")).small().strong().color(theme.text_muted);
        if ui
            .add(egui::Label::new(header).sense(egui::Sense::click()))
            .on_hover_text("Earlier agent sessions of this project")
            .clicked()
        {
            st.agent.history_open = !st.agent.history_open;
        }
        if loading {
            ui.add(egui::Spinner::new().size(10.0));
        }
        if !warnings.is_empty() {
            ui.label(RichText::new(format!("⚠ {}", warnings.len())).small().color(theme.warning))
                .on_hover_text(warnings.join("\n"));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if rows.is_empty() || !st.agent.history_open {
                return;
            }
            let label = if st.agent.confirm_clear { "Really delete all?" } else { "Delete All" };
            let color = if st.agent.confirm_clear { theme.error } else { theme.text_muted };
            let resp = ui
                .add(egui::Button::new(RichText::new(label).small().color(color)).frame(false))
                .on_hover_text("Delete every saved session of this project (running jobs are kept)");
            if resp.clicked() {
                if st.agent.confirm_clear {
                    cmd = Some(("agent.sessions.clear", Value::Null));
                    st.agent.confirm_clear = false;
                } else {
                    st.agent.confirm_clear = true;
                }
            }
            if resp.hovered() {
                return;
            }
            if st.agent.confirm_clear && ui.input(|i| i.pointer.any_click()) {
                st.agent.confirm_clear = false;
            }
        });
    });
    if !st.agent.history_open {
        if let Some((c, p)) = cmd {
            run_command(s, st, c, &p);
        }
        return;
    }
    if total == 0 && !loading && s.agents.jobs.is_empty() {
        ui.label(
            RichText::new("No saved conversations yet. Chats appear here after you start work.")
                .small()
                .color(theme.text_muted),
        );
    }
    if total > 0 {
        history_tools(ui, s, st, &mut cmd);
    }
    if rows.is_empty() && total > 0 {
        ui.label(RichText::new("No session matches.").small().color(theme.text_muted));
    }
    let now = agent_sessions::now_ms();
    ScrollArea::vertical().id_salt("agent-history").max_height(140.0).auto_shrink([false, true]).show(ui, |ui| {
        for row in &rows {
            ui.horizontal(|ui| {
                let (mark, color) = status_mark(theme, row.status);
                ui.label(RichText::new(mark).color(color)).on_hover_text(row.status.label());
                let renaming = st.agent.renaming.as_ref().is_some_and(|(k, _)| *k == row.id);
                if renaming {
                    rename_field(ui, st, &row.id, &mut cmd);
                    return;
                }
                let mut meta = format!(
                    "{} · {} · {}",
                    row.backend.label(),
                    mode_label(row.mode),
                    agent_sessions::age_label(now, row.created_ms)
                );
                if row.elsewhere {
                    meta.push_str(" · in another window");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(&meta).small().color(theme.text_muted));
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let title = if row.title.is_empty() { "(untitled)" } else { row.title.as_str() };
                        let mut tip = format!("{meta} · {}", row.status.label());
                        if let Some(c) = row.cost_usd {
                            tip.push_str(&format!(" · ${c:.2}"));
                        }
                        if row.worktree.is_some() && !row.worktree_exists {
                            tip.push_str("\nIts review worktree no longer exists.");
                        }
                        if row.elsewhere {
                            tip.push_str("\nRunning in another CodeCraft window; follow it there.");
                        } else {
                            tip.push_str("\nClick to open; right-click for more.");
                        }
                        let resp = ui
                            .add(egui::Button::selectable(false, RichText::new(title).color(theme.text)).truncate())
                            .on_hover_text(tip);
                        if resp.clicked() {
                            if row.elsewhere {
                                s.status = "This agent session is running in another CodeCraft window".into();
                            } else {
                                select_session = Some(row.id.clone());
                            }
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                select_session = Some(row.id.clone());
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                st.agent.renaming = Some((row.id.clone(), row.title.clone()));
                                ui.close();
                            }
                            if ui.button("Show Session File").clicked() {
                                cmd = Some(("agent.sessions.reveal", json!({ "session": row.id })));
                                ui.close();
                            }
                            if ui.button("Open Session File in Editor").clicked() {
                                cmd = Some(("agent.sessions.reveal", json!({ "session": row.id, "inEditor": true })));
                                ui.close();
                            }
                            for (label, format) in [("Export as Markdown…", "markdown"), ("Export as JSON…", "json")]
                            {
                                if ui.button(label).clicked() {
                                    cmd = Some((
                                        "agent.sessions.export",
                                        json!({ "session": row.id, "format": format, "pick": true }),
                                    ));
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.button(RichText::new("Delete").color(theme.error)).clicked() {
                                cmd = Some(("agent.sessions.delete", json!({ "session": row.id })));
                                ui.close();
                            }
                        });
                    });
                });
            });
        }
    });
    if let Some(key) = select_session {
        match s.execute("agent.sessions.open", &json!({ "session": key })) {
            Ok(v) => {
                st.agent.selected = v.get("job").and_then(Value::as_u64).map(JobId);
                st.agent.review_file = None;
            }
            Err(e) => s.status = e.to_string(),
        }
    }
    if let Some((c, p)) = cmd {
        run_command(s, st, c, &p);
    }
}

/// The inline title editor of a session (Enter renames, Esc cancels).
fn rename_field(ui: &mut egui::Ui, st: &mut UiState, key: &str, cmd: &mut Option<(&'static str, Value)>) {
    let Some((_, text)) = st.agent.renaming.as_mut() else { return };
    let id = Id::new(("agent-rename", key));
    let resp = ui.add(TextEdit::singleline(text).id(id).desired_width(ui.available_width() - 8.0));
    if !resp.has_focus() && !resp.lost_focus() {
        resp.request_focus();
    }
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        st.agent.renaming = None;
    } else if resp.lost_focus() {
        if ui.input(|i| i.key_pressed(Key::Enter)) && !text.trim().is_empty() {
            *cmd = Some(("agent.sessions.rename", json!({ "session": key, "title": text.trim() })));
        }
        st.agent.renaming = None;
    }
}

// ------------------------------------------------------------------------- one job

fn job_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, theme: &Theme, id: JobId) {
    let Some(job) = s.agents.job(id) else { return };
    let (mark, color) = status_mark(theme, job.status);
    let running = job.is_running();
    let mut meta =
        vec![job.backend.label().to_string(), mode_label(job.mode).to_string(), job.status.label().to_string()];
    if let Some(c) = job.cost_usd {
        meta.push(format!("${c:.2}"));
    }
    if running {
        meta.push(seconds(u64::try_from(job.started.elapsed().as_millis()).unwrap_or(u64::MAX)));
    } else if let Some(d) = job.duration_ms {
        meta.push(seconds(d));
    }
    let branch = job.worktree.as_ref().map(|w| w.branch.clone());
    let title = job.title.clone();
    let session_key = job.session_key.clone();
    let error = job.error.clone().filter(|_| job.status == JobStatus::Failed);
    let mut action: Option<&str> = None;
    let mut rename_cmd: Option<(&'static str, Value)> = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(mark).color(color));
        let renaming = session_key.as_ref().filter(|k| st.agent.renaming.as_ref().is_some_and(|(r, _)| r == *k));
        if let Some(key) = renaming {
            rename_field(ui, st, key, &mut rename_cmd);
            return;
        }
        ui.label(RichText::new(&title).strong().color(theme.text));
        if let Some(key) = &session_key
            && ui
                .add(egui::Button::new(RichText::new("Rename").small().color(theme.text_muted)).frame(false))
                .on_hover_text("Rename this session")
                .clicked()
        {
            st.agent.renaming = Some((key.clone(), title.clone()));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .small_button("Close")
                .on_hover_text("Stop the job, remove its worktree and close it (the session stays in History)")
                .clicked()
            {
                action = Some("agent.discard");
            }
            if running && ui.small_button("■ Stop").on_hover_text("Stop the agent").clicked() {
                action = Some("agent.stop");
            }
        });
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(meta.join(" · ")).small().color(theme.text_muted));
        if let Some(b) = &branch {
            ui.label(RichText::new(format!("branch {b}")).small().color(theme.text_muted));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.checkbox(&mut st.agent.show_logs, RichText::new("Logs").small());
        });
    });
    if let Some(e) = error {
        ui.label(RichText::new(e).small().color(theme.error));
    }
    if let Some((c, p)) = rename_cmd {
        run_command(s, st, c, &p);
    }
    if let Some(cmd) = action {
        run_command(s, st, cmd, &json!({ "job": id.0 }));
        if cmd == "agent.discard" {
            st.agent.selected = None;
            return;
        }
    }

    let Some(job) = s.agents.job(id) else { return };
    let reviewing = job.mode == AgentMode::Review && (!job.review.is_empty() || job.status == JobStatus::Reviewing);
    if reviewing {
        review(ui, s, st, theme, id);
        ui.separator();
    }
    let follow_h = 34.0;
    let feed_h = (ui.available_height() - follow_h).max(80.0);
    feed(ui, s, st, theme, id, feed_h);
    let Some(job) = s.agents.job(id) else { return };
    let (can, running) = (job.can_follow_up(), job.is_running());
    follow_up_bar(ui, s, st, theme, id, can, running);
}

fn follow_up_bar(
    ui: &mut egui::Ui,
    s: &mut Session,
    st: &mut UiState,
    theme: &Theme,
    id: JobId,
    can: Result<(), String>,
    running: bool,
) {
    // Without a live agent the follow-up resumes the saved session.
    let resume = s.agents.job(id).is_some_and(|j| !j.has_live_session());
    ui.add_space(2.0);
    if running {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("The agent is working…").color(theme.text_muted));
        });
        return;
    }
    if let Err(why) = can {
        ui.label(RichText::new(why).small().color(theme.text_muted));
        return;
    }
    let mut send = false;
    let prompt_id = Id::new("agent-follow-up");
    let accept = mention_keys(ui, st, prompt_id, true);
    let mut cursor = None;
    ui.horizontal(|ui| {
        let w = ui.available_width() - if resume { 74.0 } else { 60.0 };
        let hint = if resume { "Resume this session…" } else { "Follow up…" };
        let output = TextEdit::singleline(&mut st.agent.follow_up)
            .id(prompt_id)
            .hint_text(hint)
            .desired_width(w.max(80.0))
            .show(ui);
        let resp = output.response;
        if resp.has_focus() || resp.lost_focus() {
            cursor = output.cursor_range;
        }
        if !accept && resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            send = true;
        }
        let (label, tip) = if resume {
            ("Resume", "Continue the agent's session with this prompt")
        } else {
            ("Send", "Send a follow-up in this session")
        };
        if ui.add_enabled(!st.agent.follow_up.trim().is_empty(), egui::Button::new(label)).on_hover_text(tip).clicked()
        {
            send = true;
        }
    });
    attachment_mentions(ui, s, st, theme, cursor, accept, true);
    if send && !st.agent.follow_up.trim().is_empty() {
        let Some(attachments) = attachment_paths(&st.agent.context) else {
            s.status = "An attachment path is not UTF-8. Rename it or remove it before sending.".into();
            return;
        };
        match s.execute(
            "agent.followUp",
            &json!({ "job": id.0, "prompt": st.agent.follow_up, "attachments": attachments }),
        ) {
            Ok(_) => {
                st.agent.follow_up.clear();
                st.agent.context.clear();
                st.agent.review_file = None;
            }
            Err(e) => s.status = e.to_string(),
        }
    }
}

// ------------------------------------------------------------------------- review

fn change_badge(theme: &Theme, c: ChangeKind) -> (&'static str, egui::Color32) {
    match c {
        ChangeKind::Added => ("A", theme.diff_added),
        ChangeKind::Modified => ("M", theme.link),
        ChangeKind::Deleted => ("D", theme.diff_removed),
    }
}

fn review(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, theme: &Theme, id: JobId) {
    if codecraft_engine::selfupdate::is_codecraft(s, id.0)
        && st.selfupdate_preflight_job != Some(id.0)
        && !s.selfupdate.busy
    {
        st.selfupdate_preflight_job = Some(id.0);
        if let Some(source) = s.agents.job(id).map(|j| j.project.clone()) {
            run_command(s, st, "selfupdate.preflight", &json!({"source":source}));
        }
    }
    let Some(job) = s.agents.job(id) else { return };
    let files: Vec<ReviewFile> = job.review.clone();
    let pending = job.pending_review();
    let preparing = job.status == JobStatus::Reviewing && files.is_empty();
    if preparing {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("Collecting the changes…").color(theme.text_muted));
        });
        return;
    }
    let mut cmd: Option<(&str, Value)> = None;
    if codecraft_engine::selfupdate::is_codecraft(s, id.0)
        && let Some(report) = &s.selfupdate.report
        && !report.ready()
    {
        ui.label("Build requirements need attention");
        for check in report.checks.iter().filter(|c| c.level == codecraft_selfupdate::preflight::Level::Fail) {
            ui.label(&check.message);
            ui.small(&check.fix);
        }
        if report.checks.iter().any(|c| {
            matches!(c.name.as_str(), "cargo" | "rustc") && c.level == codecraft_selfupdate::preflight::Level::Fail
        }) && ui.small_button("Open rustup.rs").clicked()
        {
            cmd = Some(("selfupdate.openRustup", Value::Null));
        }
        if ui.add_enabled(!s.selfupdate.busy, egui::Button::new("Check requirements again")).clicked() {
            cmd = Some(("selfupdate.preflight", json!({"source":job.project})));
        }
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Changes · {pending}/{}", files.len())).strong())
            .on_hover_text(format!("{pending} of {} files still to review", files.len()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if pending > 0 {
                if ui.small_button("Reject All").clicked() {
                    cmd = Some(("agent.review.rejectAll", json!({ "job": id.0 })));
                }
                let own_source = codecraft_engine::selfupdate::is_codecraft(s, id.0);
                let label = if own_source { "Accept only" } else { "Accept All" };
                if ui.small_button(label).clicked() {
                    cmd = Some(("agent.review.acceptAll", json!({ "job": id.0 })));
                }
                if own_source {
                    let reason = s.selfupdate.report.as_ref().filter(|r| !r.ready()).map(|r| {
                        r.checks
                            .iter()
                            .filter(|c| c.level == codecraft_selfupdate::preflight::Level::Fail)
                            .map(|c| c.message.clone())
                            .collect::<Vec<_>>()
                            .join("\n")
                    });
                    if ui
                        .add_enabled(
                            !s.selfupdate.busy && reason.is_none(),
                            egui::Button::new("Accept, Build & Restart"),
                        )
                        .on_disabled_hover_text(reason.as_deref().unwrap_or("Self-update is busy"))
                        .clicked()
                    {
                        cmd = Some(("feedback.acceptAndBuild", json!({"job":id.0})));
                    }
                }
            }
            ui.toggle_value(&mut st.agent.side_by_side, RichText::new("Split").small())
                .on_hover_text("Side-by-side diff");
        });
    });
    let shown = st.agent.review_file.clone().filter(|r| files.iter().any(|f| &f.rel == r)).or_else(|| {
        let pending = |f: &&ReviewFile| f.decision == ReviewDecision::Pending;
        files
            .iter()
            .find(|f| pending(f) && f.change != ChangeKind::Deleted)
            .or_else(|| files.iter().find(pending))
            .or(files.first())
            .map(|f| f.rel.clone())
    });
    ScrollArea::vertical().id_salt(("agent-review-files", id.0)).max_height(130.0).auto_shrink([false, true]).show(
        ui,
        |ui| {
            for f in &files {
                ui.horizontal(|ui| {
                    let (badge, color) = change_badge(theme, f.change);
                    ui.label(RichText::new(badge).monospace().strong().color(color));
                    let selected = shown.as_deref() == Some(f.rel.as_str());
                    let name = RichText::new(&f.rel).color(match f.decision {
                        ReviewDecision::Pending => theme.text,
                        ReviewDecision::Accepted => theme.success,
                        ReviewDecision::Rejected => theme.text_disabled,
                    });
                    if ui.add(egui::Button::selectable(selected, name).truncate()).clicked() {
                        st.agent.review_file = Some(f.rel.clone());
                    }
                    if f.conflict {
                        ui.label(RichText::new("⚠").color(theme.warning)).on_hover_text(
                            "The project file changed since the agent started; accepting overwrites it.",
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match f.decision {
                        ReviewDecision::Pending => {
                            if ui.small_button("✗").on_hover_text("Reject").clicked() {
                                cmd = Some(("agent.review.reject", json!({ "job": id.0, "path": f.rel })));
                            }
                            if ui.small_button("✓").on_hover_text("Accept").clicked() {
                                cmd = Some(("agent.review.accept", json!({ "job": id.0, "path": f.rel })));
                            }
                            if !f.binary {
                                let stats = diff_view::stats(
                                    ui,
                                    ("review", id.0, f.rel.as_str()),
                                    f.original.as_deref().unwrap_or(""),
                                    f.proposed.as_deref().unwrap_or(""),
                                );
                                ui.label(
                                    RichText::new(format!("−{}", stats.removed)).small().color(theme.diff_removed),
                                );
                                ui.label(RichText::new(format!("+{}", stats.added)).small().color(theme.diff_added));
                            }
                        }
                        ReviewDecision::Accepted => {
                            ui.label(RichText::new("accepted").small().color(theme.success));
                        }
                        ReviewDecision::Rejected => {
                            ui.label(RichText::new("rejected").small().color(theme.text_muted));
                        }
                    });
                });
            }
        },
    );
    if let Some(f) = shown.and_then(|r| files.iter().find(|f| f.rel == r)) {
        if f.binary {
            ui.label(
                RichText::new("Binary or large file: accepting copies the agent's version.").color(theme.text_muted),
            );
        } else {
            let font = FontId::monospace((s.ws.settings.editor_font_size - 2.0).max(9.0));
            let opts = DiffOptions {
                side_by_side: st.agent.side_by_side,
                context: 3,
                max_height: (ui.available_height() * 0.45).clamp(120.0, 600.0),
            };
            diff_view::show(
                ui,
                theme,
                &font,
                ("review", id.0, f.rel.as_str()),
                f.original.as_deref().unwrap_or(""),
                f.proposed.as_deref().unwrap_or(""),
                &opts,
            );
        }
    }
    if let Some((c, p)) = cmd {
        run_command(s, st, c, &p);
    }
}

// ------------------------------------------------------------------------- activity feed

fn feed(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, theme: &Theme, id: JobId, height: f32) {
    let Some(job) = s.agents.job(id) else { return };
    let roots: Vec<PathBuf> = std::iter::once(job.cwd.clone()).chain(std::iter::once(job.project.clone())).collect();
    let activity: Vec<Activity> = job.activity.iter().cloned().collect();
    let show_logs = st.agent.show_logs;
    let mono_size = (s.ws.settings.editor_font_size - 1.0).max(9.0);
    let mut open: Option<PathBuf> = None;
    Frame::new().fill(theme.editor_bg).corner_radius(4).inner_margin(Margin::same(6)).show(ui, |ui| {
        ScrollArea::vertical()
            .id_salt(("agent-feed", id.0))
            .max_height(height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let roots: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
                for a in &activity {
                    activity_row(ui, theme, a, &roots, show_logs, mono_size, &mut open);
                }
            });
    });
    if let Some(path) = open {
        run_command(s, st, "file.open", &json!({ "path": path }));
    }
}

fn activity_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    a: &Activity,
    roots: &[&Path],
    show_logs: bool,
    mono_size: f32,
    open: &mut Option<PathBuf>,
) {
    match a {
        Activity::Prompt { text } => {
            ui.add_space(4.0);
            Frame::new()
                .fill(theme.list_selection_inactive)
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let short = codecraft_engine::agent::truncate(text, 600);
                    ui.label(RichText::new(short).color(theme.text)).on_hover_text(text.as_str());
                });
            ui.add_space(2.0);
        }
        Activity::Text { text } => {
            crate::markdown::render(ui, text, theme, mono_size);
        }
        Activity::Tool { summary, state, output, .. } => {
            ui.horizontal(|ui| {
                match state {
                    ToolState::Running => {
                        ui.add(egui::Spinner::new().size(10.0));
                    }
                    ToolState::Ok => {
                        ui.label(RichText::new("✓").small().color(theme.success));
                    }
                    ToolState::Failed => {
                        ui.label(RichText::new("✗").small().color(theme.error));
                    }
                }
                let resp = ui.add(egui::Label::new(RichText::new(summary).small().color(theme.text_editor)).truncate());
                if !output.trim().is_empty() {
                    let out = codecraft_engine::agent::truncate(output.trim(), 1500);
                    resp.on_hover_text(RichText::new(out).monospace());
                }
            });
        }
        Activity::FileChanged { path, change } => {
            ui.horizontal(|ui| {
                let (badge, color) = change_badge(theme, *change);
                ui.label(RichText::new(badge).small().monospace().color(color));
                let label = RichText::new(rel_display(path, roots)).small().color(theme.link);
                let resp = ui.add(egui::Label::new(label).sense(egui::Sense::click()).truncate());
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && *change != ChangeKind::Deleted {
                    *open = Some(path.clone());
                }
            });
        }
        Activity::Plan { entries } => {
            Frame::new().stroke(egui::Stroke::new(1.0, theme.separator)).corner_radius(4).inner_margin(4).show(
                ui,
                |ui| {
                    ui.set_width(ui.available_width());
                    for e in entries {
                        let (mark, color) = match e.status.as_str() {
                            "completed" => ("✓", theme.success),
                            "in_progress" => ("▶", theme.accent),
                            _ => ("·", theme.text_muted),
                        };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(mark).small().color(color));
                            ui.label(RichText::new(&e.content).small().color(theme.text_editor));
                        });
                    }
                },
            );
        }
        Activity::Result { is_error, cost_usd, duration_ms, text } => {
            let mut parts = vec![if *is_error { "Ended with an error".to_string() } else { "Done".to_string() }];
            if let Some(d) = duration_ms {
                parts.push(seconds(*d));
            }
            if let Some(c) = cost_usd {
                parts.push(format!("${c:.2}"));
            }
            let color = if *is_error { theme.error } else { theme.success };
            ui.label(
                RichText::new(format!("{} {}", if *is_error { "✗" } else { "✓" }, parts.join(" · ")))
                    .small()
                    .color(color),
            );
            if *is_error && let Some(t) = text {
                ui.label(RichText::new(codecraft_engine::agent::truncate(t, 400)).small().color(theme.error));
            }
        }
        Activity::Error { message } => {
            ui.label(RichText::new(format!("⚠ {message}")).small().color(theme.error));
        }
        Activity::Log { line } => {
            if show_logs {
                ui.label(RichText::new(line).monospace().small().color(theme.console_system));
            }
        }
        Activity::Info { text } => {
            ui.label(RichText::new(text).small().italics().color(theme.text_muted));
        }
    }
}

#[cfg(test)]
mod mention_tests {
    use super::mention_range;
    #[cfg(unix)]
    use super::attachment_paths;

    #[cfg(unix)]
    #[test]
    fn native_attachment_paths_are_checked_before_json_serialization() {
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;
        let valid = vec![PathBuf::from("src/main.rs")];
        assert_eq!(attachment_paths(&valid), Some(vec!["src/main.rs"]));
        let invalid = vec![PathBuf::from(std::ffi::OsString::from_vec(b"bad-\xff.rs".to_vec()))];
        assert!(attachment_paths(&invalid).is_none());
    }

    #[test]
    fn mentions_follow_the_unicode_caret_and_ignore_emails() {
        assert_eq!(mention_range("fix @src/main.rs later", 16), Some(4..16));
        assert_eq!(mention_range("é @文", 4), Some(3..7));
        assert_eq!(mention_range("@", 1), Some(0..1));
        assert_eq!(mention_range("mail a@b.com", 12), None);
        assert_eq!(mention_range("@src done", 9), None);
        assert_eq!(mention_range("plain", 5), None);
    }
}
