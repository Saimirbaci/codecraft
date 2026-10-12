//! Real adapters end to end: lldb-dap on a tiny Rust program and debugpy on a Python script.
//! Each test skips (passes with a note) when its adapter or toolchain is missing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::Value;

use codecraft_dap::launch::{self, NativeLaunch, PythonLaunch};
use codecraft_dap::{DapClient, DapEvent, RequestId, SourceBreakpoint, discover, types};

const WAIT: Duration = Duration::from_secs(60);

struct Driver {
    client: DapClient,
    events: Receiver<DapEvent>,
    /// Events seen while waiting for something else.
    seen: Vec<(String, Value)>,
    /// Responses that arrived while waiting for something else (adapters order them differently).
    answered: Vec<codecraft_dap::Response>,
    output: String,
}

impl Driver {
    fn next(&mut self) -> DapEvent {
        let event = self.events.recv_timeout(WAIT).expect("the adapter went quiet");
        if let DapEvent::Event { event, body } = &event {
            if event == "output" {
                let out: types::OutputEvent = types::parse(body).unwrap();
                self.output.push_str(&out.output);
            }
            self.seen.push((event.clone(), body.clone()));
        }
        event
    }

    fn response(&mut self, id: RequestId) -> Value {
        let r = match self.answered.iter().position(|r| r.id == id) {
            Some(i) => self.answered.remove(i),
            None => loop {
                match self.next() {
                    DapEvent::Response(r) if r.id == id => break r,
                    DapEvent::Response(r) => self.answered.push(r),
                    DapEvent::Exited { .. } => panic!("the adapter exited while waiting for a response"),
                    _ => {}
                }
            },
        };
        r.result.unwrap_or_else(|e| panic!("{} failed: {e}", r.command))
    }

    /// Wait for an event (it may already have arrived).
    fn event(&mut self, name: &str) -> Value {
        if let Some(i) = self.seen.iter().position(|(e, _)| e == name) {
            return self.seen.remove(i).1;
        }
        loop {
            match self.next() {
                DapEvent::Event { event, body } if event == name => {
                    self.seen.pop();
                    return body;
                }
                DapEvent::Response(r) => self.answered.push(r),
                DapEvent::Exited { .. } => panic!("the adapter exited while waiting for {name}"),
                _ => {}
            }
        }
    }

    fn top_frame(&mut self, thread: i64) -> types::StackFrame {
        let id = self.client.stack_trace(thread, 0, 20).unwrap();
        let body: types::StackTraceBody = types::parse(&self.response(id)).unwrap();
        body.stack_frames.into_iter().next().expect("no frames")
    }

    fn locals(&mut self, frame: i64) -> Vec<types::Variable> {
        let id = self.client.scopes(frame).unwrap();
        let scopes: types::ScopesBody = types::parse(&self.response(id)).unwrap();
        let scope = scopes.scopes.first().expect("no scopes").clone();
        let id = self.client.variables(scope.variables_reference).unwrap();
        let vars: types::VariablesBody = types::parse(&self.response(id)).unwrap();
        vars.variables
    }

    fn stopped(&mut self) -> types::StoppedEvent {
        types::parse(&self.event("stopped")).unwrap()
    }

    fn finish(mut self) {
        let start = Instant::now();
        loop {
            if start.elapsed() > WAIT {
                panic!("the session did not end");
            }
            match self.next() {
                DapEvent::Event { event, .. } if event == "terminated" => break,
                DapEvent::Exited { .. } => break,
                _ => {}
            }
        }
        self.client.terminate(Duration::from_secs(2));
    }
}

fn cargo() -> Option<PathBuf> {
    discover::which("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

fn write_rust_project(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"dbgme\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(
        &main,
        "fn main() {\n\
         \x20   let numbers = vec![1, 2, 3];\n\
         \x20   let total: i32 = numbers.iter().sum();\n\
         \x20   let greeting = String::from(\"hello\");\n\
         \x20   println!(\"{greeting} {total}\");\n\
         \x20   let doubled = total * 2;\n\
         \x20   println!(\"doubled {doubled}\");\n\
         }\n",
    )
    .unwrap();
    main
}

#[test]
fn lldb_dap_breakpoint_locals_step_and_exit() {
    let Ok(adapter) = discover::find_lldb_dap() else {
        eprintln!("skipping: lldb-dap is not installed");
        return;
    };
    let Some(cargo) = cargo() else {
        eprintln!("skipping: cargo is not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let main = write_rust_project(&dir);
    // An inherited CARGO_TARGET_DIR would put the binary elsewhere.
    let status = std::process::Command::new(cargo)
        .args(["build", "-q"])
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success());
    let program = dir.join("target/debug/dbgme");
    let init_commands = discover::rust_sysroot(None).map(|s| discover::rust_lldb_init_commands(&s)).unwrap_or_default();

    let (client, events) = DapClient::spawn(&adapter).unwrap();
    let mut d = Driver { client, events, seen: Vec::new(), answered: Vec::new(), output: String::new() };
    let init = d.client.initialize("lldb-dap").unwrap();
    let caps: types::Capabilities = types::parse(&d.response(init)).unwrap();
    assert!(caps.supports_configuration_done_request);
    let args = launch::native(
        adapter.kind,
        &NativeLaunch { name: "dbgme".into(), program, cwd: Some(dir.clone()), init_commands, ..Default::default() },
    );
    let launch_id = d.client.launch(args).unwrap();
    d.event("initialized");
    let bp = d.client.set_breakpoints(&main, &[SourceBreakpoint { line: 6, ..Default::default() }]).unwrap();
    let bps: types::BreakpointsBody = types::parse(&d.response(bp)).unwrap();
    assert_eq!(bps.breakpoints.len(), 1);
    let done = d.client.configuration_done().unwrap();
    d.response(done);
    d.response(launch_id);

    let stopped = d.stopped();
    assert_eq!(stopped.reason, "breakpoint", "{stopped:?}");
    let thread = stopped.thread_id.unwrap();
    let frame = d.top_frame(thread);
    assert_eq!(frame.line, 6);
    assert!(frame.source.as_ref().and_then(|s| s.path.as_ref()).is_some_and(|p| p.ends_with("main.rs")));
    let locals = d.locals(frame.id);
    let value = |name: &str| locals.iter().find(|v| v.name == name).map(|v| v.value.clone());
    assert_eq!(value("total").as_deref(), Some("6"), "{locals:?}");
    let greeting = value("greeting").unwrap_or_default();
    assert!(greeting.contains("hello"), "pretty printers should show the String: {greeting}");
    let numbers = locals.iter().find(|v| v.name == "numbers").unwrap();
    assert!(numbers.variables_reference > 0, "a Vec has children");

    let eval = d.client.evaluate("total", Some(frame.id), "watch").unwrap();
    let eval: types::EvaluateBody = types::parse(&d.response(eval)).unwrap();
    assert_eq!(eval.result, "6");

    let next = d.client.next(thread).unwrap();
    d.response(next);
    let stopped = d.stopped();
    assert_eq!(stopped.reason, "step");
    assert_eq!(d.top_frame(thread).line, 7);

    let cont = d.client.continue_(thread).unwrap();
    d.response(cont);
    let exited: types::ExitedEvent = types::parse(&d.event("exited")).unwrap();
    assert_eq!(exited.exit_code, 0);
    d.finish();
}

#[test]
fn debugpy_breakpoint_locals_step_and_exit() {
    let Ok(adapter) = discover::find_debugpy(None) else {
        eprintln!("skipping: debugpy is not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("prog.py");
    std::fs::write(
        &script,
        "def main():\n    items = [1, 2, 3]\n    total = sum(items)\n    print('total', total)\n    doubled = total * 2\n    print('doubled', doubled)\n\nmain()\n",
    )
    .unwrap();
    // macOS temp dirs are symlinks; debugpy reports real paths.
    let script = script.canonicalize().unwrap();

    let (client, events) = DapClient::spawn(&adapter).unwrap();
    let mut d = Driver { client, events, seen: Vec::new(), answered: Vec::new(), output: String::new() };
    let init = d.client.initialize("debugpy").unwrap();
    d.response(init);
    let args = launch::python(&PythonLaunch {
        name: "prog".into(),
        program: Some(script.clone()),
        cwd: Some(tmp.path().to_path_buf()),
        just_my_code: true,
        ..Default::default()
    });
    let launch_id = d.client.launch(args).unwrap();
    d.event("initialized");
    let bp = d.client.set_breakpoints(&script, &[SourceBreakpoint { line: 4, ..Default::default() }]).unwrap();
    let bps: types::BreakpointsBody = types::parse(&d.response(bp)).unwrap();
    assert!(bps.breakpoints.first().is_some_and(|b| b.verified), "{bps:?}");
    let done = d.client.configuration_done().unwrap();
    d.response(done);
    d.response(launch_id);

    let stopped = d.stopped();
    assert_eq!(stopped.reason, "breakpoint");
    let thread = stopped.thread_id.unwrap();
    let frame = d.top_frame(thread);
    assert_eq!((frame.name.as_str(), frame.line), ("main", 4));
    let locals = d.locals(frame.id);
    assert_eq!(locals.iter().find(|v| v.name == "total").map(|v| v.value.as_str()), Some("6"), "{locals:?}");

    let next = d.client.next(thread).unwrap();
    d.response(next);
    assert_eq!(d.stopped().reason, "step");
    assert_eq!(d.top_frame(thread).line, 5);

    let cont = d.client.continue_(thread).unwrap();
    d.response(cont);
    let exited: types::ExitedEvent = types::parse(&d.event("exited")).unwrap();
    assert_eq!(exited.exit_code, 0);
    assert!(d.output.contains("doubled 12"), "program output arrives as output events: {:?}", d.output);
    d.finish();
}
