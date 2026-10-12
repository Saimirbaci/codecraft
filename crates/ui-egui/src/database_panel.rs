//! The Database tool window, data source dialog, SQL console toolbar and result grids. OWNED BY the database track.

use codecraft_engine::Session;

use crate::UiState;

pub fn show(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(egui::RichText::new("Not implemented yet.").color(st.theme.text_muted));
}
