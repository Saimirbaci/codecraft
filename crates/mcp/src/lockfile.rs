//! Discovery lock files: `<lock dir>/<port>.lock`, by default `~/.claude/ide/<port>.lock`.
//!
//! Claude Code scans this directory to find IDEs it can connect to (see `PROTOCOL-NOTES.md`).
//! The file holds the auth token, so it is written with owner-only permissions where the
//! platform has them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::{McpError, Result};

/// Contents of a lock file (field names as Claude Code reads them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockInfo {
    /// Process id of the IDE that owns the server.
    pub pid: u32,
    /// Absolute folders the IDE has open; Claude Code matches its working directory against them.
    pub workspace_folders: Vec<String>,
    /// Name shown in Claude Code's `/ide` picker.
    pub ide_name: String,
    /// Always `"ws"`.
    pub transport: String,
    #[serde(default)]
    pub running_in_windows: bool,
    /// Value the client must send in the `x-claude-code-ide-authorization` header.
    pub auth_token: String,
}

/// The directory Claude Code reads lock files from: `$CLAUDE_CONFIG_DIR/ide` if that variable is
/// set, else `~/.claude/ide`. `None` if no home directory is known.
pub fn default_lock_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("ide"));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))?;
    Some(PathBuf::from(home).join(".claude").join("ide"))
}

/// Path of the lock file for `port` in `dir`.
pub fn lock_path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("{port}.lock"))
}

/// Write `info` to `<dir>/<port>.lock` (creating `dir`), replacing any existing file atomically.
pub fn write(dir: &Path, port: u16, info: &LockInfo) -> Result<PathBuf> {
    create_private_dir(dir)?;
    let path = lock_path(dir, port);
    let tmp = dir.join(format!(".{port}.lock.{}.tmp", std::process::id()));
    let json = serde_json::to_vec(info).map_err(|e| McpError::Lock(e.to_string()))?;
    let written = write_private(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(McpError::Lock(format!("cannot write {}: {e}", path.display())));
    }
    Ok(path)
}

/// Read and parse a lock file.
pub fn read(path: &Path) -> Result<LockInfo> {
    let bytes = std::fs::read(path).map_err(|e| McpError::Lock(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|e| McpError::Lock(format!("invalid lock file {}: {e}", path.display())))
}

/// Remove a lock file; a file that is already gone is fine.
pub fn remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("cannot remove lock file {}: {e}", path.display());
    }
}

/// Remove lock files written by `ide_name` whose process is gone (a crashed or killed IDE).
/// Files of other IDEs are left alone: their pid may live in another namespace (a container,
/// WSL) that shares this directory. Returns the removed paths.
pub fn cleanup_stale(dir: &Path, ide_name: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let own = std::process::id();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        let Ok(info) = read(&path) else { continue };
        if info.ide_name != ide_name || info.pid == own || pid_alive(info.pid) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            log::info!("removed stale IDE lock file {}", path.display());
            removed.push(path);
        }
    }
    removed
}

/// Whether a process with this id exists. When the check itself cannot run, the process is
/// assumed alive (never delete a lock file on a guess).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    pid_alive_os(pid)
}

#[cfg(unix)]
fn pid_alive_os(pid: u32) -> bool {
    // `kill -0` checks existence and permission without sending a signal.
    let out = Command::new("kill").arg("-0").arg(pid.to_string()).stdin(Stdio::null()).output();
    match out {
        Ok(o) if o.status.success() => true,
        // EPERM: the process exists but belongs to someone else.
        Ok(o) => String::from_utf8_lossy(&o.stderr).to_ascii_lowercase().contains("not permitted"),
        Err(_) => true,
    }
}

#[cfg(windows)]
fn pid_alive_os(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")),
        _ => true,
    }
}

#[cfg(not(any(unix, windows)))]
fn pid_alive_os(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| McpError::Lock(format!("cannot create {}: {e}", dir.display())))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| McpError::Lock(format!("cannot create {}: {e}", dir.display())))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, ide: &str) -> LockInfo {
        LockInfo {
            pid,
            workspace_folders: vec!["/tmp/project".into()],
            ide_name: ide.into(),
            transport: "ws".into(),
            running_in_windows: false,
            auth_token: "token".into(),
        }
    }

    #[test]
    fn write_read_roundtrip_with_claude_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), 4242, &info(7, "CodeCraft")).unwrap();
        assert_eq!(path, dir.path().join("4242.lock"));
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for key in ["pid", "workspaceFolders", "ideName", "transport", "runningInWindows", "authToken"] {
            assert!(raw.get(key).is_some(), "missing {key} in {raw}");
        }
        assert_eq!(read(&path).unwrap(), info(7, "CodeCraft"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "lock file must be owner-only");
        }
        remove(&path);
        assert!(!path.exists());
        remove(&path); // already gone: no error
    }

    #[cfg(unix)]
    #[test]
    fn pid_checks() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        // A child that has exited and been reaped is gone.
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn stale_cleanup_removes_only_our_dead_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let ours_dead = write(dir.path(), 1001, &info(dead, "CodeCraft")).unwrap();
        let ours_alive = write(dir.path(), 1002, &info(std::process::id(), "CodeCraft")).unwrap();
        let other_dead = write(dir.path(), 1003, &info(dead, "Visual Studio Code")).unwrap();
        let garbage = dir.path().join("1004.lock");
        std::fs::write(&garbage, b"{not json").unwrap();
        let removed = cleanup_stale(dir.path(), "CodeCraft");
        assert_eq!(removed, vec![ours_dead.clone()]);
        assert!(!ours_dead.exists());
        assert!(ours_alive.exists() && other_dead.exists() && garbage.exists());
        // A missing directory is not an error.
        assert!(cleanup_stale(&dir.path().join("missing"), "CodeCraft").is_empty());
    }
}
