//! Test runner UI (track T): the test tree beside the Run console, and the Coverage tool window.

use codecraft_engine::Session;
use egui::RichText;

use crate::UiState;

/// The Coverage tool window.
pub fn show_coverage(ui: &mut egui::Ui, _s: &mut Session, st: &mut UiState) {
    ui.label(RichText::new("No coverage data. Run tests with coverage.").color(st.theme.text_muted));
}
