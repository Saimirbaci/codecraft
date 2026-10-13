//! CodeCraft text model (L0, no workspace deps).
//!
//! A rope-backed [`Document`] with undo history and line-ending memory, multi-caret
//! [`SelectionSet`]s, [`ChangeSet`]s, editing operations ([`ops`]), caret motion ([`movement`])
//! and in-document search ([`search`]). Pure data: no I/O, no UI.
//!
//! Coordinates are **char offsets** into the document (ropey's native index). Internally the
//! text always uses `\n`; the original line ending is remembered and restored on save.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod change;
pub mod column;
mod document;
pub mod movement;
pub mod ops;
pub mod search;
mod selection;
mod util;
pub mod wrap;

#[cfg(test)]
mod fuzz_tests;
#[cfg(test)]
mod test_util;

pub use change::{Assoc, Change, ChangeSet};
pub use document::{Document, EditGroup, LineEnding, Position};
pub use selection::{Selection, SelectionSet};

/// Errors from the text model. Never a panic: every API returns this for bad input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("change {start}..{end} is out of bounds for a document of {len} chars")]
    OutOfBounds { start: usize, end: usize, len: usize },
    #[error("changes overlap")]
    Overlap,
    #[error("invalid search pattern: {0}")]
    BadPattern(String),
}

pub type Result<T> = std::result::Result<T, TextError>;

/// The result of an editing operation: the changes to apply (in pre-edit coordinates) and the
/// selections afterwards (in post-edit coordinates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub changes: ChangeSet,
    pub selections: SelectionSet,
}

impl Edit {
    /// An edit that changes nothing and leaves the selections as given.
    pub fn none(selections: SelectionSet) -> Self {
        Self { changes: ChangeSet::empty(), selections }
    }

    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }
}
