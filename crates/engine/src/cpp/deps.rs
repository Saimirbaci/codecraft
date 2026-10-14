//! Conan and vcpkg (track CL): detect `conanfile.txt` / `conanfile.py` / `vcpkg.json`, show the
//! declared packages, install them as a background job with a log (`conan install .
//! --build=missing -of <build dir>`, `vcpkg install`), and pass the toolchain file to CMake
//! (`conan_toolchain.cmake`, `<vcpkg root>/scripts/buildsystems/vcpkg.cmake`) through the
//! project's CMake settings, so the next reload uses it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{NotificationLevel, ToolWindowId};

use super::jobs::JobKind;
use crate::commands::{CmdResult, CommandSpec, Context, has_project, spec};
use crate::params::opt_str;
use crate::{CommandError, Session};

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    Conan,
    Vcpkg,
}

impl Manager {
    pub fn key(self) -> &'static str {
        match self {
            Self::Conan => "conan",
            Self::Vcpkg => "vcpkg",
        }
    }
}

/// A declared dependency.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: Option<String>,
    /// `requires`, `tool_requires`, `test_requires`, `dependencies`, …
    pub section: String,
    pub features: Vec<String>,
}

/// The project's dependency manifests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifests {
    pub conan: Option<PathBuf>,
    pub vcpkg: Option<PathBuf>,
    pub packages: Vec<(Manager, Package)>,
    pub errors: Vec<String>,
}

fn read(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `name/version[@user/channel]` → package.
fn conan_ref(text: &str, section: &str) -> Option<Package> {
    let t = text.trim().trim_matches(['"', '\'']).trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let t = t.split('#').next().unwrap_or(t).trim();
    let (name, rest) = t.split_once('/').unwrap_or((t, ""));
    let version = rest.split('@').next().filter(|v| !v.is_empty()).map(str::to_string);
    let ok = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
    ok.then(|| Package { name: name.to_string(), version, section: section.to_string(), features: Vec::new() })
}

/// `conanfile.txt`: `[requires]`, `[tool_requires]`, `[test_requires]` sections.
pub fn parse_conanfile_txt(text: &str) -> Vec<Package> {
    let mut out = Vec::new();
    let mut section = String::new();
    for line in text.lines().take(100_000) {
        let t = line.trim();
        if let Some(s) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = s.trim().to_string();
            continue;
        }
        if matches!(section.as_str(), "requires" | "tool_requires" | "build_requires" | "test_requires")
            && let Some(p) = conan_ref(t, &section)
        {
            out.push(p);
        }
    }
    out
}

/// `conanfile.py`: `requires = "a/1", "b/2"` (or a list/tuple) and `self.requires("x/1")` calls.
pub fn parse_conanfile_py(text: &str) -> Vec<Package> {
    let mut out: Vec<Package> = Vec::new();
    let mut add = |p: Option<Package>| {
        if let Some(p) = p
            && !out.iter().any(|o| o.name == p.name && o.section == p.section)
        {
            out.push(p);
        }
    };
    let quoted = |s: &str| -> Vec<String> {
        let mut v = Vec::new();
        let mut rest = s;
        while let Some(i) = rest.find(['"', '\'']) {
            let q = rest.get(i..i + 1).unwrap_or("\"");
            let after = rest.get(i + 1..).unwrap_or("");
            let Some(end) = after.find(q) else { break };
            v.push(after.get(..end).unwrap_or("").to_string());
            rest = after.get(end + 1..).unwrap_or("");
        }
        v
    };
    let lines: Vec<&str> = text.lines().take(100_000).collect();
    let mut i = 0;
    while let Some(line) = lines.get(i) {
        let t = line.trim();
        for (attr, section) in [("requires", "requires"), ("tool_requires", "tool_requires"), ("build_requires", "tool_requires"), ("test_requires", "test_requires")] {
            if let Some(rest) = t.strip_prefix(attr).map(str::trim_start).and_then(|r| r.strip_prefix('=')) {
                // A value may continue over the following lines of a list or tuple.
                let mut value = rest.to_string();
                let opens = value.matches(['(', '[']).count();
                let mut closes = value.matches([')', ']']).count();
                let mut j = i + 1;
                while opens > closes && j < lines.len() && j < i + 200 {
                    let next = lines.get(j).copied().unwrap_or("");
                    closes += next.matches([')', ']']).count();
                    value.push_str(next);
                    j += 1;
                }
                for r in quoted(&value) {
                    add(conan_ref(&r, section));
                }
            }
            let call = format!("self.{attr}(");
            if let Some(pos) = t.find(&call) {
                let arg = t.get(pos + call.len()..).unwrap_or("");
                if let Some(first) = quoted(arg).into_iter().next() {
                    add(conan_ref(&first, section));
                }
            }
        }
        i += 1;
    }
    out
}

/// `vcpkg.json`: `dependencies` (names or objects with `name`, `version>=`, `features`).
pub fn parse_vcpkg_json(text: &str) -> Result<Vec<Package>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("vcpkg.json: {e}"))?;
    let mut out = Vec::new();
    let overrides = v.get("overrides").and_then(Value::as_array);
    for d in v.get("dependencies").and_then(Value::as_array).into_iter().flatten().take(10_000) {
        let (name, version, features) = match d {
            Value::String(n) => (n.clone(), None, Vec::new()),
            Value::Object(o) => {
                let Some(n) = o.get("name").and_then(Value::as_str) else { continue };
                let version = o.get("version>=").and_then(Value::as_str).map(|v| format!(">={v}"));
                let features = o
                    .get("features")
                    .and_then(Value::as_array)
                    .map(|f| {
                        f.iter()
                            .filter_map(|x| x.as_str().or_else(|| x.get("name").and_then(Value::as_str)))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                (n.to_string(), version, features)
            }
            _ => continue,
        };
        let version = version.or_else(|| {
            overrides
                .into_iter()
                .flatten()
                .find(|o| o.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .and_then(|o| o.get("version").and_then(Value::as_str))
                .map(|v| format!("={v}"))
        });
        out.push(Package { name, version, section: "dependencies".into(), features });
    }
    Ok(out)
}

/// Find and read the project's manifests.
pub fn detect(root: &Path) -> Manifests {
    let mut m = Manifests::default();
    for (name, py) in [("conanfile.py", true), ("conanfile.txt", false)] {
        let p = root.join(name);
        if p.is_file() {
            match read(&p) {
                Ok(text) => {
                    let pkgs = if py { parse_conanfile_py(&text) } else { parse_conanfile_txt(&text) };
                    m.packages.extend(pkgs.into_iter().map(|p| (Manager::Conan, p)));
                }
                Err(e) => m.errors.push(e),
            }
            m.conan = Some(p);
            break;
        }
    }
    let v = root.join("vcpkg.json");
    if v.is_file() {
        match read(&v).and_then(|t| parse_vcpkg_json(&t)) {
            Ok(pkgs) => m.packages.extend(pkgs.into_iter().map(|p| (Manager::Vcpkg, p))),
            Err(e) => m.errors.push(e),
        }
        m.vcpkg = Some(v);
    }
    m
}

/// `conan_toolchain.cmake` under the build directory (Conan 2 puts it in the output folder or
/// `build/<type>/generators`).
pub fn find_conan_toolchain(build_dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![(build_dir.to_path_buf(), 0usize)];
    let mut visited = 0;
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > 500 {
            break;
        }
        let candidate = dir.join("conan_toolchain.cmake");
        if candidate.is_file() {
            return Some(candidate);
        }
        if depth >= 4 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut subdirs: Vec<PathBuf> = rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
        subdirs.sort();
        for d in subdirs.into_iter().rev() {
            if !d.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                stack.push((d, depth + 1));
            }
        }
    }
    None
}

/// vcpkg's CMake toolchain next to the vcpkg executable (`<root>/scripts/buildsystems/vcpkg.cmake`).
pub fn vcpkg_toolchain(vcpkg: &Path) -> Option<PathBuf> {
    let root = std::env::var_os("VCPKG_ROOT")
        .map(PathBuf::from)
        .filter(|r| r.join("scripts/buildsystems/vcpkg.cmake").is_file())
        .or_else(|| {
            let real = std::fs::canonicalize(vcpkg).unwrap_or_else(|_| vcpkg.to_path_buf());
            real.parent().map(Path::to_path_buf)
        })?;
    let t = root.join("scripts").join("buildsystems").join("vcpkg.cmake");
    t.is_file().then_some(t)
}

#[derive(Default)]
pub struct DepsState {
    pub manifests: Option<Manifests>,
    /// The last install job per manager.
    pub jobs: Vec<(Manager, u64)>,
}

fn manifests(s: &mut Session) -> Result<(PathBuf, Manifests), CommandError> {
    let root = super::project_root(s)?;
    let m = detect(&root);
    s.cpp.deps.manifests = Some(m.clone());
    Ok((root, m))
}

fn install(s: &mut Session, p: &Value) -> CmdResult {
    let (root, m) = manifests(s)?;
    let manager = match opt_str(p, "manager") {
        Some("conan") => Manager::Conan,
        Some("vcpkg") => Manager::Vcpkg,
        Some(other) => return Err(CommandError::BadParams(format!("unknown package manager `{other}` (conan, vcpkg)"))),
        None if m.conan.is_some() => Manager::Conan,
        None if m.vcpkg.is_some() => Manager::Vcpkg,
        None => return Err(CommandError::Failed("no conanfile.txt, conanfile.py or vcpkg.json in the project".into())),
    };
    let profile = super::cmake::active_profile(&root);
    let build_dir = profile.as_ref().map(|p| p.build_dir.clone()).unwrap_or_else(|| root.join(crate::run_configs::CMAKE_BUILD_DIR));
    let (program, args) = match manager {
        Manager::Conan => {
            if m.conan.is_none() {
                return Err(CommandError::Failed("no conanfile in the project".into()));
            }
            let conan = super::tool(s, "conan").map_err(CommandError::Failed)?;
            let mut args = vec!["install".to_string(), ".".into(), "--build=missing".into(), "-of".into(), build_dir.display().to_string()];
            if let Some(t) = profile.as_ref().and_then(|p| p.build_type.clone()) {
                args.push("-s".into());
                args.push(format!("build_type={t}"));
            }
            (conan, args)
        }
        Manager::Vcpkg => {
            if m.vcpkg.is_none() {
                return Err(CommandError::Failed("no vcpkg.json in the project".into()));
            }
            let vcpkg = super::tool(s, "vcpkg").map_err(CommandError::Failed)?;
            (vcpkg, vec!["install".to_string()])
        }
    };
    let id = s
        .cpp
        .jobs
        .start(JobKind::Dependencies { manager: manager.key().into() }, &format!("{} install", manager.key()), &program, &args, &root, &[], None)
        .map_err(CommandError::Failed)?;
    s.cpp.deps.jobs.retain(|(m, _)| *m != manager);
    s.cpp.deps.jobs.push((manager, id));
    s.cpp.cmake.tab = super::CmakeTab::Dependencies;
    s.ws.layout.show(ToolWindowId::CMake);
    s.status = format!("Installing {} dependencies…", manager.key());
    Ok(json!({ "job": id, "manager": manager }))
}

/// After an install: remember the toolchain file for CMake.
pub(crate) fn install_finished(s: &mut Session, id: u64, manager: &str, ok: bool) {
    let Some(root) = s.project_root() else { return };
    if !ok {
        let stopped = s.cpp.jobs.get(id).is_some_and(|j| j.status == super::jobs::JobStatus::Stopped);
        if !stopped {
            s.notify(NotificationLevel::Error, &format!("{manager} install failed"), "see the CMake tool window");
        }
        return;
    }
    let build_dir = super::cmake::active_profile(&root)
        .map(|p| p.build_dir)
        .unwrap_or_else(|| root.join(crate::run_configs::CMAKE_BUILD_DIR));
    let toolchain = match manager {
        "conan" => find_conan_toolchain(&build_dir),
        _ => super::tool(s, "vcpkg").ok().and_then(|v| vcpkg_toolchain(&v)),
    };
    match toolchain {
        Some(t) => {
            let mut settings = super::cmake::load_settings(&root);
            settings.toolchain_file = Some(t.display().to_string());
            if let Err(e) = super::cmake::save_settings(&root, &settings) {
                s.notify(NotificationLevel::Error, "CMake settings", e);
                return;
            }
            s.cpp.cmake.root = None; // profiles pick up the toolchain on the next pump
            s.cpp.cmake.needs_reload = true;
            s.notify(
                NotificationLevel::Info,
                &format!("{manager} dependencies installed"),
                format!("CMake will use {} (Reload CMake Project)", t.display()),
            );
        }
        None => s.notify(NotificationLevel::Info, &format!("{manager} dependencies installed"), "no CMake toolchain file was found"),
    }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "cpp.dependencies",
            "C++ Dependencies",
            None,
            "The project's Conan (conanfile.txt/.py) and vcpkg (vcpkg.json) manifests and declared packages",
            Context::Global,
            has_project,
            |s, _| {
                let (_, m) = manifests(s)?;
                let jobs: Vec<Value> = s
                    .cpp
                    .deps
                    .jobs
                    .iter()
                    .filter_map(|(mgr, id)| s.cpp.jobs.get(*id).map(|j| json!({ "manager": mgr, "job": id, "status": j.status })))
                    .collect();
                let toolchain = s.project_root().map(|r| super::cmake::load_settings(&r).toolchain_file);
                Ok(json!({ "manifests": m, "jobs": jobs, "toolchainFile": toolchain }))
            },
        ),
        spec(
            "cpp.dependencies.install",
            "Install C++ Dependencies",
            None,
            "{manager?: conan|vcpkg} — `conan install . --build=missing -of <build dir>` or `vcpkg install` as a \
             background job (log in the CMake tool window); the toolchain file is then passed to CMake",
            Context::Global,
            has_project,
            install,
        ),
        spec(
            "cpp.dependencies.clearToolchain",
            "Clear CMake Toolchain File",
            None,
            "Stop passing the Conan/vcpkg toolchain file to CMake",
            Context::Global,
            has_project,
            |s, _| {
                let root = super::project_root(s)?;
                let mut settings = super::cmake::load_settings(&root);
                settings.toolchain_file = None;
                super::cmake::save_settings(&root, &settings).map_err(CommandError::Failed)?;
                s.cpp.cmake.root = None;
                crate::commands::ok()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conanfile_txt() {
        let p = parse_conanfile_txt(
            "[requires]\nzlib/1.3.1\nfmt/10.2.1@user/stable # pinned\n\n[tool_requires]\ncmake/3.30.0\n[generators]\nCMakeToolchain\n",
        );
        let got: Vec<(&str, Option<&str>, &str)> = p.iter().map(|x| (x.name.as_str(), x.version.as_deref(), x.section.as_str())).collect();
        assert_eq!(got, vec![("zlib", Some("1.3.1"), "requires"), ("fmt", Some("10.2.1"), "requires"), ("cmake", Some("3.30.0"), "tool_requires")]);
    }

    #[test]
    fn conanfile_py() {
        let p = parse_conanfile_py(
            "from conan import ConanFile\nclass App(ConanFile):\n    requires = (\"zlib/1.3.1\",\n        \"boost/1.85.0\")\n\
             tool_requires = 'ninja/1.12.1'\n    def requirements(self):\n        self.requires(\"fmt/10.2.1\")\n        self.test_requires(\"gtest/1.14.0\")\n",
        );
        let got: Vec<(&str, &str)> = p.iter().map(|x| (x.name.as_str(), x.section.as_str())).collect();
        assert_eq!(got, vec![("zlib", "requires"), ("boost", "requires"), ("ninja", "tool_requires"), ("fmt", "requires"), ("gtest", "test_requires")]);
    }

    #[test]
    fn vcpkg_manifest() {
        let p = parse_vcpkg_json(
            r#"{"name": "app", "dependencies": ["fmt", {"name": "boost-asio", "version>=": "1.85.0", "features": ["ssl", {"name": "x"}]}, 5, {}],
                "overrides": [{"name": "fmt", "version": "10.1.0"}]}"#,
        )
        .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].version.as_deref(), Some("=10.1.0"));
        assert_eq!(p[1].version.as_deref(), Some(">=1.85.0"));
        assert_eq!(p[1].features, vec!["ssl", "x"]);
        assert!(parse_vcpkg_json("{").is_err());
    }

    #[test]
    fn detection_and_toolchains() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(detect(root), Manifests::default());
        std::fs::write(root.join("conanfile.txt"), "[requires]\nzlib/1.3\n").unwrap();
        std::fs::write(root.join("vcpkg.json"), "{\"dependencies\": [\"fmt\"]}").unwrap();
        let m = detect(root);
        assert!(m.conan.is_some() && m.vcpkg.is_some());
        assert_eq!(m.packages.len(), 2);
        let generators = root.join("build/build/Debug/generators");
        std::fs::create_dir_all(&generators).unwrap();
        assert_eq!(find_conan_toolchain(&root.join("build")), None);
        std::fs::write(generators.join("conan_toolchain.cmake"), "").unwrap();
        assert_eq!(find_conan_toolchain(&root.join("build")), Some(generators.join("conan_toolchain.cmake")));
        let vroot = root.join("vcpkg");
        std::fs::create_dir_all(vroot.join("scripts/buildsystems")).unwrap();
        std::fs::write(vroot.join("scripts/buildsystems/vcpkg.cmake"), "").unwrap();
        std::fs::write(vroot.join("vcpkg"), "").unwrap();
        if std::env::var_os("VCPKG_ROOT").is_none() {
            let got = vcpkg_toolchain(&vroot.join("vcpkg")).map(|p| p.canonicalize().unwrap());
            assert_eq!(got, Some(vroot.join("scripts/buildsystems/vcpkg.cmake").canonicalize().unwrap()));
        }
        for bad in ["[requires]\n/\n@\n", "", "requires = (\"unterminated"] {
            let _ = parse_conanfile_txt(bad);
            let _ = parse_conanfile_py(bad);
        }
    }
}
