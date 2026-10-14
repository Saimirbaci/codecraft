//! C++ test executables: run one (all tests, a suite or one test) in the test runner, list its
//! tests (`--gtest_list_tests`, `--list-tests`, `--list-test-cases`, `--list_content`), and the
//! project's test executables (CMake targets that include a framework, CTest's commands).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::jobs::{JobKind, JobStatus};
use crate::commands::{CmdResult, CommandSpec, Context, has_project, spec};
use crate::params::{bool_or, opt_path, opt_str, req_path};
use crate::runs::ProcessCommand;
use crate::testing::Framework;
use crate::testing::cpp::{self as cpp_tests, ListedTest};
use crate::{CommandError, Session};

/// A test listing of an executable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub framework: Option<Framework>,
    pub tests: Vec<ListedTest>,
    pub error: Option<String>,
    pub pending: bool,
}

/// The framework of an executable: the codemodel target that builds it, else CTest's commands.
pub(crate) fn detect_framework(s: &Session, exe: &Path) -> Option<Framework> {
    let st = &s.cpp.cmake;
    let by_target = st
        .codemodel
        .as_ref()
        .and_then(|m| m.targets.iter().find(|t| t.executable().is_some_and(|a| a == exe)))
        .and_then(|t| st.frameworks.get(&t.name).copied());
    by_target.or_else(|| {
        super::cmake::test_executables(st.codemodel.as_ref(), &st.frameworks, &st.ctest)
            .into_iter()
            .find(|e| e.path == exe)
            .and_then(|e| e.framework)
    })
}

fn framework_param(s: &Session, p: &Value, exe: &Path) -> Result<Framework, CommandError> {
    match opt_str(p, "framework") {
        Some(f) => Framework::parse_cpp(f)
            .ok_or_else(|| CommandError::BadParams(format!("unknown test framework `{f}` (gtest, catch2, doctest, boost)"))),
        None => detect_framework(s, exe).ok_or_else(|| {
            CommandError::BadParams(format!(
                "which test framework does {} use? Pass `framework` (gtest, catch2, doctest, boost)",
                exe.display()
            ))
        }),
    }
}

fn string_list(p: &Value, key: &str) -> Vec<String> {
    p.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).take(500).collect())
        .unwrap_or_default()
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "cpp.tests.run",
            "Run C++ Tests",
            None,
            "{executable: path, framework?: gtest|catch2|doctest|boost, suite?: string, test?: string, \
             args?: [string], cwd?: path, target?: string (build this CMake target first)} — run a test \
             executable in the test runner (tree, failures linked to source, rerun failed) → {tab}",
            Context::Global,
            has_project,
            run,
        ),
        spec(
            "cpp.tests.list",
            "List C++ Tests",
            None,
            "{executable: path, framework?: string, refresh?: bool} — the executable's tests \
             (`--gtest_list_tests`, `--list-tests`, `--list-test-cases`, `--list_content`); listed in the \
             background: poll until `pending` is false",
            Context::Global,
            has_project,
            list,
        ),
        spec(
            "cpp.tests.executables",
            "C++ Test Executables",
            None,
            "The project's test executables: CMake executable targets whose sources include GoogleTest, \
             Catch2, doctest or Boost.Test, and CTest's test commands (cmake.ctest.refresh)",
            Context::Global,
            has_project,
            |s, _| {
                let st = &s.cpp.cmake;
                Ok(json!(super::cmake::test_executables(st.codemodel.as_ref(), &st.frameworks, &st.ctest)))
            },
        ),
    ]
}

fn run(s: &mut Session, p: &Value) -> CmdResult {
    let exe = super::resolve_path(s, &req_path(p, "executable")?);
    let fw = framework_param(s, p, &exe)?;
    let suite = opt_str(p, "suite").filter(|x| !x.is_empty());
    let test = opt_str(p, "test").filter(|x| !x.is_empty());
    let mut args = cpp_tests::filter_args(fw, suite, test);
    args.extend(string_list(p, "args"));
    let cwd = opt_path(p, "cwd")
        .map(|c| super::resolve_path(s, &c))
        .or_else(|| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let cmd = match opt_str(p, "target").filter(|t| !t.is_empty()) {
        Some(target) => {
            let cmake = super::tool(s, "cmake").map_err(CommandError::Failed)?;
            let profile = s
                .cpp
                .cmake
                .active()
                .cloned()
                .ok_or_else(|| CommandError::Failed("not a CMake project".into()))?;
            super::cmake_cmds::build_then_run(&cmake, &profile, target, &exe, args, &cwd, Some(fw))
        }
        None => {
            if !exe.is_file() {
                return Err(CommandError::Failed(format!("{} does not exist (build it first)", exe.display())));
            }
            let mut cmd = ProcessCommand::new(exe.display().to_string(), &[], cwd);
            cmd.args = args;
            cmd.env.push((cpp_tests::FRAMEWORK_ENV.into(), fw.cpp_key().unwrap_or("gtest").into()));
            cmd
        }
    };
    let exe_name = super::jobs::exe_label(&exe);
    let title = match (fw, suite, test) {
        (Framework::GoogleTest, Some(s), Some(t)) => format!("{s}.{t}"),
        (_, _, Some(t)) => t.to_string(),
        (_, Some(s), None) => format!("All in {s}"),
        _ => format!("All in {exe_name}"),
    };
    let tab = crate::runs::start_process(s, &cmd, &title)?;
    Ok(json!({ "tab": tab.0, "framework": fw, "commandLine": cmd.command_line() }))
}

fn list(s: &mut Session, p: &Value) -> CmdResult {
    let exe = super::resolve_path(s, &req_path(p, "executable")?);
    if let Some(l) = s.cpp.listings.get(&exe)
        && (l.pending || !bool_or(p, "refresh", false))
    {
        return Ok(json!(l));
    }
    let fw = framework_param(s, p, &exe)?;
    if !exe.is_file() {
        return Err(CommandError::Failed(format!("{} does not exist (build it first)", exe.display())));
    }
    let args: Vec<String> = cpp_tests::list_args(fw).into_iter().map(str::to_string).collect();
    let cwd = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let env = vec![(cpp_tests::FRAMEWORK_ENV.to_string(), fw.cpp_key().unwrap_or("").to_string())];
    let id = s
        .cpp
        .jobs
        .start(
            JobKind::ListTests { executable: exe.clone(), framework: fw },
            &format!("List tests of {}", super::jobs::exe_label(&exe)),
            &exe,
            &args,
            &cwd,
            &env,
            Some(Duration::from_secs(60)),
        )
        .map_err(CommandError::Failed)?;
    let listing = Listing { framework: Some(fw), tests: Vec::new(), error: None, pending: true };
    s.cpp.listings.insert(exe, listing.clone());
    Ok(json!({ "job": id, "framework": fw, "tests": [], "pending": true }))
}

pub(crate) fn list_finished(s: &mut Session, id: u64, exe: &Path, fw: Framework) {
    let Some(job) = s.cpp.jobs.get(id) else { return };
    let tests = cpp_tests::parse_list(fw, &job.stdout, &job.stderr);
    let error = match job.status {
        JobStatus::Succeeded => None,
        _ if !tests.is_empty() => None,
        JobStatus::Failed(code) => Some(format!(
            "listing failed (exit code {})",
            code.map_or("signal".to_string(), |c| c.to_string())
        )),
        JobStatus::Stopped => Some("stopped".into()),
        JobStatus::Running => None,
    };
    s.cpp.listings.insert(exe.to_path_buf(), Listing { framework: Some(fw), tests, error, pending: false });
}
