//! Coding-agent jobs (Claude Code, Codex, Grok, Gemini, ACP agents), reviews and activity. OWNED BY the agents track.

use crate::Session;

#[derive(Default)]
pub struct AgentsState {}

/// Drain background events; true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}

/// Stop background work (window closing).
pub(crate) fn shutdown(_s: &mut Session) {}
