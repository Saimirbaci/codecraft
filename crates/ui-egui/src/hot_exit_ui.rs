//! The "restored unsaved changes" banner above an editor whose buffer came back from hot exit
//! (`codecraft_engine::hot_exit`): Compare with Disk, Keep, Discard. A file that changed on disk
//! meanwhile is flagged in the warning colour and is not saved over until kept or discarded.

use egui::{Frame, Margin, RichText, Stroke};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::hot_exit::Conflict;
use codecraft_workspace::EditorId;

use crate::{UiState, run_command};

/// The banner for the editor `eid`, if its document has restored changes.
pub(crate) fn banner(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, eid: EditorId) {
    let Some(doc) = s.ws.editor(eid).map(|e| e.doc) else { return };
    let Some(restored) = s.hot_exit.restored.get(&doc).cloned() else { return };
    let has_path = s.ws.doc(doc).is_some_and(|d| d.path.is_some());
    let theme = st.theme.clone();
    let (mark, color, text) = match restored.conflict {
        Conflict::None => (
            "⟲",
            theme.accent,
            if has_path {
                "Restored unsaved changes from the last session.".to_string()
            } else {
                "Restored this untitled buffer from the last session.".to_string()
            },
        ),
        Conflict::ChangedOnDisk => (
            "⚠",
            theme.warning,
            "Restored unsaved changes, but the file changed on disk since. It is not saved over until you keep or \
             discard them."
                .to_string(),
        ),
        Conflict::DeletedOnDisk => (
            "⚠",
            theme.warning,
            "Restored unsaved changes of a file that no longer exists on disk. Saving recreates it once you keep \
             them."
                .to_string(),
        ),
    };
    let mut cmd: Option<(&'static str, Value)> = None;
    Frame::new()
        .fill(theme.header_bg)
        .inner_margin(Margin::symmetric(8, 4))
        .stroke(Stroke::new(1.0, theme.border))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                // Not selectable: a wrapped label would otherwise take the links' clicks.
                ui.add(egui::Label::new(RichText::new(mark).color(color)).selectable(false));
                ui.add(egui::Label::new(RichText::new(text).color(theme.text)).selectable(false));
                let age = codecraft_engine::agent_sessions::age_label(
                    codecraft_engine::agent_sessions::now_ms(),
                    restored.saved_ms,
                );
                ui.add(
                    egui::Label::new(RichText::new(format!("(kept {age})")).small().color(theme.text_muted))
                        .selectable(false),
                );
                if has_path
                    && ui
                        .link("Compare with Disk")
                        .on_hover_text("Show the unsaved changes against the file on disk")
                        .clicked()
                {
                    cmd = Some(("hotExit.compareWithDisk", json!({ "doc": doc.0 })));
                }
                let keep = if restored.conflict == Conflict::None { "Dismiss" } else { "Keep My Changes" };
                if ui.link(keep).on_hover_text("Keep the restored text; saving writes it to the file").clicked() {
                    cmd = Some(("hotExit.keepChanges", json!({ "doc": doc.0 })));
                }
                let tip = if has_path { "Go back to the file on disk" } else { "Close this buffer" };
                if ui.link(RichText::new("Discard").color(theme.error)).on_hover_text(tip).clicked() {
                    cmd = Some(("hotExit.discard", json!({ "doc": doc.0 })));
                }
            });
        });
    if let Some((id, p)) = cmd {
        run_command(s, st, id, &p);
    }
}
