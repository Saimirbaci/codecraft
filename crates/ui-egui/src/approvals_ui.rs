//! Approval cards for pending agent actions, from every source (Ask-mode agent jobs, the Claude
//! Code IDE bridge, feedback jobs): a stack floating at the top right of the editor, each card
//! with its source, title, a diff (file edits) or the command, and Allow / Always / Deny.
//! Click a card to focus it; then Enter allows and Esc denies. OWNED BY the agents track.

use egui::{Align2, CornerRadius, FontId, Frame, Id, Key, Margin, Modifiers, RichText, Sense, Stroke, vec2};
use serde_json::json;

use codecraft_engine::Session;
use codecraft_engine::approvals::{ApprovalKind, ApprovalRequest};

use crate::diff_view::{self, DiffOptions};
use crate::{UiState, run_command};

const CARD_WIDTH: f32 = 440.0;
/// Cards drawn at once; the rest wait behind a "+N more" line.
const MAX_CARDS: usize = 3;

fn card_id(req: &ApprovalRequest) -> Id {
    Id::new(("approval-card", req.id.0))
}

/// Draw pending approvals as a floating stack (no-op when nothing is pending).
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if s.approvals.is_empty() {
        return;
    }
    let pending: Vec<ApprovalRequest> = s.approvals.pending().cloned().collect();
    let ctx = ui.ctx().clone();
    // Top right of the editor area: left of the right stripe and any right-hand tool window
    // (the editor rect may be stale or absent, so derive it from the layout).
    let content = ctx.content_rect();
    let mut right = content.right() - 38.0;
    if s.ws.layout.right.is_some() && !s.ws.layout.all_hidden {
        right -= s.ws.layout.right_width;
    }
    let top = st.editor_rect.map(|r| r.top()).filter(|t| *t > content.top()).unwrap_or(content.top() + 80.0);
    let area = egui::Rect::from_min_max(egui::pos2(content.left(), top), egui::pos2(right, content.bottom()));
    let theme = st.theme.clone();
    let mono = FontId::monospace((s.ws.settings.editor_font_size - 1.0).max(9.0));
    let project = s.project_root();
    let mut decisions: Vec<(u64, &'static str)> = Vec::new();
    let width = CARD_WIDTH.min(area.width() - 16.0).max(240.0);

    egui::Area::new(Id::new("codecraft-approvals"))
        .order(egui::Order::Foreground)
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(area.right_top() + vec2(-10.0, 8.0))
        .show(&ctx, |ui| {
            ui.set_width(width);
            // Badge: the pending count, and a toggle to fold the cards away.
            let label = format!("⚠ {} pending approval{}", pending.len(), if pending.len() == 1 { "" } else { "s" });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let badge = egui::Button::new(RichText::new(label).color(theme.text).strong())
                    .fill(theme.warning.gamma_multiply(0.45))
                    .corner_radius(CornerRadius::same(10));
                let tip = if st.agent.approvals_collapsed { "Show approvals" } else { "Hide approvals" };
                if ui.add(badge).on_hover_text(tip).clicked() {
                    st.agent.approvals_collapsed = !st.agent.approvals_collapsed;
                }
            });
            if st.agent.approvals_collapsed {
                return;
            }
            for req in pending.iter().take(MAX_CARDS) {
                ui.add_space(6.0);
                if let Some(d) = card(ui, &theme, &mono, project.as_deref(), req, width) {
                    decisions.push((req.id.0, d));
                }
            }
            if pending.len() > MAX_CARDS {
                ui.add_space(4.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    ui.label(RichText::new(format!("+{} more", pending.len() - MAX_CARDS)).color(theme.text_muted));
                });
            }
        });
    for (id, decision) in decisions {
        run_command(s, st, "agent.approve", &json!({ "id": id, "decision": decision }));
    }
}

/// One card; returns the decision if one was made this frame.
fn card(
    ui: &mut egui::Ui,
    theme: &crate::theme::Theme,
    mono: &FontId,
    project: Option<&std::path::Path>,
    req: &ApprovalRequest,
    width: f32,
) -> Option<&'static str> {
    let id = card_id(req);
    // Clicking the card (outside its buttons) focuses it for Enter / Esc. The click area uses the
    // card's rect from the last frame and is registered first, so the buttons stay on top.
    if let Some(rect) = ui.ctx().data(|d| d.get_temp::<egui::Rect>(id)) {
        let resp = ui.interact(rect, id, Sense::click());
        if resp.clicked() {
            resp.request_focus();
        }
    }
    let focused = ui.memory(|m| m.has_focus(id));
    let mut decision = None;
    if focused {
        // Keep focus on Escape (egui drops focus on Escape by default) so it can mean Deny.
        let filter = egui::EventFilter { escape: true, ..Default::default() };
        ui.memory_mut(|m| m.set_focus_lock_filter(id, filter));
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            decision = Some("allow");
        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            decision = Some("deny");
        }
    }
    let stroke = if focused { Stroke::new(2.0, theme.accent) } else { Stroke::new(1.0, theme.separator) };
    let inner = Frame::new()
        .fill(theme.popup_bg)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(10))
        .shadow(ui.visuals().popup_shadow)
        .show(ui, |ui| {
            ui.set_width(width - 20.0);
            ui.horizontal(|ui| {
                let icon = match req.kind {
                    ApprovalKind::FileEdit { .. } => "📝",
                    ApprovalKind::Command { .. } => "▶",
                    ApprovalKind::ToolUse { .. } => "🔧",
                };
                ui.label(RichText::new(icon).color(theme.accent));
                ui.label(RichText::new(&req.source).strong().color(theme.text));
                ui.label(RichText::new("wants to").color(theme.text_muted));
            });
            ui.label(RichText::new(&req.title).color(theme.text));
            ui.add_space(4.0);
            match &req.kind {
                ApprovalKind::FileEdit { path, original, proposed } => {
                    let shown = project
                        .and_then(|p| path.strip_prefix(p).ok())
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(shown).font(mono.clone()).color(theme.text_editor));
                        let stats = diff_view::stats(ui, ("approval", req.id.0), original, proposed);
                        diff_view::stats_label(ui, theme, &stats);
                    });
                    let opts = DiffOptions { side_by_side: false, context: 2, max_height: 220.0 };
                    diff_view::show(ui, theme, mono, ("approval", req.id.0), original, proposed, &opts);
                }
                ApprovalKind::Command { command, cwd } => {
                    Frame::new().fill(theme.editor_bg).corner_radius(4).inner_margin(6).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(format!("$ {command}")).font(mono.clone()).color(theme.text_editor));
                    });
                    if let Some(cwd) = cwd {
                        ui.label(RichText::new(format!("in {}", cwd.display())).small().color(theme.text_muted));
                    }
                }
                ApprovalKind::ToolUse { tool, input } => {
                    ui.label(RichText::new(tool).font(mono.clone()).color(theme.text_editor));
                    if !input.is_null() {
                        let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
                        let pretty = codecraft_engine::agent::truncate(&pretty, 1200);
                        Frame::new().fill(theme.editor_bg).corner_radius(4).inner_margin(6).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::ScrollArea::vertical().max_height(140.0).id_salt(id.with("input")).show(ui, |ui| {
                                ui.label(RichText::new(pretty).font(mono.clone()).color(theme.text_editor));
                            });
                        });
                    }
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let allow = egui::Button::new(RichText::new("Allow").color(theme.text).strong()).fill(theme.accent);
                if ui.add(allow).on_hover_text("Allow once (Enter when the card is focused)").clicked() {
                    decision = Some("allow");
                }
                if ui
                    .button("Always")
                    .on_hover_text(format!("Allow this and later requests from {} for this session", req.source))
                    .clicked()
                {
                    decision = Some("allowAlways");
                }
                if ui.button("Deny").on_hover_text("Deny (Esc when the card is focused)").clicked() {
                    decision = Some("deny");
                }
                if focused {
                    ui.label(RichText::new("↩ allow · esc deny").small().color(theme.text_muted));
                }
            });
        });
    let rect = inner.response.rect;
    ui.ctx().data_mut(|d| d.insert_temp(id, rect));
    // Labels are selectable and swallow clicks, so any primary click inside the card focuses it.
    let clicked_inside =
        ui.input(|i| i.pointer.primary_clicked() && i.pointer.interact_pos().is_some_and(|p| rect.contains(p)));
    if clicked_inside && decision.is_none() {
        ui.memory_mut(|m| m.request_focus(id));
    }
    decision
}
