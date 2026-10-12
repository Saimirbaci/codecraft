//! IDE server for external agents (Claude Code IDE integration / MCP). OWNED BY the IDE-server track.

use crate::Session;

#[derive(Default)]
pub struct IdeServerState {}

/// Drain background events; true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}

/// Stop background work (window closing).
pub(crate) fn shutdown(_s: &mut Session) {}
