//! Version control state: repository status, change lists, log, branches, blame, line markers. OWNED BY the Git track.

use crate::Session;

#[derive(Default)]
pub struct VcsState {}

/// Drain background events; true if anything changed.
pub(crate) fn pump(_s: &mut Session) -> bool {
    false
}

/// Stop background work (window closing).
pub(crate) fn shutdown(_s: &mut Session) {}

/// Files changed on disk (from the project file watcher): refresh status as needed.
pub(crate) fn files_changed(_s: &mut Session, _paths: &[std::path::PathBuf]) {}
