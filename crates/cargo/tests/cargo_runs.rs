//! End-to-end runs of the real `cargo` and `rustfmt` on throwaway projects (no dependencies, so
//! they work offline). Each test skips when the tool is not installed.

// clippy.toml's allow-*-in-tests covers `#[test]` fns only, not this file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use codecraft_cargo::{
    CargoCommand, CargoError, CargoInvocation, CargoMessage, Level, RunEvent, TargetSelector, TestEvent, TestOutcome,
    find_cargo, find_manifest, metadata, rustfmt, spawn,
};
use crossbeam_channel::{Receiver, RecvTimeoutError};

/// Generous: the first build of a fixture compiles std-only code but may share a busy machine.
const BUILD_TIMEOUT: Duration = Duration::from_secs(240);

const DIAG_LIB: &str = r#"pub fn answer() -> i32 {
    let unused = 1;
    42
}

#[cfg(test)]
mod tests {
    #[test]
    fn passes() {
        assert_eq!(super::answer(), 42);
    }

    #[test]
    fn fails() {
        assert_eq!(super::answer(), 41);
    }
}
"#;

const SLEEPER_MAIN: &str = r#"fn main() {
    println!("sleeping {}", std::process::id());
    std::thread::sleep(std::time::Duration::from_secs(30));
    println!("woke up");
}
"#;

fn cargo_available() -> bool {
    let ok =
        Command::new(find_cargo()).arg("--version").stdin(Stdio::null()).output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("cargo not found; skipping");
    }
    ok
}

/// A standalone package (its own `[workspace]`, so no enclosing workspace can claim it).
fn fixture(name: &str, file: &str, contents: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
    )
    .unwrap();
    let path = dir.path().join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    dir
}

fn invocation(command: CargoCommand, dir: &Path) -> CargoInvocation {
    let mut inv = CargoInvocation::new(command, dir);
    inv.extra_args.push("--offline".into());
    inv.env.push(("CARGO_TARGET_DIR".into(), dir.join("target").display().to_string()));
    inv
}

/// Events up to and including `Exited`, asserting nothing follows it.
fn collect(rx: &Receiver<RunEvent>, timeout: Duration) -> Vec<RunEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                let exited = matches!(event, RunEvent::Exited { .. });
                events.push(event);
                if exited {
                    break;
                }
            }
            Err(e) => panic!("no Exited within {timeout:?} ({e}); got {events:#?}"),
        }
    }
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected));
    events
}

fn exit_of(events: &[RunEvent]) -> (Option<i32>, bool) {
    match events.last() {
        Some(RunEvent::Exited { code, success, .. }) => (*code, *success),
        other => panic!("last event is not Exited: {other:?}"),
    }
}

fn stderr_text(events: &[RunEvent]) -> String {
    events
        .iter()
        .filter_map(|e| if let RunEvent::Stderr(l) = e { Some(l.as_str()) } else { None })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn check_reports_a_warning() {
    if !cargo_available() {
        return;
    }
    let dir = fixture("diag_fixture", "src/lib.rs", DIAG_LIB);
    let inv = invocation(CargoCommand::Check, dir.path());
    let (_handle, rx) = spawn(&inv).unwrap();
    let events = collect(&rx, BUILD_TIMEOUT);

    assert!(matches!(&events[0], RunEvent::Started { command_line } if command_line.contains("check")));
    let warning = events
        .iter()
        .find_map(|e| match e {
            RunEvent::Message(CargoMessage::CompilerMessage { diagnostic, manifest_path, .. })
                if diagnostic.level == Level::Warning =>
            {
                Some((diagnostic.clone(), manifest_path.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no warning in {events:#?}"));
    let (diagnostic, manifest_path) = warning;
    assert!(diagnostic.message.contains("unused variable"), "{diagnostic:?}");
    assert_eq!(diagnostic.code.as_deref(), Some("unused_variables"));
    let span = diagnostic.primary_span().unwrap();
    assert_eq!((span.file_name.as_str(), span.line_start), ("src/lib.rs", 2));
    assert!(manifest_path.is_some_and(|p| p.ends_with("Cargo.toml")));
    // The rendered diagnostic reads like a terminal on the console.
    assert!(stderr_text(&events).contains("unused variable"), "{events:#?}");
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Message(CargoMessage::CompilerArtifact { target_name, executable: None, .. }) if target_name == "diag_fixture"
    )));
    assert!(events.contains(&RunEvent::Message(CargoMessage::BuildFinished { success: true })));
    assert_eq!(exit_of(&events), (Some(0), true));
}

#[test]
fn test_reports_results_and_failure() {
    if !cargo_available() {
        return;
    }
    let dir = fixture("diag_fixture", "src/lib.rs", DIAG_LIB);
    let mut inv = invocation(CargoCommand::Test, dir.path());
    inv.target = Some(TargetSelector::Lib);
    let (handle, rx) = spawn(&inv).unwrap();
    let events = collect(&rx, BUILD_TIMEOUT);

    let tests: Vec<&TestEvent> =
        events.iter().filter_map(|e| if let RunEvent::Test(t) = e { Some(t) } else { None }).collect();
    assert!(tests.contains(&&TestEvent::SuiteStarted { count: 2 }), "{tests:#?}");
    assert!(
        tests.contains(&&TestEvent::Result { name: "tests::passes".into(), outcome: TestOutcome::Ok }),
        "{tests:#?}"
    );
    assert!(
        tests.contains(&&TestEvent::Result { name: "tests::fails".into(), outcome: TestOutcome::Failed }),
        "{tests:#?}"
    );
    assert!(
        tests.iter().any(|t| matches!(t, TestEvent::SuiteFinished { passed: 1, failed: 1, ignored: 0, .. })),
        "{tests:#?}"
    );
    let (code, success) = exit_of(&events);
    assert!(!success);
    assert_ne!(code, Some(0));
    assert!(!handle.is_running());
}

#[test]
fn run_is_killed_with_its_child() {
    if !cargo_available() {
        return;
    }
    let dir = fixture("sleeper_fixture", "src/main.rs", SLEEPER_MAIN);
    let inv = invocation(CargoCommand::Run, dir.path());
    let (handle, rx) = spawn(&inv).unwrap();

    // Wait for the program itself (not just cargo) to be running.
    let deadline = Instant::now() + BUILD_TIMEOUT;
    let program_pid = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(RunEvent::Stdout(line)) if line.starts_with("sleeping ") => {
                break line.trim_start_matches("sleeping ").to_owned();
            }
            Ok(RunEvent::Exited { .. }) => panic!("exited before the program started"),
            Ok(_) => {}
            Err(e) => panic!("program never started: {e}"),
        }
    };
    assert!(handle.is_running());
    assert_ne!(handle.pid(), 0);

    let asked = Instant::now();
    handle.kill().unwrap();
    let events = collect(&rx, Duration::from_secs(15));
    assert!(asked.elapsed() < Duration::from_secs(10), "kill took {:?}", asked.elapsed());
    assert!(!exit_of(&events).1);
    assert!(!events.contains(&RunEvent::Stdout("woke up".into())));
    assert!(!handle.is_running());

    // The program was cargo's child; the process-group kill must have reached it too.
    let gone_by = Instant::now() + Duration::from_secs(5);
    while Command::new("kill").args(["-0", &program_pid]).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        assert!(Instant::now() < gone_by, "program {program_pid} outlived the kill");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn metadata_of_a_real_package() {
    if !cargo_available() {
        return;
    }
    let dir = fixture("meta_fixture", "src/lib.rs", "pub fn f() {}\n");
    fs::create_dir_all(dir.path().join("src/bin")).unwrap();
    fs::write(dir.path().join("src/bin/tool.rs"), "fn main() {}\n").unwrap();
    let root = dir.path().canonicalize().unwrap();

    let manifest = find_manifest(&dir.path().join("src/bin/tool.rs")).unwrap();
    assert_eq!(manifest.canonicalize().unwrap(), root.join("Cargo.toml"));

    let meta = metadata(dir.path(), true).unwrap();
    assert_eq!(meta.workspace_root.canonicalize().unwrap(), root);
    assert_eq!(meta.workspace_members.len(), 1);
    let package = meta.package_for_file(&meta.workspace_root.join("src/bin/tool.rs")).unwrap();
    assert_eq!(package.name, "meta_fixture");
    assert_eq!(package.edition, "2021");
    let mut targets: Vec<(String, bool, bool)> =
        package.targets.iter().map(|t| (t.name.clone(), t.is_lib(), t.is_bin())).collect();
    targets.sort();
    assert_eq!(targets, [("meta_fixture".into(), true, false), ("tool".into(), false, true)]);

    // A manifest path works as well as its directory.
    assert_eq!(metadata(&dir.path().join("Cargo.toml"), true).unwrap().packages.len(), 1);
}

#[test]
fn metadata_failure_carries_stderr() {
    if !cargo_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Cargo.toml"), "this is not toml [[[\n").unwrap();
    match metadata(dir.path(), true) {
        Err(CargoError::Failed { command, stderr }) => {
            assert_eq!(command, "cargo metadata --format-version 1 --no-deps");
            assert!(!stderr.is_empty());
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn rustfmt_or_skip(source: &str, edition: &str, cwd: &Path) -> Option<codecraft_cargo::Result<String>> {
    match rustfmt(source, edition, cwd) {
        Err(CargoError::Spawn { program, source }) => {
            eprintln!("rustfmt ({program}) not runnable: {source}; skipping");
            None
        }
        other => Some(other),
    }
}

#[test]
fn rustfmt_formats_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let Some(formatted) = rustfmt_or_skip("fn main(){let x=1;println!(\"{}\",x);}", "2021", dir.path()) else {
        return;
    };
    assert_eq!(formatted.unwrap(), "fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n");

    assert_eq!(rustfmt("async fn f(){}\n", "2018", dir.path()).unwrap(), "async fn f() {}\n");

    match rustfmt("fn main( {", "2021", dir.path()) {
        Err(CargoError::Failed { command, stderr }) => {
            assert!(command.starts_with("rustfmt --edition 2021"), "{command}");
            assert!(!stderr.is_empty());
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // Large input must not deadlock on full pipes.
    let big: String = (0..20_000).map(|i| format!("fn f{i}(){{let a={i};}}\n")).collect();
    let formatted = rustfmt(&big, "2021", dir.path()).unwrap();
    assert!(formatted.contains("fn f19999() {\n    let a = 19999;\n}\n"));

    // A missing working directory is ignored rather than failing the spawn.
    let missing: PathBuf = dir.path().join("nope");
    assert_eq!(rustfmt("fn a(){}", "", &missing).unwrap(), "fn a() {}\n");
}
