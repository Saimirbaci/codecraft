use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use codecraft_text::Position;

/// Ordered most severe first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    /// Weak warning / information.
    Info,
    Hint,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Hint => "hint",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProblemSource {
    /// Language server diagnostics (rust-analyzer native + flycheck).
    RustAnalyzer,
    /// `cargo check`/`build`/`clippy` compiler messages from a Build or Run.
    Cargo,
}

/// A diagnostic in a file, in char-based positions (line, char column; zero-based).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub path: PathBuf,
    pub start: Position,
    pub end: Position,
    pub severity: Severity,
    pub message: String,
    pub code: Option<String>,
    pub source: ProblemSource,
}
