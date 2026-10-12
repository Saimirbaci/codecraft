//! Run configurations for every ecosystem the project contains (Cargo is in `runs.rs`), and
//! "run the thing at the caret" for non-Rust files.

use std::path::{Path, PathBuf};

use serde_json::Value;

use codecraft_syntax::{Language, SymbolKind};

use crate::runs::{ProcessCommand, RunConfig};

/// CLion's default CMake build directory for the Debug profile.
pub const CMAKE_BUILD_DIR: &str = "cmake-build-debug";

/// The Python interpreter to use: the project's virtualenv, else `python3`.
pub fn python_for(root: &Path) -> String {
    for venv in [".venv/bin/python", "venv/bin/python", ".env/bin/python"] {
        let p = root.join(venv);
        if p.is_file() {
            return p.display().to_string();
        }
    }
    "python3".into()
}

/// The JavaScript package manager from the lockfile.
fn package_manager(root: &Path) -> &'static str {
    if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
        "bun"
    } else if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else {
        "npm"
    }
}

/// `add_executable(name …)` targets of a CMakeLists.txt (simple scan; generator expressions and
/// variables in names are skipped).
pub fn cmake_executables(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|t| t.find("add_executable(")) {
        let start = from + i + "add_executable(".len();
        let name: String = text
            .get(start..)
            .unwrap_or("")
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
            .collect();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        from = start;
    }
    out
}

/// Explicit targets of a Makefile (`name:` at line start; no pattern rules, no `.SPECIAL`).
pub fn make_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else { continue };
        if rest.starts_with('=') || line.starts_with(['\t', ' ', '#', '.']) {
            continue;
        }
        let name = name.trim();
        if !name.is_empty()
            && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
            && !out.contains(&name.to_string())
        {
            out.push(name.to_string());
        }
        if out.len() >= 30 {
            break;
        }
    }
    out
}

/// Run configurations for the non-Cargo ecosystems found in `root`.
pub fn derive_polyglot(root: &Path) -> Vec<RunConfig> {
    let mut out = Vec::new();
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();

    // CMake (CLion conventions).
    if let Some(cmake) = read("CMakeLists.txt") {
        let configure =
            format!("cmake -S . -B {CMAKE_BUILD_DIR} -DCMAKE_BUILD_TYPE=Debug -DCMAKE_EXPORT_COMPILE_COMMANDS=ON");
        let mut build = ProcessCommand::shell(format!("{configure} && cmake --build {CMAKE_BUILD_DIR}"), root);
        build.build = true;
        out.push(RunConfig::process("CMake: Build", build));
        for exe in cmake_executables(&cmake) {
            let script =
                format!("{configure} && cmake --build {CMAKE_BUILD_DIR} --target {exe} && ./{CMAKE_BUILD_DIR}/{exe}");
            out.push(RunConfig::process(format!("Run {exe}"), ProcessCommand::shell(script, root)));
        }
        if cmake.to_ascii_lowercase().contains("enable_testing") {
            let script = format!(
                "{configure} && cmake --build {CMAKE_BUILD_DIR} && ctest --test-dir {CMAKE_BUILD_DIR} --output-on-failure"
            );
            out.push(RunConfig::process("CTest", ProcessCommand::shell(script, root)));
        }
    }

    // Make.
    if let Some(make) = read("Makefile").or_else(|| read("makefile")) {
        let mut all = ProcessCommand::new("make", &[], root);
        all.build = true;
        out.push(RunConfig::process("make", all));
        for t in make_targets(&make).into_iter().filter(|t| t != "all") {
            out.push(RunConfig::process(format!("make {t}"), ProcessCommand::new("make", &[t.as_str()], root)));
        }
    }

    // Zig.
    if root.join("build.zig").is_file() {
        let mut build = ProcessCommand::new("zig", &["build"], root);
        build.build = true;
        out.push(RunConfig::process("zig build", build));
        out.push(RunConfig::process("zig build run", ProcessCommand::new("zig", &["build", "run"], root)));
        out.push(RunConfig::process("zig build test", ProcessCommand::new("zig", &["build", "test"], root)));
    }

    // JavaScript / TypeScript package scripts.
    if let Some(pkg) = read("package.json").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        let pm = package_manager(root);
        if let Some(scripts) = pkg.get("scripts").and_then(Value::as_object) {
            for name in scripts.keys().take(40) {
                let mut cmd = ProcessCommand::new(pm, &["run", name.as_str()], root);
                cmd.build = name == "build";
                out.push(RunConfig::process(format!("{pm} run {name}"), cmd));
            }
        }
    }
    // Deno tasks.
    for f in ["deno.json", "deno.jsonc"] {
        if let Some(cfg) = read(f).and_then(|t| serde_json::from_str::<Value>(&t).ok())
            && let Some(tasks) = cfg.get("tasks").and_then(Value::as_object)
        {
            for name in tasks.keys().take(40) {
                out.push(RunConfig::process(
                    format!("deno task {name}"),
                    ProcessCommand::new("deno", &["task", name.as_str()], root),
                ));
            }
        }
    }

    // Python.
    let pyproject = read("pyproject.toml");
    let python_project =
        pyproject.is_some() || root.join("requirements.txt").is_file() || root.join("setup.py").is_file();
    if python_project {
        let py = python_for(root);
        let has_tests = root.join("tests").is_dir()
            || root.join("test").is_dir()
            || pyproject.as_deref().is_some_and(|t| t.contains("pytest"));
        if has_tests {
            out.push(RunConfig::process("pytest", ProcessCommand::new(py.clone(), &["-m", "pytest"], root)));
        }
        for main in ["main.py", "app.py", "manage.py"] {
            if root.join(main).is_file() {
                out.push(RunConfig::process(format!("Run {main}"), ProcessCommand::new(py.clone(), &[main], root)));
            }
        }
    }
    out
}

/// The configuration for "build the project" when it is not a Cargo project.
pub fn build_config(root: &Path) -> Option<RunConfig> {
    derive_polyglot(root).into_iter().find(|c| c.command.as_ref().is_some_and(|cmd| cmd.build))
}

/// What to run for a line of a non-Rust file (the Rust case is in `run_cmds.rs`).
pub fn context_config(root: &Path, path: &Path, text: &str, line: usize) -> Option<RunConfig> {
    let language = Language::from_path(path);
    let file = path.display().to_string();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let cwd: PathBuf = root.to_path_buf();
    match language {
        Language::Python => {
            let py = python_for(root);
            // A pytest test function at or above the caret line: from the outline, else a scan
            // for `def test…` / `async def test…`.
            let from_outline = codecraft_syntax::outline(language, text)
                .into_iter()
                .filter(|i| i.kind == SymbolKind::Function && i.line <= line && i.name.starts_with("test"))
                .max_by_key(|i| i.line)
                .map(|i| i.name);
            let test = from_outline.or_else(|| {
                text.lines()
                    .take(line + 1)
                    .filter_map(|l| {
                        let t = l.trim_start();
                        let t = t.strip_prefix("async ").unwrap_or(t);
                        let name: String =
                            t.strip_prefix("def ")?.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                        name.starts_with("test").then_some(name)
                    })
                    .last()
            });
            let looks_like_test = name.starts_with("test_") || name.ends_with("_test.py");
            match test {
                Some(t) if looks_like_test => {
                    let node = format!("{file}::{t}");
                    Some(RunConfig::process(
                        format!("pytest {t}"),
                        ProcessCommand::new(py, &["-m", "pytest", node.as_str()], cwd),
                    ))
                }
                _ if looks_like_test => Some(RunConfig::process(
                    format!("pytest {name}"),
                    ProcessCommand::new(py, &["-m", "pytest", file.as_str()], cwd),
                )),
                _ => Some(RunConfig::process(format!("Run {name}"), ProcessCommand::new(py, &[file.as_str()], cwd))),
            }
        }
        Language::Zig => {
            // `test "name" {` at or above the caret.
            let test = text
                .lines()
                .take(line + 1)
                .filter_map(|l| {
                    let t = l.trim_start().strip_prefix("test")?.trim_start();
                    let q = t.strip_prefix('"')?;
                    q.split_once('"').map(|(n, _)| n.to_string())
                })
                .last();
            match test {
                Some(t) => Some(RunConfig::process(
                    format!("zig test \"{t}\""),
                    ProcessCommand::new("zig", &["test", file.as_str(), "--test-filter", t.as_str()], cwd),
                )),
                None if text.contains("pub fn main") => Some(RunConfig::process(
                    format!("zig run {name}"),
                    ProcessCommand::new("zig", &["run", file.as_str()], cwd),
                )),
                None => Some(RunConfig::process(
                    format!("zig test {name}"),
                    ProcessCommand::new("zig", &["test", file.as_str()], cwd),
                )),
            }
        }
        Language::JavaScript => {
            Some(RunConfig::process(format!("node {name}"), ProcessCommand::new("node", &[file.as_str()], cwd)))
        }
        Language::TypeScript => {
            if root.join("deno.json").is_file() || root.join("deno.jsonc").is_file() {
                Some(RunConfig::process(
                    format!("deno run {name}"),
                    ProcessCommand::new("deno", &["run", "-A", file.as_str()], cwd),
                ))
            } else {
                Some(RunConfig::process(
                    format!("node {name}"),
                    ProcessCommand::new("node", &["--experimental-strip-types", "--no-warnings", file.as_str()], cwd),
                ))
            }
        }
        Language::C | Language::Cpp => {
            if !text.contains("main(") {
                return None;
            }
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "a".into());
            let out_dir = root.join(".codecraft/build");
            let exe = out_dir.join(&stem);
            let (cc, std) = if language == Language::Cpp { ("clang++", "-std=c++20") } else { ("clang", "-std=c17") };
            let script = format!(
                "mkdir -p {dir} && {cc} {std} -g -Wall {src} -o {exe} && {exe}",
                dir = codecraft_cargo::shell_quote(&out_dir.display().to_string()),
                src = codecraft_cargo::shell_quote(&file),
                exe = codecraft_cargo::shell_quote(&exe.display().to_string()),
            );
            Some(RunConfig::process(format!("Run {name}"), ProcessCommand::shell(script, cwd)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmake_and_make_scans() {
        let cm = "project(x)\nadd_executable(app main.cpp)\nADD_EXECUTABLE( tool tool.c )\nadd_library(lib a.c)\n";
        assert_eq!(cmake_executables(cm), vec!["app", "tool"]);
        let mk = ".PHONY: all\nall: app\n\ncc -o app\napp: main.o\n\ttest: ignored\nCC := clang\nclean:\n%.o: %.c\n";
        assert_eq!(make_targets(mk), vec!["all", "app", "clean"]);
    }

    #[test]
    fn polyglot_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("CMakeLists.txt"), "add_executable(demo main.cpp)\nenable_testing()\n").unwrap();
        std::fs::write(root.join("build.zig"), "").unwrap();
        std::fs::write(root.join("package.json"), r#"{"scripts":{"build":"tsc","test":"vitest"}}"#).unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(root.join("pyproject.toml"), "[tool.pytest.ini_options]\n").unwrap();
        let names: Vec<String> = derive_polyglot(root).into_iter().map(|c| c.name).collect();
        for want in ["CMake: Build", "Run demo", "CTest", "zig build run", "pnpm run build", "pnpm run test", "pytest"]
        {
            assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
        }
        assert_eq!(build_config(root).map(|c| c.name), Some("CMake: Build".to_string()));
    }

    #[test]
    fn context_runs() {
        let root = Path::new("/p");
        let py = "import x\n\ndef test_add():\n    assert 1\n";
        let c = context_config(root, Path::new("/p/tests/test_math.py"), py, 3).unwrap();
        assert!(c.command_line().contains("/p/tests/test_math.py::test_add"), "{}", c.command_line());
        let c = context_config(root, Path::new("/p/app.py"), "print(1)\n", 0).unwrap();
        assert!(c.command_line().ends_with("/p/app.py"));
        let zig = "const std = @import(\"std\");\ntest \"adds\" {\n}\n";
        let c = context_config(root, Path::new("/p/a.zig"), zig, 2).unwrap();
        assert!(c.command_line().contains("--test-filter adds"), "{}", c.command_line());
        let c = context_config(root, Path::new("/p/x.ts"), "console.log(1)", 0).unwrap();
        assert!(c.command_line().contains("--experimental-strip-types"));
        assert!(context_config(root, Path::new("/p/lib.c"), "int f(void) { return 1; }", 0).is_none());
        assert!(context_config(root, Path::new("/p/main.c"), "int main(void) { return 0; }", 0).is_some());
    }
}
