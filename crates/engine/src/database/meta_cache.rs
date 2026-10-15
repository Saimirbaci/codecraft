//! Metadata cache across disconnects (track DX).
//!
//! The introspected tree of each data source (schemas, tables, the columns, indexes and keys
//! loaded so far, row counts) is saved to `<project>/.codecraft/db-cache/<data source>.json`
//! when the data source disconnects, when the project closes and a few seconds after new
//! metadata arrived. When the project opens again (or after a disconnect), the tree is shown
//! from the cache, greyed and marked stale, without connecting; SQL completion, Go to
//! Declaration and Database Object use it offline. Connecting replaces it: the schema list and
//! every cached level are loaded again.
//!
//! The file records which server it describes (kind, host, port, database, file, user): a
//! cache of another configuration is ignored, and editing those fields forgets it. Writes are
//! atomic and happen on a background thread (at shutdown, on the closing thread); reads happen
//! on a background thread too. Unreadable or oversized files are ignored, never fatal. The
//! directory gets a `.gitignore` so the snapshot is never committed by accident.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use codecraft_db::{DataSourceConfig, DatabaseInfo, TableDetails, TableInfo};

use super::{DataSource, DbState, Event, SchemaNode, SchemaTree, TableNode};

/// Cache files, relative to the project root.
pub const CACHE_DIR: &str = ".codecraft/db-cache";
/// Files larger than this are neither written nor read.
pub const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;
/// Tables per schema kept in a cache.
pub const MAX_TABLES: usize = 50_000;
/// How long new metadata waits before it is saved (several arrive together).
pub const SAVE_DELAY: Duration = Duration::from_secs(3);
/// Cached tables whose details are reloaded after reconnecting.
const MAX_DETAIL_REFRESH: usize = 200;
const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CachedTable {
    pub info: Option<TableInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<TableDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CachedSchema {
    /// `None`: the schema's tables were never loaded.
    pub tables: Option<Vec<CachedTable>>,
}

/// What a cache file holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CachedTree {
    pub version: u32,
    /// Which server it describes ([`fingerprint`]).
    pub fingerprint: String,
    /// When it was saved (seconds since the Unix epoch).
    pub saved_unix: u64,
    pub info: Option<DatabaseInfo>,
    pub schemas: BTreeMap<String, CachedSchema>,
}

/// Saving and refreshing state, per session.
#[derive(Debug, Default)]
pub struct MetaCache {
    /// Data sources with metadata not saved yet, and since when.
    dirty: HashMap<String, Instant>,
    /// Schemas whose cached tables are being reloaded after reconnecting (data source →
    /// schemas): their cached details are kept until fresh ones arrive.
    refreshing: HashMap<String, BTreeSet<String>>,
    /// Reads in flight (data source ids).
    loading: BTreeSet<String>,
    /// Data sources whose cache file holds their current tree (nothing to write).
    clean: BTreeSet<String>,
}

impl MetaCache {
    pub fn is_loading(&self) -> bool {
        !self.loading.is_empty()
    }
}

/// The server a configuration points at: a cache of another one is ignored.
pub fn fingerprint(cfg: &DataSourceConfig) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}",
        cfg.kind,
        cfg.host.trim(),
        cfg.port_or_default(),
        cfg.database.trim(),
        cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        cfg.user.trim()
    )
}

/// The cache file of a data source.
pub fn cache_path(root: &Path, id: &str) -> PathBuf {
    let name: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .take(100)
        .collect();
    let name = if name.is_empty() { "_".to_string() } else { name };
    root.join(CACHE_DIR).join(format!("{name}.json"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The cacheable part of a data source's tree; `None` while nothing was introspected.
pub fn snapshot(ds: &DataSource) -> Option<CachedTree> {
    let info = ds.tree.info.clone()?;
    let schemas = ds
        .tree
        .schemas
        .iter()
        .map(|(name, node)| {
            let tables = node.tables.as_ref().map(|ts| {
                ts.iter()
                    .take(MAX_TABLES)
                    .map(|t| CachedTable {
                        info: Some(t.info.clone()),
                        details: t.details.clone(),
                        row_count: t.row_count,
                    })
                    .collect()
            });
            (name.clone(), CachedSchema { tables })
        })
        .collect();
    Some(CachedTree {
        version: VERSION,
        fingerprint: fingerprint(&ds.config),
        saved_unix: now_unix(),
        info: Some(info),
        schemas,
    })
}

/// A cached tree as the explorer shows it: stale, nothing loading.
pub fn to_tree(c: CachedTree) -> SchemaTree {
    let schemas = c
        .schemas
        .into_iter()
        .map(|(name, sc)| {
            let tables = sc.tables.map(|ts| {
                ts.into_iter()
                    .filter_map(|t| {
                        let info = t.info?;
                        Some(TableNode {
                            info,
                            details: t.details,
                            loading: false,
                            row_count: t.row_count,
                            counting: false,
                            error: None,
                        })
                    })
                    .collect()
            });
            (name, SchemaNode { tables, loading: false, error: None })
        })
        .collect();
    SchemaTree {
        info: c.info,
        loading: false,
        error: None,
        schemas,
        stale: true,
        cached_unix: Some(c.saved_unix).filter(|t| *t > 0),
    }
}

/// The tree kept after a disconnect: the same metadata, stale.
pub fn stale_copy(tree: &SchemaTree) -> SchemaTree {
    let mut t = tree.clone();
    t.loading = false;
    t.stale = true;
    t.cached_unix = Some(now_unix());
    for node in t.schemas.values_mut() {
        node.loading = false;
        for table in node.tables.iter_mut().flatten() {
            table.loading = false;
            table.counting = false;
        }
    }
    t
}

/// Write a cache file atomically (with the directory's `.gitignore`).
pub fn write(path: &Path, tree: &CachedTree) -> Result<(), String> {
    let json = serde_json::to_string(tree).map_err(|e| e.to_string())?;
    if json.len() as u64 > MAX_CACHE_BYTES {
        return Err(format!("the metadata of {} is too large to cache", path.display()));
    }
    let dir = path.parent().ok_or("no cache directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# CodeCraft database metadata cache\n*\n");
    }
    codecraft_fs::write_text_file_atomic(path, &json).map_err(|e| e.to_string())
}

/// Read a cache file; `None` when missing, unreadable, too large, of another version or of
/// another server.
pub fn read(path: &Path, fingerprint: &str) -> Option<CachedTree> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CACHE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<CachedTree>(&text) {
        Ok(c) if c.version == VERSION && c.fingerprint == fingerprint && c.info.is_some() => Some(c),
        Ok(_) => None,
        Err(e) => {
            log::warn!("ignoring the metadata cache {}: {e}", path.display());
            None
        }
    }
}

impl DbState {
    /// New metadata of `source` arrived: save it soon.
    pub(crate) fn cache_dirty(&mut self, source: &str) {
        self.cache.clean.remove(source);
        self.cache.dirty.entry(source.to_string()).or_insert_with(Instant::now);
    }

    /// Save the cache of one data source now (on a background thread unless `wait`).
    pub(crate) fn save_cache(&mut self, source: &str, wait: bool) {
        self.cache.dirty.remove(source);
        self.cache.clean.insert(source.to_string());
        let Some(root) = self.root.clone() else { return };
        let Some(ds) = self.source(source) else { return };
        let Some(snap) = snapshot(ds) else { return };
        let path = cache_path(&root, source);
        let job = move || {
            if let Err(e) = write(&path, &snap) {
                log::warn!("cannot save the metadata cache: {e}");
            }
        };
        if wait {
            job();
        } else if std::thread::Builder::new().name("db-cache-write".into()).spawn(job).is_err() {
            log::warn!("cannot start the metadata cache writer");
        }
    }

    /// Save every data source's cache now (closing the project or the window).
    pub(crate) fn save_caches_now(&mut self) {
        let ids: Vec<String> = self
            .sources
            .iter()
            .filter(|d| d.tree.info.is_some() && !self.cache.clean.contains(&d.config.id))
            .map(|d| d.config.id.clone())
            .collect();
        for id in ids {
            self.save_cache(&id, true);
        }
    }

    /// Save what has waited long enough (called by the pump).
    pub(crate) fn flush_caches(&mut self) {
        let due: Vec<String> =
            self.cache.dirty.iter().filter(|(_, t)| t.elapsed() >= SAVE_DELAY).map(|(k, _)| k.clone()).collect();
        for id in due {
            self.save_cache(&id, false);
        }
    }

    /// Forget a data source's cache (its connection settings changed, or it was removed).
    pub(crate) fn forget_cache(&mut self, source: &str) {
        self.cache.dirty.remove(source);
        self.cache.clean.remove(source);
        self.cache.refreshing.remove(source);
        if let Some(ds) = self.source_mut(source)
            && ds.tree.stale
        {
            ds.tree = SchemaTree::default();
        }
        if let Some(root) = &self.root {
            let path = cache_path(root, source);
            let _ = std::fs::remove_file(path);
        }
    }

    /// Read the caches of every data source in the background (the project opened).
    pub(crate) fn load_caches(&mut self) {
        let Some(root) = self.root.clone() else { return };
        let wanted: Vec<(String, String)> =
            self.sources.iter().map(|d| (d.config.id.clone(), fingerprint(&d.config))).collect();
        if wanted.is_empty() {
            return;
        }
        self.cache.loading.extend(wanted.iter().map(|(id, _)| id.clone()));
        let events = self.tx.clone();
        let spawned = std::thread::Builder::new().name("db-cache-read".into()).spawn(move || {
            for (id, fp) in wanted {
                let tree = read(&cache_path(&root, &id), &fp);
                let _ = events.send(Event::MetaCache { source: id, root: root.clone(), tree });
            }
        });
        if spawned.is_err() {
            self.cache.loading.clear();
        }
    }

    /// The tree of a data source after a disconnect: kept, stale, when it had metadata.
    pub(crate) fn keep_stale(&mut self, source: &str) -> bool {
        let Some(ds) = self.source(source) else { return false };
        if ds.tree.info.is_none() {
            return false;
        }
        // A tree the cache file already holds has nothing new to save.
        if !self.cache.clean.contains(source) {
            self.save_cache(source, false);
        }
        if let Some(ds) = self.source_mut(source) {
            ds.tree = stale_copy(&ds.tree);
        }
        true
    }
}

/// A cache file was read.
pub(crate) fn on_loaded(db: &mut DbState, source: &str, root: &Path, tree: Option<CachedTree>) {
    db.cache.loading.remove(source);
    // Another project opened meanwhile.
    if db.root.as_deref() != Some(root) {
        return;
    }
    let Some(tree) = tree else { return };
    let Some(ds) = db.source_mut(source) else { return };
    // Live metadata won the race.
    if ds.tree.info.is_some() || ds.tree.loading {
        return;
    }
    ds.tree = to_tree(tree);
    db.cache.clean.insert(source.to_string());
}

/// The schema list arrived on a connection whose tree was stale: reload every cached level.
/// Returns the requests to send.
pub(crate) fn refresh_after_connect(db: &mut DbState, source: &str, was_stale: bool) -> Vec<super::MetaReq> {
    if !was_stale {
        return Vec::new();
    }
    let Some(ds) = db.source(source) else { return Vec::new() };
    let schemas: BTreeSet<String> =
        ds.tree.schemas.iter().filter(|(_, n)| n.tables.is_some()).map(|(k, _)| k.clone()).collect();
    let reqs = schemas.iter().map(|s| super::MetaReq::Tables(s.clone())).collect();
    if !schemas.is_empty() {
        db.cache.refreshing.insert(source.to_string(), schemas);
    }
    reqs
}

/// Fresh tables of a schema arrived. When the schema was reloaded after reconnecting, the
/// cached details of tables that still exist are kept until their reload (requested here)
/// replaces them. Returns the requests to send.
pub(crate) fn carry_details(
    db: &mut DbState,
    source: &str,
    schema: &str,
    old: Option<Vec<TableNode>>,
) -> Vec<super::MetaReq> {
    let Some(set) = db.cache.refreshing.get_mut(source) else { return Vec::new() };
    if !set.remove(schema) {
        return Vec::new();
    }
    if set.is_empty() {
        db.cache.refreshing.remove(source);
    }
    let Some(old) = old else { return Vec::new() };
    let mut reqs = Vec::new();
    let Some(node) = db.source_mut(source).and_then(|d| d.tree.schemas.get_mut(schema)) else { return reqs };
    for t in node.tables.iter_mut().flatten() {
        let Some(prev) = old.iter().find(|o| o.info.name == t.info.name) else { continue };
        if t.details.is_none()
            && let Some(d) = &prev.details
        {
            t.details = Some(d.clone());
            if reqs.len() < MAX_DETAIL_REFRESH {
                reqs.push(super::MetaReq::Details(schema.to_string(), t.info.name.clone()));
            }
        }
        if t.row_count.is_none() {
            t.row_count = prev.row_count;
        }
    }
    reqs
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_db::{ColumnInfo, DbKind, TableKind};

    fn source() -> DataSource {
        let config =
            DataSourceConfig { id: "pg/1".into(), kind: DbKind::Postgres, host: "h".into(), ..Default::default() };
        let mut tree = SchemaTree {
            info: Some(DatabaseInfo {
                schemas: vec!["public".into(), "other".into()],
                default_schema: Some("public".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut t = TableNode {
            info: TableInfo { name: "users".into(), kind: TableKind::Table },
            details: None,
            loading: true,
            row_count: Some(3),
            counting: true,
            error: None,
        };
        t.details = Some(TableDetails {
            columns: vec![ColumnInfo { name: "id".into(), data_type: "int".into(), ..Default::default() }],
            ..Default::default()
        });
        tree.schemas.insert("public".into(), SchemaNode { tables: Some(vec![t]), loading: false, error: None });
        tree.schemas.insert("other".into(), SchemaNode::default());
        DataSource { config, status: super::super::ConnStatus::Connected("PostgreSQL".into()), tree }
    }

    #[test]
    fn round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let ds = source();
        let snap = snapshot(&ds).unwrap();
        let path = cache_path(dir.path(), &ds.config.id);
        assert!(path.ends_with("db-cache/pg_1.json"), "{}", path.display());
        write(&path, &snap).unwrap();
        assert!(dir.path().join(CACHE_DIR).join(".gitignore").is_file());
        let back = read(&path, &fingerprint(&ds.config)).unwrap();
        assert_eq!(back, snap);
        // Another server: ignored.
        let mut other = ds.config.clone();
        other.host = "elsewhere".into();
        assert!(read(&path, &fingerprint(&other)).is_none());
        let tree = to_tree(back);
        assert!(tree.stale && !tree.loading);
        let users = tree.schemas.get("public").and_then(|n| n.tables.as_ref()).unwrap();
        assert_eq!(users[0].details.as_ref().unwrap().columns[0].name, "id");
        assert!(!users[0].loading && !users[0].counting);
        assert!(tree.schemas.get("other").unwrap().tables.is_none());
        // Garbage, wrong versions and missing files are ignored.
        std::fs::write(&path, "{not json").unwrap();
        assert!(read(&path, &fingerprint(&ds.config)).is_none());
        std::fs::write(&path, r#"{"version": 99}"#).unwrap();
        assert!(read(&path, &fingerprint(&ds.config)).is_none());
        assert!(read(&dir.path().join("none.json"), "").is_none());
    }

    #[test]
    fn nothing_to_cache_without_metadata_and_stale_copies_stop_loading() {
        let mut ds = source();
        let stale = stale_copy(&ds.tree);
        assert!(stale.stale);
        assert!(!stale.schemas["public"].tables.as_ref().unwrap()[0].loading);
        ds.tree = SchemaTree::default();
        assert!(snapshot(&ds).is_none());
        assert_eq!(cache_path(Path::new("/p"), ""), Path::new("/p").join(CACHE_DIR).join("_.json"));
    }
}
