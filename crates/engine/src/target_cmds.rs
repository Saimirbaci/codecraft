//! Run › Manage Targets… additions (M16 track RT): saved, named targets (`.codecraft/targets.json`)
//! that run configurations, commands' `target` param and agents refer to by name, and a resolver
//! that shows what a target reference means.

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, has_project, spec};
use crate::params::req_str;
use crate::run_targets::RunTarget;
use crate::target_exec::{SavedTarget, resolve_ref, save_targets, saved_targets, target_key, target_python_of};
use crate::{CommandError, Session};

fn add(s: &mut Session, p: &Value) -> CmdResult {
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let name = req_str(p, "name")?.trim().to_string();
    if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
        return Err(CommandError::BadParams("a target needs a short name".into()));
    }
    if name.eq_ignore_ascii_case("local") || name.contains(':') {
        return Err(CommandError::BadParams(format!("`{name}` is reserved: pick another name (no `:`)")));
    }
    let v = p.get("target").filter(|v| !v.is_null()).ok_or_else(|| CommandError::BadParams("`target` is required".into()))?;
    let mut target = resolve_ref(s, v)?;
    if target.is_local() {
        return Err(CommandError::BadParams("the local machine is always a target".into()));
    }
    if let Some(py) = p.get("python").and_then(Value::as_str) {
        crate::target_exec::set_target_python(&mut target, py).map_err(CommandError::BadParams)?;
    }
    let mut all = saved_targets(&root);
    all.retain(|t| t.name != name);
    all.push(SavedTarget { name: name.clone(), target: target.clone() });
    save_targets(&root, &all).map_err(CommandError::Failed)?;
    s.status = format!("Saved target {name} ({})", target.label());
    Ok(json!({ "name": name, "target": target, "label": target.label() }))
}

fn remove(s: &mut Session, p: &Value) -> CmdResult {
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let name = req_str(p, "name")?.trim().to_string();
    let mut all = saved_targets(&root);
    let before = all.len();
    all.retain(|t| t.name != name);
    if all.len() == before {
        return Err(CommandError::Failed(format!("no saved target `{name}`")));
    }
    save_targets(&root, &all).map_err(CommandError::Failed)?;
    Ok(json!({ "removed": name }))
}

fn resolve(s: &mut Session, p: &Value) -> CmdResult {
    let v = p.get("target").filter(|v| !v.is_null()).ok_or_else(|| CommandError::BadParams("`target` is required".into()))?;
    let target = resolve_ref(s, v)?;
    let mut out = json!({
        "target": target,
        "label": target.label(),
        "key": target_key(&target),
        "python": target_python_of(&target),
    });
    if !target.is_local()
        && let Ok(ctx) = crate::target_exec::Ctx::resolve(s, &target)
        && let Some(o) = out.as_object_mut()
    {
        o.insert("remoteRoot".into(), json!(ctx.remote_root));
        o.insert("localRoot".into(), json!(ctx.local_root));
    }
    Ok(out)
}

/// Saved targets as Manage Targets rows.
pub(crate) fn saved_rows(s: &Session) -> Vec<(String, RunTarget)> {
    s.project_root().map(|r| saved_targets(&r)).unwrap_or_default().into_iter().map(|t| (t.name, t.target)).collect()
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "run.manageTargets.add",
            "Add Run Target",
            None,
            "{name: string, target: string|{kind: ssh|docker, …}, python?: string} — save a named target (SSH server \
             and remote folder, Docker image/container/Compose service, the interpreter there) in .codecraft/targets.json",
            Context::Global,
            has_project,
            add,
        ),
        spec(
            "run.manageTargets.remove",
            "Remove Run Target",
            None,
            "{name: string} — remove a saved target",
            Context::Global,
            has_project,
            remove,
        ),
        spec(
            "run.manageTargets.resolve",
            "Resolve Run Target",
            None,
            "{target: string|object} — what a target reference means: the target, its label, the project folder there",
            Context::Global,
            crate::commands::always,
            resolve,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_targets_round_trip_through_commands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&root.join("home"));
        s.open_project(&root);
        let v = s
            .execute("run.manageTargets.add", &json!({ "name": "py box", "target": "docker:python:3.12", "python": "/usr/local/bin/python" }))
            .unwrap();
        assert_eq!(v["label"], "Docker: python:3.12");
        assert!(s.execute("run.manageTargets.add", &json!({ "name": "local", "target": "docker:x" })).is_err());
        assert!(s.execute("run.manageTargets.add", &json!({ "name": "a:b", "target": "docker:x" })).is_err());
        assert!(s.execute("run.manageTargets.add", &json!({ "name": "x", "target": "local" })).is_err());
        let r = s.execute("run.manageTargets.resolve", &json!({ "target": "py box" })).unwrap();
        assert_eq!(r["python"], "/usr/local/bin/python");
        assert_eq!(r["remoteRoot"], "/workspace");
        let rows = s.execute("run.manageTargets", &Value::Null).unwrap();
        assert!(rows.as_array().unwrap().iter().any(|r| r["name"] == "py box"), "{rows}");
        s.execute("run.manageTargets.remove", &json!({ "name": "py box" })).unwrap();
        assert!(s.execute("run.manageTargets.remove", &json!({ "name": "py box" })).is_err());
        assert!(s.execute("run.manageTargets.resolve", &json!({ "target": "py box" })).is_err());
    }
}
