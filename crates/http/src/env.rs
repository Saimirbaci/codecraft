//! Environments: `http-client.env.json` (shared) and `http-client.private.env.json` (secrets,
//! kept out of version control). Both map an environment name to its variables:
//!
//! ```json
//! { "dev": { "host": "localhost:8080" }, "prod": { "host": "example.org" }, "$shared": { "v": "1" } }
//! ```
//!
//! `$shared` applies to every environment. For one name, private values win over public ones,
//! and environment values win over `$shared` ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::HttpError;

pub const PUBLIC_FILE: &str = "http-client.env.json";
pub const PRIVATE_FILE: &str = "http-client.private.env.json";
pub const SHARED: &str = "$shared";
/// Largest environment file read.
const MAX_ENV_FILE: u64 = 4 << 20;

/// The environment files found for a request file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Environments {
    pub public: Map<String, Value>,
    pub private: Map<String, Value>,
    /// The files read.
    pub files: Vec<PathBuf>,
    /// Problems reading them (shown to the user; the rest still works).
    pub errors: Vec<String>,
}

fn read_env_file(path: &Path, errors: &mut Vec<String>) -> Option<Map<String, Value>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_ENV_FILE {
        errors.push(format!("{} is too large", path.display()));
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("cannot read {}: {e}", path.display()));
            return None;
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Some(m),
        Ok(_) => {
            errors.push(format!("{}: expected a JSON object", path.display()));
            None
        }
        Err(e) => {
            errors.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

/// The directory holding the nearest `name` file: `dir`, then its parents up to `root`
/// (inclusive; without a root only `dir` is searched).
pub fn find_file(dir: &Path, root: Option<&Path>, name: &str) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let p = d.join(name);
        if p.is_file() {
            return Some(p);
        }
        match root {
            Some(r) if d != r && d.starts_with(r) => cur = d.parent(),
            _ => return None,
        }
    }
    None
}

impl Environments {
    /// Load the nearest environment files for a request file in `dir`.
    pub fn load(dir: &Path, root: Option<&Path>) -> Self {
        let mut env = Self::default();
        if let Some(p) = find_file(dir, root, PUBLIC_FILE) {
            if let Some(m) = read_env_file(&p, &mut env.errors) {
                env.public = m;
            }
            env.files.push(p);
        }
        if let Some(p) = find_file(dir, root, PRIVATE_FILE) {
            if let Some(m) = read_env_file(&p, &mut env.errors) {
                env.private = m;
            }
            env.files.push(p);
        }
        env
    }

    /// Environment names (without `$shared`), sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.public.keys().chain(self.private.keys()).filter(|k| k.as_str() != SHARED).cloned().collect();
        v.sort();
        v.dedup();
        v
    }

    /// Variables of `env` (or only `$shared` without one).
    pub fn variables(&self, env: Option<&str>) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut add = |m: &Map<String, Value>, name: &str| {
            if let Some(Value::Object(vars)) = m.get(name) {
                for (k, v) in vars {
                    if let Some(s) = scalar(v) {
                        out.insert(k.clone(), s);
                    }
                }
            }
        };
        add(&self.public, SHARED);
        add(&self.private, SHARED);
        if let Some(e) = env.filter(|e| *e != SHARED) {
            add(&self.public, e);
            add(&self.private, e);
        }
        out
    }
}

/// A variable value as text (objects such as auth configurations are skipped).
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Set `env.name = value` in the environment file `path` (created when missing). Returns the
/// line of the variable in the rewritten file.
pub fn set_variable(path: &Path, env: &str, name: &str, value: &str) -> Result<usize, HttpError> {
    let mut errors = Vec::new();
    let mut root = if path.exists() {
        read_env_file(path, &mut errors).ok_or_else(|| HttpError::Parse(errors.join("; ")))?
    } else {
        Map::new()
    };
    let entry = root.entry(env.to_string()).or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    if let Value::Object(vars) = entry {
        vars.insert(name.to_string(), Value::String(value.to_string()));
    }
    let text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| HttpError::Parse(e.to_string()))?;
    write_text(path, &format!("{text}\n"))?;
    let needle = format!("\"{}\":", name.replace('\\', "\\\\").replace('"', "\\\""));
    let env_needle = format!("\"{}\":", env.replace('\\', "\\\\").replace('"', "\\\""));
    let env_line = text.lines().position(|l| l.trim_start().starts_with(&env_needle)).unwrap_or(0);
    Ok(text
        .lines()
        .enumerate()
        .skip(env_line)
        .find(|(_, l)| l.trim_start().starts_with(&needle))
        .map_or(env_line, |(i, _)| i))
}

/// Merge `vars` into environment `env` of the file `path` (created when missing).
pub fn merge_variables(path: &Path, env: &str, vars: &BTreeMap<String, String>) -> Result<(), HttpError> {
    let mut errors = Vec::new();
    let mut root = if path.exists() {
        read_env_file(path, &mut errors).ok_or_else(|| HttpError::Parse(errors.join("; ")))?
    } else {
        Map::new()
    };
    let entry = root.entry(env.to_string()).or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    if let Value::Object(m) = entry {
        for (k, v) in vars {
            m.insert(k.clone(), Value::String(v.clone()));
        }
    }
    let text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| HttpError::Parse(e.to_string()))?;
    write_text(path, &format!("{text}\n"))
}

pub(crate) fn write_text(path: &Path, text: &str) -> Result<(), HttpError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| HttpError::Io(format!("cannot create {}: {e}", dir.display())))?;
    }
    let tmp = path.with_extension("codecraft-tmp");
    std::fs::write(&tmp, text).map_err(|e| HttpError::Io(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        HttpError::Io(format!("cannot write {}: {e}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_wins_and_shared_applies() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("api");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            dir.path().join(PUBLIC_FILE),
            r#"{"dev": {"host": "localhost", "port": 8080, "debug": true, "auth": {"Security": {}}}, "prod": {"host": "example.org"}, "$shared": {"v": "1", "host": "shared"}}"#,
        )
        .unwrap();
        std::fs::write(sub.join(PRIVATE_FILE), r#"{"dev": {"token": "secret", "host": "private-host"}}"#).unwrap();
        let env = Environments::load(&sub, Some(dir.path()));
        assert_eq!(env.files.len(), 2);
        assert_eq!(env.names(), vec!["dev".to_string(), "prod".to_string()]);
        let dev = env.variables(Some("dev"));
        assert_eq!(dev.get("host").map(String::as_str), Some("private-host"));
        assert_eq!(dev.get("port").map(String::as_str), Some("8080"));
        assert_eq!(dev.get("debug").map(String::as_str), Some("true"));
        assert_eq!(dev.get("token").map(String::as_str), Some("secret"));
        assert_eq!(dev.get("v").map(String::as_str), Some("1"));
        assert!(!dev.contains_key("auth"));
        let none = env.variables(None);
        assert_eq!(none.get("host").map(String::as_str), Some("shared"));
        // Without a root, parents are not searched.
        assert!(Environments::load(&sub, None).public.is_empty());
    }

    #[test]
    fn malformed_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PUBLIC_FILE), "{ nope").unwrap();
        std::fs::write(dir.path().join(PRIVATE_FILE), "[1]").unwrap();
        let env = Environments::load(dir.path(), None);
        assert_eq!(env.errors.len(), 2);
        assert!(env.names().is_empty());
    }

    #[test]
    fn set_and_merge_variables() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PUBLIC_FILE);
        let line = set_variable(&p, "dev", "host", "localhost").unwrap();
        assert_eq!(line, 2);
        set_variable(&p, "prod", "host", "example.org").unwrap();
        let mut m = BTreeMap::new();
        m.insert("token".to_string(), "x".to_string());
        merge_variables(&p, "dev", &m).unwrap();
        let env = Environments::load(dir.path(), None);
        assert_eq!(env.variables(Some("dev")).len(), 2);
        assert_eq!(env.variables(Some("prod")).get("host").map(String::as_str), Some("example.org"));
        std::fs::write(&p, "garbage").unwrap();
        assert!(set_variable(&p, "dev", "a", "b").is_err());
    }
}
