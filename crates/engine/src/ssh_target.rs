//! An SSH host as a run target: sync the project, then run the configuration in the mapped
//! remote directory (M11 track S).

use serde::{Deserialize, Serialize};

use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshTarget {
    /// SSH server name (a `~/.ssh/config` host or a CodeCraft-defined server).
    pub server: String,
}

impl SshTarget {
    pub fn label(&self) -> String {
        self.server.clone()
    }
}

pub(crate) fn wrap(_s: &mut Session, _t: &SshTarget, _cmd: ProcessCommand) -> Result<ProcessCommand, CommandError> {
    Err(CommandError::Failed("SSH run targets are not implemented yet".into()))
}
