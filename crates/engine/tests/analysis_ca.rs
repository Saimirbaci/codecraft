//! Code analysis for people and agents (M14 track CA), end to end: `analysis.run` jobs with the
//! real cargo clippy, rust-analyzer, ruff, clang and clangd when they are installed (each part is
//! skipped without them), fakes for cppcheck, clang-tidy, cmake, uv and basedpyright; inspection
//! profiles, glob scopes, baselines, fixes and suppressions, SARIF/HTML/JSON export and import,
//! and the commit check with a real Git repository.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::inspections::Tool;

fn pump_until(s: &mut Session, what: &str, secs: u64, mut done: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}; status: {}", s.status);
}

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("/usr/bin/which").arg(name).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

fn project(files: &[(&str, &str)]) -> Project {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("project");
    for (rel, text) in files {
        write(&root, rel, text);
    }
    std::fs::create_dir_all(&root).unwrap();
    Project { _tmp: tmp, root, home: base.join("home") }
}

fn session(p: &Project) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&p.home);
    s.execute("file.open", &json!({ "path": p.root })).unwrap();
    pump_until(&mut s, "project scan", 60, |s| s.ws.project.as_ref().is_some_and(|p| p.tree.len() > 1) && !s.jobs_running());
    s
}

/// Start an analysis and wait for its results.
fn analyze(s: &mut Session, params: Value) -> Value {
    let job = s.execute("analysis.run", &params).unwrap()["job"].as_u64().unwrap();
    let mut out = Value::Null;
    pump_until(s, "analysis", 300, |s| {
        let r = s.execute("analysis.results", &json!({ "job": job, "pageSize": 500 })).unwrap();
        let done = r["status"] != "running";
        out = r;
        done
    });
    assert_eq!(out["status"], "done", "{out:#}");
    out
}

fn problems(r: &Value) -> Vec<Value> {
    r["problems"].as_array().cloned().unwrap_or_default()
}

fn find<'a>(ps: &'a [Value], rule: &str) -> Option<&'a Value> {
    ps.iter().find(|p| p["rule"] == rule)
}

fn clippy_available() -> bool {
    Command::new(codecraft_engine::cargo::find_cargo()).args(["clippy", "--version"]).output().is_ok_and(|o| o.status.success())
}

const MAIN: &str = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\nfn narrow(x: u64) -> u32 {\n    x as u32\n}\n\nfn main() {\n    let unused = 1;\n    let v: Vec<i32> = vec![1, 2, 3];\n    if v.len() == 0 {\n        println!(\"{}\", add(1, 2));\n    }\n    println!(\"{}\", narrow(7));\n}\n";

#[test]
fn rust_analysis_profiles_baseline_fixes_and_exports() {
    if !clippy_available() {
        eprintln!("skipping: cargo clippy is not installed");
        return;
    }
    let p = project(&[
        ("Cargo.toml", "[package]\nname = \"ca\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("src/main.rs", MAIN),
    ]);
    let main = p.root.join("src/main.rs");
    let mut s = session(&p);

    // A clippy run: compact problems with 1-based positions, rule ids and fixes.
    let r = analyze(&mut s, json!({ "scope": "project", "analyzers": ["clippy"] }));
    let ps = problems(&r);
    let ret = find(&ps, "clippy::needless_return").unwrap_or_else(|| panic!("{r:#}"));
    assert_eq!(ret["path"], "src/main.rs");
    assert_eq!((ret["line"].as_u64(), ret["column"].as_u64()), (Some(2), Some(5)));
    assert_eq!(ret["severity"], "warning");
    assert_eq!(ret["analyzer"], "clippy");
    assert_eq!(ret["fix"]["safe"], true);
    assert_eq!(ret["excerpt"], "    return a + b;");
    assert!(find(&ps, "unused_variables").is_some());
    assert!(find(&ps, "clippy::cast_possible_truncation").is_none(), "pedantic is off by default");
    assert_eq!(r["analyzers"][0]["id"], "clippy");
    assert!(r["counts"]["warning"].as_u64().unwrap() >= 3);
    // Filters and pages.
    let page = s.execute("analysis.results", &json!({ "filter": { "rule": "clippy::len_zero" }, "pageSize": 1 })).unwrap();
    assert_eq!((page["total"].as_u64(), page["pages"].as_u64()), (Some(1), Some(1)));
    let page = s.execute("analysis.results", &json!({ "pageSize": 1, "page": 2 })).unwrap();
    assert_eq!(page["problems"].as_array().unwrap().len(), 1);
    assert!(page["pages"].as_u64().unwrap() >= 3);

    // A profile: pedantic on, needless_return as an error, unused_variables off.
    s.execute("analysis.profile.copy", &json!({ "from": "Default", "name": "Team" })).unwrap();
    s.execute("analysis.profile.select", &json!({ "name": "Team" })).unwrap();
    s.execute("analysis.profile.setOption", &json!({ "option": "clippyGroups", "value": ["pedantic"] })).unwrap();
    s.execute("analysis.profile.setRule", &json!({ "analyzer": "clippy", "rule": "clippy::needless_return", "severity": "error" })).unwrap();
    s.execute("analysis.profile.setRule", &json!({ "analyzer": "clippy", "rule": "unused_variables", "enabled": false })).unwrap();
    let r = analyze(&mut s, json!({ "analyzers": ["clippy"] }));
    assert_eq!(r["profile"], "Team");
    let ps = problems(&r);
    assert_eq!(find(&ps, "clippy::needless_return").unwrap()["severity"], "error");
    assert!(find(&ps, "clippy::cast_possible_truncation").is_some(), "{r:#}");
    assert!(find(&ps, "unused_variables").is_none(), "turned off with -A");
    assert_eq!(ps.first().unwrap()["severity"], "error", "errors first");

    // Baseline: everything known; a new problem is the only new one.
    s.execute("analysis.baseline.save", &json!({})).unwrap();
    assert!(p.root.join(".codecraft/inspection-baseline.json").is_file());
    std::fs::write(&main, MAIN.replace("fn main() {\n", "fn main() {\n    let w: Vec<u8> = Vec::new();\n    if w.len() == 0 {}\n")).unwrap();
    let r = analyze(&mut s, json!({ "analyzers": ["clippy"] }));
    let new: Vec<Value> = problems(&r).into_iter().filter(|p| p["new"] == true).collect();
    assert!(!new.is_empty() && new.iter().all(|p| p["line"].as_u64() == Some(11)), "{new:#?}");
    let only = s.execute("analysis.results", &json!({ "filter": { "onlyNew": true } })).unwrap();
    assert_eq!(only["total"].as_u64(), Some(new.len() as u64));

    // Fix every len_zero, suppress needless_return.
    let v = s.execute("analysis.fix", &json!({ "rule": "clippy::len_zero" })).unwrap();
    assert_eq!(v["preview"], false, "{v}");
    let text = std::fs::read_to_string(&main).unwrap();
    assert!(text.contains("if w.is_empty() {}") && text.contains("if v.is_empty() {"), "{text}");
    let id = find(&problems(&s.execute("analysis.results", &json!({ "pageSize": 500 })).unwrap()), "clippy::needless_return").unwrap()["id"]
        .as_u64()
        .unwrap();
    s.execute("analysis.suppress", &json!({ "id": id })).unwrap();
    assert!(std::fs::read_to_string(&main).unwrap().starts_with("#[allow(clippy::needless_return)]\nfn add"));
    assert!(s.execute("analysis.fix", &json!({ "job": 1 })).is_err(), "an old job is not the current results");

    // Exports: SARIF, HTML, JSON; SARIF back as offline results with a job id.
    let sarif = p.root.join("out/results.sarif");
    let v = s.execute("analysis.export", &json!({ "path": sarif })).unwrap();
    assert_eq!(v["format"], "sarif");
    let log: Value = serde_json::from_str(&std::fs::read_to_string(&sarif).unwrap()).unwrap();
    assert_eq!(log["version"], "2.1.0");
    assert!(log["runs"][0]["results"].as_array().unwrap().iter().all(|r| r["baselineState"].is_string()));
    let v = s.execute("analysis.export", &json!({ "format": "html" })).unwrap();
    assert!(v["content"].as_str().unwrap().contains("<code>src/main.rs</code>"));
    let v = s.execute("analysis.export", &json!({ "format": "json" })).unwrap();
    assert_eq!(v["content"]["format"], "codecraft-inspection-results/1");
    let v = s.execute("code.analyzeCode.viewOfflineInspectionResults", &json!({ "path": sarif })).unwrap();
    let job = v["job"].as_u64().unwrap();
    let r = s.execute("analysis.results", &json!({ "job": job })).unwrap();
    assert_eq!(r["status"], "done");
    assert!(find(&problems(&r), "clippy::cast_possible_truncation").is_some());
    s.shutdown();
}

#[test]
fn rust_analyzer_project_wide_diagnostics() {
    let Some(ra) = which("rust-analyzer") else {
        eprintln!("skipping: rust-analyzer is not installed");
        return;
    };
    if !Command::new(&ra).arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: rust-analyzer does not run");
        return;
    }
    let p = project(&[
        ("Cargo.toml", "[package]\nname = \"ra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("src/main.rs", "mod missing;\n\nfn main() {\n    let s: i32 = \"x\";\n}\n"),
    ]);
    let mut s = session(&p);
    s.inspections.programs.insert(Tool::RustAnalyzer, ra);
    let r = analyze(&mut s, json!({ "analyzers": ["rust_analyzer"] }));
    let ps = problems(&r);
    let m = find(&ps, "E0583").unwrap_or_else(|| panic!("{r:#}"));
    assert_eq!((m["line"].as_u64(), m["analyzer"].as_str()), (Some(1), Some("rust_analyzer")));
    assert_eq!(find(&ps, "E0308").unwrap()["severity"], "error");
    s.shutdown();
}

#[test]
fn python_ruff_profiles_and_managed_basedpyright() {
    let p = project(&[("app.py", "import os\n\n\ndef f(x=[]):\n    return x\n\n\nprint(f().nope)\n")]);
    let app = p.root.join("app.py");
    let mut s = session(&p);
    let ruff = which("ruff");
    if let Some(ruff) = &ruff {
        s.inspections.programs.insert(Tool::Ruff, ruff.clone());
        let r = analyze(&mut s, json!({ "analyzers": ["ruff"] }));
        let ps = problems(&r);
        assert!(find(&ps, "F401").is_some(), "{r:#}");
        assert!(find(&ps, "B006").is_none(), "B is not selected by default");
        // Strict adds Ruff's bugbear rules; a rule turned off is ignored by ruff itself.
        s.execute("analysis.profile.select", &json!({ "name": "Strict" })).unwrap();
        s.execute("analysis.profile.setRule", &json!({ "analyzer": "ruff", "rule": "F401", "enabled": false })).unwrap();
        let r = analyze(&mut s, json!({ "analyzers": ["python"] }));
        let ps = problems(&r);
        assert!(find(&ps, "B006").is_some(), "{r:#}");
        assert!(find(&ps, "F401").is_none());
        assert!(r["analyzers"][0]["command"].as_str().unwrap().contains("--ignore=F401"));
    } else {
        eprintln!("ruff is not installed: skipping its part");
    }

    // Install basedpyright: described first, then run with a fake uv into the managed tools.
    let fake_bin = p.home.parent().unwrap().join("fakes");
    let report = json!({ "generalDiagnostics": [{
        "file": app, "severity": "error", "message": "Cannot access attribute \"nope\"",
        "range": { "start": { "line": 7, "character": 10 }, "end": { "line": 7, "character": 14 } },
        "rule": "reportAttributeAccessIssue"
    }], "summary": { "errorCount": 1 } });
    let uv = script(
        &fake_bin,
        "uv",
        &format!(
            "[ \"$1 $2 $3\" = \"tool install basedpyright\" ] || exit 2\nmkdir -p \"$UV_TOOL_BIN_DIR\"\nprintf '#!/bin/sh\\ncat <<\"JSON\"\\n%s\\nJSON\\nexit 1\\n' '{report}' > \"$UV_TOOL_BIN_DIR/basedpyright\"\nchmod +x \"$UV_TOOL_BIN_DIR/basedpyright\"\necho installed"
        ),
    );
    s.analysis.install.uv_override = Some(uv);
    let v = s.execute("analysis.installTool", &json!({ "tool": "basedpyright" })).unwrap();
    assert_eq!(v["needsConfirmation"], true);
    assert!(v["command"].as_str().unwrap().contains("uv tool install basedpyright"));
    assert!(s.execute("analysis.installTool", &json!({ "tool": "rm -rf", "confirm": true })).is_err());
    s.execute("analysis.installTool", &json!({ "tool": "basedpyright", "confirm": true })).unwrap();
    pump_until(&mut s, "install", 30, |s| s.analysis.install.running.is_none());
    let last = s.analysis.install.last.clone().unwrap();
    assert!(last.ok, "{last:?}");
    assert!(p.home.join("tools/bin/basedpyright").is_file());
    let tools = s.execute("analysis.tools", &json!({})).unwrap();
    let pyright = tools["analyzers"].as_array().unwrap().iter().find(|t| t["id"] == "pyright").unwrap().clone();
    assert_eq!(pyright["available"], true);
    if which("basedpyright").is_none() && which("pyright").is_none() {
        let r = analyze(&mut s, json!({ "analyzers": ["pyright"] }));
        let hit = find(&problems(&r), "reportAttributeAccessIssue").unwrap_or_else(|| panic!("{r:#}")).clone();
        assert_eq!((hit["line"].as_u64(), hit["column"].as_u64(), hit["severity"].as_str()), (Some(8), Some(11), Some("error")));
        s.execute("analysis.suppress", &json!({ "id": hit["id"] })).unwrap();
        assert!(std::fs::read_to_string(&app).unwrap().contains("print(f().nope)  # pyright: ignore[reportAttributeAccessIssue]"));
    }
    s.shutdown();
}

const C_MAIN: &str = "#include <stdlib.h>\n\nint deref(int *p) {\n    if (!p) {\n        return *p;\n    }\n    return 0;\n}\n\nint main(void) {\n    int unused = 1;\n    int *q = NULL;\n    return deref(q);\n}\n";

#[test]
fn cpp_analyzers_with_a_generated_compile_database() {
    let p = project(&[
        ("CMakeLists.txt", "cmake_minimum_required(VERSION 3.10)\nproject(ca C)\nadd_executable(app src/main.c)\n"),
        ("src/main.c", C_MAIN),
        ("include/app.h", "int deref(int *p);\n"),
    ]);
    let main = p.root.join("src/main.c");
    let mut s = session(&p);
    let fakes = p.home.parent().unwrap().join("fakes");
    // cmake: writes the compilation database where -B says.
    let db = json!([{ "directory": p.root.join("cmake-build-debug"), "command": format!("/usr/bin/cc -Wall -I{} -o CMakeFiles/app.dir/src/main.c.o -c {}", p.root.join("include").display(), main.display()), "file": main }]);
    let cmake = script(
        &fakes,
        "cmake",
        &format!("out=''\nwhile [ $# -gt 0 ]; do [ \"$1\" = -B ] && out=\"$2\"; shift; done\nmkdir -p \"$out\"\ncat > \"$out/compile_commands.json\" <<'JSON'\n{db}\nJSON\necho configured"),
    );
    s.cpp.tools.insert("cmake".into(), cmake);
    // cppcheck and clang-tidy (not installed here): fakes in their output formats.
    let cppcheck = script(
        &fakes,
        "cppcheck",
        &format!("echo '{}:12:10: style: The scope of the variable q can be reduced. [variableScope]' 1>&2", main.display()),
    );
    s.inspections.programs.insert(Tool::Cppcheck, cppcheck);
    let null_off = C_MAIN.find("NULL").unwrap();
    let tidy = script(
        &fakes,
        "clang-tidy",
        &format!(
            "for a in \"$@\"; do case \"$a\" in --export-fixes=*) f=\"${{a#--export-fixes=}}\";; esac; done\n\
             cat > \"$f\" <<'YAML'\n---\nMainSourceFile: '{m}'\nDiagnostics:\n  - DiagnosticName: modernize-use-nullptr\n    DiagnosticMessage:\n      Message: use nullptr\n      FilePath: '{m}'\n      FileOffset: {o}\n      Replacements:\n        - FilePath: '{m}'\n          Offset: {o}\n          Length: 4\n          ReplacementText: '0'\n    Level: Warning\n...\nYAML\n\
             echo '{m}:12:14: warning: use nullptr [modernize-use-nullptr]'",
            m = main.display(),
            o = null_off
        ),
    );
    s.inspections.programs.insert(Tool::ClangTidy, tidy);
    let clang = which("clang");
    let clangd = which("clangd");
    if let Some(c) = &clang {
        s.inspections.programs.insert(Tool::ClangAnalyzer, c.clone());
    }
    if let Some(c) = &clangd {
        s.inspections.programs.insert(Tool::Clangd, c.clone());
    }
    let r = analyze(&mut s, json!({ "analyzers": ["cpp"] }));
    assert!(p.root.join("cmake-build-debug/compile_commands.json").is_file(), "generated by the CMake configure");
    let ps = problems(&r);
    let scope = find(&ps, "variableScope").unwrap_or_else(|| panic!("{r:#}"));
    assert_eq!((scope["analyzer"].as_str(), scope["severity"].as_str(), scope["line"].as_u64()), (Some("cppcheck"), Some("weak_warning"), Some(12)));
    let tidy = find(&ps, "modernize-use-nullptr").unwrap().clone();
    assert_eq!(tidy["fix"]["safe"], true, "{tidy}");
    if clang.is_some() {
        let nd = find(&ps, "core.NullDereference").unwrap_or_else(|| panic!("{r:#}")).clone();
        assert_eq!((nd["analyzer"].as_str(), nd["line"].as_u64()), (Some("clang_analyzer"), Some(5)));
        // The analyzer's path to the bug is in the description.
        let f = s.inspections.results.as_ref().unwrap().finding(nd["id"].as_u64().unwrap() as usize).unwrap().clone();
        assert!(f.description.contains("note:"), "{}", f.description);
        // NOLINT suppresses it for every C/C++ analyzer.
        s.execute("analysis.suppress", &json!({ "id": nd["id"] })).unwrap();
        assert!(std::fs::read_to_string(&main).unwrap().contains("return *p;  // NOLINT(core.NullDereference)"));
    } else {
        eprintln!("clang is not installed: skipping the static analyzer part");
    }
    if clangd.is_some() {
        let unused = find(&ps, "-Wunused-variable").unwrap_or_else(|| panic!("{r:#}"));
        assert_eq!((unused["analyzer"].as_str(), unused["line"].as_u64()), (Some("clangd"), Some(11)), "clangd's report wins over clang's");
    } else {
        eprintln!("clangd is not installed: skipping its part");
    }
    // The clang-tidy fix from its export.
    s.execute("analysis.fix", &json!({ "id": tidy["id"] })).unwrap();
    assert!(std::fs::read_to_string(&main).unwrap().contains("int *q = 0;"));
    if clang.is_some() {
        let r = analyze(&mut s, json!({ "analyzers": ["clang_analyzer"] }));
        assert!(find(&problems(&r), "core.NullDereference").is_none(), "NOLINT honoured: {r:#}");
    }
    s.shutdown();
}

#[test]
fn sql_inspections_glob_scopes_and_severities() {
    let p = project(&[
        ("db/migrations/001.sql", "CREATE TABLE users (id INT);\nDELETE FROM users;\n"),
        ("db/seed.sql", "UPDATE users SET id = 1;\n"),
        ("scripts/other.sql", "DELETE FROM users;\n"),
        ("notes.txt", "DELETE FROM users;\n"),
    ]);
    let mut s = session(&p);
    let r = analyze(&mut s, json!({ "scope": "glob", "pattern": "db/**/*.sql" }));
    let ps = problems(&r);
    assert_eq!(ps.len(), 2, "{r:#}");
    assert!(ps.iter().all(|p| p["analyzer"] == "sql" && p["path"].as_str().unwrap().starts_with("db/")));
    assert!(r["scope"].as_str().unwrap().contains("db/**/*.sql"));
    assert!(s.execute("analysis.run", &json!({ "scope": "glob", "pattern": "nothing/**" })).is_err());
    assert!(s.execute("analysis.run", &json!({ "scope": "glob" })).is_err());
    // Severity from the profile; suppress one with -- noinspection.
    s.execute("analysis.profile.setRule", &json!({ "analyzer": "sql", "rule": "SqlWithoutWhere", "severity": "error" })).unwrap();
    let r = analyze(&mut s, json!({ "scope": "glob", "pattern": "db/migrations/*.sql" }));
    let hit = problems(&r)[0].clone();
    assert_eq!((hit["severity"].as_str(), hit["line"].as_u64()), (Some("error"), Some(2)));
    s.execute("analysis.suppress", &json!({ "id": hit["id"] })).unwrap();
    let text = std::fs::read_to_string(p.root.join("db/migrations/001.sql")).unwrap();
    assert_eq!(text, "CREATE TABLE users (id INT);\n-- noinspection SqlWithoutWhere\nDELETE FROM users;\n");
    let r = analyze(&mut s, json!({ "scope": "glob", "pattern": "db/migrations/*.sql" }));
    assert_eq!(r["total"], 0);
    // The Problems tool window's project view includes the last analysis.
    let r = analyze(&mut s, json!({ "scope": "project", "analyzers": ["sql"] }));
    assert_eq!(r["total"], 2);
    let v = s.execute("analysis.problems", &json!({})).unwrap();
    assert_eq!(v["total"], 2);
    assert!(v["problems"].as_array().unwrap().iter().all(|p| p["source"] == "analysis"));
    s.shutdown();
}

#[test]
fn jobs_can_be_cancelled_and_old_results_stay_readable() {
    let p = project(&[("a.py", "import os\n"), ("q.sql", "DELETE FROM t;\n")]);
    let mut s = session(&p);
    let slow = script(&p.home.parent().unwrap().join("fakes"), "ruff", "sleep 30\necho '[]'");
    s.inspections.programs.insert(Tool::Ruff, slow);
    let first = analyze(&mut s, json!({ "analyzers": ["sql"] }));
    let job = s.execute("analysis.run", &json!({ "analyzers": ["ruff"] })).unwrap()["job"].as_u64().unwrap();
    assert_eq!(s.execute("analysis.results", &json!({ "job": job })).unwrap()["status"], "running");
    assert!(s.execute("analysis.cancel", &json!({ "job": job + 5 })).is_err());
    s.execute("analysis.cancel", &json!({ "job": job })).unwrap();
    assert_eq!(s.execute("analysis.results", &json!({ "job": job })).unwrap()["status"], "cancelled");
    let again = s.execute("analysis.results", &json!({ "job": first["job"] })).unwrap();
    assert_eq!(again["total"], 1, "the earlier job's results are kept");
    assert!(s.execute("analysis.results", &json!({ "job": 999 })).is_err());
    assert!(s.execute("analysis.run", &json!({ "analyzers": ["nope"] })).is_err());
    assert!(s.execute("analysis.run", &json!({ "scope": "nowhere" })).is_err());
    assert!(s.execute("analysis.results", &json!({ "job": first["job"], "format": "xml" })).is_err());
    let sarif = s.execute("analysis.results", &json!({ "job": first["job"], "format": "sarif" })).unwrap();
    assert_eq!(sarif["sarif"]["version"], "2.1.0");
    s.shutdown();
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn commit_check_holds_commits_with_problems() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let p = project(&[("q.sql", "SELECT 1;\n")]);
    std::fs::create_dir_all(&p.home).unwrap();
    let gitconfig = p.home.join(".gitconfig");
    std::fs::write(&gitconfig, "[user]\n\tname = Test\n\temail = t@example.com\n[commit]\n\tgpgsign = false\n").unwrap();
    let env = vec![
        ("HOME".to_string(), p.home.display().to_string()),
        ("GIT_CONFIG_GLOBAL".to_string(), gitconfig.display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
    ];
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(&p.root).envs(env.clone()).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "initial"]);
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&p.home);
    s.vcs.set_git_env(env.clone());
    s.execute("file.open", &json!({ "path": p.root })).unwrap();
    pump_until(&mut s, "repository", 30, |s| s.vcs.repo.is_some() && s.vcs.status.branch.head.is_some());

    assert_eq!(s.execute("vcs.commit.analyzeCode", &json!({})).unwrap()["enabled"], true);
    assert!(std::fs::read_to_string(p.root.join(".codecraft/inspection-profiles.json")).unwrap().contains("commitCheck"));
    std::fs::write(p.root.join("q.sql"), "SELECT 1;\nDELETE FROM users;\n").unwrap();
    pump_until(&mut s, "status", 30, |s| {
        let _ = s.execute("vcs.refresh", &json!({}));
        s.vcs.status.file("q.sql").is_some()
    });
    let v = s.execute("vcs.commit", &json!({ "message": "risky", "files": ["q.sql"] })).unwrap();
    assert_eq!(v["analyzing"], true, "{v}");
    pump_until(&mut s, "commit check", 60, |s| s.analysis.commit.warning.is_some());
    let st = s.execute("analysis.commitCheck.state", &json!({})).unwrap();
    assert_eq!((st["warning"]["warnings"].as_u64(), st["warning"]["errors"].as_u64()), (Some(1), Some(0)), "{st}");
    assert_eq!(git(&["rev-list", "--count", "HEAD"]).trim(), "1", "held back");
    s.execute("analysis.commitCheck.commitAnyway", &json!({})).unwrap();
    pump_until(&mut s, "commit", 30, |s| s.vcs.op.is_none());
    assert_eq!(git(&["rev-list", "--count", "HEAD"]).trim(), "2");
    assert_eq!(git(&["log", "-1", "--format=%s"]).trim(), "risky");

    // A clean change commits straight after the analysis.
    std::fs::write(p.root.join("q.sql"), "SELECT 1;\nSELECT 2;\n").unwrap();
    pump_until(&mut s, "status", 30, |s| {
        let _ = s.execute("vcs.refresh", &json!({}));
        s.vcs.status.file("q.sql").is_some()
    });
    let v = s.execute("vcs.commit", &json!({ "message": "clean", "files": ["q.sql"] })).unwrap();
    assert_eq!(v["analyzing"], true);
    pump_until(&mut s, "clean commit", 60, |_| git(&["rev-list", "--count", "HEAD"]).trim() == "3");
    assert!(s.analysis.commit.warning.is_none());
    s.shutdown();
}
