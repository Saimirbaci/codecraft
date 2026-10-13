//! The floating feedback panel (tools, ink, description, target, agent, Submit) and the history
//! window.

use egui::{
    Color32, CornerRadius, FontId, Id, LayerId, Order, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::feedback::{MODES, Target};

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

    section(ui, theme, "What should change?");
    ui.add(
        egui::TextEdit::multiline(&mut s.feedback.draft.text)
            .id(Id::new("codecraft-feedback-description"))
            .hint_text("Describe the feature or bug. Your drawings show where.")
            .desired_rows(5)
            .desired_width(f32::INFINITY),
    );
    if let Some(parent) = &s.feedback.draft.parent {
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
    ui.label(RichText::new(mode_label(&s.feedback.draft.mode).1).size(11.0).color(theme.text_muted));
    ui.separator();

    ui.horizontal(|ui| {
        let ready = !s.feedback.draft.text.trim().is_empty();
        let send = egui::Button::new(RichText::new("Send to agent").color(Color32::WHITE)).fill(theme.accent);
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
    last_submission(ui, s, st, theme);
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

/// Past feedback: revise (re-open as a revision) or build the job's result.
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
        .default_size(vec2(520.0, 360.0))
        .show(ctx, |ui| {
            if s.feedback.history.is_empty() {
                ui.label(RichText::new("No feedback yet.").color(theme.text_muted));
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for r in &s.feedback.history {
                    ui.horizontal(|ui| {
                        let when = r.created.get(..16).unwrap_or(&r.created).replace('T', " ");
                        ui.label(RichText::new(when).font(FontId::monospace(11.0)).color(theme.text_muted));
                        let title = if r.title.is_empty() { "(no description)" } else { r.title.as_str() };
                        ui.label(title).on_hover_text(&r.id);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(job) = r.job
                                && ui.small_button("Build & run").on_hover_text(format!("Job {job}")).clicked()
                            {
                                action = Some(("feedback.runIteration", json!({ "job": job })));
                            }
                            if ui.small_button("Revise").on_hover_text("Re-open as a revision").clicked() {
                                action = Some(("feedback.reopen", json!({ "id": r.id })));
                            }
                            let mut meta = r.backend.clone();
                            if let Some(job) = r.job {
                                meta.push_str(&format!(" · job {job}"));
                            }
                            if r.parent.is_some() {
                                meta.push_str(" · revision");
                            }
                            ui.label(RichText::new(meta).size(11.0).color(theme.text_muted));
                        });
                    });
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
