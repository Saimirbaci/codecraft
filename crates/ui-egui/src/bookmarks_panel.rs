//! Bookmarks and TODO tool windows.

use codecraft_engine::Session;
use egui::RichText;

use crate::UiState;

pub fn show_bookmarks(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(RichText::new("No bookmarks. Toggle one with F3.").color(st.theme.text_muted));
}

pub fn show_todo(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(RichText::new("No TODO items found.").color(st.theme.text_muted));
}
