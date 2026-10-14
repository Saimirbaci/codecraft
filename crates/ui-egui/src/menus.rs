//! The menu bar, generated from the observed RustRover menu catalog. Items whose command is
//! registered are live; planned ones render disabled; out-of-scope ones are hidden.

use egui::{Button, Color32, RichText};

use codecraft_engine::appearance;
use codecraft_engine::catalog::CATALOG;
use codecraft_engine::{CatalogItem, Scope, Session, registry};

use crate::{UiState, keymap, run_command};

/// Commands without an observed menu entry (or whose observed submenu is filled lazily by
/// RustRover), placed where RustRover shows them.
pub(crate) const EXTRA: &[(&str, &str)] = &[
    ("View", "view.quickDocumentation"),
    ("View › Active Editor", "view.toggleSoftWrap"),
    ("View › Active Editor", "view.toggleWhitespace"),
    ("View", "view.toggleLineNumbers"),
    ("Code", "code.showContextActions"),
    // RustRover's Refactor menu fills Extract/Inline/Change Signature for the caret's context.
    ("Refactor", "refactor.extract.variable"),
    ("Refactor", "refactor.extract.constant"),
    ("Refactor", "refactor.extract.function"),
    ("Refactor", "refactor.inline"),
    ("Refactor", "refactor.changeSignature"),
    ("Refactor", "refactor.renameFile"),
    // RustRover fills Git › Current File lazily, so the observation captured it empty.
    ("Git › Current File", "git.currentFile.annotate"),
    ("Git › Current File", "git.currentFile.showHistory"),
    ("Git › Current File", "git.currentFile.compareWithRevision"),
    ("Git › Current File", "git.currentFile.compareWithBranch"),
    ("Git › Current File", "git.currentFile.rollback"),
    ("Run", "debug.atCaret"),
    ("Run", "debug.coreDump"),
    ("Run", "debug.remote"),
    ("Run › Debugging Actions", "debug.disassembly.stepOverInstruction"),
    ("Run › Debugging Actions", "debug.disassembly.stepIntoInstruction"),
    ("Code", "code.showAssembly"),
    ("Build", "cargo.check"),
    ("Build", "cargo.clippy"),
    ("Build", "cargo.doc"),
    ("Run", "run.contextConfiguration"),
    ("Run", "cargo.test"),
    ("Run", "run.rerun"),
    // RustRover fills Run › Test History lazily (recent runs); ours opens the history dialog.
    ("Run › Test History", "run.testHistory"),
    ("Run › Test History", "run.importTestsFromFile"),
    // Edit › Bookmarks as observed lacks the toggles (RustRover adds them for the caret's line).
    ("Edit › Bookmarks", "edit.bookmarks.toggle"),
    ("Edit › Bookmarks", "edit.bookmarks.toggleWithMnemonic"),
    ("Edit › Bookmarks", "edit.bookmarks.editDescription"),
    ("File", "file.scratches"),
    // File › New › From Template… (the templates saved with File › Save File as Template…).
    ("File › New", "file.newFromTemplate"),
];

pub(crate) fn visible(item: &CatalogItem) -> bool {
    // CLion's Valgrind import is implemented (track CL) although the observed install lists it
    // as a third-party plugin item.
    !matches!(item.scope, Scope::Out | Scope::Os) || item.id == "run.importValgrindXmlResults"
}

fn children<'a>(path: &'a [&'a str]) -> impl Iterator<Item = &'static CatalogItem> + 'a {
    CATALOG.iter().filter(move |c| c.path == path && visible(c))
}

pub(crate) fn label_for(item: &CatalogItem) -> String {
    match item.id {
        "app.aboutRustRover" => "About CodeCraft".into(),
        "app.quitRustRover" => "Quit CodeCraft".into(),
        "run.run" | "run.debug" | "run.runWithCoverage" | "run.profile" | "run.runWithValgrindMemcheck" => {
            item.label.replace("'Run'", "")
        }
        // The observed label carries the zoom at capture time; the live one is `dynamic_label`.
        "view.appearance.zoomIde" => "Zoom IDE…".into(),
        _ => item.label.to_string(),
    }
}

/// An EXTRA command that the catalog already shows under `path` (by its id or an old id of it).
fn in_catalog_here(path: &[&str], id: &str) -> bool {
    let id = registry().canonical(id);
    children(path).any(|c| registry().canonical(c.id) == id)
}

/// The shortcut shown for a command in the menus (in-window and native), in RustRover notation
/// (`Shift+Cmd+F`): the command's effective shortcut, else the catalog's. The one place menus read
/// shortcuts, so keymap changes reach both menu bars through it.
pub(crate) fn raw_shortcut(id: &str) -> Option<String> {
    // Live commands follow the active keymap and the user's changes (which may remove it).
    if registry().contains(id) {
        return keymap::active_shortcuts(id).into_iter().next();
    }
    registry().catalog_item(id).and_then(|c| c.shortcut).map(str::to_string)
}

fn item_button(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, id: &str, label: &str) {
    let live = registry().contains(id);
    let enabled = live && s.enabled(id).is_ok();
    let label = appearance::dynamic_label(s, id).unwrap_or_else(|| label.to_string());
    // Toggles get a checkmark column (blank while off, so the labels stay aligned).
    let mut button = match appearance::checked(s, id) {
        Some(on) => {
            let mark = RichText::new("✓").color(if on { st.theme.text } else { Color32::TRANSPARENT });
            Button::new((mark, label))
        }
        None => Button::new(label),
    };
    if let Some(sc) = raw_shortcut(id) {
        button = button.shortcut_text(RichText::new(keymap::display(&sc)).color(st.theme.text_muted));
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
        run_command(s, st, id, &crate::platform_params(id));
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
        // An EXTRA command the catalog already shows here is not repeated.
        let extras: Vec<&str> =
            EXTRA.iter().filter(|(m, id)| *m == key && !in_catalog_here(path, id)).map(|(_, id)| *id).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_entries_are_registered_ids_the_catalog_does_not_show_there() {
        for (path, id) in EXTRA {
            assert!(registry().all().iter().any(|s| s.id == *id), "EXTRA {id} is not a registered id (an alias?)");
            let path: Vec<&str> = path.split(" › ").collect();
            assert!(!in_catalog_here(&path, id), "EXTRA {id} is already in the catalog under {path:?}");
        }
    }
}
