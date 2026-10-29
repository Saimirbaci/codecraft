//! Command-line handoff: `codecraft <path>` opens the path in the running CodeCraft instead of
//! starting a second one.
//!
//! The running app listens on a loopback TCP port and writes `<dir>/handoff.json` (owner-only)
//! with its port and a random token. A later `codecraft` process reads that file, connects and
//! sends one JSON line (`{"token": …, "request": {…}}`); the app answers `{"ok": true}` once it
//! has queued the request. No file, a dead port, a wrong token or any other failure means "no
//! running instance": the caller starts normally.
//!
//! The wire format is one request per connection, bounded in size and time, and only accepted
//! with the token, so a web page that posts to the port gets nothing.
//!
//! [`parse_args`] is the one command-line parser (launcher, app and tests share it): paths
//! (`path`, `path:line`, `path:line:col`) resolved against the caller's working directory, and the
//! few flags that matter to a handoff.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The file in the CodeCraft home directory that names the running instance's port and token.
pub const ENDPOINT_FILE: &str = "handoff.json";

/// Longest request line the app reads.
const MAX_LINE: u64 = 256 * 1024;
/// Most targets in one request.
pub const MAX_TARGETS: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// One path to open, with an optional position (1-based line and column).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

/// What a `codecraft` invocation asks the running app to do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    /// Absolute paths, in the order given. Empty: just bring the app to the front.
    #[serde(default)]
    pub targets: Vec<Target>,
    /// `-r`: open folders in the current window instead of a new one.
    #[serde(default)]
    pub reuse_window: bool,
}

/// A parsed command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub request: OpenRequest,
    /// `--no-restore`: start fresh (never handed to a running instance).
    pub no_restore: bool,
    /// Options this parser does not know (`--mcp-bridge`, `--version`, …).
    pub unknown: Vec<OsString>,
}

impl Parsed {
    /// Whether a running instance can take over: only paths and the window flags were given.
    pub fn forwardable(&self) -> bool {
        !self.no_restore && self.unknown.is_empty()
    }
}

/// Parse the arguments after the program name. `cwd` resolves relative paths.
///
/// - `-r` / `--reuse-window`: folders open in the current window; `-n` / `--new-window`: the
///   default, accepted for symmetry.
/// - `--no-restore`: [`Parsed::no_restore`].
/// - `--`: everything after it is a path.
/// - any other `-x` / `--x`: [`Parsed::unknown`].
/// - everything else is a path, `path:line` or `path:line:col`.
pub fn parse_args(args: &[OsString], cwd: &Path) -> Parsed {
    let mut parsed = Parsed::default();
    let mut paths_only = false;
    for arg in args {
        let flag = if paths_only { None } else { arg.to_str().filter(|a| a.starts_with('-') && *a != "-") };
        match flag {
            Some("--") => paths_only = true,
            Some("-r" | "--reuse-window") => parsed.request.reuse_window = true,
            Some("-n" | "--new-window") => parsed.request.reuse_window = false,
            Some("--no-restore") => parsed.no_restore = true,
            Some(_) => parsed.unknown.push(arg.clone()),
            None => {
                if parsed.request.targets.len() < MAX_TARGETS {
                    parsed.request.targets.push(parse_target(arg, cwd));
                }
            }
        }
    }
    parsed
}

/// `path`, `path:line` or `path:line:col`, made absolute against `cwd`. A trailing `:number`
/// is a position only when the whole argument is not an existing path.
pub fn parse_target(arg: &OsString, cwd: &Path) -> Target {
    let whole = resolve(Path::new(arg), cwd);
    let Some(text) = arg.to_str() else { return Target { path: whole, line: None, column: None } };
    if whole.exists() {
        return Target { path: whole, line: None, column: None };
    }
    let (rest, last) = split_number(text);
    let Some(last) = last else { return Target { path: whole, line: None, column: None } };
    let (rest2, prev) = split_number(rest);
    let (path_text, line, column) = match prev {
        Some(line) => (rest2, Some(line), Some(last)),
        None => (rest, Some(last), None),
    };
    if path_text.is_empty() {
        return Target { path: whole, line: None, column: None };
    }
    Target { path: resolve(Path::new(path_text), cwd), line, column }
}

/// `"a.rs:12"` → (`"a.rs"`, `Some(12)`); no trailing `:digits` → (`text`, `None`).
fn split_number(text: &str) -> (&str, Option<usize>) {
    match text.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => match tail.parse() {
            Ok(n) => (head, Some(n)),
            Err(_) => (text, None),
        },
        _ => (text, None),
    }
}

/// `path` made absolute against `cwd` with `.` and `..` folded away (no symlink resolution and
/// no file system access).
pub fn resolve(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------- the running app

#[derive(Serialize, Deserialize)]
struct Endpoint {
    pid: u32,
    port: u16,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    request: OpenRequest,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The listener of the running app. Dropping it stops the thread and removes the endpoint file.
pub struct Server {
    dir: PathBuf,
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Server {
    /// Listen on a loopback port and publish it in `<dir>/handoff.json`. `wake` is called after
    /// each queued request (the UI uses it to repaint). Requests arrive on the returned receiver.
    pub fn start(dir: &Path, wake: impl Fn() + Send + 'static) -> std::io::Result<(Self, Receiver<OpenRequest>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let token = new_token();
        write_endpoint(dir, &Endpoint { pid: std::process::id(), port, token: token.clone() })?;
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, token) = (Arc::clone(&stop), token.clone());
            std::thread::Builder::new().name("handoff".into()).spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = conn else { continue };
                    if let Some(request) = handle(stream, &token) {
                        if tx.send(request).is_err() {
                            break;
                        }
                        wake();
                    }
                }
            })?
        };
        let server = Self { dir: dir.to_path_buf(), port, token, stop, thread: Mutex::new(Some(thread)) };
        Ok((server, rx))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop listening and remove the endpoint file (if it is still ours).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), CONNECT_TIMEOUT);
        let thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        let path = self.dir.join(ENDPOINT_FILE);
        if read_endpoint(&path).is_some_and(|e| e.token == self.token) {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read one request from `stream`, answer it, and return it if it was valid.
fn handle(stream: TcpStream, token: &str) -> Option<OpenRequest> {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new(Read::take(&stream, MAX_LINE)).read_line(&mut line);
    let outcome = match read {
        Ok(n) if n > 0 && line.ends_with('\n') => match serde_json::from_str::<Envelope>(&line) {
            Ok(env) if same(&env.token, token) && env.request.targets.len() <= MAX_TARGETS => Ok(env.request),
            Ok(_) => Err("rejected"),
            Err(_) => Err("malformed request"),
        },
        _ => Err("incomplete request"),
    };
    let reply = match &outcome {
        Ok(_) => Reply { ok: true, error: None },
        Err(why) => Reply { ok: false, error: Some((*why).to_string()) },
    };
    if let Ok(text) = serde_json::to_string(&reply) {
        let _ = writeln!(&stream, "{text}");
    }
    outcome.ok()
}

/// Constant-time equality for the token.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 128 random bits as hex, from the standard library's randomly keyed hasher.
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut out = String::with_capacity(32);
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn read_endpoint(path: &Path) -> Option<Endpoint> {
    let small = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= 4096);
    if !small {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_endpoint(dir: &Path, endpoint: &Endpoint) -> std::io::Result<()> {
    create_private_dir(dir)?;
    let json = serde_json::to_vec(endpoint).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(".{ENDPOINT_FILE}.{}.tmp", std::process::id()));
    let result = write_private(&tmp, &json).and_then(|()| std::fs::rename(&tmp, dir.join(ENDPOINT_FILE)));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

// ---------------------------------------------------------------- the caller

/// Send `request` to the instance published in `<dir>/handoff.json`. `true`: it took the request,
/// the caller should exit. `false`: no running instance answered; start normally.
pub fn send(dir: &Path, request: &OpenRequest) -> bool {
    let Some(endpoint) = read_endpoint(&dir.join(ENDPOINT_FILE)) else { return false };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let Ok(stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else { return false };
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let envelope = Envelope { token: endpoint.token, request: request.clone() };
    let Ok(text) = serde_json::to_string(&envelope) else { return false };
    if writeln!(&stream, "{text}").is_err() {
        return false;
    }
    let mut reply = String::new();
    if BufReader::new(Read::take(&stream, 4096)).read_line(&mut reply).is_err() {
        return false;
    }
    serde_json::from_str::<Reply>(&reply).is_ok_and(|r| r.ok)
}

/// Parse `args` like the app does and hand them to a running instance (see [`send`]). `false`
/// when the arguments need a fresh process (`--no-restore`, unknown options) or nobody answered.
pub fn try_forward(dir: &Path, args: &[OsString]) -> bool {
    let Ok(cwd) = std::env::current_dir() else { return false };
    let parsed = parse_args(args, &cwd);
    parsed.forwardable() && send(dir, &parsed.request)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn paths_are_made_absolute_and_flags_are_read() {
        let p = parse_args(&os(&["-r", "src/../a.rs", "/abs/dir", "--", "-odd"]), Path::new("/work"));
        assert!(p.request.reuse_window && p.forwardable());
        let paths: Vec<_> = p.request.targets.iter().map(|t| t.path.clone()).collect();
        assert_eq!(paths, [PathBuf::from("/work/a.rs"), PathBuf::from("/abs/dir"), PathBuf::from("/work/-odd")]);
        let q = parse_args(&os(&["--no-restore", "x"]), Path::new("/w"));
        assert!(q.no_restore && !q.forwardable());
        let r = parse_args(&os(&["--mcp-bridge", "--port", "9"]), Path::new("/w"));
        assert!(!r.forwardable() && r.unknown == os(&["--mcp-bridge", "--port"]));
        assert!(parse_args(&[], Path::new("/w")).forwardable(), "no arguments just raises the app");
        assert!(!parse_args(&os(&["-n"]), Path::new("/w")).request.reuse_window);
    }

    #[test]
    fn line_and_column_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(cwd.join("a.rs"), "x").unwrap();
        std::fs::write(cwd.join("odd:7"), "x").unwrap();
        let t = |a: &str| parse_target(&OsString::from(a), cwd);
        assert_eq!(t("a.rs"), Target { path: cwd.join("a.rs"), line: None, column: None });
        assert_eq!((t("a.rs:12").line, t("a.rs:12").column), (Some(12), None));
        let both = t("a.rs:12:5");
        assert_eq!((both.path, both.line, both.column), (cwd.join("a.rs"), Some(12), Some(5)));
        // A file that really is called `odd:7` wins over the suffix reading.
        assert_eq!(t("odd:7"), Target { path: cwd.join("odd:7"), line: None, column: None });
        // Nothing numeric, or only a number, is not a position.
        assert_eq!(t("a.rs:x").line, None);
        assert_eq!(t(":9").line, None);
        assert_eq!(t("a.rs:99999999999999999999999").line, None);
        assert_eq!(t("new.rs:3").path, cwd.join("new.rs"));
    }

    #[test]
    fn a_request_reaches_the_server_and_gets_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let woke = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&woke);
        let (server, rx) = Server::start(dir.path(), move || {
            w.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let request = OpenRequest {
            targets: vec![Target { path: "/p/a.rs".into(), line: Some(3), column: None }],
            reuse_window: true,
        };
        assert!(send(dir.path(), &request));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), request);
        assert_eq!(woke.load(Ordering::SeqCst), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(ENDPOINT_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the token file is owner-only");
        }
        drop(server);
        assert!(!dir.path().join(ENDPOINT_FILE).exists(), "the endpoint goes away with the server");
        assert!(!send(dir.path(), &request), "nobody is listening any more");
    }

    #[test]
    fn bad_tokens_and_garbage_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (server, rx) = Server::start(dir.path(), || {}).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
        let ask = |line: &str| {
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            writeln!(&stream, "{line}").unwrap();
            let mut reply = String::new();
            BufReader::new(&stream).read_line(&mut reply).unwrap();
            serde_json::from_str::<Reply>(&reply).unwrap()
        };
        assert!(!ask(r#"{"token":"wrong","request":{"targets":[]}}"#).ok);
        assert!(!ask("not json").ok);
        assert!(!ask("GET / HTTP/1.1").ok);
        assert!(rx.try_recv().is_err(), "nothing was queued");
        // The server still works afterwards.
        assert!(send(dir.path(), &OpenRequest::default()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), OpenRequest::default());
    }

    #[test]
    fn oversized_and_unterminated_requests_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (server, rx) = Server::start(dir.path(), || {}).unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
        let huge = TcpStream::connect(addr).unwrap();
        let junk = vec![b'a'; (MAX_LINE as usize) + 1024];
        let _ = (&huge).write_all(&junk);
        let _ = huge.shutdown(std::net::Shutdown::Write);
        let mut reply = String::new();
        let _ = BufReader::new(&huge).read_line(&mut reply);
        assert!(!reply.contains("\"ok\":true"));
        // A client that connects and says nothing does not wedge a later request for long.
        assert!(rx.try_recv().is_err());
        assert!(send(dir.path(), &OpenRequest::default()));
    }

    #[test]
    fn stale_or_missing_endpoints_mean_no_instance() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!send(dir.path(), &OpenRequest::default()));
        // A port nobody listens on: bind, note the port, close.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        write_endpoint(dir.path(), &Endpoint { pid: 1, port, token: "t".into() }).unwrap();
        assert!(!send(dir.path(), &OpenRequest::default()));
        std::fs::write(dir.path().join(ENDPOINT_FILE), b"{not json").unwrap();
        assert!(!send(dir.path(), &OpenRequest::default()));
        std::fs::write(dir.path().join(ENDPOINT_FILE), vec![b' '; 10_000]).unwrap();
        assert!(!send(dir.path(), &OpenRequest::default()));
    }

    #[test]
    fn a_newer_instance_keeps_its_endpoint_when_an_older_one_stops() {
        let dir = tempfile::tempdir().unwrap();
        let (old, _rx_old) = Server::start(dir.path(), || {}).unwrap();
        let (new, rx_new) = Server::start(dir.path(), || {}).unwrap();
        drop(old);
        assert!(send(dir.path(), &OpenRequest::default()), "the newer instance is still reachable");
        assert!(rx_new.recv_timeout(Duration::from_secs(2)).is_ok());
        drop(new);
    }

    #[test]
    fn try_forward_skips_options_that_need_a_new_process() {
        let dir = tempfile::tempdir().unwrap();
        let (_server, rx) = Server::start(dir.path(), || {}).unwrap();
        assert!(!try_forward(dir.path(), &os(&["--no-restore"])));
        assert!(!try_forward(dir.path(), &os(&["--version"])));
        assert!(rx.try_recv().is_err());
        assert!(try_forward(dir.path(), &os(&["/some/dir"])));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().targets[0].path, PathBuf::from("/some/dir"));
    }
}
