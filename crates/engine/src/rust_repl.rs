//! Tools › Rust › Rust REPL (M12 track RS): a tool window running `evcxr` (the Rust REPL) when it
//! is installed, an install hint otherwise. Follows the Python Console: transcript, a multi-line
//! input with history (Up/Down), Restart/Stop/Clear.
//!
//! Protocol: `evcxr --ide-mode --disable-readline`. Each input is sent as one line with its
//! newlines replaced by U+2028 (evcxr turns them back into newlines). evcxr prints the `>> `
//! prompt when it waits for input and, in IDE mode, U+0091 (success) or U+0092 (error) after each
//! input's output. Outputs (stdout) and errors (stderr) stream into the transcript; the prompt is
//! hidden. Enter runs a complete input; with unbalanced brackets or an open string it adds a line.
//!
//! Threads: a writer feeds stdin, two readers forward stdout chunks and stderr lines; the engine
//! polls in `pump`. The process is killed on Stop, Restart and shutdown.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use serde::{Deserialize, Serialize};

/// evcxr's success marker (IDE mode).
pub const SUCCESS_MARKER: char = '\u{0091}';
/// evcxr's failure marker (IDE mode).
pub const FAILURE_MARKER: char = '\u{0092}';
/// Newlines inside an input travel as U+2028.
pub const LINE_SEPARATOR: char = '\u{2028}';
const PROMPT: &str = ">> ";
const MAX_TRANSCRIPT_BYTES: usize = 2 << 20;
const MAX_HISTORY: usize = 500;
pub const MAX_CODE_BYTES: usize = 1 << 20;
const MAX_LINE_BYTES: u64 = 4 << 20;
/// Output kept in the pending partial line before it is shown anyway.
const MAX_PARTIAL: usize = 64 * 1024;

pub const INSTALL_HINT: &str = "The Rust REPL needs evcxr. Install it with `cargo install --locked evcxr_repl` \
     (see https://github.com/evcxr/evcxr), then press ▶ to start it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplKind {
    Input,
    Output,
    Error,
    System,
}

enum Msg {
    Stdout(String),
    Stderr(String),
    Closed,
}

struct Process {
    child: Child,
    stdin: Sender<String>,
    rx: Receiver<Msg>,
}

impl Process {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The REPL: process, transcript, input draft and history.
#[derive(Default)]
pub struct Repl {
    process: Option<Process>,
    /// The evcxr program (tests point this at a fake).
    pub program: Option<PathBuf>,
    /// evcxr printed its first prompt.
    pub ready: bool,
    pub transcript: Vec<(ReplKind, String)>,
    transcript_bytes: usize,
    pub draft: String,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    saved_draft: String,
    /// An input is being evaluated.
    pub running: bool,
    /// Status of the last finished input (`ok` / `error`).
    pub last_status: Option<String>,
    /// Output since the last newline (prompts are recognised here).
    partial: String,
    /// The next `>> ` is a prompt (after the welcome text or a marker).
    expect_prompt: bool,
    pub exited: Option<String>,
}

impl std::fmt::Debug for Repl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repl").field("alive", &self.is_alive()).field("ready", &self.ready).finish()
    }
}

/// The evcxr binary: on `PATH` or in `~/.cargo/bin`.
pub fn find_evcxr() -> Option<PathBuf> {
    codecraft_lsp::which("evcxr").or_else(|| {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
        let p = home.join("bin").join("evcxr");
        p.is_file().then_some(p)
    })
}

impl Repl {
    pub fn is_alive(&self) -> bool {
        self.process.is_some()
    }

    /// The program to run (override, else the installed evcxr).
    pub fn resolve_program(&self) -> Option<PathBuf> {
        self.program.clone().or_else(find_evcxr)
    }

    /// Start evcxr in `cwd` (stopping a running one).
    pub fn start(&mut self, cwd: Option<&Path>) -> Result<(), String> {
        self.stop_quietly();
        let Some(program) = self.resolve_program() else {
            self.push(ReplKind::System, &format!("{INSTALL_HINT}\n"));
            return Err(INSTALL_HINT.into());
        };
        let mut cmd = Command::new(&program);
        cmd.args(["--ide-mode", "--disable-readline"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("CARGO_TERM_COLOR", "never");
        if let Some(dir) = cwd.filter(|d| d.is_dir()) {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", program.display()))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot open the REPL's pipes".into());
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
        std::thread::spawn(move || read_chunks(stdout, &out_tx));
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.by_ref().take(MAX_LINE_BYTES).read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(Msg::Stderr(String::from_utf8_lossy(&buf).into_owned())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        self.process = Some(Process { child, stdin: in_tx, rx });
        self.ready = false;
        self.running = false;
        self.expect_prompt = true;
        self.partial.clear();
        self.exited = None;
        self.push(ReplKind::System, &format!("Starting {}…\n", program.display()));
        Ok(())
    }

    fn stop_quietly(&mut self) {
        if let Some(mut p) = self.process.take() {
            p.kill();
        }
        self.ready = false;
        self.running = false;
    }

    /// Kill the process (the transcript stays).
    pub fn stop(&mut self) {
        if self.process.is_some() {
            self.stop_quietly();
            self.push(ReplKind::System, "REPL stopped.\n");
        }
    }

    /// Send an input (any newlines travel as U+2028).
    pub fn execute(&mut self, code: &str) -> Result<(), String> {
        if code.len() > MAX_CODE_BYTES {
            return Err(format!("the input is too large for the REPL ({} bytes)", code.len()));
        }
        let Some(p) = &self.process else { return Err("the Rust REPL is not running".into()) };
        let line: String = code
            .trim_end()
            .chars()
            .map(|c| if c == '\n' { LINE_SEPARATOR } else { c })
            .filter(|c| *c != '\r')
            .collect();
        p.stdin.send(line).map_err(|_| "the Rust REPL has exited".to_string())?;
        self.running = true;
        self.last_status = None;
        let shown: String = code
            .trim_end()
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{}{l}\n", if i == 0 { ">> " } else { ".. " }))
            .collect();
        self.push(ReplKind::Input, &shown);
        Ok(())
    }

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
        if self.history.is_empty() || (self.history_pos.is_none() && !back) {
            return;
        }
        let last = self.history.len() - 1;
        let next = match (self.history_pos, back) {
            (None, _) => {
                self.saved_draft = self.draft.clone();
                Some(last)
            }
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

    pub fn push(&mut self, kind: ReplKind, text: &str) {
        if text.is_empty() {
            return;
        }
        self.transcript_bytes = self.transcript_bytes.saturating_add(text.len());
        match self.transcript.last_mut() {
            Some((k, t)) if *k == kind && kind != ReplKind::Input => t.push_str(text),
            _ => self.transcript.push((kind, text.to_string())),
        }
        while self.transcript_bytes > MAX_TRANSCRIPT_BYTES && self.transcript.len() > 1 {
            let (_, t) = self.transcript.remove(0);
            self.transcript_bytes = self.transcript_bytes.saturating_sub(t.len());
        }
    }

    pub fn clear(&mut self) {
        self.transcript.clear();
        self.transcript_bytes = 0;
    }

    /// The transcript as text (agents and tests).
    pub fn text(&self) -> String {
        self.transcript.iter().map(|(_, t)| t.as_str()).collect()
    }

    fn output_kind(&self) -> ReplKind {
        if self.ready { ReplKind::Output } else { ReplKind::System }
    }

    fn flush_partial(&mut self) {
        if !self.partial.is_empty() {
            let text = crate::runs::strip_ansi(&std::mem::take(&mut self.partial));
            self.push(self.output_kind(), &text);
        }
    }

    /// Feed stdout text: prompts are dropped, markers end the running input.
    pub fn feed_stdout(&mut self, chunk: &str) -> Option<String> {
        let mut finished = None;
        for c in chunk.chars() {
            if c == SUCCESS_MARKER || c == FAILURE_MARKER {
                self.flush_partial();
                let status = if c == SUCCESS_MARKER { "ok" } else { "error" };
                finished = Some(status.to_string());
                self.last_status = finished.clone();
                self.running = false;
                self.expect_prompt = true;
                continue;
            }
            self.partial.push(c);
            if self.expect_prompt && crate::runs::strip_ansi(&self.partial) == PROMPT {
                self.partial.clear();
                self.expect_prompt = false;
                self.ready = true;
                continue;
            }
            if c == '\n' || self.partial.len() > MAX_PARTIAL {
                self.flush_partial();
            }
        }
        finished
    }

    /// Drain the reader threads. Returns (changed, the status of a finished input).
    pub fn poll(&mut self) -> (bool, Option<String>) {
        let Some(p) = &mut self.process else { return (false, None) };
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
        let exit = if closed {
            match p.child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => {
                    let _ = p.child.kill();
                    p.child.wait().ok()
                }
            }
        } else {
            None
        };
        let changed = !msgs.is_empty() || closed;
        let mut finished = None;
        for m in msgs {
            match m {
                Msg::Stdout(t) => {
                    if let Some(f) = self.feed_stdout(&t) {
                        finished = Some(f);
                    }
                }
                Msg::Stderr(t) => self.push(ReplKind::Error, &crate::runs::strip_ansi(&t)),
                Msg::Closed => {}
            }
        }
        if closed {
            self.flush_partial();
            let msg = match exit.and_then(|s| s.code()) {
                Some(c) => format!("Process finished with exit code {c}"),
                None => "Process finished".to_string(),
            };
            self.push(ReplKind::System, &format!("\n{msg}\n"));
            self.exited = Some(msg);
            self.process = None;
            self.ready = false;
            if self.running {
                self.running = false;
                finished = Some("error".into());
            }
        }
        (changed, finished)
    }
}

fn read_chunks(stdout: impl Read, tx: &Sender<Msg>) {
    let mut reader = stdout;
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(buf.get(..n).unwrap_or_default());
                // Send the valid UTF-8 prefix; keep an incomplete trailing sequence.
                let valid = match std::str::from_utf8(&pending) {
                    Ok(_) => pending.len(),
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    Err(_) => pending.len(),
                };
                let text = String::from_utf8_lossy(pending.get(..valid).unwrap_or_default()).into_owned();
                pending.drain(..valid);
                if !text.is_empty() && tx.send(Msg::Stdout(text)).is_err() {
                    return;
                }
            }
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(Msg::Stdout(String::from_utf8_lossy(&pending).into_owned()));
    }
    let _ = tx.send(Msg::Closed);
}

/// Whether an input is complete: brackets balanced and no open string, char literal or block
/// comment (lifetimes are told apart from char literals). REPL commands (`:dep …`) always are.
pub fn is_complete(code: &str) -> bool {
    let trimmed = code.trim();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return true;
    }
    let c: Vec<char> = code.chars().collect();
    let mut depth: i64 = 0;
    let mut i = 0usize;
    while let Some(&ch) = c.get(i) {
        match ch {
            '/' if c.get(i + 1) == Some(&'/') => {
                while c.get(i).is_some_and(|x| *x != '\n') {
                    i += 1;
                }
                continue;
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                let mut nest = 1;
                i += 2;
                while nest > 0 {
                    match (c.get(i), c.get(i + 1)) {
                        (None, _) => return false,
                        (Some('*'), Some('/')) => {
                            nest -= 1;
                            i += 2;
                        }
                        (Some('/'), Some('*')) => {
                            nest += 1;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
                continue;
            }
            'r' if matches!(c.get(i + 1), Some('"' | '#'))
                && !c.get(i.wrapping_sub(1)).is_some_and(|p| p.is_alphanumeric() || *p == '_') =>
            {
                // Raw string r#"…"#.
                let mut j = i + 1;
                let mut hashes = 0;
                while c.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if c.get(j) != Some(&'"') {
                    i += 1;
                    continue;
                }
                j += 1;
                loop {
                    match c.get(j) {
                        None => return false,
                        Some('"') if (1..=hashes).all(|k| c.get(j + k) == Some(&'#')) => {
                            i = j + 1 + hashes;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                continue;
            }
            '"' => {
                i += 1;
                loop {
                    match c.get(i) {
                        None => return false,
                        Some('\\') => i += 2,
                        Some('"') => break,
                        _ => i += 1,
                    }
                }
            }
            '\'' => {
                // 'a' or '\n' is a char literal; 'a (no closing quote soon) is a lifetime.
                if c.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while c.get(i).is_some_and(|x| *x != '\'') {
                        i += 1;
                    }
                } else if c.get(i + 2) == Some(&'\'') {
                    i += 2;
                }
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth <= 0
}

/// The draft after Enter on an incomplete input: a new line indented like the last one, one
/// level deeper after an opening bracket.
pub fn continue_draft(draft: &str) -> String {
    let last = draft.rsplit('\n').next().unwrap_or("");
    let indent: String = last.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let deeper = if last.trim_end().ends_with(['{', '(', '[']) { "    " } else { "" };
    format!("{draft}\n{indent}{deeper}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completeness() {
        assert!(is_complete("let x = 1;"));
        assert!(is_complete(":dep serde = { version = \"1\""));
        assert!(!is_complete("fn f() {"));
        assert!(is_complete("fn f() {\n    1\n}"));
        assert!(is_complete("let s = \"{\";"));
        assert!(!is_complete("let s = \"abc"));
        assert!(is_complete("let c = '{';"));
        assert!(is_complete("let c = '\\'';"));
        assert!(is_complete("fn f<'a>(x: &'a str) -> &'a str { x }"));
        assert!(is_complete("// {\nlet x = 1;"));
        assert!(!is_complete("/* { */ fn f() {"));
        assert!(!is_complete("/* open"));
        assert!(is_complete("let r = r#\"{\"#;"));
        assert!(!is_complete("let r = r#\"{"));
        assert!(is_complete("for x in v { println!(\"{x}\") }"));
        assert!(is_complete(""));
        assert!(is_complete("}"));
        assert_eq!(continue_draft("fn f() {"), "fn f() {\n    ");
        assert_eq!(continue_draft("fn f() {\n    let x = (1,"), "fn f() {\n    let x = (1,\n    ");
    }

    #[test]
    fn stdout_protocol() {
        let mut r = Repl { expect_prompt: true, ..Repl::default() };
        assert_eq!(r.feed_stdout("Welcome to evcxr. For help, type :help\n>"), None);
        assert!(!r.ready);
        assert_eq!(r.feed_stdout("> "), None);
        assert!(r.ready);
        r.running = true;
        assert_eq!(r.feed_stdout("42\n\u{91}>> "), Some("ok".into()));
        assert!(!r.running && r.ready);
        assert_eq!(r.feed_stdout("partial"), None);
        assert_eq!(r.feed_stdout("\u{92}\u{1b}[33m>> \u{1b}[0m"), Some("error".into()));
        let text = r.text();
        assert!(text.contains("Welcome to evcxr"), "{text}");
        assert!(text.contains("42\npartial"), "{text}");
        assert!(!text.contains(">>"), "{text}");
        // An output line that looks like a prompt mid-run is output.
        r.running = true;
        r.feed_stdout("x\n>> y\n\u{91}");
        assert!(r.text().contains(">> y"));
    }

    #[test]
    fn history_and_errors_without_a_process() {
        let mut r = Repl::default();
        assert!(r.execute("1").is_err());
        r.remember("a");
        r.remember("a");
        r.remember("b\n");
        r.draft = "typing".into();
        r.browse_history(true);
        assert_eq!(r.draft, "b");
        r.browse_history(true);
        r.browse_history(true);
        assert_eq!(r.draft, "a");
        r.browse_history(false);
        r.browse_history(false);
        assert_eq!(r.draft, "typing");
        assert!(!r.poll().0);
        r.program = Some(PathBuf::from("/definitely/not/evcxr"));
        assert!(r.start(None).is_err());
    }
}
