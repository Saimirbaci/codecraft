//! Run › Edit Configurations… and the `runConfig.*` commands behind it.
//!
//! While the dialog is open, `runConfig.create/copy/update/delete` edit its draft (Apply/OK save
//! it to `.codecraft/run-configurations.json`, Cancel drops it); otherwise they save at once.

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, simple, spec};
use crate::params::{bool_or, opt_usize, req_usize};
use crate::run_config_store::{
    self, RunConfigDialog, RunKind, Selection, UserRunConfig, check_unique, env_text, parse_env, unique_name,
};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("run.editConfigurations", "Edit Configurations…", has_project, open),
        simple("runConfig.list", "List Run Configurations", has_project, list),
        spec(
            "runConfig.create",
            "New Run Configuration",
            None,
            "{name?: string, kind?: cargo|command|python|cpp|zig|npm, command?: string, env?: \"A=1;B=2\"|{A: \"1\"}, \
             envFile?: string (a .env file, relative to the project), workingDir?: string} → {index}. \
             Cargo commands omit `cargo` (`run --bin app -- --port 80`); python: `main.py --arg` or `-m pytest`; \
             cpp: sources and flags, then `-- args` (compiled and run); zig: `build run`; npm: `run dev`.",
            Context::Global,
            has_project,
            create,
        ),
        spec(
            "runConfig.copy",
            "Copy Run Configuration",
            None,
            "{index: int, derived?: bool} — copy a user (or derived) configuration into a new user one → {index}",
            Context::Global,
            has_project,
            copy,
        ),
        spec(
            "runConfig.update",
            "Edit Run Configuration",
            None,
            "{index: int, name?, kind?, command?, env?, envFile?: string|null, workingDir?, target?: {kind: local|docker|ssh, …}|null} — change a user configuration",
            Context::Global,
            has_project,
            update,
        ),
        spec(
            "runConfig.delete",
            "Delete Run Configuration",
            None,
            "{index: int} — delete a user configuration",
            Context::Global,
            has_project,
            delete,
        ),
        spec(
            "runConfig.select",
            "Select Run Configuration",
            None,
            "{index: int, derived?: bool} — show a configuration in Edit Configurations",
            Context::Global,
            has_dialog,
            |s, p| {
                let sel = selection(s, p)?;
                if let Some(d) = &mut s.run_store.dialog {
                    d.selected = Some(sel);
                }
                ok()
            },
        ),
        simple("runConfig.apply", "Run Configurations: Apply", has_dialog, |s, _| apply(s).map(|_| Value::Null)),
        simple("runConfig.ok", "Run Configurations: OK", has_dialog, |s, _| {
            apply(s)?;
            s.run_store.dialog = None;
            ok()
        }),
        simple("runConfig.cancel", "Run Configurations: Cancel", always, |s, _| {
            s.run_store.dialog = None;
            ok()
        }),
    ]
}

fn has_dialog(s: &Session) -> Result<(), String> {
    s.run_store.dialog.as_ref().map(|_| ()).ok_or_else(|| "Edit Configurations is not open".into())
}

fn project(s: &Session) -> Result<std::path::PathBuf, CommandError> {
    s.run_store
        .root()
        .map(std::path::Path::to_path_buf)
        .or_else(|| s.ws.project.as_ref().map(|p| p.root.clone()))
        .ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

fn open(s: &mut Session, _: &Value) -> CmdResult {
    let draft = s.run_store.user.clone();
    let selected = if !draft.is_empty() {
        Some(Selection { derived: false, index: 0 })
    } else if !s.run_store.derived.is_empty() {
        Some(Selection { derived: true, index: 0 })
    } else {
        None
    };
    s.run_store.dialog = Some(RunConfigDialog { draft, selected, error: None });
    ok()
}

fn describe(c: &UserRunConfig, root: &std::path::Path) -> Value {
    let error = c.validate(root).err();
    let line = c.to_run_config(root, root).map(|r| r.command_line()).unwrap_or_default();
    json!({
        "name": c.name,
        "kind": c.kind,
        "command": c.command,
        "env": env_text(&c.env),
        "envFile": c.env_file,
        "workingDir": c.working_dir,
        "commandLine": line,
        "error": error,
    })
}

fn list(s: &mut Session, _: &Value) -> CmdResult {
    let root = project(s)?;
    let user = s.run_store.dialog.as_ref().map(|d| &d.draft).unwrap_or(&s.run_store.user);
    Ok(json!({
        "dialog": s.run_store.dialog.is_some(),
        "selected": s.run_store.dialog.as_ref().and_then(|d| d.selected),
        "user": user.iter().map(|c| describe(c, &root)).collect::<Vec<_>>(),
        "derived": s.run_store.derived.iter().map(|c| json!({ "name": c.name, "commandLine": c.command_line() })).collect::<Vec<_>>(),
        "file": s.run_store.path(),
        "error": s.run_store.error,
    }))
}

/// Apply `f` to the dialog's draft, or to the saved list (then save and rebuild). Each changed
/// configuration is validated; on error nothing changes.
fn edit<T>(s: &mut Session, f: impl FnOnce(&mut Vec<UserRunConfig>) -> Result<T, String>) -> Result<T, CommandError> {
    let root = project(s)?;
    let in_dialog = s.run_store.dialog.is_some();
    let mut list = match &s.run_store.dialog {
        Some(d) => d.draft.clone(),
        None => s.run_store.user.clone(),
    };
    let before = list.clone();
    let out = f(&mut list).map_err(CommandError::BadParams)?;
    check_unique(&list).map_err(CommandError::BadParams)?;
    // Only changed configurations are checked (one whose directory vanished stays editable).
    for c in list.iter().filter(|c| !before.contains(c)) {
        c.validate(&root).map_err(CommandError::BadParams)?;
    }
    if in_dialog {
        if let Some(d) = &mut s.run_store.dialog {
            d.draft = list;
            d.error = None;
        }
    } else {
        let previous = std::mem::replace(&mut s.run_store.user, list);
        if let Err(e) = s.run_store.save() {
            s.run_store.user = previous;
            return Err(CommandError::Failed(e));
        }
        run_config_store::rebuild(s);
    }
    Ok(out)
}

/// Fill fields of `c` from params (only those present).
fn fill(c: &mut UserRunConfig, p: &Value) -> Result<(), String> {
    if let Some(v) = p.get("name") {
        c.name = v.as_str().ok_or("`name` must be text")?.trim().to_string();
    }
    if let Some(v) = p.get("kind") {
        c.kind = RunKind::parse(v.as_str().ok_or("`kind` must be text")?)?;
    }
    if let Some(v) = p.get("command") {
        c.command = v.as_str().ok_or("`command` must be text")?.trim().to_string();
    }
    if let Some(v) = p.get("env") {
        c.env = parse_env(v)?;
    }
    if let Some(v) = p.get("envFile") {
        c.env_file = match v {
            Value::Null => String::new(),
            v => v.as_str().ok_or("`envFile` must be text")?.trim().to_string(),
        };
    }
    if let Some(v) = p.get("workingDir") {
        c.working_dir = match v {
            Value::Null => String::new(),
            v => v.as_str().ok_or("`workingDir` must be text")?.trim().to_string(),
        };
    }
    // M11: `{kind: "local"}`, `{kind: "docker", image, …}` or `{kind: "ssh", …}`; null = local.
    if let Some(v) = p.get("target") {
        c.target = match v {
            Value::Null => Default::default(),
            v => serde_json::from_value(v.clone()).map_err(|e| format!("bad `target`: {e}"))?,
        };
    }
    Ok(())
}

fn select_user(s: &mut Session, index: usize) {
    if let Some(d) = &mut s.run_store.dialog {
        d.selected = Some(Selection { derived: false, index });
    }
}

fn create(s: &mut Session, p: &Value) -> CmdResult {
    let index = edit(s, |list| {
        if list.len() >= 500 {
            return Err("too many run configurations".into());
        }
        let mut c = UserRunConfig::default();
        fill(&mut c, p)?;
        if c.command.is_empty() {
            c.command = c.kind.default_command().into();
        }
        c.name = unique_name(&c.name, list);
        list.push(c);
        Ok(list.len() - 1)
    })?;
    select_user(s, index);
    Ok(json!({ "index": index }))
}

fn copy(s: &mut Session, p: &Value) -> CmdResult {
    let index = req_usize(p, "index")?;
    let root = project(s)?;
    let source = if bool_or(p, "derived", false) {
        let c = s
            .run_store
            .derived
            .get(index)
            .ok_or_else(|| CommandError::BadParams(format!("no derived configuration {index}")))?;
        UserRunConfig::from_run_config(c, &root)
    } else {
        let list = s.run_store.dialog.as_ref().map(|d| &d.draft).unwrap_or(&s.run_store.user);
        list.get(index).cloned().ok_or_else(|| CommandError::BadParams(format!("no run configuration {index}")))?
    };
    let new = edit(s, |list| {
        let mut c = source;
        c.name = unique_name(&c.name, list);
        list.push(c);
        Ok(list.len() - 1)
    })?;
    select_user(s, new);
    Ok(json!({ "index": new }))
}

fn update(s: &mut Session, p: &Value) -> CmdResult {
    let index = req_usize(p, "index")?;
    edit(s, |list| {
        let c = list.get_mut(index).ok_or_else(|| format!("no run configuration {index}"))?;
        fill(c, p)
    })?;
    ok()
}

fn delete(s: &mut Session, p: &Value) -> CmdResult {
    let index = req_usize(p, "index")?;
    edit(s, |list| {
        if index >= list.len() {
            return Err(format!("no run configuration {index}"));
        }
        list.remove(index);
        Ok(())
    })?;
    if let Some(d) = &mut s.run_store.dialog {
        let n = d.draft.len();
        d.selected = (n > 0).then(|| Selection { derived: false, index: index.min(n - 1) });
    }
    ok()
}

fn selection(s: &Session, p: &Value) -> Result<Selection, CommandError> {
    let index = opt_usize(p, "index")?.unwrap_or(0);
    let derived = bool_or(p, "derived", false);
    let len = if derived {
        s.run_store.derived.len()
    } else {
        s.run_store.dialog.as_ref().map(|d| d.draft.len()).unwrap_or(0)
    };
    if index >= len {
        return Err(CommandError::BadParams(format!("no run configuration {index}")));
    }
    Ok(Selection { derived, index })
}

/// Save the dialog's draft.
fn apply(s: &mut Session) -> Result<(), CommandError> {
    let Some(draft) = s.run_store.dialog.as_ref().map(|d| d.draft.clone()) else { return Ok(()) };
    let root = project(s)?;
    let checked = check_unique(&draft).and_then(|_| draft.iter().try_for_each(|c| c.validate(&root)));
    if let Err(e) = checked {
        if let Some(d) = &mut s.run_store.dialog {
            d.error = Some(e.clone());
        }
        return Err(CommandError::BadParams(e));
    }
    let previous = std::mem::replace(&mut s.run_store.user, draft);
    match s.run_store.save() {
        Ok(path) => {
            run_config_store::rebuild(s);
            s.status = format!("Run configurations saved to {}", path.display());
            Ok(())
        }
        Err(e) => {
            s.run_store.user = previous;
            if let Some(d) = &mut s.run_store.dialog {
                d.error = Some(e.clone());
            }
            Err(CommandError::Failed(e))
        }
    }
}
