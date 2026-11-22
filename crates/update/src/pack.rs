use crate::{Result, invalid};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

const MAX_ENTRIES: usize = 50_000;
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Archive `dir`'s contents (not `dir` itself) as a `.tar.gz` at `out`, in sorted order and
/// without owner or time information, so the same tree packs to the same bytes.
pub fn pack(dir: &Path, out: &Path) -> Result<()> {
    let gz = flate2::write::GzEncoder::new(File::create(out)?, flate2::Compression::best());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    add(&mut tar, dir, Path::new(""))?;
    tar.into_inner()?.finish()?;
    Ok(())
}

fn add(tar: &mut tar::Builder<flate2::write::GzEncoder<File>>, root: &Path, rel: &Path) -> Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(root.join(rel))?.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let rel = rel.join(&name);
        let path = root.join(&rel);
        let meta = std::fs::symlink_metadata(&path)?;
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        if meta.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            tar.append_data(&mut header, &rel, std::io::empty())?;
            add(tar, root, &rel)?;
        } else if meta.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(mode_of(&meta));
            header.set_size(meta.len());
            tar.append_data(&mut header, &rel, File::open(&path)?)?;
        } else {
            return Err(invalid(format!("{} is not a plain file or folder; payloads hold no links", rel.display())));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 }
}
#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0o755
}

/// Unpack a `.tar.gz` into `dest` (created). Only plain files and folders with relative,
/// `..`-free paths are accepted; links and special files, too many entries or too many bytes
/// fail the whole unpack, leaving `dest` for the caller to delete.
pub fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let gz = flate2::read::GzDecoder::new(File::open(archive)?).take(MAX_BYTES);
    let mut tar = tar::Archive::new(gz);
    tar.set_preserve_permissions(true);
    tar.set_unpack_xattrs(false);
    let (mut entries, mut bytes) = (0usize, 0u64);
    for entry in tar.entries()? {
        let mut entry = entry?;
        entries += 1;
        bytes = bytes.saturating_add(entry.size());
        if entries > MAX_ENTRIES || bytes > MAX_BYTES {
            return Err(invalid("payload is too large"));
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(invalid("payload contains a link or special file"));
        }
        let path = entry.path()?.into_owned();
        if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
            return Err(invalid(format!("payload path {} is not relative", path.display())));
        }
        if !entry.unpack_in(dest)? {
            return Err(invalid(format!("payload path {} escapes the folder", path.display())));
        }
    }
    Ok(())
}
