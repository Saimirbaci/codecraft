//! Docker as a run target: run a configuration inside an image or a running container with the
//! project mounted (M11 track D).

use serde::{Deserialize, Serialize};

use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerTarget {
    /// Image to run in (`docker run --rm`), e.g. `rust:1`.
    pub image: String,
}

impl DockerTarget {
    pub fn label(&self) -> String {
        self.image.clone()
    }
}

pub(crate) fn wrap(_s: &mut Session, _t: &DockerTarget, _cmd: ProcessCommand) -> Result<ProcessCommand, CommandError> {
    Err(CommandError::Failed("Docker run targets are not implemented yet".into()))
}
