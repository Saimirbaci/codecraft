//! The floating feedback panel (tools, ink, description, target, agent, Submit) and the history
//! window.

use egui::{
    Color32, CornerRadius, FontId, Id, LayerId, Order, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::feedback::{MODES, Target, Thread};

use super::{PALETTE, Tool, WIDTHS, begin_submit, color32};
use crate::theme::Theme;
use crate::{UiState, run_command};

const PANEL: &str = "codecraft-feedback-panel";
const HISTORY: &str = "codecraft-feedback-history";

fn mode_label(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "auto" => ("Auto", "The agent works on its own"),
        "ask" => ("Ask", "The agent asks before each change or command"),
        _ => ("Review", "The agent works in a git worktree; nothing lands until you approve the diff"),
    }
}

/// A tool button with a painted icon.
fn tool_button(ui: &mut Ui, theme: &Theme, tool: Tool, selected: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(r, CornerRadius::same(4), theme.list_selection);
    } else if resp.hovered() {
        p.rect_filled(r, CornerRadius::same(4), theme.hover);
    }
    let c = r.center();
    let ink = Stroke::new(1.6, theme.text);
    let q = |x: f32, y: f32| c + vec2(x, y);
    match tool {
        Tool::Select => {
            let pts =
                vec![q(-4.0, -7.0), q(-4.0, 6.0), q(-0.5, 2.5), q(2.5, 7.5), q(4.5, 6.5), q(1.5, 1.5), q(6.0, 1.5)];
            p.add(egui::Shape::closed_line(pts, ink));
        }
        Tool::Pen => {
            let pts: Vec<_> = (0..=12).map(|i| i as f32).map(|i| q(-7.0 + i * 1.2, (i * 0.9).sin() * 4.0)).collect();
            p.add(egui::Shape::line(pts, ink));
        }
        Tool::Line => {
            p.line_segment([q(-6.0, 6.0), q(6.0, -6.0)], ink);
        }
        Tool::Arrow => {
            p.line_segment([q(-6.0, 6.0), q(6.0, -6.0)], ink);
            p.add(egui::Shape::line(vec![q(0.0, -6.0), q(6.0, -6.0), q(6.0, 0.0)], ink));
        }
        Tool::Rect => {
            p.rect_stroke(Rect::from_center_size(c, vec2(14.0, 10.0)), CornerRadius::ZERO, ink, StrokeKind::Middle);
        }
        Tool::Ellipse => {
            p.add(egui::Shape::ellipse_stroke(c, vec2(7.0, 5.0), ink));
        }
        Tool::Highlight => {
            let r = Rect::from_center_size(c, vec2(14.0, 8.0));
            p.rect_filled(r, CornerRadius::same(1), theme.warning.gamma_multiply(0.6));
        }
        Tool::Text => {
            p.text(c, egui::Align2::CENTER_CENTER, "T", FontId::proportional(16.0), theme.text);
        }
        Tool::Eraser => {
            let pts = vec![q(-7.0, 2.0), q(-1.0, -5.0), q(6.0, 1.0), q(0.0, 7.0)];
            p.add(egui::Shape::closed_line(pts, ink));
            p.line_segment([q(-4.0, -1.5), q(3.0, 4.5)], ink);
        }
    }
    let key = tool.key().symbol_or_name();
    resp.on_hover_text(format!("{} ({key})", tool.label())).clicked()
}

fn swatch(ui: &mut Ui, theme: &Theme, hex: &str, selected: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::click());
    let p = ui.painter();
    p.circle_filled(r.center(), 7.0, color32(hex, 255));
    p.circle_stroke(r.center(), 7.0, Stroke::new(1.0, theme.separator));
    if selected {
        p.circle_stroke(r.center(), 9.5, Stroke::new(1.5, theme.text));
    }
    resp.on_hover_text(hex).clicked()
}

fn width_button(ui: &mut Ui, theme: &Theme, width: f32, color: Color32, selected: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(r, CornerRadius::same(4), theme.list_selection);
    } else if resp.hovered() {
        p.rect_filled(r, CornerRadius::same(4), theme.hover);
    }
    p.circle_filled(r.center(), width / 2.0 + 0.5, color);
    resp.on_hover_text(format!("Stroke {width} pt ([ / ])")).clicked()
}

fn section(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(11.0).color(theme.text_muted));
}

/// The floating panel. Drawn above the canvas.
pub(super) fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState, screen: Rect) {
    let id = Id::new(PANEL);
    let busy = st.feedback.busy();
    let theme = st.theme.clone();
    let mut open = true;
    let width = 340.0;
    crate::windows::resizable(egui::Window::new(RichText::new("Feedback for an agent").strong()), st, "feedback")
        .id(id)
        .order(Order::Foreground)
        .open(&mut open)
        .collapsible(false)
        .min_width(width)
        .default_pos(pos2(screen.right() - width - 24.0, screen.top() + 64.0))
        .default_width(width)
        .show(ctx, |ui| {
            ui.set_min_width(width);
            ui.add_enabled_ui(!busy, |ui| body(ui, s, st, &theme));
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Capturing the window…").color(theme.text_muted));
                });
            }
        });
    ctx.move_to_top(LayerId::new(Order::Foreground, id));
    if !open {
        run_command(s, st, "feedback.toggle", &json!({ "open": false }));
    }
}

fn body(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme) {
    // Tools.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for tool in Tool::ALL {
            if tool_button(ui, theme, tool, st.feedback.tool == tool) {
                st.feedback.tool = tool;
                st.feedback.selected = None;
            }
        }
    });
    // Ink and width.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        for hex in PALETTE {
            if swatch(ui, theme, hex, st.feedback.color == hex) {
                st.feedback.color = hex.to_string();
            }
        }
        ui.add_space(8.0);
        let ink = color32(&st.feedback.color, 255);
        for w in WIDTHS {
            if width_button(ui, theme, w, ink, (st.feedback.width - w).abs() < 0.01) {
                st.feedback.width = w;
            }
        }
    });
    ui.horizontal(|ui| {
        if ui.add_enabled(s.feedback.can_undo(), egui::Button::new("Undo")).on_hover_text("⌘Z").clicked() {
            st.feedback.selected = None;
            run_command(s, st, "feedback.undo", &Value::Null);
        }
        if ui.add_enabled(s.feedback.can_redo(), egui::Button::new("Redo")).on_hover_text("⇧⌘Z").clicked() {
            st.feedback.selected = None;
            run_command(s, st, "feedback.redo", &Value::Null);
        }
        let n = s.feedback.draft.shapes.len();
        if ui.add_enabled(n > 0, egui::Button::new("Clear")).clicked() {
            st.feedback.selected = None;
            run_command(s, st, "feedback.clear", &Value::Null);
        }
        ui.label(RichText::new(format!("{n} mark{}", if n == 1 { "" } else { "s" })).color(theme.text_muted));
    });
    ui.separator();

    if let Some(thread) = s.feedback.thread.clone() {
        thread_box(ui, s, st, theme, &thread);
    }
    let continuing = s.feedback.thread.is_some();
    section(ui, theme, if continuing { "What should change next?" } else { "What should change?" });
    let hint = if s.feedback.draft.session.is_some() {
        "More to add, or something not done right? The agent continues where it left off."
    } else {
        "Describe the feature or bug. Your drawings show where."
    };
    ui.add(
        egui::TextEdit::multiline(&mut s.feedback.draft.text)
            .id(Id::new("codecraft-feedback-description"))
            .hint_text(hint)
            .desired_rows(5)
            .desired_width(f32::INFINITY),
    );
    if !continuing && let Some(parent) = &s.feedback.draft.parent {
        ui.label(RichText::new(format!("Revision of {parent}")).size(11.0).color(theme.text_muted));
    }

    section(ui, theme, "Change");
    let has_project = s.ws.project.is_some();
    ui.horizontal(|ui| {
        let t = &mut s.feedback.draft.target;
        ui.radio_value(t, Target::CodeCraft, Target::CodeCraft.label());
        ui.add_enabled_ui(has_project, |ui| {
            ui.radio_value(t, Target::Project, Target::Project.label()).on_disabled_hover_text("No project is open");
        });
    });

    section(ui, theme, "Agent");
    // A follow-up goes to the thread's own agent session: its agent and mode are fixed.
    let fixed = s.feedback.draft.session.is_some();
    let fixed_tip =
        format!("This thread continues in its {} session; New thread to choose another", s.feedback.draft.backend);
    ui.add_enabled_ui(!fixed, |ui| {
        ui.horizontal_wrapped(|ui| {
            let backends = s.feedback.backends.clone();
            for b in &backends {
                let selected = s.feedback.draft.backend == b.id;
                let mut r = ui.add_enabled(b.available, egui::Button::selectable(selected, b.label.as_str()));
                if let Some(note) = &b.note {
                    r = r.on_hover_text(note).on_disabled_hover_text(note);
                }
                if r.clicked() {
                    run_command(s, st, "feedback.set", &json!({ "backend": b.id }));
                }
            }
        });
        if let Some(note) = &s.feedback.backends_note {
            ui.label(RichText::new(note).size(11.0).color(theme.text_muted));
        }
        let current = s.feedback.backends.iter().find(|b| b.id == s.feedback.draft.backend).cloned();
        ui.horizontal(|ui| {
            for m in MODES {
                let (label, tip) = mode_label(m);
                let why = current.as_ref().and_then(|b| b.mode_unavailable(m));
                let r = ui
                    .add_enabled(why.is_none(), egui::Button::selectable(s.feedback.draft.mode == m, label))
                    .on_hover_text(tip)
                    .on_disabled_hover_text(why.unwrap_or(tip));
                if r.clicked() {
                    s.feedback.draft.mode = m.to_string();
                }
            }
        });
    })
    .response
    .on_disabled_hover_text(fixed_tip);
    ui.label(RichText::new(mode_label(&s.feedback.draft.mode).1).size(11.0).color(theme.text_muted));
    ui.separator();

    ui.horizontal(|ui| {
        let ready = !s.feedback.draft.text.trim().is_empty();
        let label = if s.feedback.draft.session.is_some() { "Send follow-up" } else { "Send to agent" };
        let send = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(theme.accent);
        if ui.add_enabled(ready, send).on_disabled_hover_text("Describe the change first").clicked() {
            begin_submit(s, st);
        }
        if ui.button("Cancel").on_hover_text("Discard the drawing and the description").clicked() {
            run_command(s, st, "feedback.cancel", &Value::Null);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("History…").clicked() {
                run_command(s, st, "feedback.history", &json!({ "open": true }));
            }
        });
    });
    if !continuing {
        recent(ui, s, st, theme);
    }
    last_submission(ui, s, st, theme);
}

/// `2026-10-12T14:25:30.000Z` → `2026-10-12 14:25`.
fn when(created: &str) -> String {
    created.get(..16).unwrap_or(created).replace('T', " ")
}

fn ellipsize(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// The thread the draft continues: its earlier requests and what the next send does.
fn thread_box(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme, thread: &Thread) {
    let mut action: Option<(&'static str, Value)> = None;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(format!("Thread: {}", ellipsize(&thread.title, 60))).strong());
        let how = match &s.feedback.draft.session {
            Some(_) => format!("Sent to the same {} session: the agent remembers its earlier work.", thread.backend),
            None => "A new agent run that gets the earlier requests.".to_string(),
        };
        ui.label(RichText::new(how).size(11.0).color(theme.text_muted));
        let n = thread.entries.len();
        egui::CollapsingHeader::new(format!("Earlier requests ({n})"))
            .id_salt("codecraft-feedback-thread-entries")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                    for e in &thread.entries {
                        ui.label(RichText::new(when(&e.created)).font(FontId::monospace(10.0)).color(theme.text_muted));
                        let text = if e.text.trim().is_empty() { e.title.as_str() } else { e.text.as_str() };
                        ui.label(ellipsize(text.trim(), 600));
                        ui.add_space(4.0);
                    }
                });
            });
        ui.horizontal_wrapped(|ui| {
            if let Some(session) = &thread.session
                && ui
                    .small_button("Agent session")
                    .on_hover_text("Open the agent's work so far in the Agent tool window")
                    .clicked()
            {
                action = Some(("agent.sessions.open", json!({ "session": session })));
            }
            if let Some(target) = build_target(thread.session.as_ref(), thread.job)
                && ui.small_button("Build & run").on_hover_text("Build and run the agent's version").clicked()
            {
                action = Some(("feedback.runIteration", target));
            }
            if ui.small_button("New thread").on_hover_text("Start new feedback instead of following up").clicked() {
                action = Some(("feedback.newThread", Value::Null));
            }
        });
    });
    if let Some((cmd, params)) = action {
        run_command(s, st, cmd, &params);
    }
}

/// Build & run's target: the saved session outlives the job id (restarts).
fn build_target(session: Option<&String>, job: Option<u64>) -> Option<Value> {
    match (session, job) {
        (Some(session), _) => Some(json!({ "session": session })),
        (None, Some(job)) => Some(json!({ "job": job })),
        (None, None) => None,
    }
}

/// The latest threads, to follow up on without opening the history.
fn recent(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme) {
    if s.feedback.threads.is_empty() {
        return;
    }
    ui.separator();
    section(ui, theme, "Recent feedback");
    let mut action = None;
    for t in s.feedback.threads.iter().take(3) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Continue")
                    .on_hover_text("Follow up on this thread: add more or fix what was not done right")
                    .clicked()
                {
                    action = Some(t.id.clone());
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(ellipsize(&t.title, 40)).truncate()).on_hover_text(&t.title);
                });
            });
        });
        let updates = t.entries.len();
        let meta =
            format!("{} · {} · {updates} request{}", when(&t.updated), t.backend, if updates == 1 { "" } else { "s" });
        ui.label(RichText::new(meta).size(10.0).color(theme.text_muted));
    }
    if let Some(id) = action {
        run_command(s, st, "feedback.continue", &json!({ "id": id }));
    }
}

/// "Sent to …" and the Build & run action for the last job.
fn last_submission(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme) {
    let Some(last) = s.feedback.last.clone() else { return };
    ui.separator();
    let what = match last.job {
        Some(j) => format!("Last feedback went to {} as job {j}.", last.backend),
        None => format!("Last feedback went to {}.", last.backend),
    };
    ui.label(RichText::new(what).size(11.0).color(theme.text_muted));
    if let Some(job) = last.job
        && ui
            .button("Build & run the new version")
            .on_hover_text("cargo run -p codecraft in the job's worktree while it is in review (try it before accepting), or in the project once the review is done")
            .clicked()
    {
        run_command(s, st, "feedback.runIteration", &json!({ "job": job }));
    }
}

/// Past feedback by thread: continue a thread, revise a request, open the agent session or build
/// its result.
pub(super) fn history(ctx: &egui::Context, s: &mut Session, st: &mut UiState, on_top: bool) {
    let id = Id::new(HISTORY);
    let theme = st.theme.clone();
    let mut open = true;
    let mut action: Option<(&'static str, Value)> = None;
    crate::windows::resizable(egui::Window::new("Feedback History"), st, "feedback-history")
        .id(id)
        .order(Order::Foreground)
        .open(&mut open)
        .collapsible(false)
        .default_size(vec2(560.0, 380.0))
        .show(ctx, |ui| {
            if s.feedback.threads.is_empty() {
                ui.label(RichText::new("No feedback yet.").color(theme.text_muted));
            }
            egui::ScrollArea::vertical().max_height(460.0).show(ui, |ui| {
                for t in &s.feedback.threads {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(when(&t.updated)).font(FontId::monospace(11.0)).color(theme.text_muted));
                        ui.label(RichText::new(ellipsize(&t.title, 60)).strong()).on_hover_text(&t.id);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(target) = build_target(t.session.as_ref(), t.job)
                                && ui
                                    .small_button("Build & run")
                                    .on_hover_text("Build and run the agent's version")
                                    .clicked()
                            {
                                action = Some(("feedback.runIteration", target));
                            }
                            if let Some(session) = &t.session
                                && ui
                                    .small_button("Session")
                                    .on_hover_text("Open the agent session in the Agent tool window")
                                    .clicked()
                            {
                                action = Some(("agent.sessions.open", json!({ "session": session })));
                            }
                            if ui
                                .small_button("Continue")
                                .on_hover_text("Follow up on this thread in the same agent session")
                                .clicked()
                            {
                                action = Some(("feedback.continue", json!({ "id": t.id })));
                            }
                            ui.label(RichText::new(&t.backend).size(11.0).color(theme.text_muted));
                        });
                    });
                    egui::CollapsingHeader::new(
                        RichText::new(format!(
                            "{} request{}",
                            t.entries.len(),
                            if t.entries.len() == 1 { "" } else { "s" }
                        ))
                        .size(11.0)
                        .color(theme.text_muted),
                    )
                    .id_salt(("codecraft-feedback-history-thread", &t.id))
                    .show(ui, |ui| {
                        for e in &t.entries {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(when(&e.created))
                                        .font(FontId::monospace(10.0))
                                        .color(theme.text_muted),
                                );
                                let title = if e.title.is_empty() { "(no description)" } else { e.title.as_str() };
                                ui.label(title).on_hover_text(ellipsize(&e.text, 600));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui
                                        .small_button("Revise")
                                        .on_hover_text("Edit this request and send it as a new run")
                                        .clicked()
                                    {
                                        action = Some(("feedback.reopen", json!({ "id": e.id })));
                                    }
                                    if let Some(job) = e.job {
                                        ui.label(
                                            RichText::new(format!("job {job}")).size(10.0).color(theme.text_muted),
                                        );
                                    }
                                });
                            });
                        }
                    });
                    ui.separator();
                }
            });
        });
    if on_top {
        ctx.move_to_top(LayerId::new(Order::Foreground, id));
    }
    if !open {
        s.feedback.history_open = false;
    }
    if let Some((cmd, params)) = action {
        run_command(s, st, cmd, &params);
    }
}
