//! Database connections, schema trees, SQL consoles and result sets. OWNED BY the database track.

use crate::Session;

#[derive(Default)]
pub struct DbState {}

/// Drain background events; true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}

/// Stop background work (window closing).
pub(crate) fn shutdown(_s: &mut Session) {}
