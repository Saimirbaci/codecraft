//! Thin data-driven version dialog; store mutations are engine commands on workers.
use crate::{UiState, run_command};
use codecraft_engine::Session;
use serde_json::{Value, json};
pub(crate) fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if !s.versions.open {
        return;
    }
    let mut open = true;
    let entries = s.versions.entries.clone();
    egui::Window::new("Versions & Rollback…").open(&mut open).resizable(true).show(ctx, |ui| {
        ui.label("Selections take effect on the next launch.");
        if s.versions.busy {
            ui.spinner();
        }
        ui.add_enabled_ui(!s.versions.busy, |ui| {
            ui.horizontal(|ui| {
                for (label, id) in [
                    ("Refresh", "version.list"),
                    ("Roll Back", "version.rollback"),
                    ("Launch Factory Version", "version.launchFactory"),
                ] {
                    if ui.button(label).clicked() {
                        run_command(s, st, id, &Value::Null);
                    }
                }
            });
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for m in entries {
                    ui.separator();
                    ui.label(format!("{} — {:?}", m.id, m.status));
                    if let Some(reason) = m.reason {
                        ui.label(reason);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Use This Version").clicked() {
                            run_command(s, st, "version.use", &json!({"id": m.id}));
                        }
                        if ui.button("Mark Good").clicked() {
                            run_command(s, st, "version.markGood", &json!({"id": m.id}));
                        }
                    });
                }
            });
        });
    });
    s.versions.open = open;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialog_paints_version_status_and_uses_help_menu_command() {
        let mut s = Session::new();
        s.versions.open = true;
        s.versions.entries.push(
            serde_json::from_value(json!({
                "id": "0.3.0-abcd-1", "semver": "0.3.0", "commit": "abcd", "built_at": 1,
                "source_branch": "fake", "parent": null, "status": "bad", "reason": "boot timeout", "size": 1
            }))
            .unwrap(),
        );
        let ctx = egui::Context::default();
        let mut st = UiState::new();
        let mut out = ctx.run_ui(Default::default(), |ui| show(ui.ctx(), &mut s, &mut st));
        out.textures_delta.clear();
        assert!(s.versions.open);
        assert!(!s.versions.busy);
        assert!(crate::menus::EXTRA.contains(&("Help", "version.list")));
        assert!(codecraft_engine::registry().contains("version.rollback"));
    }
}
