//! Agent child processes: spawned in their own process group, read line by line on worker
//! threads, reaped by a supervisor that sends [`AgentEvent::Exited`] last, and killed as a whole
//! group (the agent's own children too) through the external `kill` command (no `unsafe`).

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::cli::{CliRequest, build_command};
use crate::event::AgentEvent;
use crate::parse::StreamParser;
use crate::{AgentError, Result};

const POLL: Duration = Duration::from_millis(25);
/// After the agent exits, how long its pipes may stay open (held by a grandchild) before the run
/// is declared finished anyway.
const PIPE_GRACE: Duration = Duration::from_secs(2);
/// Time between `SIGTERM` and `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_millis(1500);
/// Longer lines are cut (stream-json lines carry whole file contents).
pub const MAX_LINE_BYTES: usize = 16 << 20;
/// At most this many stderr lines become log events per process.
const MAX_STDERR_LINES: usize = 500;

#[derive(Default)]
struct State {
    /// The child has been waited for; its pid may be reused from here on. Written under the
    /// child lock.
    reaped: AtomicBool,
    /// `Exited` was sent.
    finished: AtomicBool,
    kill_requested: AtomicBool,
}

/// A running agent process. Cloning shares the same process.
#[derive(Clone)]
pub struct AgentProcess {
    pid: u32,
    child: Arc<Mutex<Child>>,
    state: Arc<State>,
}

impl std::fmt::Debug for AgentProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProcess").field("pid", &self.pid).field("running", &self.is_running()).finish()
    }
}

fn lock(child: &Mutex<Child>) -> MutexGuard<'_, Child> {
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    thread::Builder::new().name(format!("codecraft-agent-{name}")).spawn(body).map(drop)
}

/// The pieces of a freshly spawned process.
pub(crate) struct Spawned {
    pub process: AgentProcess,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    /// Signalled by each reader when its pipe closes.
    pub done_tx: Sender<()>,
}

/// Spawn `cmd` in a new process group with piped stdout/stderr (and stdin if asked). Starts the
/// stderr reader (log events) and the supervisor; the caller reads stdout and must send one
/// `done` when it reaches EOF.
pub(crate) fn spawn(mut cmd: Command, with_stdin: bool, events: Sender<AgentEvent>) -> Result<Spawned> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if with_stdin { Stdio::piped() } else { Stdio::null() });
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().map_err(|source| AgentError::Spawn { program: program.clone(), source })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let process =
        AgentProcess { pid: child.id(), child: Arc::new(Mutex::new(child)), state: Arc::new(State::default()) };
    log::debug!("agent pid {} started: {program}", process.pid);

    let (done_tx, done_rx) = unbounded::<()>();
    let mut readers = 1; // stdout, read by the caller
    if let Some(stderr) = stderr {
        readers += 1;
        let (tx, done) = (events.clone(), done_tx.clone());
        let started = spawn_thread("stderr", move || {
            let mut n = 0usize;
            read_lines(stderr, |line| {
                n += 1;
                if n > MAX_STDERR_LINES {
                    return true;
                }
                let line = crate::truncate(line.trim_end(), 2000);
                line.is_empty() || tx.send(AgentEvent::Log { line }).is_ok()
            });
            let _ = done.send(());
        });
        if let Err(e) = started {
            process.abort();
            return Err(AgentError::Io(e));
        }
    }
    let (p, ev) = (process.clone(), events);
    if let Err(e) = spawn_thread("wait", move || supervise(&p, &done_rx, readers, &ev)) {
        process.abort();
        return Err(AgentError::Io(e));
    }
    Ok(Spawned { process, stdin, stdout, done_tx })
}

/// Poll for exit, give the readers a moment to drain, then send `Exited`.
fn supervise(p: &AgentProcess, done_rx: &Receiver<()>, readers: usize, events: &Sender<AgentEvent>) {
    let status = loop {
        {
            let mut child = lock(&p.child);
            match child.try_wait() {
                Ok(Some(status)) => {
                    p.state.reaped.store(true, Ordering::SeqCst);
                    break Some(status);
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("agent pid {}: wait failed: {e}", p.pid);
                    p.state.reaped.store(true, Ordering::SeqCst);
                    break None;
                }
            }
        }
        thread::sleep(POLL);
    };
    let deadline = Instant::now() + PIPE_GRACE;
    let mut closed = 0;
    while closed < readers {
        let left = deadline.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(left) {
            Ok(()) => closed += 1,
            Err(_) => break,
        }
    }
    let code = status.and_then(|s| s.code());
    let success = status.is_some_and(|s| s.success());
    let _ = events.send(AgentEvent::Exited { code, success });
    p.state.finished.store(true, Ordering::SeqCst);
}

/// Read `r` line by line (lossy UTF-8, lines capped at [`MAX_LINE_BYTES`]) until EOF, an error, or
/// `f` returns false.
pub(crate) fn read_lines(r: impl Read, mut f: impl FnMut(String) -> bool) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX);
        match (&mut reader).take(limit).read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) => {
                log::debug!("agent pipe read: {e}");
                return;
            }
        }
        if buf.len() >= MAX_LINE_BYTES && buf.last() != Some(&b'\n') {
            // Skip the rest of an oversized line.
            let mut sink = Vec::new();
            loop {
                sink.clear();
                match (&mut reader).take(limit).read_until(b'\n', &mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if sink.last() == Some(&b'\n') => break,
                    Ok(_) => {}
                }
            }
        }
        let line = String::from_utf8_lossy(&buf);
        if !f(line.trim_end_matches(['\n', '\r']).to_string()) {
            return;
        }
    }
}

/// Run an agent CLI headless: stdout lines go through the backend's parser.
pub fn spawn_cli(req: &CliRequest, events: Sender<AgentEvent>) -> Result<AgentProcess> {
    let cmd = build_command(req);
    let sp = spawn(cmd, false, events.clone())?;
    let mut parser = StreamParser::new(req.backend, &req.cwd);
    let done = sp.done_tx;
    if let Some(stdout) = sp.stdout {
        let started = spawn_thread("stdout", move || {
            read_lines(stdout, |line| parser.line(&line).into_iter().all(|ev| events.send(ev).is_ok()));
            let _ = done.send(());
        });
        if let Err(e) = started {
            sp.process.abort();
            return Err(AgentError::Io(e));
        }
    } else {
        let _ = done.send(());
    }
    Ok(sp.process)
}

impl AgentProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Not yet reaped.
    pub fn is_running(&self) -> bool {
        !self.state.reaped.load(Ordering::SeqCst)
    }

    /// `Exited` was sent.
    pub fn is_finished(&self) -> bool {
        self.state.finished.load(Ordering::SeqCst)
    }

    /// `SIGTERM` to the whole group, `SIGKILL` after a grace period (on a background thread).
    /// Asking again kills at once.
    pub fn kill(&self) {
        if self.is_finished() {
            return;
        }
        if self.state.kill_requested.swap(true, Ordering::SeqCst) {
            self.force_kill();
            return;
        }
        let signalled = {
            // Holding the lock keeps the leader unreaped, so its pid (the group id) can't be reused.
            let _guard = lock(&self.child);
            !self.state.reaped.load(Ordering::SeqCst) && signal_group(self.pid, "TERM")
        };
        if !signalled {
            self.force_kill();
            return;
        }
        let p = self.clone();
        let escalate = spawn_thread("kill", move || {
            let deadline = Instant::now() + TERM_GRACE;
            while Instant::now() < deadline {
                if p.is_finished() || !p.is_running() {
                    break;
                }
                thread::sleep(POLL);
            }
            p.force_kill();
        });
        if escalate.is_err() {
            self.force_kill();
        }
    }

    /// Terminate and wait (up to `grace`, then `SIGKILL`) — for shutdown, when no background
    /// thread may outlive the app.
    pub fn terminate_blocking(&self, grace: Duration) {
        if self.is_finished() {
            return;
        }
        {
            let _guard = lock(&self.child);
            if !self.state.reaped.load(Ordering::SeqCst) {
                self.state.kill_requested.store(true, Ordering::SeqCst);
                signal_group(self.pid, "TERM");
            }
        }
        let deadline = Instant::now() + grace;
        while self.is_running() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        self.force_kill();
    }

    fn force_kill(&self) {
        let mut child = lock(&self.child);
        // Even with the leader reaped, members of its group may linger (the supervisor sets
        // `finished` only after the pipes close or the grace period ends).
        if !self.is_finished() {
            signal_group(self.pid, "KILL");
        }
        if !self.state.reaped.load(Ordering::SeqCst) {
            let _ = child.kill();
        }
    }

    /// Kill and reap synchronously (spawn failed half-way).
    fn abort(&self) {
        self.force_kill();
        let _ = lock(&self.child).wait();
        self.state.reaped.store(true, Ordering::SeqCst);
        self.state.finished.store(true, Ordering::SeqCst);
    }
}

/// `kill -<signal> -<pgid>`. Returns whether `kill` could be run.
fn signal_group(pgid: u32, signal: &str) -> bool {
    #[cfg(unix)]
    {
        // `kill -SIG -0` would signal our own process group.
        if pgid == 0 {
            return false;
        }
        match Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(_) => true,
            Err(e) => {
                log::warn!("cannot run `kill`: {e}");
                false
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, signal);
        false
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use std::path::PathBuf;

    fn collect(rx: &Receiver<AgentEvent>, timeout: Duration) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let deadline = Instant::now() + timeout;
        while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            let last = matches!(ev, AgentEvent::Exited { .. });
            out.push(ev);
            if last {
                break;
            }
        }
        out
    }

    fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn req(program: PathBuf, cwd: &std::path::Path) -> CliRequest {
        CliRequest {
            backend: Backend::Claude,
            program,
            prompt: "hi".into(),
            cwd: cwd.to_path_buf(),
            bypass: true,
            resume: None,
            continue_last: false,
            scrub_api_keys: true,
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn streams_then_exits_last() {
        let tmp = tempfile::tempdir().unwrap();
        let p = script(
            tmp.path(),
            "claude",
            r#"echo '{"type":"system","subtype":"init","session_id":"s1"}'
echo "warning on stderr" >&2
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":3}'
exit 0
"#,
        );
        let (tx, rx) = unbounded();
        let proc_ = spawn_cli(&req(p, tmp.path()), tx).unwrap();
        let ev = collect(&rx, Duration::from_secs(10));
        // stdout and stderr are read on separate threads: only their own orders are fixed.
        assert!(ev.contains(&AgentEvent::Session { id: "s1".into() }));
        assert!(ev.contains(&AgentEvent::Log { line: "warning on stderr".into() }));
        assert_eq!(ev.last(), Some(&AgentEvent::Exited { code: Some(0), success: true }));
        assert!(!proc_.is_running());
    }

    #[test]
    fn kill_reaches_the_whole_group() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("child.pid");
        let p = script(
            tmp.path(),
            "claude",
            &format!("sleep 30 &\necho $! > '{}'\necho started\nwait\n", marker.display()),
        );
        let (tx, rx) = unbounded();
        let proc_ = spawn_cli(&req(p, tmp.path()), tx).unwrap();
        // Wait for the grandchild to start.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let child_pid = std::fs::read_to_string(&marker).unwrap().trim().to_string();
        proc_.kill();
        let ev = collect(&rx, Duration::from_secs(10));
        assert!(matches!(ev.last(), Some(AgentEvent::Exited { success: false, .. })));
        // The background `sleep` is gone too.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let alive =
                Command::new("kill").args(["-0", &child_pid]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "grandchild {child_pid} survived");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn missing_program_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, _rx) = unbounded();
        assert!(spawn_cli(&req(tmp.path().join("nope"), tmp.path()), tx).is_err());
    }

    #[test]
    fn long_lines_are_capped() {
        let data = vec![b'x'; MAX_LINE_BYTES + 10];
        let mut input = data.clone();
        input.extend_from_slice(b"\nnext\n");
        let mut lines = Vec::new();
        read_lines(&input[..], |l| {
            lines.push(l.len());
            true
        });
        assert_eq!(lines, vec![MAX_LINE_BYTES, 4]);
    }
}
