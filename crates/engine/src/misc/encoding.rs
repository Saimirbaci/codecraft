//! File encodings (File › File Properties › File Encoding, Add/Remove BOM): detection when a file
//! is opened, decoding for Reload in another encoding and encoding on save. UTF-8, UTF-16 LE/BE
//! and ISO-8859-1 (Latin-1).

use std::io::Read;
use std::path::Path;

use codecraft_fs::FsError;
use codecraft_workspace::{OpenDocument, TextEncoding};

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16BE_BOM: &[u8] = &[0xFE, 0xFF];

/// How many leading bytes the UTF-16 heuristic looks at.
const SNIFF: usize = 4096;

/// The byte order mark of `enc`.
pub fn bom_bytes(enc: TextEncoding) -> &'static [u8] {
    match enc {
        TextEncoding::Utf8 => UTF8_BOM,
        TextEncoding::Utf16Le => UTF16LE_BOM,
        TextEncoding::Utf16Be => UTF16BE_BOM,
        TextEncoding::Latin1 => &[],
    }
}

/// Guess the encoding of `bytes` and whether they start with a byte order mark: a BOM decides;
/// else valid UTF-8; else UTF-16 when every other byte is mostly zero (ASCII-heavy text); else
/// Latin-1.
pub fn detect(bytes: &[u8]) -> (TextEncoding, bool) {
    if bytes.starts_with(UTF8_BOM) {
        return (TextEncoding::Utf8, true);
    }
    if bytes.starts_with(UTF16LE_BOM) {
        return (TextEncoding::Utf16Le, true);
    }
    if bytes.starts_with(UTF16BE_BOM) {
        return (TextEncoding::Utf16Be, true);
    }
    if let Some(enc) = utf16_without_bom(bytes) {
        return (enc, false);
    }
    if std::str::from_utf8(bytes).is_ok() {
        return (TextEncoding::Utf8, false);
    }
    (TextEncoding::Latin1, false)
}

fn utf16_without_bom(bytes: &[u8]) -> Option<TextEncoding> {
    let head = bytes.get(..bytes.len().min(SNIFF))?;
    if head.len() < 2 || !head.len().is_multiple_of(2) && head.len() == bytes.len() {
        return None;
    }
    let pairs = head.len() / 2;
    let (mut even_zero, mut odd_zero) = (0usize, 0usize);
    for [even, odd] in head.as_chunks::<2>().0 {
        even_zero += usize::from(*even == 0);
        odd_zero += usize::from(*odd == 0);
    }
    // Mostly-ASCII UTF-16: the high byte of most code units is zero, the low byte never is.
    let mostly = |n: usize| n.saturating_mul(10) >= pairs.saturating_mul(4);
    let rarely = |n: usize| n.saturating_mul(20) <= pairs;
    if mostly(odd_zero) && rarely(even_zero) {
        Some(TextEncoding::Utf16Le)
    } else if mostly(even_zero) && rarely(odd_zero) {
        Some(TextEncoding::Utf16Be)
    } else {
        None
    }
}

/// Decode `bytes` as `enc`, dropping that encoding's BOM when present. Errors name the problem
/// (invalid UTF-8, an odd UTF-16 length, an unpaired surrogate).
pub fn decode(bytes: &[u8], enc: TextEncoding) -> Result<String, String> {
    let body = bytes.strip_prefix(bom_bytes(enc)).filter(|_| enc != TextEncoding::Latin1).unwrap_or(bytes);
    match enc {
        TextEncoding::Utf8 => {
            std::str::from_utf8(body).map(str::to_string).map_err(|e| format!("not valid UTF-8 ({e})"))
        }
        TextEncoding::Latin1 => Ok(body.iter().map(|b| char::from(*b)).collect()),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            if !body.len().is_multiple_of(2) {
                return Err(format!("not valid {}: odd number of bytes", enc.label()));
            }
            let units = body.as_chunks::<2>().0.iter().map(|pair| match enc {
                TextEncoding::Utf16Be => u16::from_be_bytes(*pair),
                _ => u16::from_le_bytes(*pair),
            });
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .map_err(|e| format!("not valid {}: {e}", enc.label()))
        }
    }
}

/// Encode `text` as `enc` (with its BOM when `bom`). Latin-1 refuses chars above U+00FF.
pub fn encode(text: &str, enc: TextEncoding, bom: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len().saturating_add(4));
    if bom {
        out.extend_from_slice(bom_bytes(enc));
    }
    match enc {
        TextEncoding::Utf8 => out.extend_from_slice(text.as_bytes()),
        TextEncoding::Utf16Le => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_le_bytes())),
        TextEncoding::Utf16Be => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_be_bytes())),
        TextEncoding::Latin1 => {
            for (i, c) in text.chars().enumerate() {
                let b = u8::try_from(u32::from(c)).map_err(|_| {
                    format!("'{c}' (U+{:04X}, char {i}) cannot be saved in {}", u32::from(c), enc.label())
                })?;
                out.push(b);
            }
        }
    }
    Ok(out)
}

/// Whether `text` can be saved in `enc`.
pub fn can_encode(text: &str, enc: TextEncoding) -> bool {
    enc != TextEncoding::Latin1 || text.chars().all(|c| u32::from(c) <= 0xFF)
}

/// Read a file for an editor: its text, encoding and BOM. Files that are not UTF-8 open as UTF-16
/// (BOM or zero-byte pattern) or Latin-1; real binaries, directories and oversized files are errors.
pub fn read_document(path: &Path) -> Result<(String, TextEncoding, bool), String> {
    match codecraft_fs::read_text_file(path) {
        Ok(text) => Ok((text, TextEncoding::Utf8, starts_with_utf8_bom(path))),
        Err(e @ (FsError::NotUtf8(_) | FsError::Binary(_))) => {
            let bytes = read_bytes(path)?;
            let (enc, bom) = detect(&bytes);
            match (&e, enc) {
                (FsError::Binary(_), TextEncoding::Utf8 | TextEncoding::Latin1) => Err(e.to_string()),
                _ => decode(&bytes, enc).map(|t| (t, enc, bom)).map_err(|d| format!("{}: {d}", path.display())),
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

fn starts_with_utf8_bom(path: &Path) -> bool {
    let mut head = [0u8; 3];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == UTF8_BOM
}

/// The file's bytes, capped like text files.
pub fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > codecraft_fs::MAX_TEXT_FILE_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(codecraft_fs::MAX_TEXT_FILE_BYTES).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(bytes)
}

/// The bytes to write for a document: its disk text in its encoding (and BOM).
pub fn disk_bytes(od: &OpenDocument) -> Result<Vec<u8>, String> {
    encode(&od.doc.to_disk_text(), od.encoding, od.bom)
}

/// Save a document to `path` in its encoding (atomic, like every other save).
pub fn write_document(path: &Path, od: &OpenDocument) -> Result<(), String> {
    if od.encoding == TextEncoding::Utf8 && !od.bom {
        return codecraft_fs::write_text_file_atomic(path, &od.doc.to_disk_text()).map_err(|e| e.to_string());
    }
    let bytes = disk_bytes(od)?;
    codecraft_fs::write_file_atomic(path, &bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_encoding_with_and_without_bom() {
        let text = "fn main() {\n    println!(\"héllo · wörld\");\n}\n";
        for enc in TextEncoding::ALL {
            for bom in [false, true] {
                if enc == TextEncoding::Latin1 && bom {
                    continue;
                }
                let bytes = encode(text, enc, bom).unwrap();
                assert_eq!(detect(&bytes), (enc, bom), "{enc:?} bom={bom}");
                assert_eq!(decode(&bytes, enc).unwrap(), text, "{enc:?} bom={bom}");
            }
        }
    }

    #[test]
    fn latin1_refuses_wide_chars_and_utf16_rejects_bad_input() {
        assert!(encode("€", TextEncoding::Latin1, false).is_err());
        assert!(can_encode("é", TextEncoding::Latin1));
        assert!(!can_encode("日本", TextEncoding::Latin1));
        assert!(decode(&[0x61, 0x00, 0x62], TextEncoding::Utf16Le).is_err());
        // A lone high surrogate.
        assert!(decode(&[0x00, 0xD8], TextEncoding::Utf16Le).is_err());
        assert!(decode(&[0xC3], TextEncoding::Utf8).is_err());
        // Hostile input never panics.
        for len in 0..9 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 97 % 256) as u8).collect();
            let _ = detect(&bytes);
            for enc in TextEncoding::ALL {
                let _ = decode(&bytes, enc);
            }
        }
    }

    #[test]
    fn reads_files_in_any_supported_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, encode("x = 'é'\n", TextEncoding::Utf16Le, true).unwrap()).unwrap();
        assert_eq!(read_document(&p).unwrap(), ("x = 'é'\n".to_string(), TextEncoding::Utf16Le, true));
        std::fs::write(&p, [b'c', b'a', b'f', 0xE9, b'\n']).unwrap();
        assert_eq!(read_document(&p).unwrap(), ("café\n".to_string(), TextEncoding::Latin1, false));
        std::fs::write(&p, [0xEF, 0xBB, 0xBF, b'o', b'k']).unwrap();
        assert_eq!(read_document(&p).unwrap(), ("ok".to_string(), TextEncoding::Utf8, true));
        // A real binary stays an error.
        std::fs::write(&p, [0x7F, b'E', b'L', b'F', 0, 0, 0, 1, 2, 3, 0, 0, 9]).unwrap();
        assert!(read_document(&p).is_err());
    }
}
