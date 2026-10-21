//! Deployment (M11 track S): the project's `.codecraft/deployment.json` and the transfer
//! operations behind Tools › Deployment (upload, download, compare, sync with a preview).
//!
//! Single files go through `ssh` (`cat` on the remote side, written to a temp name and renamed),
//! which works for any file name; folders go through `rsync` over ssh, or `scp` when rsync is
//! missing.

use std::path::{Path, PathBuf};

use codecraft_remote::command::{self as rcmd, Direction, RsyncSpec};
use codecraft_remote::deploy::{self, DeploymentConfig, ServerDeployment};
use codecraft_remote::rsync::Change;
use codecraft_remote::server::SshServer;

use crate::remote::{Follow, Step, failed, start_op};
use crate::{CommandError, Session};

/// Relative to the project root.
pub const FILE: &str = ".codecraft/deployment.json";

pub fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

/// The project's deployment configuration (empty when missing; an unreadable file is an error so
/// it is never overwritten by accident).
pub fn load(root: &Path) -> Result<DeploymentConfig, String> {
    let p = path(root);
    match std::fs::read_to_string(&p) {
        Ok(t) => DeploymentConfig::parse(&t).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeploymentConfig::default()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

pub fn save(root: &Path, cfg: &DeploymentConfig) -> Result<(), String> {
    for d in &cfg.servers {
        d.validate().map_err(|e| e.to_string())?;
    }
    let p = path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    crate::remote::write_atomic(&p, &cfg.to_json().map_err(|e| e.to_string())?)
}

pub(crate) fn project_root(s: &Session) -> Result<PathBuf, CommandError> {
    s.ws.project.as_ref().map(|p| p.root.clone()).ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

pub(crate) fn server(s: &mut Session, name: &str) -> Result<SshServer, CommandError> {
    s.remote.server(name).ok_or_else(|| CommandError::Failed(format!("unknown SSH server `{name}`")))
}

/// The deployment of `server` in the open project.
pub(crate) fn deployment_of(s: &Session, server: &str) -> Result<(PathBuf, ServerDeployment), CommandError> {
    let root = project_root(s)?;
    let cfg = load(&root).map_err(CommandError::Failed)?;
    let dep = cfg.server(server).cloned().ok_or_else(|| {
        CommandError::Failed(format!(
            "`{server}` has no mappings for this project: add one in Tools › Deployment › Configuration…"
        ))
    })?;
    Ok((root, dep))
}

/// The remote path of a local path under a deployment (with the project-relative path).
pub(crate) fn mapped(root: &Path, dep: &ServerDeployment, local: &Path) -> Result<(String, String), CommandError> {
    let rel = deploy::rel_path(root, local)
        .ok_or_else(|| CommandError::Failed(format!("{} is outside the project", local.display())))?;
    let remote = dep.remote_for(&rel).ok_or_else(|| {
        CommandError::Failed(format!(
            "{} is not mapped on `{}` (or it is excluded)",
            if rel.is_empty() { "the project root" } else { rel.as_str() },
            dep.server
        ))
    })?;
    Ok((remote, rel))
}

/// A temp file name next to `local` for a download in progress.
fn partial(local: &Path) -> PathBuf {
    let name = local.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    local.with_file_name(format!(".{name}.codecraft-download"))
}

/// Upload one file (`ssh … 'cat > remote'` with the file on stdin).
pub(crate) fn upload_file_op(
    s: &mut Session,
    server_name: &str,
    local: &Path,
    remote: &str,
    verb: &str,
) -> Result<u64, CommandError> {
    let server = server(s, server_name)?;
    let opts = s.remote.ssh_options(server_name);
    s.remote.ensure_master(&server);
    if s.remote.project.is_some() {
        if s.remote.busy() {
            return Err(failed("wait for the current remote operation before uploading"));
        }
        let root = project_root(s)?;
        let rel = deploy::rel_path(&root, local).ok_or_else(|| failed("file is outside the remote project"))?;
        let (_, baseline) = crate::remote::mirror_paths(&root)?;
        let snapshot =
            s.remote.cache_dir().ok_or_else(|| failed("cannot locate remote cache"))?.join(".uploads").join(format!(
                "{}-{}-{}",
                std::process::id(),
                crate::remote::now_ms(),
                crate::remote::short_hash(&local.display().to_string())
            ));
        let step = Step::CheckedUpload {
            root,
            server,
            options: opts,
            local: local.to_path_buf(),
            remote: remote.into(),
            baseline: baseline.join(rel),
            snapshot,
        };
        return start_op(
            s,
            server_name,
            format!("{verb} {} (check remote changes)", local.display()),
            vec![step],
            Follow::ProjectUpload { local: local.to_path_buf() },
            true,
        );
    }
    let args = rcmd::ssh_exec(&server, &opts, &rcmd::put_command(remote)).map_err(failed)?;
    let step = Step::Run { program: opts.program.clone(), args, stdin: Some(local.to_path_buf()), stdout_to: None };
    let name = local.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    start_op(s, server_name, format!("{verb} {name} → {server_name}:{remote}"), vec![step], Follow::Nothing, true)
}

/// Steps that download one remote file to `local` (temp file, then rename).
pub(crate) fn download_file_steps(
    s: &mut Session,
    server: &SshServer,
    remote: &str,
    local: &Path,
) -> Result<Vec<Step>, CommandError> {
    let opts = s.remote.ssh_options(&server.name);
    s.remote.ensure_master(server);
    let args = rcmd::ssh_exec(server, &opts, &rcmd::cat_command(remote)).map_err(failed)?;
    let tmp = partial(local);
    let mut steps = Vec::new();
    if let Some(dir) = local.parent() {
        steps.push(Step::Mkdir(dir.to_path_buf()));
    }
    steps.push(Step::Run { program: opts.program.clone(), args, stdin: None, stdout_to: Some(tmp.clone()) });
    steps.push(Step::Rename { from: tmp, to: local.to_path_buf() });
    Ok(steps)
}

/// Steps that copy a folder (rsync, else scp) in one direction. The remote folder is created
/// first for uploads.
pub(crate) fn folder_steps(
    s: &mut Session,
    server: &SshServer,
    direction: Direction,
    local: &Path,
    remote: &str,
    excludes: &[String],
) -> Result<Vec<Step>, CommandError> {
    let opts = s.remote.ssh_options(&server.name);
    s.remote.ensure_master(server);
    let mut steps = Vec::new();
    match direction {
        Direction::Upload => {
            let mk = rcmd::ssh_exec(server, &opts, &rcmd::mkdir_command(remote)).map_err(failed)?;
            steps.push(Step::run(&opts.program, mk));
        }
        Direction::Download => steps.push(Step::Mkdir(local.to_path_buf())),
    }
    if s.remote.has_rsync() {
        let spec = RsyncSpec {
            direction,
            local,
            remote,
            dir: true,
            excludes,
            dry_run: false,
            delete: false,
            update: false,
            files_from: None,
        };
        steps.push(Step::run(&s.remote.programs.rsync, rcmd::rsync_args(server, &opts, &spec).map_err(failed)?));
    } else {
        let args = rcmd::scp_args(server, &opts, direction, local, remote, true).map_err(failed)?;
        steps.push(Step::run(&s.remote.programs.scp, args));
    }
    Ok(steps)
}

/// Upload a file or folder of the project to a deployment server.
pub(crate) fn upload(s: &mut Session, server_name: &str, local: &Path) -> Result<u64, CommandError> {
    let (root, dep) = deployment_of(s, server_name)?;
    let (remote, rel) = mapped(&root, &dep, local)?;
    if local.is_file() {
        return upload_file_op(s, server_name, local, &remote, "Upload");
    }
    if !local.is_dir() {
        return Err(CommandError::Failed(format!("{} does not exist", local.display())));
    }
    let server = server(s, server_name)?;
    let excludes = dep.rsync_excludes(&rel);
    let scp = !s.remote.has_rsync();
    let steps = folder_steps(s, &server, Direction::Upload, local, &remote, &excludes)?;
    let note = if scp { " (scp: exclusions not applied)" } else { "" };
    let label = if rel.is_empty() { "project" } else { rel.as_str() };
    start_op(s, server_name, format!("Upload {label} → {server_name}:{remote}{note}"), steps, Follow::Nothing, true)
}

/// Upload several files (Upload All Open Files).
pub(crate) fn upload_files(s: &mut Session, server_name: &str, files: &[PathBuf]) -> Result<u64, CommandError> {
    let (root, dep) = deployment_of(s, server_name)?;
    let server = server(s, server_name)?;
    let opts = s.remote.ssh_options(server_name);
    s.remote.ensure_master(&server);
    let mut steps = Vec::new();
    for f in files {
        let Ok((remote, _)) = mapped(&root, &dep, f) else { continue };
        let args = rcmd::ssh_exec(&server, &opts, &rcmd::put_command(&remote)).map_err(failed)?;
        steps.push(Step::Run { program: opts.program.clone(), args, stdin: Some(f.clone()), stdout_to: None });
    }
    if steps.is_empty() {
        return Err(CommandError::Failed(format!("none of the open files is mapped on `{server_name}`")));
    }
    let n = steps.len();
    start_op(s, server_name, format!("Upload {n} open file(s) → {server_name}"), steps, Follow::Nothing, true)
}

/// Download a file or folder of the project from a deployment server (open documents reload).
pub(crate) fn download(s: &mut Session, server_name: &str, local: &Path) -> Result<u64, CommandError> {
    let (root, dep) = deployment_of(s, server_name)?;
    let (remote, rel) = mapped(&root, &dep, local)?;
    let server = server(s, server_name)?;
    let label = if rel.is_empty() { "project".to_string() } else { rel.clone() };
    let steps = if local.is_dir() {
        let excludes = dep.rsync_excludes(&rel);
        folder_steps(s, &server, Direction::Download, local, &remote, &excludes)?
    } else {
        download_file_steps(s, &server, &remote, local)?
    };
    start_op(s, server_name, format!("Download {label} ← {server_name}:{remote}"), steps, Follow::Reload, true)
}

/// Compare a local file with its deployed version (fetched to a temp file, then a diff).
pub(crate) fn compare(s: &mut Session, server_name: &str, local: &Path) -> Result<u64, CommandError> {
    if !local.is_file() {
        return Err(CommandError::Failed("select a file to compare".into()));
    }
    let (root, dep) = deployment_of(s, server_name)?;
    let (remote, _) = mapped(&root, &dep, local)?;
    let server = server(s, server_name)?;
    let cache = s
        .remote
        .cache_dir()
        .ok_or_else(|| CommandError::Failed("cannot find the home directory; set CODECRAFT_HOME".into()))?;
    let fetched =
        cache.join("compare").join(format!("{}-{}", crate::remote::now_ms(), crate::remote::short_hash(&remote)));
    let steps = download_file_steps(s, &server, &remote, &fetched)?;
    let follow = Follow::Compare {
        server: server_name.to_string(),
        remote: remote.clone(),
        local: local.to_path_buf(),
        fetched,
    };
    start_op(s, server_name, format!("Fetch {server_name}:{remote} to compare"), steps, follow, true)
}

/// Sync with Deployed: preview the differences of a folder in both directions
/// (`rsync --dry-run --itemize-changes --update`), shown by `remote.sync.*`.
pub(crate) fn sync_preview(s: &mut Session, server_name: &str, local: &Path) -> Result<u64, CommandError> {
    let root = project_root(s)?;
    let cfg = crate::deployment::load(&root).map_err(CommandError::Failed)?;
    let (_, dep) = deployment_of(s, server_name)?;
    let (remote, rel) = mapped(&root, &dep, local)?;
    if !local.is_dir() {
        return Err(CommandError::Failed("Sync works on folders; use Upload or Download for a file".into()));
    }
    let server = server(s, server_name)?;
    let excludes = dep.rsync_excludes(&rel);
    if !s.remote.has_rsync() {
        // No preview without rsync: upload with scp.
        let steps = folder_steps(s, &server, Direction::Upload, local, &remote, &excludes)?;
        return start_op(
            s,
            server_name,
            format!("Sync (scp upload) → {server_name}:{remote}"),
            steps,
            Follow::Nothing,
            true,
        );
    }
    let opts = s.remote.ssh_options(server_name);
    s.remote.ensure_master(&server);
    let mk = rcmd::ssh_exec(&server, &opts, &rcmd::mkdir_command(&remote)).map_err(failed)?;
    let mut steps = vec![Step::run(&opts.program, mk)];
    for (direction, delete) in [(Direction::Upload, cfg.delete_target_items), (Direction::Download, false)] {
        let spec = RsyncSpec {
            direction,
            local,
            remote: &remote,
            dir: true,
            excludes: &excludes,
            dry_run: true,
            delete,
            update: true,
            files_from: None,
        };
        steps.push(Step::run(&s.remote.programs.rsync, rcmd::rsync_args(&server, &opts, &spec).map_err(failed)?));
    }
    let follow = Follow::SyncPreview {
        server: server_name.to_string(),
        local: local.to_path_buf(),
        remote: remote.clone(),
        excludes,
    };
    start_op(s, server_name, format!("Compare {} with {server_name}:{remote}", local.display()), steps, follow, false)
}

/// Apply the selected items of the sync preview.
pub(crate) fn apply_sync(s: &mut Session) -> Result<u64, CommandError> {
    let preview = s.remote.sync_preview.clone().ok_or_else(|| CommandError::Failed("no sync preview".into()))?;
    let server = server(s, &preview.server)?;
    let opts = s.remote.ssh_options(&preview.server);
    s.remote.ensure_master(&server);
    let cache = s
        .remote
        .cache_dir()
        .ok_or_else(|| CommandError::Failed("cannot find the home directory; set CODECRAFT_HOME".into()))?;
    let stamp = format!("{}-{}", crate::remote::now_ms(), crate::remote::short_hash(&preview.remote));
    let selected = |d: Direction, delete: bool| -> Vec<String> {
        preview
            .items
            .iter()
            .filter(|i| i.selected && i.direction == d && (i.change == Change::Delete) == delete && !i.is_dir)
            .map(|i| i.path.clone())
            .collect()
    };
    let mut steps = Vec::new();
    let mut n = 0usize;
    for direction in [Direction::Upload, Direction::Download] {
        let files = selected(direction, false);
        if files.is_empty() {
            continue;
        }
        n += files.len();
        let tag = if direction == Direction::Upload { "up" } else { "down" };
        let list = cache.join("sync").join(format!("{stamp}-{tag}.txt"));
        steps.push(Step::Write { path: list.clone(), text: files.join("\n") + "\n" });
        let spec = RsyncSpec {
            direction,
            local: &preview.local,
            remote: &preview.remote,
            dir: true,
            excludes: &preview.excludes,
            dry_run: false,
            delete: false,
            update: false,
            files_from: Some(&list),
        };
        steps.push(Step::run(&s.remote.programs.rsync, rcmd::rsync_args(&server, &opts, &spec).map_err(failed)?));
    }
    let mut deletes: Vec<String> = preview
        .items
        .iter()
        .filter(|i| i.selected && i.direction == Direction::Upload && i.change == Change::Delete)
        .map(|i| rcmd::join(&preview.remote, &i.path))
        .collect();
    deletes.dedup();
    if !deletes.is_empty() {
        n += deletes.len();
        let cmd = rcmd::delete_command(&deletes).map_err(failed)?;
        steps.push(Step::run(&opts.program, rcmd::ssh_exec(&server, &opts, &cmd).map_err(failed)?));
    }
    s.remote.sync_preview = None;
    if steps.is_empty() {
        return Err(CommandError::Failed("nothing selected to sync".into()));
    }
    let name = preview.server.clone();
    start_op(s, &name, format!("Sync {n} item(s) with {name}:{}", preview.remote), steps, Follow::Reload, true)
}
