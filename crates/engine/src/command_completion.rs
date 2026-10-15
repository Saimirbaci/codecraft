//! Code › Code Completion › Enable Command Completion (M13 track RC). While it is on, typing `..`
//! in the editor pops up the IDE actions that apply here (enabled Code, Refactor, Navigate, Edit
//! and Run-at-caret actions) as a completion list; typing filters it like completion, Enter runs
//! the chosen action after removing the `..` and the filter text, Escape keeps what was typed.

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::popup::{CompletionItem, CompletionPopup, Popup};
use crate::{CommandError, Session, registry};

/// The kind of command items in the completion list.
pub const ACTION_KIND: &str = "action";
const MAX_ITEMS: usize = 300;

/// Menus whose actions are offered.
const MENUS: &[&str] = &["Code", "Refactor", "Navigate", "Edit"];
/// Single actions from other menus that act on the caret.
const EXTRA: &[&str] = &["run.contextConfiguration", "debug.atCaret", "run.toggleBreakpoint.lineBreakpoint"];

fn offered(id: &str) -> bool {
    if id == "code.codeCompletion.enableCommandCompletion" || id.starts_with("edit.find.") && id.contains("Next") {
        return false;
    }
    if EXTRA.contains(&id) {
        return true;
    }
    registry().menu_path(id).and_then(|p| p.first()).is_some_and(|top| MENUS.contains(top))
}

/// The enabled actions offered at the caret, as completion items (label, menu path and shortcut).
pub fn items(s: &Session) -> Vec<CompletionItem> {
    let reg = registry();
    let mut out = Vec::new();
    for spec in reg.all() {
        if !offered(spec.id) || s.enabled(spec.id).is_err() {
            continue;
        }
        let label = reg.catalog_item(spec.id).map(|c| c.label).unwrap_or(spec.label);
        let label = label.trim_end_matches('…').trim_end_matches("...").to_string();
        let mut detail: Vec<String> = Vec::new();
        if let Some(path) = reg.menu_path(spec.id) {
            detail.push(path.join(" › "));
        }
        if let Some(sc) = reg.shortcuts(spec.id).first() {
            detail.push(sc.to_string());
        }
        out.push(CompletionItem {
            label: label.clone(),
            detail: (!detail.is_empty()).then(|| detail.join("  ")),
            kind: ACTION_KIND.into(),
            filter_text: label.clone(),
            sort_text: label.to_lowercase(),
            insert_text: String::new(),
            resolved: true,
            lsp: Some(json!({ "command": spec.id })),
            documentation: Some(spec.id.to_string()),
            ..Default::default()
        });
        if out.len() >= MAX_ITEMS {
            break;
        }
    }
    out.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    out
}

/// After a typed char: `..` with command completion on opens the action list. True when it did.
pub(crate) fn after_typed(s: &mut Session, typed: char) -> bool {
    if typed != '.' || !s.ws.settings.command_completion || s.popup.is_some() {
        return false;
    }
    let Some((e, d)) = s.ws.active() else { return false };
    if e.selections.len() != 1 {
        return false;
    }
    let caret = e.selections.primary().head;
    if caret < 2 || d.doc.char_at(caret - 2) != Some('.') {
        return false;
    }
    // `...` (a third dot) is Rust's old range pattern or a spread: not a request.
    if caret >= 3 && d.doc.char_at(caret - 3) == Some('.') {
        return false;
    }
    let editor = e.id;
    let items = items(s);
    if items.is_empty() {
        return false;
    }
    s.popup = Some(Popup::Completion(CompletionPopup {
        editor,
        anchor: caret,
        visible: (0..items.len()).collect(),
        items,
        header: Some("Actions (command completion)".into()),
        ..Default::default()
    }));
    crate::completion::refresh_filter(s);
    true
}

/// Accepting a command item: remove `..` and the filter text, then run the action.
pub(crate) fn accept(s: &mut Session, popup: &CompletionPopup, item: &CompletionItem) -> Option<CmdResult> {
    if item.kind != ACTION_KIND {
        return None;
    }
    let id = item.lsp.as_ref().and_then(|v| v.get("command")).and_then(Value::as_str)?.to_string();
    let Some((e, d)) = s.ws.active() else { return Some(Ok(Value::Null)) };
    if e.id != popup.editor {
        return Some(Ok(Value::Null));
    }
    let caret = e.selections.primary().head.min(d.doc.len_chars());
    let start = popup.anchor.saturating_sub(2).min(caret);
    if d.doc.slice(start..start.saturating_add(2).min(caret)) == ".." {
        let removed = s.edit_active(codecraft_text::EditGroup::Standalone, |_, _, _| codecraft_text::Edit {
            changes: codecraft_text::ChangeSet::single(start, caret, String::new()),
            selections: codecraft_text::SelectionSet::caret(start),
        });
        if let Err(e) = removed {
            return Some(Err(e));
        }
    }
    Some(match s.execute(&id, &Value::Null) {
        Ok(v) => Ok(json!({ "command": id, "result": v })),
        Err(e) => Err(CommandError::Failed(format!("{}: {e}", item.label))),
    })
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "code.codeCompletion.enableCommandCompletion",
        "Enable Command Completion",
        None,
        "{value?: bool} — toggle: typing `..` in the editor lists the actions available at the caret; Enter runs one",
        Context::Global,
        always,
        |s, p| {
            let on = p.get("value").and_then(Value::as_bool).unwrap_or(!s.ws.settings.command_completion);
            s.ws.settings.command_completion = on;
            s.status = if on {
                "Command completion on: type .. in the editor to see actions".into()
            } else {
                "Command completion off".into()
            };
            Ok(json!(on))
        },
    )]
}
