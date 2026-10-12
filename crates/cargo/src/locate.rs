//! Finding the `cargo` and `rustfmt` binaries and the project manifest for a path.

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Manifests bigger than this are only scanned up to this many bytes for `[workspace]`.
const MAX_MANIFEST_BYTES: u64 = 4 << 20;

pub(crate) fn find_cargo() -> PathBuf {
    find_cargo_in(std::env::var_os("CARGO"), std::env::var_os("CARGO_HOME"), home_dir())
}

/// `cargo_env` (`$CARGO`) if it is an existing file, then `cargo` in `$CARGO_HOME/bin` and
/// `~/.cargo/bin`, then plain `cargo` for a `PATH` lookup.
pub(crate) fn find_cargo_in(
    cargo_env: Option<OsString>,
    cargo_home: Option<OsString>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(cargo) = cargo_env.filter(|c| !c.is_empty()).map(PathBuf::from)
        && cargo.is_file()
    {
        return cargo;
    }
    find_in_dirs(&cargo_bin_dirs(cargo_home, home), "cargo")
}

pub(crate) fn find_rustfmt() -> PathBuf {
    find_rustfmt_in(&find_cargo(), std::env::var_os("CARGO_HOME"), home_dir())
}

/// `rustfmt` next to `cargo` (same toolchain), then in `$CARGO_HOME/bin` and `~/.cargo/bin`, then
/// plain `rustfmt` for a `PATH` lookup.
pub(crate) fn find_rustfmt_in(cargo: &Path, cargo_home: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    let mut dirs = Vec::new();
    if let Some(dir) = cargo.parent().filter(|d| !d.as_os_str().is_empty()) {
        dirs.push(dir.to_path_buf());
    }
    dirs.extend(cargo_bin_dirs(cargo_home, home));
    find_in_dirs(&dirs, "rustfmt")
}

fn find_in_dirs(dirs: &[PathBuf], tool: &str) -> PathBuf {
    let file_name = format!("{tool}{}", std::env::consts::EXE_SUFFIX);
    dirs.iter().map(|dir| dir.join(&file_name)).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(tool))
}

fn cargo_bin_dirs(cargo_home: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(cargo_home) = cargo_home.filter(|c| !c.is_empty()) {
        dirs.push(PathBuf::from(cargo_home).join("bin"));
    }
    if let Some(home) = home {
        dirs.push(home.join(".cargo").join("bin"));
    }
    dirs
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|h| !h.is_empty()).map(PathBuf::from)
}

pub(crate) fn find_manifest(start: &Path) -> Option<PathBuf> {
    // Relative paths would stop the walk at the current directory; make them absolute first.
    let start = std::path::absolute(start).unwrap_or_else(|_| start.to_path_buf());
    let first_dir = if start.is_file() { start.parent()?.to_path_buf() } else { start };
    let mut nearest = None;
    let mut outermost_workspace = None;
    for dir in first_dir.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if declares_workspace(&manifest) {
            outermost_workspace = Some(manifest.clone());
        }
        if nearest.is_none() {
            nearest = Some(manifest);
        }
    }
    outermost_workspace.or(nearest)
}

fn declares_workspace(manifest: &Path) -> bool {
    let Ok(file) = File::open(manifest) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(MAX_MANIFEST_BYTES).read_to_end(&mut bytes).is_err() {
        return false;
    }
    String::from_utf8_lossy(&bytes).lines().any(is_workspace_header)
}

/// `[workspace]`, or a `[workspace.…]` sub-table header (which implies the table), ignoring
/// whitespace and a trailing comment.
pub(crate) fn is_workspace_header(line: &str) -> bool {
    let line = line.split('#').next().unwrap_or_default().trim();
    let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) else {
        return false;
    };
    if inner.starts_with('[') {
        return false;
    }
    inner.split('.').next().map(str::trim) == Some("workspace")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn touch_exe(path: &Path) {
        write(path, "#!/bin/sh\n");
    }

    const PACKAGE: &str = "[package]\nname = \"member\"\nversion = \"0.1.0\"\n";

    #[test]
    fn workspace_headers() {
        assert!(is_workspace_header("[workspace]"));
        assert!(is_workspace_header("  [workspace]  # the root"));
        assert!(is_workspace_header("[ workspace ]"));
        assert!(is_workspace_header("[workspace.package]"));
        assert!(is_workspace_header("[workspace.dependencies]"));
        assert!(!is_workspace_header("# [workspace]"));
        assert!(!is_workspace_header("[package]"));
        assert!(!is_workspace_header("[package.metadata.workspace]"));
        assert!(!is_workspace_header("[[workspace]]"));
        assert!(!is_workspace_header("workspace = true"));
        assert!(!is_workspace_header("[workspaces]"));
        assert!(!is_workspace_header(""));
        assert!(!is_workspace_header("["));
    }

    #[test]
    fn member_file_resolves_to_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n");
        write(&root.join("crates/member/Cargo.toml"), PACKAGE);
        write(&root.join("crates/member/src/lib.rs"), "");
        let expected = Some(std::path::absolute(root.join("Cargo.toml")).unwrap());
        assert_eq!(find_manifest(&root.join("crates/member/src/lib.rs")), expected);
        assert_eq!(find_manifest(&root.join("crates/member")), expected);
        assert_eq!(find_manifest(&root.join("crates/member/Cargo.toml")), expected);
        assert_eq!(find_manifest(root), expected);
        // A path that does not exist yet (a new file) still resolves through its ancestors.
        assert_eq!(find_manifest(&root.join("crates/member/src/new_module.rs")), expected);
    }

    #[test]
    fn workspace_package_header_marks_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "[workspace.package]\nversion = \"0.1.0\"\n");
        write(&root.join("member/Cargo.toml"), PACKAGE);
        assert_eq!(find_manifest(&root.join("member")), Some(std::path::absolute(root.join("Cargo.toml")).unwrap()));
    }

    #[test]
    fn standalone_crate_uses_nearest_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        write(&outer.join("Cargo.toml"), "[package]\nname = \"outer\"\n# [workspace]\n");
        write(&outer.join("inner/Cargo.toml"), PACKAGE);
        write(&outer.join("inner/src/main.rs"), "fn main() {}");
        assert_eq!(
            find_manifest(&outer.join("inner/src/main.rs")),
            Some(std::path::absolute(outer.join("inner/Cargo.toml")).unwrap())
        );
        assert_eq!(find_manifest(&outer.join("src")), Some(std::path::absolute(outer.join("Cargo.toml")).unwrap()));
    }

    #[test]
    fn outermost_workspace_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "[workspace]\n");
        write(&root.join("nested/Cargo.toml"), "[workspace]\n[package]\nname = \"nested\"\n");
        write(&root.join("nested/member/Cargo.toml"), PACKAGE);
        assert_eq!(
            find_manifest(&root.join("nested/member")),
            Some(std::path::absolute(root.join("Cargo.toml")).unwrap())
        );
    }

    #[test]
    fn no_manifest_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        fs::create_dir_all(&dir).unwrap();
        // Only meaningful when the temp dir itself is not inside a Cargo project.
        if tmp.path().ancestors().any(|d| d.join("Cargo.toml").is_file()) {
            return;
        }
        assert_eq!(find_manifest(&dir), None);
    }

    #[test]
    fn unreadable_manifest_is_not_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        fs::write(&manifest, [0xff, 0xfe, b'[', b'p', b']']).unwrap();
        assert!(!declares_workspace(&manifest));
        assert!(!declares_workspace(&tmp.path().join("missing/Cargo.toml")));
        fs::write(&manifest, b"\xff\xfe\n[workspace]\n").unwrap();
        assert!(declares_workspace(&manifest));
    }

    #[test]
    fn cargo_lookup_order() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = std::env::consts::EXE_SUFFIX;
        let explicit = tmp.path().join("explicit/cargo-bin");
        let cargo_home = tmp.path().join("cargo-home");
        let home = tmp.path().join("home");
        let in_cargo_home = cargo_home.join("bin").join(format!("cargo{exe}"));
        let in_home = home.join(".cargo/bin").join(format!("cargo{exe}"));

        let lookup = |cargo: Option<&Path>| {
            find_cargo_in(cargo.map(|c| c.as_os_str().to_owned()), Some(cargo_home.clone().into()), Some(home.clone()))
        };
        assert_eq!(lookup(Some(&explicit)), PathBuf::from("cargo"));
        touch_exe(&in_home);
        assert_eq!(lookup(Some(&explicit)), in_home);
        touch_exe(&in_cargo_home);
        assert_eq!(lookup(None), in_cargo_home);
        touch_exe(&explicit);
        assert_eq!(lookup(Some(&explicit)), explicit);
        // `$CARGO` pointing at a directory or set to "" is ignored.
        assert_eq!(lookup(Some(tmp.path())), in_cargo_home);
        assert_eq!(lookup(Some(Path::new(""))), in_cargo_home);
        assert_eq!(find_cargo_in(None, None, None), PathBuf::from("cargo"));
    }

    #[test]
    fn rustfmt_lookup_order() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = std::env::consts::EXE_SUFFIX;
        let toolchain = tmp.path().join("toolchain/bin");
        let home = tmp.path().join("home");
        let next_to_cargo = toolchain.join(format!("rustfmt{exe}"));
        let in_home = home.join(".cargo/bin").join(format!("rustfmt{exe}"));
        let cargo = toolchain.join(format!("cargo{exe}"));

        assert_eq!(find_rustfmt_in(&cargo, None, Some(home.clone())), PathBuf::from("rustfmt"));
        touch_exe(&in_home);
        assert_eq!(find_rustfmt_in(&cargo, None, Some(home.clone())), in_home);
        touch_exe(&next_to_cargo);
        assert_eq!(find_rustfmt_in(&cargo, None, Some(home.clone())), next_to_cargo);
        // Bare `cargo` (PATH lookup) has no directory to look next to.
        assert_eq!(find_rustfmt_in(Path::new("cargo"), None, Some(home)), in_home);
    }
}
