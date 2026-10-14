//! crates.io lookups: the sparse index (`<index>/<prefix>/<name>`, one JSON object per published
//! version) for versions, and the crates.io search API for crate names. Results are cached on disk
//! (`$CODECRAFT_HOME/cache/crates-index/<name>.json`) with a time to live. Blocking: callers run
//! these on worker threads. Base URLs are injectable so tests use a local server.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::semver::Version;

/// The public sparse index of crates.io.
pub const DEFAULT_INDEX_URL: &str = "https://index.crates.io";
/// The crates.io web API (search).
pub const DEFAULT_API_URL: &str = "https://crates.io";
/// crates.io asks API clients to identify themselves.
const USER_AGENT: &str = "CodeCraft IDE (Cargo.toml dependency completion)";
/// Largest index file or search response read.
const MAX_BODY: u64 = 32 << 20;
/// Versions kept per crate (the newest ones).
const MAX_VERSIONS: usize = 5000;

/// Where lookups go and how long cached answers stay fresh.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub index_url: String,
    pub api_url: String,
    /// `None`: no disk cache.
    pub cache_dir: Option<PathBuf>,
    pub ttl: Duration,
    /// Network allowed (otherwise only the disk cache answers).
    pub online: bool,
}

/// One published version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedVersion {
    #[serde(rename = "v")]
    pub version: String,
    #[serde(default)]
    pub yanked: bool,
}

/// What the index knows about a crate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateVersions {
    pub name: String,
    /// Oldest first, like the index.
    pub versions: Vec<PublishedVersion>,
    /// The index has no such crate (HTTP 404).
    #[serde(default)]
    pub not_found: bool,
    /// Unix seconds of the fetch.
    #[serde(default)]
    pub fetched: u64,
}

impl CrateVersions {
    /// Parsed, non-yanked versions.
    pub fn available(&self) -> Vec<Version> {
        self.versions.iter().filter(|v| !v.yanked).filter_map(|v| Version::parse(&v.version)).collect()
    }

    /// Non-yanked versions, newest first (stable before pre-releases of the same rank).
    pub fn newest_first(&self) -> Vec<Version> {
        let mut v = self.available();
        v.sort_by(|a, b| b.cmp(a));
        v
    }
}

/// A crate from the search API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub max_version: Option<String>,
    pub description: Option<String>,
}

/// Whether a crate name is valid for the index (ASCII alphanumerics, `-`, `_`; at most 64 chars).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The sparse index path of a crate: `1/a`, `2/ab`, `3/a/abc`, `se/rd/serde` (lowercased).
pub fn index_path(name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    let n = name.to_ascii_lowercase();
    let path = match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", n.get(..1)?),
        _ => format!("{}/{}/{n}", n.get(..2)?, n.get(2..4)?),
    };
    Some(path)
}

/// Parse an index file (one JSON object per line; bad lines are skipped).
pub fn parse_index(name: &str, body: &str) -> CrateVersions {
    let mut versions: Vec<PublishedVersion> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let version = v.get("vers")?.as_str()?.to_string();
            let yanked = v.get("yanked").and_then(Value::as_bool).unwrap_or(false);
            Some(PublishedVersion { version, yanked })
        })
        .collect();
    if versions.len() > MAX_VERSIONS {
        versions.drain(..versions.len() - MAX_VERSIONS);
    }
    CrateVersions { name: name.to_string(), versions, not_found: false, fetched: now_secs() }
}

/// Parse a crates.io search response.
pub fn parse_search(body: &str) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    v.get("crates")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let name = c.get("name")?.as_str()?.to_string();
                    if !valid_name(&name) {
                        return None;
                    }
                    let max_version = c
                        .get("max_stable_version")
                        .and_then(Value::as_str)
                        .or_else(|| c.get("max_version").and_then(Value::as_str))
                        .map(str::to_string);
                    let description = c
                        .get("description")
                        .and_then(Value::as_str)
                        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "));
                    Some(SearchHit { name, max_version, description })
                })
                .take(100)
                .collect()
        })
        .unwrap_or_default()
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cache_file(dir: &Path, name: &str) -> Option<PathBuf> {
    valid_name(name).then(|| dir.join(format!("{}.json", name.to_ascii_lowercase())))
}

/// The cached entry of a crate (any age).
pub fn read_cache(dir: &Path, name: &str) -> Option<CrateVersions> {
    let file = cache_file(dir, name)?;
    let meta = std::fs::metadata(&file).ok()?;
    if meta.len() > MAX_BODY {
        return None;
    }
    let text = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(dir: &Path, entry: &CrateVersions) -> Result<(), String> {
    let file = cache_file(dir, &entry.name).ok_or("invalid crate name")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &file).map_err(|e| e.to_string())
}

/// Names of every crate in the disk cache.
pub fn cached_names(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string))
        .filter(|n| valid_name(n))
        .take(20_000)
        .collect()
}

fn get(url: &str) -> Result<(u16, String), String> {
    let req = codecraft_http::vars::ResolvedRequest {
        method: "GET".into(),
        url: url.to_string(),
        headers: vec![("User-Agent".into(), USER_AGENT.into()), ("Accept".into(), "application/json".into())],
        follow_redirects: true,
        ..Default::default()
    };
    let opts = codecraft_http::client::ExecOptions {
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(15),
        max_keep: MAX_BODY as usize,
        max_total: MAX_BODY,
        body_file: None,
    };
    let resp = codecraft_http::client::execute(&req, &opts).map_err(|e| e.to_string())?;
    Ok((resp.status, String::from_utf8_lossy(&resp.body).into_owned()))
}

/// The versions of a crate: a fresh disk cache entry, else the index (cached afterwards), else a
/// stale cache entry when the index cannot be reached. `force` skips the freshness check.
pub fn versions(ep: &Endpoint, name: &str, force: bool) -> Result<CrateVersions, String> {
    let path = index_path(name).ok_or_else(|| format!("`{name}` is not a valid crate name"))?;
    let cached = ep.cache_dir.as_deref().and_then(|d| read_cache(d, name));
    let fresh = |c: &CrateVersions| now_secs().saturating_sub(c.fetched) < ep.ttl.as_secs();
    if let Some(c) = &cached
        && !force
        && fresh(c)
    {
        return Ok(c.clone());
    }
    if !ep.online {
        return cached.ok_or_else(|| "crates.io lookups are turned off".to_string());
    }
    let url = format!("{}/{}", ep.index_url.trim_end_matches('/'), path);
    let fetched = match get(&url) {
        Ok((200, body)) => Ok(parse_index(name, &body)),
        Ok((404 | 410 | 451, _)) => {
            Ok(CrateVersions { name: name.to_string(), not_found: true, fetched: now_secs(), ..Default::default() })
        }
        Ok((status, _)) => Err(format!("the crates.io index answered HTTP {status}")),
        Err(e) => Err(e),
    };
    match fetched {
        Ok(entry) => {
            if let Some(dir) = &ep.cache_dir
                && let Err(e) = write_cache(dir, &entry)
            {
                log::warn!("crates index cache: {e}");
            }
            Ok(entry)
        }
        Err(e) => cached.ok_or(e),
    }
}

/// Search crates.io for names (`q`, at most 20 results).
pub fn search(ep: &Endpoint, query: &str) -> Result<Vec<SearchHit>, String> {
    if !ep.online {
        return Err("crates.io lookups are turned off".into());
    }
    let q: String = query.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/api/v1/crates?q={q}&per_page=20", ep.api_url.trim_end_matches('/'));
    match get(&url)? {
        (200, body) => Ok(parse_search(&body)),
        (status, _) => Err(format!("crates.io search answered HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_paths() {
        assert_eq!(index_path("a").as_deref(), Some("1/a"));
        assert_eq!(index_path("ab").as_deref(), Some("2/ab"));
        assert_eq!(index_path("abc").as_deref(), Some("3/a/abc"));
        assert_eq!(index_path("Serde").as_deref(), Some("se/rd/serde"));
        assert_eq!(index_path("serde_json").as_deref(), Some("se/rd/serde_json"));
        assert_eq!(index_path(""), None);
        assert_eq!(index_path("../etc"), None);
        assert_eq!(index_path("é"), None);
        assert_eq!(index_path(&"a".repeat(65)), None);
    }

    #[test]
    fn index_files_parse_and_skip_garbage() {
        let body = "{\"name\":\"x\",\"vers\":\"0.1.0\",\"yanked\":false}\nnot json\n{\"vers\":\"0.2.0\",\"yanked\":true}\n\n{\"vers\":\"0.3.0-beta.1\"}\n{\"vers\":\"bad\"}\n";
        let c = parse_index("x", body);
        assert_eq!(c.versions.len(), 4);
        let avail: Vec<String> = c.newest_first().iter().map(ToString::to_string).collect();
        assert_eq!(avail, ["0.3.0-beta.1", "0.1.0"]);
    }

    #[test]
    fn search_responses_parse() {
        let body = r#"{"crates":[{"name":"serde","max_version":"1.0.210","max_stable_version":"1.0.210","description":"A generic\n serialization framework"},{"name":"bad name"},{"name":"serde_json","max_version":"1.0.1"}],"meta":{"total":2}}"#;
        let hits = parse_search(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].description.as_deref(), Some("A generic serialization framework"));
        assert_eq!(hits[1].max_version.as_deref(), Some("1.0.1"));
        assert!(parse_search("{").is_empty());
        assert!(parse_search("{\"crates\": 3}").is_empty());
    }

    #[test]
    fn offline_lookups_use_only_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let ep = Endpoint {
            index_url: "http://127.0.0.1:9".into(),
            api_url: "http://127.0.0.1:9".into(),
            cache_dir: Some(tmp.path().to_path_buf()),
            ttl: Duration::from_secs(60),
            online: false,
        };
        assert!(versions(&ep, "serde", false).is_err());
        assert!(search(&ep, "ser").is_err());
        let mut entry = parse_index("serde", "{\"vers\":\"1.0.0\"}\n");
        entry.fetched = 0;
        write_cache(tmp.path(), &entry).unwrap();
        // Stale, but offline: the cache answers.
        assert_eq!(versions(&ep, "Serde", false).unwrap().versions.len(), 1);
        assert_eq!(cached_names(tmp.path()), ["serde"]);
        assert!(versions(&ep, "no/such", false).is_err());
    }
}
