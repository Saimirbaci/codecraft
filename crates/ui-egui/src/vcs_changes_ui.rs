//! Changelists and the shelf in the Commit tool window: the changelist header menus, the Shelf
//! tab, and the Shelve Changes / New and Edit Changelist dialogs. Thin: every action is a
//! `vcs.changelist.*` / `vcs.shelve` / `vcs.unshelve` / `vcs.shelf.*` command; the state lives in
//! `Session::vcs` (`changelists`, `shelves`, `shelf_selected`, `shelf_patch`).

use std::collections::BTreeSet;

use egui::{Align, Layout, RichText, TextEdit, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::Dialog;
use codecraft_engine::vcs_git::shelf::ShelvedKind;

use crate::UiState;
use crate::theme::Theme;

type Action = Option<(&'static str, Value)>;

/// "3 min ago" / "2 h ago" / "4 d ago".
fn ago(ms: u64) -> String {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(ms);
    let secs = now.saturating_sub(ms) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// The context menu of a changelist header.
pub fn list_menu(ui: &mut Ui, s: &Session, name: &str, files: &[String], cmd: &mut Action) {
    let active = s.vcs.changelists.active == name;
    if ui.add_enabled(!active, egui::Button::new("Set Active Changelist")).clicked() {
        *cmd = Some(("vcs.changelist.setActive", json!({ "name": name })));
        ui.close();
    }
    if ui.button("Edit Changelist…").clicked() {
        *cmd = Some(("vcs.changelist.edit", json!({ "name": name })));
        ui.close();
    }
    if ui.button("New Changelist…").clicked() {
        *cmd = Some(("vcs.changelist.new", Value::Null));
        ui.close();
    }
    if ui
        .add_enabled(!active, egui::Button::new("Delete Changelist"))
        .on_disabled_hover_text("The active changelist can't be deleted")
        .clicked()
    {
        *cmd = Some(("vcs.changelist.delete", json!({ "name": name })));
        ui.close();
    }
    ui.separator();
    if ui.add_enabled(!files.is_empty(), egui::Button::new("Include Only This Changelist")).clicked() {
        *cmd = Some(("vcs.changelist.include", json!({ "name": name })));
        ui.close();
    }
    if ui.add_enabled(!files.is_empty() && s.vcs.op.is_none(), egui::Button::new("Commit Changelist…")).clicked() {
        *cmd = Some(("vcs.changelist.commit", json!({ "name": name })));
        ui.close();
    }
    if ui.add_enabled(!files.is_empty(), egui::Button::new("Shelve Changes…")).clicked() {
        *cmd = Some(("git.uncommittedChanges.shelveChanges", json!({ "changelist": name })));
        ui.close();
    }
}

/// "Move to Another Changelist" in a changed file's context menu.
pub fn move_menu(ui: &mut Ui, s: &Session, path: &str, cmd: &mut Action) {
    ui.menu_button("Move to Another Changelist", |ui| {
        let current = s.vcs.changelists.list_of(path).unwrap_or(s.vcs.changelists.active.as_str()).to_string();
        for l in &s.vcs.changelists.lists {
            if l.name == current {
                continue;
            }
            if ui.button(&l.name).clicked() {
                *cmd = Some(("vcs.changelist.move", json!({ "paths": [path], "to": l.name })));
                ui.close();
            }
        }
        if ui.button("New Changelist…").clicked() {
            *cmd = Some(("vcs.changelist.new", json!({ "files": [path] })));
            ui.close();
        }
    });
}

/// The Commit tool window's Shelf tab.
pub fn shelf_view(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme, cmd: &mut Action) {
    ui.horizontal(|ui| {
        if ui.add_enabled(s.vcs.op.is_none(), egui::Button::new("Shelve Changes…")).clicked() {
            *cmd = Some(("git.uncommittedChanges.shelveChanges", Value::Null));
        }
        if ui.button("Refresh").clicked() {
            *cmd = Some(("vcs.shelves", json!({ "refresh": true })));
        }
        if s.vcs.shelves_loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    if s.vcs.shelves.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("No shelved changes").color(theme.text_muted));
        return;
    }
    let busy = s.vcs.op.is_some();
    let patch = s.vcs.shelf_patch.clone();
    if let Some((_, path, text)) = &patch {
        egui::Panel::bottom("vcs-shelf-patch")
            .resizable(true)
            .default_size(ui.available_height() * 0.45)
            .size_range(60.0..=1200.0)
            .show(ui, |ui| {
                ui.label(RichText::new(path).strong().size(12.0));
                match text {
                    Ok(t) => {
                        crate::pull_requests_ui::patch_view(ui, theme, "vcs-shelf-patch-text", t, ui.available_height())
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(theme.error).small());
                    }
                }
            });
    }
    egui::ScrollArea::vertical().id_salt("vcs-shelves").auto_shrink([false, false]).show(ui, |ui| {
        for sh in s.vcs.shelves.clone() {
            let selected_shelf = s.vcs.shelf_selected.as_ref().is_some_and(|(id, _)| *id == sh.id);
            let header = egui::CollapsingHeader::new(RichText::new(&sh.name).strong())
                .id_salt(("vcs-shelf", &sh.id))
                .default_open(selected_shelf);
            let resp = header.show(ui, |ui| {
                for f in &sh.files {
                    ui.horizontal(|ui| {
                        let sel = s
                            .vcs
                            .shelf_selected
                            .as_ref()
                            .is_some_and(|(id, p)| *id == sh.id && p.as_deref() == Some(f.path.as_str()));
                        let color = match f.kind {
                            ShelvedKind::Modified => theme.vcs_modified,
                            ShelvedKind::Added => theme.vcs_added,
                            ShelvedKind::Unversioned => theme.vcs_untracked,
                        };
                        if ui
                            .add(
                                egui::Button::selectable(sel, RichText::new(&f.path).color(color))
                                    .frame_when_inactive(false),
                            )
                            .clicked()
                        {
                            *cmd = Some(("vcs.shelf.select", json!({ "id": sh.id, "path": f.path })));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(!busy, egui::Button::new("Delete").small())
                                .on_hover_text("Delete this file from the shelf")
                                .clicked()
                            {
                                *cmd = Some(("vcs.shelf.delete", json!({ "id": sh.id, "files": [f.path] })));
                            }
                            let resp = ui.add_enabled(!busy, egui::Button::new("Unshelve").small());
                            let label = format!("Unshelve {}", f.path);
                            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, !busy, &label));
                            if resp.on_hover_text("Unshelve only this file").clicked() {
                                *cmd = Some(("vcs.unshelve", json!({ "id": sh.id, "files": [f.path] })));
                            }
                        });
                    });
                }
            });
            resp.header_response.context_menu(|ui| {
                let rename_id = egui::Id::new(("vcs-shelf-rename", &sh.id));
                let mut name = ui.data(|d| d.get_temp::<String>(rename_id)).unwrap_or_else(|| sh.name.clone());
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut name).desired_width(180.0));
                    if ui.add_enabled(!busy && !name.trim().is_empty(), egui::Button::new("Rename")).clicked() {
                        *cmd = Some(("vcs.shelf.rename", json!({ "id": sh.id, "name": name.trim() })));
                        ui.close();
                    }
                });
                ui.data_mut(|d| d.insert_temp(rename_id, name));
            });
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(
                    RichText::new(format!(
                        "{} file{} · {}",
                        sh.files.len(),
                        if sh.files.len() == 1 { "" } else { "s" },
                        ago(sh.created_ms)
                    ))
                    .small()
                    .color(theme.text_muted),
                );
                let resp = ui.add_enabled(!busy, egui::Button::new("Unshelve").small());
                let label = format!("Unshelve {}", sh.name);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, !busy, &label));
                if resp.on_hover_text("Apply every file and remove it from the shelf").clicked() {
                    *cmd = Some(("vcs.unshelve", json!({ "id": sh.id })));
                }
                if ui.add_enabled(!busy, egui::Button::new("Unshelve and Keep").small()).clicked() {
                    *cmd = Some(("vcs.unshelve", json!({ "id": sh.id, "keep": true })));
                }
                if ui.add_enabled(!busy, egui::Button::new("Delete").small()).clicked() {
                    *cmd = Some(("vcs.shelf.delete", json!({ "id": sh.id })));
                }
            });
            ui.add_space(4.0);
        }
    });
    let _ = st;
}

/// The Shelve Changes and changelist dialogs; returns (action, close).
pub fn dialogs(ui: &mut Ui, s: &Session, theme: &Theme, d: &mut Dialog, busy: bool, enter: bool) -> (Action, bool) {
    let mut action = None;
    let mut close = false;
    let error = |ui: &mut Ui| {
        if let Some(e) = &s.vcs.dialog_error {
            ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
        }
    };
    let ok_cancel = |ui: &mut Ui, label: &str, enabled: bool| -> (bool, bool) {
        let mut r = (false, false);
        ui.add_space(6.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
            let b = egui::Button::new(RichText::new(label).strong()).fill(theme.accent);
            r.0 = ui.add_enabled(enabled, b).clicked();
            r.1 = ui.button("Cancel").clicked();
        });
        r
    };
    match d {
        Dialog::Shelve { name, files, keep } => {
            let resp = ui.add(TextEdit::singleline(name).hint_text("Shelved changes").desired_width(f32::INFINITY));
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Shelf Name"));
            ui.label(RichText::new("Changes to shelve").small().color(theme.text_muted));
            let mut chosen: BTreeSet<String> = files.iter().cloned().collect();
            let candidates: Vec<String> = s
                .vcs
                .changes()
                .into_iter()
                .chain(s.vcs.unversioned())
                .filter(|f| f.conflict.is_none())
                .map(|f| f.path.clone())
                .collect();
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for p in &candidates {
                    let mut on = chosen.contains(p);
                    let resp = ui.checkbox(&mut on, p.as_str());
                    if resp.changed() {
                        if on {
                            chosen.insert(p.clone());
                        } else {
                            chosen.remove(p);
                        }
                    }
                }
            });
            *files = candidates.iter().filter(|p| chosen.contains(*p)).cloned().collect();
            ui.checkbox(keep, "Keep the changes in the working tree");
            error(ui);
            let can = !busy && !files.is_empty();
            let (ok, cancel) = ok_cancel(ui, "Shelve Changes", can);
            if ok || (enter && can) {
                action = Some(("vcs.shelve", json!({ "name": name.trim(), "files": files, "keep": *keep })));
            }
            close |= cancel;
        }
        Dialog::ChangeList { original, name, comment, active, files } => {
            egui::Grid::new("vcs-changelist").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Name");
                let resp = ui.add(TextEdit::singleline(name).desired_width(320.0));
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Changelist Name"));
                ui.end_row();
                ui.label("Comment");
                ui.add(TextEdit::multiline(comment).desired_rows(3).desired_width(320.0));
                ui.end_row();
            });
            ui.checkbox(active, "Set active");
            if !files.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "Moves {} file{} into it",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(theme.text_muted),
                );
            }
            error(ui);
            let can = !name.trim().is_empty();
            let (ok, cancel) = ok_cancel(ui, if original.is_some() { "OK" } else { "Create" }, can);
            if ok || (enter && can) {
                action = Some(match original {
                    None => (
                        "vcs.changelist.new",
                        json!({ "name": name.trim(), "comment": comment, "active": *active, "files": files }),
                    ),
                    Some(o) => (
                        "vcs.changelist.edit",
                        json!({ "name": o, "newName": name.trim(), "comment": comment, "active": *active }),
                    ),
                });
            }
            close |= cancel;
        }
        _ => {}
    }
    (action, close)
}
