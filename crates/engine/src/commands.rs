//! The command registry. Every user-visible action is a [`CommandSpec`]; menus come from the
//! observed RustRover [`catalog`](crate::catalog) and go live when a command with the same id is
//! registered here.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Session;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("unknown command: {0}")]
    Unknown(String),
    #[error("{0}")]
    Disabled(String),
    #[error("bad parameters: {0}")]
    BadParams(String),
    #[error("{0}")]
    Failed(String),
}

pub type CmdResult = Result<Value, CommandError>;
pub type Enabled = fn(&Session) -> Result<(), String>;
pub type Run = fn(&mut Session, &Value) -> CmdResult;

/// Where a command's shortcut applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Context {
    /// Anywhere in the window.
    Global,
    /// Only while the editor has keyboard focus (typing, caret motion, editor actions).
    Editor,
}

pub struct CommandSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// Default shortcut (`Cmd+Shift+N` style; order-insensitive). `None` = the catalog's shortcut.
    pub shortcut: Option<&'static str>,
    /// Extra shortcuts (RustRover binds several keys to some actions).
    pub alt_shortcuts: &'static [&'static str],
    /// Human/agent-readable parameter description.
    pub params: &'static str,
    pub context: Context,
    pub enabled: Enabled,
    pub run: Run,
}

/// Classification of an observed menu item (see `cargo xtask catalog`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// The current milestones' surface.
    Core,
    /// RustRover features planned for later milestones.
    Later,
    /// From third-party or non-Rust plugins in the observed install.
    Out,
    /// Provided by macOS.
    Os,
}

/// One observed menu entry. `label == "---"` is a separator.
#[derive(Clone, Copy, Debug)]
pub struct CatalogItem {
    pub path: &'static [&'static str],
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub id: &'static str,
    pub submenu: bool,
    pub scope: Scope,
}

pub struct Registry {
    specs: Vec<CommandSpec>,
    by_id: HashMap<&'static str, usize>,
    catalog_by_id: HashMap<&'static str, usize>,
}

impl Registry {
    pub fn get(&self, id: &str) -> Option<&CommandSpec> {
        self.by_id.get(id).and_then(|i| self.specs.get(*i))
    }

    pub fn all(&self) -> &[CommandSpec] {
        &self.specs
    }

    pub fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    pub fn catalog_item(&self, id: &str) -> Option<&'static CatalogItem> {
        self.catalog_by_id.get(id).and_then(|i| crate::catalog::CATALOG.get(*i))
    }

    /// The effective shortcuts of a command: its own, else the catalog's, plus alternates.
    pub fn shortcuts(&self, id: &str) -> Vec<&'static str> {
        let Some(spec) = self.get(id) else { return Vec::new() };
        let primary = spec.shortcut.or_else(|| self.catalog_item(id).and_then(|c| c.shortcut));
        primary.into_iter().chain(spec.alt_shortcuts.iter().copied()).collect()
    }

    /// The menu path of a command if it appears in the catalog (e.g. `["Edit", "Find"]`).
    pub fn menu_path(&self, id: &str) -> Option<&'static [&'static str]> {
        self.catalog_item(id).map(|c| c.path)
    }
}

/// The global registry (built once).
pub fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| {
        let mut specs = Vec::new();
        specs.extend(crate::file_cmds::specs());
        specs.extend(crate::edit_cmds::specs());
        specs.extend(crate::editor_cmds::specs());
        specs.extend(crate::view_cmds::specs());
        specs.extend(crate::navigate_cmds::specs());
        specs.extend(crate::code_cmds::specs());
        specs.extend(crate::run_cmds::specs());
        specs.extend(crate::window_cmds::specs());
        specs.extend(crate::lang_cmds::specs());
        specs.extend(crate::terminal_cmds::specs());
        specs.extend(crate::vcs_cmds::specs());
        specs.extend(crate::debug_cmds::specs());
        specs.extend(crate::database_cmds::specs());
        specs.extend(crate::agent_cmds::specs());
        specs.extend(crate::ide_cmds::specs());
        specs.extend(crate::feedback_cmds::specs());
        let mut by_id = HashMap::new();
        for (i, s) in specs.iter().enumerate() {
            if by_id.insert(s.id, i).is_some() {
                log::error!("duplicate command id {}", s.id);
            }
        }
        let catalog_by_id =
            crate::catalog::CATALOG.iter().enumerate().filter(|(_, c)| c.id != "---").map(|(i, c)| (c.id, i)).collect();
        Registry { specs, by_id, catalog_by_id }
    })
}

// ---------------------------------------------------------------- enabled predicates

pub(crate) fn always(_: &Session) -> Result<(), String> {
    Ok(())
}

pub(crate) fn has_editor(s: &Session) -> Result<(), String> {
    s.ws.active().map(|_| ()).ok_or_else(|| "no editor is open".into())
}

pub(crate) fn has_project(s: &Session) -> Result<(), String> {
    s.ws.project.as_ref().map(|_| ()).ok_or_else(|| "no project is open".into())
}

pub(crate) fn has_tabs(s: &Session) -> Result<(), String> {
    if s.ws.tabs.is_empty() { Err("no editor tabs are open".into()) } else { Ok(()) }
}

pub(crate) fn has_lsp_editor(s: &Session) -> Result<(), String> {
    has_editor(s)?;
    s.lsp_ready()
}

/// Shorthand for building specs.
#[allow(clippy::too_many_arguments)]
pub(crate) const fn spec(
    id: &'static str,
    label: &'static str,
    shortcut: Option<&'static str>,
    params: &'static str,
    context: Context,
    enabled: Enabled,
    run: Run,
) -> CommandSpec {
    CommandSpec { id, label, shortcut, alt_shortcuts: &[], params, context, enabled, run }
}

/// A global command with no own shortcut (uses the catalog's) and no params.
pub(crate) const fn simple(id: &'static str, label: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    spec(id, label, None, "", Context::Global, enabled, run)
}

/// An editor-context command with no own shortcut (uses the catalog's) and no params.
pub(crate) const fn editor(id: &'static str, label: &'static str, run: Run) -> CommandSpec {
    spec(id, label, None, "", Context::Editor, has_editor, run)
}

pub(crate) fn ok() -> CmdResult {
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_unique_and_catalog_ids_known() {
        let reg = registry();
        let mut seen = std::collections::HashSet::new();
        for s in reg.all() {
            assert!(seen.insert(s.id), "duplicate {}", s.id);
            assert!(!s.label.is_empty(), "{} has no label", s.id);
        }
        // Every live catalog item has a spec; menu ids must match exactly.
        let live = crate::catalog::CATALOG.iter().filter(|c| reg.contains(c.id)).count();
        assert!(live > 50, "only {live} catalog items are live");
    }
}
