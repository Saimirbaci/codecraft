//! `cargo xtask version [patch|minor|major|<x.y.z[-pre]>]`: print or bump the workspace version.
//!
//! The version lives once, under `[workspace.package]` in the root `Cargo.toml`; every crate
//! inherits it. Bumping rewrites that line and the matching entries of the workspace's own
//! crates in `Cargo.lock` (path packages: no `source` line), so `cargo build --locked` keeps
//! working without touching any dependency. Prints the new version (the release workflow reads
//! it).

use std::path::Path;

/// A `major.minor.patch[-pre]` version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let bad = || format!("`{s}` is not a version like 1.2.3 or 1.2.3-beta.1");
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut num = || -> Result<u64, String> {
            let p = parts.next().ok_or_else(bad)?;
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return Err(bad());
            }
            p.parse().map_err(|_| bad())
        };
        let (major, minor, patch) = (num()?, num()?, num()?);
        if parts.next().is_some() {
            return Err(bad());
        }
        if let Some(p) = pre
            && (p.is_empty()
                || p.split('.').any(|id| id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')))
        {
            return Err(bad());
        }
        Ok(Self { major, minor, patch, pre: pre.map(str::to_string) })
    }

    /// The next version for `level` (`patch`, `minor`, `major`), or `level` itself when it is a
    /// version. A pre-release bumped by `patch` becomes its release (1.2.3-rc.1 → 1.2.3).
    pub fn bump(&self, level: &str) -> Result<Self, String> {
        let next = match level.trim() {
            "patch" if self.pre.is_some() => Self { pre: None, ..self.clone() },
            "patch" => Self { patch: self.patch + 1, pre: None, ..self.clone() },
            "minor" => Self { minor: self.minor + 1, patch: 0, pre: None, ..self.clone() },
            "major" => Self { major: self.major + 1, minor: 0, patch: 0, pre: None },
            other => Self::parse(other.strip_prefix('v').unwrap_or(other))?,
        };
        if next == *self {
            return Err(format!("the version is already {self}"));
        }
        Ok(next)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

/// The `version = "…"` line under `[workspace.package]`: (byte range of the value, value).
fn workspace_version(manifest: &str) -> Result<(std::ops::Range<usize>, String), String> {
    let mut in_section = false;
    let mut offset = 0;
    for line in manifest.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == "[workspace.package]";
        } else if in_section
            && let Some(rest) = t.strip_prefix("version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let value = value.trim();
            let inner =
                value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).ok_or("the version is not a string")?;
            let start = offset + line.find('"').ok_or("the version is not a string")? + 1;
            return Ok((start..start + inner.len(), inner.to_string()));
        }
        offset += line.len();
    }
    Err("no `version` under [workspace.package] in Cargo.toml".into())
}

/// Replace `old` with `new` in the lock entries of path packages (the workspace's crates).
/// Returns the new lock and how many entries changed.
pub fn bump_lock(lock: &str, old: &str, new: &str) -> (String, usize) {
    const SEP: &str = "\n[[package]]\n";
    let old_line = format!("\nversion = \"{old}\"\n");
    let new_line = format!("\nversion = \"{new}\"\n");
    let mut changed = 0;
    let blocks: Vec<String> = lock
        .split(SEP)
        .enumerate()
        .map(|(i, block)| {
            let is_path = i > 0 && block.starts_with("name = ") && !block.lines().any(|l| l.starts_with("source = "));
            // The block's own line breaks: its first line has none before it, the last may lack one after.
            let framed = format!("\n{block}\n");
            if is_path && framed.contains(&old_line) {
                changed += 1;
                let replaced = framed.replacen(&old_line, &new_line, 1);
                replaced.get(1..replaced.len() - 1).unwrap_or_default().to_string()
            } else {
                block.to_string()
            }
        })
        .collect();
    (blocks.join(SEP), changed)
}

/// `cargo xtask version [level] [--dry-run]`.
pub fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let level = args.iter().find(|a| !a.starts_with("--"));
    let manifest_path = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).map_err(|e| format!("Cargo.toml: {e}"))?;
    let (range, current) = workspace_version(&manifest)?;
    let Some(level) = level else {
        println!("{current}");
        return Ok(());
    };
    let next = Version::parse(&current)?.bump(level)?.to_string();
    let lock_path = root.join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path).map_err(|e| format!("Cargo.lock: {e}"))?;
    let (new_lock, changed) = bump_lock(&lock, &current, &next);
    if changed == 0 {
        return Err(format!("no workspace crate at {current} in Cargo.lock"));
    }
    if dry_run {
        eprintln!("would bump {current} → {next} (Cargo.toml and {changed} Cargo.lock entries)");
    } else {
        let mut new_manifest = manifest.clone();
        new_manifest.replace_range(range, &next);
        std::fs::write(&manifest_path, new_manifest).map_err(|e| format!("Cargo.toml: {e}"))?;
        std::fs::write(&lock_path, new_lock).map_err(|e| format!("Cargo.lock: {e}"))?;
        eprintln!("bumped {current} → {next} (Cargo.toml and {changed} Cargo.lock entries)");
    }
    println!("{next}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_bumps() {
        let v = Version::parse("0.1.9").unwrap();
        assert_eq!(v.bump("patch").unwrap().to_string(), "0.1.10");
        assert_eq!(v.bump("minor").unwrap().to_string(), "0.2.0");
        assert_eq!(v.bump("major").unwrap().to_string(), "1.0.0");
        assert_eq!(v.bump("v0.3.0-beta.1").unwrap().to_string(), "0.3.0-beta.1");
        assert_eq!(Version::parse("1.2.3-rc.1").unwrap().bump("patch").unwrap().to_string(), "1.2.3");
        assert!(v.bump("0.1.9").is_err(), "same version");
        for bad in ["", "1.2", "1.2.3.4", "01.2.3", "1.2.x", "1.2.3-", "1.2.3-a..b", "1.2.3-a b", "patchy"] {
            assert!(v.bump(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn finds_only_the_workspace_version() {
        let m = "[package]\nversion = \"9.9.9\"\n\n[workspace.package]\nedition = \"2024\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\n\n[workspace.dependencies]\nserde = { version = \"1\" }\n";
        let (range, v) = workspace_version(m).unwrap();
        assert_eq!(v, "0.1.0");
        assert_eq!(&m[range], "0.1.0");
        assert!(workspace_version("[package]\nversion = \"1.0.0\"\n").is_err());
    }

    #[test]
    fn bumps_only_path_packages_in_the_lock() {
        let lock = "# generated\nversion = 4\n\n[[package]]\nname = \"codecraft\"\nversion = \"0.1.0\"\ndependencies = [\n \"serde\",\n]\n\n[[package]]\nname = \"other\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"00\"\n\n[[package]]\nname = \"xtask\"\nversion = \"0.1.0\"\n";
        let (out, n) = bump_lock(lock, "0.1.0", "0.2.0");
        assert_eq!(n, 2);
        assert!(out.contains("name = \"codecraft\"\nversion = \"0.2.0\"\n"));
        assert!(out.contains("name = \"xtask\"\nversion = \"0.2.0\"\n"));
        assert!(out.contains("name = \"other\"\nversion = \"0.1.0\"\n"), "{out}");
        assert!(out.starts_with("# generated\nversion = 4\n"), "the lock format version is untouched");
        assert_eq!(out.len(), lock.len());
    }

    #[test]
    fn the_real_lock_has_every_workspace_crate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let (_, v) = workspace_version(&manifest).unwrap();
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
        let (_, n) = bump_lock(&lock, &v, "99.0.0");
        let crates = std::fs::read_dir(root.join("crates")).unwrap().count()
            + std::fs::read_dir(root.join("apps")).unwrap().count() + 1 /* xtask */;
        assert_eq!(n, crates, "every workspace crate's lock entry follows the version");
    }
}
