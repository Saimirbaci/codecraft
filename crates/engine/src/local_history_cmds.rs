//! File › Local History commands and the history window's actions.

use serde_json::{Value, json};

use crate::Session;
use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, has_project, ok, simple, spec};
use crate::local_history::{self, ViewMode};
use crate::params::{opt_path, opt_str, opt_usize, req_usize};

fn failed(e: String) -> CommandError {
    CommandError::Failed(e)
}

/// `{path}` or the active editor's file.
fn file_param(s: &Session, p: &Value) -> Result<std::path::PathBuf, CommandError> {
    if let Some(path) = opt_path(p, "path") {
        return Ok(crate::normalize_path(&path));
    }
    s.ws.active()
        .and_then(|(_, d)| d.path.clone())
        .ok_or_else(|| CommandError::Failed("no saved file is open".into()))
}

fn view_json(s: &Session) -> Value {
    match &s.local_history.view {
        Some(v) => json!({
            "title": v.title,
            "selected": v.selected,
            "revisions": v.revisions.iter().map(|r| json!({
                "id": r.id,
                "time": r.time_ms,
                "kind": r.kind,
                "path": r.path,
                "label": r.label,
                "description": local_history::describe(r),
            })).collect::<Vec<_>>(),
            "error": v.error,
        }),
        None => Value::Null,
    }
}

fn show(s: &mut Session, mode: ViewMode) -> CmdResult {
    local_history::show(s, mode).map_err(failed)?;
    Ok(view_json(s))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "file.localHistory.showHistory",
            "Show History…",
            None,
            "{path?} (default: the active file); returns the revisions",
            Context::Global,
            has_project,
            |s, p| {
                let path = file_param(s, p)?;
                show(s, ViewMode::File { path })
            },
        ),
        spec(
            "file.localHistory.showHistoryForSelection",
            "Show History for Selection…",
            None,
            "{path?, startLine?, endLine?} (default: the active editor's selected lines)",
            Context::Global,
            has_project,
            |s, p| {
                let path = file_param(s, p)?;
                let (mut start, mut end) = (opt_usize(p, "startLine")?, opt_usize(p, "endLine")?);
                if (start.is_none() || end.is_none())
                    && let Some((e, d)) = s.ws.active().filter(|(_, d)| d.path.as_deref() == Some(path.as_path()))
                {
                    let sel = e.selections.primary();
                    let (a, b) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
                    start = start.or(Some(d.doc.char_to_line(a)));
                    end = end.or(Some(d.doc.char_to_line(b)));
                }
                let start_line = start.unwrap_or(0);
                let end_line = end.unwrap_or(start_line).max(start_line);
                show(s, ViewMode::Selection { path, start_line, end_line })
            },
        ),
        simple("file.localHistory.showProjectHistory", "Show Project History…", has_project, |s, _| {
            show(s, ViewMode::Project)
        }),
        simple("file.localHistory.recentChanges", "Recent Changes", has_project, |s, _| show(s, ViewMode::Recent)),
        spec(
            "file.localHistory.putLabel",
            "Put Label…",
            None,
            "{label?}: without a label, opens the dialog",
            Context::Global,
            has_project,
            |s, p| match opt_str(p, "label") {
                Some(label) => {
                    let id = local_history::put_label(s, label).map_err(failed)?;
                    Ok(json!({ "id": id }))
                }
                None => {
                    s.local_history.label_dialog = Some(String::new());
                    ok()
                }
            },
        ),
        spec("localHistory.closeLabelDialog", "Close Put Label", None, "", Context::Global, always, |s, _| {
            s.local_history.label_dialog = None;
            ok()
        }),
        spec("localHistory.select", "Select Local History Revision", None, "{index}", Context::Global, always, |s, p| {
            let i = req_usize(p, "index")?;
            local_history::select(s, i).map_err(failed)?;
            let v = s.local_history.view.as_ref();
            Ok(json!({
                "old": v.map(|v| v.old_text.as_str()),
                "new": v.map(|v| v.new_text.as_str()),
            }))
        }),
        spec(
            "localHistory.revert",
            "Revert to Local History Revision",
            None,
            "{id?} (default: the selected revision)",
            Context::Global,
            has_project,
            |s, p| {
                let id = match p.get("id").and_then(Value::as_u64) {
                    Some(id) => id,
                    None => s
                        .local_history
                        .view
                        .as_ref()
                        .and_then(|v| v.selected.and_then(|i| v.revisions.get(i)))
                        .map(|r| r.id)
                        .ok_or_else(|| CommandError::BadParams("no revision selected".into()))?,
                };
                let path = local_history::revert(s, id).map_err(failed)?;
                s.status = format!("Reverted {}", path.display());
                ok()
            },
        ),
        spec("localHistory.close", "Close Local History", None, "", Context::Global, always, |s, _| {
            s.local_history.view = None;
            ok()
        }),
        spec("localHistory.view", "Local History Window State", None, "Returns the open window's revisions", Context::Global, always, |s, _| {
            Ok(view_json(s))
        }),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    fn project() -> (tempfile::TempDir, std::path::PathBuf, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.execute("file.open", &json!({ "path": root.join("a.txt") })).unwrap();
        (dir, root, s)
    }

    fn revisions(v: &serde_json::Value) -> Vec<String> {
        v["revisions"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap_or("").to_string()).collect()
    }

    #[test]
    fn save_records_and_revert_restores() {
        let (_d, root, mut s) = project();
        let file = root.join("a.txt");
        s.execute("editor.setSelection", &json!({ "anchor": 4, "head": 4 })).unwrap();
        s.execute("editor.type", &json!({ "text": "TWO " })).unwrap();
        s.save_all().unwrap();
        let v = s.execute("file.localHistory.showHistory", &json!({})).unwrap();
        assert_eq!(revisions(&v), vec!["Saved", "Original"]);

        // The selected revision (newest) diffs against the current text.
        let r = s.execute("localHistory.select", &json!({ "index": 1 })).unwrap();
        assert_eq!(r["old"], "one\ntwo\nthree\n");
        assert_eq!(r["new"], "one\nTWO two\nthree\n");

        // Selection history: only revisions that changed line 1.
        let v = s
            .execute("file.localHistory.showHistoryForSelection", &json!({ "startLine": 1, "endLine": 1 }))
            .unwrap();
        assert_eq!(revisions(&v), vec!["Original"]);
        let v = s
            .execute("file.localHistory.showHistoryForSelection", &json!({ "startLine": 2, "endLine": 2 }))
            .unwrap();
        assert!(revisions(&v).is_empty());

        // Revert the open file: an undoable edit.
        let id = s.local_history.store().unwrap().file_entries(&file)[0].id;
        s.execute("localHistory.revert", &json!({ "id": id })).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "one\ntwo\nthree\n");
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "one\nTWO two\nthree\n");

        // Labels and project views.
        s.execute("file.localHistory.putLabel", &json!({ "label": "checkpoint" })).unwrap();
        let v = s.execute("file.localHistory.showProjectHistory", &json!({})).unwrap();
        assert_eq!(revisions(&v), vec!["Label", "Saved", "Original"]);
        // A project revision diffs against the file's previous revision.
        let r = s.execute("localHistory.select", &json!({ "index": 1 })).unwrap();
        assert_eq!(r["old"], "one\ntwo\nthree\n");
        let v = s.execute("file.localHistory.recentChanges", &json!({})).unwrap();
        assert_eq!(revisions(&v).len(), 3);
        s.execute("file.localHistory.putLabel", &json!({})).unwrap();
        assert!(s.local_history.label_dialog.is_some());
        assert!(s.execute("file.localHistory.putLabel", &json!({ "label": "  " })).is_err());

        // A new session reads the history back.
        let mut s2 = Session::new();
        s2.lsp_enabled = false;
        s2.execute("file.open", &json!({ "path": root })).unwrap();
        let v = s2.execute("file.localHistory.showHistory", &json!({ "path": file })).unwrap();
        assert_eq!(revisions(&v), vec!["Saved", "Original"]);
    }

    #[test]
    fn revert_closed_file_writes_disk() {
        let (_d, root, mut s) = project();
        let file = root.join("a.txt");
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        s.save_all().unwrap();
        s.execute("file.closeAllTabs", &json!({})).ok();
        while s.ws.active().is_some() {
            let id = s.ws.active_editor().unwrap();
            s.ws.close_editor(id);
        }
        let id = s.local_history.store().unwrap().file_entries(&file)[0].id;
        s.execute("localHistory.revert", &json!({ "id": id })).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
        let v = s.execute("file.localHistory.showHistory", &json!({ "path": file })).unwrap();
        assert_eq!(revisions(&v)[0], "Reverted");
    }

    #[test]
    fn hostile_params() {
        let (_d, _root, mut s) = project();
        assert!(s.execute("localHistory.select", &json!({ "index": 5 })).is_err());
        assert!(s.execute("localHistory.revert", &json!({})).is_err());
        assert!(s.execute("localHistory.revert", &json!({ "id": 999 })).is_err());
        s.execute("file.localHistory.putLabel", &json!({ "label": "l" })).unwrap();
        let label = s.local_history.store().unwrap().entries()[0].id;
        assert!(s.execute("localHistory.revert", &json!({ "id": label })).is_err());
        let v = s
            .execute(
                "file.localHistory.showHistoryForSelection",
                &json!({ "startLine": usize::MAX, "endLine": 0 }),
            )
            .unwrap();
        assert!(revisions(&v).is_empty());
        assert!(s.execute("file.localHistory.showHistory", &json!({ "path": "/nonexistent/x" })).is_ok());
    }
}
