//! Python interpreters, packages and the Python Console (M11 track Y).

use crate::Session;

#[derive(Debug, Default)]
pub struct PythonState {}

impl PythonState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        false
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
