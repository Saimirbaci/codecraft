//! Run › Edit Configurations: user run configurations are created, copied from derived ones,
//! edited, deleted and persisted in `<project>/.codecraft/run-configurations.json`.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::{CommandError, Session};

fn wait_idle(s: &mut Session) {
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn open(root: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait_idle(&mut s);
    assert_eq!(s.run_store.root(), Some(root), "the store follows the project");
    s
}

fn project(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("demo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A Makefile gives derived configurations even without cargo on the machine.
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\ntest:\n\techo t\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    root
}

fn names(s: &Session) -> Vec<String> {
    s.run_configs.iter().map(|c| c.name.clone()).collect()
}

#[test]
fn create_edit_delete_and_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    let mut s = open(&root);
    assert!(names(&s).contains(&"make".to_string()));

    let r = s
        .execute(
            "runConfig.create",
            &json!({ "name": "Serve", "command": "run --bin demo -- --port 8080", "env": "RUST_LOG=debug;PORT=1" }),
        )
        .unwrap();
    assert_eq!(r["index"], 0);
    // Saved at once (no dialog), listed first, selected config unchanged by name.
    let file = root.join(".codecraft/run-configurations.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["configurations"][0]["name"], "Serve");
    assert_eq!(names(&s).first().map(String::as_str), Some("Serve"));
    let serve = &s.run_configs[0];
    let inv = serve.cargo_invocation().unwrap();
    assert_eq!(inv.program_args, vec!["--port", "8080"]);
    assert_eq!(inv.env, vec![("RUST_LOG".to_string(), "debug".to_string()), ("PORT".to_string(), "1".to_string())]);

    // Bad values are errors and change nothing.
    for bad in [
        json!({ "index": 0, "name": "" }),
        json!({ "index": 0, "command": "" }),
        json!({ "index": 0, "workingDir": "no/such/dir" }),
        json!({ "index": 0, "kind": "gradle" }),
        json!({ "index": 0, "env": "NOEQUALS" }),
        json!({ "index": 0, "command": "frobnicate" }),
        json!({ "index": 7, "name": "x" }),
    ] {
        assert!(matches!(s.execute("runConfig.update", &bad), Err(CommandError::BadParams(_))), "{bad}");
    }
    assert_eq!(s.run_store.user[0].name, "Serve");

    // A command configuration with a working directory.
    std::fs::create_dir(root.join("scripts")).unwrap();
    s.execute(
        "runConfig.create",
        &json!({ "name": "Script", "kind": "command", "command": "sh -c 'echo $PORT && pwd'", "workingDir": "scripts" }),
    )
    .unwrap();
    let script = s.run_configs.iter().find(|c| c.name == "Script").unwrap();
    let cmd = script.command.as_ref().unwrap();
    assert_eq!(
        (cmd.program.as_str(), cmd.args.clone()),
        ("sh", vec!["-c".to_string(), "echo $PORT && pwd".to_string()])
    );
    assert_eq!(cmd.cwd, root.join("scripts"));

    // Duplicate names are refused; copies get a unique name.
    assert!(s.execute("runConfig.update", &json!({ "index": 1, "name": "Serve" })).is_err());
    let r = s.execute("runConfig.copy", &json!({ "index": 0 })).unwrap();
    assert_eq!(s.run_store.user[r["index"].as_u64().unwrap() as usize].name, "Serve (2)");
    // Derived configurations copy into user ones.
    let make = s.run_store.derived.iter().position(|c| c.name == "make test").unwrap();
    let r = s.execute("runConfig.copy", &json!({ "index": make, "derived": true })).unwrap();
    let copy = &s.run_store.user[r["index"].as_u64().unwrap() as usize];
    assert_eq!((copy.name.as_str(), copy.command.as_str()), ("make test", "make test"));

    s.execute("runConfig.delete", &json!({ "index": 2 })).unwrap();
    assert_eq!(s.run_store.user.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Serve", "Script", "make test"]);

    // Reopening the project loads them back.
    s.execute("file.closeProject", &Value::Null).unwrap();
    assert!(s.run_store.user.is_empty() && s.run_configs.is_empty());
    let mut s2 = open(&root);
    assert_eq!(names(&s2).get(..3).unwrap(), ["Serve", "Script", "make test"]);
    let listed = s2.execute("runConfig.list", &Value::Null).unwrap();
    assert_eq!(listed["user"][0]["env"], "RUST_LOG=debug;PORT=1");
    assert!(listed["user"][0]["commandLine"].as_str().unwrap().starts_with("cargo run --bin demo"));
    s.shutdown();
    s2.shutdown();
}

#[test]
fn dialog_edits_a_draft_until_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    let mut s = open(&root);
    assert!(s.execute("runConfig.apply", &Value::Null).is_err(), "no dialog yet");
    s.execute("run.editConfigurations", &Value::Null).unwrap();
    let selected = s.run_store.dialog.as_ref().unwrap().selected.unwrap();
    assert!(selected.derived, "only derived configurations exist");
    s.execute("runConfig.create", &json!({ "name": "Draft" })).unwrap();
    assert!(s.run_store.user.is_empty(), "the draft is not saved yet");
    assert!(!root.join(".codecraft/run-configurations.json").exists());
    s.execute("runConfig.update", &json!({ "index": 0, "command": "test --lib" })).unwrap();
    s.execute("runConfig.apply", &Value::Null).unwrap();
    assert_eq!(s.run_store.user.len(), 1);
    assert_eq!(names(&s).first().map(String::as_str), Some("Draft"));
    // Cancel drops later edits.
    s.execute("runConfig.delete", &json!({ "index": 0 })).unwrap();
    s.execute("runConfig.cancel", &Value::Null).unwrap();
    assert_eq!(s.run_store.user.len(), 1);
    assert!(s.run_store.dialog.is_none());
    // OK saves and closes.
    s.execute("run.editConfigurations", &Value::Null).unwrap();
    s.execute("runConfig.select", &json!({ "index": 0 })).unwrap();
    assert!(s.execute("runConfig.select", &json!({ "index": 9 })).is_err());
    s.execute("runConfig.update", &json!({ "index": 0, "name": "Renamed" })).unwrap();
    s.execute("runConfig.ok", &Value::Null).unwrap();
    assert!(s.run_store.dialog.is_none());
    assert_eq!(s.run_store.user[0].name, "Renamed");
    s.shutdown();
}

#[test]
fn corrupt_file_is_reported_not_overwritten_until_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::create_dir_all(root.join(".codecraft")).unwrap();
    std::fs::write(root.join(".codecraft/run-configurations.json"), "{ nope").unwrap();
    let mut s = open(&root);
    assert!(s.run_store.error.as_deref().is_some_and(|e| e.contains("corrupt")));
    assert!(s.ws.notifications.iter().any(|n| n.title == "Run Configurations"));
    s.execute("runConfig.create", &json!({ "name": "A" })).unwrap();
    assert!(root.join(".codecraft/run-configurations.json.bad").is_file());
    s.shutdown();
}

#[test]
fn commands_need_a_project() {
    let mut s = Session::new();
    s.lsp_enabled = false;
    for id in ["run.editConfigurations", "runConfig.create", "runConfig.list"] {
        assert!(matches!(s.execute(id, &Value::Null), Err(CommandError::Disabled(_))), "{id}");
    }
}

#[test]
fn check_on_save_runs_the_external_linter() {
    let cargo_ok = std::process::Command::new(codecraft_engine::cargo::find_cargo())
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !cargo_ok {
        eprintln!("skipping: cargo not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lint");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"lint\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut s = open(&root);
    s.execute("file.open", &json!({ "path": root.join("src/main.rs") })).unwrap();
    // Off by default: saving runs nothing.
    s.execute("editor.type", &json!({ "text": "// a\n" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    assert!(s.ws.runs.is_empty());
    s.execute("settings.set", &json!({ "key": "check_on_save", "value": true })).unwrap();
    s.execute("editor.type", &json!({ "text": "// b\n" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    assert_eq!(s.ws.runs.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["Check"]);
    wait_idle(&mut s);
    s.shutdown();
}

fn run_to_end(s: &mut Session, name: &str) -> Vec<String> {
    let config = config_index(s, name);
    s.execute("run.run", &json!({ "config": config })).unwrap();
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(60) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    s.ws.runs.last().map(|r| r.lines.iter().map(|l| l.text.clone()).collect()).unwrap_or_default()
}

fn config_index(s: &Session, name: &str) -> usize {
    s.run_configs.iter().position(|c| c.name == name).unwrap()
}

#[test]
fn env_file_feeds_the_process_and_explicit_env_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(root.join(".env"), "# comment\nFROM_FILE=file\nSHARED=file\nexport QUOTED=\"a b\"\n").unwrap();
    let mut s = open(&root);
    s.execute(
        "runConfig.create",
        &json!({
            "name": "Echo", "kind": "command", "command": "sh -c 'echo $FROM_FILE/$SHARED/$QUOTED'",
            "env": "SHARED=explicit", "envFile": ".env",
        }),
    )
    .unwrap();
    let listed = s.execute("runConfig.list", &Value::Null).unwrap();
    assert_eq!(listed["user"][0]["envFile"], ".env");
    let lines = run_to_end(&mut s, "Echo");
    assert!(lines.iter().any(|l| l == "file/explicit/a b"), "{lines:?}");

    // The file is re-read on every run.
    std::fs::write(root.join(".env"), "FROM_FILE=changed\n").unwrap();
    let lines = run_to_end(&mut s, "Echo");
    assert!(lines.iter().any(|l| l == "changed/explicit/"), "{lines:?}");

    // A missing file is refused when set, and reported (not ignored) when it vanishes later.
    assert!(s.execute("runConfig.update", &json!({ "index": 0, "envFile": "nope.env" })).is_err());
    std::fs::remove_file(root.join(".env")).unwrap();
    assert!(matches!(
        s.execute("run.run", &json!({ "config": config_index(&s, "Echo") })),
        Err(CommandError::Failed(_))
    ));
    s.execute("runConfig.update", &json!({ "index": 0, "envFile": null })).unwrap();
    assert_eq!(s.run_store.user[0].env_file, "");
    s.shutdown();
}

#[test]
fn python_cpp_zig_and_npm_configurations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    let mut s = open(&root);
    for (kind, command) in
        [("python", "main.py --port 80"), ("zig", "build run"), ("npm", "run dev"), ("cpp", "main.c -O2 -- one two")]
    {
        let r = s.execute("runConfig.create", &json!({ "name": kind, "kind": kind })).unwrap();
        let i = r["index"].as_u64().unwrap() as usize;
        // The default command of a new configuration is a valid one.
        assert!(!s.run_store.user[i].command.is_empty(), "{kind}");
        s.execute("runConfig.update", &json!({ "index": i, "command": command })).unwrap();
    }
    let cmd = |s: &Session, name: &str| s.run_configs[config_index(s, name)].command.clone().unwrap();
    let py = cmd(&s, "python");
    assert!(py.program.ends_with("python") || py.program.ends_with("python3"), "{}", py.program);
    assert_eq!(py.args, ["main.py", "--port", "80"]);
    let zig = cmd(&s, "zig");
    assert_eq!((zig.program.as_str(), zig.args.clone()), ("zig", vec!["build".to_string(), "run".to_string()]));
    let npm = cmd(&s, "npm");
    assert_eq!((npm.program.as_str(), npm.args.clone()), ("npm", vec!["run".to_string(), "dev".to_string()]));
    let cpp = cmd(&s, "cpp");
    let script = &cpp.args[1];
    assert!(script.contains("cc main.c -O2 -o ") && script.ends_with("one two"), "{script}");
    assert!(!script.contains("c++"));
    // A C++ source selects c++; a command without a source file is refused.
    s.execute("runConfig.update", &json!({ "index": 3, "command": "main.cpp" })).unwrap();
    assert!(cmd(&s, "cpp").args[1].contains("c++ main.cpp"));
    assert!(s.execute("runConfig.update", &json!({ "index": 3, "command": "-O2 -- x" })).is_err());
    s.shutdown();
}

#[test]
fn cpp_configuration_compiles_and_runs_with_the_env_file() {
    let cc_ok = std::process::Command::new("cc").arg("--version").output().is_ok_and(|o| o.status.success());
    if !cc_ok {
        eprintln!("skipping: no C compiler");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = project(tmp.path());
    std::fs::write(
        root.join("hello.c"),
        "#include <stdio.h>\n#include <stdlib.h>\nint main(int c, char **v) { printf(\"%s %s\\n\", getenv(\"GREETING\"), v[1]); return 0; }\n",
    )
    .unwrap();
    std::fs::write(root.join(".env"), "GREETING=hello\n").unwrap();
    let mut s = open(&root);
    s.execute(
        "runConfig.create",
        &json!({ "name": "Hello", "kind": "cpp", "command": "hello.c -- world", "envFile": ".env" }),
    )
    .unwrap();
    let lines = run_to_end(&mut s, "Hello");
    assert!(lines.iter().any(|l| l == "hello world"), "{lines:?}");
    s.shutdown();
}

// ---------------------------------------------------------------- generated configurations

/// A Rust + Python project: a binary, a Flask app in `web/`, plus files that are not entry points.
fn polyglot(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("poly");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("web/pkg")).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\nrun:\n\techo run\nlint:\n\ttrue\n").unwrap();
    std::fs::write(root.join("web/requirements.txt"), "flask\n").unwrap();
    std::fs::write(root.join("web/app.py"), "from flask import Flask\napp = Flask(__name__)\n").unwrap();
    std::fs::write(root.join("web/pkg/helpers.py"), "if __name__ == '__main__':\n    pass\n").unwrap();
    std::fs::write(root.join("web/seed.py"), "if __name__ == '__main__':\n    pass\n").unwrap();
    // The web sub-project's own virtualenv.
    std::fs::create_dir_all(root.join("web/.venv/bin")).unwrap();
    std::fs::write(root.join("web/.venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::write(root.join("web/.venv/bin/python"), "").unwrap();
    root
}

fn open_auto(root: &Path, auto: bool) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.run_store.generate.auto = auto;
    s.execute("file.open", &json!({ "path": root })).unwrap();
    wait_idle(&mut s);
    s
}

fn user_names(s: &Session) -> Vec<String> {
    s.run_store.user.iter().map(|c| c.name.clone()).collect()
}

#[test]
fn configurations_are_generated_once_when_a_project_opens() {
    let tmp = tempfile::tempdir().unwrap();
    let root = polyglot(tmp.path());
    let file = root.join(".codecraft/run-configurations.json");

    // Off (the engine default; tests, the CLI): nothing is written.
    let s = open_auto(&root, false);
    assert!(s.run_store.user.is_empty() && !file.exists());
    drop(s);

    let mut s = open_auto(&root, true);
    let names = user_names(&s);
    assert_eq!(names, vec!["web: Flask app:app", "make", "make run"], "entry points only");
    assert!(file.exists());
    assert!(s.ws.notifications.iter().any(|n| n.message.contains("Created 3 run configurations")));
    // The derived `make` / `make run` are not listed twice in the run widget.
    let listed: Vec<String> = s.run_configs.iter().map(|c| c.name.clone()).collect();
    assert_eq!(listed.iter().filter(|n| *n == "make").count(), 1, "{listed:?}");
    assert!(listed.contains(&"make lint".to_string()), "other derived ones stay: {listed:?}");

    // The user deletes everything: reopening does not bring them back.
    for _ in 0..3 {
        s.execute("runConfig.delete", &json!({ "index": 0 })).unwrap();
    }
    drop(s);
    let s = open_auto(&root, true);
    assert!(s.run_store.user.is_empty(), "{:?}", user_names(&s));

    // The setting turns it off.
    let tmp2 = tempfile::tempdir().unwrap();
    let root2 = polyglot(tmp2.path());
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.run_store.generate.auto = true;
    s.execute("settings.set", &json!({ "key": "generate_run_configs", "value": false })).unwrap();
    s.execute("file.open", &json!({ "path": root2 })).unwrap();
    wait_idle(&mut s);
    assert!(s.run_store.user.is_empty() && !root2.join(".codecraft/run-configurations.json").exists());
}

#[test]
fn generate_proposes_and_adds_through_the_dialog() {
    let tmp = tempfile::tempdir().unwrap();
    let root = polyglot(tmp.path());
    let mut s = open(&root);
    s.execute("runConfig.create", &json!({ "name": "My make", "kind": "command", "command": "make" })).unwrap();

    s.execute("runConfig.generate", &Value::Null).unwrap();
    assert!(s.run_store.dialog.is_some(), "Generate opens Edit Configurations");
    wait_idle(&mut s);
    let view = s.execute("runConfig.generate.list", &Value::Null).unwrap();
    assert!(view["busy"].is_null(), "{view}");
    let proposals = view["proposals"].as_array().unwrap();
    let by_name = |n: &str| proposals.iter().find(|p| p["config"]["name"] == n).unwrap_or_else(|| panic!("{n}: {view}"));
    // An existing configuration with the same command is recognised (and keeps its name).
    assert_eq!(by_name("My make")["exists"], true);
    assert_eq!(by_name("My make")["selected"], false);
    assert_eq!(by_name("web: Flask app:app")["config"]["kind"], "flask");
    assert_eq!(by_name("web: Flask app:app")["config"]["working_dir"], "web");
    assert_eq!(by_name("web: Flask app:app")["group"], "Python");

    // Untick `make run`, add the rest; they go to the draft until OK.
    let i = proposals.iter().position(|p| p["config"]["name"] == "make run").unwrap();
    s.execute("runConfig.generate.select", &json!({ "index": i, "selected": false })).unwrap();
    let r = s.execute("runConfig.generate.add", &Value::Null).unwrap();
    assert_eq!(r["added"], json!(["web: Flask app:app"]));
    assert!(s.run_store.generate.view.is_none(), "adding closes the Generate view");
    assert_eq!(user_names(&s), vec!["My make"], "not saved before OK");
    s.execute("runConfig.ok", &Value::Null).unwrap();
    assert_eq!(user_names(&s), vec!["My make", "web: Flask app:app"]);
    let flask = s.run_configs.iter().find(|c| c.name == "web: Flask app:app").unwrap();
    let cmd = flask.command.as_ref().unwrap();
    assert!(cmd.args.windows(2).any(|w| w == ["-m", "flask"]), "{:?}", cmd.args);
    assert!(cmd.cwd.ends_with("web"));
    assert!(cmd.program.ends_with("web/.venv/bin/python"), "the sub-project's interpreter: {}", cmd.program);

    // Generating again: everything already there is marked, nothing new is ticked.
    s.execute("runConfig.generate", &Value::Null).unwrap();
    wait_idle(&mut s);
    let view = s.execute("runConfig.generate.list", &Value::Null).unwrap();
    let ticked: Vec<&Value> = view["proposals"].as_array().unwrap().iter().filter(|p| p["selected"] == true).collect();
    assert_eq!(ticked.len(), 1, "only `make run` is new: {view}");
    s.execute("runConfig.cancel", &Value::Null).unwrap();
    assert!(s.run_store.generate.view.is_none());

    // Bad params are errors, not panics.
    assert!(s.execute("runConfig.generate", &json!({ "agent": "vim" })).is_err());
    assert!(s.execute("runConfig.generate", &json!({ "agent": "claude", "mode": "review" })).is_err());
    assert!(matches!(s.execute("runConfig.generate.add", &Value::Null), Err(CommandError::Disabled(_))));
}

fn have(tool: &str) -> bool {
    std::process::Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn an_agent_refines_the_generated_configurations() {
    if !have("python3") {
        eprintln!("skipping: python3 not found");
        return;
    }
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_runconfig_agent.py");
    let tmp = tempfile::tempdir().unwrap();
    let root = polyglot(tmp.path());
    let mut s = open(&root);
    s.execute("agent.settings", &json!({ "acp": { "antigravity": ["python3", fake] } })).unwrap();

    s.execute("runConfig.generate", &json!({ "agent": "antigravity" })).unwrap();
    let start = Instant::now();
    loop {
        s.pump();
        let v = s.run_store.generate.view.clone().unwrap();
        if v.busy.is_none() && v.agent_job.is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "timed out: {v:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let view = s.execute("runConfig.generate.list", &Value::Null).unwrap();
    assert!(view["error"].is_null(), "{view}");
    let names: Vec<&str> =
        view["proposals"].as_array().unwrap().iter().map(|p| p["config"]["name"].as_str().unwrap()).collect();
    // The agent kept the first detected one, added a worker and its nonsense entry was dropped.
    assert_eq!(names, vec!["web: Flask app:app", "Worker"], "{view}");
    assert_eq!(view["proposals"][1]["source"], "agent");
    assert_eq!(view["proposals"][1]["config"]["env"], json!([["QUEUE", "default"]]));
    assert_eq!(view["notes"].as_array().unwrap().len(), 1, "{view}");
    s.execute("runConfig.generate.add", &Value::Null).unwrap();
    s.execute("runConfig.ok", &Value::Null).unwrap();
    assert_eq!(user_names(&s), vec!["web: Flask app:app", "Worker"]);
    s.shutdown();
}
