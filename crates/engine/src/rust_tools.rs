//! Rust depth (M12 track RS): the state behind macro expansion, Cargo.toml dependency
//! intelligence, the Rust REPL and the advisory inspections, and their pump.

use std::path::PathBuf;

use codecraft_lsp::ResponseError;
use serde_json::Value;

use crate::Session;

/// Everything track RS keeps per session.
#[derive(Default)]
pub struct RustState {
    pub deps: crate::cargo_deps::DepsState,
    pub macros: crate::macro_expand::MacroState,
    pub repl: crate::rust_repl::Repl,
    /// Files whose advisory problems are shown (cleared on the next run).
    pub(crate) advisory_files: Vec<PathBuf>,
}

impl RustState {
    pub fn busy(&self) -> bool {
        // An idle REPL waits for input: no need to keep polling it every frame.
        self.deps.busy() || self.macros.busy() || (self.repl.is_alive() && (self.repl.running || !self.repl.ready))
    }
}

/// Language server requests of the Rust tools.
pub(crate) enum RustPending {
    ExpandMacro {
        request: u64,
        path: PathBuf,
        line: usize,
    },
    /// `experimental/runnables` for a test run's file (M13 track RC).
    Runnables {
        tab: codecraft_workspace::RunTabId,
        path: PathBuf,
    },
}

pub(crate) fn on_response(s: &mut Session, pending: RustPending, result: Result<Value, ResponseError>) {
    match pending {
        RustPending::ExpandMacro { request, path, line } => {
            crate::macro_expand::on_response(s, request, path, line, result)
        }
        RustPending::Runnables { tab, path } => crate::testing::ra::on_response(s, tab, path, result),
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = crate::cargo_deps::pump(s);
    changed |= crate::macro_expand::pump(s);
    let (repl_changed, finished) = s.rust.repl.poll();
    changed |= repl_changed;
    if let Some(status) = finished {
        s.status = if status == "ok" { "Rust REPL: done".into() } else { "Rust REPL: error".into() };
    }
    changed
}

pub(crate) fn shutdown(s: &mut Session) {
    s.rust.repl.stop();
}
