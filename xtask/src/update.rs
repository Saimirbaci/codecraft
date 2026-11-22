//! `cargo xtask update-keygen` and `cargo xtask update-pack`: the release side of self-update
//! (`crates/update`, docs/release.md › Updates). The private key lives on the machine that builds
//! releases, outside the repository; only its public half (`crates/update/update.pub`) is committed.

use codecraft_update as upd;
use std::path::{Path, PathBuf};

const PUBLIC_KEY_FILE: &str = "crates/update/update.pub";

fn default_key_path() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("CODECRAFT_UPDATE_KEY") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).ok_or("no home directory")?;
    Ok(PathBuf::from(home).join(".codecraft-release").join("update.key"))
}

/// `--name value` / `--name=value` pairs; anything else is an error.
fn options(args: &[String], names: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        if !names.contains(&name) {
            return Err(format!("unknown option {arg} (expected {})", names.join(", ")));
        }
        let value = match inline {
            Some(v) => v,
            None => it.next().cloned().ok_or_else(|| format!("{name} needs a value"))?,
        };
        out.push((name.to_string(), value));
    }
    Ok(out)
}
fn get<'a>(opts: &'a [(String, String)], name: &str) -> Option<&'a str> {
    opts.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

#[cfg(unix)]
fn private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}
#[cfg(not(unix))]
fn private_dir(_: &Path) -> Result<(), String> {
    Ok(())
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(text.as_bytes()).map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Create the release signing key (refuses to replace one) and record its public half.
pub fn keygen(root: &Path, args: &[String]) -> Result<(), String> {
    let opts = options(args, &["--key"])?;
    let path = match get(&opts, "--key") {
        Some(p) => PathBuf::from(p),
        None => default_key_path()?,
    };
    if path.exists() {
        return Err(format!(
            "{} already exists. Replacing the key makes every installed app reject new updates; \
             delete the file by hand only if you mean that.",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        private_dir(dir)?;
    }
    let secret = upd::generate_secret().map_err(|e| e.to_string())?;
    let public = upd::public_key(&secret).map_err(|e| e.to_string())?;
    write_private(&path, &format!("{secret}\n"))?;
    std::fs::write(root.join(PUBLIC_KEY_FILE), format!("{public}\n")).map_err(|e| e.to_string())?;
    println!("private key: {} (back it up; never commit it)", path.display());
    println!("public key:  {public} (written to {PUBLIC_KEY_FILE}; commit it, and release a full build before updates can work)");
    Ok(())
}

fn read_secret(opts: &[(String, String)]) -> Result<String, String> {
    let path = match get(opts, "--key") {
        Some(p) => PathBuf::from(p),
        None => default_key_path()?,
    };
    let secret = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read the signing key {}: {e} (create it with `cargo xtask update-keygen`)", path.display()))?;
    Ok(secret.trim().to_string())
}

/// Pack a payload directory and write its signed manifest next to it.
///
/// `--payload <dir> --out <dir> --version <x.y.z> --commit <sha> [--repo owner/name]
/// [--min-launcher <x.y.z>] [--notes <text>] [--key <file>]`
pub fn pack(args: &[String]) -> Result<(), String> {
    let opts = options(
        args,
        &["--payload", "--out", "--version", "--commit", "--repo", "--min-launcher", "--notes", "--key"],
    )?;
    let need = |n: &str| get(&opts, n).ok_or_else(|| format!("{n} is required"));
    let (payload, out, version, commit) = (Path::new(need("--payload")?), Path::new(need("--out")?), need("--version")?, need("--commit")?);
    let repo = get(&opts, "--repo").unwrap_or(upd::DEFAULT_REPO);
    let secret = read_secret(&opts)?;
    let public = upd::public_key(&secret).map_err(|e| e.to_string())?;
    if public.trim() != upd::PUBLIC_KEY.trim() {
        return Err(format!(
            "the signing key does not match {PUBLIC_KEY_FILE}: apps built from this tree would reject the update"
        ));
    }
    if upd::Version::parse(version).is_none() {
        return Err(format!("`{version}` is not a version like 1.2.3"));
    }
    if !payload.is_dir() {
        return Err(format!("{} is not a directory", payload.display()));
    }
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let platform = upd::platform();
    let name = upd::payload_name(version, &platform);
    let archive = out.join(&name);
    upd::pack(payload, &archive).map_err(|e| e.to_string())?;
    let manifest = upd::Manifest {
        format: upd::FORMAT,
        version: version.to_string(),
        commit: commit.to_string(),
        platform: platform.clone(),
        url: format!("https://github.com/{repo}/releases/download/v{version}/{name}"),
        sha256: upd::sha256_file(&archive).map_err(|e| e.to_string())?,
        size: std::fs::metadata(&archive).map_err(|e| e.to_string())?.len(),
        min_launcher: get(&opts, "--min-launcher").unwrap_or_default().to_string(),
        notes: get(&opts, "--notes").unwrap_or_default().to_string(),
    };
    let body = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    let sig = upd::sign(&secret, &body).map_err(|e| e.to_string())?;
    let manifest_path = out.join(upd::manifest_name(&platform));
    std::fs::write(&manifest_path, &body).map_err(|e| e.to_string())?;
    std::fs::write(format!("{}.sig", manifest_path.display()), format!("{sig}\n")).map_err(|e| e.to_string())?;
    // The app's own check, run on what was just written.
    upd::verify(&upd::Config::for_repo(repo), &body, sig.as_bytes()).map_err(|e| format!("self-check failed: {e}"))?;
    println!("{}\n{}\n{}.sig", archive.display(), manifest_path.display(), manifest_path.display());
    Ok(())
}
