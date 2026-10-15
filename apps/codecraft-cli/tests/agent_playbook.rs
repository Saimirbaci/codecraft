//! M15 QA (track QB), as an agent: the playbook of `docs/agents.md` run call by call, exactly as
//! written, over the built `codecraft-cli mcp <project> --allow …` (stdio), on sample projects
//! shaped like the docs' examples, checking that every documented call is accepted and every
//! documented result field is there; plus two debug sessions driven with `session`, cancelling a
//! waiting call, and `--print-config` against the docs. Real tools (cargo, lldb-dap, python3,
//! debugpy, ruff, clippy, git, a local PostgreSQL with a throwaway schema), clean skips without
//! them. Set `CODECRAFT_QA_DUMP=1` to print every call and answer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CLI: &str = env!("CARGO_BIN_EXE_codecraft-cli");

fn dump() -> bool {
    std::env::var_os("CODECRAFT_QA_DUMP").is_some()
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: crossbeam_channel::Receiver<Value>,
    next: u64,
}

impl Mcp {
    fn start(tmp: &Path, project: &Path, allow: &str, env: &[(String, String)]) -> Self {
        let home = tmp.join("cchome");
        std::fs::create_dir_all(&home).unwrap();
        let mut cmd = Command::new(CLI);
        cmd.env("CODECRAFT_HOME", &home).env("RUST_LOG", "error").env_remove("CARGO_TARGET_DIR");
        cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let mut child = cmd
            .args(["mcp", &project.display().to_string(), "--allow", allow, "--no-lsp"])
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
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "qa-playbook", "version": "1" } }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "CodeCraft", "{init}");
        m.notify("notifications/initialized", json!({}));
        m
    }

    fn notify(&mut self, method: &str, params: Value) {
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "method": method, "params": params })).unwrap();
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).unwrap();
        self.stdin.flush().unwrap();
        id
    }

    fn wait(&mut self, id: u64, secs: u64) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(v) = self.rx.recv_timeout(Duration::from_millis(50))
                && v.get("id").and_then(Value::as_u64) == Some(id)
            {
                return Some(v);
            }
        }
        None
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.wait(id, 900).expect("the server answers")
    }

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

fn write(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root.canonicalize().unwrap()
}

/// Every documented key is in `v` (an object).
fn keys(v: &Value, names: &[&str], what: &str) {
    for k in names {
        assert!(v.get(*k).is_some(), "{what}: no `{k}` in {v}");
    }
}

fn lldb_dap() -> bool {
    has("xcrun", &["-f", "lldb-dap"]) || has("lldb-dap", &["--help"])
}

// ---------------------------------------------------------------- Debug a failing test

/// The docs' `shop` crate: `parse` (line 4 is inside it) and `tests::parses_empty` at line 14
/// whose assertion (line 16) fails.
const SHOP_LIB: &str = "pub fn parse(input: &str) -> Vec<&str> {
    // Split a comma-separated list.
    let parts: Vec<&str> = input.split(',').collect();
    parts.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two() { assert_eq!(parse(\"a,b\").len(), 2); }
    #[test]
    fn parses_empty() {
        let v = parse(\"\");
        assert_eq!(v.len(), 0);
    }
}
";

#[test]
fn playbook_debug_a_failing_test() {
    if !has("cargo", &["--version"]) || !lldb_dap() {
        eprintln!("skipping: cargo or lldb-dap is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = write(
        &tmp.path().join("shop"),
        &[
            ("Cargo.toml", "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/lib.rs", SHOP_LIB),
        ],
    );
    let mut m = Mcp::start(tmp.path(), &root, "run,debug,write", &[]);

    // 1. Find the failure.
    let r = m.ok("test_run", json!({ "name": "parses_empty" }));
    keys(&r, &["tab", "title", "commandLine", "finished", "status", "counts", "durationMs", "failures"], "test_run");
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["commandLine"], "cargo test -- parses_empty", "{r}");
    assert_eq!(r["counts"], json!({ "passed": 0, "failed": 1, "ignored": 0, "running": 0 }), "{r}");
    let f = &r["failures"][0];
    keys(f, &["name", "suite", "file", "line", "message", "excerpt"], "a failure");
    assert_eq!((f["name"].as_str(), f["line"].as_u64()), (Some("tests::parses_empty"), Some(16)), "{f}");
    assert_eq!(f["file"], json!(root.join("src/lib.rs")), "{f}");
    assert!(f["excerpt"].as_str().unwrap().contains("> 16 |"), "{f}");

    // 2. Stop in the code under test and start the test under the debugger.
    let bp = m.ok("debug_breakpoint_set", json!({ "path": "src/lib.rs", "line": 4 }));
    keys(&bp, &["id", "file", "line", "enabled"], "debug_breakpoint_set");
    let st = m.ok("debug_start", json!({ "file": "src/lib.rs", "line": 14 }));
    keys(&st, &["state", "started", "breakpoints", "next"], "debug_start");
    assert_eq!(st["started"], true, "{st}");
    let stop = m.ok("debug_wait_for_stop", json!({ "timeoutMs": 120000 }));
    keys(&stop, &["state", "reason", "thread", "frames", "locals"], "debug_wait_for_stop");
    assert_eq!((stop["state"].as_str(), stop["reason"].as_str()), (Some("stopped"), Some("breakpoint")), "{stop}");
    assert_eq!(stop["thread"]["name"], "tests::parses_empty", "{stop}");
    let top = &stop["frames"][0];
    assert_eq!((top["name"].as_str(), top["line"].as_u64()), (Some("shop::parse"), Some(4)), "{top}");
    let locals = stop["locals"].as_array().unwrap();
    let local = |n: &str| locals.iter().find(|l| l["name"] == n).cloned().unwrap_or_else(|| panic!("no {n}: {stop}"));
    assert_eq!(local("input")["value"], "\"\"", "{stop}");
    let parts = local("parts");
    assert_eq!(parts["value"], "size=1", "{stop}");
    let reference = parts["ref"].as_u64().unwrap_or_else(|| panic!("`parts` has a ref: {parts}"));

    // 3. Look closer, step, and finish.
    let vars = m.ok("debug_variables", json!({ "ref": reference }));
    assert_eq!(vars["variables"][0]["name"], "[0]", "{vars}");
    assert_eq!(vars["variables"][0]["value"], "\"\"", "{vars}");
    // LLDB evaluates fields, not Rust methods (docs fixed: the example called `is_empty()`); the
    // error says what works.
    let err = m.call("debug_evaluate", json!({ "expression": "input.is_empty()" })).unwrap_err();
    assert!(err.contains("LLDB can't call Rust methods") && err.contains("s.length == 0"), "{err}");
    assert_eq!(m.ok("debug_evaluate", json!({ "expression": "parts.len" }))["value"], "1");
    let e = m.ok("debug_evaluate", json!({ "expression": "input.length == 0" }));
    keys(&e, &["expression", "value"], "debug_evaluate");
    assert_eq!(e["value"], "true", "{e}");
    let step = m.ok("debug_control", json!({ "action": "stepOver" }));
    assert_eq!((step["state"].as_str(), step["reason"].as_str()), (Some("stopped"), Some("step")), "{step}");
    assert_eq!(step["frames"][0]["line"], 5, "{step}");
    let end = m.ok("debug_control", json!({ "action": "stop" }));
    eprintln!("debug_control stop → {end}");
    assert!(matches!(end["state"].as_str(), Some("none" | "exited")), "{end}");

    // 4. Fix, run the test again.
    let edit = m.ok(
        "file_edit",
        json!({ "path": "src/lib.rs", "oldText": "parts.into_iter().collect()", "newText": "parts.into_iter().filter(|p| !p.is_empty()).collect()" }),
    );
    assert_eq!(edit["replacements"], 1, "{edit}");
    let r = m.ok("test_run", json!({ "name": "parses_empty" }));
    assert_eq!(r["status"], "passed", "{r}");
}

// ---------------------------------------------------------------- Find a hotspot

const MAIN_PY: &str = "import sys


def setup():
    return list(range(10))


def fast():
    return sum(range(1000))


def slow_sum(n):
    total = 0
    for i in range(n):
        total += sum(range(i))
    return total


def main():
    setup()
    fast()
    print(slow_sum(int(sys.argv[1]) if len(sys.argv) > 1 else 3000))


main()
";

#[test]
fn playbook_find_a_hotspot_and_verify_the_fix() {
    if !has("python3", &["--version"]) {
        eprintln!("skipping: python3 is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = write(&tmp.path().join("app"), &[("main.py", MAIN_PY)]);
    let mut m = Mcp::start(tmp.path(), &root, "profile,write", &[]);
    let started = m.ok("profile_start", json!({ "command": "python3 main.py" }));
    keys(&started, &["tab", "job", "tool", "state", "next"], "profile_start");
    assert_eq!(started["tool"], "cProfile", "{started}");
    let hot = m.ok("profile_hotspots", json!({ "limit": 5 }));
    keys(&hot, &["snapshot", "unit", "total", "hotspots", "report", "sort", "order"], "profile_hotspots");
    let first = hot["snapshot"].as_str().unwrap().to_string();
    // `sum` (a builtin it calls) has the self time; `slow_sum` is the project's function with
    // the total time, in `hotspots` or `projectHotspots`.
    let slow = hot["hotspots"]
        .as_array()
        .into_iter()
        .chain(hot["projectHotspots"].as_array())
        .flatten()
        .find(|h| h["name"] == "slow_sum")
        .cloned();
    let slow = slow.unwrap_or_else(|| panic!("slow_sum: {hot}"));
    assert!(slow["totalPercent"].as_f64().unwrap() > 80.0, "{slow}");
    keys(&slow, &["name", "selfPercent", "totalPercent", "file", "line", "inProject"], "a hotspot");
    assert_eq!((slow["line"].as_u64(), slow["inProject"].as_bool()), (Some(12), Some(true)), "{slow}");
    // cProfile times calls, not lines: no hot lines, and a note says so (docs fixed: the
    // example showed a hot line for `python3 main.py`).
    assert_eq!(hot["report"]["hotLines"], json!([]), "{hot}");
    assert!(hot["report"]["notes"].to_string().contains("no per-line samples"), "{hot}");

    let read = m.ok("file_read", json!({ "path": "main.py", "startLine": 10, "endLine": 16 }));
    keys(&read, &["path", "startLine", "endLine", "totalLines", "unsaved", "content"], "file_read");
    let e = m.ok(
        "file_edit",
        json!({ "path": "main.py", "oldText": "total += sum(range(i))", "newText": "total += i * (i - 1) // 2" }),
    );
    assert_eq!(e["replacements"], 1);
    m.ok("profile_start", json!({ "command": "python3 main.py" }));
    let after = m.ok("profile_hotspots", json!({ "limit": 5 }));
    let second = after["snapshot"].as_str().unwrap().to_string();
    assert_ne!(first, second);
    let cmp = m.ok("codecraft_execute", json!({ "id": "profiler.compare", "params": { "a": first, "b": second } }));
    keys(&cmp, &["id", "result"], "codecraft_execute");
    keys(&cmp["result"], &["faster", "slower"], "profiler.compare");
    let faster = cmp["result"]["faster"].as_array().unwrap();
    assert!(faster.iter().any(|f| f["name"] == "slow_sum" || f["name"] == "<built-in method builtins.sum>"), "{cmp}");
    let gone = after["hotspots"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["name"] != "<built-in method builtins.sum>" || h["selfPercent"].as_f64().unwrap_or(0.0) < 50.0);
    assert!(gone, "the hot builtin is gone from the top: {after}");
}

// ---------------------------------------------------------------- Fix lint problems

fn git_env(home: &Path) -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_ALLOW_PROTOCOL".into(), "file".into()),
    ]
}

fn git(dir: &Path, home: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).envs(git_env(home)).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repo(tmp: &Path, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let home = tmp.join("githome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = QA\n\temail = qa@example.com\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    let root = write(&tmp.join("app"), files);
    git(&root, &home, &["init", "-q", "-b", "main"]);
    git(&root, &home, &["add", "-A"]);
    git(&root, &home, &["commit", "-qm", "init"]);
    (root, home)
}

#[test]
fn playbook_fix_lint_problems_in_changed_files() {
    if !has("git", &["--version"]) || !has("ruff", &["--version"]) || !has("cargo", &["clippy", "--version"]) {
        eprintln!("skipping: git, ruff or cargo clippy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (root, home) = repo(
        tmp.path(),
        &[
            ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n"),
            ("tools/gen.py", "print('gen')\n"),
            ("tools/keep.py", "print('keep')\n"),
        ],
    );
    // The uncommitted changes: a needless return in Rust, an unused import in Python.
    write(
        &root,
        &[
            (
                "src/lib.rs",
                "pub fn one() -> u32 {\n    1\n}\n\npub fn two(x: u32) -> u32 {\n    let y = x + 1;\n    return y;\n}\n",
            ),
            ("tools/gen.py", "import os\nprint('gen')\n"),
        ],
    );
    let mut m = Mcp::start(tmp.path(), &root, "run,write", &git_env(&home));
    let r = m.ok("inspect", json!({ "scope": "uncommitted", "tools": ["clippy", "ruff"] }));
    keys(&r, &["scope", "counts", "total", "tools", "findings"], "inspect");
    assert_eq!(r["scope"], "Uncommitted files (2)", "{r}");
    for t in r["tools"].as_array().unwrap() {
        keys(t, &["tool", "ok", "outcome", "commandLine"], "inspect.tools[]");
    }
    let findings = r["findings"].as_array().unwrap();
    for f in findings {
        keys(f, &["id", "severity", "inspection", "tool", "file", "line", "column", "message", "fixes"], "a finding");
    }
    let clippy =
        findings.iter().find(|f| f["inspection"] == "clippy::needless_return").unwrap_or_else(|| panic!("{r}"));
    assert_eq!(clippy["line"], 7, "{clippy}");
    let ruff = findings.iter().find(|f| f["inspection"] == "F401 unused-import").unwrap_or_else(|| panic!("{r}"));
    assert_eq!(ruff["file"], json!(root.join("tools/gen.py")), "{ruff}");
    assert!(!findings.iter().any(|f| f["file"].as_str().is_some_and(|p| p.ends_with("keep.py"))), "only changed files");

    let fixed = m.ok("codecraft_execute", json!({ "id": "analysis.fix", "params": { "id": clippy["id"] } }));
    keys(&fixed["result"], &["accepted", "conflicts", "invalid", "stale", "files", "remaining"], "analysis.fix");
    assert_eq!(fixed["result"]["accepted"], json!([clippy["id"]]), "{fixed}");
    let by_rule = m.ok("codecraft_execute", json!({ "id": "analysis.fix", "params": { "rule": "F401" } }));
    assert!(by_rule["result"].is_object(), "{by_rule}");
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(!lib.contains("return y;"), "{lib}");
    assert!(!std::fs::read_to_string(root.join("tools/gen.py")).unwrap().contains("import os"));
    let p = m.ok("problems", json!({ "path": "src" }));
    keys(&p, &["total", "errors", "problems"], "problems");
    assert_eq!(p["total"], 0, "{p}");
}

// ---------------------------------------------------------------- Inspect a database table

fn pg() -> Option<(Box<dyn codecraft_engine::db::Connection>, Value)> {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let params = json!({
        "kind": "postgres", "name": "Shop",
        "host": env("CODECRAFT_TEST_PG_HOST", "localhost"),
        "port": env("CODECRAFT_TEST_PG_PORT", "5432"),
        "user": env("CODECRAFT_TEST_PG_USER", &std::env::var("USER").unwrap_or_default()),
        "database": env("CODECRAFT_TEST_PG_DB", "postgres"),
        "savePassword": false,
        "confirmWrites": true,
        "options": { "connect_timeout": "3" },
    });
    let cfg = codecraft_engine::db::DataSourceConfig {
        kind: codecraft_engine::db::DbKind::Postgres,
        host: params["host"].as_str()?.into(),
        port: params["port"].as_str()?.parse().ok(),
        user: params["user"].as_str()?.into(),
        database: params["database"].as_str()?.into(),
        options: [("connect_timeout".to_string(), "3".to_string())].into_iter().collect(),
        ..Default::default()
    };
    match codecraft_engine::db::connect(&cfg, std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => Some((c, params)),
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            None
        }
    }
}

#[test]
fn playbook_inspect_a_database_table_safely() {
    let Some((mut admin, params)) = pg() else { return };
    let schema = format!("codecraft_qb_{}", std::process::id());
    let q =
        |c: &mut Box<dyn codecraft_engine::db::Connection>, sql: &str| codecraft_engine::db::query_all(c.as_mut(), sql);
    q(&mut admin, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).unwrap();
    q(&mut admin, &format!("CREATE SCHEMA {schema}")).unwrap();
    q(
        &mut admin,
        &format!(
            "CREATE TABLE {schema}.orders (id serial primary key, total numeric, created_at timestamptz default now())"
        ),
    )
    .unwrap();
    q(&mut admin, &format!("INSERT INTO {schema}.orders(total) SELECT g FROM generate_series(1, 30) g")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = write(&tmp.path().join("app"), &[("local.db", ""), ("README.md", "app\n")]);
    std::fs::remove_file(root.join("local.db")).unwrap();
    let mut m = Mcp::start(tmp.path(), &root, "write", &[]);
    let run = || {
        // The data source (the user adds it in the IDE; headless, through codecraft_execute).
        let added = m.ok("codecraft_execute", json!({ "id": "db.addDataSource", "params": params }));
        assert!(added["result"]["dataSource"].is_string(), "{added}");
        let sources = m.ok("db_sources", json!({}));
        let shop = sources.as_array().unwrap().iter().find(|d| d["name"] == "Shop").cloned().unwrap();
        keys(&shop, &["id", "name", "kind", "readOnly", "confirmWrites", "status"], "db_sources[]");
        let cols = m.ok(
            "db_query",
            json!({ "dataSource": "Shop", "sql": format!("select column_name, data_type from information_schema.columns where table_schema = '{schema}' and table_name = 'orders' order by ordinal_position") }),
        );
        keys(&cols, &["dataSource", "results"], "db_query");
        let r0 = &cols["results"][0];
        keys(r0, &["columns", "rows", "totalRows", "statusLine", "hasMore"], "db_query.results[]");
        assert_eq!(r0["columns"][0]["name"], "column_name", "{cols}");
        assert!(r0["columns"][0].get("typeName").is_some(), "{cols}");
        assert_eq!(r0["rows"][0], json!(["id", "integer"]), "{cols}");
        let count = m.ok(
            "db_query",
            json!({ "dataSource": "Shop", "sql": format!("select count(*), max(created_at) from {schema}.orders") }),
        );
        assert_eq!(count["results"][0]["rows"][0][0], json!(30), "{count}");
        let page = m.ok("db_query", json!({ "dataSource": "Shop", "sql": format!("select * from {schema}.orders order by id desc"), "limit": 20 }));
        assert_eq!(page["results"][0]["rows"].as_array().unwrap().len(), 20, "{page}");
        assert_eq!(page["results"][0]["hasMore"], true, "{page}");
        // EXPLAIN is a read; EXPLAIN ANALYZE and writes need db-write (not allowed here).
        m.ok("db_query", json!({ "dataSource": "Shop", "sql": format!("explain select * from {schema}.orders") }));
        let denied = m
            .call(
                "db_query",
                json!({ "dataSource": "Shop", "sql": format!("explain analyze select * from {schema}.orders") }),
            )
            .unwrap_err();
        assert!(denied.contains("db-write"), "{denied}");
        let denied = m
            .call("db_query", json!({ "dataSource": "Shop", "sql": format!("delete from {schema}.orders") }))
            .unwrap_err();
        assert!(denied.contains("db-write"), "{denied}");
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    drop(m);
    let _ = q(&mut admin, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE"));
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ---------------------------------------------------------------- Review a branch

#[test]
fn playbook_review_a_branch() {
    if !has("git", &["--version"]) || !has("cargo", &["--version"]) {
        eprintln!("skipping: git or cargo is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (root, home) = repo(
        tmp.path(),
        &[
            ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/lib.rs", "pub mod parse;\n"),
            ("src/parse.rs", "pub fn words(s: &str) -> usize {\n    s.split_whitespace().count()\n}\n"),
            ("tests/parse.rs", "#[test]\nfn counts() {\n    assert_eq!(app::parse::words(\"a b\"), 2);\n}\n"),
        ],
    );
    git(&root, &home, &["checkout", "-qb", "feature/parser"]);
    write(
        &root,
        &[(
            "src/parse.rs",
            "pub fn words(s: &str) -> usize {\n    s.split_whitespace().count()\n}\n\npub fn lines(s: &str) -> usize {\n    s.lines().count()\n}\n",
        )],
    );
    git(&root, &home, &["commit", "-qam", "lines"]);
    let mut m = Mcp::start(tmp.path(), &root, "run", &git_env(&home));
    let st = m.ok("codecraft_execute", json!({ "id": "vcs.status" }));
    keys(&st["result"], &["branch", "upstream", "ahead", "behind", "files"], "vcs.status");
    assert_eq!(st["result"]["branch"], "feature/parser", "{st}");
    let r = m.ok("inspect", json!({ "scope": "glob", "pattern": "src/parse.rs, src/lib.rs, tests/parse.rs" }));
    keys(&r, &["total", "findings"], "inspect glob");
    let p = m.ok("problems", json!({ "path": "src", "severity": "warning" }));
    keys(&p, &["total", "errors", "problems"], "problems");
    let all = m.ok("test_run", json!({}));
    assert_eq!(all["status"], "passed", "{all}");
    assert_eq!(all["failures"], json!([]), "{all}");
    let one = m.ok("test_run", json!({ "path": "tests/parse.rs" }));
    assert_eq!(one["status"], "passed", "{one}");
    assert_eq!(one["counts"]["passed"], 1, "the branch's own tests: {one}");
}

// ---------------------------------------------------------------- several sessions, cancellation

fn debugpy() -> bool {
    has("python3", &["-c", "import debugpy"])
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A server and its client, each its own session: waits and evaluations by `session`.
#[test]
fn two_debug_sessions_over_mcp() {
    if !debugpy() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let port = free_port();
    let server = format!(
        "import socket\nsrv = socket.socket()\nsrv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         srv.bind(('127.0.0.1', {port}))\nsrv.listen(1)\nconn, _ = srv.accept()\ndata = conn.recv(100).decode()\n\
         reply = data.upper()\nconn.sendall(reply.encode())\nconn.close()\n"
    );
    let client = format!(
        "import socket\nimport time\nfor _ in range(400):\n    try:\n        c = socket.create_connection(('127.0.0.1', {port}))\n        break\n    except OSError:\n        time.sleep(0.05)\n\
         c.sendall(b'ping')\nanswer = c.recv(100).decode()\nprint('client got', answer)\n"
    );
    let tmp = tempfile::tempdir().unwrap();
    let root = write(&tmp.path().join("net"), &[("server.py", &server), ("client.py", &client)]);
    let mut m = Mcp::start(tmp.path(), &root, "debug", &[]);
    m.ok("debug_breakpoint_set", json!({ "path": "server.py", "line": 9 }));
    m.ok("debug_breakpoint_set", json!({ "path": "client.py", "line": 11 }));
    let srv = m.ok("debug_start", json!({ "file": "server.py" }));
    let srv_id = srv["sessionId"].as_u64().unwrap_or_else(|| panic!("debug_start names its session: {srv}"));
    let cli = m.ok("debug_start", json!({ "file": "client.py" }));
    let cli_id = cli["sessionId"].as_u64().unwrap();
    assert_ne!(srv_id, cli_id);
    let stop = m.ok("debug_wait_for_stop", json!({ "session": srv_id, "timeoutMs": 60000 }));
    assert_eq!((stop["state"].as_str(), stop["sessionId"].as_u64()), (Some("stopped"), Some(srv_id)), "{stop}");
    let sessions = stop["sessions"].as_array().unwrap_or_else(|| panic!("several sessions are listed: {stop}"));
    assert_eq!(sessions.len(), 2, "{stop}");
    for x in sessions {
        keys(x, &["id", "name", "state"], "sessions[]");
    }
    assert!(!stop["locals"].to_string().contains("special variables"), "debugpy's grouping nodes: {stop}");
    let e = m.ok("debug_evaluate", json!({ "expression": "data", "session": srv_id }));
    assert_eq!(e["value"], "'ping'", "{e}");
    // By name too; the client is running (waiting for the answer).
    let st = m.ok("debug_stack", json!({ "session": "server.py" }));
    assert_eq!(st["frames"][0]["line"], 9, "{st}");
    let end = m.ok("debug_control", json!({ "action": "continue", "session": srv_id, "timeoutMs": 60000 }));
    assert_eq!(end["sessionId"], srv_id, "{end}");
    let stop = m.ok("debug_wait_for_stop", json!({ "session": cli_id, "timeoutMs": 60000 }));
    assert_eq!((stop["state"].as_str(), stop["sessionId"].as_u64()), (Some("stopped"), Some(cli_id)), "{stop}");
    let e = m.ok("debug_evaluate", json!({ "expression": "answer", "session": cli_id }));
    assert_eq!(e["value"], "'PING'", "{e}");
    m.ok("debug_control", json!({ "action": "stop", "session": cli_id }));
}

/// `notifications/cancelled` for a `debug_wait_for_stop` that would wait a minute: nothing is
/// answered for it, the server goes on answering, and the program can be stopped.
#[test]
fn cancelling_a_waiting_debug_call() {
    if !debugpy() {
        eprintln!("skipping: debugpy is missing");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = write(&tmp.path().join("loop"), &[("spin.py", "import time\nwhile True:\n    time.sleep(0.05)\n")]);
    let mut m = Mcp::start(tmp.path(), &root, "debug", &[]);
    m.ok("debug_start", json!({ "file": "spin.py" }));
    let waiting = m.send("tools/call", json!({ "name": "debug_wait_for_stop", "arguments": { "timeoutMs": 60000 } }));
    std::thread::sleep(Duration::from_millis(500));
    m.notify("notifications/cancelled", json!({ "requestId": waiting, "reason": "the user changed their mind" }));
    let started = Instant::now();
    let other = m.ok("debug_stack", json!({}));
    assert!(started.elapsed() < Duration::from_secs(20), "the server answers at once");
    assert!(other.is_object(), "{other}");
    let stopped = m.ok("debug_control", json!({ "action": "stop" }));
    assert!(stopped.is_object(), "{stopped}");
    assert!(m.wait(waiting, 2).is_none(), "the cancelled call is not answered");
}

// ---------------------------------------------------------------- --print-config

#[test]
fn print_config_matches_the_docs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = write(&tmp.path().join("p"), &[("a.txt", "a\n")]);
    let exe = PathBuf::from(CLI).canonicalize().unwrap().display().to_string();
    let run = |args: &[&str]| -> String {
        let out = Command::new(CLI)
            .env("CODECRAFT_HOME", tmp.path().join("home"))
            .env("HOME", tmp.path().join("home"))
            .env_remove("CODECRAFT_IDE_PORT")
            .env_remove("CODECRAFT_JOB")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let p = root.display().to_string();
    // docs: `claude mcp add codecraft -- /abs/codecraft-cli mcp /abs/project --allow run,debug,profile`
    let claude = run(&["mcp", "--print-config", "claude", &p]);
    if dump() {
        eprintln!("{claude}");
    }
    assert_eq!(claude.trim(), format!("claude mcp add codecraft -- {exe} mcp {p} --allow run,debug,profile"));
    // docs: `codex mcp add …` and the `~/.codex/config.toml` block.
    let codex = run(&["mcp", "--print-config", "codex", &p]);
    assert!(
        codex.contains("codex mcp add codecraft")
            && codex.contains("~/.codex/config.toml")
            && codex.contains("[mcp_servers.codecraft]"),
        "{codex}"
    );
    // docs: the mcpServers JSON for the project's .gemini/settings.json (~/.gemini/… with --ide).
    let gemini = run(&["mcp", "--print-config", "gemini", &p]);
    assert!(gemini.contains(".gemini/settings.json") && gemini.contains("\"mcpServers\""), "{gemini}");
    let gemini_ide = run(&["mcp", "--print-config", "gemini", "--ide"]);
    assert!(gemini_ide.contains("~/.gemini/settings.json"), "{gemini_ide}");
    // docs: .cursor/mcp.json (~/.cursor/mcp.json with --ide).
    let cursor = run(&["mcp", "--print-config", "cursor", &p]);
    assert!(cursor.contains(".cursor/mcp.json"), "{cursor}");
    let cursor_ide = run(&["mcp", "--print-config", "cursor", "--ide"]);
    assert!(cursor_ide.contains("~/.cursor/mcp.json"), "{cursor_ide}");
    let grok = run(&["mcp", "--print-config", "grok", &p]);
    assert!(grok.starts_with("grok mcp add"), "{grok}");
    let generic = run(&["mcp", "--print-config", "json", &p]);
    let body: String = generic.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["mcpServers"]["codecraft"]["args"], json!(["mcp", p, "--allow", "run,debug,profile"]), "{generic}");
    // docs: `claude mcp add --scope user codecraft-ide -- /abs/codecraft-cli mcp --ide`
    let ide = run(&["mcp", "--print-config", "claude", "--ide"]);
    assert_eq!(ide.trim(), format!("claude mcp add --scope user codecraft-ide -- {exe} mcp --ide"));
}
