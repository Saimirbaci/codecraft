//! The menu bar, generated from the observed RustRover menu catalog. Items whose command is
//! registered are live; planned ones render disabled; out-of-scope ones are hidden.

use egui::{Button, RichText};
use serde_json::Value;

use codecraft_engine::catalog::CATALOG;
use codecraft_engine::{CatalogItem, Scope, Session, registry};

use crate::{UiState, keymap, run_command};

/// Commands without an observed menu entry (or whose observed submenu is filled lazily by
/// RustRover), placed where RustRover shows them.
const EXTRA: &[(&str, &str)] = &[
    ("Edit", "edit.findUsages.findUsages"),
    ("View", "view.quickDocumentation"),
    ("View", "view.hideAllToolWindows"),
    ("View", "view.toggleSoftWrap"),
    ("View", "view.toggleWhitespace"),
    ("View", "view.toggleLineNumbers"),
    ("Code", "code.showContextActions"),
    ("Code › Folding", "code.folding.expand"),
    ("Code › Folding", "code.folding.collapse"),
    ("Code › Folding", "code.folding.expandAll"),
    ("Code › Folding", "code.folding.collapseAll"),
    // RustRover fills Run › Debugging Actions lazily, so the observation captured it empty.
    ("Run › Debugging Actions", "run.debuggingActions.resume"),
    ("Run › Debugging Actions", "run.debuggingActions.pause"),
    ("Run › Debugging Actions", "run.debuggingActions.stepOver"),
    ("Run › Debugging Actions", "run.debuggingActions.stepInto"),
    ("Run › Debugging Actions", "run.debuggingActions.forceStepInto"),
    ("Run › Debugging Actions", "run.debuggingActions.stepOut"),
    ("Run › Debugging Actions", "run.debuggingActions.runToCursor"),
    ("Run › Debugging Actions", "run.debuggingActions.evaluateExpression"),
    ("Run › Debugging Actions", "debug.stop"),
    ("Run", "debug.atCaret"),
    ("Build", "cargo.check"),
    ("Build", "cargo.clippy"),
    ("Build", "cargo.doc"),
    ("Run", "run.contextConfiguration"),
    ("Run", "cargo.test"),
    ("Run", "run.rerun"),
    ("Help", "help.findAction"),
];

fn visible(item: &CatalogItem) -> bool {
    !matches!(item.scope, Scope::Out | Scope::Os)
}

fn children<'a>(path: &'a [&'a str]) -> impl Iterator<Item = &'static CatalogItem> + 'a {
    CATALOG.iter().filter(move |c| c.path == path && visible(c))
}

fn label_for(item: &CatalogItem) -> String {
    match item.id {
        "app.aboutRustRover" => "About CodeCraft".into(),
        "app.quitRustRover" => "Quit CodeCraft".into(),
        "run.run" | "run.debug" | "run.runWithCoverage" | "run.profile" | "run.runWithValgrindMemcheck" => {
            item.label.replace("'Run'", "")
        }
        _ => item.label.to_string(),
    }
}

fn item_button(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, id: &str, label: &str) {
    let live = registry().contains(id);
    let enabled = live && s.enabled(id).is_ok();
    let mut button = Button::new(label);
    if let Some(sc) =
        keymap::shortcut_label(id).or_else(|| registry().catalog_item(id).and_then(|c| c.shortcut).map(keymap::display))
    {
        button = button.shortcut_text(RichText::new(sc).color(st.theme.text_muted));
    }
    let resp = ui.add_enabled(enabled, button);
    let resp = if !live {
        resp.on_disabled_hover_text("Not implemented yet")
    } else if let Err(reason) = s.enabled(id) {
        resp.on_disabled_hover_text(reason)
    } else {
        resp
    };
    if resp.clicked() {
        ui.close();
        run_command(s, st, id, &Value::Null);
    }
}

fn menu_items(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, path: &[&str]) {
    let mut pending_sep = false;
    let mut any = false;
    for item in children(path) {
        if item.id == "---" {
            pending_sep = any;
            continue;
        }
        if pending_sep {
            ui.separator();
            pending_sep = false;
        }
        any = true;
        if item.submenu {
            let mut sub: Vec<&str> = path.to_vec();
            sub.push(item.label);
            let key = sub.join(" › ");
            let has_children = children(&sub).any(|c| c.id != "---") || EXTRA.iter().any(|(m, _)| *m == key);
            if has_children {
                ui.menu_button(item.label, |ui| menu_items(ui, s, st, &sub));
            } else {
                ui.add_enabled(false, Button::new(item.label)).on_disabled_hover_text("Not implemented yet");
            }
        } else {
            item_button(ui, s, st, item.id, &label_for(item));
        }
    }
    {
        let key = path.join(" › ");
        let extras: Vec<&str> = EXTRA.iter().filter(|(m, _)| *m == key).map(|(_, id)| *id).collect();
        if !extras.is_empty() {
            if any {
                ui.separator();
            }
            for id in extras {
                let label = registry().get(id).map(|s| s.label).unwrap_or(id);
                item_button(ui, s, st, id, label);
            }
        }
    }
}

pub fn menu_bar_contents(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let mut tops: Vec<&'static str> = Vec::new();
    for c in CATALOG {
        if let Some(t) = c.path.first()
            && !tops.contains(t)
        {
            tops.push(t);
        }
    }
    for top in tops {
        let title = if top == "RustRover" { "CodeCraft" } else { top };
        ui.menu_button(title, |ui| {
            ui.set_min_width(260.0);
            menu_items(ui, s, st, &[top]);
        });
    }
}
