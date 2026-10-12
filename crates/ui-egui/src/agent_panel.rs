//! The Agent tool window: job launcher, activity feed, review of changes. OWNED BY the agents track.

use codecraft_engine::Session;

use crate::UiState;

pub fn show(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(egui::RichText::new("Not implemented yet.").color(st.theme.text_muted));
}
