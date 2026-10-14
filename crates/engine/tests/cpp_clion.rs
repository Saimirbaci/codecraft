//! Track CL (CLion features) through engine commands, with fake tools written as shell scripts:
//! C++ test runs (GoogleTest, Catch2, doctest, Boost.Test) in the test runner, the CMake tool
//! window's reload / targets / cache / CTest, profiles and presets driving run configurations,
//! Valgrind Memcheck, sanitizer reports, Conan installs. No real cmake, valgrind or test binary
//! runs; a sanitizer build with the system compiler runs only when `cc` supports ASan.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::testing::{Framework, TestStatus};
use codecraft_workspace::{RunStatus, RunTabId};

const WAIT: Duration = Duration::from_secs(300);

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            let console: Vec<String> = s.ws.runs.iter().flat_map(|r| r.lines.iter().map(|l| l.text.clone())).collect();
            let jobs: Vec<String> = s.cpp.jobs.list.iter().flat_map(|j| j.lines.clone()).collect();
            panic!("timed out waiting for {what}; console:\n{}\njobs:\n{}", console.join("\n"), jobs.join("\n"));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

fn open_project(root: &Path, home: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s.execute("file.open", &json!({ "path": root })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    s
}

fn tab_of(v: &Value) -> RunTabId {
    RunTabId(v["tab"].as_u64().unwrap())
}

fn wait_test_run(s: &mut Session, tab: RunTabId) {
    pump_until(s, "the test run", |s| s.testing.run(tab).is_some_and(|r| r.finished) && !s.testing.busy());
}

fn wait_tab(s: &mut Session, tab: RunTabId) {
    pump_until(s, "the run", |s| s.ws.run(tab).is_some_and(|t| t.status != RunStatus::Running));
}

fn wait_job(s: &mut Session, id: u64) {
    pump_until(s, "the job", |s| s.cpp.jobs.get(id).is_some_and(|j| !j.is_running()));
}

const GTEST_SRC: &str = "#include <gtest/gtest.h>\n\nint add(int a, int b) { return a + b; }\n\nTEST(MathTest, Adds) {\n  EXPECT_EQ(add(1, 2), 3);\n}\n\n\
TEST(MathTest, Fails) {\n  int x = 0;\n  (void)x;\n  EXPECT_EQ(add(1, 2), 4);\n}\n\nTEST(MathTest, DISABLED_Slow) {}\n\nclass ParamTest : public ::testing::TestWithParam<int> {};\n\n\
TEST_P(ParamTest, Even) {\n  EXPECT_EQ(GetParam() % 2, 0);\n}\n\n\n\nTEST(SkipTest, Later) { GTEST_SKIP() << \"not ready\"; }\n";

/// A fake GoogleTest binary: the console and JSON fixtures with paths into `root`.
fn fake_gtest(dir: &Path, root: &Path, log: &Path) -> PathBuf {
    let fix = |text: &str| text.replace("/proj", &root.display().to_string());
    let console = write(dir, "fixtures/gtest_console.txt", &fix(include_str!("../src/testing/fixtures/cpp/gtest_console.txt")));
    let report = write(dir, "fixtures/gtest.json", &fix(include_str!("../src/testing/fixtures/cpp/gtest.json")));
    let list = write(dir, "fixtures/gtest_list.txt", include_str!("../src/testing/fixtures/cpp/gtest_list.txt"));
    script(
        &dir.join("bin/math_tests"),
        &format!(
            "echo \"$@\" >> '{log}'\nout=''\nfor a in \"$@\"; do\n  case \"$a\" in\n    --gtest_list_tests) cat '{list}'; exit 0;;\n    \
             --gtest_output=json:*) out=\"${{a#--gtest_output=json:}}\";;\n  esac\ndone\ncat '{console}'\n\
             if [ -n \"$out\" ]; then cp '{report}' \"$out\"; fi\nexit 1\n",
            log = log.display(),
            list = list.display(),
            console = console.display(),
            report = report.display()
        ),
    )
}

#[test]
fn googletest_run_tree_rerun_and_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap().join("proj");
    let src = write(root, "tests/math_test.cpp", GTEST_SRC);
    let log = tmp.path().join("args.log");
    let exe = fake_gtest(&tmp.path().join("tools"), root, &log);
    let mut s = open_project(root, &tmp.path().join("home"));

    // Run everything: the console builds the tree live, the JSON report replaces it at the end.
    let r = s.execute("cpp.tests.run", &json!({ "executable": exe, "framework": "gtest" })).unwrap();
    let tab = tab_of(&r);
    wait_test_run(&mut s, tab);
    let run = s.testing.run(tab).unwrap();
    assert_eq!(run.framework, Framework::GoogleTest);
    let c = run.counts();
    assert_eq!((c.passed, c.failed, c.ignored), (2, 2, 2), "{:#?}", run.suites);
    let fails = run.suites[0].tests.iter().find(|t| t.name == "Fails").unwrap();
    assert_eq!(fails.status, TestStatus::Failed);
    assert_eq!(fails.location.as_ref().map(|l| (l.path.clone(), l.line)), Some((src.clone(), 8)));
    assert_eq!(fails.failure.as_ref().map(|l| l.line), Some(11));
    // Console output captured live stays with the test.
    assert!(fails.output.contains("Expected equality"), "{:?}", fails.output);
    // The gutter shows the last status at the test.
    let gutter = s.testing.gutter_tests(&src).cloned().unwrap_or_default();
    assert_eq!(gutter.get(&8), Some(&TestStatus::Failed));
    assert_eq!(gutter.get(&4), Some(&TestStatus::Passed));
    // Navigate to the failed test.
    s.execute("test.nextFailed", &json!({ "tab": tab.0 })).unwrap();
    s.execute("test.navigate", &json!({ "tab": tab.0 })).unwrap();
    assert_eq!(s.ws.active().unwrap().1.path.as_deref(), Some(src.as_path()));

    // Rerun Failed Tests: exactly the failed tests through --gtest_filter.
    let rerun = tab_of(&s.execute("test.rerunFailed", &json!({ "tab": tab.0 })).unwrap());
    wait_test_run(&mut s, rerun);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.lines().last().unwrap().contains("--gtest_filter=MathTest.Fails:Inst/ParamTest.Even/1"), "{args}");

    // One test from the source (the gutter ▶): the CMake project is not configured, so CTest by name.
    assert!(codecraft_engine::run_configs::gutter_run_lines(&src, GTEST_SRC).contains(&8));
    write(root, "CMakeLists.txt", "cmake_minimum_required(VERSION 3.20)\nproject(p CXX)\nenable_testing()\n");
    let cfg = codecraft_engine::run_configs::context_config(root, &src, GTEST_SRC, 10).unwrap();
    assert_eq!(cfg.name, "MathTest.Fails");
    assert!(cfg.command_line().contains("ctest --test-dir cmake-build-debug --output-on-failure -R"), "{}", cfg.command_line());
    assert!(cfg.command_line().contains("MathTest\\.Fails"), "{}", cfg.command_line());

    // List the executable's tests in the background.
    let r = s.execute("cpp.tests.list", &json!({ "executable": exe, "framework": "gtest" })).unwrap();
    assert_eq!(r["pending"], true);
    wait_job(&mut s, r["job"].as_u64().unwrap());
    let l = s.execute("cpp.tests.list", &json!({ "executable": exe })).unwrap();
    assert_eq!(l["pending"], false);
    assert_eq!(l["tests"].as_array().unwrap().len(), 5);
    assert!(s.execute("cpp.tests.run", &json!({ "executable": exe })).is_err(), "the framework is unknown without CMake");
    s.shutdown();
}

#[test]
fn catch2_doctest_and_boost_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap().join("proj");
    write(root, "README.md", "x");
    let tools = tmp.path().join("tools");
    let catch = script(
        &tools.join("catch_tests"),
        &format!(
            "out=''\nprev=''\nfor a in \"$@\"; do\n  if [ \"$prev\" = --out ]; then out=\"$a\"; fi\n  prev=\"$a\"\ndone\n\
             echo 'test cases: 5 | 1 passed | 3 failed | 1 skipped'\ncp '{}' \"$out\"\nexit 1\n",
            write(&tools, "catch2.xml", include_str!("../src/testing/fixtures/cpp/catch2.xml")).display()
        ),
    );
    let doctest = script(
        &tools.join("doc_tests"),
        &format!(
            "for a in \"$@\"; do case \"$a\" in --out=*) out=\"${{a#--out=}}\";; esac; done\ncp '{}' \"$out\"\nexit 1\n",
            write(&tools, "doctest.xml", include_str!("../src/testing/fixtures/cpp/doctest.xml")).display()
        ),
    );
    let boost = script(
        &tools.join("boost_tests"),
        &format!(
            "for a in \"$@\"; do case \"$a\" in --log_sink=*) out=\"${{a#--log_sink=}}\";; esac; done\n\
             cp '{}' \"$out\"\necho '*** 2 failures are detected in the test module \"boost_tests\"'\nexit 201\n",
            write(&tools, "boost.xml", include_str!("../src/testing/fixtures/cpp/boost.xml")).display()
        ),
    );
    let mut s = open_project(root, &tmp.path().join("home"));
    for (exe, fw, expect) in [(catch, "catch2", (1, 3, 1)), (doctest, "doctest", (1, 2, 1)), (boost, "boost", (2, 2, 1))] {
        let tab = tab_of(&s.execute("cpp.tests.run", &json!({ "executable": exe, "framework": fw })).unwrap());
        wait_test_run(&mut s, tab);
        let run = s.testing.run(tab).unwrap();
        let c = run.counts();
        assert_eq!((c.passed, c.failed, c.ignored), expect, "{fw}: {:#?}", run.suites);
        assert!(run.suites.iter().flat_map(|s| &s.tests).filter(|t| t.status == TestStatus::Failed).any(|t| t.message.is_some()));
        // Rerun Failed Tests builds the framework's filter.
        let rerun = s.testing.run(tab).map(codecraft_engine::testing::rerun_failed_origin).unwrap().unwrap();
        let codecraft_engine::testing::TestOrigin::Process(cmd) = rerun else { panic!() };
        let last = cmd.args.last().unwrap().clone();
        match fw {
            "catch2" => assert_eq!(last, "Fails,Sections,Throws"),
            "doctest" => assert_eq!(last, "--test-case=fails,in suite"),
            _ => assert_eq!(last, "--run_test=math/fails:math/deep/nested"),
        }
    }
    s.shutdown();
}

/// A fake `cmake`: configure copies a staged File API reply and cache into the build directory
/// (after checking our query file is there); `--build` succeeds.
fn fake_cmake(tools: &Path, staged: &Path, log: &Path) -> PathBuf {
    script(
        &tools.join("cmake"),
        &format!(
            "echo \"$@\" >> '{log}'\nif [ \"$1\" = --build ]; then echo \"[100%] Built target $4\"; exit 0; fi\n\
             build=''\nprev=''\nfor a in \"$@\"; do\n  if [ \"$prev\" = -B ]; then build=\"$a\"; fi\n  prev=\"$a\"\ndone\n\
             if [ ! -f \"$build/.cmake/api/v1/query/client-codecraft/query.json\" ]; then echo 'CMake Error: no query' >&2; exit 1; fi\n\
             echo '-- The CXX compiler identification is Fake 1.0'\ncp -R '{staged}/.' \"$build/\"\n\
             cp '{tests}' \"$build/tests/math_tests\"\nchmod +x \"$build/tests/math_tests\"\necho '-- Configuring done'\necho '-- Generating done'\n",
            log = log.display(),
            staged = staged.display(),
            tests = tools.join("bin/math_tests").display()
        ),
    )
}

fn stage_reply(staged: &Path, root: &Path, build: &Path) {
    let reply = staged.join(".cmake/api/v1/reply");
    write(&reply, "index-2026-10-14T10-00-00-0000.json", r#"{"reply": {"client-codecraft": {"query.json": {"responses": [
        {"kind": "codemodel", "version": {"major": 2, "minor": 7}, "jsonFile": "codemodel-v2-1.json"}]}}}}"#);
    write(
        &reply,
        "codemodel-v2-1.json",
        &format!(
            r#"{{"paths": {{"source": "{}", "build": "{}"}}, "configurations": [{{"name": "Debug",
              "directories": [{{"source": "."}}],
              "targets": [{{"name": "app", "jsonFile": "target-app.json"}}, {{"name": "math_tests", "jsonFile": "target-tests.json"}}]}}]}}"#,
            root.display(),
            build.display()
        ),
    );
    write(&reply, "target-app.json", r#"{"name": "app", "type": "EXECUTABLE", "artifacts": [{"path": "app"}], "sources": [{"path": "main.cpp"}]}"#);
    write(
        &reply,
        "target-tests.json",
        r#"{"name": "math_tests", "type": "EXECUTABLE", "artifacts": [{"path": "tests/math_tests"}], "sources": [{"path": "tests/math_test.cpp"}]}"#,
    );
    write(
        staged,
        "CMakeCache.txt",
        "# This is the CMakeCache file.\n//Choose the type of build\nCMAKE_BUILD_TYPE:STRING=Debug\n//Flags\nCMAKE_CXX_FLAGS:STRING=\n\
         CMAKE_CXX_FLAGS-ADVANCED:INTERNAL=1\nBUILD_TESTING:BOOL=ON\nCMAKE_HOME_DIRECTORY:INTERNAL=/x\n",
    );
    write(staged, "tests/.keep", "");
}

#[test]
fn cmake_tool_window_profiles_targets_cache_ctest_and_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap().join("proj");
    write(root, "CMakeLists.txt", "cmake_minimum_required(VERSION 3.20)\nproject(p CXX)\nadd_executable(app main.cpp)\nenable_testing()\n");
    write(root, "main.cpp", "int main() { return 0; }\n");
    let src = write(root, "tests/math_test.cpp", GTEST_SRC);
    write(
        root,
        "CMakePresets.json",
        r#"{"version": 6, "configurePresets": [
            {"name": "base", "hidden": true, "binaryDir": "${sourceDir}/out/${presetName}"},
            {"name": "ninja-release", "inherits": "base", "generator": "Ninja", "cacheVariables": {"CMAKE_BUILD_TYPE": "Release"}}]}"#,
    );
    let tools = tmp.path().join("tools");
    let log = tmp.path().join("cmake.log");
    let args_log = tmp.path().join("tests.log");
    fake_gtest(&tools, root, &args_log);
    let build = root.join("cmake-build-debug");
    let staged = tmp.path().join("staged");
    stage_reply(&staged, root, &build);
    let cmake = fake_cmake(&tools, &staged, &log);
    let ctest = script(
        &tools.join("ctest"),
        &format!(
            "echo '{{\"kind\": \"ctestInfo\", \"tests\": [{{\"name\": \"MathTest.Adds\", \"command\": [\"{exe}\", \"--gtest_filter=MathTest.Adds\"]}}, \
             {{\"name\": \"MathTest.Fails\", \"command\": [\"{exe}\", \"--gtest_filter=MathTest.Fails\"]}}]}}'\n",
            exe = build.join("tests/math_tests").display()
        ),
    );
    let mut s = open_project(root, &tmp.path().join("home"));
    s.cpp.tools.insert("cmake".into(), cmake.clone());
    s.cpp.tools.insert("ctest".into(), ctest);
    pump_until(&mut s, "the CMake state", |s| s.cpp.cmake.is_cmake && !s.cpp.cmake.analyzing());

    // Profiles: Debug, Release, then the visible preset.
    let p = s.execute("cmake.profiles", &Value::Null).unwrap();
    let names: Vec<&str> = p["profiles"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Debug", "Release", "ninja-release"]);
    assert_eq!(p["selected"], "Debug");

    // Reload: our File API query is written, the reply read into targets and cache.
    let r = s.execute("cmake.reload", &Value::Null).unwrap();
    wait_job(&mut s, r["job"].as_u64().unwrap());
    pump_until(&mut s, "the codemodel", |s| s.cpp.cmake.codemodel.is_some() && !s.cpp.cmake.analyzing());
    let log_text = s.execute("cmake.log", &Value::Null).unwrap();
    assert!(log_text["lines"].as_array().unwrap().iter().any(|l| l == "-- Configuring done"), "{log_text}");
    let targets = s.execute("cmake.targets", &Value::Null).unwrap();
    let t = targets["targets"].as_array().unwrap();
    assert_eq!(t.len(), 2);
    assert_eq!(t[1]["name"], "math_tests");
    assert_eq!(t[1]["testFramework"], "GoogleTest");
    // The test executable became a run configuration, and the gutter ▶ runs its target.
    assert!(s.run_configs.iter().any(|c| c.name == "All in math_tests"), "{:?}", s.run_configs.iter().map(|c| &c.name).collect::<Vec<_>>());
    let cfg = codecraft_engine::run_configs::context_config(root, &src, GTEST_SRC, 5).unwrap();
    assert_eq!(cfg.name, "MathTest.Adds");
    let cmd = cfg.command.unwrap();
    assert!(cmd.args[1].contains("--target math_tests"), "{:?}", cmd.args);
    assert_eq!(cmd.args[3], build.join("tests/math_tests").display().to_string());
    assert_eq!(cmd.args.last().unwrap(), "--gtest_filter=MathTest.Adds");

    // Run the test target from the tool window: build, then the test runner.
    let r = s.execute("cmake.runTarget", &json!({ "target": "math_tests" })).unwrap();
    assert_eq!(r["testFramework"], "GoogleTest");
    let tab = tab_of(&r);
    wait_test_run(&mut s, tab);
    assert_eq!(s.testing.run(tab).unwrap().counts().failed, 2);
    assert!(std::fs::read_to_string(&log).unwrap().contains("--build"));

    // Build a target in the Build tool window.
    let tab = tab_of(&s.execute("cmake.buildTarget", &json!({ "target": "app" })).unwrap());
    wait_tab(&mut s, tab);
    assert_eq!(s.ws.run(tab).unwrap().status, RunStatus::Succeeded);
    assert!(s.execute("cmake.buildTarget", &json!({ "target": "nope" })).is_err());

    // Cache variables: filter, advanced, edit.
    let c = s.execute("cmake.cache", &json!({ "filter": "cmake_" })).unwrap();
    let names: Vec<&str> = c["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["CMAKE_BUILD_TYPE"]);
    let c = s.execute("cmake.cache", &json!({ "filter": "", "advanced": true })).unwrap();
    assert_eq!(c["entries"].as_array().unwrap().len(), 3);
    s.execute("cmake.cache.set", &json!({ "name": "CMAKE_BUILD_TYPE", "value": "RelWithDebInfo" })).unwrap();
    assert!(std::fs::read_to_string(build.join("CMakeCache.txt")).unwrap().contains("CMAKE_BUILD_TYPE:STRING=RelWithDebInfo"));
    assert!(s.cpp.cmake.needs_reload);

    // CTest's tests.
    let r = s.execute("cmake.ctest.refresh", &Value::Null).unwrap();
    wait_job(&mut s, r["job"].as_u64().unwrap());
    assert_eq!(s.cpp.cmake.ctest.len(), 2);
    let exes = s.execute("cpp.tests.executables", &Value::Null).unwrap();
    assert_eq!(exes[0]["ctestTests"], 2);
    // The framework of a known executable is detected.
    let tab = tab_of(&s.execute("cpp.tests.run", &json!({ "executable": build.join("tests/math_tests"), "suite": "MathTest", "test": "Adds" })).unwrap());
    wait_test_run(&mut s, tab);
    assert!(std::fs::read_to_string(&args_log).unwrap().lines().last().unwrap().contains("--gtest_filter=MathTest.Adds"));

    // Select the preset profile: the build directory follows (run configurations too).
    let r = s.execute("cmake.selectProfile", &json!({ "profile": "ninja-release" })).unwrap();
    assert_eq!(r["buildDir"], root.join("out/ninja-release").display().to_string());
    assert!(std::fs::read_to_string(root.join(".codecraft/cmake.json")).unwrap().contains("ninja-release"));
    let build_cfg = s.run_configs.iter().find(|c| c.name == "CMake: Build").unwrap();
    assert!(build_cfg.command_line().contains("cmake --preset=ninja-release"), "{}", build_cfg.command_line());
    assert!(s.execute("cmake.selectProfile", &json!({ "profile": "nope" })).is_err());
    s.shutdown();
}

#[test]
fn valgrind_memcheck_and_import() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap().join("proj");
    write(root, "main.c", "int main(void) { return 0; }\n");
    let fixture = write(
        tmp.path(),
        "memcheck.xml",
        &include_str!("../src/cpp/fixtures/memcheck.xml").replace("/proj", &root.display().to_string()),
    );
    let mut s = open_project(root, &tmp.path().join("home"));
    // Without Valgrind (and always on macOS) the command explains.
    s.execute("runConfig.create", &json!({ "name": "hello", "kind": "command", "command": "echo hello" })).unwrap();
    pump_until(&mut s, "the configuration", |s| s.run_configs.iter().any(|c| c.name == "hello"));
    let err = s.execute("run.runWithValgrindMemcheck", &Value::Null).unwrap_err().to_string();
    assert!(err.contains("not available") || err.contains("Valgrind"), "{err}");

    let valgrind = script(
        &tmp.path().join("tools/valgrind"),
        &format!(
            "f=''\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --xml-file=*) f=\"${{1#--xml-file=}}\"; shift;;\n    --*) shift;;\n    *) break;;\n  esac\ndone\n\
             f=$(echo \"$f\" | sed \"s/%p/$$/\")\ncp '{}' \"$f\"\nexec \"$@\"\n",
            fixture.display()
        ),
    );
    s.cpp.tools.insert("valgrind".into(), valgrind);
    let i = s.run_configs.iter().position(|c| c.name == "hello").unwrap();
    let r = s.execute("run.runWithValgrindMemcheck", &json!({ "config": i })).unwrap();
    let tab = tab_of(&r);
    assert!(r["commandLine"].as_str().unwrap().contains("--xml-file="));
    pump_until(&mut s, "the Memcheck results", |s| s.cpp.valgrind.result(tab).is_some());
    assert!(s.ws.run(tab).unwrap().lines.iter().any(|l| l.text == "hello"));
    let v = s.execute("valgrind.results", &json!({ "tab": tab.0 })).unwrap();
    let errors = v["result"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0]["kind"], "InvalidWrite");
    assert_eq!(v["groups"][0][0], "Invalid write");
    // The first error's project frame opens main.c.
    s.execute("valgrind.navigate", &json!({ "tab": tab.0, "error": 0 })).unwrap();
    assert_eq!(s.ws.active().unwrap().1.path.as_deref(), Some(root.join("main.c").as_path()));

    // Import a saved report.
    let r = s.execute("run.importValgrindXmlResults", &json!({ "path": fixture })).unwrap();
    assert_eq!(r["errors"], 3);
    assert!(s.cpp.valgrind.result(tab_of(&r)).is_some());
    assert!(s.execute("run.importValgrindXmlResults", &json!({ "path": root.join("main.c") })).is_err());
    s.execute("run.importValgrindXmlResults", &Value::Null).unwrap();
    assert!(s.cpp.valgrind.import_requested);
    s.shutdown();
}

#[test]
fn sanitizer_reports_from_run_output() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap().join("proj");
    write(root, "main.cpp", "int main() {\n  int* a = new int[1];\n  a[1] = 0;\n  delete[] a;\n  return 0;\n}\n");
    let out = write(
        tmp.path(),
        "asan.txt",
        &include_str!("../src/cpp/fixtures/asan.txt").replace("main.cpp:", &format!("{}/main.cpp:", root.display())),
    );
    let mut s = open_project(root, &tmp.path().join("home"));
    s.execute("runConfig.create", &json!({ "name": "asan", "kind": "command", "command": format!("cat {}", out.display()) }))
        .unwrap();
    let i = s.run_configs.iter().position(|c| c.name == "asan").unwrap();
    let tab = tab_of(&s.execute("run.run", &json!({ "config": i })).unwrap());
    pump_until(&mut s, "the sanitizer report", |s| !s.cpp.sanitizers.reports(tab).is_empty());
    let r = s.execute("sanitizers.reports", &json!({ "tab": tab.0 })).unwrap();
    let rep = &r["reports"][0];
    assert_eq!(rep["kind"], "heap-buffer-overflow");
    assert_eq!(rep["sanitizer"], "Address");
    assert_eq!(rep["sections"][0]["frames"][0]["project"], true);
    s.execute("sanitizers.navigate", &json!({ "tab": tab.0, "report": 0, "section": 0, "frame": 0 })).unwrap();
    let (e, d) = s.ws.active().unwrap();
    assert_eq!(d.path.as_deref(), Some(root.join("main.cpp").as_path()));
    assert_eq!(d.doc.char_to_line(e.selections.primary().head), 4);

    // A C/C++ configuration compiled with AddressSanitizer (when the compiler supports it).
    let cc_ok = std::process::Command::new("c++").arg("--version").output().is_ok_and(|o| o.status.success());
    s.execute("runConfig.create", &json!({ "name": "native", "kind": "cpp", "command": "main.cpp" })).unwrap();
    let ci = s.run_store.user.iter().position(|c| c.name == "native").unwrap();
    s.execute("cpp.runConfig.setSanitizers", &json!({ "index": ci, "sanitizers": "address" })).unwrap();
    assert!(s.execute("cpp.runConfig.setSanitizers", &json!({ "index": ci, "sanitizers": "bogus" })).is_err());
    let i = s.run_configs.iter().position(|c| c.name == "native").unwrap();
    assert!(s.run_configs[i].command_line().contains("-fsanitize=address"), "{}", s.run_configs[i].command_line());
    if !cc_ok {
        eprintln!("skipping the native ASan run: no c++ compiler");
        s.shutdown();
        return;
    }
    let tab = tab_of(&s.execute("run.run", &json!({ "config": i })).unwrap());
    wait_tab(&mut s, tab);
    pump_until(&mut s, "the sanitizer scan", |s| s.ws.run(tab).is_some());
    s.pump();
    let reports = s.cpp.sanitizers.reports(tab).to_vec();
    if reports.is_empty() {
        let console: Vec<String> = s.ws.run(tab).unwrap().lines.iter().map(|l| l.text.clone()).collect();
        eprintln!("skipping: this compiler did not produce an ASan report:\n{}", console.join("\n"));
    } else {
        assert_eq!(reports[0].kind, "heap-buffer-overflow");
        assert_eq!(reports[0].location.as_ref().map(|l| l.line), Some(2), "{:#?}", reports[0]);
    }
    s.shutdown();
}

#[test]
fn conan_install_sets_the_cmake_toolchain() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap().join("proj");
    write(root, "CMakeLists.txt", "project(p)\n");
    write(root, "conanfile.txt", "[requires]\nzlib/1.3.1\nfmt/10.2.1\n[generators]\nCMakeToolchain\n");
    let conan = script(
        &tmp.path().join("tools/conan"),
        "of=''\nprev=''\nfor a in \"$@\"; do\n  if [ \"$prev\" = -of ]; then of=\"$a\"; fi\n  prev=\"$a\"\ndone\n\
         echo \"conan $*\"\nmkdir -p \"$of/build/Debug/generators\"\ntouch \"$of/build/Debug/generators/conan_toolchain.cmake\"\n",
    );
    let mut s = open_project(root, &tmp.path().join("home"));
    // Isolated sessions never run a real conan.
    assert!(s.execute("cpp.dependencies.install", &Value::Null).is_err());
    s.cpp.tools.insert("conan".into(), conan);
    let d = s.execute("cpp.dependencies", &Value::Null).unwrap();
    assert_eq!(d["manifests"]["packages"].as_array().unwrap().len(), 2);
    let r = s.execute("cpp.dependencies.install", &Value::Null).unwrap();
    let id = r["job"].as_u64().unwrap();
    wait_job(&mut s, id);
    let lines = s.cpp.jobs.get(id).unwrap().lines.join("\n");
    assert!(lines.contains("install . --build=missing -of"), "{lines}");
    assert!(lines.contains("build_type=Debug"), "{lines}");
    pump_until(&mut s, "the profiles and run configurations", |s| {
        s.cpp.cmake.active().is_some_and(|p| p.toolchain_file.is_some())
            && !s.cpp.cmake.analyzing()
            && s.run_configs.iter().any(|c| c.name == "CMake: Build" && c.command_line().contains("-DCMAKE_TOOLCHAIN_FILE="))
    });
    let tc = root.join("cmake-build-debug/build/Debug/generators/conan_toolchain.cmake");
    assert_eq!(s.cpp.cmake.active().unwrap().toolchain_file.as_deref(), Some(tc.display().to_string().as_str()));
    let script_line = s.run_configs.iter().find(|c| c.name == "CMake: Build").unwrap().command_line();
    assert!(script_line.contains("-DCMAKE_TOOLCHAIN_FILE="), "{script_line}");
    s.execute("cpp.dependencies.clearToolchain", &Value::Null).unwrap();
    s.shutdown();
}
