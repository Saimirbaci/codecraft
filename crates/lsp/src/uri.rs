//! `file://` URIs for local paths.
//!
//! Encoding keeps RFC 3986 unreserved characters, `/` and the path-safe sub-delimiters
//! (`!$&'()*+,;=:@`) and percent-encodes every other byte (space, `#`, `%`, `?`, non-ASCII, …).
//! That is the same output rust-analyzer produces for the same path (it uses the `url` crate),
//! so URIs we send and URIs it echoes back usually compare equal as strings. Still, compare
//! paths, not URI strings: decode with [`crate::uri_to_path`].

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::Uri;

use crate::{LspError, Result};

pub(crate) fn path_to_uri(path: &Path) -> Result<Uri> {
    if !path.is_absolute() {
        return Err(LspError::BadUri(format!("{} is not an absolute path", path.display())));
    }
    let text = encode_path(path).ok_or_else(|| LspError::BadUri(path.display().to_string()))?;
    Uri::from_str(&text).map_err(|e| LspError::BadUri(format!("{}: {e}", path.display())))
}

pub(crate) fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    if !uri.scheme()?.as_str().eq_ignore_ascii_case("file") {
        return None;
    }
    let host = uri.authority().map_or("", |authority| authority.host().as_str());
    let bytes = percent_decode(uri.path().as_str())?;
    if bytes.contains(&0) {
        return None;
    }
    decode_path(host, bytes)
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    percent_encode_into(path.as_os_str().as_bytes(), &mut out);
    Some(out)
}

#[cfg(unix)]
fn decode_path(host: &str, bytes: Vec<u8>) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) || bytes.first() != Some(&b'/') {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn encode_path(path: &Path) -> Option<String> {
    windows::path_to_uri_string(path.to_str()?)
}

#[cfg(not(unix))]
fn decode_path(host: &str, bytes: Vec<u8>) -> Option<PathBuf> {
    windows::uri_parts_to_path(host, &String::from_utf8(bytes).ok()?).map(PathBuf::from)
}

fn keep_unencoded(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte)
}

pub(crate) fn percent_encode_into(bytes: &[u8], out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in bytes {
        if keep_unencoded(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 0x0f] {
                out.push(HEX.get(usize::from(nibble)).map_or('0', |&h| char::from(h)));
            }
        }
    }
}

/// Decodes `%XX` escapes. `None` for a malformed escape (a parsed `Uri` never has one).
pub(crate) fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = hex_value(bytes.next()?)?;
            let low = hex_value(bytes.next()?)?;
            out.push(high << 4 | low);
        } else {
            out.push(byte);
        }
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// Windows path ⟷ URI string conversion. Plain string functions so they are tested on every
/// platform; only Windows builds call them.
#[cfg_attr(unix, allow(dead_code))]
mod windows {
    use super::percent_encode_into;

    /// `C:\a\b.rs` → `file:///C:/a/b.rs`; `\\server\share\x` → `file://server/share/x`.
    pub(super) fn path_to_uri_string(path: &str) -> Option<String> {
        let path = path.replace('\\', "/");
        let path = match path.strip_prefix("//?/") {
            Some(rest) => match rest.strip_prefix("UNC/") {
                Some(unc) => format!("//{unc}"),
                None => rest.to_owned(),
            },
            None => path,
        };
        let mut out = String::from("file://");
        if let Some(unc) = path.strip_prefix("//") {
            let (host, rest) = unc.split_once('/').unwrap_or((unc, ""));
            if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)) {
                return None;
            }
            out.push_str(host);
            percent_encode_into(format!("/{rest}").as_bytes(), &mut out);
        } else {
            if !has_drive(&path) {
                return None;
            }
            percent_encode_into(format!("/{path}").as_bytes(), &mut out);
        }
        Some(out)
    }

    /// The inverse of [`path_to_uri_string`], from the URI's host and decoded path.
    pub(super) fn uri_parts_to_path(host: &str, path: &str) -> Option<String> {
        if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
            return Some(format!("\\\\{host}{}", path.replace('/', "\\")));
        }
        let path = path.strip_prefix('/')?;
        has_drive(path).then(|| path.replace('/', "\\"))
    }

    fn has_drive(path: &str) -> bool {
        let mut bytes = path.bytes();
        matches!((bytes.next(), bytes.next()), (Some(letter), Some(b':')) if letter.is_ascii_alphabetic())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(text: &str) -> Uri {
        Uri::from_str(text).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn plain_path() {
        let u = path_to_uri(Path::new("/home/me/project/src/main.rs")).unwrap();
        assert_eq!(u.as_str(), "file:///home/me/project/src/main.rs");
        assert_eq!(uri_to_path(&u).unwrap(), Path::new("/home/me/project/src/main.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn roundtrips_awkward_paths() {
        for path in [
            "/",
            "/tmp/a b/c d.rs",
            "/tmp/ünïcödé/κόσμε/日本.rs",
            "/tmp/hash#tag/x.rs",
            "/tmp/100%/x%20y.rs",
            "/tmp/q?uery/x.rs",
            "/tmp/brackets[1]/{x}/a^b|c\\d`e\".rs",
            "/tmp/sub-delims!$&'()*+,;=:@/x.rs",
            "/tmp/tab\there/new\nline.rs",
        ] {
            let u = path_to_uri(Path::new(path)).unwrap();
            assert!(u.as_str().starts_with("file:///"), "{path} → {}", u.as_str());
            assert_eq!(uri_to_path(&u).unwrap(), Path::new(path), "{path} → {}", u.as_str());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_unix_path_roundtrips() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(OsStr::from_bytes(b"/tmp/bad\xff\xfe.rs"));
        let u = path_to_uri(path).unwrap();
        assert_eq!(u.as_str(), "file:///tmp/bad%FF%FE.rs");
        assert_eq!(uri_to_path(&u).unwrap(), path);
    }

    #[cfg(unix)]
    #[test]
    fn encoding_matches_the_url_crate_style() {
        let u = path_to_uri(Path::new("/a b/c#d/e%f/ü/g+h@i:j.rs")).unwrap();
        assert_eq!(u.as_str(), "file:///a%20b/c%23d/e%25f/%C3%BC/g+h@i:j.rs");
    }

    #[test]
    fn relative_paths_are_rejected() {
        assert!(matches!(path_to_uri(Path::new("src/main.rs")), Err(LspError::BadUri(_))));
        assert!(matches!(path_to_uri(Path::new("")), Err(LspError::BadUri(_))));
    }

    #[cfg(unix)]
    #[test]
    fn decodes_uris_written_by_other_clients() {
        assert_eq!(uri_to_path(&uri("file://localhost/a/b.rs")).unwrap(), Path::new("/a/b.rs"));
        assert_eq!(uri_to_path(&uri("FILE:///a/%e2%9c%93.rs")).unwrap(), Path::new("/a/✓.rs"));
        assert_eq!(uri_to_path(&uri("file:///a/b%2Fc")).unwrap(), Path::new("/a/b/c"));
    }

    #[test]
    fn rejects_non_file_uris() {
        assert_eq!(uri_to_path(&uri("https://example.com/a.rs")), None);
        assert_eq!(uri_to_path(&uri("untitled:Untitled-1")), None);
        assert_eq!(uri_to_path(&uri("file:///a/nul%00byte")), None);
        assert_eq!(uri_to_path(&uri("file:relative/path")), None);
    }

    #[cfg(unix)]
    #[test]
    fn remote_hosts_are_not_local_paths_on_unix() {
        assert_eq!(uri_to_path(&uri("file://server/share/a.rs")), None);
    }

    #[test]
    fn percent_decode_rejects_broken_escapes() {
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("a%41%c3%a9"), Some(b"aA\xc3\xa9".to_vec()));
    }

    #[test]
    fn windows_drive_paths() {
        let s = windows::path_to_uri_string(r"C:\Users\me\my project\src\main.rs").unwrap();
        assert_eq!(s, "file:///C:/Users/me/my%20project/src/main.rs");
        assert!(Uri::from_str(&s).is_ok());
        assert_eq!(
            windows::uri_parts_to_path("", "/C:/Users/me/my project/src/main.rs").unwrap(),
            r"C:\Users\me\my project\src\main.rs"
        );
        // VS Code style: lowercase drive with an encoded colon decodes to the same shape.
        assert_eq!(windows::uri_parts_to_path("", "/c:/x.rs").unwrap(), r"c:\x.rs");
    }

    #[test]
    fn windows_verbatim_and_unc_paths() {
        assert_eq!(windows::path_to_uri_string(r"\\?\D:\a.rs").unwrap(), "file:///D:/a.rs");
        assert_eq!(windows::path_to_uri_string(r"\\server\share\dir\a.rs").unwrap(), "file://server/share/dir/a.rs");
        assert_eq!(windows::path_to_uri_string(r"\\?\UNC\server\share\a.rs").unwrap(), "file://server/share/a.rs");
        assert_eq!(windows::uri_parts_to_path("server", "/share/a.rs").unwrap(), r"\\server\share\a.rs");
        assert_eq!(windows::path_to_uri_string(r"relative\a.rs"), None);
        assert_eq!(windows::uri_parts_to_path("", "/no-drive/a.rs"), None);
    }
}
