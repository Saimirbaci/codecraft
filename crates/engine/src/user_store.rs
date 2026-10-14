//! Per-user files of the editor features (saved macros, structural search templates, window
//! layouts, file templates, the copyright profile): `$CODECRAFT_HOME/<name>` (default
//! `~/.codecraft/<name>`). [`crate::Session::isolate_home`] redirects them for tests.
//!
//! Loading never fails hard: a missing file is the default value, a corrupt one is reported and
//! set aside (renamed to `<name>.bad`) on the next save instead of being overwritten.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Largest per-user JSON file read (anything bigger is treated as corrupt).
const MAX_FILE_BYTES: u64 = 16 << 20;

/// Where the per-user files live.
#[derive(Clone, Debug, Default)]
pub struct UserHome {
    /// Explicit directory (tests and embedders); otherwise `$CODECRAFT_HOME` or `~/.codecraft`.
    pub dir_override: Option<PathBuf>,
}

impl UserHome {
    pub fn dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.dir_override {
            return Some(d.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".codecraft"))
    }

    /// A file (or directory) under the home directory.
    pub fn file(&self, name: &str) -> Option<PathBuf> {
        self.dir().map(|d| d.join(name))
    }

    /// Like [`UserHome::file`], with the error message commands report.
    pub fn require(&self, name: &str) -> Result<PathBuf, String> {
        self.file(name).ok_or_else(|| "cannot find the home directory; set CODECRAFT_HOME".to_string())
    }
}

/// Read a JSON file: `Ok(None)` when it does not exist, `Err` when it cannot be read or parsed.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > MAX_FILE_BYTES => {
            return Err(format!("{} is too large ({} bytes)", path.display(), m.len()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map(Some).map_err(|e| format!("ignoring corrupt {}: {e}", path.display()))
}

/// Write a JSON file atomically (creating its directory). `set_aside`: rename the existing file
/// to `<name>.bad` first (it was corrupt and must not be lost).
pub fn save_json<T: Serialize>(path: &Path, value: &T, set_aside: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if set_aside && path.exists() {
        let mut bad = path.as_os_str().to_owned();
        bad.push(".bad");
        let _ = std::fs::rename(path, PathBuf::from(bad));
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    codecraft_fs::write_text_file_atomic(path, &text).map_err(|e| format!("cannot save {}: {e}", path.display()))
}

/// A JSON-backed list of named entries, loaded lazily from one per-user file.
#[derive(Debug)]
pub struct Store<T> {
    pub items: Vec<T>,
    loaded: bool,
    corrupt: bool,
    /// The last load or save error.
    pub error: Option<String>,
    name: &'static str,
}

impl<T> Store<T> {
    pub const fn new(name: &'static str) -> Self {
        Self { items: Vec::new(), loaded: false, corrupt: false, error: None, name }
    }

    /// Forget what was loaded (the next access reads the file again).
    pub fn reset(&mut self) {
        self.items.clear();
        self.loaded = false;
        self.corrupt = false;
        self.error = None;
    }
}

impl<T: Serialize + DeserializeOwned> Store<T> {
    /// Load the file once (missing → empty; corrupt → empty and reported).
    pub fn ensure(&mut self, home: &UserHome) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = home.file(self.name) else { return };
        match load_json::<Vec<T>>(&path) {
            Ok(Some(items)) => self.items = items,
            Ok(None) => {}
            Err(e) => {
                log::warn!("{e}");
                self.error = Some(e);
                self.corrupt = true;
            }
        }
    }

    pub fn save(&mut self, home: &UserHome) -> Result<PathBuf, String> {
        let path = home.require(self.name)?;
        let r = save_json(&path, &self.items, self.corrupt);
        match &r {
            Ok(()) => {
                self.corrupt = false;
                self.error = None;
            }
            Err(e) => self.error = Some(e.clone()),
        }
        r.map(|()| path)
    }
}

/// The current calendar year (UTC), from the system clock.
pub fn current_year() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    civil_from_days(days).0
}

/// The current date (UTC) as `YYYY-MM-DD`.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian calendar (Howard
/// Hinnant's public-domain `civil_from_days` algorithm).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        let _ = civil_from_days(i64::MAX);
        let _ = civil_from_days(i64::MIN);
        assert!(current_year() >= 2024);
        assert_eq!(today().len(), 10);
    }

    #[test]
    fn store_roundtrip_and_corrupt_file_set_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let home = UserHome { dir_override: Some(tmp.path().to_path_buf()) };
        let mut s: Store<String> = Store::new("things.json");
        s.ensure(&home);
        assert!(s.items.is_empty());
        s.items.push("a".into());
        s.save(&home).unwrap();
        let mut again: Store<String> = Store::new("things.json");
        again.ensure(&home);
        assert_eq!(again.items, vec!["a".to_string()]);

        std::fs::write(tmp.path().join("things.json"), "{ not json").unwrap();
        let mut bad: Store<String> = Store::new("things.json");
        bad.ensure(&home);
        assert!(bad.error.is_some());
        bad.items.push("b".into());
        bad.save(&home).unwrap();
        assert!(tmp.path().join("things.json.bad").exists());
        assert!(bad.error.is_none());
    }
}
