//! Refactoring commands (track R): Refactor This, extract/inline/change signature, Generate,
//! Implement/Override Methods, the refactoring preview, file operations (Move, Copy, Rename,
//! Safe Delete) and the Call/Type/Method hierarchies. Every command takes explicit params so
//! agents can drive it headlessly; without them the UI opens the matching dialog.

use std::path::Path;

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_lsp_editor, ok, spec};
use crate::params::{bool_or, opt_str, req_str, req_usize};
use crate::refactor::{self, ActionFilter, Direction, HierarchyKind, RefactorDialog, file_ops};
use crate::{CommandError, Effect, Session};

const POS: &str = "{path?: string, line?: int, column?: int} — zero-based; default: the caret";

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------ assists
        spec(
            "refactor.refactorThis",
            "Refactor This…",
            None,
            "{path?, line?, column?} — the refactorings available at the caret/selection (rust-analyzer assists plus Rename/Move/Copy)",
            Context::Editor,
            has_editor,
            |s, p| actions(s, p, ActionFilter::RefactorThis),
        ),
        spec(
            "refactor.extract.variable",
            "Extract Variable…",
            Some("Alt+Cmd+V"),
            "{path?, line?, column?} — extract the selected expression into a variable",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::ExtractVariable),
        ),
        spec(
            "refactor.extract.function",
            "Extract Function…",
            Some("Alt+Cmd+M"),
            "{path?, line?, column?} — extract the selected statements into a function",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::ExtractFunction),
        ),
        spec(
            "refactor.extract.constant",
            "Extract Constant…",
            Some("Alt+Cmd+C"),
            "{path?, line?, column?} — extract the selected constant expression into a `const`",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::ExtractConstant),
        ),
        spec(
            "refactor.inline",
            "Inline…",
            Some("Alt+Cmd+N"),
            "{path?, line?, column?} — inline the variable, call or type alias at the caret",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::Inline),
        ),
        spec(
            "refactor.changeSignature",
            "Change Signature…",
            Some("Cmd+F6"),
            "{path?, line?, column?} — signature refactorings at the caret (remove unused parameter, wrap/unwrap return type, …)",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::ChangeSignature),
        ),
        spec(
            "code.generate",
            "Generate…",
            None,
            "{path?, line?, column?} — generate code at the caret (new, getters/setters, Default, impls, derive, missing members…)",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::Generate),
        ),
        spec(
            "code.implementMethods",
            "Implement Methods…",
            None,
            "{path?, line?, column?} — add the trait's missing items to the impl at the caret",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::ImplementMembers),
        ),
        spec(
            "code.overrideMethods",
            "Override Methods…",
            None,
            "{path?, line?, column?} — add the trait's default methods to the impl at the caret",
            Context::Editor,
            has_lsp_editor,
            |s, p| actions(s, p, ActionFilter::OverrideMembers),
        ),
        // ------------------------------------------------ preview
        spec(
            "refactor.preview.open",
            "Preview Workspace Edit",
            None,
            "{edit: WorkspaceEdit, title?: string, apply?: bool} — review an LSP WorkspaceEdit (changes or documentChanges with create/rename/delete) in the refactoring preview; `apply` applies it at once. Returns the entries.",
            Context::Global,
            always,
            open_edit,
        ),
        spec(
            "refactor.preview.apply",
            "Apply Refactoring",
            None,
            "Apply the open refactoring preview (its included entries). Returns {applied, skipped, errors, files}.",
            Context::Global,
            has_preview,
            |s, _| {
                let report = refactor::apply_open_preview(s)?;
                serde_json::to_value(&report).map_err(|e| CommandError::Failed(e.to_string()))
            },
        ),
        spec("refactor.preview.cancel", "Cancel Refactoring", None, "", Context::Global, always, |s, _| {
            s.refactor.preview = None;
            ok()
        }),
        spec(
            "refactor.preview.include",
            "Include in Refactoring",
            None,
            "{index: int, include?: bool} — include or exclude one entry (toggles without `include`)",
            Context::Global,
            has_preview,
            |s, p| {
                let index = req_usize(p, "index")?;
                let include = p.get("include").and_then(Value::as_bool);
                let preview = s.refactor.preview.as_mut().ok_or_else(|| CommandError::Failed("no preview".into()))?;
                let e = preview
                    .entries
                    .get_mut(index)
                    .ok_or_else(|| CommandError::BadParams(format!("no entry {index}")))?;
                e.include = include.unwrap_or(!e.include);
                Ok(json!({ "include": e.include }))
            },
        ),
        spec(
            "refactor.preview.select",
            "Show Refactoring Entry",
            None,
            "{index: int} — show this entry's diff",
            Context::Global,
            has_preview,
            |s, p| {
                let index = req_usize(p, "index")?;
                let preview = s.refactor.preview.as_mut().ok_or_else(|| CommandError::Failed("no preview".into()))?;
                if index >= preview.entries.len() {
                    return Err(CommandError::BadParams(format!("no entry {index}")));
                }
                preview.selected = index;
                ok()
            },
        ),
        spec(
            "refactor.state",
            "Refactoring State",
            None,
            "The open preview (entries and a unified diff), dialog, hierarchy rows and last report, as JSON",
            Context::Global,
            always,
            state,
        ),
        // ------------------------------------------------ file operations
        spec(
            "refactor.moveFile",
            "Move File…",
            None,
            "{path?: string, to?: string} — move a file or directory into the directory `to` (or to the full path `to`); updates `mod` declarations via rust-analyzer when it can. Without `to` the dialog opens.",
            Context::Global,
            always,
            move_file,
        ),
        spec(
            "refactor.copyFile",
            "Copy File…",
            None,
            "{path?: string, to?: string} — copy a file or directory into the directory `to` (or to the full path `to`). Without `to` the dialog opens.",
            Context::Global,
            always,
            copy_file,
        ),
        spec(
            "refactor.renameFile",
            "Rename File…",
            None,
            "{path?: string, newName?: string} — rename a file or directory in place (a `.rs` rename updates `mod` declarations via rust-analyzer). Without `newName` the dialog opens.",
            Context::Global,
            always,
            rename_file,
        ),
        spec(
            "refactor.safeDelete",
            "Safe Delete…",
            None,
            "{path?: string, force?: bool} — search usages (`mod` declarations, paths, file-name references) and ask before deleting; `force` deletes now, removing the `mod` declarations",
            Context::Global,
            always,
            safe_delete,
        ),
        spec(
            "refactor.safeDelete.confirm",
            "Confirm Safe Delete",
            None,
            "{path: string} — delete the file of the open Safe Delete dialog (must match its path)",
            Context::Global,
            always,
            confirm_safe_delete,
        ),
        spec("refactor.dialog.cancel", "Close Refactoring Dialog", None, "", Context::Global, always, |s, _| {
            s.refactor.dialog = None;
            ok()
        }),
        spec(
            "projectView.copyPath",
            "Copy Path",
            None,
            "{path: string, relative?: bool} — copy the absolute (or project-relative) path to the clipboard",
            Context::Global,
            always,
            copy_path,
        ),
        spec(
            "projectView.revealInFinder",
            "Reveal in Finder",
            None,
            "{path: string} — show the file in Finder (the file manager elsewhere)",
            Context::Global,
            always,
            reveal,
        ),
        // ------------------------------------------------ hierarchies and navigation
        spec("navigate.callHierarchy", "Call Hierarchy", None, POS, Context::Editor, has_lsp_editor, |s, p| {
            refactor::start_hierarchy(s, HierarchyKind::Call, p)?;
            ok()
        }),
        spec("navigate.typeHierarchy", "Type Hierarchy", None, POS, Context::Editor, has_lsp_editor, |s, p| {
            refactor::start_hierarchy(s, HierarchyKind::Type, p)?;
            ok()
        }),
        spec("navigate.methodHierarchy", "Method Hierarchy", None, POS, Context::Editor, has_lsp_editor, |s, p| {
            refactor::start_hierarchy(s, HierarchyKind::Method, p)?;
            ok()
        }),
        spec("navigate.superMethod", "Super Method", None, POS, Context::Editor, has_lsp_editor, |s, p| {
            refactor::super_method(s, p)?;
            ok()
        }),
        spec("navigate.relatedSymbol", "Related Symbol…", None, POS, Context::Editor, has_lsp_editor, |s, p| {
            refactor::parent_module(s, p)?;
            ok()
        }),
        spec(
            "hierarchy.toggle",
            "Expand/Collapse Hierarchy Node",
            None,
            "{node: int} — expand (loading children) or collapse a node of the Hierarchy tool window",
            Context::Global,
            has_hierarchy,
            |s, p| {
                refactor::toggle_node(s, req_usize(p, "node")?)?;
                ok()
            },
        ),
        spec(
            "hierarchy.open",
            "Open Hierarchy Node",
            None,
            "{node: int} — jump to a node's symbol",
            Context::Global,
            has_hierarchy,
            |s, p| {
                refactor::open_node(s, req_usize(p, "node")?)?;
                ok()
            },
        ),
        spec(
            "hierarchy.setDirection",
            "Hierarchy Direction",
            None,
            "{direction: \"callers\" | \"callees\" | \"subtypes\" | \"supertypes\"}",
            Context::Global,
            has_hierarchy,
            |s, p| {
                let d = req_str(p, "direction")?;
                let d =
                    Direction::parse(d).ok_or_else(|| CommandError::BadParams(format!("unknown direction `{d}`")))?;
                refactor::set_direction(s, d)?;
                ok()
            },
        ),
        spec("hierarchy.close", "Close Hierarchy", None, "", Context::Global, always, |s, _| {
            s.refactor.hierarchy = None;
            ok()
        }),
    ]
}

fn open_edit(s: &mut Session, p: &Value) -> CmdResult {
    let edit = p.get("edit").ok_or_else(|| CommandError::BadParams("missing `edit` (an LSP WorkspaceEdit)".into()))?;
    let edit: codecraft_lsp::lsp_types::WorkspaceEdit =
        serde_json::from_value(edit.clone()).map_err(|e| CommandError::BadParams(format!("bad WorkspaceEdit: {e}")))?;
    let title = opt_str(p, "title").unwrap_or("Workspace Edit");
    let preview = s.build_preview(title, &edit)?;
    let entries: Vec<String> =
        preview.entries.iter().map(|e| e.label(s.ws.project.as_ref().map(|p| p.root.as_path()))).collect();
    if bool_or(p, "apply", false) {
        let report = s.apply_preview(&preview);
        s.refactor.last_report = Some(report.clone());
        return serde_json::to_value(&report).map_err(|e| CommandError::Failed(e.to_string()));
    }
    s.refactor.preview = Some(preview);
    Ok(json!({ "entries": entries }))
}

fn has_preview(s: &Session) -> Result<(), String> {
    s.refactor.preview.as_ref().map(|_| ()).ok_or_else(|| "no refactoring preview is open".into())
}

fn has_hierarchy(s: &Session) -> Result<(), String> {
    s.refactor.hierarchy.as_ref().map(|_| ()).ok_or_else(|| "no hierarchy is shown".into())
}

fn actions(s: &mut Session, p: &Value, filter: ActionFilter) -> CmdResult {
    refactor::code_actions(s, filter, p)?;
    ok()
}

fn state(s: &mut Session, _: &Value) -> CmdResult {
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    let preview = s.refactor.preview.as_ref().map(|pv| {
        json!({
            "title": pv.title,
            "files": pv.files(),
            "entries": pv.entries.iter().enumerate().map(|(i, e)| json!({
                "index": i, "label": e.label(root.as_deref()), "include": e.include, "kind": e.kind,
            })).collect::<Vec<_>>(),
            "diff": pv.unified_diff(root.as_deref()),
        })
    });
    let hierarchy = s.refactor.hierarchy.as_ref().map(|h| {
        json!({
            "title": h.title, "direction": h.direction, "pending": h.pending, "message": h.message,
            "rows": h.visible_rows().into_iter().filter_map(|(i, depth)| h.nodes.get(i).map(|n| json!({
                "node": i, "depth": depth, "name": n.name, "detail": n.detail, "kind": n.kind,
                "path": n.path, "line": n.line, "column": n.column, "expanded": n.expanded,
                "loading": n.loading, "leaf": n.children.as_ref().is_some_and(Vec::is_empty),
            }))).collect::<Vec<_>>(),
        })
    });
    Ok(json!({
        "preview": preview,
        "dialog": s.refactor.dialog,
        "hierarchy": hierarchy,
        "lastReport": s.refactor.last_report,
    }))
}

fn display_dir(path: &Path) -> String {
    path.parent().unwrap_or(path).display().to_string()
}

fn move_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = refactor::target_path(s, p)?;
    let Some(to) = opt_str(p, "to").filter(|t| !t.trim().is_empty()) else {
        let target = display_dir(&path);
        s.refactor.dialog = Some(RefactorDialog::MoveFile { path, target });
        return ok();
    };
    let to = crate::normalize_path(Path::new(to.trim()));
    if bool_or(p, "ask", false) {
        s.refactor.dialog = Some(RefactorDialog::MoveFile { path, target: to.display().to_string() });
        return ok();
    }
    let dest = file_ops::destination(&path, &to).map_err(CommandError::Failed)?;
    s.refactor.dialog = None;
    let title = format!("Move {}", file_name(&path));
    let done = refactor::move_path(s, &path, &dest, &title)?;
    Ok(json!({ "to": dest, "done": done }))
}

fn copy_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = refactor::target_path(s, p)?;
    let Some(to) = opt_str(p, "to").filter(|t| !t.trim().is_empty()) else {
        let target = display_dir(&path);
        let name = copy_name(&path);
        s.refactor.dialog = Some(RefactorDialog::CopyFile { path, target, name });
        return ok();
    };
    let to = crate::normalize_path(Path::new(to.trim()));
    s.refactor.dialog = None;
    let dest = refactor::copy(s, &path, &to)?;
    Ok(json!({ "to": dest }))
}

/// `name.rs` → `name_copy.rs` (a free name next to the original).
fn copy_name(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = path.parent().unwrap_or(path);
    for i in 1..100 {
        let name = if i == 1 { format!("{stem}_copy{ext}") } else { format!("{stem}_copy{i}{ext}") };
        if !dir.join(&name).exists() {
            return name;
        }
    }
    file_name(path)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn rename_file(s: &mut Session, p: &Value) -> CmdResult {
    let path = refactor::target_path(s, p)?;
    let Some(name) = opt_str(p, "newName") else {
        let name = file_name(&path);
        s.refactor.dialog = Some(RefactorDialog::RenameFile { path, name });
        return ok();
    };
    let name = file_ops::valid_file_name(name).map_err(CommandError::BadParams)?;
    let dest =
        path.parent().map(|d| d.join(name)).ok_or_else(|| CommandError::Failed("cannot rename the root".into()))?;
    if dest == path {
        s.refactor.dialog = None;
        return ok();
    }
    s.refactor.dialog = None;
    let title = format!("Rename {} to {name}", file_name(&path));
    let done = refactor::move_path(s, &path, &dest, &title)?;
    Ok(json!({ "to": dest, "done": done }))
}

fn safe_delete(s: &mut Session, p: &Value) -> CmdResult {
    let path = refactor::target_path(s, p)?;
    if bool_or(p, "force", false) {
        let report = refactor::finish_safe_delete(s, &path)?;
        return serde_json::to_value(&report).map_err(|e| CommandError::Failed(e.to_string()));
    }
    refactor::start_safe_delete(s, &path);
    ok()
}

fn confirm_safe_delete(s: &mut Session, p: &Value) -> CmdResult {
    let path = crate::params::req_path(p, "path")?;
    let path = crate::normalize_path(&path);
    match &s.refactor.dialog {
        Some(RefactorDialog::SafeDelete { path: d, .. }) if *d == path => {}
        _ => return Err(CommandError::Failed("no Safe Delete dialog is open for that path".into())),
    }
    let report = refactor::finish_safe_delete(s, &path)?;
    serde_json::to_value(&report).map_err(|e| CommandError::Failed(e.to_string()))
}

fn copy_path(s: &mut Session, p: &Value) -> CmdResult {
    let path = crate::normalize_path(&crate::params::req_path(p, "path")?);
    let text = match (&s.ws.project, bool_or(p, "relative", false)) {
        (Some(proj), true) => proj.relative(&path).into_owned(),
        _ => path.display().to_string(),
    };
    s.effects.push(Effect::CopyToClipboard(text.clone()));
    s.status = format!("Copied {text}");
    Ok(json!({ "copied": text }))
}

fn reveal(s: &mut Session, p: &Value) -> CmdResult {
    let path = crate::normalize_path(&crate::params::req_path(p, "path")?);
    if !path.exists() {
        return Err(CommandError::Failed(format!("{} does not exist", path.display())));
    }
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg("-R").arg(&path);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(if path.is_dir() { path.as_path() } else { path.parent().unwrap_or(&path) });
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // Reap the child on a thread so it never lingers as a zombie.
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            ok()
        }
        Err(e) => {
            s.status = format!("Cannot reveal {}: {e}", path.display());
            Err(CommandError::Failed(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn specs_have_labels_and_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for s in specs() {
            assert!(!s.label.is_empty());
            assert!(seen.insert(s.id), "{}", s.id);
        }
    }

    #[test]
    fn copy_names_are_free() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.rs");
        std::fs::write(&a, "").unwrap();
        assert_eq!(copy_name(&a), "a_copy.rs");
        std::fs::write(tmp.path().join("a_copy.rs"), "").unwrap();
        assert_eq!(copy_name(&a), "a_copy2.rs");
        assert_eq!(copy_name(&tmp.path().join("Makefile")), "Makefile_copy");
    }
}
