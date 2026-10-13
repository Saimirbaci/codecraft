//! Where a run configuration runs: locally, inside a Docker image/container (track D) or on an
//! SSH host (track S). A non-local configuration becomes a plain process (`docker …`, `ssh …`)
//! through [`wrap`].

use serde::{Deserialize, Serialize};

use crate::docker_target::DockerTarget;
use crate::runs::ProcessCommand;
use crate::ssh_target::SshTarget;
use crate::{CommandError, Session};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RunTarget {
    #[default]
    Local,
    Docker(DockerTarget),
    Ssh(SshTarget),
}

impl RunTarget {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// Short label for the run configuration list ("Local", "Docker: rust:1", "SSH: host").
    pub fn label(&self) -> String {
        match self {
            Self::Local => "Local".into(),
            Self::Docker(t) => format!("Docker: {}", t.label()),
            Self::Ssh(t) => format!("SSH: {}", t.label()),
        }
    }
}

/// The command that runs `cmd` on `target` (unchanged for [`RunTarget::Local`]).
pub(crate) fn wrap(s: &mut Session, target: &RunTarget, cmd: ProcessCommand) -> Result<ProcessCommand, CommandError> {
    match target {
        RunTarget::Local => Ok(cmd),
        RunTarget::Docker(t) => crate::docker_target::wrap(s, t, cmd),
        RunTarget::Ssh(t) => crate::ssh_target::wrap(s, t, cmd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_is_the_default_and_round_trips() {
        assert!(RunTarget::default().is_local());
        let json = serde_json::to_string(&RunTarget::Local).unwrap();
        assert_eq!(serde_json::from_str::<RunTarget>(&json).unwrap(), RunTarget::Local);
        let mut s = Session::new();
        let cmd = ProcessCommand::new("echo", &["hi"], "/");
        assert_eq!(wrap(&mut s, &RunTarget::Local, cmd.clone()).unwrap(), cmd);
    }
}
