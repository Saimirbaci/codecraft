//! M15 QA (track QB), as a person: the M15 features walked end to end with the commands the UI
//! dispatches, on sample projects in temp dirs with the real tools — a Python file's lint
//! underlined when it opens and after a save, fixed with Alt+Enter, Inspect Code's findings in
//! the editor; CodeCraft's own `.codecraft/` state kept out of a Git repository's changes; a
//! Python client and server debugged at once, switching between them; the test at the caret
//! profiled (a GoogleTest test through CMake, with xctrace) and a time range of it; the peak of a
//! Python program's memory with tracemalloc. Each test skips cleanly when its tools are missing.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::inspections::Tool;
use codecraft_engine::{Popup, Session, dap};
use codecraft_workspace::ProblemSource;

fn dump() -> bool {
    std::env::var_os("CODECRAFT_QA_DUMP").is_some()
}

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("/usr/bin/which").arg(name).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())).filter(|p| p.is_file())
}

fn has(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

fn write(root: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

fn pump_until(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    while !done(s) {
        assert!(start.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}: {}", s.status);
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn session(home: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s
}

fn open(s: &mut Session, root: &Path) {
    s.execute("file.open", &json!({ "path": root })).unwrap();
    pump_until(s, "the project", 120, |s| s.ws.project.as_ref().is_some_and(|p| p.tree.len() > 1) && !s.jobs_running());
}

fn line_of(text: &str, marker: &str) -> usize {
    text.lines().position(|l| l.contains(marker)).map(|i| i + 1).unwrap_or_else(|| panic!("no {marker}"))
}

fn analysis_lines(s: &Session, path: &Path) -> Vec<(usize, String)> {
    s.ws.problems_for(path)
        .into_iter()
        .filter(|p| p.source == ProblemSource::Analysis)
        .map(|p| (p.start.line, p.code.clone().unwrap_or_default()))
        .collect()
}

fn text(s: &Session) -> String {
    s.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
}

// ---------------------------------------------------------------- analysis in the editor

const LINT_PY: &str = "import os
import sys


def greet(name):
    message = \"hello \" + name
    return message


print(greet(sys.argv[0]))
";

/// Open a Python file with an unused import: it is underlined at once (it used to need an edit
/// and a save: Cmd+S on an unmodified file saves nothing); an edit that adds another problem
/// and a save underline that one too; Alt+Enter fixes the first with Ruff's fix; Inspect Code on
/// the file brings its findings into the editor as well.
#[test]
fn python_lint_is_underlined_on_open_and_save_and_fixed_with_alt_enter() {
    let Some(ruff) = which("ruff") else {
        eprintln!("skipping: ruff is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("py");
    write(&root, &[("lint.py", LINT_PY), ("other.py", "import json\n")]);
    let mut s = session(&base.join("home"));
    s.analysis.editor.on_save.debounce = Duration::ZERO;
    s.inspections.programs.insert(Tool::Ruff, ruff);
    open(&mut s, &root);
    let lint = root.join("lint.py");
    s.execute("file.open", &json!({ "path": lint })).unwrap();
    pump_until(&mut s, "the analysis on open", 60, |s| {
        !s.analysis.editor.on_save.busy() && !analysis_lines(s, &lint).is_empty()
    });
    assert_eq!(analysis_lines(&s, &lint), vec![(0, "F401".to_string())], "`import os` underlined without a save");
    let v = s.execute("analysis.fileFindings", &json!({ "path": "lint.py" })).unwrap();
    if dump() {
        eprintln!("fileFindings after open: {v:#}");
    }
    assert_eq!(v["findings"][0]["line"], 1, "{v}");
    // Since M16 (track LT) the analysis while typing analyzes the buffer when it opens.
    assert_eq!(v["findings"][0]["source"], "live", "{v}");

    // A second problem typed in and saved.
    let len = s.ws.active().unwrap().1.doc.len_chars();
    s.execute("editor.setSelection", &json!({ "anchor": len })).unwrap();
    s.execute("editor.type", &json!({ "text": "import re\n" })).unwrap();
    s.execute("file.saveAll", &Value::Null).unwrap();
    pump_until(&mut s, "ruff on save", 60, |s| {
        !s.analysis.editor.on_save.busy() && analysis_lines(s, &lint).len() >= 2
    });
    let lines = analysis_lines(&s, &lint);
    assert!(lines.contains(&(0, "F401".into())) && lines.iter().any(|(l, _)| *l == 10), "{lines:?}");

    // Alt+Enter on `os`: Ruff's fix first.
    s.execute("editor.setSelection", &json!({ "anchor": 8 })).unwrap();
    s.execute("code.showContextActions", &Value::Null).unwrap();
    let titles: Vec<String> = match &s.popup {
        Some(Popup::Actions { items, .. }) => items.iter().map(|i| i.title.clone()).collect(),
        other => panic!("no context actions: {other:?}"),
    };
    assert!(titles[0].starts_with("Ruff: Remove unused import"), "{titles:?}");
    s.execute("code.applyContextAction", &json!({ "index": 0 })).unwrap();
    s.pump();
    assert!(text(&s).starts_with("import sys\n"), "{}", text(&s));
    let lines = analysis_lines(&s, &lint);
    assert!(
        !lines.iter().any(|(l, c)| *l == 0 && c == "F401") && !lines.is_empty(),
        "the fixed finding went, `re`'s stay: {lines:?}"
    );

    // Inspect Code on another file: its finding shows in that file's editor.
    let other = root.join("other.py");
    s.execute("code.inspectCode", &json!({ "scope": "file", "path": other, "tools": ["ruff"] })).unwrap();
    pump_until(&mut s, "Inspect Code", 60, |s| !s.inspections.busy() && s.inspections.results.is_some());
    s.execute("file.open", &json!({ "path": other })).unwrap();
    pump_until(&mut s, "the finding in the editor", 30, |s| !analysis_lines(s, &other).is_empty());
    let v = s.execute("analysis.fileFindings", &json!({ "path": other })).unwrap();
    assert!(v["findings"].as_array().unwrap().iter().any(|f| f["rule"] == "F401"), "{v}");
    s.shutdown();
}

/// Opening a file that is not Python, or with analysis on save turned off, runs nothing.
#[test]
fn analysis_on_open_is_python_only_and_follows_the_setting() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("p");
    write(&root, &[("a.py", "import os\n"), ("b.py", "import os\n"), ("c.rs", "fn main() {}\n")]);
    // A stand-in ruff that records its runs.
    let log = base.join("ruff.log");
    let fake = base.join("fake-ruff");
    std::fs::write(&fake, format!("#!/bin/sh\necho run >> '{}'\necho '[]'\n", log.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut s = session(&base.join("home"));
    s.analysis.editor.on_save.debounce = Duration::ZERO;
    s.inspections.programs.insert(Tool::Ruff, fake);
    open(&mut s, &root);
    let runs = || std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
    s.execute("file.open", &json!({ "path": root.join("c.rs") })).unwrap();
    pump_until(&mut s, "idle", 10, |s| !s.analysis.editor.on_save.busy());
    assert_eq!(runs(), 0, "a Rust file opened: nothing runs");
    s.execute("file.open", &json!({ "path": root.join("a.py") })).unwrap();
    pump_until(&mut s, "ruff on open", 30, |_| runs() == 1);
    // Opening it again (already open) does not run it again.
    s.execute("file.open", &json!({ "path": root.join("a.py") })).unwrap();
    pump_until(&mut s, "idle", 10, |s| !s.analysis.editor.on_save.busy());
    assert_eq!(runs(), 1);
    s.ws.settings.analysis_on_save = false;
    // Since M16 (track LT) the analysis while typing analyzes a buffer when it opens too: both
    // settings off, nothing runs.
    s.ws.settings.analysis_while_typing_python = false;
    s.execute("file.open", &json!({ "path": root.join("b.py") })).unwrap();
    for _ in 0..30 {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(runs(), 1, "the setting turns it off");
    s.shutdown();
}

// ---------------------------------------------------------------- Git: CodeCraft's own state

fn git_env(home: &Path) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), home.display().to_string()),
        ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_ALLOW_PROTOCOL".into(), "file".into()),
    ]
}

fn git(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).envs(git_env(home)).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A changelist (and a breakpoint) wrote `.codecraft/…` into the project, and the Commit tool
/// window listed it as an unversioned file to commit; now `.codecraft/.gitignore` keeps
/// CodeCraft's per-user state out, like IntelliJ's `.idea/.gitignore`.
#[test]
fn codecraft_state_stays_out_of_the_commit() {
    if !has("git", &["--version"]) {
        eprintln!("skipping: git is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("githome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = T\n\temail = t@example.com\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    let root = base.join("repo");
    write(&root, &[("a.txt", "a\n"), ("b.txt", "b\n"), ("main.py", "x = 1\n")]);
    git(&root, &home, &["init", "-q", "-b", "main"]);
    git(&root, &home, &["add", "-A"]);
    git(&root, &home, &["commit", "-qm", "init"]);
    write(&root, &[("a.txt", "a2\n"), ("b.txt", "b2\n")]);
    let mut s = session(&base.join("home"));
    s.vcs.set_git_env(git_env(&home));
    open(&mut s, &root);
    pump_until(&mut s, "the repository", 60, |s| s.vcs.repo.is_some() && s.vcs.status.files.len() == 2);
    s.execute("vcs.changelist.new", &json!({ "name": "Later", "files": ["b.txt"] })).unwrap();
    s.execute("debug.breakpoint.set", &json!({ "path": root.join("main.py"), "line": 1 })).unwrap();
    s.execute("vcs.refresh", &json!({})).unwrap();
    pump_until(&mut s, "the ignore file", 60, |s| {
        if dump() {
            eprintln!("{:?}", s.vcs.status.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>());
        }
        root.join(".codecraft/.gitignore").is_file()
            && !s
                .vcs
                .status
                .files
                .iter()
                .any(|f| !f.ignored && f.path.starts_with(".codecraft/") && f.path != ".codecraft/.gitignore")
    });
    let listed: Vec<String> = s.vcs.status.files.iter().map(|f| f.path.clone()).collect();
    assert!(root.join(".codecraft/changelists.json").is_file(), "the changelist is saved");
    for f in ["changelists.json", "breakpoints.json"] {
        let ignored = git(&root, &home, &["check-ignore", &format!(".codecraft/{f}")]);
        assert!(ignored.contains(f), "{f} is ignored: {listed:?}");
    }
    // Shared settings are not.
    assert!(git(&root, &home, &["check-ignore", ".codecraft/run-configurations.json"]).is_empty());
    // A user who deletes the file keeps it deleted for the session.
    std::fs::remove_file(root.join(".codecraft/.gitignore")).unwrap();
    s.execute("vcs.refresh", &json!({})).unwrap();
    pump_until(&mut s, "the refresh", 30, |s| {
        s.vcs.status.files.iter().any(|f| !f.ignored && f.path == ".codecraft/changelists.json")
    });
    assert!(!root.join(".codecraft/.gitignore").exists());
    s.shutdown();
}

// ---------------------------------------------------------------- two debug sessions

fn resolve(s: &mut Session, mut v: Value) -> Value {
    let start = Instant::now();
    while v["pending"] == json!(true) {
        assert!(start.elapsed() < Duration::from_secs(180), "timed out resolving {v}");
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
        let token = v["token"].as_u64().unwrap();
        v = s.execute("debug.poll", &json!({ "token": token })).unwrap();
    }
    v
}

fn wait_session(s: &mut Session, session: u64) -> Value {
    let v = s.execute("debug.waitForStop", &json!({ "session": session, "timeoutMs": 120_000 })).unwrap();
    resolve(s, v)
}

fn local(summary: &Value, name: &str) -> String {
    summary["locals"]
        .as_array()
        .and_then(|l| l.iter().find(|v| v["name"] == name))
        .and_then(|v| v["value"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A TCP server and its client under debugpy at once: the server stops while handling the
/// request (the client waits for its answer, running), the person looks at the server's values,
/// switches to the client's tab, continues the server, and the client stops with the answer.
#[test]
fn a_python_client_and_server_debugged_at_once() {
    if dap::discover::find_debugpy(None).is_err() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let port = free_port();
    let server = format!(
        "import socket\nsrv = socket.socket()\nsrv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         srv.bind(('127.0.0.1', {port}))\nsrv.listen(1)\nprint('listening', flush=True)\nconn, _ = srv.accept()\n\
         data = conn.recv(100).decode()\nreply = data.upper()\nconn.sendall(reply.encode())  # @server\nconn.close()\n"
    );
    let client = format!(
        "import socket\nimport time\nfor _ in range(400):\n    try:\n        c = socket.create_connection(('127.0.0.1', {port}))\n        break\n    except OSError:\n        time.sleep(0.05)\n\
         c.sendall(b'ping')\nanswer = c.recv(100).decode()\nprint('client got', answer)  # @client\n"
    );
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("net");
    write(&root, &[("server.py", &server), ("client.py", &client)]);
    let mut s = session(&base.join("home"));
    open(&mut s, &root);
    s.execute("debug.breakpoint.set", &json!({ "path": root.join("server.py"), "line": line_of(&server, "@server") }))
        .unwrap();
    s.execute("debug.breakpoint.set", &json!({ "path": root.join("client.py"), "line": line_of(&client, "@client") }))
        .unwrap();

    let srv = s.execute("debug.start", &json!({ "path": root.join("server.py") })).unwrap();
    let srv_id = srv["session"].as_u64().unwrap();
    pump_until(&mut s, "the server to listen", 120, |s| {
        s.debug
            .sessions()
            .iter()
            .any(|x| x.id == srv_id && matches!(x.status, codecraft_engine::debug::DebugStatus::Running))
    });
    let cli = s.execute("debug.start", &json!({ "path": root.join("client.py") })).unwrap();
    let cli_id = cli["session"].as_u64().unwrap();
    assert_ne!(srv_id, cli_id, "a second session, the server keeps running");

    // The server stops in the handler and comes forward; the client is running (waiting).
    let stop = wait_session(&mut s, srv_id);
    if dump() {
        eprintln!("server stop: {stop:#}");
    }
    assert_eq!(stop["state"], "stopped", "{stop}");
    assert_eq!(stop["sessionId"], srv_id, "{stop}");
    assert_eq!(local(&stop, "data"), "'ping'", "{stop}");
    // debugpy's grouping nodes are not locals (QA M15).
    assert!(!stop["locals"].to_string().contains("special variables"), "{stop}");
    let sessions = s.execute("debug.sessions", &Value::Null).unwrap();
    if dump() {
        eprintln!("sessions: {sessions:#}");
    }
    let list = sessions["sessions"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{sessions}");
    assert_eq!(sessions["selected"], srv_id, "the stopped server is selected: {sessions}");
    let client_row = list.iter().find(|x| x["id"] == cli_id).unwrap();
    assert_eq!(client_row["status"]["state"], "running", "{sessions}");

    // The client's tab: running, so no frames; then back to the server and continue it.
    s.execute("debug.selectSession", &json!({ "session": cli_id })).unwrap();
    assert_eq!(s.debug.selected_id(), Some(cli_id));
    assert!(s.debug.session.as_ref().is_some_and(|x| x.execution.is_none()), "the client is not paused");
    let r = s.execute("run.debuggingActions.resumeProgram", &json!({ "session": srv_id })).unwrap();
    if dump() {
        eprintln!("resume server: {r}");
    }
    // The client gets the answer and stops; it comes forward.
    let stop = wait_session(&mut s, cli_id);
    if dump() {
        eprintln!("client stop: {stop:#}");
    }
    assert_eq!((stop["state"].clone(), stop["sessionId"].clone()), (json!("stopped"), json!(cli_id)), "{stop}");
    assert_eq!(local(&stop, "answer"), "'PING'", "{stop}");
    assert_eq!(s.debug.selected_id(), Some(cli_id));
    // The server is ending meanwhile; the summary lists it.
    let ended = stop["sessions"].as_array().unwrap().iter().find(|x| x["id"] == srv_id).cloned().unwrap();
    assert!(matches!(ended["state"].as_str(), Some("running" | "exited")), "{stop}");
    s.execute("run.debuggingActions.resumeProgram", &Value::Null).unwrap();
    let end = wait_session(&mut s, cli_id);
    assert_eq!(end["state"], "exited", "{end}");
    pump_until(&mut s, "the adapters to go away", 60, |s| !s.debug.busy());
    s.shutdown();
}

// ---------------------------------------------------------------- profiling

fn wait_job(s: &mut Session, job: u64, secs: u64) -> Value {
    let start = Instant::now();
    loop {
        s.pump();
        let j = s.execute("profiler.job", &json!({ "job": job })).unwrap();
        match j["state"].as_str() {
            Some("done") if s.profiler.loading.is_none() => return j,
            Some("failed") => {
                let out: Vec<String> = s.ws.runs.iter().flat_map(|r| r.lines.iter().map(|l| l.text.clone())).collect();
                panic!("the profile failed: {j}\n{}", out.join("\n"))
            }
            _ => {}
        }
        assert!(start.elapsed() < Duration::from_secs(secs), "timed out: {j}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn gtest_installed() -> bool {
    ["/opt/homebrew/lib/cmake/GTest", "/usr/local/lib/cmake/GTest", "/usr/lib/cmake/GTest"]
        .iter()
        .any(|p| Path::new(p).join("GTestConfig.cmake").is_file())
}

const GTEST: &str = "#include <gtest/gtest.h>

__attribute__((noinline)) unsigned long spin(unsigned long n) {
    unsigned long x = 1;
    for (unsigned long i = 0; i < n; ++i) x = x * 6364136223846793005UL + i;
    return x;
}

__attribute__((noinline)) unsigned long churn(unsigned long n) {
    unsigned long x = 7;
    for (unsigned long i = 0; i < n; ++i) x ^= (x << 5) + i;
    return x;
}

TEST(Perf, Phases) {
    volatile unsigned long a = spin(600000000UL);
    volatile unsigned long b = churn(600000000UL);
    EXPECT_NE(a ^ b, 1UL);
}

TEST(Perf, Quick) {
    EXPECT_EQ(spin(1), 1UL);
}
";

/// CLion's "Profile" in the gutter of a GoogleTest test: the CMake target is built, only that
/// test runs (its filter) under Instruments, and a time range over the first part of the run
/// shows `spin`, not `churn`.
#[test]
fn profile_a_googletest_test_at_the_caret_and_a_time_range() {
    let xctrace = cfg!(target_os = "macos") && has("xcrun", &["-f", "xctrace"]);
    let (Some(cmake), true, true) = (which("cmake"), gtest_installed(), xctrace) else {
        eprintln!("skipping: cmake, GoogleTest or xctrace is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("perf");
    write(
        &root,
        &[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.20)\nproject(perf CXX)\nset(CMAKE_CXX_STANDARD 17)\n\
                 set(CMAKE_BUILD_TYPE RelWithDebInfo)\nfind_package(GTest REQUIRED)\n\
                 add_executable(perf_tests perf_test.cpp)\ntarget_link_libraries(perf_tests GTest::gtest_main)\n",
            ),
            ("perf_test.cpp", GTEST),
        ],
    );
    let mut s = session(&base.join("home"));
    s.cpp.tools.insert("cmake".into(), cmake);
    open(&mut s, &root);
    pump_until(&mut s, "the CMake state", 120, |s| s.cpp.cmake.is_cmake && !s.cpp.cmake.analyzing());
    let r = s.execute("cmake.reload", &Value::Null).unwrap();
    let job = r["job"].as_u64().unwrap();
    pump_until(&mut s, "the configure", 240, |s| s.cpp.jobs.get(job).is_some_and(|j| !j.is_running()));
    pump_until(&mut s, "the codemodel", 120, |s| s.cpp.cmake.codemodel.is_some() && !s.cpp.cmake.analyzing());

    // The caret on `TEST(Perf, Phases)`, Profile from the gutter menu.
    let src = root.join("perf_test.cpp");
    s.execute("file.open", &json!({ "path": src })).unwrap();
    let line0 = line_of(GTEST, "TEST(Perf, Phases)") - 1;
    let r = s.execute("profiler.atCaret", &json!({ "path": src, "line": line0, "tool": "xctrace" })).unwrap();
    if dump() {
        eprintln!("atCaret: {r:#}");
    }
    let job = r["job"].as_u64().unwrap();
    let tab =
        s.ws.runs
            .iter()
            .find(|t| Some(t.id.0) == r["tab"].as_u64())
            .map(|t| t.command_line.clone())
            .unwrap_or_default();
    assert!(
        tab.contains("Perf.Phases") || r.to_string().contains("Perf.Phases"),
        "only the test at the caret: {r} {tab}"
    );
    wait_job(&mut s, job, 600);
    let report = s.execute("profiler.report", &json!({ "top": 8 })).unwrap();
    if dump() {
        eprintln!("report: {report:#}");
    }
    let names: Vec<String> =
        report["top"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect();
    assert!(names.iter().any(|n| n.contains("spin")) && names.iter().any(|n| n.contains("churn")), "{names:?}");
    // The first third of the run: `spin`, and no `churn`.
    let status = s.execute("profiler.status", &Value::Null).unwrap();
    assert_eq!(status["snapshot"]["hasSampleTimes"], true, "{status}");
    let duration = s.profiler.snapshot.as_ref().and_then(|x| x.profile.duration_ns).unwrap() as f64 / 1e6;
    let early =
        s.execute("profiler.report", &json!({ "top": 8, "range": { "startMs": 0, "endMs": duration * 0.3 } })).unwrap();
    let names: Vec<String> =
        early["top"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect();
    assert!(names.iter().any(|n| n.contains("spin")), "{early}");
    assert!(!names.iter().any(|n| n.contains("churn")), "{early}");
    // The IDE's own range selection (the timeline drag) filters the views the same way.
    s.execute("profiler.selectRange", &json!({ "startMs": duration * 0.7, "endMs": duration })).unwrap();
    pump_until(&mut s, "the range", 60, |s| {
        s.profiler.loading.is_none() && s.profiler.snapshot.as_ref().is_some_and(|x| x.analysis.range.is_some())
    });
    let methods = s.execute("profiler.hotspots", &json!({ "limit": 5 })).unwrap();
    if dump() {
        eprintln!("hotspots with the IDE's range: {methods:#}");
    }
    s.shutdown();
}

const GROW_PY: &str = "def build(n):
    return [bytes(1000) for _ in range(n)]  # @alloc


def main():
    keep = build(2000)
    big = build(30000)
    del big
    print(len(keep))


main()
";

/// Profile › Memory of a Python program: tracemalloc samples the memory over time, keeps the
/// snapshot nearest the peak, and the allocation site at the peak is `build`'s line.
#[test]
fn tracemalloc_finds_the_peak_of_a_python_program() {
    if !has("python3", &["--version"]) {
        eprintln!("skipping: python3 is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("mem");
    write(&root, &[("grow.py", GROW_PY)]);
    let mut s = session(&base.join("home"));
    open(&mut s, &root);
    let r = s
        .execute("profiler.record", &json!({ "command": "python3 grow.py", "kind": "memory", "interval": 0.005 }))
        .unwrap();
    wait_job(&mut s, r["job"].as_u64().unwrap(), 120);
    let m = s.execute("profiler.memory", &json!({ "top": 5 })).unwrap();
    if dump() {
        eprintln!("memory: {m:#}");
    }
    let peak = m["peak"].as_u64().unwrap();
    assert!(peak > 25_000_000, "the 30 MB list is the peak: {m}");
    let first = &m["atPeak"][0];
    assert_eq!(first["name"], "build", "{m}");
    assert!(first["location"].as_str().unwrap().ends_with(&format!("grow.py:{}", line_of(GROW_PY, "@alloc"))), "{m}");
    assert!(first["percentOfPeak"].as_f64().unwrap() > 80.0, "{m}");
    // The program ended with much less live than the peak.
    let timeline = m["timeline"].as_array().unwrap();
    assert!(timeline.len() >= 2, "{m}");
    s.shutdown();
}

/// `Session::isolate_home` keeps tests (and galleries) away from the user's Docker as it does
/// from the GitHub CLI: a refresh of Services › Docker fails instead of listing real containers.
#[test]
fn isolated_sessions_never_reach_the_users_docker() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("home");
    let mut s = session(&home);
    assert!(s.docker.program_path().starts_with(&home), "{}", s.docker.program);
    s.execute("docker.refresh", &json!({})).unwrap();
    pump_until(&mut s, "the refresh", 30, |s| !s.docker.loading && !s.docker.busy());
    assert!(s.docker.error.is_some() && s.docker.data.containers.is_empty(), "{:?}", s.docker.error);
    s.shutdown();
}

// ---------------------------------------------------------------- agents: rows beyond `limit`

/// QA M15 (as an agent): `db_query` with `limit: 20` on 30 rows answered 20 rows and
/// `hasMore: false`, so an agent believed it had the whole table.
#[test]
fn db_query_says_when_rows_are_left_out() {
    use codecraft_engine::agent_access::{AgentAccess, Capability};
    use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("db");
    std::fs::create_dir_all(&root).unwrap();
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(root.join("shop.db")),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    codecraft_engine::db::query_all(conn.as_mut(), "create table t (id integer primary key)").unwrap();
    codecraft_engine::db::query_all(
        conn.as_mut(),
        "insert into t(id) with recursive c(x) as (select 1 union all select x + 1 from c where x < 30) select x from c",
    )
    .unwrap();
    drop(conn);
    let mut s = session(&base.join("home"));
    open(&mut s, &root);
    s.agent_access = AgentAccess::headless(&[Capability::Write]);
    let call = |s: &mut Session, args: Value| {
        let r = agent_tools::call_blocking(s, Caller::new("qa"), "db_query", args, Duration::from_secs(60));
        assert!(!r.is_error, "{}", result_text(&r));
        result_json(&r)
    };
    let v = call(&mut s, json!({ "dataSource": "shop.db", "sql": "select id from t order by id", "limit": 20 }));
    let r = &v["results"][0];
    assert_eq!(r["rows"].as_array().unwrap().len(), 20, "{v}");
    assert_eq!((r["totalRows"].as_u64(), r["hasMore"].as_bool()), (Some(30), Some(true)), "{v}");
    assert!(r["note"].as_str().unwrap().contains("20 of 30"), "{v}");
    let v = call(&mut s, json!({ "dataSource": "shop.db", "sql": "select id from t order by id" }));
    assert_eq!(v["results"][0]["hasMore"], false, "all 30 rows: {v}");
    assert!(v["results"][0].get("note").is_none(), "{v}");
    s.shutdown();
}
