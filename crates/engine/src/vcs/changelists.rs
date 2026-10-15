//! Changelists in the Commit tool window: local changes grouped into named lists, one of them
//! active (new changes go there). Kept per project in `<project>/.codecraft/changelists.json`,
//! written only once the project uses more than the default list. The model and its rules are in
//! [`codecraft_vcs::changelist`]; this module loads, syncs with each status and provides the
//! `vcs.changelist.*` commands and Tools › Tasks & Contexts › Add Change List for '<task>'.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_vcs::changelist::{ChangeLists, DEFAULT_NAME};

use crate::commands::{CommandSpec, Context, ok, spec};
use crate::params::{bool_or, opt_str, req_str};
use crate::vcs::{CommitTab, Dialog};
use crate::{CommandError, Session};
use codecraft_workspace::ToolWindowId;

const FILE: &str = "changelists.json";

fn file_path(s: &Session) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.join(".codecraft").join(FILE))
}

/// Load the project's changelists (once per project).
pub(crate) fn load(s: &mut Session) {
    let path = file_path(s);
    if s.vcs.changelists_for == path && path.is_some() {
        return;
    }
    s.vcs.changelists_for = path.clone();
    let mut lists = match path.as_deref().map(crate::user_store::load_json::<ChangeLists>) {
        Some(Ok(Some(l))) => l,
        Some(Err(e)) => {
            log::warn!("{e}");
            ChangeLists::default()
        }
        _ => ChangeLists::default(),
    };
    lists.normalize();
    s.vcs.changelists = lists;
}

fn trivial(c: &ChangeLists) -> bool {
    c.lists.len() == 1 && c.lists.first().is_some_and(|l| l.name == DEFAULT_NAME && l.comment.is_empty())
}

/// Save the changelists (only once they are more than the default list, or the file exists).
fn persist(s: &mut Session) -> Result<(), CommandError> {
    let Some(path) = file_path(s) else { return Ok(()) };
    if trivial(&s.vcs.changelists) && !path.exists() {
        return Ok(());
    }
    crate::user_store::save_json(&path, &s.vcs.changelists, false).map_err(CommandError::Failed)
}

/// The tracked changes (what changelists hold; unversioned files are listed apart).
fn changed_paths(s: &Session) -> Vec<String> {
    s.vcs.status.changes().map(|f| f.path.clone()).collect()
}

/// New status: assign new changes to the active list, drop files that are no longer changed.
pub(crate) fn sync(s: &mut Session) {
    load(s);
    if s.vcs.status.truncated {
        // A cut list would drop assignments of files that are only missing from the list.
        return;
    }
    let paths = changed_paths(s);
    if s.vcs.changelists.sync(paths.iter().map(String::as_str))
        && let Err(e) = persist(s)
    {
        log::warn!("{e}");
    }
}

/// The tracked changes grouped by list: (list name, files), in list order.
pub fn groups(s: &Session) -> Vec<(String, Vec<String>)> {
    let mut paths = changed_paths(s);
    paths.sort();
    s.vcs.changelists.group(paths.iter().map(String::as_str))
}

fn err(e: codecraft_vcs::GitError) -> CommandError {
    CommandError::BadParams(e.to_string())
}

fn str_list(p: &Value, key: &str) -> Vec<String> {
    p.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Accept repository-relative or absolute paths.
fn rel_paths(s: &Session, paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| {
            let pb = PathBuf::from(&p);
            if pb.is_absolute() { s.vcs.rel(&pb).unwrap_or(p) } else { p }
        })
        .collect()
}

fn has_repo(s: &Session) -> Result<(), String> {
    if s.vcs.repo.is_some() {
        return Ok(());
    }
    Err(s.vcs.unavailable.clone().unwrap_or_else(|| "the project is not a Git repository".into()))
}

fn state_json(s: &Session) -> Value {
    let lists: Vec<Value> = groups(s)
        .into_iter()
        .map(|(name, files)| {
            let comment = s.vcs.changelists.get(&name).map(|l| l.comment.clone()).unwrap_or_default();
            json!({ "name": name, "comment": comment, "active": name == s.vcs.changelists.active, "files": files })
        })
        .collect();
    let unversioned: Vec<String> = s.vcs.unversioned().into_iter().map(|f| f.path.clone()).collect();
    json!({ "active": s.vcs.changelists.active, "lists": lists, "unversioned": unversioned })
}

fn set_dialog(s: &mut Session, d: Dialog) {
    s.vcs.dialog = Some(d);
    s.vcs.dialog_error = None;
    s.vcs.branches_popup = false;
}

/// Create a list (optionally active, optionally moving files into it).
fn create(
    s: &mut Session,
    name: &str,
    comment: &str,
    active: bool,
    files: Vec<String>,
) -> Result<String, CommandError> {
    let name = s.vcs.changelists.create(name, comment, active).map_err(err)?;
    if !files.is_empty() {
        let files = rel_paths(s, files);
        s.vcs.changelists.move_files(&files, &name).map_err(err)?;
    }
    persist(s)?;
    Ok(name)
}

/// Check exactly the given list's files for commit (unversioned files unchecked).
fn include_only(s: &mut Session, name: &str) -> Result<Vec<String>, CommandError> {
    if s.vcs.changelists.get(name).is_none() {
        return Err(CommandError::BadParams(format!("there is no changelist '{name}'")));
    }
    let groups = groups(s);
    let mine: Vec<String> = groups.iter().find(|(n, _)| n == name).map(|(_, f)| f.clone()).unwrap_or_default();
    let others = groups.into_iter().filter(|(n, _)| n != name).flat_map(|(_, f)| f);
    s.vcs.commit.excluded = others.collect();
    s.vcs.commit.included_untracked.clear();
    Ok(mine)
}

/// Tools › Tasks & Contexts › Add Change List for '<task>': a list named after the active task,
/// made active.
fn add_for_task(s: &mut Session) -> Result<Value, CommandError> {
    crate::tasks_contexts::ensure_loaded(s);
    let summary = s.tasks.active().map(|t| t.summary.clone()).unwrap_or_else(|| "Default task".into());
    if s.vcs.changelists.get(&summary).is_some() {
        s.vcs.changelists.set_active(&summary).map_err(err)?;
    } else {
        s.vcs.changelists.create(&summary, "", true).map_err(err)?;
    }
    persist(s)?;
    s.status = format!("Changelist '{summary}' is active");
    Ok(json!({ "name": summary }))
}

/// The label of Add Change List for '<active task>'.
pub fn add_for_task_label(s: &Session) -> String {
    format!("Add Change List for '{}'", s.tasks.active().map(|t| t.summary.as_str()).unwrap_or("Default task"))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let g = Context::Global;
    vec![
        spec(
            "vcs.changelists",
            "Changelists",
            None,
            "→ {active, lists: [{name, comment, active, files}], unversioned}",
            g,
            has_repo,
            |s, _| Ok(state_json(s)),
        ),
        spec(
            "vcs.changelist.new",
            "New Changelist…",
            None,
            "{name?: string, comment?: string, active?: bool, files?: [path]} — create a changelist (moving `files` into it); without `name` opens the dialog",
            g,
            has_repo,
            |s, p| {
                let files = str_list(p, "files");
                let Some(name) = opt_str(p, "name") else {
                    set_dialog(
                        s,
                        Dialog::ChangeList {
                            original: None,
                            name: String::new(),
                            comment: String::new(),
                            active: false,
                            files,
                        },
                    );
                    return ok();
                };
                let name = create(s, name, opt_str(p, "comment").unwrap_or(""), bool_or(p, "active", false), files)
                    .inspect_err(|e| s.vcs.dialog_error = Some(e.to_string()))?;
                if matches!(s.vcs.dialog, Some(Dialog::ChangeList { .. })) {
                    s.vcs.dialog = None;
                }
                Ok(json!({ "name": name }))
            },
        ),
        spec(
            "vcs.changelist.edit",
            "Edit Changelist…",
            None,
            "{name: string, newName?: string, comment?: string, active?: bool} — rename, describe or activate a changelist; with only `name` opens the dialog",
            g,
            has_repo,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                let Some(list) = s.vcs.changelists.get(&name).cloned() else {
                    return Err(CommandError::BadParams(format!("there is no changelist '{name}'")));
                };
                if opt_str(p, "newName").is_none() && opt_str(p, "comment").is_none() && p.get("active").is_none() {
                    let active = s.vcs.changelists.active == name;
                    set_dialog(
                        s,
                        Dialog::ChangeList {
                            original: Some(name.clone()),
                            name,
                            comment: list.comment,
                            active,
                            files: Vec::new(),
                        },
                    );
                    return ok();
                }
                let new = opt_str(p, "newName").unwrap_or(&name).to_string();
                let r = s
                    .vcs
                    .changelists
                    .rename(&name, &new, opt_str(p, "comment"))
                    .map_err(err)
                    .inspect_err(|e| s.vcs.dialog_error = Some(e.to_string()))?;
                if bool_or(p, "active", false) {
                    s.vcs.changelists.set_active(&r).map_err(err)?;
                }
                persist(s)?;
                if matches!(s.vcs.dialog, Some(Dialog::ChangeList { .. })) {
                    s.vcs.dialog = None;
                }
                Ok(json!({ "name": r }))
            },
        ),
        spec(
            "vcs.changelist.delete",
            "Delete Changelist",
            None,
            "{name: string} — delete a changelist; its files move to the active one",
            g,
            has_repo,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                s.vcs.changelists.delete(&name).map_err(err)?;
                persist(s)?;
                ok()
            },
        ),
        spec(
            "vcs.changelist.setActive",
            "Set Active Changelist",
            None,
            "{name: string} — new changes go to this list",
            g,
            has_repo,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                s.vcs.changelists.set_active(&name).map_err(err)?;
                persist(s)?;
                ok()
            },
        ),
        spec(
            "vcs.changelist.move",
            "Move to Another Changelist",
            None,
            "{paths: [path], to: string, create?: bool} — move changed files to a list (create: make the list if missing)",
            g,
            has_repo,
            |s, p| {
                let to = req_str(p, "to")?.to_string();
                let paths = rel_paths(s, str_list(p, "paths"));
                if paths.is_empty() {
                    return Err(CommandError::BadParams("`paths` (array of paths) is required".into()));
                }
                if bool_or(p, "create", false) && s.vcs.changelists.get(to.trim()).is_none() {
                    create(s, &to, "", false, paths)?;
                    return ok();
                }
                let n = s.vcs.changelists.move_files(&paths, &to).map_err(err)?;
                persist(s)?;
                Ok(json!({ "moved": n }))
            },
        ),
        spec(
            "vcs.changelist.include",
            "Include Changelist in Commit",
            None,
            "{name: string} — check exactly this list's files for commit",
            g,
            has_repo,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                let files = include_only(s, &name)?;
                Ok(json!({ "files": files }))
            },
        ),
        spec(
            "vcs.changelist.commit",
            "Commit Changelist",
            None,
            "{name: string, message?: string, push?: bool} — commit only this list's files (the message defaults to the commit box, then the list's comment)",
            g,
            |s| match &s.vcs.op {
                Some(op) => Err(format!("“{}” is running", op.label)),
                None => has_repo(s),
            },
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                let files = include_only(s, &name)?;
                if files.is_empty() {
                    return Err(CommandError::Failed(format!("The changelist '{name}' has no changes")));
                }
                if let Some(m) = opt_str(p, "message") {
                    s.vcs.commit.message = m.to_string();
                }
                if s.vcs.commit.message.trim().is_empty()
                    && let Some(c) =
                        s.vcs.changelists.get(&name).map(|l| l.comment.clone()).filter(|c| !c.trim().is_empty())
                {
                    s.vcs.commit.message = c;
                }
                crate::vcs::commit(s, Some(files), bool_or(p, "push", false))?;
                ok()
            },
        ),
        spec(
            "tools.tasksContexts.addChangeListForDefaultTask",
            "Add Change List for 'Default task'",
            None,
            "Create a changelist named after the active task and make it active (or activate it)",
            g,
            has_repo,
            |s, _| {
                let r = add_for_task(s)?;
                s.vcs.commit_tab = CommitTab::Changes;
                s.ws.layout.show(ToolWindowId::Commit);
                Ok(r)
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn commands_need_a_repository_and_valid_names() {
        let mut s = Session::new();
        assert!(s.execute("vcs.changelists", &Value::Null).is_err());
        assert!(s.execute("tools.tasksContexts.addChangeListForDefaultTask", &Value::Null).is_err());
        assert_eq!(add_for_task_label(&s), "Add Change List for 'Default task'");
        assert!(trivial(&ChangeLists::default()));
        let mut c = ChangeLists::default();
        c.create("x", "", false).unwrap();
        assert!(!trivial(&c));
    }
}
