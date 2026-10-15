//! "All tests in file" and Debug from the gutter for C/C++ tests (M13 track RC).
//!
//! `run.testsInFile` / `debug.testsInFile` run (or debug) every test of a file: the Rust `#[test]`
//! functions it declares (`cargo test -- --exact a b …`), a pytest, jest/vitest or Zig test file,
//! or the GoogleTest/Catch2/doctest/Boost.Test tests of a C/C++ source (one combined filter for
//! the executable target that compiles it). [`cpp_debug_target`] gives Debug for the C/C++ test
//! (or `main`) at a line: its CMake target is built, then the executable runs under lldb-dap
//! with that test's filter.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_syntax::{Language, SymbolKind};

use crate::commands::{CmdResult, CommandSpec, Context, has_project, spec};
use crate::debug::DebugTarget;
use crate::params::opt_path;
use crate::testing::Framework;
use crate::testing::cpp::{self as cpp_tests, TestMacro};
use crate::{CommandError, Session};

fn file_param(s: &Session, p: &Value) -> Result<PathBuf, CommandError> {
    if let Some(path) = opt_path(p, "path") {
        return Ok(crate::cpp::resolve_path(s, &path));
    }
    let (_, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    d.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved".into()))
}

fn text_of(s: &Session, path: &Path) -> Result<String, CommandError> {
    match s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        Some(d) => Ok(d.doc.text()),
        None => {
            codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))
        }
    }
}

/// One filter that selects every test in `macros` (suites are skipped: their tests are listed).
pub fn file_filter_args(fw: Framework, macros: &[TestMacro]) -> Vec<String> {
    let parts: Vec<String> = macros
        .iter()
        .filter(|m| !m.is_suite)
        .filter_map(|m| cpp_tests::filter_args(fw, m.filter.0.as_deref(), m.filter.1.as_deref()).into_iter().last())
        .collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let strip = |prefix: &str| -> Vec<String> {
        parts.iter().map(|p| p.strip_prefix(prefix).unwrap_or(p).to_string()).collect()
    };
    match fw {
        Framework::GoogleTest => vec![format!("--gtest_filter={}", strip("--gtest_filter=").join(":"))],
        Framework::Catch2 => vec![parts.join(",")],
        Framework::Doctest => vec![format!("--test-case={}", strip("--test-case=").join(","))],
        Framework::BoostTest => vec![format!("--run_test={}", strip("--run_test=").join(":"))],
        _ => Vec::new(),
    }
}

/// The executable target that compiles `path`, its build directory and the configured CMake.
struct CppExe {
    target: String,
    exe: PathBuf,
    profile: crate::cpp::cmake::Profile,
}

fn cpp_exe(root: &Path, path: &Path) -> Result<CppExe, String> {
    let profile = crate::cpp::cmake::active_profile(root).ok_or("this is not a CMake project")?;
    let model = crate::cpp::cmake::cached_codemodel(root)
        .ok_or("the CMake project is not loaded yet (CMake tool window › Reload CMake Project)")?;
    let target = model
        .targets_for_source(path)
        .into_iter()
        .find(|t| t.is_executable())
        .ok_or_else(|| format!("no CMake executable target compiles {}", path.display()))?;
    let exe = target.executable().cloned().ok_or("the target has no executable")?;
    Ok(CppExe { target: target.name.clone(), exe, profile })
}

/// Debug the C/C++ test (or test file, `all`) at a line: build its target, then debug the
/// executable with the test's filter. `None` when the line has no test and the file no `main`.
pub(crate) fn cpp_debug_target(
    s: &Session,
    path: &Path,
    line: usize,
    all: bool,
) -> Option<Result<DebugTarget, String>> {
    let root = s.project_root()?;
    let text = text_of(s, path).ok()?;
    let macros = cpp_tests::test_macros(&text);
    let fw = macros.first().map(|m| m.framework);
    let (name, args) = match fw {
        Some(fw) if all => (
            format!("All in {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            file_filter_args(fw, &macros),
        ),
        Some(_) => {
            let m = cpp_tests::macro_at(&text, line)?;
            (m.name.clone(), cpp_tests::filter_args(m.framework, m.filter.0.as_deref(), m.filter.1.as_deref()))
        }
        // No tests: a program with `main` (the run configuration's compile, then debug).
        None => {
            let config = crate::run_configs::context_config(&root, path, &text, line)?;
            let cmd = config.command.as_ref()?;
            return crate::debug_cmds::process_target(&config.name, cmd).map(Ok);
        }
    };
    Some(cpp_exe(&root, path).map(|e| {
        let cmake = crate::cpp::tool(s, "cmake").map(|p| p.display().to_string()).unwrap_or_else(|_| "cmake".into());
        DebugTarget::Executable {
            name,
            program: e.exe.clone(),
            args,
            cwd: e.exe.parent().map(Path::to_path_buf),
            env: e.profile.environment.clone(),
            build: vec![
                cmake,
                "--build".into(),
                e.profile.build_dir.display().to_string(),
                "--target".into(),
                e.target,
            ],
        }
    }))
}

/// The libtest paths of the `#[test]` functions declared in a Rust file, and the invocation of
/// the target that contains them.
fn rust_tests(s: &Session, path: &Path, text: &str) -> Option<(Vec<String>, codecraft_cargo::CargoInvocation)> {
    let outline = codecraft_syntax::outline(Language::Rust, text);
    let lines: Vec<&str> = text.lines().collect();
    let mut names = Vec::new();
    let mut base = None;
    for item in outline.iter().filter(|i| i.kind == SymbolKind::Function) {
        // The attributes (and comments) right above the function.
        let attr = lines
            .get(item.line.saturating_sub(6)..item.line)
            .unwrap_or_default()
            .iter()
            .rev()
            .map(|l| l.trim_start())
            .take_while(|t| t.starts_with("#[") || t.starts_with("//"))
            .any(|t| t.starts_with("#[test]") || t.starts_with("#[tokio::test") || t.contains("::test]"));
        if !attr {
            continue;
        }
        let Some((_, inv)) = crate::run_cmds::context_invocation(s, path, item.line) else { continue };
        if let Some(f) = inv.test_filter.clone().filter(|f| !names.contains(f)) {
            names.push(f);
        }
        base.get_or_insert(inv);
        if names.len() >= 2000 {
            break;
        }
    }
    let mut inv = base?;
    inv.test_filter = None;
    inv.test_exact = false;
    inv.program_args = std::iter::once("--exact".to_string()).chain(names.iter().cloned()).collect();
    Some((names, inv))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Run (or debug) every test in a file.
fn tests_in_file(s: &mut Session, p: &Value, debug: bool) -> CmdResult {
    let path = file_param(s, p)?;
    let text = text_of(s, &path)?;
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let title = format!("All in {}", file_name(&path));
    let reply = |tab: Option<codecraft_workspace::RunTabId>| -> CmdResult {
        Ok(json!({ "title": title.clone(), "tab": tab.map(|t| t.0), "debug": debug }))
    };
    match Language::from_path(&path) {
        Language::Rust => {
            let (names, inv) =
                rust_tests(s, &path, &text).ok_or_else(|| CommandError::Failed("no tests in this file".into()))?;
            if debug {
                crate::debug::start(s, DebugTarget::Cargo { name: title.clone(), invocation: inv })?;
                return reply(None);
            }
            let tab = crate::runs::start(s, inv, &title, false)?;
            let mut v = reply(Some(tab))?;
            v["tests"] = json!(names);
            Ok(v)
        }
        Language::C | Language::Cpp => {
            let macros = cpp_tests::test_macros(&text);
            let fw = macros.first().map(|m| m.framework).ok_or_else(|| {
                CommandError::Failed("no GoogleTest, Catch2, doctest or Boost.Test tests in this file".into())
            })?;
            if debug {
                let target = cpp_debug_target(s, &path, 0, true)
                    .ok_or_else(|| CommandError::Failed("nothing to debug".into()))?
                    .map_err(CommandError::Failed)?;
                crate::debug::start(s, target)?;
                return reply(None);
            }
            let e = cpp_exe(&root, &path).map_err(CommandError::Failed)?;
            let cmake = crate::cpp::tool(s, "cmake").map_err(CommandError::Failed)?;
            let cwd = e.exe.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone());
            let cmd = crate::cpp::cmake_cmds::build_then_run(
                &cmake,
                &e.profile,
                &e.target,
                &e.exe,
                file_filter_args(fw, &macros),
                &cwd,
                Some(fw),
            );
            let tab = crate::runs::start_process(s, &cmd, &title)?;
            reply(Some(tab))
        }
        Language::Python if debug => {
            let python = crate::python_env::project_python(&root);
            let target = DebugTarget::Python {
                name: title.clone(),
                program: None,
                module: Some("pytest".into()),
                args: vec![path.display().to_string()],
                cwd: Some(root.clone()),
                env: Vec::new(),
                python,
                just_my_code: true,
            };
            crate::debug::start(s, target)?;
            reply(None)
        }
        Language::Python | Language::JavaScript | Language::TypeScript | Language::Zig if !debug => {
            let config = whole_file_config(&root, &path, &text)
                .ok_or_else(|| CommandError::Failed("this is not a test file".into()))?;
            let tab = crate::runs::start_config(s, &config)?;
            reply(Some(tab))
        }
        _ => Err(CommandError::Failed(if debug {
            "Debug All Tests in File works for Rust, C/C++ and Python test files".into()
        } else {
            "no tests in this file".into()
        })),
    }
}

/// The run configuration of a whole non-Rust test file.
fn whole_file_config(root: &Path, path: &Path, text: &str) -> Option<crate::RunConfig> {
    let file = path.display().to_string();
    let name = file_name(path);
    match Language::from_path(path) {
        Language::Python => {
            let py = crate::run_configs::python_for(root);
            Some(crate::RunConfig::process(
                format!("pytest {name}"),
                crate::ProcessCommand::new(py, &["-m", "pytest", file.as_str()], root),
            ))
        }
        Language::JavaScript | Language::TypeScript if crate::run_configs::is_js_test_file(path) => {
            let (label, cmd) = crate::run_configs::js_test_command(root, &file, None);
            Some(crate::RunConfig::process(label, cmd))
        }
        Language::Zig if text.lines().any(|l| l.trim_start().starts_with("test ")) => Some(crate::RunConfig::process(
            format!("zig test {name}"),
            crate::ProcessCommand::new("zig", &["test", file.as_str()], root),
        )),
        _ => None,
    }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "run.testsInFile",
            "Run All Tests in File",
            None,
            "{path?: string} — run every test of the file (the active editor's by default): Rust #[test] \
             functions, a pytest/jest/vitest/Zig test file, or a C/C++ file's GoogleTest/Catch2/doctest/Boost.Test \
             tests → {title, tab}",
            Context::Global,
            has_project,
            |s, p| tests_in_file(s, p, false),
        ),
        spec(
            "debug.testsInFile",
            "Debug All Tests in File",
            None,
            "{path?: string} — debug every test of a Rust, C/C++ or Python test file",
            Context::Global,
            has_project,
            |s, p| tests_in_file(s, p, true),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(fw: Framework, suite: Option<&str>, test: &str) -> TestMacro {
        TestMacro {
            line: 0,
            framework: fw,
            is_suite: false,
            suite: suite.map(str::to_string),
            name: test.into(),
            filter: (suite.map(str::to_string), Some(test.into())),
        }
    }

    #[test]
    fn one_filter_for_all_tests_in_a_file() {
        let g = [m(Framework::GoogleTest, Some("Math"), "Adds"), m(Framework::GoogleTest, Some("Io"), "Reads")];
        assert_eq!(file_filter_args(Framework::GoogleTest, &g), vec!["--gtest_filter=Math.Adds:Io.Reads"]);
        let b = [m(Framework::BoostTest, Some("s"), "a"), m(Framework::BoostTest, None, "b")];
        assert_eq!(file_filter_args(Framework::BoostTest, &b), vec!["--run_test=s/a:b"]);
        let d = [m(Framework::Doctest, None, "x"), m(Framework::Doctest, None, "y")];
        assert_eq!(file_filter_args(Framework::Doctest, &d), vec!["--test-case=x,y"]);
        let mut suite = m(Framework::GoogleTest, Some("S"), "T");
        suite.is_suite = true;
        assert!(file_filter_args(Framework::GoogleTest, &[suite]).is_empty());
    }
}
