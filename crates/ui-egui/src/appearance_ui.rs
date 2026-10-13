//! View › Appearance in the shell (M10 track G): the UI zoom, the window's full-screen state,
//! Compact Mode spacing, the Zoom IDE popup and the Find Usages Settings dialog. Every change
//! goes through engine commands.

use egui::{Align2, Id, Key, RichText};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::appearance;
use codecraft_workspace::UsageScope;

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// Per frame: the UI zoom follows the settings and full-screen changes made outside CodeCraft
/// (the window's green button) reach the session.
pub(crate) fn sync(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    // Zoom IDE owns the zoom: egui's own Cmd+plus/minus would fight it (and Cmd+-/= fold).
    if ctx.options(|o| o.zoom_with_keyboard) {
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
    }
    let zoom = appearance::zoom_of(&s.ws.settings);
    if (ctx.zoom_factor() - zoom).abs() > 0.001 {
        ctx.set_zoom_factor(zoom);
    }
    if let Some(full) = ctx.input(|i| i.viewport().fullscreen)
        && st.viewport_full_screen != Some(full)
    {
        st.viewport_full_screen = Some(full);
        s.appearance.full_screen = full;
    }
}

/// The Find Usages Settings dialog is modal: the editor must not get the keys.
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.usages.settings_dialog.is_some()
}

/// Compact Mode: tighter spacing than the theme's (applied after it).
pub(crate) fn compact_spacing(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(4.0, 2.0);
    style.spacing.button_padding = egui::vec2(4.0, 1.0);
    style.spacing.menu_margin = egui::Margin::same(2);
    style.spacing.interact_size.y = (style.spacing.interact_size.y - 2.0).max(14.0);
    ctx.set_global_style(style);
}

pub(crate) fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    let mut cmds: Cmds = Vec::new();
    zoom_popup(&ctx, s, st, &mut cmds);
    find_usages_settings(&ctx, s, st, &mut cmds);
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// View › Appearance › Zoom IDE…: −, the current zoom, +, Reset.
fn zoom_popup(ctx: &egui::Context, s: &Session, st: &UiState, cmds: &mut Cmds) {
    if !s.appearance.zoom_popup {
        return;
    }
    let mut open = true;
    egui::Window::new("Zoom IDE")
        .id(Id::new("zoom-ide-popup"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 60.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button(RichText::new(" − ").strong()).on_hover_text("Zoom Out IDE").clicked() {
                    cmds.push(("view.appearance.zoomIde.zoomOut", Value::Null));
                }
                let pct = appearance::zoom_percent(&s.ws.settings);
                ui.label(RichText::new(format!("{pct}%")).monospace().size(14.0));
                if ui.button(RichText::new(" + ").strong()).on_hover_text("Zoom In IDE").clicked() {
                    cmds.push(("view.appearance.zoomIde.zoomIn", Value::Null));
                }
                ui.separator();
                if ui.add_enabled(pct != 100, egui::Button::new("Reset")).clicked() {
                    cmds.push(("view.appearance.zoomIde.reset", Value::Null));
                }
            });
            ui.label(RichText::new("Scales the whole window; Esc closes").size(11.0).color(st.theme.text_muted));
        });
    if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
        cmds.push(("view.appearance.zoomIde.close", Value::Null));
    }
}

/// Edit › Find Usages › Find Usages Settings…: scope and test code; Find runs Find Usages.
fn find_usages_settings(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(mut draft) = s.usages.settings_dialog.clone() else { return };
    let mut open = true;
    let mut cancel = false;
    let mut action: Option<bool> = None;
    egui::Window::new("Find Usages Settings")
        .id(Id::new("find-usages-settings"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        .show(ctx, |ui| {
            ui.label(RichText::new("Scope").strong());
            ui.radio_value(&mut draft.scope, UsageScope::Project, "Project files");
            ui.radio_value(&mut draft.scope, UsageScope::CurrentFile, "Current file");
            ui.add_space(4.0);
            ui.checkbox(&mut draft.include_tests, "Include usages in test code");
            ui.label(
                RichText::new("Test code: tests/ directories and *_test, test_*, *.test, *.spec files")
                    .size(11.0)
                    .color(st.theme.text_muted),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can_find = s.enabled("edit.findUsages.findUsages").is_ok();
                if ui.add_enabled(can_find, egui::Button::new("Find")).clicked() {
                    action = Some(true);
                }
                if ui.button("OK").clicked() {
                    action = Some(false);
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    let scope = match draft.scope {
        UsageScope::Project => "project",
        UsageScope::CurrentFile => "currentFile",
    };
    match action {
        Some(find) => cmds.push((
            "findUsages.settings.apply",
            json!({ "scope": scope, "includeTests": draft.include_tests, "find": find }),
        )),
        None if !open || cancel || ctx.input(|i| i.key_pressed(Key::Escape)) => {
            cmds.push(("findUsages.settings.close", Value::Null));
        }
        // The draft is the dialog's own state.
        None => s.usages.settings_dialog = Some(draft),
    }
}
