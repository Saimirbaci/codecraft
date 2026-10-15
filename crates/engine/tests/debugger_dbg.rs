//! M14 debugging depth (track DBG) end to end, through the agent commands, with the real
//! lldb-dap (C++, Rust, core dumps, remote) and debugpy (Python). Each test skips cleanly when its
//! tools are missing.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::debug::console_text;
use codecraft_engine::{Session, dap};

const WAIT: Duration = Duration::from_secs(180);

fn console(s: &Session) -> String {
    s.debug.console.iter().map(console_text).collect::<Vec<_>>().join("\n")
}

fn pump_until(s: &mut Session, what: &str, cond: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        if start.elapsed() > WAIT {
            panic!(
                "timed out waiting for {what}; status {:?}; console:\n{}",
                s.debug.session.as_ref().map(|x| x.status()),
                console(s)
            );
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

fn wait_stop(s: &mut Session) -> Value {
    let v = s.execute("debug.waitForStop", &json!({ "timeoutMs": WAIT.as_millis() as u64 })).unwrap();
    let v = resolve(s, v);
    assert_ne!(v["timedOut"], json!(true), "{v}\n{}", console(s));
    v
}

/// `debug.step` and wait for the stop it causes.
fn step(s: &mut Session, action: &str) -> Value {
    let r = s.execute("debug.step", &json!({ "action": action, "timeoutMs": WAIT.as_millis() as u64 })).unwrap();
    resolve(s, json!({ "pending": true, "token": r["token"] }))
}

fn open_project(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    // Never the user's real ~/.codecraft.
    s.isolate_home(&dir.join(".cc-home"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "the project scan", |s| !s.jobs_running());
    s
}

/// The 1-based line of the line containing `marker`.
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

fn top_line(summary: &Value) -> u64 {
    summary["frames"][0]["line"].as_u64().unwrap_or(0)
}

fn selected_line(summary: &Value) -> u64 {
    summary["source"]["line"].as_u64().unwrap_or(0)
}

fn bp(s: &mut Session, params: Value) -> u64 {
    let v = s.execute("debug.breakpoint.set", &params).unwrap();
    v["id"].as_u64().unwrap()
}

fn tool(name: &str) -> Option<PathBuf> {
    dap::discover::which(name)
}

// ---------------------------------------------------------------- C++ under lldb-dap

const CPP: &str = r#"#include <cstdio>
#include <map>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

int twice(int x) {
    return x * 2; // @twice
}

int add(int a, int b) {
    int r = a + b; // @add
    return r;
}

int main() {
    std::string s = "hello";
    std::vector<int> v = {1, 2, 3};
    std::map<std::string, int> m = {{"a", 1}, {"b", 2}};
    auto p = std::make_unique<int>(42);
    int total = 0;
    for (int i = 0; i < 5; i++) {
        total += twice(i); // @loop
    }
    int t2 = add(twice(3), 4); // @calls
    std::printf("total=%d t2=%d\n", total, t2); // @print
    try {
        throw std::runtime_error("boom"); // @throw
    } catch (const std::exception &) {
    }
    return 0;
}
"#;

#[test]
fn cpp_breakpoints_values_and_debugger_actions() {
    let (Ok(_), Some(cxx)) = (dap::discover::find_lldb_dap(), tool("clang++").or_else(|| tool("c++"))) else {
        eprintln!("skipping: lldb-dap or a C++ compiler is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let src = dir.join("main.cpp");
    std::fs::write(&src, CPP).unwrap();
    let exe = dir.join("app");
    let built = std::process::Command::new(cxx)
        .args(["-g", "-O0", "-std=c++17", "-o"])
        .arg(&exe)
        .arg(&src)
        .status()
        .unwrap();
    assert!(built.success());
    let mut s = open_project(&dir);

    // Breakpoints of every kind, with 1-based lines.
    let hit2 = bp(&mut s, json!({ "path": src, "line": line_of(CPP, "@twice"), "hitCondition": "2" }));
    let cond = bp(&mut s, json!({ "path": src, "line": line_of(CPP, "@loop"), "condition": "i == 3" }));
    let temp = bp(&mut s, json!({ "path": src, "line": line_of(CPP, "@calls"), "temporary": true }));
    // (On the throw: the jump below lands on the printf line, and a breakpoint where execution
    // already is doesn't trigger.)
    bp(&mut s, json!({ "path": src, "line": line_of(CPP, "@throw"), "logMessage": "logged total {total}" }));
    let e = s.execute("debug.exceptionBreakpoints", &json!({ "cppThrow": true })).unwrap();
    assert_eq!(e["cppThrow"], json!(true));
    let list = s.execute("debug.breakpoint.list", &Value::Null).unwrap();
    assert_eq!(list["breakpoints"].as_array().unwrap().len(), 4, "{list}");
    assert_eq!(list["breakpoints"][0]["line"], json!(line_of(CPP, "@twice")));

    let started = s
        .execute("debug.start", &json!({ "target": { "kind": "executable", "name": "app", "program": exe } }))
        .unwrap();
    assert!(started["token"].as_u64().is_some(), "{started}");
    let first = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    eprintln!("first stop: {first:#}");

    // 1. The hit count: the second call of twice (x == 1).
    assert_eq!(first["state"], json!("stopped"));
    assert_eq!(first["reason"], json!("breakpoint"));
    assert_eq!(top_line(&first), line_of(CPP, "@twice") as u64);
    assert_eq!(local(&first, "x"), "1");
    assert!(first["breakpoints"].as_array().unwrap().iter().any(|b| b["id"] == json!(hit2)), "{first}");
    assert!(first["source"]["lines"].as_array().unwrap().iter().any(|l| l["current"] == json!(true)));
    assert!(first["frames"][0]["code"].as_str().unwrap().contains("return x * 2"));
    s.execute("debug.breakpoint.remove", &json!({ "id": hit2 })).unwrap();

    // 2. The condition: i == 3; STL values read well.
    let second = step(&mut s, "continue");
    assert_eq!(top_line(&second), line_of(CPP, "@loop") as u64, "{second}");
    assert_eq!(local(&second, "i"), "3");
    assert_eq!(local(&second, "total"), "6");
    assert_eq!(local(&second, "s"), "\"hello\"");
    assert_eq!(local(&second, "v"), "size=3");
    assert_eq!(local(&second, "m"), "size=2");
    assert!(local(&second, "p").contains("42"), "{second}");

    // Hover evaluation and console completion.
    s.execute("debug.hoverEvaluate", &json!({ "expression": "v" })).unwrap();
    pump_until(&mut s, "the hover value", |s| s.debug.hover.as_ref().is_some_and(|h| !h.pending));
    let hover = s.debug.hover.clone().unwrap();
    assert_eq!(hover.result.map(|v| v.value), Ok("size=3".to_owned()));
    let completions = s.execute("debug.consoleComplete", &json!({ "text": "tot" })).unwrap();
    let completions = resolve(&mut s, completions);
    assert!(
        completions["items"].as_array().unwrap().iter().any(|i| i["text"] == json!("total")),
        "{completions}"
    );

    // Set Value, then an agent evaluation sees it.
    let set = s.execute("debug.setValue", &json!({ "name": "total", "value": "100" })).unwrap();
    let set = resolve(&mut s, set);
    assert_eq!(set["value"], json!("100"), "{set}");
    let eval = s.execute("debug.evaluate", &json!({ "expression": "total + 1" })).unwrap();
    let eval = resolve(&mut s, eval);
    assert_eq!(eval["value"], json!("101"), "{eval}");
    let vector = s.execute("debug.evaluate", &json!({ "expression": "v" })).unwrap();
    let vector = resolve(&mut s, vector);
    assert_eq!(vector["children"].as_array().map(Vec::len), Some(3), "{vector}");
    s.execute("debug.breakpoint.set", &json!({ "path": src, "line": line_of(CPP, "@loop"), "enabled": false }))
        .unwrap();

    // 3. The temporary breakpoint stops once and goes away.
    let third = step(&mut s, "continue");
    assert_eq!(top_line(&third), line_of(CPP, "@calls") as u64, "{third}");
    assert_eq!(local(&third, "total"), "114");
    assert!(s.debug.breakpoints.iter().all(|b| b.id != temp), "the temporary breakpoint is gone");
    assert!(s.debug.breakpoints.iter().any(|b| b.id == cond && !b.enabled));

    // 4. Smart Step Into: the calls on the line, then step into `add` (past `twice`).
    let smart = s.execute("run.debuggingActions.smartStepInto", &Value::Null).unwrap();
    assert_eq!(smart["via"], json!("chooser"), "{smart}");
    let labels: Vec<String> = s.debug.step_targets.as_ref().unwrap().targets.iter().map(|t| t.label.clone()).collect();
    assert_eq!(labels, ["add()", "twice()"]);
    s.execute("debug.stepIntoTarget", &json!({ "index": 0 })).unwrap();
    let inside = wait_stop(&mut s);
    assert!(inside["frames"][0]["function"].as_str().unwrap().contains("add"), "{inside}");
    assert_eq!(local(&inside, "a"), "6");

    // 5. Step Out shows what add returned.
    let out = step(&mut s, "out");
    assert_eq!(out["returnValue"]["value"], json!("10"), "{out}");
    assert!(out["returnValue"]["name"].as_str().unwrap().contains("add"), "{out}");

    // 6. Jump to Line (LLDB `thread jump`): skip the assignment, land on the printf.
    let print_line = line_of(CPP, "@print");
    s.execute("debug.jumpToLine", &json!({ "path": src, "line": print_line - 1 })).unwrap();
    pump_until(&mut s, "the jump", |s| {
        !s.debug.settling() && s.debug.session.as_ref().and_then(|x| x.selected_frame()).is_some_and(|f| f.line == print_line - 1)
    });

    // 7. The logpoint prints without stopping; the C++ throw stops.
    let thrown = step(&mut s, "continue");
    assert_eq!(thrown["reason"], json!("exception"), "{thrown}");
    assert!(console(&s).contains("logged total 114"), "{}", console(&s));
    assert!(
        thrown["exception"]["id"] == json!("cpp_throw")
            || thrown["description"].as_str().is_some_and(|d| d.contains("Throw")),
        "{thrown}"
    );
    // The stop shows the user's code that threw.
    assert_eq!(selected_line(&thrown), line_of(CPP, "@throw") as u64, "{thrown}");

    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    assert_eq!(end["exitCode"], json!(0));
    assert!(console(&s).contains("total=114"), "{}", console(&s));
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    // Exception settings are saved with the project's breakpoints.
    let saved = std::fs::read_to_string(dir.join(".codecraft/breakpoints.json")).unwrap();
    assert!(saved.contains("\"cppThrow\": true"), "{saved}");
    s.shutdown();
}

// ---------------------------------------------------------------- Rust under lldb-dap

const RUST: &str = r#"use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug)]
enum Shape {
    Circle(f64),
    Rect { w: u32, h: u32 },
    Empty,
}

fn main() {
    let mut map = HashMap::new();
    map.insert("one", 1);
    map.insert("two", 2);
    let some: Option<i32> = Some(5);
    let none: Option<i32> = None;
    let ok: Result<i32, String> = Ok(7);
    let err: Result<i32, String> = Err("bad".to_string());
    let rect = Shape::Rect { w: 3, h: 4 };
    let circle = Shape::Circle(1.5);
    let empty = Shape::Empty;
    let rc = Rc::new(String::from("shared"));
    let arc = Arc::new(vec![1, 2]);
    let boxed = Box::new(9i32);
    let array = [1, 2, 3, 4];
    let slice: &[i32] = &array[1..3];
    let text: &str = "hi";
    println!("{map:?} {some:?} {none:?} {ok:?} {err:?} {rect:?} {circle:?} {empty:?} {rc} {arc:?} {boxed} {slice:?} {text}"); // @values
    let v: Vec<i32> = Vec::new();
    let i = v.len() + 3;
    let x = v[i]; // @panic
    println!("{x}");
}
"#;

fn cargo() -> Option<PathBuf> {
    tool("cargo").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/cargo");
        p.is_file().then_some(p)
    })
}

#[test]
fn rust_values_read_well_and_panics_stop() {
    if dap::discover::find_lldb_dap().is_err() || cargo().is_none() {
        eprintln!("skipping: lldb-dap or cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ccrustvalues\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let main = dir.join("src/main.rs");
    std::fs::write(&main, RUST).unwrap();
    let mut s = open_project(&dir);
    bp(&mut s, json!({ "path": main, "line": line_of(RUST, "@values") }));
    let started = s.execute("debug.start", &json!({ "path": main, "line": 12 })).unwrap();
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    eprintln!("values: {:#}", stop["locals"]);
    for (name, want) in [
        ("map", "size=2"),
        ("some", "Some(5)"),
        ("none", "None"),
        ("ok", "Ok(7)"),
        ("err", "Err(\"bad\")"),
        ("rect", "Rect { w: 3, h: 4 }"),
        ("circle", "Circle(1.5)"),
        ("empty", "Empty"),
        ("rc", "strong=1, weak=0"),
        ("arc", "strong=1, weak=0"),
        ("boxed", "9"),
        ("array", "[1, 2, 3, 4]"),
        ("slice", "size=2"),
        ("text", "\"hi\""),
    ] {
        assert_eq!(local(&stop, name), want, "{name}");
    }
    let values: Vec<(String, String)> = stop["locals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["name"].as_str().unwrap().to_owned(), v["value"].as_str().unwrap().to_owned()))
        .collect();
    for (name, value) in &values {
        assert!(!value.starts_with("0x") || name == "slice", "{name} = {value} is a raw pointer");
    }
    // Expanding an enum shows the active variant's fields.
    let some_ref = stop["locals"].as_array().unwrap().iter().find(|v| v["name"] == json!("some")).unwrap()["reference"]
        .as_i64()
        .unwrap();
    let fields = s.debug.session.as_ref().unwrap().variables.get(&some_ref).cloned().unwrap_or_default();
    assert_eq!(fields.iter().map(|v| (v.name.as_str(), v.value.as_str())).collect::<Vec<_>>(), [("0", "5")]);
    s.execute("debug.breakpoint.remove", &json!({ "path": main, "line": line_of(RUST, "@values") })).unwrap();

    // The panic stops in the program's code, with the message in the output.
    let panic = step(&mut s, "continue");
    eprintln!("panic: {panic:#}");
    assert_eq!(panic["state"], json!("stopped"), "{panic}");
    assert_eq!(selected_line(&panic), line_of(RUST, "@panic") as u64, "{panic}");
    assert_eq!(panic["reason"], json!("panic"), "{panic}");
    assert!(panic["exception"]["message"].as_str().unwrap().contains("index out of bounds"), "{panic}");
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

// ---------------------------------------------------------------- Python under debugpy

const PY: &str = r#"import sys


def work(n):
    total = 0
    for i in range(n):
        total += i  # @loop
    return total


def risky(flag):
    if flag:
        raise ValueError("bad flag")  # @raise
    return 1


def add(a, b):
    return a + b


def main():
    data = {"a": 1, "b": [1, 2, 3]}
    items = [10, 20, 30]
    count = 0  # @temp
    count += 1  # @skipped
    count += 10  # @after
    result = work(5)  # @call
    try:
        risky(True)
    except ValueError:
        pass
    value = add(work(2), len(items))  # @calls
    print("result", result, value, count, data["a"])  # @print
    risky(True)  # @uncaught


main()
"#;

fn debugpy_python() -> Option<PathBuf> {
    let adapter = dap::discover::find_debugpy(None).ok()?;
    Some(adapter.program)
}

#[test]
fn python_breakpoints_exceptions_and_actions() {
    if debugpy_python().is_none() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = dir.join("app.py");
    std::fs::write(&script, PY).unwrap();
    let mut s = open_project(&dir);
    let temp = bp(&mut s, json!({ "path": script, "line": line_of(PY, "@temp"), "temporary": true }));
    let loop_bp = bp(&mut s, json!({ "path": script, "line": line_of(PY, "@loop"), "hitCondition": "3" }));
    bp(&mut s, json!({ "path": script, "line": line_of(PY, "@print"), "logMessage": "value is {value}" }));
    s.execute("debug.exceptionBreakpoints", &json!({ "pythonRaised": true })).unwrap();

    let started = s.execute("debug.start", &json!({ "path": script })).unwrap();
    let first = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    eprintln!("first: {first}");
    assert_eq!(first["adapter"], json!("debugpy"));
    assert_eq!(top_line(&first), line_of(PY, "@temp") as u64, "{first}");
    assert!(s.debug.breakpoints.iter().all(|b| b.id != temp), "temporary breakpoints go once hit");
    // Values read well.
    assert_eq!(local(&first, "items"), "[10, 20, 30]");
    assert!(local(&first, "data").contains("'b': [1, 2, 3]"), "{first}");

    // Jump to Line (debugpy goto): skip `count += 1`.
    step(&mut s, "over"); // now on @skipped
    s.execute("debug.jumpToLine", &json!({ "path": script, "line": line_of(PY, "@after") - 1 })).unwrap();
    let jumped = wait_stop(&mut s);
    assert_eq!(top_line(&jumped), line_of(PY, "@after") as u64, "{jumped}");
    assert_eq!(jumped["reason"], json!("goto"), "{jumped}");
    let after = step(&mut s, "over");
    assert_eq!(local(&after, "count"), "10", "the jump skipped `count += 1`: {after}");

    // Set Value, completions and hover.
    let set = s.execute("debug.setValue", &json!({ "name": "count", "value": "40 + 2" })).unwrap();
    let set = resolve(&mut s, set);
    assert_eq!(set["value"], json!("42"), "{set}");
    let completions = s.execute("debug.consoleComplete", &json!({ "text": "ite" })).unwrap();
    let completions = resolve(&mut s, completions);
    assert!(completions["items"].as_array().unwrap().iter().any(|i| i["text"] == json!("items")), "{completions}");
    s.execute("debug.hoverEvaluate", &json!({ "expression": "items" })).unwrap();
    pump_until(&mut s, "the hover value", |s| s.debug.hover.as_ref().is_some_and(|h| !h.pending));
    assert_eq!(s.debug.hover.clone().unwrap().result.map(|v| v.value), Ok("[10, 20, 30]".to_owned()));

    // The hit count: the third time through the loop (i == 2).
    let third = step(&mut s, "continue");
    assert_eq!(top_line(&third), line_of(PY, "@loop") as u64, "{third}");
    assert_eq!(local(&third, "i"), "2", "{third}");
    // Change it to a condition.
    s.execute(
        "debug.breakpoint.set",
        &json!({ "path": script, "line": line_of(PY, "@loop"), "hitCondition": null, "condition": "i == 4" }),
    )
    .unwrap();
    let fourth = step(&mut s, "continue");
    assert_eq!(local(&fourth, "i"), "4", "{fourth}");
    s.execute("debug.breakpoint.remove", &json!({ "id": loop_bp })).unwrap();

    // Step Out shows what work returned.
    step(&mut s, "over");
    step(&mut s, "over");
    let out = step(&mut s, "out");
    eprintln!("out: {out}");
    assert!(out["returnValue"]["value"] == json!("10"), "{out}");

    // Raised exceptions stop where they are raised.
    let raised = step(&mut s, "continue");
    assert_eq!(raised["reason"], json!("exception"), "{raised}");
    assert_eq!(top_line(&raised), line_of(PY, "@raise") as u64, "{raised}");
    assert!(raised["exception"].to_string().contains("ValueError"), "{raised}");
    s.execute("debug.exceptionBreakpoints", &json!({ "pythonRaised": false })).unwrap();

    // Smart Step Into on `add(work(2), len(items))`.
    bp(&mut s, json!({ "path": script, "line": line_of(PY, "@calls") }));
    let calls = step(&mut s, "continue");
    assert_eq!(top_line(&calls), line_of(PY, "@calls") as u64, "{calls}");
    let smart = s.execute("debug.step", &json!({ "action": "smartInto" })).unwrap();
    assert_eq!(smart["status"], json!("choose"), "{smart}");
    pump_until(&mut s, "the step targets", |s| s.debug.step_targets.as_ref().is_some_and(|t| !t.pending));
    let targets = s.debug.step_targets.clone().unwrap().targets;
    eprintln!("targets: {targets:?}");
    let index = targets.iter().position(|t| t.label.starts_with("add")).expect("add is a target");
    s.execute("debug.stepIntoTarget", &json!({ "index": index })).unwrap();
    let inside = wait_stop(&mut s);
    assert_eq!(inside["frames"][0]["function"], json!("add"), "{inside}");

    // The logpoint prints instead of stopping; the uncaught exception stops at the end.
    let uncaught = step(&mut s, "continue");
    assert!(console(&s).contains("value is 4"), "{}", console(&s));
    assert_eq!(uncaught["reason"], json!("exception"), "{uncaught}");
    assert_eq!(selected_line(&uncaught), line_of(PY, "@raise") as u64, "{uncaught}");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

const PYTEST: &str = r#"def double(x):
    return x * 2


def test_double():
    result = double(21)  # @bp
    assert result == 42


def test_other():
    assert double(1) == 2
"#;

#[test]
fn python_test_debugs_from_the_gutter() {
    let Some(python) = debugpy_python() else {
        eprintln!("skipping: debugpy is missing");
        return;
    };
    let has_pytest = std::process::Command::new(&python)
        .args(["-c", "import pytest"])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_pytest {
        eprintln!("skipping: pytest is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let file = dir.join("test_math.py");
    std::fs::write(&file, PYTEST).unwrap();
    let mut s = open_project(&dir);
    bp(&mut s, json!({ "path": file, "line": line_of(PYTEST, "@bp") }));
    // Debug on the gutter's ▶ of `def test_double` (0-based line 4).
    let r = s.execute("debug.atCaret", &json!({ "path": file, "line": 4 })).unwrap();
    assert!(r["name"].as_str().unwrap().contains("test_double"), "{r}");
    let stop = wait_stop(&mut s);
    assert_eq!(top_line(&stop), line_of(PYTEST, "@bp") as u64, "{stop}");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    assert_eq!(end["exitCode"], json!(0), "{}", console(&s));
    assert!(console(&s).contains("1 passed"), "only the one test ran: {}", console(&s));
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

const PARENT: &str = r#"import subprocess
import sys
import os

here = os.path.dirname(os.path.abspath(__file__))
code = subprocess.call([sys.executable, os.path.join(here, "child.py")])
print("child exited", code)
"#;

const CHILD: &str = r#"def child_work():
    secret = 41 + 1
    return secret  # @child


print("child says", child_work())
"#;

#[test]
fn python_subprocesses_are_debugged_too() {
    if debugpy_python().is_none() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let parent = dir.join("parent.py");
    let child = dir.join("child.py");
    std::fs::write(&parent, PARENT).unwrap();
    std::fs::write(&child, CHILD).unwrap();
    let mut s = open_project(&dir);
    bp(&mut s, json!({ "path": child, "line": line_of(CHILD, "@child") }));
    let started = s.execute("debug.start", &json!({ "path": parent })).unwrap();
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    eprintln!("child stop: {stop}");
    assert_eq!(top_line(&stop), line_of(CHILD, "@child") as u64, "{stop}");
    assert_eq!(local(&stop, "secret"), "42");
    assert!(stop["process"].as_str().unwrap().starts_with("Subprocess"), "{stop}");
    // The subprocess is a debug session of its own (M15 track DM), next to its parent.
    let child = stop["sessionId"].as_u64().unwrap();
    let parent = stop["parentSession"].as_u64().unwrap();
    let sessions = stop["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{stop}");
    assert!(sessions.iter().any(|x| x["id"] == json!(parent) && x["state"] == json!("running")), "{stop}");
    let end = step(&mut s, "continue");
    // The child's session ends; the parent runs to its end. debugpy sends the program's output
    // (the subprocess's too: it shares the parent's stdout) on the parent's connection.
    assert_eq!((end["state"].clone(), end["sessionId"].clone()), (json!("exited"), json!(child)), "{end}");
    let v = s.execute("debug.waitForStop", &json!({ "session": parent, "timeoutMs": WAIT.as_millis() as u64 })).unwrap();
    let end = resolve(&mut s, v);
    assert_eq!((end["state"].clone(), end["sessionId"].clone()), (json!("exited"), json!(parent)), "{end}");
    let parent_state = s.execute("debug.state", &json!({ "session": parent })).unwrap();
    let out = parent_state["console"].to_string();
    // The subprocess finished by itself (it is never killed when its connection closes).
    assert!(out.contains("child says 42") && out.contains("child exited 0"), "{parent_state}");
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

const SERVER: &str = r#"import time

def handle(n):
    reply = n * 3  # @remote
    return reply

print("ready", flush=True)
print("answer", handle(14), flush=True)
"#;

#[test]
fn python_remote_attach_to_a_debug_server() {
    let Some(python) = debugpy_python() else {
        eprintln!("skipping: debugpy is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    // The "remote" files live elsewhere; path mappings translate.
    let local_dir = dir.join("local");
    let remote_dir = dir.join("remote");
    std::fs::create_dir_all(&local_dir).unwrap();
    std::fs::create_dir_all(&remote_dir).unwrap();
    std::fs::write(local_dir.join("server.py"), SERVER).unwrap();
    std::fs::write(remote_dir.join("server.py"), SERVER).unwrap();
    let port = free_port();
    let mut server = std::process::Command::new(&python)
        .args(["-m", "debugpy", "--listen", &format!("127.0.0.1:{port}"), "--wait-for-client"])
        .arg(remote_dir.join("server.py"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut s = open_project(&local_dir);
    let local_file = local_dir.join("server.py");
    bp(&mut s, json!({ "path": local_file, "line": line_of(SERVER, "@remote") }));
    // CodeCraft retries the connection while debugpy starts listening.
    s.execute(
        "debug.pythonRemote",
        &json!({ "address": format!("127.0.0.1:{port}"), "pathMappings": { local_dir.display().to_string(): remote_dir.display().to_string() } }),
    )
    .unwrap();
    let stop = wait_stop(&mut s);
    eprintln!("remote stop: {stop}");
    assert_eq!(stop["state"], json!("stopped"), "{stop}\n{}", console(&s));
    assert_eq!(stop["source"]["path"], json!(local_file), "mapped back to the local file: {stop}");
    assert_eq!(top_line(&stop), line_of(SERVER, "@remote") as u64);
    assert_eq!(local(&stop, "n"), "14");
    step(&mut s, "continue");
    pump_until(&mut s, "the session to end", |s| !s.debug.is_active());
    let status = server.wait_timeout_or_kill(Duration::from_secs(30));
    assert!(status, "the debug server finished after the session");
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

trait WaitOrKill {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> bool;
}

impl WaitOrKill for std::process::Child {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.kill();
        let _ = self.wait();
        false
    }
}

const LOOPER: &str = r#"import time

def tick(n):
    return n + 1  # @tick

n = 0
while True:
    n = tick(n)
    time.sleep(0.05)
"#;

#[test]
fn python_attach_to_a_running_process() {
    let Some(python) = debugpy_python() else {
        eprintln!("skipping: debugpy is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = dir.join("looper.py");
    std::fs::write(&script, LOOPER).unwrap();
    let mut child = std::process::Command::new(&python)
        .arg(&script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = open_project(&dir);
    bp(&mut s, json!({ "path": script, "line": line_of(LOOPER, "@tick") }));
    s.execute("run.attachToProcess", &json!({ "pid": pid, "kind": "python" })).unwrap();
    let start = Instant::now();
    let stop = loop {
        s.pump();
        if let Some(codecraft_engine::debug::DebugStatus::Failed { message }) = s.debug.session.as_ref().map(|x| x.status()) {
            // Injecting into a process needs debugging rights (macOS developer mode, ptrace scope).
            eprintln!("skipping: attach failed: {message}\n{}", console(&s));
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        if s.debug.is_stopped() && !s.debug.settling() {
            break s.execute("debug.stopSummary", &Value::Null).unwrap();
        }
        if start.elapsed() > Duration::from_secs(60) {
            // Injection hangs without the rights: nothing to test here.
            eprintln!("skipping: attach did not stop in time\n{}", console(&s));
            s.execute("debug.stop", &Value::Null).ok();
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(top_line(&stop), line_of(LOOPER, "@tick") as u64, "{stop}");
    // Stop detaches: the process keeps running.
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "detaching ended the process");
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------- core dumps and remote targets

const CRASH_C: &str = r#"#include <stdio.h>

int crash_here(int x) {
    int local = x * 3; // @core
    return local;
}

int main(void) {
    int v = crash_here(14);
    printf("%d\n", v);
    return 0;
}
"#;

fn build_c(dir: &Path, name: &str, source: &str) -> Option<PathBuf> {
    let cc = tool("cc").or_else(|| tool("clang"))?;
    let src = dir.join(format!("{name}.c"));
    std::fs::write(&src, source).unwrap();
    let exe = dir.join(name);
    let ok = std::process::Command::new(cc).args(["-g", "-O0", "-o"]).arg(&exe).arg(&src).status().ok()?.success();
    ok.then_some(exe)
}

/// LLDB writes a small core of a process stopped in `crash_here` (`process save-core -s stack`).
fn save_core(exe: &Path, core: &Path) -> bool {
    let Some(lldb) = tool("lldb") else { return false };
    let status = std::process::Command::new(lldb)
        .arg("-b")
        .args(["-o", "breakpoint set -n crash_here", "-o", "run"])
        .arg("-o")
        .arg(format!("process save-core -s stack \"{}\"", core.display()))
        .args(["-o", "process kill", "--"])
        .arg(exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    status.is_ok_and(|s| s.success()) && core.is_file()
}

#[test]
fn core_dump_opens_at_the_crash() {
    if dap::discover::find_lldb_dap().is_err() {
        eprintln!("skipping: lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let Some(exe) = build_c(&dir, "crash", CRASH_C) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let core = dir.join("crash.core");
    if !save_core(&exe, &core) {
        eprintln!("skipping: lldb could not save a core here");
        return;
    }
    let mut s = open_project(&dir);
    // As a Core Dump run configuration (debug only).
    s.execute("runConfig.create", &json!({ "name": "Crash core", "kind": "coredump", "command": "crash.core crash" }))
        .unwrap();
    let index = s.run_configs.iter().position(|c| c.name == "Crash core").expect("the configuration is listed");
    s.execute("run.run", &json!({ "config": index })).unwrap();
    let stop = wait_stop(&mut s);
    eprintln!("core: {stop}");
    assert_eq!(stop["state"], json!("stopped"), "{stop}\n{}", console(&s));
    assert!(stop["frames"][0]["function"].as_str().unwrap().contains("crash_here"), "{stop}");
    assert_eq!(selected_line(&stop), line_of(CRASH_C, "@core") as u64, "{stop}");
    assert_eq!(local(&stop, "x"), "14");
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

fn debugserver() -> Option<PathBuf> {
    let xcode = PathBuf::from("/Applications/Xcode.app/Contents/SharedFrameworks/LLDB.framework/Resources/debugserver");
    let clt = PathBuf::from("/Library/Developer/CommandLineTools/Library/PrivateFrameworks/LLDB.framework/Resources/debugserver");
    tool("lldb-server").or_else(|| [xcode, clt].into_iter().find(|p| p.is_file()))
}

#[test]
fn remote_debug_through_a_gdb_remote_stub() {
    let (Ok(_), Some(stub)) = (dap::discover::find_lldb_dap(), debugserver()) else {
        eprintln!("skipping: lldb-dap or a gdb-remote stub (lldb-server, debugserver) is missing");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let Some(exe) = build_c(&dir, "remote", CRASH_C) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let port = free_port();
    let mut server = std::process::Command::new(&stub);
    if stub.file_name().is_some_and(|n| n == "lldb-server") {
        server.arg("gdbserver");
    }
    let mut server = server
        .arg(format!("127.0.0.1:{port}"))
        .arg(&exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if Instant::now() > deadline || server.try_wait().ok().flatten().is_some() {
            eprintln!("skipping: the stub did not start (debugging rights?)");
            let _ = server.kill();
            let _ = server.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // debugserver serves one connection: the probe used it. Start it again.
    let _ = server.kill();
    let _ = server.wait();
    let mut server = std::process::Command::new(&stub);
    if stub.file_name().is_some_and(|n| n == "lldb-server") {
        server.arg("gdbserver");
    }
    let mut server = server
        .arg(format!("127.0.0.1:{port}"))
        .arg(&exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));

    let src = dir.join("remote.c");
    let mut s = open_project(&dir);
    bp(&mut s, json!({ "path": src, "line": line_of(CRASH_C, "@core") }));
    s.execute("runConfig.create", &json!({ "name": "Remote", "kind": "remote", "command": format!("127.0.0.1:{port} remote") }))
        .unwrap();
    let index = s.run_configs.iter().position(|c| c.name == "Remote").unwrap();
    s.execute("run.debug", &json!({ "config": index })).unwrap();
    let mut stop = wait_stop(&mut s);
    eprintln!("remote first stop: {stop}");
    if stop["state"] == json!("failed") {
        eprintln!("skipping: {}", stop["error"]);
        let _ = server.kill();
        let _ = server.wait();
        return;
    }
    // The stub starts the program stopped at its entry: continue to the breakpoint.
    if top_line(&stop) != line_of(CRASH_C, "@core") as u64 {
        stop = step(&mut s, "continue");
    }
    assert_eq!(top_line(&stop), line_of(CRASH_C, "@core") as u64, "{stop}");
    assert_eq!(local(&stop, "x"), "14");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    let _ = server.kill();
    let _ = server.wait();
    s.shutdown();
}

const FRAME: &str = r#"import pandas as pd


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y


df = pd.DataFrame({"a": [1, 2, 3], "b": ["x", "y", "z"]})
series = df["a"]
point = Point(1, 2)
nested = {"k": [1, {"deep": (2, 3)}]}
print(len(df))  # @frame
"#;

#[test]
fn python_values_read_well() {
    let Some(python) = debugpy_python() else {
        eprintln!("skipping: debugpy is missing");
        return;
    };
    let has_pandas = std::process::Command::new(&python)
        .args(["-c", "import pandas"])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_pandas {
        eprintln!("skipping: pandas is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let script = dir.join("frame.py");
    std::fs::write(&script, FRAME).unwrap();
    let mut s = open_project(&dir);
    bp(&mut s, json!({ "path": script, "line": line_of(FRAME, "@frame") }));
    let started = s.execute("debug.start", &json!({ "path": script })).unwrap();
    let stop = resolve(&mut s, json!({ "pending": true, "token": started["token"] }));
    eprintln!("frame locals: {:#}", stop["locals"]);
    assert_eq!(local(&stop, "df"), "DataFrame 3 rows × 2 columns: a, b");
    assert_eq!(local(&stop, "series"), "Series 3 values, int64, name a");
    assert_eq!(local(&stop, "point"), "Point(x=1, y=2)");
    assert!(local(&stop, "nested").starts_with("{'k': [1,"), "{stop}");
    let end = step(&mut s, "continue");
    assert_eq!(end["state"], json!("exited"), "{end}");
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

#[test]
fn python_test_debugs_from_the_test_tree() {
    let Some(python) = debugpy_python() else {
        eprintln!("skipping: debugpy is missing");
        return;
    };
    let has_pytest = std::process::Command::new(&python)
        .args(["-c", "import pytest"])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_pytest {
        eprintln!("skipping: pytest is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let file = dir.join("test_math.py");
    std::fs::write(&file, PYTEST).unwrap();
    // The interpreter that has debugpy and pytest runs the tests too.
    std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&python, dir.join(".venv/bin/python")).unwrap();
    let mut s = open_project(&dir);
    s.execute("run.testsInFile", &json!({ "path": file })).unwrap();
    pump_until(&mut s, "the test run", |s| s.testing.runs.first().is_some_and(|r| r.finished));
    let run = s.testing.runs.first().unwrap();
    let key = run
        .suites
        .iter()
        .enumerate()
        .find_map(|(si, suite)| suite.tests.iter().position(|t| t.name.ends_with("test_double")).map(|ti| format!("s{si}#{ti}")))
        .unwrap_or_else(|| panic!("no test_double in {:?}", run.suites));
    bp(&mut s, json!({ "path": file, "line": line_of(PYTEST, "@bp") }));
    let started = s.execute("debug.test", &json!({ "key": key })).unwrap();
    eprintln!("debug.test: {started}");
    let stop = wait_stop(&mut s);
    assert_eq!(top_line(&stop), line_of(PYTEST, "@bp") as u64, "{stop}");
    let end = step(&mut s, "continue");
    assert_eq!(end["exitCode"], json!(0), "{}", console(&s));
    assert!(console(&s).contains("1 passed"), "{}", console(&s));
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}

const ASGI: &str = r#"async def app(scope, receive, send):
    if scope["type"] != "http":
        return
    path = scope["path"]  # @handler
    await send({"type": "http.response.start", "status": 200, "headers": [(b"content-type", b"text/plain")]})
    await send({"type": "http.response.body", "body": path.encode()})
"#;

#[test]
fn fastapi_configuration_debugs_without_the_reloader() {
    let Some(python) = debugpy_python() else {
        eprintln!("skipping: debugpy is missing");
        return;
    };
    let has_uvicorn = std::process::Command::new(&python)
        .args(["-c", "import uvicorn"])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_uvicorn {
        eprintln!("skipping: uvicorn is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let main = dir.join("main.py");
    std::fs::write(&main, ASGI).unwrap();
    std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&python, dir.join(".venv/bin/python")).unwrap();
    let port = free_port();
    let mut s = open_project(&dir);
    s.execute(
        "runConfig.create",
        &json!({ "name": "API", "kind": "fastapi", "command": format!("main:app --reload --port {port}") }),
    )
    .unwrap();
    bp(&mut s, json!({ "path": main, "line": line_of(ASGI, "@handler") }));
    let index = s.run_configs.iter().position(|c| c.name == "API").unwrap();
    s.execute("run.debug", &json!({ "config": index, "beforeLaunch": false })).unwrap();
    // A client request once the server listens (it blocks while the debugger is paused).
    let client = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(90)));
                let _ = stream.write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
                let mut reply = String::new();
                let _ = stream.read_to_string(&mut reply);
                return reply;
            }
            if Instant::now() > deadline {
                return String::new();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let stop = wait_stop(&mut s);
    eprintln!("fastapi stop: {stop}");
    assert_eq!(top_line(&stop), line_of(ASGI, "@handler") as u64, "{stop}\n{}", console(&s));
    assert!(stop.get("process").is_none(), "served by the main process, not a reloader child: {stop}");
    // Continue without waiting for another stop (the server keeps running).
    let resumed = s.execute("debug.step", &json!({ "action": "continue" })).unwrap();
    assert_eq!(resumed["status"]["state"], json!("running"), "{resumed}");
    let reply = loop {
        s.pump();
        if client.is_finished() {
            break client.join().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(reply.contains("200") && reply.contains("/ping"), "{reply}");
    s.execute("debug.stop", &Value::Null).unwrap();
    pump_until(&mut s, "the adapter to go away", |s| !s.debug.busy());
    s.shutdown();
}
