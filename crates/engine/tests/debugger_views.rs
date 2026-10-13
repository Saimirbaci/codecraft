//! Memory, Disassembly, Registers, watchpoints and the LLDB console against a real lldb-dap and
//! a small C program, and Show Assembly with cargo and cc. Skips when the tools are missing.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::debug::{DebugStatus, console_text};
use codecraft_engine::debug_views::DisassemblyRow;
use codecraft_engine::{Session, dap};

const WAIT: Duration = Duration::from_secs(120);

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            let console: Vec<&str> = s.debug.console.iter().map(console_text).collect();
            panic!(
                "timed out waiting for {what}; status {:?}; views {:?}; console:\n{}",
                s.debug.session.as_ref().map(|x| x.status()),
                s.debug.views,
                console.join("\n")
            );
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn settled(s: &Session) -> bool {
    s.debug.is_stopped() && !s.debug.settling()
}

fn stopped_line(s: &Session) -> Option<usize> {
    if !settled(s) {
        return None;
    }
    s.debug.session.as_ref()?.selected_frame().map(|f| f.line)
}

fn cc() -> Option<PathBuf> {
    dap::discover::which("cc").or_else(|| dap::discover::which("clang"))
}

const PROGRAM: &str = "#include <stdio.h>\n\
int counter = 0;\n\
int main(void) {\n\
    int x = 41;\n\
    char buf[8] = \"hello\";\n\
    x = x + 1;\n\
    counter = x;\n\
    counter += 1;\n\
    printf(\"%d %s %d\\n\", x, buf, counter);\n\
    return 0;\n\
}\n";

#[test]
fn memory_disassembly_registers_and_watchpoints_with_lldb_dap() {
    let Some(cc) = cc() else {
        eprintln!("skipping: no C compiler");
        return;
    };
    if dap::discover::find_lldb_dap().is_err() {
        eprintln!("skipping: lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let source = dir.join("t.c");
    std::fs::write(&source, PROGRAM).unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    // Line 6 (0-based 5): `x = x + 1;`.
    s.execute("debug.setBreakpoint", &json!({ "path": source, "line": 5 })).unwrap();
    s.execute(
        "debug.executable",
        &json!({
            "program": dir.join("t"),
            "cwd": dir,
            "build": [cc.display().to_string(), "-g", "-O0", "-o", "t", "t.c"],
        }),
    )
    .unwrap();
    pump_until(&mut s, "the breakpoint", |s| stopped_line(s) == Some(5));

    // Memory at &x: 41, then `buf` four bytes later or so; write 0x63 (99) into x.
    s.execute("debug.memory.show", &json!({ "address": "&x" })).unwrap();
    pump_until(&mut s, "the memory", |s| settled(s) && !s.debug.views.memory.loading);
    let m = s.debug.views.memory.clone();
    assert_eq!(m.error, None, "{m:?}");
    let x_address = m.address.unwrap();
    assert_eq!(m.byte_at(x_address), Some(41), "{m:?}");
    s.execute("debug.memory.write", &json!({ "address": format!("0x{x_address:x}"), "bytes": "63" })).unwrap();
    pump_until(&mut s, "the write", |s| settled(s) && s.debug.views.memory.byte_at(x_address) == Some(99));
    pump_until(&mut s, "x = 99 in the variables", |s| {
        settled(s)
            && s.debug.session.as_ref().is_some_and(|x| {
                x.scopes
                    .first()
                    .and_then(|sc| x.variables.get(&sc.reference))
                    .is_some_and(|vars| vars.iter().any(|v| v.name == "x" && v.value == "99"))
            })
    });

    // Disassembly: the current instruction and the function around it.
    s.execute("debug.disassembly.show", &Value::Null).unwrap();
    pump_until(&mut s, "the disassembly", |s| {
        settled(s) && !s.debug.views.disassembly.loading && !s.debug.views.disassembly.rows.is_empty()
    });
    let d = s.debug.views.disassembly.clone();
    assert_eq!(d.error, None);
    let current_row = d.current_row().unwrap_or_else(|| panic!("no current instruction: {d:?}"));
    assert!(current_row > 0, "instructions before the program counter: {d:?}");
    assert!(d.rows.iter().any(|r| matches!(r, DisassemblyRow::Instruction { .. })));

    // Step by instruction while the disassembly is shown: still on line 6, a new pc.
    let pc = d.current.unwrap();
    s.execute("run.debuggingActions.stepOver", &Value::Null).unwrap();
    pump_until(&mut s, "the instruction step", |s| {
        settled(s) && s.debug.views.disassembly.current.is_some_and(|c| c != pc) && !s.debug.views.disassembly.loading
    });
    assert_eq!(stopped_line(&s), Some(5));

    // Registers, grouped; the program counter changed with the next step.
    s.execute("debug.registers.show", &Value::Null).unwrap();
    pump_until(&mut s, "the registers", |s| {
        settled(s) && !s.debug.views.registers.loading && !s.debug.views.registers.groups.is_empty()
    });
    s.execute("debug.disassembly.stepOverInstruction", &Value::Null).unwrap();
    pump_until(&mut s, "the registers after a step", |s| {
        settled(s)
            && !s.debug.views.registers.loading
            && s.debug.views.registers.groups.iter().any(|g| g.registers.iter().any(|r| r.changed))
    });
    let changed: Vec<String> = s
        .debug
        .views
        .registers
        .groups
        .iter()
        .flat_map(|g| g.registers.iter().filter(|r| r.changed).map(|r| r.name.clone()))
        .collect();
    assert!(changed.iter().any(|r| r == "pc" || r == "rip"), "{changed:?}");

    // A watchpoint on the global `counter` stops where it is written.
    let globals =
        s.debug.session.as_ref().and_then(|x| x.scopes.iter().find(|sc| sc.name == "Globals").map(|sc| sc.reference));
    let r = s
        .execute("debug.watchpoint.add", &json!({ "name": "counter", "reference": globals, "evaluateName": "counter" }))
        .unwrap();
    let id = r["id"].as_u64().unwrap();
    pump_until(&mut s, "the watchpoint", |s| {
        settled(s) && s.debug.views.watchpoints.iter().any(|w| w.id == id && w.verified == Some(true))
    });
    s.execute("run.debuggingActions.resumeProgram", &Value::Null).unwrap();
    pump_until(&mut s, "the watchpoint hit", |s| {
        settled(s)
            && matches!(s.debug.session.as_ref().map(|x| x.status()), Some(DebugStatus::Stopped { description: Some(ref d), .. }) if d.contains("watchpoint"))
    });
    let line = stopped_line(&s).unwrap();
    assert!((6..=7).contains(&line), "stopped on line {line}");
    assert_eq!(s.execute("debug.watchpoint.remove", &json!({ "id": id })).unwrap()["removed"], true);

    // The LLDB console: commands, history.
    s.execute("debug.consoleEvaluate", &json!({ "expression": "frame variable x" })).unwrap();
    pump_until(&mut s, "the console result", |s| {
        // 99 written above, then `x = x + 1` ran.
        s.debug.console.iter().any(|l| console_text(l).contains("x = 100"))
    });
    assert!(!s.debug.console.iter().any(|l| console_text(l).starts_with("(lldb) ")), "echo is stripped");
    assert_eq!(s.execute("debug.console.history", &Value::Null).unwrap()["history"], json!(["frame variable x"]));

    s.execute("run.debuggingActions.resumeProgram", &Value::Null).unwrap();
    pump_until(&mut s, "the exit", |s| {
        matches!(s.debug.session.as_ref().map(|x| x.status()), Some(DebugStatus::Exited { .. }))
    });
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    let out: Vec<&str> = s.debug.console.iter().map(console_text).collect();
    assert!(out.iter().any(|l| l.contains("100 hello 101")), "{out:?}");
    s.shutdown();
}

#[test]
fn core_dump_and_remote_targets_are_checked() {
    let mut s = Session::new();
    s.lsp_enabled = false;
    assert!(s.execute("debug.coreDump", &json!({ "core": "/no/such/core" })).is_err());
    assert!(s.execute("debug.remote", &json!({ "address": "nohost" })).is_err());
    assert!(s.execute("debug.remote", &json!({ "address": "h:1", "program": "/no/such/program" })).is_err());
    // Without params the forms open in the Debug tool window.
    s.execute("debug.coreDump", &Value::Null).unwrap();
    assert_eq!(s.debug.form, Some(codecraft_engine::debug::DebugForm::CoreDump));
    assert!(s.debug.choosing);
    s.execute("debug.remote", &Value::Null).unwrap();
    assert_eq!(s.debug.form, Some(codecraft_engine::debug::DebugForm::Remote));
}

fn cargo() -> Option<PathBuf> {
    dap::discover::which("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

fn wait_for_asm(s: &mut Session) -> codecraft_engine::asm::AsmOutput {
    pump_until(s, "Show Assembly", |s| !s.debug.asm.running());
    if let Some(e) = &s.debug.asm.error {
        panic!("Show Assembly failed: {e}");
    }
    s.debug.asm.last.clone().unwrap()
}

fn active_text(s: &Session) -> (PathBuf, String, bool) {
    let (_, d) = s.ws.active().unwrap();
    (d.path.clone().unwrap(), d.doc.text(), d.read_only)
}

#[test]
fn show_assembly_for_rust_and_c() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    if let Some(cc) = cc() {
        let file = dir.join("calc.c");
        std::fs::write(&file, "int other(int a) { return a - 1; }\n\nint triple(int a) {\n    return a * 3;\n}\n")
            .unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        let r = s.execute("code.showAssembly", &json!({ "path": file, "line": 3 })).unwrap();
        assert_eq!(r["function"], "triple", "{r} with {}", cc.display());
        let out = wait_for_asm(&mut s);
        assert!(out.symbol.as_deref().is_some_and(|x| x.trim_start_matches('_') == "triple"), "{out:?}");
        let (path, text, read_only) = active_text(&s);
        assert_eq!(path, out.file);
        assert!(read_only);
        assert!(text.contains("triple") && !text.contains("other:"), "{text}");
    } else {
        eprintln!("skipping the C part: no C compiler");
    }

    if cargo().is_none() {
        eprintln!("skipping the Rust part: cargo is missing");
        return;
    }
    let project = dir.join("asmproj");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"asm-proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let lib = project.join("src/lib.rs");
    std::fs::write(
        &lib,
        "pub struct Calc;\n\nimpl Calc {\n    #[inline(never)]\n    pub fn twice(&self, x: u64) -> u64 {\n        x.wrapping_mul(2)\n    }\n}\n\npub mod inner {\n    #[inline(never)]\n    pub fn twice(x: u8) -> u8 {\n        x ^ 0x5a\n    }\n}\n",
    )
    .unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": project })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    let r = s.execute("code.showAssembly", &json!({ "path": lib, "line": 5 })).unwrap();
    assert_eq!(r["function"], "asm_proj::Calc::twice");
    let out = wait_for_asm(&mut s);
    let symbol = out.symbol.clone().unwrap_or_else(|| panic!("not found: {out:?}"));
    assert!(symbol.contains("4Calc5twice") || out.tool == "cargo asm", "{symbol}");
    let (_, text, read_only) = active_text(&s);
    assert!(read_only);
    assert!(text.contains("Calc::twice"), "{text}");
    // The module's `twice` is a different function.
    s.execute("code.showAssembly", &json!({ "path": lib, "line": 12 })).unwrap();
    let out = wait_for_asm(&mut s);
    assert!(out.symbol.as_deref().is_some_and(|x| x.contains("5inner5twice")) || out.tool == "cargo asm", "{out:?}");
    assert!(Path::new(&out.file).is_file());
}

/// A gdb-remote stub on this machine: debugserver (macOS), lldb-server or gdbserver (Linux).
fn remote_stub(port: u16, program: &Path) -> Option<std::process::Child> {
    let address = format!("127.0.0.1:{port}");
    let mut candidates: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for debugserver in [
        "/Applications/Xcode.app/Contents/SharedFrameworks/LLDB.framework/Versions/A/Resources/debugserver",
        "/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Versions/A/Resources/debugserver",
    ] {
        candidates.push((PathBuf::from(debugserver), vec![address.clone(), program.display().to_string()]));
    }
    if let Some(p) = dap::discover::which("lldb-server") {
        candidates.push((p, vec!["gdbserver".into(), address.clone(), "--".into(), program.display().to_string()]));
    }
    if let Some(p) = dap::discover::which("gdbserver") {
        candidates.push((p, vec![address.clone(), program.display().to_string()]));
    }
    let (stub, args) = candidates.into_iter().find(|(p, _)| p.is_file())?;
    std::process::Command::new(stub)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

#[test]
fn remote_debugging_through_a_gdb_remote_stub() {
    let Some(cc) = cc() else {
        eprintln!("skipping: no C compiler");
        return;
    };
    if dap::discover::find_lldb_dap().is_err() {
        eprintln!("skipping: lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let source = dir.join("t.c");
    std::fs::write(&source, PROGRAM).unwrap();
    let built = std::process::Command::new(&cc)
        .args(["-g", "-O0", "-o", "t", "t.c"])
        .current_dir(&dir)
        .status()
        .is_ok_and(|s| s.success());
    assert!(built, "cc failed");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let Some(mut stub) = remote_stub(port, &dir.join("t")) else {
        eprintln!("skipping: no debugserver, lldb-server or gdbserver");
        return;
    };
    std::thread::sleep(Duration::from_millis(800));
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("debug.setBreakpoint", &json!({ "path": source, "line": 6 })).unwrap();
    s.execute("debug.remote", &json!({ "address": format!("127.0.0.1:{port}"), "program": dir.join("t") })).unwrap();
    // The stub starts the program stopped at its entry; run to the breakpoint.
    let start = Instant::now();
    loop {
        s.pump();
        if let Some(line) = stopped_line(&s) {
            if line == 6 {
                break;
            }
            s.execute("run.debuggingActions.resumeProgram", &Value::Null).unwrap();
        }
        let status = s.debug.session.as_ref().map(|x| x.status());
        if matches!(status, Some(DebugStatus::Failed { .. })) || start.elapsed() > WAIT {
            let _ = stub.kill();
            let console: Vec<&str> = s.debug.console.iter().map(console_text).collect();
            panic!("remote session did not reach the breakpoint: {status:?}\n{}", console.join("\n"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.debug.session.as_ref().unwrap().debuggee_pid.is_none(), "a remote pid is never killed locally");
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    let _ = stub.kill();
    let _ = stub.wait();
}

#[test]
fn core_dump_debugging() {
    let (Some(cc), Some(lldb)) = (cc(), dap::discover::which("lldb")) else {
        eprintln!("skipping: no C compiler or lldb");
        return;
    };
    if dap::discover::find_lldb_dap().is_err() {
        eprintln!("skipping: lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("t.c"), PROGRAM).unwrap();
    let ok = |cmd: &mut std::process::Command| cmd.current_dir(&dir).output().is_ok_and(|o| o.status.success());
    assert!(ok(std::process::Command::new(&cc).args(["-g", "-O0", "-o", "t", "t.c"])), "cc failed");
    // A small core taken at line 6 (`x = x + 1;`, x is 41).
    let core = dir.join("core.t");
    let saved = ok(std::process::Command::new(&lldb).args([
        "--batch",
        "-o",
        "b t.c:6",
        "-o",
        "run",
        "-o",
        &format!("process save-core --style stack {}", core.display()),
        "-o",
        "kill",
        "./t",
    ]));
    if !saved || !core.is_file() {
        eprintln!("skipping: lldb could not save a core file here");
        return;
    }
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("debug.coreDump", &json!({ "core": core, "program": dir.join("t") })).unwrap();
    pump_until(&mut s, "the core's stop", |s| stopped_line(s).is_some());
    assert_eq!(stopped_line(&s), Some(5));
    pump_until(&mut s, "x in the core", |s| {
        s.debug.session.as_ref().is_some_and(|x| {
            x.scopes
                .first()
                .and_then(|sc| x.variables.get(&sc.reference))
                .is_some_and(|vars| vars.iter().any(|v| v.name == "x" && v.value == "41"))
        })
    });
    assert!(s.debug.session.as_ref().unwrap().debuggee_pid.is_none(), "a core's pid is never killed");
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
}
