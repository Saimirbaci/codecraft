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
