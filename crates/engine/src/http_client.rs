//! HTTP Client: `.http` files, environments, running requests and their responses/history (M11 track H).

use crate::Session;

#[derive(Debug, Default)]
pub struct HttpState {}

impl HttpState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        false
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
