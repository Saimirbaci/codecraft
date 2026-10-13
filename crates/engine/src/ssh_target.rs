//! An SSH host as a run target: sync the project, then run the configuration in the mapped
//! remote directory (M11 track S).
//!
//! The wrapped command is a local `sh -c` script, so nothing blocks the UI thread and the Run tab
//! shows the sync and the program's output: `ssh … mkdir -p <root> && rsync … && exec ssh …
//! 'sh -c "cd <dir> && exec env … <program> <args>"'`. Every remote word is quoted.

use std::path::Path;

use serde::{Deserialize, Serialize};

use codecraft_remote::command::{self as rcmd, Direction, RsyncSpec};
use codecraft_remote::deploy::{self, DEFAULT_EXCLUDES};
use codecraft_remote::quote;

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
}

impl Default for SshTarget {
    fn default() -> Self {
        Self { server: String::new(), remote_dir: String::new(), sync: true }
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

pub(crate) fn wrap(s: &mut Session, t: &SshTarget, cmd: ProcessCommand) -> Result<ProcessCommand, CommandError> {
    let name = t.server.trim();
    if name.is_empty() {
        return Err(CommandError::Failed("choose an SSH server for the run target".into()));
    }
    let server = crate::deployment::server(s, name)?;
    let root = s.project_root().ok_or_else(|| CommandError::Failed("no project is open".into()))?;
    let dep = crate::deployment::load(&root).ok().and_then(|c| c.server(name).cloned());
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
    let cwd_rel = deploy::rel_path(&root, &cmd.cwd).unwrap_or_default();
    let remote_cwd = rcmd::join(&remote_root, &cwd_rel);
    let program = match map_local(&cmd.program, &root, &cwd_rel) {
        Some(p) => p,
        // A local tool path means nothing on the server: run the tool by name there.
        None if Path::new(&cmd.program).is_absolute() => {
            Path::new(&cmd.program).file_name().and_then(|n| n.to_str()).unwrap_or(&cmd.program).to_string()
        }
        None => cmd.program.clone(),
    };
    let args: Vec<String> =
        cmd.args.iter().map(|a| map_local(a, &root, &cwd_rel).unwrap_or_else(|| a.clone())).collect();
    let script = quote::run_script(&remote_cwd, &cmd.env, &program, &args).map_err(failed)?;
    let opts = s.remote.ssh_options(name);
    s.remote.ensure_master(&server);
    let run = rcmd::ssh_exec(&server, &opts, &quote::sh_c(&script)).map_err(failed)?;
    if !t.sync {
        return Ok(ProcessCommand {
            program: opts.program.clone(),
            args: run,
            cwd: root,
            env: Vec::new(),
            build: cmd.build,
        });
    }
    let excludes: Vec<String> = match &dep {
        Some(d) => d.rsync_excludes(""),
        None => DEFAULT_EXCLUDES.iter().map(|x| (*x).to_string()).collect(),
    };
    let mkdir = rcmd::ssh_exec(&server, &opts, &rcmd::mkdir_command(&remote_root)).map_err(failed)?;
    let upload = if s.remote.has_rsync() {
        let spec = RsyncSpec {
            direction: Direction::Upload,
            local: &root,
            remote: &remote_root,
            dir: true,
            excludes: &excludes,
            dry_run: false,
            delete: false,
            update: false,
            files_from: None,
        };
        let mut w = vec![s.remote.programs.rsync.clone()];
        w.extend(rcmd::rsync_args(&server, &opts, &spec).map_err(failed)?);
        w
    } else {
        let mut w = vec![s.remote.programs.scp.clone()];
        w.extend(rcmd::scp_args(&server, &opts, Direction::Upload, &root, &remote_root, true).map_err(failed)?);
        w
    };
    let with_program =
        |args: Vec<String>| -> Vec<String> { std::iter::once(opts.program.clone()).chain(args).collect() };
    let line = format!(
        "{} && {} && exec {}",
        quote::join(&with_program(mkdir)),
        quote::join(&upload),
        quote::join(&with_program(run))
    );
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
