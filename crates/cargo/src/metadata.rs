//! `cargo metadata --format-version 1`: running it and mapping its JSON onto the public model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::json::null_default;
use crate::{CargoError, CargoMetadata, Dependency, Package, Result, Target};

/// The edition cargo assumes when a manifest does not set one.
const DEFAULT_EDITION: &str = "2015";

pub(crate) fn run_metadata(dir: &Path, no_deps: bool) -> Result<CargoMetadata> {
    let cargo = crate::find_cargo();
    let mut args = vec!["metadata", "--format-version", "1"];
    if no_deps {
        args.push("--no-deps");
    }
    let mut command = Command::new(&cargo);
    command.args(&args).stdin(Stdio::null());
    // Accept a manifest (or any file) as well as a directory.
    if dir.is_file() {
        if dir.file_name().is_some_and(|n| n == "Cargo.toml") {
            command.arg("--manifest-path").arg(dir);
        }
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            command.current_dir(parent);
        }
    } else if !dir.as_os_str().is_empty() {
        command.current_dir(dir);
    }
    let output =
        command.output().map_err(|source| CargoError::Spawn { program: cargo.display().to_string(), source })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stderr = if stderr.is_empty() { format!("exited with {}", output.status) } else { stderr };
        return Err(CargoError::Failed { command: format!("cargo {}", args.join(" ")), stderr });
    }
    parse_metadata(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn parse_metadata(json: &str) -> Result<CargoMetadata> {
    let raw: RawMetadata = serde_json::from_str(json.trim()).map_err(|e| CargoError::Parse(e.to_string()))?;
    Ok(raw.into())
}

#[derive(Deserialize)]
struct RawMetadata {
    workspace_root: PathBuf,
    #[serde(default, deserialize_with = "null_default")]
    target_directory: PathBuf,
    packages: Vec<RawPackage>,
    #[serde(default, deserialize_with = "null_default")]
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct RawPackage {
    id: String,
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    version: String,
    #[serde(default, deserialize_with = "null_default")]
    edition: String,
    manifest_path: PathBuf,
    #[serde(default, deserialize_with = "null_default")]
    targets: Vec<RawTarget>,
    #[serde(default, deserialize_with = "null_default")]
    dependencies: Vec<RawDependency>,
    #[serde(default, deserialize_with = "null_default")]
    features: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct RawTarget {
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    kind: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    crate_types: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    src_path: PathBuf,
    #[serde(default, deserialize_with = "null_default", rename = "required-features", alias = "required_features")]
    required_features: Vec<String>,
}

#[derive(Deserialize)]
struct RawDependency {
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    req: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    optional: bool,
}

impl From<RawMetadata> for CargoMetadata {
    fn from(raw: RawMetadata) -> Self {
        Self {
            workspace_root: raw.workspace_root,
            target_directory: raw.target_directory,
            packages: raw.packages.into_iter().map(Package::from).collect(),
            workspace_members: raw.workspace_members,
        }
    }
}

impl From<RawPackage> for Package {
    fn from(raw: RawPackage) -> Self {
        Self {
            id: raw.id,
            name: raw.name,
            version: raw.version,
            edition: if raw.edition.is_empty() { DEFAULT_EDITION.to_owned() } else { raw.edition },
            manifest_path: raw.manifest_path,
            targets: raw.targets.into_iter().map(Target::from).collect(),
            dependencies: raw.dependencies.into_iter().map(Dependency::from).collect(),
            features: raw.features,
        }
    }
}

impl From<RawTarget> for Target {
    fn from(raw: RawTarget) -> Self {
        Self {
            name: raw.name,
            kind: raw.kind,
            crate_types: raw.crate_types,
            src_path: raw.src_path,
            required_features: raw.required_features,
        }
    }
}

impl From<RawDependency> for Dependency {
    fn from(raw: RawDependency) -> Self {
        Self { name: raw.name, req: raw.req, kind: raw.kind.filter(|k| !k.is_empty()), optional: raw.optional }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-member workspace (`app` depends on `core` by path and on `serde` from crates.io) in the
    /// shape `cargo metadata --format-version 1` prints, including fields the model ignores.
    const WORKSPACE: &str = r#"{
      "packages": [
        {
          "name": "serde", "version": "1.0.200",
          "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.200",
          "license": "MIT OR Apache-2.0", "license_file": null, "description": "A serialization framework",
          "source": "registry+https://github.com/rust-lang/crates.io-index",
          "dependencies": [
            {"name": "serde_derive", "source": "registry+https://github.com/rust-lang/crates.io-index",
             "req": "^1", "kind": null, "rename": null, "optional": true, "uses_default_features": true,
             "features": [], "target": null, "registry": null}
          ],
          "targets": [
            {"kind": ["lib"], "crate_types": ["lib"], "name": "serde",
             "src_path": "/home/u/.cargo/registry/src/serde-1.0.200/src/lib.rs",
             "edition": "2018", "doc": true, "doctest": true, "test": true},
            {"kind": ["custom-build"], "crate_types": ["bin"], "name": "build-script-build",
             "src_path": "/home/u/.cargo/registry/src/serde-1.0.200/build.rs",
             "edition": "2018", "doc": false, "doctest": false, "test": false}
          ],
          "features": {"default": ["std"], "derive": ["serde_derive"], "std": []},
          "manifest_path": "/home/u/.cargo/registry/src/serde-1.0.200/Cargo.toml",
          "metadata": null, "publish": null, "authors": ["Erick Tryzelaar", "David Tolnay"],
          "categories": [], "keywords": [], "readme": "README.md", "repository": null, "homepage": null,
          "documentation": null, "edition": "2018", "links": null, "default_run": null, "rust_version": "1.31"
        },
        {
          "name": "app", "version": "0.1.0", "id": "path+file:///work/ws/app#0.1.0",
          "license": null, "source": null,
          "dependencies": [
            {"name": "core", "source": null, "req": "*", "kind": null, "rename": null, "optional": false,
             "uses_default_features": true, "features": [], "target": null, "registry": null,
             "path": "/work/ws/core"},
            {"name": "serde", "source": "registry+https://github.com/rust-lang/crates.io-index",
             "req": "^1.0", "kind": null, "rename": null, "optional": true, "uses_default_features": true,
             "features": ["derive"], "target": null, "registry": null},
            {"name": "tempfile", "source": "registry+https://github.com/rust-lang/crates.io-index",
             "req": "^3", "kind": "dev", "rename": null, "optional": false, "uses_default_features": true,
             "features": [], "target": null, "registry": null},
            {"name": "cc", "source": "registry+https://github.com/rust-lang/crates.io-index",
             "req": "^1", "kind": "build", "rename": null, "optional": false, "uses_default_features": true,
             "features": [], "target": null, "registry": null}
          ],
          "targets": [
            {"kind": ["bin"], "crate_types": ["bin"], "name": "app", "src_path": "/work/ws/app/src/main.rs",
             "edition": "2021", "doc": true, "doctest": false, "test": true},
            {"kind": ["example"], "crate_types": ["bin"], "name": "demo",
             "src_path": "/work/ws/app/examples/demo.rs", "edition": "2021", "required-features": ["serde"],
             "doc": false, "doctest": false, "test": false},
            {"kind": ["test"], "crate_types": ["bin"], "name": "smoke", "src_path": "/work/ws/app/tests/smoke.rs",
             "edition": "2021", "doc": false, "doctest": false, "test": true},
            {"kind": ["bench"], "crate_types": ["bin"], "name": "speed", "src_path": "/work/ws/app/benches/speed.rs",
             "edition": "2021", "doc": false, "doctest": false, "test": false}
          ],
          "features": {"serde": ["dep:serde"]},
          "manifest_path": "/work/ws/app/Cargo.toml",
          "metadata": null, "publish": null, "authors": [], "categories": [], "keywords": [], "readme": null,
          "repository": null, "homepage": null, "documentation": null, "edition": "2021", "links": null,
          "default_run": null, "rust_version": null
        },
        {
          "name": "core", "version": "0.2.0", "id": "path+file:///work/ws/core#0.2.0",
          "source": null, "dependencies": [],
          "targets": [
            {"kind": ["lib"], "crate_types": ["lib"], "name": "core", "src_path": "/work/ws/core/src/lib.rs",
             "edition": "2015", "doc": true, "doctest": true, "test": true},
            {"kind": ["proc-macro"], "crate_types": ["proc-macro"], "name": "core_macros",
             "src_path": "/work/ws/core/src/macros.rs", "doc": true, "doctest": true, "test": true}
          ],
          "features": {},
          "manifest_path": "/work/ws/core/Cargo.toml"
        }
      ],
      "workspace_members": ["path+file:///work/ws/app#0.1.0", "path+file:///work/ws/core#0.2.0"],
      "workspace_default_members": ["path+file:///work/ws/app#0.1.0"],
      "resolve": {"nodes": [], "root": null},
      "target_directory": "/work/ws/target",
      "build_directory": "/work/ws/target",
      "version": 1,
      "workspace_root": "/work/ws",
      "metadata": null
    }"#;

    #[test]
    fn parses_workspace_metadata() {
        let meta = parse_metadata(WORKSPACE).unwrap();
        assert_eq!(meta.workspace_root, PathBuf::from("/work/ws"));
        assert_eq!(meta.target_directory, PathBuf::from("/work/ws/target"));
        assert_eq!(meta.packages.len(), 3);
        assert_eq!(meta.workspace_members.len(), 2);

        let members: Vec<&str> = meta.workspace_packages().map(|p| p.name.as_str()).collect();
        assert_eq!(members, ["app", "core"]);

        let app = &meta.packages[1];
        assert_eq!(app.id, "path+file:///work/ws/app#0.1.0");
        assert_eq!(app.version, "0.1.0");
        assert_eq!(app.edition, "2021");
        assert_eq!(app.manifest_path, PathBuf::from("/work/ws/app/Cargo.toml"));
        assert_eq!(app.features.get("serde"), Some(&vec!["dep:serde".to_owned()]));
        assert_eq!(
            app.dependencies,
            vec![
                Dependency { name: "core".into(), req: "*".into(), kind: None, optional: false },
                Dependency { name: "serde".into(), req: "^1.0".into(), kind: None, optional: true },
                Dependency { name: "tempfile".into(), req: "^3".into(), kind: Some("dev".into()), optional: false },
                Dependency { name: "cc".into(), req: "^1".into(), kind: Some("build".into()), optional: false },
            ]
        );
        let kinds: Vec<(&str, bool, bool, bool, bool)> = app
            .targets
            .iter()
            .map(|t| (t.name.as_str(), t.is_bin(), t.is_example(), t.is_test(), t.is_bench()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("app", true, false, false, false),
                ("demo", false, true, false, false),
                ("smoke", false, false, true, false),
                ("speed", false, false, false, true),
            ]
        );
        assert_eq!(app.targets[1].required_features, ["serde"]);
        assert_eq!(app.targets[0].crate_types, ["bin"]);
        assert_eq!(app.targets[0].src_path, PathBuf::from("/work/ws/app/src/main.rs"));

        let serde = &meta.packages[0];
        assert_eq!(serde.edition, "2018");
        assert!(serde.targets[0].is_lib());
        assert!(serde.targets[1].is_build_script());
        assert_eq!(serde.dependencies[0].kind, None);
        assert!(serde.dependencies[0].optional);

        let core = &meta.packages[2];
        assert!(core.targets.iter().all(Target::is_lib));
        assert!(core.targets[1].required_features.is_empty());
    }

    #[test]
    fn package_for_file_picks_the_member() {
        let meta = parse_metadata(WORKSPACE).unwrap();
        let name = |p: &str| meta.package_for_file(Path::new(p)).map(|p| p.name.clone());
        assert_eq!(name("/work/ws/app/src/main.rs").as_deref(), Some("app"));
        assert_eq!(name("/work/ws/core/src/lib.rs").as_deref(), Some("core"));
        // Registry packages are not workspace members.
        assert_eq!(name("/home/u/.cargo/registry/src/serde-1.0.200/src/lib.rs"), None);
        assert_eq!(name("/elsewhere/main.rs"), None);
    }

    #[test]
    fn missing_edition_defaults_to_2015() {
        let json = r#"{"workspace_root": "/p", "workspace_members": ["x"], "packages": [
            {"id": "x", "name": "x", "version": "0.1.0", "manifest_path": "/p/Cargo.toml", "targets": [],
             "dependencies": [], "features": {}},
            {"id": "y", "name": "y", "version": "0.1.0", "edition": null, "manifest_path": "/p/y/Cargo.toml"}
        ]}"#;
        let meta = parse_metadata(json).unwrap();
        assert_eq!(meta.packages[0].edition, "2015");
        assert_eq!(meta.packages[1].edition, "2015");
        assert!(meta.packages[1].targets.is_empty());
        assert_eq!(meta.target_directory, PathBuf::new());
    }

    #[test]
    fn invalid_metadata_is_a_parse_error() {
        for bad in
            ["", "not json", "{}", "[]", r#"{"workspace_root": "/p"}"#, r#"{"workspace_root": 1, "packages": []}"#]
        {
            assert!(matches!(parse_metadata(bad), Err(CargoError::Parse(_))), "{bad:?} should not parse");
        }
        let deep = "[".repeat(100_000);
        assert!(matches!(parse_metadata(&deep), Err(CargoError::Parse(_))));
    }

    #[test]
    fn surrounding_whitespace_is_fine() {
        let meta = parse_metadata("\n  {\"workspace_root\": \"/p\", \"packages\": []}\n").unwrap();
        assert_eq!(meta.workspace_root, PathBuf::from("/p"));
        assert!(meta.workspace_members.is_empty());
    }
}
