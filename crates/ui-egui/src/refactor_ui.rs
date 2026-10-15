//! Refactoring UI (track R): the refactoring preview window, the file refactoring dialogs and
//! the Hierarchy tool window. Thin: state lives in `Session::refactor`, every action is a command.

use std::path::Path;

use egui::{Align, Align2, CornerRadius, FontId, Id, Key, Layout, RichText, ScrollArea, Sense, TextEdit, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::refactor::{Direction, EntryKind, HierarchyKind, RefactorDialog, UsageKind};

use crate::diff_view::{self, DiffOptions};
use crate::theme::Theme;
use crate::{UiState, run_command};

/// View state of the refactoring windows.
#[derive(Default)]
pub struct RefactorUi {
    pub side_by_side: bool,
    /// The dialog whose text field already got focus (focus it once, not every frame).
    focused_dialog: Option<String>,
}

/// The preview window and dialogs (drawn every frame).
pub fn overlays(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    preview_window(&ctx, s, st);
    dialog(&ctx, s, st);
}

fn buttons(ui: &mut Ui, ok_label: &str, ok_enabled: bool, danger: bool, theme: &Theme) -> (bool, bool) {
    let mut ok = false;
    let mut cancel = false;
    ui.add_space(6.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
        let fill = if danger { theme.error } else { theme.accent };
        if ui.add_enabled(ok_enabled, egui::Button::new(RichText::new(ok_label).strong()).fill(fill)).clicked() {
            ok = true;
        }
        if ui.button("Cancel").clicked() {
            cancel = true;
        }
    });
    (ok, cancel)
}

fn rel(s: &Session, path: &Path) -> String {
    match &s.ws.project {
        Some(p) => p.relative(path).into_owned(),
        None => path.display().to_string(),
    }
}

// ---------------------------------------------------------------- preview

fn preview_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(preview) = s.refactor.preview.clone() else { return };
    let theme = st.theme.clone();
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    let mut open = true;
    let mut action: Option<(&'static str, Value)> = None;
    let screen = ctx.content_rect();
    crate::windows::resizable(
        egui::Window::new(format!("Refactoring Preview: {}", preview.title)).default_size(vec2(
            (screen.width() * 0.75).clamp(520.0, 1200.0),
            (screen.height() * 0.7).clamp(320.0, 900.0),
        )),
        st,
        "refactor-preview",
    )
    .id(Id::new("refactor-preview"))
    .open(&mut open)
    .collapsible(false)
    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
    .show(ctx, |ui| {
        let included = preview.entries.iter().filter(|e| e.include).count();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} change(s) in {} file(s)", preview.entries.len(), preview.files()))
                    .color(theme.text_muted),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.selectable_label(st.refactor.side_by_side, RichText::new("Side by side").small()).clicked() {
                    st.refactor.side_by_side = !st.refactor.side_by_side;
                }
            });
        });
        ui.separator();
        // Buttons in a bottom panel, so the body fills exactly the rest (no growth per frame).
        egui::Panel::bottom("refactor-preview-buttons").show_separator_line(false).show(ui, |ui| {
            let (ok, cancel) = buttons(ui, &format!("Apply ({included})"), included > 0, false, &theme);
            if ok {
                action = Some(("refactor.preview.apply", Value::Null));
            }
            if cancel || ui.input(|i| i.key_pressed(Key::Escape)) {
                action = Some(("refactor.preview.cancel", Value::Null));
            }
        });
        let body_h = ui.available_height().max(120.0);
        ui.horizontal_top(|ui| {
            let list_w = (ui.available_width() * 0.3).clamp(200.0, 380.0);
            ui.allocate_ui_with_layout(vec2(list_w, body_h), Layout::top_down(Align::Min), |ui| {
                ScrollArea::vertical().id_salt("refactor-preview-list").max_height(body_h).show(ui, |ui| {
                    for (i, e) in preview.entries.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let mut inc = e.include;
                            let label = e.label(root.as_deref());
                            let resp = ui.add(egui::Checkbox::without_text(&mut inc));
                            let info = format!("Include {label}");
                            resp.widget_info(|| {
                                egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, inc, &info)
                            });
                            if resp.changed() {
                                action = Some(("refactor.preview.include", json!({ "index": i, "include": inc })));
                            }
                            let color = match e.kind {
                                EntryKind::Delete { .. } => theme.vcs_deleted,
                                EntryKind::Create { .. } => theme.vcs_added,
                                _ => theme.text,
                            };
                            let r = ui.selectable_label(i == preview.selected, RichText::new(label).color(color));
                            if r.clicked() {
                                action = Some(("refactor.preview.select", json!({ "index": i })));
                            }
                            if matches!(e.kind, EntryKind::Edit { .. }) {
                                let stats =
                                    diff_view::stats(ui, ("refactor-preview", i, &preview.title), &e.before, &e.after);
                                diff_view::stats_label(ui, &theme, &stats);
                            }
                        });
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                let Some(e) = preview.entries.get(preview.selected) else { return };
                ui.label(RichText::new(e.label(root.as_deref())).strong());
                match &e.kind {
                    EntryKind::Rename { to, .. } if e.before == e.after => {
                        ui.label(RichText::new(format!("Moved to {}", rel(s, to))).color(theme.text_muted));
                    }
                    EntryKind::Delete { .. } if e.before.is_empty() => {
                        ui.label(
                            RichText::new("The directory and everything in it will be deleted.").color(theme.warning),
                        );
                    }
                    _ => {
                        let font = FontId::monospace(12.0);
                        let opts = DiffOptions {
                            side_by_side: st.refactor.side_by_side,
                            context: 3,
                            max_height: (body_h - 24.0).max(60.0),
                        };
                        diff_view::show(
                            ui,
                            &theme,
                            &font,
                            ("refactor-preview-diff", preview.selected, e.path()),
                            &e.before,
                            &e.after,
                            &opts,
                        );
                    }
                }
            });
        });
    });
    if !open {
        action = Some(("refactor.preview.cancel", Value::Null));
    }
    if let Some((id, params)) = action {
        run_command(s, st, id, &params);
        if id == "refactor.preview.apply" {
            st.focus_editor = true;
        }
    }
}

// ---------------------------------------------------------------- dialogs

fn dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mut d) = s.refactor.dialog.clone() else {
        st.refactor.focused_dialog = None;
        return;
    };
    let theme = st.theme.clone();
    let kind = match &d {
        RefactorDialog::MoveFile { .. } => "move",
        RefactorDialog::CopyFile { .. } => "copy",
        RefactorDialog::RenameFile { .. } => "rename",
        RefactorDialog::SafeDelete { .. } => "delete",
    };
    let key = format!("{kind}:{}", d.path().display());
    let first_frame = st.refactor.focused_dialog.as_deref() != Some(key.as_str());
    let name = d.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let title = match &d {
        RefactorDialog::MoveFile { .. } => format!("Move {name}"),
        RefactorDialog::CopyFile { .. } => format!("Copy {name}"),
        RefactorDialog::RenameFile { .. } => format!("Rename {name}"),
        RefactorDialog::SafeDelete { .. } => "Safe Delete".to_string(),
    };
    let mut open = true;
    let mut action: Option<(&'static str, Value)> = None;
    let mut close = false;
    let field_w = 420.0;
    crate::windows::resizable(egui::Window::new(title).default_width(520.0), st, "refactor-dialog")
        .id(Id::new("refactor-dialog"))
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ctx, |ui| {
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            let path = d.path().to_path_buf();
            match &mut d {
                RefactorDialog::MoveFile { target, .. } => {
                    ui.label(format!("Move {} to directory:", rel(s, &path)));
                    let r = ui.add(TextEdit::singleline(target).desired_width(field_w));
                    if first_frame {
                        r.request_focus();
                    }
                    let (ok, cancel) = buttons(ui, "Refactor", !target.trim().is_empty(), false, &theme);
                    if ok || (enter && !target.trim().is_empty()) {
                        action = Some(("refactor.moveFile", json!({ "path": path, "to": target.trim() })));
                    }
                    close |= cancel;
                }
                RefactorDialog::CopyFile { target, name, .. } => {
                    egui::Grid::new("refactor-copy").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("New name");
                        let r = ui.add(TextEdit::singleline(name).desired_width(field_w));
                        if first_frame {
                            r.request_focus();
                        }
                        ui.end_row();
                        ui.label("To directory");
                        ui.add(TextEdit::singleline(target).desired_width(field_w));
                        ui.end_row();
                    });
                    let valid = !name.trim().is_empty() && !target.trim().is_empty();
                    let (ok, cancel) = buttons(ui, "Copy", valid, false, &theme);
                    if ok || (enter && valid) {
                        let to = Path::new(target.trim()).join(name.trim());
                        action = Some(("refactor.copyFile", json!({ "path": path, "to": to })));
                    }
                    close |= cancel;
                }
                RefactorDialog::RenameFile { name, .. } => {
                    ui.label(format!("Rename {} to:", rel(s, &path)));
                    let r = ui.add(TextEdit::singleline(name).desired_width(field_w));
                    if first_frame {
                        r.request_focus();
                    }
                    if path.extension().is_some_and(|e| e == "rs") {
                        ui.label(
                            RichText::new("`mod` declarations are updated when rust-analyzer is running.")
                                .small()
                                .color(theme.text_muted),
                        );
                    }
                    let valid = !name.trim().is_empty();
                    let (ok, cancel) = buttons(ui, "Refactor", valid, false, &theme);
                    if ok || (enter && valid) {
                        action = Some(("refactor.renameFile", json!({ "path": path, "newName": name.trim() })));
                    }
                    close |= cancel;
                }
                RefactorDialog::SafeDelete { usages, .. } => {
                    let what = if path.is_dir() { "directory" } else { "file" };
                    ui.label(format!("Delete {what} {}?", rel(s, &path)));
                    let ready = match usages {
                        None => {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(12.0));
                                ui.label(RichText::new("Searching for usages…").color(theme.text_muted));
                            });
                            false
                        }
                        Some(list) => {
                            let decls = list.iter().filter(|u| u.kind == UsageKind::ModDeclaration).count();
                            let refs = list.len() - decls;
                            if decls > 0 {
                                ui.label(
                                    RichText::new(format!("{decls} `mod` declaration(s) will be removed."))
                                        .color(theme.text_muted),
                                );
                            }
                            if refs > 0 {
                                ui.label(
                                    RichText::new(format!("{refs} usage(s) found; they will no longer compile:"))
                                        .color(theme.warning),
                                );
                            } else {
                                ui.label(RichText::new("No usages found.").color(theme.text_muted));
                            }
                            if !list.is_empty() {
                                ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                    for u in list.iter() {
                                        let label = format!("{}:{}  {}", rel(s, &u.path), u.line + 1, u.text);
                                        let color =
                                            if u.kind == UsageKind::Reference { theme.text } else { theme.text_muted };
                                        if ui
                                            .add(
                                                egui::Label::new(RichText::new(label).monospace().color(color))
                                                    .sense(Sense::click()),
                                            )
                                            .double_clicked()
                                        {
                                            action = Some((
                                                "navigate.openLocation",
                                                json!({ "path": u.path, "line": u.line, "column": u.column }),
                                            ));
                                        }
                                    }
                                });
                            }
                            true
                        }
                    };
                    let refs = usages.as_ref().is_some_and(|l| l.iter().any(|u| u.kind == UsageKind::Reference));
                    let label = if refs { "Delete Anyway" } else { "Delete" };
                    let (ok, cancel) = buttons(ui, label, ready, true, &theme);
                    if ok {
                        action = Some(("refactor.safeDelete.confirm", json!({ "path": path })));
                    }
                    close |= cancel;
                }
            }
        });
    st.refactor.focused_dialog = Some(key);
    if s.refactor.dialog.as_ref().is_some_and(|cur| cur.path() == d.path()) {
        // Keep the edited fields (the engine owns the dialog; usages may have arrived meanwhile).
        match (&mut s.refactor.dialog, &d) {
            (Some(RefactorDialog::MoveFile { target, .. }), RefactorDialog::MoveFile { target: t, .. }) => {
                target.clone_from(t);
            }
            (
                Some(RefactorDialog::CopyFile { target, name, .. }),
                RefactorDialog::CopyFile { target: t, name: n, .. },
            ) => {
                target.clone_from(t);
                name.clone_from(n);
            }
            (Some(RefactorDialog::RenameFile { name, .. }), RefactorDialog::RenameFile { name: n, .. }) => {
                name.clone_from(n);
            }
            _ => {}
        }
    }
    if !open || close {
        run_command(s, st, "refactor.dialog.cancel", &Value::Null);
        return;
    }
    if let Some((id, params)) = action {
        run_command(s, st, id, &params);
    }
}

// ---------------------------------------------------------------- Hierarchy tool window

/// The Hierarchy tool window (type, call and method hierarchies).
pub fn show_hierarchy(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let Some(h) = s.refactor.hierarchy.clone() else {
        ui.label(
            RichText::new("Use Navigate › Call Hierarchy (⌃⌥H) or Type Hierarchy (⌃H) on a symbol.")
                .color(theme.text_muted),
        );
        return;
    };
    let mut action: Option<(&'static str, Value)> = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&h.title).strong());
        let dirs: &[Direction] = match h.kind {
            HierarchyKind::Call => &[Direction::Callers, Direction::Callees],
            HierarchyKind::Type | HierarchyKind::Method if h.server_types => {
                &[Direction::Subtypes, Direction::Supertypes]
            }
            _ => &[],
        };
        for d in dirs {
            if ui.selectable_label(h.direction == *d, d.label()).clicked() && h.direction != *d {
                let name = match d {
                    Direction::Callers => "callers",
                    Direction::Callees => "callees",
                    Direction::Subtypes => "subtypes",
                    Direction::Supertypes => "supertypes",
                };
                action = Some(("hierarchy.setDirection", json!({ "direction": name })));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("✖").on_hover_text("Close").clicked() {
                action = Some(("hierarchy.close", Value::Null));
            }
        });
    });
    ui.separator();
    if h.pending && h.nodes.is_empty() {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("Loading…").color(theme.text_muted));
        });
    }
    if let Some(m) = &h.message {
        ui.label(RichText::new(m).color(theme.text_muted));
    }
    let rows = h.visible_rows();
    let row_h = 20.0;
    ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
        for (i, depth) in rows.get(range).unwrap_or(&[]) {
            let Some(n) = h.nodes.get(*i) else { continue };
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(260.0), row_h), Sense::click());
            if h.selected == Some(*i) {
                ui.painter().rect_filled(rect, CornerRadius::same(3), theme.list_selection);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(3), theme.hover);
            }
            let x0 = rect.left() + 4.0 + *depth as f32 * 16.0;
            let cy = rect.center().y;
            let leaf = n.children.as_ref().is_some_and(Vec::is_empty);
            let arrow = if n.loading {
                "…"
            } else if leaf {
                ""
            } else if n.expanded {
                "⏷"
            } else {
                "⏵"
            };
            ui.painter().text(
                egui::pos2(x0 + 5.0, cy),
                Align2::CENTER_CENTER,
                arrow,
                FontId::proportional(10.0),
                theme.text_muted,
            );
            let mut job = egui::text::LayoutJob::default();
            let fmt =
                |color, size| egui::TextFormat { font_id: FontId::proportional(size), color, ..Default::default() };
            job.append(&format!("{} ", n.kind), 0.0, fmt(theme.text_muted, 11.0));
            job.append(&n.name, 0.0, fmt(theme.text, 13.0));
            if let Some(d) = &n.detail {
                job.append(&format!("  {d}"), 0.0, fmt(theme.text_muted, 12.0));
            }
            let file = n.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let sites = if n.sites > 1 { format!("  ({} usages)", n.sites) } else { String::new() };
            job.append(&format!("  {file}:{}{sites}", n.line + 1), 0.0, fmt(theme.text_muted, 11.0));
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            ui.painter().with_clip_rect(rect).galley(
                egui::pos2(x0 + 16.0, cy - galley.size().y / 2.0),
                galley,
                theme.text,
            );
            if resp.double_clicked() {
                action = Some(("hierarchy.open", json!({ "node": i })));
            } else if resp.clicked() {
                let on_arrow = resp.interact_pointer_pos().is_some_and(|p| p.x < x0 + 14.0);
                if on_arrow && !leaf {
                    action = Some(("hierarchy.toggle", json!({ "node": i })));
                } else if let Some(hm) = &mut s.refactor.hierarchy {
                    hm.selected = Some(*i);
                }
            }
        }
    });
    if let Some((id, params)) = action {
        run_command(s, st, id, &params);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use codecraft_engine::Context;

    use crate::keymap::{Shortcut, bindings};

    fn bound(key: &str, context: Context) -> Option<&'static str> {
        let sc = Shortcut::parse(key).unwrap();
        bindings().into_iter().find(|b| b.shortcut == sc && b.context == context).map(|b| b.id)
    }

    #[test]
    fn refactoring_shortcuts_do_not_steal_editor_keys() {
        // Cmd+Backspace deletes the line in the editor; elsewhere it is Safe Delete.
        assert_eq!(bound("Cmd+Backspace", Context::Editor), Some("editor.deleteLine"));
        assert_eq!(bound("Cmd+Backspace", Context::Global), Some("refactor.safeDelete"));
        assert_eq!(bound("F6", Context::Global), Some("refactor.moveFile"));
        assert_eq!(bound("F5", Context::Global), Some("refactor.copyFile"));
        // Alt+Cmd+N is Inline in the editor and New Branch elsewhere.
        assert_eq!(bound("Alt+Cmd+N", Context::Editor), Some("refactor.inline"));
        for (key, id) in [
            ("Ctrl+T", "refactor.refactorThis"),
            ("Alt+Cmd+V", "refactor.extract.variable"),
            ("Alt+Cmd+M", "refactor.extract.function"),
            ("Alt+Cmd+C", "refactor.extract.constant"),
            ("Cmd+F6", "refactor.changeSignature"),
            ("Cmd+N", "code.generate"),
            ("Ctrl+I", "code.implementMethods"),
            ("Ctrl+O", "code.overrideMethods"),
            ("Ctrl+Alt+H", "navigate.callHierarchy"),
            ("Ctrl+H", "navigate.typeHierarchy"),
            ("Cmd+U", "navigate.superMethod"),
        ] {
            assert_eq!(bound(key, Context::Editor), Some(id), "{key}");
        }
    }
}
