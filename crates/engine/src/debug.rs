//! Debugger sessions (DAP): breakpoints, threads, frames, variables, console. OWNED BY the debugger track.

use crate::Session;

#[derive(Default)]
pub struct DebugState {}

/// Drain background events; true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}

/// Stop background work (window closing).
pub(crate) fn shutdown(_s: &mut Session) {}
