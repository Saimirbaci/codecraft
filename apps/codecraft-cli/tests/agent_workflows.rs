//! M14 QA: realistic agent workflows over `codecraft-cli mcp <project> --allow …` (the built
//! binary over stdio) on small sample projects, with the real tools of this machine: "why does
//! this test fail" (test_run → test_results → breakpoint → debug_start → debug_wait_for_stop →
//! debug_evaluate), "what is slow" (profile_start → profile_hotspots), "fix the lint problems"
//! (inspect → codecraft_execute analysis.fix) and "what's in this table" (db_query). Each test
//! skips cleanly when its tool (python3 + debugpy/pytest, cargo, ruff, lldb-dap, xctrace) is
//! missing. Set `CODECRAFT_QA_DUMP=1` to print every call and result.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const CLI: &str = env!("CARGO_BIN_EXE_codecraft-cli");

/// A `codecraft-cli mcp` server over stdio.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: crossbeam_channel::Receiver<Value>,
    next: u64,
}

fn dump() -> bool {
    std::env::var_os("CODECRAFT_QA_DUMP").is_some()
}

impl Mcp {
    fn start(tmp: &Path, project: &Path, allow: &str, lsp: bool) -> Self {
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut cmd = Command::new(CLI);
        // The sample projects build into their own target directories, not the test's.
        cmd.env("CODECRAFT_HOME", home.join(".codecraft")).env("RUST_LOG", "error").env_remove("CARGO_TARGET_DIR");
        let mut args = vec!["mcp".to_string(), project.display().to_string(), "--allow".into(), allow.into()];
        if !lsp {
            args.push("--no-lsp".into());
        }
        let mut child = cmd
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if dump() { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let mut m = Self { child, stdin, rx, next: 1 };
        let init = m.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "qa-agent", "version": "1" } }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "CodeCraft", "{init}");
        writeln!(m.stdin, "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).unwrap();
        m
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).unwrap();
        self.stdin.flush().unwrap();
        loop {
            let v = self.rx.recv_timeout(Duration::from_secs(600)).expect("the server answers");
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v;
            }
        }
    }

    /// Calls a tool: `Ok(json)` or `Err(message)` for a tool error.
    fn call(&mut self, name: &str, args: Value) -> Result<Value, String> {
        if dump() {
            eprintln!("\n>>> {name} {args}");
        }
        let v = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let text = v["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text: {v}")).to_string();
        let is_error = v["result"]["isError"] == true;
        if dump() {
            eprintln!("<<< {}{}", if is_error { "ERROR " } else { "" }, text);
        }
        if is_error { Err(text) } else { Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))) }
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        self.call(name, args.clone()).unwrap_or_else(|e| panic!("{name} {args}: {e}"))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn has(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

fn python_with(module: &str) -> bool {
    has("python3", &["-c", &format!("import {module}")])
}

fn write(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root.canonicalize().unwrap()
}

// ---------------------------------------------------------------- sample projects

const PY_CORE: &str = "import os
import sys


def total(prices):
    \"\"\"Sum with 10% off for 3 items or more.\"\"\"
    s = 0
    for p in prices[1:]:
        s += p
    if len(prices) >= 3:
        s = s - s // 10
    return s


def slow_primes(n):
    found = []
    for k in range(2, n):
        if all(k % d for d in range(2, k)):
            found.append(k)
    return found


def parse_qty(text):
    return int(text)
";

const PY_TESTS: &str = "from inventory.core import total


def test_total_of_two():
    assert total([10, 20]) == 30


def test_total_empty():
    assert total([]) == 0
";

const PY_MAIN: &str = "from inventory.core import slow_primes, total

print(\"total\", total([10, 20]))
print(\"primes\", len(slow_primes(4000)))
";

fn python_project(tmp: &Path) -> PathBuf {
    write(
        &tmp.join("py"),
        &[
            ("pyproject.toml", "[project]\nname = \"inventory\"\nversion = \"0.1.0\"\n"),
            ("inventory/__init__.py", ""),
            ("inventory/core.py", PY_CORE),
            ("tests/test_core.py", PY_TESTS),
            ("main.py", PY_MAIN),
        ],
    )
}

const RS_LIB: &str = "use std::collections::HashMap;

/// Sum of the prices with 10% off for 3 items or more.
pub fn total(prices: &[u32]) -> u32 {
    let mut sum = 0;
    for i in 1..prices.len() {
        sum += prices[i];
    }
    if prices.len() >= 3 {
        sum - sum / 10
    } else {
        sum
    }
}

pub fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

pub fn stock() -> HashMap<String, u32> {
    let mut m = HashMap::new();
    m.insert(\"apple\".to_string(), 3);
    m.insert(\"pear\".to_string(), 5);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_of_two() {
        assert_eq!(total(&[10, 20]), 30);
    }

    #[test]
    fn fib_small() {
        assert_eq!(fib(10), 55);
    }
}
";

const RS_MAIN: &str = "fn main() {
    let prices = vec![10, 20, 30];
    let t = shop::total(&prices);
    println!(\"total {t}\");
    let s = shop::stock();
    println!(\"apples {}\", s[\"apple\"]);
    let f = shop::fib(39);
    println!(\"fib {f}\");
}
";

fn rust_project(tmp: &Path) -> PathBuf {
    write(
        &tmp.join("rs"),
        &[
            ("Cargo.toml", "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/lib.rs", RS_LIB),
            ("src/main.rs", RS_MAIN),
        ],
    )
}

/// A project with a SQLite database (made by python3's sqlite3 module) and a report script.
fn sqlite_project(tmp: &Path) -> Option<PathBuf> {
    let root = write(
        &tmp.join("sql"),
        &[(
            "report.sql",
            "select i.name, sum(o.qty) as qty from orders o join items i on i.id = o.item_id group by i.name;\n",
        )],
    );
    let script = "import sqlite3, sys\nc = sqlite3.connect(sys.argv[1])\nc.executescript('''\
create table items(id integer primary key, name text not null, price integer);\
create table orders(id integer primary key, item_id integer references items(id), qty integer);\
insert into items(name, price) values ('apple', 3), ('pear', 5), ('plum', 7);\
insert into orders(item_id, qty) values (1, 2), (2, 1), (1, 5);''')\nc.commit()\n";
    std::fs::create_dir_all(root.join("data")).ok()?;
    let ok = Command::new("python3")
        .args(["-c", script, root.join("data/shop.db").to_str()?])
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(root)
}

const CPP_MAIN: &str = "#include <cstdio>
#include <map>
#include <string>
#include <vector>

struct Item {
    std::string name;
    int count;
};

long slow_sum(int n) {
    long total = 0;
    for (int i = 0; i < n; ++i)
        for (int j = 0; j < 2000; ++j)
            total += (i ^ j) % 7;
    return total;
}

int lookup(const std::map<std::string, int>& m, const std::string& key) {
    const int* p = nullptr;
    auto it = m.find(key);
    if (it != m.end()) p = &it->second;
    return *p;
}

int main(int argc, char** argv) {
    std::vector<Item> items{{\"apple\", 3}, {\"pear\", 5}};
    std::map<std::string, int> stock;
    for (const auto& it : items) stock[it.name] = it.count;
    std::printf(\"items %zu\\n\", items.size());
    std::printf(\"sum %ld\\n\", slow_sum(argc > 1 ? 20000 : 300));
    std::printf(\"plum %d\\n\", lookup(stock, \"plum\"));
    return 0;
}
";

fn cpp_project(tmp: &Path) -> PathBuf {
    write(
        &tmp.join("cpp"),
        &[
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.16)\nproject(inventory CXX)\nset(CMAKE_CXX_STANDARD 17)\nadd_executable(inventory main.cpp)\n",
            ),
            ("main.cpp", CPP_MAIN),
        ],
    )
}

// ---------------------------------------------------------------- workflows

fn lldb_dap() -> bool {
    has("xcrun", &["-f", "lldb-dap"]) || has("lldb-dap", &["--help"])
}

fn xctrace() -> bool {
    cfg!(target_os = "macos") && has("xcrun", &["-f", "xctrace"])
}

fn ends_with(v: &Value, suffix: &str) -> bool {
    v.as_str().is_some_and(|s| s.ends_with(suffix))
}

/// "Why does test_total_of_two fail?" in a pytest project: run the tests, read the failure, stop
/// in the function under test and look at its values.
#[test]
fn python_why_does_the_test_fail() {
    if !python_with("debugpy") || !python_with("pytest") {
        eprintln!("skipping: python3 with debugpy and pytest is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = python_project(tmp.path());
    let mut m = Mcp::start(tmp.path(), &root, "run,debug", false);
    // Right after start the project is still loading: the configurations are there anyway.
    let configs = m.ok("run_configurations", json!({}));
    assert!(configs.as_array().is_some_and(|a| a.iter().any(|c| c["name"] == "pytest")), "{configs}");

    let r = m.ok("test_run", json!({}));
    assert_eq!(r["counts"]["failed"], 1, "{r}");
    assert_eq!(r["counts"]["passed"], 1, "{r}");
    let f = &r["failures"][0];
    assert_eq!(f["name"], "test_total_of_two");
    assert!(ends_with(&f["file"], "tests/test_core.py") && f["line"] == 5, "{f}");
    assert!(f["message"].as_str().unwrap().contains("assert 20 == 30"), "{f}");
    assert_eq!(m.ok("test_results", json!({}))["failures"][0]["name"], "test_total_of_two");

    let bp = m.ok("debug_breakpoint_set", json!({ "path": "inventory/core.py", "line": 9 }));
    assert_eq!(bp["line"], 9);
    let start = m.ok("debug_start", json!({ "file": "tests/test_core.py", "line": 4 }));
    assert_eq!(start["started"], true, "{start}");
    let stop = m.ok("debug_wait_for_stop", json!({ "timeoutMs": 60000 }));
    assert_eq!(stop["state"], "stopped", "{stop}");
    assert_eq!(stop["reason"], "breakpoint");
    let top = &stop["frames"][0];
    assert!(ends_with(&top["file"], "inventory/core.py") && top["line"] == 9 && top["name"] == "total", "{top}");
    assert!(top["excerpt"].as_str().unwrap().contains("s += p"), "{top}");
    let locals = stop["locals"].as_array().unwrap();
    let local = |n: &str| locals.iter().find(|l| l["name"] == n).map(|l| l["value"].clone());
    assert_eq!(local("prices"), Some(json!("[10, 20]")));
    assert_eq!(local("p"), Some(json!("20")), "the first price was skipped: {stop}");
    let e = m.ok("debug_evaluate", json!({ "expression": "prices[0]" }));
    assert_eq!(e["value"], "10");
    let bad = m.call("debug_evaluate", json!({ "expression": "nope_x" }));
    assert!(bad.is_err(), "{bad:?}");
    let end = m.ok("debug_control", json!({ "action": "stop" }));
    assert_eq!(end["state"], "exited", "{end}");
}

/// "What is slow, and what allocates?" in a Python script: cProfile and tracemalloc through
/// profile_start with a command line, the project's own functions first.
#[test]
fn python_what_is_slow() {
    if !has("python3", &["--version"]) {
        eprintln!("skipping: python3 is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = python_project(tmp.path());
    let mut m = Mcp::start(tmp.path(), &root, "profile", false);
    let unknown = m.call("profile_start", json!({ "config": "main.py" })).unwrap_err();
    assert!(unknown.contains("Run main.py"), "names the configurations there are: {unknown}");

    let started = m.ok("profile_start", json!({ "command": "python3 main.py" }));
    assert_eq!(started["tool"], "cProfile", "{started}");
    let hot = m.ok("profile_hotspots", json!({ "limit": 20 }));
    // The project's own functions (by self time they can trail builtins on a loaded machine).
    let project: Vec<&Value> = hot["hotspots"]
        .as_array()
        .into_iter()
        .chain(hot["projectHotspots"].as_array())
        .flatten()
        .filter(|h| h["inProject"] == true)
        .collect();
    let names: Vec<&str> = project.iter().filter_map(|h| h["name"].as_str()).collect();
    assert!(names.contains(&"<genexpr>") && names.contains(&"slow_primes"), "{hot}");
    let slow = *project.iter().find(|h| h["name"] == "slow_primes").unwrap();
    assert!(ends_with(&slow["file"], "inventory/core.py") && slow["line"] == 15 && slow["inProject"] == true, "{slow}");
    // It dominates; the exact share drops on a loaded machine (startup and imports weigh more).
    assert!(slow["totalPercent"].as_f64().unwrap() > 50.0, "{slow}");

    // Memory: the import machinery allocates most; the project's functions come apart.
    let started = m.ok("profile_start", json!({ "command": "python3 main.py", "kind": "memory" }));
    assert_eq!(started["tool"], "tracemalloc", "{started}");
    let mem = m.ok("profile_hotspots", json!({ "limit": 5 }));
    assert_eq!(mem["unit"], "bytes", "{mem}");
    assert!(mem["note"].as_str().unwrap().contains("bytes"), "{mem}");
    let own = mem.get("projectHotspots").or(Some(&mem["hotspots"])).and_then(Value::as_array).unwrap();
    assert!(own.iter().any(|h| h["inProject"] == true), "{mem}");

    // A program that fails says so, with where to read its output.
    m.ok("profile_start", json!({ "command": "python3 nothere.py" }));
    let failed = m.call("profile_hotspots", json!({})).unwrap_err();
    assert!(failed.contains("failed") && failed.contains("run_output"), "{failed}");
}

/// "Fix the lint problems" in a Python package: inspect, read the problems, fix one, suppress
/// one, export SARIF.
#[test]
fn python_fix_the_lint_problems() {
    if !has("ruff", &["--version"]) {
        eprintln!("skipping: ruff is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = python_project(tmp.path());
    let mut m = Mcp::start(tmp.path(), &root, "run,write", false);
    let found = m.ok("inspect", json!({ "scope": "project", "tools": ["ruff"] }));
    assert_eq!(found["total"], 2, "{found}");
    let f0 = &found["findings"][0];
    assert!(ends_with(&f0["file"], "inventory/core.py") && f0["line"] == 1 && f0["code"] == "F401", "{f0}");
    // `problems` knows the analysis too, with the ids the fixes take.
    let problems = m.ok("problems", json!({}));
    assert_eq!(problems["total"], 2, "{problems}");
    assert_eq!(problems["problems"][0]["source"], "analysis");
    assert!(problems["problems"][0]["id"].is_u64() && problems["problems"][0]["fixable"] == true, "{problems}");

    let fixed = m.ok("codecraft_execute", json!({ "id": "analysis.fix", "params": { "id": 0 } }));
    assert_eq!(fixed["result"]["applied"], 1, "{fixed}");
    let text = std::fs::read_to_string(root.join("inventory/core.py")).unwrap();
    assert!(!text.contains("import os"), "{text}");
    let suppressed = m.ok("codecraft_execute", json!({ "id": "analysis.suppress", "params": { "id": 1 } }));
    assert!(suppressed["result"].is_object(), "{suppressed}");
    let text = std::fs::read_to_string(root.join("inventory/core.py")).unwrap();
    assert!(text.contains("import sys  # noqa: F401"), "{text}");
    let again = m.ok("inspect", json!({ "scope": "project", "tools": ["ruff"] }));
    assert_eq!(again["total"], 0, "{again}");
    let sarif = root.join("out.sarif");
    m.ok("codecraft_execute", json!({ "id": "analysis.export", "params": { "format": "sarif", "path": sarif } }));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&sarif).unwrap()).unwrap();
    assert_eq!(v["version"], "2.1.0");
}

/// "What's in table items?" on a project with a SQLite file and no data source yet.
#[test]
fn sqlite_whats_in_the_table() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(root) = sqlite_project(tmp.path()) else {
        eprintln!("skipping: python3 with sqlite3 is missing");
        return;
    };
    let mut m = Mcp::start(tmp.path(), &root, "write", false);
    let sources = m.ok("db_sources", json!({}));
    let file = &sources[0];
    assert!(file["status"] == "not added" && ends_with(&file["path"], "data/shop.db"), "{sources}");
    let wrong = m.call("db_query", json!({ "dataSource": "Shop", "sql": "select 1" })).unwrap_err();
    assert!(wrong.contains("data/shop.db"), "points at the SQLite file: {wrong}");

    let r =
        m.ok("db_query", json!({ "dataSource": "data/shop.db", "sql": "select name, price from items order by id" }));
    assert_eq!(r["results"][0]["rows"], json!([["apple", 3], ["pear", 5], ["plum", 7]]), "{r}");
    assert!(r["added"]["id"].is_string(), "{r}");
    let id = r["added"]["id"].as_str().unwrap().to_string();
    // The script's query, through the new data source's id.
    let report = std::fs::read_to_string(root.join("report.sql")).unwrap();
    let r = m.ok("db_query", json!({ "dataSource": id, "sql": report }));
    assert_eq!(r["results"][0]["totalRows"], 2, "{r}");
    let missing = m.call("db_query", json!({ "dataSource": id, "sql": "select * from nope" })).unwrap_err();
    assert!(missing.contains("no such table"), "{missing}");
    let denied = m.call("db_query", json!({ "dataSource": id, "sql": "delete from orders" })).unwrap_err();
    assert!(denied.contains("--allow db-write"), "{denied}");
    let r = m.ok("db_query", json!({ "dataSource": id, "sql": "select count(*) from orders" }));
    assert_eq!(r["results"][0]["rows"], json!([[3]]), "nothing was deleted: {r}");
}

/// "Why does tests::total_of_two fail?" and "what is slow?" in a Cargo package with a library
/// and a binary.
#[test]
fn rust_why_does_the_test_fail_and_what_is_slow() {
    if !has("cargo", &["--version"]) || !lldb_dap() {
        eprintln!("skipping: cargo or lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = rust_project(tmp.path());
    let mut m = Mcp::start(tmp.path(), &root, "run,debug,profile", false);
    let r = m.ok("test_run", json!({}));
    assert_eq!(r["counts"]["failed"], 1, "{r}");
    let f = &r["failures"][0];
    assert!(f["name"] == "tests::total_of_two" && ends_with(&f["file"], "src/lib.rs") && f["line"] == 33, "{f}");

    m.ok("debug_breakpoint_set", json!({ "path": "src/lib.rs", "line": 7 }));
    // The test in lib.rs runs from the library target, not the binary of the same name.
    m.ok("debug_start", json!({ "file": "src/lib.rs", "line": 32 }));
    let stop = m.ok("debug_wait_for_stop", json!({ "timeoutMs": 180000 }));
    assert_eq!(stop["state"], "stopped", "{stop}");
    let top = &stop["frames"][0];
    assert!(top["name"] == "shop::total" && ends_with(&top["file"], "src/lib.rs") && top["line"] == 7, "{top}");
    assert_eq!(m.ok("debug_evaluate", json!({ "expression": "prices[0]" }))["value"], "10");
    let next = m.ok("debug_control", json!({ "action": "stepOver" }));
    assert_eq!(next["state"], "stopped", "{next}");
    m.ok("debug_control", json!({ "action": "stop" }));

    if !xctrace() {
        eprintln!("skipping the profile: xctrace is missing");
        return;
    }
    m.ok("profile_start", json!({ "config": "Run shop" }));
    // Right away: the target is still building; the call waits for the snapshot.
    let hot = m.ok("profile_hotspots", json!({ "limit": 5 }));
    assert_eq!(hot["hotspots"][0]["name"], "shop::fib", "demangled: {hot}");
}

/// A CLion user's CMake project: the executable is the default configuration, a crash stops in
/// the debugger at the line, and Inspect Code finds it without analysing CMake's own files.
#[test]
fn cpp_crash_and_analysis() {
    if !has("cmake", &["--version"]) || !lldb_dap() || !has("clang", &["--version"]) {
        eprintln!("skipping: cmake, clang or lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = cpp_project(tmp.path());
    let mut m = Mcp::start(tmp.path(), &root, "run,debug", false);
    let configs = m.ok("run_configurations", json!({}));
    let list = configs.as_array().unwrap();
    assert_eq!(list.iter().filter(|c| c["name"] == "Run inventory").count(), 1, "listed once: {configs}");
    let selected = list.iter().find(|c| c["selected"] == true).unwrap();
    assert_eq!(selected["name"], "Run inventory", "{configs}");

    m.ok("debug_start", json!({}));
    let stop = m.ok("debug_wait_for_stop", json!({ "timeoutMs": 180000 }));
    assert_eq!(stop["state"], "stopped", "{stop}");
    assert_ne!(stop["reason"], "breakpoint", "{stop}");
    let top = &stop["frames"][0];
    assert!(top["name"].as_str().unwrap().contains("lookup") && top["line"] == 23, "{top}");
    let stack = m.ok("debug_stack", json!({}));
    assert!(stack["frames"].as_array().unwrap().iter().any(|f| f["name"] == "main"), "{stack}");
    m.ok("debug_control", json!({ "action": "stop" }));

    let found = m.ok("inspect", json!({ "scope": "project" }));
    let findings = found["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["code"] == "core.NullDereference" && f["line"] == 23),
        "the static analyzer sees the crash: {found}"
    );
    assert!(!found.to_string().contains("CompilerId"), "CMake's own sources are not analysed: {found}");
}
