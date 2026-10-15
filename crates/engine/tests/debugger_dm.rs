//! M15 track DM end to end with the real adapters: several debug sessions at once (two native
//! programs under lldb-dap, two Python programs under debugpy driven by an agent's tools with
//! `session`), C++ references read as their values (QA #23), Rust values (nested enums, bytes as
//! numbers, chars) without LLDB's "no plugin for rust" line (QA #22), watches kept with the
//! project, and Debug at the caret for a GoogleTest test (CMake + GoogleTest). Each test skips
//! cleanly when its tools are missing; everything runs in temp dirs with an isolated home.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::agent_access::{AgentAccess, Capability};
use codecraft_engine::agent_tools::{self, Caller, result_json, result_text};
use codecraft_engine::debug::console_text;
use codecraft_engine::{Session, dap};

const WAIT: Duration = Duration::from_secs(240);

fn console(s: &Session) -> String {
    s.debug.console.iter().map(console_text).collect::<Vec<_>>().join("\n")
}

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            panic!("timed out waiting for {what}; sessions {:?}; console:\n{}", s.debug.sessions(), console(s));
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Resolve a `{pending, token}` answer by pumping and polling, as an agent host does.
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

fn open_project(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&dir.join(".cc-home"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    s
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

fn tool(name: &str) -> Option<PathBuf> {
    dap::discover::which(name)
}

fn compile(dir: &Path, compiler: &str, source: &str, file: &str, exe: &str) -> Option<PathBuf> {
    let src = dir.join(file);
    std::fs::write(&src, source).unwrap();
    let out = dir.join(exe);
    let std = if compiler == "clang++" { "-std=c++17" } else { "-std=c11" };
    let ok = std::process::Command::new(compiler).args(["-g", "-O0", std, "-o"]).arg(&out).arg(&src).status().ok()?;
    ok.success().then_some(out)
}

// ---------------------------------------------------------------- two native programs

const SERVER: &str = r#"#include <cstdio>
#include <string>

struct Item {
    int id;
    std::string name;
};

int use(const Item& it, int& n, const int& k) {
    std::printf("server %d %d %d\n", it.id, n, k); // @server
    return it.id + n + k;
}

int main() {
    Item a{7, "apple"};
    int n = 3;
    return use(a, n, 5) == 15 ? 0 : 1;
}
"#;

const CLIENT: &str = r#"#include <stdio.h>

int main(void) {
    int total = 0;
    for (int i = 0; i < 3; i++) {
        total += i;
    }
    printf("client %d\n", total); // @client
    return 0;
}
"#;

#[test]
fn two_native_programs_are_debugged_at_once() {
    if dap::discover::find_lldb_dap().is_err() || tool("clang++").is_none() || tool("clang").is_none() {
        eprintln!("skipping: lldb-dap or clang is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let server = compile(&dir, "clang++", SERVER, "server.cpp", "server").unwrap();
    let client = compile(&dir, "clang", CLIENT, "client.c", "client").unwrap();
    let mut s = open_project(&dir);
    s.execute("debug.breakpoint.set", &json!({ "path": dir.join("server.cpp"), "line": line_of(SERVER, "@server") })).unwrap();
    s.execute("debug.breakpoint.set", &json!({ "path": dir.join("client.c"), "line": line_of(CLIENT, "@client") })).unwrap();

    let started = s.execute("debug.start", &json!({ "target": { "kind": "executable", "name": "server", "program": server } })).unwrap();
    let server_id = started["session"].as_u64().unwrap();
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    assert_eq!((stop["state"].clone(), stop["sessionId"].clone()), (json!("stopped"), json!(server_id)), "{stop}");
    // QA #23: references read as what they refer to.
    assert_eq!(local(&stop, "it"), "{id=7, name=\"apple\"}", "{stop}");
    assert_eq!(local(&stop, "n"), "3");
    assert_eq!(local(&stop, "k"), "5");

    // A second program starts while the first stays paused.
    let started = s.execute("debug.start", &json!({ "target": { "kind": "executable", "name": "client", "program": client } })).unwrap();
    let client_id = started["session"].as_u64().unwrap();
    assert_ne!(client_id, server_id);
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    assert_eq!(stop["sessionId"], json!(client_id), "{stop}");
    assert_eq!(local(&stop, "total"), "3");
    let sessions = stop["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{stop}");
    assert!(sessions.iter().all(|x| x["state"] == json!("stopped")), "{stop}");
    assert_eq!(s.debug.selected_id(), Some(client_id));
    // The editor's execution point is the selected session's.
    let exec = s.debug.session.as_ref().and_then(|x| x.execution.clone()).unwrap();
    assert!(exec.0.ends_with("client.c"), "{exec:?}");
    assert!(s.debug.breakpoints.iter().all(|b| b.verified == Some(true)), "{:?}", s.debug.breakpoints);

    // The server, through its id: its stop is still there; it runs to its end.
    let summary = s.execute("debug.stopSummary", &json!({ "session": server_id })).unwrap();
    assert_eq!(local(&summary, "n"), "3", "{summary}");
    let r = s.execute("debug.step", &json!({ "action": "continue", "session": server_id })).unwrap();
    assert_eq!(s.debug.selected_id(), Some(client_id), "acting on the server leaves the client selected");
    let end = resolve(&mut s, json!({ "pending": true, "token": r["token"] }));
    assert_eq!((end["state"].clone(), end["exitCode"].clone()), (json!("exited"), json!(0)), "{end}");
    assert_eq!(end["sessionId"], json!(server_id));
    let server_console = s.execute("debug.state", &json!({ "session": server_id })).unwrap()["console"].to_string();
    assert!(server_console.contains("server 7 3 5"), "{server_console}");
    // The client is still paused where it was, and its console has only its own output.
    assert!(s.debug.is_stopped());
    assert!(!console(&s).contains("server 7"), "{}", console(&s));
    let step = s.execute("debug.step", &json!({ "action": "over" })).unwrap();
    let step = resolve(&mut s, json!({ "pending": true, "token": step["token"] }));
    assert_eq!(step["sessionId"], json!(client_id), "{step}");
    // Stop All ends what is left.
    let stopped = s.execute("run.debuggingActions.stopAllPrograms", &Value::Null).unwrap();
    assert_eq!(stopped["debugSessions"], json!(1));
    pump_until(&mut s, "the adapters to go away", |s| !s.debug.busy());
    assert_eq!(s.debug.live_count(), 0);
    s.shutdown();
}

// ---------------------------------------------------------------- two Python programs, an agent

fn debugpy_python() -> bool {
    dap::discover::find_debugpy(None).is_ok()
}

const PY_SERVER: &str = "def handle(request):\n    reply = request.upper()\n    return reply  # @server\n\n\nprint('server', handle('ping'))\n";
const PY_CLIENT: &str = "def send(n):\n    total = n * 2\n    return total  # @client\n\n\nprint('client', send(21))\n";

fn call_ok(s: &mut Session, name: &str, args: Value) -> Value {
    let r = agent_tools::call_blocking(s, Caller::new("test agent"), name, args.clone(), WAIT);
    assert!(!r.is_error, "{name} {args} failed: {}", result_text(&r));
    result_json(&r)
}

#[test]
fn an_agent_drives_two_python_sessions() {
    if !debugpy_python() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("server.py"), PY_SERVER).unwrap();
    std::fs::write(dir.join("client.py"), PY_CLIENT).unwrap();
    let mut s = open_project(&dir);
    s.agent_access = AgentAccess::headless(&[Capability::Debug]);
    call_ok(&mut s, "debug_breakpoint_set", json!({ "path": "server.py", "line": line_of(PY_SERVER, "@server") }));
    call_ok(&mut s, "debug_breakpoint_set", json!({ "path": "client.py", "line": line_of(PY_CLIENT, "@client") }));

    let server = call_ok(&mut s, "debug_start", json!({ "file": "server.py" }));
    let server_id = server["sessionId"].as_u64().unwrap();
    let stop = call_ok(&mut s, "debug_wait_for_stop", json!({ "session": server_id, "timeoutMs": 120_000 }));
    assert_eq!((stop["state"].clone(), stop["sessionId"].clone()), (json!("stopped"), json!(server_id)), "{stop}");
    let client = call_ok(&mut s, "debug_start", json!({ "file": "client.py" }));
    let client_id = client["sessionId"].as_u64().unwrap();
    assert_ne!(client_id, server_id);
    // The client stops too (its own wait, by id).
    let stop = call_ok(&mut s, "debug_wait_for_stop", json!({ "session": client_id, "timeoutMs": 120_000 }));
    assert_eq!(stop["sessionId"], json!(client_id), "{stop}");
    assert_eq!(stop["sessions"].as_array().map(Vec::len), Some(2), "{stop}");

    // Each session evaluates in its own program.
    let ev = call_ok(&mut s, "debug_evaluate", json!({ "expression": "reply", "session": server_id }));
    assert_eq!(ev["value"], json!("'PING'"), "{ev}");
    let ev = call_ok(&mut s, "debug_evaluate", json!({ "expression": "total", "session": "client.py" }));
    assert_eq!(ev["value"], json!("42"), "{ev}");
    let vars = call_ok(&mut s, "debug_variables", json!({ "session": server_id }));
    assert!(vars.to_string().contains("reply"), "{vars}");
    let stack = call_ok(&mut s, "debug_stack", json!({ "session": server_id }));
    assert_eq!(stack["frames"][0]["name"], json!("handle"), "{stack}");

    // Continue the server: its summary says it exited, the client is still paused.
    let end = call_ok(&mut s, "debug_control", json!({ "action": "continue", "session": server_id, "timeoutMs": 120_000 }));
    assert_eq!((end["state"].clone(), end["sessionId"].clone()), (json!("exited"), json!(server_id)), "{end}");
    assert!(end["output"].to_string().contains("server PING"), "{end}");
    let paused = call_ok(&mut s, "debug_wait_for_stop", json!({ "timeoutMs": 10_000 }));
    assert_eq!((paused["state"].clone(), paused["sessionId"].clone()), (json!("stopped"), json!(client_id)), "{paused}");
    let bad = agent_tools::call_blocking(&mut s, Caller::new("test agent"), "debug_stack", json!({ "session": 999 }), WAIT);
    assert!(bad.is_error && result_text(&bad).contains("no debug session"), "{}", result_text(&bad));
    let end = call_ok(&mut s, "debug_control", json!({ "action": "continue", "timeoutMs": 120_000 }));
    assert_eq!((end["state"].clone(), end["sessionId"].clone()), (json!("exited"), json!(client_id)), "{end}");
    pump_until(&mut s, "the adapters to go away", |s| !s.debug.busy());
    s.shutdown();
}

// ---------------------------------------------------------------- Rust values

const RUST: &str = r#"#[derive(Debug)]
enum Inner {
    A(u8),
    B { x: i32 },
}

#[derive(Debug)]
enum Outer {
    Wrap(Inner),
    Pair(Option<i32>, Inner),
    Nothing,
}

fn main() {
    let nested: Option<Option<i32>> = Some(Some(5));
    let outer = Outer::Wrap(Inner::B { x: 9 });
    let pair = Outer::Pair(Some(1), Inner::A(200));
    let res: Result<Option<u8>, String> = Ok(Some(3));
    let byte: u8 = 65;
    let signed: i8 = -3;
    let ch = 'z';
    let boxed_byte = Box::new(7u8);
    let bytes = [1u8, 2, 255];
    let none = Outer::Nothing;
    println!("{nested:?} {outer:?} {pair:?} {res:?} {byte} {signed} {ch} {boxed_byte} {bytes:?} {none:?}"); // @values
}
"#;

fn cargo() -> Option<PathBuf> {
    tool("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

#[test]
fn rust_values_nest_bytes_read_as_numbers_and_lldb_noise_is_gone() {
    if dap::discover::find_lldb_dap().is_err() || cargo().is_none() {
        eprintln!("skipping: lldb-dap or cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ccdmvalues\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[lints.rust]\ndead_code = \"allow\"\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, RUST).unwrap();
    let mut s = open_project(&dir);
    s.execute("debug.breakpoint.set", &json!({ "path": main, "line": line_of(RUST, "@values") })).unwrap();
    let started = s.execute("debug.start", &json!({ "path": main, "line": line_of(RUST, "fn main") })).unwrap();
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    eprintln!("values: {:#}", stop["locals"]);
    for (name, want) in [
        ("nested", "Some(Some(5))"),
        ("outer", "Wrap(B { x: 9 })"),
        ("pair", "Pair(Some(1), A(200))"),
        ("res", "Ok(Some(3))"),
        ("byte", "65"),
        ("signed", "-3"),
        ("ch", "'z'"),
        ("boxed_byte", "7"),
        ("bytes", "[1, 2, 255]"),
        ("none", "Nothing"),
    ] {
        assert_eq!(local(&stop, name), want, "{name}: {stop}");
    }
    // QA #22: no "This version of LLDB has no plugin for the language rust" line.
    assert!(!console(&s).contains("no plugin for the language"), "{}", console(&s));
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

// ---------------------------------------------------------------- watches with the project

#[test]
fn watches_are_saved_with_the_project_and_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
    let mut s = open_project(&dir);
    s.execute("debug.addWatch", &json!({ "expression": "x + 1" })).unwrap();
    s.execute("debug.addWatch", &json!({ "expression": "len(items)" })).unwrap();
    s.execute("debug.removeWatch", &json!({ "index": 0 })).unwrap();
    s.execute("debug.addWatch", &json!({ "expression": "total" })).unwrap();
    s.shutdown();
    let saved = std::fs::read_to_string(dir.join(".codecraft/breakpoints.json")).unwrap();
    assert!(saved.contains("len(items)") && saved.contains("total") && !saved.contains("x + 1"), "{saved}");
    let again = open_project(&dir);
    let watches: Vec<&str> = again.debug.watches.iter().map(|w| w.expression.as_str()).collect();
    assert_eq!(watches, ["len(items)", "total"]);
}

// ---------------------------------------------------------------- jest at the caret

#[test]
fn a_jest_test_debugs_under_js_debug_or_says_how_to_get_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("package.json"), r#"{"devDependencies":{"jest":"^29"}}"#).unwrap();
    std::fs::create_dir_all(dir.join("node_modules/jest/bin")).unwrap();
    std::fs::write(dir.join("node_modules/jest/bin/jest.js"), "console.log('fake jest', process.argv.slice(2).join(' '));\n").unwrap();
    let test = dir.join("math.test.js");
    std::fs::write(&test, "test('adds', () => {\n  expect(1 + 1).toBe(2);\n});\n").unwrap();
    let mut s = open_project(&dir);
    let started = s.execute("debug.atCaret", &json!({ "path": test, "line": 1 })).unwrap();
    assert_eq!(started["name"], json!("adds"), "{started}");
    if dap::discover::find_js_debug().is_ok() {
        // js-debug is installed: the (fake) runner runs under it with the test's filter.
        let v = s.execute("debug.waitForStop", &json!({ "timeoutMs": WAIT.as_millis() as u64 })).unwrap();
        let end = resolve(&mut s, v);
        assert_eq!(end["state"], json!("exited"), "{end}");
        assert!(console(&s).contains("fake jest --runInBand"), "{}", console(&s));
    } else {
        // Without js-debug the session fails at once with where to get it.
        pump_until(&mut s, "the start to fail", |s| s.debug.session.as_ref().is_some_and(|x| x.status().is_finished()));
        let status = s.debug.session.as_ref().unwrap().status();
        assert!(matches!(&status, codecraft_engine::debug::DebugStatus::Failed { message } if message.contains("js-debug")), "{status:?}");
    }
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

// ---------------------------------------------------------------- GoogleTest at the caret

const GTEST: &str = r#"#include <gtest/gtest.h>

int add(int a, int b) { return a + b; }

TEST(MathTest, Adds) {
  int r = add(1, 2);
  EXPECT_EQ(r, 3); // @adds
}

TEST(MathTest, Subs) {
  int d = 2 - 1;
  EXPECT_EQ(d, 1); // @subs
}
"#;

fn gtest_installed() -> bool {
    ["/opt/homebrew/lib/cmake/GTest", "/usr/local/lib/cmake/GTest", "/usr/lib/cmake/GTest"]
        .iter()
        .any(|p| Path::new(p).join("GTestConfig.cmake").is_file())
}

#[test]
fn a_googletest_test_debugs_from_the_caret() {
    if dap::discover::find_lldb_dap().is_err() || tool("cmake").is_none() || !gtest_installed() {
        eprintln!("skipping: lldb-dap, cmake or GoogleTest is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(
        dir.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.20)\nproject(p CXX)\nset(CMAKE_CXX_STANDARD 17)\nfind_package(GTest REQUIRED)\n\
         add_executable(math_tests math_test.cpp)\ntarget_link_libraries(math_tests GTest::gtest_main)\n",
    )
    .unwrap();
    let src = dir.join("math_test.cpp");
    std::fs::write(&src, GTEST).unwrap();
    let mut s = open_project(&dir);
    // The isolated session finds tools only where it is told.
    s.cpp.tools.insert("cmake".into(), tool("cmake").unwrap());
    pump_until(&mut s, "the CMake state", |s| s.cpp.cmake.is_cmake && !s.cpp.cmake.analyzing());
    let r = s.execute("cmake.reload", &Value::Null).unwrap();
    let job = r["job"].as_u64().unwrap();
    pump_until(&mut s, "the configure", |s| s.cpp.jobs.get(job).is_some_and(|j| !j.is_running()));
    pump_until(&mut s, "the codemodel", |s| s.cpp.cmake.codemodel.is_some() && !s.cpp.cmake.analyzing());
    // Breakpoints in both tests; Debug at the caret on `Subs` runs only that test.
    s.execute("debug.breakpoint.set", &json!({ "path": src, "line": line_of(GTEST, "@adds") })).unwrap();
    s.execute("debug.breakpoint.set", &json!({ "path": src, "line": line_of(GTEST, "@subs") })).unwrap();
    let started = s.execute("debug.start", &json!({ "path": src, "line": line_of(GTEST, "TEST(MathTest, Subs)") })).unwrap();
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    assert_eq!(stop["state"], json!("stopped"), "{stop}\n{}", console(&s));
    assert_eq!(stop["source"]["line"], json!(line_of(GTEST, "@subs")), "{stop}");
    assert_eq!(local(&stop, "d"), "1");
    // `EXPECT_EQ` expands to code at several addresses of the line: LLDB may stop there again.
    let mut end = Value::Null;
    for _ in 0..4 {
        let r = s.execute("debug.step", &json!({ "action": "continue" })).unwrap();
        end = resolve(&mut s, json!({ "pending": true, "token": r["token"] }));
        if end["state"] != json!("stopped") {
            break;
        }
        assert_eq!(end["source"]["line"], json!(line_of(GTEST, "@subs")), "{end}");
    }
    assert_eq!((end["state"].clone(), end["exitCode"].clone()), (json!("exited"), json!(0)), "{end}\n{}", console(&s));
    let out = console(&s);
    assert!(out.contains("--gtest_filter=MathTest.Subs") || out.contains("1 test"), "{out}");
    assert!(!out.contains("MathTest.Adds"), "only the test at the caret ran: {out}");
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}
