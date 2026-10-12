//! Spawned servers: process exit codes, the stderr tail, kill on timeout and on drop. Shell
//! one-liners stand in for a language server (Unix only).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::thread;
use std::time::{Duration, Instant};

use codecraft_lsp::{LspClient, LspError, ServerConfig, ServerEvent};
use common::*;
use crossbeam_channel::Receiver;

fn config(command: &str, args: &[&str]) -> ServerConfig {
    ServerConfig {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        ..ServerConfig::rust_analyzer(command.into(), std::env::temp_dir())
    }
}

fn sh(script: &str) -> ServerConfig {
    config("sh", &["-c", script])
}

/// Collects `Error` events until `Exited`; returns them with the exit code.
fn until_exit(events: &Receiver<ServerEvent>) -> (Vec<String>, Option<i32>) {
    let mut errors = Vec::new();
    loop {
        match next_event(events) {
            ServerEvent::Error(error) => errors.push(error),
            ServerEvent::Exited { code } => return (errors, code),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn spawning_a_missing_binary_fails() {
    let result = LspClient::spawn(config("/definitely/not/a/language-server", &[]));
    match result {
        Err(LspError::Spawn { command, .. }) => assert!(command.contains("language-server")),
        Err(other) => panic!("expected a spawn error, got {other:?}"),
        Ok(_) => panic!("expected a spawn error"),
    }
}

#[test]
fn crash_reports_exit_code_and_stderr() {
    let (client, events) = LspClient::spawn(sh("echo first >&2; echo boom >&2; exit 3")).unwrap();
    let (errors, code) = until_exit(&events);
    assert_eq!(code, Some(3));
    assert!(
        errors.iter().any(|error| error.contains("exited unexpectedly") && error.contains("first\nboom")),
        "{errors:?}"
    );
    assert_no_event(&events);
    assert_eq!(client.stderr_tail(), ["first", "boom"]);
    assert!(matches!(client.hover(&file(), 0, 0), Err(LspError::NotRunning)));
}

#[test]
fn handshake_with_a_spawned_server() {
    // Answers `initialize` (always id 1) blindly, then reads until stdin closes.
    let script = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"hoverProvider":true}}}'
printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
cat > /dev/null"#;
    let (client, events) = LspClient::spawn(sh(script)).unwrap();
    match next_event(&events) {
        ServerEvent::Initialized(caps) => assert!(caps.hover_provider.is_some()),
        other => panic!("expected Initialized, got {other:?}"),
    }
    assert!(client.is_initialized());
    // It never answers `shutdown`, which uses up the timeout; it is then stopped either by
    // `exit` closing its stdin or by the kill right after.
    let started = Instant::now();
    client.shutdown(Duration::from_millis(300)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let (errors, _) = until_exit(&events);
    assert!(errors.is_empty(), "a requested shutdown is not an error: {errors:?}");
    assert_no_event(&events);
}

#[test]
fn graceful_exit_when_stdin_closes() {
    // Not initialized, so shutdown sends only `exit` and closes stdin, which ends `cat`.
    let (client, events) = LspClient::spawn(sh("cat > /dev/null")).unwrap();
    client.shutdown(WAIT).unwrap();
    let (errors, code) = until_exit(&events);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(code, Some(0));
    assert_no_event(&events);
}

#[test]
fn unresponsive_server_is_killed_after_the_timeout() {
    let (client, events) = LspClient::spawn(config("sleep", &["30"])).unwrap();
    let started = Instant::now();
    client.shutdown(Duration::from_millis(300)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let (errors, code) = until_exit(&events);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(code, None, "killed by a signal");
    assert_no_event(&events);
    client.shutdown(Duration::from_millis(300)).unwrap();
}

#[test]
fn dropping_the_client_kills_the_server() {
    let (client, events) = LspClient::spawn(config("sleep", &["30"])).unwrap();
    let started = Instant::now();
    drop(client);
    let (errors, code) = until_exit(&events);
    assert!(errors.is_empty(), "dropping is a requested stop: {errors:?}");
    assert_eq!(code, None);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn server_that_closes_its_output_but_keeps_running_is_killed() {
    let (_client, events) = LspClient::spawn(sh("exec 1>&-; exec sleep 30")).unwrap();
    let started = Instant::now();
    let (errors, code) = until_exit(&events);
    assert_eq!(code, None);
    assert!(errors.iter().any(|error| error.contains("exited unexpectedly")), "{errors:?}");
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn stderr_tail_keeps_the_last_50_lines() {
    let script = "i=1; while [ $i -le 80 ]; do echo line$i >&2; i=$((i+1)); done; exec sleep 30";
    let (client, events) = LspClient::spawn(sh(script)).unwrap();
    let started = Instant::now();
    loop {
        let tail = client.stderr_tail();
        if tail.last().map(String::as_str) == Some("line80") {
            assert_eq!(tail.len(), 50);
            assert_eq!(tail[0], "line31");
            break;
        }
        assert!(started.elapsed() < WAIT, "stderr never arrived: {tail:?}");
        thread::sleep(Duration::from_millis(10));
    }
    client.shutdown(Duration::from_millis(200)).unwrap();
    let (_, code) = until_exit(&events);
    assert_eq!(code, None);
}
