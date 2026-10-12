//! Terminal sessions (PTY + emulator) behind the Terminal tool window. OWNED BY the terminal track.

use crate::Session;

#[derive(Default)]
pub struct TerminalState {}

/// Drain background events; true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}

/// Stop background work (window closing).
pub(crate) fn shutdown(_s: &mut Session) {}
