//! Tools › Tasks & Contexts: local tasks, each with its own context (open editors, carets,
//! breakpoints), named contexts, and time tracking. Kept per project in
//! `<project>/.codecraft/tasks.json` (in memory only without a project).
//!
//! Switching tasks saves the current context into the active task and restores the target's.
//! Opening a task can start a Git branch for it (the New Branch dialog, prefilled). There are no
//! issue-tracker servers: tasks are local, so Configure Servers explains that.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_text::SelectionSet;

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{bool_or, opt_str, opt_usize, req_str};
use crate::user_store::now_ms;
use crate::{CommandError, Effect, Popup, Session};

pub const DEFAULT_TASK: &str = "default";
const FILE: &str = "tasks.json";
const MAX_TASKS: usize = 1000;
const MAX_CONTEXTS: usize = 200;
const MAX_EDITORS: usize = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorEntry {
    pub path: PathBuf,
    pub offset: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BreakpointEntry {
    pub path: PathBuf,
    pub line: usize,
    pub enabled: bool,
    pub condition: Option<String>,
}

/// What a context restores.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkContext {
    pub editors: Vec<EditorEntry>,
    pub active: Option<PathBuf>,
    pub breakpoints: Vec<BreakpointEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub summary: String,
    pub description: String,
    pub url: Option<String>,
    pub created_ms: u64,
    pub closed: bool,
    pub branch: Option<String>,
    /// Tracked time (ms).
    pub time_spent_ms: u64,
    pub context: WorkContext,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamedContext {
    pub name: String,
    pub saved_ms: u64,
    pub context: WorkContext,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksFile {
    pub version: u32,
    pub tasks: Vec<Task>,
    pub active: String,
    pub contexts: Vec<NamedContext>,
}

impl Default for TasksFile {
    fn default() -> Self {
        Self {
            version: 1,
            tasks: vec![Task { id: DEFAULT_TASK.into(), summary: "Default task".into(), ..Task::default() }],
            active: DEFAULT_TASK.into(),
            contexts: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskDialogKind {
    Open,
    Edit,
    SaveContext,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDialog {
    pub kind: TaskDialogKind,
    /// Task summary, or the context name.
    pub summary: String,
    pub description: String,
    pub url: String,
    pub create_branch: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChooserKind {
    SwitchTask,
    LoadContext,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksChooser {
    pub kind: ChooserKind,
    pub query: String,
}

#[derive(Debug, Default)]
pub struct TasksState {
    pub file: TasksFile,
    loaded_for: Option<Option<PathBuf>>,
    /// When the timer of the active task started (ms since the epoch).
    pub timer_started_ms: Option<u64>,
    pub dialog: Option<TaskDialog>,
    pub chooser: Option<TasksChooser>,
}

impl TasksState {
    pub fn active(&self) -> Option<&Task> {
        self.file.tasks.iter().find(|t| t.id == self.file.active)
    }

    /// Tracked time of the active task including the running timer (ms).
    pub fn active_time_ms(&self) -> u64 {
        let base = self.active().map(|t| t.time_spent_ms).unwrap_or(0);
        base.saturating_add(self.timer_started_ms.map(|s| now_ms().saturating_sub(s)).unwrap_or(0))
    }
}

/// `1h 05m`, `3m 12s`.
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 { format!("{h}h {m:02}m") } else { format!("{m}m {s:02}s") }
}

fn file_path(s: &Session) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.join(".codecraft").join(FILE))
}

/// Load the project's tasks once per project.
pub(crate) fn ensure_loaded(s: &mut Session) {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if s.tasks.loaded_for.as_ref() == Some(&root) {
        return;
    }
    s.tasks.loaded_for = Some(root);
    s.tasks.timer_started_ms = None;
    let loaded = file_path(s).and_then(|p| match crate::user_store::load_json::<TasksFile>(&p) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("{e}");
            None
        }
    });
    let mut file = loaded.unwrap_or_default();
    if !file.tasks.iter().any(|t| t.id == DEFAULT_TASK) {
        file.tasks.insert(0, Task { id: DEFAULT_TASK.into(), summary: "Default task".into(), ..Task::default() });
    }
    if !file.tasks.iter().any(|t| t.id == file.active) {
        file.active = DEFAULT_TASK.into();
    }
    file.tasks.truncate(MAX_TASKS);
    file.contexts.truncate(MAX_CONTEXTS);
    s.tasks.file = file;
}

/// Load the tasks of a newly opened project (so the menu's enabled states are right).
pub(crate) fn pump(s: &mut Session) -> bool {
    let root = s.ws.project.as_ref().map(|p| p.root.as_path());
    if s.tasks.loaded_for.as_ref().map(|o| o.as_deref()) == Some(root) {
        return false;
    }
    ensure_loaded(s);
    true
}

fn persist(s: &mut Session) -> Result<(), CommandError> {
    let Some(path) = file_path(s) else { return Ok(()) };
    crate::user_store::save_json(&path, &s.tasks.file, false).map_err(CommandError::Failed)
}

/// The current editors, carets and breakpoints.
pub fn capture(s: &Session) -> WorkContext {
    let mut editors = Vec::new();
    for id in s.ws.all_tabs() {
        let Some(e) = s.ws.editor(id) else { continue };
        let Some(path) = s.ws.doc(e.doc).and_then(|d| d.path.clone()) else { continue };
        if !editors.iter().any(|x: &EditorEntry| x.path == path) && editors.len() < MAX_EDITORS {
            editors.push(EditorEntry { path, offset: e.selections.primary().head });
        }
    }
    let breakpoints = s
        .debug
        .breakpoints
        .iter()
        .map(|b| BreakpointEntry { path: b.path.clone(), line: b.line, enabled: b.enabled, condition: b.condition.clone() })
        .collect();
    WorkContext { editors, active: s.ws.active().and_then(|(_, d)| d.path.clone()), breakpoints }
}

fn clear(s: &mut Session) {
    for id in s.ws.all_tabs() {
        crate::window_cmds::close(s, id);
    }
    if !s.debug.breakpoints.is_empty() {
        crate::debug::remove_all_breakpoints(s);
    }
}

/// Close everything and open the context's editors and breakpoints (missing files are skipped).
pub(crate) fn restore(s: &mut Session, ctx: &WorkContext) -> usize {
    clear(s);
    let mut opened = 0usize;
    for e in &ctx.editors {
        if !e.path.is_file() {
            continue;
        }
        if let Ok(id) = s.open_file(&e.path) {
            let len = s.ws.editor(id).and_then(|ed| s.ws.doc(ed.doc)).map(|d| d.doc.len_chars()).unwrap_or(0);
            s.ws.set_selections(id, SelectionSet::caret(e.offset.min(len)));
            opened += 1;
        }
    }
    if let Some(active) = &ctx.active
        && active.is_file()
    {
        let _ = s.open_file(active);
    }
    for b in &ctx.breakpoints {
        if b.path.is_file() {
            crate::debug::set_breakpoint(
                s,
                &b.path,
                b.line,
                crate::debug::BreakpointProps {
                    enabled: Some(b.enabled),
                    condition: Some(b.condition.clone()),
                    ..Default::default()
                },
            );
        }
    }
    opened
}

fn stop_timer(s: &mut Session) {
    if let Some(start) = s.tasks.timer_started_ms.take() {
        let spent = now_ms().saturating_sub(start);
        let active = s.tasks.file.active.clone();
        if let Some(t) = s.tasks.file.tasks.iter_mut().find(|t| t.id == active) {
            t.time_spent_ms = t.time_spent_ms.saturating_add(spent);
        }
    }
}

fn find_task(s: &Session, key: &str) -> Option<String> {
    let k = key.trim();
    s.tasks
        .file
        .tasks
        .iter()
        .find(|t| t.id == k)
        .or_else(|| s.tasks.file.tasks.iter().find(|t| t.summary.eq_ignore_ascii_case(k)))
        .map(|t| t.id.clone())
}

/// Save the current context into the active task and switch to `id`.
fn switch_to(s: &mut Session, id: &str) -> CmdResult {
    let timing = s.tasks.timer_started_ms.is_some();
    stop_timer(s);
    let ctx = capture(s);
    let current = s.tasks.file.active.clone();
    if let Some(t) = s.tasks.file.tasks.iter_mut().find(|t| t.id == current) {
        t.context = ctx;
    }
    let target = s.tasks.file.tasks.iter_mut().find(|t| t.id == id).ok_or_else(|| CommandError::BadParams(format!("no task '{id}'")))?;
    target.closed = false;
    let ctx = target.context.clone();
    let summary = target.summary.clone();
    s.tasks.file.active = id.to_string();
    let opened = restore(s, &ctx);
    if timing {
        s.tasks.timer_started_ms = Some(now_ms());
    }
    s.tasks.chooser = None;
    persist(s)?;
    s.status = format!("Switched to task '{summary}'");
    Ok(json!({ "task": id, "editors": opened }))
}

fn slug(summary: &str) -> String {
    let mut out = String::new();
    for c in summary.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 50 {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn open_task(s: &mut Session, p: &Value) -> CmdResult {
    let Some(summary) = opt_str(p, "summary").map(str::trim).filter(|x| !x.is_empty()).map(str::to_string) else {
        s.tasks.dialog = Some(TaskDialog {
            kind: TaskDialogKind::Open,
            summary: String::new(),
            description: String::new(),
            url: String::new(),
            create_branch: false,
            error: None,
        });
        return Ok(json!({ "dialog": true }));
    };
    if s.tasks.file.tasks.len() >= MAX_TASKS {
        return Err(CommandError::Failed("too many tasks; close some".into()));
    }
    let id = format!("task-{}", now_ms());
    let id = if s.tasks.file.tasks.iter().any(|t| t.id == id) { format!("{id}-{}", s.tasks.file.tasks.len()) } else { id };
    let branch = bool_or(p, "createBranch", false).then(|| slug(&summary)).filter(|b| !b.is_empty());
    s.tasks.file.tasks.push(Task {
        id: id.clone(),
        summary: summary.clone(),
        description: opt_str(p, "description").unwrap_or_default().to_string(),
        url: opt_str(p, "url").map(str::trim).filter(|u| !u.is_empty()).map(str::to_string),
        created_ms: now_ms(),
        branch: branch.clone(),
        ..Task::default()
    });
    s.tasks.dialog = None;
    let mut v = switch_to(s, &id)?;
    // The new task starts with an empty context unless asked to keep the current one.
    if bool_or(p, "keepContext", false) {
        let prev = s.tasks.file.tasks.iter().find(|t| t.id != id && t.context.editors.len() + t.context.breakpoints.len() > 0).map(|t| t.context.clone());
        if let Some(ctx) = prev {
            restore(s, &ctx);
        }
    }
    if let Some(b) = branch
        && s.vcs.repo.is_some()
        && s.execute("git.newBranch", &json!({})).is_ok()
        && let Some(crate::vcs::Dialog::NewBranch { name, .. }) = &mut s.vcs.dialog
    {
        *name = b.clone();
        v["branch"] = Value::String(b);
    }
    Ok(v)
}

fn edit_task(s: &mut Session, p: &Value) -> CmdResult {
    let has_any = ["summary", "description", "url"].iter().any(|k| p.get(*k).is_some());
    let active = s.tasks.active().cloned().ok_or_else(|| CommandError::Failed("no active task".into()))?;
    if !has_any {
        s.tasks.dialog = Some(TaskDialog {
            kind: TaskDialogKind::Edit,
            summary: active.summary,
            description: active.description,
            url: active.url.unwrap_or_default(),
            create_branch: false,
            error: None,
        });
        return Ok(json!({ "dialog": true }));
    }
    let id = active.id;
    let t = s.tasks.file.tasks.iter_mut().find(|t| t.id == id).ok_or_else(|| CommandError::Failed("no active task".into()))?;
    if let Some(v) = opt_str(p, "summary").map(str::trim) {
        if v.is_empty() {
            return Err(CommandError::BadParams("the summary is empty".into()));
        }
        t.summary = v.to_string();
    }
    if let Some(v) = opt_str(p, "description") {
        t.description = v.to_string();
    }
    if let Some(v) = opt_str(p, "url") {
        t.url = Some(v.trim().to_string()).filter(|u| !u.is_empty());
    }
    s.tasks.dialog = None;
    persist(s)?;
    ok()
}

fn save_context(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = opt_str(p, "name").map(str::trim).filter(|x| !x.is_empty()).map(str::to_string) else {
        s.tasks.dialog = Some(TaskDialog {
            kind: TaskDialogKind::SaveContext,
            summary: String::new(),
            description: String::new(),
            url: String::new(),
            create_branch: false,
            error: None,
        });
        return Ok(json!({ "dialog": true }));
    };
    let context = capture(s);
    s.tasks.file.contexts.retain(|c| c.name != name);
    if s.tasks.file.contexts.len() >= MAX_CONTEXTS {
        s.tasks.file.contexts.remove(0);
    }
    let editors = context.editors.len();
    s.tasks.file.contexts.push(NamedContext { name: name.clone(), saved_ms: now_ms(), context });
    s.tasks.dialog = None;
    persist(s)?;
    s.status = format!("Saved context '{name}'");
    Ok(json!({ "name": name, "editors": editors }))
}

fn load_context(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = opt_str(p, "name") else {
        if s.tasks.file.contexts.is_empty() {
            return Err(CommandError::Failed("there are no saved contexts (Save Context…)".into()));
        }
        s.tasks.chooser = Some(TasksChooser { kind: ChooserKind::LoadContext, query: String::new() });
        return Ok(json!({ "chooser": true }));
    };
    let ctx = s
        .tasks
        .file
        .contexts
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.context.clone())
        .ok_or_else(|| CommandError::BadParams(format!("no context named '{name}'")))?;
    let opened = restore(s, &ctx);
    s.tasks.chooser = None;
    s.status = format!("Loaded context '{name}'");
    Ok(json!({ "editors": opened }))
}

fn not_default(s: &Session) -> Result<(), String> {
    if s.tasks.file.active == DEFAULT_TASK { Err("the default task cannot be closed".into()) } else { Ok(()) }
}

fn active_has(s: &Session, what: fn(&Task) -> bool, reason: &str) -> Result<(), String> {
    if s.tasks.active().is_some_and(what) { Ok(()) } else { Err(reason.to_string()) }
}

/// Every command first loads the project's tasks.
macro_rules! with_tasks {
    ($f:expr) => {
        |s: &mut Session, p: &Value| {
            ensure_loaded(s);
            $f(s, p)
        }
    };
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec("tools.tasksContexts.switchTask", "Switch Task…", None, "{task?: string (id or summary)} — save the current context into the active task and restore the target's; without `task` opens the chooser", Context::Global, always, with_tasks!(|s: &mut Session, p: &Value| match opt_str(p, "task") {
            Some(t) => {
                let id = find_task(s, t).ok_or_else(|| CommandError::BadParams(format!("no task '{t}'")))?;
                switch_to(s, &id)
            }
            None => {
                s.tasks.chooser = Some(TasksChooser { kind: ChooserKind::SwitchTask, query: String::new() });
                Ok(json!({ "chooser": true }))
            }
        })),
        spec("tools.tasksContexts.openTask", "Open Task…", None, "{summary?: string, description?: string, url?: string, createBranch?: bool, keepContext?: bool} — create a local task and switch to it (with `createBranch`, the New Branch dialog opens prefilled); without `summary` opens the dialog", Context::Global, always, with_tasks!(open_task)),
        spec("tools.tasksContexts.closeActiveTask", "Close Active Task…", None, "Mark the active task closed and switch to the default task", Context::Global, not_default, with_tasks!(|s: &mut Session, _p: &Value| {
            let id = s.tasks.file.active.clone();
            let r = switch_to(s, DEFAULT_TASK)?;
            if let Some(t) = s.tasks.file.tasks.iter_mut().find(|t| t.id == id) {
                t.closed = true;
            }
            persist(s)?;
            Ok(r)
        })),
        spec("tools.tasksContexts.editDefaultTask", "Edit Active Task", None, "{summary?: string, description?: string, url?: string} — edit the active task; without params opens the dialog", Context::Global, always, with_tasks!(edit_task)),
        spec("tools.tasksContexts.showDescription", "Show Description", None, "Show the active task's description", Context::Global, |s| active_has(s, |t| !t.description.trim().is_empty(), "the active task has no description"), with_tasks!(|s: &mut Session, _p: &Value| {
            let t = s.tasks.active().cloned().ok_or_else(|| CommandError::Failed("no active task".into()))?;
            s.popup = Some(Popup::Message { title: t.summary, text: t.description });
            ok()
        })),
        spec("tools.tasksContexts.openInBrowser", "Open in Browser", None, "Open the active task's URL", Context::Global, |s| active_has(s, |t| t.url.is_some(), "the active task has no URL"), with_tasks!(|s: &mut Session, _p: &Value| {
            let url = s.tasks.active().and_then(|t| t.url.clone()).ok_or_else(|| CommandError::Failed("the active task has no URL".into()))?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(CommandError::Failed(format!("'{url}' is not a web address")));
            }
            s.effects.push(Effect::OpenUrl(url.clone()));
            Ok(json!({ "url": url }))
        })),
        spec("tools.tasksContexts.analyzeStackTraceFromTask", "Analyze Stack Trace From Task…", None, "Analyze a stack trace in the active task's description", Context::Global, |s| active_has(s, |t| !t.description.trim().is_empty(), "the active task has no description"), with_tasks!(|s: &mut Session, _p: &Value| {
            let text = s.tasks.active().map(|t| t.description.clone()).unwrap_or_default();
            s.execute("code.analyzeStackTraceOrThreadDump", &json!({ "text": text }))
        })),
        spec("tools.tasksContexts.configureServers", "Configure Servers…", None, "Explains task servers: CodeCraft's tasks are local", Context::Global, always, |s, _| {
            s.popup = Some(Popup::Message {
                title: "Task Servers".into(),
                text: "CodeCraft keeps tasks locally in the project (.codecraft/tasks.json); issue trackers are not connected. Use Open Task… to create a task with a summary, description and URL.".into(),
            });
            ok()
        }),
        spec("tools.tasksContexts.timeTracking.startTimer", "Start Timer", None, "{on?: bool} — start (or stop) tracking time on the active task", Context::Global, always, with_tasks!(|s: &mut Session, p: &Value| {
            let on = p.get("on").and_then(Value::as_bool).unwrap_or(s.tasks.timer_started_ms.is_none());
            if on {
                if s.tasks.timer_started_ms.is_none() {
                    s.tasks.timer_started_ms = Some(now_ms());
                }
            } else {
                stop_timer(s);
                persist(s)?;
            }
            Ok(json!({ "running": on, "spent": format_duration(s.tasks.active_time_ms()) }))
        })),
        spec("tools.tasksContexts.saveContext", "Save Context…", None, "{name?: string} — save the open editors and breakpoints as a named context", Context::Global, always, with_tasks!(save_context)),
        spec("tools.tasksContexts.loadContext", "Load Context…", None, "{name?: string} — close everything and open a saved context; without `name` opens the chooser", Context::Global, always, with_tasks!(load_context)),
        spec("tools.tasksContexts.clearContext", "Clear Context", None, "Close every editor and remove the breakpoints", Context::Global, always, |s, _| {
            clear(s);
            s.status = "Context cleared".into();
            ok()
        }),
        spec("tasks.list", "Tasks", None, "→ {active, timer, tasks, contexts}", Context::Global, always, with_tasks!(|s: &mut Session, _p: &Value| {
            Ok(json!({
                "active": s.tasks.file.active,
                "timer": s.tasks.timer_started_ms.map(|_| format_duration(s.tasks.active_time_ms())),
                "tasks": s.tasks.file.tasks,
                "contexts": s.tasks.file.contexts.iter().map(|c| json!({ "name": c.name, "editors": c.context.editors.len(), "breakpoints": c.context.breakpoints.len() })).collect::<Vec<_>>(),
            }))
        })),
        spec("tasks.contexts.delete", "Delete Context", None, "{name: string}", Context::Global, always, with_tasks!(|s: &mut Session, p: &Value| {
            let name = req_str(p, "name")?.to_string();
            let before = s.tasks.file.contexts.len();
            s.tasks.file.contexts.retain(|c| c.name != name);
            if before == s.tasks.file.contexts.len() {
                return Err(CommandError::BadParams(format!("no context named '{name}'")));
            }
            persist(s)?;
            ok()
        })),
        spec("tasks.chooser.accept", "Choose", None, "{index: int} — pick an item of the open chooser (its filtered list)", Context::Global, |s| s.tasks.chooser.as_ref().map(|_| ()).ok_or_else(|| "no chooser is open".into()), with_tasks!(|s: &mut Session, p: &Value| {
            let i = opt_usize(p, "index")?.unwrap_or(0);
            let items = chooser_items(s);
            let (key, _) = items.get(i).cloned().ok_or_else(|| CommandError::BadParams(format!("no item {i}")))?;
            match s.tasks.chooser.as_ref().map(|c| c.kind) {
                Some(ChooserKind::SwitchTask) => switch_to(s, &key),
                Some(ChooserKind::LoadContext) => load_context(s, &json!({ "name": key })),
                None => ok(),
            }
        })),
        spec("tasks.dialog.ok", "OK", None, "Run the open task dialog", Context::Global, |s| s.tasks.dialog.as_ref().map(|_| ()).ok_or_else(|| "no dialog is open".into()), with_tasks!(|s: &mut Session, _p: &Value| {
            let d = s.tasks.dialog.clone().ok_or_else(|| CommandError::Failed("no dialog".into()))?;
            let r = match d.kind {
                TaskDialogKind::Open => open_task(s, &json!({ "summary": d.summary, "description": d.description, "url": d.url, "createBranch": d.create_branch })),
                TaskDialogKind::Edit => edit_task(s, &json!({ "summary": d.summary, "description": d.description, "url": d.url })),
                TaskDialogKind::SaveContext => save_context(s, &json!({ "name": d.summary })),
            };
            if let Err(e) = &r
                && let Some(d) = &mut s.tasks.dialog
            {
                d.error = Some(e.to_string());
            }
            r
        })),
        spec("tasks.dismiss", "Close", None, "Close the task dialog and chooser", Context::Global, always, |s, _| {
            s.tasks.dialog = None;
            s.tasks.chooser = None;
            ok()
        }),
    ]
}

/// The open chooser's items after its query: (task id or context name, label).
pub fn chooser_items(s: &Session) -> Vec<(String, String)> {
    let Some(c) = &s.tasks.chooser else { return Vec::new() };
    let q = c.query.trim().to_lowercase();
    let matches = |label: &str| q.is_empty() || label.to_lowercase().contains(&q);
    match c.kind {
        ChooserKind::SwitchTask => s
            .tasks
            .file
            .tasks
            .iter()
            .filter(|t| matches(&t.summary))
            .map(|t| {
                let mut label = t.summary.clone();
                if t.id == s.tasks.file.active {
                    label.push_str("  (active)");
                } else if t.closed {
                    label.push_str("  (closed)");
                }
                (t.id.clone(), label)
            })
            .collect(),
        ChooserKind::LoadContext => s
            .tasks
            .file
            .contexts
            .iter()
            .filter(|c| matches(&c.name))
            .map(|c| (c.name.clone(), format!("{}  ({} files)", c.name, c.context.editors.len())))
            .collect(),
    }
}

/// The label of Edit 'Default task' with the active task's name.
pub fn edit_label(s: &Session) -> String {
    format!("Edit '{}'", s.tasks.active().map(|t| t.summary.as_str()).unwrap_or("Default task"))
}

/// Whether `p` is inside the project (contexts only reopen project files).
pub fn in_project(s: &Session, p: &Path) -> bool {
    s.ws.project.as_ref().is_none_or(|pr| p.starts_with(&pr.root))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn project() -> (tempfile::TempDir, PathBuf, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for f in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join(f), "fn x() {}\nfn y() {}\n").unwrap();
        }
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&root.join(".home"));
        s.execute("file.open", &json!({ "path": root })).unwrap();
        (tmp, root, s)
    }

    fn open_paths(s: &Session) -> Vec<String> {
        let mut v: Vec<String> = s
            .ws
            .all_tabs()
            .iter()
            .filter_map(|id| s.ws.editor(*id).and_then(|e| s.ws.doc(e.doc)).and_then(|d| d.path.clone()))
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn tasks_keep_their_own_context() {
        let (_tmp, root, mut s) = project();
        s.execute("file.open", &json!({ "path": root.join("a.rs") })).unwrap();
        s.execute("navigate.lineColumn", &json!({ "line": 2 })).ok();
        s.execute("tools.tasksContexts.openTask", &json!({ "summary": "Fix bug #12", "description": "thread 'main' panicked at src/a.rs:1:1" })).unwrap();
        assert!(open_paths(&s).is_empty(), "a new task starts empty");
        s.execute("file.open", &json!({ "path": root.join("b.rs") })).unwrap();
        s.execute("file.open", &json!({ "path": root.join("c.rs") })).unwrap();
        // Back to the default task: a.rs again.
        s.execute("tools.tasksContexts.switchTask", &json!({ "task": "Default task" })).unwrap();
        assert_eq!(open_paths(&s), ["a.rs"]);
        s.execute("tools.tasksContexts.switchTask", &json!({ "task": "fix bug #12" })).unwrap();
        assert_eq!(open_paths(&s), ["b.rs", "c.rs"]);
        assert!(s.enabled("tools.tasksContexts.showDescription").is_ok());
        s.execute("tools.tasksContexts.showDescription", &json!({})).unwrap();
        assert!(matches!(&s.popup, Some(Popup::Message { title, .. }) if title == "Fix bug #12"));
        assert!(s.enabled("tools.tasksContexts.openInBrowser").is_err());
        s.execute("tools.tasksContexts.editDefaultTask", &json!({ "url": "https://example.com/12" })).unwrap();
        s.execute("tools.tasksContexts.openInBrowser", &json!({})).unwrap();
        assert!(matches!(s.effects.last(), Some(Effect::OpenUrl(u)) if u == "https://example.com/12"));
        assert_eq!(edit_label(&s), "Edit 'Fix bug #12'");
        // Persisted per project.
        assert!(root.join(".codecraft").join(FILE).is_file());
        s.tasks.loaded_for = None;
        let v = s.execute("tasks.list", &json!({})).unwrap();
        assert_eq!(v["tasks"].as_array().unwrap().len(), 2);
        // Close returns to the default task.
        s.execute("tools.tasksContexts.closeActiveTask", &json!({})).unwrap();
        assert_eq!(s.tasks.file.active, DEFAULT_TASK);
        assert!(s.tasks.file.tasks.iter().any(|t| t.closed));
        assert!(s.enabled("tools.tasksContexts.closeActiveTask").is_err());
    }

    #[test]
    fn contexts_timer_and_chooser() {
        let (_tmp, root, mut s) = project();
        s.execute("file.open", &json!({ "path": root.join("a.rs") })).unwrap();
        s.execute("file.open", &json!({ "path": root.join("b.rs") })).unwrap();
        s.execute("tools.tasksContexts.saveContext", &json!({ "name": "pair" })).unwrap();
        s.execute("tools.tasksContexts.clearContext", &json!({})).unwrap();
        assert!(open_paths(&s).is_empty());
        s.execute("tools.tasksContexts.loadContext", &json!({})).unwrap();
        assert_eq!(chooser_items(&s).len(), 1);
        s.execute("tasks.chooser.accept", &json!({ "index": 0 })).unwrap();
        assert_eq!(open_paths(&s), ["a.rs", "b.rs"]);
        assert!(s.execute("tasks.chooser.accept", &json!({ "index": 5 })).is_err());
        // Timer.
        s.execute("tools.tasksContexts.timeTracking.startTimer", &json!({})).unwrap();
        assert!(s.tasks.timer_started_ms.is_some());
        s.tasks.timer_started_ms = Some(now_ms() - 65_000);
        s.execute("tools.tasksContexts.timeTracking.startTimer", &json!({})).unwrap();
        assert!(s.tasks.active().unwrap().time_spent_ms >= 65_000);
        assert_eq!(format_duration(65_000), "1m 05s");
        assert_eq!(format_duration(3_900_000), "1h 05m");
        // Configure Servers explains; switching to an unknown task fails.
        s.execute("tools.tasksContexts.configureServers", &json!({})).unwrap();
        assert!(s.execute("tools.tasksContexts.switchTask", &json!({ "task": "nope" })).is_err());
        assert_eq!(slug("Fix: the #12 bug!"), "fix-the-12-bug");
    }
}
