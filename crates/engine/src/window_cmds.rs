//! Window › Editor Tabs commands and tab/run-tab management.

use codecraft_workspace::{EditorId, RunTabId};

use crate::Session;
use crate::commands::{CmdResult, CommandSpec, Context, has_tabs, ok, simple, spec};
use crate::params::req_usize;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("window.editorTabs.closeTab", "Close Tab", has_tabs, |s, _| {
            if let Some(id) = s.ws.active_editor() {
                close(s, id);
            }
            ok()
        }),
        simple("window.editorTabs.closeOtherTabs", "Close Other Tabs", has_tabs, |s, _| {
            close_where(s, |s, id| Some(id) != s.ws.active_editor())
        }),
        simple("window.editorTabs.closeAllTabs", "Close All Tabs", has_tabs, |s, _| close_where(s, |_, _| true)),
        simple("window.editorTabs.closeUnmodifiedTabs", "Close Unmodified Tabs", has_tabs, |s, _| {
            close_where(s, |s, id| !s.ws.editor(id).and_then(|e| s.ws.doc(e.doc)).is_some_and(|d| d.doc.is_modified()))
        }),
        simple("window.editorTabs.closeTabsToTheLeft", "Close Tabs to the Left", has_tabs, |s, _| close_side(s, true)),
        simple("window.editorTabs.closeTabsToTheRight", "Close Tabs to the Right", has_tabs, |s, _| {
            close_side(s, false)
        }),
        simple("window.editorTabs.selectNextTab", "Select Next Tab", has_tabs, |s, _| cycle(s, 1)),
        simple("window.editorTabs.selectPreviousTab", "Select Previous Tab", has_tabs, |s, _| cycle(s, -1)),
        simple("window.editorTabs.pinTab", "Pin Tab", has_tabs, |s, _| {
            if let Some(id) = s.ws.active_editor()
                && let Some(e) = s.ws.editor_mut(id)
            {
                e.pinned = !e.pinned;
            }
            ok()
        }),
        spec("window.editorTabs.activate", "Activate Tab", None, "{editor: int}", Context::Global, has_tabs, |s, p| {
            let id = EditorId(req_usize(p, "editor")? as u64);
            if s.ws.set_active_editor(id) {
                s.effects.push(crate::Effect::FocusEditor);
            }
            ok()
        }),
        spec("window.editorTabs.close", "Close Tab by Id", None, "{editor: int}", Context::Global, has_tabs, |s, p| {
            let id = EditorId(req_usize(p, "editor")? as u64);
            close(s, id);
            ok()
        }),
        spec("run.closeTab", "Close Run Tab", None, "{tab: int}", Context::Global, crate::commands::always, |s, p| {
            let id = RunTabId(req_usize(p, "tab")? as u64);
            crate::runs::stop(s, id);
            s.ws.close_run(id);
            ok()
        }),
    ]
}

/// Close a tab; pinned tabs stay unless closed explicitly by id. Notifies the language server.
pub(crate) fn close(s: &mut Session, id: EditorId) {
    if let Some(od) = s.ws.close_editor(id) {
        if od.doc.is_modified()
            && !crate::hot_exit::keep_on_close(s, od.id)
            && let Some(path) = &od.path
            && let Err(e) = crate::misc::encoding::write_document(path, &od)
        {
            // RustRover saves on close; if that fails keep the user informed.
            s.status = format!("Cannot save {}: {e}", path.display());
        }
        if let Some(path) = &od.path {
            s.lsp_did_close(path);
        }
    }
}

/// Close the focused group's unpinned tabs matching `pred`.
pub(crate) fn close_where(s: &mut Session, pred: impl Fn(&Session, EditorId) -> bool) -> CmdResult {
    let ids: Vec<EditorId> =
        s.ws.tabs()
            .iter()
            .copied()
            .filter(|id| !s.ws.editor(*id).is_some_and(|e| e.pinned))
            .filter(|id| pred(s, *id))
            .collect();
    for id in ids {
        close(s, id);
    }
    ok()
}

fn close_side(s: &mut Session, left: bool) -> CmdResult {
    let Some(active) = s.ws.active_editor() else { return ok() };
    let Some(idx) = s.ws.tabs().iter().position(|t| *t == active) else { return ok() };
    let ids: Vec<EditorId> =
        s.ws.tabs()
            .iter()
            .enumerate()
            .filter(|(i, _)| if left { *i < idx } else { *i > idx })
            .map(|(_, id)| *id)
            .filter(|id| !s.ws.editor(*id).is_some_and(|e| e.pinned))
            .collect();
    for id in ids {
        close(s, id);
    }
    ok()
}

fn cycle(s: &mut Session, delta: isize) -> CmdResult {
    let n = s.ws.tabs().len();
    if n == 0 {
        return ok();
    }
    let idx = s.ws.active_editor().and_then(|a| s.ws.tabs().iter().position(|t| *t == a)).unwrap_or(0);
    let next = (idx as isize + delta).rem_euclid(n as isize) as usize;
    if let Some(id) = s.ws.tabs().get(next).copied() {
        s.ws.set_active_editor(id);
    }
    s.effects.push(crate::Effect::FocusEditor);
    ok()
}
