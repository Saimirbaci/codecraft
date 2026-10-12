//! The ACP client against a fake agent (`fixtures/fake_acp.py`): handshake, updates, permission
//! requests, fs reads and writes, follow-up prompts. Skips without python3.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use serde_json::json;

use codecraft_agent::acp::{self, HostRequest, PermissionChoice, PermissionKind};
use codecraft_agent::{AgentEvent, Backend, ChangeKind, cli};

fn fake() -> Option<PathBuf> {
    let ok = std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: python3 not found");
        return None;
    }
    Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_acp.py"))
}

/// Serve host requests until the turn ends; returns the turn's events.
fn turn(
    events: &Receiver<AgentEvent>,
    host: &Receiver<HostRequest>,
    dir: &Path,
    choice: PermissionChoice,
    written: &mut Vec<(PathBuf, String)>,
) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "turn did not end: {out:?}");
        while let Ok(req) = host.try_recv() {
            match req {
                HostRequest::ReadTextFile { path, responder, .. } => {
                    assert_eq!(path, dir.join("hello.txt"));
                    responder.result(json!({ "content": std::fs::read_to_string(&path).unwrap() }));
                }
                HostRequest::WriteTextFile { path, content, responder } => {
                    std::fs::write(&path, &content).unwrap();
                    written.push((path, content));
                    responder.result(json!({}));
                }
                HostRequest::Permission { kind, options, responder, .. } => {
                    out.push(AgentEvent::Log { line: format!("permission: {kind:?}") });
                    responder.permission(&options, choice);
                }
            }
        }
        if let Ok(ev) = events.recv_timeout(Duration::from_millis(20)) {
            let end = matches!(ev, AgentEvent::Result { .. } | AgentEvent::Exited { .. });
            out.push(ev);
            if end {
                return out;
            }
        }
    }
}

#[test]
fn fake_acp_agent_roundtrip() {
    let Some(script) = fake() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("hello.txt"), "hello\n").unwrap();
    let (etx, erx) = unbounded();
    let (htx, hrx) = unbounded();
    let cmd = cli::acp_command(Backend::Gemini, Path::new("python3"), &[script.display().to_string()], &dir, true);
    let client = acp::start(cmd, &dir, "shout".into(), etx, htx).unwrap();
    let mut written = Vec::new();

    let ev = turn(&erx, &hrx, &dir, PermissionChoice::AllowOnce, &mut written);
    assert!(ev.contains(&AgentEvent::Session { id: "fake-session".into() }), "{ev:?}");
    assert!(ev.contains(&AgentEvent::TextDelta { text: "Working on it.".into() }));
    assert!(ev.iter().any(|e| matches!(e, AgentEvent::Plan { entries } if entries.len() == 1)));
    let expected_kind = PermissionKind::FileEdit {
        path: dir.join("hello.txt"),
        old_text: Some("hello\n".into()),
        new_text: "HELLO\n".into(),
    };
    assert!(ev.contains(&AgentEvent::Log { line: format!("permission: {expected_kind:?}") }), "{ev:?}");
    assert!(ev.contains(&AgentEvent::ToolResult { id: "c1".into(), ok: true, output: "completed".into() }));
    assert!(ev.contains(&AgentEvent::FileChanged { path: dir.join("hello.txt"), kind: ChangeKind::Modified }));
    assert!(matches!(ev.last(), Some(AgentEvent::Result { is_error: false, .. })));
    assert_eq!(written, vec![(dir.join("hello.txt"), "HELLO\n".to_string())]);

    // Follow-up on the same session: denied this time.
    std::fs::write(dir.join("hello.txt"), "again\n").unwrap();
    assert!(client.prompt("shout again"));
    let ev = turn(&erx, &hrx, &dir, PermissionChoice::Deny, &mut written);
    assert!(ev.contains(&AgentEvent::ToolResult { id: "c1".into(), ok: false, output: "failed".into() }));
    assert!(!ev.iter().any(|e| matches!(e, AgentEvent::FileChanged { .. })));
    assert_eq!(written.len(), 1);

    // A command permission.
    assert!(client.prompt("run the tests"));
    let ev = turn(&erx, &hrx, &dir, PermissionChoice::AllowOnce, &mut written);
    let cmd_kind = PermissionKind::Command { command: "cargo test".into() };
    assert!(ev.contains(&AgentEvent::Log { line: format!("permission: {cmd_kind:?}") }), "{ev:?}");
    assert!(ev.contains(&AgentEvent::ToolResult { id: "x1".into(), ok: true, output: String::new() }));

    client.shutdown();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match erx.recv_timeout(Duration::from_millis(50)) {
            Ok(AgentEvent::Exited { .. }) => break,
            _ => assert!(Instant::now() < deadline, "agent did not exit"),
        }
    }
    assert!(!client.process().is_running());
    assert!(!client.prompt("too late"));
}

#[test]
fn agent_that_is_not_acp_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let (etx, erx) = unbounded();
    let (htx, _hrx) = unbounded();
    let cmd = cli::acp_command(
        Backend::Gemini,
        Path::new("sh"),
        &["-c".into(), "echo not-json; exit 3".into()],
        tmp.path(),
        true,
    );
    let _client = acp::start(cmd, tmp.path(), "hi".into(), etx, htx).unwrap();
    let (mut saw_error, mut exit) = (false, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(saw_error && exit.is_some()) && Instant::now() < deadline {
        match erx.recv_timeout(Duration::from_millis(50)) {
            Ok(AgentEvent::Error { .. }) => saw_error = true,
            Ok(AgentEvent::Exited { code, .. }) => exit = Some(code),
            _ => {}
        }
    }
    assert!(saw_error, "the failed handshake is reported");
    assert_eq!(exit, Some(Some(3)));
}
