//! The editor's right-click menu: Send to Chat, the language's submenus and the Git menu. What
//! is offered comes from `codecraft_engine::editor_menu`; this renders it. The enabled states are
//! sampled when the right button goes down, because the menu is drawn while the document is
//! borrowed and cannot ask the session.

use egui::{Button, Id, RichText};
use serde_json::Value;

use codecraft_engine::editor_menu::editor_groups;
use codecraft_engine::{Session, registry};

use crate::theme::Theme;
use crate::{keymap, menus};

type Cmd = (&'static str, Value);

#[derive(Clone)]
struct Item {
    id: &'static str,
    label: String,
    /// Why it is disabled (`None` = enabled).
    disabled: Option<String>,
    shortcut: Option<String>,
}

#[derive(Clone)]
pub struct Snapshot {
    send_to_chat: Result<(), String>,
    groups: Vec<(&'static str, Vec<Item>)>,
}

fn menu_id(editor: Id) -> Id {
    editor.with("editor-context-menu")
}

/// Sample the menu's state for the active editor. Call before the document is borrowed.
pub fn sample(ui: &egui::Ui, s: &Session, editor: Id) {
    if !ui.input(|i| i.pointer.secondary_pressed()) {
        return;
    }
    let Some((_, doc)) = s.ws.active() else { return };
    let groups = editor_groups(doc.language)
        .into_iter()
        .map(|g| {
            let items = g
                .ids
                .into_iter()
                .map(|id| Item {
                    id,
                    label: registry().get(id).map(|c| c.label.to_string()).unwrap_or_else(|| id.to_string()),
                    disabled: s.enabled(id).err(),
                    shortcut: menus::raw_shortcut(id).map(|sc| keymap::display(&sc)),
                })
                .collect();
            (g.title, items)
        })
        .collect();
    let snap = Snapshot { send_to_chat: s.enabled("agent.sendToChat"), groups };
    ui.data_mut(|d| d.insert_temp(menu_id(editor), snap));
}

/// The part of the menu below Cut/Copy/Paste. Commands are queued in `commands`.
pub fn contents(ui: &mut egui::Ui, theme: &Theme, editor: Id, commands: &mut Vec<Cmd>) {
    let Some(snap) = ui.data(|d| d.get_temp::<Snapshot>(menu_id(editor))) else { return };
    let chat = ui.add_enabled(snap.send_to_chat.is_ok(), Button::new("Send to Chat"));
    let chat = match &snap.send_to_chat {
        Err(why) => chat.on_disabled_hover_text(why.clone()),
        Ok(()) => chat.on_hover_text("Add the selected code to the Agent prompt"),
    };
    if chat.clicked() {
        commands.push(("agent.sendToChat", serde_json::json!({})));
        ui.close();
    }
    ui.separator();
    for (title, items) in &snap.groups {
        ui.menu_button(*title, |ui| {
            for item in items {
                let mut button = Button::new(item.label.as_str());
                if let Some(sc) = &item.shortcut {
                    button = button.shortcut_text(RichText::new(sc.as_str()).color(theme.text_muted));
                }
                let resp = ui.add_enabled(item.disabled.is_none(), button);
                let resp = match &item.disabled {
                    Some(why) => resp.on_disabled_hover_text(why.clone()),
                    None => resp,
                };
                if resp.clicked() {
                    commands.push((item.id, crate::platform_params(item.id)));
                    ui.close();
                }
            }
        });
    }
}
