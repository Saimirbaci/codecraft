//! The CMake tool window and its commands: Reload CMake Project (configure with the selected
//! profile and our File API query, output in the window's log), profiles (Debug, Release,
//! presets), targets from the codemodel (build, run, clean), the `CMakeCache.txt` variables
//! (view, edit) and CTest's test list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{NotificationLevel, ToolWindowId};

use super::cmake::{self, CacheEntry, Codemodel, CtestTest, Profile};
use super::jobs::{JobKind, JobStatus};
use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, simple, spec};
use crate::params::{bool_or, opt_str, req_str};
use crate::runs::ProcessCommand;
use crate::testing::Framework;
use crate::{CommandError, Session};

/// The CMake tool window's tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CmakeTab {
    #[default]
    Log,
    Targets,
    Cache,
    Tests,
    Dependencies,
}

/// What a background read of a build directory found.
struct Analysis {
    build_dir: PathBuf,
    codemodel: Result<Codemodel, String>,
    cache: Result<Vec<CacheEntry>, String>,
    frameworks: BTreeMap<String, Framework>,
}

/// Runs on a worker thread: also warms the test-target cache derived run configurations read.
fn analyze(root: PathBuf, build_dir: PathBuf) -> Analysis {
    let codemodel = cmake::read_codemodel(&build_dir);
    let cache = cmake::read_cache(&build_dir);
    // The cache follows the selected profile, which is the one analyzed.
    let mut frameworks: BTreeMap<String, Framework> =
        cmake::cached_test_targets(&root, true).into_iter().map(|(t, fw)| (t.name, fw)).collect();
    if let Ok(model) = &codemodel {
        frameworks.retain(|name, _| model.targets.iter().any(|t| &t.name == name));
    }
    Analysis { build_dir, codemodel, cache, frameworks }
}

#[derive(Default)]
pub struct CmakeState {
    /// The project the state belongs to.
    pub root: Option<PathBuf>,
    /// The project has a top-level `CMakeLists.txt`.
    pub is_cmake: bool,
    pub profiles: Vec<Profile>,
    pub selected: Option<String>,
    pub preset_errors: Vec<String>,
    pub codemodel: Option<Codemodel>,
    pub codemodel_error: Option<String>,
    /// Executable target → the test framework its sources include.
    pub frameworks: BTreeMap<String, Framework>,
    pub cache: Vec<CacheEntry>,
    pub cache_error: Option<String>,
    pub ctest: Vec<CtestTest>,
    pub ctest_error: Option<String>,
    /// The last reload (its log is the Log tab).
    pub reload_job: Option<u64>,
    pub ctest_job: Option<u64>,
    pub tab: CmakeTab,
    pub cache_filter: String,
    pub show_advanced: bool,
    /// A cache variable changed since the last reload.
    pub needs_reload: bool,
    analysis: Option<Receiver<Analysis>>,
}

impl CmakeState {
    pub fn analyzing(&self) -> bool {
        self.analysis.is_some()
    }

    /// The selected profile.
    pub fn active(&self) -> Option<&Profile> {
        self.profiles.iter().find(|p| Some(&p.name) == self.selected.as_ref()).or(self.profiles.first())
    }

    /// Cache entries matching the filter (advanced ones only when shown; never INTERNAL/STATIC).
    pub fn visible_cache(&self) -> Vec<&CacheEntry> {
        let f = self.cache_filter.to_ascii_lowercase();
        self.cache
            .iter()
            .filter(|e| !matches!(e.kind.as_str(), "INTERNAL" | "STATIC"))
            .filter(|e| self.show_advanced || !e.advanced)
            .filter(|e| f.is_empty() || e.name.to_ascii_lowercase().contains(&f) || e.value.to_ascii_lowercase().contains(&f))
            .collect()
    }
}

// ---------------------------------------------------------------- state upkeep

/// Load profiles for `root` and read its build directory in the background.
fn load(s: &mut Session, root: &Path) {
    let st = &mut s.cpp.cmake;
    *st = CmakeState { root: Some(root.to_path_buf()), tab: st.tab, ..CmakeState::default() };
    st.is_cmake = root.join("CMakeLists.txt").is_file();
    if !st.is_cmake {
        return;
    }
    refresh_profiles(s, root);
    start_analysis(s);
}

fn refresh_profiles(s: &mut Session, root: &Path) {
    let presets = cmake::load_presets(root);
    let settings = cmake::load_settings(root);
    let st = &mut s.cpp.cmake;
    st.profiles = cmake::profiles_with(root, &presets, &settings);
    st.preset_errors = presets.errors;
    st.selected = settings
        .profile
        .filter(|p| st.profiles.iter().any(|x| &x.name == p))
        .or_else(|| st.profiles.first().map(|p| p.name.clone()));
}

fn start_analysis(s: &mut Session) {
    let Some(dir) = s.cpp.cmake.active().map(|p| p.build_dir.clone()) else { return };
    let Some(root) = s.cpp.cmake.root.clone() else { return };
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-cmake-analysis".into()).spawn(move || {
        let _ = tx.send(analyze(root, dir));
    });
    match spawned {
        Ok(_) => s.cpp.cmake.analysis = Some(rx),
        Err(e) => log::warn!("cmake analysis thread: {e}"),
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    if root != s.cpp.cmake.root {
        match &root {
            Some(r) => load(s, r),
            None => s.cpp.cmake = CmakeState { tab: s.cpp.cmake.tab, ..CmakeState::default() },
        }
        changed = true;
    }
    let result = match &s.cpp.cmake.analysis {
        Some(rx) => match rx.try_recv() {
            Ok(a) => Some(Some(a)),
            Err(crossbeam_channel::TryRecvError::Empty) => None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Some(None),
        },
        None => None,
    };
    if let Some(result) = result {
        s.cpp.cmake.analysis = None;
        changed = true;
        if let Some(a) = result {
            let st = &mut s.cpp.cmake;
            if st.active().is_some_and(|p| p.build_dir == a.build_dir) {
                match a.codemodel {
                    Ok(m) => {
                        st.codemodel = Some(m);
                        st.codemodel_error = None;
                    }
                    Err(e) => {
                        st.codemodel = None;
                        st.codemodel_error = Some(e);
                    }
                }
                match a.cache {
                    Ok(c) => {
                        st.cache = c;
                        st.cache_error = None;
                    }
                    Err(e) => {
                        st.cache.clear();
                        st.cache_error = Some(e);
                    }
                }
                st.frameworks = a.frameworks;
                refresh_run_configs(s);
            }
        }
    }
    changed
}

/// Derived run configurations follow the profile (build directory) and the codemodel (test
/// executables).
pub(crate) fn refresh_run_configs(s: &mut Session) {
    let Some(project) = s.ws.project.as_ref() else { return };
    let root = project.root.clone();
    let mut derived = project.cargo.as_ref().map(crate::runs::derive_configs).unwrap_or_default();
    derived.extend(crate::run_configs::derive_polyglot(&root));
    if s.run_store.root().is_some() {
        s.run_store.derived = derived;
        crate::run_config_store::rebuild(s);
    } else {
        s.run_configs = derived;
    }
}

// ---------------------------------------------------------------- jobs

pub(crate) fn reload_finished(s: &mut Session, id: u64, profile: &str, ok: bool) {
    s.cpp.cmake.needs_reload = false;
    if ok {
        s.status = format!("CMake project reloaded ({profile})");
        if s.cpp.cmake.active().is_some_and(|p| p.name == profile) {
            start_analysis(s);
        }
        return;
    }
    let stopped = s.cpp.jobs.get(id).is_some_and(|j| j.status == JobStatus::Stopped);
    if stopped {
        s.status = "CMake reload stopped".into();
        return;
    }
    let tail: Vec<String> = s
        .cpp
        .jobs
        .get(id)
        .map(|j| {
            let mut v: Vec<String> =
                j.lines.iter().rev().filter(|l| l.contains("Error") || l.contains("error")).take(3).cloned().collect();
            v.reverse();
            v
        })
        .unwrap_or_default();
    let msg = if tail.is_empty() { "see the CMake tool window".to_string() } else { tail.join("\n") };
    s.notify(NotificationLevel::Error, "CMake reload failed", msg);
}

pub(crate) fn ctest_finished(s: &mut Session, id: u64, ok: bool) {
    let Some(job) = s.cpp.jobs.get(id) else { return };
    let parsed = cmake::parse_ctest_json(&job.stdout);
    let st = &mut s.cpp.cmake;
    match parsed {
        Ok(tests) => {
            st.ctest = tests;
            st.ctest_error = None;
        }
        Err(e) => st.ctest_error = Some(if ok { e } else { format!("ctest failed: {e}") }),
    }
}

// ---------------------------------------------------------------- helpers

fn cmake_project(s: &Session) -> Result<PathBuf, CommandError> {
    let root = super::project_root(s)?;
    if !root.join("CMakeLists.txt").is_file() {
        return Err(CommandError::Disabled("not a CMake project (no CMakeLists.txt)".into()));
    }
    Ok(root)
}

fn is_cmake_project(s: &Session) -> Result<(), String> {
    cmake_project(s).map(|_| ()).map_err(|e| e.to_string())
}

fn profile(s: &mut Session, name: Option<&str>) -> Result<Profile, CommandError> {
    let root = cmake_project(s)?;
    if s.cpp.cmake.root.as_deref() != Some(root.as_path()) || s.cpp.cmake.profiles.is_empty() {
        load(s, &root);
    }
    match name {
        Some(n) => s
            .cpp
            .cmake
            .profiles
            .iter()
            .find(|p| p.name == n)
            .cloned()
            .ok_or_else(|| CommandError::BadParams(format!("no CMake profile `{n}`"))),
        None => s.cpp.cmake.active().cloned().ok_or_else(|| CommandError::Failed("no CMake profile".into())),
    }
}

fn tool(s: &Session, name: &str) -> Result<PathBuf, CommandError> {
    super::tool(s, name).map_err(CommandError::Failed)
}

/// `sh -c '<cmake> --build <dir> --target <t> && exec "$@"' <arg0> <exe> <args…>`: build the
/// target, then run it (a test framework's executable runs as a test run).
pub(crate) fn build_then_run(
    cmake: &Path,
    profile: &Profile,
    target: &str,
    exe: &Path,
    args: Vec<String>,
    cwd: &Path,
    framework: Option<Framework>,
) -> ProcessCommand {
    let q = |s: &str| codecraft_cargo::shell_quote(s);
    let script = format!(
        "{} --build {} --target {} && exec \"$@\"",
        q(&cmake.display().to_string()),
        q(&profile.build_dir.display().to_string()),
        q(target)
    );
    let arg0 = if framework.is_some() { crate::testing::cpp::WRAPPER_ARG0 } else { "codecraft-run" };
    let mut cmd = ProcessCommand::shell(script, cwd);
    cmd.args.push(arg0.into());
    cmd.args.push(exe.display().to_string());
    cmd.args.extend(args);
    cmd.env = profile.environment.clone();
    if let Some(key) = framework.and_then(Framework::cpp_key) {
        cmd.env.push((crate::testing::cpp::FRAMEWORK_ENV.into(), key.into()));
    }
    cmd
}

fn target<'a>(s: &'a Session, name: &str) -> Result<&'a cmake::CmakeTarget, CommandError> {
    let model = s
        .cpp
        .cmake
        .codemodel
        .as_ref()
        .ok_or_else(|| CommandError::Failed("the CMake project is not loaded yet (Reload CMake Project)".into()))?;
    model.targets.iter().find(|t| t.name == name).ok_or_else(|| CommandError::BadParams(format!("no CMake target `{name}`")))
}

fn build_command(s: &Session, root: &Path, profile: &Profile, target: Option<&str>) -> Result<ProcessCommand, CommandError> {
    let cmake = tool(s, "cmake")?;
    let mut args: Vec<String> = match &profile.build_preset {
        Some(bp) => vec!["--build".into(), format!("--preset={bp}")],
        None => vec!["--build".into(), profile.build_dir.display().to_string()],
    };
    if let Some(t) = target {
        args.push("--target".into());
        args.push(t.to_string());
    }
    let mut cmd = ProcessCommand::new(cmake.display().to_string(), &[], root);
    cmd.args = args;
    cmd.env = profile.environment.clone();
    cmd.build = true;
    Ok(cmd)
}

// ---------------------------------------------------------------- commands

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("view.toolWindows.cmake", "CMake", always, |s, _| {
            s.ws.layout.toggle(ToolWindowId::CMake);
            ok()
        }),
        spec(
            "cmake.reload",
            "Reload CMake Project",
            None,
            "{profile?: string} — configure the project with the selected (or given) CMake profile; the output \
             is the CMake tool window's log; targets and cache are read when it ends",
            Context::Global,
            is_cmake_project,
            reload,
        ),
        simple("cmake.stop", "Stop CMake Reload", always, |s, _| {
            let id = s.cpp.cmake.reload_job.ok_or_else(|| CommandError::Disabled("no reload".into()))?;
            Ok(json!({ "stopped": s.cpp.jobs.stop(id) }))
        }),
        simple("cmake.profiles", "CMake Profiles", is_cmake_project, |s, _| {
            let root = cmake_project(s)?;
            refresh_profiles(s, &root);
            Ok(json!({
                "profiles": s.cpp.cmake.profiles,
                "selected": s.cpp.cmake.selected,
                "presetErrors": s.cpp.cmake.preset_errors,
            }))
        }),
        spec(
            "cmake.selectProfile",
            "Select CMake Profile",
            None,
            "{profile: string} — the profile (Debug, Release or a configure preset) whose build directory run \
             configurations, C++ tests and the CMake tool window use",
            Context::Global,
            is_cmake_project,
            select_profile,
        ),
        simple("cmake.state", "CMake Project State", always, |s, _| Ok(state_json(s))),
        simple("cmake.targets", "CMake Targets", is_cmake_project, |s, _| {
            let st = &s.cpp.cmake;
            let targets: Vec<Value> = st
                .codemodel
                .iter()
                .flat_map(|m| &m.targets)
                .map(|t| json!({ "name": t.name, "type": t.kind, "artifacts": t.artifacts, "directory": t.directory,
                                 "testFramework": st.frameworks.get(&t.name) }))
                .collect();
            Ok(json!({ "targets": targets, "error": st.codemodel_error, "loading": st.analyzing() }))
        }),
        spec(
            "cmake.buildTarget",
            "Build CMake Target",
            None,
            "{target?: string} — `cmake --build` the target (all targets when omitted) in the Build tool window",
            Context::Global,
            is_cmake_project,
            |s, p| {
                let root = cmake_project(s)?;
                let prof = profile(s, None)?;
                let t = opt_str(p, "target").filter(|t| !t.is_empty());
                if let Some(t) = t {
                    target(s, t)?;
                }
                let cmd = build_command(s, &root, &prof, t)?;
                let title = format!("Build {}", t.unwrap_or("all"));
                let tab = crate::runs::start_process(s, &cmd, &title)?;
                Ok(json!({ "tab": tab.0 }))
            },
        ),
        spec(
            "cmake.runTarget",
            "Run CMake Target",
            None,
            "{target: string, args?: [string]} — build an executable target and run it (a test framework's \
             executable runs in the test runner)",
            Context::Global,
            is_cmake_project,
            run_target,
        ),
        simple("cmake.clean", "Clean CMake Project", is_cmake_project, |s, _| {
            let root = cmake_project(s)?;
            let prof = profile(s, None)?;
            let cmd = build_command(s, &root, &prof, Some("clean"))?;
            let tab = crate::runs::start_process(s, &cmd, "Clean")?;
            Ok(json!({ "tab": tab.0 }))
        }),
        spec(
            "cmake.cache",
            "CMake Cache Variables",
            None,
            "{filter?: string, advanced?: bool} — the CMakeCache.txt variables of the selected profile",
            Context::Global,
            is_cmake_project,
            |s, p| {
                if let Some(f) = opt_str(p, "filter") {
                    s.cpp.cmake.cache_filter = f.chars().take(200).collect();
                }
                if let Some(a) = p.get("advanced").and_then(Value::as_bool) {
                    s.cpp.cmake.show_advanced = a;
                }
                let st = &s.cpp.cmake;
                Ok(json!({ "entries": st.visible_cache(), "error": st.cache_error, "needsReload": st.needs_reload }))
            },
        ),
        spec(
            "cmake.cache.set",
            "Set CMake Cache Variable",
            None,
            "{name: string, value: string, reload?: bool} — change a CMakeCache.txt variable of the selected \
             profile (then Reload CMake Project applies it)",
            Context::Global,
            is_cmake_project,
            set_cache,
        ),
        simple("cmake.ctest.refresh", "Refresh CTest Tests", is_cmake_project, ctest_refresh),
        spec(
            "cmake.showTab",
            "CMake Tool Window Tab",
            None,
            "{tab: log|targets|cache|tests|dependencies}",
            Context::Global,
            always,
            |s, p| {
                let tab: CmakeTab = serde_json::from_value(p.get("tab").cloned().unwrap_or(Value::Null))
                    .map_err(|_| CommandError::BadParams("`tab` must be log, targets, cache, tests or dependencies".into()))?;
                s.cpp.cmake.tab = tab;
                s.ws.layout.show(ToolWindowId::CMake);
                ok()
            },
        ),
        simple("cmake.log", "CMake Reload Log", has_project, |s, _| {
            let job = s.cpp.cmake.reload_job.and_then(|id| s.cpp.jobs.get(id));
            Ok(serde_json::to_value(job).unwrap_or(Value::Null))
        }),
    ]
}

fn state_json(s: &Session) -> Value {
    let st = &s.cpp.cmake;
    let reload = st.reload_job.and_then(|id| s.cpp.jobs.get(id));
    json!({
        "isCmake": st.is_cmake,
        "profiles": st.profiles.iter().map(|p| &p.name).collect::<Vec<_>>(),
        "selected": st.selected,
        "buildDir": st.active().map(|p| &p.build_dir),
        "presetErrors": st.preset_errors,
        "targets": st.codemodel.as_ref().map_or(0, |m| m.targets.len()),
        "testTargets": st.frameworks,
        "codemodelError": st.codemodel_error,
        "cacheEntries": st.cache.len(),
        "ctestTests": st.ctest.len(),
        "reload": reload.map(|j| json!({ "id": j.id, "status": j.status, "lines": j.lines.len() })),
        "loading": st.analyzing(),
        "needsReload": st.needs_reload,
        "tab": st.tab,
    })
}

fn reload(s: &mut Session, p: &Value) -> CmdResult {
    let root = cmake_project(s)?;
    let prof = profile(s, opt_str(p, "profile"))?;
    let cmake = tool(s, "cmake")?;
    if let Some(id) = s.cpp.cmake.reload_job
        && s.cpp.jobs.get(id).is_some_and(|j| j.is_running())
    {
        s.cpp.jobs.stop(id);
    }
    cmake::write_query(&prof.build_dir).map_err(CommandError::Failed)?;
    let args = cmake::configure_args(&root, &prof);
    let id = s
        .cpp
        .jobs
        .start(
            JobKind::CmakeReload { profile: prof.name.clone() },
            &format!("CMake ({})", prof.name),
            &cmake,
            &args,
            &root,
            &prof.environment,
            None,
        )
        .map_err(CommandError::Failed)?;
    s.cpp.cmake.reload_job = Some(id);
    s.cpp.cmake.tab = CmakeTab::Log;
    s.ws.layout.show(ToolWindowId::CMake);
    s.status = format!("Reloading CMake project ({})…", prof.name);
    Ok(json!({ "job": id, "profile": prof.name, "buildDir": prof.build_dir }))
}

fn select_profile(s: &mut Session, p: &Value) -> CmdResult {
    let root = cmake_project(s)?;
    let name = req_str(p, "profile")?.to_string();
    let prof = profile(s, Some(&name))?;
    let mut settings = cmake::load_settings(&root);
    settings.profile = Some(name.clone());
    cmake::save_settings(&root, &settings).map_err(CommandError::Failed)?;
    s.cpp.cmake.selected = Some(name.clone());
    s.cpp.cmake.codemodel = None;
    s.cpp.cmake.cache.clear();
    s.cpp.cmake.frameworks.clear();
    s.cpp.cmake.ctest.clear();
    start_analysis(s);
    refresh_run_configs(s);
    s.status = format!("CMake profile: {name}");
    Ok(json!({ "profile": name, "buildDir": prof.build_dir }))
}

fn run_target(s: &mut Session, p: &Value) -> CmdResult {
    let root = cmake_project(s)?;
    let name = req_str(p, "target")?.to_string();
    let prof = profile(s, None)?;
    let t = target(s, &name)?.clone();
    let exe = t.executable().cloned().ok_or_else(|| CommandError::BadParams(format!("`{name}` is not an executable target")))?;
    let args: Vec<String> = p
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let fw = s.cpp.cmake.frameworks.get(&name).copied();
    let cmake = tool(s, "cmake")?;
    let cwd = exe.parent().map(Path::to_path_buf).unwrap_or(root);
    let cmd = build_then_run(&cmake, &prof, &name, &exe, args, &cwd, fw);
    let title = if fw.is_some() { format!("All in {name}") } else { name.clone() };
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    Ok(json!({ "tab": tab.0, "testFramework": fw }))
}

fn set_cache(s: &mut Session, p: &Value) -> CmdResult {
    cmake_project(s)?;
    let name = req_str(p, "name")?.to_string();
    let value = req_str(p, "value")?.to_string();
    let prof = profile(s, None)?;
    let path = cmake::cache_path(&prof.build_dir);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CommandError::Failed(format!("{}: {e} (reload the CMake project first)", path.display())))?;
    let updated = cmake::set_cache_value(&text, &name, &value).map_err(CommandError::BadParams)?;
    codecraft_fs::write_text_file_atomic(&path, &updated).map_err(|e| CommandError::Failed(e.to_string()))?;
    s.cpp.cmake.cache = cmake::parse_cache(&updated);
    s.cpp.cmake.needs_reload = true;
    if bool_or(p, "reload", false) {
        return reload(s, &Value::Null);
    }
    s.status = format!("{name} = {value} (Reload CMake Project to apply)");
    ok()
}

fn ctest_refresh(s: &mut Session, _: &Value) -> CmdResult {
    let root = cmake_project(s)?;
    let prof = profile(s, None)?;
    let ctest = tool(s, "ctest")?;
    let args = vec!["--show-only=json-v1".to_string(), "--test-dir".into(), prof.build_dir.display().to_string()];
    let id = s
        .cpp
        .jobs
        .start(JobKind::CtestList, "CTest tests", &ctest, &args, &root, &prof.environment, Some(std::time::Duration::from_secs(120)))
        .map_err(CommandError::Failed)?;
    s.cpp.cmake.ctest_job = Some(id);
    Ok(json!({ "job": id }))
}
