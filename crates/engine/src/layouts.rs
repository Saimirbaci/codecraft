//! Window › Layouts (Default, Save Current Layout as New…, and per custom layout Restore / Save
//! Changes / Rename / Delete) persisted in `$CODECRAFT_HOME/layouts.json`, and the Window ›
//! Active Tool Window actions that act on the tool window shown last: Maximize, Select Next /
//! Previous Tab, Close Active Tab, Group Tabs and Resize › Stretch to Left/Right/Top/Bottom.
//!
//! A layout is which tool windows are open on each side and their sizes. The custom-layout menu
//! items act on the current custom layout (the one saved or restored last) or on `name`; without
//! one, the Layouts dialog asks.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{Layout, Side, ToolWindowId};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{opt_str, req_str};
use crate::user_store::{Store, UserHome};
use crate::{CommandError, Session};

pub const LAYOUTS_FILE: &str = "layouts.json";
/// Pixels per Stretch step.
pub const STRETCH_STEP: f32 = 40.0;
const MAX_LAYOUTS: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedLayout {
    pub name: String,
    pub layout: Layout,
}

/// What the Layouts dialog asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutAction {
    SaveAsNew,
    Restore,
    SaveChanges,
    Rename,
    Delete,
}

impl LayoutAction {
    pub fn title(self) -> &'static str {
        match self {
            Self::SaveAsNew => "Save Current Layout as New",
            Self::Restore => "Restore Layout",
            Self::SaveChanges => "Save Changes to Layout",
            Self::Rename => "Rename Layout",
            Self::Delete => "Delete Layout",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutDialog {
    pub action: LayoutAction,
    /// The layout acted on (Restore/Save Changes/Rename/Delete).
    pub selected: Option<String>,
    /// The new name (Save as New, Rename).
    pub name: String,
    pub error: Option<String>,
}

pub struct LayoutsState {
    pub saved: Store<NamedLayout>,
    pub home: UserHome,
    /// The custom layout saved or restored last.
    pub current: Option<String>,
    pub dialog: Option<LayoutDialog>,
}

impl Default for LayoutsState {
    fn default() -> Self {
        Self { saved: Store::new(LAYOUTS_FILE), home: UserHome::default(), current: None, dialog: None }
    }
}

impl LayoutsState {
    pub fn set_home(&mut self, dir: std::path::PathBuf) {
        self.home.dir_override = Some(dir);
        self.saved.reset();
    }

    pub fn names(&mut self) -> Vec<String> {
        self.saved.ensure(&self.home);
        self.saved.items.iter().map(|l| l.name.clone()).collect()
    }
}

fn save(s: &mut Session) -> Result<(), CommandError> {
    let home = s.layouts.home.clone();
    s.layouts.saved.save(&home).map(|_| ()).map_err(CommandError::Failed)
}

/// Apply a saved layout (keeps the hidden-all state off and forces the panels to the sizes).
fn apply(s: &mut Session, layout: &Layout) {
    let rev = s.ws.layout.size_rev;
    s.ws.layout = layout.clone();
    s.ws.layout.all_hidden = false;
    s.ws.layout.maximized = None;
    s.ws.layout.size_rev = rev.wrapping_add(1);
}

fn target(s: &mut Session, p: &Value, action: LayoutAction) -> Result<Option<String>, CommandError> {
    let name = opt_str(p, "name").map(str::to_string).or_else(|| s.layouts.current.clone());
    match name {
        Some(n) => {
            if !s.layouts.names().contains(&n) {
                return Err(CommandError::BadParams(format!("no layout named '{n}'")));
            }
            Ok(Some(n))
        }
        None => {
            let names = s.layouts.names();
            if names.is_empty() {
                return Err(CommandError::Failed("there are no custom layouts (Save Current Layout as New…)".into()));
            }
            s.layouts.dialog = Some(LayoutDialog { action, selected: names.first().cloned(), name: String::new(), error: None });
            Ok(None)
        }
    }
}

fn save_as_new(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = opt_str(p, "name") else {
        s.layouts.dialog = Some(LayoutDialog { action: LayoutAction::SaveAsNew, selected: None, name: String::new(), error: None });
        return Ok(json!({ "dialog": true }));
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(CommandError::BadParams("the layout needs a name".into()));
    }
    if name.eq_ignore_ascii_case("default") {
        return Err(CommandError::BadParams("'Default' is the built-in layout".into()));
    }
    let names = s.layouts.names();
    if names.contains(&name) {
        return Err(CommandError::BadParams(format!("a layout named '{name}' exists (use Save Changes)")));
    }
    if names.len() >= MAX_LAYOUTS {
        return Err(CommandError::Failed("too many layouts".into()));
    }
    let layout = s.ws.layout.clone();
    s.layouts.saved.items.push(NamedLayout { name: name.clone(), layout });
    save(s)?;
    s.layouts.current = Some(name.clone());
    s.layouts.dialog = None;
    s.status = format!("Saved layout '{name}'");
    Ok(json!({ "name": name }))
}

fn restore(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = target(s, p, LayoutAction::Restore)? else { return Ok(json!({ "dialog": true })) };
    let layout = s.layouts.saved.items.iter().find(|l| l.name == name).map(|l| l.layout.clone());
    let layout = layout.ok_or_else(|| CommandError::BadParams(format!("no layout named '{name}'")))?;
    apply(s, &layout);
    s.layouts.current = Some(name.clone());
    s.layouts.dialog = None;
    s.status = format!("Restored layout '{name}'");
    Ok(json!({ "name": name }))
}

fn save_changes(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = target(s, p, LayoutAction::SaveChanges)? else { return Ok(json!({ "dialog": true })) };
    let layout = s.ws.layout.clone();
    if let Some(l) = s.layouts.saved.items.iter_mut().find(|l| l.name == name) {
        l.layout = layout;
    }
    save(s)?;
    s.layouts.current = Some(name.clone());
    s.layouts.dialog = None;
    s.status = format!("Saved changes to layout '{name}'");
    Ok(json!({ "name": name }))
}

fn rename(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = target(s, p, LayoutAction::Rename)? else { return Ok(json!({ "dialog": true })) };
    let Some(new_name) = opt_str(p, "newName").map(str::trim).map(str::to_string) else {
        s.layouts.dialog = Some(LayoutDialog { action: LayoutAction::Rename, selected: Some(name.clone()), name: name.clone(), error: None });
        return Ok(json!({ "dialog": true }));
    };
    if new_name.is_empty() || new_name.eq_ignore_ascii_case("default") {
        return Err(CommandError::BadParams("choose another name".into()));
    }
    if new_name != name && s.layouts.names().contains(&new_name) {
        return Err(CommandError::BadParams(format!("a layout named '{new_name}' exists")));
    }
    if let Some(l) = s.layouts.saved.items.iter_mut().find(|l| l.name == name) {
        l.name = new_name.clone();
    }
    save(s)?;
    if s.layouts.current.as_deref() == Some(name.as_str()) {
        s.layouts.current = Some(new_name.clone());
    }
    s.layouts.dialog = None;
    Ok(json!({ "name": new_name }))
}

fn delete(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = target(s, p, LayoutAction::Delete)? else { return Ok(json!({ "dialog": true })) };
    s.layouts.saved.items.retain(|l| l.name != name);
    save(s)?;
    if s.layouts.current.as_deref() == Some(name.as_str()) {
        s.layouts.current = None;
    }
    s.layouts.dialog = None;
    s.status = format!("Deleted layout '{name}'");
    Ok(json!({ "deleted": name }))
}

// ---------------------------------------------------------------- Active Tool Window

fn active(s: &Session) -> Result<ToolWindowId, String> {
    s.ws.layout.active().ok_or_else(|| "no tool window is open".into())
}

fn has_active(s: &Session) -> Result<(), String> {
    active(s).map(|_| ())
}

/// Stretch the active tool window towards a side (its far edge moves that way).
fn stretch(s: &mut Session, toward: Side, vertical_up: Option<bool>) -> CmdResult {
    let tw = active(s).map_err(CommandError::Disabled)?;
    let side = tw.side();
    let delta = match (side, toward, vertical_up) {
        // Under Project, a window's size is its height.
        _ if tw.is_lower_left() => match vertical_up {
            Some(true) => STRETCH_STEP,
            Some(false) => -STRETCH_STEP,
            None => {
                s.status = format!("The {} tool window cannot stretch that way", tw.title());
                return Ok(json!({ "changed": false }));
            }
        },
        (Side::Left, Side::Right, None) | (Side::Right, Side::Left, None) => STRETCH_STEP,
        (Side::Left, Side::Left, None) | (Side::Right, Side::Right, None) => -STRETCH_STEP,
        (Side::Bottom, _, Some(true)) => STRETCH_STEP,
        (Side::Bottom, _, Some(false)) => -STRETCH_STEP,
        _ => {
            s.status = format!("The {} tool window cannot stretch that way", tw.title());
            return Ok(json!({ "changed": false }));
        }
    };
    s.ws.layout.maximized = None;
    let size = s.ws.layout.current_size(tw) + delta;
    s.ws.layout.set_size(tw, size);
    Ok(json!({ "window": tw.key(), "size": s.ws.layout.current_size(tw) }))
}

/// Tabs of the active tool window: (labels, active index).
pub fn tabs(s: &Session, tw: ToolWindowId) -> Option<(Vec<String>, usize)> {
    match tw {
        ToolWindowId::Run | ToolWindowId::Build => {
            let build = tw == ToolWindowId::Build;
            let runs: Vec<_> = s.ws.runs.iter().filter(|r| r.build == build).collect();
            let active = s.ws.active_run.and_then(|a| runs.iter().position(|r| r.id == a)).unwrap_or(runs.len().saturating_sub(1));
            Some((runs.iter().map(|r| r.title.clone()).collect(), active))
        }
        ToolWindowId::Terminal => {
            Some((s.terminal.tabs.iter().map(|t| t.osc_title.clone().unwrap_or_else(|| t.name.clone())).collect(), s.terminal.active))
        }
        ToolWindowId::Find => {
            let titles: Vec<String> = s.usages.recent.iter().map(|r| r.title.clone()).collect();
            let current = s.ws.find.as_ref().and_then(|f| titles.iter().position(|t| *t == f.title)).unwrap_or(0);
            Some((titles, current))
        }
        _ => None,
    }
}

fn select_tab(s: &mut Session, delta: isize) -> CmdResult {
    let tw = active(s).map_err(CommandError::Disabled)?;
    let Some((labels, current)) = tabs(s, tw) else {
        return Err(CommandError::Failed(format!("the {} tool window has no tabs", tw.title())));
    };
    if labels.is_empty() {
        return Ok(json!({ "tabs": 0 }));
    }
    let n = labels.len() as isize;
    let next = (current as isize + delta).rem_euclid(n) as usize;
    match tw {
        ToolWindowId::Run | ToolWindowId::Build => {
            let build = tw == ToolWindowId::Build;
            if let Some(id) = s.ws.runs.iter().filter(|r| r.build == build).nth(next).map(|r| r.id) {
                s.ws.active_run = Some(id);
            }
        }
        ToolWindowId::Terminal => {
            let id = if delta > 0 { "terminal.next" } else { "terminal.previous" };
            s.execute(id, &Value::Null)?;
        }
        ToolWindowId::Find => crate::usages::show_recent(s, next).map(|_| ())?,
        _ => {}
    }
    Ok(json!({ "tab": next, "title": labels.get(next) }))
}

fn close_tab(s: &mut Session) -> CmdResult {
    let tw = active(s).map_err(CommandError::Disabled)?;
    match tw {
        ToolWindowId::Run | ToolWindowId::Build => {
            let build = tw == ToolWindowId::Build;
            let runs: Vec<_> = s.ws.runs.iter().filter(|r| r.build == build).map(|r| r.id).collect();
            let id = s.ws.active_run.filter(|a| runs.contains(a)).or_else(|| runs.last().copied());
            let Some(id) = id else { return Ok(json!({ "closed": false })) };
            s.execute("run.closeTab", &json!({ "tab": id.0 }))?;
        }
        ToolWindowId::Terminal => {
            if s.terminal.tabs.is_empty() {
                return Ok(json!({ "closed": false }));
            }
            s.execute("terminal.close", &json!({}))?;
        }
        ToolWindowId::Find => {
            if let Some(f) = s.ws.find.take() {
                s.usages.recent.retain(|r| r.title != f.title);
            }
        }
        other => return Err(CommandError::Failed(format!("the {} tool window has no tabs", other.title()))),
    }
    Ok(json!({ "closed": true }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec("window.layouts.default", "Default", None, "Restore the default layout (Project on the left, nothing else open)", Context::Global, always, |s, _| {
            apply(s, &Layout::default());
            s.layouts.current = None;
            s.status = "Restored the default layout".into();
            ok()
        }),
        spec(
            "window.layouts.saveCurrentLayoutAsNew",
            "Save Current Layout as New…",
            None,
            "{name?: string} — save the open tool windows and their sizes as a named layout; without `name` asks",
            Context::Global,
            always,
            save_as_new,
        ),
        spec(
            "window.layouts.custom.restore",
            "Restore",
            None,
            "{name?: string} — restore a custom layout (default: the current one)",
            Context::Global,
            always,
            restore,
        ),
        spec(
            "window.layouts.custom.saveChanges",
            "Save Changes",
            None,
            "{name?: string} — overwrite a custom layout with the current tool windows",
            Context::Global,
            always,
            save_changes,
        ),
        spec(
            "window.layouts.custom.rename",
            "Rename",
            None,
            "{name?: string, newName?: string} — rename a custom layout",
            Context::Global,
            always,
            rename,
        ),
        spec("window.layouts.custom.delete", "Delete…", None, "{name?: string} — delete a custom layout", Context::Global, always, delete),
        spec("window.layouts.list", "Layouts", None, "→ {current, layouts: [name]}", Context::Global, always, |s, _| {
            let names = s.layouts.names();
            Ok(json!({ "current": s.layouts.current, "layouts": names, "error": s.layouts.saved.error }))
        }),
        spec("window.layouts.dialog.ok", "OK", None, "Run the Layouts dialog's action", Context::Global, |s| s.layouts.dialog.as_ref().map(|_| ()).ok_or_else(|| "the dialog is not open".into()), |s, _| {
            let d = s.layouts.dialog.clone().ok_or_else(|| CommandError::Failed("no dialog".into()))?;
            let name = d.selected.clone().unwrap_or_default();
            let r = match d.action {
                LayoutAction::SaveAsNew => save_as_new(s, &json!({ "name": d.name })),
                LayoutAction::Restore => restore(s, &json!({ "name": name })),
                LayoutAction::SaveChanges => save_changes(s, &json!({ "name": name })),
                LayoutAction::Rename => rename(s, &json!({ "name": name, "newName": d.name })),
                LayoutAction::Delete => delete(s, &json!({ "name": name })),
            };
            if let Err(e) = &r
                && let Some(d) = &mut s.layouts.dialog
            {
                d.error = Some(e.to_string());
            }
            r
        }),
        spec("window.layouts.dialog.cancel", "Cancel", None, "", Context::Global, always, |s, _| {
            s.layouts.dialog = None;
            ok()
        }),
        // ------------------------------------------------ Active Tool Window
        spec(
            "window.activeToolWindow.maximizeToolWindow",
            "Maximize Tool Window",
            None,
            "Let the active tool window fill the window (run again to restore)",
            Context::Global,
            has_active,
            |s, _| {
                let tw = active(s).map_err(CommandError::Disabled)?;
                if tw.is_lower_left() {
                    s.status = format!("The {} tool window shares the left side with Project and cannot be maximized", tw.title());
                    return Ok(json!({ "window": tw.key(), "maximized": false }));
                }
                let on = s.ws.layout.maximized != Some(tw);
                s.ws.layout.maximized = on.then_some(tw);
                s.ws.layout.size_rev = s.ws.layout.size_rev.wrapping_add(1);
                Ok(json!({ "window": tw.key(), "maximized": on }))
            },
        ),
        spec("window.activeToolWindow.selectNextTab", "Select Next Tab", None, "Next tab of the active tool window (Run, Build, Terminal, Find)", Context::Global, has_active, |s, _| select_tab(s, 1)),
        spec("window.activeToolWindow.selectPreviousTab", "Select Previous Tab", None, "Previous tab of the active tool window", Context::Global, has_active, |s, _| select_tab(s, -1)),
        spec("window.activeToolWindow.closeActiveTab", "Close Active Tab", None, "Close the active tool window's selected tab", Context::Global, has_active, |s, _| close_tab(s)),
        spec("window.activeToolWindow.groupTabs", "Group Tabs", None, "{on?: bool} — show tool window tabs as one grouped list (toggles)", Context::Global, always, |s, p| {
            let on = p.get("on").and_then(Value::as_bool).unwrap_or(!s.ws.layout.group_tabs);
            s.ws.layout.group_tabs = on;
            Ok(json!({ "on": on }))
        }),
        spec("window.activeToolWindow.resize.stretchToLeft", "Stretch to Left", None, "Move the active tool window's inner edge left", Context::Global, has_active, |s, _| stretch(s, Side::Left, None)),
        spec("window.activeToolWindow.resize.stretchToRight", "Stretch to Right", None, "Move the active tool window's inner edge right", Context::Global, has_active, |s, _| stretch(s, Side::Right, None)),
        spec("window.activeToolWindow.resize.stretchToTop", "Stretch to Top", None, "Make the bottom tool window taller", Context::Global, has_active, |s, _| stretch(s, Side::Bottom, Some(true))),
        spec("window.activeToolWindow.resize.stretchToBottom", "Stretch to Bottom", None, "Make the bottom tool window shorter", Context::Global, has_active, |s, _| stretch(s, Side::Bottom, Some(false))),
        spec("window.layouts.name", "Layout Name", None, "{name: string} — set the Layouts dialog's name field", Context::Global, |s| s.layouts.dialog.as_ref().map(|_| ()).ok_or_else(|| "the dialog is not open".into()), |s, p| {
            let name = req_str(p, "name")?.to_string();
            if let Some(d) = &mut s.layouts.dialog {
                d.name = name;
            }
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn save_restore_rename_delete_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.isolate_home(tmp.path());
        s.ws.layout.show(ToolWindowId::Terminal);
        s.ws.layout.show(ToolWindowId::Structure);
        s.ws.layout.set_size(ToolWindowId::Terminal, 333.0);
        s.execute("window.layouts.saveCurrentLayoutAsNew", &json!({ "name": "Debugging" })).unwrap();
        assert!(s.execute("window.layouts.saveCurrentLayoutAsNew", &json!({ "name": "Debugging" })).is_err());
        assert!(tmp.path().join(LAYOUTS_FILE).is_file());
        s.execute("window.layouts.default", &json!({})).unwrap();
        assert_eq!(s.ws.layout.bottom, None);
        assert_eq!(s.ws.layout.left, Some(ToolWindowId::Project));
        // Restore the current one (no name), from a fresh load.
        s.layouts.saved.reset();
        s.layouts.current = Some("Debugging".into());
        let rev = s.ws.layout.size_rev;
        s.execute("window.layouts.custom.restore", &json!({})).unwrap();
        assert_eq!(s.ws.layout.bottom, Some(ToolWindowId::Terminal));
        assert_eq!(s.ws.layout.left, Some(ToolWindowId::Structure));
        assert_eq!(s.ws.layout.current_size(ToolWindowId::Terminal), 333.0);
        assert!(s.ws.layout.size_rev > rev);
        // Save Changes, Rename, Delete.
        s.ws.layout.hide(ToolWindowId::Structure);
        s.execute("window.layouts.custom.saveChanges", &json!({})).unwrap();
        s.execute("window.layouts.custom.rename", &json!({ "newName": "Debug" })).unwrap();
        assert_eq!(s.layouts.current.as_deref(), Some("Debug"));
        let v = s.execute("window.layouts.list", &json!({})).unwrap();
        assert_eq!(v["layouts"], json!(["Debug"]));
        s.execute("window.layouts.custom.delete", &json!({})).unwrap();
        assert!(s.layouts.names().is_empty());
        // Without layouts the custom actions explain; without a current one they ask.
        assert!(s.execute("window.layouts.custom.restore", &json!({})).is_err());
        s.execute("window.layouts.saveCurrentLayoutAsNew", &json!({})).unwrap();
        assert_eq!(s.layouts.dialog.as_ref().unwrap().action, LayoutAction::SaveAsNew);
        s.execute("window.layouts.name", &json!({ "name": "A" })).unwrap();
        s.execute("window.layouts.dialog.ok", &json!({})).unwrap();
        s.layouts.current = None;
        assert_eq!(s.execute("window.layouts.custom.delete", &json!({})).unwrap()["dialog"], true);
        s.execute("window.layouts.dialog.ok", &json!({})).unwrap();
        assert!(s.layouts.names().is_empty());
    }

    #[test]
    fn active_tool_window_actions() {
        let mut s = Session::new();
        assert!(s.execute("window.activeToolWindow.resize.stretchToRight", &json!({})).is_ok(), "Project is open by default");
        s.ws.layout.show(ToolWindowId::Run);
        let before = s.ws.layout.current_size(ToolWindowId::Run);
        s.execute("window.activeToolWindow.resize.stretchToTop", &json!({})).unwrap();
        assert_eq!(s.ws.layout.current_size(ToolWindowId::Run), before + STRETCH_STEP);
        s.execute("window.activeToolWindow.resize.stretchToBottom", &json!({})).unwrap();
        s.execute("window.activeToolWindow.resize.stretchToBottom", &json!({})).unwrap();
        assert_eq!(s.ws.layout.current_size(ToolWindowId::Run), before - STRETCH_STEP);
        let v = s.execute("window.activeToolWindow.resize.stretchToLeft", &json!({})).unwrap();
        assert_eq!(v["changed"], false);
        for _ in 0..200 {
            s.execute("window.activeToolWindow.resize.stretchToBottom", &json!({})).unwrap();
        }
        assert_eq!(s.ws.layout.current_size(ToolWindowId::Run), 80.0);
        // Maximize toggles; hiding clears it.
        s.execute("window.activeToolWindow.maximizeToolWindow", &json!({})).unwrap();
        assert_eq!(s.ws.layout.maximized, Some(ToolWindowId::Run));
        s.execute("window.activeToolWindow.maximizeToolWindow", &json!({})).unwrap();
        assert_eq!(s.ws.layout.maximized, None);
        // The Spec View sits under Project: its size is a height, and it does not maximize.
        s.ws.layout.show(ToolWindowId::Spec);
        let height = s.ws.layout.current_size(ToolWindowId::Spec);
        let width = s.ws.layout.left_width;
        s.execute("window.activeToolWindow.resize.stretchToTop", &json!({})).unwrap();
        assert_eq!(s.ws.layout.current_size(ToolWindowId::Spec), height + STRETCH_STEP);
        assert_eq!(s.execute("window.activeToolWindow.resize.stretchToRight", &json!({})).unwrap()["changed"], false);
        assert_eq!(s.ws.layout.left_width, width);
        assert_eq!(s.execute("window.activeToolWindow.maximizeToolWindow", &json!({})).unwrap()["maximized"], false);
        assert_eq!(s.ws.layout.maximized, None);
        s.ws.layout.hide(ToolWindowId::Spec);
        s.ws.layout.show(ToolWindowId::Run);
        // Run tabs.
        let a = s.ws.add_run("a", "a", false);
        let b = s.ws.add_run("b", "b", false);
        s.ws.active_run = Some(a);
        s.execute("window.activeToolWindow.selectNextTab", &json!({})).unwrap();
        assert_eq!(s.ws.active_run, Some(b));
        s.execute("window.activeToolWindow.selectNextTab", &json!({})).unwrap();
        assert_eq!(s.ws.active_run, Some(a));
        s.execute("window.activeToolWindow.selectPreviousTab", &json!({})).unwrap();
        assert_eq!(s.ws.active_run, Some(b));
        s.execute("window.activeToolWindow.closeActiveTab", &json!({})).unwrap();
        assert_eq!(s.ws.runs.len(), 1);
        s.execute("window.activeToolWindow.groupTabs", &json!({})).unwrap();
        assert!(s.ws.layout.group_tabs);
        // A tool window without tabs says so.
        s.ws.layout.show(ToolWindowId::Problems);
        assert!(s.execute("window.activeToolWindow.selectNextTab", &json!({})).is_err());
        s.ws.layout.all_hidden = true;
        assert!(s.enabled("window.activeToolWindow.maximizeToolWindow").is_err());
    }
}
