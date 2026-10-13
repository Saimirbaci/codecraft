//! Deployment: per-project servers with path mappings (local folder ↔ remote folder) and
//! excluded paths, stored in `<project>/.codecraft/deployment.json`.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::command;
use crate::{RemoteError, RemoteResult};

/// Excluded by default from new mappings: VCS data, build output and CodeCraft's own folder.
pub const DEFAULT_EXCLUDES: &[&str] = &[".git", "target", ".codecraft", ".idea", "node_modules", "__pycache__"];

/// One mapping: a project folder and the remote folder it deploys to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PathMapping {
    /// Relative to the project root, `/`-separated; empty = the project root.
    pub local: String,
    /// Remote folder: absolute, `~/…`, or relative to the remote home directory.
    pub remote: String,
}

/// A deployment server of a project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerDeployment {
    /// SSH server name.
    pub server: String,
    pub mappings: Vec<PathMapping>,
    /// Excluded local paths: a bare name or wildcard (`target`, `*.log`) matches that name
    /// anywhere; a path with `/` (`docs/private`) is relative to the project root.
    pub excluded: Vec<String>,
}

/// `.codecraft/deployment.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeploymentConfig {
    pub version: u32,
    pub servers: Vec<ServerDeployment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_server: Option<String>,
    /// Upload saved files to the default server (Tools › Deployment › Automatic Upload).
    pub auto_upload: bool,
    /// Sync deletes target items whose source is gone (Options… "Delete target items when
    /// source ones do not exist").
    pub delete_target_items: bool,
    /// Remote Development mirror: the project is a local cache of this server's folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_development: Option<String>,
}

impl DeploymentConfig {
    pub fn parse(text: &str) -> RemoteResult<DeploymentConfig> {
        serde_json::from_str(text).map_err(|e| RemoteError::Parse(format!("deployment.json: {e}")))
    }

    pub fn to_json(&self) -> RemoteResult<String> {
        let mut c = self.clone();
        c.version = 1;
        serde_json::to_string_pretty(&c).map_err(|e| RemoteError::Parse(e.to_string()))
    }

    pub fn server(&self, name: &str) -> Option<&ServerDeployment> {
        self.servers.iter().find(|s| s.server == name)
    }

    pub fn server_mut(&mut self, name: &str) -> Option<&mut ServerDeployment> {
        self.servers.iter_mut().find(|s| s.server == name)
    }

    /// The deployment of `name`, added (with the default exclusions) when missing.
    pub fn ensure_server(&mut self, name: &str) -> Option<&mut ServerDeployment> {
        if self.server(name).is_none() {
            self.servers.push(ServerDeployment {
                server: name.to_string(),
                mappings: Vec::new(),
                excluded: DEFAULT_EXCLUDES.iter().map(|s| (*s).to_string()).collect(),
            });
        }
        self.server_mut(name)
    }

    /// The default server's deployment (the only one if there is no default).
    pub fn default_deployment(&self) -> Option<&ServerDeployment> {
        match &self.default_server {
            Some(d) => self.server(d),
            None if self.servers.len() == 1 => self.servers.first(),
            None => None,
        }
    }
}

/// `path` relative to `root` with `/` separators (`""` for the root itself); `None` outside it.
pub fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Whether `rel` is `prefix` or inside it (component-wise).
fn under(rel: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(rel.to_string());
    }
    if rel == prefix {
        return Some(String::new());
    }
    rel.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')).map(str::to_string)
}

fn norm_local(s: &str) -> String {
    s.trim().trim_matches('/').trim_start_matches("./").to_string()
}

impl ServerDeployment {
    pub fn validate(&self) -> RemoteResult<()> {
        if self.server.trim().is_empty() {
            return Err(RemoteError::Invalid("the deployment has no server".into()));
        }
        for m in &self.mappings {
            if m.remote.trim().is_empty() {
                return Err(RemoteError::Invalid("a mapping needs a remote path".into()));
            }
            if m.local.split('/').any(|c| c == "..") {
                return Err(RemoteError::Invalid("a local mapping path must stay inside the project".into()));
            }
            if m.remote.chars().any(char::is_control) {
                return Err(RemoteError::Invalid("the remote path has control characters".into()));
            }
        }
        Ok(())
    }

    /// Whether a project-relative path is excluded.
    pub fn is_excluded(&self, rel: &str) -> bool {
        let rel = norm_local(rel);
        self.excluded.iter().map(|x| norm_local(x)).filter(|x| !x.is_empty()).any(|x| {
            if x.contains('/') {
                under(&rel, &x).is_some()
            } else {
                rel.split('/').any(|c| crate::ssh_config::wildcard(&x, c))
            }
        })
    }

    /// The mapping that covers a project-relative path (the longest local prefix) and the path
    /// inside it.
    pub fn mapping_for(&self, rel: &str) -> Option<(&PathMapping, String)> {
        let rel = norm_local(rel);
        self.mappings
            .iter()
            .filter_map(|m| under(&rel, &norm_local(&m.local)).map(|inner| (m, inner)))
            .max_by_key(|(m, _)| norm_local(&m.local).len())
    }

    /// The remote path of a project-relative path (`None` if unmapped or excluded).
    pub fn remote_for(&self, rel: &str) -> Option<String> {
        if self.is_excluded(rel) {
            return None;
        }
        let (m, inner) = self.mapping_for(rel)?;
        Some(command::join(m.remote.trim(), &inner))
    }

    /// The project-relative path of a remote path (`None` if no mapping covers it).
    pub fn local_for(&self, remote: &str) -> Option<String> {
        let remote = remote.trim_end_matches('/');
        self.mappings
            .iter()
            .filter_map(|m| {
                let r = m.remote.trim().trim_end_matches('/');
                under(remote, r).map(|inner| (m, inner, r.len()))
            })
            .max_by_key(|(_, _, len)| *len)
            .map(|(m, inner, _)| {
                let l = norm_local(&m.local);
                if l.is_empty() {
                    inner
                } else if inner.is_empty() {
                    l
                } else {
                    format!("{l}/{inner}")
                }
            })
    }

    /// rsync `--exclude` patterns for a transfer rooted at the project-relative folder `root`:
    /// bare names match anywhere; root-relative paths are anchored (`/x`) when inside `root`.
    pub fn rsync_excludes(&self, root: &str) -> Vec<String> {
        let root = norm_local(root);
        let mut out = Vec::new();
        for x in self.excluded.iter().map(|x| norm_local(x)).filter(|x| !x.is_empty()) {
            if !x.contains('/') {
                out.push(x);
            } else if let Some(inner) = under(&x, &root)
                && !inner.is_empty()
            {
                out.push(format!("/{inner}"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep() -> ServerDeployment {
        ServerDeployment {
            server: "web".into(),
            mappings: vec![
                PathMapping { local: "".into(), remote: "/srv/app".into() },
                PathMapping { local: "static/".into(), remote: "/var/www/static".into() },
            ],
            excluded: vec!["target".into(), "*.log".into(), "docs/private".into()],
        }
    }

    #[test]
    fn longest_mapping_wins_both_ways() {
        let d = dep();
        assert_eq!(d.remote_for("src/main.rs").as_deref(), Some("/srv/app/src/main.rs"));
        assert_eq!(d.remote_for("").as_deref(), Some("/srv/app"));
        assert_eq!(d.remote_for("static/a.css").as_deref(), Some("/var/www/static/a.css"));
        assert_eq!(d.remote_for("staticfile").as_deref(), Some("/srv/app/staticfile"));
        assert_eq!(d.local_for("/var/www/static/a.css").as_deref(), Some("static/a.css"));
        assert_eq!(d.local_for("/srv/app/src").as_deref(), Some("src"));
        assert_eq!(d.local_for("/srv/app").as_deref(), Some(""));
        assert_eq!(d.local_for("/srv/application"), None);
        assert_eq!(d.local_for("/elsewhere"), None);
    }

    #[test]
    fn exclusions() {
        let d = dep();
        assert!(d.is_excluded("target/debug/x"));
        assert!(d.is_excluded("crates/a/target"));
        assert!(d.is_excluded("run.log"));
        assert!(d.is_excluded("docs/private/x.md"));
        assert!(!d.is_excluded("docs/privateer.md"));
        assert!(!d.is_excluded("src/main.rs"));
        assert_eq!(d.remote_for("target/x"), None);
        assert_eq!(d.rsync_excludes(""), vec!["target", "*.log", "/docs/private"]);
        assert_eq!(d.rsync_excludes("docs"), vec!["target", "*.log", "/private"]);
        assert_eq!(d.rsync_excludes("src"), vec!["target", "*.log"]);
    }

    #[test]
    fn config_round_trip_default_and_validation() {
        let mut c = DeploymentConfig::default();
        assert!(c.default_deployment().is_none());
        c.ensure_server("web").unwrap().mappings.push(PathMapping { local: "".into(), remote: "~/app".into() });
        assert_eq!(c.server("web").map(|s| s.excluded.len()), Some(DEFAULT_EXCLUDES.len()));
        assert_eq!(c.default_deployment().map(|s| s.server.as_str()), Some("web"));
        c.ensure_server("db");
        assert!(c.default_deployment().is_none());
        c.default_server = Some("db".into());
        assert_eq!(c.default_deployment().map(|s| s.server.as_str()), Some("db"));
        let back = DeploymentConfig::parse(&c.to_json().unwrap()).unwrap();
        assert_eq!(back.servers, c.servers);
        assert_eq!(back.version, 1);
        assert!(DeploymentConfig::parse("[").is_err());
        let mut bad = dep();
        bad.mappings.push(PathMapping { local: "../x".into(), remote: "/x".into() });
        assert!(bad.validate().is_err());
        assert!(dep().validate().is_ok());
    }

    #[test]
    fn rel_paths() {
        assert_eq!(rel_path(Path::new("/p"), Path::new("/p/a/b.rs")).as_deref(), Some("a/b.rs"));
        assert_eq!(rel_path(Path::new("/p"), Path::new("/p")).as_deref(), Some(""));
        assert_eq!(rel_path(Path::new("/p"), Path::new("/q/a")), None);
    }
}
