//! Refactoring UI (track R): previews, dialogs and the Hierarchy tool window.

use codecraft_engine::Session;
use egui::RichText;

use crate::UiState;

/// The Hierarchy tool window (type, call and method hierarchies).
pub fn show_hierarchy(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(RichText::new("Use Navigate › Call Hierarchy or Type Hierarchy.").color(st.theme.text_muted));
}
