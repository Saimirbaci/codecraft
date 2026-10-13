//! SSH and remote: the File Transfer and Remote Host tool windows, Deployment and Remote Development dialogs (M11 track S).

use codecraft_engine::Session;

use crate::UiState;

/// File Transfer (stub until its M11 track lands).
pub fn show_file_transfer(ui: &mut egui::Ui, _s: &mut Session, _st: &mut UiState) {
    ui.weak("File Transfer: not implemented yet");
}

/// Remote Host (stub until its M11 track lands).
pub fn show_remote_host(ui: &mut egui::Ui, _s: &mut Session, _st: &mut UiState) {
    ui.weak("Remote Host: not implemented yet");
}
