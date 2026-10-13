//! The Local History window (revisions on the left, the selected revision's diff on the right,
//! Revert) and the Put Label dialog.

use egui::{Align2, Id, Key, RichText, ScrollArea, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::local_history::{self, EntryKind, ViewMode};

use crate::diff_view::{self, DiffOptions};
use crate::{UiState, run_command};

/// Whether the history window or the label dialog is open (they read the keyboard themselves).
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.local_history.view.is_some() || s.local_history.label_dialog.is_some()
}

pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    history_window(&ctx, s, st);
    label_dialog(&ctx, s, st);
}

fn history_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(view) = s.local_history.view.as_ref() else { return };
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    let screen = ctx.content_rect();
    let title = view.title.clone();
    let project_mode = matches!(view.mode, ViewMode::Project | ViewMode::Recent);
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    let side_by_side_key = Id::new("local-history-sbs");
    let mut side_by_side = ctx.data(|d| d.get_temp::<bool>(side_by_side_key)).unwrap_or(true);
    crate::windows::resizable(
        egui::Window::new(title).default_size(vec2(
            (screen.width() * 0.75).clamp(560.0, 1300.0),
            (screen.height() * 0.7).clamp(320.0, 900.0),
        )),
        st,
        "local-history",
    )
    .id(Id::new("local-history-window"))
    .open(&mut open)
    .collapsible(false)
    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
    .show(ctx, |ui| {
        if view.revisions.is_empty() {
            ui.label(
                RichText::new(match view.mode {
                    ViewMode::Selection { .. } => "No local changes to the selected lines.",
                    _ => "No local history yet. Snapshots are taken when files are saved or change on disk.",
                })
                .color(theme.text_muted),
            );
            return;
        }
        let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
        if let Some(sel) = view.selected {
            if down && sel + 1 < view.revisions.len() {
                cmds.push(("localHistory.select", json!({ "index": sel + 1 })));
            } else if up && sel > 0 {
                cmds.push(("localHistory.select", json!({ "index": sel - 1 })));
            }
        }
        let height = ui.available_height().max(200.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(260.0);
                ScrollArea::vertical().id_salt("local-history-list").max_height(height).show(ui, |ui| {
                    for (i, r) in view.revisions.iter().enumerate() {
                        let mut text = local_history::describe(r);
                        if project_mode && let Some(p) = &r.path {
                            let shown = match &root {
                                Some(root) => p.strip_prefix(root).unwrap_or(p).display().to_string(),
                                None => p.display().to_string(),
                            };
                            text = format!("{text}\n{shown}");
                        }
                        let mut rt = RichText::new(text);
                        if r.kind == EntryKind::Label {
                            rt = rt.strong().color(theme.accent);
                        }
                        let selected = view.selected == Some(i);
                        let resp = ui.add(egui::Button::selectable(selected, rt).frame_when_inactive(false));
                        if selected {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            cmds.push(("localHistory.select", json!({ "index": i })));
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                let Some(r) = view.selected.and_then(|i| view.revisions.get(i)) else {
                    ui.label(RichText::new("Select a revision.").color(theme.text_muted));
                    return;
                };
                if let Some(e) = &view.error {
                    ui.label(RichText::new(e).color(theme.error));
                }
                if r.kind == EntryKind::Label {
                    ui.label(RichText::new(r.label.clone().unwrap_or_default()).strong());
                    ui.label(RichText::new("A label marks a point in the project's history.").color(theme.text_muted));
                    return;
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  →  {}", view.old_title, view.new_title)).color(theme.text_muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Revert").on_hover_text("Restore the file to this revision").clicked() {
                            cmds.push(("localHistory.revert", json!({ "id": r.id })));
                        }
                        if ui.selectable_label(side_by_side, RichText::new("Side by side").small()).clicked() {
                            side_by_side = !side_by_side;
                        }
                    });
                });
                let font = egui::FontId::monospace((s.ws.settings.editor_font_size - 1.0).max(9.0));
                let opts = DiffOptions { side_by_side, context: 3, max_height: (height - 30.0).max(120.0) };
                diff_view::show(ui, &theme, &font, ("local-history", r.id), &view.old_text, &view.new_text, &opts);
            });
        });
    });
    ctx.data_mut(|d| d.insert_temp(side_by_side_key, side_by_side));
    if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
        cmds.push(("localHistory.close", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn label_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let focus_key = Id::new("local-history-label-focus");
    let Some(mut text) = s.local_history.label_dialog.clone() else {
        ctx.data_mut(|d| d.remove::<bool>(focus_key));
        return;
    };
    let mut submit = false;
    let mut close = false;
    let mut open = true;
    egui::Window::new("Put Label")
        .id(Id::new("local-history-label"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ctx, |ui| {
            ui.label("Label name:");
            let edit = ui.add(egui::TextEdit::singleline(&mut text).desired_width(320.0));
            if ctx.data(|d| d.get_temp::<bool>(focus_key).is_none()) {
                ctx.data_mut(|d| d.insert_temp(focus_key, true));
                edit.request_focus();
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                submit = true;
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!text.trim().is_empty(), egui::Button::new("OK")).clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    if submit && !text.trim().is_empty() {
        run_command(s, st, "file.localHistory.putLabel", &json!({ "label": text }));
        close = true;
    } else {
        s.local_history.label_dialog = Some(text);
    }
    if close || !open {
        ctx.data_mut(|d| d.remove::<bool>(focus_key));
        run_command(s, st, "localHistory.closeLabelDialog", &Value::Null);
    }
}
