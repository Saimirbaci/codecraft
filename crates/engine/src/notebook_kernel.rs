//! The notebook kernel: a persistent process of the project's Python interpreter running an
//! embedded driver ([`KERNEL_DRIVER`]), like the Python Console's (one process, one JSON message
//! per line each way), so it works with a plain interpreter (no Jupyter or ipykernel needed).
//!
//! Requests (stdin): `{"op":"exec","id":1,"code":"…"}`, `{"op":"input","text":"…"}`,
//! `{"op":"vars","id":2}`, `{"op":"frame","req":3,…}` (a DataFrame page, see
//! [`crate::dataframe`]).
//!
//! Events (stdout lines prefixed by [`FRAME_PREFIX`]): `ready {version, executable}`,
//! `start {id, count}`, `stream {id, name, text}`, `display {id, data}` (a mime bundle:
//! `text/plain`, `text/html`, `text/markdown`, `image/png`, … from the object's `_repr_*_`
//! methods), `result {id, count, data}` (the value of a final expression), `error {id, ename,
//! evalue, traceback}`, `input {prompt}`, `done {id, status, count, vars}`, `frame {…}`.
//!
//! The driver sets matplotlib's Agg backend (`MPLBACKEND`); after each cell every open figure is
//! sent as an `image/png` display and closed, and `plt.show()` does the same mid-cell. A final
//! `;` hides the cell's value (as in Jupyter). `!cmd` lines run in a shell; `%matplotlib` and a
//! few other line magics are accepted and ignored.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use serde_json::{Map, Value, json};

use crate::python_console::{ConsoleVar, parse_frame};

/// Longest stdout line read from the kernel (an image display can be large).
const MAX_LINE_BYTES: u64 = 64 << 20;
/// Largest code sent at once.
pub const MAX_CODE_BYTES: usize = 4 << 20;

/// The kernel part of the driver (run after [`crate::dataframe::INSPECT_PY`]).
pub const KERNEL_DRIVER: &str = r#"
import sys, os, json, ast, traceback, builtins, base64, io, warnings, subprocess
_P = "\x01CCJ "
_out = sys.stdout
_in = sys.stdin
_cur = [None]
_count = [0]
def _send(o):
    _out.write(_P + json.dumps(o) + "\n")
    _out.flush()
class _Stream:
    encoding = "utf-8"
    errors = "replace"
    def __init__(self, name):
        self.name = name
    def write(self, s):
        if s:
            _send({"ev": "stream", "id": _cur[0], "name": self.name, "text": str(s)})
        return len(s)
    def writelines(self, lines):
        for l in lines:
            self.write(l)
    def flush(self):
        pass
    def isatty(self):
        return False
    def fileno(self):
        raise OSError("notebook stream has no file descriptor")
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
    _send({"ev": "input", "id": _cur[0], "prompt": str(prompt)})
    while True:
        m = _read()
        if m.get("op") == "input":
            return m.get("text", "")
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
sys.stdout = _Stream("stdout")
sys.stderr = _Stream("stderr")
sys.stdin = _Stdin()
warnings.filterwarnings("ignore", message=".*non-interactive.*")
_REPRS = (("_repr_html_", "text/html"), ("_repr_markdown_", "text/markdown"), ("_repr_svg_", "image/svg+xml"),
          ("_repr_png_", "image/png"), ("_repr_jpeg_", "image/jpeg"), ("_repr_latex_", "text/latex"),
          ("_repr_json_", "application/json"))
def _bundle(obj):
    d = {}
    if not isinstance(obj, type):
        for meth, mime in _REPRS:
            f = getattr(obj, meth, None)
            if f is None or not callable(f):
                continue
            try:
                r = f()
            except Exception:
                continue
            if isinstance(r, tuple) and r:
                r = r[0]
            if r is None:
                continue
            if isinstance(r, bytes):
                if mime in ("image/png", "image/jpeg"):
                    r = base64.b64encode(r).decode("ascii")
                else:
                    r = r.decode("utf-8", "replace")
            elif mime in ("image/png", "image/jpeg") and not isinstance(r, str):
                continue
            d[mime] = r
    try:
        t = repr(obj)
    except Exception as e:
        t = "<repr failed: %s>" % type(e).__name__
    if len(t) > 1000000:
        t = t[:1000000] + "..."
    d["text/plain"] = t
    return d
def display(*objs, **kw):
    for o in objs:
        if isinstance(o, dict) and kw.get("raw"):
            _send({"ev": "display", "id": _cur[0], "data": o})
        else:
            _send({"ev": "display", "id": _cur[0], "data": _bundle(o)})
builtins.display = display
def _flush_figures():
    plt = sys.modules.get("matplotlib.pyplot")
    if plt is None:
        return
    if not getattr(plt.show, "_cc", False):
        def _show(*a, **k):
            _flush_figures()
        _show._cc = True
        try:
            plt.show = _show
        except Exception:
            pass
    try:
        nums = list(plt.get_fignums())
    except Exception:
        return
    for n in nums:
        try:
            fig = plt.figure(n)
            buf = io.BytesIO()
            fig.savefig(buf, format="png", bbox_inches="tight")
            _send({"ev": "display", "id": _cur[0], "data": {"image/png": base64.b64encode(buf.getvalue()).decode("ascii"), "text/plain": "<Figure %d>" % n}})
        except Exception as e:
            sys.stderr.write("cannot render figure %s: %s\n" % (n, e))
    try:
        plt.close("all")
    except Exception:
        pass
def _cc_sh(cmd):
    try:
        r = subprocess.run(cmd, shell=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        sys.stdout.write(r.stdout.decode("utf-8", "replace"))
    except Exception as e:
        sys.stderr.write("%s\n" % e)
_IGNORED_MAGICS = ("matplotlib", "load_ext", "config", "autoreload", "reload_ext", "precision")
def _cc_magic(line):
    name = line.lstrip("%").split(None, 1)[0] if line.strip("%").strip() else ""
    if name in _IGNORED_MAGICS:
        return
    sys.stderr.write("UsageError: the line magic %%%s is not supported by the built-in kernel\n" % name)
def _transform(src):
    out = []
    for l in src.split("\n"):
        s = l.lstrip()
        ind = l[:len(l) - len(s)]
        if s.startswith("!"):
            out.append(ind + "_cc_sh(%r)" % s[1:])
        elif s.startswith("%") and not s.startswith("%%"):
            out.append(ind + "_cc_magic(%r)" % s)
        else:
            out.append(l)
    return "\n".join(out)
_ns = {"__name__": "__main__", "__builtins__": builtins, "display": display, "_cc_sh": _cc_sh, "_cc_magic": _cc_magic}
def _vars():
    out = []
    for k, v in list(_ns.items()):
        if k.startswith("_") or k == "display" or type(v).__name__ == "module":
            continue
        out.append(_cc_var(k, v))
        if len(out) >= 500:
            break
    return out
def _error(t, v, tb):
    if t is not None and issubclass(t, SyntaxError):
        lines = traceback.format_exception_only(t, v)
    else:
        lines = traceback.format_exception(t, v, tb)
    _send({"ev": "error", "id": _cur[0], "ename": t.__name__ if t else "Error", "evalue": str(v), "traceback": [l.rstrip("\n") for l in lines]})
def _run(m):
    _cur[0] = m.get("id")
    _count[0] += 1
    n = _count[0]
    fname = "<cell %d>" % n
    _send({"ev": "start", "id": _cur[0], "count": n})
    status = "ok"
    src = m.get("code", "")
    try:
        if src.lstrip().startswith("%%"):
            raise NotImplementedError("cell magics (%%) are not supported by the built-in kernel")
        tree = ast.parse(_transform(src), fname, "exec")
        last = None
        if tree.body and isinstance(tree.body[-1], ast.Expr) and not src.rstrip().endswith(";"):
            last = tree.body.pop()
        if tree.body:
            exec(compile(ast.Module(body=tree.body, type_ignores=[]), fname, "exec"), _ns)
        if last is not None:
            v = eval(compile(ast.Expression(body=last.value), fname, "eval"), _ns)
            if v is not None:
                _ns["_"] = v
                _send({"ev": "result", "id": _cur[0], "count": n, "data": _bundle(v)})
    except KeyboardInterrupt:
        status = "error"
        _send({"ev": "error", "id": _cur[0], "ename": "KeyboardInterrupt", "evalue": "", "traceback": ["KeyboardInterrupt"]})
    except BaseException:
        status = "error"
        t, v, tb = sys.exc_info()
        tb = tb.tb_next if tb is not None else None
        _error(t, v, tb)
    try:
        _flush_figures()
    except BaseException:
        pass
    _cc_cache.clear()
    _send({"ev": "done", "id": _cur[0], "status": status, "count": n, "vars": _vars()})
    _cur[0] = None
_send({"ev": "ready", "version": "%d.%d.%d" % sys.version_info[:3], "executable": sys.executable})
while True:
    try:
        m = _pending.pop(0) if _pending else _read()
        op = m.get("op")
        if op == "exec":
            _run(m)
        elif op == "vars":
            _send({"ev": "done", "id": m.get("id"), "status": "ok", "count": _count[0], "vars": _vars()})
        elif op == "frame":
            _send(_cc_frame(m, _ns))
    except KeyboardInterrupt:
        _send({"ev": "done", "id": _cur[0], "status": "error", "count": _count[0], "vars": _vars()})
        _cur[0] = None
    except SystemExit:
        break
"#;

/// The full driver script (`python -u -c <this>`).
pub fn driver() -> String {
    format!("{}\n{}", crate::dataframe::INSPECT_PY, KERNEL_DRIVER)
}

/// What the kernel reported.
#[derive(Clone, Debug, PartialEq)]
pub enum KernelEvent {
    Ready { version: String },
    Start { id: u64, count: u64 },
    Stream { id: Option<u64>, name: String, text: String },
    Display { id: Option<u64>, data: Map<String, Value> },
    Result { id: Option<u64>, count: u64, data: Map<String, Value> },
    Error { id: Option<u64>, ename: String, evalue: String, traceback: Vec<String> },
    Input { prompt: String },
    Done { id: Option<u64>, status: String, count: u64, vars: Option<Vec<ConsoleVar>> },
    /// A DataFrame page (the driver's `frame` answer, as sent).
    Frame(Value),
    /// A line the process wrote outside the protocol (C extensions, a crashing interpreter).
    Raw { stderr: bool, text: String },
    /// The process ended.
    Exited { code: Option<i32> },
}

enum Msg {
    Frame(Value),
    Raw(bool, String),
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

/// A kernel: the process and what is known about it.
#[derive(Default)]
pub struct Kernel {
    process: Option<Process>,
    /// The interpreter it runs.
    pub interpreter: Option<PathBuf>,
    /// `3.12.1` once ready.
    pub version: Option<String>,
    pub ready: bool,
    /// The execution request being answered.
    pub running: Option<u64>,
    next_id: u64,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("pid", &self.pid())
            .field("ready", &self.ready)
            .field("running", &self.running)
            .finish()
    }
}

impl Kernel {
    pub fn is_alive(&self) -> bool {
        self.process.is_some()
    }

    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(|p| p.child.id())
    }

    /// Start the kernel with `python` in `cwd` (stopping a running one).
    pub fn start(&mut self, python: &Path, cwd: Option<&Path>) -> Result<(), String> {
        self.stop();
        let mut cmd = Command::new(python);
        cmd.args(["-u", "-c", &driver()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1")
            .env("MPLBACKEND", "Agg");
        if let Some(dir) = cwd.filter(|d| d.is_dir()) {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", python.display()))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot open the kernel's pipes".into());
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
        std::thread::spawn(move || read_lines(stdout, &out_tx, false));
        std::thread::spawn(move || read_lines(stderr, &tx, true));
        self.process = Some(Process { child, stdin: in_tx, rx });
        self.interpreter = Some(python.to_path_buf());
        self.ready = false;
        self.version = None;
        self.running = None;
        Ok(())
    }

    /// Kill the process.
    pub fn stop(&mut self) {
        if let Some(mut p) = self.process.take() {
            p.kill();
        }
        self.ready = false;
        self.running = None;
    }

    /// `KeyboardInterrupt` in the running cell.
    pub fn interrupt(&mut self) -> Result<(), String> {
        let pid = self.pid().ok_or("the kernel is not running")?;
        crate::python_console::interrupt_pid(pid)
    }

    fn send(&self, msg: &Value) -> Result<(), String> {
        let p = self.process.as_ref().ok_or("the kernel is not running")?;
        p.stdin.send(msg.to_string()).map_err(|_| "the kernel has exited".to_string())
    }

    /// Run code; returns the request id.
    pub fn execute(&mut self, code: &str) -> Result<u64, String> {
        if code.len() > MAX_CODE_BYTES {
            return Err(format!("the cell is too large to run ({} bytes)", code.len()));
        }
        if self.running.is_some() {
            return Err("the kernel is busy".into());
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.send(&json!({ "op": "exec", "id": id, "code": code }))?;
        self.running = Some(id);
        Ok(id)
    }

    /// Answer an `input()` call.
    pub fn send_input(&mut self, text: &str) -> Result<(), String> {
        self.send(&json!({ "op": "input", "text": text }))
    }

    /// Send another request (`frame`, `vars`).
    pub fn request(&self, msg: &Value) -> Result<(), String> {
        self.send(msg)
    }

    /// Drain what the kernel reported.
    pub fn poll(&mut self) -> Vec<KernelEvent> {
        let mut events = Vec::new();
        let Some(p) = &mut self.process else { return events };
        let mut closed = false;
        let mut msgs = Vec::new();
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
        let code = if closed {
            match p.child.try_wait() {
                Ok(Some(status)) => status.code(),
                _ => {
                    let _ = p.child.kill();
                    p.child.wait().ok().and_then(|s| s.code())
                }
            }
        } else {
            None
        };
        for m in msgs {
            match m {
                Msg::Raw(stderr, text) => events.push(KernelEvent::Raw { stderr, text }),
                Msg::Frame(v) => {
                    if let Some(e) = self_event(&v) {
                        events.push(e);
                    }
                }
                Msg::Closed => {}
            }
        }
        for e in &events {
            match e {
                KernelEvent::Ready { version } => {
                    self.ready = true;
                    self.version = Some(version.clone());
                }
                KernelEvent::Done { .. } => self.running = None,
                _ => {}
            }
        }
        if closed {
            self.process = None;
            self.ready = false;
            self.running = None;
            events.push(KernelEvent::Exited { code });
        }
        events
    }
}

/// A protocol frame → its event.
pub fn self_event(v: &Value) -> Option<KernelEvent> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let id = v.get("id").and_then(Value::as_u64);
    let count = v.get("count").and_then(Value::as_u64).unwrap_or(0);
    let data = || v.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
    Some(match v.get("ev").and_then(Value::as_str)? {
        "ready" => KernelEvent::Ready { version: s("version") },
        "start" => KernelEvent::Start { id: id?, count },
        "stream" => KernelEvent::Stream { id, name: s("name"), text: s("text") },
        "display" => KernelEvent::Display { id, data: data() },
        "result" => KernelEvent::Result { id, count, data: data() },
        "error" => KernelEvent::Error {
            id,
            ename: s("ename"),
            evalue: s("evalue"),
            traceback: v
                .get("traceback")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
        },
        "input" => KernelEvent::Input { prompt: s("prompt") },
        "done" => KernelEvent::Done {
            id,
            status: v.get("status").and_then(Value::as_str).unwrap_or("ok").to_string(),
            count,
            vars: v
                .get("vars")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| serde_json::from_value(x.clone()).ok()).collect()),
        },
        "frame" => KernelEvent::Frame(v.clone()),
        _ => return None,
    })
}

fn read_lines(stream: impl Read, tx: &Sender<Msg>, stderr: bool) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.by_ref().take(MAX_LINE_BYTES).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let msg = match (stderr, parse_frame(&line)) {
                    (false, Some(v)) => Msg::Frame(v),
                    _ => Msg::Raw(stderr, line.into_owned()),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
    }
    if !stderr {
        let _ = tx.send(Msg::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn frames_become_events() {
        assert_eq!(
            self_event(&json!({"ev": "stream", "id": 3, "name": "stdout", "text": "hi"})),
            Some(KernelEvent::Stream { id: Some(3), name: "stdout".into(), text: "hi".into() })
        );
        assert_eq!(self_event(&json!({"ev": "start"})), None);
        assert_eq!(self_event(&json!({"ev": "nope"})), None);
        assert_eq!(self_event(&json!(42)), None);
        let Some(KernelEvent::Done { vars, status, .. }) =
            self_event(&json!({"ev": "done", "id": 1, "vars": [{"name": "x", "type": "int", "value": "1"}, 7]}))
        else {
            panic!()
        };
        assert_eq!(status, "ok");
        assert_eq!(vars.unwrap().len(), 1);
    }

    #[test]
    fn not_running_is_an_error() {
        let mut k = Kernel::default();
        assert!(k.execute("1").is_err());
        assert!(k.send_input("x").is_err());
        assert!(k.interrupt().is_err());
        assert!(k.request(&json!({})).is_err());
        assert!(k.poll().is_empty());
        k.stop();
        let mut k = Kernel { running: Some(1), ..Kernel::default() };
        assert!(k.execute("1").is_err());
        assert!(k.execute(&"x".repeat(MAX_CODE_BYTES + 1)).is_err());
    }

    fn collect(k: &mut Kernel, until: impl Fn(&[KernelEvent]) -> bool) -> Vec<KernelEvent> {
        let mut all = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            all.extend(k.poll());
            if until(&all) {
                return all;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out: {all:?}");
    }

    fn done(events: &[KernelEvent]) -> bool {
        events.iter().any(|e| matches!(e, KernelEvent::Done { .. }))
    }

    #[test]
    fn driver_protocol_against_real_python() {
        let Some(py) = codecraft_lsp::which("python3") else {
            eprintln!("python3 not installed; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut k = Kernel::default();
        k.start(&py, Some(tmp.path())).unwrap();
        collect(&mut k, |e| e.iter().any(|e| matches!(e, KernelEvent::Ready { .. })));
        assert!(k.ready);

        k.execute("x = 21\nprint('hi')\nx * 2").unwrap();
        let ev = collect(&mut k, done);
        assert!(ev.contains(&KernelEvent::Stream { id: Some(1), name: "stdout".into(), text: "hi".into() }), "{ev:?}");
        let result = ev.iter().find_map(|e| match e {
            KernelEvent::Result { data, count, .. } => Some((data.clone(), *count)),
            _ => None,
        });
        let (data, count) = result.unwrap();
        assert_eq!(count, 1);
        assert_eq!(data["text/plain"], "42");
        let Some(KernelEvent::Done { vars: Some(vars), status, .. }) = ev.iter().find(|e| matches!(e, KernelEvent::Done { .. })) else { panic!() };
        assert_eq!(status, "ok");
        assert!(vars.iter().any(|v| v.name == "x" && v.value == "21"), "{vars:?}");

        // A final `;` hides the value; display() and _repr_html_ make rich output.
        k.execute("class H:\n    def _repr_html_(self):\n        return '<b>h</b>'\ndisplay(H())\n5;").unwrap();
        let ev = collect(&mut k, done);
        assert!(!ev.iter().any(|e| matches!(e, KernelEvent::Result { .. })));
        let disp = ev.iter().find_map(|e| match e {
            KernelEvent::Display { data, .. } => Some(data.clone()),
            _ => None,
        });
        assert_eq!(disp.unwrap()["text/html"], "<b>h</b>");

        // Errors carry a traceback; the count keeps going.
        k.execute("def f():\n    raise ValueError('bad')\nf()").unwrap();
        let ev = collect(&mut k, done);
        let err = ev.iter().find_map(|e| match e {
            KernelEvent::Error { ename, evalue, traceback, .. } => Some((ename.clone(), evalue.clone(), traceback.clone())),
            _ => None,
        });
        let (ename, evalue, tb) = err.unwrap();
        assert_eq!((ename.as_str(), evalue.as_str()), ("ValueError", "bad"));
        assert!(tb.iter().any(|l| l.contains("<cell 3>")), "{tb:?}");
        assert!(ev.iter().any(|e| matches!(e, KernelEvent::Done { status, count: 3, .. } if status == "error")));
        // Syntax errors too.
        k.execute("a b").unwrap();
        let ev = collect(&mut k, done);
        assert!(ev.iter().any(|e| matches!(e, KernelEvent::Error { ename, .. } if ename == "SyntaxError")));

        // input() round trip; shell lines and ignored magics.
        k.execute("%matplotlib inline\nname = input('who? ')\nprint('hello', name)").unwrap();
        collect(&mut k, |e| e.iter().any(|e| matches!(e, KernelEvent::Input { .. })));
        k.send_input("ada").unwrap();
        let ev = collect(&mut k, done);
        assert!(ev.iter().any(|e| matches!(e, KernelEvent::Stream { text, .. } if text == "hello")), "{ev:?}");
        assert!(!ev.iter().any(|e| matches!(e, KernelEvent::Stream { name, .. } if name == "stderr")), "{ev:?}");
        #[cfg(unix)]
        {
            k.execute("!echo from-shell").unwrap();
            let ev = collect(&mut k, done);
            assert!(ev.iter().any(|e| matches!(e, KernelEvent::Stream { text, .. } if text.contains("from-shell"))), "{ev:?}");
        }

        // Interrupt a long loop: the kernel survives.
        #[cfg(unix)]
        {
            k.execute("import time\nwhile True:\n    time.sleep(0.01)").unwrap();
            std::thread::sleep(Duration::from_millis(300));
            k.interrupt().unwrap();
            let ev = collect(&mut k, done);
            assert!(ev.iter().any(|e| matches!(e, KernelEvent::Error { ename, .. } if ename == "KeyboardInterrupt")), "{ev:?}");
            k.execute("1 + 1").unwrap();
            let ev = collect(&mut k, done);
            assert!(ev.iter().any(|e| matches!(e, KernelEvent::Result { .. })));
        }

        // A dying kernel is an event, not a panic.
        k.execute("import os\nos._exit(3)").unwrap();
        let ev = collect(&mut k, |e| e.iter().any(|e| matches!(e, KernelEvent::Exited { .. })));
        assert!(ev.contains(&KernelEvent::Exited { code: Some(3) }), "{ev:?}");
        assert!(!k.is_alive());
        assert!(k.execute("1").is_err());
    }

    #[test]
    fn matplotlib_figures_become_png_displays() {
        let Some(py) = codecraft_lsp::which("python3") else { return };
        let probe = Command::new(&py).args(["-c", "import matplotlib"]).output();
        if !probe.is_ok_and(|o| o.status.success()) {
            eprintln!("matplotlib not installed; skipping");
            return;
        }
        let mut k = Kernel::default();
        k.start(&py, None).unwrap();
        collect(&mut k, |e| e.iter().any(|e| matches!(e, KernelEvent::Ready { .. })));
        k.execute("import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])\nplt.show()\nplt.figure()\nplt.plot([3, 1]);").unwrap();
        let ev = collect(&mut k, done);
        let pngs: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                KernelEvent::Display { data, .. } => data.get("image/png").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect();
        assert_eq!(pngs.len(), 2, "{ev:?}");
        let bytes = crate::notebook::decode_base64(&pngs[0]).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
        assert!(!ev.iter().any(|e| matches!(e, KernelEvent::Stream { name, .. } if name == "stderr")), "{ev:?}");
        k.stop();
    }
}
