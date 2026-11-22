#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
//! The whole update path against a local HTTP server: a signed manifest and payload are
//! fetched, verified, downloaded and unpacked; every kind of tampering is refused.

use codecraft_update as upd;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

struct Server {
    base: String,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let shared = files.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = shared.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                        stream.write_all(&b)
                    }
                    None => write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                };
            }
        });
        Self { base, files }
    }
    fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.files.lock().unwrap().insert(path.to_string(), body.into());
    }
}

struct Release {
    server: Server,
    secret: String,
    cfg: upd::Config,
    payload: tempfile::TempDir,
}

fn payload_tree(dir: &Path) {
    std::fs::create_dir_all(dir.join("CodeCraft.app/Contents/MacOS")).unwrap();
    std::fs::write(dir.join("CodeCraft.app/Contents/MacOS/codecraft"), "#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("CodeCraft.app/Contents/MacOS/codecraft"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    std::fs::write(dir.join("CodeCraft.app/Contents/Info.plist"), "plist").unwrap();
}

/// Publish version `version` on a fresh server, signed with a fresh key.
fn release(version: &str) -> Release {
    let server = Server::start();
    let secret = upd::generate_secret().unwrap();
    let public: [u8; 32] = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(upd::public_key(&secret).unwrap()).unwrap().try_into().unwrap()
    };
    let cfg = upd::Config {
        base_url: format!("{}/latest", server.base),
        asset_prefix: format!("{}/download/", server.base),
        public_key: public,
    };
    let payload = tempfile::tempdir().unwrap();
    payload_tree(payload.path());
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("payload.tar.gz");
    upd::pack(payload.path(), &archive).unwrap();
    let bytes = std::fs::read(&archive).unwrap();
    let platform = upd::platform();
    let name = upd::payload_name(version, &platform);
    server.put(&format!("/download/v{version}/{name}"), bytes.clone());
    let manifest = upd::Manifest {
        format: upd::FORMAT,
        version: version.into(),
        commit: "abcdef123456".into(),
        platform: platform.clone(),
        url: format!("{}/download/v{version}/{name}", server.base),
        sha256: upd::sha256_file(&archive).unwrap(),
        size: bytes.len() as u64,
        min_launcher: String::new(),
        notes: "fixes".into(),
    };
    let body = serde_json::to_vec_pretty(&manifest).unwrap();
    server.put(&format!("/latest/{}", upd::manifest_name(&platform)), body.clone());
    server.put(&format!("/latest/{}.sig", upd::manifest_name(&platform)), upd::sign(&secret, &body).unwrap());
    Release { server, secret, cfg, payload }
}

fn http() -> upd::Http {
    upd::Http::new().allow_http()
}

fn available(r: &Release, current: &str) -> upd::Manifest {
    match upd::check(&r.cfg, &http(), current).unwrap() {
        upd::Check::Available(m) => m,
        upd::Check::UpToDate => panic!("expected an update"),
    }
}

#[test]
fn a_newer_release_is_found_downloaded_and_unpacked() {
    let r = release("0.4.0");
    let manifest = available(&r, "0.3.0");
    assert_eq!((manifest.version.as_str(), manifest.notes.as_str()), ("0.4.0", "fixes"));
    let work = tempfile::tempdir().unwrap();
    let file = upd::download(&http(), &manifest, &work.path().join("dl")).unwrap();
    let dest = work.path().join("payload");
    upd::unpack(&file, &dest).unwrap();
    let exe = dest.join("CodeCraft.app/Contents/MacOS/codecraft");
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111, "stays executable");
    }
    assert_eq!(std::fs::read_to_string(dest.join("CodeCraft.app/Contents/Info.plist")).unwrap(), "plist");
    drop(r.payload);
}

#[test]
fn the_same_or_an_older_release_is_up_to_date() {
    let r = release("0.4.0");
    assert!(matches!(upd::check(&r.cfg, &http(), "0.4.0").unwrap(), upd::Check::UpToDate));
    assert!(matches!(upd::check(&r.cfg, &http(), "0.5.0").unwrap(), upd::Check::UpToDate), "never a downgrade");
    assert!(matches!(upd::check(&r.cfg, &http(), "0.4.0-rc.1").unwrap(), upd::Check::Available(_)));
}

#[test]
fn a_manifest_signed_with_another_key_is_refused() {
    let mut r = release("0.4.0");
    let platform = upd::platform();
    let body = serde_json::to_vec_pretty(&available(&r, "0.3.0")).unwrap();
    let forger = upd::generate_secret().unwrap();
    r.server.put(&format!("/latest/{}", upd::manifest_name(&platform)), body.clone());
    r.server.put(&format!("/latest/{}.sig", upd::manifest_name(&platform)), upd::sign(&forger, &body).unwrap());
    let err = upd::check(&r.cfg, &http(), "0.3.0").err().unwrap();
    assert!(matches!(err, upd::Error::Signature(_)), "{err}");
    r.secret.clear();
}

#[test]
fn an_edited_manifest_is_refused() {
    let r = release("0.4.0");
    let platform = upd::platform();
    let path = format!("/latest/{}", upd::manifest_name(&platform));
    let body = r.server.files.lock().unwrap().get(&path).cloned().unwrap();
    let edited = String::from_utf8(body).unwrap().replace("0.4.0", "9.9.9");
    r.server.put(&path, edited);
    assert!(matches!(upd::check(&r.cfg, &http(), "0.3.0").err().unwrap(), upd::Error::Signature(_)));
}

#[test]
fn signed_manifests_must_still_point_at_this_repositorys_assets() {
    let r = release("0.4.0");
    let mut manifest = available(&r, "0.3.0");
    manifest.url = "https://evil.example/payload.tar.gz".into();
    let body = serde_json::to_vec(&manifest).unwrap();
    let sig = upd::sign(&r.secret, &body).unwrap();
    let err = upd::verify(&r.cfg, &body, sig.as_bytes()).err().unwrap();
    assert!(err.to_string().contains("release asset"), "{err}");
    let mut other = available(&r, "0.3.0");
    other.sha256 = "zz".into();
    let body = serde_json::to_vec(&other).unwrap();
    assert!(upd::verify(&r.cfg, &body, upd::sign(&r.secret, &body).unwrap().as_bytes()).is_err());
}

#[test]
fn a_payload_that_differs_from_the_signed_hash_is_deleted() {
    let r = release("0.4.0");
    let manifest = available(&r, "0.3.0");
    let path = manifest.url.strip_prefix(&r.server.base).unwrap().to_string();
    let mut bytes = r.server.files.lock().unwrap().get(&path).cloned().unwrap();
    if let Some(b) = bytes.last_mut() {
        *b ^= 0xff;
    }
    r.server.put(&path, bytes);
    let work = tempfile::tempdir().unwrap();
    let err = upd::download(&http(), &manifest, work.path()).err().unwrap();
    assert!(err.to_string().contains("SHA-256"), "{err}");
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0, "nothing is kept");
}

#[test]
fn a_payload_larger_than_the_signed_size_is_cut_off() {
    let r = release("0.4.0");
    let mut manifest = available(&r, "0.3.0");
    manifest.size -= 10;
    let work = tempfile::tempdir().unwrap();
    assert!(upd::download(&http(), &manifest, work.path()).is_err());
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
}

#[test]
fn only_https_is_fetched_by_default() {
    let r = release("0.4.0");
    assert!(upd::check(&r.cfg, &upd::Http::new(), "0.3.0").is_err());
}

#[test]
fn a_missing_release_is_a_network_error_not_a_panic() {
    let r = release("0.4.0");
    let cfg = upd::Config { base_url: format!("{}/nothing", r.server.base), ..r.cfg.clone() };
    assert!(matches!(upd::check(&cfg, &http(), "0.3.0").err().unwrap(), upd::Error::Network(_)));
}

fn tar_with(name: &[u8], kind: tar::EntryType, link: Option<&str>, data: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_entry_type(kind);
    header.set_mode(0o755);
    header.set_size(data.len() as u64);
    if let Some(l) = link {
        header.set_link_name(l).unwrap();
    }
    header.set_cksum();
    let mut raw = Vec::new();
    raw.extend_from_slice(header.as_bytes());
    raw.extend_from_slice(data);
    raw.resize(raw.len().div_ceil(512) * 512, 0);
    raw.extend_from_slice(&[0u8; 1024]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&raw).unwrap();
    gz.finish().unwrap()
}

#[test]
fn hostile_archives_are_refused() {
    let work = tempfile::tempdir().unwrap();
    for (name, kind, link) in [
        (&b"../escape"[..], tar::EntryType::Regular, None),
        (&b"/abs/path"[..], tar::EntryType::Regular, None),
        (&b"a/../../b"[..], tar::EntryType::Regular, None),
        (&b"link"[..], tar::EntryType::Symlink, Some("/etc/passwd")),
        (&b"hard"[..], tar::EntryType::Link, Some("other")),
    ] {
        let archive = work.path().join("a.tar.gz");
        std::fs::write(&archive, tar_with(name, kind, link, b"x")).unwrap();
        let dest = work.path().join("out");
        let _ = std::fs::remove_dir_all(&dest);
        assert!(upd::unpack(&archive, &dest).is_err(), "{}", String::from_utf8_lossy(name));
    }
    assert!(!work.path().join("escape").exists());
    let garbage = work.path().join("g.tar.gz");
    std::fs::write(&garbage, b"not an archive").unwrap();
    assert!(upd::unpack(&garbage, &work.path().join("o2")).is_err());
}

#[test]
fn packing_is_reproducible_and_refuses_links() {
    let a = tempfile::tempdir().unwrap();
    payload_tree(a.path());
    let out = tempfile::tempdir().unwrap();
    upd::pack(a.path(), &out.path().join("1.tar.gz")).unwrap();
    upd::pack(a.path(), &out.path().join("2.tar.gz")).unwrap();
    assert_eq!(std::fs::read(out.path().join("1.tar.gz")).unwrap(), std::fs::read(out.path().join("2.tar.gz")).unwrap());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc", a.path().join("link")).unwrap();
        assert!(upd::pack(a.path(), &out.path().join("3.tar.gz")).is_err());
    }
}

#[test]
fn the_committed_public_key_is_a_valid_key() {
    let cfg = upd::Config::release();
    assert_ne!(cfg.public_key, [0; 32], "run `cargo xtask update-keygen` and commit crates/update/update.pub");
}
