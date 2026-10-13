//! SSH servers, deployment mappings, uploads/downloads, Remote Host browsing and tunnels (M11 track S).

use crate::Session;

#[derive(Debug, Default)]
pub struct RemoteState {}

impl RemoteState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        false
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
