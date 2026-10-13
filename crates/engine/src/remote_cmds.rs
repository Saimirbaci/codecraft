//! SSH and remote commands: Tools › Deployment, Start SSH Session, Remote Development, the Remote
//! Host and File Transfer tool windows and the SSH server list (M11 track S).
//!
//! Commands that need a server take `{server}`; without it the default deployment server is used
//! (the `…DefaultServer` items) or a server chooser opens ([`crate::remote::ServerPicker`]) whose
//! choice runs the command again with `{server}`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_remote::command as rcmd;
use codecraft_remote::deploy::{DEFAULT_EXCLUDES, DeploymentConfig, PathMapping};
use codecraft_remote::server::ServerConfig;
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, simple, spec};
use crate::deployment;
use crate::params::{bool_or, opt_str, req_str};
use crate::remote::{
    ConfirmDelete, DeploymentDialog, Follow, MappingForm, RemoteDevDialog, ServerPicker, Step, TestStatus, failed,
    refresh_browser, start_op,
};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------ Tools › Deployment
        spec(
            "tools.deployment.uploadToDefaultServer",
            "Upload to Default Server",
            None,
            "{path?: file or folder (default: the active file, else the project)}",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Upload, true),
        ),
        spec(
            "tools.deployment.uploadTo",
            "Upload To…",
            None,
            "{server?: string (else a chooser), path?: file or folder}",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Upload, false),
        ),
        simple(
            "tools.deployment.uploadAllOpenFilesToDefaultServer",
            "Upload All Open Files to Default Server",
            has_project,
            |s, p| upload_open(s, p, true),
        ),
        spec(
            "tools.deployment.uploadAllOpenFilesTo",
            "Upload All Open Files To…",
            None,
            "{server?: string}",
            Context::Global,
            has_project,
            |s, p| upload_open(s, p, false),
        ),
        spec(
            "tools.deployment.downloadFromDefaultServer",
            "Download from Default Server",
            None,
            "{path?: file or folder (default: the active file, else the project)}",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Download, true),
        ),
        spec(
            "tools.deployment.downloadFrom",
            "Download From…",
            None,
            "{server?: string, path?: file or folder}",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Download, false),
        ),
        spec(
            "tools.deployment.compareLocalFileWithDeployedVersion",
            "Compare Local File with Deployed Version",
            None,
            "{path?: file (default: the active file)} — diff with the default server's copy",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Compare, true),
        ),
        spec(
            "tools.deployment.compareWithDeployedTo",
            "Compare with Deployed To…",
            None,
            "{server?: string, path?: file}",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Compare, false),
        ),
        spec(
            "tools.deployment.syncLocalSubtreeWithDeployed",
            "Sync Local Subtree with Deployed",
            None,
            "{path?: folder (default: the project)} — preview the differences with the default server \
             (rsync --dry-run), then remote.sync.apply",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Sync, true),
        ),
        spec(
            "tools.deployment.syncWithDeployedTo",
            "Sync with Deployed To…",
            None,
            "{server?: string, path?: folder}",
            Context::Global,
            has_project,
            |s, p| transfer(s, p, Action::Sync, false),
        ),
        simple("tools.deployment.configuration", "Deployment Configuration…", has_project, open_configuration),
        simple("tools.deployment.options", "Deployment Options…", has_project, |s, _| {
            s.remote.options_open = true;
            ok()
        }),
        spec(
            "tools.deployment.automaticUpload",
            "Automatic Upload",
            None,
            "{on?: bool (default: toggle)} — upload saved files to the default deployment server",
            Context::Global,
            has_project,
            automatic_upload,
        ),
        spec(
            "tools.deployment.browseRemoteHost",
            "Browse Remote Host",
            None,
            "{server?: string (default: the default deployment server, else a chooser), path?: remote folder}",
            Context::Global,
            always,
            browse,
        ),
        spec(
            "tools.startSshSession",
            "Start SSH Session…",
            None,
            "{server?: string (else a chooser)} — an interactive ssh in a Terminal tab",
            Context::Global,
            always,
            start_session,
        ),
        spec(
            "file.remoteDevelopment",
            "Remote Development…",
            None,
            "{server?: string, path?: remote folder} — without both, opens the dialog; with both, mirrors \
             the folder into $CODECRAFT_HOME/remote and opens it with automatic upload and an SSH run target",
            Context::Global,
            always,
            remote_development,
        ),
        // ------------------------------------------------ servers
        simple("remote.servers.list", "SSH Servers: List", always, list_servers),
        spec(
            "remote.servers.add",
            "SSH Servers: Add",
            None,
            "{name: string, host: string, port?: int, user?: string, identityFile?: path} — saved in \
             $CODECRAFT_HOME/ssh.json (keys or the agent; no passwords)",
            Context::Global,
            always,
            add_server,
        ),
        spec(
            "remote.servers.remove",
            "SSH Servers: Remove",
            None,
            "{name: string}",
            Context::Global,
            always,
            remove_server,
        ),
        spec(
            "remote.servers.test",
            "SSH Servers: Test Connection",
            None,
            "{server: string} or an unsaved {host, port?, user?, identityFile?}",
            Context::Global,
            always,
            test_server,
        ),
        simple("remote.servers.reload", "SSH Servers: Reload", always, |s, _| {
            s.remote.reload_servers();
            list_servers(s, &Value::Null)
        }),
        // ------------------------------------------------ deployment configuration
        simple("remote.deployment.get", "Deployment: Get Configuration", has_project, |s, _| {
            let root = deployment::project_root(s)?;
            let cfg = deployment::load(&root).map_err(CommandError::Failed)?;
            serde_json::to_value(cfg).map_err(failed)
        }),
        spec(
            "remote.deployment.setMapping",
            "Deployment: Set Mapping",
            None,
            "{server: string, local?: project-relative folder (default: root), remote: remote folder, \
             excluded?: [string] | \"a;b\"} — replaces the server's mapping for `local`",
            Context::Global,
            has_project,
            set_mapping,
        ),
        spec(
            "remote.deployment.removeServer",
            "Deployment: Remove Server",
            None,
            "{server: string}",
            Context::Global,
            has_project,
            remove_deployment,
        ),
        spec(
            "remote.deployment.setDefault",
            "Deployment: Set Default Server",
            None,
            "{server: string | null}",
            Context::Global,
            has_project,
            set_default,
        ),
        spec(
            "remote.deployment.setOptions",
            "Deployment: Set Options",
            None,
            "{autoUpload?: bool, deleteTargetItems?: bool}",
            Context::Global,
            has_project,
            set_options,
        ),
        spec(
            "remote.dialog.select",
            "Deployment: Select Server",
            None,
            "{server: string} — in the Configuration dialog",
            Context::Global,
            has_project,
            select_in_dialog,
        ),
        spec(
            "remote.dialog.close",
            "Remote: Close Dialog",
            None,
            "{which?: configuration|options|remoteDevelopment|picker|confirm|sync (default: all)}",
            Context::Global,
            always,
            close_dialog,
        ),
        // ------------------------------------------------ sync preview
        spec(
            "remote.sync.toggle",
            "Sync: Toggle Item",
            None,
            "{index: int, selected?: bool}",
            Context::Global,
            has_sync,
            toggle_sync,
        ),
        simple("remote.sync.apply", "Sync: Apply", has_sync, |s, _| {
            let id = deployment::apply_sync(s)?;
            Ok(json!({ "operation": id }))
        }),
        // ------------------------------------------------ Remote Host
        spec(
            "remote.host.browse",
            "Remote Host: Browse",
            None,
            "{server?: string, path?: remote folder}",
            Context::Global,
            always,
            browse,
        ),
        spec(
            "remote.host.open",
            "Remote Host: Open",
            None,
            "{name?: entry of the listed folder, path?: remote path} — a folder is listed; a file is downloaded \
             to $CODECRAFT_HOME/remote and opened (saving uploads it)",
            Context::Global,
            has_browser,
            host_open,
        ),
        simple("remote.host.up", "Remote Host: Parent Folder", has_browser, |s, _| {
            let path = s.remote.browser.path.clone();
            s.remote.browser.path = if path == "/" { path } else { rcmd::parent_of(&path) };
            s.remote.browser.selected = None;
            refresh_browser(s)?;
            ok()
        }),
        simple("remote.host.refresh", "Remote Host: Refresh", has_browser, |s, _| {
            refresh_browser(s)?;
            ok()
        }),
        spec(
            "remote.host.select",
            "Remote Host: Select",
            None,
            "{name: string|null}",
            Context::Global,
            has_browser,
            |s, p| {
                s.remote.browser.selected = opt_str(p, "name").map(str::to_string);
                ok()
            },
        ),
        spec(
            "remote.host.mkdir",
            "Remote Host: New Folder",
            None,
            "{name: string}",
            Context::Global,
            has_browser,
            host_mkdir,
        ),
        spec(
            "remote.host.rename",
            "Remote Host: Rename",
            None,
            "{name: string, newName: string}",
            Context::Global,
            has_browser,
            host_rename,
        ),
        spec(
            "remote.host.delete",
            "Remote Host: Delete",
            None,
            "{name?: string | names?: [string], confirm?: bool} — without confirm, asks first",
            Context::Global,
            has_browser,
            host_delete,
        ),
        // ------------------------------------------------ misc
        simple("remote.fileTransfer.clear", "File Transfer: Clear", always, |s, _| {
            s.remote.log.retain(|e| e.status == crate::remote::TransferStatus::Running);
            ok()
        }),
        spec(
            "remote.syncFromRemote",
            "Sync from Remote",
            None,
            "{server?: string} — download the project from its (default) deployment server, e.g. a Remote \
             Development mirror",
            Context::Global,
            has_project,
            sync_from_remote,
        ),
        spec(
            "remote.dataSource.setTunnel",
            "Data Source: Set SSH Tunnel",
            None,
            "{dataSource: string, server: string | null} — connect through `ssh -N -L` to that server",
            Context::Global,
            has_project,
            set_tunnel,
        ),
    ]
}

fn has_browser(s: &Session) -> Result<(), String> {
    s.remote.browser.server.as_ref().map(|_| ()).ok_or_else(|| "no remote host is being browsed".into())
}

fn has_sync(s: &Session) -> Result<(), String> {
    s.remote.sync_preview.as_ref().map(|_| ()).ok_or_else(|| "no sync preview".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Upload,
    Download,
    Compare,
    Sync,
}

/// Open the chooser that runs `command` again with `{server}`.
fn pick(s: &mut Session, command: &str, title: &str, params: &Value, servers: Vec<String>) -> CmdResult {
    if servers.is_empty() {
        return Err(CommandError::Failed(
            "no SSH servers: add one in Tools › Deployment › Configuration… or ~/.ssh/config".into(),
        ));
    }
    s.remote.picker = Some(ServerPicker {
        title: title.to_string(),
        command: command.to_string(),
        params: if params.is_object() { params.clone() } else { json!({}) },
        servers,
    });
    Ok(json!({ "picker": true }))
}

/// The project's deployment servers.
fn deployment_servers(s: &Session) -> Result<(DeploymentConfig, Vec<String>), CommandError> {
    let root = deployment::project_root(s)?;
    let cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    let names = cfg.servers.iter().map(|d| d.server.clone()).collect();
    Ok((cfg, names))
}

/// The local path a deployment action works on: `path`, else the active file (folders: the
/// project), else the project root.
fn action_path(s: &Session, p: &Value, action: Action) -> Result<PathBuf, CommandError> {
    let root = deployment::project_root(s)?;
    if let Some(path) = opt_str(p, "path").filter(|x| !x.is_empty()) {
        let path = Path::new(path);
        return Ok(crate::normalize_path(&if path.is_absolute() { path.to_path_buf() } else { root.join(path) }));
    }
    if action == Action::Sync {
        return Ok(root);
    }
    if let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone()).filter(|p| p.starts_with(&root)) {
        return Ok(path);
    }
    if action == Action::Compare {
        return Err(CommandError::Failed("open the file to compare".into()));
    }
    Ok(root)
}

fn command_id(action: Action, default: bool) -> &'static str {
    match (action, default) {
        (Action::Upload, true) => "tools.deployment.uploadToDefaultServer",
        (Action::Upload, false) => "tools.deployment.uploadTo",
        (Action::Download, true) => "tools.deployment.downloadFromDefaultServer",
        (Action::Download, false) => "tools.deployment.downloadFrom",
        (Action::Compare, true) => "tools.deployment.compareLocalFileWithDeployedVersion",
        (Action::Compare, false) => "tools.deployment.compareWithDeployedTo",
        (Action::Sync, true) => "tools.deployment.syncLocalSubtreeWithDeployed",
        (Action::Sync, false) => "tools.deployment.syncWithDeployedTo",
    }
}

/// `server` param; else the default deployment server (`default`) or a chooser. `Ok(None)` when
/// the chooser opened.
fn deployment_server(
    s: &mut Session,
    p: &Value,
    command: &str,
    title: &str,
    default: bool,
) -> Result<Option<String>, CommandError> {
    if let Some(name) = opt_str(p, "server").filter(|n| !n.is_empty()) {
        return Ok(Some(name.to_string()));
    }
    let (cfg, names) = deployment_servers(s)?;
    if default {
        return cfg.default_deployment().map(|d| Some(d.server.clone())).ok_or_else(|| {
            CommandError::Failed(
                "no default deployment server: choose one in Tools › Deployment › Configuration…".into(),
            )
        });
    }
    if names.is_empty() {
        return Err(CommandError::Failed(
            "no deployment servers for this project: add one in Tools › Deployment › Configuration…".into(),
        ));
    }
    pick(s, command, title, p, names)?;
    Ok(None)
}

fn transfer(s: &mut Session, p: &Value, action: Action, default: bool) -> CmdResult {
    let title = match action {
        Action::Upload => "Upload To",
        Action::Download => "Download From",
        Action::Compare => "Compare with Deployed To",
        Action::Sync => "Sync with Deployed To",
    };
    let Some(server) = deployment_server(s, p, command_id(action, default), title, default)? else {
        return Ok(json!({ "picker": true }));
    };
    let path = action_path(s, p, action)?;
    if action == Action::Upload && s.ws.docs().any(|d| d.path.as_deref() == Some(path.as_path()) && d.doc.is_modified())
    {
        let _ = s.save_all();
    }
    let id = match action {
        Action::Upload => deployment::upload(s, &server, &path)?,
        Action::Download => deployment::download(s, &server, &path)?,
        Action::Compare => deployment::compare(s, &server, &path)?,
        Action::Sync => deployment::sync_preview(s, &server, &path)?,
    };
    s.ws.layout.show(ToolWindowId::FileTransfer);
    Ok(json!({ "operation": id, "server": server }))
}

fn upload_open(s: &mut Session, p: &Value, default: bool) -> CmdResult {
    let id = if default {
        "tools.deployment.uploadAllOpenFilesToDefaultServer"
    } else {
        "tools.deployment.uploadAllOpenFilesTo"
    };
    let Some(server) = deployment_server(s, p, id, "Upload All Open Files To", default)? else {
        return Ok(json!({ "picker": true }));
    };
    let _ = s.save_all();
    let files: Vec<PathBuf> = s.ws.docs().filter_map(|d| d.path.clone()).filter(|p| p.is_file()).collect();
    let op = deployment::upload_files(s, &server, &files)?;
    s.ws.layout.show(ToolWindowId::FileTransfer);
    Ok(json!({ "operation": op, "server": server }))
}

fn automatic_upload(s: &mut Session, p: &Value) -> CmdResult {
    let root = deployment::project_root(s)?;
    let mut cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    let on = p.get("on").and_then(Value::as_bool).unwrap_or(!cfg.auto_upload);
    if on && cfg.default_deployment().is_none() {
        return Err(CommandError::Failed(
            "Automatic Upload needs a default deployment server: Tools › Deployment › Configuration…".into(),
        ));
    }
    cfg.auto_upload = on;
    deployment::save(&root, &cfg).map_err(CommandError::Failed)?;
    s.status = if on { "Automatic upload: on".into() } else { "Automatic upload: off".into() };
    Ok(json!({ "autoUpload": on }))
}

// ---------------------------------------------------------------- configuration

fn mapping_form(cfg: &DeploymentConfig, server: &str) -> MappingForm {
    match cfg.server(server) {
        Some(d) => {
            let m = d.mappings.first().cloned().unwrap_or_default();
            MappingForm { local: m.local, remote: m.remote, excluded: d.excluded.join("; ") }
        }
        None => MappingForm { excluded: DEFAULT_EXCLUDES.join("; "), ..MappingForm::default() },
    }
}

fn open_configuration(s: &mut Session, _: &Value) -> CmdResult {
    let root = deployment::project_root(s)?;
    let cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    s.remote.reload_servers();
    let selected = cfg
        .default_deployment()
        .map(|d| d.server.clone())
        .or_else(|| cfg.servers.first().map(|d| d.server.clone()))
        .or_else(|| s.remote.server_names().into_iter().next());
    let mapping = selected.as_deref().map(|n| mapping_form(&cfg, n)).unwrap_or_default();
    s.remote.dialog = Some(DeploymentDialog { selected, mapping, new_server: None, error: None });
    Ok(json!({ "dialog": true }))
}

fn select_in_dialog(s: &mut Session, p: &Value) -> CmdResult {
    let name = req_str(p, "server")?.to_string();
    let root = deployment::project_root(s)?;
    let cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    let form = mapping_form(&cfg, &name);
    let d = s.remote.dialog.get_or_insert_with(DeploymentDialog::default);
    d.selected = Some(name);
    d.mapping = form;
    d.error = None;
    ok()
}

fn excluded_param(v: &Value) -> Result<Vec<String>, CommandError> {
    let list: Vec<String> = match v {
        Value::String(t) => t.split([';', '\n']).map(|x| x.trim().to_string()).collect(),
        Value::Array(a) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(|t| t.trim().to_string())
                    .ok_or_else(|| CommandError::BadParams("`excluded` items must be strings".into()))
            })
            .collect::<Result<_, _>>()?,
        Value::Null => Vec::new(),
        _ => return Err(CommandError::BadParams("`excluded` must be a list or a `;`-separated string".into())),
    };
    Ok(list.into_iter().filter(|x| !x.is_empty()).take(200).collect())
}

fn set_mapping(s: &mut Session, p: &Value) -> CmdResult {
    let server = req_str(p, "server")?.trim().to_string();
    if s.remote.server(&server).is_none() {
        return Err(CommandError::Failed(format!("unknown SSH server `{server}`")));
    }
    let local = opt_str(p, "local").unwrap_or("").trim().trim_matches('/').to_string();
    let remote = req_str(p, "remote")?.trim().to_string();
    let excluded = p.get("excluded").map(excluded_param).transpose()?;
    let root = deployment::project_root(s)?;
    let mut cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    let dep = cfg.ensure_server(&server).ok_or_else(|| CommandError::Failed("cannot add the server".into()))?;
    dep.mappings.retain(|m| m.local.trim().trim_matches('/') != local);
    dep.mappings.push(PathMapping { local, remote });
    if let Some(x) = excluded {
        dep.excluded = x;
    }
    if let Err(e) = dep.validate() {
        if let Some(d) = &mut s.remote.dialog {
            d.error = Some(e.to_string());
        }
        return Err(failed(e));
    }
    if cfg.default_server.is_none() && cfg.servers.len() == 1 {
        cfg.default_server = Some(server.clone());
    }
    deployment::save(&root, &cfg).map_err(CommandError::Failed)?;
    if let Some(d) = &mut s.remote.dialog {
        d.error = None;
    }
    s.status = format!("Deployment mapping saved for {server}");
    ok()
}

fn remove_deployment(s: &mut Session, p: &Value) -> CmdResult {
    let server = req_str(p, "server")?.to_string();
    let root = deployment::project_root(s)?;
    let mut cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    cfg.servers.retain(|d| d.server != server);
    if cfg.default_server.as_deref() == Some(server.as_str()) {
        cfg.default_server = None;
        cfg.auto_upload = false;
    }
    deployment::save(&root, &cfg).map_err(CommandError::Failed)?;
    ok()
}

fn set_default(s: &mut Session, p: &Value) -> CmdResult {
    let server = opt_str(p, "server").filter(|x| !x.is_empty()).map(str::to_string);
    let root = deployment::project_root(s)?;
    let mut cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    if let Some(n) = &server
        && cfg.server(n).is_none()
    {
        return Err(CommandError::Failed(format!("`{n}` has no mapping for this project yet")));
    }
    cfg.default_server = server;
    if cfg.default_server.is_none() {
        cfg.auto_upload = false;
    }
    deployment::save(&root, &cfg).map_err(CommandError::Failed)?;
    ok()
}

fn set_options(s: &mut Session, p: &Value) -> CmdResult {
    let root = deployment::project_root(s)?;
    let mut cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    if let Some(v) = p.get("autoUpload").and_then(Value::as_bool) {
        if v && cfg.default_deployment().is_none() {
            return Err(CommandError::Failed("Automatic Upload needs a default deployment server".into()));
        }
        cfg.auto_upload = v;
    }
    if let Some(v) = p.get("deleteTargetItems").and_then(Value::as_bool) {
        cfg.delete_target_items = v;
    }
    deployment::save(&root, &cfg).map_err(CommandError::Failed)?;
    Ok(json!({ "autoUpload": cfg.auto_upload, "deleteTargetItems": cfg.delete_target_items }))
}

fn close_dialog(s: &mut Session, p: &Value) -> CmdResult {
    let r = &mut s.remote;
    match opt_str(p, "which") {
        Some("configuration") => r.dialog = None,
        Some("options") => r.options_open = false,
        Some("remoteDevelopment") => r.remote_dev = None,
        Some("picker") => r.picker = None,
        Some("confirm") => r.confirm = None,
        Some("sync") => r.sync_preview = None,
        Some(other) => return Err(CommandError::BadParams(format!("unknown dialog `{other}`"))),
        None => {
            r.dialog = None;
            r.options_open = false;
            r.remote_dev = None;
            r.picker = None;
            r.confirm = None;
            r.sync_preview = None;
        }
    }
    ok()
}

fn toggle_sync(s: &mut Session, p: &Value) -> CmdResult {
    let index = crate::params::req_usize(p, "index")?;
    let preview = s.remote.sync_preview.as_mut().ok_or_else(|| CommandError::Failed("no sync preview".into()))?;
    let item = preview.items.get_mut(index).ok_or_else(|| CommandError::BadParams(format!("no item {index}")))?;
    item.selected = p.get("selected").and_then(Value::as_bool).unwrap_or(!item.selected);
    ok()
}

// ---------------------------------------------------------------- servers

fn list_servers(s: &mut Session, _: &Value) -> CmdResult {
    let servers = s.remote.servers().to_vec();
    let warnings = s.remote.server_warnings().to_vec();
    Ok(json!({ "servers": servers, "warnings": warnings }))
}

fn server_config(p: &Value) -> Result<ServerConfig, CommandError> {
    let port = match p.get("port") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) if t.trim().is_empty() => None,
        Some(v) => Some(
            v.as_u64()
                .or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()))
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| CommandError::BadParams("`port` must be between 1 and 65535".into()))?,
        ),
    };
    Ok(ServerConfig {
        name: opt_str(p, "name").unwrap_or("").trim().to_string(),
        host: req_str(p, "host")?.trim().to_string(),
        port,
        user: opt_str(p, "user").unwrap_or("").trim().to_string(),
        identity_file: opt_str(p, "identityFile").unwrap_or("").trim().to_string(),
    })
}

fn add_server(s: &mut Session, p: &Value) -> CmdResult {
    let cfg = server_config(p)?;
    let name = cfg.name.clone();
    if let Err(e) = crate::remote::add_server(s, cfg) {
        if let Some(d) = &mut s.remote.dialog {
            d.error = Some(e.to_string());
        }
        return Err(e);
    }
    if let Some(d) = &mut s.remote.dialog {
        d.new_server = None;
        d.error = None;
        d.selected = Some(name.clone());
    }
    s.status = format!("SSH server {name} saved");
    ok()
}

fn remove_server(s: &mut Session, p: &Value) -> CmdResult {
    let name = req_str(p, "name")?.trim().to_string();
    let mut file = s.remote.own_servers().map_err(CommandError::Failed)?;
    let before = file.servers.len();
    file.servers.retain(|c| c.name.trim() != name);
    if file.servers.len() == before {
        return Err(CommandError::Failed(format!(
            "`{name}` is not a CodeCraft-defined server (hosts from ~/.ssh/config are read-only)"
        )));
    }
    s.remote.save_own_servers(&file).map_err(CommandError::Failed)?;
    ok()
}

fn test_server(s: &mut Session, p: &Value) -> CmdResult {
    let (name, server) = match opt_str(p, "server").filter(|n| !n.is_empty()) {
        Some(n) => (n.to_string(), deployment::server(s, n)?),
        None => {
            let mut cfg = server_config(p)?;
            if cfg.name.is_empty() {
                cfg.name = cfg.host.clone();
            }
            cfg.validate().map_err(failed)?;
            (cfg.name.clone(), cfg.to_server())
        }
    };
    // A fresh connection (not the shared master), so the test proves the settings work.
    let opts = codecraft_remote::SshOptions {
        program: s.remote.programs.ssh.clone(),
        control_path: None,
        connect_timeout: Some(10),
    };
    let args = rcmd::ssh_exec(&server, &opts, "echo codecraft-ok").map_err(failed)?;
    s.remote.tests.insert(name.clone(), TestStatus::Running);
    let id = start_op(
        s,
        &name,
        format!("Test connection to {name}"),
        vec![Step::run(&opts.program, args)],
        Follow::Test { server: name.clone() },
        false,
    )?;
    Ok(json!({ "operation": id }))
}

// ---------------------------------------------------------------- SSH session

fn start_session(s: &mut Session, p: &Value) -> CmdResult {
    let Some(name) = opt_str(p, "server").filter(|n| !n.is_empty()).map(str::to_string) else {
        let names = s.remote.server_names();
        return pick(s, "tools.startSshSession", "Start SSH Session", p, names);
    };
    let server = deployment::server(s, &name)?;
    let opts = s.remote.ssh_options(&name);
    s.remote.ensure_master(&server);
    let args = rcmd::ssh_interactive(&server, &opts).map_err(failed)?;
    let program = crate::remote::resolve_program(&opts.program).display().to_string();
    let tab = crate::terminal::open_command(s, &format!("SSH: {name}"), &program, &args, None)?;
    Ok(json!({ "tab": tab }))
}

// ---------------------------------------------------------------- Remote Host

fn browse(s: &mut Session, p: &Value) -> CmdResult {
    let name = match opt_str(p, "server").filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => {
            let default =
                s.ws.project
                    .as_ref()
                    .and_then(|pr| deployment::load(&pr.root).ok())
                    .and_then(|c| c.default_deployment().map(|d| d.server.clone()));
            match default.or_else(|| s.remote.browser.server.clone()) {
                Some(n) => n,
                None => {
                    let names = s.remote.server_names();
                    return pick(s, "tools.deployment.browseRemoteHost", "Browse Remote Host", p, names);
                }
            }
        }
    };
    if s.remote.server(&name).is_none() {
        return Err(CommandError::Failed(format!("unknown SSH server `{name}`")));
    }
    let path = match opt_str(p, "path").filter(|x| !x.is_empty()) {
        Some(x) => x.to_string(),
        None if s.remote.browser.server.as_deref() == Some(name.as_str()) => s.remote.browser.path.clone(),
        None => {
            s.ws.project
                .as_ref()
                .and_then(|pr| deployment::load(&pr.root).ok())
                .and_then(|c| c.server(&name).and_then(|d| d.remote_for("")))
                .unwrap_or_else(|| "~".into())
        }
    };
    let b = &mut s.remote.browser;
    b.server = Some(name.clone());
    b.path = path;
    b.selected = None;
    b.entries.clear();
    refresh_browser(s)?;
    Ok(json!({ "server": name }))
}

/// A remote path from `{path}` or `{name}` (an entry of the listed folder).
fn host_path(s: &Session, p: &Value) -> Result<String, CommandError> {
    if let Some(path) = opt_str(p, "path").filter(|x| !x.is_empty()) {
        return Ok(path.to_string());
    }
    let name = req_str(p, "name")?;
    check_name(name)?;
    Ok(rcmd::join(&s.remote.browser.path, name))
}

fn check_name(name: &str) -> Result<(), CommandError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.chars().any(char::is_control) {
        return Err(CommandError::BadParams(format!("invalid name `{name}`")));
    }
    Ok(())
}

fn host_open(s: &mut Session, p: &Value) -> CmdResult {
    let server_name = s.remote.browser.server.clone().ok_or_else(|| CommandError::Failed("no remote host".into()))?;
    let path = host_path(s, p)?;
    let entry = opt_str(p, "name").and_then(|n| s.remote.browser.entries.iter().find(|e| e.name == n)).cloned();
    if entry.as_ref().is_some_and(|e| e.is_dir_like())
        || (entry.is_none() && opt_str(p, "name").is_none() && path.ends_with('/'))
    {
        s.remote.browser.path = path;
        s.remote.browser.selected = None;
        refresh_browser(s)?;
        return Ok(json!({ "listing": true }));
    }
    if let Some(e) = &entry
        && e.size > crate::remote::MAX_OPEN_BYTES
    {
        return Err(CommandError::Failed(format!("{} is too large to open ({} bytes)", e.name, e.size)));
    }
    let server = deployment::server(s, &server_name)?;
    let cache = s
        .remote
        .cache_dir()
        .ok_or_else(|| CommandError::Failed("cannot find the home directory; set CODECRAFT_HOME".into()))?;
    let file_name = Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let local = cache
        .join(crate::remote::safe_name(&server_name))
        .join("files")
        .join(crate::remote::short_hash(&path))
        .join(&file_name);
    if s.ws.docs().any(|d| d.path.as_deref() == Some(local.as_path()) && d.doc.is_modified()) {
        s.open_file(&local)?;
        return Ok(json!({ "local": local }));
    }
    let steps = deployment::download_file_steps(s, &server, &path, &local)?;
    let follow = Follow::OpenRemote { server: server_name.clone(), remote: path.clone(), local: local.clone() };
    let id = start_op(s, &server_name, format!("Download {server_name}:{path}"), steps, follow, true)?;
    Ok(json!({ "operation": id, "local": local }))
}

fn host_op(s: &mut Session, summary: String, remote_cmd: String) -> CmdResult {
    let name = s.remote.browser.server.clone().ok_or_else(|| CommandError::Failed("no remote host".into()))?;
    let server = deployment::server(s, &name)?;
    let opts = s.remote.ssh_options(&name);
    s.remote.ensure_master(&server);
    let args = rcmd::ssh_exec(&server, &opts, &remote_cmd).map_err(failed)?;
    let id = start_op(s, &name, summary, vec![Step::run(&opts.program, args)], Follow::RefreshBrowser, true)?;
    Ok(json!({ "operation": id }))
}

fn host_mkdir(s: &mut Session, p: &Value) -> CmdResult {
    let name = req_str(p, "name")?.trim();
    check_name(name)?;
    let path = rcmd::join(&s.remote.browser.path, name);
    host_op(s, format!("New folder {path}"), rcmd::mkdir_command(&path))
}

fn host_rename(s: &mut Session, p: &Value) -> CmdResult {
    let from = host_path(s, p)?;
    let new = req_str(p, "newName")?.trim();
    check_name(new)?;
    let to = rcmd::join(&rcmd::parent_of(&from), new);
    host_op(s, format!("Rename {from} → {new}"), rcmd::rename_command(&from, &to))
}

fn host_delete(s: &mut Session, p: &Value) -> CmdResult {
    let server = s.remote.browser.server.clone().ok_or_else(|| CommandError::Failed("no remote host".into()))?;
    let paths: Vec<String> = match p.get("names").and_then(Value::as_array) {
        Some(names) => names
            .iter()
            .map(|n| {
                let n = n.as_str().ok_or_else(|| CommandError::BadParams("`names` must be strings".into()))?;
                check_name(n)?;
                Ok(rcmd::join(&s.remote.browser.path, n))
            })
            .collect::<Result<_, CommandError>>()?,
        None => vec![host_path(s, p)?],
    };
    let cmd = rcmd::delete_command(&paths).map_err(failed)?;
    if !bool_or(p, "confirm", false) {
        s.remote.confirm = Some(ConfirmDelete { server, paths });
        return Ok(json!({ "confirm": true }));
    }
    s.remote.confirm = None;
    host_op(s, format!("Delete {}", paths.join(", ")), cmd)
}

// ---------------------------------------------------------------- Remote Development

fn remote_development(s: &mut Session, p: &Value) -> CmdResult {
    let server = opt_str(p, "server").filter(|x| !x.is_empty()).map(str::to_string);
    let path = opt_str(p, "path").map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    let (Some(server_name), Some(path)) = (server.clone(), path.clone()) else {
        s.remote.reload_servers();
        let first = server.or_else(|| s.remote.server_names().into_iter().next()).unwrap_or_default();
        s.remote.remote_dev =
            Some(RemoteDevDialog { server: first, path: path.unwrap_or_else(|| "~/".into()), error: None });
        return Ok(json!({ "dialog": true }));
    };
    let result = start_remote_dev(s, &server_name, &path);
    if let (Err(e), Some(d)) = (&result, &mut s.remote.remote_dev) {
        d.error = Some(e.to_string());
    }
    result
}

fn start_remote_dev(s: &mut Session, server_name: &str, path: &str) -> CmdResult {
    let server = deployment::server(s, server_name)?;
    rcmd::transfer_path(path).map_err(failed)?;
    if matches!(path.trim_end_matches('/'), "" | "~" | "/") {
        return Err(CommandError::Failed("choose a project folder, not the home or root folder".into()));
    }
    let cache = s
        .remote
        .cache_dir()
        .ok_or_else(|| CommandError::Failed("cannot find the home directory; set CODECRAFT_HOME".into()))?;
    let leaf = Path::new(path.trim_end_matches('/')).file_name().and_then(|n| n.to_str()).unwrap_or("project");
    let local = cache.join(crate::remote::safe_name(server_name)).join(format!(
        "{}-{}",
        crate::remote::safe_name(leaf),
        crate::remote::short_hash(path).get(..8).unwrap_or("")
    ));
    let excludes: Vec<String> =
        ["target", ".codecraft", "node_modules", "__pycache__"].iter().map(|x| (*x).to_string()).collect();
    let steps = deployment::folder_steps(s, &server, codecraft_remote::Direction::Download, &local, path, &excludes)?;
    // The upload step of folder_steps is a mkdir on the remote side; a download starts with a
    // local mkdir, so the steps are used as they are.
    let follow = Follow::RemoteDev { server: server_name.to_string(), remote: path.to_string(), local: local.clone() };
    let id = start_op(s, server_name, format!("Mirror {server_name}:{path}"), steps, follow, true)?;
    s.ws.layout.show(ToolWindowId::FileTransfer);
    Ok(json!({ "operation": id, "local": local }))
}

/// The mirror is in place: write its deployment (mapping, default server, automatic upload) and
/// an SSH run configuration, then open it.
pub(crate) fn remote_dev_ready(s: &mut Session, server: &str, remote: &str, local: &Path) {
    let mut cfg = deployment::load(local).unwrap_or_default();
    if let Some(dep) = cfg.ensure_server(server) {
        dep.mappings = vec![PathMapping { local: String::new(), remote: remote.to_string() }];
    }
    cfg.default_server = Some(server.to_string());
    cfg.auto_upload = true;
    cfg.remote_development = Some(server.to_string());
    if let Err(e) = deployment::save(local, &cfg) {
        s.notify(codecraft_workspace::NotificationLevel::Error, "Remote Development", e);
        return;
    }
    let runs = local.join(crate::run_config_store::FILE);
    if !runs.exists() && local.join("Cargo.toml").is_file() {
        let config = crate::run_config_store::UserRunConfig {
            name: format!("Run on {server}"),
            command: "run".into(),
            target: crate::run_targets::RunTarget::Ssh(crate::ssh_target::SshTarget {
                server: server.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text =
            serde_json::to_string_pretty(&json!({ "version": 1, "configurations": [config] })).unwrap_or_default();
        if let Some(dir) = runs.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&runs, text) {
            log::warn!("remote development run configuration: {e}");
        }
    }
    s.remote.remote_dev = None;
    s.open_project(local);
    s.status = format!("Remote Development: {server}:{remote} (automatic upload on)");
}

fn sync_from_remote(s: &mut Session, p: &Value) -> CmdResult {
    let root = deployment::project_root(s)?;
    let cfg = deployment::load(&root).map_err(CommandError::Failed)?;
    let name = match opt_str(p, "server").filter(|x| !x.is_empty()) {
        Some(n) => n.to_string(),
        None => cfg
            .remote_development
            .clone()
            .or_else(|| cfg.default_deployment().map(|d| d.server.clone()))
            .ok_or_else(|| CommandError::Failed("no default deployment server".into()))?,
    };
    let dep = cfg.server(&name).cloned().ok_or_else(|| CommandError::Failed(format!("`{name}` has no mapping")))?;
    let remote = dep.remote_for("").ok_or_else(|| CommandError::Failed("the project root is not mapped".into()))?;
    let server = deployment::server(s, &name)?;
    let _ = s.save_all();
    let excludes = dep.rsync_excludes("");
    let steps = deployment::folder_steps(s, &server, codecraft_remote::Direction::Download, &root, &remote, &excludes)?;
    let id = start_op(s, &name, format!("Sync from {name}:{remote}"), steps, Follow::Reload, true)?;
    s.ws.layout.show(ToolWindowId::FileTransfer);
    Ok(json!({ "operation": id }))
}

// ---------------------------------------------------------------- data source tunnels

fn set_tunnel(s: &mut Session, p: &Value) -> CmdResult {
    let id = req_str(p, "dataSource")?.to_string();
    let server = opt_str(p, "server").map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    if let Some(n) = &server {
        let known = s.remote.server(n);
        if known.is_none() {
            return Err(CommandError::Failed(format!("unknown SSH server `{n}`")));
        }
    }
    let ds =
        s.db.sources
            .iter_mut()
            .find(|d| d.config.id == id)
            .ok_or_else(|| CommandError::Failed(format!("no data source `{id}`")))?;
    ds.config.ssh_tunnel = server.clone();
    let name = ds.config.name.clone();
    s.db.disconnect(Some(&id));
    // Saves the data sources (an edit without changes keeps the new field).
    s.execute("db.editDataSource", &json!({ "dataSource": id, "name": name }))?;
    Ok(json!({ "dataSource": id, "sshTunnel": server }))
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
