//! Approval cards for pending agent actions (diff preview + Allow / Deny). OWNED BY the agents track.

use codecraft_engine::Session;

use crate::UiState;

/// Draw pending approvals as a floating stack (no-op when nothing is pending).
pub fn show(_ui: &mut egui::Ui, _s: &mut Session, _st: &mut UiState) {}
