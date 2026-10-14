//! CLion features (M12 track CL): CMake profiles, presets and the CMake tool window
//! (`cmake.rs`, `cmake_cmds.rs`), C++ test executables (`tests_cmds.rs`; the frameworks
//! themselves are in `testing/cpp.rs`), Valgrind Memcheck (`valgrind.rs`), sanitizer reports
//! (`sanitizers.rs`), Conan/vcpkg (`deps.rs`) and the debugger items Attach to an Unstarted
//! Process, Reset Frame and Manage Targets (`debugging.rs`).
//!
//! External tools (`cmake`, `ctest`, `valgrind`, `conan`, `vcpkg`) are found on `PATH` unless
//! [`CppState::tools`] names them (tests use fake scripts). An isolated session
//! ([`crate::Session::isolate_home`]) never looks tools up on `PATH`, so fuzzing commands cannot
//! start real ones.

pub mod cmake;
mod cmake_cmds;
pub mod debugging;
pub mod deps;
pub mod jobs;
pub mod sanitizers;
mod tests_cmds;
pub mod valgrind;
pub mod xml;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::commands::CommandSpec;
use crate::Session;

pub use cmake_cmds::CmakeTab;
pub use tests_cmds::Listing;

/// Everything track CL keeps per session.
#[derive(Default)]
pub struct CppState {
    /// Tool overrides: name (`cmake`, `ctest`, `valgrind`, `conan`, `vcpkg`) → executable.
    pub tools: BTreeMap<String, PathBuf>,
    /// Never look tools up on `PATH` (isolated sessions: tests, fuzzing).
    pub isolated: bool,
    pub cmake: cmake_cmds::CmakeState,
    pub valgrind: valgrind::ValgrindState,
    pub sanitizers: sanitizers::SanitizerState,
    pub deps: deps::DepsState,
    pub jobs: jobs::Jobs,
    /// Test listings by executable (`cpp.tests.list`).
    pub listings: BTreeMap<PathBuf, Listing>,
    /// Run › Attach to an Unstarted Process… dialog and Run › Manage Targets… dialog.
    pub debugging: debugging::DebuggingState,
}

impl CppState {
    pub fn busy(&self) -> bool {
        self.jobs.busy() || self.cmake.analyzing()
    }
}

fn install_hint(name: &str) -> String {
    match name {
        "cmake" | "ctest" => "CMake is not installed (https://cmake.org/download/ or your package manager)".into(),
        "valgrind" => "Valgrind is not installed (on Linux: your package manager's `valgrind`)".into(),
        "conan" => "Conan is not installed (pip install conan)".into(),
        "vcpkg" => "vcpkg is not installed (https://vcpkg.io; set VCPKG_ROOT)".into(),
        other => format!("{other} is not installed"),
    }
}

/// The executable for a tool: an override, else (outside isolated sessions) `PATH`.
pub(crate) fn tool(s: &Session, name: &str) -> Result<PathBuf, String> {
    if let Some(p) = s.cpp.tools.get(name) {
        return Ok(p.clone());
    }
    if s.cpp.isolated {
        return Err(format!("{name} is not available in this session"));
    }
    if name == "vcpkg"
        && let Some(root) = std::env::var_os("VCPKG_ROOT").map(PathBuf::from)
    {
        let exe = root.join(if cfg!(windows) { "vcpkg.exe" } else { "vcpkg" });
        if exe.is_file() {
            return Ok(exe);
        }
    }
    codecraft_lsp::which(name).ok_or_else(|| install_hint(name))
}

/// The project root, or a Disabled error.
pub(crate) fn project_root(s: &Session) -> Result<PathBuf, crate::CommandError> {
    s.ws
        .project
        .as_ref()
        .map(|p| p.root.clone())
        .ok_or_else(|| crate::CommandError::Disabled("no project is open".into()))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let mut v = cmake_cmds::specs();
    v.extend(tests_cmds::specs());
    v.extend(valgrind::specs());
    v.extend(sanitizers::specs());
    v.extend(deps::specs());
    v.extend(debugging::specs());
    v
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let (mut changed, finished) = s.cpp.jobs.pump();
    for id in finished {
        on_job_finished(s, id);
        changed = true;
    }
    changed |= cmake_cmds::pump(s);
    changed |= sanitizers::pump(s);
    changed |= valgrind::pump(s);
    changed
}

fn on_job_finished(s: &mut Session, id: u64) {
    let Some(job) = s.cpp.jobs.get(id) else { return };
    let kind = job.kind.clone();
    let ok = job.status == jobs::JobStatus::Succeeded;
    match kind {
        jobs::JobKind::CmakeReload { profile } => cmake_cmds::reload_finished(s, id, &profile, ok),
        jobs::JobKind::CtestList => cmake_cmds::ctest_finished(s, id, ok),
        jobs::JobKind::Dependencies { manager } => deps::install_finished(s, id, &manager, ok),
        jobs::JobKind::ListTests { executable, framework } => {
            tests_cmds::list_finished(s, id, &executable, framework)
        }
    }
}

pub(crate) fn shutdown(s: &mut Session) {
    s.cpp.jobs.stop_all();
}

/// An absolute path for a user-given one (relative = from the project root).
pub(crate) fn resolve_path(s: &Session, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match s.project_root() {
        Some(root) => crate::normalize_path(&root.join(p)),
        None => crate::normalize_path(p),
    }
}
