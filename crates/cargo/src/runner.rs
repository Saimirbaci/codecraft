//! Process runner: spawn a program with piped output, stream [`RunEvent`]s, reap it, and kill its
//! whole process tree on request.
//!
//! Each run has one reader thread per output pipe (split into lines, parse, send) and a supervisor
//! thread that forwards the readers' events in arrival order, polls the child for exit and sends
//! the final `Exited`. Every event goes through the supervisor, which is what makes `Exited` last.

use std::io::{self, ErrorKind, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use crate::{CargoError, CargoMessage, Result, RunEvent, parse_message, parse_test_line, shell_quote};

/// How often the supervisor polls the child for exit (and the kill escalation for completion).
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// After the child exits, how long its output pipes may stay open *and silent* before the run is
/// declared finished anyway. Only reached when a grandchild inherited the pipes and outlives the
/// child (a daemon started by the program); normally the pipes close with the child.
const ORPHANED_PIPE_GRACE: Duration = Duration::from_secs(2);
/// Time between `SIGTERM` and `SIGKILL` when killing.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(1500);
/// Longer lines are split; bounds memory for output without newlines.
const MAX_LINE_BYTES: usize = 4 << 20;
const READ_CHUNK_BYTES: usize = 16 << 10;

/// The running process behind a [`crate::RunHandle`].
pub(crate) struct Process {
    pid: u32,
    child: Arc<Mutex<Child>>,
    state: Arc<RunState>,
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process").field("pid", &self.pid).field("running", &self.is_running()).finish()
    }
}

#[derive(Default)]
struct RunState {
    /// The supervisor has collected the exit status: the pid may be reused from here on. Only
    /// written while holding the child lock, so a holder of the lock can trust it.
    reaped: AtomicBool,
    /// `Exited` has been sent.
    finished: AtomicBool,
    kill_requested: AtomicBool,
}

fn lock(child: &Mutex<Child>) -> MutexGuard<'_, Child> {
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn spawn(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    parse_cargo_json: bool,
) -> Result<(Process, Receiver<RunEvent>)> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if !cwd.as_os_str().is_empty() {
        command.current_dir(cwd);
    }
    for (key, value) in env {
        if key.is_empty() || key.contains(['=', '\0']) {
            log::warn!("ignoring invalid environment variable name {key:?}");
            continue;
        }
        command.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Lead a new process group so `kill` can reach everything the program starts.
        command.process_group(0);
    }

    let command_line = format_command_line(program, args);
    let started = Instant::now();
    let mut child =
        command.spawn().map_err(|source| CargoError::Spawn { program: program.display().to_string(), source })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let process = Process { pid: child.id(), child: Arc::new(Mutex::new(child)), state: Arc::new(RunState::default()) };
    log::debug!("started pid {}: {command_line}", process.pid);

    let (events, receiver) = unbounded();
    // `receiver` is alive, so this cannot fail.
    let _ = events.send(RunEvent::Started { command_line });

    let (lines_tx, lines_rx) = unbounded();
    let mut threads = Ok(());
    if let Some(stdout) = stdout {
        let tx = lines_tx.clone();
        threads = spawn_thread("stdout", move || {
            read_stream(stdout, "stdout", &tx, |line| deliver_stdout(line, parse_cargo_json, &tx))
        });
    }
    if let Some(stderr) = stderr
        && threads.is_ok()
    {
        let tx = lines_tx.clone();
        threads = spawn_thread("stderr", move || {
            read_stream(stderr, "stderr", &tx, |line| tx.send(RunEvent::Stderr(line)).is_ok())
        });
    }
    drop(lines_tx);
    let threads = threads.and_then(|()| {
        let (child, state) = (Arc::clone(&process.child), Arc::clone(&process.state));
        spawn_thread("wait", move || supervise(&child, &state, &lines_rx, &events, started))
    });
    if let Err(e) = threads {
        // Nobody would ever reap or report it: don't leave it running.
        process.abort();
        return Err(CargoError::Io(e));
    }
    Ok((process, receiver))
}

fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new().name(format!("codecraft-cargo-{name}")).spawn(body).map(drop)
}

fn format_command_line(program: &Path, args: &[String]) -> String {
    std::iter::once(shell_quote(&program.to_string_lossy()))
        .chain(args.iter().map(|a| shell_quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A stdout line: a cargo JSON message (diagnostics also echo their rendered text as `Stderr`
/// lines, the way cargo prints them in a terminal), or a `Stdout` line plus a `Test` event when
/// it is libtest output. Returns `false` once nobody listens.
fn deliver_stdout(line: String, parse_cargo_json: bool, tx: &Sender<RunEvent>) -> bool {
    if parse_cargo_json && let Some(message) = parse_message(&line) {
        if let CargoMessage::CompilerMessage { diagnostic, .. } = &message
            && let Some(rendered) = &diagnostic.rendered
        {
            for rendered_line in rendered.lines() {
                if tx.send(RunEvent::Stderr(rendered_line.to_owned())).is_err() {
                    return false;
                }
            }
        }
        return tx.send(RunEvent::Message(message)).is_ok();
    }
    let test = parse_test_line(&line);
    if tx.send(RunEvent::Stdout(line)).is_err() {
        return false;
    }
    test.is_none_or(|test| tx.send(RunEvent::Test(test)).is_ok())
}

fn read_stream(reader: impl Read, stream: &str, tx: &Sender<RunEvent>, mut deliver: impl FnMut(String) -> bool) {
    if let Err(e) = read_lines(reader, &mut deliver) {
        log::warn!("reading {stream} failed: {e}");
        let _ = tx.send(RunEvent::Error(format!("reading {stream} failed: {e}")));
    }
}

/// Feed `reader` through a [`LineSplitter`] until EOF, an error, or `deliver` returning `false`.
fn read_lines(mut reader: impl Read, deliver: &mut impl FnMut(String) -> bool) -> io::Result<()> {
    let mut splitter = LineSplitter::new(MAX_LINE_BYTES);
    let mut buf = vec![0; READ_CHUNK_BYTES];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                splitter.finish(deliver);
                return Err(e);
            }
        };
        let Some(chunk) = buf.get(..read) else {
            break;
        };
        if !splitter.feed(chunk, deliver) {
            return Ok(());
        }
    }
    splitter.finish(deliver);
    Ok(())
}

/// Splits a byte stream into lines on `\n`, `\r\n` and lone `\r` (progress updates become
/// separate lines; whitespace-only `\r` segments, which only clear a terminal line, are dropped).
/// Invalid UTF-8 is replaced. Lines longer than the cap are split.
pub(crate) struct LineSplitter {
    current: Vec<u8>,
    /// `Some(emitted)` right after a `\r`: whether that segment was emitted (a following `\n`
    /// then belongs to it) or held back as blank (emitted only if a `\n` follows: `\r\n`).
    after_cr: Option<bool>,
    max_line: usize,
}

impl LineSplitter {
    pub(crate) fn new(max_line: usize) -> Self {
        Self { current: Vec::new(), after_cr: None, max_line: max_line.max(1) }
    }

    /// Returns `false` as soon as `emit` does.
    pub(crate) fn feed(&mut self, bytes: &[u8], emit: &mut impl FnMut(String) -> bool) -> bool {
        for &byte in bytes {
            match byte {
                b'\n' => {
                    let emitted = self.after_cr.take() == Some(true);
                    if !emitted && !emit(self.take_line()) {
                        return false;
                    }
                }
                b'\r' => {
                    if self.after_cr.is_some() {
                        continue;
                    }
                    if self.current.iter().all(u8::is_ascii_whitespace) {
                        self.after_cr = Some(false);
                    } else {
                        self.after_cr = Some(true);
                        if !emit(self.take_line()) {
                            return false;
                        }
                    }
                }
                _ => {
                    if self.after_cr.take() == Some(false) {
                        self.current.clear();
                    }
                    self.current.push(byte);
                    if self.current.len() >= self.max_line && !emit(self.take_line()) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Emit a final unterminated line, if any.
    pub(crate) fn finish(&mut self, emit: &mut impl FnMut(String) -> bool) -> bool {
        let pending_cr = self.after_cr.take().is_some();
        if pending_cr || self.current.is_empty() {
            self.current.clear();
            return true;
        }
        emit(self.take_line())
    }

    fn take_line(&mut self) -> String {
        match String::from_utf8(std::mem::take(&mut self.current)) {
            Ok(line) => line,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }
}

/// Forward reader events, reap the child, then send the one `Exited`.
fn supervise(
    child: &Mutex<Child>,
    state: &RunState,
    lines: &Receiver<RunEvent>,
    events: &Sender<RunEvent>,
    started: Instant,
) {
    let mut readers_done = false;
    let mut exit: Option<(std::result::Result<ExitStatus, String>, Instant)> = None;
    let mut last_output = Instant::now();
    let mut last_poll = Instant::now();
    loop {
        if readers_done {
            if exit.is_some() {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        } else {
            match lines.recv_timeout(POLL_INTERVAL) {
                Ok(event) => {
                    last_output = Instant::now();
                    // The consumer may have dropped its receiver; keep going so the child is reaped.
                    let _ = events.send(event);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => readers_done = true,
            }
        }
        if exit.is_none() && (readers_done || last_poll.elapsed() >= POLL_INTERVAL) {
            last_poll = Instant::now();
            let mut guard = lock(child);
            let polled = match guard.try_wait() {
                Ok(Some(status)) => Some(Ok(status)),
                Ok(None) => None,
                Err(e) => Some(Err(e.to_string())),
            };
            if let Some(polled) = polled {
                state.reaped.store(true, Ordering::SeqCst);
                exit = Some((polled, Instant::now()));
            }
            drop(guard);
        }
        if let Some((_, exited_at)) = &exit
            && !readers_done
            && exited_at.elapsed() >= ORPHANED_PIPE_GRACE
            && last_output.elapsed() >= ORPHANED_PIPE_GRACE
        {
            log::debug!("process exited but its output is still held open by another process; finishing");
            break;
        }
    }
    // Only non-empty when we stopped waiting for orphaned pipes.
    while let Ok(event) = lines.try_recv() {
        let _ = events.send(event);
    }
    let (code, success, duration) = match exit {
        Some((Ok(status), at)) => (status.code(), status.success(), at.duration_since(started)),
        Some((Err(e), at)) => {
            let _ = events.send(RunEvent::Error(format!("cannot wait for process: {e}")));
            (None, false, at.duration_since(started))
        }
        None => (None, false, started.elapsed()),
    };
    let _ = events.send(RunEvent::Exited { code, success, duration });
    state.finished.store(true, Ordering::SeqCst);
}

impl Process {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.state.reaped.load(Ordering::SeqCst)
    }

    #[cfg(unix)]
    pub(crate) fn kill(&self) -> Result<()> {
        if self.state.finished.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.state.kill_requested.swap(true, Ordering::SeqCst) {
            // Asked again during the grace period: don't wait any longer.
            return force_kill(self.pid, &self.child, &self.state);
        }
        let signalled = {
            // Holding the lock keeps the leader unreaped, so its pid (the group id) can't be reused.
            let _guard = lock(&self.child);
            signal_group(self.pid, "TERM")
        };
        if !signalled {
            return force_kill(self.pid, &self.child, &self.state);
        }
        let (pid, child, state) = (self.pid, Arc::clone(&self.child), Arc::clone(&self.state));
        let escalation = spawn_thread("kill", move || {
            let deadline = Instant::now() + TERM_GRACE;
            while Instant::now() < deadline {
                if state.finished.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(POLL_INTERVAL);
            }
            if let Err(e) = force_kill(pid, &child, &state) {
                log::warn!("cannot kill pid {pid}: {e}");
            }
        });
        if let Err(e) = escalation {
            log::warn!("cannot start kill escalation thread: {e}");
            return force_kill(self.pid, &self.child, &self.state);
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub(crate) fn kill(&self) -> Result<()> {
        if self.state.finished.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.state.kill_requested.store(true, Ordering::SeqCst);
        kill_child(&self.child, &self.state)
    }

    /// Kill and reap synchronously; used when the run cannot be supervised.
    fn abort(&self) {
        #[cfg(unix)]
        let _ = force_kill(self.pid, &self.child, &self.state);
        #[cfg(not(unix))]
        let _ = kill_child(&self.child, &self.state);
        let _ = lock(&self.child).wait();
        self.state.reaped.store(true, Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn force_kill(pid: u32, child: &Mutex<Child>, state: &RunState) -> Result<()> {
    let guard = lock(child);
    if state.finished.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Even with the leader reaped, members of its group may still hold the output open.
    signal_group(pid, "KILL");
    drop(guard);
    kill_child(child, state)
}

fn kill_child(child: &Mutex<Child>, state: &RunState) -> Result<()> {
    let mut guard = lock(child);
    if state.reaped.load(Ordering::SeqCst) {
        return Ok(());
    }
    match guard.kill() {
        Ok(()) => Ok(()),
        // Older std reports an already-exited child this way.
        Err(e) if e.kind() == ErrorKind::InvalidInput => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `kill -<signal> -<pgid>` through the external `kill` command (no `unsafe` libc call).
/// Returns whether the command could be run; a group that is already gone is not an error.
#[cfg(unix)]
fn signal_group(pgid: u32, signal: &str) -> bool {
    // `kill -SIG -0` would signal *our* process group.
    if pgid == 0 {
        return false;
    }
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) => {
            if !status.success() {
                log::debug!("kill -{signal} -{pgid}: {status}");
            }
            true
        }
        Err(e) => {
            log::warn!("cannot run `kill`: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::new(MAX_LINE_BYTES);
        let mut lines = Vec::new();
        let mut emit = |line: String| {
            lines.push(line);
            true
        };
        for chunk in chunks {
            assert!(splitter.feed(chunk, &mut emit));
        }
        assert!(splitter.finish(&mut emit));
        lines
    }

    #[test]
    fn splits_lines() {
        assert_eq!(split(&[b"a\nb\n"]), ["a", "b"]);
        assert_eq!(split(&[b"a\r\nb\r\n"]), ["a", "b"]);
        assert_eq!(split(&[b"\n\n"]), ["", ""]);
        assert_eq!(split(&[b"\r\n\r\n"]), ["", ""]);
        assert_eq!(split(&[b"tail"]), ["tail"]);
        assert_eq!(split(&[b"x\ntail"]), ["x", "tail"]);
        assert_eq!(split(&[b""]), Vec::<String>::new());
        assert_eq!(split(&[b"a\r\r\nb"]), ["a", "b"]);
    }

    #[test]
    fn carriage_return_progress() {
        assert_eq!(split(&[b"10%\r20%\r30%\n"]), ["10%", "20%", "30%"]);
        // Line-clearing segments disappear.
        assert_eq!(split(&[b"working\r       \rdone\n"]), ["working", "done"]);
        assert_eq!(split(&[b"\rdone\n"]), ["done"]);
        assert_eq!(split(&[b"last\r"]), ["last"]);
        assert_eq!(split(&[b"   \r"]), Vec::<String>::new());
    }

    #[test]
    fn chunk_boundaries_do_not_matter() {
        let input: &[u8] = b"one\r\ntwo\rthree\n\nfour\r\r\nfive \xc3\xa9\n   \rsix";
        let whole = split(&[input]);
        let bytewise: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(split(&bytewise), whole);
        assert_eq!(whole, ["one", "two", "three", "", "four", "five é", "six"]);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        assert_eq!(split(&[b"\xff\xfeok\n"]), ["\u{fffd}\u{fffd}ok"]);
    }

    #[test]
    fn long_lines_are_split() {
        let mut splitter = LineSplitter::new(4);
        let mut lines = Vec::new();
        let mut emit = |line: String| {
            lines.push(line);
            true
        };
        assert!(splitter.feed(b"abcdefghij\nxy\n", &mut emit));
        assert!(splitter.finish(&mut emit));
        assert_eq!(lines, ["abcd", "efgh", "ij", "xy"]);
    }

    #[test]
    fn stops_when_the_listener_is_gone() {
        let mut splitter = LineSplitter::new(MAX_LINE_BYTES);
        let mut count = 0;
        let mut emit = |_line: String| {
            count += 1;
            false
        };
        assert!(!splitter.feed(b"a\nb\nc\n", &mut emit));
        assert_eq!(count, 1);
    }

    #[test]
    fn read_lines_reports_errors_after_flushing() {
        struct Failing(bool);
        impl Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    return Err(io::Error::other("boom"));
                }
                buf[..7].copy_from_slice(b"partial");
                Ok(7)
            }
        }
        let mut lines = Vec::new();
        let result = read_lines(Failing(false), &mut |line| {
            lines.push(line);
            true
        });
        assert_eq!(result.unwrap_err().to_string(), "boom");
        assert_eq!(lines, ["partial"]);
    }

    #[test]
    fn stdout_delivery() {
        let (tx, rx) = unbounded();
        let diagnostic = serde_json::json!({
            "reason": "compiler-message", "package_id": "p",
            "message": {"message": "unused", "level": "warning", "rendered": "warning: unused\n  |\n\n"}
        })
        .to_string();
        assert!(deliver_stdout(diagnostic.clone(), true, &tx));
        assert!(deliver_stdout(r#"{"reason":"build-finished","success":true}"#.into(), true, &tx));
        assert!(deliver_stdout("test a ... ok".into(), true, &tx));
        assert!(deliver_stdout("hello".into(), true, &tx));
        assert!(deliver_stdout(diagnostic.clone(), false, &tx));
        drop(tx);
        let events: Vec<RunEvent> = rx.iter().collect();
        assert_eq!(events.len(), 9, "{events:#?}");
        assert_eq!(events[0], RunEvent::Stderr("warning: unused".into()));
        assert_eq!(events[1], RunEvent::Stderr("  |".into()));
        assert_eq!(events[2], RunEvent::Stderr(String::new()));
        assert!(
            matches!(&events[3], RunEvent::Message(CargoMessage::CompilerMessage { diagnostic, .. }) if diagnostic.message == "unused")
        );
        assert_eq!(events[4], RunEvent::Message(CargoMessage::BuildFinished { success: true }));
        assert_eq!(events[5], RunEvent::Stdout("test a ... ok".into()));
        assert_eq!(
            events[6],
            RunEvent::Test(crate::TestEvent::Result { name: "a".into(), outcome: crate::TestOutcome::Ok })
        );
        assert_eq!(events[7], RunEvent::Stdout("hello".into()));
        // Without JSON parsing the line is plain output.
        assert_eq!(events[8], RunEvent::Stdout(diagnostic));

        let (tx, rx) = unbounded();
        drop(rx);
        assert!(!deliver_stdout("hello".into(), true, &tx));
    }

    #[test]
    fn command_line_quotes() {
        let args = ["run".to_owned(), "--".to_owned(), "a b".to_owned(), "it's".to_owned()];
        assert_eq!(
            format_command_line(Path::new("/opt/my tools/cargo"), &args),
            r"'/opt/my tools/cargo' run -- 'a b' 'it'\''s'"
        );
    }

    #[test]
    fn spawn_failure_is_an_error() {
        let missing = Path::new("/definitely/not/a/program/codecraft-xyz");
        match spawn(missing, &[], Path::new("."), &[], false) {
            Err(CargoError::Spawn { program, .. }) => assert_eq!(program, missing.display().to_string()),
            other => panic!("expected a spawn error, got {other:?}"),
        }
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("missing-dir");
        assert!(matches!(spawn(Path::new("echo"), &[], &cwd, &[], false), Err(CargoError::Spawn { .. })));
    }

    #[cfg(unix)]
    mod unix {
        use std::path::PathBuf;

        use super::*;
        use crate::TestEvent;

        const TIMEOUT: Duration = Duration::from_secs(20);

        fn sh(script: &str) -> (Process, Receiver<RunEvent>) {
            sh_with(script, Path::new("."), &[], false)
        }

        fn sh_with(script: &str, cwd: &Path, env: &[(String, String)], json: bool) -> (Process, Receiver<RunEvent>) {
            spawn(Path::new("sh"), &["-c".to_owned(), script.to_owned()], cwd, env, json).unwrap()
        }

        /// Events up to and including `Exited`; then checks the channel closes (nothing after it).
        fn collect(rx: &Receiver<RunEvent>) -> Vec<RunEvent> {
            let deadline = Instant::now() + TIMEOUT;
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
                    Err(e) => panic!("no Exited within {TIMEOUT:?} ({e}); got {events:#?}"),
                }
            }
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected));
            events
        }

        fn exited(events: &[RunEvent]) -> (Option<i32>, bool) {
            match events.last() {
                Some(RunEvent::Exited { code, success, .. }) => (*code, *success),
                other => panic!("last event is not Exited: {other:?}"),
            }
        }

        fn lines(events: &[RunEvent], stderr: bool) -> Vec<String> {
            events
                .iter()
                .filter_map(|e| match e {
                    RunEvent::Stdout(l) if !stderr => Some(l.clone()),
                    RunEvent::Stderr(l) if stderr => Some(l.clone()),
                    _ => None,
                })
                .collect()
        }

        fn alive(pid: &str) -> bool {
            Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().unwrap().success()
        }

        #[test]
        fn streams_output_and_exit_code() {
            let (process, rx) = sh("echo out1; echo err1 >&2; printf 'out2'; exit 3");
            assert_ne!(process.pid(), 0);
            let events = collect(&rx);
            assert!(matches!(&events[0], RunEvent::Started { command_line } if command_line.starts_with("sh -c ")));
            assert_eq!(lines(&events, false), ["out1", "out2"]);
            assert_eq!(lines(&events, true), ["err1"]);
            assert_eq!(exited(&events), (Some(3), false));
            assert_eq!(events.iter().filter(|e| matches!(e, RunEvent::Exited { .. })).count(), 1);
            assert!(!process.is_running());
            // Killing a finished run is a no-op.
            process.kill().unwrap();
        }

        #[test]
        fn success_and_duration() {
            let (_process, rx) = sh("sleep 0.2");
            let events = collect(&rx);
            match events.last() {
                Some(RunEvent::Exited { code: Some(0), success: true, duration }) => {
                    assert!(*duration >= Duration::from_millis(150), "{duration:?}");
                    assert!(*duration < TIMEOUT);
                }
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn env_and_cwd_apply() {
            let tmp = tempfile::tempdir().unwrap();
            let env =
                [("CODECRAFT_TEST_VAR".to_owned(), "hello there".to_owned()), ("BAD=KEY".to_owned(), "x".to_owned())];
            let (_process, rx) = sh_with("echo \"$CODECRAFT_TEST_VAR\"; pwd -P", tmp.path(), &env, false);
            let events = collect(&rx);
            let out = lines(&events, false);
            assert_eq!(out[0], "hello there");
            assert_eq!(PathBuf::from(&out[1]), tmp.path().canonicalize().unwrap());
            assert_eq!(exited(&events), (Some(0), true));
        }

        #[test]
        fn json_messages_and_test_lines() {
            let tmp = tempfile::tempdir().unwrap();
            let diagnostic = serde_json::json!({
                "reason": "compiler-message", "package_id": "p", "manifest_path": "/p/Cargo.toml",
                "message": {"message": "unused variable: `x`", "level": "warning", "code": {"code": "unused_variables"},
                            "spans": [], "children": [], "rendered": "warning: unused variable: `x`\n --> src/lib.rs:2:9\n\n"}
            });
            let script = format!(
                "{diagnostic}\n{}\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
                r#"{"reason":"build-finished","success":false}"#
            );
            std::fs::write(tmp.path().join("out.txt"), script).unwrap();
            let (_process, rx) = sh_with("cat out.txt", tmp.path(), &[], true);
            let events = collect(&rx);
            // JSON lines are not echoed raw.
            assert!(lines(&events, false).iter().all(|l| !l.starts_with('{')));
            assert_eq!(lines(&events, true), ["warning: unused variable: `x`", " --> src/lib.rs:2:9", ""]);
            let messages: Vec<&CargoMessage> =
                events.iter().filter_map(|e| if let RunEvent::Message(m) = e { Some(m) } else { None }).collect();
            assert_eq!(messages.len(), 2);
            assert!(
                matches!(messages[0], CargoMessage::CompilerMessage { diagnostic, .. } if diagnostic.code.as_deref() == Some("unused_variables"))
            );
            assert_eq!(messages[1], &CargoMessage::BuildFinished { success: false });
            let tests: Vec<&TestEvent> =
                events.iter().filter_map(|e| if let RunEvent::Test(t) = e { Some(t) } else { None }).collect();
            assert_eq!(tests.len(), 4);
            assert_eq!(tests[0], &TestEvent::SuiteStarted { count: 2 });
            assert_eq!(
                tests[3],
                &TestEvent::SuiteFinished { passed: 1, failed: 1, ignored: 0, measured: 0, filtered_out: 0 }
            );
            // Each Test event directly follows its Stdout line.
            let position =
                events.iter().position(|e| e == &RunEvent::Test(TestEvent::SuiteStarted { count: 2 })).unwrap();
            assert_eq!(events[position - 1], RunEvent::Stdout("running 2 tests".into()));
        }

        #[test]
        fn kill_ends_the_process_group() {
            // The background `sleep` is a grandchild in the same group, holding the pipes open.
            let (process, rx) = sh("sleep 30 & echo $!; wait");
            let first = rx.recv_timeout(TIMEOUT).unwrap();
            assert!(matches!(first, RunEvent::Started { .. }));
            let grandchild = match rx.recv_timeout(TIMEOUT).unwrap() {
                RunEvent::Stdout(pid) => pid,
                other => panic!("{other:?}"),
            };
            assert!(process.is_running());
            let asked = Instant::now();
            process.kill().unwrap();
            let events = collect(&rx);
            assert!(asked.elapsed() < Duration::from_secs(10), "{:?}", asked.elapsed());
            assert!(!exited(&events).1);
            assert!(!process.is_running());
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive(&grandchild) {
                assert!(Instant::now() < deadline, "grandchild {grandchild} survived");
                thread::sleep(Duration::from_millis(50));
            }
        }

        #[test]
        fn kill_escalates_when_term_is_ignored() {
            let (process, rx) = sh("trap '' TERM; echo ready; while true; do sleep 0.1; done");
            let deadline = Instant::now() + TIMEOUT;
            while rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
                != RunEvent::Stdout("ready".into())
            {}
            let asked = Instant::now();
            process.kill().unwrap();
            let events = collect(&rx);
            let took = asked.elapsed();
            assert!(took >= TERM_GRACE.saturating_sub(Duration::from_millis(100)), "{took:?}");
            assert!(took < Duration::from_secs(10), "{took:?}");
            assert_eq!(exited(&events), (None, false));
        }

        #[test]
        fn second_kill_skips_the_grace_period() {
            let (process, rx) = sh("trap '' TERM; echo ready; while true; do sleep 0.1; done");
            let deadline = Instant::now() + TIMEOUT;
            while rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap()
                != RunEvent::Stdout("ready".into())
            {}
            let asked = Instant::now();
            process.kill().unwrap();
            process.kill().unwrap();
            collect(&rx);
            assert!(asked.elapsed() < TERM_GRACE, "{:?}", asked.elapsed());
        }

        #[test]
        fn orphaned_pipes_do_not_hold_the_run_open() {
            // `sh` exits at once, but the background `sleep` keeps stdout/stderr open.
            let (process, rx) = sh("sleep 30 & echo $!");
            let started = Instant::now();
            let events = collect(&rx);
            let took = started.elapsed();
            let grandchild = lines(&events, false).pop().unwrap();
            let _ = Command::new("kill").arg(&grandchild).status();
            assert_eq!(exited(&events), (Some(0), true));
            assert!(took >= ORPHANED_PIPE_GRACE && took < Duration::from_secs(15), "{took:?}");
            assert!(!process.is_running());
        }

        #[test]
        fn dropping_the_receiver_still_reaps() {
            let (process, rx) = sh("echo hi; exit 0");
            drop(rx);
            let deadline = Instant::now() + TIMEOUT;
            while process.is_running() {
                assert!(Instant::now() < deadline, "never reaped");
                thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn lossy_output_and_no_trailing_newline() {
            let (_process, rx) = sh(r"printf 'caf\303\251 \377\nend'");
            let events = collect(&rx);
            assert_eq!(lines(&events, false), ["café \u{fffd}", "end"]);
        }
    }
}
