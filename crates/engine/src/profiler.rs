//! Profiling runs and snapshots shown in the Profiler tool window (M11 track P).

use crate::Session;

#[derive(Debug, Default)]
pub struct ProfilerState {}

impl ProfilerState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        false
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}
