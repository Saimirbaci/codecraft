//! Settings › Python › Remote Interpreters (M16 track RT): interpreters on an SSH host or in a
//! Docker image/container/Compose service, their packages, and the interpreter a run
//! configuration uses on its target. Everything goes through `python.remoteInterpreters.*`.

use egui::{RichText, Ui};
use serde_json::json;

use codecraft_engine::Session;
use codecraft_engine::run_targets::RunTarget;

use crate::settings_ui::Cmds;
use crate::theme::Theme;

/// The section (the engine holds the lists and the selection).
pub(crate) fn section(ui: &mut Ui, s: &mut Session, theme: &Theme, cmds: &mut Cmds) {
    let muted = |t: &str| RichText::new(t.to_string()).color(theme.text_muted).small();
    ui.label(muted(
        "Interpreters on an SSH host or in a Docker image, container or Compose service. Run, Debug, Test and Profile \
         use the one set on a configuration's target (Run on: in Edit Configurations).",
    ));
    let selected = s.targets.python.selected_list().cloned();
    let label = selected.as_ref().map_or_else(|| "Choose a target…".to_string(), |l| l.label.clone());
    ui.horizontal_wrapped(|ui| {
        ui.label("Target:");
        egui::ComboBox::from_id_salt("python-remote-target").selected_text(label).width(280.0).show_ui(ui, |ui| {
            // Listed only while open (reads the server list and saved targets).
            let rows = codecraft_engine::cpp::debugging::target_rows(s);
            let mut any = false;
            for r in rows.into_iter().filter(|r| !r.target.is_local()) {
                any = true;
                let text = if r.detail.is_empty() { r.name.clone() } else { format!("{}  ·  {}", r.name, r.detail) };
                if ui.button(text).clicked() {
                    cmds.push(("python.remoteInterpreters.detect", json!({ "target": r.target })));
                }
            }
            if !any {
                ui.label(muted("No SSH servers or Docker targets yet: add one in Run › Manage Targets…"));
            }
        });
        let target: Option<RunTarget> = selected.as_ref().and_then(|l| l.target.clone());
        if let Some(t) = &target {
            let busy = selected.as_ref().is_some_and(|l| l.loading);
            if ui.add_enabled(!busy, egui::Button::new("Detect")).on_hover_text("Look for python3 and virtualenvs there").clicked() {
                cmds.push(("python.remoteInterpreters.detect", json!({ "target": t })));
            }
        }
    });
    let Some(list) = selected else { return };
    if list.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(muted(&format!("Detecting on {}…", list.label)));
        });
    }
    if let Some(e) = &list.error {
        ui.label(RichText::new(e).color(theme.error).small());
    }
    if let Some(root) = &list.root {
        ui.label(muted(&format!("Project folder there: {root}")));
    }
    let Some(target) = list.target.clone() else { return };
    let configs: Vec<String> = s.run_store.user.iter().map(|c| c.name.clone()).collect();
    egui::Grid::new("python-remote-interpreters").num_columns(3).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
        for i in &list.interpreters {
            let chosen = s.targets.python.selected_python.as_deref() == Some(i.path.as_str());
            let text = format!("Python {}{}", i.version, if i.venv { "  (virtualenv)" } else { "" });
            if ui.add(egui::Button::selectable(chosen, text)).on_hover_text(&i.path).clicked() {
                cmds.push(("python.remoteInterpreters.packages", json!({ "target": target, "python": i.path })));
            }
            ui.label(muted(&i.path));
            ui.menu_button("Use for…", |ui| {
                if configs.is_empty() {
                    ui.label(muted("No run configurations yet"));
                }
                for c in &configs {
                    if ui.button(c).clicked() {
                        cmds.push((
                            "python.remoteInterpreters.use",
                            json!({ "config": c, "python": i.path, "target": target }),
                        ));
                        ui.close();
                    }
                }
            });
            ui.end_row();
        }
    });
    if let Some(p) = s.targets.python.selected_packages() {
        ui.add_space(4.0);
        if p.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(muted("Listing packages…"));
            });
        }
        if let Some(e) = &p.error {
            ui.label(RichText::new(e).color(theme.error).small());
        }
        if !p.packages.is_empty() {
            ui.label(muted(&format!("{} packages", p.packages.len())));
            egui::ScrollArea::vertical().id_salt("python-remote-packages").max_height(160.0).show(ui, |ui| {
                egui::Grid::new("python-remote-packages-grid").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                    for pk in &p.packages {
                        ui.label(&pk.name);
                        ui.label(muted(&pk.version));
                        ui.end_row();
                    }
                });
            });
        }
    }
}
