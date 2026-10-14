//! Run menu items of track CL:
//!
//! - **Attach to an Unstarted Process…** (`run.attachToAnUnstartedProcess`): wait for a process
//!   with a name (or an executable's name) to start and attach to it (lldb-dap's `waitFor`, or
//!   `process attach --name X --waitfor`).
//! - **Reset Frame** (`run.debuggingActions.resetFrame`): DAP `restartFrame` when the adapter
//!   has it, else LLDB's `thread return` (the frame is dropped and execution is back in its
//!   caller; side effects stay).
//! - **Manage Targets…** (`run.manageTargets`): in RustRover/CLion this opens the Run Targets
//!   list (where run configurations can run: the local machine, SSH, Docker). CodeCraft lists
//!   the same: the local machine, the configured SSH servers and the Docker images/containers
//!   run configurations use, with the configurations using each, and assigns a target to a user
//!   configuration.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::debug::DebugTarget;
use crate::params::{opt_str, req_usize};
use crate::run_targets::RunTarget;
use crate::{CommandError, Session};

/// The Attach to an Unstarted Process dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachWaitDialog {
    /// A process name or an executable path.
    pub process: String,
    pub error: Option<String>,
}

/// One row of Manage Targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRow {
    /// `local`, `ssh`, `docker`.
    pub kind: String,
    pub name: String,
    pub detail: String,
    /// The target as a run configuration stores it.
    pub target: RunTarget,
    /// Run configurations that run there.
    pub used_by: Vec<String>,
}

#[derive(Default)]
pub struct DebuggingState {
    pub attach_wait: Option<AttachWaitDialog>,
    /// Manage Targets… is open.
    pub targets_open: bool,
}

/// A good default for the dialog: the selected configuration's executable.
fn default_process(s: &Session) -> String {
    let Some(c) = s.run_configs.get(s.selected_run_config) else { return String::new() };
    match &c.command {
        Some(cmd) => {
            let start = crate::testing::cpp::test_args_start(cmd);
            let exe = if start > 0 { cmd.args.get(3).cloned().unwrap_or_default() } else { cmd.program.clone() };
            let name = std::path::Path::new(&exe).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(exe);
            if matches!(name.as_str(), "sh" | "bash" | "zsh") { String::new() } else { name }
        }
        None => match &c.invocation.target {
            Some(codecraft_cargo::TargetSelector::Bin(b)) => b.clone(),
            _ => String::new(),
        },
    }
}

fn attach_wait(s: &mut Session, p: &Value) -> CmdResult {
    let Some(process) = opt_str(p, "process") else {
        let process = default_process(s);
        s.cpp.debugging.attach_wait = Some(AttachWaitDialog { process, error: None });
        return ok();
    };
    let process = process.trim().to_string();
    if !codecraft_dap::launch::valid_wait_name(&process) {
        let err = format!("`{process}` is not a process name");
        if let Some(d) = &mut s.cpp.debugging.attach_wait {
            d.error = Some(err.clone());
        }
        return Err(CommandError::BadParams(err));
    }
    let name = std::path::Path::new(&process).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| process.clone());
    match crate::debug::start(s, DebugTarget::AttachWait { name: name.clone(), process: process.clone() }) {
        Ok(()) => {
            s.cpp.debugging.attach_wait = None;
            s.status = format!("Waiting for {name} to start…");
            Ok(json!({ "waitingFor": name }))
        }
        Err(e) => {
            if let Some(d) = &mut s.cpp.debugging.attach_wait {
                d.error = Some(e.to_string());
            }
            Err(e)
        }
    }
}

/// The run targets: local, SSH servers, Docker images/containers in use.
pub fn target_rows(s: &mut Session) -> Vec<TargetRow> {
    let mut rows = vec![TargetRow {
        kind: "local".into(),
        name: "Local Machine".into(),
        detail: std::env::consts::OS.to_string(),
        target: RunTarget::Local,
        used_by: Vec::new(),
    }];
    let servers: Vec<(String, String)> = s
        .remote
        .servers()
        .iter()
        .map(|sv| {
            let user = sv.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
            let port = sv.port.map(|p| format!(":{p}")).unwrap_or_default();
            (sv.name.clone(), format!("{user}{}{port}", sv.host))
        })
        .collect();
    for (name, detail) in servers {
        rows.push(TargetRow {
            kind: "ssh".into(),
            name: name.clone(),
            detail,
            target: RunTarget::Ssh(crate::ssh_target::SshTarget { server: name, ..Default::default() }),
            used_by: Vec::new(),
        });
    }
    // Targets the configurations use (Docker images and containers, SSH servers not listed).
    let configs: Vec<(String, RunTarget)> = s.run_configs.iter().map(|c| (c.name.clone(), c.target.clone())).collect();
    for (config, target) in configs {
        let (kind, name, detail) = match &target {
            RunTarget::Local => ("local", "Local Machine".to_string(), String::new()),
            RunTarget::Ssh(t) => ("ssh", t.server.clone(), t.remote_dir.clone()),
            RunTarget::Docker(t) => (
                "docker",
                if t.container.is_empty() { t.image.clone() } else { t.container.clone() },
                if t.container.is_empty() { "image".into() } else { "container".into() },
            ),
        };
        match rows.iter_mut().find(|r| r.kind == kind && r.name == name) {
            Some(r) => r.used_by.push(config),
            None => rows.push(TargetRow { kind: kind.into(), name, detail, target, used_by: vec![config] }),
        }
    }
    rows
}

fn assign(s: &mut Session, p: &Value) -> CmdResult {
    let index = req_usize(p, "config")?;
    let target: RunTarget = match p.get("target") {
        None | Some(Value::Null) => RunTarget::Local,
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| CommandError::BadParams(format!("bad `target`: {e}")))?,
    };
    let c = s
        .run_store
        .user
        .get_mut(index)
        .ok_or_else(|| CommandError::BadParams(format!("no user run configuration {index}")))?;
    let previous = std::mem::replace(&mut c.target, target.clone());
    let name = c.name.clone();
    if let Err(e) = s.run_store.save() {
        if let Some(c) = s.run_store.user.get_mut(index) {
            c.target = previous;
        }
        return Err(CommandError::Failed(e));
    }
    crate::run_config_store::rebuild(s);
    Ok(json!({ "config": name, "target": target.label() }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "run.attachToAnUnstartedProcess",
            "Attach to an Unstarted Process…",
            None,
            "{process?: string} — wait for a process with this name (or this executable's name) to start and \
             attach the debugger to it; without `process` the dialog opens",
            Context::Global,
            always,
            attach_wait,
        ),
        crate::commands::simple("debug.attachWait.cancel", "Close Attach to an Unstarted Process", always, |s, _| {
            s.cpp.debugging.attach_wait = None;
            ok()
        }),
        spec(
            "run.debuggingActions.resetFrame",
            "Reset Frame",
            None,
            "Drop the selected frame and return to its caller (DAP restartFrame, else LLDB `thread return`; side \
             effects are not undone)",
            Context::Global,
            |s| if s.debug.is_stopped() { Ok(()) } else { Err("the program is not paused".into()) },
            |s, _| {
                let how = crate::debug::reset_frame(s)?;
                Ok(json!({ "method": how }))
            },
        ),
        spec(
            "run.manageTargets",
            "Manage Targets…",
            None,
            "The run targets (local machine, SSH servers, Docker images and containers) with the run \
             configurations that use each; opens the Manage Targets dialog",
            Context::Global,
            always,
            |s, _| {
                s.cpp.debugging.targets_open = true;
                Ok(json!(target_rows(s)))
            },
        ),
        crate::commands::simple("run.manageTargets.close", "Close Manage Targets", always, |s, _| {
            s.cpp.debugging.targets_open = false;
            ok()
        }),
        spec(
            "run.manageTargets.assign",
            "Assign Run Target",
            None,
            "{config: int (user configuration index), target?: {kind: local|docker|ssh, …}} — where a user run \
             configuration runs",
            Context::Global,
            crate::commands::has_project,
            assign,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_and_validation() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.run_configs.push(crate::runs::RunConfig::process(
            "app",
            crate::runs::ProcessCommand::new("/b/server", &["--port", "1"], "/b"),
        ));
        s.execute("run.attachToAnUnstartedProcess", &Value::Null).unwrap();
        assert_eq!(s.cpp.debugging.attach_wait.as_ref().map(|d| d.process.as_str()), Some("server"));
        let err = s.execute("run.attachToAnUnstartedProcess", &json!({ "process": "a\nb" })).unwrap_err();
        assert!(matches!(err, CommandError::BadParams(_)));
        assert!(s.cpp.debugging.attach_wait.as_ref().unwrap().error.is_some());
        s.execute("debug.attachWait.cancel", &Value::Null).unwrap();
        assert!(s.cpp.debugging.attach_wait.is_none());
        // Reset Frame needs a paused program.
        assert!(s.execute("run.debuggingActions.resetFrame", &Value::Null).is_err());
    }

    #[test]
    fn manage_targets_lists_local_and_used_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(tmp.path());
        std::fs::write(tmp.path().join("ssh_config"), "Host build-box\n  HostName 10.0.0.5\n  User dev\n").unwrap();
        let mut docker = crate::runs::RunConfig::process("in docker", crate::runs::ProcessCommand::new("app", &[], "/p"));
        docker.target = RunTarget::Docker(crate::docker_target::DockerTarget { image: "gcc:14".into(), ..Default::default() });
        s.run_configs.push(docker);
        s.run_configs.push(crate::runs::RunConfig::process("local", crate::runs::ProcessCommand::new("app", &[], "/p")));
        let rows = s.execute("run.manageTargets", &Value::Null).unwrap();
        let rows: Vec<TargetRow> = serde_json::from_value(rows).unwrap();
        assert!(s.cpp.debugging.targets_open);
        assert_eq!(rows[0].kind, "local");
        assert_eq!(rows[0].used_by, vec!["local"]);
        let ssh = rows.iter().find(|r| r.kind == "ssh").expect("the ssh_config host");
        assert_eq!((ssh.name.as_str(), ssh.detail.as_str()), ("build-box", "dev@10.0.0.5"));
        let d = rows.iter().find(|r| r.kind == "docker").unwrap();
        assert_eq!((d.name.as_str(), d.used_by.clone()), ("gcc:14", vec!["in docker".to_string()]));
        s.execute("run.manageTargets.close", &Value::Null).unwrap();
        assert!(!s.cpp.debugging.targets_open);
    }

    // ------------------------------------------------------------ Reset Frame, scripted adapter

    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use codecraft_dap::{AdapterKind, Capabilities, DapClient};

    #[derive(Default)]
    struct World {
        popped: bool,
        restarts: u32,
        requests: Vec<(String, Value)>,
    }

    fn write_message(w: &Arc<Mutex<std::io::PipeWriter>>, message: &Value) {
        let body = serde_json::to_vec(message).unwrap();
        let mut w = w.lock().unwrap();
        let _ = w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        let _ = w.write_all(&body);
        let _ = w.flush();
    }

    fn read_message(r: &mut BufReader<std::io::PipeReader>) -> Option<Value> {
        let mut len = None;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length: ") {
                len = v.parse::<usize>().ok();
            }
        }
        let mut body = vec![0; len?];
        r.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    fn stopped(reason: &str) -> Value {
        json!({ "seq": 0, "type": "event", "event": "stopped", "body": { "reason": reason, "threadId": 1 } })
    }

    fn answer(world: &Mutex<World>, command: &str, args: &Value, events: &mut Vec<Value>) -> Result<Value, String> {
        let mut w = world.lock().unwrap();
        w.requests.push((command.to_owned(), args.clone()));
        match command {
            "threads" => Ok(json!({ "threads": [{ "id": 1, "name": "main" }] })),
            "stackTrace" => {
                let main = json!({ "id": 101, "name": "main", "line": 8, "column": 1, "source": { "path": "/p/main.c" } });
                let inner = json!({ "id": 100, "name": "inner", "line": 3, "column": 1, "source": { "path": "/p/main.c" } });
                Ok(json!({ "stackFrames": if w.popped { vec![main] } else { vec![inner, main] } }))
            }
            "scopes" => Ok(json!({ "scopes": [] })),
            "evaluate" if args["expression"] == "thread return" => {
                w.popped = true;
                Ok(json!({ "result": "* thread #1, stop reason = step\n  frame #0: main at main.c:8" }))
            }
            "restartFrame" => {
                w.restarts += 1;
                events.push(stopped("restart"));
                Ok(Value::Null)
            }
            _ => Err(format!("unsupported {command}")),
        }
    }

    fn fake(kind: AdapterKind, caps: Capabilities) -> (Session, Arc<Mutex<World>>) {
        let (client_reader, adapter_writer) = std::io::pipe().unwrap();
        let (adapter_reader, client_writer) = std::io::pipe().unwrap();
        let (client, events) = DapClient::connect("fake", client_reader, client_writer).unwrap();
        let world = Arc::new(Mutex::new(World::default()));
        let writer = Arc::new(Mutex::new(adapter_writer));
        let (w2, writer2) = (Arc::clone(&world), Arc::clone(&writer));
        std::thread::spawn(move || {
            let mut reader = BufReader::new(adapter_reader);
            while let Some(req) = read_message(&mut reader) {
                let command = req["command"].as_str().unwrap_or_default().to_owned();
                let mut queued = Vec::new();
                let response = match answer(&w2, &command, &req["arguments"], &mut queued) {
                    Ok(body) => json!({ "seq": 0, "type": "response", "request_seq": req["seq"], "command": command,
                                        "success": true, "body": body }),
                    Err(e) => json!({ "seq": 0, "type": "response", "request_seq": req["seq"], "command": command,
                                      "success": false, "message": e }),
                };
                write_message(&writer2, &response);
                for e in queued {
                    write_message(&writer2, &e);
                }
            }
        });
        let mut s = Session::new();
        s.lsp_enabled = false;
        crate::debug::install_fake(&mut s, client, events, kind, caps);
        write_message(&writer, &stopped("breakpoint"));
        pump_frames(&mut s, 2);
        (s, world)
    }

    fn pump_frames(s: &mut Session, n: usize) {
        let start = Instant::now();
        loop {
            let ok = s.debug.is_stopped()
                && !s.debug.settling()
                && s.debug.session.as_ref().is_some_and(|x| x.frames.len() == n);
            if ok {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(120), "timed out waiting for {n} frames");
            s.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn requests(world: &Mutex<World>, command: &str) -> Vec<Value> {
        world.lock().unwrap().requests.iter().filter(|(c, _)| c == command).map(|(_, a)| a.clone()).collect()
    }

    #[test]
    fn reset_frame_with_lldb_thread_return() {
        let (mut s, world) = fake(AdapterKind::LldbDap, Capabilities::default());
        let r = s.execute("run.debuggingActions.resetFrame", &Value::Null).unwrap();
        assert_eq!(r["method"], "thread return");
        pump_frames(&mut s, 1);
        let eval = requests(&world, "evaluate");
        let last = eval.last().unwrap();
        assert_eq!((&last["expression"], &last["frameId"], &last["context"]), (&json!("thread return"), &json!(100), &json!("repl")));
        assert_eq!(s.debug.session.as_ref().unwrap().frames[0].name, "main");
        // The outermost frame cannot be reset.
        let err = s.execute("run.debuggingActions.resetFrame", &Value::Null).unwrap_err();
        assert!(err.to_string().contains("outermost"), "{err}");
    }

    #[test]
    fn reset_frame_with_restart_frame_and_unsupported_adapters() {
        let caps = Capabilities { supports_restart_frame: true, ..Capabilities::default() };
        let (mut s, world) = fake(AdapterKind::LldbDap, caps);
        let r = s.execute("run.debuggingActions.resetFrame", &Value::Null).unwrap();
        assert_eq!(r["method"], "restartFrame");
        pump_frames(&mut s, 2);
        assert_eq!(world.lock().unwrap().restarts, 1);
        assert_eq!(requests(&world, "restartFrame")[0]["frameId"], 100);
        assert!(requests(&world, "evaluate").is_empty());

        let (mut s, _) = fake(AdapterKind::Other, Capabilities::default());
        let err = s.execute("run.debuggingActions.resetFrame", &Value::Null).unwrap_err();
        assert!(err.to_string().contains("cannot reset frames"), "{err}");
    }
}
