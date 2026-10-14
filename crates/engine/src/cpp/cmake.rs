//! CMake (track CL): `CMakePresets.json` / `CMakeUserPresets.json` (configure, build and test
//! presets with inheritance and `${macro}` expansion), CMake profiles (Debug, Release and one per
//! configure preset, like CLion) with the selected one kept in `<project>/.codecraft/cmake.json`,
//! the CMake File API (`.cmake/api/v1`: our stateful query, the `codemodel-v2` reply's targets
//! with their artifacts and sources), `CMakeCache.txt` and `ctest --show-only=json-v1`.
//!
//! Everything here is pure or plain file I/O with size caps; the commands and the reload job are
//! in `cmake_cmds.rs`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::testing::Framework;

/// The selected profile (and toolchain from Conan/vcpkg), relative to the project root.
pub const SETTINGS_FILE: &str = ".codecraft/cmake.json";
/// Our File API client name (`.cmake/api/v1/query/client-codecraft/query.json`).
pub const QUERY_CLIENT: &str = "client-codecraft";
/// Largest JSON or cache file read.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TARGETS: usize = 5_000;
const MAX_SOURCES: usize = 20_000;
const MAX_PRESETS: usize = 1_000;
const MAX_INCLUDES: usize = 64;

fn read_capped(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("{} is too large ({} bytes)", path.display(), meta.len()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------- presets

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurePreset {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub hidden: bool,
    pub inherits: Vec<String>,
    pub generator: Option<String>,
    pub binary_dir: Option<String>,
    pub install_dir: Option<String>,
    pub toolchain_file: Option<String>,
    pub cache_variables: Vec<(String, String)>,
    pub environment: Vec<(String, String)>,
    /// The file that defined it.
    pub file: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildPreset {
    pub name: String,
    pub display_name: Option<String>,
    pub hidden: bool,
    pub inherits: Vec<String>,
    pub configure_preset: Option<String>,
    pub configuration: Option<String>,
    pub targets: Vec<String>,
    pub jobs: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestPreset {
    pub name: String,
    pub display_name: Option<String>,
    pub hidden: bool,
    pub inherits: Vec<String>,
    pub configure_preset: Option<String>,
    pub configuration: Option<String>,
}

/// The presets of a project, inheritance resolved and macros expanded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presets {
    pub configure: Vec<ConfigurePreset>,
    pub build: Vec<BuildPreset>,
    pub test: Vec<TestPreset>,
    /// Problems reading them (bad JSON, unknown parents, cycles).
    pub errors: Vec<String>,
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `cacheVariables` values: a string, a bool, or `{type, value}`; `null` unsets an inherited one.
fn cache_vars(v: Option<&Value>) -> Vec<(String, Option<String>)> {
    let Some(map) = v.and_then(Value::as_object) else { return Vec::new() };
    map.iter()
        .map(|(k, v)| {
            let value = match v {
                Value::String(s) => Some(s.clone()),
                Value::Bool(b) => Some(if *b { "TRUE".into() } else { "FALSE".into() }),
                Value::Object(o) => o.get("value").map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
                    other => other.to_string(),
                }),
                Value::Null => None,
                other => Some(other.to_string()),
            };
            (k.clone(), value)
        })
        .collect()
}

fn env_vars(v: Option<&Value>) -> Vec<(String, Option<String>)> {
    let Some(map) = v.and_then(Value::as_object) else { return Vec::new() };
    map.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect()
}

/// One configure preset as written (before inheritance).
#[derive(Clone, Debug, Default)]
struct RawConfigure {
    preset: ConfigurePreset,
    hidden: Option<bool>,
    cache: Vec<(String, Option<String>)>,
    env: Vec<(String, Option<String>)>,
}

#[derive(Default)]
struct RawFiles {
    configure: Vec<RawConfigure>,
    build: Vec<(BuildPreset, Value)>,
    test: Vec<(TestPreset, Value)>,
    errors: Vec<String>,
}

/// Read one presets file and the files it includes (bounded).
fn read_presets_file(path: &Path, raw: &mut RawFiles, seen: &mut Vec<PathBuf>) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if seen.contains(&canonical) {
        return;
    }
    if seen.len() >= MAX_INCLUDES {
        raw.errors.push("too many included presets files".into());
        return;
    }
    seen.push(canonical);
    let text = match read_capped(path) {
        Ok(t) => t,
        Err(e) => {
            raw.errors.push(e);
            return;
        }
    };
    parse_presets_text(&text, path, raw);
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return };
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    for inc in str_list(v.get("include")) {
        let p = PathBuf::from(&inc);
        let p = if p.is_absolute() { p } else { dir.join(p) };
        read_presets_file(&p, raw, seen);
    }
}

fn parse_presets_text(text: &str, file: &Path, raw: &mut RawFiles) {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            raw.errors.push(format!("{}: {e}", file.display()));
            return;
        }
    };
    for p in v.get("configurePresets").and_then(Value::as_array).into_iter().flatten().take(MAX_PRESETS) {
        let Some(name) = opt_str(p, "name") else { continue };
        let binary_dir = opt_str(p, "binaryDir");
        raw.configure.push(RawConfigure {
            preset: ConfigurePreset {
                name,
                display_name: opt_str(p, "displayName"),
                description: opt_str(p, "description"),
                hidden: false,
                inherits: str_list(p.get("inherits")),
                generator: opt_str(p, "generator"),
                binary_dir,
                install_dir: opt_str(p, "installDir"),
                toolchain_file: opt_str(p, "toolchainFile"),
                cache_variables: Vec::new(),
                environment: Vec::new(),
                file: file.to_path_buf(),
            },
            hidden: p.get("hidden").and_then(Value::as_bool),
            cache: cache_vars(p.get("cacheVariables")),
            env: env_vars(p.get("environment")),
        });
    }
    for p in v.get("buildPresets").and_then(Value::as_array).into_iter().flatten().take(MAX_PRESETS) {
        let Some(name) = opt_str(p, "name") else { continue };
        raw.build.push((
            BuildPreset {
                name,
                display_name: opt_str(p, "displayName"),
                hidden: p.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                inherits: str_list(p.get("inherits")),
                configure_preset: opt_str(p, "configurePreset"),
                configuration: opt_str(p, "configuration"),
                targets: str_list(p.get("targets")),
                jobs: p.get("jobs").and_then(Value::as_u64),
            },
            p.clone(),
        ));
    }
    for p in v.get("testPresets").and_then(Value::as_array).into_iter().flatten().take(MAX_PRESETS) {
        let Some(name) = opt_str(p, "name") else { continue };
        raw.test.push((
            TestPreset {
                name,
                display_name: opt_str(p, "displayName"),
                hidden: p.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                inherits: str_list(p.get("inherits")),
                configure_preset: opt_str(p, "configurePreset"),
                configuration: opt_str(p, "configuration"),
            },
            p.clone(),
        ));
    }
}

/// Apply inheritance to configure preset `i` (depth first; the first parent wins over later
/// ones, the preset itself over all of them). `stack` detects cycles.
fn resolve_configure(
    i: usize,
    raw: &[RawConfigure],
    done: &mut HashMap<usize, RawConfigure>,
    stack: &mut Vec<usize>,
    errors: &mut Vec<String>,
) -> Option<RawConfigure> {
    if let Some(r) = done.get(&i) {
        return Some(r.clone());
    }
    let me = raw.get(i)?;
    if stack.contains(&i) || stack.len() > 64 {
        errors.push(format!("preset `{}` inherits from itself", me.preset.name));
        return None;
    }
    stack.push(i);
    let mut out = me.clone();
    for parent in &me.preset.inherits {
        let Some(pi) = raw.iter().position(|r| &r.preset.name == parent) else {
            errors.push(format!("preset `{}` inherits from unknown preset `{parent}`", me.preset.name));
            continue;
        };
        // A cycle invalidates every preset on it (CMake rejects them too).
        let Some(p) = resolve_configure(pi, raw, done, stack, errors) else {
            stack.pop();
            return None;
        };
        let fill = |mine: &mut Option<String>, theirs: &Option<String>| {
            if mine.is_none() {
                mine.clone_from(theirs);
            }
        };
        fill(&mut out.preset.generator, &p.preset.generator);
        fill(&mut out.preset.binary_dir, &p.preset.binary_dir);
        fill(&mut out.preset.install_dir, &p.preset.install_dir);
        fill(&mut out.preset.toolchain_file, &p.preset.toolchain_file);
        fill(&mut out.preset.display_name, &p.preset.display_name);
        for (k, v) in &p.cache {
            if !out.cache.iter().any(|(o, _)| o == k) {
                out.cache.push((k.clone(), v.clone()));
            }
        }
        for (k, v) in &p.env {
            if !out.env.iter().any(|(o, _)| o == k) {
                out.env.push((k.clone(), v.clone()));
            }
        }
    }
    stack.pop();
    // `hidden` is not inherited.
    out.preset.hidden = me.hidden.unwrap_or(false);
    done.insert(i, out.clone());
    Some(out)
}

/// What `${…}` macros expand to.
pub struct MacroContext<'a> {
    pub source_dir: &'a Path,
    pub preset_name: &'a str,
    pub generator: Option<&'a str>,
    pub file_dir: &'a Path,
    pub env: &'a [(String, String)],
}

fn host_system_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        "windows" => "Windows",
        _ => "Unknown",
    }
}

/// Expand `${sourceDir}`, `${sourceParentDir}`, `${sourceDirName}`, `${presetName}`,
/// `${generator}`, `${hostSystemName}`, `${fileDir}`, `${dollar}`, `${pathListSep}`,
/// `$env{NAME}` (the preset's environment, then the process's) and `$penv{NAME}`. Unknown
/// macros stay as written.
pub fn expand_macros(value: &str, ctx: &MacroContext<'_>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find('$') {
        out.push_str(rest.get(..i).unwrap_or(""));
        let after = rest.get(i..).unwrap_or("");
        let (kind, body_start) = if after.starts_with("${") {
            ("", 2)
        } else if after.starts_with("$env{") {
            ("env", 5)
        } else if after.starts_with("$penv{") {
            ("penv", 6)
        } else {
            out.push('$');
            rest = after.get(1..).unwrap_or("");
            continue;
        };
        let Some(close) = after.find('}') else {
            out.push_str(after);
            return out;
        };
        let name = after.get(body_start..close).unwrap_or("");
        let expanded = match kind {
            "env" => ctx
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var(name).ok())
                .or(Some(String::new())),
            "penv" => std::env::var(name).ok().or(Some(String::new())),
            _ => match name {
                "sourceDir" => Some(ctx.source_dir.display().to_string()),
                "sourceParentDir" => ctx.source_dir.parent().map(|p| p.display().to_string()),
                "sourceDirName" => ctx.source_dir.file_name().map(|n| n.to_string_lossy().into_owned()),
                "presetName" => Some(ctx.preset_name.to_string()),
                "generator" => Some(ctx.generator.unwrap_or("").to_string()),
                "hostSystemName" => Some(host_system_name().to_string()),
                "fileDir" => Some(ctx.file_dir.display().to_string()),
                "dollar" => Some("$".into()),
                "pathListSep" => Some(if cfg!(windows) { ";" } else { ":" }.into()),
                _ => None,
            },
        };
        match expanded {
            Some(e) => out.push_str(&e),
            None => out.push_str(after.get(..=close).unwrap_or("")),
        }
        rest = after.get(close + 1..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// Load the project's presets (`CMakePresets.json`, then `CMakeUserPresets.json`, which
/// implicitly includes it), resolve inheritance and expand macros.
pub fn load_presets(root: &Path) -> Presets {
    let mut raw = RawFiles::default();
    let mut seen = Vec::new();
    for f in ["CMakePresets.json", "CMakeUserPresets.json"] {
        let p = root.join(f);
        if p.is_file() {
            read_presets_file(&p, &mut raw, &mut seen);
        }
    }
    let mut errors = std::mem::take(&mut raw.errors);
    let finish_errors = |errors: &mut Vec<String>| {
        let mut seen = Vec::new();
        errors.retain(|e| {
            let fresh = !seen.contains(e);
            seen.push(e.clone());
            fresh
        });
    };
    let mut done = HashMap::new();
    let mut configure = Vec::new();
    for i in 0..raw.configure.len() {
        let Some(r) = resolve_configure(i, &raw.configure, &mut done, &mut Vec::new(), &mut errors) else { continue };
        let env: Vec<(String, String)> =
            r.env.iter().filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone()))).collect();
        let file_dir = r.preset.file.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
        let ctx = MacroContext {
            source_dir: root,
            preset_name: &r.preset.name,
            generator: r.preset.generator.as_deref(),
            file_dir: &file_dir,
            env: &env,
        };
        let mut p = r.preset.clone();
        p.binary_dir = p.binary_dir.map(|d| expand_macros(&d, &ctx));
        p.install_dir = p.install_dir.map(|d| expand_macros(&d, &ctx));
        p.toolchain_file = p.toolchain_file.map(|d| expand_macros(&d, &ctx));
        p.cache_variables =
            r.cache.iter().filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), expand_macros(v, &ctx)))).collect();
        p.environment = env.iter().map(|(k, v)| (k.clone(), expand_macros(v, &ctx))).collect();
        configure.push(p);
    }
    // Build and test presets: plain field inheritance (first parent wins).
    let build_raw: Vec<BuildPreset> = raw.build.iter().map(|(b, _)| b.clone()).collect();
    let build = build_raw
        .iter()
        .map(|b| {
            let mut out = b.clone();
            let mut stack = vec![b.name.clone()];
            let mut parents: Vec<String> = b.inherits.clone();
            while let Some(parent) = parents.first().cloned() {
                parents.remove(0);
                if stack.contains(&parent) || stack.len() > 64 {
                    errors.push(format!("build preset `{}` has an inheritance cycle", b.name));
                    break;
                }
                stack.push(parent.clone());
                let Some(p) = build_raw.iter().find(|x| x.name == parent) else { continue };
                if out.configure_preset.is_none() {
                    out.configure_preset.clone_from(&p.configure_preset);
                }
                if out.configuration.is_none() {
                    out.configuration.clone_from(&p.configuration);
                }
                if out.targets.is_empty() {
                    out.targets.clone_from(&p.targets);
                }
                if out.jobs.is_none() {
                    out.jobs = p.jobs;
                }
                parents.extend(p.inherits.iter().cloned());
            }
            out
        })
        .collect();
    let test_raw: Vec<TestPreset> = raw.test.iter().map(|(t, _)| t.clone()).collect();
    let test = test_raw
        .iter()
        .map(|t| {
            let mut out = t.clone();
            let mut seen = vec![t.name.clone()];
            let mut parents = t.inherits.clone();
            while let Some(parent) = parents.first().cloned() {
                parents.remove(0);
                if seen.contains(&parent) || seen.len() > 64 {
                    break;
                }
                seen.push(parent.clone());
                let Some(p) = test_raw.iter().find(|x| x.name == parent) else { continue };
                if out.configure_preset.is_none() {
                    out.configure_preset.clone_from(&p.configure_preset);
                }
                if out.configuration.is_none() {
                    out.configuration.clone_from(&p.configuration);
                }
                parents.extend(p.inherits.iter().cloned());
            }
            out
        })
        .collect();
    finish_errors(&mut errors);
    Presets { configure, build, test, errors }
}

// ---------------------------------------------------------------- profiles

/// Per-project CMake settings (`.codecraft/cmake.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CmakeSettings {
    /// The selected profile's name.
    pub profile: Option<String>,
    /// A toolchain file for every profile without one (Conan's `conan_toolchain.cmake`, vcpkg's
    /// `vcpkg.cmake`).
    pub toolchain_file: Option<String>,
    /// Sanitizers added to every built-in profile's compile flags (`address,undefined`).
    pub sanitizers: Option<String>,
}

pub fn settings_path(root: &Path) -> PathBuf {
    root.join(SETTINGS_FILE)
}

pub fn load_settings(root: &Path) -> CmakeSettings {
    read_capped(&settings_path(root)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_settings(root: &Path, settings: &CmakeSettings) -> Result<(), String> {
    let path = settings_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    codecraft_fs::write_file_atomic(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// A CMake profile: what to configure with and where the build tree goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub build_type: Option<String>,
    /// Absolute.
    pub build_dir: PathBuf,
    pub generator: Option<String>,
    /// Configured with `cmake --preset <name>`.
    pub preset: Option<String>,
    /// A build preset for the configure preset.
    pub build_preset: Option<String>,
    pub test_preset: Option<String>,
    pub cache_variables: Vec<(String, String)>,
    pub environment: Vec<(String, String)>,
    /// A toolchain file passed as `-DCMAKE_TOOLCHAIN_FILE` (Conan/vcpkg; never one a preset sets).
    pub toolchain_file: Option<String>,
    /// The preset sets `binaryDir` (else we pass `-B`).
    pub preset_binary_dir: bool,
}

/// The built-in profiles: Debug (`cmake-build-debug`, CLion's default) and Release.
pub const BUILTIN: [(&str, &str); 2] = [("Debug", "cmake-build-debug"), ("Release", "cmake-build-release")];

/// The project's profiles: Debug and Release, then one per visible configure preset.
pub fn profiles(root: &Path) -> Vec<Profile> {
    profiles_with(root, &load_presets(root), &load_settings(root))
}

pub fn profiles_with(root: &Path, presets: &Presets, settings: &CmakeSettings) -> Vec<Profile> {
    let mut out: Vec<Profile> = BUILTIN
        .iter()
        .map(|(name, dir)| {
            let mut cache = Vec::new();
            if let Some(s) = settings.sanitizers.as_deref().filter(|s| !s.trim().is_empty()) {
                let flags = sanitizer_flags(s);
                cache.push(("CMAKE_C_FLAGS".to_string(), flags.clone()));
                cache.push(("CMAKE_CXX_FLAGS".to_string(), flags.clone()));
                cache.push(("CMAKE_EXE_LINKER_FLAGS".to_string(), format!("-fsanitize={}", s.trim())));
            }
            Profile {
                name: (*name).to_string(),
                build_type: Some((*name).to_string()),
                build_dir: root.join(dir),
                generator: None,
                preset: None,
                build_preset: None,
                test_preset: None,
                cache_variables: cache,
                environment: Vec::new(),
                toolchain_file: settings.toolchain_file.clone(),
                preset_binary_dir: false,
            }
        })
        .collect();
    for p in presets.configure.iter().filter(|p| !p.hidden) {
        let build_dir = match p.binary_dir.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => {
                let d = PathBuf::from(d);
                crate::normalize_path(&if d.is_absolute() { d } else { root.join(d) })
            }
            None => root.join(format!("cmake-build-{}", p.name)),
        };
        let build_type = p.cache_variables.iter().find(|(k, _)| k == "CMAKE_BUILD_TYPE").map(|(_, v)| v.clone());
        out.push(Profile {
            name: p.name.clone(),
            build_type,
            build_dir,
            generator: p.generator.clone(),
            preset: Some(p.name.clone()),
            build_preset: presets
                .build
                .iter()
                .find(|b| !b.hidden && b.configure_preset.as_deref() == Some(p.name.as_str()))
                .map(|b| b.name.clone()),
            test_preset: presets
                .test
                .iter()
                .find(|t| !t.hidden && t.configure_preset.as_deref() == Some(p.name.as_str()))
                .map(|t| t.name.clone()),
            cache_variables: p.cache_variables.clone(),
            environment: p.environment.clone(),
            // A preset's own `toolchainFile` is applied by CMake; ours only fills in for presets without one.
            toolchain_file: if p.toolchain_file.is_some() { None } else { settings.toolchain_file.clone() },
            preset_binary_dir: p.binary_dir.as_deref().is_some_and(|d| !d.is_empty()),
        });
    }
    out
}

/// `-fsanitize=<list> -fno-omit-frame-pointer -g`.
pub fn sanitizer_flags(list: &str) -> String {
    format!("-fsanitize={} -fno-omit-frame-pointer -g", list.trim())
}

/// The selected profile (the first one when the selection is unknown).
pub fn active_profile(root: &Path) -> Option<Profile> {
    let settings = load_settings(root);
    let all = profiles_with(root, &load_presets(root), &settings);
    let wanted = settings.profile.as_deref();
    all.iter().find(|p| Some(p.name.as_str()) == wanted).or(all.first()).cloned()
}

/// The arguments of `cmake` that configure `profile` (run in the project root).
pub fn configure_args(root: &Path, profile: &Profile) -> Vec<String> {
    let mut args = Vec::new();
    match &profile.preset {
        Some(name) => {
            args.push(format!("--preset={name}"));
            if !profile.preset_binary_dir {
                args.push("-B".into());
                args.push(profile.build_dir.display().to_string());
            }
        }
        None => {
            args.extend(["-S".into(), root.display().to_string(), "-B".into(), profile.build_dir.display().to_string()]);
            if let Some(g) = &profile.generator {
                args.push("-G".into());
                args.push(g.clone());
            }
            if let Some(t) = &profile.build_type {
                args.push(format!("-DCMAKE_BUILD_TYPE={t}"));
            }
            for (k, v) in &profile.cache_variables {
                args.push(format!("-D{k}={v}"));
            }
        }
    }
    args.push("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into());
    if let Some(t) = profile.toolchain_file.as_deref().filter(|t| !t.is_empty())
        && (profile.preset.is_none() || !profile.cache_variables.iter().any(|(k, _)| k == "CMAKE_TOOLCHAIN_FILE"))
    {
        args.push(format!("-DCMAKE_TOOLCHAIN_FILE={t}"));
    }
    args
}

/// The build directory as written in shell scripts: relative to the project when inside it.
pub fn build_dir_text(root: &Path, profile: &Profile) -> String {
    match profile.build_dir.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => profile.build_dir.display().to_string(),
    }
}

/// The shell command that configures the active profile and the build directory it uses, for
/// derived run configurations. The default (Debug, no presets) is CLion's
/// `cmake -S . -B cmake-build-debug -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`.
pub fn configure_script(root: &Path) -> (String, String) {
    let Some(profile) = active_profile(root) else {
        let dir = crate::run_configs::CMAKE_BUILD_DIR;
        return (
            dir.to_string(),
            format!("cmake -S . -B {dir} -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON"),
        );
    };
    let dir = build_dir_text(root, &profile);
    let q = |s: &str| codecraft_cargo::shell_quote(s);
    let mut words: Vec<String> = vec!["cmake".into()];
    match &profile.preset {
        Some(name) => {
            words.push(q(&format!("--preset={name}")));
            if !profile.preset_binary_dir {
                words.push("-B".into());
                words.push(q(&dir));
            }
        }
        None => {
            words.extend(["-S".into(), ".".into(), "-B".into(), q(&dir)]);
            if let Some(g) = &profile.generator {
                words.push("-G".into());
                words.push(q(g));
            }
            if let Some(t) = &profile.build_type {
                words.push(q(&format!("-DCMAKE_BUILD_TYPE={t}")));
            }
            for (k, v) in &profile.cache_variables {
                words.push(q(&format!("-D{k}={v}")));
            }
        }
    }
    words.push("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into());
    if let Some(t) = profile.toolchain_file.as_deref().filter(|t| !t.is_empty())
        && (profile.preset.is_none() || !profile.cache_variables.iter().any(|(k, _)| k == "CMAKE_TOOLCHAIN_FILE"))
    {
        words.push(q(&format!("-DCMAKE_TOOLCHAIN_FILE={t}")));
    }
    (dir, words.join(" "))
}

// ---------------------------------------------------------------- File API

/// Write our stateful query (codemodel v2, cache v2) into `build_dir`.
pub fn write_query(build_dir: &Path) -> Result<PathBuf, String> {
    let dir = build_dir.join(".cmake").join("api").join("v1").join("query").join(QUERY_CLIENT);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("query.json");
    let query = serde_json::json!({
        "requests": [
            { "kind": "codemodel", "version": 2 },
            { "kind": "cache", "version": 2 },
        ],
        "client": { "codecraft": {} },
    });
    let text = serde_json::to_string_pretty(&query).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// A target of the codemodel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CmakeTarget {
    pub name: String,
    /// `EXECUTABLE`, `STATIC_LIBRARY`, `SHARED_LIBRARY`, `MODULE_LIBRARY`, `OBJECT_LIBRARY`,
    /// `INTERFACE_LIBRARY`, `UTILITY`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Absolute.
    pub artifacts: Vec<PathBuf>,
    /// Absolute.
    pub sources: Vec<PathBuf>,
    /// The source subdirectory (`.` for the top level).
    pub directory: String,
}

impl CmakeTarget {
    pub fn is_executable(&self) -> bool {
        self.kind == "EXECUTABLE"
    }

    /// The executable it builds.
    pub fn executable(&self) -> Option<&PathBuf> {
        self.is_executable().then(|| self.artifacts.first()).flatten()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Codemodel {
    pub source_dir: PathBuf,
    pub build_dir: PathBuf,
    pub configuration: Option<String>,
    pub targets: Vec<CmakeTarget>,
}

impl Codemodel {
    /// Targets that compile `path`.
    pub fn targets_for_source(&self, path: &Path) -> Vec<&CmakeTarget> {
        let wanted = crate::normalize_path(path);
        let canonical = std::fs::canonicalize(path).ok();
        self.targets
            .iter()
            .filter(|t| t.sources.iter().any(|s| *s == wanted || canonical.as_ref().is_some_and(|c| s == c)))
            .collect()
    }
}

fn reply_dir(build_dir: &Path) -> PathBuf {
    build_dir.join(".cmake").join("api").join("v1").join("reply")
}

/// The newest reply index file.
pub fn reply_index(build_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(reply_dir(build_dir)).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("index-") && n.ends_with(".json"))
        })
        .max()
}

/// Resolve `p` against `base` unless absolute.
fn under(base: &Path, p: &str) -> PathBuf {
    let p = PathBuf::from(p);
    crate::normalize_path(&if p.is_absolute() { p } else { base.join(p) })
}

/// Read the codemodel reply in `build_dir`.
pub fn read_codemodel(build_dir: &Path) -> Result<Codemodel, String> {
    let index_path = reply_index(build_dir).ok_or_else(|| {
        format!("no CMake File API reply in {} (reload the CMake project)", build_dir.display())
    })?;
    let index: Value = serde_json::from_str(&read_capped(&index_path)?).map_err(|e| format!("{}: {e}", index_path.display()))?;
    let dir = reply_dir(build_dir);
    // Our client's responses first, else any codemodel object of the index.
    let from_client = index
        .pointer(&format!("/reply/{QUERY_CLIENT}/query.json/responses"))
        .and_then(Value::as_array)
        .and_then(|r| r.iter().find(|x| x.get("kind").and_then(Value::as_str) == Some("codemodel")))
        .and_then(|x| x.get("jsonFile").and_then(Value::as_str));
    let from_objects = index
        .get("objects")
        .and_then(Value::as_array)
        .and_then(|r| r.iter().find(|x| x.get("kind").and_then(Value::as_str) == Some("codemodel")))
        .and_then(|x| x.get("jsonFile").and_then(Value::as_str));
    let file = from_client.or(from_objects).ok_or("the CMake File API reply has no codemodel")?;
    let file = safe_reply_name(file)?;
    let model: Value = serde_json::from_str(&read_capped(&dir.join(file))?).map_err(|e| format!("{file}: {e}"))?;
    let source_dir = model.pointer("/paths/source").and_then(Value::as_str).map(PathBuf::from).unwrap_or_default();
    let build = model
        .pointer("/paths/build")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| build_dir.to_path_buf());
    let config = model.get("configurations").and_then(Value::as_array).and_then(|c| c.first());
    let mut out = Codemodel {
        source_dir: source_dir.clone(),
        build_dir: build.clone(),
        configuration: config.and_then(|c| c.get("name")).and_then(Value::as_str).map(str::to_string),
        targets: Vec::new(),
    };
    let directories: Vec<String> = config
        .and_then(|c| c.get("directories"))
        .and_then(Value::as_array)
        .map(|d| d.iter().map(|x| x.get("source").and_then(Value::as_str).unwrap_or(".").to_string()).collect())
        .unwrap_or_default();
    for t in config.and_then(|c| c.get("targets")).and_then(Value::as_array).into_iter().flatten().take(MAX_TARGETS) {
        let Some(json_file) = t.get("jsonFile").and_then(Value::as_str) else { continue };
        let Ok(json_file) = safe_reply_name(json_file) else { continue };
        let Ok(text) = read_capped(&dir.join(json_file)) else { continue };
        let Ok(tv) = serde_json::from_str::<Value>(&text) else { continue };
        let name = tv.get("name").and_then(Value::as_str).or_else(|| t.get("name").and_then(Value::as_str)).unwrap_or("");
        let directory = t
            .get("directoryIndex")
            .and_then(Value::as_u64)
            .and_then(|i| directories.get(usize::try_from(i).ok()?))
            .cloned()
            .unwrap_or_else(|| ".".into());
        out.targets.push(CmakeTarget {
            name: name.to_string(),
            kind: tv.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
            artifacts: tv
                .get("artifacts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| a.get("path").and_then(Value::as_str))
                .map(|p| under(&build, p))
                .collect(),
            sources: tv
                .get("sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(MAX_SOURCES)
                .filter_map(|s| s.get("path").and_then(Value::as_str))
                .map(|p| under(&source_dir, p))
                .collect(),
            directory,
        });
    }
    out.targets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Reply file names come from the reply itself: keep them inside the reply directory.
fn safe_reply_name(name: &str) -> Result<&str, String> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err(format!("bad File API reply file name `{name}`"));
    }
    Ok(name)
}

/// The codemodel of the active profile, cached by reply index path and modification time
/// (derived run configurations and run-at-caret ask often).
pub fn cached_codemodel(root: &Path) -> Option<Codemodel> {
    type Cache = Mutex<Option<(PathBuf, Option<SystemTime>, Codemodel)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let profile = active_profile(root)?;
    let index = reply_index(&profile.build_dir)?;
    let mtime = std::fs::metadata(&index).and_then(|m| m.modified()).ok();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((p, m, model)) = guard.as_ref()
            && *p == index
            && *m == mtime
        {
            return Some(model.clone());
        }
    }
    let model = read_codemodel(&profile.build_dir).ok()?;
    *cache.lock().unwrap_or_else(PoisonError::into_inner) = Some((index, mtime, model.clone()));
    Some(model)
}

/// Executable targets of the active profile that use a test framework (cached like
/// [`cached_codemodel`]). Reading their sources is slow on big projects, so only `compute`
/// callers (the CMake analysis worker thread) fill the cache; others get what is cached.
pub fn cached_test_targets(root: &Path, compute: bool) -> Vec<(CmakeTarget, Framework)> {
    type Cache = Mutex<Option<(PathBuf, Option<SystemTime>, Vec<(CmakeTarget, Framework)>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let Some(profile) = active_profile(root) else { return Vec::new() };
    let Some(index) = reply_index(&profile.build_dir) else { return Vec::new() };
    let mtime = std::fs::metadata(&index).and_then(|m| m.modified()).ok();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((p, m, list)) = guard.as_ref()
            && *p == index
            && *m == mtime
        {
            return list.clone();
        }
    }
    if !compute {
        return Vec::new();
    }
    let Some(model) = cached_codemodel(root) else { return Vec::new() };
    let list: Vec<(CmakeTarget, Framework)> = model
        .targets
        .iter()
        .filter(|t| t.is_executable())
        .take(200)
        .filter_map(|t| target_framework(t).map(|fw| (t.clone(), fw)))
        .collect();
    *cache.lock().unwrap_or_else(PoisonError::into_inner) = Some((index, mtime, list.clone()));
    list
}

/// "All in <target>" run configurations for the test executables (built first).
pub fn test_target_configs(root: &Path) -> Vec<crate::runs::RunConfig> {
    let Some(profile) = active_profile(root) else { return Vec::new() };
    cached_test_targets(root, false)
        .into_iter()
        .take(30)
        .filter_map(|(t, fw)| {
            let exe = t.executable()?.clone();
            let cwd = exe.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
            let cmd = super::cmake_cmds::build_then_run(Path::new("cmake"), &profile, &t.name, &exe, Vec::new(), &cwd, Some(fw));
            Some(crate::runs::RunConfig::process(format!("All in {}", t.name), cmd))
        })
        .collect()
}

/// A CTest `-R` regex for a test macro (GoogleTest names are `Suite.Name`; `*` is a wildcard).
fn ctest_regex(m: &crate::testing::cpp::TestMacro) -> String {
    let pattern = match (m.framework, &m.filter) {
        (Framework::GoogleTest, (Some(s), Some(t))) => format!("{s}.{t}"),
        (Framework::GoogleTest, (Some(s), None)) => format!("{s}.*"),
        (Framework::BoostTest, (Some(s), Some(t))) => format!("{s}/{t}"),
        (_, (_, Some(t))) => t.clone(),
        (_, (Some(s), None)) => format!("{s}*"),
        _ => m.name.clone(),
    };
    format!("^{}$", regex::escape(&pattern).replace("\\*", ".*"))
}

/// The run configuration for a C++ test (or suite) macro in `path`: build its CMake target and
/// run the executable with the framework's filter; when the project is not configured yet (or
/// no target compiles the file), configure, build and run it through CTest by name.
pub fn test_config(root: &Path, path: &Path, m: &crate::testing::cpp::TestMacro) -> crate::runs::RunConfig {
    let title = match (m.framework, &m.suite, m.is_suite) {
        (_, _, true) => format!("All in {}", m.name),
        (Framework::GoogleTest, Some(s), false) => format!("{s}.{}", m.name),
        _ => m.name.clone(),
    };
    let args = crate::testing::cpp::filter_args(m.framework, m.filter.0.as_deref(), m.filter.1.as_deref());
    if let (Some(profile), Some(model)) = (active_profile(root), cached_codemodel(root))
        && let Some(t) = model.targets_for_source(path).into_iter().find(|t| t.is_executable())
        && let Some(exe) = t.executable()
    {
        let cwd = exe.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
        let cmd = super::cmake_cmds::build_then_run(Path::new("cmake"), &profile, &t.name, exe, args, &cwd, Some(m.framework));
        return crate::runs::RunConfig::process(title, cmd);
    }
    let (dir, configure) = configure_script(root);
    let q = codecraft_cargo::shell_quote(&dir);
    let script = format!(
        "{configure} && cmake --build {q} && ctest --test-dir {q} --output-on-failure -R {}",
        codecraft_cargo::shell_quote(&ctest_regex(m))
    );
    crate::runs::RunConfig::process(title, crate::runs::ProcessCommand::shell(script, root))
}

/// The test framework an executable target uses (from its sources' includes).
pub fn target_framework(target: &CmakeTarget) -> Option<Framework> {
    for src in target.sources.iter().take(200) {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "cpp" | "cc" | "cxx" | "c++" | "C" | "c") {
            continue;
        }
        let Ok(text) = read_capped(src) else { continue };
        if let Some(fw) = crate::testing::cpp::framework_in_source(&text) {
            return Some(fw);
        }
    }
    None
}

// ---------------------------------------------------------------- CMakeCache.txt

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub name: String,
    /// `BOOL`, `STRING`, `PATH`, `FILEPATH`, `INTERNAL`, `STATIC`, `UNINITIALIZED`.
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
    pub help: String,
    pub advanced: bool,
}

/// `NAME:TYPE=VALUE` lines with `//` help comments above them.
pub fn parse_cache(text: &str) -> Vec<CacheEntry> {
    let mut out: Vec<CacheEntry> = Vec::new();
    let mut help: Vec<&str> = Vec::new();
    let mut advanced: Vec<String> = Vec::new();
    for line in text.lines().take(200_000) {
        let line = line.trim_end_matches('\r');
        if let Some(h) = line.strip_prefix("//") {
            help.push(h.trim());
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            help.clear();
            continue;
        }
        let Some((head, value)) = line.split_once('=') else {
            help.clear();
            continue;
        };
        let (name, kind) = match head.rsplit_once(':') {
            Some((n, k)) => (n.trim_matches('"'), k),
            None => (head.trim_matches('"'), "UNINITIALIZED"),
        };
        if let Some(base) = name.strip_suffix("-ADVANCED") {
            if value.trim() == "1" {
                advanced.push(base.to_string());
            }
            help.clear();
            continue;
        }
        if name.is_empty() {
            help.clear();
            continue;
        }
        out.push(CacheEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            value: value.to_string(),
            help: help.join(" "),
            advanced: false,
        });
        help.clear();
    }
    for e in &mut out {
        e.advanced = advanced.contains(&e.name);
    }
    out
}

/// Set `name` to `value` in a cache file's text (a new `STRING` entry when it is not there).
pub fn set_cache_value(text: &str, name: &str, value: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['\n', '\r', '=', ':']) {
        return Err(format!("`{name}` is not a cache variable name"));
    }
    if value.contains(['\n', '\r']) {
        return Err("a cache value cannot contain line breaks".into());
    }
    let mut found = false;
    let mut out = String::with_capacity(text.len() + value.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = line.get(body.len()..).unwrap_or("");
        if !found
            && !body.starts_with("//")
            && !body.starts_with('#')
            && let Some((head, _)) = body.split_once('=')
            && head.rsplit_once(':').map_or(head, |(n, _)| n).trim_matches('"') == name
        {
            out.push_str(head);
            out.push('=');
            out.push_str(value);
            out.push_str(ending);
            found = true;
            continue;
        }
        out.push_str(line);
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{name}:STRING={value}\n"));
    }
    Ok(out)
}

pub fn cache_path(build_dir: &Path) -> PathBuf {
    build_dir.join("CMakeCache.txt")
}

pub fn read_cache(build_dir: &Path) -> Result<Vec<CacheEntry>, String> {
    read_capped(&cache_path(build_dir)).map(|t| parse_cache(&t))
}

// ---------------------------------------------------------------- CTest

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtestTest {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub labels: Vec<String>,
}

/// `ctest --show-only=json-v1`.
pub fn parse_ctest_json(text: &str) -> Result<Vec<CtestTest>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("ctest output is not JSON: {e}"))?;
    let mut out = Vec::new();
    for t in v.get("tests").and_then(Value::as_array).into_iter().flatten().take(100_000) {
        let Some(name) = t.get("name").and_then(Value::as_str) else { continue };
        let command: Vec<String> =
            t.get("command").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        let props = t.get("properties").and_then(Value::as_array);
        let prop = |n: &str| props.into_iter().flatten().find(|p| p.get("name").and_then(Value::as_str) == Some(n));
        out.push(CtestTest {
            name: name.to_string(),
            command,
            cwd: prop("WORKING_DIRECTORY").and_then(|p| p.get("value")).and_then(Value::as_str).map(PathBuf::from),
            labels: prop("LABELS").map(|p| str_list(p.get("value"))).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// A test executable of the project: from the codemodel and/or CTest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestExecutable {
    pub path: PathBuf,
    pub target: Option<String>,
    pub framework: Option<Framework>,
    /// CTest tests that run it.
    pub ctest_tests: usize,
}

/// Merge codemodel executables that use a test framework with CTest's commands.
pub fn test_executables(
    model: Option<&Codemodel>,
    frameworks: &BTreeMap<String, Framework>,
    ctest: &[CtestTest],
) -> Vec<TestExecutable> {
    let mut out: Vec<TestExecutable> = Vec::new();
    if let Some(model) = model {
        for t in &model.targets {
            let (Some(exe), Some(fw)) = (t.executable(), frameworks.get(&t.name)) else { continue };
            out.push(TestExecutable { path: exe.clone(), target: Some(t.name.clone()), framework: Some(*fw), ctest_tests: 0 });
        }
    }
    for c in ctest {
        let Some(exe) = c.command.first() else { continue };
        let path = PathBuf::from(exe);
        let cmd = crate::runs::ProcessCommand {
            program: exe.clone(),
            args: c.command.get(1..).unwrap_or(&[]).to_vec(),
            cwd: PathBuf::new(),
            env: Vec::new(),
            build: false,
        };
        let fw = crate::testing::cpp::framework_of(&cmd);
        match out.iter_mut().find(|e| e.path == path) {
            Some(e) => {
                e.ctest_tests += 1;
                if e.framework.is_none() {
                    e.framework = fw;
                }
            }
            None => {
                let target = model
                    .and_then(|m| m.targets.iter().find(|t| t.executable() == Some(&path)))
                    .map(|t| t.name.clone());
                let framework = fw.or_else(|| target.as_ref().and_then(|t| frameworks.get(t).copied()));
                out.push(TestExecutable { path, target, framework, ctest_tests: 1 });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    const PRESETS: &str = r#"{
      "version": 6,
      "include": ["more/Presets.json"],
      "configurePresets": [
        { "name": "base", "hidden": true, "generator": "Ninja", "binaryDir": "${sourceDir}/out/${presetName}",
          "cacheVariables": { "CMAKE_CXX_STANDARD": "20", "BASE_ONLY": { "type": "BOOL", "value": true } },
          "environment": { "CC": "clang" } },
        { "name": "dev", "inherits": "base", "displayName": "Developer",
          "cacheVariables": { "CMAKE_BUILD_TYPE": "Debug", "CMAKE_CXX_STANDARD": "23", "BASE_ONLY": null },
          "toolchainFile": "$env{CC}-${hostSystemName}.cmake" },
        { "name": "loop1", "inherits": "loop2" },
        { "name": "loop2", "inherits": ["loop1"] },
        { "name": "orphan", "inherits": "nope" }
      ],
      "buildPresets": [
        { "name": "dev-build", "configurePreset": "dev", "targets": ["app"] },
        { "name": "child-build", "inherits": "dev-build", "jobs": 4 }
      ],
      "testPresets": [ { "name": "dev-test", "configurePreset": "dev" } ]
    }"#;

    #[test]
    fn presets_inheritance_macros_and_includes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "CMakePresets.json", PRESETS);
        write(root, "more/Presets.json", r#"{"version": 6, "configurePresets": [{"name": "inc", "binaryDir": "${fileDir}/b"}]}"#);
        write(root, "CMakeUserPresets.json", r#"{"version": 6, "configurePresets": [{"name": "mine", "inherits": "dev", "binaryDir": "${sourceParentDir}/${sourceDirName}-mine"}]}"#);
        let p = load_presets(root);
        let names: Vec<&str> = p.configure.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["base", "dev", "orphan", "inc", "mine"], "{:?}", p.errors);
        let dev = p.configure.iter().find(|c| c.name == "dev").unwrap();
        assert!(!dev.hidden);
        assert_eq!(dev.generator.as_deref(), Some("Ninja"));
        assert_eq!(dev.binary_dir.as_deref(), Some(format!("{}/out/dev", root.display()).as_str()));
        let var = |n: &str| dev.cache_variables.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
        assert_eq!(var("CMAKE_CXX_STANDARD"), Some("23"));
        assert_eq!(var("CMAKE_BUILD_TYPE"), Some("Debug"));
        assert_eq!(var("BASE_ONLY"), None);
        assert_eq!(dev.toolchain_file.as_deref(), Some(format!("clang-{}.cmake", host_system_name()).as_str()));
        assert!(p.configure.iter().find(|c| c.name == "base").unwrap().hidden);
        assert!(p.errors.iter().any(|e| e.contains("inherits from itself")), "{:?}", p.errors);
        assert!(p.errors.iter().any(|e| e.contains("unknown preset `nope`")));
        let inc = p.configure.iter().find(|c| c.name == "inc").unwrap();
        assert_eq!(inc.binary_dir.as_deref(), Some(format!("{}/more/b", root.display()).as_str()));
        let mine = p.configure.iter().find(|c| c.name == "mine").unwrap();
        let expected = format!("{}/{}-mine", root.parent().unwrap().display(), root.file_name().unwrap().to_string_lossy());
        assert_eq!(mine.binary_dir.as_deref(), Some(expected.as_str()));
        assert_eq!(mine.generator.as_deref(), Some("Ninja"));
        let child = p.build.iter().find(|b| b.name == "child-build").unwrap();
        assert_eq!((child.configure_preset.as_deref(), child.jobs, child.targets.clone()), (Some("dev"), Some(4), vec!["app".to_string()]));

        // Profiles: Debug, Release, then visible presets; the selection persists.
        let profiles = profiles(root);
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Debug", "Release", "dev", "orphan", "inc", "mine"]);
        let dev = &profiles[2];
        assert_eq!(dev.build_preset.as_deref(), Some("dev-build"));
        assert_eq!(dev.test_preset.as_deref(), Some("dev-test"));
        assert_eq!(dev.build_dir, root.join("out/dev"));
        assert_eq!(profiles[3].build_dir, root.join("cmake-build-orphan"));
        assert_eq!(active_profile(root).unwrap().name, "Debug");
        assert_eq!(configure_script(root), ("cmake-build-debug".to_string(), "cmake -S . -B cmake-build-debug -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string()));
        save_settings(root, &CmakeSettings { profile: Some("dev".into()), ..Default::default() }).unwrap();
        assert_eq!(active_profile(root).unwrap().name, "dev");
        assert_eq!(configure_script(root), ("out/dev".to_string(), "cmake --preset=dev -DCMAKE_EXPORT_COMPILE_COMMANDS=ON".to_string()));
        let args = configure_args(root, &profiles[3]);
        assert_eq!(args[..3], ["--preset=orphan".to_string(), "-B".into(), root.join("cmake-build-orphan").display().to_string()]);
        save_settings(
            root,
            &CmakeSettings { profile: Some("Release".into()), toolchain_file: Some("/t/conan.cmake".into()), sanitizers: Some("address".into()) },
        )
        .unwrap();
        let (dir, script) = configure_script(root);
        assert_eq!(dir, "cmake-build-release");
        assert!(script.contains("'-DCMAKE_CXX_FLAGS=-fsanitize=address -fno-omit-frame-pointer -g'"), "{script}");
        assert!(script.ends_with("-DCMAKE_TOOLCHAIN_FILE=/t/conan.cmake"), "{script}");
    }

    #[test]
    fn hostile_presets_never_panic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for text in ["", "{", "[]", "{\"configurePresets\": 5}", "{\"configurePresets\": [{\"name\": 1}, {}, {\"name\": \"x\", \"inherits\": [1, \"x\"]}]}", "{\"include\": [\"CMakePresets.json\"]}"] {
            write(root, "CMakePresets.json", text);
            let _ = load_presets(root);
            let _ = profiles(root);
        }
        let ctx = MacroContext { source_dir: Path::new("/"), preset_name: "p", generator: None, file_dir: Path::new("/"), env: &[] };
        for v in ["$", "${", "${sourceDir", "$env{", "${unknown}x", "$$", "${dollar}{x}", "é${presetName}ü"] {
            let _ = expand_macros(v, &ctx);
        }
        assert_eq!(expand_macros("${unknown}/${presetName}/${dollar}", &ctx), "${unknown}/p/$");
        assert_eq!(expand_macros("${sourceParentDir}", &ctx), "${sourceParentDir}");
    }

    #[test]
    fn file_api_reply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        let build = dir.path().join("build");
        let q = write_query(&build).unwrap();
        assert!(q.ends_with(".cmake/api/v1/query/client-codecraft/query.json"));
        assert!(read_codemodel(&build).unwrap_err().contains("no CMake File API reply"));
        let reply = build.join(".cmake/api/v1/reply");
        write(&reply, "index-2026-10-14T10-00-00-0000.json", r#"{
            "cmake": {"version": {"string": "3.30.0"}},
            "objects": [{"kind": "codemodel", "version": {"major": 2, "minor": 7}, "jsonFile": "codemodel-v2-old.json"}],
            "reply": {"client-codecraft": {"query.json": {"responses": [
                {"kind": "codemodel", "version": {"major": 2, "minor": 7}, "jsonFile": "codemodel-v2-abc.json"},
                {"kind": "cache", "version": {"major": 2, "minor": 0}, "jsonFile": "cache-v2-abc.json"}
            ]}}}
        }"#);
        write(&reply, "index-2026-10-13T10-00-00-0000.json", "{}");
        write(&reply, "codemodel-v2-abc.json", &format!(r#"{{
            "kind": "codemodel", "paths": {{"source": "{src}", "build": "{bld}"}},
            "configurations": [{{"name": "Debug",
                "directories": [{{"source": "."}}, {{"source": "tests"}}],
                "targets": [
                    {{"name": "tests", "directoryIndex": 1, "jsonFile": "target-tests.json"}},
                    {{"name": "app", "directoryIndex": 0, "jsonFile": "target-app.json"}},
                    {{"name": "evil", "jsonFile": "../../../../etc/passwd"}}
                ]}}]
        }}"#, src = root.display(), bld = build.display()));
        write(&reply, "target-app.json", r#"{"name": "app", "type": "EXECUTABLE", "artifacts": [{"path": "app"}], "sources": [{"path": "main.cpp"}, {"path": "util.cpp"}]}"#);
        write(&reply, "target-tests.json", r#"{"name": "tests", "type": "EXECUTABLE", "artifacts": [{"path": "tests/tests"}], "sources": [{"path": "tests/math_test.cpp"}]}"#);
        let model = read_codemodel(&build).unwrap();
        assert_eq!(model.configuration.as_deref(), Some("Debug"));
        assert_eq!(model.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["app", "tests"]);
        let tests = &model.targets[1];
        assert_eq!(tests.executable(), Some(&build.join("tests/tests")));
        assert_eq!(tests.directory, "tests");
        assert_eq!(model.targets_for_source(&root.join("tests/math_test.cpp")).len(), 1);
        write(&root, "tests/math_test.cpp", "#include <gtest/gtest.h>\nTEST(A, B) {}\n");
        assert_eq!(target_framework(tests), Some(Framework::GoogleTest));
        assert_eq!(target_framework(&model.targets[0]), None);
        let mut fws = BTreeMap::new();
        fws.insert("tests".to_string(), Framework::GoogleTest);
        let ctest = parse_ctest_json(&format!(r#"{{"kind": "ctestInfo", "tests": [
            {{"name": "A.B", "command": ["{exe}", "--gtest_filter=A.B"], "properties": [{{"name": "WORKING_DIRECTORY", "value": "/w"}}, {{"name": "LABELS", "value": ["unit"]}}]}},
            {{"name": "other", "command": ["/x/other"]}},
            {{"name": "no command"}}
        ]}}"#, exe = build.join("tests/tests").display())).unwrap();
        assert_eq!(ctest[0].cwd.as_deref(), Some(Path::new("/w")));
        assert_eq!(ctest[0].labels, vec!["unit"]);
        let exes = test_executables(Some(&model), &fws, &ctest);
        assert_eq!(exes.len(), 2);
        assert_eq!((exes[0].ctest_tests, exes[0].framework), (1, Some(Framework::GoogleTest)));
        assert_eq!((exes[1].path.as_path(), exes[1].framework), (Path::new("/x/other"), None));
        assert!(parse_ctest_json("not json").is_err());
        // Corrupt replies fail cleanly.
        write(&reply, "codemodel-v2-abc.json", "{");
        assert!(read_codemodel(&build).is_err());
    }

    #[test]
    fn cache_entries() {
        let text = "# This is the CMakeCache file.\n\n//Build type\nCMAKE_BUILD_TYPE:STRING=Debug\n//Flags\n//more help\n\
                    CMAKE_CXX_FLAGS:STRING=\nCMAKE_CXX_FLAGS-ADVANCED:INTERNAL=1\n\"QUOTED:NAME\":BOOL=ON\nNOTYPE=1\n\
                    CMAKE_HOME_DIRECTORY:INTERNAL=/src\r\nbroken line\n";
        let e = parse_cache(text);
        let names: Vec<&str> = e.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["CMAKE_BUILD_TYPE", "CMAKE_CXX_FLAGS", "QUOTED:NAME", "NOTYPE", "CMAKE_HOME_DIRECTORY"]);
        assert_eq!(e[0].help, "Build type");
        assert_eq!(e[1].help, "Flags more help");
        assert!(e[1].advanced && !e[0].advanced);
        assert_eq!(e[3].kind, "UNINITIALIZED");
        assert_eq!(e[4].value, "/src");
        let changed = set_cache_value(text, "CMAKE_BUILD_TYPE", "Release").unwrap();
        assert!(changed.contains("CMAKE_BUILD_TYPE:STRING=Release\n"));
        assert!(changed.contains("CMAKE_HOME_DIRECTORY:INTERNAL=/src\r\n"));
        let added = set_cache_value("A:BOOL=ON", "NEW_VAR", "x y").unwrap();
        assert_eq!(added, "A:BOOL=ON\nNEW_VAR:STRING=x y\n");
        assert!(set_cache_value(text, "A=B", "x").is_err());
        assert!(set_cache_value(text, "A", "x\ny").is_err());
        let _ = parse_cache("\u{0}:=\n:=\n=\n-ADVANCED:INTERNAL=1\n");
    }
}
