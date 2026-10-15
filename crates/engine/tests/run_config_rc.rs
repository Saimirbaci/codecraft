//! M13 track RC: before-launch tasks, configuration templates, configurations stored as project
//! files, debugger options, the CMake Application kind, jest results, the Zig and TypeScript tool
//! windows (fake `zig`/`tsc`), Modify Selected Code (a 127.0.0.1 fake provider), command
//! completion and Compile and Reload Modified Files.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{CommandError, Popup, Session};

fn wait_idle(s: &mut Session) {
    let start = Instant::now();
    while (s.jobs_running() || s.busy_without_servers()) && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    s.pump();
}

trait Idle {
    fn busy_without_servers(&self) -> bool;
}

impl Idle for Session {
    fn busy_without_servers(&self) -> bool {
        self.launch.busy() || self.zig.busy() || self.typescript.busy() || self.modify.busy()
    }
}

fn open(root: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&root.parent().unwrap().join("home"));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait_idle(&mut s);
    s
}

fn project(dir: &Path) -> PathBuf {
    let root = dir.join("demo");
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn index_of(s: &Session, name: &str) -> usize {
    s.run_configs.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no configuration {name}"))
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn before_launch_tasks_run_in_order_and_failures_stop_the_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    let mut s = open(&root);
    let log = |what: &str| format!("sh -c 'echo {what} >> order.txt'");
    s.execute("runConfig.create", &json!({ "name": "Gen", "kind": "command", "command": log("gen") })).unwrap();
    s.execute(
        "runConfig.create",
        &json!({
            "name": "Main",
            "kind": "command",
            "command": log("main"),
            "beforeLaunch": [
                { "kind": "external", "command": log("ext") },
                "run:Gen",
            ],
        }),
    )
    .unwrap();
    let preview = s.execute("runConfig.beforeLaunch.preview", &json!({ "name": "Main" })).unwrap();
    assert_eq!(preview, json!([format!("Run external tool '{}'", log("ext")), "Run 'Gen'"]));
    let r = s.execute("run.run", &json!({ "config": index_of(&s, "Main") })).unwrap();
    assert_eq!(r["pending"], true, "{r}");
    wait_idle(&mut s);
    let order = std::fs::read_to_string(root.join("order.txt")).unwrap();
    assert_eq!(order, "ext\ngen\nmain\n");
    assert!(s.launch.pending.is_none() && s.launch.last_error.is_none());

    // A failing task stops the launch and names the task.
    s.execute(
        "runConfig.create",
        &json!({ "name": "Guarded", "kind": "command", "command": log("guarded"), "beforeLaunch": [{ "kind": "external", "command": "false" }] }),
    )
    .unwrap();
    s.execute("run.run", &json!({ "config": index_of(&s, "Guarded") })).unwrap();
    wait_idle(&mut s);
    let err = s.launch.last_error.clone().unwrap();
    assert!(err.contains("'false' failed") && err.contains("'Guarded' was not started"), "{err}");
    assert!(!std::fs::read_to_string(root.join("order.txt")).unwrap().contains("guarded"));
    assert!(s.ws.notifications.iter().any(|n| n.message.contains("was not started")));

    // `beforeLaunch: false` runs the configuration alone.
    s.execute("run.run", &json!({ "config": index_of(&s, "Guarded"), "beforeLaunch": false })).unwrap();
    wait_idle(&mut s);
    assert!(std::fs::read_to_string(root.join("order.txt")).unwrap().ends_with("guarded\n"));

    // Loops and unknown configurations are refused before anything runs.
    s.execute(
        "runConfig.create",
        &json!({ "name": "A", "kind": "command", "command": "true", "beforeLaunch": ["run:B"] }),
    )
    .unwrap();
    s.execute(
        "runConfig.create",
        &json!({ "name": "B", "kind": "command", "command": "true", "beforeLaunch": ["run:A"] }),
    )
    .unwrap();
    let e = s.execute("run.run", &json!({ "config": index_of(&s, "A") })).unwrap_err();
    assert!(e.to_string().contains("loop"), "{e}");
    s.execute(
        "runConfig.create",
        &json!({ "name": "C", "kind": "command", "command": "true", "beforeLaunch": ["run:Nope"] }),
    )
    .unwrap();
    let e = s.execute("run.run", &json!({ "config": index_of(&s, "C") })).unwrap_err();
    assert!(e.to_string().contains("no run configuration named `Nope`"), "{e}");
    // A configuration can't run itself; bad task shapes are bad params.
    let i = s.run_store.user.iter().position(|c| c.name == "C").unwrap();
    for bad in [
        json!(["run:C"]),
        json!([{ "kind": "teleport" }]),
        json!("build"),
        json!([{ "kind": "external", "command": "'" }]),
    ] {
        assert!(
            matches!(
                s.execute("runConfig.update", &json!({ "index": i, "beforeLaunch": bad })),
                Err(CommandError::BadParams(_))
            ),
            "{bad}"
        );
    }
    // Saved with the configuration.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".codecraft/run-configurations.json")).unwrap())
            .unwrap();
    let main = saved["configurations"].as_array().unwrap().iter().find(|c| c["name"] == "Main").unwrap();
    assert_eq!(main["before_launch"][1], json!({ "kind": "runConfiguration", "name": "Gen" }));
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn stopping_a_before_launch_task_cancels_the_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    let mut s = open(&root);
    s.execute(
        "runConfig.create",
        &json!({ "name": "Slow", "kind": "command", "command": "sh -c 'echo ran > ran.txt'", "beforeLaunch": [{ "kind": "external", "command": "sleep 30" }] }),
    )
    .unwrap();
    s.execute("run.run", &json!({ "config": index_of(&s, "Slow") })).unwrap();
    let state = s.execute("runConfig.beforeLaunch.state", &Value::Null).unwrap();
    assert_eq!(state["pending"]["config"], "Slow");
    s.execute("run.stop", &Value::Null).unwrap();
    wait_idle(&mut s);
    assert!(s.launch.last_error.as_deref().unwrap().contains("was stopped"));
    assert!(!root.join("ran.txt").exists());
    s.shutdown();
}

#[test]
fn templates_give_new_configurations_their_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let mut s = open(&root);
    s.execute(
        "runConfig.template.update",
        &json!({ "kind": "command", "env": "MODE=dev", "workingDir": "sub", "beforeLaunch": ["build"], "debugger": { "stopAtEntry": true } }),
    )
    .unwrap();
    s.execute("runConfig.create", &json!({ "name": "Tool", "kind": "command", "command": "make x" })).unwrap();
    let c = s.run_store.user.iter().find(|c| c.name == "Tool").unwrap().clone();
    assert_eq!(c.env, vec![("MODE".to_string(), "dev".to_string())]);
    assert_eq!(c.working_dir, "sub");
    assert_eq!(c.before_launch.len(), 1);
    assert!(c.debugger.stop_at_entry);
    // A kind without a template starts from its default command.
    s.execute("runConfig.create", &json!({ "name": "Py", "kind": "python" })).unwrap();
    assert_eq!(s.run_store.user.iter().find(|c| c.name == "Py").unwrap().command, "main.py");
    // Templates persist with the project.
    let again = open(&root);
    let list = {
        let mut a = again;
        a.execute("runConfig.templates.list", &Value::Null).unwrap()
    };
    let command = list.as_array().unwrap().iter().find(|t| t["kind"] == "command").unwrap();
    assert_eq!(command["customized"], true);
    assert_eq!(command["env"], "MODE=dev");
    // Through the dialog: draft until Apply; reset drops it.
    s.execute("run.editConfigurations", &Value::Null).unwrap();
    s.execute("runConfig.template.select", &json!({ "kind": "command" })).unwrap();
    assert_eq!(
        s.run_store.dialog.as_ref().unwrap().template,
        Some(codecraft_engine::run_config_store::RunKind::Command)
    );
    s.execute("runConfig.template.update", &json!({ "kind": "command", "reset": true })).unwrap();
    assert!(!s.run_store.templates.is_empty(), "saved only on Apply");
    s.execute("runConfig.ok", &Value::Null).unwrap();
    assert!(s.run_store.templates.is_empty());
    assert!(matches!(
        s.execute("runConfig.template.update", &json!({ "kind": "nope" })),
        Err(CommandError::BadParams(_))
    ));
    s.shutdown();
}

#[test]
fn configurations_stored_as_project_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    let mut s = open(&root);
    s.execute(
        "runConfig.create",
        &json!({ "name": "Shared: serve/dev", "kind": "command", "command": "make serve", "storeAsProjectFile": true }),
    )
    .unwrap();
    s.execute("runConfig.create", &json!({ "name": "Mine", "kind": "command", "command": "make mine" })).unwrap();
    let dir = root.join(".codecraft/runConfigurations");
    let file = dir.join("Shared_ serve_dev.json");
    assert!(file.is_file(), "{:?}", std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>()));
    let main: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".codecraft/run-configurations.json")).unwrap())
            .unwrap();
    let names: Vec<&str> =
        main["configurations"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names, ["Mine"]);
    let listed = s.execute("runConfig.list", &Value::Null).unwrap();
    assert_eq!(listed["user"][0]["projectFile"], ".codecraft/runConfigurations/Shared_ serve_dev.json");

    // A teammate's file appears on the next load; a broken one is reported, not fatal.
    std::fs::write(dir.join("teammate.json"), r#"{"kind": "command", "command": "make lint"}"#).unwrap();
    std::fs::write(dir.join("broken.json"), "{").unwrap();
    let mut s = open(&root);
    assert!(s.run_configs.iter().any(|c| c.name == "teammate"));
    assert!(s.run_store.error.as_deref().is_some_and(|e| e.contains("broken.json")));
    let shared = s.run_store.user.iter().find(|c| c.name.starts_with("Shared")).unwrap();
    assert!(shared.store_as_project_file);

    // Renaming moves the file; turning the option off moves it back into the main file.
    let i = s.run_store.user.iter().position(|c| c.name.starts_with("Shared")).unwrap();
    s.execute("runConfig.update", &json!({ "index": i, "name": "Serve" })).unwrap();
    assert!(!file.exists() && dir.join("Serve.json").is_file());
    s.execute("runConfig.update", &json!({ "index": i, "storeAsProjectFile": false })).unwrap();
    assert!(!dir.join("Serve.json").exists());
    assert!(dir.join("broken.json").exists(), "files it never loaded are left alone");
    let main: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".codecraft/run-configurations.json")).unwrap())
            .unwrap();
    assert!(main["configurations"].as_array().unwrap().iter().any(|c| c["name"] == "Serve"));
    s.shutdown();
}

#[test]
fn debugger_options_are_kept_and_used() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(root.join("prog"), "not really a program").unwrap();
    let mut s = open(&root);
    s.execute(
        "runConfig.create",
        &json!({
            "name": "Prog",
            "kind": "command",
            "command": "./prog --flag",
            "debugger": { "initCommands": "settings set target.x86-disassembly-flavor intel\n\nbreakpoint set -n main", "stopAtEntry": true, "env": "DEBUGGEE=1" },
        }),
    )
    .unwrap();
    let c = s.run_store.user.iter().find(|c| c.name == "Prog").unwrap();
    assert_eq!(c.debugger.init_commands.len(), 2);
    assert_eq!(c.debugger.env, vec![("DEBUGGEE".to_string(), "1".to_string())]);
    let listed = s.execute("runConfig.list", &Value::Null).unwrap();
    assert_eq!(listed["user"][0]["debugger"]["stop_at_entry"], true);
    for bad in [json!({ "stopAtEntry": "yes" }), json!({ "initCommands": 5 }), json!(7)] {
        assert!(s.execute("runConfig.update", &json!({ "index": 0, "debugger": bad })).is_err());
    }
    // Debug passes them to the session (the adapter itself may be missing here).
    let _ = s.execute("run.debug", &json!({ "config": index_of(&s, "Prog") }));
    assert!(s.debug.last_options.stop_at_entry);
    assert_eq!(s.debug.last_options.init_commands[1], "breakpoint set -n main");
    let _ = s.execute("run.stop", &Value::Null);
    s.shutdown();
}

#[test]
fn cmake_application_builds_its_target_then_runs_the_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(root.join("CMakeLists.txt"), "project(x)\nadd_executable(app main.cpp)\n").unwrap();
    let mut s = open(&root);
    s.execute("runConfig.create", &json!({ "name": "App", "kind": "cmake", "command": "app -- --port 1" })).unwrap();
    let config = s.run_configs[index_of(&s, "App")].clone();
    let cmd = config.command.unwrap();
    let script = cmd.args.last().unwrap();
    assert!(script.contains("cmake --build") && script.contains("--target app"), "{script}");
    assert!(script.ends_with(&format!("{} --port 1", root.join("cmake-build-debug/app").display())), "{script}");
    // An explicit executable, and a working directory.
    std::fs::create_dir_all(root.join("run")).unwrap();
    let i = s.run_store.user.iter().position(|c| c.name == "App").unwrap();
    s.execute("runConfig.update", &json!({ "index": i, "executable": "out/app2", "workingDir": "run" })).unwrap();
    let cmd = s.run_configs[index_of(&s, "App")].command.clone().unwrap();
    let script = cmd.args.last().unwrap();
    assert!(
        script.contains(&format!("cd {} && {}", root.join("run").display(), root.join("out/app2").display())),
        "{script}"
    );
    // Debug maps it to the executable, built first.
    for bad in ["", "a b -- x"] {
        assert!(s.execute("runConfig.update", &json!({ "index": i, "command": bad })).is_err(), "{bad}");
    }
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn jest_results_fill_the_test_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(
        root.join("math.test.js"),
        "describe('math', () => {\n  test('adds', () => {});\n  test('fails', () => {});\n});\n",
    )
    .unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let report = json!({
        "testResults": [{
            "name": root.join("math.test.js"),
            "status": "failed",
            "assertionResults": [
                { "ancestorTitles": ["math"], "title": "adds", "status": "passed", "duration": 2, "failureMessages": [] },
                { "ancestorTitles": ["math"], "title": "fails", "status": "failed", "duration": 5,
                  "failureMessages": [format!("Error: boom\n    at Object.<anonymous> ({}:3:9)", root.join("math.test.js").display())] }
            ]
        }]
    });
    std::fs::write(tmp.path().join("report.json"), report.to_string()).unwrap();
    script(
        &bin.join("jest"),
        &format!(
            "for a in \"$@\"; do case \"$a\" in --outputFile=*) out=\"${{a#--outputFile=}}\";; esac; done\ncp '{}' \"$out\"\necho 'FAIL math.test.js'\nexit 1\n",
            tmp.path().join("report.json").display()
        ),
    );
    let mut s = open(&root);
    let jest = bin.join("jest").display().to_string();
    s.execute(
        "runConfig.create",
        &json!({ "name": "Jest", "kind": "command", "command": format!("{jest} math.test.js") }),
    )
    .unwrap();
    let tab = s.execute("run.run", &json!({ "config": index_of(&s, "Jest") })).unwrap()["tab"].as_u64().unwrap();
    wait_idle(&mut s);
    let run = s.testing.run(codecraft_engine::workspace::RunTabId(tab)).unwrap();
    assert_eq!(run.framework, codecraft_engine::testing::Framework::Jest);
    let tests: Vec<(&str, codecraft_engine::testing::TestStatus)> =
        run.suites.iter().flat_map(|s| &s.tests).map(|t| (t.name.as_str(), t.status)).collect();
    assert_eq!(
        tests,
        [
            ("math › adds", codecraft_engine::testing::TestStatus::Passed),
            ("math › fails", codecraft_engine::testing::TestStatus::Failed)
        ]
    );
    let failed = &run.suites[0].tests[1];
    assert_eq!(failed.failure.as_ref().map(|f| f.line), Some(2));
    assert_eq!(failed.location.as_ref().map(|f| f.line), Some(2), "found from the test(\"fails\" line");
    // Rerun Failed Tests filters by name.
    let origin = codecraft_engine::testing::rerun_failed_origin(run).unwrap();
    let codecraft_engine::testing::TestOrigin::Process(cmd) = origin else { panic!() };
    assert_eq!(cmd.args.last().map(String::as_str), Some("^(?:math fails)$"));
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn zig_tool_window_lists_runs_and_refuses_bad_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(root.join("build.zig"), "// build\n").unwrap();
    let fake = tmp.path().join("zig");
    script(
        &fake,
        "if [ \"$1 $2\" = \"build -l\" ]; then\n  printf '  install (default)   Copy build artifacts\\n  run                 Run the app\\n  test                Run unit tests\\n'\nelse\n  echo \"zig $*\" > zig-ran.txt\nfi\n",
    );
    let mut s = open(&root);
    assert!(codecraft_engine::zig_tool::is_zig_project(&s));
    // Isolated sessions never run the user's zig.
    assert!(s.execute("zig.refresh", &Value::Null).is_err());
    s.zig.program = Some(fake.clone());
    s.execute("view.toolWindows.zig", &Value::Null).unwrap();
    assert!(s.ws.layout.is_visible(codecraft_engine::workspace::ToolWindowId::Zig));
    wait_idle(&mut s);
    let state = s.execute("zig.steps", &Value::Null).unwrap();
    let names: Vec<&str> = state["steps"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, ["install", "run", "test"]);
    assert_eq!(state["steps"][0]["default"], true);
    s.execute("zig.runStep", &json!({ "step": "test" })).unwrap();
    wait_idle(&mut s);
    assert_eq!(std::fs::read_to_string(root.join("zig-ran.txt")).unwrap(), "zig build test\n");
    for bad in [json!({ "step": "nope" }), json!({ "step": "--prefix" }), json!({})] {
        assert!(s.execute("zig.runStep", &bad).is_err(), "{bad}");
    }
    assert!(
        s.execute("zig.debugStep", &json!({ "step": "test" })).unwrap_err().to_string().contains("doesn't produce")
    );
    s.shutdown();
}

#[cfg(unix)]
#[test]
fn typescript_tool_window_checks_with_tsc() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.ts"), "let x: number = 'a';\n").unwrap();
    std::fs::write(
        root.join("tsconfig.json"),
        "{ // c\n \"compilerOptions\": { \"strict\": true, \"target\": \"ES2022\", },\n \"include\": [\"src\"] }",
    )
    .unwrap();
    let fake = tmp.path().join("tsc");
    script(
        &fake,
        "echo \"$@\" > tsc-args.txt\necho \"src/a.ts(1,5): error TS2322: Type 'string' is not assignable to type 'number'.\"\nexit 2\n",
    );
    let mut s = open(&root);
    assert!(codecraft_engine::typescript_tool::is_typescript_project(&s));
    assert!(s.execute("typescript.check", &Value::Null).is_err(), "no tsc in an isolated session");
    s.typescript.program = Some(fake);
    s.execute("view.toolWindows.typeScript", &Value::Null).unwrap();
    let state = s.execute("typescript.state", &Value::Null).unwrap();
    let options = state["configs"][0]["options"].as_array().unwrap();
    assert!(options.contains(&json!(["strict", "true"])), "{options:?}");
    s.execute("typescript.check", &Value::Null).unwrap();
    wait_idle(&mut s);
    let state = s.execute("typescript.state", &Value::Null).unwrap();
    assert_eq!(state["errors"][0]["code"], "TS2322");
    assert_eq!(state["errors"][0]["line"], 0);
    let args = std::fs::read_to_string(root.join("tsc-args.txt")).unwrap();
    assert!(args.starts_with("--noEmit -p") && args.contains("--pretty false"), "{args}");
    s.execute("typescript.openError", &json!({ "index": 0 })).unwrap();
    let (e, d) = s.ws.active().unwrap();
    assert_eq!(d.path.as_deref(), Some(root.join("src/a.ts").as_path()));
    assert_eq!(e.selections.primary().head, 4);
    let r = s.execute("typescript.restartService", &Value::Null).unwrap();
    assert_eq!(r["restarted"], false);
    wait_idle(&mut s);
    assert!(s.execute("typescript.openError", &json!({ "index": 9 })).is_err());
    s.shutdown();
}

/// A one-shot 127.0.0.1 HTTP server answering like Ollama.
fn fake_ollama(reply: String) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let h = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = conn.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let len = text[..head_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= head_end + 4 + len {
                    break;
                }
            }
        }
        let body = json!({ "message": { "content": reply } }).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(resp.as_bytes()).unwrap();
        String::from_utf8_lossy(&buf).to_string()
    });
    (url, h)
}

fn curl_available() -> bool {
    std::process::Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn modify_selected_code_shows_a_diff_to_accept() {
    if !curl_available() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let (url, server) = fake_ollama("```rust\nfn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n```".into());
    let mut s = Session::new();
    s.ws.settings.commit_ai_provider = "ollama".into();
    s.ws.settings.ollama_url = url;
    let text = "// math\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let d = s.ws.add_document(Some("/tmp/m.rs".into()), text);
    s.ws.open_editor(d).unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": 8, "head": text.chars().count() })).unwrap();
    s.execute("code.modifySelectedCode", &Value::Null).unwrap();
    assert!(s.modify.prompt.is_some());
    assert!(matches!(
        s.execute("editor.modifyCode.submit", &json!({ "instruction": " " })),
        Err(CommandError::BadParams(_))
    ));
    s.execute("editor.modifyCode.setInstruction", &json!({ "text": "use i64" })).unwrap();
    s.execute("editor.modifyCode.submit", &Value::Null).unwrap();
    let start = Instant::now();
    while s.modify.busy() && start.elapsed() < Duration::from_secs(30) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let request = server.join().unwrap();
    assert!(request.contains("POST /api/chat") && request.contains("use i64"), "{request}");
    assert!(request.contains("<FRAGMENT>\\nfn add(a: i32"), "{request}");
    let p = s.modify.proposal.clone().unwrap_or_else(|| panic!("{:?}", s.modify.error));
    assert_eq!((p.added, p.removed), (1, 1));
    // Nothing changed until Accept.
    assert_eq!(s.ws.doc(d).unwrap().doc.text(), text);
    s.execute("editor.modifyCode.accept", &Value::Null).unwrap();
    assert_eq!(s.ws.doc(d).unwrap().doc.text(), "// math\nfn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n");
    s.execute("edit.undo", &Value::Null).unwrap();
    assert_eq!(s.ws.doc(d).unwrap().doc.text(), text, "one undo step");

    // A proposal over code that changed meanwhile is refused.
    s.modify.proposal = Some(codecraft_engine::modify_code::proposal(
        p.editor,
        p.doc,
        (p.start, p.end),
        "x".into(),
        "something else".into(),
        "y".into(),
    ));
    assert!(s.execute("editor.modifyCode.accept", &Value::Null).unwrap_err().to_string().contains("changed"));
    assert!(s.execute("editor.modifyCode.reject", &Value::Null).is_err(), "dropped");
}

#[test]
fn command_completion_lists_actions_after_two_dots() {
    let mut s = Session::new();
    let d = s.ws.add_document(Some("/tmp/c.rs".into()), "let a = 1;\n");
    s.ws.open_editor(d).unwrap();
    s.execute("editor.setSelection", &json!({ "anchor": 10, "head": 10 })).unwrap();
    // Off: `..` is just text.
    s.execute("editor.type", &json!({ "text": "." })).unwrap();
    s.execute("editor.type", &json!({ "text": "." })).unwrap();
    assert!(s.popup.is_none());
    assert_eq!(codecraft_engine::appearance::checked(&s, "code.codeCompletion.enableCommandCompletion"), Some(false));
    s.execute("code.codeCompletion.enableCommandCompletion", &Value::Null).unwrap();
    assert_eq!(codecraft_engine::appearance::checked(&s, "code.codeCompletion.enableCommandCompletion"), Some(true));
    s.execute("edit.undo", &Value::Null).unwrap();
    s.execute("edit.undo", &Value::Null).unwrap();
    s.execute("editor.type", &json!({ "text": "." })).unwrap();
    s.execute("editor.type", &json!({ "text": "." })).unwrap();
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no popup: {:?}", s.popup) };
    assert!(c.items.iter().all(|i| i.kind == "action"));
    assert!(
        c.items.iter().any(|i| i.label == "Comment with Line Comment"),
        "{:?}",
        c.items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    // Typing filters; Enter runs the action and removes `..` and the filter.
    for c in ["C", "w", "L", "C"] {
        s.execute("editor.type", &json!({ "text": c })).unwrap();
    }
    let Some(Popup::Completion(c)) = &s.popup else { panic!("closed while filtering") };
    let first = c.visible.first().and_then(|i| c.items.get(*i)).unwrap();
    assert_eq!(first.label, "Comment with Line Comment");
    s.execute("editor.completion.accept", &Value::Null).unwrap();
    assert_eq!(s.ws.doc(d).unwrap().doc.text(), "// let a = 1;\n");
    // `...` is not a request.
    s.execute("editor.type", &json!({ "text": "x..." })).unwrap();
    assert!(s.popup.is_none());
}

#[test]
fn compile_and_reload_restarts_the_session_with_a_new_build() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Session::new();
    assert!(s.enabled("run.debuggingActions.compileAndReloadModifiedFiles").is_err());
    let program = tmp.path().join("prog");
    std::fs::write(&program, "").unwrap();
    // A session that is building its program (a slow fake build).
    let target = json!({
        "kind": "executable",
        "name": "prog",
        "program": program,
        "cwd": tmp.path(),
        "build": ["sh", "-c", "sleep 5"],
    });
    s.execute("debug.launch", &json!({ "target": target })).unwrap();
    s.debug.last_options.stop_at_entry = true;
    let r = s.execute("run.debuggingActions.compileAndReloadModifiedFiles", &Value::Null).unwrap();
    assert!(r["message"].as_str().unwrap().contains("no hot swap"), "{r}");
    assert_eq!(s.debug.session.as_ref().unwrap().status, Some(codecraft_engine::debug::DebugStatus::Building));
    assert!(s.debug.last_options.stop_at_entry, "the debugger options are kept");
    // An attached process can't be rebuilt.
    s.debug.last_target = Some(codecraft_engine::debug::DebugTarget::Attach {
        name: "x".into(),
        pid: 12345,
        process: codecraft_engine::debug::ProcessKind::Native,
    });
    assert!(
        s.execute("run.debuggingActions.compileAndReloadModifiedFiles", &Value::Null)
            .unwrap_err()
            .to_string()
            .contains("attached")
    );
    s.shutdown();
}

fn cargo_available() -> bool {
    std::process::Command::new("cargo").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn all_tests_in_a_rust_file_run_by_exact_name() {
    if !cargo_available() {
        eprintln!("skipping: cargo not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("tdemo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod math;\n").unwrap();
    std::fs::write(
        root.join("src/math.rs"),
        "pub fn one() -> i32 { 1 }\n\n#[test]\nfn top() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn inner() {}\n\n    fn helper() {}\n}\n",
    )
    .unwrap();
    let mut s = open(&root);
    let r = s.execute("run.testsInFile", &json!({ "path": root.join("src/math.rs") })).unwrap();
    assert_eq!(r["tests"], json!(["math::top", "math::tests::inner"]));
    let tab = r["tab"].as_u64().unwrap();
    let line = s.ws.runs.iter().find(|t| t.id.0 == tab).unwrap().command_line.clone();
    assert!(line.contains("--lib") && line.ends_with("-- --exact math::top math::tests::inner"), "{line}");
    s.execute("run.stop", &Value::Null).unwrap();
    // No tests: an error, not a run.
    assert!(s.execute("run.testsInFile", &json!({ "path": root.join("src/lib.rs") })).is_err());
    s.shutdown();
}

#[test]
fn cpp_tests_debug_needs_the_cmake_model() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(root.join("CMakeLists.txt"), "project(x)\nadd_executable(t t.cpp)\n").unwrap();
    std::fs::write(root.join("t.cpp"), "#include <gtest/gtest.h>\n\nTEST(Math, Adds) {\n  EXPECT_EQ(1, 1);\n}\n")
        .unwrap();
    let mut s = open(&root);
    let e = s.execute("debug.atCaret", &json!({ "path": root.join("t.cpp"), "line": 3 })).unwrap_err();
    assert!(e.to_string().contains("not loaded"), "{e}");
    let e = s.execute("debug.testsInFile", &json!({ "path": root.join("t.cpp") })).unwrap_err();
    assert!(e.to_string().contains("not loaded"), "{e}");
    assert!(s.execute("run.testsInFile", &json!({ "path": root.join("CMakeLists.txt") })).is_err());
    s.shutdown();
}
