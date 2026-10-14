//! Docker commands: Services › Docker, Tools › Services, Dockerfile/Compose runs, Dev Containers (M11 track D).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, simple, spec};
use crate::docker::{self, PruneKind, devcontainer};
use crate::params::{bool_or, opt_str, opt_usize, req_str};
use crate::services::ServicesSection;
use crate::{CommandError, Session};

/// Lines of history `docker logs` starts with.
const DEFAULT_TAIL: usize = 500;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------------------ Tools › Services
        spec(
            "tools.services.docker.connection",
            "Docker Connection…",
            None,
            "{context?: string|null} — use a Docker connection (a `docker context`; null = the CLI's current one) \
             in Services › Docker; without params shows the connections",
            Context::Global,
            always,
            connection,
        ),
        simple("tools.services.docker.connectionsFromContexts", "Docker Connections From Contexts", always, |s, _| {
            docker::load_contexts(s);
            show(s);
            ok()
        }),
        spec(
            "tools.services.dockerRegistry",
            "Docker Registry…",
            None,
            "{login?: bool, registry?: string} — registry login status from the Docker CLI config; `login` opens \
             `docker login [registry]` in a Terminal tab → {registries, credsStore, configPath}",
            Context::Global,
            always,
            registry,
        ),
        simple(
            "tools.services.devContainers.newDevContainerConfiguration",
            "New Dev Container Configuration…",
            has_project,
            new_dev_container_config,
        ),
        simple("file.new.devContainerConfig", "Dev Container Config…", has_project, new_dev_container_config),
        simple(
            "tools.services.devContainers.manageDevContainerBackends",
            "Manage Dev Container Backends",
            always,
            backends,
        ),
        simple("tools.services.devContainers.showDevContainers", "Show Dev Containers", always, |s, _| {
            docker::load_dev_containers(s);
            show(s);
            ok()
        }),
        spec(
            "file.new.dockerfile",
            "Dockerfile",
            None,
            "{dir?: string, name?: string (default Dockerfile)} — create a Dockerfile (a two-stage Rust build in \
             Cargo projects) and open it; an existing one is opened → {path, created}",
            Context::Global,
            has_project,
            new_dockerfile,
        ),
        // ------------------------------------------------------------ Services › Docker
        simple("docker.refresh", "Docker: Refresh", always, |s, _| {
            if !s.docker.contexts_loaded {
                docker::load_contexts(s);
            }
            docker::refresh(s);
            ok()
        }),
        simple("docker.list", "Docker: List", always, list),
        spec("docker.container.start", "Docker: Start Container", None, ID, Context::Global, always, |s, p| {
            container_op(s, p, "start", "Starting")
        }),
        spec("docker.container.stop", "Docker: Stop Container", None, ID, Context::Global, always, |s, p| {
            container_op(s, p, "stop", "Stopping")
        }),
        spec("docker.container.restart", "Docker: Restart Container", None, ID, Context::Global, always, |s, p| {
            container_op(s, p, "restart", "Restarting")
        }),
        spec("docker.container.pause", "Docker: Pause Container", None, ID, Context::Global, always, |s, p| {
            container_op(s, p, "pause", "Pausing")
        }),
        spec("docker.container.unpause", "Docker: Unpause Container", None, ID, Context::Global, always, |s, p| {
            container_op(s, p, "unpause", "Unpausing")
        }),
        spec(
            "docker.container.remove",
            "Docker: Delete Container",
            None,
            "{id: string, force?: bool} — `docker rm` (force: also a running one)",
            Context::Global,
            always,
            |s, p| {
                let id = id_param(p, "id")?;
                let mut args = vec!["rm".to_string()];
                if bool_or(p, "force", false) {
                    args.push("-f".into());
                }
                args.push(id.clone());
                docker::run_op(s, format!("Deleting {}", display_name(s, &id)), args, true);
                ok()
            },
        ),
        spec(
            "docker.container.logs",
            "Docker: Show Log",
            None,
            "{id: string, tail?: int (default 500)} — follow the container's log (`docker logs -f`) in Services → {log}",
            Context::Global,
            always,
            logs,
        ),
        spec(
            "docker.logs.select",
            "Docker: Select Log",
            None,
            "{log: int} — show an open log view",
            Context::Global,
            always,
            |s, p| {
                let id = log_param(s, p)?;
                s.docker.active_log = Some(id);
                s.docker.inspect = None;
                ok()
            },
        ),
        spec(
            "docker.logs.close",
            "Docker: Close Log",
            None,
            "{log: int} — close a log view and stop following",
            Context::Global,
            always,
            |s, p| {
                let id = log_param(s, p)?;
                docker::close_logs(s, id);
                ok()
            },
        ),
        spec(
            "docker.container.exec",
            "Docker: Exec",
            None,
            "{id: string, shell?: string} — open a Terminal tab in the container (`docker exec -it`; bash when \
             present, else sh) → {tab}",
            Context::Global,
            always,
            exec,
        ),
        spec(
            "docker.inspect",
            "Docker: Inspect",
            None,
            "{id: string, kind?: container|image|volume|network} — `docker inspect` as JSON in Services",
            Context::Global,
            always,
            |s, p| {
                let id = id_param(p, "id")?;
                let kind = opt_str(p, "kind").unwrap_or("container");
                if !matches!(kind, "container" | "image" | "volume" | "network") {
                    return Err(CommandError::BadParams(format!("unknown kind {kind}")));
                }
                docker::inspect(s, kind, &id);
                show(s);
                ok()
            },
        ),
        simple("docker.inspect.close", "Docker: Close Inspect", always, |s, _| {
            s.docker.inspect = None;
            ok()
        }),
        spec(
            "docker.image.pull",
            "Docker: Pull Image",
            None,
            "{image: string} — `docker pull` (e.g. `rust:1`)",
            Context::Global,
            always,
            |s, p| {
                let image = id_param(p, "image")?;
                docker::run_op(s, format!("Pulling {image}"), vec!["pull".into(), image], true);
                ok()
            },
        ),
        spec(
            "docker.image.remove",
            "Docker: Delete Image",
            None,
            "{id: string, force?: bool} — `docker rmi`",
            Context::Global,
            always,
            |s, p| {
                let id = id_param(p, "id")?;
                let mut args = vec!["rmi".to_string()];
                if bool_or(p, "force", false) {
                    args.push("-f".into());
                }
                args.push(id.clone());
                docker::run_op(s, format!("Deleting image {id}"), args, true);
                ok()
            },
        ),
        spec(
            "docker.image.run",
            "Docker: Create Container",
            None,
            "{image: string, options?: string (docker run options, e.g. `-p 8080:80 --name web`)} — `docker run -d`",
            Context::Global,
            always,
            |s, p| {
                let image = id_param(p, "image")?;
                let mut args = vec!["run".to_string(), "-d".into()];
                args.extend(
                    crate::run_config_store::split_words(opt_str(p, "options").unwrap_or(""))
                        .map_err(CommandError::BadParams)?,
                );
                args.push(image.clone());
                docker::run_op(s, format!("Creating a container from {image}"), args, true);
                ok()
            },
        ),
        spec(
            "docker.volume.remove",
            "Docker: Delete Volume",
            None,
            "{name: string} — `docker volume rm`",
            Context::Global,
            always,
            |s, p| {
                let name = id_param(p, "name")?;
                docker::run_op(s, format!("Deleting volume {name}"), vec!["volume".into(), "rm".into(), name], true);
                ok()
            },
        ),
        spec(
            "docker.network.remove",
            "Docker: Delete Network",
            None,
            "{id: string} — `docker network rm`",
            Context::Global,
            always,
            |s, p| {
                let id = id_param(p, "id")?;
                docker::run_op(s, format!("Deleting network {id}"), vec!["network".into(), "rm".into(), id], true);
                ok()
            },
        ),
        spec(
            "docker.prune",
            "Docker: Prune",
            None,
            "{what: system|containers|images|volumes|networks, confirm?: bool} — without `confirm: true` asks first \
             (Services shows the question) → {needsConfirmation}",
            Context::Global,
            always,
            prune,
        ),
        simple("docker.prune.cancel", "Docker: Cancel Prune", always, |s, _| {
            s.docker.confirm = None;
            ok()
        }),
        spec("docker.compose.up", "Docker Compose: Up", None, PROJECT, Context::Global, always, |s, p| {
            run_compose(s, p, "up", "Starting")
        }),
        spec("docker.compose.down", "Docker Compose: Down", None, PROJECT, Context::Global, always, |s, p| {
            run_compose(s, p, "down", "Removing")
        }),
        spec("docker.compose.start", "Docker Compose: Start", None, PROJECT, Context::Global, always, |s, p| {
            run_compose(s, p, "start", "Starting")
        }),
        spec("docker.compose.stop", "Docker Compose: Stop", None, PROJECT, Context::Global, always, |s, p| {
            run_compose(s, p, "stop", "Stopping")
        }),
        spec("docker.compose.restart", "Docker Compose: Restart", None, PROJECT, Context::Global, always, |s, p| {
            run_compose(s, p, "restart", "Restarting")
        }),
        simple("docker.output.clear", "Docker: Clear Output", always, |s, _| {
            s.docker.output.clear();
            ok()
        }),
        // ------------------------------------------------------------ Dev Containers
        simple("docker.devContainer.start", "Dev Container: Build and Start", has_project, dev_container_start),
        simple("docker.devContainer.terminal", "Dev Container: Open Terminal", has_project, dev_container_terminal),
    ]
}

fn show(s: &mut Session) {
    crate::services::show(s, ServicesSection::Docker);
}

const ID: &str = "{id: string} — a container id or name";
const PROJECT: &str = "{project: string} — a Compose project name";

fn container_op(s: &mut Session, p: &Value, verb: &str, doing: &str) -> CmdResult {
    let id = id_param(p, "id")?;
    docker::run_op(s, format!("{doing} {}", display_name(s, &id)), vec![verb.to_string(), id], true);
    ok()
}

/// An id, name or image reference from params: not empty, no leading `-` (it would be an
/// option), no control characters.
fn id_param(p: &Value, key: &str) -> Result<String, CommandError> {
    let v = req_str(p, key)?.trim();
    if v.is_empty() || v.starts_with('-') || v.chars().any(char::is_control) || v.len() > 1024 {
        return Err(CommandError::BadParams(format!("bad {key} `{v}`")));
    }
    Ok(v.to_string())
}

fn display_name(s: &Session, id: &str) -> String {
    s.docker.data.container(id).map(|c| c.name.clone()).unwrap_or_else(|| id.to_string())
}

fn log_param(s: &Session, p: &Value) -> Result<u64, CommandError> {
    let id = opt_usize(p, "log")?.ok_or_else(|| CommandError::BadParams("`log` is required".into()))? as u64;
    if s.docker.log(id).is_none() {
        return Err(CommandError::BadParams(format!("no log view {id}")));
    }
    Ok(id)
}

fn connection(s: &mut Session, p: &Value) -> CmdResult {
    match p.get("context") {
        None => {
            docker::load_contexts(s);
            show(s);
        }
        Some(Value::Null) => {
            docker::select_connection(s, None);
            show(s);
        }
        Some(v) => {
            let name = v.as_str().map(str::trim).filter(|n| !n.is_empty() && !n.starts_with('-'));
            let name = name.ok_or_else(|| CommandError::BadParams("`context` must be a context name".into()))?;
            docker::select_connection(s, Some(name.to_string()));
            show(s);
        }
    }
    Ok(json!({ "connection": s.docker.connection }))
}

fn list(s: &mut Session, _: &Value) -> CmdResult {
    let d = &s.docker;
    let logs: Vec<Value> = d
        .logs
        .iter()
        .map(|l| json!({ "log": l.id, "container": l.container, "running": l.running, "lines": l.lines.len() }))
        .collect();
    Ok(json!({
        "connection": d.connection,
        "contexts": d.contexts,
        "loading": d.loading,
        "error": d.error,
        "containers": d.data.containers,
        "images": d.data.images,
        "volumes": d.data.volumes,
        "networks": d.data.networks,
        "compose": d.data.compose_projects(),
        "devContainers": d.dev_containers,
        "logs": logs,
        "operations": d.operations,
    }))
}

fn logs(s: &mut Session, p: &Value) -> CmdResult {
    let id = id_param(p, "id")?;
    let tail = opt_usize(p, "tail")?.unwrap_or(DEFAULT_TAIL).min(docker::MAX_LOG_LINES);
    let log = docker::open_logs(s, &id, tail).map_err(CommandError::Failed)?;
    Ok(json!({ "log": log }))
}

fn exec(s: &mut Session, p: &Value) -> CmdResult {
    let id = id_param(p, "id")?;
    let mut args = s.docker.base_args();
    args.extend(["exec".into(), "-it".into(), id.clone()]);
    match opt_str(p, "shell").map(str::trim).filter(|sh| !sh.is_empty()) {
        Some(sh) => args.push(sh.to_string()),
        None => args.extend(["sh".into(), "-c".into(), devcontainer::SHELL_SCRIPT.into()]),
    }
    let program = s.docker.program_path().display().to_string();
    let title = display_name(s, &id);
    let tab = crate::terminal::open_command(s, &title, &program, &args, None)?;
    Ok(json!({ "tab": tab }))
}

fn prune(s: &mut Session, p: &Value) -> CmdResult {
    let what = req_str(p, "what")?;
    let kind = PruneKind::parse(what).ok_or_else(|| CommandError::BadParams(format!("unknown prune target {what}")))?;
    if !bool_or(p, "confirm", false) {
        s.docker.confirm = Some(docker::Confirm {
            prune: kind,
            message: format!("Remove all {}? This cannot be undone.", kind.label().to_lowercase()),
        });
        show(s);
        return Ok(json!({ "needsConfirmation": true }));
    }
    s.docker.confirm = None;
    docker::run_op(s, format!("Pruning {}", kind.key()), kind.args(), true);
    Ok(json!({ "needsConfirmation": false }))
}

/// `docker compose -p <project> [--project-directory <dir>] [-f <file>]… <verb>`.
pub(crate) fn compose_args(s: &Session, project: &str, verb: &str) -> Vec<String> {
    let mut args = vec!["compose".to_string(), "-p".into(), project.to_string()];
    if let Some(p) = s.docker.data.compose_projects().into_iter().find(|p| p.name == project) {
        if !p.working_dir.is_empty() && Path::new(&p.working_dir).is_dir() {
            args.extend(["--project-directory".into(), p.working_dir.clone()]);
        }
        for f in p.config_files.iter().filter(|f| Path::new(f).is_file()) {
            args.extend(["-f".into(), f.clone()]);
        }
    }
    args.push(verb.to_string());
    if verb == "up" {
        args.push("-d".into());
    }
    args
}

fn run_compose(s: &mut Session, p: &Value, verb: &str, doing: &str) -> CmdResult {
    let project = id_param(p, "project")?;
    let args = compose_args(s, &project, verb);
    docker::run_op(s, format!("{doing} {project}"), args, true);
    ok()
}

fn registry(s: &mut Session, p: &Value) -> CmdResult {
    let status = docker::registry_status(s);
    if bool_or(p, "login", false) {
        let mut args = s.docker.base_args();
        args.push("login".into());
        if let Some(r) = opt_str(p, "registry").map(str::trim).filter(|r| !r.is_empty()) {
            if r.starts_with('-') {
                return Err(CommandError::BadParams(format!("bad registry `{r}`")));
            }
            args.push(r.to_string());
        }
        let program = s.docker.program_path().display().to_string();
        crate::terminal::open_command(s, "docker login", &program, &args, None)?;
    } else {
        show(s);
    }
    Ok(json!({ "registries": status.registries, "credsStore": status.creds_store, "configPath": status.config_path }))
}

fn backends(s: &mut Session, _: &Value) -> CmdResult {
    docker::load_contexts(s);
    show(s);
    let cli = s.docker.devcontainer_cli();
    s.status = match &cli {
        Some(p) => format!("Dev Containers: devcontainer CLI at {}", p.display()),
        None => "Dev Containers: using docker build/run (the devcontainer CLI is not installed)".into(),
    };
    Ok(json!({
        "devcontainerCli": cli,
        "docker": s.docker.program_path(),
        "connection": s.docker.connection_label(),
    }))
}

fn project_root(s: &Session) -> Result<PathBuf, CommandError> {
    s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

fn open_created(s: &mut Session, path: &Path) -> Result<(), CommandError> {
    s.jobs_rescan();
    if let Some(proj) = &mut s.ws.project {
        proj.reveal(path);
    }
    s.open_file(path)?;
    Ok(())
}

fn new_dev_container_config(s: &mut Session, _: &Value) -> CmdResult {
    let root = project_root(s)?;
    if let Some(existing) = devcontainer::find_config(&root) {
        s.open_file(&existing)?;
        return Ok(json!({ "path": existing, "created": false }));
    }
    let dir = root.join(".devcontainer");
    let path = dir.join("devcontainer.json");
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    std::fs::create_dir_all(&dir).map_err(|e| CommandError::Failed(format!("{}: {e}", dir.display())))?;
    std::fs::write(&path, devcontainer::template(&name))
        .map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
    open_created(s, &path)?;
    Ok(json!({ "path": path, "created": true }))
}

fn new_dockerfile(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let dir = match opt_str(p, "dir") {
        Some(d) => {
            let d = PathBuf::from(d);
            if d.is_absolute() { d } else { root.join(d) }
        }
        None => {
            s.ws.project
                .as_ref()
                .and_then(|pr| pr.selected.clone())
                .map(|sel| if sel.is_dir() { sel } else { sel.parent().map(Path::to_path_buf).unwrap_or(sel) })
                .unwrap_or_else(|| root.clone())
        }
    };
    let name = opt_str(p, "name").map(str::trim).filter(|n| !n.is_empty()).unwrap_or("Dockerfile");
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err(CommandError::BadParams(format!("bad file name `{name}`")));
    }
    if !dir.is_dir() {
        return Err(CommandError::Failed(format!("not a directory: {}", dir.display())));
    }
    let path = dir.join(name);
    if path.exists() {
        s.open_file(&path)?;
        return Ok(json!({ "path": path, "created": false }));
    }
    let cargo = dir.join("Cargo.toml").is_file() || root.join("Cargo.toml").is_file();
    let bin = s
        .ws
        .project
        .as_ref()
        .and_then(|pr| pr.cargo.as_ref())
        .and_then(|m| m.packages.iter().flat_map(|p| p.targets.iter()).find(|t| t.is_bin()).map(|t| t.name.clone()))
        .unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "app".into()));
    std::fs::write(&path, docker::run::dockerfile_template(cargo, &bin))
        .map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
    open_created(s, &path)?;
    Ok(json!({ "path": path, "created": true }))
}

fn dev_container(s: &Session) -> Result<(PathBuf, PathBuf, devcontainer::DevContainerConfig), CommandError> {
    let root = project_root(s)?;
    let path = devcontainer::find_config(&root).ok_or_else(|| {
        CommandError::Failed(
            "No .devcontainer/devcontainer.json (Tools › Services › Dev Containers › New Dev Container Configuration…)"
                .into(),
        )
    })?;
    let cfg = devcontainer::read_config(&path).map_err(CommandError::Failed)?;
    Ok((root, path, cfg))
}

fn dev_container_start(s: &mut Session, _: &Value) -> CmdResult {
    let (root, path, cfg) = dev_container(s)?;
    let docker = s.docker.program_path().display().to_string();
    let cli = s.docker.devcontainer_cli();
    let cmd = devcontainer::up_command(&docker, cli.as_deref(), &root, &path, &cfg).map_err(CommandError::Failed)?;
    let title = format!("Dev Container: {}", cfg.name.clone().unwrap_or_else(|| devcontainer::container_name(&root)));
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    Ok(json!({ "tab": tab.0, "container": devcontainer::container_name(&root), "cli": cli }))
}

fn dev_container_terminal(s: &mut Session, _: &Value) -> CmdResult {
    let (root, path, cfg) = dev_container(s)?;
    let docker = s.docker.program_path().display().to_string();
    let cli = s.docker.devcontainer_cli();
    let (program, args) = devcontainer::terminal_command(&docker, cli.as_deref(), &root, &path, &cfg);
    let title = cfg.name.clone().unwrap_or_else(|| "Dev Container".into());
    let tab = crate::terminal::open_command(s, &title, &program, &args, Some(root))?;
    Ok(json!({ "tab": tab }))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;

    const CTX: &str = r#"{"Current":true,"Description":"local","DockerEndpoint":"unix:///x.sock","Error":"","Name":"default"}
{"Current":false,"Description":"remote","DockerEndpoint":"ssh://h","Error":"","Name":"remote"}"#;
    const PS: &str = r#"{"Command":"\"app\"","ID":"c1","Image":"web:1","Labels":"com.docker.compose.project=shop,com.docker.compose.service=web","Names":"shop-web-1","Ports":"","State":"running","Status":"Up 1 hour"}
{"Command":"\"sh\"","ID":"c2","Image":"alpine","Labels":"","Names":"lonely","State":"exited","Status":"Exited (0)"}"#;
    const DEV: &str = r#"{"ID":"d1","Image":"rust:1","Labels":"devcontainer.local_folder=/p","Names":"codecraft-devcontainer-p","State":"running","Status":"Up"}"#;
    const IMAGES: &str = r#"{"ID":"i1","Repository":"web","Tag":"1","Size":"10MB"}"#;
    const VOLUMES: &str = r#"{"Driver":"local","Name":"data"}"#;
    const NETWORKS: &str = r#"{"Driver":"bridge","ID":"n1","Name":"bridge","Scope":"local"}"#;

    /// A fake `docker` (sh) that logs its arguments to `<dir>/calls` and prints canned JSON.
    pub(super) fn fake_docker(dir: &Path) -> PathBuf {
        let calls = dir.join("calls");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> '{calls}'
if [ "$1" = "--context" ]; then shift 2; fi
case "$1" in
  context) printf '%s\n' '{CTX}' ;;
  ps) if [ "$3" = "--filter" ]; then printf '%s\n' '{DEV}'; else printf '%s\n' '{PS}'; fi ;;
  images) printf '%s\n' '{IMAGES}' ;;
  volume) if [ "$2" = ls ]; then printf '%s\n' '{VOLUMES}'; else echo "$3"; fi ;;
  network) if [ "$2" = ls ]; then printf '%s\n' '{NETWORKS}'; else echo "$3"; fi ;;
  logs) echo "first line"; echo "second line"; exec sleep 30 ;;
  inspect) echo '[{{"Id":"c1","State":{{"Running":true}}}}]' ;;
  stop) if [ "$2" = missing ]; then echo "Error: No such container: missing" >&2; exit 1; fi; echo "$2" ;;
  *) echo "$@" ;;
esac
"#,
            calls = calls.display()
        );
        let path = dir.join("docker");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub(super) fn session(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        for (name, text) in files {
            let p = root.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.docker.program = fake_docker(dir.path()).display().to_string();
        s.docker.devcontainer_program = dir.path().join("no-devcontainer-cli").display().to_string();
        s.docker.config_dir = Some(dir.path().join("docker-config"));
        (dir, root, s)
    }

    pub(super) fn pump_until(s: &mut Session, what: impl Fn(&Session) -> bool) {
        let start = Instant::now();
        while !what(s) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out");
            s.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn calls(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("calls")).unwrap_or_default()
    }

    pub(super) fn idle(s: &Session) -> bool {
        !s.docker.busy() && !s.docker.loading
    }

    #[test]
    fn refresh_lists_everything_and_connections_switch_context() {
        let (dir, _root, mut s) = session(&[]);
        s.execute("docker.refresh", &json!({})).unwrap();
        pump_until(&mut s, idle);
        assert!(s.docker.error.is_none(), "{:?}", s.docker.error);
        assert_eq!(s.docker.data.containers.len(), 2);
        assert_eq!(s.docker.data.images[0].reference(), "web:1");
        assert_eq!(s.docker.data.volumes[0].name, "data");
        assert_eq!(s.docker.data.networks[0].name, "bridge");
        assert_eq!(s.docker.contexts.len(), 2);
        let v = s.execute("docker.list", &json!({})).unwrap();
        assert_eq!(v["compose"][0]["name"], "shop");
        assert_eq!(s.services.section, ServicesSection::Database, "refresh alone does not switch sections");
        s.execute("tools.services.docker.connection", &json!({ "context": "remote" })).unwrap();
        assert_eq!(s.services.section, ServicesSection::Docker);
        pump_until(&mut s, idle);
        assert!(calls(&dir).contains("--context remote ps -a --format {{json .}}"), "{}", calls(&dir));
        assert!(s.execute("tools.services.docker.connection", &json!({ "context": "-H evil" })).is_err());
        s.execute("tools.services.docker.connection", &json!({ "context": null })).unwrap();
        assert_eq!(s.docker.connection, None);
        pump_until(&mut s, idle);
    }

    #[test]
    fn container_actions_run_and_report_errors() {
        let (dir, _root, mut s) = session(&[]);
        s.execute("docker.container.stop", &json!({ "id": "c1" })).unwrap();
        assert_eq!(s.docker.operations.len(), 1);
        pump_until(&mut s, |s| s.docker.operations.is_empty() && idle(s));
        assert!(calls(&dir).lines().any(|l| l == "stop c1"), "{}", calls(&dir));
        // The lists reload after an action.
        assert!(s.docker.loaded);
        s.execute("docker.container.stop", &json!({ "id": "missing" })).unwrap();
        pump_until(&mut s, |s| s.docker.operations.is_empty() && idle(s));
        assert!(s.docker.output.iter().any(|l| l.contains("No such container")), "{:?}", s.docker.output);
        for id in
            ["docker.container.start", "docker.container.restart", "docker.container.pause", "docker.container.unpause"]
        {
            s.execute(id, &json!({ "id": "c2" })).unwrap();
        }
        s.execute("docker.container.remove", &json!({ "id": "c2", "force": true })).unwrap();
        s.execute("docker.image.remove", &json!({ "id": "i1" })).unwrap();
        s.execute("docker.image.pull", &json!({ "image": "rust:1" })).unwrap();
        s.execute("docker.image.run", &json!({ "image": "web:1", "options": "-p 8080:80" })).unwrap();
        s.execute("docker.volume.remove", &json!({ "name": "data" })).unwrap();
        s.execute("docker.network.remove", &json!({ "id": "n1" })).unwrap();
        pump_until(&mut s, |s| s.docker.operations.is_empty() && idle(s));
        let c = calls(&dir);
        for line in [
            "start c2",
            "restart c2",
            "pause c2",
            "unpause c2",
            "rm -f c2",
            "rmi i1",
            "pull rust:1",
            "run -d -p 8080:80 web:1",
            "volume rm data",
            "network rm n1",
        ] {
            assert!(c.lines().any(|l| l == line), "{line} missing in {c}");
        }
        // Option injection and empty ids are refused.
        assert!(matches!(s.execute("docker.container.stop", &json!({ "id": "-a" })), Err(CommandError::BadParams(_))));
        assert!(s.execute("docker.container.stop", &json!({ "id": "" })).is_err());
        assert!(s.execute("docker.container.stop", &json!({})).is_err());
    }

    #[test]
    fn prune_asks_first() {
        let (dir, _root, mut s) = session(&[]);
        let v = s.execute("docker.prune", &json!({ "what": "volumes" })).unwrap();
        assert_eq!(v["needsConfirmation"], true);
        assert_eq!(s.docker.confirm.as_ref().map(|c| c.prune), Some(PruneKind::Volumes));
        assert!(!calls(&dir).contains("prune"));
        s.execute("docker.prune.cancel", &json!({})).unwrap();
        assert!(s.docker.confirm.is_none());
        s.execute("docker.prune", &json!({ "what": "system", "confirm": true })).unwrap();
        pump_until(&mut s, |s| s.docker.operations.is_empty() && idle(s));
        assert!(calls(&dir).lines().any(|l| l == "system prune -f"));
        assert!(s.execute("docker.prune", &json!({ "what": "everything" })).is_err());
    }

    #[test]
    fn logs_stream_and_stop_on_close_and_shutdown() {
        let (_dir, _root, mut s) = session(&[]);
        let v = s.execute("docker.container.logs", &json!({ "id": "c1", "tail": 10 })).unwrap();
        let log = v["log"].as_u64().unwrap();
        pump_until(&mut s, |s| s.docker.log(log).is_some_and(|l| l.lines.len() >= 2));
        assert!(s.docker.log(log).unwrap().running);
        assert_eq!(s.docker.active_log, Some(log));
        // Asking again reuses the running view.
        let again = s.execute("docker.container.logs", &json!({ "id": "c1" })).unwrap();
        assert_eq!(again["log"].as_u64(), Some(log));
        s.execute("docker.logs.close", &json!({ "log": log })).unwrap();
        assert!(s.docker.logs.is_empty() && s.docker.active_log.is_none());
        assert!(s.execute("docker.logs.close", &json!({ "log": log })).is_err());
        let v = s.execute("docker.container.logs", &json!({ "id": "c2" })).unwrap();
        let log = v["log"].as_u64().unwrap();
        crate::docker::shutdown(&mut s);
        assert!(!s.docker.log(log).unwrap().running);
        assert!(!s.docker.busy());
    }

    #[test]
    fn inspect_shows_pretty_json() {
        let (_dir, _root, mut s) = session(&[]);
        s.execute("docker.inspect", &json!({ "id": "c1" })).unwrap();
        pump_until(&mut s, |s| s.docker.inspect.is_some());
        let i = s.docker.inspect.clone().unwrap();
        assert!(i.json.contains("\n  {\n    \"Id\": \"c1\""), "{}", i.json);
        s.execute("docker.inspect.close", &json!({})).unwrap();
        assert!(s.docker.inspect.is_none());
        assert!(s.execute("docker.inspect", &json!({ "id": "x", "kind": "pod" })).is_err());
    }

    #[test]
    fn compose_project_actions() {
        let (dir, _root, mut s) = session(&[]);
        s.execute("docker.refresh", &json!({})).unwrap();
        pump_until(&mut s, idle);
        s.execute("docker.compose.up", &json!({ "project": "shop" })).unwrap();
        s.execute("docker.compose.down", &json!({ "project": "shop" })).unwrap();
        pump_until(&mut s, |s| s.docker.operations.is_empty() && idle(s));
        let c = calls(&dir);
        assert!(c.lines().any(|l| l == "compose -p shop up -d"), "{c}");
        assert!(c.lines().any(|l| l == "compose -p shop down"), "{c}");
    }

    #[test]
    fn exec_and_registry_login_open_terminals() {
        let (dir, _root, mut s) = session(&[]);
        let v = s.execute("docker.container.exec", &json!({ "id": "c1" })).unwrap();
        assert!(v["tab"].is_u64());
        std::fs::create_dir_all(dir.path().join("docker-config")).unwrap();
        std::fs::write(
            dir.path().join("docker-config/config.json"),
            r#"{"auths":{"ghcr.io":{}},"credsStore":"desktop"}"#,
        )
        .unwrap();
        let v = s.execute("tools.services.dockerRegistry", &json!({})).unwrap();
        assert_eq!(v["registries"], json!(["ghcr.io"]));
        assert_eq!(v["credsStore"], "desktop");
        s.execute("tools.services.dockerRegistry", &json!({ "login": true, "registry": "ghcr.io" })).unwrap();
        assert!(
            s.execute("tools.services.dockerRegistry", &json!({ "login": true, "registry": "--password=x" })).is_err()
        );
        // Both terminals start asynchronously, in either order.
        let start = Instant::now();
        while !(calls(&dir).contains("login ghcr.io") && calls(&dir).contains("exec -it c1 sh -c")) {
            assert!(start.elapsed() < Duration::from_secs(20), "{}", calls(&dir));
            s.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        s.shutdown();
    }

    #[test]
    fn new_dockerfile_and_dev_container_config() {
        let (_dir, root, mut s) = session(&[("Cargo.toml", "[package]\nname = \"shop\"\n")]);
        let v = s.execute("file.new.dockerfile", &json!({})).unwrap();
        assert_eq!(v["created"], true);
        let text = std::fs::read_to_string(root.join("Dockerfile")).unwrap();
        assert!(text.contains("cargo build --release"), "{text}");
        assert_eq!(s.ws.active().unwrap().1.language, codecraft_syntax::Language::Dockerfile);
        assert_eq!(s.execute("file.new.dockerfile", &json!({})).unwrap()["created"], false);
        assert!(s.execute("file.new.dockerfile", &json!({ "name": "../x" })).is_err());
        let v = s.execute("file.new.dockerfile", &json!({ "name": "Dockerfile.dev" })).unwrap();
        assert_eq!(v["created"], true);
        let v = s.execute("tools.services.devContainers.newDevContainerConfiguration", &json!({})).unwrap();
        assert_eq!(v["created"], true);
        let cfg = devcontainer::read_config(&root.join(".devcontainer/devcontainer.json")).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("proj"));
        assert_eq!(s.execute("file.new.devContainerConfig", &json!({})).unwrap()["created"], false);
    }

    #[test]
    fn dev_container_start_terminal_and_list() {
        let (dir, root, mut s) = session(&[(
            ".devcontainer/devcontainer.json",
            "{ \"image\": \"rust:1\", // c\n \"postCreateCommand\": \"true\", }",
        )]);
        let v = s.execute("tools.services.devContainers.manageDevContainerBackends", &json!({})).unwrap();
        assert!(v["devcontainerCli"].is_null());
        let v = s.execute("docker.devContainer.start", &json!({})).unwrap();
        assert_eq!(v["container"], "codecraft-devcontainer-proj");
        let start = Instant::now();
        while !calls(&dir).contains("exec -w /workspaces/proj codecraft-devcontainer-proj sh -c true") {
            assert!(start.elapsed() < Duration::from_secs(20), "{}", calls(&dir));
            s.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        let c = calls(&dir);
        assert!(
            c.contains(&format!(
                "run -d --name codecraft-devcontainer-proj --label devcontainer.local_folder={}",
                root.display()
            )),
            "{c}"
        );
        assert!(c.contains("rm -f codecraft-devcontainer-proj"), "{c}");
        s.execute("tools.services.devContainers.showDevContainers", &json!({})).unwrap();
        pump_until(&mut s, |s| !s.docker.dev_containers.is_empty() && idle(s));
        assert_eq!(s.docker.dev_containers[0].id, "d1");
        assert!(s.execute("docker.devContainer.terminal", &json!({})).unwrap()["tab"].is_u64());
        s.shutdown();
        let (_d2, _r2, mut s2) = session(&[]);
        assert!(s2.execute("docker.devContainer.start", &json!({})).is_err());
    }

    #[test]
    fn docker_target_runs_a_configuration_in_an_image() {
        let (dir, root, mut s) = session(&[]);
        let c = crate::run_config_store::UserRunConfig {
            name: "hello".into(),
            kind: crate::run_config_store::RunKind::Command,
            command: "echo hi".into(),
            target: crate::run_targets::RunTarget::Docker(crate::docker_target::DockerTarget {
                image: "alpine".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = c.to_run_config(&root, &root).unwrap();
        crate::runs::start_config(&mut s, &config).unwrap();
        let expected = format!("run --rm -i -v {}:/workspace -w /workspace alpine echo hi", root.display());
        let start = Instant::now();
        while !calls(&dir).contains(&expected) {
            assert!(start.elapsed() < Duration::from_secs(20), "{}", calls(&dir));
            s.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        // `runConfig.create` takes the target as JSON.
        let v = s
            .execute(
                "runConfig.create",
                &json!({ "name": "img", "kind": "dockerfile", "command": "Dockerfile -p 80:80", "target": { "kind": "docker", "image": "rust:1" } }),
            )
            .unwrap();
        let i = v["index"].as_u64().unwrap() as usize;
        assert!(matches!(&s.run_store.user[i].target, crate::run_targets::RunTarget::Docker(t) if t.image == "rust:1"));
        assert_eq!(s.run_store.user[i].kind, crate::run_config_store::RunKind::Dockerfile);
        assert!(s.execute("runConfig.update", &json!({ "index": i, "target": { "kind": "nowhere" } })).is_err());
        s.execute("runConfig.update", &json!({ "index": i, "target": null })).unwrap();
        assert!(s.run_store.user[i].target.is_local());
        s.shutdown();
    }

    #[test]
    fn gutter_hooks_cover_dockerfiles_and_compose_files() {
        let compose = "services:\n  web:\n    image: x\n";
        assert_eq!(crate::run_configs::gutter_run_lines(Path::new("/p/compose.yaml"), compose), vec![0, 1]);
        assert_eq!(crate::run_configs::gutter_run_lines(Path::new("/p/Dockerfile"), "FROM x\n"), vec![0]);
        let c = crate::run_configs::context_config(Path::new("/p"), Path::new("/p/compose.yaml"), compose, 2).unwrap();
        assert_eq!(c.name, "Compose: web");
    }
}

#[cfg(test)]
mod hostile {
    use super::tests::{pump_until, session};
    use super::*;

    /// Every Docker command with hostile params against the fake `docker`: errors, never panics.
    #[test]
    fn docker_commands_never_panic() {
        let (_dir, _root, mut s) = session(&[]);
        let params = [
            Value::Null,
            json!({}),
            json!([1]),
            json!({ "id": "", "image": "-x", "name": 7, "project": "\u{0}", "log": u64::MAX, "tail": u64::MAX }),
            json!({ "id": "c1", "kind": "nope", "what": "nope", "context": 5, "shell": "" }),
            json!({ "id": "😀", "image": "a b", "options": "'open", "registry": "-x", "login": true }),
            json!({ "dir": "/does/not/exist", "name": ".." }),
        ];
        let ids: Vec<&str> = crate::commands::registry()
            .all()
            .iter()
            .map(|c| c.id)
            .filter(|id| {
                id.starts_with("docker.") || id.starts_with("tools.services.d") || id.starts_with("file.new.d")
            })
            .collect();
        assert!(ids.len() > 30, "{ids:?}");
        for id in ids {
            for p in &params {
                if let Err(CommandError::Failed(msg)) = s.execute(id, p) {
                    assert!(!msg.starts_with("internal error"), "{id} panicked with {p}: {msg}");
                }
                s.pump();
            }
        }
        pump_until(&mut s, |s| s.docker.operations.is_empty() && !s.docker.loading);
        s.shutdown();
    }
}
