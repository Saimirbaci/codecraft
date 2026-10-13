//! Docker: connections, containers, images, volumes, networks and Compose projects through the `docker` CLI (M11 track D).

use crate::Session;

#[derive(Debug, Default)]
pub struct DockerState {}

impl DockerState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        false
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
