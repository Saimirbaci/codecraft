//! Safe text file I/O: binary detection, size caps, BOM handling, atomic writes.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{FsError, Result};

/// A NUL byte in this many leading bytes marks a file as binary.
pub(crate) const BINARY_SNIFF_BYTES: u64 = 8 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

fn io_error(path: &Path, source: std::io::Error) -> FsError {
    FsError::Io { path: path.to_path_buf(), source }
}

pub(crate) fn read_text_file_limited(path: &Path, limit: u64) -> Result<String> {
    let bytes = read_bytes_limited(path, limit)?;
    decode(path, bytes)
}

/// Read a regular file of at most `limit` bytes that has no NUL in its first 8 KiB.
pub(crate) fn read_bytes_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    // Stat first: opening a FIFO would block, and a device could be endless.
    let meta = fs::metadata(path).map_err(|e| io_error(path, e))?;
    if meta.is_dir() {
        return Err(io_error(path, std::io::Error::new(ErrorKind::IsADirectory, "is a directory")));
    }
    if !meta.is_file() {
        return Err(io_error(path, std::io::Error::new(ErrorKind::InvalidInput, "not a regular file")));
    }
    if meta.len() > limit {
        return Err(FsError::TooLarge { path: path.to_path_buf(), size: meta.len(), limit });
    }
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    let sniff = BINARY_SNIFF_BYTES.min(limit.saturating_add(1));
    (&mut file).take(sniff).read_to_end(&mut bytes).map_err(|e| io_error(path, e))?;
    if bytes.contains(&0) {
        return Err(FsError::Binary(path.to_path_buf()));
    }
    // The file may have grown since the stat: read at most one byte past the limit to notice.
    let rest = limit.saturating_add(1).saturating_sub(bytes.len() as u64);
    file.take(rest).read_to_end(&mut bytes).map_err(|e| io_error(path, e))?;
    let size = bytes.len() as u64;
    if size > limit {
        return Err(FsError::TooLarge { path: path.to_path_buf(), size, limit });
    }
    Ok(bytes)
}

/// UTF-8 decode, dropping a leading byte order mark.
pub(crate) fn decode(path: &Path, mut bytes: Vec<u8>) -> Result<String> {
    if bytes.starts_with(UTF8_BOM) {
        bytes.drain(..UTF8_BOM.len());
    }
    String::from_utf8(bytes).map_err(|_| FsError::NotUtf8(path.to_path_buf()))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    // Write through a symlink to its target so the link itself survives the rename.
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path).map_err(|e| io_error(path, e))?,
        _ => path.to_path_buf(),
    };
    let existing = match fs::metadata(&target) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(io_error(&target, e)),
    };
    if let Some(meta) = &existing {
        if meta.is_dir() {
            return Err(io_error(&target, std::io::Error::new(ErrorKind::IsADirectory, "is a directory")));
        }
        if meta.permissions().readonly() {
            return Err(io_error(&target, std::io::Error::new(ErrorKind::PermissionDenied, "file is read-only")));
        }
    }
    let name = target.file_name().ok_or_else(|| FsError::Other(format!("{}: not a file path", path.display())))?;
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let (tmp_path, file) = create_temp(&dir, &name.to_string_lossy())?;
    let written = write_and_sync(file, contents, existing.as_ref()).and_then(|()| fs::rename(&tmp_path, &target));
    if let Err(source) = written {
        let _ = fs::remove_file(&tmp_path);
        return Err(FsError::Io { path: target, source });
    }
    sync_dir(&dir);
    Ok(())
}

/// Create `.{name}.codecraft-tmp-{pid}-{n}` exclusively in `dir`.
fn create_temp(dir: &Path, name: &str) -> Result<(PathBuf, File)> {
    // Keep the temp name well under NAME_MAX whatever the original length.
    let short: String = name.chars().take(64).collect();
    let pid = std::process::id();
    for _ in 0..64 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = dir.join(OsString::from(format!(".{short}.codecraft-tmp-{pid}-{n}")));
        match OpenOptions::new().write(true).create_new(true).open(&tmp_path) {
            Ok(file) => return Ok((tmp_path, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error(&tmp_path, e)),
        }
    }
    Err(FsError::Other(format!("{}: could not create a temporary file", dir.display())))
}

/// Write, copy the original file's permissions (and owner, best effort), fsync, close.
fn write_and_sync(mut file: File, contents: &[u8], original: Option<&fs::Metadata>) -> std::io::Result<()> {
    file.write_all(contents)?;
    if let Some(meta) = original {
        file.set_permissions(meta.permissions())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Only succeeds for root or when nothing changes; a new owner is not an error.
            let _ = std::os::unix::fs::fchown(&file, Some(meta.uid()), Some(meta.gid()));
        }
    }
    file.sync_all()
}

/// Persist the rename itself (best effort; not supported everywhere).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{read_text_file, write_text_file_atomic};

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("codecraft-tmp"))
            .collect()
    }

    #[test]
    fn reads_text_and_strips_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        fs::write(&path, b"\xEF\xBB\xBFfn main() {}\n").unwrap();
        assert_eq!(read_text_file(&path).unwrap(), "fn main() {}\n");
        fs::write(&path, "héllo").unwrap();
        assert_eq!(read_text_file(&path).unwrap(), "héllo");
        fs::write(&path, "").unwrap();
        assert_eq!(read_text_file(&path).unwrap(), "");
        fs::write(&path, b"\xEF\xBB\xBF").unwrap();
        assert_eq!(read_text_file(&path).unwrap(), "");
    }

    #[test]
    fn rejects_binary_invalid_utf8_and_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("a.bin");
        fs::write(&bin, b"ELF\0\x01\x02").unwrap();
        assert!(matches!(read_text_file(&bin), Err(FsError::Binary(_))));

        // A NUL after the first 8 KiB is not sniffed: the file is decoded (and NUL is valid UTF-8).
        let late_nul = dir.path().join("late.txt");
        let mut bytes = vec![b'a'; 9000];
        bytes.push(0);
        fs::write(&late_nul, &bytes).unwrap();
        assert_eq!(read_text_file(&late_nul).unwrap().len(), 9001);

        let latin1 = dir.path().join("latin1.txt");
        fs::write(&latin1, b"caf\xE9").unwrap();
        assert!(matches!(read_text_file(&latin1), Err(FsError::NotUtf8(_))));

        let big = dir.path().join("big.txt");
        fs::write(&big, "x".repeat(101)).unwrap();
        match read_text_file_limited(&big, 100) {
            Err(FsError::TooLarge { size, limit, .. }) => assert_eq!((size, limit), (101, 100)),
            other => panic!("{other:?}"),
        }
        assert_eq!(read_text_file_limited(&big, 101).unwrap().len(), 101);
        assert!(matches!(read_text_file_limited(&big, 0), Err(FsError::TooLarge { .. })));
    }

    #[test]
    fn read_errors_for_missing_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(read_text_file(&dir.path().join("missing")), Err(FsError::Io { .. })));
        match read_text_file(dir.path()) {
            Err(FsError::Io { source, .. }) => assert_eq!(source.kind(), ErrorKind::IsADirectory),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.rs");
        write_text_file_atomic(&path, "fn a() {}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn a() {}");
        write_text_file_atomic(&path, "fn b() {}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn b() {}");
        write_text_file_atomic(&path, "").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        let long_name = dir.path().join(format!("{}.rs", "n".repeat(200)));
        write_text_file_atomic(&long_name, "x").unwrap();
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_failures_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let missing_dir = dir.path().join("no/such/dir/file.rs");
        assert!(matches!(write_text_file_atomic(&missing_dir, "x"), Err(FsError::Io { .. })));
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        match write_text_file_atomic(&sub, "x") {
            Err(FsError::Io { source, .. }) => assert_eq!(source.kind(), ErrorKind::IsADirectory),
            other => panic!("{other:?}"),
        }
        assert!(write_text_file_atomic(Path::new("/"), "x").is_err());
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_refuses_read_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.txt");
        fs::write(&path, "original").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).unwrap();
        match write_text_file_atomic(&path, "new") {
            Err(FsError::Io { source, .. }) => assert_eq!(source.kind(), ErrorKind::PermissionDenied),
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        write_text_file_atomic(&path, "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o750);
        assert_eq!(fs::read_to_string(&path).unwrap(), "#!/bin/sh\necho hi\n");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rs");
        let link = dir.path().join("link.rs");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_text_file_atomic(&link, "new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_files_are_refused() {
        assert!(matches!(read_text_file(Path::new("/dev/null")), Err(FsError::Io { .. })));
    }
}
