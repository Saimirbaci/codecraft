//! The Services tool window: a section bar (Database, Docker, HTTP Requests) above the selected
//! section's panel. The selection lives in `Session::services` so commands can switch it.

use serde_json::json;

use codecraft_engine::Session;
use codecraft_engine::services::ServicesSection;

use crate::{UiState, run_command};

pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let current = s.services.section;
    let mut pick = None;
    ui.horizontal(|ui| {
        for section in ServicesSection::ALL {
            if ui.selectable_label(current == section, section.title()).clicked() && current != section {
                pick = Some(section);
            }
        }
    });
    if let Some(section) = pick {
        run_command(s, st, "services.select", &json!({ "section": section.key() }));
    }
    ui.separator();
    match s.services.section {
        ServicesSection::Database => crate::database_panel::services(ui, s, st),
        ServicesSection::Docker => crate::docker_ui::show_services(ui, s, st),
        ServicesSection::Http => crate::http_ui::show_services(ui, s, st),
    }
}
