//! The Commit and Git tool windows, branch widget and VCS decorations. OWNED BY the Git track.

use codecraft_engine::Session;

use crate::UiState;

pub fn show(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(egui::RichText::new("Not implemented yet.").color(st.theme.text_muted));
}

/// The Git tool window (log, console); `show` is the Commit tool window.
pub fn show_git(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    show(ui, s, st);
}
