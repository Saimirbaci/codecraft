//! Languages & Frameworks › SQL for files (track DX): the SQL dialect of a file or folder and
//! the data source files run against when nothing else says.
//!
//! Saved per project in `.codecraft/sql-files.json` as paths relative to the project (`/`
//! separated; the empty path is the whole project). The most specific path wins: a file's own
//! entry, then its closest folder, then the project. A file's data source still comes first
//! from its console folder or its own binding (`db.setDataSource`, `db.attachFile`); these
//! defaults fill in for every other `.sql` file. The dialect decides how statements are split
//! and parameters found when a file runs; without one, the data source's dialect applies.
//! Redis consoles always keep the Redis command syntax.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use codecraft_db::{DbKind, Dialect};

use super::DbState;

/// The settings file, relative to the project root.
pub const FILE: &str = ".codecraft/sql-files.json";
/// Entries kept per map.
const MAX_ENTRIES: usize = 10_000;

/// The dialect names a file can be given (`generic` plus the database kinds that speak SQL).
pub const DIALECTS: &[(&str, &str)] = &[
    ("generic", "Generic SQL"),
    ("sqlite", "SQLite"),
    ("postgres", "PostgreSQL"),
    ("mysql", "MySQL"),
    ("mariadb", "MariaDB"),
    ("sqlserver", "Microsoft SQL Server"),
    ("clickhouse", "ClickHouse"),
    ("duckdb", "DuckDB"),
];

/// A dialect name (`postgres`, `PostgreSQL`, `tsql`, `generic`, …) as its canonical id.
pub fn parse_dialect(s: &str) -> Option<&'static str> {
    let k = s.trim().to_ascii_lowercase();
    if matches!(k.as_str(), "generic" | "sql" | "ansi" | "sql92") {
        return Some("generic");
    }
    let by_label = || DIALECTS.iter().find(|(_, l)| l.eq_ignore_ascii_case(&k)).and_then(|(id, _)| DbKind::parse(id));
    let kind = DbKind::parse(&k).or_else(by_label)?;
    if !kind.is_sql() {
        return None;
    }
    DIALECTS.iter().map(|(id, _)| *id).find(|id| DbKind::parse(id) == Some(kind))
}

/// How statements of a dialect are split.
pub fn splitter(id: &str) -> Dialect {
    match DbKind::parse(id) {
        Some(k) => k.dialect(),
        None => Dialect::Generic,
    }
}

pub fn label(id: &str) -> &'static str {
    DIALECTS.iter().find(|(d, _)| *d == id).map(|(_, l)| *l).unwrap_or("Generic SQL")
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SqlFileSettings {
    /// Path → dialect id ([`DIALECTS`]).
    pub dialects: BTreeMap<String, String>,
    /// Path → data source id.
    pub data_sources: BTreeMap<String, String>,
}

/// `path` relative to `root` with `/` separators; `None` outside the project.
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = if path.is_absolute() { path.strip_prefix(root).ok()? } else { path };
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            // `..` or a root inside a relative path would leave the project.
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// The entry of `map` that applies to `rel`: the path itself, else its closest folder, else
/// the project (`""`).
fn lookup<'a>(map: &'a BTreeMap<String, String>, rel: &str) -> Option<(&'a str, &'a str)> {
    let mut key = rel.trim_matches('/');
    loop {
        if let Some((k, v)) = map.get_key_value(key) {
            return Some((k.as_str(), v.as_str()));
        }
        if key.is_empty() {
            return None;
        }
        key = key.rsplit_once('/').map(|(a, _)| a).unwrap_or("");
    }
}

impl SqlFileSettings {
    /// Read the project's settings (empty when there are none or they cannot be read).
    pub fn load(root: Option<&Path>) -> Self {
        let Some(path) = root.map(|r| r.join(FILE)).filter(|p| p.is_file()) else { return Self::default() };
        match codecraft_fs::read_text_file(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Self>(&t).map_err(|e| e.to_string()))
        {
            Ok(mut s) => {
                s.dialects.retain(|_, v| parse_dialect(v).is_some());
                s
            }
            Err(e) => {
                log::warn!("ignoring {}: {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), String> {
        let path = root.join(FILE);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        codecraft_fs::write_text_file_atomic(&path, &format!("{json}\n")).map_err(|e| e.to_string())
    }

    /// The dialect of a file and the path it was set on.
    pub fn dialect_entry(&self, root: Option<&Path>, path: &Path) -> Option<(String, String)> {
        let rel = relative(root?, path)?;
        lookup(&self.dialects, &rel).map(|(k, v)| (k.to_string(), v.to_string()))
    }

    /// The default data source of a file (its own entry, its folder's or the project's).
    pub fn default_source(&self, root: Option<&Path>, path: &Path) -> Option<String> {
        let rel = relative(root?, path)?;
        lookup(&self.data_sources, &rel).map(|(_, v)| v.to_string())
    }

    /// Set (`Some`) or clear (`None`) a dialect for a path.
    pub fn set_dialect(&mut self, key: String, dialect: Option<&str>) -> Result<(), String> {
        match dialect {
            Some(d) => {
                let id = parse_dialect(d).ok_or_else(|| {
                    format!(
                        "unknown dialect `{d}` ({})",
                        DIALECTS.iter().map(|(i, _)| *i).collect::<Vec<_>>().join(", ")
                    )
                })?;
                if !self.dialects.contains_key(&key) && self.dialects.len() >= MAX_ENTRIES {
                    return Err("too many dialect entries".into());
                }
                self.dialects.insert(key, id.to_string());
            }
            None => {
                self.dialects.remove(&key);
            }
        }
        Ok(())
    }

    pub fn set_source(&mut self, key: String, source: Option<String>) -> Result<(), String> {
        match source {
            Some(id) => {
                if !self.data_sources.contains_key(&key) && self.data_sources.len() >= MAX_ENTRIES {
                    return Err("too many data source entries".into());
                }
                self.data_sources.insert(key, id);
            }
            None => {
                self.data_sources.remove(&key);
            }
        }
        Ok(())
    }

    /// A data source was removed: its defaults go too.
    pub fn forget_source(&mut self, id: &str) {
        self.data_sources.retain(|_, v| v != id);
    }
}

impl DbState {
    /// How a file's statements are split: Redis consoles keep Redis syntax, then the file's
    /// (or its folder's) dialect, else the data source's, else generic SQL.
    pub fn dialect_for(&self, path: Option<&Path>, source: Option<&str>) -> Dialect {
        let kind = source.and_then(|id| self.source(id)).map(|d| d.config.kind);
        if kind == Some(DbKind::Redis) {
            return Dialect::Redis;
        }
        if let Some((_, d)) = path.and_then(|p| self.sql_files.dialect_entry(self.project_root(), p)) {
            return splitter(&d);
        }
        kind.map(DbKind::dialect).unwrap_or_default()
    }

    /// The dialect id shown for a file (its own or inherited setting, else its data source's
    /// kind), and where it comes from (`file`, `folder`, `project`, `dataSource`, `default`).
    pub fn dialect_info(&self, path: Option<&Path>, source: Option<&str>) -> (String, &'static str) {
        if let Some(p) = path
            && let Some((key, d)) = self.sql_files.dialect_entry(self.project_root(), p)
        {
            let rel = self.project_root().and_then(|r| relative(r, p)).unwrap_or_default();
            let from = if key.is_empty() {
                "project"
            } else if key == rel {
                "file"
            } else {
                "folder"
            };
            return (d, from);
        }
        match source.and_then(|id| self.source(id)).map(|d| d.config.kind) {
            Some(k) => {
                let id = DIALECTS.iter().map(|(i, _)| *i).find(|i| DbKind::parse(i) == Some(k)).unwrap_or("generic");
                (id.to_string(), "dataSource")
            }
            None => ("generic".into(), "default"),
        }
    }

    /// Whether a document runs Redis commands (a Redis data source's console or file): no SQL
    /// completion, inspections or navigation there.
    pub fn is_redis_path(&self, path: Option<&Path>) -> bool {
        self.source_for_path(path).and_then(|id| self.source(&id)).is_some_and(|d| d.config.kind == DbKind::Redis)
    }

    /// The settings key for a path the user names (absolute in the project, or relative).
    pub(crate) fn sql_files_key(&self, path: &Path) -> Result<String, String> {
        let root = self.project_root().ok_or("open a project first")?;
        relative(root, path).ok_or_else(|| format!("{} is outside the project", path.display()))
    }

    /// Save the file settings.
    pub(crate) fn save_sql_files(&self) -> Result<(), String> {
        let root: PathBuf = self.project_root().ok_or("open a project first")?.to_path_buf();
        self.sql_files.save(&root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_names() {
        assert_eq!(parse_dialect("PostgreSQL"), Some("postgres"));
        assert_eq!(parse_dialect("tsql"), Some("sqlserver"));
        assert_eq!(parse_dialect("Microsoft SQL Server"), Some("sqlserver"));
        assert_eq!(parse_dialect("SQL"), Some("generic"));
        assert_eq!(parse_dialect("redis"), None);
        assert_eq!(parse_dialect("oracle"), None);
        assert_eq!(splitter("sqlserver"), Dialect::TSql);
        assert_eq!(splitter("generic"), Dialect::Generic);
        assert_eq!(label("duckdb"), "DuckDB");
    }

    #[test]
    fn most_specific_path_wins() {
        let root = Path::new("/p");
        let mut s = SqlFileSettings::default();
        s.set_dialect(String::new(), Some("postgres")).unwrap();
        s.set_dialect("db/mysql".into(), Some("mysql")).unwrap();
        s.set_dialect("db/mysql/special.sql".into(), Some("sqlite")).unwrap();
        assert!(s.set_dialect("x".into(), Some("nope")).is_err());
        let d = |p: &str| s.dialect_entry(Some(root), Path::new(p)).map(|(_, d)| d);
        assert_eq!(d("/p/a.sql").as_deref(), Some("postgres"));
        assert_eq!(d("/p/db/mysql/q/1.sql").as_deref(), Some("mysql"));
        assert_eq!(d("/p/db/mysql/special.sql").as_deref(), Some("sqlite"));
        assert_eq!(d("/elsewhere/a.sql"), None);
        assert_eq!(s.dialect_entry(None, Path::new("/p/a.sql")), None);
        s.set_source("reports".into(), Some("wh".into())).unwrap();
        assert_eq!(s.default_source(Some(root), Path::new("/p/reports/q.sql")).as_deref(), Some("wh"));
        assert_eq!(s.default_source(Some(root), Path::new("/p/other.sql")), None);
        s.forget_source("wh");
        assert!(s.data_sources.is_empty());
        assert_eq!(relative(root, Path::new("/p/./a/../b")), None);
        assert_eq!(relative(root, Path::new("a/b.sql")).as_deref(), Some("a/b.sql"));
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SqlFileSettings::default();
        s.set_dialect("q".into(), Some("mysql")).unwrap();
        s.save(dir.path()).unwrap();
        assert_eq!(SqlFileSettings::load(Some(dir.path())), s);
        std::fs::write(dir.path().join(FILE), "{oops").unwrap();
        assert_eq!(SqlFileSettings::load(Some(dir.path())), SqlFileSettings::default());
        assert_eq!(SqlFileSettings::load(None), SqlFileSettings::default());
    }
}
