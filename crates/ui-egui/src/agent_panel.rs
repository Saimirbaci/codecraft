//! The Agent tool window: job launcher (backend, mode, prompt, context toggles), the job list,
//! a live activity feed, follow-ups, and the review of worktree changes with per-file diffs.
//! OWNED BY the agents track.

use std::path::{Path, PathBuf};

use egui::{Align, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Modifiers, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent::{Backend, BackendInfo, ChangeKind};
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
    pub selected: Option<JobId>,
    pub follow_up: String,
    /// The review file whose diff is shown (relative path).
    pub review_file: Option<String>,
    pub side_by_side: bool,
    pub show_logs: bool,
    pub approvals_collapsed: bool,
}

impl Default for AgentUi {
    fn default() -> Self {
        Self {
            backend: Backend::Claude,
            mode: AgentMode::Review,
            prompt: String::new(),
            attach_file: true,
            attach_selection: true,
            selected: None,
            follow_up: String::new(),
            review_file: None,
            side_by_side: false,
            show_logs: false,
            approvals_collapsed: false,
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
    let backends: Vec<BackendInfo> = s.agents.backends().to_vec();
    new_job(ui, s, st, &theme, &backends);
    ui.add_space(4.0);
    ui.separator();
    if s.agents.jobs.is_empty() {
        ui.label(RichText::new("No agent jobs yet.").color(theme.text_muted));
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
        });
    });
    let support = current.as_ref().map_or(Err("unknown agent".to_string()), |i| mode_support(i, st.agent.mode));
    if let Err(why) = &support {
        ui.label(RichText::new(why).small().color(theme.warning));
    }

    let prompt_id = Id::new("agent-prompt");
    let mut send =
        ui.memory(|m| m.has_focus(prompt_id)) && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
    ui.add(
        TextEdit::multiline(&mut st.agent.prompt)
            .id(prompt_id)
            .hint_text("Describe the task… (⌘↩ to send)")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
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
        let params = json!({
            "backend": st.agent.backend.id(),
            "mode": mode_id(st.agent.mode),
            "prompt": st.agent.prompt,
            "includeActiveFile": st.agent.attach_file,
            "includeSelection": st.agent.attach_selection,
        });
        match s.execute("agent.start", &params) {
            Ok(v) => {
                st.agent.prompt.clear();
                st.agent.selected = v.get("job").and_then(Value::as_u64).map(JobId);
                st.agent.review_file = None;
            }
            Err(e) => s.status = e.to_string(),
        }
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
    let error = job.error.clone().filter(|_| job.status == JobStatus::Failed);
    let mut action: Option<&str> = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(mark).color(color));
        ui.label(RichText::new(&title).strong().color(theme.text));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("Discard").on_hover_text("Stop the job, remove its worktree and forget it").clicked() {
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
    ui.horizontal(|ui| {
        let w = ui.available_width() - 60.0;
        let resp =
            ui.add(TextEdit::singleline(&mut st.agent.follow_up).hint_text("Follow up…").desired_width(w.max(80.0)));
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            send = true;
        }
        if ui.add_enabled(!st.agent.follow_up.trim().is_empty(), egui::Button::new("Send")).clicked() {
            send = true;
        }
    });
    if send && !st.agent.follow_up.trim().is_empty() {
        let prompt = std::mem::take(&mut st.agent.follow_up);
        run_command(s, st, "agent.followUp", &json!({ "job": id.0, "prompt": prompt }));
        st.agent.review_file = None;
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
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Changes · {pending}/{}", files.len())).strong())
            .on_hover_text(format!("{pending} of {} files still to review", files.len()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if pending > 0 {
                if ui.small_button("Reject All").clicked() {
                    cmd = Some(("agent.review.rejectAll", json!({ "job": id.0 })));
                }
                if ui.small_button("Accept All").clicked() {
                    cmd = Some(("agent.review.acceptAll", json!({ "job": id.0 })));
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
