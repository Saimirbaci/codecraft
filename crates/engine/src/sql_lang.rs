//! SQL inside Rust strings and SQL editing support (M11 track Q).

use crate::Session;

#[derive(Debug, Default)]
pub struct SqlLangState {}

impl SqlLangState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        false
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
