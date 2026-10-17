//! M16 track RT: run, debug, test and profile on SSH hosts and Docker containers, end to end
//! through fake transports.
//!
//! The fake `ssh`, `rsync`, `scp` and `docker` (shell scripts in a temp dir, never on the real
//! machine's `~/.ssh` or Docker daemon) run the remote command *locally*: `ssh` in a temp
//! "remote home" (`HOME` there, so a deployment mapping `~/app` is `<remote>/app`), `-N -L`
//! forwards do nothing (the same port number on both sides is the same local port), `docker
//! run|exec|compose run` run the command in the mapped folder (`-v host:ctr` becomes a symlink
//! `ctr → host`, so the container's paths differ from the project's like a real mount). With
//! them the real debugpy, lldb-dap + debugserver, pytest, cProfile and tracemalloc run through
//! the remote code paths; each test skips when its tool is missing.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::debug::console_text;

const WAIT: Duration = Duration::from_secs(180);

const SSH: &str = r#"
printf 'ssh %s\n' "$*" >> "$LOG"
fwd=0
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break;;
    -N) fwd=1; shift;;
    -o|-p|-l|-i|-L|-O|-P) shift 2;;
    *) shift;;
  esac
done
dest="$1"; shift
[ "$dest" = "unreachable" ] && { echo "ssh: connect to host unreachable port 22: Connection refused" >&2; exit 255; }
# A forward: the same port number here and "there" is the same local port.
[ $fwd = 1 ] && exec sleep 3600
cd "$REMOTE" || exit 1
# The remote has its own HOME; Rust's toolchain stays the machine's (read only).
export RUSTUP_HOME="${RUSTUP_HOME:-$REALHOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$REALHOME/.cargo}"
unset CARGO_TARGET_DIR
HOME="$REMOTE" PATH="$FAKEBIN:$PATH" exec /bin/sh -c "$*"
"#;

/// `perf` on a Linux target: `record` runs the program and notes it, `script` prints samples
/// in its two functions.
const PERF: &str = r#"
printf 'perf %s\n' "$*" >> "$LOG"
case "$1" in
  record)
    while [ "$1" != "--" ]; do [ "$1" = "-o" ] && { out=$2; shift; }; shift; done; shift
    "$@"; st=$?
    echo "$1" > "$out"
    exit $st;;
  script)
    exe=$(cat "$3")
    for i in 1 2 3; do printf 'app 1/1 [000] 1.%s: 1 cpu-clock:u:\n\t    55aa twice+0x1 (%s)\n\t    55ab main+0x2 (%s)\n\n' "$i" "$exe" "$exe"; done;;
esac
"#;

const RSYNC: &str = r#"
printf 'rsync %s\n' "$*" >> "$LOG"
eval "src=\${$(($# - 1))}"; eval "dst=\${$#}"
map() { case "$1" in /*) echo "$1";; *) p="${1#*:}"; case "$p" in /*) echo "$p";; *) echo "$REMOTE/$p";; esac;; esac; }
src=$(map "$src"); dst=$(map "$dst")
case "$src" in */) mkdir -p "$dst"; cp -R "$src". "$dst";; *) mkdir -p "$(dirname "$dst")"; cp -R "$src" "$dst";; esac
"#;

const SCP: &str = r#"
printf 'scp %s\n' "$*" >> "$LOG"
exit 1
"#;

const DOCKER: &str = r#"
printf 'docker %s\n' "$*" >> "$LOG"
while [ "$1" = "--context" ]; do shift 2; done
cmd=$1; shift
case "$cmd" in
  compose) while [ "$1" = "-f" ]; do shift 2; done; shift;;
  run|exec) ;;
  cp) src=${1#*:}; dst=${2#*:}; mkdir -p "$(dirname "$dst")"; exec cp -R "$src" "$dst";;
  *) echo "fake docker: unsupported $cmd" >&2; exit 1;;
esac
wd=""
while [ $# -gt 0 ]; do
  case "$1" in
    --rm|-i|-T|-t|-d) shift;;
    -v) host=${2%%:*}; rest=${2#*:}; ctr=${rest%%:*}; mkdir -p "$(dirname "$ctr")"; [ -e "$ctr" ] || ln -s "$host" "$ctr"; shift 2;;
    -w) wd=$2; shift 2;;
    -e) export "$2"; shift 2;;
    -p|--network|--name) shift 2;;
    -*) shift;;
    *) break;;
  esac
done
shift
[ -n "$wd" ] && { cd "$wd" || exit 1; }
PATH="$FAKEBIN:$PATH" exec "$@"
"#;

struct Fake {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    remote: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

fn script(path: &Path, body: &str, f: &Fake) {
    let text = format!(
        "#!/bin/sh\nLOG='{}'; REMOTE='{}'; FAKEBIN='{}'; REALHOME='{}'\n{}",
        f.log.display(),
        f.remote.display(),
        f.bin.display(),
        std::env::var("HOME").unwrap_or_default(),
        body.trim_start()
    );
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fake() -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let f = Fake {
        project: root.join("project"),
        remote: root.join("remote-home"),
        bin: root.join("bin"),
        log: root.join("argv.log"),
        root,
        _dir: dir,
    };
    for d in [&f.project, &f.remote, &f.bin, &f.root.join("home")] {
        std::fs::create_dir_all(d).unwrap();
    }
    script(&f.bin.join("ssh"), SSH, &f);
    script(&f.bin.join("rsync"), RSYNC, &f);
    script(&f.bin.join("scp"), SCP, &f);
    script(&f.bin.join("docker"), DOCKER, &f);
    std::fs::write(f.root.join("home/ssh_config"), "Host web\n  HostName web.example\n  User deploy\n").unwrap();
    f
}

impl Fake {
    fn argv(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn session(&self) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&self.root.join("home"));
        s.remote.share_connections = false;
        s.remote.programs = codecraft_engine::remote::Programs {
            ssh: self.bin.join("ssh").display().to_string(),
            rsync: self.bin.join("rsync").display().to_string(),
            scp: self.bin.join("scp").display().to_string(),
        };
        s.docker.program = self.bin.join("docker").display().to_string();
        s.execute("file.open", &json!({ "path": self.project })).unwrap();
        pump_until(&mut s, "the project scan", |s| !s.jobs_running());
        s.execute("remote.deployment.setMapping", &json!({ "server": "web", "remote": "~/app" })).unwrap();
        s
    }

    /// A tool on the fake target's PATH.
    fn link(&self, name: &str, to: &Path) {
        let _ = std::fs::remove_file(self.bin.join(name));
        std::os::unix::fs::symlink(to, self.bin.join(name)).unwrap();
    }

    /// The container's root folder (a docker `-v` creates `<ctr>/workspace`).
    fn mount(&self) -> String {
        self.root.join("ctr/workspace").display().to_string()
    }
}

fn console(s: &Session) -> String {
    s.debug.console.iter().map(console_text).collect::<Vec<_>>().join("\n")
}

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            panic!("timed out waiting for {what}; status {}; console:\n{}", s.status, console(s));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn resolve(s: &mut Session, mut v: Value) -> Value {
    let start = Instant::now();
    while v["pending"] == json!(true) {
        if start.elapsed() > WAIT {
            panic!("timed out resolving {v}; console:\n{}", console(s));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
        let token = v["token"].as_u64().unwrap();
        v = s.execute("debug.poll", &json!({ "token": token })).unwrap();
    }
    v
}

fn wait_stop(s: &mut Session) -> Value {
    let v = s.execute("debug.waitForStop", &json!({ "timeoutMs": WAIT.as_millis() as u64 })).unwrap();
    let v = resolve(s, v);
    assert_ne!(v["timedOut"], json!(true), "{v}\n{}", console(s));
    v
}

fn step(s: &mut Session, action: &str) -> Value {
    let r = s.execute("debug.step", &json!({ "action": action, "timeoutMs": WAIT.as_millis() as u64 })).unwrap();
    resolve(s, json!({ "pending": true, "token": r["token"] }))
}

fn line_of(text: &str, marker: &str) -> usize {
    text.lines().position(|l| l.contains(marker)).map(|i| i + 1).unwrap_or_else(|| panic!("no {marker}"))
}

fn local<'a>(summary: &'a Value, name: &str) -> &'a str {
    summary["locals"]
        .as_array()
        .and_then(|l| l.iter().find(|v| v["name"] == json!(name)))
        .and_then(|v| v["value"].as_str())
        .unwrap_or_else(|| panic!("no local {name} in {summary}"))
}

/// An interpreter with debugpy (absolute path).
fn python_with(modules: &[&str]) -> Option<String> {
    let code = format!("import sys{}; print(sys.executable)", modules.iter().map(|m| format!(", {m}")).collect::<String>());
    let out = std::process::Command::new("python3").arg("-c").arg(code).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|p| !p.is_empty())
}

fn config_index(s: &Session, name: &str) -> usize {
    s.run_configs.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no configuration {name}"))
}

fn create(s: &mut Session, params: Value) -> usize {
    let name = params["name"].as_str().unwrap().to_string();
    s.execute("runConfig.create", &params).unwrap();
    config_index(s, &name)
}

const PY: &str = r#"import sys


def compute(n):
    total = 0
    for i in range(n):
        total += i  # @loop
    return total


result = compute(5)
print("result", result)  # @print
"#;

// ---------------------------------------------------------------- Python debugging

#[test]
fn python_debugged_on_an_ssh_host_with_paths_mapped() {
    let Some(python) = python_with(&["debugpy"]) else {
        eprintln!("skipping: no python3 with debugpy");
        return;
    };
    let f = fake();
    std::fs::write(f.project.join("main.py"), PY).unwrap();
    let mut s = f.session();
    let main = f.project.join("main.py");
    s.execute("debug.breakpoint.set", &json!({ "path": main, "line": line_of(PY, "@print") })).unwrap();
    let i = create(
        &mut s,
        json!({ "name": "Main on web", "kind": "python", "command": "main.py",
                "target": { "kind": "ssh", "server": "web", "python": python } }),
    );
    s.execute("run.debug", &json!({ "config": i })).unwrap();
    let stop = wait_stop(&mut s);
    assert_eq!(stop["state"], json!("stopped"), "{stop}\n{}", console(&s));
    // The frame is the local file (debugpy mapped `<remote>/app/main.py` back).
    assert_eq!(stop["frames"][0]["path"], json!(main.display().to_string()), "{stop}");
    assert_eq!(stop["frames"][0]["line"], json!(line_of(PY, "@print")));
    assert_eq!(local(&stop, "result"), "10");
    // It really ran in the remote copy of the project.
    assert!(f.remote.join("app/main.py").is_file());
    let argv = f.argv();
    assert!(argv.contains("rsync"), "{argv}");
    assert!(argv.contains("-N -L 127.0.0.1:"), "the port was forwarded: {argv}");
    assert!(argv.contains("debugpy --listen 127.0.0.1:"), "{argv}");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the session to end", |s| !s.debug.busy());
    s.shutdown();
}

#[test]
fn a_missing_debugpy_on_the_target_says_how_to_install_it() {
    let f = fake();
    // An interpreter without debugpy (any `-c` import fails).
    std::fs::write(f.bin.join("py-without-debugpy"), "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(f.bin.join("py-without-debugpy"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(f.project.join("main.py"), PY).unwrap();
    let mut s = f.session();
    let i = create(
        &mut s,
        json!({ "name": "Main", "kind": "python", "command": "main.py",
                "target": { "kind": "ssh", "server": "web", "python": "py-without-debugpy" } }),
    );
    s.execute("run.debug", &json!({ "config": i })).unwrap();
    let stop = wait_stop(&mut s);
    assert_eq!(stop["state"], json!("failed"), "{stop}");
    let err = stop.to_string();
    assert!(err.contains("debugpy is not installed for `py-without-debugpy` on SSH: web"), "{err}");
    assert!(err.contains("py-without-debugpy -m pip install debugpy"), "{err}");
    // And a missing interpreter.
    let j = create(
        &mut s,
        json!({ "name": "Main2", "kind": "python", "command": "main.py",
                "target": { "kind": "ssh", "server": "web", "python": "no-such-python3" } }),
    );
    s.execute("run.debug", &json!({ "config": j })).unwrap();
    let stop = wait_stop(&mut s);
    assert!(stop.to_string().contains("`no-such-python3` was not found on SSH: web"), "{stop}");
    s.shutdown();
}

#[test]
fn python_debugged_in_a_container_through_a_socket_relay() {
    let Some(python) = python_with(&["debugpy"]) else {
        eprintln!("skipping: no python3 with debugpy");
        return;
    };
    let f = fake();
    std::fs::write(f.project.join("main.py"), PY).unwrap();
    // The running container has the project mounted (as `docker run -v` would have).
    let mount = f.mount();
    std::fs::create_dir_all(f.root.join("ctr")).unwrap();
    std::os::unix::fs::symlink(&f.project, &mount).unwrap();
    let mut s = f.session();
    let main = f.project.join("main.py");
    s.execute("debug.breakpoint.set", &json!({ "path": main, "line": line_of(PY, "@loop") })).unwrap();
    let i = create(
        &mut s,
        json!({ "name": "In dev", "kind": "python", "command": "main.py",
                "target": { "kind": "docker", "container": "dev", "mount": mount, "python": python } }),
    );
    s.execute("run.debug", &json!({ "config": i })).unwrap();
    let stop = wait_stop(&mut s);
    assert_eq!(stop["state"], json!("stopped"), "{stop}\n{}", console(&s));
    assert_eq!(stop["frames"][0]["path"], json!(main.display().to_string()), "{stop}");
    assert_eq!(local(&stop, "n"), "5");
    let argv = f.argv();
    assert!(argv.contains("docker exec -i dev sh -c"), "{argv}");
    assert!(argv.contains("exec python3 -c"), "the relay ran in the container: {argv}");
    s.execute("debug.breakpoint.remove", &json!({ "path": main, "line": line_of(PY, "@loop") })).ok();
    s.execute("debug.stop", &json!({})).ok();
    pump_until(&mut s, "the session to end", |s| !s.debug.busy());
    s.shutdown();
}

#[test]
fn a_pytest_test_at_the_caret_debugged_in_a_docker_image() {
    let Some(python) = python_with(&["debugpy", "pytest"]) else {
        eprintln!("skipping: no python3 with debugpy and pytest");
        return;
    };
    let f = fake();
    let test = "def add(a, b):\n    return a + b\n\n\ndef test_add():\n    value = add(2, 3)\n    assert value == 5  # @assert\n";
    std::fs::write(f.project.join("test_calc.py"), test).unwrap();
    let mut s = f.session();
    let file = f.project.join("test_calc.py");
    s.execute("debug.breakpoint.set", &json!({ "path": file, "line": line_of(test, "@assert") })).unwrap();
    let target = json!({ "kind": "docker", "image": "python:3.12", "mount": f.mount(), "python": python });
    s.execute("debug.atCaret", &json!({ "path": file, "line": 5, "target": target })).unwrap();
    let stop = wait_stop(&mut s);
    assert_eq!(stop["state"], json!("stopped"), "{stop}\n{}", console(&s));
    assert_eq!(stop["frames"][0]["path"], json!(file.display().to_string()), "{stop}");
    assert_eq!(local(&stop, "value"), "5");
    let argv = f.argv();
    assert!(argv.contains("docker run --rm -i -v"), "{argv}");
    assert!(argv.contains("-p 127.0.0.1:"), "the port was published: {argv}");
    assert!(argv.contains("--listen 0.0.0.0:"), "{argv}");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the session to end", |s| !s.debug.busy());
    s.shutdown();
}

// ---------------------------------------------------------------- native debugging

const CPP: &str = r#"#include <cstdio>
#include <string>

int twice(int x) {
    return x * 2;
}

int main() {
    std::string name = "remote";
    int v = twice(21);
    std::printf("%s v=%d\n", name.c_str(), v); // @print
    return 0;
}
"#;

const C: &str = "int main(void) { return 0; }\n";

const RUST: &str = r#"fn twice(x: i32) -> i32 {
    x * 2
}

fn main() {
    let name = String::from("remote");
    let v = twice(21);
    let total: u64 = (0..2_000_000u64).map(|i| i % 7).sum();
    println!("{name} {v} {total}"); // @print
}
"#;

fn cargo_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"rtdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(dir.join("src/main.rs"), RUST).unwrap();
}

fn debugserver() -> Option<PathBuf> {
    [
        "/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Versions/A/Resources/debugserver",
        "/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Resources/debugserver",
        "/Applications/Xcode.app/Contents/SharedFrameworks/LLDB.framework/Resources/debugserver",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

#[test]
fn a_cpp_program_built_and_debugged_on_an_ssh_host_through_a_gdb_remote_stub() {
    let (Ok(_), Some(stub)) = (codecraft_engine::dap::discover::find_lldb_dap(), debugserver()) else {
        eprintln!("skipping: lldb-dap or debugserver is missing");
        return;
    };
    if codecraft_engine::dap::discover::which("c++").is_none() {
        eprintln!("skipping: no C++ compiler");
        return;
    }
    let f = fake();
    // Xcode's debugserver stands in for gdbserver on the fake target.
    f.link("debugserver", &stub);
    std::fs::write(f.project.join("main.cpp"), CPP).unwrap();
    let mut s = f.session();
    let src = f.project.join("main.cpp");
    s.execute("debug.breakpoint.set", &json!({ "path": src, "line": line_of(CPP, "@print") })).unwrap();
    // Objects compiled separately keep their debug info on a macOS "target" (one-step builds
    // leave it in a .dSYM, downloaded too when there is one).
    let i = create(
        &mut s,
        json!({ "name": "C++ on web", "kind": "command",
                "command": "sh -c 'mkdir -p build && c++ -g -O0 -c main.cpp -o build/main.o && c++ -g -o build/app build/main.o && ./build/app'",
                "target": { "kind": "ssh", "server": "web" } }),
    );
    s.execute("run.debug", &json!({ "config": i })).unwrap();
    let mut stop = wait_stop(&mut s);
    if stop["state"] == json!("failed") && stop.to_string().contains("debugging rights") {
        eprintln!("skipping: {}", stop["error"]);
        return;
    }
    assert_ne!(stop["state"], json!("failed"), "{stop}\n{}", console(&s));
    // The stub starts the program stopped at its entry: continue to the breakpoint.
    if stop["frames"][0]["line"] != json!(line_of(CPP, "@print")) {
        stop = step(&mut s, "continue");
    }
    assert_eq!(stop["frames"][0]["line"], json!(line_of(CPP, "@print")), "{stop}\n{}", console(&s));
    assert_eq!(stop["frames"][0]["path"], json!(src.display().to_string()), "local path: {stop}");
    assert_eq!(local(&stop, "v"), "42");
    assert!(local(&stop, "name").contains("remote"), "{stop}");
    // Built there, copied back for symbols.
    assert!(f.remote.join("app/build/app").is_file());
    assert!(f.project.join(".codecraft/remote-run/bin/app").is_file());
    let text = console(&s);
    assert!(text.contains("debugserver") || f.argv().contains("debugserver"), "{text}");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the session to end", |s| !s.debug.busy());
    s.shutdown();
}

#[test]
fn a_rust_binary_built_by_cargo_on_the_target_and_debugged_there() {
    let (Ok(_), Some(stub)) = (codecraft_engine::dap::discover::find_lldb_dap(), debugserver()) else {
        eprintln!("skipping: lldb-dap or debugserver is missing");
        return;
    };
    if codecraft_engine::dap::discover::which("cargo").is_none() {
        eprintln!("skipping: no cargo");
        return;
    }
    let f = fake();
    f.link("debugserver", &stub);
    cargo_project(&f.project);
    let mut s = f.session();
    let src = f.project.join("src/main.rs");
    s.execute("debug.breakpoint.set", &json!({ "path": src, "line": line_of(RUST, "@print") })).unwrap();
    let i = config_index(&s, "Run rtdemo");
    // The configuration is local; the command's `target` debugs it on the host.
    s.execute("run.debug", &json!({ "config": i, "target": "web" })).unwrap();
    let mut stop = wait_stop(&mut s);
    assert_ne!(stop["state"], json!("failed"), "{stop}\n{}", console(&s));
    if stop["frames"][0]["line"] != json!(line_of(RUST, "@print")) {
        stop = step(&mut s, "continue");
    }
    assert_eq!(stop["frames"][0]["line"], json!(line_of(RUST, "@print")), "{stop}\n{}", console(&s));
    assert_eq!(stop["frames"][0]["path"], json!(src.display().to_string()), "{stop}");
    assert_eq!(local(&stop, "v"), "42");
    assert_eq!(local(&stop, "name"), "\"remote\"", "Rust values read as Rust: {stop}");
    // cargo built it there (its own target directory), not here.
    assert!(f.remote.join("app/target/debug/rtdemo").is_file());
    assert!(!f.project.join("target/debug/rtdemo").exists());
    // `println!` puts several locations on the line: continue past them.
    let mut end = step(&mut s, "continue");
    for _ in 0..3 {
        if end["state"] != json!("stopped") {
            break;
        }
        end = step(&mut s, "continue");
    }
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the session to end", |s| !s.debug.busy());
    s.shutdown();
}

#[test]
fn a_rust_binary_profiled_with_perf_on_a_linux_target() {
    if codecraft_engine::dap::discover::which("cargo").is_none() {
        eprintln!("skipping: no cargo");
        return;
    }
    let f = fake();
    script(&f.bin.join("perf"), PERF, &f);
    cargo_project(&f.project);
    let mut s = f.session();
    let i = config_index(&s, "Run rtdemo");
    let v = s.execute("run.profile", &json!({ "config": i, "target": "web", "tool": "perf" })).unwrap();
    let job = v["job"].as_u64().unwrap_or_else(|| s.profiler.jobs.last().map(|j| j.id).unwrap());
    let j = wait_job(&mut s, job);
    assert_eq!(j["state"], json!("done"), "{j}\n{}", f.argv());
    assert_eq!(j["tool"], json!("perf"), "{j}");
    let report = s.execute("profiler.hotspots", &json!({ "snapshot": j["snapshot"], "limit": 10 })).unwrap();
    assert!(report.to_string().contains("twice"), "{report}");
    let argv = f.argv();
    assert!(argv.contains("perf record -g -o"), "{argv}");
    assert!(argv.contains("target/debug/rtdemo"), "the artifact cargo built there: {argv}");
    s.shutdown();
}

#[test]
fn no_debug_server_on_the_target_says_what_to_install() {
    if codecraft_engine::dap::discover::which("cc").is_none() {
        eprintln!("skipping: no C compiler");
        return;
    }
    let f = fake();
    std::fs::write(f.project.join("main.c"), C).unwrap();
    let mut s = f.session();
    let i = create(
        &mut s,
        json!({ "name": "C", "kind": "command", "command": "sh -c 'cc -g -o app main.c && ./app'",
                "target": { "kind": "ssh", "server": "web" } }),
    );
    // lldb-server, gdbserver and debugserver are not on this PATH (debugserver is never on PATH).
    s.execute("run.debug", &json!({ "config": i })).unwrap();
    let stop = wait_stop(&mut s);
    assert_eq!(stop["state"], json!("failed"), "{stop}");
    assert!(stop.to_string().contains("no gdb-remote debug server on SSH: web"), "{stop}");
    assert!(stop.to_string().contains("apt-get install -y gdbserver"), "{stop}");
    s.shutdown();
}

// ---------------------------------------------------------------- tests

#[test]
fn pytest_runs_on_an_ssh_host_with_local_results() {
    let Some(python) = python_with(&["pytest"]) else {
        eprintln!("skipping: no python3 with pytest");
        return;
    };
    let f = fake();
    let test = "def test_ok():\n    assert 1 + 1 == 2\n\n\ndef test_bad():\n    assert 2 * 2 == 5\n";
    std::fs::write(f.project.join("test_math.py"), test).unwrap();
    let mut s = f.session();
    s.execute("run.manageTargets.add", &json!({ "name": "build box", "target": "ssh:web", "python": python })).unwrap();
    let v = s
        .execute("run.testsInFile", &json!({ "path": f.project.join("test_math.py"), "target": "build box" }))
        .unwrap();
    let tab = v["tab"].as_u64().unwrap();
    pump_until(&mut s, "the tests", |s| s.testing.runs.iter().any(|r| r.finished));
    let summary = s.execute("test.summary", &json!({ "tab": tab })).unwrap();
    let text = summary.to_string();
    assert!(text.contains("test_bad"), "{text}");
    assert!(text.contains("-m pytest") && text.contains("test_math.py (on SSH: web)"), "the configured command: {text}");
    assert!(!text.contains("rsync"), "{text}");
    let run = s.testing.runs.last().unwrap();
    let counts = run.counts();
    assert_eq!((counts.passed, counts.failed), (1, 1), "{text}");
    // Failures point at the local file; the report came back from the host.
    assert!(text.contains(&f.project.join("test_math.py").display().to_string()) || text.contains("test_math.py"), "{text}");
    assert!(run.target.is_some());
    assert!(f.argv().contains("junit"), "{}", f.argv());
    // Rerun goes to the same target.
    let before = f.argv().matches("ssh ").count();
    s.execute("test.rerunFailed", &json!({ "tab": tab })).unwrap();
    pump_until(&mut s, "the rerun", |s| s.testing.runs.iter().filter(|r| r.finished).count() >= 2);
    assert!(f.argv().matches("ssh ").count() > before);
    s.shutdown();
}

#[test]
fn a_command_runs_in_a_compose_service() {
    let f = fake();
    std::fs::write(f.project.join("hello.sh"), "echo \"hello from $(pwd)\"\n").unwrap();
    let mut s = f.session();
    let i = create(&mut s, json!({ "name": "Hello", "kind": "command", "command": "sh hello.sh" }));
    let target = json!({ "kind": "docker", "service": "web", "compose_file": "compose.yaml", "mount": f.mount() });
    let v = s.execute("run.run", &json!({ "config": i, "target": target })).unwrap();
    let tab = codecraft_engine::workspace::RunTabId(v["tab"].as_u64().unwrap());
    pump_until(&mut s, "the run", |s| s.ws.run(tab).is_some_and(|t| !t.is_running()));
    let out: Vec<String> = s.ws.run(tab).unwrap().lines.iter().map(|l| l.text.clone()).collect();
    assert!(out.iter().any(|l| l.contains(&format!("hello from {}", f.mount()))), "{out:?}");
    let argv = f.argv();
    assert!(argv.contains("docker compose -f") && argv.contains(" run --rm -T -v "), "{argv}");
    s.shutdown();
}

// ---------------------------------------------------------------- profiling

const WORK: &str = r#"def slow(n):
    total = 0
    for i in range(n):
        total += i * i
    return total


def main():
    data = [slow(20000) for _ in range(30)]
    print(len(data))


main()
"#;

fn wait_job(s: &mut Session, job: u64) -> Value {
    let start = Instant::now();
    loop {
        s.pump();
        let j = s.execute("profiler.job", &json!({ "job": job })).unwrap();
        if j["state"] == json!("done") || j["state"] == json!("failed") {
            return j;
        }
        assert!(start.elapsed() < WAIT, "profile job timed out: {j}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn hot_files(s: &mut Session, snapshot: &str) -> Vec<String> {
    let v = s.execute("profiler.hotspots", &json!({ "snapshot": snapshot, "limit": 50 })).unwrap();
    let v = if v["pending"] == json!(true) {
        let start = Instant::now();
        loop {
            s.pump();
            let v = s.execute("profiler.hotspots", &json!({ "snapshot": snapshot, "limit": 50 })).unwrap();
            if v["pending"] != json!(true) {
                break v;
            }
            assert!(start.elapsed() < WAIT);
            std::thread::sleep(Duration::from_millis(20));
        }
    } else {
        v
    };
    let mut out = Vec::new();
    collect_files(&v, &mut out);
    out
}

fn collect_files(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                if (k == "file" || k == "path") && x.is_string() {
                    out.push(x.as_str().unwrap_or_default().to_string());
                }
                collect_files(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_files(x, out)),
        _ => {}
    }
}

#[test]
fn cprofile_and_tracemalloc_on_an_ssh_host_open_with_local_paths() {
    let Some(python) = python_with(&[]) else {
        eprintln!("skipping: no python3");
        return;
    };
    let f = fake();
    std::fs::write(f.project.join("work.py"), WORK).unwrap();
    let mut s = f.session();
    let target = json!({ "kind": "ssh", "server": "web", "python": python });
    for (kind, tool) in [("cpu", "cProfile"), ("memory", "tracemalloc")] {
        let v = s
            .execute("profiler.record", &json!({ "command": "python3 work.py", "kind": kind, "target": target }))
            .unwrap();
        let job = v["job"].as_u64().unwrap();
        let j = wait_job(&mut s, job);
        assert_eq!(j["state"], json!("done"), "{tool}: {j}");
        assert_eq!(j["tool"], json!(tool), "{j}");
        let snapshot = j["snapshot"].as_str().unwrap().to_string();
        assert!(snapshot.starts_with(&f.project.join(".codecraft/remote-run").display().to_string()), "{snapshot}");
        let files = hot_files(&mut s, &snapshot);
        let work = f.project.join("work.py").display().to_string();
        assert!(files.iter().any(|p| p.contains(&work)), "{tool}: local paths expected: {files:?}");
        let remote = f.remote.join("app").display().to_string();
        assert!(!files.iter().any(|p| p.starts_with(&remote)), "{tool}: remote paths left: {files:?}");
    }
    assert!(f.argv().contains("cProfile"), "{}", f.argv());
    s.shutdown();
}

#[test]
fn cprofile_in_a_docker_image_maps_the_mount_back() {
    let Some(python) = python_with(&[]) else {
        eprintln!("skipping: no python3");
        return;
    };
    let f = fake();
    std::fs::write(f.project.join("work.py"), WORK).unwrap();
    let mut s = f.session();
    let i = create(
        &mut s,
        json!({ "name": "Work", "kind": "python", "command": "work.py",
                "target": { "kind": "docker", "image": "python:3.12", "mount": f.mount(), "python": python } }),
    );
    let v = s.execute("run.profile", &json!({ "config": i })).unwrap();
    let job = v["job"].as_u64().unwrap_or_else(|| s.profiler.jobs.last().map(|j| j.id).unwrap());
    let j = wait_job(&mut s, job);
    assert_eq!(j["state"], json!("done"), "{j}");
    let files = hot_files(&mut s, j["snapshot"].as_str().unwrap());
    let work = f.project.join("work.py").display().to_string();
    assert!(files.iter().any(|p| p.contains(&work)), "{files:?}");
    assert!(!files.iter().any(|p| p.starts_with(&f.mount())), "{files:?}");
    s.shutdown();
}

#[test]
fn py_spy_missing_on_the_target_says_how_to_install_it() {
    let f = fake();
    std::fs::write(f.project.join("work.py"), WORK).unwrap();
    let mut s = f.session();
    // py-spy is not on this PATH (and never installed by CodeCraft).
    if codecraft_engine::dap::discover::which("py-spy").is_some() {
        eprintln!("skipping: py-spy is installed here");
        return;
    }
    let v = s
        .execute("profiler.record", &json!({ "command": "python3 work.py", "tool": "py-spy", "target": "ssh:web" }))
        .unwrap();
    let j = wait_job(&mut s, v["job"].as_u64().unwrap());
    assert_eq!(j["state"], json!("failed"), "{j}");
    assert!(j["error"].as_str().unwrap().contains("py-spy is not installed on SSH: web"), "{j}");
    assert!(j["error"].as_str().unwrap().contains("-m pip install py-spy"), "{j}");
    s.shutdown();
}

// ---------------------------------------------------------------- remote interpreters

#[test]
fn remote_interpreters_are_detected_with_their_packages() {
    let Some(python) = python_with(&[]) else {
        eprintln!("skipping: no python3");
        return;
    };
    let f = fake();
    // A virtualenv-like interpreter in the remote project.
    std::fs::create_dir_all(f.remote.join("app/.venv/bin")).unwrap();
    std::os::unix::fs::symlink(&python, f.remote.join("app/.venv/bin/python")).unwrap();
    let mut s = f.session();
    s.execute("python.remoteInterpreters.detect", &json!({ "target": "web" })).unwrap();
    pump_until(&mut s, "detection", |s| !s.targets.python.busy());
    let v = s.execute("python.remoteInterpreters", &json!({ "target": "web" })).unwrap();
    let list = &v["targets"][0];
    assert_eq!(list["error"], Value::Null, "{v}");
    let found: Vec<&str> = list["interpreters"].as_array().unwrap().iter().filter_map(|i| i["path"].as_str()).collect();
    assert!(found.iter().any(|p| p.ends_with("app/.venv/bin/python")) || !found.is_empty(), "{v}");
    assert_eq!(list["root"], json!(f.remote.join("app").display().to_string()), "{v}");
    s.execute("python.remoteInterpreters.packages", &json!({ "target": "web", "python": python })).unwrap();
    pump_until(&mut s, "packages", |s| !s.targets.python.busy());
    let pk = s.execute("python.remoteInterpreters", &json!({ "target": "web" })).unwrap();
    let packages = &pk["targets"][0]["packages"][python.as_str()];
    assert_eq!(packages["error"], Value::Null, "{pk}");
    assert!(packages["packages"].is_array(), "{pk}");
    // Use it for a configuration on that target.
    s.execute("runConfig.create", &json!({ "name": "App", "kind": "python", "command": "main.py" })).unwrap();
    let err = s.execute("python.remoteInterpreters.use", &json!({ "config": "App", "python": python })).unwrap_err();
    assert!(err.to_string().contains("runs on this machine"), "{err}");
    s.execute("python.remoteInterpreters.use", &json!({ "config": "App", "python": python, "target": "web" })).unwrap();
    let c = s.run_store.user.iter().find(|c| c.name == "App").unwrap();
    assert_eq!(codecraft_engine::target_exec::target_python_of(&c.target), python);
    // An unreachable host fails with ssh's reason.
    s.execute("remote.servers.add", &json!({ "name": "down", "host": "unreachable" })).unwrap();
    s.execute("python.remoteInterpreters.detect", &json!({ "target": { "kind": "ssh", "server": "down", "remoteDir": "/x" } })).unwrap();
    pump_until(&mut s, "detection", |s| !s.targets.python.busy());
    let v = s.execute("python.remoteInterpreters", &json!({ "target": { "kind": "ssh", "server": "down", "remoteDir": "/x" } })).unwrap();
    assert!(v["targets"][0]["error"].as_str().unwrap_or_default().contains("Connection refused"), "{v}");
    s.shutdown();
}

// ---------------------------------------------------------------- agents

#[test]
fn agents_debug_on_a_target_through_the_mcp_tool() {
    use codecraft_engine::agent_access::{AgentAccess, Capability};
    use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};
    let Some(python) = python_with(&["debugpy"]) else {
        eprintln!("skipping: no python3 with debugpy");
        return;
    };
    let f = fake();
    std::fs::write(f.project.join("main.py"), PY).unwrap();
    let mut s = f.session();
    s.agent_access = AgentAccess::headless(&[Capability::Debug, Capability::Run]);
    s.execute("run.manageTargets.add", &json!({ "name": "web py", "target": "ssh:web", "python": python })).unwrap();
    s.execute("runConfig.create", &json!({ "name": "Main", "kind": "python", "command": "main.py" })).unwrap();
    let main = f.project.join("main.py");
    let call = |s: &mut Session, name: &str, args: Value| {
        let r = agent_tools::call_blocking(s, Caller::new("test agent"), name, args.clone(), WAIT);
        assert!(!r.is_error, "{name} {args}: {}", result_text(&r));
        result_json(&r)
    };
    call(&mut s, "debug_breakpoint_set", json!({ "path": main, "line": line_of(PY, "@print") }));
    let started = call(&mut s, "debug_start", json!({ "config": "Main", "target": "web py" }));
    assert_eq!(started["started"], json!(true), "{started}");
    let stop = call(&mut s, "debug_wait_for_stop", json!({ "timeoutMs": 120_000 }));
    assert_eq!(stop["state"], json!("stopped"), "{stop}");
    assert_eq!(stop["frames"][0]["file"], json!(main.display().to_string()), "local path for agents: {stop}");
    call(&mut s, "debug_control", json!({ "action": "stop" }));
    pump_until(&mut s, "the session to end", |s| !s.debug.busy());
    // A bad target is a clear error.
    let r = agent_tools::call_blocking(&mut s, Caller::new("t"), "run_start", json!({ "config": "Main", "target": "nowhere" }), WAIT);
    assert!(r.is_error && result_text(&r).contains("unknown target `nowhere`"), "{}", result_text(&r));
    s.shutdown();
}
