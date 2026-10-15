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

/// Command ids that were renamed to the observed catalog's id once RustRover's lazily filled
/// submenus were captured: `(old id, catalog id)`. The old ids keep working for agents, scripts
/// and saved keymaps ([`Registry::get`] resolves them), but only the catalog id is registered, so
/// menus, the keymap and command lists show each action once.
pub const ALIASES: &[(&str, &str)] = &[
    ("edit.paste", "edit.paste.paste"),
    ("edit.bookmarks.next", "edit.bookmarks.nextLineBookmark"),
    ("edit.bookmarks.previous", "edit.bookmarks.previousLineBookmark"),
    ("edit.bookmarks.show", "edit.bookmarks.showLineBookmarks"),
    ("edit.bookmarks.gotoMnemonic", "edit.bookmarks.goToMnemonic"),
    ("run.debuggingActions.resume", "run.debuggingActions.resumeProgram"),
    ("run.debuggingActions.pause", "run.debuggingActions.pauseAllPrograms"),
    ("code.folding.toggle", "code.folding.toggleFolding"),
    ("code.reformatFileWithRustfmt", "tools.rust.reformatFileWithRustfmt"),
    ("code.reformatCargoProjectWithRustfmt", "tools.rust.reformatCargoProjectWithRustfmt"),
    ("view.hideAllToolWindows", "window.activeToolWindow.hideAllWindows"),
];

pub struct Registry {
    specs: Vec<CommandSpec>,
    by_id: HashMap<&'static str, usize>,
    catalog_by_id: HashMap<&'static str, usize>,
    aliases: HashMap<&'static str, &'static str>,
}

impl Registry {
    /// The command registered for `id` (an [`ALIASES`] old id resolves to its catalog id).
    pub fn get(&self, id: &str) -> Option<&CommandSpec> {
        self.by_id.get(self.canonical(id)).and_then(|i| self.specs.get(*i))
    }

    /// The registered id for `id`: the catalog id of an old (aliased) id, else `id` itself.
    pub fn canonical<'a>(&self, id: &'a str) -> &'a str {
        match self.aliases.get(id) {
            Some(primary) => primary,
            None => id,
        }
    }

    /// The old ids that resolve to `id`.
    pub fn aliases_of(&self, id: &str) -> Vec<&'static str> {
        ALIASES.iter().filter(|(_, p)| *p == id).map(|(a, _)| *a).collect()
    }

    pub fn all(&self) -> &[CommandSpec] {
        &self.specs
    }

    pub fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(self.canonical(id))
    }

    pub fn catalog_item(&self, id: &str) -> Option<&'static CatalogItem> {
        self.catalog_by_id.get(self.canonical(id)).and_then(|i| crate::catalog::CATALOG.get(*i))
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
        specs.extend(crate::app_cmds::specs());
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
        specs.extend(crate::vcs_ext_cmds::specs());
        specs.extend(crate::vcs::changelists::specs());
        specs.extend(crate::vcs::shelf::specs());
        specs.extend(crate::github_prs::specs());
        specs.extend(crate::debug_cmds::specs());
        specs.extend(crate::database_cmds::specs());
        specs.extend(crate::agent_cmds::specs());
        specs.extend(crate::agent_sessions::specs());
        specs.extend(crate::ide_cmds::specs());
        specs.extend(crate::feedback_cmds::specs());
        specs.extend(crate::editing_cmds::specs());
        specs.extend(crate::completion::specs());
        specs.extend(crate::split_cmds::specs());
        specs.extend(crate::testing_cmds::specs());
        specs.extend(crate::refactor_cmds::specs());
        specs.extend(crate::bookmark_cmds::specs());
        specs.extend(crate::todo::specs());
        specs.extend(crate::local_history_cmds::specs());
        specs.extend(crate::scratch::specs());
        specs.extend(crate::settings_cmds::specs());
        specs.extend(crate::prefs_cmds::specs());
        specs.extend(crate::run_config_cmds::specs());
        specs.extend(crate::misc_cmds::specs());
        specs.extend(crate::inspection_cmds::specs());
        specs.extend(crate::render_cmds::specs());
        specs.extend(crate::session_store::specs());
        specs.extend(crate::hot_exit::specs());
        specs.extend(crate::appearance::specs());
        specs.extend(crate::paste::specs());
        specs.extend(crate::usages::specs());
        specs.extend(crate::folding::specs());
        specs.extend(crate::tool_window_cmds::specs());
        specs.extend(crate::services::specs());
        specs.extend(crate::docker_cmds::specs());
        specs.extend(crate::remote_cmds::specs());
        specs.extend(crate::http_cmds::specs());
        specs.extend(crate::python_cmds::specs());
        specs.extend(crate::profiler_cmds::specs());
        specs.extend(crate::sql_lang_cmds::specs());
        specs.extend(crate::rust_cmds::specs());
        specs.extend(crate::notebook_cmds::specs());
        specs.extend(crate::python_output::specs());
        specs.extend(crate::python_format::specs());
        specs.extend(crate::templates_django::specs());
        specs.extend(crate::ssr_cmds::specs());
        specs.extend(crate::macros::specs());
        specs.extend(crate::presentation::specs());
        specs.extend(crate::analysis::specs());
        specs.extend(crate::duplicates::specs());
        specs.extend(crate::data_flow::specs());
        specs.extend(crate::layouts::specs());
        specs.extend(crate::file_templates::specs());
        specs.extend(crate::copyright::specs());
        specs.extend(crate::tasks_contexts::specs());
        specs.extend(crate::markdown_export::specs());
        specs.extend(crate::inline_completion::specs());
        specs.extend(crate::cpp::specs());
        // M13 track RC: before-launch tasks, all tests in a file.
        specs.extend(crate::before_launch::specs());
        specs.extend(crate::test_file::specs());
        // M13 track RC: the last menu items.
        specs.extend(crate::zig_tool::specs());
        specs.extend(crate::typescript_tool::specs());
        specs.extend(crate::modify_code::specs());
        specs.extend(crate::command_completion::specs());
        // Routes come last so they wrap every registered command. SQL files and SQL in strings:
        // navigation commands handle SQL (track DC).
        crate::database_cmds::sql_editor::route(&mut specs);
        // Python files format with Ruff/Black; templates navigate and complete (track PY).
        crate::python_format::route(&mut specs);
        crate::templates_django::route(&mut specs);
        let mut by_id = HashMap::new();
        for (i, s) in specs.iter().enumerate() {
            if by_id.insert(s.id, i).is_some() {
                log::error!("duplicate command id {}", s.id);
            }
        }
        let catalog_by_id =
            crate::catalog::CATALOG.iter().enumerate().filter(|(_, c)| c.id != "---").map(|(i, c)| (c.id, i)).collect();
        // An alias never shadows a registered id.
        let aliases = ALIASES.iter().copied().filter(|(old, _)| !by_id.contains_key(old)).collect();
        Registry { specs, by_id, catalog_by_id, aliases }
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
    if s.ws.tabs().is_empty() { Err("no editor tabs are open".into()) } else { Ok(()) }
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

    #[test]
    fn aliases_resolve_to_registered_catalog_ids() {
        let reg = registry();
        for (old, primary) in ALIASES {
            assert!(!reg.all().iter().any(|s| s.id == *old), "{old} is still registered");
            let spec = reg.get(old).unwrap_or_else(|| panic!("{old} does not resolve"));
            assert_eq!(spec.id, *primary);
            assert!(reg.catalog_item(primary).is_some(), "{primary} is not a catalog id");
            assert_eq!(reg.canonical(old), *primary);
            assert!(reg.contains(old));
            assert_eq!(reg.shortcuts(old), reg.shortcuts(primary));
            assert!(reg.aliases_of(primary).contains(old));
        }
        assert_eq!(reg.canonical("edit.copy"), "edit.copy");
    }

    #[test]
    fn old_ids_execute_through_their_alias() {
        let mut s = crate::Session::new();
        let d = s.ws.add_document(None, "abc");
        s.ws.open_editor(d);
        s.execute("edit.paste", &serde_json::json!({ "text": "x" })).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "xabc");
        assert!(s.enabled("edit.paste").is_ok());
        assert!(matches!(s.execute("no.such.command", &Value::Null), Err(CommandError::Unknown(_))));
    }
}
