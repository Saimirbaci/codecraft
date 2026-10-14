//! Data source configuration (saved per project without secrets).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{DbError, DbResult};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    #[default]
    Sqlite,
    Postgres,
    Mysql,
    Mariadb,
}

impl DbKind {
    pub const ALL: [DbKind; 4] = [DbKind::Sqlite, DbKind::Postgres, DbKind::Mysql, DbKind::Mariadb];

    pub fn label(self) -> &'static str {
        match self {
            DbKind::Sqlite => "SQLite",
            DbKind::Postgres => "PostgreSQL",
            DbKind::Mysql => "MySQL",
            DbKind::Mariadb => "MariaDB",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Sqlite => None,
            DbKind::Postgres => Some(5432),
            DbKind::Mysql | DbKind::Mariadb => Some(3306),
        }
    }

    pub fn is_file_based(self) -> bool {
        self == DbKind::Sqlite
    }

    pub fn parse(s: &str) -> Option<DbKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sqlite" | "sqlite3" => Some(DbKind::Sqlite),
            "postgres" | "postgresql" | "pg" => Some(DbKind::Postgres),
            "mysql" => Some(DbKind::Mysql),
            "mariadb" => Some(DbKind::Mariadb),
            _ => None,
        }
    }

    pub fn dialect(self) -> crate::Dialect {
        match self {
            DbKind::Sqlite => crate::Dialect::Sqlite,
            DbKind::Postgres => crate::Dialect::Postgres,
            DbKind::Mysql | DbKind::Mariadb => crate::Dialect::Mysql,
        }
    }
}

/// PostgreSQL `sslmode`, with libpq's meaning: `prefer` and `require` encrypt without verifying
/// the certificate, `verify-ca` verifies the certificate chain and `verify-full` (`Verify`) also
/// the host name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    /// `verify-full`.
    Verify,
    /// `verify-ca`: a trusted chain, any host name.
    VerifyCa,
}

/// TLS files from the data source options (libpq names), as paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsFiles {
    /// `sslrootcert`: PEM certificates trusted in addition to the system roots.
    pub root_cert: Option<PathBuf>,
    /// `sslcert` + `sslkey`: client certificate and PKCS#8 key (PEM).
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

/// A data source as stored in `.codecraft/datasources.json`. Passwords are not part of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSourceConfig {
    /// Stable id (also the secret store account and the console directory name).
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// SQLite database file (absolute, or relative to the project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Driver options: `sslmode` (disable|prefer|require|verify-ca|verify-full), `sslrootcert`,
    /// `sslcert`, `sslkey` (PEM files), `connect_timeout` (seconds).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// A password for this data source is kept in the secret store.
    #[serde(default)]
    pub password_saved: bool,
    #[serde(default)]
    pub read_only: bool,
    /// Connect through an SSH tunnel to this SSH server (`ssh -N -L`, M11 track S): a server
    /// name from `~/.ssh/config` / Tools › Deployment, or `[user@]host[:port]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_tunnel: Option<String>,
    /// An SSH tunnel defined on the data source itself (DataGrip's SSH/SSL tab); wins over
    /// `ssh_tunnel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTunnelConfig>,
    /// A colour name from [`DATA_SOURCE_COLORS`], shown on the tree node, consoles and results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Production guard: confirm before running statements that modify data or schema.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confirm_writes: bool,
    /// Consoles use manual commit: the first statement opens a transaction that Commit or
    /// Rollback ends.
    #[serde(default, skip_serializing_if = "is_false")]
    pub manual_commit: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// How the SSH tunnel authenticates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SshAuth {
    /// `~/.ssh/config`, the default keys and the SSH agent.
    #[default]
    Agent,
    /// A private key file (its passphrase, if any, in the secret store).
    Key,
    /// A password (in the secret store).
    Password,
}

impl SshAuth {
    pub const ALL: [SshAuth; 3] = [SshAuth::Agent, SshAuth::Key, SshAuth::Password];

    pub fn id(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Key => "key",
            Self::Password => "password",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "OpenSSH config and agent",
            Self::Key => "Key pair",
            Self::Password => "Password",
        }
    }

    pub fn parse(s: &str) -> Option<SshAuth> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agent" | "openssh" | "config" => Some(Self::Agent),
            "key" | "keypair" | "key pair" => Some(Self::Key),
            "password" => Some(Self::Password),
            _ => None,
        }
    }
}

/// An SSH tunnel of one data source: `ssh -N -L 127.0.0.1:<port>:<db host>:<db port> user@host`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshTunnelConfig {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    pub auth: SshAuth,
    /// Private key file for [`SshAuth::Key`] (`~/` allowed).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identity_file: String,
    /// The password or key passphrase is in the secret store (account [`DataSourceConfig::ssh_secret_account`]).
    #[serde(skip_serializing_if = "is_false")]
    pub secret_saved: bool,
}

impl SshTunnelConfig {
    /// `[user@]host[:port]` (IPv6 hosts in brackets: `[::1]:22`).
    pub fn parse_destination(s: &str) -> Option<SshTunnelConfig> {
        let s = s.trim();
        if s.is_empty() || s.starts_with('-') || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        let (user, rest) = match s.rsplit_once('@') {
            Some((u, r)) if !u.is_empty() => (u.to_string(), r),
            Some(_) => return None,
            None => (String::new(), s),
        };
        let (host, port) = if let Some(r) = rest.strip_prefix('[') {
            let (h, after) = r.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p > 0)?),
                None if after.is_empty() => None,
                None => return None,
            };
            (h.to_string(), port)
        } else if rest.matches(':').count() == 1 {
            let (h, p) = rest.split_once(':')?;
            (h.to_string(), Some(p.parse::<u16>().ok().filter(|p| *p > 0)?))
        } else {
            (rest.to_string(), None)
        };
        if host.is_empty() || host.starts_with('-') {
            return None;
        }
        Some(SshTunnelConfig { host, port, user, ..Default::default() })
    }

    /// `user@host:port` as shown to the user.
    pub fn display(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let mut out = if self.user.is_empty() { host } else { format!("{}@{host}", self.user) };
        if let Some(p) = self.port {
            out.push_str(&format!(":{p}"));
        }
        out
    }
}

/// Data source colours (names; the UI maps them to theme colours).
pub const DATA_SOURCE_COLORS: [&str; 7] = ["red", "orange", "yellow", "green", "blue", "violet", "gray"];

impl DataSourceConfig {
    /// The secret store account of the SSH tunnel's password or key passphrase.
    pub fn ssh_secret_account(&self) -> String {
        format!("{}:ssh", self.id)
    }

    pub fn validate(&self) -> DbResult<()> {
        if self.kind.is_file_based() {
            match &self.path {
                Some(p) if !p.as_os_str().is_empty() => Ok(()),
                _ => Err(DbError::Config("an SQLite data source needs a database file".into())),
            }
        } else if self.host.trim().is_empty() {
            Err(DbError::Config(format!("a {} data source needs a host", self.kind.label())))
        } else {
            Ok(())
        }
    }

    /// The colour, if it is one of [`DATA_SOURCE_COLORS`].
    pub fn color_name(&self) -> Option<&'static str> {
        let c = self.color.as_deref()?.trim().to_ascii_lowercase();
        DATA_SOURCE_COLORS
            .iter()
            .copied()
            .find(|k| *k == c || c == "grey" && *k == "gray" || c == "purple" && *k == "violet")
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.or(self.kind.default_port()).unwrap_or(0)
    }

    pub fn ssl_mode(&self) -> DbResult<SslMode> {
        match self.options.get("sslmode").map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("prefer") | Some("allow") => Ok(SslMode::Prefer),
            Some("disable") => Ok(SslMode::Disable),
            Some("require") => Ok(SslMode::Require),
            Some("verify-ca") => Ok(SslMode::VerifyCa),
            Some("verify-full") => Ok(SslMode::Verify),
            Some(other) => Err(DbError::Config(format!("unknown sslmode `{other}`"))),
        }
    }

    /// The TLS files named in the options; relative paths resolve against `base` (the project).
    pub fn tls_files(&self, base: Option<&std::path::Path>) -> DbResult<TlsFiles> {
        let get = |k: &str| -> Option<PathBuf> {
            let v = self.options.get(k)?.trim();
            if v.is_empty() {
                return None;
            }
            let p = PathBuf::from(v);
            Some(match base {
                Some(b) if p.is_relative() => b.join(p),
                _ => p,
            })
        };
        let files = TlsFiles { root_cert: get("sslrootcert"), client_cert: get("sslcert"), client_key: get("sslkey") };
        if files.client_cert.is_some() != files.client_key.is_some() {
            return Err(DbError::Config("a client certificate needs both `sslcert` and `sslkey`".into()));
        }
        Ok(files)
    }

    pub fn connect_timeout(&self) -> std::time::Duration {
        self.options
            .get("connect_timeout")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(|s| std::time::Duration::from_secs(s.min(600)))
            .unwrap_or(crate::CONNECT_TIMEOUT)
    }

    /// `user@host:port/database` or the file path, for the tree and tooltips.
    pub fn summary(&self) -> String {
        if self.kind.is_file_based() {
            return self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        }
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
        let db = if self.database.is_empty() { String::new() } else { format!("/{}", self.database) };
        format!("{user}{}:{}{db}", self.host, self.port_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_without_secrets_or_empty_fields() {
        let cfg = DataSourceConfig {
            id: "ds1".into(),
            name: "local".into(),
            kind: DbKind::Postgres,
            host: "localhost".into(),
            user: "me".into(),
            password_saved: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("password\""));
        assert!(!json.contains("path"));
        let back: DataSourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(cfg.summary(), "me@localhost:5432");
    }

    #[test]
    fn validation_and_options() {
        let mut cfg = DataSourceConfig { kind: DbKind::Sqlite, ..Default::default() };
        assert!(cfg.validate().is_err());
        cfg.path = Some("a.db".into());
        assert!(cfg.validate().is_ok());
        cfg.kind = DbKind::Mysql;
        assert!(cfg.validate().is_err());
        cfg.options.insert("sslmode".into(), "require".into());
        assert_eq!(cfg.ssl_mode().unwrap(), SslMode::Require);
        cfg.options.insert("sslmode".into(), "verify-ca".into());
        assert_eq!(cfg.ssl_mode().unwrap(), SslMode::VerifyCa);
        cfg.options.insert("sslmode".into(), "Verify-Full".into());
        assert_eq!(cfg.ssl_mode().unwrap(), SslMode::Verify);
        cfg.options.insert("sslrootcert".into(), "certs/root.pem".into());
        let files = cfg.tls_files(Some(std::path::Path::new("/p"))).unwrap();
        assert_eq!(files.root_cert, Some(PathBuf::from("/p/certs/root.pem")));
        cfg.options.insert("sslcert".into(), "/abs/client.pem".into());
        assert!(cfg.tls_files(None).is_err());
        cfg.options.insert("sslkey".into(), "key.pem".into());
        let files = cfg.tls_files(None).unwrap();
        assert_eq!(files.client_cert, Some(PathBuf::from("/abs/client.pem")));
        assert_eq!(files.client_key, Some(PathBuf::from("key.pem")));
        cfg.color = Some("Purple".into());
        assert_eq!(cfg.color_name(), Some("violet"));
        cfg.color = Some("chartreuse".into());
        assert_eq!(cfg.color_name(), None);
        let json = serde_json::to_string(&DataSourceConfig { confirm_writes: true, ..Default::default() }).unwrap();
        assert!(json.contains("\"confirm_writes\":true") && !json.contains("manual_commit"), "{json}");
        cfg.options.insert("sslmode".into(), "bogus".into());
        assert!(cfg.ssl_mode().is_err());
        assert_eq!(DbKind::parse("PostgreSQL"), Some(DbKind::Postgres));
        assert_eq!(DbKind::parse("oracle"), None);
    }
}

#[cfg(test)]
mod ssh_tests {
    use super::*;

    #[test]
    fn ssh_destinations_parse() {
        let d = SshTunnelConfig::parse_destination("10.1.2.3").unwrap();
        assert_eq!((d.host.as_str(), d.port, d.user.as_str()), ("10.1.2.3", None, ""));
        let d = SshTunnelConfig::parse_destination("deploy@10.1.2.3:2222").unwrap();
        assert_eq!((d.host.as_str(), d.port, d.user.as_str()), ("10.1.2.3", Some(2222), "deploy"));
        assert_eq!(d.display(), "deploy@10.1.2.3:2222");
        let d = SshTunnelConfig::parse_destination("me@[::1]:22").unwrap();
        assert_eq!((d.host.as_str(), d.port), ("::1", Some(22)));
        assert_eq!(d.display(), "me@[::1]:22");
        let d = SshTunnelConfig::parse_destination("fe80::1").unwrap();
        assert_eq!((d.host.as_str(), d.port), ("fe80::1", None));
        for bad in ["", "-oProxyCommand=x", "a b", "@host", "host:0", "host:x", "[::1]x", "u@:22"] {
            assert!(SshTunnelConfig::parse_destination(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn ssh_config_round_trips_without_secrets() {
        let cfg = DataSourceConfig {
            id: "prod".into(),
            ssh: Some(SshTunnelConfig {
                host: "1.2.3.4".into(),
                user: "ops".into(),
                auth: SshAuth::Password,
                secret_saved: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"auth\":\"password\"") && !json.contains("identity_file"), "{json}");
        let back: DataSourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(cfg.ssh_secret_account(), "prod:ssh");
        assert_eq!(SshAuth::parse("Key pair"), Some(SshAuth::Key));
    }
}
