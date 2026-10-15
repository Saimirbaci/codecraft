//! The merge editor (Resolve Conflicts › Merge…): yours on the left, the result in the centre
//! (an editable text), theirs on the right, all three scrolling together. Each change has accept
//! arrows (» on the left, « on the right) and an ignore button in the gutters; the toolbar counts
//! conflicts, steps between them and accepts one side for all. Thin: every click is a
//! `vcs.merge.*` command and typing in the result is `vcs.merge.setResult`; the model is
//! `Session::vcs.merge` ([`codecraft_engine::vcs_merge::MergeView`]).

use std::ops::Range;
use std::sync::Arc;

use egui::{Align, Align2, Color32, FontId, Id, Layout, Rect, RichText, Sense, Stroke, TextEdit, Ui, pos2, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs_git::merge3::ChunkKind;
use codecraft_engine::vcs_merge::MergeView;

use crate::theme::Theme;
use crate::{UiState, run_command};

type Action = Option<(&'static str, Value)>;

const GUTTER_W: f32 = 44.0;

/// The background of a change in a side pane.
fn side_fill(theme: &Theme, kind: ChunkKind, resolved: bool) -> Color32 {
    match (kind, resolved) {
        (ChunkKind::Conflict, false) => theme.diff_removed_bg,
        (_, true) => theme.diff_added_bg,
        _ => theme.diff_modified_bg,
    }
}

/// Paint `text` read-only, one row per line, with backgrounds for `ranges` (line ranges).
fn paint_pane(
    ui: &mut Ui,
    theme: &Theme,
    font: &FontId,
    row_h: f32,
    width: f32,
    text: &str,
    ranges: &[(Range<usize>, Color32, bool)],
) -> Rect {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = if text.ends_with('\n') { lines.len().saturating_sub(1) } else { lines.len() }.max(1);
    let (rect, _) = ui.allocate_exact_size(vec2(width, n as f32 * row_h), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.editor_bg);
    for (r, fill, selected) in ranges {
        let top = rect.top() + r.start as f32 * row_h;
        let h = r.end.saturating_sub(r.start) as f32 * row_h;
        let band = Rect::from_min_size(pos2(rect.left(), top), vec2(width, h.max(2.0)));
        painter.rect_filled(band, 0.0, *fill);
        if *selected {
            painter.rect_stroke(band, 0.0, Stroke::new(1.0, theme.accent), egui::StrokeKind::Inside);
        }
    }
    let clip = ui.clip_rect();
    let first = ((clip.top() - rect.top()) / row_h).floor().max(0.0) as usize;
    let last = (((clip.bottom() - rect.top()) / row_h).ceil().max(0.0) as usize).min(n);
    for (i, line) in lines.iter().enumerate().take(last).skip(first) {
        let y = rect.top() + i as f32 * row_h;
        painter.text(pos2(rect.left() + 4.0, y), Align2::LEFT_TOP, *line, font.clone(), theme.text_editor);
    }
    rect
}

/// An accept/ignore button in a gutter with an accessible name. Placed by hand (it doesn't move
/// the layout cursor).
fn gutter_button(ui: &mut Ui, rect: Rect, text: &str, name: String, tip: &str, theme: &Theme) -> bool {
    let resp = ui.interact(rect, ui.id().with(("merge-gutter", &name)), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, theme.list_selection_inactive);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(13.0), theme.accent);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    resp.on_hover_text(tip).clicked()
}

pub fn merge_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(view) = s.vcs.merge.clone() else { return };
    let theme = st.theme.clone();
    let mut open = true;
    let mut action: Action = None;
    let unresolved = view.unresolved();
    let title = format!("Merge Revisions for {}", view.path);
    crate::windows::resizable(egui::Window::new(title).default_size([1180.0, 720.0]), st, "vcs-merge")
        .id(Id::new("vcs-merge"))
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            toolbar(ui, &view, unresolved, &theme, &mut action);
            ui.separator();
            let footer_h = 64.0;
            let rect = ui.available_rect_before_wrap();
            let body = Rect::from_min_size(rect.min, vec2(rect.width(), (rect.height() - footer_h).max(120.0)));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
            panes(&mut child, &view, &theme, &mut action);
            ui.allocate_rect(body, Sense::hover());
            footer(ui, s, unresolved, &theme, &mut action);
        });
    if !open {
        action = Some(("vcs.merge.close", Value::Null));
    }
    if let Some((id, params)) = action {
        run_command(s, st, id, &params);
    }
}

fn toolbar(ui: &mut Ui, view: &MergeView, unresolved: usize, theme: &Theme, action: &mut Action) {
    ui.horizontal(|ui| {
        if ui.button("Apply Non-Conflicting Changes").clicked() {
            *action = Some(("vcs.merge.applyNonConflicting", Value::Null));
        }
        if ui.button("Accept Left").on_hover_text("Resolve every conflict with yours").clicked() {
            *action = Some(("vcs.merge.resolveAll", json!({ "resolution": "ours" })));
        }
        if ui.button("Accept Right").on_hover_text("Resolve every conflict with theirs").clicked() {
            *action = Some(("vcs.merge.resolveAll", json!({ "resolution": "theirs" })));
        }
        ui.separator();
        if ui.button("▲").on_hover_text("Previous conflict").clicked() {
            *action = Some(("vcs.merge.select", json!({ "next": false })));
        }
        if ui.button("▼").on_hover_text("Next conflict").clicked() {
            *action = Some(("vcs.merge.select", json!({ "next": true })));
        }
        let conflicts = view.conflicts();
        let summary = if unresolved == 0 {
            RichText::new(format!(
                "All changes are resolved ({conflicts} conflict{})",
                if conflicts == 1 { "" } else { "s" }
            ))
            .color(theme.success)
        } else {
            RichText::new(format!(
                "{unresolved} of {conflicts} conflict{} unresolved",
                if conflicts == 1 { "" } else { "s" }
            ))
            .color(theme.vcs_conflict)
        };
        ui.label(summary);
        ui.label(
            RichText::new(format!("{} change{}", view.chunks.len(), if view.chunks.len() == 1 { "" } else { "s" }))
                .small()
                .color(theme.text_muted),
        );
    });
}

fn panes(ui: &mut Ui, view: &MergeView, theme: &Theme, action: &mut Action) {
    let font = FontId::monospace(12.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font));
    let pane_w = ((ui.available_width() - 2.0 * GUTTER_W - 16.0) / 3.0).max(140.0);
    // Column titles.
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
    let step = pane_w + GUTTER_W + 8.0;
    for (i, text) in
        [format!("Yours ({})", view.ours_label), "Result".to_string(), format!("Theirs ({})", view.theirs_label)]
            .into_iter()
            .enumerate()
    {
        let r = Rect::from_min_size(pos2(row.left() + i as f32 * step, row.top()), vec2(pane_w, 18.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(r).layout(Layout::left_to_right(Align::Center)), |ui| {
            ui.label(RichText::new(text).strong().small());
        });
    }
    if view.chunks.is_empty() {
        ui.label(RichText::new("The versions don't differ.").color(theme.text_muted));
    }
    let rows = |t: &str| t.split('\n').count();
    let tall = rows(&view.ours).max(rows(&view.theirs)).max(rows(&view.text)) as f32 * row_h;
    let resolved =
        |i: usize| view.resolutions.get(i).copied().flatten().is_some() || view.edited.get(i).copied().unwrap_or(false);
    egui::ScrollArea::both().id_salt("vcs-merge-panes").auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Yours.
            let ours_ranges: Vec<(Range<usize>, Color32, bool)> = view
                .chunks
                .iter()
                .enumerate()
                .filter(|(_, c)| c.kind != ChunkKind::Theirs)
                .map(|(i, c)| (c.ours.clone(), side_fill(theme, c.kind, resolved(i)), view.selected == i))
                .collect();
            let left = paint_pane(ui, theme, &font, row_h, pane_w, &view.ours, &ours_ranges);
            // Left gutter: » accept yours, × ignore.
            let (gutter, _) = ui.allocate_exact_size(vec2(GUTTER_W, tall.max(left.height())), Sense::hover());
            for (i, c) in view.chunks.iter().enumerate() {
                if c.kind == ChunkKind::Theirs || c.kind == ChunkKind::Same {
                    continue;
                }
                let y = gutter.top() + c.ours.start as f32 * row_h;
                let r = Rect::from_min_size(pos2(gutter.left(), y), vec2(20.0, row_h));
                if gutter_button(ui, r, "»", format!("Accept yours for change {}", i + 1), "Accept Left", theme) {
                    *action = Some(("vcs.merge.resolve", json!({ "chunk": i, "resolution": "ours" })));
                }
                let r = Rect::from_min_size(pos2(gutter.left() + 22.0, y), vec2(20.0, row_h));
                if c.kind == ChunkKind::Conflict
                    && gutter_button(
                        ui,
                        r,
                        "×",
                        format!("Ignore change {}", i + 1),
                        "Ignore (keep the base lines)",
                        theme,
                    )
                {
                    *action = Some(("vcs.merge.resolve", json!({ "chunk": i, "resolution": "base" })));
                }
            }
            // Result (editable).
            result_pane(ui, view, theme, &font, row_h, pane_w, action);
            // Right gutter: « accept theirs, × ignore.
            let (gutter, _) = ui.allocate_exact_size(vec2(GUTTER_W, tall.max(left.height())), Sense::hover());
            for (i, c) in view.chunks.iter().enumerate() {
                if c.kind == ChunkKind::Ours || c.kind == ChunkKind::Same {
                    continue;
                }
                let y = gutter.top() + c.theirs.start as f32 * row_h;
                let r = Rect::from_min_size(pos2(gutter.right() - 20.0, y), vec2(20.0, row_h));
                if gutter_button(ui, r, "«", format!("Accept theirs for change {}", i + 1), "Accept Right", theme) {
                    *action = Some(("vcs.merge.resolve", json!({ "chunk": i, "resolution": "theirs" })));
                }
                let r = Rect::from_min_size(pos2(gutter.right() - 42.0, y), vec2(20.0, row_h));
                if c.kind == ChunkKind::Conflict
                    && gutter_button(
                        ui,
                        r,
                        "×",
                        format!("Ignore theirs for change {}", i + 1),
                        "Ignore (keep the base lines)",
                        theme,
                    )
                {
                    *action = Some(("vcs.merge.resolve", json!({ "chunk": i, "resolution": "base" })));
                }
            }
            // Theirs.
            let theirs_ranges: Vec<(Range<usize>, Color32, bool)> = view
                .chunks
                .iter()
                .enumerate()
                .filter(|(_, c)| c.kind != ChunkKind::Ours)
                .map(|(i, c)| (c.theirs.clone(), side_fill(theme, c.kind, resolved(i)), view.selected == i))
                .collect();
            paint_pane(ui, theme, &font, row_h, pane_w, &view.theirs, &theirs_ranges);
        });
    });
}

fn result_pane(
    ui: &mut Ui,
    view: &MergeView,
    theme: &Theme,
    font: &FontId,
    row_h: f32,
    width: f32,
    action: &mut Action,
) {
    let lines = view.text.split('\n').count().max(1);
    let top = ui.cursor().top();
    let left = ui.cursor().left();
    // Change backgrounds behind the text.
    let painter = ui.painter().clone();
    painter.rect_filled(Rect::from_min_size(pos2(left, top), vec2(width, lines as f32 * row_h)), 0.0, theme.editor_bg);
    for (i, c) in view.chunks.iter().enumerate() {
        let Some(r) = view.result_lines(i) else { continue };
        let unresolved =
            view.resolutions.get(i).copied().flatten().is_none() && !view.edited.get(i).copied().unwrap_or(false);
        let fill = if unresolved {
            theme.diff_removed_bg
        } else if c.kind == ChunkKind::Conflict {
            theme.diff_added_bg
        } else {
            theme.diff_modified_bg
        };
        let band = Rect::from_min_size(
            pos2(left, top + r.start as f32 * row_h),
            vec2(width, (r.end.saturating_sub(r.start)) as f32 * row_h).max(vec2(width, 2.0)),
        );
        painter.rect_filled(band, 0.0, fill);
        if view.selected == i {
            painter.rect_stroke(band, 0.0, Stroke::new(1.0, theme.accent), egui::StrokeKind::Inside);
        }
    }
    let mut text = view.text.clone();
    let f = font.clone();
    let color = theme.text_editor;
    let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| -> Arc<egui::Galley> {
        let job = egui::text::LayoutJob::simple(buf.as_str().to_string(), f.clone(), color, f32::INFINITY);
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    };
    let resp = ui.add(
        TextEdit::multiline(&mut text)
            .id(Id::new("vcs-merge-result"))
            .code_editor()
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .desired_rows(lines)
            .desired_width(width)
            .layouter(&mut layouter),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Merge Result"));
    if resp.changed() && text != view.text {
        *action = Some(("vcs.merge.setResult", json!({ "text": text })));
    }
}

fn footer(ui: &mut Ui, s: &Session, unresolved: usize, theme: &Theme, action: &mut Action) {
    ui.separator();
    if let Some(e) = &s.vcs.dialog_error {
        ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
    }
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
        let apply = egui::Button::new(RichText::new("Apply").strong()).fill(theme.accent);
        if ui
            .add_enabled(unresolved == 0 && s.vcs.op.is_none(), apply)
            .on_disabled_hover_text("Resolve every conflict first")
            .clicked()
        {
            *action = Some(("vcs.merge.apply", Value::Null));
        }
        if ui.button("Cancel").clicked() {
            *action = Some(("vcs.merge.close", Value::Null));
        }
        if unresolved > 0
            && ui
                .button("Save with Markers")
                .on_hover_text("Write the file with conflict markers; it stays conflicted")
                .clicked()
        {
            *action = Some(("vcs.merge.apply", json!({ "allowUnresolved": true })));
        }
    });
}
