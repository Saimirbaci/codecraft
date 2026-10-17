//! An SSH host as a run target: sync the project, then run the configuration in the mapped
//! remote directory (M11 track S).
//!
//! The wrapped command is a local `sh -c` script, so nothing blocks the UI thread and the Run tab
//! shows the sync and the program's output: `ssh … mkdir -p <root> && rsync … && exec ssh …
//! 'sh -c "cd <dir> && exec env … <program> <args>"'`. Every remote word is quoted. Files the
//! run writes for CodeCraft (a test report) are downloaded when it ends (M16 track RT).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use codecraft_remote::command::{self as rcmd, Direction, RsyncSpec};
use codecraft_remote::deploy::{self, DEFAULT_EXCLUDES, ServerDeployment};
use codecraft_remote::server::SshServer;
use codecraft_remote::{SshOptions, quote};

use crate::remote::failed;
use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SshTarget {
    /// SSH server name (a `~/.ssh/config` host or a CodeCraft-defined server).
    pub server: String,
    /// The remote project folder; empty = the project root's deployment mapping on the server.
    pub remote_dir: String,
    /// Upload the project (rsync, deployment exclusions) before each run.
    pub sync: bool,
    /// The Python interpreter on the server (`/srv/app/.venv/bin/python`); empty = the
    /// configuration's interpreter by name (`python3`). Settings › Python › Remote interpreters
    /// sets it (M16 track RT).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub python: String,
}

impl Default for SshTarget {
    fn default() -> Self {
        Self { server: String::new(), remote_dir: String::new(), sync: true, python: String::new() }
    }
}

impl SshTarget {
    pub fn label(&self) -> String {
        if self.remote_dir.is_empty() { self.server.clone() } else { format!("{}:{}", self.server, self.remote_dir) }
    }
}

/// `to` relative to the folder `from` (both `/`-separated, relative to the same root).
fn relative(from: &str, to: &str) -> String {
    let a: Vec<&str> = from.split('/').filter(|x| !x.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|x| !x.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; a.len().saturating_sub(common)];
    parts.extend(b.get(common..).unwrap_or(&[]));
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

/// A local path inside the project becomes a path relative to the remote working folder
/// (`cwd_rel`, relative to the project), which also works when the remote root is `~/…`.
fn map_local(word: &str, root: &Path, cwd_rel: &str) -> Option<String> {
    if !Path::new(word).is_absolute() {
        return None;
    }
    let rel = deploy::rel_path(root, Path::new(word))?;
    Some(relative(cwd_rel, &rel))
}

/// An SSH target resolved for the open project: the server, how to run ssh and where the
/// project is on the server.
#[derive(Clone, Debug)]
pub(crate) struct Resolved {
    pub server: SshServer,
    pub opts: SshOptions,
    /// The local project root.
    pub root: PathBuf,
    /// The project folder on the server (as configured: absolute, `~/…` or relative to home).
    pub remote_root: String,
    /// rsync exclusions for a project upload.
    pub excludes: Vec<String>,
}

/// Resolve `t` (reads the server list and the deployment mapping; no network).
pub(crate) fn resolve(s: &mut Session, t: &SshTarget) -> Result<Resolved, CommandError> {
    let name = t.server.trim();
    if name.is_empty() {
        return Err(CommandError::Failed("choose an SSH server for the run target".into()));
    }
    let server = crate::deployment::server(s, name)?;
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let dep: Option<ServerDeployment> = crate::deployment::load(&root).ok().and_then(|c| c.server(name).cloned());
    let remote_root = match (t.remote_dir.trim(), &dep) {
        (dir, _) if !dir.is_empty() => dir.to_string(),
        (_, Some(d)) => d.remote_for("").ok_or_else(|| {
            CommandError::Failed(format!(
                "the project root is not mapped on `{name}`: set the run target's remote folder"
            ))
        })?,
        (_, None) => {
            return Err(CommandError::Failed(format!(
                "map the project to a folder on `{name}` (Tools › Deployment › Configuration…) or set the run target's remote folder"
            )));
        }
    };
    let excludes: Vec<String> = match &dep {
        Some(d) => d.rsync_excludes(""),
        None => DEFAULT_EXCLUDES.iter().map(|x| (*x).to_string()).collect(),
    };
    let opts = s.remote.ssh_options(name);
    s.remote.ensure_master(&server);
    Ok(Resolved { server, opts, root, remote_root, excludes })
}

/// The words of the project upload (`rsync …` or the `scp` fallback), the program first.
pub(crate) fn upload_words(s: &Session, r: &Resolved) -> Result<Vec<String>, CommandError> {
    if s.remote.has_rsync() {
        let spec = RsyncSpec {
            direction: Direction::Upload,
            local: &r.root,
            remote: &r.remote_root,
            dir: true,
            excludes: &r.excludes,
            dry_run: false,
            delete: false,
            update: false,
            files_from: None,
        };
        let mut w = vec![s.remote.programs.rsync.clone()];
        w.extend(rcmd::rsync_args(&r.server, &r.opts, &spec).map_err(failed)?);
        Ok(w)
    } else {
        let mut w = vec![s.remote.programs.scp.clone()];
        w.extend(rcmd::scp_args(&r.server, &r.opts, Direction::Upload, &r.root, &r.remote_root, true).map_err(failed)?);
        Ok(w)
    }
}

/// The words that copy one remote file (or folder) to a local path (rsync, else scp).
pub(crate) fn download_words(
    s: &Session,
    r: &Resolved,
    remote: &str,
    local: &Path,
    dir: bool,
) -> Result<Vec<String>, CommandError> {
    if s.remote.has_rsync() {
        let spec = RsyncSpec {
            direction: Direction::Download,
            local,
            remote,
            dir,
            excludes: &[],
            dry_run: false,
            delete: false,
            update: false,
            files_from: None,
        };
        let mut w = vec![s.remote.programs.rsync.clone()];
        w.extend(rcmd::rsync_args(&r.server, &r.opts, &spec).map_err(failed)?);
        Ok(w)
    } else {
        let mut w = vec![s.remote.programs.scp.clone()];
        w.extend(rcmd::scp_args(&r.server, &r.opts, Direction::Download, local, remote, dir).map_err(failed)?);
        Ok(w)
    }
}

/// `ssh … <dest> 'sh -c …'`, the program first.
pub(crate) fn exec_words(r: &Resolved, script: &str) -> Result<Vec<String>, CommandError> {
    let run = rcmd::ssh_exec(&r.server, &r.opts, &quote::sh_c(script)).map_err(failed)?;
    Ok(std::iter::once(r.opts.program.clone()).chain(run).collect())
}

/// The local `sh -c` steps of a project upload: `ssh mkdir -p <root> && rsync …`.
pub(crate) fn sync_line(s: &Session, r: &Resolved) -> Result<String, CommandError> {
    let mkdir = rcmd::ssh_exec(&r.server, &r.opts, &rcmd::mkdir_command(&r.remote_root)).map_err(failed)?;
    let mkdir: Vec<String> = std::iter::once(r.opts.program.clone()).chain(mkdir).collect();
    Ok(format!("{} && {}", quote::join(&mkdir), quote::join(&upload_words(s, r)?)))
}

pub(crate) fn wrap(s: &mut Session, t: &SshTarget, cmd: ProcessCommand) -> Result<ProcessCommand, CommandError> {
    wrap_with(s, t, cmd, &[])
}

/// [`wrap`], then copy each `(remote path, local path)` back when the program ends (its exit
/// status is kept). Remote paths are relative to the remote project folder or absolute.
pub(crate) fn wrap_with(
    s: &mut Session,
    t: &SshTarget,
    cmd: ProcessCommand,
    downloads: &[(String, PathBuf)],
) -> Result<ProcessCommand, CommandError> {
    let r = resolve(s, t)?;
    let root = r.root.clone();
    let cwd_rel = deploy::rel_path(&root, &cmd.cwd).unwrap_or_default();
    let remote_cwd = rcmd::join(&r.remote_root, &cwd_rel);
    // The target's own interpreter replaces the configuration's (a virtualenv inside the project,
    // `.venv/bin/python`, is not portable) when one is set.
    let program = match (crate::docker_target::target_python(&t.python, &cmd.program), map_local(&cmd.program, &root, &cwd_rel)) {
        (Some(py), _) => py,
        (None, Some(p)) => p,
        // A local tool path means nothing on the server: run the tool by name there.
        (None, None) if Path::new(&cmd.program).is_absolute() => {
            Path::new(&cmd.program).file_name().and_then(|n| n.to_str()).unwrap_or(&cmd.program).to_string()
        }
        (None, None) => cmd.program.clone(),
    };
    let args: Vec<String> =
        cmd.args.iter().map(|a| map_local(a, &root, &cwd_rel).unwrap_or_else(|| a.clone())).collect();
    let script = quote::run_script(&remote_cwd, &cmd.env, &program, &args).map_err(failed)?;
    let mut run = exec_words(&r, &script)?;
    let mut tail = String::new();
    for (remote, local) in downloads {
        let remote = if remote.starts_with('/') || remote.starts_with('~') {
            remote.clone()
        } else {
            rcmd::join(&r.remote_root, remote)
        };
        let words = download_words(s, &r, &remote, local, false)?;
        let parent = local.parent().map(|p| p.display().to_string()).unwrap_or_else(|| ".".into());
        tail.push_str(&format!("; mkdir -p {} && {} >/dev/null", quote::sh_quote(&parent), quote::join(&words)));
    }
    if !t.sync && tail.is_empty() {
        let program = run.remove(0);
        return Ok(ProcessCommand { program, args: run, cwd: root, env: Vec::new(), build: cmd.build });
    }
    let run_line = if tail.is_empty() {
        format!("exec {}", quote::join(&run))
    } else {
        format!("{}; cc_st=$?{tail}; exit $cc_st", quote::join(&run))
    };
    let line = if t.sync { format!("{} && {{ {run_line}; }}", sync_line(s, &r)?) } else { run_line };
    let mut wrapped = ProcessCommand::shell(line, root);
    wrapped.build = cmd.build;
    Ok(wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_labels() {
        let t = SshTarget::default();
        assert!(t.sync);
        let back: SshTarget = serde_json::from_str(r#"{"server":"web"}"#).unwrap();
        assert_eq!(back.server, "web");
        assert!(back.sync);
        assert!(back.python.is_empty());
        assert_eq!(SshTarget { remote_dir: "/srv".into(), ..back }.label(), "web:/srv");
    }

    #[test]
    fn local_paths_become_relative_to_the_working_folder() {
        let root = Path::new("/p/proj");
        assert_eq!(map_local("/p/proj/src/main.py", root, "").as_deref(), Some("src/main.py"));
        assert_eq!(map_local("/p/proj/run.sh", root, "scripts/x").as_deref(), Some("../../run.sh"));
        assert_eq!(map_local("/p/proj/scripts", root, "scripts").as_deref(), Some("."));
        assert_eq!(map_local("/elsewhere/x", root, ""), None);
        assert_eq!(map_local("relative", root, ""), None);
    }
}
