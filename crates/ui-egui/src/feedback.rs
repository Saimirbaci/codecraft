//! The feedback overlay: draw on top of CodeCraft, describe a change, and send it to a coding
//! agent. OWNED BY the feedback track.

use codecraft_engine::Session;

use crate::UiState;

/// Draw the overlay when active (no-op otherwise). Called last in the frame.
pub fn overlay(_ui: &mut egui::Ui, _s: &mut Session, _st: &mut UiState) {}
