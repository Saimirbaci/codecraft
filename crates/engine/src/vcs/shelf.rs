//! Shelve / Unshelve: local changes set aside as patches in `<project>/.codecraft/shelf/` (see
//! [`codecraft_vcs::shelf`]), shown in the Commit tool window's Shelf tab. Shelving, unshelving,
//! renaming and deleting run as the repository operation on a worker thread; the list is read
//! on a worker too and arrives through [`super::Event::Shelves`].

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_vcs::GitError;
use codecraft_vcs::shelf::{ShelveRequest, ShelvedKind};
use codecraft_workspace::ToolWindowId;

use super::{CommitTab, Dialog, Event, spawn};
use crate::commands::{CmdResult, CommandSpec, Context, ok, spec};
use crate::params::{bool_or, opt_str, req_str};
use crate::{CommandError, Session};

/// `<project>/.codecraft/shelf`.
pub fn root(s: &Session) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.join(".codecraft").join("shelf"))
}

/// Reload the shelf list in the background.
pub(crate) fn load(s: &mut Session) {
    let Some(root) = root(s) else { return };
    s.vcs.shelves_loading = true;
    let tx = s.vcs.tx.clone();
    spawn("shelves", move || {
        let _ = tx.send(Event::Shelves(codecraft_vcs::shelf::list(&root)));
    });
}

fn require_root(s: &Session) -> Result<PathBuf, CommandError> {
    root(s).ok_or_else(|| CommandError::Failed("open a project first".into()))
}

fn str_list(p: &Value, key: &str) -> Option<Vec<String>> {
    p.get(key)?.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

fn rel_paths(s: &Session, paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| {
            let pb = PathBuf::from(&p);
            if pb.is_absolute() { s.vcs.rel(&pb).unwrap_or(p) } else { p }
        })
        .collect()
}

fn idle(s: &Session) -> Result<(), String> {
    if s.vcs.repo.is_none() {
        return Err(s.vcs.unavailable.clone().unwrap_or_else(|| "the project is not a Git repository".into()));
    }
    match &s.vcs.op {
        Some(op) => Err(format!("“{}” is running", op.label)),
        None => Ok(()),
    }
}

fn has_repo(s: &Session) -> Result<(), String> {
    if s.vcs.repo.is_some() {
        return Ok(());
    }
    Err(s.vcs.unavailable.clone().unwrap_or_else(|| "the project is not a Git repository".into()))
}

fn has_shelves(s: &Session) -> Result<(), String> {
    idle(s)?;
    if s.vcs.shelves.is_empty() { Err("the shelf is empty".into()) } else { Ok(()) }
}

/// Shelve files (repository-relative): patches first, then the files are reverted.
pub fn shelve(s: &mut Session, name: &str, files: &[String], keep: bool) -> Result<(), CommandError> {
    let root = require_root(s)?;
    let mut req =
        ShelveRequest { name: name.to_string(), keep, created_ms: crate::user_store::now_ms(), ..Default::default() };
    for f in files {
        let kind = match s.vcs.status.file(f) {
            Some(st) if st.untracked => ShelvedKind::Unversioned,
            Some(st) if st.ignored => continue,
            Some(st) if st.conflict.is_some() => {
                return Err(CommandError::Failed(format!("{f} has a merge conflict: resolve it first")));
            }
            Some(st) if !st.in_head() => ShelvedKind::Added,
            Some(_) => ShelvedKind::Modified,
            None => continue,
        };
        req.files.push((f.clone(), kind));
    }
    if req.files.is_empty() {
        return Err(CommandError::Failed("Select the changes to shelve".into()));
    }
    // Unsaved edits would be written back over the reverted files (and are what gets shelved).
    let _ = s.save_all();
    if !keep {
        for (f, _) in &req.files {
            if let Some(abs) = s.vcs.abs(f)
                && let Some(id) = s.ws.doc_by_path(&abs)
                && let Some(od) = s.ws.doc_mut(id)
                && od.doc.is_modified()
            {
                od.doc.mark_saved();
            }
        }
    }
    s.vcs.dialog = None;
    super::start_op(s, "Shelve Changes", super::OpKind::Shelf, move |git| {
        let shelf = codecraft_vcs::shelf::shelve(git, &root, &req)?;
        let n = shelf.files.len();
        Ok(format!("{n} file{} shelved in “{}”", if n == 1 { "" } else { "s" }, shelf.name))
    })
}

/// Unshelve a shelf (all files, or `files`).
pub fn unshelve(s: &mut Session, id: &str, files: Option<Vec<String>>, keep: bool) -> Result<(), CommandError> {
    let root = require_root(s)?;
    if !s.vcs.shelves.iter().any(|sh| sh.id == id) {
        return Err(CommandError::BadParams(format!("there is no shelf '{id}'")));
    }
    let _ = s.save_all();
    let id = id.to_string();
    super::start_op(s, "Unshelve Changes", super::OpKind::Shelf, move |git| {
        let r = codecraft_vcs::shelf::unshelve(git, &root, &id, files.as_deref(), keep)?;
        let mut msg = format!("{} file{} unshelved", r.applied.len(), if r.applied.len() == 1 { "" } else { "s" });
        if !r.conflicted.is_empty() {
            msg.push_str(&format!("; conflicts in {}", r.conflicted.join(", ")));
        }
        if !r.failed.is_empty() {
            let why: Vec<String> = r.failed.iter().map(|(p, e)| format!("{p}: {e}")).collect();
            if r.applied.is_empty() && r.conflicted.is_empty() {
                return Err(GitError::Failed { command: "git apply".into(), code: None, message: why.join("\n") });
            }
            msg.push_str(&format!("; not applied: {}", why.join("; ")));
        }
        Ok(msg)
    })
}

fn select(s: &mut Session, id: &str, path: Option<String>) -> CmdResult {
    let root = require_root(s)?;
    let Some(shelf) = s.vcs.shelves.iter().find(|sh| sh.id == id) else {
        return Err(CommandError::BadParams(format!("there is no shelf '{id}'")));
    };
    if let Some(p) = &path
        && !shelf.files.iter().any(|f| &f.path == p)
    {
        return Err(CommandError::BadParams(format!("{p} is not in this shelf")));
    }
    s.vcs.shelf_selected = Some((id.to_string(), path.clone()));
    s.vcs.shelf_patch = None;
    if let Some(path) = path {
        let tx = s.vcs.tx.clone();
        let id = id.to_string();
        spawn("shelf-patch", move || {
            let result = codecraft_vcs::shelf::patch_text(&root, &id, &path);
            let _ = tx.send(Event::ShelfPatch { id, path, result });
        });
    }
    ok()
}

fn shelves_json(s: &Session) -> Value {
    let list: Vec<Value> = s
        .vcs
        .shelves
        .iter()
        .map(|sh| {
            json!({
                "id": sh.id,
                "name": sh.name,
                "createdMs": sh.created_ms,
                "base": sh.base,
                "files": sh.files.iter().map(|f| json!({ "path": f.path, "kind": f.kind })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "loading": s.vcs.shelves_loading, "shelves": list })
}

fn show_shelf(s: &mut Session) {
    s.vcs.commit_tab = CommitTab::Shelf;
    s.ws.layout.show(ToolWindowId::Commit);
    load(s);
}

/// Git › Uncommitted Changes › Shelve Changes…: the dialog, prefilled with the checked changes
/// (or with a changelist's files, `{changelist: name}`).
pub(crate) fn shelve_dialog(s: &mut Session, p: &Value) -> CmdResult {
    let files = match (str_list(p, "files"), opt_str(p, "changelist")) {
        (Some(files), _) => rel_paths(s, files),
        (None, Some(name)) => super::changelists::groups(s)
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, f)| f)
            .ok_or_else(|| CommandError::BadParams(format!("there is no changelist '{name}'")))?,
        (None, None) => s.vcs.included_paths(),
    };
    let name = s.vcs.commit.message.lines().next().unwrap_or("").trim().to_string();
    s.vcs.dialog = Some(Dialog::Shelve { name, files, keep: false });
    s.vcs.dialog_error = None;
    s.vcs.branches_popup = false;
    ok()
}

/// Git › Uncommitted Changes › Show Shelf.
pub(crate) fn show_shelf_cmd(s: &mut Session, _p: &Value) -> CmdResult {
    show_shelf(s);
    ok()
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        spec(
            "vcs.shelve",
            "Shelve Changes",
            None,
            "{name?: string, files?: [path], keep?: bool} — save the files' changes as patches in the shelf and revert them (keep: leave them in the work tree too); files default to the checked changes",
            g,
            idle,
            |s, p| {
                let (dialog_name, dialog_files, dialog_keep) = match &s.vcs.dialog {
                    Some(Dialog::Shelve { name, files, keep }) => (name.clone(), Some(files.clone()), *keep),
                    _ => (String::new(), None, false),
                };
                let name = opt_str(p, "name").map(str::to_string).unwrap_or(dialog_name);
                let files = match str_list(p, "files") {
                    Some(f) => rel_paths(s, f),
                    None => dialog_files.unwrap_or_else(|| s.vcs.included_paths()),
                };
                let keep = bool_or(p, "keep", dialog_keep);
                shelve(s, &name, &files, keep).inspect_err(|e| s.vcs.dialog_error = Some(e.to_string()))?;
                ok()
            },
        ),
        spec(
            "vcs.shelves",
            "Shelf",
            None,
            "{refresh?: bool} → {loading, shelves: [{id, name, createdMs, base, files: [{path, kind}]}]} (refresh reloads in the background)",
            g,
            has_repo,
            |s, p| {
                if bool_or(p, "refresh", false) {
                    load(s);
                }
                Ok(shelves_json(s))
            },
        ),
        spec(
            "vcs.unshelve",
            "Unshelve Changes",
            None,
            "{id: string, files?: [path], keep?: bool} — apply a shelf's patches (all, or only `files`); applied files leave the shelf unless keep; a file changed since merges three ways (conflicts go to the merge view)",
            g,
            has_shelves,
            |s, p| {
                let id = req_str(p, "id")?.to_string();
                let files = str_list(p, "files").map(|f| rel_paths(s, f));
                unshelve(s, &id, files, bool_or(p, "keep", false))?;
                ok()
            },
        ),
        spec(
            "vcs.shelf.delete",
            "Delete Shelved Changes",
            None,
            "{id: string, files?: [path]} — delete a shelf (or only some of its files)",
            g,
            has_shelves,
            |s, p| {
                let root = require_root(s)?;
                let id = req_str(p, "id")?.to_string();
                if !s.vcs.shelves.iter().any(|sh| sh.id == id) {
                    return Err(CommandError::BadParams(format!("there is no shelf '{id}'")));
                }
                let files = str_list(p, "files");
                super::start_op(s, "Delete Shelved Changes", super::OpKind::Shelf, move |_| {
                    codecraft_vcs::shelf::delete(&root, &id, files.as_deref())?;
                    Ok("Shelved changes deleted".into())
                })?;
                ok()
            },
        ),
        spec(
            "vcs.shelf.rename",
            "Rename Shelved Changes",
            None,
            "{id: string, name: string}",
            g,
            has_shelves,
            |s, p| {
                let root = require_root(s)?;
                let id = req_str(p, "id")?.to_string();
                let name = req_str(p, "name")?.trim().to_string();
                if name.is_empty() {
                    return Err(CommandError::BadParams("enter a name".into()));
                }
                super::start_op(s, "Rename Shelved Changes", super::OpKind::Shelf, move |_| {
                    codecraft_vcs::shelf::rename(&root, &id, &name)?;
                    Ok(format!("Renamed to “{name}”"))
                })?;
                ok()
            },
        ),
        spec(
            "vcs.shelf.select",
            "Select Shelved Change",
            None,
            "{id: string, path?: path} — select a shelf (and a file: its patch loads for preview)",
            g,
            has_repo,
            |s, p| {
                let id = req_str(p, "id")?.to_string();
                let path = opt_str(p, "path").map(str::to_string);
                select(s, &id, path)
            },
        ),
        spec(
            "vcs.shelf.patch",
            "Shelved Patch",
            None,
            "→ {id, path, patch, error} of the selected shelved file (null while loading)",
            g,
            has_repo,
            |s, _| {
                Ok(match &s.vcs.shelf_patch {
                    Some((id, path, Ok(text))) => json!({ "id": id, "path": path, "patch": text, "error": null }),
                    Some((id, path, Err(e))) => json!({ "id": id, "path": path, "patch": null, "error": e }),
                    None => Value::Null,
                })
            },
        ),
    ]
}
