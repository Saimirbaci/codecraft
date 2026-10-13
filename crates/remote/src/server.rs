//! SSH servers: the concrete hosts of `~/.ssh/config` plus CodeCraft-defined servers kept in
//! `$CODECRAFT_HOME/ssh.json` (host, port, user, identity file; keys or the agent only, never
//! passwords).

use serde::{Deserialize, Serialize};

use crate::ssh_config::SshConfig;
use crate::{RemoteError, RemoteResult};

/// A CodeCraft-defined server as stored in `ssh.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    pub name: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Private key file (`~/` allowed); empty = the agent and ssh's defaults.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identity_file: String,
}

/// `ssh.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServersFile {
    pub version: u32,
    pub servers: Vec<ServerConfig>,
}

impl ServersFile {
    pub fn parse(text: &str) -> RemoteResult<ServersFile> {
        serde_json::from_str(text).map_err(|e| RemoteError::Parse(format!("ssh.json: {e}")))
    }

    pub fn to_json(&self) -> RemoteResult<String> {
        serde_json::to_string_pretty(&ServersFile { version: 1, servers: self.servers.clone() })
            .map_err(|e| RemoteError::Parse(e.to_string()))
    }
}

/// Where a server is defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerSource {
    /// A `Host` of `~/.ssh/config` (connected to by alias, so ssh applies all of its options).
    SshConfig,
    /// `ssh.json`.
    CodeCraft,
}

/// A server CodeCraft can connect to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshServer {
    pub name: String,
    /// The host connected to (the alias's `HostName` for config hosts; display only).
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity_file: Option<String>,
    pub source: ServerSource,
}

fn bad(msg: impl Into<String>) -> RemoteError {
    RemoteError::Invalid(msg.into())
}

/// Words that become command-line arguments must not look like options or carry whitespace.
fn check_word(what: &str, v: &str) -> RemoteResult<()> {
    if v.starts_with('-') {
        return Err(bad(format!("{what} must not start with `-`")));
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad(format!("{what} must not contain spaces")));
    }
    Ok(())
}

impl ServerConfig {
    pub fn validate(&self) -> RemoteResult<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(bad("the server needs a name"));
        }
        if name.chars().any(char::is_control) || name.len() > 100 {
            return Err(bad("the server name is too long or has control characters"));
        }
        if self.host.trim().is_empty() {
            return Err(bad("the server needs a host"));
        }
        check_word("the host", &self.host)?;
        if self.host.contains('@') {
            return Err(bad("put the user in the User field, not in the host"));
        }
        if !self.user.is_empty() {
            check_word("the user", &self.user)?;
            if self.user.contains(['@', ':', '/']) {
                return Err(bad("the user must not contain `@`, `:` or `/`"));
            }
        }
        if self.port == Some(0) {
            return Err(bad("the port must be between 1 and 65535"));
        }
        if self.identity_file.starts_with('-') || self.identity_file.chars().any(char::is_control) {
            return Err(bad("invalid identity file"));
        }
        Ok(())
    }

    pub fn to_server(&self) -> SshServer {
        SshServer {
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            port: self.port,
            user: (!self.user.is_empty()).then(|| self.user.clone()),
            identity_file: (!self.identity_file.is_empty()).then(|| self.identity_file.clone()),
            source: ServerSource::CodeCraft,
        }
    }
}

impl SshServer {
    /// A server for a `~/.ssh/config` alias (details for display only).
    pub fn from_config(cfg: &SshConfig, alias: &str) -> SshServer {
        let info = cfg.resolve(alias);
        SshServer {
            name: alias.to_string(),
            host: info.hostname,
            port: info.port,
            user: info.user,
            identity_file: info.identity_files.first().cloned(),
            source: ServerSource::SshConfig,
        }
    }

    /// `user@host:port` for lists.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.host);
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        s
    }

    /// The ssh options that select this server's port, user and key (empty for config aliases,
    /// whose options ssh reads itself).
    pub fn connect_options(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.source == ServerSource::SshConfig {
            return out;
        }
        if let Some(p) = self.port {
            out.extend(["-p".to_string(), p.to_string()]);
        }
        if let Some(u) = &self.user {
            out.extend(["-l".to_string(), u.clone()]);
        }
        if let Some(i) = &self.identity_file {
            out.extend(["-i".to_string(), i.clone(), "-o".into(), "IdentitiesOnly=yes".into()]);
        }
        out
    }

    /// The ssh destination word (the alias, or the host).
    pub fn destination(&self) -> RemoteResult<String> {
        let d = match self.source {
            ServerSource::SshConfig => self.name.clone(),
            ServerSource::CodeCraft => self.host.clone(),
        };
        if d.is_empty() {
            return Err(bad("the server has no host"));
        }
        check_word("the host", &d)?;
        Ok(d)
    }

    /// `[user@]host` as rsync and scp write it before `:path` (IPv6 hosts in brackets).
    pub fn transfer_host(&self) -> RemoteResult<String> {
        let d = self.destination()?;
        let host = if d.contains(':') { format!("[{d}]") } else { d };
        Ok(match (&self.user, self.source) {
            (Some(u), ServerSource::CodeCraft) => format!("{u}@{host}"),
            _ => host,
        })
    }
}

/// All servers: CodeCraft-defined ones first (they win a name clash), then the config's hosts.
pub fn merge(config: &SshConfig, own: &[ServerConfig]) -> Vec<SshServer> {
    let mut out: Vec<SshServer> = own.iter().filter(|c| c.validate().is_ok()).map(ServerConfig::to_server).collect();
    for alias in config.hosts() {
        if out.iter().any(|s| s.name == alias) || check_word("host", &alias).is_err() {
            continue;
        }
        out.push(SshServer::from_config(config, &alias));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(name: &str, host: &str) -> ServerConfig {
        ServerConfig { name: name.into(), host: host.into(), ..ServerConfig::default() }
    }

    #[test]
    fn validation_rejects_option_injection_and_bad_fields() {
        assert!(own("a", "example.com").validate().is_ok());
        assert!(own("", "example.com").validate().is_err());
        assert!(own("a", "").validate().is_err());
        assert!(own("a", "-oProxyCommand=sh").validate().is_err());
        assert!(own("a", "two words").validate().is_err());
        assert!(own("a", "u@h").validate().is_err());
        let mut c = own("a", "h");
        c.user = "-x".into();
        assert!(c.validate().is_err());
        c.user = "ok".into();
        c.port = Some(0);
        assert!(c.validate().is_err());
        c.port = Some(22);
        c.identity_file = "-i".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn merge_prefers_own_servers_and_skips_invalid() {
        let cfg = SshConfig::parse("Host web\n HostName w.example\n User u\n Port 2200\nHost db\n", None, None);
        let servers = merge(&cfg, &[own("web", "override.example"), own("bad", "-x")]);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["web", "db"]);
        assert_eq!(servers[0].host, "override.example");
        assert_eq!(servers[0].source, ServerSource::CodeCraft);
        let cfg_only = merge(&cfg, &[]);
        assert_eq!(cfg_only[0].label(), "u@w.example:2200");
        assert!(cfg_only[0].connect_options().is_empty());
        assert_eq!(cfg_only[0].destination().unwrap(), "web");
    }

    #[test]
    fn connect_options_and_transfer_host() {
        let c = ServerConfig {
            name: "box".into(),
            host: "10.0.0.1".into(),
            port: Some(2222),
            user: "me".into(),
            identity_file: "~/.ssh/k".into(),
        };
        let s = c.to_server();
        assert_eq!(s.connect_options(), vec!["-p", "2222", "-l", "me", "-i", "~/.ssh/k", "-o", "IdentitiesOnly=yes"]);
        assert_eq!(s.destination().unwrap(), "10.0.0.1");
        assert_eq!(s.transfer_host().unwrap(), "me@10.0.0.1");
        let v6 = own("v6", "::1").to_server();
        assert_eq!(v6.transfer_host().unwrap(), "[::1]");
    }

    #[test]
    fn servers_file_round_trips_and_tolerates_garbage() {
        let f = ServersFile { version: 1, servers: vec![own("a", "h")] };
        let back = ServersFile::parse(&f.to_json().unwrap()).unwrap();
        assert_eq!(back, f);
        assert!(ServersFile::parse("{").is_err());
        assert_eq!(ServersFile::parse("{}").unwrap().servers.len(), 0);
    }
}
