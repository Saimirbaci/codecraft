//! Shell scripts that run on a run target (an SSH host or a Docker container) when CodeCraft
//! runs, debugs, tests or profiles there (M16 track RT).
//!
//! Pure text, like the rest of this crate: the engine hands these scripts to `ssh … sh -c '…'`
//! or `docker exec|run … sh -c '…'`. Every word that comes from a configuration is quoted.
//!
//! The scripts talk back through marker lines on their output:
//!
//! - [`ROOT_MARK`]`/abs/path`: the project folder's absolute path on the target (a deployment
//!   mapping may say `~/app`), for debugger path mappings and profile paths.
//! - [`LISTEN_MARK`]` <port>`: a debug server listens (the forwarded port is useless before:
//!   `ssh -L` and Docker's port proxy accept connections while nothing listens behind them).
//! - [`MISSING_MARK`]`<message>`: a tool is missing on the target; the message says what to
//!   install there (CodeCraft never installs anything on a target by itself).

use crate::quote::{is_env_name, quote_path, sh_quote};
use crate::{RemoteError, RemoteResult};

/// The project folder's absolute path on the target.
pub const ROOT_MARK: &str = "CODECRAFT-REMOTE-ROOT=";
/// A debug server is listening.
pub const LISTEN_MARK: &str = "CODECRAFT-LISTENING";
/// A tool is missing on the target.
pub const MISSING_MARK: &str = "CODECRAFT-MISSING: ";
/// Which gdb-remote server a native debug session uses (`lldb-server`, `gdbserver`, `debugserver`).
pub const SERVER_MARK: &str = "CODECRAFT-DEBUG-SERVER=";

/// The marker's value in a line of output (`None` when the line has no such marker).
pub fn marker<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    let at = line.find(mark)?;
    line.get(at + mark.len()..).map(str::trim)
}

/// `cd <dir> || exit 1; <cargo env>; export K='v'; …` — the start of every script.
pub fn prelude(dir: &str, env: &[(String, String)]) -> RemoteResult<String> {
    let mut out = format!("cd {} || exit 1; ", quote_path(dir));
    out.push_str("if [ -f \"$HOME/.cargo/env\" ]; then . \"$HOME/.cargo/env\"; fi; ");
    for (k, v) in env {
        if !is_env_name(k) {
            return Err(RemoteError::Invalid(format!("invalid environment variable name `{k}`")));
        }
        out.push_str(&format!("export {k}={}; ", sh_quote(v)));
    }
    Ok(out)
}

/// Print [`ROOT_MARK`] with the absolute path of `root`.
pub fn print_root(root: &str) -> String {
    format!("printf '%s%s\\n' '{ROOT_MARK}' \"$(cd {} 2>/dev/null && pwd)\"; ", quote_path(root))
}

/// Report a missing tool (stderr) and stop with status 127.
pub fn missing(message: &str) -> String {
    format!("echo {} >&2; exit 127", sh_quote(&format!("{MISSING_MARK}{message}")))
}

/// Stop with [`missing`] unless `program` is on the target's `PATH` (or is an executable path).
pub fn require_program(program: &str, message: &str) -> String {
    format!("command -v {} >/dev/null 2>&1 || {{ {}; }}; ", sh_quote(program), missing(message))
}

/// Stop with [`missing`] unless `python` can import `module`.
pub fn require_python_module(python: &str, module: &str, message: &str) -> String {
    format!(
        "{} -c {} >/dev/null 2>&1 || {{ {}; }}; ",
        sh_quote(python),
        sh_quote(&format!("import {module}")),
        missing(message)
    )
}

/// What to tell the user when the interpreter is missing on `target`.
pub fn python_missing_message(python: &str, target: &str) -> String {
    format!(
        "`{python}` was not found on {target}. Install Python there (Debian/Ubuntu: `apt-get install -y python3 \
         python3-pip`) or choose the target's interpreter (Settings › Python › Remote interpreters)"
    )
}

/// What to tell the user when debugpy is missing for `python` on `target`.
pub fn debugpy_missing_message(python: &str, target: &str) -> String {
    format!("debugpy is not installed for `{python}` on {target}. Install it there: `{python} -m pip install debugpy`")
}

/// What to tell the user when no gdb-remote server is on `target`.
pub fn debug_server_missing_message(target: &str) -> String {
    format!(
        "no gdb-remote debug server on {target}: install gdbserver (Debian/Ubuntu: `apt-get install -y gdbserver`) \
         or lldb-server (`apt-get install -y lldb`) there"
    )
}

/// What to tell the user when py-spy is missing.
pub fn py_spy_missing_message(python: &str, target: &str) -> String {
    format!("py-spy is not installed on {target}. Install it there: `{python} -m pip install py-spy`")
}

/// What to tell the user when no native profiler is on `target`.
pub fn native_profiler_missing_message(target: &str) -> String {
    format!(
        "no native profiler on {target}: install samply (`cargo install --locked samply`) or perf (Debian: \
         `apt-get install -y linux-perf`, Ubuntu: `apt-get install -y linux-tools-$(uname -r)`) there"
    )
}

/// A shell function `cc_listening PORT` that succeeds when something listens on TCP `PORT`,
/// without connecting to it (a gdb-remote stub serves a single connection): `/proc/net/tcp`
/// on Linux, else `ss`, `netstat` or `lsof`. `cc_can_check` tells whether any of them exists.
const LISTENING_FN: &str = r#"cc_can_check() { [ -r /proc/net/tcp ] || command -v ss >/dev/null 2>&1 || command -v netstat >/dev/null 2>&1 || command -v lsof >/dev/null 2>&1; }
cc_listening() {
  if [ -r /proc/net/tcp ]; then
    cc_h=$(printf '%04X' "$1")
    cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | grep -Eiq "^ *[0-9]+: [0-9A-F]+:$cc_h [0-9A-F]+:[0-9A-F]+ 0A" && return 0
  fi
  if command -v ss >/dev/null 2>&1; then ss -ltn 2>/dev/null | grep -Eq "[.:]$1[[:space:]]" && return 0; fi
  if command -v netstat >/dev/null 2>&1; then netstat -an 2>/dev/null | grep LISTEN | grep -Eq "[.:]$1[[:space:]]" && return 0; fi
  if command -v lsof >/dev/null 2>&1; then lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1 && return 0; fi
  return 1
}
"#;

/// A shell function `cc_serve PORT CMD…`: start CMD in the background, print [`LISTEN_MARK`]
/// once it listens on PORT (or after 1.5 s when the target has no way to tell), then wait for
/// it and exit with its status. If CMD exits before listening, its status is the script's.
const SERVE_FN: &str = r#"cc_serve() {
  cc_port=$1; shift
  "$@" &
  cc_pid=$!
  trap 'kill $cc_pid 2>/dev/null' INT TERM HUP
  cc_i=0
  while :; do
    if ! kill -0 $cc_pid 2>/dev/null; then wait $cc_pid; exit $?; fi
    if cc_can_check; then
      if cc_listening $cc_port; then echo "CODECRAFT-LISTENING $cc_port"; break; fi
      [ $cc_i -ge 600 ] && { echo "CODECRAFT-LISTENING $cc_port (not verified)"; break; }
    elif [ $cc_i -ge 15 ]; then echo "CODECRAFT-LISTENING $cc_port (not verified)"; break; fi
    cc_i=$((cc_i + 1))
    sleep 0.1 2>/dev/null || sleep 1
  done
  wait $cc_pid
}
"#;

/// The words of a command, quoted.
fn words(program: &str, args: &[String]) -> String {
    std::iter::once(program).chain(args.iter().map(String::as_str)).map(sh_quote).collect::<Vec<_>>().join(" ")
}

/// Serve `program args` (a debug server listening on `port`) and wait for it: the script prints
/// [`LISTEN_MARK`] when the port is open.
pub fn serve(port: u16, program: &str, args: &[String]) -> String {
    format!("{LISTENING_FN}{SERVE_FN}cc_serve {port} {}\n", words(program, args))
}

/// The debugpy server: `python -m debugpy --listen host:port --wait-for-client (script | -m
/// module) args`, served with [`serve`]. Checks for the interpreter and debugpy first.
pub fn debugpy_server(
    python: &str,
    listen_host: &str,
    port: u16,
    script: Option<&str>,
    module: Option<&str>,
    args: &[String],
    target: &str,
) -> RemoteResult<String> {
    let mut argv: Vec<String> =
        vec!["-m".into(), "debugpy".into(), "--listen".into(), format!("{listen_host}:{port}"), "--wait-for-client".into()];
    match (module, script) {
        (Some(m), _) => {
            argv.push("-m".into());
            argv.push(m.to_string());
        }
        (None, Some(s)) => argv.push(s.to_string()),
        (None, None) => return Err(RemoteError::Invalid("nothing to debug: no script or module".into())),
    }
    argv.extend(args.iter().cloned());
    let mut out = require_program(python, &python_missing_message(python, target));
    out.push_str(&require_python_module(python, "debugpy", &debugpy_missing_message(python, target)));
    out.push_str(&serve(port, python, &argv));
    Ok(out)
}

/// Choose a gdb-remote stub on the target (lldb-server, gdbserver, debugserver) and print
/// [`SERVER_MARK`], or stop with [`missing`].
pub fn require_debug_server(target: &str) -> String {
    format!(
        "if command -v lldb-server >/dev/null 2>&1; then cc_ds=lldb-server; elif command -v gdbserver >/dev/null 2>&1; \
         then cc_ds=gdbserver; elif command -v debugserver >/dev/null 2>&1; then cc_ds=debugserver; else {}; fi; \
         echo \"{SERVER_MARK}$cc_ds\"; ",
        missing(&debug_server_missing_message(target))
    )
}

/// A gdb-remote stub serving `program args` on `listen_host:port`: `lldb-server gdbserver
/// host:port -- prog args`, `gdbserver host:port prog args` or `debugserver host:port prog args`
/// (whichever the target has), served with [`serve`].
pub fn gdb_server(listen_host: &str, port: u16, program: &str, args: &[String], target: &str) -> String {
    let address = sh_quote(&format!("{listen_host}:{port}"));
    let rest = words(program, args);
    format!(
        "{}{LISTENING_FN}{SERVE_FN}case $cc_ds in\n  lldb-server) cc_serve {port} lldb-server gdbserver {address} -- {rest};;\n  \
         *) cc_serve {port} \"$cc_ds\" {address} {rest};;\nesac\n",
        require_debug_server(target)
    )
}

/// Run `program args` in the foreground and keep its status in `cc_st` (for scripts that do more
/// afterwards).
pub fn run_keep_status(program: &str, args: &[String]) -> String {
    format!("{}; cc_st=$?; ", words(program, args))
}

/// `mkdir -p` for a remote folder.
pub fn mkdir(dir: &str) -> String {
    format!("mkdir -p -- {}; ", quote_path(dir))
}

/// A small Python program for `docker exec` relays: pipes stdin/stdout to `127.0.0.1:<argv[1]>`.
pub const RELAY_PY: &str = r#"import os,socket,sys,threading
s=socket.create_connection(('127.0.0.1',int(sys.argv[1])))
def up():
    while True:
        b=os.read(0,65536)
        if not b: break
        s.sendall(b)
    try: s.shutdown(socket.SHUT_WR)
    except OSError: pass
threading.Thread(target=up,daemon=True).start()
while True:
    b=s.recv(65536)
    if not b: break
    os.write(1,b)
"#;

/// The relay inside a container: python3, else `nc`, else `socat`, to `127.0.0.1:port`.
pub fn relay(port: u16) -> String {
    format!(
        "if command -v python3 >/dev/null 2>&1; then exec python3 -c {py} {port}; \
         elif command -v nc >/dev/null 2>&1; then exec nc 127.0.0.1 {port}; \
         elif command -v socat >/dev/null 2>&1; then exec socat - TCP:127.0.0.1:{port}; \
         else {}; fi",
        missing("the container has neither python3, nc nor socat to relay the debugger connection: publish a port instead (run options `-p`)"),
        py = sh_quote(RELAY_PY)
    )
}

/// Detect Python interpreters on the target: `python3`/`python` on PATH and virtualenvs in the
/// project folder, `~/.virtualenvs`, pyenv, conda and `/opt`. Prints `CODECRAFT-PY <path>\t<version>\t<venv 0|1>`
/// per interpreter (parsed by [`parse_interpreters`]).
pub fn detect_pythons(root: &str) -> String {
    let root = quote_path(root);
    format!(
        "{}cc_py() {{ \"$1\" -c 'import sys; print(\"CODECRAFT-PY \" + sys.executable + \"\\t\" + \".\".join(map(str, sys.version_info[:3])) + \"\\t\" + (\"1\" if sys.prefix != getattr(sys, \"base_prefix\", sys.prefix) else \"0\"))' 2>/dev/null; }}; \
         for p in python3 python python3.13 python3.12 python3.11 python3.10 python3.9 python3.8; do cc_p=$(command -v \"$p\" 2>/dev/null) && cc_py \"$cc_p\"; done; \
         for d in {root}/.venv {root}/venv {root}/env \"$HOME\"/.virtualenvs/* \"$HOME\"/.pyenv/versions/* \"$HOME\"/miniconda3 \"$HOME\"/miniconda3/envs/* \"$HOME\"/anaconda3 \"$HOME\"/anaconda3/envs/* /opt/conda /opt/conda/envs/* /opt/venv /venv; do \
           [ -x \"$d/bin/python\" ] && cc_py \"$d/bin/python\"; done; exit 0",
        print_root_plain(&root)
    )
}

fn print_root_plain(quoted_root: &str) -> String {
    format!("printf '%s%s\\n' '{ROOT_MARK}' \"$(cd {quoted_root} 2>/dev/null && pwd)\"; ")
}

/// One interpreter found by [`detect_pythons`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInterpreter {
    pub path: String,
    pub version: String,
    /// A virtual environment (not the base interpreter).
    pub venv: bool,
}

/// The interpreters in [`detect_pythons`]'s output, without duplicates (first seen wins).
pub fn parse_interpreters(output: &str) -> Vec<RemoteInterpreter> {
    let mut out: Vec<RemoteInterpreter> = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.trim_end().strip_prefix("CODECRAFT-PY ") else { continue };
        let mut parts = rest.split('\t');
        let (Some(path), Some(version)) = (parts.next(), parts.next()) else { continue };
        let venv = parts.next() == Some("1");
        let path = path.trim();
        if path.is_empty() || out.iter().any(|i| i.path == path) || out.len() >= 200 {
            continue;
        }
        out.push(RemoteInterpreter { path: path.to_string(), version: version.trim().to_string(), venv });
    }
    out
}

/// List an interpreter's installed distributions as one JSON line `CODECRAFT-PKGS [[name,
/// version], …]` (importlib.metadata: no pip needed).
pub fn list_packages(python: &str) -> String {
    let code = "import json\ntry:\n    import importlib.metadata as m\nexcept ImportError:\n    m = None\n\
                pkgs = sorted({((d.metadata['Name'] or ''), d.version) for d in m.distributions()}) if m else []\n\
                print('CODECRAFT-PKGS ' + json.dumps([p for p in pkgs if p[0]]))\n";
    format!(
        "{}{} -c {}",
        require_program(python, &format!("`{python}` was not found on the target")),
        sh_quote(python),
        sh_quote(code)
    )
}

/// One installed distribution.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemotePackage {
    pub name: String,
    pub version: String,
}

/// The packages in [`list_packages`]'s output.
pub fn parse_packages(output: &str) -> Option<Vec<RemotePackage>> {
    let line = output.lines().find_map(|l| l.trim().strip_prefix("CODECRAFT-PKGS "))?;
    let rows: Vec<(String, String)> = serde_json::from_str(line).ok()?;
    Some(rows.into_iter().take(10_000).map(|(name, version)| RemotePackage { name, version }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> std::process::Output {
        std::process::Command::new("sh").arg("-c").arg(script).output().unwrap()
    }

    #[test]
    fn markers_are_found_anywhere_in_a_line() {
        assert_eq!(marker("CODECRAFT-REMOTE-ROOT=/srv/app\n", ROOT_MARK), Some("/srv/app"));
        assert_eq!(marker("[stderr] CODECRAFT-MISSING: no x", MISSING_MARK), Some("no x"));
        assert_eq!(marker("nothing here", LISTEN_MARK), None);
    }

    #[test]
    fn prelude_quotes_and_validates() {
        let p = prelude("~/my app", &[("A".into(), "x y".into())]).unwrap();
        assert!(p.starts_with("cd \"$HOME\"/'my app' || exit 1; "), "{p}");
        assert!(p.contains("export A='x y'; "));
        assert!(prelude("/x", &[("A B".into(), "1".into())]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn root_missing_and_requirements_run_in_sh() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();
        let out = sh(&format!("{}exit 0", print_root(&d.display().to_string())));
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(marker(text.trim(), ROOT_MARK), Some(d.display().to_string().as_str()));
        let out = sh(&require_program("surely-not-a-program-xyz", "install `xyz` there"));
        assert_eq!(out.status.code(), Some(127));
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert_eq!(marker(&err, MISSING_MARK), Some("install `xyz` there"));
        let out = sh(&format!("{}echo ok", require_program("sh", "x")));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
        let out = sh(&require_python_module("surely-no-python-xyz", "debugpy", "no debugpy"));
        assert_eq!(out.status.code(), Some(127));
    }

    #[cfg(unix)]
    #[test]
    fn serve_reports_an_early_exit_status() {
        // A server that dies at once: the script exits with its status, no listening marker.
        let out = sh(&serve(1, "sh", &["-c".into(), "echo boom >&2; exit 3".into()]));
        assert_eq!(out.status.code(), Some(3));
        assert!(!String::from_utf8_lossy(&out.stdout).contains(LISTEN_MARK));
        assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn serve_announces_a_listening_port() {
        let Ok(python) = which_python() else { return };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let code = format!(
            "import socket,time\ns=socket.socket()\ns.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\ns.bind(('127.0.0.1',{port}))\ns.listen(1)\ntime.sleep(1.5)\n"
        );
        let out = sh(&serve(port, &python, &["-c".into(), code]));
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(text.contains(&format!("{LISTEN_MARK} {port}")), "{text} / {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.status.code(), Some(0));
    }

    fn which_python() -> Result<String, ()> {
        for p in ["python3", "python"] {
            if std::process::Command::new(p).arg("-c").arg("pass").output().is_ok_and(|o| o.status.success()) {
                return Ok(p.to_string());
            }
        }
        Err(())
    }

    #[test]
    fn debugpy_and_gdb_server_scripts() {
        let s = debugpy_server("python3", "127.0.0.1", 5678, Some("main.py"), None, &["a b".into()], "SSH: web").unwrap();
        assert!(s.contains("cc_serve 5678 python3 -m debugpy --listen 127.0.0.1:5678 --wait-for-client main.py 'a b'"), "{s}");
        assert!(s.contains("python3 -m pip install debugpy"), "{s}");
        let m = debugpy_server("/v/bin/python", "0.0.0.0", 1, None, Some("pytest"), &[], "x").unwrap();
        assert!(m.contains("--wait-for-client -m pytest"), "{m}");
        assert!(debugpy_server("p", "h", 1, None, None, &[], "x").is_err());
        let g = gdb_server("127.0.0.1", 1234, "/srv/app/target/debug/app", &["--x".into()], "SSH: b");
        assert!(g.contains("lldb-server) cc_serve 1234 lldb-server gdbserver 127.0.0.1:1234 -- /srv/app/target/debug/app --x;;"), "{g}");
        assert!(g.contains("*) cc_serve 1234 \"$cc_ds\" 127.0.0.1:1234 /srv/app/target/debug/app --x;;"), "{g}");
        assert!(g.contains("apt-get install -y gdbserver"));
    }

    #[cfg(unix)]
    #[test]
    fn debug_server_choice_fails_with_a_hint() {
        // An empty PATH has none of the stubs.
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(require_debug_server("Docker: x"))
            .env("PATH", "/nonexistent")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(127));
        assert!(String::from_utf8_lossy(&out.stderr).contains("no gdb-remote debug server on Docker: x"));
    }

    #[test]
    fn interpreters_and_packages_parse() {
        let out = "CODECRAFT-REMOTE-ROOT=/srv/app\nCODECRAFT-PY /usr/bin/python3\t3.11.2\t0\nCODECRAFT-PY /srv/app/.venv/bin/python\t3.11.2\t1\nCODECRAFT-PY /usr/bin/python3\t3.11.2\t0\ngarbage\nCODECRAFT-PY \t\t\n";
        let found = parse_interpreters(out);
        assert_eq!(found.len(), 2);
        assert!(found[1].venv && found[1].path == "/srv/app/.venv/bin/python");
        let pk = parse_packages("x\nCODECRAFT-PKGS [[\"debugpy\", \"1.8.0\"], [\"pytest\", \"8.0\"]]\n").unwrap();
        assert_eq!(pk[0], RemotePackage { name: "debugpy".into(), version: "1.8.0".into() });
        assert!(parse_packages("CODECRAFT-PKGS {bad").is_none());
        assert!(parse_packages("").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn detection_and_package_listing_run_locally() {
        let Ok(python) = which_python() else { return };
        let dir = tempfile::tempdir().unwrap();
        let out = sh(&detect_pythons(&dir.path().display().to_string()));
        let found = parse_interpreters(&String::from_utf8_lossy(&out.stdout));
        assert!(!found.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
        let out = sh(&list_packages(&python));
        assert!(parse_packages(&String::from_utf8_lossy(&out.stdout)).is_some(), "{out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_relay_pipes_a_connection() {
        let Ok(python) = which_python() else { return };
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let echo = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            c.read_exact(&mut buf).unwrap();
            c.write_all(&buf).unwrap();
        });
        let mut child = std::process::Command::new(&python)
            .arg("-c")
            .arg(RELAY_PY)
            .arg(port.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(b"hello").unwrap();
        let mut out = [0u8; 5];
        child.stdout.as_mut().unwrap().read_exact(&mut out).unwrap();
        assert_eq!(&out, b"hello");
        echo.join().unwrap();
        drop(child.stdin.take());
        let _ = child.wait();
    }

    #[test]
    fn relay_quotes_the_program() {
        let r = relay(5678);
        assert!(r.contains("exec python3 -c '") && r.contains("' 5678;"), "{r}");
        assert!(r.contains("nc 127.0.0.1 5678"));
    }
}
