//! Running `git`: one child process per command, in its own process group, never attached to a
//! terminal prompt. Output can be echoed line by line to a console [`Sink`] (the Git tool
//! window's Console tab) and long commands can be cancelled through a [`CancelToken`].

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::{GitError, Result};

/// Commands print at most this much on stdout before they are stopped (status of a huge repo,
/// a blob too large to diff).
pub const DEFAULT_MAX_OUTPUT: usize = 256 << 20;
/// Stderr kept for error messages.
const MAX_STDERR: usize = 1 << 20;
/// Grace period between `TERM` and `KILL` when cancelling.
const KILL_GRACE: Duration = Duration::from_millis(1500);

/// One line of the operations console.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleLine {
    /// A command started, e.g. `git push origin main`.
    Command(String),
    /// A line of output. `progress` lines ended with `\r` (git's progress meters) and are
    /// replaced by the next line.
    Output { stderr: bool, text: String, progress: bool },
    /// The command finished with this exit code (`None`: killed by a signal).
    Exit(Option<i32>),
}

pub type Sink = Sender<ConsoleLine>;

/// Cancels the command currently running under it by killing its whole process group
/// (`git` and any `ssh`, credential helper or hook it started).
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    cancelled: AtomicBool,
    /// The running child's pid (= its process group id), cleared before it is reaped.
    pid: Mutex<Option<u32>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Stop the running command (and refuse to start new ones).
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let pid = *self.inner.pid.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(pid) = pid else { return };
        kill_group(pid, "TERM");
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new().name("git-kill".into()).spawn(move || {
            std::thread::sleep(KILL_GRACE);
            let guard = inner.pid.lock().unwrap_or_else(PoisonError::into_inner);
            if *guard == Some(pid) {
                kill_group(pid, "KILL");
            }
        });
        if let Err(e) = spawned {
            log::warn!("cannot start the git kill timer: {e}");
        }
    }

    fn set_pid(&self, pid: Option<u32>) {
        *self.inner.pid.lock().unwrap_or_else(PoisonError::into_inner) = pid;
    }
}

/// Send `signal` to process group `pgid` with the `kill` command (no `unsafe`).
fn kill_group(pgid: u32, signal: &str) {
    if pgid <= 1 || !cfg!(unix) {
        return;
    }
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(e) = status {
        log::warn!("kill -{signal} -{pgid}: {e}");
    }
}

/// A `git` invoker bound to a working directory.
#[derive(Clone, Debug)]
pub struct Git {
    dir: PathBuf,
    program: PathBuf,
    sink: Option<Sink>,
    cancel: Option<CancelToken>,
    /// Extra environment (tests set an isolated `HOME`/`GIT_CONFIG_GLOBAL`).
    env: Vec<(OsString, OsString)>,
}

/// What a finished command printed.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: Option<i32>,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stdout and stderr together (hook output, merge messages) for the user.
    pub fn combined(&self) -> String {
        let mut s = self.text();
        if !self.stderr.trim().is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s.trim_end().to_string()
    }
}

impl Git {
    /// Run `git` (found on `PATH`) in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), program: PathBuf::from("git"), sink: None, cancel: None, env: Vec::new() }
    }

    /// Echo every command and its output to `sink`.
    pub fn with_sink(mut self, sink: Sink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Add an environment variable for every command.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Use another `git` binary.
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The same invoker in another directory (clone runs in the parent of its target).
    pub fn in_dir(&self, dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), ..self.clone() }
    }

    pub fn cancel_token(&self) -> Option<&CancelToken> {
        self.cancel.as_ref()
    }

    /// Start building a command.
    pub fn cmd<I, S>(&self, args: I) -> Cmd<'_>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Cmd {
            git: self,
            args: args.into_iter().map(|a| a.as_ref().to_os_string()).collect(),
            stdin: None,
            read_only: false,
            network: false,
            echo: true,
            ok_codes: vec![0],
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }

    /// A read-only query: `--no-optional-locks`, never echoed to the console.
    pub fn read<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.cmd(args).read_only().run()?.stdout)
    }

    /// A read-only query as (lossy) text.
    pub fn read_text<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.read(args).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// A command that changes the repository, echoed to the console.
    pub fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd(args).run()
    }

    fn emit(&self, line: ConsoleLine) {
        if let Some(s) = &self.sink {
            let _ = s.send(line);
        }
    }
}

/// A command being built; see [`Git::cmd`].
pub struct Cmd<'g> {
    git: &'g Git,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    read_only: bool,
    network: bool,
    echo: bool,
    ok_codes: Vec<i32>,
    max_output: usize,
}

impl Cmd<'_> {
    /// Doesn't change the repository: adds `--no-optional-locks` (no index refresh, so status
    /// never wakes the file watcher) and is not echoed.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self.echo = false;
        self
    }

    /// Talks to a remote: SSH runs in batch mode unless the user configured their own SSH
    /// command, so it never waits for a passphrase on a terminal nobody sees.
    pub fn network(mut self) -> Self {
        self.network = true;
        self
    }

    pub fn stdin(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    /// Don't echo this command to the console.
    pub fn quiet(mut self) -> Self {
        self.echo = false;
        self
    }

    /// Echo this command to the console even though it is read-only.
    pub fn echo(mut self) -> Self {
        self.echo = true;
        self
    }

    /// Treat this exit code as success too (`git diff --no-index` exits 1 on differences).
    pub fn allow_exit(mut self, code: i32) -> Self {
        self.ok_codes.push(code);
        self
    }

    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }

    fn display(&self) -> String {
        let mut s = String::from("git");
        for a in &self.args {
            let a = a.to_string_lossy();
            s.push(' ');
            if a.is_empty() || a.contains([' ', '\t', '\n', '"', '\'']) {
                s.push('"');
                s.push_str(&a.replace('"', "\\\""));
                s.push('"');
            } else {
                s.push_str(&a);
            }
        }
        s
    }

    /// Run to completion. A non-allowed exit code is an error carrying git's message.
    pub fn run(self) -> Result<Output> {
        let git = self.git;
        if git.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(GitError::Cancelled);
        }
        let shown = self.display();
        let mut command = Command::new(&git.program);
        command.current_dir(&git.dir);
        command.arg("--no-pager");
        if self.read_only {
            command.arg("--no-optional-locks");
        }
        command.args(["-c", "core.quotepath=false", "-c", "color.ui=false", "-c", "log.showSignature=false"]);
        command.args(&self.args);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
            .env("LANGUAGE", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        if self.read_only {
            command.env("GIT_OPTIONAL_LOCKS", "0");
        }
        for (k, v) in &git.env {
            command.env(k, v);
        }
        if self.network && std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
            let configured = git
                .cmd(["config", "--get", "core.sshCommand"])
                .read_only()
                .allow_exit(1)
                .run()
                .map(|o| !o.text().trim().is_empty())
                .unwrap_or(false);
            if !configured {
                command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
            }
        }
        command.stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let echo = self.echo && git.sink.is_some();
        if echo {
            git.emit(ConsoleLine::Command(shown.clone()));
        }
        log::debug!("{shown} (in {})", git.dir.display());
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                if git.dir.is_dir() {
                    GitError::GitNotFound
                } else {
                    GitError::Io(format!("{}: {e}", git.dir.display()))
                }
            } else {
                GitError::Io(format!("cannot start git: {e}"))
            }
        })?;
        if let Some(c) = &git.cancel {
            c.set_pid(Some(child.id()));
        }
        // Feed stdin on its own thread so a large input can't deadlock against our reads.
        let writer = match (child.stdin.take(), self.stdin) {
            (Some(mut pipe), Some(input)) => std::thread::Builder::new()
                .name("git-stdin".into())
                .spawn(move || {
                    let _ = pipe.write_all(&input);
                })
                .ok(),
            _ => None,
        };
        let sink = if echo { git.sink.clone() } else { None };
        let stderr_reader = child.stderr.take().and_then(|pipe| {
            let sink = sink.clone();
            std::thread::Builder::new()
                .name("git-stderr".into())
                .spawn(move || read_stream(pipe, true, sink, MAX_STDERR))
                .ok()
        });
        let max = self.max_output;
        let (stdout, overflow) = match child.stdout.take() {
            Some(pipe) => {
                let (bytes, overflow) = read_stream(pipe, false, sink.clone(), max);
                if overflow {
                    // Stop a runaway command (its group, before it is reaped).
                    kill_group(child.id(), "KILL");
                }
                (bytes, overflow)
            }
            None => (Vec::new(), false),
        };
        let (stderr_bytes, _) = stderr_reader.and_then(|h| h.join().ok()).unwrap_or_default();
        if let Some(w) = writer {
            let _ = w.join();
        }
        if let Some(c) = &git.cancel {
            c.set_pid(None);
        }
        let status = child.wait().map_err(|e| GitError::Io(format!("waiting for git: {e}")))?;
        let code = status.code();
        if echo {
            git.emit(ConsoleLine::Exit(code));
        }
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        if git.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(GitError::Cancelled);
        }
        if overflow {
            return Err(GitError::TooLarge(format!("{shown} printed more than {max} bytes")));
        }
        let out = Output { stdout, stderr, code };
        match code {
            Some(c) if self.ok_codes.contains(&c) => Ok(out),
            _ => Err(failure(&shown, &out)),
        }
    }

    /// Run and return stdout as (lossy) text.
    pub fn text(self) -> Result<String> {
        self.run().map(|o| o.text())
    }
}

/// Read a pipe to EOF (up to `max` bytes kept), echoing lines to `sink`. Returns the bytes and
/// whether the cap was hit.
fn read_stream(mut pipe: impl Read, stderr: bool, sink: Option<Sink>, max: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut line: Vec<u8> = Vec::new();
    let mut overflow = false;
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let chunk = buf.get(..n).unwrap_or_default();
        if out.len().saturating_add(n) > max {
            overflow = true;
            break;
        }
        out.extend_from_slice(chunk);
        if let Some(sink) = &sink {
            for &b in chunk {
                if b == b'\n' || b == b'\r' {
                    let text = String::from_utf8_lossy(&line).into_owned();
                    let _ = sink.send(ConsoleLine::Output { stderr, text, progress: b == b'\r' });
                    line.clear();
                } else if line.len() < 64 * 1024 {
                    line.push(b);
                }
            }
        }
    }
    if let Some(sink) = &sink
        && !line.is_empty()
    {
        let _ = sink.send(ConsoleLine::Output {
            stderr,
            text: String::from_utf8_lossy(&line).into_owned(),
            progress: false,
        });
    }
    (out, overflow)
}

/// Patterns git, ssh and credential helpers print when a remote wants credentials we may not ask
/// for (no terminal prompts).
const AUTH_PATTERNS: &[&str] = &[
    "terminal prompts disabled",
    "could not read username",
    "could not read password",
    "authentication failed",
    "permission denied (publickey",
    "host key verification failed",
    "invalid username or password",
    "403 forbidden",
    "the requested url returned error: 401",
    "the requested url returned error: 403",
];

/// An error for a failed command: authentication problems are recognised.
fn failure(shown: &str, out: &Output) -> GitError {
    let stderr = out.stderr.trim();
    let message = if stderr.is_empty() { out.text().trim().to_string() } else { stderr.to_string() };
    let lower = message.to_lowercase();
    if AUTH_PATTERNS.iter().any(|p| lower.contains(p)) {
        return GitError::AuthRequired(clean_message(&message));
    }
    GitError::Failed { command: shown.to_string(), code: out.code, message: clean_message(&message) }
}

/// Drop progress noise (`\r` meters) and keep the last 40 lines.
fn clean_message(message: &str) -> String {
    let lines: Vec<&str> =
        message.lines().map(|l| l.rsplit('\r').next().unwrap_or(l)).filter(|l| !l.trim().is_empty()).collect();
    let skip = lines.len().saturating_sub(40);
    lines.get(skip..).unwrap_or_default().join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors_are_recognised() {
        let out = Output {
            stdout: Vec::new(),
            stderr: "fatal: could not read Username for 'https://example.com': terminal prompts disabled\n".into(),
            code: Some(128),
        };
        assert!(matches!(failure("git push", &out), GitError::AuthRequired(_)));
        let out = Output { stdout: Vec::new(), stderr: "error: pathspec 'x' did not match\n".into(), code: Some(1) };
        match failure("git add x", &out) {
            GitError::Failed { message, code, .. } => {
                assert_eq!(code, Some(1));
                assert!(message.contains("pathspec"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn display_quotes_arguments() {
        let git = Git::new(".");
        let c = git.cmd(["commit", "-m", "two words", ""]);
        assert_eq!(c.display(), "git commit -m \"two words\" \"\"");
    }

    #[test]
    fn progress_lines_are_split_on_carriage_returns() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (bytes, overflow) = read_stream(&b"a 10%\ra 100%\ndone"[..], true, Some(tx), 1024);
        assert!(!overflow);
        assert_eq!(bytes.len(), 17);
        let lines: Vec<ConsoleLine> = rx.try_iter().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], ConsoleLine::Output { stderr: true, text: "a 10%".into(), progress: true });
        assert_eq!(lines[2], ConsoleLine::Output { stderr: true, text: "done".into(), progress: false });
        let (_, overflow) = read_stream(&[0u8; 100][..], false, None, 10);
        assert!(overflow);
    }
}
