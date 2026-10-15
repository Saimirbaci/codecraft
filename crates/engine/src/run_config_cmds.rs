//! Run › Edit Configurations… and the `runConfig.*` commands behind it.
//!
//! While the dialog is open, `runConfig.create/copy/update/delete` edit its draft (Apply/OK save
//! it to `.codecraft/run-configurations.json`, Cancel drops it); otherwise they save at once.

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, simple, spec};
use crate::params::{bool_or, opt_str, opt_usize, req_usize};
use crate::run_config_store::{
    self, BeforeLaunchTask, RunConfigDialog, RunKind, Selection, UserRunConfig, check_unique, env_text, parse_env,
    unique_name,
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
            "{name?: string, kind?: cargo|command|python|cpp|zig|npm|django|fastapi|flask|dockerfile|compose|cmake, \
             command?: string, env?: \"A=1;B=2\"|{A: \"1\"}, envFile?: string (a .env file, relative to the project), \
             workingDir?: string, beforeLaunch?: [{kind: build}|{kind: runConfiguration, name}|{kind: external, \
             command, workingDir?}], debugger?: {initCommands?: [string], stopAtEntry?: bool, env?}, \
             storeAsProjectFile?: bool, executable?: string (cmake)} → {index}. A new configuration starts from \
             its kind's template (runConfig.template.update). Cargo commands omit `cargo` (`run --bin app -- --port 80`); \
             python: `main.py --arg` or `-m pytest`; cpp: sources and flags, then `-- args` (compiled and run); \
             zig: `build run`; npm: `run dev`; cmake: a CMake target, then `-- args` (built, then its executable runs).",
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
            "{index: int, name?, kind?, command?, env?, envFile?: string|null, workingDir?, target?: {kind: local|docker|ssh, …}|null, \
             beforeLaunch?: [task]|null, debugger?: {initCommands?, stopAtEntry?, env?}|null, storeAsProjectFile?: bool, \
             executable?: string} — change a user configuration (see runConfig.create)",
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
                    d.template = None;
                }
                ok()
            },
        ),
        spec(
            "runConfig.generate",
            "Generate Run Configurations",
            None,
            "{agent?: \"claude\"|\"codex\"|\"grok\"|\"gemini\", mode?: \"ask\"|\"auto\" (default ask)} — open Edit \
             Configurations and propose configurations for the project's entry points (Cargo binaries, \
             Django/FastAPI/Flask/Streamlit apps, `__main__` packages, CMake/Meson/Zig executables, sources with \
             main(), npm scripts, tests). With `agent`, that coding agent reviews the detected list and the \
             project and proposes the final one (read-only; in Ask mode anything else needs approval). \
             Poll runConfig.generate.list; add with runConfig.generate.add.",
            Context::Global,
            has_project,
            generate,
        ),
        simple("runConfig.generate.list", "List Generated Run Configurations", has_project, |s, _| {
            Ok(serde_json::to_value(&s.run_store.generate.view).unwrap_or(Value::Null))
        }),
        spec(
            "runConfig.generate.select",
            "Select Generated Run Configuration",
            None,
            "{index: int, selected: bool} or {all: bool} — tick proposals to add",
            Context::Global,
            has_generate_view,
            generate_select,
        ),
        spec(
            "runConfig.generate.add",
            "Add Generated Run Configurations",
            None,
            "{indices?: [int]} — add the ticked (or the given) proposals as configurations (to the dialog's \
             draft when it is open; Apply/OK saves) → {added: [name]}",
            Context::Global,
            has_generate_view,
            generate_add,
        ),
        simple("runConfig.generate.cancel", "Close Generated Run Configurations", always, |s, _| {
            crate::run_config_gen::cancel(s);
            ok()
        }),
        simple("runConfig.templates.list", "List Run Configuration Templates", has_project, |s, _| {
            let templates = s.run_store.dialog.as_ref().map(|d| &d.templates).unwrap_or(&s.run_store.templates);
            let root = project(s)?;
            Ok(json!(
                RunKind::ALL
                    .iter()
                    .map(|k| {
                        let t = run_config_store::template_in(templates, *k);
                        let mut v = describe(&t, &root);
                        v["customized"] = json!(templates.iter().any(|o| o.kind == *k));
                        v
                    })
                    .collect::<Vec<_>>()
            ))
        }),
        spec(
            "runConfig.template.update",
            "Edit Run Configuration Template",
            None,
            "{kind: string, command?, env?, envFile?, workingDir?, target?, beforeLaunch?, debugger?, \
             storeAsProjectFile?, executable?, reset?: bool} — change the defaults new configurations of that kind \
             start from (in the dialog's draft while Edit Configurations is open)",
            Context::Global,
            has_project,
            template_update,
        ),
        spec(
            "runConfig.template.select",
            "Show Run Configuration Template",
            None,
            "{kind: string|null} — show a kind's template in Edit Configurations (null: back to the configurations)",
            Context::Global,
            has_dialog,
            |s, p| {
                let kind = match p.get("kind") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        RunKind::parse(
                            v.as_str().ok_or_else(|| CommandError::BadParams("`kind` must be text".into()))?,
                        )
                        .map_err(CommandError::BadParams)?,
                    ),
                };
                if let Some(d) = &mut s.run_store.dialog {
                    d.template = kind;
                }
                ok()
            },
        ),
        simple("runConfig.apply", "Run Configurations: Apply", has_dialog, |s, _| apply(s).map(|_| Value::Null)),
        simple("runConfig.ok", "Run Configurations: OK", has_dialog, |s, _| {
            apply(s)?;
            s.run_store.dialog = None;
            crate::run_config_gen::cancel(s);
            ok()
        }),
        simple("runConfig.cancel", "Run Configurations: Cancel", always, |s, _| {
            s.run_store.dialog = None;
            crate::run_config_gen::cancel(s);
            ok()
        }),
    ]
}

fn has_generate_view(s: &Session) -> Result<(), String> {
    s.run_store.generate.view.as_ref().map(|_| ()).ok_or_else(|| "no generated run configurations are shown".into())
}

fn generate(s: &mut Session, p: &Value) -> CmdResult {
    let root = project(s)?;
    let agent = match opt_str(p, "agent").filter(|a| !a.trim().is_empty()) {
        None => None,
        Some(id) => {
            let backend = codecraft_agent::Backend::from_id(id).ok_or_else(|| {
                CommandError::BadParams(format!("unknown agent `{id}` (claude, codex, grok, gemini)"))
            })?;
            let mode = match opt_str(p, "mode").unwrap_or("ask") {
                "ask" => crate::approvals::AgentMode::Ask,
                "auto" => crate::approvals::AgentMode::Auto,
                other => return Err(CommandError::BadParams(format!("unknown mode `{other}` (ask or auto)"))),
            };
            Some((backend, mode))
        }
    };
    if s.run_store.dialog.is_none() {
        open(s, &Value::Null)?;
    }
    crate::run_config_gen::start(s, root, agent)?;
    ok()
}

fn generate_select(s: &mut Session, p: &Value) -> CmdResult {
    let Some(view) = &mut s.run_store.generate.view else { return ok() };
    if let Some(all) = p.get("all").and_then(Value::as_bool) {
        for pr in view.proposals.iter_mut().filter(|pr| !pr.exists) {
            pr.selected = all;
        }
        return ok();
    }
    let index = req_usize(p, "index")?;
    let pr = view
        .proposals
        .get_mut(index)
        .ok_or_else(|| CommandError::BadParams(format!("no generated configuration {index}")))?;
    pr.selected = bool_or(p, "selected", !pr.selected);
    ok()
}

fn generate_add(s: &mut Session, p: &Value) -> CmdResult {
    let Some(view) = &s.run_store.generate.view else { return Ok(json!({ "added": [] })) };
    let picked: Vec<UserRunConfig> = match p.get("indices") {
        None | Some(Value::Null) => {
            view.proposals.iter().filter(|pr| pr.selected && !pr.exists).map(|pr| pr.config.clone()).collect()
        }
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for v in items {
                let i = v
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    .ok_or_else(|| CommandError::BadParams("`indices` must be numbers".into()))?;
                let pr = view
                    .proposals
                    .get(i)
                    .ok_or_else(|| CommandError::BadParams(format!("no generated configuration {i}")))?;
                if !pr.exists {
                    out.push(pr.config.clone());
                }
            }
            out
        }
        Some(_) => return Err(CommandError::BadParams("`indices` must be an array".into())),
    };
    let added = edit(s, |list| {
        let mut names = Vec::new();
        for mut c in picked {
            if list.len() >= 500 {
                return Err("too many run configurations".into());
            }
            if list.iter().any(|o| crate::run_config_gen::same_run(o, &c)) {
                continue;
            }
            c.name = unique_name(&c.name, list);
            names.push(c.name.clone());
            list.push(c);
        }
        Ok(names)
    })?;
    crate::run_config_gen::cancel(s);
    if let (Some(first), Some(d)) = (added.first(), &mut s.run_store.dialog)
        && let Some(i) = d.draft.iter().position(|c| &c.name == first)
    {
        d.selected = Some(Selection { derived: false, index: i });
    }
    Ok(json!({ "added": added }))
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
    let templates = s.run_store.templates.clone();
    s.run_store.dialog = Some(RunConfigDialog { draft, selected, error: None, templates, template: None });
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
        "beforeLaunch": c.before_launch,
        "beforeLaunchLabels": c.before_launch.iter().map(BeforeLaunchTask::label).collect::<Vec<_>>(),
        "debugger": c.debugger,
        "storeAsProjectFile": c.store_as_project_file,
        "executable": c.executable,
        "projectFile": c.store_as_project_file.then(|| {
            format!("{}/{}", run_config_store::PROJECT_FILES_DIR, run_config_store::project_file_name(&c.name))
        }),
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
    // M13 track RC: before-launch tasks, debugger options, project file, CMake executable.
    if let Some(v) = p.get("beforeLaunch") {
        c.before_launch = match v {
            Value::Null => Vec::new(),
            Value::Array(items) => {
                if items.len() > 20 {
                    return Err("at most 20 before-launch tasks".into());
                }
                items.iter().map(BeforeLaunchTask::from_value).collect::<Result<_, _>>()?
            }
            _ => return Err("`beforeLaunch` must be a list of tasks".into()),
        };
    }
    if let Some(v) = p.get("debugger") {
        c.debugger.fill(v)?;
    }
    if let Some(v) = p.get("storeAsProjectFile") {
        c.store_as_project_file = v.as_bool().ok_or("`storeAsProjectFile` must be true or false")?;
    }
    if let Some(v) = p.get("executable") {
        c.executable = match v {
            Value::Null => String::new(),
            v => v.as_str().ok_or("`executable` must be text")?.trim().to_string(),
        };
    }
    Ok(())
}

/// Change a kind's template (the dialog's draft while it is open, else saved at once).
fn template_update(s: &mut Session, p: &Value) -> CmdResult {
    let kind = RunKind::parse(crate::params::req_str(p, "kind")?).map_err(CommandError::BadParams)?;
    let root = project(s)?;
    let reset = bool_or(p, "reset", false);
    let mut templates = match &s.run_store.dialog {
        Some(d) => d.templates.clone(),
        None => s.run_store.templates.clone(),
    };
    if reset {
        templates.retain(|t| t.kind != kind);
    } else {
        let mut t = run_config_store::template_in(&templates, kind);
        // A template's kind and name are fixed.
        let mut fields = p.clone();
        if let Some(o) = fields.as_object_mut() {
            o.remove("kind");
            o.remove("name");
        }
        fill(&mut t, &fields).map_err(CommandError::BadParams)?;
        t.kind = kind;
        t.name = kind.label().into();
        if t.command.trim().is_empty() {
            t.command = kind.default_command().into();
        }
        // Checked like a configuration, except that what it names may not exist yet.
        if let Err(e) = t.validate(&root)
            && !e.contains("does not exist")
            && !e.contains("CMake target")
        {
            return Err(CommandError::BadParams(e));
        }
        templates.retain(|o| o.kind != kind);
        templates.push(t);
    }
    if let Some(d) = &mut s.run_store.dialog {
        d.templates = templates;
        return ok();
    }
    let previous = std::mem::replace(&mut s.run_store.templates, templates);
    if let Err(e) = s.run_store.save() {
        s.run_store.templates = previous;
        return Err(CommandError::Failed(e));
    }
    ok()
}

fn select_user(s: &mut Session, index: usize) {
    if let Some(d) = &mut s.run_store.dialog {
        d.selected = Some(Selection { derived: false, index });
        d.template = None;
    }
}

fn create(s: &mut Session, p: &Value) -> CmdResult {
    // A new configuration starts from its kind's template.
    let kind = match p.get("kind").and_then(Value::as_str) {
        Some(k) => RunKind::parse(k).map_err(CommandError::BadParams)?,
        None => RunKind::default(),
    };
    let templates =
        s.run_store.dialog.as_ref().map(|d| d.templates.clone()).unwrap_or_else(|| s.run_store.templates.clone());
    let index = edit(s, |list| {
        if list.len() >= 500 {
            return Err("too many run configurations".into());
        }
        let mut c = run_config_store::from_template(&templates, kind);
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
    let templates = s.run_store.dialog.as_ref().map(|d| d.templates.clone()).unwrap_or_default();
    let root = project(s)?;
    let checked = check_unique(&draft).and_then(|_| draft.iter().try_for_each(|c| c.validate(&root)));
    if let Err(e) = checked {
        if let Some(d) = &mut s.run_store.dialog {
            d.error = Some(e.clone());
        }
        return Err(CommandError::BadParams(e));
    }
    let previous = std::mem::replace(&mut s.run_store.user, draft);
    let previous_templates = std::mem::replace(&mut s.run_store.templates, templates);
    match s.run_store.save() {
        Ok(path) => {
            run_config_store::rebuild(s);
            s.status = format!("Run configurations saved to {}", path.display());
            Ok(())
        }
        Err(e) => {
            s.run_store.user = previous;
            s.run_store.templates = previous_templates;
            if let Some(d) = &mut s.run_store.dialog {
                d.error = Some(e.clone());
            }
            Err(CommandError::Failed(e))
        }
    }
}
