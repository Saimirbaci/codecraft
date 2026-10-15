//! M14 QA regressions in the engine: a CMake project's run configurations are listed once with the
//! executable selected (Debug/Profile of the default configuration ran `cmake --build`), and
//! the test at a line in `src/lib.rs` of a package with `src/main.rs` runs from the library.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use codecraft_engine::Session;

fn session(home: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s
}

fn wait_loaded(s: &mut Session) {
    let start = Instant::now();
    while (s.project_loading() || s.jobs_running()) && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!s.project_loading(), "the project loads");
}

fn names(s: &Session) -> Vec<String> {
    s.run_configs.iter().map(|c| c.name.clone()).collect()
}

#[test]
fn cmake_project_selects_its_executable_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("inv");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(inv CXX)\nadd_executable(inventory main.cpp)\n",
    )
    .unwrap();
    std::fs::write(root.join("main.cpp"), "int main() { return 0; }\n").unwrap();
    let mut s = session(&base.join("home"));
    for round in 0..2 {
        // Opened twice (File › Open again, or a reload): still listed once.
        s.execute("file.open", &json!({ "path": root })).unwrap();
        wait_loaded(&mut s);
        let list = names(&s);
        let runs = list.iter().filter(|n| *n == "Run inventory").count();
        assert_eq!(runs, 1, "round {round}: {list:?}");
        let builds = list.iter().filter(|n| *n == "CMake: Build").count();
        assert_eq!(builds, 1, "round {round}: {list:?}");
        assert_eq!(s.run_configs[s.selected_run_config].name, "Run inventory", "round {round}: {list:?}");
    }
    // Opening the project again (closing it first) starts from the executable again.
    let build = s.run_configs.iter().position(|c| c.name == "CMake: Build").unwrap();
    s.selected_run_config = build;
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait_loaded(&mut s);
    assert_eq!(s.run_configs[s.selected_run_config].name, "Run inventory", "a fresh open selects the executable again");
    s.shutdown();
}

fn cargo_available() -> bool {
    std::process::Command::new("cargo").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn test_in_lib_rs_runs_from_the_library_target() {
    if !cargo_available() {
        eprintln!("skipping: cargo is missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("shop");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn two() -> u32 {\n    2\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {\n        assert_eq!(super::two(), 2);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"{}\", shop::two());\n}\n").unwrap();
    let mut s = session(&base.join("home"));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait_loaded(&mut s);
    // The gutter's Run on `fn it_works` (line 8, 0-based 7).
    s.execute("run.contextConfiguration", &json!({ "path": root.join("src/lib.rs"), "line": 7 })).unwrap();
    let tab = s.ws.runs.last().expect("a Run tab");
    assert!(tab.command_line.contains("--lib"), "{}", tab.command_line);
    assert!(!tab.command_line.contains("--bin"), "{}", tab.command_line);
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(120) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let tab = s.ws.runs.last().unwrap();
    let text: Vec<&str> = tab.lines.iter().map(|l| l.text.as_str()).collect();
    assert!(text.iter().any(|l| l.contains("1 passed")), "{text:?}");
    s.shutdown();
}
