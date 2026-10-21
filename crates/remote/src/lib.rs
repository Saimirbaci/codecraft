//! CodeCraft remote: SSH config parsing, ssh/rsync/scp command builders and remote listing parsers.
//!
//! Pure logic for the SSH and remote features (M11 track S): nothing here starts a process or
//! opens a connection. The engine runs the argument vectors built here on worker threads.
//!
//! - [`ssh_config`]: `~/.ssh/config` with `Host` patterns and `Include`.
//! - [`server`]: SSH servers (config hosts plus CodeCraft-defined ones from `ssh.json`).
//! - [`command`]: argument vectors for `ssh` (BatchMode, ControlMaster), `rsync` and `scp`.
//! - [`quote`]: POSIX shell quoting for remote commands.
//! - [`rsync`]: `--itemize-changes` output.
//! - [`listing`]: `ls -lAn` output.
//! - [`deploy`]: per-project deployment mappings (`.codecraft/deployment.json`).
//! - [`target_script`]: scripts run on a run target to debug, test and profile there (M16 RT).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod command;
pub mod deploy;
pub mod listing;
pub mod mirror;
pub mod quote;
pub mod rsync;
pub mod server;
pub mod ssh_config;
pub mod target_script;

pub use command::{Direction, RsyncSpec, SshOptions, TunnelAuth};
pub use deploy::{DeploymentConfig, PathMapping, ServerDeployment};
pub use listing::{EntryKind, RemoteEntry};
pub use quote::sh_quote;
pub use rsync::{Change, ItemKind, ItemizedChange};
pub use server::{ServerConfig, ServerSource, ServersFile, SshServer};
pub use ssh_config::{HostInfo, SshConfig};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Parse(String),
}

pub type RemoteResult<T> = Result<T, RemoteError>;
