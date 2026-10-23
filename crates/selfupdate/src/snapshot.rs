//! Bounded, symlink-free copies. Configuration snapshots never traverse arbitrary home trees.
use crate::{Result, Store, atomic_write, invalid};
use std::fs;
use std::path::Path;
const FILE_CAP: u64 = 5 * 1024 * 1024;
const TOTAL_CAP: u64 = 25 * 1024 * 1024;
const ENTRY_CAP: usize = 10000;
const CONFIG: &[&str] = &[
    "settings.json",
    "settings",
    "keymaps",
    "keymap.json",
    "ssh.json",
    "ssh-servers.json",
    "remote-servers.json",
    "sessions",
    "session.json",
    "unsaved",
    "layouts.json",
    "file-templates",
    "scratches",
];

pub(super) fn copy_payload(src: &Path, dst: &Path) -> Result<u64> {
    let mut budget = Budget { bytes: 0, entries: 0, max: 8 * 1024 * 1024 * 1024 };
    copy(src, dst, &mut budget, false, 0)?;
    Ok(budget.bytes)
}
struct Budget {
    bytes: u64,
    entries: usize,
    max: u64,
}
fn copy(src: &Path, dst: &Path, budget: &mut Budget, config: bool, depth: usize) -> Result<()> {
    if depth > 32 {
        return Err(invalid("copy nesting limit"));
    }
    if config && dst.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(invalid("symlink in copy destination"));
    }
    let meta = fs::symlink_metadata(src)?;
    budget.entries = budget.entries.saturating_add(1);
    if budget.entries > ENTRY_CAP {
        return Err(invalid("copy entry limit"));
    }
    if meta.file_type().is_symlink() {
        return if config { Ok(()) } else { Err(invalid("payload symlinks are unsupported")) };
    }
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            if config && ["worktrees", "caches", "cache", "target", ".git"].iter().any(|n| entry.file_name() == *n) {
                continue;
            }
            copy(&entry.path(), &dst.join(entry.file_name()), budget, config, depth.saturating_add(1))?;
        }
    } else if meta.is_file() {
        if config && meta.len() > FILE_CAP {
            return Ok(());
        }
        budget.bytes = budget.bytes.checked_add(meta.len()).ok_or_else(|| invalid("copy size overflow"))?;
        if budget.bytes > budget.max {
            return Err(invalid("copy size limit"));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if config {
            use std::io::Read;
            let mut bytes = Vec::new();
            fs::File::open(src)?.take(FILE_CAP + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > FILE_CAP {
                return Err(invalid("configuration grew during snapshot"));
            }
            // Atomic config saves may replace a file between metadata and open. Charge the
            // actual bytes read, so concurrent saves cannot bypass the total snapshot cap.
            budget.bytes = budget
                .bytes
                .saturating_sub(meta.len())
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| invalid("snapshot size overflow"))?;
            if budget.bytes > budget.max {
                return Err(invalid("copy size limit"));
            }
            atomic_write(dst, &bytes)?;
        } else {
            fs::copy(src, dst)?;
        }
    }
    Ok(())
}
impl Store {
    /// First-launch snapshot is published only once and only after a complete bounded copy.
    pub fn snapshot_state(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let dir = self.version_dir(id)?;
        let backup = dir.join("state-backup");
        if backup.join("complete").is_file() {
            return Ok(());
        }
        let temp = dir.join("state-backup.pending");
        if temp.exists() {
            fs::remove_dir_all(&temp)?;
        }
        fs::create_dir_all(&temp)?;
        let mut budget = Budget { bytes: 0, entries: 0, max: TOTAL_CAP };
        for name in CONFIG {
            let src = self.base.join(name);
            match src.symlink_metadata() {
                Ok(_) => copy(&src, &temp.join(name), &mut budget, true, 0)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    atomic_write(&temp.join("absent").join(name), b"1")?
                }
                Err(e) => return Err(e),
            }
        }
        atomic_write(&temp.join("complete"), b"1")?;
        if backup.exists() {
            return Err(invalid("incomplete existing backup"));
        }
        fs::rename(&temp, &backup)?;
        #[cfg(unix)]
        fs::File::open(&dir)?.sync_all()?;
        Ok(())
    }
    pub fn restore_state_backup(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.restore_state_backup_unlocked(id)
    }
    pub(super) fn restore_state_backup_unlocked(&self, id: &str) -> Result<()> {
        let backup = self.version_dir(id)?.join("state-backup");
        if !backup.join("complete").is_file() {
            return Ok(());
        }
        let mut budget = Budget { bytes: 0, entries: 0, max: TOTAL_CAP };
        for name in CONFIG {
            let src = backup.join(name);
            if backup.join("absent").join(name).is_file() {
                let target = self.base.join(name);
                if target.symlink_metadata().is_ok() {
                    let quarantine = backup.join(format!(
                        "quarantine-{}-{}",
                        crate::now(),
                        crate::SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ));
                    fs::create_dir(&quarantine)?;
                    fs::rename(&target, quarantine.join(name))?;
                }
            } else if src.symlink_metadata().is_ok() {
                copy(&src, &self.base.join(name), &mut budget, true, 0)?;
            }
        }
        Ok(())
    }
}
