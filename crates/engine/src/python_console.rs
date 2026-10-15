//! The Python Console: a REPL subprocess running the selected interpreter with a small embedded
//! driver script ([`DRIVER`]).
//!
//! Protocol: the engine writes one JSON object per line to the driver's stdin
//! (`{"op":"exec","id":1,"code":"…","force":false}`, `{"op":"input","text":"…"}`); the driver
//! answers with frames on stdout, one per line, each prefixed by [`FRAME_PREFIX`]:
//! `{"ev":"ready","version":"3.12.1"}`, `{"ev":"out"|"err","text":"…"}` (streamed while code
//! runs), `{"ev":"input","prompt":"…"}` (the code called `input()`), and
//! `{"ev":"done","id":1,"status":"ok"|"error"|"incomplete","vars":[…]}`. Other stdout lines
//! (C extensions writing to fd 1) are shown as output.
//!
//! Incomplete input is detected with `codeop` in `single` mode (so a block needs its closing
//! blank line, as in the interactive interpreter), falling back to `exec` mode for several
//! statements. Execution runs every statement and echoes the value of a final expression.
//!
//! Threads: a writer thread feeds stdin (a busy interpreter never blocks the UI), a reader
//! thread parses stdout frames, another forwards stderr. The engine polls in `pump`; the process
//! is killed on Stop, Restart, project close and shutdown (and when the console is dropped).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Marks a protocol frame on the driver's stdout.
pub const FRAME_PREFIX: &str = "\u{1}CCJ ";
/// Console transcript kept (older output is dropped).
const MAX_TRANSCRIPT_BYTES: usize = 2 << 20;
/// Longest stdout line read from the driver.
const MAX_LINE_BYTES: u64 = 4 << 20;
/// Inputs kept in the history.
const MAX_HISTORY: usize = 500;
/// Largest code sent at once.
pub const MAX_CODE_BYTES: usize = 4 << 20;

/// The driver script, run with `python -u -c DRIVER`.
pub const DRIVER: &str = r#"
import sys, json, ast, codeop, traceback, builtins
_P = "\x01CCJ "
_out = sys.stdout
_in = sys.stdin
def _send(o):
    _out.write(_P + json.dumps(o) + "\n")
    _out.flush()
class _Stream:
    encoding = "utf-8"
    errors = "replace"
    def __init__(self, ev):
        self.ev = ev
    def write(self, s):
        if s:
            _send({"ev": self.ev, "text": str(s)})
        return len(s)
    def writelines(self, lines):
        for l in lines:
            self.write(l)
    def flush(self):
        pass
    def isatty(self):
        return False
    def fileno(self):
        raise OSError("console stream has no file descriptor")
_pending = []
def _read():
    while True:
        line = _in.readline()
        if not line:
            raise SystemExit(0)
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except Exception:
            continue
def _input(prompt=""):
    if prompt:
        sys.stdout.write(str(prompt))
    _send({"ev": "input", "prompt": str(prompt)})
    while True:
        m = _read()
        if m.get("op") == "input":
            return m.get("text", "")
        if m.get("op") == "exec":
            _pending.append(m)
class _Stdin:
    encoding = "utf-8"
    def readline(self, *a):
        return _input("") + "\n"
    def read(self, *a):
        return _input("")
    def isatty(self):
        return False
builtins.input = _input
sys.stdout = _Stream("out")
sys.stderr = _Stream("err")
sys.stdin = _Stdin()
_ns = {"__name__": "__main__", "__builtins__": builtins}
def _incomplete(src):
    try:
        return codeop.compile_command(src, "<input>", "single") is None
    except (SyntaxError, ValueError, OverflowError):
        pass
    try:
        return codeop.compile_command(src, "<input>", "exec") is None
    except (SyntaxError, ValueError, OverflowError):
        return False
def _short(v):
    try:
        r = repr(v)
    except Exception as e:
        r = "<repr failed: %s>" % type(e).__name__
    return r if len(r) <= 200 else r[:197] + "..."
def _vars():
    out = []
    for k, v in list(_ns.items()):
        if k.startswith("_") or type(v).__name__ == "module":
            continue
        out.append(_cc_var(k, v))
        if len(out) >= 500:
            break
    return out
def _exec(src):
    try:
        tree = ast.parse(src, "<input>", "exec")
    except (SyntaxError, ValueError, OverflowError):
        sys.stderr.write("".join(traceback.format_exception_only(*sys.exc_info()[:2])))
        return "error"
    last = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        last = tree.body.pop()
    try:
        if tree.body:
            exec(compile(ast.Module(body=tree.body, type_ignores=[]), "<input>", "exec"), _ns)
        if last is not None:
            exec(compile(ast.Interactive(body=[last]), "<input>", "single"), _ns)
    except SystemExit:
        raise
    except BaseException:
        t, v, tb = sys.exc_info()
        tb = tb.tb_next if tb is not None else None
        sys.stderr.write("".join(traceback.format_exception(t, v, tb)))
        return "error"
    return "ok"
def _handle(m):
    src = m.get("code", "")
    if not m.get("force") and _incomplete(src):
        _send({"ev": "done", "id": m.get("id"), "status": "incomplete"})
        return
    status = _exec(src)
    _cc_cache.clear()
    _send({"ev": "done", "id": m.get("id"), "status": status, "vars": _vars()})
_send({"ev": "ready", "version": "%d.%d.%d" % sys.version_info[:3], "executable": sys.executable})
while True:
    try:
        m = _pending.pop(0) if _pending else _read()
        if m.get("op") == "exec":
            _handle(m)
        elif m.get("op") == "vars":
            _send({"ev": "done", "id": m.get("id"), "status": "ok", "vars": _vars()})
        elif m.get("op") == "frame":
            _send(_cc_frame(m, _ns))
    except KeyboardInterrupt:
        sys.stderr.write("KeyboardInterrupt\n")
        _send({"ev": "done", "id": None, "status": "error", "vars": _vars()})
    except SystemExit:
        break
"#;

/// The full console driver: the shared inspection helpers ([`crate::dataframe::INSPECT_PY`],
/// View as DataFrame) and [`DRIVER`].
pub fn driver() -> String {
    format!("{}\n{}", crate::dataframe::INSPECT_PY, DRIVER)
}

/// Who wrote a transcript chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleKind {
    /// What the user ran (shown with `>>>` prompts).
    Input,
    Output,
    Error,
    /// Console messages (started, restarted, exited).
    System,
}

/// A variable of the console namespace (from the driver).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleVar {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub value: String,
    /// A DataFrame, Series, 1-D/2-D array or list of rows: View as DataFrame/Array works (M12).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub viewable: bool,
    /// Its shape (`(100, 3)`) when viewable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
}

/// What the reader threads report.
#[derive(Debug)]
enum Msg {
    Frame(Value),
    Raw(ConsoleKind, String),
    Closed,
}

/// The running console process.
pub struct ConsoleProcess {
    /// `None` once killed (a reaper thread collects its exit).
    child: Option<Child>,
    stdin: Sender<String>,
    rx: Receiver<Msg>,
}

impl ConsoleProcess {
    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// Kill the process without waiting for it on this (the UI) thread.
    fn kill(&mut self) {
        let _ = self.end();
    }

    /// Kill the process and return its exit status if it is gone within ~100 ms; otherwise a
    /// reaper thread waits for it, so the caller never blocks on a process that won't die.
    fn end(&mut self) -> Option<std::process::ExitStatus> {
        let mut child = self.child.take()?;
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        let _ = child.kill();
        for _ in 0..20 {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = std::thread::Builder::new().name("console-reaper".into()).spawn(move || {
            let _ = child.wait();
        });
        None
    }
}

impl Drop for ConsoleProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

impl std::fmt::Debug for ConsoleProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleProcess").field("pid", &self.pid()).finish()
    }
}

/// The console's state: process, transcript, input draft, history and variables.
#[derive(Debug, Default)]
pub struct Console {
    pub process: Option<ConsoleProcess>,
    /// The interpreter it runs.
    pub interpreter: Option<PathBuf>,
    /// `3.12.1` once the driver is ready.
    pub version: Option<String>,
    pub ready: bool,
    /// Chunks of the transcript, consecutive chunks of one kind merged.
    pub transcript: Vec<(ConsoleKind, String)>,
    transcript_bytes: usize,
    /// The input being typed (multi-line).
    pub draft: String,
    /// Executed inputs, oldest first.
    pub history: Vec<String>,
    /// Position while browsing the history (Up/Down); `None` = editing the draft.
    pub history_pos: Option<usize>,
    /// The draft before browsing the history started.
    saved_draft: String,
    /// Code sent and not answered yet (id).
    pub running: Option<u64>,
    /// The draft that was sent (back into the draft when incomplete, else into the history).
    pub pending_input: Option<String>,
    /// The code called `input()` and waits for a line.
    pub awaiting_input: Option<String>,
    pub vars: Vec<ConsoleVar>,
    next_id: u64,
    /// The last exit message (process ended).
    pub exited: Option<String>,
    /// `frame` answers (View as DataFrame) not taken yet.
    pub frames: Vec<Value>,
}

impl Console {
    pub fn is_alive(&self) -> bool {
        self.process.is_some()
    }

    /// Start the console with `python` in `cwd` (stopping a running one).
    pub fn start(&mut self, python: &Path, cwd: Option<&Path>) -> Result<(), String> {
        self.stop();
        let mut cmd = Command::new(python);
        cmd.args(["-u", "-c", &driver()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1");
        if let Some(dir) = cwd.filter(|d| d.is_dir()) {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", python.display()))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot open the console's pipes".into());
        };
        let (in_tx, in_rx) = unbounded::<String>();
        std::thread::spawn(move || {
            for line in in_rx {
                if stdin.write_all(line.as_bytes()).is_err()
                    || stdin.write_all(b"\n").is_err()
                    || stdin.flush().is_err()
                {
                    break;
                }
            }
        });
        let (tx, rx) = unbounded();
        let out_tx = tx.clone();
        std::thread::spawn(move || read_stdout(stdout, &out_tx));
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.by_ref().take(MAX_LINE_BYTES).read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let _ = tx.send(Msg::Raw(ConsoleKind::Error, String::from_utf8_lossy(&buf).into_owned()));
                    }
                }
            }
        });
        self.process = Some(ConsoleProcess { child: Some(child), stdin: in_tx, rx });
        self.interpreter = Some(python.to_path_buf());
        self.ready = false;
        self.version = None;
        self.running = None;
        self.awaiting_input = None;
        self.exited = None;
        self.vars.clear();
        self.push(ConsoleKind::System, &format!("Starting {}…\n", python.display()));
        Ok(())
    }

    /// Kill the process (the transcript stays).
    pub fn stop(&mut self) {
        if let Some(mut p) = self.process.take() {
            p.kill();
            self.push(ConsoleKind::System, "Console stopped.\n");
        }
        self.ready = false;
        self.running = None;
        self.awaiting_input = None;
    }

    /// Interrupt running code (`KeyboardInterrupt`).
    pub fn interrupt(&mut self) -> Result<(), String> {
        let Some(p) = &self.process else { return Err("the Python Console is not running".into()) };
        interrupt_pid(p.pid())
    }

    /// Send code to run; `force` skips the incomplete-input check (Execute Selection).
    pub fn execute(&mut self, code: &str, force: bool) -> Result<u64, String> {
        if code.len() > MAX_CODE_BYTES {
            return Err(format!("the code is too large to run in the console ({} bytes)", code.len()));
        }
        let Some(p) = &self.process else { return Err("the Python Console is not running".into()) };
        if self.running.is_some() {
            return Err("the console is still running the previous input".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let msg = json!({ "op": "exec", "id": id, "code": code, "force": force }).to_string();
        p.stdin.send(msg).map_err(|_| "the Python Console has exited".to_string())?;
        self.running = Some(id);
        Ok(id)
    }

    /// Send another request to the driver (a View as DataFrame `frame` page).
    pub fn send_op(&self, msg: &Value) -> Result<(), String> {
        let Some(p) = &self.process else { return Err("the Python Console is not running".into()) };
        p.stdin.send(msg.to_string()).map_err(|_| "the Python Console has exited".to_string())
    }

    /// Answer an `input()` call.
    pub fn send_input(&mut self, text: &str) -> Result<(), String> {
        let Some(p) = &self.process else { return Err("the Python Console is not running".into()) };
        let msg = json!({ "op": "input", "text": text }).to_string();
        p.stdin.send(msg).map_err(|_| "the Python Console has exited".to_string())?;
        self.awaiting_input = None;
        self.push(ConsoleKind::Input, &format!("{text}\n"));
        Ok(())
    }

    /// Add a history entry (consecutive duplicates collapse).
    pub fn remember(&mut self, code: &str) {
        let code = code.trim_end();
        if code.trim().is_empty() || self.history.last().is_some_and(|h| h == code) {
            return;
        }
        if self.history.len() >= MAX_HISTORY {
            self.history.remove(0);
        }
        self.history.push(code.to_string());
        self.history_pos = None;
    }

    /// Up (`back`) / Down through the history into the draft.
    pub fn browse_history(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        if self.history_pos.is_none() && !back {
            return;
        }
        let next = match (self.history_pos, back) {
            (None, true) => {
                self.saved_draft = self.draft.clone();
                Some(last)
            }
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i >= last => None,
            (Some(i), false) => Some(i + 1),
        };
        self.history_pos = next;
        self.draft = match next.and_then(|i| self.history.get(i)) {
            Some(h) => h.clone(),
            None => std::mem::take(&mut self.saved_draft),
        };
    }

    /// Append to the transcript (merging with the last chunk of the same kind).
    pub fn push(&mut self, kind: ConsoleKind, text: &str) {
        if text.is_empty() {
            return;
        }
        self.transcript_bytes = self.transcript_bytes.saturating_add(text.len());
        match self.transcript.last_mut() {
            // Each input is its own chunk (an incomplete one is taken back).
            Some((k, t)) if *k == kind && kind != ConsoleKind::Input => t.push_str(text),
            _ => self.transcript.push((kind, text.to_string())),
        }
        while self.transcript_bytes > MAX_TRANSCRIPT_BYTES && self.transcript.len() > 1 {
            let (_, t) = self.transcript.remove(0);
            self.transcript_bytes = self.transcript_bytes.saturating_sub(t.len());
        }
    }

    /// Remove the last input from the transcript (it was incomplete).
    pub fn take_back_input(&mut self) {
        if let Some((ConsoleKind::Input, t)) = self.transcript.last() {
            let n = t.len();
            self.transcript.pop();
            self.transcript_bytes = self.transcript_bytes.saturating_sub(n);
        }
    }

    pub fn clear(&mut self) {
        self.transcript.clear();
        self.transcript_bytes = 0;
    }

    /// The transcript as text (for agents and tests).
    pub fn text(&self) -> String {
        self.transcript.iter().map(|(_, t)| t.as_str()).collect()
    }

    /// Drain the reader threads. Returns what finished: `Some(status)` when an input was answered.
    pub fn poll(&mut self) -> Poll {
        let mut poll = Poll::default();
        let Some(p) = &mut self.process else { return poll };
        let mut msgs = Vec::new();
        let mut closed = false;
        loop {
            match p.rx.try_recv() {
                Ok(Msg::Closed) => closed = true,
                Ok(m) => msgs.push(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
            if msgs.len() > 10_000 {
                break;
            }
        }
        // Without stdout the driver cannot answer: make sure the process is gone (killing first so
        // the wait cannot block when it closed stdout but kept running).
        let exit = if closed {
            p.end()
        } else {
            None
        };
        poll.changed = !msgs.is_empty() || closed;
        for m in msgs {
            match m {
                Msg::Raw(kind, text) => self.push(kind, &text),
                Msg::Frame(v) => self.frame(&v, &mut poll),
                Msg::Closed => {}
            }
        }
        if closed {
            let code = exit.and_then(|s| s.code());
            let msg = match code {
                Some(c) => format!("Process finished with exit code {c}"),
                None => "Process finished".to_string(),
            };
            self.push(ConsoleKind::System, &format!("\n{msg}\n"));
            self.exited = Some(msg);
            self.process = None;
            self.ready = false;
            if self.running.take().is_some() {
                poll.finished = Some("error".into());
            }
            self.awaiting_input = None;
        }
        poll
    }

    fn frame(&mut self, v: &Value, poll: &mut Poll) {
        let text = || v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        match v.get("ev").and_then(Value::as_str) {
            Some("ready") => {
                self.ready = true;
                self.version = v.get("version").and_then(Value::as_str).map(str::to_owned);
                let version = self.version.clone().unwrap_or_default();
                self.push(ConsoleKind::System, &format!("Python {version}\n"));
            }
            Some("out") => self.push(ConsoleKind::Output, &text()),
            Some("err") => self.push(ConsoleKind::Error, &text()),
            Some("frame") => {
                if self.frames.len() >= 64 {
                    self.frames.remove(0);
                }
                self.frames.push(v.clone());
            }
            Some("input") => {
                self.awaiting_input = Some(v.get("prompt").and_then(Value::as_str).unwrap_or("").to_string());
            }
            Some("done") => {
                if let Some(vars) = v.get("vars").and_then(Value::as_array) {
                    self.vars = vars.iter().filter_map(|x| serde_json::from_value(x.clone()).ok()).collect();
                }
                // An interrupt reports `done` without an id; any `done` ends the current input.
                self.running = None;
                self.awaiting_input = None;
                poll.finished = Some(v.get("status").and_then(Value::as_str).unwrap_or("ok").to_string());
            }
            _ => {}
        }
    }
}

/// What [`Console::poll`] saw.
#[derive(Debug, Default)]
pub struct Poll {
    pub changed: bool,
    /// The status of a finished input (`ok`, `error`, `incomplete`).
    pub finished: Option<String>,
}

fn read_stdout(stdout: impl Read, tx: &Sender<Msg>) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.by_ref().take(MAX_LINE_BYTES).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let msg = match parse_frame(&line) {
                    Some(v) => Msg::Frame(v),
                    None => Msg::Raw(ConsoleKind::Output, line.into_owned()),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send(Msg::Closed);
}

/// A protocol frame line → its JSON.
pub fn parse_frame(line: &str) -> Option<Value> {
    let body = line.strip_prefix(FRAME_PREFIX)?;
    serde_json::from_str(body.trim_end()).ok()
}

/// The draft after an incomplete input: a new line indented like the last one, one level deeper
/// after a `:`.
pub fn continue_draft(draft: &str) -> String {
    let last = draft.rsplit('\n').next().unwrap_or("");
    let indent: String = last.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let deeper = if last.trim_end().ends_with(':') { "    " } else { "" };
    format!("{draft}\n{indent}{deeper}")
}

/// Whether Enter should run the draft without asking the driver: its last line is blank (the
/// blank line that closes a block).
pub fn ends_block(draft: &str) -> bool {
    draft.contains('\n') && draft.rsplit('\n').next().is_some_and(|l| l.trim().is_empty())
}

/// `KeyboardInterrupt` in the console process.
pub(crate) fn interrupt_pid(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("cannot interrupt the console: {e}"))?;
        if status.success() { Ok(()) } else { Err("cannot interrupt the console".into()) }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err("interrupting the console is not supported on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn frames_and_drafts() {
        assert_eq!(parse_frame("\u{1}CCJ {\"ev\":\"out\",\"text\":\"hi\"}\n").unwrap()["text"], "hi");
        assert_eq!(parse_frame("plain output"), None);
        assert_eq!(parse_frame("\u{1}CCJ {broken"), None);
        assert_eq!(continue_draft("def f():"), "def f():\n    ");
        assert_eq!(continue_draft("def f():\n    x = 1"), "def f():\n    x = 1\n    ");
        assert_eq!(continue_draft("x = (1,"), "x = (1,\n");
        assert!(ends_block("def f():\n    return 1\n"));
        assert!(ends_block("def f():\n    return 1\n    "));
        assert!(!ends_block("x = 1"));
        assert!(!ends_block("def f():\n    return 1"));
    }

    #[test]
    fn history_browsing_restores_the_draft() {
        let mut c = Console::default();
        c.remember("a = 1");
        c.remember("a = 1");
        c.remember("b = 2\n");
        c.remember("   ");
        assert_eq!(c.history, vec!["a = 1", "b = 2"]);
        c.draft = "typing".into();
        c.browse_history(true);
        assert_eq!(c.draft, "b = 2");
        c.browse_history(true);
        c.browse_history(true);
        assert_eq!(c.draft, "a = 1");
        c.browse_history(false);
        assert_eq!(c.draft, "b = 2");
        c.browse_history(false);
        assert_eq!(c.draft, "typing");
        c.browse_history(false);
        assert_eq!(c.draft, "typing");
    }

    #[test]
    fn transcript_merges_and_is_capped() {
        let mut c = Console::default();
        c.push(ConsoleKind::Output, "a");
        c.push(ConsoleKind::Output, "b\n");
        c.push(ConsoleKind::Error, "e\n");
        c.push(ConsoleKind::Output, "");
        assert_eq!(c.transcript.len(), 2);
        assert_eq!(c.text(), "ab\ne\n");
        let big = "x".repeat(1 << 20);
        for _ in 0..5 {
            c.push(ConsoleKind::Input, &big);
            c.push(ConsoleKind::Output, &big);
        }
        assert!(c.transcript_bytes <= MAX_TRANSCRIPT_BYTES);
        c.clear();
        assert!(c.transcript.is_empty());
    }

    #[test]
    fn not_running_is_an_error() {
        let mut c = Console::default();
        assert!(c.execute("1", false).is_err());
        assert!(c.send_input("x").is_err());
        assert!(c.interrupt().is_err());
        assert!(c.execute(&"x".repeat(MAX_CODE_BYTES + 1), true).is_err());
        c.stop();
        assert!(!c.poll().changed);
    }

    fn python3() -> Option<PathBuf> {
        codecraft_lsp::which("python3")
    }

    fn wait(c: &mut Console) -> Option<String> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            let p = c.poll();
            if let Some(status) = p.finished {
                return Some(status);
            }
            if c.awaiting_input.is_some() {
                return Some("input".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn wait_ready(c: &mut Console) {
        let start = Instant::now();
        while !c.ready && start.elapsed() < Duration::from_secs(20) {
            c.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(c.ready, "console did not start: {}", c.text());
    }

    /// Stop never waits on the caller's (UI) thread for a busy console (QA M14 #20).
    #[test]
    fn stop_returns_at_once_while_code_runs() {
        let Some(py) = python3() else {
            eprintln!("python3 not installed; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Console::default();
        c.start(&py, Some(tmp.path())).unwrap();
        wait_ready(&mut c);
        c.execute("import time\nwhile True:\n    time.sleep(0.01)\n", true).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        c.stop();
        assert!(start.elapsed() < Duration::from_millis(500), "stop took {:?}", start.elapsed());
        assert!(c.process.is_none() && !c.ready);
    }

    #[test]
    fn driver_protocol_against_real_python() {
        let Some(py) = python3() else {
            eprintln!("python3 not installed; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Console::default();
        c.start(&py, Some(tmp.path())).unwrap();
        wait_ready(&mut c);
        assert!(c.version.as_deref().is_some_and(|v| v.starts_with('3')));

        // Statements, then an echoed final expression; stdout captured.
        c.execute("x = 21\nprint('hello')\nx * 2", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("ok"));
        let text = c.text();
        assert!(text.contains("hello\n") && text.contains("42\n"), "{text}");
        assert!(c.vars.iter().any(|v| v.name == "x" && v.type_name == "int" && v.value == "21"), "{:?}", c.vars);

        // Incomplete input: a block needs its closing blank line, unless forced.
        c.execute("def f(a):", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("incomplete"));
        c.execute("def f(a):\n    return a + 1", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("incomplete"));
        c.execute("def f(a):\n    return a + 1\n", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("ok"));
        c.execute("if True:\n    y = f(1)\nprint(y)", true).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("ok"));
        assert!(c.text().contains("2\n"));

        // Errors go to stderr with a traceback; syntax errors too.
        c.execute("1/0", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("error"));
        assert!(c.text().contains("ZeroDivisionError"));
        c.execute("a b", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("error"));
        assert!(c.text().contains("SyntaxError"));

        // input() round trip.
        c.execute("name = input('who? ')\nprint('hi', name)", false).unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("input"));
        assert_eq!(c.awaiting_input.as_deref(), Some("who? "));
        c.send_input("ada").unwrap();
        assert_eq!(wait(&mut c).as_deref(), Some("ok"));
        assert!(c.text().contains("hi ada"));

        // Interrupt a long loop.
        #[cfg(unix)]
        {
            c.execute("import time\nwhile True:\n    time.sleep(0.01)", true).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            c.poll();
            c.interrupt().unwrap();
            assert_eq!(wait(&mut c).as_deref(), Some("error"));
            assert!(c.text().contains("KeyboardInterrupt"));
        }

        // exit() ends the process; it is reported, not a panic.
        c.execute("exit()", false).unwrap();
        let start = Instant::now();
        while c.is_alive() && start.elapsed() < Duration::from_secs(20) {
            c.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!c.is_alive());
        assert!(c.exited.is_some());
        assert!(c.execute("1", false).is_err());

        // Restart and stop.
        c.start(&py, None).unwrap();
        wait_ready(&mut c);
        c.stop();
        assert!(!c.is_alive());
    }
}
