//! Settings › Editor › Font: the system's font families (Font dropdown) and the editor font's
//! data for the UI.
//!
//! Font directories are scanned on a worker thread. Only each file's table directory and its
//! `name`, `post` and `OS/2` tables are read (seeking, with every offset and length checked), so
//! a scan stays fast and a truncated or garbage file is skipped, never fatal. Monospaced
//! families are those with `post.isFixedPitch` or a monospaced PANOSE proportion. The chosen
//! family's file is read on a worker too; the UI checks it once more before handing it to the
//! renderer and falls back to the built-in fonts when it is unusable.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};

/// Most font files looked at in one scan.
const MAX_FILES: usize = 20_000;
/// Deepest directory level scanned below a font directory.
const MAX_DEPTH: usize = 6;
/// Largest font file loaded for the editor.
pub const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;
/// Largest `name` table read.
const MAX_NAME_TABLE: usize = 1024 * 1024;

/// One face of a font file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontFace {
    pub family: String,
    pub style: String,
    pub path: PathBuf,
    /// Index in a collection (`.ttc`); 0 otherwise.
    pub index: u32,
    pub monospace: bool,
    pub weight: u16,
    pub italic: bool,
}

/// A font family as listed in Settings: its regular face.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontFamily {
    pub name: String,
    pub monospace: bool,
    pub path: PathBuf,
    pub index: u32,
}

/// The editor font's file, read for the UI.
#[derive(Clone)]
pub struct LoadedFont {
    pub family: String,
    pub path: PathBuf,
    pub index: u32,
    pub data: Arc<Vec<u8>>,
}

impl std::fmt::Debug for LoadedFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedFont")
            .field("family", &self.family)
            .field("path", &self.path)
            .field("index", &self.index)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Font listing and loading state (one per session).
#[derive(Debug, Default)]
pub struct FontsState {
    /// The families found by the last scan; `None` before the first.
    pub families: Option<Vec<FontFamily>>,
    pub scanning: bool,
    /// Directories scanned instead of the platform's font directories (tests, embedders).
    pub dirs_override: Option<Vec<PathBuf>>,
    /// The editor font family's data (none: the built-in fonts).
    pub loaded: Option<LoadedFont>,
    /// Why the editor font could not be used.
    pub error: Option<String>,
    /// Bumped whenever `loaded` changes (the UI rebuilds its fonts).
    pub generation: u64,
    /// The family last asked for (not asked again until the setting changes).
    requested: Option<String>,
    scan_rx: Option<Receiver<Vec<FontFamily>>>,
    load_rx: Option<Receiver<Result<LoadedFont, String>>>,
}

impl FontsState {
    /// The directories a scan looks in.
    pub fn dirs(&self) -> Vec<PathBuf> {
        match &self.dirs_override {
            Some(d) => d.clone(),
            None => default_dirs(),
        }
    }

    /// Start scanning (unless a scan is running).
    pub fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        let dirs = self.dirs();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new().name("codecraft-font-scan".into()).spawn(move || {
            let _ = tx.send(scan(&dirs));
        });
        match spawned {
            Ok(_) => {
                self.scan_rx = Some(rx);
                self.scanning = true;
            }
            Err(e) => self.error = Some(format!("cannot scan fonts: {e}")),
        }
    }

    /// The family called `name` (case-insensitive) in the last scan.
    pub fn family(&self, name: &str) -> Option<&FontFamily> {
        let name = name.trim();
        self.families.as_ref()?.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }
}

/// The platform's font directories (the user's first).
pub fn default_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        out.extend(home.iter().map(|h| h.join("Library/Fonts")));
        out.push("/Library/Fonts".into());
        out.push("/System/Library/Fonts".into());
    } else if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|h| !h.is_empty()) {
            out.push(PathBuf::from(local).join("Microsoft\\Windows\\Fonts"));
        }
        let windir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
        out.push(windir.join("Fonts"));
    } else {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|h| !h.is_empty()) {
            out.push(PathBuf::from(data).join("fonts"));
        }
        out.extend(home.iter().flat_map(|h| [h.join(".local/share/fonts"), h.join(".fonts")]));
        out.push("/usr/share/fonts".into());
        out.push("/usr/local/share/fonts".into());
    }
    out
}

fn is_font_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["ttf", "otf", "ttc", "otc"].iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Scan directories for font families (blocking; runs on a worker thread). Hidden families
/// (names starting with `.`) are left out; families are sorted by name.
pub fn scan(dirs: &[PathBuf]) -> Vec<FontFamily> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = dirs.iter().map(|d| (d.clone(), 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            if files.len() >= MAX_FILES {
                break;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() && depth < MAX_DEPTH {
                stack.push((p, depth + 1));
            } else if (ft.is_file() || ft.is_symlink()) && is_font_file(&p) {
                files.push(p);
            }
        }
    }
    let faces: Vec<FontFace> = files.iter().flat_map(|p| read_faces(p)).collect();
    families(faces)
}

/// Group faces into families, each represented by its most regular face.
pub fn families(faces: Vec<FontFace>) -> Vec<FontFamily> {
    let mut by_name: std::collections::BTreeMap<String, Vec<FontFace>> = std::collections::BTreeMap::new();
    for f in faces.into_iter().filter(|f| !f.family.trim().is_empty() && !f.family.starts_with('.')) {
        by_name.entry(f.family.trim().to_string()).or_default().push(f);
    }
    by_name
        .into_iter()
        .filter_map(|(name, faces)| {
            let monospace = faces.iter().any(|f| f.monospace);
            let best = faces.iter().min_by_key(|f| {
                let regular = matches!(f.style.to_ascii_lowercase().as_str(), "regular" | "book" | "roman" | "normal");
                (f.italic, !regular, (i32::from(f.weight) - 400).unsigned_abs(), f.path.clone(), f.index)
            })?;
            Some(FontFamily { name, monospace, path: best.path.clone(), index: best.index })
        })
        .collect()
}

// ---------------------------------------------------------------- reading font files

fn be16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([*s.first()?, *s.get(1)?]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]))
}

/// Random access to a font's bytes (a file, or bytes in memory).
trait Source {
    fn len(&self) -> u64;
    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>>;
}

struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if offset.checked_add(len as u64)? > self.len {
            return None;
        }
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; len];
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

struct BytesSource<'a>(&'a [u8]);

impl Source for BytesSource<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(offset).ok()?;
        self.0.get(start..start.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

/// The faces of a font file (none for unreadable or malformed files).
pub fn read_faces(path: &Path) -> Vec<FontFace> {
    let Ok(file) = std::fs::File::open(path) else { return Vec::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut src = FileSource { file, len };
    faces_in(&mut src, path)
}

/// The faces in font bytes (for checking a loaded file).
pub fn faces_in_bytes(bytes: &[u8], path: &Path) -> Vec<FontFace> {
    faces_in(&mut BytesSource(bytes), path)
}

fn faces_in(src: &mut dyn Source, path: &Path) -> Vec<FontFace> {
    let Some(head) = src.read(0, 12) else { return Vec::new() };
    if head.get(..4) == Some(b"ttcf") {
        let n = be32(&head, 8).unwrap_or(0).min(64);
        let Some(offsets) = src.read(12, n as usize * 4) else { return Vec::new() };
        return (0..n)
            .filter_map(|i| {
                let off = be32(&offsets, i as usize * 4)?;
                face_at(src, u64::from(off), path, i)
            })
            .collect();
    }
    face_at(src, 0, path, 0).into_iter().collect()
}

/// The face whose table directory starts at `offset`.
fn face_at(src: &mut dyn Source, offset: u64, path: &Path, index: u32) -> Option<FontFace> {
    let head = src.read(offset, 12)?;
    let version = be32(&head, 0)?;
    if !matches!(version, 0x0001_0000 | 0x4F54_544F /* OTTO */ | 0x7472_7565 /* true */) {
        return None;
    }
    let n = usize::from(be16(&head, 4)?).min(512);
    let records = src.read(offset.checked_add(12)?, n.checked_mul(16)?)?;
    let mut table = |tag: &[u8; 4], cap: usize| -> Option<Vec<u8>> {
        let i = (0..n).find(|i| records.get(i * 16..i * 16 + 4) == Some(&tag[..]))?;
        let off = be32(&records, i * 16 + 8)?;
        let len = (be32(&records, i * 16 + 12)? as usize).min(cap);
        if u64::from(off).checked_add(len as u64)? > src.len() {
            return None;
        }
        src.read(u64::from(off), len)
    };
    let name = table(b"name", MAX_NAME_TABLE)?;
    let post = table(b"post", 64);
    let os2 = table(b"OS/2", 128);
    let family = name_string(&name, &[16, 1])?;
    let style = name_string(&name, &[17, 2]).unwrap_or_else(|| "Regular".into());
    let fixed = post.as_deref().and_then(|p| be32(p, 12)).is_some_and(|v| v != 0);
    let panose_mono = os2.as_deref().is_some_and(|o| o.get(32) == Some(&2) && o.get(35) == Some(&9));
    let weight = os2.as_deref().and_then(|o| be16(o, 4)).unwrap_or(400);
    let italic = os2.as_deref().and_then(|o| be16(o, 62)).is_some_and(|s| s & 1 != 0)
        || style.to_ascii_lowercase().contains("italic")
        || style.to_ascii_lowercase().contains("oblique");
    Some(FontFace {
        family: clean(&family),
        style: clean(&style),
        path: path.to_path_buf(),
        index,
        monospace: fixed || panose_mono,
        weight,
        italic,
    })
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(120).collect()
}

/// The best string for the first name id in `ids` that the table has: Windows English, any
/// Windows, Unicode, then Mac Roman English.
fn name_string(t: &[u8], ids: &[u16]) -> Option<String> {
    let count = usize::from(be16(t, 2)?).min(4096);
    let storage = usize::from(be16(t, 4)?);
    for id in ids {
        let mut best: Option<(u8, String)> = None;
        for i in 0..count {
            let r = 6 + i * 12;
            let (platform, encoding, lang, name_id) = (be16(t, r)?, be16(t, r + 2)?, be16(t, r + 4)?, be16(t, r + 6)?);
            if name_id != *id {
                continue;
            }
            let len = usize::from(be16(t, r + 8)?);
            let off = storage.checked_add(usize::from(be16(t, r + 10)?))?;
            let Some(bytes) = t.get(off..off.checked_add(len)?) else { continue };
            let (rank, text) = match (platform, encoding) {
                (3, 0 | 1 | 10) => (if lang == 0x0409 { 0 } else { 1 }, utf16be(bytes)),
                (0, _) => (2, utf16be(bytes)),
                (1, 0) if lang == 0 => (3, bytes.iter().map(|b| char::from(*b)).collect()),
                _ => continue,
            };
            if text.trim().is_empty() {
                continue;
            }
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, text));
            }
        }
        if let Some((_, s)) = best {
            return Some(s);
        }
    }
    None
}

fn utf16be(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
    String::from_utf16_lossy(&units)
}

/// Read a font file for the editor: at most [`MAX_FONT_BYTES`], and it must hold a face at
/// `index` whose names can be read.
pub fn load(family: &str, path: &Path, index: u32) -> Result<LoadedFont, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_FONT_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if !faces_in_bytes(&data, path).iter().any(|f| f.index == index) {
        return Err(format!("{} is not a usable font file", path.display()));
    }
    Ok(LoadedFont { family: family.to_string(), path: path.to_path_buf(), index, data: Arc::new(data) })
}

/// Keep the editor font loaded for `wanted` (the `editor_font_family` setting): scan when the
/// family list is needed, read the family's file on a worker, drop it when the setting is
/// cleared. Returns true if anything changed.
pub fn pump(st: &mut FontsState, wanted: &str) -> bool {
    let mut changed = false;
    if let Some(rx) = &st.scan_rx {
        match rx.try_recv() {
            Ok(list) => {
                st.families = Some(list);
                st.scanning = false;
                st.scan_rx = None;
                changed = true;
            }
            Err(TryRecvError::Disconnected) => {
                st.families.get_or_insert_with(Vec::new);
                st.scanning = false;
                st.scan_rx = None;
                changed = true;
            }
            Err(TryRecvError::Empty) => {}
        }
    }
    if let Some(rx) = &st.load_rx {
        match rx.try_recv() {
            Ok(Ok(font)) => {
                st.loaded = Some(font);
                st.error = None;
                st.generation = st.generation.wrapping_add(1);
                st.load_rx = None;
                changed = true;
            }
            Ok(Err(e)) => {
                st.error = Some(e);
                st.load_rx = None;
                changed = true;
            }
            Err(TryRecvError::Disconnected) => {
                st.load_rx = None;
                changed = true;
            }
            Err(TryRecvError::Empty) => {}
        }
    }
    let wanted = wanted.trim();
    if wanted.is_empty() {
        st.requested = None;
        if st.loaded.take().is_some() || st.error.take().is_some() {
            st.generation = st.generation.wrapping_add(1);
            changed = true;
        }
        return changed;
    }
    let have = st.loaded.as_ref().is_some_and(|l| l.family.eq_ignore_ascii_case(wanted));
    if have || st.load_rx.is_some() || st.requested.as_deref() == Some(wanted) {
        return changed;
    }
    if st.families.is_none() {
        st.start_scan();
        return changed;
    }
    st.requested = Some(wanted.to_string());
    let Some(f) = st.family(wanted).cloned() else {
        st.error = Some(format!("font family `{wanted}` was not found; using the default font"));
        if st.loaded.take().is_some() {
            st.generation = st.generation.wrapping_add(1);
        }
        return true;
    };
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("codecraft-font-load".into())
        .spawn(move || {
            let _ = tx.send(load(&f.name, &f.path, f.index));
        });
    match spawned {
        Ok(_) => st.load_rx = Some(rx),
        Err(e) => st.error = Some(format!("cannot load the font: {e}")),
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal sfnt with `name`, `post` and `OS/2` tables (enough for the scanner; not a
    /// renderable font).
    pub(crate) fn synthetic_font(family: &str, style: &str, mono: bool, weight: u16) -> Vec<u8> {
        let utf16 = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
        let (fam, sty) = (utf16(family), utf16(style));
        let mut name = Vec::new();
        name.extend(0u16.to_be_bytes());
        name.extend(2u16.to_be_bytes());
        name.extend((6u16 + 24).to_be_bytes());
        for (id, len, off) in [(1u16, fam.len(), 0usize), (2, sty.len(), fam.len())] {
            for v in [3u16, 1, 0x0409, id, len as u16, off as u16] {
                name.extend(v.to_be_bytes());
            }
        }
        name.extend(&fam);
        name.extend(&sty);
        let mut post = vec![0u8; 32];
        post[12..16].copy_from_slice(&u32::from(mono).to_be_bytes());
        let mut os2 = vec![0u8; 96];
        os2[4..6].copy_from_slice(&weight.to_be_bytes());
        let tables: [(&[u8; 4], Vec<u8>); 3] = [(b"OS/2", os2), (b"name", name), (b"post", post)];
        let mut out = Vec::new();
        out.extend(0x0001_0000u32.to_be_bytes());
        out.extend((tables.len() as u16).to_be_bytes());
        out.extend([0u8; 6]);
        let mut offset = 12 + 16 * tables.len();
        let mut body = Vec::new();
        for (tag, data) in &tables {
            out.extend(*tag);
            out.extend(0u32.to_be_bytes());
            out.extend((offset as u32).to_be_bytes());
            out.extend((data.len() as u32).to_be_bytes());
            body.extend(data);
            while body.len() % 4 != 0 {
                body.push(0);
            }
            offset = 12 + 16 * tables.len() + body.len();
        }
        out.extend(body);
        out
    }

    #[test]
    fn reads_names_and_monospace_flags() {
        let bytes = synthetic_font("Fixture Mono", "Regular", true, 400);
        let faces = faces_in_bytes(&bytes, Path::new("x.ttf"));
        assert_eq!(faces.len(), 1);
        assert_eq!((faces[0].family.as_str(), faces[0].style.as_str()), ("Fixture Mono", "Regular"));
        assert!(faces[0].monospace);
        let sans = faces_in_bytes(&synthetic_font("Fixture Sans", "Bold", false, 700), Path::new("y.ttf"));
        assert!(!sans[0].monospace);
        assert_eq!(sans[0].weight, 700);
    }

    #[test]
    fn garbage_and_truncated_files_are_skipped() {
        let good = synthetic_font("Fixture Mono", "Regular", true, 400);
        for len in [0, 3, 11, 12, 20, 40, good.len() / 2, good.len() - 1] {
            let _ = faces_in_bytes(&good[..len], Path::new("t.ttf"));
        }
        let mut lying = good.clone();
        // A table record pointing far outside the file.
        lying[12 + 16 + 8..12 + 16 + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(faces_in_bytes(&lying, Path::new("l.ttf")).is_empty());
        let mut ttc = b"ttcf".to_vec();
        ttc.extend([0, 1, 0, 0]);
        ttc.extend(u32::MAX.to_be_bytes());
        assert!(faces_in_bytes(&ttc, Path::new("c.ttc")).is_empty());
        assert!(faces_in_bytes(&[0xFFu8; 4096], Path::new("g.ttf")).is_empty());
    }

    #[test]
    fn scans_fixture_dirs_into_families() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("nested/deeper");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("FixtureMono-Regular.ttf"), synthetic_font("Fixture Mono", "Regular", true, 400)).unwrap();
        std::fs::write(sub.join("FixtureMono-Bold.ttf"), synthetic_font("Fixture Mono", "Bold", true, 700)).unwrap();
        std::fs::write(dir.path().join("Sans.otf"), synthetic_font("Fixture Sans", "Regular", false, 400)).unwrap();
        std::fs::write(dir.path().join("Hidden.ttf"), synthetic_font(".Hidden UI", "Regular", false, 400)).unwrap();
        std::fs::write(dir.path().join("Broken.ttf"), b"not a font at all").unwrap();
        std::fs::write(dir.path().join("readme.txt"), "x").unwrap();
        let fams = scan(&[dir.path().to_path_buf(), dir.path().join("missing")]);
        let names: Vec<&str> = fams.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["Fixture Mono", "Fixture Sans"]);
        assert!(fams[0].monospace && !fams[1].monospace);
        assert!(fams[0].path.ends_with("FixtureMono-Regular.ttf"), "the regular face represents the family");
        assert!(load("Fixture Mono", &fams[0].path, 0).is_ok());
        assert!(load("Broken", &dir.path().join("Broken.ttf"), 0).is_err());
        assert!(load("Gone", &dir.path().join("gone.ttf"), 0).is_err());
    }

    #[test]
    fn pump_scans_loads_and_reports_missing_families() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("F.ttf"), synthetic_font("Fixture Mono", "Regular", true, 400)).unwrap();
        let mut st = FontsState { dirs_override: Some(vec![dir.path().to_path_buf()]), ..Default::default() };
        let wait = |st: &mut FontsState, wanted: &str, done: &dyn Fn(&FontsState) -> bool| {
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_secs(10) {
                pump(st, wanted);
                if done(st) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        };
        assert!(wait(&mut st, "fixture mono", &|s| s.loaded.is_some()));
        let g = st.generation;
        assert!(wait(&mut st, "Nope Font", &|s| s.error.is_some()));
        assert!(st.loaded.is_none() && st.generation != g);
        assert!(wait(&mut st, "", &|s| s.error.is_none() && s.loaded.is_none()));
    }
}
