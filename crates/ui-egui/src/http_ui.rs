//! HTTP Client responses in the Services tool window (M11 track H).

use codecraft_engine::Session;

use crate::UiState;

/// HTTP Requests (stub until its M11 track lands).
pub fn show_services(ui: &mut egui::Ui, _s: &mut Session, _st: &mut UiState) {
    ui.weak("HTTP Requests: not implemented yet");
}
