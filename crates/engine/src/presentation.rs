//! View › Appearance › Presentation Assistant: while it is on, every action invoked (from a menu,
//! a shortcut, Find Action or an agent) is announced with its name and shortcut in an overlay at
//! the bottom of the window, for screencasts and talks.
//!
//! The engine records the last actions (`Session::execute` reports every top-level command); the
//! UI shows the newest one for a few seconds with the shortcut of the active keymap.

use std::collections::VecDeque;
use std::time::Instant;

use serde_json::{Value, json};

use crate::Session;
use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::bool_or;

/// Actions remembered.
const HISTORY: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    pub id: String,
    pub label: String,
    /// The menu path ("Edit › Find"), when the action is in a menu.
    pub menu: Option<String>,
    pub at: Instant,
}

#[derive(Debug, Default)]
pub struct PresentationState {
    pub enabled: bool,
    pub recent: VecDeque<Announcement>,
}

impl PresentationState {
    pub fn latest(&self) -> Option<&Announcement> {
        self.recent.back()
    }
}

/// Typing, caret motion and internal plumbing are not announced (they are not "actions").
fn announced(id: &str) -> bool {
    let reg = crate::registry();
    if reg.catalog_item(id).is_some() {
        return true;
    }
    !id.starts_with("editor.") && !reg.shortcuts(id).is_empty()
}

/// Called by `Session::execute` after a top-level command succeeded.
pub(crate) fn note(s: &mut Session, id: &str) {
    if !s.presentation.enabled || !announced(id) {
        return;
    }
    let reg = crate::registry();
    let label = reg
        .catalog_item(id)
        .map(|c| c.label.to_string())
        .or_else(|| reg.get(id).map(|c| c.label.to_string()))
        .unwrap_or_else(|| id.to_string());
    let menu = reg.menu_path(id).map(|p| p.join(" › "));
    let recent = &mut s.presentation.recent;
    recent.push_back(Announcement { id: id.to_string(), label: label.trim_end_matches('…').to_string(), menu, at: Instant::now() });
    while recent.len() > HISTORY {
        recent.pop_front();
    }
}

fn toggle(s: &mut Session, p: &Value) -> CmdResult {
    let on = bool_or(p, "on", !s.presentation.enabled);
    s.presentation.enabled = on;
    if !on {
        s.presentation.recent.clear();
    }
    s.status = format!("Presentation Assistant {}", if on { "on" } else { "off" });
    Ok(json!({ "on": on }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "view.appearance.presentationAssistant",
            "Presentation Assistant",
            None,
            "{on?: bool} — show the name and shortcut of each invoked action in an overlay (toggles)",
            Context::Global,
            always,
            toggle,
        ),
        spec(
            "presentationAssistant.recent",
            "Presentation Assistant Actions",
            None,
            "→ the last announced actions [{id, label, menu, shortcuts}]",
            Context::Global,
            always,
            |s, _| {
                let reg = crate::registry();
                let list: Vec<Value> = s
                    .presentation
                    .recent
                    .iter()
                    .map(|a| json!({ "id": a.id, "label": a.label, "menu": a.menu, "shortcuts": reg.shortcuts(&a.id) }))
                    .collect();
                Ok(json!({ "on": s.presentation.enabled, "actions": list }))
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn announces_actions_not_typing() {
        let mut s = Session::new();
        let d = s.ws.add_document(None, "abc");
        s.ws.open_editor(d);
        s.execute("view.toggleLineNumbers", &json!({})).unwrap();
        assert!(s.presentation.recent.is_empty());
        s.execute("view.appearance.presentationAssistant", &json!({})).unwrap();
        assert!(s.presentation.recent.back().is_some_and(|a| a.id == "view.appearance.presentationAssistant"));
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        s.execute("editor.right", &json!({})).unwrap();
        s.execute("edit.selectAll", &json!({})).unwrap();
        let a = s.presentation.latest().unwrap();
        assert_eq!(a.id, "edit.selectAll");
        assert_eq!(a.label, "Select All");
        assert_eq!(a.menu.as_deref(), Some("Edit"));
        let v = s.execute("presentationAssistant.recent", &json!({})).unwrap();
        assert_eq!(v["actions"].as_array().unwrap().len(), 2);
        assert_eq!(v["actions"][1]["shortcuts"][0], "Cmd+A");
        s.execute("view.appearance.presentationAssistant", &json!({ "on": false })).unwrap();
        assert!(s.presentation.recent.is_empty());
    }
}
