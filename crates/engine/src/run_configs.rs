//! Run configurations for every ecosystem the project contains (Cargo is in `runs.rs`), and
//! "run the thing at the caret" for non-Rust files.

use std::path::{Path, PathBuf};

use serde_json::Value;

use codecraft_syntax::{Language, SymbolKind};

use crate::runs::{ProcessCommand, RunConfig};

/// CLion's default CMake build directory for the Debug profile.
pub const CMAKE_BUILD_DIR: &str = "cmake-build-debug";

/// The Python interpreter to use: the project's selected interpreter (Python status bar widget),
/// else its virtualenv, else `python3`.
pub fn python_for(root: &Path) -> String {
    crate::python_env::project_python(root).map(|p| p.display().to_string()).unwrap_or_else(|| "python3".into())
}

/// The interpreter for a configuration that runs in `cwd`: the project's selected interpreter,
/// else the virtualenv of the sub-project it runs in (`backend/.venv`), else the project's.
pub fn python_in(project: &Path, cwd: &Path) -> String {
    crate::python_env::selected_interpreter(project)
        .or_else(|| (cwd != project && cwd.starts_with(project)).then(|| crate::python_env::project_venv(cwd)).flatten())
        .or_else(|| crate::python_env::project_venv(project))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "python3".into())
}

/// The package manager for a configuration that runs in `cwd`: from the sub-project's lockfile
/// when it has one, else the project's.
pub(crate) fn package_manager_in(project: &Path, cwd: &Path) -> &'static str {
    let locks = ["bun.lockb", "bun.lock", "pnpm-lock.yaml", "yarn.lock", "package-lock.json"];
    if cwd != project && locks.iter().any(|l| cwd.join(l).is_file()) {
        package_manager(cwd)
    } else {
        package_manager(project)
    }
}

/// The JavaScript package manager from the lockfile.
pub(crate) fn package_manager(root: &Path) -> &'static str {
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
        out.extend(crate::python_project::framework_configs(root, &py));
    }
    out
}

/// The configuration for "build the project" when it is not a Cargo project.
pub fn build_config(root: &Path) -> Option<RunConfig> {
    derive_polyglot(root).into_iter().find(|c| c.command.as_ref().is_some_and(|cmd| cmd.build))
}

/// A Python test file by pytest's default discovery (`test_*.py`, `*_test.py`).
fn is_python_test_file(name: &str) -> bool {
    name.starts_with("test_") || name.ends_with("_test.py")
}

/// A JavaScript/TypeScript test file by jest/vitest conventions (`*.test.ts`, `*.spec.js`, files
/// under `__tests__`).
pub fn is_js_test_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    name.contains(".test.") || name.contains(".spec.") || path.components().any(|c| c.as_os_str() == "__tests__")
}

/// The test name of a jest/vitest call on a line: `test("adds", …)`, `it('works', …)`,
/// `describe(`…`)`, `test.only(…)`, `it.skip(…)`. `None` for other lines.
pub fn js_test_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = ["test", "it", "describe"].iter().find_map(|k| t.strip_prefix(k))?;
    let rest = match rest.strip_prefix('.') {
        Some(m) => {
            let modifier: String = m.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            if !matches!(modifier.as_str(), "only" | "skip" | "concurrent" | "todo" | "fails") {
                return None;
            }
            m.get(modifier.len()..)?
        }
        None => rest,
    };
    let rest = rest.trim_start().strip_prefix('(')?.trim_start();
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\'' | '`'))?;
    let body = rest.get(quote.len_utf8()..)?;
    let end = body.find(quote)?;
    body.get(..end).map(str::to_owned)
}

/// The `test "name" {` (or bare `test {`) block opening a Zig line.
fn is_zig_test(line: &str) -> bool {
    line.trim_start().strip_prefix("test").is_some_and(|r| r.trim_start().starts_with(['"', '{']) || r.starts_with(' '))
}

/// Lines with a gutter ▶ run icon in a non-Rust file (Rust's come from its outline in the UI):
/// C/C++ `main`, Python `if __name__ == "__main__":` and pytest test functions and classes, Zig
/// `pub fn main` and `test` blocks, jest/vitest `test`/`it`/`describe` calls in test files.
pub fn gutter_run_lines(path: &Path, text: &str) -> Vec<usize> {
    if let Some(lines) = crate::docker::run::gutter_run_lines(path, text) {
        return lines;
    }
    let language = Language::from_path(path);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut out = Vec::new();
    match language {
        Language::C | Language::Cpp => {
            for (i, l) in text.lines().enumerate() {
                // A definition at file level: `int main(…) {`, `int main(void)` with the brace below.
                let t = l.trim_end();
                let before = t.split("main(").next().unwrap_or("");
                if !l.starts_with([' ', '\t'])
                    && t.contains("main(")
                    && !t.ends_with(';')
                    && before
                        .split_whitespace()
                        .last()
                        .is_some_and(|ty| matches!(ty, "int" | "void" | "auto" | "signed" | "unsigned"))
                    && !before.contains(['=', '"', '/'])
                {
                    out.push(i);
                }
            }
        }
        Language::Python => {
            let tests = is_python_test_file(&name);
            for (i, l) in text.lines().enumerate() {
                let t = l.trim_start();
                let main = t.starts_with("if __name__") && t.contains("__main__") && l == t;
                let def = t.strip_prefix("async ").unwrap_or(t);
                let test = tests
                    && (def.strip_prefix("def ").is_some_and(|r| r.starts_with("test"))
                        || (l == t && t.strip_prefix("class ").is_some_and(|r| r.starts_with("Test"))));
                if main || test {
                    out.push(i);
                }
            }
        }
        Language::Zig => {
            for (i, l) in text.lines().enumerate() {
                let t = l.trim_start();
                if t.starts_with("pub fn main(") || (l == t && is_zig_test(l)) {
                    out.push(i);
                }
            }
        }
        Language::JavaScript | Language::TypeScript if is_js_test_file(path) => {
            for (i, l) in text.lines().enumerate() {
                if js_test_name(l).is_some() {
                    out.push(i);
                }
            }
        }
        // HTTP Client: one ▶ per request.
        Language::Http => out = codecraft_http::parse::request_lines(text),
        _ => {}
    }
    out
}

/// The JavaScript test runner a project uses (from `package.json` dependencies): vitest, jest,
/// else Node's built-in `node --test`.
fn js_test_command(root: &Path, file: &str, test: Option<&str>) -> (String, ProcessCommand) {
    let pkg = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    let pkg: Value = serde_json::from_str(&pkg).unwrap_or(Value::Null);
    let has = |dep: &str| {
        ["dependencies", "devDependencies"].iter().any(|k| pkg.get(k).and_then(|d| d.get(dep)).is_some())
    };
    let runner = if has("vitest") {
        "vitest"
    } else if has("jest") {
        "jest"
    } else {
        "node"
    };
    let label = test.unwrap_or(file).to_owned();
    if runner == "node" {
        let mut args = vec!["--test".to_owned()];
        if let Some(t) = test {
            args.push(format!("--test-name-pattern={t}"));
        }
        args.push(file.to_owned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        return (format!("node --test {label}"), ProcessCommand::new("node", &args, root));
    }
    let (exec, prefix): (&str, &[&str]) = match package_manager(root) {
        "pnpm" => ("pnpm", &["exec"]),
        "yarn" => ("yarn", &[]),
        "bun" => ("bunx", &[]),
        _ => ("npx", &[]),
    };
    let mut args: Vec<String> = prefix.iter().map(|a| (*a).to_owned()).collect();
    args.push(runner.to_owned());
    if runner == "vitest" {
        args.push("run".into());
    }
    args.push(file.to_owned());
    if let Some(t) = test {
        args.push("-t".into());
        args.push(t.to_owned());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    (format!("{runner} {label}"), ProcessCommand::new(exec, &args, root))
}

/// What to run for a line of a non-Rust file (the Rust case is in `run_cmds.rs`).
pub fn context_config(root: &Path, path: &Path, text: &str, line: usize) -> Option<RunConfig> {
    if let Some(config) = crate::docker::run::context_config(root, path, text, line) {
        return Some(config);
    }
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
        Language::JavaScript | Language::TypeScript if is_js_test_file(path) => {
            // The jest/vitest call at or above the caret line, else the whole file.
            let test = text.lines().take(line.saturating_add(1)).filter_map(js_test_name).last();
            let (label, cmd) = js_test_command(root, &file, test.as_deref());
            Some(RunConfig::process(label, cmd))
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

    #[test]
    fn gutter_icons_per_language() {
        let lines = |p: &str, t: &str| gutter_run_lines(Path::new(p), t);
        let c = "#include <stdio.h>\nint helper(void);\nint main(int argc, char **argv) {\n    return 0;\n}\n";
        assert_eq!(lines("/p/main.c", c), vec![2]);
        assert_eq!(lines("/p/main.cpp", "auto x = main();\nint main()\n{\n}\n"), vec![1]);
        assert!(lines("/p/lib.c", "int main(void);\nstatic int f(void) { return main(); }\n").is_empty());
        let py = "def helper():\n    pass\n\nif __name__ == \"__main__\":\n    helper()\n";
        assert_eq!(lines("/p/app.py", py), vec![3]);
        let pyt = "import pytest\n\ndef test_a():\n    pass\n\nasync def test_b():\n    pass\n\nclass TestX:\n    def test_c(self):\n        pass\n";
        assert_eq!(lines("/p/tests/test_x.py", pyt), vec![2, 5, 8, 9]);
        // Test functions only count in test files.
        assert!(lines("/p/app2.py", "def test_a():\n    pass\n").is_empty());
        let zig = "const std = @import(\"std\");\npub fn main() !void {\n}\ntest \"adds\" {\n}\ntest {\n}\nconst testing = 1;\n";
        assert_eq!(lines("/p/main.zig", zig), vec![1, 3, 5]);
        let ts = "import { test, it, describe } from 'vitest';\ndescribe('math', () => {\n  it(\"adds\", () => {});\n  test.only(`subs`, () => {});\n  item('x');\n});\n";
        assert_eq!(lines("/p/src/math.test.ts", ts), vec![1, 2, 3]);
        assert!(lines("/p/src/math.ts", ts).is_empty());
        assert_eq!(lines("/p/__tests__/a.js", "test('a', () => {})\n"), vec![0]);
        assert!(lines("/p/README.md", "int main() {}\n").is_empty());
        assert_eq!(js_test_name("  it.skip('a b', fn)"), Some("a b".into()));
        assert_eq!(js_test_name("  it.foo('a')"), None);
        assert_eq!(js_test_name("test(name)"), None);
        assert_eq!(js_test_name("it('unterminated"), None);
        assert_eq!(js_test_name("it('é')"), Some("é".into()));
    }

    #[test]
    fn js_test_context_runs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("src/math.test.ts");
        let ts = "describe('math', () => {\n  it('adds', () => {\n    expect(1).toBe(1);\n  });\n});\n";
        // No test framework in package.json: Node's test runner.
        let c = context_config(root, &file, ts, 2).unwrap();
        assert!(c.command_line().contains("node --test --test-name-pattern=adds"), "{}", c.command_line());
        std::fs::write(root.join("package.json"), r#"{"devDependencies":{"vitest":"^2"}}"#).unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        let c = context_config(root, &file, ts, 2).unwrap();
        assert!(c.command_line().contains("pnpm exec vitest run"), "{}", c.command_line());
        assert!(c.command_line().contains("-t adds"), "{}", c.command_line());
        let c = context_config(root, &file, ts, 0).unwrap();
        assert!(c.command_line().ends_with("-t math"), "{}", c.command_line());
        std::fs::write(root.join("package.json"), r#"{"devDependencies":{"jest":"^29"}}"#).unwrap();
        std::fs::remove_file(root.join("pnpm-lock.yaml")).unwrap();
        let c = context_config(root, &file, ts, 2).unwrap();
        assert!(c.command_line().starts_with("npx jest"), "{}", c.command_line());
        // A plain (non-test) TypeScript file still runs with node.
        let c = context_config(root, &root.join("src/main.ts"), "console.log(1)", 0).unwrap();
        assert!(c.command_line().contains("--experimental-strip-types"));
    }
}
