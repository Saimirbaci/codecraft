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
}

impl DataSourceConfig {
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
        cfg.options.insert("sslmode".into(), "bogus".into());
        assert!(cfg.ssl_mode().is_err());
        assert_eq!(DbKind::parse("PostgreSQL"), Some(DbKind::Postgres));
        assert_eq!(DbKind::parse("oracle"), None);
    }
}
