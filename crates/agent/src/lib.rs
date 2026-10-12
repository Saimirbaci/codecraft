//! CodeCraft coding-agent backends (L0, no workspace deps).
//!
//! - [`backend`]: which agent CLIs exist (Claude Code, Codex, Grok, Gemini), found on an enriched
//!   `PATH` because GUI apps start with a minimal one.
//! - [`cli`] + [`parse`] + [`process`]: run a CLI headless and turn its JSON stream into the
//!   common [`AgentEvent`] model.
//! - [`acp`]: a client for the Agent Client Protocol (JSON-RPC 2.0 over the agent's stdio), used
//!   when every change needs approval.
//! - [`git`]: per-job git worktrees for review mode, and the list of files a job changed.
//! - [`diff`]: line diffs (unified and side by side) on top of the `similar` crate.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod acp;
pub mod backend;
pub mod cli;
pub mod diff;
pub mod event;
pub mod git;
pub mod parse;
pub mod process;

pub use backend::{Backend, BackendInfo};
pub use event::{AgentEvent, ChangeKind, PlanEntry};
pub use process::AgentProcess;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("cannot start {program}: {source}")]
    Spawn { program: String, source: std::io::Error },
    #[error("{0}")]
    Git(String),
    #[error("{0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Cut `s` to at most `max` chars, marking the cut with `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
