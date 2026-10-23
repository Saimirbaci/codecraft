//! Standalone version store. Contract stable: public types and operations are additive.
#![forbid(unsafe_code)]

pub mod build;
pub mod health;
pub mod preflight;
mod snapshot;
pub mod source;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FACTORY: &str = "factory";
pub const BOOT_WINDOW: u64 = 300;
pub const MAX_ATTEMPTS: u32 = 2;
pub const LOG_LIMIT: u64 = 1024 * 1024;
const JSON_LIMIT: u64 = 64 * 1024;
const VERSION_LIST_LIMIT: usize = 1024;
static SERIAL: AtomicU64 = AtomicU64::new(0);
pub type Result<T> = io::Result<T>;
fn invalid(s: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s.into())
}
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
pub fn default_base() -> Result<PathBuf> {
    std::env::var_os("CODECRAFT_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".codecraft")))
        .ok_or_else(|| invalid("set CODECRAFT_HOME: home directory unavailable"))
}

/// Validate a single unambiguous path component and its semver/shortsha/counter structure.
pub fn valid_id(id: &str) -> bool {
    if id.len() > 160 || !id.is_ascii() {
        return false;
    }
    let mut parts = id.rsplitn(3, '-');
    let n = parts.next().unwrap_or("");
    let sha = parts.next().unwrap_or("");
    let semver = parts.next().unwrap_or("");
    valid_semver(semver)
        && (4..=40).contains(&sha.len())
        && sha.bytes().all(|b| b.is_ascii_hexdigit())
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
        && n.parse::<u64>().is_ok()
}
fn valid_semver(version: &str) -> bool {
    fn identifiers(s: &str, numeric_zero: bool) -> bool {
        !s.is_empty()
            && s.split('.').all(|part| {
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && (!numeric_zero
                        || !part.bytes().all(|b| b.is_ascii_digit())
                        || part.len() == 1
                        || !part.starts_with('0'))
            })
    }
    let (version, build) = version.split_once('+').map_or((version, None), |(v, b)| (v, Some(b)));
    if build.is_some_and(|b| !identifiers(b, false)) {
        return false;
    }
    let (core, pre) = version.split_once('-').map_or((version, None), |(v, p)| (v, Some(p)));
    if pre.is_some_and(|p| !identifiers(p, true)) {
        return false;
    }
    let nums: Vec<_> = core.split('.').collect();
    nums.len() == 3
        && nums.iter().all(|s| {
            !s.is_empty()
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.parse::<u64>().is_ok()
                && (s.len() == 1 || !s.starts_with('0'))
        })
}

fn check_id(id: &str) -> Result<()> {
    if valid_id(id) { Ok(()) } else { Err(invalid("invalid version id")) }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Staged,
    Installed,
    Good,
    Bad,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub semver: String,
    pub commit: String,
    pub built_at: u64,
    pub source_branch: String,
    pub parent: Option<String>,
    pub status: Status,
    pub reason: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub feedback_summary: String,
    #[serde(default)]
    pub feedback_job: Option<u64>,
    #[serde(default)]
    pub feedback_session: Option<String>,
    #[serde(default)]
    pub dirty_hash: Option<String>,
    #[serde(default)]
    pub user_commit: Option<String>,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub last_failure: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Boot {
    pub id: String,
    pub started_at: u64,
    pub attempts: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub schema: u32,
    pub current: Option<String>,
    pub last_good: Option<String>,
    pub previous: Option<String>,
    pub boot: Option<Boot>,
    pub pending_health: bool,
}
impl Default for State {
    fn default() -> Self {
        Self { schema: 1, current: None, last_good: None, previous: None, boot: None, pending_health: false }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub why: String,
}
#[derive(Clone, Debug, Default)]
pub struct BootState {
    pub state: State,
    pub available: BTreeSet<String>,
    pub bad: BTreeSet<String>,
}
/// Pure selection; the store supplies only validated, launchable metadata entries.
pub fn choose(boot: &BootState, _at: u64) -> Choice {
    let failed = boot.state.boot.as_ref().filter(|_| boot.state.pending_health);
    for (id, why) in [(&boot.state.current, "current"), (&boot.state.last_good, "last good")] {
        if let Some(id) = id
            && boot.available.contains(id)
            && !boot.bad.contains(id)
            && !failed.is_some_and(|b| &b.id == id)
        {
            return Choice { id: id.clone(), why: why.into() };
        }
    }
    Choice { id: FACTORY.into(), why: "factory fallback".into() }
}

/// Atomic replacement. Leftover temporary files are never read as pointer files.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("file needs parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".update-{}-{}", std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > JSON_LIMIT {
        return None;
    }
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > JSON_LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(JSON_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > JSON_LIMIT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value).map_err(|e| invalid(e.to_string()))?)
}

/// OS locks survive stale files and release automatically on process death. Never unlink locks.
pub struct Lock(File);
impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::open(path, false)
    }
    fn blocking(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        file.lock()?;
        Ok(Self(file))
    }
    pub fn shared(path: &Path) -> Result<Self> {
        Self::open(path, true)
    }
    fn open(path: &Path, shared: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        (if shared { file.try_lock_shared() } else { file.try_lock() })
            .map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, format!("store busy: {e}")))?;
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    pub base: PathBuf,
    pub factory: PathBuf,
}
impl Store {
    pub fn new(base: PathBuf, factory: PathBuf) -> Self {
        Self { base, factory }
    }
    pub fn lock(&self) -> Result<Lock> {
        Lock::acquire(&self.base.join("store.lock"))
    }
    pub fn launcher_lock(&self) -> Result<Lock> {
        Lock::acquire(&self.base.join("launcher.lock"))
    }
    pub fn status_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&serde_json::json!({ "state": self.state(), "versions": self.list(), "choice": choose(&self.boot_state(), now()), "factory": self.factory.to_string_lossy() })).map_err(|e| invalid(e.to_string()))
    }
    pub fn state(&self) -> State {
        read_json::<State>(&self.base.join("state.json"))
            .filter(|s| {
                s.schema == 1
                    && s.boot.as_ref().is_none_or(|b| b.id == FACTORY || valid_id(&b.id))
                    && [&s.current, &s.last_good, &s.previous]
                        .iter()
                        .all(|id| id.as_ref().is_none_or(|id| valid_id(id)))
            })
            .unwrap_or_default()
    }
    pub fn save_state(&self, state: &State) -> Result<()> {
        write_json(&self.base.join("state.json"), state)
    }
    pub fn version_dir(&self, id: &str) -> Result<PathBuf> {
        check_id(id)?;
        Ok(self.base.join("versions").join(id))
    }
    pub fn meta(&self, id: &str) -> Option<Meta> {
        let m = read_json::<Meta>(&self.version_dir(id).ok()?.join("meta.json"))?;
        (m.id == id).then_some(m)
    }
    pub fn list(&self) -> Vec<Meta> {
        let mut list: Vec<_> = fs::read_dir(self.base.join("versions"))
            .into_iter()
            .flatten()
            .flatten()
            .take(VERSION_LIST_LIMIT)
            .filter_map(|e| e.file_name().to_str().and_then(|id| self.meta(id)))
            .collect();
        list.sort_by_key(|m| std::cmp::Reverse(counter(&m.id)));
        list
    }
    pub fn boot_state(&self) -> BootState {
        let mut b = BootState { state: self.state(), ..Default::default() };
        for m in self.list() {
            if self.executable(&m.id).is_ok_and(|p| p.is_file()) {
                b.available.insert(m.id.clone());
            }
            if m.status == Status::Bad || m.reason.is_some() {
                b.bad.insert(m.id);
            }
        }
        b
    }
    pub fn executable(&self, id: &str) -> Result<PathBuf> {
        let dir = if id == FACTORY { self.factory.clone() } else { self.version_dir(id)?.join("payload") };
        let mac = dir.join("CodeCraft.app/Contents/MacOS/codecraft");
        if mac.is_file() {
            return Ok(mac);
        }
        Ok(dir.join(if cfg!(windows) { "codecraft.exe" } else { "codecraft" }))
    }
    /// Caller supplies source provenance; staging never changes active pointers.
    pub fn stage(&self, payload: &Path, semver: &str, commit: &str, branch: &str) -> Result<String> {
        if !(4..=64).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) || branch.len() > 4096 {
            return Err(invalid("invalid or oversized source provenance"));
        }
        let _lock = self.lock()?;
        let versions = self.base.join("versions");
        fs::create_dir_all(&versions)?;
        let n = fs::read_dir(&versions)?
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(counter))
            .max()
            .unwrap_or(0)
            .max(read_json::<u64>(&self.base.join("version-counter.json")).unwrap_or(0))
            .checked_add(1)
            .ok_or_else(|| invalid("version counter exhausted"))?;
        let sha: String = commit.chars().take(12).collect();
        let id = format!("{semver}-{sha}-{n}");
        check_id(&id)?;
        write_json(&self.base.join("version-counter.json"), &n)?;
        let dir = self.version_dir(&id)?;
        fs::create_dir(&dir)?;
        let result = (|| {
            let size = snapshot::copy_payload(payload, &dir.join("payload"))?;
            let meta = Meta {
                id: id.clone(),
                semver: semver.into(),
                commit: commit.into(),
                built_at: now(),
                source_branch: branch.into(),
                parent: self.state().current,
                status: Status::Staged,
                reason: None,
                size,
                feedback_summary: String::new(),
                feedback_job: None,
                feedback_session: None,
                dirty_hash: None,
                user_commit: None,
                failures: 0,
                last_failure: 0,
            };
            if !self.executable(&id)?.is_file() {
                return Err(invalid("payload has no app executable"));
            }
            write_json(&dir.join("meta.json"), &meta)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        result?;
        self.log(&format!("staged {id}"))?;
        Ok(id)
    }
    pub fn provenance(
        &self,
        id: &str,
        summary: &str,
        job: Option<u64>,
        dirty: Option<String>,
        user_commit: Option<String>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        let mut meta = self.meta(id).ok_or_else(|| invalid("missing version"))?;
        meta.feedback_summary = summary.chars().take(4096).collect();
        meta.feedback_job = job;
        meta.dirty_hash = dirty.map(|s| s.chars().take(128).collect());
        meta.user_commit = user_commit.filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        write_json(&self.version_dir(id)?.join("meta.json"), &meta)
    }
    pub fn feedback_session(&self, id: &str, session: Option<&str>) -> Result<()> {
        let _lock = self.lock()?;
        let mut meta = self.meta(id).ok_or_else(|| invalid("missing version"))?;
        meta.feedback_session = session.map(|s| s.chars().take(512).collect());
        write_json(&self.version_dir(id)?.join("meta.json"), &meta)
    }
    pub fn promote_pending(&self, id: &str) -> Result<()> {
        self.promote(id)
    }
    pub fn promote(&self, id: &str) -> Result<()> {
        self.use_version(id)
    }
    pub fn use_version(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut meta = self.meta(id).ok_or_else(|| invalid("missing/corrupt version metadata"))?;
        meta.status = Status::Installed;
        meta.reason = None;
        write_json(&self.version_dir(id)?.join("meta.json"), &meta)?;
        let mut state = self.state();
        if state.current.as_deref() != Some(id) {
            state.previous = state.current.take();
        }
        state.current = Some(id.into());
        state.pending_health = false;
        state.boot = None;
        self.save_state(&state)?;
        self.log(&format!("promoted {id}"))
    }
    pub fn mark_good(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut s = self.state();
        if id != FACTORY {
            self.set_status(id, Status::Good, None)?;
            s.last_good = Some(id.into());
        }
        s.pending_health = false;
        self.save_state(&s)?;
        self.log(&format!("healthy {id}"))
    }
    /// A clean, healthy process exit breaks the consecutive abnormal-exit streak.
    pub fn clean_exit(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        if id != FACTORY
            && let Some(mut m) = self.meta(id)
        {
            m.failures = 0;
            m.last_failure = 0;
            write_json(&self.version_dir(id)?.join("meta.json"), &m)?;
        }
        let mut state = self.state();
        if !state.pending_health && state.boot.as_ref().is_some_and(|b| b.id == id) {
            state.boot = None;
            self.save_state(&state)?;
        }
        self.log(&format!("clean exit {id}"))
    }
    fn set_status(&self, id: &str, status: Status, reason: Option<String>) -> Result<()> {
        if id == FACTORY {
            return Ok(());
        }
        let mut m = self.meta(id).ok_or_else(|| invalid("missing/corrupt metadata"))?;
        m.status = status;
        m.reason = reason;
        write_json(&self.version_dir(id)?.join("meta.json"), &m)
    }
    pub fn mark_bad(&self, id: &str, reason: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.set_status(id, Status::Bad, Some(reason.chars().take(4096).collect()))
    }
    pub fn begin_boot(&self, id: &str, at: u64) -> Result<()> {
        let _lock = self.lock()?;
        let mut s = self.state();
        let old = s.boot.filter(|b| s.pending_health && b.id == id && at.saturating_sub(b.started_at) <= BOOT_WINDOW);
        s.boot = Some(Boot {
            id: id.into(),
            started_at: old.as_ref().map_or(at, |b| b.started_at),
            attempts: old.map_or(1, |b| b.attempts.saturating_add(1)),
        });
        s.pending_health = true;
        self.save_state(&s)?;
        self.log(&format!("boot {id}"))
    }
    pub fn failed(&self, id: &str, reason: &str) -> Result<()> {
        let reason: String = reason.chars().take(4096).collect();
        let _lock = self.lock()?;
        let mut s = self.state();
        if id != FACTORY
            && let Some(mut m) = self.meta(id)
        {
            m.failures =
                if now().saturating_sub(m.last_failure) <= BOOT_WINDOW { m.failures.saturating_add(1) } else { 1 };
            m.last_failure = now();
            m.reason = Some(reason.clone());
            if m.failures >= MAX_ATTEMPTS {
                m.status = Status::Bad;
            }
            write_json(&self.version_dir(id)?.join("meta.json"), &m)?;
        }
        if s.last_good.as_deref() == Some(id) {
            s.last_good =
                s.previous.clone().filter(|previous| self.meta(previous).is_some_and(|m| m.status == Status::Good));
        }
        // Pending failed boot remains recorded, so a subsequent launch cannot retry it silently.
        s.pending_health = true;
        self.save_state(&s)?;
        if id != FACTORY {
            self.restore_state_backup_unlocked(id)?;
        }
        self.log(&format!("rolled back {id}: {reason}"))?;
        atomic_write(
            &self.base.join("rollback-notice.txt"),
            format!("Rolled back {id}: {reason}\n{}", self.log_tail()).as_bytes(),
        )
    }
    pub fn rollback(&self) -> Result<Choice> {
        let _lock = self.lock()?;
        let mut b = self.boot_state();
        let old = b.state.current.clone();
        if let Some(id) = &old {
            b.bad.insert(id.clone());
        }
        let mut choice = choose(&b, now());
        if choice.id == FACTORY
            && let Some(id) = &b.state.previous
            && b.available.contains(id)
            && !b.bad.contains(id)
            && self.meta(id).is_some_and(|m| m.status == Status::Good)
        {
            choice = Choice { id: id.clone(), why: "previous".into() };
        }
        if let Some(id) = &old {
            self.restore_state_backup_unlocked(id)?;
        }
        b.state.current = (choice.id != FACTORY).then(|| choice.id.clone());
        b.state.last_good = (choice.id != FACTORY && self.meta(&choice.id).is_some_and(|m| m.status == Status::Good))
            .then(|| choice.id.clone());
        b.state.previous = old;
        b.state.pending_health = false;
        b.state.boot = None;
        self.save_state(&b.state)?;
        self.log(&format!("manual rollback: {}", choice.id))?;
        atomic_write(
            &self.base.join("rollback-notice.txt"),
            format!("Rolled back manually to {}.\n{}", choice.id, self.log_tail()).as_bytes(),
        )?;
        Ok(choice)
    }
    pub fn running_lease(&self, id: &str) -> Result<Lock> {
        if id != FACTORY {
            check_id(id)?;
        }
        Lock::acquire(&self.base.join("running").join(format!("{id}.lock")))
    }
    pub fn app_lease(&self, id: &str) -> Result<Lock> {
        if id != FACTORY {
            check_id(id)?;
        }
        Lock::shared(&self.base.join("running").join(format!("{id}.lock")))
    }
    pub fn gc(&self, keep: usize) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let s = self.state();
        let list = self.list();
        let mut protected: BTreeSet<_> = list.iter().take(keep).map(|m| m.id.clone()).collect();
        protected.extend([s.current, s.last_good, s.previous].into_iter().flatten());
        let mut removed = Vec::new();
        for m in list {
            if protected.contains(&m.id) {
                continue;
            }
            let Ok(_lease) = self.running_lease(&m.id) else {
                continue;
            };
            fs::remove_dir_all(self.version_dir(&m.id)?)?;
            removed.push(m.id);
        }
        Ok(removed)
    }
    pub fn log(&self, message: &str) -> Result<()> {
        fs::create_dir_all(&self.base)?;
        let _lock = Lock::blocking(&self.base.join("log.lock"))?;
        let path = self.base.join("launcher.log");
        if fs::metadata(&path).is_ok_and(|m| m.len() >= LOG_LIMIT) {
            fs::rename(&path, self.base.join("launcher.log.1"))?;
        }
        let mut f = OpenOptions::new().append(true).create(true).open(path)?;
        writeln!(f, "{} {}", now(), message.chars().take(8192).collect::<String>())
    }
    pub fn log_tail(&self) -> String {
        use std::io::{Seek, SeekFrom};
        let Ok(mut f) = File::open(self.base.join("launcher.log")) else {
            return String::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(8192)));
        let mut bytes = Vec::new();
        let _ = f.take(8192).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
    pub fn take_notice(&self) -> Result<Option<String>> {
        let _lock = self.lock()?;
        let path = self.base.join("rollback-notice.txt");
        let Ok(f) = File::open(&path) else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        f.take(16384).read_to_end(&mut bytes)?;
        fs::remove_file(path)?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}
fn counter(id: &str) -> u64 {
    id.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Store, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("source");
        fs::create_dir(&payload).unwrap();
        fs::write(payload.join("codecraft"), "fake executable").unwrap();
        let store = Store::new(dir.path().join("home with spaces"), payload.clone());
        (dir, store, payload)
    }
    fn stage(s: &Store, p: &Path) -> String {
        s.stage(p, "1.2.3", "abcdef1234567890", "test").unwrap()
    }
    #[test]
    fn hostile_ids() {
        for id in [
            "",
            "..",
            "../1.0.0-abcd-1",
            "/1.0.0-abcd-1",
            "1.0.0-abcd-1/",
            "1.0.0-abcd-18446744073709551616",
            "1.0.0-❤-1",
            "1-abcd-2",
            "1.2.3-zzzz-3",
            "1.0.0-abcd-+1",
            "1.2.3-abcd--2",
            "1.2.3--abcd-1",
            "01.2.3-abcd-1",
            "1.2.3+-abcd-1",
            "1.2.3-a..b-abcd-1",
        ] {
            assert!(!valid_id(id), "{id}");
        }
        for id in ["1.2.3-abcd-1", "0.3.0-beta.1+build-deadbeef-2"] {
            assert!(valid_id(id), "{id}");
        }
    }
    #[test]
    fn pure_selection_table() {
        let a = "1.0.0-abcd-1".to_string();
        let b = "1.0.0-abcd-2".to_string();
        let initial = BootState {
            state: State { current: Some(a.clone()), last_good: Some(b.clone()), ..Default::default() },
            available: [a.clone(), b.clone()].into(),
            bad: BTreeSet::new(),
        };
        for (bad, pending, available, expected) in [
            (false, false, true, a.as_str()),
            (true, false, true, b.as_str()),
            (false, true, true, b.as_str()),
            (false, false, false, FACTORY),
        ] {
            let mut boot = initial.clone();
            if bad {
                boot.bad.insert(a.clone());
            }
            if pending {
                boot.state.boot = Some(Boot { id: a.clone(), started_at: 100, attempts: 1 });
                boot.state.pending_health = true;
            }
            if !available {
                boot.available.clear();
            }
            assert_eq!(choose(&boot, 110).id, expected);
        }
    }
    #[test]
    fn promotion_failures_reenable_and_gc_leases() {
        let (_dir, s, p) = fixture();
        let good = stage(&s, &p);
        s.promote(&good).unwrap();
        s.mark_good(&good).unwrap();
        let bad = stage(&s, &p);
        s.promote(&bad).unwrap();
        assert_eq!(s.state().previous.as_deref(), Some(good.as_str()));
        s.begin_boot(&bad, now()).unwrap();
        s.failed(&bad, "crash").unwrap();
        assert_eq!(choose(&s.boot_state(), now()).id, good);
        s.begin_boot(&bad, now()).unwrap();
        s.failed(&bad, "crash again").unwrap();
        assert_eq!(s.meta(&bad).unwrap().status, Status::Bad);
        assert_eq!(choose(&s.boot_state(), now()).id, good);
        s.use_version(&bad).unwrap();
        assert_eq!(choose(&s.boot_state(), now()).id, bad);
        let running = stage(&s, &p);
        let _lease = s.app_lease(&running).unwrap();
        let old = stage(&s, &p);
        let newest = stage(&s, &p);
        let removed = s.gc(1).unwrap();
        assert!(removed.contains(&old));
        for id in [&good, &bad, &running, &newest] {
            assert!(s.meta(id).is_some());
        }
        assert!(s.factory.is_dir());
    }
    #[test]
    fn corrupt_json_and_interrupted_atomic_writes() {
        let (_dir, s, p) = fixture();
        let id = stage(&s, &p);
        s.promote(&id).unwrap();
        fs::write(s.base.join(".update-interrupted"), "{broken").unwrap();
        assert_eq!(s.state().current.as_deref(), Some(id.as_str()));
        for bytes in ["", "{", "garbage", "{\"schema\":99}", "{\"current\":\"../../oops\"}"] {
            fs::write(s.base.join("state.json"), bytes).unwrap();
            assert!(s.state().current.is_none());
            assert_eq!(choose(&s.boot_state(), now()).id, FACTORY);
        }
        s.promote(&id).unwrap();
        for bytes in ["", "{", "garbage"] {
            fs::write(s.version_dir(&id).unwrap().join("meta.json"), bytes).unwrap();
            assert!(s.meta(&id).is_none());
            assert_eq!(choose(&s.boot_state(), now()).id, FACTORY);
        }
        assert!(s.executable(FACTORY).unwrap().is_file());
        fs::write(s.base.join("state.json"), "{\"schema\":1,\"new_field\":42}").unwrap();
        assert_eq!(s.state().schema, 1);
    }
    #[test]
    fn backup_first_launch_roundtrip_caps_and_exclusions() {
        let (_dir, s, p) = fixture();
        let id = stage(&s, &p);
        fs::write(s.base.join("settings.json"), "original").unwrap();
        fs::create_dir_all(s.base.join("sessions")).unwrap();
        File::create(s.base.join("sessions/large")).unwrap().set_len(5 * 1024 * 1024 + 1).unwrap();
        fs::create_dir_all(s.base.join("worktrees")).unwrap();
        fs::write(s.base.join("worktrees/secret"), "skip").unwrap();
        s.snapshot_state(&id).unwrap();
        fs::write(s.base.join("settings.json"), "changed").unwrap();
        s.snapshot_state(&id).unwrap();
        s.restore_state_backup(&id).unwrap();
        assert_eq!(fs::read_to_string(s.base.join("settings.json")).unwrap(), "original");
        let backup = s.version_dir(&id).unwrap().join("state-backup");
        assert!(!backup.join("worktrees").exists());
        assert!(!backup.join("sessions/large").exists());
        let next = stage(&s, &p);
        fs::create_dir_all(s.base.join("settings")).unwrap();
        for i in 0..6 {
            File::create(s.base.join(format!("settings/{i}"))).unwrap().set_len(5 * 1024 * 1024).unwrap();
        }
        assert!(s.snapshot_state(&next).is_err());
        assert!(!s.version_dir(&next).unwrap().join("state-backup/complete").exists());
    }
    #[test]
    fn counter_survives_gc_and_absent_config_is_restored() {
        let (_dir, s, p) = fixture();
        let first = stage(&s, &p);
        s.snapshot_state(&first).unwrap();
        fs::write(s.base.join("settings.json"), "bad newly created config").unwrap();
        s.restore_state_backup(&first).unwrap();
        assert!(!s.base.join("settings.json").exists());
        assert_eq!(s.gc(0).unwrap(), vec![first.clone()]);
        let second = stage(&s, &p);
        assert!(counter(&second) > counter(&first));
    }

    #[test]
    fn lock_releases_stale_file_and_log_rotates() {
        let (_dir, s, _p) = fixture();
        let lock = s.launcher_lock().unwrap();
        assert!(s.launcher_lock().is_err());
        drop(lock);
        assert!(s.launcher_lock().is_ok());
        fs::write(s.base.join("launcher.log"), vec![b'x'; LOG_LIMIT as usize]).unwrap();
        s.log("decision").unwrap();
        assert!(s.base.join("launcher.log.1").is_file());
        assert!(s.log_tail().contains("decision"));
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;
        let (dir, mut s, p) = fixture();
        s.base = dir.path().join(std::ffi::OsString::from_vec(b"nonutf8-\xff space".to_vec()));
        if let Err(e) = fs::create_dir_all(&s.base) {
            // EILSEQ: APFS (92 on macOS) and some Linux filesystems (84) refuse non-UTF-8 names.
            if e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(84 | 92)) {
                eprintln!("non-UTF8 filesystem fixture unavailable on this filesystem or sandbox: {e}");
                return;
            }
            panic!("non-UTF8 fixture: {e}");
        }
        let id = stage(&s, &p);
        s.promote(&id).unwrap();
        assert!(s.status_json().is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_restore_guard() {
        use std::os::unix::fs::symlink;
        let (dir, s, p) = fixture();
        let id = stage(&s, &p);
        fs::write(s.base.join("settings.json"), "safe").unwrap();
        s.snapshot_state(&id).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "untouched").unwrap();
        fs::remove_file(s.base.join("settings.json")).unwrap();
        symlink(&outside, s.base.join("settings.json")).unwrap();
        assert!(s.restore_state_backup(&id).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
        assert!(s.status_json().is_ok());
    }
}
