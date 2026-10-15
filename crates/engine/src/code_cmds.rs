//! Code and Refactor menu commands.

use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation};
use codecraft_text::{EditGroup, ops};

use crate::commands::{CmdResult, CommandSpec, Context, editor, has_editor, has_lsp_editor, has_project, ok, spec};
use crate::edit_cmds::run_op;
use crate::params::{opt_str, opt_usize};
use crate::{CommandError, Popup, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        editor("code.commentWithLineComment", "Comment with Line Comment", |s, _| run_op(s, ops::toggle_line_comment)),
        editor("code.commentWithBlockComment", "Comment with Block Comment", |s, _| {
            run_op(s, ops::toggle_block_comment)
        }),
        editor("code.moveLineUp", "Move Line Up", |s, _| move_lines(s, true)),
        editor("code.moveLineDown", "Move Line Down", |s, _| move_lines(s, false)),
        editor("code.moveStatementUp", "Move Statement Up", |s, _| move_lines(s, true)),
        editor("code.moveStatementDown", "Move Statement Down", |s, _| move_lines(s, false)),
        spec(
            "code.codeCompletion.basic",
            "Basic",
            None,
            "",
            Context::Editor,
            |s| {
                crate::cargo_deps::or_cargo_toml(s, |s| {
                    crate::http_client::or_http_editor(s, |s| crate::sql_lang::or_in_injection(s, has_lsp_editor))
                })
            },
            basic_completion,
        ),
        spec("code.reformatCode", "Reformat Code", None, "", Context::Editor, has_editor, reformat),
        spec("code.reformatFile", "Reformat File…", None, "", Context::Editor, has_editor, reformat),
        spec(
            "tools.rust.reformatFileWithRustfmt",
            "Reformat File with Rustfmt",
            None,
            "",
            Context::Editor,
            has_editor,
            |s, _| {
                let (_, did) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
                s.start_rustfmt(did).map_err(CommandError::Failed)?;
                ok()
            },
        ),
        spec(
            "tools.rust.reformatCargoProjectWithRustfmt",
            "Reformat Cargo Project with Rustfmt",
            None,
            "",
            Context::Global,
            has_project,
            |s, _| {
                let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project".into()))?;
                let mut inv = CargoInvocation::new(CargoCommand::Fmt, root);
                inv.json_messages = false;
                crate::runs::start(s, inv, "cargo fmt", true)?;
                ok()
            },
        ),
        spec(
            "code.showContextActions",
            "Show Context Actions",
            Some("Alt+Enter"),
            "",
            Context::Editor,
            |s| {
                // An analyzer's fixes at the caret work without a language server (M15 EA).
                if has_editor(s).is_ok() && crate::analysis::editor::has_context_actions(s) {
                    return Ok(());
                }
                crate::cargo_deps::or_cargo_toml(s, has_lsp_editor)
            },
            |s, _| {
                // Cargo.toml has no language server: dependency updates and advisories (M12 RS).
                if crate::cargo_deps::is_cargo_toml_editor(s) {
                    return crate::cargo_deps::show_context_actions(s);
                }
                // No language server for the file: the analyzer's fixes and suppressions alone.
                if has_lsp_editor(s).is_err() {
                    return crate::analysis::editor::show_context_actions(s);
                }
                s.lsp_code_actions()?;
                ok()
            },
        ),
        spec(
            "code.applyContextAction",
            "Apply Context Action",
            None,
            "{index?: int} — apply the selected (or given) action of the open Context Actions popup",
            Context::Global,
            has_editor,
            apply_context_action,
        ),
        spec(
            "refactor.rename",
            "Rename…",
            None,
            "{newName?: string, preview?: bool} — renames the symbol at the caret (multi-file results open the refactoring preview; `preview` always does); without newName the server is asked whether it can be renamed and the UI asks for the name",
            Context::Editor,
            has_lsp_editor,
            rename,
        ),
        spec("code.folding.expand", "Expand", Some("Cmd+="), "", Context::Editor, has_editor, |s, _| {
            fold(s, Some(false), false)
        }),
        spec("code.folding.collapse", "Collapse", Some("Cmd+-"), "", Context::Editor, has_editor, |s, _| {
            fold(s, Some(true), false)
        }),
        spec("code.folding.expandAll", "Expand All", Some("Shift+Cmd+="), "", Context::Editor, has_editor, |s, _| {
            fold(s, Some(false), true)
        }),
        spec(
            "code.folding.collapseAll",
            "Collapse All",
            Some("Shift+Cmd+-"),
            "",
            Context::Editor,
            has_editor,
            |s, _| fold(s, Some(true), true),
        ),
        spec("code.folding.toggleFolding", "Toggle Folding", None, "{line?: int} (default: the region at the caret)", Context::Editor, has_editor, |s, p| {
            let line = opt_usize(p, "line")?;
            toggle_fold_at(s, line)
        }),
    ]
}

fn move_lines(s: &mut Session, up: bool) -> CmdResult {
    s.edit_active(EditGroup::Standalone, |d, sel, _| {
        ops::move_lines(d, sel, up).unwrap_or_else(|| codecraft_text::Edit::none(sel.clone()))
    })?;
    ok()
}

fn basic_completion(s: &mut Session, _: &Value) -> CmdResult {
    if crate::cargo_deps::is_cargo_toml_editor(s) {
        return crate::cargo_deps::complete(s, false);
    }
    if crate::http_client::is_http_editor(s) {
        return crate::http_client::complete(s, false);
    }
    if crate::sql_lang::in_injection(s) {
        return crate::sql_lang::complete(s, false);
    }
    let Some((e, d)) = s.ws.active() else { return ok() };
    let caret = e.selections.primary().head;
    let mut anchor = caret;
    while anchor > 0 && d.doc.char_at(anchor - 1).is_some_and(|c| c.is_alphanumeric() || c == '_') {
        anchor -= 1;
    }
    s.lsp_completion(anchor)?;
    ok()
}

/// Alt+Cmd+L: the language server's formatter (rustfmt under the hood for Rust), else rustfmt.
fn reformat(s: &mut Session, _: &Value) -> CmdResult {
    let (_, did) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
    let lang = s.ws.doc(did).map(|d| d.language).unwrap_or_default();
    if s.lsp_ready().is_ok() && lang == codecraft_syntax::Language::Rust {
        s.lsp_format()?;
    } else if lang == codecraft_syntax::Language::Rust {
        s.start_rustfmt(did).map_err(CommandError::Failed)?;
    } else if lang == codecraft_syntax::Language::Sql {
        crate::sql_lang::reformat_document(s)?;
    } else {
        s.status = "No formatter for this file type".into();
    }
    ok()
}

fn apply_context_action(s: &mut Session, p: &Value) -> CmdResult {
    let Some(Popup::Actions { items, selected, .. }) = s.popup.take() else { return ok() };
    let idx = opt_usize(p, "index")?.unwrap_or(selected);
    let Some(item) = items.get(idx) else { return Err(CommandError::BadParams(format!("no action {idx}"))) };
    s.apply_code_action(&item.payload)?;
    Ok(json!({ "applied": item.title }))
}

fn rename(s: &mut Session, p: &Value) -> CmdResult {
    let new_name = match p.get("newName") {
        None | Some(Value::Null) => None,
        Some(_) => Some(opt_str(p, "newName").ok_or_else(|| CommandError::BadParams("`newName` must be a string".into()))?),
    };
    // Shift+F6 again (or an agent's rename) while renaming in place finishes that rename.
    if s.completion.rename.is_some() {
        return crate::inline_rename::commit(s, new_name);
    }
    crate::refactor::rename(s, new_name, crate::params::bool_or(p, "preview", false))?;
    ok()
}

/// Collapse/expand the fold at the caret (`all` = every region).
fn fold(s: &mut Session, collapse: Option<bool>, all: bool) -> CmdResult {
    let Some((eid, did)) = s.active_ids() else { return ok() };
    let caret_line = s.ws.active().map(|(e, d)| d.doc.char_to_line(e.selections.primary().head)).unwrap_or(0);
    let ranges = s.ws.doc_mut(did).map(|d| d.folding_ranges().to_vec()).unwrap_or_default();
    let Some(editor) = s.ws.editor_mut(eid) else { return ok() };
    if all {
        if collapse == Some(true) {
            editor.folded.extend(ranges.iter().map(|r| r.start_line));
        } else {
            editor.folded.clear();
        }
        return ok();
    }
    // Innermost region containing the caret line.
    let target =
        ranges.iter().filter(|r| r.start_line <= caret_line && caret_line <= r.end_line).max_by_key(|r| r.start_line);
    if let Some(r) = target {
        match collapse {
            Some(true) => {
                editor.folded.insert(r.start_line);
            }
            Some(false) => {
                editor.folded.remove(&r.start_line);
            }
            None => {
                if !editor.folded.remove(&r.start_line) {
                    editor.folded.insert(r.start_line);
                }
            }
        }
    }
    ok()
}

fn toggle_fold_at(s: &mut Session, line: Option<usize>) -> CmdResult {
    let Some(line) = line else { return fold(s, None, false) };
    let Some((eid, did)) = s.active_ids() else { return ok() };
    let exists = s.ws.doc_mut(did).is_some_and(|d| d.folding_ranges().iter().any(|r| r.start_line == line));
    if exists
        && let Some(editor) = s.ws.editor_mut(eid)
        && !editor.folded.remove(&line)
    {
        editor.folded.insert(line);
    }
    ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn specs_have_labels() {
        for s in super::specs() {
            assert!(!s.label.is_empty());
        }
    }
}
